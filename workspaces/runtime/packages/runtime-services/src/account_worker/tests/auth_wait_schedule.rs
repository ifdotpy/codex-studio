use super::super::vm_refresh_deadline_after_session_change;
use super::*;
use serde_json::json;
use std::time::UNIX_EPOCH;

async fn write_agent_and_resume(state: &State, attempt_id: &str, resume_id: &str, due_at: f64) {
    let agent_id = state.agent.clone();
    let epoch = state.epoch;
    let resume_id = resume_id.to_owned();
    let attempt = json!({"id": attempt_id});
    let receipt = json!({
        "status": "auth_probe_wait",
        "accountKey": "default",
        "dueAt": due_at,
    });
    state
        .bridge
        .store
        .transaction_with(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(\
                    record,'$.provider','claude','$.accountKey','default','$.epoch',?1,\
                    '$.startAttempt',json(?2)) WHERE id=?3",
                rusqlite::params![epoch, attempt.to_string(), &agent_id],
            )?;
            tx.execute(
                "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES(?1,?2,?3) \
                 ON CONFLICT(id) DO UPDATE SET agent=excluded.agent,record=excluded.record",
                rusqlite::params![resume_id, agent_id, receipt.to_string()],
            )?;
            Ok::<(), studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
}

async fn deadline(state: &State) -> Option<std::time::SystemTime> {
    crate::claude_auth_runtime::earliest_wait_deadline(&state.bridge.store, "default")
        .await
        .unwrap()
}

fn unix_seconds(time: std::time::SystemTime) -> f64 {
    time.duration_since(UNIX_EPOCH).unwrap().as_secs_f64()
}

fn receipt_id(attempt_id: &str) -> String {
    format!(
        "{}{attempt_id}",
        crate::claude_auth_wait::PROBE_RECEIPT_ID_PREFIX
    )
}

#[tokio::test]
async fn empty_auth_schedule_has_no_deadline_and_first_commit_wakes_subscriber() {
    let state = State::new().await;
    let mut changes = state.bridge.auth_wait_changes();

    assert_eq!(deadline(&state).await, None);
    assert!(!changes.has_changed().unwrap());

    let due_at = 4_000_000_000.0;
    write_agent_and_resume(
        &state,
        "current-attempt",
        &receipt_id("current-attempt"),
        due_at,
    )
    .await;
    changes.changed().await.unwrap();
    assert_eq!(unix_seconds(deadline(&state).await.unwrap()), due_at);
}

#[tokio::test]
async fn current_attempt_deadline_ignores_stale_receipt_and_tracks_persisted_retry_time() {
    let state = State::new().await;
    write_agent_and_resume(&state, "current-attempt", &receipt_id("stale-attempt"), 1.0).await;
    write_agent_and_resume(
        &state,
        "current-attempt",
        &receipt_id("current-attempt"),
        4_000_000_123.25,
    )
    .await;

    assert_eq!(
        unix_seconds(deadline(&state).await.unwrap()),
        4_000_000_123.25,
        "a stale due receipt cannot trigger an immediate retry loop"
    );

    let mut changes = state.bridge.auth_wait_changes();
    write_agent_and_resume(
        &state,
        "current-attempt",
        &receipt_id("current-attempt"),
        4_000_000_050.5,
    )
    .await;
    changes.changed().await.unwrap();
    assert_eq!(
        unix_seconds(deadline(&state).await.unwrap()),
        4_000_000_050.5,
        "an earlier persisted deadline wakes the subscriber and is observed exactly"
    );
}

#[test]
fn vm_refresh_timer_tracks_first_and_last_matching_session_only() {
    let now = tokio::time::Instant::now();
    let cadence_deadline = now + crate::claude_auth_runtime::VM_CREDENTIAL_REFRESH_INTERVAL;

    assert_eq!(
        vm_refresh_deadline_after_session_change(false, false, true, None, now),
        None,
        "an empty account never starts a periodic VM timer"
    );
    assert_eq!(
        vm_refresh_deadline_after_session_change(false, true, true, None, now),
        Some(now),
        "the first matching VM preserves the immediate refresh"
    );
    assert_eq!(
        vm_refresh_deadline_after_session_change(true, true, true, Some(cadence_deadline), now),
        Some(cadence_deadline),
        "adding or retiring a non-last VM preserves the cadence"
    );
    assert_eq!(
        vm_refresh_deadline_after_session_change(true, false, true, Some(cadence_deadline), now),
        None,
        "retiring the last matching VM disarms the timer"
    );
    assert_eq!(
        vm_refresh_deadline_after_session_change(false, true, false, None, now),
        None,
        "a matching VM without a credential resolver has no timer"
    );
}
