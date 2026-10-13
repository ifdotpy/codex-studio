//! Durable identity and compare-before-publication for host worktree provisioning.
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::path::PathBuf;

use crate::{
    AgentRecord, AgentRepository, CheckpointRecord, CheckpointsRepository, EventWriteRepository,
    StoreError, StoreResult, Table, WorkspaceSnapshot,
};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ProvisionIdentity {
    pub id: String,
    pub agent: String,
    pub source: Value,
    pub repository: PathBuf,
    pub common_dir: PathBuf,
    pub destination: PathBuf,
    pub cwd: PathBuf,
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
        "threadId",
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
    ];
    Value::Object(
        FIELDS
            .iter()
            .filter_map(|key| actor.get(*key).map(|v| ((*key).into(), v.clone())))
            .collect(),
    )
}

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
        now: f64,
    ) -> StoreResult<bool> {
        let Some(current) = CheckpointsRepository::workspace_snapshot(tx, &plan.agent)? else {
            return Ok(false);
        };
        if &current != expected || workspace_generation(&current.agent.record) != plan.source {
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

    pub fn publish(
        tx: &Transaction<'_>,
        plan: &ProvisionIdentity,
        checkpoint: Result<&Value, &str>,
        now: f64,
    ) -> StoreResult<bool> {
        let Some(mut operation) = Self::journal(tx, &plan.id)? else {
            return Ok(false);
        };
        let current = CheckpointsRepository::agent(tx, &plan.agent)?;
        let valid = current.as_ref().is_some_and(|a| {
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
