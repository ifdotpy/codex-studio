//! Pure lifecycle decisions over checked Python-written records.
//!
//! The adapter supplies time and one transaction snapshot. Decisions never send
//! native requests: operations requiring a live owner are refused before writes.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_store::AgentStatus;
use thiserror::Error;

use crate::event_write::{python_strip, truthy};

const MAX_ROWS: usize = 10_000;
const MAX_RECORD_BYTES: usize = 1_048_576;
const MAX_SAFE_EPOCH: i64 = 9_007_199_254_740_991;
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

#[derive(Deserialize)]
struct VoiceSession {
    agent: String,
    state: Option<String>,
    ended: Option<f64>,
}

pub struct ManagementSnapshot {
    agents: Vec<Agent>,
    work: Vec<Work>,
    voice_sessions: Vec<VoiceSession>,
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
}
impl Decision {
    fn result(result: Value) -> Self {
        Self {
            result,
            writes: Vec::new(),
            cancel_pending: Vec::new(),
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
        let mut voice_sessions = Vec::new();
        for (table, rows) in &records.tables {
            if rows.len() > MAX_ROWS {
                return Err(ManagementError::Unavailable(
                    "agent-management/snapshot-limit",
                ));
            }
            let mut ids = BTreeSet::new();
            for row in rows {
                let id = row.get("id").and_then(Value::as_str).unwrap_or("<missing>");
                let invalid = || ManagementError::Stored {
                    table: table.clone(),
                    id: id.to_owned(),
                };
                if !row.is_object()
                    || id == "<missing>"
                    || !ids.insert(id)
                    || row.to_string().len() > MAX_RECORD_BYTES
                {
                    return Err(invalid());
                }
                if let Some(schema) = studio_runtime_schema::table_schema(table)
                    && !studio_runtime_schema::validate(schema, row).map_err(|_| invalid())?
                {
                    return Err(invalid());
                }
                if table == "voice_sessions" {
                    voice_sessions
                        .push(serde_json::from_value(row.clone()).map_err(|_| invalid())?);
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
            voice_sessions,
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
    fn refuse_live(&self, agent: &Agent) -> Result<(), ManagementError> {
        if agent.active()
            || [
                "threadId",
                "turnId",
                "startAttempt",
                "workspaceOperation",
                "accountTransferId",
            ]
            .iter()
            .any(|key| agent.flag(key))
        {
            return Err(ManagementError::Unavailable(
                "account-worker/agent-interrupt",
            ));
        }
        if ["remoteWorker", "remoteAnchor", "remoteOrigin"]
            .iter()
            .any(|key| agent.flag(key))
        {
            return Err(ManagementError::Unavailable(
                "cross-server/agent-management",
            ));
        }
        if agent.flag("capacityRetry")
            || agent
                .record
                .get("usageResume")
                .and_then(|r| r.get("status"))
                .and_then(Value::as_str)
                .is_some_and(|s| matches!(s, "scheduled" | "started"))
        {
            return Err(ManagementError::Unavailable("usage-policy/agent-stop"));
        }
        if self
            .owned("runtime_monitors", "agent", agent.id())
            .iter()
            .any(|r| is_status(r, &["starting", "running", "approval"]))
        {
            return Err(ManagementError::Unavailable("monitor-driver/agent-stop"));
        }
        if self.voice_sessions.iter().any(|session| {
            session.agent == agent.id() && session.state.is_some() && session.ended.is_none()
        }) {
            return Err(ManagementError::Unavailable("voice-driver/agent-stop"));
        }
        Ok(())
    }
    fn has_voice_data(&self, agent: &Agent) -> bool {
        [
            "voice_sessions",
            "voice_records",
            "voice_deliveries",
            "voice_approvals",
        ]
        .iter()
        .any(|table| !self.owned(table, "agent", agent.id()).is_empty())
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
        blocks
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

fn is_status(row: &Value, statuses: &[&str]) -> bool {
    row.get("status")
        .and_then(Value::as_str)
        .is_some_and(|s| statuses.contains(&s))
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
    if agent.record.get("executionMode").and_then(Value::as_str) == Some("vm")
        || [
            "imageWorkspaceReady",
            "worktreeReady",
            "worktreeCleanup",
            "cleanedWorktree",
            "cleanedImageWorkspace",
        ]
        .iter()
        .any(|key| agent.flag(key))
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
    if !now.is_finite() || now <= 0.0 {
        return Err(ManagementError::Invalid("Invalid lifecycle timestamp"));
    }
    match action {
        Action::Stop {
            target,
            descendants,
        } => stop(snapshot, target, *descendants, false, now),
        Action::Delete { target } => stop(snapshot, target, true, true, now),
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

fn stop(
    snapshot: &ManagementSnapshot,
    target: &str,
    descendants: bool,
    delete: bool,
    now: f64,
) -> Result<Decision, ManagementError> {
    let agents = snapshot.descendants(target, descendants)?;
    for agent in &agents {
        snapshot.refuse_live(agent)?;
        // Deletion also removes saved voice data; stop only ends open sessions.
        if delete && snapshot.has_voice_data(agent) {
            return Err(ManagementError::Unavailable("voice-driver/agent-stop"));
        }
        if delete {
            workspace_dependency(agent)?;
            if agent.flag("imageWorkspace") {
                return Err(ManagementError::Unavailable(
                    "workspace-state/agent-lifecycle",
                ));
            }
        }
        if !delete
            && agent.identity.is_lead != Some(true)
            && agent.identity.parent_id.is_some()
            && (!descendants || agent.id() == target)
            && !agent.flag("deletedAt")
            && !agent.flag("agentArchive")
        {
            return Err(ManagementError::Unavailable(
                "event-driver/child-stop-notification",
            ));
        }
    }
    if delete
        && snapshot
            .rows("runtime_work")
            .iter()
            .any(|r| !is_status(r, &["accepted", "cancelled"]))
    {
        return Err(ManagementError::Unavailable("work-driver/deletion-release"));
    }
    let mut ids = agents.iter().map(|a| a.id().to_owned()).collect::<Vec<_>>();
    ids.sort();
    let result = if delete {
        json!({"deleted":ids})
    } else {
        json!({"stopped":ids})
    };
    let mut decision = Decision::result(result);
    for agent in agents {
        let mut record = agent.record.clone();
        let epoch = next_epoch(agent)?;
        let object = record
            .as_object_mut()
            .ok_or(ManagementError::Invalid("Invalid agent"))?;
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
        object.insert(
            "error".to_owned(),
            json!(if delete {
                "Conversation deleted"
            } else {
                "Stopped by user"
            }),
        );
        if delete && !agent.flag("deletedAt") {
            object.insert("deletedAt".to_owned(), json!(now));
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
            return Err(ManagementError::Unavailable(
                "workspace-state/agent-lifecycle",
            ));
        }
        workspace_dependency(agent)?;
        return Ok(Decision::result(
            json!({"status":"archived","agent":brief(&agent.record),"replayed":true,"workspace":null,"worktree":{"state":"kept","reason":"already archived","bytes":0},"unassignedWork":agent.record["agentArchive"].get("unassignedWork").cloned().unwrap_or(json!([]))}),
        ));
    }
    snapshot.tool_dependency(agent)?;
    // Python may reconcile OS command liveness while computing blockers.
    if snapshot
        .owned("runtime_tasks", "agent", agent.id())
        .iter()
        .any(|r| is_status(r, &["running", "starting", "pending", "unknown"]))
    {
        return Err(ManagementError::Unavailable(
            "process-driver/archive-reconciliation",
        ));
    }
    let blockers = snapshot.blockers(agent, unassign);
    if !blockers.is_empty() {
        return Ok(Decision::result(
            json!({"status":"blocked","agent":brief(&agent.record),"blockers":blockers}),
        ));
    }
    snapshot.refuse_live(agent)?;
    workspace_dependency(agent)?;
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
    record["agentArchive"] = json!({"at":now,"by":actor,"reason":reason,"epoch":record["epoch"],"cleanupPending":false,"unknownToolRequests":[],"unassignedWork":unassigned});
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
