//! Bounded, process-local model catalog reads for display.

use crate::account_session_runtime::SessionOutcome as ReceiptOutcome;
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use studio_runtime_process::provider_seam::Shutdown;
use studio_runtime_schema::ProviderKind;
use studio_runtime_schema::model_catalog::{ModelCatalog, ProviderModel};
use studio_runtime_schema::shared_values::{
    MODEL_CATALOG_ADMISSION_WAIT_SECONDS, MODEL_CATALOG_CACHE_TTL_SECONDS,
    MODEL_CATALOG_ITEMS_PER_PAGE, MODEL_CATALOG_LAST_GOOD_MAX_AGE_SECONDS,
    MODEL_CATALOG_MAX_FRAME_BYTES, MODEL_CATALOG_MAX_PAGES,
    MODEL_CATALOG_MAX_RESPONSE_BYTES, MODEL_CATALOG_OPERATION_TIMEOUT_SECONDS,
};
use tokio::time::Instant;

const CATALOG_TTL: Duration = Duration::from_secs(MODEL_CATALOG_CACHE_TTL_SECONDS);
/// How long one failed provider refresh answers reads as unavailable before the
/// next read starts another refresh. Owner: this cache. Unit: seconds. Kept short
/// so a transient failure (a cold provider start under load, an idle-closed
/// session) does not hide the catalog for the full success TTL.
const CATALOG_FAILURE_RETRY: Duration = Duration::from_secs(5);
const LAST_GOOD_MAX_AGE: Duration = Duration::from_secs(MODEL_CATALOG_LAST_GOOD_MAX_AGE_SECONDS);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(MODEL_CATALOG_OPERATION_TIMEOUT_SECONDS);
const CATALOG_ADMISSION_WAIT: Duration = Duration::from_secs(MODEL_CATALOG_ADMISSION_WAIT_SECONDS);
const MAX_CATALOG_PAGE_BYTES: usize = MODEL_CATALOG_MAX_FRAME_BYTES;
const MAX_CATALOG_RESPONSE_BYTES: usize = MODEL_CATALOG_MAX_RESPONSE_BYTES;
const MAX_CATALOG_PAGES: usize = MODEL_CATALOG_MAX_PAGES;
pub type ChangeCallback =
    Arc<dyn Fn(&str, &str, ProviderKind, Option<ModelCatalog>) + Send + Sync + 'static>;

/// Dependency name reported when no fresh model catalog can be read for an
/// admission decision. The condition is transient: a later read can succeed.
pub const MODEL_CATALOG_DEPENDENCY: &str = "provider-session/model-list";

#[derive(Clone, Debug, thiserror::Error, PartialEq, Eq)]
pub enum ModelListError {
    #[error("Unknown account")]
    UnknownAccount,
    #[error("Account provider changed")]
    ProviderMismatch,
    #[error("Invalid model/list params")]
    InvalidParams,
    #[error("Model catalog is pending")]
    Pending,
    #[error("Model catalog is unavailable")]
    Unavailable,
}

// Account records can contain fields unrelated to catalog identity. A missing or
// non-string home has historically denoted the empty home.
#[derive(Deserialize)]
struct CatalogAccount {
    id: String,
    #[serde(default, deserialize_with = "catalog_home")]
    home: String,
    #[serde(deserialize_with = "catalog_provider")]
    provider: ProviderKind,
}

fn catalog_provider<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<ProviderKind, D::Error> {
    let provider = String::deserialize(deserializer)?;
    provider.parse().map_err(serde::de::Error::custom)
}

fn catalog_home<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Home {
        String(String),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Home::deserialize(deserializer)? {
        Home::String(home) => home,
        Home::Other(_) => String::new(),
    })
}

/// Shared catalog surface used by rename and the HTTP model route.
pub trait ModelListPort: Send + Sync {
    fn list<'a>(
        &'a self,
        account_key: &'a str,
        provider: ProviderKind,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<ModelCatalog, ModelListError>> + Send + 'a>>;
}

impl ModelListPort for ModelCatalogService {
    fn list<'a>(
        &'a self,
        account_key: &'a str,
        provider: ProviderKind,
        params: Value,
    ) -> Pin<Box<dyn Future<Output = Result<ModelCatalog, ModelListError>> + Send + 'a>> {
        Box::pin(self.list_models(account_key, provider, params))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub enum CatalogRead {
    Ready(ModelCatalog),
    Pending,
    Unavailable,
}

async fn read_account_session_catalog(
    session: Arc<dyn crate::account_session_runtime::AccountSessionPort>,
    account: String,
    _provider: ProviderKind,
    stop: Shutdown,
) -> Result<ModelCatalog, ()> {
    let mut shutdown = stop.subscribe();
    let mut rows = Vec::new();
    // A refresh is a new durable provider read. Page retries within this refresh
    // keep their identity; a later refresh must not replay an expired receipt.
    static REFRESH_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let refresh_id = format!(
        "{}-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ())?
            .as_nanos(),
        REFRESH_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    );
    let mut first_page = None;
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    let mut seen_models = std::collections::HashSet::new();
    let mut total_bytes = 0_usize;
    for page_index in 0..MAX_CATALOG_PAGES {
        let mut params = json!({
            "limit": MODEL_CATALOG_ITEMS_PER_PAGE,
            "includeHidden": true
        });
        if let Some(cursor) = &cursor {
            params
                .as_object_mut()
                .ok_or(())?
                .insert("cursor".into(), Value::String(cursor.clone()));
        }
        let session_clone = session.clone();
        let account_clone = account.clone();
        // An idle or not-yet-started account worker has no scope. Start it
        // (bounded) instead of reporting the catalog unavailable.
        let current_scope = match session.current_scope(&account) {
            Some(scope) => scope,
            None => tokio::select! {
                _ = Shutdown::cancelled(&mut shutdown) => return Err(()),
                result = tokio::time::timeout(CATALOG_TIMEOUT, session.ensure_session(&account)) => {
                    result.map_err(|_| ())?.map_err(|_| ())?
                }
            },
        };
        let operation = format!(
            "model-list:{account}:{refresh_id}:{page_index}:{}",
            cursor.as_deref().unwrap_or("first")
        );
        use crate::account_session_runtime::{ProviderRequest, ProviderResponse};
        let request = session_clone.provider_request(ProviderRequest {
            root_request_id: String::new(),
            admitted_request: None,
            account: account_clone,
            operation: operation.clone(),
            actor: None,
            deleted_actor_session: None,
            expected_scope: Some(current_scope.clone()),
            method: studio_runtime_providers::Method::ModelList,
            params,
            deadline: std::time::Instant::now() + CATALOG_TIMEOUT,
        });
        let response = tokio::select! {
            _ = Shutdown::cancelled(&mut shutdown) => return Err(()),
            result = tokio::time::timeout(CATALOG_TIMEOUT, request) => {
                let receipt = result.map_err(|_| ())?.map_err(|_| ())?;
                if receipt.scope != current_scope
                    || receipt.operation != operation
                {
                    return Err(());
                }
                let ReceiptOutcome::Written { response: ProviderResponse::Reply(value), .. } = receipt.outcome else { return Err(()); };
                value
            },
        };
        let bytes = serde_json::to_vec(&response).map_err(|_| ())?.len();
        if bytes > MAX_CATALOG_PAGE_BYTES {
            return Err(());
        }
        total_bytes = total_bytes.checked_add(bytes).ok_or(())?;
        if total_bytes > MAX_CATALOG_RESPONSE_BYTES {
            return Err(());
        }
        let mut page = ModelCatalog::parse(response).map_err(|error| {
            eprintln!("Invalid provider model catalog: {error}");
        })?;
        for row in &page.data {
            if !seen_models.insert(row.model.clone()) {
                return Err(());
            }
        }
        rows.append(&mut page.data);
        let next = page.next_cursor.clone().flatten();
        if first_page.is_none() {
            first_page = Some(page);
        }
        match next {
            None => {
                let mut result = first_page.ok_or(())?;
                result.data = rows;
                result.next_cursor = Some(None);
                return Ok(result);
            }
            Some(value) if !value.is_empty() && seen_cursors.insert(value.clone()) => {
                cursor = Some(value)
            }
            Some(_) => return Err(()),
        }
    }
    Err(())
}

/// Admission reads never expose the display cache's stale fallback. The caller
/// must retain account/home/provider identity and recheck expiry before freezing.
#[derive(Clone, Debug, PartialEq)]
pub enum AdmissionCatalogRead {
    Fresh {
        value: ModelCatalog,
        account_key: String,
        home: String,
        provider: ProviderKind,
        generation: u64,
        expires_at: std::time::Instant,
    },
    Pending,
    Unavailable,
}

struct CacheEntry {
    /// When `value` was last read successfully from the provider. Unlike
    /// `expires_at`, a failed refresh keeps it.
    read_at: Option<Instant>,
    generation: u64,
    value: Option<ModelCatalog>,
    expires_at: Option<Instant>,
    error: bool,
    pending: bool,
}

/// Cache entries and their generation sequence advance under the same lock.
#[derive(Default)]
struct CatalogState {
    entries: HashMap<(String, String, ProviderKind), CacheEntry>,
    next_generation: u64,
}

impl CatalogState {
    fn admission_read(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
        now: Instant,
    ) -> AdmissionCatalogRead {
        let key = (account_key.to_owned(), home.to_owned(), provider);
        let Some(entry) = self.entries.get(&key) else {
            return AdmissionCatalogRead::Pending;
        };
        if entry.pending {
            return AdmissionCatalogRead::Pending;
        }
        if entry.error {
            return AdmissionCatalogRead::Unavailable;
        }
        match (&entry.value, entry.expires_at) {
            (Some(value), Some(expires_at)) if expires_at > now => AdmissionCatalogRead::Fresh {
                value: value.clone(),
                account_key: key.0,
                home: key.1,
                provider: key.2,
                generation: entry.generation,
                expires_at: expires_at.into_std(),
            },
            _ => AdmissionCatalogRead::Pending,
        }
    }
}

struct CatalogInner {
    state: Mutex<CatalogState>,
    accounts: Mutex<HashMap<String, (String, ProviderKind)>>,
    accounts_initialized: AtomicBool,
    account_session: Mutex<Option<Arc<dyn crate::account_session_runtime::AccountSessionPort>>>,
    on_change: ChangeCallback,
    refresh_changed: tokio::sync::Notify,
    tasks: tokio::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    cleanup_error: Arc<Mutex<Option<String>>>,
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
        provider: ProviderKind,
    ) -> AdmissionCatalogRead {
        let Ok(cache) = self.0.state.lock() else {
            return AdmissionCatalogRead::Unavailable;
        };
        cache.admission_read(account_key, home, provider, Instant::now())
    }

    /// Observe exact account tuples under one lock and at one time after parallel reads.
    /// This never starts a provider or returns the display cache's stale fallback.
    #[must_use]
    pub fn read_for_admission_batch(
        &self,
        accounts: &[(&str, &str, ProviderKind)],
    ) -> Vec<AdmissionCatalogRead> {
        if accounts.is_empty() {
            return Vec::new();
        }
        // Match refresh admission's lock order. A retired binding cannot reuse
        // an old cache entry while retain_accounts waits for the cache lock.
        let Ok(profiles) = self.0.accounts.lock() else {
            return vec![AdmissionCatalogRead::Unavailable; accounts.len()];
        };
        let initialized = self.0.accounts_initialized.load(Ordering::Acquire);
        let Ok(cache) = self.0.state.lock() else {
            return vec![AdmissionCatalogRead::Unavailable; accounts.len()];
        };
        let now = Instant::now();
        accounts
            .iter()
            .map(|(account, home, provider)| {
                if initialized
                    && !profiles
                        .get(*account)
                        .is_some_and(|identity| identity.0 == *home && identity.1 == *provider)
                {
                    return AdmissionCatalogRead::Unavailable;
                }
                cache.admission_read(account, home, *provider, now)
            })
            .collect()
    }

    /// Start/coalesce the async refresh and wait for its published result at a
    /// point of need, outside the worker startup and synchronous store section.
    pub async fn wait_for_admission_catalog(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
    ) -> AdmissionCatalogRead {
        let deadline = Instant::now() + CATALOG_ADMISSION_WAIT;
        if tokio::time::timeout_at(deadline, self.read(account_key, home, provider, false))
            .await
            .is_err()
        {
            return AdmissionCatalogRead::Pending;
        }
        loop {
            let notified = self.0.refresh_changed.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            let result = self.read_for_admission(account_key, home, provider);
            if !matches!(result, AdmissionCatalogRead::Pending) {
                return result;
            }
            if tokio::time::timeout_at(deadline, notified).await.is_err() {
                return AdmissionCatalogRead::Pending;
            }
        }
    }

    /// Wait until admission can make progress on this catalog: start or join a
    /// refresh now and wait for a published result; after a cached failure,
    /// wait for its retry deadline (or any earlier published result).
    pub async fn refreshed(&self, account_key: &str, home: &str, provider: ProviderKind) {
        let notified = self.0.refresh_changed.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let _status = self.read(account_key, home, provider, false).await;
        if matches!(
            self.read_for_admission(account_key, home, provider),
            AdmissionCatalogRead::Fresh { .. }
        ) {
            return;
        }
        let key = (account_key.to_owned(), home.to_owned(), provider);
        let retry_at = self.0.state.lock().ok().and_then(|cache| {
            cache
                .entries
                .get(&key)
                .filter(|entry| entry.error && !entry.pending)
                .and_then(|entry| entry.expires_at)
        });
        match retry_at {
            Some(at) => {
                tokio::select! { () = &mut notified => {}, () = tokio::time::sleep_until(at) => {} }
            }
            None => notified.await,
        }
    }

    #[must_use]
    pub fn new() -> Self {
        Self::with_change_callback(Arc::new(|_, _, _, _| {}))
    }

    /// Construct the cache with an owner callback for completed account catalog changes.
    #[must_use]
    pub fn with_change_callback(on_change: ChangeCallback) -> Self {
        Self(Arc::new(CatalogInner {
            state: Mutex::new(CatalogState::default()),
            accounts: Mutex::new(HashMap::new()),
            accounts_initialized: AtomicBool::new(false),
            account_session: Mutex::new(None),
            on_change,
            refresh_changed: tokio::sync::Notify::new(),
            tasks: tokio::sync::Mutex::new(Vec::new()),
            cleanup_error: Arc::default(),
            stop: Shutdown::default(),
        }))
    }

    /// Display reads return stale data immediately while one refresh runs.
    pub async fn read(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
        retry: bool,
    ) -> CatalogRead {
        // Providers read the catalog over the account's retained session;
        // without that port there is no catalog source.
        let no_session = self
            .0
            .account_session
            .lock()
            .ok()
            .is_none_or(|session| session.is_none());
        if *self.0.stop.subscribe().borrow() || no_session {
            return CatalogRead::Unavailable;
        }

        let cache_key = (account_key.to_owned(), home.to_owned(), provider);
        // Own task-slot registration before marking the cache pending. If this
        // future is cancelled while waiting for the registry, no orphaned
        // pending entry can suppress future refresh attempts.
        loop {
            let finished = {
                let mut tasks = self.0.tasks.lock().await;
                if *self.0.stop.subscribe().borrow() {
                    return CatalogRead::Unavailable;
                }
                tasks
                    .iter()
                    .position(tokio::task::JoinHandle::is_finished)
                    .map(|index| tasks.swap_remove(index))
            };
            let Some(mut task) = finished else {
                break;
            };
            if let Err(error) = (&mut task).await {
                *self.0.cleanup_error.lock().expect("catalog cleanup report") =
                    Some(error.to_string());
            }
            drop(task);
        }
        // Acquire the task registry before reserving the cache entry. Once
        // Pending is published below, task registration is synchronous while
        // this guard is held, so cancellation cannot strand an unowned refresh.
        let mut tasks = self.0.tasks.lock().await;
        if *self.0.stop.subscribe().borrow() {
            return CatalogRead::Unavailable;
        }
        // Keep the retained identity stable while reserving a refresh. A route
        // that captured an old account snapshot must not restart its old catalog
        // after retain_accounts has rotated the account home or provider.
        let profiles = match self.0.accounts.lock() {
            Ok(profiles) => profiles,
            Err(_) => return CatalogRead::Unavailable,
        };
        if self.0.accounts_initialized.load(Ordering::Acquire)
            && !profiles
                .get(account_key)
                .is_some_and(|identity| identity.0 == home && identity.1 == provider)
        {
            return CatalogRead::Unavailable;
        }
        // Registry acquisition and completed-task cleanup can suspend. Sample
        // freshness only after those waits so an entry cannot expire while this
        // read is queued and still be returned as fresh.
        let now = Instant::now();
        let (generation, stale) = {
            let Ok(mut cache) = self.0.state.lock() else {
                return CatalogRead::Unavailable;
            };
            if let Some(entry) = cache.entries.get_mut(&cache_key) {
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
            let stale = cache
                .entries
                .get(&cache_key)
                .and_then(|entry| entry.value.clone());
            let last_read = cache.entries.get(&cache_key).and_then(|entry| entry.read_at);
            // A changed home/provider replaces the same account's former entry.
            cache
                .entries
                .retain(|key, _| key.0 != account_key || *key == cache_key);
            cache.next_generation = cache.next_generation.saturating_add(1);
            let generation = cache.next_generation;
            cache.entries.insert(
                cache_key.clone(),
                CacheEntry {
                    read_at: last_read,
                    generation,
                    value: stale.clone(),
                    expires_at: None,
                    error: false,
                    pending: true,
                },
            );
            (generation, stale)
        };

        let service = Arc::downgrade(&self.0);
        let account_session = self
            .0
            .account_session
            .lock()
            .ok()
            .and_then(|session| session.clone());
        let stop = self.0.stop.clone();
        let account = account_key.to_owned();
        tasks.push(tokio::spawn(async move {
            let result = match account_session {
                Some(session) => {
                    read_account_session_catalog(session, account.clone(), provider, stop).await
                }
                None => Err(()),
            };
            if let Some(inner) = service.upgrade() {
                store_result(&inner, &cache_key, generation, result);
            }
        }));
        stale.map_or(CatalogRead::Pending, CatalogRead::Ready)
    }

    /// Wait for a cold display read to publish, while returning stale display
    /// data immediately when the cache already has a value. A failed refresh
    /// serves the last known catalog for display; admission never does.
    pub async fn read_until_ready(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
        retry: bool,
    ) -> CatalogRead {
        match self
            .wait_until_ready(account_key, home, provider, retry)
            .await
        {
            CatalogRead::Unavailable => self
                .last_known(account_key, home, provider)
                .map_or(CatalogRead::Unavailable, CatalogRead::Ready),
            read @ CatalogRead::Ready(_) | read @ CatalogRead::Pending => read,
        }
    }

    /// The last successfully read catalog of this account, when it is younger
    /// than `MODEL_CATALOG_LAST_GOOD_MAX_AGE_SECONDS`. Worker creation validates a
    /// model against it when a fresh read fails or times out. Never starts a read.
    #[must_use]
    pub fn last_good(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
    ) -> Option<ModelCatalog> {
        if !self.account_identity_is_current(account_key, home, provider) {
            return None;
        }
        let key = (account_key.to_owned(), home.to_owned(), provider);
        let cache = self.0.state.lock().ok()?;
        let entry = cache.entries.get(&key)?;
        let age = Instant::now().checked_duration_since(entry.read_at?)?;
        (age <= LAST_GOOD_MAX_AGE).then(|| entry.value.clone())?
    }

    fn last_known(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
    ) -> Option<ModelCatalog> {
        if !self.account_identity_is_current(account_key, home, provider) {
            return None;
        }
        let key = (account_key.to_owned(), home.to_owned(), provider);
        self.0.state.lock().ok()?.entries.get(&key)?.value.clone()
    }

    async fn wait_until_ready(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
        retry: bool,
    ) -> CatalogRead {
        let deadline = Instant::now() + CATALOG_TIMEOUT;
        match tokio::time::timeout_at(deadline, self.read(account_key, home, provider, retry)).await
        {
            Ok(CatalogRead::Ready(value)) => {
                return if self.account_identity_is_current(account_key, home, provider) {
                    CatalogRead::Ready(value)
                } else {
                    CatalogRead::Unavailable
                };
            }
            Ok(CatalogRead::Unavailable) => return CatalogRead::Unavailable,
            Ok(CatalogRead::Pending) => {}
            Err(_) => return CatalogRead::Pending,
        }

        loop {
            let changed = self.0.refresh_changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            match tokio::time::timeout_at(deadline, self.read(account_key, home, provider, false))
                .await
            {
                Ok(CatalogRead::Ready(value)) => {
                    return if self.account_identity_is_current(account_key, home, provider) {
                        CatalogRead::Ready(value)
                    } else {
                        CatalogRead::Unavailable
                    };
                }
                Ok(CatalogRead::Unavailable) => return CatalogRead::Unavailable,
                Ok(CatalogRead::Pending) => {}
                Err(_) => return CatalogRead::Pending,
            }
            if tokio::time::timeout_at(deadline, changed).await.is_err() {
                return CatalogRead::Pending;
            }
        }
    }

    fn account_identity_is_current(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
    ) -> bool {
        if !self.0.accounts_initialized.load(Ordering::Acquire) {
            return true;
        }
        self.0.accounts.lock().ok().is_some_and(|profiles| {
            profiles
                .get(account_key)
                .is_some_and(|identity| identity.0 == home && identity.1 == provider)
        })
    }

    /// Start or join a catalog refresh and wait for its completion notification.
    /// Admission paths use this before durable provider effects so they never
    /// create a child from stale or pending model metadata.
    pub async fn read_until_fresh(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
    ) -> CatalogRead {
        let wait_for_result = async {
            loop {
                if let AdmissionCatalogRead::Fresh { value, .. } =
                    self.read_for_admission(account_key, home, provider)
                {
                    return CatalogRead::Ready(value);
                }
                let changed = self.0.refresh_changed.notified();
                tokio::pin!(changed);
                changed.as_mut().enable();
                match self.read(account_key, home, provider, false).await {
                    CatalogRead::Unavailable => return CatalogRead::Unavailable,
                    CatalogRead::Pending | CatalogRead::Ready(_) => {
                        if let AdmissionCatalogRead::Fresh { value, .. } =
                            self.read_for_admission(account_key, home, provider)
                        {
                            return CatalogRead::Ready(value);
                        }
                        changed.await;
                    }
                }
            }
        };
        tokio::time::timeout(CATALOG_TIMEOUT, wait_for_result)
            .await
            .unwrap_or(CatalogRead::Pending)
    }
    pub fn retain_accounts(&self, accounts: &[Value]) {
        let accounts: Vec<CatalogAccount> = accounts
            .iter()
            .filter(|account| account.is_object())
            .filter_map(|account| CatalogAccount::deserialize(account).ok())
            .collect();
        self.retain_catalog_accounts(&accounts);
    }

    pub fn retain_public_accounts(&self, accounts: &[crate::account_policy::PublicAccount]) {
        let accounts: Vec<CatalogAccount> = accounts
            .iter()
            .filter_map(|account| {
                Some(CatalogAccount {
                    id: account.id.as_str()?.to_owned(),
                    home: account.home.as_str().unwrap_or_default().to_owned(),
                    provider: account.provider.as_str()?.parse().ok()?,
                })
            })
            .collect();
        self.retain_catalog_accounts(&accounts);
    }

    fn retain_catalog_accounts(&self, accounts: &[CatalogAccount]) {
        let mut retired = Vec::new();
        if let Ok(mut profiles) = self.0.accounts.lock() {
            let retained = accounts
                .iter()
                .map(|account| (account.id.clone(), (account.home.clone(), account.provider)))
                .collect::<HashMap<_, _>>();
            retired.extend(
                profiles
                    .iter()
                    .filter(|&(id, (home, provider))| {
                        retained.get(id) != Some(&(home.clone(), *provider))
                    })
                    .map(|(id, (home, provider))| (id.clone(), home.clone(), *provider)),
            );
            profiles.clear();
            profiles.extend(retained);
            self.0.accounts_initialized.store(true, Ordering::Release);
        }
        self.0
            .state
            .lock()
            .expect("catalog cache")
            .entries
            .retain(|key, _| {
                accounts.iter().any(|account| {
                    account.id == key.0 && account.home == key.1 && account.provider == key.2
                })
            });
        self.0.refresh_changed.notify_waiters();
        for (account, home, provider) in retired {
            (self.0.on_change)(&account, &home, provider, None);
        }
    }

    /// Return the registered provider/home identity for transfer validation.
    #[must_use]
    pub fn account_profile(&self, account: &str) -> Option<(String, ProviderKind)> {
        self.0.accounts.lock().ok()?.get(account).cloned()
    }
    /// Attach the account registry's retained provider session. Model reads
    /// share the live account connection instead of launching a second provider.
    pub fn set_account_session(
        &self,
        session: Option<Arc<dyn crate::account_session_runtime::AccountSessionPort>>,
    ) {
        if let Ok(mut current) = self.0.account_session.lock() {
            *current = session;
        }
    }

    #[cfg(test)]
    pub(crate) fn seed_test_admission_catalog(
        &self,
        account_key: &str,
        home: &str,
        provider: ProviderKind,
        value: Value,
    ) {
        self.0.state.lock().expect("catalog cache").entries.insert(
            (account_key.to_owned(), home.to_owned(), provider),
            CacheEntry {
                read_at: Some(Instant::now()),
                generation: 1,
                value: Some(ModelCatalog::parse(value).expect("valid fixture catalog")),
                expires_at: Some(Instant::now() + Duration::from_secs(30)),
                error: false,
                pending: false,
            },
        );
    }

    pub fn model_list_port(&self) -> Arc<dyn ModelListPort> {
        Arc::new(self.clone())
    }

    async fn list_models(
        &self,
        account: &str,
        provider: ProviderKind,
        params: Value,
    ) -> Result<ModelCatalog, ModelListError> {
        if params != json!({"limit": MODEL_CATALOG_ITEMS_PER_PAGE}) {
            return Err(ModelListError::InvalidParams);
        }
        let (home, actual_provider) = self
            .0
            .accounts
            .lock()
            .map_err(|_| ModelListError::Unavailable)?
            .get(account)
            .cloned()
            .ok_or(ModelListError::UnknownAccount)?;
        if actual_provider != provider {
            return Err(ModelListError::ProviderMismatch);
        }
        match self.read(account, &home, provider, false).await {
            CatalogRead::Ready(value) => Ok(value),
            CatalogRead::Pending => Err(ModelListError::Pending),
            CatalogRead::Unavailable => Err(ModelListError::Unavailable),
        }
    }
    pub async fn shutdown(&self) -> std::io::Result<()> {
        self.0.stop.cancel();
        // Handles remain registry-owned while awaiting: cancelling this drain
        // cannot detach unfinished cleanup, and the next shutdown resumes it.
        let mut tasks = self.0.tasks.lock().await;
        while let Some(task) = tasks.last_mut() {
            if let Err(error) = task.await {
                *self.0.cleanup_error.lock().expect("catalog cleanup report") =
                    Some(error.to_string());
            }
            drop(tasks.pop());
        }
        match self
            .0
            .cleanup_error
            .lock()
            .expect("catalog cleanup report")
            .as_ref()
        {
            Some(error) => Err(std::io::Error::other(error.clone())),
            None => Ok(()),
        }
    }
}

impl Default for ModelCatalogService {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for CatalogInner {
    fn drop(&mut self) {
        self.stop.cancel();
    }
}

fn store_result(
    inner: &CatalogInner,
    key: &(String, String, ProviderKind),
    generation: u64,
    result: Result<ModelCatalog, ()>,
) -> bool {
    let stored = if let Ok(mut cache) = inner.state.lock()
        && let Some(entry) = cache.entries.get_mut(key)
        && entry.generation == generation
        && entry.pending
    {
        entry.pending = false;
        match result {
            Ok(value) => {
                entry.expires_at = Some(Instant::now() + CATALOG_TTL);
                entry.value = Some(value);
                entry.read_at = Some(Instant::now());
                entry.error = false;
            }
            Err(()) => {
                entry.expires_at = Some(Instant::now() + CATALOG_FAILURE_RETRY);
                entry.error = true;
            }
        }
        true
    } else {
        false
    };
    if stored {
        let value = inner
            .state
            .lock()
            .ok()
            .and_then(|cache| cache.entries.get(key).and_then(|entry| entry.value.clone()));
        (inner.on_change)(&key.0, &key.1, key.2, value);
        inner.refresh_changed.notify_waiters();
    }
    stored
}

impl From<&ProviderModel> for crate::submit_gates::ModelCapabilities {
    fn from(row: &ProviderModel) -> Self {
        Self {
            model: row.model.clone(),
            resolved_model: row.resolved_model.clone().flatten(),
            is_default: row.is_default.unwrap_or(false),
            hidden: row.hidden.unwrap_or(false),
            access_programs: row.cyber_programs().map(<[String]>::to_vec),
            supported_efforts: row
                .supported_reasoning_efforts
                .iter()
                .flatten()
                .map(|effort| effort.reasoning_effort.clone())
                .collect(),
            default_effort: row.default_reasoning_effort.clone().flatten(),
            priority: row
                .service_tiers
                .iter()
                .flatten()
                .any(|tier| tier.id == "priority"),
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    fn typed(value: serde_json::Value) -> studio_runtime_schema::model_catalog::ModelCatalog {
        studio_runtime_schema::model_catalog::ModelCatalog::parse(value)
            .expect("valid test catalog")
    }

    use super::*;

    use std::sync::Mutex as StdMutex;

    #[test]
    fn typed_cache_and_admission_signatures() {
        const _: fn(&ProviderModel) -> crate::submit_gates::ModelCapabilities =
            |model| model.into();
        let service = ModelCatalogService::new();
        if let AdmissionCatalogRead::Fresh { value, .. } =
            service.read_for_admission("account", "/home", ProviderKind::Codex)
        {
            let _: ModelCatalog = value;
        }
        let entry = CacheEntry {
        read_at: None,
            generation: 1,
            value: None,
            expires_at: None,
            error: false,
            pending: false,
        };
        let _: Option<ModelCatalog> = entry.value;
    }

    /// Retained-session fake that answers each `model/list` with the next page.
    /// A gated fake holds every reply until `release` grants it.
    pub(crate) struct AccountSessionPages {
        pages: StdMutex<Vec<Value>>,
        params: StdMutex<Vec<Value>>,
        operations: StdMutex<Vec<String>>,
        gate: tokio::sync::Semaphore,
    }

    impl AccountSessionPages {
        pub(crate) fn new(pages: Vec<Value>) -> Self {
            Self::with_permits(pages, tokio::sync::Semaphore::MAX_PERMITS)
        }
        pub(crate) fn gated(pages: Vec<Value>) -> Self {
            Self::with_permits(pages, 0)
        }
        fn with_permits(pages: Vec<Value>, permits: usize) -> Self {
            Self {
                pages: StdMutex::new(pages),
                params: StdMutex::new(Vec::new()),
                operations: StdMutex::new(Vec::new()),
                gate: tokio::sync::Semaphore::new(permits),
            }
        }
        pub(crate) fn requests(&self) -> usize {
            self.operations.lock().expect("operations").len()
        }
        pub(crate) fn release(&self) {
            self.gate.add_permits(1);
        }
    }

    /// Serve `service`'s catalog refreshes from `pages` over a fake retained session.
    pub(crate) fn serve_pages(
        service: &ModelCatalogService,
        pages: AccountSessionPages,
    ) -> Arc<AccountSessionPages> {
        let session = Arc::new(pages);
        service.set_account_session(Some(session.clone()));
        session
    }

    async fn scripted(pages: Vec<Value>) -> Result<Value, ()> {
        read_account_session_catalog(
            Arc::new(AccountSessionPages::new(pages)),
            "account".into(),
            ProviderKind::Codex,
            Shutdown::default(),
        )
        .await
        .map(|catalog| serde_json::to_value(catalog).expect("catalog serializes"))
    }
    impl crate::account_session_runtime::AccountSessionPort for AccountSessionPages {
        fn current_scope(
            &self,
            account: &str,
        ) -> Option<crate::account_session_runtime::ConnectionScope> {
            Some(crate::account_session_runtime::ConnectionScope {
                account: account.to_owned(),
                connection: "claude-connection".into(),
                generation: 4,
            })
        }

        fn provider_request<'a>(
            &'a self,
            request: crate::account_session_runtime::ProviderRequest,
        ) -> crate::account_session_runtime::AccountSessionFuture<
            'a,
            crate::account_session_runtime::SessionReceipt,
        > {
            Box::pin(async move {
                use crate::account_session_runtime::SessionOutcome as ReceiptOutcome;
                use crate::account_session_runtime::{ProviderResponse, SessionReceipt};
                self.params.lock().expect("params").push(request.params);
                self.operations
                    .lock()
                    .expect("operations")
                    .push(request.operation.clone());
                let _reply = self.gate.acquire().await.expect("reply gate");
                if self.pages.lock().expect("pages").is_empty() {
                    return Err(
                        crate::account_session_runtime::AccountSessionError::InvalidResponse(
                            "/".into(),
                        ),
                    );
                }
                let scope = self.current_scope(&request.account).expect("test scope");
                Ok(SessionReceipt {
                    scope,
                    operation: request.operation,
                    replayed: false,
                    outcome: ReceiptOutcome::Written {
                        ticket: studio_runtime_providers::RequestTicket {
                            id: studio_runtime_providers::RequestId::String("catalog-test".into()),
                            generation: 4,
                            method: studio_runtime_providers::Method::ModelList,
                        },
                        response: ProviderResponse::Reply(
                            self.pages.lock().expect("pages").remove(0),
                        ),
                    },
                })
            })
        }
    }

    #[tokio::test]
    async fn claude_catalog_uses_retained_session_and_paginates_python_params() {
        let session = Arc::new(AccountSessionPages::new(vec![
            json!({"data":[{"model":"one"}],"nextCursor":"next","metadata":true}),
            json!({"data":[{"model":"two"}],"nextCursor":null}),
        ]));
        let result = read_account_session_catalog(
            session.clone(),
            "account".into(),
            ProviderKind::Claude,
            Shutdown::default(),
        )
        .await
        .expect("catalog pages");
        assert_eq!(
            serde_json::to_value(result).expect("catalog serializes"),
            json!({
                "data":[{"model":"one"},{"model":"two"}],
                "nextCursor":null,
                "metadata":true
            })
        );
        assert_eq!(
            session.params.lock().expect("params").as_slice(),
            &[
                json!({"limit":MODEL_CATALOG_ITEMS_PER_PAGE,"includeHidden":true}),
                json!({"limit":MODEL_CATALOG_ITEMS_PER_PAGE,"includeHidden":true,"cursor":"next"}),
            ]
        );
    }

    /// Idle worker: no scope until `ensure_session` starts it.
    struct LazySession {
        inner: AccountSessionPages,
        started: std::sync::atomic::AtomicBool,
        ensure_fails: bool,
    }
    impl crate::account_session_runtime::AccountSessionPort for LazySession {
        fn current_scope(
            &self,
            account: &str,
        ) -> Option<crate::account_session_runtime::ConnectionScope> {
            if self.started.load(Ordering::SeqCst) {
                self.inner.current_scope(account)
            } else {
                None
            }
        }
        fn ensure_session<'a>(
            &'a self,
            account: &'a str,
        ) -> crate::account_session_runtime::AccountSessionFuture<
            'a,
            crate::account_session_runtime::ConnectionScope,
        > {
            Box::pin(async move {
                if self.ensure_fails {
                    return Err(crate::account_session_runtime::AccountSessionError::Unknown);
                }
                self.started.store(true, Ordering::SeqCst);
                Ok(self.inner.current_scope(account).expect("scope"))
            })
        }
        fn provider_request<'a>(
            &'a self,
            request: crate::account_session_runtime::ProviderRequest,
        ) -> crate::account_session_runtime::AccountSessionFuture<
            'a,
            crate::account_session_runtime::SessionReceipt,
        > {
            self.inner.provider_request(request)
        }
    }

    fn lazy_session(ensure_fails: bool) -> Arc<LazySession> {
        Arc::new(LazySession {
            inner: AccountSessionPages::new(vec![json!({"data":[{"model":"gpt-6-luna"}]})]),
            started: std::sync::atomic::AtomicBool::new(false),
            ensure_fails,
        })
    }

    #[tokio::test]
    async fn catalog_refresh_starts_an_idle_account_worker() {
        let session = lazy_session(false);
        let result = read_account_session_catalog(
            session.clone(),
            "codex2".into(),
            ProviderKind::Codex,
            Shutdown::default(),
        )
        .await
        .expect("catalog after lazy start");
        assert_eq!(result.data[0].model, "gpt-6-luna");
        assert!(session.started.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn catalog_refresh_reports_unavailable_when_worker_cannot_start() {
        let session = lazy_session(true);
        assert!(
            read_account_session_catalog(
                session,
                "codex2".into(),
                ProviderKind::Codex,
                Shutdown::default(),
            )
            .await
            .is_err()
        );
    }

    #[tokio::test]
    async fn each_catalog_refresh_has_a_fresh_durable_operation_identity() {
        let session = Arc::new(AccountSessionPages::new(vec![
            json!({"data":[{"model":"one"}]}),
            json!({"data":[{"model":"one"}]}),
        ]));
        for _ in 0..2 {
            read_account_session_catalog(
                session.clone(),
                "account".into(),
                ProviderKind::Claude,
                Shutdown::default(),
            )
            .await
            .expect("catalog refresh");
        }
        let operations = session.operations.lock().expect("operations");
        assert_eq!(operations.len(), 2);
        assert_ne!(operations[0], operations[1]);
        assert!(
            operations
                .iter()
                .all(|operation| operation.starts_with("model-list:account:"))
        );
    }

    #[tokio::test]
    async fn shared_model_list_checks_request_and_account_identity() {
        let service = ModelCatalogService::new();
        service.retain_accounts(&[json!({
            "id":"claude-account",
            "home":"/home/claude",
            "provider":"claude"
        })]);
        let port = service.model_list_port();
        assert_eq!(
            port.list(
                "missing",
                ProviderKind::Claude,
                json!({"limit": MODEL_CATALOG_ITEMS_PER_PAGE})
            )
            .await,
            Err(ModelListError::UnknownAccount)
        );
        assert_eq!(
            port.list(
                "claude-account",
                ProviderKind::Codex,
                json!({"limit": MODEL_CATALOG_ITEMS_PER_PAGE})
            )
            .await,
            Err(ModelListError::ProviderMismatch)
        );
        assert_eq!(
            port.list(
                "claude-account",
                ProviderKind::Claude,
                json!({"cursor":"caller-owned"})
            )
            .await,
            Err(ModelListError::InvalidParams)
        );
    }

    #[tokio::test]
    async fn model_list_params_keep_exact_object_contract() {
        let service = ModelCatalogService::new();
        let port = service.model_list_port();
        for params in [
            json!([MODEL_CATALOG_ITEMS_PER_PAGE]),
            json!({"limit": 100.0}),
            json!({"limit": 100, "extra": true}),
            json!({"limit": null}),
        ] {
            assert_eq!(
                port.list("missing", ProviderKind::Codex, params).await,
                Err(ModelListError::InvalidParams)
            );
        }
    }

    #[test]
    fn retained_account_projection_preserves_home_and_identity_rules() {
        let service = ModelCatalogService::new();
        service.retain_accounts(&[
            json!({"id":"default-home", "home":null, "provider":"codex"}),
            json!({"id":"wrong-home", "home":7, "provider":"claude"}),
            json!({"id":"valid", "home":"/home", "provider":"codex", "extra":true}),
            json!({"id":"bad", "provider":null}),
            json!({"id":"object-provider", "provider":{"codex":null}}),
            json!(["array", "/home", "codex"]),
        ]);
        assert_eq!(
            service.account_profile("default-home"),
            Some((String::new(), ProviderKind::Codex))
        );
        assert_eq!(
            service.account_profile("wrong-home"),
            Some((String::new(), ProviderKind::Claude))
        );
        assert_eq!(
            service.account_profile("valid"),
            Some(("/home".into(), ProviderKind::Codex))
        );
        assert_eq!(service.account_profile("bad"), None);
        assert_eq!(service.account_profile("object-provider"), None);
        assert_eq!(service.account_profile("array"), None);
    }

    pub(crate) fn expire_fixture(service: &ModelCatalogService) {
        for entry in service.0.state.lock().expect("cache").entries.values_mut() {
            entry.expires_at = Some(Instant::now());
        }
    }
    pub(crate) fn publish_fixture(
        service: &ModelCatalogService,
        account: &str,
        home: &str,
        value: Value,
    ) {
        let key = (account.to_owned(), home.to_owned(), ProviderKind::Codex);
        service.0.state.lock().expect("cache").entries.insert(
            key.clone(),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        assert!(store_result(&service.0, &key, 7, Ok(typed(value))));
    }

    #[tokio::test]
    async fn read_until_fresh_waits_for_catalog_change_notification() {
        for provider in [ProviderKind::Codex, ProviderKind::Claude] {
            let service = ModelCatalogService::new();
            let _pages = serve_pages(&service, AccountSessionPages::gated(Vec::new()));
            let key = ("account".to_owned(), "/home".to_owned(), provider);
            service.0.state.lock().expect("cache").entries.insert(
                key.clone(),
                CacheEntry {
                    read_at: None,
                    generation: 7,
                    value: Some(typed(json!({"data":[{"model":"stale"}]}))),
                    expires_at: None,
                    error: false,
                    pending: true,
                },
            );
            let waiting = service.clone();
            let read = tokio::spawn(async move {
                waiting.read_until_fresh("account", "/home", provider).await
            });
            tokio::task::yield_now().await;
            let value = json!({"data":[{"model":"fixture"}]});
            assert!(store_result(&service.0, &key, 7, Ok(typed(value.clone()))));
            assert_eq!(
                read.await.expect("catalog waiter"),
                CatalogRead::Ready(typed(value))
            );
            service.shutdown().await.expect("catalog task shutdown");
        }
    }

    #[tokio::test]
    async fn cold_display_read_waits_for_shared_refresh() {
        let service = ModelCatalogService::new();
        let pages = serve_pages(
            &service,
            AccountSessionPages::new(vec![json!({"data":[{"model":"fixture"}]})]),
        );

        let (first, second) = tokio::join!(
            service.read_until_ready("account", "/home", ProviderKind::Codex, false),
            service.read_until_ready("account", "/home", ProviderKind::Codex, false),
        );
        let ready = CatalogRead::Ready(typed(json!({
            "data":[{"model":"fixture"}],
            "nextCursor":null
        })));
        assert_eq!(first, ready);
        assert_eq!(second, ready);
        assert_eq!(pages.requests(), 1);
        service.shutdown().await.expect("catalog task shutdown");
    }

    #[tokio::test]
    async fn cold_display_read_reports_failed_refresh_as_unavailable() {
        let service = ModelCatalogService::new();
        let key = (
            "account".to_owned(),
            "/home".to_owned(),
            ProviderKind::Codex,
        );
        service.0.state.lock().expect("cache").entries.insert(
            key.clone(),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        let waiting = service.clone();
        let read = tokio::spawn(async move {
            waiting
                .read_until_ready("account", "/home", ProviderKind::Codex, false)
                .await
        });
        tokio::task::yield_now().await;
        assert!(store_result(&service.0, &key, 7, Err(())));
        assert_eq!(
            read.await.expect("catalog waiter"),
            CatalogRead::Unavailable
        );
    }

    #[tokio::test]
    async fn failed_refresh_serves_last_known_display_catalog_but_not_admission() {
        let service = ModelCatalogService::new();
        let key = (
            "account".to_owned(),
            "/home".to_owned(),
            ProviderKind::Codex,
        );
        let known = json!({"data":[{"model":"known"}],"nextCursor":null});
        service.0.state.lock().expect("cache").entries.insert(
            key.clone(),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: Some(typed(known.clone())),
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        assert!(store_result(&service.0, &key, 7, Err(())));

        assert_eq!(
            service.read_for_admission(&key.0, &key.1, key.2),
            super::AdmissionCatalogRead::Unavailable
        );
        assert_eq!(
            service
                .read("account", "/home", ProviderKind::Codex, false)
                .await,
            CatalogRead::Unavailable
        );
        assert_eq!(
            service
                .read_until_ready("account", "/home", ProviderKind::Codex, false)
                .await,
            CatalogRead::Ready(typed(known))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn failed_refresh_is_retried_after_the_failure_backoff_not_the_ttl() {
        assert!(CATALOG_FAILURE_RETRY < CATALOG_TTL);
        let service = ModelCatalogService::new();
        let key = (
            "account".to_owned(),
            "/home".to_owned(),
            ProviderKind::Codex,
        );
        service.0.state.lock().expect("cache").entries.insert(
            key.clone(),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        assert!(store_result(&service.0, &key, 7, Err(())));
        let pages = serve_pages(
            &service,
            AccountSessionPages::new(vec![json!({"data":[{"model":"fixture"}]})]),
        );

        assert_eq!(
            service
                .read("account", "/home", ProviderKind::Codex, false)
                .await,
            CatalogRead::Unavailable
        );
        assert_eq!(pages.requests(), 0);

        tokio::time::advance(CATALOG_FAILURE_RETRY).await;
        assert_eq!(
            service
                .read_until_ready("account", "/home", ProviderKind::Codex, false)
                .await,
            CatalogRead::Ready(typed(
                json!({"data":[{"model":"fixture"}],"nextCursor":null})
            ))
        );
        assert_eq!(pages.requests(), 1);
        service.shutdown().await.expect("catalog task shutdown");
    }

    #[tokio::test(start_paused = true)]
    async fn cold_display_read_keeps_its_bounded_pending_result() {
        let service = ModelCatalogService::new();
        let _pages = serve_pages(&service, AccountSessionPages::gated(Vec::new()));
        service.0.state.lock().expect("cache").entries.insert(
            (
                "account".to_owned(),
                "/home".to_owned(),
                ProviderKind::Codex,
            ),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );
        let waiting = service.clone();
        let read = tokio::spawn(async move {
            waiting
                .read_until_ready("account", "/home", ProviderKind::Codex, false)
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(CATALOG_TIMEOUT).await;
        assert_eq!(read.await.expect("catalog waiter"), CatalogRead::Pending);
    }

    #[tokio::test]
    async fn stale_display_value_returns_without_waiting_for_refresh() {
        let service = ModelCatalogService::new();
        service.0.state.lock().expect("cache").entries.insert(
            (
                "account".to_owned(),
                "/home".to_owned(),
                ProviderKind::Codex,
            ),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: Some(typed(json!({"data":[{"model":"stale"}]}))),
                expires_at: None,
                error: false,
                pending: true,
            },
        );

        assert_eq!(
            service
                .read_until_ready("account", "/home", ProviderKind::Codex, false)
                .await,
            CatalogRead::Ready(typed(json!({"data":[{"model":"stale"}]})))
        );
    }

    #[tokio::test]
    async fn account_rotation_rejects_waiter_and_does_not_repopulate_old_identity() {
        let service = ModelCatalogService::new();
        let key = (
            "account".to_owned(),
            "/old-home".to_owned(),
            ProviderKind::Codex,
        );
        service.retain_accounts(&[json!({
            "id":"account",
            "home":"/old-home",
            "provider":"codex"
        })]);
        service.0.state.lock().expect("cache").entries.insert(
            key.clone(),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: None,
                expires_at: None,
                error: false,
                pending: true,
            },
        );

        let waiting = service.clone();
        let read = tokio::spawn(async move {
            waiting
                .read_until_ready("account", "/old-home", ProviderKind::Codex, false)
                .await
        });
        tokio::task::yield_now().await;
        service.retain_accounts(&[json!({
            "id":"account",
            "home":"/new-home",
            "provider":"claude"
        })]);

        assert_eq!(
            read.await.expect("catalog waiter"),
            CatalogRead::Unavailable
        );
        assert!(
            !service
                .0
                .state
                .lock()
                .expect("cache")
                .entries
                .contains_key(&key)
        );
        assert_eq!(
            service
                .read_until_ready("account", "/old-home", ProviderKind::Codex, false)
                .await,
            CatalogRead::Unavailable
        );
        assert!(
            !service
                .0
                .state
                .lock()
                .expect("cache")
                .entries
                .contains_key(&key)
        );
    }

    #[tokio::test]
    async fn cancelled_shutdown_keeps_refresh_join_ownership() {
        let service = ModelCatalogService::new();
        let (release, wait) = tokio::sync::oneshot::channel::<()>();
        service.0.tasks.lock().await.push(tokio::spawn(async move {
            wait.await.expect("release refresh");
        }));
        let first = service.clone();
        let drain = tokio::spawn(async move { first.shutdown().await });
        tokio::time::timeout(Duration::from_secs(10), async {
            while !*service.0.stop.subscribe().borrow() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("drain starts");
        drain.abort();
        assert!(drain.await.expect_err("cancelled drain").is_cancelled());
        assert_eq!(
            service.0.tasks.lock().await.len(),
            1,
            "unfinished refresh remains owned"
        );
        release.send(()).expect("refresh retained");
        service.shutdown().await.expect("resumed drain");
        assert!(service.0.tasks.lock().await.is_empty());
        *service.0.cleanup_error.lock().expect("report") =
            Some("provider child 12345 outlived reap grace; termination is unconfirmed".into());
        let error = service
            .shutdown()
            .await
            .expect_err("cleanup failure remains observable");
        assert!(error.to_string().contains("12345"));
    }

    #[tokio::test]
    async fn cancelled_read_waiting_for_task_registry_does_not_leave_pending_entry() {
        let service = ModelCatalogService::new();
        let _pages = serve_pages(&service, AccountSessionPages::gated(Vec::new()));
        let key = (
            "account".to_owned(),
            "/home".to_owned(),
            ProviderKind::Codex,
        );
        service.0.state.lock().expect("cache").entries.insert(
            key.clone(),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: Some(typed(json!({"data":[{"model":"stale"}]}))),
                expires_at: Some(Instant::now()),
                error: false,
                pending: false,
            },
        );
        let task_guard = service.0.tasks.lock().await;
        {
            let read = service.read("account", "/home", ProviderKind::Codex, false);
            tokio::pin!(read);
            tokio::select! {
                biased;
                result = &mut read => panic!("read passed the held task registry: {result:?}"),
                () = tokio::task::yield_now() => {},
            }
        }
        drop(task_guard);

        let cache = service.0.state.lock().expect("catalog cache");
        assert!(!cache.entries[&key].pending);
        assert_eq!(
            cache.entries[&key].value,
            Some(typed(json!({"data":[{"model":"stale"}]})))
        );
    }

    #[tokio::test(start_paused = true)]
    async fn read_rechecks_expiry_after_waiting_for_task_registry() {
        let service = ModelCatalogService::new();
        let _pages = serve_pages(&service, AccountSessionPages::gated(Vec::new()));
        let key = (
            "account".to_owned(),
            "/home".to_owned(),
            ProviderKind::Codex,
        );
        service.0.state.lock().expect("cache").entries.insert(
            key.clone(),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: Some(typed(json!({"data":[{"model":"stale"}]}))),
                expires_at: Some(Instant::now() + Duration::from_secs(1)),
                error: false,
                pending: false,
            },
        );
        let task_guard = service.0.tasks.lock().await;
        let read = service.read("account", "/home", ProviderKind::Codex, false);
        tokio::pin!(read);
        tokio::select! {
            biased;
            result = &mut read => panic!("read passed the held task registry: {result:?}"),
            () = tokio::task::yield_now() => {},
        }
        tokio::time::advance(Duration::from_secs(2)).await;
        drop(task_guard);

        assert_eq!(
            read.await,
            CatalogRead::Ready(typed(json!({"data":[{"model":"stale"}]}))),
        );
        assert!(
            service.0.state.lock().expect("cache").entries[&key].pending,
            "expired stale data must have a refresh registered"
        );
        service.shutdown().await.expect("join refresh task");
    }

    #[tokio::test(start_paused = true)]
    async fn admission_never_exposes_cold_stale_pending_failed_or_foreign_catalogs() {
        use super::AdmissionCatalogRead;
        let service = ModelCatalogService::new();
        let key = (
            "account".to_owned(),
            "/disposable".to_owned(),
            ProviderKind::Codex,
        );
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, key.2),
            AdmissionCatalogRead::Pending
        );
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, ProviderKind::Claude),
            AdmissionCatalogRead::Pending
        );
        let value = json!({"data": [{"model": "one"}]});
        let expiry = Instant::now() + CATALOG_TTL;
        service.0.state.lock().expect("cache").entries.insert(
            key.clone(),
            CacheEntry {
                read_at: None,
                generation: 7,
                value: Some(typed(value.clone())),
                expires_at: Some(expiry),
                error: false,
                pending: false,
            },
        );
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, key.2),
            AdmissionCatalogRead::Fresh {
                value: typed(value.clone()),
                account_key: key.0.clone(),
                home: key.1.clone(),
                provider: key.2,
                generation: 7,
                expires_at: expiry.into_std(),
            }
        );
        assert_eq!(
            service.read_for_admission("foreign", &key.1, key.2),
            AdmissionCatalogRead::Pending
        );
        assert_eq!(
            service.read_for_admission(&key.0, "/changed-home", key.2),
            AdmissionCatalogRead::Pending
        );
        tokio::time::advance(CATALOG_TTL).await;
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, key.2),
            AdmissionCatalogRead::Pending
        );
        {
            let mut cache = service.0.state.lock().expect("cache");
            let entry = cache.entries.get_mut(&key).expect("entry");
            entry.expires_at = Some(Instant::now() + CATALOG_TTL);
            entry.pending = true;
        }
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, key.2),
            AdmissionCatalogRead::Pending
        );
        assert!(store_result(&service.0, &key, 7, Err(())));
        assert_eq!(
            service.read_for_admission(&key.0, &key.1, key.2),
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
    async fn malformed_known_model_fields_fail_provider_ingress() {
        for (field, value) in [
            ("hidden", json!("yes")),
            ("isDefault", json!(1)),
            ("resolvedModel", json!(false)),
            ("defaultReasoningEffort", json!([])),
            ("supportedReasoningEfforts", json!([{"reasoningEffort": 4}])),
            ("serviceTiers", json!([{"id": false}])),
            ("availableAccessPrograms", json!({"cyber": ["standard", 1]})),
        ] {
            let mut row = json!({"model":"valid"});
            row[field] = value;
            assert_eq!(
                scripted(vec![json!({"data":[row]})]).await,
                Err(()),
                "{field}"
            );
        }
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
}
