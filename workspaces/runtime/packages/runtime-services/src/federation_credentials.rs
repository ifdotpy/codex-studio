//! Lazy local credentials and recoverable identity renames.
//!
//! The credential file is authoritative for the keys. The existing name journal
//! owns request identity: a retry may only finish the same label, never change it.
//! Keys, server ID and audit time are server-owned; the local route supplies the
//! actor. A pending journal survives a crash between file replacement and commit.

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::{
    rand::SystemRandom,
    signature::{Ed25519KeyPair, KeyPair},
};
use serde_json::{Value, json};
use spki::{
    AlgorithmIdentifierRef, ObjectIdentifier, SubjectPublicKeyInfoRef,
    der::{Encode, asn1::BitStringRef},
};
use std::{fs, io::Write, path::Path, sync::Mutex};
use studio_runtime_store::{FederationCredentialsRepository as Journal, RuntimeStore, StoreError};
use thiserror::Error;

use crate::remote_transport::{self, RemoteTransportError, StoredMultiServerIdentity};

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("The local server credential storage is unavailable or unsafe")]
    Storage,
    #[error("The stored local server credential is invalid")]
    Identity(#[from] RemoteTransportError),
    #[error("The server name request ID has different content")]
    Conflict,
    #[error("Use 1 to 80 visible characters without outer spaces")]
    InvalidName,
    #[error("Invalid stored object runtime_access_name_requests")]
    InvalidJournal,
    #[error("The identity journal is unavailable")]
    Store(#[from] StoreError),
    #[error("The remote server requires an authenticated signed request")]
    RemoteRequired,
}

/// Loaded once on first use, like Python's `_keys`. Never formats stored secrets.
#[derive(Default)]
pub struct Credentials {
    identity: Mutex<Option<StoredMultiServerIdentity>>,
}

impl Credentials {
    pub fn identity(
        &self,
        store: &RuntimeStore,
        origin: Option<&str>,
    ) -> Result<Value, CredentialError> {
        let mut saved = self.identity.lock().map_err(|_| CredentialError::Storage)?;
        let identity = self.load(store, &mut saved)?;
        Ok(public_identity(identity, origin))
    }

    pub fn rename(
        &self,
        store: &RuntimeStore,
        target: &str,
        label: &str,
        request_id: &str,
        actor: &str,
    ) -> Result<(), CredentialError> {
        validate_name(label)?;
        if !self.is_local_target(store, target)? {
            return Err(CredentialError::RemoteRequired);
        }
        let mut saved = self.identity.lock().map_err(|_| CredentialError::Storage)?;
        let identity = self.load(store, &mut saved)?;
        // Check both journal identities before writing either. The target table
        // prevents a local request from reusing an earlier remote command ID.
        store.transaction_with(|tx| -> Result<(), CredentialError> {
            if !Journal::reserve_name(tx, request_id, label, actor)? {
                return Err(CredentialError::Conflict);
            }
            Ok(())
        })?;
        self.recover(store, identity)
    }

    /// Resolve a target without creating credentials or recovering pending writes.
    pub fn is_local_target(
        &self,
        store: &RuntimeStore,
        target: &str,
    ) -> Result<bool, CredentialError> {
        if target == "local" {
            return Ok(true);
        }
        let saved = self.identity.lock().map_err(|_| CredentialError::Storage)?;
        if let Some(identity) = saved.as_ref() {
            return Ok(target == identity.server_id);
        }
        match fs::symlink_metadata(store.root().join("multi-server/identity.json")) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(CredentialError::Storage),
            Ok(_) => {
                Ok(target == remote_transport::read_multi_server_identity(store.root())?.server_id)
            }
        }
    }

    fn load<'a>(
        &self,
        store: &RuntimeStore,
        saved: &'a mut Option<StoredMultiServerIdentity>,
    ) -> Result<&'a mut StoredMultiServerIdentity, CredentialError> {
        if saved.is_none() {
            let path = store.root().join("multi-server/identity.json");
            match fs::symlink_metadata(&path) {
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                    private_directory(store.root())?;
                    let identity = generate()?;
                    write_identity(store.root(), &identity, true)?;
                }
                Err(_) => return Err(CredentialError::Storage),
            }
            let mut identity = remote_transport::read_multi_server_identity(store.root())?;
            self.recover(store, &mut identity)?;
            *saved = Some(identity);
        }
        saved.as_mut().ok_or(CredentialError::Storage)
    }

    fn recover(
        &self,
        store: &RuntimeStore,
        identity: &mut StoredMultiServerIdentity,
    ) -> Result<(), CredentialError> {
        // The outer credential mutex and the store's single writer serialize
        // recovery; no HTTP call can observe an applied journal with stale keys.
        store.transaction_with(|tx| -> Result<(), CredentialError> {
            let pending = Journal::pending_names(tx)?;
            // Refuse malformed journal rows before changing any file.
            for (_, label, _) in &pending {
                validate_name(label).map_err(|_| CredentialError::InvalidJournal)?;
            }
            for (id, label, actor) in pending {
                let mut updated = identity.clone();
                updated.label = label;
                write_identity(store.root(), &updated, false)?;
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|_| CredentialError::Storage)?
                    .as_secs_f64();
                Journal::complete_name(tx, &id, &updated.server_id, &actor, now)?;
                *identity = updated;
            }
            Ok(())
        })
    }
}

fn public_identity(identity: &StoredMultiServerIdentity, origin: Option<&str>) -> Value {
    json!({"serverId":identity.server_id,"label":identity.label,"publicKey":identity.public_key,"origin":origin,"tailscaleUser":null})
}

fn validate_name(label: &str) -> Result<(), CredentialError> {
    // Python's visible-character rule is not in the committed name schema.
    // Unicode categories require the schema validator's Unicode-aware regex.
    static VISIBLE: std::sync::OnceLock<
        Result<jsonschema::Validator, jsonschema::ValidationError<'static>>,
    > = std::sync::OnceLock::new();
    let validator = VISIBLE.get_or_init(|| {
        jsonschema::validator_for(
            &json!({"type":"string","minLength":1,"maxLength":80,"pattern":r"^[^\p{C}]*$"}),
        )
        .map_err(|error| error.to_owned())
    });
    if label != label.trim()
        || label.chars().any(|c| c.is_whitespace() && c != ' ')
        || !validator
            .as_ref()
            .map_err(|_| CredentialError::Storage)?
            .is_valid(&json!(label))
    {
        return Err(CredentialError::InvalidName);
    }
    Ok(())
}

fn private_directory(root: &Path) -> Result<(), CredentialError> {
    let directory = root.join("multi-server");
    let mut builder = fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    match builder.create(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(_) => return Err(CredentialError::Storage),
    }
    let metadata = fs::symlink_metadata(&directory).map_err(|_| CredentialError::Storage)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(CredentialError::Storage);
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o077 != 0 {
            return Err(CredentialError::Storage);
        }
    }
    #[cfg(windows)]
    {
        return Err(CredentialError::Storage);
    }
    Ok(())
}

fn write_identity(
    root: &Path,
    identity: &StoredMultiServerIdentity,
    create: bool,
) -> Result<(), CredentialError> {
    private_directory(root)?;
    let directory = root.join("multi-server");
    let path = directory.join("identity.json");
    if !create {
        // Reuse the transport's bounded, no-follow permission and key checks.
        remote_transport::read_multi_server_identity(root)?;
    }
    identity.signing_identity()?;
    let mut file =
        tempfile::NamedTempFile::new_in(&directory).map_err(|_| CredentialError::Storage)?;
    serde_json::to_writer(file.as_file_mut(), identity).map_err(|_| CredentialError::Storage)?;
    file.flush().map_err(|_| CredentialError::Storage)?;
    file.as_file()
        .sync_all()
        .map_err(|_| CredentialError::Storage)?;
    if create {
        file.persist_noclobber(path)
            .map_err(|_| CredentialError::Storage)?;
    } else {
        file.persist(path).map_err(|_| CredentialError::Storage)?;
    }
    fs::File::open(directory)
        .and_then(|file| file.sync_all())
        .map_err(|_| CredentialError::Storage)?;
    Ok(())
}

fn generate() -> Result<StoredMultiServerIdentity, CredentialError> {
    let key = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| CredentialError::Storage)?;
    let pair = Ed25519KeyPair::from_pkcs8(key.as_ref()).map_err(|_| CredentialError::Storage)?;
    let public = SubjectPublicKeyInfoRef {
        algorithm: AlgorithmIdentifierRef {
            oid: ObjectIdentifier::new_unwrap("1.3.101.112"),
            parameters: None,
        },
        subject_public_key: BitStringRef::from_bytes(pair.public_key().as_ref())
            .map_err(|_| CredentialError::Storage)?,
    }
    .to_der()
    .map_err(|_| CredentialError::Storage)?;
    let mut id = [0_u8; 16];
    getrandom::fill(&mut id).map_err(|_| CredentialError::Storage)?;
    Ok(StoredMultiServerIdentity {
        server_id: uuid::Builder::from_random_bytes(id)
            .into_uuid()
            .simple()
            .to_string(),
        label: "Studio server".to_owned(),
        public_key: pem("PUBLIC KEY", &public),
        private_key: pem("PRIVATE KEY", key.as_ref()),
    })
}

fn pem(label: &str, der: &[u8]) -> String {
    format!(
        "-----BEGIN {label}-----\n{}\n-----END {label}-----\n",
        STANDARD.encode(der)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generated_private_identity_round_trips_and_signs_transport_bytes() {
        let root = tempfile::tempdir().expect("temporary credential root");
        let identity = generate().expect("generated temporary key");
        write_identity(root.path(), &identity, true).expect("private file created");
        let loaded = remote_transport::load_multi_server_identity(root.path()).expect("keys load");
        let signature = loaded.sign(b"loopback credential contract");
        remote_transport::verify_signature(
            loaded.public_key_pem(),
            b"loopback credential contract",
            &signature,
        )
        .expect("signature verifies");
        assert!(!format!("{loaded:?}").contains("PRIVATE KEY"));
        assert!(
            write_identity(root.path(), &identity, true).is_err(),
            "creation cannot replace keys"
        );
    }

    #[cfg(unix)]
    #[test]
    fn identity_writer_refuses_symlinks_and_unsafe_modes_without_replacement() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let root = tempfile::tempdir().expect("temporary credential root");
        let identity = generate().expect("generated temporary key");
        write_identity(root.path(), &identity, true).expect("private file created");
        let path = root.path().join("multi-server/identity.json");
        let bytes = fs::read(&path).expect("temporary key file");
        fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("unsafe fixture mode");
        assert!(write_identity(root.path(), &identity, false).is_err());
        assert!(fs::read(&path).expect("unchanged file") == bytes);
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("restore fixture mode");
        let target = root.path().join("original.json");
        fs::rename(&path, &target).expect("move temporary fixture");
        symlink(&target, &path).expect("temporary symlink");
        assert!(write_identity(root.path(), &identity, false).is_err());
        assert!(path.is_symlink());
        assert!(fs::read(&target).expect("unchanged target") == bytes);
    }
}
