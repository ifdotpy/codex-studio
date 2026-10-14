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
use std::sync::Mutex;
use studio_runtime_store::RuntimeStore;

const OUTPUT_LOCKS: usize = 64;

pub struct MonitorOutput {
    locks: [Mutex<()>; OUTPUT_LOCKS],
}
impl Default for MonitorOutput {
    fn default() -> Self {
        Self {
            locks: std::array::from_fn(|_| Mutex::new(())),
        }
    }
}
impl MonitorOutput {
    /// Offset belongs to the exact native stream receipt, never to a retry's local counter.
    pub fn append(
        &self,
        store: &RuntimeStore,
        id: &str,
        operation: &Operation,
        offset: u64,
        bytes: &[u8],
        now: f64,
    ) -> Result<(), MonitorError> {
        if bytes.len() > monitor_policy::CHUNK_BYTES {
            return Err(MonitorError::InvalidOutputReceipt("chunk exceeds bound"));
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        id.hash(&mut hasher);
        let _guard = self.locks[hasher.finish() as usize % OUTPUT_LOCKS]
            .lock()
            .map_err(|_| MonitorError::Refused("Monitor output lock failed"))?;
        let (before, _) = store.transaction_with(|tx| monitor_runtime::snapshot(tx, id))?;
        let count = before
            .record()
            .get("bytes")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0);
        if before.operation() != Some(operation) {
            return Err(MonitorError::InvalidOutputReceipt(
                "operation identity changed",
            ));
        }
        if offset > count {
            return Err(MonitorError::InvalidOutputReceipt("cursor has a gap"));
        }
        if before
            .record()
            .get("status")
            .and_then(serde_json::Value::as_str)
            != Some("running")
        {
            return Ok(());
        }
        let path = std::path::Path::new(id);
        if path.components().count() != 1 || path.file_name().and_then(|s| s.to_str()) != Some(id) {
            return Err(MonitorError::InvalidOutputReceipt("invalid log identity"));
        }
        let root = store.root().join("monitor-logs");
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
        if before
            .record()
            .get("log")
            .and_then(serde_json::Value::as_str)
            != path.to_str()
        {
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
        store.transaction_with(|tx| {
            let (current, actor) = monitor_runtime::snapshot(tx, id)?;
            if current.record() != before.record() {
                return Err(MonitorError::Refused(
                    "Monitor changed during log delivery; reconcile exact receipt",
                ));
            }
            let decision = monitor_policy::output(&current, operation, bytes, now)?;
            monitor_runtime::apply(tx, &current, &actor, &decision, now)
        })
    }
}
