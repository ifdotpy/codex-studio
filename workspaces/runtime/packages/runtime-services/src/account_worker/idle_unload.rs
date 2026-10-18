//! Unload idle provider threads and stop an account's idle provider process.
//!
//! The worker snapshots loaded threads on attach and relevant state changes.
//! A thread whose owners have no turn, unsettled input, request,
//! monitor, or transfer (see `SchedulerRepository::idle_unload_thread_in_use`)
//! and that this worker has no request for starts its idle clock; a thread
//! idle for the `idleSessionUnloadMinutes` setting is unsubscribed after the
//! provider confirms it is idle with no background terminal or queued input.
//! When every loaded thread is unsubscribed and the worker holds no request,
//! callback, effect, or VM session, the worker closes its session, which stops
//! the provider process. The next wake starts a new session, and its
//! `thread/resume` reloads the thread with its history. A request whose reply
//! is lost leaves the thread loaded; unsubscribe is never assumed.
use super::*;
use std::time::Duration;
use studio_runtime_store::OrchestrationSettingsRepository;
use tokio::time::Instant;

/// Deadline of one provider request of the sweep.
const IDLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
/// Retry delay after a provider snapshot or idle request has an unknown result.
const IDLE_RETRY_BACKOFF: Duration = Duration::from_secs(30);
/// Thread IDs requested per `thread/loaded/list` page.
const LOADED_PAGE_LIMIT: u64 = 100;
/// Pages read before a sweep gives up; a longer list is left for later.
const MAX_LOADED_PAGES: usize = 16;
/// Background terminals requested per `thread/backgroundTerminals/list` page.
const TERMINAL_PAGE_LIMIT: u64 = 100;
/// `thread/read` statuses of a thread with no running turn.
const IDLE_THREAD_STATUSES: [&str; 2] = ["idle", "notLoaded"];
/// `thread/unsubscribe` statuses that leave this connection unsubscribed.
const UNSUBSCRIBED_STATUSES: [&str; 3] = ["unsubscribed", "notSubscribed", "notLoaded"];

#[derive(Default)]
struct IdleUsageSnapshot {
    busy_threads: BTreeSet<String>,
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
    /// One cause per thread determines which event source may retry it.
    blocked: BTreeMap<String, IdleBlocker>,
    /// Threads this connection unsubscribed and has not used since.
    released: BTreeSet<String>,
    /// When the process was first seen with every loaded thread released.
    process: Option<Instant>,
    /// The process is otherwise quiescent but a durable account-wide owner remains.
    process_blocked: bool,
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
        self.process_blocked = false;
    }

    fn replace_loaded(&mut self, loaded: BTreeSet<String>) {
        self.initialized = true;
        self.loaded = loaded;
        self.process = None;
        self.process_blocked = false;
        self.threads
            .retain(|thread, _| self.loaded.contains(thread));
        self.blocked
            .retain(|thread, _| self.loaded.contains(thread));
        self.released.retain(|thread| self.loaded.contains(thread));
    }

    fn remove_loaded(&mut self, thread: &str, now: Instant) {
        self.loaded.remove(thread);
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

    pub(super) fn has_blocked_candidates(&self) -> bool {
        self.has_blocked(IdleBlocker::Durable) || self.process_blocked
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

    fn take_blocked(&mut self, reason: IdleBlocker) -> BTreeSet<String> {
        let threads = self.blocked_by(reason);
        for thread in &threads {
            self.blocked.remove(thread);
        }
        threads
    }

    fn due_threads(&self, now: Instant, delay: Duration) -> BTreeSet<String> {
        self.threads
            .iter()
            .filter(|(thread, since)| {
                !self.released.contains(*thread)
                    && !self.blocked.contains_key(*thread)
                    && now.saturating_duration_since(**since) >= delay
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
        now.saturating_duration_since(since) >= delay
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
            .map(|(_, since)| *since + delay)
            .chain(self.process.map(|since| since + delay))
            .min()
    }
}

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
        let due = self.idle_clocks.due_threads(now, delay);
        if !self.pending.is_empty() || !self.retained_requests.is_empty() {
            for thread in &due {
                self.idle_clocks.block(thread, IdleBlocker::Local);
            }
            self.idle_deadline = next_deadline(&self.idle_clocks, self.idle_delay);
            return Ok(());
        }
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
        self.idle_clocks.replace_loaded(loaded);
        let loaded = self.idle_clocks.loaded().clone();
        let usage = self.idle_usage_snapshot(loaded.clone()).await?;
        for thread in loaded {
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
        let locally_unused = !self.closing
            && self
                .idle_clocks
                .loaded()
                .iter()
                .all(|thread| self.idle_clocks.released(thread))
            && self.quiescent()
            && self.inbox.is_empty();
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
        self.idle_clocks.process_blocked = locally_unused && account_in_use;
        if self.idle_clocks.process_due(process_unused, now, delay) {
            let state_changed = *self.idle_state_changes.borrow() != observed_generation;
            if state_changed
                || self.idle_setting_changes.has_changed().unwrap_or(true)
                || !self.inbox.is_empty()
            {
                self.idle_clocks.process_blocked = state_changed;
                self.idle_clocks.process_due(false, now, delay);
                self.idle_deadline = Some(tokio::time::Instant::now() + IDLE_RETRY_BACKOFF);
            } else {
                self.stop_idle_session();
                self.idle_deadline = None;
            }
        } else if retry {
            self.idle_deadline = Some(tokio::time::Instant::now() + IDLE_RETRY_BACKOFF);
        } else {
            self.idle_deadline = next_deadline(&self.idle_clocks, Some(delay));
        }
        Ok(())
    }

    pub(super) async fn idle_state_changed(&mut self, now: Instant) -> Result<(), WorkerError> {
        let _ = self.idle_state_changes.borrow_and_update();
        let blocked = self.idle_clocks.blocked_by(IdleBlocker::Durable);
        if blocked.is_empty() && !self.idle_clocks.process_blocked {
            return Ok(());
        }
        let Some(delay) = self.idle_delay else {
            return Ok(());
        };
        let usage = self.idle_usage_snapshot(blocked.clone()).await?;
        for thread in blocked {
            let requested = self.thread_requested(&thread);
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
            || !self.pending.is_empty()
            || !self.retained_requests.is_empty()
        {
            return Ok(());
        }
        let blocked = self.idle_clocks.take_blocked(IdleBlocker::Local);
        if blocked.is_empty() {
            return Ok(());
        }
        let Some(delay) = self.idle_delay else {
            return Ok(());
        };
        let now = Instant::now();
        let usage = self.idle_usage_snapshot(blocked.clone()).await?;
        for thread in blocked {
            let requested = self.thread_requested(&thread);
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
            self.idle_state_changed(now).await
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
        let thread = thread.to_owned();
        let in_use = self
            .db("idle_unload_thread", move |store| async move {
                store
                    .read_async("runtime-services/account_worker_read", move |connection| {
                        Ok(SchedulerRepository::idle_unload_thread_in_use(
                            connection, &account, &thread,
                        )?)
                    })
                    .await
            })
            .await?;
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
                    let (busy_threads, account_in_use) =
                        SchedulerRepository::idle_unload_usage_snapshot(
                            connection, &account, &threads,
                        )?;
                    Ok(IdleUsageSnapshot {
                        busy_threads,
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
    fn quiescent(&self) -> bool {
        self.pending.is_empty()
            && self.retained_requests.is_empty()
            && self.retained_frames.is_empty()
            && self.retained_barriers.is_empty()
            && self.callbacks.is_empty()
            && self.managed_callback_cursors.is_empty()
            && self.managed_callbacks.is_empty()
            && self.effects.is_empty()
            && self.deferred.is_empty()
            && self.queued_commands.is_empty()
            && self.vm_sessions.is_empty()
            && self.resolving_vm_actors.is_empty()
            && self.vm_notification_waiters.is_empty()
            && self.request_waiters.is_empty()
            && self.restart_recovery.is_none()
            && !self.connection_recovery_active
            && self.connection_recovery.is_empty()
            && self.retry_at.is_none()
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
        let prepared = self
            .control
            .prepare(RequestSpec {
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
            })
            .await;
        let Ok(prepared) = prepared else {
            return Ok(None);
        };
        let observed_generation = *self.idle_state_changes.borrow();
        if let Some(thread) = unsubscribe_thread.as_deref() {
            if !self.queued_commands.is_empty()
                || !self.inbox.is_empty()
                || self.closing
                || !self.thread_unused(thread).await?
                || *self.idle_state_changes.borrow() != observed_generation
                || self.idle_setting_changes.has_changed().unwrap_or(true)
            {
                return Ok(None);
            }
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
        let control = self.control.clone();
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
    fn idle_deadline_is_exact_and_absent_when_disabled_or_empty() {
        let start = Instant::now();
        let mut clocks = IdleClocks::default();
        assert_eq!(
            next_deadline(&clocks, Some(DELAY)),
            None,
            "empty stays asleep"
        );
        assert_eq!(next_deadline(&clocks, None), None, "disabled stays asleep");

        clocks.thread_due("early", true, start, DELAY);
        clocks.thread_due("later", true, start + DELAY / 2, DELAY);
        assert_eq!(next_deadline(&clocks, Some(DELAY)), Some(start + DELAY));

        clocks.touch("early");
        assert_eq!(
            next_deadline(&clocks, Some(DELAY)),
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
        assert_eq!(next_deadline(&clocks, Some(DELAY)), Some(start + DELAY));
        clocks.block("streaming", IdleBlocker::Durable);
        assert_eq!(next_deadline(&clocks, Some(DELAY)), None);
    }

    #[test]
    fn a_local_request_rearms_its_candidate_when_the_request_drains() {
        let start = Instant::now();
        let completed = start + DELAY * 2;
        let mut clocks = IdleClocks::default();
        clocks.thread_due("thread", true, start, DELAY);
        clocks.block("thread", IdleBlocker::Local);
        assert_eq!(next_deadline(&clocks, Some(DELAY)), None);

        let local_blocked = clocks.take_blocked(IdleBlocker::Local);
        for thread in local_blocked {
            clocks.eligibility_changed(&thread, true, completed, DELAY);
        }
        assert_eq!(
            next_deadline(&clocks, Some(DELAY)),
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
        assert_eq!(next_deadline(&clocks, Some(DELAY)), None);

        let cleared = attached + DELAY * 2;
        assert!(!clocks.eligibility_changed("thread", true, cleared, DELAY));
        assert_eq!(next_deadline(&clocks, Some(DELAY)), Some(cleared + DELAY));
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

        assert!(!clocks.has_blocked_candidates());
        assert!(clocks.blocked_by(IdleBlocker::Durable).is_empty());
        assert_eq!(next_deadline(&clocks, Some(DELAY)), None);

        assert!(!clocks.eligibility_changed("thread", true, start, DELAY));
        assert_eq!(next_deadline(&clocks, Some(DELAY)), Some(start + DELAY));
        assert!(!clocks.blocked.contains_key("thread"));
    }
}
