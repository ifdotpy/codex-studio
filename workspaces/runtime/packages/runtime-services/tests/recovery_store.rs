#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::json;
use std::{
    fs,
    path::Path,
    process::Command,
    sync::{Arc, Mutex, OnceLock},
};
use studio_runtime_services::{
    recovery::{
        RecoveryAction, RecoveryEffectResult, RecoveryEffects, RecoveryError, RecoveryOutcome,
    },
    recovery_history::ConnectionScope,
    recovery_policy::RecoverySnapshot,
    recovery_runtime::ObservationSource,
    recovery_store::{CompletionWriter, RecoveryStore},
    scheduler::lifecycle_transaction,
};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore, Transaction};
fn scope() -> ConnectionScope {
    ConnectionScope {
        account: "default".into(),
        connection: "connection".into(),
        generation: 1,
    }
}
#[allow(clippy::disallowed_methods)]
fn fixture() -> tempfile::TempDir {
    fn temporary() -> tempfile::TempDir {
        let base = std::env::temp_dir();
        tempfile::Builder::new()
            .prefix("cs-rust-turn-recovery-")
            .tempdir_in(base)
            .expect("isolated store")
    }
    static TEMPLATE: OnceLock<Vec<(std::ffi::OsString, Vec<u8>)>> = OnceLock::new();
    let template = TEMPLATE.get_or_init(|| {
        let root = temporary();
        let result = Command::new(python_test_support::prepared_python(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        ))
        .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recovery/state.py"))
        .arg(root.path())
        .output()
        .expect("valid recovery fixture");
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        fs::read_dir(root.path())
            .expect("template files")
            .map(|entry| {
                let entry = entry.expect("file");
                (
                    entry.file_name(),
                    fs::read(entry.path()).expect("template bytes"),
                )
            })
            .collect()
    });
    let result = temporary();
    for (name, bytes) in template {
        fs::write(result.path().join(name), bytes).expect("cloned template");
    }
    result
}
async fn open_store(path: &Path) -> Arc<RuntimeStore> {
    Arc::new(
        RuntimeStore::open(Some(path))
            .await
            .expect("open recovery fixture"),
    )
}
fn snapshot(store: &RuntimeStore) -> RecoverySnapshot {
    let raw = AgentRepository::get_raw(
        &store.test_reader().expect("valid recovery fixture"),
        &AgentId::from("agent"),
    )
    .expect("valid recovery fixture")
    .expect("valid recovery fixture");
    RecoverySnapshot::from_record(serde_json::from_str(&raw).expect("valid recovery fixture"))
        .expect("valid recovery fixture")
}
struct Completion;
impl CompletionWriter for Completion {
    fn start_held(
        &mut self,
        _: &Transaction<'_>,
        _: &serde_json::Value,
        _: &str,
        _: &str,
    ) -> Result<(), RecoveryError> {
        Ok(())
    }

    fn complete(
        &mut self,
        _: &Transaction<'_>,
        _: &RecoverySnapshot,
        _: &ConnectionScope,
        _: &RecoveryAction,
    ) -> Result<RecoveryEffectResult, RecoveryError> {
        Err(RecoveryError::EffectsUnavailable)
    }
}
async fn apply(
    store: &Arc<RuntimeStore>,
    expected: &RecoverySnapshot,
    current: bool,
    action: RecoveryAction,
) -> Result<RecoveryOutcome, RecoveryError> {
    RecoveryStore {
        store: Arc::clone(store),
        connection_current: Arc::new(move |_| current),
        completion: Arc::new(Mutex::new(Box::new(Completion))),
        at: 100.0,
        observation_current: Arc::new(|_| None),
    }
    .apply(expected, &scope(), action)
    .await
    .map(|result| result.outcome)
}
struct NoRecoveryDelivery;
impl studio_runtime_services::recovery_store::RecoveryDelivery for NoRecoveryDelivery {
    fn start_held(
        &mut self,
        _: &Transaction<'_>,
        _: &serde_json::Value,
        _: &str,
        _: &str,
    ) -> Result<(), RecoveryError> {
        Ok(())
    }
    fn terminal(
        &mut self,
        _: &Transaction<'_>,
        _: &mut serde_json::Value,
        _: &serde_json::Value,
        _: studio_runtime_services::recovery::CompletionMode,
    ) -> Result<(), RecoveryError> {
        Ok(())
    }
}
async fn apply_lifecycle(
    store: &Arc<RuntimeStore>,
    expected: &RecoverySnapshot,
    action: RecoveryAction,
) -> Result<RecoveryOutcome, RecoveryError> {
    use studio_runtime_services::recovery_store::{
        LifecycleCompletionWriter, RecoveryMessageWriter,
    };
    let delivery: Box<dyn studio_runtime_services::recovery_store::RecoveryDelivery> =
        Box::new(NoRecoveryDelivery);
    let write_message: Box<RecoveryMessageWriter> = Box::new(|_, _, _, _, _, _, _| Ok(None));
    let completion = LifecycleCompletionWriter {
        at: 100.0,
        delivery,
        write_message,
    };
    RecoveryStore {
        store: Arc::clone(store),
        connection_current: Arc::new(|_| true),
        completion: Arc::new(Mutex::new(Box::new(completion))),
        at: 100.0,
        observation_current: Arc::new(|_| None),
    }
    .apply(expected, &scope(), action)
    .await
    .map(|result| result.outcome)
}
fn events(store: &RuntimeStore) -> Vec<(String, String)> {
    store
        .test_reader()
        .expect("valid recovery fixture")
        .prepare("SELECT id,status FROM runtime_events WHERE agent='agent' ORDER BY id")
        .expect("valid recovery fixture")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("valid recovery fixture")
        .collect::<Result<_, _>>()
        .expect("valid recovery fixture")
}
fn projection(store: &RuntimeStore) -> Vec<String> {
    let db = store.test_reader().expect("store read");
    ["SELECT json_group_array(json_array(collection,id,seq,hash,payload,deleted)) FROM (SELECT * FROM sync_entities ORDER BY collection,id)","SELECT json_group_array(json_array(key,value)) FROM (SELECT * FROM sync_entity_meta ORDER BY key)"]
        .iter().map(|sql|db.query_row(sql,[],|row|row.get(0)).expect("projection read")).collect()
}
async fn preparation_retry_after_response_loss_preserves_exact_input() {
    let root = fixture();
    let store = open_store(root.path()).await;
    let before = snapshot(&store);
    let result = apply(
        &store,
        &before,
        true,
        RecoveryAction::RestorePreparation {
            disconnected: false,
        },
    )
    .await
    .expect("valid recovery fixture");
    assert_eq!(
        result,
        RecoveryOutcome::Applied(json!({"status":"input_restored","attemptId":"attempt"}))
    );
    let after = snapshot(&store).record().clone();
    let queue = events(&store);
    drop(store);
    let reopened = open_store(root.path()).await;
    assert_eq!(
        apply(
            &reopened,
            &before,
            true,
            RecoveryAction::RestorePreparation {
                disconnected: false
            }
        )
        .await
        .expect("valid recovery fixture"),
        RecoveryOutcome::Superseded
    );
    assert_eq!(snapshot(&reopened).record(), &after);
    assert_eq!(events(&reopened), queue);
    assert_eq!(queue, vec![("event".into(), "pending".into())]);
}
async fn stale_epoch_connection_and_uncertain_input_write_nothing() {
    for change in ["epoch", "connection", "uncertain", "other_uncertain"] {
        let root = fixture();
        let store = open_store(root.path()).await;
        let before = snapshot(&store);
        store
            .transaction(move |tx| {
                match change {
                    "epoch" => {
                        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',1) WHERE id='agent'", [])?;
                    }
                    "uncertain" => {
                        tx.execute("UPDATE runtime_events SET status='uncertain' WHERE id='event'", [])?;
                    }
                    "other_uncertain" => {
                        // The attempted input stays pending: only the global uncertainty
                        // guard can prevent recovery while another input is unresolved.
                        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES('other','agent','user','unresolved input','uncertain',2,0,NULL,NULL)", [])?;
                    }
                    _ => {}
                }
                Ok(())
            })
            .await
            .expect("valid recovery fixture");
        let prior = snapshot(&store).record().clone();
        let queue = events(&store);
        let renderer = projection(&store);
        assert!(matches!(
            apply(
                &store,
                &before,
                change != "connection",
                RecoveryAction::RestorePreparation {
                    disconnected: false
                }
            )
            .await
            .expect("valid recovery fixture"),
            RecoveryOutcome::Superseded | RecoveryOutcome::Unconfirmed
        ));
        assert_eq!(snapshot(&store).record(), &prior);
        assert_eq!(events(&store), queue);
        assert_eq!(projection(&store), renderer);
    }
}
async fn preparation_transaction_failpoints_roll_back_every_row() {
    for fail in ["before_first_event", "before_agent", "after_agent"] {
        let root = fixture();
        let store = open_store(root.path()).await;
        lifecycle_transaction(&store,|tx| {let mut a=AgentRepository::get(tx,&AgentId::from("agent"))?.expect("valid recovery fixture");let attempt=a.extra.remove("startAttempt").expect("valid recovery fixture");a.status=studio_runtime_store::AgentStatus::Interrupted;a.extra.insert("autoWake".into(),json!(false));a.extra.insert("error".into(),json!(studio_runtime_store::PROVIDER_DISCONNECTED_TURN_ERROR));a.extra.insert("disconnectRecovery".into(),json!({"epoch":0,"accountKey":"default","threadId":"thread","turnId":null,"autoWake":true,"startAttempt":attempt}));AgentRepository::put(tx,&a)?;tx.execute("UPDATE runtime_events SET status='reserved' WHERE id='event'",[])?;Ok(())}).await.expect("valid recovery fixture");
        let before = snapshot(&store);
        let queue = events(&store);
        let renderer = projection(&store);
        store.transaction(move |tx| {tx.execute_batch(if fail=="before_first_event" {"CREATE TRIGGER recovery_fail BEFORE UPDATE ON runtime_events BEGIN SELECT RAISE(ABORT,'injected'); END;"}else if fail=="before_agent" {"CREATE TRIGGER recovery_fail BEFORE UPDATE ON runtime_agents BEGIN SELECT RAISE(ABORT,'injected'); END;"}else{"CREATE TRIGGER recovery_fail AFTER UPDATE ON runtime_agents BEGIN SELECT RAISE(ABORT,'injected'); END;"})?;Ok(())}).await.expect("valid recovery fixture");
        assert!(
            apply(
                &store,
                &before,
                true,
                RecoveryAction::RestorePreparation { disconnected: true }
            )
            .await
            .is_err()
        );
        assert_eq!(snapshot(&store).record(), before.record());
        assert_eq!(events(&store), queue);
        assert_eq!(projection(&store), renderer);
        drop(store);
        let reopened = open_store(root.path()).await;
        assert_eq!(snapshot(&reopened).record(), before.record());
        assert_eq!(events(&reopened), queue);
        assert_eq!(projection(&reopened), renderer);
    }
}
async fn orphan_requires_the_completed_turn_receipt_and_no_uncertain_input() {
    let root = fixture();
    let store = open_store(root.path()).await;
    lifecycle_transaction(&store, |tx| {
        let mut a =
            AgentRepository::get(tx, &AgentId::from("agent"))?.expect("valid recovery fixture");
        a.extra.remove("startAttempt");
        a.extra.insert("inFlight".into(), json!(true));
        AgentRepository::put(tx, &a)
    })
    .await
    .expect("valid recovery fixture");
    let before = snapshot(&store);
    let action = RecoveryAction::OrphanIdle {
        latest: Some(json!({"id":"old","status":"completed"})),
    };
    assert_eq!(
        apply(&store, &before, true, action.clone())
            .await
            .expect("valid recovery fixture"),
        RecoveryOutcome::Unconfirmed
    );
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_completed_turns VALUES ('agent:old')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("valid recovery fixture");
    assert!(matches!(
        apply(&store, &before, true, action)
            .await
            .expect("valid recovery fixture"),
        RecoveryOutcome::Applied(_)
    ));
    assert_eq!(snapshot(&store).record()["inFlight"], false);
}

#[tokio::test]
async fn recovery_transaction_restart_matrix() {
    preparation_retry_after_response_loss_preserves_exact_input().await;
    stale_epoch_connection_and_uncertain_input_write_nothing().await;
    preparation_transaction_failpoints_roll_back_every_row().await;
    orphan_requires_the_completed_turn_receipt_and_no_uncertain_input().await;
    queued_active_wait_is_adopted_and_keeps_input_pending().await;
    pending_native_request_blocks_queued_wait_adoption().await;
    queued_active_wait_terminal_is_reconciled_and_saved().await;
}

async fn queued_active_snapshot(store: &Arc<RuntimeStore>) -> RecoverySnapshot {
    let cases: Vec<serde_json::Value> =
        serde_json::from_str(include_str!("recovery/cases.json")).expect("recovery corpus");
    let mut agent = cases
        .iter()
        .find(|case| case["name"] == "queued-active")
        .expect("queued active fixture")["agent"]
        .clone();
    agent["connectionRecovery"]["requestId"] = json!("queued-recovery");
    agent["connectionRecovery"]["requestState"] = json!("pending");
    agent["connectionRecovery"]["requestBasis"] = json!([]);
    agent["connectionRecovery"]["requestReceipt"] = serde_json::Value::Null;
    lifecycle_transaction(store, move |tx| {
        let record = studio_runtime_store::AgentRecord::from_value(agent.clone())
            .expect("valid agent fixture");
        AgentRepository::put(tx, &record)?;
        tx.execute(
            "UPDATE runtime_events SET status='pending',turn_id=NULL,error=NULL WHERE id='event'",
            [],
        )?;
        Ok(())
    })
    .await
    .expect("seed queued active wait");
    snapshot(store)
}

async fn queued_active_wait_is_adopted_and_keeps_input_pending() {
    let root = fixture();
    let store = open_store(root.path()).await;
    let before = queued_active_snapshot(&store).await;
    let turn = json!({"id":"turn","threadId":"thread","status":"inProgress","items":[],"itemsView":"full"});
    let result = apply_lifecycle(&store, &before, RecoveryAction::AdoptQueuedWait { turn })
        .await
        .expect("adopt active native turn");
    assert_eq!(
        result,
        RecoveryOutcome::Applied(json!({"status":"adopted","turnId":"turn"}))
    );
    let after = snapshot(&store).record().clone();
    assert_eq!(after["status"], "running");
    assert_eq!(after["inFlight"], true);
    assert!(after.get("startAttempt").is_none());
    assert!(after.get("contextRepairWait").is_none());
    assert_eq!(after["connectionRecovery"]["queuedInputPreserved"], true);
    assert_eq!(after["connectionRecovery"]["requestState"], "result");
    assert_eq!(
        after["connectionRecovery"]["requestReceipt"]["status"],
        "adopted"
    );
    assert_eq!(events(&store), vec![("event".into(), "pending".into())]);
}

async fn pending_native_request_blocks_queued_wait_adoption() {
    let root = fixture();
    let store = open_store(root.path()).await;
    let before = queued_active_snapshot(&store).await;
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_requests(id,record) VALUES('pending-request',?1)",
                [json!({"agent":"agent","turnId":"turn","status":"pending"}).to_string()],
            )?;
            Ok(())
        })
        .await
        .expect("seed pending runtime request");
    let original = snapshot(&store).record().clone();
    let turn = json!({"id":"turn","threadId":"thread","status":"inProgress","items":[],"itemsView":"full"});
    assert_eq!(
        apply_lifecycle(&store, &before, RecoveryAction::AdoptQueuedWait { turn })
            .await
            .expect("pending request makes adoption unconfirmed"),
        RecoveryOutcome::Unconfirmed
    );
    assert_eq!(snapshot(&store).record(), &original);
    assert_eq!(events(&store), vec![("event".into(), "pending".into())]);
}

async fn queued_active_wait_terminal_is_reconciled_and_saved() {
    use studio_runtime_services::recovery::CompletionMode;
    let root = fixture();
    let store = open_store(root.path()).await;
    let before = queued_active_snapshot(&store).await;
    let turn =
        json!({"id":"turn","threadId":"thread","status":"completed","items":[],"itemsView":"full"});
    let result = apply_lifecycle(
        &store,
        &before,
        RecoveryAction::Terminal {
            turn,
            native_state: "idle".into(),
            mode: CompletionMode::QueuedActiveWait,
        },
    )
    .await
    .expect("reconcile exact terminal turn");
    assert_eq!(
        result,
        RecoveryOutcome::Applied(
            json!({"status":"reconciled","turnId":"turn","outcome":"completed"})
        )
    );
    let after = snapshot(&store).record().clone();
    assert_eq!(after["status"], "completed");
    assert_eq!(after["turnId"], serde_json::Value::Null);
    assert_eq!(after["connectionRecovery"]["requestState"], "result");
    assert_eq!(
        after["connectionRecovery"]["requestReceipt"]["status"],
        "reconciled"
    );
    assert_eq!(after["connectionRecovery"]["queuedInputPreserved"], true);
    assert_eq!(events(&store), vec![("event".into(), "pending".into())]);
}

#[allow(clippy::disallowed_methods)]
fn oracle(name: &str) -> Vec<serde_json::Value> {
    let output = Command::new(python_test_support::prepared_python(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    ))
    .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join(format!("tests/recovery/{name}.py")))
    .output()
    .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("cases")
}
async fn seed_agent(store: &Arc<RuntimeStore>, agent: &serde_json::Value) {
    let agent = agent.clone();
    lifecycle_transaction(store, move |tx| {
        tx.execute(
            "UPDATE runtime_agents SET record=?1 WHERE id='agent'",
            [agent.to_string()],
        )?;
        tx.execute(
            "UPDATE runtime_events SET status='uncertain',error='unknown' WHERE id='event'",
            [],
        )?;
        Ok(())
    })
    .await
    .expect("seed");
}
struct StartReader {
    replies: std::collections::VecDeque<serde_json::Value>,
    observation: studio_runtime_services::recovery_absence::RecoveryObservation,
    state: serde_json::Value,
}
impl studio_runtime_services::recovery_history::RecoveryReader for StartReader {
    fn observation(
        &mut self,
        _: &ConnectionScope,
    ) -> Result<
        studio_runtime_services::recovery_absence::RecoveryObservation,
        studio_runtime_services::recovery_history::HistoryError,
    > {
        Ok(self.observation.clone())
    }
    fn read<'a>(
        &'a mut self,
        scope: &ConnectionScope,
        method: studio_runtime_services::recovery_history::HistoryMethod,
        _: serde_json::Value,
        _: std::time::Instant,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<
            studio_runtime_services::recovery_history::HistoryReceipt,
            studio_runtime_services::recovery_history::HistoryError,
        >,
    > {
        let scope = scope.clone();
        Box::pin(async move {
            use studio_runtime_services::recovery_history::*;
            Ok(HistoryReceipt {
                scope,
                body: match method {
                    HistoryMethod::Turns => self.replies.pop_front().expect("page"),
                    HistoryMethod::Thread => json!({"thread":{"id":"thread","status":self.state}}),
                    _ => panic!("unexpected read"),
                },
            })
        })
    }
    fn barrier<'a>(
        &'a mut self,
        _: &ConnectionScope,
        _: std::time::Instant,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<(), studio_runtime_services::recovery_history::HistoryError>,
    > {
        Box::pin(async { Ok(()) })
    }
}
#[derive(Default)]
struct HoldWriter {
    stopped: Arc<Mutex<Vec<serde_json::Value>>>,
}
impl CompletionWriter for HoldWriter {
    fn start_held(
        &mut self,
        tx: &Transaction<'_>,
        _: &serde_json::Value,
        reason: &str,
        event_id: &str,
    ) -> Result<(), RecoveryError> {
        self.stopped
            .lock()
            .map_err(|_| RecoveryError::CommitFailed)?
            .push(json!({"status":"interrupted","reason":reason,"id":event_id}));
        tx.execute(
            "INSERT INTO runtime_completed_turns(id) VALUES(?1)",
            [event_id],
        )?;
        Ok(())
    }
    fn complete(
        &mut self,
        _: &Transaction<'_>,
        _: &RecoverySnapshot,
        _: &ConnectionScope,
        _: &RecoveryAction,
    ) -> Result<RecoveryEffectResult, RecoveryError> {
        panic!("no terminal completion")
    }
}
#[tokio::test]
async fn legacy_start_absence_matches_python_writes_and_visible_wait() {
    for case in oracle("start_oracle")
        .into_iter()
        .filter(|c| c["history_case"] == false)
    {
        let root = fixture();
        let store = open_store(root.path()).await;
        seed_agent(&store, &case["agent"]).await;
        let before = snapshot(&store);
        let observation: studio_runtime_services::recovery_absence::RecoveryObservation =
            serde_json::from_value(case["observation"].clone()).expect("observation");
        let mut reader = StartReader {
            replies: case["replies"].as_array().expect("pages").clone().into(),
            state: case["state"].clone(),
            observation: observation.clone(),
        };
        let stopped = Arc::new(Mutex::new(Vec::new()));
        let writer = HoldWriter {
            stopped: Arc::clone(&stopped),
        };
        let current_observation = observation.clone();
        let current: Arc<ObservationSource> =
            Arc::new(move |_: &ConnectionScope| Some(current_observation.clone()));
        let mut effects = RecoveryStore {
            store: Arc::clone(&store),
            connection_current: Arc::new(|_| true),
            completion: Arc::new(Mutex::new(Box::new(writer))),
            at: observation.at,
            observation_current: current,
        };
        let result = studio_runtime_services::recovery::recover_turn(
            &before,
            observation.scope.clone(),
            &mut reader,
            &mut effects,
        )
        .await
        .expect("recover")
        .outcome;
        assert_eq!(
            result,
            RecoveryOutcome::Applied(case["result"].clone()),
            "{}",
            case["name"]
        );
        let mut after = snapshot(&store).record().clone();
        if case["result"]["status"] == "unconfirmed" {
            assert_eq!(
                after["startRecoveryCheck"]["reason"],
                "start-outcome-unconfirmed"
            );
            after
                .as_object_mut()
                .expect("record")
                .remove("startRecoveryCheck");
        }
        assert_eq!(after, case["after"], "{}", case["name"]);
        assert_eq!(
            json!(*stopped.lock().expect("stopped witness")),
            case["stopped"],
            "{}",
            case["name"]
        );
        let queue = events(&store);
        assert_eq!(queue[0].1, case["events"][0][3], "{}", case["name"]);
        let saved = snapshot(&store).record().clone();
        drop(effects);
        drop(store);
        let reopened = open_store(root.path()).await;
        assert_eq!(snapshot(&reopened).record(), &saved);
        assert_eq!(events(&reopened), queue);
    }
}

#[tokio::test]
async fn start_absence_failpoints_roll_back_restore_wait_and_hold() {
    let cases = oracle("start_oracle");
    for (name, table) in [
        ("restore", "runtime_events"),
        ("restore", "runtime_agents"),
        ("waiting", "runtime_agents"),
        ("hold", "runtime_completed_turns"),
        ("hold", "runtime_agents"),
    ] {
        for timing in ["BEFORE", "AFTER"] {
            let case = cases.iter().find(|c| c["name"] == name).expect("case");
            let root = fixture();
            let store = open_store(root.path()).await;
            seed_agent(&store, &case["agent"]).await;
            let before = snapshot(&store);
            let queue = events(&store);
            let renderer = projection(&store);
            let op = if table == "runtime_completed_turns" {
                "INSERT"
            } else {
                "UPDATE"
            };
            store.transaction(move |tx| {tx.execute_batch(&format!("CREATE TRIGGER recovery_fail {timing} {op} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END;"))?;Ok(())}).await.expect("trigger");
            let observation: studio_runtime_services::recovery_absence::RecoveryObservation =
                serde_json::from_value(case["observation"].clone()).expect("observation");
            let current_observation = observation.clone();
            let current: Arc<ObservationSource> =
                Arc::new(move |_: &ConnectionScope| Some(current_observation.clone()));
            let writer = HoldWriter::default();
            let result = RecoveryStore {
                store: Arc::clone(&store),
                connection_current: Arc::new(|_| true),
                completion: Arc::new(Mutex::new(Box::new(writer))),
                at: observation.at,
                observation_current: current,
            }
            .apply(
                &before,
                &observation.scope,
                RecoveryAction::StartAbsent {
                    observation: observation.clone(),
                    complete_history: true,
                    idle_twice: true,
                    confirmed_not_applied: false,
                },
            )
            .await;
            assert!(result.is_err(), "{name}/{table}/{timing}");
            drop(store);
            let reopened = open_store(root.path()).await;
            assert_eq!(snapshot(&reopened).record(), before.record());
            assert_eq!(events(&reopened), queue);
            assert_eq!(projection(&reopened), renderer);
            let count: i64 = reopened
                .test_reader()
                .expect("read")
                .query_row("SELECT count(*) FROM runtime_completed_turns", [], |r| {
                    r.get(0)
                })
                .expect("count");
            assert_eq!(count, 0);
        }
    }
}

#[tokio::test]
async fn restart_not_applied_failpoints_roll_back_event_and_agent_together() {
    let case = oracle("start_oracle")
        .into_iter()
        .find(|case| case["name"] == "restore")
        .expect("restore oracle case");
    for (table, operation) in [("runtime_events", "UPDATE"), ("runtime_agents", "UPDATE")] {
        for timing in ["BEFORE", "AFTER"] {
            let root = fixture();
            let store = open_store(root.path()).await;
            let mut agent = case["agent"].clone();
            let mut attempt = agent["startAttempt"].clone();
            attempt["executionOutcome"] = json!("unknown");
            agent["restartRecovery"] = json!({"stage":"held","startAttempt":attempt});
            agent["contextRepairWait"] = json!({"events":["event"]});
            agent["status"] = json!("queued");
            agent["autoWake"] = json!(true);
            agent["inFlight"] = json!(false);
            agent["error"] = json!(null);
            if let Some(agent) = agent.as_object_mut() {
                agent.remove("startAttempt");
            } else {
                panic!("restore oracle agent is not an object");
            }
            seed_agent(&store, &agent).await;
            let before = snapshot(&store);
            let queue = events(&store);
            store
                .transaction(move |tx| {
                    tx.execute_batch(&format!(
                        "CREATE TRIGGER fail_reconcile {timing} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END;"
                    ))?;
                    Ok(())
                })
                .await
                .expect("trigger");
            let observation: studio_runtime_services::recovery_absence::RecoveryObservation =
                serde_json::from_value(case["observation"].clone()).expect("observation");
            let current_observation = observation.clone();
            let current: Arc<ObservationSource> =
                Arc::new(move |_: &ConnectionScope| Some(current_observation.clone()));
            let writer = HoldWriter::default();
            let result = RecoveryStore {
                store: Arc::clone(&store),
                connection_current: Arc::new(|_| true),
                completion: Arc::new(Mutex::new(Box::new(writer))),
                at: observation.at,
                observation_current: current,
            }
            .apply(
                &before,
                &observation.scope,
                RecoveryAction::StartAbsent {
                    observation: observation.clone(),
                    complete_history: true,
                    idle_twice: true,
                    confirmed_not_applied: true,
                },
            )
            .await;
            assert!(result.is_err(), "{table}/{timing}");
            drop(store);
            let reopened = open_store(root.path()).await;
            assert_eq!(snapshot(&reopened).record(), before.record());
            assert_eq!(events(&reopened), queue);
        }
    }
}

struct TaskWriter;
impl studio_runtime_services::task_recovery_store::TaskCompletionWriter for TaskWriter {
    fn resolve_receipt<'a>(
        &'a self,
        _: Arc<serde_json::Value>,
    ) -> futures_util::future::BoxFuture<'a, Result<serde_json::Value, RecoveryError>> {
        Box::pin(async { panic!("no receipt") })
    }
    fn complete<'a>(
        &'a self,
        _: &'a studio_runtime_services::task_recovery::TaskRecoverySnapshot,
        _: &'a ConnectionScope,
        _: serde_json::Value,
    ) -> futures_util::future::BoxFuture<'a, Result<bool, RecoveryError>> {
        Box::pin(async { panic!("no terminal item") })
    }
}
async fn seed_task(store: &Arc<RuntimeStore>, case: &serde_json::Value) {
    seed_agent(store, &case["agent"]).await;
    let case = case.clone();
    lifecycle_transaction(store, move |tx| {
        tx.execute(
            "INSERT INTO runtime_tasks(id,record) VALUES('agent:item',?1)",
            [case["task"].to_string()],
        )?;
        tx.execute(
            "INSERT INTO runtime_items(id,agent,record,created) VALUES('agent:item','agent',?1,10)",
            [json!({"id":"agent:item","turnId":"turn","toolStatus":"inProgress"}).to_string()],
        )?;
        if case["observation"]["completed_turn"] == "agent:turn" {
            tx.execute(
                "INSERT INTO runtime_completed_turns VALUES('agent:turn')",
                [],
            )?;
        }
        Ok(())
    })
    .await
    .expect("seed task");
}
fn task_rows(store: &RuntimeStore) -> Vec<serde_json::Value> {
    ["runtime_tasks", "runtime_items"]
        .iter()
        .map(|table| {
            let raw: String = store
                .test_reader()
                .expect("read")
                .query_row(
                    &format!("SELECT record FROM {table} WHERE id='agent:item'"),
                    [],
                    |r| r.get(0),
                )
                .expect("row");
            serde_json::from_str(&raw).expect("json")
        })
        .collect()
}
struct TaskReader {
    observation: studio_runtime_services::recovery_absence::RecoveryObservation,
    options: serde_json::Value,
    page: usize,
}
impl studio_runtime_services::recovery_history::RecoveryReader for TaskReader {
    fn observation(
        &mut self,
        _: &ConnectionScope,
    ) -> Result<
        studio_runtime_services::recovery_absence::RecoveryObservation,
        studio_runtime_services::recovery_history::HistoryError,
    > {
        Ok(self.observation.clone())
    }
    fn barrier<'a>(
        &'a mut self,
        _: &ConnectionScope,
        _: std::time::Instant,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<(), studio_runtime_services::recovery_history::HistoryError>,
    > {
        Box::pin(async move {
            if self.page > 0 && self.options["quiet_after"] == false {
                self.observation.quiet = false;
            }
            Ok(())
        })
    }
    fn read<'a>(
        &'a mut self,
        scope: &ConnectionScope,
        method: studio_runtime_services::recovery_history::HistoryMethod,
        _: serde_json::Value,
        _: std::time::Instant,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<
            studio_runtime_services::recovery_history::HistoryReceipt,
            studio_runtime_services::recovery_history::HistoryError,
        >,
    > {
        let scope = scope.clone();
        Box::pin(async move {
            use studio_runtime_services::recovery_history::*;
            let o = &self.options;
            let body = match method {
                HistoryMethod::Items => {
                    if o["unsupported"] == true {
                        return Err(HistoryError::Unsupported);
                    }
                    let item = json!({"turnId":"turn","item":{"id":"item","type":"commandExecution","status":"inProgress","processId":"42"}});
                    json!({"data":if o["duplicate"]==true {vec![item.clone(),item]}else if o["progress"]==true {vec![item]}else{vec![]}})
                }
                HistoryMethod::Thread => {
                    json!({"thread":{"id":"thread","status":{"type":o.get("state").cloned().unwrap_or(json!("idle"))}}})
                }
                HistoryMethod::BackgroundTerminals => {
                    if o["unavailable"] == true {
                        return Err(HistoryError::Unavailable);
                    }
                    let pages = o
                        .get("pages")
                        .cloned()
                        .unwrap_or(json!([o.get("terminals").cloned().unwrap_or(json!([]))]));
                    let body = json!({"data":pages[self.page],"nextCursor":if self.page+1<pages.as_array().expect("pages").len(){json!("next")}else{serde_json::Value::Null}});
                    self.page += 1;
                    body
                }
                _ => panic!("unexpected method"),
            };
            Ok(HistoryReceipt { scope, body })
        })
    }
}
#[tokio::test]
async fn task_absence_consumer_matches_python_lost_and_persists_named_hold() {
    use studio_runtime_services::{recovery_absence::*, task_recovery::*, task_recovery_store::*};
    for case in oracle("absence_oracle") {
        let root = fixture();
        let store = open_store(root.path()).await;
        seed_task(&store, &case).await;
        let observation: RecoveryObservation =
            serde_json::from_value(case["observation"].clone()).expect("observation");
        let before = TaskRecoverySnapshot::from_records(
            case["agent"].clone(),
            case["task"].clone(),
            vec![],
            vec![],
        )
        .expect("snapshot");
        assert_eq!(
            command_absence_source(
                before.agent(),
                before.task(),
                &observation,
                observation.completed_turn.is_some()
            ),
            case["source"].as_bool().expect("source"),
            "{}",
            case["name"]
        );
        let current_observation = observation.clone();
        let current: Arc<ObservationSource> =
            Arc::new(move |_: &ConnectionScope| Some(current_observation.clone()));
        let mut first = observation.clone();
        first.quiet = case["options"]
            .get("quiet")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(true);
        let mut reader = TaskReader {
            observation: first,
            options: case["options"].clone(),
            page: 0,
        };
        let mut effects = TaskRecoveryStore {
            store: Arc::clone(&store),
            observation_current: current,
            completion: Arc::new(TaskWriter),
        };
        assert!(
            recover_task(
                &before,
                observation.scope.clone(),
                &mut reader,
                &mut effects
            )
            .await
            .expect("recover"),
            "{}",
            case["name"]
        );
        let rows = task_rows(&store);
        let projected: String = store
            .test_reader()
            .expect("read")
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='task' AND id='agent:item'",
                [],
                |r| r.get(0),
            )
            .expect("visible task");
        let projected: serde_json::Value = serde_json::from_str(&projected).expect("projection");
        assert_eq!(projected["value"]["status"], rows[0]["status"]);
        // Error is an existing task-detail field, deliberately absent from TaskEntityDto.
        let detail = studio_runtime_store::WorkspaceViewsRepository::task(
            &store.test_reader().expect("read"),
            "agent:item",
        )
        .expect("task API read")
        .expect("task");
        assert_eq!(detail["error"], rows[0]["error"]);
        assert_eq!(
            studio_runtime_services::sync_views::project_persisted_record("taskDetail", &detail)
                .expect("detail projection")["error"],
            rows[0]["error"]
        );
        if case["absent"] == true {
            assert_eq!(rows[0], case["after"], "{}", case["name"]);
            assert_eq!(rows[1], case["item"]);
        } else {
            assert_eq!(rows[0]["status"], "running");
            assert_eq!(rows[0]["recoveryHold"]["stage"], "held");
            assert_eq!(rows[0]["recoveryHold"]["outcome"], "unknown");
            assert_eq!(rows[1], case["item"]);
            assert!(rows[0].get("finished").is_none());
        }
        drop(effects);
        drop(store);
        let reopened = open_store(root.path()).await;
        assert_eq!(task_rows(&reopened), rows);
    }
}
#[tokio::test]
async fn task_absence_failpoints_and_stale_proofs_preserve_rows() {
    use studio_runtime_services::{recovery_absence::*, task_recovery::*, task_recovery_store::*};
    let case = &oracle("absence_oracle")[0];
    for mode in ["lost", "hold"] {
        for (table, timing) in [
            ("runtime_tasks", "BEFORE"),
            ("runtime_tasks", "AFTER"),
            ("runtime_items", "BEFORE"),
            ("runtime_items", "AFTER"),
            ("sync_entities", "BEFORE"),
            ("sync_entities", "AFTER"),
        ] {
            if mode == "hold" && table == "runtime_items" {
                continue;
            }
            let root = fixture();
            let store = open_store(root.path()).await;
            seed_task(&store, case).await;
            let before = TaskRecoverySnapshot::from_records(
                case["agent"].clone(),
                case["task"].clone(),
                vec![],
                vec![],
            )
            .expect("snapshot");
            let rows = task_rows(&store);
            let renderer = projection(&store);
            store.transaction(move |tx|{tx.execute_batch(&format!("CREATE TRIGGER recovery_fail {timing} {} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END;", if table=="sync_entities" {"INSERT"} else {"UPDATE"}))?;Ok(())}).await.expect("trigger");
            let observation: RecoveryObservation =
                serde_json::from_value(case["observation"].clone()).expect("observation");
            let current_observation = observation.clone();
            let current: Arc<ObservationSource> =
                Arc::new(move |_: &ConnectionScope| Some(current_observation.clone()));
            let decision = if mode == "lost" {
                TaskAbsenceDecision::Lost
            } else {
                TaskAbsenceDecision::Hold {
                    reason: "native-command-absence-unproved".into(),
                }
            };
            let result = TaskRecoveryStore {
                store: Arc::clone(&store),
                observation_current: current,
                completion: Arc::new(TaskWriter),
            }
            .apply_absence(&before, &observation.scope, &observation, decision)
            .await;
            assert!(result.is_err());
            drop(store);
            let reopened = open_store(root.path()).await;
            assert_eq!(task_rows(&reopened), rows);
            assert_eq!(projection(&reopened), renderer);
        }
    }
    for change in [
        "agent",
        "task",
        "completed",
        "quiet",
        "reserved",
        "connection",
    ] {
        let root = fixture();
        let store = open_store(root.path()).await;
        seed_task(&store, case).await;
        let before = TaskRecoverySnapshot::from_records(
            case["agent"].clone(),
            case["task"].clone(),
            vec![],
            vec![],
        )
        .expect("snapshot");
        store.transaction(move |tx|{match change{
            "agent"=>{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',1)",[])?;},
            "task"=>{tx.execute("UPDATE runtime_tasks SET record=json_set(record,'$.processId','changed')",[])?;},
            "completed"=>{tx.execute("DELETE FROM runtime_completed_turns",[])?;},_=>{}
        }Ok(())}).await.expect("change");
        let rows = task_rows(&store);
        let observation: RecoveryObservation =
            serde_json::from_value(case["observation"].clone()).expect("observation");
        let mut current_observation = observation.clone();
        if change == "quiet" {
            current_observation.quiet = false;
        }
        if change == "reserved" {
            current_observation.account_reserved = true;
        }
        if change == "connection" {
            current_observation.scope.generation += 1;
        }
        let current: Arc<ObservationSource> =
            Arc::new(move |_: &ConnectionScope| Some(current_observation.clone()));
        TaskRecoveryStore {
            store: Arc::clone(&store),
            observation_current: current,
            completion: Arc::new(TaskWriter),
        }
        .apply_absence(
            &before,
            &observation.scope,
            &observation,
            TaskAbsenceDecision::Lost,
        )
        .await
        .expect("apply");
        let after = task_rows(&store);
        assert_ne!(after[0]["status"], "lost");
        assert_eq!(after[1], rows[1]);
        if ["agent", "task", "connection"].contains(&change) {
            assert_eq!(after, rows);
        }
    }
}

#[tokio::test]
async fn held_task_rechecks_current_receipts_before_later_absence_recovery() {
    use studio_runtime_services::{recovery_absence::*, task_recovery::*, task_recovery_store::*};
    let case = oracle("absence_oracle")
        .into_iter()
        .find(|c| c["name"] == "terminal-list-unavailable")
        .expect("case");
    let root = fixture();
    let store = open_store(root.path()).await;
    seed_task(&store, &case).await;
    let before = TaskRecoverySnapshot::from_records(
        case["agent"].clone(),
        case["task"].clone(),
        vec![],
        vec![],
    )
    .expect("snapshot");
    let observation: RecoveryObservation =
        serde_json::from_value(case["observation"].clone()).expect("observation");
    let current_observation = observation.clone();
    let current: Arc<ObservationSource> =
        Arc::new(move |_: &ConnectionScope| Some(current_observation.clone()));
    let hold = TaskAbsenceDecision::Hold {
        reason: "native-command-absence-unproved".into(),
    };
    TaskRecoveryStore {
        store: Arc::clone(&store),
        observation_current: Arc::clone(&current),
        completion: Arc::new(TaskWriter),
    }
    .apply_absence(&before, &observation.scope, &observation, hold)
    .await
    .expect("hold");
    drop(store);
    let store = open_store(root.path()).await;
    let held = task_rows(&store)[0].clone();
    assert_eq!(held["status"], "running");
    assert_eq!(held["recoveryHold"]["stage"], "held");
    let mut effects = TaskRecoveryStore {
        store: Arc::clone(&store),
        observation_current: current,
        completion: Arc::new(TaskWriter),
    };
    assert!(
        !effects
            .apply_absence(
                &before,
                &observation.scope,
                &observation,
                TaskAbsenceDecision::Lost
            )
            .await
            .expect("stale proof")
    );
    let current_task =
        TaskRecoverySnapshot::from_records(case["agent"].clone(), held, vec![], vec![])
            .expect("held snapshot");
    let mut reader = TaskReader {
        observation: observation.clone(),
        options: json!({}),
        page: 0,
    };
    assert!(
        recover_task(
            &current_task,
            observation.scope.clone(),
            &mut reader,
            &mut effects
        )
        .await
        .expect("later proof")
    );
    let after = &task_rows(&store)[0];
    assert_eq!(after["status"], "lost");
    assert!(after.get("recoveryHold").is_none());
}

#[tokio::test]
async fn an_adopted_turn_that_finished_while_the_worker_was_dead_is_reconciled() {
    // Live cutover 4: three agents stayed `running` with restartRecovery
    // "continued"/"active" after their native turns completed while the
    // account worker was down; nothing delivered turn/completed again.
    use studio_runtime_services::recovery::CompletionMode;
    let root = fixture();
    let store = open_store(root.path()).await;
    let attempt = json!({"id":"attempt","epoch":0,"accountKey":"default","threadId":"thread",
        "turnId":"turn","events":["event"],"submitted":true,"connectionGeneration":1,
        "activeAtReservation":false,"created":10});
    let agent = json!({
        "id":"agent","rootId":"agent","epoch":0,"turnEpoch":0,"accountKey":"default",
        "threadId":"thread","turnId":"turn","status":"running","autoWake":true,"inFlight":false,
        "error":null,"provider":"codex","lastCompletedTurn":"earlier",
        "lastCompletedTurnStatus":"completed","startAttempt":attempt,
        "disconnectRecovery":{"accountKey":"default","at":10.0,"autoWake":true,
            "connectionId":null,"epoch":0,"source":"restart","startAttempt":attempt,
            "threadId":"thread","turnId":"turn"},
        "restartRecovery":{"accountKey":"default","at":10.0,"autoWake":true,"connectionId":null,
            "epoch":0,"observedAt":20.0,"outcome":"active","stage":"continued",
            "startAttempt":attempt,"threadId":"thread","turnId":"turn"},
        "connectionRecovery":{"at":20.0,"automatic":true,"outcome":"active",
            "previousError":"Server restarted during a turn. Review history, then send a new instruction.",
            "source":"native_thread_read","turnId":"turn"}
    });
    lifecycle_transaction(&store, move |tx| {
        let record =
            studio_runtime_store::AgentRecord::from_value(agent.clone()).expect("agent fixture");
        AgentRepository::put(tx, &record)?;
        // Input that arrived while the worker was down.
        tx.execute(
            "UPDATE runtime_events SET status='pending',turn_id=NULL,error=NULL,epoch=0 WHERE id='event'",
            [],
        )?;
        Ok(())
    })
    .await
    .expect("seed the adopted turn");
    let before = snapshot(&store);
    assert!(before.eligibility().restart_turn);
    let turn =
        json!({"id":"turn","threadId":"thread","status":"completed","items":[],"itemsView":"full"});
    let result = apply_lifecycle(
        &store,
        &before,
        RecoveryAction::Terminal {
            turn,
            native_state: "idle".into(),
            mode: CompletionMode::Restart,
        },
    )
    .await
    .expect("reconcile the finished turn");
    assert_eq!(
        result,
        RecoveryOutcome::Applied(
            json!({"status":"reconciled","turnId":"turn","outcome":"completed"})
        )
    );
    let after = snapshot(&store).record().clone();
    assert_eq!(
        after["status"], "queued",
        "the pending input runs next, as after any finished turn"
    );
    assert_eq!(after["turnId"], serde_json::Value::Null);
    assert_eq!(after["lastCompletedTurn"], "turn");
    assert_eq!(events(&store), vec![("event".into(), "pending".into())]);
    assert_eq!(after["autoWake"], true, "the worker keeps its permission");
    assert_eq!(after["restartRecovery"]["stage"], "finished");
    assert!(after.get("startAttempt").is_none());
}
