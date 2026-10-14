//! Latest-value fanout for committed changes at the HTTP boundary.

use std::time::{SystemTime, UNIX_EPOCH};

use tokio::sync::watch;

pub const MAX_SAFE_REVISION: i64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangeSnapshot {
    pub epoch: String,
    pub revision: i64,
    /// Last broad invalidation; token and explicit-resource publications leave it unchanged.
    pub resource_revision: i64,
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
            resource_revision: 0,
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
        self.try_publish(1, true, |_| {});
    }

    /// Decide closure, advance the cursor and record publication under the same guard.
    /// The callback must not read this hub or block on another publisher.
    pub fn try_publish(
        &self,
        steps: u32,
        invalidate_all: bool,
        record: impl FnOnce(&ChangeSnapshot),
    ) -> bool {
        self.sender.send_if_modified(|snapshot| {
            if snapshot.closed {
                return false;
            }
            snapshot.revision = snapshot
                .revision
                .saturating_add(i64::from(steps))
                .min(MAX_SAFE_REVISION);
            if invalidate_all {
                snapshot.resource_revision = snapshot.revision;
            }
            record(snapshot);
            true
        })
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

#[cfg(test)]
mod publication_tests {
    use super::*;
    use std::sync::mpsc;

    #[test]
    fn publication_guard_finishes_record_before_racing_close() {
        let hub = ChangeNotificationHub::new();
        let (recording, record_started) = mpsc::channel();
        let (release, gate) = mpsc::channel();
        let publisher = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                hub.try_publish(1, false, |snapshot| {
                    assert!(!snapshot.closed);
                    recording.send(snapshot.revision).expect("record starts");
                    gate.recv().expect("release record");
                })
            })
        };
        assert_eq!(record_started.recv().expect("publication gate"), 1);
        let (closing, close_started) = mpsc::channel();
        let closer = {
            let hub = hub.clone();
            std::thread::spawn(move || {
                closing.send(()).expect("close starts");
                hub.close();
            })
        };
        close_started
            .recv()
            .expect("close raced with guarded record");
        release.send(()).expect("finish publication");
        assert!(publisher.join().expect("publisher"));
        closer.join().expect("closer");
        assert!(hub.snapshot().closed);
        assert_eq!(hub.snapshot().revision, 1);
        assert!(!hub.try_publish(1, false, |_| panic!("closed publication")));
    }
}
