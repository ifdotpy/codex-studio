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
