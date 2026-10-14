//! Bounded, latest-value token-rate publication for resource streams.

use std::collections::BTreeMap;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use crate::change_notifications::ChangeNotificationHub;
use crate::notification_dispatch::NotificationPostCommit;

const MAX_OBSERVATION_RECEIPTS: usize = 256;
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
    observations: watch::Sender<Option<NotificationPostCommit>>,
    observation_ids: Arc<Mutex<VecDeque<(String, u64, u64)>>>,
    changes: Option<Arc<ChangeNotificationHub>>,
}

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
            observations: watch::channel(None).0,
            observation_ids: Arc::default(),
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

    /// Retain the latest committed provider observation for the rate projector.
    /// The projector is not implemented in Rust yet; the durable worker receipt
    /// is still acknowledged only after this bounded hub accepts the observation.
    pub fn publish_observation(
        &self,
        effect: NotificationPostCommit,
    ) -> Result<bool, TokenRateError> {
        let observation = match &effect {
            NotificationPostCommit::Analytics { observation, .. }
            | NotificationPostCommit::AnalyticsStream { observation, .. }
            | NotificationPostCommit::TokenRateStream(observation)
            | NotificationPostCommit::TokenRateAssociation(observation) => observation,
        };
        let identity = (
            observation.delivery.provider_handle.clone(),
            observation.delivery.generation,
            observation.delivery.cursor,
        );
        let mut ids = self
            .observation_ids
            .lock()
            .map_err(|_| TokenRateError::Unavailable)?;
        if ids.contains(&identity) {
            return Ok(false);
        }
        if ids.len() == MAX_OBSERVATION_RECEIPTS {
            ids.pop_front();
        }
        ids.push_back(identity);
        let changed = self.observations.send_if_modified(|current| {
            if current.as_ref() == Some(&effect) {
                false
            } else {
                *current = Some(effect);
                true
            }
        });
        if changed && let Some(changes) = &self.changes {
            changes.try_publish(1, false, |_| {});
        }
        Ok(changed)
    }

    pub fn subscribe_observations(&self) -> watch::Receiver<Option<NotificationPostCommit>> {
        self.observations.subscribe()
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
        TOKEN_RATE_IDENTIFIER_MAX_CODEPOINTS, TOKEN_RATE_MAX_ENTRIES, TOKEN_RATE_MAX_TOTAL_ENTRIES,
        TokenRateHub,
    };
    use serde_json::json;

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
