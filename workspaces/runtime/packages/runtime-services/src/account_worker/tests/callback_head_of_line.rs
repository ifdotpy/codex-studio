//! One provider callback whose handler does not finish must not stop the
//! session from reading the provider journal. The session acknowledges the
//! journal only as a contiguous settled prefix, so an unanswered callback holds
//! every later entry unsettled; once the unsettled bound is reached the
//! session stops reading, and a concurrent request's reply is never read.
//!
//! Both scenarios hold agent A's `item/tool/call` handler, then send more than
//! the unsettled bound of provider frames for agent B.
#![cfg(unix)]
use super::*;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// Bounds every wait for progress; a stalled session fails an assertion
/// instead of hanging. Unit: wall time.
const PROGRESS_DEADLINE: Duration = Duration::from_secs(10);
/// How long the flood may take before the session counts as having stopped
/// reading it; only the failing path waits this long. Unit: wall time.
const STALL_PROBE: Duration = Duration::from_secs(2);
/// Deadline of a provider request in the fixture (`timeout_ms` of the peer).
const REQUEST_DEADLINE: Duration = Duration::from_secs(60);
/// Provider frames for agent B sent behind the held callback: more than the
/// session's unsettled bound plus every lane that buffers frames ahead of it
/// (the peer's journal lane and the session's 16-slot event lane), so a held
/// cursor stops the journal intake before the last frame is accepted.
const FLOOD_FRAMES: u64 =
    (crate::provider_session_task::MAX_UNSETTLED + 3 * PEER_JOURNAL_CAPACITY) as u64;
const HELD_CALLBACK: &str = "held-callback";
/// Both thread request hold reasons (resume and start) contain this text.
const UNKNOWN_HOLD: &str = "outcome is unknown; input remains pending";

/// A clone of the fixture agent on its own thread; no thread means a new one.
async fn clone_agent(state: &State, id: &str, thread: Option<&str>, input: &str) {
    let first = state.agent.clone();
    let (id_row, thread) = (id.to_owned(), thread.map(str::to_owned));
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(first.as_str()))?.unwrap();
                row.id = AgentId::from(id_row.as_str());
                row.root_id = AgentId::from(id_row.as_str());
                match thread {
                    Some(thread) => row.extra.insert("threadId".into(), json!(thread)),
                    None => row.extra.remove("threadId"),
                };
                AgentRepository::put(tx, &row)?;
                Ok::<_, WorkerError>(())
            })
            .await
        })
        .await
        .unwrap();
    state.enqueue_for(id, input).await;
}

async fn record_of(state: &State, id: &str) -> Value {
    let id = id.to_owned();
    state
        .bridge
        .run(move |store| async move {
            let raw: String = store.test_reader()?.query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [id],
                |r| r.get(0),
            )?;
            Ok(serde_json::from_str(&raw).unwrap())
        })
        .await
        .unwrap()
}

async fn wake(state: &State, worker: &AccountHandle, agent: &str) {
    worker
        .send(AccountCommand::Wake {
            agent_id: agent.to_owned(),
            epoch: state.epoch,
        })
        .await
        .unwrap();
}

/// Wake `agent`, resume its thread and return the written `turn/start`.
async fn start_turn(
    state: &State,
    peer: &mut Peer,
    worker: &AccountHandle,
    agent: &str,
    thread: &str,
) -> Value {
    wake(state, worker, agent).await;
    let resume = peer.write("thread/resume").await;
    peer.reply(&resume, json!({"thread":{"id":thread}})).await;
    peer.write("turn/start").await
}

/// Handler of the held callback: it reports its start and waits for release.
struct HeldHandler {
    started: tokio::sync::mpsc::UnboundedSender<()>,
    release: tokio::sync::Mutex<Option<oneshot::Receiver<()>>>,
}

impl crate::provider_callbacks::tools::StatusWatchPort for HeldHandler {
    fn status<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _actor: &'a crate::managed_host_tools::Actor,
        _args: &'a Value,
        _now: f64,
    ) -> futures_util::future::BoxFuture<'a, Result<Value, crate::managed_host_tools::Error>> {
        Box::pin(async move {
            let unavailable = crate::managed_host_tools::Error::Unavailable("test/held-handler");
            self.started.send(()).map_err(|_| unavailable)?;
            let release = self.release.lock().await.take();
            if let Some(release) = release {
                release.await.map_err(|_| {
                    crate::managed_host_tools::Error::Unavailable("test/held-handler")
                })?;
            }
            Ok(json!({"status":"answered"}))
        })
    }

    fn rules<'a>(
        &'a self,
        _store: &'a RuntimeStore,
        _actor: &'a crate::managed_host_tools::Actor,
        _args: &'a Value,
        _request_id: &'a str,
        _now: f64,
    ) -> futures_util::future::BoxFuture<
        'a,
        Result<
            crate::provider_callbacks::tools::RulesActionResult,
            crate::managed_host_tools::Error,
        >,
    > {
        Box::pin(async {
            Ok(crate::provider_callbacks::tools::RulesActionResult {
                value: Value::Null,
                cancel_monitor_ids: Vec::new(),
            })
        })
    }
}

/// Agent A runs a turn whose `item/tool/call` handler is held; agent B runs
/// a turn; agent C is queued for a new thread.
struct Stall {
    state: State,
    peer: Peer,
    worker: AccountHandle,
    release: Option<oneshot::Sender<()>>,
    a: String,
}

async fn stalled() -> Stall {
    let state = State::new().await;
    let a = state.agent.clone();
    clone_agent(&state, "agent-b", Some("fake-thread-1"), "b-input").await;
    clone_agent(&state, "agent-c", None, "c-input").await;
    let mut peer = Peer::new();
    let worker = peer.worker(state.bridge.clone()).await;
    let a_start = start_turn(&state, &mut peer, &worker, &a, "fake-thread-0").await;
    peer.accepted(&a_start).await;
    let b_start = start_turn(&state, &mut peer, &worker, "agent-b", "fake-thread-1").await;
    peer.reply(&b_start, json!({"turn":{"id":"turn-b"}})).await;
    let agent = a.clone();
    state
        .bridge
        .run(move |store| async move {
            store.transaction_with::<_, StoreError>(move |tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.startAttempt.connectionId','1') WHERE id=?1",
                    [agent],
                )?;
                Ok(())
            }).await?;
            Ok(())
        })
        .await
        .expect("bind the live connection");
    let (started, mut callback_started) = tokio::sync::mpsc::unbounded_channel();
    let (release, held) = oneshot::channel();
    state.bridge.set_managed_tool_runtime(
        crate::provider_callbacks::ManagedToolRuntime::from_ports_for_test(
            crate::provider_callbacks::ManagedToolRuntimePorts {
                status_watch: Some(Arc::new(HeldHandler {
                    started,
                    release: tokio::sync::Mutex::new(Some(held)),
                })),
                child_creation: None,
                monitor_create: None,
                interrupt: None,
                monitor_input: None,
                monitor_cancel: None,
                work_action: None,
                late: Default::default(),
            },
        ),
    );
    peer.frame(json!({
        "id":HELD_CALLBACK,
        "method":"item/tool/call",
        "params":{"threadId":"fake-thread-0","turnId":"turn","callId":"held",
            "tool":"orchestration_status","arguments":{}}
    }))
    .await;
    tokio::time::timeout(PROGRESS_DEADLINE, callback_started.recv())
        .await
        .expect("the handler of agent A's callback starts")
        .expect("handler start signal");
    Stall {
        state,
        peer,
        worker,
        release: Some(release),
        a,
    }
}

/// Provider notifications of agent B's running turn.
fn b_flood() -> impl Iterator<Item = Value> {
    (0..FLOOD_FRAMES).map(|index| {
        json!({"method":"thread/tokenUsage/updated","params":{"threadId":"fake-thread-1","turnId":"turn-b",
            "tokenUsage":{"total":{"inputTokens":index,"cachedInputTokens":0,"reasoningOutputTokens":0,"outputTokens":0,"totalTokens":index},
            "last":{"inputTokens":1,"cachedInputTokens":0,"reasoningOutputTokens":0,"outputTokens":0,"totalTokens":1}}}})
    })
}

fn b_completed() -> Value {
    json!({"method":"turn/completed","params":{"threadId":"fake-thread-1","turn":{"id":"turn-b","status":"completed"}}})
}

/// Sends `frames` behind the held callback from a task, as a provider writes
/// them without waiting for the session; `sent` counts the frames the
/// session accepted into its journal lane. The peer's cursor moves past them.
fn send_frames(
    peer: &mut Peer,
    frames: Vec<Value>,
    sent: Arc<AtomicU64>,
) -> tokio::task::JoinHandle<()> {
    let (entries, clock, generation) = (peer.entries.clone(), peer.clock.clone(), peer.generation);
    let first = peer.cursor + 1;
    peer.cursor += frames.len() as u64;
    tokio::spawn(async move {
        for (offset, frame) in frames.iter().enumerate() {
            let entry = journal_frame(&clock, generation, first + offset as u64, frame);
            if entries.send(entry).await.is_err() {
                return;
            }
            sent.fetch_add(1, Ordering::SeqCst);
        }
    })
}

/// Let the request deadline pass and report what the stall did to agent C.
async fn stall_report(stall: &Stall, sent: &AtomicU64, total: u64) -> String {
    tokio::time::pause();
    tokio::time::advance(REQUEST_DEADLINE + Duration::from_secs(1)).await;
    tokio::time::resume();
    let error = record_of(&stall.state, "agent-c").await["error"].clone();
    format!(
        "the session read {} of {total} provider frames behind agent A's unanswered callback; \
         agent C's error after the request deadline: {error}",
        sent.load(Ordering::SeqCst)
    )
}

/// Reads writes until one is `method`; the others are returned in order.
async fn write_of(peer: &mut Peer, method: &str) -> (Value, Vec<Value>) {
    let mut others = Vec::new();
    loop {
        let wire = peer.next_write().await;
        let _ignored = wire.receipt.send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        });
        if wire.value["method"] == method {
            return (wire.value, others);
        }
        others.push(wire.value);
    }
}

/// The answer of the held callback, written once after the handler returns.
async fn assert_answer_written_once(stall: &mut Stall, last_cursor: u64) {
    let answer = tokio::time::timeout(PROGRESS_DEADLINE, stall.peer.next_write())
        .await
        .expect("agent A's callback answer is written after its handler returns");
    assert_eq!(answer.value["id"], HELD_CALLBACK, "{}", answer.value);
    assert_eq!(answer.value["result"]["success"], true, "{}", answer.value);
    answer
        .receipt
        .send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        })
        .ok()
        .expect("callback write receipt receiver");
    tokio::time::timeout(
        PROGRESS_DEADLINE,
        stall.peer.acknowledged_at_least(last_cursor),
    )
    .await
    .expect("the journal is acknowledged up to its last frame once the callback is answered");
    worker_barrier(&stall.worker).await;
    assert!(
        stall.peer.writes.try_recv().is_err(),
        "the callback answer is written exactly once"
    );
}

#[tokio::test]
async fn held_callback_does_not_stop_other_agents_or_a_new_thread() {
    let mut stall = stalled().await;
    // Agent C starts a new thread; its reply arrives behind the flood.
    wake(&stall.state, &stall.worker, "agent-c").await;
    let thread_start = stall.peer.write("thread/start").await;
    let mut frames: Vec<Value> = b_flood().collect();
    frames.push(b_completed());
    frames.push(json!({"id":thread_start["id"],"result":{"thread":{"id":"fake-thread-2"}}}));
    let (total, sent) = (frames.len() as u64, Arc::new(AtomicU64::new(0)));
    let sender = send_frames(&mut stall.peer, frames, sent.clone());
    let last_cursor = stall.peer.cursor;

    let turn_start = tokio::time::timeout(PROGRESS_DEADLINE, async {
        write_of(&mut stall.peer, "turn/start").await
    })
    .await;
    let (turn_start, _) = match turn_start {
        Ok(found) => found,
        Err(_) => panic!(
            "agent C never reached turn/start: {}",
            stall_report(&stall, &sent, total).await
        ),
    };
    assert_eq!(turn_start["params"]["threadId"], "fake-thread-2");
    sender.await.unwrap();
    let b = record_of(&stall.state, "agent-b").await;
    assert_eq!(b["lastCompletedTurn"], "turn-b", "agent B's turn: {b}");
    for agent in [stall.a.as_str(), "agent-b", "agent-c"] {
        let error = record_of(&stall.state, agent).await["error"].clone();
        assert!(
            !error.as_str().unwrap_or_default().contains(UNKNOWN_HOLD),
            "{agent} is held with an unknown outcome: {error}"
        );
    }

    stall
        .release
        .take()
        .unwrap()
        .send(())
        .expect("release agent A's handler");
    assert_answer_written_once(&mut stall, last_cursor).await;
    stall.worker.shutdown().await.unwrap();
}

#[tokio::test]
async fn held_callback_is_answered_and_another_agent_stops_behind_an_active_journal_barrier() {
    let Stall {
        state: _state,
        mut peer,
        mut worker,
        release,
        a: _,
    } = stalled().await;
    let release = release.expect("held handler");
    // Keep B's turn active so its interrupt and A's answer both need the lane.
    let frames: Vec<Value> = b_flood().collect();
    let sent = Arc::new(AtomicU64::new(0));
    let sender = send_frames(&mut peer, frames, sent.clone());
    let last_cursor = peer.cursor;
    // A reading session takes the whole flood; a stalled one leaves frames
    // queued in its lane, which the barrier then waits for.
    let still_sending = tokio::time::timeout(STALL_PROBE, sender).await.is_err();

    let barrier = tokio::time::timeout(PROGRESS_DEADLINE, worker.session_journal_barrier());
    let rest = async {
        worker
            .send(AccountCommand::Stop {
                agent_id: "agent-b".into(),
            })
            .await
            .unwrap();
        release.send(()).expect("release agent A's handler");
        // The callback answer and the interrupt both go through the session
        // command lane, which an active barrier blocks.
        let mut methods = Vec::new();
        for _ in 0..2 {
            let wire = tokio::time::timeout(PROGRESS_DEADLINE, peer.next_write())
                .await
                .unwrap_or_else(|_| {
                    panic!(
                        "a write is missing behind the journal barrier (flood still sending: {still_sending}, \
                         {} frames read); got {methods:?}",
                        sent.load(Ordering::SeqCst)
                    )
                });
            methods.push(if wire.value["id"] == HELD_CALLBACK {
                assert_eq!(wire.value["result"]["success"], true);
                "callback answer".to_owned()
            } else {
                assert_eq!(wire.value["method"], "turn/interrupt");
                assert_eq!(wire.value["params"]["threadId"], "fake-thread-1");
                wire.value["method"].as_str().unwrap_or("?").to_owned()
            });
            let _ignored = wire.receipt.send(CommandReceipt::Written {
                remote_id: None,
                duplicate: false,
            });
        }
        methods.sort();
        assert_eq!(
            methods,
            ["callback answer", "turn/interrupt"],
            "the answer of agent A's held callback and the stop of agent B are both written"
        );
    };
    let (barrier, ()) = tokio::join!(barrier, rest);
    barrier
        .expect("the journal barrier completes")
        .expect("the journal barrier is not refused");
    tokio::time::timeout(PROGRESS_DEADLINE, peer.acknowledged_at_least(last_cursor))
        .await
        .expect("the journal is acknowledged up to its last frame");
    worker_barrier(&worker).await;
    assert!(
        peer.writes.try_recv().is_err(),
        "the answer and the interrupt are written exactly once"
    );
    worker.shutdown().await.unwrap();
}
