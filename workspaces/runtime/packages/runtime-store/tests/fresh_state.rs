use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, StoreError};

#[tokio::test]
async fn retired_sqlite_families_are_absent_on_fresh_open() {
    let root = temporary_state("retired");
    drop(RuntimeStore::open(Some(root.path())).await.expect("open"));
    for (file, names) in [
        (
            "canvas.sqlite3",
            &[
                "runtime_turn_item_links_v1",
                "runtime_turn_item_link_state_v1",
                "runtime_budget_usage",
                "runtime_model_modes",
                "runtime_native_catalog_updates",
                "runtime_native_limit_refreshes",
                "runtime_native_sweeps",
                "runtime_supervisor_cursor",
                "runtime_server_sequence",
                "runtime_payload_refs",
                "runtime_search",
                "runtime_search_indexed",
                "runtime_search_rows",
                "runtime_search_rows_rollout",
            ][..],
        ),
        (
            "analytics.sqlite3",
            &[
                "analytics_cost_changes_v1",
                "analytics_cost_roots_v1",
                "analytics_cost_state_v1",
                "analytics_cost_projection_v1",
            ][..],
        ),
    ] {
        let db = Connection::open(root.path().join(file)).expect("database");
        for name in names {
            let exists: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                    [name],
                    |row| row.get(0),
                )
                .expect("catalog");
            assert!(!exists, "{file}: retired object {name} remains");
        }
    }
}

#[tokio::test]
async fn retired_sqlite_upgrade_preserves_records_and_parent_writes() {
    let root = temporary_state("retired-upgrade");
    drop(RuntimeStore::open(Some(root.path())).await.expect("fresh"));
    let fixture: Value = serde_json::from_str(include_str!("fixtures/retired-sqlite-schema.json"))
        .expect("old catalog");
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        let db = Connection::open(root.path().join(file)).expect("database");
        for entry in fixture[file]["sqlite_master"].as_array().expect("objects") {
            db.execute_batch(entry["sql"].as_str().expect("SQL"))
                .expect("old object");
        }
        if file == "canvas.sqlite3" {
            db.execute_batch("CREATE TABLE runtime_model_modes(agent TEXT, request TEXT, version TEXT, record TEXT, PRIMARY KEY(agent,request,version)); CREATE TABLE runtime_native_catalog_updates(id TEXT PRIMARY KEY, record TEXT NOT NULL); CREATE TABLE runtime_native_limit_refreshes(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_budget VALUES('kept','{\"replayConflicts\":[\"sentinel\"]}'); INSERT INTO runtime_budget_usage(id,agent,kind,tokens,at) VALUES('old','agent','response',9,1); INSERT INTO runtime_payload_refs VALUES('runtime_checkpoints','old','body','hash'); INSERT INTO runtime_search VALUES('id','agent','kind','old text');").expect("old data");
            db.execute_batch("INSERT INTO runtime_model_modes VALUES('agent','request','version','{}'); INSERT INTO runtime_native_catalog_updates VALUES('old','{}'); INSERT INTO runtime_native_limit_refreshes VALUES('old','{}'); INSERT INTO runtime_native_sweeps VALUES('old','{}'); INSERT INTO runtime_supervisor_cursor VALUES('old',1,'epoch'); INSERT INTO runtime_server_sequence VALUES(1,1); INSERT INTO runtime_turn_item_links_v1 VALUES('old','agent','turn',1,'epoch'); INSERT INTO runtime_turn_item_link_state_v1 VALUES(1,'epoch',1,NULL,NULL,1,0,0,1); INSERT INTO runtime_search_indexed VALUES('old'); INSERT INTO runtime_search_rows VALUES('old',1); INSERT INTO runtime_search_rows_rollout VALUES(1,1);").expect("retired rows");
        } else {
            db.execute_batch("INSERT INTO analytics_cost_state_v1 VALUES(1,'old'); INSERT INTO analytics_cost_roots_v1 VALUES('root',1,0); INSERT INTO analytics_cost_changes_v1 VALUES('root','old',1);").expect("old cost data");
            db.execute(
                "INSERT INTO analytics_cost_projection_v1 VALUES('projection','root','[1]')",
                [],
            )
            .expect("retired projection row");
        }
    }
    drop(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("upgrade"),
    );
    let canvas = Connection::open(root.path().join("canvas.sqlite3")).expect("canvas");
    let budget: String = canvas
        .query_row(
            "SELECT record FROM runtime_budget WHERE id='kept'",
            [],
            |row| row.get(0),
        )
        .expect("budget");
    assert_eq!(budget, "{\"replayConflicts\":[\"sentinel\"]}");
    for (table, column) in [
        ("runtime_checkpoints", "record"),
        ("runtime_tool_requests", "record"),
        ("runtime_tool_results", "result"),
    ] {
        canvas
            .execute(
                &format!("INSERT INTO {table}(id,{column}) VALUES(?1,?2)"),
                [
                    "test",
                    "{\"_payloadBlobs\":{\"body\":{\"sha256\":\"hash\"}}}",
                ],
            )
            .expect("insert parent");
        canvas
            .execute(
                &format!("UPDATE {table} SET {column}=?1 WHERE id='test'"),
                ["{}"],
            )
            .expect("update parent");
        canvas
            .execute(&format!("DELETE FROM {table} WHERE id='test'"), [])
            .expect("delete parent");
    }
    let schema_version: i64 = canvas
        .pragma_query_value(None, "schema_version", |row| row.get(0))
        .expect("schema version");
    drop(canvas);
    drop(RuntimeStore::open(Some(root.path())).await.expect("reopen"));
    let canvas = Connection::open(root.path().join("canvas.sqlite3")).expect("canvas");
    assert_eq!(
        schema_version,
        canvas
            .pragma_query_value(None, "schema_version", |row| row.get::<_, i64>(0))
            .expect("schema version")
    );
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        let db = Connection::open(root.path().join(file)).expect("database");
        for entry in fixture[file]["sqlite_master"].as_array().expect("objects") {
            let name = entry["name"].as_str().expect("name");
            let exists: bool = db
                .query_row(
                    "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                    [name],
                    |row| row.get(0),
                )
                .expect("catalog");
            assert!(!exists, "{file}: {name} remains");
        }
    }
    for name in [
        "runtime_model_modes",
        "runtime_native_catalog_updates",
        "runtime_native_limit_refreshes",
        "runtime_search_config",
        "runtime_search_content",
        "runtime_search_data",
        "runtime_search_docsize",
        "runtime_search_idx",
    ] {
        let exists: bool = canvas
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1)",
                [name],
                |row| row.get(0),
            )
            .expect("catalog");
        assert!(!exists, "{name} remains");
    }
}

#[tokio::test]
async fn open_recreates_the_task_indexes_rust_readers_rely_on() {
    let state_directory = temporary_state("task-index");
    let state = state_directory.path();
    drop(
        RuntimeStore::open(Some(state))
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
        RuntimeStore::open(Some(state))
            .await
            .expect("Rust reopens the state"),
    );
    assert!(index_exists(), "open recreates the index");
}

#[tokio::test]
async fn partial_database_set_is_refused_without_changing_existing_files() {
    let state_directory = temporary_state("partial");
    let state = state_directory.path();
    let canvas = state.join("canvas.sqlite3");
    fs::write(&canvas, b"partial database sentinel").expect("sentinel writes");
    let before = filesystem_snapshot(state);

    let error = match RuntimeStore::open(Some(state)).await {
        Ok(_) => panic!("partial state must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StoreError::SchemaMismatch(message) if message.contains("partial state database") && message.contains("canvas.sqlite3"))
    );
    assert_eq!(filesystem_snapshot(state), before);
    assert!(!state.join("runtime.lock").exists());
    assert!(!state.join("analytics.sqlite3").exists());
}

#[tokio::test]
async fn foreign_files_are_refused_without_creating_any_state_files() {
    let state_directory = temporary_state("foreign");
    let state = state_directory.path();
    let foreign = state.join("notes.txt");
    fs::write(foreign, b"keep this file byte-for-byte").expect("foreign sentinel writes");
    let before = filesystem_snapshot(state);

    let error = match RuntimeStore::open(Some(state)).await {
        Ok(_) => panic!("foreign state-directory entries must be refused"),
        Err(error) => error,
    };
    assert!(matches!(error, StoreError::SchemaMismatch(message)
        if message.contains("unrecognized entry") && message.contains("notes.txt")));
    assert_eq!(filesystem_snapshot(state), before);
    assert!(!state.join("runtime.lock").exists());
}

#[tokio::test]
async fn serving_open_claims_the_state_and_a_foreign_owner_is_refused_before_writes() {
    use studio_runtime_store::state_owner::{STATE_OWNER_FILE, StateOwnerClaim};
    let state_directory = temporary_state("serving-owner");
    let state = state_directory.path();
    let first = StateOwnerClaim {
        backend_build: "a".repeat(64),
    };
    let store = RuntimeStore::open_claiming(Some(state), &first)
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
        RuntimeStore::open_claiming(Some(state), &update)
            .await
            .expect("a later Rust build with the same state format opens its state"),
    );

    let foreign_directory = temporary_state("foreign-owner");
    let foreign = foreign_directory.path();
    let prior = json!({"runtime":"rust","formatVersion":2,"backendBuild":"c".repeat(64)});
    fs::write(foreign.join(STATE_OWNER_FILE), prior.to_string()).expect("foreign marker");
    let before = filesystem_snapshot(foreign);
    let refused = RuntimeStore::open_claiming(Some(foreign), &first).await;
    assert!(matches!(refused, Err(StoreError::SchemaMismatch(message))
        if message.contains(STATE_OWNER_FILE)));
    let after: BTreeMap<_, _> = filesystem_snapshot(foreign)
        .into_iter()
        .filter(|(path, _)| !path.ends_with("runtime.lock"))
        .collect();
    let before: BTreeMap<_, _> = before
        .into_iter()
        .filter(|(path, _)| !path.ends_with("runtime.lock"))
        .collect();
    assert_eq!(after, before, "a refused claim creates no database");
}

#[tokio::test]
async fn desktop_rust_state_owner_marker_is_accepted_before_database_creation() {
    let state_directory = temporary_state("desktop-rust-owner");
    let state = state_directory.path();
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

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("desktop's preclaimed fresh state opens in the Rust server");
    drop(store);
    assert!(state.join("canvas.sqlite3").is_file());
    assert!(owner.is_file());
    assert!(canvas_log.is_file());
}

#[tokio::test]
async fn python_startup_artifacts_are_accepted_for_a_fresh_state() {
    let state_directory = temporary_state("python-lock");
    let state = state_directory.path();
    let lock = state.join("runtime.lock");
    fs::write(&lock, b"python startup lock").expect("Python lock sentinel writes");
    create_private_directory(&state.join("accounts"));
    create_private_directory(&state.join("multi-server"));

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("Python startup artifacts are allowed entries");
    drop(store);
    assert_eq!(
        fs::read(lock).expect("lock remains readable"),
        b"python startup lock"
    );
    assert!(state.join("canvas.sqlite3").is_file());
    assert!(state.join("analytics.sqlite3").is_file());
}

/// launchd or systemd can start the supervisor before the first backend. The
/// files below are the ones `supervisor_daemon::serve` creates (lease, open WAL
/// journal and socket) plus the launch agent's supervisor.log.
#[cfg(unix)]
#[tokio::test]
async fn running_rust_supervisor_files_are_accepted_and_its_journal_is_kept() {
    use std::os::unix::fs::MetadataExt;

    let state_directory = temporary_state("rsup");
    let state = state_directory.path();
    let journal = studio_runtime_store::supervisor_journal::SupervisorJournal::open(state)
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

    let store = RuntimeStore::open(Some(state))
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
        fs::read_dir(state)
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
}

/// A new Mac can turn on background recovery before the first backend starts. The
/// recovery helper (desktop recovery.cjs) writes its private configuration, the recovery
/// job (recover_backend.py, umask 077) opens its private lock, and launchd opens the
/// job's log with its own umask; then the job starts the first backend.
#[cfg(unix)]
#[tokio::test]
async fn recovery_files_written_before_the_first_start_are_accepted() {
    use std::os::unix::fs::PermissionsExt;

    let state_directory = temporary_state("recovery-first");
    let state = state_directory.path();
    let configuration = br#"{"version":1,"enabled":true}"#;
    for (name, body, mode) in [
        ("background-recovery.json", &configuration[..], 0o600),
        ("background-recovery.lock", &b""[..], 0o600),
        (
            "background-recovery.log",
            &b"{\"event\": \"backend-recovery\"}\n"[..],
            0o644,
        ),
    ] {
        fs::write(state.join(name), body).expect("recovery file writes");
        fs::set_permissions(state.join(name), fs::Permissions::from_mode(mode))
            .expect("recovery file mode sets");
    }

    let store = RuntimeStore::open(Some(state))
        .await
        .expect("recovery files written before the first start are allowed entries");
    drop(store);
    assert!(state.join("canvas.sqlite3").is_file());
    assert_eq!(
        fs::read(state.join("background-recovery.json")).expect("configuration reads"),
        configuration,
        "the recovery configuration is kept"
    );
}

/// The recovery configuration names the saved launch environment; a readable copy is not
/// one that the recovery helper wrote, so a fresh state still refuses it.
#[cfg(unix)]
#[tokio::test]
async fn a_readable_recovery_configuration_is_still_refused() {
    use std::os::unix::fs::PermissionsExt;

    let state_directory = temporary_state("recovery-readable");
    let state = state_directory.path();
    fs::write(state.join("background-recovery.json"), b"{}").expect("configuration writes");
    fs::set_permissions(
        state.join("background-recovery.json"),
        fs::Permissions::from_mode(0o644),
    )
    .expect("configuration mode sets");
    let error = RuntimeStore::open(Some(state))
        .await
        .err()
        .expect("a readable recovery configuration is refused");
    assert!(
        matches!(error, StoreError::SchemaMismatch(ref message) if message.contains("background-recovery.json")),
        "{error}"
    );
    assert!(!state.join("canvas.sqlite3").exists());
}

#[tokio::test]
async fn fresh_state_without_supervisor_still_creates_the_supervisor_journal() {
    let state_directory = temporary_state("no-supervisor");
    let state = state_directory.path();
    let store = RuntimeStore::open(Some(state))
        .await
        .expect("an empty state opens");
    drop(store);
    assert!(state.join("supervisor.sqlite3").is_file());
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
        let state_directory = temporary_state(name);
        let state = state_directory.path();
        for file in files {
            fs::write(state.join(file), b"sentinel").expect("fixture file writes");
        }
        let before = filesystem_snapshot(state);
        let error = match RuntimeStore::open(Some(state)).await {
            Ok(_) => panic!("{name}: state must be refused"),
            Err(error) => error,
        };
        assert!(
            matches!(&error, StoreError::SchemaMismatch(text) if text.contains(message)),
            "{name}: unexpected refusal {error}"
        );
        assert_eq!(filesystem_snapshot(state), before, "{name}: files changed");
        assert!(!state.join("runtime.lock").exists(), "{name}: lock created");
    }
}

/// A random suffix keeps parallel test processes apart. The short prefix keeps
/// `supervisor.sock` under the macOS 104-byte socket path limit, also below a
/// long per-user TMPDIR such as /var/folders/<id>/T.
fn temporary_state(name: &str) -> tempfile::TempDir {
    tempfile::Builder::new()
        .prefix(&format!("sfs-{name}-"))
        .tempdir()
        .expect("temporary state directory creates")
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

#[tokio::test]
async fn rust_fresh_state_catalog_is_deterministic_and_survives_reopen() {
    fn catalog(root: &Path) -> BTreeMap<String, Vec<(String, String, String)>> {
        ["canvas.sqlite3", "analytics.sqlite3"]
            .into_iter()
            .map(|name| {
                let db = Connection::open(root.join(name)).expect("fresh database opens");
                let mut statement = db.prepare(
                    "SELECT type,name,sql FROM sqlite_master WHERE sql IS NOT NULL ORDER BY type,name",
                ).expect("fresh catalog query prepares");
                let objects = statement.query_map([], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                }).expect("fresh catalog query runs")
                  .collect::<Result<Vec<_>, _>>().expect("fresh catalog reads");
                assert!(!objects.is_empty(), "{name} has a schema");
                let journal: String = db.pragma_query_value(None, "journal_mode", |row| row.get(0))
                    .expect("journal mode reads");
                assert_eq!(journal, "wal");
                (name.to_owned(), objects)
            }).collect()
    }
    let first = temporary_state("catalog-first");
    let second = temporary_state("catalog-second");
    drop(
        RuntimeStore::open(Some(first.path()))
            .await
            .expect("first fresh state opens"),
    );
    let snapshot = catalog(first.path());
    drop(
        RuntimeStore::open(Some(second.path()))
            .await
            .expect("second fresh state opens"),
    );
    assert_eq!(
        catalog(second.path()),
        snapshot,
        "fresh roots have the same catalog"
    );
    drop(
        RuntimeStore::open(Some(first.path()))
            .await
            .expect("fresh state reopens"),
    );
    assert_eq!(
        catalog(first.path()),
        snapshot,
        "reopen preserves the full catalog"
    );
}

#[tokio::test]
async fn scheduler_probes_use_bounded_indexes() {
    let root = temporary_state("scheduler-indexes");
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("fresh store");
    let db = store.test_reader().expect("reader");
    for (sql, index) in [
        (
            "EXPLAIN QUERY PLAN SELECT json_extract(result,'$.taskId'),result FROM runtime_operation_receipts WHERE signature='accepted-archive-v1' AND json_extract(result,'$.status') IN ('pending','retryable','unknown') AND COALESCE(json_extract(result,'$.nextAttemptAt'),0)<=1 ORDER BY COALESCE(json_extract(result,'$.nextAttemptAt'),0),id LIMIT 20",
            "runtime_accepted_archive_due",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT CAST(json_extract(result,'$.nextAttemptAt') AS REAL) FROM runtime_operation_receipts WHERE signature='accepted-archive-v1' AND json_extract(result,'$.status') IN ('pending','retryable','unknown') AND COALESCE(json_extract(result,'$.nextAttemptAt'),0)>1 AND json_type(result,'$.nextAttemptAt') IN ('integer','real') AND CAST(json_extract(result,'$.nextAttemptAt') AS REAL)>1 ORDER BY COALESCE(json_extract(result,'$.nextAttemptAt'),0),id LIMIT 1",
            "runtime_accepted_archive_due",
        ),
        (
            "EXPLAIN QUERY PLAN SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE legacy_unsettled=1)",
            "runtime_agent_unsettled_legacy",
        ),
    ] {
        let mut statement = db.prepare(sql).expect("plan");
        let plan: Vec<String> = statement
            .query_map([], |row| row.get(3))
            .expect("query")
            .collect::<Result<_, _>>()
            .expect("rows");
        assert!(plan.iter().any(|line| line.contains(index)), "{plan:?}");
        if index == "runtime_accepted_archive_due" {
            assert!(plan.iter().any(|line| line.contains("SEARCH")), "{plan:?}");
        }
        assert!(
            !plan.iter().any(|line| line.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }
}

#[tokio::test]
async fn scheduler_indexes_upgrade_and_rollback_with_retained_receipts() {
    let root = temporary_state("scheduler-index-upgrade");
    drop(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("fresh store"),
    );
    let db = Connection::open(root.path().join("canvas.sqlite3")).expect("canvas");
    let catalog = |db: &Connection| -> Vec<(String, String)> {
        db.prepare("SELECT name,sql FROM sqlite_master WHERE name IN ('runtime_accepted_archive_due','runtime_agent_unsettled_legacy') ORDER BY name")
            .expect("catalog").query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("indexes").collect::<Result<_, _>>().expect("rows")
    };
    let fresh_indexes = catalog(&db);
    db.execute_batch("DROP INDEX runtime_accepted_archive_due; DROP INDEX runtime_agent_unsettled_legacy; INSERT INTO runtime_operation_receipts VALUES('old','accepted-archive-v1','invalid');").expect("old state");
    drop(db);
    assert!(
        RuntimeStore::open(Some(root.path())).await.is_err(),
        "invalid indexed receipt blocks migration"
    );
    let db = Connection::open(root.path().join("canvas.sqlite3")).expect("canvas");
    let count: i64 = db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('runtime_accepted_archive_due','runtime_agent_unsettled_legacy')", [], |row| row.get(0)).expect("catalog");
    assert_eq!(count, 0, "failed migration rolls back indexes");
    let receipt: String = db
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id='old'",
            [],
            |row| row.get(0),
        )
        .expect("receipt");
    assert_eq!(receipt, "invalid");
    db.execute(
        "UPDATE runtime_operation_receipts SET result=?1 WHERE id='old'",
        [r#"{"status":"pending","nextAttemptAt":42}"#],
    )
    .expect("repair fixture");
    drop(db);
    for _ in 0..2 {
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("upgrade and reopen");
        let db = store.test_reader().expect("reader");
        let due: f64 = db.query_row("SELECT json_extract(result,'$.nextAttemptAt') FROM runtime_operation_receipts WHERE id='old'", [], |row| row.get(0)).expect("retained receipt");
        assert_eq!(due, 42.0);
        let count: i64 = db.query_row("SELECT count(*) FROM sqlite_master WHERE name IN ('runtime_accepted_archive_due','runtime_agent_unsettled_legacy')", [], |row| row.get(0)).expect("catalog");
        assert_eq!(count, 2);
        assert_eq!(catalog(&db), fresh_indexes, "fresh and migrated DDL agree");
    }
}

#[tokio::test]
async fn open_removes_retired_entity_scope_fingerprint_indexes() {
    let directory = temporary_state("entity-scope-index");
    drop(
        RuntimeStore::open(Some(directory.path()))
            .await
            .expect("fresh state"),
    );
    let connection = Connection::open(directory.path().join("canvas.sqlite3")).expect("canvas");
    connection
        .execute_batch(
            "CREATE INDEX IF NOT EXISTS sync_entities_live_agent_scope ON sync_entities(collection,id); CREATE INDEX IF NOT EXISTS sync_entities_live_root_scope ON sync_entities(collection,id); CREATE INDEX IF NOT EXISTS sync_entities_invalid_payload ON sync_entities(collection,id);",
        )
        .expect("old catalog");
    drop(connection);
    drop(
        RuntimeStore::open(Some(directory.path()))
            .await
            .expect("upgrade existing state"),
    );
    let connection = Connection::open(directory.path().join("canvas.sqlite3")).expect("canvas");
    let count: i64 = connection.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name IN ('sync_entities_live_agent_scope','sync_entities_live_root_scope','sync_entities_invalid_payload')", [], |row| row.get(0)).expect("catalog");
    assert_eq!(
        count, 0,
        "retired fingerprint indexes must not survive reopen"
    );
}
