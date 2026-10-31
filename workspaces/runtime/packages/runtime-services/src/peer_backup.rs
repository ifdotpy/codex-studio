//! Backup of `canvas.sqlite3` to a paired server (docs/backup.md).
//!
//! The sending side takes the segments that `studio_runtime_store::wal_backup` writes at each
//! checkpoint and the base snapshot, and sends them as `backup.put` requests. The receiving
//! side stores them under `<state>/backup-received/<source server>/<generation>/`.
//! A `backup.put` carries one chunk at an offset. The receiver appends the chunk only when the
//! offset is the end of the file it holds and answers the length it holds, so a sender that
//! lost an answer, or a replayed request, continues at the right offset and nothing is written
//! twice.
//!
//! The snapshot is sent as zstd blocks. Settings are in `<state>/backup/settings.json`:
//! off by default.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::watch;

use studio_runtime_schema::persistence::{RemoteOutcome, RequestIdentity};
use studio_runtime_schema::shared_values::{
    BACKUP_CHUNK_BYTES, BACKUP_DEFAULT_RETAIN_GENERATIONS, BACKUP_MAX_RETAIN_GENERATIONS,
    BACKUP_PUT_ATTEMPTS, BACKUP_PUT_RETRY_SECONDS, BACKUP_RETRY_SECONDS,
    BACKUP_SNAPSHOT_BLOCK_BYTES,
};
use studio_runtime_store::RuntimeStore;
use studio_runtime_store::wal_backup::{SNAPSHOT_FILE, encode_snapshot_block, parse_segment_name};

use crate::atomic_file;
use crate::multi_server_orchestration::{HttpsPairedTransport, PairedTransport};

/// The orchestration action of one backup chunk.
pub const ACTION: &str = "backup.put";
/// Name of the snapshot in a `backup.put` payload.
const SNAPSHOT_NAME: &str = "snapshot";
const SETTINGS_FILE: &str = "backup/settings.json";
const RECEIVED_DIR: &str = "backup-received";
/// Seconds that one `backup.put` request waits.
const PUT_REQUEST: Duration = Duration::from_secs(60);
/// Wait between two tries of one `backup.put`; short in tests.
const PUT_RETRY_DELAY: Duration = if cfg!(test) {
    Duration::from_millis(10)
} else {
    Duration::from_secs(BACKUP_PUT_RETRY_SECONDS)
};
const INVALID_RECEIPT: &str = "The paired server returned an invalid receipt";

/// The owner's backup settings.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    /// Off until the owner turns it on.
    pub enabled: bool,
    /// The paired server that receives the backup.
    pub target: Option<String>,
    /// Generations that the paired server keeps for this server, newest first.
    pub retain_generations: u64,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            enabled: false,
            target: None,
            retain_generations: BACKUP_DEFAULT_RETAIN_GENERATIONS,
        }
    }
}

impl Settings {
    /// The settings of a state directory. A missing or unreadable file means off.
    pub fn read(root: &Path) -> Self {
        std::fs::read(root.join(SETTINGS_FILE))
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Check the values and replace the settings file.
    pub fn write(&self, root: &Path) -> Result<(), String> {
        if !(1..=BACKUP_MAX_RETAIN_GENERATIONS).contains(&self.retain_generations) {
            return Err(format!(
                "retainGenerations must be 1 to {BACKUP_MAX_RETAIN_GENERATIONS}"
            ));
        }
        if self.enabled && self.target.as_deref().is_none_or(str::is_empty) {
            return Err("a target server is required to turn the backup on".to_owned());
        }
        let path = root.join(SETTINGS_FILE);
        let bytes = serde_json::to_vec(self).map_err(|error| error.to_string())?;
        std::fs::create_dir_all(path.parent().ok_or("settings path")?)
            .and_then(|()| atomic_file::replace(&path, &bytes, None, true))
            .map_err(|error| error.to_string())
    }
}

// ---------------------------------------------------------------------------------------
// Receiving side
// ---------------------------------------------------------------------------------------

/// A refused request is not applied; a failure leaves the outcome unknown.
#[derive(Debug)]
pub enum ReceiveError {
    Refused(String),
    Failed(String),
}

fn refused(message: &str) -> ReceiveError {
    ReceiveError::Refused(message.to_owned())
}

fn failed(error: impl std::fmt::Display) -> ReceiveError {
    ReceiveError::Failed(error.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Put {
    generation: String,
    file: String,
    offset: u64,
    /// Base64 of the chunk.
    data: String,
    sha256: String,
    /// The chunk is the last one of the file.
    end: bool,
    retain: u64,
}

/// A name that is one path component of letters, digits, dot, dash or underscore.
fn plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'_'))
}

/// Store one chunk from the paired server `principal`. The answer is
/// `{"received": <bytes held>, "done": <file complete>}`.
pub fn receive(root: &Path, principal: &str, payload: &Value) -> Result<Value, ReceiveError> {
    let put: Put =
        serde_json::from_value(payload.clone()).map_err(|_| refused("Invalid backup request"))?;
    let segment = parse_segment_name(&put.file).is_some();
    if !plain_name(principal)
        || !plain_name(&put.generation)
        || !(segment || put.file == SNAPSHOT_NAME)
        || !(1..=BACKUP_MAX_RETAIN_GENERATIONS).contains(&put.retain)
    {
        return Err(refused("Invalid backup request"));
    }
    let data = base64::engine::general_purpose::STANDARD
        .decode(&put.data)
        .map_err(|_| refused("Invalid backup chunk"))?;
    if data.len() > BACKUP_CHUNK_BYTES
        || put.sha256 != data_encoding::HEXLOWER.encode(&Sha256::digest(&data))
    {
        return Err(refused("The backup chunk does not match its hash"));
    }
    let source = root.join(RECEIVED_DIR).join(principal);
    let folder = source.join(&put.generation);
    let name = if segment {
        put.file.as_str()
    } else {
        SNAPSHOT_FILE
    };
    let target = folder.join(name);
    if target.is_file() {
        let held = target.metadata().map_err(failed)?.len();
        return Ok(json!({"received": held, "done": true}));
    }
    if segment && !folder.join(SNAPSHOT_FILE).is_file() {
        return Ok(json!({"received": 0, "done": false, "noSnapshot": true}));
    }
    std::fs::create_dir_all(&folder).map_err(failed)?;
    let partial = folder.join(format!("{name}.part"));
    let held = partial.metadata().map_or(0, |metadata| metadata.len());
    if put.offset != held {
        return Ok(json!({"received": held, "done": false}));
    }
    {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&partial)
            .map_err(failed)?;
        file.write_all(&data).map_err(failed)?;
        if put.end {
            file.sync_all().map_err(failed)?;
        }
    }
    let received = held + data.len() as u64;
    if put.end {
        std::fs::rename(&partial, &target).map_err(failed)?;
        if !segment {
            prune(&source, usize::try_from(put.retain).unwrap_or(1));
        }
    }
    Ok(json!({"received": received, "done": put.end}))
}

/// Keep the newest `retain` generation folders of one source server. A generation name
/// starts with its start time, so the names sort by age.
fn prune(source: &Path, retain: usize) {
    let Ok(entries) = std::fs::read_dir(source) else {
        return;
    };
    let mut generations: Vec<PathBuf> = entries
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.is_dir())
        .collect();
    generations.sort();
    let drop_count = generations.len().saturating_sub(retain);
    for old in generations.into_iter().take(drop_count) {
        if let Err(error) = std::fs::remove_dir_all(&old) {
            eprintln!("backup: could not remove {}: {error}", old.display());
        }
    }
}

// ---------------------------------------------------------------------------------------
// Sending side
// ---------------------------------------------------------------------------------------

/// Why a pass stopped.
#[derive(Debug)]
enum Stop {
    /// The backend stops.
    Shutdown,
    /// The paired server lost the generation; the next pass starts a new one.
    Rebuild,
    /// The paired server or the files failed; the next pass tries again.
    Error(String),
}

fn error(message: impl std::fmt::Display) -> Stop {
    Stop::Error(message.to_string())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Reply {
    received: u64,
    done: bool,
    /// The paired server holds no snapshot of this generation.
    #[serde(default)]
    no_snapshot: bool,
}

/// The sender of one server. A task runs its passes one after another.
pub struct Sender {
    store: Arc<RuntimeStore>,
    transport: Arc<dyn PairedTransport>,
    stop: watch::Receiver<bool>,
}

impl Sender {
    pub fn new(
        store: Arc<RuntimeStore>,
        transport: Arc<dyn PairedTransport>,
        stop: watch::Receiver<bool>,
    ) -> Self {
        Self {
            store,
            transport,
            stop,
        }
    }

    pub fn installed(store: Arc<RuntimeStore>, stop: watch::Receiver<bool>) -> Self {
        let transport = Arc::new(HttpsPairedTransport::new(Arc::clone(&store)));
        Self::new(store, transport, stop)
    }

    /// Run passes until the backend stops. A pass ends when everything that exists is sent;
    /// the next one starts at a new segment, a settings change, or after a failure.
    pub async fn run(self) {
        let mut stop = self.stop.clone();
        loop {
            let outcome = self.pass().await;
            let backup = self.store.wal_backup();
            let mut retry = false;
            match outcome {
                Ok(()) => {}
                Err(Stop::Shutdown) => return,
                Err(Stop::Rebuild) => {
                    if let Some(backup) = &backup {
                        backup.break_generation("the paired server lost the snapshot");
                        backup.wake();
                    }
                }
                Err(Stop::Error(message)) => {
                    eprintln!("backup pass failed: {message}");
                    if let Some(backup) = &backup {
                        backup.set_error(Some(message));
                    }
                    retry = true;
                }
            }
            let on = Settings::read(self.store.root()).enabled;
            let segments = async {
                if let (Some(backup), true) = (&backup, on && !retry) {
                    backup.changed().await;
                } else {
                    std::future::pending::<()>().await;
                }
            };
            tokio::select! {
                _ = stop.wait_for(|stopped| *stopped) => return,
                () = segments => {}
                () = tokio::time::sleep(Duration::from_secs(BACKUP_RETRY_SECONDS)), if retry || !on => {}
            }
        }
    }

    fn check_stop(&self) -> Result<(), Stop> {
        if *self.stop.borrow() {
            Err(Stop::Shutdown)
        } else {
            Ok(())
        }
    }

    async fn pass(&self) -> Result<(), Stop> {
        let root = self.store.root().to_path_buf();
        let settings = Settings::read(&root);
        let (true, Some(target)) = (settings.enabled, settings.target.clone()) else {
            // Turned off: stop copying the WAL and drop the outbox.
            if let Some(backup) = self.store.wal_backup()
                && backup.status().broken.is_none()
                && backup.status().generation.is_some()
            {
                backup.break_generation("the backup is turned off");
            }
            return Ok(());
        };
        let backup = self.store.install_wal_backup().map_err(error)?;
        let status = backup.status();
        if status.generation.is_none() || status.broken.is_some() {
            self.store.begin_backup_generation().await.map_err(error)?;
        }
        let generation = backup
            .status()
            .generation
            .ok_or_else(|| error("no generation"))?;
        if !backup.status().snapshot_sent {
            // The read connection of the shipper keeps the copy consistent. A copy that
            // fails after the retries of each request starts again as a new generation,
            // because the compressed bytes sent so far cannot be made again.
            if let Err(stop) = self
                .send_snapshot(&target, &generation, backup.database_path(), &settings)
                .await
            {
                if !matches!(stop, Stop::Shutdown) {
                    backup.break_generation("the snapshot upload failed");
                }
                return Err(stop);
            }
            if !backup.end_snapshot() {
                return Err(error("the generation broke during the snapshot"));
            }
        }
        for segment in backup.segments() {
            self.check_stop()?;
            self.send_file(
                &target,
                &generation,
                &segment.name,
                &segment.path,
                &settings,
            )
            .await?;
            backup.ack(&segment);
        }
        backup.set_error(None);
        Ok(())
    }

    /// Send the database file as compressed blocks (`wal_backup::encode_snapshot_block`).
    async fn send_snapshot(
        &self,
        target: &str,
        generation: &str,
        path: &Path,
        settings: &Settings,
    ) -> Result<(), Stop> {
        let mut file = std::fs::File::open(path).map_err(error)?;
        let size = file.metadata().map_err(error)?.len();
        if size == 0 {
            return Err(error("the database file is empty"));
        }
        let (mut read, mut sent) = (0_u64, 0_u64);
        while read < size {
            self.check_stop()?;
            let (returned, raw_len, block) = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/peer_backup",
                move || -> std::io::Result<_> {
                    use std::io::Read;
                    let mut raw = vec![0_u8; BACKUP_SNAPSHOT_BLOCK_BYTES];
                    let mut filled = 0;
                    while filled < raw.len() {
                        match file.read(&mut raw[filled..])? {
                            0 => break,
                            count => filled += count,
                        }
                    }
                    Ok((file, filled as u64, encode_snapshot_block(&raw[..filled])?))
                },
            )
            .await
            .map_err(error)?
            .map_err(error)?;
            file = returned;
            read += raw_len;
            // A file that ends early (it never shrinks) would loop; stop at its real end.
            let last = read >= size || raw_len == 0;
            let pieces = block.chunks(BACKUP_CHUNK_BYTES).count();
            for (index, piece) in block.chunks(BACKUP_CHUNK_BYTES).enumerate() {
                let reply = self
                    .put_chunk(
                        target,
                        generation,
                        SNAPSHOT_NAME,
                        sent,
                        piece,
                        last && index + 1 == pieces,
                        settings,
                    )
                    .await?;
                if reply.received != sent + piece.len() as u64 {
                    return Err(error(
                        "the paired server holds another length of the snapshot",
                    ));
                }
                sent += piece.len() as u64;
            }
            if raw_len == 0 {
                break;
            }
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn put_chunk(
        &self,
        target: &str,
        generation: &str,
        name: &str,
        offset: u64,
        chunk: &[u8],
        end: bool,
        settings: &Settings,
    ) -> Result<Reply, Stop> {
        self.put(
            target,
            json!({
                "generation": generation, "file": name, "offset": offset,
                "data": base64::engine::general_purpose::STANDARD.encode(chunk),
                "sha256": data_encoding::HEXLOWER.encode(&Sha256::digest(chunk)),
                "end": end, "retain": settings.retain_generations,
            }),
        )
        .await
    }

    async fn send_file(
        &self,
        target: &str,
        generation: &str,
        name: &str,
        path: &Path,
        settings: &Settings,
    ) -> Result<(), Stop> {
        let mut file = tokio::fs::File::open(path).await.map_err(error)?;
        let size = file.metadata().await.map_err(error)?.len();
        let mut offset = 0_u64;
        let mut buffer = vec![0_u8; BACKUP_CHUNK_BYTES];
        loop {
            self.check_stop()?;
            file.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(error)?;
            let wanted = usize::try_from((size - offset).min(BACKUP_CHUNK_BYTES as u64))
                .unwrap_or(BACKUP_CHUNK_BYTES);
            file.read_exact(&mut buffer[..wanted])
                .await
                .map_err(error)?;
            let chunk = &buffer[..wanted];
            let end = offset + wanted as u64 == size;
            let reply = self
                .put_chunk(target, generation, name, offset, chunk, end, settings)
                .await?;
            if reply.no_snapshot {
                return Err(Stop::Rebuild);
            }
            if reply.received > size {
                return Err(error("the paired server holds more than this file"));
            }
            if reply.done && reply.received == size {
                return Ok(());
            }
            // Either the chunk landed, or the paired server holds another length: continue there.
            offset = reply.received;
        }
    }

    /// One `backup.put`, tried again after a transport failure. A chunk lands at its
    /// offset, so a try after a lost answer changes nothing.
    async fn put(&self, target: &str, payload: Value) -> Result<Reply, Stop> {
        let mut attempt = 1;
        loop {
            let result = self.put_once(target, &payload).await;
            if let Err(Stop::Error(message)) = &result
                && attempt < BACKUP_PUT_ATTEMPTS
            {
                eprintln!("backup put failed, trying again: {message}");
                attempt += 1;
                tokio::time::sleep(PUT_RETRY_DELAY).await;
                self.check_stop()?;
                continue;
            }
            return result;
        }
    }

    async fn put_once(&self, target: &str, payload: &Value) -> Result<Reply, Stop> {
        let key = RequestIdentity::issue_root();
        let envelope =
            json!({"requestId": key, "operation": [], "action": ACTION, "payload": payload});
        let receipt = self
            .transport
            .exchange(target, &envelope, PUT_REQUEST)
            .await
            .map_err(error)?;
        if receipt.request_id != key {
            return Err(error(INVALID_RECEIPT));
        }
        match receipt.outcome {
            RemoteOutcome::Applied => receipt
                .value
                .and_then(|value| serde_json::from_str(value.get()).ok())
                .ok_or_else(|| error(INVALID_RECEIPT)),
            RemoteOutcome::NotApplied => {
                Err(Stop::Error(receipt.error.unwrap_or_else(|| {
                    "The paired server refused the backup".to_owned()
                })))
            }
            RemoteOutcome::Unknown => Err(error("The paired server did not answer")),
        }
    }
}

#[cfg(test)]
mod tests;
