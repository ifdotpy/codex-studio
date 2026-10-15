//! Bounded, latest-value token-rate publication for resource streams.

use std::collections::VecDeque;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex};

use crate::change_notifications::ChangeNotificationHub;
use crate::notification_dispatch::{NotificationObservation, NotificationPostCommit};
use serde::{Deserialize, Serialize};
use studio_runtime_schema::{
    TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS, TOKEN_RATE_MAX_ENTRIES, TOKEN_RATE_MAX_TOTAL_ENTRIES,
};
use thiserror::Error;
use tokio::sync::watch;

#[derive(Clone, Debug, Default, Deserialize, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct TokenRateValue {
    turn_id: String,
    active: bool,
    estimated: bool,
    rate: FiniteRate,
    output_tokens: FiniteRate,
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
    rates: BTreeMap<String, TokenRateValue>,
    teams: BTreeMap<String, BTreeMap<String, TokenRateValue>>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum TokenRateError {
    #[error("token-rate snapshot is invalid")]
    InvalidSnapshot,
    #[error("token-rate publication is unavailable")]
    Unavailable,
}

/// A bounded watch channel; slow subscribers always receive the newest snapshot.
#[derive(Clone)]
pub struct TokenRateHub {
    sender: watch::Sender<TokenRateSnapshot>,
    projector: Arc<Mutex<RateProjector>>,
    changes: Option<Arc<ChangeNotificationHub>>,
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
    lifetime: Option<f64>,
    baseline: Option<f64>,
    timing_valid: bool,
    generation_started: Option<f64>,
    samples: VecDeque<(f64, f64)>,
    response_outputs: BTreeMap<String, f64>,
    tools: BTreeSet<String>,
    generation_elapsed: f64,
    response_durations: VecDeque<(f64, f64)>,
    response_duration_pending_tokens: f64,
}

// Parity owner: apps/server/src/codex_token_rate.py (MAX_RATE, MIN_DURATION,
// text() estimate and rolling window); the unit test below keeps both aligned.
const MAX_TOKEN_RATE: f64 = 1_000.0;
const MIN_GENERATION_DURATION_SECONDS: f64 = 0.25;
const TOKEN_ESTIMATE_TOKENS_PER_CHARACTER: f64 = 0.25;
const TOKEN_RATE_WINDOW_SECONDS: f64 = 4.0;
const MAX_PROJECTED_AGENTS: usize = TOKEN_RATE_MAX_ENTRIES;
// Retain provider delivery watermarks across snapshot eviction.
const MAX_TOKEN_RATE_WATERMARKS: usize = MAX_PROJECTED_AGENTS * 2;
// Parity owner: apps/server/src/codex_token_rate.py::MAX_PENDING_RESPONSE_DURATION_ENTRIES.
const MAX_PENDING_RESPONSE_DURATION_ENTRIES: usize = 1024;

impl TokenRateSnapshot {
    pub fn from_value(value: serde_json::Value) -> Result<Self, TokenRateError> {
        let snapshot: Self =
            serde_json::from_value(value).map_err(|_| TokenRateError::InvalidSnapshot)?;
        let total_rate_entries =
            snapshot.rates.len() + snapshot.teams.values().map(BTreeMap::len).sum::<usize>();
        if snapshot.rates.len() > TOKEN_RATE_MAX_ENTRIES
            || snapshot.teams.len() > TOKEN_RATE_MAX_ENTRIES
            || total_rate_entries > TOKEN_RATE_MAX_TOTAL_ENTRIES
            || snapshot
                .rates
                .iter()
                .any(|(id, rate)| !valid_id(id) || !valid_rate(rate))
            || snapshot.teams.iter().any(|(root, team)| {
                !valid_id(root)
                    || team.len() > TOKEN_RATE_MAX_ENTRIES
                    || team
                        .iter()
                        .any(|(id, rate)| !valid_id(id) || !valid_rate(rate))
            })
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
        serde_json::to_value(self).unwrap_or_else(|_| serde_json::json!({"rates":{},"teams":{}}))
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.rates.is_empty() && self.teams.is_empty()
    }
}

impl TokenRateHub {
    #[must_use]
    pub fn new() -> Self {
        Self {
            sender: watch::channel(TokenRateSnapshot::empty()).0,
            projector: Arc::default(),
            changes: None,
        }
    }

    /// Bind token-only publications to the shared cursor without invalidating resources.
    #[must_use]
    pub fn with_change_hub(changes: Arc<ChangeNotificationHub>) -> Self {
        Self {
            changes: Some(changes),
            ..Self::new()
        }
    }

    #[must_use]
    pub fn snapshot(&self) -> TokenRateSnapshot {
        self.sender.borrow().clone()
    }

    #[must_use]
    pub fn subscribe(&self) -> watch::Receiver<TokenRateSnapshot> {
        self.sender.subscribe()
    }

    /// Project one committed provider observation into HTTP sync. The exact
    /// provider/generation/cursor/effect watermark stays with this owner even
    /// when its bounded snapshot entry is evicted.
    pub fn publish_observation(
        &self,
        effect: NotificationPostCommit,
    ) -> Result<bool, TokenRateError> {
        let Some(observation) = effect_observation(&effect) else {
            return Err(TokenRateError::Unavailable);
        };
        let agent = observation
            .agent
            .as_ref()
            .ok_or(TokenRateError::Unavailable)?;
        let agent_id = agent["id"].as_str().ok_or(TokenRateError::Unavailable)?;
        let delivery = &observation.delivery;
        let effect_bit = match effect {
            NotificationPostCommit::Analytics { .. }
            | NotificationPostCommit::TokenRateUsage(_) => 1,
            NotificationPostCommit::TokenRateStream(_) => 2,
            NotificationPostCommit::TokenRateAssociation(_) => 4,
            NotificationPostCommit::AnalyticsStream { .. }
            | NotificationPostCommit::Recovered { .. } => {
                return Err(TokenRateError::Unavailable);
            }
        };
        let snapshot = {
            let mut projector = self
                .projector
                .lock()
                .map_err(|_| TokenRateError::Unavailable)?;
            let new_generation = match projector.watermarks.get_mut(&delivery.provider_handle) {
                Some(watermark) if delivery.generation < watermark.generation => return Ok(false),
                Some(watermark) if delivery.generation > watermark.generation => {
                    *watermark = DeliveryWatermark {
                        generation: delivery.generation,
                        cursor: delivery.cursor,
                        effects: effect_bit,
                    };
                    true
                }
                Some(watermark) if delivery.cursor < watermark.cursor => return Ok(false),
                Some(watermark) if delivery.cursor == watermark.cursor => {
                    if watermark.effects & effect_bit != 0 {
                        return Ok(false);
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
                    projector.watermarks.insert(
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
                projector.agents.remove(agent_id);
            }
            let entry = projector.agents.entry(agent_id.to_owned()).or_default();
            entry.provider_handle.clone_from(&delivery.provider_handle);
            entry.root = agent
                .get("rootId")
                .and_then(serde_json::Value::as_str)
                .filter(|root| !root.is_empty())
                .unwrap_or(agent_id)
                .to_owned();
            project_effect(entry, &effect, observation);
            while projector.agents.len() > MAX_PROJECTED_AGENTS {
                let oldest = projector.agents.keys().next().cloned();
                if let Some(oldest) = oldest {
                    projector.agents.remove(&oldest);
                }
            }
            while projector.watermarks.len() > MAX_TOKEN_RATE_WATERMARKS {
                let referenced = projector
                    .agents
                    .values()
                    .map(|agent| agent.provider_handle.as_str())
                    .collect::<BTreeSet<_>>();
                let oldest = projector
                    .watermarks
                    .keys()
                    .find(|provider| !referenced.contains(provider.as_str()))
                    .cloned();
                if let Some(oldest) = oldest {
                    projector.watermarks.remove(&oldest);
                } else {
                    break;
                }
            }
            snapshot_from_projector(&projector)
        };
        self.publish(snapshot.to_value())
            .map_err(|_| TokenRateError::InvalidSnapshot)
    }

    /// Publish one already validated producer snapshot; identical updates are coalesced.
    pub fn publish(&self, value: serde_json::Value) -> Result<bool, TokenRateError> {
        let snapshot = TokenRateSnapshot::from_value(value)?;
        Ok(self.sender.send_if_modified(|current| {
            if *current == snapshot {
                false
            } else if let Some(changes) = &self.changes {
                changes.try_publish(1, false, |_| *current = snapshot)
            } else {
                *current = snapshot;
                true
            }
        }))
    }
}

fn effect_observation(effect: &NotificationPostCommit) -> Option<&NotificationObservation> {
    match effect {
        NotificationPostCommit::Analytics { observation, .. }
        | NotificationPostCommit::AnalyticsStream { observation, .. }
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
            "item/agentMessage/delta" | "item/reasoning/textDelta" => {
                if state.active {
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
            }
            _ => {}
        },
        NotificationPostCommit::Analytics { .. } | NotificationPostCommit::TokenRateUsage(_) => {
            match observation.method.as_str() {
                "turn/started" => {
                    if let Some(turn) = observed_turn(params, agent_turn(observation)) {
                        if turn != state.turn {
                            state.turn = turn;
                            state.active = true;
                            state.rate = 0.0;
                            state.actual = 0.0;
                            state.pending = 0.0;
                            state.estimated = false;
                            state.baseline = state.lifetime;
                            state.samples.clear();
                            state.response_outputs.clear();
                            state.tools.clear();
                            state.generation_elapsed = 0.0;
                            state.response_durations.clear();
                            state.response_duration_pending_tokens = 0.0;
                        }
                        state.generation_started = Some(at);
                        state.timing_valid = true;
                    }
                }
                "thread/tokenUsage/updated" => project_usage(state, params, at),
                "turn/completed"
                    if observed_turn(params, Some(&state.turn)).as_deref() == Some(&state.turn) =>
                {
                    state.active = false;
                    state.estimated = false;
                }
                "turn/completed" => {}
                _ => {}
            }
        }
        NotificationPostCommit::TokenRateAssociation(_) => match observation.method.as_str() {
            "thread/tokenUsage/updated" => associate_response(state, params),
            "item/started" | "item/completed" => project_item_association(state, observation),
            _ => {}
        },
        NotificationPostCommit::AnalyticsStream { .. } => {}
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

fn project_usage(state: &mut ProjectedAgent, params: &serde_json::Value, at: f64) {
    let usage = &params["tokenUsage"];
    let total = usage["total"]["outputTokens"].as_f64();
    let last = usage["last"]["outputTokens"].as_f64();
    let response = params["responseOutputTokens"]
        .as_f64()
        .or_else(|| params["responseUsage"]["outputTokens"].as_f64())
        .or(last);
    if state.baseline.is_none() {
        state.baseline = total.map(|total| (total - last.unwrap_or(0.0)).max(0.0));
    }
    if let Some(turn_output) = params["turnOutputTokens"].as_f64() {
        state.actual = state.actual.max(turn_output);
    } else if let (Some(total), Some(baseline)) = (total, state.baseline) {
        state.actual = state.actual.max((total - baseline).max(0.0));
    } else if let Some(response) = response {
        let identity = params["responseId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(|id| format!("response:{id}"))
            .unwrap_or_else(|| {
                format!(
                    "pending:{}:{response}",
                    usage["last"]["totalTokens"].as_u64().unwrap_or_default()
                )
            });
        state.response_outputs.insert(identity, response);
        while state.response_outputs.len() > 1024 {
            if let Some(oldest) = state.response_outputs.keys().next().cloned() {
                state.response_outputs.remove(&oldest);
            }
        }
        state.actual = state.response_outputs.values().sum();
    }
    if let Some(total) = total {
        state.lifetime = Some(total);
    }
    if let Some(response) = response {
        let queued_duration = state
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

fn associate_response(state: &mut ProjectedAgent, params: &serde_json::Value) {
    let Some(response_id) = params["responseId"].as_str().filter(|id| !id.is_empty()) else {
        return;
    };
    let Some(output_tokens) = params["requestUsage"]["outputTokens"]
        .as_f64()
        .or_else(|| params["tokenUsage"]["last"]["outputTokens"].as_f64())
    else {
        return;
    };
    let pending = state
        .response_outputs
        .iter()
        .filter(|(key, value)| key.starts_with("pending:") && **value == output_tokens)
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    if pending.len() == 1 {
        state.response_outputs.remove(&pending[0]);
        state
            .response_outputs
            .insert(format!("response:{response_id}"), output_tokens);
        state.actual = state.response_outputs.values().sum();
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
        state.turn = turn.to_owned();
        state.active = true;
        state.estimated = false;
        state.rate = 0.0;
        state.actual = 0.0;
        state.pending = 0.0;
        state.baseline = state.lifetime;
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

fn snapshot_from_projector(projector: &RateProjector) -> TokenRateSnapshot {
    let mut snapshot = TokenRateSnapshot::empty();
    for (agent_id, state) in &projector.agents {
        if state.turn.is_empty() {
            continue;
        }
        let value = TokenRateValue {
            turn_id: state.turn.clone(),
            active: state.active,
            estimated: state.estimated,
            rate: FiniteRate(state.rate.clamp(0.0, MAX_TOKEN_RATE)),
            output_tokens: FiniteRate(
                (state.actual + state.pending + state.response_duration_pending_tokens).max(0.0),
            ),
        };
        snapshot.rates.insert(agent_id.clone(), value.clone());
        snapshot
            .teams
            .entry(state.root.clone())
            .or_default()
            .insert(agent_id.clone(), value);
    }
    snapshot
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

fn valid_rate(value: &TokenRateValue) -> bool {
    valid_id(&value.turn_id)
        && value.rate.0.is_finite()
        && value.rate.0 >= 0.0
        && value.output_tokens.0.is_finite()
        && value.output_tokens.0 >= 0.0
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_PROJECTED_AGENTS, MAX_TOKEN_RATE_WATERMARKS, ProjectedAgent,
        TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS, TOKEN_RATE_MAX_ENTRIES,
        TOKEN_RATE_MAX_TOTAL_ENTRIES, TokenRateHub,
    };
    use crate::notification_dispatch::{
        NotificationObservation, NotificationPostCommit, ProviderDeliveryIdentity, SharedJson,
    };
    use serde_json::json;

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

    #[test]
    fn token_rate_projection_constants_match_python_owner() {
        const PYTHON_OWNER: &str = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../apps/server/src/codex_token_rate.py"
        ));
        assert!(PYTHON_OWNER.contains("MAX_RATE = 1000.0"));
        assert!(PYTHON_OWNER.contains("MIN_DURATION = .25"));
        assert!(PYTHON_OWNER.contains("len(text) / 4.0"));
        assert!(PYTHON_OWNER.contains("now - self.text_samples[0][0] > 4"));
        assert!(PYTHON_OWNER.contains("MAX_PENDING_RESPONSE_DURATION_ENTRIES = 1024"));
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

        super::project_usage(&mut state, &json!({"responseOutputTokens":10}), 8.0);

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
        let value = json!({"rates":{"agent-a":{"turnId":"turn-a","active":true,
            "estimated":false,"rate":8.5,"outputTokens":17}},"teams":{}});
        assert!(hub.publish(value.clone()).expect("valid snapshot"));
        assert!(!hub.publish(value.clone()).expect("duplicate snapshot"));
        assert!(subscriber.changed().await.is_ok());
        assert_eq!(
            subscriber.borrow_and_update().rates["agent-a"]
                .output_tokens
                .0,
            17.0
        );
    }

    #[test]
    fn malformed_or_oversized_snapshots_are_refused() {
        let hub = TokenRateHub::new();
        assert!(
            hub.publish(json!({"rates":{"agent-a":{"turnId":"turn-a","active":true,
            "estimated":false,"rate":-1,"outputTokens":17}},"teams":{}}))
                .is_err()
        );
        assert!(
            hub.publish(json!({"rates":{},"teams":{},"extra":true}))
                .is_err()
        );
    }

    #[test]
    fn delivery_watermark_survives_projection_eviction_and_acknowledgement_loss() {
        let hub = TokenRateHub::new();
        let original = usage_effect("agent-0000", "provider-a", 1, 1, "turn-a");
        assert!(
            hub.publish_observation(original.clone())
                .expect("first publish")
        );
        let after_publish = hub.snapshot();
        assert!(
            !hub.publish_observation(original.clone())
                .expect("retry after lost acknowledgement")
        );
        assert!(
            !hub.publish_observation(usage_effect(
                "agent-other",
                "provider-a",
                1,
                1,
                "turn-other"
            ))
            .expect("provider identity deduplicates across replay")
        );
        assert_eq!(hub.snapshot(), after_publish);

        {
            let mut projector = hub.projector.lock().expect("token-rate projector");
            for index in 1..MAX_PROJECTED_AGENTS {
                projector
                    .agents
                    .insert(format!("agent-{index:04}"), ProjectedAgent::default());
            }
        }
        assert!(
            hub.publish_observation(usage_effect("agent-zzzz", "provider-b", 1, 1, "turn-b"))
                .expect("fill bounded projection")
        );
        assert!(!hub.snapshot().rates.contains_key("agent-0000"));
        assert!(
            !hub.publish_observation(original)
                .expect("replay after projection eviction")
        );
        assert!(!hub.snapshot().rates.contains_key("agent-0000"));
    }

    #[test]
    fn watermark_cap_preserves_provider_of_a_live_projection() {
        let hub = TokenRateHub::new();
        assert!(
            hub.publish_observation(usage_effect("z-agent", "a-provider", 1, 1, "old-turn"))
                .expect("first provider delivery")
        );
        assert!(
            hub.publish_observation(usage_effect(
                "z-agent",
                "a-provider",
                1,
                2,
                "current-turn"
            ))
            .expect("advance live projection")
        );

        {
            let mut projector = hub.projector.lock().expect("token-rate projector");
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
        }

        assert!(
            hub.publish_observation(usage_effect("a-agent", "z-provider", 1, 1, "other-turn"))
                .expect("cross the watermark cap")
        );
        {
            let projector = hub.projector.lock().expect("token-rate projector");
            assert_eq!(projector.watermarks.len(), MAX_TOKEN_RATE_WATERMARKS);
            assert!(projector.watermarks.contains_key("a-provider"));
            assert!(!projector.watermarks.contains_key("b-provider-0000"));
        }

        let current = hub.snapshot();
        assert!(
            !hub.publish_observation(usage_effect("z-agent", "a-provider", 1, 1, "old-turn"))
                .expect("old committed delivery remains deduplicated")
        );
        assert_eq!(hub.snapshot(), current);
        assert_eq!(hub.snapshot().rates["z-agent"].turn_id, "current-turn");
    }

    #[test]
    fn an_older_generation_cannot_replace_a_newer_projection() {
        let hub = TokenRateHub::new();
        assert!(
            hub.publish_observation(usage_effect("agent", "provider-a", 9, 1, "new-turn"))
                .expect("new generation publishes")
        );
        let current = hub.snapshot();
        assert!(
            !hub.publish_observation(usage_effect("agent", "provider-a", 8, 99, "old-turn"))
                .expect("old generation is stale")
        );
        assert_eq!(hub.snapshot(), current);
        assert_eq!(hub.snapshot().rates["agent"].turn_id, "new-turn");
    }

    #[test]
    fn opaque_unicode_identifiers_use_the_generated_codepoint_bound() {
        let identifier = "💠".repeat(TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS);
        let value =
            json!({"turnId":identifier,"active":true,"estimated":false,"rate":0,"outputTokens":0});
        let mut rates = serde_json::Map::new();
        rates.insert(identifier, value);
        let mut snapshot = json!({"rates":{},"teams":{}});
        snapshot["rates"] = serde_json::Value::Object(rates);
        assert!(TokenRateHub::new().publish(snapshot).is_ok());

        let too_long = "💠".repeat(TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS + 1);
        let value =
            json!({"turnId":too_long,"active":true,"estimated":false,"rate":0,"outputTokens":0});
        assert!(
            TokenRateHub::new()
                .publish(json!({"rates":{"agent":value},"teams":{}}))
                .is_err()
        );

        let value =
            json!({"turnId":"turn","active":true,"estimated":false,"rate":0,"outputTokens":0});
        let mut rates = serde_json::Map::new();
        rates.insert(too_long, value);
        let mut snapshot = json!({"rates":{},"teams":{}});
        snapshot["rates"] = serde_json::Value::Object(rates);
        assert!(TokenRateHub::new().publish(snapshot).is_err());
    }

    #[test]
    fn maximum_agent_bound_is_enforced_for_rates_and_team_members() {
        let value = json!({"turnId":"t","active":true,"estimated":false,"rate":0,"outputTokens":0});
        let rates = (0..=TOKEN_RATE_MAX_ENTRIES)
            .map(|index| (format!("a{index}"), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        assert!(
            TokenRateHub::new()
                .publish(json!({"rates":rates,"teams":{}}))
                .is_err()
        );
        let teams = (0..=TOKEN_RATE_MAX_ENTRIES)
            .map(|index| (format!("t{index}"), serde_json::json!({})))
            .collect::<serde_json::Map<_, _>>();
        assert!(
            TokenRateHub::new()
                .publish(json!({"rates":{},"teams":teams}))
                .is_err()
        );
        let team = (0..=TOKEN_RATE_MAX_ENTRIES)
            .map(|index| (format!("a{index}"), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        assert!(
            TokenRateHub::new()
                .publish(json!({"rates":{},"teams":{"root":team}}))
                .is_err()
        );
    }

    #[test]
    fn aggregate_rate_values_are_bounded_across_workspace_and_team_views() {
        let value = json!({"turnId":"t","active":true,"estimated":false,"rate":0,"outputTokens":0});
        let rates = (0..TOKEN_RATE_MAX_ENTRIES)
            .map(|index| (format!("a{index}"), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        let team = (0..TOKEN_RATE_MAX_ENTRIES)
            .map(|index| (format!("a{index}"), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        assert_eq!(TOKEN_RATE_MAX_TOTAL_ENTRIES, TOKEN_RATE_MAX_ENTRIES * 2);
        assert!(
            TokenRateHub::new()
                .publish(json!({"rates":rates,"teams":{"root":team}}))
                .is_ok()
        );

        let overfull_team = (0..TOKEN_RATE_MAX_ENTRIES)
            .map(|index| (format!("a{index}"), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        let overfull_rates = (0..TOKEN_RATE_MAX_ENTRIES)
            .map(|index| (format!("a{index}"), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        let extra = serde_json::Map::from_iter([("extra".to_owned(), value)]);
        assert!(
            TokenRateHub::new()
                .publish(
                    json!({"rates":overfull_rates,"teams":{"root":overfull_team,"other":extra}})
                )
                .is_err()
        );
    }
}
