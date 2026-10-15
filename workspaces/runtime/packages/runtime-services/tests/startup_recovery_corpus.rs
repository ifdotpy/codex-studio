#[path = "support/prepared_python.rs"]
mod prepared_python;

use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::process::Command;
use studio_runtime_services::{monitor_runtime, scheduler};
use studio_runtime_store::RuntimeStore;

const NOW: f64 = 2_000_000_000.0;

fn state_root() -> tempfile::TempDir {
    let root = std::env::temp_dir().join("cs-startup-recovery-corpus");
    std::fs::create_dir_all(&root).expect("tmpfs startup corpus root");
    tempfile::Builder::new()
        .prefix("startup-recovery-")
        .tempdir_in(root)
        .expect("tmpfs Python-written state")
}

fn oracle(root: &Path, command: &str) -> Value {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = manifest.join("../../apps/server/src");
    let python = prepared_python::prepared_python(&source);
    let isolation = tempfile::Builder::new()
        .prefix("startup-recovery-oracle-")
        .tempdir()
        .expect("isolated Python environment");
    let cache = isolation.path().join("cache");
    for directory in ["home", "codex", "claude", "cache"] {
        std::fs::create_dir_all(isolation.path().join(directory))
            .expect("create isolated Python directory");
    }
    let output = Command::new(python)
        .arg(manifest.join("tests/startup_recovery/oracle.py"))
        .arg(command)
        .arg(root)
        .env_clear()
        .env("HOME", isolation.path().join("home"))
        .env("CODEX_HOME", isolation.path().join("codex"))
        .env("CLAUDE_CONFIG_DIR", isolation.path().join("claude"))
        .env("XDG_CACHE_HOME", cache)
        .env("STUDIO_TEST_ISOLATION_ROOT", isolation.path())
        .env("PATH", "/usr/local/bin:/usr/bin:/bin")
        .env("TMPDIR", std::env::temp_dir())
        .env(
            "CODEX_NODE",
            std::env::var_os("CODEX_NODE").unwrap_or_else(|| "node".into()),
        )
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .output()
        .expect("start isolated Runtime oracle");
    assert!(
        output.status.success(),
        "Runtime oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    if command == "recover" {
        serde_json::from_slice(&output.stdout).expect("Runtime recovery snapshot")
    } else {
        json!({})
    }
}

fn copy_state(source: &Path, target: &Path) {
    std::fs::create_dir_all(target).expect("create cloned state");
    for entry in std::fs::read_dir(source).expect("read source state") {
        let entry = entry.expect("state entry");
        let path = entry.path();
        let destination = target.join(entry.file_name());
        if path.is_dir() {
            copy_state(&path, &destination);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o700))
                    .expect("restrict copied state directory");
            }
        } else {
            std::fs::copy(&path, &destination).expect("copy committed Python state");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&destination, std::fs::Permissions::from_mode(0o600))
                    .expect("restrict copied state file");
            }
        }
    }
}

fn rust_snapshot(store: &RuntimeStore) -> Value {
    let db = store.read().expect("read Rust startup rows");
    let records = |table: &str| -> Vec<Value> {
        let mut statement = db
            .prepare(&format!("SELECT record FROM {table} ORDER BY id"))
            .expect("prepare startup snapshot");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query startup records")
            .map(|row| {
                serde_json::from_str(&row.expect("startup row")).expect("startup record JSON")
            })
            .collect()
    };
    let events = {
        let mut statement = db
            .prepare(
                "SELECT id,agent,kind,text,status,created,epoch,turn_id,error \
             FROM runtime_events ORDER BY id",
            )
            .expect("prepare event snapshot");
        statement
            .query_map([], |row| {
                Ok(json!({
                    "id": row.get::<_, String>(0)?, "agent": row.get::<_, String>(1)?,
                    "kind": row.get::<_, String>(2)?, "text": row.get::<_, String>(3)?,
                    "status": row.get::<_, String>(4)?, "created": row.get::<_, f64>(5)?,
                    "epoch": row.get::<_, i64>(6)?, "turn_id": row.get::<_, Option<String>>(7)?,
                    "error": row.get::<_, Option<String>>(8)?,
                }))
            })
            .expect("query event snapshot")
            .map(|row| {
                let event = row.expect("event row");
                let text = event["text"].as_str().expect("event text");
                let text =
                    serde_json::from_str(text).unwrap_or_else(|_| Value::String(text.into()));
                let mut event = event;
                event["text"] = text;
                event
            })
            .collect::<Vec<_>>()
    };
    let sync_entities = {
        let mut statement = db
            .prepare(
                "SELECT collection,id,payload,deleted FROM sync_entities ORDER BY collection,id",
            )
            .expect("prepare projection snapshot");
        statement
            .query_map([], |row| {
                Ok(json!({
                    "collection": row.get::<_, String>(0)?, "id": row.get::<_, String>(1)?,
                    "payload": row.get::<_, String>(2)?, "deleted": row.get::<_, i64>(3)?,
                }))
            })
            .expect("query projections")
            .map(|row| {
                let mut projection = row.expect("projection row");
                projection["payload"] = serde_json::from_str(
                    projection["payload"].as_str().expect("projection payload"),
                )
                .expect("projection JSON");
                projection
            })
            .collect::<Vec<_>>()
    };
    json!({
        "agents": records("runtime_agents"),
        "requests": records("runtime_requests"),
        "monitors": records("runtime_monitors"),
        "events": events,
        "sync_entities": sync_entities,
    })
}

fn durable_hold_agent_ids(snapshot: &Value) -> std::collections::BTreeSet<String> {
    snapshot["agents"]
        .as_array()
        .expect("agents")
        .iter()
        .filter(|agent| {
            agent["nativeFailureHold"] == true
                || agent
                    .get("startOutcomeHold")
                    .is_some_and(|value| !value.is_null())
                || agent
                    .get("contextRepairWait")
                    .is_some_and(|value| !value.is_null())
                || agent
                    .get("restartRecovery")
                    .is_some_and(|value| !value.is_null())
                || agent
                    .get("disconnectRecovery")
                    .is_some_and(|value| !value.is_null())
        })
        .filter_map(|agent| agent["id"].as_str().map(str::to_owned))
        .collect()
}

fn redact_registered_agent_differences(
    snapshot: &mut Value,
    deviations: &Value,
    held_agent_ids: &std::collections::BTreeSet<String>,
) {
    let paths = deviations["durable_reconciliation_hold"]["fields"]
        .as_array()
        .expect("registered startup difference");
    assert!(
        deviations["durable_reconciliation_hold"]["reason"]
            .as_str()
            .is_some_and(|reason| reason.contains("durable hold"))
    );
    for agent in snapshot["agents"].as_array_mut().expect("agents") {
        if !agent["id"]
            .as_str()
            .is_some_and(|id| held_agent_ids.contains(id))
        {
            continue;
        }
        for path in paths {
            let Some(path) = path.as_str() else { continue };
            if let Some(field) = path.strip_prefix("/runtime_agents/*/record/")
                && !field.contains('/')
            {
                agent.as_object_mut().expect("agent object").remove(field);
            }
        }
    }
    for projection in snapshot["sync_entities"]
        .as_array_mut()
        .expect("sync entities")
    {
        if projection["collection"] != "agent" {
            continue;
        }
        if !projection["id"]
            .as_str()
            .is_some_and(|id| held_agent_ids.contains(id))
        {
            continue;
        }
        for path in paths {
            let Some(field) = path
                .as_str()
                .and_then(|path| path.strip_prefix("/sync_entities/*/payload/value/"))
            else {
                continue;
            };
            if !field.contains('/') {
                projection["payload"]["value"]
                    .as_object_mut()
                    .expect("agent projection value")
                    .remove(field);
            }
        }
    }
}

fn collect_differences(path: &str, left: &Value, right: &Value, out: &mut Vec<String>) {
    if out.len() >= 40 || left == right {
        return;
    }
    match (left, right) {
        (Value::Object(left), Value::Object(right)) => {
            let keys = left
                .keys()
                .chain(right.keys())
                .collect::<std::collections::BTreeSet<_>>();
            for key in keys {
                collect_differences(
                    &format!("{path}/{key}"),
                    left.get(key).unwrap_or(&Value::Null),
                    right.get(key).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        (Value::Array(left), Value::Array(right)) => {
            for index in 0..left.len().max(right.len()) {
                collect_differences(
                    &format!("{path}/{index}"),
                    left.get(index).unwrap_or(&Value::Null),
                    right.get(index).unwrap_or(&Value::Null),
                    out,
                );
            }
        }
        _ => out.push(format!("{path}: {left} != {right}")),
    }
}

#[test]
fn python_constructor_and_rust_startup_match_requests_monitors_events_and_projections() {
    let root = state_root();
    let python_state = root.path().join("python");
    let rust_state = root.path().join("rust");
    std::fs::create_dir_all(&python_state).expect("seed state directory");
    oracle(&python_state, "seed");
    copy_state(&python_state, &rust_state);
    let python = oracle(&python_state, "recover");

    let store = RuntimeStore::open(Some(&rust_state)).expect("open Python-written state");
    monitor_runtime::recover_on_startup(&store, NOW).expect("Rust startup recovery");
    scheduler::recover_on_startup(&store).expect("existing account-worker startup recovery");
    let mut rust = rust_snapshot(&store);
    let mut python = python;
    let deviations_path =
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/turn_lifecycle/deviations.json");
    let deviations: Value = serde_json::from_slice(
        &std::fs::read(deviations_path).expect("read registered differences"),
    )
    .expect("deviation registry JSON");
    let mut held_agent_ids = durable_hold_agent_ids(&rust);
    held_agent_ids.extend(durable_hold_agent_ids(&python));
    assert!(
        !held_agent_ids.is_empty(),
        "fixture includes a registered durable hold"
    );
    redact_registered_agent_differences(&mut rust, &deviations, &held_agent_ids);
    redact_registered_agent_differences(&mut python, &deviations, &held_agent_ids);
    let mut differences = Vec::new();
    collect_differences("", &rust, &python, &mut differences);
    assert!(
        differences.is_empty(),
        "complete startup rows and projections differ:\n{}",
        differences.join("\n")
    );
}
