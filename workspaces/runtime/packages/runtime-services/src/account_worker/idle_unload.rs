//! Unload idle provider threads and stop an account's idle provider process.
//!
//! Every [`IDLE_SWEEP_INTERVAL`] the worker lists the threads its provider has
//! loaded. A thread whose owners have no turn, unsettled input, request,
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

/// How often an account worker looks for idle threads.
pub(super) const IDLE_SWEEP_INTERVAL: Duration = Duration::from_secs(60);
/// Deadline of one provider request of the sweep.
const IDLE_REQUEST_TIMEOUT: Duration = Duration::from_secs(10);
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

/// Idle clocks of one connection's loaded threads and of its process.
#[derive(Debug, Default)]
pub(super) struct IdleClocks {
    /// When each loaded thread was first seen unused, since its last use.
    threads: BTreeMap<String, Instant>,
    /// Threads this connection unsubscribed and has not used since.
    released: BTreeSet<String>,
    /// When the process was first seen with every loaded thread released.
    process: Option<Instant>,
}

impl IdleClocks {
    /// The worker sent a request for `thread`; its idle time starts again.
    pub(super) fn touch(&mut self, thread: &str) {
        self.threads.remove(thread);
        self.released.remove(thread);
    }

    /// Forget threads the provider no longer lists as loaded.
    fn retain_loaded(&mut self, loaded: &BTreeSet<String>) {
        self.threads.retain(|thread, _| loaded.contains(thread));
        self.released.retain(|thread| loaded.contains(thread));
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

    fn release(&mut self, thread: &str) {
        if let Some(since) = self.threads.remove(thread) {
            // The process has been idle at least as long as its last thread.
            self.process = Some(self.process.map_or(since, |process| process.min(since)));
        }
        self.released.insert(thread.to_owned());
    }

    fn released(&self, thread: &str) -> bool {
        self.released.contains(thread)
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
}

fn empty_page(page: &Value) -> bool {
    page["data"].as_array().is_some_and(Vec::is_empty)
        && page.get("nextCursor").is_none_or(Value::is_null)
}

impl Worker {
    pub(super) async fn idle_sweep(&mut self, now: Instant) -> Result<(), WorkerError> {
        self.idle_sweep_at = tokio::time::Instant::now() + IDLE_SWEEP_INTERVAL;
        if self.closing
            || self.session.status.borrow().initialization != InitializationStatus::Ready
        {
            return Ok(());
        }
        let delay = match self
            .db("idle_unload_delay", |store| async move {
                blocking_store_read(store, |store| {
                    Ok(OrchestrationSettingsRepository::idle_session_unload(
                        &*store.read()?,
                    )?)
                })
                .await
            })
            .await
        {
            Ok(Some(delay)) => delay,
            Ok(None) => {
                self.idle_clocks = IdleClocks::default();
                return Ok(());
            }
            Err(error) => {
                eprintln!("idle unload deferred for account {}: {error}", self.account);
                return Ok(());
            }
        };
        let Some(loaded) = self.loaded_threads().await? else {
            return Ok(());
        };
        self.idle_clocks.retain_loaded(&loaded);
        for thread in &loaded {
            if self.closing {
                return Ok(());
            }
            if self.idle_clocks.released(thread) {
                continue;
            }
            let unused = self.thread_unused(thread).await?;
            if self.idle_clocks.thread_due(thread, unused, now, delay)
                && self.unload_thread(thread).await?
            {
                self.idle_clocks.release(thread);
            }
        }
        let process_unused = !self.closing
            && loaded
                .iter()
                .all(|thread| self.idle_clocks.released(thread))
            && self.quiescent()
            && !self.account_in_use().await?;
        if self.idle_clocks.process_due(process_unused, now, delay) {
            self.stop_idle_session();
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
    async fn thread_unused(&mut self, thread: &str) -> Result<bool, WorkerError> {
        let requested = self
            .pending
            .values()
            .any(|pending| pending.intent.thread_id.as_deref() == Some(thread))
            || self
                .retained_requests
                .values()
                .any(|request| request.thread_id() == Some(thread));
        if requested {
            return Ok(false);
        }
        let account = self.account.clone();
        let thread = thread.to_owned();
        let in_use = self
            .db("idle_unload_thread", move |store| async move {
                blocking_store_read(store, move |store| {
                    Ok(SchedulerRepository::idle_unload_thread_in_use(
                        &*store.read()?,
                        &account,
                        &thread,
                    )?)
                })
                .await
            })
            .await?;
        Ok(!in_use)
    }

    async fn account_in_use(&mut self) -> Result<bool, WorkerError> {
        let account = self.account.clone();
        self.db("idle_unload_account", move |store| async move {
            blocking_store_read(store, move |store| {
                Ok(SchedulerRepository::idle_unload_account_in_use(
                    &*store.read()?,
                    &account,
                )?)
            })
            .await
        })
        .await
    }

    /// Unsubscribe `thread` once the provider confirms it is idle with no
    /// background terminal or queued input; true when it is unsubscribed.
    async fn unload_thread(&mut self, thread: &str) -> Result<bool, WorkerError> {
        let Some(read) = self
            .idle_request(
                Method::ThreadRead,
                json!({"threadId": thread, "includeTurns": false}),
            )
            .await?
        else {
            return Ok(false);
        };
        let native = &read["thread"];
        if native["id"] != thread
            || !native["status"]["type"]
                .as_str()
                .is_some_and(|status| IDLE_THREAD_STATUSES.contains(&status))
        {
            return Ok(false);
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
                _ => return Ok(false),
            }
        }
        // Commands that arrived during the checks wait in the queue; recheck
        // what this worker and the store say before the unsubscribe is sent.
        if !self.thread_unused(thread).await? || self.closing {
            return Ok(false);
        }
        Ok(self
            .idle_request(Method::ThreadUnsubscribe, json!({"threadId": thread}))
            .await?
            .is_some_and(|reply| {
                reply["status"]
                    .as_str()
                    .is_some_and(|status| UNSUBSCRIBED_STATUSES.contains(&status))
            }))
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
        self.idle_stopped = true;
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
        let (reply, response) = tokio::sync::oneshot::channel();
        self.request_waiters.insert(operation.clone(), reply);
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
        clocks.retain_loaded(&BTreeSet::new());
        assert!(!clocks.released("u"), "an unlisted thread is forgotten");
    }
}
