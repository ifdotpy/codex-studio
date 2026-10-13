//! Validated model pricing and token cost calculation.

use std::collections::BTreeMap;

use serde_json::Value;
use thiserror::Error;

#[derive(Debug, Error, Eq, PartialEq)]
pub enum PricingError {
    #[error("pricing catalog is invalid")]
    InvalidCatalog,
}

#[derive(Clone, Debug, PartialEq)]
struct Rates {
    release_date: Option<String>,
    input: f64,
    output: f64,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
    tiers: Vec<PriceTier>,
    legacy_over: Option<PriceTier>,
}

#[derive(Clone, Debug, PartialEq)]
struct PriceTier {
    size: f64,
    input: Option<f64>,
    output: Option<f64>,
    cache_read: Option<f64>,
    cache_write: Option<f64>,
}

/// A catalog accepted from a validated models.dev artifact.
///
/// Fields are private so every instance has passed the same constructor.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct PricingCatalog {
    models: BTreeMap<(String, String), Rates>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PriceStatus {
    Priced,
    Incomplete,
    Unpriced,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Price {
    pub amount_usd: Option<f64>,
    pub status: PriceStatus,
    pub context_tier_used: bool,
}

impl PricingCatalog {
    pub fn from_value(value: &Value) -> Result<Self, PricingError> {
        let providers = value
            .get("providers")
            .and_then(Value::as_object)
            .ok_or(PricingError::InvalidCatalog)?;
        let mut models = BTreeMap::new();
        for provider in ["openai", "anthropic"] {
            let source = providers
                .get(provider)
                .and_then(|entry| entry.get("models"))
                .and_then(Value::as_object)
                .ok_or(PricingError::InvalidCatalog)?;
            if source.values().all(|entry| parse_rates(entry).is_none()) {
                return Err(PricingError::InvalidCatalog);
            }
            for (name, record) in source {
                if let Some(rates) = parse_rates(record) {
                    models.insert((provider.to_owned(), name.clone()), rates);
                }
            }
        }
        Ok(Self { models })
    }

    fn rates(&self, provider: &str, model: &str) -> Option<&Rates> {
        let normalized = model.strip_suffix("[1m]").unwrap_or(model);
        self.models
            .get(&(provider.to_owned(), normalized.to_owned()))
            .or_else(|| {
                let family = (provider == "anthropic")
                    .then(|| self.latest_claude_family(normalized))
                    .flatten();
                family.and_then(|name| self.models.get(&(provider.to_owned(), name)))
            })
            .or_else(|| {
                let (base, date) = normalized.rsplit_once('-')?;
                (date.len() == 8 && date.bytes().all(|byte| byte.is_ascii_digit()))
                    .then(|| self.models.get(&(provider.to_owned(), base.to_owned())))
                    .flatten()
            })
    }

    fn latest_claude_family(&self, alias: &str) -> Option<String> {
        if alias.is_empty() || !alias.bytes().all(|byte| byte.is_ascii_lowercase()) {
            return None;
        }
        self.models
            .iter()
            .filter_map(|((provider, name), rates)| {
                let suffix = name.strip_prefix(&format!("claude-{alias}-"))?;
                suffix
                    .starts_with(|character: char| character.is_ascii_digit())
                    .then_some((name, rates))
                    .filter(|_| {
                        suffix
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || byte == b'.' || byte == b'-')
                            && provider == "anthropic"
                    })
            })
            .max_by(|(left_name, left), (right_name, right)| {
                left.release_date
                    .cmp(&right.release_date)
                    .then_with(|| left_name.cmp(right_name))
            })
            .map(|(name, _)| name.clone())
    }

    pub fn price_usage(
        &self,
        provider: &str,
        model: &str,
        usage: &Value,
        context_tokens: Option<f64>,
        input_tokens_are_uncached: bool,
    ) -> Price {
        let Some(rates) = self.rates(provider, model) else {
            return Price::unpriced();
        };
        price_with_rates(rates, usage, context_tokens, input_tokens_are_uncached)
    }
}

impl Price {
    fn unpriced() -> Self {
        Self {
            amount_usd: None,
            status: PriceStatus::Unpriced,
            context_tier_used: false,
        }
    }

    fn incomplete(tier: bool) -> Self {
        Self {
            amount_usd: None,
            status: PriceStatus::Incomplete,
            context_tier_used: tier,
        }
    }
}

fn parse_rates(record: &Value) -> Option<Rates> {
    let cost = record.get("cost")?;
    let input = amount(cost.get("input")?)?;
    let output = amount(cost.get("output")?)?;
    let mut tiers = cost
        .get("tiers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(parse_tier)
        .collect::<Vec<_>>();
    tiers.sort_by(|left, right| left.size.total_cmp(&right.size));
    let legacy_over = cost
        .get("context_over_200k")
        .and_then(|over| parse_optional_tier(over, 200_000.0));
    Some(Rates {
        release_date: record
            .get("release_date")
            .and_then(Value::as_str)
            .map(str::to_owned),
        input,
        output,
        cache_read: cost.get("cache_read").and_then(amount),
        cache_write: cost.get("cache_write").and_then(amount),
        tiers,
        legacy_over,
    })
}

fn parse_tier(value: &Value) -> Option<PriceTier> {
    let size = value
        .get("tier")
        .filter(|tier| tier.get("type").and_then(Value::as_str) == Some("context"))
        .and_then(|tier| tier.get("size"))
        .and_then(amount)?;
    parse_optional_tier(value, size)
}

fn parse_optional_tier(value: &Value, size: f64) -> Option<PriceTier> {
    Some(PriceTier {
        size,
        input: value.get("input").and_then(amount),
        output: value.get("output").and_then(amount),
        cache_read: value.get("cache_read").and_then(amount),
        cache_write: value.get("cache_write").and_then(amount),
    })
}

fn price_with_rates(
    rates: &Rates,
    usage: &Value,
    context_tokens: Option<f64>,
    input_tokens_are_uncached: bool,
) -> Price {
    let input = usage.get("inputTokens").and_then(amount);
    let output = usage.get("outputTokens").and_then(amount);
    let Some((input, output)) = input.zip(output) else {
        return Price::incomplete(false);
    };
    let cached = usage.get("cachedInputTokens").and_then(amount);
    let mut cache_write = usage.get("cacheWriteInputTokens").and_then(amount);
    if rates.cache_read.is_some() && cached.is_none() {
        return Price::incomplete(false);
    }
    if rates.cache_write.is_some() && cache_write.is_none() {
        cache_write = Some(0.0);
    }
    let (base, cached, cache_write) = if input_tokens_are_uncached {
        (input, cached.unwrap_or(0.0), cache_write.unwrap_or(0.0))
    } else {
        let cached = cached.unwrap_or(0.0).min(input);
        let cache_write = cache_write.unwrap_or(0.0).min((input - cached).max(0.0));
        ((input - cached - cache_write).max(0.0), cached, cache_write)
    };
    let threshold = rates
        .tiers
        .first()
        .map(|tier| tier.size)
        .or_else(|| rates.legacy_over.as_ref().map(|tier| tier.size));
    let inclusive_tier = !rates.tiers.is_empty();
    let tier_used = threshold.is_some_and(|threshold| {
        context_tokens.is_some_and(|context| {
            if inclusive_tier {
                context >= threshold
            } else {
                context > threshold
            }
        })
    });
    let selected = if tier_used && !rates.tiers.is_empty() {
        context_tokens
            .and_then(|context| rates.tiers.iter().rev().find(|tier| context >= tier.size))
    } else if tier_used {
        rates.legacy_over.as_ref()
    } else {
        None
    };
    let input_rate = selected.and_then(|tier| tier.input).unwrap_or(rates.input);
    let output_rate = selected
        .and_then(|tier| tier.output)
        .unwrap_or(rates.output);
    let read_rate = selected
        .and_then(|tier| tier.cache_read)
        .or(rates.cache_read);
    let write_rate = selected
        .and_then(|tier| tier.cache_write)
        .or(rates.cache_write);
    if (cached > 0.0 && read_rate.is_none()) || (cache_write > 0.0 && write_rate.is_none()) {
        return Price::incomplete(tier_used);
    }
    let total = (base * input_rate
        + output * output_rate
        + cached * read_rate.unwrap_or(0.0)
        + cache_write * write_rate.unwrap_or(0.0))
        / 1_000_000.0;
    Price {
        amount_usd: Some(total),
        status: PriceStatus::Priced,
        context_tier_used: tier_used,
    }
}

fn amount(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .filter(|number| number.is_finite() && *number >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::{PriceStatus, PricingCatalog};
    use serde_json::json;

    fn catalog() -> PricingCatalog {
        PricingCatalog::from_value(&json!({"providers": {
            "openai": {"models": {"gpt-test": {"cost": {
                "input": 1.0, "output": 2.0, "cache_read": 0.5, "cache_write": 1.5,
                "tiers": [{"tier":{"type":"context","size":100},
                    "input":3.0,"output":4.0,"cache_read":1.0,"cache_write":2.0}]
            }}}},
            "anthropic": {"models": {"claude-sonnet-5-5": {"release_date":"2026-01-01",
                "cost":{"input":5,"output":10}}}}
        }}))
        .expect("valid catalog")
    }

    #[test]
    fn prices_cached_tokens_and_inclusive_context_tier() {
        let value = catalog().price_usage(
            "openai",
            "gpt-test",
            &json!({"inputTokens":100,"cachedInputTokens":20,
                "cacheWriteInputTokens":10,"outputTokens":5}),
            Some(100.0),
            false,
        );
        assert_eq!(value.status, PriceStatus::Priced);
        assert!(value.context_tier_used);
        assert_eq!(
            value.amount_usd,
            Some((70.0 * 3.0 + 5.0 * 4.0 + 20.0 + 20.0) / 1_000_000.0)
        );
    }

    #[test]
    fn legacy_context_over_200k_boundary_is_strict() {
        let catalog = PricingCatalog::from_value(&json!({
            "providers": {
                "openai": {"models": {"gpt-legacy": {"cost": {
                    "input": 1, "output": 2,
                    "context_over_200k": {"input": 10, "output": 20}
                }}}},
                "anthropic": {"models": {"claude-test": {"cost": {"input": 1, "output": 1}}}}
            }
        }))
        .expect("legacy catalog");
        let usage = json!({"inputTokens":100,"outputTokens":10});
        let at_boundary =
            catalog.price_usage("openai", "gpt-legacy", &usage, Some(200_000.0), false);
        let above_boundary =
            catalog.price_usage("openai", "gpt-legacy", &usage, Some(200_001.0), false);
        assert!(!at_boundary.context_tier_used);
        assert_eq!(at_boundary.amount_usd, Some(120.0 / 1_000_000.0));
        assert!(above_boundary.context_tier_used);
        assert_eq!(above_boundary.amount_usd, Some(1_200.0 / 1_000_000.0));
    }

    #[test]
    fn aliases_and_dated_model_names_resolve_to_catalog_records() {
        let catalog = catalog();
        assert_eq!(
            catalog
                .price_usage(
                    "anthropic",
                    "sonnet",
                    &json!({
                        "inputTokens":2,"outputTokens":3
                    }),
                    None,
                    false
                )
                .amount_usd,
            Some(40.0 / 1_000_000.0)
        );
        assert_eq!(
            catalog
                .price_usage(
                    "openai",
                    "gpt-test-20261001",
                    &json!({
                        "inputTokens":2,"cachedInputTokens":0,"outputTokens":3
                    }),
                    None,
                    false
                )
                .amount_usd,
            Some(8.0 / 1_000_000.0)
        );
    }

    #[test]
    fn missing_required_usage_is_incomplete_and_unknown_model_is_unpriced() {
        let catalog = catalog();
        assert_eq!(
            catalog
                .price_usage("openai", "gpt-test", &json!({}), None, false)
                .status,
            PriceStatus::Incomplete
        );
        assert_eq!(
            catalog
                .price_usage("openai", "unknown", &json!({}), None, false)
                .status,
            PriceStatus::Unpriced
        );
    }
}
