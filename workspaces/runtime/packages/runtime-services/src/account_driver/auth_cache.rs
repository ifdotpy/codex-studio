//! One cached auth status per credential location, for every provider.
//!
//! The core is provider-neutral: a provider supplies a [`CredentialLocation`]
//! (what to watch) and a probe closure (how to read the status). Claude's
//! `claude auth status` child costs about 180 ms and Codex parses `auth.json`;
//! both are probed once and then served from memory. Only events end a cached status: a change
//! to the profile's credential files (a file watch), an explicit
//! [`AuthCache::invalidate`] after a Studio login or a provider auth
//! error reported by a turn. There is no time-based expiry. Concurrent readers
//! of one location share a single probe.

use super::{AuthMetadata, AuthStatus};
use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};

/// The production cache shared by every account driver of this process.
pub(crate) fn shared() -> &'static AuthCache {
    static CACHE: OnceLock<AuthCache> = OnceLock::new();
    CACHE.get_or_init(AuthCache::default)
}

/// Where one profile keeps the files that decide its auth status.
#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(crate) struct CredentialLocation {
    /// Provider and probe identity, such as the resolved `claude` executable.
    pub identity: String,
    /// Non-recursively watched directories that hold the credential files.
    pub directories: Vec<PathBuf>,
    /// File names inside those directories whose change ends a cached status.
    pub credential_files: &'static [&'static str],
}

#[derive(Default)]
pub(crate) struct AuthCache {
    entries: Mutex<HashMap<CredentialLocation, Arc<Entry>>>,
}

struct Entry {
    /// Advanced by every invalidation; a status is valid only for the
    /// generation read before its probe began.
    generation: Arc<AtomicU64>,
    cached: Mutex<Option<(u64, AuthMetadata)>>,
    /// Keeps the file watches alive; `None` when the location cannot be
    /// watched, in which case no status is cached.
    watchers: Option<Vec<RecommendedWatcher>>,
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
        let entry = self.entry(location);
        // Holding this lock through the probe makes concurrent readers wait for it.
        let mut cached = entry.cached.lock().unwrap_or_else(PoisonError::into_inner);
        let generation = entry.generation.load(Ordering::Acquire);
        if let Some((cached_generation, metadata)) = cached.as_ref()
            && *cached_generation == generation
        {
            return metadata.clone();
        }
        let metadata = probe();
        let conclusive = matches!(metadata.status, AuthStatus::Ready | AuthStatus::SignedOut);
        *cached = (conclusive && entry.watchers.is_some()).then(|| (generation, metadata.clone()));
        metadata
    }

    /// End the cached status of `location`; the next read probes once.
    pub(crate) fn invalidate(&self, location: &CredentialLocation) {
        let entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(entry) = entries.get(location) {
            entry.generation.fetch_add(1, Ordering::AcqRel);
        }
    }

    fn entry(&self, location: &CredentialLocation) -> Arc<Entry> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        Arc::clone(
            entries
                .entry(location.clone())
                .or_insert_with(|| Arc::new(Entry::watch(location))),
        )
    }
}

impl Entry {
    fn watch(location: &CredentialLocation) -> Self {
        let generation = Arc::new(AtomicU64::new(0));
        let watchers = location
            .directories
            .iter()
            .map(|directory| watch_directory(directory, location.credential_files, &generation))
            .collect::<Option<Vec<_>>>();
        Self {
            generation,
            cached: Mutex::new(None),
            watchers,
        }
    }
}

fn watch_directory(
    directory: &Path,
    credential_files: &'static [&'static str],
    generation: &Arc<AtomicU64>,
) -> Option<RecommendedWatcher> {
    let watched = directory.to_owned();
    let counter = Arc::clone(generation);
    let mut watcher = notify::recommended_watcher(move |result: notify::Result<notify::Event>| {
        let relevant = match &result {
            // A watcher error may have lost events, so the status is unknown.
            Err(_) => true,
            Ok(event) => {
                !matches!(event.kind, EventKind::Access(_))
                    && (event.paths.is_empty()
                        || event.paths.iter().any(|path| {
                            path == &watched
                                || path
                                    .file_name()
                                    .and_then(OsStr::to_str)
                                    .is_some_and(|name| credential_files.contains(&name))
                        }))
            }
        };
        if relevant {
            counter.fetch_add(1, Ordering::AcqRel);
        }
    })
    .ok()?;
    watcher.watch(directory, RecursiveMode::NonRecursive).ok()?;
    Some(watcher)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;
    use std::time::{Duration, Instant};

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
            credential_files: &[".credentials.json"],
        }
    }

    fn codex_location(directory: &Path) -> CredentialLocation {
        CredentialLocation {
            identity: "codex".into(),
            directories: vec![directory.to_owned()],
            credential_files: &["auth.json"],
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
        // The watch delivers asynchronously: wait for the invalidation, not a fixed time.
        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.entry(&location).generation.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "file change was not observed");
            std::thread::sleep(Duration::from_millis(5));
        }
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
    fn unwatchable_location_is_never_cached() {
        let directory = tempfile::tempdir().expect("config dir");
        let (cache, probes) = (AuthCache::default(), probes());
        let location = location(&directory.path().join("missing"));
        for _ in 0..2 {
            cache.read(&location, || probes.run(ready()));
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
        let deadline = Instant::now() + Duration::from_secs(10);
        while cache.entry(&location).generation.load(Ordering::Acquire) == 0 {
            assert!(Instant::now() < deadline, "file change was not observed");
            std::thread::sleep(Duration::from_millis(5));
        }
        for _ in 0..3 {
            cache.read(&location, || probes.run(ready()));
        }
        assert_eq!(probes.count(), 2);
        cache.invalidate(&location);
        cache.read(&location, || probes.run(AuthMetadata::signed_out()));
        assert_eq!(probes.count(), 3);
    }
}
