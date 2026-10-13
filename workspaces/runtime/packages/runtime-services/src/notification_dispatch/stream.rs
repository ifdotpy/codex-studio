//! Bounded transient text-stream keys and pending deltas, keyed like Python's buffer.

use std::collections::{HashMap, HashSet, VecDeque};

const MAX_STREAMS: usize = 8_192;

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub(super) struct StreamKey {
    pub account: String,
    pub generation: u64,
    pub thread: String,
    pub turn: Option<String>,
    pub item: String,
    pub method: String,
}

#[derive(Clone, Debug, Default)]
struct Entry {
    pending: String,
}

#[derive(Clone, Debug, Default)]
pub struct NotificationStreamBuffer {
    entries: HashMap<StreamKey, Entry>,
    entry_order: VecDeque<StreamKey>,
    closed: HashSet<StreamKey>,
    closed_order: VecDeque<StreamKey>,
}

impl NotificationStreamBuffer {
    pub(super) fn enqueue_delta(&mut self, key: StreamKey, delta: &str) -> bool {
        if self.closed.contains(&key) {
            return false;
        }
        if !self.entries.contains_key(&key) {
            self.make_room();
            self.entry_order.push_back(key.clone());
            self.entries.insert(key.clone(), Entry::default());
        }
        let Some(entry) = self.entries.get_mut(&key) else {
            return false;
        };
        entry.pending.push_str(delta);
        true
    }

    pub(super) fn flush(&mut self, key: &StreamKey, current_text: &str) -> Option<String> {
        let entry = self.entries.get_mut(key)?;
        if entry.pending.is_empty() {
            return None;
        }
        let mut text = String::with_capacity(current_text.len() + entry.pending.len());
        text.push_str(current_text);
        text.push_str(&entry.pending);
        entry.pending.clear();
        Some(text)
    }

    pub(super) fn start(&mut self, key: &StreamKey) {
        if let Some(entry) = self.entries.get_mut(key) {
            entry.pending.clear();
        }
    }

    pub(super) fn complete(
        &mut self,
        key: &StreamKey,
        previous_text: &str,
        snapshot: Option<&str>,
    ) -> String {
        let Some(entry) = self.entries.remove(key) else {
            return snapshot
                .filter(|text| !text.is_empty())
                .unwrap_or(previous_text)
                .to_owned();
        };
        self.entry_order.retain(|candidate| candidate != key);
        let text = if let Some(snapshot) = snapshot.filter(|text| !text.is_empty()) {
            snapshot.to_owned()
        } else {
            let mut text = String::with_capacity(previous_text.len() + entry.pending.len());
            text.push_str(previous_text);
            text.push_str(&entry.pending);
            text
        };
        self.close_key(key.clone());
        text
    }

    fn make_room(&mut self) {
        while self.entries.len() >= MAX_STREAMS {
            let Some(oldest) = self.entry_order.pop_front() else {
                break;
            };
            self.entries.remove(&oldest);
        }
    }

    fn close_key(&mut self, key: StreamKey) {
        if self.closed.insert(key.clone()) {
            self.closed_order.push_back(key);
        }
        while self.closed.len() > MAX_STREAMS {
            if let Some(oldest) = self.closed_order.pop_front() {
                self.closed.remove(&oldest);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{MAX_STREAMS, NotificationStreamBuffer, StreamKey};

    fn key(generation: u64, item: &str, method: &str) -> StreamKey {
        StreamKey {
            account: "default".to_owned(),
            generation,
            thread: "thread".to_owned(),
            turn: Some("turn".to_owned()),
            item: item.to_owned(),
            method: method.to_owned(),
        }
    }

    #[test]
    fn flushed_delta_is_replaced_by_snapshot_and_closed_keys_are_generation_scoped() {
        let mut streams = NotificationStreamBuffer::default();
        let first = key(7, "answer", "item/agentMessage/delta");
        assert!(streams.enqueue_delta(first.clone(), "before"));
        assert_eq!(streams.flush(&first, "").as_deref(), Some("before"));
        assert!(streams.enqueue_delta(first.clone(), "pending"));
        streams.start(&first);
        assert_eq!(streams.complete(&first, "snapshot", Some("final")), "final");
        assert!(!streams.enqueue_delta(first, "late"));

        let next = key(8, "answer", "item/agentMessage/delta");
        assert!(streams.enqueue_delta(next.clone(), "late"));
        assert_eq!(streams.flush(&next, "final").as_deref(), Some("finallate"));
    }

    #[test]
    fn completion_without_a_stream_does_not_close_the_first_delta() {
        let mut streams = NotificationStreamBuffer::default();
        let key = key(7, "answer", "item/agentMessage/delta");
        assert_eq!(streams.complete(&key, "final", None), "final");
        assert!(streams.enqueue_delta(key, "late"));
    }

    #[test]
    fn stream_key_includes_account_generation_thread_turn_item_and_method() {
        let baseline = key(7, "answer", "item/agentMessage/delta");
        let mut other = baseline.clone();
        other.account = "another".to_owned();
        assert_ne!(baseline, other);
        let mut other = baseline.clone();
        other.generation = 8;
        assert_ne!(baseline, other);
        let mut other = baseline.clone();
        other.thread = "another-thread".to_owned();
        assert_ne!(baseline, other);
        let mut other = baseline.clone();
        other.turn = None;
        assert_ne!(baseline, other);
        let mut other = baseline.clone();
        other.item = "another-item".to_owned();
        assert_ne!(baseline, other);
        assert_ne!(
            baseline,
            key(7, "answer", "item/commandExecution/outputDelta")
        );
    }

    #[test]
    fn transient_stream_storage_is_bounded() {
        let mut streams = NotificationStreamBuffer::default();
        for index in 0..=MAX_STREAMS {
            assert!(
                streams.enqueue_delta(key(7, &index.to_string(), "item/agentMessage/delta"), "x")
            );
        }
        let oldest = key(7, "0", "item/agentMessage/delta");
        assert!(streams.flush(&oldest, "").is_none());
    }
}
