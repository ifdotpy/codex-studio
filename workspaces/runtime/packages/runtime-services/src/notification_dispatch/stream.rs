//! Pure, bounded connection stream state. SQLite commit controls when updates apply.
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_STREAMS: usize = 8_192;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub(super) struct StreamKey {
    pub account: String,
    pub generation: u64,
    pub thread: String,
    pub turn: Option<String>,
    pub item: String,
    pub method: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    key: StreamKey,
    flushed: bool,
    base: Option<String>,
    pending: String,
    count: u64,
    params: Value,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
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
    pub(crate) fn pending_bytes(&self) -> usize {
        self.entries.iter().map(|entry| entry.pending.len()).sum()
    }
    pub(crate) fn threads(&self) -> Vec<(String, u64, String)> {
        self.entries
            .iter()
            .filter(|entry| entry.count > 0)
            .map(|entry| {
                (
                    entry.key.account.clone(),
                    entry.key.generation,
                    entry.key.thread.clone(),
                )
            })
            .collect()
    }

    pub(super) fn base(&self, key: &StreamKey) -> Option<&str> {
        self.entries
            .iter()
            .find(|entry| &entry.key == key)?
            .base
            .as_deref()
    }
    pub(super) fn preserve_base(&mut self, key: &StreamKey, hash: String) -> bool {
        if let Some(entry) = self.entries.iter_mut().find(|entry| &entry.key == key)
            && entry.flushed
            && entry.base.is_none()
        {
            entry.base = Some(hash);
            return true;
        }
        false
    }
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
                flushed: false,
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
        if entry.key.method == "item/commandExecution/outputDelta" {
            entry.pending = entry
                .pending
                .chars()
                .rev()
                .take(12_000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
        }
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
        entry.flushed = true;
        entry.base = None;
        let mut text = current_text.to_owned();
        text.push_str(&std::mem::take(&mut entry.pending));
        if key.method == "item/commandExecution/outputDelta" {
            text = text
                .chars()
                .rev()
                .take(12_000)
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
        }
        let count = std::mem::take(&mut entry.count);
        Some((text, count, std::mem::take(&mut entry.params)))
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

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn account_worker_command_output_keeps_python_character_tail() {
        let key = StreamKey {
            account: "default".into(),
            generation: 1,
            thread: "thread".into(),
            turn: Some("turn".into()),
            item: "command".into(),
            method: "item/commandExecution/outputDelta".into(),
        };
        let mut streams = NotificationStreamBuffer::default();
        let text = "界".repeat(13_000);
        assert!(matches!(
            streams.enqueue_delta(key.clone(), &text, &Value::Null),
            EnqueueResult::Buffered
        ));
        assert_eq!(streams.pending_bytes(), 12_000 * 3);
        let (text, _, _) = streams.flush(&key, "older").expect("buffered output");
        assert_eq!(text, "界".repeat(12_000));
    }
}
