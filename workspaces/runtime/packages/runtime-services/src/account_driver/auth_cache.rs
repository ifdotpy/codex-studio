//! One cached auth status per credential location, for every provider.
//!
//! The core is provider-neutral: a provider supplies a [`CredentialLocation`]
//! (which files decide the status) and a probe closure (how to read it).
//! Claude's `claude auth status` child costs about 180 ms and Codex parses
//! `auth.json`; both are probed once and then served from memory.
//!
//! A cached status ends in two ways, never by time: every read compares the
//! credential files' fingerprint (device, inode, size, modification time, or
//! absence) with the one taken before the probe, and a mismatch re-probes; and
//! [`AuthCache::invalidate`] ends it after a Studio login or a provider auth
//! error reported by a turn. The comparison is synchronous, so a read right
//! after a credential rewrite never sees the old status. Concurrent readers of
//! one location share a single probe.
//!
//! [`AuthCache::changes`] advances when a status may have changed: on every
//! invalidation and when a probe returns another status than the previous
//! probe of that location. Held turn starts wait on it, never on a timer.
//!
//! [`AuthCache::watch`] makes credential files raise that event themselves: an
//! external `claude login` or `auth.json` rewrite invalidates the profile's entry
//! and advances the changes at once, without waiting for the next status read.
//! The parent directories are watched, so an atomic rename-replace is seen.

use super::{AuthMetadata, AuthStatus};
use notify::{Event, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::{HashMap, HashSet};
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, OnceLock, PoisonError, Weak};
use std::time::{Duration, Instant};
use studio_runtime_store::path_identity;
use tokio::sync::watch;

/// Length of the window that merges one burst of filesystem events of a
/// credential file (create, write and rename of a single login) into one
/// invalidation, in milliseconds as a [`Duration`].
pub(crate) const CREDENTIAL_EVENT_BURST: Duration = Duration::from_millis(100);

/// The production cache shared by every account driver of this process.
pub(crate) fn shared() -> &'static AuthCache {
    static CACHE: OnceLock<AuthCache> = OnceLock::new();
    CACHE.get_or_init(AuthCache::default)
}

/// Where one profile keeps the files that decide its auth status.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct CredentialLocation {
    /// Provider and probe identity, such as the resolved `claude` executable.
    pub identity: String,
    /// Directories that hold the credential files.
    pub directories: Vec<PathBuf>,
    /// Files inside those directories whose state decides the status.
    pub credential_files: &'static [CredentialFile],
}

/// One credential file and which of its contents decide the auth status.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CredentialFile {
    pub name: &'static str,
    pub format: CredentialFormat,
}

/// The part of a credential file that identifies a login. Claude Code and the
/// providers rewrite these files for unrelated reasons (history, token
/// refresh); only a change of this part is an auth event.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum CredentialFormat {
    /// `~/.claude.json`: the `oauthAccount` object.
    ClaudeAccount,
    /// `.credentials.json`: token presence and `subscriptionType`, not the tokens.
    ClaudeToken,
    /// `auth.json`: mode, API key, account id, and token presence.
    CodexAuth,
}

/// Upper bound on a credential file read for its identity, in bytes.
/// `~/.claude.json` grows with project history, so the bound is generous.
const CREDENTIAL_READ_LIMIT_BYTES: u64 = 64 * 1024 * 1024;

fn present(value: &Option<serde_json::Value>) -> bool {
    use serde_json::Value;
    !matches!(value, None | Some(Value::Null | Value::Bool(false)))
        && value.as_ref().and_then(Value::as_str) != Some("")
}

impl CredentialFormat {
    /// Digest of the identity part of `bytes`; `None` when it does not parse,
    /// which is a state of its own so that a repaired file is a change.
    fn identity(self, bytes: &[u8]) -> Option<u64> {
        use serde::Deserialize;
        use serde_json::Value;
        use std::hash::{Hash, Hasher};
        #[derive(Deserialize)]
        struct Account {
            #[serde(rename = "oauthAccount")]
            account: Option<Value>,
        }
        #[derive(Deserialize)]
        struct Token {
            #[serde(rename = "claudeAiOauth")]
            oauth: Option<Oauth>,
        }
        #[derive(Deserialize)]
        struct Oauth {
            #[serde(rename = "accessToken")]
            access: Option<Value>,
            #[serde(rename = "refreshToken")]
            refresh: Option<Value>,
            #[serde(rename = "subscriptionType")]
            subscription: Option<Value>,
        }
        #[derive(Deserialize)]
        struct Codex {
            auth_mode: Option<Value>,
            #[serde(rename = "OPENAI_API_KEY")]
            api_key: Option<Value>,
            tokens: Option<Tokens>,
        }
        #[derive(Deserialize)]
        struct Tokens {
            account_id: Option<Value>,
            access_token: Option<Value>,
            id_token: Option<Value>,
        }
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        match self {
            Self::ClaudeAccount => serde_json::from_slice::<Account>(bytes)
                .ok()?
                .account
                .map(|value| value.to_string())
                .hash(&mut hasher),
            Self::ClaudeToken => {
                let oauth = serde_json::from_slice::<Token>(bytes).ok()?.oauth;
                oauth
                    .map(|oauth| {
                        (
                            present(&oauth.access),
                            present(&oauth.refresh),
                            oauth.subscription.map(|value| value.to_string()),
                        )
                    })
                    .hash(&mut hasher);
            }
            Self::CodexAuth => {
                let auth = serde_json::from_slice::<Codex>(bytes).ok()?;
                let tokens = auth.tokens;
                (
                    auth.auth_mode.map(|value| value.to_string()),
                    auth.api_key.map(|value| value.to_string()),
                    tokens
                        .as_ref()
                        .and_then(|tokens| tokens.account_id.as_ref())
                        .map(ToString::to_string),
                    tokens
                        .as_ref()
                        .is_some_and(|tokens| present(&tokens.access_token)),
                    tokens
                        .as_ref()
                        .is_some_and(|tokens| present(&tokens.id_token)),
                )
                    .hash(&mut hasher);
            }
        }
        Some(hasher.finish())
    }
}

/// Identity of one file's contents as the filesystem reports them; `None` is a
/// missing file, which is a state of its own.
type FileFingerprint = Option<(u64, u64, u64, i64, i64)>;

impl CredentialLocation {
    /// Digest of what every credential file of this location says about the
    /// login. Unreadable and missing files are states of their own.
    fn identity_digest(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        use std::io::Read;
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        for directory in &self.directories {
            for file in self.credential_files {
                let identity = std::fs::File::open(directory.join(file.name))
                    .ok()
                    .map(|handle| {
                        let mut bytes = Vec::new();
                        handle
                            .take(CREDENTIAL_READ_LIMIT_BYTES)
                            .read_to_end(&mut bytes)
                            .ok()
                            .and_then(|_| file.format.identity(&bytes))
                    });
                identity.hash(&mut hasher);
            }
        }
        hasher.finish()
    }

    /// Every spelling of this location's directories that an event can name:
    /// as written and resolved. macOS FSEvents reports resolved paths, so a
    /// directory under a link such as `/var` arrives as `/private/var/...`.
    fn event_directories(&self) -> Vec<PathBuf> {
        let mut spellings = self.directories.clone();
        spellings.extend(
            self.directories
                .iter()
                .filter_map(|directory| path_identity::resolve(directory))
                .filter(|resolved| !self.directories.contains(resolved)),
        );
        spellings
    }

    fn fingerprint(&self) -> Vec<FileFingerprint> {
        self.directories
            .iter()
            .flat_map(|directory| {
                self.credential_files.iter().map(|file| {
                    std::fs::metadata(directory.join(file.name))
                        .ok()
                        .map(|meta| {
                            (
                                meta.dev(),
                                meta.ino(),
                                meta.len(),
                                meta.mtime(),
                                meta.mtime_nsec(),
                            )
                        })
                })
            })
            .collect()
    }
}

pub(crate) struct AuthCache {
    core: Arc<Core>,
    /// Created by the first [`AuthCache::watch`].
    watcher: Mutex<Option<CredentialWatcher>>,
}

/// The state that both readers and the watcher's worker thread reach.
struct Core {
    entries: Mutex<HashMap<CredentialLocation, Arc<Entry>>>,
    /// Advanced when any location's status may have changed.
    changes: watch::Sender<u64>,
}

impl Default for AuthCache {
    fn default() -> Self {
        Self {
            core: Arc::new(Core {
                entries: Mutex::default(),
                changes: watch::channel(0).0,
            }),
            watcher: Mutex::new(None),
        }
    }
}

/// A credential location under watch and the directory spellings that its
/// events can name ([`CredentialLocation::event_directories`]).
struct WatchedLocation {
    location: CredentialLocation,
    directories: Vec<PathBuf>,
}

impl WatchedLocation {
    /// Whether `event` names one of this location's credential files.
    fn is_touched_by(&self, event: &Event) -> bool {
        event.paths.iter().any(|path| {
            path.parent()
                .is_some_and(|parent| self.directories.iter().any(|directory| directory == parent))
                && path.file_name().is_some_and(|name| {
                    self.location
                        .credential_files
                        .iter()
                        .any(|file| name == file.name)
                })
        })
    }
}

/// Credential locations under watch, shared with the notify callback.
type WatchedLocations = Arc<Mutex<Vec<WatchedLocation>>>;

/// Identity digest of each watched location as last seen.
type SeenIdentities = Arc<Mutex<HashMap<CredentialLocation, u64>>>;

/// One directory watch per distinct parent of a credential file.
struct CredentialWatcher {
    watcher: RecommendedWatcher,
    locations: WatchedLocations,
    seen: SeenIdentities,
    watched: HashSet<PathBuf>,
}

impl CredentialWatcher {
    fn new(core: Weak<Core>) -> notify::Result<Self> {
        let locations = WatchedLocations::default();
        let seen = SeenIdentities::default();
        let worker_seen = Arc::clone(&seen);
        let (events, bursts) = mpsc::channel::<CredentialLocation>();
        let callback_locations = Arc::clone(&locations);
        let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let Ok(event) = result else { return };
            if event.kind.is_access() {
                return;
            }
            let locations = callback_locations
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            for watched in locations
                .iter()
                .filter(|watched| watched.is_touched_by(&event))
            {
                events.send(watched.location.clone()).ok();
            }
        })?;
        // Ends when the watcher, which owns the sender, is dropped.
        std::thread::Builder::new()
            .name("credential-watch".into())
            .spawn(move || {
                while let Ok(first) = bursts.recv() {
                    let mut touched = HashSet::from([first]);
                    let deadline = Instant::now() + CREDENTIAL_EVENT_BURST;
                    while let Ok(next) =
                        bursts.recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    {
                        touched.insert(next);
                    }
                    let Some(core) = core.upgrade() else { return };
                    for location in touched {
                        // An event that left the login as it was is not an auth event.
                        let digest = location.identity_digest();
                        let previous = worker_seen
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner)
                            .insert(location.clone(), digest);
                        if previous != Some(digest) {
                            core.invalidate(&location);
                        }
                    }
                }
            })
            .map_err(notify::Error::io)?;
        Ok(Self {
            watcher,
            locations,
            seen,
            watched: HashSet::new(),
        })
    }

    fn set(&mut self, wanted: Vec<CredentialLocation>) {
        let directories: HashSet<PathBuf> = wanted
            .iter()
            .flat_map(|location| location.directories.iter().cloned())
            .collect();
        {
            // Baseline for new locations, taken before their directories are watched.
            let mut seen = self.seen.lock().unwrap_or_else(PoisonError::into_inner);
            seen.retain(|location, _| wanted.contains(location));
            for location in &wanted {
                seen.entry(location.clone())
                    .or_insert_with(|| location.identity_digest());
            }
        }
        // Resolved once here, not for each event in the notify callback.
        let watched = wanted
            .into_iter()
            .map(|location| WatchedLocation {
                directories: location.event_directories(),
                location,
            })
            .collect();
        *self
            .locations
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = watched;
        self.watched.retain(|directory| {
            let keep = directories.contains(directory);
            if !keep {
                self.watcher.unwatch(directory).ok();
            }
            keep
        });
        for directory in directories {
            // A directory that does not exist yet is retried by the next call.
            if !self.watched.contains(&directory)
                && self
                    .watcher
                    .watch(&directory, RecursiveMode::NonRecursive)
                    .is_ok()
            {
                self.watched.insert(directory);
            }
        }
    }
}

#[derive(Default)]
struct Entry {
    /// Advanced by every explicit invalidation; a status is valid only for the
    /// generation read before its probe began.
    generation: AtomicU64,
    cached: Mutex<Probed>,
}

#[derive(Default)]
struct Probed {
    cached: Option<Cached>,
    /// Status of the latest probe, conclusive or not.
    last: Option<AuthStatus>,
}

struct Cached {
    generation: u64,
    fingerprint: Vec<FileFingerprint>,
    metadata: AuthMetadata,
}

impl AuthCache {
    /// Return the cached status of `location`, running `probe` once when none
    /// is valid. Only conclusive statuses are cached: an error or a platform
    /// "unchanged" answer is probed again by the next reader.
    pub(crate) fn read(
        &self,
        location: &CredentialLocation,
        probe: impl FnOnce() -> AuthMetadata,
    ) -> AuthMetadata {
        let entry = self.core.entry(location);
        // Holding this lock through the probe makes concurrent readers wait for it.
        let mut cached = entry.cached.lock().unwrap_or_else(PoisonError::into_inner);
        let generation = entry.generation.load(Ordering::Acquire);
        // Taken before the probe: a rewrite during the probe mismatches next time.
        let fingerprint = location.fingerprint();
        if let Some(hit) = cached.cached.as_ref()
            && hit.generation == generation
            && hit.fingerprint == fingerprint
        {
            return hit.metadata.clone();
        }
        let metadata = probe();
        let conclusive = matches!(metadata.status, AuthStatus::Ready | AuthStatus::SignedOut);
        cached.cached = conclusive.then(|| Cached {
            generation,
            fingerprint,
            metadata: metadata.clone(),
        });
        if cached
            .last
            .replace(metadata.status)
            .is_some_and(|last| last != metadata.status)
        {
            self.core.publish_change();
        }
        metadata
    }

    /// End the cached status of `location`; the next read probes once.
    pub(crate) fn invalidate(&self, location: &CredentialLocation) {
        self.core.invalidate(location);
    }

    /// Advances when a cached status may have changed; see the module notes.
    pub(crate) fn changes(&self) -> watch::Receiver<u64> {
        self.core.changes.subscribe()
    }

    /// Watch exactly the credential files of `locations`: a change invalidates
    /// that location and advances [`AuthCache::changes`]. Locations dropped from
    /// the set are unwatched, new ones watched.
    pub(crate) fn watch(&self, locations: Vec<CredentialLocation>) {
        let mut slot = self.watcher.lock().unwrap_or_else(PoisonError::into_inner);
        if slot.is_none() {
            match CredentialWatcher::new(Arc::downgrade(&self.core)) {
                Ok(watcher) => *slot = Some(watcher),
                Err(error) => return eprintln!("credential watch unavailable: {error}"),
            }
        }
        if let Some(watcher) = slot.as_mut() {
            watcher.set(locations);
        }
    }
}

impl Core {
    fn invalidate(&self, location: &CredentialLocation) {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = entries.get(location) {
            entry.generation.fetch_add(1, Ordering::AcqRel);
        }
        drop(entries);
        self.publish_change();
    }

    fn publish_change(&self) {
        self.changes
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }

    fn entry(&self, location: &CredentialLocation) -> Arc<Entry> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(entries.entry(location.clone()).or_default())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;
    use std::sync::atomic::AtomicUsize;
    use std::time::Duration;

    fn ready() -> AuthMetadata {
        AuthMetadata {
            status: AuthStatus::Ready,
            ..AuthMetadata::signed_out()
        }
    }

    fn location(directory: &Path) -> CredentialLocation {
        CredentialLocation {
            identity: "claude".into(),
            directories: vec![directory.to_owned()],
            credential_files: &[CredentialFile {
                name: ".credentials.json",
                format: CredentialFormat::ClaudeToken,
            }],
        }
    }

    fn codex_location(directory: &Path) -> CredentialLocation {
        CredentialLocation {
            identity: "codex".into(),
            directories: vec![directory.to_owned()],
            credential_files: &[CredentialFile {
                name: "auth.json",
                format: CredentialFormat::CodexAuth,
            }],
        }
    }

    /// Probes seen by one test, counted so the test can assert exact spawns.
    struct Probes(AtomicUsize);

    impl Probes {
        fn run(&self, metadata: AuthMetadata) -> AuthMetadata {
            self.0.fetch_add(1, Ordering::SeqCst);
            metadata
        }

        fn count(&self) -> usize {
            self.0.load(Ordering::SeqCst)
        }
    }

    fn probes() -> Probes {
        Probes(AtomicUsize::new(0))
    }

    #[test]
    fn cached_status_is_served_without_a_second_probe() {
        let directory = tempfile::tempdir().expect("config dir");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = location(directory.path());
        for _ in 0..5 {
            let metadata = cache.read(&location, || probes.run(ready()));
            assert_eq!(metadata.status, AuthStatus::Ready);
        }
        assert_eq!(probes.count(), 1);
    }

    #[test]
    fn changes_advance_on_invalidation_and_a_new_status_only() {
        let directory = tempfile::tempdir().expect("config dir");
        let cache = AuthCache::default();
        let location = location(directory.path());
        let changes = cache.changes();
        cache.read(&location, ready);
        cache.read(&location, ready);
        assert!(!changes.has_changed().expect("cache alive"), "first status");
        cache.invalidate(&location);
        assert!(changes.has_changed().expect("cache alive"), "invalidation");
        let mut changes = cache.changes();
        cache.read(&location, ready);
        assert!(!changes.has_changed().expect("cache alive"), "same status");
        cache.invalidate(&location);
        changes.mark_unchanged();
        cache.read(&location, AuthMetadata::signed_out);
        assert!(changes.has_changed().expect("cache alive"), "new status");
    }

    #[test]
    fn explicit_invalidation_causes_exactly_one_reprobe() {
        let directory = tempfile::tempdir().expect("config dir");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = location(directory.path());
        cache.read(&location, || probes.run(ready()));
        cache.invalidate(&location);
        cache.invalidate(&location);
        for _ in 0..3 {
            cache.read(&location, || probes.run(ready()));
        }
        assert_eq!(probes.count(), 2);
    }

    #[test]
    fn credential_file_change_causes_exactly_one_reprobe() {
        let directory = tempfile::tempdir().expect("config dir");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = location(directory.path());
        cache.read(&location, || probes.run(ready()));
        std::fs::write(directory.path().join(".credentials.json"), "{}").expect("credentials");
        for _ in 0..3 {
            cache.read(&location, || probes.run(ready()));
        }
        assert_eq!(probes.count(), 2);
    }

    #[test]
    fn concurrent_readers_share_one_probe() {
        let directory = tempfile::tempdir().expect("config dir");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = location(directory.path());
        std::thread::scope(|scope| {
            for _ in 0..64 {
                scope.spawn(|| {
                    let metadata = cache.read(&location, || {
                        std::thread::sleep(Duration::from_millis(50));
                        probes.run(ready())
                    });
                    assert_eq!(metadata.status, AuthStatus::Ready);
                });
            }
        });
        assert_eq!(probes.count(), 1);
    }

    #[test]
    fn invalidation_during_a_probe_discards_its_result() {
        let directory = tempfile::tempdir().expect("config dir");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = location(directory.path());
        cache.read(&location, || {
            cache.invalidate(&location);
            probes.run(ready())
        });
        cache.read(&location, || probes.run(ready()));
        assert_eq!(probes.count(), 2);
    }

    #[test]
    fn auth_error_reprobes_once_and_the_failure_stays_visible() {
        let directory = tempfile::tempdir().expect("config dir");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = location(directory.path());
        assert_eq!(
            cache.read(&location, || probes.run(ready())).status,
            AuthStatus::Ready
        );
        // A turn reports a provider auth error: the next read probes once and the
        // refreshed status, not the stale Ready, is what every reader sees.
        cache.invalidate(&location);
        for _ in 0..3 {
            let metadata = cache.read(&location, || probes.run(AuthMetadata::signed_out()));
            assert_eq!(metadata.status, AuthStatus::SignedOut);
        }
        assert_eq!(probes.count(), 2);
    }

    #[test]
    fn probe_errors_are_not_cached() {
        let directory = tempfile::tempdir().expect("config dir");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = location(directory.path());
        for _ in 0..2 {
            let metadata = cache.read(&location, || probes.run(AuthMetadata::error(Some("spawn"))));
            assert_eq!(metadata.status, AuthStatus::Error);
        }
        assert_eq!(probes.count(), 2);
    }

    #[test]
    fn codex_auth_file_change_causes_exactly_one_reprobe() {
        let directory = tempfile::tempdir().expect("codex home");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = codex_location(directory.path());
        for _ in 0..3 {
            cache.read(&location, || probes.run(ready()));
        }
        assert_eq!(probes.count(), 1);
        std::fs::write(directory.path().join("auth.json"), "{}").expect("auth file");
        for _ in 0..3 {
            cache.read(&location, || probes.run(ready()));
        }
        assert_eq!(probes.count(), 2);
        cache.invalidate(&location);
        cache.read(&location, || probes.run(AuthMetadata::signed_out()));
        assert_eq!(probes.count(), 3);
    }

    #[test]
    fn replaced_and_removed_credential_files_each_cause_one_reprobe() {
        let directory = tempfile::tempdir().expect("config dir");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = location(directory.path());
        let file = directory.path().join(".credentials.json");
        std::fs::write(&file, "aa").expect("credentials");
        cache.read(&location, || probes.run(ready()));
        // Same size, new inode: an atomic replace is still a change.
        std::fs::write(directory.path().join("next"), "bb").expect("replacement");
        std::fs::rename(directory.path().join("next"), &file).expect("replace");
        cache.read(&location, || probes.run(ready()));
        cache.read(&location, || probes.run(ready()));
        assert_eq!(probes.count(), 2);
        std::fs::remove_file(&file).expect("remove");
        cache.read(&location, || probes.run(AuthMetadata::signed_out()));
        cache.read(&location, || probes.run(AuthMetadata::signed_out()));
        assert_eq!(probes.count(), 3);
    }

    /// Event-driven invalidation, with a watcher on real directories. Each wait
    /// ends at the cache's own change event, never on a sleep.
    mod watched {
        use super::*;

        const EVENT_WAIT: Duration = Duration::from_secs(20);

        async fn next_change(changes: &mut watch::Receiver<u64>) {
            tokio::time::timeout(EVENT_WAIT, changes.changed())
                .await
                .expect("a change event within the wait")
                .expect("cache alive");
        }

        fn generation(cache: &AuthCache, location: &CredentialLocation) -> u64 {
            cache.core.entry(location).generation.load(Ordering::SeqCst)
        }

        /// A credential file body whose login identity is `identity`; `extra`
        /// stands for the unrelated parts that providers rewrite all the time.
        fn body(name: &str, identity: u64, extra: u64) -> String {
            match name {
                "auth.json" => format!(
                    r#"{{"tokens":{{"account_id":"{identity}","access_token":"t{extra}"}}}}"#
                ),
                ".claude.json" => {
                    format!(
                        r#"{{"history":{extra},"oauthAccount":{{"accountUuid":"{identity}"}}}}"#
                    )
                }
                _ => format!(
                    r#"{{"claudeAiOauth":{{"accessToken":"t{extra}","subscriptionType":"{identity}"}}}}"#
                ),
            }
        }

        /// Next identity, so that every call changes the login.
        fn fresh() -> u64 {
            static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
            NEXT.fetch_add(1, Ordering::SeqCst)
        }

        /// How a test rewrites a credential file to another login.
        fn rewrite(directory: &Path, name: &str, replace: bool) {
            write_body(directory, name, &body(name, fresh(), 0), replace);
        }

        fn write_body(directory: &Path, name: &str, contents: &str, replace: bool) {
            if replace {
                std::fs::write(directory.join("next.tmp"), contents).expect("replacement");
                std::fs::rename(directory.join("next.tmp"), directory.join(name)).expect("rename");
            } else {
                std::fs::write(directory.join(name), contents).expect("rewrite");
            }
        }

        fn provider_cases(claude: &Path, codex: &Path) -> [(CredentialLocation, &'static str); 2] {
            [
                (location(claude), ".credentials.json"),
                (codex_location(codex), "auth.json"),
            ]
        }

        #[tokio::test]
        async fn a_rewrite_invalidates_exactly_that_profile_once() {
            for replace in [false, true] {
                let (claude, codex) = (
                    tempfile::tempdir().expect("claude"),
                    tempfile::tempdir().expect("codex"),
                );
                let cases = provider_cases(claude.path(), codex.path());
                let cache = AuthCache::default();
                for (location, _) in &cases {
                    cache.read(location, ready);
                }
                cache.watch(cases.iter().map(|(location, _)| location.clone()).collect());
                let mut changes = cache.changes();
                for (index, (location, name)) in cases.iter().enumerate() {
                    rewrite(location.directories[0].as_path(), name, replace);
                    next_change(&mut changes).await;
                    // One burst is one change, and only this profile is invalidated.
                    assert_eq!(*changes.borrow(), index as u64 + 1, "replace={replace}");
                    for (other, _) in &cases {
                        let expected = u64::from(cases[..=index].iter().any(|(l, _)| l == other));
                        assert_eq!(generation(&cache, other), expected, "replace={replace}");
                    }
                }
            }
        }

        #[tokio::test]
        async fn an_unrelated_file_in_the_directory_changes_nothing() {
            let (claude, codex) = (
                tempfile::tempdir().expect("claude"),
                tempfile::tempdir().expect("codex"),
            );
            let [(claude_location, _), (codex_location, sentinel)] =
                provider_cases(claude.path(), codex.path());
            let cache = AuthCache::default();
            cache.read(&claude_location, ready);
            cache.watch(vec![claude_location.clone(), codex_location.clone()]);
            let mut changes = cache.changes();
            std::fs::write(claude.path().join("settings.json"), "{}").expect("unrelated");
            // Events of one watcher arrive in order: once the sentinel's change is
            // seen, the earlier unrelated event has been handled as well.
            rewrite(codex.path(), sentinel, false);
            next_change(&mut changes).await;
            assert_eq!(*changes.borrow(), 1);
            assert_eq!(generation(&cache, &claude_location), 0);
        }

        #[tokio::test]
        async fn a_rewrite_of_unrelated_fields_raises_no_event() {
            let (claude, codex) = (
                tempfile::tempdir().expect("claude"),
                tempfile::tempdir().expect("codex"),
            );
            let account = CredentialLocation {
                identity: "claude".into(),
                directories: vec![claude.path().to_owned()],
                credential_files: &[
                    CredentialFile {
                        name: ".credentials.json",
                        format: CredentialFormat::ClaudeToken,
                    },
                    CredentialFile {
                        name: ".claude.json",
                        format: CredentialFormat::ClaudeAccount,
                    },
                ],
            };
            let [(_, token), (codex_location, auth)] = provider_cases(claude.path(), codex.path());
            for (directory, name) in [
                (claude.path(), token),
                (claude.path(), ".claude.json"),
                (codex.path(), auth),
            ] {
                write_body(directory, name, &body(name, 7, 0), false);
            }
            let cache = AuthCache::default();
            cache.watch(vec![account.clone(), codex_location.clone()]);
            let mut changes = cache.changes();
            // Token refresh and history growth keep the login: no event, not even
            // a replaced file.
            for (directory, name, replace) in [
                (claude.path(), token, false),
                (claude.path(), ".claude.json", false),
                (claude.path(), ".claude.json", true),
                (codex.path(), auth, true),
            ] {
                write_body(directory, name, &body(name, 7, fresh()), replace);
            }
            // The sentinel changes a login after all of the above has arrived.
            rewrite(claude.path(), ".claude.json", false);
            next_change(&mut changes).await;
            assert_eq!(*changes.borrow(), 1);
            assert_eq!(generation(&cache, &codex_location), 0);
        }

        #[tokio::test]
        async fn the_watch_set_follows_added_and_removed_profiles() {
            let (first, second) = (
                tempfile::tempdir().expect("first"),
                tempfile::tempdir().expect("second"),
            );
            let [(first_location, first_name), (second_location, second_name)] =
                provider_cases(first.path(), second.path());
            let cache = AuthCache::default();
            let mut changes = cache.changes();
            cache.watch(vec![first_location.clone()]);
            // Unwatched yet: this must not be seen; the first profile is the sentinel.
            rewrite(second.path(), second_name, false);
            rewrite(first.path(), first_name, false);
            next_change(&mut changes).await;
            assert_eq!(*changes.borrow(), 1);
            cache.watch(vec![first_location.clone(), second_location.clone()]);
            rewrite(second.path(), second_name, false);
            next_change(&mut changes).await;
            assert_eq!(*changes.borrow(), 2);
            cache.watch(vec![second_location.clone()]);
            rewrite(first.path(), first_name, false);
            rewrite(second.path(), second_name, true);
            next_change(&mut changes).await;
            assert_eq!(*changes.borrow(), 3);
            assert_eq!(generation(&cache, &first_location), 0);
        }
    }
}
