//! The outcome of the `turn/start` that carried a capacity retry's follow-up
//! input settles its record through the account worker's store, for both
//! providers: an accepted start binds the turn and the turn's completion
//! finishes the record, a rejected start ends it `failed`, and a reply without
//! a turn id ends it `unknown`. Only an accepted start counts the attempt.
use super::auth_hold::{agents, record_of};
use super::*;
use crate::capacity_retry::{self, Action, Failure};
use crate::continuation::Snapshot;
use crate::continuation_runtime::CapacityStartOutcome;
use crate::scheduler::{TurnEvent, TurnIntent};
use studio_runtime_providers::ProviderKind;
use studio_runtime_store::{ContinuationRepository, ContinuationTable};

const PREVIOUS_TURN: &str = "previous-turn";
const NOW: f64 = 1000.0;

/// Claim a scheduled capacity retry of the fixture agent as the continuation driver does:
/// the record is `starting` and the agent is queued for the retry's follow-up input.
async fn claim_retry(state: &State) -> String {
    let agent = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut row =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    for (key, value) in [
                        ("lastCompletedTurn", json!(PREVIOUS_TURN)),
                        ("autoWake", json!(true)),
                        ("status", json!("completed")),
                        ("inFlight", json!(false)),
                    ] {
                        row.extra.insert(key.into(), value);
                    }
                    row.extra.remove("startAttempt");
                    AgentRepository::put(tx, &row)?;
                    let raw = ContinuationRepository::snapshot(
                        tx,
                        &agent,
                        ContinuationTable::Capacity,
                        None,
                    )?;
                    let snapshot = Snapshot::from_records(raw).expect("snapshot");
                    let settings = json!({});
                    let failure = capacity_retry::failed(
                        &snapshot,
                        Failure::ServerOverloaded,
                        PREVIOUS_TURN,
                        &settings,
                        NOW,
                    )
                    .expect("failure");
                    let scheduled = failure.record.expect("scheduled retry");
                    let held = AgentRecord::from_value(failure.agent.expect("agent"))
                        .map_err(StoreError::from)?;
                    AgentRepository::put(tx, &held)?;
                    ContinuationRepository::put_record(
                        tx,
                        ContinuationTable::Capacity,
                        &agent,
                        &scheduled,
                    )?;
                    let raw = ContinuationRepository::snapshot(
                        tx,
                        &agent,
                        ContinuationTable::Capacity,
                        scheduled["id"].as_str(),
                    )?;
                    let snapshot = Snapshot::from_records(raw).expect("scheduled snapshot");
                    let claim = capacity_retry::transition(
                        &snapshot,
                        Action::Claim {
                            settings: &settings,
                            automatic: true,
                        },
                        NOW,
                    )
                    .expect("claim");
                    let record = claim.record.expect("claimed record");
                    ContinuationRepository::put_record(
                        tx,
                        ContinuationTable::Capacity,
                        &agent,
                        &record,
                    )?;
                    let claimed = AgentRecord::from_value(claim.agent.expect("claimed agent"))
                        .map_err(StoreError::from)?;
                    AgentRepository::put(tx, &claimed)?;
                    Ok(record["id"].as_str().expect("id").to_owned())
                })
                .await
        })
        .await
        .unwrap()
}

async fn retry_of(state: &State, id: &str) -> Value {
    let (agent, id) = (state.agent.clone(), id.to_owned());
    state
        .bridge
        .run(move |store| async move {
            Ok(ContinuationRepository::snapshot(
                &store.test_reader()?,
                &agent,
                ContinuationTable::Capacity,
                Some(&id),
            )?)
        })
        .await
        .unwrap()["record"]
        .clone()
}

/// Settle the outcome of the `turn/start` that carried the retry's follow-up in one
/// transaction, as the worker does.
async fn settle(state: &State, id: &str, outcome: CapacityStartOutcome<'static>) {
    let (agent, followup) = (
        state.agent.clone(),
        format!("{}{id}", capacity_retry::CAPACITY_FOLLOWUP_PREFIX),
    );
    let (epoch, thread) = (state.epoch, "fake-thread-0");
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let intent = TurnIntent {
                        agent_id: agent,
                        epoch,
                        attempt_id: "attempt".into(),
                        account_key: "default".into(),
                        provider: ProviderKind::Codex,
                        thread_id: Some(thread.into()),
                        cwd: None,
                        model: None,
                        events: vec![TurnEvent {
                            id: followup,
                            kind: "followup".into(),
                            text: capacity_retry::CONTINUATION.into(),
                            created: NOW,
                            epoch,
                            error: None,
                            metadata: json!({}),
                        }],
                        radio_authorized: false,
                    };
                    Ok::<_, WorkerError>(store::stage_capacity_start(tx, &intent, outcome)?)
                })
                .await
        })
        .await
        .unwrap();
}

/// Both providers share the account worker's settlement; each provider's agent takes every
/// outcome. A finished turn closes only an accepted retry.
#[tokio::test]
async fn a_capacity_retry_start_settles_its_record_for_every_outcome() {
    for kind in ProviderKind::ALL {
        for outcome in ["accepted", "rejected", "no-turn-id"] {
            let state = State::new().await;
            agents(&state, kind, 0).await;
            let id = claim_retry(&state).await;
            assert_eq!(retry_of(&state, &id).await["status"], "starting");
            let (status, count) = match outcome {
                "accepted" => {
                    settle(
                        &state,
                        &id,
                        CapacityStartOutcome::Accepted { turn_id: "turn" },
                    )
                    .await;
                    ("starting", 1)
                }
                "rejected" => {
                    let failed = CapacityStartOutcome::Failed {
                        reason: "rejected",
                        unknown: false,
                    };
                    settle(&state, &id, failed).await;
                    ("failed", 0)
                }
                _ => {
                    let lost = CapacityStartOutcome::Failed {
                        reason: "no turn id",
                        unknown: true,
                    };
                    settle(&state, &id, lost).await;
                    ("unknown", 0)
                }
            };
            let retry = retry_of(&state, &id).await;
            assert_eq!(retry["status"], status, "{kind:?} {outcome}: {retry}");
            let agent = record_of(&state, &state.agent).await;
            assert_eq!(
                agent["capacityRetryCount"].as_u64().unwrap_or(0),
                count,
                "{kind:?} {outcome}: {agent}"
            );
            assert_eq!(agent["capacityRetry"]["status"], status);
            if outcome == "accepted" {
                assert_eq!(retry["acceptedTurnId"], "turn");
                let completion = capacity_retry::Action::Completed {
                    turn_id: "turn",
                    completed: true,
                };
                complete(&state, &id, completion).await;
                assert_eq!(retry_of(&state, &id).await["status"], "finished");
            }
        }
    }
}

/// Apply the terminal-turn transition to the stored retry.
async fn complete(state: &State, id: &str, action: Action<'static>) {
    let (agent, id) = (state.agent.clone(), id.to_owned());
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let raw = ContinuationRepository::snapshot(
                        tx,
                        &agent,
                        ContinuationTable::Capacity,
                        Some(&id),
                    )?;
                    let snapshot = Snapshot::from_records(raw).expect("snapshot");
                    let decision =
                        capacity_retry::transition(&snapshot, action, NOW).expect("done");
                    ContinuationRepository::put_record(
                        tx,
                        ContinuationTable::Capacity,
                        &agent,
                        &decision.record.expect("record"),
                    )?;
                    Ok::<_, WorkerError>(())
                })
                .await
        })
        .await
        .unwrap();
}
