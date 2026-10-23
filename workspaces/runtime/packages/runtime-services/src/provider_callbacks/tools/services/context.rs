//! `orchestration_context` topics, as Python's `EfficiencyMixin.model_context`
//! reports them. The managed tool dispatcher answers `plan`, and the context
//! port answers `monitor`; this module answers the other topics.
use super::*;
use crate::account_worker::{WorkerError, preparation::PreparationResources};
use crate::managed_host_tools::{Actor, Error};
use studio_runtime_schema::shared_values::PROGRESS_FILE_RETIRED_NOTICE;

pub(super) fn topic(
    tx: &Transaction<'_>,
    actor: &Actor,
    topic: &str,
    preparation: Option<&PreparationResources>,
) -> Result<Value, Error> {
    let record = actor.record().value();
    let lead = studio_runtime_store::truthy(record.get("isLead"));
    match topic {
        "complaints" => Ok(Value::Array(
            records::all(tx, "runtime_complaints")?
                .into_iter()
                .filter(|complaint| {
                    complaint["leadId"] == actor.record().root_id.0.as_str()
                        && complaint["recipient"] == "lead"
                        && crate::wakeups::needs_response(complaint)
                })
                .collect(),
        )),
        "profiles" => {
            let profiles = serde_json::to_value(studio_runtime_store::ProfileRepository::list(tx)?)
                .map_err(|_| Error::Invalid("Stored profiles are unreadable"))?;
            Ok(json!({ "profiles": profiles }))
        }
        // The tool schema of existing threads still names this topic. It now
        // only says that Studio no longer uses the progress file.
        "panel" if lead => Ok(json!(PROGRESS_FILE_RETIRED_NOTICE)),
        "background" if lead => Ok(json!({
            "monitor": "Use orchestration_monitor for commands needing a result. Every command exit wakes you regardless of wake_on. Quiet monitors wake after 1800 seconds by default; set stall_timeout_seconds to 0 to disable stall wakes.",
            "polling": "Let a monitor or a script report changed facts. Do not poll unchanged status through model calls.",
        })),
        "tools" => crate::account_worker::preparation::actor_tool_definitions(
            tx,
            resources(preparation)?,
            &record,
        )
        .map_err(worker_error),
        _ => Err(Error::Invalid("Unknown context topic")),
    }
}

fn resources(preparation: Option<&PreparationResources>) -> Result<&PreparationResources, Error> {
    preparation.ok_or(Error::Unavailable("model-context/preparation-resources"))
}

fn worker_error(error: WorkerError) -> Error {
    match error {
        WorkerError::Store(error) => Error::Store(error),
        _ => Error::Invalid("Studio context is unavailable for this agent"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    /// Python `EfficiencyMixin.model_context` answered the `tools` topic of a
    /// VM agent with `Runtime.tool_definitions`; Rust answered that the VM
    /// catalog was unavailable.
    #[tokio::test]
    async fn the_tools_topic_lists_the_tools_of_a_vm_agent() {
        let directory = tempfile::tempdir().unwrap();
        let store = studio_runtime_store::RuntimeStore::open(Some(directory.path()))
            .await
            .unwrap();
        let resources = PreparationResources::bundled(
            String::new(),
            crate::model_catalog::ModelCatalogService::new(),
        )
        .unwrap();
        store
            .transaction_with(move |tx| {
                let agent = |id: &str, role: &str, thread: &str| {
                    json!({"id":id, "rootId":"lead", "parentId":(id != "lead").then_some("lead"),
                        "epoch":1, "status":"running", "isLead":id == "lead", "name":id,
                        "role":role, "provider":"codex", "accountKey":"default",
                        "executionMode":"vm", "cwd":"/line", "threadId":thread})
                };
                let lead = agent("lead", "orchestrator", "thread-0");
                let reviewer = agent("reviewer", "reviewer", "thread-1");
                for record in [&lead, &reviewer] {
                    studio_runtime_store::AgentRepository::put(
                        tx,
                        &studio_runtime_store::AgentRecord::from_value(record.clone()).unwrap(),
                    )?;
                }
                for (record, vm_tool) in [(&lead, "host_exec"), (&reviewer, "layr_read")] {
                    let actor =
                        Actor::from_record(record, record["id"].as_str().unwrap(), 1).unwrap();
                    let tools = topic(tx, &actor, "tools", Some(&resources)).expect("VM tools");
                    let names = tools
                        .as_array()
                        .unwrap()
                        .iter()
                        .map(|tool| tool["name"].as_str().unwrap().to_owned())
                        .collect::<Vec<_>>();
                    assert_eq!(names.last().map(String::as_str), Some(vm_tool), "{names:?}");
                    assert!(names.iter().any(|name| name == "orchestration_sign_in"));
                }
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .unwrap();
    }
}
