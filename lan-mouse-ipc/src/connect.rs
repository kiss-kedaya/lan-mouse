use crate::{ConnectionError, FrontendEvent, FrontendRequest, IpcError};
use std::{
    cmp::min,
    io::{self, BufReader, LineWriter, Lines, prelude::*},
    thread,
    time::Duration,
};

#[cfg(unix)]
use std::os::unix::net::UnixStream;

#[cfg(windows)]
use std::net::TcpStream;

pub struct FrontendEventReader {
    #[cfg(unix)]
    lines: Lines<BufReader<UnixStream>>,
    #[cfg(windows)]
    lines: Lines<BufReader<TcpStream>>,
}

pub struct FrontendRequestWriter {
    #[cfg(unix)]
    line_writer: LineWriter<UnixStream>,
    #[cfg(windows)]
    line_writer: LineWriter<TcpStream>,
}

impl FrontendEventReader {
    pub fn next_event(&mut self) -> Option<Result<FrontendEvent, IpcError>> {
        match self.lines.next()? {
            Err(e) => Some(Err(e.into())),
            Ok(l) => Some(serde_json::from_str(l.as_str()).map_err(|e| e.into())),
        }
    }
}

impl FrontendRequestWriter {
    pub fn request(&mut self, request: FrontendRequest) -> Result<(), io::Error> {
        let mut json = serde_json::to_string(&request).unwrap();
        log::debug!("requesting: {json}");
        json.push('\n');
        self.line_writer.write_all(json.as_bytes())?;
        Ok(())
    }
}

/// Try to connect to the service once without retrying.
pub fn try_connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    #[cfg(unix)]
    let rx = {
        let socket_path = crate::default_socket_path()?;
        UnixStream::connect(&socket_path)?
    };
    #[cfg(windows)]
    let rx = TcpStream::connect("127.0.0.1:5252")?;
    make_connection(rx)
}

/// like [`try_connect`], but returns `None` when nothing listens instead of
/// an error; the caller then starts a service of its own
pub fn try_connect_opt() -> Option<(FrontendEventReader, FrontendRequestWriter)> {
    #[cfg(unix)]
    let rx = UnixStream::connect(crate::default_socket_path().ok()?).ok()?;
    #[cfg(windows)]
    let rx = TcpStream::connect("127.0.0.1:5252").ok()?;
    make_connection(rx).ok()
}

pub fn connect() -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let rx = wait_for_service()?;
    make_connection(rx)
}

/// Connects, giving the service up to `timeout` to open its socket. A
/// freshly started service needs a moment (it loads the certificate, binds
/// its ports), and a frontend that gave up in that moment finds a service
/// running later but has no window to show it.
pub fn connect_with_timeout(
    timeout: Duration,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let deadline = std::time::Instant::now() + timeout;
    let rx = wait_for_service_until(deadline)?;
    make_connection(rx)
}

#[cfg(unix)]
fn make_connection(
    rx: UnixStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { lines };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

#[cfg(windows)]
fn make_connection(
    rx: TcpStream,
) -> Result<(FrontendEventReader, FrontendRequestWriter), ConnectionError> {
    let tx = rx.try_clone()?;
    let buf_reader = BufReader::new(rx);
    let lines = buf_reader.lines();
    let line_writer = LineWriter::new(tx);
    let reader = FrontendEventReader { lines };
    let writer = FrontendRequestWriter { line_writer };
    Ok((reader, writer))
}

/// wait for the lan-mouse socket to come online
#[cfg(unix)]
fn wait_for_service() -> Result<UnixStream, ConnectionError> {
    wait_for_service_until(std::time::Instant::now() + RETRY_FOREVER)
}

#[cfg(unix)]
fn wait_for_service_until(deadline: std::time::Instant) -> Result<UnixStream, ConnectionError> {
    let socket_path = crate::default_socket_path()?;
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = UnixStream::connect(&socket_path) {
            break Ok(stream);
        }
        if std::time::Instant::now() >= deadline {
            break Err(ConnectionError::Timeout);
        }
        // a signaling mechanism or inotify could be used to
        // improve this
        thread::sleep(exponential_back_off(&mut duration));
    }
}

#[cfg(windows)]
fn wait_for_service() -> Result<TcpStream, ConnectionError> {
    wait_for_service_until(std::time::Instant::now() + RETRY_FOREVER)
}

#[cfg(windows)]
fn wait_for_service_until(deadline: std::time::Instant) -> Result<TcpStream, ConnectionError> {
    let mut duration = Duration::from_millis(10);
    loop {
        if let Ok(stream) = TcpStream::connect("127.0.0.1:5252") {
            break Ok(stream);
        }
        if std::time::Instant::now() >= deadline {
            break Err(ConnectionError::Timeout);
        }
        thread::sleep(exponential_back_off(&mut duration));
    }
}

/// the default of `connect` and `connect_async` without a timeout
const RETRY_FOREVER: Duration = Duration::from_secs(60 * 60 * 24 * 365);

fn exponential_back_off(duration: &mut Duration) -> Duration {
    let new = duration.saturating_mul(2);
    *duration = min(new, Duration::from_secs(1));
    *duration
}
