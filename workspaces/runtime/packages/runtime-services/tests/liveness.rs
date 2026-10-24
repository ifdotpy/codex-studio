//! The liveness pass wakes an idle chat whose owner message waits, once, and
//! leaves a held chat and its input alone.
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore};

async fn seeded(now: f64) -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let root = tempfile::tempdir().expect("liveness state");
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
    let path = root.path().to_path_buf();
    store
        .transaction(move |tx| {
            for (id, fields) in [
                ("idle", json!({})),
                ("stopped", json!({"status":"paused","autoWake":false})),
                ("held", json!({"status":"waiting","nativeFailureHold":true})),
            ] {
                let mut record = json!({"id":id,"rootId":id,"epoch":0,"status":"completed",
                    "autoWake":true,"cwd":path.to_string_lossy(),"accountKey":"default",
                    "provider":"codex","name":id});
                for (key, value) in fields.as_object().expect("fields") {
                    record[key] = value.clone();
                }
                AgentRepository::put(tx, &AgentRecord::from_value(record)?)?;
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) \
                     VALUES (?1,?2,'user','continue','pending',?3,0)",
                    rusqlite::params![format!("{id}-message"), id, now - 180.0],
                )?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("seed");
    (root, store)
}

async fn agent(store: &RuntimeStore, id: &'static str) -> Value {
    store
        .read_async("tests/liveness", move |tx| {
            AgentRepository::get(tx, &AgentId::from(id))
        })
        .await
        .expect("read")
        .map(|record| serde_json::to_value(record).expect("agent json"))
        .expect("agent exists")
}

async fn event_status(store: &RuntimeStore, id: &'static str) -> String {
    store
        .read_async("tests/liveness", move |tx| {
            Ok::<_, studio_runtime_store::StoreError>(tx.query_row(
                "SELECT status FROM runtime_events WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )?)
        })
        .await
        .expect("event")
}

#[tokio::test]
async fn an_idle_chat_with_a_waiting_message_is_woken_once() {
    let now = studio_runtime_services::event_write::unix_time().expect("clock");
    let (_root, store) = seeded(now).await;

    let report = studio_runtime_services::liveness::pass(&store, now)
        .await
        .expect("liveness pass");
    assert_eq!(report.woken, ["idle"], "only the idle, unheld chat wakes");
    assert_eq!(agent(&store, "idle").await["status"], "queued");
    assert_eq!(
        event_status(&store, "idle-message").await,
        "pending",
        "the wake does not touch the input"
    );
    for held in ["stopped", "held"] {
        assert_ne!(agent(&store, held).await["status"], "queued", "{held}");
    }

    // The claim did not happen (for example a hold that only the claim sees):
    // the chat is idle again, and the same message is not woken twice.
    store
        .transaction(|tx| {
            let id = AgentId::from("idle");
            let mut record = AgentRepository::get(tx, &id)?.expect("idle");
            record.status = studio_runtime_store::AgentStatus::Completed;
            AgentRepository::put(tx, &record)
        })
        .await
        .expect("reset");
    let again = studio_runtime_services::liveness::pass(&store, now + 120.0)
        .await
        .expect("second pass");
    assert!(again.woken.is_empty(), "one wake per waiting message");
    assert_eq!(agent(&store, "idle").await["status"], "completed");
}
