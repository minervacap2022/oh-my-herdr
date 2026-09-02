#![cfg(windows)]

use std::fs;
use std::io::{self, Read, Write};
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Barrier};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use interprocess::local_socket::{prelude::*, GenericNamespaced, Stream};

const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const REQUEST_TIMEOUT: Duration = Duration::from_secs(6);
const REQUEST_POLL_INTERVAL: Duration = Duration::from_millis(5);
const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
const SERVER_READY_TIMEOUT: Duration = Duration::from_secs(15);
const SERVER_STOP_TIMEOUT: Duration = Duration::from_secs(5);
const SERVER_STOP_REQUEST_TIMEOUT: Duration = Duration::from_millis(250);
const TEST_BASE_CLEANUP_TIMEOUT: Duration = Duration::from_secs(5);

struct SpawnedServer {
    child: Child,
    socket_path: PathBuf,
}

impl Drop for SpawnedServer {
    fn drop(&mut self) {
        let socket_path = self.socket_path.clone();
        let (sent, received) = mpsc::channel();
        let _ = thread::Builder::new()
            .name("herdr-test-server-stop".into())
            .spawn(move || {
                let _ = sent.send(request(
                    &socket_path,
                    serde_json::json!({"id":"test:stop","method":"server.stop","params":{}}),
                ));
            });
        let _ = received.recv_timeout(SERVER_STOP_REQUEST_TIMEOUT);

        let deadline = Instant::now() + SERVER_STOP_TIMEOUT;
        while Instant::now() < deadline {
            match self.child.try_wait() {
                Ok(Some(_)) => return,
                Ok(None) => thread::sleep(Duration::from_millis(25)),
                Err(_) => break,
            }
        }

        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

struct TestBaseCleanup(PathBuf);

impl Drop for TestBaseCleanup {
    fn drop(&mut self) {
        let deadline = Instant::now() + TEST_BASE_CLEANUP_TIMEOUT;
        loop {
            match fs::remove_dir_all(&self.0) {
                Ok(()) => return,
                Err(error) if error.kind() == io::ErrorKind::NotFound => return,
                Err(error) if Instant::now() >= deadline => {
                    eprintln!(
                        "failed to clean up Windows agent registry test base {}: {error}",
                        self.0.display()
                    );
                    return;
                }
                Err(_) => thread::sleep(Duration::from_millis(25)),
            }
        }
    }
}

fn unique_test_dir() -> PathBuf {
    static COUNTER: AtomicUsize = AtomicUsize::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "herdr-windows-agent-registry-{}-{nanos}-{counter}",
        std::process::id()
    ))
}

fn connect(socket_path: &Path) -> io::Result<Stream> {
    let name = socket_path.to_string_lossy().to_string();
    let name = name.to_ns_name::<GenericNamespaced>()?;
    Stream::connect(name)
}

fn request(socket_path: &Path, value: serde_json::Value) -> io::Result<serde_json::Value> {
    let mut stream = connect(socket_path)?;
    stream.set_nonblocking(true)?;
    let deadline = Instant::now() + REQUEST_TIMEOUT;
    let mut request_bytes = serde_json::to_vec(&value).map_err(io::Error::other)?;
    request_bytes.push(b'\n');
    let mut written = 0;
    while written < request_bytes.len() {
        ensure_request_before_deadline(deadline)?;
        match stream.write(&request_bytes[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "Herdr API named pipe accepted no request bytes",
                ));
            }
            Ok(count) => {
                written += count;
                ensure_request_before_deadline(deadline)?;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                wait_for_request_progress(deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
    stream.flush()?;

    let mut line = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        ensure_request_before_deadline(deadline)?;
        match stream.read(&mut chunk) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "Herdr API closed the named pipe before responding",
                ));
            }
            Ok(read) => {
                let chunk = &chunk[..read];
                if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
                    append_response_bytes(&mut line, &chunk[..newline])?;
                    ensure_request_before_deadline(deadline)?;
                    return serde_json::from_slice(&line).map_err(io::Error::other);
                }
                append_response_bytes(&mut line, chunk)?;
                ensure_request_before_deadline(deadline)?;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                wait_for_request_progress(deadline)?;
            }
            Err(error) => return Err(error),
        }
    }
}

fn append_response_bytes(line: &mut Vec<u8>, bytes: &[u8]) -> io::Result<()> {
    if line.len().saturating_add(bytes.len()) > MAX_RESPONSE_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Herdr API response line is too large",
        ));
    }
    line.extend_from_slice(bytes);
    Ok(())
}

fn ensure_request_before_deadline(deadline: Instant) -> io::Result<Duration> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "Herdr API request timed out"))
}

fn wait_for_request_progress(deadline: Instant) -> io::Result<()> {
    let remaining = ensure_request_before_deadline(deadline)?;
    thread::sleep(remaining.min(REQUEST_POLL_INTERVAL));
    Ok(())
}

fn wait_for_api(socket_path: &Path) {
    let deadline = Instant::now() + SERVER_READY_TIMEOUT;
    let mut last_error = String::new();
    while Instant::now() < deadline {
        if socket_path.exists() {
            match request(
                socket_path,
                serde_json::json!({"id":"test:ping","method":"ping","params":{}}),
            ) {
                Ok(response) if response.get("result").is_some() => return,
                Ok(response) => last_error = format!("unexpected response: {response}"),
                Err(error) => last_error = error.to_string(),
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "server did not become ready at {}; last error: {last_error}",
        socket_path.display()
    );
}

fn wait_for_file_contains(path: &Path, needle: &str) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut last_contents = String::new();
    while Instant::now() < deadline {
        if let Ok(contents) = fs::read_to_string(path) {
            last_contents = contents;
            if last_contents.contains(needle) {
                return last_contents;
            }
        }
        thread::sleep(Duration::from_millis(25));
    }
    panic!(
        "{} did not contain {needle:?}; last contents: {last_contents:?}",
        path.display()
    );
}

fn assert_ok(response: serde_json::Value) {
    assert!(
        response.get("result").is_some(),
        "expected successful API response, got {response}"
    );
}

fn concurrent_requests(
    first_socket: &Path,
    first_request: serde_json::Value,
    second_socket: &Path,
    second_request: serde_json::Value,
) -> (serde_json::Value, serde_json::Value) {
    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = Arc::clone(&barrier);
    let first_socket = first_socket.to_path_buf();
    let first = thread::spawn(move || {
        first_barrier.wait();
        request(&first_socket, first_request).unwrap()
    });
    let second_barrier = Arc::clone(&barrier);
    let second_socket = second_socket.to_path_buf();
    let second = thread::spawn(move || {
        second_barrier.wait();
        request(&second_socket, second_request).unwrap()
    });

    barrier.wait();
    (first.join().unwrap(), second.join().unwrap())
}

fn request_until_agent_spawned(socket_path: &Path, value: serde_json::Value) -> serde_json::Value {
    let deadline = Instant::now() + SERVER_READY_TIMEOUT;
    loop {
        let response = request(socket_path, value.clone()).unwrap();
        if response.get("result").is_some() {
            return response;
        }
        assert_eq!(
            response
                .pointer("/error/code")
                .and_then(serde_json::Value::as_str),
            Some("agent_pane_busy"),
            "agent.spawn failed unexpectedly: {response}"
        );
        assert!(
            Instant::now() < deadline,
            "agent pane did not become available: {response}"
        );
        thread::sleep(Duration::from_millis(25));
    }
}

fn concurrent_agent_spawn_requests(
    first_socket: &Path,
    first_request: serde_json::Value,
    second_socket: &Path,
    second_request: serde_json::Value,
) -> (serde_json::Value, serde_json::Value) {
    let barrier = Arc::new(Barrier::new(3));
    let first_barrier = Arc::clone(&barrier);
    let first_socket = first_socket.to_path_buf();
    let first = thread::spawn(move || {
        first_barrier.wait();
        request_until_agent_spawned(&first_socket, first_request)
    });
    let second_barrier = Arc::clone(&barrier);
    let second_socket = second_socket.to_path_buf();
    let second = thread::spawn(move || {
        second_barrier.wait();
        request_until_agent_spawned(&second_socket, second_request)
    });

    barrier.wait();
    (first.join().unwrap(), second.join().unwrap())
}

fn write_harness_fixture(bin_dir: &Path, name: &str) {
    let source = bin_dir.join(format!("{name}.rs"));
    let executable = bin_dir.join(format!("{name}.exe"));
    fs::write(
        &source,
        format!(
            "fn main() {{\n\
                 let mut args = std::env::args().skip(1);\n\
                 let mut marker = None;\n\
                 let mut cwd_marker = None;\n\
                 while let Some(argument) = args.next() {{\n\
                     match argument.as_str() {{\n\
                         \"--marker\" => marker = Some(args.next().expect(\"missing marker path\")),\n\
                         \"--cwd-marker\" => cwd_marker = Some(args.next().expect(\"missing cwd marker path\")),\n\
                         _ => panic!(\"unexpected fixture argument: {{argument}}\"),\n\
                     }}\n\
                 }}\n\
                 if let Some(marker) = marker {{\n\
                     std::fs::write(marker, \"{name}\").expect(\"write harness marker\");\n\
                 }}\n\
                 if let Some(cwd_marker) = cwd_marker {{\n\
                     let cwd = std::env::current_dir().expect(\"fixture working directory\");\n\
                     std::fs::write(cwd_marker, cwd.display().to_string()).expect(\"write cwd marker\");\n\
                 }}\n\
             }}\n"
        ),
    )
    .unwrap();
    let output = Command::new("rustc")
        .args(["--edition", "2021"])
        .arg(&source)
        .arg("-o")
        .arg(&executable)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "failed to compile {name}.exe fixture: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn spawn_server(
    config_home: &Path,
    runtime_dir: &Path,
    socket_path: &Path,
    session_name: &str,
    fake_bin: &Path,
) -> SpawnedServer {
    fs::create_dir_all(config_home).unwrap();
    fs::create_dir_all(runtime_dir).unwrap();
    let config_path = config_home.join("herdr-test.toml");
    fs::write(
        &config_path,
        "onboarding = false\n[terminal]\nshell_mode = \"non_login\"\n",
    )
    .unwrap();

    let inherited_path = std::env::var_os("PATH").unwrap_or_default();
    let path = format!(
        "{};{}",
        fake_bin.display(),
        inherited_path.to_string_lossy()
    );

    let mut command = Command::new(env!("CARGO_BIN_EXE_herdr"));
    command
        .arg("server")
        .env("XDG_CONFIG_HOME", config_home)
        .env("XDG_RUNTIME_DIR", runtime_dir)
        .env("XDG_STATE_HOME", runtime_dir.join("state"))
        .env("HERDR_CONFIG_PATH", config_path)
        .env("HERDR_SOCKET_PATH", socket_path)
        .env("HERDR_SESSION", session_name)
        .env("PATH", path)
        .env_remove("HERDR_CLIENT_SOCKET_PATH")
        .env_remove("HERDR_ENV")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .creation_flags(CREATE_NO_WINDOW);

    SpawnedServer {
        child: command.spawn().unwrap(),
        socket_path: socket_path.to_path_buf(),
    }
}

fn registry_path(config_home: &Path, session_name: &str) -> PathBuf {
    config_home
        .join("herdr-dev")
        .join("sessions")
        .join(session_name)
        .join("agents.json")
}

#[test]
fn windows_shared_session_registry_preserves_concurrent_profiles_spawns_and_latest_profile() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let first_runtime_dir = base.join("runtime-first");
    let second_runtime_dir = base.join("runtime-second");
    let first_socket = first_runtime_dir.join("first.sock");
    let second_socket = second_runtime_dir.join("second.sock");
    let session_name = "windows-shared-agent-registry";
    let fake_bin = base.join("bin");
    let _cleanup = TestBaseCleanup(base.clone());
    let first_spawn_marker = base.join("first-spawn-kind");
    let second_spawn_marker = base.join("second-spawn-kind");
    let first_spawn_cwd_marker = base.join("first-spawn-cwd");
    let second_spawn_cwd_marker = base.join("second-spawn-cwd");
    let latest_spawn_marker = base.join("latest-spawn-kind");
    let latest_spawn_cwd_marker = base.join("latest-spawn-cwd");
    fs::create_dir_all(&fake_bin).unwrap();
    let expected_cwd = fs::canonicalize(&base).unwrap();
    write_harness_fixture(&fake_bin, "claude");
    write_harness_fixture(&fake_bin, "codex");

    let _first = spawn_server(
        &config_home,
        &first_runtime_dir,
        &first_socket,
        session_name,
        &fake_bin,
    );
    let _second = spawn_server(
        &config_home,
        &second_runtime_dir,
        &second_socket,
        session_name,
        &fake_bin,
    );
    wait_for_api(&first_socket);
    wait_for_api(&second_socket);

    let (first_profile, second_profile) = concurrent_requests(
        &first_socket,
        serde_json::json!({
            "id": "test:first-profile",
            "method": "agent.profile.set",
            "params": {"role": "reviewer", "harness": "claude"}
        }),
        &second_socket,
        serde_json::json!({
            "id": "test:second-profile",
            "method": "agent.profile.set",
            "params": {"role": "architect", "harness": "codex"}
        }),
    );
    assert_ok(first_profile);
    assert_ok(second_profile);
    let architect_from_first = request(
        &first_socket,
        serde_json::json!({
            "id": "test:architect-from-first",
            "method": "agent.profile.get",
            "params": {"role": "architect"}
        }),
    )
    .unwrap();
    assert_ok(architect_from_first.clone());
    assert_eq!(
        architect_from_first["result"]["profile"]["harness"],
        "codex"
    );
    let profiles_from_second = request(
        &second_socket,
        serde_json::json!({
            "id": "test:profiles-from-second",
            "method": "agent.profile.list",
            "params": {}
        }),
    )
    .unwrap();
    assert_ok(profiles_from_second.clone());
    let profile_roles = profiles_from_second["result"]["profiles"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|profile| profile["role"].as_str())
        .collect::<Vec<_>>();
    assert!(profile_roles.contains(&"reviewer"));
    assert!(profile_roles.contains(&"architect"));

    let path = registry_path(&config_home, session_name);
    let registry = wait_for_file_contains(&path, "\"architect\"");
    let registry: serde_json::Value = serde_json::from_str(&registry).unwrap();
    assert!(registry["profiles"].get("reviewer").is_some());
    assert!(registry["profiles"].get("architect").is_some());

    for (socket, id) in [
        (&first_socket, "test:first-workspace"),
        (&second_socket, "test:second-workspace"),
    ] {
        assert_ok(
            request(
                socket,
                serde_json::json!({
                    "id": id,
                    "method": "workspace.create",
                    "params": {"cwd": base, "focus": true}
                }),
            )
            .unwrap(),
        );
    }

    let (first_spawn, second_spawn) = concurrent_agent_spawn_requests(
        &first_socket,
        serde_json::json!({
            "id": "test:first-spawn",
            "method": "agent.spawn",
            "params": {
                "role": "reviewer",
                "kind": "claude",
                "cwd_mode": "tab",
                "args": [
                    "--marker", first_spawn_marker,
                    "--cwd-marker", first_spawn_cwd_marker
                ],
                "timeout_ms": 5000
            }
        }),
        &second_socket,
        serde_json::json!({
            "id": "test:second-spawn",
            "method": "agent.spawn",
            "params": {
                "role": "reviewer",
                "kind": "claude",
                "cwd_mode": "tab",
                "args": [
                    "--marker", second_spawn_marker,
                    "--cwd-marker", second_spawn_cwd_marker
                ],
                "timeout_ms": 5000
            }
        }),
    );
    assert_ok(first_spawn.clone());
    assert_ok(second_spawn.clone());
    let mut spawned_names = [
        first_spawn["result"]["spawned"]["name"]
            .as_str()
            .unwrap()
            .to_string(),
        second_spawn["result"]["spawned"]["name"]
            .as_str()
            .unwrap()
            .to_string(),
    ];
    spawned_names.sort();
    assert_eq!(spawned_names, ["reviewer", "reviewer-replica-1"]);
    let first_pane_id = first_spawn["result"]["spawned"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let second_pane_id = second_spawn["result"]["spawned"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(first_pane_id, second_pane_id);
    let (replica_socket, replica_pane_id) =
        if first_spawn["result"]["spawned"]["name"] == "reviewer-replica-1" {
            (&first_socket, first_pane_id)
        } else {
            (&second_socket, second_pane_id)
        };

    let registry = wait_for_file_contains(&path, "reviewer-replica-1");
    let registry: serde_json::Value = serde_json::from_str(&registry).unwrap();
    assert_eq!(registry["profiles"]["reviewer"]["replicas_assigned"], 1);
    let roster_names = registry["roster"]
        .as_object()
        .unwrap()
        .values()
        .filter_map(|entry| entry["display_name"].as_str())
        .collect::<Vec<_>>();
    assert!(roster_names.contains(&"reviewer"));
    assert!(roster_names.contains(&"reviewer-replica-1"));

    assert_ok(
        request(
            replica_socket,
            serde_json::json!({
                "id": "test:replica-working",
                "method": "pane.report_agent",
                "params": {
                    "pane_id": replica_pane_id,
                    "source": "shared-registry-test",
                    "agent": "claude",
                    "state": "working",
                    "seq": 1
                }
            }),
        )
        .unwrap(),
    );
    let registry = wait_for_file_contains(&path, "\"working\"");
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
    for marker in [&first_spawn_marker, &second_spawn_marker] {
        assert_eq!(wait_for_file_contains(marker, "claude").trim(), "claude");
    }
    for marker in [&first_spawn_cwd_marker, &second_spawn_cwd_marker] {
        assert_eq!(
            wait_for_file_contains(marker, &expected_cwd.display().to_string()).trim(),
            expected_cwd.display().to_string()
        );
    }

    assert_ok(
        request(
            &first_socket,
            serde_json::json!({
                "id": "test:stale-profile",
                "method": "agent.profile.set",
                "params": {"role": "snapshot", "harness": "codex"}
            }),
        )
        .unwrap(),
    );
    assert_ok(
        request(
            &second_socket,
            serde_json::json!({
                "id": "test:latest-profile",
                "method": "agent.profile.set",
                "params": {"role": "snapshot", "harness": "claude"}
            }),
        )
        .unwrap(),
    );
    assert_ok(
        request(
            &first_socket,
            serde_json::json!({
                "id": "test:latest-spawn",
                "method": "agent.spawn",
                "params": {
                    "role": "snapshot",
                    "cwd_mode": "tab",
                    "args": [
                        "--marker", latest_spawn_marker,
                        "--cwd-marker", latest_spawn_cwd_marker
                    ],
                    "timeout_ms": 5000
                }
            }),
        )
        .unwrap(),
    );
    assert_eq!(
        wait_for_file_contains(&latest_spawn_marker, "claude").trim(),
        "claude"
    );
    assert_eq!(
        wait_for_file_contains(
            &latest_spawn_cwd_marker,
            &expected_cwd.display().to_string()
        )
        .trim(),
        expected_cwd.display().to_string()
    );
}
