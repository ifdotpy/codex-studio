//! Read-only session cost estimates over the shared analytics store.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use serde_json::{Value, json};
use studio_runtime_store::{
    AnalyticsRepository, AnalyticsStoreError, CostUsageRepositoryError, RuntimeStore, analytics,
    read_cost_usage,
};
use thiserror::Error;

use crate::cost_usage::{CostUsageError, CostUsageSnapshot};
use crate::pricing::PricingCatalog;

#[derive(Debug, Error)]
pub enum CostsError {
    #[error("costs require dependency {0}")]
    Dependency(&'static str),
    #[error(transparent)]
    Analytics(#[from] analytics::SessionCostError),
    #[error(transparent)]
    Repository(#[from] AnalyticsStoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error(transparent)]
    Usage(#[from] CostUsageRepositoryError),
    #[error(transparent)]
    Grouping(#[from] CostUsageError),
    #[error("pricing artifact is invalid")]
    InvalidPricing,
    #[error("cost computation failed")]
    Computation,
}

/// Return a current estimate, using the existing Python-published cache when valid.
pub fn session_cost(store: &RuntimeStore, agent_id: &str) -> Result<Value, CostsError> {
    if let Some(value) = crate::analytics::persisted_session_cost(store, agent_id)? {
        return Ok(value);
    }
    let repository = AnalyticsRepository::new(store.root());
    let mut db = repository.open_read_only()?;
    let tx = db.transaction()?;
    let root = analytics::cost_root(&tx, agent_id)?;
    let generation = analytics::usage_signature(&tx, &root)?.generation();
    let members = analytics::cost_agents(&tx, agent_id, &root)?;
    let mut providers_by_account = BTreeMap::new();
    let mut has_claude_history = false;
    for (_, record) in &members {
        let provider = record.get("provider").and_then(Value::as_str);
        has_claude_history |= provider == Some("claude");
        if let (Some(key), Some(provider)) =
            (record.get("accountKey").and_then(Value::as_str), provider)
        {
            if let Some(provider) = pricing_provider(provider) {
                providers_by_account.insert(key.to_owned(), provider.to_owned());
            }
        }
        if let Some(history) = record.get("accountHistory").and_then(Value::as_array) {
            for entry in history {
                let (Some(key), Some(provider)) = (
                    entry.get("accountKey").and_then(Value::as_str),
                    entry.get("provider").and_then(Value::as_str),
                ) else {
                    continue;
                };
                has_claude_history |= provider == "claude";
                if let Some(provider) = pricing_provider(provider) {
                    providers_by_account.insert(key.to_owned(), provider.to_owned());
                }
            }
        }
    }
    if has_claude_history {
        return Err(CostsError::Dependency("costs/claude-local-usage"));
    }
    let rows = read_cost_usage(&tx, &root)?;
    tx.commit()?;
    let snapshot = CostUsageSnapshot::from_records(rows.into_iter().map(|row| {
        (
            row.sequence,
            row.agent,
            row.thread,
            row.turn,
            row.at,
            row.record,
        )
    }))?;

    let pricing_path = store.root().join("model-pricing/models-dev-v1.json");
    let raw =
        fs::read(pricing_path).map_err(|_| CostsError::Dependency("costs/pricing-refresh"))?;
    let artifact: Value = serde_json::from_slice(&raw).map_err(|_| CostsError::InvalidPricing)?;
    if artifact.get("version").and_then(Value::as_u64) != Some(1) {
        return Err(CostsError::InvalidPricing);
    }
    let catalog_value = artifact.get("catalog").ok_or(CostsError::InvalidPricing)?;
    let catalog =
        PricingCatalog::from_value(catalog_value).map_err(|_| CostsError::InvalidPricing)?;

    let mut provider_totals = BTreeMap::<String, f64>::new();
    let mut model_totals = BTreeMap::<String, f64>::new();
    let mut unknown = BTreeSet::new();
    let mut priced_count = 0_u64;
    let mut total = 0.0;
    let mut tier_used = false;
    for group in snapshot.groups() {
        let model = group.model.as_deref().unwrap_or("Unknown model");
        if model == "<synthetic>" {
            continue;
        }
        let provider = provider_for(model).or_else(|| {
            providers_by_account
                .get(&group.account_key)
                .map(String::as_str)
        });
        let Some(provider) = provider else {
            unknown.insert(model.to_owned());
            continue;
        };
        let usage = json!({
            "inputTokens": group.input_tokens,
            "cachedInputTokens": group.cached_tokens,
            "cacheWriteInputTokens": group.write_tokens,
            "outputTokens": group.output_tokens,
        });
        let context = group.input_tokens.map(|input| {
            if group.input_uncached {
                input + group.cached_tokens.unwrap_or(0.0) + group.write_tokens.unwrap_or(0.0)
            } else {
                input
            }
        });
        let price = catalog.price_usage(provider, model, &usage, context, group.input_uncached);
        let Some(amount) = price.amount_usd.filter(|amount| amount.is_finite()) else {
            unknown.insert(model.to_owned());
            continue;
        };
        let count = group.count as f64;
        let subtotal = amount * count;
        total += subtotal;
        priced_count = priced_count.saturating_add(group.count as u64);
        *provider_totals.entry(provider.to_owned()).or_default() += subtotal;
        *model_totals.entry(model.to_owned()).or_default() += subtotal;
        tier_used |= price.context_tier_used;
    }
    if !total.is_finite() {
        return Err(CostsError::Computation);
    }
    Ok(json!({
        "rootId": root,
        "generation": generation,
        "totalUSD": if priced_count == 0 { Value::Null } else { json!(total) },
        "pricedSamples": priced_count,
        "breakdown": {"providers": provider_totals, "models": model_totals},
        "unknownModels": unknown,
        "estimated": true,
        "pricingState": "ready",
        "cacheAgeSeconds": 0,
        "refreshing": false,
        "claudeHistoryIncomplete": false,
        "method": format!("API prices from models.dev; cached input rates are applied when reported.{}",
            if tier_used { " Published context tier applied where request size matched its threshold." }
            else { " Base rates used when request size was unavailable or below the published threshold." }),
    }))
}

fn provider_for(model: &str) -> Option<&'static str> {
    if model.starts_with("claude-")
        || matches!(
            model.strip_suffix("[1m]").unwrap_or(model),
            "opus" | "sonnet" | "haiku" | "fable"
        )
    {
        Some("anthropic")
    } else if model.starts_with("gpt-")
        || model.starts_with("o1")
        || model.starts_with("o3")
        || model.starts_with("o4")
    {
        Some("openai")
    } else {
        None
    }
}

fn pricing_provider(provider: &str) -> Option<&'static str> {
    match provider {
        "openai" | "codex" => Some("openai"),
        "anthropic" | "claude" => Some("anthropic"),
        _ => None,
    }
}
