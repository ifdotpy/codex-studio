//! One bounded directory scan per profile, kept current by filesystem events.
use super::{HistoryError, RolloutPath};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_PROFILE_DISCOVERY_ENTRIES: usize = 65_536;
const MAX_ROUND_DISCOVERY_ENTRIES: usize = 65_536;
pub struct DiscoveryRound {
    profiles: BTreeMap<PathBuf, Option<Vec<PathBuf>>>,
    limit_profiles: std::collections::BTreeSet<PathBuf>,
    entries: usize,
    profile_entry_limit: usize,
    round_entry_limit: usize,
}

impl Default for DiscoveryRound {
    fn default() -> Self {
        Self {
            profiles: BTreeMap::new(),
            limit_profiles: std::collections::BTreeSet::new(),
            entries: 0,
            profile_entry_limit: MAX_PROFILE_DISCOVERY_ENTRIES,
            round_entry_limit: MAX_ROUND_DISCOVERY_ENTRIES,
        }
    }
}
impl DiscoveryRound {
    #[cfg(test)]
    fn with_entry_limits(profile_entry_limit: usize, round_entry_limit: usize) -> Self {
        Self {
            profile_entry_limit,
            round_entry_limit,
            ..Self::default()
        }
    }

    /// Keep a previously discovered profile current without rescanning its
    /// directory tree for each rollout append.
    pub fn file_changed(&mut self, profile: &Path, path: &Path, removed: bool, structural: bool) {
        let profile = canonical_or_absolute(profile);
        if structural {
            self.entries = 0;
            self.limit_profiles.remove(&profile);
        }
        let Some(paths) = self.profiles.get_mut(&profile) else {
            return;
        };
        let Some(paths) = paths else {
            self.profiles.remove(&profile);
            return;
        };
        let path = canonical_or_absolute(path);
        if removed {
            paths.retain(|candidate| candidate != &path);
        } else if !paths.iter().any(|candidate| candidate == &path) {
            paths.push(path);
        }
    }

    /// A directory event or watcher overflow invalidates only that profile.
    pub fn invalidate(&mut self, profile: &Path) {
        let profile = canonical_or_absolute(profile);
        self.profiles.remove(&profile);
        self.limit_profiles.remove(&profile);
        self.entries = 0;
    }

    /// The scan budget applies to the first discovery in each event round;
    /// cached profiles remain valid across rounds.
    pub fn begin_round(&mut self) {
        self.entries = 0;
    }

    pub fn resolve(
        &mut self,
        profile: &Path,
        thread: &str,
        cancel: &AtomicBool,
    ) -> Result<RolloutPath, HistoryError> {
        if cancel.load(Ordering::Relaxed) {
            return Err(HistoryError::Cancelled);
        }
        let profile =
            fs::canonicalize(profile).map_err(HistoryError::io(super::PROFILE_DIRECTORY))?;
        if self.limit_profiles.contains(&profile) {
            return Err(HistoryError::DiscoveryLimit);
        }
        if !self.profiles.contains_key(&profile) {
            // Empty profiles also consume the bounded per-profile and round work budgets.
            let mut profile_entries = 0;
            let paths = match self.scan(&profile, cancel, &mut profile_entries) {
                Ok(paths) => paths,
                Err(HistoryError::DiscoveryLimit) => {
                    self.limit_profiles.insert(profile.clone());
                    return Err(HistoryError::DiscoveryLimit);
                }
                Err(HistoryError::DiscoveryRoundLimit) => {
                    return Err(HistoryError::DiscoveryRoundLimit);
                }
                Err(error) => return Err(error),
            };
            self.profiles.insert(profile.clone(), paths);
        }
        let Some(paths) = self.profiles.get(&profile).and_then(Option::as_ref) else {
            return Ok(RolloutPath::Unreadable);
        };
        let suffix = format!("-{thread}.jsonl");
        let mut found = paths.iter().filter(|p| {
            p.file_name()
                .and_then(|s| s.to_str())
                .is_some_and(|s| s.ends_with(&suffix))
        });
        let first = found.next();
        Ok(if found.next().is_some() {
            RolloutPath::Ambiguous
        } else {
            first
                .cloned()
                .map(RolloutPath::Found)
                .unwrap_or(RolloutPath::Missing)
        })
    }
    fn visit(&mut self, profile_entries: &mut usize) -> Result<(), HistoryError> {
        *profile_entries += 1;
        if *profile_entries > self.profile_entry_limit {
            return Err(HistoryError::DiscoveryLimit);
        }
        self.entries += 1;
        if self.entries > self.round_entry_limit {
            return Err(HistoryError::DiscoveryRoundLimit);
        }
        Ok(())
    }
    fn scan(
        &mut self,
        profile: &Path,
        cancel: &AtomicBool,
        profile_entries: &mut usize,
    ) -> Result<Option<Vec<PathBuf>>, HistoryError> {
        let mut directories = vec![profile.join("sessions"), profile.join("archived_sessions")];
        let mut paths = Vec::new();
        self.visit(profile_entries)?;
        while let Some(directory) = directories.pop() {
            if cancel.load(Ordering::Relaxed) {
                return Err(HistoryError::Cancelled);
            }
            let read = match fs::read_dir(directory) {
                Ok(read) => read,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(_) => return Ok(None),
            };
            for entry in read {
                self.visit(profile_entries)?;
                if cancel.load(Ordering::Relaxed) {
                    return Err(HistoryError::Cancelled);
                }
                let entry = entry.map_err(HistoryError::io(super::ROLLOUT_DIRECTORY))?;
                let kind = entry
                    .file_type()
                    .map_err(HistoryError::io(super::ROLLOUT_DIRECTORY))?;
                if kind.is_dir() {
                    directories.push(entry.path());
                } else if entry
                    .file_name()
                    .to_str()
                    .is_some_and(|s| s.starts_with("rollout-") && s.ends_with(".jsonl"))
                {
                    paths.push(entry.path());
                }
            }
        }
        Ok(Some(paths))
    }
}

/// The resolved form of `path` ([`studio_runtime_store::path_identity::resolve`]), so that the
/// event of a removed rollout names the same path as its discovery did.
fn canonical_or_absolute(path: &Path) -> PathBuf {
    studio_runtime_store::debug_assert_outside_sql();
    if let Some(path) = studio_runtime_store::path_identity::resolve(path) {
        return path;
    }
    if path.is_absolute() {
        path.to_owned()
    } else {
        std::env::current_dir()
            .map(|cwd| cwd.join(path))
            .unwrap_or_else(|_| path.to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;

    #[test]
    fn file_notification_updates_cached_profile_without_rescanning() {
        let root = tempfile::tempdir().expect("profile");
        let sessions = root.path().join("sessions");
        fs::create_dir_all(&sessions).expect("sessions directory");
        let first = sessions.join("rollout-first-thread.jsonl");
        fs::write(&first, b"{}").expect("initial rollout");

        let mut discovery = DiscoveryRound::default();
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            discovery.resolve(root.path(), "thread", &cancel),
            Ok(RolloutPath::Found(_))
        ));

        let appended = sessions.join("rollout-second-thread.jsonl");
        fs::write(&appended, b"{}").expect("new rollout");
        discovery.file_changed(root.path(), &appended, false, true);
        fs::remove_dir_all(&sessions).expect("remove scanned tree to prove no rescan");
        // Discovery reports resolved paths; the macOS temporary folder is under `/var`.
        assert!(matches!(
            discovery.resolve(root.path(), "second-thread", &cancel),
            Ok(RolloutPath::Found(path))
                if studio_runtime_store::path_identity::same(&path, &appended)
        ));
    }

    #[test]
    fn over_limit_profile_is_cached_until_any_profile_file_change() {
        let root = tempfile::tempdir().expect("profile");
        let sessions = root.path().join("sessions");
        fs::create_dir_all(&sessions).expect("sessions directory");
        for thread in ["blocked-thread", "unrelated-one", "unrelated-two"] {
            fs::write(sessions.join(format!("rollout-{thread}.jsonl")), b"{}")
                .expect("rollout file");
        }

        let mut discovery = DiscoveryRound::with_entry_limits(3, usize::MAX);
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            discovery.resolve(root.path(), "blocked-thread", &cancel),
            Err(HistoryError::DiscoveryLimit)
        ));
        let visits = discovery.entries;
        assert!(matches!(
            discovery.resolve(root.path(), "another-thread", &cancel),
            Err(HistoryError::DiscoveryLimit)
        ));
        assert_eq!(
            discovery.entries, visits,
            "cached failure avoids another scan"
        );

        let removed = sessions.join("rollout-unrelated-two.jsonl");
        fs::remove_file(&removed).expect("remove unrelated rollout");
        discovery.file_changed(root.path(), &removed, true, true);
        assert!(matches!(
            discovery.resolve(root.path(), "blocked-thread", &cancel),
            Ok(RolloutPath::Found(path)) if path.ends_with("rollout-blocked-thread.jsonl")
        ));
    }

    #[test]
    fn round_budget_exhaustion_resets_without_permanently_blocking_a_valid_profile() {
        let root = tempfile::tempdir().expect("profiles");
        let first = root.path().join("first");
        let second = root.path().join("second");
        for profile in [&first, &second] {
            fs::create_dir_all(profile.join("sessions")).expect("sessions directory");
            fs::write(profile.join("sessions/rollout-thread.jsonl"), b"{}").expect("rollout file");
        }

        let mut discovery = DiscoveryRound::with_entry_limits(8, 3);
        let cancel = AtomicBool::new(false);
        assert!(matches!(
            discovery.resolve(&first, "thread", &cancel),
            Ok(RolloutPath::Found(_))
        ));
        assert!(matches!(
            discovery.resolve(&second, "thread", &cancel),
            Err(HistoryError::DiscoveryRoundLimit)
        ));
        assert!(matches!(
            discovery.resolve(&second, "thread", &cancel),
            Err(HistoryError::DiscoveryRoundLimit)
        ));
        discovery.begin_round();
        assert!(matches!(
            discovery.resolve(&second, "thread", &cancel),
            Ok(RolloutPath::Found(_))
        ));
    }
}
