//! Private, bounded audio-chunk storage for native voice sessions.

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, VoiceRepository};
use thiserror::Error;

use super::{VoiceError, VoiceRecordInput, policy};

const AUDIO_RETENTION: Duration = Duration::from_secs(7 * 24 * 60 * 60);

#[derive(Debug, Error)]
pub enum VoiceAudioError {
    #[error(transparent)]
    Request(#[from] policy::VoiceError),
    #[error(transparent)]
    Voice(#[from] VoiceError),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error("voice audio storage failed")]
    Io(#[from] std::io::Error),
}

/// Persist one synthetic or captured audio chunk and its transcript event.
/// The deterministic file name makes retries idempotent and detects identity
/// reuse with different bytes.
pub fn save_audio(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    chunk_id: &str,
    encoded_audio: &str,
    mime: &str,
) -> Result<Value, VoiceAudioError> {
    let connection = store.read()?;
    super::checked_agent(&connection, agent_id)?;
    let session = VoiceRepository::session_exists(&connection, agent_id, session_id)?;
    drop(connection);

    let request = policy::AudioChunkRequest {
        agent: studio_runtime_store::AgentId::from(agent_id),
        session_id: policy::SessionId::parse(session_id.to_owned())?,
        chunk_id: chunk_id.to_owned(),
        base64: encoded_audio.to_owned(),
        mime: mime.to_owned(),
    };

    let directory = store.root().join("voice-audio");
    ensure_audio_directory(&directory)?;
    let session_prefix = audio_session_prefix(agent_id, session_id);
    let validated = validate_then_prune(request.clone(), session, &directory, &session_prefix)?;

    let name = audio_name(agent_id, session_id, chunk_id);
    let path = directory.join(&name);
    let legacy_path = directory.join(legacy_audio_name(agent_id, session_id, chunk_id));
    let legacy_exists = legacy_path.exists();
    let existing = read_existing_chunk(&path)?.or(read_existing_chunk(&legacy_path)?);
    let decision = match existing.as_deref() {
        Some(bytes) => policy::decide_audio(request, session, Some(bytes))?,
        None => validated,
    };
    let chunk = match decision {
        policy::AudioDecision::Store(chunk) => {
            match create_private_file(&path, chunk.bytes()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    let existing = read_existing_chunk(&path)?.ok_or(error)?;
                    let _ = policy::decide_audio(
                        policy::AudioChunkRequest {
                            agent: studio_runtime_store::AgentId::from(agent_id),
                            session_id: policy::SessionId::parse(session_id.to_owned())?,
                            chunk_id: chunk_id.to_owned(),
                            base64: encoded_audio.to_owned(),
                            mime: mime.to_owned(),
                        },
                        true,
                        Some(&existing),
                    )?;
                }
                Err(error) => return Err(error.into()),
            }
            chunk
        }
        policy::AudioDecision::Reuse(chunk) => chunk,
    };

    Ok(super::record(
        store,
        VoiceRecordInput {
            agent_id: agent_id.to_owned(),
            session_id: session_id.to_owned(),
            event_id: format!(
                "audio:{}",
                legacy_audio_name(agent_id, session_id, chunk_id)
            ),
            kind: "event".to_owned(),
            text: "audio saved".to_owned(),
            item_id: String::new(),
            previous_item_id: String::new(),
            payload: Some(json!({
                "file": if legacy_exists { legacy_audio_name(agent_id, session_id, chunk_id) } else { name },
                "mime": chunk.mime()
            })),
            internal: false,
        },
    )?)
}

fn audio_name(agent: &str, session: &str, chunk: &str) -> String {
    format!(
        "{}{}",
        audio_session_prefix(agent, session),
        studio_runtime_store::sha256_hex(chunk.as_bytes())
    )
}

fn legacy_audio_name(agent: &str, session: &str, chunk: &str) -> String {
    studio_runtime_store::sha256_hex(format!("{agent}:{session}:{chunk}").as_bytes())
}

fn audio_session_prefix(agent: &str, session: &str) -> String {
    let mut identity = Vec::with_capacity(agent.len() + session.len() + 16);
    identity.extend_from_slice(&(agent.len() as u64).to_be_bytes());
    identity.extend_from_slice(agent.as_bytes());
    identity.extend_from_slice(&(session.len() as u64).to_be_bytes());
    identity.extend_from_slice(session.as_bytes());
    format!("s-{}-c-", studio_runtime_store::sha256_hex(&identity))
}

fn validate_then_prune(
    request: policy::AudioChunkRequest,
    session_exists: bool,
    directory: &Path,
    session_prefix: &str,
) -> Result<policy::AudioDecision, VoiceAudioError> {
    let validated = policy::decide_audio(request, session_exists, None)?;
    prune_expired_audio(directory, session_prefix)?;
    Ok(validated)
}

fn ensure_audio_directory(path: &Path) -> Result<(), std::io::Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
        Ok(_) => Err(std::io::Error::other("voice audio path is not a directory")),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            create_private_directory(path)
        }
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn create_private_directory(path: &Path) -> Result<(), std::io::Error> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = fs::DirBuilder::new();
    builder.mode(0o700);
    match builder.create(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_audio_directory(path)
        }
        Err(error) => Err(error),
    }
}

#[cfg(not(unix))]
fn create_private_directory(path: &Path) -> Result<(), std::io::Error> {
    match fs::create_dir(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            ensure_audio_directory(path)
        }
        Err(error) => Err(error),
    }
}

fn read_existing_chunk(path: &Path) -> Result<Option<Vec<u8>>, std::io::Error> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => fs::read(path).map(Some),
        Ok(_) => Err(std::io::Error::other(
            "voice audio entry is not a regular file",
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn create_private_file(path: &Path, bytes: &[u8]) -> Result<(), std::io::Error> {
    create_private_file_with(path, |output| {
        output.write_all(bytes)?;
        output.flush()?;
        output.sync_all()
    })
}

fn create_private_file_with(
    path: &Path,
    write: impl FnOnce(&mut fs::File) -> Result<(), std::io::Error>,
) -> Result<(), std::io::Error> {
    let parent = path
        .parent()
        .ok_or_else(|| std::io::Error::other("audio path has no parent"))?;
    static TEMP_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nonce = format!(
        "{}:{}:{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
        TEMP_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let temporary = parent.join(format!(
        ".voice-{}.tmp",
        uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, nonce.as_bytes())
    ));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut output = options.open(&temporary)?;
        write(&mut output)?;
        output.flush()?;
        output.sync_all()?;
        drop(output);
        // Hard-link publication is atomic and refuses to overwrite a racing
        // chunk with different bytes; both names are on the same filesystem.
        fs::hard_link(&temporary, path)?;
        fs::remove_file(&temporary)
    })();
    if result.is_err() {
        match fs::remove_file(&temporary) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(_) => {}
        }
    }
    result
}

fn prune_expired_audio(directory: &Path, session_prefix: &str) -> Result<(), std::io::Error> {
    let cutoff = SystemTime::now().checked_sub(AUDIO_RETENTION);
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let metadata = entry.file_type()?;
        if !metadata.is_file() {
            continue;
        }
        if !entry
            .file_name()
            .to_string_lossy()
            .starts_with(session_prefix)
        {
            continue;
        }
        let modified = entry.metadata()?.modified()?;
        if cutoff.is_some_and(|cutoff| modified < cutoff) {
            match fs::remove_file(entry.path()) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interrupted_audio_write_leaves_no_final_or_temporary_partial_file() {
        let root = tempfile::tempdir().expect("private audio directory");
        let path = root.path().join("chunk");
        let interrupted = create_private_file_with(&path, |output| {
            output.write_all(b"partial")?;
            Err(std::io::Error::other("simulated write interruption"))
        });
        assert!(interrupted.is_err());
        assert!(
            !path.exists(),
            "partial bytes must not become the final chunk"
        );
        assert_eq!(fs::read_dir(root.path()).expect("directory").count(), 0);

        create_private_file(&path, b"complete retry").expect("exact retry can publish");
        assert_eq!(fs::read(&path).expect("complete file"), b"complete retry");
    }
}
