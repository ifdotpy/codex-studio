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

/// Prepare all databases for an empty state directory, refusing partial state.
pub(crate) fn prepare(root: &Path) -> StoreResult<bool> {
    let present = DATABASES.map(|name| root.join(name).exists());
    if present.iter().all(|exists| !exists) {
        create_fresh(root)?;
        return Ok(true);
    }
    if !CORE_DATABASES.iter().all(|name| root.join(name).is_file()) {
        return Err(StoreError::SchemaMismatch(
            "partial state directory: canvas.sqlite3 and analytics.sqlite3 must both exist"
                .to_owned(),
        ));
    }
    Ok(false)
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

    for kind in ["table", "index", "trigger"] {
        let database = &snapshot[schema_name];
        let catalog = database["sqlite_master"]
            .as_array()
            .ok_or(StoreError::RecordContract(
                "fresh schema catalog is missing",
            ))?;
        for entry in catalog {
            if entry["type"].as_str() != Some(kind) {
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
