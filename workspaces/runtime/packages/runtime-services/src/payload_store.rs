//! Persist uploaded payloads beside the state database and index them in SQLite.

use std::collections::HashSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::workspace_files;
use base64::Engine;
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, StoreError, WorkspaceFilesRepository, sha256_hex,
};
use thiserror::Error;
use uuid::Uuid;

const MAX_UPLOAD_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum PayloadStoreError {
    #[error("Invalid file data")]
    InvalidData,
    #[error("Invalid filename")]
    InvalidFilename,
    #[error("Files must contain 1 byte to 20 MiB")]
    ContentLimit,
    #[error("This upload id has different content")]
    UploadConflict,
    #[error("Unknown managed agent")]
    UnknownAgent,
    #[error("This agent was deleted")]
    DeletedAgent,
    #[error("Invalid stored object runtime_assets/{id}")]
    InvalidStoredAsset { id: String },
    #[error("Payload storage failed")]
    Io(#[from] std::io::Error),
    #[error("Payload storage database failed")]
    Store(#[from] StoreError),
    #[error("Payload storage JSON failed")]
    Json(#[from] serde_json::Error),
    #[error("A server timestamp could not be created")]
    Clock,
    #[error("An upload identity could not be created")]
    Entropy,
}

/// Store the decoded bytes first, then insert their record in the same writer
/// transaction. A crash between those steps leaves an unindexed directory;
/// the next serialized upload removes unindexed directories before writing.
pub fn upload(
    store: &RuntimeStore,
    agent_id: &str,
    name: &str,
    id: Option<&str>,
    encoded: &str,
) -> Result<Value, PayloadStoreError> {
    let stripped_name = crate::event_write::python_strip(name);
    if stripped_name.is_empty() || stripped_name.chars().count() > 255 {
        return Err(PayloadStoreError::InvalidFilename);
    }
    let name = Path::new(stripped_name)
        .file_name()
        .and_then(|part| part.to_str())
        .filter(|name| !name.is_empty() && *name != "." && *name != "..")
        .ok_or(PayloadStoreError::InvalidFilename)?
        .to_owned();
    let id = match id {
        Some(id) => Uuid::parse_str(id).map_err(|_| PayloadStoreError::InvalidData)?,
        None => {
            let mut bytes = [0_u8; 16];
            getrandom::fill(&mut bytes).map_err(|_| PayloadStoreError::Entropy)?;
            uuid::Builder::from_random_bytes(bytes).into_uuid()
        }
    };
    let id = id.to_string();
    validate_encoded_length(encoded)?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| PayloadStoreError::InvalidData)?;
    if bytes.is_empty() || bytes.len() > MAX_UPLOAD_BYTES {
        return Err(PayloadStoreError::ContentLimit);
    }

    let digest = sha256_hex(&bytes);
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|_| PayloadStoreError::Clock)?
        .as_secs_f64();
    let mime = workspace_files::mime_type(Path::new(&name));
    let image = is_image(&bytes);
    let mime = if mime.starts_with("image/") && !image {
        "application/octet-stream".to_owned()
    } else {
        mime
    };
    let directory = store.root().join("uploads").join(&id);
    let path = directory.join(&name);
    let path_string = path.to_string_lossy().into_owned();

    store.transaction_with(|transaction| {
        let agent = AgentRepository::get(transaction, &AgentId::from(agent_id))?
            .ok_or(PayloadStoreError::UnknownAgent)?;
        if crate::event_write::truthy(agent.extra.get("deletedAt")) {
            return Err(PayloadStoreError::DeletedAgent);
        }

        if let Some(existing) = WorkspaceFilesRepository::asset(transaction, &id)? {
            let value = existing.value().clone();
            let existing_id = existing.id().as_ref().to_owned();
            if !valid_asset_record(&value, &existing_id, store.root()) {
                return Err(PayloadStoreError::InvalidStoredAsset { id: existing_id });
            }
            let existing_agent = value.get("agent").and_then(Value::as_str);
            let existing_name = value.get("name").and_then(Value::as_str);
            let existing_hash = value.get("hash").and_then(Value::as_str);
            if existing_agent == Some(agent_id)
                && existing_name == Some(name.as_str())
                && existing_hash == Some(digest.as_str())
            {
                let mut view = value;
                if let Some(record) = view.as_object_mut() {
                    record.remove("path");
                }
                return Ok(view);
            }
            return Err(PayloadStoreError::UploadConflict);
        }

        remove_unindexed_uploads(store.root(), transaction)?;
        atomic_write(store.root(), &directory, &path, &bytes)?;
        let record = json!({
            "id": id,
            "agent": agent_id,
            "name": name,
            "path": path_string,
            "mime": mime,
            "image": image,
            "size": bytes.len(),
            "hash": digest,
            "created": created,
        });
        WorkspaceFilesRepository::insert_asset(transaction, &id, &record)?;
        let mut view = record;
        if let Some(record) = view.as_object_mut() {
            record.remove("path");
        }
        Ok(view)
    })
}

fn remove_unindexed_uploads(
    root: &Path,
    connection: &studio_runtime_store::Connection,
) -> Result<(), PayloadStoreError> {
    let uploads = root.join("uploads");
    let metadata = match fs::symlink_metadata(&uploads) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !metadata.is_dir() {
        return Err(std::io::Error::other("upload directory is not a directory").into());
    }
    let known = WorkspaceFilesRepository::asset_ids(connection)?
        .into_iter()
        .collect::<HashSet<_>>();
    for entry in fs::read_dir(uploads)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        let id = entry.file_name().to_string_lossy().to_string();
        if !known.contains(&id) {
            if file_type.is_dir() {
                fs::remove_dir_all(entry.path())?;
            } else if file_type.is_symlink() {
                fs::remove_file(entry.path())?;
            }
        }
    }
    Ok(())
}

fn atomic_write(
    root: &Path,
    directory: &Path,
    path: &Path,
    bytes: &[u8],
) -> Result<(), PayloadStoreError> {
    let uploads = root.join("uploads");
    create_directory_synced(&uploads, root)?;
    create_directory_synced(directory, &uploads)?;

    let mut temporary = tempfile::NamedTempFile::new_in(directory)?;
    temporary.write_all(bytes)?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    sync_directory(directory)?;
    Ok(())
}

fn create_directory_synced(path: &Path, parent: &Path) -> Result<(), PayloadStoreError> {
    match fs::create_dir(path) {
        Ok(()) => sync_directory(parent)?,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error.into()),
    }
    if !fs::symlink_metadata(path)?.is_dir() {
        return Err(std::io::Error::other("upload path is not a directory").into());
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

fn is_image(bytes: &[u8]) -> bool {
    bytes.starts_with(b"\x89PNG\r\n\x1a\n")
        || bytes.starts_with(b"\xff\xd8\xff")
        || bytes.starts_with(b"GIF87a")
        || bytes.starts_with(b"GIF89a")
        || (bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP")
}

fn validate_encoded_length(encoded: &str) -> Result<(), PayloadStoreError> {
    let length = encoded.len();
    if !length.is_multiple_of(4) {
        return Err(PayloadStoreError::InvalidData);
    }
    let padding = encoded
        .as_bytes()
        .iter()
        .rev()
        .take_while(|byte| **byte == b'=')
        .count();
    let decoded_length = length / 4 * 3 - padding.min(2);
    if decoded_length > MAX_UPLOAD_BYTES {
        return Err(PayloadStoreError::ContentLimit);
    }
    Ok(())
}

fn valid_asset_record(record: &Value, row_id: &str, root: &Path) -> bool {
    let Some(object) = record.as_object() else {
        return false;
    };
    let Some(name) = object.get("name").and_then(Value::as_str) else {
        return false;
    };
    let expected_path = root
        .join("uploads")
        .join(row_id)
        .join(name)
        .to_string_lossy()
        .into_owned();
    object.get("id").and_then(Value::as_str) == Some(row_id)
        && Uuid::parse_str(row_id).is_ok_and(|parsed| parsed.to_string() == row_id)
        && object
            .get("agent")
            .and_then(Value::as_str)
            .is_some_and(|agent| !agent.is_empty())
        && crate::event_write::python_strip(name) == name
        && name.chars().count() <= 255
        && Path::new(name).file_name().and_then(|part| part.to_str()) == Some(name)
        && Path::new(&expected_path).is_absolute()
        && object.get("path").and_then(Value::as_str) == Some(expected_path.as_str())
        && object
            .get("mime")
            .and_then(Value::as_str)
            .is_some_and(|mime| !mime.is_empty())
        && object.get("image").and_then(Value::as_bool).is_some()
        && object
            .get("size")
            .and_then(Value::as_u64)
            .is_some_and(|size| size > 0 && size <= MAX_UPLOAD_BYTES as u64)
        && object
            .get("hash")
            .and_then(Value::as_str)
            .is_some_and(|hash| {
                hash.len() == 64 && hash.bytes().all(|byte| byte.is_ascii_hexdigit())
            })
        && object
            .get("created")
            .and_then(Value::as_f64)
            .is_some_and(f64::is_finite)
}

#[cfg(test)]
mod tests {
    use std::fs;

    use serde_json::json;

    use super::{MAX_UPLOAD_BYTES, atomic_write, valid_asset_record, validate_encoded_length};

    #[test]
    fn atomic_write_handles_payload_name_and_syncs_new_directory_chain() {
        let root = tempfile::tempdir().expect("temporary state directory creates");
        let id = "8f319af0-59e9-4c6a-8ec8-e99c9e354092";
        let directory = root.path().join("uploads").join(id);
        let path = directory.join(".payload.tmp");

        atomic_write(root.path(), &directory, &path, b"payload")
            .expect("payload publishes with its legitimate filename");

        assert_eq!(fs::read(path).expect("payload reads"), b"payload");
    }

    #[test]
    fn encoded_size_is_rejected_before_base64_decode() {
        let encoded_bytes = MAX_UPLOAD_BYTES.div_ceil(3) * 4;
        let over_limit_unpadded = "A".repeat(encoded_bytes);
        assert!(matches!(
            validate_encoded_length(&over_limit_unpadded),
            Err(super::PayloadStoreError::ContentLimit)
        ));

        let at_limit = format!("{}=", "A".repeat(encoded_bytes - 1));
        assert!(validate_encoded_length(&at_limit).is_ok());
    }

    #[test]
    fn replay_validation_requires_the_full_stored_asset_record() {
        let root = tempfile::tempdir().expect("temporary state directory creates");
        let id = "8f319af0-59e9-4c6a-8ec8-e99c9e354092";
        let record = json!({
            "id": id,
            "agent": "agent",
            "name": "asset.txt",
            "path": root
                .path()
                .join("uploads")
                .join(id)
                .join("asset.txt")
                .to_string_lossy(),
            "mime": "text/plain",
            "image": false,
            "size": 7,
            "hash": "a".repeat(64),
            "created": 1.0,
        });
        assert!(valid_asset_record(&record, id, root.path()));
        for field in [
            "id", "agent", "name", "path", "mime", "image", "size", "hash", "created",
        ] {
            let mut corrupt = record.clone();
            corrupt.as_object_mut().expect("object").remove(field);
            assert!(
                !valid_asset_record(&corrupt, id, root.path()),
                "missing {field} is invalid"
            );
        }
        for (field, value) in [
            ("id", json!("not-a-uuid")),
            ("agent", json!(null)),
            ("name", json!("../asset.txt")),
            ("path", json!("/outside/asset.txt")),
            ("mime", json!("")),
            ("image", json!("false")),
            ("size", json!(0)),
            ("hash", json!("bad")),
            ("created", json!("now")),
        ] {
            let mut corrupt = record.clone();
            corrupt
                .as_object_mut()
                .expect("object")
                .insert(field.into(), value);
            assert!(
                !valid_asset_record(&corrupt, id, root.path()),
                "invalid {field} is refused"
            );
        }
    }
}
