//! Native PROGRESS.md change detection for active panel subscriptions.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use serde_json::Value;
use thiserror::Error;

use crate::change_notifications::ChangeNotificationHub;
use crate::progress_panel::{self, ProgressPanelError};

#[derive(Debug, Error)]
pub enum ProgressWatchError {
    #[error("progress file watcher could not start")]
    Watcher(#[from] notify::Error),
    #[error("progress file watch path could not be prepared")]
    Panel(#[from] ProgressPanelError),
    #[error("progress file watching requires native Linux inotify or macOS kqueue")]
    UnsupportedPlatform,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ProgressSignature {
    revision: Option<String>,
    error: Option<String>,
    exists: bool,
}

/// Keeps native watches alive for the lifetime of one SSE resource subscription.
pub struct ProgressFileWatch {
    _watcher: RecommendedWatcher,
}

/// Watch each active panel's parent directory and publish changed file revisions.
pub fn watch_progress_files(
    state_dir: &Path,
    agents: &[String],
    changes: ChangeNotificationHub,
) -> Result<Option<ProgressFileWatch>, ProgressWatchError> {
    #[cfg(any(target_os = "linux", target_os = "macos"))]
    {
        if agents.is_empty() {
            return Ok(None);
        }

        let mut directories = HashMap::<PathBuf, String>::new();
        for agent in agents {
            let directory = progress_panel::prepare_watch_directory(state_dir, agent)?;
            directories.insert(directory, agent.clone());
        }
        let directories = Arc::new(directories);
        let signatures = Arc::new(Mutex::new(HashMap::<String, ProgressSignature>::new()));
        let event_directories = Arc::clone(&directories);
        let event_signatures = Arc::clone(&signatures);
        let root = state_dir.to_owned();
        let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
            let Ok(event) = result else {
                return;
            };
            if !is_file_change(&event.kind) {
                return;
            }
            let agents = matching_agents(&event, &event_directories);
            for agent in agents {
                let Some(signature) = read_signature(&root, &agent) else {
                    continue;
                };
                let changed = {
                    let mut current = event_signatures
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    current.insert(agent, signature.clone()).as_ref() != Some(&signature)
                };
                if changed {
                    changes.notify_one();
                }
            }
        })?;
        let mut watcher = watcher;
        for directory in directories.keys() {
            watcher.watch(directory, RecursiveMode::NonRecursive)?;
        }

        {
            let mut current = signatures
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for agent in directories.values() {
                if let Some(signature) = read_signature(state_dir, agent) {
                    current.entry(agent.clone()).or_insert(signature);
                }
            }
        }
        Ok(Some(ProgressFileWatch { _watcher: watcher }))
    }

    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (state_dir, changes);
        if agents.is_empty() {
            Ok(None)
        } else {
            Err(ProgressWatchError::UnsupportedPlatform)
        }
    }
}

fn is_file_change(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

fn matching_agents(event: &Event, directories: &HashMap<PathBuf, String>) -> BTreeSet<String> {
    event
        .paths
        .iter()
        .filter_map(|path| {
            let parent = if path.file_name().is_some_and(|name| name == "PROGRESS.md") {
                path.parent()?
            } else {
                path
            };
            directories.get(parent).cloned()
        })
        .collect()
}

fn read_signature(state_dir: &Path, agent: &str) -> Option<ProgressSignature> {
    let panel = progress_panel::read_progress(state_dir, agent).ok()?;
    Some(ProgressSignature {
        revision: panel
            .get("revision")
            .and_then(Value::as_str)
            .map(str::to_owned),
        error: panel
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_owned),
        exists: panel.get("exists").and_then(Value::as_bool) == Some(true),
    })
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;

    use notify::{Event, EventKind};

    use super::{ProgressSignature, matching_agents};

    #[test]
    fn progress_file_events_match_only_the_watched_parent_and_name() {
        let directory = PathBuf::from("/state/progress/agent-a");
        let directories = HashMap::from([(directory.clone(), "agent-a".to_owned())]);
        let exact_file = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(directory.join("PROGRESS.md"));
        let other_file = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(directory.join("PROGRESS.layout.json"));
        let parent_event = Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
            .add_path(directory.clone());

        assert_eq!(
            matching_agents(&exact_file, &directories)
                .into_iter()
                .collect::<Vec<_>>(),
            ["agent-a"]
        );
        assert!(matching_agents(&other_file, &directories).is_empty());
        assert_eq!(
            matching_agents(&parent_event, &directories)
                .into_iter()
                .collect::<Vec<_>>(),
            ["agent-a"]
        );
    }

    #[test]
    fn progress_snapshot_tracks_the_python_revision_error_and_exists_tuple() {
        let before = ProgressSignature {
            revision: Some("1:2:3:4:5".to_owned()),
            error: None,
            exists: true,
        };
        let same = before.clone();
        let removed = ProgressSignature {
            revision: None,
            error: None,
            exists: false,
        };
        assert_eq!(before, same);
        assert_ne!(before, removed);
    }
}
