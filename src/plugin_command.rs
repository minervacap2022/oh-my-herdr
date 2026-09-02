use std::ffi::{OsStr, OsString};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

pub(crate) const PLUGIN_COMMAND_OUTPUT_MAX_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PluginCommandRunnerSpec {
    pub(crate) lease_id: String,
    pub(crate) program: String,
    pub(crate) args: Vec<String>,
    pub(crate) cwd: PathBuf,
    pub(crate) env: Vec<(String, String)>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct PluginCommandRunnerResult {
    pub(crate) finished_unix_ms: u64,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: String,
    pub(crate) stderr: String,
    pub(crate) error: Option<String>,
}

pub(crate) fn command_for_argv_in_dir(program: &str, args: &[String], cwd: &Path) -> Command {
    let program = program_for_cwd(program, cwd);
    let mut command = command_for_program(&program);
    command.args(args).current_dir(cwd);
    command
}

pub(crate) fn command_for_plugin_argv_in_dir(
    program: &str,
    args: &[String],
    cwd: &Path,
    env: &[(String, String)],
) -> Command {
    command_for_argv_in_dir(resolve_host_binary_alias(program, env), args, cwd)
}

pub(crate) fn spawn_runner(spec: &PluginCommandRunnerSpec) -> std::io::Result<std::process::Child> {
    let task_path = runner_task_path(&spec.lease_id);
    write_runner_spec(&task_path, spec)?;
    let current_exe = match std::env::current_exe() {
        Ok(current_exe) => current_exe,
        Err(err) => {
            let _ = std::fs::remove_file(&task_path);
            return Err(err);
        }
    };
    let mut command = crate::noninteractive_process::command(current_exe);
    command
        .arg("server")
        .arg("--plugin-command-runner")
        .arg(&task_path)
        .arg(&spec.lease_id)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    crate::platform::detach_server_daemon_command(&mut command);
    match command.spawn() {
        Ok(mut child) => {
            if let Err(err) = crate::persist::plugin_command_leases::track_runner_process(
                &spec.lease_id,
                child.id(),
            ) {
                let _ = child.kill();
                let _ = child.wait();
                let _ = std::fs::remove_file(task_path);
                return Err(err);
            }
            Ok(child)
        }
        Err(err) => {
            let _ = std::fs::remove_file(task_path);
            Err(err)
        }
    }
}

pub(crate) fn run_runner(task_path: &Path, lease_id: &str) -> std::io::Result<()> {
    let result = read_runner_spec(task_path, lease_id).map_or_else(
        |err| PluginCommandRunnerResult {
            finished_unix_ms: current_unix_ms(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(err.to_string()),
        },
        |spec| {
            run_spec(spec, |process_id| {
                crate::persist::plugin_command_leases::track_command_process(lease_id, process_id)
            })
        },
    );
    let release_error = crate::persist::plugin_command_leases::release(lease_id).err();
    let mut result = result;
    if let Some(err) = release_error.as_ref() {
        let message = format!("plugin command completed but failed to release its lease: {err}");
        result.error = Some(match result.error.take() {
            Some(previous) => format!("{previous}; {message}"),
            None => message,
        });
    }
    let stdout = std::io::stdout();
    let mut stdout = stdout.lock();
    serde_json::to_writer(&mut stdout, &result)?;
    stdout.flush()?;
    if let Some(err) = release_error {
        return Err(std::io::Error::other(err));
    }
    Ok(())
}

fn runner_task_path(lease_id: &str) -> PathBuf {
    crate::session::data_dir().join(format!(".{lease_id}.json"))
}

fn write_runner_spec(path: &Path, spec: &PluginCommandRunnerSpec) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let result = (|| {
        let json = serde_json::to_vec(spec)?;
        let mut task = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        task.write_all(&json)?;
        task.sync_all()
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(path);
    }
    result
}

fn read_runner_spec(path: &Path, lease_id: &str) -> std::io::Result<PluginCommandRunnerSpec> {
    let bytes = std::fs::read(path);
    let _ = std::fs::remove_file(path);
    let spec: PluginCommandRunnerSpec = serde_json::from_slice(&bytes?)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err))?;
    if spec.lease_id != lease_id {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "plugin command runner lease identifier did not match its task",
        ));
    }
    Ok(spec)
}

fn run_spec(
    spec: PluginCommandRunnerSpec,
    track_command_process: impl FnOnce(u32) -> std::io::Result<()>,
) -> PluginCommandRunnerResult {
    let child = command_for_plugin_argv_in_dir(&spec.program, &spec.args, &spec.cwd, &spec.env)
        .envs(spec.env)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();
    match child {
        Ok(mut child) => {
            if let Err(err) = track_command_process(child.id()) {
                let _ = child.kill();
                let _ = child.wait();
                return PluginCommandRunnerResult {
                    finished_unix_ms: current_unix_ms(),
                    exit_code: None,
                    stdout: String::new(),
                    stderr: String::new(),
                    error: Some(format!("failed to track plugin command process: {err}")),
                };
            }
            let stdout_reader = child.stdout.take().map(|stdout| {
                std::thread::spawn(move || {
                    read_capped_plugin_output(stdout, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
                })
            });
            let stderr_reader = child.stderr.take().map(|stderr| {
                std::thread::spawn(move || {
                    read_capped_plugin_output(stderr, PLUGIN_COMMAND_OUTPUT_MAX_BYTES)
                })
            });
            match child.wait() {
                Ok(status) => PluginCommandRunnerResult {
                    finished_unix_ms: current_unix_ms(),
                    exit_code: status.code(),
                    stdout: stdout_reader
                        .and_then(|reader| reader.join().ok())
                        .unwrap_or_default(),
                    stderr: stderr_reader
                        .and_then(|reader| reader.join().ok())
                        .unwrap_or_default(),
                    error: None,
                },
                Err(err) => PluginCommandRunnerResult {
                    finished_unix_ms: current_unix_ms(),
                    exit_code: None,
                    stdout: stdout_reader
                        .and_then(|reader| reader.join().ok())
                        .unwrap_or_default(),
                    stderr: stderr_reader
                        .and_then(|reader| reader.join().ok())
                        .unwrap_or_default(),
                    error: Some(err.to_string()),
                },
            }
        }
        Err(err) => PluginCommandRunnerResult {
            finished_unix_ms: current_unix_ms(),
            exit_code: None,
            stdout: String::new(),
            stderr: String::new(),
            error: Some(err.to_string()),
        },
    }
}

fn resolve_host_binary_alias<'a>(program: &'a str, env: &'a [(String, String)]) -> &'a str {
    if !is_host_binary_alias(program) {
        return program;
    }

    env.iter()
        .rev()
        .find_map(|(name, value)| {
            (name == "HERDR_BIN_PATH" && !value.is_empty()).then_some(value.as_str())
        })
        .unwrap_or(program)
}

#[cfg(windows)]
fn is_host_binary_alias(program: &str) -> bool {
    program.eq_ignore_ascii_case("herdr") || program.eq_ignore_ascii_case("herdr.exe")
}

#[cfg(not(windows))]
fn is_host_binary_alias(program: &str) -> bool {
    program == "herdr"
}

pub(crate) fn current_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

pub(crate) fn read_capped_plugin_output(mut reader: impl Read, cap: usize) -> String {
    let mut kept = Vec::with_capacity(cap.min(8192));
    let mut buf = [0u8; 8192];
    let mut truncated = false;
    loop {
        match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                let remaining = cap.saturating_sub(kept.len());
                if remaining > 0 {
                    kept.extend_from_slice(&buf[..n.min(remaining)]);
                }
                if n > remaining {
                    truncated = true;
                }
            }
            Err(err) if err.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    let mut output = String::from_utf8_lossy(&kept).into_owned();
    if truncated {
        output.push_str(&format!(
            "\n[herdr truncated plugin output after {cap} bytes]"
        ));
    }
    output
}

fn program_for_cwd(program: &str, cwd: &Path) -> OsString {
    let path = Path::new(program);
    let has_separator = program.contains('/') || (cfg!(windows) && program.contains('\\'));
    if path.is_relative() && has_separator {
        let relative = path.strip_prefix(Path::new(".")).unwrap_or(path);
        cwd.join(relative).into_os_string()
    } else {
        path.as_os_str().to_os_string()
    }
}

#[cfg(not(windows))]
fn command_for_program(program: &OsStr) -> Command {
    crate::noninteractive_process::command(program)
}

#[cfg(windows)]
fn command_for_program(program: &OsStr) -> Command {
    let resolved = resolve_windows_program(program);
    let command_program = resolved.as_ref().map_or_else(
        || program.to_os_string(),
        |path| path.as_os_str().to_os_string(),
    );
    if is_windows_batch_file_name(program)
        || resolved
            .as_ref()
            .is_some_and(|path| is_windows_batch_path(path))
    {
        let shell =
            std::env::var_os("ComSpec").unwrap_or_else(|| r"C:\Windows\System32\cmd.exe".into());
        let mut command = crate::noninteractive_process::command(shell);
        command.arg("/d").arg("/c").arg(command_program);
        command
    } else {
        crate::noninteractive_process::command(command_program)
    }
}

#[cfg(windows)]
fn resolve_windows_program(program: &OsStr) -> Option<PathBuf> {
    if has_path_separator(program) {
        return None;
    }
    let path = Path::new(program);
    if path.extension().is_some() {
        return std::env::var_os("PATH").and_then(|path_var| {
            std::env::split_paths(&path_var)
                .map(|dir| dir.join(program))
                .find(|candidate| candidate.is_file())
        });
    }
    let extensions = windows_path_extensions();
    std::env::var_os("PATH").and_then(|path_var| {
        std::env::split_paths(&path_var).find_map(|dir| {
            extensions
                .iter()
                .map(|extension| {
                    let mut file_name = program.to_os_string();
                    file_name.push(extension);
                    dir.join(file_name)
                })
                .find(|candidate| candidate.is_file())
        })
    })
}

#[cfg(windows)]
fn windows_path_extensions() -> Vec<String> {
    std::env::var_os("PATHEXT")
        .map(|value| {
            value
                .to_string_lossy()
                .split(';')
                .map(str::trim)
                .filter(|part| !part.is_empty())
                .map(|part| {
                    if part.starts_with('.') {
                        part.to_string()
                    } else {
                        format!(".{part}")
                    }
                })
                .collect::<Vec<_>>()
        })
        .filter(|extensions| !extensions.is_empty())
        .unwrap_or_else(|| {
            vec![
                ".COM".to_string(),
                ".EXE".to_string(),
                ".BAT".to_string(),
                ".CMD".to_string(),
            ]
        })
}

#[cfg(windows)]
fn has_path_separator(program: &OsStr) -> bool {
    program.to_string_lossy().contains(['/', '\\'])
}

#[cfg(windows)]
fn is_windows_batch_path(path: &Path) -> bool {
    path.extension()
        .and_then(OsStr::to_str)
        .is_some_and(is_windows_batch_extension)
}

#[cfg(any(windows, test))]
fn is_windows_batch_file_name(program: &OsStr) -> bool {
    Path::new(program)
        .extension()
        .and_then(OsStr::to_str)
        .is_some_and(is_windows_batch_extension)
}

#[cfg(any(windows, test))]
fn is_windows_batch_extension(extension: &str) -> bool {
    extension.eq_ignore_ascii_case("cmd") || extension.eq_ignore_ascii_case("bat")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn plugin_command_uses_host_binary_for_herdr_alias() {
        use std::os::unix::fs::PermissionsExt as _;

        let root = std::env::temp_dir().join(format!(
            "herdr-plugin-command-host-binary-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create plugin command fixture");
        let host_binary = root.join("host-herdr");
        std::fs::write(&host_binary, "#!/bin/sh\nprintf 'host-herdr'\n")
            .expect("write host binary fixture");
        std::fs::set_permissions(&host_binary, std::fs::Permissions::from_mode(0o755))
            .expect("make host binary executable");

        let result = run_spec(
            PluginCommandRunnerSpec {
                lease_id: "test-lease".to_string(),
                program: "herdr".to_string(),
                args: vec!["--version".to_string()],
                cwd: root.clone(),
                env: vec![(
                    "HERDR_BIN_PATH".to_string(),
                    host_binary.display().to_string(),
                )],
            },
            |_| Ok(()),
        );
        let _ = std::fs::remove_dir_all(root);

        assert_eq!(result.exit_code, Some(0));
        assert_eq!(result.stdout, "host-herdr");
    }

    #[test]
    fn only_herdr_aliases_use_host_binary_path() {
        let env = vec![
            ("HERDR_BIN_PATH".to_string(), "/host/herdr".to_string()),
            ("HERDR_BIN_PATH".to_string(), String::new()),
        ];

        assert_eq!(resolve_host_binary_alias("herdr", &env), "/host/herdr");
        #[cfg(windows)]
        assert_eq!(resolve_host_binary_alias("herdr.exe", &env), "/host/herdr");
        #[cfg(not(windows))]
        assert_eq!(resolve_host_binary_alias("herdr.exe", &env), "herdr.exe");
        assert_eq!(resolve_host_binary_alias("sh", &env), "sh");
    }

    #[test]
    fn resolves_explicit_relative_program_against_working_directory() {
        let cwd = Path::new("plugin-root");

        assert_eq!(
            program_for_cwd("./bin/tool", cwd),
            cwd.join("bin/tool").into_os_string()
        );
        assert_eq!(program_for_cwd("tool", cwd), OsString::from("tool"));
    }

    #[test]
    fn recognizes_windows_batch_extensions_case_insensitively() {
        assert!(is_windows_batch_file_name(OsStr::new("npm.cmd")));
        assert!(is_windows_batch_file_name(OsStr::new("script.BAT")));
        assert!(!is_windows_batch_file_name(OsStr::new("node.exe")));
        assert!(!is_windows_batch_file_name(OsStr::new("node")));
    }

    #[cfg(windows)]
    #[test]
    fn windows_batch_command_captures_output() {
        let path = std::env::temp_dir().join(format!(
            "herdr-plugin-command-output-{}.cmd",
            std::process::id()
        ));
        std::fs::write(&path, "@echo off\r\necho plugin-%1\r\n").expect("write batch fixture");
        let cwd = path.parent().expect("batch fixture parent");

        let output =
            command_for_argv_in_dir(&path.display().to_string(), &["ready".to_string()], cwd)
                .output()
                .expect("run batch fixture");
        let _ = std::fs::remove_file(&path);

        assert!(output.status.success(), "{output:?}");
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "plugin-ready"
        );
    }

    #[cfg(windows)]
    #[test]
    fn windows_explicit_relative_executable_runs_from_working_directory() {
        let root = std::env::temp_dir().join(format!(
            "herdr-plugin-relative-command-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create relative command fixture");
        let source = PathBuf::from(std::env::var_os("SystemRoot").expect("SystemRoot"))
            .join("System32")
            .join("where.exe");
        let executable = root.join("tool.exe");
        std::fs::copy(source, &executable).expect("copy relative command fixture");

        let output = command_for_argv_in_dir("./tool.exe", &["/?".to_string()], &root)
            .output()
            .expect("run relative executable");
        let _ = std::fs::remove_dir_all(&root);

        assert!(output.status.success(), "{output:?}");
    }
}
