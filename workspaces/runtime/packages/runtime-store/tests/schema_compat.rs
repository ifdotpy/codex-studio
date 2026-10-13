use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, ChatMessageId, MessageRepository, QueueEvent, QueueEventId,
    QueueEventRepository, RuntimeStore, StoreError, TaskId, TaskRepository,
};

const LIVE_SCHEMA: &str = include_str!("fixtures/live-state-schema.json");

#[test]
fn live_schema_opens_and_supported_repositories_preserve_the_catalog() {
    let state = temporary_state();
    create_from_live_schema(&state);
    seed_records(&state.join("canvas.sqlite3"));
    let analytics_before = catalog(&state.join("analytics.sqlite3"));
    let derived_before_open = derived_catalog(&state.join("canvas.sqlite3"));

    let store = RuntimeStore::open(Some(&state)).expect("schema-only live fixture opens");
    let replaced = replaced_derived_objects(
        &derived_before_open,
        &derived_catalog(&state.join("canvas.sqlite3")),
    );
    assert_eq!(
        replaced,
        Vec::<String>::new(),
        "live fixture first-open replacements"
    );
    let before = catalog(&state.join("canvas.sqlite3"));
    let reader = store.read().expect("reader opens");
    assert!(
        AgentRepository::get(&reader, &AgentId::from("fixture-agent"))
            .expect("agent reads")
            .is_some()
    );
    assert!(
        TaskRepository::get(&reader, &TaskId::from("fixture-task"))
            .expect("task reads")
            .is_some()
    );
    assert!(
        QueueEventRepository::get(&reader, &QueueEventId::from("fixture-event"))
            .expect("event reads")
            .is_none()
    );
    assert!(
        MessageRepository::get(&reader, &ChatMessageId::from("fixture-message"))
            .expect("message reads")
            .is_some()
    );
    drop(reader);

    store
        .transaction(|tx| {
            let mut agent = AgentRepository::get(tx, &AgentId::from("fixture-agent"))?
                .expect("seeded agent exists");
            agent.extra.insert("rustWrite".into(), json!(true));
            AgentRepository::put(tx, &agent)?;

            let mut task = TaskRepository::get(tx, &TaskId::from("fixture-task"))?
                .expect("seeded task exists");
            task.set("status", json!("running"));
            TaskRepository::put(tx, &task)?;

            QueueEventRepository::put(
                tx,
                &QueueEvent {
                    id: QueueEventId::from("fixture-event"),
                    agent: AgentId::from("fixture-agent"),
                    kind: "work_review".into(),
                    text: "{}".into(),
                    status: "running".into(),
                    created: 5.0,
                    epoch: 0,
                    turn_id: None,
                    error: None,
                },
            )?;

            let mut message = MessageRepository::get(tx, &ChatMessageId::from("fixture-message"))?
                .expect("seeded message exists");
            message.deliveries["rustWrite"] = json!(true);
            MessageRepository::put(tx, &message)?;
            Ok(())
        })
        .expect("all supported repositories write on the live schema");
    let reader = store.read().expect("reader opens after writes");
    assert_eq!(
        QueueEventRepository::get(&reader, &QueueEventId::from("fixture-event"))
            .expect("written event reads")
            .expect("event exists")
            .status,
        "running"
    );
    drop(reader);
    store
        .verify_schema()
        .expect("required schema remains valid");
    drop(store);

    assert_eq!(before, catalog(&state.join("canvas.sqlite3")));
    assert_eq!(analytics_before, catalog(&state.join("analytics.sqlite3")));
    let reopened = RuntimeStore::open(Some(&state)).expect("Python-shaped schema remains openable");
    let reader = reopened.read().expect("reader opens after Rust writes");
    assert_eq!(
        AgentRepository::get(&reader, &AgentId::from("fixture-agent"))
            .expect("agent reads")
            .expect("agent exists")
            .value()["rustWrite"],
        true
    );
    drop(reader);
    drop(reopened);
    cleanup(state);
}

#[test]
fn missing_required_objects_are_typed_and_name_the_object() {
    for (sql, expected) in [
        ("DROP TABLE runtime_tasks", "runtime_tasks"),
        (
            "DROP TABLE runtime_tasks; CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY)",
            "runtime_tasks",
        ),
    ] {
        let state = temporary_state();
        create_from_live_schema(&state);
        Connection::open(state.join("canvas.sqlite3"))
            .expect("database opens")
            .execute_batch(sql)
            .expect("negative case mutates disposable schema");
        let before = catalog(&state.join("canvas.sqlite3"));
        let error = match RuntimeStore::open(Some(&state)) {
            Ok(_) => panic!("incompatible required schema was accepted"),
            Err(error) => error,
        };
        assert!(
            matches!(error, StoreError::SchemaObjectMismatch { ref object, .. } if object == expected),
            "expected typed refusal naming {expected}, got {error:?}"
        );
        assert_eq!(before, catalog(&state.join("canvas.sqlite3")));
        cleanup(state);
    }
}

#[test]
fn renamed_sync_table_with_equivalent_structure_opens() {
    let state = temporary_state();
    create_from_live_schema(&state);
    let canvas = state.join("canvas.sqlite3");
    let initial_store = RuntimeStore::open(Some(&state)).expect("fixture prepares on open");
    drop(initial_store);
    let identity = Connection::open(&canvas)
        .expect("fixture DB opens")
        .query_row("SELECT id FROM sync_identity", [], |row| {
            row.get::<_, String>(0)
        })
        .expect("fixture has sync identity");
    Connection::open(&canvas)
        .expect("fixture DB opens")
        .execute_batch(
            "ALTER TABLE sync_identity RENAME TO saved_sync_identity;
             ALTER TABLE saved_sync_identity RENAME TO sync_identity;",
        )
        .expect("rename round trip preserves the table structure");

    let store = RuntimeStore::open(Some(&state)).expect("structurally equivalent table opens");
    assert_eq!(
        store
            .read()
            .expect("reader opens")
            .query_row("SELECT id FROM sync_identity", [], |row| row
                .get::<_, String>(0))
            .expect("identity remains readable"),
        identity
    );
    drop(store);
    cleanup(state);
}

#[test]
fn missing_automatic_unique_index_is_named_and_left_untouched() {
    let state = temporary_state();
    create_from_live_schema(&state);
    let canvas = state.join("canvas.sqlite3");
    Connection::open(&canvas)
        .expect("fixture DB opens")
        .execute_batch(
            "DROP TABLE sync_documents;
             CREATE TABLE sync_documents (
                 seq INTEGER PRIMARY KEY AUTOINCREMENT,
                 scope TEXT NOT NULL,
                 id TEXT NOT NULL,
                 payload TEXT NOT NULL,
                 deleted INTEGER NOT NULL DEFAULT 0
             );",
        )
        .expect("table has matching columns but lacks its unique constraint");
    let before = catalog(&canvas);

    let error = match RuntimeStore::open(Some(&state)) {
        Ok(_) => panic!("sync_documents without UNIQUE(scope,id) was accepted"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StoreError::SchemaObjectMismatch { ref object, ref reason, .. }
            if object == "sync_documents" && reason.contains("UNIQUE/PRIMARY KEY")),
        "constraint error should identify sync_documents, got {error:?}"
    );
    assert_eq!(catalog(&canvas), before);
    cleanup(state);
}

#[test]
fn unicode_altered_table_column_is_named_and_left_untouched() {
    let state = temporary_state();
    create_from_live_schema(&state);
    let canvas = state.join("canvas.sqlite3");
    let altered_table = format!(
        "DROP TABLE sync_identity; CREATE TABLE sync_identity(id TEXT{}PRIMARY KEY);",
        '\u{00a0}'
    );
    Connection::open(&canvas)
        .expect("fixture DB opens")
        .execute_batch(&altered_table)
        .expect("SQLite accepts the altered type declaration");
    let before = catalog(&canvas);

    let error = match RuntimeStore::open(Some(&state)) {
        Ok(_) => panic!("incompatible sync_identity column is accepted"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StoreError::SchemaObjectMismatch { ref object, ref reason, .. } if object == "sync_identity" && reason.contains("column id")),
        "table error should name sync_identity.id, got {error:?}"
    );
    assert_eq!(catalog(&canvas), before);
    cleanup(state);
}

#[test]
fn sync_schema_has_no_unchecked_table_constraints() {
    let sql = include_str!("../src/sync_schema.sql").to_ascii_uppercase();
    assert!(
        !sql.contains("CHECK") && !sql.contains("REFERENCES"),
        "extend structural comparison before adding CHECK or FOREIGN KEY constraints"
    );

    let state = temporary_state();
    create_from_live_schema(&state);
    let store = RuntimeStore::open(Some(&state)).expect("scratch-derived sync schema opens");
    let db = store.read().expect("reader opens");
    let mut statement = db
        .prepare("SELECT name,sql FROM sqlite_master WHERE type='table' AND (name GLOB 'sync_*' OR name='runtime_transcript_revisions')")
        .expect("owned table query prepares");
    let tables = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("owned table query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("owned tables read");
    drop(statement);
    for (table, _table_sql) in tables {
        let foreign_keys: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM pragma_foreign_key_list(?1)",
                [&table],
                |row| row.get(0),
            )
            .expect("foreign key count reads");
        assert_eq!(foreign_keys, 0, "extend FOREIGN KEY comparison for {table}");
        let generated_columns: i64 = db
            .query_row(
                "SELECT COUNT(*) FROM pragma_table_xinfo(?1) WHERE hidden!=0",
                [&table],
                |row| row.get(0),
            )
            .expect("generated column count reads");
        assert_eq!(
            generated_columns, 0,
            "unexpected generated column in {table}"
        );
    }
    drop(db);
    drop(store);
    cleanup(state);
}

#[test]
fn noncanonical_tombstone_triggers_are_replaced() {
    let state = temporary_state();
    create_from_live_schema(&state);
    let trigger = "sync_entity_tombstone_count_update";
    let canonical_store = RuntimeStore::open(Some(&state)).expect("canonical schema opens");
    drop(canonical_store);
    let canvas = state.join("canvas.sqlite3");
    let canonical = stored_sql(&canvas, trigger);

    for whitespace_variant in [true, false] {
        let id = if whitespace_variant {
            "whitespace-tombstone"
        } else {
            "unicode-tombstone"
        };
        let database = Connection::open(&canvas).expect("fixture DB opens");
        let original = stored_sql(&canvas, trigger);
        let changed = if whitespace_variant {
            original.replace(
                "UPDATE OF deleted,collection",
                "UPDATE OF deleted,   collection",
            )
        } else {
            original.replace(
                "UPDATE OF deleted,collection",
                &format!("UPDATE OF deleted,{}collection", '\u{00a0}'),
            )
        };
        assert_ne!(changed, original, "fixture trigger contains target text");
        database
            .execute_batch(&format!("DROP TRIGGER {trigger}"))
            .expect("original trigger drops");
        database
            .execute_batch(&changed)
            .expect("SQLite accepts the noncanonical trigger");
        drop(database);

        let store = RuntimeStore::open(Some(&state)).expect("noncanonical trigger is repaired");
        assert_eq!(stored_sql(&canvas, trigger), canonical);
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('transcript:test',?1,1,'h','{}',1)",
                    [id],
                )?;
                tx.execute(
                    "UPDATE sync_entities SET collection='event' WHERE collection='transcript:test' AND id=?1",
                    [id],
                )?;
                Ok(())
            })
            .expect("repaired trigger handles collection-only update");
        assert_eq!(
            store
                .read()
                .expect("reader opens")
                .query_row(
                    "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_count'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .expect("tombstone count reads"),
            "1"
        );
        store
            .transaction(|tx| {
                tx.execute("DELETE FROM sync_entities WHERE id=?1", [id])?;
                Ok(())
            })
            .expect("variant tombstone is removed before the next case");
    }
    cleanup(state);
}

fn drop_sync_family(path: &Path) {
    let db = Connection::open(path).expect("fixture DB opens");
    let mut statement = db
        .prepare(
            "SELECT type,name FROM sqlite_master WHERE name LIKE 'sync_%' OR name IN ('runtime_event_created_id','runtime_transcript_revisions') ORDER BY CASE type WHEN 'trigger' THEN 0 WHEN 'index' THEN 1 WHEN 'table' THEN 2 ELSE 3 END",
        )
        .expect("sync objects query prepares");
    let objects = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .expect("sync objects query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("sync objects read");
    drop(statement);
    for (kind, name) in objects {
        let quote = name.replace('"', "\"\"");
        db.execute_batch(&format!("DROP {kind} \"{quote}\""))
            .expect("sync family object drops in dependency order");
    }
}

fn prepare_with_python(state: &Path) {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let script = r#"
import contextlib, pathlib, sqlite3, sys
sys.path.insert(0, sys.argv[2])
from codex_sync import SyncStore
from codex_sync_entities import ensure_tables, install_bypass_triggers, register_functions
path = pathlib.Path(sys.argv[1]) / 'canvas.sqlite3'
@contextlib.contextmanager
def connect():
    db = sqlite3.connect(path)
    try:
        yield db
    finally:
        db.close()
with connect() as db:
    register_functions(db)
    ensure_tables(db)
    install_bypass_triggers(db)
store = SyncStore(connect, lambda _agent: None)
store._ensure_versions()
"#;
    let output = Command::new(python_executable())
        .arg("-c")
        .arg(script)
        .arg(state)
        .arg(&source)
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("isolated Python starts for the schema producer test");
    assert!(
        output.status.success(),
        "Python SyncStore preparation failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn python_executable() -> PathBuf {
    if let Some(python) = std::env::var_os("PYTHON") {
        return PathBuf::from(python);
    }
    let resolver =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src/codex_python.py");
    let output = Command::new("python3")
        .arg(resolver)
        .output()
        .expect("isolated Python resolver starts");
    assert!(
        output.status.success(),
        "prepare the isolated Python environment before this test: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    PathBuf::from(
        String::from_utf8(output.stdout)
            .expect("Python path is UTF-8")
            .trim(),
    )
}

fn derived_catalog(path: &Path) -> Vec<(String, String, String)> {
    let db = Connection::open(path).expect("fixture DB opens");
    let mut statement = db
        .prepare(
            "SELECT type,name,sql FROM sqlite_master WHERE type IN ('index','trigger') AND (name GLOB 'sync_*' OR name IN ('runtime_event_created_id','runtime_transcript_revisions')) ORDER BY type,name",
        )
        .expect("derived catalog query prepares");
    statement
        .query_map([], |row| {
            let sql: Option<String> = row.get(2)?;
            Ok((row.get(0)?, row.get(1)?, sql.unwrap_or_default()))
        })
        .expect("derived catalog query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("derived catalog reads")
}

fn replaced_derived_objects(
    before: &[(String, String, String)],
    after: &[(String, String, String)],
) -> Vec<String> {
    before
        .iter()
        .filter_map(|(kind, name, sql)| {
            after
                .iter()
                .find(|(after_kind, after_name, _)| {
                    kind == after_kind && name.eq_ignore_ascii_case(after_name)
                })
                .filter(|(_, _, after_sql)| sql != after_sql)
                .map(|(kind, name, _)| format!("{kind}:{name}"))
        })
        .collect()
}

fn sync_seed_rows(path: &Path) -> Vec<String> {
    let db = Connection::open(path).expect("fixture DB opens");
    let identity: (i64, Option<i64>) = db
        .query_row(
            "SELECT COUNT(*),MIN(length(id)) FROM sync_identity",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("identity seed reads");
    let mut rows = vec![format!(
        "identity:{}:{}",
        identity.0,
        identity.1.unwrap_or_default()
    )];
    let mut statement = db
        .prepare("SELECT id,value FROM sync_generation ORDER BY id")
        .expect("generation query prepares");
    rows.extend(
        statement
            .query_map([], |row| {
                Ok(format!(
                    "generation:{}:{}",
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?
                ))
            })
            .expect("generation query executes")
            .collect::<Result<Vec<_>, _>>()
            .expect("generation seeds read"),
    );
    let mut statement = db
        .prepare("SELECT key,value FROM sync_entity_meta ORDER BY key")
        .expect("entity meta query prepares");
    rows.extend(
        statement
            .query_map([], |row| {
                Ok(format!(
                    "meta:{}:{}",
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?
                ))
            })
            .expect("entity meta query executes")
            .collect::<Result<Vec<_>, _>>()
            .expect("entity meta seed reads"),
    );
    rows
}

#[test]
fn python_prepared_sync_schema_matches_rust_bootstrap_and_is_idempotent() {
    let python_state = temporary_state();
    let rust_state = temporary_state();
    for state in [&python_state, &rust_state] {
        create_from_live_schema(state);
        drop_sync_family(&state.join("canvas.sqlite3"));
    }
    prepare_with_python(&python_state);
    let python_canvas = python_state.join("canvas.sqlite3");
    let python_before_open = derived_catalog(&python_canvas);
    let python_seeds = sync_seed_rows(&python_state.join("canvas.sqlite3"));

    let python_store = RuntimeStore::open(Some(&python_state))
        .expect("Rust structurally accepts the actual Python-prepared tables");
    drop(python_store);
    let python_after_open = derived_catalog(&python_canvas);
    let python_replaced = replaced_derived_objects(&python_before_open, &python_after_open);

    let store = RuntimeStore::open(Some(&rust_state))
        .expect("Rust opens an empty sync family and bootstraps it");
    assert_eq!(
        derived_catalog(&rust_state.join("canvas.sqlite3")),
        python_after_open,
        "indexes and triggers match the canonical stored SQL byte for byte"
    );
    assert_eq!(
        python_replaced,
        Vec::<String>::new(),
        "Python first-open replacements"
    );
    assert_eq!(
        sync_seed_rows(&rust_state.join("canvas.sqlite3")),
        python_seeds
    );

    store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO sync_documents(scope,id,payload,deleted) VALUES('kept','doc','{}',0)",
                [],
            )?;
            tx.execute(
                "INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('kept','entity',1,'h','{}',0)",
                [],
            )?;
            Ok(())
        })
        .expect("sentinel sync rows insert");
    let before_second_raw_catalog = catalog(&rust_state.join("canvas.sqlite3"));
    let before_second_seeds = sync_seed_rows(&rust_state.join("canvas.sqlite3"));
    let before_second_schema_version = schema_version(&rust_state.join("canvas.sqlite3"));
    drop(store);
    let store = RuntimeStore::open(Some(&rust_state)).expect("second store open succeeds");
    assert_eq!(
        sync_seed_rows(&rust_state.join("canvas.sqlite3")),
        before_second_seeds
    );
    assert_eq!(
        catalog(&rust_state.join("canvas.sqlite3")),
        before_second_raw_catalog,
        "second open leaves every stored sqlite_master definition byte-identical"
    );
    assert_eq!(
        schema_version(&rust_state.join("canvas.sqlite3")),
        before_second_schema_version,
        "second open does not bump the schema cookie"
    );
    drop(store);
    cleanup(python_state);
    cleanup(rust_state);
}

#[test]
fn sync_bootstrap_repairs_missing_objects_in_the_committed_live_fixture() {
    let state = temporary_state();
    create_from_live_schema(&state);
    let canvas = state.join("canvas.sqlite3");
    let db = Connection::open(&canvas).expect("fixture DB opens");
    db.execute_batch(
        "DROP INDEX sync_entities_tombstone_order;
         DROP TABLE sync_versions;
         DROP TRIGGER sync_entity_event_UPDATE;
         CREATE TRIGGER sync_watch_legacy AFTER INSERT ON runtime_events BEGIN SELECT 1; END;
         CREATE TRIGGER syncXwatchYprotect_rows BEFORE DELETE ON runtime_tasks BEGIN SELECT RAISE(ABORT,'retain task'); END;
         INSERT INTO runtime_tasks(id,record) VALUES('protected-task','{}');
         DELETE FROM sync_entity_meta WHERE key='entity_tombstone_count';
         INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('event','seeded-tombstone',11,'h','{}',1);
         INSERT INTO sync_documents(scope,id,payload,deleted) VALUES('kept','existing','{\"x\":1}',0);
         INSERT INTO sync_entities(collection,id,seq,hash,payload,deleted) VALUES('kept','existing',9,'kept-hash','{\"x\":1}',0);",
    )
    .expect("fixture loses only lazily created objects");
    drop(db);

    let store = RuntimeStore::open(Some(&state)).expect("partially prepared fixture opens");
    let legacy_watch: i64 = store
        .read()
        .expect("reader opens")
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name GLOB 'sync_watch_*'",
            [],
            |row| row.get(0),
        )
        .expect("legacy watch trigger count reads");
    assert_eq!(legacy_watch, 0);
    let protected_trigger: i64 = store
        .read()
        .expect("reader opens")
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE type='trigger' AND name='syncXwatchYprotect_rows'",
            [],
            |row| row.get(0),
        )
        .expect("unrelated trigger count reads");
    assert_eq!(protected_trigger, 1);
    let blocked_delete = store.transaction(|tx| {
        tx.execute("DELETE FROM runtime_tasks WHERE id='protected-task'", [])?;
        Ok(())
    });
    assert!(blocked_delete.is_err(), "unrelated task protection remains");
    store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES('bootstrapped-event','fixture-agent','test','{}','queued',1.0,0,NULL,NULL)",
                [],
            )?;
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('revision-agent','{}')",
                [],
            )?;
            Ok(())
        })
        .expect("registered sync functions and transcript triggers run on writes");
    let db = store.read().expect("reader opens");
    assert_eq!(
        db.query_row(
            "SELECT value FROM sync_entity_meta WHERE key='entity_tombstone_count'",
            [],
            |row| row.get::<_, String>(0),
        )
        .expect("missing tombstone count is seeded from rows"),
        "1"
    );
    assert_eq!(
        db.query_row(
            "SELECT payload FROM sync_documents WHERE id='existing'",
            [],
            |row| row.get::<_, String>(0)
        )
        .expect("existing document remains"),
        "{\"x\":1}"
    );
    assert_eq!(
        db.query_row(
            "SELECT hash FROM sync_entities WHERE id='existing'",
            [],
            |row| row.get::<_, String>(0)
        )
        .expect("existing entity remains"),
        "kept-hash"
    );
    assert!(
        db.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='index' AND name='sync_entities_tombstone_order'",
            [],
            |_| Ok(())
        )
        .is_ok()
    );
    let payload: String = db
        .query_row(
            "SELECT payload FROM sync_entities WHERE collection='event' AND id='bootstrapped-event'",
            [],
            |row| row.get(0),
        )
        .expect("runtime event trigger projects the inserted event");
    assert_eq!(
        serde_json::from_str::<Value>(&payload).expect("event projection is JSON")["id"],
        "bootstrapped-event"
    );
    assert_eq!(
        db.query_row(
            "SELECT revision FROM runtime_transcript_revisions WHERE agent='revision-agent'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .expect("transcript revision trigger records agent change"),
        1
    );
    assert!(
        db.query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name='sync_versions'",
            [],
            |_| Ok(())
        )
        .is_ok()
    );
    drop(db);
    drop(store);
    cleanup(state);
}

#[test]
fn incompatible_sync_table_is_named_and_left_untouched() {
    let state = temporary_state();
    create_from_live_schema(&state);
    let canvas = state.join("canvas.sqlite3");
    let db = Connection::open(&canvas).expect("fixture DB opens");
    db.execute_batch(
        "DROP TABLE sync_entities;
         CREATE TABLE sync_entities(collection TEXT NOT NULL, id TEXT NOT NULL, PRIMARY KEY(collection,id));",
    )
    .expect("incompatible table fixture is created");
    drop(db);
    let before = catalog(&canvas);
    let error = match RuntimeStore::open(Some(&state)) {
        Ok(_) => panic!("incompatible same-name table is accepted during open"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StoreError::SchemaObjectMismatch { ref object, .. } if object == "sync_entities"),
        "bootstrap should name sync_entities, got {error:?}"
    );
    assert_eq!(catalog(&canvas), before);
    cleanup(state);
}

#[test]
fn sync_bootstrap_failure_rolls_back_earlier_ddl_and_seed_rows() {
    let state = temporary_state();
    create_from_live_schema(&state);
    let canvas = state.join("canvas.sqlite3");
    drop_sync_family(&canvas);
    let db = Connection::open(&canvas).expect("fixture DB opens");
    db.execute_batch(
        "CREATE TABLE sync_identity (id TEXT PRIMARY KEY);
         CREATE TRIGGER sync_watch_legacy AFTER INSERT ON runtime_events BEGIN SELECT 1; END;
         CREATE TRIGGER reject_sync_identity BEFORE INSERT ON sync_identity BEGIN SELECT RAISE(ABORT,'fixture failure'); END;",
    )
    .expect("failing seed trigger is created");
    drop(db);
    let before = catalog(&canvas);
    assert!(
        RuntimeStore::open(Some(&state)).is_err(),
        "identity seed failure should abort open"
    );
    assert_eq!(catalog(&canvas), before);
    cleanup(state);
}

fn create_from_live_schema(root: &Path) {
    let schema: Value = serde_json::from_str(LIVE_SCHEMA).expect("schema-only fixture parses");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        let connection = Connection::open(root.join(database)).expect("database file opens");
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .expect("WAL and FULL are enabled");
        let mut objects = schema[database]["sqlite_master"]
            .as_array()
            .expect("fixture contains SQLite objects")
            .iter()
            .filter_map(|object| {
                let name = object["name"].as_str()?;
                let sql = object["sql"].as_str()?;
                // FTS5 creates these shadow tables as part of CREATE VIRTUAL TABLE.
                if [
                    "runtime_search_next_config",
                    "runtime_search_next_data",
                    "runtime_search_next_docsize",
                    "runtime_search_next_idx",
                ]
                .contains(&name)
                {
                    return None;
                }
                Some((
                    object["type"].as_str().unwrap_or_default().to_owned(),
                    name.to_owned(),
                    sql.to_owned(),
                ))
            })
            .collect::<Vec<_>>();
        objects.sort_by(|left, right| {
            object_order(&left.0)
                .cmp(&object_order(&right.0))
                .then(left.1.cmp(&right.1))
        });
        for (_, _, sql) in objects {
            connection.execute_batch(&sql).unwrap_or_else(|error| {
                panic!("live fixture DDL failed for {database}: {error}; SQL={sql}")
            });
        }
        connection
            .pragma_update(None, "user_version", 0)
            .expect("version matches supplied state");
    }
}

fn object_order(kind: &str) -> u8 {
    match kind {
        "table" => 0,
        "index" => 1,
        "trigger" => 2,
        "view" => 3,
        _ => 4,
    }
}

fn seed_records(path: &Path) {
    let db = Connection::open(path).expect("canvas opens");
    db.execute(
        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
        params!["fixture-agent", r#"{"id":"fixture-agent","rootId":"fixture-agent","epoch":0,"status":"idle","futureField":{"x":true}}"#],
    )
    .expect("agent fixture inserts");
    db.execute(
        "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
        params!["fixture-task", r#"{"id":"fixture-task","agent":"fixture-agent","kind":"command","status":"completed","created":1.0}"#],
    )
    .expect("runtime task fixture inserts");
    db.execute(
        "INSERT INTO runtime_work(id,record) VALUES(?1,?2)",
        params!["fixture-work", r#"{"id":"fixture-work","rootId":"fixture-agent","status":"ready","title":"t","created":1.0,"updated":2.0,"version":1,"futureField":true}"#],
    )
    .expect("work-board fixture inserts");
    db.execute(
        "INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES('fixture-message','fixture-room','fixture-agent','hello',1.0,'{}')",
        [],
    )
    .expect("message fixture inserts");
}

fn catalog(path: &Path) -> Vec<(String, String, Option<String>)> {
    let db = Connection::open(path).expect("database opens");
    let mut statement = db
        .prepare("SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")
        .expect("catalog query prepares");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("catalog query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("catalog reads")
}

fn schema_version(path: &Path) -> i64 {
    Connection::open(path)
        .expect("database opens")
        .pragma_query_value(None, "schema_version", |row| row.get(0))
        .expect("schema cookie reads")
}

fn stored_sql(path: &Path, name: &str) -> String {
    Connection::open(path)
        .expect("database opens")
        .query_row(
            "SELECT sql FROM sqlite_master WHERE type='trigger' AND name=?1",
            [name],
            |row| row.get(0),
        )
        .expect("trigger SQL is stored")
}

fn temporary_state() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after Unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "runtime-store-schema-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("temporary state directory creates");
    root
}

fn cleanup(root: PathBuf) {
    fs::remove_dir_all(root).expect("temporary state directory is removed");
}
