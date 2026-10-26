//! Bounded, latest-value token-rate publication for resource streams.

use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, OnceLock};

use crate::change_notifications::{ChangeNotificationHub, SYNC_FRAME_MIN_INTERVAL};
use crate::notification_dispatch::{NotificationObservation, NotificationPostCommit};
use serde::{Deserialize, Serialize};
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::usage::{
    UsageObservation, UsageProvider, UsageScope, UsageTokens, parse_token_usage,
};
use studio_runtime_schema::{TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS, TOKEN_RATE_MAX_ENTRIES};
use thiserror::Error;
use tokio::sync::{mpsc, oneshot, watch};
use tokio::time::Instant;

#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenRateRecord {
    agent_id: TokenRateIdentifier,
    root_id: TokenRateIdentifier,
    turn_id: TokenRateIdentifier,
    active: bool,
    estimated: bool,
    rate: FiniteRate,
    output_tokens: FiniteRate,
}

/// Opaque agent/root/turn identity bounded by the schema-owned Unicode limit.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(try_from = "String")]
struct TokenRateIdentifier(String);

impl TryFrom<String> for TokenRateIdentifier {
    type Error = TokenRateError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        if valid_id(&value) {
            Ok(Self(value))
        } else {
            Err(TokenRateError::InvalidSnapshot)
        }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
struct FiniteRate(f64);

impl<'de> Deserialize<'de> for FiniteRate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = f64::deserialize(deserializer)?;
        if value.is_finite() && value >= 0.0 {
            Ok(Self(value))
        } else {
            Err(serde::de::Error::custom(
                "expected a finite nonnegative number",
            ))
        }
    }
}

impl Serialize for FiniteRate {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        serializer.serialize_f64(self.0)
    }
}

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenRateSnapshot {
    records: Vec<TokenRateRecord>,
    #[serde(skip)]
    wire: WireCache,
}

/// JSON text of the record list of a snapshot, written once for all
/// stream subscribers; a stream frame embeds these fragments verbatim.
#[derive(Debug)]
pub struct TokenRateWire {
    pub records: String,
}

/// Borrowed changes from the snapshot this stream last sent. No record lists are copied.
#[derive(Serialize)]
pub struct TokenRateDelta<'a> {
    records: ChangedRecords<'a>,
    removed: RemovedAgents<'a>,
}

struct ChangedRecords<'a> {
    current: &'a [TokenRateRecord],
    previous: &'a [TokenRateRecord],
}

impl Serialize for ChangedRecords<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(None)?;
        let mut previous = self.previous.iter().peekable();
        for record in self.current {
            while previous
                .peek()
                .is_some_and(|old| old.agent_id < record.agent_id)
            {
                previous.next();
            }
            if previous.peek().is_none_or(|old| *old != record) {
                sequence.serialize_element(record)?;
            }
        }
        sequence.end()
    }
}

struct RemovedAgents<'a> {
    current: &'a [TokenRateRecord],
    previous: &'a [TokenRateRecord],
}

impl Serialize for RemovedAgents<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut sequence = serializer.serialize_seq(None)?;
        let mut current = self.current.iter().peekable();
        for record in self.previous {
            while current
                .peek()
                .is_some_and(|new| new.agent_id < record.agent_id)
            {
                current.next();
            }
            if current
                .peek()
                .is_none_or(|new| new.agent_id != record.agent_id)
            {
                sequence.serialize_element(&record.agent_id)?;
            }
        }
        sequence.end()
    }
}

/// Lazily filled serialization of one immutable snapshot. It never takes part in equality.
#[derive(Debug, Default)]
struct WireCache(std::sync::OnceLock<Arc<TokenRateWire>>);

impl Clone for WireCache {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

impl PartialEq for WireCache {
    fn eq(&self, _: &Self) -> bool {
        true
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum TokenRateError {
    #[error("token-rate snapshot is invalid")]
    InvalidSnapshot,
    #[error("token-rate publication is unavailable")]
    Unavailable,
}

/// A bounded watch channel; slow subscribers always receive the newest snapshot.
///
/// One projector task owns the per-agent state. An observation only queues for it, the task
/// updates the one agent it names, and it builds one immutable snapshot per
/// `SYNC_FRAME_MIN_INTERVAL` while observations arrive; it sleeps while none do.
pub struct TokenRateHub {
    publication: Arc<Publication>,
    /// Queue of the projector task, which the first observation starts.
    projector: OnceLock<mpsc::UnboundedSender<ProjectorCommand>>,
}

struct Publication {
    sender: watch::Sender<Arc<TokenRateSnapshot>>,
    changes: Option<Arc<ChangeNotificationHub>>,
}

enum ProjectorCommand {
    Observe {
        effect: NotificationPostCommit,
        effect_bit: u8,
    },
    Settle(oneshot::Sender<()>),
}

#[derive(Default)]
struct RateProjector {
    agents: BTreeMap<String, ProjectedAgent>,
    // One volatile delivery watermark per provider, matching the durable outbox identity.
    watermarks: BTreeMap<String, DeliveryWatermark>,
}

struct DeliveryWatermark {
    generation: u64,
    cursor: u64,
    effects: u8,
}

#[derive(Default)]
struct ProjectedAgent {
    provider_handle: String,
    root: String,
    turn: String,
    active: bool,
    estimated: bool,
    rate: f64,
    actual: f64,
    pending: f64,
    lifetime: Option<UsageTokens>,
    baseline: Option<f64>,
    timing_valid: bool,
    generation_started: Option<f64>,
    samples: VecDeque<(f64, f64)>,
    // Exact first response values for this turn; eviction would make replay change totals.
    response_outputs: BTreeMap<(String, String), UsageTokens>,
    response_output_total: f64,
    tools: BTreeSet<String>,
    generation_elapsed: f64,
    response_durations: VecDeque<(f64, f64)>,
    response_duration_pending_tokens: f64,
}

// This projector owns its rate cap (tokens/second), minimum duration (seconds),
// text estimate (tokens/character), and rolling window (seconds).
const MAX_TOKEN_RATE: f64 = 1_000.0;
const MIN_GENERATION_DURATION_SECONDS: f64 = 0.25;
const TOKEN_ESTIMATE_TOKENS_PER_CHARACTER: f64 = 0.25;
const TOKEN_RATE_WINDOW_SECONDS: f64 = 4.0;
const MAX_PROJECTED_AGENTS: usize = TOKEN_RATE_MAX_ENTRIES;
// Retain provider delivery watermarks across snapshot eviction.
const MAX_TOKEN_RATE_WATERMARKS: usize = MAX_PROJECTED_AGENTS * 2;
// Maximum queued response-duration observations per projected agent (entries).
const MAX_PENDING_RESPONSE_DURATION_ENTRIES: usize = 1024;

impl TokenRateSnapshot {
    pub fn from_value(value: serde_json::Value) -> Result<Self, TokenRateError> {
        let snapshot: Self =
            serde_json::from_value(value).map_err(|_| TokenRateError::InvalidSnapshot)?;
        snapshot.validated()
    }

    /// The snapshot, when it respects the entry and value limits of the wire contract.
    fn validated(self) -> Result<Self, TokenRateError> {
        let mut snapshot = self;
        // Projector snapshots already follow BTreeMap order; external producer snapshots
        // are normalized once here so each stream can diff in linear time without an index.
        snapshot
            .records
            .sort_unstable_by(|a, b| a.agent_id.cmp(&b.agent_id));
        if snapshot.records.len() > TOKEN_RATE_MAX_ENTRIES
            || snapshot
                .records
                .windows(2)
                .any(|pair| pair[0].agent_id == pair[1].agent_id)
        {
            return Err(TokenRateError::InvalidSnapshot);
        }
        Ok(snapshot)
    }

    #[must_use]
    pub fn empty() -> Self {
        Self::default()
    }

    #[must_use]
    pub fn to_value(&self) -> serde_json::Value {
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({"records":[]}))
    }

    #[must_use]
    pub fn delta_since<'a>(&'a self, previous: &'a Self) -> TokenRateDelta<'a> {
        TokenRateDelta {
            records: ChangedRecords {
                current: &self.records,
                previous: &previous.records,
            },
            removed: RemovedAgents {
                current: &self.records,
                previous: &previous.records,
            },
        }
    }

    /// The serialized record list, written on first use and then shared.
    #[must_use]
    pub fn wire(&self) -> Arc<TokenRateWire> {
        Arc::clone(self.wire.0.get_or_init(|| {
            let text =
                |result: serde_json::Result<String>| result.unwrap_or_else(|_| "[]".to_owned());
            Arc::new(TokenRateWire {
                records: text(serde_json::to_string(&self.records)),
            })
        }))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.records.is_empty()
    }
}

impl TokenRateHub {
    #[must_use]
    pub fn new() -> Self {
        Self::publishing(None)
    }

    /// Bind token-only publications to the shared cursor without invalidating resources.
    #[must_use]
    pub fn with_change_hub(changes: Arc<ChangeNotificationHub>) -> Self {
        Self::publishing(Some(changes))
    }

    fn publishing(changes: Option<Arc<ChangeNotificationHub>>) -> Self {
        Self {
            publication: Arc::new(Publication {
                sender: watch::channel(Arc::new(TokenRateSnapshot::empty())).0,
                changes,
            }),
            projector: OnceLock::new(),
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> TokenRateSnapshot {
        TokenRateSnapshot::clone(&self.publication.sender.borrow())
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<Arc<TokenRateSnapshot>> {
        self.publication.sender.subscribe()
    }

    /// Queue one committed provider observation for projection into HTTP sync. The exact
    /// provider/generation/cursor/effect watermark stays with the projector even when its
    /// bounded snapshot entry is evicted. Must be called inside a Tokio runtime.
    pub fn publish_observation(
        &self,
        effect: NotificationPostCommit,
    ) -> Result<(), TokenRateError> {
        let effect_bit = match effect {
            NotificationPostCommit::Analytics { .. }
            | NotificationPostCommit::AnalyticsCaptured(_)
            | NotificationPostCommit::TokenRateUsage(_) => 1,
            NotificationPostCommit::TokenRateStream(_) => 2,
            NotificationPostCommit::TokenRateAssociation(_) => 4,
            NotificationPostCommit::AnalyticsStream { .. }
            | NotificationPostCommit::AnalyticsStreamCaptured(_)
            | NotificationPostCommit::Recovered { .. } => {
                return Err(TokenRateError::Unavailable);
            }
        };
        effect_agent_id(&effect).ok_or(TokenRateError::Unavailable)?;
        self.queue(ProjectorCommand::Observe { effect, effect_bit })
    }

    /// Wait until every observation queued before this call is projected and published.
    pub async fn settled(&self) {
        let (sender, receiver) = oneshot::channel();
        if self.queue(ProjectorCommand::Settle(sender)).is_ok() {
            receiver.await.ok();
        }
    }

    fn queue(&self, command: ProjectorCommand) -> Result<(), TokenRateError> {
        self.projector
            .get_or_init(|| {
                let (sender, receiver) = mpsc::unbounded_channel();
                tokio::spawn(run_projector(receiver, Arc::clone(&self.publication)));
                sender
            })
            .send(command)
            .map_err(|_| TokenRateError::Unavailable)
    }

    /// Publish one already validated producer snapshot; identical updates are coalesced.
    pub fn publish(&self, value: serde_json::Value) -> Result<bool, TokenRateError> {
        Ok(self
            .publication
            .publish(TokenRateSnapshot::from_value(value)?))
    }
}

impl Publication {
    fn publish(&self, snapshot: TokenRateSnapshot) -> bool {
        self.sender.send_if_modified(|current| {
            if **current == snapshot {
                false
            } else if let Some(changes) = &self.changes {
                changes.try_publish(1, false, |_| *current = Arc::new(snapshot))
            } else {
                *current = Arc::new(snapshot);
                true
            }
        })
    }
}

/// The projector task: it sleeps until an observation arrives, folds in everything that
/// arrives before the next frame instant, and then publishes one snapshot. A settle request
/// publishes at once. The task ends with the hub.
async fn run_projector(
    mut commands: mpsc::UnboundedReceiver<ProjectorCommand>,
    publication: Arc<Publication>,
) {
    let mut projector = RateProjector::default();
    let mut next_frame = Instant::now();
    let mut settled = Vec::new();
    let mut dirty = false;
    while let Some(command) = commands.recv().await {
        let mut open = true;
        let mut command = Some(command);
        let frame = tokio::time::sleep_until(next_frame);
        tokio::pin!(frame);
        loop {
            match command.take() {
                Some(ProjectorCommand::Observe { effect, effect_bit }) => {
                    dirty |= projector.observe(&effect, effect_bit);
                }
                Some(ProjectorCommand::Settle(waiter)) => settled.push(waiter),
                None => {}
            }
            if !settled.is_empty() {
                break;
            }
            tokio::select! {
                biased;
                () = &mut frame => break,
                next = commands.recv() => match next {
                    Some(next) => command = Some(next),
                    None => {
                        open = false;
                        break;
                    }
                },
            }
        }
        if std::mem::take(&mut dirty) {
            publication.publish(projector.snapshot());
            next_frame = Instant::now() + SYNC_FRAME_MIN_INTERVAL;
        }
        for waiter in settled.drain(..) {
            waiter.send(()).ok();
        }
        if !open {
            return;
        }
    }
}

impl RateProjector {
    /// Fold one observation into its agent; false when the delivery watermark already has it.
    fn observe(&mut self, effect: &NotificationPostCommit, effect_bit: u8) -> bool {
        let (Some(observation), Some(agent_id)) =
            (effect_observation(effect), effect_agent_id(effect))
        else {
            return false;
        };
        let delivery = &observation.delivery;
        let new_generation = match self.watermarks.get_mut(&delivery.provider_handle) {
            Some(watermark) if delivery.generation < watermark.generation => return false,
            Some(watermark) if delivery.generation > watermark.generation => {
                *watermark = DeliveryWatermark {
                    generation: delivery.generation,
                    cursor: delivery.cursor,
                    effects: effect_bit,
                };
                true
            }
            Some(watermark) if delivery.cursor < watermark.cursor => return false,
            Some(watermark) if delivery.cursor == watermark.cursor => {
                if watermark.effects & effect_bit != 0 {
                    return false;
                }
                watermark.effects |= effect_bit;
                false
            }
            Some(watermark) => {
                watermark.cursor = delivery.cursor;
                watermark.effects = effect_bit;
                false
            }
            None => {
                self.watermarks.insert(
                    delivery.provider_handle.clone(),
                    DeliveryWatermark {
                        generation: delivery.generation,
                        cursor: delivery.cursor,
                        effects: effect_bit,
                    },
                );
                true
            }
        };
        if new_generation {
            self.agents.remove(agent_id);
        }
        if !self.agents.contains_key(agent_id) {
            self.agents
                .insert(agent_id.to_owned(), ProjectedAgent::default());
        }
        let Some(entry) = self.agents.get_mut(agent_id) else {
            return false;
        };
        entry.provider_handle.clone_from(&delivery.provider_handle);
        let root = observation
            .agent
            .as_ref()
            .and_then(|agent| agent["rootId"].as_str())
            .filter(|root| !root.is_empty())
            .unwrap_or(agent_id);
        if entry.root != root {
            root.clone_into(&mut entry.root);
        }
        project_effect(entry, effect, observation);
        while self.agents.len() > MAX_PROJECTED_AGENTS {
            self.agents.pop_first();
        }
        while self.watermarks.len() > MAX_TOKEN_RATE_WATERMARKS {
            let referenced = self
                .agents
                .values()
                .map(|agent| agent.provider_handle.as_str())
                .collect::<BTreeSet<_>>();
            let oldest = self
                .watermarks
                .keys()
                .find(|provider| !referenced.contains(provider.as_str()))
                .cloned();
            if let Some(oldest) = oldest {
                self.watermarks.remove(&oldest);
            } else {
                break;
            }
        }
        true
    }

    /// The immutable snapshot of every agent with a turn. Entries whose identifiers break the
    /// wire bounds are left out, so one bad identifier cannot stop publication.
    fn snapshot(&self) -> TokenRateSnapshot {
        let mut snapshot = TokenRateSnapshot::empty();
        for (agent_id, state) in &self.agents {
            let (Ok(agent_id), Ok(root_id), Ok(turn_id)) = (
                TokenRateIdentifier::try_from(agent_id.clone()),
                TokenRateIdentifier::try_from(state.root.clone()),
                TokenRateIdentifier::try_from(state.turn.clone()),
            ) else {
                continue;
            };
            let rate = state.rate.clamp(0.0, MAX_TOKEN_RATE);
            let output_tokens =
                (state.actual + state.pending + state.response_duration_pending_tokens).max(0.0);
            if !rate.is_finite() || !output_tokens.is_finite() {
                continue;
            }
            snapshot.records.push(TokenRateRecord {
                agent_id,
                root_id,
                turn_id,
                active: state.active,
                estimated: state.estimated,
                rate: FiniteRate(rate),
                output_tokens: FiniteRate(output_tokens),
            });
        }
        snapshot
    }
}

fn effect_agent_id(effect: &NotificationPostCommit) -> Option<&str> {
    effect_observation(effect)?.agent.as_ref()?["id"].as_str()
}

fn effect_observation(effect: &NotificationPostCommit) -> Option<&NotificationObservation> {
    match effect {
        NotificationPostCommit::Analytics { observation, .. }
        | NotificationPostCommit::AnalyticsStream { observation, .. }
        | NotificationPostCommit::AnalyticsCaptured(observation)
        | NotificationPostCommit::AnalyticsStreamCaptured(observation)
        | NotificationPostCommit::TokenRateStream(observation)
        | NotificationPostCommit::TokenRateAssociation(observation)
        | NotificationPostCommit::TokenRateUsage(observation) => Some(observation),
        NotificationPostCommit::Recovered { .. } => None,
    }
}

fn project_effect(
    state: &mut ProjectedAgent,
    effect: &NotificationPostCommit,
    observation: &NotificationObservation,
) {
    let params = &observation.params;
    let at = observation.observed_at;
    match effect {
        NotificationPostCommit::TokenRateStream(_) => match observation.method.as_str() {
            "provider/generationStarted" => {
                if state.active {
                    state.generation_started = Some(at);
                    state.generation_elapsed = 0.0;
                    state.timing_valid = true;
                }
            }
            "item/agentMessage/delta" | "item/reasoning/textDelta" if state.active => {
                let characters = params["coalescedDeltaCharacters"]
                    .as_u64()
                    .map(|count| count as f64)
                    .or_else(|| {
                        params["delta"]
                            .as_str()
                            .map(|delta| delta.chars().count() as f64)
                    });
                let Some(characters) = characters else {
                    return;
                };
                let tokens = characters * TOKEN_ESTIMATE_TOKENS_PER_CHARACTER;
                state.pending += tokens;
                state.estimated = true;
                state.samples.push_back((at, tokens));
                while state
                    .samples
                    .front()
                    .is_some_and(|(sample_at, _)| at - sample_at > TOKEN_RATE_WINDOW_SECONDS)
                {
                    state.samples.pop_front();
                }
                if state.timing_valid
                    && let Some((first_at, _)) = state.samples.front()
                {
                    let elapsed = (at - first_at).max(1.0);
                    state.rate = round_rate(
                        (state.samples.iter().map(|(_, count)| count).sum::<f64>() / elapsed)
                            .min(MAX_TOKEN_RATE),
                    );
                }
            }
            _ => {}
        },
        NotificationPostCommit::Analytics { .. }
        | NotificationPostCommit::AnalyticsCaptured(_)
        | NotificationPostCommit::TokenRateUsage(_) => match observation.method.as_str() {
            "turn/started" => {
                if let Some(turn) = observed_turn(params, agent_turn(observation)) {
                    if turn != state.turn {
                        state.turn = turn;
                        state.active = true;
                        state.rate = 0.0;
                        state.actual = 0.0;
                        state.pending = 0.0;
                        state.estimated = false;
                        state.baseline = state.lifetime.as_ref().map(|tokens| tokens.output as f64);
                        state.samples.clear();
                        state.response_outputs.clear();
                        state.response_output_total = 0.0;
                        state.tools.clear();
                        state.generation_elapsed = 0.0;
                        state.response_durations.clear();
                        state.response_duration_pending_tokens = 0.0;
                    }
                    state.generation_started = Some(at);
                    state.timing_valid = true;
                }
            }
            "thread/tokenUsage/updated" => project_usage(state, observation),
            "turn/completed"
                if observed_turn(params, Some(&state.turn)).as_deref() == Some(&state.turn) =>
            {
                state.active = false;
                state.estimated = false;
            }
            "turn/completed" => {}
            _ => {}
        },
        NotificationPostCommit::TokenRateAssociation(_) => match observation.method.as_str() {
            "item/started" | "item/completed" => project_item_association(state, observation),
            _ => {}
        },
        NotificationPostCommit::AnalyticsStream { .. }
        | NotificationPostCommit::AnalyticsStreamCaptured(_) => {}
        NotificationPostCommit::Recovered { .. } => {}
    }
}

fn agent_turn(observation: &NotificationObservation) -> Option<&str> {
    observation
        .agent
        .as_ref()
        .and_then(|agent| agent["turnId"].as_str())
}

fn observed_turn<'a>(params: &'a serde_json::Value, fallback: Option<&'a str>) -> Option<String> {
    params["turnId"]
        .as_str()
        .or_else(|| params["turn"]["id"].as_str())
        .or(fallback)
        .filter(|turn| !turn.is_empty())
        .map(str::to_owned)
}

fn project_usage(state: &mut ProjectedAgent, observation: &NotificationObservation) {
    let Some(agent) = observation.agent.as_ref() else {
        return;
    };
    let provider = match ProviderKind::of_record(agent) {
        Ok(ProviderKind::Codex) => UsageProvider::Codex,
        Ok(ProviderKind::Claude) => UsageProvider::Claude,
        Err(_) => return,
    };
    let Ok(usage) = parse_token_usage(provider, &observation.params) else {
        // Invalid evidence cannot replace a live estimate with a rate sample.
        return;
    };
    let turn_id = match &usage {
        UsageObservation::Response { turn_id, .. }
        | UsageObservation::Cumulative { turn_id, .. }
        | UsageObservation::Gap { turn_id, .. } => turn_id.as_deref(),
    };
    let turn = turn_id
        .or_else(|| observation.params["turnId"].as_str())
        .or_else(|| agent_turn(observation));
    if turn != Some(state.turn.as_str()) {
        return;
    }
    project_usage_observation(state, usage, observation.observed_at);
}

fn project_usage_observation(state: &mut ProjectedAgent, usage: UsageObservation, at: f64) {
    match usage {
        UsageObservation::Gap { .. }
        | UsageObservation::Cumulative {
            scope: UsageScope::ClaudeQuery,
            ..
        } => {}
        UsageObservation::Cumulative {
            scope: UsageScope::CodexThread,
            total,
            last,
            ..
        } => {
            let output = total.output as f64;
            if state.baseline.is_none()
                || state
                    .lifetime
                    .as_ref()
                    .is_some_and(|previous| total.decreased_from(previous))
            {
                state.baseline =
                    Some((output - last.map_or(0.0, |tokens| tokens.output as f64)).max(0.0));
            }
            state.lifetime = Some(total);
            if state.response_outputs.is_empty() {
                state.actual = state
                    .actual
                    .max((output - state.baseline.unwrap_or(0.0)).max(0.0));
                state.pending = 0.0;
                // The cumulative total includes output estimated before tool starts.
                // Keep durations for delayed response records, without counting their
                // provisional tokens again in the snapshot.
                for (_, pending) in &mut state.response_durations {
                    *pending = 0.0;
                }
                state.response_duration_pending_tokens = 0.0;
                state.estimated = false;
            }
        }
        UsageObservation::Response {
            response_id,
            tokens,
            ..
        } => {
            let identity = (state.turn.clone(), response_id);
            if state.response_outputs.contains_key(&identity) {
                // The accounting owner records replay conflicts. The rate projection
                // preserves the first value and never consumes timing twice.
                return;
            }
            let response = tokens.output as f64;
            state.response_outputs.insert(identity, tokens);
            state.response_output_total += response;
            state.actual = state.response_output_total;
            let queued_duration =
                state
                    .response_durations
                    .pop_front()
                    .map(|(duration, pending)| {
                        state.response_duration_pending_tokens =
                            (state.response_duration_pending_tokens - pending).max(0.0);
                        duration
                    });
            let duration = queued_duration.or_else(|| {
                state.generation_started.map(|started| {
                    state.generation_elapsed + (at - started).max(MIN_GENERATION_DURATION_SECONDS)
                })
            });
            if state.active
                && state.timing_valid
                && let Some(duration) = duration
            {
                state.rate = round_rate(
                    (response / duration.max(MIN_GENERATION_DURATION_SECONDS)).min(MAX_TOKEN_RATE),
                );
                if queued_duration.is_none() {
                    state.pending = 0.0;
                    state.estimated = false;
                    state.generation_started = Some(at);
                    state.generation_elapsed = 0.0;
                }
            }
        }
    }
}

/// Match Python TokenRates.observe's tool and assistant-item timing transitions.
/// These associations affect the volatile HTTP snapshot only; receipt identity
/// watermarks make replay of the same transition harmless.
fn project_item_association(state: &mut ProjectedAgent, observation: &NotificationObservation) {
    let params = &observation.params;
    let agent = observation.agent.as_ref();
    let turn = observed_turn(params, agent.and_then(|agent| agent["turnId"].as_str()));
    if observation.method == "item/started"
        && !state.active
        && agent.is_some_and(|agent| agent["inFlight"].as_bool() == Some(true))
        && let Some(turn) = turn.as_deref()
    {
        state.response_outputs.clear();
        state.response_output_total = 0.0;
        state.turn = turn.to_owned();
        state.active = true;
        state.estimated = false;
        state.rate = 0.0;
        state.actual = 0.0;
        state.pending = 0.0;
        state.baseline = state.lifetime.as_ref().map(|tokens| tokens.output as f64);
        state.timing_valid = false;
    }
    if !state.active || turn.as_deref() != Some(state.turn.as_str()) {
        return;
    }
    let item = &params["item"];
    if observation.method == "item/started" && item["type"] == "agentMessage" && !state.timing_valid
    {
        state.generation_started = Some(observation.observed_at);
        state.generation_elapsed = 0.0;
        state.timing_valid = true;
    }
    let item_id = item["id"].as_str().or_else(|| params["itemId"].as_str());
    let item_type = item["type"].as_str().unwrap_or_default();
    let is_tool = !matches!(
        item_type,
        "userMessage"
            | "agentMessage"
            | "reasoning"
            | "plan"
            | "contextCompaction"
            | "compactionSnapshot"
    );
    if is_tool && let Some(item_id) = item_id {
        if observation.method == "item/started" && !state.tools.contains(item_id) {
            if state.tools.is_empty()
                && let Some(started) = state.generation_started.take()
            {
                let elapsed = (observation.observed_at - started).max(0.0);
                state.generation_elapsed += elapsed;
                append_response_duration(state, elapsed, state.pending);
                state.pending = 0.0;
                state.samples.clear();
            }
            state.tools.insert(item_id.to_owned());
        } else if observation.method == "item/completed"
            && state.tools.remove(item_id)
            && state.tools.is_empty()
        {
            state.generation_started = Some(observation.observed_at);
            state.timing_valid = true;
        }
    }
}

fn append_response_duration(state: &mut ProjectedAgent, duration: f64, pending: f64) {
    if state.response_durations.len() == MAX_PENDING_RESPONSE_DURATION_ENTRIES
        && let Some((_, evicted_pending)) = state.response_durations.pop_front()
    {
        state.response_duration_pending_tokens -= evicted_pending;
    }
    state.response_durations.push_back((duration, pending));
    state.response_duration_pending_tokens += pending;
}

fn round_rate(value: f64) -> f64 {
    (value * 100.0).round() / 100.0
}

impl Default for TokenRateHub {
    fn default() -> Self {
        Self::new()
    }
}

fn valid_id(value: &str) -> bool {
    !value.is_empty()
        && value
            .chars()
            .take(TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS + 1)
            .count()
            <= TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PROJECTED_AGENTS, MAX_TOKEN_RATE_WATERMARKS, ProjectedAgent, RateProjector,
        TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS, TOKEN_RATE_MAX_ENTRIES, TokenRateHub,
    };
    use crate::notification_dispatch::{
        NotificationObservation, NotificationPostCommit, ProviderDeliveryIdentity, SharedJson,
    };
    use serde_json::json;

    /// Every subscriber of one published snapshot shares one serialization, and it equals the
    /// snapshot's own JSON members.
    #[test]
    fn subscribers_share_one_serialization_of_a_snapshot() {
        let hub = TokenRateHub::new();
        let (first, second) = (hub.subscribe(), hub.subscribe());
        hub.publish(json!({
            "records": [{"agentId":"agent","rootId":"root","turnId":"turn","active":true,"estimated":false,"rate":2.5,"outputTokens":7.0}]
        }))
        .expect("publish");
        let (a, b) = (first.borrow().wire(), second.borrow().wire());
        assert!(std::sync::Arc::ptr_eq(&a, &b));
        let expected = hub.snapshot().to_value();
        assert_eq!(
            serde_json::from_str::<Vec<super::TokenRateRecord>>(&a.records).expect("records"),
            hub.snapshot().records
        );
        assert_eq!(expected["records"].as_array().expect("records").len(), 1);
    }

    #[test]
    fn delta_borrows_sorted_records_without_serializing_the_full_snapshot() {
        let record = |id: &str, root: &str, rate: f64| {
            json!({
                "agentId":id,"rootId":root,"turnId":"turn","active":true,"estimated":false,"rate":rate,"outputTokens":10.0
            })
        };
        let old = super::TokenRateSnapshot::from_value(json!({"records":[record("z", "root", 1.0), record("a", "root", 1.0), record("keep", "root", 1.0)]})).expect("old");
        let moved = record("a", "new-root", 1.0);
        let added = record("b", "root", 2.0);
        let current = super::TokenRateSnapshot::from_value(
            json!({"records":[record("keep", "root", 1.0), added.clone(), moved.clone()]}),
        )
        .expect("current");
        assert_eq!(
            serde_json::to_value(current.delta_since(&old)).expect("delta"),
            json!({"records":[moved, added],"removed":["z"]})
        );
        assert_eq!(
            serde_json::to_value(current.delta_since(&current)).expect("unchanged"),
            json!({"records":[],"removed":[]})
        );
        assert!(
            current.wire.0.get().is_none(),
            "delta path must not allocate or serialize a whole snapshot"
        );
        assert!(old.wire.0.get().is_none());
    }

    fn usage_effect(
        agent_id: &str,
        provider: &str,
        generation: u64,
        cursor: u64,
        turn: &str,
    ) -> NotificationPostCommit {
        NotificationPostCommit::TokenRateUsage(NotificationObservation {
            agent: Some(SharedJson::new(
                json!({"id":agent_id,"rootId":agent_id,"turnId":turn}),
            )),
            method: "turn/started".into(),
            params: SharedJson::new(json!({"turnId":turn})),
            delivery: ProviderDeliveryIdentity {
                provider_handle: provider.into(),
                generation,
                cursor,
            },
            observed_at: cursor as f64,
            provider_at: cursor as f64,
        })
    }

    fn response_usage(turn: &str, response: &str, output: u64) -> serde_json::Value {
        json!({"turnId":turn,"responseId":response,"usageSource":"claudeResponse",
            "requestUsage":{"inputTokens":0,"outputTokens":output},
            "responseOutputTokens":output})
    }

    fn usage_at(projector: &mut RateProjector, cursor: u64, at: f64, params: serde_json::Value) {
        let mut effect = usage_effect("agent", "provider", 1, cursor, "turn");
        if let NotificationPostCommit::TokenRateUsage(observation) = &mut effect {
            observation.agent = Some(SharedJson::new(
                json!({"id":"agent","rootId":"agent","turnId":"turn","provider":"claude"}),
            ));
            observation.method = "thread/tokenUsage/updated".into();
            observation.params = SharedJson::new(params);
            observation.observed_at = at;
        }
        assert!(observe(projector, &effect));
    }

    #[test]
    fn repeated_response_usage_with_fresh_receipt_keeps_its_first_duration() {
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider", 1, 1, "turn")
        ));
        usage_at(
            &mut projector,
            2,
            11.0,
            response_usage("turn", "response", 100),
        );
        assert_eq!(projector.snapshot().records[0].rate.0, 10.0);
        usage_at(
            &mut projector,
            3,
            12.0,
            response_usage("turn", "response", 100),
        );
        assert_eq!(projector.snapshot().records[0].rate.0, 10.0);
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 100.0);
    }

    #[test]
    fn old_turn_usage_cannot_change_the_live_turn() {
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider", 1, 1, "turn")
        ));
        usage_at(
            &mut projector,
            2,
            11.0,
            response_usage("turn", "response", 100),
        );
        let current = projector.snapshot();
        usage_at(
            &mut projector,
            3,
            12.0,
            response_usage("old-turn", "response", 999),
        );
        assert_eq!(projector.snapshot(), current);
    }

    #[test]
    fn response_only_totals_survive_the_old_response_cache_bound() {
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider", 1, 1, "turn")
        ));
        for index in 0..1025 {
            usage_at(
                &mut projector,
                index + 2,
                index as f64 + 2.0,
                response_usage("turn", &format!("response-{index:04}"), 1),
            );
        }
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 1025.0);
        usage_at(
            &mut projector,
            1027,
            1027.0,
            response_usage("turn", "response-0000", 1),
        );
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 1025.0);
    }

    #[test]
    fn final_cumulative_usage_does_not_replace_the_response_rate() {
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider", 1, 1, "turn")
        ));
        usage_at(
            &mut projector,
            2,
            11.0,
            response_usage("turn", "response", 100),
        );
        usage_at(
            &mut projector,
            3,
            12.0,
            json!({"turnId":"turn","tokenUsage":{"total":{"inputTokens":0,"outputTokens":100},"last":{"inputTokens":0,"outputTokens":100}}}),
        );
        assert_eq!(projector.snapshot().records[0].rate.0, 10.0);
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 100.0);
    }

    fn typed_usage(usage: studio_runtime_schema::usage::UsageObservation) -> serde_json::Value {
        json!({"studioUsage":usage})
    }

    fn typed_response(turn: &str, response: &str, output: u64) -> serde_json::Value {
        typed_usage(studio_runtime_schema::usage::UsageObservation::Response {
            response_id: response.into(),
            turn_id: Some(turn.into()),
            model: "claude".into(),
            tokens: studio_runtime_schema::usage::UsageTokens {
                output,
                ..Default::default()
            },
            evidence: studio_runtime_schema::usage::UsageEvidence::Live,
            parent_tool_use_id: None,
            model_context_window: None,
        })
    }

    #[test]
    fn typed_responses_keep_first_values_and_do_not_consume_the_next_duration_on_replay() {
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider", 1, 1, "turn")
        ));
        usage_at(
            &mut projector,
            2,
            11.0,
            typed_response("turn", "first", 100),
        );
        super::append_response_duration(
            projector.agents.get_mut("agent").expect("agent"),
            2.0,
            20.0,
        );
        usage_at(
            &mut projector,
            3,
            12.0,
            typed_response("turn", "first", 999),
        );
        let state = &projector.agents["agent"];
        assert_eq!(state.rate, 10.0);
        assert_eq!(state.actual, 100.0);
        assert_eq!(state.response_durations.front(), Some(&(2.0, 20.0)));
        usage_at(
            &mut projector,
            4,
            13.0,
            typed_response("turn", "second", 50),
        );
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 150.0);
        assert_eq!(projector.snapshot().records[0].rate.0, 25.0);
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider", 1, 5, "next-turn")
        ));
        usage_at(
            &mut projector,
            6,
            15.0,
            typed_response("next-turn", "first", 30),
        );
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 30.0);
        assert_eq!(projector.snapshot().records[0].rate.0, 3.0);
        let current = projector.snapshot();
        usage_at(&mut projector, 7, 16.0, typed_response("turn", "late", 500));
        assert_eq!(projector.snapshot(), current);
    }

    #[test]
    fn typed_gaps_and_cumulative_fallback_never_sample_response_timing() {
        use studio_runtime_schema::usage::{
            UsageGapReason, UsageObservation, UsageScope, UsageTokens,
        };
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider", 1, 1, "turn")
        ));
        usage_at(
            &mut projector,
            2,
            5.0,
            typed_usage(UsageObservation::Gap {
                response_id: "gap".into(),
                turn_id: Some("turn".into()),
                model: None,
                reason: UsageGapReason::StreamInterrupted,
            }),
        );
        assert_eq!(projector.agents["agent"].generation_started, Some(1.0));
        let cumulative = |turn: &str, output| {
            typed_usage(UsageObservation::Cumulative {
                scope: UsageScope::CodexThread,
                turn_id: Some(turn.into()),
                total: UsageTokens {
                    output,
                    ..Default::default()
                },
                last: Some(UsageTokens {
                    output: 20,
                    ..Default::default()
                }),
                model_context_window: None,
                models: None,
            })
        };
        usage_at(&mut projector, 3, 6.0, cumulative("turn", 120));
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 20.0);
        assert_eq!(projector.snapshot().records[0].rate.0, 0.0);
        assert_eq!(projector.agents["agent"].generation_started, Some(1.0));
        usage_at(
            &mut projector,
            4,
            11.0,
            typed_response("turn", "response", 100),
        );
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 100.0);
        assert_eq!(projector.snapshot().records[0].rate.0, 10.0);
        usage_at(&mut projector, 5, 12.0, cumulative("turn", 999));
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 100.0);
        assert_eq!(projector.snapshot().records[0].rate.0, 10.0);
        let current = projector.snapshot();
        usage_at(&mut projector, 6, 13.0, cumulative("old-turn", 2000));
        assert_eq!(projector.snapshot(), current);
    }

    #[test]
    fn restored_claude_query_total_keeps_the_pending_estimate_until_response() {
        use studio_runtime_schema::usage::{UsageObservation, UsageScope, UsageTokens};
        let mut projector = RateProjector::default();
        for (effect, bit) in provider_turn("provider", "agent", 0).into_iter().take(4) {
            assert!(projector.observe(&effect, bit));
        }
        let before = projector.snapshot();
        assert!(before.records[0].estimated);
        assert!(projector.agents["agent"].response_duration_pending_tokens > 0.0);
        usage_at(
            &mut projector,
            7,
            7.0,
            typed_usage(UsageObservation::Cumulative {
                scope: UsageScope::ClaudeQuery,
                turn_id: Some("turn".into()),
                total: UsageTokens {
                    output: 50_000,
                    ..Default::default()
                },
                last: None,
                model_context_window: None,
                models: None,
            }),
        );
        assert_eq!(projector.agents["agent"].actual, 0.0);
        let after = projector.snapshot();
        assert_eq!(
            after.records[0].output_tokens,
            before.records[0].output_tokens
        );
        assert_eq!(after.records[0].rate, before.records[0].rate);
        assert!(after.records[0].estimated);
        usage_at(
            &mut projector,
            8,
            8.0,
            typed_response("turn", "response", 100),
        );
        assert_eq!(projector.agents["agent"].actual, 100.0);
    }

    #[test]
    fn reasoning_counter_decrease_resets_the_cumulative_rate_baseline() {
        use studio_runtime_schema::usage::{UsageObservation, UsageScope, UsageTokens};
        for (before, after, expected_output) in [
            (Some(90), Some(20), 100.),
            (Some(90), None, 150.),
            (None, Some(20), 150.),
        ] {
            let mut projector = RateProjector::default();
            assert!(observe(
                &mut projector,
                &usage_effect("agent", "provider", 1, 1, "turn")
            ));
            for (cursor, input, output, reasoning, last_output) in
                [(2, 100, 100, before, 100), (3, 110, 150, after, 50)]
            {
                usage_at(
                    &mut projector,
                    cursor,
                    cursor as f64,
                    typed_usage(UsageObservation::Cumulative {
                        scope: UsageScope::CodexThread,
                        turn_id: Some("turn".into()),
                        total: UsageTokens {
                            uncached_input: input,
                            output,
                            reasoning_output: reasoning,
                            ..Default::default()
                        },
                        last: Some(UsageTokens {
                            output: last_output,
                            ..Default::default()
                        }),
                        model_context_window: None,
                        models: None,
                    }),
                );
            }
            assert_eq!(
                projector.snapshot().records[0].output_tokens.0,
                expected_output,
                "known reasoning reset starts a new cumulative baseline"
            );
        }
    }

    #[test]
    fn cumulative_fallback_reconciles_queued_stream_estimates() {
        let mut projector = RateProjector::default();
        for (effect, bit) in provider_turn("provider", "agent", 0).into_iter().take(4) {
            assert!(projector.observe(&effect, bit));
        }
        assert!(projector.agents["agent"].response_duration_pending_tokens > 0.0);
        usage_at(
            &mut projector,
            7,
            7.0,
            typed_usage(studio_runtime_schema::usage::UsageObservation::Cumulative {
                scope: studio_runtime_schema::usage::UsageScope::CodexThread,
                turn_id: Some("turn".into()),
                total: studio_runtime_schema::usage::UsageTokens {
                    output: 30,
                    ..Default::default()
                },
                last: Some(studio_runtime_schema::usage::UsageTokens {
                    output: 30,
                    ..Default::default()
                }),
                model_context_window: None,
                models: None,
            }),
        );
        assert_eq!(projector.snapshot().records[0].output_tokens.0, 30.0);
        assert_eq!(
            projector.agents["agent"].response_duration_pending_tokens,
            0.0
        );
    }

    #[test]
    fn legacy_total_only_aggregate_cannot_change_the_live_projection() {
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider", 1, 1, "turn")
        ));
        usage_at(
            &mut projector,
            2,
            11.0,
            typed_response("turn", "response", 100),
        );
        let current = projector.snapshot();
        usage_at(
            &mut projector,
            3,
            12.0,
            json!({
                "turnId":"turn", "turnOutputTokens":999, "responseOutputTokens":100,
                "tokenUsage":{"total":{"totalTokens":5000}, "last":{"inputTokens":10,"outputTokens":100}}
            }),
        );
        assert_eq!(projector.snapshot(), current);
        assert_eq!(projector.agents["agent"].generation_started, Some(11.0));
    }

    #[test]
    fn token_rate_projection_constants_keep_the_established_bounds() {
        assert_eq!(super::MAX_TOKEN_RATE, 1_000.0);
        assert_eq!(super::MIN_GENERATION_DURATION_SECONDS, 0.25);
        assert_eq!(super::TOKEN_ESTIMATE_TOKENS_PER_CHARACTER, 0.25);
        assert_eq!(super::TOKEN_RATE_WINDOW_SECONDS, 4.0);
        assert_eq!(super::MAX_PENDING_RESPONSE_DURATION_ENTRIES, 1024);
    }

    #[test]
    fn pending_response_durations_match_python_bound() {
        let mut state = super::ProjectedAgent::default();
        for duration in 0..=super::MAX_PENDING_RESPONSE_DURATION_ENTRIES {
            super::append_response_duration(&mut state, duration as f64, 1.0);
        }
        assert_eq!(state.response_durations.len(), 1024);
        assert_eq!(state.response_durations.front(), Some(&(1.0, 1.0)));
        assert_eq!(state.response_durations.back(), Some(&(1024.0, 1.0)));
        assert_eq!(state.response_duration_pending_tokens, 1024.0);
    }

    #[test]
    fn delayed_usage_keeps_the_newer_response_estimate_and_timing() {
        let mut state = super::ProjectedAgent {
            active: true,
            estimated: true,
            pending: 20.0,
            turn: "turn".into(),
            timing_valid: true,
            generation_started: Some(5.0),
            generation_elapsed: 2.0,
            response_durations: [(1.0, 10.0)].into(),
            response_duration_pending_tokens: 10.0,
            ..super::ProjectedAgent::default()
        };

        super::project_usage_observation(&mut state, serde_json::from_value(json!({"kind":"response","responseId":"response","turnId":"turn","model":"claude","evidence":"live","tokens":{"uncachedInput":0,"cacheReadInput":0,"cacheWriteInput":0,"output":10}})).expect("typed response"), 8.0);

        assert_eq!(state.actual, 10.0);
        assert_eq!(state.pending, 20.0);
        assert!(state.estimated);
        assert_eq!(state.generation_started, Some(5.0));
        assert_eq!(state.generation_elapsed, 2.0);
        assert_eq!(state.response_duration_pending_tokens, 0.0);
    }

    #[test]
    fn item_associations_track_tool_timing() {
        let mut state = super::ProjectedAgent {
            active: true,
            turn: "turn".into(),
            timing_valid: true,
            generation_started: Some(1.0),
            pending: 8.0,
            ..super::ProjectedAgent::default()
        };
        let observation = |method: &str, at: f64| super::NotificationObservation {
            agent: Some(SharedJson::new(
                json!({"id":"agent","inFlight":true,"turnId":"turn"}),
            )),
            method: method.into(),
            params: SharedJson::new(
                json!({"turnId":"turn","item":{"id":"tool","type":"functionCall"}}),
            ),
            delivery: crate::notification_dispatch::ProviderDeliveryIdentity {
                provider_handle: "provider".into(),
                generation: 1,
                cursor: at as u64,
            },
            observed_at: at,
            provider_at: at,
        };
        let started = observation("item/started", 5.0);
        super::project_effect(
            &mut state,
            &crate::notification_dispatch::NotificationPostCommit::TokenRateAssociation(
                started.clone(),
            ),
            &started,
        );
        assert!(state.tools.contains("tool"));
        assert_eq!(state.generation_started, None);
        assert_eq!(state.response_duration_pending_tokens, 8.0);

        let completed = observation("item/completed", 6.0);
        super::project_effect(
            &mut state,
            &crate::notification_dispatch::NotificationPostCommit::TokenRateAssociation(
                completed.clone(),
            ),
            &completed,
        );
        assert!(state.tools.is_empty());
        assert_eq!(state.generation_started, Some(6.0));
    }

    #[tokio::test]
    async fn latest_snapshot_is_coalesced_for_subscribers() {
        let hub = TokenRateHub::new();
        let mut subscriber = hub.subscribe();
        let value = json!({"records":[{"agentId":"agent-a","rootId":"root","turnId":"turn-a","active":true,
            "estimated":false,"rate":8.5,"outputTokens":17}]});
        assert!(hub.publish(value.clone()).expect("valid snapshot"));
        assert!(!hub.publish(value.clone()).expect("duplicate snapshot"));
        assert!(subscriber.changed().await.is_ok());
        assert_eq!(
            subscriber.borrow_and_update().records[0].output_tokens.0,
            17.0
        );
    }

    #[test]
    fn malformed_or_oversized_snapshots_are_refused() {
        let hub = TokenRateHub::new();
        assert!(
            hub.publish(json!({"records":[{"agentId":"agent-a","rootId":"root","turnId":"turn-a","active":true,
            "estimated":false,"rate":-1,"outputTokens":17}]}))
                .is_err()
        );
        assert!(hub.publish(json!({"records":[],"extra":true})).is_err());
    }

    /// A usage observation folded into the projector, as the projector task does.
    fn observe(projector: &mut RateProjector, effect: &NotificationPostCommit) -> bool {
        projector.observe(effect, 1)
    }

    #[test]
    fn delivery_watermark_survives_projection_eviction_and_acknowledgement_loss() {
        let mut projector = RateProjector::default();
        let original = usage_effect("agent-0000", "provider-a", 1, 1, "turn-a");
        assert!(observe(&mut projector, &original));
        let after_publish = projector.snapshot();
        assert!(
            !observe(&mut projector, &original),
            "retry after lost acknowledgement"
        );
        assert!(
            !observe(
                &mut projector,
                &usage_effect("agent-other", "provider-a", 1, 1, "turn-other")
            ),
            "provider identity deduplicates across replay"
        );
        assert_eq!(projector.snapshot(), after_publish);

        for index in 1..MAX_PROJECTED_AGENTS {
            projector
                .agents
                .insert(format!("agent-{index:04}"), ProjectedAgent::default());
        }
        assert!(observe(
            &mut projector,
            &usage_effect("agent-zzzz", "provider-b", 1, 1, "turn-b")
        ));
        assert!(
            !projector
                .snapshot()
                .records
                .iter()
                .any(|record| record.agent_id.0 == "agent-0000")
        );
        assert!(
            !observe(&mut projector, &original),
            "replay after projection eviction"
        );
        assert!(
            !projector
                .snapshot()
                .records
                .iter()
                .any(|record| record.agent_id.0 == "agent-0000")
        );
    }

    #[test]
    fn watermark_cap_preserves_provider_of_a_live_projection() {
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("z-agent", "a-provider", 1, 1, "old-turn")
        ));
        assert!(observe(
            &mut projector,
            &usage_effect("z-agent", "a-provider", 1, 2, "current-turn")
        ));
        for index in 0..(MAX_TOKEN_RATE_WATERMARKS - 1) {
            projector.watermarks.insert(
                format!("b-provider-{index:04}"),
                super::DeliveryWatermark {
                    generation: 1,
                    cursor: 1,
                    effects: 1,
                },
            );
        }
        assert_eq!(projector.watermarks.len(), MAX_TOKEN_RATE_WATERMARKS);

        assert!(observe(
            &mut projector,
            &usage_effect("a-agent", "z-provider", 1, 1, "other-turn")
        ));
        assert_eq!(projector.watermarks.len(), MAX_TOKEN_RATE_WATERMARKS);
        assert!(projector.watermarks.contains_key("a-provider"));
        assert!(!projector.watermarks.contains_key("b-provider-0000"));

        let current = projector.snapshot();
        assert!(
            !observe(
                &mut projector,
                &usage_effect("z-agent", "a-provider", 1, 1, "old-turn")
            ),
            "old committed delivery remains deduplicated"
        );
        assert_eq!(projector.snapshot(), current);
        assert_eq!(
            projector
                .snapshot()
                .records
                .iter()
                .find(|record| record.agent_id.0 == "z-agent")
                .expect("agent")
                .turn_id
                .0,
            "current-turn"
        );
    }

    #[test]
    fn an_older_generation_cannot_replace_a_newer_projection() {
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider-a", 9, 1, "new-turn")
        ));
        let current = projector.snapshot();
        assert!(!observe(
            &mut projector,
            &usage_effect("agent", "provider-a", 8, 99, "old-turn")
        ));
        assert_eq!(projector.snapshot(), current);
        assert_eq!(projector.snapshot().records[0].turn_id.0, "new-turn");
    }

    /// A mixed turn of both providers: Codex streams raw deltas, the Claude bridge streams
    /// coalesced character counts; both report usage and tool items.
    fn provider_turn(provider: &str, agent: &str, base: u64) -> Vec<(NotificationPostCommit, u8)> {
        let observation = |cursor: u64, method: &str, params: serde_json::Value| {
            NotificationObservation {
                agent: Some(SharedJson::new(
                    json!({"id":agent,"rootId":"root","turnId":"turn","inFlight":true,"provider":if provider.starts_with("claude") { "claude" } else { "codex" }}),
                )),
                method: method.into(),
                params: SharedJson::new(params),
                delivery: ProviderDeliveryIdentity {
                    provider_handle: provider.into(),
                    generation: 1,
                    cursor: base + cursor,
                },
                observed_at: (base + cursor) as f64 * 0.1,
                provider_at: 0.0,
            }
        };
        let delta = if provider.starts_with("claude") {
            json!({"coalescedDeltaCharacters":40})
        } else {
            json!({"delta":"some streamed answer text"})
        };
        vec![
            (
                NotificationPostCommit::TokenRateUsage(observation(
                    1,
                    "turn/started",
                    json!({"turnId":"turn"}),
                )),
                1,
            ),
            (
                NotificationPostCommit::TokenRateStream(observation(
                    2,
                    "item/agentMessage/delta",
                    delta.clone(),
                )),
                2,
            ),
            (
                NotificationPostCommit::TokenRateStream(observation(
                    3,
                    "item/agentMessage/delta",
                    delta,
                )),
                2,
            ),
            (
                NotificationPostCommit::TokenRateAssociation(observation(
                    4,
                    "item/started",
                    json!({"turnId":"turn","item":{"id":"tool","type":"commandExecution"}}),
                )),
                4,
            ),
            (
                NotificationPostCommit::TokenRateAssociation(observation(
                    5,
                    "item/completed",
                    json!({"turnId":"turn","item":{"id":"tool","type":"commandExecution"}}),
                )),
                4,
            ),
            (
                NotificationPostCommit::TokenRateUsage(observation(
                    6,
                    "thread/tokenUsage/updated",
                    json!({"threadId":"thread","turnId":"turn","tokenUsage":{"total":{"inputTokens":0,"cachedInputTokens":0,"outputTokens":30,"reasoningOutputTokens":0,"totalTokens":30},"last":{"inputTokens":0,"cachedInputTokens":0,"outputTokens":12,"reasoningOutputTokens":0,"totalTokens":12}}}),
                )),
                1,
            ),
        ]
    }

    /// The projector task's coalesced snapshot equals the snapshot built synchronously from
    /// the same observations, for Claude and Codex agents interleaved.
    #[tokio::test(start_paused = true)]
    async fn projector_task_snapshot_equals_the_direct_projection() {
        let hub = TokenRateHub::new();
        let mut direct = RateProjector::default();
        let observations = (0..8)
            .flat_map(|index| {
                let provider = if index % 2 == 0 { "claude" } else { "codex" };
                provider_turn(
                    &format!("{provider}-{index}"),
                    &format!("agent-{index}"),
                    10,
                )
            })
            .collect::<Vec<_>>();
        for (effect, bit) in observations {
            direct.observe(&effect, bit);
            hub.publish_observation(effect).expect("queue observation");
        }
        hub.settled().await;
        let snapshot = hub.snapshot();
        assert_eq!(snapshot.records.len(), 8);
        assert_eq!(snapshot, direct.snapshot());
        assert_eq!(snapshot.to_value(), direct.snapshot().to_value());
    }

    /// A burst of observations publishes once at its start and once per frame interval after.
    #[tokio::test(start_paused = true)]
    async fn an_observation_burst_publishes_one_snapshot_per_frame_interval() {
        let changes =
            std::sync::Arc::new(crate::change_notifications::ChangeNotificationHub::new());
        let hub = TokenRateHub::with_change_hub(std::sync::Arc::clone(&changes));
        let mut subscriber = hub.subscribe();
        let turn = provider_turn("codex", "agent", 0);
        let (started, _) = turn[0].clone();
        hub.publish_observation(started).expect("queue start");
        subscriber.changed().await.expect("leading frame");
        let leading = tokio::time::Instant::now();
        for cursor in 0..100 {
            let mut delta = turn[1].0.clone();
            if let NotificationPostCommit::TokenRateStream(observation) = &mut delta {
                observation.delivery.cursor = 10 + cursor;
            }
            hub.publish_observation(delta).expect("queue delta");
        }
        subscriber.changed().await.expect("trailing frame");
        assert!(leading.elapsed() >= super::SYNC_FRAME_MIN_INTERVAL);
        hub.settled().await;
        assert!(!subscriber.has_changed().expect("hub open"));
        assert_eq!(
            changes.snapshot().revision,
            2,
            "one leading and one trailing frame"
        );
        assert!(subscriber.borrow().records[0].output_tokens.0 > 0.0);
    }

    #[test]
    fn record_bounds_and_duplicate_identities_are_enforced_atomically() {
        let hub = TokenRateHub::new();
        let id = "💠".repeat(TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS);
        let record = json!({"agentId":id,"rootId":id,"turnId":id,"active":true,"estimated":false,"rate":0,"outputTokens":0});
        assert!(
            hub.publish(json!({"records":[record]}))
                .expect("Unicode boundary")
        );
        let saved = hub.snapshot();
        for field in ["agentId", "rootId", "turnId"] {
            let mut bad = record.clone();
            bad[field] = json!("💠".repeat(TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS + 1));
            assert!(hub.publish(json!({"records":[bad]})).is_err());
        }
        assert!(hub.publish(json!({"records":[record,record]})).is_err());
        assert_eq!(hub.snapshot(), saved);
        let records = (0..TOKEN_RATE_MAX_ENTRIES)
            .map(|index| {
                json!({"agentId":format!("a{index}"),"rootId":"root","turnId":"turn",
                    "active":true,"estimated":false,"rate":0,"outputTokens":0})
            })
            .collect::<Vec<_>>();
        assert!(
            hub.publish(json!({"records":records}))
                .expect("entry boundary")
        );
        let saved = hub.snapshot();
        let mut overfull = records;
        overfull.push(record);
        assert!(hub.publish(json!({"records":overfull})).is_err());
        assert!(
            hub.snapshot() == saved,
            "overfull rejection preserves the snapshot"
        );
    }

    #[test]
    fn projector_records_own_root_membership_and_only_serialize_each_agent_once() {
        let mut projector = RateProjector::default();
        assert!(observe(
            &mut projector,
            &usage_effect("agent", "provider", 1, 1, "turn")
        ));
        assert_eq!(projector.snapshot().records[0].root_id.0, "agent");
        let mut moved = usage_effect("agent", "provider", 1, 2, "turn");
        if let NotificationPostCommit::TokenRateUsage(observation) = &mut moved {
            observation.agent = Some(SharedJson::new(
                json!({"id":"agent","rootId":"other","turnId":"turn"}),
            ));
        }
        assert!(observe(&mut projector, &moved));
        let snapshot = projector.snapshot();
        assert_eq!(snapshot.records.len(), 1);
        assert_eq!(snapshot.records[0].root_id.0, "other");
        projector.agents.remove("agent");
        assert!(projector.snapshot().is_empty());
    }
}
