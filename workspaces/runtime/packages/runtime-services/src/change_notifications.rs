//! Synchronous fanout for committed changes at the HTTP boundary.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

type Listener = Arc<dyn Fn() + Send + Sync + 'static>;
pub const MAX_SAFE_REVISION: i64 = 9_007_199_254_740_991;

struct HubState {
    epoch: String,
    revision: i64,
    closed: bool,
    next_listener_id: u64,
    listeners: BTreeMap<u64, Listener>,
}

/// A process-local, post-commit change signal shared by route writers and SSE clients.
#[derive(Clone)]
pub struct ChangeNotificationHub {
    state: Arc<Mutex<HubState>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeSnapshot {
    pub epoch: String,
    pub revision: i64,
    pub closed: bool,
}

/// Removes one callback from its hub when the associated client disconnects.
pub struct ChangeSubscription {
    state: Weak<Mutex<HubState>>,
    listener_id: Option<u64>,
}

impl Default for ChangeNotificationHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeNotificationHub {
    pub fn new() -> Self {
        Self {
            state: Arc::new(Mutex::new(HubState {
                epoch: process_epoch(),
                revision: 0,
                closed: false,
                next_listener_id: 0,
                listeners: BTreeMap::new(),
            })),
        }
    }

    pub fn snapshot(&self) -> ChangeSnapshot {
        let state = self.lock();
        ChangeSnapshot {
            epoch: state.epoch.clone(),
            revision: state.revision,
            closed: state.closed,
        }
    }

    /// Register a non-blocking wake callback and capture the revision atomically.
    pub fn subscribe(
        &self,
        listener: impl Fn() + Send + Sync + 'static,
    ) -> (ChangeSnapshot, ChangeSubscription) {
        let listener: Listener = Arc::new(listener);
        let (snapshot, listener_id) = {
            let mut state = self.lock();
            let snapshot = ChangeSnapshot {
                epoch: state.epoch.clone(),
                revision: state.revision,
                closed: state.closed,
            };
            let listener_id = if state.closed {
                None
            } else {
                let listener_id = state.next_listener_id;
                state.next_listener_id = state.next_listener_id.saturating_add(1);
                state.listeners.insert(listener_id, Arc::clone(&listener));
                Some(listener_id)
            };
            (snapshot, listener_id)
        };

        if snapshot.closed {
            listener();
        }

        (
            snapshot,
            ChangeSubscription {
                state: Arc::downgrade(&self.state),
                listener_id,
            },
        )
    }

    /// Broadcast one successful post-commit write to every active subscriber.
    pub fn notify_one(&self) {
        let listeners = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.revision = state.revision.saturating_add(1).min(MAX_SAFE_REVISION);
            state.listeners.values().cloned().collect::<Vec<_>>()
        };
        for listener in listeners {
            listener();
        }
    }

    /// Wake active streams and prevent later writes from publishing new revisions.
    pub fn close(&self) {
        let listeners = {
            let mut state = self.lock();
            if state.closed {
                return;
            }
            state.closed = true;
            state.listeners.values().cloned().collect::<Vec<_>>()
        };
        for listener in listeners {
            listener();
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, HubState> {
        self.state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

fn process_epoch() -> String {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}", std::process::id(), started)
}

impl Drop for ChangeSubscription {
    fn drop(&mut self) {
        let Some(listener_id) = self.listener_id else {
            return;
        };
        let Some(state) = self.state.upgrade() else {
            return;
        };
        state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .listeners
            .remove(&listener_id);
    }
}
