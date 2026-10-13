//! Bounded, latest-value token-rate publication for resource streams.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;

const MAX_AGENTS: usize = 1024;
const MAX_ID_BYTES: usize = 200;

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
}

/// A bounded watch channel; slow subscribers always receive the newest snapshot.
#[derive(Clone)]
pub struct TokenRateHub {
    sender: watch::Sender<TokenRateSnapshot>,
}

impl TokenRateSnapshot {
    pub fn from_value(value: serde_json::Value) -> Result<Self, TokenRateError> {
        let snapshot: Self =
            serde_json::from_value(value).map_err(|_| TokenRateError::InvalidSnapshot)?;
        if snapshot.rates.len() > MAX_AGENTS
            || snapshot.teams.len() > MAX_AGENTS
            || snapshot
                .rates
                .iter()
                .any(|(id, rate)| !valid_id(id) || !valid_rate(rate))
            || snapshot.teams.iter().any(|(root, team)| {
                !valid_id(root)
                    || team.len() > MAX_AGENTS
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

    /// Publish one already validated producer snapshot; identical updates are coalesced.
    pub fn publish(&self, value: serde_json::Value) -> Result<bool, TokenRateError> {
        let snapshot = TokenRateSnapshot::from_value(value)?;
        Ok(self.sender.send_if_modified(|current| {
            if *current == snapshot {
                false
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
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte))
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
    use super::TokenRateHub;
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
}
