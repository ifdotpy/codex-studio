//! Validated model pricing and token cost calculation.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::{Arc, LazyLock};

use serde::Deserialize;
use serde_json::Value;
use studio_runtime_schema::pricing::{PricingArtifact, RawCatalog, RawModel, RawRates, RawTier};
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

/// Path of the models.dev pricing artifact under the state root.
pub(crate) const PRICING_ARTIFACT: &str = "model-pricing/models-dev-v1.json";

/// The pricing artifact (about 6 MB of JSON) parsed once per file version.
pub struct LoadedPricing {
    /// `None` when the artifact is not a valid version-1 catalog.
    pub catalog: Option<PricingCatalog>,
    /// Cache signature of the catalog, which stored session costs record.
    pub signature: Option<String>,
    /// Most recent catalog refresh attempt, in Unix epoch seconds.
    pub(crate) last_attempt_at: f64,
}

static LOADED: LazyLock<crate::file_derived::FileDerived<LoadedPricing>> =
    LazyLock::new(crate::file_derived::FileDerived::default);

/// The current pricing artifact of `state_root`, derived again only after the file changed.
/// `None` when the file is missing or unreadable.
pub fn load_pricing(state_root: &Path) -> Option<Arc<LoadedPricing>> {
    LOADED.get_or_load(&state_root.join(PRICING_ARTIFACT), |path| {
        let artifact = serde_json::from_slice::<PricingArtifact>(&std::fs::read(path).ok()?).ok();
        let catalog_value = artifact
            .as_ref()
            .filter(|artifact| artifact.version == 1)
            .map(|artifact| &artifact.catalog);
        let catalog = catalog_value.and_then(|value| PricingCatalog::from_value(value).ok());
        let signature = catalog_value
            .filter(|_| catalog.is_some())
            .and_then(crate::analytics::catalog_signature);
        let last_attempt_at = artifact
            .as_ref()
            .and_then(|artifact| {
                artifact
                    .last_attempt_at
                    .or(artifact.fetched_at_epoch)
                    .or_else(|| {
                        artifact
                            .fetched_at
                            .as_deref()
                            .and_then(|stamp| chrono::DateTime::parse_from_rfc3339(stamp).ok())
                            .map(|stamp| stamp.timestamp() as f64)
                    })
            })
            .unwrap_or_default();
        Some(LoadedPricing {
            catalog,
            signature,
            last_attempt_at,
        })
    })
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
        let catalog = RawCatalog::deserialize(value).map_err(|_| PricingError::InvalidCatalog)?;
        let mut models = BTreeMap::new();
        for (provider, source) in [
            ("openai", catalog.providers.openai),
            ("anthropic", catalog.providers.anthropic),
        ] {
            let source = source.ok_or(PricingError::InvalidCatalog)?.models;
            let parsed = source
                .into_iter()
                .filter_map(|(name, record)| Some((name, model_rates(record)?)))
                .collect::<Vec<_>>();
            if parsed.is_empty() {
                return Err(PricingError::InvalidCatalog);
            }
            for (name, rates) in parsed {
                models.insert((provider.to_owned(), name), rates);
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
        usage: TokenUsage,
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

fn model_rates(model: RawModel) -> Option<Rates> {
    let cost = model.cost?;
    let input = valid_amount(cost.input)?;
    let output = valid_amount(cost.output)?;
    let mut tiers = cost
        .tiers
        .into_iter()
        .filter_map(tier_price)
        .collect::<Vec<_>>();
    tiers.sort_by(|left, right| left.size.total_cmp(&right.size));
    let legacy_over = cost
        .context_over_200k
        .map(|over| rates_price(over, 200_000.0));
    Some(Rates {
        release_date: model.release_date,
        input,
        output,
        cache_read: valid_amount(cost.cache_read),
        cache_write: valid_amount(cost.cache_write),
        tiers,
        legacy_over,
    })
}

fn tier_price(tier: RawTier) -> Option<PriceTier> {
    let key = tier.tier?;
    (key.kind.as_deref() == Some("context")).then_some(())?;
    let size = valid_amount(key.size)?;
    Some(rates_price(tier.rates, size))
}

fn rates_price(rates: RawRates, size: f64) -> PriceTier {
    PriceTier {
        size,
        input: valid_amount(rates.input),
        output: valid_amount(rates.output),
        cache_read: valid_amount(rates.cache_read),
        cache_write: valid_amount(rates.cache_write),
    }
}

/// Anthropic one-hour cache-write USD per million as a ratio to the selected input rate.
/// Other-language consumers: installed-smoke/usage.ts, guarded by its owner parity test.
pub const CLAUDE_CACHE_WRITE_1H_INPUT_MULTIPLIER: f64 = 2.0;

/// Token counts of one priced sample; a missing count is `None`.
#[derive(Clone, Copy, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenUsage {
    pub input_tokens: Option<f64>,
    pub cached_input_tokens: Option<f64>,
    pub cache_write_input_tokens: Option<f64>,
    pub cache_write_input_1h_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
}

fn price_with_rates(
    rates: &Rates,
    usage: TokenUsage,
    context_tokens: Option<f64>,
    input_tokens_are_uncached: bool,
) -> Price {
    let input = valid_amount(usage.input_tokens);
    let output = valid_amount(usage.output_tokens);
    let Some((input, output)) = input.zip(output) else {
        return Price::incomplete(false);
    };
    let cached = valid_amount(usage.cached_input_tokens);
    let mut cache_write = valid_amount(usage.cache_write_input_tokens);
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
    let hour = match usage.cache_write_input_1h_tokens {
        None => 0.0,
        Some(value) if value.is_finite() && value >= 0.0 && value <= cache_write => value,
        Some(_) => return Price::incomplete(tier_used),
    };
    if (cached > 0.0 && read_rate.is_none()) || (cache_write > hour && write_rate.is_none()) {
        return Price::incomplete(tier_used);
    }
    let total = (base * input_rate
        + output * output_rate
        + cached * read_rate.unwrap_or(0.0)
        + (cache_write - hour) * write_rate.unwrap_or(0.0)
        + hour * CLAUDE_CACHE_WRITE_1H_INPUT_MULTIPLIER * input_rate)
        / 1_000_000.0;
    Price {
        amount_usd: Some(total),
        status: PriceStatus::Priced,
        context_tier_used: tier_used,
    }
}

/// A finite, nonnegative price or count.
fn valid_amount(value: Option<f64>) -> Option<f64> {
    value.filter(|number| number.is_finite() && *number >= 0.0)
}

#[cfg(test)]
mod tests {
    use super::{PriceStatus, PricingCatalog};
    use serde_json::json;

    fn usage(value: serde_json::Value) -> super::TokenUsage {
        serde_json::from_value(value).expect("token usage")
    }

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
    fn cache_write_1h_prices_selected_input_tier_and_preserves_legacy() {
        let catalog = catalog();
        let split = usage(
            json!({"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":10,"cacheWriteInput1hTokens":6,"outputTokens":5}),
        );
        let price = catalog.price_usage("openai", "gpt-test", split, Some(100.0), true);
        assert_eq!(price.amount_usd, Some(384.0 / 1_000_000.0));
        let legacy = usage(
            json!({"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":10,"outputTokens":5}),
        );
        assert_eq!(
            catalog
                .price_usage("openai", "gpt-test", legacy, Some(100.0), true)
                .amount_usd,
            Some(360.0 / 1_000_000.0)
        );
    }
    #[test]
    fn prices_cached_tokens_and_inclusive_context_tier() {
        let value = catalog().price_usage(
            "openai",
            "gpt-test",
            usage(json!({"inputTokens":100,"cachedInputTokens":20,
                "cacheWriteInputTokens":10,"outputTokens":5})),
            Some(100.0),
            false,
        );
        assert_eq!(value.status, PriceStatus::Priced);
        assert!(value.context_tier_used);
        // 70 uncached input at 3, 5 output at 4, 20 cache reads at 1 and
        // 10 cache writes at 2 USD per million tokens.
        assert_eq!(value.amount_usd, Some(0.00027));
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
        let tokens = usage(json!({"inputTokens":100,"outputTokens":10}));
        let at_boundary =
            catalog.price_usage("openai", "gpt-legacy", tokens, Some(200_000.0), false);
        let above_boundary =
            catalog.price_usage("openai", "gpt-legacy", tokens, Some(200_001.0), false);
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
                    usage(json!({
                        "inputTokens":2,"outputTokens":3
                    })),
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
                    usage(json!({
                        "inputTokens":2,"cachedInputTokens":0,"outputTokens":3
                    })),
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
                .price_usage("openai", "gpt-test", usage(json!({})), None, false)
                .status,
            PriceStatus::Incomplete
        );
        assert_eq!(
            catalog
                .price_usage("openai", "unknown", usage(json!({})), None, false)
                .status,
            PriceStatus::Unpriced
        );
    }
}
