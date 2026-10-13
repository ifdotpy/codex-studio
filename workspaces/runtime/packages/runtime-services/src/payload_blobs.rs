//! Content-addressed blobs used by large checkpoint and tool payloads.

use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

const EXTERNALIZE_THRESHOLD: usize = 64 * 1024;
const TASK_PREVIEW_BYTES: usize = 512;

#[derive(Debug, Error)]
pub enum PayloadBlobError {
    #[error("Invalid payload blob reference")]
    InvalidReference,
    #[error("Payload blob failed its size or digest check")]
    Integrity,
    #[error("Invalid stored payload record")]
    InvalidRecord,
    #[error("Payload blob filesystem operation failed")]
    Io(#[from] std::io::Error),
    #[error("Payload JSON operation failed")]
    Json(#[from] serde_json::Error),
}

#[derive(Clone, Debug, PartialEq)]
pub struct BlobWrite {
    pub sha256: String,
    pub bytes: Vec<u8>,
    pub preview: Value,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ExternalizeDecision {
    pub record: Value,
    pub writes: Vec<BlobWrite>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct BlobRef {
    sha256: String,
    size: u64,
}

impl BlobRef {
    fn parse(value: &Value) -> Result<Self, PayloadBlobError> {
        let hash = value
            .get("sha256")
            .and_then(Value::as_str)
            .filter(|hash| {
                hash.len() == 64
                    && hash
                        .bytes()
                        .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            })
            .ok_or(PayloadBlobError::InvalidReference)?;
        let size = value
            .get("size")
            .and_then(Value::as_u64)
            .ok_or(PayloadBlobError::InvalidReference)?;
        Ok(Self {
            sha256: hash.to_owned(),
            size,
        })
    }

    fn value(&self, preview: Value) -> Value {
        json!({"sha256": self.sha256, "size": self.size, "preview": preview})
    }
}

/// Decide the record projection and blob effects without touching the filesystem.
pub fn decide_externalize(
    table: &str,
    input: &Value,
) -> Result<ExternalizeDecision, PayloadBlobError> {
    let mut record = input
        .as_object()
        .cloned()
        .ok_or(PayloadBlobError::InvalidRecord)?;
    let fields: &[&str] = match table {
        "checkpoints" => &["items"],
        "tool_requests" => &["result"],
        _ => &[],
    };
    let mut refs = payload_refs(&record)?;
    let mut writes = Vec::new();
    for field in fields {
        let Some(value) = record.get(*field).cloned() else {
            continue;
        };
        let bytes = serde_json::to_vec(&value)?;
        if bytes.len() <= EXTERNALIZE_THRESHOLD {
            refs.remove(*field);
            continue;
        }
        let preview = match &value {
            Value::Array(items) => {
                small_preview(&Value::Array(items.iter().take(1).cloned().collect()), 0)
            }
            Value::Object(object) => {
                let mut selected = Map::new();
                for key in ["success", "outcome", "stage"] {
                    if let Some(value) = object.get(key) {
                        selected.insert(key.into(), value.clone());
                    }
                }
                if let Some(Value::Array(items)) = object.get("contentItems") {
                    selected.insert(
                        "contentItems".into(),
                        small_preview(&Value::Array(items.iter().take(1).cloned().collect()), 0),
                    );
                }
                small_preview(&Value::Object(selected), 0)
            }
            _ => Value::Null,
        };
        let hash = hash_hex(&bytes);
        let reference = BlobRef {
            sha256: hash.clone(),
            size: bytes.len() as u64,
        };
        refs.insert((*field).to_owned(), reference.value(preview.clone()));
        record.insert((*field).to_owned(), preview.clone());
        writes.push(BlobWrite {
            sha256: hash,
            bytes,
            preview,
        });
    }
    if !refs.is_empty() {
        record.insert("_payloadBlobs".into(), Value::Object(refs));
    }
    Ok(ExternalizeDecision {
        record: Value::Object(record),
        writes,
    })
}

/// Externalize tool-result content independently of the table record shape.
pub fn decide_externalize_result(input: &Value) -> Result<ExternalizeDecision, PayloadBlobError> {
    let mut record = input
        .as_object()
        .cloned()
        .ok_or(PayloadBlobError::InvalidRecord)?;
    let Some(contents) = record.get("contentItems").cloned() else {
        return Ok(ExternalizeDecision {
            record: Value::Object(record),
            writes: Vec::new(),
        });
    };
    let bytes = serde_json::to_vec(&contents)?;
    if bytes.len() <= EXTERNALIZE_THRESHOLD {
        return Ok(ExternalizeDecision {
            record: Value::Object(record),
            writes: Vec::new(),
        });
    }
    let preview = match &contents {
        Value::Array(items) => {
            small_preview(&Value::Array(items.iter().take(1).cloned().collect()), 0)
        }
        _ => Value::Null,
    };
    let hash = hash_hex(&bytes);
    let reference = BlobRef {
        sha256: hash.clone(),
        size: bytes.len() as u64,
    };
    let mut refs = payload_refs(&record)?;
    refs.insert("contentItems".into(), reference.value(preview.clone()));
    record.insert("contentItems".into(), preview.clone());
    record.insert("_payloadBlobs".into(), Value::Object(refs));
    Ok(ExternalizeDecision {
        record: Value::Object(record),
        writes: vec![BlobWrite {
            sha256: hash,
            bytes,
            preview,
        }],
    })
}

/// Publish each decision blob durably beneath `<state>/blobs`.
pub fn write_blobs(state_root: &Path, writes: &[BlobWrite]) -> Result<(), PayloadBlobError> {
    let root = state_root.join("blobs");
    ensure_directory(&root, state_root)?;
    for write in writes {
        let reference = BlobRef {
            sha256: write.sha256.clone(),
            size: write.bytes.len() as u64,
        };
        if hash_hex(&write.bytes) != reference.sha256 {
            return Err(PayloadBlobError::Integrity);
        }
        let directory = blob_path(&root, &reference)
            .parent()
            .map(Path::to_path_buf)
            .ok_or(PayloadBlobError::InvalidReference)?;
        let first_level = root.join(&reference.sha256[..2]);
        ensure_directory(&first_level, &root)?;
        ensure_directory(&directory, &first_level)?;
        let target = blob_path(&root, &reference);
        if target.exists() {
            verify_existing_blob(&target, &write.bytes, &reference.sha256)?;
            continue;
        }
        let mut temporary = tempfile::NamedTempFile::new_in(&directory)?;
        temporary.write_all(&write.bytes)?;
        temporary.as_file().sync_all()?;
        match temporary.persist_noclobber(&target) {
            Ok(_) => sync_directory(&directory)?,
            Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
                verify_existing_blob(&target, &write.bytes, &reference.sha256)?;
            }
            Err(error) => return Err(error.error.into()),
        }
    }
    Ok(())
}

pub fn load_blob(state_root: &Path, reference: &Value) -> Result<Vec<u8>, PayloadBlobError> {
    let reference = BlobRef::parse(reference)?;
    let bytes = fs::read(blob_path(&state_root.join("blobs"), &reference))?;
    if bytes.len() as u64 != reference.size || hash_hex(&bytes) != reference.sha256 {
        return Err(PayloadBlobError::Integrity);
    }
    Ok(bytes)
}

pub fn resolve_record(state_root: &Path, input: &Value) -> Result<Value, PayloadBlobError> {
    let mut record = input
        .as_object()
        .cloned()
        .ok_or(PayloadBlobError::InvalidRecord)?;
    if let Some(refs) = record.remove("_payloadBlobs") {
        let refs = refs.as_object().ok_or(PayloadBlobError::InvalidReference)?;
        for (field, reference) in refs {
            let bytes = load_blob(state_root, reference)?;
            record.insert(field.clone(), serde_json::from_slice(&bytes)?);
        }
    }
    Ok(Value::Object(record))
}

pub fn trim_task(record: &Value) -> Result<Value, PayloadBlobError> {
    let mut record = record
        .as_object()
        .cloned()
        .ok_or(PayloadBlobError::InvalidRecord)?;
    if let Some(Value::String(tail)) = record.get("tail") {
        let preview = task_preview(tail);
        if preview.len() < tail.len() {
            record.insert("tail".into(), Value::String(preview));
            record.insert("outputTruncated".into(), Value::Bool(true));
            record.insert("outputPreviewOnly".into(), Value::Bool(true));
        }
    }
    Ok(Value::Object(record))
}

pub fn task_preview(text: &str) -> String {
    if text.len() <= TASK_PREVIEW_BYTES {
        return text.to_owned();
    }
    let mut start = text.len() - TASK_PREVIEW_BYTES;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

fn small_preview(value: &Value, depth: usize) -> Value {
    match value {
        Value::String(text) => {
            let mut end = text.len().min(TASK_PREVIEW_BYTES);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let mut preview = text[..end].to_owned();
            if end < text.len() {
                preview.push('…');
            }
            Value::String(preview)
        }
        Value::Array(values) => Value::Array(
            values
                .iter()
                .take(1)
                .map(|item| small_preview(item, depth + 1))
                .collect(),
        ),
        Value::Object(values) if depth >= 4 => Value::String("[preview omitted]".into()),
        Value::Object(values) => Value::Object(
            values
                .iter()
                .take(8)
                .map(|(key, item)| (key.clone(), small_preview(item, depth + 1)))
                .collect(),
        ),
        other => other.clone(),
    }
}

fn blob_path(root: &Path, reference: &BlobRef) -> PathBuf {
    root.join(&reference.sha256[..2])
        .join(&reference.sha256[2..4])
        .join(&reference.sha256)
}

fn payload_refs(record: &Map<String, Value>) -> Result<Map<String, Value>, PayloadBlobError> {
    let Some(value) = record.get("_payloadBlobs") else {
        return Ok(Map::new());
    };
    let refs = value
        .as_object()
        .cloned()
        .ok_or(PayloadBlobError::InvalidReference)?;
    for reference in refs.values() {
        BlobRef::parse(reference)?;
    }
    Ok(refs)
}

fn verify_existing_blob(
    path: &Path,
    expected_bytes: &[u8],
    expected_hash: &str,
) -> Result<(), PayloadBlobError> {
    if !fs::symlink_metadata(path)?.is_file() {
        return Err(PayloadBlobError::Integrity);
    }
    let existing = fs::read(path)?;
    if existing != expected_bytes || hash_hex(&existing) != expected_hash {
        return Err(PayloadBlobError::Integrity);
    }
    Ok(())
}

fn ensure_directory(path: &Path, parent: &Path) -> Result<(), PayloadBlobError> {
    match fs::create_dir(path) {
        Ok(()) => sync_directory(parent)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(std::io::Error::other("payload path is not a directory").into());
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<(), std::io::Error> {
    #[cfg(unix)]
    File::open(path)?.sync_all()?;
    #[cfg(not(unix))]
    let _ = path;
    Ok(())
}

fn hash_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{
        PayloadBlobError, decide_externalize, decide_externalize_result, load_blob, resolve_record,
        task_preview, trim_task, write_blobs,
    };
    use serde_json::{Value, json};

    #[test]
    fn oversized_checkpoint_decision_is_pure_then_round_trips_durably() {
        let source = json!({"id":"checkpoint", "items":[{"text":"x".repeat(70_000)}]});
        let decision = decide_externalize("checkpoints", &source).expect("decision");
        assert_eq!(
            source["items"][0]["text"].as_str().expect("text").len(),
            70_000
        );
        assert_eq!(decision.writes.len(), 1);
        let preview_ref = &decision.record["_payloadBlobs"]["items"];
        let root = tempfile::tempdir().expect("state root");
        write_blobs(root.path(), &decision.writes).expect("blob write");
        let resolved = resolve_record(root.path(), &decision.record).expect("resolve");
        assert_eq!(resolved, source);
        assert_eq!(
            load_blob(root.path(), preview_ref).expect("validated read"),
            decision.writes[0].bytes
        );
    }

    #[test]
    fn result_preview_keeps_only_supported_summary_fields() {
        let source =
            json!({"success":true,"private":"x","contentItems":[{"text":"y".repeat(70_000)}]});
        let decision = decide_externalize_result(&source).expect("decision");
        assert_eq!(
            decision.record["_payloadBlobs"]["contentItems"]["preview"][0]["text"]
                .as_str()
                .expect("preview")
                .len(),
            515
        );
        assert_eq!(
            decision.record["contentItems"],
            decision.record["_payloadBlobs"]["contentItems"]["preview"]
        );
    }

    #[test]
    fn invalid_refs_and_corrupt_blob_bytes_fail_closed() {
        let root = tempfile::tempdir().expect("state root");
        assert!(matches!(
            load_blob(root.path(), &json!({"sha256":"bad","size":1})),
            Err(PayloadBlobError::InvalidReference)
        ));
        assert!(matches!(
            decide_externalize(
                "checkpoints",
                &json!({"items":["x".repeat(70_000)],"_payloadBlobs":{"old":{"sha256":"bad","size":1}}})
            ),
            Err(PayloadBlobError::InvalidReference)
        ));
        let decision = decide_externalize("checkpoints", &json!({"items":["a".repeat(70_000)]}))
            .expect("decision");
        write_blobs(root.path(), &decision.writes).expect("write");
        let reference = &decision.record["_payloadBlobs"]["items"];
        let hash = reference["sha256"].as_str().expect("hash");
        let path = root
            .path()
            .join("blobs")
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(hash);
        std::fs::write(path, b"corrupt").expect("corrupt test file");
        assert!(matches!(
            load_blob(root.path(), reference),
            Err(PayloadBlobError::Integrity)
        ));
    }

    #[test]
    fn task_preview_uses_a_utf8_safe_tail_and_marks_truncation() {
        let source = json!({"tail": format!("{}{}", "a".repeat(511), "💙")});
        let preview = task_preview(source["tail"].as_str().expect("tail"));
        assert!(preview.len() <= 512);
        let trimmed = trim_task(&source).expect("trim");
        assert_eq!(trimmed["outputTruncated"], Value::Bool(true));
        assert_eq!(trimmed["outputPreviewOnly"], Value::Bool(true));
    }
}
