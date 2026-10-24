//! Every writer transaction publishes the client view of the agent and request
//! rows it changed. The guard tests write through raw SQL, the lowest layer that
//! every repository and future write path uses, so a write path that skips the
//! publication fails them.

use super::test_support::{chat_view, client_entity, seed_chat, store};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::{AgentRecord, AgentRepository, StoreError};

const CHAT: &str = "published-chat";

#[tokio::test]
async fn summary_entities_publish_at_the_source_write() {
    let (_dir, store) = store().await;
    seed_chat(&store, chat(json!({}))).await;
    let plan = json!({"id":CHAT,"rootId":CHAT,"text":"Plan","version":1,"updated":1});
    write(
        &store,
        "INSERT INTO runtime_plans(id,record) VALUES(?1,?2)",
        vec![CHAT.to_owned(), plan.to_string()],
    )
    .await;
    assert_eq!(
        client_entity(&store, "plan", CHAT).expect("plan entity").1["text"],
        "Plan"
    );
    let annotation = json!({"id":"note","agent":CHAT,"rootId":CHAT,"path":"a.rs","line":1,"text":"Comment","created":1});
    write(
        &store,
        "INSERT INTO runtime_annotations(id,record) VALUES(?1,?2)",
        vec!["note".into(), annotation.to_string()],
    )
    .await;
    assert_eq!(
        client_entity(&store, "annotation", "note")
            .expect("annotation entity")
            .1["text"],
        "Comment"
    );
    write(
        &store,
        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,?2,'user','Input','pending',1,1)",
        vec!["receipt".into(), CHAT.into()],
    )
    .await;
    let receipt = client_entity(&store, "messageReceipt", "receipt")
        .expect("receipt entity")
        .1;
    assert_eq!(receipt["agent"], CHAT);
    assert_eq!(receipt["status"], "pending");
    assert_eq!(receipt["materialized"], false);
    let item_id = format!("{CHAT}:receipt");
    let item = json!({"id":item_id,"role":"user","text":"Input","at":1});
    write(
        &store,
        "INSERT INTO runtime_items(id,agent,record,created) VALUES(?1,?2,?3,1)",
        vec![item_id.clone(), CHAT.into(), item.to_string()],
    )
    .await;
    assert_eq!(
        client_entity(&store, "messageReceipt", "receipt")
            .expect("materialized receipt")
            .1["materialized"],
        true
    );
    write(
        &store,
        "DELETE FROM runtime_items WHERE id=?1",
        vec![item_id],
    )
    .await;
    assert_eq!(
        client_entity(&store, "messageReceipt", "receipt")
            .expect("retained receipt")
            .1["materialized"],
        false
    );
    write(
        &store,
        "UPDATE runtime_events SET status='delivered' WHERE id=?1",
        vec!["receipt".into()],
    )
    .await;
    assert_eq!(
        client_entity(&store, "messageReceipt", "receipt")
            .expect("receipt")
            .1["status"],
        "delivered"
    );
    let task = json!({"id":"task","agent":CHAT,"kind":"command","status":"running","created":1,"tail":"output","error":"failure","arguments":"Explicit read only"});
    write(
        &store,
        "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
        vec!["task".into(), task.to_string()],
    )
    .await;
    let summary = client_entity(&store, "task", "task")
        .expect("task summary")
        .1;
    assert_eq!(summary["tail"], "output");
    assert_eq!(summary["error"], "failure");
    assert!(summary.get("arguments").is_none());
    let tail = format!("{}Newest output", "Older output ".repeat(1_000));
    write(
        &store,
        "UPDATE runtime_tasks SET record=json_set(record,'$.tail',?2) WHERE id=?1",
        vec!["task".into(), tail.clone()],
    )
    .await;
    let summary = client_entity(&store, "task", "task")
        .expect("bounded task summary")
        .1;
    assert!(
        summary["tail"]
            .as_str()
            .expect("tail")
            .ends_with("Newest output")
    );
    assert!(summary["tail"].as_str().expect("tail").len() < tail.len());
    assert_eq!(summary["outputTruncated"], true);
    for (table, collection, id) in [
        ("runtime_tasks", "task", "task"),
        ("runtime_annotations", "annotation", "note"),
        ("runtime_plans", "plan", CHAT),
        ("runtime_events", "messageReceipt", "receipt"),
    ] {
        let (table, id) = (table.to_owned(), id.to_owned());
        let lookup_id = id.clone();
        store
            .transaction_with(move |tx| {
                tx.execute(&format!("DELETE FROM {table} WHERE id=?1"), [id])?;
                Ok::<_, StoreError>(())
            })
            .await
            .expect("delete source");
        assert!(
            client_entity(&store, collection, &lookup_id)
                .expect("summary tombstone")
                .0
        );
    }
}

fn chat(extra: Value) -> Value {
    let mut record = json!({
        "id":CHAT,"rootId":CHAT,"epoch":1,"status":"idle","name":"Published chat",
        "provider":"codex","model":"gpt-6","prompt":"Start","cwd":"/fixture",
        "accountKey":"default","isLead":true,"autoWake":false
    });
    record
        .as_object_mut()
        .expect("chat fixture")
        .extend(extra.as_object().expect("extra fixture").clone());
    record
}

async fn write(store: &studio_runtime_store::RuntimeStore, sql: &'static str, params: Vec<String>) {
    store
        .transaction_with(move |tx| {
            tx.execute(sql, rusqlite::params_from_iter(params))
                .map_err(StoreError::from)?;
            Ok::<_, StoreError>(())
        })
        .await
        .expect("the raw write commits");
}

#[tokio::test]
async fn a_raw_agent_row_write_reaches_the_client() {
    let (_dir, store) = store().await;
    seed_chat(&store, chat(json!({}))).await;
    write(
        &store,
        "UPDATE runtime_agents SET record=json_set(record,'$.name','Renamed by SQL') WHERE id=?1",
        vec![CHAT.to_owned()],
    )
    .await;
    assert_eq!(chat_view(&store, CHAT)["name"], "Renamed by SQL");

    let unpublished = "unpublished-chat";
    let mut record = chat(json!({"name":"Never published"}));
    record["id"] = json!(unpublished);
    record["rootId"] = json!(unpublished);
    write(
        &store,
        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
        vec![unpublished.to_owned(), record.to_string()],
    )
    .await;
    assert_eq!(
        chat_view(&store, unpublished)["name"],
        "Never published",
        "a chat without a client entity gets one"
    );
}

#[tokio::test]
async fn studio_error_rows_are_published_as_entities_with_their_source_write() {
    let (_dir, store) = store().await;
    seed_chat(&store, chat(json!({}))).await;
    write(
        &store,
        "UPDATE runtime_agents SET record=json_set(record,'$.status','failed','$.error',?2) WHERE id=?1",
        vec![CHAT.to_owned(), studio_runtime_schema::shared_values::PROVIDER_DISCONNECTED_TURN_ERROR.to_owned()],
    )
    .await;

    let (deleted, value) = client_entity(&store, "studioError", "agent:published-chat:")
        .expect("Studio error entity is published with its source row");
    assert!(!deleted);
    assert_eq!(value["agentId"], CHAT);
    assert_eq!(value["kind"], "turn_disconnected");
    let sequence = store
        .test_read_transaction(|tx| {
            studio_runtime_store::SyncDocumentsRepository::entity_sequence(tx)
        })
        .expect("source publication sequence");
    crate::studio_errors::refresh(&store)
        .await
        .expect("equal refresh");
    assert_eq!(
        store
            .test_read_transaction(|tx| {
                studio_runtime_store::SyncDocumentsRepository::entity_sequence(tx)
            })
            .expect("equal refresh sequence"),
        sequence
    );

    store
        .transaction_with(|tx| crate::studio_errors::dismiss(tx, "agent:published-chat:", 100.0))
        .await
        .expect("dismissal commits");
    assert!(
        client_entity(&store, "studioError", "agent:published-chat:")
            .expect("tombstone")
            .0
    );
}

#[tokio::test]
async fn a_raw_request_row_write_reaches_the_client_and_a_closed_one_leaves_it() {
    let (_dir, store) = store().await;
    seed_chat(&store, chat(json!({}))).await;
    let card = json!({"id":"raw-card","agent":CHAT,"method":"agent/asyncQuestion",
        "status":"pending","params":{"message":"Continue?"}});
    write(
        &store,
        "INSERT INTO runtime_requests(id,record) VALUES(?1,?2)",
        vec!["raw-card".to_owned(), card.to_string()],
    )
    .await;
    let (retired, view) = client_entity(&store, "request", "raw-card").expect("card published");
    assert!(!retired);
    assert_eq!(view["status"], "pending");
    write(
        &store,
        "UPDATE runtime_requests SET record=json_set(record,'$.status','answered') WHERE id=?1",
        vec!["raw-card".to_owned()],
    )
    .await;
    let (retired, _) = client_entity(&store, "request", "raw-card").expect("card known");
    assert!(retired, "an answered card leaves the client");
}

#[tokio::test]
async fn a_publication_before_a_later_write_in_the_same_transaction_is_redone() {
    let (_dir, store) = store().await;
    seed_chat(&store, chat(json!({}))).await;
    store
        .transaction_with(|tx| {
            let mut agent = AgentRepository::get(tx, &CHAT.into())?.expect("chat");
            crate::agents_create::publish_agent(tx, &agent.value())?;
            agent.extra.insert("name".into(), json!("Written after"));
            AgentRepository::put(tx, &agent)?;
            Ok::<_, StoreError>(())
        })
        .await
        .expect("the write commits");
    assert_eq!(chat_view(&store, CHAT)["name"], "Written after");
}

#[tokio::test]
async fn synced_agent_values_recompute_the_view_and_create_a_missing_entity() {
    let dir = tempfile::tempdir().expect("isolated state");
    let store = studio_runtime_store::RuntimeStore::open(Some(dir.path()))
        .await
        .expect("store");
    let worker = json!({
        "id":"sync-worker","rootId":CHAT,"parentId":CHAT,"epoch":1,"status":"running",
        "name":"Worker","provider":"codex","model":"gpt-6","prompt":"Task","cwd":"/fixture",
        "accountKey":"default","isLead":false,"autoWake":false
    });
    store
        .transaction_with(move |tx| {
            // The row exists, but its writer never published it.
            AgentRepository::put(tx, &AgentRecord::from_value(worker)?)?;
            Ok::<_, StoreError>(())
        })
        .await
        .expect("the worker row is stored");
    super::install(&store).await.expect("publisher installed");
    assert!(client_entity(&store, "agent", "sync-worker").is_none());
    store
        .transaction_with(|tx| {
            let mut agent = AgentRepository::get(tx, &"sync-worker".into())?.expect("worker");
            for (key, value) in [
                ("status", json!("completed")),
                ("lastCompletedTurn", json!("turn-1")),
                ("lastAnswer", json!("The work is done.")),
            ] {
                agent.extra.insert(key.into(), value);
            }
            AgentRepository::put(tx, &agent)?;
            studio_runtime_store::EventWriteRepository::set_agent_sync_values(
                tx,
                "sync-worker",
                &[("status", json!("completed"))],
            )?;
            Ok::<_, StoreError>(())
        })
        .await
        .expect("a sync of a chat without a client entity commits");
    let view = chat_view(&store, "sync-worker");
    assert_eq!(view["status"], "completed");
    assert_eq!(
        view["overview"]["result"], "The work is done.",
        "the derived overview follows the stored row"
    );
}

// Item 4 of the audit: Claude session controls (POST /api/claude/session).
#[tokio::test]
async fn a_claude_control_write_reaches_the_client() {
    let (_dir, store) = store().await;
    seed_chat(&store, chat(json!({"provider":"claude","status":"idle"}))).await;
    store
        .transaction_with(|tx| {
            let mut agent = AgentRepository::get(tx, &CHAT.into())?
                .expect("chat")
                .value();
            agent["status"] = json!("paused");
            agent["workspaceOperation"] = json!("branch_recovery");
            let operation = json!({"id":"control-op","requestId":"control-request",
                "kind":"claude_rollback","agent":CHAT,"signature":"a".repeat(64),"source":{},
                "phase":"recovery_required","created":1.0});
            studio_runtime_store::ClaudeControlRepository::apply(tx, &agent, &operation, &[])
        })
        .await
        .expect("the control write commits");
    let view = chat_view(&store, CHAT);
    assert_eq!(view["status"], "paused");
    assert_eq!(view["workspaceOperation"], "branch_recovery");
}

// Item 7 of the audit: usage resume (POST /api/usage-resume).
#[tokio::test]
async fn a_usage_resume_change_reaches_the_client() {
    let (_dir, store) = store().await;
    let resume = json!({"id":"resume-1","status":"scheduled","dueAt":123.0,"threadId":"thread",
        "turnId":"turn","accountKey":"default","epoch":1,"cause":"usage_limit","failedAt":100.0,
        "authAttempt":0,"authRefreshMarker":null,"plannedAt":null,"resetAt":null,"reason":null});
    seed_chat(&store, chat(json!({"usageResume":resume.clone()}))).await;
    store
        .transaction_with(move |tx| {
            tx.execute(
                "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES('resume-1',?1,?2)",
                rusqlite::params![CHAT, resume.to_string()],
            )
            .map_err(StoreError::from)?;
            studio_runtime_store::disable_usage_resume(tx, CHAT, "resume-1", 200.0)
                .map_err(|_| StoreError::RecordContract("usage resume"))?;
            Ok::<_, StoreError>(())
        })
        .await
        .expect("the usage resume write commits");
    let view = chat_view(&store, CHAT);
    assert_eq!(view["usageResume"]["status"], "cancelled");
}

// Item 9 of the audit: the repair after a Claude sign-in.
#[tokio::test]
async fn a_claude_sign_in_repair_reaches_the_client() {
    use crate::claude_login::ports::LoginChats;
    let (_dir, store) = store().await;
    seed_chat(
        &store,
        chat(
            json!({"provider":"claude","accountKey":"claude-account","status":"failed",
            "error":{"message":"OAuth session expired"}}),
        ),
    )
    .await;
    let store = Arc::new(store);
    crate::claude_login::chats::StoredChats(Arc::clone(&store))
        .refresh("claude-account")
        .await
        .expect("the repair commits");
    let view = chat_view(&store, CHAT);
    assert_eq!(
        view["status"], "idle",
        "the repaired chat no longer shows failed"
    );
    assert_eq!(view["error"], Value::Null);
}

#[tokio::test]
async fn queue_source_writes_publish_and_remove_items_without_http_reads() {
    let (_dir, store) = store().await;
    seed_chat(&store, chat(json!({}))).await;
    write(&store, "INSERT INTO runtime_events(id,agent,epoch,kind,text,status,created) VALUES(?1,?2,1,'user','Queued text','pending',1)", vec!["queued-source".into(), CHAT.into()]).await;
    let (deleted, view) = client_entity(&store, "queue", CHAT).expect("queue published at source");
    assert!(!deleted);
    assert_eq!(view["items"][0]["text"], "Queued text");
    assert_eq!(view["capabilities"]["reorder"], true);
    let revision = view["revision"].clone();
    write(
        &store,
        "UPDATE runtime_events SET status='delivered' WHERE id=?1",
        vec!["queued-source".into()],
    )
    .await;
    let (_, view) = client_entity(&store, "queue", CHAT).expect("empty queue published");
    assert_eq!(view["items"], json!([]));
    assert_ne!(view["revision"], revision);
}

#[tokio::test]
async fn terminal_source_writes_publish_summary_without_process_identity() {
    let (_dir, store) = store().await;
    seed_chat(&store, chat(json!({}))).await;
    let terminal = json!({"id":"terminal-source","agent":CHAT,"cwd":"/fixture","title":"Source shell","status":"running","created":1,"updated":1,"_processIdentity":{"pid":999}});
    write(
        &store,
        "INSERT INTO user_terminals(id,record,output,offset) VALUES(?1,?2,'',0)",
        vec!["terminal-source".into(), terminal.to_string()],
    )
    .await;
    let (_, value) =
        client_entity(&store, "terminal", "terminal-source").expect("terminal summary published");
    assert_eq!(value["title"], "Source shell");
    assert!(value.get("_processIdentity").is_none());
    write(
        &store,
        "UPDATE user_terminals SET record=json_set(record,'$.status','closed') WHERE id=?1",
        vec!["terminal-source".into()],
    )
    .await;
    assert!(
        client_entity(&store, "terminal", "terminal-source")
            .expect("terminal removal published")
            .0
    );
}
