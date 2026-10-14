//! Server-owned scheduling for bounded provider history imports.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use studio_runtime_services::account_driver::AccountDriver;
use studio_runtime_services::analytics_history::{
    HistoryClock, HistoryCollector, HistoryError, StoreHistoryBudgetSink,
};
use studio_runtime_store::{AgentRecord, RuntimeStore, Table};
use tokio::sync::watch;
use tokio::task::JoinHandle;

const HISTORY_ERROR_BACKOFF_BASE: Duration = Duration::from_millis(250);
const HISTORY_ERROR_BACKOFF_MAX: Duration = Duration::from_secs(5);

pub(crate) struct HistoryRuntime {
    store: Arc<RuntimeStore>,
    accounts: Arc<AccountDriver>,
    collector: HistoryCollector,
    cancelled: AtomicBool,
    started: std::time::Instant,
}

impl HistoryRuntime {
    pub(crate) fn new(store: Arc<RuntimeStore>, accounts: Arc<AccountDriver>) -> Self {
        Self {
            store,
            accounts,
            collector: HistoryCollector::default(),
            cancelled: AtomicBool::new(false),
            started: std::time::Instant::now(),
        }
    }

    fn step(&mut self) -> Result<bool, HistoryError> {
        let db = self.store.read().map_err(HistoryError::Store)?;
        let actors = Table::<AgentRecord>::new(&db)
            .list()
            .map_err(HistoryError::Store)?
            .into_iter()
            .map(|agent| agent.value())
            .collect::<Vec<_>>();
        drop(db);

        let accounts = Arc::clone(&self.accounts);
        let profile = move |account_key: &str| {
            let snapshot = accounts.snapshot().map_err(|_| HistoryError::Read)?;
            snapshot
                .accounts
                .iter()
                .find(|account| account["id"].as_str() == Some(account_key))
                .and_then(|account| account["home"].as_str())
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .ok_or(HistoryError::Read)
        };
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |duration| duration.as_secs_f64());
        let clock = HistoryClock {
            observed_at: now,
            monotonic: self.started.elapsed().as_secs_f64(),
        };
        let budget = StoreHistoryBudgetSink::new(&self.store);
        self.collector.step(
            &self.store,
            &actors,
            profile,
            clock,
            &self.cancelled,
            &budget,
        )
    }

    pub(crate) fn spawn(
        self,
        mut shutdown: watch::Receiver<bool>,
    ) -> JoinHandle<Result<(), HistoryError>> {
        tokio::spawn(async move {
            let mut runtime = self;
            let mut failures = 0_u32;
            let mut first = true;
            loop {
                if !first {
                    let delay = if failures > 0 {
                        history_error_backoff(failures)
                    } else {
                        let now = runtime.started.elapsed().as_secs_f64();
                        Duration::from_secs_f64(runtime.collector.schedule.delay(now, 0.0, 0.0))
                    };
                    tokio::select! {
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() { break; }
                        }
                        _ = tokio::time::sleep(delay) => {}
                    }
                }
                first = false;
                if *shutdown.borrow() {
                    break;
                }
                let (next, result) = studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-server/history_runtime",
                    move || {
                        let result = runtime.step();
                        (runtime, result)
                    },
                )
                .await
                .map_err(|_| HistoryError::Read)?;
                runtime = next;
                match result {
                    Ok(_) => failures = 0,
                    Err(error) => {
                        failures = failures.saturating_add(1);
                        eprintln!("provider history import failed: {error}");
                    }
                }
            }
            runtime
                .cancelled
                .store(true, std::sync::atomic::Ordering::Release);
            Ok(())
        })
    }
}

fn history_error_backoff(failures: u32) -> Duration {
    let exponent = failures.saturating_sub(1).min(5);
    HISTORY_ERROR_BACKOFF_BASE
        .saturating_mul(1_u32 << exponent)
        .min(HISTORY_ERROR_BACKOFF_MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn shutdown_stops_the_history_task() {
        let root = tempdir().expect("temporary store");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("store"));
        let account_path = root.path().join("accounts.json");
        std::fs::write(
            &account_path,
            serde_json::json!({
                "version": 1,
                "defaultAccountKey": "default",
                "accounts": {"default": {
                    "id": "default", "home": root.path(), "label": "Fixture",
                    "source": "Codex CLI", "status": "signedOut"
                }},
                "logins": {}, "deleteReceipts": {}
            })
            .to_string(),
        )
        .expect("fixture account registry");
        let accounts = Arc::new(AccountDriver::open(&account_path).expect("account driver"));
        let runtime = HistoryRuntime::new(store, accounts);
        let (shutdown, receiver) = watch::channel(false);
        let task = runtime.spawn(receiver);
        tokio::task::yield_now().await;
        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("history task shutdown deadline")
            .expect("history task join")
            .expect("history task clean shutdown");
    }
}
