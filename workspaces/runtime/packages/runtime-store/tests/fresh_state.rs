#[path = "../../runtime-services/tests/support/prepared_python.rs"]
mod prepared_python_support;

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_store::{
    ReviewerArchiveRepository, RuntimeStore, SchedulerRepository, StartupRecoveryRepository,
    StoreError, TaskRepository, check_state_readonly,
};

// Rust-owned feature indexes stay outside Python lazy DDL. The runtime-owned
// indexes below have repository DDL metadata checked against Rust sqlite_master
// before this exact named exception is applied to Python catalog parity.
const RUST_OWNED_FEATURE_SCHEMA_INDEXES: &[&str] = &["runtime_agent_callback_context"];
const RUST_OWNED_RUNTIME_INDEX_NAMES: &[&str] = &[
    "runtime_agent_account_wake_candidates_v3",
    "runtime_agent_cleanup_intent",
    "runtime_agent_deleted_unsettled",
    "runtime_agent_reviewer_archive_pending",
    "runtime_agent_reviewer_archive_retry_due",
    "runtime_agent_valid_parent",
    "runtime_event_child_result_pending",
    "runtime_event_reviewer_archive_delivery",
    "runtime_monitor_startup_active",
    "runtime_request_startup_unsettled",
    "runtime_tool_request_startup_unsettled",
];

#[tokio::test]
async fn rust_fresh_state_matches_python_and_python_opens_it_without_schema_migration() {
    let rust_state = temporary_state("rust");
    let feature_schema = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/feature_schema.rs");
    let expected = create_python_state(&rust_state, &feature_schema);
    fs::remove_dir_all(&rust_state).expect("Python oracle state removes");
    fs::create_dir_all(&rust_state).expect("Rust state directory recreates");

    let open_started = std::time::Instant::now();
    let store = RuntimeStore::open(Some(&rust_state))
        .await
        .expect("Rust creates an empty state");
    eprintln!(
        "empty synthetic RuntimeStore::open: {:?}",
        open_started.elapsed()
    );
    let rust_catalog = catalog_snapshot(&rust_state);
    assert_rust_owned_feature_schema_objects(&rust_catalog, &feature_schema);
    assert_rust_owned_runtime_indexes(&rust_catalog);
    assert_shared_task_indexes_match_python(&expected["databases"]);
    assert_valid_markers(&expected["markers"]);
    assert_eq!(
        catalog_without_rust_owned_indexes(&rust_catalog),
        expected["databases"],
        "{}",
        catalog_diff(
            &catalog_without_rust_owned_indexes(&rust_catalog),
            &expected["databases"]
        )
    );
    assert_eq!(sync_seeds(&rust_state), expected["sync_seeds"]);
    assert_eq!(sync_meta(&rust_state), expected["sync_meta"]);
    let rust_markers = marker_snapshot(&rust_state);
    assert_valid_markers(&rust_markers);
    assert_eq!(
        normalize_markers(&rust_markers),
        normalize_markers(&expected["markers"]),
        "Python and Rust startup marker rows differ beyond the named random/time values"
    );
    assert_eq!(
        check_state_readonly(&rust_state)
            .expect("state check accepts Rust state")
            .tables
            .iter()
            .map(|table| table.rejected_count)
            .sum::<u64>(),
        0
    );
    drop(store);

    let opened = open_python_state(&rust_state, &feature_schema);
    assert_eq!(
        opened["before"], opened["after"],
        "Python startup performs no schema migration"
    );
    assert_rust_owned_feature_schema_objects(&opened["before"], &feature_schema);
    assert_rust_owned_runtime_indexes(&opened["before"]);
    assert_eq!(
        catalog_without_rust_owned_indexes(&opened["before"]),
        expected["databases"]
    );
    assert_eq!(opened["sync_seeds"], expected["sync_seeds"]);
    assert_eq!(opened["markers_before"], rust_markers);
    assert_eq!(
        opened["markers_after"], rust_markers,
        "Python startup leaves Rust's initialized migration markers untouched"
    );

    cleanup(rust_state);
}

#[tokio::test]
async fn open_recreates_the_task_indexes_rust_readers_rely_on() {
    let state = temporary_state("task-index");
    drop(
        RuntimeStore::open(Some(&state))
            .await
            .expect("Rust creates an empty state"),
    );
    let index_exists = || {
        Connection::open(state.join("canvas.sqlite3"))
            .expect("canvas opens")
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master \
                 WHERE type='index' AND name='runtime_task_agent_created_id')",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("catalog reads")
    };
    assert!(index_exists());
    Connection::open(state.join("canvas.sqlite3"))
        .expect("canvas opens")
        .execute_batch("DROP INDEX runtime_task_agent_created_id")
        .expect("index drops");
    assert!(!index_exists());
    drop(
        RuntimeStore::open(Some(&state))
            .await
            .expect("Rust reopens the state"),
    );
    assert!(index_exists(), "open recreates the index");
    cleanup(state);
}

#[tokio::test]
async fn partial_database_set_is_refused_without_changing_existing_files() {
    let state = temporary_state("partial");
    let canvas = state.join("canvas.sqlite3");
    fs::write(&canvas, b"partial database sentinel").expect("sentinel writes");
    let before = filesystem_snapshot(&state);

    let error = match RuntimeStore::open(Some(&state)).await {
        Ok(_) => panic!("partial state must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StoreError::SchemaMismatch(message) if message.contains("partial state database") && message.contains("canvas.sqlite3"))
    );
    assert_eq!(filesystem_snapshot(&state), before);
    assert!(!state.join("runtime.lock").exists());
    assert!(!state.join("analytics.sqlite3").exists());
    cleanup(state);
}

#[tokio::test]
async fn foreign_files_are_refused_without_creating_any_state_files() {
    let state = temporary_state("foreign");
    let foreign = state.join("notes.txt");
    fs::write(&foreign, b"keep this file byte-for-byte").expect("foreign sentinel writes");
    let before = filesystem_snapshot(&state);

    let error = match RuntimeStore::open(Some(&state)).await {
        Ok(_) => panic!("foreign state-directory entries must be refused"),
        Err(error) => error,
    };
    assert!(matches!(error, StoreError::SchemaMismatch(message)
        if message.contains("unrecognized entry") && message.contains("notes.txt")));
    assert_eq!(filesystem_snapshot(&state), before);
    assert!(!state.join("runtime.lock").exists());
    cleanup(state);
}

#[tokio::test]
async fn serving_open_claims_the_state_and_a_foreign_owner_is_refused_before_writes() {
    use studio_runtime_store::state_owner::{STATE_OWNER_FILE, StateOwnerClaim};
    let state = temporary_state("serving-owner");
    let first = StateOwnerClaim {
        backend_build: "a".repeat(64),
    };
    let store = RuntimeStore::open_claiming(Some(&state), &first)
        .await
        .expect("a serving Rust open claims a fresh state");
    drop(store);
    let marker: Value =
        serde_json::from_slice(&fs::read(state.join(STATE_OWNER_FILE)).expect("marker"))
            .expect("marker JSON");
    assert_eq!(
        marker,
        json!({"runtime":"rust","formatVersion":1,"backendBuild":"a".repeat(64)})
    );
    let update = StateOwnerClaim {
        backend_build: "b".repeat(64),
    };
    drop(
        RuntimeStore::open_claiming(Some(&state), &update)
            .await
            .expect("a later Rust build with the same state format opens its state"),
    );

    let foreign = temporary_state("foreign-owner");
    let prior = json!({"runtime":"rust","formatVersion":2,"backendBuild":"c".repeat(64)});
    fs::write(foreign.join(STATE_OWNER_FILE), prior.to_string()).expect("foreign marker");
    let before = filesystem_snapshot(&foreign);
    let refused = RuntimeStore::open_claiming(Some(&foreign), &first).await;
    assert!(matches!(refused, Err(StoreError::SchemaMismatch(message))
        if message.contains(STATE_OWNER_FILE)));
    let after: BTreeMap<_, _> = filesystem_snapshot(&foreign)
        .into_iter()
        .filter(|(path, _)| !path.ends_with("runtime.lock"))
        .collect();
    let before: BTreeMap<_, _> = before
        .into_iter()
        .filter(|(path, _)| !path.ends_with("runtime.lock"))
        .collect();
    assert_eq!(after, before, "a refused claim creates no database");
    cleanup(state);
    cleanup(foreign);
}

#[tokio::test]
async fn desktop_rust_state_owner_marker_is_accepted_before_database_creation() {
    let state = temporary_state("desktop-rust-owner");
    let owner = state.join("rust-state-owner.json");
    fs::write(
        &owner,
        br#"{"runtime":"rust","formatVersion":1,"backendBuild":"0123456789abcdef"}"#,
    )
    .expect("desktop owner marker writes");
    let canvas_log = state.join("canvas.log");
    fs::write(&canvas_log, b"desktop startup log\n").expect("desktop log opens before spawn");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for file in [&owner, &canvas_log] {
            fs::set_permissions(file, fs::Permissions::from_mode(0o600))
                .expect("desktop startup files are private");
        }
    }

    let store = RuntimeStore::open(Some(&state))
        .await
        .expect("desktop's preclaimed fresh state opens in the Rust server");
    drop(store);
    assert!(state.join("canvas.sqlite3").is_file());
    assert!(owner.is_file());
    assert!(canvas_log.is_file());
    cleanup(state);
}

#[tokio::test]
async fn python_startup_artifacts_are_accepted_for_a_fresh_state() {
    let state = temporary_state("python-lock");
    let lock = state.join("runtime.lock");
    fs::write(&lock, b"python startup lock").expect("Python lock sentinel writes");
    create_private_directory(&state.join("accounts"));
    create_private_directory(&state.join("multi-server"));

    let store = RuntimeStore::open(Some(&state))
        .await
        .expect("Python startup artifacts are allowed entries");
    drop(store);
    assert_eq!(
        fs::read(lock).expect("lock remains readable"),
        b"python startup lock"
    );
    assert!(state.join("canvas.sqlite3").is_file());
    assert!(state.join("analytics.sqlite3").is_file());
    cleanup(state);
}

/// launchd or systemd can start the supervisor before the first backend. The
/// files below are the ones `supervisor_daemon::serve` creates (lease, open WAL
/// journal and socket) plus the launch agent's supervisor.log.
#[cfg(unix)]
#[tokio::test]
async fn running_rust_supervisor_files_are_accepted_and_its_journal_is_kept() {
    use std::os::unix::fs::MetadataExt;

    // A short name keeps the socket path under the macOS 104-byte limit.
    let state = temporary_state("rsup");
    let journal = studio_runtime_store::supervisor_journal::SupervisorJournal::open(&state)
        .expect("supervisor journal opens first");
    journal
        .reserve("server-command:kept", "signature", 1.0)
        .expect("supervisor reserves a handle");
    journal
        .opened("server-command:kept", "signature", 4242, "start")
        .expect("supervisor records the child");
    fs::write(
        state.join("supervisor.lock"),
        br#"{"pid":4242,"startTime":"start"}"#,
    )
    .expect("supervisor lease writes");
    fs::write(state.join("supervisor.log"), b"supervisor started\n")
        .expect("launch agent log writes");
    let listener = std::os::unix::net::UnixListener::bind(state.join("supervisor.sock"))
        .expect("supervisor socket binds");
    for sidecar in ["supervisor.sqlite3-wal", "supervisor.sqlite3-shm"] {
        assert!(
            state.join(sidecar).is_file(),
            "the open journal has its {sidecar} file"
        );
    }
    let journal_inode = fs::metadata(state.join("supervisor.sqlite3"))
        .expect("journal metadata")
        .ino();

    let store = RuntimeStore::open(Some(&state))
        .await
        .expect("a fresh state with only supervisor files opens");
    drop(store);

    assert!(state.join("canvas.sqlite3").is_file());
    assert!(state.join("analytics.sqlite3").is_file());
    assert_eq!(
        fs::metadata(state.join("supervisor.sqlite3"))
            .expect("journal metadata")
            .ino(),
        journal_inode,
        "the running supervisor's journal is not replaced"
    );
    assert!(
        journal
            .handle("server-command:kept")
            .expect("journal still reads")
            .is_some(),
        "the supervisor keeps its recorded handle"
    );
    assert!(
        fs::read_dir(&state)
            .expect("state entries")
            .filter_map(Result::ok)
            .all(|entry| !entry
                .file_name()
                .to_string_lossy()
                .starts_with(".rust-state-bootstrap-")),
        "the staging directory is removed"
    );
    drop(listener);
    drop(journal);
    cleanup(state);
}

#[tokio::test]
async fn fresh_state_without_supervisor_still_creates_the_supervisor_journal() {
    let state = temporary_state("no-supervisor");
    let store = RuntimeStore::open(Some(&state))
        .await
        .expect("an empty state opens");
    drop(store);
    assert!(state.join("supervisor.sqlite3").is_file());
    cleanup(state);
}

#[tokio::test]
async fn supervisor_files_do_not_hide_partial_or_foreign_state() {
    let cases: [(&str, &[&str], &str); 3] = [
        (
            "one-core-database",
            &["supervisor.sqlite3", "canvas.sqlite3"],
            "partial state database",
        ),
        (
            "stray-wal",
            &["supervisor.lock", "supervisor.sqlite3-wal"],
            "supervisor.sqlite3-wal",
        ),
        (
            "foreign-file",
            &["supervisor.sqlite3", "supervisor.lock", "notes.txt"],
            "notes.txt",
        ),
    ];
    for (name, files, message) in cases {
        let state = temporary_state(name);
        for file in files {
            fs::write(state.join(file), b"sentinel").expect("fixture file writes");
        }
        let before = filesystem_snapshot(&state);
        let error = match RuntimeStore::open(Some(&state)).await {
            Ok(_) => panic!("{name}: state must be refused"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, StoreError::SchemaMismatch(text) if text.contains(message)),
            "{name}: unexpected refusal {error}"
        );
        assert_eq!(filesystem_snapshot(&state), before, "{name}: files changed");
        assert!(!state.join("runtime.lock").exists(), "{name}: lock created");
        cleanup(state);
    }
}

fn create_python_state(root: &Path, feature_schema: &Path) -> Value {
    python(root, feature_schema, PYTHON_CREATE)
}

fn open_python_state(root: &Path, feature_schema: &Path) -> Value {
    python(root, feature_schema, PYTHON_OPEN)
}

fn python(root: &Path, feature_schema: &Path, script: &str) -> Value {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let output = Command::new(prepared_python_support::prepared_python(&source))
        .arg("-c")
        .arg(script)
        .arg(root)
        .arg(&source)
        .arg(feature_schema)
        .arg(
            serde_json::to_string(RUST_OWNED_FEATURE_SCHEMA_INDEXES)
                .expect("Rust-owned schema objects serialize"),
        )
        .arg(
            serde_json::to_string(
                &owned_runtime_indexes()
                    .iter()
                    .map(|definition| {
                        json!({
                            "name": definition.name,
                            "table": definition.table,
                            "create_sql": definition.create_sql,
                        })
                    })
                    .collect::<Vec<_>>(),
            )
            .expect("Rust-owned runtime index metadata serializes"),
        )
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("isolated Python starts for fresh-schema parity");
    assert!(
        output.status.success(),
        "Python schema oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("Python schema oracle returns JSON")
}

fn catalog_without_rust_owned_indexes(catalog: &Value) -> Value {
    let mut common = catalog.clone();
    if let Some(objects) = common["canvas.sqlite3"]["sqlite_master"].as_array_mut() {
        objects.retain(|object| {
            let name = object["name"].as_str().unwrap_or_default();
            !RUST_OWNED_FEATURE_SCHEMA_INDEXES.contains(&name)
                && !RUST_OWNED_RUNTIME_INDEX_NAMES.contains(&name)
        });
    }
    common
}

fn owned_runtime_indexes() -> Vec<studio_runtime_store::OwnedRuntimeIndexDefinition> {
    let mut definitions = vec![SchedulerRepository::account_wake_index_definition()];
    definitions.extend(ReviewerArchiveRepository::index_definitions());
    definitions.extend(SchedulerRepository::child_result_hold_index_definitions());
    definitions.extend(StartupRecoveryRepository::index_definitions());
    definitions.sort_by_key(|definition| definition.name);
    definitions
}

/// Task indexes that Rust readers rely on are also created by Python startup;
/// Rust's `IF NOT EXISTS` DDL must name the same definition.
fn assert_shared_task_indexes_match_python(python_databases: &Value) {
    for definition in TaskRepository::index_definitions() {
        let python = python_databases["canvas.sqlite3"]["sqlite_master"]
            .as_array()
            .and_then(|objects| {
                objects
                    .iter()
                    .find(|object| object["name"] == definition.name)
            })
            .unwrap_or_else(|| panic!("Python creates {}", definition.name));
        assert_eq!(python["table"], definition.table, "{}", definition.name);
        assert_eq!(
            normalize_schema_sql(python["sql"].as_str().expect("index SQL is present")),
            normalize_schema_sql(&definition.create_sql.replace("IF NOT EXISTS ", "")),
            "{} matches the Python definition",
            definition.name
        );
    }
}

fn assert_rust_owned_runtime_indexes(catalog: &Value) {
    let definitions = owned_runtime_indexes();
    let names = definitions
        .iter()
        .map(|definition| definition.name)
        .collect::<Vec<_>>();
    assert_eq!(names.as_slice(), RUST_OWNED_RUNTIME_INDEX_NAMES);
    for definition in definitions {
        let expected_sql =
            normalize_schema_sql(&definition.create_sql.replace("IF NOT EXISTS ", ""));
        let actual = catalog["canvas.sqlite3"]["sqlite_master"]
            .as_array()
            .and_then(|objects| {
                objects
                    .iter()
                    .find(|object| object["name"] == definition.name)
            })
            .unwrap_or_else(|| panic!("Rust startup creates its owned index {}", definition.name));
        assert_eq!(
            actual["type"], "index",
            "{} remains an index",
            definition.name
        );
        assert_eq!(
            actual["table"], definition.table,
            "{} table",
            definition.name
        );
        assert_eq!(
            normalize_schema_sql(actual["sql"].as_str().expect("index SQL is present")),
            expected_sql,
            "{} matches its repository-owned DDL definition",
            definition.name
        );
    }
}

fn assert_rust_owned_feature_schema_objects(catalog: &Value, feature_schema: &Path) {
    let source = fs::read_to_string(feature_schema).expect("canonical feature schema reads");
    for name in RUST_OWNED_FEATURE_SCHEMA_INDEXES {
        let declaration = format!("CREATE INDEX IF NOT EXISTS {name}");
        let start = source
            .find(&declaration)
            .unwrap_or_else(|| panic!("canonical feature schema owns {name}"));
        let remainder = &source[start..];
        let end = remainder
            .find(';')
            .unwrap_or_else(|| panic!("canonical feature schema declaration for {name} ends"));
        let expected_sql = normalize_schema_sql(&remainder[..end].replace("IF NOT EXISTS ", ""));
        let actual = catalog["canvas.sqlite3"]["sqlite_master"]
            .as_array()
            .and_then(|objects| objects.iter().find(|object| object["name"] == *name))
            .unwrap_or_else(|| panic!("Rust startup creates its owned feature index {name}"));
        assert_eq!(actual["type"], "index", "{name} remains an index");
        assert_eq!(
            actual["table"], "runtime_agents",
            "{name} indexes runtime_agents"
        );
        assert_eq!(
            normalize_schema_sql(actual["sql"].as_str().expect("index SQL is present")),
            expected_sql,
            "{name} matches its canonical Rust feature-schema declaration"
        );
    }
}

fn normalize_schema_sql(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .replace("( ", "(")
        .replace(" ,", ",")
        .replace(", ", ",")
        .replace(" )", ")")
}

fn catalog_snapshot(state: &Path) -> Value {
    let mut databases = serde_json::Map::new();
    for name in ["canvas.sqlite3", "analytics.sqlite3", "supervisor.sqlite3"] {
        let connection = Connection::open(state.join(name)).expect("Rust-created database opens");
        let mut statement = connection
            .prepare("SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")
            .expect("catalog query prepares");
        let rows = statement
            .query_map([], |row| {
                Ok(json!({
                    "type": row.get::<_, String>(0)?,
                    "name": row.get::<_, String>(1)?,
                    "table": row.get::<_, String>(2)?,
                    "sql": row.get::<_, Option<String>>(3)?.map(|sql| {
                        sql.split_whitespace().collect::<Vec<_>>().join(" ")
                            .replace("( ", "(").replace(" ,", ",").replace(", ", ",").replace(" )", ")")
                    }),
                }))
            })
            .expect("catalog query executes")
            .collect::<Result<Vec<_>, _>>()
            .expect("catalog reads");
        let journal_mode: String = connection
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode reads");
        let synchronous: i64 = connection
            .query_row("PRAGMA synchronous", [], |row| row.get(0))
            .expect("synchronous reads");
        let user_version: i64 = connection
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .expect("user_version reads");
        databases.insert(
            name.to_owned(),
            json!({
                "sqlite_master": rows,
                "pragmas": {
                    "journal_mode": journal_mode,
                    "synchronous": synchronous,
                    "user_version": user_version,
                }
            }),
        );
        drop(statement);
    }
    Value::Object(databases)
}

fn sync_seeds(state: &Path) -> Value {
    let connection = Connection::open(state.join("canvas.sqlite3")).expect("canvas opens");
    let mut statement = connection
        .prepare(
            "SELECT collection,id,payload,hash,deleted FROM sync_entities ORDER BY collection,id",
        )
        .expect("seed query prepares");
    let rows = statement
        .query_map([], |row| {
            Ok(json!({
                "collection": row.get::<_, String>(0)?,
                "id": row.get::<_, String>(1)?,
                "payload": row.get::<_, Option<String>>(2)?,
                "hash": row.get::<_, String>(3)?,
                "deleted": row.get::<_, i64>(4)? != 0,
            }))
        })
        .expect("seed query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("seed rows read");
    json!(rows)
}

fn sync_meta(state: &Path) -> Value {
    let connection = Connection::open(state.join("canvas.sqlite3")).expect("canvas opens");
    let mut statement = connection
        .prepare("SELECT key,value FROM sync_entity_meta ORDER BY key")
        .expect("meta query prepares");
    let rows = statement
        .query_map([], |row| {
            Ok(json!([row.get::<_, String>(0)?, row.get::<_, String>(1)?]))
        })
        .expect("meta query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("meta reads");
    json!(rows)
}

fn marker_snapshot(state: &Path) -> Value {
    let canvas = Connection::open(state.join("canvas.sqlite3")).expect("canvas marker db opens");
    let turn_link = canvas
        .query_row(
            "SELECT epoch,source_rootpage,cursor,target,complete,scanned_rows,source_bytes,updated \
             FROM runtime_turn_item_link_state_v1 WHERE id=1",
            [],
            |row| {
                Ok(json!({
                    "epoch": row.get::<_, String>(0)?,
                    "source_rootpage": row.get::<_, i64>(1)?,
                    "cursor": row.get::<_, Option<i64>>(2)?,
                    "target": row.get::<_, Option<i64>>(3)?,
                    "complete": row.get::<_, i64>(4)?,
                    "scanned_rows": row.get::<_, i64>(5)?,
                    "source_bytes": row.get::<_, i64>(6)?,
                    "updated": row.get::<_, f64>(7)?,
                }))
            },
        )
        .expect("turn-item migration marker exists");
    let generation = canvas
        .query_row(
            "SELECT value FROM runtime_agent_record_generation WHERE id=1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("agent generation marker exists");
    let scheduler_state = canvas
        .query_row(
            "SELECT floor,generation,entries,total FROM runtime_scheduler_agent_change_state WHERE id=1",
            [],
            |row| {
                Ok(json!([
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                ]))
            },
        )
        .expect("scheduler change marker exists");
    let analytics =
        Connection::open(state.join("analytics.sqlite3")).expect("analytics marker db opens");
    let cost_token = analytics
        .query_row(
            "SELECT token FROM analytics_cost_state_v1 WHERE singleton=1",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("analytics cost migration marker exists");
    let tracking_since = analytics
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='trackingSince'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("analytics tracking start marker exists");
    json!({
        "turn_item_links": turn_link,
        "analytics_cost": cost_token,
        "agent_record_generation": generation,
        "scheduler_change": scheduler_state,
        "analytics_tracking_since": tracking_since,
    })
}

fn assert_valid_markers(markers: &Value) {
    let valid_hex = |value: &Value| {
        value.as_str().is_some_and(|text| {
            text.len() == 32 && text.bytes().all(|byte| byte.is_ascii_hexdigit())
        })
    };
    let turn = &markers["turn_item_links"];
    assert!(
        valid_hex(&turn["epoch"]),
        "turn-item epoch matches Python's UUID hex format"
    );
    assert!(
        turn["source_rootpage"]
            .as_i64()
            .is_some_and(|value| value > 0)
    );
    assert!(turn["cursor"].is_null() && turn["target"].is_null());
    assert_eq!(turn["complete"], 1);
    assert_eq!(turn["scanned_rows"], 0);
    assert_eq!(turn["source_bytes"], 0);
    assert!(turn["updated"].as_f64().is_some_and(f64::is_finite));
    assert!(
        valid_hex(&markers["analytics_cost"]),
        "analytics cost token matches Python's 128-bit lowercase hex format"
    );
    assert_eq!(markers["agent_record_generation"], 0);
    assert_eq!(markers["scheduler_change"], json!([0, 0, 0, 0]));
    assert!(
        markers["analytics_tracking_since"]
            .as_str()
            .and_then(|value| value.parse::<f64>().ok())
            .is_some_and(f64::is_finite)
    );
}

/// Normalize only Python's random UUID/tokens and startup timestamps.
fn normalize_markers(markers: &Value) -> Value {
    let mut normalized = markers.clone();
    normalized["turn_item_links"]["epoch"] = json!("<random-epoch>");
    normalized["turn_item_links"]["updated"] = json!("<startup-time>");
    normalized["analytics_cost"] = json!("<random-cost-token>");
    normalized["analytics_tracking_since"] = json!("<startup-time>");
    normalized
}

fn catalog_diff(actual: &Value, expected: &Value) -> String {
    let mut lines = Vec::new();
    for database in ["canvas.sqlite3", "analytics.sqlite3", "supervisor.sqlite3"] {
        let objects = |catalog: &Value| {
            catalog[database]["sqlite_master"]
                .as_array()
                .into_iter()
                .flatten()
                .map(|entry| {
                    (
                        entry["name"].as_str().unwrap_or_default().to_owned(),
                        entry.clone(),
                    )
                })
                .collect::<std::collections::BTreeMap<_, _>>()
        };
        let actual_objects = objects(actual);
        let expected_objects = objects(expected);
        for name in actual_objects
            .keys()
            .chain(expected_objects.keys())
            .cloned()
            .collect::<std::collections::BTreeSet<_>>()
        {
            if actual_objects.get(&name) != expected_objects.get(&name) {
                lines.push(format!(
                    "{database}:{name}: actual={:?}; expected={:?}",
                    actual_objects.get(&name).map(|entry| &entry["sql"]),
                    expected_objects.get(&name).map(|entry| &entry["sql"])
                ));
            }
        }
        if actual[database]["pragmas"] != expected[database]["pragmas"] {
            lines.push(format!(
                "{database}: pragma actual={} expected={}",
                actual[database]["pragmas"], expected[database]["pragmas"]
            ));
        }
    }
    lines.join("\n")
}

fn temporary_state(name: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let root = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!(
        "studio-fresh-state-{name}-{}-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("test clock is after epoch")
            .as_nanos()
    ));
    if path.exists() {
        fs::remove_dir_all(&path).expect("stale temporary state directory removes");
    }
    fs::create_dir_all(&path).expect("temporary state directory creates");
    path
}

fn cleanup(path: PathBuf) {
    fs::remove_dir_all(path).expect("temporary state directory removes");
}

fn filesystem_snapshot(root: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn visit(root: &Path, directory: &Path, snapshot: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(directory).expect("directory entries read") {
            let path = entry.expect("directory entry reads").path();
            let relative = path
                .strip_prefix(root)
                .expect("entry remains under root")
                .to_path_buf();
            let metadata = fs::symlink_metadata(&path).expect("entry metadata reads");
            if metadata.file_type().is_dir() {
                snapshot.insert(relative, Vec::new());
                visit(root, &path, snapshot);
            } else {
                snapshot.insert(relative, fs::read(path).expect("file bytes read"));
            }
        }
    }

    let mut snapshot = BTreeMap::new();
    visit(root, root, &mut snapshot);
    snapshot
}

fn create_private_directory(path: &Path) {
    fs::create_dir(path).expect("Python startup directory creates");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("Python startup directory is owner-only");
    }
}

const PYTHON_CREATE: &str = r#"
import ast, json, pathlib, re, sqlite3, sys
sys.path.insert(0, str(pathlib.Path(sys.argv[2]).resolve().parents[0] / "tests"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
sys.path.insert(0, sys.argv[2])
from codex_canvas import Canvas
from codex_runtime import Runtime
from runtime_test_environment import make_runtime
from codex_process_supervisor import Journal

class QuietRuntime(Runtime):
    def schedule(self): pass

root = pathlib.Path(sys.argv[1])
def install_lazy_ddl(connection):
    feature_sql = pathlib.Path(sys.argv[3]).read_text()
    targets = set(re.findall(r"CREATE\s+(?:VIRTUAL\s+)?(?:TABLE|INDEX|TRIGGER)\s+IF NOT EXISTS\s+([A-Za-z_][A-Za-z_0-9]*)", feature_sql, re.I))
    rust_owned = set(json.loads(sys.argv[4]))
    if rust_owned - targets:
        raise AssertionError("Rust-owned feature schema object is absent from canonical schema: " + ", ".join(sorted(rust_owned - targets)))
    runtime_owned = {entry["name"] for entry in json.loads(sys.argv[5])}
    created = set(row[0] for row in connection.execute("SELECT name FROM sqlite_master"))
    statements = {}
    pattern = re.compile(r"CREATE\s+(?:VIRTUAL\s+)?(?:TABLE|INDEX)\s+(?:IF NOT EXISTS\s+)?([A-Za-z_][A-Za-z_0-9]*)\b", re.I | re.S)
    for owner in sorted(pathlib.Path(sys.argv[2]).glob("codex_*.py")):
        tree = ast.parse(owner.read_text(), filename=str(owner))
        for node in ast.walk(tree):
            if isinstance(node, ast.Call) and node.args:
                value = node.args[0]
                if isinstance(value, ast.Constant) and isinstance(value.value, str):
                    for sql in value.value.split(';'):
                        match = pattern.search(sql)
                        if match and match.group(1) in targets | runtime_owned:
                            statements.setdefault(match.group(1), sql.strip())
    if runtime_owned & set(statements):
        raise AssertionError("Rust-owned runtime index also has a Python DDL owner: " + ", ".join(sorted(runtime_owned & set(statements))))
    if rust_owned & set(statements):
        raise AssertionError("Rust-owned feature schema object also has a Python DDL owner: " + ", ".join(sorted(rust_owned & set(statements))))
    for name, statement in sorted(statements.items(), key=lambda item: "INDEX" in item[1].upper()):
        if "IF NOT EXISTS" in statement.upper() or name not in created:
            connection.execute(statement)
    missing = targets - created - set(statements) - rust_owned
    if missing:
        raise AssertionError("Python lazy DDL owner not found for: " + ", ".join(sorted(missing)))

canvas = Canvas(root=root)
runtime = make_runtime(QuietRuntime, root, lambda *_args, **_kwargs: None)
with runtime.db() as connection:
    install_lazy_ddl(connection)
from codex_sync import SyncStore
from codex_sync_entities import seed
sync_store = SyncStore(runtime.db, lambda *_args, **_kwargs: None)
sync_store.generation()
with runtime.db() as connection:
    seed(connection, runtime_owner=runtime, canvas_owner=canvas)
journal = Journal(root)
journal.close()
def db_snapshot(connection, schema):
    normalize = lambda sql: re.sub(r"\s+", " ", re.sub(r"\(\s+", "(", re.sub(r"\s*,\s*", ",", re.sub(r"\s+\)", ")", sql)))).strip()
    objects = [dict(type=kind, name=name, table=table, sql=normalize(sql) if sql else None) for kind,name,table,sql in connection.execute(
        f"SELECT type,name,tbl_name,sql FROM {schema}.sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")]
    return {"sqlite_master": objects, "pragmas": {
        "journal_mode": connection.execute(f"PRAGMA {schema}.journal_mode").fetchone()[0],
        "synchronous": connection.execute(f"PRAGMA {schema}.synchronous").fetchone()[0],
        "user_version": connection.execute(f"PRAGMA {schema}.user_version").fetchone()[0]}}
with runtime.db(analytics=True) as connection:
    databases = {"canvas.sqlite3": db_snapshot(connection, "main"),
                 "analytics.sqlite3": db_snapshot(connection, "analytics")}
supervisor = sqlite3.connect(root / "supervisor.sqlite3")
databases["supervisor.sqlite3"] = db_snapshot(supervisor, "main")
supervisor.close()
with runtime.db() as connection:
    seeds = [dict(collection=c,id=i,payload=p,hash=h,deleted=bool(d)) for c,i,p,h,d in connection.execute(
        "SELECT collection,id,payload,hash,deleted FROM sync_entities ORDER BY collection,id")]
    meta = [list(row) for row in connection.execute("SELECT key,value FROM sync_entity_meta ORDER BY key")]
def markers():
    with runtime.db() as connection:
        turn = connection.execute("SELECT epoch,source_rootpage,cursor,target,complete,scanned_rows,source_bytes,updated "
                                  "FROM runtime_turn_item_link_state_v1 WHERE id=1").fetchone()
        generation = connection.execute("SELECT value FROM runtime_agent_record_generation WHERE id=1").fetchone()[0]
        scheduler = connection.execute("SELECT floor,generation,entries,total "
                                        "FROM runtime_scheduler_agent_change_state WHERE id=1").fetchone()
    with runtime.analytics_db() as connection:
        cost = connection.execute("SELECT token FROM analytics_cost_state_v1 WHERE singleton=1").fetchone()[0]
        tracking_since = connection.execute("SELECT value FROM analytics_meta WHERE key='trackingSince'").fetchone()[0]
    return {"turn_item_links": {"epoch":turn[0],"source_rootpage":turn[1],"cursor":turn[2],"target":turn[3],
                                "complete":turn[4],"scanned_rows":turn[5],"source_bytes":turn[6],"updated":turn[7]},
            "analytics_cost":cost,"agent_record_generation":generation,"scheduler_change":list(scheduler),
            "analytics_tracking_since":tracking_since}
state_markers = markers()
runtime.close()
print(json.dumps({"databases": databases, "sync_seeds": seeds, "sync_meta": meta, "markers":state_markers}, sort_keys=True))
"#;

const PYTHON_OPEN: &str = r#"
import ast, json, pathlib, re, sqlite3, sys
sys.path.insert(0, str(pathlib.Path(sys.argv[2]).resolve().parents[0] / "tests"))
from test_isolation import isolate_supervisor_environment
isolate_supervisor_environment()
sys.path.insert(0, sys.argv[2])
from codex_canvas import Canvas
from codex_runtime import Runtime
from runtime_test_environment import make_runtime

class QuietRuntime(Runtime):
    def schedule(self): pass

root = pathlib.Path(sys.argv[1])
def install_lazy_ddl(connection):
    feature_sql = pathlib.Path(sys.argv[3]).read_text()
    targets = set(re.findall(r"CREATE\s+(?:VIRTUAL\s+)?(?:TABLE|INDEX|TRIGGER)\s+IF NOT EXISTS\s+([A-Za-z_][A-Za-z_0-9]*)", feature_sql, re.I))
    rust_owned = set(json.loads(sys.argv[4]))
    if rust_owned - targets:
        raise AssertionError("Rust-owned feature schema object is absent from canonical schema: " + ", ".join(sorted(rust_owned - targets)))
    runtime_owned = {entry["name"] for entry in json.loads(sys.argv[5])}
    created = set(row[0] for row in connection.execute("SELECT name FROM sqlite_master"))
    statements = {}
    pattern = re.compile(r"CREATE\s+(?:VIRTUAL\s+)?(?:TABLE|INDEX)\s+(?:IF NOT EXISTS\s+)?([A-Za-z_][A-Za-z_0-9]*)\b", re.I | re.S)
    for owner in sorted(pathlib.Path(sys.argv[2]).glob("codex_*.py")):
        tree = ast.parse(owner.read_text(), filename=str(owner))
        for node in ast.walk(tree):
            if isinstance(node, ast.Call) and node.args:
                value = node.args[0]
                if isinstance(value, ast.Constant) and isinstance(value.value, str):
                    for sql in value.value.split(';'):
                        match = pattern.search(sql)
                        if match and match.group(1) in targets | runtime_owned:
                            statements.setdefault(match.group(1), sql.strip())
    if runtime_owned & set(statements):
        raise AssertionError("Rust-owned runtime index also has a Python DDL owner: " + ", ".join(sorted(runtime_owned & set(statements))))
    if rust_owned & set(statements):
        raise AssertionError("Rust-owned feature schema object also has a Python DDL owner: " + ", ".join(sorted(rust_owned & set(statements))))
    for name, statement in sorted(statements.items(), key=lambda item: "INDEX" in item[1].upper()):
        if "IF NOT EXISTS" in statement.upper() or name not in created:
            connection.execute(statement)
    missing = targets - created - set(statements) - rust_owned
    if missing:
        raise AssertionError("Python lazy DDL owner not found for: " + ", ".join(sorted(missing)))

def snapshot():
    normalize = lambda sql: re.sub(r"\s+", " ", re.sub(r"\(\s+", "(", re.sub(r"\s*,\s*", ",", re.sub(r"\s+\)", ")", sql)))).strip()
    with sqlite3.connect(root / "canvas.sqlite3") as connection:
        main = [dict(type=k,name=n,table=t,sql=normalize(s) if s else None) for k,n,t,s in connection.execute(
            "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")]
        version = connection.execute("PRAGMA user_version").fetchone()[0]
        journal = connection.execute("PRAGMA journal_mode").fetchone()[0]
        synchronous = connection.execute("PRAGMA synchronous").fetchone()[0]
    with sqlite3.connect(root / "analytics.sqlite3") as connection:
        analytics = [dict(type=k,name=n,table=t,sql=normalize(s) if s else None) for k,n,t,s in connection.execute(
            "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")]
        analytics_version = connection.execute("PRAGMA user_version").fetchone()[0]
        analytics_journal = connection.execute("PRAGMA journal_mode").fetchone()[0]
        analytics_synchronous = connection.execute("PRAGMA synchronous").fetchone()[0]
    with sqlite3.connect(root / "supervisor.sqlite3") as connection:
        supervisor = [dict(type=k,name=n,table=t,sql=normalize(s) if s else None) for k,n,t,s in connection.execute(
            "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")]
        supervisor_version = connection.execute("PRAGMA user_version").fetchone()[0]
        supervisor_journal = connection.execute("PRAGMA journal_mode").fetchone()[0]
        supervisor_synchronous = connection.execute("PRAGMA synchronous").fetchone()[0]
    return {"canvas.sqlite3":{"sqlite_master":main,"pragmas":{"journal_mode":journal,"synchronous":synchronous,"user_version":version}},
            "analytics.sqlite3":{"sqlite_master":analytics,"pragmas":{"journal_mode":analytics_journal,"synchronous":analytics_synchronous,"user_version":analytics_version}},
            "supervisor.sqlite3":{"sqlite_master":supervisor,"pragmas":{"journal_mode":supervisor_journal,"synchronous":supervisor_synchronous,"user_version":supervisor_version}}}

before = snapshot()
def markers():
    with sqlite3.connect(root / "canvas.sqlite3") as connection:
        turn = connection.execute("SELECT epoch,source_rootpage,cursor,target,complete,scanned_rows,source_bytes,updated "
                                  "FROM runtime_turn_item_link_state_v1 WHERE id=1").fetchone()
        generation = connection.execute("SELECT value FROM runtime_agent_record_generation WHERE id=1").fetchone()[0]
        scheduler = connection.execute("SELECT floor,generation,entries,total "
                                       "FROM runtime_scheduler_agent_change_state WHERE id=1").fetchone()
    with sqlite3.connect(root / "analytics.sqlite3") as connection:
        cost = connection.execute("SELECT token FROM analytics_cost_state_v1 WHERE singleton=1").fetchone()[0]
        tracking_since = connection.execute("SELECT value FROM analytics_meta WHERE key='trackingSince'").fetchone()[0]
    return {"turn_item_links": {"epoch":turn[0],"source_rootpage":turn[1],"cursor":turn[2],"target":turn[3],
                                "complete":turn[4],"scanned_rows":turn[5],"source_bytes":turn[6],"updated":turn[7]},
            "analytics_cost":cost,"agent_record_generation":generation,"scheduler_change":list(scheduler),
            "analytics_tracking_since":tracking_since}
markers_before = markers()
canvas = Canvas(root=root)
runtime = make_runtime(QuietRuntime, root, lambda *_args, **_kwargs: None)
from codex_sync import SyncStore
from codex_sync_entities import seed
sync_store = SyncStore(runtime.db, lambda *_args, **_kwargs: None)
sync_store.generation()
with runtime.db() as connection:
    install_lazy_ddl(connection)
    seed(connection, runtime_owner=runtime, canvas_owner=canvas)
    for (name,) in connection.execute("SELECT name FROM sqlite_master WHERE type='table' AND name NOT LIKE 'sqlite_%'"):
        connection.execute('SELECT * FROM "' + name.replace('"', '""') + '" LIMIT 0')
    seeds = [dict(collection=c,id=i,payload=p,hash=h,deleted=bool(d)) for c,i,p,h,d in connection.execute(
        "SELECT collection,id,payload,hash,deleted FROM sync_entities ORDER BY collection,id")]
runtime.close()
print(json.dumps({"before":before,"after":snapshot(),"sync_seeds":seeds,
                  "markers_before":markers_before,"markers_after":markers()},sort_keys=True))
"#;
