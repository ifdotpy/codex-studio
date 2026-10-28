#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::disallowed_methods)]
//! Live handover of a real server (src/handover.rs): the port stays open, SSE streams
//! end for a resume, a failed new server gives the socket back.

use std::{
    fs,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};
use studio_runtime_server::handover;
use tempfile::tempdir;
use tokio::{
    io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader},
    net::TcpStream,
    process::{Child, Command},
    time::timeout,
};

const READY_WAIT: Duration = Duration::from_secs(60);

struct Site {
    _root: tempfile::TempDir,
    state: PathBuf,
    home: PathBuf,
}

/// A failed test shows what each server wrote to stderr.
impl Drop for Site {
    fn drop(&mut self) {
        if std::thread::panicking() {
            for name in ["old", "new"] {
                let log = fs::read_to_string(self.home.join(format!("{name}.log")));
                eprintln!("--- {name} server stderr ---\n{}", log.unwrap_or_default());
            }
        }
    }
}

async fn site() -> Site {
    let root = tempdir().unwrap();
    let base = fs::canonicalize(root.path()).unwrap();
    let (state, home) = (base.join("state"), base.join("home"));
    fs::create_dir_all(&state).unwrap();
    fs::create_dir_all(&home).unwrap();
    let store = studio_runtime_store::RuntimeStore::open(Some(&state))
        .await
        .unwrap();
    crate::fixture_support::seed(
        &store,
        &state,
        crate::fixture_support::FixtureKind::Dispatch,
    )
    .await;
    drop(store);
    Site {
        _root: root,
        state,
        home,
    }
}

fn server(site: &Site, name: &str, extra: &[&str]) -> Child {
    let log = fs::File::create(site.home.join(format!("{name}.log"))).unwrap();
    Command::new(env!("CARGO_BIN_EXE_studio-runtime-server"))
        .args(["--port", "0"])
        .args(extra)
        .current_dir(&site.home)
        .env_clear()
        .env("HOME", &site.home)
        .env("PATH", "/usr/bin:/bin")
        .env("XDG_STATE_HOME", site.home.join("xdg-state"))
        .env("XDG_CACHE_HOME", site.home.join("cache"))
        .env("TMPDIR", &site.home)
        .env("CODEX_AGENTS_STATE_DIR", &site.state)
        .stdout(Stdio::piped())
        .stderr(log)
        .stdin(Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .unwrap()
}

/// The port from the readiness line of a started server.
async fn served_port(child: &mut Child) -> u16 {
    let stdout = child.stdout.take().unwrap();
    let mut lines = BufReader::new(stdout);
    let mut line = String::new();
    let read = timeout(READY_WAIT, lines.read_line(&mut line)).await;
    assert!(
        matches!(read, Ok(Ok(count)) if count > 0),
        "no readiness line: {line}"
    );
    // Keep the pipe open: a closed stdout would fail the server's later prints.
    std::mem::forget(lines);
    line.trim()
        .strip_prefix("Codex Canvas: http://127.0.0.1:")
        .unwrap()
        .parse()
        .unwrap()
}

async fn open_stream(port: u16) -> TcpStream {
    let protocol = studio_runtime_schema::sync_protocol::SYNC_PROTOCOL_VERSION;
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /api/sync/stream?protocol={protocol}&resources=%5B%7B%22kind%22%3A%22accounts%22%7D%5D HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nOrigin: http://127.0.0.1:{port}\r\nAccept: text/event-stream\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut seen = String::new();
    let mut buffer = [0_u8; 1024];
    timeout(Duration::from_secs(10), async {
        while !seen.contains("event: resources") {
            let count = stream.read(&mut buffer).await.unwrap();
            assert_ne!(count, 0, "stream ended early: {seen}");
            seen.push_str(&String::from_utf8_lossy(&buffer[..count]));
        }
    })
    .await
    .expect("initial event");
    stream
}

/// Wait until the server ends the stream. A clean end is what makes a client resume.
async fn stream_ends(mut stream: TcpStream) {
    let mut buffer = [0_u8; 1024];
    timeout(Duration::from_secs(30), async {
        while stream.read(&mut buffer).await.unwrap_or(0) != 0 {}
    })
    .await
    .expect("the stream did not end during the drain");
}

/// The PID in `/api/desktop`.
async fn serving_pid(port: u16) -> u32 {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.unwrap();
    stream
        .write_all(
            format!(
                "GET /api/desktop HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .unwrap();
    let mut body = String::new();
    timeout(Duration::from_secs(30), stream.read_to_string(&mut body))
        .await
        .expect("answer")
        .unwrap();
    let digits = body.split("\"pid\":").nth(1).expect(&body);
    digits
        .trim_start()
        .chars()
        .take_while(char::is_ascii_digit)
        .collect::<String>()
        .parse()
        .unwrap()
}

/// Asks every few milliseconds. `refused` counts requests that failed to connect or did not
/// get a 200 answer; `slowest`
/// is the longest connect-to-answer time in milliseconds.
struct Probe {
    stop: Arc<AtomicBool>,
    refused: Arc<AtomicU64>,
    slowest: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

/// One request without keep-alive. `None` when the connection fails or ends before any
/// answer byte.
async fn ask(port: u16) -> Option<Vec<u8>> {
    let mut stream = TcpStream::connect(("127.0.0.1", port)).await.ok()?;
    stream
        .write_all(
            format!(
                "GET /api/desktop HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
            )
            .as_bytes(),
        )
        .await
        .ok()?;
    let mut answer = Vec::new();
    timeout(Duration::from_secs(30), stream.read_to_end(&mut answer))
        .await
        .ok();
    (!answer.is_empty()).then_some(answer)
}

fn probe(port: u16) -> Probe {
    let stop = Arc::new(AtomicBool::new(false));
    let (refused, slowest) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (s, r, l) = (stop.clone(), refused.clone(), slowest.clone());
    let task = tokio::spawn(async move {
        while !s.load(Ordering::Relaxed) {
            let began = Instant::now();
            // A connection that the old server accepted and closed before it answered is
            // retried once, as an HTTP client does for a GET.
            let mut answer = ask(port).await;
            if answer.is_none() {
                answer = ask(port).await;
            }
            // Every answer must be a success, not only every connect.
            match answer {
                Some(bytes) if bytes.starts_with(b"HTTP/1.1 200") => {
                    l.fetch_max(began.elapsed().as_millis() as u64, Ordering::Relaxed);
                }
                _ => {
                    r.fetch_add(1, Ordering::Relaxed);
                }
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    });
    Probe {
        stop,
        refused,
        slowest,
        task,
    }
}

impl Probe {
    async fn finish(self) -> (u64, u64) {
        self.stop.store(true, Ordering::Relaxed);
        self.task.await.unwrap();
        (
            self.refused.load(Ordering::Relaxed),
            self.slowest.load(Ordering::Relaxed),
        )
    }
}

/// Stops whatever listens on these ports when a test ends or fails: the resumed old build
/// and the standby are not children of the test.
struct Listeners(Vec<u16>);

impl Drop for Listeners {
    fn drop(&mut self) {
        for port in &self.0 {
            let found = std::process::Command::new("/usr/sbin/lsof")
                .args(["-nP", "-t", &format!("-iTCP:{port}"), "-sTCP:LISTEN"])
                .output();
            for pid in String::from_utf8_lossy(&found.map(|out| out.stdout).unwrap_or_default())
                .split_whitespace()
            {
                if pid != std::process::id().to_string() {
                    std::process::Command::new("/bin/kill")
                        .args(["-9", pid])
                        .status()
                        .ok();
                }
            }
        }
    }
}

async fn log_has(site: &Site, name: &str, text: &str) -> bool {
    fs::read_to_string(site.home.join(format!("{name}.log"))).is_ok_and(|log| log.contains(text))
}

#[tokio::test]
async fn a_new_server_takes_the_port_and_the_old_one_drains_its_stream() {
    let site = site().await;
    let mut old = server(&site, "old", &[]);
    let port = served_port(&mut old).await;
    let _listeners = Listeners(vec![port]);
    let stream = open_stream(port).await;
    let probe = probe(port);

    let mut new = server(&site, "new", &[handover::TAKEOVER_ARG]);
    // The new server prints its line only when it serves.
    let new_port = served_port(&mut new).await;
    assert_eq!(new_port, port, "the new server must serve the old port");
    stream_ends(stream).await;
    let status = timeout(Duration::from_secs(30), old.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "old server: {status}");
    assert_eq!(serving_pid(port).await, new.id().unwrap());
    let (refused, slowest) = probe.finish().await;
    eprintln!("handover: failed requests {refused}, slowest answer {slowest} ms");
    assert_eq!(refused, 0, "a request failed during the handover");
    // The new server serves the resumed stream.
    drop(open_stream(port).await);
    new.kill().await.unwrap();
}

/// A new server that is gone before `DRAINED` (B1) or after it but before it opens the
/// state: the old build serves again on the same port.
#[tokio::test]
async fn a_new_server_that_dies_before_it_opens_the_state_gives_the_socket_back() {
    let site = site().await;
    let mut old = server(&site, "old", &[]);
    let port = served_port(&mut old).await;
    let _listeners = Listeners(vec![port]);
    let old_pid = old.id().unwrap();
    let stream = open_stream(port).await;
    let probe = probe(port);

    // The new server gets the socket, then ends the line at once: it died before it
    // connected anywhere else.
    let state = site.state.clone();
    let (listener, go) = tokio::task::spawn_blocking(move || handover::request(&state))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(listener.local_addr().unwrap().port(), port);
    drop(go);
    stream_ends(stream).await;
    // On macOS, closing this copy of the socket resets the connections that wait in its
    // queue. A real new server that dies closes its copy too; the test keeps it open so
    // that the probe checks the rollback itself.
    std::mem::forget(listener);

    let status = timeout(Duration::from_secs(30), old.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "old server: {status}");
    let resumed = serving_pid(port).await;
    assert_ne!(resumed, old_pid, "the old build resumes in a new process");
    drop(open_stream(port).await);
    let (refused, slowest) = probe.finish().await;
    eprintln!("rollback: failed requests {refused}, slowest answer {slowest} ms");
    assert_eq!(refused, 0, "a request failed during the rollback");
}

/// After `OPEN` the state may have migrated: the standby must not start the old build.
#[tokio::test]
async fn a_new_server_that_fails_after_it_opens_the_state_fails_forward() {
    let site = site().await;
    let mut old = server(&site, "old", &[]);
    let port = served_port(&mut old).await;
    let _listeners = Listeners(vec![port]);
    let state = site.state.clone();
    let mut taken =
        tokio::task::spawn_blocking(move || handover::take(&state, Duration::from_secs(20)))
            .await
            .unwrap()
            .unwrap();
    let mut line = taken.line.take().expect("a standby line");
    line.open().unwrap();
    line.close();
    drop(taken);
    // The standby exits: nothing listens, and the old build does not come back.
    timeout(Duration::from_secs(30), async {
        while TcpStream::connect(("127.0.0.1", port)).await.is_ok() {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the port stayed open after a failure past OPEN");
    // The standby says why it left the port, and the old build did not come back.
    timeout(Duration::from_secs(30), async {
        while !log_has(&site, "old", "failed after it opened the state").await {
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("the standby did not report a failure forward");
    assert!(!log_has(&site, "old", "the old build resumes").await);
    assert!(TcpStream::connect(("127.0.0.1", port)).await.is_err());
}

/// The old server ends without a standby (B2): the new server holds the socket and serves.
#[tokio::test]
async fn a_new_server_serves_when_the_old_one_is_killed_during_the_drain() {
    let site = site().await;
    let mut old = server(&site, "old", &[]);
    let port = served_port(&mut old).await;
    let _listeners = Listeners(vec![port]);
    let mut new = server(&site, "new", &[handover::TAKEOVER_ARG]);
    timeout(Duration::from_secs(30), async {
        while !log_has(&site, "old", "requested by a live handover").await {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the old server did not start its drain");
    old.kill().await.unwrap();
    let new_port = served_port(&mut new).await;
    assert_eq!(new_port, port);
    assert_eq!(serving_pid(port).await, new.id().unwrap());
    new.kill().await.unwrap();
}
