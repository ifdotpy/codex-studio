//! Runtime ownership of the existing account workers; all input and recovery
//! decisions remain in the worker. Wakes are hints over committed SQLite rows.
//! Registered runtime difference: `image-workspace-dependency-hold` persists
//! an image-workspace refusal on the agent and leaves its input event pending.
use futures_util::{StreamExt, stream::FuturesUnordered};
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use studio_runtime_process::provider_seam::{OpenOptions, ProviderTaskFactory, Shutdown};
use studio_runtime_services::{
    account_worker::{
        AccountCommand, AccountHandle, StoreBridge, WorkerError, preparation::PreparationResources,
        start_account_worker_with_preparation,
    },
    provider_session_task::SessionTaskConfig,
};
use studio_runtime_store::{
    AgentId, AgentRepository, AgentStatus, RuntimeStore, SchedulerRepository,
};
use tokio::sync::{Notify, mpsc};

const MAX_ACCOUNTS: usize = 128;
const WAKE_PAGE: usize = 32;
const RETRY_DELAY: Duration = Duration::from_secs(1);
const MAX_RETRY_DELAY: Duration = Duration::from_secs(30);
const IMAGE_EXECUTION_DEPENDENCY: &str = "workspace-state/image-execution";

fn next_retry_delay(current: Duration) -> Duration {
    current.saturating_mul(2).min(MAX_RETRY_DELAY)
}

fn defer_wake(
    last_error: Option<&str>,
    now: tokio::time::Instant,
    retry_at: tokio::time::Instant,
) -> bool {
    last_error.is_some() && now < retry_at
}

enum RegistryTrigger {
    Wake,
    Retry,
    WorkerExited(String, Result<(), WorkerError>),
}

pub(crate) type LaunchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<AccountLaunch, WorkerError>> + Send + 'a>>;

pub(crate) struct AccountLaunch {
    pub factory: Arc<dyn ProviderTaskFactory>,
    pub options: OpenOptions,
    pub config: SessionTaskConfig,
    pub resources: PreparationResources,
}

/// Launch resolution runs outside transactions. A successful return owns the
/// attachment until the worker consumes it; loss of a receipt never retries input.
pub(crate) trait AccountLauncher: Send + Sync {
    fn launch(&self, account: String) -> LaunchFuture<'_>;
}

pub(crate) struct AccountRegistry {
    store: Arc<RuntimeStore>,
    bridge: StoreBridge,
    launcher: Arc<dyn AccountLauncher>,
    workers: BTreeMap<String, AccountHandle>,
    after_account: String,
    after_agent: BTreeMap<String, String>,
}

impl AccountRegistry {
    pub async fn new(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
    ) -> Result<Self, WorkerError> {
        let bridge = StoreBridge::new(store.clone()).await?;
        Ok(Self {
            store,
            bridge,
            launcher,
            workers: BTreeMap::new(),
            after_account: String::new(),
            after_agent: BTreeMap::new(),
        })
    }

    pub(crate) async fn start(&mut self, account: &str) -> Result<(), WorkerError> {
        if self.workers.contains_key(account) {
            return Ok(());
        }
        let launch = self.launcher.launch(account.to_owned()).await?;
        let worker = start_account_worker_with_preparation(
            self.bridge.clone(),
            account.to_owned(),
            launch.factory,
            launch.options,
            launch.config,
            launch.resources,
        )
        .await?;
        self.workers.insert(account.to_owned(), worker);
        Ok(())
    }

    /// Startup and post-commit use the same persisted selection, so a crash
    /// between commit and notify cannot strand a queued event.
    pub async fn reconcile(&mut self, shutdown: Option<&Shutdown>) -> Result<(), WorkerError> {
        let stopped = || shutdown.is_some_and(|stop| *stop.subscribe().borrow());
        let store = self.store.clone();
        let after_account = self.after_account.clone();
        let after_agent = self.after_agent.clone();
        let targets =
            tokio::task::spawn_blocking(move || target_page(&store, &after_account, &after_agent))
                .await
                .map_err(|_| WorkerError::TaskFailed)??;
        let mut failure = None;
        for (account, pending_agents) in targets {
            if stopped() {
                break;
            }
            self.after_account.clone_from(&account);
            let image_agents = image_workspace_agents(&self.store, &pending_agents)?;
            hold_image_workspace_agents(&self.store, &image_agents)?;
            for agent_id in &image_agents {
                self.after_agent.insert(account.clone(), agent_id.clone());
            }
            let agents = pending_agents
                .into_iter()
                .filter(|(agent_id, _)| !image_agents.contains(agent_id))
                .collect::<Vec<_>>();
            let image_recovery =
                agents.is_empty() && has_image_workspace_recovery(&self.store, &account)?;
            // An image-only pending page must not start an account provider
            // outside the workspace image's shared mount namespace.
            if agents.is_empty()
                && (!image_agents.is_empty() || image_recovery)
                && !self.workers.contains_key(&account)
            {
                if image_recovery {
                    eprintln!(
                        "account registry refused recovery for account {account}: workspace-state/image-execution"
                    );
                }
                continue;
            }
            if let Err(error) = self.start(&account).await {
                failure.get_or_insert(error);
                continue;
            }
            if stopped() {
                break;
            }
            let worker = self.workers.get(&account).ok_or(WorkerError::Closed)?;
            for (agent_id, epoch) in agents {
                match worker.try_send(AccountCommand::Wake {
                    agent_id: agent_id.clone(),
                    epoch,
                }) {
                    Ok(()) => {
                        self.after_agent.insert(account.clone(), agent_id);
                    }
                    // Durable input remains pending. Timer/next commit retries
                    // the hint; only the worker can claim and submit it.
                    Err(mpsc::error::TrySendError::Full(_)) => break,
                    Err(mpsc::error::TrySendError::Closed(_)) => break,
                }
            }
        }
        failure.map_or(Ok(()), Err)
    }

    pub async fn run(mut self, wake: Arc<Notify>, shutdown: Shutdown) -> Result<(), WorkerError> {
        let mut stop = shutdown.subscribe();
        let mut retry_delay = RETRY_DELAY;
        let mut retry_at = tokio::time::Instant::now();
        let mut last_reconcile_error: Option<String> = None;
        loop {
            if *stop.borrow() {
                break;
            }
            // Borrow existing join handles rather than introducing another
            // account task or a second command queue.
            let trigger = {
                let mut exits: FuturesUnordered<_> =
                    self.workers
                        .iter_mut()
                        .map(|(account, worker)| async move {
                            (account.clone(), worker.completion().await)
                        })
                        .collect();
                tokio::select! { biased;
                    _ = Shutdown::cancelled(&mut stop) => break,
                    exit = exits.next(), if !exits.is_empty() => {
                        exit.map(|(account, result)| RegistryTrigger::WorkerExited(account, result))
                    },
                    _ = wake.notified() => Some(RegistryTrigger::Wake),
                    _ = tokio::time::sleep_until(retry_at) => Some(RegistryTrigger::Retry),
                }
            };
            match trigger {
                Some(RegistryTrigger::WorkerExited(account, result)) => {
                    self.workers.remove(&account);
                    if let Err(error) = result {
                        eprintln!("account worker ended: {error}");
                    }
                    // Do not reconnect in a hot loop after an unavailable provider.
                    continue;
                }
                Some(RegistryTrigger::Wake)
                    if defer_wake(
                        last_reconcile_error.as_deref(),
                        tokio::time::Instant::now(),
                        retry_at,
                    ) =>
                {
                    // Committed rows remain durable until the scheduled retry.
                    continue;
                }
                Some(RegistryTrigger::Wake | RegistryTrigger::Retry) => {}
                None => continue,
            }
            match self.reconcile(Some(&shutdown)).await {
                Err(error) => {
                    let message = error.to_string();
                    if last_reconcile_error.as_deref() != Some(&message) {
                        eprintln!("account registry reconciliation deferred: {message}");
                        last_reconcile_error = Some(message);
                    }
                    retry_at = tokio::time::Instant::now() + retry_delay;
                    retry_delay = next_retry_delay(retry_delay);
                }
                Ok(()) => {
                    if last_reconcile_error.take().is_some() {
                        eprintln!("account registry reconciliation recovered");
                    }
                    retry_delay = RETRY_DELAY;
                    retry_at = tokio::time::Instant::now() + RETRY_DELAY;
                }
            }
        }
        self.shutdown().await
    }

    pub async fn shutdown(&mut self) -> Result<(), WorkerError> {
        for worker in self.workers.values() {
            worker.cancel();
        }
        let mut failure = None;
        for (_, mut worker) in std::mem::take(&mut self.workers) {
            if let Err(error) = worker.completion().await {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

pub(crate) fn target_page(
    store: &RuntimeStore,
    after_account: &str,
    after_agent: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Vec<(String, i64)>>, WorkerError> {
    let db = store.read()?;
    SchedulerRepository::account_wake_page(&db, after_account, after_agent, MAX_ACCOUNTS, WAKE_PAGE)
        .map_err(Into::into)
}

fn image_workspace_agents(
    store: &RuntimeStore,
    agents: &[(String, i64)],
) -> Result<std::collections::BTreeSet<String>, WorkerError> {
    let db = store.read()?;
    let mut image_agents = std::collections::BTreeSet::new();
    for (agent_id, _) in agents {
        let mode = db.query_row(
            "SELECT json_extract(record,'$.workspaceMode') FROM runtime_agents WHERE id=?1",
            [agent_id],
            |row| row.get::<_, Option<String>>(0),
        )?;
        if mode.as_deref() == Some("image") {
            image_agents.insert(agent_id.clone());
        }
    }
    Ok(image_agents)
}

/// Keep the durable event pending while exposing the unavailable workspace executor on its agent.
fn hold_image_workspace_agents(
    store: &RuntimeStore,
    agents: &std::collections::BTreeSet<String>,
) -> Result<(), WorkerError> {
    store.transaction_with(|tx| {
        for id in agents {
            let Some(mut agent) = AgentRepository::get(tx, &AgentId::from(id.as_str()))? else {
                continue;
            };
            if agent
                .extra
                .get("workspaceMode")
                .and_then(|mode| mode.as_str())
                != Some("image")
            {
                continue;
            }
            if agent.status == AgentStatus::Paused
                && agent.extra.get("error").and_then(|value| value.as_str())
                    == Some(IMAGE_EXECUTION_DEPENDENCY)
                && agent
                    .extra
                    .get("nativeFailureHold")
                    .and_then(|value| value.as_bool())
                    == Some(true)
            {
                continue;
            }
            agent.status = AgentStatus::Paused;
            agent
                .extra
                .insert("autoWake".into(), serde_json::json!(false));
            agent
                .extra
                .insert("inFlight".into(), serde_json::json!(false));
            agent
                .extra
                .insert("nativeFailureHold".into(), serde_json::json!(true));
            agent.extra.insert(
                "error".into(),
                serde_json::json!(IMAGE_EXECUTION_DEPENDENCY),
            );
            agent.extra.insert(
                "startOutcomeHold".into(),
                serde_json::json!({"stage":"held","reason":IMAGE_EXECUTION_DEPENDENCY}),
            );
            AgentRepository::put(tx, &agent)?;
        }
        Ok::<_, WorkerError>(())
    })
}

fn has_image_workspace_recovery(store: &RuntimeStore, account: &str) -> Result<bool, WorkerError> {
    let db = store.read()?;
    Ok(db.query_row(
        "SELECT EXISTS(
          SELECT 1 FROM runtime_agents a
          WHERE COALESCE(json_extract(a.record,'$.accountKey'),'default')=?1
            AND json_extract(a.record,'$.workspaceMode')='image'
            AND json_extract(a.record,'$.deletedAt') IS NULL
            AND COALESCE(json_extract(a.record,'$.remoteWorker'),0)=0
            AND COALESCE(json_extract(a.record,'$.remoteAnchor'),0)=0
            AND ((COALESCE(json_extract(a.record,'$.provider'),'codex')='codex'
                  AND json_extract(a.record,'$.startAttempt.submitted')=1
                  AND json_extract(a.record,'$.startAttempt.connectionGeneration') IS NOT NULL
                  AND json_extract(a.record,'$.status') IN ('starting','running','waiting','paused','interrupted'))
              OR (json_type(a.record,'$.disconnectRecovery')='object' AND (
                  COALESCE(json_extract(a.record,'$.status'),'') NOT IN ('completed','waiting','paused','parked')
                  OR json_extract(a.record,'$.disconnectRecovery.stage') IS NOT NULL))
              OR (json_type(a.record,'$.restartRecovery')='object' AND (
                  COALESCE(json_extract(a.record,'$.status'),'') NOT IN ('completed','waiting','paused','parked')
                  OR COALESCE(json_extract(a.record,'$.restartRecovery.stage'),'')
                     NOT IN ('finished','continued','reattached','input_restored'))))
        )",
        [account],
        |row| row.get(0),
    )?)
}

#[cfg(test)]
mod tests {
    use super::{MAX_RETRY_DELAY, RETRY_DELAY, defer_wake, next_retry_delay};
    use std::time::Duration;

    #[test]
    fn reconciliation_retry_delay_doubles_to_a_cap() {
        let delays = (0..8)
            .scan(RETRY_DELAY, |delay, _| {
                let current = *delay;
                *delay = next_retry_delay(current);
                Some(current)
            })
            .collect::<Vec<_>>();

        assert_eq!(
            delays,
            [1, 2, 4, 8, 16, 30, 30, 30].map(Duration::from_secs),
        );
        assert_eq!(next_retry_delay(MAX_RETRY_DELAY), MAX_RETRY_DELAY);
    }

    #[test]
    fn reconciliation_wakes_wait_for_the_scheduled_retry_after_failure() {
        let now = tokio::time::Instant::now();
        let retry_at = now + Duration::from_secs(2);
        assert!(defer_wake(Some("supervisor unavailable"), now, retry_at));
        assert!(!defer_wake(None, now, retry_at));
        assert!(!defer_wake(
            Some("supervisor unavailable"),
            retry_at,
            retry_at,
        ));
    }
}
