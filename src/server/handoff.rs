#[cfg(unix)]
use std::ffi::CStr;
#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io::{self, Read, Write};
#[cfg(unix)]
use std::os::fd::{AsRawFd, RawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
#[cfg(unix)]
use std::path::{Path, PathBuf};
#[cfg(unix)]
use std::process::{Child, Command};
#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use serde::{Deserialize, Serialize};
#[cfg(unix)]
use tracing::{info, warn};

#[cfg(unix)]
const HANDOFF_VERSION: u32 = 1;
#[cfg(unix)]
const READY_TIMEOUT: Duration = Duration::from_secs(30);
#[cfg(unix)]
const OWNED_ACK_TIMEOUT: Duration = Duration::from_millis(500);
#[cfg(unix)]
pub(crate) const MAX_FDS_PER_HANDOFF: usize = 64;
#[cfg(unix)]
pub(crate) const MAX_REPLAY_BYTES_PER_PANE: usize = 8 * 1024;
#[cfg(unix)]
pub(crate) const COMMIT_TIMEOUT: Duration = READY_TIMEOUT;

#[cfg(unix)]
#[derive(Serialize, Deserialize)]
pub(crate) struct HandoffManifest {
    pub version: u32,
    pub source_version: String,
    pub source_protocol: u32,
    pub expected_version: Option<String>,
    pub expected_protocol: Option<u32>,
    pub snapshot: crate::persist::SessionSnapshot,
    pub panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    /// An outer window title set over the API outlives the server that took the
    /// call, so a handoff carries it rather than falling back to the config.
    /// Absent from manifests written before this field existed.
    #[serde(default)]
    pub api_window_title: Option<String>,
}

#[cfg(unix)]
pub(crate) struct ReceivedHandoff {
    pub manifest: HandoffManifest,
    pub fds: Vec<RawFd>,
    pub stream: UnixStream,
}

#[cfg(unix)]
pub(crate) struct HandoffSocket {
    path: PathBuf,
    directory: PathBuf,
}

#[cfg(unix)]
impl HandoffSocket {
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn bind_listener(&mut self) -> io::Result<UnixListener> {
        let listener = UnixListener::bind(&self.path)?;
        let setup = (|| -> io::Result<()> {
            listener.set_nonblocking(true)?;
            restrict_socket_permissions(&self.path)?;
            Ok(())
        })();
        match setup {
            Ok(()) => Ok(listener),
            Err(error) => {
                drop(listener);
                Err(error)
            }
        }
    }

    fn cleanup(&mut self) {
        let _ = fs::remove_file(&self.path);
        let _ = fs::remove_dir(&self.directory);
    }
}

#[cfg(unix)]
impl Drop for HandoffSocket {
    fn drop(&mut self) {
        self.cleanup();
    }
}

#[cfg(unix)]
pub(crate) fn handoff_socket() -> io::Result<HandoffSocket> {
    let mut template = b"/tmp/herdr-handoff-XXXXXX\0".to_vec();
    let directory = unsafe { libc::mkdtemp(template.as_mut_ptr().cast()) };
    if directory.is_null() {
        return Err(io::Error::last_os_error());
    }

    let directory = PathBuf::from(
        unsafe { CStr::from_ptr(directory) }
            .to_string_lossy()
            .into_owned(),
    );
    Ok(HandoffSocket {
        path: directory.join("h.sock"),
        directory,
    })
}

#[cfg(unix)]
pub(crate) fn spawn_handoff_import(
    import_exe: Option<&Path>,
    socket_path: &Path,
    token: &str,
) -> io::Result<Child> {
    let fallback_exe;
    let exe = if let Some(import_exe) = import_exe {
        import_exe
    } else {
        fallback_exe = std::env::current_exe().map_err(|err| {
            io::Error::new(
                err.kind(),
                format!("failed to determine herdr executable path: {err}"),
            )
        })?;
        &fallback_exe
    };
    let mut command = Command::new(exe);
    command
        .arg("server")
        .arg("--handoff-import")
        .arg(socket_path)
        .arg(token)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    if crate::session::explicit_session_requested() {
        // The import child no longer has the original `--session` argument, so
        // stale socket overrides must not mask the inherited HERDR_SESSION.
        command
            .env_remove(crate::api::SOCKET_PATH_ENV_VAR)
            .env_remove(crate::server::socket_paths::CLIENT_SOCKET_PATH_ENV_VAR);
    }
    crate::platform::detach_server_daemon_command(&mut command);
    command.spawn().map_err(|err| {
        io::Error::new(
            err.kind(),
            format!(
                "failed to spawn handoff import server at {}: {err}",
                exe.display()
            ),
        )
    })
}

#[cfg(unix)]
pub(crate) fn cleanup_failed_import_child(child: &mut Child) {
    let pid = child.id();
    match child.try_wait() {
        Ok(Some(status)) => {
            info!(pid, status = %status, "handoff import server exited during rollback");
            return;
        }
        Ok(None) => {}
        Err(err) => {
            warn!(pid, err = %err, "failed to inspect handoff import server before rollback");
        }
    }

    if let Err(err) = child.kill() {
        warn!(pid, err = %err, "failed to kill handoff import server during rollback");
    }
    match child.wait() {
        Ok(status) => {
            info!(pid, status = %status, "handoff import server reaped during rollback");
        }
        Err(err) => {
            warn!(pid, err = %err, "failed to reap handoff import server during rollback");
        }
    }
}

#[cfg(unix)]
pub(crate) fn accept_and_validate_on(
    listener: UnixListener,
    token: &str,
    manifest: &HandoffManifest,
) -> io::Result<UnixStream> {
    let (mut stream, _) = accept_with_timeout(&listener, READY_TIMEOUT)?;
    stream.set_nonblocking(false)?;
    stream.set_write_timeout(Some(READY_TIMEOUT))?;
    let token_line = read_line_unbuffered(&mut stream, Instant::now() + READY_TIMEOUT)?;
    if token_line.trim_end() != token {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "handoff import token mismatch",
        ));
    }

    serde_json::to_writer(&mut stream, manifest).map_err(io::Error::other)?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let validated = read_line_unbuffered(&mut stream, Instant::now() + READY_TIMEOUT)?;
    if validated.trim_end() != "validated" {
        return Err(io::Error::other("handoff import did not validate manifest"));
    }
    Ok(stream)
}

#[cfg(unix)]
pub(crate) fn send_fds_and_wait_restored(stream: &mut UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.len() > MAX_FDS_PER_HANDOFF {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("handoff supports at most {MAX_FDS_PER_HANDOFF} pane file descriptors at once"),
        ));
    }
    send_fds(stream, fds)?;

    let restored = read_line_unbuffered(&mut *stream, Instant::now() + READY_TIMEOUT)?;
    if restored.trim_end() != "restored" {
        return Err(io::Error::other(
            "handoff import did not report restored runtimes",
        ));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn wait_ready(stream: &mut UnixStream) -> io::Result<()> {
    let ready = read_line_unbuffered(&mut *stream, Instant::now() + READY_TIMEOUT)?;
    if ready.trim_end() != "ready" {
        return Err(io::Error::other("handoff import did not report ready"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_committed(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"committed\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_owned_ack(stream: &mut UnixStream) {
    match read_line_unbuffered(&mut *stream, Instant::now() + OWNED_ACK_TIMEOUT) {
        Ok(owned) if owned.trim_end() == "owned" => {}
        Ok(other) => {
            warn!(
                response = %other.trim_end(),
                "handoff import sent unexpected ownership ack after commit"
            );
        }
        Err(err) => {
            warn!(err = %err, "handoff import ownership ack was not received after commit");
        }
    }
}

#[cfg(unix)]
pub(crate) fn receive(socket_path: &Path, token: &str) -> io::Result<ReceivedHandoff> {
    let mut stream = UnixStream::connect(socket_path)?;
    stream.write_all(token.as_bytes())?;
    stream.write_all(b"\n")?;
    stream.flush()?;

    let manifest_line = read_line_unbuffered(&mut stream, Instant::now() + READY_TIMEOUT)?;
    let manifest: HandoffManifest =
        serde_json::from_str(&manifest_line).map_err(io::Error::other)?;
    if manifest.version != HANDOFF_VERSION {
        return Err(io::Error::other(format!(
            "unsupported handoff version {}",
            manifest.version
        )));
    }
    if manifest
        .expected_protocol
        .is_some_and(|protocol| protocol != crate::protocol::PROTOCOL_VERSION)
    {
        return Err(io::Error::other(format!(
            "handoff expected protocol {}, but this server speaks protocol {}",
            manifest.expected_protocol.unwrap_or_default(),
            crate::protocol::PROTOCOL_VERSION
        )));
    }
    if manifest
        .expected_version
        .as_deref()
        .is_some_and(|version| version != crate::build_info::version())
    {
        return Err(io::Error::other(format!(
            "handoff expected herdr v{}, but this server is v{}",
            manifest.expected_version.as_deref().unwrap_or("unknown"),
            crate::build_info::version()
        )));
    }
    stream.write_all(b"validated\n")?;
    stream.flush()?;
    let fds = recv_fds(&stream, manifest.panes.len())?;
    Ok(ReceivedHandoff {
        manifest,
        fds,
        stream,
    })
}

#[cfg(unix)]
pub(crate) fn report_restored(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"restored\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn report_ready(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"ready\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn wait_committed(stream: &mut UnixStream) -> io::Result<()> {
    let committed = read_line_unbuffered(&mut *stream, Instant::now() + READY_TIMEOUT)?;
    if committed.trim_end() != "committed" {
        return Err(io::Error::other("handoff source did not commit"));
    }
    Ok(())
}

#[cfg(unix)]
pub(crate) fn report_owned(stream: &mut UnixStream) -> io::Result<()> {
    stream.write_all(b"owned\n")?;
    stream.flush()
}

#[cfg(unix)]
pub(crate) fn manifest_for(
    snapshot: crate::persist::SessionSnapshot,
    panes: Vec<crate::handoff_runtime::HandoffRuntimeState>,
    expected_protocol: Option<u32>,
    expected_version: Option<String>,
    api_window_title: Option<String>,
) -> HandoffManifest {
    HandoffManifest {
        version: HANDOFF_VERSION,
        source_version: crate::build_info::version(),
        source_protocol: crate::protocol::PROTOCOL_VERSION,
        expected_version,
        expected_protocol,
        snapshot,
        panes,
        api_window_title,
    }
}

#[cfg(unix)]
fn restrict_socket_permissions(path: &Path) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
}

#[cfg(unix)]
fn accept_with_timeout(
    listener: &UnixListener,
    timeout: Duration,
) -> io::Result<(UnixStream, std::os::unix::net::SocketAddr)> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match listener.accept() {
            Ok(accepted) => return Ok(accepted),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "timed out waiting for handoff import connection",
                    ));
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => return Err(err),
        }
    }
}

#[cfg(unix)]
fn read_line_unbuffered(stream: &mut UnixStream, deadline: Instant) -> io::Result<String> {
    let mut bytes = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(handoff_line_timeout_error());
        }
        stream.set_read_timeout(Some(remaining))?;
        match stream.read(&mut byte) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "handoff stream closed while reading line",
                ));
            }
            Ok(_) => {}
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                ) =>
            {
                if Instant::now() >= deadline {
                    return Err(handoff_line_timeout_error());
                }
                continue;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
        if Instant::now() >= deadline {
            return Err(handoff_line_timeout_error());
        }
        bytes.push(byte[0]);
        if byte[0] == b'\n' {
            return String::from_utf8(bytes)
                .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
        }
        if bytes.len() > 16 * 1024 * 1024 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "handoff line exceeded maximum size",
            ));
        }
    }
}

#[cfg(unix)]
fn handoff_line_timeout_error() -> io::Error {
    io::Error::new(io::ErrorKind::TimedOut, "handoff line read timed out")
}

#[cfg(unix)]
fn send_fds(stream: &UnixStream, fds: &[RawFd]) -> io::Result<()> {
    if fds.is_empty() {
        return Ok(());
    }
    let byte = [b'F'];
    let iov = [libc::iovec {
        iov_base: byte.as_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = std::mem::size_of_val(fds);
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_ptr() as *mut libc::iovec;
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null() {
            return Err(io::Error::other("failed to allocate fd control message"));
        }
        (*cmsg).cmsg_level = libc::SOL_SOCKET;
        (*cmsg).cmsg_type = libc::SCM_RIGHTS;
        (*cmsg).cmsg_len = libc::CMSG_LEN(fd_bytes as u32) as _;
        std::ptr::copy_nonoverlapping(fds.as_ptr() as *const u8, libc::CMSG_DATA(cmsg), fd_bytes);
        if libc::sendmsg(stream.as_raw_fd(), &msg, 0) < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn recv_fds(stream: &UnixStream, expected: usize) -> io::Result<Vec<RawFd>> {
    if expected == 0 {
        return Ok(Vec::new());
    }
    let mut byte = [0u8; 1];
    let mut iov = [libc::iovec {
        iov_base: byte.as_mut_ptr() as *mut libc::c_void,
        iov_len: byte.len(),
    }];
    let fd_bytes = expected * std::mem::size_of::<RawFd>();
    let mut control = vec![0u8; unsafe { libc::CMSG_SPACE(fd_bytes as u32) as usize }];
    let mut msg: libc::msghdr = unsafe { std::mem::zeroed() };
    msg.msg_iov = iov.as_mut_ptr();
    msg.msg_iovlen = iov.len() as _;
    msg.msg_control = control.as_mut_ptr() as *mut libc::c_void;
    msg.msg_controllen = control.len() as _;

    let read = unsafe { libc::recvmsg(stream.as_raw_fd(), &mut msg, 0) };
    if read < 0 {
        return Err(io::Error::last_os_error());
    }
    if msg.msg_flags & libc::MSG_CTRUNC != 0 {
        return Err(io::Error::other("handoff fd control message was truncated"));
    }

    let mut out = Vec::new();
    unsafe {
        let cmsg = libc::CMSG_FIRSTHDR(&msg);
        if cmsg.is_null()
            || (*cmsg).cmsg_level != libc::SOL_SOCKET
            || (*cmsg).cmsg_type != libc::SCM_RIGHTS
        {
            return Err(io::Error::other("handoff fd message missing SCM_RIGHTS"));
        }
        let data_len = ((*cmsg).cmsg_len as usize).saturating_sub(libc::CMSG_LEN(0) as usize);
        let count = data_len / std::mem::size_of::<RawFd>();
        let data = libc::CMSG_DATA(cmsg) as *const RawFd;
        for idx in 0..count {
            out.push(*data.add(idx));
        }
    }
    if out.len() != expected {
        for fd in out {
            let _ = unsafe { libc::close(fd) };
        }
        return Err(io::Error::other(format!(
            "expected {expected} handoff fds, received fewer"
        )));
    }
    for &fd in &out {
        if let Err(err) = crate::pty::fd::set_cloexec(fd) {
            for fd in out {
                let _ = unsafe { libc::close(fd) };
            }
            return Err(err);
        }
    }
    Ok(out)
}

#[cfg(unix)]
pub(crate) fn log_import_result(panes: usize) {
    info!(panes, "handoff import ready");
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs::File;
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;

    fn empty_snapshot() -> crate::persist::SessionSnapshot {
        crate::persist::SessionSnapshot {
            version: 0,
            workspaces: Vec::new(),
            active: None,
            selected: 0,
            sidebar_width: None,
            sidebar_section_split: None,
            collapsed_space_keys: Default::default(),
        }
    }

    #[test]
    fn a_handoff_carries_an_api_set_window_title() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
        );

        assert_eq!(manifest.api_window_title.as_deref(), Some("deploying"));
    }

    #[test]
    fn handoff_socket_uses_a_short_fresh_private_temp_directory() {
        use std::os::unix::fs::MetadataExt;

        let mut socket = handoff_socket().expect("handoff socket should allocate");
        let path = socket.path().to_owned();
        let directory = path
            .parent()
            .expect("handoff socket should have a parent directory")
            .to_owned();
        let metadata = fs::metadata(&directory).expect("handoff directory should exist");

        assert_eq!(directory.parent(), Some(Path::new("/tmp")));
        assert_eq!(metadata.mode() & 0o077, 0);
        assert!(path.to_string_lossy().len() < 64);

        let listener = socket
            .bind_listener()
            .expect("handoff socket should bind inside its private directory");
        drop(listener);
        drop(socket);

        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn handoff_socket_cleans_up_after_bind_failure() {
        let mut socket = handoff_socket().expect("handoff socket should allocate");
        let path = socket.path().to_owned();
        let directory = path
            .parent()
            .expect("handoff socket should have a parent directory")
            .to_owned();
        fs::write(&path, b"occupied").expect("socket path should be occupied for the test");

        assert!(socket.bind_listener().is_err());
        drop(socket);

        assert!(!path.exists());
        assert!(!directory.exists());
    }

    #[test]
    fn a_manifest_written_before_the_title_field_still_loads() {
        let manifest = manifest_for(
            empty_snapshot(),
            Vec::new(),
            None,
            None,
            Some("deploying".to_string()),
        );
        let mut value = serde_json::to_value(&manifest).expect("manifest should serialize");
        value
            .as_object_mut()
            .expect("manifest should be a json object")
            .remove("api_window_title");

        let older: HandoffManifest =
            serde_json::from_value(value).expect("an older manifest should still load");

        assert!(older.api_window_title.is_none());
    }

    #[test]
    fn received_handoff_fds_close_on_exec() {
        let (sender, receiver) = UnixStream::pair().unwrap();
        let source = File::open("/dev/null").unwrap();

        send_fds(&sender, &[source.as_raw_fd()]).unwrap();
        let received = recv_fds(&receiver, 1).unwrap();
        let flags = unsafe { libc::fcntl(received[0], libc::F_GETFD) };

        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        unsafe { libc::close(received[0]) };
    }

    #[test]
    fn handoff_line_read_rejects_trickle_past_total_deadline() {
        use std::io::Write as _;

        let (mut writer, mut reader) = UnixStream::pair().unwrap();
        let (started_tx, started_rx) = std::sync::mpsc::sync_channel(1);
        let writer = std::thread::spawn(move || {
            writer.write_all(b"r").expect("write first line byte");
            writer.flush().expect("flush first line byte");
            started_tx.send(()).expect("signal first line byte");
            for byte in b"eady\n" {
                std::thread::sleep(Duration::from_millis(15));
                if writer.write_all(std::slice::from_ref(byte)).is_err() {
                    break;
                }
                if writer.flush().is_err() {
                    break;
                }
            }
        });
        let timeout = Duration::from_millis(50);
        let deadline = timeout + Duration::from_millis(250);

        started_rx.recv().expect("first line byte arrives");
        let started = std::time::Instant::now();
        let result = read_line_unbuffered(&mut reader, std::time::Instant::now() + timeout);

        assert!(
            matches!(
                result,
                Err(ref error) if error.kind() == io::ErrorKind::TimedOut
            ),
            "unexpected handoff line result: {result:?}"
        );
        assert!(
            started.elapsed() <= deadline,
            "trickled handoff line exceeded the {deadline:?} total deadline"
        );
        writer.join().expect("trickle writer join");
    }
}
