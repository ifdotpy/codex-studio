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
    let rust_state = temporary_state("rust");
    let feature_schema = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/feature_schema.rs");
    let expected = create_python_state(&rust_state, &feature_schema);
    fs::remove_dir_all(&rust_state).expect("Python oracle state removes");
    fs::create_dir_all(&rust_state).expect("Rust state directory recreates");

    let store = RuntimeStore::open(Some(&rust_state)).expect("Rust creates an empty state");
    let rust_catalog = catalog_snapshot(&rust_state);
    assert_valid_markers(&expected["markers"]);
    assert_eq!(
        rust_catalog,
        expected["databases"],
        "{}",
        catalog_diff(&rust_catalog, &expected["databases"])
    );
    assert_eq!(sync_seeds(&rust_state), expected["sync_seeds"]);
    assert_eq!(sync_meta(&rust_state), expected["sync_meta"]);
    let rust_markers = marker_snapshot(&rust_state);
    assert_valid_markers(&rust_markers);
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
    assert_eq!(opened["before"], expected["databases"]);
    assert_eq!(opened["sync_seeds"], expected["sync_seeds"]);
    assert_eq!(opened["markers_before"], rust_markers);
    assert_eq!(
        opened["markers_after"], rust_markers,
        "Python startup leaves Rust's initialized migration markers untouched"
    );

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
                        if match and match.group(1) in targets:
                            statements.setdefault(match.group(1), sql.strip())
    for name, statement in sorted(statements.items(), key=lambda item: "INDEX" in item[1].upper()):
        if "IF NOT EXISTS" in statement.upper() or name not in created:
            connection.execute(statement)
    missing = targets - created - set(statements)
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
                        if match and match.group(1) in targets:
                            statements.setdefault(match.group(1), sql.strip())
    for name, statement in sorted(statements.items(), key=lambda item: "INDEX" in item[1].upper()):
        if "IF NOT EXISTS" in statement.upper() or name not in created:
            connection.execute(statement)
    missing = targets - created - set(statements)
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
