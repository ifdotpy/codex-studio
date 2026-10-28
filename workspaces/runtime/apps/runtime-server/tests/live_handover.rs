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

/// Connects every few milliseconds. `refused` counts connections that failed; `slowest`
/// is the longest connect-to-answer time in milliseconds.
struct Probe {
    stop: Arc<AtomicBool>,
    refused: Arc<AtomicU64>,
    slowest: Arc<AtomicU64>,
    task: tokio::task::JoinHandle<()>,
}

fn probe(port: u16) -> Probe {
    let stop = Arc::new(AtomicBool::new(false));
    let (refused, slowest) = (Arc::new(AtomicU64::new(0)), Arc::new(AtomicU64::new(0)));
    let (s, r, l) = (stop.clone(), refused.clone(), slowest.clone());
    let task = tokio::spawn(async move {
        while !s.load(Ordering::Relaxed) {
            let began = Instant::now();
            match TcpStream::connect(("127.0.0.1", port)).await {
                Err(_) => {
                    r.fetch_add(1, Ordering::Relaxed);
                }
                Ok(mut stream) => {
                    let sent = stream
                        .write_all(
                            b"GET /api/desktop HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n",
                        )
                        .await;
                    let mut sink = Vec::new();
                    let _ = timeout(Duration::from_secs(30), stream.read_to_end(&mut sink)).await;
                    if sent.is_ok() {
                        l.fetch_max(began.elapsed().as_millis() as u64, Ordering::Relaxed);
                    }
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

#[tokio::test]
async fn a_new_server_takes_the_port_and_the_old_one_drains_its_stream() {
    let site = site().await;
    let mut old = server(&site, "old", &[]);
    let port = served_port(&mut old).await;
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
    eprintln!("handover: refused connections {refused}, slowest answer {slowest} ms");
    assert_eq!(refused, 0, "the port refused a connection");
    // The new server serves the resumed stream.
    drop(open_stream(port).await);
    new.kill().await.unwrap();
}

#[tokio::test]
async fn a_failed_new_server_gives_the_socket_back_and_the_old_build_serves_again() {
    let site = site().await;
    let mut old = server(&site, "old", &[]);
    let port = served_port(&mut old).await;
    let old_pid = old.id().unwrap();
    let stream = open_stream(port).await;
    let probe = probe(port);

    // The new server takes the socket, then dies before it serves: no `READY`.
    let state = site.state.clone();
    let taken = tokio::task::spawn_blocking(move || handover::take(&state))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.listener.local_addr().unwrap().port(), port);
    stream_ends(stream).await;
    drop(taken);

    // The old build resumes under a new PID; its old process ended after the drain.
    let status = timeout(Duration::from_secs(30), old.wait())
        .await
        .unwrap()
        .unwrap();
    assert!(status.success(), "old server: {status}");
    let resumed = serving_pid(port).await;
    assert_ne!(resumed, old_pid, "the old build resumes in a new process");
    drop(open_stream(port).await);
    let (refused, slowest) = probe.finish().await;
    eprintln!("rollback: refused connections {refused}, slowest answer {slowest} ms");
    assert_eq!(refused, 0, "the port refused a connection");
    // Stop the resumed server (not our child): SIGTERM through its PID.
    let _ = std::process::Command::new("/bin/kill")
        .arg(resumed.to_string())
        .status();
}
