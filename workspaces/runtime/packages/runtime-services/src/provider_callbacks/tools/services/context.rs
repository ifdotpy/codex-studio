//! `orchestration_context` topics, as Python's `EfficiencyMixin.model_context`
//! reports them. The managed tool dispatcher answers `plan`, and the context
//! port answers `monitor`; this module answers the other topics.
use super::*;
use crate::account_worker::{WorkerError, preparation::PreparationResources};
use crate::managed_host_tools::{Actor, Error};
use std::path::Path;
use studio_runtime_schema::shared_values::PROGRESS_FILE_LIMIT_LABEL;

/// The skill file whose text the `panel` topic returns, relative to the skills root.
const PANEL_GUIDANCE: &str = "codex-workspace/references/panel.md";

pub(super) fn topic(
    tx: &Transaction<'_>,
    actor: &Actor,
    topic: &str,
    state_root: &Path,
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
        "panel" if lead => {
            let resources = resources(preparation)?;
            Ok(json!(format!(
                "{}\n\n{}",
                progress(resources, &record, state_root)?,
                panel_guidance(resources)?
            )))
        }
        "background" if lead => Ok(json!({
            "workflow": progress(resources(preparation)?, &record, state_root)?,
            "monitor": "Use orchestration_monitor for commands needing a result. Every command exit wakes you regardless of wake_on. Quiet monitors wake after 1800 seconds by default; set stall_timeout_seconds to 0 to disable stall wakes.",
            "polling": "Let a script write PROGRESS.md when its facts change. Do not poll unchanged status through model calls.",
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

/// Python's `codex_vm_agents.progress_context`: a VM lead edits its progress
/// file in the guest; a host lead receives its role guidance's progress text.
fn progress(
    resources: &PreparationResources,
    record: &Value,
    state_root: &Path,
) -> Result<String, Error> {
    if record["executionMode"] != "vm" {
        return crate::account_worker::preparation::progress_context(resources, record, state_root)
            .map_err(worker_error);
    }
    let home = record["layrHome"]
        .as_str()
        .unwrap_or(crate::linux_vm_provider::DEFAULT_GUEST_HOME);
    let id = record["id"]
        .as_str()
        .ok_or(Error::Invalid("Invalid callback actor"))?;
    let path = Path::new(home)
        .join(".studio/progress")
        .join(id)
        .join("PROGRESS.md");
    Ok(format!(
        "Your progress file is {}. Read and edit this UTF-8 Markdown file with ordinary file tools. Studio displays it above the composer. Keep it at most {PROGRESS_FILE_LIMIT_LABEL}. Update it when the status changes.",
        path.display()
    ))
}

/// Python's `Runtime.panel_guidance`: the installed skill file, read on each request.
fn panel_guidance(resources: &PreparationResources) -> Result<String, Error> {
    let guide = resources.skills_root.join(PANEL_GUIDANCE);
    let content = std::fs::read_to_string(&guide).map_err(|_| {
        Error::Invalid("Studio panel guidance is missing. Update the installed Studio workspace")
    })?;
    let content = content.trim();
    if content.is_empty() {
        return Err(Error::Invalid(
            "Studio panel guidance is empty. Update the installed Studio workspace",
        ));
    }
    Ok(format!(
        "[Studio panel guidance: {}]\n{content}",
        guide.display()
    ))
}

fn worker_error(error: WorkerError) -> Error {
    match error {
        WorkerError::Store(error) => Error::Store(error),
        _ => Error::Invalid("Studio context is unavailable for this agent"),
    }
}
