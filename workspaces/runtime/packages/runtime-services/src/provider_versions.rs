//! Provider version checks, upstream release cache, and the desktop/sync notice projection.
//!
//! Python reference: `codex_provider_update_notice.py:12-94`,
//! `codex_provider_upstream_releases.py:18-191`, and
//! `codex_provider_versions.py:19-376`. The probe and upstream transport are
//! injected ports. Upstream failures are retained as stale/empty cache data;
//! they never escape through startup or a status read.
//! Active sessions are injected separately: Python snapshots only `servers`
//! entries outside `offline_accounts` at `codex_provider_versions.py:183-186`,
//! then rechecks `(account key, server identity, connection id)` at `:245-248`.
//! The startup integrator leaves this port absent until account-worker/session
//! wiring exists; registry rows alone are intentionally not version profiles.
//! The future session-snapshot supplier should call `ProviderVersionMonitor::wake`
//! when it changes its live connection set.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError, Weak};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use process_wrap::std::CommandWrap;
use regex::Regex;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_process::child_priority::ChildPriority;
use studio_runtime_store::{RuntimeStore, StoreError, SyncDocumentsRepository};
use thiserror::Error;

const CHECK_INTERVAL_SECONDS: u64 = 300;
const UPSTREAM_TTL_SECONDS: f64 = 24.0 * 60.0 * 60.0;
const PROBE_DEADLINE: Duration = Duration::from_secs(5);
const MAX_RELEASE_BYTES: usize = 1024 * 1024;
const REGISTRY_ACCEPT: &str = "application/vnd.npm.install-v1+json";
const RELEASES: [(&str, &str, &str); 2] = [
    (
        "claude",
        "@anthropic-ai/claude-code",
        "https://registry.npmjs.org/@anthropic-ai%2fclaude-code",
    ),
    (
        "codex",
        "@openai/codex",
        "https://registry.npmjs.org/@openai%2fcodex",
    ),
];
const BASELINES: [(&str, &str, &str); 2] = [
    ("codex", "0.153.4", "Codex CLI"),
    ("claude", "2.1.278", "Claude Code"),
];

#[derive(Clone, Debug, Eq, PartialEq)]
struct VersionKey {
    major: String,
    minor: String,
    patch: String,
    release: bool,
    prerelease: Vec<VersionPart>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
enum VersionPart {
    Number(String),
    Text(String),
}

impl Ord for VersionPart {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Number(left), Self::Number(right)) => number_cmp(left, right),
            (Self::Number(_), Self::Text(_)) => Ordering::Less,
            (Self::Text(_), Self::Number(_)) => Ordering::Greater,
            (Self::Text(left), Self::Text(right)) => left.cmp(right),
        }
    }
}

impl PartialOrd for VersionPart {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VersionKey {
    fn cmp(&self, other: &Self) -> Ordering {
        number_cmp(&self.major, &other.major)
            .then_with(|| number_cmp(&self.minor, &other.minor))
            .then_with(|| number_cmp(&self.patch, &other.patch))
            .then_with(|| self.release.cmp(&other.release))
            .then_with(|| self.prerelease.cmp(&other.prerelease))
    }
}

impl PartialOrd for VersionKey {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn number_cmp(left: &str, right: &str) -> Ordering {
    let left = left.trim_start_matches('0');
    let right = right.trim_start_matches('0');
    left.len().cmp(&right.len()).then_with(|| left.cmp(right))
}

fn parse_key(value: &str) -> Option<VersionKey> {
    static PATTERN: std::sync::OnceLock<Option<Regex>> = std::sync::OnceLock::new();
    let pattern = PATTERN
        .get_or_init(|| Regex::new(r"([0-9]+)\.([0-9]+)\.([0-9]+)(?:-([0-9A-Za-z.-]+))?").ok())
        .as_ref()?;
    let captures = pattern.captures_iter(value).find(|captures| {
        let start = captures.get(0).map(|m| m.start()).unwrap_or_default();
        start == 0 || !value.as_bytes()[start - 1].is_ascii_digit()
    })?;
    let prerelease = captures.get(4).map(|part| part.as_str());
    Some(VersionKey {
        major: captures.get(1)?.as_str().into(),
        minor: captures.get(2)?.as_str().into(),
        patch: captures.get(3)?.as_str().into(),
        release: prerelease.is_none(),
        prerelease: prerelease
            .unwrap_or("")
            .split('.')
            .map(|part| {
                if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
                    VersionPart::Number(part.into())
                } else {
                    VersionPart::Text(part.into())
                }
            })
            .collect(),
    })
}

fn compare_version_text(installed: &str, available: &str) -> Option<Ordering> {
    Some(parse_key(installed)?.cmp(&parse_key(available)?))
}

/// Account inputs are immutable snapshots captured before any provider call.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ProviderProfile {
    pub account_key: String,
    pub provider: String,
    pub configured_binary: Option<String>,
    /// Stable identity of the live server object captured with Python's `id(server)`.
    pub server_identity: String,
    /// Current connection generation; `None` matches a missing Python connection id.
    pub connection_id: Option<String>,
}

/// Snapshot of live, non-offline sessions only. Implementations must omit registry-only accounts.
pub trait ActiveProviderConnections: Send + Sync + 'static {
    fn snapshot(&self) -> Vec<ProviderProfile>;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct ProviderPortError;

#[derive(Clone, Debug, Default, PartialEq)]
pub struct VersionFacts {
    pub running_version: Option<String>,
    pub installed_version: Option<String>,
    pub configured_version: Option<String>,
    pub available_version: Option<String>,
    pub error: Option<String>,
    pub running_note: Option<String>,
}

/// Running `--version` is injected; tests must provide a scripted implementation.
pub trait VersionProbe: Send + Sync + 'static {
    fn probe(
        &self,
        profile: &ProviderProfile,
        deadline: Instant,
    ) -> Result<VersionFacts, ProviderPortError>;
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct UpstreamReply {
    pub status: u16,
    pub etag: Option<String>,
    pub latest: Option<String>,
}

/// The transport receives an absolute deadline and never receives credentials.
pub trait UpstreamFetch: Send + Sync + 'static {
    fn fetch(
        &self,
        url: &str,
        etag: Option<&str>,
        deadline: Instant,
    ) -> Result<UpstreamReply, ProviderPortError>;
}

pub trait Clock: Send + Sync + 'static {
    fn now(&self) -> f64;
}

/// Durable preferences and native-notice writes used by the monitor.
pub trait ProviderVersionStore: Send + Sync + 'static {
    fn upstream_enabled(&self) -> bool;
    fn persist_warnings(
        &self,
        observed_accounts: &[String],
        warnings: &[Value],
    ) -> Result<(), ProviderVersionsError>;
}

struct SyncProviderVersionStore(Arc<RuntimeStore>);

/// Creates the SQLite-backed preference and notice adapter used by runtime-server.
pub fn runtime_provider_version_store(store: Arc<RuntimeStore>) -> Arc<dyn ProviderVersionStore> {
    Arc::new(SyncProviderVersionStore(store))
}

impl ProviderVersionStore for SyncProviderVersionStore {
    fn upstream_enabled(&self) -> bool {
        let Ok(connection) = self.0.read() else {
            return false;
        };
        let payload = match SyncDocumentsRepository::preference_payload(&connection, "user") {
            Ok(payload) => payload,
            Err(_) => return false,
        };
        let Some(payload) = payload else {
            return true;
        };
        let Ok(value) = serde_json::from_str::<Value>(&payload) else {
            return false;
        };
        value
            .pointer(
                "/value/fields/[\"codex-studio-preferences-v1\",\"checkProviderUpdates\"]/value",
            )
            .and_then(Value::as_bool)
            .unwrap_or(true)
    }

    fn persist_warnings(
        &self,
        observed_accounts: &[String],
        warnings: &[Value],
    ) -> Result<(), ProviderVersionsError> {
        self.0.transaction(|transaction| {
            let raw: Option<String> = transaction
                .query_row(
                    "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0",
                    [],
                    |row| row.get(0),
                )
                .optional()?;
            let current: Value = match raw {
                Some(text) => serde_json::from_str(&text).map_err(|_| StoreError::InvalidSyncObject {
                    scope: "workspace".into(),
                    id: "current".into(),
                })?,
                None => json!({"nativeNotices": []}),
            };
            let mut notices = match current.pointer("/value/nativeNotices") {
                None | Some(Value::Null) => Vec::new(),
                Some(Value::Array(rows)) => rows.clone(),
                _ => return Err(StoreError::InvalidSyncObject { scope: "workspace".into(), id: "current".into() }),
            };
            let observed_accounts = observed_accounts.iter().map(String::as_str).collect::<std::collections::BTreeSet<_>>();
            let is_observed_warning = |notice: &Value| {
                notice.get("id").and_then(Value::as_str).is_some_and(is_provider_warning_id)
                    && notice.get("accountKey").and_then(Value::as_str).is_some_and(|account| observed_accounts.contains(account))
            };
            let previous = notices
                .iter()
                .filter(|notice| is_observed_warning(notice))
                .cloned()
                .collect::<Vec<_>>();
            notices.retain(|notice| !is_observed_warning(notice));
            for warning in warnings {
                let id = warning.get("id").and_then(Value::as_str).ok_or(StoreError::InvalidSyncObject { scope: "workspace".into(), id: "current".into() })?;
                if !is_provider_warning_id(id) {
                    continue;
                }
                let same_version = previous
                    .iter()
                    .find(|old| old.get("id").and_then(Value::as_str) == Some(id) && old.get("version") == warning.get("version"));
                if let Some(old) = same_version {
                    notices.push(old.clone());
                } else {
                    notices.push(warning.clone());
                }
            }
            let old_projection = current.pointer("/value/nativeNotices").and_then(Value::as_array);
            if old_projection.is_some_and(|old| old == &notices) {
                return Ok(());
            }
            SyncDocumentsRepository::patch_entity(transaction, "workspace", "current", json!({"nativeNotices":notices}))?;
            Ok(())
        })?;
        Ok(())
    }
}

#[derive(Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_secs_f64())
            .unwrap_or_default()
    }
}

/// Bounded `--version` runner used by the runtime-server integration.
#[derive(Default)]
pub struct SystemVersionProbe;

impl VersionProbe for SystemVersionProbe {
    fn probe(
        &self,
        profile: &ProviderProfile,
        deadline: Instant,
    ) -> Result<VersionFacts, ProviderPortError> {
        let binary =
            profile
                .configured_binary
                .as_deref()
                .unwrap_or(match profile.provider.as_str() {
                    "claude" => "claude",
                    _ => "codex",
                });
        let environment =
            crate::account_driver::claude::subscription_environment(std::env::vars_os().collect());
        let mut command = CommandWrap::with_new(binary, |command| {
            command
                .arg("--version")
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .env_clear()
                .envs(environment);
        });
        #[cfg(unix)]
        command.wrap(process_wrap::std::ProcessGroup::leader());
        ChildPriority::Normal.apply_before_spawn(&mut command);
        let mut child = command.spawn().map_err(|_| ProviderPortError)?;
        let stdout = child.stdout().take().ok_or(ProviderPortError)?;
        let reader = thread::spawn(move || {
            use std::io::Read;
            let mut output = Vec::new();
            stdout.take(201).read_to_end(&mut output).map(|_| output)
        });
        let status = loop {
            if Instant::now() >= deadline {
                let _ignored = child.start_kill();
                let _ignored = child.try_wait();
                return Err(ProviderPortError);
            }
            if let Some(status) = child.try_wait().map_err(|_| ProviderPortError)? {
                break status;
            }
            thread::park_timeout(Duration::from_millis(10));
        };
        let bytes = reader
            .join()
            .map_err(|_| ProviderPortError)?
            .map_err(|_| ProviderPortError)?;
        if !status.success() || bytes.len() > 200 {
            return Err(ProviderPortError);
        }
        let output = String::from_utf8(bytes).map_err(|_| ProviderPortError)?;
        let version = output.trim().to_owned();
        if parse_key(&version).is_none() {
            return Err(ProviderPortError);
        }
        Ok(VersionFacts {
            // Claude's CLI executable is not the live session version. Python
            // deliberately reports that session value as unknown.
            running_version: (profile.provider != "claude").then(|| version.clone()),
            installed_version: Some(version),
            configured_version: None,
            available_version: None,
            error: None,
            running_note: (profile.provider == "claude")
                .then(|| "The exact version already running in Claude sessions is unknown.".into()),
        })
    }
}

/// Unauthenticated npm registry adapter. Each exchange is bounded by its passed deadline.
#[derive(Default)]
pub struct NpmReleaseFetcher;

impl UpstreamFetch for NpmReleaseFetcher {
    fn fetch(
        &self,
        url: &str,
        etag: Option<&str>,
        deadline: Instant,
    ) -> Result<UpstreamReply, ProviderPortError> {
        use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
        let timeout = deadline
            .checked_duration_since(Instant::now())
            .filter(|duration| !duration.is_zero())
            .ok_or(ProviderPortError)?;
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .http_status_as_error(false)
            .max_redirects(0)
            .tls_config(tls)
            .timeout_global(Some(timeout))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let mut request = agent.get(url).header("Accept", REGISTRY_ACCEPT);
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        let response = request.call().map_err(|_| ProviderPortError)?;
        let status = response.status().as_u16();
        let etag = response
            .headers()
            .get("ETag")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        if status == 304 {
            return Ok(UpstreamReply {
                status,
                etag,
                latest: None,
            });
        }
        let bytes = response
            .into_body()
            .into_with_config()
            .limit(MAX_RELEASE_BYTES as u64)
            .read_to_vec()
            .map_err(|_| ProviderPortError)?;
        let document: Value = serde_json::from_slice(&bytes).map_err(|_| ProviderPortError)?;
        let latest = document
            .pointer("/dist-tags/latest")
            .and_then(Value::as_str)
            .map(str::to_owned);
        Ok(UpstreamReply {
            status,
            etag,
            latest,
        })
    }
}

#[derive(Debug, Error)]
pub enum ProviderVersionsError {
    #[error("provider version monitor lock was poisoned")]
    Poisoned,
    #[error("provider version cache could not be read")]
    CacheRead,
    #[error("provider version notice storage failed")]
    Store(#[from] StoreError),
    #[error("provider version notice projection is malformed")]
    InvalidProjection,
}

#[derive(Clone, Debug)]
struct CacheEntry {
    version: Option<String>,
    etag: Option<String>,
    fetched_at: Option<f64>,
    last_attempt_at: f64,
}

impl Default for CacheEntry {
    fn default() -> Self {
        Self {
            version: None,
            etag: None,
            fetched_at: None,
            last_attempt_at: 0.0,
        }
    }
}

fn attempt_due(entry: &CacheEntry, now: f64) -> bool {
    now - entry.last_attempt_at >= UPSTREAM_TTL_SECONDS
}

fn cached_version(entry: &CacheEntry, now: f64) -> Option<&str> {
    let version = entry.version.as_deref()?;
    let fetched_at = entry.fetched_at?;
    (now - fetched_at < UPSTREAM_TTL_SECONDS && parse_key(version).is_some()).then_some(version)
}

#[derive(Default)]
struct MonitorState {
    providers: Vec<Value>,
    profile_identity: Vec<(String, String, Option<String>)>,
    checked_at: Option<f64>,
    upstream: std::collections::BTreeMap<String, CacheEntry>,
    upstream_enabled: bool,
    check_requested: bool,
    stop: bool,
}

/// Retained service value shared by desktop reads and the sole monitor worker.
pub struct ProviderVersionMonitor {
    store: Arc<dyn ProviderVersionStore>,
    connections: Option<Arc<dyn ActiveProviderConnections>>,
    probe: Arc<dyn VersionProbe>,
    upstream_fetch: Arc<dyn UpstreamFetch>,
    clock: Arc<dyn Clock>,
    cache_path: PathBuf,
    state: Mutex<MonitorState>,
    wake: Condvar,
    checked: Condvar,
    worker: Mutex<Option<JoinHandle<()>>>,
}

impl ProviderVersionMonitor {
    /// Starts exactly one background checker; this returns without probing or networking.
    pub fn start(
        store: Arc<dyn ProviderVersionStore>,
        connections: Option<Arc<dyn ActiveProviderConnections>>,
        cache_path: PathBuf,
        probe: Arc<dyn VersionProbe>,
        upstream_fetch: Arc<dyn UpstreamFetch>,
        clock: Arc<dyn Clock>,
    ) -> Result<Arc<Self>, ProviderVersionsError> {
        // A corrupt/unreadable cache is a missed optimization, never a startup failure.
        let cache = read_cache(&cache_path).unwrap_or_default();
        let profiles = connections
            .as_ref()
            .map_or_else(Vec::new, |port| port.snapshot());
        let initial = profiles
            .iter()
            .map(|profile| checking_row(profile, clock.now()))
            .collect();
        let monitor = Arc::new(Self {
            store,
            connections,
            probe,
            upstream_fetch,
            clock,
            cache_path,
            state: Mutex::new(MonitorState {
                providers: initial,
                profile_identity: profiles.iter().map(owned_connection_identity).collect(),
                checked_at: None,
                upstream: cache,
                upstream_enabled: true,
                check_requested: false,
                stop: false,
            }),
            wake: Condvar::new(),
            checked: Condvar::new(),
            worker: Mutex::new(None),
        });
        let weak = Arc::downgrade(&monitor);
        let handle = thread::Builder::new()
            .name("provider-version-monitor".into())
            .spawn(move || monitor_loop(weak))
            .map_err(|_| ProviderVersionsError::CacheRead)?;
        *monitor
            .worker
            .lock()
            .map_err(|_| ProviderVersionsError::Poisoned)? = Some(handle);
        Ok(monitor)
    }

    /// Snapshot reads only retained memory; no provider, file, or network effect occurs here.
    pub fn status(&self) -> Value {
        let active = self
            .connections
            .as_ref()
            .map_or_else(Vec::new, |port| port.snapshot());
        let Ok(mut state) = self.state.lock() else {
            return json!({"checkedAt":null,"providers":[],"warnings":[]});
        };
        let active_identity = active
            .iter()
            .map(owned_connection_identity)
            .collect::<Vec<_>>();
        let current = active_identity == state.profile_identity;
        let providers = if current {
            state.providers.clone()
        } else {
            state.check_requested = true;
            self.wake.notify_one();
            active
                .iter()
                .map(|profile| checking_row(profile, self.clock.now()))
                .collect()
        };
        let warnings = warnings_projection(&providers, state.upstream_enabled);
        json!({"checkedAt":if current { state.checked_at } else { None },"providers":providers,"warnings":warnings})
    }

    /// Requests an immediate pass after the active-session source changes.
    /// The integrator's session registry should call this when it updates its snapshot.
    pub fn wake(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.check_requested = true;
            self.wake.notify_one();
        }
    }

    /// Stops the sole worker at its next bounded port boundary.
    pub fn shutdown(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stop = true;
            self.wake.notify_all();
        }
        if let Ok(mut worker) = self.worker.lock()
            && let Some(handle) = worker.take()
        {
            let _ignored = handle.join();
        }
    }

    /// Waits for the first completed snapshot; normal desktop reads never wait.
    pub fn wait_until_checked(&self, timeout: Duration) -> bool {
        let Ok(state) = self.state.lock() else {
            return false;
        };
        let (state, _) = self
            .checked
            .wait_timeout_while(state, timeout, |state| state.checked_at.is_none())
            .unwrap_or_else(PoisonError::into_inner);
        state.checked_at.is_some()
    }

    fn check_once(&self) {
        let profiles = self
            .connections
            .as_ref()
            .map_or_else(Vec::new, |port| port.snapshot());
        let now = self.clock.now();
        let upstream_enabled = self.upstream_enabled();
        let upstream_versions = if upstream_enabled {
            self.check_upstream(now)
        } else {
            Default::default()
        };
        let mut providers = Vec::with_capacity(profiles.len());
        for profile in &profiles {
            if !self.connection_is_current(&profiles) {
                return;
            }
            if !matches!(profile.provider.as_str(), "codex" | "claude") {
                providers.push(unknown_row(profile, now));
                continue;
            }
            let facts = self
                .probe
                .probe(profile, Instant::now() + PROBE_DEADLINE)
                .unwrap_or_else(|_| VersionFacts {
                    error: Some("Could not check the provider CLI version.".into()),
                    ..VersionFacts::default()
                });
            providers.push(diagnostic(
                profile,
                facts,
                upstream_versions.get(&profile.provider),
                now,
            ));
        }
        if !self.connection_is_current(&profiles) {
            return;
        }
        let warnings = warnings_projection(&providers, upstream_enabled);
        if profiles.is_empty() && warnings.is_empty() {
            if let Ok(mut state) = self.state.lock() {
                state.providers.clear();
                state.profile_identity.clear();
                state.checked_at = Some(now);
                state.upstream_enabled = upstream_enabled;
                self.checked.notify_all();
            }
            return;
        }
        let observed_accounts = profiles
            .iter()
            .map(|profile| profile.account_key.clone())
            .collect::<Vec<_>>();
        if self.persist_warnings(&observed_accounts, &warnings).is_ok()
            && let Ok(mut state) = self.state.lock()
        {
            state.providers = providers;
            state.profile_identity = profiles.iter().map(owned_connection_identity).collect();
            state.checked_at = Some(now);
            state.upstream_enabled = upstream_enabled;
            self.checked.notify_all();
        }
    }

    fn connection_is_current(&self, expected: &[ProviderProfile]) -> bool {
        self.connections
            .as_ref()
            .map_or(expected.is_empty(), |port| {
                let current = port.snapshot();
                current.iter().map(connection_identity).collect::<Vec<_>>()
                    == expected.iter().map(connection_identity).collect::<Vec<_>>()
            })
    }

    fn upstream_enabled(&self) -> bool {
        self.store.upstream_enabled()
    }

    fn check_upstream(&self, now: f64) -> std::collections::BTreeMap<String, String> {
        let mut cache = self
            .state
            .lock()
            .map(|state| state.upstream.clone())
            .unwrap_or_default();
        for (_, package, url) in RELEASES {
            let entry = cache.entry(package.into()).or_default();
            if !attempt_due(entry, now) {
                continue;
            }
            entry.last_attempt_at = now;
            let deadline = Instant::now() + PROBE_DEADLINE;
            if let Ok(reply) = self
                .upstream_fetch
                .fetch(url, entry.etag.as_deref(), deadline)
            {
                match reply.status {
                    304 if entry
                        .version
                        .as_deref()
                        .is_some_and(|version| parse_key(version).is_some()) =>
                    {
                        entry.fetched_at = Some(now);
                        if reply.etag.is_some() {
                            entry.etag = reply.etag;
                        }
                    }
                    200 if reply
                        .latest
                        .as_deref()
                        .is_some_and(|version| parse_key(version).is_some()) =>
                    {
                        entry.version = reply.latest;
                        entry.fetched_at = Some(now);
                        if reply.etag.is_some() {
                            entry.etag = reply.etag;
                        }
                    }
                    _ => {}
                }
            }
            if let Ok(mut state) = self.state.lock() {
                state.upstream = cache.clone();
            }
            let _ignored = save_cache(&self.cache_path, &cache);
        }
        cache
            .into_iter()
            .filter_map(|(package, entry)| {
                let provider = RELEASES
                    .iter()
                    .find(|(_, candidate, _)| *candidate == package)?
                    .0;
                cached_version(&entry, now).map(|version| (provider.to_owned(), version.to_owned()))
            })
            .collect()
    }

    fn persist_warnings(
        &self,
        observed_accounts: &[String],
        warnings: &[Value],
    ) -> Result<(), ProviderVersionsError> {
        self.store.persist_warnings(observed_accounts, warnings)
    }
}

fn connection_identity(profile: &ProviderProfile) -> (&str, &str, Option<&str>) {
    (
        &profile.account_key,
        &profile.server_identity,
        profile.connection_id.as_deref(),
    )
}

fn is_provider_warning_id(id: &str) -> bool {
    id.starts_with("provider-version:") || id.starts_with("provider-update:")
}

fn owned_connection_identity(profile: &ProviderProfile) -> (String, String, Option<String>) {
    (
        profile.account_key.clone(),
        profile.server_identity.clone(),
        profile.connection_id.clone(),
    )
}

impl Drop for ProviderVersionMonitor {
    fn drop(&mut self) {
        if let Ok(state) = self.state.get_mut() {
            state.stop = true;
            self.wake.notify_all();
        }
        if let Ok(worker) = self.worker.get_mut()
            && let Some(handle) = worker.take()
        {
            let _ignored = handle.join();
        }
    }
}

fn monitor_loop(monitor: Weak<ProviderVersionMonitor>) {
    loop {
        let Some(service) = monitor.upgrade() else {
            return;
        };
        service.check_once();
        let Ok(state) = service.state.lock() else {
            return;
        };
        let (mut state, _) = service
            .wake
            .wait_timeout_while(
                state,
                Duration::from_secs(CHECK_INTERVAL_SECONDS),
                |state| !state.stop && !state.check_requested,
            )
            .unwrap_or_else(PoisonError::into_inner);
        if state.stop {
            return;
        }
        state.check_requested = false;
    }
}

fn checking_row(profile: &ProviderProfile, _now: f64) -> Value {
    json!({
        "id":format!("provider-version:{}", profile.account_key),
        "accountKey":profile.account_key,
        "provider":profile.provider,
        "status":"checking",
        "runningVersion":null,
        "installedVersion":null,
        "configuredVersion":null,
        "baseline":baseline(&profile.provider),
        "error":null,
        "message":"Checking provider CLI version.",
        "at":null
    })
}

fn unknown_row(profile: &ProviderProfile, now: f64) -> Value {
    json!({
        "id":format!("provider-version:{}", profile.account_key),
        "accountKey":profile.account_key,
        "provider":profile.provider,
        "status":"unknown",
        "runningVersion":null,
        "installedVersion":null,
        "configuredVersion":null,
        "baseline":Value::Null,
        "error":null,
        "message":"No evidence-backed version baseline is configured for this provider.",
        "at":now
    })
}

fn baseline(provider: &str) -> Option<&'static str> {
    BASELINES
        .iter()
        .find(|(name, _, _)| *name == provider)
        .map(|(_, version, _)| *version)
}

fn diagnostic(
    profile: &ProviderProfile,
    facts: VersionFacts,
    upstream: Option<&String>,
    now: f64,
) -> Value {
    let baseline_version = baseline(&profile.provider).unwrap_or_default();
    let running = facts.running_version.as_deref();
    let installed = facts.installed_version.as_deref();
    let configured = facts.configured_version.as_deref();
    let mut notices = Vec::new();
    for (available, source) in [
        (facts.available_version.as_deref(), "installed"),
        (upstream.map(String::as_str), "upstream"),
    ] {
        if let (Some(installed), Some(available)) = (installed, available)
            && compare_version_text(installed, available)
                .is_some_and(|order| order == Ordering::Less)
        {
            notices.push(json!({"availableVersion":available,"source":source}));
        }
    }
    let warning = [
        running.map(|version| (version, "running")),
        installed.map(|version| (version, "launchedExecutable")),
        configured.map(|version| (version, "configuredProfile")),
    ]
    .into_iter()
    .flatten()
    .find_map(|(version, source)| baseline_warning(&profile.provider, version, source));
    let known = [running, installed, configured]
        .into_iter()
        .flatten()
        .any(|version| {
            compare_version_text(version, baseline_version)
                .is_some_and(|order| order == Ordering::Less)
        });
    let status = if known {
        "outdated"
    } else if running.is_some() {
        "current"
    } else if facts.error.is_some() && installed.is_none() && configured.is_none() {
        "error"
    } else {
        "unknown"
    };
    json!({
        "id":format!("provider-version:{}", profile.account_key),
        "accountKey":profile.account_key,
        "provider":profile.provider,
        "status":status,
        "runningVersion":facts.running_version,
        "installedVersion":facts.installed_version,
        "configuredVersion":facts.configured_version,
        "updateNotices":notices,
        "baseline":baseline_version,
        "error":facts.error,
        "message":warning.or(facts.running_note).or_else(|| matches!(status,"unknown"|"error").then_some("Could not determine a provider CLI version.".to_owned())),
        "at":now
    })
}

fn baseline_warning(provider: &str, version: &str, source: &str) -> Option<String> {
    let (baseline, label) = BASELINES
        .iter()
        .find(|(name, _, _)| *name == provider)
        .map(|(_, version, label)| (*version, *label))?;
    if !compare_version_text(version, baseline).is_some_and(|order| order == Ordering::Less) {
        return None;
    }
    let detail = match source {
        "running" => format!("The running {label} version {version} is below"),
        "launchedExecutable" => format!(
            "The executable configured for this {label} connection currently reports {version}, below"
        ),
        _ => format!("The current {label} profile executable reports {version}, below"),
    };
    let caveat = if source == "running" {
        ""
    } else {
        " The exact version already running may be different."
    };
    Some(format!(
        "{detail} this repository's lowest recorded tested version ({baseline}).{caveat} It may work poorly or fail. You can continue at your own risk."
    ))
}

fn update_warnings(provider: &Value, upstream_enabled: bool) -> Vec<Value> {
    let Some(notices) = provider.get("updateNotices").and_then(Value::as_array) else {
        return Vec::new();
    };
    let Some(account) = provider.get("accountKey").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(name) = provider.get("provider").and_then(Value::as_str) else {
        return Vec::new();
    };
    let Some(baseline) = provider.get("baseline").and_then(Value::as_str) else {
        return Vec::new();
    };
    let current = provider
        .get("installedVersion")
        .and_then(Value::as_str)
        .unwrap_or("unknown");
    let label = BASELINES
        .iter()
        .find(|(provider, _, _)| *provider == name)
        .map(|(_, _, label)| *label)
        .unwrap_or(name);
    notices.iter().filter_map(|notice| {
        let available = notice.get("availableVersion")?.as_str()?;
        let source = notice.get("source")?.as_str()?;
        if source == "upstream" && !upstream_enabled { return None; }
        let message = if source == "upstream" {
            format!("A newer {label} release is available upstream than the version currently in use. Current: {current} · Available: {available}.")
        } else if name == "codex" {
            format!("A newer installed {label} version is selected for runtime updates, but this connection is still using {current}. Available: {available}.")
        } else {
            format!("A newer installed {label} version is not in use on this connection yet. Current: {current} · Available: {available}.")
        };
        Some(json!({
            "id":format!("provider-update:{account}{}", if source == "upstream" { ":upstream" } else { "" }),
            "accountKey":account,
            "provider":name,
            "version":available,
            "baseline":baseline,
            "message":message,
            "at":provider.get("at"),
            "level":"info"
        }))
    }).collect()
}

fn warning_projection(provider: &Value, upstream_enabled: bool) -> Vec<Value> {
    let mut warnings = Vec::new();
    if provider.get("status").and_then(Value::as_str) == Some("outdated")
        && let Some(message) = provider.get("message").and_then(Value::as_str)
    {
        let version = provider
            .get("runningVersion")
            .and_then(Value::as_str)
            .or_else(|| provider.get("installedVersion").and_then(Value::as_str))
            .or_else(|| provider.get("configuredVersion").and_then(Value::as_str));
        if let (Some(id), Some(account), Some(name), Some(baseline), Some(version)) = (
            provider.get("id").and_then(Value::as_str),
            provider.get("accountKey").and_then(Value::as_str),
            provider.get("provider").and_then(Value::as_str),
            provider.get("baseline").and_then(Value::as_str),
            version,
        ) {
            warnings.push(json!({"id":id,"accountKey":account,"provider":name,"version":version,"baseline":baseline,"message":message,"at":provider.get("at")}));
        }
    }
    warnings.extend(update_warnings(provider, upstream_enabled));
    warnings
}

fn warnings_projection(providers: &[Value], upstream_enabled: bool) -> Vec<Value> {
    let mut version_warnings = Vec::new();
    let mut update_warnings = Vec::new();
    for warning in providers
        .iter()
        .flat_map(|provider| warning_projection(provider, upstream_enabled))
    {
        if warning
            .get("id")
            .and_then(Value::as_str)
            .is_some_and(|id| id.starts_with("provider-version:"))
        {
            version_warnings.push(warning);
        } else {
            update_warnings.push(warning);
        }
    }
    version_warnings.extend(update_warnings);
    version_warnings
}

fn read_cache(
    path: &Path,
) -> Result<std::collections::BTreeMap<String, CacheEntry>, ProviderVersionsError> {
    let text = match fs::read_to_string(path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Default::default()),
        Err(_) => return Ok(Default::default()),
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return Ok(Default::default());
    };
    let Some(packages) = value.get("packages").and_then(Value::as_object) else {
        return Ok(Default::default());
    };
    Ok(packages
        .iter()
        .filter_map(|(package, entry)| {
            if !RELEASES.iter().any(|(_, known, _)| known == package) {
                return None;
            }
            let version = entry
                .get("version")
                .and_then(Value::as_str)
                .filter(|value| parse_key(value).is_some())
                .map(str::to_owned);
            let etag = entry.get("etag").and_then(Value::as_str).map(str::to_owned);
            Some((
                package.clone(),
                CacheEntry {
                    version,
                    etag,
                    fetched_at: entry.get("fetchedAt").and_then(Value::as_f64),
                    last_attempt_at: entry
                        .get("lastAttemptAt")
                        .and_then(Value::as_f64)
                        .unwrap_or_default(),
                },
            ))
        })
        .collect())
}

fn save_cache(
    path: &Path,
    cache: &std::collections::BTreeMap<String, CacheEntry>,
) -> Result<(), ProviderVersionsError> {
    let packages: serde_json::Map<String, Value> = cache.iter().map(|(package, entry)| {
        (package.clone(), json!({"version":entry.version,"etag":entry.etag,"fetchedAt":entry.fetched_at,"lastAttemptAt":entry.last_attempt_at}))
    }).collect();
    let bytes = serde_json::to_vec(&json!({"version":1,"packages":packages}))
        .map_err(|_| ProviderVersionsError::CacheRead)?;
    if bytes.len() > MAX_RELEASE_BYTES {
        return Err(ProviderVersionsError::CacheRead);
    }
    let parent = path.parent().ok_or(ProviderVersionsError::CacheRead)?;
    fs::create_dir_all(parent).map_err(|_| ProviderVersionsError::CacheRead)?;
    let temp =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| ProviderVersionsError::CacheRead)?;
    use std::io::Write;
    temp.as_file()
        .write_all(&bytes)
        .map_err(|_| ProviderVersionsError::CacheRead)?;
    temp.persist(path)
        .map_err(|_| ProviderVersionsError::CacheRead)?;
    Ok(())
}

#[cfg(test)]
#[path = "provider_versions/tests.rs"]
mod tests;
