//! `layr_read`: one read-only layr command of a VM reviewer, run in the guest
//! as the reviewer's own Linux user (`layr.exec`), so layr applies the
//! reviewer's role and the project's deny rules (docs/vm-layr.md).

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use base64::Engine;
use serde_json::{Value, json};

use crate::linux_vm_guest::{GuestError, GuestRpc};
use crate::managed_host_tools::{Actor, DeliveryCertainty, DispatchedOperation, OperationResult};

/// The tool name.
pub(crate) const TOOL: &str = "layr_read";
/// The dependency that `layr_read` names until the host attaches the guest channel.
pub(crate) const DEPENDENCY: &str = "layr-read/vm-guest";
/// The layr commands that a reviewer may run, after optional `--line <line>` pairs.
pub(crate) const COMMANDS: [&str; 9] = [
    "blame",
    "diff",
    "grep",
    "log",
    "ls-files",
    "merge-base",
    "rev-parse",
    "show",
    "status",
];
/// Largest argument count of one call.
const MAX_ARGUMENTS: usize = 64;
/// Longest argument, in characters.
const MAX_ARGUMENT_CHARS: usize = 4096;
/// Guest request ID prefix of one exact tool request.
const REQUEST_ID_PREFIX: &str = "layr-read:";
/// Guest wait of `layr.exec`, in seconds: the guest command runs for at most 300 seconds.
const GUEST_WAIT_SECONDS: u64 = 330;

const REFUSAL: &str = "layr_read is for VM reviewers";

/// The tool definition that VM reviewers get.
pub(crate) fn tool_definition() -> Option<&'static crate::thread_prepare::ToolDefinition> {
    static DEFINITION: OnceLock<Option<crate::thread_prepare::ToolDefinition>> = OnceLock::new();
    DEFINITION
        .get_or_init(|| {
            serde_json::from_value(json!({
                "type": "function", "name": TOOL,
                "description": format!(
                    "Run one read-only layr command as your Linux user in your read-only line, outside your sandbox: {}. \
                     args is the argument list after `layr`, for example [\"diff\", \"<base>\", \"<state>\"] or \
                     [\"show\", \"<state>:<path>\"]. [\"--line\", \"<line>\", \"log\"] reads another line of your project. \
                     Use head for the current named state. log --json writes JSON Lines; status --json writes one JSON object. \
                     Write notes and copies only in $TMPDIR.",
                    COMMANDS.join(", ")
                ),
                "inputSchema": {"type": "object", "properties": {
                    "args": {"type": "array", "items": {"type": "string"}, "minItems": 1, "maxItems": MAX_ARGUMENTS}},
                    "required": ["args"]},
            }))
            .ok()
        })
        .as_ref()
}

/// The argument list after `layr`, or the refusal text.
pub(crate) fn command(args: &Value) -> Result<Vec<String>, String> {
    let argv: Vec<String> = match args.get("args") {
        Some(Value::Array(items))
            if !items.is_empty()
                && items.len() <= MAX_ARGUMENTS
                && items.iter().all(|item| {
                    item.as_str().is_some_and(|item| {
                        !item.contains('\0') && item.chars().count() <= MAX_ARGUMENT_CHARS
                    })
                }) =>
        {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        }
        _ => return Err(format!("Supply args as 1 to {MAX_ARGUMENTS} strings")),
    };
    let mut index = 0;
    while index < argv.len() && argv[index] == "--line" {
        index += 2;
    }
    if index >= argv.len() || !COMMANDS.contains(&argv[index].as_str()) {
        return Err(format!(
            "layr_read runs only {}, after optional --line <line>",
            COMMANDS.join(", ")
        ));
    }
    Ok(argv)
}

/// The guest request identity of tool request `request_id`: a retry of the
/// same request gets the first answer of the guest, and the command does not
/// run again.
pub(crate) fn identity(actor_id: &str, request_id: &str) -> String {
    format!(
        "{REQUEST_ID_PREFIX}{}",
        studio_runtime_store::sha256_hex(format!("{actor_id}:{request_id}").as_bytes())
    )
}

fn decoded(result: &Value, field: &str) -> String {
    let text = result
        .get(field)
        .and_then(Value::as_str)
        .unwrap_or_default();
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(text)
        .unwrap_or_default();
    String::from_utf8_lossy(&bytes).into_owned()
}

fn error_text(error: &GuestError) -> String {
    match error {
        GuestError::Rejected { code, detail, .. } => {
            let text = [code.as_deref(), detail.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(": ");
            format!("Linux guest layr.exec: {text}")
        }
        error => format!("Linux guest layr.exec failed: {error}"),
    }
}

/// Run one `layr_read` call after the claim of tool request `request_id` committed.
pub(crate) async fn run(
    actor: &Actor,
    args: &Value,
    request_id: &str,
    guest: Arc<dyn GuestRpc>,
) -> DispatchedOperation {
    let record = actor.record();
    let reviewer = record.extra.get("executionMode").and_then(Value::as_str) == Some("vm")
        && record.extra.get("role").and_then(Value::as_str) == Some("reviewer")
        && studio_runtime_store::truthy(record.extra.get("layrReady"));
    if !reviewer {
        return crate::host_exec::refused(REFUSAL.into());
    }
    let argv = match command(args) {
        Ok(argv) => argv,
        Err(message) => return crate::host_exec::refused(message),
    };
    let mut layr = vec!["layr".to_owned()];
    layr.extend(argv);
    let params = json!({"agentId": record.id.0, "argv": layr});
    let identity = identity(&record.id.0, request_id);
    let outcome = crate::host_exec::on_guest_thread(move || {
        guest.call(
            "layr.exec",
            &params,
            Some(&identity),
            Duration::from_secs(GUEST_WAIT_SECONDS),
        )
    })
    .await
    .and_then(|outcome| outcome);
    match outcome {
        Ok(result @ Value::Object(_)) => {
            let mut value = json!({
                "exitCode": result.get("exitCode").cloned().unwrap_or(Value::Null),
                "stdout": decoded(&result, "stdout"),
                "stderr": decoded(&result, "stderr"),
            });
            if let Some(reason) = result
                .get("reason")
                .filter(|reason| studio_runtime_store::truthy(Some(reason)))
            {
                value["reason"] = reason.clone();
            }
            DispatchedOperation {
                result: OperationResult {
                    value,
                    signals: Vec::new(),
                },
                certainty: DeliveryCertainty::Written,
                applied: Some(true),
            }
        }
        Ok(_) => crate::host_exec::unknown("The layr command has no proven result.".into()),
        // Nothing reached the guest: a connection or request check failed first.
        Err(error) if !error.uncertain() && !matches!(error, GuestError::Rejected { .. }) => {
            crate::host_exec::refused(error_text(&error))
        }
        // The guest answered with an explicit error, or its answer was lost.
        // The same request identity finds the first outcome.
        Err(error) => crate::host_exec::unknown(error_text(&error)),
    }
}

#[cfg(test)]
mod tests;

/// Native reviewers retain their provider sandbox. This port permits history reads only.
pub(crate) async fn run_native(
    store: &studio_runtime_store::RuntimeStore,
    actor: &Actor,
    args: &Value,
    request_id: &str,
) -> DispatchedOperation {
    let record = actor.record().value();
    if record["role"] != "reviewer" || record["layrReady"] != true {
        return crate::host_exec::refused("layr_read requires a ready Layr reviewer".into());
    }
    let argv = match command(args) {
        Ok(argv) => argv,
        Err(error) => return crate::host_exec::refused(error),
    };
    let state_dir = store.root().to_owned();
    let request_id = identity(actor.record().id.0.as_str(), request_id);
    let outcome = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/native-layr/read",
        move || {
            crate::layr_native::operation(
                &state_dir,
                &request_id,
                &json!({"agent":record["id"],"args":argv}),
                || {
                    let cwd = record["cwd"].as_str().ok_or_else(|| {
                        crate::layr_native::NativeLayrError::Refused("Missing reviewer cwd".into())
                    })?;
                    let refs: Vec<&str> = argv.iter().map(String::as_str).collect();
                    let output =
                        crate::layr_native::run(&state_dir, std::path::Path::new(cwd), &refs)?;
                    Ok(json!({"exitCode":0,"stdout":output,"stderr":""}))
                },
            )
        },
    )
    .await;
    match outcome {
        Ok(Ok(value)) => DispatchedOperation {
            result: OperationResult {
                value,
                signals: Vec::new(),
            },
            certainty: DeliveryCertainty::Written,
            applied: Some(true),
        },
        Ok(Err(error)) => crate::host_exec::refused(error.to_string()),
        Err(_) => crate::host_exec::unknown("Native Layr read worker failed".into()),
    }
}
