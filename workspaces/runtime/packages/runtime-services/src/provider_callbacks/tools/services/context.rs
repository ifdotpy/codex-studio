//! `orchestration_context` topics, as Python's `EfficiencyMixin.model_context`
//! reports them. The managed tool dispatcher answers `plan`, and the context
//! port answers `monitor`; this module answers the other topics.
use super::*;
use crate::account_worker::{WorkerError, preparation::PreparationResources};
use crate::managed_host_tools::{Actor, Error};
use std::path::Path;
use studio_runtime_schema::shared_values::PROGRESS_FILE_LIMIT_LABEL;

/// The skill file whose text the `panel` topic returns, relative to the skills root.
pub(super) const PANEL_GUIDANCE: &str = "codex-workspace/references/panel.md";

pub(super) fn topic(
    tx: &Transaction<'_>,
    actor: &Actor,
    topic: &str,
    state_root: &Path,
    preparation: Option<&PreparationResources>,
    panel_guidance: Option<&PanelGuidance>,
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
                panel_guidance
                    .ok_or(Error::Unavailable("model-context/prepared-panel-guidance"))?
                    .text()?
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

/// One file snapshot per panel callback, prepared before entering SQL.
pub(crate) struct PanelGuidance(Result<String, PanelGuidanceError>);
enum PanelGuidanceError {
    Resources,
    Missing,
    Empty,
}
impl PanelGuidance {
    pub(crate) fn read(resources: Option<&PreparationResources>) -> Self {
        studio_runtime_store::debug_assert_outside_sql();
        let Some(resources) = resources else {
            return Self(Err(PanelGuidanceError::Resources));
        };
        let guide = resources.skills_root.join(PANEL_GUIDANCE);
        let content = match std::fs::read_to_string(&guide) {
            Ok(content) => content,
            Err(_) => return Self(Err(PanelGuidanceError::Missing)),
        };
        let content = content.trim();
        if content.is_empty() {
            return Self(Err(PanelGuidanceError::Empty));
        }
        Self(Ok(format!(
            "[Studio panel guidance: {}]\n{content}",
            guide.display()
        )))
    }
    fn text(&self) -> Result<&str, Error> {
        self.0.as_deref().map_err(|error| match error {
            PanelGuidanceError::Resources => {
                Error::Unavailable("model-context/preparation-resources")
            }
            PanelGuidanceError::Missing => Error::Invalid(
                "Studio panel guidance is missing. Update the installed Studio workspace",
            ),
            PanelGuidanceError::Empty => Error::Invalid(
                "Studio panel guidance is empty. Update the installed Studio workspace",
            ),
        })
    }
}

fn worker_error(error: WorkerError) -> Error {
    match error {
        WorkerError::Store(error) => Error::Store(error),
        _ => Error::Invalid("Studio context is unavailable for this agent"),
    }
}
