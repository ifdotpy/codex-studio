//! Costable usage groups derived from Python-written analytics rows.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;
use thiserror::Error;

const MAX_RECORDS: usize = 1_000_000;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum CostUsageError {
    #[error("cost usage contains an invalid stored row")]
    InvalidStoredRow,
    #[error("cost usage exceeds the supported record bound")]
    RecordLimit,
}

/// One model/account/token tuple and its occurrence count.
#[derive(Clone, Debug, PartialEq)]
pub struct CostGroup {
    pub model: Option<String>,
    pub account_key: String,
    pub input_tokens: Option<f64>,
    pub cached_tokens: Option<f64>,
    pub write_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
    pub input_uncached: bool,
    pub count: usize,
}

#[derive(Clone, Debug)]
struct UsageRecord {
    sequence: i64,
    at: f64,
    agent: Option<String>,
    thread: Option<String>,
    turn: Option<String>,
    record_agent: Option<String>,
    record_thread: Option<String>,
    record_turn: Option<String>,
    model: Option<String>,
    account_key: String,
    input_tokens: Option<f64>,
    cached_tokens: Option<f64>,
    write_tokens: Option<f64>,
    output_tokens: Option<f64>,
    input_uncached: bool,
    has_response: bool,
}

/// Validated usage records captured in one read-only analytics snapshot.
#[derive(Clone, Debug, Default)]
pub struct CostUsageSnapshot {
    groups: Vec<CostGroup>,
}

impl CostUsageSnapshot {
    /// Construct from `(sequence, agent, thread, turn, at, record)` rows.
    pub fn from_records(
        rows: impl IntoIterator<
            Item = (
                i64,
                Option<String>,
                Option<String>,
                Option<String>,
                f64,
                Value,
            ),
        >,
    ) -> Result<Self, CostUsageError> {
        let mut records = Vec::new();
        for (sequence, agent, thread, turn, at, value) in rows {
            if records.len() == MAX_RECORDS {
                return Err(CostUsageError::RecordLimit);
            }
            let record = value.as_object().ok_or(CostUsageError::InvalidStoredRow)?;
            let usage = record
                .get("delta")
                .filter(|delta| has_token_pair(delta))
                .or_else(|| record.get("last").filter(|last| last.is_object()));
            let get_number = |name: &str| {
                usage
                    .and_then(|tokens| tokens.get(name))
                    .and_then(Value::as_f64)
            };
            records.push(UsageRecord {
                sequence,
                at,
                agent,
                thread,
                turn,
                record_agent: record
                    .get("agentId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                record_thread: record
                    .get("threadId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                record_turn: record
                    .get("turnId")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                model: record
                    .get("model")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
                account_key: record
                    .get("accountKey")
                    .and_then(Value::as_str)
                    .unwrap_or("default")
                    .to_owned(),
                input_tokens: get_number("inputTokens"),
                cached_tokens: get_number("cachedInputTokens"),
                write_tokens: get_number("cacheWriteInputTokens"),
                output_tokens: get_number("outputTokens"),
                input_uncached: record.get("inputTokensAreUncached").is_some_and(|value| {
                    value.as_bool() == Some(true) || value.as_i64() == Some(1)
                }),
                has_response: record.get("responseId").is_some_and(is_truthy),
            });
        }
        Ok(Self {
            groups: group_records(records),
        })
    }

    #[must_use]
    pub fn groups(&self) -> &[CostGroup] {
        &self.groups
    }
}

fn has_token_pair(value: &Value) -> bool {
    ["inputTokens", "outputTokens"].into_iter().all(|key| {
        value
            .get(key)
            .is_some_and(|token| token.is_number() || token.as_bool().is_some())
    })
}

fn is_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_some_and(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct TurnKey(Option<String>, Option<String>, Option<String>);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ModelKey(Option<String>, Option<String>);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey(
    Option<String>,
    String,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    Option<u64>,
    bool,
);

fn amount_key(value: Option<f64>) -> Option<u64> {
    value.map(|number| if number == 0.0 { 0.0_f64 } else { number }.to_bits())
}

fn group_records(records: Vec<UsageRecord>) -> Vec<CostGroup> {
    let mut responded = BTreeSet::new();
    let mut turn_models: BTreeMap<ModelKey, (f64, i64, String)> = BTreeMap::new();
    let mut thread_models: BTreeMap<ModelKey, (f64, i64, String)> = BTreeMap::new();
    for row in &records {
        if row.has_response {
            responded.insert(TurnKey(
                row.agent.clone(),
                row.thread.clone(),
                row.turn.clone(),
            ));
        }
    }
    for row in &records {
        if let Some(model) = &row.model
            && (row.has_response
                || !responded.contains(&TurnKey(
                    row.agent.clone(),
                    row.thread.clone(),
                    row.turn.clone(),
                )))
        {
            update_model(
                &mut turn_models,
                ModelKey(row.record_agent.clone(), row.record_turn.clone()),
                row,
                model,
            );
            update_model(
                &mut thread_models,
                ModelKey(row.record_agent.clone(), row.record_thread.clone()),
                row,
                model,
            );
        }
    }
    let mut groups: BTreeMap<GroupKey, CostGroup> = BTreeMap::new();
    for row in records {
        if !row.has_response
            && responded.contains(&TurnKey(
                row.agent.clone(),
                row.thread.clone(),
                row.turn.clone(),
            ))
        {
            continue;
        }
        let model = row.model.clone().or_else(|| {
            turn_models
                .get(&ModelKey(row.record_agent.clone(), row.record_turn.clone()))
                .map(|(_, _, model)| model.clone())
                .or_else(|| {
                    thread_models
                        .get(&ModelKey(
                            row.record_agent.clone(),
                            row.record_thread.clone(),
                        ))
                        .map(|(_, _, model)| model.clone())
                })
        });
        let key = GroupKey(
            model.clone(),
            row.account_key.clone(),
            amount_key(row.input_tokens),
            amount_key(row.cached_tokens),
            amount_key(row.write_tokens),
            amount_key(row.output_tokens),
            row.input_uncached,
        );
        groups
            .entry(key)
            .and_modify(|group| group.count += 1)
            .or_insert(CostGroup {
                model,
                account_key: row.account_key,
                input_tokens: row.input_tokens,
                cached_tokens: row.cached_tokens,
                write_tokens: row.write_tokens,
                output_tokens: row.output_tokens,
                input_uncached: row.input_uncached,
                count: 1,
            });
    }
    groups.into_values().collect()
}

fn update_model(
    models: &mut BTreeMap<ModelKey, (f64, i64, String)>,
    key: ModelKey,
    row: &UsageRecord,
    model: &str,
) {
    let candidate = (row.at, row.sequence, model.to_owned());
    if models
        .get(&key)
        .is_none_or(|current| (candidate.0, candidate.1) > (current.0, current.1))
    {
        models.insert(key, candidate);
    }
}

#[cfg(test)]
mod tests {
    use super::{CostUsageError, CostUsageSnapshot};
    use serde_json::json;

    #[test]
    fn current_response_replaces_provisional_usage_and_inherits_latest_model() {
        let rows = vec![
            (
                1,
                Some("a".into()),
                Some("t".into()),
                Some("turn".into()),
                1.0,
                json!({"agentId":"a","threadId":"t","turnId":"turn","model":"gpt-old","delta":{"inputTokens":5,"outputTokens":1}}),
            ),
            (
                2,
                Some("a".into()),
                Some("t".into()),
                Some("turn".into()),
                2.0,
                json!({"agentId":"a","threadId":"t","turnId":"turn","model":"gpt-new","responseId":"r","last":{"inputTokens":8,"outputTokens":2}}),
            ),
            (
                3,
                Some("a".into()),
                Some("t".into()),
                Some("next".into()),
                3.0,
                json!({"agentId":"a","threadId":"t","turnId":"next","last":{"inputTokens":4,"outputTokens":1}}),
            ),
        ];
        let snapshot = CostUsageSnapshot::from_records(rows).expect("valid rows");
        assert_eq!(snapshot.groups().len(), 2);
        assert!(
            snapshot
                .groups()
                .iter()
                .any(|group| group.model.as_deref() == Some("gpt-new")
                    && group.input_tokens == Some(8.0)
                    && group.count == 1)
        );
        assert!(
            snapshot
                .groups()
                .iter()
                .any(|group| group.model.as_deref() == Some("gpt-new")
                    && group.input_tokens == Some(4.0)
                    && group.count == 1)
        );
    }

    #[test]
    fn malformed_record_is_a_located_storage_error() {
        assert!(matches!(
            CostUsageSnapshot::from_records([(1, None, None, None, 0.0, json!(null))]),
            Err(CostUsageError::InvalidStoredRow)
        ));
    }
}
