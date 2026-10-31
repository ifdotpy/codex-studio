//! Exact-offset log delivery. File writes precede SQLite; replay compares bytes.
//! A fixed lock set bounds memory and serializes this runtime's output per monitor.

use crate::{
    monitor_policy::{self, MonitorError, Operation},
    monitor_runtime,
};
#[cfg(not(unix))]
use std::fs::OpenOptions;
use std::fs::{self, File};
use std::hash::{Hash, Hasher};
use std::io::{Read, Seek, SeekFrom, Write};
use studio_runtime_store::RuntimeStore;
use tokio::sync::Mutex;

const OUTPUT_LOCKS: usize = 64;

/// Directory under the state root that holds each monitor's output log,
/// `<id>.log`. Python owns the same layout (`codex_runtime.py`, `monitor-logs`).
/// Every monitor record names its log in this directory; delivery refuses any
/// other path.
pub const MONITOR_LOG_DIR: &str = "monitor-logs";

/// The monitor log directory of a state root.
#[must_use]
pub fn log_root(state_root: &std::path::Path) -> std::path::PathBuf {
    state_root.join(MONITOR_LOG_DIR)
}

pub struct MonitorOutput {
    locks: [Mutex<()>; OUTPUT_LOCKS],
    #[cfg(test)]
    after_sync: std::sync::Mutex<
        Option<(
            std::sync::Arc<tokio::sync::Notify>,
            std::sync::Arc<tokio::sync::Notify>,
        )>,
    >,
}
impl Default for MonitorOutput {
    fn default() -> Self {
        Self {
            locks: std::array::from_fn(|_| Mutex::const_new(())),
            #[cfg(test)]
            after_sync: std::sync::Mutex::new(None),
        }
    }
}
impl MonitorOutput {
    /// Offset belongs to the exact native stream receipt, never to a retry's local counter.
    pub async fn append(
        &self,
        store: &RuntimeStore,
        id: &str,
        operation: &Operation,
        offset: u64,
        bytes: &[u8],
        now: f64,
    ) -> Result<(), MonitorError> {
        self.append_stream(store, id, operation, offset, "stdout", bytes, now)
            .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn append_stream(
        &self,
        store: &RuntimeStore,
        id: &str,
        operation: &Operation,
        offset: u64,
        stream: &str,
        bytes: &[u8],
        now: f64,
    ) -> Result<(), MonitorError> {
        if bytes.is_empty() {
            return self
                .append_chunk(store, id, operation, offset, stream, bytes, now)
                .await;
        }
        for (index, chunk) in bytes.chunks(monitor_policy::CHUNK_BYTES).enumerate() {
            let chunk_offset = offset
                .checked_add((index * monitor_policy::CHUNK_BYTES) as u64)
                .ok_or(MonitorError::InvalidOutputReceipt("cursor overflow"))?;
            self.append_chunk(store, id, operation, chunk_offset, stream, chunk, now)
                .await?;
        }
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn append_chunk(
        &self,
        store: &RuntimeStore,
        id: &str,
        operation: &Operation,
        offset: u64,
        stream: &str,
        bytes: &[u8],
        now: f64,
    ) -> Result<(), MonitorError> {
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        let _guard = self.locks[hasher.finish() as usize % OUTPUT_LOCKS]
            .lock()
            .await;
        let id_owned = id.to_owned();
        let (before, _) = store
            .transaction_with(move |tx| monitor_runtime::snapshot(tx, &id_owned))
            .await?;
        let count = before.record().bytes.get() as u64;
        if before
            .operation()
            .is_none_or(|current| !current.same_identity(operation))
        {
            return Err(MonitorError::InvalidOutputReceipt(
                "operation identity changed",
            ));
        }
        // Output that reaches a finished monitor is ignored and does not advance
        // its byte count, so every later chunk of it is ignored too, not a gap.
        if before.status() != monitor_policy::MonitorStatus::Running {
            return Ok(());
        }
        if offset > count {
            return Err(MonitorError::InvalidOutputReceipt("cursor has a gap"));
        }
        let path = std::path::Path::new(id);
        if path.components().count() != 1 || path.file_name().and_then(|s| s.to_str()) != Some(id) {
            return Err(MonitorError::InvalidOutputReceipt("invalid log identity"));
        }
        let root = log_root(store.root());
        if !root.exists() {
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                fs::DirBuilder::new().mode(0o700).create(&root)?;
            }
            #[cfg(not(unix))]
            fs::create_dir(&root)?;
        }
        if !fs::symlink_metadata(&root)?.is_dir() {
            return Err(MonitorError::InvalidOutputReceipt("invalid log directory"));
        }
        let path = root.join(format!("{id}.log"));
        // The record can name the log through a link to the state root.
        if !studio_runtime_store::path_identity::same(
            std::path::Path::new(&before.record().log),
            &path,
        ) {
            return Err(MonitorError::InvalidOutputReceipt(
                "log path differs from state directory",
            ));
        }
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && !metadata.is_file()
        {
            return Err(MonitorError::InvalidOutputReceipt("invalid log file"));
        }
        #[cfg(unix)]
        let mut file = {
            use rustix::fs::{Mode, OFlags, open, openat};
            let directory = open(
                &root,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(std::io::Error::from)?;
            let descriptor = openat(
                &directory,
                format!("{id}.log"),
                OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::RUSR | Mode::WUSR,
            )
            .map_err(std::io::Error::from)?;
            File::from(descriptor)
        };
        #[cfg(not(unix))]
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        if !file.metadata()?.is_file() {
            return Err(MonitorError::InvalidOutputReceipt("invalid log file"));
        }
        let length = file.metadata()?.len();
        if length < offset {
            return Err(MonitorError::InvalidOutputReceipt(
                "log is missing committed output",
            ));
        }
        let existing = (length - offset).min(bytes.len() as u64) as usize;
        let mut previous = vec![0; existing];
        file.seek(SeekFrom::Start(offset))?;
        file.read_exact(&mut previous)?;
        if previous != bytes[..existing] {
            return Err(MonitorError::InvalidOutputReceipt(
                "receipt bytes differ at committed offset",
            ));
        }
        if offset < count {
            if offset + bytes.len() as u64 > count {
                return Err(MonitorError::InvalidOutputReceipt(
                    "receipt overlaps committed cursor",
                ));
            }
            return Ok(());
        }
        file.write_all(&bytes[existing..])?;
        file.sync_all()?;
        File::open(&root)?.sync_all()?;
        #[cfg(test)]
        if let Some((synced, resume)) = { self.after_sync.lock().unwrap().take() } {
            synced.notify_one();
            resume.notified().await;
        }
        let id = id.to_owned();
        let operation = operation.clone();
        let bytes = bytes.to_vec();
        let stream = stream.to_owned();
        store
            .transaction_with(move |tx| {
                let (current, actor) = monitor_runtime::snapshot(tx, &id)?;
                if current.record() != before.record() {
                    return Err(MonitorError::Refused(
                        "Monitor changed during log delivery; reconcile exact receipt",
                    ));
                }
                let decision =
                    monitor_policy::output_stream(&current, &operation, &stream, &bytes, now)?;
                monitor_runtime::apply(tx, &current, &actor, &decision, now)
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor_driver::{MonitorNative, NotSentReason, Submission};
    use crate::monitor_policy::Effect;
    use crate::monitor_runtime_wiring::MonitorEvent;
    use std::sync::Arc;
    use studio_runtime_store::{AgentRecord, AgentRepository, MonitorRecord, Table};
    use tokio::sync::Notify;

    struct Missing;
    impl MonitorNative for Missing {
        fn submit<'a>(
            &'a self,
            _: &'a Operation,
            _: &'a Effect,
        ) -> futures_util::future::BoxFuture<'a, Submission> {
            Box::pin(async {
                Submission::NotSent {
                    reason: NotSentReason::MonitorMissing,
                    error: "command no longer running".into(),
                }
            })
        }
    }

    #[tokio::test]
    async fn missing_cancel_cannot_finish_ahead_of_fsynced_queued_output() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        let log = log_root(store.root())
            .join("m.log")
            .to_string_lossy()
            .into_owned();
        let operation = store.transaction_with(move |tx| {
            let actor = AgentRecord::from_value(serde_json::json!({"compactions":0,"accountKey":"default","provider":studio_runtime_schema::ProviderKind::Codex,"id":"worker","rootId":"worker","epoch":0,"status":"running","autoWake":true})).unwrap();
            AgentRepository::put(tx, &actor)?;
            let mut record: MonitorRecord = serde_json::from_str(r#"{"id":"m","agent":"worker","epoch":0,"status":"running","command":"test","created":5,"tail":"","requestId":"request","activityAt":5,"activityGeneration":0,"bytes":0,"cwd":"/work","interactive":false,"livenessCommand":"","log":"/work/m.log","stallTimeoutSeconds":1800,"stallWakeGeneration":-1,"successExitCodes":[0],"timeout_ms":3600000,"wakeOn":"exit","outputPatterns":["READY"],"operation":{"agent":"worker","epoch":0,"accountKey":"default","connectionId":"connection"}}"#).unwrap();
            record.log = log;
            let operation = record.operation.clone().unwrap();
            Table::new(tx).put(&record)?;
            crate::monitor_event_queue::enqueue_output_in_transaction(tx, "m", "default", 1, 1, &crate::monitor_event_queue::OutputDelta { delta_base64: "UkVBRFkK".into(), stream: None }).unwrap();
            Ok::<_, studio_runtime_store::StoreError>(operation)
        }).await.unwrap();
        let delivery = crate::monitor_event_queue::next(&store)
            .await
            .unwrap()
            .unwrap();
        let synced = Arc::new(Notify::new());
        let resume = Arc::new(Notify::new());
        let output = Arc::new(MonitorOutput::default());
        *output.after_sync.lock().unwrap() = Some((synced.clone(), resume.clone()));
        let task = {
            let output = output.clone();
            let store = store.clone();
            let operation = operation.clone();
            tokio::spawn(async move {
                output
                    .append(&store, "m", &operation, 0, b"READY\n", 10.0)
                    .await
            })
        };
        tokio::time::timeout(std::time::Duration::from_secs(10), synced.notified())
            .await
            .unwrap();
        crate::monitor_driver::cancel(&store, &Missing, "m", Some("worker"), 11.0)
            .await
            .unwrap();
        resume.notify_one();
        assert!(
            task.await.unwrap().is_err(),
            "changed row requires exact replay"
        );
        output
            .append(&store, "m", &operation, 0, b"READY\n", 12.0)
            .await
            .unwrap();
        crate::monitor_event_queue::acknowledge(&store, &delivery)
            .await
            .unwrap();
        let (bytes, tail) = store
            .read_async("test/output-replay", |db| {
                let record = Table::<MonitorRecord>::new(db)
                    .get(&"m".to_owned())?
                    .unwrap();
                Ok::<_, studio_runtime_store::StoreError>((
                    record.bytes.get(),
                    record.tail.as_str().to_owned(),
                ))
            })
            .await
            .unwrap();
        assert_eq!(
            (bytes, tail.as_str()),
            (6, "READY\n"),
            "replayed fsynced output must be recorded before ACK"
        );
        let next = crate::monitor_event_queue::next(&store)
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(next.event, MonitorEvent::Exit { .. }),
            "terminal result stays behind output"
        );
        let MonitorEvent::Exit {
            code,
            ref error,
            finished,
        } = next.event
        else {
            unreachable!()
        };
        crate::monitor_receipts::finish(
            &store,
            &crate::monitor_receipts::ResultReceipt {
                version: 1,
                key: "m".into(),
                code,
                error: error.clone(),
                operation: Some(operation.clone()),
                finished,
            },
        )
        .await
        .unwrap();
        crate::monitor_event_queue::acknowledge(&store, &next)
            .await
            .unwrap();
        crate::monitor_event_queue::acknowledge(&store, &next)
            .await
            .unwrap();
        let path = store.root().to_path_buf();
        drop(store);
        let reopened = RuntimeStore::open(Some(&path)).await.unwrap();
        reopened
            .read_async("test/restart-output-replay", |db| {
                let record = Table::<MonitorRecord>::new(db)
                    .get(&"m".to_owned())?
                    .unwrap();
                assert_eq!(record.bytes.get(), 6);
                assert_eq!(record.tail.as_str(), "READY\n");
                assert_eq!(
                    record.status,
                    studio_runtime_schema::studio_errors::MonitorStatus::Cancelled
                );
                let wakes: i64 = db.query_row(
                    "SELECT COUNT(*) FROM runtime_events WHERE kind='monitor_output'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(wakes, 1, "the matched output wakes once across replay");
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .unwrap();
    }
}
