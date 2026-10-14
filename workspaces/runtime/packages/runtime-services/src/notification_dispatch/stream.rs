//! Pure, bounded connection stream state. SQLite commit controls when updates apply.
use super::{
    NotificationDispatchError, NotificationHourMetrics, NotificationObservation,
    NotificationPostCommit, NotificationPublicationBatch, NotificationStreamMetrics,
};
use serde::{Deserialize, Serialize};
use serde_json::Value;

const MAX_STREAMS: usize = 8_192;
const MAX_PENDING_TEXT_BYTES: usize = 64 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(super) struct StreamKey {
    #[serde(default)]
    pub agent_id: String,
    pub account: String,
    pub generation: u64,
    pub thread: String,
    pub turn: Option<String>,
    pub item: String,
    pub method: String,
}

impl PartialEq for StreamKey {
    fn eq(&self, other: &Self) -> bool {
        self.account == other.account
            && self.generation == other.generation
            && self.thread == other.thread
            && self.turn == other.turn
            && self.item == other.item
            && self.method == other.method
    }
}

impl Eq for StreamKey {}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
struct Entry {
    key: StreamKey,
    flushed: bool,
    base: Option<String>,
    pending: String,
    count: u64,
    #[serde(default)]
    characters: u64,
    params: Value,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct NotificationStreamBuffer {
    entries: Vec<Entry>,
    closed: Vec<StreamKey>,
    #[serde(default)]
    analytics: Vec<(
        StreamKey,
        NotificationObservation,
        NotificationStreamMetrics,
    )>,
    #[serde(default)]
    post_commit: Vec<NotificationPostCommit>,
    #[serde(default)]
    publication_sequence: u64,
    #[serde(default)]
    analytics_overflow: u64,
    #[serde(default)]
    analytics_skipped: Vec<StreamKey>,
}

pub(super) enum EnqueueResult {
    Buffered,
    Closed,
    Full,
    FlushRequired,
}

impl NotificationStreamBuffer {
    pub(crate) fn boundary_identity(&self) -> String {
        let mut keys = self
            .entries
            .iter()
            .map(|entry| {
                format!(
                    "{}:{}:{}:{}:{}:{}",
                    entry.key.account,
                    entry.key.generation,
                    entry.key.thread,
                    entry.key.turn.as_deref().unwrap_or(""),
                    entry.key.item,
                    entry.key.method,
                )
            })
            .collect::<Vec<_>>();
        keys.sort();
        keys.join("|")
    }

    /// Take staged timer-flush effects from the returned buffer copy. Journal
    /// them with that buffer in the same transaction; publish only after commit.
    /// Discard both the returned buffer and these values on rollback.
    pub fn take_post_commit(
        &mut self,
    ) -> Result<NotificationPublicationBatch, NotificationDispatchError> {
        if self.post_commit.is_empty() && self.analytics_overflow == 0 {
            return Ok(NotificationPublicationBatch::default());
        }
        self.publication_sequence = self
            .publication_sequence
            .checked_add(1)
            .ok_or(NotificationDispatchError::InvalidNotification)?;
        Ok(NotificationPublicationBatch {
            sequence: self.publication_sequence,
            effects: std::mem::take(&mut self.post_commit),
            analytics_overflow: std::mem::take(&mut self.analytics_overflow),
        })
    }
    pub(super) fn queue_effect(&mut self, effect: NotificationPostCommit) {
        // As in Python, excess analytics captures are dropped, counted and never
        // allowed to refuse a notification, lose a receipt or cancel a session.
        // Counts match Python's 64-capture queue. Rust measures retained JSON
        // bytes (8 MiB), rather than CPython heap object sizes.
        if matches!(
            effect,
            NotificationPostCommit::Analytics { .. }
                | NotificationPostCommit::AnalyticsStream { .. }
        ) {
            let count = self
                .post_commit
                .iter()
                .filter(|effect| {
                    matches!(
                        effect,
                        NotificationPostCommit::Analytics { .. }
                            | NotificationPostCommit::AnalyticsStream { .. }
                    )
                })
                .count();
            let bytes: usize = self
                .post_commit
                .iter()
                .filter(|effect| {
                    matches!(
                        effect,
                        NotificationPostCommit::Analytics { .. }
                            | NotificationPostCommit::AnalyticsStream { .. }
                    )
                })
                .map(|effect| serde_json::to_vec(effect).map_or(usize::MAX, |bytes| bytes.len()))
                .fold(0, usize::saturating_add);
            let size = serde_json::to_vec(&effect).map_or(usize::MAX, |bytes| bytes.len());
            if count >= 64 || bytes.saturating_add(size) > 8 * 1024 * 1024 {
                self.analytics_overflow = self.analytics_overflow.saturating_add(1);
                return;
            }
        }
        self.post_commit.push(effect);
    }
    pub(super) fn stage_analytics(
        &mut self,
        key: StreamKey,
        mut observation: NotificationObservation,
    ) -> Result<(), NotificationDispatchError> {
        if self.closed.contains(&key) || self.analytics_skipped.contains(&key) {
            return Ok(());
        }
        let delta = observation.params["delta"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        let encoded_bytes =
            studio_runtime_store::python_json_compact(&observation.params).len() as u64;
        let hour = ((observation.observed_at / 3600.0).floor() * 3600.0) as u64;
        observation.agent = None;
        if let Some((_, saved, metrics)) = self.analytics.iter_mut().find(|(old, _, _)| old == &key)
        {
            let mut text = saved.params["delta"]
                .as_str()
                .unwrap_or_default()
                .to_owned();
            text.push_str(&delta);
            saved.params["delta"] =
                Value::String(if key.method == "item/commandExecution/outputDelta" {
                    super::suffix(&text, 12_000)
                } else {
                    text
                });
            metrics.bytes = metrics.bytes.saturating_add(delta.len() as u64);
            metrics.characters = metrics
                .characters
                .saturating_add(delta.chars().count() as u64);
            metrics.lines = metrics
                .lines
                .saturating_add(delta.bytes().filter(|byte| *byte == b'\n').count() as u64);
            if let Some(bucket) = metrics.hours.iter_mut().find(|bucket| bucket.hour == hour) {
                bucket.count = bucket.count.saturating_add(1);
                bucket.encoded_bytes = bucket.encoded_bytes.saturating_add(encoded_bytes);
            } else {
                metrics.hours.push(NotificationHourMetrics {
                    hour,
                    count: 1,
                    encoded_bytes,
                });
            }
        } else {
            if key.method == "item/commandExecution/outputDelta" {
                observation.params["delta"] = Value::String(super::suffix(&delta, 12_000));
            }
            self.analytics.push((
                key.clone(),
                observation,
                NotificationStreamMetrics {
                    hours: vec![NotificationHourMetrics {
                        hour,
                        count: 1,
                        encoded_bytes,
                    }],
                    bytes: delta.len() as u64,
                    characters: delta.chars().count() as u64,
                    lines: delta.bytes().filter(|byte| *byte == b'\n').count() as u64,
                },
            ));
        }
        let retained_bytes = self
            .analytics
            .iter()
            .map(|(_, observation, metrics)| {
                observation.params.to_string().len().saturating_add(
                    metrics
                        .hours
                        .len()
                        .saturating_mul(std::mem::size_of::<NotificationHourMetrics>()),
                )
            })
            .fold(0usize, usize::saturating_add);
        if retained_bytes > 8 * 1024 * 1024 {
            // Drop this whole analytics capture, never its transcript or receipt.
            // Suppress its later samples until flush instead of counting each as
            // a separate overflowed capture.
            self.analytics
                .retain(|(sample_key, _, _)| sample_key != &key);
            self.analytics_skipped.push(key);
            self.analytics_overflow = self.analytics_overflow.saturating_add(1);
        }
        Ok(())
    }
    pub(super) fn flush_analytics(&mut self, key: &StreamKey, agent: &Value) {
        self.analytics_skipped
            .retain(|sample_key| sample_key != key);
        let mut pending = Vec::new();
        for (sample_key, mut observation, metrics) in std::mem::take(&mut self.analytics) {
            if &sample_key == key {
                observation.agent = Some(agent.clone());
                self.queue_effect(NotificationPostCommit::AnalyticsStream {
                    observation,
                    metrics,
                });
            } else {
                pending.push((sample_key, observation, metrics));
            }
        }
        self.analytics = pending;
    }
    pub(crate) fn pending_bytes(&self) -> usize {
        self.entries
            .iter()
            .map(|entry| entry.pending.len().max(usize::from(entry.count > 0)))
            .sum()
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
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect()
    }

    pub(super) fn has_pending(&self, key: &StreamKey) -> bool {
        self.entries
            .iter()
            .any(|entry| &entry.key == key && entry.count > 0)
    }

    pub(super) fn has_base(&self, key: &StreamKey) -> bool {
        self.entries
            .iter()
            .any(|entry| &entry.key == key && entry.base.is_some())
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
    pub(super) fn set_flushed_base(&mut self, key: &StreamKey, hash: String) {
        if let Some(entry) = self.entries.iter_mut().find(|entry| &entry.key == key) {
            entry.base = Some(hash);
            entry.flushed = true;
        }
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
                characters: 0,
                params: params.clone(),
            });
            self.entries.len() - 1
        };
        let entry = &mut self.entries[index];
        if entry.count == 0 {
            entry.params = params.clone();
            // Delta text belongs only to pending, never to a second retained copy.
            if let Some(params) = entry.params.as_object_mut() {
                params.remove("delta");
            }
        }
        entry.count += 1;
        entry.characters = entry
            .characters
            .saturating_add(delta.chars().count() as u64);
        if entry.key.method == "item/agentMessage/delta"
            && entry.pending.len().saturating_add(delta.len()) >= MAX_PENDING_TEXT_BYTES
        {
            // The adapter writes pending plus this borrowed delta in one flush.
            // Even one oversized frame never enters the retained stream buffer.
            return EnqueueResult::FlushRequired;
        }
        entry.pending.push_str(delta);
        if entry.key.method == "item/commandExecution/outputDelta" {
            entry.pending = super::suffix(&entry.pending, 12_000);
        }
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
        self.flush_with_delta(key, current_text, None)
    }

    pub(super) fn flush_with_delta(
        &mut self,
        key: &StreamKey,
        current_text: &str,
        delta: Option<&str>,
    ) -> Option<(String, u64, Value)> {
        let entry = self.entries.iter_mut().find(|entry| &entry.key == key)?;
        if entry.count == 0 {
            return None;
        }
        entry.flushed = true;
        entry.base = None;
        let mut text = current_text.to_owned();
        text.push_str(&std::mem::take(&mut entry.pending));
        if let Some(delta) = delta {
            text.push_str(delta);
        }
        if key.method == "item/commandExecution/outputDelta" {
            text = super::suffix(&text, 12_000);
        }
        let count = std::mem::take(&mut entry.count);
        let mut params = std::mem::take(&mut entry.params);
        params["_streamCharacters"] = Value::from(std::mem::take(&mut entry.characters));
        Some((text, count, params))
    }

    pub(super) fn close(&mut self, key: &StreamKey) {
        self.analytics
            .retain(|(sample_key, _, _)| sample_key != key);
        self.analytics_skipped
            .retain(|sample_key| sample_key != key);
        self.entries.retain(|entry| &entry.key != key);
        if !self.closed.contains(key) {
            self.closed.push(key.clone());
        }
        if self.closed.len() > MAX_STREAMS {
            self.closed.remove(0);
        }
    }

    pub(super) fn discard(&mut self, key: &StreamKey) {
        self.analytics
            .retain(|(sample_key, _, _)| sample_key != key);
        self.analytics_skipped
            .retain(|sample_key| sample_key != key);
        self.entries.retain(|entry| &entry.key != key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legacy_closed_stream_matches_owner_enriched_key() {
        let mut streams: NotificationStreamBuffer = serde_json::from_value(serde_json::json!({
            "entries": [],
            "closed": [{
                "account": "default",
                "generation": 1,
                "thread": "thread",
                "turn": "turn",
                "item": "answer",
                "method": "item/agentMessage/delta"
            }]
        }))
        .expect("legacy stream metadata");
        let key = StreamKey {
            agent_id: "agent".into(),
            account: "default".into(),
            generation: 1,
            thread: "thread".into(),
            turn: Some("turn".into()),
            item: "answer".into(),
            method: "item/agentMessage/delta".into(),
        };
        assert!(matches!(
            streams.enqueue_delta(key, "late text", &Value::Null),
            EnqueueResult::Closed
        ));
    }

    #[test]
    fn account_worker_command_output_keeps_python_character_tail() {
        let key = StreamKey {
            agent_id: "agent".into(),
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
