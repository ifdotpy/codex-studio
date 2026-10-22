//! Capacity retry transitions. A claim queues one follow-up input; the ordinary
//! queued start path (start gate, auth gate, admission) starts it like any other
//! input. A claim is never permission to resend an unknown turn.
use crate::continuation::{
    ContinuationError, Decision, Effect, Result, Snapshot, WaitStatus, account, flag, now_checked,
    patch, present, remove, text,
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
            _ => &CONNECTION_DELAYS,
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
        || !flag(&s.agent, "autoWake")
        || present(&s.agent, "deletedAt")
        || !present(&s.agent, "threadId")
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
        account(&s.agent),
        s.agent["threadId"],
        s.agent["epoch"],
        turn_id
    ]);
    let id = crate::event_write::request_signature(&identity);
    let count = s
        .agent
        .get("capacityRetryCount")
        .and_then(Value::as_u64)
        .unwrap_or(0);
    let delays = kind.delays();
    let delay = usize::try_from(count).ok().and_then(|i| delays.get(i));
    let mut r = json!({"id":id,"threadId":s.agent["threadId"],"turnId":turn_id,"accountKey":account(&s.agent),"epoch":s.agent["epoch"],"cause":kind.kind(),"status":if delay.is_some(){"scheduled"}else{"exhausted"},"dueAt":delay.map(|d|now+d),"attempt":count.saturating_add(1),"maxAttempts":delays.len(),"cwd":s.agent["cwd"],"settings":settings,"taskClaims":s.claims});
    if let Some(reason) = reason {
        r["reason"] = json!(reason);
    }
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
    let mut r = stored.clone();
    let mut agent = s.agent.clone();
    let mut hold = None;
    // A start outcome belongs to the claimed follow-up of this retry, once.
    let start_pending = matches!(s.status(), Some(WaitStatus::Starting | WaitStatus::Unknown))
        && present(&r, "claimedAt")
        && !present(&r, "acceptedTurnId")
        && r.get("epoch") == agent.get("epoch")
        && account(&r) == account(&agent);
    match action {
        Action::Completed { turn_id, completed } => {
            if text(&r, "acceptedTurnId") != turn_id {
                return Ok(Decision::default());
            }
            if text(&r, "status") == "finished" {
                return Ok(Decision::default());
            }
            patch(&mut r, json!({"status":"finished","dueAt":null}));
            if completed {
                remove(&mut agent, "nativeFailureHold");
            }
        }

        Action::Cancel => {
            if text(&r, "status") != "scheduled" || present(&r, "claimedAt") {
                return Ok(Decision::default());
            }
            patch(
                &mut r,
                json!({"status":"cancelled","dueAt":null,"reason":null}),
            );
            hold = Some("cancelled");
        }
        Action::Wait(reason) => {
            if text(&r, "status") == "unknown" || present(&r, "acceptedTurnId") {
                return Ok(Decision::default());
            }
            let waits = r
                .get("waits")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(1);
            remove(&mut r, "claimedAt");
            patch(
                &mut r,
                json!({"status":"scheduled","dueAt":now+(WAIT_STEP*waits as f64).min(WAIT_MAX),"waits":waits,"reason":reason}),
            );
        }
        Action::DiskWait(reason) => {
            if text(&r, "status") == "unknown" || present(&r, "acceptedTurnId") {
                return Ok(Decision::default());
            }
            remove(&mut r, "claimedAt");
            patch(
                &mut r,
                json!({"status":"scheduled","dueAt":now+DISK_FULL_RECHECK_SECONDS as f64,"reason":reason}),
            );
        }
        Action::Excluded(reason) => {
            if text(&r, "status") != "scheduled" || present(&r, "acceptedTurnId") {
                return Ok(Decision::default());
            }
            patch(
                &mut r,
                json!({"status":"cancelled","dueAt":null,"reason":reason}),
            );
            patch(
                &mut agent,
                json!({"status":"failed","inFlight":false,"error":reason}),
            );
            hold = Some("account-excluded");
        }
        Action::Error { reason, unknown } => {
            if !start_pending || unknown && s.status() == Some(WaitStatus::Unknown) {
                return Ok(Decision::default());
            }
            patch(
                &mut r,
                json!({"status":if unknown{"unknown"}else{"failed"},"dueAt":null,"reason":reason}),
            );
            if s.status() == Some(WaitStatus::Starting) {
                hold = Some(if unknown { "unknown" } else { "failed" });
            }
        }
        Action::Accepted { turn_id } => {
            if !start_pending {
                return Ok(Decision::default());
            }
            r["acceptedTurnId"] = json!(turn_id);
            patch(
                &mut r,
                json!({"status":"starting","reason":null,"dueAt":null}),
            );
            agent["capacityRetryCount"] = json!(
                agent
                    .get("capacityRetryCount")
                    .and_then(Value::as_u64)
                    .unwrap_or(0)
                    .saturating_add(1)
            );
        }
        Action::Claim {
            settings,
            automatic,
        } => {
            if automatic && text(&r, "status") != "scheduled" {
                return Ok(Decision::default());
            }
            if present(&r, "claimedAt")
                || !matches!(text(&r, "status"), "scheduled" | "cancelled" | "exhausted")
            {
                return Ok(Decision::default());
            }
            if !s.claims_valid(&r) {
                patch(
                    &mut r,
                    json!({"status":"cancelled","dueAt":null,"reason":TASK_CHANGED}),
                );
                hold = Some("task-changed");
            } else {
                if present(&agent, "deletedAt")
                    || !flag(&agent, "autoWake")
                    || ["epoch", "threadId", "cwd"]
                        .iter()
                        .any(|k| r.get(*k) != agent.get(*k))
                    || account(&r) != account(&agent)
                    || r.get("settings") != Some(settings)
                    || r.get("turnId") != agent.get("lastCompletedTurn")
                {
                    return Err(ContinuationError::Invalid(
                        "/capacity/earlier-agent-state".into(),
                    ));
                }
                // The follow-up input waits in the queue like any other input:
                // the start gate, auth gate and admission of the ordinary
                // start path decide when it starts, and the input is never
                // claimed twice. The failure hold ends with the claim.
                patch(
                    &mut r,
                    json!({"status":"starting","dueAt":null,"claimedAt":now,"reason":null}),
                );
                remove(&mut agent, "nativeFailureHold");
                patch(&mut agent, json!({"status":"queued","error":null}));
                r["updatedAt"] = json!(now);
                agent["capacityRetry"] = r.clone();
                let id = format!("{CAPACITY_FOLLOWUP_PREFIX}{}", text(&r, "id"));
                return Ok(Decision {
                    agent: Some(agent),
                    record: Some(r),
                    effects: vec![Effect::Followup {
                        id,
                        text: CONTINUATION.into(),
                    }],
                    ..Decision::default()
                });
            }
        }
    }
    r["updatedAt"] = json!(now);
    agent["capacityRetry"] = r.clone();
    let mut d = Decision {
        agent: Some(agent),
        record: Some(r),
        ..Decision::default()
    };
    if let Some(transition) = hold {
        let reason = if transition == "cancelled" {
            "The automatic capacity retry was cancelled."
        } else {
            text(d.record.as_ref().unwrap_or(&Value::Null), "reason")
        }
        .to_owned();
        d.hold(text(stored, "id"), transition, &reason);
    }
    Ok(d)
}
