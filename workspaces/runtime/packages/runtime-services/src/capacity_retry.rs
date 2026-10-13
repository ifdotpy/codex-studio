//! Capacity retry transitions. A claim is an intent, never permission to resend an unknown turn.
use crate::continuation::{
    ContinuationError, Decision, Effect, Result, Snapshot, account, flag, now_checked, patch,
    present, remove, text,
};
use crate::usage_resume::TASK_CHANGED;
use serde_json::{Value, json};
const WAIT_MAX: f64 = 300.0;
const WAIT_STEP: f64 = 15.0;
const DELAYS: [f64; 4] = [10.0, 30.0, 120.0, 300.0];
const CONNECTION_DELAYS: [f64; 7] = [10.0, 30.0, 120.0, 300.0, 600.0, 1200.0, 1800.0];
#[derive(Clone, Copy, Debug)]
pub enum Failure {
    ServerOverloaded,
    InternalServerError,
    HttpConnectionFailed,
    ResponseStreamConnectionFailed,
    ResponseStreamDisconnected,
    ResponseTooManyFailedAttempts,
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
        }
    }
    fn delays(self) -> &'static [f64] {
        if matches!(self, Self::ServerOverloaded | Self::InternalServerError) {
            &DELAYS
        } else {
            &CONNECTION_DELAYS
        }
    }
}
#[derive(Clone, Copy, Debug)]
pub enum Action<'a> {
    Restart,
    Completed {
        turn_id: &'a str,
        completed: bool,
    },
    Reset(&'a str),
    Cancel,
    Wait(&'a str),
    Error {
        reason: &'a str,
        unknown: bool,
    },
    Accepted {
        attempt: &'a Value,
        turn_id: &'a str,
    },
    Claim {
        settings: &'a Value,
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
    let r = json!({"id":id,"threadId":s.agent["threadId"],"turnId":turn_id,"accountKey":account(&s.agent),"epoch":s.agent["epoch"],"cause":kind.kind(),"status":if delay.is_some(){"scheduled"}else{"exhausted"},"dueAt":delay.map(|d|now+d),"attempt":count.saturating_add(1),"maxAttempts":delays.len(),"cwd":s.agent["cwd"],"settings":settings,"taskClaims":s.claims});
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
    let attempt = agent.get("startAttempt").cloned().unwrap_or(Value::Null);
    match action {
        Action::Completed { turn_id, completed } => {
            if text(&r, "acceptedTurnId") != turn_id {
                return Ok(Decision::default());
            }
            patch(&mut r, json!({"status":"finished","dueAt":null}));
            if completed {
                remove(&mut agent, "nativeFailureHold");
            }
        }

        Action::Reset(reason) => {
            if attempt.get("capacityRetryId") == r.get("id") && !flag(&attempt, "submitted") {
                remove(&mut agent, "startAttempt");
                patch(&mut agent, json!({"inFlight":false,"status":"failed"}));
            }
            patch(
                &mut r,
                json!({"status":"finished","dueAt":null,"reason":reason}),
            );
            remove(&mut agent, "capacityRetry");
            remove(&mut agent, "capacityRetryCount");
        }
        Action::Restart => {
            if matches!(text(&r, "status"), "starting" | "unknown") {
                if attempt.get("capacityRetryId") == r.get("id")
                    && !flag(&attempt, "submitted")
                    && !present(&r, "acceptedTurnId")
                {
                    patch(
                        &mut r,
                        json!({"status":"cancelled","dueAt":null,"reason":"The server restarted before turn submission. Automatic retry was cancelled."}),
                    );
                    remove(&mut r, "claimedAt");
                    patch(
                        &mut agent,
                        json!({"status":"failed","autoWake":true,"inFlight":false}),
                    );
                } else {
                    patch(
                        &mut r,
                        json!({"status":"unknown","dueAt":null,"reason":"The server restarted. The turn outcome is unknown."}),
                    );
                }
                hold = Some("restart-terminal");
            } else if text(&r, "status") == "scheduled"
                && (!present(&r, "taskClaims") || !s.claims_valid(&r))
            {
                patch(
                    &mut r,
                    json!({"status":"cancelled","dueAt":null,"reason":"The server restarted. Automatic retry was cancelled."}),
                );
                hold = Some("restart-terminal");
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
            if text(&r, "status") == "unknown"
                || present(&r, "acceptedTurnId")
                || flag(&attempt, "submitted")
            {
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
        Action::Error { reason, unknown } => {
            patch(
                &mut r,
                json!({"status":if unknown{"unknown"}else{"failed"},"dueAt":null,"reason":reason}),
            );
        }
        Action::Accepted {
            attempt: receipt,
            turn_id,
        } => {
            // The caller's transport receipt must name the persisted claim, epoch and account.
            if text(receipt, "action") != "capacity"
                || receipt.get("id") != attempt.get("id")
                || receipt.get("capacityRetryId") != r.get("id")
                || receipt.get("epoch") != agent.get("epoch")
                || account(receipt) != account(&agent)
                || present(&r, "acceptedTurnId")
            {
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
        Action::Claim { settings } => {
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
                // Workspace, request, budget and slot admission belong to the turn driver.
                // This pure intent must be checked and reserved by that driver in the same transaction.
                patch(
                    &mut r,
                    json!({"status":"starting","dueAt":null,"claimedAt":now,"reason":null}),
                );
                let attempt = json!({"id":format!("capacity:{}",text(&r,"id")),"epoch":agent["epoch"],"events":[],"action":"capacity","submitted":false,"capacityRetryId":r["id"],"accountKey":r["accountKey"]});
                let epoch = agent["epoch"].clone();
                patch(
                    &mut agent,
                    json!({"status":"starting","inFlight":true,"turnEpoch":epoch,"startAttempt":attempt}),
                );
                r["updatedAt"] = json!(now);
                agent["capacityRetry"] = r.clone();
                return Ok(Decision {
                    agent: Some(agent),
                    record: Some(r),
                    effects: vec![Effect::CapacityStart { attempt }],
                    ..Decision::default()
                });
            }
        }
    }
    r["updatedAt"] = json!(now);
    if !matches!(action, Action::Reset(_)) {
        agent["capacityRetry"] = r.clone();
    }
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
