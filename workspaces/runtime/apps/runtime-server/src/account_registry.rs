//! Runtime ownership of the existing account workers; all input and recovery
//! decisions remain in the worker. Wakes are hints over committed SQLite rows.
use futures_util::{StreamExt, stream::FuturesUnordered};
use rusqlite::OptionalExtension;
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use studio_runtime_process::provider_seam::{OpenOptions, ProviderTaskFactory, Shutdown};
use studio_runtime_services::{
    account_worker::{
        AccountCommand, AccountHandle, StoreBridge, WorkerError, preparation::PreparationResources,
        start_account_worker_with_preparation,
    },
    provider_session_task::SessionTaskConfig,
};
use studio_runtime_store::{RuntimeStore, StoreError};
use tokio::sync::{Notify, mpsc};

const MAX_ACCOUNTS: usize = 128;
const WAKE_PAGE: usize = 32;
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
        for (account, agents) in targets {
            if stopped() {
                break;
            }
            self.after_account.clone_from(&account);
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

// One pass is bounded independently of the total durable backlog. Account and
// agent keyset cursors wrap only at the end; blocked claims cannot monopolize a
// full wake lane on every pass. Cursor advancement follows successful enqueue.
const TARGETS: &str = "WITH targets AS (
 SELECT a.id, COALESCE(json_extract(a.record,'$.accountKey'),'default') account,
 json_type(a.record,'$.accountKey') account_type,
 json_extract(a.record,'$.epoch') epoch,
 (json_extract(a.record,'$.status')='queued'
  AND json_extract(a.record,'$.autoWake')=1
  AND COALESCE(json_extract(a.record,'$.inFlight'),0)=0
  AND COALESCE(json_extract(a.record,'$.nativeFailureHold'),0)=0
  AND EXISTS(SELECT 1 FROM runtime_events e WHERE e.agent=a.id
     AND e.epoch=json_extract(a.record,'$.epoch') AND e.status='pending')) pending,
 ((COALESCE(json_extract(a.record,'$.provider'),'codex')='codex'
   AND json_extract(a.record,'$.startAttempt.submitted')=1
   AND json_extract(a.record,'$.startAttempt.connectionGeneration') IS NOT NULL
   AND json_extract(a.record,'$.status') IN ('starting','running','waiting','paused','interrupted'))
 OR (json_type(a.record,'$.disconnectRecovery')='object' AND (
   COALESCE(json_extract(a.record,'$.status'),'') NOT IN ('completed','waiting','paused','parked')
   OR json_extract(a.record,'$.disconnectRecovery.stage') IS NOT NULL))
 OR (json_type(a.record,'$.restartRecovery')='object' AND (
   COALESCE(json_extract(a.record,'$.status'),'') NOT IN ('completed','waiting','paused','parked')
   OR COALESCE(json_extract(a.record,'$.restartRecovery.stage'),'')
      NOT IN ('finished','continued','reattached','input_restored')))) recovery
 FROM runtime_agents a WHERE json_extract(a.record,'$.deletedAt') IS NULL
 AND COALESCE(json_extract(a.record,'$.remoteWorker'),0)=0
 AND COALESCE(json_extract(a.record,'$.remoteAnchor'),0)=0)
";

pub(crate) fn target_page(
    store: &RuntimeStore,
    after_account: &str,
    after_agent: &BTreeMap<String, String>,
) -> Result<BTreeMap<String, Vec<(String, i64)>>, WorkerError> {
    let db = store.read()?;
    let invalid: Option<String> = db
        .query_row(
            &format!(
                "{TARGETS} SELECT id FROM targets WHERE (pending OR recovery)
        AND (account='' OR account_type NOT IN ('null','text')) LIMIT 1"
            ),
            [],
            |r| r.get(0),
        )
        .optional()?;
    if let Some(id) = invalid {
        return Err(StoreError::InvalidStoredObject {
            table: "runtime_agents",
            id,
        }
        .into());
    }
    let mut accounts = db.prepare(&format!(
        "{TARGETS} SELECT DISTINCT account FROM targets
        WHERE (pending OR recovery) AND account>?1 ORDER BY account LIMIT ?2"
    ))?;
    let mut read_accounts = |after: &str| -> Result<Vec<String>, rusqlite::Error> {
        accounts
            .query_map(rusqlite::params![after, MAX_ACCOUNTS as i64], |r| r.get(0))?
            .collect()
    };
    let mut selected = read_accounts(after_account)?;
    if selected.is_empty() && !after_account.is_empty() {
        selected = read_accounts("")?;
    }
    let mut pending = db.prepare(&format!(
        "{TARGETS} SELECT id,epoch FROM targets
        WHERE pending AND account=?1 AND id>?2 ORDER BY id LIMIT ?3"
    ))?;
    let mut result = BTreeMap::new();
    for account in selected {
        let after = after_agent.get(&account).map(String::as_str).unwrap_or("");
        let mut read_agents = |after: &str| -> Result<Vec<(String, i64)>, rusqlite::Error> {
            pending
                .query_map(rusqlite::params![account, after, WAKE_PAGE as i64], |r| {
                    Ok((r.get(0)?, r.get(1)?))
                })?
                .collect()
        };
        let mut agents = read_agents(after)?;
        if agents.is_empty() && !after.is_empty() {
            agents = read_agents("")?;
        }
        result.insert(account, agents);
    }
    Ok(result)
}
