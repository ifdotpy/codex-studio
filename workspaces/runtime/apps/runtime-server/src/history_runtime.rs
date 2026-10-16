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
use tokio::sync::{mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const HISTORY_ERROR_BACKOFF_BASE: Duration = Duration::from_millis(250);
const HISTORY_ERROR_BACKOFF_MAX: Duration = Duration::from_secs(5);

struct ManualHistoryTick {
    clock: HistoryClock,
    completed: oneshot::Sender<Result<bool, String>>,
}

#[cfg(test)]
struct HistoryStepTrigger(mpsc::UnboundedSender<ManualHistoryTick>);

#[cfg(test)]
impl HistoryStepTrigger {
    async fn step(&self, clock: HistoryClock) -> Result<bool, String> {
        let (completed, result) = oneshot::channel();
        self.0
            .send(ManualHistoryTick { clock, completed })
            .map_err(|_| "history task stopped before the requested step".to_owned())?;
        result
            .await
            .map_err(|_| "history task stopped before completing the requested step")?
    }
}

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

    fn step(&mut self, clock: HistoryClock) -> Result<bool, HistoryError> {
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
        shutdown: watch::Receiver<bool>,
    ) -> JoinHandle<Result<(), HistoryError>> {
        self.spawn_inner(shutdown, None, true)
    }

    #[cfg(test)]
    fn spawn_for_test(
        self,
        shutdown: watch::Receiver<bool>,
    ) -> (JoinHandle<Result<(), HistoryError>>, HistoryStepTrigger) {
        let (sender, receiver) = mpsc::unbounded_channel();
        (
            self.spawn_inner(shutdown, Some(receiver), false),
            HistoryStepTrigger(sender),
        )
    }

    fn spawn_inner(
        self,
        mut shutdown: watch::Receiver<bool>,
        mut manual_ticks: Option<mpsc::UnboundedReceiver<ManualHistoryTick>>,
        immediate_start: bool,
    ) -> JoinHandle<Result<(), HistoryError>> {
        tokio::spawn(async move {
            let mut runtime = self;
            let mut failures = 0_u32;
            let mut first = immediate_start;
            loop {
                let manual_tick = if let Some(ticks) = manual_ticks.as_mut() {
                    tokio::select! {
                        changed = shutdown.changed() => {
                            if changed.is_err() || *shutdown.borrow() { break; }
                            continue;
                        }
                        tick = ticks.recv() => {
                            let Some(tick) = tick else { break; };
                            Some(tick)
                        }
                    }
                } else {
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
                    None
                };
                if *shutdown.borrow() {
                    break;
                }
                let clock = manual_tick
                    .as_ref()
                    .map_or_else(|| history_clock(&runtime), |tick| tick.clock);
                let completion = manual_tick.map(|tick| tick.completed);
                let (next, result) = studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-server/history_runtime",
                    move || {
                        let result = runtime.step(clock);
                        (runtime, result)
                    },
                )
                .await
                .map_err(|_| HistoryError::Read)?;
                runtime = next;
                if let Some(completed) = completion {
                    let response = result.as_ref().copied().map_err(ToString::to_string);
                    if completed.send(response).is_err() {
                        eprintln!("provider history tick requester ended before completion");
                    }
                }
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

fn history_clock(runtime: &HistoryRuntime) -> HistoryClock {
    HistoryClock {
        observed_at: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_or(0.0, |duration| duration.as_secs_f64()),
        monotonic: runtime.started.elapsed().as_secs_f64(),
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
    use serde_json::{Value, json};
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

    #[tokio::test]
    async fn composed_history_task_imports_and_replays_fixture_usage_once() {
        let root = tempdir().expect("temporary fixture root");
        let state = root.path().join("state");
        let home = root.path().join("provider-home");
        let sessions = home.join("sessions");
        std::fs::create_dir_all(&sessions).expect("fixture sessions");
        std::fs::create_dir_all(&state).expect("fixture state");

        let account_path = root.path().join("accounts.json");
        std::fs::write(
            &account_path,
            json!({
                "version": 1,
                "defaultAccountKey": "default",
                "accounts": {"default": {
                    "id": "default", "home": home, "label": "Fixture",
                    "source": "Codex CLI", "status": "signedOut"
                }},
                "logins": {}, "deleteReceipts": {}
            })
            .to_string(),
        )
        .expect("fixture account registry");
        let accounts = Arc::new(AccountDriver::open(&account_path).expect("account driver"));
        let store = Arc::new(RuntimeStore::open(Some(&state)).expect("store"));
        let agent = json!({
            "id": "history-agent", "rootId": "history-agent", "parentId": null,
            "name": "History fixture", "epoch": 0, "status": "running",
            "accountKey": "default", "accountHistory": [], "provider": "codex",
            "threadId": "history-thread", "turnId": "history-turn", "model": "fixture-model",
            "effort": "high", "fastMode": false, "daybreakEnabled": false,
            "cyberAccessProgram": false, "cwd": root.path(), "deletedAt": null
        });
        let stored_agent = AgentRecord::from_value(agent.clone()).expect("typed fixture agent");
        store
            .transaction(|tx| Table::<AgentRecord>::new(tx).put(&stored_agent))
            .expect("persist fixture agent");

        let rollout = sessions.join("rollout-fixture-history-thread.jsonl");
        let records: [Value; 4] = [
            json!({"type":"session_meta","timestamp":1,"payload":{"id":"history-thread"}}),
            json!({"type":"event_msg","timestamp":2,"payload":{"type":"task_started","turn_id":"history-turn"}}),
            json!({"type":"event_msg","timestamp":3,"payload":{"type":"task_complete","turn_id":"history-turn","status":"completed"}}),
            json!({"type":"token_usage_record","timestamp":4,"payload":{
                "thread_id":"history-thread","response_id":"history-response",
                "usage":{"total_tokens":23},"thread_token_usage":{"total_tokens":23}
            }}),
        ];
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend(serde_json::to_vec(&record).expect("rollout record"));
            bytes.push(b'\n');
        }
        std::fs::write(&rollout, bytes).expect("fixture rollout");

        let runtime = HistoryRuntime::new(Arc::clone(&store), accounts);
        let (shutdown, receiver) = watch::channel(false);
        let (task, trigger) = runtime.spawn_for_test(receiver);
        assert!(
            trigger
                .step(HistoryClock {
                    observed_at: 200.0,
                    monotonic: 10.0,
                })
                .await
                .expect("first composed history step")
        );

        let budget_counts = || {
            store
                .read_transaction(|tx| {
                    Ok((
                        tx.query_row("SELECT count(*) FROM runtime_budget_usage", [], |row| {
                            row.get::<_, i64>(0)
                        })?,
                        tx.query_row(
                            "SELECT COALESCE(sum(tokens), 0) FROM runtime_budget_usage",
                            [],
                            |row| row.get::<_, i64>(0),
                        )?,
                    ))
                })
                .expect("read budget usage")
        };
        let analytics = rusqlite::Connection::open(state.join("analytics.sqlite3"))
            .expect("analytics database");
        let analytics_count = || {
            analytics
                .query_row(
                    "SELECT count(*) FROM analytics_usage WHERE json_extract(record,'$.responseId')='history-response'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read analytics usage")
        };
        assert_eq!(budget_counts(), (1, 23));
        assert_eq!(analytics_count(), 1);

        trigger
            .step(HistoryClock {
                observed_at: 205.0,
                monotonic: 15.0,
            })
            .await
            .expect("replay composed history step");
        assert_eq!(budget_counts(), (1, 23));
        assert_eq!(analytics_count(), 1);

        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("history task shutdown deadline")
            .expect("history task join")
            .expect("history task clean shutdown");
    }
}
