//! Session cost estimates over the incremental analytics cost aggregate.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::sync::LazyLock;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_store::cost_aggregate::{
    CostMember, CostReceipt, CostTokens, DEFAULT_ACCOUNT, cost_groups,
};
use studio_runtime_store::{
    AnalyticsRepository, AnalyticsStoreError, RuntimeStore, StoreError, analytics,
};
use thiserror::Error;

use crate::file_derived::FileDerived;
use crate::pricing::TokenUsage;

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
    Store(#[from] StoreError),
    #[error("Claude session state is invalid")]
    InvalidSessionState,
    #[error("pricing artifact is invalid")]
    InvalidPricing,
    #[error("cost computation failed")]
    Computation,
    #[error("Claude session costs require account profile snapshots")]
    AccountProfilesRequired,
}

/// Return a current estimate without account snapshots; a tree with Claude
/// history needs them ([`session_cost_with_account_profiles`]).
pub fn session_cost(store: &RuntimeStore, agent_id: &str) -> Result<Value, CostsError> {
    session_cost_inner(store, agent_id, &BTreeMap::new(), false)
}

/// Return a session estimate with account snapshots used to locate local Claude profiles.
pub fn session_cost_with_account_profiles(
    store: &RuntimeStore,
    agent_id: &str,
    accounts: &BTreeMap<String, Value>,
) -> Result<Value, CostsError> {
    session_cost_inner(store, agent_id, accounts, true)
}

/// A non-empty string.
fn nonempty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|value| !value.is_empty())
}

fn session_cost_inner(
    store: &RuntimeStore,
    agent_id: &str,
    accounts: &BTreeMap<String, Value>,
    has_account_profiles: bool,
) -> Result<Value, CostsError> {
    let account_providers = accounts
        .iter()
        .filter_map(|(key, account)| Some((key.as_str(), account.get("provider")?.as_str()?)))
        .collect::<BTreeMap<_, _>>();
    let repository = AnalyticsRepository::new(store.root());
    let mut db = repository.open_read_only()?;
    let tx = db.transaction()?;
    let root = analytics::cost_root(&tx, agent_id)?;
    let generation = analytics::usage_generation(&tx, &root)?;
    let members = analytics::cost_agents(&tx, agent_id, &root)?;
    let mut providers_by_account = BTreeMap::<&str, &'static str>::new();
    let mut has_claude_history = false;
    for (_, member) in &members {
        let provider = nonempty(&member.provider);
        has_claude_history |= provider == Some("claude");
        let account_key = member.account_key.as_deref().unwrap_or(DEFAULT_ACCOUNT);
        let current = account_providers
            .get(account_key)
            .copied()
            .filter(|value| !value.is_empty());
        // The current account's provider stands in for a missing embedded one.
        has_claude_history |= provider.is_none() && current == Some("claude");
        if let Some(resolved) = provider.or(current).and_then(pricing_provider) {
            providers_by_account.insert(account_key, resolved);
        }
        for entry in member.account_history.iter().flatten() {
            let (Some(key), Some(provider)) = (&entry.account_key, &entry.provider) else {
                continue;
            };
            has_claude_history |= provider == "claude";
            if let Some(provider) = pricing_provider(provider) {
                providers_by_account.insert(key, provider);
            }
        }
    }
    if has_claude_history && !has_account_profiles {
        return Err(CostsError::AccountProfilesRequired);
    }
    let mut local_claude = read_local_claude_usage(store.root(), &members, accounts)?;
    let receipts = local_claude
        .iter()
        .map(|row| {
            (
                row.account_key.clone(),
                row.thread_id.clone(),
                row.usage.id.clone(),
            )
        })
        .collect::<BTreeSet<CostReceipt>>();
    let partition =
        studio_runtime_store::cost_aggregate::partition_journal_receipts(&tx, &receipts)?;
    local_claude.retain(|row| {
        partition.accepted.contains(&(
            row.account_key.clone(),
            row.thread_id.clone(),
            row.usage.id.clone(),
        ))
    });
    let groups = cost_groups(&tx, &root, &partition.accepted)?;
    tx.commit()?;

    let loaded = crate::pricing::load_pricing(store.root())
        .ok_or(CostsError::Dependency("costs/pricing-refresh"))?;
    let catalog = loaded.catalog.as_ref().ok_or(CostsError::InvalidPricing)?;

    let mut provider_totals = BTreeMap::<&str, f64>::new();
    let mut model_totals = BTreeMap::<&str, f64>::new();
    let mut unknown = BTreeSet::<&str>::new();
    let mut priced_count = 0_u64;
    let mut total = 0.0;
    let mut tier_used = false;
    let priced = groups
        .iter()
        .map(|group| {
            (
                group.model.as_deref().unwrap_or("Unknown model"),
                Some(group.account_key.as_str()),
                group.tokens,
                group.input_uncached,
                group.count,
            )
        })
        .chain(local_claude.iter().map(|row| {
            (
                row.usage.model.as_str(),
                None,
                row.usage.tokens,
                row.usage.tokens.input_tokens.is_some(),
                1,
            )
        }));
    for (model, account_key, tokens, uncached, count) in priced {
        if model == "<synthetic>" {
            continue;
        }
        // Analytics rows fall back to their account's provider; Claude log rows are Anthropic.
        let provider = match account_key {
            Some(account_key) => {
                provider_for(model).or_else(|| providers_by_account.get(account_key).copied())
            }
            None => Some("anthropic"),
        };
        let Some(provider) = provider else {
            unknown.insert(model);
            continue;
        };
        let usage = TokenUsage {
            input_tokens: tokens.input_tokens,
            cached_input_tokens: tokens.cached_input_tokens,
            cache_write_input_tokens: tokens.cache_write_input_tokens,
            cache_write_input_1h_tokens: tokens.cache_write_input_1h_tokens,
            output_tokens: tokens.output_tokens,
        };
        let context = tokens.input_tokens.map(|input| {
            if uncached {
                input
                    + tokens.cached_input_tokens.unwrap_or(0.0)
                    + tokens.cache_write_input_tokens.unwrap_or(0.0)
            } else {
                input
            }
        });
        let price = catalog.price_usage(provider, model, usage, context, uncached);
        let Some(amount) = price.amount_usd.filter(|amount| amount.is_finite()) else {
            unknown.insert(model);
            continue;
        };
        let subtotal = amount * count as f64;
        total += subtotal;
        priced_count = priced_count.saturating_add(count);
        *provider_totals.entry(provider).or_default() += subtotal;
        *model_totals.entry(model).or_default() += subtotal;
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
        "claudeHistoryIncomplete": has_claude_history,
        "method": format!("API prices from models.dev; cached input rates are applied when reported.{}{}",
            if has_claude_history { " Earlier Claude totals can be incomplete because earlier responses may not have separate saved usage rows." } else { "" },
            if tier_used { " Published context tier applied where request size matched its threshold." }
            else { " Base rates used when request size was unavailable or below the published threshold." }),
    }))
}

struct ReconciliationUsageSnapshot {
    recorded: Vec<studio_runtime_store::cost_aggregate::RecordedResponse>,
    observations: Vec<studio_runtime_schema::usage::UsageObservation>,
    truncated: bool,
}

fn read_usage_snapshot(
    db: &rusqlite::Connection,
    agent: &str,
    limit: usize,
) -> Result<ReconciliationUsageSnapshot, CostsError> {
    let window =
        studio_runtime_store::cost_aggregate::reconciliation_usage_window(db, agent, limit)?;
    Ok(ReconciliationUsageSnapshot {
        recorded: window.responses,
        observations: window.observations,
        truncated: window.truncated,
    })
}

/// Compare the exact agent's stored response facts with native evidence. This never
/// imports history, repairs analytics, or contacts a provider.
pub fn usage_reconciliation(
    store: &RuntimeStore,
    agent_id: &str,
    accounts: &BTreeMap<String, Value>,
) -> Result<Value, CostsError> {
    use rusqlite::OptionalExtension;
    use studio_runtime_schema::usage::{UsageObservation, UsageScope};

    let repository = AnalyticsRepository::new(store.root());
    let mut db = repository.open_read_only()?;
    let tx = db.transaction()?;
    // Analytics preserves archived/deleted agents after the runtime row disappears.
    let raw: Option<String> = tx.query_row(
        "SELECT record FROM analytics_agents WHERE id=?1 UNION ALL SELECT record FROM canvas.runtime_agents WHERE id=?1 LIMIT 1",
        [agent_id], |row| row.get(0),
    ).optional()?;
    let raw = raw.ok_or(CostsError::Analytics(
        analytics::SessionCostError::InvalidRequest("Unknown chat"),
    ))?;
    let member: CostMember =
        serde_json::from_str(&raw).map_err(|_| CostsError::InvalidSessionState)?;
    let mut budget = EvidenceBudget::default();
    let snapshot = read_usage_snapshot(&tx, agent_id, MAX_RECONCILIATION_SOURCE_ROWS)?;
    let recorded = snapshot.recorded;
    budget.truncated |= snapshot.truncated;
    let mut gaps = BTreeMap::new();
    let mut provider_costs = BTreeMap::new();
    for observation in snapshot.observations {
        match observation {
            UsageObservation::Gap {
                ref response_id, ..
            } => {
                gaps.insert(response_id.clone(), observation);
            }
            UsageObservation::Cumulative {
                scope: UsageScope::ClaudeQuery,
                models: Some(models),
                ..
            } => {
                // These are cumulative observations: the latest per model wins, never sum them.
                for model in models {
                    if let Some(cost) = model.provider_cost_usd {
                        provider_costs.insert(model.model, cost);
                    }
                }
            }
            _ => {}
        }
    }
    tx.commit()?;

    let mut native = BTreeMap::<String, NativeResponse>::new();
    let members = vec![(agent_id.to_owned(), member.clone())];
    for row in read_local_claude_usage_inner(store.root(), &members, accounts, Some(&mut budget))? {
        let tokens = log_tokens(row.usage.tokens);
        if tokens.is_none() {
            budget.incomplete = true;
        }
        merge_native(
            &mut native,
            row.usage.id,
            NativeResponse {
                model: Some(row.usage.model),
                tokens,
                conflict: row.usage.conflicting_usage,
            },
        );
    }
    read_codex_evidence(&member, accounts, &mut native, &mut budget)?;
    if native.is_empty() {
        budget.incomplete = true;
    }
    let loaded = crate::pricing::load_pricing(store.root());
    let catalog = loaded.as_ref().and_then(|loaded| loaded.catalog.as_ref());
    let mut rows = BTreeMap::<
        String,
        (
            Option<studio_runtime_store::cost_aggregate::RecordedResponse>,
            Option<NativeResponse>,
        ),
    >::new();
    for row in recorded {
        gaps.remove(&row.response_id);
        match rows.entry(row.response_id.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((Some(row), None));
            }
            std::collections::btree_map::Entry::Occupied(mut entry) => {
                // A provisional replay cannot replace final evidence. Within final
                // observations retain the first, just like the response owner.
                if entry.get().0.as_ref().is_some_and(|existing| {
                    existing.evidence == studio_runtime_schema::usage::UsageEvidence::Legacy
                }) && row.evidence != studio_runtime_schema::usage::UsageEvidence::Legacy
                {
                    entry.get_mut().0 = Some(row);
                }
            }
        }
    }
    for (id, evidence) in native {
        if rows.len() >= MAX_RECONCILIATION_RESPONSES && !rows.contains_key(&id) {
            budget.truncated = true;
            continue;
        }
        rows.entry(id).or_insert((None, None)).1 = Some(evidence);
    }
    let mut missing_recorded = 0;
    let mut missing_evidence = 0;
    let mut mismatched = 0;
    let mut unknown_recorded = 0;
    let mut known_recorded = 0;
    let mut recorded_token_total = Some(studio_runtime_schema::usage::UsageTokens {
        cache_write_input_1h: Some(0),
        reasoning_output: Some(0),
        ..Default::default()
    });
    let mut evidence_token_total = Some(studio_runtime_schema::usage::UsageTokens {
        cache_write_input_1h: Some(0),
        reasoning_output: Some(0),
        ..Default::default()
    });
    let mut recorded_total = Some(0.0);
    let mut evidence_total = Some(0.0);
    let responses = rows.into_iter().map(|(id, (recorded, native))| {
        missing_recorded += usize::from(recorded.is_none());
        missing_evidence += usize::from(native.is_none());
        let recorded_tokens = recorded.as_ref().and_then(|row| row.tokens.as_ref());
        let evidence_tokens = native.as_ref().and_then(|row| row.tokens.as_ref());
        let recorded_model = recorded.as_ref().and_then(|row| row.model.as_deref());
        let evidence_model = native.as_ref().and_then(|row| row.model.as_deref());
        let conflict = native.as_ref().is_some_and(|row| row.conflict);
        let mismatch = recorded_tokens.is_some() && evidence_tokens.is_some()
            && (conflict || !same_billable_tokens(recorded_tokens, evidence_tokens) || recorded_model != evidence_model);
        mismatched += usize::from(mismatch);
        let recorded_cost = price_response(catalog, recorded_model, recorded_tokens);
        let evidence_cost = price_response(catalog, evidence_model, evidence_tokens);
        if recorded.is_some() {
            if recorded_tokens.is_some() {
                known_recorded += 1;
                recorded_token_total = add_tokens(recorded_token_total.take(), recorded_tokens);
                recorded_total = recorded_total.zip(recorded_cost).map(|(total, cost)| total + cost);
            } else { unknown_recorded += 1; }
        }
        if native.is_some() { evidence_token_total = add_tokens(evidence_token_total.take(), evidence_tokens); evidence_total = evidence_total.zip(evidence_cost).map(|(total, cost)| total + cost); }
        json!({"responseId": id, "model":recorded_model.or(evidence_model),
            "recordedTokens":recorded_tokens,"recordedEvidence":recorded.as_ref().map(|row| row.evidence),
            "evidenceTokens":evidence_tokens,"recordedCostUsd":recorded_cost,"evidenceCostUsd":evidence_cost,
            "journalConflict":conflict,"mismatched":mismatch})
    }).collect::<Vec<_>>();
    if known_recorded == 0 && unknown_recorded > 0 {
        recorded_total = None;
        recorded_token_total = None;
    }
    let unresolved_gaps = !gaps.is_empty();
    Ok(
        json!({"agentId":agent_id,"counts":{"responses":responses.len(),"missingRecorded":missing_recorded,
        "missingEvidence":missing_evidence,"mismatched":mismatched,"unknownRecorded":unknown_recorded,"rejectedEntries":budget.rejected},"responses":responses,
        "files":budget.files,"gaps":gaps.into_values().collect::<Vec<_>>(),"providerCosts":provider_costs.into_iter().map(|(model,cost)|json!({"model":model,"providerCostUsd":cost})).collect::<Vec<_>>(),
        "totals":{"recordedCostUsd":recorded_total,"evidenceCostUsd":evidence_total,"recordedTokens":recorded_token_total,"evidenceTokens":evidence_token_total},
        "incomplete":budget.incomplete || budget.truncated || missing_evidence > 0 || unresolved_gaps,"truncated":budget.truncated}),
    )
}

#[derive(Clone, Debug)]
struct NativeResponse {
    model: Option<String>,
    tokens: Option<studio_runtime_schema::usage::UsageTokens>,
    conflict: bool,
}

fn add_tokens(
    total: Option<studio_runtime_schema::usage::UsageTokens>,
    tokens: Option<&studio_runtime_schema::usage::UsageTokens>,
) -> Option<studio_runtime_schema::usage::UsageTokens> {
    let total = total?;
    let tokens = tokens?;
    Some(studio_runtime_schema::usage::UsageTokens {
        uncached_input: total.uncached_input.checked_add(tokens.uncached_input)?,
        cache_read_input: total
            .cache_read_input
            .checked_add(tokens.cache_read_input)?,
        cache_write_input: total
            .cache_write_input
            .checked_add(tokens.cache_write_input)?,
        // Contract v4: absence uses legacy pricing, so it contributes zero one-hour writes.
        cache_write_input_1h: Some(
            total
                .cache_write_input_1h
                .unwrap_or(0)
                .checked_add(tokens.cache_write_input_1h.unwrap_or(0))?,
        ),
        output: total.output.checked_add(tokens.output)?,
        reasoning_output: total
            .reasoning_output
            .zip(tokens.reasoning_output)
            .and_then(|(a, b)| a.checked_add(b)),
    })
}

fn same_billable_tokens(
    left: Option<&studio_runtime_schema::usage::UsageTokens>,
    right: Option<&studio_runtime_schema::usage::UsageTokens>,
) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => {
            left.uncached_input == right.uncached_input
                && left.cache_read_input == right.cache_read_input
                && left.cache_write_input == right.cache_write_input
                && left.cache_write_input_1h.unwrap_or(0) == right.cache_write_input_1h.unwrap_or(0)
                && left.output == right.output
                && left
                    .reasoning_output
                    .zip(right.reasoning_output)
                    .is_none_or(|(a, b)| a == b)
        }
        _ => false,
    }
}

fn merge_native(rows: &mut BTreeMap<String, NativeResponse>, id: String, row: NativeResponse) {
    if let Some(first) = rows.get_mut(&id) {
        first.conflict |= row.conflict
            || !same_billable_tokens(first.tokens.as_ref(), row.tokens.as_ref())
            || first.model != row.model;
    } else {
        rows.insert(id, row);
    }
}

fn log_tokens(tokens: CostTokens) -> Option<studio_runtime_schema::usage::UsageTokens> {
    let count = |number: Option<f64>| {
        number
            .filter(|n| {
                n.is_finite()
                    && *n >= 0.0
                    && n.fract() == 0.0
                    && *n <= studio_runtime_schema::shared_values::MAX_SAFE_INTEGER as f64
            })
            .map(|n| n as u64)
    };
    Some(studio_runtime_schema::usage::UsageTokens {
        uncached_input: count(tokens.input_tokens)?,
        cache_read_input: count(tokens.cached_input_tokens)?,
        cache_write_input: count(tokens.cache_write_input_tokens)?,
        cache_write_input_1h: tokens
            .cache_write_input_1h_tokens
            .and_then(|n| count(Some(n))),
        output: count(tokens.output_tokens)?,
        reasoning_output: None,
    })
}

fn price_response(
    catalog: Option<&crate::pricing::PricingCatalog>,
    model: Option<&str>,
    tokens: Option<&studio_runtime_schema::usage::UsageTokens>,
) -> Option<f64> {
    let model = model?;
    let tokens = tokens?;
    let usage = TokenUsage {
        input_tokens: Some(tokens.uncached_input as f64),
        cached_input_tokens: Some(tokens.cache_read_input as f64),
        cache_write_input_tokens: Some(tokens.cache_write_input as f64),
        cache_write_input_1h_tokens: tokens.cache_write_input_1h.map(|n| n as f64),
        output_tokens: Some(tokens.output as f64),
    };
    let context = tokens
        .uncached_input
        .checked_add(tokens.cache_read_input)?
        .checked_add(tokens.cache_write_input)?;
    catalog?
        .price_usage(
            provider_for(model)?,
            model,
            usage,
            Some(context as f64),
            true,
        )
        .amount_usd
        .filter(|cost| cost.is_finite())
}

fn read_codex_evidence(
    member: &CostMember,
    accounts: &BTreeMap<String, Value>,
    rows: &mut BTreeMap<String, NativeResponse>,
    budget: &mut EvidenceBudget,
) -> Result<(), CostsError> {
    use crate::analytics_history::RolloutPath;
    let mut sessions = BTreeSet::new();
    if let Some(thread) = &member.thread_id {
        sessions.insert((
            member
                .account_key
                .as_deref()
                .unwrap_or(DEFAULT_ACCOUNT)
                .to_owned(),
            thread.clone(),
        ));
    }
    for entry in member.account_history.iter().flatten() {
        if let (Some(account), Some(thread)) = (&entry.account_key, &entry.thread_id) {
            sessions.insert((account.clone(), thread.clone()));
        }
    }
    for (account, thread) in sessions {
        if !budget.visit() {
            break;
        }
        let Some(profile) = accounts
            .get(&account)
            .filter(|account| account["provider"] == "codex")
            .and_then(|account| account["home"].as_str())
        else {
            continue;
        };
        let profile = expand_home(PathBuf::from(profile));
        let remaining_entries = MAX_EVIDENCE_ENTRIES.saturating_sub(budget.entries);
        let discovered = budget.discovery.resolve_with_entry_budget(
            &profile,
            &thread,
            &std::sync::atomic::AtomicBool::new(false),
            remaining_entries,
        );
        budget.entries += discovered.examined;
        for rejected in discovered.rejections {
            budget.reject(
                &rejected.path,
                EvidenceFileKind::CodexRollout,
                rejected.reason,
            );
        }
        let path = match discovered.path {
            Ok(RolloutPath::Found(path)) => path,
            Err(
                crate::analytics_history::HistoryError::DiscoveryLimit
                | crate::analytics_history::HistoryError::DiscoveryRoundLimit,
            ) => {
                budget.report(&profile, EvidenceFileKind::CodexRollout, true, true);
                continue;
            }
            _ => {
                budget.report(&profile, EvidenceFileKind::CodexRollout, true, false);
                continue;
            }
        };
        let Some(cached) = budget.load(
            &profile,
            &path,
            EvidenceFileKind::CodexRollout,
            Some(&thread),
        ) else {
            continue;
        };
        let Some(NativeFileData::Codex(evidence)) = &cached.data else {
            continue;
        };
        for (id, response) in evidence {
            merge_native(rows, id.clone(), response.clone());
        }
    }
    Ok(())
}

fn parse_codex_evidence(
    bytes: &[u8],
    thread: &str,
    limit: usize,
) -> (Vec<(String, NativeResponse)>, bool, bool) {
    use crate::analytics_rollout::{RolloutAction, RolloutContext};
    use studio_runtime_schema::usage::{UsageObservation, UsageProvider, parse_token_usage};
    let Ok(mut context) = RolloutContext::from_record(json!({"threadId":thread})) else {
        return (Vec::new(), true, false);
    };
    let mut rows = BTreeMap::new();
    let mut incomplete = false;
    let mut truncated = false;
    for (index, line) in bytes
        .split(|byte| *byte == b'\n')
        .filter(|line| !line.is_empty())
        .enumerate()
    {
        let Ok(record) = serde_json::from_slice::<Value>(line) else {
            incomplete = true;
            continue;
        };
        let actions = match context.actions(&record, &format!("diagnostic:{index}"), 0.0) {
            Ok(actions) => actions,
            Err(_) => {
                incomplete = true;
                continue;
            }
        };
        for action in actions {
            match action {
                RolloutAction::Event { method, params, .. }
                    if method == "thread/tokenUsage/updated" =>
                {
                    match parse_token_usage(UsageProvider::Codex, &params) {
                        Ok(UsageObservation::Response {
                            response_id,
                            model,
                            tokens,
                            ..
                        }) => {
                            if rows.len() == limit && !rows.contains_key(&response_id) {
                                truncated = true;
                                continue;
                            }
                            #[cfg(test)]
                            if !rows.contains_key(&response_id) {
                                EVIDENCE_RESPONSES_MATERIALIZED
                                    .with(|count| count.set(count.get() + 1));
                            }
                            merge_native(
                                &mut rows,
                                response_id,
                                NativeResponse {
                                    model: Some(model),
                                    tokens: Some(tokens),
                                    conflict: false,
                                },
                            );
                        }
                        Ok(_) => {}
                        Err(_) => incomplete = true,
                    }
                }
                RolloutAction::WrongThread => incomplete = true,
                _ => {}
            }
        }
    }
    (rows.into_iter().collect(), incomplete, truncated)
}

// Session identifier and state-file byte caps for local cost scans.
const MAX_SESSION_ID_BYTES: usize = 100;

/// One assistant response of a Claude session log.
#[derive(Clone, Debug)]
struct LogUsage {
    id: String,
    model: String,
    /// Input counts are all known or all `None`; known input is uncached.
    tokens: CostTokens,
    /// A later journal line disagreed with the retained first value.
    conflicting_usage: bool,
}

#[derive(Clone, Debug)]
struct LocalClaudeUsage {
    account_key: String,
    thread_id: String,
    usage: LogUsage,
}

/// Usage rows per session log, parsed again only after the log changed.
static CLAUDE_LOGS: LazyLock<FileDerived<Vec<LogUsage>>> = LazyLock::new(FileDerived::default);
/// Native session ids per Studio session state file, read again only after it changed.
static SESSION_NATIVE_IDS: LazyLock<FileDerived<Result<Vec<String>, ()>>> =
    LazyLock::new(FileDerived::default);

/// Maximum distinct responses in one diagnostic comparison (unit: responses).
const MAX_RECONCILIATION_RESPONSES: usize = 10_000;
/// Maximum indexed SQLite source rows examined per comparison, including a
/// single lookahead row used to report truncation (unit: source rows).
const MAX_RECONCILIATION_SOURCE_ROWS: usize = 10_000;
/// Maximum native bytes read across one comparison, including session state (unit: bytes).
pub const MAX_RECONCILIATION_NATIVE_BYTES: u64 = 64 * 1024 * 1024;
/// Maximum filesystem entries/files examined across one comparison (unit: entries).
const MAX_EVIDENCE_ENTRIES: usize = 10_000;
/// Retained bounded native derivations per process (unit: file/context entries).
const EVIDENCE_CACHE_ENTRIES: usize = 32;

#[derive(Clone, Copy, Debug, serde::Serialize)]
#[serde(rename_all = "camelCase")]
enum EvidenceFileKind {
    ClaudeState,
    ClaudeJournal,
    CodexRollout,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct EvidenceFileReport {
    file: String,
    kind: EvidenceFileKind,
    incomplete: bool,
    truncated: bool,
    reason: Option<&'static str>,
}

enum NativeFileData {
    State(Vec<String>),
    Claude(Vec<LogUsage>),
    Codex(Vec<(String, NativeResponse)>),
}
struct CachedNativeFile {
    data: Option<NativeFileData>,
    bytes: u64,
    incomplete: bool,
    truncated: bool,
}
static NATIVE_EVIDENCE: LazyLock<FileDerived<CachedNativeFile>> =
    LazyLock::new(|| FileDerived::with_capacity(EVIDENCE_CACHE_ENTRIES));

#[derive(Default)]
struct EvidenceBudget {
    entries: usize,
    bytes: u64,
    responses: usize,
    incomplete: bool,
    truncated: bool,
    files: Vec<EvidenceFileReport>,
    rejected: usize,
    discovery: crate::analytics_history::DiscoveryRound,
    roots: BTreeMap<PathBuf, Option<std::sync::Arc<crate::analytics_history::NativeEvidenceRoot>>>,
}
#[cfg(test)]
thread_local! {
    static EVIDENCE_FILE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static EVIDENCE_RESPONSES_MATERIALIZED: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl EvidenceBudget {
    fn root(
        &mut self,
        root: &Path,
    ) -> Option<std::sync::Arc<crate::analytics_history::NativeEvidenceRoot>> {
        self.roots
            .entry(root.to_owned())
            .or_insert_with(|| {
                crate::analytics_history::NativeEvidenceRoot::new(root)
                    .ok()
                    .map(std::sync::Arc::new)
            })
            .clone()
    }
    fn visit(&mut self) -> bool {
        if self.entries >= MAX_EVIDENCE_ENTRIES {
            self.truncated = true;
            return false;
        }
        self.entries += 1;
        true
    }
    fn report(&mut self, path: &Path, kind: EvidenceFileKind, incomplete: bool, truncated: bool) {
        self.incomplete |= incomplete;
        self.truncated |= truncated;
        self.files.push(EvidenceFileReport {
            file: path
                .file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned(),
            kind,
            incomplete,
            truncated,
            reason: if truncated {
                Some("Native evidence exceeded the request allowance")
            } else if incomplete {
                Some("Native evidence is unavailable or incomplete")
            } else {
                None
            },
        });
    }
    fn reject(&mut self, path: &Path, kind: EvidenceFileKind, reason: &'static str) {
        self.rejected += 1;
        self.report(path, kind, true, false);
        if let Some(report) = self.files.last_mut() {
            report.reason = Some(reason);
        }
    }
    fn load(
        &mut self,
        root: &Path,
        path: &Path,
        kind: EvidenceFileKind,
        thread: Option<&str>,
    ) -> Option<std::sync::Arc<CachedNativeFile>> {
        if !self.visit() {
            return None;
        }
        // Missing optional aliases/session files are not failed evidence reads.
        if path.try_exists().is_ok_and(|exists| !exists) {
            return None;
        }
        let limit = MAX_RECONCILIATION_NATIVE_BYTES.saturating_sub(self.bytes);
        let remaining = MAX_RECONCILIATION_RESPONSES.saturating_sub(self.responses);
        if limit == 0 || (remaining == 0 && !matches!(kind, EvidenceFileKind::ClaudeState)) {
            self.report(path, kind, true, true);
            return None;
        }
        // The importer owns confinement/open/regular-file validation. It runs on
        // every lookup, including hits, so a changed symlink cannot bypass it.
        let Some(anchor) = self.root(root) else {
            self.reject(path, kind, "Configured native root is unavailable");
            return None;
        };
        let opened = match anchor.open(path) {
            Ok(opened) => opened,
            Err(error) => {
                self.reject(path, kind, error.reason());
                return None;
            }
        };
        let context = format!("{kind:?}:{thread:?}:{limit}:{remaining}");
        let cached =
            NATIVE_EVIDENCE.get_or_load_opened(&opened.path, &opened.metadata, context, || {
                if matches!(kind, EvidenceFileKind::ClaudeState) && opened.metadata.len() > limit {
                    return CachedNativeFile {
                        data: None,
                        bytes: 0,
                        incomplete: true,
                        truncated: true,
                    };
                }
                #[cfg(test)]
                EVIDENCE_FILE_READS.with(|reads| reads.set(reads.get() + 1));
                let mut bytes = Vec::new();
                let size = opened.metadata.len().min(limit);
                let read_error = opened.file.take(size).read_to_end(&mut bytes).is_err();
                let charged = bytes.len() as u64;
                let mut incomplete = read_error || charged != size;
                let mut truncated = opened.metadata.len() > limit;
                // Journals/rollouts can retain complete lines from a bounded prefix.
                // A partial state JSON cannot authorize additional native aliases.
                if truncated && !matches!(kind, EvidenceFileKind::ClaudeState) {
                    if let Some(end) = bytes.iter().rposition(|byte| *byte == b'\n') {
                        bytes.truncate(end + 1);
                    } else {
                        bytes.clear();
                    }
                }
                let data = match kind {
                    EvidenceFileKind::ClaudeState => {
                        if truncated {
                            incomplete = true;
                            None
                        } else {
                            match parse_session_native_ids(bytes.as_slice()) {
                                Ok(mut ids) => {
                                    truncated |= ids.len() > MAX_EVIDENCE_ENTRIES;
                                    ids.truncate(MAX_EVIDENCE_ENTRIES);
                                    Some(NativeFileData::State(ids))
                                }
                                Err(()) => {
                                    incomplete = true;
                                    None
                                }
                            }
                        }
                    }
                    EvidenceFileKind::ClaudeJournal => {
                        let (rows, bad_lines, limited) =
                            parse_claude_log_reader_limited(std::io::Cursor::new(bytes), remaining);
                        incomplete |= bad_lines;
                        truncated |= limited;
                        Some(NativeFileData::Claude(rows))
                    }
                    EvidenceFileKind::CodexRollout => {
                        let header = bytes
                            .split(|byte| *byte == b'\n')
                            .next()
                            .unwrap_or_default();
                        if !crate::analytics_history::native_rollout_header_matches(
                            header,
                            thread.unwrap_or_default(),
                        ) {
                            incomplete = true;
                            None
                        } else {
                            let (rows, bad_lines, limited) =
                                parse_codex_evidence(&bytes, thread.unwrap_or_default(), remaining);
                            incomplete |= bad_lines;
                            truncated |= limited;
                            Some(NativeFileData::Codex(rows))
                        }
                    }
                };
                CachedNativeFile {
                    data,
                    bytes: charged,
                    incomplete: incomplete || truncated,
                    truncated,
                }
            });
        // Cached prefixes consume exactly the same request allowance as a cold
        // read. A hit can never make a request exceed its coverage bound.
        self.bytes += cached.bytes;
        self.responses += match &cached.data {
            Some(NativeFileData::Claude(rows)) => rows.len(),
            Some(NativeFileData::Codex(rows)) => rows.len(),
            _ => 0,
        };
        self.report(path, kind, cached.incomplete, cached.truncated);
        Some(cached)
    }
    #[cfg(test)]
    fn read(&mut self, path: &Path) -> Option<Vec<u8>> {
        let cached = self.load(path.parent()?, path, EvidenceFileKind::ClaudeJournal, None)?;
        cached.data.as_ref()?;
        Some(Vec::new())
    }
}

fn read_local_claude_usage(
    state_root: &Path,
    members: &[(String, CostMember)],
    accounts: &BTreeMap<String, Value>,
) -> Result<Vec<LocalClaudeUsage>, CostsError> {
    read_local_claude_usage_inner(state_root, members, accounts, None)
}

fn read_local_claude_usage_inner(
    state_root: &Path,
    members: &[(String, CostMember)],
    accounts: &BTreeMap<String, Value>,
    mut budget: Option<&mut EvidenceBudget>,
) -> Result<Vec<LocalClaudeUsage>, CostsError> {
    let mut sessions = BTreeSet::new();
    for (_, member) in members {
        let current_key = member.account_key.as_deref().unwrap_or(DEFAULT_ACCOUNT);
        if account_claude_config(current_key, accounts).is_some()
            && let Some(thread_id) = &member.thread_id
        {
            sessions.insert((current_key.to_owned(), thread_id.clone()));
        }
        for entry in member.account_history.iter().flatten() {
            if entry.provider.as_deref() != Some("claude") {
                continue;
            }
            let (Some(account_key), Some(thread_id)) = (&entry.account_key, &entry.thread_id)
            else {
                continue;
            };
            if account_claude_config(account_key, accounts).is_some() {
                sessions.insert((account_key.clone(), thread_id.clone()));
            }
        }
    }

    let mut rows = BTreeMap::new();
    for (account_key, thread_id) in sessions {
        if let Some(budget) = budget.as_deref_mut()
            && !budget.visit()
        {
            break;
        }
        if !valid_session_id(&account_key) || !valid_session_id(&thread_id) {
            continue;
        }
        let Some(config) = account_claude_config(&account_key, accounts) else {
            continue;
        };
        let mut ids = BTreeSet::from([thread_id.clone()]);
        let server_root = crate::account_driver::account_server_root(state_root, &account_key);
        let state_path = server_root
            .join("sessions")
            .join(format!("{thread_id}.json"));
        if let Some(budget) = budget.as_deref_mut() {
            if let Some(cached) =
                budget.load(state_root, &state_path, EvidenceFileKind::ClaudeState, None)
                && let Some(NativeFileData::State(native)) = &cached.data
            {
                ids.extend(native.iter().cloned());
            }
        } else if let Some(native) =
            SESSION_NATIVE_IDS.get_or_load(&state_path, read_session_native_ids)
        {
            let native = native
                .as_ref()
                .as_ref()
                .map_err(|()| CostsError::InvalidSessionState)?;
            ids.extend(native.iter().cloned());
        }
        let projects = config.join("projects");
        if let Some(budget) = budget.as_deref_mut() {
            let Some(anchor) = budget.root(&config) else {
                budget.report(&projects, EvidenceFileKind::ClaudeJournal, true, false);
                continue;
            };
            if !budget.visit() {
                break;
            }
            if let Err(error) = anchor.directory(&projects) {
                budget.reject(&projects, EvidenceFileKind::ClaudeJournal, error.reason());
                continue;
            }
        }
        let project_entries = match fs::read_dir(&projects) {
            Ok(entries) => entries,
            Err(_) => {
                if let Some(budget) = budget.as_deref_mut() {
                    budget.report(&projects, EvidenceFileKind::ClaudeJournal, true, false);
                }
                continue;
            }
        };
        for entry in project_entries {
            if let Some(budget) = budget.as_deref_mut()
                && !budget.visit()
            {
                break;
            }
            let project = match entry {
                Ok(project) => project,
                Err(_) => {
                    if let Some(budget) = budget.as_deref_mut() {
                        budget.report(&projects, EvidenceFileKind::ClaudeJournal, true, false);
                    }
                    continue;
                }
            };
            let project_path = project.path();
            if let Some(budget) = budget.as_deref_mut() {
                let Some(anchor) = budget.root(&config) else {
                    budget.report(&project_path, EvidenceFileKind::ClaudeJournal, true, false);
                    continue;
                };
                if let Err(error) = anchor.directory(&project_path) {
                    budget.reject(
                        &project_path,
                        EvidenceFileKind::ClaudeJournal,
                        error.reason(),
                    );
                    continue;
                }
            }
            match project.file_type() {
                Ok(kind) if kind.is_dir() => {}
                Ok(_) => continue,
                Err(_) => {
                    if let Some(budget) = budget.as_deref_mut() {
                        budget.report(&project_path, EvidenceFileKind::ClaudeJournal, true, false);
                    }
                    continue;
                }
            }
            for id in &ids {
                let path = project_path.join(format!("{id}.jsonl"));
                let log = if let Some(budget) = budget.as_deref_mut() {
                    let Some(cached) =
                        budget.load(&config, &path, EvidenceFileKind::ClaudeJournal, None)
                    else {
                        continue;
                    };
                    let Some(NativeFileData::Claude(rows)) = &cached.data else {
                        continue;
                    };
                    std::sync::Arc::new(rows.clone())
                } else {
                    let Some(log) = CLAUDE_LOGS.get_or_load(&path, parse_claude_log) else {
                        continue;
                    };
                    log
                };
                for usage in log.iter() {
                    if rows.len() >= MAX_RECONCILIATION_RESPONSES && budget.is_some() {
                        if let Some(budget) = budget.as_deref_mut() {
                            budget.truncated = true;
                        }
                        break;
                    }
                    rows.entry((account_key.clone(), thread_id.clone(), usage.id.clone()))
                        .and_modify(|first: &mut LocalClaudeUsage| {
                            first.usage.conflicting_usage |= usage.conflicting_usage
                                || first.usage.tokens != usage.tokens
                                || first.usage.model != usage.model;
                        })
                        .or_insert_with(|| LocalClaudeUsage {
                            account_key: account_key.clone(),
                            thread_id: thread_id.clone(),
                            usage: usage.clone(),
                        });
                }
            }
        }
    }
    Ok(rows.into_values().collect())
}

fn account_claude_config(account_key: &str, accounts: &BTreeMap<String, Value>) -> Option<PathBuf> {
    let account = accounts.get(account_key)?;
    if account.get("provider").and_then(Value::as_str) != Some("claude") {
        return None;
    }
    let config_dir = account
        .get("claudeOptions")
        .and_then(|options| options.get("configDir"))
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    let home = account
        .get("home")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty());
    let path = config_dir
        .or(home)
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| PathBuf::from(home).join(".claude")))?;
    Some(expand_home(path))
}

fn expand_home(path: PathBuf) -> PathBuf {
    if path == Path::new("~") {
        return std::env::var_os("HOME").map(PathBuf::from).unwrap_or(path);
    }
    if let Ok(suffix) = path.strip_prefix("~/") {
        return std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join(suffix))
            .unwrap_or(path);
    }
    path
}

fn valid_session_id(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_SESSION_ID_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// The snapshot format of the Claude bridge session store. The bridge owns it, so it
/// is read from the bridge source instead of being copied here.
fn claude_session_format() -> &'static str {
    static FORMAT: LazyLock<&'static str> = LazyLock::new(|| {
        include_str!("../../../../providers/apps/claude-bridge/session-store.ts")
            .split_once("const FORMAT = \"")
            .and_then(|(_, rest)| rest.split_once('"'))
            .map(|(format, _)| format)
            .expect("Claude session store format")
    });
    *FORMAT
}

/// The native ids a Studio session state file names. Studio writes this file, so a
/// file of another shape is an error, not something to skip.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionState {
    native_id: Option<String>,
    #[serde(default)]
    history_branches: Vec<NativeIdOf>,
    #[serde(default)]
    control_requests: BTreeMap<String, ControlRequest>,
}

/// A saved session file: the bridge snapshot `{format, sequence, session}`, or the
/// earlier file that is the session object itself.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct SessionFile {
    format: Option<String>,
    session: Option<SessionState>,
    native_id: Option<String>,
    #[serde(default)]
    history_branches: Vec<NativeIdOf>,
    #[serde(default)]
    control_requests: BTreeMap<String, ControlRequest>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct NativeIdOf {
    native_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ControlRequest {
    source_native_id: Option<String>,
    target_native_id: Option<String>,
}

/// The valid native ids of one session state file; `Err` when the file does not decode.
/// The file holds every turn, so it is read as a stream: the turns are skipped, not kept.
fn read_session_native_ids(path: &Path) -> Option<Result<Vec<String>, ()>> {
    let file = File::open(path).ok()?;
    Some(parse_session_native_ids(BufReader::with_capacity(
        1 << 16,
        file,
    )))
}

fn parse_session_native_ids(reader: impl Read) -> Result<Vec<String>, ()> {
    let Ok(saved) = serde_json::from_reader::<_, SessionFile>(reader) else {
        return Err(());
    };
    let state = match saved.format {
        Some(format) if format == claude_session_format() => match saved.session {
            Some(session) => session,
            None => return Err(()),
        },
        Some(_) => return Err(()),
        None => SessionState {
            native_id: saved.native_id,
            history_branches: saved.history_branches,
            control_requests: saved.control_requests,
        },
    };
    let ids = std::iter::once(state.native_id)
        .chain(
            state
                .history_branches
                .into_iter()
                .map(|branch| branch.native_id),
        )
        .chain(
            state
                .control_requests
                .into_values()
                .flat_map(|control| [control.source_native_id, control.target_native_id]),
        )
        .flatten()
        .filter(|id| valid_session_id(id))
        .collect::<BTreeSet<_>>();
    Ok(ids.into_iter().collect())
}

/// Just the line kind: Claude writes many kinds of line and only assistant responses matter.
#[derive(Deserialize)]
struct LogLineKind<'a> {
    #[serde(rename = "type", borrow)]
    kind: Option<&'a str>,
}

/// An assistant line of a Claude session log (a format Claude owns, not Studio).
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct AssistantLine {
    timestamp: Option<String>,
    request_id: Option<String>,
    uuid: Option<String>,
    message: Option<LogMessage>,
}

#[derive(Deserialize)]
struct LogMessage {
    id: Option<String>,
    model: Option<String>,
    usage: Option<LogTokens>,
}

#[derive(Deserialize)]
struct LogTokens {
    input_tokens: Option<f64>,
    cache_read_input_tokens: Option<f64>,
    cache_creation_input_tokens: Option<f64>,
    cache_creation: Option<LogCacheCreation>,
    output_tokens: Option<f64>,
}

#[derive(Deserialize)]
struct LogCacheCreation {
    ephemeral_1h_input_tokens: Option<f64>,
}

/// Session log lines that did not decode and were skipped, since process start. A line
/// is skipped whole; surfaced in diagnostics as `claudeLogLinesSkipped`.
static CLAUDE_LOG_LINES_SKIPPED: AtomicU64 = AtomicU64::new(0);

/// Count of Claude session log lines skipped because they did not decode.
pub fn claude_log_lines_skipped() -> u64 {
    CLAUDE_LOG_LINES_SKIPPED.load(Ordering::Relaxed)
}

/// The first usage row of every response id in one session log, in file order.
fn parse_claude_log(path: &Path) -> Option<Vec<LogUsage>> {
    let file = File::open(path).ok()?;
    Some(parse_claude_log_reader(BufReader::new(file)).0)
}

fn parse_claude_log_reader(reader: impl BufRead) -> (Vec<LogUsage>, bool) {
    let (rows, incomplete, _) = parse_claude_log_reader_limited(reader, usize::MAX);
    (rows, incomplete)
}

fn parse_claude_log_reader_limited(
    mut reader: impl BufRead,
    limit: usize,
) -> (Vec<LogUsage>, bool, bool) {
    let mut truncated = false;
    let mut incomplete = false;
    let mut rows: Vec<LogUsage> = Vec::new();
    let mut seen = BTreeMap::<String, usize>::new();
    let mut line = Vec::new();
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) => break,
            Err(_) => {
                incomplete = true;
                break;
            }
            Ok(_) => {}
        }
        let mut skipped = || {
            incomplete = true;
            CLAUDE_LOG_LINES_SKIPPED.fetch_add(1, Ordering::Relaxed);
        };
        match serde_json::from_slice::<LogLineKind>(&line) {
            Ok(LogLineKind {
                kind: Some("assistant"),
            }) => {}
            Ok(_) => continue,
            Err(_) => {
                skipped();
                continue;
            }
        }
        let Ok(row) = serde_json::from_slice::<AssistantLine>(&line) else {
            skipped();
            continue;
        };
        let Some(message) = row.message else {
            continue;
        };
        let Some(model) = message
            .model
            .filter(|model| !model.is_empty() && model != "<synthetic>")
        else {
            continue;
        };
        if !valid_claude_timestamp(row.timestamp.as_deref()) {
            continue;
        }
        let Some(usage) = message.usage else {
            continue;
        };
        let Some(id) = [message.id, row.request_id, row.uuid]
            .into_iter()
            .flatten()
            .find(|id| !id.is_empty())
        else {
            continue;
        };
        // An absent count is zero; a negative count voids all three input counts.
        let count = |value: Option<f64>| match value {
            None => Some(0.0),
            Some(number) => (number >= 0.0).then_some(number),
        };
        let input_values = [
            count(usage.input_tokens),
            count(usage.cache_read_input_tokens),
            count(usage.cache_creation_input_tokens),
        ];
        let (input_tokens, cached_tokens, write_tokens) =
            if input_values.iter().all(Option::is_some) {
                (input_values[0], input_values[1], input_values[2])
            } else {
                (None, None, None)
            };
        let tokens = CostTokens {
            input_tokens,
            cached_input_tokens: cached_tokens,
            cache_write_input_tokens: write_tokens,
            cache_write_input_1h_tokens: write_tokens
                .zip(count(
                    usage
                        .cache_creation
                        .and_then(|split| split.ephemeral_1h_input_tokens),
                ))
                .map(|(write, hour)| hour.min(write)),
            output_tokens: usage.output_tokens,
        };
        if let Some(&index) = seen.get(&id) {
            rows[index].conflicting_usage |=
                rows[index].tokens != tokens || rows[index].model != model;
            continue;
        }
        if rows.len() == limit {
            truncated = true;
            continue;
        }
        seen.insert(id.clone(), rows.len());
        #[cfg(test)]
        EVIDENCE_RESPONSES_MATERIALIZED.with(|count| count.set(count.get() + 1));
        rows.push(LogUsage {
            id,
            model,
            tokens,
            conflicting_usage: false,
        });
    }
    (rows, incomplete, truncated)
}

fn valid_claude_timestamp(value: Option<&str>) -> bool {
    value.is_some_and(|value| {
        chrono::DateTime::parse_from_rfc3339(value).is_ok()
            || chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S%.f").is_ok()
            || chrono::NaiveDateTime::parse_from_str(value, "%Y-%m-%d %H:%M:%S%.f").is_ok()
            || chrono::NaiveDate::parse_from_str(value, "%Y-%m-%d").is_ok()
    })
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

#[cfg(test)]
mod tests {
    use super::{
        CostsError, read_local_claude_usage, session_cost, session_cost_with_account_profiles,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::fs;
    use studio_runtime_store::analytics_capture::{AnalyticsCaptureRepository, CaptureTable};
    use studio_runtime_store::cost_aggregate::{CostMember, CostTokens};
    use studio_runtime_store::{RuntimeStore, StoreError};
    use tempfile::tempdir;

    fn member(record: Value) -> CostMember {
        serde_json::from_value(record).expect("agent cost members")
    }

    /// The skipped line count is one process-wide counter, and tests run in parallel
    /// threads. Tests that assert on it hold this lock, so they cannot count each other.
    static SKIPPED_LINE_COUNTER: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn hold_skipped_line_counter() -> std::sync::MutexGuard<'static, ()> {
        SKIPPED_LINE_COUNTER
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    #[tokio::test]
    async fn usage_reconciliation_cache_write_1h_mixed_legacy_totals() {
        let temp = tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_,StoreError>(|tx,_| {
            AnalyticsCaptureRepository::write(tx,CaptureTable::Agents,&json!({"id":"agent","rootId":"agent","accountKey":"claude","provider":"claude"}))?;
            for (id, hour) in [("legacy",None),("v4",Some(10))] {
                let mut tokens = json!({"uncachedInput":0,"cacheReadInput":0,"cacheWriteInput":10,"output":0});
                if let Some(hour) = hour { tokens["cacheWriteInput1h"] = json!(hour); }
                AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&json!({"id":id,"agentId":"agent","rootId":"agent","at":1,"last":{},"total":{},
                    "studioUsage":{"kind":"response","responseId":id,"model":"claude-test","evidence":"live","tokens":tokens}}))?;
            }
            Ok(())
        }).await.expect("mixed responses");
        let result = super::usage_reconciliation(&store, "agent", &Default::default())
            .expect("public reconciliation");
        assert_eq!(result["totals"]["recordedTokens"]["cacheWriteInput"], 20);
        assert_eq!(result["totals"]["recordedTokens"]["cacheWriteInput1h"], 10);
    }

    #[tokio::test]
    async fn usage_reconciliation_cache_write_1h_saved_run_session_cost() {
        let temp = tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        let runs: Value = serde_json::from_str(include_str!(
            "../../runtime-schema/src/fixtures/claude-cache-write-1h.json"
        ))
        .expect("native counts");
        fs::create_dir_all(temp.path().join("model-pricing")).expect("pricing directory");
        fs::write(temp.path().join("model-pricing/models-dev-v1.json"),json!({"version":1,"catalog":{"providers":{
            "openai":{"models":{"gpt":{"cost":{"input":1,"output":1}}}},
            "anthropic":{"models":{"claude-haiku-5-5":{"cost":{"input":0.1,"output":0.5,"cache_read":0.01,"cache_write":0.125}}}}
        }}}).to_string()).expect("pricing");
        let run = runs[0].clone();
        store.analytics_transaction::<_,StoreError>(move |tx,_| {
            AnalyticsCaptureRepository::write(tx,CaptureTable::Agents,&json!({"id":"agent","rootId":"agent","accountKey":"claude","provider":"claude"}))?;
            for response in run["responses"].as_array().expect("responses") {
                let raw = &response["rawUsage"];
                AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&json!({"id":response["responseId"],"agentId":"agent","rootId":"agent","at":1,"accountKey":"claude","last":{},"total":{},
                    "studioUsage":{"kind":"response","responseId":response["responseId"],"model":response["model"],"evidence":"live","tokens":{
                        "uncachedInput":raw["input_tokens"],"cacheReadInput":raw["cache_read_input_tokens"],"cacheWriteInput":raw["cache_creation_input_tokens"],"cacheWriteInput1h":raw["cache_creation"]["ephemeral_1h_input_tokens"],"output":raw["output_tokens"]}}}))?;
            }
            Ok(())
        }).await.expect("persist native counts");
        let cost = session_cost_with_account_profiles(&store, "agent", &Default::default())
            .expect("session cost");
        assert_eq!(cost["pricedSamples"], 4);
        assert!((cost["totalUSD"].as_f64().expect("price") - 0.00515774).abs() < 1e-12);
        let evidence = super::usage_reconciliation(&store, "agent", &Default::default())
            .expect("reconciliation");
        assert!(
            (evidence["totals"]["recordedCostUsd"]
                .as_f64()
                .expect("price")
                - 0.00515774)
                .abs()
                < 1e-12
        );
    }

    #[test]
    fn usage_reconciliation_cache_write_1h_journal_and_price() {
        let raw = json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z","message":{"id":"r","model":"claude-test","usage":{"input_tokens":2,"cache_read_input_tokens":3,"cache_creation_input_tokens":10,"cache_creation":{"ephemeral_1h_input_tokens":99},"output_tokens":4}}});
        let (rows, incomplete) =
            super::parse_claude_log_reader(std::io::Cursor::new(raw.to_string()));
        assert!(!incomplete);
        let tokens = super::log_tokens(rows[0].tokens).expect("journal tokens");
        assert_eq!(
            serde_json::to_value(&tokens).expect("tokens")["cacheWriteInput1h"],
            10
        );
        let catalog = crate::pricing::PricingCatalog::from_value(&json!({"providers":{
            "openai":{"models":{"gpt":{"cost":{"input":1,"output":1}}}},
            "anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":2,"cache_read":0.5,"cache_write":1.25}}}}
        }})).expect("catalog");
        assert_eq!(
            super::price_response(Some(&catalog), Some("claude-test"), Some(&tokens)),
            Some(31.5 / 1_000_000.0)
        );
    }

    fn reconciliation_journal_fixture(
        root: &std::path::Path,
    ) -> (Vec<(String, CostMember)>, BTreeMap<String, Value>) {
        let profile = root.join("profile");
        fs::create_dir_all(profile.join("projects/project")).expect("project");
        fs::write(profile.join("projects/project/thread.jsonl"),
            json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z","message":{"id":"response","model":"claude-sonnet-4-5","usage":{"input_tokens":10,"output_tokens":2}}}).to_string()).expect("journal");
        (
            vec![(
                "agent".into(),
                member(json!({"accountKey":"claude","provider":"claude","threadId":"thread"})),
            )],
            BTreeMap::from([(
                "claude".into(),
                json!({"provider":"claude","claudeOptions":{"configDir":profile}}),
            )]),
        )
    }

    fn reconciliation_codex_fixture(
        root: &std::path::Path,
    ) -> (CostMember, BTreeMap<String, Value>) {
        fs::create_dir_all(root.join("sessions")).expect("sessions");
        let mut lines = vec![
            json!({"type":"session_meta","payload":{"id":"thread"}}),
            json!({"type":"turn_context","payload":{"model":"gpt-test"}}),
        ];
        for id in ["one", "two", "three"] {
            lines.push(json!({"type":"token_usage_record","payload":{"response_id":id,"usage":{"input_tokens":10,"output_tokens":2}}}));
        }
        lines.push(json!({"type":"token_usage_record","payload":{"response_id":"one","usage":{"input_tokens":10,"output_tokens":9}}}));
        fs::write(
            root.join("sessions/rollout-test-thread.jsonl"),
            lines
                .iter()
                .map(Value::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .expect("rollout");
        (
            member(json!({"accountKey":"codex","threadId":"thread"})),
            BTreeMap::from([("codex".into(), json!({"provider":"codex","home":root}))]),
        )
    }

    #[cfg(unix)]
    #[test]
    fn usage_reconciliation_sql_examination_is_bounded_before_kind_filtering() {
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };
        let measure = |count: usize| {
            let mut db = rusqlite::Connection::open_in_memory().expect("analytics");
            db.execute_batch("CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,agent TEXT NOT NULL,at REAL NOT NULL,record TEXT NOT NULL); CREATE INDEX analytics_usage_migration ON analytics_usage(agent,seq); CREATE INDEX analytics_usage_scope ON analytics_usage(agent,at);").expect("usage source");
            let tx = db.transaction().expect("seed transaction");
            for index in 0..count {
                let record = json!({"studioUsage":{"kind":"response","responseId":format!("r{index}"),"model":"gpt-test","evidence":"live","tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}}});
                tx.execute(
                    "INSERT INTO analytics_usage(agent,at,record) VALUES('agent',?1,?2)",
                    rusqlite::params![index as i64, record.to_string()],
                )
                .expect("source row");
            }
            tx.commit().expect("seed");
            let instructions = Arc::new(AtomicU64::new(0));
            let counter = Arc::clone(&instructions);
            db.progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )
            .expect("VM counter");
            let snapshot = super::read_usage_snapshot(&db, "agent", 4).expect("bounded snapshot");
            db.progress_handler(1, None::<fn() -> bool>)
                .expect("remove VM counter");
            assert!(!snapshot.recorded.is_empty() && snapshot.recorded.len() <= 4);
            assert!(snapshot.truncated);
            instructions.load(Ordering::Relaxed)
        };
        assert_eq!(
            measure(100),
            measure(1000),
            "non-matching source rows must not increase SQLite VM work"
        );
    }

    #[cfg(unix)]
    #[test]
    fn usage_reconciliation_discovery_skips_symlink_archive_before_valid_sessions() {
        let root = tempfile::tempdir().expect("profile");
        let foreign = tempfile::tempdir().expect("foreign archive");
        let (member, accounts) = reconciliation_codex_fixture(root.path());
        for index in 0..20 {
            fs::write(foreign.path().join(format!("unrelated-{index}")), b"{}")
                .expect("foreign entry");
        }
        std::os::unix::fs::symlink(foreign.path(), root.path().join("archived_sessions"))
            .expect("archive link");
        let mut budget = super::EvidenceBudget {
            entries: super::MAX_EVIDENCE_ENTRIES - 12,
            ..Default::default()
        };
        let mut rows = BTreeMap::new();
        super::read_codex_evidence(&member, &accounts, &mut rows, &mut budget)
            .expect("valid sibling");
        assert_eq!(
            rows.len(),
            3,
            "rejected archive cannot hide valid session evidence"
        );
        assert_eq!(budget.rejected, 1);
        assert!(!budget.truncated);
        assert!(
            budget
                .files
                .iter()
                .any(|file| file.reason.is_some_and(|reason| reason.contains("symlink")))
        );
    }

    #[cfg(unix)]
    #[test]
    fn usage_reconciliation_discovery_rejects_alias_before_ambiguity_resolution() {
        let root = tempfile::tempdir().expect("profile");
        let (member, accounts) = reconciliation_codex_fixture(root.path());
        std::os::unix::fs::symlink(
            root.path().join("sessions/rollout-test-thread.jsonl"),
            root.path().join("sessions/rollout-alias-thread.jsonl"),
        )
        .expect("rollout link");
        let mut budget = super::EvidenceBudget::default();
        let mut rows = BTreeMap::new();
        super::read_codex_evidence(&member, &accounts, &mut rows, &mut budget)
            .expect("valid sibling");
        assert_eq!(
            rows.len(),
            3,
            "symlink candidate cannot make valid evidence ambiguous"
        );
        assert_eq!(budget.rejected, 1);
        assert!(
            budget
                .files
                .iter()
                .any(|file| file.reason.is_some_and(|reason| reason.contains("symlink")))
        );
    }

    #[test]
    fn usage_reconciliation_journal_ancestor_symlink_cannot_escape_profile() {
        let root = tempfile::tempdir().expect("state");
        let external = tempfile::tempdir().expect("external");
        let (members, accounts) = reconciliation_journal_fixture(root.path());
        let projects = root.path().join("profile/projects");
        fs::rename(&projects, external.path().join("projects")).expect("move projects");
        std::os::unix::fs::symlink(external.path().join("projects"), &projects)
            .expect("ancestor link");
        let mut budget = super::EvidenceBudget::default();
        let rows = super::read_local_claude_usage_inner(
            root.path(),
            &members,
            &accounts,
            Some(&mut budget),
        )
        .expect("comparison");
        assert!(
            rows.is_empty(),
            "configured profile root must confine ancestors too"
        );
        assert!(budget.incomplete);
        assert_eq!(budget.rejected, 1);
        assert!(
            budget
                .files
                .iter()
                .any(|file| file.reason.is_some_and(|reason| reason.contains("symlink")))
        );
    }

    #[test]
    fn usage_reconciliation_discovery_consumes_remaining_request_entries() {
        let root = tempfile::tempdir().expect("profile");
        let (member, accounts) = reconciliation_codex_fixture(root.path());
        let mut budget = super::EvidenceBudget {
            entries: super::MAX_EVIDENCE_ENTRIES - 2,
            ..Default::default()
        };
        let mut rows = BTreeMap::new();
        super::read_codex_evidence(&member, &accounts, &mut rows, &mut budget).expect("comparison");
        assert!(
            budget.truncated,
            "native discovery exhaustion must be truncation"
        );
        assert!(budget.entries <= super::MAX_EVIDENCE_ENTRIES);
        assert!(rows.is_empty());
    }

    #[test]
    fn usage_reconciliation_claude_parser_materializes_only_remaining_responses() {
        let root = tempfile::tempdir().expect("state");
        let (members, accounts) = reconciliation_journal_fixture(root.path());
        let journal = root.path().join("profile/projects/project/thread.jsonl");
        let original: Value =
            serde_json::from_slice(&fs::read(&journal).expect("journal")).expect("row");
        let lines = ["one", "two", "three"].map(|id| {
            let mut row = original.clone();
            row["message"]["id"] = json!(id);
            row.to_string()
        });
        let mut duplicate = original;
        duplicate["message"]["id"] = json!("one");
        duplicate["message"]["usage"]["output_tokens"] = json!(9);
        fs::write(&journal, format!("{}\n{duplicate}", lines.join("\n"))).expect("rows");
        super::EVIDENCE_RESPONSES_MATERIALIZED.with(|count| count.set(0));
        let mut budget = super::EvidenceBudget {
            responses: super::MAX_RECONCILIATION_RESPONSES - 1,
            ..Default::default()
        };
        let rows = super::read_local_claude_usage_inner(
            root.path(),
            &members,
            &accounts,
            Some(&mut budget),
        )
        .expect("comparison");
        assert_eq!(rows.len(), 1);
        assert!(rows[0].usage.conflicting_usage);
        assert_eq!(
            super::EVIDENCE_RESPONSES_MATERIALIZED.with(std::cell::Cell::get),
            1
        );
        assert!(budget.truncated);
    }

    #[test]
    fn usage_reconciliation_codex_parser_materializes_only_remaining_responses() {
        let root = tempfile::tempdir().expect("profile");
        let (member, accounts) = reconciliation_codex_fixture(root.path());
        super::EVIDENCE_RESPONSES_MATERIALIZED.with(|count| count.set(0));
        let mut budget = super::EvidenceBudget {
            responses: super::MAX_RECONCILIATION_RESPONSES - 1,
            ..Default::default()
        };
        let mut rows = BTreeMap::new();
        super::read_codex_evidence(&member, &accounts, &mut rows, &mut budget).expect("comparison");
        assert_eq!(rows.len(), 1);
        assert!(rows["one"].conflict);
        assert_eq!(
            super::EVIDENCE_RESPONSES_MATERIALIZED.with(std::cell::Cell::get),
            1
        );
        assert!(budget.truncated);
    }

    #[test]
    fn usage_reconciliation_partial_state_is_incomplete_instead_of_a_request_error() {
        let root = tempfile::tempdir().expect("state");
        let (members, accounts) = reconciliation_journal_fixture(root.path());
        let sessions = root.path().join("account-servers/claude/sessions");
        fs::create_dir_all(&sessions).expect("sessions");
        fs::write(sessions.join("thread.json"), b"{broken state").expect("state");
        let mut budget = super::EvidenceBudget::default();
        let rows = super::read_local_claude_usage_inner(
            root.path(),
            &members,
            &accounts,
            Some(&mut budget),
        )
        .expect("one broken native file cannot fail the request");
        assert_eq!(rows.len(), 1);
        assert!(budget.incomplete);
        fs::write(sessions.join("thread.json"), b"{a truncated state document").expect("state");
        let mut budget = super::EvidenceBudget {
            bytes: super::MAX_RECONCILIATION_NATIVE_BYTES - 4,
            ..Default::default()
        };
        assert!(
            super::read_local_claude_usage_inner(
                root.path(),
                &members,
                &accounts,
                Some(&mut budget)
            )
            .is_ok()
        );
        assert!(budget.truncated);
    }

    #[test]
    fn usage_reconciliation_total_byte_budget_is_shared_without_overshoot() {
        let root = tempfile::tempdir().expect("evidence");
        let first = root.path().join("first");
        let second = root.path().join("second");
        fs::write(&first, b"{}\n").expect("first");
        fs::write(&second, b"{}\n").expect("second");
        let mut budget = super::EvidenceBudget {
            bytes: super::MAX_RECONCILIATION_NATIVE_BYTES - 4,
            ..Default::default()
        };
        budget.read(&first).expect("first read");
        assert!(budget.read(&second).is_some());
        assert!(budget.bytes <= super::MAX_RECONCILIATION_NATIVE_BYTES);
        assert!(budget.truncated);
    }

    #[test]
    fn usage_reconciliation_unchanged_journal_is_parsed_once() {
        let root = tempfile::tempdir().expect("state");
        let (members, accounts) = reconciliation_journal_fixture(root.path());
        super::EVIDENCE_FILE_READS.with(|reads| reads.set(0));
        for _ in 0..2 {
            let mut budget = super::EvidenceBudget::default();
            assert_eq!(
                super::read_local_claude_usage_inner(
                    root.path(),
                    &members,
                    &accounts,
                    Some(&mut budget)
                )
                .expect("journal")
                .len(),
                1
            );
        }
        assert_eq!(
            super::EVIDENCE_FILE_READS.with(std::cell::Cell::get),
            1,
            "unchanged native evidence must be reused"
        );
        let journal = root.path().join("profile/projects/project/thread.jsonl");
        let original = fs::read_to_string(&journal).expect("original");
        let replacement = journal.with_extension("replacement");
        fs::write(&replacement, original.replace("response", "replaced")).expect("replacement");
        fs::rename(&replacement, &journal).expect("replace inode");
        let mut budget = super::EvidenceBudget::default();
        let rows = super::read_local_claude_usage_inner(
            root.path(),
            &members,
            &accounts,
            Some(&mut budget),
        )
        .expect("replacement");
        assert_eq!(rows[0].usage.id, "replaced");
        assert_eq!(super::EVIDENCE_FILE_READS.with(std::cell::Cell::get), 2);
        fs::write(&journal, b"").expect("truncate");
        let mut budget = super::EvidenceBudget::default();
        assert!(
            super::read_local_claude_usage_inner(
                root.path(),
                &members,
                &accounts,
                Some(&mut budget)
            )
            .expect("truncated")
            .is_empty()
        );
        assert_eq!(super::EVIDENCE_FILE_READS.with(std::cell::Cell::get), 3);
    }

    #[test]
    fn usage_reconciliation_codex_rejects_foreign_session_header() {
        let root = tempfile::tempdir().expect("profile");
        fs::create_dir(root.path().join("sessions")).expect("sessions");
        let header = json!({"type":"session_meta","payload":{"id":"another-thread"}});
        let usage = json!({"type":"token_usage_record","payload":{"response_id":"response","usage":{"input_tokens":10,"output_tokens":2}}});
        fs::write(
            root.path().join("sessions/rollout-test-thread.jsonl"),
            format!("{header}\n{usage}\n"),
        )
        .expect("rollout");
        let member = member(json!({"accountKey":"codex","threadId":"thread"}));
        let accounts = BTreeMap::from([(
            "codex".into(),
            json!({"provider":"codex","home":root.path()}),
        )]);
        let mut rows = BTreeMap::new();
        let mut budget = super::EvidenceBudget::default();
        super::read_codex_evidence(&member, &accounts, &mut rows, &mut budget).expect("comparison");
        assert!(
            rows.is_empty(),
            "foreign rollout header cannot authorize evidence"
        );
        assert!(budget.incomplete);
    }

    #[cfg(unix)]
    #[test]
    fn usage_reconciliation_journal_symlink_cannot_escape_its_project() {
        let root = tempfile::tempdir().expect("state");
        let external = tempfile::tempdir().expect("external");
        let (members, accounts) = reconciliation_journal_fixture(root.path());
        let journal = root.path().join("profile/projects/project/thread.jsonl");
        fs::rename(&journal, external.path().join("foreign.jsonl")).expect("move");
        std::os::unix::fs::symlink(external.path().join("foreign.jsonl"), &journal)
            .expect("symlink");
        let mut budget = super::EvidenceBudget::default();
        let rows = super::read_local_claude_usage_inner(
            root.path(),
            &members,
            &accounts,
            Some(&mut budget),
        )
        .expect("comparison");
        assert!(
            rows.is_empty(),
            "foreign journal file cannot authorize evidence"
        );
        assert!(budget.incomplete);
    }

    #[tokio::test]
    async fn usage_reconciliation_prefers_final_recorded_response_to_later_legacy_rows() {
        let directory = tempfile::tempdir().expect("temporary analytics");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store");
        store.analytics_transaction::<_,StoreError>(|tx,_| {
            AnalyticsCaptureRepository::write(tx,CaptureTable::Agents,&json!({"id":"agent","rootId":"agent"}))?;
            let mut final_row = json!({"id":"final","agentId":"agent","rootId":"agent","at":1,"model":"gpt-5","responseId":"response",
                "last":{"inputTokens":10,"outputTokens":50},"total":{},"studioUsage":{"kind":"response","responseId":"response","model":"gpt-5",
                    "tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":50},"evidence":"live"}});
            AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&final_row)?;
            final_row.as_object_mut().expect("row").remove("studioUsage");
            final_row["id"] = json!("legacy");
            final_row["at"] = json!(2);
            final_row["last"]["outputTokens"] = json!(10);
            AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&final_row)?;
            Ok(())
        }).await.expect("response records");
        let result =
            super::usage_reconciliation(&store, "agent", &BTreeMap::new()).expect("reconciliation");
        assert_eq!(result["responses"][0]["recordedEvidence"], "live");
        assert_eq!(result["responses"][0]["recordedTokens"]["output"], 50);
    }

    #[test]
    fn usage_reconciliation_codex_evidence_uses_the_existing_rollout_importer() {
        let directory = tempfile::tempdir().expect("temporary rollout profile");
        fs::create_dir(directory.path().join("sessions")).expect("sessions");
        let context = json!({"type":"turn_context","payload":{"turn_id":"turn","model":"gpt-5"}});
        let usage = json!({"type":"token_usage_record","payload":{"thread_id":"thread","response_id":"response",
            "usage":{"input_tokens":100,"cached_input_tokens":30,"output_tokens":20,"reasoning_output_tokens":7}}});
        fs::write(
            directory.path().join("sessions/rollout-test-thread.jsonl"),
            format!(
                "{}\n{context}\n{usage}\n",
                json!({"type":"session_meta","payload":{"id":"thread"}})
            ),
        )
        .expect("rollout");
        let member = member(json!({"provider":"codex","accountKey":"codex","threadId":"thread"}));
        let accounts = BTreeMap::from([(
            "codex".into(),
            json!({"provider":"codex","home":directory.path()}),
        )]);
        let mut native = BTreeMap::new();
        let mut budget = super::EvidenceBudget::default();
        super::read_codex_evidence(&member, &accounts, &mut native, &mut budget)
            .expect("native evidence");
        let response = native.get("response").expect("response from native parser");
        assert_eq!(response.model.as_deref(), Some("gpt-5"));
        let tokens = response.tokens.as_ref().expect("known tokens");
        assert_eq!(
            (
                tokens.uncached_input,
                tokens.cache_read_input,
                tokens.output
            ),
            (70, 30, 20)
        );
        assert_eq!(tokens.reasoning_output, Some(7));
        assert!(!budget.incomplete);
        assert!(!budget.truncated);
    }

    #[test]
    fn usage_reconciliation_prices_request_tiers_and_keeps_unknown_tokens() {
        use studio_runtime_schema::usage::UsageTokens;
        let catalog = crate::pricing::PricingCatalog::from_value(&json!({"providers":{
            "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":2,"cache_read":0.5,"cache_write":1.5,
                "tiers":[{"tier":{"type":"context","size":100},"input":3,"output":4,"cache_read":1,"cache_write":2}]}}}},
            "anthropic":{"models":{"claude-test":{"cost":{"input":1,"output":1}}}}
        }})).expect("catalog");
        let tokens = UsageTokens {
            uncached_input: 70,
            cache_read_input: 20,
            cache_write_input: 10,
            cache_write_input_1h: None,
            output: 5,
            reasoning_output: Some(2),
        };
        assert_eq!(
            super::price_response(Some(&catalog), Some("gpt-test"), Some(&tokens)),
            Some(0.00027)
        );
        assert_eq!(
            super::price_response(Some(&catalog), Some("gpt-test"), None),
            None
        );
        let mut unknown_reasoning = tokens.clone();
        unknown_reasoning.reasoning_output = None;
        assert!(super::same_billable_tokens(
            Some(&tokens),
            Some(&unknown_reasoning)
        ));
        assert!(!super::same_billable_tokens(Some(&tokens), None));
        assert!(super::log_tokens(super::CostTokens::default()).is_none());
    }

    #[test]
    fn usage_reconciliation_native_reader_stops_at_entry_budget() {
        let directory = tempfile::tempdir().expect("temporary evidence");
        let path = directory.path().join("journal");
        fs::write(&path, b"{}\n").expect("evidence");
        let mut budget = super::EvidenceBudget {
            entries: super::MAX_EVIDENCE_ENTRIES,
            ..Default::default()
        };
        assert!(budget.read(&path).is_none());
        assert!(budget.truncated);
        assert_eq!(budget.bytes, 0);
        let mut budget = super::EvidenceBudget {
            bytes: super::MAX_RECONCILIATION_NATIVE_BYTES,
            ..Default::default()
        };
        assert!(budget.read(&path).is_none());
        assert!(budget.truncated);
    }

    #[test]
    fn usage_reconciliation_detects_differing_journal_replays() {
        let directory = tempfile::tempdir().expect("temporary journal");
        let path = directory.path().join("thread.jsonl");
        let row = json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z",
            "message":{"id":"response","model":"claude-sonnet-4-5",
            "usage":{"input_tokens":10,"output_tokens":2}}});
        fs::write(&path, format!("{row}\n{row}\n")).expect("identical journal rows");
        assert!(!super::parse_claude_log(&path).expect("journal")[0].conflicting_usage);
        let mut differing = row.clone();
        differing["message"]["usage"]["output_tokens"] = json!(7);
        fs::write(&path, format!("{row}\n{row}\n{differing}\n")).expect("journal");
        let rows = super::parse_claude_log(&path).expect("parsed journal");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].tokens.output_tokens, Some(2.0));
        assert!(
            rows[0].conflicting_usage,
            "a later different usage must remain visible"
        );
    }

    #[test]
    fn local_claude_session_usage_is_thread_scoped_deduplicated_and_uses_account_history() {
        let _counter = hold_skipped_line_counter();
        let temp = tempdir().expect("temporary fixture directory");
        let state = temp.path().join("state");
        let current_profile = temp.path().join("current-claude");
        let previous_profile = temp.path().join("previous-claude");
        let current_project = current_profile.join("projects/project");
        let previous_project = previous_profile.join("projects/project");
        fs::create_dir_all(&current_project).expect("create current project");
        fs::create_dir_all(&previous_project).expect("create previous project");
        fs::create_dir_all(state.join("account-servers/current/sessions"))
            .expect("create account session state");
        fs::write(
            state.join("account-servers/current/sessions/thread-current.json"),
            json!({
                "nativeId": "native-current",
                "historyBranches": [{"nativeId": "branch-current"}],
                "controlRequests": {"control-1": {
                    "sourceNativeId": "source-current",
                    "targetNativeId": "target-current"
                }}
            })
            .to_string(),
        )
        .expect("write current session");
        let valid_row = json!({
            "type": "assistant",
            "timestamp": "2026-09-01T12:00:00Z",
            "message": {
                "id": "response-1",
                "model": "claude-sonnet-4-5",
                "usage": {
                    "input_tokens": 100,
                    "cache_read_input_tokens": 20,
                    "cache_creation_input_tokens": 5,
                    "output_tokens": 10
                }
            }
        });
        let invalid_identity = json!({
            "type": "assistant",
            "timestamp": "2026-09-01T12:00:00Z",
            "requestId": "must-not-be-used",
            "message": {"id": 7, "model": "claude-sonnet-4-5", "usage": {}}
        });
        let naive_timestamp = json!({
            "type": "assistant",
            "timestamp": "2026-09-01T12:00:00",
            "message": {
                "id": "response-naive-time",
                "model": "claude-sonnet-4-5",
                "usage": {
                    "input_tokens": 10,
                    "cache_read_input_tokens": 0,
                    "cache_creation_input_tokens": 0,
                    "output_tokens": 2
                }
            }
        });
        fs::write(
            current_project.join("thread-current.jsonl"),
            format!("{valid_row}\n{invalid_identity}\n{naive_timestamp}\n"),
        )
        .expect("write primary current history rows");
        for id in [
            "native-current",
            "branch-current",
            "source-current",
            "target-current",
        ] {
            fs::write(
                current_project.join(format!("{id}.jsonl")),
                format!("{}\n", valid_row),
            )
            .expect("write current history row");
        }
        fs::write(
            current_project.join("unrelated-thread.jsonl"),
            format!("{}\n", valid_row),
        )
        .expect("write unrelated history row");
        let mut malformed_tokens = valid_row.clone();
        malformed_tokens["message"]["id"] = json!("response-2");
        malformed_tokens["message"]["usage"]["input_tokens"] = json!(true);
        fs::write(
            previous_project.join("thread-previous.jsonl"),
            format!("{}\n", malformed_tokens),
        )
        .expect("write previous history row");
        let accounts = BTreeMap::from([
            (
                "current".to_owned(),
                json!({"provider":"claude", "claudeOptions":{"configDir":current_profile}}),
            ),
            (
                "previous".to_owned(),
                json!({"provider":"claude", "claudeOptions":{"configDir":previous_profile}}),
            ),
        ]);
        let members = vec![(
            "agent".to_owned(),
            member(json!({
                "accountKey":"current",
                "threadId":"thread-current",
                "accountHistory":[{
                    "accountKey":"previous",
                    "threadId":"thread-previous",
                    "provider":"claude"
                }]
            })),
        )];

        let skipped_before = super::claude_log_lines_skipped();
        let rows = read_local_claude_usage(&state, &members, &accounts).expect("local usage");
        // The previous account's row has a boolean token count: that line is skipped whole
        // and counted, as is the line whose response id is a number.
        assert_eq!(rows.len(), 2);
        assert_eq!(super::claude_log_lines_skipped() - skipped_before, 2);
        assert!(!rows.iter().any(|row| row.usage.id == "must-not-be-used"));
        assert!(rows.iter().any(|row| row.usage.id == "response-naive-time"));
        let current = rows
            .iter()
            .find(|row| row.account_key == "current")
            .expect("current session row");
        assert_eq!(current.thread_id, "thread-current");
        assert_eq!(current.usage.id, "response-1");
        assert_eq!(
            current.usage.tokens,
            CostTokens {
                input_tokens: Some(100.0),
                cached_input_tokens: Some(20.0),
                cache_write_input_tokens: Some(5.0),
                cache_write_input_1h_tokens: Some(0.0),
                output_tokens: Some(10.0),
            }
        );
        assert!(!rows.iter().any(|row| row.account_key == "previous"));
    }

    /// A Claude log line that does not decode is skipped whole and counted; one that merely is
    /// not an assistant response is not an error. A Studio state file of another shape fails.
    #[test]
    fn malformed_log_lines_are_skipped_and_counted_and_bad_state_files_fail() {
        let _counter = hold_skipped_line_counter();
        let temp = tempdir().expect("temporary fixture directory");
        let state = temp.path().join("state");
        let profile = temp.path().join("claude");
        let project = profile.join("projects/project");
        fs::create_dir_all(&project).expect("create project");
        fs::create_dir_all(state.join("account-servers/acct/sessions")).expect("sessions");
        let good = json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z",
            "message":{"id":"r1","model":"claude-sonnet-4-5","usage":{"input_tokens":1,"output_tokens":2}}});
        let wrong_type = json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z",
            "message":{"id":7,"model":"claude-sonnet-4-5","usage":{}}});
        fs::write(
            project.join("thread.jsonl"),
            format!(
                "{good}\nnot json\n{wrong_type}\n{}\n",
                json!({"type":"summary","message":"free text"})
            ),
        )
        .expect("write log");
        let accounts = BTreeMap::from([(
            "acct".to_owned(),
            json!({"provider":"claude","claudeOptions":{"configDir":profile}}),
        )]);
        let members = vec![(
            "agent".to_owned(),
            member(json!({"accountKey":"acct","threadId":"thread"})),
        )];
        let before = super::claude_log_lines_skipped();
        let rows = read_local_claude_usage(&state, &members, &accounts).expect("usage");
        assert_eq!(rows.len(), 1);
        assert_eq!(super::claude_log_lines_skipped() - before, 2);
        // Parsed once per file version: a second read does not count them again.
        read_local_claude_usage(&state, &members, &accounts).expect("usage again");
        assert_eq!(super::claude_log_lines_skipped() - before, 2);

        fs::write(
            state.join("account-servers/acct/sessions/thread.json"),
            r#"{"nativeId": 5}"#,
        )
        .expect("write bad state");
        assert!(matches!(
            read_local_claude_usage(&state, &members, &accounts),
            Err(CostsError::InvalidSessionState)
        ));
    }

    /// The Claude bridge saves a snapshot envelope whose session holds every turn, so the
    /// file can pass 16 MiB. Its native ids still select that session's logs.
    #[test]
    fn bridge_session_snapshots_are_read_at_any_size() {
        let temp = tempdir().expect("temporary fixture directory");
        let state = temp.path().join("state");
        let profile = temp.path().join("claude");
        let project = profile.join("projects/project");
        fs::create_dir_all(&project).expect("create project");
        fs::create_dir_all(state.join("account-servers/acct/sessions")).expect("sessions");
        let row = |id: &str| {
            json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z",
                "message":{"id":id,"model":"claude-sonnet-4-5",
                    "usage":{"input_tokens":1,"output_tokens":2}}})
        };
        fs::write(
            project.join("native-small.jsonl"),
            format!("{}\n", row("r-small")),
        )
        .expect("write small native log");
        fs::write(
            project.join("native-large.jsonl"),
            format!("{}\n", row("r-large")),
        )
        .expect("write large native log");
        let accounts = BTreeMap::from([(
            "acct".to_owned(),
            json!({"provider":"claude","claudeOptions":{"configDir":profile}}),
        )]);
        for (thread, native, bytes) in [
            ("thread-small", "native-small", 1_024),
            ("thread-large", "native-large", 17 * 1024 * 1024),
        ] {
            use std::io::{Read, Write};
            let mut snapshot = fs::File::create(
                state.join(format!("account-servers/acct/sessions/{thread}.json")),
            )
            .expect("create session snapshot");
            write!(snapshot, "{{\"format\":{:?},\"sequence\":3,\"session\":{{\"id\":{thread:?},\"nativeId\":{native:?},\"turns\":[{{\"text\":\"", super::claude_session_format()).expect("snapshot prefix");
            std::io::copy(&mut std::io::repeat(b'x').take(bytes), &mut snapshot)
                .expect("snapshot turns");
            snapshot.write_all(b"\"}]}}").expect("snapshot suffix");
            let members = vec![(
                "agent".to_owned(),
                member(json!({"accountKey":"acct","threadId":thread})),
            )];
            let rows =
                read_local_claude_usage(&state, &members, &accounts).expect("snapshot usage");
            assert_eq!(
                rows.iter()
                    .map(|row| row.usage.id.as_str())
                    .collect::<Vec<_>>(),
                [if thread == "thread-small" {
                    "r-small"
                } else {
                    "r-large"
                }]
            );
        }

        // A snapshot of another format is still an error, not an empty session.
        fs::write(
            state.join("account-servers/acct/sessions/thread-small.json"),
            json!({"format":"studio-claude-session-v0","session":{"nativeId":"native-small"}})
                .to_string(),
        )
        .expect("write unknown snapshot format");
        let members = vec![(
            "agent".to_owned(),
            member(json!({"accountKey":"acct","threadId":"thread-small"})),
        )];
        assert!(matches!(
            read_local_claude_usage(&state, &members, &accounts),
            Err(CostsError::InvalidSessionState)
        ));
    }

    #[test]
    fn non_claude_accounts_do_not_read_local_project_logs() {
        let temp = tempdir().expect("temporary fixture directory");
        let config = temp.path().join("profile");
        fs::create_dir_all(config.join("projects/project")).expect("create project");
        fs::write(
            config.join("projects/project/thread.jsonl"),
            format!("{}\n", json!({"type":"assistant"})),
        )
        .expect("write ignored history");
        let accounts = BTreeMap::from([(
            "account".to_owned(),
            json!({"provider":"openai", "claudeOptions":{"configDir":config}}),
        )]);
        let members = vec![(
            "agent".to_owned(),
            member(json!({"accountKey":"account", "threadId":"thread"})),
        )];
        assert!(
            read_local_claude_usage(temp.path(), &members, &accounts)
                .expect("local usage")
                .is_empty()
        );
    }

    #[tokio::test]
    async fn usage_reconciliation_session_cost_preserves_final_rows_and_fills_legacy_history() {
        let temp = tempdir().expect("temporary fixture directory");
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("runtime store opens");
        let profile = temp.path().join("claude-profile");
        let project = profile.join("projects/project");
        fs::create_dir_all(project).expect("create Claude project");
        fs::create_dir_all(temp.path().join("account-servers/claude/sessions"))
            .expect("create account session directory");
        fs::write(
            temp.path()
                .join("account-servers/claude/sessions/thread.json"),
            json!({"nativeId":"native-thread"}).to_string(),
        )
        .expect("write session metadata");
        let local_row = json!({
            "type":"assistant",
            "timestamp":"2026-09-01T12:00:00Z",
            "message":{
                "id":"response-1",
                "model":"claude-sonnet-4-5",
                "usage":{
                    "input_tokens":100,
                    "cache_read_input_tokens":20,
                    "cache_creation_input_tokens":5,
                    "output_tokens":10
                }
            }
        });
        let incomplete_row = json!({
            "type":"assistant",
            "timestamp":"2026-09-01T12:01:00Z",
            "message":{
                "id":"response-2",
                "model":"claude-sonnet-4-5",
                "usage":{"input_tokens":50,"cache_read_input_tokens":0,"cache_creation_input_tokens":0}
            }
        });
        for session_id in ["thread", "native-thread"] {
            fs::write(
                temp.path()
                    .join("claude-profile/projects/project")
                    .join(format!("{session_id}.jsonl")),
                format!("{local_row}\n{incomplete_row}\n"),
            )
            .expect("write local cost rows");
        }
        fs::create_dir_all(temp.path().join("model-pricing")).expect("create pricing directory");
        fs::write(
            temp.path().join("model-pricing/models-dev-v1.json"),
            json!({
                "version":1,
                "catalog":{"providers":{
                    "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":1}}}},
                    "anthropic":{"models":{"claude-sonnet-4-5":{"cost":{
                        "input":1,"output":2,"cache_read":0.5,"cache_write":1
                    }}}}
                }}
            })
            .to_string(),
        )
        .expect("write pricing catalog");
        let agent = json!({
            "id":"agent",
            "rootId":"agent",
            "accountKey":"claude",
            "provider":"claude",
            "threadId":"thread",
            "accountHistory":[]
        });
        let receipt = json!({
            "id":"usage-receipt",
            "agentId":"agent",
            "rootId":"agent",
            "threadId":"thread",
            "turnId":"turn-1",
            "at":1,
            "accountKey":"claude",
            "responseId":"response-1",
            "model":"claude-sonnet-4-5",
            "last":{"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":5,"outputTokens":10},
            "total":{"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":5,"outputTokens":10,"totalTokens":135}
        });
        store
            .analytics_transaction::<_, StoreError>(move |tx, _| {
                AnalyticsCaptureRepository::write(tx, CaptureTable::Agents, &agent)?;
                AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &receipt)?;
                Ok(())
            })
            .await
            .expect("write analytics fixtures");
        assert!(matches!(
            session_cost(&store, "agent"),
            Err(CostsError::AccountProfilesRequired)
        ));
        let accounts = BTreeMap::from([(
            "claude".to_owned(),
            json!({"provider":"claude", "claudeOptions":{"configDir":profile}}),
        )]);

        let result = session_cost_with_account_profiles(&store, "agent", &accounts)
            .expect("session cost is available");
        assert_eq!(result["pricedSamples"], 1);
        assert_eq!(result["totalUSD"], json!(0.000135));
        assert_eq!(result["claudeHistoryIncomplete"], true);
        assert_eq!(result["unknownModels"], json!(["claude-sonnet-4-5"]));

        // Cached log parses and the cached catalog follow the files: a second answer
        // in the log is priced, and new rates replace the old ones.
        let second = json!({
            "type":"assistant",
            "timestamp":"2026-09-01T12:02:00Z",
            "message":{
                "id":"response-3",
                "model":"claude-sonnet-4-5",
                "usage":{
                    "input_tokens":100,
                    "cache_read_input_tokens":20,
                    "cache_creation_input_tokens":5,
                    "output_tokens":10
                }
            }
        });
        let log = temp
            .path()
            .join("claude-profile/projects/project/thread.jsonl");
        let mut text = fs::read_to_string(&log).expect("read log");
        text.push_str(&format!("{second}\n"));
        fs::write(&log, text).expect("append a response");
        let grown = session_cost_with_account_profiles(&store, "agent", &accounts)
            .expect("session cost after the log grew");
        assert_eq!(grown["pricedSamples"], 2);
        assert_eq!(grown["totalUSD"], json!(0.00027));
        fs::write(
            temp.path().join("model-pricing/models-dev-v1.json"),
            json!({
                "version":1,
                "catalog":{"providers":{
                    "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":1}}}},
                    "anthropic":{"models":{"claude-sonnet-4-5":{"cost":{
                        "input":2,"output":4,"cache_read":1,"cache_write":2
                    }}}}
                }}
            })
            .to_string(),
        )
        .expect("rewrite pricing catalog");
        let repriced = session_cost_with_account_profiles(&store, "agent", &accounts)
            .expect("session cost after the rates changed");
        assert_eq!(repriced["totalUSD"], json!(0.00054));
        // A final response is authoritative even if a journal reports different usage.
        for evidence in ["live", "recovered"] {
            store.analytics_transaction::<_,StoreError>(move |tx,_| {
                let raw: String = tx.query_row("SELECT record FROM analytics_usage WHERE id='usage-receipt'", [], |row|row.get(0))?;
                let mut row: Value = serde_json::from_str(&raw)?;
                row["inputTokensAreUncached"] = json!(true);
                row["last"]["outputTokens"] = json!(50);
                row["studioUsage"] = json!({"kind":"response","responseId":"response-1","model":"claude-sonnet-4-5",
                    "tokens":{"uncachedInput":100,"cacheReadInput":20,"cacheWriteInput":5,"output":50},"evidence":evidence});
                AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&row)?;
                Ok(())
            }).await.expect("record final response");
            let final_cost =
                session_cost_with_account_profiles(&store, "agent", &accounts).expect("final cost");
            assert_eq!(
                final_cost["totalUSD"],
                json!(0.0007),
                "{evidence} final response must override journal estimates"
            );
        }
    }
}
