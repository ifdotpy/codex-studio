//! VM-only `host_exec`: one macOS command from a VM agent's layr line
//! (docs/host-exec.md).
//!
//! This module owns the tool contract: the argument checks, the guest request
//! and its identity, the waits and the output limit. The Mac side is the service
//! of [`crate::host_exec_service`]. A lost or uncertain guest answer never
//! repeats the command: the caller gets an unknown outcome with the operation ID
//! to read with `status` or `list`.

use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use futures_util::future::BoxFuture;
use serde_json::{Map, Value, json};
use studio_runtime_store::RuntimeStore;

use crate::linux_vm_guest::{GuestError, GuestRpc};
use crate::managed_host_tools::{Actor, DeliveryCertainty, DispatchedOperation, OperationResult};

/// The tool name.
pub(crate) const TOOL: &str = "host_exec";
/// The dependency that `host_exec` names until the host attaches its runtime.
pub(crate) const DEPENDENCY: &str = "host-exec/layr-bridge";

/// Run time of a command without `timeout_seconds`, in seconds.
pub(crate) const DEFAULT_TIMEOUT_SECONDS: f64 = 300.0;
/// Largest `timeout_seconds`, in seconds; the bundled definition states the same maximum.
pub(crate) const MAX_TIMEOUT_SECONDS: f64 = 3600.0;
/// Seconds that the guest wait of an execute request adds to the command
/// timeout: the slot transfer before the command and the result transfer after it.
pub(crate) const EXECUTE_STREAM_MARGIN_SECONDS: f64 = 3660.0;
/// Guest wait of a status, cancel, list or release request, in seconds.
pub(crate) const STREAM_TIMEOUT_SECONDS: f64 = 60.0;
/// Largest command output that a tool result keeps, in characters. The rest
/// stays readable with action status.
pub(crate) const OUTPUT_LIMIT_CHARS: usize = 262_144;
/// The shell that runs the command text: argv is this pair plus the command.
pub(crate) const COMMAND_SHELL: [&str; 2] = ["/bin/zsh", "-lc"];
/// Guest request ID prefix of one exact tool request.
pub(crate) const REQUEST_ID_PREFIX: &str = "host-exec:";
/// Guest request ID prefix of the channel token configuration.
pub(crate) const CONFIGURE_ID_PREFIX: &str = "host-configure:";
/// Guest wait of `host.configure`, in seconds.
pub(crate) const CONFIGURE_TIMEOUT_SECONDS: f64 = 15.0;
/// The channel token file in the VM state folder: the Mac service checks it,
/// the guest gets it with `host.configure`.
pub(crate) const TOKEN_FILE: &str = "host-exec-token";
/// The tool receipt field that keeps the exact guest request.
pub(crate) const RECEIPT_REQUEST_FIELD: &str = "hostExecRequest";
/// Text after an error that has an operation ID; `{}` is the ID.
pub(crate) const OPERATION_NOTE: &str =
    "Operation: {}. Use action status or list; never repeat the command.";

const REVIEWER_REFUSAL: &str = "host_exec is unavailable to read-only reviewers";
const VM_ONLY_REFUSAL: &str = "host_exec is available only to VM agents";

/// The bundled tool definition (`host_exec_tool.json`, its owner).
pub(crate) fn tool_definition() -> Option<&'static crate::thread_prepare::ToolDefinition> {
    static DEFINITION: OnceLock<Option<crate::thread_prepare::ToolDefinition>> = OnceLock::new();
    DEFINITION
        .get_or_init(|| serde_json::from_str(include_str!("host_exec_tool.json")).ok())
        .as_ref()
}

/// The actions of the bundled definition; the guest broker implements each.
fn known_action(action: &str) -> bool {
    tool_definition()
        .and_then(|tool| tool.input_schema.get("properties"))
        .and_then(|properties| properties.pointer("/action/enum"))
        .and_then(Value::as_array)
        .is_some_and(|actions| actions.iter().any(|known| known == action))
}

/// Keeps the Mac `host_exec` service running.
pub trait HostExecService: Send + Sync {
    /// Make sure a healthy service of the current build serves the VM state
    /// folder, then return the channel token. An error proves that no guest
    /// request was sent.
    fn ensure(&self) -> BoxFuture<'_, Result<String, String>>;
}

/// Starts the Mac service as `studio-runtime-server host-exec-service` from
/// the backend executable (`host_exec_service::lifecycle::ensure_service`): it
/// creates the token, probes `host-exec.sock`, asks an idle service of another
/// build to retire, and starts the current build in its own session.
pub struct RustHostExecService {
    program: PathBuf,
    vm_state_dir: PathBuf,
    build: fn() -> std::io::Result<String>,
    browser_host: Option<PathBuf>,
}

impl RustHostExecService {
    /// `program` is the `studio-runtime-server` executable; `build` gives its
    /// build identity, which the first call computes when startup did not.
    pub fn new(
        program: PathBuf,
        vm_state_dir: PathBuf,
        build: fn() -> std::io::Result<String>,
    ) -> Self {
        Self {
            program,
            vm_state_dir,
            build,
            browser_host: None,
        }
    }

    /// Give the commands the host bridge in `folder` (`<state>/browser-host`) for their
    /// sign-in links.
    #[must_use]
    pub fn with_browser_host(mut self, folder: PathBuf) -> Self {
        self.browser_host = Some(folder);
        self
    }
}

impl HostExecService for RustHostExecService {
    fn ensure(&self) -> BoxFuture<'_, Result<String, String>> {
        let (program, state, build) = (self.program.clone(), self.vm_state_dir.clone(), self.build);
        let browser_host = self.browser_host.clone();
        Box::pin(async move {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/host-exec-service",
                move || {
                    let build = build().map_err(|error| {
                        format!("The backend build identity is unknown: {error}")
                    })?;
                    crate::host_exec_service::lifecycle::ensure_service(
                        &state,
                        &program,
                        &build,
                        browser_host.as_deref(),
                    )
                },
            )
            .await
            .unwrap_or_else(|_| Err("The host_exec service check ended without a result".into()))
        })
    }
}

/// The guest channel and the Mac service of `host_exec`.
pub struct HostExecRuntime {
    guest: Arc<dyn GuestRpc>,
    service: Arc<dyn HostExecService>,
}

impl HostExecRuntime {
    pub fn new(guest: Arc<dyn GuestRpc>, service: Arc<dyn HostExecService>) -> Arc<Self> {
        Arc::new(Self { guest, service })
    }

    /// Make sure the Mac service runs and give the guest its channel token, as
    /// each call does. A VM start does it once the guest answers.
    pub async fn configure_guest(&self) -> Result<(), String> {
        let token = self.service.ensure().await?;
        configure(Arc::clone(&self.guest), token)
            .await
            .map_err(|error| guest_error_text(&error))
    }
}

/// The exact guest request of one tool request (the receipt field `hostExecRequest`).
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct GuestRequest {
    pub(crate) request_id: String,
    pub(crate) params: Value,
    /// Guest wait, in seconds.
    pub(crate) timeout: f64,
}

impl GuestRequest {
    fn record(&self) -> Value {
        json!({"requestId": self.request_id, "params": self.params, "timeout": self.timeout})
    }

    fn operation_note(&self) -> Option<String> {
        self.params
            .get("operationId")
            .and_then(Value::as_str)
            .map(operation_note)
    }
}

pub(crate) fn operation_note(operation: &str) -> String {
    OPERATION_NOTE.replace("{}", operation)
}

/// The argument checks and guest parameters of one call. `request_id` is the
/// tool request ID, used when the caller supplies no `request_id`.
pub(crate) fn guest_request(
    actor_id: &str,
    actor_cwd: Option<&Value>,
    args: &Value,
    request_id: &str,
) -> Result<GuestRequest, String> {
    let action = match args.get("action") {
        None => "execute",
        Some(Value::String(action)) if known_action(action) => action.as_str(),
        Some(_) => return Err("Unknown host_exec action".into()),
    };
    let identity = match args.get("request_id") {
        Some(Value::String(value)) if !value.is_empty() => value.clone(),
        Some(Value::Number(value)) if value.as_f64() != Some(0.0) => value.to_string(),
        None | Some(Value::Null) | Some(Value::String(_)) | Some(Value::Number(_)) => {
            request_id.to_owned()
        }
        Some(_) => return Err("Supply request_id as a string".into()),
    };
    // Provider call IDs can exceed the guest's identity limit. Hash, do not truncate.
    let identity = format!(
        "{REQUEST_ID_PREFIX}{}",
        studio_runtime_store::sha256_hex(format!("{actor_id}:{identity}").as_bytes())
    );
    let mut params = Map::new();
    params.insert("agentId".into(), json!(actor_id));
    params.insert("action".into(), json!(action));
    let timeout = if action == "execute" {
        let command = args
            .get("command")
            .and_then(Value::as_str)
            .filter(|command| !command.is_empty() && !command.contains('\0'))
            .ok_or("Supply a command")?;
        let refusal =
            || format!("The command timeout must be between 0 and {MAX_TIMEOUT_SECONDS} seconds");
        let timeout = match args.get("timeout_seconds") {
            None => json!(DEFAULT_TIMEOUT_SECONDS as u64),
            Some(value @ Value::Number(number))
                if number
                    .as_f64()
                    .is_some_and(|seconds| seconds > 0.0 && seconds <= MAX_TIMEOUT_SECONDS) =>
            {
                value.clone()
            }
            Some(_) => return Err(refusal()),
        };
        let seconds = timeout.as_f64().ok_or_else(refusal)?;
        params.insert(
            "argv".into(),
            json!([COMMAND_SHELL[0], COMMAND_SHELL[1], command]),
        );
        params.insert(
            "cwd".into(),
            args.get("cwd")
                .or(actor_cwd)
                .cloned()
                .unwrap_or(Value::Null),
        );
        params.insert("timeoutSeconds".into(), timeout);
        params.insert("operationId".into(), json!(identity));
        seconds + EXECUTE_STREAM_MARGIN_SECONDS
    } else if action == "list" {
        STREAM_TIMEOUT_SECONDS
    } else {
        let operation = args
            .get("operation_id")
            .and_then(Value::as_str)
            .filter(|operation| !operation.is_empty())
            .ok_or("Supply the host_exec operation_id")?;
        params.insert("operationId".into(), json!(operation));
        params.insert(
            "afterSeq".into(),
            args.get("after_seq").cloned().unwrap_or(json!(0)),
        );
        STREAM_TIMEOUT_SECONDS
    };
    Ok(GuestRequest {
        request_id: identity,
        params: Value::Object(params),
        timeout,
    })
}

/// Run one `host_exec` call after the claim of tool request
/// `request_id` committed.
pub(crate) async fn run(
    store: Arc<RuntimeStore>,
    actor: &Actor,
    args: &Value,
    request_id: &str,
    runtime: &HostExecRuntime,
) -> DispatchedOperation {
    let record = actor.record();
    if record.extra.get("role").and_then(Value::as_str) == Some("reviewer") {
        return refused(REVIEWER_REFUSAL.into());
    }
    let root_mode = if record.root_id == record.id {
        record.extra.get("executionMode").cloned()
    } else {
        let root_id = record.root_id.clone();
        match store
            .read_async("runtime-services/host_exec", move |tx| {
                studio_runtime_store::AgentRepository::get(tx, &root_id)
            })
            .await
        {
            Ok(root) => root.and_then(|root| root.extra.get("executionMode").cloned()),
            Err(error) => return refused(format!("Could not read the root agent: {error}")),
        }
    };
    if root_mode.as_ref().and_then(Value::as_str) != Some("vm")
        || record.extra.get("executionMode").and_then(Value::as_str) != Some("vm")
    {
        return refused(VM_ONLY_REFUSAL.into());
    }
    let request = match guest_request(&record.id.0, record.extra.get("cwd"), args, request_id) {
        Ok(request) => request,
        Err(message) => return refused(message),
    };
    if let Err(error) = remember(&store, request_id, &request).await {
        return refused(format!("Could not save the host_exec request: {error}"));
    }
    // The Mac service may have stopped since the VM started; a check costs one
    // local request. The guest gets the channel token of that service.
    let token = match runtime.service.ensure().await {
        Ok(token) => token,
        Err(message) => return refused(message),
    };
    if let Err(error) = configure(Arc::clone(&runtime.guest), token).await {
        return refused(format!(
            "The host command channel is not configured: {}",
            guest_error_text(&error)
        ));
    }
    let guest = Arc::clone(&runtime.guest);
    let sent = request.clone();
    let (outcome, output) = on_guest_thread(move || stream(guest.as_ref(), &sent))
        .await
        .unwrap_or_else(|error| (Err(error), String::new()));
    settle(&request, outcome, output)
}

/// Run one guest exchange on its own thread. A command can run for hours, so
/// it must not hold a thread of the runtime's blocking pool, which serves
/// store reads (`browser_cards::guest_call` follows the same rule).
pub(crate) async fn on_guest_thread<T: Send + 'static>(
    work: impl FnOnce() -> T + Send + 'static,
) -> Result<T, GuestError> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    if std::thread::Builder::new()
        .name("studio-host-exec".into())
        .spawn(move || drop(sender.send(work())))
        .is_err()
    {
        // Nothing reached the guest.
        return Err(GuestError::Transport {
            message: "could not start the guest call".into(),
            uncertain: false,
        });
    }
    receiver.await.map_err(|_| GuestError::Transport {
        message: "the guest task ended without a result".into(),
        uncertain: true,
    })
}

/// Save the exact guest request on its running
/// receipt before the guest call, so a restart can find the same request.
async fn remember(
    store: &RuntimeStore,
    request_id: &str,
    request: &GuestRequest,
) -> Result<(), crate::provider_callbacks::CallbackError> {
    let id = request_id.to_owned();
    let saved = request.record();
    store
        .transaction_with(move |tx| {
            let Some(mut record) =
                studio_runtime_store::system_reads::get(tx, "runtime_tool_requests", &id)?
            else {
                return Ok(());
            };
            if record["stage"] != "running" || record.get(RECEIPT_REQUEST_FIELD) == Some(&saved) {
                return Ok(());
            }
            record[RECEIPT_REQUEST_FIELD] = saved;
            crate::provider_callbacks::tools::put(tx, &record)
        })
        .await
}

/// Give the guest the channel token of the Mac service.
async fn configure(guest: Arc<dyn GuestRpc>, token: String) -> Result<(), GuestError> {
    on_guest_thread(move || {
        let identity = format!(
            "{CONFIGURE_ID_PREFIX}{}",
            studio_runtime_store::sha256_hex(token.as_bytes())
        );
        guest
            .call(
                "host.configure",
                &json!({"token": token}),
                Some(&identity),
                Duration::from_secs_f64(CONFIGURE_TIMEOUT_SECONDS),
            )
            .map(drop)
    })
    .await?
}

/// Send the request and keep up to [`OUTPUT_LIMIT_CHARS`] of its output frames.
fn stream(guest: &dyn GuestRpc, request: &GuestRequest) -> (Result<Value, GuestError>, String) {
    let mut output = String::new();
    let mut length = 0_usize;
    let mut on_frame = |frame: Value| {
        if frame.get("event").and_then(Value::as_str) == Some("output") {
            let text = frame
                .pointer("/data/text")
                .and_then(Value::as_str)
                .unwrap_or_default();
            if length < OUTPUT_LIMIT_CHARS {
                output.extend(text.chars().take(OUTPUT_LIMIT_CHARS - length));
            }
            length = length.saturating_add(text.chars().count());
        }
        Ok(())
    };
    let outcome = guest.stream(
        "host.exec",
        &request.params,
        Some(&request.request_id),
        Duration::from_secs_f64(request.timeout),
        &mut on_frame,
    );
    (outcome, output)
}

fn settle(
    request: &GuestRequest,
    outcome: Result<Value, GuestError>,
    output: String,
) -> DispatchedOperation {
    let note = request.operation_note();
    let with_note = |text: String| match &note {
        Some(note) => format!("{text} {note}"),
        None => text,
    };
    match outcome {
        Ok(Value::Object(mut result)) => {
            result.insert("output".into(), json!(output));
            DispatchedOperation {
                result: OperationResult {
                    value: Value::Object(result),
                    signals: Vec::new(),
                },
                certainty: DeliveryCertainty::Written,
                applied: Some(true),
            }
        }
        Ok(_) => unknown(with_note("The host command has no proven result.".into())),
        // Nothing reached the guest: a connection or request check failed first.
        Err(error) if !error.uncertain() && !matches!(error, GuestError::Rejected { .. }) => {
            refused(with_note(guest_error_text(&error)))
        }
        // The guest answered with an explicit error, or its answer was lost.
        // Both keep their uncertainty: the command is
        // never sent again, and the operation ID finds its outcome.
        Err(error) => unknown(with_note(guest_error_text(&error))),
    }
}

/// The text of a `host.exec` or `host.configure` failure.
fn guest_error_text(error: &GuestError) -> String {
    match error {
        GuestError::Rejected { code, detail, .. } => {
            let text = [code.as_deref(), detail.as_deref()]
                .into_iter()
                .flatten()
                .collect::<Vec<_>>()
                .join(": ");
            format!("VM guest host.exec: {text}")
        }
        error => format!("VM guest host.exec failed: {error}"),
    }
}

pub(crate) fn refused(message: String) -> DispatchedOperation {
    DispatchedOperation {
        result: OperationResult {
            value: json!(message),
            signals: Vec::new(),
        },
        certainty: DeliveryCertainty::NotSent,
        applied: Some(false),
    }
}

pub(crate) fn unknown(message: String) -> DispatchedOperation {
    DispatchedOperation {
        result: OperationResult {
            value: json!(message),
            signals: Vec::new(),
        },
        certainty: DeliveryCertainty::Unknown,
        applied: None,
    }
}

#[cfg(test)]
mod tests;
