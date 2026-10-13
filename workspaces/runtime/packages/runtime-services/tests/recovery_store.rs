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
    static TEMPLATE: OnceLock<tempfile::TempDir> = OnceLock::new();
    let template = TEMPLATE.get_or_init(|| {
        let root = tempfile::tempdir().expect("valid recovery fixture");
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
        root
    });
    fn copy(from: &Path, to: &Path) {
        fs::create_dir_all(to).expect("valid recovery fixture");
        for entry in fs::read_dir(from).expect("valid recovery fixture") {
            let entry = entry.expect("valid recovery fixture");
            if entry.file_type().expect("valid recovery fixture").is_dir() {
                copy(&entry.path(), &to.join(entry.file_name()));
            } else {
                fs::copy(entry.path(), to.join(entry.file_name())).expect("valid recovery fixture");
            }
        }
    }
    let result = tempfile::tempdir().expect("valid recovery fixture");
    copy(template.path(), result.path());
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
    for change in ["epoch", "connection", "uncertain"] {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("valid recovery fixture");
        let before = snapshot(&store);
        store.transaction(|tx| {match change {"epoch"=>{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',1) WHERE id='agent'",[])?;},"uncertain"=>{tx.execute("UPDATE runtime_events SET status='uncertain' WHERE id='event'",[])?;},_=>{}}Ok(())}).expect("valid recovery fixture");
        let prior = snapshot(&store).record().clone();
        let queue = events(&store);
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
