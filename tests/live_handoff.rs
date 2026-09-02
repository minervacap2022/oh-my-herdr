mod support;

use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, MutexGuard, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use portable_pty::{native_pty_system, Child, CommandBuilder, MasterPty, PtySize};
use support::{
    cleanup_test_base, client_handshake, register_runtime_dir, register_spawned_herdr_pid,
    send_input, unregister_spawned_herdr_pid, wait_for_disconnect, wait_for_socket,
};

struct SpawnedHerdr {
    _master: Box<dyn MasterPty + Send>,
    child: Box<dyn Child + Send + Sync>,
}

struct RequestError {
    retryable: bool,
    message: String,
}

impl Drop for SpawnedHerdr {
    fn drop(&mut self) {
        let pid = self.child.process_id();
        let _ = self.child.kill();
        unregister_spawned_herdr_pid(pid);
    }
}

fn test_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn unique_test_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    PathBuf::from(format!("/tmp/hlh-{}-{n}", std::process::id()))
}

fn spawn_server(config_home: &Path, runtime_dir: &Path, api_socket: &Path) -> SpawnedHerdr {
    spawn_server_with_env(config_home, runtime_dir, api_socket, &[])
}

fn spawn_server_with_env(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket: &Path,
    extra_env: &[(&str, &str)],
) -> SpawnedHerdr {
    spawn_server_with_config_and_env(
        config_home,
        runtime_dir,
        api_socket,
        "onboarding = false\n",
        extra_env,
    )
}

fn spawn_server_with_config_and_env(
    config_home: &Path,
    runtime_dir: &Path,
    api_socket: &Path,
    config: &str,
    extra_env: &[(&str, &str)],
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    let config_path = config_home.join("herdr/config.toml");
    fs::write(&config_path, config).unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_CONFIG_PATH", config_path);
    cmd.env("HERDR_SOCKET_PATH", api_socket);
    cmd.env(
        "HERDR_CLIENT_SOCKET_PATH",
        runtime_dir.join("herdr-client.sock"),
    );
    cmd.env("SHELL", "/bin/sh");
    for (key, value) in extra_env {
        cmd.env(key, value);
    }

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn spawn_named_session_server(
    config_home: &Path,
    runtime_dir: &Path,
    session_name: &str,
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr-dev")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    fs::write(
        config_home.join("herdr-dev/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("HERDR_SESSION", session_name);
    cmd.env_remove("HERDR_SOCKET_PATH");
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn spawn_default_session_server(config_home: &Path, runtime_dir: &Path) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr-dev")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    fs::write(
        config_home.join("herdr-dev/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env("XDG_STATE_HOME", runtime_dir.join("state"));
    cmd.env_remove("HERDR_SESSION");
    cmd.env_remove("HERDR_SOCKET_PATH");
    cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    cmd.env("SHELL", "/bin/sh");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn spawn_server_with_args_and_socket_env(
    config_home: &Path,
    runtime_dir: &Path,
    session_name: Option<&str>,
    api_socket_env: Option<&Path>,
    client_socket_env: Option<&Path>,
) -> SpawnedHerdr {
    fs::create_dir_all(config_home.join("herdr-dev")).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    fs::write(
        config_home.join("herdr-dev/config.toml"),
        "onboarding = false\n",
    )
    .unwrap();

    let pair = native_pty_system()
        .openpty(PtySize {
            rows: 24,
            cols: 80,
            pixel_width: 0,
            pixel_height: 0,
        })
        .unwrap();
    let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_herdr"));
    if let Some(session_name) = session_name {
        cmd.arg("--session");
        cmd.arg(session_name);
    }
    cmd.arg("server");
    cmd.env("XDG_CONFIG_HOME", config_home);
    cmd.env("XDG_RUNTIME_DIR", runtime_dir);
    cmd.env_remove("HERDR_SESSION");
    if let Some(api_socket_env) = api_socket_env {
        cmd.env("HERDR_SOCKET_PATH", api_socket_env);
    } else {
        cmd.env_remove("HERDR_SOCKET_PATH");
    }
    if let Some(client_socket_env) = client_socket_env {
        cmd.env("HERDR_CLIENT_SOCKET_PATH", client_socket_env);
    } else {
        cmd.env_remove("HERDR_CLIENT_SOCKET_PATH");
    }
    cmd.env("SHELL", "/bin/sh");

    let child = pair.slave.spawn_command(cmd).unwrap();
    register_spawned_herdr_pid(child.process_id());
    SpawnedHerdr {
        _master: pair.master,
        child,
    }
}

fn try_request(
    socket_path: &Path,
    request: serde_json::Value,
) -> Result<serde_json::Value, RequestError> {
    let mut stream = UnixStream::connect(socket_path).map_err(|err| RequestError {
        retryable: true,
        message: format!("connect {}: {err}", socket_path.display()),
    })?;
    let request_text = request.to_string();
    stream
        .write_all(request_text.as_bytes())
        .map_err(|err| RequestError {
            retryable: true,
            message: format!("write request to {}: {err}", socket_path.display()),
        })?;
    stream.write_all(b"\n").map_err(|err| RequestError {
        retryable: true,
        message: format!("write newline to {}: {err}", socket_path.display()),
    })?;
    stream.flush().map_err(|err| RequestError {
        retryable: true,
        message: format!("flush request to {}: {err}", socket_path.display()),
    })?;
    let mut line = String::new();
    BufReader::new(stream)
        .read_line(&mut line)
        .map_err(|err| RequestError {
            retryable: true,
            message: format!("read response from {}: {err}", socket_path.display()),
        })?;
    if line.is_empty() {
        return Err(RequestError {
            retryable: true,
            message: format!(
                "empty response from {} for request {request_text}",
                socket_path.display()
            ),
        });
    }
    serde_json::from_str(&line).map_err(|err| RequestError {
        retryable: false,
        message: format!(
            "parse response from {} for request {request_text}: {err}; response was {line:?}",
            socket_path.display()
        ),
    })
}

fn request(socket_path: &Path, request: serde_json::Value) -> serde_json::Value {
    try_request(socket_path, request).unwrap_or_else(|err| panic!("{}", err.message))
}

fn assert_ok(response: serde_json::Value) {
    assert!(
        response.get("result").is_some(),
        "api request failed: {response}"
    );
}

fn wait_for_server_stop(socket_path: &Path) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while try_request(
        socket_path,
        serde_json::json!({"id":"test:ping","method":"ping","params":{}}),
    )
    .is_ok()
    {
        assert!(
            Instant::now() < deadline,
            "replacement server did not exit after server.stop"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn wait_for_server_process_exit(pid: u32) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_state = String::new();
    while Instant::now() < deadline {
        let process_state = std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", &pid.to_string()])
            .output()
            .ok()
            .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
            .unwrap_or_default();
        if process_state.is_empty() || process_state.starts_with('Z') {
            return;
        }
        last_state = process_state;
        thread::sleep(Duration::from_millis(25));
    }
    panic!("replacement server {pid} did not exit after server.stop; last state: {last_state}");
}

fn wait_for_api(socket_path: &Path, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_error = String::new();
    while Instant::now() < deadline {
        match try_request(
            socket_path,
            serde_json::json!({"id":"test:ping","method":"ping","params":{}}),
        ) {
            Ok(response) if response.get("result").is_some() => return,
            Ok(response) => panic!("api ping returned non-success response: {response}"),
            Err(err) if !err.retryable => panic!("{}", err.message),
            Err(err) => {
                last_error = err.message;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "api did not become ready at {}; last error: {last_error}",
        socket_path.display()
    );
}

fn write_plugin_manifest(root: &Path, plugin_id: &str) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("herdr-plugin.toml"),
        format!(
            r#"id = "{plugin_id}"
name = "Live handoff test"
version = "0.1.0"
min_herdr_version = "0.6.10"
platforms = ["linux", "macos", "windows"]
"#
        ),
    )
    .unwrap();
}

fn link_plugin(socket_path: &Path, root: &Path) {
    assert_ok(request(
        socket_path,
        serde_json::json!({
            "id": "test:plugin:link",
            "method": "plugin.link",
            "params": {"path": root, "enabled": true}
        }),
    ));
}

fn create_committed_repo(path: &Path) {
    fs::create_dir_all(path).unwrap();
    assert!(Command::new("git")
        .args(["init", "--quiet"])
        .current_dir(path)
        .status()
        .unwrap()
        .success());
    for (key, value) in [
        ("user.email", "herdr@example.invalid"),
        ("user.name", "Herdr Test"),
    ] {
        assert!(Command::new("git")
            .args(["config", key, value])
            .current_dir(path)
            .status()
            .unwrap()
            .success());
    }
    fs::write(path.join("README.md"), "test\n").unwrap();
    assert!(Command::new("git")
        .args(["add", "README.md"])
        .current_dir(path)
        .status()
        .unwrap()
        .success());
    assert!(Command::new("git")
        .args(["commit", "--quiet", "-m", "initial"])
        .current_dir(path)
        .status()
        .unwrap()
        .success());
}

fn write_blocking_plugin_manifest(root: &Path, plugin_id: &str, started: &Path, release: &Path) {
    fs::create_dir_all(root).unwrap();
    fs::write(
        root.join("herdr-plugin.toml"),
        format!(
            r#"id = "{plugin_id}"
name = "Lease holder"
version = "0.1.0"
min_herdr_version = "0.6.10"
platforms = ["linux", "macos", "windows"]

[[actions]]
id = "hold"
title = "Hold"
command = ["sh", "-c", "touch {}; while [ ! -e {} ]; do sleep 0.01; done"]
"#,
            started.display(),
            release.display()
        ),
    )
    .unwrap();
}

#[cfg(unix)]
fn wait_for_plugin_command_runner_pid(timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    let runner_binary = env!("CARGO_BIN_EXE_herdr");
    let mut last_processes = String::new();
    while Instant::now() < deadline {
        let output = Command::new("ps")
            .args(["-axo", "pid=,command="])
            .output()
            .unwrap();
        last_processes = String::from_utf8_lossy(&output.stdout).into_owned();
        if let Some(pid) = last_processes.lines().find_map(|line| {
            (line.contains(runner_binary) && line.contains("--plugin-command-runner"))
                .then(|| line.split_whitespace().next())
                .flatten()
                .and_then(|pid| pid.parse().ok())
        }) {
            return pid;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "plugin command runner did not appear before timeout; processes were {last_processes:?}"
    );
}

fn create_linked_worktree(socket_path: &Path, repo: &Path, checkout: &Path) -> String {
    let parent = request(
        socket_path,
        serde_json::json!({
            "id": "test:workspace:create-parent",
            "method": "workspace.create",
            "params": {"cwd": repo, "focus": false}
        }),
    );
    assert_ok(parent.clone());
    let parent_id = parent["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string();
    let linked = request(
        socket_path,
        serde_json::json!({
            "id": "test:worktree:create",
            "method": "worktree.create",
            "params": {
                "workspace_id": parent_id,
                "branch": "worktree/plugin-command-lease",
                "path": checkout,
                "focus": false
            }
        }),
    );
    assert_ok(linked.clone());
    linked["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap()
        .to_string()
}

fn invoke_blocking_plugin(socket_path: &Path, plugin_id: &str) {
    assert_ok(request(
        socket_path,
        serde_json::json!({
            "id": "test:plugin:invoke",
            "method": "plugin.action.invoke",
            "params": {"plugin_id": plugin_id, "action_id": "hold"}
        }),
    ));
}

fn force_remove_worktree(socket_path: &Path, workspace_id: &str) -> serde_json::Value {
    request(
        socket_path,
        serde_json::json!({
            "id": "test:worktree:remove",
            "method": "worktree.remove",
            "params": {"workspace_id": workspace_id, "force": true}
        }),
    )
}

fn wait_for_force_remove(socket_path: &Path, workspace_id: &str) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let response = force_remove_worktree(socket_path, workspace_id);
        if response["error"]["code"] != "plugin_command_in_progress" {
            assert_ok(response);
            return;
        }
        assert!(
            Instant::now() < deadline,
            "plugin command lease did not release before worktree removal"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn listed_plugin_ids(socket_path: &Path) -> Vec<String> {
    let response = request(
        socket_path,
        serde_json::json!({"id":"test:plugin:list","method":"plugin.list","params":{}}),
    );
    assert_ok(response.clone());
    response["result"]["plugins"]
        .as_array()
        .unwrap()
        .iter()
        .map(|plugin| plugin["plugin_id"].as_str().unwrap().to_string())
        .collect()
}

fn saved_plugin_ids(registry_path: &Path) -> Vec<String> {
    let mut ids =
        serde_json::from_str::<Vec<serde_json::Value>>(&fs::read_to_string(registry_path).unwrap())
            .unwrap()
            .into_iter()
            .map(|plugin| plugin["plugin_id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>();
    ids.sort();
    ids
}

fn wait_for_output(socket_path: &Path, pane_id: &str, needle: &str) {
    let deadline = Instant::now() + Duration::from_secs(5);
    let mut last_text = String::new();
    let mut last_response = serde_json::Value::Null;
    while Instant::now() < deadline {
        let response = request(
            socket_path,
            serde_json::json!({
                "id": "test:pane:read",
                "method": "pane.read",
                "params": {
                    "pane_id": pane_id,
                    "source": "visible",
                    "lines": 20,
                    "format": "text",
                    "strip_ansi": true
                }
            }),
        );
        last_response = response.clone();
        let text = response["result"]["read"]["text"]
            .as_str()
            .unwrap_or_default();
        last_text = text.to_string();
        if text.contains(needle) {
            return;
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "pane output did not contain {needle:?}; last text was {last_text:?}; last response was {last_response}"
    );
}

fn wait_for_file_contains(path: &Path, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last_text = String::new();
    while Instant::now() < deadline {
        if let Ok(text) = fs::read_to_string(path) {
            last_text = text;
            if last_text.contains(needle) {
                return last_text;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "{} did not contain {needle:?}; last text was {last_text:?}",
        path.display()
    );
}

#[cfg(target_os = "linux")]
fn server_ptmx_fd_count(pid: u32) -> usize {
    let Ok(entries) = fs::read_dir(format!("/proc/{pid}/fd")) else {
        return 0;
    };
    entries
        .filter_map(Result::ok)
        .filter_map(|entry| fs::read_link(entry.path()).ok())
        // ptmx master node: /dev/ptmx or /dev/pts/ptmx (devpts); slaves /dev/pts/<N> excluded.
        .filter(|target| target == Path::new("/dev/ptmx") || target == Path::new("/dev/pts/ptmx"))
        .count()
}

#[cfg(target_os = "macos")]
fn server_ptmx_fd_count(pid: u32) -> usize {
    let Ok(output) = std::process::Command::new("lsof")
        .args(["-nP", "-p", &pid.to_string()])
        .output()
    else {
        return 0;
    };
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|line| line.contains("/dev/ptmx"))
        .count()
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
fn wait_for_server_ptmx_fd_count(pid: u32, expected: usize, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    let mut last_count = 0;
    while Instant::now() < deadline {
        last_count = server_ptmx_fd_count(pid);
        if last_count == expected {
            return;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!("server pid {pid} had {last_count} ptmx master fds; expected {expected}");
}

#[cfg(target_os = "linux")]
fn wait_for_replacement_server_pid(runtime_dir: &Path, old_pid: u32, timeout: Duration) -> u32 {
    let deadline = Instant::now() + timeout;
    let mut last_pids = Vec::new();
    while Instant::now() < deadline {
        last_pids = support::herdr_server_pids_for_runtime_dir(runtime_dir).unwrap_or_default();
        if let Some(pid) = last_pids.iter().copied().find(|pid| *pid != old_pid) {
            return pid;
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "replacement server for {} did not appear; last pids: {:?}",
        runtime_dir.display(),
        last_pids
    );
}

#[cfg(target_os = "macos")]
fn wait_for_replacement_server_pid(runtime_dir: &Path, old_pid: u32, timeout: Duration) -> u32 {
    let api_socket = runtime_dir.join("herdr.sock");
    let socket_env = format!("HERDR_SOCKET_PATH={}", api_socket.display());
    let deadline = Instant::now() + timeout;
    let mut last_stdout = String::new();
    while Instant::now() < deadline {
        if let Ok(output) = std::process::Command::new("ps")
            .args(["eww", "-ax"])
            .output()
        {
            last_stdout = String::from_utf8_lossy(&output.stdout).into_owned();
            for line in last_stdout.lines() {
                let Some(pid_text) = line.split_whitespace().next() else {
                    continue;
                };
                let Ok(pid) = pid_text.parse::<u32>() else {
                    continue;
                };
                if pid == old_pid {
                    continue;
                }
                if line.contains("server --handoff-import") && line.contains(&socket_env) {
                    return pid;
                }
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "replacement server for {} did not appear; last pgrep output: {}",
        runtime_dir.display(),
        last_stdout
    );
}

fn unused_local_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

fn wait_for_http_contains(port: u16, needle: &str, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    let mut last_response = String::new();
    while Instant::now() < deadline {
        if let Ok(mut stream) = TcpStream::connect(("127.0.0.1", port)) {
            let _ =
                stream.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n");
            let mut response = String::new();
            let _ = stream.read_to_string(&mut response);
            last_response = response;
            if last_response.contains(needle) {
                return last_response;
            }
        }
        thread::sleep(Duration::from_millis(50));
    }
    panic!(
        "http server on port {port} did not return {needle:?}; last response was {last_response:?}"
    );
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
#[test]
fn live_server_holds_one_pty_master_fd_per_pane() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let server_pid = spawned
        .child
        .process_id()
        .expect("test server should expose pid");
    wait_for_server_ptmx_fd_count(server_pid, 0, Duration::from_secs(5));

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_server_ptmx_fd_count(server_pid, 1, Duration::from_secs(5));

    let second = request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:split-second",
            "method": "pane.split",
            "params": {
                "target_pane_id": pane_id,
                "direction": "right",
                "focus": true
            }
        }),
    );
    assert_ok(second.clone());
    let second_pane_id = second["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    wait_for_server_ptmx_fd_count(server_pid, 2, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:split-third",
            "method": "pane.split",
            "params": {
                "target_pane_id": second_pane_id,
                "direction": "down",
                "focus": true
            }
        }),
    ));
    wait_for_server_ptmx_fd_count(server_pid, 3, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    let replacement_pid =
        wait_for_replacement_server_pid(&runtime_dir, server_pid, Duration::from_secs(10));
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_server_ptmx_fd_count(replacement_pid, 3, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    ));
    wait_for_server_stop(&api_socket);
    drop(spawned);
    wait_for_server_process_exit(replacement_pid);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_named_session_socket_paths() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let session_dir = config_home.join("herdr-dev/sessions/work");
    let api_socket = session_dir.join("herdr.sock");
    let client_socket = session_dir.join("herdr-client.sock");

    let spawned = spawn_named_session_server(&config_home, &runtime_dir, "work");
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));
    assert!(
        !config_home.join("herdr-dev/herdr.sock").exists(),
        "named handoff unexpectedly bound the default session API socket"
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_ignores_leaked_default_socket_env_for_named_session() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let default_session_dir = config_home.join("herdr-dev");
    let default_api_socket = default_session_dir.join("herdr.sock");
    let default_client_socket = default_session_dir.join("herdr-client.sock");
    let work_session_dir = config_home.join("herdr-dev/sessions/work");
    let work_api_socket = work_session_dir.join("herdr.sock");
    let work_client_socket = work_session_dir.join("herdr-client.sock");

    let default_spawned = spawn_default_session_server(&config_home, &runtime_dir);
    wait_for_socket(&default_api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let work_spawned = spawn_server_with_args_and_socket_env(
        &config_home,
        &runtime_dir,
        Some("work"),
        Some(&default_api_socket),
        Some(&default_client_socket),
    );
    wait_for_socket(&work_api_socket, Duration::from_secs(10));

    assert_ok(request(
        &work_api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(work_spawned);
    wait_for_api(&default_api_socket, Duration::from_secs(10));
    wait_for_api(&work_api_socket, Duration::from_secs(10));
    wait_for_socket(&work_client_socket, Duration::from_secs(5));

    let _ = request(
        &work_api_socket,
        serde_json::json!({"id":"test:stop-work","method":"server.stop","params":{}}),
    );
    let _ = request(
        &default_api_socket,
        serde_json::json!({"id":"test:stop-default","method":"server.stop","params":{}}),
    );
    drop(default_spawned);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_client_socket_env_without_api_socket_env() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = config_home.join("herdr-dev/herdr.sock");
    let client_socket = runtime_dir.join("custom-client.sock");

    let spawned = spawn_server_with_args_and_socket_env(
        &config_home,
        &runtime_dir,
        None,
        None,
        Some(&client_socket),
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_installed_plugins() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = config_home.join("herdr-dev/herdr.sock");
    let registry_path = config_home.join("herdr-dev/plugins.json");
    let existing_plugin = base.join("plugins/existing");
    let added_plugin = base.join("plugins/added");
    write_plugin_manifest(&existing_plugin, "test.live-handoff-existing");
    write_plugin_manifest(&added_plugin, "test.live-handoff-added");

    let spawned = spawn_default_session_server(&config_home, &runtime_dir);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    link_plugin(&api_socket, &existing_plugin);
    assert_eq!(
        listed_plugin_ids(&api_socket),
        ["test.live-handoff-existing"]
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    assert_eq!(
        listed_plugin_ids(&api_socket),
        ["test.live-handoff-existing"]
    );
    link_plugin(&api_socket, &added_plugin);
    assert_eq!(
        saved_plugin_ids(&registry_path),
        ["test.live-handoff-added", "test.live-handoff-existing"]
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn plugin_command_lease_survives_server_stop_and_replacement() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = config_home.join("herdr-dev/herdr.sock");
    let repo = base.join("repo");
    let checkout = base.join("checkout");
    let plugin_root = checkout.join("plugin");
    let started = plugin_root.join("started");
    let release = plugin_root.join("release");
    let plugin_id = "test.stop-plugin-command-lease";
    create_committed_repo(&repo);

    let spawned = spawn_default_session_server(&config_home, &runtime_dir);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let workspace_id = create_linked_worktree(&api_socket, &repo, &checkout);
    write_blocking_plugin_manifest(&plugin_root, plugin_id, &started, &release);
    link_plugin(&api_socket, &plugin_root);
    invoke_blocking_plugin(&api_socket, plugin_id);
    wait_for_file_contains(&started, "", Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    ));
    wait_for_server_stop(&api_socket);
    drop(spawned);

    let replacement = spawn_default_session_server(&config_home, &runtime_dir);
    wait_for_api(&api_socket, Duration::from_secs(10));
    let blocked = force_remove_worktree(&api_socket, &workspace_id);
    assert_eq!(blocked["error"]["code"], "plugin_command_in_progress");
    assert!(checkout.exists());

    fs::write(&release, b"release").unwrap();
    wait_for_force_remove(&api_socket, &workspace_id);
    assert!(!checkout.exists());

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(replacement);
    cleanup_test_base(&base);
}

#[test]
fn plugin_command_lease_survives_live_handoff() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = config_home.join("herdr-dev/herdr.sock");
    let repo = base.join("repo");
    let checkout = base.join("checkout");
    let plugin_root = checkout.join("plugin");
    let started = plugin_root.join("started");
    let release = plugin_root.join("release");
    let plugin_id = "test.handoff-plugin-command-lease";
    create_committed_repo(&repo);

    let spawned = spawn_default_session_server(&config_home, &runtime_dir);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let workspace_id = create_linked_worktree(&api_socket, &repo, &checkout);
    write_blocking_plugin_manifest(&plugin_root, plugin_id, &started, &release);
    link_plugin(&api_socket, &plugin_root);
    invoke_blocking_plugin(&api_socket, plugin_id);
    wait_for_file_contains(&started, "", Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    let blocked = force_remove_worktree(&api_socket, &workspace_id);
    assert_eq!(blocked["error"]["code"], "plugin_command_in_progress");
    assert!(checkout.exists());

    fs::write(&release, b"release").unwrap();
    wait_for_force_remove(&api_socket, &workspace_id);
    assert!(!checkout.exists());

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_agent_registry_after_immediate_profile_edit() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let context = base.join("reviewer.md");
    fs::create_dir_all(&base).unwrap();
    fs::write(&context, "reviewer context\n").unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": base, "focus": true}
        }),
    ));
    let profile = request(
        &api_socket,
        serde_json::json!({
            "id": "test:profile:set-md",
            "method": "agent.profile.set_md",
            "params": {
                "role": "reviewer",
                "name": "reviewer.md",
                "path": context,
            }
        }),
    );
    assert_ok(profile);

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    ));
    wait_for_server_stop(&api_socket);
    let registry_path = config_home.join("herdr-dev/agents.json");
    let registry = wait_for_file_contains(&registry_path, "\"reviewer\"", Duration::from_secs(5));
    let registry: serde_json::Value = serde_json::from_str(&registry).unwrap();
    assert_eq!(
        registry["profiles"]["reviewer"]["native_cwd"],
        fs::canonicalize(&base).unwrap().display().to_string()
    );
    assert_eq!(
        registry["profiles"]["reviewer"]["mds"][0]["path"],
        fs::canonicalize(&context).unwrap().display().to_string()
    );

    cleanup_test_base(&base);
}

#[cfg(unix)]
#[test]
fn completed_plugin_command_after_runner_kill_releases_durable_lease() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = config_home.join("herdr-dev/herdr.sock");
    let repo = base.join("repo");
    let checkout = base.join("checkout");
    let plugin_root = checkout.join("plugin");
    let started = plugin_root.join("started");
    let release = plugin_root.join("release");
    let finished = plugin_root.join("finished");
    let plugin_id = "test.killed-plugin-command-runner";
    create_committed_repo(&repo);

    let spawned = spawn_default_session_server(&config_home, &runtime_dir);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let workspace_id = create_linked_worktree(&api_socket, &repo, &checkout);
    fs::create_dir_all(&plugin_root).unwrap();
    fs::write(
        plugin_root.join("herdr-plugin.toml"),
        format!(
            r#"id = "{plugin_id}"
name = "Killed runner lease"
version = "0.1.0"
min_herdr_version = "0.6.10"
platforms = ["linux", "macos", "windows"]

[[actions]]
id = "hold"
title = "Hold"
command = ["sh", "-c", "touch {}; while [ ! -e {} ]; do sleep 0.01; done; touch {}"]
"#,
            started.display(),
            release.display(),
            finished.display()
        ),
    )
    .unwrap();
    link_plugin(&api_socket, plugin_root.as_path());
    invoke_blocking_plugin(&api_socket, plugin_id);
    wait_for_file_contains(&started, "", Duration::from_secs(5));

    let runner_pid = wait_for_plugin_command_runner_pid(Duration::from_secs(5));
    assert_eq!(
        unsafe { libc::kill(runner_pid as libc::pid_t, libc::SIGKILL) },
        0
    );
    fs::write(&release, b"release").unwrap();
    wait_for_file_contains(&finished, "", Duration::from_secs(5));

    wait_for_force_remove(&api_socket, &workspace_id);
    assert!(!checkout.exists());

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn shared_session_profile_updates_preserve_both_servers_changes() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let first_runtime_dir = base.join("runtime-first");
    let second_runtime_dir = base.join("runtime-second");
    let first_api_socket = first_runtime_dir.join("first.sock");
    let second_api_socket = second_runtime_dir.join("second.sock");
    let session_name = "shared-profile-registry";

    let first = spawn_server_with_env(
        &config_home,
        &first_runtime_dir,
        &first_api_socket,
        &[("HERDR_SESSION", session_name)],
    );
    let second = spawn_server_with_env(
        &config_home,
        &second_runtime_dir,
        &second_api_socket,
        &[("HERDR_SESSION", session_name)],
    );
    wait_for_socket(&first_api_socket, Duration::from_secs(10));
    wait_for_socket(&second_api_socket, Duration::from_secs(10));
    register_runtime_dir(&first_runtime_dir);
    register_runtime_dir(&second_runtime_dir);

    assert_ok(request(
        &first_api_socket,
        serde_json::json!({
            "id": "test:first-profile",
            "method": "agent.profile.set",
            "params": {"role": "reviewer", "harness": "claude"}
        }),
    ));
    assert_ok(request(
        &second_api_socket,
        serde_json::json!({
            "id": "test:second-profile",
            "method": "agent.profile.set",
            "params": {"role": "architect", "harness": "codex"}
        }),
    ));
    let architect_from_first = request(
        &first_api_socket,
        serde_json::json!({
            "id": "test:architect-from-first",
            "method": "agent.profile.get",
            "params": {"role": "architect"}
        }),
    );
    assert_ok(architect_from_first.clone());
    assert_eq!(
        architect_from_first["result"]["profile"]["harness"],
        "codex"
    );
    let profiles_from_second = request(
        &second_api_socket,
        serde_json::json!({
            "id": "test:profiles-from-second",
            "method": "agent.profile.list",
            "params": {}
        }),
    );
    assert_ok(profiles_from_second.clone());
    let profile_roles = profiles_from_second["result"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|profile| profile["role"].as_str())
        .collect::<Vec<_>>();
    assert!(profile_roles.contains(&"reviewer"));
    assert!(profile_roles.contains(&"architect"));

    let registry_path = config_home
        .join("herdr-dev")
        .join("sessions")
        .join(session_name)
        .join("agents.json");
    let registry = wait_for_file_contains(&registry_path, "\"architect\"", Duration::from_secs(5));
    let registry: serde_json::Value = serde_json::from_str(&registry).unwrap();
    assert!(
        registry["profiles"].get("reviewer").is_some(),
        "first server's profile was lost: {registry}"
    );
    assert!(
        registry["profiles"].get("architect").is_some(),
        "second server's profile was lost: {registry}"
    );

    drop(second);
    drop(first);
    cleanup_test_base(&base);
}

#[test]
fn shared_session_same_role_spawns_reserve_distinct_names() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let first_runtime_dir = base.join("runtime-first");
    let second_runtime_dir = base.join("runtime-second");
    let first_api_socket = first_runtime_dir.join("first.sock");
    let second_api_socket = second_runtime_dir.join("second.sock");
    let session_name = "shared-spawn-registry";
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_claude = bin.join("claude");
    fs::write(
        &fake_claude,
        "#!/bin/sh\nwhile IFS= read -r _; do :; done\n",
    )
    .unwrap();
    fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o755)).unwrap();
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{inherited_path}", bin.display());

    let first = spawn_server_with_config_and_env(
        &config_home,
        &first_runtime_dir,
        &first_api_socket,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
        &[("HERDR_SESSION", session_name), ("PATH", path.as_str())],
    );
    let second = spawn_server_with_config_and_env(
        &config_home,
        &second_runtime_dir,
        &second_api_socket,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
        &[("HERDR_SESSION", session_name), ("PATH", path.as_str())],
    );
    wait_for_socket(&first_api_socket, Duration::from_secs(10));
    wait_for_socket(&second_api_socket, Duration::from_secs(10));
    register_runtime_dir(&first_runtime_dir);
    register_runtime_dir(&second_runtime_dir);

    for (socket, id) in [
        (&first_api_socket, "test:first-workspace"),
        (&second_api_socket, "test:second-workspace"),
    ] {
        assert_ok(request(
            socket,
            serde_json::json!({
                "id": id,
                "method": "workspace.create",
                "params": {"cwd": base, "focus": true}
            }),
        ));
    }

    let first_spawn = request(
        &first_api_socket,
        serde_json::json!({
            "id": "test:first-spawn",
            "method": "agent.spawn",
            "params": {
                "role": "reviewer",
                "kind": "claude",
                "cwd_mode": "tab",
                "timeout_ms": 5000
            }
        }),
    );
    assert_ok(first_spawn.clone());
    assert_eq!(first_spawn["result"]["spawned"]["name"], "reviewer");
    let first_pane_id = first_spawn["result"]["spawned"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let second_spawn = request(
        &second_api_socket,
        serde_json::json!({
            "id": "test:second-spawn",
            "method": "agent.spawn",
            "params": {
                "role": "reviewer",
                "kind": "claude",
                "cwd_mode": "tab",
                "timeout_ms": 5000
            }
        }),
    );
    assert_ok(second_spawn.clone());
    assert_eq!(
        second_spawn["result"]["spawned"]["name"],
        "reviewer-replica-1"
    );
    let second_pane_id = second_spawn["result"]["spawned"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(first_pane_id, second_pane_id);

    let registry_path = config_home
        .join("herdr-dev")
        .join("sessions")
        .join(session_name)
        .join("agents.json");
    let registry =
        wait_for_file_contains(&registry_path, "reviewer-replica-1", Duration::from_secs(5));
    let registry: serde_json::Value = serde_json::from_str(&registry).unwrap();
    assert_eq!(registry["profiles"]["reviewer"]["replicas_assigned"], 1);
    let roster_names = registry["roster"]
        .as_object()
        .unwrap()
        .values()
        .map(|entry| entry["display_name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(roster_names.contains(&"reviewer"));
    assert!(roster_names.contains(&"reviewer-replica-1"));

    assert_ok(request(
        &second_api_socket,
        serde_json::json!({
            "id": "test:second-working",
            "method": "pane.report_agent",
            "params": {
                "pane_id": second_pane_id,
                "source": "shared-registry-test",
                "agent": "claude",
                "state": "working",
                "seq": 1
            }
        }),
    ));
    let registry = wait_for_file_contains(&registry_path, "\"working\"", Duration::from_secs(5));
    let registry: serde_json::Value = serde_json::from_str(&registry).unwrap();
    let roster = registry["roster"].as_object().unwrap();
    let status_for = |display_name: &str| {
        roster
            .values()
            .find(|entry| entry["display_name"] == display_name)
            .and_then(|entry| entry["status"].as_str())
    };
    assert_eq!(status_for("reviewer"), Some("active"));
    assert_eq!(status_for("reviewer-replica-1"), Some("working"));

    drop(second);
    drop(first);
    cleanup_test_base(&base);
}

#[test]
fn shared_session_start_reserves_primary_name_for_spawn() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let first_runtime_dir = base.join("runtime-first");
    let second_runtime_dir = base.join("runtime-second");
    let first_api_socket = first_runtime_dir.join("first.sock");
    let second_api_socket = second_runtime_dir.join("second.sock");
    let session_name = "shared-start-registry";
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_pi = bin.join("pi");
    fs::write(&fake_pi, "#!/bin/sh\nwhile IFS= read -r _; do :; done\n").unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{inherited_path}", bin.display());

    let first = spawn_server_with_config_and_env(
        &config_home,
        &first_runtime_dir,
        &first_api_socket,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
        &[("HERDR_SESSION", session_name), ("PATH", path.as_str())],
    );
    let second = spawn_server_with_config_and_env(
        &config_home,
        &second_runtime_dir,
        &second_api_socket,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
        &[("HERDR_SESSION", session_name), ("PATH", path.as_str())],
    );
    wait_for_socket(&first_api_socket, Duration::from_secs(10));
    wait_for_socket(&second_api_socket, Duration::from_secs(10));
    register_runtime_dir(&first_runtime_dir);
    register_runtime_dir(&second_runtime_dir);

    let first_workspace = request(
        &first_api_socket,
        serde_json::json!({
            "id": "test:first-workspace",
            "method": "workspace.create",
            "params": {"cwd": base, "focus": true}
        }),
    );
    assert_ok(first_workspace.clone());
    let first_pane_id = first_workspace["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let first_start = request(
        &first_api_socket,
        serde_json::json!({
            "id": "test:first-start",
            "method": "agent.start",
            "params": {
                "name": "reviewer",
                "kind": "pi",
                "pane_id": first_pane_id,
                "timeout_ms": 5000
            }
        }),
    );
    assert_ok(first_start);

    assert_ok(request(
        &second_api_socket,
        serde_json::json!({
            "id": "test:second-workspace",
            "method": "workspace.create",
            "params": {"cwd": base, "focus": true}
        }),
    ));
    let second_spawn = request(
        &second_api_socket,
        serde_json::json!({
            "id": "test:second-spawn",
            "method": "agent.spawn",
            "params": {
                "role": "reviewer",
                "kind": "pi",
                "cwd_mode": "tab",
                "timeout_ms": 5000
            }
        }),
    );
    assert_ok(second_spawn.clone());
    assert_eq!(
        second_spawn["result"]["spawned"]["name"],
        "reviewer-replica-1"
    );

    let registry_path = config_home
        .join("herdr-dev")
        .join("sessions")
        .join(session_name)
        .join("agents.json");
    let registry =
        wait_for_file_contains(&registry_path, "reviewer-replica-1", Duration::from_secs(5));
    let registry: serde_json::Value = serde_json::from_str(&registry).unwrap();
    let roster = registry["roster"].as_object().unwrap();
    let live_names = roster
        .values()
        .filter(|entry| entry["status"] != "terminated")
        .filter(|entry| entry["last_pane"].is_string())
        .map(|entry| entry["display_name"].as_str().unwrap())
        .collect::<Vec<_>>();
    assert!(live_names.contains(&"reviewer"));
    assert!(live_names.contains(&"reviewer-replica-1"));

    drop(second);
    drop(first);
    cleanup_test_base(&base);
}

#[test]
fn shared_session_spawn_uses_the_latest_profile_snapshot() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let first_runtime_dir = base.join("runtime-first");
    let second_runtime_dir = base.join("runtime-second");
    let first_api_socket = first_runtime_dir.join("first.sock");
    let second_api_socket = second_runtime_dir.join("second.sock");
    let session_name = "shared-profile-spawn";
    let bin = base.join("bin");
    let launched_kind = base.join("launched-kind");
    fs::create_dir_all(&bin).unwrap();
    for (kind, executable) in [("claude", "claude"), ("codex", "codex")] {
        let path = bin.join(executable);
        fs::write(
            &path,
            format!(
                "#!/bin/sh\nprintf '%s' {kind} > {}\nwhile IFS= read -r _; do :; done\n",
                launched_kind.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    }
    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{inherited_path}", bin.display());

    let first = spawn_server_with_config_and_env(
        &config_home,
        &first_runtime_dir,
        &first_api_socket,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
        &[("HERDR_SESSION", session_name), ("PATH", path.as_str())],
    );
    let second = spawn_server_with_config_and_env(
        &config_home,
        &second_runtime_dir,
        &second_api_socket,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
        &[("HERDR_SESSION", session_name), ("PATH", path.as_str())],
    );
    wait_for_socket(&first_api_socket, Duration::from_secs(10));
    wait_for_socket(&second_api_socket, Duration::from_secs(10));
    register_runtime_dir(&first_runtime_dir);
    register_runtime_dir(&second_runtime_dir);

    assert_ok(request(
        &first_api_socket,
        serde_json::json!({
            "id": "test:profile",
            "method": "agent.profile.set",
            "params": {"role": "reviewer", "harness": "claude"}
        }),
    ));
    assert_ok(request(
        &second_api_socket,
        serde_json::json!({
            "id": "test:workspace",
            "method": "workspace.create",
            "params": {"cwd": base, "focus": true}
        }),
    ));
    assert_ok(request(
        &second_api_socket,
        serde_json::json!({
            "id": "test:spawn",
            "method": "agent.spawn",
            "params": {
                "role": "reviewer",
                "cwd_mode": "tab",
                "timeout_ms": 5000
            }
        }),
    ));
    assert_eq!(
        wait_for_file_contains(&launched_kind, "claude", Duration::from_secs(5)),
        "claude"
    );

    drop(second);
    drop(first);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_typed_profile_and_saved_harness_spawn() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let workspace_cwd = base.join("workspace");
    let agent_cwd = base.join("agent-cwd");
    let context = base.join("reviewer.md");
    let bin = base.join("bin");
    let argv_marker = base.join("claude-argv");
    let cwd_marker = base.join("claude-cwd");

    fs::create_dir_all(&workspace_cwd).unwrap();
    fs::create_dir_all(&agent_cwd).unwrap();
    fs::create_dir_all(&bin).unwrap();
    fs::write(&context, "persistent reviewer context\n").unwrap();
    let fake_claude = bin.join("claude");
    fs::write(
        &fake_claude,
        format!(
            "#!/bin/sh\npwd > {}\nprintf '%s\\n' \"$@\" > {}\nexport HERDR_AGENT=claude\nwhile IFS= read -r _; do :; done\n",
            cwd_marker.display(),
            argv_marker.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_claude, fs::Permissions::from_mode(0o755)).unwrap();

    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{inherited_path}", bin.display());
    let spawned = spawn_server_with_config_and_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
        &[("PATH", path.as_str())],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": workspace_cwd, "focus": true}
        }),
    ));
    let canonical_cwd = fs::canonicalize(&agent_cwd).unwrap();
    let canonical_context = fs::canonicalize(&context).unwrap();

    let set = request(
        &api_socket,
        serde_json::json!({
            "id": "test:profile:set",
            "method": "agent.profile.set",
            "params": {
                "role": "reviewer",
                "harness": "claude",
                "native_cwd": agent_cwd,
                "model": "sonnet",
                "effort": "high",
                "apikey_ref": "keychain:reviewer",
                "allowlist": ["read", "write"]
            }
        }),
    );
    assert_ok(set.clone());
    let profile = &set["result"]["profile"];
    assert_eq!(profile["harness"], "claude");
    assert_eq!(profile["native_cwd"], canonical_cwd.display().to_string());
    assert_eq!(profile["model"], "sonnet");
    assert_eq!(profile["effort"], "high");
    assert_eq!(profile["apikey_ref"], "keychain:reviewer");
    assert_eq!(profile["allowlist"], serde_json::json!(["read", "write"]));

    let set_md = request(
        &api_socket,
        serde_json::json!({
            "id": "test:profile:set-md",
            "method": "agent.profile.set_md",
            "params": {
                "role": "reviewer",
                "name": "reviewer.md",
                "path": context
            }
        }),
    );
    assert_ok(set_md.clone());
    assert_eq!(
        set_md["result"]["profile"]["mds"],
        serde_json::json!([{
            "name": "reviewer.md",
            "path": canonical_context.display().to_string()
        }])
    );

    let get_before_clear = request(
        &api_socket,
        serde_json::json!({
            "id": "test:profile:get-before-clear",
            "method": "agent.profile.get",
            "params": {"role": "reviewer"}
        }),
    );
    assert_eq!(get_before_clear["result"]["profile"]["model"], "sonnet");
    let list_before_clear = request(
        &api_socket,
        serde_json::json!({
            "id": "test:profile:list-before-clear",
            "method": "agent.profile.list",
            "params": {}
        }),
    );
    assert!(list_before_clear["result"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .any(|profile| profile["role"] == "reviewer"));

    let clear = request(
        &api_socket,
        serde_json::json!({
            "id": "test:profile:clear",
            "method": "agent.profile.set",
            "params": {
                "role": "reviewer",
                "clear_model": true,
                "clear_effort": true,
                "clear_apikey_ref": true,
                "clear_allowlist": true
            }
        }),
    );
    assert_ok(clear.clone());
    for field in ["model", "effort", "apikey_ref", "allowlist"] {
        assert!(
            clear["result"]["profile"].get(field).is_none(),
            "{field} was not cleared: {clear}"
        );
    }

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    let get_after_handoff = request(
        &api_socket,
        serde_json::json!({
            "id": "test:profile:get-after-handoff",
            "method": "agent.profile.get",
            "params": {"role": "reviewer"}
        }),
    );
    let persisted = &get_after_handoff["result"]["profile"];
    assert_eq!(persisted["harness"], "claude");
    assert_eq!(persisted["native_cwd"], canonical_cwd.display().to_string());
    assert_eq!(
        persisted["mds"],
        serde_json::json!([{
            "name": "reviewer.md",
            "path": canonical_context.display().to_string()
        }])
    );
    for field in ["model", "effort", "apikey_ref", "allowlist"] {
        assert!(
            persisted.get(field).is_none(),
            "{field} reappeared after handoff: {persisted}"
        );
    }
    let list_after_handoff = request(
        &api_socket,
        serde_json::json!({
            "id": "test:profile:list-after-handoff",
            "method": "agent.profile.list",
            "params": {}
        }),
    );
    let profiles = list_after_handoff["result"]["profiles"].as_array().unwrap();
    assert_eq!(profiles.len(), 1);
    assert_eq!(profiles[0], persisted.clone());

    let launched = request(
        &api_socket,
        serde_json::json!({
            "id": "test:spawn:from-profile",
            "method": "agent.spawn",
            "params": {"role": "reviewer", "cwd_mode": "agent", "timeout_ms": 5000}
        }),
    );
    assert_ok(launched.clone());
    assert_eq!(
        launched["result"]["spawned"]["argv"],
        serde_json::json!([
            "claude",
            "--append-system-prompt-file",
            canonical_context.display().to_string()
        ])
    );
    assert_eq!(
        wait_for_file_contains(
            &cwd_marker,
            &canonical_cwd.display().to_string(),
            Duration::from_secs(5),
        )
        .trim(),
        canonical_cwd.display().to_string()
    );
    assert_eq!(
        wait_for_file_contains(
            &argv_marker,
            "--append-system-prompt-file",
            Duration::from_secs(5),
        ),
        format!(
            "--append-system-prompt-file\n{}\n",
            canonical_context.display()
        )
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    ));
    wait_for_server_stop(&api_socket);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_elapsed_recent_input_guard() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let bin = base.join("bin");
    let fake_pi = bin.join("pi");
    let delayed_import = base.join("delayed-import");
    fs::create_dir_all(&bin).unwrap();
    fs::write(&fake_pi, "#!/bin/sh\nexec /bin/sleep 30\n").unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        &delayed_import,
        format!(
            "#!/bin/sh\nsleep 1\nexec {} \"$@\"\n",
            env!("CARGO_BIN_EXE_herdr")
        ),
    )
    .unwrap();
    fs::set_permissions(&delayed_import, fs::Permissions::from_mode(0o755)).unwrap();

    let inherited_path = std::env::var("PATH").unwrap_or_default();
    let path = format!("{}:{inherited_path}", bin.display());
    let spawned = spawn_server_with_config_and_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
        &[("PATH", path.as_str())],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let workspace = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": base, "focus": true}
        }),
    );
    assert_ok(workspace.clone());
    let pane_id = workspace["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:recent-input",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "true", "keys": ["Enter"]}
        }),
    ));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:handoff",
            "method": "server.live_handoff",
            "params": {"import_exe": delayed_import}
        }),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    let spawned_agent = request(
        &api_socket,
        serde_json::json!({
            "id": "test:spawn-after-guard",
            "method": "agent.spawn",
            "params": {"role": "reviewer", "kind": "pi", "timeout_ms": 5000}
        }),
    );
    assert_ok(spawned_agent.clone());
    assert_eq!(spawned_agent["result"]["spawned"]["pane_id"], pane_id);
    assert_eq!(spawned_agent["result"]["spawned"]["split"], false);

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    ));
    wait_for_server_stop(&api_socket);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_pane_process_io() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let marker = base.join("child.pid");
    let second_marker = base.join("second-child.pid");
    let hup_marker = base.join("hup");
    let second_hup_marker = base.join("second-hup");
    let received_marker = base.join("received");
    let second_received_marker = base.join("second-received");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:split",
            "method": "pane.split",
            "params": {
                "target_pane_id": pane_id,
                "direction": "right",
                "focus": false
            }
        }),
    );
    assert_ok(split.clone());
    let second_pane_id = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let command = format!(
        "sh -c 'echo READY $$ > {}; trap \"echo HUP >> {}\" HUP; while read line; do echo got:$line; echo got:$line >> {}; done'",
        marker.display(),
        hup_marker.display(),
        received_marker.display()
    );
    let second_command = format!(
        "sh -c 'echo SECOND_READY $$ > {}; trap \"echo HUP >> {}\" HUP; while read line; do echo second:$line; echo second:$line >> {}; done'",
        second_marker.display(),
        second_hup_marker.display(),
        second_received_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:second-pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": second_pane_id, "text": second_command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&marker, Duration::from_secs(5));
    support::wait_for_file(&second_marker, Duration::from_secs(5));
    let pid_text = fs::read_to_string(&marker).unwrap();
    let child_pid: u32 = pid_text.split_whitespace().last().unwrap().parse().unwrap();
    let second_pid_text = fs::read_to_string(&second_marker).unwrap();
    let second_child_pid: u32 = second_pid_text
        .split_whitespace()
        .last()
        .unwrap()
        .parse()
        .unwrap();
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);
    assert_eq!(unsafe { libc::kill(second_child_pid as libc::pid_t, 0) }, 0);

    let protocol = request(
        &api_socket,
        serde_json::json!({"id":"test:protocol","method":"ping","params":{}}),
    )["result"]["protocol"]
        .as_u64()
        .unwrap() as u32;
    let mut client_stream = UnixStream::connect(&client_socket).unwrap();
    let (server_protocol, error) = client_handshake(&mut client_stream, protocol, 80, 24).unwrap();
    assert_eq!(server_protocol, protocol);
    assert!(error.is_none(), "client handshake failed: {error:?}");

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:before-log",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "before_replay", "keys": ["Enter"]}
        }),
    ));
    wait_for_output(&api_socket, &pane_id, "got:before_replay");

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    assert!(
        wait_for_disconnect(&mut client_stream, Duration::from_secs(5)).unwrap(),
        "connected clients should disconnect during live handoff"
    );
    thread::sleep(Duration::from_millis(300));
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);
    assert_eq!(unsafe { libc::kill(second_child_pid as libc::pid_t, 0) }, 0);
    assert!(
        !hup_marker.exists(),
        "pane process received HUP during handoff"
    );
    assert!(
        !second_hup_marker.exists(),
        "second pane process received HUP during handoff"
    );
    wait_for_output(&api_socket, &pane_id, "got:before_replay");

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:send",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "after-handoff", "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &received_marker,
        "got:after-handoff",
        Duration::from_secs(5),
    );
    wait_for_output(&api_socket, &pane_id, "got:after-handoff");
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:second-pane:send",
            "method": "pane.send_input",
            "params": {"pane_id": second_pane_id, "text": "after-handoff-second", "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &second_received_marker,
        "second:after-handoff-second",
        Duration::from_secs(5),
    );
    wait_for_output(&api_socket, &second_pane_id, "second:after-handoff-sec");

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    let _ = client_socket;
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_retries_session_persistence_after_storage_recovers() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let app_dir = if cfg!(debug_assertions) {
        "herdr-dev"
    } else {
        "herdr"
    };
    let session_path = config_home.join(app_dir).join("session.json");
    fs::create_dir_all(session_path.parent().unwrap()).unwrap();
    fs::create_dir(&session_path).unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let label = "handoff-save-after-storage-recovery";
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"label": label}
        }),
    ));
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:handoff",
            "method": "server.live_handoff",
            "params": {}
        }),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    assert!(
        session_path.is_dir(),
        "the forced pre-handoff save should fail while storage is blocked"
    );
    fs::remove_dir(&session_path).unwrap();

    let saved = wait_for_file_contains(&session_path, label, Duration::from_secs(8));
    assert!(saved.contains(label));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    ));
    wait_for_server_stop(&api_socket);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_keyboard_protocol_for_client_input() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let script = base.join("read-raw.py");
    let ready_marker = base.join("keyboard-ready");
    let received_marker = base.join("keyboard-received");

    fs::create_dir_all(&base).unwrap();
    fs::write(
        &script,
        format!(
            r#"import os
import pathlib
import select
import sys
import tty

sys.stdout.buffer.write(b"\x1b[>5u")
sys.stdout.flush()
pathlib.Path({ready:?}).write_text("ready")
tty.setraw(sys.stdin.fileno())
ready_fds, _, _ = select.select([sys.stdin.fileno()], [], [], 5)
data = os.read(sys.stdin.fileno(), 32) if ready_fds else b""
pathlib.Path({received:?}).write_text(data.hex())
"#,
            ready = ready_marker.display().to_string(),
            received = received_marker.display().to_string()
        ),
    )
    .unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("python3 {}", script.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&ready_marker, Duration::from_secs(5));

    let protocol = request(
        &api_socket,
        serde_json::json!({"id":"test:protocol","method":"ping","params":{}}),
    )["result"]["protocol"]
        .as_u64()
        .unwrap() as u32;
    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let mut client_stream = UnixStream::connect(&client_socket).unwrap();
    let (server_protocol, error) = client_handshake(&mut client_stream, protocol, 80, 24).unwrap();
    assert_eq!(server_protocol, protocol);
    assert!(error.is_none(), "client handshake failed: {error:?}");
    send_input(&mut client_stream, b"\x1b[13;2u").unwrap();

    wait_for_file_contains(&received_marker, "1b5b31333b3275", Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_modify_other_keys_for_client_input() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let script = base.join("read-raw.py");
    let ready_marker = base.join("modify-ready");
    let received_marker = base.join("modify-received");

    fs::create_dir_all(&base).unwrap();
    fs::write(
        &script,
        format!(
            r#"import os
import pathlib
import select
import sys
import tty

sys.stdout.buffer.write(b"\x1b[>4;2m")
sys.stdout.flush()
pathlib.Path({ready:?}).write_text("ready")
tty.setraw(sys.stdin.fileno())
ready_fds, _, _ = select.select([sys.stdin.fileno()], [], [], 5)
data = os.read(sys.stdin.fileno(), 32) if ready_fds else b""
pathlib.Path({received:?}).write_text(data.hex())
"#,
            ready = ready_marker.display().to_string(),
            received = received_marker.display().to_string()
        ),
    )
    .unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("python3 {}", script.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&ready_marker, Duration::from_secs(5));

    let protocol = request(
        &api_socket,
        serde_json::json!({"id":"test:protocol","method":"ping","params":{}}),
    )["result"]["protocol"]
        .as_u64()
        .unwrap() as u32;
    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));

    let mut client_stream = UnixStream::connect(&client_socket).unwrap();
    let (server_protocol, error) = client_handshake(&mut client_stream, protocol, 80, 24).unwrap();
    assert_eq!(server_protocol, protocol);
    assert!(error.is_none(), "client handshake failed: {error:?}");
    send_input(&mut client_stream, b"\x1b[13;2u").unwrap();

    wait_for_file_contains(
        &received_marker,
        "1b5b32373b323b31337e",
        Duration::from_secs(5),
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_accepts_canonical_pane_id_from_child_env() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let pane_id_marker = base.join("pane-id");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:print-id",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("printf '%s' \"$HERDR_PANE_ID\" > {}", pane_id_marker.display()), "keys": ["Enter"]}
        }),
    ));
    let old_pane_id = wait_for_file_contains(&pane_id_marker, &pane_id, Duration::from_secs(5));
    assert!(
        old_pane_id == pane_id,
        "unexpected pane id from env: {old_pane_id:?}"
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:old-pane-report",
            "method": "pane.report_agent",
            "params": {
                "pane_id": old_pane_id,
                "source": "handoff-test",
                "agent": "pi",
                "state": "working"
            }
        }),
    ));
    let agents = request(
        &api_socket,
        serde_json::json!({"id":"test:agent-list","method":"agent.list","params":{}}),
    );
    let found = agents["result"]["agents"]
        .as_array()
        .unwrap()
        .iter()
        .any(|agent| {
            agent["agent"].as_str() == Some("pi")
                && agent["agent_status"].as_str() == Some("working")
        });
    assert!(
        found,
        "old pane id report did not update restored pane: {agents}"
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_keeps_unmanaged_agent_name_bound_to_saved_session() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let old_session = base.join("old-session.jsonl");
    let new_session = base.join("new-session.jsonl");
    let started_marker = base.join("agent-started");
    let fake_pi = base.join("pi");
    fs::create_dir_all(&base).unwrap();
    fs::write(
        &fake_pi,
        format!(
            "#!/bin/bash\nexport HERDR_AGENT=pi\necho started > {}\nexec -a pi /bin/sleep 30\n",
            started_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:start-agent",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": fake_pi, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&started_marker, Duration::from_secs(5));
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:session",
            "method": "pane.report_agent_session",
            "params": {
                "pane_id": pane_id,
                "source": "herdr:pi",
                "agent": "pi",
                "seq": 1,
                "agent_session_path": old_session,
                "session_start_source": "startup"
            }
        }),
    ));
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:report",
            "method": "pane.report_agent",
            "params": {
                "pane_id": pane_id,
                "source": "herdr:pi",
                "agent": "pi",
                "state": "idle",
                "seq": 2,
                "agent_session_path": old_session
            }
        }),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let response = request(
            &api_socket,
            serde_json::json!({
                "id": "test:agent:wait-for-process",
                "method": "agent.get",
                "params": {"target": pane_id}
            }),
        );
        if response.get("result").is_some() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "agent process was not detected: {response}"
        );
        thread::sleep(Duration::from_millis(25));
    }
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:rename",
            "method": "agent.rename",
            "params": {"target": pane_id, "name": "reviewer"}
        }),
    ));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:new-session",
            "method": "pane.report_agent_session",
            "params": {
                "pane_id": pane_id,
                "source": "herdr:pi",
                "agent": "pi",
                "seq": 3,
                "agent_session_path": new_session,
                "session_start_source": "new"
            }
        }),
    ));
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let old_name = request(
            &api_socket,
            serde_json::json!({
                "id": "test:agent:get-old-name",
                "method": "agent.get",
                "params": {"target": "reviewer"}
            }),
        );
        if old_name["error"]["code"] == "agent_not_found" {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "old session alias was not cleared: {old_name}"
        );
        thread::sleep(Duration::from_millis(25));
    }

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_keeps_agent_started_pane_after_launch_settles() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let started_marker = base.join("agent-started");
    let exited_marker = base.join("agent-exited");
    let shell_marker = base.join("shell-after-agent");
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_pi = bin.join("pi");
    fs::write(
        &fake_pi,
        format!(
            "#!/bin/bash\nexport HERDR_AGENT=pi\necho started > {}\nbash -c 'exec -a pi /bin/sleep 1'\necho exited > {}\n",
            started_marker.display(),
            exited_marker.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/bin:/usr/bin", bin.display());

    let spawned = spawn_server_with_config_and_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
        &[("PATH", path.as_str())],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);
    let workspace = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace-create",
            "method": "workspace.create",
            "params": { "cwd": "/tmp", "focus": false }
        }),
    );
    assert_ok(workspace.clone());
    let pane_id = workspace["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let deadline = Instant::now() + Duration::from_secs(2);
    let started = loop {
        let response = request(
            &api_socket,
            serde_json::json!({
                "id": "test:agent-start",
                "method": "agent.start",
                "params": {
                    "name": "handoff-agent",
                    "kind": "pi",
                    "pane_id": pane_id,
                    "timeout_ms": 5000
                }
            }),
        );
        if response.get("result").is_some() {
            break response;
        }
        assert_eq!(
            response["error"]["code"], "agent_pane_busy",
            "agent.start failed before the new pane shell became available: {response}"
        );
        assert!(
            Instant::now() < deadline,
            "new pane shell did not become available: {response}"
        );
        thread::sleep(Duration::from_millis(25));
    };
    assert_ok(started);
    support::wait_for_file(&started_marker, Duration::from_secs(5));
    support::wait_for_file(&exited_marker, Duration::from_secs(5));
    let settle_deadline = Instant::now() + Duration::from_secs(6);
    loop {
        let handoff = request(
            &api_socket,
            serde_json::json!({
                "id": "test:handoff",
                "method": "server.live_handoff",
                "params": {}
            }),
        );
        if handoff.get("result").is_some() {
            break;
        }
        assert_eq!(handoff["error"]["code"], "handoff_failed");
        assert!(
            Instant::now() < settle_deadline,
            "managed agent launch did not settle: {handoff}"
        );
        thread::sleep(Duration::from_millis(25));
    }

    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    thread::sleep(Duration::from_millis(300));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:shell-after-agent",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("echo alive > {}", shell_marker.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&shell_marker, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_rejects_deferred_agent_spawn() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let shell_started = base.join("shell-started");
    let slow_shell = base.join("slow-shell");
    fs::create_dir_all(&base).unwrap();
    fs::write(
        &slow_shell,
        format!(
            "#!/bin/sh\necho started > {}\nexec /bin/sleep 10\n",
            shell_started.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&slow_shell, fs::Permissions::from_mode(0o755)).unwrap();

    let config = format!(
        "onboarding = false\n[terminal]\ndefault_shell = \"{}\"\nshell_mode = \"non_login\"\n",
        slow_shell.display()
    );
    let spawned =
        spawn_server_with_config_and_env(&config_home, &runtime_dir, &api_socket, &config, &[]);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": { "cwd": "/tmp", "focus": true }
        }),
    ));
    support::wait_for_file(&shell_started, Duration::from_secs(5));

    let mut deferred_spawn = UnixStream::connect(&api_socket).unwrap();
    deferred_spawn
        .write_all(
            serde_json::json!({
                "id": "test:agent:spawn",
                "method": "agent.spawn",
                "params": {
                    "role": "reviewer",
                    "kind": "pi",
                    "cwd_mode": "tab",
                    "timeout_ms": 5000
                }
            })
            .to_string()
            .as_bytes(),
        )
        .unwrap();
    deferred_spawn.write_all(b"\n").unwrap();
    deferred_spawn.flush().unwrap();

    let registry_path = config_home.join("herdr-dev/agents.json");
    let registry = wait_for_file_contains(&registry_path, "\"reviewer\"", Duration::from_secs(5));
    let registry: serde_json::Value = serde_json::from_str(&registry).unwrap();
    assert!(registry["roster"]
        .as_object()
        .unwrap()
        .values()
        .any(|entry| { entry["display_name"] == "reviewer" && entry["status"] == "active" }));

    let handoff = request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    );
    assert_eq!(handoff["error"]["code"], "handoff_failed");
    assert!(handoff["error"]["message"]
        .as_str()
        .unwrap()
        .contains("waiting for a shell"));
    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:ping","method":"ping","params":{}}),
    ));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(deferred_spawn);
    cleanup_test_base(&base);
    drop(spawned);
}

#[test]
fn live_handoff_rejects_unsettled_agent_spawn() {
    use std::os::unix::fs::PermissionsExt;

    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let shell_started = base.join("shell-started");
    let default_shell = base.join("default-shell");
    let bin = base.join("bin");
    let fake_pi = bin.join("pi");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        &default_shell,
        format!(
            "#!/bin/sh\necho started > {}\nexec /bin/sh\n",
            shell_started.display()
        ),
    )
    .unwrap();
    fs::set_permissions(&default_shell, fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(
        &fake_pi,
        "#!/bin/sh\nexport HERDR_AGENT=pi\nexec /bin/sleep 10\n",
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();
    let config = format!(
        "onboarding = false\n[terminal]\ndefault_shell = \"{}\"\nshell_mode = \"non_login\"\n",
        default_shell.display()
    );
    let path = format!("{}:/bin:/usr/bin", bin.display());
    let spawned = spawn_server_with_config_and_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &config,
        &[("PATH", path.as_str())],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": { "cwd": "/tmp", "focus": true }
        }),
    ));
    support::wait_for_file(&shell_started, Duration::from_secs(5));

    let agent = request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:spawn",
            "method": "agent.spawn",
            "params": {
                "role": "reviewer",
                "kind": "pi",
                "cwd_mode": "tab",
                "timeout_ms": 5000
            }
        }),
    );
    assert_ok(agent);

    let handoff = request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    );
    assert_eq!(handoff["error"]["code"], "handoff_failed");
    assert!(handoff["error"]["message"]
        .as_str()
        .unwrap()
        .contains("launch is settling"));
    let agent = request(
        &api_socket,
        serde_json::json!({
            "id": "test:agent:get",
            "method": "agent.get",
            "params": { "target": "reviewer" }
        }),
    );
    assert_ok(agent.clone());
    assert_eq!(agent["result"]["agent"]["name"], "reviewer");

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
    drop(spawned);
}

#[test]
fn live_handoff_keeps_shell_pane_after_foreground_process_exits() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let started_marker = base.join("foreground-started");
    let exited_marker = base.join("foreground-exited");
    let shell_marker = base.join("shell-after-foreground");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "sh -c 'echo started > {}; sleep 1; echo exited > {}'",
        started_marker.display(),
        exited_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run-foreground",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&started_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    support::wait_for_file(&exited_marker, Duration::from_secs(5));

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:shell-after-foreground",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": format!("echo alive > {}", shell_marker.display()), "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&shell_marker, Duration::from_secs(5));

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_python_http_server() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let web_root = base.join("web");
    fs::create_dir_all(&web_root).unwrap();
    fs::write(
        web_root.join("index.html"),
        "hello-from-python-before-and-after",
    )
    .unwrap();
    let port = unused_local_port();

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": web_root, "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run-python",
            "method": "pane.send_input",
            "params": {
                "pane_id": pane_id,
                "text": format!("python3 -m http.server {port} --bind 127.0.0.1"),
                "keys": ["Enter"]
            }
        }),
    ));
    wait_for_http_contains(
        port,
        "hello-from-python-before-and-after",
        Duration::from_secs(10),
    );

    assert_ok(request(
        &api_socket,
        serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
    ));
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_http_contains(
        port,
        "hello-from-python-before-and-after",
        Duration::from_secs(10),
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    let _ = client_socket;
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_preserves_http_servers_across_multiple_sessions() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let sessions = [
        (None, config_home.join("herdr-dev/herdr.sock")),
        (
            Some("work"),
            config_home.join("herdr-dev/sessions/work/herdr.sock"),
        ),
    ];
    let mut spawned = Vec::new();
    let mut ports = Vec::new();

    for (session_name, api_socket) in &sessions {
        let web_root = base.join(format!("web-{}", session_name.unwrap_or("default")));
        fs::create_dir_all(&web_root).unwrap();
        fs::write(
            web_root.join("index.html"),
            format!("hello-from-{}", session_name.unwrap_or("default")),
        )
        .unwrap();
        let port = unused_local_port();
        let server = if let Some(session_name) = session_name {
            spawn_named_session_server(&config_home, &runtime_dir, session_name)
        } else {
            spawn_default_session_server(&config_home, &runtime_dir)
        };
        wait_for_socket(api_socket, Duration::from_secs(10));
        let created = request(
            api_socket,
            serde_json::json!({
                "id": "test:workspace:create",
                "method": "workspace.create",
                "params": {"cwd": web_root, "focus": true}
            }),
        );
        let pane_id = created["result"]["root_pane"]["pane_id"]
            .as_str()
            .unwrap()
            .to_string();
        assert_ok(request(
            api_socket,
            serde_json::json!({
                "id": "test:pane:run-python",
                "method": "pane.send_input",
                "params": {
                    "pane_id": pane_id,
                    "text": format!("python3 -m http.server {port} --bind 127.0.0.1"),
                    "keys": ["Enter"]
                }
            }),
        ));
        wait_for_http_contains(
            port,
            &format!("hello-from-{}", session_name.unwrap_or("default")),
            Duration::from_secs(10),
        );
        spawned.push(server);
        ports.push((port, session_name.unwrap_or("default").to_string()));
    }
    register_runtime_dir(&runtime_dir);

    for (_session_name, api_socket) in &sessions {
        assert_ok(request(
            api_socket,
            serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
        ));
    }
    drop(spawned);

    for (_session_name, api_socket) in &sessions {
        wait_for_api(api_socket, Duration::from_secs(10));
    }
    for (port, label) in ports {
        wait_for_http_contains(
            port,
            &format!("hello-from-{label}"),
            Duration::from_secs(10),
        );
    }

    for (_session_name, api_socket) in &sessions {
        let _ = request(
            api_socket,
            serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
        );
    }
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_bad_expected_protocol_rolls_back_old_server() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let marker = base.join("child.pid");
    let received_marker = base.join("received");

    let spawned = spawn_server(&config_home, &runtime_dir, &api_socket);
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "sh -c 'echo READY $$ > {}; while read line; do echo got:$line; echo got:$line >> {}; done'",
        marker.display(),
        received_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&marker, Duration::from_secs(5));
    let pid_text = fs::read_to_string(&marker).unwrap();
    let child_pid: u32 = pid_text.split_whitespace().last().unwrap().parse().unwrap();

    let failed = request(
        &api_socket,
        serde_json::json!({
            "id": "test:bad-handoff",
            "method": "server.live_handoff",
            "params": {"expected_protocol": 999999}
        }),
    );
    assert!(
        failed.get("error").is_some(),
        "bad protocol handoff should fail: {failed}"
    );
    wait_for_api(&api_socket, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:send-after-failed-handoff",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": "after-failed-handoff", "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &received_marker,
        "got:after-failed-handoff",
        Duration::from_secs(5),
    );
    wait_for_output(&api_socket, &pane_id, "got:after-failed-handoff");

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

fn live_handoff_import_failure_rolls_back_old_server_at(failure_point: &str) {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let client_socket = runtime_dir.join("herdr-client.sock");
    let marker = base.join("child.pid");
    let received_marker = base.join("received");

    let spawned = spawn_server_with_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &[("HERDR_TEST_HANDOFF_IMPORT_FAIL", failure_point)],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "sh -c 'echo READY $$ > {}; while read line; do echo got:$line; echo got:$line >> {}; done'",
        marker.display(),
        received_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:run",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&marker, Duration::from_secs(5));
    let pid_text = fs::read_to_string(&marker).unwrap();
    let child_pid: u32 = pid_text.split_whitespace().last().unwrap().parse().unwrap();

    let failed = request(
        &api_socket,
        serde_json::json!({"id":"test:handoff-fail","method":"server.live_handoff","params":{}}),
    );
    assert!(
        failed.get("error").is_some(),
        "{failure_point} handoff should fail: {failed}"
    );
    wait_for_api(&api_socket, Duration::from_secs(10));
    wait_for_socket(&client_socket, Duration::from_secs(5));
    assert_eq!(unsafe { libc::kill(child_pid as libc::pid_t, 0) }, 0);

    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:send-after-import-failure",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": failure_point, "keys": ["Enter"]}
        }),
    ));
    wait_for_file_contains(
        &received_marker,
        &format!("got:{failure_point}"),
        Duration::from_secs(5),
    );

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    drop(spawned);
    cleanup_test_base(&base);
}

#[test]
fn live_handoff_after_restored_failure_rolls_back_old_server() {
    live_handoff_import_failure_rolls_back_old_server_at("after_restored");
}

#[cfg(debug_assertions)]
#[test]
fn live_handoff_shutdown_kills_survivor_after_leader_exits_during_import() {
    let _lock = test_lock();
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let api_socket = runtime_dir.join("herdr.sock");
    let pause_dir = base.join("handoff-pause");
    let processes_marker = base.join("processes");

    fs::create_dir_all(&pause_dir).unwrap();
    let spawned = spawn_server_with_env(
        &config_home,
        &runtime_dir,
        &api_socket,
        &[(
            "HERDR_TEST_HANDOFF_IMPORT_PAUSE_DIR",
            pause_dir.to_str().unwrap(),
        )],
    );
    wait_for_socket(&api_socket, Duration::from_secs(10));
    register_runtime_dir(&runtime_dir);

    let created = request(
        &api_socket,
        serde_json::json!({
            "id": "test:workspace:create",
            "method": "workspace.create",
            "params": {"cwd": "/tmp", "focus": true}
        }),
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let command = format!(
        "exec sh -c 'sh -c '\"'\"'trap \"\" HUP TERM; while :; do sleep 1; done'\"'\"' & survivor=$!; echo \"$$ $survivor\" > {}; wait'",
        processes_marker.display()
    );
    assert_ok(request(
        &api_socket,
        serde_json::json!({
            "id": "test:pane:start-survivor",
            "method": "pane.send_input",
            "params": {"pane_id": pane_id, "text": command, "keys": ["Enter"]}
        }),
    ));
    support::wait_for_file(&processes_marker, Duration::from_secs(5));
    let process_ids = fs::read_to_string(&processes_marker).unwrap();
    let process_ids = process_ids
        .split_whitespace()
        .map(|value| value.parse::<u32>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(
        process_ids.len(),
        2,
        "unexpected process marker: {process_ids:?}"
    );
    let leader_pid = process_ids[0];
    let survivor_pid = process_ids[1];
    assert_eq!(unsafe { libc::kill(leader_pid as libc::pid_t, 0) }, 0);
    assert_eq!(unsafe { libc::kill(survivor_pid as libc::pid_t, 0) }, 0);
    assert_eq!(
        unsafe { libc::getsid(leader_pid as libc::pid_t) },
        unsafe { libc::getsid(survivor_pid as libc::pid_t) },
        "leader and survivor must share a session"
    );

    let handoff_socket = api_socket.clone();
    let handoff = thread::spawn(move || {
        request(
            &handoff_socket,
            serde_json::json!({"id":"test:handoff","method":"server.live_handoff","params":{}}),
        )
    });
    support::wait_for_file(&pause_dir.join("ready"), Duration::from_secs(5));
    assert_eq!(
        unsafe { libc::kill(leader_pid as libc::pid_t, libc::SIGTERM) },
        0
    );
    let leader_exit_deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(leader_pid as libc::pid_t, 0) } == 0 {
        assert!(
            Instant::now() < leader_exit_deadline,
            "pane leader {leader_pid} did not exit during handoff import"
        );
        thread::sleep(Duration::from_millis(25));
    }
    fs::write(pause_dir.join("release"), b"release").unwrap();
    assert_ok(handoff.join().unwrap());
    drop(spawned);
    wait_for_api(&api_socket, Duration::from_secs(10));
    assert_eq!(unsafe { libc::kill(survivor_pid as libc::pid_t, 0) }, 0);

    let _ = request(
        &api_socket,
        serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
    );
    let survivor_exit_deadline = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(survivor_pid as libc::pid_t, 0) } == 0 {
        assert!(
            Instant::now() < survivor_exit_deadline,
            "same-session survivor {survivor_pid} leaked after handoff shutdown"
        );
        thread::sleep(Duration::from_millis(25));
    }
    cleanup_test_base(&base);
}
