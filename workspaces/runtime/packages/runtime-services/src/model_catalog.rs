//! Bounded, process-local model catalog reads for display.

use crate::provider_session_task::{
    RequestSpec, SessionHandle, SessionOutcome, SessionTaskConfig, SessionTaskError, WriteOutcome,
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(unix)]
use studio_runtime_process::provider_stdio::StdioProviderTaskFactory;
use studio_runtime_process::{
    Generation, OperationId, ProviderHandle, ProviderLaunchSpec,
    provider_seam::{
        InitializationStatus, OpenOptions, ProviderTaskFactory, SessionClock, Shutdown,
    },
};
use studio_runtime_providers::{Method, RequestId, RequestOutcome, ResponseBody};
use tokio::time::Instant;

const MAX_REFRESHES: usize = 4;
const CATALOG_TTL: Duration = Duration::from_secs(300);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CATALOG_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CATALOG_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CATALOG_PAGES: usize = 100;
pub type ChangeCallback = Arc<dyn Fn() + Send + Sync + 'static>;

#[derive(Clone, Debug, PartialEq)]
pub enum CatalogRead {
    Ready(Value),
    Pending,
    Unavailable,
}

struct CacheEntry {
    generation: u64,
    value: Option<Value>,
    expires_at: Option<Instant>,
    error: bool,
    pending: bool,
}

struct CatalogInner {
    executable: Option<PathBuf>,
    cache: Mutex<HashMap<(String, String, String), CacheEntry>>,
    next_generation: Mutex<u64>,
    on_change: ChangeCallback,
    slots: Arc<tokio::sync::Semaphore>,
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    stop: Shutdown,
}

/// A single HTTP-state-owned cache and its provider process configuration.
#[derive(Clone)]
pub struct ModelCatalogService(Arc<CatalogInner>);

impl ModelCatalogService {
    #[must_use]
    pub fn new(executable: Option<String>) -> Self {
        Self::with_change_callback(executable, Arc::new(|| {}))
    }

    /// Construct the cache with a shared resource-change callback for completed reads.
    #[must_use]
    pub fn with_change_callback(executable: Option<String>, on_change: ChangeCallback) -> Self {
        Self(Arc::new(CatalogInner {
            executable: executable.map(PathBuf::from),
            cache: Mutex::new(HashMap::new()),
            next_generation: Mutex::new(0),
            on_change,
            slots: Arc::new(tokio::sync::Semaphore::new(MAX_REFRESHES)),
            tasks: Mutex::new(Vec::new()),
            stop: Shutdown::default(),
        }))
    }

    /// Display reads return stale data immediately while one refresh runs.
    pub async fn read(
        &self,
        account_key: &str,
        home: &str,
        provider: &str,
        retry: bool,
    ) -> CatalogRead {
        if provider != "codex" || *self.0.stop.subscribe().borrow() {
            return CatalogRead::Unavailable;
        }

        let now = Instant::now();
        let cache_key = (account_key.to_owned(), home.to_owned(), provider.to_owned());
        let (generation, stale, permit) = {
            let Ok(mut cache) = self.0.cache.lock() else {
                return CatalogRead::Unavailable;
            };
            if let Some(entry) = cache.get_mut(&cache_key) {
                if entry.pending {
                    return match &entry.value {
                        Some(value) => CatalogRead::Ready(value.clone()),
                        None => CatalogRead::Pending,
                    };
                }
                if entry.expires_at.is_some_and(|expires| expires > now) {
                    if entry.error {
                        if !retry {
                            return CatalogRead::Unavailable;
                        }
                    } else if let Some(value) = &entry.value {
                        return CatalogRead::Ready(value.clone());
                    }
                }
            }
            let stale = cache.get(&cache_key).and_then(|entry| entry.value.clone());
            let Ok(permit) = self.0.slots.clone().try_acquire_owned() else {
                return stale.map_or(CatalogRead::Pending, CatalogRead::Ready);
            };
            // A changed home/provider replaces the same account's former entry.
            cache.retain(|key, _| key.0 != account_key || *key == cache_key);
            let Ok(mut next) = self.0.next_generation.lock() else {
                return CatalogRead::Unavailable;
            };
            *next = next.saturating_add(1);
            let generation = *next;
            cache.insert(
                cache_key.clone(),
                CacheEntry {
                    generation,
                    value: stale.clone(),
                    expires_at: None,
                    error: false,
                    pending: true,
                },
            );
            (generation, stale, permit)
        };

        let service = Arc::downgrade(&self.0);
        let executable = self.0.executable.clone();
        let stop = self.0.stop.clone();
        let account = account_key.to_owned();
        let home = home.to_owned();
        let mut tasks = self.0.tasks.lock().expect("catalog task registry");
        if *self.0.stop.subscribe().borrow() {
            return CatalogRead::Unavailable;
        }
        tasks.retain(|task| !task.is_finished());
        tasks.push(tokio::spawn(async move {
            let _permit = permit;
            let result =
                read_provider_catalog(executable.as_deref(), &home, account != "default", stop)
                    .await;
            if let Some(inner) = service.upgrade() {
                store_result(&inner, &cache_key, generation, result);
            }
        }));
        stale.map_or(CatalogRead::Pending, CatalogRead::Ready)
    }
    pub fn retain_accounts(&self, accounts: &[Value]) {
        self.0
            .cache
            .lock()
            .expect("catalog cache")
            .retain(|key, _| {
                accounts.iter().any(|account| {
                    account["id"].as_str() == Some(key.0.as_str())
                        && account["home"].as_str().unwrap_or("") == key.1
                        && account["provider"].as_str().unwrap_or("codex") == key.2
                })
            });
    }
    pub async fn shutdown(&self) -> std::io::Result<()> {
        let tasks = {
            let mut tasks = self.0.tasks.lock().expect("catalog tasks");
            self.0.stop.cancel();
            std::mem::take(&mut *tasks)
        };
        let mut result = Ok(());
        for task in tasks {
            if let Err(error) = task.await {
                result = Err(std::io::Error::other(error));
            }
        }
        result
    }
}

impl Drop for CatalogInner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn store_result(
    inner: &CatalogInner,
    key: &(String, String, String),
    generation: u64,
    result: Result<Value, ()>,
) -> bool {
    let stored = if let Ok(mut cache) = inner.cache.lock()
        && let Some(entry) = cache.get_mut(key)
        && entry.generation == generation
        && entry.pending
    {
        entry.pending = false;
        entry.expires_at = Some(Instant::now() + CATALOG_TTL);
        match result {
            Ok(value) => {
                entry.value = Some(value);
                entry.error = false;
            }
            Err(()) => entry.error = true,
        }
        true
    } else {
        false
    };
    if stored {
        (inner.on_change)();
    }
    stored
}

async fn read_provider_catalog(
    executable: Option<&std::path::Path>,
    home: &str,
    isolated: bool,
    stop: Shutdown,
) -> Result<Value, ()> {
    let started = Instant::now();
    let executable = executable.unwrap_or_else(|| std::path::Path::new("codex"));
    let mut command = vec![
        executable.to_string_lossy().into_owned(),
        "app-server".into(),
        "--listen".into(),
        "stdio://".into(),
    ];
    let mut environment: BTreeMap<String, String> = std::env::vars().collect();
    environment.insert("CODEX_HOME".into(), home.into());
    if isolated {
        command.extend(["-c".into(), "cli_auth_credentials_store=\"file\"".into()]);
        environment.remove("OPENAI_API_KEY");
        environment.remove("CODEX_API_KEY");
    }
    let launch = ProviderLaunchSpec::new(
        command,
        environment,
        std::env::current_dir().map_err(|_| ())?,
    );
    #[cfg(unix)]
    let factory = Arc::new(StdioProviderTaskFactory {
        deadline: started + CATALOG_TIMEOUT,
        max_frame_bytes: MAX_CATALOG_PAGE_BYTES,
        max_output_bytes: MAX_CATALOG_RESPONSE_BYTES,
    });
    #[cfg(unix)]
    {
        read_session_with_shutdown(factory, launch, started, stop).await
    }
    #[cfg(not(unix))]
    {
        let _launch = launch;
        Err(())
    }
}

#[cfg(test)]
async fn read_session(
    factory: Arc<dyn ProviderTaskFactory>,
    launch: ProviderLaunchSpec,
    started: Instant,
) -> Result<Value, ()> {
    read_session_with_shutdown(factory, launch, started, Shutdown::default()).await
}
async fn read_session_with_shutdown(
    factory: Arc<dyn ProviderTaskFactory>,
    launch: ProviderLaunchSpec,
    started: Instant,
    shutdown: Shutdown,
) -> Result<Value, ()> {
    let mut stop = shutdown.subscribe();
    if *stop.borrow() {
        return Err(());
    }
    let mut session = SessionHandle::open(
        factory,
        ProviderHandle::for_account("model-catalog"),
        OpenOptions {
            clock: SessionClock::from_origin(started),
            launch,
            expected_generation: Some(Generation(1)),
        },
        SessionTaskConfig {
            generation: Generation(1),
            claude: false,
            timeout_ms: duration_ms(CATALOG_TIMEOUT),
            initialize_operation: OperationId("catalog-initialize".into()),
            request_id_prefix: None,
            recovered: vec![],
            initialize_params: json!({
                "clientInfo": {"name": "codex_studio_catalog", "version": "1.0.0"},
                "capabilities": {"experimentalApi": true}
            }),
        },
    )
    .await
    .map_err(|_| ())?;
    let result = tokio::select! { biased;
        _ = Shutdown::cancelled(&mut stop) => Err(()),
        result = read_pages(&mut session, started) => result,
    };
    let closed = session.shutdown().await;
    // Cancellation also closes the adapter lanes; the session may observe that
    // closure before its stop signal. A complete catalog still requires reaping.
    if !matches!(closed.result, Ok(()) | Err(SessionTaskError::Closed))
        || closed.adapter.is_none_or(|exit| exit.result.is_err())
    {
        return Err(());
    }
    result
}

async fn next_event(
    session: &mut SessionHandle,
) -> Result<crate::provider_session_task::SessionEvent, ()> {
    let event = session.events.recv().await.ok_or(())?;
    // This catalog is deliberately ephemeral: consuming the entry settles it.
    if let Some(journal) = &event.journal {
        session
            .control
            .settled(journal.cursor)
            .await
            .map_err(|_| ())?;
    }
    Ok(event)
}

async fn catalog_drive<T>(
    session: &mut SessionHandle,
    future: impl std::future::Future<Output = Result<T, SessionTaskError>>,
    pending: &mut std::collections::VecDeque<crate::provider_session_task::SessionEvent>,
) -> Result<T, ()> {
    tokio::pin!(future);
    loop {
        tokio::select! { biased;
            result = &mut future => return result.map_err(|_|()),
            event = next_event(session) => {
                let event = event?;
                if !event.outcomes.is_empty() {
                    if pending.len() == crate::provider_session_task::MAX_UNSETTLED { return Err(()); }
                    pending.push_back(event);
                }
            }
        }
    }
}

async fn read_pages(session: &mut SessionHandle, started: Instant) -> Result<Value, ()> {
    while session.status.borrow().initialization != InitializationStatus::Ready {
        tokio::select! {
            result = session.status.changed() => result.map_err(|_| ())?,
            event = session.events.recv() => {
                let event = event.ok_or(())?;
                if let Some(journal) = &event.journal { session.control.settled(journal.cursor).await.map_err(|_| ())?; }
                if event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::GenerationClosed { .. })) { return Err(()); }
            }
        }
    }
    let mut pending = std::collections::VecDeque::new();
    let mut rows = Vec::new();
    let mut first_page = None;
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    let mut seen_models = std::collections::HashSet::new();
    let mut total_bytes = 0_usize;

    loop {
        if cursor.is_some() && seen_cursors.len() >= MAX_CATALOG_PAGES {
            return Err(());
        }
        let remaining = CATALOG_TIMEOUT.checked_sub(started.elapsed()).ok_or(())?;
        let timeout_ms = duration_ms(remaining);
        let params = match &cursor {
            Some(cursor) => json!({"limit": 100, "includeHidden": true, "cursor": cursor}),
            None => json!({"limit": 100, "includeHidden": true}),
        };
        let control = session.control.clone();
        let prepared = catalog_drive(
            session,
            control.prepare(RequestSpec {
                id: RequestId::String(format!("catalog-page-{}", seen_cursors.len())),
                operation_id: OperationId(format!("catalog-page-{}", seen_cursors.len())),
                method: Method::ModelList,
                params,
                thread_id: None,
                lightweight: true,
                timeout_ms,
            }),
            &mut pending,
        )
        .await?;
        let ticket = catalog_drive(session, control.submit(prepared), &mut pending).await?;
        let page = 'response: loop {
            let event = match pending.pop_front() {
                Some(event) => event,
                None => next_event(session).await?,
            };
            for outcome in event.outcomes {
                match outcome {
                    SessionOutcome::Resolved {
                        ticket: received,
                        outcome:
                            RequestOutcome::Reply {
                                body: ResponseBody::Result(value),
                                late: false,
                            },
                    } if received == ticket => break 'response value,
                    SessionOutcome::Resolved {
                        ticket: received, ..
                    } if received == ticket => return Err(()),
                    SessionOutcome::Write {
                        ticket: Some(received),
                        outcome: WriteOutcome::NotSent | WriteOutcome::Unknown,
                        ..
                    } if received == ticket => return Err(()),
                    SessionOutcome::GenerationClosed { .. } => return Err(()),
                    _ => {}
                }
            }
        };
        let page_bytes = serde_json::to_vec(&page).map_err(|_| ())?.len();
        if page_bytes > MAX_CATALOG_PAGE_BYTES {
            return Err(());
        }
        total_bytes = total_bytes.checked_add(page_bytes).ok_or(())?;
        if total_bytes > MAX_CATALOG_RESPONSE_BYTES {
            return Err(());
        }
        let page_object = page.as_object().ok_or(())?;
        let data = page_object
            .get("data")
            .and_then(Value::as_array)
            .ok_or(())?;
        if first_page.is_none() {
            first_page = Some(page_object.clone());
        }
        for row in data {
            let model = row
                .as_object()
                .and_then(|row| row.get("model"))
                .and_then(Value::as_str)
                .filter(|model| !model.trim().is_empty())
                .ok_or(())?;
            if !seen_models.insert(model.to_owned()) {
                return Err(());
            }
            rows.push(row.clone());
        }
        match page_object.get("nextCursor") {
            None | Some(Value::Null) => break,
            Some(Value::String(value))
                if !value.is_empty() && seen_cursors.insert(value.clone()) =>
            {
                cursor = Some(value.clone());
            }
            _ => return Err(()),
        }
    }

    let mut result = first_page.ok_or(())?;
    result.insert("data".to_owned(), Value::Array(rows));
    result.insert("nextCursor".to_owned(), Value::Null);
    Ok(Value::Object(result))
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use studio_runtime_process::{EventCursor, provider_seam::*};
    use studio_runtime_providers::Frame;
    use tokio::sync::{mpsc, watch};

    struct Script(Vec<Value>, usize);
    impl ProviderTaskFactory for Script {
        fn open(
            &self,
            handle: ProviderHandle,
            options: OpenOptions,
            shutdown: Shutdown,
        ) -> OpenFuture<'_> {
            let pages = self.0.clone();
            let flood = self.1;
            Box::pin(async move {
                let (commands, mut inbox) = mpsc::channel::<AdapterCommand>(LANE_CAPACITY);
                let (entries, journal) = mpsc::channel(LANE_CAPACITY);
                let (state, status) = watch::channel(AttachmentStatus {
                    handle,
                    open_attempted: true,
                    info: None,
                    initialization: InitializationStatus::AwaitingReply,
                });
                let mut stop = shutdown.subscribe();
                let task = tokio::spawn(async move {
                    let serve = async {
                        let mut write_index = 0;
                        while let Some(command) = inbox.recv().await {
                            match command {
                                AdapterCommand::Write {
                                    reply, observed, ..
                                } => {
                                    if write_index == 2 {
                                        let burst = async {
                                            for cursor in 1..=flood {
                                                entries
                                                    .send(JournalEntry {
                                                        cursor: EventCursor(cursor as u64),
                                                        generation: Generation(1),
                                                        received_at_ms: options.clock.now_ms(),
                                                        payload: JournalPayload::Other {
                                                            kind: "catalog notice".into(),
                                                            payload: json!({}),
                                                        },
                                                    })
                                                    .await
                                                    .expect("burst journal");
                                            }
                                        };
                                        tokio::pin!(burst);
                                        loop {
                                            tokio::select! {
                                                _ = &mut burst => break,
                                                command = inbox.recv() => match command.expect("burst acknowledgement") {
                                                    AdapterCommand::Ack {cursor,reply} => { reply.send(CommandReceipt::Acknowledged(cursor)).ok().expect("burst ACK"); },
                                                    _ => panic!("catalog must wait for the first write receipt"),
                                                }
                                            }
                                        }
                                    }
                                    let receipt = if write_index == 0 {
                                        CommandReceipt::CachedInitialize(json!({}))
                                    } else {
                                        CommandReceipt::Written {
                                            remote_id: None,
                                            duplicate: false,
                                        }
                                    };
                                    reply.send(receipt).ok().expect("receipt");
                                    observed.await.expect("receipt observed");
                                    if write_index >= 2 {
                                        let index = write_index - 2;
                                        if let Some(page) = pages.get(index) {
                                            entries
                                                .send(JournalEntry {
                                                    cursor: EventCursor(
                                                        flood as u64 + index as u64 + 1,
                                                    ),
                                                    generation: Generation(1),
                                                    received_at_ms: options.clock.now_ms(),
                                                    payload: JournalPayload::Frame(
                                                        Frame::Response {
                                                            id: RequestId::String(format!(
                                                                "catalog-page-{index}"
                                                            )),
                                                            body: ResponseBody::Result(
                                                                page.clone(),
                                                            ),
                                                            fields: Default::default(),
                                                        },
                                                    ),
                                                })
                                                .await
                                                .expect("entry accepted");
                                        }
                                    }
                                    write_index += 1;
                                }
                                AdapterCommand::Ack { cursor, reply } => {
                                    reply
                                        .send(CommandReceipt::Acknowledged(cursor))
                                        .ok()
                                        .expect("ack");
                                }
                            }
                        }
                    };
                    tokio::select! { _ = serve => {}, _ = stop.changed() => {} }
                    drop(state);
                    TaskExit {
                        result: Ok(()),
                        recovery_needed: None,
                    }
                });
                Ok(ProviderAttachment::from_task(
                    commands, journal, status, shutdown, task,
                ))
            })
        }
    }
    async fn scripted(pages: Vec<Value>) -> Result<Value, ()> {
        read_session(
            Arc::new(Script(pages, 0)),
            ProviderLaunchSpec::new(vec!["scripted".into()], BTreeMap::new(), "/var/tmp".into()),
            Instant::now(),
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn pages_preserve_metadata_and_hidden_models() {
        assert_eq!(
            scripted(vec![
                json!({"data":[{"model":"one"}],"nextCursor":"next","metadata":true}),
                json!({"data":[{"model":"two","hidden":true}],"nextCursor":null}),
            ])
            .await,
            Ok(
                json!({"data":[{"model":"one"},{"model":"two","hidden":true}],"nextCursor":null,"metadata":true})
            )
        );
    }

    #[tokio::test(start_paused = true)]
    async fn malformed_duplicate_and_cyclic_pages_are_refused() {
        for pages in [
            vec![json!({"data":[{}]})],
            vec![json!({"data":[{"model":""}]})],
            vec![json!({"data":[{"model":"same"},{"model":"same"}]})],
            vec![
                json!({"data":[],"nextCursor":"again"}),
                json!({"data":[],"nextCursor":"again"}),
            ],
            vec![json!({"data":[],"nextCursor":2})],
        ] {
            assert_eq!(scripted(pages).await, Err(()));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn missing_reply_uses_the_exchange_deadline() {
        let started = Instant::now();
        assert_eq!(scripted(vec![]).await, Err(()));
        assert_eq!(started.elapsed(), CATALOG_TIMEOUT);
    }

    #[tokio::test(start_paused = true)]
    async fn pagination_and_page_size_are_bounded() {
        let pages = (0..MAX_CATALOG_PAGES)
            .map(|index| json!({"data":[],"nextCursor":index.to_string()}))
            .collect();
        assert_eq!(scripted(pages).await, Err(()));
        assert_eq!(
            scripted(vec![
                json!({"data":[],"extra":"x".repeat(MAX_CATALOG_PAGE_BYTES)})
            ])
            .await,
            Err(())
        );
    }

    #[tokio::test(start_paused = true)]
    async fn cache_keeps_stale_pending_values_and_notifies_only_once() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = notifications.clone();
        let service = ModelCatalogService::with_change_callback(
            None,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }),
        );
        assert_eq!(
            service.read("account", "/home", "claude", false).await,
            CatalogRead::Unavailable
        );
        let key = ("account".to_owned(), "/home".to_owned(), "codex".to_owned());
        let value = json!({"data":[]});
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: Some(value.clone()),
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        assert_eq!(
            service.read("account", "/home", "codex", false).await,
            CatalogRead::Ready(value.clone())
        );
        assert!(store_result(&service.0, &key, 7, Ok(value.clone())));
        assert!(!store_result(&service.0, &key, 7, Ok(value.clone())));
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert_eq!(
            service.read("account", "/home", "codex", false).await,
            CatalogRead::Ready(value)
        );
    }
    #[tokio::test(start_paused = true)]
    async fn refresh_budget_cache_pruning_and_shutdown_are_bounded() {
        let service = ModelCatalogService::new(Some(
            "/var/tmp/cs-rust-account-worker/missing-provider".into(),
        ));
        let permits = service
            .0
            .slots
            .clone()
            .acquire_many_owned(MAX_REFRESHES as u32)
            .await
            .expect("catalog resource check");
        for account in 0..100 {
            assert_eq!(
                service
                    .read(&account.to_string(), "/var/tmp", "codex", false)
                    .await,
                CatalogRead::Pending
            );
        }
        assert!(
            service
                .0
                .cache
                .lock()
                .expect("catalog resource check")
                .is_empty()
        );
        assert!(
            service
                .0
                .tasks
                .lock()
                .expect("catalog resource check")
                .is_empty()
        );
        drop(permits);
        for account in 0..MAX_REFRESHES {
            assert_eq!(
                service
                    .read(&account.to_string(), "/var/tmp", "codex", false)
                    .await,
                CatalogRead::Pending
            );
        }
        assert_eq!(service.0.slots.available_permits(), 0);
        assert_eq!(
            service
                .0
                .tasks
                .lock()
                .expect("catalog resource check")
                .len(),
            MAX_REFRESHES
        );
        service.retain_accounts(&[json!({"id":"0","home":"/var/tmp","provider":"codex"})]);
        assert_eq!(
            service
                .0
                .cache
                .lock()
                .expect("catalog resource check")
                .len(),
            1
        );
        service.shutdown().await.expect("catalog resource check");
        assert_eq!(service.0.slots.available_permits(), MAX_REFRESHES);
        assert!(
            service
                .0
                .tasks
                .lock()
                .expect("catalog resource check")
                .is_empty()
        );
        assert_eq!(
            service.read("new", "/var/tmp", "codex", false).await,
            CatalogRead::Unavailable
        );
    }
    #[tokio::test(start_paused = true)]
    async fn catalog_drains_notifications_while_write_receipt_waits() {
        let page = json!({"data":[],"nextCursor":null});
        assert_eq!(
            read_session(
                Arc::new(Script(vec![page.clone()], LANE_CAPACITY * 4)),
                ProviderLaunchSpec::new(
                    vec!["scripted".into()],
                    BTreeMap::new(),
                    "/var/tmp".into()
                ),
                Instant::now()
            )
            .await,
            Ok(page)
        );
    }
}
