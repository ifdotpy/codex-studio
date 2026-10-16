//! Lazy local credentials and recoverable identity renames.
//!
//! The credential file is authoritative for the keys. The existing name journal
//! owns request identity: a retry may only finish the same label, never change it.
//! Keys, server ID and audit time are server-owned; the local route supplies the
//! actor. A pending journal survives a crash between file replacement and commit.
//! Only Unix credential permissions are supported at first cutover. Windows and
//! other platforms explicitly refuse credentials until their secure file support exists.

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

use crate::remote_transport::{
    self, RemoteIdentity, RemoteTransportError, StoredMultiServerIdentity,
};

#[derive(Debug, Error)]
pub enum CredentialError {
    #[error("Not implemented: requires server-credentials/platform-support")]
    UnsupportedPlatform,
    #[error("The local server credential storage is unavailable or unsafe")]
    Storage,
    #[error("The stored local server credential is invalid")]
    Identity(#[from] RemoteTransportError),
    #[error("The server name request ID has different content")]
    Conflict,
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
    /// Create Python-compatible federation signing keys in the existing identity row.
    /// Key material is generated before the transaction; an insert race keeps the
    /// first committed identity and never replaces it.
    pub fn ensure_federation_identity(
        store: &RuntimeStore,
        label: &str,
        lead_name: &str,
        created: f64,
    ) -> Result<RemoteIdentity, CredentialError> {
        validate_federation_label(label)?;
        let exists = store.read_transaction(|connection| {
            connection
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_federation_identity WHERE id='local')",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .map_err(StoreError::from)
        })?;
        if exists {
            return remote_transport::load_federation_identity(store)
                .map_err(CredentialError::Identity);
        }

        let generated = generate()?;
        let state_id = uuid::Uuid::new_v4().to_string();
        let record = json!({
            "stateId": state_id,
            "privateKey": generated.private_key,
            "publicKey": generated.public_key,
            "label": label,
            "leadName": lead_name,
            "created": created,
        });
        store.transaction_with(|tx| -> Result<(), CredentialError> {
            let exists: bool = tx
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_federation_identity WHERE id='local')",
                    [],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)?;
            if !exists {
                tx.execute(
                    "INSERT INTO runtime_federation_identity(id,record) VALUES('local',?1)",
                    [record.to_string()],
                )
                .map_err(StoreError::from)?;
            }
            Ok(())
        })?;
        remote_transport::load_federation_identity(store).map_err(CredentialError::Identity)
    }

    pub fn identity(
        &self,
        store: &RuntimeStore,
        origin: Option<&str>,
    ) -> Result<Value, CredentialError> {
        supported_platform()?;
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
        self.rename_with_notification(store, target, label, request_id, actor, |_| {})
    }

    /// Complete a durable rename while notifying the owner after each pending
    /// identity file write. The callback runs before later journal completion
    /// can fail, and receives the stable request ID for deduplicated recovery.
    pub fn rename_with_notification(
        &self,
        store: &RuntimeStore,
        target: &str,
        label: &str,
        request_id: &str,
        actor: &str,
        mut after_identity_write: impl FnMut(&str),
    ) -> Result<(), CredentialError> {
        supported_platform()?;
        if !self.is_local_target(store, target)? {
            return Err(CredentialError::RemoteRequired);
        }
        let mut saved = self.identity.lock().map_err(|_| CredentialError::Storage)?;
        // Check both journal identities before writing either. The target table
        // prevents a local request from reusing an earlier remote command ID.
        store.transaction_with(|tx| -> Result<(), CredentialError> {
            if !Journal::reserve_name(tx, request_id, label, actor)? {
                return Err(CredentialError::Conflict);
            }
            Ok(())
        })?;
        // Credential creation and pending recovery follow a committed reservation:
        // a conflicting request must not create a file or apply older journal rows.
        let identity = self.load_with_notification(
            store,
            &mut saved,
            &mut after_identity_write,
        )?;
        self.recover_with_notification(store, identity, &mut after_identity_write)
    }

    /// Resolve a target without creating credentials or recovering pending writes.
    pub fn is_local_target(
        &self,
        store: &RuntimeStore,
        target: &str,
    ) -> Result<bool, CredentialError> {
        supported_platform()?;
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
        self.load_with_notification(store, saved, &mut |_| {})
    }

    fn load_with_notification<'a>(
        &self,
        store: &RuntimeStore,
        saved: &'a mut Option<StoredMultiServerIdentity>,
        after_identity_write: &mut impl FnMut(&str),
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
            self.recover_with_notification(store, &mut identity, after_identity_write)?;
            store.set_local_server_id(Some(identity.server_id.clone()))?;
            *saved = Some(identity);
        }
        saved.as_mut().ok_or(CredentialError::Storage)
    }

    fn recover_with_notification(
        &self,
        store: &RuntimeStore,
        identity: &mut StoredMultiServerIdentity,
        after_identity_write: &mut impl FnMut(&str),
    ) -> Result<(), CredentialError> {
        // File access stays outside store transactions; the writer rechecks
        // the journal before marking each recovery complete.
        let pending = store.read_transaction_with_error(Journal::pending_names)?;
        if pending.is_empty() {
            return Ok(());
        }
        for (_, label, _) in &pending {
            validate_stored_name(label)?;
        }
        let mut updates = Vec::with_capacity(pending.len());
        let mut updated = identity.clone();
        for (id, label, actor) in pending {
            updated.label = label;
            write_identity(store.root(), &updated, false)?;
            after_identity_write(&id);
            updates.push((id, actor, updated.label.clone()));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| CredentialError::Storage)?
            .as_secs_f64();
        store.transaction_with(|tx| -> Result<(), CredentialError> {
            let current = Journal::pending_names(tx)?;
            for (id, actor, label) in &updates {
                if !current
                    .iter()
                    .any(|(current_id, current_label, current_actor)| {
                        current_id == id && current_label == label && current_actor == actor
                    })
                {
                    return Err(CredentialError::InvalidJournal);
                }
                Journal::complete_name(tx, id, &updated.server_id, actor, now)?;
            }
            Ok(())
        })?;
        *identity = updated;
        Ok(())
    }
}

fn public_identity(identity: &StoredMultiServerIdentity, origin: Option<&str>) -> Value {
    json!({"serverId":identity.server_id,"label":identity.label,"publicKey":identity.public_key,"origin":origin,"tailscaleUser":null})
}

fn validate_stored_name(label: &str) -> Result<(), CredentialError> {
    if studio_runtime_schema::validate(
        "RenameServer",
        &json!({"label": label, "requestId": "recovery"}),
    )
    .map_err(|_| CredentialError::InvalidJournal)?
    {
        Ok(())
    } else {
        Err(CredentialError::InvalidJournal)
    }
}

fn validate_federation_label(label: &str) -> Result<(), CredentialError> {
    if label.trim().is_empty() || label.trim().chars().count() > 80 {
        return Err(CredentialError::Storage);
    }
    Ok(())
}

fn supported_platform() -> Result<(), CredentialError> {
    if cfg!(unix) {
        Ok(())
    } else {
        Err(CredentialError::UnsupportedPlatform)
    }
}

fn private_directory(root: &Path) -> Result<(), CredentialError> {
    supported_platform()?;
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

    #[cfg(unix)]
    #[test]
    fn first_use_conflict_preserves_the_credential_directory_and_journal() {
        for directory_exists in [false, true] {
            for foreign_target in [false, true] {
                let (root, store) = remote_transport::tests::runtime_store_fixture();
                store
                    .transaction(|tx| {
                        assert!(Journal::reserve_name(tx, "reserved", "Original", "local")?);
                        if foreign_target {
                            tx.execute(
                                "UPDATE runtime_access_name_targets SET server='remote'",
                                [],
                            )?;
                        }
                        Ok(())
                    })
                    .expect("existing journal reservation");
                let directory = root.join("multi-server");
                if directory_exists {
                    private_directory(&root).expect("empty credential directory");
                }
                let credentials = Credentials::default();
                let label = if foreign_target {
                    "Original"
                } else {
                    "Changed"
                };
                assert!(matches!(
                    credentials.rename(&store, "local", label, "reserved", "local"),
                    Err(CredentialError::Conflict)
                ));
                assert_eq!(directory.exists(), directory_exists);
                if directory_exists {
                    assert_eq!(
                        fs::read_dir(&directory)
                            .expect("credential directory")
                            .count(),
                        0
                    );
                }
                store
                    .transaction(|tx| {
                        assert_eq!(
                            Journal::pending_names(tx)?,
                            vec![("reserved".into(), "Original".into(), "local".into())]
                        );
                        Ok(())
                    })
                    .expect("reservation remains unapplied");
                drop(store);
                fs::remove_dir_all(root).expect("remove disposable state");
            }
        }
    }

    #[cfg(unix)]
    #[test]
    fn rename_notifies_after_identity_file_write_with_stable_request_id() {
        let (root, store) = remote_transport::tests::runtime_store_fixture();
        let credentials = Credentials::default();
        let mut notified = Vec::new();

        credentials
            .rename_with_notification(
                &store,
                "local",
                "Renamed server",
                "rename-notify",
                "local",
                |request_id| notified.push(request_id.to_owned()),
            )
            .expect("rename completes");

        assert_eq!(notified, vec!["rename-notify"]);
        let identity = remote_transport::read_multi_server_identity(&root).expect("identity read");
        assert_eq!(identity.label, "Renamed server");
    }

    #[cfg(unix)]
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
