//! Server-owned scheduling for bounded provider history imports.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher, event::ModifyKind};
use serde_json::Value;
use studio_runtime_services::account_driver::AccountDriver;
use studio_runtime_services::analytics_history::{
    HistoryClock, HistoryCollector, HistoryError, StoreHistoryBudgetSink,
};
use studio_runtime_store::{AgentRecord, ObservedTable, RuntimeStore, Table};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const HISTORY_ERROR_BACKOFF_BASE: Duration = Duration::from_millis(250);
const HISTORY_ERROR_BACKOFF_MAX: Duration = Duration::from_secs(5);
const MAX_PENDING_FILE_EVENTS: usize = 8192;

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
    cancelled: Arc<AtomicBool>,
    started: std::time::Instant,
}

#[derive(Default)]
struct PendingHistoryEvents {
    roster_dirty: bool,
    accounts_dirty: bool,
    overflow: bool,
    files: BTreeMap<PathBuf, PendingFileEvent>,
    error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct PendingFileEvent {
    removed: bool,
    structural: bool,
}

#[derive(Default)]
struct HistoryWake {
    pending: std::sync::Mutex<PendingHistoryEvents>,
    notify: Notify,
    #[cfg(test)]
    watcher_recoveries: std::sync::atomic::AtomicU64,
}

impl HistoryWake {
    fn commit(&self) {
        self.with_pending(|pending| pending.roster_dirty = true);
    }

    fn filesystem(&self, result: notify::Result<Event>, registry_parent: &Path) {
        match result {
            Ok(event) => {
                let kind = &event.kind;
                if matches!(kind, EventKind::Access(_)) {
                    return;
                }
                let removed_event = matches!(kind, EventKind::Remove(_));
                let structural = matches!(
                    kind,
                    EventKind::Create(_)
                        | EventKind::Remove(_)
                        | EventKind::Modify(ModifyKind::Name(_))
                );
                let needs_rescan =
                    event.need_rescan() || matches!(kind, EventKind::Any | EventKind::Other);
                self.with_pending(|pending| {
                    if needs_rescan || event.paths.is_empty() {
                        pending.overflow = true;
                        pending.accounts_dirty = true;
                        pending.files.clear();
                    }
                    for path in event.paths {
                        let removed = removed_event || !path.exists();
                        if registry_event_path(&path, registry_parent) {
                            pending.accounts_dirty = true;
                        }
                        if !pending.overflow {
                            record_file_change(
                                pending,
                                path,
                                PendingFileEvent {
                                    removed,
                                    structural,
                                },
                            );
                        }
                    }
                });
            }
            Err(error) => {
                self.with_pending(|pending| pending.error = Some(error.to_string()));
            }
        }
    }

    fn with_pending(&self, update: impl FnOnce(&mut PendingHistoryEvents)) {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        update(&mut pending);
        drop(pending);
        self.notify.notify_one();
    }

    fn take(&self) -> PendingHistoryEvents {
        let mut pending = self
            .pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        std::mem::take(&mut *pending)
    }
}

fn record_file_change(pending: &mut PendingHistoryEvents, path: PathBuf, event: PendingFileEvent) {
    if pending.files.len() >= MAX_PENDING_FILE_EVENTS && !pending.files.contains_key(&path) {
        pending.overflow = true;
        pending.accounts_dirty = true;
        pending.files.clear();
    } else {
        pending.files.insert(path, event);
    }
}

fn registry_event_path(path: &Path, registry_parent: &Path) -> bool {
    path == registry_parent.join("registry.json")
        || path == registry_parent
        || registry_parent.starts_with(path)
}

struct HistoryWatcher {
    watcher: RecommendedWatcher,
    registry_parent: PathBuf,
    accounts: BTreeMap<String, PathBuf>,
    watched: BTreeSet<PathBuf>,
    registry_watches: BTreeSet<PathBuf>,
}

impl HistoryWatcher {
    fn new(root: &Path, wake: Arc<HistoryWake>) -> Result<Self, HistoryError> {
        let registry_path = root.join("accounts/registry.json");
        let registry_parent = registry_path.parent().ok_or(HistoryError::Read)?.to_owned();
        let callback_wake = Arc::clone(&wake);
        let callback_parent = registry_parent.clone();
        let mut watcher = notify::recommended_watcher(move |event| {
            callback_wake.filesystem(event, &callback_parent);
        })?;
        let mut watched = BTreeSet::new();
        let directory = nearest_existing_directory(&registry_parent)?;
        watcher.watch(&directory, RecursiveMode::NonRecursive)?;
        watched.insert(directory);
        let registry_watches = watched.clone();
        Ok(Self {
            watcher,
            registry_parent,
            accounts: BTreeMap::new(),
            watched,
            registry_watches,
        })
    }

    fn watch_profiles(
        &mut self,
        accounts: BTreeMap<String, PathBuf>,
    ) -> Result<Vec<PathBuf>, HistoryError> {
        let accounts = accounts
            .into_iter()
            .map(|(id, path)| (id, absolute_path(&path)))
            .collect::<BTreeMap<_, _>>();
        if accounts == self.accounts {
            return Ok(Vec::new());
        }
        let changed = self
            .accounts
            .values()
            .chain(accounts.values())
            .cloned()
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        self.accounts = accounts;
        self.reconfigure()?;
        Ok(changed)
    }

    fn reconfigure(&mut self) -> Result<(), HistoryError> {
        for path in self.watched.difference(&self.registry_watches) {
            if let Err(error) = self.watcher.unwatch(path)
                && !matches!(
                    error.kind,
                    notify::ErrorKind::WatchNotFound | notify::ErrorKind::PathNotFound
                )
            {
                return Err(error.into());
            }
        }
        self.watched.clone_from(&self.registry_watches);
        let registry_directory = nearest_existing_directory(&self.registry_parent)?;
        if self.registry_watches.insert(registry_directory.clone()) {
            self.watcher
                .watch(&registry_directory, RecursiveMode::NonRecursive)?;
            self.watched.insert(registry_directory);
        }
        let mut directories = BTreeMap::<PathBuf, RecursiveMode>::new();
        for directory in &self.registry_watches {
            directories.insert(directory.clone(), RecursiveMode::NonRecursive);
        }
        for home in self.accounts.values() {
            let existing_home = nearest_existing_directory(home)?;
            directories.insert(existing_home, RecursiveMode::NonRecursive);
            for name in ["sessions", "archived_sessions"] {
                let session_root = home.join(name);
                if session_root.is_dir() {
                    directories.insert(session_root, RecursiveMode::Recursive);
                } else {
                    directories.insert(
                        nearest_existing_directory(&session_root)?,
                        RecursiveMode::NonRecursive,
                    );
                }
            }
        }
        for (path, mode) in directories {
            if !self.watched.contains(&path) {
                self.watcher.watch(&path, mode)?;
            }
            self.watched.insert(path);
        }
        Ok(())
    }
}

fn absolute_path(path: &Path) -> PathBuf {
    std::fs::canonicalize(path).unwrap_or_else(|_| {
        if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(path))
                .unwrap_or_else(|_| path.to_owned())
        }
    })
}

fn nearest_existing_directory(path: &Path) -> Result<PathBuf, HistoryError> {
    let mut candidate = absolute_path(path);
    loop {
        if candidate.is_dir() {
            return Ok(candidate);
        }
        if !candidate.pop() {
            return Err(HistoryError::Read);
        }
    }
}

impl HistoryRuntime {
    pub(crate) fn new(store: Arc<RuntimeStore>, accounts: Arc<AccountDriver>) -> Self {
        Self {
            store,
            accounts,
            collector: HistoryCollector::default(),
            cancelled: Arc::new(AtomicBool::new(false)),
            started: std::time::Instant::now(),
        }
    }

    fn read_roster(store: &RuntimeStore) -> Result<Vec<Value>, HistoryError> {
        let db = store.read().map_err(HistoryError::Store)?;
        let actors = Table::<AgentRecord>::new(&db)
            .list()
            .map_err(HistoryError::Store)?
            .into_iter()
            .map(|agent| agent.value())
            .collect::<Vec<_>>();
        drop(db);
        Ok(actors)
    }

    async fn step(&mut self, actors: &[Value], clock: HistoryClock) -> Result<bool, HistoryError> {
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
        self.collector
            .step(
                &self.store,
                actors,
                profile,
                clock,
                Arc::clone(&self.cancelled),
                &budget,
            )
            .await
    }

    async fn refresh_accounts(
        &self,
        watcher: &mut HistoryWatcher,
    ) -> Result<Vec<PathBuf>, HistoryError> {
        let accounts = Arc::clone(&self.accounts);
        let homes = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/history-account-profiles",
            move || {
                let snapshot = accounts.snapshot()?;
                Ok::<_, HistoryError>(
                    snapshot
                        .accounts
                        .iter()
                        .filter_map(|account| {
                            let id = account.get("id")?.as_str()?;
                            let home = account.get("home")?.as_str()?;
                            (!home.is_empty()).then(|| (id.to_owned(), PathBuf::from(home)))
                        })
                        .collect::<BTreeMap<_, _>>(),
                )
            },
        )
        .await
        .map_err(|_| HistoryError::Read)??;
        watcher.watch_profiles(homes)
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
        shutdown: watch::Receiver<bool>,
        mut manual_ticks: Option<mpsc::UnboundedReceiver<ManualHistoryTick>>,
        _immediate_start: bool,
    ) -> JoinHandle<Result<(), HistoryError>> {
        if manual_ticks.is_none() {
            return tokio::spawn(async move { run_event_driven(self, shutdown).await });
        }
        tokio::spawn(async move {
            let mut runtime = self;
            let mut shutdown = shutdown;
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
                    unreachable!("automatic history uses the event-driven runner")
                };
                if *shutdown.borrow() {
                    break;
                }
                let clock = manual_tick
                    .as_ref()
                    .map_or_else(|| history_clock(&runtime), |tick| tick.clock);
                let completion = manual_tick.map(|tick| tick.completed);
                let store = Arc::clone(&runtime.store);
                let actors = studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-server/history_roster",
                    move || HistoryRuntime::read_roster(&store),
                )
                .await
                .map_err(|_| HistoryError::Read)?;
                let result = match actors {
                    Ok(actors) => runtime.step(&actors, clock).await,
                    Err(error) => Err(error),
                };
                if let Some(completed) = completion {
                    let response = result.as_ref().copied().map_err(ToString::to_string);
                    if completed.send(response).is_err() {
                        eprintln!("provider history tick requester ended before completion");
                    }
                }
                match result {
                    Ok(_) => {}
                    Err(error) => {
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

async fn run_event_driven(
    runtime: HistoryRuntime,
    shutdown: watch::Receiver<bool>,
) -> Result<(), HistoryError> {
    run_event_driven_with_wake(runtime, shutdown, Arc::new(HistoryWake::default())).await
}

async fn run_event_driven_with_wake(
    mut runtime: HistoryRuntime,
    mut shutdown: watch::Receiver<bool>,
    wake: Arc<HistoryWake>,
) -> Result<(), HistoryError> {
    let observer_wake = Arc::clone(&wake);
    runtime
        .store
        .add_table_change_observer(ObservedTable::Agents, move || observer_wake.commit());

    // Install the registry watch before reading either external accounts or the
    // durable roster. Notifications arriving during these snapshots stay queued.
    let mut watcher = HistoryWatcher::new(runtime.store.root(), Arc::clone(&wake))?;
    let initial_profiles = runtime.refresh_accounts(&mut watcher).await?;
    for profile in initial_profiles {
        runtime.collector.profile_changed(&profile);
    }

    let (next_runtime, roster) = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/history_roster_startup",
        move || {
            let roster = HistoryRuntime::read_roster(&runtime.store);
            (runtime, roster)
        },
    )
    .await
    .map_err(|_| HistoryError::Read)?;
    runtime = next_runtime;
    let mut roster = Arc::new(roster?);
    runtime.collector.schedule.refresh(&roster)?;

    let mut failures = 0_u32;
    let mut watcher_failures = 0_u32;
    let mut retry_at: Option<tokio::time::Instant> = None;
    loop {
        if *shutdown.borrow() {
            break;
        }

        let pending = wake.take();
        if let Some(error) = pending.error {
            eprintln!("provider history filesystem watcher failed: {error}");
            drop(watcher);
            let restored = restore_history_watcher(
                &mut runtime,
                &wake,
                &mut shutdown,
                &roster,
                &mut watcher_failures,
            )
            .await?;
            let Some(restored) = restored else {
                break;
            };
            watcher = restored;
            // The batch was consumed above. Re-read the durable roster after
            // recovery so a coalesced commit is not lost with the old watcher.
            wake.commit();
            continue;
        }
        let roster_changed = pending.roster_dirty;
        let mut reconfigure_watches = pending.overflow;

        if pending.accounts_dirty {
            let changed_profiles = match runtime.refresh_accounts(&mut watcher).await {
                Ok(changed) => changed,
                Err(error) => {
                    eprintln!("provider history account-watch update failed: {error}");
                    drop(watcher);
                    let restored = restore_history_watcher(
                        &mut runtime,
                        &wake,
                        &mut shutdown,
                        &roster,
                        &mut watcher_failures,
                    )
                    .await?;
                    let Some(restored) = restored else {
                        break;
                    };
                    watcher = restored;
                    wake.commit();
                    continue;
                }
            };
            if !changed_profiles.is_empty() {
                for profile in changed_profiles {
                    runtime.collector.profile_changed(&profile);
                }
                notify_all_agents(&mut runtime, &roster)?;
            }
            reconfigure_watches = true;
        }

        if pending.overflow {
            for home in watcher.accounts.values() {
                runtime.collector.profile_changed(home);
            }
            notify_all_agents(&mut runtime, &roster)?;
        }

        for (path, event) in pending.files {
            let affected = watcher
                .accounts
                .iter()
                .filter(|(_, home)| path.starts_with(home.as_path()) || home.starts_with(&path))
                .map(|(account, home)| (account.clone(), home.clone()))
                .collect::<Vec<_>>();
            for (account, home) in affected {
                if is_rollout_profile_path(&home, &path) {
                    runtime.collector.file_changed(&home, &path, event.removed);
                    for actor in roster
                        .iter()
                        .filter(|actor| actor_matches_rollout(actor, &account, &path))
                    {
                        runtime
                            .collector
                            .schedule
                            .notify(actor["id"].as_str().ok_or(HistoryError::Read)?)?;
                    }
                } else if profile_tree_changed(&home, &path, event) {
                    runtime.collector.profile_changed(&home);
                    notify_account_agents(&mut runtime, &roster, &account)?;
                    reconfigure_watches = true;
                }
            }
        }

        if reconfigure_watches && let Err(error) = watcher.reconfigure() {
            eprintln!("provider history watch update failed: {error}");
            drop(watcher);
            let restored = restore_history_watcher(
                &mut runtime,
                &wake,
                &mut shutdown,
                &roster,
                &mut watcher_failures,
            )
            .await?;
            let Some(restored) = restored else {
                break;
            };
            watcher = restored;
            wake.commit();
            continue;
        }

        if roster_changed {
            let (next_runtime, next_roster) =
                studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-server/history_roster_refresh",
                    move || {
                        let roster = HistoryRuntime::read_roster(&runtime.store);
                        (runtime, roster)
                    },
                )
                .await
                .map_err(|_| HistoryError::Read)?;
            runtime = next_runtime;
            let next_roster = next_roster?;
            runtime.collector.schedule.refresh(&next_roster)?;
            roster = Arc::new(next_roster);
        }

        if retry_at.is_some_and(|deadline| tokio::time::Instant::now() < deadline) {
            let deadline = retry_at.expect("checked retry deadline");
            tokio::select! {
                changed = shutdown.changed() => {
                    if changed.is_err() || *shutdown.borrow() { break; }
                }
                _ = wake.notify.notified() => {}
                _ = tokio::time::sleep_until(deadline) => { retry_at = None; }
            }
            continue;
        }

        if runtime.collector.schedule.has_pending() {
            let clock = history_clock(&runtime);
            let actors = Arc::clone(&roster);
            let result = runtime.step(&actors, clock).await;
            match result {
                Ok(_) => {
                    failures = 0;
                    retry_at = None;
                }
                Err(error) => {
                    failures = failures.saturating_add(1);
                    retry_at = Some(tokio::time::Instant::now() + history_error_backoff(failures));
                    eprintln!("provider history import failed: {error}");
                }
            }
            continue;
        }

        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
            _ = wake.notify.notified() => {}
        }
    }

    runtime.cancelled.store(true, Ordering::Release);
    Ok(())
}

async fn restore_history_watcher(
    runtime: &mut HistoryRuntime,
    wake: &Arc<HistoryWake>,
    shutdown: &mut watch::Receiver<bool>,
    roster: &[Value],
    failures: &mut u32,
) -> Result<Option<HistoryWatcher>, HistoryError> {
    loop {
        if *shutdown.borrow() {
            return Ok(None);
        }
        let attempt = async {
            let mut watcher = HistoryWatcher::new(runtime.store.root(), Arc::clone(wake))?;
            runtime.refresh_accounts(&mut watcher).await?;
            for home in watcher.accounts.values() {
                runtime.collector.profile_changed(home);
            }
            notify_all_agents(runtime, roster)?;
            Ok::<_, HistoryError>(watcher)
        }
        .await;
        match attempt {
            Ok(watcher) => {
                *failures = 0;
                #[cfg(test)]
                wake.watcher_recoveries.fetch_add(1, Ordering::AcqRel);
                return Ok(Some(watcher));
            }
            Err(error) => {
                *failures = failures.saturating_add(1);
                eprintln!("provider history filesystem watcher retry failed: {error}");
                let deadline = tokio::time::Instant::now() + history_error_backoff(*failures);
                tokio::select! {
                    changed = shutdown.changed() => {
                        if changed.is_err() || *shutdown.borrow() { return Ok(None); }
                    }
                    _ = tokio::time::sleep_until(deadline) => {}
                }
            }
        }
    }
}

fn is_rollout_profile_path(home: &Path, path: &Path) -> bool {
    let in_rollout_tree = ["sessions", "archived_sessions"]
        .iter()
        .any(|directory| path.starts_with(home.join(directory)));
    let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
        return false;
    };
    in_rollout_tree && name.starts_with("rollout-") && name.ends_with(".jsonl") && !path.is_dir()
}

fn profile_tree_changed(home: &Path, path: &Path, event: PendingFileEvent) -> bool {
    if !event.structural {
        return false;
    }
    if path == home || home.starts_with(path) {
        return true;
    }
    ["sessions", "archived_sessions"].iter().any(|directory| {
        let root = home.join(directory);
        path == root || (path.starts_with(&root) && (path.is_dir() || event.removed))
    })
}

fn rollout_matches_thread(path: &Path, thread: &str) -> bool {
    let suffix = format!("-{thread}.jsonl");
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| name.starts_with("rollout-") && name.ends_with(&suffix))
}

fn actor_matches_rollout(actor: &Value, account: &str, path: &Path) -> bool {
    actor["accountKey"].as_str().unwrap_or("default") == account
        && actor["threadId"]
            .as_str()
            .is_some_and(|thread| rollout_matches_thread(path, thread))
}

fn notify_all_agents(runtime: &mut HistoryRuntime, roster: &[Value]) -> Result<(), HistoryError> {
    for actor in roster {
        if actor["threadId"]
            .as_str()
            .is_some_and(|thread| !thread.is_empty())
        {
            runtime
                .collector
                .schedule
                .notify(actor["id"].as_str().ok_or(HistoryError::Read)?)?;
        }
    }
    Ok(())
}

fn notify_account_agents(
    runtime: &mut HistoryRuntime,
    roster: &[Value],
    account: &str,
) -> Result<(), HistoryError> {
    for actor in roster.iter().filter(|actor| {
        actor["accountKey"].as_str().unwrap_or("default") == account
            && actor["threadId"]
                .as_str()
                .is_some_and(|thread| !thread.is_empty())
    }) {
        runtime
            .collector
            .schedule
            .notify(actor["id"].as_str().ok_or(HistoryError::Read)?)?;
    }
    Ok(())
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
    async fn only_agent_commits_dirty_the_history_roster() {
        let root = tempdir().expect("temporary store");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let wake = Arc::new(HistoryWake::default());
        let observer_wake = Arc::clone(&wake);
        store.add_table_change_observer(ObservedTable::Agents, move || observer_wake.commit());

        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
                     VALUES('unrelated-history-event','history-agent','user','input','pending',1,0,NULL,NULL)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("commit unrelated event input");
        assert!(
            !wake.take().roster_dirty,
            "event journal changes must not scan the history agent roster"
        );

        let actor = AgentRecord::from_value(json!({
            "id":"history-agent", "rootId":"history-agent", "parentId":null,
            "name":"History fixture", "epoch":0, "status":"running",
            "accountHistory":[], "provider":"codex", "threadId":"history-thread",
            "turnId":"history-turn", "effort":"high", "fastMode":false,
            "daybreakEnabled":false, "cyberAccessProgram":false,
            "cwd":"/fixture", "deletedAt":null
        }))
        .expect("typed history actor");
        store
            .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&actor))
            .await
            .expect("commit agent roster input");
        assert!(
            wake.take().roster_dirty,
            "agent changes must refresh the history roster"
        );
    }

    #[test]
    fn watcher_events_coalesce_by_path_and_overflow_requests_rescan() {
        let path = PathBuf::from("/profile/sessions/rollout-thread.jsonl");
        let mut pending = PendingHistoryEvents::default();
        record_file_change(
            &mut pending,
            path.clone(),
            PendingFileEvent {
                removed: false,
                structural: false,
            },
        );
        record_file_change(
            &mut pending,
            path.clone(),
            PendingFileEvent {
                removed: true,
                structural: true,
            },
        );
        assert_eq!(pending.files.len(), 1);
        assert_eq!(
            pending.files.get(&path),
            Some(&PendingFileEvent {
                removed: true,
                structural: true,
            })
        );

        let mut full = PendingHistoryEvents::default();
        for index in 0..MAX_PENDING_FILE_EVENTS {
            record_file_change(
                &mut full,
                PathBuf::from(format!("/profile/{index}.jsonl")),
                PendingFileEvent {
                    removed: false,
                    structural: false,
                },
            );
        }
        record_file_change(
            &mut full,
            PathBuf::from("/profile/overflow.jsonl"),
            PendingFileEvent {
                removed: false,
                structural: false,
            },
        );
        assert!(full.overflow);
        assert!(full.accounts_dirty);
        assert!(full.files.is_empty());
    }

    #[test]
    fn watcher_ignores_reads_and_unrelated_account_directory_files() {
        let wake = HistoryWake::default();
        let parent = Path::new("/state/accounts");
        wake.filesystem(
            Ok(Event::new(EventKind::Access(
                notify::event::AccessKind::Read,
            ))),
            parent,
        );
        let pending = wake.take();
        assert!(!pending.overflow);
        assert!(!pending.accounts_dirty);
        assert!(pending.files.is_empty());

        assert!(!registry_event_path(
            Path::new("/state/accounts/logs/refresh.json"),
            parent
        ));
        assert!(registry_event_path(
            Path::new("/state/accounts/registry.json"),
            parent
        ));
    }

    #[test]
    fn rollout_notifications_match_timestamped_files_only_in_scanned_trees() {
        let home = Path::new("/profile");
        let path = Path::new("/profile/sessions/2026/rollout-20261016-thread-id.jsonl");
        assert!(is_rollout_profile_path(home, path));
        assert!(rollout_matches_thread(path, "thread-id"));
        assert!(!rollout_matches_thread(path, "other-thread"));
        assert!(!is_rollout_profile_path(
            home,
            Path::new("/profile/rollout-20261016-thread-id.jsonl")
        ));
        assert!(!is_rollout_profile_path(
            home,
            Path::new("/profile/sessions/2026/rollout-20261016-thread-id.jsonl/child")
        ));
        let legacy_actor = json!({"id":"legacy","threadId":"thread-id"});
        assert!(actor_matches_rollout(&legacy_actor, "default", path));
        assert!(!actor_matches_rollout(&legacy_actor, "other-account", path));
    }

    fn read_history_checkpoint(state: &Path) -> Option<Value> {
        read_history_checkpoint_for(state, "history-agent:default:history-thread")
    }

    fn read_history_checkpoint_for(state: &Path, id: &str) -> Option<Value> {
        let db = studio_runtime_store::AnalyticsRepository::new(state)
            .open_read_only()
            .ok()?;
        studio_runtime_store::analytics_capture::AnalyticsCaptureRepository::read(
            &db,
            studio_runtime_store::analytics_capture::CaptureTable::History,
            id,
        )
        .ok()?
    }

    async fn wait_for_offset(state: &Path, offset: u64) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(checkpoint) = read_history_checkpoint(state)
                    && checkpoint["offset"].as_u64() == Some(offset)
                {
                    return checkpoint;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("history watcher should import before its deadline")
    }

    #[tokio::test]
    async fn shutdown_stops_the_history_task() {
        let root = tempdir().expect("temporary store");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
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
    async fn native_rollout_append_wakes_idle_history_runtime() {
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
        let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("store"));
        let agent = json!({
            "id": "history-agent", "rootId": "history-agent", "parentId": null,
            "name": "History fixture", "epoch": 0, "status": "running",
            "accountHistory": [], "provider": "codex",
            "threadId": "history-thread", "turnId": "history-turn", "model": "fixture-model",
            "effort": "high", "fastMode": false, "daybreakEnabled": false,
            "cyberAccessProgram": false, "cwd": root.path(), "deletedAt": null
        });
        let stored_agent = AgentRecord::from_value(agent.clone()).expect("typed fixture agent");
        store
            .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&stored_agent))
            .await
            .expect("persist fixture agent");

        let rollout = sessions.join("rollout-20261016-history-thread.jsonl");
        let records = [
            json!({"type":"session_meta","timestamp":1,"payload":{"id":"history-thread"}}),
            json!({"type":"event_msg","timestamp":2,"payload":{"type":"task_started","turn_id":"history-turn"}}),
            json!({"type":"event_msg","timestamp":3,"payload":{"type":"task_complete","turn_id":"history-turn","status":"completed"}}),
            json!({"type":"token_usage_record","timestamp":4,"payload":{
                "thread_id":"history-thread","response_id":"history-response-1",
                "usage":{"total_tokens":23},"thread_token_usage":{"total_tokens":23}
            }}),
        ];
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend(serde_json::to_vec(&record).expect("rollout record"));
            bytes.push(b'\n');
        }
        std::fs::write(&rollout, &bytes).expect("fixture rollout");

        let runtime = HistoryRuntime::new(Arc::clone(&store), accounts);
        let (shutdown, receiver) = watch::channel(false);
        let wake = Arc::new(HistoryWake::default());
        let task = tokio::spawn(run_event_driven_with_wake(
            runtime,
            receiver,
            Arc::clone(&wake),
        ));
        let first = wait_for_offset(&state, bytes.len() as u64).await;
        assert_eq!(first["status"], "current");

        let second_agent = AgentRecord::from_value(json!({
            "id": "history-agent-after-recovery", "rootId": "history-agent-after-recovery",
            "parentId": null, "name": "History recovery fixture", "epoch": 0,
            "status": "running", "accountHistory": [], "provider": "codex",
            "threadId": "history-thread-after-recovery", "turnId": "history-turn-after-recovery",
            "model": "fixture-model", "effort": "high", "fastMode": false,
            "daybreakEnabled": false, "cyberAccessProgram": false, "cwd": root.path(),
            "deletedAt": null
        }))
        .expect("typed recovery fixture agent");
        let second_agent_id = second_agent.id().as_ref().to_owned();
        store
            .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&second_agent))
            .await
            .expect("persist recovery fixture agent");
        let second_thread = "history-thread-after-recovery";
        let second_rollout = sessions.join("rollout-20261016-history-thread-after-recovery.jsonl");
        let second_records = [
            json!({"type":"session_meta","timestamp":1,"payload":{"id":second_thread}}),
            json!({"type":"event_msg","timestamp":2,"payload":{"type":"task_started","turn_id":"history-turn-after-recovery"}}),
            json!({"type":"event_msg","timestamp":3,"payload":{"type":"task_complete","turn_id":"history-turn-after-recovery","status":"completed"}}),
            json!({"type":"token_usage_record","timestamp":4,"payload":{
                "thread_id":second_thread,"response_id":"history-response-after-recovery",
                "usage":{"total_tokens":23},"thread_token_usage":{"total_tokens":23}
            }}),
        ];
        let mut second_bytes = Vec::new();
        for record in second_records {
            second_bytes.extend(serde_json::to_vec(&record).expect("recovery rollout record"));
            second_bytes.push(b'\n');
        }
        std::fs::write(&second_rollout, &second_bytes).expect("fixture recovery rollout");
        let second_checkpoint_id = format!("{second_agent_id}:default:{second_thread}");

        // The store commit and watcher error arrive in one pending batch. A
        // successful recovery must re-read the roster before returning idle.
        wake.filesystem(
            Err(notify::Error::generic("fixture watcher failure")),
            &state.join("accounts"),
        );
        tokio::time::timeout(Duration::from_secs(5), async {
            while wake.watcher_recoveries.load(Ordering::Acquire) == 0 {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("filesystem watcher should recover before its deadline");

        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(checkpoint) = read_history_checkpoint_for(&state, &second_checkpoint_id)
                    && checkpoint["offset"].as_u64() == Some(second_bytes.len() as u64)
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("roster commit coalesced with watcher failure should import new actor");

        let appended = json!({"type":"token_usage_record","timestamp":5,"payload":{
            "thread_id":"history-thread","response_id":"history-response-2",
            "usage":{"total_tokens":31},"thread_token_usage":{"total_tokens":54}
        }});
        let mut append_bytes = serde_json::to_vec(&appended).expect("appended rollout record");
        append_bytes.push(b'\n');
        use std::io::Write;
        std::fs::OpenOptions::new()
            .append(true)
            .open(&rollout)
            .expect("open rollout for append")
            .write_all(&append_bytes)
            .expect("append rollout record");
        let second = wait_for_offset(&state, (bytes.len() + append_bytes.len()) as u64).await;
        assert_eq!(second["status"], "current");
        assert!(
            second["importedRecords"].as_u64().unwrap_or_default()
                > first["importedRecords"].as_u64().unwrap_or_default()
        );

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
        let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("store"));
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
            .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&stored_agent))
            .await
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
