#[path = "../../runtime-services/tests/support/prepared_python.rs"]
mod prepared_python_support;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::Connection;
use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, StoreError, check_state_readonly};

#[test]
fn rust_fresh_state_matches_python_and_python_opens_it_without_schema_migration() {
    let python_state = temporary_state("python");
    let rust_state = temporary_state("rust");
    let expected = create_python_state(&python_state);

    let store = RuntimeStore::open(Some(&rust_state)).expect("Rust creates an empty state");
    assert_eq!(catalog_snapshot(&rust_state), expected["databases"]);
    assert_eq!(sync_seeds(&rust_state), expected["sync_seeds"]);
    assert_eq!(sync_meta(&rust_state), expected["sync_meta"]);
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

    let opened = open_python_state(&rust_state);
    assert_eq!(
        opened["before"], opened["after"],
        "Python startup performs no schema migration"
    );
    assert_eq!(opened["before"], expected["databases"]);
    assert_eq!(opened["sync_seeds"], expected["sync_seeds"]);

    cleanup(python_state);
    cleanup(rust_state);
}

#[test]
fn partial_database_set_is_refused_without_changing_existing_files() {
    let state = temporary_state("partial");
    let canvas = state.join("canvas.sqlite3");
    fs::write(&canvas, b"partial database sentinel").expect("sentinel writes");
    let before = fs::read(&canvas).expect("sentinel reads");

    let error = match RuntimeStore::open(Some(&state)) {
        Ok(_) => panic!("partial state must be refused"),
        Err(error) => error,
    };
    assert!(
        matches!(error, StoreError::SchemaMismatch(message) if message.contains("partial state directory"))
    );
    assert_eq!(
        fs::read(&canvas).expect("sentinel remains readable"),
        before
    );
    assert!(!state.join("analytics.sqlite3").exists());
    cleanup(state);
}

fn create_python_state(root: &Path) -> Value {
    python(root, PYTHON_CREATE)
}

fn open_python_state(root: &Path) -> Value {
    python(root, PYTHON_OPEN)
}

fn python(root: &Path, script: &str) -> Value {
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
    let output = Command::new(prepared_python_support::prepared_python(&source))
        .arg("-c")
        .arg(script)
        .arg(root)
        .arg(&source)
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
                    "sql": row.get::<_, Option<String>>(3)?,
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

fn temporary_state(name: &str) -> PathBuf {
    static NEXT_ID: AtomicU64 = AtomicU64::new(0);
    let root = std::env::var_os("TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    let path = root.join(format!(
        "studio-fresh-state-{name}-{}-{}",
        std::process::id(),
        NEXT_ID.fetch_add(1, Ordering::Relaxed)
    ));
    fs::create_dir_all(&path).expect("temporary state directory creates");
    path
}

fn cleanup(path: PathBuf) {
    fs::remove_dir_all(path).expect("temporary state directory removes");
}

const PYTHON_CREATE: &str = r#"
import json, pathlib, sqlite3, sys
sys.path.insert(0, sys.argv[2])
from codex_canvas import Canvas
from codex_runtime import Runtime
from runtime_test_environment import make_runtime
from codex_process_supervisor import Journal

class QuietRuntime(Runtime):
    def schedule(self): pass

root = pathlib.Path(sys.argv[1])
canvas = Canvas(root=root)
runtime = make_runtime(QuietRuntime, root, lambda *_args, **_kwargs: None)
journal = Journal(root)
journal.close()
def db_snapshot(connection, schema):
    objects = [dict(type=kind, name=name, table=table, sql=sql) for kind,name,table,sql in connection.execute(
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
runtime.close()
print(json.dumps({"databases": databases, "sync_seeds": seeds, "sync_meta": meta}, sort_keys=True))
"#;

const PYTHON_OPEN: &str = r#"
import json, pathlib, sqlite3, sys
sys.path.insert(0, sys.argv[2])
from codex_canvas import Canvas
from codex_runtime import Runtime
from runtime_test_environment import make_runtime

class QuietRuntime(Runtime):
    def schedule(self): pass

root = pathlib.Path(sys.argv[1])
def snapshot():
    with sqlite3.connect(root / "canvas.sqlite3") as connection:
        main = [dict(type=k,name=n,table=t,sql=s) for k,n,t,s in connection.execute(
            "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")]
        version = connection.execute("PRAGMA user_version").fetchone()[0]
        journal = connection.execute("PRAGMA journal_mode").fetchone()[0]
    with sqlite3.connect(root / "analytics.sqlite3") as connection:
        analytics = [dict(type=k,name=n,table=t,sql=s) for k,n,t,s in connection.execute(
            "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")]
        analytics_version = connection.execute("PRAGMA user_version").fetchone()[0]
        analytics_journal = connection.execute("PRAGMA journal_mode").fetchone()[0]
    with sqlite3.connect(root / "supervisor.sqlite3") as connection:
        supervisor = [dict(type=k,name=n,table=t,sql=s) for k,n,t,s in connection.execute(
            "SELECT type,name,tbl_name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")]
        supervisor_version = connection.execute("PRAGMA user_version").fetchone()[0]
        supervisor_journal = connection.execute("PRAGMA journal_mode").fetchone()[0]
    return {"canvas.sqlite3":{"sqlite_master":main,"pragmas":{"journal_mode":journal,"user_version":version}},
            "analytics.sqlite3":{"sqlite_master":analytics,"pragmas":{"journal_mode":analytics_journal,"user_version":analytics_version}},
            "supervisor.sqlite3":{"sqlite_master":supervisor,"pragmas":{"journal_mode":supervisor_journal,"user_version":supervisor_version}}}

before = snapshot()
Canvas(root=root)
runtime = make_runtime(QuietRuntime, root, lambda *_args, **_kwargs: None)
with runtime.db() as connection:
    seeds = [dict(collection=c,id=i,payload=p,hash=h,deleted=bool(d)) for c,i,p,h,d in connection.execute(
        "SELECT collection,id,payload,hash,deleted FROM sync_entities ORDER BY collection,id")]
runtime.close()
print(json.dumps({"before":before,"after":snapshot(),"sync_seeds":seeds},sort_keys=True))
"#;
