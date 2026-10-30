use serde::Serialize;
use studio_runtime_services::recovery_policy::RecoverySnapshot;
use studio_runtime_store::SchedulerRepository;

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Agent<'a> {
    id: &'a str,
    root_id: &'a str,
    provider: &'a str,
    epoch: i64,
    account_key: &'a str,
    thread_id: &'a str,
    status: &'a str,
    in_flight: bool,
    auto_wake: bool,
    start_attempt: Attempt<'a>,
    start_outcome_hold: Hold<'a>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Attempt<'a> {
    id: &'a str,
    epoch: i64,
    account_key: &'a str,
    thread_id: Option<&'a str>,
    submitted: bool,
    events: Vec<&'a str>,
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Hold<'a> {
    stage: &'a str,
    reason: &'a str,
    attempt_id: &'a str,
}

#[test]
fn held_unclean_start_is_eligible_for_exact_history_only() {
    let mut agent = Agent {
        id: "agent",
        root_id: "agent",
        provider: "codex",
        epoch: 4,
        account_key: "account",
        thread_id: "thread",
        status: "interrupted",
        in_flight: false,
        auto_wake: false,
        start_attempt: Attempt {
            id: "attempt",
            epoch: 4,
            account_key: "account",
            thread_id: None,
            submitted: false,
            events: vec!["event"],
        },
        start_outcome_hold: Hold {
            stage: "held",
            reason: SchedulerRepository::UNCLEAN_START_HOLD,
            attempt_id: "attempt",
        },
    };
    let snapshot = RecoverySnapshot::from_record(studio_runtime_store::projection_value(&agent))
        .expect("held agent");
    assert!(snapshot.eligibility().unconfirmed_start);
    agent.start_outcome_hold.attempt_id = "different";
    let changed = RecoverySnapshot::from_record(studio_runtime_store::projection_value(&agent))
        .expect("changed agent");
    assert!(!changed.eligibility().unconfirmed_start);
}
