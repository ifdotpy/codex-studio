//! Every decided command outcome has one durable owner wake, across restarts.
use crate::monitor_fixtures as fixtures;
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
            native_id: Some(1),
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
    let reason = "provider_rejected";
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
    assert_eq!(record.value()["reattachRecovery"]["proof"], reason);
    assert_eq!(
        record.value()["status"],
        "lost",
        "decided loss is terminal even if a late receipt arrives"
    );
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
async fn restart_preserves_unsubmitted_start_and_pending_approval() {
    for status in ["starting", "approval"] {
        let (root, store, id, _operation) = fixture().await;
        let selected = id.clone();
        store.transaction(move |tx| {
            tx.execute("UPDATE runtime_monitors SET record=json_remove(json_set(record,'$.status',?1),'$.operation') WHERE id=?2", (status, &selected))?;
            if status == "approval" {
                studio_runtime_store::Table::new(tx).put(&studio_runtime_store::RequestRecord::from_value(json!({"id":"approval","agent":"agent","method":"monitor/approve","status":"pending","params":{"monitorId":selected},"createdAt":2000.0}))?)?;
            }
            Ok(())
        }).await.expect("persist unsubmitted intent");
        drop(store);
        let store = RuntimeStore::open(Some(&root.path().join("wake")))
            .await
            .expect("restart");
        for now in [2001.0, 2002.0] {
            monitor_runtime::recover_on_startup(&store, now)
                .await
                .expect("preserve first-start intent");
            assert_eq!(fixtures::events(&store), json!([]));
            let starts = studio_runtime_services::monitor_driver::pending_start_ids(&store)
                .await
                .expect("pending starts");
            assert_eq!(
                starts,
                if status == "starting" {
                    vec![id.clone()]
                } else {
                    vec![]
                }
            );
            let record = MonitorsRepository::get(&store.test_reader().expect("reader"), &id)
                .expect("read")
                .expect("monitor");
            assert_eq!(record.value()["status"], status);
            assert!(record.value().get("reattachRecovery").is_none());
            if status == "approval" {
                let request =
                    studio_runtime_store::Table::<studio_runtime_store::RequestRecord>::new(
                        &store.test_reader().expect("reader"),
                    )
                    .get(&"approval".to_owned())
                    .expect("request read")
                    .expect("approval");
                assert_eq!(request.value()["status"], "pending");
            }
        }
        if status == "approval" {
            let answer = monitor_runtime::answer_approval(
                &store,
                "approval",
                &json!({"decision":"accept"}),
                2003.0,
                true,
            )
            .await
            .expect("answer survives restart")
            .expect("monitor approval");
            assert!(answer.accepted);
            assert_eq!(
                studio_runtime_services::monitor_driver::pending_start_ids(&store)
                    .await
                    .expect("approved start"),
                vec![id]
            );
        }
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

/// Fail every monitor row write that stages a reattach, as a full disk fails a
/// SQLite write. The trigger is the exact terminal write of an Unknown start.
const FAIL_RECONCILE_WRITE: &str = "CREATE TRIGGER fail_reconcile_insert BEFORE INSERT ON runtime_monitors WHEN json_extract(NEW.record,'$.reattachRecovery.status')='running' BEGIN SELECT RAISE(ABORT,'database or disk is full'); END; CREATE TRIGGER fail_reconcile_update BEFORE UPDATE ON runtime_monitors WHEN json_extract(NEW.record,'$.reattachRecovery.status')='running' BEGIN SELECT RAISE(ABORT,'database or disk is full'); END;";
const FREE_RECONCILE_WRITE: &str =
    "DROP TRIGGER fail_reconcile_insert; DROP TRIGGER fail_reconcile_update;";

#[tokio::test]
async fn unknown_submission_reconciliation_is_retried_after_a_failed_write_and_wakes_once() {
    use std::sync::atomic::{AtomicU32, Ordering};
    use studio_runtime_services::{
        monitor_driver::{self, MonitorNative, Submission},
        monitor_policy::{Effect, MonitorError},
    };
    /// The account worker ended while the command ran, so the reply is
    /// Unknown. The first pause frees the disk, as the user did.
    struct FullDiskPort<'a> {
        store: &'a RuntimeStore,
        pauses: AtomicU32,
    }
    impl MonitorNative for FullDiskPort<'_> {
        fn submit<'a>(
            &'a self,
            _: &'a Operation,
            _: &'a Effect,
        ) -> futures_util::future::BoxFuture<'a, Submission> {
            Box::pin(async { Submission::Unknown })
        }
        fn terminal_write_pause<'a>(
            &'a self,
            attempt: u32,
            error: &'a MonitorError,
        ) -> futures_util::future::BoxFuture<'a, ()> {
            Box::pin(async move {
                assert!(error.retryable_store_failure(), "{error}");
                assert_eq!(self.pauses.fetch_add(1, Ordering::SeqCst) + 1, attempt);
                self.store
                    .transaction(|tx| Ok(tx.execute_batch(FREE_RECONCILE_WRITE)?))
                    .await
                    .expect("free the disk");
            })
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
    store
        .transaction(|tx| Ok(tx.execute_batch(FAIL_RECONCILE_WRITE)?))
        .await
        .expect("fill the disk");
    let port = FullDiskPort {
        store: &store,
        pauses: AtomicU32::new(0),
    };
    assert!(matches!(
        monitor_driver::start(&store, &port, snapshot.id(), &prepared, 2001.0)
            .await
            .expect("an Unknown start is reconciled once the store accepts the write"),
        Submission::Unknown
    ));
    assert_eq!(port.pauses.load(Ordering::SeqCst), 1, "one failed write");
    let monitor = MonitorsRepository::get(&store.test_reader().expect("reader"), snapshot.id())
        .expect("read")
        .expect("monitor");
    assert_eq!(monitor.value()["status"], "running");
    assert_eq!(monitor.value()["reattachRecovery"]["status"], "running");
    assert_eq!(fixtures::events(&store), json!([]));
    monitor_runtime::settle_reattach(&store, snapshot.id(), "provider_rejected", 2002.0)
        .await
        .expect("decided lost");
    assert_wake(&store, "lost");
}

mod orphan {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use studio_runtime_services::{
        monitor_driver::{MonitorNative, Submission},
        monitor_policy::{Effect, MonitorError, MonitorSnapshot, ReattachOutcome},
        monitor_runtime_wiring::{Delivery, InputReply, MonitorPort, MonitorRuntime},
        rules_engine::RuleCheck,
        rules_tick::{CheckReconciliation, CheckSubmission, RuleMonitor, RulesTickError},
    };

    /// A port whose command/exec reply future is gone: the account worker
    /// ended, so no waiter in this process will ever deliver the exit.
    struct OrphanPort {
        owned: bool,
        reattaches: AtomicUsize,
    }
    impl MonitorNative for OrphanPort {
        fn submit<'a>(
            &'a self,
            _: &'a Operation,
            _: &'a Effect,
        ) -> futures_util::future::BoxFuture<'a, Submission> {
            Box::pin(async { Submission::Unknown })
        }
    }
    impl RuleMonitor for OrphanPort {
        fn available(&self) -> bool {
            false
        }
        fn launch<'a>(
            &'a self,
            _: &'a RuleCheck,
        ) -> futures_util::future::BoxFuture<'a, CheckSubmission> {
            Box::pin(async { CheckSubmission::Unknown })
        }
        fn reconcile<'a>(
            &'a self,
            _: &'a RuleCheck,
        ) -> futures_util::future::BoxFuture<'a, CheckReconciliation> {
            Box::pin(async { CheckReconciliation::Unknown })
        }
        fn cancel<'a>(
            &'a self,
            _: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Result<(), RulesTickError>> {
            Box::pin(async { Ok(()) })
        }
    }
    impl MonitorPort for OrphanPort {
        fn next(
            &self,
        ) -> futures_util::future::BoxFuture<'_, Result<Option<Delivery>, MonitorError>> {
            Box::pin(async { Ok(None) })
        }
        fn acknowledge<'a>(
            &'a self,
            _: &'a Delivery,
        ) -> futures_util::future::BoxFuture<'a, Result<(), MonitorError>> {
            Box::pin(async { Ok(()) })
        }
        fn exit_owned(&self, _: &str) -> Option<bool> {
            Some(self.owned)
        }
        fn reattach<'a>(
            &'a self,
            _: &'a MonitorSnapshot,
        ) -> futures_util::future::BoxFuture<'a, Result<ReattachOutcome, MonitorError>> {
            Box::pin(async move {
                self.reattaches.fetch_add(1, Ordering::SeqCst);
                // The provider does not know the exact `monitor:<id>` operation.
                Ok(ReattachOutcome::Gone("operation_not_accepted".into()))
            })
        }
        fn probe<'a>(
            &'a self,
            _: &'a str,
            _: i64,
            _: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Result<(), MonitorError>> {
            Box::pin(async { Ok(()) })
        }
        fn input_reply<'a>(
            &'a self,
            _: &'a Operation,
            _: &'a str,
        ) -> futures_util::future::BoxFuture<'a, InputReply> {
            Box::pin(async { InputReply::Unknown("unused".into()) })
        }
    }

    async fn runtime(
        owned: bool,
    ) -> (
        tempfile::TempDir,
        Arc<RuntimeStore>,
        Arc<OrphanPort>,
        MonitorRuntime,
        String,
    ) {
        let (root, store, id, _) = fixture().await;
        let store = Arc::new(store);
        let port = Arc::new(OrphanPort {
            owned,
            reattaches: AtomicUsize::new(0),
        });
        let runtime = MonitorRuntime::new(
            Arc::clone(&store),
            port.clone(),
            Arc::new(tokio::sync::Notify::new()),
        );
        (root, store, port, runtime, id)
    }

    fn monitor(store: &RuntimeStore, id: &str) -> Value {
        MonitorsRepository::get(&store.test_reader().expect("reader"), id)
            .expect("read")
            .expect("monitor")
            .value()
            .clone()
    }

    #[tokio::test]
    async fn running_command_without_an_exit_owner_is_reattached_and_settled_once() {
        let (_root, store, port, runtime, id) = runtime(false).await;
        runtime.tick(2001.0).await.expect("orphan check");
        assert_eq!(
            monitor(&store, &id)["reattachRecovery"]["status"],
            "running"
        );
        assert_eq!(fixtures::events(&store), json!([]));
        runtime.tick(2002.0).await.expect("exact reattach");
        assert_eq!(port.reattaches.load(Ordering::SeqCst), 1);
        assert_eq!(monitor(&store, &id)["status"], "lost");
        assert_wake(&store, "lost");
        runtime.tick(2003.0).await.expect("idle tick");
        runtime.tick(2004.0).await.expect("idle tick");
        assert_eq!(port.reattaches.load(Ordering::SeqCst), 1);
        assert_wake(&store, "lost");
    }

    #[tokio::test]
    async fn running_command_with_a_live_exit_owner_is_left_to_its_owner() {
        let (_root, store, port, runtime, id) = runtime(true).await;
        runtime.tick(2001.0).await.expect("orphan check");
        runtime.tick(2002.0).await.expect("second tick");
        assert_eq!(port.reattaches.load(Ordering::SeqCst), 0);
        let record = monitor(&store, &id);
        assert_eq!(record["status"], "running");
        assert!(record.get("reattachRecovery").is_none());
        assert_eq!(fixtures::events(&store), json!([]));
    }
}
