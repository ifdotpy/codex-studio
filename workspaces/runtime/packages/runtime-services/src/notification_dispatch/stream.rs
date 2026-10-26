//! Pure, bounded connection stream state. Cloned buffers share immutable entries;
//! staged writes detach only the touched entry, and SQLite commit controls apply.
use super::items::TOOL_OUTPUT_CHARS;
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

const MAX_PENDING_TEXT_BYTES: usize = 64 * 1024;
/// Sanity bound on remembered finished streams (count). A turn's keys are dropped
/// when it settles, so only a turn that never settles can reach this; the oldest
/// key is then evicted first.
const MAX_CLOSED_STREAMS: usize = 8_192;
/// Best-effort stream statistics staged in one journal batch. Matches Python's
/// 64-capture analytics queue; complete analytics notifications are not counted.
const MAX_POST_COMMIT_STREAM_EFFECTS: usize = 64;
/// Retained JSON bytes of best-effort stream statistics staged in one journal batch.
const MAX_POST_COMMIT_STREAM_BYTES: usize = 8 * 1024 * 1024;
/// Retained bytes of stream captures awaiting their flush.
const MAX_STAGED_ANALYTICS_BYTES: usize = 8 * 1024 * 1024;
#[cfg(test)]
thread_local! {
    static DELTA_SIZE_INPUT_BYTES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

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
    /// Account and opaque metadata signature owned by account_worker::store.
    /// Validates this cache against one committed snapshot; never serialized.
    #[serde(skip)]
    pub(crate) store_revision: Option<(String, String)>,
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

/// The durable metadata of a stream buffer: every field, with each entry's
/// pending text, count and params blanked (the pending form keeps them).
/// It borrows the buffer, so persisting copies no stream text.
#[derive(Serialize)]
struct StreamMetadata<'a> {
    entries: Vec<EntryMetadata<'a>>,
    closed: &'a [Shared<StreamKey>],
    analytics: &'a [Shared<AnalyticsCapture>],
    post_commit: &'a [Shared<NotificationPostCommit>],
    publication_sequence: u64,
    analytics_overflow: u64,
    analytics_skipped: &'a [Shared<StreamKey>],
}

#[derive(Serialize)]
struct EntryMetadata<'a> {
    key: &'a StreamKey,
    flushed: bool,
    base: &'a Option<String>,
    pending: &'static str,
    count: u64,
    characters: u64,
    params: (),
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
    FlushRequired,
}

impl NotificationStreamBuffer {
    /// Only intermediate stream statistics are best-effort. Complete analytics
    /// notifications are durable publication receipts and never enter the stream
    /// bounds. The store's receipt policy uses this same owner.
    pub(crate) fn is_best_effort_analytics(effect: &NotificationPostCommit) -> bool {
        matches!(effect, NotificationPostCommit::AnalyticsStream { .. })
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
            if Self::is_best_effort_analytics(effect) {
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
    pub(crate) fn changed_from(&self, previous: &Self) -> bool {
        self.publication_sequence != previous.publication_sequence
            || self.analytics_overflow != previous.analytics_overflow
            || shared_entries_changed(&self.entries, &previous.entries)
            || shared_entries_changed(&self.closed, &previous.closed)
            || shared_entries_changed(&self.analytics, &previous.analytics)
            || shared_entries_changed(&self.post_commit, &previous.post_commit)
            || shared_entries_changed(&self.analytics_skipped, &previous.analytics_skipped)
    }
    pub(super) fn queue_effect(&mut self, effect: NotificationPostCommit) {
        // As in Python, excess stream captures are dropped, counted and never
        // allowed to refuse a notification, lose a receipt or cancel a session.
        // Complete analytics notifications are never dropped here: the store
        // persists them as durable receipts in the same journal transaction.
        // Rust measures retained JSON bytes, rather than CPython heap sizes.
        if Self::is_best_effort_analytics(&effect) {
            let size = serde_json::to_vec(&effect).map_or(usize::MAX, |bytes| bytes.len());
            if self.post_commit_analytics_count >= MAX_POST_COMMIT_STREAM_EFFECTS
                || self.post_commit_analytics_bytes.saturating_add(size)
                    > MAX_POST_COMMIT_STREAM_BYTES
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
                let mut removed_bytes = 0;
                let mut added_bytes = serde_json::to_string(&delta)
                    .map_or(usize::MAX, |value| value.len())
                    .saturating_sub(2);
                let mut text = match std::mem::take(&mut saved.params["delta"]) {
                    Value::String(text) => text,
                    other => {
                        removed_bytes =
                            serde_json::to_string(&other).map_or(usize::MAX, |value| value.len());
                        added_bytes = added_bytes.saturating_add(2);
                        String::new()
                    }
                };
                #[cfg(test)]
                DELTA_SIZE_INPUT_BYTES.update(|bytes| bytes + delta.len());
                text.push_str(&delta);
                if key.method == "item/commandExecution/outputDelta" {
                    let start = text
                        .char_indices()
                        .rev()
                        .nth(TOOL_OUTPUT_CHARS - 1)
                        .map_or(0, |(index, _)| index);
                    removed_bytes = removed_bytes.saturating_add(
                        serde_json::to_string(&text[..start])
                            .map_or(usize::MAX, |value| value.len())
                            .saturating_sub(2),
                    );
                    #[cfg(test)]
                    DELTA_SIZE_INPUT_BYTES.update(|bytes| bytes + start);
                    text.drain(..start);
                }
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
                    .saturating_add(added_bytes)
                    .saturating_sub(removed_bytes)
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
                observation.params["delta"] =
                    Value::String(super::suffix(&delta, TOOL_OUTPUT_CHARS));
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
        if self.analytics_bytes > MAX_STAGED_ANALYTICS_BYTES {
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
            capture.observation.agent = Some(super::SharedJson::agent_projection(agent));
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
    /// The durable JSON text of this buffer: its metadata, and its entries
    /// with pending deltas (`None` when nothing is pending).
    pub(crate) fn persisted_text(&self) -> serde_json::Result<(String, Option<String>)> {
        let metadata = serde_json::to_string(&StreamMetadata {
            entries: self
                .entries
                .iter()
                .map(|entry| EntryMetadata {
                    key: &entry.key,
                    flushed: entry.flushed,
                    base: &entry.base,
                    pending: "",
                    count: 0,
                    characters: entry.characters,
                    params: (),
                })
                .collect(),
            closed: &self.closed,
            analytics: &self.analytics,
            post_commit: &self.post_commit,
            publication_sequence: self.publication_sequence,
            analytics_overflow: self.analytics_overflow,
            analytics_skipped: &self.analytics_skipped,
        })?;
        let pending = self
            .entries
            .iter()
            .filter(|entry| entry.count > 0)
            .collect::<Vec<_>>();
        let pending = if pending.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&pending)?)
        };
        Ok((metadata, pending))
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
    /// A flushed stream whose stored item still holds its flushed text.
    pub(super) fn awaits_base(&self, key: &StreamKey) -> bool {
        self.indexes.entries.get(key).is_some_and(|index| {
            self.entries[*index].flushed && self.entries[*index].base.is_none()
        })
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
            entry.pending = super::suffix(&entry.pending, TOOL_OUTPUT_CHARS);
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
            text = super::suffix(&text, TOOL_OUTPUT_CHARS);
        }
        let count = std::mem::take(&mut entry.count);
        let mut params = std::mem::take(&mut entry.params);
        params["_streamCharacters"] = Value::from(std::mem::take(&mut entry.characters));
        self.pending_bytes = self.pending_bytes.saturating_sub(pending_before);
        Some((text, count, params))
    }

    /// Drop the open stream, capture and sample of `key` without touching the
    /// other keys' indexes more than the removal itself requires.
    fn forget(&mut self, key: &StreamKey) {
        let indexes = Arc::make_mut(&mut self.indexes);
        if let Some(index) = indexes.entries.remove(key) {
            let removed = self.entries.remove(index);
            let pending = removed.pending.len().max(usize::from(removed.count > 0));
            self.pending_bytes = self.pending_bytes.saturating_sub(pending);
            for later in indexes.entries.values_mut().filter(|later| **later > index) {
                *later -= 1;
            }
        }
        if let Some(index) = indexes.analytics.remove(key) {
            let removed = self.analytics.remove(index).into_owned();
            self.analytics_bytes = self.analytics_bytes.saturating_sub(removed.retained_bytes);
            for later in indexes
                .analytics
                .values_mut()
                .filter(|later| **later > index)
            {
                *later -= 1;
            }
        }
        if indexes.analytics_skipped.remove(key) {
            self.analytics_skipped
                .retain(|sample_key| **sample_key != *key);
        }
    }

    /// Finish the stream: later deltas for `key` are dropped until its turn settles.
    pub(super) fn close(&mut self, key: &StreamKey) {
        self.forget(key);
        let indexes = Arc::make_mut(&mut self.indexes);
        if indexes.closed.insert(key.clone()) {
            self.closed.push(Shared::new(key.clone()));
        }
        if self.closed.len() > MAX_CLOSED_STREAMS {
            let evicted = self.closed.remove(0);
            indexes.closed.remove(&*evicted);
        }
    }

    pub(super) fn discard(&mut self, key: &StreamKey) {
        self.forget(key);
    }

    /// The turn ended: its finished streams no longer need a closed key, because
    /// dispatch drops a delta of a turn that is not in progress by the turn alone.
    /// Keys without a turn settle with any turn of their thread.
    pub(crate) fn settle_turn(&mut self, account: &str, thread: &str, turn: &str) {
        self.retain_closed(|key| {
            !(key.account == account
                && key.thread == thread
                && key.turn.as_deref().is_none_or(|key_turn| key_turn == turn))
        });
    }

    /// Keep only the closed keys `keep` accepts; it runs once per key.
    pub(super) fn retain_closed(&mut self, mut keep: impl FnMut(&StreamKey) -> bool) {
        let mut dropped = Vec::new();
        self.closed.retain(|key| {
            let kept = keep(key);
            if !kept {
                dropped.push(StreamKey::clone(key));
            }
            kept
        });
        if !dropped.is_empty() {
            let indexes = Arc::make_mut(&mut self.indexes);
            for key in &dropped {
                indexes.closed.remove(key);
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn closed_len(&self) -> usize {
        self.closed.len()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn analytics_delta_size_work_counts_only_the_new_fragment() {
        let key = turn_key(0, 0);
        let mut streams = NotificationStreamBuffer::default();
        let mut observation = analytics_observation(&key.method, 1);
        observation.params = super::super::SharedJson::new(json!({"delta":"a".repeat(64_000)}));
        streams.stage_analytics(key.clone(), observation).unwrap();
        DELTA_SIZE_INPUT_BYTES.set(0);
        let fragment = "\"\\\n\u{0001}é";
        let text = &mut streams.analytics[0].value_mut().observation.params["delta"];
        let Value::String(text) = text else {
            panic!("string delta")
        };
        text.reserve(fragment.len());
        let allocation = text.as_ptr();
        let mut observation = analytics_observation(&key.method, 2);
        observation.params = super::super::SharedJson::new(json!({"delta":fragment}));
        streams.stage_analytics(key, observation).unwrap();
        assert_eq!(DELTA_SIZE_INPUT_BYTES.get(), fragment.len());
        let capture = &streams.analytics[0];
        assert_eq!(
            capture.observation.params["delta"]
                .as_str()
                .unwrap()
                .as_ptr(),
            allocation
        );
        assert_eq!(capture.retained_bytes, 64_050);
    }

    #[test]
    fn analytics_delta_size_matches_json_after_escaped_unicode_tail_trimming() {
        let mut streams = NotificationStreamBuffer::default();
        let mut key = turn_key(0, 0);
        key.method = "item/commandExecution/outputDelta".into();
        for (cursor, (fragment, expected, retained_bytes)) in [
            (
                "界\"\\\n\u{0001}".repeat(3_000),
                "界\"\\\n\u{0001}".repeat(2_400),
                36_036,
            ),
            (
                "é".into(),
                format!("\"\\\n\u{0001}{}é", "界\"\\\n\u{0001}".repeat(2_399)),
                36_059,
            ),
            ("z".repeat(13_000), "z".repeat(12_000), 12_084),
        ]
        .into_iter()
        .enumerate()
        {
            let mut observation = analytics_observation(&key.method, cursor as u64);
            observation.observed_at = (cursor + 1) as f64 * 3600.0;
            observation.params = super::super::SharedJson::new(json!({"delta":fragment}));
            streams.stage_analytics(key.clone(), observation).unwrap();
            let capture = &streams.analytics[0];
            assert_eq!(capture.observation.params["delta"], expected);
            assert_eq!(capture.retained_bytes, retained_bytes);
            let restored: NotificationStreamBuffer =
                serde_json::from_value(serde_json::to_value(&streams).unwrap()).unwrap();
            assert_eq!(restored.analytics_bytes, streams.analytics_bytes);
        }
    }

    #[test]
    fn completed_streams_release_capacity_over_a_worker_lifetime() {
        let mut streams = NotificationStreamBuffer::default();
        for index in 0..(MAX_CLOSED_STREAMS + 1) {
            let key = StreamKey {
                agent_id: "agent".into(),
                account: "default".into(),
                generation: 1,
                thread: "thread".into(),
                turn: Some("turn".into()),
                item: index.to_string(),
                method: "item/agentMessage/delta".into(),
            };
            assert!(matches!(
                streams.enqueue_delta(key.clone(), "x", &Value::Null),
                EnqueueResult::Buffered
            ));
            streams.close(&key);
            assert!(streams.entries.is_empty());
            let retained = (index + 1).min(MAX_CLOSED_STREAMS);
            assert_eq!(streams.closed.len(), retained);
            assert_eq!(streams.indexes.closed.len(), retained);
            assert!(matches!(
                streams.enqueue_delta(key.clone(), "late", &Value::Null),
                EnqueueResult::Closed
            ));
            if index == MAX_CLOSED_STREAMS {
                let oldest = StreamKey {
                    item: "0".into(),
                    ..key
                };
                assert!(!streams.indexes.closed.contains(&oldest));
                assert!(streams.closed.iter().all(|closed| **closed != oldest));
            }
        }
        let key = StreamKey {
            agent_id: "agent".into(),
            account: "default".into(),
            generation: 1,
            thread: "thread".into(),
            turn: Some("turn".into()),
            item: "after-cap".into(),
            method: "item/agentMessage/delta".into(),
        };
        assert!(matches!(
            streams.enqueue_delta(key, "still alive", &Value::Null),
            EnqueueResult::Buffered
        ));
    }

    fn turn_key(turn: usize, item: usize) -> StreamKey {
        StreamKey {
            agent_id: "agent".into(),
            account: "default".into(),
            generation: 1,
            thread: "thread".into(),
            turn: Some(format!("turn-{turn}")),
            item: format!("item-{item}"),
            method: "item/agentMessage/delta".into(),
        }
    }

    #[test]
    fn closed_keys_stay_bounded_by_the_turn_in_progress_over_many_turns() {
        let mut streams = NotificationStreamBuffer::default();
        for turn in 0..1_000 {
            for item in 0..3 {
                let key = turn_key(turn, item);
                streams.enqueue_delta(key.clone(), "x", &Value::Null);
                streams.close(&key);
            }
            assert_eq!(streams.closed.len(), 3);
            streams.settle_turn("default", "thread", &format!("turn-{turn}"));
            assert!(streams.closed.is_empty());
            assert!(streams.indexes.closed.is_empty());
        }
    }

    #[test]
    fn closing_keeps_the_other_open_streams_addressable() {
        let mut streams = NotificationStreamBuffer::default();
        for item in 0..3 {
            streams.enqueue_delta(turn_key(0, item), "a", &Value::Null);
        }
        streams.close(&turn_key(0, 0));
        streams.discard(&turn_key(0, 1));
        assert!(matches!(
            streams.enqueue_delta(turn_key(0, 2), "b", &Value::Null),
            EnqueueResult::Buffered
        ));
        assert_eq!(streams.entries.len(), 1);
        assert_eq!(streams.entries[0].pending, "ab");
        assert!(matches!(
            streams.enqueue_delta(turn_key(0, 0), "late", &Value::Null),
            EnqueueResult::Closed
        ));
    }

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

    /// The borrowed durable form equals the former one: the buffer as a JSON
    /// value with each entry's pending text, count and params blanked, plus a
    /// copy of every entry with pending deltas.
    #[test]
    fn persisted_text_matches_the_blanked_value_form() {
        let key = |item: &str, turn: Option<&str>| StreamKey {
            agent_id: "agent".into(),
            account: "default".into(),
            generation: 3,
            thread: "thread".into(),
            turn: turn.map(str::to_owned),
            item: item.into(),
            method: "item/agentMessage/delta".into(),
        };
        let params = json!({"threadId":"thread","turnId":"turn","itemId":"a"});
        let mut streams = NotificationStreamBuffer::default();
        assert!(matches!(
            streams.enqueue_delta(key("a", Some("turn")), "pending é", &params),
            EnqueueResult::Buffered
        ));
        assert!(matches!(
            streams.enqueue_delta(key("b", None), "flushed", &params),
            EnqueueResult::Buffered
        ));
        streams.flush(&key("b", None), "base").expect("flushes");
        streams.preserve_base(&key("b", None), "hash".into());
        assert!(matches!(
            streams.enqueue_delta(key("c", Some("turn")), "closed", &params),
            EnqueueResult::Buffered
        ));
        streams.flush(&key("c", Some("turn")), "").expect("flushes");
        streams.close(&key("c", Some("turn")));
        let former = |streams: &NotificationStreamBuffer| {
            let mut metadata = serde_json::to_value(streams).expect("buffer encodes");
            let mut pending = Vec::new();
            for entry in metadata["entries"].as_array_mut().expect("entries") {
                if entry["count"].as_u64().unwrap_or(0) > 0 {
                    pending.push(entry.clone());
                }
                entry["pending"] = json!("");
                entry["count"] = json!(0);
                entry["params"] = Value::Null;
            }
            (metadata, (!pending.is_empty()).then(|| json!(pending)))
        };
        let parse = |text: &str| serde_json::from_str::<Value>(text).expect("JSON text");
        for buffer in [streams.clone(), NotificationStreamBuffer::default()] {
            let (metadata, pending) = buffer.persisted_text().expect("encodes");
            let (expected_metadata, expected_pending) = former(&buffer);
            assert_eq!(parse(&metadata), expected_metadata);
            assert_eq!(pending.as_deref().map(parse), expected_pending);
        }
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
        assert_eq!(streams.pending_bytes(), TOOL_OUTPUT_CHARS * 3);
        let (text, _, _) = streams.flush(&key, "older").expect("buffered output");
        assert_eq!(text, "界".repeat(TOOL_OUTPUT_CHARS));
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
            params: crate::notification_dispatch::SharedJson::new(json!({"delta":delta})),
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

    fn analytics_observation(method: &str, cursor: u64) -> NotificationObservation {
        NotificationObservation {
            agent: None,
            method: method.into(),
            params: crate::notification_dispatch::SharedJson::new(json!({"delta":"x"})),
            delivery: crate::notification_dispatch::ProviderDeliveryIdentity {
                provider_handle: "account:default".into(),
                generation: 1,
                cursor,
            },
            observed_at: 3600.0,
            provider_at: 3600.0,
        }
    }

    #[test]
    fn complete_analytics_bypasses_the_stream_queue_bound() {
        let mut streams = NotificationStreamBuffer::default();
        for cursor in 0..MAX_POST_COMMIT_STREAM_EFFECTS as u64 {
            streams.queue_effect(NotificationPostCommit::AnalyticsStream {
                observation: analytics_observation("item/agentMessage/delta", cursor),
                metrics: NotificationStreamMetrics::default(),
            });
        }
        streams.queue_effect(NotificationPostCommit::Analytics {
            observation: analytics_observation("thread/tokenUsage/updated", 1_000),
            budget_capture_value: Some(12),
            usage_conflict: None,
            ledger_since: Default::default(),
            usage_unknown: false,
        });
        streams.queue_effect(NotificationPostCommit::AnalyticsStream {
            observation: analytics_observation("item/agentMessage/delta", 1_001),
            metrics: NotificationStreamMetrics::default(),
        });
        let batch = streams.take_post_commit().expect("take staged batch");
        // The 65th stream capture is shed and counted; the complete usage is kept.
        assert_eq!(batch.analytics_overflow, 1);
        assert_eq!(batch.effects.len(), MAX_POST_COMMIT_STREAM_EFFECTS + 1);
        assert!(batch.effects.iter().any(|effect| matches!(
            effect,
            NotificationPostCommit::Analytics {
                budget_capture_value: Some(12),
                usage_conflict: None,
                ..
            }
        )));
    }
}
