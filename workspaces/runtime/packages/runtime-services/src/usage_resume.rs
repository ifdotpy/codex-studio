//! Durable usage/authentication continuation decisions; observations never invoke a provider.
use crate::continuation::{
    ContinuationError, Decision, Effect, POLL_SECONDS, Result, Snapshot, account, flag,
    now_checked, number, patch, present, remove, text,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

const RESET_GRACE: f64 = 5.0;
const RESUME_MIN: f64 = 60.0;
const AUTH_BACKOFF: [f64; 3] = [180.0, 600.0, 1800.0];
const FRESH_SECONDS: f64 = 5.0;
pub const CONTINUATION: &str = "The previous turn stopped because this account reached a usage or rate limit. Check the current task and conversation state, then continue the same task. Do not repeat completed work.";
pub const AUTH_CONTINUATION: &str = "The previous turn stopped because this account sign-in failed. Check the current task and conversation state, then continue the same task. Do not repeat completed work.";
const STOPPED: &str = "The chat stopped or changed accounts before automatic resume.";
const CHANGED: &str = "The failed turn or chat state changed before automatic resume.";
pub const TASK_CHANGED: &str = "The assigned task changed before automatic continuation.";
const OFF: &str = "Automatic resume is off for this chat.";

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    UsageLimit,
    RateLimit,
    Auth,
}
#[derive(Clone)]
pub struct LimitsObservation {
    raw: Value,
    marker: Option<String>,
}
impl LimitsObservation {
    pub fn from_record(raw: Value, marker: Option<String>) -> Result<Self> {
        if let Some((path, _)) =
            studio_runtime_schema::first_error("continuation/ContinuationLimitsObservation", &raw)
                .map_err(|_| ContinuationError::Invalid("/limits/schema".into()))?
        {
            return Err(ContinuationError::Invalid(format!("/limits{path}")));
        }
        Ok(Self { raw, marker })
    }
    fn fresh_for(&self, key: &str, after: f64, strict: bool) -> bool {
        !present(&self.raw, "error")
            && !flag(&self.raw, "stale")
            && self
                .raw
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or(key)
                == key
            && number(&self.raw, "at")
                .is_some_and(|at| if strict { at > after } else { at >= after })
    }
    pub fn reset_at(&self) -> Option<f64> {
        reset_at(&self.raw["data"])
    }
    pub fn allowed(&self, now: f64) -> bool {
        allowed(&self.raw["data"], now)
    }
}
pub fn next_check(reset: Option<f64>, now: f64) -> f64 {
    reset
        .filter(|r| *r > now)
        .map_or(now + POLL_SECONDS, |r| r + RESET_GRACE)
}
fn auth_backoff(record: &Value) -> f64 {
    AUTH_BACKOFF[(record
        .get("authAttempt")
        .and_then(Value::as_u64)
        .unwrap_or(0)
        .min(2)) as usize]
}
fn buckets(data: &Value) -> impl Iterator<Item = &Value> {
    std::iter::once(&data["rateLimits"]).chain(
        data.get("rateLimitsByLimitId")
            .and_then(Value::as_object)
            .into_iter()
            .flat_map(|m| m.values()),
    )
}
fn reset_at(data: &Value) -> Option<f64> {
    buckets(data)
        .flat_map(|b| {
            let windows: Vec<&Value> = ["primary", "secondary"]
                .iter()
                .filter_map(|n| b.get(*n))
                .filter(|v| v.is_object())
                .collect();
            let full = windows
                .iter()
                .any(|w| number(w, "usedPercent").is_some_and(|p| p >= 100.0));
            windows
                .into_iter()
                .filter(move |w| {
                    if full {
                        number(w, "usedPercent").is_some_and(|p| p >= 100.0)
                    } else {
                        b.get("rateLimitReachedType").is_some_and(|v| !v.is_null())
                    }
                })
                .filter_map(|w| number(w, "resetsAt"))
        })
        .reduce(f64::max)
}
fn allowed(data: &Value, now: f64) -> bool {
    if !present(data, "rateLimits")
        || data
            .get("ordinaryUsageAllowed")
            .is_some_and(|v| v != &Value::Bool(true))
    {
        return false;
    }
    let mut measured = false;
    for bucket in buckets(data) {
        if !bucket.is_object()
            || bucket
                .get("rateLimitReachedType")
                .is_some_and(|v| !v.is_null())
            || present(bucket, "spendControlReached")
            || number(&bucket["individualLimit"], "remainingPercent").is_some_and(|p| p <= 0.0)
        {
            return false;
        }
        for key in ["primary", "secondary"] {
            if !present(bucket, key) {
                continue;
            }
            let window = &bucket[key];
            let Some(used) = number(window, "usedPercent") else {
                return false;
            };
            measured = true;
            if used >= 100.0 || number(window, "resetsAt").is_some_and(|t| t <= now) {
                return false;
            }
        }
    }
    measured
}
fn cancel(s: &Snapshot, r: &Value, reason: &str, now: f64) -> Decision {
    let mut r = r.clone();
    patch(
        &mut r,
        json!({"status":"cancelled","dueAt":null,"reason":reason}),
    );
    Decision::save(s, r, "usageResume", now)
}
fn excluded(s: &Snapshot, key: &str) -> bool {
    let root = if text(&s.agent, "id") == text(&s.agent, "rootId") {
        Some(&s.agent)
    } else {
        s.ancestors
            .iter()
            .find(|a| text(a, "id") == text(&s.agent, "rootId"))
    };
    root.and_then(|r| r.get("excludedAccountKeys"))
        .and_then(Value::as_array)
        .is_some_and(|keys| keys.iter().any(|k| k.as_str() == Some(key)))
}
/// Called after an account observation; current identity and task ownership are checked anew.
pub fn tick(s: &Snapshot, limits: &LimitsObservation, now: f64) -> Result<Decision> {
    now_checked(now)?;
    let Some(r) = &s.record else {
        return Ok(Decision::default());
    };
    if !s.current("usageResume")
        || text(&s.agent["usageResume"], "status") != "scheduled"
        || s.status() != Some(crate::continuation::WaitStatus::Scheduled)
        || number(r, "dueAt").is_none_or(|t| t > now)
    {
        return Ok(Decision::default());
    }
    let key = account(r);
    if excluded(s, key) {
        let reason =
            format!("Automatic resume stopped because account {key} is disabled for this chat.");
        let mut d = cancel(s, r, &reason, now);
        if let Some(a) = &mut d.agent {
            patch(
                a,
                json!({"status":"failed","inFlight":false,"error":reason}),
            );
        }
        d.hold(text(r, "id"), "account-excluded", &reason);
        return Ok(d);
    }
    if present(&s.agent, "deletedAt")
        || !flag(&s.agent, "autoWake")
        || s.agent.get("usageResumeEnabled") == Some(&Value::Bool(false))
        || account(&s.agent) != key
        || !s.parents_running(false)
    {
        return Ok(cancel(s, r, STOPPED, now));
    }
    if ["threadId", "epoch"]
        .iter()
        .any(|k| s.agent.get(*k) != r.get(*k))
        || s.agent.get("lastCompletedTurn") != r.get("turnId")
        || !flag(&s.agent, "nativeFailureHold")
    {
        return Ok(cancel(s, r, CHANGED, now));
    }
    if !s.claims_valid(r) {
        let mut d = cancel(s, r, TASK_CHANGED, now);
        d.hold(text(r, "id"), "task-changed", TASK_CHANGED);
        return Ok(d);
    }
    let auth = text(r, "cause") == "auth";
    let failed = number(r, "failedAt").unwrap_or(0.0);
    let fresh = limits.fresh_for(key, now - FRESH_SECONDS, false);
    let recovered = if auth {
        number(r, "proofAt").is_some_and(|t| t > failed)
            || limits.fresh_for(key, failed, true)
            || (account(&limits.raw) == key
                && limits.marker.as_ref().is_some_and(|marker| {
                    r.get("authRefreshMarker")
                        .and_then(Value::as_str)
                        .is_some_and(|old| old != marker)
                }))
    } else {
        fresh && limits.allowed(now)
    };
    let mut r = r.clone();
    if !recovered {
        let reset = limits.reset_at();
        let due = if auth {
            now + auth_backoff(&r)
        } else {
            next_check(reset, now)
        };
        patch(
            &mut r,
            json!({"dueAt":due,"plannedAt":reset.filter(|t|*t>now),"resetAt":reset,"lastCheckedAt":limits.raw.get("at"),"waitingForAuth":auth,"reason":if auth {Some("Waiting for the account sign-in.")} else if fresh {None} else {Some("Waiting for a current account limits result.")}}),
        );
        return Ok(Decision::save(s, r, "usageResume", now));
    }
    let id = format!("usage-resume:{}", text(&r, "id"));
    patch(
        &mut r,
        json!({"status":"started","dueAt":null,"startedAt":now,"reason":null}),
    );
    let mut d = Decision::save(s, r, "usageResume", now);
    d.effects.push(Effect::Followup {
        id,
        text: if auth {
            AUTH_CONTINUATION
        } else {
            CONTINUATION
        }
        .into(),
    });
    if let Some(a) = &mut d.agent {
        remove(a, "nativeFailureHold");
        patch(a, json!({"status":"queued","error":null}));
    }
    Ok(d)
}
/// Limit telemetry may advance a wait, but never earlier than the anti-loop floor.
pub fn limits_changed(s: &Snapshot, limits: &LimitsObservation, now: f64) -> Result<Decision> {
    now_checked(now)?;
    let Some(r) = &s.record else {
        return Ok(Decision::default());
    };
    if text(r, "status") != "scheduled"
        || text(r, "cause") == "auth"
        || present(&limits.raw, "error")
        || !present(&limits.raw, "data")
    {
        return Ok(Decision::default());
    }
    let reset = limits.reset_at();
    let mut r = r.clone();
    patch(
        &mut r,
        json!({"resetAt":reset,"plannedAt":reset.filter(|t|*t>now)}),
    );
    if limits.allowed(now) {
        r["dueAt"] = json!(now.max(number(&r, "failedAt").unwrap_or(now) + RESUME_MIN));
    } else if reset.is_some_and(|t| t > now) {
        r["dueAt"] = json!(next_check(reset, now));
    }
    let mut d = Decision::save(s, r, "usageResume", now);
    if !s.current("usageResume") {
        d.agent = None;
    }
    Ok(d)
}
/// Existing exact cancelled rows can be re-enabled; a changed failed turn needs turn capture.
pub fn set_enabled(s: &Snapshot, enabled: bool, now: f64) -> Result<Decision> {
    now_checked(now)?;
    let Some(r) = &s.record else {
        return Err(ContinuationError::Invalid("/record".into()));
    };
    if !s.current("usageResume") {
        return Ok(Decision::default());
    }
    let mut r = r.clone();
    if !enabled && text(&r, "status") == "scheduled" {
        patch(
            &mut r,
            json!({"status":"cancelled","dueAt":null,"reason":OFF}),
        );
    } else if enabled
        && text(&r, "status") == "cancelled"
        && text(&r, "reason") == OFF
        && flag(&s.agent, "nativeFailureHold")
        && text(&s.agent, "lastCompletedTurnStatus") == "failed"
        && recoverable_error(&s.agent["error"], text(&s.agent, "provider"))?
    {
        if s.agent.get("lastCompletedTurn") != r.get("turnId") {
            return Err(ContinuationError::Unavailable(
                "turn-driver/failed-turn-capture",
            ));
        }
        let due = if text(&r, "cause") == "auth" {
            now + POLL_SECONDS
        } else {
            next_check(number(&r, "resetAt"), now)
        };
        patch(
            &mut r,
            json!({"status":"scheduled","reason":null,"dueAt":due}),
        );
    }
    let mut d = Decision::save(s, r, "usageResume", now);
    if let Some(a) = &mut d.agent {
        a["usageResumeEnabled"] = json!(enabled);
    }
    Ok(d)
}

/// Turn completion invokes this with its already-classified, known terminal failure.
/// The caller must query this exact identity before supplying the snapshot.
pub fn failure_identity(s: &Snapshot, turn_id: &str) -> String {
    crate::event_write::request_signature(&json!([
        s.agent_id(),
        account(&s.agent),
        s.agent["threadId"],
        s.agent["epoch"],
        turn_id
    ]))
}
pub struct FailureObservation<'a> {
    pub turn_id: &'a str,
    pub cause: Cause,
    pub error_reset: Option<f64>,
    pub auth_attempt: u64,
    pub limits: &'a LimitsObservation,
}
pub fn record_failure(s: &Snapshot, failure: FailureObservation<'_>, now: f64) -> Result<Decision> {
    now_checked(now)?;
    if crate::continuation::native_thread_blocked(&s.agent)
        || !flag(&s.agent, "autoWake")
        || present(&s.agent, "deletedAt")
        || s.agent.get("usageResumeEnabled") == Some(&Value::Bool(false))
        || !present(&s.agent, "threadId")
        || s.agent
            .get("turnEpoch")
            .is_some_and(|e| Some(e) != s.agent.get("epoch"))
    {
        return Ok(Decision::default());
    }
    let id = failure_identity(s, failure.turn_id);
    if s.record.as_ref().is_some_and(|r| text(r, "id") == id) {
        return Ok(Decision::default());
    }
    let is_auth = failure.cause == Cause::Auth;
    let reset = if is_auth {
        failure.error_reset
    } else {
        failure
            .limits
            .reset_at()
            .into_iter()
            .chain(failure.error_reset)
            .reduce(f64::max)
    };
    let excluded = excluded(s, account(&s.agent));
    let reason = excluded.then(|| {
        format!(
            "Automatic resume stopped because account {} is disabled for this chat.",
            account(&s.agent)
        )
    });
    let mut r = json!({"id":id,"status":if excluded{"cancelled"}else{"scheduled"},"accountKey":account(&s.agent),"threadId":s.agent["threadId"],"epoch":s.agent["epoch"],"turnId":failure.turn_id,"cause":failure.cause,"failedAt":now,"authAttempt":if is_auth {failure.auth_attempt}else{0},"authRefreshMarker":if is_auth&&!excluded {failure.limits.marker.as_ref()}else{None},"dueAt":null,"plannedAt":if excluded{None}else{reset.filter(|t|*t>now)},"resetAt":if excluded{None}else{reset},"reason":reason,"taskClaims":s.claims});
    if !excluded {
        r["dueAt"] = json!(if is_auth {
            now + auth_backoff(&r)
        } else {
            next_check(reset, now)
        });
    }
    let mut d = Decision::save(s, r, "usageResume", now);
    if let Some(a) = &mut d.agent {
        if is_auth {
            a["authResumeAttempt"] = json!(failure.auth_attempt.saturating_add(1));
        }
        if let Some(reason) = &reason {
            patch(
                a,
                json!({"status":"failed","inFlight":false,"error":reason}),
            );
        }
    }
    if let Some(reason) = reason {
        d.hold(&id, "account-excluded", &reason);
    }
    Ok(d)
}
/// One successful turn is proof only for older auth waits on the same account.
pub fn completed_proof(s: &Snapshot, account_key: &str, now: f64) -> Result<Decision> {
    now_checked(now)?;
    let Some(r) = &s.record else {
        return Ok(Decision::default());
    };
    if account(r) != account_key
        || text(r, "cause") != "auth"
        || text(r, "status") != "scheduled"
        || number(r, "failedAt").is_none_or(|t| t >= now)
    {
        return Ok(Decision::default());
    }
    let mut r = r.clone();
    patch(&mut r, json!({"proofAt":now,"dueAt":now}));
    let agent = if s.current("usageResume") {
        let mut a = s.agent.clone();
        a["usageResume"] = r.clone();
        Some(a)
    } else {
        None
    };
    Ok(Decision {
        agent,
        record: Some(r),
        ..Decision::default()
    })
}

fn recoverable_error(error: &Value, provider: &str) -> Result<bool> {
    let info = error.get("codexErrorInfo");
    let kind = info
        .and_then(Value::as_str)
        .or_else(|| {
            info.and_then(Value::as_object)
                .and_then(|m| m.keys().next().map(String::as_str))
        })
        .unwrap_or("");
    if matches!(kind, "usageLimitExceeded" | "rateLimitExceeded")
        || kind.to_ascii_lowercase().contains("auth")
    {
        return Ok(true);
    }
    static VALIDATOR: std::sync::OnceLock<std::result::Result<jsonschema::Validator, String>> =
        std::sync::OnceLock::new();
    let validator=VALIDATOR.get_or_init(||jsonschema::validator_for(&json!({"type":"object","anyOf":[
      {"properties":{"message":{"pattern":r"(?i)\b401\b"}},"required":["message"]},
      {"properties":{"message":{"allOf":[{"pattern":r"(?i)\b403\b"},{"pattern":r"(?i)\b(?:invalid|expired|revoked).{0,32}\b(?:token|key)\b|\b(?:token|key)\b.{0,32}\b(?:invalid|expired|revoked)\b|\bsign[ -]?in\b|\bauthentication required\b|\bunauthorized\b"}]}},"required":["message"]},
      {"properties":{"provider":{"const":"claude"},"message":{"pattern":"(?i)oauth session expired|failed to authenticate"}},"required":["provider","message"]}
    ]})).map_err(|e|e.to_string())).as_ref().map_err(|_|ContinuationError::Invalid("/error/schema".into()))?;
    Ok(validator.is_valid(&json!({"message":text(error,"message"),"provider":provider})))
}
