//! Pure, bounded connection stream state. SQLite commit controls when updates apply.
use serde_json::Value;

const MAX_STREAMS: usize = 8_192;

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct StreamKey {
    pub account: String,
    pub generation: u64,
    pub thread: String,
    pub turn: Option<String>,
    pub item: String,
    pub method: String,
}

#[derive(Clone, Debug, PartialEq)]
struct Entry {
    key: StreamKey,
    base: Option<String>,
    pending: String,
    count: u64,
    params: Value,
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct NotificationStreamBuffer {
    entries: Vec<Entry>,
    closed: Vec<StreamKey>,
}

pub(super) enum EnqueueResult {
    Buffered,
    Closed,
    Full,
}

impl NotificationStreamBuffer {
    pub(super) fn enqueue_delta(
        &mut self,
        key: StreamKey,
        delta: &str,
        params: &Value,
    ) -> EnqueueResult {
        if self.closed.contains(&key) {
            return EnqueueResult::Closed;
        }
        let index = if let Some(index) = self.entries.iter().position(|entry| entry.key == key) {
            index
        } else {
            if self.entries.len() == MAX_STREAMS {
                return EnqueueResult::Full;
            }
            self.entries.push(Entry {
                key,
                base: None,
                pending: String::new(),
                count: 0,
                params: params.clone(),
            });
            self.entries.len() - 1
        };
        let entry = &mut self.entries[index];
        if entry.count == 0 {
            entry.params = params.clone();
        }
        entry.pending.push_str(delta);
        entry.count += 1;
        EnqueueResult::Buffered
    }

    pub(super) fn selected(&self, terminal: Option<&StreamKey>) -> Vec<StreamKey> {
        self.entries
            .iter()
            .filter(|entry| {
                terminal.is_none_or(|key| {
                    entry.key.account == key.account
                        && entry.key.thread == key.thread
                        && entry.key.item == key.item
                        && key
                            .turn
                            .as_ref()
                            .is_none_or(|turn| entry.key.turn.as_ref() == Some(turn))
                })
            })
            .map(|entry| entry.key.clone())
            .collect()
    }

    pub(super) fn flush(
        &mut self,
        key: &StreamKey,
        current_text: &str,
    ) -> Option<(String, u64, Value)> {
        let entry = self.entries.iter_mut().find(|entry| &entry.key == key)?;
        if entry.count == 0 {
            return None;
        }
        let text = entry.base.get_or_insert_with(|| current_text.to_owned());
        text.push_str(&entry.pending);
        entry.pending.clear();
        let count = std::mem::take(&mut entry.count);
        Some((text.clone(), count, entry.params.clone()))
    }

    pub(super) fn close(&mut self, key: &StreamKey) {
        self.entries.retain(|entry| &entry.key != key);
        if !self.closed.contains(key) {
            self.closed.push(key.clone());
        }
        if self.closed.len() > MAX_STREAMS {
            self.closed.remove(0);
        }
    }

    pub(super) fn discard(&mut self, key: &StreamKey) {
        self.entries.retain(|entry| &entry.key != key);
    }
}
