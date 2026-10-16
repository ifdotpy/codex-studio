use serde_json::Value;
use std::{path::Path, sync::OnceLock};
use studio_runtime_store::{AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table};

pub fn corpus(root: &Path) -> Vec<Value> {
    static TEMPLATE: OnceLock<(tempfile::TempDir, Vec<Value>)> = OnceLock::new();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let (template_root, cases) = TEMPLATE.get_or_init(|| {
        let template_root = tempfile::tempdir().expect("monitor fixture template");
        let source = manifest.join("../../apps/server/src");
        let script = manifest.join("tests/monitors/oracle.py");
        let mut command =
            std::process::Command::new(crate::prepared_python_support::prepared_python(&source));
        command
            .arg("-B")
            .arg(&script)
            .arg(&source)
            .arg(template_root.path());
        let output = crate::prepared_python_support::output_with_timeout(
            &mut command,
            &script,
            crate::prepared_python_support::PYTHON_ORACLE_TIMEOUT,
        )
        .unwrap_or_else(|error| panic!("{}: {error}", script.display()));
        assert!(
            output.status.success(),
            "{}: {}",
            script.display(),
            String::from_utf8_lossy(&output.stderr)
        );
        let cases = serde_json::from_slice(&output.stdout).expect("monitor fixture");
        (template_root, cases)
    });
    let template = template_root.path().join("template");
    let destination = root.join("template");
    std::fs::create_dir(&destination).expect("monitor fixture");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.join(database), destination.join(database))
            .expect("monitor fixture");
    }
    let mut cases = cases.clone();
    for case in &mut cases {
        rebase_fixture_paths(case, template_root.path(), root);
    }
    cases
}

fn rebase_fixture_paths(value: &mut Value, from: &Path, to: &Path) {
    match value {
        Value::String(path) => {
            let source = Path::new(path);
            if let Ok(relative) = source.strip_prefix(from) {
                *path = to.join(relative).to_string_lossy().into_owned();
            }
        }
        Value::Array(values) => {
            for value in values {
                rebase_fixture_paths(value, from, to);
            }
        }
        Value::Object(values) => {
            for value in values.values_mut() {
                rebase_fixture_paths(value, from, to);
            }
        }
        _ => {}
    }
}

pub fn store(root: &Path, name: &str) -> RuntimeStore {
    let target = root.join(name);
    std::fs::create_dir(&target).expect("monitor fixture");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(root.join("template").join(database), target.join(database))
            .expect("monitor fixture");
    }
    RuntimeStore::open(Some(&target)).expect("monitor fixture")
}

pub fn seed(store: &RuntimeStore, case: &Value) {
    store
        .transaction(|tx| {
            tx.execute("DELETE FROM runtime_events", [])?;
            tx.execute("DELETE FROM runtime_monitors", [])?;
            if let Some(events) = case["priorEvents"].as_array() {
                for e in events {
                    tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES(?,?,?,?,?,?,?,?,?)", rusqlite::params![e["id"].as_str(),e["agent"].as_str(),e["kind"].as_str(),serde_json::to_string(&e["text"]).expect("event"),e["status"].as_str(),e["created"].as_f64(),e["epoch"].as_i64(),e["turn_id"].as_str(),e["error"].as_str()])?;
                }
            }
            AgentRepository::put(tx, &AgentRecord::from_value(case["actor"].clone())?)?;
            Table::new(tx).put(&MonitorRecord::from_value(case["before"].clone())?)
        })
        .expect("monitor fixture");
}

pub fn events(store: &RuntimeStore) -> Value {
    let connection = store.read().expect("monitor fixture");
    let mut query = connection.prepare("SELECT id,agent,kind,text,status,created,epoch,turn_id,error FROM runtime_events ORDER BY id").expect("monitor fixture");
    let rows = query.query_map([], |r| Ok(serde_json::json!({
        "id":r.get::<_, String>(0)?, "agent":r.get::<_, String>(1)?, "kind":r.get::<_, String>(2)?,
        "text":serde_json::from_str::<Value>(&r.get::<_, String>(3)?).expect("monitor fixture"), "status":r.get::<_, String>(4)?, "created":r.get::<_, f64>(5)?, "epoch":r.get::<_, i64>(6)?, "turn_id":r.get::<_, Option<String>>(7)?, "error":r.get::<_, Option<String>>(8)?
    }))).expect("monitor fixture").collect::<Result<Vec<_>, _>>().expect("monitor fixture");
    Value::Array(rows)
}

pub fn tempdir() -> tempfile::TempDir {
    match std::env::var_os("STUDIO_MONITOR_TEST_ROOT") {
        Some(root) => tempfile::Builder::new()
            .prefix("monitor-")
            .tempdir_in(root)
            .expect("isolated monitor fixture root"),
        None => tempfile::tempdir().expect("monitor fixture"),
    }
}
