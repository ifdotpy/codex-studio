//! Filesystem preparation for host project writes. No filesystem access in a transaction.
use crate::projects::{ProjectsError, project_key};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use studio_runtime_store::{ProjectsRepository, RuntimeStore};

/// Paths are resolved once outside the writer, then their source rows are rechecked.
/// An intervening host-path edit fails before any write rather than using stale membership.
pub struct PreparedHost {
    projects: Vec<(String, Value)>,
    agent_fences: Vec<AgentFence>,
    paths: BTreeMap<String, String>,
    server: Option<String>,
}
#[derive(Clone, Debug, PartialEq, Eq)]
struct AgentFence {
    id: String,
    path: Option<Value>,
    cwd: Option<Value>,
    epoch: Option<Value>,
    status: Option<Value>,
    auto_wake: Option<Value>,
    deleted_at: Option<Value>,
    archived_at: Option<Value>,
    attempt_id: Option<Value>,
    attempt_epoch: Option<Value>,
    attempt_connection_generation: Option<Value>,
}
impl PreparedHost {
    /// Read the project and agent rows, then resolve their paths on the blocking pool.
    pub async fn load(store: &RuntimeStore) -> Result<Self, ProjectsError> {
        let (projects, agents) = store
            .read_async("runtime-services/host_project_runtime", |db| {
                Ok::<_, ProjectsError>((
                    ProjectsRepository::list(db)?,
                    ProjectsRepository::agents(db)?,
                ))
            })
            .await?;
        let server = store.local_server_id()?;
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/host_project_runtime",
            move || Self::from_rows(projects, agents, server),
        )
        .await
        .map_err(|_| ProjectsError::PreparationWorker)?
    }

    fn from_rows(
        projects: Vec<(String, Value)>,
        mut agents: Vec<(String, Value)>,
        server: Option<String>,
    ) -> Result<Self, ProjectsError> {
        for (table, rows) in [("runtime_projects", &projects), ("runtime_agents", &agents)] {
            for (id, row) in rows {
                studio_runtime_store::system_reads::validate(table, id, row.clone())?;
            }
        }
        agents.sort_by(|a, b| a.0.cmp(&b.0));
        let mut paths = BTreeMap::new();
        for (_, row) in projects.iter().chain(&agents) {
            for field in ["path", "cwd"] {
                if let Some(path) = row.get(field).and_then(Value::as_str) {
                    paths.insert(path.to_owned(), project_key(Some(&json!(path)), false)?);
                }
            }
        }
        let agent_fences = agent_fences(&agents);
        Ok(Self {
            projects,
            agent_fences,
            paths,
            server,
        })
    }

    pub fn recheck(&self, db: &Connection) -> Result<(), ProjectsError> {
        if ProjectsRepository::list(db)? != self.projects
            || !agent_fences_match(
                &self.agent_fences,
                &agent_fences(&ProjectsRepository::agents(db)?),
            )
        {
            return Err(ProjectsError::Conflict(
                "Project state changed during preparation. Retry",
            ));
        }
        Ok(())
    }

    /// Re-resolve every prepared path immediately before opening a writer.
    /// The filesystem can still change after this check; the transaction fences
    /// source rows but deliberately performs no filesystem I/O.
    /// This is intentionally filesystem work outside the transaction; `recheck`
    /// then fences the database rows inside the transaction that applies them.
    pub fn recheck_paths(&self) -> Result<(), ProjectsError> {
        for (_, row) in &self.projects {
            for field in ["path", "cwd"] {
                let Some(path) = row.get(field).and_then(Value::as_str) else {
                    continue;
                };
                let current = project_key(Some(&json!(path)), false)?;
                if self.paths.get(path) != Some(&current) {
                    return Err(ProjectsError::Conflict(
                        "Project path changed during preparation. Retry",
                    ));
                }
            }
        }
        for agent in &self.agent_fences {
            for path in [agent.path.as_ref(), agent.cwd.as_ref()]
                .into_iter()
                .flatten()
            {
                let Some(path) = path.as_str() else {
                    continue;
                };
                let current = project_key(Some(&json!(path)), false)?;
                if self.paths.get(path) != Some(&current) {
                    return Err(ProjectsError::Conflict(
                        "Project path changed during preparation. Retry",
                    ));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn resolve(&self, path: &str) -> Result<&str, ProjectsError> {
        self.paths
            .get(path)
            .map(String::as_str)
            .ok_or(ProjectsError::Conflict(
                "Project path changed during preparation. Retry",
            ))
    }

    pub(crate) fn server(&self) -> Result<&str, ProjectsError> {
        self.server.as_deref().ok_or(ProjectsError::Requires(
            "cross-server-driver/local-identity",
        ))
    }

    pub(crate) fn resolved_paths(&self) -> &BTreeMap<String, String> {
        &self.paths
    }
}

fn agent_fences(agents: &[(String, Value)]) -> Vec<AgentFence> {
    let mut fences = agents
        .iter()
        .map(|(id, row)| AgentFence {
            id: id.clone(),
            path: row.get("path").cloned(),
            cwd: row.get("cwd").cloned(),
            epoch: row.get("epoch").cloned(),
            status: row.get("status").cloned(),
            auto_wake: row.get("autoWake").cloned(),
            deleted_at: row.get("deletedAt").cloned(),
            archived_at: row.get("archivedAt").cloned(),
            attempt_id: row.pointer("/startAttempt/id").cloned(),
            attempt_epoch: row.pointer("/startAttempt/epoch").cloned(),
            attempt_connection_generation: row
                .pointer("/startAttempt/connectionGeneration")
                .cloned(),
        })
        .collect::<Vec<_>>();
    fences.sort_by(|left, right| left.id.cmp(&right.id));
    fences
}

fn agent_fences_match(before: &[AgentFence], current: &[AgentFence]) -> bool {
    before.len() == current.len()
        && before.iter().zip(current).all(|(before, current)| {
            before.id == current.id
                && before.path == current.path
                && before.cwd == current.cwd
                && before.epoch == current.epoch
                && before.auto_wake == current.auto_wake
                && before.deleted_at == current.deleted_at
                && before.archived_at == current.archived_at
                && (before.status == current.status
                    || (before.status.as_ref().and_then(Value::as_str) == Some("idle")
                        && current.status.as_ref().and_then(Value::as_str) == Some("running")
                        && before.attempt_id.is_some()
                        && same_start_attempt(before, current))
                    // The account dispatcher parks an already-starting agent
                    // with no runnable event as waiting. This status-only
                    // transition does not change the prepared path or attempt.
                    || (before.status.as_ref().and_then(Value::as_str) == Some("starting")
                        && current.status.as_ref().and_then(Value::as_str) == Some("waiting")
                        && before.attempt_id.is_some()
                        && same_start_attempt(before, current)))
        })
}

fn same_start_attempt(before: &AgentFence, current: &AgentFence) -> bool {
    before.attempt_id == current.attempt_id
        && before.attempt_epoch == current.attempt_epoch
        && before.attempt_connection_generation == current.attempt_connection_generation
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fence(status: &str) -> AgentFence {
        AgentFence {
            id: "agent".into(),
            path: Some(json!("/workspace")),
            cwd: Some(json!("/workspace")),
            epoch: Some(json!(7)),
            status: Some(json!(status)),
            auto_wake: Some(json!(true)),
            deleted_at: None,
            archived_at: None,
            attempt_id: Some(json!("attempt")),
            attempt_epoch: Some(json!(7)),
            attempt_connection_generation: Some(json!(3)),
        }
    }

    #[test]
    fn same_attempt_starting_to_waiting_keeps_prepared_host_valid() {
        assert!(agent_fences_match(
            &[fence("starting")],
            &[fence("waiting")]
        ));
    }

    #[test]
    fn status_progression_does_not_hide_changed_attempt_identity_or_paths() {
        let before = fence("starting");
        for mutate in [
            Box::new(|current: &mut AgentFence| current.attempt_id = Some(json!("other")))
                as Box<dyn Fn(&mut AgentFence)>,
            Box::new(|current: &mut AgentFence| current.attempt_epoch = Some(json!(8))),
            Box::new(|current: &mut AgentFence| {
                current.attempt_connection_generation = Some(json!(4))
            }),
            Box::new(|current: &mut AgentFence| current.epoch = Some(json!(8))),
            Box::new(|current: &mut AgentFence| current.path = Some(json!("/other"))),
            Box::new(|current: &mut AgentFence| current.cwd = Some(json!("/other"))),
        ] {
            let mut current = fence("waiting");
            mutate(&mut current);
            assert!(!agent_fences_match(
                std::slice::from_ref(&before),
                &[current]
            ));
        }
    }
}

/// A checked local binding for P02's draft/commit transaction.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProjectBinding {
    pub project_id: String,
    pub project_server_id: String,
}
impl PreparedHost {
    pub fn chat_binding(
        &self,
        db: &Connection,
        cwd: &str,
        project_id: Option<&str>,
        home_server: Option<&str>,
    ) -> Result<Option<ProjectBinding>, ProjectsError> {
        if project_id.is_none() && home_server.is_none() {
            return Ok(None);
        }
        let project_id = project_id
            .filter(|s| !s.is_empty())
            .ok_or(ProjectsError::Invalid("a project ID is required"))?;
        let home = home_server
            .filter(|s| !s.is_empty())
            .ok_or(ProjectsError::Invalid("a project server ID is required"))?;
        let local = self.server()?;
        if home != local {
            return Err(ProjectsError::Requires(
                "cross-server-driver/project-binding",
            ));
        }
        self.recheck(db)?;
        for (_, p) in ProjectsRepository::list(db)? {
            let direct = p["id"] == project_id
                && p["homeServerId"] == home
                && p["locations"].as_array().is_some_and(|rows| {
                    rows.iter()
                        .any(|l| l["serverId"] == local && l["path"] == cwd)
                });
            let alias = p["path"] == cwd
                && p["projectAliases"].as_array().is_some_and(|rows| {
                    rows.iter()
                        .any(|a| a["serverId"] == home && a["projectId"] == project_id)
                });
            if direct || alias {
                return Ok(Some(ProjectBinding {
                    project_id: project_id.to_owned(),
                    project_server_id: home.to_owned(),
                }));
            }
        }
        Err(ProjectsError::Invalid(
            "The selected folder is not a location of this project",
        ))
    }

    pub fn settings_key(
        &self,
        db: &Connection,
        cwd: &str,
        binding: Option<&ProjectBinding>,
    ) -> Result<String, ProjectsError> {
        let Some(binding) = binding else {
            return Ok(cwd.to_owned());
        };
        let local = self.server()?;
        for (id, p) in ProjectsRepository::list(db)? {
            let direct = id == binding.project_id
                && p["homeServerId"] == binding.project_server_id
                && p["locations"].as_array().is_some_and(|rows| {
                    rows.iter()
                        .any(|l| l["serverId"] == local && l["path"] == cwd)
                });
            let alias = p["path"] == cwd
                && p["projectAliases"].as_array().is_some_and(|rows| {
                    rows.iter().any(|a| {
                        a["serverId"] == binding.project_server_id
                            && a["projectId"] == binding.project_id
                    })
                });
            if direct || alias {
                return Ok(id);
            }
        }
        Ok(cwd.to_owned())
    }
}
