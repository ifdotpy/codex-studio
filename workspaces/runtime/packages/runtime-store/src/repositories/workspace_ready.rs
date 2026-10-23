//! Durable identity and compare-before-publication for host worktree provisioning.
use crate::path_identity::{CanonicalPath, LexicalPath};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use crate::{
    AgentRecord, AgentRepository, CheckpointRecord, CheckpointsRepository, EventWriteRepository,
    StoreError, StoreResult, Table, WorkspaceSnapshot,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvisionIdentity {
    pub id: String,
    pub agent: String,
    pub source: Value,
    /// The thread the "Before first turn" checkpoint records; not part of `source`, so a
    /// thread/start does not refuse a prepared workspace.
    #[serde(default)]
    pub thread_id: Value,
    pub repository: CanonicalPath,
    pub common_dir: CanonicalPath,
    pub destination: LexicalPath,
    pub cwd: LexicalPath,
    pub branch: String,
    pub base_commit: String,
}

/// Only workspace-affecting fields participate; unrelated progress updates may continue.
pub fn workspace_generation(actor: &Value) -> Value {
    const FIELDS: &[&str] = &[
        "id",
        "epoch",
        "cwd",
        "accountKey",
        "rootId",
        "autoWake",
        "deletedAt",
        "worktree",
        "worktreeReady",
        "branch",
        "workerBaseCommit",
        "workspaceMode",
        "workspaceModeExplicit",
        "workspaceInGit",
        "executionMode",
        "environment",
        "imageWorkspace",
        "imageWorkspaceReady",
        "imageWorkspacePhase",
        "layrReady",
        "layrLine",
        "checkpointHistoryHead",
        "lastCompletedTurn",
    ];
    Value::Object(
        FIELDS
            .iter()
            .filter_map(|key| actor.get(*key).map(|v| ((*key).into(), v.clone())))
            .collect(),
    )
}

/// Submission must compare this opaque token inside its transaction before consuming input.
/// The only path accessor is successful validation; refused admission returns no cwd.
/// ```compile_fail
/// use studio_runtime_store::workspace_ready::ReadyToken;
/// fn bypass(token: ReadyToken) { let _ = token.cwd; }
/// ```
#[derive(Clone, PartialEq)]
pub struct ReadyToken {
    agent: String,
    epoch: u64,
    source: Value,
    cwd: PathBuf,
    paths: WorkspacePaths,
}
impl std::fmt::Debug for ReadyToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ReadyToken(..)")
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadyRefusal;

impl ReadyToken {
    pub fn new(record: &Value, cwd: PathBuf) -> Self {
        Self {
            agent: record["id"].as_str().unwrap_or_default().into(),
            epoch: record["epoch"].as_u64().unwrap_or_default(),
            source: workspace_generation(record),
            paths: WorkspacePaths(BTreeMap::from([(
                PathBuf::from(record["cwd"].as_str().unwrap_or("/")),
                cwd.clone(),
            )])),
            cwd,
        }
    }
    pub fn with_paths(mut self, paths: WorkspacePaths) -> Self {
        self.paths = paths;
        self
    }
    pub fn validate(&self, tx: &Transaction<'_>) -> StoreResult<Result<PathBuf, ReadyRefusal>> {
        let Some(current) = CheckpointsRepository::reservation_snapshot(tx, &self.agent)? else {
            return Ok(Err(ReadyRefusal));
        };
        let valid = current.agent.record["epoch"] == self.epoch
            && workspace_generation(&current.agent.record) == self.source
            && current.agent.record["autoWake"] == true
            && current
                .agent
                .record
                .get("deletedAt")
                .is_none_or(Value::is_null)
            && self
                .paths
                .reservations(&current)
                .is_some_and(|reservations| {
                    !reservations
                        .iter()
                        .any(|r| r.blocks(&self.agent, &self.cwd, None))
                });
        Ok(if valid {
            Ok(self.cwd.clone())
        } else {
            Err(ReadyRefusal)
        })
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Reservation {
    pub agent: String,
    pub cwd: PathBuf,
    pub operation: String,
    pub id: Option<String>,
    pub phase: Option<String>,
}

impl Reservation {
    /// Whether this reservation conflicts with `actor` working in `cwd` and, when planning a
    /// worktree, creating `destination`. Another agent's provision owns only its destination:
    /// it conflicts with an actor inside that destination or planning an overlapping one, never
    /// with an actor elsewhere in the same repository.
    #[must_use]
    pub fn blocks(&self, actor: &str, cwd: &Path, destination: Option<&Path>) -> bool {
        if self.agent == actor {
            return true;
        }
        if self.operation == "provision" {
            cwd.starts_with(&self.cwd) || destination.is_some_and(|d| overlaps(d, &self.cwd))
        } else {
            overlaps(cwd, &self.cwd) || destination.is_some_and(|d| overlaps(d, &self.cwd))
        }
    }
}

pub fn overlaps(a: &Path, b: &Path) -> bool {
    a.starts_with(b) || b.starts_with(a)
}

/// Raw path bindings observed before writer admission. Include idle agent paths:
/// they may acquire a reservation before the transaction starts.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct WorkspacePaths(BTreeMap<PathBuf, PathBuf>);
impl WorkspacePaths {
    pub fn capture(snapshot: &WorkspaceSnapshot, extra: impl IntoIterator<Item = PathBuf>) -> Self {
        let paths = std::iter::once(&snapshot.agent)
            .chain(&snapshot.agents)
            .chain(&snapshot.operations)
            .filter_map(|row| row.record["cwd"].as_str().map(PathBuf::from))
            .chain(extra);
        Self(
            paths
                .map(|path| {
                    let resolved = crate::path_identity::resolve(&path)
                        .map(crate::path_identity::CanonicalPath::into_path_buf)
                        .unwrap_or_else(|| path.clone());
                    (path, resolved)
                })
                .collect(),
        )
    }
    /// `None` when a reservation names a path whose identity was not captured.
    /// A provisioning destination needs no capture: the provisioning owner
    /// builds it under the canonical repository root and refuses a linked
    /// ancestor, so its recorded form is its identity. A sibling that starts
    /// provisioning after this capture therefore leaves the capture valid.
    pub fn reservations(&self, snapshot: &WorkspaceSnapshot) -> Option<Vec<Reservation>> {
        let mut missing = false;
        let result = reservations_with(snapshot, |value, destination| {
            let path = PathBuf::from(value.as_str().unwrap_or("/"));
            self.0.get(&path).cloned().unwrap_or_else(|| {
                missing |= !destination;
                path
            })
        });
        (!missing).then_some(result)
    }
}

/// Resolve paths with the path identity owner; unresolved paths remain
/// evidence rather than disappearing.
pub fn reservations(snapshot: &WorkspaceSnapshot) -> Vec<Reservation> {
    reservations_with(snapshot, |v, _| {
        let path = PathBuf::from(v.as_str().unwrap_or("/"));
        crate::path_identity::resolve(&path)
            .map(crate::path_identity::CanonicalPath::into_path_buf)
            .unwrap_or(path)
    })
}

/// `path` resolves a recorded path; its flag says whether the path is a
/// provisioning destination.
fn reservations_with(
    snapshot: &WorkspaceSnapshot,
    mut path: impl FnMut(&Value, bool) -> PathBuf,
) -> Vec<Reservation> {
    let mut result = Vec::new();
    for other in &snapshot.agents {
        if let Some(kind) = other.record["workspaceOperation"].as_str() {
            let id = other.record["workspaceReservationId"].as_str();
            let op = snapshot
                .operations
                .iter()
                .find(|op| Some(op.id.as_str()) == id)
                .or_else(|| {
                    snapshot
                        .operations
                        .iter()
                        .find(|op| op.record["agent"] == other.id)
                });
            // A provision reserves its destination; the agent's cwd is still the source.
            let cwd = match (kind, op) {
                ("provision", Some(op)) => {
                    path(&op.record["cwd"], op.record["kind"] == "provision")
                }
                _ => path(&other.record["cwd"], false),
            };
            result.push(Reservation {
                agent: other.id.clone(),
                cwd,
                operation: kind.into(),
                id: id.map(str::to_owned).or_else(|| op.map(|o| o.id.clone())),
                phase: op
                    .and_then(|o| o.record["phase"].as_str())
                    .map(str::to_owned),
            });
        }
    }
    for op in &snapshot.operations {
        if result.iter().any(|r| r.id.as_deref() == Some(&op.id)) {
            continue;
        }
        result.push(Reservation {
            agent: op.record["agent"].as_str().unwrap_or_default().into(),
            cwd: path(&op.record["cwd"], op.record["kind"] == "provision"),
            operation: op.record["kind"].as_str().unwrap_or_default().into(),
            id: Some(op.id.clone()),
            phase: op.record["phase"].as_str().map(str::to_owned),
        });
    }
    result
}

fn conflicts(
    snapshot: &WorkspaceSnapshot,
    plan: &ProvisionIdentity,
    paths: &WorkspacePaths,
) -> Option<Vec<Reservation>> {
    let raw = PathBuf::from(plan.source["cwd"].as_str().unwrap_or("/"));
    let source = paths.0.get(&raw)?;
    Some(
        paths
            .reservations(snapshot)?
            .into_iter()
            .filter(|r| {
                !(r.agent == plan.agent
                    && r.operation == "provision"
                    && r.id.as_deref() == Some(&plan.id))
                    && r.blocks(&plan.agent, source, Some(&plan.destination))
            })
            .collect(),
    )
}

/// Release reason stored on a provisioning journal that ended because its
/// failed worker was archived.
pub const ARCHIVED_PROVISION_RELEASE: &str =
    "Worker archived; files at the provisioning destination were kept";

pub struct WorkspaceReadyRepository;
impl WorkspaceReadyRepository {
    pub fn journal(connection: &rusqlite::Connection, id: &str) -> StoreResult<Option<Value>> {
        connection
            .query_row(
                "SELECT record FROM runtime_workspace_operations WHERE id=?1",
                [id],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|raw| serde_json::from_str(&raw).map_err(StoreError::from))
            .transpose()
    }

    pub fn reserve(
        tx: &Transaction<'_>,
        expected: &WorkspaceSnapshot,
        plan: &ProvisionIdentity,
        paths: &WorkspacePaths,
        now: f64,
    ) -> StoreResult<bool> {
        let Some(current) = CheckpointsRepository::reservation_snapshot(tx, &plan.agent)? else {
            return Ok(false);
        };
        if workspace_generation(&current.agent.record) != plan.source
            || conflicts(&current, plan, paths) != conflicts(expected, plan, paths)
            || conflicts(&current, plan, paths).is_none_or(|conflicts| !conflicts.is_empty())
        {
            return Ok(false);
        }
        let mut operation = if let Some(operation) = Self::journal(tx, &plan.id)? {
            if operation.get("plan") != Some(&serde_json::to_value(plan)?)
                || operation["phase"] == "completed"
            {
                return Ok(false);
            }
            operation
        } else {
            json!({"id":plan.id,"kind":"provision","agent":plan.agent,"cwd":plan.destination,
                "epoch":plan.source.get("epoch"),"created":now,"plan":plan})
        };
        operation["phase"] = json!("local_mutation");
        operation["updated"] = json!(now);
        tx.execute("INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![plan.id, operation.to_string()])?;
        let mut actor = current.agent.record;
        actor["workspaceOperation"] = json!("provision");
        actor["workspaceReservationId"] = json!(plan.id);
        actor["worktreePreparation"] = json!("preparing");
        write_agent(
            tx,
            &actor,
            &[
                "workspaceOperation",
                "workspaceReservationId",
                "worktreePreparation",
            ],
        )?;
        Ok(true)
    }

    /// Leave evidence even when an actor was deleted, stopped, or replaced during Git.
    pub fn uncertain(
        tx: &Transaction<'_>,
        plan: &ProvisionIdentity,
        reason: &str,
        now: f64,
    ) -> StoreResult<()> {
        if let Some(mut operation) = Self::journal(tx, &plan.id)? {
            if operation.get("plan") != Some(&serde_json::to_value(plan)?)
                || operation["phase"] == "completed"
            {
                return Ok(());
            }
            operation["phase"] = json!("recovery_required");
            operation["error"] = json!(reason);
            operation["updated"] = json!(now);
            tx.execute(
                "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
                params![plan.id, operation.to_string()],
            )?;
        }
        Ok(())
    }

    /// End the `recovery_required` provisioning `id` of `agent` when the
    /// failed worker is archived. Its Git effect has returned, so no workspace
    /// change is in flight. The journal stays as history with its error and
    /// destination; files there are not touched. A journal in any other state
    /// fails the transaction.
    pub fn release_archived(
        tx: &Transaction<'_>,
        id: &str,
        agent: &str,
        now: f64,
    ) -> StoreResult<()> {
        let Some(mut operation) = Self::journal(tx, id)? else {
            return Err(StoreError::RecordContract(
                "Archived provisioning journal is missing",
            ));
        };
        if operation["kind"] != "provision"
            || operation["agent"] != agent
            || operation["phase"] != "recovery_required"
        {
            return Err(StoreError::RecordContract(
                "Archived provisioning changed before it was released",
            ));
        }
        operation["phase"] = json!("failed");
        operation["updated"] = json!(now);
        operation["release"] = json!({"reason":ARCHIVED_PROVISION_RELEASE,"at":now});
        tx.execute(
            "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
            params![id, operation.to_string()],
        )?;
        Ok(())
    }

    pub fn publish(
        tx: &Transaction<'_>,
        plan: &ProvisionIdentity,
        paths: &WorkspacePaths,
        checkpoint: Result<&Value, &str>,
        now: f64,
    ) -> StoreResult<bool> {
        let Some(mut operation) = Self::journal(tx, &plan.id)? else {
            return Ok(false);
        };
        let current = CheckpointsRepository::agent(tx, &plan.agent)?;
        let relevant = CheckpointsRepository::reservation_snapshot(tx, &plan.agent)?;
        let valid = relevant.as_ref().is_some_and(|s| {
            conflicts(s, plan, paths).is_some_and(|conflicts| conflicts.is_empty())
        }) && current.as_ref().is_some_and(|a| {
            workspace_generation(&a.record) == plan.source
                && a.record["autoWake"] == true
                && a.record.get("deletedAt").is_none_or(Value::is_null)
                && a.record["workspaceReservationId"] == plan.id
                && a.record["workspaceOperation"] == "provision"
        }) && operation.get("plan") == Some(&serde_json::to_value(plan)?)
            && matches!(
                operation["phase"].as_str(),
                Some("local_mutation" | "recovery_required")
            );
        if !valid {
            Self::uncertain(
                tx,
                plan,
                "Actor changed; created worktree remains unattached",
                now,
            )?;
            return Ok(false);
        }
        let Some(current) = current else {
            return Ok(false);
        };
        let mut actor = current.record;
        actor["cwd"] = json!(plan.cwd);
        actor["branch"] = json!(plan.branch);
        actor["worktreeReady"] = json!(true);
        actor["workspaceOperation"] = Value::Null;
        if let Some(object) = actor.as_object_mut() {
            object.remove("worktreePreparation");
            object.remove("workspaceReservationId");
        }
        match checkpoint {
            Ok(value) => {
                let mut value = value.clone();
                let parent = actor.get("checkpointHistoryHead").and_then(Value::as_str);
                let boundary = parent
                    .map(|id| CheckpointsRepository::checkpoint(tx, id))
                    .transpose()?
                    .flatten()
                    .and_then(|p| p.record["historyBoundary"].as_i64())
                    .filter(|b| *b != 0);
                let (delta, boundary) =
                    CheckpointsRepository::history_delta(tx, &plan.agent, boundary)?;
                value["historyParent"] = json!(parent);
                value["historyDelta"] = json!(delta);
                value["historyBoundary"] = json!(boundary);
                value["turnId"] = actor
                    .get("lastCompletedTurn")
                    .cloned()
                    .unwrap_or(Value::Null);
                let record = CheckpointRecord::from_value(value.clone())?;
                Table::<CheckpointRecord>::new(tx).put(&record)?;
                actor["checkpointHistoryHead"] = value["id"].clone();
                actor["checkpointError"] = Value::Null;
            }
            Err(error) => actor["checkpointError"] = json!(error),
        }
        write_agent(
            tx,
            &actor,
            &[
                "cwd",
                "branch",
                "worktreeReady",
                "workspaceOperation",
                "workspaceReservationId",
                "worktreePreparation",
                "checkpointHistoryHead",
                "lastCompletedTurn",
                "checkpointError",
            ],
        )?;
        operation["phase"] = json!("completed");
        operation["updated"] = json!(now);
        operation["error"] = Value::Null;
        tx.execute(
            "UPDATE runtime_workspace_operations SET record=?2 WHERE id=?1",
            params![plan.id, operation.to_string()],
        )?;
        Ok(true)
    }
}

fn write_agent(tx: &Transaction<'_>, value: &Value, fields: &[&str]) -> StoreResult<()> {
    let record = AgentRecord::from_value(value.clone())?;
    AgentRepository::put(tx, &record)?;
    let changes = fields
        .iter()
        .map(|field| (*field, value.get(*field).cloned().unwrap_or(Value::Null)))
        .collect::<Vec<_>>();
    EventWriteRepository::set_agent_sync_values(tx, record.id().0.as_str(), &changes)
}
