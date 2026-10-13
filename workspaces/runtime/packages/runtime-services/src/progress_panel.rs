//! Progress-file reads and renderer measurement writes.

use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde_json::{Map, Value, json};
use thiserror::Error;

const MAX_PROGRESS_BYTES: usize = 128 * 1024;
const MAX_LAYOUT_BYTES: usize = 64 * 1024;
const MAX_CLIENTS: usize = 32;
const FRESH_SECONDS: f64 = 90.0;
const SAFE_INTEGER: u64 = 9_007_199_254_740_991;
const RENDERER_V1: &str = "progress-markdown-v1";
const RENDERER_V2: &str = "progress-markdown-v2";
const LAYOUT_FILE: &str = "PROGRESS.layout.json";

#[derive(Debug, Error)]
pub enum ProgressPanelError {
    #[error("Invalid agent identity for PROGRESS.md")]
    InvalidAgent,
    #[error("PROGRESS.md exceeds the 128 KiB limit")]
    ProgressTooLarge,
    #[error("PROGRESS.md must contain valid UTF-8 text")]
    ProgressUtf8,
    #[error("PROGRESS.md changed during the read. Studio will retry")]
    ChangedDuringRead,
    #[error("Invalid progress layout: {0}")]
    InvalidLayout(&'static str),
    #[error("The progress file changed. Measure its current revision")]
    StaleRevision,
    #[error("A newer layout measurement already exists")]
    NewerMeasurement,
    #[error("Too many active progress clients. Retry after old reports expire")]
    TooManyClients,
    #[error("Another layout measurement is being saved. Retry")]
    Busy,
    #[error("The progress file changed while saving its measurement")]
    ChangedWhileSaving,
    #[error("Invalid stored progress layout")]
    InvalidStoredLayout,
    #[error("Progress file operation failed")]
    Io(#[from] std::io::Error),
}

pub fn progress_path(state_dir: &Path, agent: &str) -> Result<PathBuf, ProgressPanelError> {
    if agent.is_empty()
        || agent.len() > 200
        || !agent
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ProgressPanelError::InvalidAgent);
    }
    let root = if state_dir.is_absolute() {
        state_dir.to_owned()
    } else {
        std::env::current_dir()?.join(state_dir)
    };
    Ok(root.join("progress").join(agent).join("PROGRESS.md"))
}

pub fn read_progress(state_dir: &Path, agent: &str) -> Result<Value, ProgressPanelError> {
    let path = progress_path(state_dir, agent)?;
    let mut panel = json!({
        "id": agent,
        "agent": agent,
        "format": "markdown",
        "markdown": "",
        "path": path,
        "revision": null,
        "updated": null,
        "exists": false,
        "error": null,
    });
    match read_file(&path) {
        Ok(Some((markdown, revision, updated))) => {
            panel["markdown"] = Value::String(markdown);
            panel["revision"] = Value::String(revision);
            panel["updated"] = json!(updated);
            panel["exists"] = Value::Bool(true);
        }
        Ok(None) => {}
        Err(ProgressPanelError::ProgressTooLarge) => {
            panel["exists"] = Value::Bool(true);
            panel["error"] =
                json!("Cannot read PROGRESS.md: PROGRESS.md exceeds the 128 KiB limit");
        }
        Err(ProgressPanelError::ProgressUtf8) => {
            panel["exists"] = Value::Bool(true);
            panel["error"] = json!("PROGRESS.md must contain valid UTF-8 text");
        }
        Err(error) => {
            if matches!(error, ProgressPanelError::ChangedDuringRead) {
                panel["exists"] = Value::Bool(true);
            }
            panel["error"] = json!(format!("Cannot read PROGRESS.md: {error}"));
        }
    }
    Ok(panel)
}

fn read_file(path: &Path) -> Result<Option<(String, String, f64)>, ProgressPanelError> {
    if let Some(agent_dir) = path.parent() {
        if let Some(progress_dir) = agent_dir.parent() {
            match fs::symlink_metadata(progress_dir) {
                Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                    return Err(std::io::Error::other("progress directory must be regular").into());
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(error) => return Err(error.into()),
                _ => {}
            }
        }
        match fs::symlink_metadata(agent_dir) {
            Ok(metadata) if metadata.file_type().is_symlink() || !metadata.is_dir() => {
                return Err(
                    std::io::Error::other("agent progress directory must be regular").into(),
                );
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            _ => {}
        }
    }
    let metadata = match fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::other("PROGRESS.md must be a regular file").into());
    }
    if metadata.len() > MAX_PROGRESS_BYTES as u64 {
        return Err(ProgressPanelError::ProgressTooLarge);
    }
    let mut file = match OpenOptions::new().read(true).open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    let before = file.metadata()?;
    let current = fs::symlink_metadata(path)?;
    if !before.is_file()
        || current.file_type().is_symlink()
        || !current.is_file()
        || !same_file(&before, &current)
    {
        return Err(std::io::Error::other("PROGRESS.md must be a regular file").into());
    }
    let mut bytes = Vec::with_capacity(before.len() as usize);
    Read::by_ref(&mut file)
        .take((MAX_PROGRESS_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let after = file.metadata()?;
    if bytes.len() > MAX_PROGRESS_BYTES {
        return Err(ProgressPanelError::ProgressTooLarge);
    }
    let first_revision = revision(&before)?;
    let final_revision = revision(&after)?;
    if first_revision != final_revision {
        return Err(ProgressPanelError::ChangedDuringRead);
    }
    let markdown = String::from_utf8(bytes).map_err(|_| ProgressPanelError::ProgressUtf8)?;
    let updated = after
        .modified()?
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    Ok(Some((markdown, final_revision, updated)))
}

fn same_file(left: &fs::Metadata, right: &fs::Metadata) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        left.dev() == right.dev() && left.ino() == right.ino()
    }
    #[cfg(not(unix))]
    {
        left.len() == right.len() && left.modified().ok() == right.modified().ok()
    }
}

fn revision(metadata: &fs::Metadata) -> Result<String, ProgressPanelError> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let modified_ns = metadata.mtime() as i128 * 1_000_000_000 + metadata.mtime_nsec() as i128;
        let changed_ns = metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128;
        Ok(format!(
            "{}:{}:{}:{}:{}",
            metadata.dev(),
            metadata.ino(),
            modified_ns,
            changed_ns,
            metadata.len()
        ))
    }
    #[cfg(windows)]
    {
        let _ = metadata;
        Err(ProgressPanelError::Io(std::io::Error::other(
            "resource-policy/windows-file-identity",
        )))
    }
}

pub fn record_layout(state_dir: &Path, body: &Value) -> Result<Value, ProgressPanelError> {
    let report = validate_report(body)?;
    let agent = required_string(body, "agent")?;
    let progress = progress_path(state_dir, agent)?;
    let directory = progress.parent().ok_or(ProgressPanelError::InvalidAgent)?;
    if let Some(progress_directory) = directory.parent() {
        check_directory(progress_directory)?;
    }
    check_directory(directory)?;
    let lock_path = directory.join(".layout.lock");
    let lock = open_regular(&lock_path, true)?;
    lock.try_lock().map_err(|error| match error {
        std::fs::TryLockError::WouldBlock => ProgressPanelError::Busy,
        std::fs::TryLockError::Error(error) => ProgressPanelError::Io(error),
    })?;

    let (markdown, revision, _) = read_file(&progress)
        .map_err(|_| ProgressPanelError::StaleRevision)?
        .ok_or(ProgressPanelError::StaleRevision)?;
    let requested_revision = required_string(body, "revision")?;
    if revision != requested_revision {
        return Err(ProgressPanelError::StaleRevision);
    }
    let digest = studio_runtime_store::sha256_hex(markdown.as_bytes());
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let layout_path = directory.join(LAYOUT_FILE);
    let previous = read_layout(&layout_path)?;
    let matching = previous.get("version").and_then(Value::as_u64) == Some(1)
        && previous.get("agent").and_then(Value::as_str) == Some(agent)
        && previous.get("revision").and_then(Value::as_str) == Some(revision.as_str())
        && previous.get("sha256").and_then(Value::as_str) == Some(digest.as_str());
    let mut reports = if matching {
        fresh_reports(&previous, now)
    } else {
        Vec::new()
    };
    let client = report["client"]
        .as_str()
        .ok_or(ProgressPanelError::InvalidLayout("client"))?;
    for existing in &reports {
        if existing.get("client").and_then(Value::as_str) == Some(client)
            && existing
                .get("sequence")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                >= report["sequence"].as_u64().unwrap_or(0)
        {
            if existing.get("sequence") == report.get("sequence")
                && report.iter().all(|(key, value)| {
                    existing
                        .get(key)
                        .is_some_and(|stored| report_value_equal(stored, value))
                })
            {
                return Ok(json!({
                    "version": 1,
                    "agent": agent,
                    "revision": revision,
                    "sha256": digest,
                    "status": status(&reports),
                    "updatedAt": previous.get("updatedAt").cloned().unwrap_or(json!(now)),
                    "reports": reports,
                }));
            }
            return Err(ProgressPanelError::NewerMeasurement);
        }
    }
    let others = reports
        .drain(..)
        .filter(|item| item.get("client").and_then(Value::as_str) != Some(client))
        .collect::<Vec<_>>();
    if others.len() >= MAX_CLIENTS {
        return Err(ProgressPanelError::TooManyClients);
    }
    let mut saved_report = report;
    saved_report.insert("measuredAt".to_owned(), json!(now));
    saved_report.insert("expiresAt".to_owned(), json!(now + FRESH_SECONDS));
    let mut updated_reports = others;
    updated_reports.push(Value::Object(saved_report));
    let result = json!({
        "version": 1,
        "agent": agent,
        "revision": revision,
        "sha256": digest,
        "status": status(&updated_reports),
        "updatedAt": now,
        "reports": updated_reports,
    });
    let mut bytes =
        serde_json::to_vec_pretty(&result).map_err(|_| ProgressPanelError::InvalidStoredLayout)?;
    bytes.push(b'\n');
    if bytes.len() > MAX_LAYOUT_BYTES {
        return Err(ProgressPanelError::InvalidLayout("layout size"));
    }
    atomic_replace(directory, &layout_path, &progress, &revision, &bytes)?;
    Ok(result)
}

fn report_value_equal(left: &Value, right: &Value) -> bool {
    match (left.as_f64(), right.as_f64()) {
        (Some(left), Some(right)) => left == right,
        _ => left == right,
    }
}

fn validate_report(body: &Value) -> Result<Map<String, Value>, ProgressPanelError> {
    let object = body
        .as_object()
        .ok_or(ProgressPanelError::InvalidLayout("body"))?;
    let renderer = required_string(body, "renderer")?;
    let fields: &[&str] = if renderer == RENDERER_V2 {
        &[
            "overflowX",
            "overflowY",
            "totalLines",
            "visibleLines",
            "lastVisibleLine",
            "lastVisibleHeading",
        ]
    } else if renderer == RENDERER_V1 {
        &[]
    } else {
        return Err(ProgressPanelError::InvalidLayout("renderer"));
    };
    let mut expected = [
        "agent",
        "revision",
        "client",
        "sequence",
        "renderer",
        "width",
        "height",
        "contentWidth",
        "contentHeight",
        "fits",
        "reason",
    ]
    .into_iter()
    .chain(fields.iter().copied())
    .collect::<Vec<_>>();
    expected.sort_unstable();
    let mut actual = object.keys().map(String::as_str).collect::<Vec<_>>();
    actual.sort_unstable();
    if expected != actual {
        return Err(ProgressPanelError::InvalidLayout("fields"));
    }
    let revision = required_string(body, "revision")?;
    if revision.is_empty() || revision.len() > 200 {
        return Err(ProgressPanelError::InvalidLayout("revision"));
    }
    let client = required_string(body, "client")?;
    if client.is_empty()
        || client.len() > 100
        || !client
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err(ProgressPanelError::InvalidLayout("client"));
    }
    if body["sequence"]
        .as_u64()
        .is_none_or(|value| value > SAFE_INTEGER)
    {
        return Err(ProgressPanelError::InvalidLayout("sequence"));
    }
    for field in ["width", "height", "contentWidth", "contentHeight"] {
        let value = number(&body[field]).ok_or(ProgressPanelError::InvalidLayout(field))?;
        if !(0.0..=10_000_000.0).contains(&value) {
            return Err(ProgressPanelError::InvalidLayout(field));
        }
    }
    let width = number(&body["width"]).unwrap_or_default();
    let height = number(&body["height"]).unwrap_or_default();
    let content_width = number(&body["contentWidth"]).unwrap_or_default();
    let content_height = number(&body["contentHeight"]).unwrap_or_default();
    if width <= 0.0
        || width > 100_000.0
        || height <= 0.0
        || height
            > if renderer == RENDERER_V2 {
                280.0
            } else {
                150.0
            }
    {
        return Err(ProgressPanelError::InvalidLayout(
            if width <= 0.0 || width > 100_000.0 {
                "width"
            } else {
                "height"
            },
        ));
    }
    let fits = body["fits"]
        .as_bool()
        .ok_or(ProgressPanelError::InvalidLayout("fits"))?;
    let reason = &body["reason"];
    if (!reason.is_null() && !matches!(reason.as_str(), Some("overflow" | "unsupported")))
        || (fits
            && (!reason.is_null() || content_width > width + 0.5 || content_height > height + 0.5))
        || (!fits && reason.is_null())
    {
        return Err(ProgressPanelError::InvalidLayout(if fits {
            "fits"
        } else {
            "reason"
        }));
    }
    if renderer == RENDERER_V2 {
        for (field, dimension, content) in [
            ("overflowX", width, content_width),
            ("overflowY", height, content_height),
        ] {
            let value = number(&body[field]).ok_or(ProgressPanelError::InvalidLayout(field))?;
            if value < 0.0 || (value - (content - dimension).max(0.0)).abs() > 0.5 {
                return Err(ProgressPanelError::InvalidLayout(field));
            }
        }
        let total = body["totalLines"]
            .as_u64()
            .ok_or(ProgressPanelError::InvalidLayout("totalLines"))?;
        let visible = body["visibleLines"]
            .as_u64()
            .ok_or(ProgressPanelError::InvalidLayout("visibleLines"))?;
        if total > 2048 || visible > total || (fits && visible != total) {
            return Err(ProgressPanelError::InvalidLayout("visibleLines"));
        }
        for field in ["lastVisibleLine", "lastVisibleHeading"] {
            if !body[field].is_null()
                && body[field]
                    .as_str()
                    .is_none_or(|value| value.chars().count() > 200)
            {
                return Err(ProgressPanelError::InvalidLayout(field));
            }
        }
    }
    let mut report = Map::new();
    for (key, value) in object {
        if key != "agent" && key != "revision" {
            report.insert(key.clone(), value.clone());
        }
    }
    Ok(report)
}

fn number(value: &Value) -> Option<f64> {
    value.as_f64().filter(|number| number.is_finite())
}

fn required_string<'a>(value: &'a Value, key: &'static str) -> Result<&'a str, ProgressPanelError> {
    value[key]
        .as_str()
        .ok_or(ProgressPanelError::InvalidLayout(key))
}

fn fresh_reports(value: &Value, now: f64) -> Vec<Value> {
    value
        .get("reports")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|row| {
            let measured = row.get("measuredAt").and_then(Value::as_f64);
            measured.is_some_and(|measured| {
                measured.is_finite() && now - measured >= 0.0 && now - measured < FRESH_SECONDS
            })
        })
        .filter(|row| {
            let Some(renderer) = row.get("renderer").and_then(Value::as_str) else {
                return false;
            };
            let mut body = row.as_object().cloned().unwrap_or_default();
            body.remove("measuredAt");
            body.remove("expiresAt");
            let mut full = body;
            full.insert("agent".into(), json!("feedback"));
            full.insert("revision".into(), json!("feedback"));
            validate_report(&Value::Object(full)).is_ok()
                && matches!(renderer, RENDERER_V1 | RENDERER_V2)
        })
        .cloned()
        .collect()
}

fn status(reports: &[Value]) -> &'static str {
    if reports.is_empty() {
        return "unmeasured";
    }
    if reports.iter().any(|row| {
        row.get("reason").and_then(Value::as_str) == Some("unsupported")
            || (row.get("renderer").and_then(Value::as_str) == Some(RENDERER_V1)
                && row.get("fits").and_then(Value::as_bool) == Some(false))
    }) {
        "does_not_fit"
    } else if reports
        .iter()
        .all(|row| row.get("fits").and_then(Value::as_bool) == Some(true))
    {
        "fits"
    } else {
        "clipped"
    }
}

fn read_layout(path: &Path) -> Result<Value, ProgressPanelError> {
    let mut file = match open_regular(path, false) {
        Ok(file) => file,
        Err(ProgressPanelError::Io(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(json!({}));
        }
        Err(error) => return Err(error),
    };
    let metadata = file.metadata()?;
    if metadata.len() > MAX_LAYOUT_BYTES as u64 {
        return Err(ProgressPanelError::InvalidStoredLayout);
    }
    let mut bytes = Vec::new();
    Read::by_ref(&mut file)
        .take((MAX_LAYOUT_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    if bytes.len() > MAX_LAYOUT_BYTES {
        return Err(ProgressPanelError::InvalidStoredLayout);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| ProgressPanelError::InvalidStoredLayout)?;
    validate_stored_layout(&value)?;
    Ok(value)
}

fn validate_stored_layout(value: &Value) -> Result<(), ProgressPanelError> {
    let object = value
        .as_object()
        .ok_or(ProgressPanelError::InvalidStoredLayout)?;
    let mut keys = object.keys().map(String::as_str).collect::<Vec<_>>();
    keys.sort_unstable();
    if keys
        != [
            "agent",
            "reports",
            "revision",
            "sha256",
            "status",
            "updatedAt",
            "version",
        ]
        || object.get("version").and_then(Value::as_u64) != Some(1)
        || object
            .get("agent")
            .and_then(Value::as_str)
            .is_none_or(|agent| {
                agent.is_empty()
                    || agent.len() > 200
                    || !agent
                        .bytes()
                        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
            })
        || object
            .get("revision")
            .and_then(Value::as_str)
            .is_none_or(|revision| revision.is_empty() || revision.len() > 200)
        || object
            .get("sha256")
            .and_then(Value::as_str)
            .is_none_or(|digest| {
                digest.len() != 64
                    || !digest
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
        || !matches!(
            object.get("status").and_then(Value::as_str),
            Some("fits" | "does_not_fit" | "unmeasured" | "clipped")
        )
        || object.get("updatedAt").and_then(number).is_none()
    {
        return Err(ProgressPanelError::InvalidStoredLayout);
    }
    let reports = object
        .get("reports")
        .and_then(Value::as_array)
        .filter(|reports| reports.len() <= MAX_CLIENTS)
        .ok_or(ProgressPanelError::InvalidStoredLayout)?;
    let mut clients = std::collections::HashSet::new();
    for row in reports {
        let report = row
            .as_object()
            .ok_or(ProgressPanelError::InvalidStoredLayout)?;
        let measured = report.get("measuredAt").and_then(number);
        let expires = report.get("expiresAt").and_then(number);
        if measured.is_none_or(|value| value < 0.0)
            || expires.is_none_or(|value| value < measured.unwrap_or_default())
        {
            return Err(ProgressPanelError::InvalidStoredLayout);
        }
        let mut full = report.clone();
        full.remove("measuredAt");
        full.remove("expiresAt");
        full.insert("agent".to_owned(), object["agent"].clone());
        full.insert("revision".to_owned(), object["revision"].clone());
        let parsed = validate_report(&Value::Object(full))
            .map_err(|_| ProgressPanelError::InvalidStoredLayout)?;
        let client = parsed
            .get("client")
            .and_then(Value::as_str)
            .ok_or(ProgressPanelError::InvalidStoredLayout)?;
        if !clients.insert(client.to_owned()) {
            return Err(ProgressPanelError::InvalidStoredLayout);
        }
    }
    Ok(())
}

fn open_regular(path: &Path, create: bool) -> Result<File, ProgressPanelError> {
    match fs::symlink_metadata(path) {
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err(std::io::Error::other("file must be regular").into());
            }
        }
        Err(error) if create && error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(create)
        .create(create)
        .truncate(false);
    #[cfg(unix)]
    if create {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options.open(path)?;
    let opened = file.metadata()?;
    let after = fs::symlink_metadata(path)?;
    if !opened.is_file()
        || after.file_type().is_symlink()
        || !after.is_file()
        || !same_file(&opened, &after)
    {
        return Err(std::io::Error::other("file must be regular").into());
    }
    Ok(file)
}

fn check_directory(directory: &Path) -> Result<(), ProgressPanelError> {
    let metadata = fs::symlink_metadata(directory)?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(std::io::Error::other("progress directory must be regular").into());
    }
    Ok(())
}

fn atomic_replace(
    directory: &Path,
    target: &Path,
    progress: &Path,
    expected_revision: &str,
    bytes: &[u8],
) -> Result<(), ProgressPanelError> {
    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o444))?;
    }
    let temporary_path = temporary.into_temp_path();
    let current = read_file(progress).map_err(|_| ProgressPanelError::ChangedWhileSaving)?;
    if current.map(|(_, revision, _)| revision).as_deref() != Some(expected_revision) {
        return Err(ProgressPanelError::ChangedWhileSaving);
    }
    fs::rename(&temporary_path, target)?;
    #[cfg(unix)]
    File::open(directory)?.sync_all()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{ProgressPanelError, atomic_replace, read_progress, record_layout};
    use serde_json::{Value, json};
    use std::fs;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_ROOT: AtomicU64 = AtomicU64::new(0);

    struct TempRoot(PathBuf);

    impl TempRoot {
        fn path(&self) -> &std::path::Path {
            &self.0
        }
    }

    impl Drop for TempRoot {
        fn drop(&mut self) {
            match fs::remove_dir_all(&self.0) {
                Ok(()) | Err(_) => {}
            }
        }
    }

    fn fixture() -> TempRoot {
        let root = std::env::temp_dir().join(format!(
            "rust-progress-panel-{}-{}",
            std::process::id(),
            NEXT_ROOT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(root.join("progress/agent-a")).expect("agent dir creates");
        TempRoot(root)
    }

    fn report(revision: &str, sequence: u64) -> Value {
        json!({
            "agent":"agent-a", "revision":revision, "client":"desktop", "sequence":sequence,
            "renderer":"progress-markdown-v1", "width":320, "height":150,
            "contentWidth":100, "contentHeight":40, "fits":true, "reason":null,
        })
    }

    #[test]
    fn reads_python_progress_shape_and_refuses_oversize_or_invalid_utf8_without_truncating() {
        let root = fixture();
        let path = root.path().join("progress/agent-a/PROGRESS.md");
        fs::write(&path, "# Python\n\n- visible\n").expect("progress writes");
        let panel = read_progress(root.path(), "agent-a").expect("panel reads");
        assert_eq!(panel["markdown"], "# Python\n\n- visible\n");
        assert!(panel["exists"].as_bool().unwrap_or(false));
        assert!(panel["revision"].as_str().is_some());

        let bytes = vec![b'x'; 128 * 1024 + 1];
        fs::write(&path, &bytes).expect("large content writes");
        let panel = read_progress(root.path(), "agent-a").expect("large file returns panel error");
        assert!(panel["error"].as_str().is_some());
        assert!(panel["exists"].as_bool().unwrap_or(false));
        assert_eq!(fs::read(&path).expect("large file retained"), bytes);

        fs::write(&path, [0xff]).expect("invalid UTF-8 writes");
        let panel =
            read_progress(root.path(), "agent-a").expect("invalid UTF-8 returns panel error");
        assert_eq!(panel["markdown"], "");
        assert_eq!(panel["revision"], Value::Null);
        assert!(panel["exists"].as_bool().unwrap_or(false));
    }

    #[test]
    fn missing_file_is_read_only_and_agent_path_is_confined() {
        let root = fixture();
        let panel = read_progress(root.path(), "agent-a").expect("missing file is empty");
        assert!(!panel["exists"].as_bool().unwrap_or(true));
        assert!(!root.path().join("progress/agent-a/PROGRESS.md").exists());
        assert!(matches!(
            read_progress(root.path(), "../other"),
            Err(ProgressPanelError::InvalidAgent)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_progress_directories_before_creating_a_lock() {
        let root = fixture();
        let progress = root.path().join("progress");
        let agent_dir = progress.join("agent-a");
        let outside = root.path().join("outside");
        fs::create_dir(&outside).expect("outside directory creates");
        fs::write(outside.join("PROGRESS.md"), "# Outside\n").expect("outside file writes");
        fs::remove_dir(&agent_dir).expect("fixture agent directory removed");
        std::os::unix::fs::symlink(&outside, &agent_dir).expect("agent symlink creates");
        let revision = "0:0:0:0:0";
        assert!(record_layout(root.path(), &report(revision, 1)).is_err());
        assert!(!outside.join(".layout.lock").exists());
    }

    #[test]
    fn writes_atomic_layout_and_identical_retry_does_not_refresh_measurement() {
        let root = fixture();
        let path = root.path().join("progress/agent-a/PROGRESS.md");
        fs::write(&path, "# Status\n").expect("progress writes");
        let revision = read_progress(root.path(), "agent-a").expect("progress reads")["revision"]
            .as_str()
            .expect("revision is string")
            .to_owned();
        let first = record_layout(root.path(), &report(&revision, 1)).expect("layout writes");
        let repeated =
            record_layout(root.path(), &report(&revision, 1)).expect("retry is idempotent");
        assert_eq!(first, repeated);
        let stored: Value = serde_json::from_slice(
            &fs::read(path.with_file_name("PROGRESS.layout.json")).expect("layout file exists"),
        )
        .expect("layout file parses");
        assert_eq!(stored["reports"][0]["sequence"], 1);
        assert_eq!(stored["sha256"].as_str().unwrap_or_default().len(), 64);
    }

    #[test]
    fn stale_revision_and_invalid_report_do_not_change_existing_file() {
        let root = fixture();
        let path = root.path().join("progress/agent-a/PROGRESS.md");
        fs::write(&path, "# Status\n").expect("progress writes");
        assert!(matches!(
            record_layout(root.path(), &report("stale", 1)),
            Err(ProgressPanelError::StaleRevision)
        ));
        let mut invalid = report("revision", 1);
        invalid["fits"] = json!(false);
        assert!(matches!(
            record_layout(root.path(), &invalid),
            Err(ProgressPanelError::InvalidLayout(_))
        ));
        assert!(!path.with_file_name("PROGRESS.layout.json").exists());
    }

    #[test]
    fn unreadable_current_progress_is_a_stale_conflict() {
        let root = fixture();
        let path = root.path().join("progress/agent-a/PROGRESS.md");
        fs::write(&path, "# Status\n").expect("progress writes");
        let revision = read_progress(root.path(), "agent-a").expect("progress reads")["revision"]
            .as_str()
            .expect("revision is string")
            .to_owned();
        fs::write(&path, [0xff]).expect("invalid UTF-8 writes");
        assert!(matches!(
            record_layout(root.path(), &report(&revision, 1)),
            Err(ProgressPanelError::StaleRevision)
        ));
        assert!(!path.with_file_name("PROGRESS.layout.json").exists());
    }

    #[test]
    fn final_revision_check_preserves_previous_layout() {
        let root = fixture();
        let progress = root.path().join("progress/agent-a/PROGRESS.md");
        let directory = progress.parent().expect("agent directory");
        fs::write(&progress, "# Original\n").expect("progress writes");
        let revision = read_progress(root.path(), "agent-a").expect("progress reads")["revision"]
            .as_str()
            .expect("revision is string")
            .to_owned();
        let target = progress.with_file_name("PROGRESS.layout.json");
        fs::write(&target, b"previous layout").expect("previous feedback writes");
        fs::write(&progress, "# Changed during save\n").expect("progress changes");
        assert!(matches!(
            atomic_replace(directory, &target, &progress, &revision, b"stale layout"),
            Err(ProgressPanelError::ChangedWhileSaving)
        ));
        assert_eq!(
            fs::read(&target).expect("previous feedback remains"),
            b"previous layout"
        );
    }

    #[test]
    fn invalid_stored_layout_is_refused_without_replacing_it() {
        let root = fixture();
        let path = root.path().join("progress/agent-a/PROGRESS.md");
        fs::write(&path, "# Status\n").expect("progress writes");
        let revision = read_progress(root.path(), "agent-a").expect("progress reads")["revision"]
            .as_str()
            .expect("revision is string")
            .to_owned();
        let layout = path.with_file_name("PROGRESS.layout.json");
        fs::write(&layout, b"[]").expect("invalid stored layout writes");
        assert!(matches!(
            record_layout(root.path(), &report(&revision, 1)),
            Err(ProgressPanelError::InvalidStoredLayout)
        ));
        assert_eq!(fs::read(&layout).expect("layout remains"), b"[]");
    }

    #[test]
    fn reads_layout_json_written_by_python_and_preserves_sequence_rules() {
        let root = fixture();
        let path = root.path().join("progress/agent-a/PROGRESS.md");
        fs::write(&path, "# Status\n").expect("progress writes");
        let panel = read_progress(root.path(), "agent-a").expect("progress reads");
        let revision = panel["revision"].as_str().expect("revision is string");
        let report = report(revision, 2);
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock is after the Unix epoch")
            .as_secs_f64();
        let stored = json!({
            "version":1, "agent":"agent-a", "revision":revision,
            "sha256":studio_runtime_store::sha256_hex(b"# Status\n"),
            "status":"fits", "updatedAt":now,
            "reports":[{"client":"phone", "sequence":1, "renderer":"progress-markdown-v1",
                "width":320, "height":150, "contentWidth":100, "contentHeight":40,
                "fits":true, "reason":null, "measuredAt":now, "expiresAt":now+90.0}],
        });
        fs::write(
            path.with_file_name("PROGRESS.layout.json"),
            serde_json::to_vec(&stored).expect("layout serializes"),
        )
        .expect("Python format layout file writes");
        let updated = record_layout(root.path(), &report).expect("Python feedback is retained");
        assert_eq!(updated["reports"].as_array().expect("report list").len(), 2);
        assert_eq!(updated["reports"][0]["client"], "phone");
        assert_eq!(updated["reports"][1]["sequence"], 2);
    }

    #[test]
    fn helper_paths_are_agent_scoped() {
        let root = std::path::Path::new("/tmp/example");
        assert!(
            super::progress_path(root, "agent-a")
                .expect("valid id")
                .ends_with("progress/agent-a/PROGRESS.md")
        );
    }
}
