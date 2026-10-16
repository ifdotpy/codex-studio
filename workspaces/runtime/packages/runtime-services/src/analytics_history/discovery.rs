//! One bounded directory scan per profile, kept current by filesystem events.
use super::{HistoryError, RolloutPath};
use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};

const MAX_DISCOVERY_ENTRIES: usize = 65_536;
#[derive(Default)]
pub struct DiscoveryRound {
    profiles: BTreeMap<PathBuf, Option<Vec<PathBuf>>>,
    entries: usize,
}
impl DiscoveryRound {
    /// Keep a previously discovered profile current without rescanning its
    /// directory tree for each rollout append.
    pub fn file_changed(&mut self, profile: &Path, path: &Path, removed: bool) {
        let profile = canonical_or_absolute(profile);
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
        self.profiles.remove(&canonical_or_absolute(profile));
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
        let profile = fs::canonicalize(profile).map_err(|_| HistoryError::Read)?;
        if !self.profiles.contains_key(&profile) {
            // Empty profiles also consume the round's global memory/work budget.
            self.visit()?;
            let paths = self.scan(&profile, cancel)?;
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
    fn visit(&mut self) -> Result<(), HistoryError> {
        self.entries += 1;
        if self.entries > MAX_DISCOVERY_ENTRIES {
            return Err(HistoryError::DiscoveryLimit);
        }
        Ok(())
    }
    fn scan(
        &mut self,
        profile: &Path,
        cancel: &AtomicBool,
    ) -> Result<Option<Vec<PathBuf>>, HistoryError> {
        let mut directories = vec![profile.join("sessions"), profile.join("archived_sessions")];
        let mut paths = Vec::new();
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
                self.visit()?;
                if cancel.load(Ordering::Relaxed) {
                    return Err(HistoryError::Cancelled);
                }
                let entry = entry.map_err(|_| HistoryError::Read)?;
                let kind = entry.file_type().map_err(|_| HistoryError::Read)?;
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

fn canonical_or_absolute(path: &Path) -> PathBuf {
    if let Ok(path) = fs::canonicalize(path) {
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
        discovery.file_changed(root.path(), &appended, false);
        fs::remove_dir_all(&sessions).expect("remove scanned tree to prove no rescan");
        assert!(matches!(
            discovery.resolve(root.path(), "second-thread", &cancel),
            Ok(RolloutPath::Found(path)) if path == appended
        ));
    }
}
