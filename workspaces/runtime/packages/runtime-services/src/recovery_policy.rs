//! Recovery eligibility is a read decision. Silence never authorizes input replay.
use crate::event_write::truthy;
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_schema::provider_errors::is_misalignment_policy_violation;
use studio_runtime_schema::recovery_record::{RecoveryAgentRecord, RecoveryFields, RecoverySource};
use studio_runtime_schema::shared_values::LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR;
use studio_runtime_store::{
    BACKEND_RESTARTED_TURN_ERROR, PROVIDER_DISCONNECTED_TURN_ERROR,
    VM_PROVIDER_DISCONNECTED_TURN_ERROR,
};

static NULL_VALUE: Value = Value::Null;

pub const UNKNOWN_START_HOLD_SECONDS: f64 = 600.0;
const SUPERVISOR_UNKNOWN: &str =
    "Cannot verify the existing supervisor child; native outcome remains unknown";
const ACTIVE_REPAIR: &[&str] = &["preparing", "submitted", "unknown", "ready"];

#[derive(Debug, thiserror::Error)]
pub enum InvalidRecoveryRecord {
    #[error("invalid recovery record at {location}")]
    Schema { location: &'static str },
    #[error("invalid recovery record {table}/{id}")]
    Stored { table: &'static str, id: String },
}

/// Built only from persisted JSON. Keep the source intact for compare-and-apply.
#[derive(Clone)]
pub struct RecoverySnapshot {
    agent: Arc<Value>,
    typed: Arc<RecoveryAgentRecord>,
}
/// Proof from the recovery policy that this saved turn may be checked automatically.
/// Private fields keep the account caller and completion writer on one eligibility owner.
pub(crate) struct AutomaticTurnRecovery<'a> {
    record: &'a Value,
    typed: &'a RecoveryAgentRecord,
    kind: AutomaticTurnKind,
}

enum AutomaticTurnKind {
    Restart,
    Transport,
}

impl AutomaticTurnRecovery<'_> {
    pub(crate) fn marker_key(&self) -> &'static str {
        match self.kind {
            AutomaticTurnKind::Restart => "restartRecovery",
            AutomaticTurnKind::Transport => "disconnectRecovery",
        }
    }

    pub(crate) fn can_continue(&self, turn_id: &str) -> bool {
        let receipt = &self.record[self.marker_key()];
        receipt["turnId"] == turn_id
            && match self.kind {
                AutomaticTurnKind::Restart => {
                    matches!(
                        self.typed
                            .restart_recovery
                            .as_deref()
                            .and_then(|r| r.stage.as_deref()),
                        Some("pending" | "superseded")
                    )
                }
                AutomaticTurnKind::Transport => true,
            }
    }
}

impl RecoverySnapshot {
    pub fn from_record(agent: Value) -> Result<Self, InvalidRecoveryRecord> {
        let typed: RecoveryAgentRecord =
            serde::Deserialize::deserialize(&agent).map_err(|_| InvalidRecoveryRecord::Schema {
                location: "runtime_agents.recovery",
            })?;
        Ok(Self {
            agent: Arc::new(agent),
            typed: Arc::new(typed),
        })
    }
    pub fn record(&self) -> &Value {
        self.agent.as_ref()
    }
    pub fn start_attempt(&self) -> &Value {
        self.agent
            .get("startAttempt")
            .filter(|attempt| !attempt.is_null())
            .or_else(|| {
                self.agent
                    .get("restartRecovery")
                    .and_then(|r| r.get("startAttempt"))
            })
            .unwrap_or(&NULL_VALUE)
    }
    pub(crate) fn restart_input_wait(&self) -> bool {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Marker {
            stage: Option<String>,
        }
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Record {
            restart_recovery: Option<Marker>,
            context_repair_wait: Option<serde::de::IgnoredAny>,
        }
        serde_json::from_str::<Record>(&self.agent.to_string())
            .ok()
            .is_some_and(|record| {
                record
                    .restart_recovery
                    .and_then(|marker| marker.stage)
                    .as_deref()
                    == Some("held")
                    && record.context_repair_wait.is_some()
            })
    }

    pub(crate) fn is_unclean_start(&self) -> bool {
        unclean_start_candidate(&self.agent.to_string()) && clear(&self.agent, &self.typed)
    }

    pub fn eligibility(&self) -> Eligibility {
        eligibility(&self.agent, &self.typed)
    }

    pub(crate) fn automatic_turn(&self) -> Option<AutomaticTurnRecovery<'_>> {
        let eligible = self.eligibility();
        let kind = if eligible.restart_turn {
            AutomaticTurnKind::Restart
        } else if eligible.transport_turn {
            AutomaticTurnKind::Transport
        } else {
            return None;
        };
        Some(AutomaticTurnRecovery {
            record: self.record(),
            typed: &self.typed,
            kind,
        })
    }

    /// Stable identity for one persisted disconnect/restart recovery. A later
    /// transport receipt changes the identity even when the agent turn is the
    /// same, so an older saved result cannot answer a new recovery.
    pub fn connection_recovery_id(&self) -> String {
        connection_recovery_id(&self.agent)
    }
}

pub fn connection_recovery_id(agent: &Value) -> String {
    studio_runtime_store::sha256_hex(connection_recovery_basis(agent).to_string().as_bytes())
}

pub fn connection_recovery_basis(agent: &Value) -> Value {
    let disconnect = &agent["disconnectRecovery"];
    let restart = &agent["restartRecovery"];
    json!([
        agent["id"],
        agent["epoch"],
        agent.get("accountKey").cloned().unwrap_or(json!("default")),
        agent["threadId"],
        agent["turnId"],
        disconnect
            .get("connectionId")
            .cloned()
            .unwrap_or(Value::Null),
        disconnect.get("at").cloned().unwrap_or(Value::Null),
        disconnect["startAttempt"]
            .get("id")
            .cloned()
            .unwrap_or(Value::Null),
        disconnect.get("turnId").cloned().unwrap_or(Value::Null),
        restart.get("connectionId").cloned().unwrap_or(Value::Null),
        restart.get("at").cloned().unwrap_or(Value::Null),
        restart.get("turnId").cloned().unwrap_or(Value::Null),
        restart["startAttempt"]
            .get("id")
            .cloned()
            .unwrap_or(Value::Null),
    ])
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
fn thread_block(a: &Value, typed: &RecoveryAgentRecord) -> bool {
    (typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
        && a["nativeThreadBlock"]["threadId"] == a["threadId"])
        || (typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
            && (is_misalignment_policy_violation(&a["error"])
                || (typed
                    .native_turn_error
                    .as_deref()
                    .and_then(|e| e.turn_id.as_deref())
                    .is_some_and(|s| !s.is_empty())
                    && a["nativeTurnError"]["turnId"] == a["turnId"]
                    && is_misalignment_policy_violation(&a["nativeTurnError"]["error"]))))
}
fn clear(a: &Value, typed: &RecoveryAgentRecord) -> bool {
    ![
        "deletedAt",
        "nativeFailureHold",
        "accountTransferId",
        "workspaceOperation",
    ]
    .iter()
    .any(|k| has(a, k))
        && !thread_block(a, typed)
}
fn attempt_matches(a: &Value, t: &Value, typed: &RecoveryFields) -> bool {
    typed.id.as_deref().is_some_and(|s| !s.is_empty())
        && t["epoch"] == a["epoch"]
        && account_equal(t, a)
        && !has(t, "action")
        && !typed.active_at_reservation.unwrap_or(false)
        && !typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && !typed
            .observed_turn_id
            .as_deref()
            .is_some_and(|s| !s.is_empty())
}
fn events_valid(t: &RecoveryFields) -> bool {
    t.events.as_ref().is_some_and(|events| {
        !events.is_empty()
            && events.iter().all(|event| !event.is_empty())
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

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UncleanStartCandidate {
    epoch: i64,
    account_key: Option<String>,
    thread_id: Option<String>,
    status: String,
    in_flight: Option<bool>,
    auto_wake: Option<bool>,
    start_attempt: UncleanAttempt,
    start_outcome_hold: UncleanHold,
    deleted_at: Option<serde::de::IgnoredAny>,
    native_failure_hold: Option<bool>,
    account_transfer_id: Option<String>,
    workspace_operation: Option<serde::de::IgnoredAny>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UncleanAttempt {
    id: String,
    epoch: i64,
    account_key: Option<String>,
    thread_id: Option<String>,
    submitted: bool,
    events: Vec<String>,
    turn_id: Option<String>,
    observed_turn_id: Option<String>,
    action: Option<serde::de::IgnoredAny>,
    active_at_reservation: Option<bool>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct UncleanHold {
    stage: String,
    reason: Option<String>,
    attempt_id: Option<String>,
}

fn unclean_start_candidate(raw: &str) -> bool {
    let Ok(candidate) = serde_json::from_str::<UncleanStartCandidate>(raw) else {
        return false;
    };
    let attempt = &candidate.start_attempt;
    let hold = &candidate.start_outcome_hold;
    let mut events = std::collections::BTreeSet::new();
    candidate.status == "interrupted"
        && candidate.in_flight == Some(false)
        && candidate.auto_wake == Some(false)
        && candidate.deleted_at.is_none()
        && candidate.native_failure_hold != Some(true)
        && candidate.account_transfer_id.is_none()
        && candidate.workspace_operation.is_none()
        && candidate
            .thread_id
            .as_deref()
            .is_some_and(|thread| !thread.is_empty())
        && attempt
            .thread_id
            .as_ref()
            .is_none_or(|thread| Some(thread) == candidate.thread_id.as_ref())
        && candidate.account_key.as_deref().unwrap_or("default")
            == attempt.account_key.as_deref().unwrap_or("default")
        && candidate.epoch == attempt.epoch
        && !attempt.id.is_empty()
        && !attempt.submitted
        && attempt.turn_id.is_none()
        && attempt.observed_turn_id.is_none()
        && attempt.action.is_none()
        && attempt.active_at_reservation != Some(true)
        && !attempt.events.is_empty()
        && attempt
            .events
            .iter()
            .all(|event| !event.is_empty() && events.insert(event))
        && hold.stage == "held"
        && hold.reason.as_deref()
            == Some(studio_runtime_store::SchedulerRepository::UNCLEAN_START_HOLD)
        && hold.attempt_id.as_deref() == Some(attempt.id.as_str())
}

fn eligibility(a: &Value, typed: &RecoveryAgentRecord) -> Eligibility {
    let empty = RecoveryFields::default();
    let rt = typed.restart_recovery.as_deref().unwrap_or(&empty);
    let tt = typed
        .start_attempt
        .as_deref()
        .or(rt.start_attempt.as_deref())
        .unwrap_or(&empty);
    let d_typed = typed.disconnect_recovery.as_deref().unwrap_or(&empty);
    let dt_typed = d_typed.start_attempt.as_deref().unwrap_or(&empty);
    let repair_typed = typed.context_repair.as_deref().unwrap_or(&empty);

    let r = &a["restartRecovery"];
    let t = a
        .get("startAttempt")
        .filter(|v| !v.is_null())
        .unwrap_or(&r["startAttempt"]);
    let d = &a["disconnectRecovery"];
    let w = &a["contextRepairWait"];
    let repair = &a["contextRepair"];
    let c = &a["connectionRecovery"];
    let inactive = !typed.in_flight.unwrap_or(false);
    let stopped = inactive && !typed.auto_wake.unwrap_or(false);
    let turn_epoch = a.get("turnEpoch").unwrap_or(&a["epoch"]) == &a["epoch"];
    let orphan_busy = typed.in_flight.unwrap_or(false)
        && !typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && !has(a, "startAttempt")
        && typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
        && !has(a, "deletedAt");
    let restart_input_wait = rt.stage.as_deref() == Some("held") && has(a, "contextRepairWait");
    let unconfirmed_start = (unclean_start_candidate(&a.to_string()) && clear(a, typed))
        || (typed.in_flight.unwrap_or(false)
            || typed
                .start_outcome_hold
                .as_deref()
                .and_then(|hold| hold.stage.as_deref())
                == Some("held")
            || restart_input_wait)
            && typed.auto_wake.unwrap_or(false)
            && !typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
            && typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
            && ![
                "deletedAt",
                "nativeFailureHold",
                "accountTransferId",
                "workspaceOperation",
            ]
            .iter()
            .any(|k| has(a, k))
            && !has(t, "action")
            && tt.id.as_deref().is_some_and(|s| !s.is_empty())
            && tt.submitted == Some(true)
            && (tt.execution_outcome.as_deref() == Some("unknown")
                // A process stop can end a submitted start before its response
                // records an outcome; a held restart marker keeps that attempt
                // without one, which is unknown too.
                || (restart_input_wait
                    && !has(a, "startAttempt")
                    && tt.execution_outcome.is_none())
                || a["error"] == "turn/start response timed out; outcome unknown")
            && t["epoch"] == a["epoch"]
            && account_equal(t, a)
            && t["threadId"] == a["threadId"]
            && tt.events.as_ref().is_some_and(|events| !events.is_empty())
            && !tt.turn_id.as_deref().is_some_and(|s| !s.is_empty())
            && !tt
                .observed_turn_id
                .as_deref()
                .is_some_and(|s| !s.is_empty());
    let busy_provider = typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
        && a["error"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            .zip(studio_runtime_providers::ProviderKind::of_record(a).ok())
            .is_some_and(|(error, kind)| {
                crate::provider_adapter::adapter(kind).is_busy_error(&error)
            });
    let preparation = typed.status == "failed"
        && typed.auto_wake.unwrap_or(false)
        && inactive
        && !typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && clear(a, typed)
        && (busy_provider
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
        && attempt_matches(a, t, tt)
        && tt.submitted == Some(false)
        && tt.events.as_ref().is_some_and(|events| !events.is_empty());
    let dt = &d["startAttempt"];
    let disconnected_preparation = typed.status == "interrupted"
        && stopped
        && !has(a, "startAttempt")
        && !typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && one(
            &a["error"],
            &[
                PROVIDER_DISCONNECTED_TURN_ERROR,
                VM_PROVIDER_DISCONNECTED_TURN_ERROR,
                LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
            ],
        )
        && d_typed.auto_wake.unwrap_or(false)
        && !d_typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && same(a, d, &["epoch", "accountKey", "threadId"])
        && attempt_matches(a, dt, dt_typed)
        && dt_typed.submitted == Some(false)
        && events_valid(dt_typed)
        && clear(a, typed)
        && !matches!(rt.stage.as_deref(), Some("pending" | "held" | "superseded"));
    let state_match = (matches!(
        typed.status.as_str(),
        "interrupted" | "paused" | "completed" | "failed"
    ) && (a["turnId"].is_null() || a["turnId"] == r["turnId"]))
        || (typed.status == "running"
            && typed.in_flight.unwrap_or(false)
            && typed.auto_wake.unwrap_or(false));
    let pending_restart_turn = matches!(rt.stage.as_deref(), Some("pending" | "superseded"))
        && rt.auto_wake.unwrap_or(false)
        && rt.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && matches!(d_typed.source.as_ref(), Some(RecoverySource::Text(source)) if source == "restart")
        && r["epoch"] == a["epoch"]
        && account_equal(r, a)
        && r["threadId"] == a["threadId"]
        && state_match
        && clear(a, typed);
    let mut wait_scope = scope(a);
    wait_scope["attemptId"] = t["id"].clone();
    let supervisor_wait = typed.status == "failed"
        && !has(a, "contextRepairWait")
        && a["error"] == SUPERVISOR_UNKNOWN;
    let context_wait = typed.status == "queued"
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
        && typed.auto_wake.unwrap_or(false)
        && inactive
        && clear(a, typed)
        && typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
        && rt.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && (a["turnId"].is_null() || a["turnId"] == r["turnId"])
        && attempt_matches(a, t, tt)
        && tt.submitted == Some(false)
        && tt.events.as_ref().is_some_and(|events| !events.is_empty())
        && rt.stage.as_deref() == Some("pending")
        && rt.auto_wake.unwrap_or(false)
        && same(r, a, &["epoch", "accountKey", "threadId"])
        && !repair_typed
            .phase
            .as_deref()
            .is_some_and(|s| ACTIVE_REPAIR.contains(&s));
    let surviving = rt.stage.as_deref() == Some("finished")
        && rt.auto_wake.unwrap_or(false)
        && matches!(d_typed.source.as_ref(), Some(RecoverySource::Text(source)) if source == "restart")
        && d_typed.auto_wake.unwrap_or(false)
        && same(d, a, &["epoch", "accountKey", "threadId", "turnId"])
        && repair_typed.phase.as_deref() == Some("failed")
        && repair["error"].as_str().is_some_and(|s| {
            s.starts_with("Context repair requires a confirmed idle native thread; native status: ")
        })
        && repair["source"]["threadId"] == a["threadId"];
    let continued = rt.stage.as_deref() == Some("continued")
        && rt.outcome.as_deref() == Some("active")
        && c["source"] == "native_thread_read"
        && c["outcome"] == "active"
        && c["automatic"] == true
        && c["turnId"] == a["turnId"];
    // A turn adopted on reattach to the same provider generation ("continued",
    // outcome "active") waits for the provider's turn/completed. When that frame
    // reached a worker that ended before its commit, nothing delivers it again
    // (live cutover 4: three agents stayed `running` after their native turns
    // finished). Python reads the native thread on restart; this makes the
    // adopted turn a restart turn again, so the next worker start reads native
    // history: a finished turn is reconciled, a running one only records the
    // check. A queued restart follow-up (`eventId`) is not an adopted turn.
    let reattached_turn = continued
        && r["eventId"].is_null()
        && rt.auto_wake.unwrap_or(false)
        && rt.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && r["turnId"] == a["turnId"]
        && matches!(d_typed.source.as_ref(), Some(RecoverySource::Text(source)) if source == "restart")
        && typed.status == "running"
        && same(r, a, &["epoch", "accountKey", "threadId"])
        && clear(a, typed);
    let restart_turn = pending_restart_turn || reattached_turn;
    let same_repair = repair["agent"] == a["id"]
        && same(&repair["source"], &scope(a), &["id", "accountKey", "epoch"])
        && ((repair_typed.phase.as_deref() == Some("unchanged")
            && repair["source"]["threadId"] == a["threadId"])
            || (repair_typed.phase.as_deref() == Some("completed")
                && repair["newThreadId"] == a["threadId"])
            || surviving);
    let queued_active_wait = typed.status == "queued"
        && typed.auto_wake.unwrap_or(false)
        && inactive
        && typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
        && typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && turn_epoch
        && clear(a, typed)
        && same_repair
        && a["error"] == "Context repair waits for the current agent operation"
        && w["error"] == a["error"]
        && w["scope"] == "local"
        && w["source"] == wait_scope
        && !["action", "actionRequestId", "actionIdentity"]
            .iter()
            .any(|k| has(w, k) || has(t, k))
        && attempt_matches(a, t, tt)
        && tt.submitted == Some(false)
        && events_valid(tt)
        && w["events"] == t["events"]
        && (continued || surviving)
        && rt.auto_wake.unwrap_or(false)
        && same(r, a, &["epoch", "accountKey", "threadId", "turnId"]);
    let interrupted_transport = typed.status == "interrupted"
        && stopped
        && one(
            &a["error"],
            &[
                PROVIDER_DISCONNECTED_TURN_ERROR,
                VM_PROVIDER_DISCONNECTED_TURN_ERROR,
                LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
            ],
        );
    // A disconnected turn that generation reconciliation adopted again on the
    // same provider generation (connection recovery outcome "active") waits for
    // the provider's turn/completed. The turn can end while no worker is
    // attached, and then nothing reads that end (22 October 2026: agents stayed
    // `running`, and busy-turn input went to turns the provider had finished).
    // As for an adopted restart turn, the worker reads it in native history
    // once per connection: an ended turn is reconciled, a running one only
    // records the check.
    let reattached_transport = typed.status == "running"
        && typed.auto_wake.unwrap_or(false)
        && c["source"] == "native_thread_read"
        && c["outcome"] == "active"
        && c["automatic"] == true
        && c["turnId"] == a["turnId"];
    let transport_turn = (interrupted_transport || reattached_transport)
        && typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
        && typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && clear(a, typed)
        && !has(a, "contextRepairWait")
        && !repair_typed
            .phase
            .as_deref()
            .is_some_and(|s| ACTIVE_REPAIR.contains(&s))
        && !matches!(rt.stage.as_deref(), Some("pending" | "superseded"))
        && !matches!(d_typed.source.as_ref(), Some(RecoverySource::Text(source)) if source == "restart")
        && d_typed.auto_wake.unwrap_or(false)
        && same(d, a, &["epoch", "accountKey", "threadId", "turnId"])
        && turn_epoch
        && (!has(a, "startAttempt")
            || (t == &d["startAttempt"]
                && tt.id.as_deref().is_some_and(|s| !s.is_empty())
                && tt.submitted == Some(true)
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
        || (typed.status == "interrupted"
            && stopped
            && !has(a, "startAttempt")
            && !matches!(rt.stage.as_deref(), Some("pending" | "superseded"))
            && !["accountTransferId", "workspaceOperation", "deletedAt"]
                .iter()
                .any(|k| has(a, k))
            && typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
            && typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
            && one(
                &a["error"],
                &[
                    PROVIDER_DISCONNECTED_TURN_ERROR,
                    VM_PROVIDER_DISCONNECTED_TURN_ERROR,
                    LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR,
                    BACKEND_RESTARTED_TURN_ERROR,
                ],
            )
            && !thread_block(a, typed));
    let completed_owner = typed.status == "completed"
        && inactive
        && !typed.turn_id.as_deref().is_some_and(|s| !s.is_empty())
        && !has(a, "startAttempt");
    let task_owner = !has(a, "deletedAt")
        && crate::provider_adapter::native_history(a)
        && typed.thread_id.as_deref().is_some_and(|s| !s.is_empty())
        && !has(a, "accountTransferId")
        && !has(a, "workspaceOperation")
        && (!has(a, "contextRepairWait") || completed_owner)
        && !matches!(typed.status.as_str(), "queued" | "starting")
        && !repair_typed
            .phase
            .as_deref()
            .is_some_and(|s| ACTIVE_REPAIR.contains(&s));
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

#[cfg(test)]
mod unbounded_batch_tests {
    use super::*;

    #[test]
    fn recovery_batch_eligibility_keeps_identity_checks_above_the_former_cap() {
        for count in [1, 1000] {
            let ids: Vec<_> = (0..count).map(|index| format!("event-{index}")).collect();
            let mut agent = json!({"compactions":0,"provider":studio_runtime_schema::ProviderKind::Codex,"id":"agent","rootId":"agent","epoch":1,"accountKey":"default",
                "threadId":"thread","status":"interrupted","inFlight":false,"autoWake":false,
                "error":PROVIDER_DISCONNECTED_TURN_ERROR,"disconnectRecovery":{"autoWake":true,
                    "epoch":1,"accountKey":"default","threadId":"thread",
                    "startAttempt":{"id":"attempt","epoch":1,"accountKey":"default","threadId":"thread","submitted":false,"events":ids}}});
            let eligible = |record: Value| {
                RecoverySnapshot::from_record(record)
                    .expect("recovery record")
                    .eligibility()
                    .disconnected_preparation
            };
            assert!(eligible(agent.clone()));
            agent["disconnectRecovery"]["startAttempt"]["events"]
                .as_array_mut()
                .expect("ids")
                .push(json!("event-0"));
            assert!(
                !eligible(agent.clone()),
                "duplicate identity must remain blocked"
            );
            agent["disconnectRecovery"]["startAttempt"]["events"] = json!([]);
            assert!(!eligible(agent), "empty input must remain blocked");
        }
    }

    #[test]
    fn numeric_epoch_alias_does_not_change_exact_receipt_identity() {
        let mut agent = json!({"compactions":0,"id":"agent","rootId":"agent","epoch":1,"accountKey":"default",
            "threadId":"thread","status":"interrupted","inFlight":false,"autoWake":false,
            "error":PROVIDER_DISCONNECTED_TURN_ERROR,"disconnectRecovery":{"autoWake":true,
                "epoch":1,"accountKey":"default","threadId":"thread",
                "startAttempt":{"id":"attempt","epoch":1,"accountKey":"default","threadId":"thread","submitted":false,"events":["event"]}}});
        assert!(
            RecoverySnapshot::from_record(agent.clone())
                .expect("record")
                .eligibility()
                .disconnected_preparation
        );
        agent["disconnectRecovery"]["epoch"] = json!(1.0);
        assert!(
            !RecoverySnapshot::from_record(agent)
                .expect("numeric alias accepted")
                .eligibility()
                .disconnected_preparation
        );
    }
}

#[cfg(test)]
mod connection_recovery_identity_tests {
    use super::connection_recovery_id;
    use serde_json::json;

    #[test]
    fn a_new_disconnect_cannot_reuse_an_old_connection_recovery_receipt() {
        let first = json!({
            "id":"agent-1", "epoch":4, "accountKey":"acct", "threadId":"thread-1",
            "turnId":"turn-1", "disconnectRecovery":{"connectionId":"conn-1","at":10.0}
        });
        let second = json!({
            "id":"agent-1", "epoch":4, "accountKey":"acct", "threadId":"thread-1",
            "turnId":"turn-1", "disconnectRecovery":{"connectionId":"conn-2","at":11.0}
        });
        assert_ne!(
            connection_recovery_id(&first),
            connection_recovery_id(&second)
        );
    }
}

#[cfg(test)]
mod disconnect_text_tests {
    use super::{Eligibility, RecoverySnapshot};
    use serde_json::json;
    use studio_runtime_store::{
        PROVIDER_DISCONNECTED_TURN_ERROR, VM_PROVIDER_DISCONNECTED_TURN_ERROR,
    };

    fn interrupted(error: &str) -> serde_json::Value {
        json!({"compactions":0,"id":"agent-1","rootId":"agent-1","epoch":4,"accountKey":"acct","status":"interrupted",
               "inFlight":false,"autoWake":false,"threadId":"thread-1","turnId":"turn-1",
               "error":error})
    }

    fn eligibility(record: &serde_json::Value) -> Eligibility {
        RecoverySnapshot::from_record(record.clone())
            .expect("recovery record")
            .eligibility()
    }

    #[test]
    fn a_vm_provider_disconnect_has_the_manual_connection_check() {
        assert!(eligibility(&interrupted(PROVIDER_DISCONNECTED_TURN_ERROR)).connection);
        assert!(eligibility(&interrupted(VM_PROVIDER_DISCONNECTED_TURN_ERROR)).connection);
        assert!(
            eligibility(&interrupted(
                super::LEGACY_VM_PROVIDER_DISCONNECTED_TURN_ERROR
            ))
            .connection
        );
        assert!(!eligibility(&interrupted("Some other failure")).connection);
    }
}

#[cfg(test)]
mod reattached_turn_tests {
    use super::RecoverySnapshot;
    use serde_json::{Value, json};

    /// The live cutover 4 shape: a restart turn adopted on reattach to the same
    /// provider generation whose turn/completed never reached a committed worker.
    fn stuck(in_flight: bool) -> Value {
        let attempt = json!({"id":"attempt-1","accountKey":"acct","epoch":0,"threadId":"thread-1",
            "turnId":"turn-9","submitted":true,"connectionGeneration":5,"events":["event-1"],
            "activeAtReservation":false});
        json!({"compactions":0,
            "id":"agent-1","rootId":"agent-1","epoch":0,"turnEpoch":0,"accountKey":"acct","threadId":"thread-1",
            "provider":"codex","status":"running","inFlight":in_flight,"autoWake":true,
            "turnId":"turn-9","error":null,"lastCompletedTurn":"turn-8",
            "lastCompletedTurnStatus":"completed","startAttempt":attempt,
            "disconnectRecovery":{"accountKey":"acct","at":10.0,"autoWake":true,"connectionId":null,
                "epoch":0,"source":"restart","startAttempt":attempt,"threadId":"thread-1",
                "turnId":"turn-9"},
            "restartRecovery":{"accountKey":"acct","at":10.0,"autoWake":true,"connectionId":null,
                "epoch":0,"observedAt":20.0,"outcome":"active","stage":"continued",
                "startAttempt":attempt,"threadId":"thread-1","turnId":"turn-9"},
            "connectionRecovery":{"at":20.0,"automatic":true,"outcome":"active",
                "previousError":"Server restarted during a turn. Review history, then send a new instruction.",
                "source":"native_thread_read","turnId":"turn-9"}
        })
    }

    #[test]
    fn an_adopted_restart_turn_is_checked_against_native_history_again() {
        for in_flight in [false, true] {
            let eligibility = RecoverySnapshot::from_record(stuck(in_flight))
                .expect("snapshot")
                .eligibility();
            assert!(eligibility.restart_turn, "inFlight {in_flight}");
            assert!(eligibility.connection, "inFlight {in_flight}");
        }
    }

    #[test]
    fn a_queued_restart_follow_up_or_another_turn_is_not_an_adopted_turn() {
        let mut follow_up = stuck(false);
        follow_up["restartRecovery"]["eventId"] = json!("restart:agent-1:0:turn-9");
        let mut other_turn = stuck(false);
        other_turn["turnId"] = json!("turn-10");
        let mut held = stuck(false);
        held["nativeFailureHold"] = json!(true);
        for record in [follow_up, other_turn, held] {
            let eligibility = RecoverySnapshot::from_record(record.clone())
                .expect("snapshot")
                .eligibility();
            assert!(!eligibility.restart_turn, "{record}");
        }
    }
}
