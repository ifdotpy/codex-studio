//! Creates the committed fresh-state catalog when the canvas and analytics
//! databases do not exist. A supervisor journal that already exists is kept.
//!
//! The committed `runtime-state-schema.json` beside this file is the schema
//! source for this bootstrap.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, TransactionBehavior};
use serde_json::{Value, json};

use crate::{StoreError, StoreResult};

const SNAPSHOT: &str = include_str!("runtime-state-schema.json");
const CORE_DATABASES: [&str; 2] = ["canvas.sqlite3", "analytics.sqlite3"];
/// The process supervisor's journal (`codex_process_supervisor.Journal` and
/// `supervisor_journal::SupervisorJournal`). The supervisor owns it once it exists.
const SUPERVISOR_DATABASE: &str = "supervisor.sqlite3";
/// Files of background recovery in the state folder. Owners: `files.config` and the
/// launch agent's log in desktop recovery.cjs, and the lock in recover_backend.py `run`.
const RECOVERY_CONFIGURATION: &str = "background-recovery.json";
const RECOVERY_LOCK: &str = "background-recovery.lock";
const RECOVERY_LOG: &str = "background-recovery.log";

/// Validate startup contents without creating the runtime lock or changing files.
pub(crate) fn validate(root: &Path) -> StoreResult<()> {
    let supervisor = entry_kind(&root.join(SUPERVISOR_DATABASE));
    let core_exists = CORE_DATABASES
        .iter()
        .any(|name| !matches!(entry_kind(&root.join(name)), EntryKind::Missing));
    let core_complete = CORE_DATABASES
        .iter()
        .all(|name| matches!(entry_kind(&root.join(name)), EntryKind::File));
    if core_exists && !core_complete {
        let present = CORE_DATABASES
            .iter()
            .find(|name| !matches!(entry_kind(&root.join(name)), EntryKind::Missing))
            .copied()
            .unwrap_or(CORE_DATABASES[0]);
        return Err(StoreError::SchemaMismatch(format!(
            "partial state database at {}: both canvas.sqlite3 and analytics.sqlite3 must exist",
            root.join(present).display()
        )));
    }
    if matches!(supervisor, EntryKind::Other) {
        return Err(StoreError::SchemaMismatch(format!(
            "partial supervisor database at {}",
            root.join(SUPERVISOR_DATABASE).display()
        )));
    }
    if core_complete {
        return Ok(());
    }

    let journal_exists = matches!(supervisor, EntryKind::File);
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let accepted = match name.to_str() {
            // Python creates this lease before its databases.
            Some("runtime.lock") => matches!(entry_kind(&path), EntryKind::File),
            // The Rust server claims the state (state_owner.rs) before it creates
            // the databases; a killed claim can leave its temporary file. The
            // launcher opens the log before spawn.
            Some(
                crate::state_owner::STATE_OWNER_FILE
                | crate::state_owner::STATE_OWNER_TEMPORARY_FILE
                | "canvas.log",
            ) => accepted_private_file(&path),
            // Runtime.__init__ creates these private directories and their
            // registry/identity files before it opens the SQLite databases.
            Some("accounts") => accepted_python_directory(&path, "registry.json"),
            Some("multi-server") => accepted_python_directory(&path, "identity.json"),
            // The host installer and migration command can run before the first backend.
            Some(crate::NATIVE_LAYR_DIRECTORY) => {
                fs::symlink_metadata(&path).is_ok_and(|metadata| metadata.file_type().is_dir())
            }
            // launchd (macOS) and systemd (Linux) can start the process
            // supervisor before the first backend. Python and Rust supervisors
            // create their lease, journal and socket, and the launch agent
            // sends supervisor output to supervisor.log. Python creates these
            // with the process umask, so no private mode is required. A
            // Python backend starts beside them and never writes the journal.
            Some(SUPERVISOR_DATABASE | "supervisor.lock" | "supervisor.log") => {
                matches!(entry_kind(&path), EntryKind::File)
            }
            // SQLite WAL sidecars are valid only beside their journal. A stray
            // WAL file could otherwise pair with a newly created journal.
            Some("supervisor.sqlite3-wal" | "supervisor.sqlite3-shm") => {
                journal_exists && matches!(entry_kind(&path), EntryKind::File)
            }
            Some("supervisor.sock") => is_socket(&path),
            // Background recovery can be turned on before the first backend starts: the
            // recovery helper (desktop recovery.cjs) writes its private configuration, and
            // the recovery job (recover_backend.py, umask 077) opens its private lock before
            // it starts the server.
            Some(RECOVERY_CONFIGURATION | RECOVERY_LOCK) => accepted_private_file(&path),
            // launchd opens the recovery job's log with its own umask, as for supervisor.log.
            Some(RECOVERY_LOG) => matches!(entry_kind(&path), EntryKind::File),
            _ => false,
        };
        if !accepted {
            return Err(StoreError::SchemaMismatch(format!(
                "unrecognized entry in fresh state directory: {}",
                path.display()
            )));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn is_socket(path: &Path) -> bool {
    use std::os::unix::fs::FileTypeExt;
    fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_socket())
}

#[cfg(not(unix))]
fn is_socket(_path: &Path) -> bool {
    false
}

#[derive(Clone, Copy)]
enum EntryKind {
    Missing,
    File,
    Other,
}

fn entry_kind(path: &Path) -> EntryKind {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_file() => EntryKind::File,
        Ok(_) => EntryKind::Other,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => EntryKind::Missing,
        Err(_) => EntryKind::Other,
    }
}

fn accepted_python_directory(path: &Path, accepted_file: &str) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    if !metadata.file_type().is_dir() || !private_permissions(&metadata) {
        return false;
    }
    let Ok(entries) = fs::read_dir(path) else {
        return false;
    };
    for entry in entries {
        let Ok(entry) = entry else {
            return false;
        };
        let file_metadata = match fs::symlink_metadata(entry.path()) {
            Ok(metadata) => metadata,
            Err(_) => return false,
        };
        if entry.file_name() != accepted_file
            || !file_metadata.file_type().is_file()
            || !private_permissions(&file_metadata)
        {
            return false;
        }
    }
    // Python can resume after creating the directory but before its file.
    true
}

fn accepted_private_file(path: &Path) -> bool {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return false;
    };
    metadata.file_type().is_file() && private_permissions(&metadata)
}

#[cfg(unix)]
fn private_permissions(metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o077 == 0
}

#[cfg(not(unix))]
fn private_permissions(_metadata: &fs::Metadata) -> bool {
    true
}

/// Prepare databases after the read-only startup-content decision.
pub(crate) fn prepare(root: &Path) -> StoreResult<bool> {
    validate(root)?;
    if CORE_DATABASES
        .iter()
        .all(|name| matches!(entry_kind(&root.join(name)), EntryKind::File))
    {
        return Ok(false);
    }
    if CORE_DATABASES
        .iter()
        .all(|name| matches!(entry_kind(&root.join(name)), EntryKind::Missing))
    {
        create_fresh(root)?;
        return Ok(true);
    }
    Err(StoreError::SchemaMismatch(format!(
        "partial state directory at {}",
        root.display()
    )))
}

fn create_fresh(root: &Path) -> StoreResult<()> {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let staging = root.join(format!(
        ".rust-state-bootstrap-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir(&staging)?;
    let result = build_staging(&staging, root).and_then(|()| publish(&staging, root));
    let cleanup = fs::remove_dir_all(&staging);
    match (result, cleanup) {
        (Err(error), _) => Err(error),
        (Ok(()), Err(error)) => Err(StoreError::StateDirectory(error)),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn build_staging(staging: &Path, final_root: &Path) -> StoreResult<()> {
    let snapshot: Value = serde_json::from_str(SNAPSHOT)?;
    for (file, schema) in CORE_DATABASES
        .iter()
        .zip(["canvas.sqlite3", "analytics.sqlite3"])
    {
        create_database(&staging.join(file), &snapshot, schema)?;
    }

    let mut canvas = Connection::open(staging.join("canvas.sqlite3"))?;
    crate::schema::register_python_trigger_functions(&canvas)?;
    let transaction = canvas.transaction_with_behavior(TransactionBehavior::Immediate)?;
    crate::bootstrap::prepare_sync_schema(&transaction)?;
    seed_python_lazy_markers(&transaction, staging)?;
    seed_workspace(&transaction, final_root)?;
    transaction.commit()?;
    drop(canvas);

    let supervisor = crate::supervisor_journal::SupervisorJournal::open(staging)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
    drop(supervisor);
    Ok(())
}

/// Seed the retained generation and analytics tracking markers.
fn seed_python_lazy_markers(
    transaction: &rusqlite::Transaction<'_>,
    staging: &Path,
) -> StoreResult<()> {
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    transaction.execute(
        "INSERT INTO runtime_agent_record_generation(id,value) VALUES(1,0)",
        [],
    )?;
    transaction.execute(
        "INSERT INTO runtime_scheduler_agent_change_state VALUES(1,0,0,0,0)",
        [],
    )?;

    let analytics = Connection::open(staging.join("analytics.sqlite3"))?;
    analytics.execute(
        "INSERT OR IGNORE INTO analytics_meta(key,value) VALUES('trackingSince',?1)",
        [format!("{}", created as f64 / 1_000_000_000.0)],
    )?;
    Ok(())
}

fn create_database(path: &Path, snapshot: &Value, schema_name: &str) -> StoreResult<Connection> {
    let mut connection = Connection::open(path)?;
    crate::sqlite_durability::configure_writer(&connection)?;
    let transaction = connection.transaction()?;

    let catalog =
        snapshot[schema_name]["sqlite_master"]
            .as_array()
            .ok_or(StoreError::RecordContract(
                "fresh schema catalog is missing",
            ))?;
    let mut objects = catalog
        .iter()
        .map(|entry| {
            let order = entry["create_order"]
                .as_i64()
                .ok_or(StoreError::RecordContract(
                    "fresh schema object has no creation order",
                ))?;
            Ok((order, entry))
        })
        .collect::<StoreResult<Vec<_>>>()?;
    objects.sort_by_key(|(order, _)| *order);
    for (_, entry) in objects {
        let Some(kind) = entry["type"].as_str() else {
            return Err(StoreError::RecordContract(
                "fresh schema object has no type",
            ));
        };
        if !matches!(kind, "table" | "index" | "trigger") {
            continue;
        }
        let sql = entry["sql"]
            .as_str()
            .ok_or(StoreError::RecordContract("fresh schema object has no DDL"))?;
        transaction.execute_batch(sql)?;
    }
    let version = snapshot[schema_name]["pragma_user_version"]
        .as_i64()
        .ok_or(StoreError::RecordContract(
            "fresh schema user_version is missing",
        ))?;
    transaction.pragma_update(None, "user_version", version)?;
    transaction.commit()?;
    Ok(connection)
}

/// Repository witnesses use the production catalog without files, fsyncs or writer actors.
#[cfg(any(test, feature = "test-support"))]
pub fn test_canvas() -> Connection {
    let snapshot = serde_json::from_str(SNAPSHOT).expect("fresh schema snapshot");
    let mut connection = create_database(Path::new(":memory:"), &snapshot, "canvas.sqlite3")
        .expect("in-memory canvas");
    crate::schema::register_python_trigger_functions(&connection).expect("projection functions");
    let tx = connection.transaction().expect("fixture transaction");
    crate::migrations::prepare_canvas_fixture(&tx).expect("current canvas format");
    tx.commit().expect("fixture commits");
    connection
}

/// A migrated, disposable fresh store copied only within a single test.
#[cfg(any(test, feature = "test-support"))]
pub struct TestStoreTemplate {
    canvas: Vec<u8>,
    analytics: Vec<u8>,
}

#[cfg(any(test, feature = "test-support"))]
impl TestStoreTemplate {
    pub async fn new() -> StoreResult<Self> {
        let root = tempfile::tempdir()?;
        let store = crate::RuntimeStore::open(Some(root.path())).await?;
        store.checkpoint_wal_passive().await?;
        drop(store);
        Ok(Self {
            canvas: tokio::fs::read(root.path().join("canvas.sqlite3")).await?,
            analytics: tokio::fs::read(root.path().join("analytics.sqlite3")).await?,
        })
    }

    pub async fn open(&self, root: &Path) -> StoreResult<crate::RuntimeStore> {
        tokio::fs::create_dir(root).await?;
        tokio::fs::write(root.join("canvas.sqlite3"), &self.canvas).await?;
        tokio::fs::write(root.join("analytics.sqlite3"), &self.analytics).await?;
        let store = crate::RuntimeStore::open(Some(root)).await?;
        let root = store.root().to_path_buf();
        store.transaction(move |tx| {
            tx.execute_batch("DELETE FROM sync_identity; INSERT INTO sync_identity(id) VALUES(lower(hex(randomblob(16))))")?;
            seed_workspace(tx, &root)
        }).await?;
        Ok(store)
    }
}

#[cfg(test)]
mod template_tests {
    use super::TestStoreTemplate;

    #[tokio::test]
    async fn copies_have_distinct_identities_and_workspace_roots() {
        let template = TestStoreTemplate::new().await.expect("template");
        let root = tempfile::tempdir().expect("cases");
        assert!(template.open(root.path()).await.is_err());
        let first = template
            .open(&root.path().join("first"))
            .await
            .expect("first");
        let second = template
            .open(&root.path().join("second"))
            .await
            .expect("second");
        let inspect = |store: &crate::RuntimeStore| {
            let db = store.test_reader().expect("reader");
            let identity: String = db
                .query_row("SELECT id FROM sync_identity", [], |row| row.get(0))
                .expect("identity");
            let state_dir: String = db.query_row("SELECT json_extract(payload, '$.value.stateDir') FROM sync_entities WHERE collection='workspace' AND id='current'", [], |row| row.get(0)).expect("workspace");
            assert_eq!(state_dir, store.root().to_string_lossy().as_ref());
            identity
        };
        assert_ne!(inspect(&first), inspect(&second));
    }
}

fn seed_workspace(transaction: &rusqlite::Transaction<'_>, state_dir: &Path) -> StoreResult<()> {
    let value = json!({
        "connected": false,
        "nativeNotices": [],
        "peerTeamsVersion": 1,
        "projectOrganizationVersion": 1,
        "sidebarOrder": {"groups": null, "revision": 0},
        "stateDir": state_dir.to_string_lossy(),
        "tasksHistoryLimit": 100,
    });
    crate::repositories::sync_documents::SyncDocumentsRepository::put_entity(
        transaction,
        "workspace",
        "current",
        value,
        false,
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO sync_entity_meta(key,value) VALUES('seeded','1')",
        [],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO sync_entity_meta(key,value) VALUES('agent_organization_fields','3')",
        [],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO sync_entity_meta(key,value) VALUES('event_window_seq','0')",
        [],
    )?;
    transaction.execute(
        "INSERT OR IGNORE INTO sync_entity_meta(key,value) VALUES('task_window_migrated','1')",
        [],
    )?;
    // New stores have no summary or conversation-detail source rows; their empty projections are complete.
    for marker in [
        crate::AGENT_PROJECTIONS_SEED_MARKER,
        crate::SUMMARY_ENTITIES_SEED_MARKER,
        crate::CONVERSATION_DETAILS_SEED_MARKER,
        crate::WORK_DETAILS_SEED_MARKER,
    ] {
        transaction.execute(
            "INSERT OR IGNORE INTO sync_entity_meta(key,value) VALUES(?1,'1')",
            [marker],
        )?;
    }
    Ok(())
}

fn publish(staging: &Path, root: &Path) -> StoreResult<()> {
    let mut published = Vec::<PathBuf>::new();
    for name in CORE_DATABASES {
        let from = staging.join(name);
        let to = root.join(name);
        if let Err(error) = fs::rename(&from, &to) {
            published.retain(|path| fs::remove_file(path).is_err());
            return Err(StoreError::StateDirectory(error));
        }
        published.push(to);
    }
    // A supervisor that started first owns its journal, and it can create one
    // at any moment. A hard link never replaces an existing journal, so the
    // staged journal is published only when no supervisor has created one.
    match fs::hard_link(
        staging.join(SUPERVISOR_DATABASE),
        root.join(SUPERVISOR_DATABASE),
    ) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => {
            published.retain(|path| fs::remove_file(path).is_err());
            Err(StoreError::StateDirectory(error))
        }
    }
}

#[cfg(test)]
mod native_layr_tests {
    use super::*;

    #[test]
    fn native_layr_install_can_precede_the_first_backend_start() {
        let root = tempfile::tempdir().expect("fixture operation succeeds");
        let native = root.path().join(crate::NATIVE_LAYR_DIRECTORY);
        fs::create_dir_all(native.join("bin")).expect("fixture operation succeeds");
        fs::write(native.join("bin/layr"), "installed host binary")
            .expect("fixture operation succeeds");
        validate(root.path()).expect("fixture operation succeeds");
        assert!(!root.path().join("canvas.sqlite3").exists());
    }

    #[test]
    fn native_layr_installation_directory_cannot_be_a_file() {
        let root = tempfile::tempdir().expect("fixture operation succeeds");
        fs::write(root.path().join(crate::NATIVE_LAYR_DIRECTORY), "wrong type")
            .expect("fixture operation succeeds");
        assert!(validate(root.path()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn native_layr_installation_directory_cannot_be_a_symlink() {
        let root = tempfile::tempdir().expect("fixture operation succeeds");
        let outside = tempfile::tempdir().expect("fixture operation succeeds");
        std::os::unix::fs::symlink(
            outside.path(),
            root.path().join(crate::NATIVE_LAYR_DIRECTORY),
        )
        .expect("fixture operation succeeds");
        assert!(validate(root.path()).is_err());
    }
}
