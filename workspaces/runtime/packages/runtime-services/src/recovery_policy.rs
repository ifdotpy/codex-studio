//! Recovery eligibility is a read decision. Silence never authorizes input replay.
use crate::event_write::truthy;
use serde::Serialize;
use serde_json::{Value, json};

pub const MAX_EVENTS: usize = 32;
pub const UNKNOWN_START_HOLD_SECONDS: f64 = 600.0;
const DISCONNECTED: &str = "Codex disconnected. Review the transcript before resuming.";
const RESTARTED: &str =
    "Server restarted during a turn. Review history, then send a new instruction.";
const VM_DISCONNECTED: &str = "Linux VM provider disconnected. The turn outcome is unknown. Review the transcript before resuming.";
const SUPERVISOR_UNKNOWN: &str =
    "Cannot verify the existing supervisor child; native outcome remains unknown";
const ACTIVE_REPAIR: &[&str] = &["preparing", "submitted", "unknown", "ready"];

#[derive(Debug, thiserror::Error)]
#[error("invalid recovery record at {location}")]
pub struct InvalidRecoveryRecord {
    pub location: String,
}

/// Built only from persisted JSON. Keep the source intact for compare-and-apply.
#[derive(Clone)]
pub struct RecoverySnapshot {
    agent: Value,
}
impl RecoverySnapshot {
    pub fn from_record(agent: Value) -> Result<Self, InvalidRecoveryRecord> {
        if !studio_runtime_schema::validate("RecoveryAgentRecord", &agent).map_err(|_| {
            InvalidRecoveryRecord {
                location: "RecoveryAgentRecord.schema".into(),
            }
        })? {
            return Err(InvalidRecoveryRecord {
                location: "runtime_agents.recovery".into(),
            });
        }
        Ok(Self { agent })
    }
    pub fn record(&self) -> &Value {
        &self.agent
    }
    pub fn eligibility(&self) -> Eligibility {
        eligibility(&self.agent)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Eligibility {
    pub orphan_busy: bool,
    pub unconfirmed_start: bool,
    pub preparation: bool,
    pub disconnected_preparation: bool,
    pub restart_turn: bool,
    pub queued_restart: bool,
    pub queued_active_wait: bool,
    pub transport_turn: bool,
    pub connection: bool,
    pub task_owner: bool,
}
fn has(v: &Value, k: &str) -> bool {
    truthy(v.get(k))
}
fn one(v: &Value, values: &[&str]) -> bool {
    v.as_str().is_some_and(|s| values.contains(&s))
}
fn same(a: &Value, b: &Value, keys: &[&str]) -> bool {
    keys.iter().all(|k| {
        a.get(*k) == b.get(*k)
            || a.get(*k).unwrap_or(&Value::Null) == b.get(*k).unwrap_or(&Value::Null)
    })
}
fn account_equal(a: &Value, b: &Value) -> bool {
    a.get("accountKey").cloned().unwrap_or(json!("default"))
        == b.get("accountKey").cloned().unwrap_or(json!("default"))
}
fn thread_block(a: &Value) -> bool {
    (has(a, "threadId") && a["nativeThreadBlock"]["threadId"] == a["threadId"])
        || (has(a, "threadId")
            && (a["error"]["codexErrorInfo"] == "misalignmentPolicyViolation"
                || (has(&a["nativeTurnError"], "turnId")
                    && a["nativeTurnError"]["turnId"] == a["turnId"]
                    && a["nativeTurnError"]["error"]["codexErrorInfo"]
                        == "misalignmentPolicyViolation")))
}
fn clear(a: &Value) -> bool {
    ![
        "deletedAt",
        "nativeFailureHold",
        "accountTransferId",
        "workspaceOperation",
    ]
    .iter()
    .any(|k| has(a, k))
        && !thread_block(a)
}
fn attempt_matches(a: &Value, t: &Value) -> bool {
    has(t, "id")
        && t["epoch"] == a["epoch"]
        && account_equal(t, a)
        && !has(t, "action")
        && !has(t, "activeAtReservation")
        && !has(t, "turnId")
        && !has(t, "observedTurnId")
}
fn events_valid(t: &Value) -> bool {
    t["events"].as_array().is_some_and(|events| {
        !events.is_empty()
            && events.len() <= MAX_EVENTS
            && events
                .iter()
                .all(|e| e.as_str().is_some_and(|s| !s.is_empty()))
            && events
                .iter()
                .collect::<std::collections::HashSet<_>>()
                .len()
                == events.len()
    })
}
fn scope(a: &Value) -> Value {
    json!({"id":a["id"],"accountKey":a["accountKey"],"epoch":a["epoch"],"threadId":a["threadId"]})
}
fn eligibility(a: &Value) -> Eligibility {
    let t = &a["startAttempt"];
    let d = &a["disconnectRecovery"];
    let r = &a["restartRecovery"];
    let w = &a["contextRepairWait"];
    let repair = &a["contextRepair"];
    let c = &a["connectionRecovery"];
    let inactive = !has(a, "inFlight");
    let stopped = inactive && !has(a, "autoWake");
    let turn_epoch = a.get("turnEpoch").unwrap_or(&a["epoch"]) == &a["epoch"];
    let orphan_busy = has(a, "inFlight")
        && !has(a, "turnId")
        && !has(a, "startAttempt")
        && has(a, "threadId")
        && !has(a, "deletedAt");
    let unconfirmed_start = (has(a, "inFlight") || a["startOutcomeHold"]["stage"] == "held")
        && has(a, "autoWake")
        && !has(a, "turnId")
        && has(a, "threadId")
        && ![
            "deletedAt",
            "nativeFailureHold",
            "accountTransferId",
            "workspaceOperation",
        ]
        .iter()
        .any(|k| has(a, k))
        && !has(t, "action")
        && has(t, "id")
        && t["submitted"] == true
        && (t["executionOutcome"] == "unknown"
            || a["error"] == "turn/start response timed out; outcome unknown")
        && t["epoch"] == a["epoch"]
        && account_equal(t, a)
        && t["threadId"] == a["threadId"]
        && has(t, "events")
        && !has(t, "turnId")
        && !has(t, "observedTurnId");
    let busy_claude = a["provider"] == "claude"
        && has(a, "threadId")
        && a["error"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            == Some(json!({"code":-32000,"message":"Claude is still working"}));
    let preparation = a["status"] == "failed"
        && has(a, "autoWake")
        && inactive
        && !has(a, "turnId")
        && clear(a)
        && (busy_claude
            || one(
                &a["error"],
                &[
                    "AF_UNIX path too long",
                    "[Errno 2] No such file or directory",
                    SUPERVISOR_UNKNOWN,
                    "Supervisor native launch settings changed; existing work was preserved",
                    "Supervisor open failed: Supervisor handle exists with an incompatible or stopped child",
                ],
            ))
        && attempt_matches(a, t)
        && t["submitted"] == false
        && has(t, "events");
    let dt = &d["startAttempt"];
    let disconnected_preparation = a["status"] == "interrupted"
        && stopped
        && !has(a, "startAttempt")
        && !has(a, "turnId")
        && one(&a["error"], &[DISCONNECTED, VM_DISCONNECTED])
        && has(d, "autoWake")
        && !has(d, "turnId")
        && same(a, d, &["epoch", "accountKey", "threadId"])
        && attempt_matches(a, dt)
        && dt["submitted"] == false
        && events_valid(dt)
        && clear(a)
        && !one(&r["stage"], &["pending", "held", "superseded"]);
    let state_match = (one(
        &a["status"],
        &["interrupted", "paused", "completed", "failed"],
    ) && (a["turnId"].is_null() || a["turnId"] == r["turnId"]))
        || (a["status"] == "running" && has(a, "inFlight") && has(a, "autoWake"));
    let restart_turn = one(&r["stage"], &["pending", "superseded"])
        && has(r, "autoWake")
        && has(r, "turnId")
        && d["source"] == "restart"
        && r["epoch"] == a["epoch"]
        && account_equal(r, a)
        && r["threadId"] == a["threadId"]
        && state_match
        && clear(a);
    let mut wait_scope = scope(a);
    wait_scope["attemptId"] = t["id"].clone();
    let supervisor_wait =
        a["status"] == "failed" && !has(a, "contextRepairWait") && a["error"] == SUPERVISOR_UNKNOWN;
    let context_wait = a["status"] == "queued"
        && one(
            &a["error"],
            &[
                "Context repair waits for the existing native recovery receipt",
                "The native session was active at the last check. Your message remains queued.",
            ],
        )
        && w["error"] == a["error"]
        && one(&w["scope"], &["local", "native"])
        && w["source"] == wait_scope
        && w["events"] == t["events"]
        && !has(w, "action");
    let queued_restart = (supervisor_wait || context_wait)
        && has(a, "autoWake")
        && inactive
        && clear(a)
        && has(a, "threadId")
        && has(r, "turnId")
        && (a["turnId"].is_null() || a["turnId"] == r["turnId"])
        && attempt_matches(a, t)
        && t["submitted"] == false
        && has(t, "events")
        && r["stage"] == "pending"
        && has(r, "autoWake")
        && same(r, a, &["epoch", "accountKey", "threadId"])
        && !one(&repair["phase"], ACTIVE_REPAIR);
    let surviving = r["stage"] == "finished"
        && has(r, "autoWake")
        && d["source"] == "restart"
        && has(d, "autoWake")
        && same(d, a, &["epoch", "accountKey", "threadId", "turnId"])
        && repair["phase"] == "failed"
        && repair["error"].as_str().is_some_and(|s| {
            s.starts_with("Context repair requires a confirmed idle native thread; native status: ")
        })
        && repair["source"]["threadId"] == a["threadId"];
    let continued = r["stage"] == "continued"
        && r["outcome"] == "active"
        && c["source"] == "native_thread_read"
        && c["outcome"] == "active"
        && c["automatic"] == true
        && c["turnId"] == a["turnId"];
    let same_repair = repair["agent"] == a["id"]
        && same(&repair["source"], &scope(a), &["id", "accountKey", "epoch"])
        && ((repair["phase"] == "unchanged" && repair["source"]["threadId"] == a["threadId"])
            || (repair["phase"] == "completed" && repair["newThreadId"] == a["threadId"])
            || surviving);
    let queued_active_wait = a["status"] == "queued"
        && has(a, "autoWake")
        && inactive
        && has(a, "threadId")
        && has(a, "turnId")
        && turn_epoch
        && clear(a)
        && same_repair
        && a["error"] == "Context repair waits for the current agent operation"
        && w["error"] == a["error"]
        && w["scope"] == "local"
        && w["source"] == wait_scope
        && !["action", "actionRequestId", "actionIdentity"]
            .iter()
            .any(|k| has(w, k) || has(t, k))
        && attempt_matches(a, t)
        && t["submitted"] == false
        && events_valid(t)
        && w["events"] == t["events"]
        && (continued || surviving)
        && has(r, "autoWake")
        && same(r, a, &["epoch", "accountKey", "threadId", "turnId"]);
    let transport_turn = a["status"] == "interrupted"
        && stopped
        && has(a, "threadId")
        && has(a, "turnId")
        && one(&a["error"], &[DISCONNECTED, VM_DISCONNECTED])
        && clear(a)
        && !has(a, "contextRepairWait")
        && !one(&repair["phase"], ACTIVE_REPAIR)
        && !one(&r["stage"], &["pending", "superseded"])
        && d["source"] != "restart"
        && has(d, "autoWake")
        && same(d, a, &["epoch", "accountKey", "threadId", "turnId"])
        && turn_epoch
        && (!has(a, "startAttempt")
            || (t == &d["startAttempt"]
                && has(t, "id")
                && t["submitted"] == true
                && !has(t, "action")
                && t["epoch"] == a["epoch"]
                && account_equal(t, a)
                && (t["turnId"].is_null() || t["turnId"] == a["turnId"])
                && (t["observedTurnId"].is_null() || t["observedTurnId"] == a["turnId"])));
    let connection = disconnected_preparation
        || preparation
        || queued_restart
        || queued_active_wait
        || restart_turn
        || transport_turn
        || (a["status"] == "interrupted"
            && stopped
            && !has(a, "startAttempt")
            && !one(&r["stage"], &["pending", "superseded"])
            && !["accountTransferId", "workspaceOperation", "deletedAt"]
                .iter()
                .any(|k| has(a, k))
            && has(a, "threadId")
            && has(a, "turnId")
            && one(&a["error"], &[DISCONNECTED, RESTARTED])
            && !thread_block(a));
    let completed_owner =
        a["status"] == "completed" && inactive && !has(a, "turnId") && !has(a, "startAttempt");
    let task_owner = !has(a, "deletedAt")
        && a.get("provider").is_none_or(|p| p == "codex")
        && has(a, "threadId")
        && !has(a, "accountTransferId")
        && !has(a, "workspaceOperation")
        && (!has(a, "contextRepairWait") || completed_owner)
        && !one(&a["status"], &["queued", "starting"])
        && !one(&repair["phase"], ACTIVE_REPAIR);
    Eligibility {
        orphan_busy,
        unconfirmed_start,
        preparation,
        disconnected_preparation,
        restart_turn,
        queued_restart,
        queued_active_wait,
        transport_turn,
        connection,
        task_owner,
    }
}

/// Exact native operation receipts are required before delivering completion.
pub fn native_operations_settled(turn: &Value) -> bool {
    turn.get("items").is_none()
        || turn["items"].as_array().is_some_and(|items| {
            items.iter().all(|item| match item["type"].as_str() {
                Some("commandExecution") => {
                    one(&item["status"], &["completed", "failed", "declined"])
                        && (item["status"] == "declined"
                            || item["exitCode"].is_i64()
                            || item["exitCode"].is_u64())
                }
                Some("dynamicToolCall") => item["status"] == "completed",
                Some("mcpToolCall" | "fileChange" | "computerToolCall") => {
                    one(&item["status"], &["completed", "failed", "declined"])
                }
                _ => true,
            })
        })
}
