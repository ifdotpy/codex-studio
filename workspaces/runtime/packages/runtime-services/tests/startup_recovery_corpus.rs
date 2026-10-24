use serde_json::{Value, json};
use std::path::PathBuf;
use studio_runtime_services::{monitor_runtime, scheduler};
use studio_runtime_store::RuntimeStore;

const NOW: f64 = 2_000_000_000.0;
// Permission bits keep the disposable identity private, as required by the store.
#[cfg(unix)]
const PRIVATE_DIRECTORY_MODE: u32 = 0o700;
#[cfg(unix)]
const PRIVATE_FILE_MODE: u32 = 0o600;

fn state_root() -> tempfile::TempDir {
    tempfile::tempdir().expect("startup state")
}

fn rust_snapshot(store: &RuntimeStore) -> Value {
    let db = store.test_reader().expect("read Rust startup rows");
    let records = |table: &str| -> Vec<Value> {
        let mut statement = db
            .prepare(&format!("SELECT record FROM {table} ORDER BY id"))
            .expect("prepare startup snapshot");
        statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("query startup records")
            .map(|row| {
                let mut record: Value =
                    serde_json::from_str(&row.expect("startup row")).expect("startup record JSON");
                let id = record["id"].as_str().unwrap_or_default().to_owned();
                studio_runtime_store::agent_volatile::merge_if_agent(&db, table, &id, &mut record)
                    .expect("volatile agent fields");
                record
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

#[tokio::test]
async fn python_constructor_and_rust_startup_match_requests_monitors_events_and_projections() {
    let root = state_root();
    let rust_state = root.path().join("rust");
    crate::recovery_fixtures::state(
        &rust_state,
        include_str!("startup_recovery/canvas.sql"),
        include_str!("startup_recovery/analytics.sql"),
    );
    let identity_directory = rust_state.join("multi-server");
    std::fs::create_dir(&identity_directory).expect("create identity directory");
    let identity_path = identity_directory.join("identity.json");
    std::fs::write(
        &identity_path,
        include_str!("startup_recovery/server-identity.json"),
    )
    .expect("restore frozen public server identity");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(
            &identity_directory,
            std::fs::Permissions::from_mode(PRIVATE_DIRECTORY_MODE),
        )
        .expect("private identity directory");
        std::fs::set_permissions(
            &identity_path,
            std::fs::Permissions::from_mode(PRIVATE_FILE_MODE),
        )
        .expect("private identity file");
    }
    let python: Value = serde_json::from_str(include_str!("startup_recovery/oracle.json"))
        .expect("frozen startup snapshot");

    let store = RuntimeStore::open(Some(&rust_state))
        .await
        .expect("open Python-written state");
    monitor_runtime::recover_on_startup(&store, NOW)
        .await
        .expect("Rust startup recovery");
    scheduler::recover_on_startup(&store)
        .await
        .expect("existing account-worker startup recovery");
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
    // bb5f67c67 retired the published token count, not the stored agent field.
    // Adapt only the frozen Python agent view and require Rust to omit it.
    for projection in python["sync_entities"]
        .as_array_mut()
        .expect("Python sync entities")
        .iter_mut()
        .filter(|projection| projection["collection"] == "agent")
    {
        let actual = rust["sync_entities"]
            .as_array()
            .expect("Rust sync entities")
            .iter()
            .find(|actual| actual["collection"] == "agent" && actual["id"] == projection["id"])
            .expect("matching Rust agent projection");
        assert!(
            actual["payload"]["value"].get("tokensUsed").is_none(),
            "Rust must not publish the retired tokensUsed field"
        );
        assert_eq!(
            projection["payload"]["value"]
                .as_object_mut()
                .expect("Python agent projection")
                .remove("tokensUsed"),
            Some(json!(0)),
            "the frozen Python agent view contains its retired token count"
        );
    }
    assert_eq!(rust, python, "complete startup rows and projections differ");
}
