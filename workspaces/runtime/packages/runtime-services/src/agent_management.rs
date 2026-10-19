//! Pure lifecycle decisions over checked Python-written records.
//!
//! The adapter supplies time and one transaction snapshot. Decisions never send
//! native requests: operations requiring a live owner are refused before writes.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::MAX_SAFE_INTEGER;
use studio_runtime_store::AgentStatus;
use thiserror::Error;

use crate::agent_lifecycle_runtime::{LifecycleEffect, LifecycleEffectKind};
use crate::event_write::{python_strip, truthy};

const MAX_SAFE_EPOCH: i64 = MAX_SAFE_INTEGER;
const BRIEF_FIELDS: &[&str] = &[
    "id",
    "name",
    "parentId",
    "status",
    "inFlight",
    "threadId",
    "turnId",
    "error",
    "lastEvent",
    "agentArchive",
    "nativeRelease",
    "parkedEvent",
    "imageWorkspace",
    "imageWorkspaceReady",
    "imageWorkspacePhase",
    "imageWorkspaceError",
    "imageWorkspaceCreatedAt",
    "cleanedImageWorkspace",
];

#[derive(Debug, Error)]
pub enum ManagementError {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("Invalid stored object {table}/{id}")]
    Stored { table: String, id: String },
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
}

/// Commands supplied by the authenticated HTTP edge or the future tool dispatcher.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Stop {
        target: String,
        descendants: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reason: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sender_epoch: Option<i64>,
    },
    RemoteStop {
        principal: String,
        link: String,
        target: String,
        request_id: String,
        control_epoch: i64,
        reason: String,
    },
    Delete {
        target: String,
    },
    Archive {
        actor: String,
        epoch: i64,
        target: String,
        reason: String,
        #[serde(default)]
        unassign_work: bool,
    },
    Restore {
        actor: String,
        epoch: i64,
        target: String,
    },
    Inspect {
        actor: String,
        epoch: i64,
        target: String,
    },
}

impl Action {
    /// The agent that issued the action, when it is not the target.
    pub fn acting_agent(&self) -> Option<&str> {
        match self {
            Self::Stop { sender, .. } => sender.as_deref(),
            Self::Archive { actor, .. }
            | Self::Restore { actor, .. }
            | Self::Inspect { actor, .. } => Some(actor),
            Self::RemoteStop { .. } | Self::Delete { .. } => None,
        }
    }
    /// The agent whose descendant tree scopes this action's history reads.
    pub fn target(&self) -> &str {
        match self {
            Self::Stop { target, .. }
            | Self::RemoteStop { target, .. }
            | Self::Delete { target }
            | Self::Archive { target, .. }
            | Self::Restore { target, .. }
            | Self::Inspect { target, .. } => target,
        }
    }
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Identity {
    id: String,
    root_id: String,
    epoch: i64,
    status: AgentStatus,
    parent_id: Option<String>,
    auto_wake: Option<bool>,
    is_lead: Option<bool>,
    in_flight: Option<bool>,
}

#[derive(Clone)]
struct Agent {
    identity: Identity,
    record: Value,
}
impl Agent {
    fn flag(&self, name: &str) -> bool {
        truthy(self.record.get(name))
    }
    fn id(&self) -> &str {
        &self.identity.id
    }
    fn active(&self) -> bool {
        self.identity.in_flight == Some(true)
            || matches!(
                self.identity.status,
                AgentStatus::Queued
                    | AgentStatus::Starting
                    | AgentStatus::Running
                    | AgentStatus::Approval
            )
    }
}

/// Raw rows from one store transaction, also the differential corpus boundary.
#[derive(Clone, Default, Deserialize, Serialize)]
pub struct Records {
    pub tables: BTreeMap<String, Vec<Value>>,
}

#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
struct WorkIdentity {
    owner: Option<String>,
    root_id: Option<String>,
    status: Option<String>,
    #[serde(default)]
    version: i64,
    #[serde(default)]
    releases: Vec<Value>,
}

struct Work {
    identity: WorkIdentity,
    record: Value,
}

pub struct ManagementSnapshot {
    agents: Vec<Agent>,
    work: Vec<Work>,
    tables: BTreeMap<String, Vec<Value>>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RowWrite {
    pub table: String,
    pub record: Value,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Decision {
    pub result: Value,
    pub writes: Vec<RowWrite>,
    pub cancel_pending: Vec<String>,
    #[serde(skip)]
    pub(crate) effects: Vec<LifecycleEffect>,
}
impl Decision {
    fn result(result: Value) -> Self {
        Self {
            result,
            writes: Vec::new(),
            cancel_pending: Vec::new(),
            effects: Vec::new(),
        }
    }
    fn write(&mut self, table: &str, record: Value) {
        self.writes.push(RowWrite {
            table: table.to_owned(),
            record,
        });
    }
}

impl ManagementSnapshot {
    /// The only constructor: validate each stored row before any decision.
    pub fn from_records(records: Records) -> Result<Self, ManagementError> {
        let mut agents = Vec::new();
        let mut work = Vec::new();
        for (table, rows) in &records.tables {
            let mut ids = BTreeSet::new();
            for row in rows {
                let id = row.get("id").and_then(Value::as_str).unwrap_or("<missing>");
                let invalid = || ManagementError::Stored {
                    table: table.clone(),
                    id: id.to_owned(),
                };
                if !row.is_object() || id == "<missing>" || !ids.insert(id) {
                    return Err(invalid());
                }
                if let Some(schema) = studio_runtime_schema::table_schema(table)
                    && !studio_runtime_schema::validate(schema, row).map_err(|_| invalid())?
                {
                    return Err(invalid());
                }
                if table == "runtime_work" {
                    work.push(Work {
                        identity: serde_json::from_value(row.clone()).map_err(|_| invalid())?,
                        record: row.clone(),
                    });
                }
                if table == "runtime_agents" {
                    agents.push(Agent {
                        identity: serde_json::from_value(row.clone()).map_err(|_| invalid())?,
                        record: row.clone(),
                    });
                }
            }
        }
        Ok(Self {
            agents,
            work,
            tables: records.tables,
        })
    }

    fn rows(&self, table: &str) -> &[Value] {
        self.tables
            .get(table)
            .map(Vec::as_slice)
            .unwrap_or_default()
    }
    fn agent(&self, id: &str) -> Result<&Agent, ManagementError> {
        self.agents
            .iter()
            .find(|a| a.id() == id)
            .ok_or(ManagementError::Invalid("Unknown agent"))
    }
    fn authorize(&self, actor: &str, epoch: i64, target: &str) -> Result<&Agent, ManagementError> {
        let actor = self.agent(actor)?;
        if actor.identity.is_lead != Some(true)
            || actor.flag("deletedAt")
            || actor.identity.auto_wake != Some(true)
            || actor.identity.epoch != epoch
        {
            return Err(ManagementError::Invalid(
                "Only the active orchestrator can manage its workers",
            ));
        }
        let target = self.agent(target)?;
        if target.identity.is_lead == Some(true)
            || target.id() == actor.id()
            || target.identity.root_id != actor.identity.root_id
        {
            return Err(ManagementError::Invalid(
                "You can manage only your own descendant workers",
            ));
        }
        let mut seen = BTreeSet::new();
        let mut current = target;
        while current.identity.parent_id.as_deref() != Some(actor.id()) {
            if !seen.insert(current.id()) {
                return Err(ManagementError::Invalid(
                    "This worker is not your descendant",
                ));
            }
            current = self.agent(current.identity.parent_id.as_deref().ok_or(
                ManagementError::Invalid("This worker is not your descendant"),
            )?)?;
        }
        Ok(target)
    }
    fn authorize_interrupt(
        &self,
        sender: &str,
        epoch: i64,
        target: &str,
    ) -> Result<(&Agent, &Agent), ManagementError> {
        let sender = self.agent(sender)?;
        if sender.flag("deletedAt")
            || sender.identity.auto_wake != Some(true)
            || sender.identity.epoch != epoch
        {
            return Err(ManagementError::Invalid("Sender was stopped"));
        }
        let target_agent = self.agent(target)?;
        if target_agent.identity.root_id != sender.identity.root_id
            || target_agent.id() == sender.id()
        {
            return Err(ManagementError::Invalid(
                "You can interrupt only your descendants",
            ));
        }
        let mut current = target_agent;
        let mut seen = BTreeSet::new();
        while current.identity.parent_id.as_deref() != Some(sender.id()) {
            if !seen.insert(current.id()) {
                return Err(ManagementError::Invalid(
                    "You can interrupt only your descendants",
                ));
            }
            current = self.agent(current.identity.parent_id.as_deref().ok_or(
                ManagementError::Invalid("You can interrupt only your descendants"),
            )?)?;
        }
        Ok((sender, target_agent))
    }
    fn descendants(&self, target: &str, descendants: bool) -> Result<Vec<&Agent>, ManagementError> {
        self.agent(target)?;
        let mut ids = BTreeSet::from([target]);
        if descendants {
            loop {
                let count = ids.len();
                for a in &self.agents {
                    if a.identity
                        .parent_id
                        .as_deref()
                        .is_some_and(|id| ids.contains(id))
                    {
                        ids.insert(a.id());
                    }
                }
                if count == ids.len() {
                    break;
                }
            }
        }
        Ok(self
            .agents
            .iter()
            .filter(|a| ids.contains(a.id()))
            .collect())
    }
    /// Descendant tree of `target`, the owner scope of a lifecycle decision.
    #[cfg(test)]
    pub(crate) fn owner_scope(&self, target: &str) -> Result<BTreeSet<String>, ManagementError> {
        Ok(self
            .descendants(target, true)?
            .into_iter()
            .map(|agent| agent.id().to_owned())
            .collect())
    }
    fn owned<'a>(&'a self, table: &str, field: &str, agent: &str) -> Vec<&'a Value> {
        self.rows(table)
            .iter()
            .filter(|row| row.get(field).and_then(Value::as_str) == Some(agent))
            .collect()
    }
    fn tool_dependency(&self, agent: &Agent) -> Result<(), ManagementError> {
        if !self
            .owned("runtime_tool_requests", "agent", agent.id())
            .is_empty()
        {
            return Err(ManagementError::Unavailable(
                "tool-driver/archive-reconciliation",
            ));
        }
        Ok(())
    }
    fn has_active_voice_session(&self, agent: &Agent) -> bool {
        self.rows("voice_sessions").iter().any(|session| {
            session.get("agent").and_then(Value::as_str) == Some(agent.id())
                && session.get("state").is_some_and(|state| !state.is_null())
                && session.get("ended").is_none_or(Value::is_null)
        })
    }
    fn refuse_live(&self, agent: &Agent) -> Result<(), ManagementError> {
        self.stop_effects(agent, &mut Vec::new(), false, false)
    }
    fn stop_effects(
        &self,
        agent: &Agent,
        effects: &mut Vec<LifecycleEffectKind>,
        live: bool,
        deleting: bool,
    ) -> Result<(), ManagementError> {
        let mut stage = |kind: LifecycleEffectKind| {
            if !live {
                return Err(ManagementError::Unavailable(kind.dependency()));
            }
            effects.push(kind);
            Ok(())
        };
        let capacity_retry_is_local = agent
            .record
            .pointer("/capacityRetry/id")
            .and_then(Value::as_str)
            .zip(
                agent
                    .record
                    .pointer("/startAttempt/capacityRetryId")
                    .and_then(Value::as_str),
            )
            .is_some_and(|(retry_id, attempt_retry_id)| {
                retry_id == attempt_retry_id
                    && agent
                        .record
                        .pointer("/startAttempt/submitted")
                        .and_then(Value::as_bool)
                        != Some(true)
            });
        if !capacity_retry_is_local
            && (agent.active()
                || [
                    "threadId",
                    "turnId",
                    "startAttempt",
                    "workspaceOperation",
                    "accountTransferId",
                ]
                .iter()
                .any(|key| agent.flag(key)))
        {
            stage(LifecycleEffectKind::Interrupt)?;
        }
        if (agent.flag("capacityRetry") && !capacity_retry_is_local)
            || agent
                .record
                .get("usageResume")
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| matches!(s, "scheduled" | "started"))
        {
            stage(LifecycleEffectKind::CancelResume)?;
        }
        if self
            .owned("runtime_monitors", "agent", agent.id())
            .iter()
            .any(|r| is_status(r, &["starting", "running", "approval"]))
        {
            stage(LifecycleEffectKind::CancelMonitors)?;
        }
        if !deleting && self.has_active_voice_session(agent) {
            stage(LifecycleEffectKind::VoiceStop)?;
        }
        Ok(())
    }
    fn blockers(&self, agent: &Agent, unassign: bool) -> Vec<Value> {
        let mut blocks = Vec::new();
        let mut add = |kind: &str, ids: Vec<Value>| {
            if !ids.is_empty() {
                blocks.push(json!({"kind":kind,"count":ids.len(),"ids":ids.into_iter().take(20).collect::<Vec<_>>()}));
            }
        };
        if agent.active() {
            add("active_turn", vec![json!(agent.id())]);
        }
        if agent.flag("parkedEvent") {
            add("parked_event", vec![agent.record["parkedEvent"].clone()]);
        }
        if agent.flag("workspaceOperation") {
            add("workspace_operation", vec![json!(agent.id())]);
        }
        if agent.record.get("executionMode").and_then(Value::as_str) == Some("vm") {
            add(
                "turn_state",
                agent
                    .record
                    .get("layrTurnSaves")
                    .and_then(Value::as_object)
                    .map(|saves| {
                        saves
                            .iter()
                            .filter(|(_, saved)| {
                                saved.get("status").and_then(Value::as_str) != Some("saved")
                            })
                            .map(|(turn, _)| json!(turn))
                            .collect()
                    })
                    .unwrap_or_default(),
            );
        }
        add(
            "descendants",
            self.agents
                .iter()
                .filter(|a| {
                    a.identity.parent_id.as_deref() == Some(agent.id()) && !a.flag("deletedAt")
                })
                .map(|a| json!(a.id()))
                .collect(),
        );
        add(
            "input_delivery",
            self.owned("runtime_events", "agent", agent.id())
                .into_iter()
                .filter(|r| {
                    r.get("epoch").and_then(Value::as_i64) == Some(agent.identity.epoch)
                        && is_status(r, &["pending", "reserved", "dispatching", "uncertain"])
                })
                .map(|r| r["id"].clone())
                .collect(),
        );
        add(
            "monitors",
            self.owned("runtime_monitors", "agent", agent.id())
                .into_iter()
                .filter(|r| is_status(r, &["starting", "running", "approval"]))
                .map(|r| r["id"].clone())
                .collect(),
        );
        add(
            "background_tasks",
            self.owned("runtime_tasks", "agent", agent.id())
                .into_iter()
                .filter(|r| is_status(r, &["running", "starting", "pending", "unknown"]))
                .map(|r| r["id"].clone())
                .collect(),
        );
        add(
            "questions",
            self.owned("runtime_requests", "agent", agent.id())
                .into_iter()
                .filter(|r| is_status(r, &["pending"]))
                .map(|r| r["id"].clone())
                .collect(),
        );
        if !unassign {
            add(
                "assigned_work",
                self.open_work(agent)
                    .iter()
                    .map(|r| r.record["id"].clone())
                    .collect(),
            );
        }
        // Python `_blockers`: an open request, or an unknown outcome while the
        // agent may still act on it. A finished agent's unknown outcomes are
        // recorded in the archive instead (`unknownToolRequests`).
        let finished = finished(agent);
        add(
            "tool_requests",
            self.owned("runtime_tool_requests", "agent", agent.id())
                .into_iter()
                .filter(|r| {
                    matches!(
                        r.get("stage").and_then(Value::as_str),
                        Some("queued" | "running")
                    ) || (!finished && r.get("outcome").and_then(Value::as_str) == Some("unknown"))
                })
                .map(|r| r["id"].clone())
                .collect(),
        );
        blocks
    }
    /// Tool requests whose outcome stays unknown, kept with the archive record.
    fn unknown_tool_requests(&self, agent: &Agent) -> Vec<Value> {
        let mut ids = self
            .owned("runtime_tool_requests", "agent", agent.id())
            .into_iter()
            .filter(|r| r.get("outcome").and_then(Value::as_str) == Some("unknown"))
            .filter_map(|r| r.get("id").and_then(Value::as_str))
            .map(str::to_owned)
            .collect::<Vec<_>>();
        ids.sort();
        ids.into_iter().map(Value::String).collect()
    }
    fn open_work(&self, agent: &Agent) -> Vec<&Work> {
        self.work
            .iter()
            .filter(|work| {
                work.identity.owner.as_deref() == Some(agent.id())
                    && !work
                        .identity
                        .status
                        .as_deref()
                        .is_some_and(|status| matches!(status, "accepted" | "cancelled"))
            })
            .collect()
    }
}

/// Python `_finished`.
fn finished(agent: &Agent) -> bool {
    matches!(
        agent.record.get("status").and_then(Value::as_str),
        Some("completed" | "failed" | "interrupted")
    ) || (agent.record.get("status").and_then(Value::as_str) == Some("paused")
        && !agent.flag("autoWake"))
}

/// Python `_completed_native_turn`: a durable completion of the last submitted
/// turn proves that no native turn is left to wait for.
fn completed_native_turn(agent: &Agent) -> bool {
    let record = &agent.record;
    let attempt_turn = record.pointer("/startAttempt/turnId");
    let turn = record
        .get("lastCompletedTurn")
        .filter(|turn| truthy(Some(turn)));
    let completed_without_attempt = !agent.flag("startAttempt")
        && matches!(
            record
                .get("lastCompletedTurnStatus")
                .and_then(Value::as_str),
            Some("completed" | "failed" | "interrupted")
        );
    turn.is_some_and(|turn| attempt_turn == Some(turn) || completed_without_attempt)
        && record.get("turnId").is_none_or(Value::is_null)
        && !agent.flag("inFlight")
        && finished(agent)
}

/// Python `manage_agent` archive replay: the cleanup receipts already saved.
fn archived_cleanup_result(agent: &Agent) -> (Value, Value) {
    let record = &agent.record;
    let cleaned = (!agent.flag("worktreeReady"))
        .then(|| record.get("cleanedWorktree"))
        .flatten()
        .filter(|cleaned| truthy(Some(cleaned)));
    let worktree = match cleaned {
        Some(cleaned) if truthy(cleaned.get("missing")) => {
            json!({"state":"missing","reason":cleaned.get("note").cloned().unwrap_or(Value::Null),"bytes":0})
        }
        Some(cleaned) => {
            json!({"state":"removed","bytes":cleaned.get("bytes").cloned().unwrap_or(Value::Null)})
        }
        None => json!({"state":"kept","reason":"already archived","bytes":0}),
    };
    let cleaned_image = (!agent.flag("imageWorkspaceReady"))
        .then(|| record.get("cleanedImageWorkspace"))
        .flatten()
        .filter(|cleaned| truthy(Some(cleaned)));
    let workspace = record
        .get("imageWorkspaceCleanupResult")
        .filter(|result| truthy(Some(result)))
        .cloned()
        .or_else(|| {
            cleaned_image.map(|cleaned| {
                json!({"state":"archived","bytes":cleaned.get("bytes").cloned().unwrap_or(json!(0))})
            })
        })
        .unwrap_or(Value::Null);
    (worktree, workspace)
}

fn is_status(row: &Value, statuses: &[&str]) -> bool {
    row.get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| statuses.contains(&s))
}

fn has_releasable_deleted_work(snapshot: &ManagementSnapshot, agents: &[&Agent]) -> bool {
    agents.iter().any(|agent| {
        agent.identity.parent_id.is_some()
            && snapshot.work.iter().any(|work| {
                work.identity.owner.as_deref() == Some(agent.id())
                    && work
                        .identity
                        .status
                        .as_deref()
                        .is_some_and(|status| ["ready", "running", "blocked"].contains(&status))
            })
    })
}
fn brief(record: &Value) -> Value {
    Value::Object(
        BRIEF_FIELDS
            .iter()
            .map(|key| {
                (
                    (*key).to_owned(),
                    record.get(key).cloned().unwrap_or(Value::Null),
                )
            })
            .collect(),
    )
}
fn next_epoch(agent: &Agent) -> Result<i64, ManagementError> {
    agent
        .identity
        .epoch
        .checked_add(1)
        .filter(|epoch| *epoch <= MAX_SAFE_EPOCH)
        .ok_or(ManagementError::Invalid("Agent epoch limit reached"))
}
fn workspace_dependency(agent: &Agent) -> Result<(), ManagementError> {
    let image_removed = agent
        .record
        .get("imageWorkspacePhase")
        .and_then(Value::as_str)
        == Some("removed");
    if agent.record.get("executionMode").and_then(Value::as_str) == Some("vm")
        || ["worktreeReady", "worktreeCleanup", "cleanedWorktree"]
            .iter()
            .any(|key| agent.flag(key))
        || (!image_removed
            && ["imageWorkspaceReady", "cleanedImageWorkspace"]
                .iter()
                .any(|key| agent.flag(key)))
    {
        return Err(ManagementError::Unavailable(
            "workspace-state/agent-lifecycle",
        ));
    }
    Ok(())
}

/// Caller-provided time is audit provenance, never part of authorization.
pub fn decide(
    snapshot: &ManagementSnapshot,
    action: &Action,
    now: f64,
) -> Result<Decision, ManagementError> {
    let decision = decide_inner(snapshot, action, now, false)?;
    if let Some(effect) = decision.effects.first() {
        return Err(ManagementError::Unavailable(effect.kind().dependency()));
    }
    Ok(decision)
}

pub(crate) fn decide_with_effects(
    snapshot: &ManagementSnapshot,
    action: &Action,
    now: f64,
) -> Result<Decision, ManagementError> {
    decide_inner(snapshot, action, now, true)
}

fn decide_inner(
    snapshot: &ManagementSnapshot,
    action: &Action,
    now: f64,
    live: bool,
) -> Result<Decision, ManagementError> {
    if !now.is_finite() || now <= 0.0 {
        return Err(ManagementError::Invalid("Invalid lifecycle timestamp"));
    }
    match action {
        Action::Stop {
            target,
            descendants,
            reason,
            sender,
            sender_epoch,
        } => {
            let reason = if let Some(sender) = sender {
                let epoch = sender_epoch.ok_or(ManagementError::Invalid("senderEpoch"))?;
                let (sender, _) = snapshot.authorize_interrupt(sender, epoch, target)?;
                Some(
                    format!(
                        "Stopped by agent {}",
                        sender
                            .record
                            .get("name")
                            .and_then(Value::as_str)
                            .unwrap_or(sender.id())
                    )
                    .chars()
                    .take(160)
                    .collect(),
                )
            } else {
                reason.clone()
            };
            stop(
                snapshot,
                target,
                StopOptions {
                    descendants: *descendants,
                    delete: false,
                    reason: reason.as_deref(),
                    now,
                    live,
                    remote_request: None,
                },
            )
        }
        Action::RemoteStop {
            principal,
            link,
            target,
            request_id,
            control_epoch,
            reason,
        } => {
            let agent = snapshot.agent(target)?;
            let expected = json!({"server":principal,"link":link});
            if agent.record.get("remoteWorker") != Some(&expected) || !agent.flag("movedTo") {
                return Err(ManagementError::Invalid("The remote worker route changed"));
            }
            if agent
                .record
                .get("remoteStopRequest")
                .and_then(Value::as_str)
                == Some(request_id)
            {
                return Ok(Decision::result(json!({"stopped":[target]})));
            }
            if agent
                .record
                .get("remoteControlEpoch")
                .and_then(Value::as_i64)
                .is_some_and(|current| current > *control_epoch)
            {
                return Err(ManagementError::Invalid("The remote stop was superseded"));
            }
            stop(
                snapshot,
                target,
                StopOptions {
                    descendants: true,
                    delete: false,
                    reason: Some(reason),
                    now,
                    live,
                    remote_request: Some((request_id, *control_epoch)),
                },
            )
        }
        Action::Delete { target } => stop(
            snapshot,
            target,
            StopOptions {
                descendants: true,
                delete: true,
                reason: None,
                now,
                live,
                remote_request: None,
            },
        ),
        Action::Archive {
            actor,
            epoch,
            target,
            reason,
            unassign_work,
        } => {
            let agent = snapshot.authorize(actor, *epoch, target)?;
            archive(snapshot, agent, actor, reason, *unassign_work, now)
        }
        Action::Restore {
            actor,
            epoch,
            target,
        } => {
            let agent = snapshot.authorize(actor, *epoch, target)?;
            restore(snapshot, agent)
        }
        Action::Inspect {
            actor,
            epoch,
            target,
        } => {
            let agent = snapshot.authorize(actor, *epoch, target)?;
            snapshot.tool_dependency(agent)?;
            // Python inspection also consults preparation futures and scheduler liveness.
            Err(ManagementError::Unavailable(
                "account-worker/agent-inspection",
            ))
        }
    }
}

struct StopOptions<'a> {
    descendants: bool,
    delete: bool,
    now: f64,
    live: bool,
    reason: Option<&'a str>,
    remote_request: Option<(&'a str, i64)>,
}

fn stop(
    snapshot: &ManagementSnapshot,
    target: &str,
    options: StopOptions<'_>,
) -> Result<Decision, ManagementError> {
    let StopOptions {
        descendants,
        delete,
        reason,
        now,
        live,
        remote_request,
    } = options;
    let agents = snapshot.descendants(target, descendants)?;
    let mut required = BTreeMap::new();
    for agent in &agents {
        let mut effects = Vec::new();
        snapshot.stop_effects(agent, &mut effects, live, delete)?;
        if !delete
            && agent.identity.is_lead != Some(true)
            && agent.identity.parent_id.is_some()
            && (!descendants || agent.id() == target)
            && !agent.flag("deletedAt")
            && !agent.flag("agentArchive")
        {
            if !live {
                return Err(ManagementError::Unavailable(
                    LifecycleEffectKind::NotifyParent.dependency(),
                ));
            }
            effects.push(LifecycleEffectKind::NotifyParent);
        }
        if delete
            && (agent.identity.in_flight == Some(true)
                || agent.flag("turnId")
                || truthy(agent.record["startAttempt"].get("activeAtReservation")))
        {
            effects.push(LifecycleEffectKind::FinalizeDeleted);
        }
        required.insert(agent.id(), effects);
    }
    // Python selects releasable rows by failed/deleted child owner, then only
    // work statuses ready/running/blocked (codex_runtime.py:2816-2823).
    if delete && has_releasable_deleted_work(snapshot, &agents) {
        required
            .entry(target)
            .or_default()
            .push(LifecycleEffectKind::ReleaseWork);
    }
    let mut ids = agents.iter().map(|a| a.id().to_owned()).collect::<Vec<_>>();
    ids.sort();
    let result = if delete {
        json!({"deleted":ids})
    } else {
        json!({"stopped":ids})
    };
    let reason = if delete {
        "Conversation deleted".to_owned()
    } else if let Some(reason) = reason {
        let reason = python_strip(reason);
        if reason.is_empty() || reason.chars().count() > 160 {
            return Err(ManagementError::Invalid("Invalid stop reason"));
        }
        reason.to_owned()
    } else {
        "Stopped by user".to_owned()
    };
    let mut decision = Decision::result(result);
    for agent in agents {
        let mut record = agent.record.clone();
        let epoch = next_epoch(agent)?;
        let object = record
            .as_object_mut()
            .ok_or(ManagementError::Invalid("Invalid agent"))?;
        let retry_id = object
            .get("capacityRetry")
            .and_then(|retry| retry.get("id"))
            .and_then(Value::as_str);
        let cancel_unsubmitted_retry = retry_id.is_some_and(|retry_id| {
            object.get("startAttempt").is_some_and(|attempt| {
                attempt.get("capacityRetryId").and_then(Value::as_str) == Some(retry_id)
                    && attempt.get("submitted") != Some(&json!(true))
            })
        });
        if cancel_unsubmitted_retry {
            object.remove("startAttempt");
            object.insert("inFlight".to_owned(), json!(false));
        }
        let receipt = object.remove("parkReceipt");
        object.remove("parkedEvent");
        object.remove("parkAfterTurn");
        object.remove("capacityRetryCount");
        object.remove("capacityRetry");
        if let Some(Value::Object(mut receipt)) = receipt {
            receipt.insert("cancelledAtEpoch".to_owned(), json!(epoch));
            object.insert("cancelledPark".to_owned(), Value::Object(receipt));
        }
        object.insert("autoWake".to_owned(), json!(false));
        object.insert("epoch".to_owned(), json!(epoch));
        object.insert("status".to_owned(), json!("paused"));
        object.insert("error".to_owned(), json!(reason.clone()));
        if let Some((request_id, control_epoch)) = remote_request
            && agent.id() == target
        {
            object.insert("remoteStopRequest".to_owned(), json!(request_id));
            object.insert("remoteControlEpoch".to_owned(), json!(control_epoch));
        }
        if agent.flag("autoWake")
            && !agent.flag("movedTo")
            && agent
                .record
                .get("remoteWorker")
                .is_some_and(Value::is_object)
        {
            let request_id = remote_stop_request_id(agent.id(), epoch);
            object.insert("remoteStopRequest".to_owned(), json!(request_id));
            object.insert("remoteControlEpoch".to_owned(), json!(epoch));
            required
                .entry(agent.id())
                .or_default()
                .push(LifecycleEffectKind::RemoteStop);
        }
        if delete && !agent.flag("deletedAt") {
            object.insert("deletedAt".to_owned(), json!(now));
        }
        if delete && agent.record.get("workspaceCleanupIntent").is_none() {
            record["workspaceCleanupIntent"] =
                crate::workspace_cleanup_runtime::queued_cleanup_intent(
                    agent.id(),
                    epoch,
                    crate::workspace_cleanup_runtime::CleanupAction::Delete,
                );
        }
        for kind in required.remove(agent.id()).unwrap_or_default() {
            decision.effects.push(LifecycleEffect::new(
                kind,
                agent.record.clone(),
                record.clone(),
                now,
            ));
        }
        decision.write("runtime_agents", record);
        decision.cancel_pending.push(agent.id().to_owned());
    }
    for request in snapshot.rows("runtime_requests") {
        if request
            .get("agent")
            .and_then(Value::as_str)
            .is_some_and(|id| ids.iter().any(|a| a == id))
            && is_status(request, &["pending"])
            && request.get("method").and_then(Value::as_str) != Some("monitor/approve")
        {
            let mut request = request.clone();
            request["status"] = json!("expired");
            decision.write("runtime_requests", request);
        }
    }
    Ok(decision)
}

fn remote_stop_request_id(agent_id: &str, control_epoch: i64) -> String {
    // Match Python identity(agent_id, "stop", str(control_epoch)) in
    // codex_multi_server_orchestration.py:60 and :782-784.
    uuid::Uuid::new_v5(
        &uuid::Uuid::NAMESPACE_URL,
        format!("studio-cross-server:{agent_id}:stop:{control_epoch}").as_bytes(),
    )
    .to_string()
}

fn archive(
    snapshot: &ManagementSnapshot,
    agent: &Agent,
    actor: &str,
    reason: &str,
    unassign: bool,
    now: f64,
) -> Result<Decision, ManagementError> {
    if agent.flag("deletedAt") && !agent.flag("agentArchive") {
        return Err(ManagementError::Invalid(
            "This worker was deleted, not archived",
        ));
    }
    if agent.flag("agentArchive") {
        if truthy(agent.record["agentArchive"].get("cleanupPending")) {
            return Ok(Decision::result(json!({
                "status":"archived",
                "agent":brief(&agent.record),
                "replayed":true,
                "cleanupPending":true,
                "workspace":null,
                "worktree":{"state":"pending","bytes":0},
                "unassignedWork":agent.record["agentArchive"].get("unassignedWork").cloned().unwrap_or(json!([]))
            })));
        }
        let (worktree, workspace) = archived_cleanup_result(agent);
        return Ok(Decision::result(
            json!({"status":"archived","agent":brief(&agent.record),"replayed":true,"workspace":workspace,"worktree":worktree,"unassignedWork":agent.record["agentArchive"].get("unassignedWork").cloned().unwrap_or(json!([]))}),
        ));
    }
    // Python `_blockers`. A running, starting, pending or unknown background
    // task is the `background_tasks` blocker; it is retried, never refused.
    let blockers = snapshot.blockers(agent, unassign);
    if !blockers.is_empty() {
        return Ok(Decision::result(
            json!({"status":"blocked","agent":brief(&agent.record),"blockers":blockers}),
        ));
    }
    // Python reads the native thread state first and, when the account is
    // offline, accepts a durable completion of the last submitted turn. This
    // decision has no provider session, so it uses that durable proof only.
    if agent.flag("threadId") && !completed_native_turn(agent) {
        return Ok(Decision::result(json!({
            "status":"blocked",
            "agent":brief(&agent.record),
            "blockers":[{"kind":"native_state_unconfirmed","count":1,"ids":[agent.id()]}]
        })));
    }
    // The workspace cleanup service removes the worktree, archives the image
    // or releases the layr line from the durable intent written below.
    let reason = python_strip(reason);
    if reason.is_empty() || reason.chars().count() > 1000 {
        return Err(ManagementError::Invalid(
            "Give an archive reason with 1 to 1000 characters",
        ));
    }
    let mut decision = Decision::result(Value::Null);
    let mut unassigned = Vec::new();
    if unassign {
        for work in snapshot.open_work(agent) {
            if work.identity.root_id.as_deref() != Some(&agent.identity.root_id) {
                return Err(ManagementError::Invalid(
                    "An assigned task belongs to another team",
                ));
            }
            let version = work
                .identity
                .version
                .checked_add(1)
                .filter(|v| *v <= MAX_SAFE_EPOCH)
                .ok_or(ManagementError::Invalid("Work version limit reached"))?;
            let releases = work.identity.releases.clone();
            let mut work = work.record.clone();
            if work.get("releases").is_none() {
                work["releases"] = json!(releases);
            }
            let id = work_id(work.get("id"));
            work["releases"]
                .as_array_mut()
                .ok_or_else(|| ManagementError::Stored {
                    table: "runtime_work".to_owned(),
                    id,
                })?
                .push(json!({"agent":agent.id(),"reason":reason,"created":now,"by":actor}));
            work["owner"] = Value::Null;
            work["status"] = json!("ready");
            work["version"] = json!(version);
            work["updated"] = json!(now);
            unassigned.push(work["id"].clone());
            decision.write("runtime_work", work);
        }
    }
    let mut record = agent.record.clone();
    record["deletedAt"] = json!(now);
    record["autoWake"] = json!(false);
    record["status"] = json!("paused");
    record["epoch"] = json!(next_epoch(agent)?);
    record["agentArchive"] = json!({"at":now,"by":actor,"reason":reason,"epoch":record["epoch"],"cleanupPending":true,"unknownToolRequests":snapshot.unknown_tool_requests(agent),"unassignedWork":unassigned});
    record["workspaceCleanupIntent"] = crate::workspace_cleanup_runtime::queued_cleanup_intent(
        agent.id(),
        record["epoch"]
            .as_i64()
            .ok_or(ManagementError::Invalid("Invalid archive epoch"))?,
        crate::workspace_cleanup_runtime::CleanupAction::Archive,
    );
    decision.result = json!({"status":"archived","agent":brief(&record),"worktree":{"state":"none","bytes":0},"unassignedWork":unassigned});
    decision.write("runtime_agents", record);
    Ok(decision)
}
fn work_id(id: Option<&Value>) -> String {
    id.and_then(Value::as_str).unwrap_or("<missing>").to_owned()
}
fn restore(snapshot: &ManagementSnapshot, agent: &Agent) -> Result<Decision, ManagementError> {
    if agent.flag("deletedAt") && !agent.flag("agentArchive") {
        return Err(ManagementError::Invalid(
            "This worker was deleted, not archived",
        ));
    }
    if !agent.flag("agentArchive") {
        return Ok(Decision::result(
            json!({"status":"not_archived","agent":brief(&agent.record)}),
        ));
    }
    if agent.record.get("executionMode").and_then(Value::as_str) == Some("vm") {
        return Ok(Decision::result(
            json!({"status":"blocked","reason":"The archived layr line was removed. Create a new worker from the saved state."}),
        ));
    }
    let archive = &agent.record["agentArchive"];
    if agent.record.get("deletedAt") != archive.get("at")
        || archive.get("epoch").and_then(Value::as_i64) != Some(agent.identity.epoch)
    {
        return Err(ManagementError::Invalid(
            "The archived worker changed; restoration requires inspection",
        ));
    }
    if truthy(archive.get("cleanupPending")) {
        return Ok(Decision::result(
            json!({"status":"blocked","reason":"Archive cleanup is still in progress"}),
        ));
    }
    let parent = snapshot.agent(agent.identity.parent_id.as_deref().ok_or(
        ManagementError::Invalid("This worker is not your descendant"),
    )?)?;
    if parent.flag("deletedAt") {
        return Err(ManagementError::Invalid("Restore the parent first"));
    }
    workspace_dependency(agent)?;
    let mut record = agent.record.clone();
    let object = record
        .as_object_mut()
        .ok_or(ManagementError::Invalid("Invalid agent"))?;
    object.remove("deletedAt");
    object.remove("agentArchive");
    object.insert("status".to_owned(), json!("paused"));
    object.insert("autoWake".to_owned(), json!(false));
    object.insert("epoch".to_owned(), json!(next_epoch(agent)?));
    if agent.flag("imageWorkspace") && !agent.flag("imageWorkspaceReady") {
        object.insert("imageWorkspacePhase".to_owned(), json!("read_only"));
    }
    let mut decision = Decision::result(
        json!({"status":"restored","agent":brief(&record),"next":"Use orchestration_send to resume with an instruction"}),
    );
    decision.write("runtime_agents", record);
    Ok(decision)
}

#[cfg(test)]
mod lifecycle_scope_parity_tests {
    use super::{Action, ManagementSnapshot, Records, decide_with_effects};
    use serde_json::{Value, json};
    use std::collections::{BTreeMap, BTreeSet};

    fn agent(id: &str, root: &str, parent: Option<&str>, lead: bool) -> Value {
        let mut record = json!({
            "id":id,
            "rootId":root,
            "epoch":1,
            "status":"paused",
            "autoWake":lead,
            "isLead":lead,
        });
        if let Some(parent) = parent {
            record["parentId"] = json!(parent);
        }
        record
    }

    fn scoped_records(records: &Records, owners: &BTreeSet<String>) -> Records {
        let mut tables = BTreeMap::new();
        for (table, rows) in &records.tables {
            if table == "runtime_agents" {
                tables.insert(table.clone(), rows.clone());
                continue;
            }
            let field = if table == "runtime_work" {
                "owner"
            } else {
                "agent"
            };
            let matching = rows
                .iter()
                .filter(|row| {
                    row.get(field)
                        .and_then(Value::as_str)
                        .is_some_and(|owner| owners.contains(owner))
                })
                .cloned()
                .collect();
            tables.insert(table.clone(), matching);
        }
        Records { tables }
    }

    fn effect_values(decision: &super::Decision) -> Vec<Value> {
        decision
            .effects
            .iter()
            .map(|effect| {
                json!({
                    "kind":effect.kind(),
                    "before":effect.before(),
                    "stopped":effect.stopped(),
                    "at":effect.at(),
                })
            })
            .collect()
    }

    #[test]
    fn scoped_lifecycle_decisions_match_full_snapshot_with_unrelated_history() {
        let full = Records {
            tables: BTreeMap::from([
                (
                    "runtime_agents".to_owned(),
                    vec![
                        agent("lead", "lead", None, true),
                        agent("worker", "lead", Some("lead"), false),
                        agent("nested", "lead", Some("worker"), false),
                        agent("other-lead", "other-lead", None, true),
                    ],
                ),
                (
                    "runtime_monitors".to_owned(),
                    vec![
                        json!({"id":"worker-monitor","agent":"worker","status":"running"}),
                        json!({"id":"unrelated-monitor","agent":"other-lead","status":"running"}),
                    ],
                ),
                (
                    "runtime_requests".to_owned(),
                    vec![
                        json!({"id":"worker-request","agent":"worker","status":"pending","method":"user/input"}),
                        json!({"id":"approval-request","agent":"nested","status":"pending","method":"monitor/approve"}),
                        json!({"id":"unrelated-request","agent":"other-lead","status":"pending","method":"user/input"}),
                    ],
                ),
                (
                    "runtime_tasks".to_owned(),
                    vec![
                        json!({"id":"worker-task","agent":"worker","status":"pending"}),
                        json!({"id":"unrelated-task","agent":"other-lead","status":"running"}),
                    ],
                ),
                (
                    "runtime_tool_requests".to_owned(),
                    vec![
                        json!({"id":"unrelated-tool","agent":"other-lead","stage":"completed","outcome":"applied"}),
                    ],
                ),
                (
                    "runtime_work".to_owned(),
                    vec![
                        json!({"id":"worker-work","owner":"worker","rootId":"lead","status":"ready","version":1}),
                        json!({"id":"unrelated-work","owner":"other-lead","rootId":"other-lead","status":"running","version":1}),
                    ],
                ),
                (
                    "runtime_events".to_owned(),
                    vec![
                        json!({"id":"worker-event","agent":"worker","status":"pending","epoch":1}),
                        json!({"id":"unrelated-event","agent":"other-lead","status":"pending","epoch":1}),
                    ],
                ),
                (
                    "voice_sessions".to_owned(),
                    vec![
                        json!({"id":"worker-voice","agent":"worker","state":"running"}),
                        json!({"id":"unrelated-voice","agent":"other-lead","state":"running"}),
                    ],
                ),
            ]),
        };
        let full_snapshot = ManagementSnapshot::from_records(full.clone()).expect("full snapshot");
        let owners = full_snapshot
            .owner_scope("lead")
            .expect("complete descendant set");
        assert_eq!(
            owners,
            BTreeSet::from(["lead".to_owned(), "nested".to_owned(), "worker".to_owned()])
        );

        let scoped = scoped_records(&full, &owners);
        let scoped_snapshot = ManagementSnapshot::from_records(scoped).expect("scoped snapshot");
        let action = Action::Delete {
            target: "lead".to_owned(),
        };
        assert_same_decision(&full_snapshot, &scoped_snapshot, &action);

        // Non-delete actions scope history to their target's descendant tree.
        let worker_owners = full_snapshot
            .owner_scope("worker")
            .expect("worker descendant set");
        let worker_scoped = ManagementSnapshot::from_records(scoped_records(&full, &worker_owners))
            .expect("worker scoped snapshot");
        for action in [
            Action::Stop {
                target: "worker".to_owned(),
                descendants: true,
                reason: None,
                sender: None,
                sender_epoch: None,
            },
            Action::Archive {
                actor: "lead".to_owned(),
                epoch: 1,
                target: "worker".to_owned(),
                reason: "done".to_owned(),
                unassign_work: true,
            },
        ] {
            assert_same_decision(&full_snapshot, &worker_scoped, &action);
        }
    }

    #[test]
    fn scoped_restore_and_inspect_match_full_snapshot_for_a_worker_with_descendants() {
        let mut archived = agent("archived", "lead", Some("lead"), false);
        archived["deletedAt"] = json!(5.0);
        archived["agentArchive"] = json!({"at":5.0,"epoch":1});
        let mut archived_child = agent("archived-child", "lead", Some("archived"), false);
        archived_child["deletedAt"] = json!(5.0);
        archived_child["agentArchive"] = json!({"at":5.0,"epoch":1});
        let full = Records {
            tables: BTreeMap::from([
                (
                    "runtime_agents".to_owned(),
                    vec![
                        agent("lead", "lead", None, true),
                        agent("worker", "lead", Some("lead"), false),
                        agent("nested", "lead", Some("worker"), false),
                        archived,
                        archived_child,
                        agent("other-lead", "other-lead", None, true),
                    ],
                ),
                (
                    "runtime_tool_requests".to_owned(),
                    vec![
                        json!({"id":"nested-tool","agent":"nested","stage":"running","outcome":"pending"}),
                        json!({"id":"unrelated-tool","agent":"other-lead","stage":"running","outcome":"pending"}),
                    ],
                ),
                (
                    "runtime_tasks".to_owned(),
                    vec![
                        json!({"id":"nested-task","agent":"nested","status":"running"}),
                        json!({"id":"archived-child-task","agent":"archived-child","status":"completed"}),
                        json!({"id":"unrelated-task","agent":"other-lead","status":"running"}),
                    ],
                ),
                (
                    "voice_sessions".to_owned(),
                    vec![
                        json!({"id":"nested-voice","agent":"nested","state":"running"}),
                        json!({"id":"unrelated-voice","agent":"other-lead","state":"running"}),
                    ],
                ),
            ]),
        };
        let full_snapshot = ManagementSnapshot::from_records(full.clone()).expect("full snapshot");
        let scoped_for = |target: &str| {
            let owners = full_snapshot.owner_scope(target).expect("owner scope");
            ManagementSnapshot::from_records(scoped_records(&full, &owners))
                .expect("scoped snapshot")
        };
        assert!(
            full_snapshot
                .owner_scope("worker")
                .expect("worker scope")
                .contains("nested"),
            "the worker's scope includes its descendants"
        );

        let restore = Action::Restore {
            actor: "lead".to_owned(),
            epoch: 1,
            target: "archived".to_owned(),
        };
        let restored = decide_with_effects(&full_snapshot, &restore, 10.0).expect("restore");
        assert_eq!(restored.result["status"], "restored");
        assert_same_decision(&full_snapshot, &scoped_for("archived"), &restore);

        for target in ["worker", "nested"] {
            let inspect = Action::Inspect {
                actor: "lead".to_owned(),
                epoch: 1,
                target: target.to_owned(),
            };
            assert_same_decision(&full_snapshot, &scoped_for(target), &inspect);
        }
        // The descendant's own tool request decides its inspection in both.
        let inspect_nested = Action::Inspect {
            actor: "lead".to_owned(),
            epoch: 1,
            target: "nested".to_owned(),
        };
        let Err(refusal) = decide_with_effects(&scoped_for("nested"), &inspect_nested, 10.0) else {
            panic!("inspection is unavailable");
        };
        assert!(
            format!("{refusal:?}").contains("tool-driver/archive-reconciliation"),
            "{refusal:?}"
        );
    }

    fn assert_same_decision(
        full: &ManagementSnapshot,
        scoped: &ManagementSnapshot,
        action: &Action,
    ) {
        let (full_decision, scoped_decision) = match (
            decide_with_effects(full, action, 10.0),
            decide_with_effects(scoped, action, 10.0),
        ) {
            (Ok(full), Ok(scoped)) => (full, scoped),
            (Err(full), Err(scoped)) => {
                assert_eq!(format!("{full:?}"), format!("{scoped:?}"));
                return;
            }
            (full, scoped) => panic!(
                "scoped decision diverged: full ok={} scoped ok={}",
                full.is_ok(),
                scoped.is_ok()
            ),
        };
        assert_eq!(full_decision.result, scoped_decision.result);
        assert_eq!(
            serde_json::to_value(&full_decision.writes).expect("full writes"),
            serde_json::to_value(&scoped_decision.writes).expect("scoped writes")
        );
        assert_eq!(full_decision.cancel_pending, scoped_decision.cancel_pending);
        assert_eq!(
            effect_values(&full_decision),
            effect_values(&scoped_decision)
        );
    }
}

#[cfg(test)]
mod archive_parity_tests {
    use super::{Action, ManagementSnapshot, Records, decide_with_effects};
    use serde_json::{Value, json};
    use std::collections::BTreeMap;

    /// A finished reviewer as the live state holds it after its result reached
    /// the lead: a native thread, finished tool requests, finished tasks.
    fn records(worker: Value, tools: Vec<Value>, tasks: Vec<Value>) -> Records {
        Records {
            tables: BTreeMap::from([
                (
                    "runtime_agents".to_owned(),
                    vec![
                        json!({"id":"lead","rootId":"lead","epoch":4,"status":"running",
                            "autoWake":true,"isLead":true}),
                        worker,
                    ],
                ),
                ("runtime_tool_requests".to_owned(), tools),
                ("runtime_tasks".to_owned(), tasks),
            ]),
        }
    }

    fn worker(fields: Value) -> Value {
        let mut record = json!({"id":"worker","rootId":"lead","parentId":"lead","epoch":2,
            "status":"completed","autoWake":true,"isLead":false,"role":"reviewer",
            "threadId":"thread-1","lastCompletedTurn":"turn-9",
            "lastCompletedTurnStatus":"completed"});
        for (key, value) in fields.as_object().expect("fields") {
            record[key] = value.clone();
        }
        record
    }

    fn archive(records: Records) -> Value {
        let snapshot = ManagementSnapshot::from_records(records).expect("snapshot");
        decide_with_effects(
            &snapshot,
            &Action::Archive {
                actor: "lead".to_owned(),
                epoch: 4,
                target: "worker".to_owned(),
                reason: "Reviewer result delivered to parent".to_owned(),
                unassign_work: false,
            },
            50.0,
        )
        .expect("archive decides")
        .result
    }

    #[test]
    fn a_finished_worker_with_a_thread_and_tool_history_is_archived() {
        // Live cutover 4: 18 reviewers kept "Not implemented: requires
        // tool-driver/archive-reconciliation" or "account-worker/agent-interrupt".
        let result = archive(records(
            worker(json!({})),
            vec![
                json!({"id":"tool-done","agent":"worker","stage":"completed","outcome":"applied"}),
                json!({"id":"tool-lost","agent":"worker","stage":"failed","outcome":"unknown"}),
            ],
            vec![json!({"id":"task-done","agent":"worker","status":"completed"})],
        ));
        assert_eq!(result["status"], "archived", "{result}");
    }

    #[test]
    fn open_work_is_a_blocker_not_a_refusal() {
        let blocked = |records: Records, kind: &str| {
            let result = archive(records);
            assert_eq!(result["status"], "blocked", "{result}");
            assert!(
                result["blockers"]
                    .as_array()
                    .is_some_and(|items| items.iter().any(|item| item["kind"] == kind)),
                "{kind}: {result}"
            );
        };
        blocked(
            records(
                worker(json!({})),
                vec![
                    json!({"id":"tool-open","agent":"worker","stage":"running","outcome":"pending"}),
                ],
                Vec::new(),
            ),
            "tool_requests",
        );
        blocked(
            records(
                worker(json!({})),
                Vec::new(),
                vec![json!({"id":"task-open","agent":"worker","status":"running"})],
            ),
            "background_tasks",
        );
        // Python reads the native thread, or accepts the durable completion of
        // the last submitted turn; without either the archive waits.
        blocked(
            records(
                worker(json!({"lastCompletedTurnStatus":Value::Null})),
                Vec::new(),
                Vec::new(),
            ),
            "native_state_unconfirmed",
        );
        // A worker that may still act keeps its unknown outcome as a blocker.
        blocked(
            records(
                worker(json!({"status":"waiting"})),
                vec![
                    json!({"id":"tool-lost","agent":"worker","stage":"failed","outcome":"unknown"}),
                ],
                Vec::new(),
            ),
            "tool_requests",
        );
    }
}
