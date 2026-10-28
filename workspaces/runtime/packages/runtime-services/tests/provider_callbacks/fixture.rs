//! Shared callback fixture written through current owner contracts.
use rusqlite::{Transaction, params, types::Value as SqlValue};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use studio_runtime_schema::persistence::{
    EventKind, EventStatus, RequestIdentity, ToolRequestKind, ToolRequestStatus,
};
use studio_runtime_store::request_results::{Admission, RequestResults, ResultEnvelope};
use studio_runtime_store::{
    AgentRecord, AgentRepository, EventMetadataRecord, ExecutionRunRecord, MonitorRecord,
    ProjectsRepository, QueueEvent, QueueEventRepository, RequestRecord, RuntimeStore, StoreError,
    SyncDocumentsRepository, Table, ToolResultRecord, TranscriptItemRecord,
};

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Seed {
    agents: Vec<AgentRecord>,
    event_metadata: Vec<Metadata>,
    events: Vec<Event>,
    execution_runs: Vec<Value>,
    execution_effects: Vec<Value>,
    execution_nodes: Vec<ExecutionNode>,
    transcript_items: Vec<Item>,
    monitors: Vec<MonitorRecord>,
    projects: Vec<Value>,
    requests: Vec<RequestRecord>,
    tool_requests: Vec<Value>,
    sync_entities: Vec<Entity>,
    metadata: Vec<String>,
    #[serde(rename = "analytics.sqlite3")]
    analytics: Vec<String>,
}
#[derive(serde::Serialize, Deserialize)]
struct ExecutionNode {
    status: studio_runtime_schema::persistence::ExecutionNodeStatus,
    #[serde(flatten)]
    fields: serde_json::Map<String, Value>,
}

#[derive(Deserialize)]
struct Metadata {
    id: String,
    record: EventMetadataRecord,
}
#[derive(Deserialize)]
struct Item {
    id: String,
    agent: String,
    record: TranscriptItemRecord,
    created: f64,
}
#[derive(Deserialize)]
struct Entity {
    collection: String,
    id: String,
    value: Value,
    deleted: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Event {
    id: String,
    agent: String,
    kind: EventKind,
    text: String,
    status: EventStatus,
    created: f64,
    epoch: i64,
    turn_id: Option<String>,
    error: Option<String>,
}

fn replace_ids(value: &mut Value, ids: &BTreeMap<String, String>) {
    match value {
        Value::String(text) => {
            if let Some(id) = ids.get(text) {
                *text = id.clone();
            } else if let Ok(mut encoded) = serde_json::from_str::<Value>(text)
                && (encoded.is_object() || encoded.is_array())
            {
                replace_ids(&mut encoded, ids);
                *text = encoded.to_string();
            }
        }
        Value::Array(items) => {
            for item in items {
                replace_ids(item, ids);
            }
        }
        Value::Object(fields) => {
            for item in fields.values_mut() {
                replace_ids(item, ids);
            }
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => {}
    }
}

fn seed_tools(tx: &Transaction<'_>, tools: Vec<Value>) -> Result<(), StoreError> {
    for mut task in tools {
        let id = task["id"].as_str().expect("tool identity").to_owned();
        let identity = RequestIdentity::new(
            "managed_tool",
            &[
                task["agent"].as_str().expect("tool agent"),
                task["accountKey"].as_str().expect("tool account"),
                task["threadId"].as_str().expect("tool thread"),
            ],
            &id,
            &[],
        )
        .expect("fixture UUIDv7");
        let Admission::Fresh(token) = RequestResults::admit(
            tx,
            &identity,
            task["signature"].as_str().expect("tool signature"),
            chrono::Utc::now().timestamp_millis(),
        )?
        else {
            panic!("unique fixture task");
        };
        task["admitted"] = serde_json::to_value(&token)?;
        task["kind"] = json!(ToolRequestKind::ManagedTool);
        task["status"] = serde_json::to_value(serde_json::from_value::<ToolRequestStatus>(
            task["stage"].clone(),
        )?)?;
        task["supervisor"] = json!({"generation":7});
        if matches!(task["outcome"].as_str(), Some("applied" | "not_applied")) {
            let reply = ToolResultRecord::from_value(
                task.as_object_mut()
                    .expect("task")
                    .remove("result")
                    .expect("completed reply"),
            )?;
            RequestResults::settle(
                tx,
                &token,
                &ResultEnvelope {
                    actor: task["agent"].as_str().map(str::to_owned),
                    reply,
                    references: Vec::new(),
                },
            )?;
        }
        let task =
            studio_runtime_store::system_reads::validate("runtime_tool_requests", &id, task)?;
        tx.execute(
            "INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2)",
            params![id, task.to_string()],
        )?;
    }
    Ok(())
}

pub(crate) async fn state() -> (tempfile::TempDir, RuntimeStore, Value) {
    let root = tempfile::tempdir().expect("isolated callback state");
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("callback store");
    let mut seed: Seed =
        serde_json::from_str(include_str!("seed.json")).expect("current callback seed");
    let mut oracle: Value =
        serde_json::from_str(include_str!("expected.json")).expect("callback expectations");
    let mut ids = BTreeMap::new();
    for task in &seed.tool_requests {
        ids.insert(
            task["id"].as_str().expect("task id").to_owned(),
            RequestIdentity::issue_root(),
        );
        if let Some(alias) = task["request_id"].as_str() {
            ids.insert(alias.to_owned(), RequestIdentity::issue_root());
        }
    }
    for effect in &seed.execution_effects {
        if let Some(id) = effect["referenceId"]
            .as_str()
            .and_then(|id| ids.get(id))
            .cloned()
        {
            ids.insert(
                effect["id"].as_str().expect("effect id").to_owned(),
                format!("{}:{id}", effect["kind"].as_str().expect("effect kind")),
            );
        }
    }
    replace_ids(&mut oracle, &ids);
    for record in seed
        .tool_requests
        .iter_mut()
        .chain(seed.execution_effects.iter_mut())
    {
        replace_ids(record, &ids);
    }
    for node in &mut seed.execution_nodes {
        for field in node.fields.values_mut() {
            replace_ids(field, &ids);
        }
    }
    for entity in &mut seed.sync_entities {
        if let Some(id) = ids.get(&entity.id) {
            entity.id = id.clone();
        }
        replace_ids(&mut entity.value, &ids);
    }
    for case in oracle["reservations"]
        .as_array_mut()
        .expect("reservation oracle")
    {
        let mut args = case["message"]["params"]["arguments"].clone();
        if let Some(encoded) = args.as_str() {
            args = serde_json::from_str(encoded).expect("encoded arguments");
        }
        let signature = studio_runtime_store::sha256_hex(
            studio_runtime_store::python_json_compact(
                &json!({"tool":case["message"]["params"]["tool"],"arguments":args}),
            )
            .as_bytes(),
        );
        case["record"]["signature"] = json!(signature);
        let task = seed
            .tool_requests
            .iter_mut()
            .find(|task| task["callId"] == case["record"]["callId"])
            .expect("seeded reservation");
        task["signature"] = json!(signature);
        task["arguments"] = args;
    }
    let analytics = seed.analytics.join("\n");
    store.transaction(move |tx| {
        for agent in seed.agents { AgentRepository::put(tx, &agent)?; }
        for metadata in seed.event_metadata { Table::new(tx).put_at(&metadata.id.into(), &metadata.record)?; }
        for event in seed.events { QueueEventRepository::put(tx, &QueueEvent {id:event.id.into(),agent:event.agent.into(),kind:event.kind.as_str().into(),text:event.text,status:event.status.as_str().into(),created:event.created,epoch:event.epoch,turn_id:event.turn_id,error:event.error})?; }
        for record in seed.execution_runs {
            let fields = [
                ("agent",SqlValue::Text(record["agent"].as_str().expect("run agent").into())),
                ("account",SqlValue::Text(record["accountKey"].as_str().expect("run account").into())),
                ("epoch",SqlValue::Integer(record["epoch"].as_i64().expect("run epoch"))),
                ("thread",record["threadId"].as_str().map_or(SqlValue::Null,|s|SqlValue::Text(s.into()))),
                ("turn",record["turnId"].as_str().map_or(SqlValue::Null,|s|SqlValue::Text(s.into()))),
                ("created",SqlValue::Real(record["created"].as_f64().expect("run time"))),
            ];
            Table::new(tx).put_with_columns(&ExecutionRunRecord::from_value(record)?, &fields)?;
        }
        for record in seed.execution_effects {
            let id = record["id"].as_str().expect("effect id");
            let record = studio_runtime_store::system_reads::validate("runtime_execution_effects", id, record.clone())?;
            tx.execute("INSERT INTO runtime_execution_effects(id,run,kind,reference,record) VALUES(?1,?2,?3,?4,?5)",params![record["id"].as_str(),record["runId"].as_str(),record["kind"].as_str(),record["referenceId"].as_str(),record.to_string()])?;
        }
        for record in seed.execution_nodes {
            tx.execute("INSERT INTO runtime_execution_nodes(id,run,record) VALUES(?1,?2,?3)",params![record.fields["id"].as_str(),record.fields["runId"].as_str(),serde_json::to_string(&record)?])?;
        }
        for item in seed.transcript_items { assert_eq!(item.record.value()["id"],item.id); Table::new(tx).put_with_columns(&item.record,&[("agent",SqlValue::Text(item.agent)),("created",SqlValue::Real(item.created))])?; }
        for monitor in seed.monitors { Table::new(tx).put(&monitor)?; }
        for project in seed.projects { ProjectsRepository::put_project(tx,&project)?; }
        for request in seed.requests { Table::new(tx).put(&request)?; }
        seed_tools(tx,seed.tool_requests)?;
        for entity in seed.sync_entities { SyncDocumentsRepository::put_entity(tx,&entity.collection,&entity.id,entity.value,entity.deleted)?; }
        // Metadata counters and read-only analytics have no typed write owner.
        tx.execute_batch(&seed.metadata.join("\n"))?;
        Ok(())
    }).await.expect("seed callback owners");
    store
        .analytics_transaction::<_, StoreError>(move |tx, _| {
            tx.execute_batch(&analytics)?;
            Ok(())
        })
        .await
        .expect("seed callback analytics");
    (root, store, oracle)
}
