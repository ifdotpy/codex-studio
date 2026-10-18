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

use super::{AuthMetadata, AuthStatus};
use std::collections::HashMap;
use std::os::unix::fs::MetadataExt;
use std::path::PathBuf;
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
    /// Directories that hold the credential files.
    pub directories: Vec<PathBuf>,
    /// File names inside those directories whose state decides the status.
    pub credential_files: &'static [&'static str],
}

/// Identity of one file's contents as the filesystem reports them; `None` is a
/// missing file, which is a state of its own.
type FileFingerprint = Option<(u64, u64, u64, i64, i64)>;

impl CredentialLocation {
    fn fingerprint(&self) -> Vec<FileFingerprint> {
        self.directories
            .iter()
            .flat_map(|directory| {
                self.credential_files.iter().map(|name| {
                    std::fs::metadata(directory.join(name)).ok().map(|meta| {
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

#[derive(Default)]
pub(crate) struct AuthCache {
    entries: Mutex<HashMap<CredentialLocation, Arc<Entry>>>,
}

#[derive(Default)]
struct Entry {
    /// Advanced by every explicit invalidation; a status is valid only for the
    /// generation read before its probe began.
    generation: AtomicU64,
    cached: Mutex<Option<Cached>>,
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
        let entry = self.entry(location);
        // Holding this lock through the probe makes concurrent readers wait for it.
        let mut cached = entry.cached.lock().unwrap_or_else(PoisonError::into_inner);
        let generation = entry.generation.load(Ordering::Acquire);
        // Taken before the probe: a rewrite during the probe mismatches next time.
        let fingerprint = location.fingerprint();
        if let Some(hit) = cached.as_ref()
            && hit.generation == generation
            && hit.fingerprint == fingerprint
        {
            return hit.metadata.clone();
        }
        let metadata = probe();
        let conclusive = matches!(metadata.status, AuthStatus::Ready | AuthStatus::SignedOut);
        *cached = conclusive.then(|| Cached {
            generation,
            fingerprint,
            metadata: metadata.clone(),
        });
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
}
