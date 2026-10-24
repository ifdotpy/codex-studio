//! First-result journal compatible with Python monitor-results files.
//! Publish with fsync + no-clobber link, commit rows/events, then acknowledge.

use crate::monitor_policy::{MonitorError, Operation};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use studio_runtime_store::RuntimeStore;

const RECEIPT_BYTES: u64 = 128 * 1024;
static RECOVERY_WARNING_REPORTED: AtomicBool = AtomicBool::new(false);

fn report_recovery_warning(path: &Path, error: impl std::fmt::Display) {
    if !RECOVERY_WARNING_REPORTED.swap(true, Ordering::Relaxed) {
        crate::diagnostics::log_stderr(format_args!(
            "monitor receipt recovery skipped {}: {error}",
            path.display()
        ));
    }
}

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

/// Ensure the first-result directory exists before installing its native watcher.
/// The consumer then scans after watch registration, covering both existing and
/// concurrently published receipts without an interval scan.
pub fn prepare_watch_directory(root: &Path) -> Result<PathBuf, MonitorError> {
    let directory = root.join("monitor-results");
    match fs::create_dir(&directory) {
        Ok(()) => {
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))?;
            }
            File::open(root)?.sync_all()?;
        }
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(MonitorError::Io(error)),
    }
    if !fs::symlink_metadata(&directory)?.is_dir() {
        return Err(MonitorError::InvalidResultReceipt("directory"));
    }
    Ok(directory)
}

/// No provider or database call happens before the exact first result is durable.
pub fn persist(root: &Path, receipt: &ResultReceipt) -> Result<ResultReceipt, MonitorError> {
    receipt.validate()?;
    let directory = prepare_watch_directory(root)?;
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

/// Run receipt file I/O on the blocking pool, off the async workers.
async fn blocking<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, MonitorError> + Send + 'static,
) -> Result<T, MonitorError> {
    studio_runtime_process::blocking_pool::spawn_blocking("runtime-services/monitor_receipts", work)
        .await
        .map_err(|_| MonitorError::Io(std::io::Error::other("monitor receipt file task failed")))?
}

pub async fn finish(store: &RuntimeStore, receipt: &ResultReceipt) -> Result<(), MonitorError> {
    let root = store.root().to_path_buf();
    let receipt = receipt.clone();
    let saved = blocking(move || persist(&root, &receipt)).await?;
    restore(store, &saved).await?;
    let root = store.root().to_path_buf();
    blocking(move || acknowledge(&root, &saved.key)).await
}

async fn restore(store: &RuntimeStore, receipt: &ResultReceipt) -> Result<(), MonitorError> {
    let receipt = receipt.clone();
    store
        .transaction_with(move |tx| {
            crate::monitor_runtime::finish_in_transaction(
                tx,
                &receipt.key,
                receipt.operation.as_ref(),
                receipt.code,
                receipt.error.as_deref(),
                receipt.finished,
            )
        })
        .await
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
pub async fn recover(
    store: &RuntimeStore,
    keys: &[String],
) -> Vec<(String, Result<(), MonitorError>)> {
    let mut results = Vec::new();
    for id in keys.iter().take(64) {
        let root = store.root().to_path_buf();
        let key = id.clone();
        let result = match blocking(move || read(&path(&root, &key), Some(&key))).await {
            Ok(receipt) => match restore(store, &receipt).await {
                Ok(()) => {
                    let root = store.root().to_path_buf();
                    let key = id.clone();
                    blocking(move || acknowledge(&root, &key)).await
                }
                Err(error) => Err(error),
            },
            Err(error) => Err(error),
        };
        results.push((id.clone(), result));
    }
    results
}

/// Open a recovery directory; `None` when it is absent or unusable (reported,
/// except when absent). The iterator is consumed incrementally by blocking
/// tasks so a bounded recovery batch does not materialize the entire directory.
fn receipt_paths(directory: &Path) -> Option<fs::ReadDir> {
    match fs::symlink_metadata(directory) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return None,
        Err(error) => {
            report_recovery_warning(directory, error);
            return None;
        }
        Ok(metadata) if !metadata.is_dir() => {
            report_recovery_warning(directory, "not a directory");
            return None;
        }
        Ok(_) => {}
    }
    match fs::read_dir(directory) {
        Ok(entries) => Some(entries),
        Err(error) => {
            report_recovery_warning(directory, error);
            None
        }
    }
}

/// Return the next valid receipt and keep the directory cursor for the next
/// offloaded step. Invalid entries and receipts are reported and skipped.
fn next_receipt<I: Iterator<Item = std::io::Result<fs::DirEntry>>>(
    mut entries: I,
    root: &Path,
) -> (I, Option<(PathBuf, ResultReceipt)>) {
    loop {
        let entry = match entries.next() {
            Some(Ok(entry)) => entry,
            Some(Err(error)) => {
                report_recovery_warning(root, error);
                continue;
            }
            None => return (entries, None),
        };
        let receipt_path = entry.path();
        if receipt_path
            .extension()
            .is_some_and(|extension| extension == "json")
        {
            let receipt = match read(&receipt_path, None) {
                Ok(receipt) => receipt,
                Err(error) => {
                    report_recovery_warning(&receipt_path, error);
                    continue;
                }
            };
            if receipt_path != path(root, &receipt.key) {
                report_recovery_warning(&receipt_path, "invalid identity");
                continue;
            }
            return (entries, Some((receipt_path, receipt)));
        }
    }
}

/// Recover at most `limit` valid receipts, scanning past invalid entries so they
/// cannot starve later receipts. Invalid or unreadable files remain in place and
/// are reported; successful batches can be drained by the caller until short.
pub async fn recover_pending(store: &RuntimeStore, limit: usize) -> Result<usize, MonitorError> {
    if limit == 0 {
        return Ok(0);
    }
    let directory = store.root().join("monitor-results");
    let root = store.root().to_path_buf();
    let scan_directory = directory.clone();
    let Some(mut entries) = blocking(move || Ok(receipt_paths(&scan_directory))).await? else {
        return Ok(0);
    };
    let mut recovered = 0;
    loop {
        let receipt_root = root.clone();
        let (next_entries, receipt) =
            blocking(move || Ok(next_receipt(entries, &receipt_root))).await?;
        entries = next_entries;
        let Some((entry, receipt)) = receipt else {
            return Ok(recovered);
        };
        if let Err(error) = restore(store, &receipt).await {
            if crate::monitor_runtime::startup_store_failure(&error) {
                return Err(error);
            }
            report_recovery_warning(&entry, error);
            continue;
        }
        let root = store.root().to_path_buf();
        let key = receipt.key.clone();
        if let Err(error) = blocking(move || acknowledge(&root, &key)).await {
            report_recovery_warning(&entry, &error);
            return Err(error);
        }
        recovered += 1;
        if recovered >= limit.min(64) {
            return Ok(recovered);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn receipt_iteration_skips_invalid_files_and_keeps_a_cursor() {
        let root = tempfile::tempdir().expect("temporary state directory");
        let directory = prepare_watch_directory(root.path()).expect("receipt directory");
        fs::write(directory.join("invalid.json"), b"not a receipt").expect("write corrupt receipt");
        for key in ["valid-receipt", "second-receipt"] {
            persist(
                root.path(),
                &ResultReceipt {
                    version: 1,
                    key: key.into(),
                    code: Some(0),
                    error: None,
                    operation: None,
                    finished: 1.0,
                },
            )
            .expect("persist valid receipt");
        }
        fs::write(directory.join("another-invalid.json"), b"invalid")
            .expect("second corrupt receipt");
        let order = [
            directory.join("invalid.json"),
            path(root.path(), "valid-receipt"),
            directory.join("another-invalid.json"),
            path(root.path(), "second-receipt"),
        ];
        // Keep real directory entries and impose the interleaving read_dir cannot guarantee.
        let mut entries: Vec<_> = receipt_paths(&directory)
            .expect("receipt directory is readable")
            .map(|entry| entry.expect("directory entry"))
            .collect();
        entries.sort_by_key(|entry| {
            order
                .iter()
                .position(|p| p == &entry.path())
                .expect("ordered entry")
        });
        let mut entries = entries.into_iter().map(Ok);
        for key in ["valid-receipt", "second-receipt"] {
            let (remaining, found) = next_receipt(entries, root.path());
            entries = remaining;
            let (found_path, receipt) = found.expect("valid receipt after corruption");
            assert_eq!(found_path, path(root.path(), key));
            assert_eq!(receipt.key, key);
        }
        let (_, exhausted) = next_receipt(entries, root.path());
        assert!(exhausted.is_none());
    }
}
