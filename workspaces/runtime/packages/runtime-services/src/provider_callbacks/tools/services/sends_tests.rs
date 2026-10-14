use super::*;
use crate::managed_host_tools::SendPort;
use std::{path::PathBuf, process::Command, sync::OnceLock};
use studio_runtime_store::RuntimeStore;

#[path = "../../../../tests/support/prepared_python.rs"]
mod prepared_python;

fn template() -> &'static Value {
    static TEMPLATE: OnceLock<(tempfile::TempDir, Value)> = OnceLock::new();
    &TEMPLATE
        .get_or_init(|| {
            let scratch = tempfile::tempdir().unwrap();
            let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
            let source = manifest.join("../../apps/server/src");
            let oracle = manifest.join("tests/provider_callbacks/oracle.py");
            let output = Command::new(prepared_python::prepared_python(&source))
                .arg(oracle)
                .arg(scratch.path())
                .output()
                .unwrap();
            assert!(
                output.status.success(),
                "oracle: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            (scratch, serde_json::from_slice(&output.stdout).unwrap())
        })
        .1
}

fn send(sender_index: usize, target: &str, id: &str) -> (tempfile::TempDir, RuntimeStore, Value) {
    let oracle = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(oracle["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = RuntimeStore::open(Some(directory.path())).unwrap();
    let actor = oracle["agents"][sender_index].clone();
    let context = CallbackContext {
        agent: actor["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: actor["epoch"].as_u64().unwrap(),
        thread: actor["threadId"].as_str().unwrap().into(),
        turn: actor["turnId"].as_str().unwrap().into(),
    };
    let reader = store.read().unwrap();
    let admission =
        crate::message_admission::PreparedAdmission::load(&reader, &context.agent).unwrap();
    drop(reader);
    let ports = ProductionPorts::new(context.clone(), None, admission);
    let raw: String = store
        .read()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [&context.agent],
            |row| row.get(0),
        )
        .unwrap();
    let actor = crate::managed_host_tools::Actor::from_record(
        &serde_json::from_str(&raw).unwrap(),
        &context.agent,
        context.epoch,
    )
    .unwrap();
    let args = json!({"agent_id":target,"text":"dispatch input"});
    let result = store
        .transaction_with(|tx| ports.send(tx, &actor, &args, id, 20.0))
        .unwrap();
    (directory, store, result.value)
}

#[test]
fn complaint_submit_read_and_respond_share_the_notification_service() {
    let oracle = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(oracle["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = RuntimeStore::open(Some(directory.path())).unwrap();
    let worker = oracle["agents"][1].clone();
    let lead = oracle["agents"][0].clone();
    let submitted = store
        .transaction_with(|tx| {
            crate::notification_dispatch::agent_complaint_action(
                tx,
                &worker,
                &json!({"action":"submit","text":"Need help"}),
                "complaint-call",
                20.0,
            )
        })
        .unwrap();
    assert_eq!(submitted["recipient"], "lead");
    let complaint_id = submitted["id"].as_str().unwrap();
    let read = store
        .transaction_with(|tx| {
            crate::notification_dispatch::agent_complaint_action(
                tx,
                &lead,
                &json!({"action":"read","complaint_id":complaint_id}),
                "read-call",
                21.0,
            )
        })
        .unwrap();
    assert_eq!(read["complaints"][0]["readAt"], 21.0);
    let responded = store
        .transaction_with(|tx| {
            crate::notification_dispatch::agent_complaint_action(
                tx,
                &lead,
                &json!({"action":"respond","complaint_id":complaint_id,"text":"Investigating","status":"in_progress"}),
                "response-call",
                22.0,
            )
        })
        .unwrap();
    assert_eq!(responded["responses"][0]["author"], lead["id"]);
    let reader = store.read().unwrap();
    let response_event: i64 = reader
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE agent=?1 AND kind='complaint_response'",
            [worker["id"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(response_event, 1);
}

#[test]
fn send_to_descendant_is_followup_input_while_parent_and_broadcast_are_chat() {
    let oracle = template();
    let child = oracle["agents"][1]["id"].as_str().unwrap();
    let lead = oracle["agents"][0]["id"].as_str().unwrap();

    let (_root, store, value) = send(0, child, "send-child");
    assert_eq!(value["status"], "queued");
    let reader = store.read().unwrap();
    let event: (String, String, String) = reader
        .query_row(
            "SELECT agent,kind,text FROM runtime_events WHERE id='send-child'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .unwrap();
    assert_eq!(
        event,
        (child.to_owned(), "followup".into(), "dispatch input".into())
    );
    drop(reader);

    let (_root, store, value) = send(1, lead, "send-parent");
    let mut member_ids = [lead, child];
    member_ids.sort_unstable();
    assert_eq!(
        value["room"],
        format!("private:{}:{}", member_ids[0], member_ids[1])
    );
    let reader = store.read().unwrap();
    let event_count: i64 = reader
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id='send-parent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let chat_count: i64 = reader
        .query_row(
            "SELECT count(*) FROM runtime_chat_messages WHERE id='send-parent'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!((event_count, chat_count), (0, 1));
    drop(reader);

    let (_root, store, value) = send(1, "broadcast", "send-broadcast");
    assert!(value["room"].as_str().unwrap().starts_with("broadcast:"));
    let reader = store.read().unwrap();
    let chat_count: i64 = reader
        .query_row(
            "SELECT count(*) FROM runtime_chat_messages WHERE id='send-broadcast'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(chat_count, 1);
}
