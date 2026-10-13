//! Persist uploaded payloads beside the state database and index them in SQLite.

use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
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
    #[error("Invalid stored object assets")]
    InvalidStoredAsset,
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
    let name = Path::new(name)
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
        if agent.extra.get("deletedAt").is_some_and(truthy) {
            return Err(PayloadStoreError::DeletedAgent);
        }

        if let Some(existing) = WorkspaceFilesRepository::asset(transaction, &id)? {
            let value = serde_json::from_str::<Value>(&existing.record)
                .map_err(|_| PayloadStoreError::InvalidStoredAsset)?;
            if value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                return Err(PayloadStoreError::InvalidStoredAsset);
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
            if existing_agent.is_none() || existing_name.is_none() || existing_hash.is_none() {
                return Err(PayloadStoreError::InvalidStoredAsset);
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
    fs::create_dir_all(directory)?;
    let temporary = directory.join(".payload.tmp");
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&temporary)?;
    let result = (|| {
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        if path.exists() {
            fs::remove_file(path)?;
        }
        fs::rename(&temporary, path)?;
        sync_directory(directory)?;
        sync_directory(&uploads)?;
        Ok::<_, std::io::Error>(())
    })();
    if result.is_err() {
        let _cleanup_result = fs::remove_file(temporary);
    }
    result?;
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

fn truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(values) => !values.is_empty(),
        Value::Object(values) => !values.is_empty(),
    }
}
