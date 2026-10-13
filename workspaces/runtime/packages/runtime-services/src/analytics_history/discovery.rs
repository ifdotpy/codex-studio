//! One bounded directory scan per profile, shared by a scheduler round's actors.
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
