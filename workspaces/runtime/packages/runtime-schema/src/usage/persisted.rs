//! Compatibility boundary for analytics rows written before the typed usage contract.
use super::{
    UsageEvidence, UsageObservation, UsageParseError, UsageProvider, UsageScope, UsageTokens,
};
use serde_json::Value;

/// Reconstruct history notifications without interpreting legacy columns in consumers.
/// Sparse Claude rows retain their known total for the legacy totals-only budget path.
pub fn analytics_usage_notification(
    provider: UsageProvider,
    row: &Value,
) -> Result<Value, UsageParseError> {
    match parse_analytics_usage_row(provider, row) {
        Ok(observation) => {
            let mut notification = serde_json::json!({"threadId":row["threadId"],"turnId":row["turnId"],"studioUsage":observation});
            if row.get("studioUsage").is_none() {
                notification["tokenUsage"] =
                    serde_json::json!({"total":row["total"],"last":row["last"]});
            }
            Ok(notification)
        }
        Err(_) if provider == UsageProvider::Claude && row.get("studioUsage").is_none() => Ok(
            serde_json::json!({"threadId":row["threadId"],"turnId":row["turnId"],"responseId":row["responseId"],
                "rawTokenUsageRecord":row["rawTokenUsageRecord"],"requestUsage":row["requestUsage"],
                "usageSource":row["usageSource"],"tokenUsage":{"total":row["total"],"last":row["last"]}}),
        ),
        Err(error) => Err(error),
    }
}

/// Decode canonical observations first, retaining legacy rows without rewriting them.
/// Legacy response evidence remains provisional until native journal recovery replaces it.
pub fn parse_analytics_usage_row(
    provider: UsageProvider,
    row: &Value,
) -> Result<UsageObservation, UsageParseError> {
    if let Some(value) = row.get("studioUsage") {
        return Ok(serde_json::from_value(value.clone())?);
    }
    let text = |key: &str| row.get(key).and_then(Value::as_str).map(str::to_owned);
    let turn_id = text("turnId");
    let model_context_window = row
        .get("modelContextWindow")
        .filter(|v| !v.is_null())
        .map(|v| legacy_count(v, "modelContextWindow"))
        .transpose()?;
    let input_uncached = row.get("inputTokensAreUncached").and_then(Value::as_bool) == Some(true)
        || row.get("usageSource").and_then(Value::as_str) == Some("claudeResponse");
    let token_provider = if input_uncached {
        UsageProvider::Claude
    } else {
        UsageProvider::Codex
    };
    if let Some(response_id) = text("responseId").filter(|id| !id.is_empty()) {
        let selected = row
            .get("delta")
            .filter(|v| known(v, "inputTokens") && known(v, "outputTokens"))
            .or_else(|| row.get("requestUsage").filter(|v| v.is_object()))
            .unwrap_or(&row["last"]);
        return Ok(UsageObservation::Response {
            response_id,
            turn_id,
            model: text("model").unwrap_or_default(),
            tokens: tokens(selected, token_provider)?,
            evidence: UsageEvidence::Legacy,
            parent_tool_use_id: text("parentToolUseId"),
            model_context_window,
        });
    }
    Ok(UsageObservation::Cumulative {
        scope: if provider == UsageProvider::Claude {
            UsageScope::ClaudeQuery
        } else {
            UsageScope::CodexThread
        },
        turn_id,
        total: tokens(&row["total"], token_provider)?,
        last: row
            .get("last")
            .filter(|v| known(v, "inputTokens") && known(v, "outputTokens"))
            .map(|v| tokens(v, token_provider))
            .transpose()?,
        model_context_window,
        models: None,
    })
}
fn known(value: &Value, key: &str) -> bool {
    value.get(key).is_some_and(|v| !v.is_null())
}
fn legacy_count(value: &Value, key: &str) -> Result<u64, UsageParseError> {
    if let Some(n) = value.as_u64() {
        return Ok(n);
    }
    if let Some(n) = value
        .as_f64()
        .filter(|n| n.is_finite() && *n >= 0.0 && n.fract() == 0.0 && *n < u64::MAX as f64)
    {
        return Ok(n as u64);
    }
    Err(UsageParseError(format!("invalid legacy analytics {key}")))
}
fn tokens(value: &Value, provider: UsageProvider) -> Result<UsageTokens, UsageParseError> {
    let count = |key: &str, required: bool| match value.get(key).filter(|v| !v.is_null()) {
        Some(v) => legacy_count(v, key),
        None if !required => Ok(0),
        None => Err(UsageParseError(format!("missing legacy analytics {key}"))),
    };
    let input = count("inputTokens", true)?;
    let read = count("cachedInputTokens", false)?;
    let write = count("cacheWriteInputTokens", false)?;
    let (uncached_input, cache_read_input, cache_write_input) = if provider == UsageProvider::Codex
    {
        let read = read.min(input);
        let write = write.min(input - read);
        (input - read - write, read, write)
    } else {
        (input, read, write)
    };
    Ok(UsageTokens {
        uncached_input,
        cache_read_input,
        cache_write_input,
        output: count("outputTokens", true)?,
        reasoning_output: value
            .get("reasoningOutputTokens")
            .filter(|v| !v.is_null())
            .map(|v| legacy_count(v, "reasoningOutputTokens"))
            .transpose()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn legacy_history_notification_preserves_exact_counter_receipt_objects() {
        let row = json!({"threadId":"thread","turnId":"turn",
            "total":{"inputTokens":10,"cachedInputTokens":2,"outputTokens":3,"totalTokens":13,"legacyExtension":1},
            "last":{"inputTokens":5,"outputTokens":1,"totalTokens":6}});
        let notification =
            analytics_usage_notification(UsageProvider::Codex, &row).expect("legacy notification");
        assert_eq!(notification["studioUsage"]["kind"], "cumulative");
        assert_eq!(
            notification["tokenUsage"]["total"], row["total"],
            "legacy receipt hashes include exact counter objects"
        );
        assert_eq!(notification["tokenUsage"]["last"], row["last"]);
    }

    #[test]
    fn analytics_schema_copies_match_the_usage_owner() {
        // Owner: runtime-schema::usage::{UsageObservation, UsageTokens}; schemas() is its renderer.
        let expected: Value = serde_json::from_str(
            &serde_json::to_string(&super::super::schemas())
                .expect("owner schema")
                .replace("#/components/schemas/", "#/$defs/"),
        )
        .expect("local references");
        for text in [
            include_str!("../schemas/analytics-usage.schema.json"),
            include_str!("../schemas/analytics-notice.schema.json"),
        ] {
            let schema: Value = serde_json::from_str(text).expect("analytics schema");
            assert_eq!(
                schema["properties"]["studioUsage"],
                json!({"$ref":"#/$defs/UsageObservation"})
            );
            for (name, value) in expected.as_object().expect("schemas") {
                assert_eq!(
                    &schema["$defs"][name], value,
                    "analytics schema must consume usage owner: {name}"
                );
            }
        }
    }
    #[test]
    fn legacy_rows_preserve_delta_and_uncached_semantics() {
        for (uncached, expected) in [(false, 7), (true, 10)] {
            let row = json!({"responseId":"r","model":"m","inputTokensAreUncached":uncached,
                "last":{"inputTokens":100,"outputTokens":200},
                "delta":{"inputTokens":10.0,"cachedInputTokens":2,"cacheWriteInputTokens":1,"outputTokens":3.0}});
            let UsageObservation::Response {
                tokens, evidence, ..
            } = parse_analytics_usage_row(UsageProvider::Codex, &row).expect("legacy row")
            else {
                panic!("response")
            };
            assert_eq!(tokens.uncached_input, expected);
            assert_eq!(tokens.output, 3);
            assert_eq!(evidence, UsageEvidence::Legacy);
        }
        let row = json!({"total":{"inputTokens":10,"outputTokens":2},"last":{}});
        assert!(matches!(
            parse_analytics_usage_row(UsageProvider::Claude, &row).expect("old cumulative"),
            UsageObservation::Cumulative {
                scope: UsageScope::ClaudeQuery,
                ..
            }
        ));
    }
    #[test]
    fn canonical_rows_remain_strict_and_take_precedence() {
        let mut row = json!({"last":{},"studioUsage":{"kind":"response","responseId":"r","model":"m","evidence":"live","tokens":{"uncachedInput":1,"cacheReadInput":2,"cacheWriteInput":3,"output":4}}});
        assert!(matches!(
            parse_analytics_usage_row(UsageProvider::Codex, &row).expect("canonical"),
            UsageObservation::Response {
                evidence: UsageEvidence::Live,
                ..
            }
        ));
        row["studioUsage"]["tokens"]["extra"] = json!(1);
        assert!(parse_analytics_usage_row(UsageProvider::Codex, &row).is_err());
    }
}
