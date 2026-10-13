//! First-result journal compatible with Python monitor-results files.
//! Publish with fsync + no-clobber link, commit rows/events, then acknowledge.

use crate::monitor_policy::{MonitorError, Operation};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use studio_runtime_store::RuntimeStore;

const RECEIPT_BYTES: u64 = 128 * 1024;

#[derive(Clone, Deserialize, Serialize)]
pub struct ResultReceipt {
    pub version: u8,
    pub key: String,
    pub code: Option<i64>,
    pub error: Option<String>,
    pub operation: Option<Operation>,
    pub finished: f64,
}

impl ResultReceipt {
    fn validate(&self) -> Result<(), MonitorError> {
        let value = serde_json::to_value(self).map_err(studio_runtime_store::StoreError::from)?;
        if !studio_runtime_schema::validate("PersistedMonitorResult", &value)
            .map_err(|_| MonitorError::InvalidResultReceipt("schema"))?
            || (self.code.is_none() && self.error.as_deref().is_none_or(str::is_empty))
        {
            return Err(MonitorError::InvalidResultReceipt("fields"));
        }
        Ok(())
    }
}

fn path(root: &Path, id: &str) -> PathBuf {
    root.join("monitor-results").join(format!(
        "{}.json",
        studio_runtime_store::sha256_hex(id.as_bytes())
    ))
}

/// No provider or database call happens before the exact first result is durable.
pub fn persist(root: &Path, receipt: &ResultReceipt) -> Result<ResultReceipt, MonitorError> {
    receipt.validate()?;
    let directory = root.join("monitor-results");
    if !directory.exists() {
        fs::create_dir(&directory)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
        }
        File::open(root)?.sync_all()?;
    }
    if !fs::symlink_metadata(&directory)?.is_dir() {
        return Err(MonitorError::InvalidResultReceipt("directory"));
    }
    let target = path(root, &receipt.key);
    if target.exists() {
        return read(&target, Some(&receipt.key));
    }
    let bytes = serde_json::to_vec(receipt).map_err(studio_runtime_store::StoreError::from)?;
    if bytes.len() as u64 > RECEIPT_BYTES {
        return Err(MonitorError::InvalidResultReceipt("size"));
    }
    let mut temp = tempfile::NamedTempFile::new_in(&directory)?;
    temp.write_all(&bytes)?;
    temp.as_file().sync_all()?;
    match temp.persist_noclobber(&target) {
        Ok(_) => (),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => (),
        Err(error) => return Err(MonitorError::Io(error.error)),
    }
    File::open(&directory)?.sync_all()?;
    read(&target, Some(&receipt.key))
}

fn read(path: &Path, id: Option<&str>) -> Result<ResultReceipt, MonitorError> {
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_file() || metadata.len() > RECEIPT_BYTES {
        return Err(MonitorError::InvalidResultReceipt("file"));
    }
    let mut bytes = Vec::new();
    File::open(path)?
        .take(RECEIPT_BYTES + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() as u64 > RECEIPT_BYTES {
        return Err(MonitorError::InvalidResultReceipt("size"));
    }
    let receipt: ResultReceipt =
        serde_json::from_slice(&bytes).map_err(|_| MonitorError::InvalidResultReceipt("json"))?;
    receipt.validate()?;
    if id.is_some_and(|id| receipt.key != id) {
        return Err(MonitorError::InvalidResultReceipt("identity"));
    }
    Ok(receipt)
}

pub fn finish(store: &RuntimeStore, receipt: &ResultReceipt) -> Result<(), MonitorError> {
    let saved = persist(store.root(), receipt)?;
    restore(store, &saved)?;
    acknowledge(store.root(), &saved.key)
}

fn restore(store: &RuntimeStore, receipt: &ResultReceipt) -> Result<(), MonitorError> {
    store.transaction_with(|tx| {
        crate::monitor_runtime::finish_in_transaction(
            tx,
            &receipt.key,
            receipt.operation.as_ref(),
            receipt.code,
            receipt.error.as_deref(),
            receipt.finished,
        )
    })
}

fn acknowledge(root: &Path, id: &str) -> Result<(), MonitorError> {
    match fs::remove_file(path(root, id)) {
        Ok(()) => File::open(root.join("monitor-results"))?
            .sync_all()
            .map_err(MonitorError::Io),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(MonitorError::Io(error)),
    }
}

/// Reconcile selected identities in bounded batches; corrupt/foreign receipts remain for diagnosis.
pub fn recover(store: &RuntimeStore, keys: &[String]) -> Vec<(String, Result<(), MonitorError>)> {
    keys.iter()
        .take(64)
        .map(|id| {
            let result = read(&path(store.root(), id), Some(id))
                .and_then(|receipt| restore(store, &receipt))
                .and_then(|()| acknowledge(store.root(), id));
            (id.clone(), result)
        })
        .collect()
}

/// Bounded startup/timer recovery, including receipts for already terminal rows.
/// A corrupt receipt stops this batch visibly and is retained for diagnosis.
pub fn recover_pending(store: &RuntimeStore, limit: usize) -> Result<usize, MonitorError> {
    if limit == 0 {
        return Ok(0);
    }
    let directory = store.root().join("monitor-results");
    match fs::symlink_metadata(&directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(error.into()),
        Ok(metadata) if !metadata.is_dir() => {
            return Err(MonitorError::InvalidResultReceipt("directory"));
        }
        Ok(_) => {}
    }
    let mut recovered = 0;
    let mut entries = fs::read_dir(directory)?;
    for _ in 0..64 {
        let Some(entry) = entries.next() else {
            return Ok(recovered);
        };
        let entry = entry?;
        if entry
            .path()
            .extension()
            .is_none_or(|extension| extension != "json")
        {
            continue;
        }
        let receipt = read(&entry.path(), None)?;
        if entry.path() != path(store.root(), &receipt.key) {
            return Err(MonitorError::InvalidResultReceipt("identity"));
        }
        restore(store, &receipt)?;
        acknowledge(store.root(), &receipt.key)?;
        recovered += 1;
        if recovered >= limit.min(64) {
            return Ok(recovered);
        }
    }
    if entries.next().is_some() {
        return Err(MonitorError::InvalidResultReceipt("scan bound"));
    }
    Ok(recovered)
}
