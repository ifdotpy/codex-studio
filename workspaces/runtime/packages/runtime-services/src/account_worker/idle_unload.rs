//! Unload idle provider threads and stop an account's idle provider process.
//!
//! The worker snapshots loaded threads on attach and relevant state changes.
//! A thread whose owners have no turn, unsettled input, request,
//! monitor, or transfer (see `SchedulerRepository::idle_unload_thread_in_use`)
//! and that this worker has no request for starts its idle clock; a thread
//! idle for the `idleSessionUnloadMinutes` setting is unsubscribed after the
//! provider confirms it is idle with no background terminal or queued input
//! (a request in flight for that thread parks only that thread until the
//! request finishes; requests for other threads, such as another agent's turn
//! or a monitor command, never hold it);
//! a finished managed worker (not a lead, not waiting, no open work) is due as
//! soon as its turn settles, [`WORKER_UNLOAD_DELAY`], and so is a thread whose
//! owner was deleted; deleting an owner re-evaluates its running clock at once.
//! The next turn's `thread/resume` reloads the thread with its history.
//! When every loaded thread is unsubscribed and the worker holds no request,
//! callback, effect, or VM session for the same setting, the worker closes its
//! session, which stops the provider process. Zero minutes turns idle
//! unloading off. A request whose reply is lost leaves the thread loaded;
//! unsubscribe is never assumed.
use super::*;
use crate::provider_session_task::SessionStatus;
use std::time::Duration;
use studio_runtime_schema::shared_values::NATIVE_TERMINAL_ITEMS_PER_PAGE as TERMINAL_PAGE_LIMIT;
use studio_runtime_store::OrchestrationSettingsRepository;
use tokio::time::Instant;

/// Journal ACK progress also publishes session status. It does not invalidate
/// the loaded-thread inventory or restart its idle clocks.
pub(super) fn session_status_invalidates_snapshot(
    status: &SessionStatus,
    generation: Generation,
) -> bool {
    status.generation != generation || status.initialization != InitializationStatus::Ready
}

/// How long a loaded provider thread that no live lead or unfinished worker
/// owns (a finished managed worker's, or a deleted agent's) with a settled turn and no
/// pending input, request, or background work stays loaded; unit: wall time.
/// Zero unloads it as soon as its turn settles, so provider memory follows
/// running workers, not loaded agents (leads and ordinary chats keep the
/// `idleSessionUnloadMinutes` setting, because every resume costs them CPU and
/// time and may ask the user). Each loaded Claude thread holds its own `claude`
/// CLI (about 240 MiB RSS). The next turn resumes the thread; a cold CLI start with resume
/// costs about 0.25 CPU-s against about 0.015 CPU-s for a warm turn, and a Codex
/// unload with resume 15-70 ms against about 32 ms for a warm turn (scripted
/// model fixtures, 2026-10-19). Codex frees an unsubscribed thread after its
/// own `thread_unload_delay_secs`, which the host launch sets to zero
/// (`CODEX_THREAD_UNLOAD_DELAY` in `provider_adapter`).
const WORKER_UNLOAD_DELAY: Duration = Duration::ZERO;
/// Deadline of one provider request of the sweep.
const IDLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Retry delay after a provider snapshot or idle request has an unknown result.
const IDLE_RETRY_BACKOFF: Duration = Duration::from_secs(30);
/// Thread IDs requested per `thread/loaded/list` page.
const LOADED_PAGE_LIMIT: u64 = 100;
/// Pages read before a sweep gives up; a longer list is left for later.
const MAX_LOADED_PAGES: usize = 16;
/// `thread/read` statuses of a thread with no running turn. A turn that ended
/// with a provider error leaves the thread in `systemError` until its next
/// turn; Codex unloads and resumes such a thread like an idle one (measured on
/// codex 0.160.1, 2026-10-22), so a finished worker whose last turn failed
/// must not keep its thread and MCP server processes loaded.
const IDLE_THREAD_STATUSES: [&str; 3] = ["idle", "notLoaded", "systemError"];
/// `thread/unsubscribe` statuses that leave this connection unsubscribed.
const UNSUBSCRIBED_STATUSES: [&str; 3] = ["unsubscribed", "notSubscribed", "notLoaded"];

/// A durable usage read for idle eligibility that runs off the serialized
/// worker; its result is applied when it completes.
pub(super) type IdleSnapshotTask = tokio::task::JoinHandle<IdleSnapshotResult>;
/// When the re-check started, the candidates it read, and their usage.
pub(super) type IdleSnapshotResult =
    Result<(Instant, BTreeSet<String>, IdleUsageSnapshot), WorkerError>;

#[derive(Default)]
pub(super) struct IdleUsageSnapshot {
    busy_threads: BTreeSet<String>,
    /// Candidates no live lead or unfinished worker owns; they unload at once.
    immediate_threads: BTreeSet<String>,
    account_in_use: bool,
    /// Watch generation before the authoritative database read.
    state_generation: u64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum IdleBlocker {
    Durable,
    Provider,
    Local,
}

/// Idle clocks of one connection's loaded threads and of its process.
#[derive(Debug, Default)]
pub(super) struct IdleClocks {
    /// Provider-loaded threads known from the attach snapshot and native events.
    loaded: BTreeSet<String>,
    /// When each loaded thread was first seen unused, since its last use.
    threads: BTreeMap<String, Instant>,
    /// Loaded threads no live lead or unfinished worker owns; they unload at
    /// [`WORKER_UNLOAD_DELAY`].
    immediate: BTreeSet<String>,
    /// One cause per thread determines which event source may retry it.
    blocked: BTreeMap<String, IdleBlocker>,
    /// Threads this connection unsubscribed and has not used since.
    released: BTreeSet<String>,
    /// When the process was first seen with every loaded thread released.
    process: Option<Instant>,
    /// The event source that can recheck the idle process's remaining owner.
    process_blocked: Option<IdleBlocker>,
    /// After a sweep with an unknown provider result, no deadline is earlier
    /// than this backoff, whatever state change recomputes it.
    retry_at: Option<Instant>,
    initialized: bool,
}

impl IdleClocks {
    /// The worker sent a request for `thread`; its idle time starts again.
    pub(super) fn touch(&mut self, thread: &str) {
        self.loaded.insert(thread.to_owned());
        self.threads.remove(thread);
        self.blocked.remove(thread);
        self.released.remove(thread);
        self.process = None;
        self.process_blocked = None;
    }

    fn replace_loaded(&mut self, loaded: BTreeSet<String>) {
        self.initialized = true;
        self.loaded = loaded;
        self.process = None;
        self.process_blocked = None;
        self.threads
            .retain(|thread, _| self.loaded.contains(thread));
        self.blocked
            .retain(|thread, _| self.loaded.contains(thread));
        self.released.retain(|thread| self.loaded.contains(thread));
        self.immediate.retain(|thread| self.loaded.contains(thread));
    }

    /// Record whether `thread` unloads at once: no live lead or unfinished
    /// worker owns it.
    fn set_immediate(&mut self, thread: &str, immediate: bool) {
        if immediate {
            self.immediate.insert(thread.to_owned());
        } else {
            self.immediate.remove(thread);
        }
    }

    /// How long `thread` must stay unused before it unloads; `setting` is the
    /// `idleSessionUnloadMinutes` delay of leads and ordinary chats.
    fn unload_delay(&self, thread: &str, setting: Duration) -> Duration {
        if self.immediate.contains(thread) {
            WORKER_UNLOAD_DELAY
        } else {
            setting
        }
    }

    fn remove_loaded(&mut self, thread: &str, now: Instant) {
        self.loaded.remove(thread);
        self.immediate.remove(thread);
        self.release(thread);
        if self
            .loaded
            .iter()
            .all(|thread| self.released.contains(thread))
        {
            self.process.get_or_insert(now);
        }
    }

    fn loaded(&self) -> &BTreeSet<String> {
        &self.loaded
    }

    fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// A committed state change may change the outcome of a candidate: a
    /// durable blocker may have cleared, or the owner of a thread waiting for
    /// the idle setting may have been deleted or finished.
    pub(super) fn has_recheck_candidates(&self) -> bool {
        self.has_blocked(IdleBlocker::Durable)
            || self.process_blocked == Some(IdleBlocker::Durable)
            || self.threads.keys().any(|thread| self.setting_clock(thread))
    }

    /// `thread` runs the idle setting's clock: an unused thread of a lead or
    /// unfinished worker.
    fn setting_clock(&self, thread: &str) -> bool {
        !self.immediate.contains(thread)
            && !self.released.contains(thread)
            && !self.blocked.contains_key(thread)
    }

    /// Durably blocked candidates and running setting clocks, re-read after a
    /// committed state change.
    fn recheck_candidates(&self) -> BTreeSet<String> {
        let mut candidates = self.blocked_by(IdleBlocker::Durable);
        candidates.extend(
            self.threads
                .keys()
                .filter(|thread| self.setting_clock(thread))
                .cloned(),
        );
        candidates
    }

    pub(super) fn has_local_blockers(&self) -> bool {
        self.process_blocked == Some(IdleBlocker::Local) || self.has_blocked(IdleBlocker::Local)
    }

    fn has_blocked(&self, reason: IdleBlocker) -> bool {
        self.blocked.values().any(|blocked| *blocked == reason)
    }

    fn blocked_by(&self, reason: IdleBlocker) -> BTreeSet<String> {
        self.blocked
            .iter()
            .filter(|(_, blocked)| **blocked == reason)
            .map(|(thread, _)| thread.clone())
            .collect()
    }

    fn due_threads(&self, now: Instant, delay: Duration) -> BTreeSet<String> {
        self.threads
            .iter()
            .filter(|(thread, since)| {
                !self.released.contains(*thread)
                    && !self.blocked.contains_key(*thread)
                    && now.saturating_duration_since(**since) >= self.unload_delay(thread, delay)
            })
            .map(|(thread, _)| thread.clone())
            .collect()
    }

    /// Record whether `thread` is unused at `now`; true once it has been
    /// unused for `delay` without a break.
    fn thread_due(&mut self, thread: &str, unused: bool, now: Instant, delay: Duration) -> bool {
        if !unused {
            self.threads.remove(thread);
            return false;
        }
        let since = *self.threads.entry(thread.to_owned()).or_insert(now);
        now.saturating_duration_since(since) >= self.unload_delay(thread, delay)
    }

    fn eligibility_changed(
        &mut self,
        thread: &str,
        unused: bool,
        now: Instant,
        delay: Duration,
    ) -> bool {
        if unused {
            self.unblock(thread);
            self.thread_due(thread, true, now, delay)
        } else {
            self.thread_due(thread, false, now, delay);
            self.block(thread, IdleBlocker::Durable);
            false
        }
    }

    fn release(&mut self, thread: &str) {
        self.blocked.remove(thread);
        if let Some(since) = self.threads.remove(thread) {
            // The process has been idle at least as long as its last thread.
            self.process = Some(self.process.map_or(since, |process| process.min(since)));
        }
        self.released.insert(thread.to_owned());
    }

    fn released(&self, thread: &str) -> bool {
        self.released.contains(thread)
    }

    fn block(&mut self, thread: &str, reason: IdleBlocker) {
        self.threads.remove(thread);
        self.blocked.insert(thread.to_owned(), reason);
    }

    fn unblock(&mut self, thread: &str) {
        self.blocked.remove(thread);
    }

    /// Record whether the process is unused at `now`; true once it has been
    /// unused for `delay` without a break.
    fn process_due(&mut self, unused: bool, now: Instant, delay: Duration) -> bool {
        if !unused {
            self.process = None;
            return false;
        }
        let since = *self.process.get_or_insert(now);
        now.saturating_duration_since(since) >= delay
    }

    fn next_deadline(&self, delay: Duration) -> Option<Instant> {
        self.threads
            .iter()
            .filter(|(thread, _)| {
                !self.released.contains(*thread) && !self.blocked.contains_key(*thread)
            })
            .map(|(thread, since)| *since + self.unload_delay(thread, delay))
            .chain(
                self.process
                    .filter(|_| self.process_blocked.is_none())
                    .map(|since| since + delay),
            )
            .min()
            .map(|deadline| self.retry_at.map_or(deadline, |retry| deadline.max(retry)))
    }
}

/// The next idle deadline; `delay` is the setting, `None` when off.
fn next_deadline(clocks: &IdleClocks, delay: Option<Duration>) -> Option<Instant> {
    delay.and_then(|delay| clocks.next_deadline(delay))
}

fn empty_page(page: &Value) -> bool {
    page["data"].as_array().is_some_and(Vec::is_empty)
        && page.get("nextCursor").is_none_or(Value::is_null)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum UnloadResult {
    Unloaded,
    Busy(IdleBlocker),
    Retry,
}

impl Worker {
    pub(super) fn idle_thread_touched(&mut self, thread: &str) {
        self.idle_clocks.touch(thread);
        self.idle_deadline = next_deadline(&self.idle_clocks, self.idle_delay);
    }

    pub(super) async fn idle_sweep(&mut self, now: Instant) -> Result<(), WorkerError> {
        // A request in flight may be starting a turn; the next sweep decides.
        if self.closing
            || self.session.status.borrow().initialization != InitializationStatus::Ready
        {
            self.idle_deadline = None;
            return Ok(());
        }
        let Some(delay) = self.idle_delay else {
            self.idle_deadline = None;
            return Ok(());
        };
        // A request in flight for a due thread may be starting a turn on it;
        // that thread waits for its own request (`idle_local_work_completed`).
        // A request for another thread or for no thread (another agent's turn,
        // a monitor's `command/exec`) must not hold it: on a busy account one
        // is almost always in flight, and a finished worker's thread gets no
        // later event that would check it again. Live 2026-10-22: finished and
        // deleted workers kept their threads, and each its MCP server
        // processes, loaded for days. The process clock still waits for all
        // local work (`idle_check_process`).
        let (requested, due): (BTreeSet<String>, BTreeSet<String>) = self
            .idle_clocks
            .due_threads(now, delay)
            .into_iter()
            .partition(|thread| self.thread_requested(thread));
        for thread in &requested {
            self.idle_clocks.block(thread, IdleBlocker::Local);
        }
        self.idle_clocks.retry_at = None;
        self.idle_check_threads(now, delay, due).await
    }

    pub(super) async fn idle_initialize(&mut self, now: Instant) -> Result<(), WorkerError> {
        self.idle_initialized = true;
        let _observed_generation = *self.idle_state_changes.borrow_and_update();
        let delay = self
            .db("idle_unload_delay", |store| async move {
                store
                    .read_async("runtime-services/account_worker_read", |connection| {
                        Ok(OrchestrationSettingsRepository::idle_session_unload(
                            connection,
                        )?)
                    })
                    .await
            })
            .await;
        let delay = match delay {
            Ok(Some(delay)) => delay,
            Ok(None) => {
                self.idle_clocks = IdleClocks::default();
                self.idle_delay = None;
                self.idle_deadline = None;
                return Ok(());
            }
            Err(error) => {
                eprintln!("idle unload deferred for account {}: {error}", self.account);
                self.idle_initialized = false;
                self.idle_deadline = Some(tokio::time::Instant::now() + IDLE_RETRY_BACKOFF);
                return Ok(());
            }
        };
        self.idle_delay = Some(delay);
        self.idle_snapshot(now, delay).await
    }

    async fn idle_snapshot(&mut self, now: Instant, delay: Duration) -> Result<(), WorkerError> {
        let Some(loaded) = self.loaded_threads().await? else {
            self.idle_initialized = false;
            self.idle_deadline = Some(tokio::time::Instant::now() + IDLE_RETRY_BACKOFF);
            return Ok(());
        };
        self.thread_proofs
            .retain(|_, proof| loaded.contains(&proof.thread_id));
        self.idle_clocks.replace_loaded(loaded);
        let loaded = self.idle_clocks.loaded().clone();
        let usage = self.idle_usage_snapshot(loaded.clone()).await?;
        for thread in loaded {
            self.idle_clocks
                .set_immediate(&thread, usage.immediate_threads.contains(&thread));
            self.idle_clocks.eligibility_changed(
                &thread,
                !usage.busy_threads.contains(&thread),
                now,
                delay,
            );
        }
        self.idle_check_process(now, delay, Some(usage), false)
            .await
    }

    async fn idle_check_threads(
        &mut self,
        now: Instant,
        delay: Duration,
        threads: BTreeSet<String>,
    ) -> Result<(), WorkerError> {
        let mut retry = false;
        for thread in &threads {
            if self.closing {
                return Ok(());
            }
            if self.idle_clocks.released(thread) {
                continue;
            }
            let unused = self.thread_unused(thread).await?;
            if self
                .idle_clocks
                .eligibility_changed(thread, unused, now, delay)
            {
                match self.unload_thread(thread).await? {
                    UnloadResult::Unloaded => self.idle_clocks.release(thread),
                    UnloadResult::Busy(reason) => self.idle_clocks.block(thread, reason),
                    UnloadResult::Retry => retry = true,
                }
            }
        }
        self.idle_check_process(now, delay, None, retry).await
    }

    // Event completion can recheck the process clock while an idle provider
    // request is pumping events. It must never reenter thread inspection.
    async fn idle_check_process(
        &mut self,
        now: Instant,
        delay: Duration,
        usage_snapshot: Option<IdleUsageSnapshot>,
        retry: bool,
    ) -> Result<(), WorkerError> {
        let all_released = self
            .idle_clocks
            .loaded()
            .iter()
            .all(|thread| self.idle_clocks.released(thread));
        if !self.closing && all_released && !self.provider_quiescent() {
            self.idle_clocks.process_blocked = Some(IdleBlocker::Local);
            self.idle_deadline = next_deadline(&self.idle_clocks, Some(delay));
            return Ok(());
        }
        let locally_unused = !self.closing && all_released;
        let (account_in_use, observed_generation) = if locally_unused {
            let usage = match usage_snapshot {
                Some(usage) => usage,
                None => self.idle_usage_snapshot(BTreeSet::new()).await?,
            };
            (usage.account_in_use, usage.state_generation)
        } else {
            (false, *self.idle_state_changes.borrow())
        };
        let process_unused = locally_unused && !account_in_use;
        self.idle_clocks.process_blocked =
            (locally_unused && account_in_use).then_some(IdleBlocker::Durable);
        if self.idle_clocks.process_due(process_unused, now, delay) {
            let state_changed = *self.idle_state_changes.borrow() != observed_generation;
            if state_changed || self.idle_setting_changes.has_changed().unwrap_or(true) {
                self.idle_clocks.process_blocked = state_changed.then_some(IdleBlocker::Durable);
                // The pending watch event will recheck ownership. An unrelated
                // state change must not erase time the provider spent idle.
                self.idle_deadline = None;
            } else if !self.inbox.is_empty() || !self.queued_commands.is_empty() {
                // Commands take priority over the due timer in the serve loop.
                // Keep the clock so draining the queue cannot lose its wake.
                self.idle_deadline = next_deadline(&self.idle_clocks, Some(delay));
            } else {
                self.stop_idle_session();
                self.idle_deadline = None;
            }
        } else if retry {
            let retry_at = Instant::now() + IDLE_RETRY_BACKOFF;
            self.idle_clocks.retry_at = Some(retry_at);
            self.idle_deadline = Some(retry_at);
        } else {
            self.idle_deadline = next_deadline(&self.idle_clocks, Some(delay));
        }
        Ok(())
    }

    /// Start the durable re-check of blocked idle candidates and running setting
    /// clocks off the worker; one read is in flight at a time and later
    /// changes coalesce into the next.
    pub(super) fn idle_state_changed(&mut self, now: Instant) {
        let _ = self.idle_state_changes.borrow_and_update();
        let blocked = self.idle_clocks.recheck_candidates();
        if self.idle_delay.is_none()
            || (blocked.is_empty()
                && self.idle_clocks.process_blocked != Some(IdleBlocker::Durable))
        {
            return;
        }
        let store = Arc::clone(&self.bridge.store);
        let account = self.account.clone();
        let state_generation = *self.idle_state_changes.borrow();
        self.idle_snapshot = Some(tokio::spawn(async move {
            let usage = store
                .read_async("runtime-services/account_worker_read", move |connection| {
                    let (busy_threads, immediate_threads, account_in_use) =
                        SchedulerRepository::idle_unload_usage_snapshot(
                            connection, &account, &blocked,
                        )?;
                    Ok::<_, WorkerError>((
                        blocked,
                        IdleUsageSnapshot {
                            busy_threads,
                            immediate_threads,
                            account_in_use,
                            state_generation,
                        },
                    ))
                })
                .await?;
            Ok((now, usage.0, usage.1))
        }));
    }

    /// Apply a completed off-worker usage read to the idle clocks.
    pub(super) async fn idle_snapshot_ready(
        &mut self,
        now: Instant,
        blocked: BTreeSet<String>,
        usage: IdleUsageSnapshot,
    ) -> Result<(), WorkerError> {
        let Some(delay) = self.idle_delay else {
            return Ok(());
        };
        for thread in blocked {
            let requested = self.thread_requested(&thread);
            self.idle_clocks
                .set_immediate(&thread, usage.immediate_threads.contains(&thread));
            self.idle_clocks.eligibility_changed(
                &thread,
                !requested && !usage.busy_threads.contains(&thread),
                now,
                delay,
            );
        }
        self.idle_check_process(now, delay, Some(usage), false)
            .await
    }

    pub(super) async fn idle_local_work_completed(&mut self) -> Result<(), WorkerError> {
        if self.closing
            || self.session.status.borrow().initialization != InitializationStatus::Ready
        {
            return Ok(());
        }
        // A thread waits only for its own requests; the process waits until
        // the worker holds no local work at all.
        let blocked: BTreeSet<String> = self
            .idle_clocks
            .blocked_by(IdleBlocker::Local)
            .into_iter()
            .filter(|thread| !self.thread_requested(thread))
            .collect();
        let process = self.idle_clocks.process_blocked == Some(IdleBlocker::Local)
            && self.provider_quiescent();
        if blocked.is_empty() && !process {
            return Ok(());
        }
        for thread in &blocked {
            self.idle_clocks.unblock(thread);
        }
        let Some(delay) = self.idle_delay else {
            return Ok(());
        };
        let now = Instant::now();
        let usage = self.idle_usage_snapshot(blocked.clone()).await?;
        for thread in blocked {
            let requested = self.thread_requested(&thread);
            self.idle_clocks
                .set_immediate(&thread, usage.immediate_threads.contains(&thread));
            self.idle_clocks.eligibility_changed(
                &thread,
                !requested && !usage.busy_threads.contains(&thread),
                now,
                delay,
            );
        }
        self.idle_check_process(now, delay, Some(usage), false)
            .await
    }

    pub(super) async fn idle_setting_changed(&mut self, now: Instant) -> Result<(), WorkerError> {
        let delay = self
            .db("idle_unload_delay", |store| async move {
                store
                    .read_async("runtime-services/account_worker_read", |connection| {
                        Ok(OrchestrationSettingsRepository::idle_session_unload(
                            connection,
                        )?)
                    })
                    .await
            })
            .await;
        let delay = match delay {
            Ok(delay) => delay,
            Err(error) => {
                eprintln!(
                    "idle unload setting refresh deferred for account {}: {error}",
                    self.account
                );
                self.idle_initialized = false;
                self.idle_deadline = Some(tokio::time::Instant::now() + IDLE_RETRY_BACKOFF);
                return Ok(());
            }
        };
        self.idle_initialized = true;
        let Some(delay) = delay else {
            self.idle_delay = None;
            self.idle_clocks = IdleClocks::default();
            self.idle_deadline = None;
            return Ok(());
        };
        let newly_enabled = self.idle_delay.is_none();
        self.idle_delay = Some(delay);
        if newly_enabled || !self.idle_clocks.is_initialized() {
            self.idle_snapshot(now, delay).await
        } else {
            self.idle_deadline = next_deadline(&self.idle_clocks, self.idle_delay);
            self.idle_state_changed(now);
            Ok(())
        }
    }

    pub(super) async fn idle_provider_notification(
        &mut self,
        method: &str,
        params: Option<&Value>,
    ) -> Result<(), WorkerError> {
        let Some(thread) = params
            .and_then(|params| params.get("threadId"))
            .and_then(Value::as_str)
            .filter(|thread| !thread.is_empty())
        else {
            return Ok(());
        };
        if method == "thread/closed"
            || params.is_some_and(|params| params["status"]["type"] == "notLoaded")
        {
            self.forget_thread_proofs(thread);
            self.idle_clocks.remove_loaded(thread, Instant::now());
            self.idle_deadline = next_deadline(&self.idle_clocks, self.idle_delay);
            return Ok(());
        }
        self.idle_clocks.loaded.insert(thread.to_owned());
        if method == "turn/completed"
            || params.is_some_and(|params| params["status"]["type"] == "idle")
        {
            let Some(delay) = self.idle_delay else {
                return Ok(());
            };
            let unused = self.thread_unused(thread).await?;
            self.idle_clocks
                .eligibility_changed(thread, unused, Instant::now(), delay);
            self.idle_deadline = next_deadline(&self.idle_clocks, self.idle_delay);
        } else {
            self.idle_thread_touched(thread);
        }
        Ok(())
    }

    /// Every thread the provider has loaded, or `None` when the list is not
    /// complete; an incomplete list never stops the process.
    async fn loaded_threads(&mut self) -> Result<Option<BTreeSet<String>>, WorkerError> {
        let mut loaded = BTreeSet::new();
        let mut cursor = Value::Null;
        for _ in 0..MAX_LOADED_PAGES {
            let mut params = json!({"limit": LOADED_PAGE_LIMIT});
            if !cursor.is_null() {
                params["cursor"] = cursor;
            }
            let Some(page) = self.idle_request(Method::ThreadLoadedList, params).await? else {
                return Ok(None);
            };
            let Some(threads) = page["data"].as_array() else {
                return Ok(None);
            };
            for thread in threads {
                let Some(thread) = thread.as_str().filter(|thread| !thread.is_empty()) else {
                    return Ok(None);
                };
                loaded.insert(thread.to_owned());
            }
            cursor = page.get("nextCursor").cloned().unwrap_or(Value::Null);
            if cursor.is_null() {
                return Ok(Some(loaded));
            }
        }
        Ok(None)
    }

    /// No request of this worker targets `thread` and no durable state uses it.
    fn thread_requested(&self, thread: &str) -> bool {
        self.pending
            .values()
            .any(|pending| pending.intent.thread_id.as_deref() == Some(thread))
            || self
                .retained_requests
                .values()
                .any(|request| request.thread_id() == Some(thread))
    }

    async fn thread_unused(&mut self, thread: &str) -> Result<bool, WorkerError> {
        let requested = self.thread_requested(thread);
        if requested {
            return Ok(false);
        }
        let account = self.account.clone();
        let owned = thread.to_owned();
        let (in_use, immediate) = self
            .db("idle_unload_thread", move |store| async move {
                store
                    .read_async("runtime-services/account_worker_read", move |connection| {
                        let immediate = SchedulerRepository::idle_unload_immediate_threads(
                            connection,
                            &account,
                            &BTreeSet::from([owned.clone()]),
                        )?;
                        Ok((
                            SchedulerRepository::idle_unload_thread_in_use(
                                connection, &account, &owned,
                            )?,
                            !immediate.is_empty(),
                        ))
                    })
                    .await
            })
            .await?;
        self.idle_clocks.set_immediate(thread, immediate);
        Ok(!in_use)
    }

    /// One bounded account read for the known loaded or blocked candidates.
    async fn idle_usage_snapshot(
        &mut self,
        threads: BTreeSet<String>,
    ) -> Result<IdleUsageSnapshot, WorkerError> {
        let account = self.account.clone();
        let state_generation = *self.idle_state_changes.borrow();
        self.db("idle_unload_usage_snapshot", move |store| async move {
            store
                .read_async("runtime-services/account_worker_read", move |connection| {
                    let (busy_threads, immediate_threads, account_in_use) =
                        SchedulerRepository::idle_unload_usage_snapshot(
                            connection, &account, &threads,
                        )?;
                    Ok(IdleUsageSnapshot {
                        busy_threads,
                        immediate_threads,
                        account_in_use,
                        state_generation,
                    })
                })
                .await
        })
        .await
    }

    /// Unsubscribe `thread` once the provider confirms it is idle with no
    /// background terminal or queued input; true when it is unsubscribed.
    async fn unload_thread(&mut self, thread: &str) -> Result<UnloadResult, WorkerError> {
        // Any unload attempt may succeed, even when its reply is lost.
        self.forget_thread_proofs(thread);
        let Some(read) = self
            .idle_request(
                Method::ThreadRead,
                json!({"threadId": thread, "includeTurns": false}),
            )
            .await?
        else {
            return Ok(UnloadResult::Retry);
        };
        let native = &read["thread"];
        if native["id"] != thread
            || !native["status"]["type"]
                .as_str()
                .is_some_and(|status| IDLE_THREAD_STATUSES.contains(&status))
        {
            return Ok(UnloadResult::Busy(IdleBlocker::Provider));
        }
        for (method, params) in [
            (
                Method::ThreadBackgroundTerminalsList,
                json!({"threadId": thread, "limit": TERMINAL_PAGE_LIMIT}),
            ),
            (Method::ThreadQueueList, json!({"threadId": thread})),
        ] {
            match self.idle_request(method, params).await? {
                Some(page) if empty_page(&page) => {}
                None => return Ok(UnloadResult::Retry),
                Some(_) => return Ok(UnloadResult::Busy(IdleBlocker::Provider)),
            }
        }
        // Commands that arrived during the checks wait in the queue; recheck
        // what this worker and the store say before the unsubscribe is sent.
        if self.closing || self.thread_requested(thread) {
            return Ok(UnloadResult::Busy(IdleBlocker::Local));
        }
        if !self.thread_unused(thread).await? {
            return Ok(UnloadResult::Busy(IdleBlocker::Durable));
        }
        let Some(reply) = self
            .idle_request(Method::ThreadUnsubscribe, json!({"threadId": thread}))
            .await?
        else {
            return Ok(UnloadResult::Retry);
        };
        Ok(
            if reply["status"]
                .as_str()
                .is_some_and(|status| UNSUBSCRIBED_STATUSES.contains(&status))
            {
                UnloadResult::Unloaded
            } else {
                UnloadResult::Retry
            },
        )
    }

    /// The worker holds no provider request, callback, effect, recovery, or
    /// VM session. Buffered stream text needs no provider; closing flushes it.
    // Queued commands are fenced at the final stop decision; they do not erase
    // the provider's idle clock before those commands have been processed.
    fn provider_quiescent(&self) -> bool {
        self.pending.is_empty()
            && self.retained_requests.is_empty()
            && self.retained_frames.is_empty()
            && self.retained_barriers.is_empty()
            && self.callbacks.is_empty()
            && self.managed_callback_cursors.is_empty()
            && self.managed_callbacks.is_empty()
            && self.effects.is_empty()
            && self.deferred.is_empty()
            && self.capacity_blocked.is_empty()
            && self.vm_sessions.is_empty()
            && self.resolving_vm_actors.is_empty()
            && self.vm_notification_waiters.is_empty()
            && self.request_waiters.is_empty()
            && self.restart_recovery.is_none()
            && !self.connection_recovery_active
            && self.connection_recovery.is_empty()
            && !self.resume_preparations
            && self.preparation_waits.is_empty()
            && self.preparations.is_empty()
    }

    /// Close the session; requests that arrive from now on were never sent.
    fn stop_idle_session(&mut self) {
        self.closing = true;
        self.idle_exit
            .store(true, std::sync::atomic::Ordering::Release);
        self.inbox.close();
        while let Ok(command) = self.inbox.try_recv() {
            if let AccountCommand::RetainedRequest(command) = command {
                command.not_sent();
            }
        }
    }

    /// One sweep request on the account session. `None` is a refusal, an
    /// error reply, or an unknown outcome.
    async fn idle_request(
        &mut self,
        method: Method,
        params: Value,
    ) -> Result<Option<Value>, WorkerError> {
        let unsubscribe_thread = (method == Method::ThreadUnsubscribe)
            .then(|| params["threadId"].as_str().map(str::to_owned))
            .flatten();
        let operation = format!(
            "idle-unload:{}:{}",
            self.connection_id,
            uuid::Uuid::new_v4()
        );
        let deadline = tokio::time::Instant::now() + IDLE_REQUEST_TIMEOUT;
        let control = self.control.clone();
        // The session admits a preparation only after it hands its held journal
        // frame to this worker, so keep reading events while it waits.
        let prepared = match self
            .pump_until(
                control.prepare(RequestSpec {
                    id: RequestId::String(operation.clone()),
                    operation_id: OperationId(operation.clone()),
                    method,
                    thread_id: params["threadId"].as_str().map(str::to_owned),
                    params,
                    lightweight: true,
                    timeout_ms: IDLE_REQUEST_TIMEOUT
                        .as_millis()
                        .try_into()
                        .unwrap_or(u64::MAX),
                }),
            )
            .await
        {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(_not_prepared)) => return Ok(None),
            Err(WorkerError::Closed) => {
                self.closing = true;
                return Ok(None);
            }
            Err(error) => return Err(error),
        };
        let observed_generation = *self.idle_state_changes.borrow();
        if let Some(thread) = unsubscribe_thread.as_deref()
            && (!self.queued_commands.is_empty()
                || !self.inbox.is_empty()
                || self.closing
                || !self.thread_unused(thread).await?
                || *self.idle_state_changes.borrow() != observed_generation
                || self.idle_setting_changes.has_changed().unwrap_or(true))
        {
            return Ok(None);
        }
        let (reply, response) = tokio::sync::oneshot::channel();
        self.request_waiters.insert(operation.clone(), reply);
        if unsubscribe_thread.is_some()
            && (self.closing
                || !self.queued_commands.is_empty()
                || !self.inbox.is_empty()
                || *self.idle_state_changes.borrow() != observed_generation
                || self.idle_setting_changes.has_changed().unwrap_or(true))
        {
            self.request_waiters.remove(&operation);
            return Ok(None);
        }
        let outcome = match self.pump_until(control.submit(prepared)).await {
            Ok(Ok(_ticket)) => self
                .pump_until(tokio::time::timeout_at(deadline, response))
                .await
                .map(|reply| reply.ok().and_then(Result::ok)),
            Ok(Err(_not_admitted)) => Ok(None),
            Err(error) => Err(error),
        };
        self.request_waiters.remove(&operation);
        match outcome {
            Ok(Some(RequestOutcome::Reply {
                body: ResponseBody::Result(value),
                ..
            })) => Ok(Some(value)),
            Ok(_) => Ok(None),
            Err(WorkerError::Closed) => {
                self.closing = true;
                Ok(None)
            }
            Err(error) => Err(error),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_runtime_process::{
        EventCursor,
        provider_seam::{AttachmentInfo, AttachmentStatus},
    };

    #[test]
    fn acknowledged_cursor_keeps_the_loaded_snapshot_valid() {
        let generation = Generation(7);
        let handle = ProviderHandle::for_account("default");
        let mut status = SessionStatus {
            generation,
            initialization: InitializationStatus::Ready,
            initialize_result: None,
            attachment: AttachmentStatus {
                handle: handle.clone(),
                open_attempted: true,
                initialization: InitializationStatus::Ready,
                info: Some(AttachmentInfo {
                    handle,
                    generation,
                    pid: 1,
                    acknowledged: EventCursor(0),
                    resumed: false,
                }),
            },
        };
        assert!(!session_status_invalidates_snapshot(&status, generation));
        status
            .attachment
            .info
            .as_mut()
            .expect("ready session has attachment metadata")
            .acknowledged = EventCursor(1);
        assert!(!session_status_invalidates_snapshot(&status, generation));
        status.initialization = InitializationStatus::AwaitingReply;
        assert!(session_status_invalidates_snapshot(&status, generation));
        status.initialization = InitializationStatus::Ready;
        status.generation = Generation(generation.0 + 1);
        assert!(session_status_invalidates_snapshot(&status, generation));
    }

    const DELAY: Duration = Duration::from_secs(15 * 60);

    #[test]
    fn a_thread_is_due_only_after_the_delay_without_a_break() {
        let start = Instant::now();
        let mut clocks = IdleClocks::default();
        assert!(!clocks.thread_due("t", true, start, DELAY));
        assert!(!clocks.thread_due("t", true, start + DELAY / 2, DELAY));
        assert!(
            !clocks.thread_due("t", false, start + DELAY, DELAY),
            "use resets"
        );
        assert!(!clocks.thread_due("t", true, start + DELAY, DELAY));
        assert!(clocks.thread_due("t", true, start + DELAY * 2, DELAY));
        clocks.touch("t");
        assert!(
            !clocks.thread_due("t", true, start + DELAY * 2, DELAY),
            "a request starts the clock again"
        );
    }

    #[test]
    fn the_process_stops_with_its_last_thread_or_after_its_own_delay() {
        let start = Instant::now();
        let mut clocks = IdleClocks::default();
        assert!(clocks.thread_due("t", true, start, Duration::ZERO));
        clocks.release("t");
        assert!(clocks.released("t"));
        assert!(
            clocks.process_due(true, start + DELAY, DELAY),
            "the process has been idle since its last thread was"
        );
        clocks.process_due(false, start + DELAY, DELAY);
        assert!(
            !clocks.process_due(true, start + DELAY, DELAY),
            "use resets"
        );
        assert!(clocks.process_due(true, start + DELAY * 2, DELAY));
        clocks.touch("t");
        assert!(!clocks.released("t"), "a request reloads a released thread");
        clocks.release("u");
        clocks.replace_loaded(BTreeSet::new());
        assert!(!clocks.released("u"), "an unlisted thread is forgotten");
    }

    #[test]
    fn a_local_process_owner_parks_the_due_timer_until_its_completion_event() {
        let start = Instant::now();
        let mut clocks = IdleClocks::default();
        assert!(!clocks.process_due(true, start, DELAY));
        clocks.process_blocked = Some(IdleBlocker::Local);
        assert!(clocks.has_local_blockers());
        assert!(!clocks.has_recheck_candidates());
        assert_eq!(clocks.next_deadline(DELAY), None);

        clocks.process_blocked = None;
        assert_eq!(clocks.next_deadline(DELAY), Some(start + DELAY));
        assert!(clocks.process_due(true, start + DELAY, DELAY));
    }

    #[test]
    fn idle_deadline_is_exact_and_absent_when_disabled_or_empty() {
        let start = Instant::now();
        let mut clocks = IdleClocks::default();
        assert_eq!(clocks.next_deadline(DELAY), None, "empty stays asleep");
        assert_eq!(next_deadline(&clocks, None), None, "disabled stays asleep");

        clocks.thread_due("early", true, start, DELAY);
        clocks.thread_due("later", true, start + DELAY / 2, DELAY);
        assert_eq!(clocks.next_deadline(DELAY), Some(start + DELAY));

        clocks.touch("early");
        assert_eq!(
            clocks.next_deadline(DELAY),
            Some(start + DELAY * 3 / 2),
            "new input cancels the stale deadline and preserves the next idle thread"
        );
    }

    #[test]
    fn durable_blockers_wait_for_a_change_hint_instead_of_a_timer() {
        let start = Instant::now();
        let mut clocks = IdleClocks::default();
        clocks.replace_loaded(BTreeSet::from([
            "blocked".to_owned(),
            "streaming".to_owned(),
        ]));
        clocks.thread_due("blocked", true, start, DELAY);
        clocks.thread_due("streaming", true, start, DELAY);
        clocks.block("blocked", IdleBlocker::Durable);

        assert_eq!(
            clocks.due_threads(start + DELAY, DELAY),
            BTreeSet::from(["streaming".to_owned()])
        );
        assert_eq!(clocks.next_deadline(DELAY), Some(start + DELAY));
        clocks.block("streaming", IdleBlocker::Durable);
        assert_eq!(clocks.next_deadline(DELAY), None);
    }

    #[test]
    fn a_local_request_rearms_its_candidate_when_the_request_drains() {
        let start = Instant::now();
        let completed = start + DELAY * 2;
        let mut clocks = IdleClocks::default();
        clocks.thread_due("thread", true, start, DELAY);
        clocks.block("thread", IdleBlocker::Local);
        assert_eq!(clocks.next_deadline(DELAY), None);

        for thread in clocks.blocked_by(IdleBlocker::Local) {
            clocks.unblock(&thread);
            clocks.eligibility_changed(&thread, true, completed, DELAY);
        }
        assert_eq!(
            clocks.next_deadline(DELAY),
            Some(completed + DELAY),
            "local completion starts a fresh idle interval without a polling retry"
        );
    }

    #[test]
    fn an_initially_blocked_thread_starts_its_clock_when_the_blocker_clears() {
        let attached = Instant::now();
        let mut clocks = IdleClocks::default();
        clocks.replace_loaded(BTreeSet::from(["thread".to_owned()]));
        assert!(!clocks.eligibility_changed("thread", false, attached, DELAY));
        assert_eq!(clocks.next_deadline(DELAY), None);

        let cleared = attached + DELAY * 2;
        assert!(!clocks.eligibility_changed("thread", true, cleared, DELAY));
        assert_eq!(clocks.next_deadline(DELAY), Some(cleared + DELAY));
        assert!(
            clocks
                .due_threads(cleared + DELAY, DELAY)
                .contains("thread")
        );
    }

    #[test]
    fn native_busy_waits_for_its_thread_event_not_an_unrelated_store_change() {
        let start = Instant::now();
        let mut clocks = IdleClocks::default();
        clocks.replace_loaded(BTreeSet::from(["thread".to_owned()]));
        clocks.block("thread", IdleBlocker::Provider);

        assert!(!clocks.has_recheck_candidates());
        assert!(clocks.blocked_by(IdleBlocker::Durable).is_empty());
        assert_eq!(clocks.next_deadline(DELAY), None);

        assert!(!clocks.eligibility_changed("thread", true, start, DELAY));
        assert_eq!(clocks.next_deadline(DELAY), Some(start + DELAY));
        assert!(!clocks.blocked.contains_key("thread"));
    }

    #[test]
    fn a_worker_is_due_at_once_and_a_lead_waits_for_the_setting() {
        let start = Instant::now();
        let mut clocks = IdleClocks::default();
        clocks.replace_loaded(BTreeSet::from(["worker".to_owned(), "lead".to_owned()]));
        clocks.set_immediate("worker", true);
        assert!(clocks.eligibility_changed("worker", true, start, DELAY));
        assert!(!clocks.eligibility_changed("lead", true, start, DELAY));
        assert_eq!(next_deadline(&clocks, Some(DELAY)), Some(start));
        assert_eq!(next_deadline(&clocks, None), None, "zero minutes is off");
        assert_eq!(
            clocks.due_threads(start, DELAY),
            BTreeSet::from(["worker".to_owned()])
        );

        clocks.release("worker");
        assert_eq!(
            next_deadline(&clocks, Some(DELAY)),
            Some(start + DELAY),
            "the lead and then the process wait for the setting"
        );
        assert_eq!(
            clocks.due_threads(start + DELAY, DELAY),
            BTreeSet::from(["lead".to_owned()])
        );
    }

    #[test]
    fn a_running_setting_clock_is_rechecked_and_due_at_once_when_its_owner_is_deleted() {
        let start = Instant::now();
        let mut clocks = IdleClocks::default();
        clocks.replace_loaded(BTreeSet::from(["lead".to_owned()]));
        assert!(!clocks.has_recheck_candidates(), "no clock runs yet");
        assert!(!clocks.eligibility_changed("lead", true, start, DELAY));
        assert!(clocks.has_recheck_candidates());
        assert_eq!(
            clocks.recheck_candidates(),
            BTreeSet::from(["lead".to_owned()])
        );

        // The re-read finds no live owner that keeps the thread any more.
        let deleted = start + DELAY / 2;
        clocks.set_immediate("lead", true);
        assert!(clocks.eligibility_changed("lead", true, deleted, DELAY));
        assert_eq!(next_deadline(&clocks, Some(DELAY)), Some(start));
        assert!(
            !clocks.has_recheck_candidates(),
            "an immediate thread waits for no state change"
        );
    }
}
