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
use std::{
    fs,
    io::Write,
    path::Path,
    sync::{Arc, Mutex as StdMutex},
};
use studio_runtime_store::{FederationCredentialsRepository as Journal, RuntimeStore, StoreError};
use thiserror::Error;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;
use tokio_util::task::TaskTracker;

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
pub struct Credentials {
    identity: Arc<Mutex<Option<StoredMultiServerIdentity>>>,
    admission_open: StdMutex<bool>,
    operations: TaskTracker,
    #[cfg(test)]
    test_write_gate: Option<Arc<TestWriteGate>>,
}

impl Default for Credentials {
    fn default() -> Self {
        Self {
            identity: Arc::default(),
            admission_open: StdMutex::new(true),
            operations: TaskTracker::new(),
            #[cfg(test)]
            test_write_gate: None,
        }
    }
}

impl Credentials {
    fn spawn_operation<F, T>(
        &self,
        operation: F,
    ) -> Result<JoinHandle<Result<T, CredentialError>>, CredentialError>
    where
        F: std::future::Future<Output = Result<T, CredentialError>> + Send + 'static,
        T: Send + 'static,
    {
        let admission = self
            .admission_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if !*admission {
            return Err(CredentialError::Storage);
        }
        let tracker_lifetime = self.operations.clone();
        Ok(self.operations.spawn(async move {
            let _tracker_lifetime = tracker_lifetime;
            operation.await
        }))
    }

    /// Stop admitting credential operations before server shutdown begins.
    pub fn close_admission(&self) {
        let mut admission = self
            .admission_open
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *admission = false;
        self.operations.close();
    }

    /// Drain admitted identity operations before their store and filesystem dependencies stop.
    pub async fn shutdown(&self) {
        self.close_admission();
        self.operations.wait().await;
    }

    /// Create Python-compatible federation signing keys in the existing identity row.
    /// Key material is generated before the transaction; an insert race keeps the
    /// first committed identity and never replaces it.
    pub async fn ensure_federation_identity(
        store: Arc<RuntimeStore>,
        label: &str,
        lead_name: &str,
        created: f64,
    ) -> Result<RemoteIdentity, CredentialError> {
        validate_federation_label(label)?;
        let exists = store
            .read_async("runtime-services/federation_credentials", |connection| {
                connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM runtime_federation_identity WHERE id='local')",
                        [],
                        |row| row.get::<_, bool>(0),
                    )
                    .map_err(StoreError::from)
            })
            .await?;
        if exists {
            return load_federation_identity(&store).await;
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
        store
            .transaction_with(move |tx| -> Result<(), CredentialError> {
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
            })
            .await?;
        load_federation_identity(&store).await
    }

    pub async fn identity(
        &self,
        store: Arc<RuntimeStore>,
        origin: Option<&str>,
    ) -> Result<Value, CredentialError> {
        let identity_cache = Arc::clone(&self.identity);
        #[cfg(test)]
        let write_gate = self.test_write_gate.clone();
        let origin = origin.map(str::to_owned);
        self.spawn_operation(async move {
            supported_platform()?;
            let mut saved = identity_cache.lock().await;
            let identity = Self::load(
                store,
                &mut saved,
                #[cfg(test)]
                write_gate,
            )
            .await?;
            Ok(public_identity(identity, origin.as_deref()))
        })?
        .await
        .map_err(|_| CredentialError::Storage)?
    }

    pub async fn rename(
        &self,
        store: Arc<RuntimeStore>,
        target: &str,
        label: &str,
        request_id: &str,
        actor: &str,
    ) -> Result<(), CredentialError> {
        self.rename_with_notification(store, target, label, request_id, actor, |_| {})
            .await
    }

    /// Complete a durable rename while notifying the owner after each pending
    /// identity file write. The callback runs before later journal completion
    /// can fail, and receives the stable request ID for deduplicated recovery.
    pub async fn rename_with_notification(
        &self,
        store: Arc<RuntimeStore>,
        target: &str,
        label: &str,
        request_id: &str,
        actor: &str,
        mut after_identity_write: impl FnMut(&str) + Send + 'static,
    ) -> Result<(), CredentialError> {
        let identity_cache = Arc::clone(&self.identity);
        #[cfg(test)]
        let write_gate = self.test_write_gate.clone();
        let target = target.to_owned();
        let request_id = request_id.to_owned();
        let label = label.to_owned();
        let actor = actor.to_owned();
        self.spawn_operation(async move {
            supported_platform()?;
            let mut saved = identity_cache.lock().await;
            if !Self::is_local_target_with_saved(Arc::clone(&store), &target, &saved).await? {
                return Err(CredentialError::RemoteRequired);
            }
            // Check both journal identities before writing either. The target table
            // prevents a local request from reusing an earlier remote command ID.
            let request_id_for_reservation = request_id.clone();
            let label_for_reservation = label.clone();
            let actor_for_reservation = actor.clone();
            store
                .transaction_with(move |tx| -> Result<(), CredentialError> {
                    if !Journal::reserve_name(
                        tx,
                        &request_id_for_reservation,
                        &label_for_reservation,
                        &actor_for_reservation,
                    )? {
                        return Err(CredentialError::Conflict);
                    }
                    Ok(())
                })
                .await?;
            // Credential creation and pending recovery follow a committed reservation:
            // a conflicting request must not create a file or apply older journal rows.
            let identity = Self::load_with_notification(
                Arc::clone(&store),
                &mut saved,
                &mut after_identity_write,
                #[cfg(test)]
                write_gate.clone(),
            )
            .await?;
            Self::recover_with_notification(
                Arc::clone(&store),
                identity,
                &mut after_identity_write,
                #[cfg(test)]
                write_gate,
            )
            .await
        })?
        .await
        .map_err(|_| CredentialError::Storage)?
    }

    /// Resolve a target without creating credentials or recovering pending writes.
    pub async fn is_local_target(
        &self,
        store: Arc<RuntimeStore>,
        target: &str,
    ) -> Result<bool, CredentialError> {
        supported_platform()?;
        if target == "local" {
            return Ok(true);
        }
        let saved = self.identity.lock().await;
        if let Some(identity) = saved.as_ref() {
            return Ok(target == identity.server_id);
        }
        drop(saved);
        let root = store.root().to_path_buf();
        let target = target.to_owned();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/federation_credentials",
            move || match fs::symlink_metadata(root.join("multi-server/identity.json")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(_) => Err(CredentialError::Storage),
                Ok(_) => {
                    Ok(target == remote_transport::read_multi_server_identity(&root)?.server_id)
                }
            },
        )
        .await
        .map_err(|_| CredentialError::Storage)?
    }

    async fn is_local_target_with_saved(
        store: Arc<RuntimeStore>,
        target: &str,
        saved: &Option<StoredMultiServerIdentity>,
    ) -> Result<bool, CredentialError> {
        if target == "local" {
            return Ok(true);
        }
        if let Some(identity) = saved.as_ref() {
            return Ok(target == identity.server_id);
        }
        let root = store.root().to_path_buf();
        let target = target.to_owned();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/federation_credentials",
            move || match fs::symlink_metadata(root.join("multi-server/identity.json")) {
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
                Err(_) => Err(CredentialError::Storage),
                Ok(_) => {
                    Ok(target == remote_transport::read_multi_server_identity(&root)?.server_id)
                }
            },
        )
        .await
        .map_err(|_| CredentialError::Storage)?
    }

    async fn load(
        store: Arc<RuntimeStore>,
        saved: &mut Option<StoredMultiServerIdentity>,
        #[cfg(test)] write_gate: Option<Arc<TestWriteGate>>,
    ) -> Result<&mut StoredMultiServerIdentity, CredentialError> {
        Self::load_with_notification(
            store,
            saved,
            &mut |_| {},
            #[cfg(test)]
            write_gate,
        )
        .await
    }

    async fn load_with_notification<'a>(
        store: Arc<RuntimeStore>,
        saved: &'a mut Option<StoredMultiServerIdentity>,
        after_identity_write: &mut impl FnMut(&str),
        #[cfg(test)] write_gate: Option<Arc<TestWriteGate>>,
    ) -> Result<&'a mut StoredMultiServerIdentity, CredentialError> {
        if saved.is_none() {
            let root = store.root().to_path_buf();
            #[cfg(test)]
            let gate = write_gate.clone();
            let mut identity = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/federation_credentials",
                move || {
                    let path = root.join("multi-server/identity.json");
                    match fs::symlink_metadata(&path) {
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            private_directory(&root)?;
                            let identity = generate()?;
                            #[cfg(test)]
                            if let Some(gate) = gate {
                                gate.pause_first_write();
                            }
                            write_identity(&root, &identity, true)?;
                        }
                        Err(_) => return Err(CredentialError::Storage),
                    }
                    remote_transport::read_multi_server_identity(&root)
                        .map_err(CredentialError::from)
                },
            )
            .await
            .map_err(|_| CredentialError::Storage)??;
            Self::recover_with_notification(
                Arc::clone(&store),
                &mut identity,
                after_identity_write,
                #[cfg(test)]
                write_gate,
            )
            .await?;
            store.set_local_server_id(Some(identity.server_id.clone()))?;
            *saved = Some(identity);
        }
        saved.as_mut().ok_or(CredentialError::Storage)
    }

    async fn recover_with_notification(
        store: Arc<RuntimeStore>,
        identity: &mut StoredMultiServerIdentity,
        after_identity_write: &mut impl FnMut(&str),
        #[cfg(test)] write_gate: Option<Arc<TestWriteGate>>,
    ) -> Result<(), CredentialError> {
        // File access stays outside store transactions; the writer rechecks
        // the journal before marking each recovery complete.
        let pending = store
            .read_async(
                "runtime-services/federation_credentials",
                Journal::pending_names,
            )
            .await?;
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
            let root = store.root().to_path_buf();
            let updated_for_write = updated.clone();
            #[cfg(test)]
            let gate = write_gate.clone();
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/federation_credentials",
                move || {
                    #[cfg(test)]
                    if let Some(gate) = gate {
                        gate.pause_first_write();
                    }
                    write_identity(&root, &updated_for_write, false)
                },
            )
            .await
            .map_err(|_| CredentialError::Storage)??;
            after_identity_write(&id);
            updates.push((id, actor, updated.label.clone()));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| CredentialError::Storage)?
            .as_secs_f64();
        let updates_for_commit = updates.clone();
        let server_id_for_commit = updated.server_id.clone();
        store
            .transaction_with(move |tx| -> Result<(), CredentialError> {
                let current = Journal::pending_names(tx)?;
                for (id, actor, label) in &updates_for_commit {
                    if !current
                        .iter()
                        .any(|(current_id, current_label, current_actor)| {
                            current_id == id && current_label == label && current_actor == actor
                        })
                    {
                        return Err(CredentialError::InvalidJournal);
                    }
                    Journal::complete_name(tx, id, &server_id_for_commit, actor, now)?;
                }
                Ok(())
            })
            .await?;
        *identity = updated;
        Ok(())
    }
}

#[cfg(test)]
struct TestWriteGate {
    state: std::sync::Mutex<(bool, bool)>,
    changed: std::sync::Condvar,
}

#[cfg(test)]
impl TestWriteGate {
    fn new() -> Self {
        Self {
            state: std::sync::Mutex::new((false, false)),
            changed: std::sync::Condvar::new(),
        }
    }

    fn pause_first_write(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if state.0 {
            return;
        }
        state.0 = true;
        self.changed.notify_all();
        while !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
    }

    fn wait_until_reached(&self) -> bool {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        while !state.0 && !state.1 {
            state = self
                .changed
                .wait(state)
                .unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        state.0
    }

    fn release(&self) {
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        state.1 = true;
        self.changed.notify_all();
    }
}

#[cfg(test)]
struct TestWriteGateRelease(Arc<TestWriteGate>);

#[cfg(test)]
impl Drop for TestWriteGateRelease {
    fn drop(&mut self) {
        self.0.release();
    }
}

async fn load_federation_identity(
    store: &Arc<RuntimeStore>,
) -> Result<RemoteIdentity, CredentialError> {
    remote_transport::load_federation_identity(store)
        .await
        .map_err(CredentialError::Identity)
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
    #[tokio::test]
    async fn first_use_conflict_preserves_the_credential_directory_and_journal() {
        for directory_exists in [false, true] {
            for foreign_target in [false, true] {
                let (_directory, root, store) =
                    remote_transport::tests::runtime_store_fixture().await;
                store
                    .transaction(move |tx| {
                        assert!(Journal::reserve_name(tx, "reserved", "Original", "local")?);
                        if foreign_target {
                            tx.execute(
                                "UPDATE runtime_access_name_targets SET server='remote'",
                                [],
                            )?;
                        }
                        Ok(())
                    })
                    .await
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
                    credentials
                        .rename(Arc::clone(&store), "local", label, "reserved", "local")
                        .await,
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
                    .await
                    .expect("reservation remains unapplied");
                drop(store);
            }
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn rename_notifies_after_identity_file_write_with_stable_request_id() {
        let (_directory, root, store) = remote_transport::tests::runtime_store_fixture().await;
        let credentials = Credentials::default();
        let notified = Arc::new(StdMutex::new(Vec::new()));
        let notified_by_callback = Arc::clone(&notified);

        credentials
            .rename_with_notification(
                Arc::clone(&store),
                "local",
                "Renamed server",
                "rename-notify",
                "local",
                move |request_id| {
                    notified_by_callback
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .push(request_id.to_owned())
                },
            )
            .await
            .expect("rename completes");

        assert_eq!(
            *notified
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
            vec!["rename-notify"]
        );
        let identity = remote_transport::read_multi_server_identity(&root).expect("identity read");
        assert_eq!(identity.label, "Renamed server");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canceled_rename_retains_serialization_until_file_write_and_commit_finish() {
        let (_directory, root, store) = remote_transport::tests::runtime_store_fixture().await;
        let mut credentials = Credentials::default();
        credentials
            .identity(Arc::clone(&store), None)
            .await
            .expect("initial identity");
        let gate = Arc::new(TestWriteGate::new());
        let _release_gate = TestWriteGateRelease(Arc::clone(&gate));
        credentials.test_write_gate = Some(Arc::clone(&gate));
        let credentials = Arc::new(credentials);

        let first_credentials = Arc::clone(&credentials);
        let first_store = Arc::clone(&store);
        let first = tokio::spawn(async move {
            first_credentials
                .rename(first_store, "local", "First", "cancel-first", "local")
                .await
        });
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-services/federation_credentials",
                    {
                        let gate = Arc::clone(&gate);
                        move || gate.wait_until_reached()
                    }
                ),
            )
            .await
            .expect("first native write gate timeout")
            .expect("first native write gate task"),
            "first native write reached"
        );
        first.abort();
        assert!(first.await.expect_err("waiter canceled").is_cancelled());

        let second_credentials = Arc::clone(&credentials);
        let second_store = Arc::clone(&store);
        let second = tokio::spawn(async move {
            second_credentials
                .rename(second_store, "local", "Second", "cancel-second", "local")
                .await
        });
        tokio::task::yield_now().await;
        assert!(
            !second.is_finished(),
            "second rename waits for the first owner"
        );
        gate.release();
        second
            .await
            .expect("second operation joins")
            .expect("second rename");

        let identity = remote_transport::read_multi_server_identity(&root).expect("identity read");
        assert_eq!(identity.label, "Second");
        store
            .transaction(|tx| {
                assert!(Journal::pending_names(tx)?.is_empty());
                Ok(())
            })
            .await
            .expect("journal settled");
        drop(store);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn canceled_initial_identity_creation_finishes_before_later_rename() {
        let (_directory, root, store) = remote_transport::tests::runtime_store_fixture().await;
        let mut credentials = Credentials::default();
        let gate = Arc::new(TestWriteGate::new());
        let _release_gate = TestWriteGateRelease(Arc::clone(&gate));
        credentials.test_write_gate = Some(Arc::clone(&gate));
        let credentials = Arc::new(credentials);

        let first_credentials = Arc::clone(&credentials);
        let first_store = Arc::clone(&store);
        let first =
            tokio::spawn(async move { first_credentials.identity(first_store, None).await });
        assert!(
            tokio::time::timeout(
                std::time::Duration::from_secs(5),
                studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-services/federation_credentials",
                    {
                        let gate = Arc::clone(&gate);
                        move || gate.wait_until_reached()
                    }
                ),
            )
            .await
            .expect("initial native write gate timeout")
            .expect("initial native write gate task"),
            "initial native write reached"
        );
        first.abort();
        assert!(first.await.expect_err("waiter canceled").is_cancelled());

        let second_credentials = Arc::clone(&credentials);
        let second_store = Arc::clone(&store);
        let second = tokio::spawn(async move {
            second_credentials
                .rename(
                    second_store,
                    "local",
                    "After create",
                    "after-create",
                    "local",
                )
                .await
        });
        tokio::task::yield_now().await;
        assert!(!second.is_finished(), "rename waits for identity creation");
        gate.release();
        second
            .await
            .expect("rename joins")
            .expect("rename completes");

        let identity = remote_transport::read_multi_server_identity(&root).expect("identity read");
        assert_eq!(identity.label, "After create");
        drop(store);
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
