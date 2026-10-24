//! A team account transfer advances at the turn wake of each member, before
//! the claim of its input, which refuses an agent the transfer holds. A member
//! busy with a turn at the request moves to the destination account at its
//! next wake. A lazy member moves its history before its input is claimed.
use super::*;
use crate::account_session_runtime::{
    AccountSessionError, AccountSessionFuture, AccountSessionPort, ConnectionScope,
    ProviderRequest, SessionReceipt,
};

const TRANSFER: &str = "5a300f72-4ea6-4d30-8a0e-9b2c13c4a001";

async fn edit_agent(state: &State, edit: impl FnOnce(&mut AgentRecord) + Send + 'static) {
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut row =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    edit(&mut row);
                    AgentRepository::put(tx, &row)?;
                    Ok(())
                })
                .await
        })
        .await
        .unwrap();
}

/// Request a team transfer of the fixture agent to `target` (a Codex account).
async fn request_transfer(state: &State, target: &'static str) -> Value {
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            crate::account_transfer_driver::request(
                &store, TRANSFER, &agent, target, "team", "codex", 1.0,
            )
            .await
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
        })
        .await
        .unwrap()
}

async fn stored_transfer(state: &State) -> Value {
    state
        .bridge
        .run(|store| async move {
            let raw: String = store.test_reader()?.query_row(
                "SELECT record FROM runtime_account_transfers WHERE id=?1",
                [TRANSFER],
                |row| row.get(0),
            )?;
            Ok(serde_json::from_str::<Value>(&raw).unwrap())
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn member_busy_at_the_request_moves_at_its_next_wake() {
    let state = State::new().await;
    edit_agent(&state, |row| {
        row.status = studio_runtime_store::AgentStatus::Running;
        row.extra.insert("inFlight".into(), json!(true));
    })
    .await;
    let requested = request_transfer(&state, "target").await;
    let member = &requested["members"][state.agent.as_str()];
    assert_eq!(member["phase"], "waiting");
    let summary = state.record().await["accountTransfer"].clone();
    assert_eq!(summary["waitingCount"], 1);
    assert_eq!(
        summary["waiting"],
        crate::account_transfer::WAITING_FOR_NEXT_TURN,
        "the summary names why the member has not moved"
    );
    // The turn ends; its queued input wakes the agent on its source account.
    edit_agent(&state, |row| {
        row.status = studio_runtime_store::AgentStatus::Queued;
        row.extra.insert("inFlight".into(), json!(false));
    })
    .await;
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;

    assert!(
        peer.writes.try_recv().is_err(),
        "the source account sends no thread request for a moving member"
    );
    let record = state.record().await;
    assert_eq!(record["accountKey"], "target");
    assert_eq!(record["lazyAccountTransfer"]["id"], TRANSFER);
    assert_eq!(record["lazyAccountTransfer"]["sourceAccountKey"], "default");
    let member = stored_transfer(&state).await["members"][state.agent.as_str()].clone();
    assert_eq!(member["phase"], "lazy");
    assert_eq!(member["lazy"], true);
    assert_eq!(record["accountTransfer"]["waitingCount"], 0);
    assert_eq!(
        record["accountTransfer"]["waiting"],
        crate::account_transfer::WAITING_FOR_NEXT_TURN
    );
    assert_eq!(
        state.event_status("input").await,
        "pending",
        "the destination account claims the queued input"
    );
    worker.shutdown().await.unwrap();
}

/// Records each provider read and refuses it before any write.
#[derive(Default)]
struct RefusingReads(std::sync::Mutex<Vec<(String, String)>>);
impl AccountSessionPort for RefusingReads {
    fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
        Some(ConnectionScope {
            account: account.to_owned(),
            connection: format!("{account}-connection"),
            generation: 1,
        })
    }
    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        self.0
            .lock()
            .unwrap()
            .push((request.account.clone(), request.method.as_str().to_owned()));
        Box::pin(async {
            Err(AccountSessionError::Refused(
                "fixture refuses provider reads",
            ))
        })
    }
}

#[tokio::test]
async fn lazy_member_moves_its_history_before_its_input_is_claimed() {
    let state = State::new().await;
    edit_agent(&state, |row| {
        row.extra.insert("accountKey".into(), json!("source"));
        row.extra
            .insert("executionSettingsAccountKey".into(), json!("source"));
    })
    .await;
    let requested = request_transfer(&state, "default").await;
    assert_eq!(requested["members"][state.agent.as_str()]["phase"], "lazy");
    assert_eq!(state.record().await["accountKey"], "default");

    let reads = Arc::new(RefusingReads::default());
    let mut resources = test_resources().unwrap();
    let home = state.root.join("default-home");
    std::fs::create_dir_all(&home).unwrap();
    resources
        .model_catalog
        .retain_accounts(&[json!({"id":"default","home":home,"provider":"codex"})]);
    resources.account_session = Some(reads.clone());
    let mut peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.unwrap();
    }
    state.wake(&worker).await;
    worker_barrier(&worker).await;

    assert!(
        reads
            .0
            .lock()
            .unwrap()
            .contains(&("default".to_owned(), "model/list".to_owned())),
        "the wake starts the lazy move with the destination catalog read"
    );
    assert!(
        peer.writes.try_recv().is_err(),
        "no thread request precedes the history move"
    );
    let member = stored_transfer(&state).await["members"][state.agent.as_str()].clone();
    assert_eq!(member["phase"], "blocked", "the refused move is visible");
    assert_eq!(state.record().await["lazyAccountTransfer"]["id"], TRANSFER);
    assert_eq!(
        state.event_status("input").await,
        "pending",
        "a refused move leaves the input queued"
    );

    // Each later wake holds the blocked member until Retry or Cancel. It does
    // not run the move again (a write and a log line per wake).
    let moves = || {
        state
            .bridge
            .faults
            .operations
            .lock()
            .unwrap()
            .iter()
            .filter(|(name, _, _)| name == "account_transfer_move")
            .count()
    };
    let moves_before = moves();
    let reads_before = reads.0.lock().unwrap().len();
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    assert_eq!(moves(), moves_before, "a blocked member is not moved again");
    assert_eq!(reads.0.lock().unwrap().len(), reads_before);
    assert_eq!(state.event_status("input").await, "pending");
    worker.shutdown().await.unwrap();
}
