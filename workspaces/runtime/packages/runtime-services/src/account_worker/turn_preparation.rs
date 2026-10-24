//! Per-agent turn preparation (wake claim, auth check, admission, workspace,
//! ready snapshot) and the FULL submitted commit run as the agent's own tasks,
//! concurrently with other agents. The account worker keeps only the provider
//! write, fences, cancellation and unknown outcomes (see `submission`).
use super::*;
use crate::{diagnostics::measure_runtime_stage_async as stage, workspace_ready::ReadyToken};

pub(super) struct Run {
    pub bridge: StoreBridge,
    pub resources: Arc<preparation::PreparationResources>,
    pub generation: u64,
    pub connection_id: String,
    pub intent: TurnIntent,
    pub method: Method,
    /// Agent-row commit sequence read before the attempt; a pending step
    /// waits for the rows it depends on to change after it.
    pub since: u64,
    /// Durable effect completed only after this preparation is handled.
    pub effect: Option<String>,
    /// This connection's loaded-thread proof; with unchanged thread settings a
    /// thread request becomes the turn itself.
    pub loaded: Option<crate::thread_prepare::LoadedIdentity>,
    /// Workspace token of this attempt's accepted thread request; its commit
    /// froze the settings, so the turn needs no separate admission.
    pub carried: Option<ReadyToken>,
}

impl Run {
    /// The preparation may submit a turn, so the pre-start gates apply.
    fn may_start_turn(&self) -> bool {
        self.method == Method::TurnStart || self.loaded.is_some()
    }
}

pub(super) struct Done {
    pub agent_id: String,
    pub effect: Option<String>,
    pub outcome: Result<Outcome, WorkerError>,
}

pub(super) enum Outcome {
    Prepared(Box<Prepared>),
    /// A wake whose claim started no turn preparation.
    Unclaimed(Unclaimed),
    /// A prepared request's submitted commit; the worker writes it.
    Committed(submission::Committed),
}

pub(super) struct Prepared {
    pub intent: TurnIntent,
    /// The requested method; the prepared request may carry another one.
    pub method: Method,
    pub since: u64,
    /// The loaded-thread proof this preparation planned with.
    pub loaded: Option<crate::thread_prepare::LoadedIdentity>,
    pub step: store::PreparationStep<preparation::RequestPreparation>,
}

/// What a wake's claim found instead of a turn to prepare.
pub(super) enum Unclaimed {
    /// Busy-turn input claimed for the running turn, and that turn's id.
    Steer(Box<TurnIntent>, String, Box<preparation::PreparedSteer>),
    /// The disk is full: the input stays queued at this epoch until the recheck.
    DiskFull(i64),
    /// A queued native review child: its first turn is `review/start`.
    NativeReview,
    /// Nothing to start now.
    Nothing,
}

/// One agent's wake. Its claim runs in this task, so a wake never waits
/// behind other agents' submissions in the account worker.
pub(super) struct Wake {
    pub bridge: StoreBridge,
    /// The account registry, for the live start throttle; fixtures have none.
    pub driver: Option<Arc<crate::account_driver::AccountDriver>>,
    pub account: String,
    pub agent_id: String,
    pub epoch: i64,
    pub effect: Option<String>,
    /// Native review children run their first turn with these resources.
    pub resources: Arc<preparation::PreparationResources>,
}

pub(super) async fn run(run: Run) -> Box<Done> {
    let step = Box::pin(stage("turn_preparation.total", prepare(&run))).await;
    Box::new(Done {
        agent_id: run.intent.agent_id.clone(),
        effect: run.effect,
        outcome: step.map(|step| {
            Outcome::Prepared(Box::new(Prepared {
                intent: run.intent,
                method: run.method,
                since: run.since,
                loaded: run.loaded,
                step,
            }))
        }),
    })
}

/// Claim the agent's queued input, then prepare the claimed turn that
/// `start` describes in the same task.
pub(super) async fn wake(
    wake: Wake,
    start: impl FnOnce(TurnIntent, Option<String>) -> Run + Send,
) -> Box<Done> {
    let outcome = match stage("turn_preparation.claim", claim(&wake)).await {
        Ok(Ok(intent)) => return Box::pin(run(start(intent, wake.effect))).await,
        Ok(Err(Unclaimed::NativeReview)) => {
            // The driver saves every outcome on the child before it returns.
            if let Err(error) = crate::native_review_turn::run(
                Arc::clone(&wake.bridge.store),
                &wake.resources,
                &wake.agent_id,
                wake.epoch,
            )
            .await
            {
                eprintln!(
                    "native review for agent {} did not start: {error}",
                    wake.agent_id
                );
            }
            Ok(Outcome::Unclaimed(Unclaimed::Nothing))
        }
        Ok(Err(unclaimed)) => Ok(Outcome::Unclaimed(unclaimed)),
        Err(error) => Err(error),
    };
    Box::new(Done {
        agent_id: wake.agent_id,
        effect: wake.effect,
        outcome,
    })
}

/// Commit a prepared request in the agent's task. A ready commit deleted
/// `effect` in its submitted transaction; otherwise the worker completes it.
pub(super) async fn commit(
    agent_id: String,
    effect: Option<String>,
    mut commit: submission::Commit,
) -> Box<Done> {
    commit.effect = effect.clone();
    let outcome = Box::pin(commit.run()).await;
    Box::new(Done {
        agent_id,
        effect: effect.filter(|_| !matches!(outcome, Ok(submission::Committed::Ready(_)))),
        outcome: outcome.map(Outcome::Committed),
    })
}

async fn claim(wake: &Wake) -> Result<Result<TurnIntent, Unclaimed>, WorkerError> {
    let Wake {
        bridge,
        driver,
        account,
        agent_id,
        epoch,
        ..
    } = wake;
    let epoch = *epoch;
    let owner = agent_id.clone();
    let owner_account = account.clone();
    bridge
        .worker_run("quarantine_pending_metadata", move |store| async move {
            let read_owner = owner.clone();
            let read_account = owner_account.clone();
            let invalid = store
                .read_async("runtime-services/pending_metadata", move |db| {
                    let owned = AgentRepository::get(db, &AgentId::from(read_owner.as_str()))?
                        .is_some_and(|agent| {
                            agent.epoch == epoch
                                && agent
                                    .extra
                                    .get("accountKey")
                                    .and_then(Value::as_str)
                                    .unwrap_or("default")
                                    == read_account
                        });
                    if owned {
                        SchedulerRepository::invalid_pending_metadata(db, &read_owner, epoch)
                    } else {
                        Ok(BTreeMap::new())
                    }
                })
                .await?;
            if !invalid.is_empty() {
                scheduler::lifecycle_transaction_with(&store, move |tx| {
                    if AgentRepository::get(tx, &AgentId::from(owner.as_str()))?.is_some_and(
                        |agent| {
                            agent.epoch == epoch
                                && agent
                                    .extra
                                    .get("accountKey")
                                    .and_then(Value::as_str)
                                    .unwrap_or("default")
                                    == owner_account
                        },
                    ) {
                        let invalid =
                            SchedulerRepository::invalid_pending_metadata(tx, &owner, epoch)?;
                        if !invalid.is_empty() {
                            SchedulerRepository::quarantine_pending_events(
                                tx,
                                &owner,
                                epoch,
                                &invalid,
                                "Invalid stored event metadata",
                            )?;
                        }
                    }
                    Ok::<_, WorkerError>(())
                })
                .await?;
            }
            Ok(())
        })
        .await?;
    let (a, g) = (account.clone(), agent_id.clone());
    match bridge
        .worker_run("claim_steer", move |store| async move {
            steer::claim_for_wake(store, &a, &g, epoch).await
        })
        .await?
    {
        steer::WakeClaim::Steer(intent, turn) => {
            let resources = wake.resources.clone();
            let prepare_intent = intent.clone();
            let expected_turn = turn.clone();
            let prepared = bridge
                .prepare_run("steer_preparation", move |store| async move {
                    preparation::prepare_steer(store, &resources, &prepare_intent, &expected_turn)
                        .await
                })
                .await;
            match prepared {
                Ok(prepared) => return Ok(Err(Unclaimed::Steer(intent, turn, Box::new(prepared)))),
                Err(error) => {
                    eprintln!("busy-turn input for {} deferred: {error}", intent.agent_id);
                    let reason = error.to_string();
                    let resources = wake.resources.clone();
                    let retry = bridge
                        .worker_run("steer_preparation_refused", move |store| async move {
                            preparation::refuse_steer(store, &resources, &intent, &reason).await
                        })
                        .await?;
                    if retry {
                        return Box::pin(claim(wake)).await;
                    }
                    return Ok(Err(Unclaimed::Nothing));
                }
            }
        }
        steer::WakeClaim::Busy => return Ok(Err(Unclaimed::Nothing)),
        steer::WakeClaim::Idle => {}
    }
    // The claim below refuses an agent that an account transfer holds.
    if !Box::pin(stage(
        "turn_preparation.account_transfer",
        advance_transfer(wake),
    ))
    .await?
    {
        return Ok(Err(Unclaimed::Nothing));
    }
    // A new turn waits for the account's start throttle before its claim, so
    // its input stays queued meanwhile; a slot not claimed below is freed. A
    // host without an account registry (fixtures; production hosts always
    // have one) has no throttle settings, as for the auth gate.
    let slot = match driver {
        Some(driver) => Some(
            stage(
                "turn_preparation.start_gate",
                bridge.start_gates.gate(account).admit(
                    agent_id,
                    epoch,
                    crate::account_start_gate::StartSources {
                        store: Arc::clone(&bridge.store),
                        agent_changes: bridge.agent_changes(),
                        driver: Arc::clone(driver),
                        account: account.clone(),
                    },
                ),
            )
            .await
            .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?,
        ),
        None => None,
    };
    if crate::disk_space::hold_agent_turn(
        &bridge.store,
        agent_id,
        epoch,
        #[cfg(test)]
        bridge.disk_gate.as_deref(),
    )
    .await?
    {
        // While the disk is full no new turn starts. The input stays queued
        // with its identity; the desktop shows one notice with the free space.
        return Ok(Err(Unclaimed::DiskFull(epoch)));
    }
    // Python's dispatcher claims a pending native review before queued input.
    if crate::native_review_turn::candidate(&bridge.store, agent_id, epoch, account).await? {
        return Ok(Err(Unclaimed::NativeReview));
    }
    loop {
        let (a, g) = (account.clone(), agent_id.clone());
        match bridge
            .worker_run("claim", move |store| async move {
                store::claim(store, &a, &g, epoch).await
            })
            .await
        {
            Ok(Some(intent)) => {
                if let Some(slot) = slot {
                    slot.claimed();
                }
                return Ok(Ok(intent));
            }
            Ok(None) => return Ok(Err(Unclaimed::Nothing)),
            Err(error) if invalid_agent_record_error(&error) => {
                let (invalid_id, pointer, reason) = invalid_agent_record_details(&error)
                    .expect("guard matches InvalidSyncObjectField");
                let invalid_id = invalid_id.to_owned();
                let pointer = pointer.to_owned();
                let reason = reason.to_owned();
                let was_requested_agent = invalid_id == *agent_id;
                let diagnostic = error.to_string();
                bridge
                    .worker_run("hold_invalid_agent", move |store| async move {
                        store::hold_invalid_agent_record(
                            &store,
                            &invalid_id,
                            &pointer,
                            &reason,
                            &diagnostic,
                        )
                        .await
                    })
                    .await?;
                if was_requested_agent {
                    return Ok(Err(Unclaimed::Nothing));
                }
            }
            Err(error) => return Err(error),
        }
    }
}

async fn prepare(
    run: &Run,
) -> Result<store::PreparationStep<preparation::RequestPreparation>, WorkerError> {
    let Run {
        bridge,
        resources,
        intent,
        ..
    } = run;
    let may_start_turn = run.may_start_turn();
    if may_start_turn && !stage("turn_preparation.account_auth", require_account_auth(run)).await? {
        return Ok(store::PreparationStep::Settled);
    }
    if let Some(token) = run.carried.clone() {
        let i = intent.clone();
        let r = resources.clone();
        stage(
            "turn_preparation.admit",
            bridge.prepare_run("preparation", move |s| async move {
                before_start(&s, &r, &i).await
            }),
        )
        .await?;
        match Box::pin(ready(run, None, token)).await {
            // A stale carried token or unfrozen settings take the full path.
            Err(WorkerError::PreparationRefused(_)) => {}
            result => return Box::pin(settle(run, result)).await,
        }
    }
    let i = intent.clone();
    let r = resources.clone();
    let selection = match stage(
        "turn_preparation.admit",
        bridge.prepare_run("preparation", move |s| async move {
            if may_start_turn {
                before_start(&s, &r, &i).await?;
            }
            store::preparation_step(&s, &r, &i, preparation::admit(Arc::clone(&s), &r, &i)).await
        }),
    )
    .await?
    .into_ready()
    {
        Ok(selection) => selection,
        Err(step) => return Ok(step),
    };
    let i = intent.clone();
    let r = resources.clone();
    let token = match Box::pin(stage(
        "turn_preparation.workspace",
        bridge.prepare_run("workspace", move |s| async move {
            Box::pin(store::preparation_step(
                &s,
                &r,
                &i,
                preparation::workspace(Arc::clone(&s), &r, &i),
            ))
            .await
        }),
    ))
    .await?
    .into_ready()
    {
        Ok(token) => token,
        Err(step) => return Ok(step),
    };
    let result = Box::pin(ready(run, Some(selection), token)).await;
    Box::pin(settle(run, result)).await
}

fn transfer_error(error: crate::account_transfer::TransferError) -> WorkerError {
    match error {
        crate::account_transfer::TransferError::Store(error) => WorkerError::Store(error),
        error => WorkerError::PreparationRefused(error.to_string()),
    }
}

/// Advance the agent's account transfer at its wake, before its input is
/// claimed. A member that was busy at the request is released to the
/// destination account, whose worker then claims the input. A lazy member of
/// this account moves its history here first. `false` leaves the input queued;
/// the transfer record names the wait or the refusal.
async fn advance_transfer(wake: &Wake) -> Result<bool, WorkerError> {
    let Wake {
        bridge,
        account,
        agent_id,
        resources,
        ..
    } = wake;
    let (g, a) = (agent_id.clone(), account.clone());
    // The member's phase, when an account transfer holds this agent.
    let held = bridge
        .worker_run("account_transfer_hold", move |store| async move {
            store
                .read_async("runtime-services/account_transfer_hold", move |db| {
                    let Some(agent) = AgentRepository::get(db, &AgentId::from(g.as_str()))? else {
                        return Ok::<_, WorkerError>(None);
                    };
                    let Some(operation) = agent
                        .extra
                        .get("accountTransferId")
                        .and_then(Value::as_str)
                        .filter(|id| !id.is_empty())
                    else {
                        return Ok(None);
                    };
                    let phase =
                        studio_runtime_store::AccountTransferRepository::get(db, operation)?
                            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                            .and_then(|record| {
                                record["members"][g.as_str()]["phase"]
                                    .as_str()
                                    .map(str::to_owned)
                            })
                            .unwrap_or_default();
                    let lazy_here =
                        studio_runtime_store::truthy(agent.extra.get("lazyAccountTransfer"))
                            && agent
                                .extra
                                .get("accountKey")
                                .and_then(Value::as_str)
                                .unwrap_or("default")
                                == a;
                    Ok(Some((phase, lazy_here)))
                })
                .await
        })
        .await?;
    let Some((phase, lazy_here)) = held else {
        return Ok(true);
    };
    // Any other phase waits for its receipt, Retry or Cancel; the transfer
    // record names the reason, so a wake neither writes nor logs.
    if phase == "waiting" && !lazy_here {
        let g = agent_id.clone();
        match bridge
            .worker_run("account_transfer_release", move |store| async move {
                crate::account_transfer_driver::release_waiting(&store, &g, now())
                    .await
                    .map_err(transfer_error)
            })
            .await
        {
            Ok(true) => bridge.event_wake().notify_one(),
            Ok(false) => {}
            Err(WorkerError::PreparationRefused(reason)) => {
                eprintln!("account transfer of {agent_id} was not released: {reason}");
            }
            Err(error) => return Err(error),
        }
        return Ok(false);
    }
    if phase != "lazy" || !lazy_here {
        return Ok(false);
    }
    let (g, r) = (agent_id.clone(), resources.clone());
    let moved = bridge
        .prepare_run("account_transfer_move", move |store| async move {
            crate::account_transfer_executor::before_start(
                store,
                r.account_session.clone(),
                &r.model_catalog,
                &r,
                &g,
                now(),
            )
            .await
            .map_err(transfer_error)
        })
        .await;
    match moved {
        Ok(agent) => Ok(!studio_runtime_store::truthy(
            agent.get("accountTransferId"),
        )),
        Err(WorkerError::PreparationRefused(reason)) => {
            eprintln!("account transfer of {agent_id} did not move: {reason}");
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

/// Apply an account transfer requested before this turn starts.
async fn before_start(
    store: &Arc<RuntimeStore>,
    resources: &preparation::PreparationResources,
    intent: &TurnIntent,
) -> Result<(), WorkerError> {
    crate::account_transfer_executor::before_start(
        Arc::clone(store),
        resources.account_session.clone(),
        &resources.model_catalog,
        resources,
        &intent.agent_id,
        now(),
    )
    .await
    .map(|_| ())
    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
}

async fn ready(
    run: &Run,
    selection: Option<preparation::Admission>,
    token: ReadyToken,
) -> Result<preparation::RequestPreparation, WorkerError> {
    let i = run.intent.clone();
    let r = run.resources.clone();
    let method = run.method;
    let generation = run.generation;
    let connection_id = run.connection_id.clone();
    let loaded = run.loaded.clone();
    let ready = run.bridge.prepare_run("preparation", move |s| async move {
        Box::pin(preparation::prepare_ready(
            Arc::clone(&s),
            &r,
            &i,
            method,
            generation,
            &connection_id,
            selection,
            token,
            loaded,
        ))
        .await
    });
    Box::pin(stage("turn_preparation.ready", ready)).await
}

async fn settle(
    run: &Run,
    result: Result<preparation::RequestPreparation, WorkerError>,
) -> Result<store::PreparationStep<preparation::RequestPreparation>, WorkerError> {
    Box::pin(store::preparation_step(
        &run.bridge.store,
        &run.resources,
        &run.intent,
        async { result },
    ))
    .await
}

/// Pre-start account auth gate for every provider (see
/// `account_auth_gate`). A host without an account registry (fixtures and
/// transfer probes; production hosts always have one) has no auth source to
/// gate on.
async fn require_account_auth(run: &Run) -> Result<bool, WorkerError> {
    let Some(driver) = run.resources.account_driver.clone() else {
        return Ok(true);
    };
    crate::account_auth_gate::require_before_start(
        run.bridge.store.clone(),
        driver,
        run.resources.claude_account_pin.clone(),
        &run.intent,
    )
    .await
    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
}
