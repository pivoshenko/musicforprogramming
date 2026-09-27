//! The daemon client: connecting, autostarting, and one request at a time with a timeout.
//!
//! Synchronous by design: the interface is a blocking `ratatui` loop, so the client is a
//! plain `UnixStream` with read timeouts rather than a second async runtime.

use std::collections::VecDeque;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use mfp_core::Catalog;
use mfp_core::error::ErrorCode;
use mfp_core::paths;
use mfp_core::protocol::{
    Command, ErrorObject, EventFrame, Frame, Request, ResultBody, StateSnapshot,
};

/// How long a request may wait for its response.
///
/// Work that outlasts this - loading, downloading, a streamed seek of around 11 seconds -
/// is acknowledged well inside it and reports progress through snapshots, never tripping it.
pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

/// How long to retry connecting after autostarting a daemon.
pub const AUTOSTART_TIMEOUT: Duration = Duration::from_secs(2);

/// The shortest wait a read may be given: `set_read_timeout` rejects a zero duration.
const MIN_WAIT: Duration = Duration::from_millis(1);

/// The longest a single read blocks before the caller gets its deadline checked again.
const READ_SLICE: Duration = Duration::from_millis(50);

/// Why a request did not produce a result.
#[derive(Debug)]
pub enum ClientError {
    /// No daemon could be reached or started. The message names the daemon log file.
    /// Reported to a headless caller as [`crate::cli::EXIT_UNREACHABLE`].
    Unreachable(String),
    /// The daemon answered with a structured error. Reported as
    /// [`crate::cli::EXIT_REJECTED`].
    Rejected(ErrorObject),
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Unreachable(message) => write!(f, "The daemon is unreachable: {message}"),
            Self::Rejected(error) => write!(f, "{}", error.message),
        }
    }
}

impl std::error::Error for ClientError {}

#[derive(Debug)]
pub struct Client {
    stream: UnixStream,
    /// Bytes read from the socket that do not yet form a whole line. A read can return half
    /// a frame, and `BufRead::read_line` would drop the partial line when its read times out,
    /// so lines are reassembled here.
    pending: Vec<u8>,
    /// Events that arrived while a response was being waited for.
    events: VecDeque<EventFrame>,
    /// The read timeout in effect on the socket, so it is set only when it changes rather
    /// than before every read.
    read_timeout: Option<Duration>,
    next_id: i64,
    timeout: Duration,
}

impl Client {
    /// Connects, autostarting a daemon detached from this terminal when the endpoint is
    /// missing or refuses - never for any other failure - then retrying with backoff for up
    /// to [`AUTOSTART_TIMEOUT`].
    pub fn connect() -> Result<Self, ClientError> {
        Self::connect_autostarting(&paths::socket_path(), &daemon_program())
    }

    /// Connects to a daemon already listening, without starting one.
    ///
    /// What the interface's retry loop uses: [`Self::connect`] would spawn a daemon and block
    /// on it for up to [`AUTOSTART_TIMEOUT`], so against one that crashes as it comes up the
    /// event loop would read no key for two seconds, the key that quits included.
    pub fn reattach() -> Result<Self, ClientError> {
        Self::connect_at(&paths::socket_path())
    }

    /// Connects to a daemon already listening at `path`, without starting one.
    pub fn connect_at(path: &Path) -> Result<Self, ClientError> {
        UnixStream::connect(path)
            .map_err(|error| ClientError::Unreachable(format!("{}: {error}", path.display())))
            .and_then(Self::from_stream)
    }

    /// Overrides [`REQUEST_TIMEOUT`] for this connection.
    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

    /// Sends a command and waits for the response carrying its id, up to
    /// [`REQUEST_TIMEOUT`]. Events arriving meanwhile are buffered, not discarded.
    pub fn request(&mut self, command: Command) -> Result<ResultBody, ClientError> {
        let id = self.next_id;
        self.next_id += 1;

        let mut line = serde_json::to_string(&Request { id, cmd: command })
            .map_err(|error| ClientError::Unreachable(error.to_string()))?;
        line.push('\n');
        self.stream
            .write_all(line.as_bytes())
            .and_then(|()| self.stream.flush())
            .map_err(|error| ClientError::Unreachable(error.to_string()))?;

        let deadline = Instant::now() + self.timeout;
        loop {
            match self.read_frame(deadline)? {
                Frame::Event(event) => self.events.push_back(event),
                Frame::Response(response) if response.id == Some(id) => {
                    if let Some(error) = response.error {
                        return Err(ClientError::Rejected(error));
                    }
                    return response.result.ok_or_else(|| {
                        ClientError::Rejected(ErrorObject {
                            code: ErrorCode::Internal,
                            message: "The daemon answered with neither a result nor an error"
                                .into(),
                        })
                    });
                }
                // A response to a request this client has already given up on
                Frame::Response(_) => {}
            }
        }
    }

    pub fn status(&mut self) -> Result<StateSnapshot, ClientError> {
        match self.request(Command::Status)? {
            ResultBody::State { state } => Ok(state),
            _ => Err(ClientError::Rejected(ErrorObject {
                code: ErrorCode::Internal,
                message: "The daemon answered status without a snapshot".into(),
            })),
        }
    }

    /// The catalog the daemon holds. Clients ask rather than read the cache file, so what
    /// they list is what the daemon serves from.
    pub fn catalog(&mut self) -> Result<Catalog, ClientError> {
        match self.request(Command::ListCatalog)? {
            ResultBody::Catalog { catalog } => Ok(catalog),
            _ => Err(ClientError::Rejected(ErrorObject {
                code: ErrorCode::Internal,
                message: "The daemon answered list_catalog without a catalog".into(),
            })),
        }
    }

    /// Starts pushed state events on this connection.
    pub fn subscribe(&mut self) -> Result<(), ClientError> {
        self.request(Command::Subscribe { spectrum: true })
            .map(|_| ())
    }

    /// Reads the next line, which is either a response or a pushed event.
    pub fn next_frame(&mut self) -> Result<Frame, ClientError> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Frame::Event(event));
        }
        self.read_frame(Instant::now() + self.timeout)
    }

    /// The next frame if one is already there, waiting no longer than `wait` for it.
    ///
    /// What keeps the interface redrawing, however long the daemon takes to say something.
    pub fn poll_frame(&mut self, wait: Duration) -> Result<Option<Frame>, ClientError> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Some(Frame::Event(event)));
        }
        loop {
            if let Some(line) = self.take_line() {
                return self.parse_line(&line).map(Some);
            }
            if !self.fill(wait)? {
                return Ok(None);
            }
        }
    }

    /// The socket's descriptor, so a caller can wait on this connection and on the
    /// terminal in one blocking call rather than alternating short timeouts on each.
    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.stream.as_raw_fd()
    }

    /// An event buffered while a response was being waited for.
    pub fn take_event(&mut self) -> Option<EventFrame> {
        self.events.pop_front()
    }

    fn connect_autostarting(socket: &Path, daemon: &Path) -> Result<Self, ClientError> {
        match UnixStream::connect(socket) {
            Ok(stream) => return Self::from_stream(stream),
            Err(error)
                if !matches!(
                    error.kind(),
                    ErrorKind::NotFound | ErrorKind::ConnectionRefused
                ) =>
            {
                return Err(ClientError::Unreachable(format!(
                    "{}: {error}",
                    socket.display()
                )));
            }
            Err(_) => {}
        }

        spawn_daemon(daemon)?;

        let deadline = Instant::now() + AUTOSTART_TIMEOUT;
        let mut backoff = Duration::from_millis(25);
        loop {
            std::thread::sleep(backoff);
            if let Ok(stream) = UnixStream::connect(socket) {
                return Self::from_stream(stream);
            }
            if Instant::now() >= deadline {
                let log = paths::log_file()
                    .map(|path| path.display().to_string())
                    .unwrap_or_else(|_| "the daemon log".into());
                return Err(ClientError::Unreachable(format!(
                    "Started {} but it did not accept a connection on {} within {:?}; see {log}",
                    daemon.display(),
                    socket.display(),
                    AUTOSTART_TIMEOUT
                )));
            }
            backoff = (backoff * 2).min(Duration::from_millis(200));
        }
    }

    fn from_stream(stream: UnixStream) -> Result<Self, ClientError> {
        stream
            .set_write_timeout(Some(REQUEST_TIMEOUT))
            .map_err(|error| ClientError::Unreachable(error.to_string()))?;
        Ok(Self {
            stream,
            pending: Vec::new(),
            events: VecDeque::new(),
            read_timeout: None,
            next_id: 1,
            timeout: REQUEST_TIMEOUT,
        })
    }

    fn read_frame(&mut self, deadline: Instant) -> Result<Frame, ClientError> {
        loop {
            if let Some(line) = self.take_line() {
                return self.parse_line(&line);
            }
            let now = Instant::now();
            if now >= deadline {
                return Err(ClientError::Unreachable(format!(
                    "No answer within {:?}",
                    self.timeout
                )));
            }
            self.fill((deadline - now).min(READ_SLICE))?;
        }
    }

    /// Reads whatever is available, waiting at most `wait`. `false` means nothing arrived
    /// in that time, which is not an error.
    fn fill(&mut self, wait: Duration) -> Result<bool, ClientError> {
        self.set_read_timeout(wait.max(MIN_WAIT));

        let mut chunk = [0u8; 4096];
        match self.stream.read(&mut chunk) {
            Ok(0) => Err(ClientError::Unreachable(
                "The daemon closed the connection".into(),
            )),
            Ok(read) => {
                self.pending.extend_from_slice(&chunk[..read]);
                Ok(true)
            }
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                Ok(false)
            }
            Err(error) => Err(ClientError::Unreachable(error.to_string())),
        }
    }

    /// Sets the read timeout, and only when it differs from the one already in effect.
    ///
    /// A failure is deliberately not fatal: on Darwin `setsockopt(SO_RCVTIMEO)` fails with
    /// `EINVAL` once the peer has closed, though everything it wrote before closing is still
    /// readable, so treating that as a lost connection would throw away a response already
    /// arrived. The last timeout set stays in effect, and against a closed peer the next read
    /// returns at once anyway, so skipping this can never block.
    fn set_read_timeout(&mut self, timeout: Duration) {
        if self.read_timeout == Some(timeout) {
            return;
        }
        if self.stream.set_read_timeout(Some(timeout)).is_ok() {
            self.read_timeout = Some(timeout);
        }
    }

    fn take_line(&mut self) -> Option<String> {
        let end = self.pending.iter().position(|byte| *byte == b'\n')?;
        let line = self.pending.drain(..=end).collect::<Vec<_>>();
        Some(String::from_utf8_lossy(&line[..end]).into_owned())
    }

    fn parse_line(&self, line: &str) -> Result<Frame, ClientError> {
        serde_json::from_str(line).map_err(|error| {
            ClientError::Unreachable(format!(
                "The daemon sent a frame this client cannot read: {error}"
            ))
        })
    }
}

/// The daemon executable to autostart: `$MFP_DAEMON`, else `mfp-daemon` beside this
/// binary, else whatever `PATH` resolves.
fn daemon_program() -> PathBuf {
    if let Some(override_path) = std::env::var_os("MFP_DAEMON").filter(|value| !value.is_empty()) {
        return PathBuf::from(override_path);
    }
    if let Ok(current) = std::env::current_exe()
        && let Some(sibling) = current.parent().map(|dir| dir.join("mfp-daemon"))
        && sibling.is_file()
    {
        return sibling;
    }
    PathBuf::from("mfp-daemon")
}

/// Daemons this process started, held only so they can be reaped.
///
/// A `Child` dropped without being waited on stays defunct until this process exits, and the
/// interface retries a lost daemon every couple of seconds - against one that dies as it
/// starts, that is a defunct process per attempt for the rest of the session.
static SPAWNED: std::sync::Mutex<Vec<std::process::Child>> = std::sync::Mutex::new(Vec::new());

/// Drops every child that has already exited, waiting on it first so it does not stay
/// defunct. A running child is kept: the daemon is meant to outlive this process and must
/// not be waited on.
fn reap_finished(children: &mut Vec<std::process::Child>) {
    children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
}

/// Starts a daemon in its own process group with its output in the log file, so it
/// survives this process and can never write over the interface.
fn spawn_daemon(program: &Path) -> Result<(), ClientError> {
    let log = paths::log_file().map_err(|error| {
        ClientError::Unreachable(format!("Cannot resolve the log file: {error}"))
    })?;
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| ClientError::Unreachable(format!("{}: {error}", parent.display())))?;
    }
    let out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log)
        .map_err(|error| ClientError::Unreachable(format!("{}: {error}", log.display())))?;
    let err = out
        .try_clone()
        .map_err(|error| ClientError::Unreachable(error.to_string()))?;

    let child = std::process::Command::new(program)
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err)
        .process_group(0)
        .spawn()
        .map_err(|error| {
            ClientError::Unreachable(format!(
                "Cannot start {}: {error}; see {}",
                program.display(),
                log.display()
            ))
        })?;

    // Recovered from poisoning rather than propagated: nothing here can panic while the
    // lock is held, and failing a daemon start over a lock is worse than a stray child
    let mut spawned = SPAWNED.lock().unwrap_or_else(|error| error.into_inner());
    reap_finished(&mut spawned);
    spawned.push(child);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::io::BufRead;
    use std::os::unix::net::UnixListener;
    use std::path::PathBuf;
    use std::thread::JoinHandle;

    use mfp_core::protocol::{Event, PlaybackState, Response};

    use super::*;

    /// A listener that hands each accepted connection to `script`, so a test can drive
    /// exactly the frames it cares about without a real daemon.
    fn fake_daemon<F>(path: PathBuf, script: F) -> JoinHandle<()>
    where
        F: Fn(UnixStream) + Send + 'static,
    {
        let listener = UnixListener::bind(&path).unwrap();
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                match stream {
                    Ok(stream) => script(stream),
                    Err(_) => break,
                }
            }
        })
    }

    fn socket(dir: &tempfile::TempDir) -> PathBuf {
        dir.path().join("daemon.sock")
    }

    fn snapshot() -> StateSnapshot {
        let mut snapshot = StateSnapshot::stopped();
        snapshot.playback = PlaybackState::Playing;
        snapshot
    }

    fn read_request(stream: &UnixStream) -> Request {
        let mut line = String::new();
        std::io::BufReader::new(stream)
            .read_line(&mut line)
            .unwrap();
        serde_json::from_str(&line).unwrap()
    }

    fn write_frame(mut stream: &UnixStream, value: serde_json::Value) {
        let mut line = value.to_string();
        line.push('\n');
        stream.write_all(line.as_bytes()).unwrap();
        stream.flush().unwrap();
    }

    #[test]
    fn a_request_that_is_never_answered_times_out_rather_than_hanging() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket(&dir);
        // hold the connection open and say nothing: the wedged-daemon case
        let _daemon = fake_daemon(path.clone(), |stream| {
            let _ = read_request(&stream);
            std::thread::sleep(Duration::from_secs(2));
        });

        let mut client = Client::connect_at(&path).unwrap();
        client.set_request_timeout(Duration::from_millis(150));

        let started = Instant::now();
        let failure = client.request(Command::Pause).unwrap_err();

        assert!(
            matches!(failure, ClientError::Unreachable(_)),
            "{failure:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "the request hung for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_response_is_matched_to_its_request_and_events_are_buffered() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket(&dir);
        let _daemon = fake_daemon(path.clone(), |stream| {
            let request = read_request(&stream);
            // an event overtakes the response, which must not be mistaken for it
            write_frame(
                &stream,
                serde_json::to_value(EventFrame {
                    event: Event::State { state: snapshot() },
                })
                .unwrap(),
            );
            // the pause forces two separate reads rather than both frames landing in one,
            // which is the ordering that matters
            std::thread::sleep(Duration::from_millis(60));
            write_frame(
                &stream,
                serde_json::to_value(Response::state(request.id, snapshot())).unwrap(),
            );
        });

        let mut client = Client::connect_at(&path).unwrap();
        let state = client.status().unwrap();

        assert_eq!(state.playback, PlaybackState::Playing);
        assert!(
            matches!(client.take_event(), Some(EventFrame { .. })),
            "the event that overtook the response was discarded"
        );
    }

    #[test]
    fn a_response_written_just_before_the_daemon_hangs_up_is_still_delivered() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket(&dir);
        let _daemon = fake_daemon(path.clone(), |stream| {
            let request = read_request(&stream);
            // the pause makes the client come back for a second read *after* the hang-up
            std::thread::sleep(Duration::from_millis(60));
            write_frame(
                &stream,
                serde_json::to_value(Response::state(request.id, snapshot())).unwrap(),
            );
            // and the daemon goes away the instant it has answered. On Darwin the socket
            // then refuses setsockopt, while the answer it just wrote is still readable
            drop(stream);
        });

        let mut client = Client::connect_at(&path).unwrap();

        assert_eq!(
            client.status().unwrap().playback,
            PlaybackState::Playing,
            "an answer already in the receive buffer was discarded as unreachable"
        );
    }

    #[test]
    fn a_daemon_error_surfaces_as_a_rejection() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket(&dir);
        let _daemon = fake_daemon(path.clone(), |stream| {
            let request = read_request(&stream);
            write_frame(
                &stream,
                serde_json::to_value(Response::failure(
                    Some(request.id),
                    &mfp_core::Error::NotPlaying,
                ))
                .unwrap(),
            );
        });

        let mut client = Client::connect_at(&path).unwrap();
        let failure = client
            .request(Command::Seek {
                position_secs: Some(30.0),
                delta_secs: None,
            })
            .unwrap_err();

        match failure {
            ClientError::Rejected(error) => assert_eq!(error.code, ErrorCode::NotPlaying),
            other => panic!("expected a rejection, got {other:?}"),
        }
    }

    #[test]
    fn polling_returns_nothing_when_the_daemon_is_quiet() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket(&dir);
        let _daemon = fake_daemon(path.clone(), |_stream| {
            std::thread::sleep(Duration::from_millis(400));
        });

        let mut client = Client::connect_at(&path).unwrap();
        let started = Instant::now();

        assert!(
            client
                .poll_frame(Duration::from_millis(5))
                .unwrap()
                .is_none()
        );
        assert!(started.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn a_frame_split_across_reads_is_reassembled() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket(&dir);
        let _daemon = fake_daemon(path.clone(), |mut stream| {
            let line = serde_json::to_string(&EventFrame {
                event: Event::State { state: snapshot() },
            })
            .unwrap();
            let (head, tail) = line.split_at(line.len() / 2);
            stream.write_all(head.as_bytes()).unwrap();
            stream.flush().unwrap();
            std::thread::sleep(Duration::from_millis(60));
            stream.write_all(tail.as_bytes()).unwrap();
            stream.write_all(b"\n").unwrap();
            stream.flush().unwrap();
        });

        let mut client = Client::connect_at(&path).unwrap();
        assert!(matches!(client.next_frame().unwrap(), Frame::Event(_)));
    }

    #[test]
    fn a_lost_daemon_is_unreachable_and_a_later_one_is_reachable_again() {
        let dir = tempfile::tempdir().unwrap();
        let path = socket(&dir);
        let first = fake_daemon(path.clone(), drop);

        let mut client = Client::connect_at(&path).unwrap();
        let failure = client.status().unwrap_err();
        assert!(
            matches!(failure, ClientError::Unreachable(_)),
            "{failure:?}"
        );
        drop(first);

        // the daemon comes back at the same endpoint and the interface reattaches
        std::fs::remove_file(&path).unwrap();
        let _second = fake_daemon(path.clone(), |stream| {
            let request = read_request(&stream);
            write_frame(
                &stream,
                serde_json::to_value(Response::state(request.id, snapshot())).unwrap(),
            );
        });
        let mut client = Client::connect_at(&path).unwrap();
        assert_eq!(client.status().unwrap().playback, PlaybackState::Playing);
    }

    // == Reattaching ==

    /// The interface retries a lost daemon on this path, inside its event loop. The
    /// autostarting path would block that loop for [`AUTOSTART_TIMEOUT`] per attempt against
    /// a daemon that dies as it comes up, so no key would be read.
    #[test]
    fn reattaching_to_a_missing_daemon_fails_at_once_rather_than_waiting_on_an_autostart() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();

        let failure = Client::connect_at(&socket(&dir)).unwrap_err();

        assert!(
            matches!(failure, ClientError::Unreachable(_)),
            "{failure:?}"
        );
        assert!(
            started.elapsed() * 4 < AUTOSTART_TIMEOUT,
            "the retry path blocked for {:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_daemon_that_has_already_exited_is_reaped_rather_than_left_defunct() {
        let mut children = vec![
            std::process::Command::new("/usr/bin/true").spawn().unwrap(),
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        ];
        // `try_wait` reports nothing until the child has actually gone
        while !matches!(children[0].try_wait(), Ok(Some(_))) {
            std::thread::sleep(Duration::from_millis(5));
        }

        reap_finished(&mut children);

        assert_eq!(children.len(), 1, "the exited daemon was left defunct");
        children[0].kill().unwrap();
        children[0].wait().unwrap();
    }

    #[test]
    fn a_daemon_still_running_is_never_waited_on() {
        let mut children = vec![
            std::process::Command::new("/bin/sleep")
                .arg("30")
                .spawn()
                .unwrap(),
        ];

        reap_finished(&mut children);

        assert_eq!(
            children.len(),
            1,
            "a live daemon was waited on, which would block until it exits"
        );
        children[0].kill().unwrap();
        children[0].wait().unwrap();
    }

    #[test]
    fn a_daemon_that_cannot_be_started_is_reported_rather_than_waited_on() {
        let dir = tempfile::tempdir().unwrap();
        let started = Instant::now();

        let failure =
            Client::connect_autostarting(&socket(&dir), &dir.path().join("no-such-daemon"))
                .unwrap_err();

        match failure {
            ClientError::Unreachable(message) => assert!(
                message.contains("no-such-daemon"),
                "the message does not name the daemon: {message}"
            ),
            other => panic!("expected unreachable, got {other:?}"),
        }
        assert!(started.elapsed() < AUTOSTART_TIMEOUT);
    }
}
