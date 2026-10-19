//! The concurrent part of a submission. The durable `submitted` commit runs
//! in the agent's own task, concurrently with other agents; the account worker
//! keeps only the provider write. The FULL commit still returns before the
//! write starts, so a crash between them leaves a durable submitted receipt
//! with an Unknown outcome, never an unrecorded write.
use super::*;
use crate::provider_session_task::PreparedRequest;

/// One request to commit, built by the worker and run in the agent's task
/// (or inline for an interrupt).
pub(super) struct Commit {
    pub bridge: StoreBridge,
    pub account: String,
    pub connection_id: String,
    pub generation: u64,
    /// Provider reply deadline of the request; unit: milliseconds.
    pub timeout_ms: u64,
    pub control: SessionControl,
    pub id: RequestId,
    pub context: Pending,
    pub method: Method,
    pub params: Value,
    pub interrupt_turn: Option<(String, i64)>,
    pub submission: Option<crate::notification_dispatch::PreparedSubmission>,
    pub guard: Option<preparation::CommitGuard>,
    pub receipt_checked: bool,
    /// The durable effect the submitted commit deletes.
    pub effect: Option<String>,
}

pub(super) enum Committed {
    /// Already submitted, or the durable boundary refused it: nothing to write.
    Nothing,
    /// The agent may run in a VM: the worker resolves its session, then
    /// finishes the commit inline. Carries the launch lookup's result.
    Vm(
        Box<Commit>,
        Result<Option<preparation::VmProviderLaunchRequest>, WorkerError>,
    ),
    /// The FULL submitted commit returned; only the write remains.
    Ready(Box<Ready>),
}

pub(super) struct Ready {
    pub id: RequestId,
    pub context: Pending,
    pub method: Method,
    pub prepared: PreparedRequest,
    pub control: SessionControl,
    /// Written to a VM actor's own session.
    pub vm: bool,
}

impl Commit {
    pub async fn run(self) -> Result<Committed, WorkerError> {
        if !self.receipt_checked {
            let operation = self.context.operation.clone();
            if self
                .bridge
                .worker_run("was_submitted", move |store| async move {
                    store::was_submitted(&store, &operation).await
                })
                .await?
            {
                return Ok(Committed::Nothing);
            }
        }
        if matches!(
            self.method,
            Method::ThreadStart | Method::TurnStart | Method::TurnInterrupt | Method::ThreadResume
        ) {
            let launch =
                vm_launch_request(&self.bridge, &self.context.intent, self.generation).await;
            if !matches!(launch, Ok(None)) {
                // The worker completes the effect after the VM commit.
                let mut commit = self;
                commit.effect = None;
                return Ok(Committed::Vm(Box::new(commit), launch));
            }
        }
        let control = self.control.clone();
        self.finish(control, false).await
    }

    /// Validate the request on `control`'s session, then commit `submitted`.
    pub async fn finish(self, control: SessionControl, vm: bool) -> Result<Committed, WorkerError> {
        let Self {
            bridge,
            account,
            connection_id,
            timeout_ms,
            id,
            context,
            method,
            params,
            interrupt_turn,
            submission,
            guard,
            effect,
            ..
        } = self;
        let prepared = match crate::diagnostics::measure_runtime_stage_async(
            "turn_submission.prepare",
            control.prepare(RequestSpec {
                id: id.clone(),
                operation_id: OperationId(context.operation.clone()),
                method,
                params,
                thread_id: context.intent.thread_id.clone(),
                lightweight: method == Method::TurnInterrupt,
                timeout_ms,
            }),
        )
        .await
        {
            Ok(prepared) => prepared,
            Err(SessionTaskError::Closed) if vm => return Ok(Committed::Nothing),
            Err(error) => return Err(error.into()),
        };
        let recovery = prepared.recovery_record();
        let save_context = context.clone();
        let authorized = crate::diagnostics::measure_runtime_stage_async(
            "turn_submission.submitted",
            bridge.worker_run("submitted", move |store| async move {
                store::submitted(
                    &store,
                    save_context,
                    &account,
                    recovery,
                    store::SubmissionEvidence {
                        turn: interrupt_turn.map(|(turn, _)| turn),
                        submission,
                        guard,
                        connection_id,
                        effect,
                    },
                )
                .await
            }),
        )
        .await?;
        if !authorized {
            return Ok(Committed::Nothing);
        }
        Ok(Committed::Ready(Box::new(Ready {
            id,
            context,
            method,
            prepared,
            control,
            vm,
        })))
    }
}

/// The agent's VM launch request, or `None` for a host agent.
async fn vm_launch_request(
    bridge: &StoreBridge,
    intent: &TurnIntent,
    generation: u64,
) -> Result<Option<preparation::VmProviderLaunchRequest>, WorkerError> {
    let agent_id = intent.agent_id.clone();
    let lookup_id = agent_id.clone();
    let intent_cwd = intent.cwd.clone();
    let account_key = intent.account_key.clone();
    let provider = intent.provider.key().to_owned();
    let intent_epoch = intent.epoch;
    bridge
        .worker_run("vm_launch_request", move |store| async move {
            store
                .read_async("runtime-services/account_worker_read", move |connection| {
                    let Some(record) =
                        AgentRepository::get(connection, &AgentId::from(lookup_id.as_str()))?
                    else {
                        return Err(WorkerError::PreparationRefused("vm-agent/missing".into()));
                    };
                    if record.extra.get("executionMode").and_then(Value::as_str) != Some("vm") {
                        return Ok(None);
                    }
                    if !crate::linux_vm_provider::vm_actor_matches(
                        connection,
                        &record.id.0,
                        intent_epoch,
                        &account_key,
                        Some(&provider),
                    )? {
                        return Err(WorkerError::PreparationRefused(
                            "vm-agent/identity-changed".into(),
                        ));
                    }
                    let cwd = record
                        .extra
                        .get("cwd")
                        .and_then(Value::as_str)
                        .or(intent_cwd.as_deref())
                        .filter(|value| value.starts_with('/'))
                        .ok_or_else(|| WorkerError::PreparationRefused("vm-agent/cwd".into()))?;
                    let guest_home = record
                        .extra
                        .get("layrHome")
                        .and_then(Value::as_str)
                        .filter(|value| value.starts_with('/'))
                        .unwrap_or(crate::linux_vm_provider::DEFAULT_GUEST_HOME);
                    Ok(Some(preparation::VmProviderLaunchRequest {
                        agent_id: record.id.0,
                        epoch: record.epoch,
                        generation,
                        account_key,
                        provider,
                        cwd: cwd.to_owned(),
                        guest_home: guest_home.to_owned(),
                    }))
                })
                .await
        })
        .await
}
