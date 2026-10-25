//! Read-only workspace maintenance facts for one authorized team.

use crate::agent_management::{ManagementError, ManagementSnapshot, Records};
#[cfg(unix)]
use crate::linux_vm_guest::GuestRpc;
use serde_json::{Value, json};
#[cfg(unix)]
use std::time::Duration;
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    sync::Arc,
};
use studio_runtime_store::{AgentId, AgentRepository, RuntimeStore};

/// External reads stay outside the store reader and never run cleanup effects.
trait Sources: Send + Sync {
    fn workspaces(&self) -> Result<Vec<Value>, String>;
    fn base(&self, repo: &Path) -> Result<Value, String>;
    fn linux_vm(&self, state_dir: &Path) -> Value;
}

struct ProductionSources;

impl Sources for ProductionSources {
    fn workspaces(&self) -> Result<Vec<Value>, String> {
        Err(crate::legacy_image_workspace::FINISHED_MESSAGE.to_owned())
    }

    fn base(&self, _repo: &Path) -> Result<Value, String> {
        Err(crate::legacy_image_workspace::FINISHED_MESSAGE.to_owned())
    }

    #[cfg(unix)]
    fn linux_vm(&self, state_dir: &Path) -> Value {
        let vm_dir = crate::vault::linux_vm_state_dir(state_dir);
        let control = crate::linux_vm_guest::UnixVmControlRpc::new(&vm_dir);
        let settings = match crate::linux_vm_settings::get_settings(&vm_dir) {
            Ok(settings) => settings,
            Err(error) => return json!({"state":"unavailable","error":error.to_string()}),
        };
        let host = match crate::linux_vm_settings::status(&vm_dir, &control) {
            Ok(status) => status,
            Err(error) => return json!({"state":"unavailable","error":error.to_string()}),
        };
        let mut result = json!({
            "state":host["state"], "settings":settings,
            "allocatedDiskBytes":host["allocatedDiskBytes"], "memoryLimitBytes":settings.memory_bytes,
        });
        if host["state"] == "running" {
            let guest = crate::linux_vm_guest::UnixGuestRpc::new(&vm_dir);
            match guest.call("health", &json!({}), None, Duration::from_secs(15)) {
                Ok(health) => {
                    result["disk"] = health.get("disk").cloned().unwrap_or(Value::Null);
                    result["memory"] = health.get("memory").cloned().unwrap_or(Value::Null);
                }
                Err(_) => result["health"] = json!({"state":"unavailable"}),
            }
        }
        result
    }

    #[cfg(not(unix))]
    fn linux_vm(&self, _: &Path) -> Value {
        json!({"state":"unavailable","error":"The VM control service requires Unix"})
    }
}

pub(crate) async fn report(
    store: Arc<RuntimeStore>,
    actor: String,
    epoch: i64,
) -> Result<Value, ManagementError> {
    report_with(store, actor, epoch, Arc::new(ProductionSources)).await
}

async fn report_with(
    store: Arc<RuntimeStore>,
    actor: String,
    epoch: i64,
    sources: Arc<dyn Sources>,
) -> Result<Value, ManagementError> {
    let (lead, team) = store
        .read_async("runtime-services/agent-management/maintenance", move |db| {
            let lead = AgentRepository::get(db, &AgentId::from(actor.as_str()))?
                .ok_or(ManagementError::Invalid("Unknown managed agent"))?;
            let mut statement = db
                .prepare_cached("SELECT id FROM runtime_agents WHERE root_id=?1 ORDER BY id")
                .map_err(studio_runtime_store::StoreError::from)?;
            let ids = statement
                .query_map([&lead.root_id.0], |row| row.get::<_, String>(0))
                .and_then(Iterator::collect::<Result<BTreeSet<_>, _>>)
                .map_err(studio_runtime_store::StoreError::from)?;
            let agents = studio_runtime_store::agent_management::read_agents(db, &ids)?;
            ManagementSnapshot::from_records(Records {
                tables: BTreeMap::from([("runtime_agents".into(), agents)]),
            })?
            .maintenance_team(&actor, epoch)
        })
        .await?;
    let state_dir = store.state_dir().to_path_buf();
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/agent-management/maintenance",
        move || collect(&lead, &team, &state_dir, sources.as_ref()),
    )
    .await
    .map_err(|_| ManagementError::Invalid("Workspace maintenance report is unavailable"))
}

fn collect(lead: &Value, team: &[Value], state_dir: &Path, sources: &dyn Sources) -> Value {
    let mut worktrees = Vec::new();
    for agent in team {
        if let Some(row) = crate::workspace_cleanup_runtime::maintenance_worktree(agent, state_dir)
        {
            worktrees.push(row);
        }
    }
    let mut result = json!({"worktrees":worktrees,"workspaces":[],"bases":[]});
    let by_id = team
        .iter()
        .filter_map(|agent| agent["id"].as_str().map(|id| (id, agent)))
        .collect::<BTreeMap<_, _>>();
    let mut repos = team
        .iter()
        .filter(|agent| agent["environment"] != "linux" && agent["executionMode"] != "vm")
        .filter_map(|agent| agent["imageWorkspaceRepo"].as_str())
        .collect::<BTreeSet<_>>();
    if let Some(repo) = lead["imageWorkspaceBaseRepo"].as_str() {
        repos.insert(repo);
    }
    if !repos.is_empty() || team.iter().any(crate::legacy_image_workspace::is_legacy) {
        match sources.workspaces() {
            Ok(rows) => {
                result["workspaces"] = json!(rows.iter().filter_map(|workspace| {
                    let id = workspace.get("agentId").or_else(|| workspace.get("agent_id")).or_else(|| workspace.get("id"))?.as_str()?;
                    let agent = by_id.get(id)?;
                    (studio_runtime_store::truthy(agent.get("deletedAt")) || agent["imageWorkspaceReady"] != true)
                        .then(|| json!({"id":id,"state":workspace["state"],"path":workspace["mount"],"reason":"archived or stale"}))
                }).collect::<Vec<_>>());
            }
            Err(error) => result["workspaceError"] = json!({"state":"unavailable","error":error}),
        }
    }
    result["bases"] = json!(
        repos
            .into_iter()
            .map(|repo| {
                let mut row = sources
                    .base(Path::new(repo))
                    .unwrap_or_else(|error| json!({"state":"unavailable","error":error}));
                row["repo"] = json!(repo);
                row
            })
            .collect::<Vec<_>>()
    );
    if team
        .iter()
        .any(|agent| agent["environment"] == "linux" || agent["executionMode"] == "vm")
    {
        result["linuxVM"] = sources.linux_vm(state_dir);
    }
    result
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeSources {
        calls: Mutex<Vec<String>>,
        fail_images: bool,
    }

    impl Sources for FakeSources {
        fn workspaces(&self) -> Result<Vec<Value>, String> {
            self.calls.lock().expect("calls").push("workspaces".into());
            if self.fail_images {
                return Err("Image workspace state is invalid".into());
            }
            Ok(vec![
                json!({"agentId":"archived","state":"archived","mount":"/owned"}),
                json!({"agentId":"active","state":"ready","mount":"/active"}),
                json!({"agentId":"other-team","state":"archived","mount":"/private"}),
            ])
        }

        fn base(&self, repo: &Path) -> Result<Value, String> {
            self.calls
                .lock()
                .expect("calls")
                .push(repo.display().to_string());
            if self.fail_images {
                return Err("Image workspace state is invalid".into());
            }
            Ok(json!({"state":"ready","version":"base-version","error":null}))
        }

        fn linux_vm(&self, _: &Path) -> Value {
            self.calls.lock().expect("calls").push("linuxVM".into());
            json!({"state":"stopped","allocatedDiskBytes":42,"memoryLimitBytes":64})
        }
    }

    #[test]
    fn maintenance_report_filters_image_rows_and_reports_vm_resources_without_changes() {
        let root = tempfile::tempdir().expect("state directory");
        let lead = json!({"id":"lead","imageWorkspaceBaseRepo":"/base"});
        let team = vec![
            json!({"id":"archived","deletedAt":1,"imageWorkspaceReady":false,"imageWorkspaceRepo":"/repo"}),
            json!({"id":"active","imageWorkspaceReady":true,"imageWorkspaceRepo":"/repo"}),
            json!({"id":"vm","executionMode":"vm","imageWorkspaceRepo":"/vm-repo"}),
        ];
        let before = team.clone();
        let sources = FakeSources::default();
        let report = collect(&lead, &team, root.path(), &sources);
        assert_eq!(
            report["workspaces"],
            json!([{"id":"archived","state":"archived","path":"/owned","reason":"archived or stale"}])
        );
        assert_eq!(report["bases"].as_array().expect("bases").len(), 2);
        assert_eq!(report["linuxVM"]["allocatedDiskBytes"], 42);
        assert_eq!(team, before);
        assert_eq!(
            *sources.calls.lock().expect("calls"),
            vec!["workspaces", "/base", "/repo", "linuxVM"]
        );
    }

    #[test]
    fn maintenance_report_keeps_image_read_failures_visible() {
        let root = tempfile::tempdir().expect("state directory");
        let sources = FakeSources {
            fail_images: true,
            ..Default::default()
        };
        let report = collect(
            &json!({"imageWorkspaceBaseRepo":"/base"}),
            &[],
            root.path(),
            &sources,
        );
        assert_eq!(report["workspaceError"]["state"], "unavailable");
        assert_eq!(report["bases"][0]["state"], "unavailable");
        assert_eq!(report["bases"][0]["repo"], "/base");
    }

    async fn scoped_fixture() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let root = tempfile::tempdir().expect("state directory");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        store.transaction(|tx| {
            for record in [
                json!({"id":"lead","rootId":"lead","epoch":1,"status":"running","autoWake":true,"isLead":true}),
                json!({"id":"worker","rootId":"lead","parentId":"lead","epoch":1,"status":"completed","autoWake":true,"isLead":false}),
                json!({"id":"other-lead","rootId":"other-lead","epoch":1,"status":"running","autoWake":true,"isLead":true,"imageWorkspaceBaseRepo":"/private"}),
            ] {
                AgentRepository::put(tx, &studio_runtime_store::AgentRecord::from_value(record)?)?;
            }
            Ok(())
        }).await.expect("seed");
        (root, store)
    }

    #[tokio::test]
    async fn maintenance_report_refuses_workers_stale_epochs_and_stopped_leads_before_external_reads()
     {
        let (_root, store) = scoped_fixture().await;
        let sources = Arc::new(FakeSources::default());
        for (actor, epoch) in [("worker", 1), ("lead", 2)] {
            assert!(
                report_with(store.clone(), actor.into(), epoch, sources.clone())
                    .await
                    .is_err()
            );
        }
        store
            .transaction(|tx| {
                let mut lead = AgentRepository::get(tx, &AgentId::from("lead"))?.expect("lead");
                lead.extra.insert("autoWake".into(), json!(false));
                AgentRepository::put(tx, &lead)
            })
            .await
            .expect("stop lead");
        assert!(
            report_with(store, "lead".into(), 1, sources.clone())
                .await
                .is_err()
        );
        assert!(sources.calls.lock().expect("calls").is_empty());
    }

    #[tokio::test]
    async fn maintenance_report_does_not_read_another_teams_bases_or_change_agent_rows() {
        let (_root, store) = scoped_fixture().await;
        let sources = Arc::new(FakeSources::default());
        let before = store
            .read_async("maintenance snapshot", |db| {
                studio_runtime_store::system_reads::all(db, "runtime_agents")
            })
            .await
            .expect("before");
        let report = report_with(store.clone(), "lead".into(), 1, sources.clone())
            .await
            .expect("report");
        assert_eq!(report, json!({"worktrees":[],"workspaces":[],"bases":[]}));
        let after = store
            .read_async("maintenance snapshot", |db| {
                studio_runtime_store::system_reads::all(db, "runtime_agents")
            })
            .await
            .expect("after");
        assert_eq!(after, before);
        assert!(sources.calls.lock().expect("calls").is_empty());
    }

    #[test]
    fn maintenance_report_preserves_unverified_files_and_reports_missing_worktree_roots() {
        let root = tempfile::tempdir().expect("state directory");
        let worktree = root.path().join(".worktrees/codex-agents/worker");
        std::fs::create_dir_all(&worktree).expect("worker directory");
        std::fs::write(worktree.join("keep.txt"), "owned data").expect("file");
        let missing = root.path().join(".worktrees/codex-agents/missing");
        let team = vec![
            json!({"id":"worker","cwd":worktree,"worktreeReady":true,"deletedAt":1}),
            json!({"id":"missing","cwd":missing,"worktreeReady":true}),
        ];
        let report = collect(&json!({}), &team, root.path(), &FakeSources::default());
        assert_eq!(report["worktrees"].as_array().expect("worktrees").len(), 2);
        assert_eq!(report["worktrees"][0]["folderMissing"], false);
        assert_eq!(report["worktrees"][1]["folderMissing"], true);
        assert_eq!(
            std::fs::read_to_string(worktree.join("keep.txt")).expect("preserved file"),
            "owned data"
        );
    }
}
