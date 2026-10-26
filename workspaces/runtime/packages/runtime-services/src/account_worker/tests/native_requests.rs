//! Native approvals and questions from the account session (live Claude lead
//! c8b8b265, 22 October 2026). The Claude bridge sent
//! `item/tool/requestUserInput` for AskUserQuestion. The account worker
//! answered -32601 "Provider callback is not supported by this runtime", the
//! chat showed "Tool permission request failed", and the agent continued
//! without the user's answer. A native request of a lead must wait as a card
//! until the user answers it through the `/api/answer` runtime.
use super::*;
use crate::account_worker::retained::RetainedSessionPort;
use crate::callback_runtime::{CallbackReplyFuture, CallbackReplyPort, CallbackRuntime};
use crate::provider_callbacks::{CallbackContext, Reply};

const QUESTION: &str = "claude:question-1";
const ORCHESTRATOR_ONLY: &str = "Only the orchestrator can ask the user. Send your question with orchestration_message target=lead; the orchestrator decides whether to contact the user.";

/// The bridge's AskUserQuestion request (`permissions` in bridge.mjs).
fn question(id: &str) -> Value {
    json!({"id":id,"method":"item/tool/requestUserInput","params":{
        "threadId":"fake-thread-0","turnId":"turn","itemId":"toolu_question",
        "questions":[{"id":"0","header":"Disk","question":"Which disk test first?",
            "options":[{"label":"APFS clone","description":"Copy on write"},{"label":"btrfs"}],
            "multiSelect":false,"isOther":true,"isSecret":false}]}})
}

/// The bridge's tool approval request; a Codex command approval has this shape.
fn approval(id: &str) -> Value {
    json!({"id":id,"method":"item/commandExecution/requestApproval","params":{
        "threadId":"fake-thread-0","turnId":"turn","itemId":"toolu_bash",
        "command":"rm -rf target","cwd":"/fixture/project","reason":"Claude requests permission to use Bash"}})
}

/// The registry broker's reply port (`BrokerCallbackReplyPort` in the runtime
/// server) over one account worker session.
struct WorkerReplies(RetainedSessionPort);

impl CallbackReplyPort for WorkerReplies {
    fn has_live_session(&self, context: &CallbackContext) -> bool {
        self.0.serves_callback(context)
    }

    fn deliver<'a>(
        &'a self,
        reply: &'a Reply,
        deadline: std::time::Duration,
    ) -> CallbackReplyFuture<'a> {
        Box::pin(self.0.deliver_callback_reply(reply, deadline))
    }
}

fn answers(runtime_session: RetainedSessionPort, state: &State) -> Arc<CallbackRuntime> {
    Arc::new(CallbackRuntime::new(
        Arc::clone(&state.bridge.store),
        Arc::new(WorkerReplies(runtime_session)),
    ))
}

fn cards(state: &State) -> Vec<Value> {
    let reader = state.bridge.store.test_reader().expect("reader");
    let mut statement = reader
        .prepare("SELECT record FROM runtime_requests ORDER BY rowid")
        .expect("request query");
    statement
        .query_map([], |row| row.get::<_, String>(0))
        .expect("request rows")
        .map(|raw| serde_json::from_str(&raw.expect("request row")).expect("request JSON"))
        .collect()
}

/// Wait for the card of `rpc`. The provider must not get an answer first.
async fn card_of(state: &State, peer: &mut Peer, rpc: impl Into<Value>) -> Value {
    let rpc = rpc.into();
    let mut changes = state.commits();
    let card = async {
        loop {
            if let Some(card) = cards(state).into_iter().find(|card| card["rpcId"] == rpc) {
                return card;
            }
            changes.changed().await.expect("native card commit");
        }
    };
    tokio::select! {
        card = card => card,
        wire = peer.next_write() => panic!("the provider got an answer before the user: {}", wire.value),
    }
}

async fn status_of(state: &State, card: &str) -> String {
    cards(state)
        .into_iter()
        .find(|row| row["id"] == card)
        .and_then(|row| row["status"].as_str().map(str::to_owned))
        .expect("card row")
}

async fn set_lead(state: &State, lead: bool) {
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with::<_, StoreError>(move |tx| {
                    tx.execute(
                        "UPDATE runtime_agents SET record=json_set(record,'$.isLead',json(?2)) WHERE id=?1",
                        rusqlite::params![agent, lead.to_string()],
                    )?;
                    Ok(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
}

/// A running turn on a fresh worker.
async fn running(state: &State) -> (Peer, AccountHandle) {
    let mut peer = Peer::new();
    let worker = peer.worker(state.bridge.clone()).await;
    let start = state.start(&mut peer, &worker).await;
    peer.accepted(&start).await;
    (peer, worker)
}

/// Answer `card` as the user and return the frame the provider received.
async fn answer_once(
    runtime: &Arc<CallbackRuntime>,
    peer: &mut Peer,
    card: &str,
    data: Value,
) -> Value {
    let task = {
        let runtime = Arc::clone(runtime);
        let card = card.to_owned();
        let data = data.clone();
        tokio::spawn(async move { runtime.answer(&card, &data, 30.0).await })
    };
    let wire = peer.next_write().await;
    wire.receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .expect("answer receipt receiver");
    assert_eq!(
        task.await.unwrap().expect("the answer is delivered"),
        json!({"status":"answered"})
    );
    // The same answer again replays its receipt and writes nothing.
    assert_eq!(
        runtime.answer(card, &data, 31.0).await.expect("replay"),
        json!({"status":"answered","replayed":true})
    );
    assert!(peer.writes.try_recv().is_err(), "one answer per request");
    wire.value
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_lead_question_waits_for_the_user_and_its_answer_reaches_the_provider_once() {
    let state = State::new().await;
    let (mut peer, mut worker) = running(&state).await;
    let cursor = peer.frame(question(QUESTION)).await;
    peer.acknowledged(cursor).await;
    let card = card_of(&state, &mut peer, QUESTION).await;
    assert_eq!(card["method"], "item/tool/requestUserInput");
    assert_eq!(card["status"], "pending");
    assert_eq!(card["agent"], state.agent);
    assert_eq!(
        card["params"]["questions"][0]["question"],
        "Which disk test first?"
    );
    // The card owns the answer; the worker owes nothing more for this request.
    callbacks_retired(&state).await;
    assert!(peer.writes.try_recv().is_err(), "nobody answered yet");
    assert_eq!(state.record().await["status"], "approval");

    let runtime = answers(worker.retained_session(), &state);
    let id = card["id"].as_str().unwrap();
    let written = answer_once(
        &runtime,
        &mut peer,
        id,
        json!({"answers":{"0":{"answers":["APFS clone"]}}}),
    )
    .await;
    assert_eq!(
        written,
        json!({"id":QUESTION,"result":{"answers":{"0":{"answers":["APFS clone"]}}}})
    );
    assert_eq!(status_of(&state, id).await, "answered");
    assert_eq!(state.record().await["status"], "running");
    worker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_worker_question_gets_the_orchestrator_refusal() {
    let state = State::new().await;
    let (mut peer, mut worker) = running(&state).await;
    set_lead(&state, false).await;
    peer.frame(question(QUESTION)).await;
    let wire = peer.next_write().await;
    assert_eq!(wire.value["id"], QUESTION);
    assert_eq!(wire.value["error"]["code"], -32600);
    assert_eq!(wire.value["error"]["message"], ORCHESTRATOR_ONLY);
    wire.receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .unwrap();
    callbacks_retired(&state).await;
    assert!(cards(&state).is_empty(), "a refusal stores no card");
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_approval_card_carries_the_user_decision_and_resolution_closes_another() {
    let state = State::new().await;
    let (mut peer, mut worker) = running(&state).await;
    peer.frame(approval("claude:approval-1")).await;
    let card = card_of(&state, &mut peer, "claude:approval-1").await;
    callbacks_retired(&state).await;
    let runtime = answers(worker.retained_session(), &state);
    let id = card["id"].as_str().unwrap();
    let written = answer_once(&runtime, &mut peer, id, json!({"decision":"accept"})).await;
    assert_eq!(
        written,
        json!({"id":"claude:approval-1","result":{"decision":"accept"}})
    );
    assert_eq!(status_of(&state, id).await, "answered");

    // The provider withdraws a second approval (the bridge after its timeout,
    // Codex at a turn transition). A resolution never counts as an approval.
    peer.frame(approval("claude:approval-2")).await;
    let second = card_of(&state, &mut peer, "claude:approval-2").await;
    callbacks_retired(&state).await;
    assert_eq!(state.record().await["status"], "approval");
    let cursor = peer
        .frame(json!({"method":"serverRequest/resolved","params":{
            "threadId":"fake-thread-0","requestId":"claude:approval-2"}}))
        .await;
    peer.acknowledged(cursor).await;
    let second = second["id"].as_str().unwrap();
    assert_eq!(status_of(&state, second).await, "resolved");
    assert_eq!(state.record().await["status"], "running");
    assert!(
        runtime
            .answer(second, &json!({"decision":"accept"}), 32.0)
            .await
            .is_err(),
        "a resolved request takes no answer"
    );
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}

/// A Codex MCP elicitation may name no turn; the thread's active turn owns it.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_elicitation_without_a_turn_id_is_answered_with_the_form_content() {
    let state = State::new().await;
    let (mut peer, mut worker) = running(&state).await;
    peer.frame(
        json!({"id":7,"method":"mcpServer/elicitation/request","params":{
        "threadId":"fake-thread-0","turnId":null,"serverName":"docs","mode":"form",
        "message":"Which branch?","requestedSchema":{"type":"object",
            "properties":{"branch":{"type":"string","title":"Branch"}},"required":["branch"]}}}),
    )
    .await;
    let card = card_of(&state, &mut peer, 7).await;
    callbacks_retired(&state).await;
    let runtime = answers(worker.retained_session(), &state);
    let written = answer_once(
        &runtime,
        &mut peer,
        card["id"].as_str().unwrap(),
        json!({"decision":"accept","content":{"branch":"main"}}),
    )
    .await;
    assert_eq!(
        written,
        json!({"id":7,"result":{"action":"accept","content":{"branch":"main"}}})
    );
    worker.shutdown().await.unwrap();
}

/// A replacement worker attaches to the same provider generation with a new
/// connection id (stop_after_reattach.rs). The bridge still waits.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_pending_question_survives_a_worker_reattach_and_is_answered_once() {
    let state = State::new().await;
    let (mut first, mut first_worker) = running(&state).await;
    first.frame(question(QUESTION)).await;
    let card = card_of(&state, &mut first, QUESTION).await;
    callbacks_retired(&state).await;
    first_worker.shutdown().await.unwrap();
    state
        .bridge
        .run(|store| async move {
            scheduler::lifecycle_transaction_with(&store, |tx| {
                store::disconnect_provider_generation(tx, "default", 1, false)?;
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();

    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    answer_adopted_turn_check(&mut peer).await;
    worker_barrier(&worker).await;
    assert_ne!(
        card["connectionId"],
        json!(worker.retained_session().scope().connection),
        "the connection id changed"
    );
    let id = card["id"].as_str().unwrap();
    assert_eq!(status_of(&state, id).await, "pending");
    assert_eq!(cards(&state).len(), 1, "the request is not admitted again");
    assert!(
        peer.writes.try_recv().is_err(),
        "the restart answers nothing"
    );

    let runtime = answers(worker.retained_session(), &state);
    let written = answer_once(
        &runtime,
        &mut peer,
        id,
        json!({"answers":{"0":{"answers":["btrfs"]}}}),
    )
    .await;
    assert_eq!(
        written,
        json!({"id":QUESTION,"result":{"answers":{"0":{"answers":["btrfs"]}}}})
    );
    assert_eq!(status_of(&state, id).await, "answered");
    worker.shutdown().await.unwrap();
}

/// A new provider generation is a new process: the old request can never be
/// answered, so its card expires instead of waiting forever.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_new_provider_generation_expires_the_old_question() {
    let state = State::new().await;
    let (mut first, mut first_worker) = running(&state).await;
    first.frame(question(QUESTION)).await;
    let card = card_of(&state, &mut first, QUESTION).await;
    callbacks_retired(&state).await;
    first_worker.shutdown().await.unwrap();

    let peer = Peer::identity("default", 2, None);
    let mut worker = peer.worker(state.bridge.clone()).await;
    worker_barrier(&worker).await;
    assert_eq!(
        status_of(&state, card["id"].as_str().unwrap()).await,
        "expired"
    );
    worker.shutdown().await.unwrap();
}
