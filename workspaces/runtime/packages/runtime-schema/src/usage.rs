//! Provider-neutral accounting observations. This module alone interprets usage wire keys.
pub mod persisted;
pub use persisted::parse_analytics_usage_row;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Disjoint billable token categories; reasoning is already included in output.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageTokens {
    /// Base-rate input, excluding cache reads and writes; tokens.
    pub uncached_input: u64,
    /// Input served from the prompt cache; tokens.
    pub cache_read_input: u64,
    /// Input written to the prompt cache; tokens.
    pub cache_write_input: u64,
    /// All generated output, including reasoning; tokens.
    pub output: u64,
    /// Known reasoning subset of output; tokens, absent when unknown.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning_output: Option<u64>,
}
impl UsageTokens {
    /// Total billable tokens, counting reasoning only once.
    pub fn total(&self) -> Option<u64> {
        self.uncached_input
            .checked_add(self.cache_read_input)?
            .checked_add(self.cache_write_input)?
            .checked_add(self.output)
    }
    /// Whether any billable counter decreased; a new counter epoch is required.
    pub fn decreased_from(&self, previous: &Self) -> bool {
        self.uncached_input < previous.uncached_input
            || self.cache_read_input < previous.cache_read_input
            || self.cache_write_input < previous.cache_write_input
            || self.output < previous.output
    }
}
/// Provenance of a final response; legacy observations may be provisional.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum UsageEvidence {
    Live,
    Recovered,
    Legacy,
}
/// Counter ownership: a native Codex thread or one Claude SDK query.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum UsageScope {
    CodexThread,
    ClaudeQuery,
}
/// Why final response accounting is unavailable.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub enum UsageGapReason {
    StreamInterrupted,
    QueryClosed,
}
/// One model's cumulative contribution to a Claude query.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct CumulativeModelUsage {
    /// Provider model identifier; dimensionless.
    pub model: String,
    /// Disjoint cumulative token counts; tokens.
    pub tokens: UsageTokens,
    /// Provider's informational cost estimate; USD.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_cost_usd: Option<f64>,
}
/// Accounting evidence, preserving response identity and cumulative counter scope.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(
    tag = "kind",
    rename_all = "camelCase",
    rename_all_fields = "camelCase",
    deny_unknown_fields
)]
pub enum UsageObservation {
    /// Final usage for one response, keyed by agent and response ID.
    Response {
        /// Native response identifier; dimensionless.
        response_id: String,
        /// Native turn identifier; dimensionless.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Provider model identifier; dimensionless.
        model: String,
        /// Final disjoint token counts; tokens.
        tokens: UsageTokens,
        /// How the final usage was collected.
        evidence: UsageEvidence,
        /// Parent tool identifier; absent means main loop.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        parent_tool_use_id: Option<String>,
        /// Model context capacity; tokens.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_context_window: Option<u64>,
    },
    /// Running provider counter; never added to response records.
    Cumulative {
        /// Counter's reset and accumulation domain.
        scope: UsageScope,
        /// Native turn identifier; dimensionless.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Running disjoint category counts; tokens.
        total: UsageTokens,
        /// Most recent response without an identity; tokens.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        last: Option<UsageTokens>,
        /// Model context capacity; tokens.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model_context_window: Option<u64>,
        /// Per-model running counters and informational provider costs.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        models: Option<Vec<CumulativeModelUsage>>,
    },
    /// A response with no final usage; unknown rather than zero tokens.
    Gap {
        /// Native response identifier; dimensionless.
        response_id: String,
        /// Native turn identifier; dimensionless.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        turn_id: Option<String>,
        /// Provider model identifier, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        /// Final-usage loss reason.
        reason: UsageGapReason,
    },
}
/// Provider at the usage conversion boundary; independent of provider services.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UsageProvider {
    Codex,
    Claude,
}
/// Invalid accounting evidence; callers must preserve this uncertainty.
#[derive(Debug)]
pub struct UsageParseError(pub String);
impl std::fmt::Display for UsageParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}
impl std::error::Error for UsageParseError {}
impl From<serde_json::Error> for UsageParseError {
    fn from(value: serde_json::Error) -> Self {
        Self(value.to_string())
    }
}

/// Codex CLI 0.164.0 generated notification schema; native compatibility owner.
pub const CODEX_USAGE_SCHEMA: &str =
    include_str!("schemas/codex/ThreadTokenUsageUpdatedNotification.json");
/// Mirror of Codex's generated TokenUsageBreakdown (signed int64 on the wire).
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodexTokenUsageBreakdown {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    #[serde(default)]
    pub cache_write_input_tokens: i64,
    pub output_tokens: i64,
    pub reasoning_output_tokens: i64,
    pub total_tokens: i64,
}
impl CodexTokenUsageBreakdown {
    /// Convert inclusive Codex input exactly as pricing::price_usage does.
    pub fn tokens(&self) -> Result<UsageTokens, UsageParseError> {
        let positive =
            |n| u64::try_from(n).map_err(|_| UsageParseError("negative Codex token count".into()));
        let input = positive(self.input_tokens)?;
        let read = positive(self.cached_input_tokens)?.min(input);
        let write = positive(self.cache_write_input_tokens)?.min(input - read);
        positive(self.total_tokens)?;
        Ok(UsageTokens {
            uncached_input: input - read - write,
            cache_read_input: read,
            cache_write_input: write,
            output: positive(self.output_tokens)?,
            reasoning_output: Some(positive(self.reasoning_output_tokens)?),
        })
    }
}
/// Mirror of Codex's generated ThreadTokenUsage.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodexThreadTokenUsage {
    pub total: CodexTokenUsageBreakdown,
    pub last: CodexTokenUsageBreakdown,
    pub model_context_window: Option<i64>,
}
/// Mirror of Codex's generated ThreadTokenUsageUpdatedNotification.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "camelCase")]
pub struct CodexThreadTokenUsageUpdatedNotification {
    pub thread_id: String,
    pub turn_id: String,
    pub token_usage: CodexThreadTokenUsage,
}

fn optional_text(value: &Value, key: &str) -> Result<Option<String>, UsageParseError> {
    match value.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(s.clone())),
        _ => Err(UsageParseError(format!("invalid {key}"))),
    }
}
fn count(value: &Value, key: &str, optional: bool) -> Result<u64, UsageParseError> {
    match value.get(key) {
        None | Some(Value::Null) if optional => Ok(0),
        Some(v) => v
            .as_u64()
            .ok_or_else(|| UsageParseError(format!("invalid {key}"))),
        None => Err(UsageParseError(format!("missing {key}"))),
    }
}
fn legacy_tokens(value: &Value, provider: UsageProvider) -> Result<UsageTokens, UsageParseError> {
    if !value.is_object() {
        return Err(UsageParseError("missing token breakdown".into()));
    }
    let input = count(value, "inputTokens", false)?;
    let read = count(value, "cachedInputTokens", true)?;
    let write = count(value, "cacheWriteInputTokens", true)?;
    let (input, read, write) = if provider == UsageProvider::Codex {
        let read = read.min(input);
        let write = write.min(input - read);
        (input - read - write, read, write)
    } else {
        (input, read, write)
    };
    Ok(UsageTokens {
        uncached_input: input,
        cache_read_input: read,
        cache_write_input: write,
        output: count(value, "outputTokens", false)?,
        reasoning_output: value
            .get("reasoningOutputTokens")
            .filter(|v| !v.is_null())
            .map(|_| count(value, "reasoningOutputTokens", false))
            .transpose()?,
    })
}
/// Parse every supported usage notification once. Legacy branches support bridges
/// already running when the typed contract is delivered and historical imports.
pub fn parse_token_usage(
    provider: UsageProvider,
    params: &Value,
) -> Result<UsageObservation, UsageParseError> {
    if let Some(usage) = params.get("studioUsage") {
        return Ok(serde_json::from_value(usage.clone())?);
    }
    let source = params.get("usageSource").and_then(Value::as_str);
    let turn_id = optional_text(params, "turnId")?;
    let usage = &params["tokenUsage"];
    let model_context_window = usage
        .get("modelContextWindow")
        .filter(|v| !v.is_null())
        .map(|v| {
            v.as_u64()
                .ok_or_else(|| UsageParseError("invalid modelContextWindow".into()))
        })
        .transpose()?;
    let recovered = provider == UsageProvider::Codex
        && (source == Some("responseRecord")
            || params
                .get("rawTokenUsageRecord")
                .is_some_and(Value::is_object));
    if source == Some("claudeResponse") && provider == UsageProvider::Claude || recovered {
        let response_id = optional_text(params, "responseId")?
            .filter(|s| !s.is_empty())
            .ok_or_else(|| UsageParseError("missing responseId".into()))?;
        let model = optional_text(params, "model")?
            .or_else(|| {
                params
                    .pointer("/rawTokenUsageRecord/model")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
            })
            .unwrap_or_default();
        return Ok(UsageObservation::Response {
            response_id,
            turn_id,
            model,
            tokens: legacy_tokens(
                params
                    .get("requestUsage")
                    .filter(|v| v.is_object())
                    .unwrap_or(&usage["last"]),
                provider,
            )?,
            evidence: if recovered {
                UsageEvidence::Recovered
            } else {
                UsageEvidence::Legacy
            },
            parent_tool_use_id: optional_text(params, "parentToolUseId")?,
            model_context_window,
        });
    }
    if source.is_some() && source != Some("tokenCount") {
        return Err(UsageParseError("unsupported usageSource".into()));
    }
    // Synthetic imports and old Claude bridges may omit native-required fields.
    // Only native Codex notifications deserialize the exact vendored SDK mirror.
    if provider == UsageProvider::Codex && source.is_none() {
        let native: CodexThreadTokenUsageUpdatedNotification =
            serde_json::from_value(params.clone())?;
        return Ok(UsageObservation::Cumulative {
            scope: UsageScope::CodexThread,
            turn_id: Some(native.turn_id),
            total: native.token_usage.total.tokens()?,
            last: Some(native.token_usage.last.tokens()?),
            model_context_window,
            models: None,
        });
    }
    Ok(UsageObservation::Cumulative {
        scope: if provider == UsageProvider::Codex {
            UsageScope::CodexThread
        } else {
            UsageScope::ClaudeQuery
        },
        turn_id,
        total: legacy_tokens(&usage["total"], provider)?,
        last: usage
            .get("last")
            .filter(|v| v.is_object() && !v.as_object().is_some_and(|m| m.is_empty()))
            .map(|v| legacy_tokens(v, provider))
            .transpose()?,
        model_context_window,
        models: None,
    })
}

/// Transitional totals from pre-contract Claude bridges. Remove after those
/// bridges and their journal replay window have retired; these are not categories.
#[derive(Clone, Debug)]
pub struct LegacyUsageTotals {
    pub response_id: Option<String>,
    pub scope: UsageScope,
    pub total_tokens: u64,
    pub last_total_tokens: Option<u64>,
    pub turn_id: Option<String>,
    pub model: Option<String>,
    pub model_context_window: Option<u64>,
}
/// Pre-contract totals retain their previous budget treatment for old-ledger and
/// journal migration only; canonical claudeQuery observations never charge budget.
/// Remove this exception after pre-contract bridges and their replay window retire.
/// Read only pre-contract Claude total-only usage. A present studioUsage is
/// authoritative: malformed typed observations never fall back to legacy input.
pub fn parse_legacy_totals(
    provider: UsageProvider,
    params: &Value,
) -> Result<LegacyUsageTotals, UsageParseError> {
    let source = params.get("usageSource").and_then(Value::as_str);
    if provider != UsageProvider::Claude
        || params.get("studioUsage").is_some()
        || source.is_some_and(|s| s != "claudeResponse")
    {
        return Err(UsageParseError("not legacy Claude usage".into()));
    }
    let usage = &params["tokenUsage"];
    let response = source == Some("claudeResponse");
    let total_tokens = count(
        if response {
            &params["requestUsage"]
        } else {
            &usage["total"]
        },
        "totalTokens",
        false,
    )?;
    let last_total_tokens = if response {
        Some(total_tokens)
    } else {
        usage
            .get("last")
            .and_then(|v| v.get("totalTokens"))
            .filter(|v| !v.is_null())
            .map(|_| count(&usage["last"], "totalTokens", false))
            .transpose()?
    };
    let model_context_window = usage
        .get("modelContextWindow")
        .filter(|v| !v.is_null())
        .map(|_| count(usage, "modelContextWindow", false))
        .transpose()?;
    Ok(LegacyUsageTotals {
        response_id: if response {
            Some(
                optional_text(params, "responseId")?
                    .filter(|s| !s.is_empty())
                    .ok_or_else(|| UsageParseError("missing legacy responseId".into()))?,
            )
        } else {
            None
        },
        scope: UsageScope::ClaudeQuery,
        total_tokens,
        last_total_tokens,
        turn_id: optional_text(params, "turnId")?,
        model: optional_text(params, "model")?,
        model_context_window,
    })
}

/// Exact pre-contract notice receipt preimage for native live/imported counters.
/// Hash with the existing Python-compatible JSON and SHA-256 storage owners.
/// Retained until committed history pages and old Claude journals cannot replay.
pub fn legacy_notice_identity(
    agent_id: &str,
    account_key: &str,
    turn_id: Option<&str>,
    params: &Value,
) -> Option<Value> {
    let usage = params.get("tokenUsage")?.as_object()?;
    Some(serde_json::json!([
        agent_id,
        account_key,
        "notice",
        turn_id,
        usage.get("total").unwrap_or(&Value::Null),
        usage.get("last").unwrap_or(&Value::Null)
    ]))
}

/// OpenAPI fragments rendered from the accounting owner, including named leaf types.
pub fn schemas() -> serde_json::Map<String, Value> {
    let mut generator = schemars::generate::SchemaSettings::draft2020_12()
        .for_serialize()
        .with(|s| s.definitions_path = "/components/schemas".into())
        .into_generator();
    generator.subschema_for::<UsageObservation>();
    generator.take_definitions(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn breakdown() -> Value {
        json!({"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":5,"outputTokens":10,"reasoningOutputTokens":3,"totalTokens":110})
    }
    #[test]
    fn native_codex_inclusive_input_becomes_cumulative_uncached_input() {
        let parsed = parse_token_usage(UsageProvider::Codex,&json!({"threadId":"t","turnId":"u","tokenUsage":{"total":breakdown(),"last":breakdown(),"modelContextWindow":200000}})).expect("native usage");
        let UsageObservation::Cumulative {
            scope, total, last, ..
        } = parsed
        else {
            panic!("cumulative");
        };
        assert_eq!(scope, UsageScope::CodexThread);
        assert_eq!(total.uncached_input, 75);
        assert_eq!(total.total(), Some(110));
        assert_eq!(last, Some(total));
    }
    #[test]
    fn legacy_claude_response_and_query_remain_supported() {
        let p = json!({"usageSource":"claudeResponse","responseId":"r","model":"claude","requestUsage":breakdown()});
        let UsageObservation::Response {
            tokens, evidence, ..
        } = parse_token_usage(UsageProvider::Claude, &p).expect("legacy response")
        else {
            panic!("response");
        };
        assert_eq!(tokens.uncached_input, 100);
        assert_eq!(evidence, UsageEvidence::Legacy);
        let UsageObservation::Cumulative { scope, .. } = parse_token_usage(
            UsageProvider::Claude,
            &json!({"tokenUsage":{"total":breakdown()}}),
        )
        .expect("legacy aggregate") else {
            panic!("cumulative");
        };
        assert_eq!(scope, UsageScope::ClaudeQuery);
    }
    #[test]
    fn rollout_response_and_counter_keep_distinct_evidence() {
        let p = json!({"usageSource":"responseRecord","responseId":"r","requestUsage":breakdown(),"rawTokenUsageRecord":{"model":"gpt"}});
        let UsageObservation::Response {
            tokens,
            evidence,
            model,
            ..
        } = parse_token_usage(UsageProvider::Codex, &p).expect("rollout response")
        else {
            panic!("response");
        };
        assert_eq!(evidence, UsageEvidence::Recovered);
        assert_eq!(model, "gpt");
        assert_eq!(tokens.uncached_input, 75);
        assert!(matches!(
            parse_token_usage(
                UsageProvider::Codex,
                &json!({"usageSource":"tokenCount","tokenUsage":{"total":breakdown()}})
            )
            .expect("rollout count"),
            UsageObservation::Cumulative {
                scope: UsageScope::CodexThread,
                ..
            }
        ));
    }
    #[test]
    fn studio_usage_rejects_unknown_fields_and_negative_or_fractional_tokens() {
        let mut usage = json!({"kind":"response","responseId":"r","model":"claude","evidence":"live","tokens":{"uncachedInput":1,"cacheReadInput":0,"cacheWriteInput":0,"output":2}});
        let parse = |v| parse_token_usage(UsageProvider::Claude, &json!({"studioUsage":v}));
        assert!(parse(usage.clone()).is_ok());
        usage["extra"] = json!(1);
        assert!(parse(usage.clone()).is_err());
        usage.as_object_mut().expect("object").remove("extra");
        usage["tokens"]["extra"] = json!(1);
        assert!(parse(usage.clone()).is_err());
        usage["tokens"]
            .as_object_mut()
            .expect("tokens")
            .remove("extra");
        for n in [json!(-1), json!(0.5), json!(true)] {
            usage["tokens"]["output"] = n;
            assert!(parse(usage.clone()).is_err());
        }
        assert!(parse(json!({"kind":"gap","responseId":"r","reason":"queryClosed"})).is_ok());
    }
    #[test]
    fn codex_serde_mirror_matches_vendored_provider_schema() {
        // Owner: codex-cli 0.164.0 `app-server generate-json-schema`.
        fn normalize(value: &mut Value) {
            match value {
                Value::Object(map) => {
                    for key in ["$schema", "title", "description"] {
                        map.remove(key);
                    }
                    if let Some(Value::Array(required)) = map.get_mut("required") {
                        required.sort_by_key(|value| value.as_str().unwrap_or_default().to_owned());
                    }
                    for value in map.values_mut() {
                        normalize(value);
                    }
                }
                Value::Array(values) => {
                    for value in values {
                        normalize(value);
                    }
                }
                _ => {}
            }
        }
        let mut generated = schemars::generate::SchemaSettings::draft07()
            .into_generator()
            .into_root_schema_for::<CodexThreadTokenUsageUpdatedNotification>()
            .to_value();
        let text = serde_json::to_string(&generated)
            .expect("schema")
            .replace("CodexThreadTokenUsage", "ThreadTokenUsage")
            .replace("CodexTokenUsageBreakdown", "TokenUsageBreakdown");
        generated = serde_json::from_str(&text).expect("renamed schema");
        let mut vendor: Value = serde_json::from_str(CODEX_USAGE_SCHEMA).expect("vendor schema");
        normalize(&mut generated);
        normalize(&mut vendor);
        assert_eq!(
            generated, vendor,
            "Rust mirror must match the Codex-generated schema owner"
        );
    }
}

#[cfg(test)]
mod legacy_tests {
    use super::*;
    use serde_json::json;
    #[test]
    fn old_claude_totals_do_not_invent_categories_or_accept_invalid_typed_usage() {
        let params = json!({"tokenUsage":{"total":{"totalTokens":25},"last":{"totalTokens":5},"modelContextWindow":1000}});
        assert!(parse_token_usage(UsageProvider::Claude, &params).is_err());
        let total =
            parse_legacy_totals(UsageProvider::Claude, &params).expect("pre-contract total");
        assert_eq!(total.total_tokens, 25);
        assert_eq!(total.last_total_tokens, Some(5));
        assert!(parse_legacy_totals(UsageProvider::Codex, &params).is_err());
        let mut invalid = params;
        invalid["studioUsage"] = json!({"kind":"unknown"});
        assert!(parse_legacy_totals(UsageProvider::Claude, &invalid).is_err());
    }
}
