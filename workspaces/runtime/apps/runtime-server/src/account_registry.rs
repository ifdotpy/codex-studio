//! Runtime ownership of the existing account workers; all input and recovery
//! decisions remain in the worker. Wakes are hints over committed SQLite rows.
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
    AgentId, AgentRepository, RuntimeStore, SchedulerRepository, StoreError,
};
use tokio::sync::{Notify, mpsc};

const MAX_ACCOUNTS: usize = 128;
const MAX_PENDING_AGENTS: usize = 4096;
const RETRY_DELAY: Duration = Duration::from_secs(1);

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
        })
    }

    pub(crate) async fn start(&mut self, account: &str) -> Result<(), WorkerError> {
        if self.workers.contains_key(account) {
            return Ok(());
        }
        if self.workers.len() == MAX_ACCOUNTS {
            return Err(WorkerError::Capacity);
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
        let targets = tokio::task::spawn_blocking(move || targets(&store))
            .await
            .map_err(|_| WorkerError::TaskFailed)??;
        let mut failure = None;
        for (account, agents) in targets {
            if stopped() {
                break;
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
                match worker.try_send(AccountCommand::Wake { agent_id, epoch }) {
                    Ok(()) => {}
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
        let mut retry = tokio::time::interval(RETRY_DELAY);
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        loop {
            if *stop.borrow() {
                break;
            }
            // Borrow existing join handles rather than introducing another
            // account task or a second command queue.
            let exited = {
                let mut exits: FuturesUnordered<_> =
                    self.workers
                        .iter_mut()
                        .map(|(account, worker)| async move {
                            (account.clone(), worker.completion().await)
                        })
                        .collect();
                tokio::select! { biased;
                    _ = Shutdown::cancelled(&mut stop) => break,
                    exit = exits.next(), if !exits.is_empty() => exit,
                    _ = wake.notified() => None,
                    _ = retry.tick() => None,
                }
            };
            if let Some((account, result)) = exited {
                self.workers.remove(&account);
                if let Err(error) = result {
                    eprintln!("account worker ended: {error}");
                }
                // Do not reconnect in a hot loop after an unavailable provider.
                continue;
            }
            if let Err(error) = self.reconcile(Some(&shutdown)).await {
                eprintln!("account registry reconciliation deferred: {error}");
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

pub(crate) fn targets(
    store: &RuntimeStore,
) -> Result<BTreeMap<String, Vec<(String, i64)>>, WorkerError> {
    let db = store.read()?;
    let pending = SchedulerRepository::pending_agents(&db)?;
    if pending.len() > MAX_PENDING_AGENTS {
        return Err(WorkerError::Capacity);
    }
    let mut targets: BTreeMap<String, Vec<(String, i64)>> = BTreeMap::new();
    for (id, epoch) in pending {
        let agent = AgentRepository::get(&db, &AgentId::from(id.as_str()))?.ok_or_else(|| {
            StoreError::InvalidStoredObject {
                table: "runtime_agents",
                id: id.clone(),
            }
        })?;
        let account = match agent.extra.get("accountKey") {
            None | Some(serde_json::Value::Null) => "default",
            Some(serde_json::Value::String(key)) if !key.is_empty() => key,
            _ => {
                return Err(StoreError::InvalidStoredObject {
                    table: "runtime_agents",
                    id,
                }
                .into());
            }
        };
        targets
            .entry(account.to_owned())
            .or_default()
            .push((id, epoch));
    }
    for account in SchedulerRepository::accounts_with_unknown_submitted_turns(&db)? {
        targets.entry(account).or_default();
    }
    // A disconnected request may already have moved into restartRecovery.
    // The worker alone interprets these snapshots and fences stale generations.
    let mut statement = db.prepare(
        "SELECT DISTINCT COALESCE(json_extract(record,'$.accountKey'),'default') \
         FROM runtime_agents WHERE json_extract(record,'$.deletedAt') IS NULL \
         AND COALESCE(json_extract(record,'$.remoteWorker'),0)=0 \
         AND COALESCE(json_extract(record,'$.remoteAnchor'),0)=0 \
         AND ((json_type(record,'$.disconnectRecovery')='object' AND (\
               json_extract(record,'$.status') NOT IN ('completed','waiting','paused','parked') \
               OR json_extract(record,'$.disconnectRecovery.stage') IS NOT NULL)) \
           OR (json_type(record,'$.restartRecovery')='object' AND (\
               json_extract(record,'$.status') NOT IN ('completed','waiting','paused','parked') \
               OR COALESCE(json_extract(record,'$.restartRecovery.stage'),'') \
                  NOT IN ('finished','continued','reattached','input_restored')))) LIMIT ?1",
    )?;
    for account in
        statement.query_map([(MAX_ACCOUNTS + 1) as i64], |row| row.get::<_, String>(0))?
    {
        targets.entry(account?).or_default();
    }
    if targets.len() > MAX_ACCOUNTS {
        return Err(WorkerError::Capacity);
    }
    Ok(targets)
}
