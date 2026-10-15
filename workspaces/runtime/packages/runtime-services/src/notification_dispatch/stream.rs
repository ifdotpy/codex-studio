//! Pure, bounded connection stream state. Cloned buffers share immutable entries;
//! staged writes detach only the touched entry, and SQLite commit controls apply.
use super::{
    NotificationDispatchError, NotificationHourMetrics, NotificationObservation,
    NotificationPostCommit, NotificationPublicationBatch, NotificationStreamMetrics,
};
use std::ops::{Deref, DerefMut};
use std::{
    collections::{HashMap, HashSet},
    hash::{Hash, Hasher},
    sync::Arc,
};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
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

impl Hash for StreamKey {
    fn hash<H: Hasher>(&self, state: &mut H) {
        self.account.hash(state);
        self.generation.hash(state);
        self.thread.hash(state);
        self.turn.hash(state);
        self.item.hash(state);
        self.method.hash(state);
    }
}

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

#[derive(Clone, Debug, PartialEq)]
struct AnalyticsCapture {
    key: StreamKey,
    observation: NotificationObservation,
    metrics: NotificationStreamMetrics,
    retained_bytes: usize,
}

impl Serialize for AnalyticsCapture {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        (&self.key, &self.observation, &self.metrics).serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for AnalyticsCapture {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let (key, observation, metrics) = <(
            StreamKey,
            NotificationObservation,
            NotificationStreamMetrics,
        )>::deserialize(deserializer)?;
        let retained_bytes = NotificationStreamBuffer::capture_size(&observation, &metrics);
        Ok(Self {
            key,
            observation,
            metrics,
            retained_bytes,
        })
    }
}

/// Clone-shared state detaches only when the staged transaction mutates it.
/// Serialization stays transparent so persisted stream bytes keep their shape.
#[derive(Clone, Debug, PartialEq)]
struct Shared<T>(Arc<T>);

impl<T> Shared<T> {
    fn new(value: T) -> Self {
        Self(Arc::new(value))
    }

    fn into_owned(self) -> T
    where
        T: Clone,
    {
        Arc::try_unwrap(self.0).unwrap_or_else(|value| (*value).clone())
    }

    fn value(&self) -> &T {
        self.0.as_ref()
    }

    fn shares_allocation_with(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    fn value_mut(&mut self) -> &mut T
    where
        T: Clone,
    {
        Arc::make_mut(&mut self.0)
    }
}

fn shared_entries_changed<T>(current: &[Shared<T>], previous: &[Shared<T>]) -> bool {
    current.len() != previous.len()
        || current
            .iter()
            .zip(previous)
            .any(|(current, previous)| !current.shares_allocation_with(previous))
}

impl<T> Deref for Shared<T> {
    type Target = T;

    fn deref(&self) -> &Self::Target {
        self.0.as_ref()
    }
}

impl<T: Clone> DerefMut for Shared<T> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.value_mut()
    }
}

impl<T: Serialize> Serialize for Shared<T> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for Shared<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::new)
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
struct StreamIndexes {
    entries: HashMap<StreamKey, usize>,
    closed: HashSet<StreamKey>,
    analytics: HashMap<StreamKey, usize>,
    analytics_skipped: HashSet<StreamKey>,
}

#[derive(Clone, Debug, Default, PartialEq, Serialize)]
pub struct NotificationStreamBuffer {
    entries: Vec<Shared<Entry>>,
    closed: Vec<Shared<StreamKey>>,
    #[serde(default)]
    analytics: Vec<Shared<AnalyticsCapture>>,
    #[serde(default)]
    post_commit: Vec<Shared<NotificationPostCommit>>,
    #[serde(default)]
    publication_sequence: u64,
    #[serde(default)]
    analytics_overflow: u64,
    #[serde(default)]
    analytics_skipped: Vec<Shared<StreamKey>>,
    #[serde(skip)]
    indexes: Arc<StreamIndexes>,
    #[serde(skip)]
    pending_bytes: usize,
    #[serde(skip)]
    analytics_bytes: usize,
    #[serde(skip)]
    post_commit_analytics_count: usize,
    #[serde(skip)]
    post_commit_analytics_bytes: usize,
}

#[derive(Deserialize)]
struct PersistedNotificationStreamBuffer {
    #[serde(default)]
    entries: Vec<Shared<Entry>>,
    #[serde(default)]
    closed: Vec<Shared<StreamKey>>,
    #[serde(default)]
    analytics: Vec<Shared<AnalyticsCapture>>,
    #[serde(default)]
    post_commit: Vec<Shared<NotificationPostCommit>>,
    #[serde(default)]
    publication_sequence: u64,
    #[serde(default)]
    analytics_overflow: u64,
    #[serde(default)]
    analytics_skipped: Vec<Shared<StreamKey>>,
}

impl<'de> Deserialize<'de> for NotificationStreamBuffer {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let saved = PersistedNotificationStreamBuffer::deserialize(deserializer)?;
        let mut buffer = Self {
            entries: saved.entries,
            closed: saved.closed,
            analytics: saved.analytics,
            post_commit: saved.post_commit,
            publication_sequence: saved.publication_sequence,
            analytics_overflow: saved.analytics_overflow,
            analytics_skipped: saved.analytics_skipped,
            ..Self::default()
        };
        buffer.rebuild_runtime_state();
        Ok(buffer)
    }
}

pub(super) enum EnqueueResult {
    Buffered,
    Closed,
    Full,
    FlushRequired,
}

impl NotificationStreamBuffer {
    fn is_analytics_effect(effect: &NotificationPostCommit) -> bool {
        matches!(
            effect,
            NotificationPostCommit::Analytics { .. }
                | NotificationPostCommit::AnalyticsStream { .. }
        )
    }

    fn capture_size(
        observation: &NotificationObservation,
        metrics: &NotificationStreamMetrics,
    ) -> usize {
        observation.params.to_string().len().saturating_add(
            metrics
                .hours
                .len()
                .saturating_mul(std::mem::size_of::<NotificationHourMetrics>()),
        )
    }

    fn rebuild_runtime_state(&mut self) {
        let mut indexes = StreamIndexes::default();
        self.pending_bytes = 0;
        for (index, entry) in self.entries.iter().enumerate() {
            indexes.entries.insert(entry.key.clone(), index);
            self.pending_bytes = self
                .pending_bytes
                .saturating_add(entry.pending.len().max(usize::from(entry.count > 0)));
        }
        indexes
            .closed
            .extend(self.closed.iter().map(|key| (**key).clone()));
        indexes
            .analytics_skipped
            .extend(self.analytics_skipped.iter().map(|key| (**key).clone()));
        self.analytics_bytes = 0;
        for (index, capture) in self.analytics.iter().enumerate() {
            indexes.analytics.insert(capture.key.clone(), index);
            self.analytics_bytes = self.analytics_bytes.saturating_add(capture.retained_bytes);
        }
        self.post_commit_analytics_count = 0;
        self.post_commit_analytics_bytes = 0;
        for effect in &self.post_commit {
            if Self::is_analytics_effect(effect) {
                self.post_commit_analytics_count += 1;
                self.post_commit_analytics_bytes = self.post_commit_analytics_bytes.saturating_add(
                    serde_json::to_vec(effect.value()).map_or(usize::MAX, |v| v.len()),
                );
            }
        }
        self.indexes = Arc::new(indexes);
    }

    fn rebuild_indexes(&mut self) {
        let mut indexes = StreamIndexes::default();
        indexes.entries.extend(
            self.entries
                .iter()
                .enumerate()
                .map(|(index, entry)| (entry.key.clone(), index)),
        );
        indexes
            .closed
            .extend(self.closed.iter().map(|key| (**key).clone()));
        indexes.analytics.extend(
            self.analytics
                .iter()
                .enumerate()
                .map(|(index, capture)| (capture.key.clone(), index)),
        );
        indexes
            .analytics_skipped
            .extend(self.analytics_skipped.iter().map(|key| (**key).clone()));
        self.indexes = Arc::new(indexes);
    }

    #[cfg(test)]
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
        let effects = std::mem::take(&mut self.post_commit)
            .into_iter()
            .map(Shared::into_owned)
            .collect();
        self.post_commit_analytics_count = 0;
        self.post_commit_analytics_bytes = 0;
        Ok(NotificationPublicationBatch {
            sequence: self.publication_sequence,
            effects,
            analytics_overflow: std::mem::take(&mut self.analytics_overflow),
        })
    }
    pub(super) fn changed_from(&self, previous: &Self) -> bool {
        self.publication_sequence != previous.publication_sequence
            || self.analytics_overflow != previous.analytics_overflow
            || shared_entries_changed(&self.entries, &previous.entries)
            || shared_entries_changed(&self.closed, &previous.closed)
            || shared_entries_changed(&self.analytics, &previous.analytics)
            || shared_entries_changed(&self.post_commit, &previous.post_commit)
            || shared_entries_changed(&self.analytics_skipped, &previous.analytics_skipped)
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
            let size = serde_json::to_vec(&effect).map_or(usize::MAX, |bytes| bytes.len());
            if self.post_commit_analytics_count >= 64
                || self.post_commit_analytics_bytes.saturating_add(size) > 8 * 1024 * 1024
            {
                self.analytics_overflow = self.analytics_overflow.saturating_add(1);
                return;
            }
            self.post_commit_analytics_count += 1;
            self.post_commit_analytics_bytes =
                self.post_commit_analytics_bytes.saturating_add(size);
        }
        self.post_commit.push(Shared::new(effect));
    }

    pub(crate) fn queue_effects(&mut self, effects: Vec<NotificationPostCommit>) {
        for effect in effects {
            self.queue_effect(effect);
        }
    }
    pub(crate) fn stage_recovered_notification(
        &mut self,
        identity: String,
        observation: super::RecoveredNotificationObservation,
    ) {
        self.queue_effect(NotificationPostCommit::Recovered {
            identity,
            observation,
        });
    }
    pub(super) fn stage_analytics(
        &mut self,
        key: StreamKey,
        mut observation: NotificationObservation,
    ) -> Result<(), NotificationDispatchError> {
        if self.indexes.closed.contains(&key) || self.indexes.analytics_skipped.contains(&key) {
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
        if let Some(index) = self.indexes.analytics.get(&key).copied() {
            let (old_size, size) = {
                let capture = self.analytics[index].value_mut();
                let old_size = capture.retained_bytes;
                let saved = &mut capture.observation;
                let metrics = &mut capture.metrics;
                let old_delta = saved.params["delta"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned();
                let old_encoded_delta =
                    serde_json::to_string(&saved.params["delta"]).map_or(usize::MAX, |v| v.len());
                let mut text = old_delta.clone();
                text.push_str(&delta);
                if key.method == "item/commandExecution/outputDelta" {
                    text = super::suffix(&text, 12_000);
                }
                let new_encoded_delta =
                    serde_json::to_string(&text).map_or(usize::MAX, |v| v.len());
                saved.params["delta"] = Value::String(text);
                // Preserve the last included cursor while observed_at remains first-event time.
                saved.delivery = observation.delivery.clone();
                metrics.bytes = metrics.bytes.saturating_add(delta.len() as u64);
                metrics.characters = metrics
                    .characters
                    .saturating_add(delta.chars().count() as u64);
                metrics.lines = metrics
                    .lines
                    .saturating_add(delta.bytes().filter(|byte| *byte == b'\n').count() as u64);
                let new_hour = if let Some(bucket) =
                    metrics.hours.iter_mut().find(|bucket| bucket.hour == hour)
                {
                    bucket.count = bucket.count.saturating_add(1);
                    bucket.encoded_bytes = bucket.encoded_bytes.saturating_add(encoded_bytes);
                    false
                } else {
                    metrics.hours.push(NotificationHourMetrics {
                        hour,
                        count: 1,
                        encoded_bytes,
                    });
                    true
                };
                let size = old_size
                    .saturating_sub(old_encoded_delta)
                    .saturating_add(new_encoded_delta)
                    .saturating_add(if new_hour {
                        std::mem::size_of::<NotificationHourMetrics>()
                    } else {
                        0
                    });
                capture.retained_bytes = size;
                (old_size, size)
            };
            self.analytics_bytes = self
                .analytics_bytes
                .saturating_sub(old_size)
                .saturating_add(size);
        } else {
            if key.method == "item/commandExecution/outputDelta" {
                observation.params["delta"] = Value::String(super::suffix(&delta, 12_000));
            }
            let metrics = NotificationStreamMetrics {
                hours: vec![NotificationHourMetrics {
                    hour,
                    count: 1,
                    encoded_bytes,
                }],
                bytes: delta.len() as u64,
                characters: delta.chars().count() as u64,
                lines: delta.bytes().filter(|byte| *byte == b'\n').count() as u64,
            };
            let index = self.analytics.len();
            let size = Self::capture_size(&observation, &metrics);
            self.analytics.push(Shared::new(AnalyticsCapture {
                key: key.clone(),
                observation,
                metrics,
                retained_bytes: size,
            }));
            Arc::make_mut(&mut self.indexes)
                .analytics
                .insert(key.clone(), index);
            self.analytics_bytes = self.analytics_bytes.saturating_add(size);
        }
        if self.analytics_bytes > 8 * 1024 * 1024 {
            // Drop this whole analytics capture, never its transcript or receipt.
            // Suppress its later samples until flush instead of counting each as
            // a separate overflowed capture.
            if let Some(index) = self.indexes.analytics.get(&key).copied() {
                let removed = self.analytics.remove(index).into_owned();
                self.analytics_bytes = self.analytics_bytes.saturating_sub(removed.retained_bytes);
            }
            self.analytics_skipped.push(Shared::new(key));
            self.rebuild_indexes();
            self.analytics_overflow = self.analytics_overflow.saturating_add(1);
        }
        Ok(())
    }
    pub(super) fn flush_analytics(&mut self, key: &StreamKey, agent: &Value) {
        self.analytics_skipped
            .retain(|sample_key| **sample_key != *key);
        if let Some(index) = self.indexes.analytics.get(key).copied() {
            let mut capture = self.analytics.remove(index).into_owned();
            capture.observation.agent = Some(agent.clone());
            self.queue_effect(NotificationPostCommit::AnalyticsStream {
                observation: capture.observation,
                metrics: capture.metrics,
            });
            self.analytics_bytes = self.analytics_bytes.saturating_sub(capture.retained_bytes);
            self.rebuild_indexes();
        } else {
            self.rebuild_indexes();
        }
    }
    pub(crate) fn pending_bytes(&self) -> usize {
        self.pending_bytes
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
        self.indexes
            .entries
            .get(key)
            .is_some_and(|index| self.entries[*index].count > 0)
    }

    pub(super) fn has_base(&self, key: &StreamKey) -> bool {
        self.indexes
            .entries
            .get(key)
            .is_some_and(|index| self.entries[*index].base.is_some())
    }

    pub(super) fn base(&self, key: &StreamKey) -> Option<&str> {
        self.entries
            .get(*self.indexes.entries.get(key)?)?
            .base
            .as_deref()
    }
    pub(super) fn preserve_base(&mut self, key: &StreamKey, hash: String) -> bool {
        if let Some(index) = self.indexes.entries.get(key).copied()
            && let Some(entry) = self.entries.get_mut(index)
            && entry.flushed
            && entry.base.is_none()
        {
            entry.base = Some(hash);
            return true;
        }
        false
    }
    pub(super) fn set_flushed_base(&mut self, key: &StreamKey, hash: String) {
        if let Some(index) = self.indexes.entries.get(key).copied()
            && let Some(entry) = self.entries.get_mut(index)
        {
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
        if self.indexes.closed.contains(&key) {
            return EnqueueResult::Closed;
        }
        let index = if let Some(index) = self.indexes.entries.get(&key).copied() {
            index
        } else {
            if self.entries.len() == MAX_STREAMS {
                return EnqueueResult::Full;
            }
            self.entries.push(Shared::new(Entry {
                key: key.clone(),
                flushed: false,
                base: None,
                pending: String::new(),
                count: 0,
                characters: 0,
                params: params.clone(),
            }));
            let index = self.entries.len() - 1;
            Arc::make_mut(&mut self.indexes).entries.insert(key, index);
            index
        };
        let entry = &mut self.entries[index];
        let pending_before = entry.pending.len().max(usize::from(entry.count > 0));
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
            let pending_after = entry.pending.len().max(usize::from(entry.count > 0));
            self.pending_bytes = self
                .pending_bytes
                .saturating_sub(pending_before)
                .saturating_add(pending_after);
            return EnqueueResult::FlushRequired;
        }
        entry.pending.push_str(delta);
        if entry.key.method == "item/commandExecution/outputDelta" {
            entry.pending = super::suffix(&entry.pending, 12_000);
        }
        let pending_after = entry.pending.len().max(usize::from(entry.count > 0));
        self.pending_bytes = self
            .pending_bytes
            .saturating_sub(pending_before)
            .saturating_add(pending_after);
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
        let index = *self.indexes.entries.get(key)?;
        let entry = self.entries.get_mut(index)?;
        let pending_before = entry.pending.len().max(usize::from(entry.count > 0));
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
        self.pending_bytes = self.pending_bytes.saturating_sub(pending_before);
        Some((text, count, params))
    }

    pub(super) fn close(&mut self, key: &StreamKey) {
        let pending_removed = self
            .indexes
            .entries
            .get(key)
            .map(|index| {
                self.entries[*index]
                    .pending
                    .len()
                    .max(usize::from(self.entries[*index].count > 0))
            })
            .unwrap_or_default();
        if let Some(index) = self.indexes.analytics.get(key).copied() {
            let removed = self.analytics.remove(index).into_owned();
            self.analytics_bytes = self.analytics_bytes.saturating_sub(removed.retained_bytes);
        }
        self.analytics_skipped
            .retain(|sample_key| **sample_key != *key);
        self.entries.retain(|entry| &entry.key != key);
        self.pending_bytes = self.pending_bytes.saturating_sub(pending_removed);
        if !self.indexes.closed.contains(key) {
            self.closed.push(Shared::new(key.clone()));
        }
        if self.closed.len() > MAX_STREAMS {
            self.closed.remove(0);
        }
        self.rebuild_indexes();
    }

    pub(super) fn discard(&mut self, key: &StreamKey) {
        let pending_removed = self
            .indexes
            .entries
            .get(key)
            .map(|index| {
                self.entries[*index]
                    .pending
                    .len()
                    .max(usize::from(self.entries[*index].count > 0))
            })
            .unwrap_or_default();
        if let Some(index) = self.indexes.analytics.get(key).copied() {
            let removed = self.analytics.remove(index).into_owned();
            self.analytics_bytes = self.analytics_bytes.saturating_sub(removed.retained_bytes);
        }
        self.analytics_skipped
            .retain(|sample_key| **sample_key != *key);
        self.entries.retain(|entry| &entry.key != key);
        self.pending_bytes = self.pending_bytes.saturating_sub(pending_removed);
        self.rebuild_indexes();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn staged_mutation_detaches_only_the_affected_entry() {
        let first = StreamKey {
            agent_id: "agent".into(),
            account: "default".into(),
            generation: 1,
            thread: "thread".into(),
            turn: Some("turn".into()),
            item: "first".into(),
            method: "item/agentMessage/delta".into(),
        };
        let second = StreamKey {
            item: "second".into(),
            ..first.clone()
        };
        let first_params =
            json!({"threadId":"thread","turnId":"turn","itemId":"first","delta":"old"});
        let second_params =
            json!({"threadId":"thread","turnId":"turn","itemId":"second","delta":"other"});
        let mut original = NotificationStreamBuffer::default();
        assert!(matches!(
            original.enqueue_delta(first.clone(), "old", &first_params),
            EnqueueResult::Buffered
        ));
        assert!(matches!(
            original.enqueue_delta(second.clone(), "other", &second_params),
            EnqueueResult::Buffered
        ));

        let mut staged = original.clone();
        assert!(Arc::ptr_eq(&original.entries[0].0, &staged.entries[0].0));
        assert!(Arc::ptr_eq(&original.entries[1].0, &staged.entries[1].0));
        let next_params =
            json!({"threadId":"thread","turnId":"turn","itemId":"first","delta":"new"});
        assert!(matches!(
            staged.enqueue_delta(first, "new", &next_params),
            EnqueueResult::Buffered
        ));

        assert!(!Arc::ptr_eq(&original.entries[0].0, &staged.entries[0].0));
        assert!(Arc::ptr_eq(&original.entries[1].0, &staged.entries[1].0));
        assert_eq!(original.entries[0].pending, "old");
        assert_eq!(staged.entries[0].pending, "oldnew");
        // For N retained entries and one changed entry: N deep Entry clones
        // become one; the other N-1 entries remain pointer-shared.
    }

    #[test]
    fn flush_then_close_keeps_flush_result_and_closes_future_deltas() {
        let key = StreamKey {
            agent_id: "agent".into(),
            account: "default".into(),
            generation: 1,
            thread: "thread".into(),
            turn: Some("turn".into()),
            item: "answer".into(),
            method: "item/agentMessage/delta".into(),
        };
        let params =
            json!({"threadId":"thread","turnId":"turn","itemId":"answer","delta":"pending"});
        let mut streams = NotificationStreamBuffer::default();
        assert!(matches!(
            streams.enqueue_delta(key.clone(), "pending", &params),
            EnqueueResult::Buffered
        ));

        let flushed = streams
            .flush_with_delta(&key, "base", Some("terminal"))
            .expect("pending stream flushes before close");
        streams.close(&key);

        assert_eq!(flushed.0, "basependingterminal");
        assert_eq!(flushed.1, 1);
        assert!(!streams.has_pending(&key));
        assert!(matches!(
            streams.enqueue_delta(key, "late", &params),
            EnqueueResult::Closed
        ));
    }

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

    #[test]
    fn indexes_and_counters_rebuild_from_transparent_persisted_shape() {
        let key = StreamKey {
            agent_id: "agent".into(),
            account: "default".into(),
            generation: 1,
            thread: "thread".into(),
            turn: Some("turn".into()),
            item: "answer".into(),
            method: "item/agentMessage/delta".into(),
        };
        let mut streams = NotificationStreamBuffer::default();
        streams.enqueue_delta(key.clone(), "abc", &json!({"delta":"abc"}));
        assert!(streams.has_pending(&key));
        assert_eq!(streams.pending_bytes(), 3);
        let value = serde_json::to_value(&streams).unwrap();
        assert!(value.get("indexes").is_none());
        let mut restored: NotificationStreamBuffer = serde_json::from_value(value).unwrap();
        assert!(restored.has_pending(&key));
        assert_eq!(restored.pending_bytes(), 3);
        let _flushed = restored.flush(&key, "").unwrap();
        assert_eq!(restored.pending_bytes(), 0);
    }

    #[test]
    fn analytics_index_and_byte_counters_track_aggregation_and_flush() {
        let key = StreamKey {
            agent_id: "agent".into(),
            account: "default".into(),
            generation: 1,
            thread: "thread".into(),
            turn: Some("turn".into()),
            item: "answer".into(),
            method: "item/agentMessage/delta".into(),
        };
        let observation = |cursor, delta: &str| NotificationObservation {
            agent: None,
            method: key.method.clone(),
            params: json!({"delta":delta}),
            delivery: crate::notification_dispatch::ProviderDeliveryIdentity {
                provider_handle: "account:default".into(),
                generation: 1,
                cursor,
            },
            observed_at: 3600.0,
            provider_at: 3600.0,
        };
        let mut streams = NotificationStreamBuffer::default();
        streams
            .stage_analytics(key.clone(), observation(1, "a"))
            .unwrap();
        let first_size = streams.analytics_bytes;
        streams
            .stage_analytics(key.clone(), observation(2, "b"))
            .unwrap();
        assert_eq!(streams.analytics.len(), 1);
        assert_eq!(streams.analytics[0].observation.params["delta"], "ab");
        assert_eq!(
            serde_json::to_value(&streams).unwrap()["analytics"][0]
                .as_array()
                .unwrap()
                .len(),
            3
        );
        assert!(streams.analytics_bytes > first_size);
        assert_eq!(streams.indexes.analytics.get(&key), Some(&0));
        let mut restored: NotificationStreamBuffer =
            serde_json::from_value(serde_json::to_value(&streams).unwrap()).unwrap();
        assert_eq!(restored.analytics_bytes, streams.analytics_bytes);
        assert_eq!(restored.indexes.analytics.get(&key), Some(&0));
        restored.flush_analytics(&key, &json!({"id":"agent"}));
        assert!(restored.analytics.is_empty());
        assert_eq!(restored.analytics_bytes, 0);
        assert_eq!(restored.post_commit_analytics_count, 1);
        let _batch = restored.take_post_commit().unwrap();
        assert_eq!(restored.post_commit_analytics_count, 0);
        assert_eq!(restored.post_commit_analytics_bytes, 0);
    }
}
