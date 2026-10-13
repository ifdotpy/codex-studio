use serde_json::json;
use std::{fs, path::Path, process::Command, sync::OnceLock};
use studio_runtime_services::{
    recovery::{RecoveryAction, RecoveryEffects, RecoveryError, RecoveryOutcome},
    recovery_history::ConnectionScope,
    recovery_policy::RecoverySnapshot,
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
fn fixture() -> tempfile::TempDir {
    fn temporary() -> tempfile::TempDir {
        let base = if Path::new("/dev/shm").is_dir() {
            std::path::PathBuf::from("/dev/shm")
        } else {
            std::env::temp_dir()
        };
        tempfile::Builder::new()
            .prefix("cs-rust-turn-recovery-")
            .tempdir_in(base)
            .expect("isolated store")
    }
    static TEMPLATE: OnceLock<Vec<(std::ffi::OsString, Vec<u8>)>> = OnceLock::new();
    let template = TEMPLATE.get_or_init(|| {
        let root = temporary();
        let result = Command::new(
            std::env::var_os("CODEX_AGENTS_PYTHON").expect("prepared Python required"),
        )
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
fn snapshot(store: &RuntimeStore) -> RecoverySnapshot {
    let raw = AgentRepository::get_raw(
        &store.read().expect("valid recovery fixture"),
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
    ) -> Result<RecoveryOutcome, RecoveryError> {
        Err(RecoveryError::EffectsUnavailable)
    }
}
fn apply(
    store: &RuntimeStore,
    expected: &RecoverySnapshot,
    current: bool,
    action: RecoveryAction,
) -> Result<RecoveryOutcome, RecoveryError> {
    RecoveryStore {
        store,
        connection_current: &|_| current,
        completion: &mut Completion,
        at: 100.0,
        observation_current: &|_| None,
    }
    .apply(expected, &scope(), action)
}
fn events(store: &RuntimeStore) -> Vec<(String, String)> {
    store
        .read()
        .expect("valid recovery fixture")
        .prepare("SELECT id,status FROM runtime_events WHERE agent='agent' ORDER BY id")
        .expect("valid recovery fixture")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("valid recovery fixture")
        .collect::<Result<_, _>>()
        .expect("valid recovery fixture")
}
fn projection(store: &RuntimeStore) -> Vec<String> {
    let db = store.read().expect("store read");
    ["SELECT json_group_array(json_array(collection,id,seq,hash,payload,deleted)) FROM (SELECT * FROM sync_entities ORDER BY collection,id)","SELECT json_group_array(json_array(key,value)) FROM (SELECT * FROM sync_entity_meta ORDER BY key)"]
        .iter().map(|sql|db.query_row(sql,[],|row|row.get(0)).expect("projection read")).collect()
}
fn preparation_retry_after_response_loss_preserves_exact_input() {
    let root = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("valid recovery fixture");
    let before = snapshot(&store);
    let result = apply(
        &store,
        &before,
        true,
        RecoveryAction::RestorePreparation {
            disconnected: false,
        },
    )
    .expect("valid recovery fixture");
    assert_eq!(
        result,
        RecoveryOutcome::Applied(json!({"status":"input_restored","attemptId":"attempt"}))
    );
    let after = snapshot(&store).record().clone();
    let queue = events(&store);
    drop(store);
    let reopened = RuntimeStore::open(Some(root.path())).expect("valid recovery fixture");
    assert_eq!(
        apply(
            &reopened,
            &before,
            true,
            RecoveryAction::RestorePreparation {
                disconnected: false
            }
        )
        .expect("valid recovery fixture"),
        RecoveryOutcome::Superseded
    );
    assert_eq!(snapshot(&reopened).record(), &after);
    assert_eq!(events(&reopened), queue);
    assert_eq!(queue, vec![("event".into(), "pending".into())]);
}
fn stale_epoch_connection_and_uncertain_input_write_nothing() {
    for change in ["epoch", "connection", "uncertain", "other_uncertain"] {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("valid recovery fixture");
        let before = snapshot(&store);
        store
            .transaction(|tx| {
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
            .expect("valid recovery fixture"),
            RecoveryOutcome::Superseded | RecoveryOutcome::Unconfirmed
        ));
        assert_eq!(snapshot(&store).record(), &prior);
        assert_eq!(events(&store), queue);
        assert_eq!(projection(&store), renderer);
    }
}
fn preparation_transaction_failpoints_roll_back_every_row() {
    for fail in ["before_first_event", "before_agent", "after_agent"] {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("valid recovery fixture");
        lifecycle_transaction(&store,|tx| {let mut a=AgentRepository::get(tx,&AgentId::from("agent"))?.expect("valid recovery fixture");let attempt=a.extra.remove("startAttempt").expect("valid recovery fixture");a.status=studio_runtime_store::AgentStatus::Interrupted;a.extra.insert("autoWake".into(),json!(false));a.extra.insert("error".into(),json!("Codex disconnected. Review the transcript before resuming."));a.extra.insert("disconnectRecovery".into(),json!({"epoch":0,"accountKey":"default","threadId":"thread","turnId":null,"autoWake":true,"startAttempt":attempt}));AgentRepository::put(tx,&a)?;tx.execute("UPDATE runtime_events SET status='reserved' WHERE id='event'",[])?;Ok(())}).expect("valid recovery fixture");
        let before = snapshot(&store);
        let queue = events(&store);
        let renderer = projection(&store);
        store.transaction(|tx| {tx.execute_batch(if fail=="before_first_event" {"CREATE TRIGGER recovery_fail BEFORE UPDATE ON runtime_events BEGIN SELECT RAISE(ABORT,'injected'); END;"}else if fail=="before_agent" {"CREATE TRIGGER recovery_fail BEFORE UPDATE ON runtime_agents BEGIN SELECT RAISE(ABORT,'injected'); END;"}else{"CREATE TRIGGER recovery_fail AFTER UPDATE ON runtime_agents BEGIN SELECT RAISE(ABORT,'injected'); END;"})?;Ok(())}).expect("valid recovery fixture");
        assert!(
            apply(
                &store,
                &before,
                true,
                RecoveryAction::RestorePreparation { disconnected: true }
            )
            .is_err()
        );
        assert_eq!(snapshot(&store).record(), before.record());
        assert_eq!(events(&store), queue);
        assert_eq!(projection(&store), renderer);
        drop(store);
        let reopened = RuntimeStore::open(Some(root.path())).expect("valid recovery fixture");
        assert_eq!(snapshot(&reopened).record(), before.record());
        assert_eq!(events(&reopened), queue);
        assert_eq!(projection(&reopened), renderer);
    }
}
fn orphan_requires_the_completed_turn_receipt_and_no_uncertain_input() {
    let root = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("valid recovery fixture");
    lifecycle_transaction(&store, |tx| {
        let mut a =
            AgentRepository::get(tx, &AgentId::from("agent"))?.expect("valid recovery fixture");
        a.extra.remove("startAttempt");
        a.extra.insert("inFlight".into(), json!(true));
        AgentRepository::put(tx, &a)
    })
    .expect("valid recovery fixture");
    let before = snapshot(&store);
    let action = RecoveryAction::OrphanIdle {
        latest: Some(json!({"id":"old","status":"completed"})),
    };
    assert_eq!(
        apply(&store, &before, true, action.clone()).expect("valid recovery fixture"),
        RecoveryOutcome::Unconfirmed
    );
    store
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runtime_completed_turns VALUES ('agent:old')",
                [],
            )?;
            Ok(())
        })
        .expect("valid recovery fixture");
    assert!(matches!(
        apply(&store, &before, true, action).expect("valid recovery fixture"),
        RecoveryOutcome::Applied(_)
    ));
    assert_eq!(snapshot(&store).record()["inFlight"], false);
}

#[test]
fn recovery_transaction_restart_matrix() {
    preparation_retry_after_response_loss_preserves_exact_input();
    stale_epoch_connection_and_uncertain_input_write_nothing();
    preparation_transaction_failpoints_roll_back_every_row();
    orphan_requires_the_completed_turn_receipt_and_no_uncertain_input();
}

fn oracle(name: &str) -> Vec<serde_json::Value> {
    let output = Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").expect("Python"))
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
fn seed_agent(store: &RuntimeStore, agent: &serde_json::Value) {
    lifecycle_transaction(store, |tx| {
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
    fn read(
        &mut self,
        scope: &ConnectionScope,
        method: studio_runtime_services::recovery_history::HistoryMethod,
        _: serde_json::Value,
        _: std::time::Instant,
    ) -> Result<
        studio_runtime_services::recovery_history::HistoryReceipt,
        studio_runtime_services::recovery_history::HistoryError,
    > {
        use studio_runtime_services::recovery_history::*;
        Ok(HistoryReceipt {
            scope: scope.clone(),
            body: match method {
                HistoryMethod::Turns => self.replies.pop_front().expect("page"),
                HistoryMethod::Thread => json!({"thread":{"id":"thread","status":self.state}}),
                _ => panic!("unexpected read"),
            },
        })
    }
    fn barrier(
        &mut self,
        _: &ConnectionScope,
        _: std::time::Instant,
    ) -> Result<(), studio_runtime_services::recovery_history::HistoryError> {
        Ok(())
    }
}
#[derive(Default)]
struct HoldWriter {
    stopped: Vec<serde_json::Value>,
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
    ) -> Result<RecoveryOutcome, RecoveryError> {
        panic!("no terminal completion")
    }
}
#[test]
fn legacy_start_absence_matches_python_writes_and_visible_wait() {
    for case in oracle("start_oracle")
        .into_iter()
        .filter(|c| c["history_case"] == false)
    {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        seed_agent(&store, &case["agent"]);
        let before = snapshot(&store);
        let observation: studio_runtime_services::recovery_absence::RecoveryObservation =
            serde_json::from_value(case["observation"].clone()).expect("observation");
        let mut reader = StartReader {
            replies: case["replies"].as_array().expect("pages").clone().into(),
            state: case["state"].clone(),
            observation: observation.clone(),
        };
        let mut writer = HoldWriter::default();
        let current = |_: &ConnectionScope| Some(observation.clone());
        let mut effects = RecoveryStore {
            store: &store,
            connection_current: &|_| true,
            completion: &mut writer,
            at: observation.at,
            observation_current: &current,
        };
        let result = studio_runtime_services::recovery::recover_turn(
            &before,
            observation.scope.clone(),
            &mut reader,
            &mut effects,
        )
        .expect("recover");
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
        assert_eq!(json!(writer.stopped), case["stopped"], "{}", case["name"]);
        let queue = events(&store);
        assert_eq!(queue[0].1, case["events"][0][3], "{}", case["name"]);
        let saved = snapshot(&store).record().clone();
        drop(store);
        let reopened = RuntimeStore::open(Some(root.path())).expect("reopen");
        assert_eq!(snapshot(&reopened).record(), &saved);
        assert_eq!(events(&reopened), queue);
    }
}

#[test]
fn start_absence_failpoints_roll_back_restore_wait_and_hold() {
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
            let store = RuntimeStore::open(Some(root.path())).expect("store");
            seed_agent(&store, &case["agent"]);
            let before = snapshot(&store);
            let queue = events(&store);
            let renderer = projection(&store);
            let op = if table == "runtime_completed_turns" {
                "INSERT"
            } else {
                "UPDATE"
            };
            store.transaction(|tx| {tx.execute_batch(&format!("CREATE TRIGGER recovery_fail {timing} {op} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END;"))?;Ok(())}).expect("trigger");
            let observation: studio_runtime_services::recovery_absence::RecoveryObservation =
                serde_json::from_value(case["observation"].clone()).expect("observation");
            let current = |_: &ConnectionScope| Some(observation.clone());
            let mut writer = HoldWriter::default();
            let result = RecoveryStore {
                store: &store,
                connection_current: &|_| true,
                completion: &mut writer,
                at: observation.at,
                observation_current: &current,
            }
            .apply(
                &before,
                &observation.scope,
                RecoveryAction::StartAbsent {
                    observation: observation.clone(),
                    complete_history: true,
                    idle_twice: true,
                },
            );
            assert!(result.is_err(), "{name}/{table}/{timing}");
            drop(store);
            let reopened = RuntimeStore::open(Some(root.path())).expect("reopen");
            assert_eq!(snapshot(&reopened).record(), before.record());
            assert_eq!(events(&reopened), queue);
            assert_eq!(projection(&reopened), renderer);
            let count: i64 = reopened
                .read()
                .expect("read")
                .query_row("SELECT count(*) FROM runtime_completed_turns", [], |r| {
                    r.get(0)
                })
                .expect("count");
            assert_eq!(count, 0);
        }
    }
}

struct TaskWriter;
impl studio_runtime_services::task_recovery_store::TaskCompletionWriter for TaskWriter {
    fn resolve_receipt(
        &mut self,
        _: &serde_json::Value,
    ) -> Result<serde_json::Value, RecoveryError> {
        panic!("no receipt")
    }
    fn complete(
        &mut self,
        _: &studio_runtime_services::task_recovery::TaskRecoverySnapshot,
        _: &ConnectionScope,
        _: serde_json::Value,
    ) -> Result<bool, RecoveryError> {
        panic!("no terminal item")
    }
}
fn seed_task(store: &RuntimeStore, case: &serde_json::Value) {
    seed_agent(store, &case["agent"]);
    lifecycle_transaction(store, |tx| {
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
    .expect("seed task");
}
fn task_rows(store: &RuntimeStore) -> Vec<serde_json::Value> {
    ["runtime_tasks", "runtime_items"]
        .iter()
        .map(|table| {
            let raw: String = store
                .read()
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
    fn barrier(
        &mut self,
        _: &ConnectionScope,
        _: std::time::Instant,
    ) -> Result<(), studio_runtime_services::recovery_history::HistoryError> {
        if self.page > 0 && self.options["quiet_after"] == false {
            self.observation.quiet = false;
        }
        Ok(())
    }
    fn read(
        &mut self,
        scope: &ConnectionScope,
        method: studio_runtime_services::recovery_history::HistoryMethod,
        _: serde_json::Value,
        _: std::time::Instant,
    ) -> Result<
        studio_runtime_services::recovery_history::HistoryReceipt,
        studio_runtime_services::recovery_history::HistoryError,
    > {
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
        Ok(HistoryReceipt {
            scope: scope.clone(),
            body,
        })
    }
}
#[test]
fn task_absence_consumer_matches_python_lost_and_persists_named_hold() {
    use studio_runtime_services::{recovery_absence::*, task_recovery::*, task_recovery_store::*};
    for case in oracle("absence_oracle") {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        seed_task(&store, &case);
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
        let current = |_: &ConnectionScope| Some(observation.clone());
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
            store: &store,
            observation_current: &current,
            completion: &mut TaskWriter,
        };
        assert!(
            recover_task(
                &before,
                observation.scope.clone(),
                &mut reader,
                &mut effects
            )
            .expect("recover"),
            "{}",
            case["name"]
        );
        let rows = task_rows(&store);
        let projected: String = store
            .read()
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
            &store.read().expect("read"),
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
        drop(store);
        let reopened = RuntimeStore::open(Some(root.path())).expect("reopen");
        assert_eq!(task_rows(&reopened), rows);
    }
}
#[test]
fn task_absence_failpoints_and_stale_proofs_preserve_rows() {
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
            let store = RuntimeStore::open(Some(root.path())).expect("store");
            seed_task(&store, case);
            let before = TaskRecoverySnapshot::from_records(
                case["agent"].clone(),
                case["task"].clone(),
                vec![],
                vec![],
            )
            .expect("snapshot");
            let rows = task_rows(&store);
            let renderer = projection(&store);
            store.transaction(|tx|{tx.execute_batch(&format!("CREATE TRIGGER recovery_fail {timing} {} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END;", if table=="sync_entities" {"INSERT"} else {"UPDATE"}))?;Ok(())}).expect("trigger");
            let observation: RecoveryObservation =
                serde_json::from_value(case["observation"].clone()).expect("observation");
            let current = |_: &ConnectionScope| Some(observation.clone());
            let decision = if mode == "lost" {
                TaskAbsenceDecision::Lost
            } else {
                TaskAbsenceDecision::Hold {
                    reason: "native-command-absence-unproved".into(),
                }
            };
            let result = TaskRecoveryStore {
                store: &store,
                observation_current: &current,
                completion: &mut TaskWriter,
            }
            .apply_absence(&before, &observation.scope, &observation, decision);
            assert!(result.is_err());
            drop(store);
            let reopened = RuntimeStore::open(Some(root.path())).expect("reopen");
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
        let store = RuntimeStore::open(Some(root.path())).expect("store");
        seed_task(&store, case);
        let before = TaskRecoverySnapshot::from_records(
            case["agent"].clone(),
            case["task"].clone(),
            vec![],
            vec![],
        )
        .expect("snapshot");
        store.transaction(|tx|{match change{
            "agent"=>{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',1)",[])?;},
            "task"=>{tx.execute("UPDATE runtime_tasks SET record=json_set(record,'$.processId','changed')",[])?;},
            "completed"=>{tx.execute("DELETE FROM runtime_completed_turns",[])?;},_=>{}
        }Ok(())}).expect("change");
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
        let current = |_: &ConnectionScope| Some(current_observation.clone());
        TaskRecoveryStore {
            store: &store,
            observation_current: &current,
            completion: &mut TaskWriter,
        }
        .apply_absence(
            &before,
            &observation.scope,
            &observation,
            TaskAbsenceDecision::Lost,
        )
        .expect("apply");
        let after = task_rows(&store);
        assert_ne!(after[0]["status"], "lost");
        assert_eq!(after[1], rows[1]);
        if ["agent", "task", "connection"].contains(&change) {
            assert_eq!(after, rows);
        }
    }
}

#[test]
fn held_task_rechecks_current_receipts_before_later_absence_recovery() {
    use studio_runtime_services::{recovery_absence::*, task_recovery::*, task_recovery_store::*};
    let case = oracle("absence_oracle")
        .into_iter()
        .find(|c| c["name"] == "terminal-list-unavailable")
        .expect("case");
    let root = fixture();
    let store = RuntimeStore::open(Some(root.path())).expect("store");
    seed_task(&store, &case);
    let before = TaskRecoverySnapshot::from_records(
        case["agent"].clone(),
        case["task"].clone(),
        vec![],
        vec![],
    )
    .expect("snapshot");
    let observation: RecoveryObservation =
        serde_json::from_value(case["observation"].clone()).expect("observation");
    let current = |_: &ConnectionScope| Some(observation.clone());
    let hold = TaskAbsenceDecision::Hold {
        reason: "native-command-absence-unproved".into(),
    };
    TaskRecoveryStore {
        store: &store,
        observation_current: &current,
        completion: &mut TaskWriter,
    }
    .apply_absence(&before, &observation.scope, &observation, hold)
    .expect("hold");
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("reopen");
    let held = task_rows(&store)[0].clone();
    assert_eq!(held["status"], "running");
    assert_eq!(held["recoveryHold"]["stage"], "held");
    let mut effects = TaskRecoveryStore {
        store: &store,
        observation_current: &current,
        completion: &mut TaskWriter,
    };
    assert!(
        !effects
            .apply_absence(
                &before,
                &observation.scope,
                &observation,
                TaskAbsenceDecision::Lost
            )
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
        .expect("later proof")
    );
    let after = &task_rows(&store)[0];
    assert_eq!(after["status"], "lost");
    assert!(after.get("recoveryHold").is_none());
}
