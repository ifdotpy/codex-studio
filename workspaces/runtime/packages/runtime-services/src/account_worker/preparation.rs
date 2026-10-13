use super::*;
use crate::notification_dispatch::{PreparedSubmission, SubmittedInput};
use studio_runtime_store::AgentRecord;

/// Start-time preparation boundary. Dynamic context is deliberately not ported;
/// this pass-through worker is not eligible for wiring to real providers yet.
pub struct PreparationInput<'a> {
    pub intent: &'a TurnIntent,
    pub agent: &'a AgentRecord,
    pub state_root: &'a std::path::Path,
    pub submitted_at: f64,
}
pub struct PreparedTurn {
    pub params: Value,
    pub submission: PreparedSubmission,
}
pub fn prepare_turn(input: PreparationInput<'_>) -> Result<PreparedTurn, String> {
    let intent = input.intent;
    let params = provider_turn_params(input.state_root, intent, input.agent)?;
    let first = intent.events.first().ok_or("submission has no input")?;
    let submission = PreparedSubmission {
        first_event_id: first.id.clone(),
        submitted_at: input.submitted_at,
        created_at: first.created,
        text: params["input"][0]["text"]
            .as_str()
            .unwrap_or_default()
            .into(),
        inputs: intent
            .events
            .iter()
            .map(|e| SubmittedInput {
                id: e.id.clone(),
                at: e.created,
                kind: e.kind.clone(),
                text: e.text.clone(),
                assets: e.metadata.get("assets").cloned().unwrap_or(json!([])),
            })
            .collect(),
        assets: json!([]),
    };
    Ok(PreparedTurn { params, submission })
}

fn turn_start_params(intent: &TurnIntent) -> Value {
    let text = intent
        .events
        .iter()
        .map(|event| event.text.as_str())
        .collect::<Vec<_>>()
        .join("\n\n");
    let mut params = json!({
        "threadId": intent.thread_id,
        "cwd": intent.cwd,
        "clientUserMessageId": intent.events.first().map(|event| event.id.as_str()),
        "input": [{"type":"text", "text":text}]
    });
    if let Some(model) = &intent.model {
        params["model"] = Value::String(model.clone());
    }
    params
}

fn provider_turn_params(
    state_root: &std::path::Path,
    intent: &TurnIntent,
    agent: &AgentRecord,
) -> Result<Value, String> {
    if intent.provider != "codex" {
        return Err("This Rust turn driver currently supports the Codex provider only.".into());
    }
    if intent.thread_id.as_deref().is_none_or(str::is_empty)
        || intent.cwd.as_deref().is_none_or(str::is_empty)
        || intent.events.is_empty()
        || intent.events.iter().any(|event| {
            event
                .metadata
                .get("assets")
                .is_some_and(|v| v.as_array().is_none_or(|v| !v.is_empty()))
        })
    {
        return Err("This Rust turn driver requires an existing text-only provider thread.".into());
    }
    if agent.extra.get("pendingSettings").is_some_and(|value| {
        !value.is_null() && value.as_object().is_none_or(|object| !object.is_empty())
    }) {
        return Err(
            "pendingSettings are not supported by the Rust turn driver; apply settings first."
                .into(),
        );
    }
    if agent.extra.get("workspaceReady").and_then(Value::as_bool) == Some(false)
        || (agent.extra.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
            && agent
                .extra
                .get("imageWorkspaceReady")
                .and_then(Value::as_bool)
                != Some(true))
        || (agent.extra.get("executionMode").and_then(Value::as_str) == Some("vm")
            && agent.extra.get("layrReady").and_then(Value::as_bool) == Some(false))
    {
        return Err(
            "workspace is not ready; prepare the workspace before submitting input.".into(),
        );
    }
    let mut params = turn_start_params(intent);
    if let Some(effort) = agent
        .extra
        .get("nativeEffort")
        .or_else(|| agent.extra.get("effort"))
        .filter(|value| !value.is_null())
    {
        params["effort"] = effort.clone();
    }
    params["serviceTier"] = Value::String(
        if agent.extra.get("fastMode").and_then(Value::as_bool) == Some(true) {
            "priority"
        } else {
            "default"
        }
        .into(),
    );
    let program = if agent.extra.get("daybreakEnabled").and_then(Value::as_bool) == Some(true) {
        agent
            .extra
            .get("cyberAccessProgram")
            .and_then(Value::as_str)
            .filter(|value| matches!(*value, "daybreakBlue" | "daybreakRed"))
            .ok_or_else(|| {
                "selected Daybreak access program is unavailable; input remains pending.".to_owned()
            })?
    } else {
        "standard"
    };
    params["cyberAccessProgram"] = Value::String(program.to_owned());
    if agent.extra.get("executionMode").and_then(Value::as_str) == Some("vm")
        && agent.extra.get("role").and_then(Value::as_str) == Some("reviewer")
    {
        params["approvalPolicy"] = json!("never");
        params["sandboxPolicy"] = json!({"type":"readOnly"});
    } else if let Some(yolo) = agent.extra.get("yoloMode").filter(|value| !value.is_null()) {
        let Some(yolo) = yolo.as_bool() else {
            return Err(
                "permission settings cannot be applied because yoloMode is invalid.".into(),
            );
        };
        params["approvalPolicy"] = Value::String(if yolo { "never" } else { "on-request" }.into());
        if yolo {
            params["sandboxPolicy"] = json!({"type":"dangerFullAccess"});
        } else if agent.extra.get("role").and_then(Value::as_str) == Some("reviewer") {
            params["sandboxPolicy"] = json!({"type":"readOnly"});
        } else {
            let Some(cwd) = intent.cwd.as_deref().filter(|cwd| !cwd.is_empty()) else {
                return Err(
                    "workspace path is missing; permission settings cannot be applied.".into(),
                );
            };
            let mut roots = vec![cwd.to_owned()];
            if agent.extra.get("isLead").and_then(Value::as_bool) == Some(true) {
                let progress =
                    if agent.extra.get("executionMode").and_then(Value::as_str) == Some("vm") {
                        let home = agent
                            .extra
                            .get("layrHome")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                "VM lead progress path is unavailable; input remains pending."
                                    .to_owned()
                            })?;
                        std::path::PathBuf::from(home)
                            .join(".studio/progress")
                            .join(&intent.agent_id)
                    } else {
                        state_root.join("progress").join(&intent.agent_id)
                    };
                // Provisioning is owned by the workspace service; do not grant symlink access.
                if progress.is_dir()
                    && std::fs::canonicalize(&progress).ok().as_ref() == Some(&progress)
                {
                    roots.push(progress.to_string_lossy().into_owned());
                } else {
                    return Err("Lead progress directory requires workspace provisioning; input remains pending.".into());
                }
            }
            params["sandboxPolicy"] =
                json!({"type":"workspaceWrite","writableRoots":roots,"networkAccess":false});
        }
    }
    Ok(params)
}
