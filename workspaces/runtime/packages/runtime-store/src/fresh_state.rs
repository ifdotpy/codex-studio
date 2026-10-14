//! Creates the committed fresh-state catalog when no runtime databases exist.
//!
//! `apps/server/tests/fixtures/runtime-state-schema.json` is generated from the
//! Python startup path by `runtime-state-schema-contract.py`; it is the schema
//! source for this bootstrap. That contract also checks the created catalog
//! against a disposable Python-created state.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::{StoreError, StoreResult};

const SNAPSHOT: &str =
    include_str!("../../../apps/server/tests/fixtures/runtime-state-schema.json");
const CORE_DATABASES: [&str; 2] = ["canvas.sqlite3", "analytics.sqlite3"];
const DATABASES: [&str; 3] = ["canvas.sqlite3", "analytics.sqlite3", "supervisor.sqlite3"];

/// Validate startup contents without creating the runtime lock or changing files.
pub(crate) fn validate(root: &Path) -> StoreResult<()> {
    let databases = DATABASES.map(|name| entry_kind(&root.join(name)));
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
    if core_complete {
        if matches!(databases[2], EntryKind::Other) {
            return Err(StoreError::SchemaMismatch(format!(
                "partial supervisor database at {}",
                root.join(DATABASES[2]).display()
            )));
        }
        return Ok(());
    }
    if !matches!(databases[2], EntryKind::Missing) {
        return Err(StoreError::SchemaMismatch(format!(
            "partial state database at {}: canvas.sqlite3 and analytics.sqlite3 are missing",
            root.join(DATABASES[2]).display()
        )));
    }

    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let path = entry.path();
        let name = entry.file_name();
        let accepted = match name.to_str() {
            // Python creates this lease before its databases.
            Some("runtime.lock") => matches!(entry_kind(&path), EntryKind::File),
            // Runtime.__init__ creates these private directories and their
            // registry/identity files before it opens the SQLite databases.
            Some("accounts") => accepted_python_directory(&path, "registry.json"),
            Some("multi-server") => accepted_python_directory(&path, "identity.json"),
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
    seed_python_lazy_markers(&transaction, staging, final_root)?;
    seed_workspace(&transaction, final_root)?;
    transaction.commit()?;
    drop(canvas);

    let supervisor = crate::supervisor_journal::SupervisorJournal::open(staging)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
    drop(supervisor);
    Ok(())
}

/// Match the marker rows Python creates with its lazy v1 indexes on startup.
fn seed_python_lazy_markers(
    transaction: &rusqlite::Transaction<'_>,
    staging: &Path,
    state_dir: &Path,
) -> StoreResult<()> {
    let created = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let nonce = format!("{}:{}:{}", state_dir.display(), std::process::id(), created);
    let token = Uuid::new_v5(&Uuid::NAMESPACE_OID, nonce.as_bytes())
        .simple()
        .to_string();

    let rootpage = transaction
        .query_row(
            "SELECT rootpage FROM sqlite_master WHERE type='table' AND name='runtime_items'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if let Some(rootpage) = rootpage {
        transaction.execute(
            "INSERT INTO runtime_turn_item_link_state_v1 VALUES (1,?,?,NULL,NULL,1,0,0,?)",
            rusqlite::params![token, rootpage, created as f64 / 1_000_000_000.0],
        )?;
    }
    transaction.execute(
        "INSERT INTO runtime_agent_record_generation(id,value) VALUES(1,0)",
        [],
    )?;
    transaction.execute(
        "INSERT INTO runtime_scheduler_agent_change_state VALUES(1,0,0,0,0)",
        [],
    )?;

    // Python's codex_cost_usage.install inserts a random 128-bit token when
    // it first installs the derived analytics index.
    let analytics_path = staging.join("analytics.sqlite3");
    let analytics = Connection::open(analytics_path)?;
    let has_cost_state = analytics
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_cost_state_v1'",
            [],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    if has_cost_state {
        analytics.execute(
            "INSERT OR IGNORE INTO analytics_cost_state_v1(singleton,token) VALUES(1,?1)",
            [&token],
        )?;
    }
    analytics.execute(
        "INSERT OR IGNORE INTO analytics_meta(key,value) VALUES('trackingSince',?1)",
        [format!("{}", created as f64 / 1_000_000_000.0)],
    )?;
    Ok(())
}

fn create_database(path: &Path, snapshot: &Value, schema_name: &str) -> StoreResult<()> {
    let connection = Connection::open(path)?;
    connection.pragma_update(None, "journal_mode", "WAL")?;
    connection.pragma_update(None, "synchronous", "FULL")?;

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
        let name = entry["name"].as_str().ok_or(StoreError::RecordContract(
            "fresh schema object has no name",
        ))?;
        if kind == "table" && is_virtual_table_shadow(name) {
            continue;
        }
        let sql = entry["sql"]
            .as_str()
            .ok_or(StoreError::RecordContract("fresh schema object has no DDL"))?;
        connection.execute_batch(sql)?;
    }
    let version = snapshot[schema_name]["pragma_user_version"]
        .as_i64()
        .ok_or(StoreError::RecordContract(
            "fresh schema user_version is missing",
        ))?;
    connection.pragma_update(None, "user_version", version)?;
    Ok(())
}

fn is_virtual_table_shadow(name: &str) -> bool {
    [
        "runtime_search_config",
        "runtime_search_content",
        "runtime_search_data",
        "runtime_search_docsize",
        "runtime_search_idx",
    ]
    .contains(&name)
}

fn seed_workspace(transaction: &rusqlite::Transaction<'_>, state_dir: &Path) -> StoreResult<()> {
    let value = json!({
        "connected": false,
        "nativeNotices": [],
        "peerTeamsVersion": 1,
        "projectOrganizationVersion": 1,
        "rateLimits": {"accountKey": "default", "data": null, "at": null, "error": null},
        "rateLimitsByAccount": {},
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
    Ok(())
}

fn publish(staging: &Path, root: &Path) -> StoreResult<()> {
    let mut published = Vec::<PathBuf>::new();
    for name in DATABASES {
        let from = staging.join(name);
        let to = root.join(name);
        if let Err(error) = fs::rename(&from, &to) {
            published.retain(|path| fs::remove_file(path).is_err());
            return Err(StoreError::StateDirectory(error));
        }
        published.push(to);
    }
    Ok(())
}
