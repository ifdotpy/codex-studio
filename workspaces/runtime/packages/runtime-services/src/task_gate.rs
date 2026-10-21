//! Task gates use monitor admission, execution, output bounds, and approvals.
//! Submission and monitor admission commit together. A restart never certifies a gate.
use crate::event_write::{self, EventWriteError};
use crate::monitor_policy::MonitorError;
use crate::monitor_runtime::{MonitorAdmission, WorkspaceAdmission};
use serde_json::{Value, json};
use std::path::Path;
use studio_runtime_store::{AgentRecord, AgentRepository, Transaction, WorkItemsRepository};

pub fn normalize(value: &Value) -> Result<Value, EventWriteError> {
    let gate =
        studio_runtime_schema::task_gate::parse(value).map_err(EventWriteError::InvalidRequest)?;
    serde_json::to_value(gate)
        .map_err(studio_runtime_store::StoreError::from)
        .map_err(Into::into)
}

pub fn set_default(
    tx: &Transaction<'_>,
    actor_id: &str,
    gate: &Value,
    request_id: Option<&str>,
    signature: &str,
) -> Result<Value, EventWriteError> {
    let mut actor = event_write::checked_agent(tx, actor_id)?;
    if actor.id != actor.root_id {
        return Err(EventWriteError::InvalidRequest(
            "Only the lead can set the default acceptance gate",
        ));
    }
    let gate = normalize(gate)?;
    if let Some(saved) =
        event_write::read_receipt(tx, request_id, signature, "runtime_agents", actor_id, |v| {
            v["id"] == actor_id && v.get("defaultGate").is_some()
        })?
    {
        return Ok(saved);
    }
    actor.extra.insert("taskDefaultGate".into(), gate.clone());
    AgentRepository::put(tx, &actor)?;
    let receipt = json!({"id":actor_id,"defaultGate":gate});
    event_write::save_receipt(tx, request_id, signature, &receipt)?;
    Ok(receipt)
}

/// The guard and command both execute through the managed monitor sandbox.
/// The guard refuses another commit or local source changes, before and after the check.
fn revision_command(command: &str, revision: &str) -> Option<String> {
    if !matches!(revision.len(), 40 | 64) || !revision.bytes().all(|v| v.is_ascii_hexdigit()) {
        return None;
    }
    let quoted = format!("'{}'", command.replace('\'', "'\\''"));
    let guard = format!(
        "studio_gate_verify() {{ studio_gate_head=$(git rev-parse --verify HEAD) && test \"$studio_gate_head\" = '{revision}' && studio_gate_tree=$(git status --porcelain --untracked-files=normal) && test -z \"$studio_gate_tree\"; }}"
    );
    Some(format!(
        "{guard}; studio_gate_verify || {{ echo 'Acceptance gate: submitted revision or clean workspace check failed' >&2; exit 1; }}; sh -c {quoted}; studio_gate_code=$?; studio_gate_verify || {{ echo 'Acceptance gate: workspace changed during the check' >&2; exit 1; }}; exit \"$studio_gate_code\""
    ))
}

pub fn admit(
    tx: &Transaction<'_>,
    submitter: &AgentRecord,
    task: &mut Value,
    workspace: Option<&(dyn WorkspaceAdmission + Send + Sync)>,
    state_dir: &Path,
    now: f64,
) -> Result<bool, MonitorError> {
    let Some(config) = task.get("gate").filter(|v| !v.is_null()).cloned() else {
        return Ok(false);
    };
    let config = normalize(&config)?;
    let result = task["results"]
        .as_array_mut()
        .and_then(|v| v.last_mut())
        .ok_or(MonitorError::Refused("Missing gate submission"))?;
    let result_id = result["id"]
        .as_str()
        .ok_or(MonitorError::Refused("Missing result identity"))?
        .to_owned();
    let revision = result["revision"].as_str().unwrap_or_default();
    let cwd = submitter.extra.get("cwd").cloned().unwrap_or(Value::Null);
    let mut evidence = json!({"status":"not_run","config":config,"cwd":cwd,"revision":revision,
        "exitCode":null,"output":"","log":null,"monitorId":null});
    // Never execute a command against an unverified revision. Resubmit with a full Git commit.
    let command = revision_command(config["command"].as_str().unwrap_or_default(), revision);
    if command.is_none() || result.get("layrStateId").is_some() {
        evidence["error"] =
            json!("Acceptance gates require a Git workspace and its full commit ID");
    } else if let Some(workspace) = workspace {
        let request_id = format!("task-gate:{result_id}");
        let request = json!({"command":command,"timeout_ms":config["timeout_ms"],"stall_timeout_seconds":0,"success_exit_codes":[0]});
        let log_root = crate::monitor_output::log_root(state_dir)
            .to_string_lossy()
            .into_owned();
        let admission = MonitorAdmission {
            actor_id: &submitter.id.0,
            epoch: submitter.epoch,
            request_id: &request_id,
            request: &request,
            log_root: &log_root,
        };
        let mut monitor =
            crate::monitor_runtime::create_in_transaction(tx, &admission, workspace, now)?;
        monitor["taskGate"] = json!({"taskId":task["id"],"resultId":result_id});
        studio_runtime_store::monitor_runtime::put(tx, monitor.clone())?;
        evidence["status"] = json!("running");
        evidence["monitorId"] = monitor["id"].clone();
        evidence["log"] = monitor["log"].clone();
    } else {
        evidence["error"] = json!("Acceptance gate workspace was not prepared; submit again");
    }
    task["results"].as_array_mut().unwrap().last_mut().unwrap()["gate"] = evidence;
    Ok(true)
}

pub fn require_pass(task: &Value, override_reason: Option<&str>) -> Result<(), EventWriteError> {
    if let Some(reason) = override_reason {
        if reason.trim().is_empty()
            || reason.chars().count() > studio_runtime_schema::task_gate::MAX_TEXT_CODEPOINTS
        {
            return Err(EventWriteError::InvalidRequest(
                "Supply a nonempty gate override reason",
            ));
        }
        return Ok(());
    }
    let Some(config) = task.get("gate").filter(|v| !v.is_null()) else {
        return Ok(());
    };
    let gate = task["results"]
        .as_array()
        .and_then(|v| v.last())
        .and_then(|v| v.get("gate"));
    if gate.is_some_and(|gate| {
        gate["status"] == "passed"
            && gate["exitCode"] == 0
            && gate["config"] == *config
            && gate["monitorId"].is_string()
    }) {
        return Ok(());
    }
    Err(EventWriteError::Conflict(
        "The acceptance gate has not passed. Resubmit to run it, or supply override_reason",
    ))
}

pub fn notify_review(
    tx: &Transaction<'_>,
    task: &Value,
    result: &Value,
    now: f64,
) -> Result<bool, MonitorError> {
    let root = task["rootId"]
        .as_str()
        .ok_or(MonitorError::Refused("Missing task lead"))?;
    let lead = event_write::checked_agent(tx, root)?;
    Ok(event_write::enqueue_recovery_event(
        tx,
        &lead,
        format!("work-result:{}", result["id"].as_str().unwrap_or_default()),
        "work_review",
        json!({"task":task["id"],"title":task["title"],"result":result}).to_string(),
        now,
    )?)
}

/// Return true for a gate monitor, including stale deliveries. A stale success cannot replace unknown.
pub fn settle(tx: &Transaction<'_>, monitor: &Value, now: f64) -> Result<bool, MonitorError> {
    let Some(binding) = monitor.get("taskGate") else {
        return Ok(false);
    };
    let Some(task_id) = binding["taskId"].as_str() else {
        return Err(MonitorError::Refused("Invalid gate task identity"));
    };
    let Some(mut task) = WorkItemsRepository::work_record(tx, task_id)? else {
        return Ok(true);
    };
    let Some(result) = task["results"].as_array_mut().and_then(|v| v.last_mut()) else {
        return Ok(true);
    };
    if result["id"] != binding["resultId"]
        || result["gate"]["monitorId"] != monitor["id"]
        || result["gate"]["status"] != "running"
    {
        return Ok(true);
    }
    let status = match monitor["status"].as_str() {
        Some("completed") if monitor["exitCode"] == 0 => "passed",
        Some("lost") => "unknown",
        Some("failed" | "cancelled" | "completed") => "failed",
        _ => return Ok(true),
    };
    result["gate"]["status"] = json!(status);
    result["gate"]["exitCode"] = monitor["exitCode"].clone();
    result["gate"]["output"] = json!(
        monitor["tail"]
            .as_str()
            .unwrap_or_default()
            .chars()
            .take(studio_runtime_schema::shared_values::MONITOR_OUTPUT_TAIL_CODEPOINTS)
            .collect::<String>()
    );
    result["gate"]["log"] = monitor["log"].clone();
    result["gate"]["error"] = monitor["error"].clone();
    result["gate"]["finished"] = json!(now);
    let result = result.clone();
    task["version"] = json!(task["version"].as_u64().unwrap_or_default() + 1);
    task["updated"] = json!(now);
    WorkItemsRepository::update_work(tx, task_id, &task)?;
    if task["status"] == "review" {
        notify_review(tx, &task, &result, now)?;
    }
    Ok(true)
}

pub fn recover(tx: &Transaction<'_>, now: f64) -> Result<(), MonitorError> {
    for row in WorkItemsRepository::work(tx)? {
        let Some(mut task) = WorkItemsRepository::work_record(tx, &row.id)? else {
            continue;
        };
        let Some(result) = task["results"].as_array_mut().and_then(|v| v.last_mut()) else {
            continue;
        };
        if result["gate"]["status"] != "running" {
            continue;
        }
        result["gate"]["status"] = json!("unknown");
        result["gate"]["error"] = json!(
            "Server restarted before the gate result was recorded. Submit again to rerun the gate"
        );
        result["gate"]["finished"] = json!(now);
        let result = result.clone();
        task["version"] = json!(task["version"].as_u64().unwrap_or_default() + 1);
        task["updated"] = json!(now);
        WorkItemsRepository::update_work(tx, &row.id, &task)?;
        if task["status"] == "review" {
            notify_review(tx, &task, &result, now)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::work_write::{self, AcceptWork, SubmitWork, WorkWrite, WorkWriteError};
    use std::sync::Arc;
    use studio_runtime_store::{MonitorsRepository, RuntimeStore};

    #[test]
    fn acceptance_gate_command_checks_revision_source_changes_and_exit_code() {
        use crate::bounded_child::{Request, run_without_git_environment};
        use std::ffi::OsStr;
        let root = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            run_without_git_environment(Request {
                program: OsStr::new("git"),
                git_directory: Some(root.path().as_os_str()),
                args,
                environment: &[],
                input: None,
                stdout_limit: 4096,
                stderr_limit: 4096,
                timeout: std::time::Duration::from_secs(5),
            })
            .unwrap()
        };
        git(&["init", "-q"]);
        git(&[
            "-c",
            "user.name=Gate Test",
            "-c",
            "user.email=gate@example.invalid",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "--allow-empty",
            "-qm",
            "fixture",
        ]);
        let revision = String::from_utf8(git(&["rev-parse", "HEAD"]).stdout.bytes)
            .unwrap()
            .trim()
            .to_owned();
        let run = |command: &str, revision: &str| {
            let wrapped = revision_command(command, revision).unwrap();
            let script = format!("cd '{}' && {}", root.path().display(), wrapped);
            run_without_git_environment(Request {
                program: OsStr::new("sh"),
                git_directory: None,
                args: &["-c", &script],
                environment: &[],
                input: None,
                stdout_limit: 4096,
                stderr_limit: 4096,
                timeout: std::time::Duration::from_secs(5),
            })
        };
        assert!(run("printf 'verified'", &revision).is_ok());
        assert!(run("exit 7", &revision).is_err());
        assert!(run("true", REV).is_err());
        assert!(run("touch changed-source", &revision).is_err());
        assert!(run("true", &revision).is_err());
    }

    struct Workspace;
    impl WorkspaceAdmission for Workspace {
        fn check(&self, _: &Transaction<'_>, _: &AgentRecord) -> Result<(), MonitorError> {
            Ok(())
        }
    }
    async fn fixture(gate: Value, approved: bool) -> (Arc<RuntimeStore>, tempfile::TempDir) {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        let cwd = root.path().to_string_lossy().into_owned();
        store.transaction(move |tx| {
            for id in ["lead", "worker", "dependent"] {
                AgentRepository::put(tx, &AgentRecord::from_value(json!({"id":id,"rootId":"lead","isLead":id=="lead",
                    "epoch":1,"status":"running","autoWake":true,"agentMode":"multi","concurrency":8,"cwd":cwd,"yoloMode":approved}))?)?;
            }
            for (id, owner, deps) in [("task","worker",json!([])),("next","dependent",json!(["task"]))] {
                WorkItemsRepository::put_work(tx, id, &json!({"id":id,"rootId":"lead","title":id,"description":"",
                    "owner":owner,"dependencies":deps,"status":"running","version":1,"created":1.0,"updated":1.0,
                    "results":[],"decisions":[],"gate":gate}))?;
            }
            Ok(())
        }).await.unwrap();
        (store, root)
    }
    async fn submit(store: &Arc<RuntimeStore>, request: &str, revision: &str) -> Value {
        let state_dir = store.root().to_owned();
        let request = request.to_owned();
        let revision = revision.to_owned();
        store
            .transaction_with(move |tx| {
                work_write::write_action_as_agent(
                    tx,
                    "worker",
                    "worker",
                    Some(&request),
                    &request,
                    WorkWrite::Submit(SubmitWork {
                        task_id: "task".into(),
                        version: None,
                        result: "done".into(),
                        checks: "reported".into(),
                        revision,
                        files: vec![],
                        prepared_files: None,
                        prepared_gate_workspace: Some(Arc::new(Workspace)),
                        state_dir,
                        vm_evidence: None,
                        vm_evidence_request_id: None,
                    }),
                )
            })
            .await
            .unwrap()
            .record
    }
    async fn accept(
        store: &Arc<RuntimeStore>,
        reason: Option<&str>,
    ) -> Result<Value, WorkWriteError> {
        let reason = reason.map(str::to_owned);
        store
            .transaction_with(move |tx| {
                work_write::write_action_as_agent(
                    tx,
                    "lead",
                    "lead",
                    Some("accept"),
                    "accept",
                    WorkWrite::Accept(AcceptWork {
                        task_id: "task".into(),
                        version: None,
                        reason: "reviewed".into(),
                        merged_result_id: None,
                        override_reason: reason,
                    }),
                )
            })
            .await
            .map(|v| v.record)
    }
    async fn finish(store: &Arc<RuntimeStore>, record: &Value, code: i64) {
        let id = record["results"].as_array().unwrap().last().unwrap()["gate"]["monitorId"]
            .as_str()
            .unwrap()
            .to_owned();
        store
            .transaction_with(move |tx| {
                crate::monitor_runtime::finish_in_transaction(tx, &id, None, Some(code), None, 2.0)
            })
            .await
            .unwrap();
    }
    async fn events(store: &Arc<RuntimeStore>, kind: &str) -> Vec<Value> {
        let kind = kind.to_owned();
        store
            .read_async("gate-test/events", move |tx| {
                let mut stmt =
                    tx.prepare("SELECT text FROM runtime_events WHERE kind=?1 ORDER BY id")?;
                let rows = stmt
                    .query_map([kind], |r| r.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, studio_runtime_store::StoreError>(
                    rows.into_iter()
                        .map(|r| serde_json::from_str(&r).unwrap())
                        .collect(),
                )
            })
            .await
            .unwrap()
    }
    const REV: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";

    #[tokio::test]
    async fn acceptance_gate_passing_allows_accept_and_publishes_evidence() {
        let (store, _root) = fixture(normalize(&json!("true")).unwrap(), true).await;
        let record = submit(&store, "submit", REV).await;
        assert!(events(&store, "work_review").await.is_empty());
        assert!(accept(&store, None).await.is_err());
        finish(&store, &record, 0).await;
        let events = events(&store, "work_review").await;
        assert_eq!(events.len(), 1);
        assert_eq!(events[0]["result"]["gate"]["exitCode"], 0);
        assert!(
            events[0]["result"]["gate"]["log"]
                .as_str()
                .unwrap()
                .ends_with(".log")
        );
        assert_eq!(accept(&store, None).await.unwrap()["status"], "accepted");
    }
    #[tokio::test]
    async fn acceptance_gate_failure_requires_override_and_releases_dependents_once() {
        let (store, _root) = fixture(normalize(&json!("false")).unwrap(), true).await;
        let record = submit(&store, "submit", REV).await;
        finish(&store, &record, 1).await;
        assert!(accept(&store, None).await.is_err());
        assert!(accept(&store, Some(" ")).await.is_err());
        assert!(events(&store, "work_ready").await.is_empty());
        let accepted = accept(&store, Some("Known fixture failure; reviewed manually"))
            .await
            .unwrap();
        assert_eq!(
            accepted["decisions"][0]["gateOverrideReason"],
            "Known fixture failure; reviewed manually"
        );
        assert_eq!(events(&store, "work_ready").await.len(), 1);
        assert_eq!(
            accept(&store, Some("Known fixture failure; reviewed manually"))
                .await
                .unwrap()["status"],
            "accepted"
        );
        assert_eq!(events(&store, "work_ready").await.len(), 1);
    }
    #[tokio::test]
    async fn acceptance_gate_restart_is_unknown_and_late_success_cannot_pass() {
        let (store, root) = fixture(normalize(&json!("true")).unwrap(), true).await;
        let record = submit(&store, "submit", REV).await;
        drop(store);
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        crate::monitor_runtime::recover_on_startup(&store, 3.0)
            .await
            .unwrap();
        assert!(accept(&store, None).await.is_err());
        let reviews = events(&store, "work_review").await;
        assert_eq!(reviews.len(), 1);
        assert_eq!(reviews[0]["result"]["gate"]["status"], "unknown");
        // A retained provider reply after restart must not certify the interrupted run.
        let id = record["results"][0]["gate"]["monitorId"]
            .as_str()
            .unwrap()
            .to_owned();
        store
            .transaction_with(move |tx| {
                let mut monitor = MonitorsRepository::get(tx, &id)?.unwrap().value().clone();
                monitor["status"] = json!("completed");
                monitor["exitCode"] = json!(0);
                settle(tx, &monitor, 4.0)
            })
            .await
            .unwrap();
        assert!(accept(&store, None).await.is_err());
        let resubmitted = submit(&store, "rerun", REV).await;
        assert_ne!(
            resubmitted["results"][1]["gate"]["monitorId"],
            record["results"][0]["gate"]["monitorId"]
        );
        finish(&store, &resubmitted, 0).await;
        assert_eq!(accept(&store, None).await.unwrap()["status"], "accepted");
    }
    #[tokio::test]
    async fn acceptance_gate_no_gate_preserves_submit_and_accept() {
        let (store, _root) = fixture(Value::Null, true).await;
        let record = submit(&store, "submit", "old-revision-format").await;
        assert!(record["results"][0].get("gate").is_none());
        assert_eq!(events(&store, "work_review").await.len(), 1);
        assert_eq!(accept(&store, None).await.unwrap()["status"], "accepted");
    }
    #[tokio::test]
    async fn acceptance_gate_monitor_retains_approval_and_submission_replay_identity() {
        let (store, _root) = fixture(
            normalize(&json!({"command":"true","timeout_ms":1234})).unwrap(),
            false,
        )
        .await;
        let record = submit(&store, "submit", REV).await;
        assert_eq!(submit(&store, "submit", REV).await, record);
        let id = record["results"][0]["gate"]["monitorId"]
            .as_str()
            .unwrap()
            .to_owned();
        let monitor = store
            .read_async("gate-test/monitor", move |tx| {
                MonitorsRepository::get(tx, &id)
            })
            .await
            .unwrap()
            .unwrap();
        assert_eq!(monitor.value()["status"], "approval");
        assert_eq!(monitor.value()["agent"], "worker");
        assert_eq!(monitor.value()["timeout_ms"], 1234);
        assert!(accept(&store, None).await.is_err());
    }
    #[tokio::test]
    async fn acceptance_gate_unverified_revision_is_not_run() {
        let (store, _root) = fixture(normalize(&json!("true")).unwrap(), true).await;
        let record = submit(&store, "submit", "HEAD; false").await;
        assert_eq!(record["results"][0]["gate"]["status"], "not_run");
        assert!(accept(&store, None).await.is_err());
        assert_eq!(
            events(&store, "work_review").await[0]["result"]["gate"]["status"],
            "not_run"
        );
    }
}
