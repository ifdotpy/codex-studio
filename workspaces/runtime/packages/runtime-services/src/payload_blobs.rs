//! Content-addressed blobs used by large checkpoint and tool payloads.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::de::{MapAccess, SeqAccess, Visitor};
use serde::ser::{SerializeMap, SerializeSeq};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

const EXTERNALIZE_THRESHOLD: usize = 64 * 1024;
const TASK_PREVIEW_BYTES: usize = 512;

#[derive(Clone, Debug, PartialEq)]
enum OrderedValue {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(String),
    Array(Vec<Self>),
    Object(Vec<(String, Self)>),
}

impl OrderedValue {
    fn from_value(value: &Value) -> Self {
        match value {
            Value::Null => Self::Null,
            Value::Bool(value) => Self::Bool(*value),
            Value::Number(value) => Self::Number(value.clone()),
            Value::String(value) => Self::String(value.clone()),
            Value::Array(values) => Self::Array(values.iter().map(Self::from_value).collect()),
            Value::Object(values) => Self::Object(
                values
                    .iter()
                    .map(|(key, value)| (key.clone(), Self::from_value(value)))
                    .collect(),
            ),
        }
    }

    fn into_value(self) -> Value {
        match self {
            Self::Null => Value::Null,
            Self::Bool(value) => Value::Bool(value),
            Self::Number(value) => Value::Number(value),
            Self::String(value) => Value::String(value),
            Self::Array(values) => Value::Array(values.into_iter().map(Self::into_value).collect()),
            Self::Object(values) => Value::Object(
                values
                    .into_iter()
                    .map(|(key, value)| (key, value.into_value()))
                    .collect(),
            ),
        }
    }

    fn get(&self, name: &str) -> Option<&Self> {
        match self {
            Self::Object(values) => values
                .iter()
                .find_map(|(key, value)| (key == name).then_some(value)),
            _ => None,
        }
    }
}

impl Serialize for OrderedValue {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        match self {
            Self::Null => serializer.serialize_unit(),
            Self::Bool(value) => serializer.serialize_bool(*value),
            Self::Number(value) => value.serialize(serializer),
            Self::String(value) => serializer.serialize_str(value),
            Self::Array(values) => {
                let mut sequence = serializer.serialize_seq(Some(values.len()))?;
                for value in values {
                    sequence.serialize_element(value)?;
                }
                sequence.end()
            }
            Self::Object(values) => {
                let mut map = serializer.serialize_map(Some(values.len()))?;
                for (key, value) in values {
                    map.serialize_entry(key, value)?;
                }
                map.end()
            }
        }
    }
}

impl<'de> Deserialize<'de> for OrderedValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct OrderedValueVisitor;

        impl<'de> Visitor<'de> for OrderedValueVisitor {
            type Value = OrderedValue;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedValue::Null)
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(OrderedValue::Null)
            }

            fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
                Ok(OrderedValue::Bool(value))
            }

            fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
                Ok(OrderedValue::Number(value.into()))
            }

            fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
                Ok(OrderedValue::Number(value.into()))
            }

            fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                serde_json::Number::from_f64(value)
                    .map(OrderedValue::Number)
                    .ok_or_else(|| E::custom("non-finite JSON number"))
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
                Ok(OrderedValue::String(value.to_owned()))
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                Ok(OrderedValue::String(value))
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: SeqAccess<'de>,
            {
                let mut values = Vec::new();
                while let Some(value) = sequence.next_element()? {
                    values.push(value);
                }
                Ok(OrderedValue::Array(values))
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut values: Vec<(String, OrderedValue)> = Vec::new();
                while let Some((key, value)) = map.next_entry::<String, OrderedValue>()? {
                    if let Some((_, previous)) = values.iter_mut().find(|(name, _)| *name == key) {
                        *previous = value;
                    } else {
                        values.push((key, value));
                    }
                }
                Ok(OrderedValue::Object(values))
            }
        }

        deserializer.deserialize_any(OrderedValueVisitor)
    }
}

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
    decide_externalize_ordered(table, OrderedValue::from_value(input))
}

/// Preserve source object order when the caller still has the stored JSON text.
pub fn decide_externalize_json(
    table: &str,
    input: &str,
) -> Result<ExternalizeDecision, PayloadBlobError> {
    let input = serde_json::from_str::<OrderedValue>(input)?;
    decide_externalize_ordered(table, input)
}

fn decide_externalize_ordered(
    table: &str,
    input: OrderedValue,
) -> Result<ExternalizeDecision, PayloadBlobError> {
    let OrderedValue::Object(entries) = input else {
        return Err(PayloadBlobError::InvalidRecord);
    };
    let mut record: Map<String, Value> = entries
        .iter()
        .map(|(key, value)| (key.clone(), value.clone().into_value()))
        .collect();
    let fields: &[&str] = match table {
        "checkpoints" => &["items"],
        "tool_requests" => &["result"],
        _ => &[],
    };
    let mut refs = payload_refs(&record)?;
    let mut writes = Vec::new();
    for field in fields {
        let Some(value) = entries
            .iter()
            .find_map(|(key, value)| (key == field).then_some(value))
        else {
            continue;
        };
        let bytes = serde_json::to_vec(&value)?;
        if bytes.len() <= EXTERNALIZE_THRESHOLD {
            refs.remove(*field);
            continue;
        }
        let preview = match &value {
            OrderedValue::Array(items) => OrderedValue::Array(
                items
                    .iter()
                    .take(1)
                    .map(|item| small_preview(item, 0))
                    .collect(),
            )
            .into_value(),
            OrderedValue::Object(_) => {
                let mut selected = Vec::new();
                for key in ["success", "outcome", "stage"] {
                    if let Some(value) = value.get(key) {
                        selected.push((key.to_owned(), value.clone()));
                    }
                }
                if let Some(OrderedValue::Array(items)) = value.get("contentItems") {
                    selected.push((
                        "contentItems".into(),
                        OrderedValue::Array(
                            items
                                .iter()
                                .take(1)
                                .map(|item| small_preview(item, 0))
                                .collect(),
                        ),
                    ));
                }
                small_preview(&OrderedValue::Object(selected), 0).into_value()
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
    } else {
        record.remove("_payloadBlobs");
    }
    Ok(ExternalizeDecision {
        record: Value::Object(record),
        writes,
    })
}

/// Externalize tool-result content independently of the table record shape.
pub fn decide_externalize_result(input: &Value) -> Result<ExternalizeDecision, PayloadBlobError> {
    decide_externalize_result_ordered(OrderedValue::from_value(input))
}

/// Externalize a result from raw JSON while preserving any nested object order.
pub fn decide_externalize_result_json(
    input: &str,
) -> Result<ExternalizeDecision, PayloadBlobError> {
    decide_externalize_result_ordered(serde_json::from_str::<OrderedValue>(input)?)
}

fn decide_externalize_result_ordered(
    input: OrderedValue,
) -> Result<ExternalizeDecision, PayloadBlobError> {
    let OrderedValue::Object(entries) = input else {
        return Err(PayloadBlobError::InvalidRecord);
    };
    let mut record: Map<String, Value> = entries
        .iter()
        .map(|(key, value)| (key.clone(), value.clone().into_value()))
        .collect();
    let Some(contents) = entries
        .iter()
        .find_map(|(key, value)| (key == "contentItems").then_some(value))
    else {
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
        OrderedValue::Array(items) => OrderedValue::Array(
            items
                .iter()
                .take(1)
                .map(|item| small_preview(item, 0))
                .collect(),
        )
        .into_value(),
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
    let bytes = read_blob_without_symlinks(state_root, &reference)?;
    if bytes.len() as u64 != reference.size || hash_hex(&bytes) != reference.sha256 {
        return Err(PayloadBlobError::Integrity);
    }
    Ok(bytes)
}

#[cfg(unix)]
fn read_blob_without_symlinks(
    state_root: &Path,
    reference: &BlobRef,
) -> Result<Vec<u8>, PayloadBlobError> {
    use rustix::fs::{Mode, OFlags, open, openat};

    let directory_flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW;
    let root = open(state_root, directory_flags, Mode::empty()).map_err(std::io::Error::from)?;
    let blobs =
        openat(&root, "blobs", directory_flags, Mode::empty()).map_err(std::io::Error::from)?;
    let first = openat(
        &blobs,
        &reference.sha256[..2],
        directory_flags,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let second = openat(
        &first,
        &reference.sha256[2..4],
        directory_flags,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let descriptor = openat(
        &second,
        &reference.sha256,
        OFlags::RDONLY | OFlags::NOFOLLOW,
        Mode::empty(),
    )
    .map_err(std::io::Error::from)?;
    let mut file = File::from(descriptor);
    if !file.metadata()?.is_file() {
        return Err(PayloadBlobError::Integrity);
    }
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes)?;
    Ok(bytes)
}

#[cfg(not(unix))]
fn read_blob_without_symlinks(
    state_root: &Path,
    reference: &BlobRef,
) -> Result<Vec<u8>, PayloadBlobError> {
    let root = state_root.join("blobs");
    let first = root.join(&reference.sha256[..2]);
    let second = first.join(&reference.sha256[2..4]);
    let target = second.join(&reference.sha256);
    for path in [&root, &first, &second, &target] {
        if fs::symlink_metadata(path)?.file_type().is_symlink() {
            return Err(PayloadBlobError::Integrity);
        }
    }
    fs::read(target).map_err(Into::into)
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

fn small_preview(value: &OrderedValue, depth: usize) -> OrderedValue {
    match value {
        OrderedValue::String(text) => {
            let mut end = text.len().min(TASK_PREVIEW_BYTES);
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            let mut preview = text[..end].to_owned();
            if end < text.len() {
                preview.push('…');
            }
            OrderedValue::String(preview)
        }
        OrderedValue::Array(values) => OrderedValue::Array(
            values
                .iter()
                .take(1)
                .map(|item| small_preview(item, depth + 1))
                .collect(),
        ),
        OrderedValue::Object(_) if depth >= 4 => OrderedValue::String("[preview omitted]".into()),
        OrderedValue::Object(values) => OrderedValue::Object(
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
        let mut corrupt = decision.writes[0].bytes.clone();
        corrupt[0] ^= 1;
        std::fs::write(path, corrupt).expect("corrupt test file");
        assert!(matches!(
            load_blob(root.path(), reference),
            Err(PayloadBlobError::Integrity)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn blob_storage_refuses_symlink_directories_and_targets() {
        use std::os::unix::fs::symlink;

        let state = tempfile::tempdir().expect("state root");
        let outside = tempfile::tempdir().expect("outside directory");
        let decision = decide_externalize("checkpoints", &json!({"items":["x".repeat(70_000)]}))
            .expect("decision");
        let hash = &decision.writes[0].sha256;
        let blob_root = state.path().join("blobs");
        std::fs::create_dir(&blob_root).expect("blob root");
        symlink(outside.path(), blob_root.join(&hash[..2])).expect("directory symlink");
        assert!(write_blobs(state.path(), &decision.writes).is_err());
        assert_eq!(
            std::fs::read_dir(outside.path())
                .expect("outside entries")
                .count(),
            0
        );

        let state = tempfile::tempdir().expect("second state root");
        let outside = tempfile::tempdir().expect("second outside directory");
        let decision = decide_externalize("checkpoints", &json!({"items":["y".repeat(70_000)]}))
            .expect("second decision");
        let hash = &decision.writes[0].sha256;
        let target = state
            .path()
            .join("blobs")
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(hash);
        std::fs::create_dir_all(target.parent().expect("target parent")).expect("parents");
        let outside_file = outside.path().join("payload");
        std::fs::write(&outside_file, &decision.writes[0].bytes).expect("outside payload");
        symlink(outside_file, &target).expect("target symlink");
        assert!(write_blobs(state.path(), &decision.writes).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn blob_reads_refuse_symlinked_parent_and_target_components() {
        use std::os::unix::fs::symlink;

        let source = json!({"items":["z".repeat(70_000)]});
        let decision = decide_externalize("checkpoints", &source).expect("decision");
        let reference = &decision.record["_payloadBlobs"]["items"];
        let hash = reference["sha256"].as_str().expect("hash");

        let state = tempfile::tempdir().expect("parent-symlink state");
        let outside = tempfile::tempdir().expect("parent-symlink target");
        let outside_shard = outside.path().join(&hash[..2]);
        std::fs::create_dir(&outside_shard).expect("outside shard");
        let outside_second = outside_shard.join(&hash[2..4]);
        std::fs::create_dir(&outside_second).expect("outside second shard");
        std::fs::write(outside_second.join(hash), &decision.writes[0].bytes).expect("outside blob");
        let blob_root = state.path().join("blobs");
        std::fs::create_dir(&blob_root).expect("blob root");
        symlink(&outside_shard, blob_root.join(&hash[..2])).expect("shard symlink");
        assert!(matches!(
            load_blob(state.path(), reference),
            Err(PayloadBlobError::Io(_)) | Err(PayloadBlobError::Integrity)
        ));

        let state = tempfile::tempdir().expect("target-symlink state");
        let outside = tempfile::tempdir().expect("target-symlink target");
        let target = state
            .path()
            .join("blobs")
            .join(&hash[..2])
            .join(&hash[2..4])
            .join(hash);
        std::fs::create_dir_all(target.parent().expect("target parent")).expect("parents");
        let outside_file = outside.path().join("payload");
        std::fs::write(&outside_file, &decision.writes[0].bytes).expect("outside payload");
        symlink(outside_file, &target).expect("target symlink");
        assert!(matches!(
            load_blob(state.path(), reference),
            Err(PayloadBlobError::Io(_)) | Err(PayloadBlobError::Integrity)
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

    #[test]
    fn small_payload_clears_its_old_reference_and_unknown_tables_are_unchanged() {
        let hash = "a".repeat(64);
        let small = json!({"items":["small"],"_payloadBlobs":{"items":{"sha256":hash,"size":1,"preview":[]}}});
        let decision = decide_externalize("checkpoints", &small).expect("decision");
        assert_eq!(decision.writes.len(), 0);
        assert!(decision.record.get("_payloadBlobs").is_none());

        let unknown = json!({"items":["x".repeat(70_000)]});
        let decision = decide_externalize("other", &unknown).expect("decision");
        assert_eq!(decision.record, unknown);
        assert!(decision.writes.is_empty());
    }

    #[test]
    fn object_and_result_previews_keep_only_the_supported_small_projection() {
        let value = json!({
            "success": true,
            "outcome": "ok",
            "stage": "done",
            "private": "x".repeat(70_000),
            "contentItems": ["first", "second"]
        });
        let decision = decide_externalize("tool_requests", &json!({"result": value, "id":"tool"}))
            .expect("tool request decision");
        assert_eq!(decision.record["result"]["success"], true);
        assert_eq!(decision.record["result"]["outcome"], "ok");
        assert_eq!(decision.record["result"]["stage"], "done");
        assert!(decision.record["result"].get("private").is_none());
        assert_eq!(decision.record["result"]["contentItems"], json!(["first"]));

        let source = json!({"contentItems":["small"]});
        let unchanged = decide_externalize_result(&source).expect("small result");
        assert_eq!(unchanged.record, source);
        assert!(unchanged.writes.is_empty());
        let absent = json!({"success":true});
        assert_eq!(
            decide_externalize_result(&absent)
                .expect("result without content")
                .record,
            absent
        );
    }
}
