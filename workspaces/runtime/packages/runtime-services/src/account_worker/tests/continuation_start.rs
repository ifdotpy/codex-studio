//! A scheduled continuation starts its turn through the account worker, for
//! both providers: a child turn fails with a capacity or usage error, the
//! continuation tick runs once its delay has elapsed, and the woken worker
//! writes a new `turn/start` through the ordinary queued start path (start
//! gate, auth gate, claim). The parent then gets exactly one `child_result`.
use super::auth_hold::{Account, agents, record_of, wake_agent};
use super::*;
use crate::usage_resume::LimitsObservation;
use studio_runtime_providers::ProviderKind;

/// Bounds each wait for a worker write or settlement; unit: wall time.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(60);
const LEAD: &str = "lead";
const FAILED_TURN: &str = "turn-1";
const RETRIED_TURN: &str = "turn-2";

/// Make the fixture agent a child of an auto-waking lead on its own thread.
async fn child_of_lead(state: &State) {
    let child = state.agent.clone();
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(child.as_str()))?.unwrap();
                let mut lead = row.clone();
                lead.id = AgentId::from(LEAD);
                lead.root_id = AgentId::from(LEAD);
                lead.status = studio_runtime_store::AgentStatus::Completed;
                for (key, value) in [
                    ("threadId", json!("lead-thread")),
                    ("inFlight", json!(false)),
                    ("concurrency", json!(8)),
                ] {
                    lead.extra.insert(key.into(), value);
                }
                lead.extra.remove("startAttempt");
                AgentRepository::put(tx, &lead)?;
                row.root_id = AgentId::from(LEAD);
                row.extra.insert("parentId".into(), json!(LEAD));
                row.extra.insert("isLead".into(), json!(false));
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
}

/// Optionally preset the child's capacity retry count, so the next retry is its last.
async fn set_retry_count(state: &State, count: u64) {
    let child = state.agent.clone();
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut row = AgentRepository::get(tx, &AgentId::from(child.as_str()))?.unwrap();
                row.extra.insert("capacityRetryCount".into(), json!(count));
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
}

/// The lead's `child_result` events: (id, payload).
async fn child_results(state: &State) -> Vec<(String, Value)> {
    state
        .bridge
        .run(|store| async move {
            let connection = store.test_reader()?;
            let mut statement = connection.prepare(
                "SELECT id,text FROM runtime_events WHERE agent=?1 AND kind='child_result' ORDER BY rowid",
            )?;
            let rows = statement
                .query_map([LEAD], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            Ok(rows
                .into_iter()
                .map(|(id, text)| (id, serde_json::from_str(&text).expect("payload")))
                .collect())
        })
        .await
        .unwrap()
}

/// Read a follow-up by its queue identity (annotation helpers add another prefix).
async fn followup_status(state: &State, id: &str) -> String {
    let id = studio_runtime_store::QueueEventId::from(id);
    state
        .bridge
        .run(move |store| async move {
            Ok(
                studio_runtime_store::QueueEventRepository::get(&store.test_reader()?, &id)?
                    .expect("capacity follow-up exists")
                    .status,
            )
        })
        .await
        .unwrap()
}

/// The next `turn/start`, resuming the thread first when this connection has
/// not loaded it. Fails at [`DEADLINE`] when the worker writes nothing.
async fn next_turn_start(peer: &mut Peer) -> Value {
    let mut wire = tokio::time::timeout(DEADLINE, peer.writes.recv())
        .await
        .expect("the worker writes a turn/start")
        .expect("worker submits request");
    if wire.value["method"] == "thread/resume" {
        let resume = wire.value;
        let _ignored = wire.receipt.send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        });
        let thread = resume["params"]["threadId"].clone();
        peer.reply(&resume, json!({"thread":{"id":thread}})).await;
        wire = tokio::time::timeout(DEADLINE, peer.writes.recv())
            .await
            .expect("the worker writes a turn/start")
            .expect("worker submits request");
    }
    assert_eq!(wire.value["method"], "turn/start", "{}", wire.value);
    let _ignored = wire.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    wire.value
}

/// Accept `start` as `turn` and complete it with `status` and `error`.
async fn finish(peer: &mut Peer, start: &Value, turn: &str, status: &str, error: Value) {
    peer.reply(start, json!({"turn":{"id":turn}})).await;
    let cursor = peer
        .frame(
            json!({"method":"turn/completed","params":{"threadId":"fake-thread-0",
            "turn":{"id":turn,"status":status,"error":error}}}),
        )
        .await;
    peer.acknowledged(cursor).await;
}

/// Run the continuation tick for the child at the record's due time plus one
/// second, as the server's continuation scheduler does once the delay elapsed.
async fn tick_when_due(state: &State, field: &'static str, limits: Option<LimitsObservation>) {
    let record = record_of(state, &state.agent).await;
    let due = record[field]["dueAt"]
        .as_f64()
        .unwrap_or_else(|| panic!("{field} is due: {record}"));
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let staged = crate::continuation_runtime::stage_tick(
                        tx,
                        &agent,
                        due + 1.0,
                        limits.as_ref(),
                    )
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                    assert!(staged.changed(), "the due tick changes the continuation");
                    Ok::<_, StoreError>(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
}

fn capacity_error() -> Value {
    json!({"message":"Server overloaded","codexErrorInfo":"serverOverloaded"})
}

/// Start the child's first turn and fail it with `error`.
async fn fail_first_turn(state: &State, peer: &mut Peer, worker: &AccountHandle, error: Value) {
    wake_agent(state, worker, &state.agent).await;
    let start = next_turn_start(peer).await;
    finish(peer, &start, FAILED_TURN, "failed", error).await;
    let record = record_of(state, &state.agent).await;
    assert_eq!(record["status"], "failed", "{record}");
}

async fn open(state: &State, account: &Account) -> (Peer, AccountHandle) {
    let mut peer = Peer::new();
    peer.wire = account.kind.wire();
    let worker = peer
        .open_worker_with_resources(state.bridge.clone(), account.resources())
        .await;
    (peer, worker)
}

async fn fixture(kind: ProviderKind) -> (State, Account) {
    let state = State::new().await;
    agents(&state, kind, 0).await;
    child_of_lead(&state).await;
    let account = Account::new(&state, kind);
    (state, account)
}

/// A capacity failure schedules a retry; once due, the worker starts it,
/// the retried turn completes, the retry ends `finished`, and the lead gets
/// the retried turn's result once.
#[tokio::test]
async fn a_due_capacity_retry_starts_its_turn_and_reports_once() {
    for kind in ProviderKind::ALL {
        let (state, account) = fixture(kind).await;
        let (mut peer, mut worker) = open(&state, &account).await;
        fail_first_turn(&state, &mut peer, &worker, capacity_error()).await;
        let record = record_of(&state, &state.agent).await;
        assert_eq!(record["capacityRetry"]["status"], "scheduled", "{kind:?}");
        assert!(child_results(&state).await.is_empty(), "{kind:?}");

        tick_when_due(&state, "capacityRetry", None).await;
        wake_agent(&state, &worker, &state.agent).await;
        let start = next_turn_start(&mut peer).await;
        finish(&mut peer, &start, RETRIED_TURN, "completed", Value::Null).await;

        let record = record_of(&state, &state.agent).await;
        assert_eq!(record["status"], "completed", "{kind:?}: {record}");
        assert_eq!(record["capacityRetry"]["status"], "finished", "{kind:?}");
        assert_eq!(record["capacityRetry"]["acceptedTurnId"], RETRIED_TURN);
        assert_eq!(record["capacityRetryCount"], 1, "{kind:?}");
        let results = child_results(&state).await;
        assert_eq!(results.len(), 1, "{kind:?}: {results:?}");
        assert_eq!(
            results[0].0,
            format!("child:{}:{RETRIED_TURN}", state.agent)
        );
        assert_eq!(results[0].1["status"], "completed", "{kind:?}");
        worker.shutdown().await.unwrap();
    }
}

/// Crash at the durable submission boundary before a write/reply receipt. Both
/// providers restore the exact ticket, expose Unknown, and never send the input again.
#[tokio::test]
async fn capacity_retry_restart_unknown_reports_once_without_resending() {
    for (kind, generation) in ProviderKind::ALL
        .into_iter()
        .flat_map(|kind| [1, 2].map(move |generation| (kind, generation)))
    {
        let (state, account) = fixture(kind).await;
        let (mut peer, mut worker) = open(&state, &account).await;
        fail_first_turn(&state, &mut peer, &worker, capacity_error()).await;
        worker.shutdown().await.unwrap();
        tick_when_due(&state, "capacityRetry", None).await;
        let agent = state.agent.clone();
        let epoch = state.epoch;
        let resources = account.resources();
        let id = state
            .bridge
            .run(move |store| async move {
                let intent = store::claim(Arc::clone(&store), "default", &agent, epoch)
                    .await?
                    .expect("retry claimed");
                let operation = format!("account:default:{}:turn/start", intent.attempt_id);
                let request = RecoveredRequest {
                    ticket: studio_runtime_providers::RequestTicket {
                        id: RequestId::String(operation.clone()),
                        generation: 1,
                        method: Method::TurnStart,
                    },
                    operation_id: OperationId(operation.clone()),
                    thread_id: intent.thread_id.clone(),
                    lightweight: false,
                    deadline_ms: DEADLINE.as_millis().try_into().unwrap(),
                    unknown: false,
                    remote_id: None,
                };
                let prepared = preparation::prepare(
                    Arc::clone(&store),
                    &resources,
                    &intent,
                    Method::TurnStart,
                    1,
                )
                .await?;
                let id = request.ticket.id.clone();
                assert!(
                    store::submitted(
                        &store,
                        Pending {
                            thread: prepared.thread,
                            compiled: prepared.compiled,
                            intent: prepared.intent,
                            method: Method::TurnStart.as_str().into(),
                            operation,
                        },
                        "default",
                        request,
                        store::SubmissionEvidence {
                            turn: None,
                            submission: prepared.submission,
                            guard: Some(prepared.guard),
                            connection_id: "test-connection".into(),
                            effect: None,
                        },
                    )
                    .await?
                );
                Ok(id)
            })
            .await
            .unwrap();
        let before = record_of(&state, &state.agent).await;
        let retry_id = before["capacityRetry"]["id"].as_str().unwrap().to_owned();
        let followup = format!(
            "{}{retry_id}",
            crate::capacity_retry::CAPACITY_FOLLOWUP_PREFIX
        );
        assert_eq!(before["capacityRetry"]["status"], "starting");
        assert!(child_results(&state).await.is_empty());

        // A new bridge is the backend restart boundary. Repeated recovery must
        // keep both the ticket and its one parent notice.
        let bridge = StoreBridge::new(state.bridge.store.clone()).await.unwrap();
        for _ in 0..2 {
            bridge
                .run(move |store| async move {
                    store::recover(&store, "default", Generation(generation), true).await?;
                    Ok(())
                })
                .await
                .unwrap();
            let record = record_of(&state, &state.agent).await;
            assert_eq!(
                record["capacityRetry"]["status"], "unknown",
                "{kind:?}: {record}"
            );
            assert_eq!(
                super::capacity_retry::retry_of(&state, &retry_id).await["status"],
                "unknown"
            );
            assert_eq!(followup_status(&state, &followup).await, "uncertain");
            let results = child_results(&state).await;
            assert_eq!(results.len(), 1, "{kind:?}: {results:?}");
            assert_eq!(results[0].0, format!("hold:{retry_id}:unknown"));
            assert!(results[0].1["reason"].as_str().unwrap().contains("unknown"));
        }
        let mut peer = Peer::identity("default", generation, None);
        peer.wire = kind.wire();
        let mut worker = peer
            .open_worker_with_resources(bridge, account.resources())
            .await;
        if generation == 1 {
            // A positive late receipt refines the same retry and batch.
            let cursor = peer
                .frame(json!({"id":id,"result":{"turn":{"id":RETRIED_TURN}}}))
                .await;
            peer.acknowledged(cursor).await;
            let record = record_of(&state, &state.agent).await;
            assert_eq!(
                record["capacityRetry"]["acceptedTurnId"], RETRIED_TURN,
                "{kind:?}: {record}"
            );
            assert_eq!(record["capacityRetryCount"], 1);
            assert_eq!(followup_status(&state, &followup).await, "delivered");
            assert_eq!(child_results(&state).await.len(), 1);
        }
        worker.shutdown().await.unwrap();
        while let Ok(write) = peer.writes.try_recv() {
            assert_ne!(
                write.value["method"], "turn/start",
                "{kind:?}: {}",
                write.value
            );
        }
    }
}

/// The last allowed retry fails again: the retry is exhausted and the lead
/// gets the retried turn's failure once.
#[tokio::test]
async fn an_exhausted_capacity_retry_reports_the_last_failure_once() {
    for kind in ProviderKind::ALL {
        let (state, account) = fixture(kind).await;
        set_retry_count(&state, 3).await;
        let (mut peer, mut worker) = open(&state, &account).await;
        fail_first_turn(&state, &mut peer, &worker, capacity_error()).await;
        assert!(child_results(&state).await.is_empty(), "{kind:?}");

        tick_when_due(&state, "capacityRetry", None).await;
        wake_agent(&state, &worker, &state.agent).await;
        let start = next_turn_start(&mut peer).await;
        finish(&mut peer, &start, RETRIED_TURN, "failed", capacity_error()).await;

        let record = record_of(&state, &state.agent).await;
        assert_eq!(record["status"], "failed", "{kind:?}: {record}");
        assert_eq!(record["capacityRetry"]["status"], "exhausted", "{kind:?}");
        assert_eq!(record["capacityRetry"]["turnId"], RETRIED_TURN);
        let results = child_results(&state).await;
        assert_eq!(results.len(), 1, "{kind:?}: {results:?}");
        assert_eq!(
            results[0].0,
            format!("child:{}:{RETRIED_TURN}", state.agent)
        );
        assert_eq!(results[0].1["status"], "failed", "{kind:?}");
        assert_eq!(results[0].1["result"], "Server overloaded", "{kind:?}");
        worker.shutdown().await.unwrap();
    }
}

/// A usage-limit failure schedules a resume; once due with fresh allowed
/// limits, the worker starts the continuation turn and the lead gets its
/// result once.
#[tokio::test]
async fn a_due_usage_resume_starts_its_turn_and_reports_once() {
    for kind in ProviderKind::ALL {
        let (state, account) = fixture(kind).await;
        let (mut peer, mut worker) = open(&state, &account).await;
        let usage = json!({"message":"Usage limit reached","codexErrorInfo":"usageLimitExceeded"});
        fail_first_turn(&state, &mut peer, &worker, usage).await;
        let record = record_of(&state, &state.agent).await;
        assert_eq!(record["usageResume"]["status"], "scheduled", "{kind:?}");
        assert!(child_results(&state).await.is_empty(), "{kind:?}");

        let due = record["usageResume"]["dueAt"].as_f64().expect("due") + 1.0;
        let limits = LimitsObservation::from_record(
            json!({"accountKey":"default","at":due,"error":null,
                "data":{"rateLimits":{"primary":{"usedPercent":10.0,"resetsAt":due + 3600.0}}}}),
            None,
        )
        .expect("limits");
        tick_when_due(&state, "usageResume", Some(limits)).await;
        wake_agent(&state, &worker, &state.agent).await;
        let start = next_turn_start(&mut peer).await;
        assert!(
            start["params"]["input"]
                .to_string()
                .contains("usage or rate limit"),
            "{kind:?}: {start}"
        );
        finish(&mut peer, &start, RETRIED_TURN, "completed", Value::Null).await;

        let record = record_of(&state, &state.agent).await;
        assert_eq!(record["status"], "completed", "{kind:?}: {record}");
        assert_eq!(record["usageResume"]["status"], "started", "{kind:?}");
        let results = child_results(&state).await;
        assert_eq!(results.len(), 1, "{kind:?}: {results:?}");
        assert_eq!(
            results[0].0,
            format!("child:{}:{RETRIED_TURN}", state.agent)
        );
        assert_eq!(results[0].1["status"], "completed", "{kind:?}");
        worker.shutdown().await.unwrap();
    }
}

/// The Claude bridge reports a session limit with the provider reset time. The
/// resume is planned for that reset; once due with fresh allowed limits, the
/// worker starts one continuation turn and the lead gets its result once.
#[tokio::test]
async fn a_claude_session_limit_resumes_once_after_the_provider_reset() {
    let (state, account) = fixture(ProviderKind::Claude).await;
    let (mut peer, mut worker) = open(&state, &account).await;
    let reset = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
        .floor()
        + 3600.0;
    let limit = json!({"message":"You've hit your session limit · resets 3:40am (UTC)",
        "codexErrorInfo":"rateLimitExceeded","resetsAt":reset});
    fail_first_turn(&state, &mut peer, &worker, limit).await;
    let record = record_of(&state, &state.agent).await;
    assert_eq!(record["usageResume"]["status"], "scheduled", "{record}");
    assert_eq!(record["usageResume"]["cause"], "rate_limit", "{record}");
    assert_eq!(record["usageResume"]["plannedAt"], reset, "{record}");
    assert_eq!(record["usageResume"]["dueAt"], reset + 5.0, "{record}");
    assert!(child_results(&state).await.is_empty());

    let due = reset + 6.0;
    let limits = LimitsObservation::from_record(
        json!({"accountKey":"default","at":due,"error":null,
            "data":{"rateLimits":{"primary":{"usedPercent":0.0,"resetsAt":due + 18_000.0}}}}),
        None,
    )
    .expect("limits");
    tick_when_due(&state, "usageResume", Some(limits)).await;
    wake_agent(&state, &worker, &state.agent).await;
    let start = next_turn_start(&mut peer).await;
    assert!(
        start["params"]["input"]
            .to_string()
            .contains("usage or rate limit"),
        "{start}"
    );
    finish(&mut peer, &start, RETRIED_TURN, "completed", Value::Null).await;

    let record = record_of(&state, &state.agent).await;
    assert_eq!(record["status"], "completed", "{record}");
    assert_eq!(record["usageResume"]["status"], "started");
    let results = child_results(&state).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(
        results[0].0,
        format!("child:{}:{RETRIED_TURN}", state.agent)
    );
    worker.shutdown().await.unwrap();
    while let Ok(write) = peer.writes.try_recv() {
        assert_ne!(write.value["method"], "turn/start", "{}", write.value);
    }
}

fn unix_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
        .floor()
}

/// The fresh allowed limits of the fixture account at `at`.
fn allowed_limits(at: f64) -> LimitsObservation {
    LimitsObservation::from_record(
        json!({"accountKey":"default","at":at,"error":null,
            "data":{"rateLimits":{"primary":{"usedPercent":0.0,"resetsAt":at + 18_000.0}}}}),
        None,
    )
    .expect("limits")
}

/// Accept the continuation `start` as the retried turn, complete it, and check
/// that the lead got its result once and no other turn started.
async fn finish_continuation(
    state: &State,
    peer: &mut Peer,
    worker: &mut AccountHandle,
    start: &Value,
) {
    assert!(
        start["params"]["input"]
            .to_string()
            .contains("usage or rate limit"),
        "{start}"
    );
    finish(peer, start, RETRIED_TURN, "completed", Value::Null).await;
    let record = record_of(state, &state.agent).await;
    assert_eq!(record["status"], "completed", "{}", record["error"]);
    assert_eq!(record["usageResume"]["status"], "started");
    let results = child_results(state).await;
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(
        results[0].0,
        format!("child:{}:{RETRIED_TURN}", state.agent)
    );
    worker.shutdown().await.unwrap();
    while let Ok(write) = peer.writes.try_recv() {
        assert_ne!(write.value["method"], "turn/start", "{}", write.value);
    }
}

/// The account limits cannot be read after the provider reset, for example
/// while the account's session is down. The wait must not poll for hours: it
/// resumes once at the reset that the limit error named, and the continuation
/// turn starts once.
#[tokio::test]
async fn a_claude_session_limit_resumes_once_after_its_reset_when_limits_reads_fail() {
    let (state, account) = fixture(ProviderKind::Claude).await;
    let (mut peer, mut worker) = open(&state, &account).await;
    let reset = unix_now() + 3600.0;
    let limit = json!({"message":"You've hit your session limit · resets 3:40am (UTC)",
        "codexErrorInfo":"rateLimitExceeded","resetsAt":reset});
    fail_first_turn(&state, &mut peer, &worker, limit).await;
    assert_eq!(
        record_of(&state, &state.agent).await["usageResume"]["dueAt"],
        reset + 5.0
    );

    tick_when_due(&state, "usageResume", None).await;
    let record = record_of(&state, &state.agent).await;
    assert_eq!(
        record["usageResume"]["status"], "started",
        "{}",
        record["usageResume"]
    );
    wake_agent(&state, &worker, &state.agent).await;
    let start = next_turn_start(&mut peer).await;
    finish_continuation(&state, &mut peer, &mut worker, &start).await;
}

/// A turn that Claude started itself failed at the session limit with only
/// the limit notice as its error, while the chat's last usage resume was an
/// old `started` one. The new failure must get its own wait, and the events
/// that arrive while the chat waits must reach it with the continuation once
/// the limit has reset.
#[tokio::test]
async fn a_claude_limit_named_only_by_its_notice_waits_and_delivers_held_events() {
    let (state, account) = fixture(ProviderKind::Claude).await;
    let (mut peer, mut worker) = open(&state, &account).await;
    let agent = state.agent.clone();
    let epoch = state.epoch;
    let now = unix_now();
    state
        .bridge
        .run(move |store| async move {
            scheduler::lifecycle_transaction_with(store.as_ref(), move |tx| {
                let stale = json!({"id":"earlier-resume","accountKey":"default",
                    "threadId":"fake-thread-0","epoch":epoch,"turnId":"turn-0",
                    "cause":"rate_limit","status":"started","failedAt":now - 7200.0,
                    "startedAt":now - 3600.0,"dueAt":null,"taskClaims":[]});
                tx.execute(
                    "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES('earlier-resume',?1,?2)",
                    rusqlite::params![agent, stale.to_string()],
                )?;
                let mut row = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                row.extra.insert("usageResume".into(), stale);
                AgentRepository::put(tx, &row)?;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
    let notice =
        json!({"message":"You've hit your session limit · resets 12:20am (Europe/Warsaw)"});
    fail_first_turn(&state, &mut peer, &worker, notice).await;
    let record = record_of(&state, &state.agent).await;
    assert_eq!(
        record["usageResume"]["status"], "scheduled",
        "{}",
        record["usageResume"]
    );
    assert_eq!(
        record["usageResume"]["turnId"], FAILED_TURN,
        "{}",
        record["usageResume"]
    );
    assert_eq!(
        record["usageResume"]["cause"], "rate_limit",
        "{}",
        record["usageResume"]
    );

    // A child reports while the chat waits for the limit.
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            scheduler::lifecycle_transaction_with(store.as_ref(), move |tx| {
                let mut child = AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                child.id = AgentId::from("held-child");
                child.status = studio_runtime_store::AgentStatus::Completed;
                child.extra.insert("parentId".into(), json!(agent));
                child.extra.insert("isLead".into(), json!(false));
                child.extra.insert("threadId".into(), json!("held-child-thread"));
                child.extra.remove("usageResume");
                AgentRepository::put(tx, &child)?;
                let text = json!({"agent":"held-child","name":"Child","status":"completed",
                    "result":"the release notes are ready"});
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('held-result',?1,'child_result',?2,'pending',?3,?4)",
                    rusqlite::params![agent, text.to_string(), now, epoch],
                )?;
                tx.execute(
                    "INSERT INTO runtime_event_meta(id,record) VALUES('held-result',?1)",
                    [json!({"delivery":"current_turn","source":"limit-test"}).to_string()],
                )?;
                Ok(())
            })
            .await
        })
        .await
        .unwrap();

    let due = record_of(&state, &state.agent).await["usageResume"]["dueAt"]
        .as_f64()
        .expect("due");
    tick_when_due(&state, "usageResume", Some(allowed_limits(due + 1.0))).await;
    wake_agent(&state, &worker, &state.agent).await;
    let start = next_turn_start(&mut peer).await;
    assert!(
        start["params"]["input"]
            .to_string()
            .contains("the release notes are ready"),
        "{start}"
    );
    finish_continuation(&state, &mut peer, &mut worker, &start).await;
}
