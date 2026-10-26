//! Serialized models.dev pricing artifact and the catalog fields read by Studio.
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// The artifact envelope; the catalog stays a document because its signature is the hash
/// of its canonical JSON text.
#[derive(serde::Deserialize, schemars::JsonSchema)]
pub struct PricingArtifact {
    pub version: u64,
    #[schemars(with = "RawCatalog")]
    pub catalog: Value,
    #[serde(default, rename = "lastAttemptAt")]
    pub last_attempt_at: Option<f64>,
    #[serde(default, rename = "fetchedAtEpoch")]
    pub fetched_at_epoch: Option<f64>,
    #[serde(default, rename = "fetchedAt")]
    pub fetched_at: Option<String>,
}

/// The models.dev catalog members this module reads. The artifact is Studio's own
/// download of a published schema, so a catalog of another shape is invalid.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct RawCatalog {
    pub providers: RawProviders,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RawProviders {
    pub openai: Option<RawProvider>,
    pub anthropic: Option<RawProvider>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RawProvider {
    pub models: BTreeMap<String, RawModel>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RawModel {
    pub release_date: Option<String>,
    pub cost: Option<RawCost>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RawCost {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
    #[serde(default)]
    pub tiers: Vec<RawTier>,
    pub context_over_200k: Option<RawRates>,
}

/// The rates one tier (or the legacy over-200k block) overrides.
#[derive(Deserialize, schemars::JsonSchema)]
pub struct RawRates {
    pub input: Option<f64>,
    pub output: Option<f64>,
    pub cache_read: Option<f64>,
    pub cache_write: Option<f64>,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RawTier {
    pub tier: Option<RawTierKey>,
    #[serde(flatten)]
    pub rates: RawRates,
}

#[derive(Deserialize, schemars::JsonSchema)]
pub struct RawTierKey {
    #[serde(rename = "type")]
    pub kind: Option<String>,
    pub size: Option<f64>,
}
