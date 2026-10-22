#![allow(clippy::unwrap_used)]
#![allow(clippy::duplicate_mod)]

use super::*;
use crate::managed_host_tools::{PostCommitPort, SendPort};
use std::{path::PathBuf, process::Command, sync::OnceLock};
use studio_runtime_store::RuntimeStore;

#[path = "../../../../tests/support/prepared_python.rs"]
mod prepared_python;

#[allow(clippy::disallowed_methods)]
fn template() -> (tempfile::TempDir, Value) {
    static TEMPLATE: OnceLock<(prepared_python::StateTemplate, Value)> = OnceLock::new();
    let (files, value) = TEMPLATE.get_or_init(|| {
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
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        (
            prepared_python::StateTemplate::capture(&PathBuf::from(
                value["template"].as_str().unwrap(),
            )),
            value,
        )
    });
    let directory = tempfile::tempdir().unwrap();
    files.write(directory.path());
    let mut value = value.clone();
    value["template"] = json!(directory.path());
    (directory, value)
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
    try_send_with(sender_index, target, id, Vec::new()).await
}

async fn try_send_with(
    sender_index: usize,
    target: &str,
    id: &str,
    extra_agents: Vec<Value>,
) -> (
    tempfile::TempDir,
    RuntimeStore,
    Result<Value, crate::managed_host_tools::Error>,
) {
    try_send_args_with(
        sender_index,
        json!({"agent_id":target,"text":"dispatch input"}),
        id,
        extra_agents,
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
    try_send_args_with(sender_index, args, id, Vec::new()).await
}

async fn try_send_args_with(
    sender_index: usize,
    args: Value,
    id: &str,
    extra_agents: Vec<Value>,
) -> (
    tempfile::TempDir,
    RuntimeStore,
    Result<Value, crate::managed_host_tools::Error>,
) {
    let (directory, oracle) = template();
    let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
    store
        .transaction(move |tx| {
            for record in extra_agents {
                studio_runtime_store::AgentRepository::put(
                    tx,
                    &studio_runtime_store::AgentRecord::from_value(record)?,
                )?;
            }
            Ok(())
        })
        .await
        .unwrap();
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
    let admission = crate::message_admission::PreparedAdmission::read(&reader, &context.agent)
        .expect("SQL admission inputs")
        .prepare()
        .await
        .unwrap();
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
        panel_guidance: None,
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
    let (directory, oracle) = template();
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
    let admission = crate::message_admission::PreparedAdmission::read(&reader, &context.agent)
        .expect("SQL admission inputs")
        .prepare()
        .await
        .unwrap();
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
        panel_guidance: None,
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
    let (directory, oracle) = template();
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
    let admission = crate::message_admission::PreparedAdmission::read(&reader, &context.agent)
        .expect("SQL admission inputs")
        .prepare()
        .await
        .unwrap();
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
        panel_guidance: None,
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
    let (directory, oracle) = template();
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
    let (_template_directory, oracle) = template();
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
    let (_template_directory, oracle) = template();
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
    let (directory, oracle) = template();
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
        let admission = crate::message_admission::PreparedAdmission::read(&reader, &context.agent)
            .expect("SQL admission inputs")
            .prepare()
            .await
            .unwrap();
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
            panel_guidance: None,
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
    let (_template_directory, oracle) = template();
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

/// 2026-10-21: three agents of another team got a cwd with a trailing slash,
/// and every `orchestration_send` got "Message ingress refused this request".
/// The visibility scan checked each agent of another root, and one refused
/// record failed the whole call. A trailing slash is the same path identity.
/// A record that the gate cannot check (here a relative cwd) stays hidden and
/// does not refuse sends to other agents.
#[tokio::test]
async fn an_unusual_record_of_another_team_does_not_refuse_sends() {
    let (_template_directory, oracle) = template();
    let lead = &oracle["agents"][0];
    let child = oracle["agents"][1]["id"].as_str().unwrap();
    let foreign = |id: &str, cwd: &str| {
        let mut record = lead.clone();
        record["id"] = json!(id);
        record["rootId"] = json!("99999999-root-of-another-team");
        record["parentId"] = json!("99999999-root-of-another-team");
        record["isLead"] = json!(false);
        record["threadId"] = json!(format!("thread-{id}"));
        record["cwd"] = json!(cwd);
        record
    };
    let trailing = format!("{}/", lead["cwd"].as_str().unwrap());
    let others = || {
        vec![
            foreign("aaaaaaaa-trailing-slash", &trailing),
            foreign("bbbbbbbb-relative-cwd", "relative/cwd"),
        ]
    };
    for (target, id) in [
        (child, "send-child-full"),
        (&child[..8], "send-child-prefix"),
    ] {
        let (_root, _store, sent) = try_send_with(0, target, id, others()).await;
        assert_eq!(sent.unwrap()["status"], "queued", "{target}");
    }
    let (_root, store, refused) =
        try_send_with(0, "bbbbbbbb-relative-cwd", "send-hidden", others()).await;
    let mut rows = store
        .transaction(|tx| Ok(messages::authorization_records(tx).unwrap()))
        .await
        .unwrap();
    assert_eq!(
        rows.resolved_paths[&trailing],
        lead["cwd"].as_str().unwrap()
    );
    rows.recipient_id = "aaaaaaaa-trailing-slash".into();
    crate::submit_gates::AuthorizationSnapshot::from_records(&rows)
        .expect("a trailing slash is the same path identity");
    assert!(
        matches!(
            refused,
            Err(crate::managed_host_tools::Error::Invalid(
                "Unknown agent ID in this caller's scope"
            ))
        ),
        "{refused:?}"
    );
}

/// A broadcast checks each team member. One member record that the gate
/// cannot check (here a relative cwd) must not refuse the whole broadcast:
/// every other member gets the message, and the result names the refused
/// member with its stored-record location. The saved receipt holds the same
/// result, so a replay of the request reports the same members.
#[tokio::test]
async fn a_broadcast_names_a_member_that_cannot_be_checked_and_reaches_the_others() {
    let (_template_directory, oracle) = template();
    let lead = &oracle["agents"][0];
    let lead_id = lead["id"].as_str().unwrap();
    let child = oracle["agents"][1]["id"].as_str().unwrap();
    let bad = "cccccccc-member-relative-cwd";
    let mut member = lead.clone();
    member["id"] = json!(bad);
    member["parentId"] = json!(lead_id);
    member["isLead"] = json!(false);
    member["threadId"] = json!(format!("thread-{bad}"));
    member["cwd"] = json!("relative/cwd");
    let (_root, store, sent) =
        try_send_with(0, "broadcast", "send-broadcast-bad-member", vec![member]).await;
    let sent = sent.expect("one bad member does not refuse the broadcast");
    assert!(sent["deliveries"].get(child).is_some(), "{sent}");
    assert!(sent["deliveries"].get(bad).is_none(), "{sent}");
    assert_eq!(
        sent["refused"][bad],
        json!(format!("Invalid stored record at agents/{bad}/cwd")),
        "{sent}"
    );
    let reader = store.test_reader().unwrap();
    let (deliveries, receipt): (String, String) = reader
        .query_row(
            "SELECT m.deliveries,r.result FROM runtime_chat_messages m JOIN runtime_operation_receipts r ON r.id=m.id WHERE m.id='send-broadcast-bad-member'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&deliveries).unwrap(),
        sent["deliveries"]
    );
    assert_eq!(serde_json::from_str::<Value>(&receipt).unwrap(), sent);
    let bad_events: i64 = reader
        .query_row(
            "SELECT count(*) FROM runtime_events WHERE agent=?1",
            [bad],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(bad_events, 0);
}

/// When no member of a broadcast can be checked, nothing is sent, and the
/// refusal names the stored-record location.
#[tokio::test]
async fn a_broadcast_with_no_member_that_can_be_checked_is_refused_by_location() {
    let (_template_directory, oracle) = template();
    let lead = oracle["agents"][0]["id"].as_str().unwrap();
    let template = PathBuf::from(oracle["template"].as_str().unwrap()).join("canvas.sqlite3");
    let connection =
        rusqlite::Connection::open_with_flags(template, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let mut statement = connection
        .prepare("SELECT record FROM runtime_agents WHERE json_extract(record,'$.rootId')=?1 AND id!=?1 ORDER BY id")
        .unwrap();
    let broken = statement
        .query_map([lead], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|raw| {
            let mut record: Value = serde_json::from_str(&raw.unwrap()).unwrap();
            record["cwd"] = json!("relative/cwd");
            record
        })
        .collect::<Vec<_>>();
    let members = broken
        .iter()
        .filter(|record| !studio_runtime_store::truthy(record.get("deletedAt")))
        .map(|record| {
            format!(
                "Invalid stored record at agents/{}/cwd",
                record["id"].as_str().unwrap()
            )
        })
        .collect::<Vec<_>>();
    assert!(!members.is_empty(), "the fixture team has a member");
    let (_root, store, sent) =
        try_send_with(0, "broadcast", "send-broadcast-no-member", broken).await;
    let refusal = sent.expect_err("no member can be checked").to_string();
    assert!(members.contains(&refusal), "{refusal}");
    let messages: i64 = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT count(*) FROM runtime_chat_messages WHERE id='send-broadcast-no-member'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(messages, 0);
}

/// A stored-record refusal names its location instead of a generic text.
#[test]
fn a_stored_refusal_names_its_location() {
    assert_eq!(
        callback_refusal(CallbackError::Stored("agents/a/cwd".into())).to_string(),
        "Invalid stored record at agents/a/cwd"
    );
}

/// Python `codex_radio.guard_message`: a radio member's private message is
/// refused only while its own shared reply turn is in flight.
#[tokio::test]
async fn radio_member_message_is_refused_only_during_its_shared_reply_turn() {
    let (directory, oracle) = template();
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

#[derive(Clone, Copy)]
enum BlobRace {
    Unchanged,
    ActorEpoch,
    ResultReference,
    LateResult,
    Abandoned,
    RecoveryWrite,
    InlineExpiry,
    InvalidAction,
    Missing,
    Corrupt,
    #[cfg(unix)]
    Symlink,
}

async fn callback_blob_case(race: BlobRace) {
    use crate::payload_blobs::{TestIoGate, TestIoOperation};
    use crate::provider_callbacks::{CallbackDecision, dispatch};
    use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
    const HANG_GUARD: std::time::Duration = std::time::Duration::from_secs(10);
    const LARGE_RESULT_BYTES: usize = 128 * 1024;
    let (directory, oracle) = template();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let actor = &oracle["agents"][0];
    let context = CallbackContext {
        agent: actor["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: 3,
        thread: "thread-0".into(),
        turn: "turn-0".into(),
    };
    let frame = |id: &str, tool: &str, args: Value| {
        Frame::Request(RequestFrame {
            id: RequestId::String(id.into()),
            method: Method::ItemToolCall,
            params: Some(
                json!({"threadId":context.thread,"turnId":context.turn,"callId":id,"tool":tool,"arguments":args}),
            ),
            fields: Default::default(),
        })
    };
    dispatch(
        Arc::clone(&store),
        &context,
        &frame("saved", "orchestration_request", json!({"action":"list"})),
        10.0,
    )
    .await
    .unwrap();
    if matches!(race, BlobRace::InvalidAction) {
        let reply = dispatch(
            Arc::clone(&store),
            &context,
            &frame(
                "invalid",
                "orchestration_request",
                json!({"action":"invalid"}),
            ),
            11.0,
        )
        .await;
        let Ok(CallbackDecision::Answer(reply)) = reply else {
            panic!("invalid arguments must retain a tool refusal: {reply:?}")
        };
        let studio_runtime_providers::ResponseBody::Result(body) = reply.body else {
            panic!("tool refusal body")
        };
        assert_eq!(body["success"], false);
        return;
    }
    let mut result = json!({"success":true,"contentItems":[{"type":"inputText","text":"x".repeat(LARGE_RESULT_BYTES)}]});
    if matches!(race, BlobRace::InlineExpiry) {
        result["serverOutputExpiresAt"] = json!(10.0);
    }
    let decision = crate::payload_blobs::decide_externalize_result(&result).unwrap();
    crate::payload_blobs::write_blobs(directory.path(), &decision.writes).unwrap();
    let hash = &decision.writes[0].sha256;
    let blob_path = directory
        .path()
        .join("blobs")
        .join(&hash[..2])
        .join(&hash[2..4])
        .join(hash);
    let recover = !matches!(
        race,
        BlobRace::Unchanged
            | BlobRace::ActorEpoch
            | BlobRace::ResultReference
            | BlobRace::Abandoned
            | BlobRace::InlineExpiry
    );
    let inline_result = matches!(race, BlobRace::InlineExpiry).then(|| result.to_string());
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_tool_results SET result=?1 WHERE id='thread-0:saved'", [decision.record.to_string()])?;
        if let Some(result) = inline_result { tx.execute("UPDATE runtime_tool_requests SET record=json_set(record,'$.result',json(?1)) WHERE id='thread-0:saved'", [result])?; }
        if recover {
            tx.execute("UPDATE runtime_tool_requests SET record=json_remove(json_set(record,'$.stage','running','$.outcome','pending'),'$.result','$.finished') WHERE id='thread-0:saved'", [])?;
        }
        Ok(())
    }).await.unwrap();
    if matches!(race, BlobRace::InlineExpiry) {
        let reply = dispatch(
            Arc::clone(&store),
            &context,
            &frame(
                "read",
                "orchestration_request",
                json!({"action":"get","request_id":"thread-0:saved"}),
            ),
            11.0,
        )
        .await
        .unwrap();
        let CallbackDecision::Answer(reply) = reply else {
            panic!("request answer")
        };
        let studio_runtime_providers::ResponseBody::Result(body) = reply.body else {
            panic!("request body")
        };
        let record: Value =
            serde_json::from_str(body["contentItems"][0]["text"].as_str().unwrap()).unwrap();
        assert!(
            record["result"]["contentItems"][0]["text"]
                .as_str()
                .unwrap()
                .contains("expired"),
            "{record}"
        );
        return;
    }
    match race {
        BlobRace::Missing => std::fs::remove_file(&blob_path).unwrap(),
        BlobRace::Corrupt => std::fs::write(&blob_path, b"invalid blob").unwrap(),
        #[cfg(unix)]
        BlobRace::Symlink => {
            let original = directory.path().join("blob-original");
            std::fs::rename(&blob_path, &original).unwrap();
            std::os::unix::fs::symlink(original, &blob_path).unwrap();
        }
        _ => {}
    }
    let fault = matches!(race, BlobRace::Missing | BlobRace::Corrupt);
    #[cfg(unix)]
    let fault = fault || matches!(race, BlobRace::Symlink);
    if fault {
        let reply = dispatch(
            Arc::clone(&store),
            &context,
            &frame(
                "read",
                "orchestration_read",
                json!({"output_ref":"thread-0:saved"}),
            ),
            11.0,
        )
        .await;
        assert!(matches!(reply, Err(CallbackError::Stored(_))), "{reply:?}");
        let row = records::get(
            &store.test_reader().unwrap(),
            "runtime_tool_requests",
            "thread-0:saved",
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            row["stage"], "running",
            "bad payload must not settle recovery"
        );
        return;
    }
    let alternate = if matches!(race, BlobRace::ResultReference) {
        let result = json!({"success":true,"contentItems":[{"type":"inputText","text":"y".repeat(LARGE_RESULT_BYTES)}]});
        let decision = crate::payload_blobs::decide_externalize_result(&result).unwrap();
        crate::payload_blobs::write_blobs(directory.path(), &decision.writes).unwrap();
        Some(decision.record)
    } else {
        None
    };
    let (entered, arrival) = tokio::sync::oneshot::channel();
    let (release, blocked) = std::sync::mpsc::channel();
    let operation = if matches!(race, BlobRace::RecoveryWrite) {
        TestIoOperation::Write
    } else {
        TestIoOperation::Read
    };
    crate::payload_blobs::test_io_gates()
        .lock()
        .unwrap()
        .insert(
            (directory.path().to_path_buf(), operation),
            TestIoGate {
                entered,
                release: blocked,
            },
        );
    let read = if matches!(race, BlobRace::RecoveryWrite) {
        frame(
            "read",
            "orchestration_request",
            json!({"action":"get","request_id":"thread-0:saved"}),
        )
    } else {
        frame(
            "read",
            "orchestration_read",
            json!({"output_ref":"thread-0:saved"}),
        )
    };
    let read_store = Arc::clone(&store);
    let read_task = tokio::spawn(async move { dispatch(read_store, &context, &read, 11.0).await });
    tokio::time::timeout(HANG_GUARD, arrival)
        .await
        .unwrap()
        .unwrap();
    let writer_store = Arc::clone(&store);
    let mut writer = tokio::spawn(async move {
        writer_store.transaction(move |tx| {
        match race {
            BlobRace::ActorEpoch => { tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',4,'$.turnEpoch',4) WHERE json_extract(record,'$.threadId')='thread-0'", [])?; }
            BlobRace::ResultReference => { tx.execute("UPDATE runtime_tool_results SET result=?1 WHERE id='thread-0:saved'", [alternate.unwrap().to_string()])?; }
            BlobRace::LateResult => { tx.execute("UPDATE runtime_tool_results SET result=?1 WHERE id='thread-0:saved'", [json!({"success":true,"contentItems":[{"type":"inputText","text":"late saved result"}]}).to_string()])?; }
            _ => { tx.execute("UPDATE runtime_tool_requests SET record=record WHERE id='thread-0:saved'", [])?; }
        }
        Ok(())
    }).await
    });
    let completed = tokio::time::timeout(HANG_GUARD, &mut writer).await;
    if matches!(race, BlobRace::Abandoned) {
        read_task.abort();
    }
    release.send(()).unwrap();
    assert!(
        completed.is_ok(),
        "a gated blob read held the Canvas writer"
    );
    completed.unwrap().unwrap().unwrap();
    if matches!(race, BlobRace::Abandoned) {
        assert!(read_task.await.unwrap_err().is_cancelled());
    } else {
        let reply = read_task.await.unwrap();
        match race {
            BlobRace::ActorEpoch | BlobRace::ResultReference | BlobRace::LateResult => {
                assert!(matches!(reply, Err(CallbackError::Stale)), "{reply:?}")
            }
            _ => assert!(
                matches!(reply, Ok(CallbackDecision::Answer(_))),
                "{reply:?}"
            ),
        }
    }
    let reader = store.test_reader().unwrap();
    if matches!(race, BlobRace::RecoveryWrite) {
        let raw = records::get(&reader, "runtime_tool_requests", "thread-0:saved")
            .unwrap()
            .unwrap();
        assert_eq!(raw["stage"], "completed");
        assert!(raw.get("_payloadBlobs").is_some());
        let resolved = crate::payload_blobs::resolve_record(directory.path(), &raw).unwrap();
        assert_eq!(
            resolved["result"], result,
            "referenced payload was durable at commit"
        );
    }
    if matches!(race, BlobRace::LateResult) {
        let raw = records::get(&reader, "runtime_tool_requests", "thread-0:saved")
            .unwrap()
            .unwrap();
        assert_eq!(
            raw["stage"], "running",
            "stale preparation must not settle the target receipt"
        );
        assert_eq!(
            records::get(&reader, "runtime_tool_results", "thread-0:saved")
                .unwrap()
                .unwrap()["contentItems"][0]["text"],
            "late saved result"
        );
    }
}

#[tokio::test]
async fn callback_blob_read_allows_an_unrelated_writer() {
    callback_blob_case(BlobRace::Unchanged).await;
}
#[tokio::test]
async fn callback_blob_actor_epoch_change_refuses_settlement() {
    callback_blob_case(BlobRace::ActorEpoch).await;
}
#[tokio::test]
async fn callback_blob_changed_result_refuses_settlement() {
    callback_blob_case(BlobRace::ResultReference).await;
}
#[tokio::test]
async fn callback_blob_late_result_refuses_stale_recovery() {
    callback_blob_case(BlobRace::LateResult).await;
}
#[tokio::test]
async fn callback_blob_abandoned_response_does_not_replay() {
    callback_blob_case(BlobRace::Abandoned).await;
}
#[tokio::test]
async fn callback_blob_recovery_write_precedes_referencing_commit() {
    callback_blob_case(BlobRace::RecoveryWrite).await;
}

#[tokio::test]
async fn callback_blob_missing_file_does_not_settle_recovery() {
    callback_blob_case(BlobRace::Missing).await;
}
#[tokio::test]
async fn callback_blob_corrupt_file_does_not_settle_recovery() {
    callback_blob_case(BlobRace::Corrupt).await;
}
#[cfg(unix)]
#[tokio::test]
async fn callback_blob_symlink_does_not_settle_recovery() {
    callback_blob_case(BlobRace::Symlink).await;
}

#[tokio::test]
async fn callback_panel_uses_one_file_snapshot_per_callback() {
    use crate::provider_callbacks::{
        CallbackDecision, ManagedToolPortsConfig, dispatch_with_managed_tools,
    };
    use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};
    let (directory, oracle) = template();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let context = CallbackContext {
        agent: oracle["agents"][0]["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: 3,
        thread: "thread-0".into(),
        turn: "turn-0".into(),
    };
    let mut resources = crate::account_worker::preparation::PreparationResources::bundled(
        String::new(),
        crate::model_catalog::ModelCatalogService::new(),
    )
    .unwrap();
    resources.skills_root = directory.path().join("skills");
    let guide = resources.skills_root.join(context::PANEL_GUIDANCE);
    std::fs::create_dir_all(guide.parent().unwrap()).unwrap();
    let resources = Arc::new(resources);
    for content in ["first panel snapshot", "second panel snapshot"] {
        std::fs::write(&guide, content).unwrap();
        let frame = Frame::Request(RequestFrame {
            id: RequestId::String(content.into()),
            method: Method::ItemToolCall,
            params: Some(
                json!({"threadId":context.thread,"turnId":context.turn,"callId":content,"tool":"orchestration_context","arguments":{"topic":"panel"}}),
            ),
            fields: Default::default(),
        });
        let input = Input::from_frame(&frame).unwrap().unwrap();
        let ports = crate::provider_callbacks::tools::production_ports(
            &store,
            &context,
            &input,
            ManagedToolPortsConfig {
                wake: Arc::new(tokio::sync::Notify::new()),
                monitor_dispatch_wake: Arc::new(tokio::sync::Notify::new()),
                runtime_ports: empty_runtime_ports(),
                preparation: Some(Arc::clone(&resources)),
            },
        )
        .await
        .unwrap();
        std::fs::remove_file(&guide).unwrap();
        let reply = dispatch_with_managed_tools(
            Arc::clone(&store),
            &context,
            &frame,
            11.0,
            Arc::new(ports),
        )
        .await
        .unwrap();
        let CallbackDecision::Answer(reply) = reply else {
            panic!("panel reply")
        };
        let ResponseBody::Result(body) = reply.body else {
            panic!("panel result")
        };
        assert_eq!(body["success"], true, "{body}");
        assert!(body.to_string().contains(content), "{body}");
    }
}

#[tokio::test]
async fn callback_request_expires_a_terminal_inline_result() {
    callback_blob_case(BlobRace::InlineExpiry).await;
}

#[tokio::test]
async fn callback_request_invalid_action_preserves_tool_refusal() {
    callback_blob_case(BlobRace::InvalidAction).await;
}
