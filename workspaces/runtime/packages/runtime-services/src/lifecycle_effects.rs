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
//! `image_workspace_summary` / `child_stopped_event` in
//! `codex_runtime.py:7671-7745`; and
//! `release_failed_work` in `codex_work.py:1313-1355`.

use std::collections::{HashMap, HashSet};
use std::future::Future;
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
        AccountSessionError, AccountSessionPort, ConnectionScope, ProviderRequest,
        ProviderResponse, RequestCertainty,
    },
    account_worker::retained::RequestActor,
    agent_lifecycle_runtime::{LifecycleEffect, LifecycleEffectKind, LifecyclePorts},
    agent_management::{Action, ManagementError},
    event_write,
    monitor_lifecycle::MonitorLifecyclePorts,
    multi_server_transport::{
        self, HttpsPairedTransport, OrchestrationDriver, PairedTransport,
        RuntimeOrchestrationEffects,
    },
    sync_policy, work_write,
};

const VOICE_STOP_SIGNATURE: &str = "lifecycle.voice-stop.v1";

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
// Ordinary voice stops can remain in flight until their 30-second deadline.
const VOICE_DELETE_HELD_WAIT: Duration = Duration::from_secs(35);

async fn measure_runtime_stage_async<T>(
    name: &'static str,
    operation: impl Future<Output = T>,
) -> T {
    use std::sync::OnceLock;

    static ENABLED: OnceLock<bool> = OnceLock::new();
    if !*ENABLED.get_or_init(|| {
        std::env::var("CODEX_RUNTIME_STAGE_TIMINGS").is_ok_and(|value| value == "1")
    }) {
        return operation.await;
    }
    let started = Instant::now();
    let result = operation.await;
    eprintln!(
        "runtime_stage name={name} elapsed_ms={}",
        started.elapsed().as_millis()
    );
    result
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum InterruptWriteResult {
    Accepted,
    Rejected,
    NotSent,
    Unknown,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InterruptRequest {
    pub ticket: String,
    pub agent: String,
    pub epoch: i64,
    pub thread: String,
    pub turn: String,
    pub scope: ConnectionScope,
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
        let is_review = before.get("nativeReview").and_then(Value::as_bool) == Some(true)
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
        let connection = attempt
            .get("connectionId")
            .and_then(Value::as_str)
            .or_else(|| before.get("connectionId").and_then(Value::as_str))
            .ok_or(ManagementError::Unavailable(
                "account-worker/agent-interrupt",
            ))?
            .to_owned();
        let stopped_epoch = effect
            .stopped()
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or(ManagementError::Invalid("Invalid stopped epoch"))?;
        let request_epoch = before
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or(ManagementError::Invalid("Invalid interrupt actor epoch"))?;
        let ticket = format!(
            "lifecycle-interrupt:{}",
            event_write::request_signature(&json!([
                agent,
                stopped_epoch,
                generation,
                attempt.get("id"),
                &account,
                &connection,
                thread,
                turn
            ]))
        );
        if !SchedulerRepository::prepare_interrupt(tx, agent, thread, turn, generation, &ticket)? {
            return Err(ManagementError::Unavailable(
                "account-worker/agent-interrupt",
            ));
        }
        let request = InterruptRequest {
            ticket,
            agent: agent.to_owned(),
            epoch: request_epoch,
            thread: thread.to_owned(),
            turn: turn.to_owned(),
            scope: ConnectionScope {
                account,
                connection,
                generation,
            },
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
            let epoch = request.epoch;
            let persisted = self
                .store
                .read_async("runtime-services/lifecycle_effects", move |tx| {
                    let Some(agent) = AgentRepository::get(tx, &agent_id)? else {
                        return Ok::<_, studio_runtime_store::StoreError>(false);
                    };
                    let interrupt =
                        &agent.extra.get("startAttempt").unwrap_or(&Value::Null)["interrupt"];
                    Ok(interrupt["requestTicket"].as_str() == Some(ticket.as_str())
                        && interrupt["outcome"].as_str() == Some("unknown")
                        && agent.epoch == epoch.saturating_add(1))
                })
                .await?;
            if !persisted {
                continue;
            }
            let outcome = request_interrupt(self.interrupt_broker.as_ref(), &request).await;
            let (state, error) = match outcome {
                InterruptWriteResult::Accepted => ("accepted", None),
                InterruptWriteResult::Rejected => ("rejected", None),
                InterruptWriteResult::NotSent => ("unsent", None),
                InterruptWriteResult::Unknown => ("unknown", None),
            };
            let request_ticket = request.ticket.clone();
            let generation = request.scope.generation;
            self.store
                .transaction_with_error(move |tx| {
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
        }
        Ok(())
    }

    async fn dispatch_remote_stops(&self) -> Result<(), ManagementError> {
        let pending = self
            .store
            .read_async("runtime-services/lifecycle_effects", |tx| {
                let mut statement = tx.prepare(
                    "SELECT id,server,body FROM runtime_server_outbox \
                 WHERE state IN ('queued','running') ORDER BY rowid",
                )?;
                let rows = statement.query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(studio_runtime_store::StoreError::from)
            })
            .await?;
        if pending.is_empty() {
            return Ok(());
        }
        let effects = Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&self.store)));
        let driver = OrchestrationDriver::new(
            Arc::clone(&self.store),
            Arc::clone(&self.remote_transport),
            effects,
        );
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_secs() as i64)
            .unwrap_or_default();
        for (request_id, server, body) in pending {
            let Ok(envelope) = serde_json::from_str::<Value>(&body) else {
                eprintln!("stored cross-server stop intent is invalid: {request_id}");
                continue;
            };
            if envelope.get("action").and_then(Value::as_str) != Some("stop") {
                continue;
            }
            let Some(payload) = envelope.get("payload") else {
                eprintln!("stored cross-server stop intent is incomplete: {request_id}");
                continue;
            };
            // The signed driver preserves the exact request identity. A lost
            // reply stays pending and is replayed only under the same identity.
            if let Err(error) = driver
                .exchange(&server, "stop", payload, &request_id, now)
                .await
            {
                eprintln!("cross-server stop remains queued ({request_id}): {error}");
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
                    let record: Value = serde_json::from_str(&raw)
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
    let expected = request.scope.clone();
    let receipt = port
        .provider_request(ProviderRequest {
            account: request.scope.account.clone(),
            operation: request.ticket.clone(),
            actor: Some(RequestActor {
                agent: request.agent.clone(),
                epoch: request.epoch,
            }),
            deleted_actor_session: None,
            expected_scope: Some(expected.clone()),
            method: studio_runtime_providers::Method::TurnInterrupt,
            params: json!({"threadId":request.thread,"turnId":request.turn}),
            deadline: Instant::now() + INTERRUPT_TIMEOUT,
        })
        .await;
    match receipt {
        Ok(receipt)
            if receipt.operation == request.ticket && receipt.scope.account == expected.account =>
        {
            match receipt.certainty {
                RequestCertainty::Written
                    if receipt.scope == expected
                        && receipt.ticket.as_ref().is_some_and(|ticket| {
                        ticket.generation == expected.generation
                            && ticket.method == studio_runtime_providers::Method::TurnInterrupt
                            && matches!(&ticket.id, studio_runtime_providers::RequestId::String(id) if id == &request.ticket)
                    }) => match receipt.result {
                        Some(ProviderResponse::Reply(_)) => InterruptWriteResult::Accepted,
                        Some(ProviderResponse::Rejected(_)) => InterruptWriteResult::Rejected,
                        None => InterruptWriteResult::Unknown,
                    },
                RequestCertainty::NotSent => InterruptWriteResult::NotSent,
                RequestCertainty::Written | RequestCertainty::Unknown => InterruptWriteResult::Unknown,
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
            .map_err(|_| ManagementError::Invalid("Invalid voice deletion intent"))?;
        if !intent.sessions.is_empty() && self.voice_provider.is_none() {
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
                    self.dispatch_deleted_voice_sessions(),
                )
                .await?;
                measure_runtime_stage_async(
                    "ports.committed.dispatch_active_voice",
                    self.dispatch_active_voice_stops(),
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
        let id = format!("agent-voice-stop:{agent_id}:{epoch}");
        let result = json!({"agent":agent_id,"epoch":epoch,"status":"pending"});
        tx.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3) ON CONFLICT(id) DO NOTHING",
            rusqlite::params![id, VOICE_STOP_SIGNATURE, result.to_string()],
        )
        .map_err(studio_runtime_store::StoreError::from)?;
        let stored_signature: String = tx
            .query_row(
                "SELECT signature FROM runtime_operation_receipts WHERE id=?1",
                [&id],
                |row| row.get(0),
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        if stored_signature != VOICE_STOP_SIGNATURE {
            return Err(ManagementError::Invalid(
                "Voice stop intent identity collision",
            ));
        }
        Ok(())
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
    async fn dispatch_active_voice_stops(&self) -> Result<(), ManagementError> {
        let intents = self
            .store
            .read_async("runtime-services/lifecycle_effects", |tx| {
                let mut statement = tx.prepare(
                    "SELECT id,json_extract(result,'$.agent') FROM runtime_operation_receipts \
                 WHERE signature=?1 AND json_extract(result,'$.status')='pending' ORDER BY id",
                )?;
                let rows = statement.query_map([VOICE_STOP_SIGNATURE], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(studio_runtime_store::StoreError::from)
            })
            .await?;
        if intents.is_empty() {
            return Ok(());
        }
        let provider = self
            .voice_provider
            .clone()
            .ok_or(ManagementError::Unavailable("voice-driver/agent-stop"))?;
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ManagementError::Unavailable("voice-driver/agent-stop"))?;
        for (intent_id, agent_id) in intents {
            let dispatch_key = intent_id.clone();
            let already_dispatching = !self
                .voice_dispatching
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .insert(dispatch_key.clone());
            if already_dispatching {
                continue;
            }
            let store = Arc::clone(&self.store);
            let provider = Arc::clone(&provider);
            let dispatching = Arc::clone(&self.voice_dispatching);
            let completion_store = Arc::clone(&store);
            let completion_intent = intent_id.clone();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|duration| duration.as_secs_f64())
                .unwrap_or_default();
            handle.spawn(async move {
                if let Err(error) = crate::voice::provider::end_active_for_agent(
                    store,
                    provider,
                    agent_id.clone(),
                    now,
                )
                .await
                {
                    eprintln!("voice sessions remain stopped-pending for {agent_id}: {error}");
                } else if let Err(error) = completion_store
                    .transaction(move |tx| {
                        tx.execute(
                            "DELETE FROM runtime_operation_receipts WHERE id=?1 AND signature=?2 \
                         AND json_extract(result,'$.status')='pending'",
                            rusqlite::params![completion_intent, VOICE_STOP_SIGNATURE],
                        )?;
                        Ok(())
                    })
                    .await
                {
                    eprintln!("voice stop intent remains pending for {agent_id}: {error}");
                }
                dispatching
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .remove(&dispatch_key);
            });
        }
        Ok(())
    }

    async fn dispatch_deleted_voice_sessions(&self) -> Result<(), ManagementError> {
        let intents = crate::voice::provider::pending_delete_intents(&self.store)
            .await
            .map_err(|_| ManagementError::Invalid("Invalid voice deletion intent"))?;
        if intents.is_empty() {
            return Ok(());
        }
        let Some(provider) = self.voice_provider.clone() else {
            return if intents.iter().all(|intent| intent.sessions.is_empty()) {
                Ok(())
            } else {
                Err(ManagementError::Unavailable("voice-driver/agent-stop"))
            };
        };
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| ManagementError::Unavailable("voice-driver/agent-stop"))?;
        for intent in intents {
            if intent.sessions.is_empty() && intent.held_uncertain.is_empty() {
                continue;
            }
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
                let held_deadline = tokio::time::Instant::now() + VOICE_DELETE_HELD_WAIT;
                let mut session_attempts = HashMap::<String, usize>::new();
                let mut poll_attempt = 0;
                loop {
                    let pending = match crate::voice::provider::pending_delete_intents(&store).await
                    {
                        Ok(intents) => intents
                            .into_iter()
                            .find(|pending| pending.id == intent.id)
                            .filter(|pending| {
                                !pending.sessions.is_empty() || !pending.held_uncertain.is_empty()
                            }),
                        Err(error) => {
                            eprintln!("deleted voice sessions remain pending: {error}");
                            if tokio::time::Instant::now() >= held_deadline {
                                break;
                            }
                            tokio::time::sleep(voice_delete_retry_delay(poll_attempt)).await;
                            poll_attempt += 1;
                            continue;
                        }
                    };
                    let Some(pending) = pending else {
                        break;
                    };
                    let dispatchable: Vec<_> = pending
                        .sessions
                        .into_iter()
                        .filter(|session| {
                            session_attempts
                                .get(&session.session_id)
                                .copied()
                                .unwrap_or_default()
                                < VOICE_DELETE_RETRY_ATTEMPTS
                        })
                        .collect();
                    if dispatchable.is_empty() {
                        if pending.held_uncertain.is_empty()
                            || tokio::time::Instant::now() >= held_deadline
                        {
                            break;
                        }
                        tokio::time::sleep(voice_delete_retry_delay(poll_attempt)).await;
                        poll_attempt += 1;
                        continue;
                    }
                    let backoff_attempt = dispatchable
                        .iter()
                        .filter_map(|session| session_attempts.get(&session.session_id).copied())
                        .max()
                        .unwrap_or_default();
                    let now = std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|duration| duration.as_secs_f64())
                        .unwrap_or(0.0);
                    if let Err(error) = crate::voice::provider::end_deleted_sessions_tracked(
                        Arc::clone(&store),
                        Arc::clone(&provider),
                        pending.id,
                        dispatchable.clone(),
                        now,
                    )
                    .await
                    {
                        eprintln!("deleted voice sessions remain pending: {error}");
                    }
                    for session in dispatchable {
                        *session_attempts.entry(session.session_id).or_default() += 1;
                    }
                    tokio::time::sleep(voice_delete_retry_delay(backoff_attempt)).await;
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

/// Work records owned by `agent`, in rowid order, through the owner index.
fn owned_work_records(tx: &Transaction<'_>, agent: &str) -> Result<Vec<Value>, ManagementError> {
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
        .filter_map(|raw| serde_json::from_str::<Value>(&raw).ok())
        .collect())
}

fn notify_parent_with_id(
    tx: &Transaction<'_>,
    effect: &LifecycleEffect,
    event_id_override: Option<&str>,
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
            work.get("rootId").and_then(Value::as_str)
                == agent.get("rootId").and_then(Value::as_str)
                && work
                    .get("status")
                    .and_then(Value::as_str)
                    .is_some_and(|status| {
                        ["ready", "running", "blocked", "review"].contains(&status)
                    })
        })
        .collect::<Vec<_>>();
    tasks.sort_by(|left, right| {
        right
            .get("updated")
            .and_then(Value::as_f64)
            .unwrap_or_default()
            .total_cmp(
                &left
                    .get("updated")
                    .and_then(Value::as_f64)
                    .unwrap_or_default(),
            )
    });
    let tasks = tasks.into_iter().map(|work| {
        let results = work.get("results").and_then(Value::as_array);
        let current_result = results.and_then(|values| values.last());
        let decisions = work.get("decisions").and_then(Value::as_array);
        let latest = decisions.and_then(|values| values.last());
        let current_assignment = current_result
            .and_then(|result| result.get("agent").and_then(Value::as_str)) == Some(id);
        let undecided = current_assignment
            && work.get("status").and_then(Value::as_str) == Some("review")
            && latest.and_then(|decision| decision.get("resultId"))
                != current_result.and_then(|result| result.get("id"));
        json!({
            "task_id": work.get("id").cloned().unwrap_or(Value::Null),
            "status": work.get("status").cloned().unwrap_or(Value::Null),
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
    payload["next_step"] = json!("send");
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
        _ => ManagementError::Invalid("Unable to write child stop event"),
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
        "workspace": image_workspace_summary(agent),
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
fn notify_parent_of_settled_turn(
    tx: &Transaction<'_>,
    agent: &AgentRecord,
    turn_id: &str,
    at: f64,
) -> studio_runtime_store::StoreResult<()> {
    let agent_id = agent.id.0.as_str();
    let Some(parent_id) = agent.extra.get("parentId").and_then(Value::as_str) else {
        return Ok(());
    };
    let flag = |key: &str| studio_runtime_store::truthy(agent.extra.get(key));
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
    if !(flag("autoWake") || disconnected_completion)
        || flag("deletedAt")
        || flag("agentArchive")
        || flag("executionMove")
        || stopped
        || safety_retry
        || agent.status == AgentStatus::Waiting
        || agent.extra.get("turnEpoch").and_then(Value::as_i64) != Some(agent.epoch)
        || SchedulerRepository::has_pending_continuation_event(tx, agent_id, agent.epoch)?
    {
        return Ok(());
    }
    let Some(parent) = AgentRepository::get(tx, &AgentId::from(parent_id))? else {
        return Ok(());
    };
    let result = match agent
        .extra
        .get("error")
        .filter(|error| studio_runtime_store::truthy(Some(error)))
    {
        Some(error) => json!(error.to_string()),
        None => agent
            .extra
            .get("lastAnswer")
            .cloned()
            .unwrap_or(json!("No final text returned")),
    };
    let payload = child_result_payload(&value, json!(agent.status), result);
    event_write::enqueue_recovery_event(
        tx,
        &parent,
        format!("child:{agent_id}:{turn_id}"),
        "child_result",
        payload.to_string(),
        at,
    )
    .map_err(|error| match error {
        event_write::EventWriteError::Store(error) => error,
        other => studio_runtime_store::StoreError::SchemaMismatch(other.to_string()),
    })?;
    Ok(())
}

fn image_workspace_summary(agent: &Value) -> Value {
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
    let phase = agent.get("imageWorkspacePhase");
    let mode = agent
        .get("workspaceMode")
        .and_then(Value::as_str)
        .or_else(|| {
            if studio_runtime_store::truthy(agent.get("imageWorkspace")) {
                Some("image")
            } else if studio_runtime_store::truthy(agent.get("worktree")) {
                Some("worktree")
            } else {
                Some("shared")
            }
        })
        .unwrap_or("shared");
    let mut result = json!({"mode":mode,"path":agent.get("cwd").cloned().unwrap_or(Value::Null)});
    let image = studio_runtime_store::truthy(agent.get("imageWorkspace"));
    let fallback = phase.and_then(Value::as_str) == Some("fallback");
    if !image && !fallback && mode != "image" {
        return result;
    }
    let ready = studio_runtime_store::truthy(agent.get("imageWorkspaceReady"));
    let state = agent
        .get("imageWorkspaceBaseState")
        .filter(|value| !value.is_null())
        .cloned()
        .or_else(|| {
            if ready {
                Some(json!("ready"))
            } else if phase.and_then(Value::as_str) == Some("read_only") {
                Some(json!("building"))
            } else {
                phase.cloned()
            }
        })
        .unwrap_or(Value::Null);
    let taken_at = agent
        .get("imageWorkspaceCreatedAt")
        .and_then(Value::as_f64)
        .and_then(|created| chrono::DateTime::from_timestamp(created as i64, 0))
        .map(|created| created.to_rfc3339_opts(chrono::SecondsFormat::Secs, true));
    result["source"] = agent
        .get("imageWorkspaceRepo")
        .cloned()
        .unwrap_or(Value::Null);
    result["state"] = state;
    result["takenAt"] = taken_at.map_or(Value::Null, |at| json!(at));
    result["includesUncommittedChanges"] = json!(matches!(mode, "image" | "shared"));
    if let Some(error) = agent.get("imageWorkspaceError") {
        result["error"] = error.clone();
    }
    result
}

fn finalize_deleted(tx: &Transaction<'_>, id: &str) -> Result<(), ManagementError> {
    let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(id))? else {
        return Ok(());
    };
    if !studio_runtime_store::truthy(agent.extra.get("deletedAt")) {
        return Ok(());
    }
    let agent_value =
        serde_json::to_value(&agent).map_err(studio_runtime_store::StoreError::from)?;
    if !needs_deleted_settlement(&agent_value) {
        return Ok(());
    }
    agent.status = studio_runtime_store::AgentStatus::Paused;
    agent.extra.insert("inFlight".to_owned(), json!(false));
    agent.extra.insert("turnId".to_owned(), Value::Null);
    agent.extra.insert("activity".to_owned(), Value::Null);
    agent.extra.insert("activeTools".to_owned(), json!([]));
    if let Some(attempt) = agent.extra.get_mut("startAttempt")
        && studio_runtime_store::truthy(attempt.get("activeAtReservation"))
    {
        attempt["activeAtReservation"] = json!(false);
    }
    AgentRepository::put(tx, &agent)?;
    Ok(())
}

fn needs_deleted_settlement(agent: &Value) -> bool {
    studio_runtime_store::truthy(agent.get("deletedAt"))
        && (studio_runtime_store::truthy(agent.get("inFlight"))
            || studio_runtime_store::truthy(agent.get("turnId"))
            || studio_runtime_store::truthy(agent.pointer("/startAttempt/activeAtReservation"))
            || agent
                .get("status")
                .and_then(Value::as_str)
                .is_some_and(|status| {
                    studio_runtime_store::startup_recovery::ACTIVE_AGENT_STATUSES.contains(&status)
                }))
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
    fn deleted_voice_retry_is_bounded_and_caps_backoff() {
        assert_eq!(VOICE_DELETE_RETRY_ATTEMPTS, 8);
        assert!(VOICE_DELETE_HELD_WAIT > Duration::from_secs(30));
        assert_eq!(voice_delete_retry_delay(0), Duration::from_millis(100));
        assert_eq!(voice_delete_retry_delay(4), Duration::from_millis(1_600));
        assert_eq!(voice_delete_retry_delay(7), Duration::from_millis(1_600));
    }
    use crate::account_session_runtime::{AccountSessionFuture, NotSentReason, SessionReceipt};
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
                    scope: self.scope.clone().unwrap_or_else(|| {
                        request
                            .expected_scope
                            .clone()
                            .expect("test broker request expected scope")
                    }),
                    operation: request.operation.clone(),
                    ticket: Some(RequestTicket {
                        generation: request
                            .expected_scope
                            .as_ref()
                            .expect("test broker request expected scope")
                            .generation,
                        id: RequestId::String(request.operation.clone()),
                        method: request.method,
                    }),
                    certainty,
                    result: self.response.clone(),
                    not_sent_reason: (certainty == RequestCertainty::NotSent)
                        .then_some(NotSentReason::WorkerNotSent),
                    replayed: false,
                };
                *self.observed.lock().expect("observed request lock") = Some(request);
                Ok(receipt)
            })
        }
    }

    fn request() -> InterruptRequest {
        InterruptRequest {
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
        }
    }

    #[tokio::test]
    async fn interrupt_dispatch_binds_expected_scope_and_actor() {
        let request = request();
        let broker = Broker {
            scope: Some(request.scope.clone()),
            certainty: RequestCertainty::Written,
            response: Some(ProviderResponse::Reply(json!({"turnId":"turn"}))),
            observed: Mutex::new(None),
        };

        assert_eq!(
            request_interrupt(&broker, &request).await,
            InterruptWriteResult::Accepted
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
            InterruptWriteResult::Rejected
        );
    }
}
