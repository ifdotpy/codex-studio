//! Managed host tool boundary for provider callbacks.
//!
//! Python dispatch is `codex_runtime.py::Runtime.dynamic` (8863–9210); spawn is
//! `Runtime.spawn_agents` (8611–8862), send is `Runtime.send` (5392–5487), stop
//! is `Runtime.stop` (10867–10933), and complaints are `Runtime.complaint`
//! (9539–9630). Directory/context projections are in `codex_efficiency.py`
//! (233–335, 635–760), search in `codex_work.py::search_work` (1415–1490),
//! watches in `codex_rules.py::RulesRuntime.rules` (179–349), monitors in
//! `codex_runtime.py::Runtime.monitor` (9948–10220), and reviews in
//! `codex_agent_review.py::request` (114–260).
//! The callback package owns durable request admission, result receipts, exact
//! replay and cancellation. This leaf owns authority checks and the typed
//! operation boundary; adapters own writes and post-commit wakeups.
//!
//! Send follows `Runtime.dynamic`'s two branches (codex_runtime.py:9050–9082):
//! descendant instructions call `Runtime.send` with the authenticated actor id
//! and epoch and enqueue a `followup`; other targets call `Runtime.chat_message`
//! with the same actor identity and default `message` importance. `parent` and
//! `lead` resolve through the actor's stored edges; a private room contains the
//! sorted sender/recipient ids after `peer_pair_allowed` or checked-team
//! authorization; `broadcast` contains the sender's root id and team members.
//! The descendant input uses `coordination::send_followup`, which writes its
//! durable queue event through `event_write::enqueue_event`.
//!
//! Spawn and Review use the P02 child-creation services after a durable callback
//! claim. Interrupt uses P10 `stop_by_agent` and the shared `LifecyclePorts`;
//! Status and Watch use the model-directory and rules-management services;
//! Monitor create uses workspace admission and persists its start intent before
//! waking the retained monitor runtime. Managed MonitorInput uses the callback
//! request id as its durable provider ticket and callback replay identity.
//!
//! Registered owner decision: "monitor input has no request identity; pending owner decision".
//! A lost HTTP response cannot be distinguished from a new
//! input. Provider writes still persist their own ticket first and never resend
//! Unknown.
//!
//! Remote-origin routing follows Python `Runtime.dynamic` (codex_runtime.py
//! 8969–8997, 9004–9049): Search and local Context topics remain local, while
//! context plan/complaints, Complaint, Status, non-descendant Send, and moved
//! Spawn/Interrupt require named `multi-server-runtime/*` adapters. The
//! monitor Context topic remains a local monitor snapshot.

use rusqlite::Transaction;
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_store::WorkItemsRepository;
use thiserror::Error;

const MAX_CALL_BYTES: usize = 1_000_000;
const MAX_REQUEST_ID_CHARS: usize = 1500;
const MAX_CHILDREN: usize = 64;

#[derive(Clone, Copy, Debug, Deserialize, PartialEq, Eq)]
pub(crate) enum ManagedHostTool {
    #[serde(rename = "orchestration_spawn")]
    Spawn,
    #[serde(rename = "orchestration_send")]
    Send,
    #[serde(rename = "orchestration_interrupt")]
    Interrupt,
    #[serde(rename = "orchestration_status")]
    Status,
    #[serde(rename = "orchestration_search")]
    Search,
    #[serde(rename = "orchestration_context")]
    Context,
    #[serde(rename = "orchestration_complaint")]
    Complaint,
    #[serde(rename = "orchestration_watch")]
    Watch,
    #[serde(rename = "orchestration_monitor")]
    Monitor,
    #[serde(rename = "orchestration_review")]
    Review,
}

#[derive(Clone, Debug)]
pub(crate) struct Actor {
    record: studio_runtime_store::AgentRecord,
    id: String,
    name: Option<String>,
    root_id: String,
    parent_id: Option<String>,
    is_lead: bool,
    reviewer: bool,
    provider: Option<String>,
    remote_origin: bool,
    execution_move: bool,
}

impl Actor {
    /// Construct the actor only from the already schema-checked callback row.
    pub(crate) fn from_record(
        value: &Value,
        expected_id: &str,
        expected_epoch: u64,
    ) -> Result<Self, Error> {
        let record = studio_runtime_store::AgentRecord::from_value(value.clone())
            .map_err(|_| Error::Invalid("Invalid callback actor"))?;
        let epoch =
            u64::try_from(record.epoch).map_err(|_| Error::Invalid("Invalid callback actor"))?;
        if record.id.0 != expected_id || epoch != expected_epoch {
            return Err(Error::Stale);
        }
        Ok(Self {
            id: record.id.0.clone(),
            name: record
                .extra
                .get("name")
                .and_then(Value::as_str)
                .map(str::to_owned),
            root_id: record.root_id.0.clone(),
            parent_id: record
                .extra
                .get("parentId")
                .and_then(Value::as_str)
                .map(str::to_owned),
            is_lead: record.extra.get("isLead") == Some(&Value::Bool(true)),
            reviewer: record.extra.get("role").and_then(Value::as_str) == Some("reviewer"),
            provider: record
                .extra
                .get("provider")
                .and_then(Value::as_str)
                .map(str::to_owned),
            remote_origin: truthy(record.extra.get("remoteOrigin")),
            execution_move: truthy(record.extra.get("executionMove")),
            record,
        })
    }

    pub(crate) fn record(&self) -> &studio_runtime_store::AgentRecord {
        &self.record
    }
}

#[derive(Clone, Debug)]
pub(crate) struct Call {
    pub(crate) tool: ManagedHostTool,
    pub(crate) args: Value,
    pub(crate) request_id: String,
}

impl Call {
    pub(crate) fn checked(tool: &str, args: Value, request_id: &str) -> Result<Self, Error> {
        if !args.is_object() || args.to_string().len() > MAX_CALL_BYTES {
            return Err(Error::Invalid("Invalid or oversized tool arguments"));
        }
        let tool = serde_json::from_value(json!(tool))
            .map_err(|_| Error::Invalid("Unknown managed host tool"))?;
        validate_request_id(request_id)?;
        Ok(Self {
            tool,
            args,
            request_id: request_id.to_owned(),
        })
    }
}

#[derive(Debug, Error)]
pub(crate) enum Error {
    #[error("{0}")]
    Invalid(&'static str),
    #[error("The caller identity changed")]
    Stale,
    #[error("invalid stored object {table}/{id} at {location}")]
    InvalidStored {
        table: &'static str,
        id: String,
        location: String,
    },
    #[error("Not implemented: requires {0}")]
    Unavailable(&'static str),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Work(#[from] crate::work_write::WorkWriteError),
}

/// Child construction stays behind P02's host creation seam. Every method is
/// called with the same writer transaction used for assignment and the tool
/// receipt; adapters must do no process, filesystem, provider or async work.
pub(crate) trait ChildCreationPort {
    type Prepared;

    fn prepare_work_item(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        request_id: &str,
        spec: &Value,
        now: f64,
    ) -> Result<Self::Prepared, Error>;

    fn recheck_worker(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        prepared: &Self::Prepared,
    ) -> Result<(), Error>;

    fn persisted_record(
        &self,
        tx: &Transaction<'_>,
        prepared: &Self::Prepared,
    ) -> Result<Value, Error>;

    /// Return only a wake signal. The initial worker event remains a durable
    /// row and startup reconciliation starts the child if this signal is lost.
    fn wake_signal(&self, prepared: &Self::Prepared) -> WakeSignal;
}

/// Transactional send adapters write the durable message/worker intent only;
/// their signal is released by the callback dispatcher after its final commit.
pub(crate) trait SendPort {
    fn send(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error>;
}

/// P10 lifecycle writes are staged in the callback transaction. The caller
/// drains the committed lifecycle effects only after that outer transaction.
pub(crate) trait InterruptPort {
    fn interrupt_in_transaction(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        target_id: &str,
        reason: &str,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error>;
}

pub(crate) trait StatusPort {
    fn status(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        args: &Value,
    ) -> Result<OperationResult, Error>;
}

pub(crate) trait SearchPort {
    fn search(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        args: &Value,
    ) -> Result<OperationResult, Error>;
}

pub(crate) trait ContextPort {
    fn context(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        args: &Value,
    ) -> Result<OperationResult, Error>;
}

pub(crate) trait ComplaintPort {
    fn complaint(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error>;
}

/// P15 owns watch policy and monitor persistence; its retained runtime is woken
/// by the caller after commit, never from this transaction.
pub(crate) trait WatchMonitorPort {
    fn watch(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error>;
    fn monitor(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error>;
}

/// P11 owns native review preparation/creation and its post-commit worker wake.
pub(crate) trait ReviewPort {
    fn review(
        &self,
        tx: &Transaction<'_>,
        actor: &Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<OperationResult, Error>;
}

/// Signals are deliberately weaker than effects: each corresponds to a
/// durable intent that its owner also reconciles after restart.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct WakeSignal {
    owner: String,
}

impl WakeSignal {
    pub(crate) fn new(owner: impl Into<String>) -> Self {
        Self {
            owner: owner.into(),
        }
    }

    pub(crate) fn owner(&self) -> &str {
        &self.owner
    }
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct OperationResult {
    pub(crate) value: Value,
    pub(crate) signals: Vec<WakeSignal>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DeliveryCertainty {
    Written,
    NotSent,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub(crate) struct DispatchedOperation {
    pub(crate) result: OperationResult,
    pub(crate) certainty: DeliveryCertainty,
    /// `Some(true/false)` is a provider's committed application/rejection
    /// receipt; `None` preserves an unresolved outcome.
    pub(crate) applied: Option<bool>,
}

/// The transaction caller persists `value` and the durable intent before
/// returning this object. Its `signals` are released only after commit.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Staged {
    pub(crate) value: Value,
    pub(crate) signals: Vec<WakeSignal>,
}

pub(crate) trait Ports:
    ChildCreationPort
    + SendPort
    + InterruptPort
    + StatusPort
    + SearchPort
    + ContextPort
    + ComplaintPort
    + WatchMonitorPort
    + ReviewPort
    + PostCommitPort
{
}

impl<T> Ports for T where
    T: ChildCreationPort
        + SendPort
        + InterruptPort
        + StatusPort
        + SearchPort
        + ContextPort
        + ComplaintPort
        + WatchMonitorPort
        + ReviewPort
        + PostCommitPort
{
}

pub(crate) trait PostCommitPort {
    /// Execute an operation after its callback claim has committed. `None`
    /// selects the leaf's transaction-local adapter; implementations return a
    /// certainty receipt for provider/process effects and preserve the exact id.
    fn dispatch_managed(
        &self,
        _store: &studio_runtime_store::RuntimeStore,
        _actor: &Actor,
        _tool: &str,
        _args: &Value,
        _request_id: &str,
        _now: f64,
    ) -> Result<Option<DispatchedOperation>, Error> {
        Ok(None)
    }

    /// Wake only. The owner must independently reconcile the durable intent.
    fn release(&self, signals: &[WakeSignal]) -> Result<(), Error>;
}

/// Safe fallback for callback callers that have not been given the runtime
/// adapter bundle yet. Every operation remains an explicit named refusal.
pub(crate) struct UnavailablePorts;

impl ChildCreationPort for UnavailablePorts {
    type Prepared = ();

    fn prepare_work_item(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &str,
        _: &Value,
        _: f64,
    ) -> Result<Self::Prepared, Error> {
        Err(Error::Unavailable("agent-create/worker-driver"))
    }

    fn recheck_worker(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &Self::Prepared,
    ) -> Result<(), Error> {
        Err(Error::Unavailable("agent-create/worker-driver"))
    }

    fn persisted_record(&self, _: &Transaction<'_>, _: &Self::Prepared) -> Result<Value, Error> {
        Err(Error::Unavailable("agent-create/worker-driver"))
    }

    fn wake_signal(&self, _: &Self::Prepared) -> WakeSignal {
        WakeSignal::new("worker-driver")
    }
}

impl SendPort for UnavailablePorts {
    fn send(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &Value,
        _: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        Err(Error::Unavailable(
            "agent-management/resume-and-stop-revocation",
        ))
    }
}

impl InterruptPort for UnavailablePorts {
    fn interrupt_in_transaction(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &str,
        _: &str,
        _: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        Err(Error::Unavailable("agent-management/runtime-driver"))
    }
}

impl StatusPort for UnavailablePorts {
    fn status(&self, _: &Transaction<'_>, _: &Actor, _: &Value) -> Result<OperationResult, Error> {
        Err(Error::Unavailable(
            "model-directory/status-and-dispatch-capacity",
        ))
    }
}

impl SearchPort for UnavailablePorts {
    fn search(&self, _: &Transaction<'_>, _: &Actor, _: &Value) -> Result<OperationResult, Error> {
        Err(Error::Unavailable(
            "model-search/authorized-cross-source-search",
        ))
    }
}

impl ContextPort for UnavailablePorts {
    fn context(&self, _: &Transaction<'_>, _: &Actor, _: &Value) -> Result<OperationResult, Error> {
        Err(Error::Unavailable("model-context/requested-topic"))
    }
}

impl ComplaintPort for UnavailablePorts {
    fn complaint(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &Value,
        _: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        Err(Error::Unavailable("complaints/agent-authority"))
    }
}

impl WatchMonitorPort for UnavailablePorts {
    fn watch(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &Value,
        _: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        Err(Error::Unavailable("rules/runtime-driver"))
    }

    fn monitor(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &Value,
        _: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        Err(Error::Unavailable("monitors/runtime-driver"))
    }
}

impl ReviewPort for UnavailablePorts {
    fn review(
        &self,
        _: &Transaction<'_>,
        _: &Actor,
        _: &Value,
        _: &str,
        _: f64,
    ) -> Result<OperationResult, Error> {
        Err(Error::Unavailable("native-review/worker-driver"))
    }
}

impl PostCommitPort for UnavailablePorts {
    fn release(&self, signals: &[WakeSignal]) -> Result<(), Error> {
        // Touch owner labels to keep the wake contract available to adapters,
        // while the fallback correctly performs no external activity.
        drop(signals.iter().map(WakeSignal::owner).collect::<Vec<_>>());
        Ok(())
    }
}

pub(crate) fn execute(
    tx: &Transaction<'_>,
    actor: &Actor,
    tool: &str,
    args: &Value,
    request_id: &str,
    now: f64,
    ports: &impl Ports,
) -> Result<Staged, Error> {
    let call = Call::checked(tool, args.clone(), request_id)?;
    if !now.is_finite() || now < 0.0 {
        return Err(Error::Invalid("Invalid callback clock"));
    }
    if let Some(dependency) = remote_dependency(tx, actor, &call)? {
        return Err(Error::Unavailable(dependency));
    }
    if actor.execution_move {
        return Err(Error::Invalid(
            "The move is accepted. Finish this turn before the next task",
        ));
    }
    let result = match call.tool {
        ManagedHostTool::Spawn => spawn(tx, actor, &call, now, ports)?,
        ManagedHostTool::Send => ports.send(tx, actor, &call.args, &call.request_id, now)?,
        ManagedHostTool::Interrupt => {
            let target = required_text(&call.args, "agent_id")?;
            if !is_descendant(tx, actor, target)? {
                return Err(Error::Invalid("You can interrupt only your descendants"));
            }
            let reason = format!(
                "Stopped by agent {}",
                actor
                    .name
                    .as_deref()
                    .filter(|name| !name.is_empty())
                    .unwrap_or(&actor.id)
            );
            let reason = reason.chars().take(160).collect::<String>();
            ports.interrupt_in_transaction(tx, actor, target, &reason, &call.request_id, now)?
        }
        ManagedHostTool::Status => ports.status(tx, actor, &call.args)?,
        ManagedHostTool::Search => ports.search(tx, actor, &call.args)?,
        ManagedHostTool::Context if call.args["topic"] == "plan" => {
            let plan = WorkItemsRepository::plan(tx, &actor.root_id)?
                .and_then(|record| record.record)
                .map(|raw| {
                    serde_json::from_str::<Value>(&raw)
                        .map_err(|_| Error::Invalid("Invalid stored work plan"))
                })
                .transpose()?
                .filter(|record| record["native"].is_object());
            let content = plan
                .map(|record| {
                    json!({
                        "id":record["id"],
                        "rootId":record["rootId"],
                        "steps":record["native"].get("plan").cloned().unwrap_or(json!([])),
                        "explanation":record["native"].get("explanation")
                            .filter(|value| truthy(Some(value)))
                            .cloned()
                            .unwrap_or(json!(""))
                    })
                })
                .unwrap_or(Value::Null);
            OperationResult {
                value: json!({
                    "topic":"plan",
                    "version":studio_runtime_store::sha256_hex(content.to_string().as_bytes())[..24].to_owned(),
                    "content":content
                }),
                signals: Vec::new(),
            }
        }
        ManagedHostTool::Context => ports.context(tx, actor, &call.args)?,
        ManagedHostTool::Complaint => {
            ports.complaint(tx, actor, &call.args, &call.request_id, now)?
        }
        ManagedHostTool::Watch => ports.watch(tx, actor, &call.args, &call.request_id, now)?,
        ManagedHostTool::Monitor => ports.monitor(tx, actor, &call.args, &call.request_id, now)?,
        ManagedHostTool::Review => {
            if actor
                .record
                .extra
                .get("executionMode")
                .and_then(Value::as_str)
                == Some("vm")
            {
                return Err(Error::Unavailable("layr-review/worker-driver"));
            }
            if actor.provider.as_deref().unwrap_or("codex") != "codex" {
                return Err(Error::Invalid("Native review requires a Codex agent"));
            }
            if truthy(actor.record.extra.get("nativeReview")) {
                return Err(Error::Invalid(
                    "A native reviewer cannot create another review",
                ));
            }
            ports.review(tx, actor, &call.args, &call.request_id, now)?
        }
    };
    Ok(Staged {
        value: result.value,
        signals: result.signals,
    })
}

/// Match Runtime.dynamic's remote-origin branches. Search, monitor Context,
/// watches, monitors, reviews and ordinary interrupts remain local. Calls whose
/// Python branch delegates to multi_server.worker_call retain a named refusal
/// until that adapter is available.
fn remote_dependency(
    tx: &Transaction<'_>,
    actor: &Actor,
    call: &Call,
) -> Result<Option<&'static str>, Error> {
    if !actor.remote_origin {
        return Ok(None);
    }
    let moved_from = truthy(actor.record.extra.get("movedFrom"));
    let dependency = match call.tool {
        ManagedHostTool::Send => {
            // Python routes every moved remote-origin send through move_send
            // before resolving the target, including a local descendant.
            if moved_from {
                return Ok(Some("multi-server-runtime/move-send"));
            }
            let target = required_text(&call.args, "agent_id")?;
            // chat_message owns user messages and approved federation targets
            // locally. Let its adapter return those dependencies instead of
            // misrouting them through the remote message transport.
            if target == "user" || target.starts_with("remote:") || target.starts_with("federated:")
            {
                None
            } else {
                (!is_descendant(tx, actor, target)?).then_some("multi-server-runtime/message")
            }
        }
        ManagedHostTool::Status => Some("multi-server-runtime/directory"),
        ManagedHostTool::Complaint => Some("multi-server-runtime/complaint"),
        ManagedHostTool::Context
            if matches!(call.args["topic"].as_str(), Some("plan" | "complaints")) =>
        {
            Some("multi-server-runtime/context")
        }
        ManagedHostTool::Spawn if moved_from => Some("multi-server-runtime/spawn"),
        ManagedHostTool::Interrupt if moved_from => Some("multi-server-runtime/interrupt"),
        _ => None,
    };
    Ok(dependency)
}

fn spawn(
    tx: &Transaction<'_>,
    actor: &Actor,
    call: &Call,
    now: f64,
    ports: &impl Ports,
) -> Result<OperationResult, Error> {
    if actor.reviewer {
        return Err(Error::Invalid("Reviewers cannot create agents"));
    }
    if !actor.is_lead || actor.parent_id.is_some() || actor.root_id != actor.id {
        return Err(Error::Invalid("Only the active lead can spawn workers"));
    }
    if call.args.get("server").is_some_and(|v| truthy(Some(v))) {
        return Err(Error::Unavailable("multi-server/worker-call"));
    }
    let specs = call
        .args
        .get("agents")
        .and_then(Value::as_array)
        .filter(|agents| !agents.is_empty() && agents.len() <= MAX_CHILDREN)
        .ok_or(Error::Invalid("Supply 1 to 64 agents"))?;
    let mut children = Vec::with_capacity(specs.len());
    let mut signals = Vec::new();
    let mut assigned = std::collections::BTreeSet::new();
    for spec in specs {
        if !spec.is_object() {
            return Err(Error::Invalid("Invalid agent specification"));
        }
        let name = required_text(spec, "name")?.trim();
        let prompt = required_text(spec, "prompt")?.trim();
        let role = match spec.get("role") {
            None => "implementer",
            Some(Value::String(role)) => role.as_str(),
            Some(_) => {
                return Err(Error::Invalid(
                    "Every worker needs a name, task and valid role",
                ));
            }
        };
        if name.is_empty()
            || name.chars().count() > 100
            || prompt.is_empty()
            || prompt.chars().count() > 32_000
            || !matches!(role, "implementer" | "reviewer")
        {
            return Err(Error::Invalid(
                "Every worker needs a name, task and valid role",
            ));
        }
        if spec.get("server").is_some_and(|v| truthy(Some(v))) {
            return Err(Error::Unavailable("multi-server/worker-call"));
        }
        if spec
            .get("task_id")
            .is_some_and(|v| !v.is_string() || v.as_str().is_none_or(str::is_empty))
        {
            return Err(Error::Invalid("task_id must be a task id"));
        }
        if spec.get("base_ref").is_some_and(|v| {
            v.as_str()
                .is_none_or(|s| s.trim().is_empty() || s.chars().count() > 1024)
        }) {
            return Err(Error::Invalid("base_ref must be a branch, tag, or commit"));
        }
        let prepared = ports.prepare_work_item(tx, actor, &call.request_id, spec, now)?;
        ports.recheck_worker(tx, actor, &prepared)?;
        let persisted = ports.persisted_record(tx, &prepared)?;
        let child_id = field(&persisted, "id")?;
        let spec_task_id = spec.get("task_id").and_then(Value::as_str);
        if let Some(task_id) = spec_task_id {
            if !actor.is_lead {
                return Err(Error::Invalid(
                    "Only the lead can delegate an existing task; ask the lead to assign it",
                ));
            }
            if !assigned.insert(task_id) {
                return Err(Error::Invalid("Assign each task to one worker in a batch"));
            }
            crate::work_assignment_runtime::assign_in_transaction(
                tx,
                task_id,
                child_id,
                actor.record(),
                now,
            )?;
        }
        signals.push(ports.wake_signal(&prepared));
        children.push(project_child(&persisted, spec_task_id));
    }
    Ok(OperationResult {
        value: json!({"requestId":call.request_id,"agents":children,"delivery":"Results wake you automatically. Finish your turn while waiting."}),
        signals,
    })
}

/// Match the public child projection returned by Python `spawn_agents`; never
/// return a full stored agent record to the model.
fn project_child(record: &Value, task_id: Option<&str>) -> Value {
    let mut output = serde_json::Map::new();
    for key in [
        "id",
        "name",
        "status",
        "model",
        "effort",
        "fastMode",
        "accountKey",
        "provider",
        "cwd",
        "worktree",
    ] {
        output.insert(
            key.to_owned(),
            record.get(key).cloned().unwrap_or(Value::Null),
        );
    }
    output.insert(
        "environment".to_owned(),
        record
            .get("environment")
            .cloned()
            .unwrap_or_else(|| json!("host")),
    );
    output.insert(
        "workspace".to_owned(),
        record
            .get("workspaceMode")
            .filter(|mode| truthy(Some(mode)))
            .cloned()
            .unwrap_or_else(|| {
                if truthy(record.get("imageWorkspace")) {
                    if record["environment"] == "linux" {
                        json!("linux")
                    } else {
                        json!("image")
                    }
                } else if truthy(record.get("worktree")) {
                    json!("worktree")
                } else {
                    json!("shared")
                }
            }),
    );
    output.insert(
        "workspaceBackend".to_owned(),
        record
            .get("workspaceBackend")
            .cloned()
            .unwrap_or(Value::Null),
    );
    if truthy(record.get("imageWorkspace")) {
        output.insert(
            "workspaceState".to_owned(),
            json!(if truthy(record.get("imageWorkspaceReady")) {
                "ready"
            } else {
                "building"
            }),
        );
    }
    if truthy(record.get("imageWorkspaceError")) {
        copy_if(record, &mut output, "workspaceError", "imageWorkspaceError");
    }
    if let Some(base_commit) = record.get("workerBaseCommit") {
        output.insert("baseRef".to_owned(), record["workerBaseRef"].clone());
        output.insert("baseCommit".to_owned(), base_commit.clone());
    }
    if let Some(task_id) = task_id {
        output.insert("taskId".to_owned(), json!(task_id));
    }
    if truthy(record.get("workerBaseBehindMain")) {
        let behind = &record["workerBaseBehindMain"];
        let main_ref = record
            .get("workerBaseMainRef")
            .cloned()
            .unwrap_or_else(|| json!("main"));
        output.insert("baseBehindMain".to_owned(), behind.clone());
        output.insert("baseMainRef".to_owned(), main_ref.clone());
        output.insert(
            "baseWarning".to_owned(),
            json!(format!(
                "Base commit {} is {} commits behind {}",
                record["workerBaseCommit"].as_str().unwrap_or(""),
                behind,
                main_ref.as_str().unwrap_or("main"),
            )),
        );
    }
    if truthy(record.get("worktreeWarning")) {
        copy_if(record, &mut output, "warning", "worktreeWarning");
    }
    Value::Object(output)
}

fn copy_if(record: &Value, output: &mut serde_json::Map<String, Value>, to: &str, from: &str) {
    if let Some(value) = record.get(from) {
        output.insert(to.to_owned(), value.clone());
    }
}

fn is_descendant(tx: &Transaction<'_>, actor: &Actor, target: &str) -> Result<bool, Error> {
    let records = studio_runtime_store::system_reads::all(tx, "runtime_agents")?;
    let mut current = records
        .iter()
        .find(|record| record.get("id").and_then(Value::as_str) == Some(target));
    let mut visited = 0usize;
    while let Some(record) = current {
        if record.get("rootId").and_then(Value::as_str) != Some(actor.root_id.as_str()) {
            return Ok(false);
        }
        let parent = record.get("parentId").and_then(Value::as_str);
        if parent == Some(actor.id.as_str()) {
            return Ok(true);
        }
        let Some(parent) = parent else {
            return Ok(false);
        };
        visited += 1;
        if visited > records.len() {
            return Err(Error::Invalid("Invalid stored agent ancestry"));
        }
        current = records
            .iter()
            .find(|candidate| candidate.get("id").and_then(Value::as_str) == Some(parent));
    }
    Ok(false)
}

fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, Error> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(Error::Invalid("Invalid callback actor"))
}

fn required_text<'a>(value: &'a Value, name: &str) -> Result<&'a str, Error> {
    value
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(Error::Invalid("Missing required text argument"))
}

fn validate_request_id(value: &str) -> Result<(), Error> {
    if value.is_empty()
        || value.chars().count() > MAX_REQUEST_ID_CHARS
        || value.trim() != value
        || value.chars().any(char::is_control)
    {
        return Err(Error::Invalid("Invalid managed tool request id"));
    }
    Ok(())
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) | Some(Value::Bool(false)) => false,
        Some(Value::Number(number)) => number.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(string)) => !string.is_empty(),
        Some(Value::Array(items)) => !items.is_empty(),
        Some(Value::Object(fields)) => !fields.is_empty(),
        Some(Value::Bool(true)) => true,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn actor_snapshot_binds_id_and_epoch() {
        let value = json!({"id":"lead","rootId":"lead","epoch":3,"status":"running","isLead":true});
        let actor = Actor::from_record(&value, "lead", 3).unwrap();
        assert_eq!(actor.id, "lead");
        assert_eq!(actor.root_id, "lead");
        assert_eq!(actor.record.epoch, 3);
        assert!(matches!(
            Actor::from_record(&value, "other", 3),
            Err(Error::Stale)
        ));
        assert!(matches!(
            Actor::from_record(&value, "lead", 4),
            Err(Error::Stale)
        ));
    }

    #[test]
    fn parser_accepts_only_managed_tools_and_bounded_identity() {
        let call = Call::checked("orchestration_spawn", json!({"agents":[{}]}), "retry-1").unwrap();
        assert_eq!(call.tool, ManagedHostTool::Spawn);
        assert!(Call::checked("orchestration_message", json!({}), "retry-1").is_err());
        assert!(Call::checked("orchestration_send", json!({}), " retry-1").is_err());
    }

    #[test]
    fn remote_and_move_flags_are_truthy_only_for_nonempty_values() {
        assert!(!truthy(Some(&Value::Null)));
        assert!(!truthy(Some(&json!(""))));
        assert!(!truthy(Some(&json!([]))));
        assert!(truthy(Some(&json!({"remote":true}))));
        assert!(truthy(Some(&json!(1))));
    }

    #[test]
    fn child_projection_does_not_return_private_record_fields() {
        let child = json!({
            "id":"worker-1", "name":"Worker", "status":"starting", "provider":"codex",
            "cwd":"/work", "worktree":true, "workspaceMode":"worktree",
            "apiKey":"must-not-leak", "prompt":"private task text", "rootId":"lead"
        });
        let projected = project_child(&child, Some("task-1"));
        assert_eq!(projected["taskId"], "task-1");
        assert_eq!(projected["workspace"], "worktree");
        assert!(projected.get("apiKey").is_none());
        assert!(projected.get("prompt").is_none());
        assert!(projected.get("rootId").is_none());
    }

    #[test]
    fn remote_local_reads_are_allowed_and_cross_server_calls_name_the_dependency() {
        let actor = Actor::from_record(
            &json!({"id":"worker","rootId":"lead","epoch":5,"status":"running","remoteOrigin":true}),
            "worker",
            5,
        )
        .unwrap();
        let mut connection = rusqlite::Connection::open_in_memory().unwrap();
        let transaction = connection.transaction().unwrap();
        let remote_status = execute(
            &transaction,
            &actor,
            "orchestration_status",
            &json!({}),
            "thread:status-1",
            1.0,
            &UnavailablePorts,
        );
        assert!(matches!(
            remote_status,
            Err(Error::Unavailable("multi-server-runtime/directory"))
        ));
    }
}

#[cfg(test)]
#[path = "managed_host_tools/ingress_tests.rs"]
mod ingress_tests;
