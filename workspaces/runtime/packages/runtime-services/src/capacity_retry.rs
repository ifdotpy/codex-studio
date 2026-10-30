//! Capacity retry transitions. A claim queues one follow-up input; the ordinary
//! queued start path (start gate, auth gate, admission) starts it like any other
//! input. A claim is never permission to resend an unknown turn.
use crate::continuation::{
    CapacityDraft, ContinuationError, ContinuationUpdate as U, Decision, Effect, ProposedAgent,
    ProposedRecord, Result, Snapshot, WaitStatus, account, flag, now_checked, present, text,
};
use crate::usage_resume::TASK_CHANGED;
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{DISK_FULL_MAX_RESUMES, DISK_FULL_RECHECK_SECONDS};
const WAIT_MAX: f64 = 300.0;
const WAIT_STEP: f64 = 15.0;
const DELAYS: [f64; 4] = [10.0, 30.0, 120.0, 300.0];
const CONNECTION_DELAYS: [f64; 7] = [10.0, 30.0, 120.0, 300.0, 600.0, 1200.0, 1800.0];
/// A disk-full retry is due at once; the tick waits while the disk is still full.
const DISK_FULL_DELAYS: [f64; DISK_FULL_MAX_RESUMES as usize] =
    [0.0; DISK_FULL_MAX_RESUMES as usize];
/// Prefix of a capacity retry's follow-up event id; the rest is the retry record id.
pub const CAPACITY_FOLLOWUP_PREFIX: &str = "capacity-retry:";
/// The follow-up input that continues the turn a capacity failure stopped.
pub const CONTINUATION: &str = "The previous turn stopped on a temporary provider, connection or disk error. Check the current task and conversation state, then continue the same task. Do not repeat completed work.";
#[derive(Clone, Copy, Debug)]
pub enum Failure {
    ServerOverloaded,
    InternalServerError,
    HttpConnectionFailed,
    ResponseStreamConnectionFailed,
    ResponseStreamDisconnected,
    ResponseTooManyFailedAttempts,
    /// The turn failed because the disk was full. The retry waits for free space.
    DiskFull,
}
impl Failure {
    fn kind(self) -> &'static str {
        match self {
            Self::ServerOverloaded => "serverOverloaded",
            Self::InternalServerError => "internalServerError",
            Self::HttpConnectionFailed => "httpConnectionFailed",
            Self::ResponseStreamConnectionFailed => "responseStreamConnectionFailed",
            Self::ResponseStreamDisconnected => "responseStreamDisconnected",
            Self::ResponseTooManyFailedAttempts => "responseTooManyFailedAttempts",
            Self::DiskFull => crate::disk_space::DISK_FULL_KIND,
        }
    }
    fn delays(self) -> &'static [f64] {
        match self {
            Self::ServerOverloaded | Self::InternalServerError => &DELAYS,
            Self::DiskFull => &DISK_FULL_DELAYS,
            Self::HttpConnectionFailed
            | Self::ResponseStreamConnectionFailed
            | Self::ResponseStreamDisconnected
            | Self::ResponseTooManyFailedAttempts => &CONNECTION_DELAYS,
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Action<'a> {
    Completed {
        turn_id: &'a str,
        completed: bool,
    },
    Cancel,
    Wait(&'a str),
    /// The disk is full: check free space again after a fixed interval.
    DiskWait(&'a str),
    Excluded(&'a str),
    /// The `turn/start` that carried the retry's follow-up was rejected or its outcome is unknown.
    Error {
        reason: &'a str,
        unknown: bool,
    },
    /// The provider accepted the `turn/start` that carried the retry's follow-up.
    Accepted {
        turn_id: &'a str,
    },
    Claim {
        settings: &'a Value,
        automatic: bool,
    },
}
/// Construct a new exact failure row after the turn driver has classified a known terminal failure.
pub fn failed(
    s: &Snapshot,
    kind: Failure,
    turn_id: &str,
    settings: &Value,
    now: f64,
) -> Result<Decision> {
    failed_with_reason(s, kind, turn_id, settings, now, None)
}

/// As [`failed`], with the visible reason of a disk-full hold.
pub fn failed_with_reason(
    s: &Snapshot,
    kind: Failure,
    turn_id: &str,
    settings: &Value,
    now: f64,
    reason: Option<&str>,
) -> Result<Decision> {
    now_checked(now)?;
    if crate::continuation::native_thread_blocked(&s.agent)
        || !s.agent_record().auto_wake.unwrap_or(false)
        || present(&s.agent, "deletedAt")
        || s.agent_record().thread_id.is_none()
        || s.agent
            .get("turnEpoch")
            .is_some_and(|e| Some(e) != s.agent.get("epoch"))
    {
        return Ok(Decision::default());
    }
    if s.record.is_some() {
        return Ok(Decision::default());
    }
    let identity = json!([
        s.agent_id(),
        s.agent_account(),
        s.agent_record().thread_id,
        s.agent_record().epoch,
        turn_id
    ]);
    let id = crate::event_write::request_signature(&identity);
    let count = s
        .agent_view()
        .capacity_retry_count
        .as_ref()
        .and_then(|n| n.as_u64())
        .unwrap_or(0);
    let delays = kind.delays();
    let delay = usize::try_from(count).ok().and_then(|i| delays.get(i));
    let r = ProposedRecord::capacity(CapacityDraft {
        id,
        thread_id: s.agent_record().thread_id.clone(),
        turn_id: turn_id.into(),
        account_key: s.agent_account().into(),
        epoch: s.agent_record().epoch,
        cause: kind.kind(),
        status: if delay.is_some() {
            "scheduled"
        } else {
            "exhausted"
        },
        due_at: delay.map(|d| now + d),
        attempt: count.saturating_add(1),
        max_attempts: delays.len(),
        cwd: s.agent["cwd"].clone(),
        settings: settings.clone(),
        task_claims: s.claims.clone(),
        reason: reason.map(str::to_owned),
        updated_at: now,
    });
    Ok(Decision::save(s, r, "capacityRetry", now))
}
pub fn transition(s: &Snapshot, action: Action<'_>, now: f64) -> Result<Decision> {
    now_checked(now)?;
    let Some(stored) = &s.record else {
        return Ok(Decision::default());
    };
    if !s.current("capacityRetry") {
        return Ok(Decision::default());
    }
    let mut r = ProposedRecord::existing(s).expect("validated record");
    let mut agent = ProposedAgent::new(s);
    let mut hold = None;
    let accepted_turn = s.capacity_record().map_or_else(
        || text(stored, "acceptedTurnId"),
        |record| {
            record
                .accepted_turn_id
                .as_ref()
                .map_or("", |id| id.as_str())
        },
    );
    let accepted = s.capacity_record().map_or_else(
        || present(stored, "acceptedTurnId"),
        |record| record.accepted_turn_id.is_some(),
    );
    let claimed = s.capacity_record().map_or_else(
        || present(stored, "claimedAt"),
        |record| record.claimed_at.as_ref().is_some_and(|at| at.get() != 0.0),
    );
    // A start outcome belongs to the claimed follow-up of this retry, once.
    let start_pending = matches!(s.status(), Some(WaitStatus::Starting | WaitStatus::Unknown))
        && claimed
        && !accepted
        && stored.get("epoch") == agent.value().get("epoch")
        && s.record_account() == Some(account(agent.value()));
    match action {
        Action::Completed { turn_id, completed } => {
            if accepted_turn != turn_id {
                return Ok(Decision::default());
            }
            if s.status() == Some(WaitStatus::Finished) {
                return Ok(Decision::default());
            }
            r.update(U::Status("finished"));
            r.update(U::DueAt(None));
            if completed {
                agent.clear_hold();
            }
        }

        Action::Cancel => {
            if s.status() != Some(WaitStatus::Scheduled) || claimed {
                return Ok(Decision::default());
            }
            r.update(U::Status("cancelled"));
            r.update(U::DueAt(None));
            r.update(U::Reason(None));
            hold = Some("cancelled");
        }
        Action::Wait(reason) => {
            if s.status() == Some(WaitStatus::Unknown) || accepted {
                return Ok(Decision::default());
            }
            let waits = s
                .capacity_record()
                .map(|record| record.waits.as_ref().and_then(|n| n.as_u64()))
                .unwrap_or_else(|| stored.get("waits").and_then(Value::as_u64))
                .unwrap_or(0)
                .saturating_add(1);
            r.update(U::RemoveClaimedAt);
            r.update(U::Status("scheduled"));
            r.update(U::DueAt(Some(
                now + (WAIT_STEP * waits as f64).min(WAIT_MAX),
            )));
            r.update(U::Waits(waits));
            r.update(U::Reason(Some(reason.into())));
        }
        Action::DiskWait(reason) => {
            if s.status() == Some(WaitStatus::Unknown) || accepted {
                return Ok(Decision::default());
            }
            r.update(U::RemoveClaimedAt);
            r.update(U::Status("scheduled"));
            r.update(U::DueAt(Some(now + DISK_FULL_RECHECK_SECONDS as f64)));
            r.update(U::Reason(Some(reason.into())));
        }
        Action::Excluded(reason) => {
            if s.status() != Some(WaitStatus::Scheduled) || accepted {
                return Ok(Decision::default());
            }
            r.update(U::Status("cancelled"));
            r.update(U::DueAt(None));
            r.update(U::Reason(Some(reason.into())));
            agent.failed(reason);
            hold = Some("account-excluded");
        }
        Action::Error { reason, unknown } => {
            if !start_pending || unknown && s.status() == Some(WaitStatus::Unknown) {
                return Ok(Decision::default());
            }
            r.update(U::Status(if unknown { "unknown" } else { "failed" }));
            r.update(U::DueAt(None));
            r.update(U::Reason(Some(reason.into())));
            if s.status() == Some(WaitStatus::Starting) {
                hold = Some(if unknown { "unknown" } else { "failed" });
            }
        }
        Action::Accepted { turn_id } => {
            if !start_pending {
                return Ok(Decision::default());
            }
            r.update(U::AcceptedTurnId(turn_id.into()));
            r.update(U::Status("starting"));
            r.update(U::Reason(None));
            r.update(U::DueAt(None));
            agent.capacity_count(
                agent
                    .value()
                    .get("capacityRetryCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(1),
            );
        }
        Action::Claim {
            settings,
            automatic,
        } => {
            if automatic && s.status() != Some(WaitStatus::Scheduled) {
                return Ok(Decision::default());
            }
            if claimed
                || !matches!(
                    s.status(),
                    Some(WaitStatus::Scheduled | WaitStatus::Cancelled | WaitStatus::Exhausted)
                )
            {
                return Ok(Decision::default());
            }
            if !s.claims_valid() {
                r.update(U::Status("cancelled"));
                r.update(U::DueAt(None));
                r.update(U::Reason(Some(TASK_CHANGED.into())));
                hold = Some("task-changed");
            } else {
                if present(agent.value(), "deletedAt")
                    || !flag(agent.value(), "autoWake")
                    || ["epoch", "threadId", "cwd"]
                        .iter()
                        .any(|k| stored.get(*k) != agent.value().get(*k))
                    || s.record_account() != Some(account(agent.value()))
                    || stored.get("settings") != Some(settings)
                    || stored.get("turnId") != agent.value().get("lastCompletedTurn")
                {
                    return Err(ContinuationError::Invalid(
                        "/capacity/earlier-agent-state".into(),
                    ));
                }
                // The follow-up input waits in the queue like any other input:
                // the start gate, auth gate and admission of the ordinary
                // start path decide when it starts, and the input is never
                // claimed twice. The failure hold ends with the claim.
                r.update(U::Status("starting"));
                r.update(U::DueAt(None));
                r.update(U::ClaimedAt(now));
                r.update(U::Reason(None));
                agent.clear_hold();
                agent.queued();
                r.update(U::UpdatedAt(now));
                agent.continuation("capacityRetry", r.value());
                let id = format!("{CAPACITY_FOLLOWUP_PREFIX}{}", s.record_id().unwrap_or(""));
                return Ok(Decision {
                    agent: Some(agent),
                    record: Some(r),
                    effects: vec![Effect::Followup {
                        id,
                        text: CONTINUATION.into(),
                    }],
                });
            }
        }
    }
    r.update(U::UpdatedAt(now));
    agent.continuation("capacityRetry", r.value());
    let mut d = Decision {
        agent: Some(agent),
        record: Some(r),
        ..Decision::default()
    };
    if let Some(transition) = hold {
        let reason = if transition == "cancelled" {
            "The automatic capacity retry was cancelled.".to_owned()
        } else {
            text(
                &d.record
                    .as_ref()
                    .map(ProposedRecord::value)
                    .unwrap_or(Value::Null),
                "reason",
            )
            .to_owned()
        };
        d.hold(s.record_id().unwrap_or(""), transition, &reason);
    }
    Ok(d)
}
