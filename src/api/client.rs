use std::fmt;
use std::io::{self, BufRead, BufReader, Write};
#[cfg(not(windows))]
use std::path::Path;
use std::path::PathBuf;
use std::thread;
use std::time::{Duration, Instant};

use crate::api::schema::{
    ErrorResponse, Method, PingParams, Request, ResponseResult, SuccessResponse,
};
use crate::ipc::LocalStream;

#[cfg(windows)]
use interprocess::local_socket::traits::Stream as _;
#[cfg(not(windows))]
use interprocess::{
    local_socket::{prelude::*, ConnectOptions, GenericFilePath},
    ConnectWaitMode,
};
/// API connection target resolved by clients at the process edge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ConnectionTarget {
    LocalSession(Option<String>),
    SocketPath(PathBuf),
}

impl ConnectionTarget {
    fn socket_path(&self) -> PathBuf {
        match self {
            Self::LocalSession(None) => crate::api::socket_path(),
            Self::LocalSession(Some(name)) => crate::session::api_socket_path_for(Some(name)),
            Self::SocketPath(path) => path.clone(),
        }
    }
}

/// Reusable client for Herdr's newline-delimited JSON API.
#[derive(Debug, Clone)]
pub struct ApiClient {
    target: ConnectionTarget,
}

impl ApiClient {
    pub fn local() -> Self {
        Self::for_target(ConnectionTarget::LocalSession(None))
    }

    pub fn for_target(target: ConnectionTarget) -> Self {
        Self { target }
    }

    pub fn socket_path(&self) -> PathBuf {
        self.target.socket_path()
    }

    pub fn request(&self, request: Request) -> Result<SuccessResponse, ApiClientError> {
        let value = self.request_value(&request)?;
        parse_response_value(value)
    }

    pub fn request_value(&self, request: &Request) -> Result<serde_json::Value, ApiClientError> {
        let request_bytes = serialize_request_line(request)?;
        let mut stream = self.connect()?;
        write_request(&mut stream, &request_bytes)?;

        let mut reader = BufReader::new(stream);
        read_json_line(&mut reader)
    }

    pub fn request_value_with_timeout(
        &self,
        request: &Request,
        timeout: Duration,
    ) -> Result<serde_json::Value, ApiClientError> {
        let request_bytes = serialize_request_line(request)?;
        let deadline = request_deadline(timeout)?;

        #[cfg(windows)]
        {
            let mut stream = self.connect()?;
            request_value_with_windows_timeout(&mut stream, &request_bytes, deadline)
        }

        #[cfg(not(windows))]
        {
            request_value_with_unix_timeout(&self.socket_path(), &request_bytes, deadline)
        }
    }

    pub fn status(&self) -> Result<crate::api::RuntimeStatus, ApiClientError> {
        let response = self.request(Request {
            id: "api-client:status".into(),
            method: Method::Ping(PingParams::default()),
        })?;
        match response.result {
            ResponseResult::Pong {
                version,
                protocol,
                capabilities,
            } => Ok(crate::api::RuntimeStatus {
                version: Some(version),
                protocol: Some(protocol),
                capabilities,
            }),
            result => Err(ApiClientError::UnexpectedResult(format!("{result:?}"))),
        }
    }

    fn connect(&self) -> io::Result<LocalStream> {
        crate::ipc::connect_local_stream(&self.socket_path())
    }
}

#[cfg(not(windows))]
const UNIX_REQUEST_POLL_INTERVAL: Duration = Duration::from_millis(5);

#[cfg(not(windows))]
fn request_value_with_unix_timeout(
    socket_path: &Path,
    request_bytes: &[u8],
    deadline: Instant,
) -> Result<serde_json::Value, ApiClientError> {
    let remaining = ensure_unix_request_before_deadline(deadline)?;
    let mut stream = connect_unix_stream_with_timeout(socket_path, remaining)
        .map_err(|error| normalize_unix_deadline_error(error, deadline))?;
    ensure_unix_request_before_deadline(deadline)?;
    crate::ipc::set_local_stream_polling(&mut stream, true)?;

    write_unix_request_until_deadline(&mut stream, request_bytes, deadline)?;
    read_unix_response_until_deadline(&mut stream, deadline)
}

#[cfg(not(windows))]
fn connect_unix_stream_with_timeout(path: &Path, timeout: Duration) -> io::Result<LocalStream> {
    let name = path.to_fs_name::<GenericFilePath>()?;
    ConnectOptions::new()
        .name(name)
        .wait_mode(ConnectWaitMode::Timeout(timeout))
        .connect_sync()
}

#[cfg(not(windows))]
fn write_unix_request_until_deadline(
    stream: &mut LocalStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), ApiClientError> {
    let mut written = 0;
    while written < bytes.len() {
        ensure_unix_request_before_deadline(deadline)?;
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                ensure_unix_request_before_deadline(deadline)?;
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "Herdr API socket accepted no request bytes",
                )
                .into());
            }
            Ok(count) => {
                written += count;
                ensure_unix_request_before_deadline(deadline)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_for_unix_request_progress(deadline)?;
            }
            Err(error) => return Err(normalize_unix_deadline_error(error, deadline)),
        }
    }
    stream
        .flush()
        .map_err(|error| normalize_unix_deadline_error(error, deadline))?;
    ensure_unix_request_before_deadline(deadline)?;
    Ok(())
}

#[cfg(not(windows))]
fn read_unix_response_until_deadline(
    stream: &mut LocalStream,
    deadline: Instant,
) -> Result<serde_json::Value, ApiClientError> {
    let mut line = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        ensure_unix_request_before_deadline(deadline)?;
        match std::io::Read::read(stream, &mut chunk) {
            Ok(0) => {
                ensure_unix_request_before_deadline(deadline)?;
                return Err(unterminated_response_error(&line));
            }
            Ok(read) => {
                let chunk = &chunk[..read];
                if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
                    append_json_line_bytes(&mut line, &chunk[..newline])?;
                    ensure_unix_request_before_deadline(deadline)?;
                    return parse_unix_json_line(&line);
                }
                append_json_line_bytes(&mut line, chunk)?;
                ensure_unix_request_before_deadline(deadline)?;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                wait_for_unix_request_progress(deadline)?;
            }
            Err(error) => return Err(normalize_unix_deadline_error(error, deadline)),
        }
    }
}

#[cfg(not(windows))]
fn parse_unix_json_line(line: &[u8]) -> Result<serde_json::Value, ApiClientError> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiClientError::EmptyResponse);
    }
    serde_json::from_slice(line).map_err(ApiClientError::Json)
}

#[cfg(not(windows))]
fn ensure_unix_request_before_deadline(deadline: Instant) -> Result<Duration, ApiClientError> {
    deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(unix_request_timed_out)
}

#[cfg(not(windows))]
fn wait_for_unix_request_progress(deadline: Instant) -> Result<(), ApiClientError> {
    let remaining = ensure_unix_request_before_deadline(deadline)?;
    thread::sleep(remaining.min(UNIX_REQUEST_POLL_INTERVAL));
    Ok(())
}

#[cfg(not(windows))]
fn normalize_unix_deadline_error(error: io::Error, deadline: Instant) -> ApiClientError {
    if matches!(
        error.kind(),
        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
    ) && Instant::now() >= deadline
    {
        return unix_request_timed_out();
    }
    error.into()
}

#[cfg(not(windows))]
fn unix_request_timed_out() -> ApiClientError {
    ApiClientError::Io(io::Error::new(
        io::ErrorKind::TimedOut,
        "Herdr API request timed out",
    ))
}

fn request_deadline(timeout: Duration) -> Result<Instant, ApiClientError> {
    Instant::now().checked_add(timeout).ok_or_else(|| {
        ApiClientError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Herdr API request timeout is too large",
        ))
    })
}

#[cfg(windows)]
const WINDOWS_REQUEST_POLL_INTERVAL: Duration = Duration::from_millis(5);
#[cfg(windows)]
#[cfg(windows)]
fn request_value_with_windows_timeout(
    stream: &mut LocalStream,
    request_bytes: &[u8],
    deadline: Instant,
) -> Result<serde_json::Value, ApiClientError> {
    stream.set_nonblocking(true)?;
    write_request_until_deadline(stream, request_bytes, deadline)?;
    read_response_until_deadline(stream, deadline)
}

#[cfg(windows)]
fn write_request_until_deadline(
    stream: &mut LocalStream,
    bytes: &[u8],
    deadline: Instant,
) -> Result<(), ApiClientError> {
    let mut written = 0;
    while written < bytes.len() {
        ensure_windows_request_before_deadline(deadline)?;
        match stream.write(&bytes[written..]) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "Herdr API named pipe accepted no request bytes",
                )
                .into());
            }
            Ok(count) => {
                written += count;
                ensure_windows_request_before_deadline(deadline)?;
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                ) =>
            {
                wait_for_windows_request_progress(deadline)?;
            }
            Err(error) => return Err(error.into()),
        }
    }
    stream.flush()?;
    Ok(())
}

#[cfg(windows)]
fn read_response_until_deadline(
    stream: &mut LocalStream,
    deadline: Instant,
) -> Result<serde_json::Value, ApiClientError> {
    let mut line = Vec::new();
    let mut chunk = [0_u8; 4096];
    loop {
        ensure_windows_request_before_deadline(deadline)?;
        match crate::ipc::poll_local_stream_read_count(stream, &mut chunk)? {
            crate::ipc::LocalStreamReadCount::Data(read) => {
                let chunk = &chunk[..read];
                if let Some(newline) = chunk.iter().position(|byte| *byte == b'\n') {
                    append_json_line_bytes(&mut line, &chunk[..newline])?;
                    ensure_windows_request_before_deadline(deadline)?;
                    if line.iter().all(u8::is_ascii_whitespace) {
                        return Err(ApiClientError::EmptyResponse);
                    }
                    return serde_json::from_slice(&line).map_err(ApiClientError::Json);
                }
                append_json_line_bytes(&mut line, chunk)?;
                ensure_windows_request_before_deadline(deadline)?;
            }
            crate::ipc::LocalStreamReadCount::Closed => {
                return Err(unterminated_response_error(&line));
            }
            crate::ipc::LocalStreamReadCount::Pending => {
                wait_for_windows_request_progress(deadline)?;
            }
        }
    }
}

fn append_json_line_bytes(line: &mut Vec<u8>, bytes: &[u8]) -> Result<(), ApiClientError> {
    if line.len().saturating_add(bytes.len()) > crate::api::MAX_JSON_LINE_BYTES {
        return Err(ApiClientError::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "Herdr API response line is too large",
        )));
    }
    line.extend_from_slice(bytes);
    Ok(())
}

#[cfg(windows)]
fn ensure_windows_request_before_deadline(deadline: Instant) -> Result<Duration, ApiClientError> {
    deadline
        .checked_duration_since(Instant::now())
        .ok_or_else(|| {
            ApiClientError::Io(io::Error::new(
                io::ErrorKind::TimedOut,
                "Herdr API request timed out",
            ))
        })
}

#[cfg(windows)]
fn wait_for_windows_request_progress(deadline: Instant) -> Result<(), ApiClientError> {
    let remaining = ensure_windows_request_before_deadline(deadline)?;
    thread::sleep(remaining.min(WINDOWS_REQUEST_POLL_INTERVAL));
    Ok(())
}

#[derive(Debug)]
pub enum ApiClientError {
    Io(io::Error),
    Json(serde_json::Error),
    ErrorResponse(ErrorResponse),
    EmptyResponse,
    UnexpectedResult(String),
}

impl fmt::Display for ApiClientError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "{err}"),
            Self::ErrorResponse(response) => write!(f, "{}", response.error.message),
            Self::EmptyResponse => write!(f, "empty api response"),
            Self::UnexpectedResult(result) => write!(f, "unexpected api result: {result}"),
        }
    }
}

impl std::error::Error for ApiClientError {}

impl From<io::Error> for ApiClientError {
    fn from(err: io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<serde_json::Error> for ApiClientError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

fn serialize_request_line(request: &Request) -> Result<Vec<u8>, ApiClientError> {
    let mut line = serde_json::to_vec(request)?;
    if line.len() > crate::api::MAX_JSON_LINE_BYTES {
        return Err(ApiClientError::Io(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Herdr API request line is too large",
        )));
    }
    line.push(b'\n');
    Ok(line)
}

fn write_request(stream: &mut LocalStream, bytes: &[u8]) -> Result<(), ApiClientError> {
    stream.write_all(bytes)?;
    stream.flush()?;
    Ok(())
}

fn read_json_line<T>(reader: &mut BufReader<LocalStream>) -> Result<T, ApiClientError>
where
    T: serde::de::DeserializeOwned,
{
    let mut line = Vec::new();
    let mut terminated = false;
    loop {
        let buffer = reader.fill_buf()?;
        if buffer.is_empty() {
            break;
        }
        let (bytes, consumed, complete) = match buffer.iter().position(|byte| *byte == b'\n') {
            Some(newline) => (&buffer[..newline], newline + 1, true),
            None => (buffer, buffer.len(), false),
        };
        append_json_line_bytes(&mut line, bytes)?;
        reader.consume(consumed);
        if complete {
            terminated = true;
            break;
        }
    }
    if !terminated {
        return Err(unterminated_response_error(&line));
    }
    if line.iter().all(u8::is_ascii_whitespace) {
        return Err(ApiClientError::EmptyResponse);
    }
    serde_json::from_slice(&line).map_err(ApiClientError::Json)
}

fn unterminated_response_error(line: &[u8]) -> ApiClientError {
    if line.iter().all(u8::is_ascii_whitespace) {
        ApiClientError::EmptyResponse
    } else {
        ApiClientError::Io(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "Herdr API response ended before newline",
        ))
    }
}

#[derive(serde::Deserialize)]
#[serde(untagged)]
enum WireResponse {
    Success(Box<SuccessResponse>),
    Error(ErrorResponse),
}

pub(crate) fn parse_response_value(
    value: serde_json::Value,
) -> Result<SuccessResponse, ApiClientError> {
    match serde_json::from_value(value)? {
        WireResponse::Success(response) => Ok(*response),
        WireResponse::Error(response) => Err(ApiClientError::ErrorResponse(response)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{SystemTime, UNIX_EPOCH};

    #[cfg(unix)]
    fn unique_test_socket_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        PathBuf::from("/tmp").join(format!(
            "herdr-api-client-{name}-{}-{nanos}.sock",
            std::process::id()
        ))
    }

    #[test]
    fn local_session_target_resolves_named_session_socket() {
        let client = ApiClient::for_target(ConnectionTarget::LocalSession(Some("work".into())));
        assert!(client.socket_path().ends_with("sessions/work/herdr.sock"));
    }

    #[test]
    fn socket_path_target_uses_explicit_path() {
        let path = PathBuf::from("/tmp/herdr-test.sock");
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        assert_eq!(client.socket_path(), path);
    }

    #[test]
    fn request_timeout_rejects_an_unrepresentable_deadline() {
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(PathBuf::from(
            "/tmp/herdr-api-client-missing.sock",
        )));
        let result = client.request_value_with_timeout(
            &Request {
                id: "api-client:oversized-timeout".into(),
                method: Method::Ping(PingParams::default()),
            },
            Duration::MAX,
        );

        let ApiClientError::Io(error) = result.unwrap_err() else {
            panic!("oversized timeout unexpectedly completed");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(error.to_string(), "Herdr API request timeout is too large");
    }

    #[test]
    fn request_rejects_an_oversized_request_before_connecting() {
        let request = Request {
            id: "api-client:oversized-request".into(),
            method: Method::PaneSendText(crate::api::schema::PaneSendTextParams {
                pane_id: "pane_1".into(),
                text: "x".repeat(crate::api::MAX_JSON_LINE_BYTES),
            }),
        };
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(PathBuf::from(
            "/tmp/herdr-api-client-missing.sock",
        )));

        for result in [
            client.request_value(&request),
            client.request_value_with_timeout(&request, Duration::from_secs(1)),
        ] {
            let ApiClientError::Io(error) = result.unwrap_err() else {
                panic!("oversized request unexpectedly completed");
            };
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert_eq!(error.to_string(), "Herdr API request line is too large");
        }
    }

    #[cfg(unix)]
    #[test]
    fn request_rejects_an_oversized_response_line() {
        let path = unique_test_socket_path("oversized-response");
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            let _ = stream.write_all(&vec![b'x'; crate::api::MAX_JSON_LINE_BYTES + 1]);
            let _ = stream.flush();
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let result = client.request_value(&Request {
            id: "api-client:oversized-response".into(),
            method: Method::Ping(PingParams::default()),
        });

        server.join().unwrap();
        let _ = std::fs::remove_file(path);

        let ApiClientError::Io(error) = result.unwrap_err() else {
            panic!("oversized response unexpectedly completed");
        };
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(error.to_string(), "Herdr API response line is too large");
    }

    #[cfg(unix)]
    #[test]
    fn request_rejects_an_unterminated_response() {
        let path = unique_test_socket_path("unterminated-response");
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            stream.write_all(br#"{"result":"unterminated"}"#).unwrap();
            stream.flush().unwrap();
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let result = client.request_value(&Request {
            id: "api-client:unterminated-response".into(),
            method: Method::Ping(PingParams::default()),
        });

        server.join().unwrap();
        let _ = std::fs::remove_file(path);

        let ApiClientError::Io(error) = result.unwrap_err() else {
            panic!("unterminated response unexpectedly completed");
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "Herdr API response ended before newline");
    }

    #[cfg(unix)]
    #[test]
    fn request_timeout_rejects_an_unterminated_response() {
        let path = unique_test_socket_path("unterminated-timed-response");
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();
            stream.write_all(br#"{"result":"unterminated"}"#).unwrap();
            stream.flush().unwrap();
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let result = client.request_value_with_timeout(
            &Request {
                id: "api-client:unterminated-timed-response".into(),
                method: Method::Ping(PingParams::default()),
            },
            Duration::from_secs(1),
        );

        server.join().unwrap();
        let _ = std::fs::remove_file(path);

        let ApiClientError::Io(error) = result.unwrap_err() else {
            panic!("unterminated response unexpectedly completed");
        };
        assert_eq!(error.kind(), io::ErrorKind::UnexpectedEof);
        assert_eq!(error.to_string(), "Herdr API response ended before newline");
    }

    #[cfg(unix)]
    #[test]
    fn request_timeout_is_total_while_response_trickles() {
        let path = unique_test_socket_path("trickled-response");
        let listener = crate::ipc::bind_local_listener(&path).unwrap();
        let server = thread::spawn(move || {
            let mut stream = listener.accept().unwrap();
            let mut request = String::new();
            BufReader::new(&mut stream).read_line(&mut request).unwrap();

            for byte in b"{\"result\":\"trickled response\"}\n" {
                if stream.write_all(&[*byte]).is_err() || stream.flush().is_err() {
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        });
        let client = ApiClient::for_target(ConnectionTarget::SocketPath(path.clone()));
        let result = client.request_value_with_timeout(
            &Request {
                id: "api-client:trickled-response".into(),
                method: Method::Ping(PingParams::default()),
            },
            Duration::from_millis(80),
        );

        server.join().unwrap();
        let _ = std::fs::remove_file(path);

        let ApiClientError::Io(error) = result.unwrap_err() else {
            panic!("trickled response unexpectedly completed within the total timeout");
        };
        assert_eq!(error.kind(), io::ErrorKind::TimedOut);
        assert_eq!(error.to_string(), "Herdr API request timed out");
    }
}
