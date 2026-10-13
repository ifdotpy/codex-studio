//! Bounded, process-local model catalog reads for display.

use crate::provider_exchange;
use crate::provider_session_task::{
    RequestSpec, SessionHandle, SessionOutcome, SessionTaskError, WriteOutcome,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
#[cfg(test)]
use studio_runtime_process::provider_seam::ProviderTaskFactory;
#[cfg(test)]
use studio_runtime_process::{Generation, ProviderHandle, ProviderLaunchSpec};
use studio_runtime_process::{
    OperationId,
    provider_seam::{InitializationStatus, Shutdown},
};
use studio_runtime_providers::{Method, RequestId, RequestOutcome, ResponseBody};
use tokio::time::Instant;

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

/// Admission reads never expose the display cache's stale fallback. The caller
/// must retain account/home/provider identity and recheck expiry before freezing.
#[derive(Clone, Debug, PartialEq)]
pub enum AdmissionCatalogRead {
    Fresh {
        value: Value,
        account_key: String,
        home: String,
        provider: String,
        generation: u64,
        expires_at: std::time::Instant,
    },
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
    tasks: Mutex<Vec<tokio::task::JoinHandle<()>>>,
    stop: Shutdown,
}

/// A single HTTP-state-owned cache and its provider process configuration.
#[derive(Clone)]
pub struct ModelCatalogService(Arc<CatalogInner>);

impl ModelCatalogService {
    /// Observe only an already refreshed catalog. This never starts a provider;
    /// use `read` outside admission to request/coalesce the asynchronous refresh.
    #[must_use]
    pub fn read_for_admission(
        &self,
        account_key: &str,
        home: &str,
        provider: &str,
    ) -> AdmissionCatalogRead {
        let key = (account_key.to_owned(), home.to_owned(), provider.to_owned());
        if provider != "codex" {
            return AdmissionCatalogRead::Unavailable;
        }
        let Ok(cache) = self.0.cache.lock() else {
            return AdmissionCatalogRead::Unavailable;
        };
        let Some(entry) = cache.get(&key) else {
            return AdmissionCatalogRead::Pending;
        };
        if entry.pending {
            return AdmissionCatalogRead::Pending;
        }
        if entry.error {
            return AdmissionCatalogRead::Unavailable;
        }
        match (&entry.value, entry.expires_at) {
            (Some(value), Some(expires_at)) if expires_at > Instant::now() => {
                AdmissionCatalogRead::Fresh {
                    value: value.clone(),
                    account_key: key.0,
                    home: key.1,
                    provider: key.2,
                    generation: entry.generation,
                    expires_at: expires_at.into_std(),
                }
            }
            _ => AdmissionCatalogRead::Pending,
        }
    }

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
            let Some(permit) = provider_exchange::try_admit() else {
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
            let result = read_provider_catalog(
                permit,
                executable.as_deref(),
                &home,
                account != "default",
                stop,
            )
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
    admission: provider_exchange::ExchangeAdmission,
    executable: Option<&std::path::Path>,
    home: &str,
    isolated: bool,
    stop: Shutdown,
) -> Result<Value, ()> {
    provider_exchange::run_admitted(
        admission,
        executable,
        home,
        isolated,
        provider_exchange::ExchangeSpec {
            timeout: CATALOG_TIMEOUT,
            shutdown: stop,
            initialize_operation: OperationId("catalog-initialize".into()),
            initialize_params: json!({
                "clientInfo": {"name": "codex_studio_catalog", "version": "1.0.0"},
                "capabilities": {"experimentalApi": true}
            }),
        },
        |session, started| Box::pin(read_pages(session, started)),
    )
    .await
}

#[cfg(test)]
async fn read_session(
    factory: Arc<dyn ProviderTaskFactory>,
    launch: ProviderLaunchSpec,
    started: Instant,
) -> Result<Value, ()> {
    read_session_with_shutdown(factory, launch, started, Shutdown::default()).await
}
#[cfg(test)]
async fn read_session_with_shutdown(
    factory: Arc<dyn ProviderTaskFactory>,
    launch: ProviderLaunchSpec,
    started: Instant,
    shutdown: Shutdown,
) -> Result<Value, ()> {
    provider_exchange::run_with_factory(
        factory,
        launch,
        started,
        provider_exchange::ExchangeSpec {
            timeout: CATALOG_TIMEOUT,
            shutdown,
            initialize_operation: OperationId("catalog-initialize".into()),
            initialize_params: json!({
                "clientInfo": {"name": "codex_studio_catalog", "version": "1.0.0"},
                "capabilities": {"experimentalApi": true}
            }),
        },
        |session, started| Box::pin(read_pages(session, started)),
    )
    .await
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
    use std::collections::BTreeMap;
    use studio_runtime_process::ProviderLaunchSpec;
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
    async fn admission_never_exposes_cold_stale_pending_failed_or_foreign_catalogs() {
        use super::AdmissionCatalogRead;
        let service = ModelCatalogService::new(None);
        let key = (
            "account".to_owned(),
            "/disposable".to_owned(),
            "codex".to_owned(),
        );
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Pending
        );
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, "claude"),
            AdmissionCatalogRead::Unavailable
        );
        let value = json!({"data": [{"model": "one"}]});
        let expiry = Instant::now() + CATALOG_TTL;
        service.0.cache.lock().expect("cache").insert(
            key.clone(),
            CacheEntry {
                generation: 7,
                value: Some(value.clone()),
                expires_at: Some(expiry),
                error: false,
                pending: false,
            },
        );
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Fresh {
                value: value.clone(),
                account_key: key.0.clone(),
                home: key.1.clone(),
                provider: key.2.clone(),
                generation: 7,
                expires_at: expiry.into_std(),
            }
        );
        assert_eq!(
            service.read_for_admission("foreign", &key.1, &key.2),
            AdmissionCatalogRead::Pending
        );
        assert_eq!(
            service.read_for_admission(&key.0, "/changed-home", &key.2),
            AdmissionCatalogRead::Pending
        );
        tokio::time::advance(CATALOG_TTL).await;
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Pending
        );
        {
            let mut cache = service.0.cache.lock().expect("cache");
            let entry = cache.get_mut(&key).expect("entry");
            entry.expires_at = Some(Instant::now() + CATALOG_TTL);
            entry.pending = true;
        }
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Pending
        );
        assert!(store_result(&service.0, &key, 7, Err(())));
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, &key.2),
            AdmissionCatalogRead::Unavailable
        );
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
    async fn cache_pruning_and_shutdown_are_bounded() {
        let service = ModelCatalogService::new(None);
        for account in 0..100 {
            service
                .0
                .cache
                .lock()
                .expect("catalog resource check")
                .insert(
                    (account.to_string(), "/var/tmp".into(), "codex".into()),
                    CacheEntry {
                        generation: 1,
                        value: Some(json!({"models":[]})),
                        expires_at: Some(Instant::now() + CATALOG_TTL),
                        error: false,
                        pending: false,
                    },
                );
        }
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
