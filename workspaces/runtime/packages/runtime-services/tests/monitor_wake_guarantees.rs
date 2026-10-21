//! Every decided command outcome has one durable owner wake, across restarts.
#[path = "monitors/fixtures.rs"]
mod fixtures;
use serde_json::{Value, json};
use studio_runtime_services::{
    monitor_policy::{Operation, ReattachProof},
    monitor_runtime,
};
use studio_runtime_store::{AgentRepository, AgentStatus, MonitorsRepository, RuntimeStore};

async fn fixture() -> (tempfile::TempDir, RuntimeStore, String, Operation) {
    let root = fixtures::tempdir();
    let mut case = fixtures::corpus(root.path()).remove(0);
    case["actor"]["status"] = json!("waiting_on_monitor");
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    let operation = serde_json::from_value(case["before"]["operation"].clone()).expect("operation");
    let store = fixtures::store(root.path(), "wake").await;
    fixtures::seed(&store, &case).await;
    (root, store, id, operation)
}

async fn finish(
    store: &RuntimeStore,
    id: &str,
    operation: &Operation,
    code: Option<i64>,
    error: Option<&str>,
) {
    let (id, operation, error) = (id.to_owned(), operation.clone(), error.map(str::to_owned));
    store
        .transaction_with(move |tx| {
            monitor_runtime::finish_in_transaction(
                tx,
                &id,
                Some(&operation),
                code,
                error.as_deref(),
                2002.0,
            )
        })
        .await
        .expect("terminal result commits");
}

fn assert_wake(store: &RuntimeStore, expected: &str) {
    let events = fixtures::events(store);
    let events = events.as_array().expect("events");
    assert_eq!(events.len(), 1, "exactly one terminal identity");
    assert_eq!(events[0]["kind"], "monitor_exit");
    assert_eq!(events[0]["status"], "pending");
    assert_eq!(events[0]["text"]["status"], expected);
    assert_eq!(
        AgentRepository::get(&store.test_reader().expect("reader"), &"agent".into())
            .expect("read agent")
            .expect("agent")
            .status,
        AgentStatus::Queued
    );
}

#[tokio::test]
async fn success_failure_signal_and_cancel_each_queue_one_owner_wake() {
    for (code, error, cancel, expected) in [
        (Some(0), None, false, "completed"),
        (Some(1), None, false, "failed"),
        (Some(-9), None, false, "failed"),
        (None, Some("native process lost"), false, "failed"),
        (Some(-15), None, true, "cancelled"),
    ] {
        let (_root, store, id, operation) = fixture().await;
        if cancel {
            monitor_runtime::cancel(&store, &id, Some("agent"), 2001.0)
                .await
                .expect("cancel");
            assert_eq!(fixtures::events(&store), json!([]));
        }
        finish(&store, &id, &operation, code, error).await;
        assert_wake(&store, expected);
        finish(&store, &id, &operation, code, error).await;
        assert_wake(&store, expected);
        // A replay cannot requeue an already delivered completion.
        store
            .transaction(|tx| {
                tx.execute("UPDATE runtime_events SET status='delivered'", [])?;
                Ok(())
            })
            .await
            .expect("deliver");
        finish(&store, &id, &operation, code, error).await;
        assert_eq!(fixtures::events(&store)[0]["status"], "delivered");
    }
}

#[tokio::test]
async fn restart_surviving_command_wakes_only_on_its_real_exit() {
    let (root, store, id, operation) = fixture().await;
    drop(store);
    let store = RuntimeStore::open(Some(&root.path().join("wake")))
        .await
        .expect("reopen backend state");
    monitor_runtime::recover_on_startup(&store, 2001.0)
        .await
        .expect("restart");
    assert_eq!(fixtures::events(&store), json!([]));
    assert_eq!(
        MonitorsRepository::get(&store.test_reader().expect("reader"), &id)
            .expect("read monitor")
            .expect("monitor")
            .value()["status"],
        "running"
    );
    monitor_runtime::reattach(
        &store,
        &id,
        &ReattachProof {
            operation: operation.clone(),
            operation_id: format!("monitor:{id}"),
            native_id: 1,
            current_connection: "new-connection".into(),
        },
        2001.5,
    )
    .await
    .expect("reattach retained process");
    assert_eq!(fixtures::events(&store), json!([]));
    finish(&store, &id, &operation, Some(0), None).await;
    assert_wake(&store, "completed");
    monitor_runtime::recover_on_startup(&store, 2003.0)
        .await
        .expect("repeat restart");
    assert_wake(&store, "completed");
}

#[tokio::test]
async fn restart_dead_or_unreachable_command_wakes_once_as_lost() {
    for reason in [
        "provider_rejected",
        "no_operation",
        "reattach_window_expired",
    ] {
        let (root, store, id, operation) = fixture().await;
        drop(store);
        let store = RuntimeStore::open(Some(&root.path().join("wake")))
            .await
            .expect("reopen");
        monitor_runtime::recover_on_startup(&store, 2001.0)
            .await
            .expect("restart");
        monitor_runtime::recover_on_startup(&store, 2001.5)
            .await
            .expect("duplicate restart");
        assert_eq!(fixtures::events(&store), json!([]));
        monitor_runtime::settle_reattach(&store, &id, reason, 2002.0)
            .await
            .expect("decide loss");
        assert_wake(&store, "lost");
        monitor_runtime::settle_reattach(&store, &id, reason, 2003.0)
            .await
            .expect("replay");
        assert_wake(&store, "lost");
        finish(&store, &id, &operation, Some(0), None).await;
        assert_wake(&store, "lost");
        let record = MonitorsRepository::get(&store.test_reader().expect("reader"), &id)
            .expect("read")
            .expect("monitor");
        assert_eq!(
            record.value()["status"],
            "lost",
            "decided loss is terminal even if a late receipt arrives"
        );
    }
}

#[tokio::test]
async fn completion_does_not_override_stops_deletion_or_new_epochs() {
    for patch in [
        json!({"autoWake":false,"status":"paused"}),
        json!({"deletedAt":2001}),
        json!({"epoch":3}),
    ] {
        let (_root, store, id, operation) = fixture().await;
        store
            .transaction(move |tx| {
                let mut agent = AgentRepository::get(tx, &"agent".into())?
                    .expect("agent")
                    .value();
                for (key, value) in patch.as_object().expect("patch") {
                    agent[key] = value.clone();
                }
                AgentRepository::put(tx, &studio_runtime_store::AgentRecord::from_value(agent)?)
            })
            .await
            .expect("stop or replace owner");
        finish(&store, &id, &operation, Some(0), None).await;
        assert_eq!(fixtures::events(&store)[0]["status"], "cancelled");
    }
}

#[test]
fn agent_status_consumers_match_the_runtime_store_owner() {
    use std::collections::BTreeSet;
    let owned: BTreeSet<_> = AgentStatus::VALUES.iter().copied().collect();
    let api: Value =
        serde_json::from_str(include_str!("../../runtime-http/openapi.json")).expect("OpenAPI");
    fn check_enums(value: &Value, owned: &BTreeSet<&str>, count: &mut usize) {
        match value {
            Value::Object(object) => {
                if let Some(values) = object.get("enum").and_then(Value::as_array)
                    && values.iter().any(|value| value == "capacity-retry")
                {
                    let consumer = values
                        .iter()
                        .map(|value| value.as_str().expect("status string"))
                        .collect();
                    assert_eq!(
                        *owned, consumer,
                        "AgentStatus owner: runtime-store/src/records.rs"
                    );
                    *count += 1;
                }
                for child in object.values() {
                    check_enums(child, owned, count);
                }
            }
            Value::Array(values) => {
                for child in values {
                    check_enums(child, owned, count);
                }
            }
            _ => (),
        }
    }
    let mut count = 0;
    check_enums(&api, &owned, &mut count);
    assert_eq!(
        count, 3,
        "check the shared schema and every inlined API status enum"
    );
    let python = include_str!("../../../apps/server/src/studio_api/sync/models.py");
    let class = python
        .split("class AgentStatus(ContractStrEnum):")
        .nth(1)
        .expect("Python consumer")
        .split("\n\nclass ")
        .next()
        .expect("class body");
    let consumer = class
        .lines()
        .filter_map(|line| {
            line.split_once(" = \"")
                .map(|(_, value)| value.trim_end_matches('"'))
        })
        .collect();
    assert_eq!(
        owned, consumer,
        "Python must consume runtime-store AgentStatus values"
    );
    let records = include_str!("../../../apps/server/src/codex_records.py");
    let literal = records
        .split("AgentStatusValue = Literal[")
        .nth(1)
        .expect("persisted status consumer")
        .split(']')
        .next()
        .expect("literal body");
    let consumer = literal
        .split('"')
        .enumerate()
        .filter_map(|(index, value)| (index % 2 == 1).then_some(value))
        .collect();
    assert_eq!(
        owned, consumer,
        "persisted Python records must consume runtime-store AgentStatus values"
    );
    let web = include_str!("../../../../client/apps/web/src/generated/api.ts");
    let consumers = web
        .split(';')
        .filter(|union| union.contains("| \"capacity-retry\""));
    let mut count = 0;
    for consumer in consumers {
        let consumer = consumer
            .lines()
            .filter_map(|line| {
                line.trim()
                    .strip_prefix("| \"")
                    .map(|line| line.trim_end_matches('"'))
            })
            .collect();
        assert_eq!(
            owned, consumer,
            "every generated web status enum must match its Rust owner"
        );
        count += 1;
    }
    assert_eq!(count, 3, "named enum and both inlined web consumers");
}

#[tokio::test]
async fn unstarted_cancel_and_start_refusal_each_wake_once() {
    for cancel in [true, false] {
        let (_root, store, id, _operation) = fixture().await;
        let selected = id.clone();
        store.transaction(move |tx| {
            tx.execute("UPDATE runtime_monitors SET record=json_remove(json_set(record,'$.status','starting'),'$.operation') WHERE id=?1", [selected])?;
            Ok(())
        }).await.expect("unstarted command");
        if cancel {
            monitor_runtime::cancel(&store, &id, Some("agent"), 2002.0)
                .await
                .expect("cancel before launch");
            monitor_runtime::cancel(&store, &id, Some("agent"), 2003.0)
                .await
                .expect("replay cancellation");
            assert_wake(&store, "cancelled");
        } else {
            for _ in 0..2 {
                let selected = id.clone();
                store
                    .transaction_with(move |tx| {
                        monitor_runtime::finish_in_transaction(
                            tx,
                            &selected,
                            None,
                            None,
                            Some("Preparation refused"),
                            2002.0,
                        )
                    })
                    .await
                    .expect("failed preparation");
            }
            assert_wake(&store, "failed");
        }
    }
}

#[tokio::test]
async fn restart_of_unsubmitted_start_or_approval_decides_loss_without_relaunch() {
    for status in ["starting", "approval"] {
        let (root, store, id, _operation) = fixture().await;
        let selected = id.clone();
        store.transaction(move |tx| {
            tx.execute("UPDATE runtime_monitors SET record=json_remove(json_set(record,'$.status',?1),'$.operation') WHERE id=?2", (status, selected))?;
            Ok(())
        }).await.expect("persist a start whose preparation was interrupted");
        drop(store);
        let store = RuntimeStore::open(Some(&root.path().join("wake")))
            .await
            .expect("restart backend");
        monitor_runtime::recover_on_startup(&store, 2001.0)
            .await
            .expect("stage recovery");
        assert_eq!(fixtures::events(&store), json!([]));
        assert!(
            studio_runtime_services::monitor_driver::pending_start_ids(&store)
                .await
                .expect("pending starts")
                .is_empty(),
            "restart never launches the command again"
        );
        monitor_runtime::settle_reattach(&store, &id, "no_operation", 2002.0)
            .await
            .expect("decide unsubmitted loss");
        assert_wake(&store, "lost");
        monitor_runtime::recover_on_startup(&store, 2003.0)
            .await
            .expect("repeat restart");
        assert_wake(&store, "lost");
    }
}

#[tokio::test]
async fn unknown_submission_enters_the_same_reconciliation_and_wakes_once() {
    use studio_runtime_services::{
        monitor_driver::{self, MonitorNative, Submission},
        monitor_policy::Effect,
    };
    struct UnknownPort;
    impl MonitorNative for UnknownPort {
        fn submit<'a>(
            &'a self,
            _: &'a Operation,
            _: &'a Effect,
        ) -> futures_util::future::BoxFuture<'a, Submission> {
            Box::pin(async { Submission::Unknown })
        }
    }
    let (_root, store, id, operation) = fixture().await;
    let selected = id.clone();
    store.transaction(move |tx| {
        tx.execute("UPDATE runtime_monitors SET record=json_remove(json_set(record,'$.status','starting'),'$.operation') WHERE id=?1", [selected])?;
        Ok(())
    }).await.expect("unsubmitted");
    let (snapshot, actor) = store
        .read_async("test/prepare", move |tx| monitor_runtime::snapshot(tx, &id))
        .await
        .expect("snapshot");
    let prepared = monitor_runtime::PreparedStart {
        monitor: snapshot.record().clone(),
        actor: actor.value(),
        operation,
        params: json!({"command":["true"],"processId":snapshot.id(), "cwd":snapshot.record()["cwd"], "timeoutMs":snapshot.record()["timeout_ms"], "sandboxPolicy":{"type":"readOnly"}}),
    };
    assert!(matches!(
        monitor_driver::start(&store, &UnknownPort, snapshot.id(), &prepared, 2001.0)
            .await
            .expect("unknown launch"),
        Submission::Unknown
    ));
    let monitor = MonitorsRepository::get(&store.test_reader().expect("reader"), snapshot.id())
        .expect("read")
        .expect("monitor");
    assert_eq!(monitor.value()["status"], "running");
    assert_eq!(monitor.value()["reattachRecovery"]["status"], "running");
    assert!(
        monitor.value()["reattachRecovery"]["startedAt"]
            .as_f64()
            .is_some()
    );
    assert_eq!(fixtures::events(&store), json!([]));
    monitor_runtime::settle_reattach(&store, snapshot.id(), "provider_rejected", 2002.0)
        .await
        .expect("decided lost");
    monitor_runtime::settle_reattach(&store, snapshot.id(), "provider_rejected", 2003.0)
        .await
        .expect("repeat");
    assert_wake(&store, "lost");
}
