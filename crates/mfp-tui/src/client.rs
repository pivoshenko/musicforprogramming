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

pub const REQUEST_TIMEOUT: Duration = Duration::from_secs(5);

pub const AUTOSTART_TIMEOUT: Duration = Duration::from_secs(2);

const MIN_WAIT: Duration = Duration::from_millis(1);

const READ_SLICE: Duration = Duration::from_millis(50);

#[derive(Debug)]
pub enum ClientError {
    Unreachable(String),

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

    pending: Vec<u8>,

    events: VecDeque<EventFrame>,

    read_timeout: Option<Duration>,
    next_id: i64,
    timeout: Duration,
}

impl Client {
    pub fn connect() -> Result<Self, ClientError> {
        Self::connect_autostarting(&paths::socket_path(), &daemon_program())
    }

    pub fn reattach() -> Result<Self, ClientError> {
        Self::connect_at(&paths::socket_path())
    }

    pub fn connect_at(path: &Path) -> Result<Self, ClientError> {
        UnixStream::connect(path)
            .map_err(|error| ClientError::Unreachable(format!("{}: {error}", path.display())))
            .and_then(Self::from_stream)
    }

    pub fn set_request_timeout(&mut self, timeout: Duration) {
        self.timeout = timeout;
    }

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

    pub fn catalog(&mut self) -> Result<Catalog, ClientError> {
        match self.request(Command::ListCatalog)? {
            ResultBody::Catalog { catalog } => Ok(catalog),
            _ => Err(ClientError::Rejected(ErrorObject {
                code: ErrorCode::Internal,
                message: "The daemon answered list_catalog without a catalog".into(),
            })),
        }
    }

    pub fn subscribe(&mut self) -> Result<(), ClientError> {
        self.request(Command::Subscribe { spectrum: true })
            .map(|_| ())
    }

    pub fn next_frame(&mut self) -> Result<Frame, ClientError> {
        if let Some(event) = self.events.pop_front() {
            return Ok(Frame::Event(event));
        }
        self.read_frame(Instant::now() + self.timeout)
    }

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

    pub fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.stream.as_raw_fd()
    }

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

static SPAWNED: std::sync::Mutex<Vec<std::process::Child>> = std::sync::Mutex::new(Vec::new());

fn reap_finished(children: &mut Vec<std::process::Child>) {
    children.retain_mut(|child| matches!(child.try_wait(), Ok(None)));
}

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

    let mut command = std::process::Command::new(program);
    command
        .stdin(std::process::Stdio::null())
        .stdout(out)
        .stderr(err);

    // SAFETY: `setsid` is async-signal-safe and touches nothing this fork shares
    unsafe {
        command.pre_exec(|| match libc::setsid() {
            -1 => Err(std::io::Error::last_os_error()),
            _ => Ok(()),
        });
    }

    let child = command.spawn().map_err(|error| {
        ClientError::Unreachable(format!(
            "Cannot start {}: {error}; see {}",
            program.display(),
            log.display()
        ))
    })?;

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

            write_frame(
                &stream,
                serde_json::to_value(EventFrame {
                    event: Event::State { state: snapshot() },
                })
                .unwrap(),
            );

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

            std::thread::sleep(Duration::from_millis(60));
            write_frame(
                &stream,
                serde_json::to_value(Response::state(request.id, snapshot())).unwrap(),
            );

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
