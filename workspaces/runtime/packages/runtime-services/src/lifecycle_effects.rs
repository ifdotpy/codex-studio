//! Durable production adapters for stop and delete lifecycle effects.
//!
//! Interrupt intents are written with the agent lifecycle transaction. The
//! provider broker is called only after commit, and an ambiguous result remains
//! Unknown and is never retried by this adapter.
//!
//! Ported effects: Runtime.stop in `codex_runtime.py:10984-11021`,
//! `capacity_reset` in `codex_capacity_retry.py:72-80`, and
//! `usage_resume_cancel` in `codex_usage_resume.py:375-381`;
//! Runtime.delete_conversation and
//! `_settle_deleted_conversations` in `codex_runtime.py:5121-5250`;
//! `workspace_summary` / `child_stopped_event` in
//! `codex_runtime.py:7671-7745`; and
//! `release_failed_work` in `codex_work.py:1313-1355`.

use crate::account_session_runtime::SessionOutcome as ReceiptOutcome;
use std::collections::HashSet;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, AgentStatus, ContinuationRepository, ContinuationTable,
    RuntimeStore, SchedulerRepository, Transaction,
};
use tokio::sync::Notify;

use crate::{
    account_session_runtime::{
        AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest, ProviderResponse,
    },
    account_worker::retained::RequestActor,
    agent_lifecycle_runtime::{LifecycleEffect, LifecycleEffectKind, LifecyclePorts},
    agent_management::{Action, ManagementError},
    diagnostics::measure_runtime_stage_async,
    event_write,
    monitor_lifecycle::MonitorLifecyclePorts,
    multi_server_transport::{
        self, HttpsPairedTransport, OrchestrationDriver, PairedTransport,
        RuntimeOrchestrationEffects,
    },
    sync_policy, work_write,
};

/// Recovery shares stop/delete's canonical parent notification and failed-work
/// release owners. These writes run inside RecoveryStore's transaction.
pub struct ProductionRecoveryDelivery {
    pub at: f64,
}

impl crate::recovery_store::RecoveryDelivery for ProductionRecoveryDelivery {
    fn start_held(
        &mut self,
        tx: &Transaction<'_>,
        agent: &Value,
        _: &str,
        event_id: &str,
    ) -> Result<(), crate::recovery::RecoveryError> {
        let effect = LifecycleEffect::new(
            LifecycleEffectKind::NotifyParent,
            agent.clone(),
            agent.clone(),
            self.at,
        );
        notify_parent_with_id(tx, &effect, Some(event_id))
            .map_err(|_| crate::recovery::RecoveryError::CommitFailed)
    }

    fn terminal(
        &mut self,
        tx: &Transaction<'_>,
        agent: &mut Value,
        turn: &Value,
        _: crate::recovery::CompletionMode,
    ) -> Result<(), crate::recovery::RecoveryError> {
        let mut owner = AgentRecord::from_value(agent.clone())
            .map_err(|_| crate::recovery::RecoveryError::CommitFailed)?;
        studio_runtime_store::SchedulerRepository::wait_for_owned_monitors(tx, &mut owner)?;
        *agent = owner.value();
        let mut before = agent.clone();
        before["turnId"] = turn["id"].clone();
        let effect = LifecycleEffect::new(
            LifecycleEffectKind::NotifyParent,
            before,
            agent.clone(),
            self.at,
        );
        // Like Python connection recovery, a completed turn reports its result
        // under the turn-completion id; any other outcome is a stopped child.
        if turn["status"] == "completed" {
            let record = AgentRecord::from_value(agent.clone())
                .map_err(|_| crate::recovery::RecoveryError::CommitFailed)?;
            let turn_id = turn["id"].as_str().unwrap_or_default();
            notify_parent_of_settled_turn(tx, &record, turn_id, self.at)?;
        } else {
            notify_parent(tx, &effect).map_err(|_| crate::recovery::RecoveryError::CommitFailed)?;
        }
        release_work(tx, &effect).map_err(|_| crate::recovery::RecoveryError::CommitFailed)
    }
}

const INTERRUPT_TIMEOUT: Duration = Duration::from_secs(10);
const VOICE_DELETE_RETRY_ATTEMPTS: usize = 8;

#[derive(Clone, Debug, PartialEq, Eq)]
enum InterruptWriteResult {
    Accepted(Value),
    Rejected(Value),
    NotSent,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptRequest {
    pub ticket: String,
    pub root_request_id: String,
    pub agent: String,
    pub epoch: i64,
    pub thread: String,
    pub turn: String,
    pub scope: ConnectionScope,
    /// The provider request that ends the thread's command processes which
    /// outlive the interrupt (see `studio_runtime_providers::stop_terminals_method`).
    pub terminals: Option<TerminalCleanup>,
}

/// One post-interrupt request that ends a stopped thread's surviving commands.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TerminalCleanup {
    pub operation: String,
    pub method: studio_runtime_providers::Method,
}

/// Operation prefixes of a stop's provider requests; one stop signature names both.
const INTERRUPT_OPERATION_PREFIX: &str = "lifecycle-interrupt:";
const TERMINALS_OPERATION_PREFIX: &str = "lifecycle-terminals:";

/// The command cleanup operation of the stop whose interrupt ticket is
/// `ticket`, or `None` when the ticket is not a stop's. A worker that sends a
/// stop's unsettled requests later uses these exact operation ids, so the
/// retained ledger sends each of them at most once.
pub(crate) fn stop_terminals_operation(ticket: &str) -> Option<String> {
    ticket
        .strip_prefix(INTERRUPT_OPERATION_PREFIX)
        .filter(|signature| !signature.is_empty())
        .map(|signature| format!("{TERMINALS_OPERATION_PREFIX}{signature}"))
}

/// Production LifecyclePorts constructor used by stop/delete callers.
pub struct ProductionLifecyclePorts {
    store: Arc<RuntimeStore>,
    monitors: MonitorLifecyclePorts,
    monitor_wake: Arc<Notify>,
    lifecycle_wake: Arc<Notify>,
    continuation_wake: Option<Arc<Notify>>,
    interrupt_broker: Arc<dyn AccountSessionPort>,
    remote_transport: Arc<dyn PairedTransport>,
    voice_provider: Option<Arc<dyn crate::voice_runtime::VoiceRequestPort>>,
    prepared: Mutex<Vec<InterruptRequest>>,
    voice_dispatching: Arc<Mutex<HashSet<String>>>,
}

impl ProductionLifecyclePorts {
    pub fn new(
        store: Arc<RuntimeStore>,
        monitor_wake: Arc<Notify>,
        lifecycle_wake: Arc<Notify>,
        interrupt_broker: Arc<dyn AccountSessionPort>,
        voice_provider: Option<Arc<dyn crate::voice_runtime::VoiceRequestPort>>,
    ) -> Self {
        let remote_transport: Arc<dyn PairedTransport> =
            Arc::new(HttpsPairedTransport::new(Arc::clone(&store)));
        let ports = Self {
            store,
            monitors: MonitorLifecyclePorts::new(monitor_wake.clone()),
            monitor_wake,
            lifecycle_wake,
            continuation_wake: None,
            interrupt_broker,
            remote_transport,
            voice_provider,
            prepared: Mutex::new(Vec::new()),
            voice_dispatching: Arc::new(Mutex::new(HashSet::new())),
        };
        // Startup callers invoke committed() to reconcile durable tombstones;
        // execute() also retries reconciliation before accepting a new action.
        ports
    }

    /// Inject a fake paired peer for deterministic remote lifecycle tests.
    pub fn with_remote_transport(mut self, transport: Arc<dyn PairedTransport>) -> Self {
        self.remote_transport = transport;
        self
    }

    pub fn with_continuation_wake(mut self, wake: Arc<Notify>) -> Self {
        self.continuation_wake = Some(wake);
        self
    }

    /// Run the production stop/delete adapter. Effect intents and lifecycle
    /// rows commit together; broker dispatch begins only after that commit.
    pub async fn execute(
        self: &Arc<Self>,
        action: &Action,
        now: f64,
    ) -> Result<Value, ManagementError> {
        // Retry startup settlement here and surface storage failures before
        // making a new lifecycle decision.
        self.reconcile_deleted_tombstones().await?;
        let concrete_ports = Arc::clone(self);
        let lifecycle_ports: Arc<dyn LifecyclePorts> = concrete_ports;
        let result = crate::agent_management_store::execute_prepared(
            Arc::clone(&self.store),
            action,
            now,
            Some(lifecycle_ports),
        )
        .await?;
        self.committed().await?;
        Ok(result)
    }

    fn stage_interrupt(
        &self,
        tx: &Transaction<'_>,
        effect: &LifecycleEffect,
    ) -> Result<(), ManagementError> {
        let before = effect.before();
        let agent = required_str(before, "id", "Invalid interrupt owner")?;
        let Some(thread) = before
            .get("threadId")
            .and_then(Value::as_str)
            .filter(|thread| !thread.is_empty())
        else {
            return Ok(());
        };
        let attempt = before.get("startAttempt").unwrap_or(&Value::Null);
        // Python tests the stored review object for truth.
        let is_review = studio_runtime_store::truthy(before.get("nativeReview"))
            && attempt.get("action").and_then(Value::as_str) == Some("review")
            && attempt.get("submitted").and_then(Value::as_bool) == Some(true)
            && studio_runtime_store::truthy(before.get("inFlight"))
            && attempt.get("threadId").and_then(Value::as_str) == Some(thread)
            && before.get("turnId").is_none_or(Value::is_null);
        let current_turn = before.get("turnId").and_then(Value::as_str).filter(|turn| {
            !turn.is_empty() && studio_runtime_store::truthy(before.get("inFlight"))
        });
        let Some(turn) = current_turn.or_else(|| is_review.then_some("")) else {
            // Python only interrupts an active native turn (or its special
            // submitted review). Preparation and idle stops need no provider IO.
            return Ok(());
        };
        if attempt.get("interrupt").is_some_and(|saved| {
            saved.get("threadId").and_then(Value::as_str) == Some(thread)
                && saved.get("turnId").and_then(Value::as_str) == Some(turn)
                && matches!(
                    saved.get("outcome").and_then(Value::as_str),
                    Some("unknown" | "accepted")
                )
        }) {
            // SchedulerRepository also refuses uncertain tickets. Treat the
            // existing intent as already handled locally so a repeated stop
            // can commit without resending it.
            return Ok(());
        }
        let generation = before
            .get("connectionGeneration")
            .and_then(Value::as_u64)
            .or_else(|| attempt.get("connectionGeneration").and_then(Value::as_u64))
            .ok_or(ManagementError::Unavailable(
                "account-worker/agent-interrupt",
            ))?;
        let account = attempt
            .get("accountKey")
            .and_then(Value::as_str)
            .or_else(|| before.get("accountKey").and_then(Value::as_str))
            .unwrap_or("default")
            .to_owned();
        let stored_connection = attempt
            .get("connectionId")
            .and_then(Value::as_str)
            .or_else(|| before.get("connectionId").and_then(Value::as_str))
            .ok_or(ManagementError::Unavailable(
                "account-worker/agent-interrupt",
            ))?;
        // The provider generation names the app-server process that runs the
        // turn. A replacement account worker attaches to that same process with
        // a new connection id, and the turn keeps running there, so address the
        // live connection of that generation. A request bound to the replaced
        // connection id is refused before it is written, and the turn and its
        // commands would keep running. Another generation keeps the stored id.
        let connection = match self.interrupt_broker.current_scope(&account) {
            Some(live) if live.account == account && live.generation == generation => {
                live.connection
            }
            _ => stored_connection.to_owned(),
        };
        let stopped_epoch = effect
            .stopped()
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or(ManagementError::Invalid("Invalid stopped epoch"))?;
        let request_epoch = before
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or(ManagementError::Invalid("Invalid interrupt actor epoch"))?;
        let signature = event_write::request_signature(&json!([
            agent,
            stopped_epoch,
            generation,
            attempt.get("id"),
            &account,
            &connection,
            thread,
            turn
        ]));
        let ticket = format!("{INTERRUPT_OPERATION_PREFIX}{signature}");
        if !SchedulerRepository::prepare_interrupt(tx, agent, thread, turn, generation, &ticket)? {
            return Err(ManagementError::Unavailable(
                "account-worker/agent-interrupt",
            ));
        }
        let root_request_id = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let mut owner = AgentRepository::get(tx, &AgentId::from(agent))?
            .ok_or(ManagementError::Invalid("Missing interrupt owner"))?;
        owner
            .start_attempt
            .as_mut()
            .and_then(|attempt| attempt.interrupt.as_mut())
            .ok_or(ManagementError::Invalid("Missing interrupt intent"))?
            .root_request_id = Some(root_request_id.clone());
        AgentRepository::put(tx, &owner)?;
        let terminals = studio_runtime_providers::ProviderKind::of_field(before.get("provider"))
            .ok()
            .and_then(studio_runtime_providers::stop_terminals_method)
            .map(|method| TerminalCleanup {
                operation: format!("{TERMINALS_OPERATION_PREFIX}{signature}"),
                method,
            });
        let request = InterruptRequest {
            ticket,
            root_request_id,
            agent: agent.to_owned(),
            epoch: request_epoch,
            thread: thread.to_owned(),
            turn: turn.to_owned(),
            scope: ConnectionScope {
                account,
                connection,
                generation,
            },
            terminals,
        };
        self.prepared
            .lock()
            .map_err(|_| ManagementError::Invalid("Lifecycle interrupt queue unavailable"))?
            .push(request);
        Ok(())
    }

    async fn dispatch_prepared(&self) -> Result<(), ManagementError> {
        let pending = {
            let mut queue = self
                .prepared
                .lock()
                .map_err(|_| ManagementError::Invalid("Lifecycle interrupt queue unavailable"))?;
            std::mem::take(&mut *queue)
        };
        let mut dispatched = HashSet::new();
        for request in pending {
            // A failed transaction can leave only its in-memory candidate.
            // If the exact action is then retried and committed, coalesce the
            // duplicate ticket so one durable identity is dispatched once.
            if !dispatched.insert(request.ticket.clone()) {
                continue;
            }
            // A rollback leaves no durable matching receipt, so discard the
            // in-memory candidate without contacting the provider.
            let agent_id = AgentId::from(request.agent.as_str());
            let ticket = request.ticket.clone();
            let persisted =
                self.store
                    .read_async("runtime-services/lifecycle_effects", move |tx| {
                        let Some(agent) = AgentRepository::get(tx, &agent_id)? else {
                            return Ok::<_, studio_runtime_store::StoreError>(false);
                        };
                        let interrupt = agent
                            .start_attempt
                            .as_ref()
                            .and_then(|a| a.interrupt.as_ref());
                        Ok(interrupt.and_then(|v| v.request_ticket.as_deref())
                            == Some(ticket.as_str())
                            && interrupt.and_then(|v| v.outcome)
                                == Some(studio_runtime_store::InterruptOutcome::Unknown))
                    })
                    .await?;
            if !persisted {
                continue;
            }
            let outcome = request_interrupt(self.interrupt_broker.as_ref(), &request).await;
            let (state, error) = match &outcome {
                InterruptWriteResult::Accepted(_) => ("accepted", None),
                InterruptWriteResult::Rejected(_) => ("rejected", None),
                InterruptWriteResult::NotSent => ("unsent", None),
                InterruptWriteResult::Unknown => ("unknown", None),
            };
            let request_ticket = request.ticket.clone();
            let generation = request.scope.generation;
            let settled_outcome = outcome.clone();
            let settled_request = request.clone();
            self.store
                .transaction_with_error(move |tx| {
                    let response = match &settled_outcome {
                        InterruptWriteResult::Accepted(response) => response,
                        InterruptWriteResult::Rejected(error) => &error["data"]["interruptReceipt"],
                        InterruptWriteResult::NotSent | InterruptWriteResult::Unknown => {
                            &Value::Null
                        }
                    };
                    SchedulerRepository::settle_interrupt_inputs(
                        tx,
                        &settled_request.agent,
                        &settled_request.scope.account,
                        generation,
                        &settled_request.thread,
                        response,
                    )?;
                    SchedulerRepository::settle_interrupt_result(
                        tx,
                        &request_ticket,
                        generation,
                        state,
                        error,
                    )?;
                    Ok::<_, ManagementError>(())
                })
                .await?;
            // An interrupt ends the turn, not the commands it left running.
            // Unless the interrupt provably never left Studio, end them on the
            // same provider process. The request has its own operation id and
            // is sent once; a lost reply is not resent.
            if let Some(cleanup) = &request.terminals
                && outcome != InterruptWriteResult::NotSent
            {
                let cleaned =
                    request_terminal_cleanup(self.interrupt_broker.as_ref(), &request, cleanup)
                        .await;
                if !matches!(cleaned, InterruptWriteResult::Accepted(_)) {
                    eprintln!(
                        "stopped agent {} command cleanup on thread {} was not confirmed: {cleaned:?}",
                        request.agent, request.thread
                    );
                }
            }
        }
        Ok(())
    }

    async fn dispatch_remote_stops(&self) -> Result<(), ManagementError> {
        #[derive(serde::Deserialize, serde::Serialize)]
        #[serde(rename_all = "camelCase", deny_unknown_fields)]
        struct RemoteOperation {
            request_id: String,
            operation: Vec<String>,
            action: studio_runtime_schema::persistence::ServerOperationKind,
            payload: Box<serde_json::value::RawValue>,
        }
        let pending = self
            .store
            .read_async("runtime-services/lifecycle_effects", |tx| {
                crate::server_outbox::all(tx)
                    .map(|entries| {
                        entries
                            .into_iter()
                            .map(|entry| (entry.request_id, entry.target, entry.record.body))
                            .collect::<Vec<_>>()
                    })
                    .map_err(|error| match error {
                        crate::multi_server_auth::AccessError::Store(error) => error,
                        other @ (crate::multi_server_auth::AccessError::Refused { .. }
                        | crate::multi_server_auth::AccessError::Stored { .. }
                        | crate::multi_server_auth::AccessError::Sqlite(_)) => {
                            studio_runtime_store::StoreError::SchemaMismatch(other.to_string())
                        }
                    })
            })
            .await?;
        let driver = OrchestrationDriver::new(
            Arc::clone(&self.store),
            Arc::clone(&self.remote_transport),
            Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&self.store))),
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or_default();
        for (id, server, body) in pending {
            let Ok(envelope) = serde_json::from_str::<RemoteOperation>(&body) else {
                eprintln!("stored cross-server stop intent is invalid: {id}");
                continue;
            };
            if envelope.action != studio_runtime_schema::persistence::ServerOperationKind::Stop {
                continue;
            }
            let request = serde_json::to_value(&envelope)
                .map_err(|_| ManagementError::Invalid("Invalid cross-server stop intent"))?;
            if let Err(error) = driver.exchange_operation(&server, &request, now).await {
                eprintln!("cross-server stop remains queued ({id}): {error}");
            }
        }
        Ok(())
    }

    async fn reconcile_deleted_tombstones(&self) -> Result<(), ManagementError> {
        let pending = self
            .store
            .read_async("runtime-services/lifecycle_effects", |tx| {
                let mut pending = Vec::new();
                for (id, raw) in
                    studio_runtime_store::StartupRecoveryRepository::deleted_agents_maybe_unsettled(
                        tx,
                    )?
                {
                    let record: AgentRecord = serde_json::from_str(&raw)
                        .map_err(studio_runtime_store::StoreError::from)?;
                    if needs_deleted_settlement(&record) {
                        pending.push(id);
                    }
                }
                Ok::<_, studio_runtime_store::StoreError>(pending)
            })
            .await?;
        if pending.is_empty() {
            return Ok(());
        }
        self.store
            .transaction_with_error(move |tx| {
                for id in &pending {
                    finalize_deleted(tx, id)?;
                }
                Ok::<_, ManagementError>(())
            })
            .await?;
        Ok(())
    }
}

async fn request_interrupt(
    port: &dyn AccountSessionPort,
    request: &InterruptRequest,
) -> InterruptWriteResult {
    request_stop_work(
        port,
        request,
        &request.ticket,
        studio_runtime_providers::Method::TurnInterrupt,
        json!({"threadId":request.thread,"turnId":request.turn}),
    )
    .await
}

async fn request_terminal_cleanup(
    port: &dyn AccountSessionPort,
    request: &InterruptRequest,
    cleanup: &TerminalCleanup,
) -> InterruptWriteResult {
    request_stop_work(
        port,
        request,
        &cleanup.operation,
        cleanup.method,
        json!({"threadId":request.thread}),
    )
    .await
}

/// Send one request that ends a stopped turn's provider work on the stop's
/// exact scope, as the prior-epoch actor the retained worker authorizes.
async fn request_stop_work(
    port: &dyn AccountSessionPort,
    request: &InterruptRequest,
    operation: &str,
    method: studio_runtime_providers::Method,
    params: Value,
) -> InterruptWriteResult {
    let expected = request.scope.clone();
    let receipt = port
        .provider_request(ProviderRequest {
            account: request.scope.account.clone(),
            root_request_id: request.root_request_id.clone(),
            admitted_request: None,
            operation: operation.to_owned(),
            actor: Some(RequestActor {
                agent: request.agent.clone(),
                epoch: request.epoch,
            }),
            deleted_actor_session: None,
            expected_scope: Some(expected.clone()),
            method,
            params,
            deadline: Instant::now() + INTERRUPT_TIMEOUT,
        })
        .await;
    // The receipt names the exact operation; the retained worker derives the
    // wire request id from it, so the id is not compared here (as voice and
    // catalog reads do). A check against the operation text made every real
    // interrupt receipt look unknown.
    match receipt {
        Ok(receipt)
            if receipt.operation == operation && receipt.scope.account == expected.account =>
        {
            match receipt.outcome {
                ReceiptOutcome::Written { ticket, response }
                    if receipt.scope == expected
                        && ticket.generation == expected.generation
                        && ticket.method == method =>
                {
                    match response {
                        ProviderResponse::Reply(response) => {
                            InterruptWriteResult::Accepted(response)
                        }
                        ProviderResponse::Rejected(error) => InterruptWriteResult::Rejected(error),
                    }
                }
                ReceiptOutcome::WorkerNotSent { .. }
                | ReceiptOutcome::NoLiveAccountWorker
                | ReceiptOutcome::ExpectedScopeMismatch
                | ReceiptOutcome::DeadlineExpired
                | ReceiptOutcome::NotHandedOff => InterruptWriteResult::NotSent,
                ReceiptOutcome::Written { .. } | ReceiptOutcome::Unknown { .. } => {
                    InterruptWriteResult::Unknown
                }
            }
        }
        Err(AccountSessionError::Requires(_)) | Err(AccountSessionError::Refused(_)) => {
            InterruptWriteResult::NotSent
        }
        Err(_) | Ok(_) => InterruptWriteResult::Unknown,
    }
}

impl LifecyclePorts for ProductionLifecyclePorts {
    fn stage(&self, tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
        match effect.kind() {
            LifecycleEffectKind::Interrupt => self.stage_interrupt(tx, effect),
            LifecycleEffectKind::CancelResume => cancel_resume(tx, effect),
            LifecycleEffectKind::CancelMonitors => self.monitors.stage(tx, effect),
            LifecycleEffectKind::NotifyParent => notify_parent(tx, effect),
            LifecycleEffectKind::ReleaseWork => release_work(tx, effect),
            LifecycleEffectKind::FinalizeDeleted => {
                // The decision writes the tombstone and still-active fields
                // in this transaction; post-commit/restart scans consume it.
                required_str(effect.before(), "id", "Invalid deleted agent")?;
                Ok(())
            }
            LifecycleEffectKind::VoiceStop => self.stage_voice_stop(tx, effect),
            LifecycleEffectKind::RemoteStop => self.stage_remote_stop(tx, effect),
        }
    }

    fn stage_voice_delete(
        &self,
        tx: &Transaction<'_>,
        agent_id: &str,
    ) -> Result<(), ManagementError> {
        let intent = crate::voice::provider::stage_delete_sessions(tx, agent_id)
            .map_err(voice_effect_error)?;
        if !intent.is_empty() && self.voice_provider.is_none() {
            return Err(ManagementError::Unavailable("voice-driver/agent-stop"));
        }
        Ok(())
    }

    fn committed(&self) -> crate::agent_lifecycle_runtime::LifecycleCommitFuture<'_> {
        Box::pin(async move {
            measure_runtime_stage_async("ports.committed", async {
                self.monitor_wake.notify_one();
                self.lifecycle_wake.notify_one();
                if let Some(wake) = &self.continuation_wake {
                    wake.notify_one();
                }
                measure_runtime_stage_async(
                    "ports.committed.reconcile_tombstones_before",
                    self.reconcile_deleted_tombstones(),
                )
                .await?;
                measure_runtime_stage_async(
                    "ports.committed.dispatch_prepared",
                    self.dispatch_prepared(),
                )
                .await?;
                measure_runtime_stage_async(
                    "ports.committed.reconcile_tombstones_after",
                    self.reconcile_deleted_tombstones(),
                )
                .await?;
                measure_runtime_stage_async(
                    "ports.committed.dispatch_deleted_voice",
                    self.dispatch_voice_stops(
                        studio_runtime_schema::persistence::OutboxKind::VoiceDelete,
                    ),
                )
                .await?;
                measure_runtime_stage_async(
                    "ports.committed.dispatch_active_voice",
                    self.dispatch_voice_stops(
                        studio_runtime_schema::persistence::OutboxKind::VoiceStop,
                    ),
                )
                .await?;
                measure_runtime_stage_async(
                    "ports.committed.dispatch_remote",
                    self.dispatch_remote_stops(),
                )
                .await?;
                measure_runtime_stage_async("ports.committed.workspace_cleanup", async {
                    crate::workspace_cleanup_runtime::reconcile_pending_cleanup(Arc::clone(
                        &self.store,
                    ))
                    .await
                    .map_err(|_| ManagementError::Unavailable("workspace-state/agent-lifecycle"))
                })
                .await?;
                Ok(())
            })
            .await
        })
    }
}

impl ProductionLifecyclePorts {
    fn stage_voice_stop(
        &self,
        tx: &Transaction<'_>,
        effect: &LifecycleEffect,
    ) -> Result<(), ManagementError> {
        let agent_id = required_str(effect.before(), "id", "Invalid voice stop owner")?;
        let epoch = effect
            .stopped()
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or(ManagementError::Invalid("Invalid voice stop epoch"))?;
        crate::voice::provider::stage_voice_stop_sessions(tx, agent_id, epoch)
            .map_err(voice_effect_error)
    }

    fn stage_remote_stop(
        &self,
        tx: &Transaction<'_>,
        effect: &LifecycleEffect,
    ) -> Result<(), ManagementError> {
        let agent_id = required_str(effect.before(), "id", "Invalid remote stop owner")?;
        let remote = effect
            .before()
            .get("remoteWorker")
            .filter(|remote| remote.is_object())
            .ok_or(ManagementError::Invalid("Invalid remote worker route"))?;
        let server = required_str(remote, "server", "Invalid remote worker server")?;
        let link = required_str(remote, "link", "Invalid remote worker link")?;
        let request_id = required_str(
            effect.stopped(),
            "remoteStopRequest",
            "Invalid remote stop request identity",
        )?;
        let control_epoch = effect
            .stopped()
            .get("remoteControlEpoch")
            .and_then(Value::as_i64)
            .ok_or(ManagementError::Invalid("Invalid remote control epoch"))?;
        let reason = effect
            .stopped()
            .get("error")
            .and_then(Value::as_str)
            .filter(|reason| !reason.is_empty())
            .unwrap_or("Stopped by the parent");
        let payload = json!({
            "link": link,
            "worker": agent_id,
            "reason": reason,
            "controlEpoch": control_epoch,
        });
        multi_server_transport::enqueue_in_transaction(
            tx,
            server,
            "stop",
            &payload,
            request_id,
            effect.at().max(0.0) as i64,
        )
        .map_err(|_| ManagementError::Unavailable("cross-server/agent-management"))
    }
}

impl ProductionLifecyclePorts {
    async fn dispatch_voice_stops(
        &self,
        kind: studio_runtime_schema::persistence::OutboxKind,
    ) -> Result<(), ManagementError> {
        let intents = crate::voice::provider::pending_voice_intents(&self.store, kind)
            .await
            .map_err(voice_effect_error)?;
        if intents.is_empty() {
            return Ok(());
        }
        let Some(provider) = self.voice_provider.clone() else {
            return Err(ManagementError::Unavailable("voice-driver/agent-stop"));
        };
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ManagementError::Unavailable("voice-driver/agent-stop"))?;
        for intent in intents {
            let already_dispatching = !self
                .voice_dispatching
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(intent.id.clone());
            if already_dispatching {
                continue;
            }
            let provider = Arc::clone(&provider);
            let store = Arc::clone(&self.store);
            let dispatching = Arc::clone(&self.voice_dispatching);
            handle.spawn(async move {
                let attempts =
                    if kind == studio_runtime_schema::persistence::OutboxKind::VoiceDelete {
                        VOICE_DELETE_RETRY_ATTEMPTS
                    } else {
                        1
                    };
                for attempt in 0..attempts {
                    let pending = match crate::voice::provider::pending_voice_intent(
                        &store,
                        intent.id.clone(),
                    )
                    .await
                    {
                        Ok(Some(pending)) => pending,
                        Ok(_) => break,
                        Err(error) => {
                            eprintln!("voice sessions remain pending: {error}");
                            break;
                        }
                    };
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_secs_f64())
                        .unwrap_or_default();
                    if let Err(error) = crate::voice::provider::end_voice_sessions_tracked(
                        Arc::clone(&store),
                        Arc::clone(&provider),
                        pending.id,
                        kind,
                        pending.session,
                        now,
                    )
                    .await
                    {
                        eprintln!("voice sessions remain pending: {error}");
                        break;
                    }
                    if attempt + 1 < attempts {
                        tokio::time::sleep(voice_delete_retry_delay(attempt)).await;
                    }
                }
                dispatching
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&intent.id);
            });
        }
        Ok(())
    }
}

fn voice_delete_retry_delay(attempt: usize) -> Duration {
    Duration::from_millis(100_u64.saturating_mul(1_u64 << attempt.min(4)))
}

fn voice_effect_error(error: crate::voice::provider::VoiceProviderError) -> ManagementError {
    use crate::voice::provider::VoiceProviderError;
    match error {
        VoiceProviderError::Expired => ManagementError::Expired,
        VoiceProviderError::Store(error) => ManagementError::Store(error),
        VoiceProviderError::Outbox(error) => ManagementError::Store(error.into()),
        VoiceProviderError::RequestResult(error) => ManagementError::Store(error.into()),
        VoiceProviderError::Provider(error) => {
            if let crate::account_worker::WorkerError::Store(error) = *error {
                ManagementError::Store(error)
            } else {
                ManagementError::Unavailable("account-worker/voice-stop")
            }
        }
        VoiceProviderError::ContextUnavailable
        | VoiceProviderError::AdapterUnavailable
        | VoiceProviderError::RequiresDependency(_)
        | VoiceProviderError::NotSent
        | VoiceProviderError::Unknown
        | VoiceProviderError::Rejected => ManagementError::Unavailable("voice-driver/agent-stop"),
        VoiceProviderError::ContextChanged
        | VoiceProviderError::Operation
        | VoiceProviderError::Policy(_)
        | VoiceProviderError::Voice(_)
        | VoiceProviderError::Monitor(_) => ManagementError::Invalid("Invalid voice effect intent"),
    }
}

fn cancel_resume(tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
    let before = effect.before();
    let agent_id = required_str(before, "id", "Invalid continuation owner")?;
    for (table, projection) in [
        (ContinuationTable::Usage, "usageResume"),
        (ContinuationTable::Capacity, "capacityRetry"),
    ] {
        let Some(record) = before.get(projection).filter(|record| record.is_object()) else {
            continue;
        };
        let id = required_str(record, "id", "Invalid continuation identity")?;
        let mut stored = ContinuationRepository::snapshot(tx, agent_id, table, Some(id))?
            .get("record")
            .cloned()
            .ok_or(ManagementError::Invalid("Invalid stored continuation"))?;
        let status = stored.get("status").and_then(Value::as_str).unwrap_or("");
        let should_cancel = if projection == "usageResume" {
            matches!(status, "scheduled" | "started")
        } else {
            true
        };
        if !should_cancel {
            continue;
        }
        stored["status"] = json!(if projection == "capacityRetry" {
            "finished"
        } else {
            "cancelled"
        });
        stored["dueAt"] = Value::Null;
        stored["reason"] = json!("The agent was stopped.");
        ContinuationRepository::put_record(tx, table, agent_id, &stored)?;
        if projection == "usageResume"
            && let Some(mut current) = AgentRepository::get(tx, &AgentId::from(agent_id))?
        {
            let mut updated = current
                .extra
                .get(projection)
                .cloned()
                .unwrap_or(Value::Null);
            if updated.get("id").and_then(Value::as_str) == Some(id) {
                updated["status"] = json!("cancelled");
                updated["dueAt"] = Value::Null;
                updated["reason"] = stored["reason"].clone();
                current.extra.insert(projection.to_owned(), updated);
                AgentRepository::put(tx, &current)?;
            }
        }
    }
    Ok(())
}

fn release_work(tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
    // Only deleted or failed workers that own unfinished work can be released;
    // the SQL selection is a superset that the exact test below refines.
    let mut owners = Vec::new();
    if sync_policy::table_exists(tx, "runtime_work")? {
        let mut statement = tx
            .prepare_cached(
                "SELECT DISTINCT a.id FROM runtime_work w JOIN runtime_agents a \
                 ON a.id=json_extract(w.record,'$.owner') \
                 WHERE json_extract(w.record,'$.status') IN ('ready','running','blocked') \
                 AND json_extract(a.record,'$.parentId') IS NOT NULL \
                 AND (COALESCE(json_extract(a.record,'$.deletedAt'),0)!=0 \
                      OR json_extract(a.record,'$.status')='failed')",
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        owners = statement
            .query_map([], |row| row.get::<_, String>(0))
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(studio_runtime_store::StoreError::from)?;
    }
    if let Some(scope) = effect.owner_scope() {
        owners.retain(|owner| scope.contains(owner));
    }
    let mut candidates = Vec::new();
    for owner in owners {
        let Some(agent) = AgentRepository::get(tx, &AgentId::from(owner.as_str()))? else {
            continue;
        };
        let value = agent.value();
        let eligible = value
            .get("parentId")
            .is_some_and(|parent| !parent.is_null())
            && (studio_runtime_store::truthy(value.get("deletedAt"))
                || value.get("status").and_then(Value::as_str) == Some("failed"));
        if eligible {
            let rowid: i64 = tx
                .query_row(
                    "SELECT rowid FROM runtime_agents WHERE id=?1",
                    [&owner],
                    |row| row.get(0),
                )
                .map_err(studio_runtime_store::StoreError::from)?;
            candidates.push((rowid, agent));
        }
    }
    // Release in stored order, like the former full pass over the agents.
    candidates.sort_by_key(|(rowid, _)| *rowid);
    let agents = candidates
        .into_iter()
        .map(|(_, agent)| agent)
        .collect::<Vec<_>>();
    let _ = work_write::release_failed_work(tx, &agents, effect.at())
        .map_err(|_| ManagementError::Invalid("Unable to release deleted worker work"))?;
    Ok(())
}

fn notify_parent(tx: &Transaction<'_>, effect: &LifecycleEffect) -> Result<(), ManagementError> {
    notify_parent_with_id(tx, effect, None)
}

pub(crate) fn notify_continuation_hold(
    tx: &Transaction<'_>,
    before: &Value,
    stopped: &Value,
    operation: &str,
    transition: &str,
    reason: &str,
    at: f64,
) -> Result<(), ManagementError> {
    let mut stopped = stopped.clone();
    stopped["error"] = Value::String(reason.to_owned());
    let effect = LifecycleEffect::new(
        LifecycleEffectKind::NotifyParent,
        before.clone(),
        stopped,
        at,
    );
    notify_parent_with_id(tx, &effect, Some(&format!("hold:{operation}:{transition}")))
}

/// This notification historically accepts partial work rows, unlike the checked
/// WorkItem owner. Only the fields used by this projection are inspected.
#[derive(Default)]
struct NotificationWork {
    id: Value,
    root_id: Option<String>,
    status: Option<String>,
    updated: f64,
    results: Option<Vec<Value>>,
    decisions: Option<Vec<Value>>,
}
impl<'de> serde::Deserialize<'de> for NotificationWork {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct WorkVisitor;
        impl<'de> serde::de::Visitor<'de> for WorkVisitor {
            type Value = NotificationWork;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a work object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                mut map: M,
            ) -> Result<Self::Value, M::Error> {
                let mut work = NotificationWork::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "id" => work.id = map.next_value()?,
                        "rootId" => {
                            work.root_id = map.next_value::<Value>()?.as_str().map(str::to_owned)
                        }
                        "status" => {
                            work.status = map.next_value::<Value>()?.as_str().map(str::to_owned)
                        }
                        "updated" => {
                            work.updated = map.next_value::<Value>()?.as_f64().unwrap_or_default()
                        }
                        "results" => {
                            work.results = match map.next_value::<Value>()? {
                                Value::Array(values) => Some(values),
                                Value::Null
                                | Value::Bool(_)
                                | Value::Number(_)
                                | Value::String(_)
                                | Value::Object(_) => None,
                            }
                        }
                        "decisions" => {
                            work.decisions = match map.next_value::<Value>()? {
                                Value::Array(values) => Some(values),
                                Value::Null
                                | Value::Bool(_)
                                | Value::Number(_)
                                | Value::String(_)
                                | Value::Object(_) => None,
                            }
                        }
                        _ => {
                            map.next_value::<Value>()?;
                        }
                    }
                }
                Ok(work)
            }
        }
        deserializer.deserialize_map(WorkVisitor)
    }
}

/// Work records owned by `agent`, in rowid order, through the owner index.
fn owned_work_records(
    tx: &Transaction<'_>,
    agent: &str,
) -> Result<Vec<NotificationWork>, ManagementError> {
    if !sync_policy::table_exists(tx, "runtime_work")? {
        return Ok(Vec::new());
    }
    let mut statement = tx
        .prepare_cached(
            "SELECT record FROM runtime_work WHERE json_extract(record,'$.owner')=?1 \
             ORDER BY rowid",
        )
        .map_err(studio_runtime_store::StoreError::from)?;
    let rows = statement
        .query_map([agent], |row| row.get::<_, String>(0))
        .and_then(Iterator::collect::<Result<Vec<_>, _>>)
        .map_err(studio_runtime_store::StoreError::from)?;
    Ok(rows
        .into_iter()
        .filter_map(|raw| serde_json::from_str::<NotificationWork>(&raw).ok())
        .collect())
}

fn notify_parent_with_id(
    tx: &Transaction<'_>,
    effect: &LifecycleEffect,
    event_id_override: Option<&str>,
) -> Result<(), ManagementError> {
    notify_parent_with_step(tx, effect, event_id_override, "send")
}

/// Python `child_stopped_event(db, a, "failed", reason, "start:" + attempt)`:
/// a child whose native start failed before a turn existed tells its parent
/// once. Python names the next step "recover" for a failed child.
pub(crate) fn notify_parent_of_start_failure(
    tx: &Transaction<'_>,
    before: &Value,
    failed: &Value,
    attempt: &str,
    at: f64,
) -> studio_runtime_store::StoreResult<()> {
    let id = format!(
        "child-stop:{}:{}:start:{attempt}",
        failed.get("id").and_then(Value::as_str).unwrap_or_default(),
        failed
            .get("epoch")
            .and_then(Value::as_i64)
            .unwrap_or_default()
    );
    let effect = LifecycleEffect::new(
        LifecycleEffectKind::NotifyParent,
        before.clone(),
        failed.clone(),
        at,
    );
    match notify_parent_with_step(tx, &effect, Some(&id), "recover") {
        Ok(())
        | Err(ManagementError::Unavailable(crate::monitor_runtime::REMOTE_EVENTS_DEPENDENCY)) => {
            Ok(())
        }
        Err(ManagementError::Store(error)) => Err(error),
        Err(other) => Err(studio_runtime_store::StoreError::SchemaMismatch(
            other.to_string(),
        )),
    }
}

fn notify_parent_with_step(
    tx: &Transaction<'_>,
    effect: &LifecycleEffect,
    event_id_override: Option<&str>,
    next_step: &str,
) -> Result<(), ManagementError> {
    let agent = effect.stopped();
    if agent.get("isLead").and_then(Value::as_bool) == Some(true)
        || agent.get("parentId").and_then(Value::as_str).is_none()
        || studio_runtime_store::truthy(agent.get("deletedAt"))
        || studio_runtime_store::truthy(agent.get("agentArchive"))
    {
        return Ok(());
    }
    let id = required_str(agent, "id", "Invalid stopped child")?;
    let parent_id = required_str(agent, "parentId", "Invalid stopped child parent")?;
    let parent = AgentRepository::get(tx, &AgentId::from(parent_id))?
        .ok_or(ManagementError::Invalid("Invalid stopped child parent"))?;
    let mut tasks = owned_work_records(tx, id)?
        .into_iter()
        .filter(|work| {
            work.root_id.as_deref() == agent.get("rootId").and_then(Value::as_str)
                && work.status.as_deref().is_some_and(|status| {
                    ["ready", "running", "blocked", "review"].contains(&status)
                })
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| right.updated.total_cmp(&left.updated));
    let tasks = tasks.into_iter().map(|work| {
        let current_result = work.results.as_ref().and_then(|values| values.last());
        let latest = work.decisions.as_ref().and_then(|values| values.last());
        let current_assignment = current_result
            .and_then(|result| result.get("agent").and_then(Value::as_str)) == Some(id);
        let undecided = current_assignment
            && work.status.as_deref() == Some("review")
            && latest.and_then(|decision| decision.get("resultId"))
                != current_result.and_then(|result| result.get("id"));
        json!({
            "task_id": work.id,
            "status": work.status,
            "current_result_id": current_result.and_then(|result| result.get("id")).cloned().unwrap_or(Value::Null),
            "latest_decision": latest.and_then(|decision| decision.get("decision")).cloned().unwrap_or(Value::Null),
            "result_submitted": undecided,
            "needs_resubmission": current_assignment
                && latest.and_then(|decision| decision.get("decision")).and_then(Value::as_str) == Some("reject")
                && latest.and_then(|decision| decision.get("resultId"))
                    == current_result.and_then(|result| result.get("id")),
        })
    }).collect::<Vec<_>>();
    let reason = agent.get("error").cloned().unwrap_or(Value::Null);
    let reason_text = reason
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| reason.to_string());
    let activity = agent.get("activity").and_then(Value::as_object);
    let last_activity = activity
        .and_then(|value| value.get("at"))
        .filter(|value| !value.is_null() && value.as_f64() != Some(0.0))
        .or_else(|| {
            agent
                .get("lastEvent")
                .filter(|value| !value.is_null() && value.as_f64() != Some(0.0))
        })
        .or_else(|| {
            agent
                .get("lastUpdated")
                .filter(|value| !value.is_null() && value.as_f64() != Some(0.0))
        })
        .or_else(|| agent.get("created"))
        .cloned()
        .unwrap_or(Value::Null);
    let mut payload = child_result_payload(
        agent,
        agent.get("status").cloned().unwrap_or(json!("paused")),
        json!(reason_text),
    );
    payload["reason"] = reason;
    payload["last_activity"] = last_activity;
    payload["task_id"] = tasks
        .first()
        .and_then(|task| task.get("task_id"))
        .cloned()
        .unwrap_or(Value::Null);
    payload["result_submitted"] = json!(
        tasks
            .iter()
            .any(|task| task.get("result_submitted").and_then(Value::as_bool) == Some(true))
    );
    payload["tasks"] = json!(tasks);
    payload["next_step"] = json!(next_step);
    payload["requested_by_lead"] = json!(false);
    let stopped_turn = effect
        .before()
        .get("turnId")
        .and_then(Value::as_str)
        .filter(|turn| !turn.is_empty())
        .or_else(|| {
            effect
                .before()
                .pointer("/startAttempt/id")
                .and_then(Value::as_str)
        })
        .unwrap_or("stop");
    let generated_event_id = format!(
        "child-stop:{id}:{}:stop:{stopped_turn}",
        effect.stopped()["epoch"].as_i64().unwrap_or_default()
    );
    let event_id = event_id_override.unwrap_or(&generated_event_id);
    event_write::enqueue_recovery_event(
        tx,
        &parent,
        event_id.to_owned(),
        "child_result",
        payload.to_string(),
        effect.at(),
    )
    .map_err(|error| match error {
        event_write::EventWriteError::RemoteEventsUnavailable => {
            ManagementError::Unavailable(crate::monitor_runtime::REMOTE_EVENTS_DEPENDENCY)
        }
        event_write::EventWriteError::Store(error) => ManagementError::Store(error),
        event_write::EventWriteError::RequestResult(error) => ManagementError::Store(error.into()),
        event_write::EventWriteError::InvalidRequest(_)
        | event_write::EventWriteError::InvalidResultSchema(_)
        | event_write::EventWriteError::Conflict(_)
        | event_write::EventWriteError::WorkerAssignmentUnavailable
        | event_write::EventWriteError::InvalidStoredObject { .. }
        | event_write::EventWriteError::Clock => {
            ManagementError::Invalid("Unable to write child stop event")
        }
    })?;
    Ok(())
}

/// Tell a disconnected child's parent that its turn or start attempt stopped.
///
/// The id is the one [`notify_parent`] derives for the same stop, so a later
/// connection-recovery stop notice for this turn is a no-op. A child without a
/// stored parent has nobody to tell.
pub(crate) fn notify_parent_of_disconnect(
    tx: &Transaction<'_>,
    agent_id: &str,
    at: f64,
) -> studio_runtime_store::StoreResult<()> {
    let Some(raw) = AgentRepository::get_raw(tx, &AgentId::from(agent_id))? else {
        return Ok(());
    };
    let stopped: Value = serde_json::from_str(&raw).map_err(|_| {
        studio_runtime_store::StoreError::InvalidStoredObject {
            table: "runtime_agents",
            id: agent_id.into(),
        }
    })?;
    let Some(parent) = stopped.get("parentId").and_then(Value::as_str) else {
        return Ok(());
    };
    if AgentRepository::get(tx, &AgentId::from(parent))?.is_none() {
        return Ok(());
    }
    let mut before = stopped.clone();
    if !studio_runtime_store::truthy(before.get("turnId")) {
        before["startAttempt"] = stopped["disconnectRecovery"]["startAttempt"].clone();
    }
    let effect = LifecycleEffect::new(LifecycleEffectKind::NotifyParent, before, stopped, at);
    match notify_parent(tx, &effect) {
        // A remote proxy has no local event queue. Its disconnect still commits.
        // A local queue write failure remains an error so the transaction rolls back.
        Ok(())
        | Err(ManagementError::Unavailable(crate::monitor_runtime::REMOTE_EVENTS_DEPENDENCY)) => {
            Ok(())
        }
        Err(ManagementError::Store(error)) => Err(error),
        Err(other) => Err(studio_runtime_store::StoreError::SchemaMismatch(
            other.to_string(),
        )),
    }
}

/// The fields every `child_result` payload carries (Python `Runtime.parent_event`).
fn child_result_payload(agent: &Value, status: Value, result: Value) -> Value {
    json!({
        "agent_id": agent.get("id").cloned().unwrap_or(Value::Null),
        "name": agent.get("name").cloned().unwrap_or(Value::Null),
        "status": status,
        "result": result,
        "cwd": agent.get("cwd").cloned().unwrap_or(Value::Null),
        "branch": agent.get("branch").cloned().unwrap_or(Value::Null),
        "workspace": workspace_summary(agent),
    })
}

/// Report a settled child turn to its parent inside the settling transaction.
///
/// Ports the turn-completion branch of Python `Runtime.notification` that calls
/// `parent_event`. A stopped turn, input that still owns the continuation, a
/// stale turn epoch, a move, or a safety retry of this turn writes nothing.
/// The deterministic id `child:{id}:{turn}` makes a replay a no-op; the
/// enqueue queues an auto-waking parent.
pub(crate) fn notify_parent_of_turn(
    tx: &Transaction<'_>,
    agent_id: &str,
    turn_id: &str,
    at: f64,
) -> studio_runtime_store::StoreResult<()> {
    match AgentRepository::get(tx, &AgentId::from(agent_id))? {
        Some(agent) => notify_parent_of_settled_turn(tx, &agent, turn_id, at),
        None => Ok(()),
    }
}

/// [`notify_parent_of_turn`] for a settled record that may not be stored yet.
///
/// A provider disconnect clears `autoWake` and records whether the child
/// auto-woke in `disconnectRecovery`. A turn that this disconnect receipt
/// covers and that settles `completed` still reports to the parent.
/// Deliberate divergence: Python's `parent_event` stays silent once autoWake is
/// false, so the parent of such a child never learned its result.
/// A failed turn reports through [`notify_parent_of_failed_turn`] once its
/// retry decision is staged.
fn notify_parent_of_settled_turn(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    turn_id: &str,
    at: f64,
) -> studio_runtime_store::StoreResult<()> {
    let agent_id = agent.id.0.as_str();
    if !agent.parent_id.is_some() {
        return Ok(());
    }
    let stopped = matches!(
        agent.status,
        AgentStatus::Failed | AgentStatus::Interrupted | AgentStatus::Paused
    ) && agent.extra.get("lastCompletedTurnStatus") != Some(&json!("completed"));
    let value = agent.value();
    let safety_retry = value.pointer("/nativeSafetyRetry/turnId") == Some(&json!(turn_id))
        && crate::safety_buffering::active(&value).unwrap_or(true);
    let disconnected_completion = agent.status == AgentStatus::Completed
        && value.pointer("/disconnectRecovery/turnId") == Some(&json!(turn_id))
        && value.pointer("/disconnectRecovery/autoWake") == Some(&json!(true));
    if !(agent.auto_wake == Some(true) || disconnected_completion)
        || stopped
        || safety_retry
        || agent.status == AgentStatus::Waiting
        || !reports_turns(agent)
        || SchedulerRepository::has_pending_continuation_event(tx, agent_id, agent.epoch)?
    {
        return Ok(());
    }
    enqueue_turn_result(tx, agent, &value, turn_id, at)
}

/// Report a failed child turn to its parent, once, after the settling
/// transaction staged its retry decision.
///
/// A capacity retry or usage resume that is scheduled or starting for this
/// turn answers later: its retried turn settles under its own
/// `child:{id}:{turn}` id, and a retry that ends without a turn tells the
/// parent through its hold notice. Otherwise the parent gets a `failed`
/// `child_result` with the error under the turn-completion id, so a replay is
/// a no-op. A failed turn holds the child (`nativeFailureHold`), so pending
/// input does not continue it.
pub(crate) fn notify_parent_of_failed_turn(
    tx: &Transaction<'_>,
    agent_id: &str,
    turn_id: &str,
    at: f64,
) -> studio_runtime_store::StoreResult<()> {
    let Some(agent) = AgentRepository::get(tx, &AgentId::from(agent_id))? else {
        return Ok(());
    };
    let retrying = |key: &str| {
        agent.extra.get(key).is_some_and(|retry| {
            retry.get("turnId") == Some(&json!(turn_id))
                && matches!(
                    retry.get("status").and_then(Value::as_str),
                    Some("scheduled" | "starting" | "started")
                )
        })
    };
    if agent.status != AgentStatus::Failed
        || !agent.parent_id.is_some()
        || agent.extra.get("lastCompletedTurn") != Some(&json!(turn_id))
        || agent.auto_wake != Some(true)
        || !reports_turns(&agent)
        || retrying("capacityRetry")
        || retrying("usageResume")
    {
        return Ok(());
    }
    enqueue_turn_result(tx, &agent, &agent.value(), turn_id, at)
}

/// A live child of the current turn epoch: not deleted, archived or moving.
fn reports_turns(agent: &AgentRecord) -> bool {
    let flag = |key: &str| studio_runtime_store::truthy(agent.extra.get(key));
    !agent.is_deleted()
        && agent.agent_archive.is_none()
        && !flag("executionMove")
        && agent.turn_epoch == Some(agent.epoch)
}

/// Existing child-result fields plus the optional derived unsubmitted-assignment notice.
#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct ChildTurnResultPayload {
    agent_id: Option<String>,
    name: Option<String>,
    status: AgentStatus,
    result: Value,
    cwd: Option<String>,
    branch: Option<String>,
    workspace: Value,
    #[serde(default)]
    stall: Option<studio_runtime_schema::agent_progress::AgentStall>,
}

/// Queue the `child_result` of one settled turn under `child:{id}:{turn}`.
fn enqueue_turn_result(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    value: &Value,
    turn_id: &str,
    at: f64,
) -> studio_runtime_store::StoreResult<()> {
    let Some(parent_id) = agent.parent_id.as_deref() else {
        return Ok(());
    };
    let Some(parent) = AgentRepository::get(tx, &AgentId::from(parent_id))? else {
        return Ok(());
    };
    let result = match agent
        .extra
        .get("error")
        .filter(|error| studio_runtime_store::truthy(Some(error)))
    {
        Some(error) => json!(
            error
                .get("message")
                .and_then(Value::as_str)
                .or(error.as_str())
                .map_or_else(|| error.to_string(), str::to_owned)
        ),
        None => json!(
            agent
                .last_answer
                .as_deref()
                .unwrap_or("No final text returned")
        ),
    };
    let assignments = crate::agent_progress::assignments(tx, &agent.id.0)?;
    let progress = crate::agent_progress::derive(tx, value, &assignments)?;
    let mut payload: ChildTurnResultPayload =
        serde_json::from_value(child_result_payload(value, json!(agent.status), result))?;
    payload.stall = progress.stall;
    event_write::enqueue_recovery_event(
        tx,
        &parent,
        format!("child:{}:{turn_id}", agent.id.0),
        "child_result",
        serde_json::to_string(&payload)?,
        at,
    )
    .map_err(|error| match error {
        event_write::EventWriteError::Store(error) => error,
        event_write::EventWriteError::RequestResult(error) => error.into(),
        other @ event_write::EventWriteError::InvalidRequest(_)
        | other @ event_write::EventWriteError::InvalidResultSchema(_)
        | other @ event_write::EventWriteError::Conflict(_)
        | other @ event_write::EventWriteError::RemoteEventsUnavailable
        | other @ event_write::EventWriteError::WorkerAssignmentUnavailable
        | other @ event_write::EventWriteError::InvalidStoredObject { .. }
        | other @ event_write::EventWriteError::Clock => {
            studio_runtime_store::StoreError::SchemaMismatch(other.to_string())
        }
    })?;
    Ok(())
}

fn workspace_summary(agent: &Value) -> Value {
    if agent.get("executionMode").and_then(Value::as_str) == Some("vm") {
        return json!({
            "workspace":"layr",
            "environment":"linux",
            "projectId":agent.get("layrProjectId").cloned().unwrap_or(Value::Null),
            "line":agent.get("layrLine").cloned().unwrap_or(Value::Null),
            "stateId":agent.get("layrStateId").cloned().unwrap_or(Value::Null),
            "path":agent.get("cwd").cloned().unwrap_or(Value::Null),
        });
    }
    let mode = agent
        .get("workspaceMode")
        .and_then(Value::as_str)
        .unwrap_or_else(|| {
            if studio_runtime_store::truthy(agent.get("worktree")) {
                "worktree"
            } else {
                "shared"
            }
        });
    json!({"mode":mode,"path":agent.get("cwd").cloned().unwrap_or(Value::Null)})
}

fn finalize_deleted(tx: &Transaction<'_>, id: &str) -> Result<(), ManagementError> {
    let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(id))? else {
        return Ok(());
    };
    if !agent.is_deleted() {
        return Ok(());
    }
    if !needs_deleted_settlement(&agent) {
        return Ok(());
    }
    agent.status = studio_runtime_store::AgentStatus::Paused;
    agent.in_flight = Some(false);
    agent.turn_id = None;
    agent.activity = None;
    agent.active_tools = Some(Vec::new());
    if let Some(attempt) = agent.start_attempt.as_mut()
        && attempt.active_at_reservation == Some(true)
    {
        attempt.active_at_reservation = Some(false);
    }
    AgentRepository::put(tx, &agent)?;
    Ok(())
}

fn needs_deleted_settlement(agent: &AgentRecord) -> bool {
    agent.is_deleted()
        && (agent.in_flight == Some(true)
            || agent
                .turn_id
                .as_deref()
                .is_some_and(|turn| !turn.is_empty())
            || agent
                .start_attempt
                .as_ref()
                .and_then(|a| a.active_at_reservation)
                == Some(true)
            || studio_runtime_store::startup_recovery::ACTIVE_AGENT_STATUSES
                .contains(&agent.status.as_str()))
}

fn required_str<'a>(
    value: &'a Value,
    key: &str,
    error: &'static str,
) -> Result<&'a str, ManagementError> {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or(ManagementError::Invalid(error))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn notification_work_projection_keeps_legacy_shapes_and_duplicate_last_values() {
        let work: NotificationWork = serde_json::from_str(r#"{"id":5,"id":{"final":true},"rootId":null,"status":9,"status":"review","updated":"bad","results":false,"decisions":[1,null,{"resultId":null}]}"#).unwrap();
        assert_eq!(work.id, json!({"final":true}));
        assert_eq!(work.root_id, None);
        assert_eq!(work.status.as_deref(), Some("review"));
        assert_eq!(work.updated, 0.0);
        assert!(work.results.is_none());
        assert_eq!(
            work.decisions.unwrap(),
            vec![json!(1), Value::Null, json!({"resultId":null})]
        );
        assert!(serde_json::from_str::<NotificationWork>("{").is_err());
    }

    #[test]
    fn notification_reader_skips_rows_with_invalid_unused_json_values() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT NOT NULL);")
            .unwrap();
        let depth = format!("{}0{}", "[".repeat(150), "]".repeat(150));
        let rows = [
            (
                "valid",
                r#"{"owner":"child","id":"valid","status":"ready"}"#.to_owned(),
            ),
            (
                "overflow",
                r#"{"owner":"child","id":"overflow","status":"ready","extra":1e400}"#.to_owned(),
            ),
            (
                "deep",
                format!(r#"{{"owner":"child","id":"deep","status":"ready","extra":{depth}}}"#),
            ),
        ];
        for (id, raw) in &rows {
            db.execute(
                "INSERT INTO runtime_work VALUES(?1,?2)",
                rusqlite::params![id, raw],
            )
            .unwrap();
        }
        assert!(serde_json::from_str::<Value>(&rows[1].1).is_err());
        assert!(serde_json::from_str::<Value>(&rows[2].1).is_err());
        let tx = db.transaction().unwrap();
        let work = owned_work_records(&tx, "child").unwrap();
        assert_eq!(work.len(), 1);
        assert_eq!(work[0].id, "valid");
    }

    #[test]
    fn malformed_work_json_remains_a_sql_query_error_without_writes() {
        let mut db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_work(id TEXT PRIMARY KEY, record TEXT NOT NULL); INSERT INTO runtime_work VALUES('bad','{');").unwrap();
        let tx = db.transaction().unwrap();
        assert!(matches!(
            owned_work_records(&tx, "child"),
            Err(ManagementError::Store(_))
        ));
        let raw: String = tx
            .query_row(
                "SELECT record FROM runtime_work WHERE id='bad'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(raw, "{");
    }

    #[tokio::test]
    async fn parent_notifications_preserve_partial_work_and_replay_identity() {
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction(|tx| {
            let parent = AgentRecord::from_value(json!({"compactions":0,"accountKey":"default","id":"lead","rootId":"lead","epoch":1,"status":"completed","provider":"codex","isLead":true,"autoWake":true,"name":"Lead","cwd":"/work"}))?;
            AgentRepository::put(tx, &parent)?;
            for (row, work) in [
                ("first", json!({"owner":"child","rootId":"lead","status":"review","id":{"legacy":1},"updated":"bad","results":[{"agent":"child","id":7}],"decisions":[{"resultId":7,"decision":"reject"}]})),
                ("second", json!({"owner":"child","rootId":"lead","status":"ready","updated":4,"id":null})),
                ("third", json!({"owner":"child","rootId":"lead","status":"blocked","id":"third","updated":0})),
                ("other", json!({"owner":"child","rootId":"other","status":"ready","id":"other"})),
            ] {
                tx.execute("INSERT INTO runtime_work(id,record) VALUES(?1,?2)", rusqlite::params![row, work.to_string()])?;
            }
            Ok(())
        }).await.unwrap();
        let child = json!({"compactions":0,"id":"child","rootId":"lead","parentId":"lead","epoch":1,"status":"failed","name":"Worker","error":"failed"});
        let before = json!({"id":"child","turnId":"turn"});
        for mode in ["stop", "start", "hold"] {
            let child = child.clone();
            let before = before.clone();
            store
                .transaction(move |tx| {
                    let mut first_snapshot = None;
                    for _ in 0..2 {
                        match mode {
                            "stop" => {
                                let effect = LifecycleEffect::new(
                                    LifecycleEffectKind::NotifyParent,
                                    before.clone(),
                                    child.clone(),
                                    3.0,
                                );
                                notify_parent(tx, &effect).map_err(|e| {
                                    studio_runtime_store::StoreError::SchemaMismatch(e.to_string())
                                })?;
                            }
                            "start" => {
                                notify_parent_of_start_failure(tx, &before, &child, "attempt", 3.0)?
                            }
                            _ => notify_continuation_hold(
                                tx,
                                &before,
                                &child,
                                "op",
                                "transition",
                                "failed",
                                3.0,
                            )
                            .map_err(|e| {
                                studio_runtime_store::StoreError::SchemaMismatch(e.to_string())
                            })?,
                        }
                        let mut query = tx.prepare("SELECT id,text FROM runtime_events WHERE agent='lead' AND kind='child_result' ORDER BY id")?;
                        let snapshot = query.query_map([], |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)))?.collect::<Result<Vec<_>, _>>()?;
                        if let Some(first) = &first_snapshot {
                            assert_eq!(&snapshot, first);
                        } else {
                            first_snapshot = Some(snapshot);
                        }
                    }
                    Ok(())
                })
                .await
                .unwrap();
        }
        let db = store.test_reader().unwrap();
        let mut query = db.prepare("SELECT id,text FROM runtime_events WHERE agent='lead' AND kind='child_result' ORDER BY id").unwrap();
        let events = query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert_eq!(events.len(), 3);
        for (_, text) in events {
            let payload: Value = serde_json::from_str(&text).unwrap();
            assert_eq!(payload["tasks"][0]["task_id"], Value::Null);
            assert_eq!(payload["tasks"][1]["task_id"], json!({"legacy":1}));
            assert_eq!(payload["tasks"][2]["task_id"], "third");
            assert_eq!(payload["tasks"][1]["current_result_id"], 7);
            assert_eq!(payload["tasks"][1]["latest_decision"], "reject");
            assert_eq!(payload["tasks"][1]["needs_resubmission"], true);
            assert_eq!(payload["tasks"][1]["result_submitted"], false);
        }
    }

    #[test]
    fn deleted_voice_retry_is_bounded_and_caps_backoff() {
        assert_eq!(VOICE_DELETE_RETRY_ATTEMPTS, 8);
        assert_eq!(voice_delete_retry_delay(0), Duration::from_millis(100));
        assert_eq!(voice_delete_retry_delay(4), Duration::from_millis(1_600));
        assert_eq!(voice_delete_retry_delay(7), Duration::from_millis(1_600));
    }
    use crate::account_session_runtime::SessionOutcome as ReceiptOutcome;
    use crate::account_session_runtime::{AccountSessionFuture, RequestCertainty, SessionReceipt};
    use studio_runtime_providers::{RequestId, RequestTicket};

    struct Broker {
        scope: Option<ConnectionScope>,
        certainty: RequestCertainty,
        response: Option<ProviderResponse>,
        observed: Mutex<Option<ProviderRequest>>,
    }

    impl AccountSessionPort for Broker {
        fn current_scope(&self, _: &str) -> Option<ConnectionScope> {
            self.scope.clone()
        }

        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            Box::pin(async move {
                let certainty = if self.scope.is_none() {
                    RequestCertainty::NotSent
                } else {
                    self.certainty
                };
                let receipt = SessionReceipt {
                    outcome: {
                        let ticket = RequestTicket {
                            generation: request
                                .expected_scope
                                .as_ref()
                                .expect("test broker request expected scope")
                                .generation,
                            id: RequestId::String(request.operation.clone()),
                            method: request.method,
                        };
                        match (certainty, self.response.clone()) {
                            (RequestCertainty::Written, Some(response)) => {
                                ReceiptOutcome::Written { ticket, response }
                            }
                            (RequestCertainty::Written, None) | (RequestCertainty::Unknown, _) => {
                                ReceiptOutcome::Unknown {
                                    ticket: RequestTicket {
                                        generation: request
                                            .expected_scope
                                            .as_ref()
                                            .expect("test broker request expected scope")
                                            .generation,
                                        id: RequestId::String(request.operation.clone()),
                                        method: request.method,
                                    },
                                }
                            }
                            (RequestCertainty::NotSent, _) => {
                                ReceiptOutcome::WorkerNotSent { ticket }
                            }
                        }
                    },
                    scope: self.scope.clone().unwrap_or_else(|| {
                        request
                            .expected_scope
                            .clone()
                            .expect("test broker request expected scope")
                    }),
                    operation: request.operation.clone(),
                    replayed: false,
                };
                *self.observed.lock().expect("observed request lock") = Some(request);
                Ok(receipt)
            })
        }
    }

    fn request() -> InterruptRequest {
        InterruptRequest {
            root_request_id: studio_runtime_schema::persistence::RequestIdentity::issue_root(),
            ticket: "lifecycle-interrupt:request".into(),
            agent: "agent".into(),
            epoch: 4,
            thread: "thread".into(),
            turn: "turn".into(),
            scope: ConnectionScope {
                account: "acct".into(),
                connection: "connection".into(),
                generation: 8,
            },
            terminals: None,
        }
    }

    #[tokio::test]
    async fn interrupt_dispatch_binds_expected_scope_and_actor() {
        let request = request();
        let broker = Broker {
            scope: Some(request.scope.clone()),
            certainty: RequestCertainty::Written,
            response: Some(ProviderResponse::Reply(
                json!({"cancelled":["studio:input"],"still_queued":[]}),
            )),
            observed: Mutex::new(None),
        };

        assert_eq!(
            request_interrupt(&broker, &request).await,
            InterruptWriteResult::Accepted(json!({"cancelled":["studio:input"],"still_queued":[]}))
        );
        let observed = broker.observed.lock().expect("observed request lock");
        let observed = observed.as_ref().expect("broker records request");
        assert_eq!(observed.expected_scope.as_ref(), Some(&request.scope));
        assert_eq!(observed.operation, request.ticket);
        assert_eq!(
            observed.actor.as_ref().map(|actor| actor.agent.as_str()),
            Some("agent")
        );
        assert_eq!(observed.actor.as_ref().map(|actor| actor.epoch), Some(4));
        assert_eq!(
            observed.method,
            studio_runtime_providers::Method::TurnInterrupt
        );
        assert_eq!(
            observed.params,
            json!({"threadId":"thread","turnId":"turn"})
        );
    }

    #[tokio::test]
    async fn interrupt_uses_broker_scope_guard_and_preserves_unknown() {
        let request = request();
        let broker = Broker {
            scope: None,
            certainty: RequestCertainty::Unknown,
            response: None,
            observed: Mutex::new(None),
        };
        assert_eq!(
            request_interrupt(&broker, &request).await,
            InterruptWriteResult::NotSent
        );
        {
            let observed = broker.observed.lock().expect("observed request lock");
            assert_eq!(
                observed
                    .as_ref()
                    .and_then(|request| request.expected_scope.as_ref()),
                Some(&request.scope)
            );
        }

        let broker = Broker {
            scope: Some(request.scope.clone()),
            certainty: RequestCertainty::Unknown,
            response: None,
            observed: Mutex::new(None),
        };
        assert_eq!(
            request_interrupt(&broker, &request).await,
            InterruptWriteResult::Unknown
        );
    }

    #[tokio::test]
    async fn interrupt_rejection_is_written_not_unknown() {
        let request = request();
        let broker = Broker {
            scope: Some(request.scope.clone()),
            certainty: RequestCertainty::Written,
            response: Some(ProviderResponse::Rejected(json!({"code":"rejected"}))),
            observed: Mutex::new(None),
        };
        assert_eq!(
            request_interrupt(&broker, &request).await,
            InterruptWriteResult::Rejected(json!({"code":"rejected"}))
        );
    }
}
