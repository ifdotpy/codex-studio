//! Native PROGRESS.md change detection for active panel subscriptions.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use thiserror::Error;

use crate::change_notifications::ChangeNotificationHub;
use crate::progress_panel::{self, ProgressPanelError, ProgressSignature};

#[derive(Debug, Error)]
pub enum ProgressWatchError {
    #[error("progress file watcher could not start")]
    Watcher(#[from] notify::Error),
    #[error("progress file watch path could not be prepared")]
    Panel(#[from] ProgressPanelError),
    #[error("progress file watching requires native Linux inotify or macOS kqueue")]
    UnsupportedPlatform,
}

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
struct AgentKey {
    state_dir: PathBuf,
    id: String,
}

struct ProgressSubscriber {
    agents: BTreeSet<AgentKey>,
    changes: ChangeNotificationHub,
}

#[derive(Default)]
struct WatcherState {
    directories: HashMap<PathBuf, AgentKey>,
    agent_directories: HashMap<AgentKey, PathBuf>,
    agent_references: HashMap<AgentKey, usize>,
    signatures: HashMap<AgentKey, ProgressSignature>,
    subscribers: HashMap<u64, ProgressSubscriber>,
}

struct WatcherService {
    state: Arc<Mutex<WatcherState>>,
    watcher: Mutex<RecommendedWatcher>,
    subscription_lock: Mutex<()>,
}

static NEXT_SUBSCRIBER_ID: AtomicU64 = AtomicU64::new(1);

fn watcher_service() -> &'static Mutex<Option<Arc<WatcherService>>> {
    static SERVICE: OnceLock<Mutex<Option<Arc<WatcherService>>>> = OnceLock::new();
    SERVICE.get_or_init(|| Mutex::new(None))
}

/// Keeps a reference-counted subscription on the process-wide native watcher.
pub struct ProgressFileWatch {
    service: Weak<WatcherService>,
    subscriber_id: u64,
}

impl Drop for ProgressFileWatch {
    fn drop(&mut self) {
        let Some(service) = self.service.upgrade() else {
            return;
        };
        service.unsubscribe(self.subscriber_id);
    }
}

/// Subscribe active panels to the one process-wide native watcher.
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

        let state_dir = if state_dir.is_absolute() {
            state_dir.to_owned()
        } else {
            std::env::current_dir()
                .map_err(ProgressPanelError::from)?
                .join(state_dir)
        };
        let mut agent_directories = HashMap::<AgentKey, PathBuf>::new();
        for agent in agents {
            let directory = progress_panel::prepare_watch_directory(&state_dir, agent)?;
            agent_directories.insert(
                AgentKey {
                    state_dir: state_dir.clone(),
                    id: agent.clone(),
                },
                directory,
            );
        }
        let service = get_or_create_service()?;
        Ok(Some(service.subscribe(agent_directories, changes)?))
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

fn matching_agents(event: &Event, directories: &HashMap<PathBuf, AgentKey>) -> BTreeSet<AgentKey> {
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

fn read_signature(agent: &AgentKey) -> Option<ProgressSignature> {
    progress_panel::read_progress_signature(&agent.state_dir, &agent.id).ok()
}

fn get_or_create_service() -> Result<Arc<WatcherService>, ProgressWatchError> {
    let mut service_slot = watcher_service()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(service) = service_slot.as_ref() {
        return Ok(Arc::clone(service));
    }

    let state = Arc::new(Mutex::new(WatcherState::default()));
    let event_state = Arc::clone(&state);
    let watcher = notify::recommended_watcher(move |result: notify::Result<Event>| {
        let Ok(event) = result else {
            return;
        };
        if !is_file_change(&event.kind) {
            return;
        }
        let agents = {
            let state = event_state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            matching_agents(&event, &state.directories)
        };
        for agent in agents {
            let Some(signature) = read_signature(&agent) else {
                continue;
            };
            let subscribers = {
                let mut state = event_state
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if state.signatures.get(&agent) == Some(&signature) {
                    continue;
                }
                state.signatures.insert(agent.clone(), signature);
                state
                    .subscribers
                    .values()
                    .filter(|subscriber| subscriber.agents.contains(&agent))
                    .map(|subscriber| subscriber.changes.clone())
                    .collect::<Vec<_>>()
            };
            for changes in subscribers {
                changes.notify_one();
            }
        }
    })?;
    let service = Arc::new(WatcherService {
        state,
        watcher: Mutex::new(watcher),
        subscription_lock: Mutex::new(()),
    });
    *service_slot = Some(Arc::clone(&service));
    Ok(service)
}

impl WatcherService {
    fn subscribe(
        self: &Arc<Self>,
        agent_directories: HashMap<AgentKey, PathBuf>,
        changes: ChangeNotificationHub,
    ) -> Result<ProgressFileWatch, ProgressWatchError> {
        self.subscribe_with_registration_hook(agent_directories, changes, || {})
    }

    fn subscribe_with_registration_hook(
        self: &Arc<Self>,
        agent_directories: HashMap<AgentKey, PathBuf>,
        changes: ChangeNotificationHub,
        after_registration: impl FnOnce(),
    ) -> Result<ProgressFileWatch, ProgressWatchError> {
        let _subscription = self
            .subscription_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let new_agents = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            agent_directories
                .iter()
                .filter(|(agent, _)| !state.agent_references.contains_key(*agent))
                .map(|(agent, directory)| (agent.clone(), directory.clone()))
                .collect::<Vec<_>>()
        };

        let mut newly_watched = Vec::<PathBuf>::new();
        {
            let mut watcher = self
                .watcher
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            for (_, directory) in &new_agents {
                if let Err(error) = watcher.watch(directory, RecursiveMode::NonRecursive) {
                    for watched in newly_watched {
                        if let Err(unwatch_error) = watcher.unwatch(&watched) {
                            eprintln!("Unable to roll back progress watch: {unwatch_error}");
                        }
                    }
                    return Err(error.into());
                }
                newly_watched.push(directory.clone());
            }
        }

        // Keep a pre-registration signature so a change that lands before the
        // subscriber is visible can be detected by the post-registration read.
        let signatures = new_agents
            .iter()
            .filter_map(|(agent, _)| {
                read_signature(agent).map(|signature| (agent.clone(), signature))
            })
            .collect::<HashMap<_, _>>();
        let subscriber_id = NEXT_SUBSCRIBER_ID.fetch_add(1, Ordering::Relaxed);
        let agents = agent_directories.keys().cloned().collect::<BTreeSet<_>>();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for (agent, directory) in agent_directories {
            if !state.agent_references.contains_key(&agent) {
                state.directories.insert(directory.clone(), agent.clone());
                state.agent_directories.insert(agent.clone(), directory);
                if let Some(signature) = signatures.get(&agent) {
                    state.signatures.insert(agent.clone(), signature.clone());
                }
            }
            *state.agent_references.entry(agent).or_default() += 1;
        }
        state
            .subscribers
            .insert(subscriber_id, ProgressSubscriber { agents, changes });
        drop(state);

        // The native watcher and subscriber are now registered. Re-read to
        // close the gap where a filesystem event could have been dropped.
        after_registration();
        let current_signatures = new_agents
            .iter()
            .filter_map(|(agent, _)| {
                read_signature(agent).map(|signature| (agent.clone(), signature))
            })
            .collect::<HashMap<_, _>>();
        let changed_subscribers = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let changed_agents = current_signatures
                .into_iter()
                .filter_map(|(agent, signature)| {
                    if state.signatures.get(&agent) != signatures.get(&agent) {
                        // An event callback already advanced this signature while
                        // the post-registration read was in progress.
                        return None;
                    }
                    if state.signatures.get(&agent) == Some(&signature) {
                        return None;
                    }
                    state.signatures.insert(agent.clone(), signature);
                    Some(agent)
                })
                .collect::<BTreeSet<_>>();
            state
                .subscribers
                .values()
                .filter(|subscriber| !subscriber.agents.is_disjoint(&changed_agents))
                .map(|subscriber| subscriber.changes.clone())
                .collect::<Vec<_>>()
        };
        for changes in changed_subscribers {
            changes.notify_one();
        }
        Ok(ProgressFileWatch {
            service: Arc::downgrade(self),
            subscriber_id,
        })
    }

    fn unsubscribe(&self, subscriber_id: u64) {
        let _subscription = self
            .subscription_lock
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let directories = {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(subscriber) = state.subscribers.remove(&subscriber_id) else {
                return;
            };
            let mut directories = Vec::new();
            for agent in subscriber.agents {
                let Some(references) = state.agent_references.get_mut(&agent) else {
                    continue;
                };
                *references -= 1;
                if *references == 0 {
                    state.agent_references.remove(&agent);
                    state.signatures.remove(&agent);
                    if let Some(directory) = state.agent_directories.remove(&agent) {
                        state.directories.remove(&directory);
                        directories.push(directory);
                    }
                }
            }
            directories
        };
        let mut watcher = self
            .watcher
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        for directory in directories {
            if let Err(error) = watcher.unwatch(&directory) {
                eprintln!("Unable to release progress watch: {error}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::sync::mpsc;

    use notify::{Event, EventKind};

    use super::{
        AgentKey, ProgressSignature, get_or_create_service, matching_agents, watch_progress_files,
    };
    use crate::change_notifications::ChangeNotificationHub;

    #[test]
    fn progress_file_events_match_only_the_watched_parent_and_name() {
        let directory = PathBuf::from("/state/progress/agent-a");
        let agent = AgentKey {
            state_dir: PathBuf::from("/state"),
            id: "agent-a".to_owned(),
        };
        let directories = HashMap::from([(directory.clone(), agent.clone())]);
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
            std::slice::from_ref(&agent)
        );
        assert!(matching_agents(&other_file, &directories).is_empty());
        assert_eq!(
            matching_agents(&parent_event, &directories)
                .into_iter()
                .collect::<Vec<_>>(),
            [agent]
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

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn streams_share_one_watcher_and_reference_count_agent_subscriptions() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let agent = AgentKey {
            state_dir: directory.path().to_owned(),
            id: "agent-a".to_owned(),
        };
        let first = watch_progress_files(
            directory.path(),
            &["agent-a".to_owned()],
            ChangeNotificationHub::new(),
        )
        .expect("first watcher subscription")
        .expect("active panel has a watcher");
        let second = watch_progress_files(
            directory.path(),
            &["agent-a".to_owned()],
            ChangeNotificationHub::new(),
        )
        .expect("second watcher subscription")
        .expect("active panel has a watcher");
        let first_service = first.service.upgrade().expect("watcher service is live");
        let second_service = second.service.upgrade().expect("watcher service is live");
        assert!(std::sync::Arc::ptr_eq(&first_service, &second_service));
        let other_root = tempfile::tempdir().expect("second state directory");
        let other_agent = AgentKey {
            state_dir: other_root.path().to_owned(),
            id: "agent-a".to_owned(),
        };
        let third = watch_progress_files(
            other_root.path(),
            &["agent-a".to_owned()],
            ChangeNotificationHub::new(),
        )
        .expect("third watcher subscription")
        .expect("second root has a watcher");
        let third_service = third.service.upgrade().expect("watcher service is live");
        assert!(std::sync::Arc::ptr_eq(&first_service, &third_service));
        assert_eq!(
            first_service
                .state
                .lock()
                .expect("watcher state")
                .agent_references
                .get(&agent),
            Some(&2)
        );
        assert_eq!(
            first_service
                .state
                .lock()
                .expect("watcher state")
                .agent_references
                .get(&other_agent),
            Some(&1)
        );

        drop(first);
        assert_eq!(
            first_service
                .state
                .lock()
                .expect("watcher state")
                .agent_references
                .get(&agent),
            Some(&1)
        );
        drop(second);
        let state = first_service.state.lock().expect("watcher state");
        assert!(!state.agent_references.contains_key(&agent));
        assert!(!state.signatures.contains_key(&agent));
        assert!(state.agent_references.contains_key(&other_agent));
        drop(state);
        let third_directory = other_root.path().join("progress/agent-a");
        let (cleanup_complete, cleanup_finished) = mpsc::channel();
        let cleanup = std::thread::spawn(move || {
            drop(third);
            cleanup_complete
                .send(())
                .expect("cleanup waiter is still listening");
        });
        cleanup_finished
            .recv()
            .expect("watcher cleanup completes before the subscription is dropped");
        cleanup
            .join()
            .expect("watcher cleanup thread exits cleanly");
        assert!(
            !first_service
                .state
                .lock()
                .expect("watcher state")
                .directories
                .contains_key(&third_directory),
            "dropping the last subscription removes its watched directory"
        );
    }

    #[cfg(any(target_os = "linux", target_os = "macos"))]
    #[test]
    fn subscription_rereads_progress_after_registration() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let agent_directory = crate::progress_panel::prepare_watch_directory(
            directory.path(),
            "registration-race-agent",
        )
        .expect("watch directory is prepared");
        let progress_file = agent_directory.join("PROGRESS.md");
        std::fs::write(&progress_file, "before\n").expect("initial progress file is written");
        let agent = AgentKey {
            state_dir: directory.path().to_owned(),
            id: "registration-race-agent".to_owned(),
        };
        let agents = HashMap::from([(agent, agent_directory)]);
        let service = get_or_create_service().expect("process watcher starts");
        let changes = ChangeNotificationHub::new();
        let (registered, registration_gate) = mpsc::channel();
        let (release, release_gate) = mpsc::channel();
        let worker = {
            let service = service.clone();
            let changes = changes.clone();
            std::thread::spawn(move || {
                service.subscribe_with_registration_hook(agents, changes, || {
                    registered.send(()).expect("registration gate is open");
                    release_gate.recv().expect("release registration read");
                })
            })
        };

        registration_gate
            .recv()
            .expect("subscriber is registered before the post-read");
        std::fs::write(&progress_file, "after\n")
            .expect("progress changes in the registration window");
        release
            .send(())
            .expect("post-registration read is released");
        let subscription = worker
            .join()
            .expect("subscription worker joins")
            .expect("subscription succeeds");

        assert_eq!(changes.snapshot().revision, 1);
        drop(subscription);
    }
}
