//! Owns the `studio-peer-link` child used by the paired-server QUIC adapter.

use std::io;
use std::net::Ipv4Addr;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use studio_runtime_services::remote_transport::peer_link_config_line;
use studio_runtime_store::{ObservedTable, RuntimeStore, StoreError};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const LINK_PORT: u16 = 4621;
const START_TIMEOUT: Duration = Duration::from_secs(5);
const CHILD_STOP_TIMEOUT: Duration = Duration::from_secs(3);
const MAX_READY_LINE: usize = 4096;
const FIRST_RETRY_DELAY: Duration = Duration::from_millis(250);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const LINK_HEALTHY_RESET_AFTER: Duration = Duration::from_secs(60);

pub struct PeerLinkRuntime {
    stop: CancellationToken,
    task: Option<JoinHandle<()>>,
}

struct RunningLink {
    child: Child,
    stdin: ChildStdin,
    peers: Vec<String>,
    advert: String,
    started_at: tokio::time::Instant,
}

enum LinkEvent {
    Stop,
    Changed,
    Retry,
    Child(std::io::Result<std::process::ExitStatus>),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PeerSource {
    Snapshot,
    LastGood,
}

struct PeerLinkWorkState {
    refresh_snapshot: bool,
    retry_snapshot_at: Option<tokio::time::Instant>,
    retry_start_at: Option<tokio::time::Instant>,
    snapshot_backoff: RetryBackoff,
    start_backoff: RetryBackoff,
}

struct RetryBackoff {
    delay: Duration,
}

impl PeerLinkWorkState {
    fn new() -> Self {
        Self {
            refresh_snapshot: true,
            retry_snapshot_at: None,
            retry_start_at: None,
            snapshot_backoff: RetryBackoff::new(),
            start_backoff: RetryBackoff::new(),
        }
    }

    fn table_changed(&mut self) {
        self.refresh_snapshot = true;
        self.retry_snapshot_at = None;
        self.retry_start_at = None;
    }

    fn take_snapshot_if_due(&mut self, now: tokio::time::Instant) -> bool {
        if self.refresh_snapshot
            || self
                .retry_snapshot_at
                .is_some_and(|deadline| deadline <= now)
        {
            self.refresh_snapshot = false;
            self.retry_snapshot_at = None;
            true
        } else {
            false
        }
    }

    fn take_start_if_due(&mut self, now: tokio::time::Instant) -> bool {
        if self.retry_start_at.is_some_and(|deadline| deadline <= now) {
            self.retry_start_at = None;
            true
        } else {
            false
        }
    }

    fn snapshot_failed(&mut self, now: tokio::time::Instant) {
        self.retry_snapshot_at = Some(now + self.snapshot_backoff.next_delay());
    }

    fn snapshot_succeeded(&mut self) {
        self.retry_snapshot_at = None;
        self.snapshot_backoff.reset();
    }

    fn start_failed(&mut self, now: tokio::time::Instant) {
        self.retry_start_at = Some(now + self.start_backoff.next_delay());
    }

    fn start_succeeded(&mut self) {
        self.retry_start_at = None;
    }

    fn child_exited(&mut self, now: tokio::time::Instant, uptime: Duration) {
        if uptime >= LINK_HEALTHY_RESET_AFTER {
            self.start_backoff.reset();
        }
        self.start_failed(now);
    }

    fn next_deadline(&self) -> Option<tokio::time::Instant> {
        match (self.retry_snapshot_at, self.retry_start_at) {
            (Some(snapshot), Some(start)) => Some(snapshot.min(start)),
            (Some(snapshot), None) => Some(snapshot),
            (None, Some(start)) => Some(start),
            (None, None) => None,
        }
    }
}

impl RetryBackoff {
    fn new() -> Self {
        Self {
            delay: FIRST_RETRY_DELAY,
        }
    }

    fn next_delay(&mut self) -> Duration {
        let delay = self.delay;
        self.delay = next_retry_delay(self.delay);
        delay
    }

    fn reset(&mut self) {
        self.delay = FIRST_RETRY_DELAY;
    }
}

#[derive(Serialize)]
struct PeerUpdate<'a> {
    peers: &'a [String],
}

#[derive(Deserialize)]
struct PeerLinkReady {
    ready: bool,
    port: u16,
}

impl PeerLinkReady {
    fn is_valid(&self) -> bool {
        self.ready && self.port > 0
    }
}

impl PeerLinkRuntime {
    pub fn start(
        store: Arc<RuntimeStore>,
        backend_port: u16,
        public_origin: String,
        state_root: PathBuf,
        advert: Arc<RwLock<Option<String>>>,
    ) -> Result<Self, StoreError> {
        let (changed, changes) = mpsc::channel(1);
        // Subscribe before the first snapshot so a commit between registration
        // and the read remains queued for a second, authoritative snapshot.
        store.add_table_change_observer(ObservedTable::AccessClients, move || {
            match changed.try_send(()) {
                Ok(()) | Err(mpsc::error::TrySendError::Full(())) => {}
                Err(mpsc::error::TrySendError::Closed(())) => {}
            }
        })?;
        let stop = CancellationToken::new();
        let task_stop = stop.clone();
        let task_advert = Arc::clone(&advert);
        let task = tokio::spawn(run(
            store,
            backend_port,
            public_origin,
            state_root,
            task_advert,
            changes,
            task_stop,
        ));
        Ok(Self {
            stop,
            task: Some(task),
        })
    }

    pub async fn shutdown(&mut self) {
        self.stop.cancel();
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

impl Drop for PeerLinkRuntime {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

async fn run(
    store: Arc<RuntimeStore>,
    backend_port: u16,
    public_origin: String,
    state_root: PathBuf,
    advert: Arc<RwLock<Option<String>>>,
    mut changes: mpsc::Receiver<()>,
    stopped: CancellationToken,
) {
    let forwarded_host = forwarded_host(&public_origin);
    let socket = state_root.join("multi-server").join("peer-link.sock");
    let mut running: Option<RunningLink> = None;
    let mut last_peers: Option<Vec<String>> = None;
    let mut work = PeerLinkWorkState::new();

    loop {
        let now = tokio::time::Instant::now();
        let read_snapshot = work.take_snapshot_if_due(now);
        let retry_start = work.take_start_if_due(now);
        let mut should_start = retry_start;

        if read_snapshot || retry_start {
            let peer_source = if read_snapshot {
                PeerSource::Snapshot
            } else {
                PeerSource::LastGood
            };
            let peers_result = tokio::select! {
                _ = stopped.cancelled() => break,
                result = peers_for_source(
                    peer_source,
                    !forwarded_host.is_empty(),
                    last_peers.as_deref(),
                    || paired_server_keys(Arc::clone(&store)),
                ) => result,
            };
            match peers_result {
                Ok(None) => {
                    // Unsupported or missing public origin disables the link for
                    // this process lifetime; no database retry can change it.
                    publish(&advert, None);
                    stopped.cancelled().await;
                    break;
                }
                Ok(Some(peers)) => {
                    if read_snapshot {
                        work.snapshot_succeeded();
                    }
                    let peers_changed = last_peers.as_ref() != Some(&peers);
                    last_peers = Some(peers.clone());
                    let link_failed = if let Some(link) = running.as_mut() {
                        peers_changed
                            && !matches!(
                                tokio::time::timeout(START_TIMEOUT, update_peers(link, &peers))
                                    .await,
                                Ok(Ok(()))
                            )
                    } else {
                        false
                    };

                    if link_failed {
                        if let Some(link) = running.take() {
                            stop_link(link).await;
                        }
                        publish(&advert, None);
                    }
                    should_start |= running.is_none();
                }
                Err(error) => {
                    if read_snapshot {
                        // A failed read does not mean that every peer was revoked.
                        // Keep the last published configuration and retry with backoff.
                        eprintln!("peer-link paired-server snapshot failed: {error}");
                    } else {
                        eprintln!("peer-link restart snapshot is unavailable: {error}");
                    }
                    work.snapshot_failed(tokio::time::Instant::now());
                }
            }
        }

        if running.is_none()
            && should_start
            && let Some(peers) = last_peers.as_deref()
        {
            // Startup has bounded child handshakes and owns explicit cleanup.
            let started = start_link(
                &state_root,
                &socket,
                &forwarded_host,
                backend_port,
                peers,
                &stopped,
            )
            .await;
            match started {
                Some(link) => {
                    publish(&advert, Some(link.advert.clone()));
                    running = Some(link);
                    work.start_succeeded();
                }
                None => {
                    if stopped.is_cancelled() {
                        break;
                    }
                    work.start_failed(tokio::time::Instant::now());
                }
            }
        }

        let next_deadline = work.next_deadline();
        if running.is_some() {
            let started_at = running
                .as_ref()
                .expect("running peer link was checked")
                .started_at;
            let event = wait_for_link_event(
                running.as_mut().expect("running peer link was checked"),
                &mut changes,
                &stopped,
                next_deadline,
            )
            .await;
            match event {
                LinkEvent::Stop => break,
                LinkEvent::Changed => work.table_changed(),
                LinkEvent::Retry => {}
                LinkEvent::Child(Ok(status)) => {
                    eprintln!("peer-link child exited with {status}");
                    let uptime = started_at.elapsed();
                    running = None;
                    publish(&advert, None);
                    work.child_exited(tokio::time::Instant::now(), uptime);
                }
                LinkEvent::Child(Err(error)) => {
                    eprintln!("peer-link child wait failed: {error}");
                    let uptime = started_at.elapsed();
                    if let Some(link) = running.take() {
                        stop_link(link).await;
                    }
                    publish(&advert, None);
                    work.child_exited(tokio::time::Instant::now(), uptime);
                }
            }
        } else if let Some(deadline) = next_deadline {
            tokio::select! {
                _ = stopped.cancelled() => break,
                event = changes.recv() => {
                    if event.is_none() {
                        break;
                    }
                    work.table_changed();
                },
                _ = tokio::time::sleep_until(deadline) => {},
            }
        } else {
            tokio::select! {
                _ = stopped.cancelled() => break,
                event = changes.recv() => {
                    if event.is_none() {
                        break;
                    }
                    work.table_changed();
                },
            }
        }
    }

    if let Some(link) = running.take() {
        stop_link(link).await;
    }
    publish(&advert, None);
}

async fn peers_for_source<F, Fut>(
    source: PeerSource,
    enabled: bool,
    last_peers: Option<&[String]>,
    read_snapshot: F,
) -> Result<Option<Vec<String>>, String>
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = Result<Vec<String>, String>>,
{
    if !enabled {
        return Ok(None);
    }
    match source {
        PeerSource::Snapshot => read_snapshot().await.map(Some),
        PeerSource::LastGood => last_peers
            .map(|peers| Some(peers.to_vec()))
            .ok_or_else(|| "peer-link restart requested without a snapshot".to_owned()),
    }
}

fn forwarded_host(public_origin: &str) -> String {
    public_origin
        .strip_prefix("https://")
        .and_then(|value| value.split('/').next())
        .unwrap_or_default()
        .to_owned()
}

async fn wait_for_link_event(
    link: &mut RunningLink,
    changes: &mut mpsc::Receiver<()>,
    stopped: &CancellationToken,
    retry_at: Option<tokio::time::Instant>,
) -> LinkEvent {
    let wait = link.child.wait();
    tokio::pin!(wait);
    let retry = retry_at
        .unwrap_or_else(|| tokio::time::Instant::now() + Duration::from_secs(365 * 24 * 60 * 60));
    tokio::select! {
        _ = stopped.cancelled() => LinkEvent::Stop,
        event = changes.recv() => match event {
            Some(()) => LinkEvent::Changed,
            None => LinkEvent::Stop,
        },
        _ = tokio::time::sleep_until(retry), if retry_at.is_some() => LinkEvent::Retry,
        status = &mut wait => LinkEvent::Child(status),
    }
}

fn next_retry_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RETRY_DELAY)
}

fn publish(advert: &RwLock<Option<String>>, value: Option<String>) {
    if let Ok(mut current) = advert.write() {
        *current = value;
    }
}

async fn update_peers(link: &mut RunningLink, peers: &[String]) -> std::io::Result<()> {
    let mut update = serde_json::to_vec(&PeerUpdate { peers }).map_err(std::io::Error::other)?;
    update.push(b'\n');
    link.stdin.write_all(&update).await?;
    link.stdin.flush().await?;
    link.peers = peers.to_vec();
    Ok(())
}

async fn paired_server_keys(store: Arc<RuntimeStore>) -> Result<Vec<String>, String> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/peer-link-snapshot",
        move || {
            let connection = store.read().map_err(|error| error.to_string())?;
            let mut statement = connection
                .prepare_cached(
                    "SELECT json_extract(record,'$.publicKey') FROM runtime_access_clients \
                     WHERE json_extract(record,'$.kind')='server' \
                       AND json_extract(record,'$.status')='paired' \
                       AND COALESCE(json_extract(record,'$.revoked'),0)=0 \
                     ORDER BY id",
                )
                .map_err(|error| error.to_string())?;
            let keys = statement
                .query_map([], |row| row.get::<_, String>(0))
                .map_err(|error| error.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|error| error.to_string())?;
            Ok(keys)
        },
    )
    .await
    .map_err(|error| error.to_string())?
}

async fn tailscale_ipv4() -> Option<Ipv4Addr> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/peer-link-tailscale",
        tailscale_ipv4_blocking,
    )
    .await
    .ok()
    .flatten()
}

fn tailscale_ipv4_blocking() -> Option<Ipv4Addr> {
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

async fn start_link(
    state_root: &Path,
    socket: &Path,
    forwarded_host: &str,
    backend_port: u16,
    peers: &[String],
    stopped: &CancellationToken,
) -> Option<RunningLink> {
    if forwarded_host.is_empty() {
        return None;
    }
    let address = tokio::select! {
        _ = stopped.cancelled() => return None,
        address = tailscale_ipv4() => address?,
    };
    let socket_parent = socket.parent()?.to_path_buf();
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/peer-link-socket-directory",
        move || std::fs::create_dir_all(socket_parent),
    )
    .await
    .ok()?
    .ok()?;
    let executable = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/peer-link-binary-search",
        peer_link_binary,
    )
    .await
    .ok()??;
    for port in [LINK_PORT, 0] {
        if stopped.is_cancelled() {
            return None;
        }
        let listen = format!("{address}:{port}");
        let config_state_root = state_root.to_path_buf();
        let config_socket = socket.to_path_buf();
        let config_peers = peers.to_vec();
        let config_forwarded_host = forwarded_host.to_owned();
        let config = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/peer-link-config",
            move || {
                peer_link_config_line(
                    &config_state_root,
                    &listen,
                    &config_peers,
                    backend_port,
                    &config_forwarded_host,
                    &config_socket,
                )
            },
        )
        .await
        .ok()?
        .ok()?;
        let mut child = match Command::new(&executable)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
        {
            Ok(child) => child,
            Err(_) => continue,
        };
        let Some(mut stdin) = child.stdin.take() else {
            stop_child(child).await;
            continue;
        };
        let Some(stdout) = child.stdout.take() else {
            drop(stdin);
            stop_child(child).await;
            continue;
        };
        let write_config = async {
            stdin.write_all(config.as_bytes()).await?;
            stdin.write_all(b"\n").await?;
            stdin.flush().await
        };
        let write_result = tokio::select! {
            _ = stopped.cancelled() => None,
            result = tokio::time::timeout(START_TIMEOUT, write_config) => Some(result),
        };
        let stopped_during_write = write_result.is_none();
        if !matches!(&write_result, Some(Ok(Ok(())))) {
            drop(stdin);
            stop_child(child).await;
            if stopped_during_write {
                return None;
            }
            continue;
        }

        let Some((child, stdin, ready)) = wait_ready_or_cancel(child, stdin, stdout, stopped).await
        else {
            if stopped.is_cancelled() {
                return None;
            }
            continue;
        };
        return Some(RunningLink {
            child,
            stdin,
            peers: peers.to_vec(),
            advert: format!("{address}:{}", ready.port),
            started_at: tokio::time::Instant::now(),
        });
    }
    None
}

async fn wait_ready_or_cancel(
    child: Child,
    stdin: ChildStdin,
    stdout: tokio::process::ChildStdout,
    stopped: &CancellationToken,
) -> Option<(Child, ChildStdin, PeerLinkReady)> {
    let ready_result = tokio::select! {
        _ = stopped.cancelled() => None,
        result = tokio::time::timeout(START_TIMEOUT, read_ready_line(stdout)) => Some(result),
    };
    let ready_line = match ready_result {
        Some(Ok(Ok(Some(line)))) => line,
        _ => {
            drop(stdin);
            stop_child(child).await;
            return None;
        }
    };
    let ready: PeerLinkReady = match serde_json::from_str::<PeerLinkReady>(&ready_line) {
        Ok(ready) if ready.is_valid() => ready,
        _ => {
            drop(stdin);
            stop_child(child).await;
            return None;
        }
    };
    Some((child, stdin, ready))
}

async fn read_ready_line(stdout: tokio::process::ChildStdout) -> io::Result<Option<String>> {
    let mut reader = BufReader::new(stdout);
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return if line.is_empty() {
                Ok(None)
            } else {
                String::from_utf8(line).map(Some).map_err(io::Error::other)
            };
        }
        let consumed = available
            .iter()
            .position(|byte| *byte == b'\n')
            .map_or(available.len(), |index| index + 1);
        if line.len().saturating_add(consumed) > MAX_READY_LINE {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "peer-link ready line exceeds the size limit",
            ));
        }
        let has_newline = available.get(consumed.saturating_sub(1)) == Some(&b'\n');
        line.extend_from_slice(&available[..consumed]);
        reader.consume(consumed);
        if has_newline {
            return String::from_utf8(line).map(Some).map_err(io::Error::other);
        }
    }
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

async fn stop_link(link: RunningLink) {
    drop(link.stdin);
    stop_child(link.child).await;
}

async fn stop_child(mut child: Child) {
    match tokio::time::timeout(CHILD_STOP_TIMEOUT, child.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(_)) | Err(_) => {
            let _ = child.kill().await;
            let _ = child.wait().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        FIRST_RETRY_DELAY, LINK_HEALTHY_RESET_AFTER, MAX_RETRY_DELAY, PeerLinkReady,
        PeerLinkWorkState, PeerSource, forwarded_host, next_retry_delay, peers_for_source,
        wait_ready_or_cancel,
    };
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn retry_backoff_is_bounded_and_starts_short() {
        assert_eq!(FIRST_RETRY_DELAY, Duration::from_millis(250));
        let mut delay = FIRST_RETRY_DELAY;
        for _ in 0..16 {
            delay = next_retry_delay(delay);
        }
        assert_eq!(delay, MAX_RETRY_DELAY);
    }

    #[test]
    fn ready_contract_checks_required_typed_fields_and_accepts_additions() {
        let ready: PeerLinkReady =
            serde_json::from_str(r#"{"ready":true,"port":4621,"protocolVersion":2}"#)
                .expect("additive readiness fields remain compatible");
        assert!(ready.is_valid());
        let unready: PeerLinkReady =
            serde_json::from_str(r#"{"ready":false,"port":4621}"#).unwrap();
        assert!(!unready.is_valid());
        assert!(serde_json::from_str::<PeerLinkReady>(r#"{"ready":"yes","port":4621}"#).is_err());
    }

    #[tokio::test]
    async fn disabled_origin_skips_peer_snapshot_reads() {
        let reads = Arc::new(AtomicUsize::new(0));
        let read_counter = Arc::clone(&reads);
        let host = forwarded_host("http://example.test");
        assert!(host.is_empty());
        let peers = peers_for_source(
            PeerSource::Snapshot,
            !host.is_empty(),
            None,
            move || async move {
                read_counter.fetch_add(1, Ordering::Relaxed);
                Ok(Vec::new())
            },
        )
        .await
        .unwrap();

        assert_eq!(peers, None);
        assert_eq!(reads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn child_restart_retry_reuses_last_snapshot_without_database_read() {
        let reads = Arc::new(AtomicUsize::new(0));
        let read_counter = Arc::clone(&reads);
        let last_peers = vec!["paired-key".to_owned()];
        let peers = peers_for_source(
            PeerSource::LastGood,
            true,
            Some(&last_peers),
            move || async move {
                read_counter.fetch_add(1, Ordering::Relaxed);
                Ok(Vec::new())
            },
        )
        .await
        .unwrap();

        assert_eq!(peers, Some(last_peers));
        assert_eq!(reads.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn snapshot_retry_retries_the_database_read() {
        let reads = Arc::new(AtomicUsize::new(0));
        let read_counter = Arc::clone(&reads);
        let last_good = vec!["last-good-key".to_owned()];
        let peers = peers_for_source(
            PeerSource::Snapshot,
            true,
            Some(&last_good),
            move || async move {
                read_counter.fetch_add(1, Ordering::Relaxed);
                Err("read failed".to_owned())
            },
        )
        .await;

        assert_eq!(peers, Err("read failed".to_owned()));
        assert_eq!(reads.load(Ordering::Relaxed), 1);
    }

    #[test]
    fn short_lived_ready_child_keeps_start_failure_backoff() {
        let start = tokio::time::Instant::now();
        let mut work = PeerLinkWorkState::new();
        work.start_failed(start);
        assert_eq!(work.retry_start_at.unwrap() - start, FIRST_RETRY_DELAY);

        let first_retry = start + FIRST_RETRY_DELAY;
        assert!(work.take_start_if_due(first_retry));
        work.start_succeeded();
        let quick_exit = first_retry + Duration::from_millis(10);
        work.child_exited(quick_exit, Duration::from_millis(10));
        assert_eq!(
            work.retry_start_at.unwrap() - quick_exit,
            FIRST_RETRY_DELAY * 2
        );

        work.child_exited(
            quick_exit + LINK_HEALTHY_RESET_AFTER,
            LINK_HEALTHY_RESET_AFTER,
        );
        assert_eq!(
            work.retry_start_at.unwrap() - (quick_exit + LINK_HEALTHY_RESET_AFTER),
            FIRST_RETRY_DELAY
        );
    }

    #[test]
    fn child_exit_and_successful_restart_preserve_pending_snapshot_retry() {
        let start = tokio::time::Instant::now();
        let mut work = PeerLinkWorkState::new();
        work.refresh_snapshot = false;
        work.snapshot_failed(start);
        work.snapshot_failed(start);
        work.snapshot_failed(start);
        let snapshot_deadline = work.retry_snapshot_at.unwrap();
        assert_eq!(snapshot_deadline - start, Duration::from_secs(1));

        let child_exit = start + Duration::from_millis(10);
        work.child_exited(child_exit, Duration::from_millis(10));
        let start_deadline = work.retry_start_at.unwrap();
        assert!(work.take_start_if_due(start_deadline));
        work.start_succeeded();

        assert_eq!(work.retry_snapshot_at, Some(snapshot_deadline));
        assert!(!work.take_snapshot_if_due(start_deadline));
        assert!(work.take_snapshot_if_due(snapshot_deadline));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn startup_cancellation_closes_stdin_and_reaps_the_child() {
        use std::process::Stdio;
        use tokio::process::Command;
        use tokio_util::sync::CancellationToken;

        let directory = tempfile::tempdir().unwrap();
        let marker = directory.path().join("child-stopped");
        let mut child = Command::new("sh")
            .args([
                "-c",
                "read line || { printf stopped > \"$1\"; exit 0; }; sleep 30",
                "sh",
            ])
            .arg(&marker)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        let stdout = child.stdout.take().unwrap();
        let stopped = CancellationToken::new();
        let task_stopped = stopped.clone();
        let startup =
            tokio::spawn(
                async move { wait_ready_or_cancel(child, stdin, stdout, &task_stopped).await },
            );

        tokio::task::yield_now().await;
        stopped.cancel();
        assert!(startup.await.unwrap().is_none());
        assert_eq!(std::fs::read_to_string(marker).unwrap(), "stopped");
    }
}
