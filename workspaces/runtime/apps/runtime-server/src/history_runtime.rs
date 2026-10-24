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
    ANALYTICS_DATABASE, BLOCKING_TASK, HistoryClock, HistoryCollector, HistoryError,
    HistoryRetryPolicy, StoreHistoryBudgetSink,
};
use studio_runtime_store::{AgentRecord, ObservedTable, RuntimeStore, Table, TableChange};
use tokio::sync::{Notify, mpsc, oneshot, watch};
use tokio::task::JoinHandle;

const HISTORY_ERROR_BACKOFF_BASE: Duration = Duration::from_millis(250);
const HISTORY_ERROR_BACKOFF_MAX: Duration = Duration::from_secs(5);
const MAX_PENDING_FILE_EVENTS: usize = 8192;
/// Agent rowids coalesced per wake batch before the roster falls back to a full read. Unit: rowids.
const MAX_PENDING_AGENT_ROW_HINTS: usize = 4096;
/// Read-failure target classes owned by the server history runner.
const ACCOUNT_REGISTRY: &str = "account registry";
const AGENT_RECORD: &str = "agent record";
const WATCH_DIRECTORY: &str = "watch directory";

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
    /// Deleted agents whose history is final, by agent id, with the `deletedAt` the final
    /// checkpoint saw. They are left out of the roster; a changed `deletedAt` brings them back.
    settled: BTreeMap<String, Value>,
}

#[derive(Default)]
struct PendingHistoryEvents {
    /// A full agent-roster reread is required; set by unknown or overflowed hints.
    roster_dirty: bool,
    /// Physical agent rows to reread, coalesced and sorted by rowid.
    agent_rows: BTreeSet<i64>,
    budget_dirty: bool,
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
    #[cfg(test)]
    watcher_recovered: Notify,
    #[cfg(test)]
    initialized: Notify,
    #[cfg(test)]
    step_attempts: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    step_completions: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    step_pending: std::sync::atomic::AtomicBool,
    #[cfg(test)]
    budget_failures: std::sync::atomic::AtomicU64,
    #[cfg(test)]
    progressed: Notify,
}

impl HistoryWake {
    fn commit(&self) {
        self.with_pending(|pending| pending.roster_dirty = true);
    }

    fn budget(&self) {
        self.with_pending(|pending| pending.budget_dirty = true);
    }

    fn agents(&self, change: TableChange<'_>) {
        self.with_pending(|pending| match change {
            TableChange::Rows(row_ids) => {
                if !pending.roster_dirty {
                    if pending.agent_rows.len() + row_ids.len() > MAX_PENDING_AGENT_ROW_HINTS {
                        pending.roster_dirty = true;
                        pending.agent_rows.clear();
                    } else {
                        pending.agent_rows.extend(row_ids.iter().copied());
                    }
                }
            }
            TableChange::Full => pending.roster_dirty = true,
        });
    }

    fn filesystem(&self, result: notify::Result<Event>, registry_parent: &Path) {
        self.filesystem_with_exists(result, registry_parent, Path::exists);
    }

    fn filesystem_with_exists(
        &self,
        result: notify::Result<Event>,
        registry_parent: &Path,
        mut exists: impl FnMut(&Path) -> bool,
    ) {
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
                let empty_paths = event.paths.is_empty();
                // Metadata may block on the filesystem. Store observers share
                // pending, so prepare all filesystem evidence before locking it.
                let paths = event
                    .paths
                    .into_iter()
                    .map(|path| {
                        let removed = removed_event || !exists(&path);
                        (
                            path,
                            PendingFileEvent {
                                removed,
                                structural,
                            },
                        )
                    })
                    .collect::<Vec<_>>();
                self.with_pending(|pending| {
                    if needs_rescan || empty_paths {
                        pending.overflow = true;
                        pending.accounts_dirty = true;
                        pending.files.clear();
                    }
                    for (path, event) in paths {
                        if registry_event_path(&path, registry_parent) {
                            pending.accounts_dirty = true;
                        }
                        if !pending.overflow {
                            record_file_change(pending, path, event);
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
        #[cfg(test)]
        self.progressed.notify_one();
        std::mem::take(&mut *pending)
    }
}

fn record_file_change(pending: &mut PendingHistoryEvents, path: PathBuf, event: PendingFileEvent) {
    if let Some(previous) = pending.files.get_mut(&path) {
        previous.removed = event.removed;
        previous.structural |= event.structural;
    } else if pending.files.len() >= MAX_PENDING_FILE_EVENTS {
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
        let registry_parent = registry_path
            .parent()
            .ok_or(HistoryError::read(ACCOUNT_REGISTRY))?
            .to_owned();
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
            .map(|(id, path)| {
                (
                    id,
                    studio_runtime_store::path_identity::PathEvidence::existing_or_absolute(&path)
                        .into_path_buf(),
                )
            })
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

fn nearest_existing_directory(path: &Path) -> Result<PathBuf, HistoryError> {
    let mut candidate =
        studio_runtime_store::path_identity::PathEvidence::existing_or_absolute(path)
            .into_path_buf();
    loop {
        if candidate.is_dir() {
            return Ok(candidate);
        }
        if !candidate.pop() {
            return Err(HistoryError::read(WATCH_DIRECTORY));
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
            settled: BTreeMap::new(),
        }
    }

    /// Full roster in rowid order; the row ids key later incremental rereads.
    async fn read_roster(
        store: &RuntimeStore,
        settled: &BTreeMap<String, Value>,
    ) -> Result<(Vec<i64>, Vec<Value>), HistoryError> {
        let rows = store
            .read_async("runtime-server/history_roster", |db| {
                Table::<AgentRecord>::new(db).list_with_row_ids()
            })
            .await
            .map_err(HistoryError::Store)?;
        Ok(rows
            .into_iter()
            .map(|(row_id, agent)| (row_id, agent.value()))
            .filter(|(_, agent)| !is_retired(agent, settled))
            .unzip())
    }

    async fn step(
        &mut self,
        row_ids: &[i64],
        actors: &[Value],
        clock: HistoryClock,
    ) -> Result<bool, HistoryError> {
        let accounts = Arc::clone(&self.accounts);
        let profile = move |account_key: &str| {
            let snapshot = accounts
                .snapshot()
                .map_err(|_| HistoryError::read(ACCOUNT_REGISTRY))?;
            // Python `accounts.home` refuses unknown accounts and accounts in
            // the changed or error state; the importer records that refusal.
            // The active snapshot omits signed-out logins and deleted accounts.
            snapshot
                .accounts
                .iter()
                .find(|account| account["id"].as_str() == Some(account_key))
                .filter(|account| !matches!(account["status"].as_str(), Some("changed" | "error")))
                .and_then(|account| account["home"].as_str())
                .filter(|home| !home.is_empty())
                .map(PathBuf::from)
                .ok_or(HistoryError::ProfileUnavailable)
        };
        let budget = StoreHistoryBudgetSink::new(&self.store);
        self.collector
            .step(
                &self.store,
                |row| {
                    row_ids
                        .binary_search(&row)
                        .ok()
                        .and_then(|index| actors.get(index))
                },
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
        .map_err(|_| HistoryError::read(BLOCKING_TASK))??;
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
                let actors = HistoryRuntime::read_roster(&runtime.store, &BTreeMap::new()).await;
                let result = match actors {
                    Ok((row_ids, actors)) => match runtime
                        .collector
                        .schedule
                        .refresh_ordered(&row_ids, &actors)
                    {
                        Ok(()) => runtime.step(&row_ids, &actors, clock).await,
                        Err(error) => Err(error.into()),
                    },
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
        .add_table_row_change_observer(ObservedTable::Agents, move |change| {
            observer_wake.agents(change)
        });
    let budget_observer_wake = Arc::clone(&wake);
    runtime
        .store
        .add_table_change_observer(ObservedTable::Budget, move || budget_observer_wake.budget());
    let usage_observer_wake = Arc::clone(&wake);
    runtime
        .store
        .add_table_change_observer(ObservedTable::BudgetUsage, move || {
            usage_observer_wake.budget()
        });

    // Install the registry watch before reading either external accounts or the
    // durable roster. Notifications arriving during these snapshots stay queued.
    let mut watcher = HistoryWatcher::new(runtime.store.root(), Arc::clone(&wake))?;
    let initial_profiles = runtime.refresh_accounts(&mut watcher).await?;
    for profile in initial_profiles {
        runtime.collector.profile_changed(&profile);
    }

    let root = runtime.store.root().to_path_buf();
    runtime.settled = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/history_settled_tombstones",
        move || read_settled_tombstones(&root),
    )
    .await
    .map_err(|_| HistoryError::read(BLOCKING_TASK))??;
    let (mut roster_row_ids, actors) =
        HistoryRuntime::read_roster(&runtime.store, &runtime.settled).await?;
    let mut roster = Arc::new(actors);
    runtime
        .collector
        .schedule
        .refresh_ordered(&roster_row_ids, &roster)?;
    #[cfg(test)]
    wake.initialized.notify_one();

    let mut failures = 0_u32;
    let mut watcher_failures = 0_u32;
    let mut retry_at: Option<tokio::time::Instant> = None;
    loop {
        if *shutdown.borrow() {
            break;
        }

        let pending = wake.take();
        if pending.budget_dirty {
            runtime.collector.schedule.budget_changed();
        }
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
        let roster_full = pending.roster_dirty;
        let roster_rows = pending.agent_rows;
        let roster_changed = roster_full || !roster_rows.is_empty();
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
                    runtime.collector.file_changed(
                        &home,
                        &path,
                        event.removed,
                        event.structural,
                        &account,
                    );
                    for actor in roster
                        .iter()
                        .filter(|actor| actor_matches_rollout(actor, &account, &path))
                    {
                        runtime.collector.schedule.notify_rollout(
                            actor["id"]
                                .as_str()
                                .ok_or(HistoryError::read(AGENT_RECORD))?,
                            event.structural,
                        )?;
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

        if roster_full {
            let (next_row_ids, next_actors) =
                HistoryRuntime::read_roster(&runtime.store, &runtime.settled).await?;
            runtime
                .collector
                .schedule
                .refresh_ordered(&next_row_ids, &next_actors)?;
            roster_row_ids = next_row_ids;
            roster = Arc::new(next_actors);
        } else if roster_changed {
            let row_ids = roster_rows.into_iter().collect::<Vec<_>>();
            let changes = read_roster_rows(&runtime.store, row_ids, &runtime.settled).await?;
            runtime.collector.schedule.update_rows(&changes)?;
            apply_roster_rows(Arc::make_mut(&mut roster), &mut roster_row_ids, changes);
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
            #[cfg(test)]
            wake.step_attempts
                .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
            let result = runtime.step(&roster_row_ids, &roster, clock).await;
            if result.is_ok() {
                retire_settled(
                    &mut runtime,
                    Arc::make_mut(&mut roster),
                    &mut roster_row_ids,
                );
            }
            match result {
                Ok(_) => {
                    failures = 0;
                    retry_at = None;
                }
                Err(error) => {
                    #[cfg(test)]
                    if error.retry_policy() == HistoryRetryPolicy::WaitForBudgetChange {
                        wake.budget_failures
                            .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                    }
                    if error.retry_policy() != HistoryRetryPolicy::Backoff {
                        failures = 0;
                        retry_at = None;
                    } else {
                        failures = failures.saturating_add(1);
                        retry_at =
                            Some(tokio::time::Instant::now() + history_error_backoff(failures));
                    }
                    eprintln!("provider history import failed: {error}");
                }
            }
            #[cfg(test)]
            {
                wake.step_pending.store(
                    runtime.collector.schedule.has_pending(),
                    std::sync::atomic::Ordering::Release,
                );
                wake.step_completions
                    .fetch_add(1, std::sync::atomic::Ordering::AcqRel);
                wake.progressed.notify_one();
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
                #[cfg(test)]
                wake.watcher_recovered.notify_one();
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

/// Reads only the physical rows named by change hints; `None` means the row is gone.
async fn read_roster_rows(
    store: &RuntimeStore,
    row_ids: Vec<i64>,
    settled: &BTreeMap<String, Value>,
) -> Result<Vec<(i64, Option<Value>)>, HistoryError> {
    let rows = store
        .read_async("runtime-server/history_roster_rows", move |db| {
            let table = Table::<AgentRecord>::new(db);
            row_ids
                .into_iter()
                .map(|row_id| Ok((row_id, table.get_by_row_id(row_id)?)))
                .collect::<studio_runtime_store::StoreResult<Vec<_>>>()
        })
        .await
        .map_err(HistoryError::Store)?;
    Ok(rows
        .into_iter()
        .map(|(row_id, agent)| {
            (
                row_id,
                agent
                    .map(|agent| agent.value())
                    .filter(|agent| !is_retired(agent, settled)),
            )
        })
        .collect())
}

/// A deleted agent leaves the roster once nothing is left to import for it: it never
/// reached a thread, or its final checkpoint already saw this `deletedAt`.
fn is_retired(agent: &Value, settled: &BTreeMap<String, Value>) -> bool {
    let deleted_at = &agent["deletedAt"];
    !deleted_at.is_null()
        && (agent["threadId"].as_str().is_none_or(str::is_empty)
            || agent["id"]
                .as_str()
                .and_then(|id| settled.get(id))
                .is_some_and(|final_deleted_at| final_deleted_at == deleted_at))
}

/// Final checkpoints of deleted agents, so a restart does not import every tombstone again.
fn read_settled_tombstones(root: &Path) -> Result<BTreeMap<String, Value>, HistoryError> {
    let Ok(db) = studio_runtime_store::AnalyticsRepository::new(root).open_read_only() else {
        return Ok(BTreeMap::new());
    };
    let mut statement = db
        .prepare(
            "SELECT agent,json_quote(json_extract(record,'$.deletedAt')) FROM analytics_history \
             WHERE json_extract(record,'$.deletedAt') IS NOT NULL \
             AND coalesce(json_extract(record,'$.status'),'') <> 'catchingUp'",
        )
        .map_err(|_| HistoryError::read(ANALYTICS_DATABASE))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|_| HistoryError::read(ANALYTICS_DATABASE))?;
    let mut settled = BTreeMap::new();
    for row in rows {
        let (agent, deleted_at) = row.map_err(|_| HistoryError::read(ANALYTICS_DATABASE))?;
        let deleted_at = serde_json::from_str(&deleted_at)
            .map_err(|_| HistoryError::read(ANALYTICS_DATABASE))?;
        settled.insert(agent, deleted_at);
    }
    Ok(settled)
}

/// Moves deleted agents whose import is final out of the roster, keeping row ids aligned.
fn retire_settled(runtime: &mut HistoryRuntime, actors: &mut Vec<Value>, row_ids: &mut Vec<i64>) {
    let mut index = 0;
    while index < actors.len() {
        let actor = &actors[index];
        if !actor["deletedAt"].is_null() && runtime.collector.schedule.settled(actor) {
            let id = actor["id"].as_str().unwrap_or_default().to_owned();
            runtime.collector.schedule.remove(&id);
            runtime.settled.insert(id, actor["deletedAt"].clone());
            actors.remove(index);
            row_ids.remove(index);
        } else {
            index += 1;
        }
    }
}

/// Applies row changes to the rowid-ordered roster in place. A replaced agent
/// arrives under a new rowid with no delete hint for the old row, so the stale
/// entry with the same agent id is removed first.
fn apply_roster_rows(
    actors: &mut Vec<Value>,
    row_ids: &mut Vec<i64>,
    changes: Vec<(i64, Option<Value>)>,
) {
    for (row_id, value) in changes {
        if let Some(agent_id) = value.as_ref().and_then(|value| value["id"].as_str()) {
            let mut index = 0;
            while index < actors.len() {
                if row_ids[index] != row_id && actors[index]["id"].as_str() == Some(agent_id) {
                    actors.remove(index);
                    row_ids.remove(index);
                } else {
                    index += 1;
                }
            }
        }
        match (row_ids.binary_search(&row_id), value) {
            (Ok(index), Some(value)) => actors[index] = value,
            (Ok(index), None) => {
                actors.remove(index);
                row_ids.remove(index);
            }
            (Err(index), Some(value)) => {
                actors.insert(index, value);
                row_ids.insert(index, row_id);
            }
            (Err(_), None) => {}
        }
    }
}

fn notify_all_agents(runtime: &mut HistoryRuntime, roster: &[Value]) -> Result<(), HistoryError> {
    for actor in roster {
        if actor["threadId"]
            .as_str()
            .is_some_and(|thread| !thread.is_empty())
        {
            runtime.collector.schedule.notify(
                actor["id"]
                    .as_str()
                    .ok_or(HistoryError::read(AGENT_RECORD))?,
            )?;
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
        runtime.collector.schedule.notify(
            actor["id"]
                .as_str()
                .ok_or(HistoryError::read(AGENT_RECORD))?,
        )?;
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
    async fn slow_filesystem_metadata_does_not_block_a_store_observer() {
        const OBSERVER_DEADLINE: Duration = Duration::from_secs(5);
        let root = tempdir().expect("temporary store");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        let wake = Arc::new(HistoryWake::default());
        let observer_wake = Arc::clone(&wake);
        store.add_table_change_observer(ObservedTable::Budget, move || observer_wake.budget());
        let filesystem_wake = Arc::clone(&wake);
        let (entered, checking) = oneshot::channel();
        let (release, blocked) = std::sync::mpsc::channel();
        let filesystem = std::thread::spawn(move || {
            let mut entered = Some(entered);
            filesystem_wake.filesystem_with_exists(
                Ok(Event::new(EventKind::Modify(ModifyKind::Any))
                    .add_path(PathBuf::from("/profile/sessions/rollout-thread.jsonl"))),
                Path::new("/state/accounts"),
                |_| {
                    entered
                        .take()
                        .expect("one metadata check")
                        .send(())
                        .expect("entered");
                    blocked.recv().expect("release metadata check");
                    true
                },
            );
        });
        checking.await.expect("metadata lookup started");
        let committed = tokio::time::timeout(
            OBSERVER_DEADLINE,
            store.transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_budget(id,record) VALUES('observer-witness','{}')",
                    [],
                )?;
                Ok(())
            }),
        )
        .await;
        // Release and join even when the regression blocks the transaction.
        release.send(()).expect("release filesystem");
        filesystem.join().expect("filesystem callback");
        committed
            .expect("store observer completes while metadata is blocked")
            .expect("commit");
        let pending = wake.take();
        assert!(pending.budget_dirty);
        assert_eq!(
            pending.files.values().copied().collect::<Vec<_>>(),
            vec![PendingFileEvent {
                removed: false,
                structural: false,
            }]
        );
    }

    #[tokio::test]
    async fn only_agent_commits_dirty_the_history_roster() {
        let root = tempdir().expect("temporary store");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let wake = Arc::new(HistoryWake::default());
        let observer_wake = Arc::clone(&wake);
        store.add_table_change_observer(ObservedTable::Agents, move || observer_wake.commit());
        let budget_observer_wake = Arc::clone(&wake);
        store.add_table_change_observer(ObservedTable::Budget, move || {
            budget_observer_wake.budget()
        });
        let usage_observer_wake = Arc::clone(&wake);
        store.add_table_change_observer(ObservedTable::BudgetUsage, move || {
            usage_observer_wake.budget()
        });

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

        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_budget(id,record) VALUES('history-budget-fixture','{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("commit budget input");
        let pending = wake.take();
        assert!(
            pending.budget_dirty,
            "budget changes must wake blocked imports"
        );
        assert!(
            !pending.roster_dirty,
            "budget changes must not read or refresh the agent roster"
        );

        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_budget_usage(id,agent,kind,tokens,at) VALUES('history-budget-usage-fixture','history-agent','response',1,1.0)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("commit usage repair");
        assert!(
            wake.take().budget_dirty,
            "usage changes also resume budget-invalid imports"
        );
    }

    #[test]
    fn retired_tombstones_leave_the_roster_until_their_deletion_changes() {
        let settled = BTreeMap::from([("gone".to_owned(), json!("2026-10-18T00:00:00Z"))]);
        let live = json!({"id":"live","threadId":"t","deletedAt":null});
        let never_started = json!({"id":"empty","threadId":"","deletedAt":"2026-10-18T00:00:00Z"});
        let settled_tombstone =
            json!({"id":"gone","threadId":"t","deletedAt":"2026-10-18T00:00:00Z"});
        let pending_tombstone =
            json!({"id":"late","threadId":"t","deletedAt":"2026-10-18T00:00:00Z"});
        let redeleted = json!({"id":"gone","threadId":"t","deletedAt":"2026-10-19T00:00:00Z"});
        assert!(!is_retired(&live, &settled));
        assert!(is_retired(&never_started, &settled));
        assert!(is_retired(&settled_tombstone, &settled));
        assert!(
            !is_retired(&pending_tombstone, &settled),
            "final cost catch-up still owed"
        );
        assert!(
            !is_retired(&redeleted, &settled),
            "a changed deletion is imported again"
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn roster_reads_skip_settled_tombstones_and_final_checkpoints_seed_them() {
        let root = tempdir().expect("temporary store");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        for (id, deleted_at) in [
            ("live", Value::Null),
            ("done", json!(5.0)),
            ("owed", json!(6.0)),
            ("claude-done", json!(7.0)),
        ] {
            let actor = AgentRecord::from_value(json!({
                "id":id, "rootId":id, "parentId":null,
                "name":"Tombstone fixture", "epoch":0, "status":"idle",
                "accountHistory":[], "provider":if id.starts_with("claude") { "claude" } else { "codex" },
                "threadId":id, "turnId":"turn", "effort":"high", "fastMode":false,
                "daybreakEnabled":false, "cyberAccessProgram":false,
                "cwd":"/fixture", "deletedAt":deleted_at
            }))
            .expect("typed fixture agent");
            store
                .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&actor))
                .await
                .expect("commit agent row");
        }
        let db = rusqlite::Connection::open(root.path().join("analytics.sqlite3"))
            .expect("analytics database");
        for (agent, status) in [
            ("done", "current"),
            ("owed", "catchingUp"),
            ("claude-done", "liveOnly"),
        ] {
            let record = json!({
                "id": format!("{agent}:default:{agent}"), "agent": agent, "accountKey": "default",
                "threadId": agent, "deletedAt": match agent { "done" => json!(5.0), "owed" => json!(6.0), _ => json!(7.0) },
                "status": status, "offset": 0, "importedRecords": 0, "malformedLines": 0,
                "context": {"threadId": agent}
            });
            db.execute(
                "INSERT INTO analytics_history(id,agent,record) VALUES (?1,?2,?3)",
                rusqlite::params![record["id"].as_str(), agent, record.to_string()],
            )
            .expect("seed final checkpoint");
        }
        drop(db);
        let settled = read_settled_tombstones(root.path()).expect("settled tombstones");
        assert_eq!(
            settled,
            BTreeMap::from([
                ("claude-done".to_owned(), json!(7.0)),
                ("done".to_owned(), json!(5.0)),
            ])
        );
        let (row_ids, roster) = HistoryRuntime::read_roster(&store, &settled)
            .await
            .expect("roster");
        let ids = roster
            .iter()
            .map(|a| a["id"].as_str().expect("id"))
            .collect::<Vec<_>>();
        assert_eq!(ids, ["live", "owed"]);
        assert_eq!(row_ids.len(), 2);
        let rows = read_roster_rows(&store, row_ids, &settled)
            .await
            .expect("rows");
        assert!(rows.iter().all(|(_, agent)| agent.is_some()));
    }

    #[test]
    fn roster_hints_replace_reinserted_agents_and_keep_rowid_order() {
        let mut actors = vec![json!({"id":"a"}), json!({"id":"b"}), json!({"id":"c"})];
        let mut row_ids = vec![1, 2, 3];
        // REPLACE gives agent "b" a new rowid and reports no delete for row 2.
        apply_roster_rows(
            &mut actors,
            &mut row_ids,
            vec![(4, Some(json!({"id":"b","name":"replacement"})))],
        );
        assert_eq!(row_ids, vec![1, 3, 4]);
        assert_eq!(
            actors,
            vec![
                json!({"id":"a"}),
                json!({"id":"c"}),
                json!({"id":"b","name":"replacement"}),
            ]
        );
    }

    #[test]
    fn roster_hints_remove_physical_rows_and_keep_tombstones() {
        let mut actors = vec![json!({"id":"a"}), json!({"id":"b"})];
        let mut row_ids = vec![1, 2];
        apply_roster_rows(
            &mut actors,
            &mut row_ids,
            vec![
                (1, None),
                (
                    2,
                    Some(json!({"id":"b","deletedAt":"2026-10-18T00:00:00Z"})),
                ),
            ],
        );
        assert_eq!(row_ids, vec![2]);
        assert_eq!(
            actors,
            vec![json!({"id":"b","deletedAt":"2026-10-18T00:00:00Z"})]
        );
    }

    #[test]
    fn agent_row_hints_coalesce_and_full_or_overflow_requires_reread() {
        let wake = HistoryWake::default();
        wake.agents(TableChange::Rows(&[3, 5, 3]));
        wake.agents(TableChange::Rows(&[5, 9]));
        let pending = wake.take();
        assert!(!pending.roster_dirty);
        assert_eq!(pending.agent_rows, BTreeSet::from([3, 5, 9]));

        let overflow = vec![7; MAX_PENDING_AGENT_ROW_HINTS + 1];
        wake.agents(TableChange::Rows(&overflow));
        assert!(wake.take().roster_dirty);

        wake.agents(TableChange::Full);
        assert!(wake.take().roster_dirty);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn row_hint_rereads_only_the_selected_agent_row() {
        let root = tempdir().expect("temporary store");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        for id in ["row-agent-a", "row-agent-b"] {
            let actor = AgentRecord::from_value(json!({
                "id":id, "rootId":id, "parentId":null,
                "name":"Row fixture", "epoch":0, "status":"running",
                "accountHistory":[], "provider":"codex", "threadId":id,
                "turnId":"row-turn", "effort":"high", "fastMode":false,
                "daybreakEnabled":false, "cyberAccessProgram":false,
                "cwd":"/fixture", "deletedAt":null
            }))
            .expect("typed fixture agent");
            store
                .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&actor))
                .await
                .expect("commit agent row");
        }
        let (row_ids, _) = HistoryRuntime::read_roster(&store, &BTreeMap::new())
            .await
            .expect("full roster");
        assert_eq!(row_ids.len(), 2);
        assert!(row_ids[0] < row_ids[1], "full roster keeps rowid order");
        let row_b = row_ids[1];

        let changes = read_roster_rows(&store, vec![row_b], &BTreeMap::new())
            .await
            .expect("row hint read");
        assert_eq!(changes.len(), 1);
        assert_eq!(changes[0].0, row_b);
        assert_eq!(
            changes[0].1.as_ref().expect("row exists")["id"],
            json!("row-agent-b")
        );

        store
            .transaction(|tx| {
                tx.execute("DELETE FROM runtime_agents WHERE id = 'row-agent-b'", [])?;
                Ok(())
            })
            .await
            .expect("delete agent row");
        assert_eq!(
            read_roster_rows(&store, vec![row_b], &BTreeMap::new())
                .await
                .expect("deleted row hint"),
            vec![(row_b, None)]
        );
    }

    #[test]
    fn watcher_events_coalesce_by_path_and_overflow_requests_rescan() {
        let path = PathBuf::from("/profile/sessions/rollout-thread.jsonl");
        let create_then_modify =
            PathBuf::from("/profile/sessions/rollout-create-then-modify.jsonl");
        let mut pending = PendingHistoryEvents::default();
        record_file_change(
            &mut pending,
            create_then_modify.clone(),
            PendingFileEvent {
                removed: false,
                structural: true,
            },
        );
        record_file_change(
            &mut pending,
            create_then_modify.clone(),
            PendingFileEvent {
                removed: false,
                structural: false,
            },
        );
        assert_eq!(
            pending.files.get(&create_then_modify),
            Some(&PendingFileEvent {
                removed: false,
                structural: true,
            }),
            "a later data modification must not erase an earlier structural event"
        );

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
        assert_eq!(pending.files.len(), 2);
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

    /// Cutover attempt 3: the kqueue watcher held one descriptor per session file
    /// and folder (about 14,000). New descriptors then got numbers above macOS
    /// OPEN_MAX (10240), and every child spawn of the server failed with EBADF
    /// from posix_spawn, so Git workspace checks failed.
    #[cfg(target_os = "macos")]
    #[test]
    fn the_history_watcher_does_not_hold_a_descriptor_per_session_file() {
        fn open_descriptors() -> usize {
            std::fs::read_dir("/dev/fd")
                .expect("descriptor table")
                .count()
        }
        let root = tempdir().expect("temporary state");
        let home = root.path().join("profile");
        for day in 0..8 {
            let folder = home.join(format!("sessions/2026/10/{day:02}"));
            std::fs::create_dir_all(&folder).expect("session folder");
            for file in 0..32 {
                std::fs::write(folder.join(format!("rollout-{file}.jsonl")), "{}\n")
                    .expect("session file");
            }
        }
        std::fs::create_dir_all(root.path().join("accounts")).expect("registry folder");
        let before = open_descriptors();
        let mut watcher = HistoryWatcher::new(root.path(), Arc::new(HistoryWake::default()))
            .expect("history watcher");
        watcher
            .watch_profiles(BTreeMap::from([("account".to_owned(), home)]))
            .expect("profile watch");
        let held = open_descriptors().saturating_sub(before);
        // 256 files exceed the bound even without counting the eight folders.
        assert!(held < 200, "the watcher holds {held} descriptors");
        drop(watcher);
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

    async fn wait_for_offset(wake: &HistoryWake, state: &Path, offset: u64) -> Value {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = wake.progressed.notified();
                if let Some(checkpoint) = read_history_checkpoint(state)
                    && checkpoint["offset"].as_u64() == Some(offset)
                {
                    return checkpoint;
                }
                notified.await;
            }
        })
        .await
        .expect("history watcher should import before its deadline")
    }

    async fn wait_for_step_completion(wake: &HistoryWake, count: u64) {
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = wake.progressed.notified();
                if wake.step_completions.load(Ordering::Acquire) >= count {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("history step completion deadline");
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

        // Use the production registry path so the watcher observes accounts,
        // rather than falling back to the parent containing SQLite WAL files.
        let account_path = state.join("accounts/registry.json");
        let account_dir = account_path.parent().expect("fixture account parent");
        std::fs::create_dir_all(account_dir).expect("fixture account directory");
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
        // Python creates the registry private, and the fresh state check
        // accepts only that shape before the databases exist.
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(account_dir, std::fs::Permissions::from_mode(0o700))
                .expect("private fixture account directory");
            std::fs::set_permissions(&account_path, std::fs::Permissions::from_mode(0o600))
                .expect("private fixture account registry");
        }
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

        let second_thread = "history-thread-after-recovery";
        let second_response = "history-response-after-recovery";
        let second_rollout = sessions.join("rollout-20261016-history-thread-after-recovery.jsonl");
        let second_records = [
            json!({"type":"session_meta","timestamp":1,"payload":{"id":second_thread}}),
            json!({"type":"event_msg","timestamp":2,"payload":{"type":"task_started","turn_id":"history-turn-after-recovery"}}),
            json!({"type":"event_msg","timestamp":3,"payload":{"type":"task_complete","turn_id":"history-turn-after-recovery","status":"completed"}}),
            json!({"type":"token_usage_record","timestamp":4,"payload":{
                "thread_id":second_thread,"response_id":second_response,
                "usage":{"total_tokens":23},"thread_token_usage":{"total_tokens":23}
            }}),
        ];
        let mut second_bytes = Vec::new();
        for record in second_records {
            second_bytes.extend(serde_json::to_vec(&record).expect("recovery rollout record"));
            second_bytes.push(b'\n');
        }
        std::fs::write(&second_rollout, &second_bytes).expect("fixture recovery rollout");

        let runtime = HistoryRuntime::new(Arc::clone(&store), accounts);
        let (shutdown, receiver) = watch::channel(false);
        let wake = Arc::new(HistoryWake::default());
        let task = tokio::spawn(run_event_driven_with_wake(
            runtime,
            receiver,
            Arc::clone(&wake),
        ));
        tokio::time::timeout(Duration::from_secs(5), wake.initialized.notified())
            .await
            .expect("history runtime initialization deadline");
        let first = wait_for_offset(&wake, &state, bytes.len() as u64).await;
        assert_eq!(first["status"], "current");
        wait_for_step_completion(&wake, 1).await;
        let attempts_before_failure = wake.step_attempts.load(Ordering::Acquire);
        let completions_before_failure = wake.step_completions.load(Ordering::Acquire);
        let budget_failures_before = wake.budget_failures.load(Ordering::Acquire);

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
        let second_agent_value = second_agent.value();
        let budget_snapshot = studio_runtime_services::budget::BudgetSnapshot::from_records(
            &second_agent_value,
            &second_agent_value,
            None,
            200.0,
        )
        .expect("valid fixture budget identity");
        let usage_params = json!({
            "threadId": second_thread,
            "turnId": "history-turn-after-recovery",
            "responseId": second_response,
            "tokenUsage": {
                "total": {"totalTokens": 23},
                "last": {"totalTokens": 23}
            },
            "requestUsage": {"totalTokens": 23},
            "rawTokenUsageRecord": {
                "response_id": second_response,
                "usage": {"total_tokens": 23},
                "thread_token_usage": {"total_tokens": 23}
            },
            "usageSource": "responseRecord"
        });
        let usage_key = studio_runtime_services::budget::UsageObservation::from_record(
            &budget_snapshot,
            &usage_params,
            4.0,
            studio_runtime_services::budget::CaptureSource::History,
        )
        .expect("valid fixture usage");
        let usage_key = usage_key.key().to_owned();
        let second_checkpoint_id = format!("{second_agent_id}:default:{second_thread}");
        let invalid_budget_usage_id = usage_key.clone();
        let invalid_budget_id = second_agent_id.clone();
        // A commit's table hints arrive when its caller resolves the transaction.
        // Another roster refresh can read the committed actor first and block it
        // for a budget change; a later budget hint from the same commit would then
        // give it an extra attempt. Commit the budget rows alone and let the loop
        // consume their hint before the actor that reads them becomes visible.
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_budget(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        invalid_budget_id,
                        json!({
                            "cutoff": 0.0,
                            "floor": 0,
                            "spent": 0,
                            "before": 0,
                            "after": 0,
                            "historicalNotices": 0,
                            "noticeSpent": 0,
                            "counter": null,
                            "thread": second_thread,
                            "noticeAt": 0.0,
                            "created": 0.0,
                            "lastAmount": null,
                            "ambiguousNotices": [],
                            "faults": []
                        })
                        .to_string()
                    ],
                )?;
                tx.execute(
                    "INSERT INTO runtime_budget_usage(id,agent,kind,tokens,at) VALUES(?1,'wrong-agent','response',23,4.0)",
                    [invalid_budget_usage_id],
                )?;
                Ok(())
            })
            .await
            .expect("persist invalid usage identity");
        // The hint is recorded before the transaction returns; the loop clears
        // it when it takes the batch.
        tokio::time::timeout(Duration::from_secs(5), async {
            while wake
                .pending
                .lock()
                .expect("pending history events")
                .budget_dirty
            {
                wake.progressed.notified().await;
            }
        })
        .await
        .expect("history loop should consume the budget hint before its deadline");
        wake.pending
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .error = Some("fixture watcher failure".to_owned());
        store
            .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&second_agent))
            .await
            .expect("persist recovery fixture agent");

        // The store commit wake releases the coalesced watcher failure. A
        // successful recovery must reread the durable roster before returning idle.
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                let notified = wake.watcher_recovered.notified();
                if wake.watcher_recoveries.load(Ordering::Acquire) != 0 {
                    break;
                }
                notified.await;
            }
        })
        .await
        .expect("filesystem watcher should recover before its deadline");

        // Watcher recovery notifies the old roster, and the queued commit adds
        // the new actor. Drain both bounded hints before asserting the second
        // actor's durable budget failure left the loop idle.
        wait_for_step_completion(&wake, completions_before_failure + 2).await;
        assert_eq!(
            wake.step_attempts.load(Ordering::Acquire),
            attempts_before_failure + 2,
            "the old-roster refresh and recovery actor each get one attempt"
        );
        assert!(
            read_history_checkpoint_for(&state, &second_checkpoint_id).is_none(),
            "invalid usage identity must retain the history checkpoint"
        );
        assert_eq!(
            wake.budget_failures.load(Ordering::Acquire),
            budget_failures_before + 1,
            "the second actor's failure must be classified as waiting for a budget change"
        );

        tokio::time::pause();
        tokio::time::advance(Duration::from_secs(60)).await;
        tokio::task::yield_now().await;
        assert_eq!(
            wake.step_attempts.load(Ordering::Acquire),
            attempts_before_failure + 2,
            "permanent budget input failures must remain idle without a timer"
        );
        assert_eq!(
            wake.step_completions.load(Ordering::Acquire),
            completions_before_failure + 2,
            "both bounded recovery attempts should finish before time advances"
        );
        assert!(
            !wake.step_pending.load(Ordering::Acquire),
            "permanent budget failure should leave no scheduled history retry"
        );
        tokio::time::resume();

        let repaired_usage_id = usage_key.clone();
        store
            .transaction(move |tx| {
                tx.execute(
                    "UPDATE runtime_budget_usage SET agent='history-agent-after-recovery' WHERE id=?1",
                    [repaired_usage_id],
                )?;
                Ok(())
            })
            .await
            .expect("repair only the durable budget usage identity");
        wait_for_step_completion(&wake, completions_before_failure + 3).await;
        let second = read_history_checkpoint_for(&state, &second_checkpoint_id)
            .expect("repaired usage should commit the second actor checkpoint");
        assert_eq!(second["offset"].as_u64(), Some(second_bytes.len() as u64));
        assert_eq!(second["status"], "current");

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
        let second =
            wait_for_offset(&wake, &state, (bytes.len() + append_bytes.len()) as u64).await;
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
                .test_reader()
                .and_then(|tx| {
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

    /// Live state keeps agents of signed-out login accounts whose profile
    /// directory is gone. The active snapshot omits those accounts. Each
    /// attempt must record `profileUnavailable` like Python, not fail the
    /// whole step with a read error.
    #[tokio::test]
    async fn signed_out_login_account_records_profile_unavailable() {
        let root = tempdir().expect("temporary fixture root");
        let state = root.path().join("state");
        std::fs::create_dir_all(&state).expect("fixture state");
        let account_path = root.path().join("accounts.json");
        std::fs::write(
            &account_path,
            json!({
                "version": 1,
                "defaultAccountKey": "default",
                "accounts": {
                    "default": {
                        "id": "default", "home": root.path(), "label": "Fixture",
                        "source": "Codex CLI", "status": "signedOut"
                    },
                    "login-removed": {
                        "id": "login-removed", "home": root.path().join("removed-login-home"),
                        "label": "Removed", "source": "Codex CLI", "status": "signedOut"
                    }
                },
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
            "accountKey": "login-removed", "accountHistory": [], "provider": "codex",
            "threadId": "history-thread", "turnId": "history-turn", "model": "fixture-model",
            "effort": "high", "fastMode": false, "daybreakEnabled": false,
            "cyberAccessProgram": false, "cwd": root.path(), "deletedAt": null
        });
        let stored_agent = AgentRecord::from_value(agent).expect("typed fixture agent");
        store
            .transaction(move |tx| Table::<AgentRecord>::new(tx).put(&stored_agent))
            .await
            .expect("persist fixture agent");

        let runtime = HistoryRuntime::new(Arc::clone(&store), accounts);
        let (shutdown, receiver) = watch::channel(false);
        let (task, trigger) = runtime.spawn_for_test(receiver);
        let advanced = trigger
            .step(HistoryClock {
                observed_at: 200.0,
                monotonic: 10.0,
            })
            .await
            .expect("an unavailable account profile is not a read failure");
        assert!(!advanced);
        let checkpoint =
            read_history_checkpoint_for(&state, "history-agent:login-removed:history-thread")
                .expect("profile refusal checkpoint");
        assert_eq!(checkpoint["status"], "profileUnavailable");

        shutdown.send_replace(true);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("history task shutdown deadline")
            .expect("history task join")
            .expect("history task clean shutdown");
    }
}
