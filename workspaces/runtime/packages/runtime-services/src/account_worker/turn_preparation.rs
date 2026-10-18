//! Per-agent turn preparation (auth check, admission, workspace, ready
//! snapshot) runs as its own task, concurrently with other agents. The account
//! worker keeps only the serialized submission: the FULL submitted commit,
//! the provider write, fences, cancellation and unknown outcomes.
use super::*;
use crate::{diagnostics::measure_runtime_stage_async as stage, workspace_ready::ReadyToken};

pub(super) struct Run {
    pub bridge: StoreBridge,
    pub resources: preparation::PreparationResources,
    pub retained_port: retained::RetainedSessionPort,
    pub generation: u64,
    pub connection_id: String,
    pub intent: TurnIntent,
    pub method: Method,
    /// Workspace token of this attempt's thread preparation, reused by its
    /// first turn so one agent turn runs one workspace pass.
    pub thread_workspace: Option<ReadyToken>,
    /// Subscribed before the attempt; a pending step waits for its next change.
    pub agent_changes: watch::Receiver<u64>,
    /// Durable effect completed only after this preparation is handled.
    pub effect: Option<String>,
}

pub(super) struct Done {
    pub intent: TurnIntent,
    pub agent_changes: watch::Receiver<u64>,
    pub effect: Option<String>,
    pub outcome: Result<store::PreparationStep<preparation::RequestPreparation>, WorkerError>,
}

pub(super) async fn run(run: Run) -> Box<Done> {
    let outcome = stage("turn_preparation.total", prepare(&run)).await;
    Box::new(Done {
        intent: run.intent,
        agent_changes: run.agent_changes,
        effect: run.effect,
        outcome,
    })
}

async fn prepare(
    run: &Run,
) -> Result<store::PreparationStep<preparation::RequestPreparation>, WorkerError> {
    let Run {
        bridge,
        resources,
        intent,
        method,
        ..
    } = run;
    let method = *method;
    if method == Method::TurnStart
        && !stage("turn_preparation.account_auth", require_account_auth(run)).await?
    {
        return Ok(store::PreparationStep::Settled);
    }
    let i = intent.clone();
    let r = resources.clone();
    let selection = match stage(
        "turn_preparation.admit",
        bridge.prepare_run("preparation", move |s| async move {
            if method == Method::TurnStart {
                crate::account_transfer_executor::before_start(
                    Arc::clone(&s),
                    r.account_session.clone(),
                    &r.model_catalog,
                    &r,
                    &i.agent_id,
                    now(),
                )
                .await
                .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
            }
            store::preparation_step(&s, &i, preparation::admit(Arc::clone(&s), &r, &i)).await
        }),
    )
    .await?
    .into_ready()
    {
        Ok(selection) => selection,
        Err(step) => return Ok(step),
    };
    if let Some(token) = run.thread_workspace.clone() {
        // The token is revalidated inside the ready snapshot's transaction; a
        // stale one falls back to a fresh workspace pass instead of refusing.
        match ready(run, selection.clone(), token).await {
            Err(WorkerError::PreparationRefused(reason))
                if reason == preparation::WORKSPACE_TOKEN_STALE => {}
            result => return settle(run, result).await,
        }
    }
    let i = intent.clone();
    let r = resources.clone();
    let token = match stage(
        "turn_preparation.workspace",
        bridge.prepare_run("workspace", move |s| async move {
            store::preparation_step(&s, &i, preparation::workspace(Arc::clone(&s), &r, &i)).await
        }),
    )
    .await?
    .into_ready()
    {
        Ok(token) => token,
        Err(step) => return Ok(step),
    };
    let result = ready(run, selection, token).await;
    settle(run, result).await
}

async fn ready(
    run: &Run,
    selection: preparation::Admission,
    token: ReadyToken,
) -> Result<preparation::RequestPreparation, WorkerError> {
    let i = run.intent.clone();
    let r = run.resources.clone();
    let method = run.method;
    let generation = run.generation;
    let connection_id = run.connection_id.clone();
    let ready = run.bridge.prepare_run("preparation", move |s| async move {
        preparation::prepare_ready(
            Arc::clone(&s),
            &r,
            &i,
            method,
            generation,
            &connection_id,
            selection,
            token,
        )
        .await
    });
    stage("turn_preparation.ready", ready).await
}

async fn settle(
    run: &Run,
    result: Result<preparation::RequestPreparation, WorkerError>,
) -> Result<store::PreparationStep<preparation::RequestPreparation>, WorkerError> {
    store::preparation_step(&run.bridge.store, &run.intent, async { result }).await
}

/// Pre-start account auth gate for every provider (see
/// `claude_auth_runtime::require_before_start`). Claude's latch proof reads the
/// account through this worker's retained port, which the worker serves while
/// this task waits. A host without an account registry (fixtures and transfer
/// probes; production hosts always have one) has no auth source to gate on.
async fn require_account_auth(run: &Run) -> Result<bool, WorkerError> {
    let Some(driver) = run.resources.account_driver.clone() else {
        return Ok(true);
    };
    let port = run.retained_port.clone();
    crate::claude_auth_runtime::require_before_start(
        run.bridge.store.clone(),
        driver,
        run.resources.claude_account_pin.clone(),
        &run.intent,
        |actor, deadline| async move {
            crate::claude_auth_runtime::account_read(&port, actor, deadline).await
        },
    )
    .await
    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
}
