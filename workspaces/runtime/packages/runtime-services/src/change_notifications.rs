//! Latest-value fanout for committed changes at the HTTP boundary.

use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

pub const MAX_SAFE_REVISION: i64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeSnapshot {
    pub epoch: String,
    pub revision: i64,
    pub closed: bool,
}

/// A process-local post-commit change publisher backed by Tokio watch.
#[derive(Clone)]
pub struct ChangeNotificationHub {
    sender: watch::Sender<ChangeSnapshot>,
}

impl Default for ChangeNotificationHub {
    fn default() -> Self {
        Self::new()
    }
}

impl ChangeNotificationHub {
    pub fn new() -> Self {
        let (sender, _) = watch::channel(ChangeSnapshot {
            epoch: process_epoch(),
            revision: 0,
            closed: false,
        });
        Self { sender }
    }

    pub fn snapshot(&self) -> ChangeSnapshot {
        self.sender.borrow().clone()
    }

    /// Subscribe at the current revision. Each receiver tracks its own cursor.
    pub fn subscribe(&self) -> watch::Receiver<ChangeSnapshot> {
        self.sender.subscribe()
    }

    /// Publish a successful store commit or progress-file change.
    pub fn notify_one(&self) {
        self.sender.send_modify(|snapshot| {
            if !snapshot.closed {
                snapshot.revision = snapshot.revision.saturating_add(1).min(MAX_SAFE_REVISION);
            }
        });
    }

    /// Wake active streams and prevent later writes from publishing revisions.
    pub fn close(&self) {
        self.sender.send_modify(|snapshot| snapshot.closed = true);
    }
}

fn process_epoch() -> String {
    let started = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    format!("{:x}-{:x}", std::process::id(), started)
}

#[cfg(test)]
mod tests {
    use super::{ChangeNotificationHub, MAX_SAFE_REVISION};

    #[test]
    fn receivers_observe_latest_revision_without_a_bounded_queue() {
        let hub = ChangeNotificationHub::new();
        let mut first = hub.subscribe();
        let mut second = hub.subscribe();

        hub.notify_one();
        hub.notify_one();
        hub.notify_one();

        assert_eq!(first.borrow_and_update().revision, 3);
        assert_eq!(second.borrow_and_update().revision, 3);
        assert!(!first.has_changed().expect("hub sender remains open"));
        assert!(!second.has_changed().expect("hub sender remains open"));
    }

    #[test]
    fn revisions_saturate_and_closed_hubs_ignore_later_writes() {
        let hub = ChangeNotificationHub::new();
        hub.sender
            .send_modify(|snapshot| snapshot.revision = MAX_SAFE_REVISION);
        hub.notify_one();
        assert_eq!(hub.snapshot().revision, MAX_SAFE_REVISION);

        let mut receiver = hub.subscribe();
        hub.close();
        hub.notify_one();

        assert!(receiver.borrow_and_update().closed);
        assert_eq!(receiver.borrow().revision, MAX_SAFE_REVISION);
    }
}
