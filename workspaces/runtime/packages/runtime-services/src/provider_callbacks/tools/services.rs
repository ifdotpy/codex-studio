//! Closed local service table. Every entry checks actor authority in the same
//! transaction as its effects. No HTTP adapters and no user-authored substitutes.
mod context;
pub(crate) mod manage;
pub(crate) mod messages;
mod reads;
#[cfg(test)]
#[path = "services/sends_tests.rs"]
mod sends_tests;
mod work;
use super::*;
use crate::{event_write, work_policy};
use futures_util::future::BoxFuture;
use studio_runtime_store::WorkItemsRepository;

/// Default local bundle for callback callers. Message ingress reuses the
/// callback package's checked transactional implementation; capabilities whose
/// owning runtime adapter has not landed retain their named refusal.
pub(crate) struct ProductionPorts {
    context: CallbackContext,
    room_directory: Option<crate::sync_entity_context::RoomDirectory>,
    admission: crate::message_admission::PreparedAdmission,
    event_wake: std::sync::Arc<tokio::sync::Notify>,
    monitor_dispatch_wake: std::sync::Arc<tokio::sync::Notify>,
    runtime_ports: super::ManagedToolRuntimePorts,
    state_root: std::path::PathBuf,
    preparation: Option<std::sync::Arc<crate::account_worker::preparation::PreparationResources>>,
}

pub(crate) struct ProductionPortsConfig {
    pub(crate) context: CallbackContext,
    pub(crate) room_directory: Option<crate::sync_entity_context::RoomDirectory>,
    pub(crate) admission: crate::message_admission::PreparedAdmission,
    pub(crate) event_wake: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) monitor_dispatch_wake: std::sync::Arc<tokio::sync::Notify>,
    pub(crate) runtime_ports: super::ManagedToolRuntimePorts,
    /// The store's state directory, which owns progress files.
    pub(crate) state_root: std::path::PathBuf,
    pub(crate) preparation:
        Option<std::sync::Arc<crate::account_worker::preparation::PreparationResources>>,
}

impl ProductionPorts {
    pub(crate) fn new(config: ProductionPortsConfig) -> Self {
        Self {
            context: config.context,
            room_directory: config.room_directory,
            admission: config.admission,
            event_wake: config.event_wake,
            monitor_dispatch_wake: config.monitor_dispatch_wake,
            runtime_ports: config.runtime_ports,
            state_root: config.state_root,
            preparation: config.preparation,
        }
    }
}

impl crate::managed_host_tools::SendPort for ProductionPorts {
    fn send(
        &self,
        tx: &Transaction<'_>,
        _actor: &crate::managed_host_tools::Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        let snapshot = super::super::storage::snapshot(tx, &self.context)
            .map_err(|_| crate::managed_host_tools::Error::Stale)?;
        let target = args["agent_id"]
            .as_str()
            .ok_or(crate::managed_host_tools::Error::Invalid(
                "Supply agent_id for orchestration_send",
            ))?;
        // Python resolves every orchestration_send agent_id except parent, lead,
        // broadcast, all and workspace as a visible agent ID, so "user" is a
        // too-short ID there. A lead contacts the user with orchestration_message.
        if target == "user" {
            return Err(crate::managed_host_tools::Error::Invalid(
                "Too short agent ID; use at least 8 characters",
            ));
        }
        // An agent ID names one visible agent: the full ID, or a prefix of at
        // least eight characters that matches exactly one agent in the caller's
        // scope. An ambiguous or unknown ID is refused with its reason.
        let resolved;
        let target = if matches!(target, "parent" | "lead" | "broadcast" | "all")
            || target.starts_with("remote:")
            || target.starts_with("federated:")
        {
            target
        } else {
            resolved = messages::resolve_agent(tx, &snapshot, target).map_err(callback_refusal)?;
            resolved.as_str()
        };
        if is_descendant(tx, snapshot.context().agent.as_str(), target)? {
            let text = args["text"]
                .as_str()
                .ok_or(crate::managed_host_tools::Error::Invalid(
                    "Message must have text",
                ))?;
            let result = crate::coordination::send_followup(
                tx,
                crate::coordination::AgentFollowup {
                    sender_id: snapshot.context().agent.as_str(),
                    sender_epoch: self.context.epoch as i64,
                    recipient_id: target,
                    id: request_id,
                    text,
                    delivery: studio_runtime_schema::delivery::DeliveryMoment::for_event(
                        "followup",
                        text,
                        Some(args),
                    )
                    .map_err(|_| {
                        crate::managed_host_tools::Error::Invalid("Invalid delivery moment")
                    })?,
                    now,
                },
                &self.admission,
            )
            .map_err(|error| match error {
                crate::coordination::CoordinationError::Store(error) => {
                    crate::managed_host_tools::Error::Store(error)
                }
                crate::coordination::CoordinationError::RequiresDependency(name) => {
                    crate::managed_host_tools::Error::Unavailable(name)
                }
                _ => crate::managed_host_tools::Error::Invalid("Agent input was refused"),
            })?;
            return Ok(crate::managed_host_tools::OperationResult {
                value: result.receipt,
                signals: if result.event_inserted {
                    vec![crate::managed_host_tools::WakeSignal::new(
                        "account-worker/queued-event",
                    )]
                } else {
                    Vec::new()
                },
            });
        }
        // Runtime.dynamic routes non-descendants through chat_message, which
        // supplies its default importance and never accepts progress metadata.
        let chat_args = json!({"target":target,"text":args["text"],"delivery":args["delivery"]});
        let value =
            messages::send(tx, &snapshot, &chat_args, request_id, now).map_err(callback_refusal)?;
        Ok(crate::managed_host_tools::OperationResult {
            value,
            signals: Vec::new(),
        })
    }
}

/// The agent whose authority cancels a command watch. The owner cancels its
/// own watch. An ancestor may cancel a descendant's watch, as it may interrupt
/// or archive that descendant. Every other caller stays itself, so
/// `monitor_policy::cancel` refuses it as another agent's command.
fn monitor_cancel_owner(
    tx: &Transaction<'_>,
    caller: &str,
    monitor_id: &str,
) -> Result<String, crate::managed_host_tools::Error> {
    let owner =
        studio_runtime_store::MonitorsRepository::get(tx, monitor_id)?.and_then(|monitor| {
            monitor
                .value()
                .get("agent")
                .and_then(Value::as_str)
                .map(str::to_owned)
        });
    match owner {
        Some(owner) if owner != caller && is_descendant(tx, caller, &owner)? => Ok(owner),
        _ => Ok(caller.to_owned()),
    }
}

/// Keep a named refusal visible to the caller. A stored-record refusal names
/// its location, which holds a table key, a record ID and a field name, never
/// a stored value. Other failures keep their generic text.
fn callback_refusal(error: CallbackError) -> crate::managed_host_tools::Error {
    match error {
        CallbackError::Store(error) => crate::managed_host_tools::Error::Store(error),
        CallbackError::Dependency(name) => crate::managed_host_tools::Error::Unavailable(name),
        CallbackError::Invalid(message) => crate::managed_host_tools::Error::Invalid(message),
        CallbackError::Stored(location) => crate::managed_host_tools::Error::StoredAt(location),
        _ => crate::managed_host_tools::Error::Invalid("Message ingress refused this request"),
    }
}

fn is_descendant(
    tx: &Transaction<'_>,
    sender_id: &str,
    target_id: &str,
) -> Result<bool, crate::managed_host_tools::Error> {
    let records = records::all(tx, "runtime_agents")?;
    let mut by_id = std::collections::BTreeMap::new();
    for record in records {
        let id = record
            .get("id")
            .and_then(Value::as_str)
            .ok_or(crate::managed_host_tools::Error::Stale)?;
        by_id.insert(id.to_owned(), record);
    }
    let Some(mut cursor) = by_id.get(target_id) else {
        return Ok(false);
    };
    let mut visited = std::collections::BTreeSet::new();
    while let Some(parent) = cursor.get("parentId").and_then(Value::as_str) {
        if parent == sender_id {
            return Ok(true);
        }
        if !visited.insert(parent.to_owned()) {
            return Err(crate::managed_host_tools::Error::Stale);
        }
        let Some(next) = by_id.get(parent) else {
            return Ok(false);
        };
        cursor = next;
    }
    Ok(false)
}

impl crate::managed_host_tools::ChildCreationPort for ProductionPorts {
    type Prepared = ();
    fn prepare_work_item(
        &self,
        _: &Transaction<'_>,
        _: &crate::managed_host_tools::Actor,
        _: &str,
        _: &Value,
        _: f64,
    ) -> Result<(), crate::managed_host_tools::Error> {
        Err(crate::managed_host_tools::Error::Unavailable(
            "agent-create/worker-driver",
        ))
    }
    fn recheck_worker(
        &self,
        _: &Transaction<'_>,
        _: &crate::managed_host_tools::Actor,
        _: &(),
    ) -> Result<(), crate::managed_host_tools::Error> {
        Err(crate::managed_host_tools::Error::Unavailable(
            "agent-create/worker-driver",
        ))
    }
    fn persisted_record(
        &self,
        _: &Transaction<'_>,
        _: &(),
    ) -> Result<Value, crate::managed_host_tools::Error> {
        Err(crate::managed_host_tools::Error::Unavailable(
            "agent-create/worker-driver",
        ))
    }
    fn wake_signal(&self, _: &()) -> crate::managed_host_tools::WakeSignal {
        crate::managed_host_tools::WakeSignal::new("worker-driver")
    }
}

impl crate::managed_host_tools::InterruptPort for ProductionPorts {
    fn resolve_target(
        &self,
        tx: &Transaction<'_>,
        _: &crate::managed_host_tools::Actor,
        supplied: &str,
    ) -> Result<String, crate::managed_host_tools::Error> {
        let snapshot = super::super::storage::snapshot(tx, &self.context)
            .map_err(|_| crate::managed_host_tools::Error::Stale)?;
        messages::resolve_agent(tx, &snapshot, supplied).map_err(|error| match error {
            CallbackError::Store(error) => crate::managed_host_tools::Error::Store(error),
            CallbackError::Stored(location) => crate::managed_host_tools::Error::StoredAt(location),
            CallbackError::Invalid(message) => crate::managed_host_tools::Error::Invalid(message),
            _ => crate::managed_host_tools::Error::Invalid("Unknown managed agent"),
        })
    }

    fn interrupt_in_transaction(
        &self,
        _: &Transaction<'_>,
        _: &crate::managed_host_tools::Actor,
        _: &str,
        _: &str,
        _: &str,
        _: f64,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        Err(crate::managed_host_tools::Error::Unavailable(
            "account-worker/agent-interrupt",
        ))
    }
}

impl crate::managed_host_tools::StatusPort for ProductionPorts {
    fn status(
        &self,
        _: &Transaction<'_>,
        _: &crate::managed_host_tools::Actor,
        _: &Value,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        Err(crate::managed_host_tools::Error::Unavailable(
            "model-directory/status-and-dispatch-capacity",
        ))
    }
}
impl crate::managed_host_tools::SearchPort for ProductionPorts {
    fn search(
        &self,
        tx: &Transaction<'_>,
        actor: &crate::managed_host_tools::Actor,
        args: &Value,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        let directory =
            self.room_directory
                .as_ref()
                .ok_or(crate::managed_host_tools::Error::Unavailable(
                    "transcript/search-directory",
                ))?;
        let query = args["query"]
            .as_str()
            .ok_or(crate::managed_host_tools::Error::Invalid(
                "Invalid search query",
            ))?;
        let limit = args["limit"].as_i64().unwrap_or(50);
        let value = crate::transcript::search_work_in_transaction(
            tx,
            directory,
            query,
            Some(&actor.record().id.0),
            limit,
        )
        .map_err(search_error)?;
        Ok(crate::managed_host_tools::OperationResult {
            value,
            signals: Vec::new(),
        })
    }
}
impl crate::managed_host_tools::ContextPort for ProductionPorts {
    fn context(
        &self,
        tx: &Transaction<'_>,
        actor: &crate::managed_host_tools::Actor,
        args: &Value,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        let topic = args["topic"].as_str().unwrap_or_default();
        let content = if topic == "monitor" {
            let id = args["id"]
                .as_str()
                .ok_or(crate::managed_host_tools::Error::Invalid(
                    "Missing monitor id",
                ))?;
            let (snapshot, monitor_actor) =
                crate::monitor_runtime::snapshot(tx, id).map_err(monitor_context_error)?;
            if monitor_actor.root_id != actor.record().root_id {
                return Err(crate::managed_host_tools::Error::Invalid(
                    "Unknown monitor in this team",
                ));
            }
            snapshot.record().clone()
        } else {
            context::topic(
                tx,
                actor,
                topic,
                &self.state_root,
                self.preparation.as_deref(),
            )?
        };
        let version = studio_runtime_store::sha256_hex(
            studio_runtime_store::python_json_compact(&content).as_bytes(),
        )[..24]
            .to_owned();
        Ok(crate::managed_host_tools::OperationResult {
            value: json!({"topic":topic,"version":version,"content":content}),
            signals: Vec::new(),
        })
    }
}
impl crate::managed_host_tools::ComplaintPort for ProductionPorts {
    fn complaint(
        &self,
        tx: &Transaction<'_>,
        actor: &crate::managed_host_tools::Actor,
        args: &Value,
        request_id: &str,
        now: f64,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        let actor = serde_json::to_value(actor.record())
            .map_err(|_| crate::managed_host_tools::Error::Invalid("Invalid complaint actor"))?;
        let value =
            crate::notification_dispatch::agent_complaint_action(tx, &actor, args, request_id, now)
                .map_err(|error| match error {
                    crate::notification_dispatch::NotificationDispatchError::Store(error) => {
                        crate::managed_host_tools::Error::Store(error)
                    }
                    crate::notification_dispatch::NotificationDispatchError::ConflictingInput => {
                        crate::managed_host_tools::Error::Invalid(if args["action"] == "submit" {
                            "This complaint id has different content"
                        } else {
                            "This response id has different content"
                        })
                    }
                    crate::notification_dispatch::NotificationDispatchError::Dependency(name) => {
                        crate::managed_host_tools::Error::Unavailable(name)
                    }
                    _ => crate::managed_host_tools::Error::Invalid("Complaint action was refused"),
                })?;
        let should_wake = (args["action"] == "submit" && value["recipient"] == "lead")
            || (args["action"] == "respond" && value["author"] != actor["id"]);
        Ok(crate::managed_host_tools::OperationResult {
            value,
            signals: if should_wake {
                vec![crate::managed_host_tools::WakeSignal::new(
                    "event-driver/complaint-notification",
                )]
            } else {
                Vec::new()
            },
        })
    }
}
impl crate::managed_host_tools::WatchMonitorPort for ProductionPorts {
    fn watch(
        &self,
        _: &Transaction<'_>,
        _: &crate::managed_host_tools::Actor,
        _: &Value,
        _: &str,
        _: f64,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        Err(crate::managed_host_tools::Error::Unavailable(
            "rules/runtime-driver",
        ))
    }
    fn monitor(
        &self,
        tx: &Transaction<'_>,
        actor: &crate::managed_host_tools::Actor,
        args: &Value,
        _: &str,
        now: f64,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        if args["action"].as_str() != Some("cancel") {
            return Err(crate::managed_host_tools::Error::Unavailable(
                "workspace-state/monitor-admission",
            ));
        }
        let id = args["monitor_id"]
            .as_str()
            .ok_or(crate::managed_host_tools::Error::Invalid(
                "Missing monitor_id",
            ))?;
        let owner = monitor_cancel_owner(tx, &actor.record().id.0, id)?;
        let (value, operation) = crate::monitor_driver::reserve_cancel(tx, id, Some(&owner), now)
            .map_err(|error| match error {
            crate::monitor_policy::MonitorError::Store(error) => {
                crate::managed_host_tools::Error::Store(error)
            }
            crate::monitor_policy::MonitorError::Unavailable(name) => {
                crate::managed_host_tools::Error::Unavailable(name)
            }
            crate::monitor_policy::MonitorError::Refused(reason) => {
                crate::managed_host_tools::Error::Invalid(reason)
            }
            _ => crate::managed_host_tools::Error::Invalid("Monitor cancellation was refused"),
        })?;
        let signals = operation
            .into_iter()
            .map(|_| crate::managed_host_tools::WakeSignal::new("monitor-driver"))
            .collect();
        Ok(crate::managed_host_tools::OperationResult { value, signals })
    }
}
impl crate::managed_host_tools::ReviewPort for ProductionPorts {
    fn review(
        &self,
        tx: &Transaction<'_>,
        actor: &crate::managed_host_tools::Actor,
        args: &Value,
        request_id: &str,
        _: f64,
    ) -> Result<crate::managed_host_tools::OperationResult, crate::managed_host_tools::Error> {
        let root =
            studio_runtime_store::ContextRepairRepository::agent(tx, &actor.record().root_id.0)?
                .ok_or(crate::managed_host_tools::Error::Stale)?;
        let directory = actor
            .record()
            .extra
            .get("cwd")
            .and_then(Value::as_str)
            .ok_or(crate::managed_host_tools::Error::Unavailable(
                "agent-create/worker-driver",
            ))?;
        let key = args
            .get("request_id")
            .and_then(Value::as_str)
            .unwrap_or(request_id)
            .to_owned();
        let plan = crate::native_review::ReviewPlan::from_records(
            serde_json::to_value(actor.record())
                .map_err(|_| crate::managed_host_tools::Error::Invalid("Invalid reviewer"))?,
            root,
            args.clone(),
            key,
            directory.to_owned(),
        )
        .map_err(|_| crate::managed_host_tools::Error::Invalid("Invalid native review request"))?;
        let value = crate::native_review::reserve(tx, &plan, &UnavailableReviewCreator).map_err(
            |error| match error {
                crate::native_actions::NativeError::Unavailable(name) => {
                    crate::managed_host_tools::Error::Unavailable(name)
                }
                crate::native_actions::NativeError::Store(error) => {
                    crate::managed_host_tools::Error::Store(error)
                }
                _ => crate::managed_host_tools::Error::Unavailable("agent-create/worker-driver"),
            },
        )?;
        Ok(crate::managed_host_tools::OperationResult {
            value,
            signals: Vec::new(),
        })
    }
}

struct UnavailableReviewCreator;
impl crate::native_review::ReviewCreator for UnavailableReviewCreator {
    fn child(
        &self,
        _: &Transaction<'_>,
        _: &Value,
        _: &Value,
    ) -> crate::native_actions::Result<Value> {
        Err(crate::native_actions::NativeError::Unavailable(
            "agent-create/worker-driver",
        ))
    }
    fn receipt(
        &self,
        _: &Transaction<'_>,
        _: &str,
        _: &Value,
    ) -> crate::native_actions::Result<()> {
        Err(crate::native_actions::NativeError::Unavailable(
            "agent-create/worker-driver",
        ))
    }
}
impl crate::managed_host_tools::PostCommitPort for ProductionPorts {
    fn dispatch_managed<'a>(
        &'a self,
        store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
        actor: &'a crate::managed_host_tools::Actor,
        tool: &'a str,
        args: &'a Value,
        request_id: &'a str,
        now: f64,
    ) -> BoxFuture<
        'a,
        Result<
            Option<crate::managed_host_tools::DispatchedOperation>,
            crate::managed_host_tools::Error,
        >,
    > {
        Box::pin(async move {
            super::runtime_dispatch::dispatch(super::runtime_dispatch::DispatchRequest {
                store,
                actor,
                context: &self.context,
                tool,
                args,
                request_id,
                now,
                ports: &self.runtime_ports,
            })
            .await
        })
    }

    fn release(
        &self,
        signals: &[crate::managed_host_tools::WakeSignal],
    ) -> Result<(), crate::managed_host_tools::Error> {
        for signal in signals {
            if signal.owner() == "monitor-driver" {
                self.monitor_dispatch_wake.notify_one();
            } else {
                // The account registry re-reads committed rows and wakes the
                // owning worker; durable intents are also recovered on ticks.
                self.event_wake.notify_one();
            }
        }
        Ok(())
    }
}

pub(super) async fn resolve_interrupt_target(
    store: &studio_runtime_store::RuntimeStore,
    context: &CallbackContext,
    supplied: &str,
) -> Result<String, crate::managed_host_tools::Error> {
    let context = context.clone();
    let supplied = supplied.to_owned();
    store
        .transaction_with(move |tx| {
            let snapshot = super::super::storage::snapshot(tx, &context)
                .map_err(|_| crate::managed_host_tools::Error::Stale)?;
            let target =
                messages::resolve_agent(tx, &snapshot, &supplied).map_err(|error| match error {
                    CallbackError::Store(error) => crate::managed_host_tools::Error::Store(error),
                    CallbackError::Stored(location) => {
                        crate::managed_host_tools::Error::StoredAt(location)
                    }
                    CallbackError::Invalid(message) => {
                        crate::managed_host_tools::Error::Invalid(message)
                    }
                    _ => crate::managed_host_tools::Error::Invalid("Unknown managed agent"),
                })?;
            if !is_descendant(tx, &context.agent, &target)? {
                return Err(crate::managed_host_tools::Error::Invalid(
                    "You can interrupt only your descendants",
                ));
            }
            Ok(target)
        })
        .await
}

/// Python `dynamic` for `orchestration_send`: the visible target ID and
/// whether it is a descendant of the caller.
pub(super) async fn resolve_send_target(
    store: &studio_runtime_store::RuntimeStore,
    context: &CallbackContext,
    supplied: &str,
) -> Result<(String, bool), crate::managed_host_tools::Error> {
    let context = context.clone();
    let supplied = supplied.to_owned();
    store
        .transaction_with(move |tx| {
            let snapshot = super::super::storage::snapshot(tx, &context)
                .map_err(|_| crate::managed_host_tools::Error::Stale)?;
            let target =
                messages::resolve_agent(tx, &snapshot, &supplied).map_err(|error| match error {
                    CallbackError::Store(error) => crate::managed_host_tools::Error::Store(error),
                    CallbackError::Stored(location) => {
                        crate::managed_host_tools::Error::StoredAt(location)
                    }
                    CallbackError::Invalid(message) => {
                        crate::managed_host_tools::Error::Invalid(message)
                    }
                    _ => crate::managed_host_tools::Error::Invalid("Unknown managed agent"),
                })?;
            let descendant = is_descendant(tx, &context.agent, &target)?;
            Ok((target, descendant))
        })
        .await
}

impl From<event_write::EventWriteError> for CallbackError {
    fn from(error: event_write::EventWriteError) -> Self {
        use event_write::EventWriteError as E;
        match error {
            E::InvalidResultSchema(message) => Self::InvalidResultSchema(message),
            E::Store(error) => Self::Store(error),
            E::InvalidRequest(message) | E::Conflict(message) => Self::Invalid(message),
            E::WorkerAssignmentUnavailable => Self::Dependency("worker-driver/work-assignment"),
            _ => Self::Dependency("event-write/required-runtime-driver"),
        }
    }
}
impl From<crate::coordination::CoordinationError> for CallbackError {
    fn from(error: crate::coordination::CoordinationError) -> Self {
        use crate::coordination::CoordinationError as E;
        match error {
            E::Store(error) => Self::Store(error),
            E::InvalidRequest(message) | E::Conflict(message) => Self::Invalid(message),
            E::RequiresDependency(name) => Self::Dependency(name),
            E::InvalidStored { object, id } => Self::Stored(format!("{object}/{id}")),
            E::AdmissionDenied(_) => Self::Invalid("Team token budget admission denied"),
            E::BudgetRuntime(_) => Self::Invalid("Unable to verify team token budget"),
        }
    }
}
pub(super) fn read_only(name: &str, args: &Value) -> bool {
    (name == "orchestration_monitor"
        && args["monitor_id"].is_string()
        && args.get("command").is_none())
        || matches!(
            name,
            "orchestration_read"
                | "orchestration_peers"
                | "orchestration_search"
                | "orchestration_chat_read"
        )
        || name == "orchestration_task"
            && matches!(
                args["action"].as_str().unwrap_or("list"),
                "list" | "get" | "read" | "history"
            )
        || name == "orchestration_request"
            && matches!(args["action"].as_str().unwrap_or("list"), "list" | "get")
}
#[allow(clippy::too_many_arguments)]
pub(super) fn execute(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    kind: ToolKind,
    name: &str,
    args: &Value,
    id: &str,
    now: f64,
    state_root: &std::path::Path,
    ports: &(impl crate::managed_host_tools::Ports<Prepared = ()> + ?Sized),
) -> Result<crate::managed_host_tools::Staged, CallbackError> {
    let actor = snapshot.actor();
    if studio_runtime_store::truthy(actor.get("executionMove"))
        && !matches!(
            name,
            "orchestration_move" | "orchestration_teleport" | "orchestration_request"
        )
    {
        return Err(CallbackError::Invalid(
            "The move is accepted. Finish this turn before the next task",
        ));
    }
    // The managed-tool dispatcher sends a remote worker's home-server calls
    // first. Any such call that still arrives here had no dispatcher; the
    // worker's other tools run on this server, as in Python.
    if studio_runtime_store::truthy(actor.get("remoteOrigin")) {
        let record = studio_runtime_store::AgentRecord::from_value(actor.clone())
            .map_err(|_| CallbackError::Stale)?;
        if crate::multi_server_tools::worker_route(name, args, &record).is_some() {
            return Err(CallbackError::Dependency(
                super::runtime_dispatch::WORKER_CALL_DEPENDENCY,
            ));
        }
    }
    let mut resolved_args = args.clone();
    let field = match kind {
        ToolKind::Message => args["target"]
            .as_str()
            .filter(|target| {
                !matches!(*target, "user" | "parent" | "lead" | "broadcast" | "all")
                    && !target.starts_with("remote:")
                    && !target.starts_with("federated:")
            })
            .map(|_| "target"),
        ToolKind::Task => args["owner"]
            .as_str()
            .filter(|owner| !owner.is_empty())
            .map(|_| "owner"),
        _ => None,
    };
    if let Some(field) = field {
        resolved_args[field] = json!(messages::resolve_agent(tx, snapshot, text(args, field)?)?);
    }
    let args = &resolved_args;
    if matches!(kind, ToolKind::CancelMonitor) {
        let monitor_id = text(args, "monitor_id")?;
        let owner =
            monitor_cancel_owner(tx, &snapshot.context().agent, monitor_id).map_err(|error| {
                match error {
                    crate::managed_host_tools::Error::Store(error) => CallbackError::Store(error),
                    _ => CallbackError::Stale,
                }
            })?;
        let (value, operation) = crate::monitor_driver::reserve_cancel(
            tx,
            monitor_id,
            Some(&owner),
            now,
        )
        .map_err(|error| match error {
            crate::monitor_policy::MonitorError::Store(error) => CallbackError::Store(error),
            crate::monitor_policy::MonitorError::Unavailable(name) => {
                CallbackError::Dependency(name)
            }
            crate::monitor_policy::MonitorError::Refused(reason) => CallbackError::Invalid(reason),
            _ => CallbackError::Invalid("Monitor cancellation was refused"),
        })?;
        return Ok(crate::managed_host_tools::Staged {
            value,
            signals: operation
                .into_iter()
                .map(|_| crate::managed_host_tools::WakeSignal::new("monitor-driver"))
                .collect(),
        });
    }
    if matches!(kind, ToolKind::MonitorInput) {
        return Err(CallbackError::Dependency("monitor-input-request-identity"));
    }
    if matches!(kind, ToolKind::AgentManage) {
        let (value, signals) = manage::execute(tx, snapshot, args, now)?;
        return Ok(crate::managed_host_tools::Staged { value, signals });
    }
    match kind {
        ToolKind::Spawn
        | ToolKind::Send
        | ToolKind::Interrupt
        | ToolKind::Status
        | ToolKind::Search
        | ToolKind::Context
        | ToolKind::Review
        | ToolKind::Monitor
        | ToolKind::Watch
        | ToolKind::Complaint => {
            let managed_actor = crate::managed_host_tools::Actor::from_record(
                actor,
                &snapshot.context().agent,
                snapshot.context().epoch,
            )
            .map_err(managed_error)?;
            let result =
                crate::managed_host_tools::execute(tx, &managed_actor, name, args, id, now, ports)
                    .map_err(managed_error)?;
            return Ok(result);
        }
        _ => {}
    }
    let value = match kind {
        ToolKind::Request => Ok(tool_requests::execute(
            tx,
            &snapshot.context().agent,
            match args["action"].as_str().unwrap_or("list") {
                "list" => tool_requests::Action::List,
                "get" => tool_requests::Action::Get(text(args, "request_id")?),
                "cancel" => tool_requests::Action::Cancel(text(args, "request_id")?),
                _ => return Err(CallbackError::Invalid("Invalid request action")),
            },
            now,
        )?),
        ToolKind::Task => work::execute(tx, snapshot, args, id),
        ToolKind::Message => messages::send(tx, snapshot, args, id, now),
        ToolKind::ChatRead => reads::chat(tx, snapshot, args),
        ToolKind::Peers => messages::peers(tx, snapshot, args),
        ToolKind::Read => reads::read(tx, snapshot, args, now, state_root),
        ToolKind::Status
        | ToolKind::Search
        | ToolKind::Context
        | ToolKind::Send
        | ToolKind::Spawn
        | ToolKind::Review
        | ToolKind::Monitor
        | ToolKind::Interrupt
        | ToolKind::Watch
        | ToolKind::Complaint => Err(CallbackError::Invalid(
            "Managed tool dispatcher missed a tool",
        )),
        ToolKind::MonitorInput => Err(CallbackError::Dependency("monitor-input-request-identity")),
        ToolKind::CancelMonitor => Err(CallbackError::Invalid(
            "Monitor cancellation dispatch was not handled",
        )),
        ToolKind::Move | ToolKind::Teleport => Err(CallbackError::Dependency(
            "agent-move/exact-native-history-and-paired-driver",
        )),
        ToolKind::Servers => Err(CallbackError::Dependency("multi-server/tool-coordinator")),
        ToolKind::AgentManage => Err(CallbackError::Invalid(
            "Agent management dispatch was not handled",
        )),
        ToolKind::Speak => Err(CallbackError::Dependency("voice/provider-session")),
        // The managed-tool dispatcher sends every sign-in link to the browser bridge.
        ToolKind::SignIn => Err(CallbackError::Dependency(
            super::runtime_dispatch::SIGN_IN_DEPENDENCY,
        )),
        ToolKind::HostExec => Err(CallbackError::Dependency("host-exec/layr-bridge")),
        ToolKind::Title => {
            if actor["isLead"] != true {
                return Err(CallbackError::Invalid(
                    "Only the orchestrator can set the conversation title",
                ));
            }
            let title = text(args, "title")?.trim();
            if title.is_empty() || title.chars().count() > 80 {
                return Err(CallbackError::Invalid(
                    "Only a lead can set a title of 1 to 80 characters",
                ));
            }
            let mut updated = actor.clone();
            if updated["manualName"] != true {
                updated["name"] = json!(title);
            }
            updated["needsTitle"] = json!(false);
            // Python puts the agent row; the projection alone would be lost.
            super::super::storage::save_actor(tx, &updated)?;
            Ok(json!({"title":updated["name"]}))
        }
        ToolKind::Unknown => Err(CallbackError::Invalid(
            "Unknown orchestration tool; use a tool from the session catalog",
        )),
    }?;
    Ok(crate::managed_host_tools::Staged {
        value,
        signals: Vec::new(),
    })
}

pub(super) fn managed_error(error: crate::managed_host_tools::Error) -> CallbackError {
    use crate::managed_host_tools::Error as E;
    match error {
        E::Invalid(message) => CallbackError::Invalid(message),
        E::Stale => CallbackError::Stale,
        E::InvalidStored {
            table,
            id,
            location,
        } => CallbackError::Stored(format!("{table}/{id} at {location}")),
        E::StoredAt(location) => CallbackError::Stored(location),
        E::Unavailable(name) => CallbackError::Dependency(name),
        E::Store(error) => CallbackError::Store(error),
        E::Sqlite(error) => CallbackError::Store(error.into()),
        E::Work(crate::work_write::WorkWriteError::Event(error)) => error.into(),
        E::Work(crate::work_write::WorkWriteError::Store(error)) => CallbackError::Store(error),
        E::Work(crate::work_write::WorkWriteError::NotImplemented(name)) => {
            CallbackError::Dependency(name)
        }
        E::Work(crate::work_write::WorkWriteError::PreparationWorker) => {
            CallbackError::Dependency("work-write/preparation")
        }
    }
}

fn search_error(error: crate::transcript::TranscriptError) -> crate::managed_host_tools::Error {
    use crate::managed_host_tools::Error as E;
    use crate::transcript::TranscriptError as T;
    match error {
        T::InvalidSearch => E::Invalid("Invalid search query"),
        T::InvalidStoredObject { table, id } => {
            E::Store(studio_runtime_store::StoreError::InvalidStoredObject { table, id })
        }
        T::InvalidStoredState { table, id } => {
            E::Store(studio_runtime_store::StoreError::InvalidSyncObject { scope: table, id })
        }
        T::RequiresDependency(name) => E::Unavailable(name),
        T::Store => E::Store(studio_runtime_store::StoreError::SchemaMismatch(
            "transcript storage failed".into(),
        )),
        _ => E::Invalid("Search result is unavailable"),
    }
}

fn monitor_context_error(
    error: crate::monitor_policy::MonitorError,
) -> crate::managed_host_tools::Error {
    use crate::managed_host_tools::Error as E;
    use crate::monitor_policy::MonitorError as M;
    match error {
        M::Store(error) => E::Store(error),
        M::InvalidStored { id, location } => E::InvalidStored {
            table: "runtime_monitors",
            id,
            location,
        },
        M::Unavailable(name) => E::Unavailable(name),
        M::Event(crate::event_write::EventWriteError::Store(error)) => E::Store(error),
        _ => E::Invalid("Monitor context is unavailable"),
    }
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, CallbackError> {
    v[key]
        .as_str()
        .ok_or(CallbackError::Invalid("Missing required text argument"))
}
fn brief(value: &Value) -> Value {
    let fields = ["id", "title", "owner", "status", "version", "blockedBy"];
    Value::Object(
        fields
            .into_iter()
            .map(|key| (key.into(), value[key].clone()))
            .collect(),
    )
}
fn page(rows: Vec<Value>, args: &Value, scope: Value) -> Result<Value, CallbackError> {
    let mut revision_rows = rows.clone();
    for row in &mut revision_rows {
        if row["entry"] == "agent"
            && let Some(value) = row["value"].as_object_mut()
        {
            value.remove("status");
        }
    }
    paginate(rows, &revision_rows, args, scope, Some(MODEL_PAGE_BYTES))
}

/// Model text bytes of one list page (Python `model_text_bytes` limit).
const MODEL_PAGE_BYTES: usize = 13000;

/// Python `model_page(rows, args, scope, byte_limit)`.
fn model_page(
    rows: Vec<Value>,
    args: &Value,
    scope: Value,
    byte_limit: Option<usize>,
) -> Result<Value, CallbackError> {
    let revision_rows = rows.clone();
    paginate(rows, &revision_rows, args, scope, byte_limit)
}

fn paginate(
    rows: Vec<Value>,
    revision_rows: &[Value],
    args: &Value,
    scope: Value,
    byte_limit: Option<usize>,
) -> Result<Value, CallbackError> {
    use base64::Engine;
    let limit = match args.get("limit") {
        None => 20,
        Some(v) => v
            .as_u64()
            .ok_or(CallbackError::Invalid("limit must be 1 to 50"))? as usize,
    };
    if !(1..=50).contains(&limit) {
        return Err(CallbackError::Invalid("limit must be 1 to 50"));
    }
    let revision =
        studio_runtime_store::sha256_hex(json!([scope, revision_rows]).to_string().as_bytes())
            [..24]
            .to_owned();
    let offset = if let Some(cursor) = args["cursor"].as_str() {
        let invalid = || {
            CallbackError::Invalid("List changed or cursor is invalid. Read the first page again.")
        };
        let raw = base64::engine::general_purpose::URL_SAFE
            .decode(cursor)
            .map_err(|_| invalid())?;
        let cursor: Value = serde_json::from_slice(&raw).map_err(|_| invalid())?;
        let offset = cursor["offset"].as_u64().ok_or_else(invalid)? as usize;
        if offset > rows.len() || cursor["revision"] != revision {
            return Err(invalid());
        }
        offset
    } else {
        0
    };
    let mut end = (offset + limit).min(rows.len());
    while let Some(byte_limit) = byte_limit
        && end > offset + 1
        && json!([{"type":"inputText","text":json!(rows[offset..end]).to_string()}])
            .to_string()
            .len()
            > byte_limit
    {
        end -= 1;
    }
    let cursor = (end < rows.len()).then(|| {
        base64::engine::general_purpose::URL_SAFE
            .encode(json!({"offset":end,"revision":revision}).to_string())
    });
    Ok(
        json!({"apiVersion":2,"items":rows[offset..end],"total":rows.len(),"revision":revision,"nextCursor":cursor}),
    )
}
