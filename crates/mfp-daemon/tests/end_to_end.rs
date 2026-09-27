//! The daemon as a process.
//!
//! Everything else in the suite runs the daemon in-process, with the audio device as its
//! one stand-in. This drives the built binary: its own socket, state file and caches, a
//! real episode streamed from upstream, and the state file read back as a user would.
//!
//! It reaches the network and needs an audio output device, so like the other tests that
//! touch the real world it is not run by default:
//! `cargo test -p mfp-daemon --test end_to_end -- --ignored --nocapture`.

// An integration test is its own crate, so the library's cfg(test) exemption does not reach it
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::float_cmp
)]

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::Value;

/// Generous: starting the daemon fetches the catalog, and a distant seek in a streamed
/// source is measured to take up to eleven seconds.
const PATIENCE: Duration = Duration::from_secs(90);

/// Far enough in that the store's 30-second start margin does not apply, and a position
/// no episode would reach on its own during the test.
const TARGET_SECS: f64 = 1_200.0;

/// A daemon process with every path it uses inside one directory.
struct Daemon {
    process: Child,
    socket: PathBuf,
}

impl Daemon {
    /// Starts the daemon and waits until it answers on its socket.
    fn start(root: &Path) -> Self {
        let socket = root.join("daemon.sock");
        let process = Command::new(env!("CARGO_BIN_EXE_mfp-daemon"))
            .env("MFP_SOCKET", &socket)
            // the MFP_* overrides are used exactly as given, with no `mfp` appended,
            // and they are the only way to scope an instance: the socket alone is not
            .env("MFP_STATE_DIR", root.join("state"))
            .env("MFP_CACHE_DIR", root.join("cache"))
            .env("MFP_CONFIG_DIR", root.join("config"))
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("the daemon binary is built");

        let deadline = Instant::now() + PATIENCE;
        while UnixStream::connect(&socket).is_err() {
            assert!(Instant::now() < deadline, "the daemon never answered");
            std::thread::sleep(Duration::from_millis(50));
        }
        Self { process, socket }
    }

    fn client(&self) -> Client {
        Client::connect(&self.socket)
    }

    /// Shuts the daemon down over the wire, then waits for the exit that does the final write.
    fn shutdown(mut self) {
        self.client()
            .request(r#"{"id":99,"cmd":{"type":"shutdown"}}"#);
        let deadline = Instant::now() + PATIENCE;
        loop {
            match self.process.try_wait().expect("the daemon is waitable") {
                Some(_) => return,
                None => assert!(Instant::now() < deadline, "the daemon did not exit"),
            }
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Daemon {
    fn drop(&mut self) {
        // a test that failed mid-flight must not leave a daemon holding the audio device
        let _ = self.process.kill();
        let _ = self.process.wait();
    }
}

struct Client {
    lines: std::io::Lines<BufReader<UnixStream>>,
    writer: UnixStream,
}

impl Client {
    fn connect(socket: &Path) -> Self {
        let writer = UnixStream::connect(socket).expect("the daemon is listening");
        let reader = writer.try_clone().expect("the socket is cloneable");
        Self {
            lines: BufReader::new(reader).lines(),
            writer,
        }
    }

    fn request(&mut self, line: &str) -> Value {
        self.writer.write_all(line.as_bytes()).unwrap();
        self.writer.write_all(b"\n").unwrap();
        let answered = self
            .lines
            .next()
            .expect("the daemon closed the connection")
            .unwrap();
        serde_json::from_str(&answered).unwrap()
    }

    fn status(&mut self) -> Value {
        self.request(r#"{"id":1,"cmd":{"type":"status"}}"#)["result"]["state"].clone()
    }

    /// Polls status until it satisfies `settled`, returning the snapshot that did.
    fn settle(&mut self, what: &str, settled: impl Fn(&Value) -> bool) -> Value {
        let deadline = Instant::now() + PATIENCE;
        loop {
            let status = self.status();
            if settled(&status) {
                return status;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; the last status was {status}"
            );
            std::thread::sleep(Duration::from_millis(200));
        }
    }
}

fn position(status: &Value) -> f64 {
    status["position_secs"].as_f64().unwrap()
}

/// The identifier of an episode long enough that the target sits well inside it.
fn a_long_episode(client: &mut Client) -> String {
    let catalog = client.request(r#"{"id":2,"cmd":{"type":"list_catalog"}}"#);
    let episodes = catalog["result"]["catalog"]["episodes"]
        .as_array()
        .expect("a catalog")
        .clone();
    let episode = episodes
        .iter()
        .find(|episode| episode["duration_secs"].as_f64().unwrap_or_default() > 3_600.0)
        .expect("an episode over an hour long");
    episode["slug"]
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| panic!("the catalog entry carries no identifier: {episode}"))
}

fn session(root: &Path) -> Value {
    let path = root.join("state").join("state.json");
    let text = std::fs::read_to_string(&path)
        .unwrap_or_else(|error| panic!("{} is unreadable: {error}", path.display()));
    serde_json::from_str(&text).unwrap()
}

/// The whole of the defect this suite exists for, at the level a user meets it: play,
/// move somewhere worth resuming from, shut the daemon down, and start it again.
#[test]
#[ignore = "spawns the daemon binary: needs an audio output device, reaches the network, and is audible -- the player has no volume of its own, so turn the system volume down first"]
fn a_position_reached_in_one_run_is_written_to_disk_and_resumed_in_the_next() {
    let root = tempfile::tempdir().unwrap();
    let daemon = Daemon::start(root.path());
    let mut client = daemon.client();
    let id = a_long_episode(&mut client);

    client.request(&format!(
        r#"{{"id":4,"cmd":{{"type":"play","slug":"{id}"}}}}"#
    ));
    client.settle("playback to start", |status| {
        status["playback"] == "playing"
    });
    client.request(&format!(
        r#"{{"id":5,"cmd":{{"type":"seek","position_secs":{TARGET_SECS}}}}}"#
    ));
    let reached = client.settle("the seek to land", |status| {
        status["playback"] == "playing" && position(status) >= TARGET_SECS
    });
    // long enough that the recording is the daemon's periodic one and not a coincidence
    std::thread::sleep(Duration::from_secs(2));
    drop(client);
    daemon.shutdown();

    let session = session(root.path());
    let recorded = session["positions"][&id]
        .as_f64()
        .unwrap_or_else(|| panic!("no position was recorded for {id}: {session}"));
    assert!(
        (recorded - position(&reached)).abs() < 10.0,
        "recorded {recorded} but playback had reached {}",
        position(&reached)
    );
    assert_eq!(session["last_played"], id.as_str());

    // and the next run starts the episode there rather than at the beginning
    let restarted = Daemon::start(root.path());
    let mut client = restarted.client();
    client.request(&format!(
        r#"{{"id":6,"cmd":{{"type":"play","slug":"{id}"}}}}"#
    ));
    let resumed = client.settle("playback to resume where it left off", |status| {
        status["playback"] == "playing" && position(status) > 0.0
    });
    assert!(
        position(&resumed) >= recorded - 10.0,
        "resumed at {} rather than near {recorded}",
        position(&resumed)
    );
    drop(client);
    restarted.shutdown();
}
