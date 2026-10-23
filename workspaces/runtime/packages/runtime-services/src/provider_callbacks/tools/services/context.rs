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
        .map_err(worker_error)?
        .ok_or(Error::Unavailable("model-context/vm-tool-catalog")),
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
