//! Owns the `studio-peer-link` child used by the paired-server QUIC adapter.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, RwLock};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use studio_runtime_services::remote_transport::peer_link_config_line;
use studio_runtime_store::RuntimeStore;

const LINK_PORT: u16 = 4621;
const REFRESH_INTERVAL: Duration = Duration::from_secs(30);
const START_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_STOP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_READY_LINE: usize = 4096;

pub struct PeerLinkRuntime {
    advert: Arc<RwLock<Option<String>>>,
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

struct RunningLink {
    child: Child,
    stdin: ChildStdin,
    peers: Vec<String>,
    advert: String,
}

impl PeerLinkRuntime {
    pub fn start(
        store: Arc<RuntimeStore>,
        backend_port: u16,
        public_origin: String,
        state_root: PathBuf,
    ) -> Self {
        let advert = Arc::new(RwLock::new(None));
        let (stop, receiver) = mpsc::channel();
        let thread_advert = Arc::clone(&advert);
        let thread = thread::Builder::new()
            .name("studio-peer-link-owner".to_owned())
            .spawn(move || {
                run(
                    store,
                    backend_port,
                    public_origin,
                    state_root,
                    thread_advert,
                    receiver,
                )
            })
            .ok();
        Self {
            advert,
            stop: Some(stop),
            thread,
        }
    }

    pub fn advert_handle(&self) -> Arc<RwLock<Option<String>>> {
        Arc::clone(&self.advert)
    }

    pub fn shutdown(&mut self) {
        if let Some(stop) = self.stop.take() {
            match stop.send(()) {
                Ok(()) | Err(_) => {}
            }
        }
        if let Some(thread) = self.thread.take() {
            drop(thread.join());
        }
    }
}

impl Drop for PeerLinkRuntime {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn run(
    store: Arc<RuntimeStore>,
    backend_port: u16,
    public_origin: String,
    state_root: PathBuf,
    advert: Arc<RwLock<Option<String>>>,
    stop: Receiver<()>,
) {
    let forwarded_host = public_origin
        .strip_prefix("https://")
        .and_then(|value| value.split('/').next())
        .unwrap_or_default()
        .to_owned();
    let socket = state_root.join("multi-server").join("peer-link.sock");
    let mut running: Option<RunningLink> = None;
    loop {
        let keys = paired_server_keys(&store).unwrap_or_default();
        let mut restart = false;
        if let Some(link) = running.as_mut() {
            match link.child.try_wait() {
                Ok(Some(_)) | Err(_) => restart = true,
                Ok(None) if link.peers != keys => {
                    if update_peers(link, &keys).is_err() {
                        restart = true;
                    }
                }
                Ok(None) => {}
            }
        }
        if restart {
            if let Some(link) = running.take() {
                stop_link(link);
            }
            publish(&advert, None);
        }
        if running.is_none()
            && let Some(link) =
                start_link(&state_root, &socket, &forwarded_host, backend_port, &keys)
        {
            publish(&advert, Some(link.advert.clone()));
            running = Some(link);
        }
        match stop.recv_timeout(REFRESH_INTERVAL) {
            Ok(()) | Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(mpsc::RecvTimeoutError::Timeout) => {}
        }
    }
    if let Some(link) = running.take() {
        stop_link(link);
    }
    publish(&advert, None);
}

fn publish(advert: &RwLock<Option<String>>, value: Option<String>) {
    if let Ok(mut current) = advert.write() {
        *current = value;
    }
}

fn update_peers(link: &mut RunningLink, peers: &[String]) -> std::io::Result<()> {
    serde_json::to_writer(&mut link.stdin, &serde_json::json!({"peers":peers}))?;
    link.stdin.write_all(b"\n")?;
    link.stdin.flush()?;
    link.peers = peers.to_vec();
    Ok(())
}

fn paired_server_keys(store: &RuntimeStore) -> Option<Vec<String>> {
    let connection = store.read().ok()?;
    let mut statement = connection
        .prepare_cached(
            "SELECT json_extract(record,'$.publicKey') FROM runtime_access_clients \
             WHERE json_extract(record,'$.kind')='server' \
               AND json_extract(record,'$.status')='paired' \
               AND COALESCE(json_extract(record,'$.revoked'),0)=0 \
             ORDER BY id",
        )
        .ok()?;
    let keys = statement
        .query_map([], |row| row.get::<_, String>(0))
        .ok()?
        .collect::<Result<Vec<_>, _>>()
        .ok()?;
    Some(keys)
}

fn tailscale_ipv4() -> Option<Ipv4Addr> {
    let status = crate::multi_server_owner::tailscale_json(&["status", "--json"]).ok()?;
    if status.get("BackendState")?.as_str()? != "Running" {
        return None;
    }
    status
        .get("Self")?
        .get("TailscaleIPs")?
        .as_array()?
        .iter()
        .filter_map(|value| value.as_str()?.parse::<Ipv4Addr>().ok())
        .find(|address| {
            studio_runtime_services::multi_server_auth::tailnet_address((*address).into())
        })
}

fn start_link(
    state_root: &Path,
    socket: &Path,
    forwarded_host: &str,
    backend_port: u16,
    peers: &[String],
) -> Option<RunningLink> {
    if forwarded_host.is_empty() {
        return None;
    }
    let address = tailscale_ipv4()?;
    std::fs::create_dir_all(socket.parent()?).ok()?;
    let executable = peer_link_binary()?;
    for port in [LINK_PORT, 0] {
        let listen = format!("{address}:{port}");
        let config = peer_link_config_line(
            state_root,
            &listen,
            peers,
            backend_port,
            forwarded_host,
            socket,
        )
        .ok()?;
        let mut child = Command::new(&executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .ok()?;
        let Some(mut stdin) = child.stdin.take() else {
            stop_child(child);
            continue;
        };
        let Some(stdout) = child.stdout.take() else {
            stop_child(child);
            continue;
        };
        if writeln!(stdin, "{config}")
            .and_then(|()| stdin.flush())
            .is_err()
        {
            drop(stdin);
            stop_child(child);
            continue;
        }
        let (sender, receiver) = mpsc::channel();
        let reader = thread::spawn(move || {
            let mut line = String::new();
            let result = BufReader::new(stdout)
                .take((MAX_READY_LINE + 1) as u64)
                .read_line(&mut line)
                .map(|_| line);
            drop(sender.send(result));
        });
        let ready_line = match receiver.recv_timeout(START_TIMEOUT) {
            Ok(Ok(line)) if line.len() <= MAX_READY_LINE => line,
            _ => {
                drop(stdin);
                stop_child(child);
                drop(reader.join());
                continue;
            }
        };
        drop(reader.join());
        let ready: serde_json::Value = match serde_json::from_str(&ready_line) {
            Ok(value) => value,
            Err(_) => {
                drop(stdin);
                stop_child(child);
                continue;
            }
        };
        let Some(port) = ready
            .get("port")
            .and_then(serde_json::Value::as_u64)
            .and_then(|value| u16::try_from(value).ok())
            .filter(|value| *value > 0)
        else {
            drop(stdin);
            stop_child(child);
            continue;
        };
        if ready.get("ready") != Some(&serde_json::Value::Bool(true)) {
            drop(stdin);
            stop_child(child);
            continue;
        }
        return Some(RunningLink {
            child,
            stdin,
            peers: peers.to_vec(),
            advert: format!("{address}:{port}"),
        });
    }
    None
}

fn peer_link_binary() -> Option<PathBuf> {
    let configured = std::env::var_os("STUDIO_PEER_LINK_BIN").map(PathBuf::from);
    let current = std::env::current_exe().ok();
    let mut candidates = Vec::new();
    if let Some(path) = configured {
        candidates.push(path);
    }
    if let Some(executable) = current
        && let Some(parent) = executable.parent()
    {
        candidates.push(parent.join("studio-peer-link"));
        for ancestor in parent.ancestors() {
            candidates.push(ancestor.join("target/release/studio-peer-link"));
        }
    }
    if let Some(path) = std::env::var_os("PATH") {
        candidates.extend(
            std::env::split_paths(&path).map(|directory| directory.join("studio-peer-link")),
        );
    }
    candidates.into_iter().find(|path| {
        path.is_file()
            && std::fs::metadata(path).is_ok_and(|metadata| {
                use std::os::unix::fs::PermissionsExt;
                metadata.permissions().mode() & 0o111 != 0
            })
    })
}

fn stop_link(link: RunningLink) {
    drop(link.stdin);
    stop_child(link.child);
}

fn stop_child(mut child: Child) {
    let deadline = Instant::now() + CHILD_STOP_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) | Err(_) => {
                drop(child.kill());
                drop(child.wait());
                return;
            }
        }
    }
}
