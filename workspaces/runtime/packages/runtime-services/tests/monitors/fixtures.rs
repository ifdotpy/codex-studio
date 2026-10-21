use serde_json::{Value, json};
use std::path::Path;
use studio_runtime_store::{AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table};

/// The monitor cases the recovery witnesses start from: one running command
/// of agent `agent`, and the exit event its successful finish must queue.
pub fn corpus(root: &Path) -> Vec<Value> {
    let cwd = root.to_string_lossy().into_owned();
    let id = "f8fc5c50-e4e4-50bc-8ff5-885105058c9d";
    let log = root.join("monitor-logs").join(format!("{id}.log"));
    let log = log.to_string_lossy().into_owned();
    let operation =
        json!({"agent":"agent","epoch":2,"accountKey":"default","connectionId":"connection"});
    let before = json!({"id":id,"agent":"agent","epoch":2,"turnId":null,"requestId":"monitor-corpus",
        "command":"printf marker","cwd":cwd,"timeout_ms":3600000,"stallTimeoutSeconds":1800,
        "livenessCommand":"","activityAt":2000.0,"activityGeneration":0,"stallWakeGeneration":-1,
        "wakeOn":"exit","successExitCodes":[0,2],"outputPatterns":[],"outputEventSequence":0,
        "outputEventCount":0,"outputEventIds":[],"status":"running","created":2000.0,
        "exitCode":null,"tail":"","bytes":0,"interactive":false,"ruleId":null,"log":log,
        "operation":operation,"cancelRequested":false});
    let actor = json!({"id":"agent","rootId":"agent","epoch":2,"autoWake":true,"status":"idle",
        "cwd":cwd,"accountKey":"default"});
    let events = json!([{"id":format!("monitor:{id}"),"agent":"agent","kind":"monitor_exit",
        "text":{"id":id,"command":"printf marker","status":"completed","exitCode":0,"error":null,
            "tail":"","log":log,"bytes":0},
        "status":"pending","created":2000.0,"epoch":2,"turn_id":null,"error":null}]);
    let mut after = before.clone();
    after["status"] = json!("completed");
    after["exitCode"] = json!(0);
    after["outputUtf8Pending"] = json!({});
    after["error"] = Value::Null;
    after["finished"] = json!(2000.0);
    after["configurationPending"] = json!(false);
    vec![
        json!({"name":"finish-0-None-False","action":"finish","before":before,"after":after,"actor":actor,
        "argument":[0,null],"events":events,"priorEvents":[],"probes":[],"probeResults":[],
        "submissions":[]}),
    ]
}

pub async fn store(root: &Path, name: &str) -> RuntimeStore {
    let target = root.join(name);
    std::fs::create_dir(&target).expect("monitor fixture");
    RuntimeStore::open(Some(&target))
        .await
        .expect("monitor fixture")
}

pub async fn seed(store: &RuntimeStore, case: &Value) {
    let case = case.clone();
    store
        .transaction(move |tx| {
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
        .await
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
