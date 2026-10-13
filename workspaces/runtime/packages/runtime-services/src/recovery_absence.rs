//! Pure absence decisions over observations supplied by the owning worker.
use crate::{
    event_write::truthy, recovery_history::ConnectionScope, recovery_policy::RecoverySnapshot,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SupervisorIdentity {
    pub state_dir: String,
    pub handle: String,
    pub generation: u64,
}
/// Captured on the named connection after draining earlier notifications. The
/// adapter must read these flags again inside a transaction that acts on them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecoveryObservation {
    pub scope: ConnectionScope,
    pub at: f64,
    pub supervisor: Option<SupervisorIdentity>,
    pub quiet: bool,
    pub journal_drained: bool,
    pub account_reserved: bool,
    pub tools_refreshing: bool,
    pub preparation_pending: bool,
    pub completed_turn: Option<String>,
}
impl RecoveryObservation {
    pub fn valid(&self, scope: &ConnectionScope) -> bool {
        self.scope == *scope && self.at.is_finite() && self.at >= 0.0
    }
    pub fn replaced_child(&self, snapshot: &RecoverySnapshot) -> bool {
        let previous = &snapshot.record()["startAttempt"]["supervisorIdentity"];
        self.supervisor.as_ref().is_some_and(|current| {
            !current.state_dir.is_empty()
                && !current.handle.is_empty()
                && previous["stateDir"] == current.state_dir
                && previous["handle"] == current.handle
                && previous["generation"]
                    .as_u64()
                    .is_some_and(|g| current.generation > g)
                && current.generation == self.scope.generation
        })
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StartAbsenceDecision {
    Restore,
    Waiting,
    Hold,
}
pub fn start_absence(
    snapshot: &RecoverySnapshot,
    observation: &RecoveryObservation,
    complete_history: bool,
    idle_twice: bool,
) -> StartAbsenceDecision {
    let a = snapshot.record();
    if complete_history && idle_twice && observation.replaced_child(snapshot) {
        return StartAbsenceDecision::Restore;
    }
    let age = a["startAttempt"]["created"]
        .as_f64()
        .map(|at| observation.at - at)
        .unwrap_or(0.0);
    if complete_history
        && idle_twice
        && observation.quiet
        && observation.journal_drained
        && age >= crate::recovery_policy::UNKNOWN_START_HOLD_SECONDS
        && !truthy(a["startOutcomeHold"].get("stage"))
    {
        StartAbsenceDecision::Hold
    } else {
        StartAbsenceDecision::Waiting
    }
}

pub const LOST_COMMAND: &str =
    "Native command session is absent. Exit outcome unknown; command was not replayed.";
/// Store must additionally verify the exact completed-turn receipt in its transaction.
pub fn command_absence_source(
    agent: &RecoverySnapshot,
    task: &Value,
    observation: &RecoveryObservation,
    completed_turn: bool,
) -> bool {
    let a = agent.record();
    let process = &task["processId"];
    if task["type"] != "commandExecution"
        || !(process.as_str().is_some_and(|s| !s.is_empty())
            || process.is_i64()
            || process.is_u64())
        || truthy(a.get("inFlight"))
        || truthy(a.get("turnId"))
        || truthy(a.get("activeTools"))
        || matches!(
            a["status"].as_str(),
            Some("running" | "starting" | "approval" | "queued")
        )
        || observation.account_reserved
        || observation.tools_refreshing
        || observation.preparation_pending
        || !completed_turn
    {
        return false;
    }
    let Some(created) = task["created"].as_f64() else {
        return false;
    };
    match a.get("accountHistory").filter(|v| !v.is_null()) {
        None => true,
        Some(Value::Array(moves)) => moves.iter().all(|m| {
            m["at"].as_f64().is_some_and(|at| {
                at < created
                    || (m["accountKey"]
                        == a.get("accountKey")
                            .cloned()
                            .unwrap_or(Value::from("default"))
                        && m["threadId"] == a["threadId"])
            })
        }),
        _ => false,
    }
}
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum TaskAbsenceDecision {
    Lost,
    Hold { reason: String },
}
