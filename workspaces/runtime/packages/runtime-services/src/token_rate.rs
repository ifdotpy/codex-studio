//! Bounded, latest-value token-rate publication for resource streams.

use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

use crate::change_notifications::ChangeNotificationHub;

use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::sync::watch;

pub const MAX_SNAPSHOT_BYTES: usize = 262_144;

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

#[derive(Clone, Debug)]
struct FiniteRate(serde_json::Number);

// Python compares integers and floats numerically, including beyond f64's exact integer range.
impl PartialEq for FiniteRate {
    fn eq(&self, other: &Self) -> bool {
        match (self.0.as_u64(), other.0.as_u64()) {
            (Some(left), Some(right)) => left == right,
            (Some(integer), None) | (None, Some(integer)) => {
                let float = if self.0.as_u64().is_none() {
                    self.0.as_f64()
                } else {
                    other.0.as_f64()
                };
                float.is_some_and(|value| {
                    value.fract() == 0.0
                        && value < 18446744073709551616.0
                        && value as u64 == integer
                })
            }
            _ => self.0.as_f64() == other.0.as_f64(),
        }
    }
}

impl Default for FiniteRate {
    fn default() -> Self {
        Self(0.into())
    }
}

impl<'de> Deserialize<'de> for FiniteRate {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let value = serde_json::Number::deserialize(deserializer)?;
        if value
            .as_f64()
            .is_some_and(|value| value.is_finite() && value >= 0.0)
        {
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
        self.0.serialize(serializer)
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
    changes: Option<Arc<ChangeNotificationHub>>,
}

impl TokenRateSnapshot {
    pub fn from_value(value: serde_json::Value) -> Result<Self, TokenRateError> {
        if value.to_string().len() > MAX_SNAPSHOT_BYTES || !valid_snapshot_schema(&value)? {
            return Err(TokenRateError::InvalidSnapshot);
        }
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
            changes: None,
        }
    }

    /// Bind token-only publication to the shared cursor without invalidating other resources.
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
        && value.len() <= MAX_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._:/-".contains(&byte))
}

fn valid_rate(value: &TokenRateValue) -> bool {
    valid_id(&value.turn_id)
}

fn valid_snapshot_schema(value: &serde_json::Value) -> Result<bool, TokenRateError> {
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, String>> = OnceLock::new();
    let validator = VALIDATOR
        .get_or_init(|| {
            let document = crate::sync_views::openapi_document();
            let mut schema = document["components"]["schemas"]["ResourceTokenRatesEvent"].clone();
            schema["properties"]
                .as_object_mut()
                .ok_or("missing token-rate properties")?
                .retain(|name, _| matches!(name.as_str(), "rates" | "teams"));
            schema["required"] = serde_json::json!(["rates", "teams"]);
            schema["components"] = document["components"].clone();
            jsonschema::options()
                .build(&schema)
                .map_err(|error| error.to_string())
        })
        .as_ref()
        .map_err(|_| TokenRateError::InvalidSnapshot)?;
    Ok(validator.is_valid(value))
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
            serde_json::Number::from(17)
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
    fn maximum_agent_bound_is_enforced_for_rates_and_team_members() {
        let value = json!({"turnId":"t","active":true,"estimated":false,"rate":0,"outputTokens":0});
        let rates = (0..=1024)
            .map(|index| (format!("a{index}"), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        assert!(
            TokenRateHub::new()
                .publish(json!({"rates":rates,"teams":{}}))
                .is_err()
        );
        let team = (0..=1024)
            .map(|index| (format!("a{index}"), value.clone()))
            .collect::<serde_json::Map<_, _>>();
        assert!(
            TokenRateHub::new()
                .publish(json!({"rates":{},"teams":{"root":team}}))
                .is_err()
        );
    }
}
