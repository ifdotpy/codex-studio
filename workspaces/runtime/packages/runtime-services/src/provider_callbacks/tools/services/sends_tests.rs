#![allow(clippy::unwrap_used)]
#![allow(clippy::duplicate_mod)]

use super::*;
use crate::managed_host_tools::{PostCommitPort, SendPort};
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

async fn send(
    sender_index: usize,
    target: &str,
    id: &str,
) -> (tempfile::TempDir, RuntimeStore, Value) {
    let (directory, store, result) = try_send(sender_index, target, id).await;
    (directory, store, result.unwrap())
}

async fn try_send(
    sender_index: usize,
    target: &str,
    id: &str,
) -> (
    tempfile::TempDir,
    RuntimeStore,
    Result<Value, crate::managed_host_tools::Error>,
) {
    try_send_args(
        sender_index,
        json!({"agent_id":target,"text":"dispatch input"}),
        id,
    )
    .await
}

async fn try_send_args(
    sender_index: usize,
    args: Value,
    id: &str,
) -> (
    tempfile::TempDir,
    RuntimeStore,
    Result<Value, crate::managed_host_tools::Error>,
) {
    let oracle = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(oracle["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
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
    let reader = store.test_reader().unwrap();
    let admission =
        crate::message_admission::PreparedAdmission::load(&reader, &context.agent).unwrap();
    drop(reader);
    let ports = ProductionPorts::new(ProductionPortsConfig {
        context: context.clone(),
        room_directory: None,
        admission,
        event_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        monitor_dispatch_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        runtime_ports: empty_runtime_ports(),
        state_root: store.state_dir().to_path_buf(),
        preparation: None,
    });
    let raw: String = store
        .test_reader()
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
    let id = id.to_owned();
    let result = store
        .transaction_with(move |tx| ports.send(tx, &actor, &args, &id, 20.0))
        .await
        .map(|result| result.value);
    (directory, store, result)
}

#[tokio::test]
async fn production_release_notifies_the_registry_after_a_committed_send() {
    let oracle = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(oracle["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
    let actor = oracle["agents"][0].clone();
    let context = CallbackContext {
        agent: actor["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: actor["epoch"].as_u64().unwrap(),
        thread: actor["threadId"].as_str().unwrap().into(),
        turn: actor["turnId"].as_str().unwrap().into(),
    };
    let reader = store.test_reader().unwrap();
    let admission =
        crate::message_admission::PreparedAdmission::load(&reader, &context.agent).unwrap();
    drop(reader);
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());
    let ports = ProductionPorts::new(ProductionPortsConfig {
        context,
        room_directory: None,
        admission,
        event_wake: wake.clone(),
        monitor_dispatch_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
        runtime_ports: empty_runtime_ports(),
        state_root: store.state_dir().to_path_buf(),
        preparation: None,
    });
    let mut notified = Box::pin(wake.notified());
    assert!(futures_util::FutureExt::now_or_never(notified.as_mut()).is_none());
    let actor = crate::managed_host_tools::Actor::from_record(
        &actor,
        oracle["agents"][0]["id"].as_str().unwrap(),
        3,
    )
    .unwrap();
    let child_id = oracle["agents"][1]["id"].as_str().unwrap().to_owned();
    let actor_for_send = actor.clone();
    let ports_for_send = std::sync::Arc::new(ports);
    let ports_for_release = std::sync::Arc::clone(&ports_for_send);
    let args = json!({"agent_id":child_id,"text":"wake after commit"});
    let staged = store
        .transaction_with(move |tx| {
            ports_for_send.send(tx, &actor_for_send, &args, "wake-after-commit", 20.0)
        })
        .await
        .unwrap();
    let event_count: i64 = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE id='wake-after-commit'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(event_count, 1, "durable input commits before release");
    ports_for_release.release(&staged.signals).unwrap();
    futures_util::FutureExt::now_or_never(notified.as_mut())
        .expect("released wake signal is delivered");
}

#[tokio::test]
async fn monitor_cancel_release_notifies_only_the_monitor_dispatcher() {
    let oracle = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(oracle["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
    let actor = oracle["agents"][0].clone();
    let context = CallbackContext {
        agent: actor["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: actor["epoch"].as_u64().unwrap(),
        thread: actor["threadId"].as_str().unwrap().into(),
        turn: actor["turnId"].as_str().unwrap().into(),
    };
    let reader = store.test_reader().unwrap();
    let admission =
        crate::message_admission::PreparedAdmission::load(&reader, &context.agent).unwrap();
    drop(reader);
    let event_wake = std::sync::Arc::new(tokio::sync::Notify::new());
    let monitor_wake = std::sync::Arc::new(tokio::sync::Notify::new());
    let ports = ProductionPorts::new(ProductionPortsConfig {
        context,
        room_directory: None,
        admission,
        event_wake: event_wake.clone(),
        monitor_dispatch_wake: monitor_wake.clone(),
        runtime_ports: empty_runtime_ports(),
        state_root: store.state_dir().to_path_buf(),
        preparation: None,
    });
    let mut event_notified = Box::pin(event_wake.notified());
    let mut monitor_notified = Box::pin(monitor_wake.notified());
    assert!(futures_util::FutureExt::now_or_never(event_notified.as_mut()).is_none());
    assert!(futures_util::FutureExt::now_or_never(monitor_notified.as_mut()).is_none());

    ports
        .release(&[crate::managed_host_tools::WakeSignal::new("monitor-driver")])
        .unwrap();

    assert!(futures_util::FutureExt::now_or_never(event_notified.as_mut()).is_none());
    futures_util::FutureExt::now_or_never(monitor_notified.as_mut())
        .expect("monitor cancellation wakes the dedicated dispatcher");
}

#[tokio::test]
async fn complaint_submit_read_and_respond_share_the_notification_service() {
    let oracle = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(oracle["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
    let worker = oracle["agents"][1].clone();
    let lead = oracle["agents"][0].clone();
    let worker_for_submit = worker.clone();
    let submitted = store
        .transaction_with(move |tx| {
            crate::notification_dispatch::agent_complaint_action(
                tx,
                &worker_for_submit,
                &json!({"action":"submit","text":"Need help"}),
                "complaint-call",
                20.0,
            )
        })
        .await
        .unwrap();
    assert_eq!(submitted["recipient"], "lead");
    let complaint_id = submitted["id"].as_str().unwrap().to_owned();
    let lead_for_read = lead.clone();
    let complaint_id_for_read = complaint_id.clone();
    let read = store
        .transaction_with(move |tx| {
            crate::notification_dispatch::agent_complaint_action(
                tx,
                &lead_for_read,
                &json!({"action":"read","complaint_id":complaint_id_for_read}),
                "read-call",
                21.0,
            )
        })
        .await
        .unwrap();
    assert_eq!(read["complaints"][0]["readAt"], 21.0);
    let lead_for_response = lead.clone();
    let complaint_id_for_response = complaint_id.clone();
    let responded = store
        .transaction_with(move |tx| {
            crate::notification_dispatch::agent_complaint_action(
                tx,
                &lead_for_response,
                &json!({"action":"respond","complaint_id":complaint_id_for_response,"text":"Investigating","status":"in_progress"}),
                "response-call",
                22.0,
            )
        })
        .await
        .unwrap();
    assert_eq!(responded["responses"][0]["author"], lead["id"]);
    let reader = store.test_reader().unwrap();
    let response_event: i64 = reader
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE agent=?1 AND kind='complaint_response'",
            [worker["id"].as_str().unwrap()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(response_event, 1);
}

#[tokio::test]
async fn send_to_descendant_is_followup_input_while_parent_and_broadcast_are_chat() {
    let oracle = template();
    let child = oracle["agents"][1]["id"].as_str().unwrap();
    let lead = oracle["agents"][0]["id"].as_str().unwrap();

    let (_root, store, value) = send(0, child, "send-child").await;
    assert_eq!(value["status"], "queued");
    let reader = store.test_reader().unwrap();
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

    let (_root, store, value) = send(1, lead, "send-parent").await;
    let mut member_ids = [lead, child];
    member_ids.sort_unstable();
    assert_eq!(
        value["room"],
        format!("private:{}:{}", member_ids[0], member_ids[1])
    );
    let reader = store.test_reader().unwrap();
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

    let (_root, store, value) = send(1, "broadcast", "send-broadcast").await;
    assert!(value["room"].as_str().unwrap().starts_with("broadcast:"));
    let reader = store.test_reader().unwrap();
    let chat_count: i64 = reader
        .query_row(
            "SELECT count(*) FROM runtime_chat_messages WHERE id='send-broadcast'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(chat_count, 1);
}

#[tokio::test]
async fn send_preserves_explicit_next_turn_for_followup_and_chat() {
    use studio_runtime_schema::DeliveryMoment;
    let oracle = template();
    for (sender, target_index, event_prefix) in [(0, 1, ""), (1, 0, "chat:")] {
        let target = oracle["agents"][target_index]["id"].as_str().unwrap();
        let id = format!("explicit-next-{sender}");
        let (_root, store, result) = try_send_args(
            sender,
            json!({"agent_id":target,"text":"wait for the active turn","delivery":DeliveryMoment::NextTurn}),
            &id,
        )
        .await;
        result.unwrap();
        let event_id = if event_prefix.is_empty() {
            id
        } else {
            format!("{event_prefix}{id}:{target}")
        };
        let reader = store.test_reader().unwrap();
        let (kind, metadata): (String, String) = reader
            .query_row(
                "SELECT e.kind,m.record FROM runtime_events e JOIN runtime_event_meta m ON m.id=e.id WHERE e.id=?1 AND e.agent=?2",
                [event_id.as_str(), target],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            kind,
            if sender == 0 {
                "followup"
            } else {
                "agent_message"
            }
        );
        let metadata: Value = serde_json::from_str(&metadata).unwrap();
        assert_eq!(metadata["delivery"], json!(DeliveryMoment::NextTurn));
    }
}

/// 2026-10-20: a lead could not cancel the stuck command watches of its
/// finished reviewer child, so it could not archive the child either. An
/// ancestor cancels a descendant's watch; any other agent is refused by name.
#[tokio::test]
async fn an_ancestor_cancels_a_descendant_watch_and_others_are_refused_by_name() {
    let oracle = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(oracle["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
    let log_root = store.state_dir().join("monitor-logs");
    let watch = |id: &str, owner: &Value| {
        json!({"id":id,"agent":owner["id"],"epoch":owner["epoch"],"turnId":null,
            "requestId":format!("request-{id}"),"command":"gh run watch 1 > watch.log 2>&1",
            "cwd":directory.path().to_string_lossy(),"timeout_ms":3600000,
            "stallTimeoutSeconds":1800,"livenessCommand":"","activityAt":10.0,
            "activityGeneration":0,"stallWakeGeneration":-1,"wakeOn":"exit",
            "successExitCodes":[0],"outputPatterns":[],"outputEventSequence":0,
            "outputEventCount":0,"outputEventIds":[],"status":"running","created":10.0,
            "exitCode":null,"tail":"","bytes":0,"interactive":false,"ruleId":null,
            "log":log_root.join(format!("{id}.log")).to_string_lossy(),
            "operation":{"agent":owner["id"],"epoch":owner["epoch"],
                "accountKey":owner.get("accountKey").and_then(Value::as_str).unwrap_or("default"),
                "connectionId":"connection"},
            "cancelRequested":false})
    };
    let lead = oracle["agents"][0].clone();
    let child = oracle["agents"][1].clone();
    let watches = [watch("child-watch", &child), watch("lead-watch", &lead)];
    store
        .transaction(move |tx| {
            for record in watches {
                studio_runtime_store::Table::new(tx)
                    .put(&studio_runtime_store::MonitorRecord::from_value(record)?)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    async fn cancel_as(
        store: &RuntimeStore,
        caller: &Value,
        monitor: &str,
        request: &str,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        use crate::managed_host_tools::WatchMonitorPort;
        let context = CallbackContext {
            agent: caller["id"].as_str().unwrap().into(),
            account: "default".into(),
            connection: "connection".into(),
            generation: 7,
            epoch: caller["epoch"].as_u64().unwrap(),
            thread: caller["threadId"].as_str().unwrap().into(),
            turn: caller["turnId"].as_str().unwrap().into(),
        };
        let reader = store.test_reader().unwrap();
        let admission =
            crate::message_admission::PreparedAdmission::load(&reader, &context.agent).unwrap();
        drop(reader);
        let actor =
            crate::managed_host_tools::Actor::from_record(caller, &context.agent, context.epoch)
                .unwrap();
        let ports = ProductionPorts::new(ProductionPortsConfig {
            context,
            room_directory: None,
            admission,
            event_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            monitor_dispatch_wake: std::sync::Arc::new(tokio::sync::Notify::new()),
            runtime_ports: empty_runtime_ports(),
            state_root: store.state_dir().to_path_buf(),
            preparation: None,
        });
        let args = json!({"action":"cancel","monitor_id":monitor});
        let request = request.to_owned();
        store
            .transaction_with(move |tx| ports.monitor(tx, &actor, &args, &request, 20.0))
            .await
    }

    let cancelled = cancel_as(&store, &lead, "child-watch", "lead-cancels-child")
        .await
        .unwrap();
    assert_eq!(cancelled.value["cancelRequested"], true);
    let refused = cancel_as(&store, &child, "lead-watch", "child-cancels-lead").await;
    assert!(
        matches!(
            refused,
            Err(crate::managed_host_tools::Error::Invalid(
                "This command belongs to another agent"
            ))
        ),
        "{refused:?}"
    );
}

/// 2026-10-20: a lead's `orchestration_send` with the short ID `d0c9c794` of
/// its child got "Message ingress refused this request". A unique visible
/// prefix of at least eight characters names that agent; any other ID is
/// refused with its reason.
#[tokio::test]
async fn send_resolves_a_unique_visible_prefix_and_names_a_refused_id() {
    let oracle = template();
    let child = oracle["agents"][1]["id"].as_str().unwrap();

    let (_root, store, value) = send(0, &child[..8], "send-child-prefix").await;
    assert_eq!(value["status"], "queued");
    let agent: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT agent FROM runtime_events WHERE id='send-child-prefix'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(agent, child);

    let (_root, _store, refused) = try_send(0, "0000000f", "send-unknown-prefix").await;
    assert!(
        matches!(
            refused,
            Err(crate::managed_host_tools::Error::Invalid(
                "Unknown agent ID in this caller's scope"
            ))
        ),
        "{refused:?}"
    );
    let (_root, _store, refused) = try_send(0, &child[..7], "send-short-prefix").await;
    assert!(
        matches!(
            refused,
            Err(crate::managed_host_tools::Error::Invalid(
                "Too short agent ID; use at least 8 characters"
            ))
        ),
        "{refused:?}"
    );
}

/// Python `codex_radio.guard_message`: a radio member's private message is
/// refused only while its own shared reply turn is in flight.
#[tokio::test]
async fn radio_member_message_is_refused_only_during_its_shared_reply_turn() {
    let oracle = template();
    let directory = tempfile::tempdir().unwrap();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(
            PathBuf::from(oracle["template"].as_str().unwrap()).join(name),
            directory.path().join(name),
        )
        .unwrap();
    }
    let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
    let lead = oracle["agents"][0]["id"].as_str().unwrap().to_owned();
    let worker = oracle["agents"][1].clone();
    let worker_id = worker["id"].as_str().unwrap().to_owned();
    let turn = worker["turnId"].as_str().unwrap().to_owned();
    let epoch = worker["epoch"].as_i64().unwrap();
    let room = json!({"id":"radio:guard","kind":"private","members":[lead,worker_id],
        "projectPath":"/fixture","customName":"Guard","created":10.0,"userHidden":false,
        "radio":{"direct":true,"teamId":"guard","revision":1,"status":"speaking",
            "speaker":worker_id,"next":[],"error":null,"seen":{},
            "active":{"eventId":"radio-turn:guard","agentId":worker_id,"epoch":epoch,
                "threadId":worker["threadId"],"identity":{},"through":0}}});
    let worker_for_setup = worker_id.clone();
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            tx.execute(
                "INSERT INTO runtime_rooms(id,record) VALUES('radio:guard',?1)",
                [room.to_string()],
            )?;
            tx.execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES('radio-turn:guard',?1,'radio_turn','{}','delivered',10.0,?2,?3,NULL)",
                params![worker_for_setup, epoch, turn],
            )?;
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.inFlight',json('true')) WHERE id=?1",
                [&worker_for_setup],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let send = |id: &'static str| {
        let lead = lead.clone();
        let worker = worker.clone();
        let store = &store;
        async move {
            let context = CallbackContext {
                agent: worker["id"].as_str().unwrap().into(),
                account: "default".into(),
                connection: "connection".into(),
                generation: 7,
                epoch: worker["epoch"].as_u64().unwrap(),
                thread: worker["threadId"].as_str().unwrap().into(),
                turn: worker["turnId"].as_str().unwrap().into(),
            };
            store
                .transaction_with(move |tx| {
                    let snapshot = super::super::super::storage::snapshot(tx, &context)?;
                    messages::send(
                        tx,
                        &snapshot,
                        &json!({"target":lead,"text":"private note"}),
                        id,
                        20.0,
                    )
                })
                .await
        }
    };
    let refused = send("radio-guard-live").await;
    assert!(
        matches!(
            refused,
            Err(CallbackError::Invalid(
                "Your reply is shared automatically. Reply in this turn and let Studio pass the floor."
            ))
        ),
        "{refused:?}"
    );
    let worker_for_finish = worker_id.clone();
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.inFlight',json('false')) WHERE id=?1",
                [&worker_for_finish],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let delivered = send("radio-guard-idle")
        .await
        .expect("a radio member may message privately outside its shared reply turn");
    assert_eq!(delivered["id"], "radio-guard-idle");
    let chat_count: i64 = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runtime_chat_messages WHERE id IN ('radio-guard-live','radio-guard-idle')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(chat_count, 1);
}

fn empty_runtime_ports() -> crate::provider_callbacks::tools::ManagedToolRuntimePorts {
    crate::provider_callbacks::tools::ManagedToolRuntimePorts {
        status_watch: None,
        child_creation: None,
        monitor_create: None,
        interrupt: None,
        monitor_input: None,
        monitor_cancel: None,
        work_action: None,
        late: Default::default(),
    }
}
