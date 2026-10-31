//! Incremental per-root session-cost aggregate in analytics.sqlite3.
//!
//! Every `analytics_usage` write ([`apply_usage_write`]) updates, in the same
//! transaction, `analytics_cost_rows` (the cost facts of each usage row and the
//! contribution it currently adds) and `analytics_cost_groups` (per root, the
//! number of counted rows per model/account/token tuple). The aggregate is
//! therefore crash-consistent with the usage rows and the projection cursor,
//! and a cursor-0 rebuild of analytics rebuilds it.
//!
//! Counting rules, within one root:
//! - cumulative observations and gaps never contribute response costs;
//! - each (agent, response) contributes once, preferring the first final evidence
//!   over provisional legacy rows;
//! - a row without a model takes the model of the latest (at, seq) eligible row
//!   of the same (agent, turn), else of the same (agent, thread); a row is
//!   eligible when it has a model and is not itself suppressed by the first rule.
//!
//! A model-bearing row depends on its turn and duplicate response identities, so
//! a write refreshes those scopes; a model-less row depends on its whole
//! agent, so an agent with eligible model-less rows is refreshed whole.
//!
//! Pricing is not stored: a reader prices the few groups with the current
//! catalog, so a catalog change needs no invalidation.
use studio_runtime_schema::provider_kind::ProviderKind;

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
#[cfg(test)]
use std::path::Path;

#[cfg(test)]
use rusqlite::OpenFlags;
use rusqlite::{Connection, OptionalExtension, Row, params};
use serde::Deserialize;
use studio_runtime_schema::usage::{UsageEvidence, UsageObservation, UsageTokens};

use crate::{CachedSql, StoreError, StoreResult};

/// Current tables of the aggregate, installed during the analytics startup transaction.
pub(crate) const TABLES: &str = "
CREATE TABLE analytics_cost_rows (
    id TEXT PRIMARY KEY, root TEXT NOT NULL, agent TEXT NOT NULL, thread TEXT, turn TEXT,
    seq INTEGER NOT NULL, at REAL NOT NULL, model TEXT, account_key TEXT, response TEXT,
    input REAL, cached REAL, cache_write REAL, output REAL, uncached INTEGER NOT NULL,
    counted INTEGER NOT NULL, counted_model TEXT, eligible INTEGER, final_evidence INTEGER NOT NULL, cache_write_1h REAL);
CREATE TABLE analytics_cost_groups (
    root TEXT NOT NULL, model TEXT, account TEXT NOT NULL, input REAL, cached REAL,
    cache_write REAL, output REAL, uncached INTEGER NOT NULL, count INTEGER NOT NULL, cache_write_1h REAL);
";

/// Indexes of the aggregate, built after the backfill loaded its rows.
const INDEXES: &str = "
CREATE INDEX analytics_cost_rows_scope ON analytics_cost_rows(root, agent, thread, turn);
CREATE INDEX analytics_cost_rows_response ON analytics_cost_rows(root, thread, response)
    WHERE response IS NOT NULL;
";

/// Seek response counterparts and eligible model-less facts within one root/agent scope.
/// Install after compatibility columns exist; retire the broader model-less index once.
const SCOPE_INDEXES: &str = "
CREATE INDEX IF NOT EXISTS analytics_cost_rows_agent_response
    ON analytics_cost_rows(root,agent,response) WHERE response IS NOT NULL;
CREATE INDEX IF NOT EXISTS analytics_cost_rows_eligible_unmodeled
    ON analytics_cost_rows(root,agent) WHERE model IS NULL AND eligible IS NOT 0;
DROP INDEX IF EXISTS analytics_cost_rows_unmodeled;
";

const ROW_COLUMNS: &str = "id,agent,thread,turn,seq,at,model,account_key,response,\
    input,cached,cache_write,output,uncached,counted,counted_model,eligible,final_evidence,cache_write_1h";

/// The account key of a usage row or agent record without `accountKey`.
pub const DEFAULT_ACCOUNT: &str = "default";

/// Batched receipt lookup through selected current/history scopes and the existing usage index.
/// Parameters: serialized receipt tuples and selected (agent, account, thread) tuples.
const JOURNAL_RECEIPT_SQL: &str = "
WITH receipts AS MATERIALIZED (
 SELECT json_extract(value,'$[0]') account,json_extract(value,'$[1]') thread,
        json_extract(value,'$[2]') response FROM json_each(?1)
), scopes AS MATERIALIZED (
 SELECT json_extract(value,'$[0]') agent,json_extract(value,'$[1]') account,
        json_extract(value,'$[2]') thread FROM json_each(?2)
)
SELECT r.account,r.thread,r.response,u.record,u.id
FROM receipts r JOIN scopes s ON s.account=r.account AND s.thread=r.thread
CROSS JOIN analytics_usage u INDEXED BY analytics_usage_response
WHERE u.agent=s.agent AND u.thread=s.thread AND json_extract(u.record,'$.responseId')=r.response
AND json_type(u.record,'$.studioUsage') IS NOT NULL";

#[cfg(test)]
thread_local! {
    static RECEIPT_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static JOURNAL_DELTA_RUNS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Token counts of one usage row as priced; `None` is an unreported count.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CostTokens {
    pub input_tokens: Option<f64>,
    pub cached_input_tokens: Option<f64>,
    pub cache_write_input_tokens: Option<f64>,
    pub cache_write_input_1h_tokens: Option<f64>,
    pub output_tokens: Option<f64>,
}

/// The `AnalyticsUsage` members that decide a row's cost.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct UsageCostFields<'a> {
    #[serde(borrow, default)]
    model: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    account_key: Option<Cow<'a, str>>,
    #[serde(borrow, default)]
    response_id: Option<Cow<'a, str>>,
}

impl From<&UsageTokens> for CostTokens {
    fn from(tokens: &UsageTokens) -> Self {
        Self {
            input_tokens: Some(tokens.uncached_input() as f64),
            cached_input_tokens: Some(tokens.cache_read_input() as f64),
            cache_write_input_tokens: Some(tokens.cache_write_input() as f64),
            cache_write_input_1h_tokens: tokens.cache_write_input_1h().map(|n| n as f64),
            output_tokens: Some(tokens.output() as f64),
        }
    }
}

fn cost_observation(
    db: &Connection,
    record: &serde_json::Value,
) -> StoreResult<Option<UsageObservation>> {
    if record.get("studioUsage").is_some() {
        return studio_runtime_schema::usage::ledger::UsageLedgerRow::from_typed_analytics(record)
            .map(|ledger| Some(ledger.into_observation()))
            .map_err(|error| StoreError::SchemaValidation(error.to_string()));
    }
    let provider =
        super::analytics_capture::AnalyticsCaptureRepository::legacy_usage_provider(db, record)?;
    studio_runtime_schema::usage::persisted::parse_analytics_cost_usage_row(provider, record)
        .map_err(|error| StoreError::SchemaValidation(error.to_string()))
}

/// The members of an agent record that decide how its usage is priced and
/// where its Claude session logs are.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CostMember {
    #[serde(default)]
    pub account_key: Option<String>,
    pub provider: ProviderKind,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub account_history: Option<Vec<CostAccountHistory>>,
}

/// One earlier account of an agent.
#[derive(Clone, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CostAccountHistory {
    #[serde(default)]
    pub account_key: Option<String>,
    pub provider: ProviderKind,
    #[serde(default)]
    pub thread_id: Option<String>,
}

/// One model/account/token tuple of a root and the number of usage rows it counts.
#[derive(Clone, Debug, PartialEq)]
pub struct CostGroup {
    pub model: Option<String>,
    pub account_key: String,
    pub tokens: CostTokens,
    pub input_uncached: bool,
    pub count: u64,
}

/// Revision and changed SQLite group identities; absent groups are tombstones.
pub type CostGroupChanges = (i64, Vec<(i64, Option<CostGroup>)>);

/// Read current cost groups or only group identities changed since `revision`.
/// A missing group denotes deletion or movement to another root.
pub fn cost_group_changes(
    db: &Connection,
    root: &str,
    revision: Option<i64>,
) -> StoreResult<CostGroupChanges> {
    let current: i64 = db.query_row(
        "SELECT revision FROM analytics_usage_revision_clock",
        [],
        |row| row.get(0),
    )?;
    let sql = if revision.is_some() {
        "SELECT v.id,g.root,g.model,g.account,g.input,g.cached,g.cache_write,g.output,g.uncached,g.count,g.cache_write_1h
         FROM analytics_cost_group_revisions v LEFT JOIN analytics_cost_groups g ON g.rowid=v.id
         WHERE v.revision>?1 AND v.revision<=?2"
    } else {
        "SELECT rowid,root,model,account,input,cached,cache_write,output,uncached,count,cache_write_1h
         FROM analytics_cost_groups WHERE root=?3"
    };
    let mut statement = db.prepare_cached(sql)?;
    let mut rows = if let Some(revision) = revision {
        statement.query(params![revision, current])?
    } else {
        statement.query(params![rusqlite::types::Null, rusqlite::types::Null, root])?
    };
    let mut result = Vec::new();
    while let Some(row) = rows.next()? {
        let id = row.get(0)?;
        let row_root: Option<String> = row.get(1)?;
        let group = if row_root.as_deref() == Some(root) && row.get::<_, i64>(9)? > 0 {
            Some(CostGroup {
                model: row.get(2)?,
                account_key: row.get(3)?,
                tokens: CostTokens {
                    input_tokens: row.get(4)?,
                    cached_input_tokens: row.get(5)?,
                    cache_write_input_tokens: row.get(6)?,
                    output_tokens: row.get(7)?,
                    cache_write_input_1h_tokens: row.get(10)?,
                },
                input_uncached: row.get(8)?,
                count: row.get::<_, i64>(9)? as u64,
            })
        } else {
            None
        };
        result.push((id, group));
    }
    Ok((current, result))
}

/// A Claude log response that replaces the analytics row of the same
/// (account key, Studio thread, response id).
pub type CostReceipt = (String, String, String);

/// One recorded response for diagnostics; missing legacy counts remain unknown.
#[derive(Clone, Debug)]
pub struct RecordedResponse {
    pub agent_id: String,
    pub response_id: String,
    pub model: Option<String>,
    pub tokens: Option<UsageTokens>,
    pub evidence: UsageEvidence,
    /// Server observation timestamp, seconds since Unix epoch.
    pub at: f64,
}

/// Journal receipts eligible to replace provisional rows, and those superseded by final evidence.
#[derive(Clone, Debug, Default)]
pub struct JournalPartition {
    pub accepted: BTreeSet<CostReceipt>,
    pub superseded: BTreeSet<CostReceipt>,
}

type ReceiptScopes = BTreeSet<(String, String, String)>;

/// Validated journal matches retained by the reader for one session root.
#[derive(Default)]
pub struct JournalReceiptCache {
    entry: ReceiptEntry,
    facts: HashMap<String, Vec<(CostReceipt, bool)>>,
    final_counts: HashMap<CostReceipt, usize>,
}

// The delta and its validity marker are replaced together.
#[derive(Default)]
struct ReceiptEntry {
    root: Option<String>,
    scopes: ReceiptScopes,
    receipts: BTreeSet<CostReceipt>,
    revision: Option<i64>,
    partition: JournalPartition,
    deltas: Option<BTreeMap<GroupKey, i64>>,
}

fn receipt_scopes(members: &[(String, CostMember)]) -> ReceiptScopes {
    members
        .iter()
        .flat_map(|(agent, member)| {
            let current =
                std::iter::once((member.account_key.as_deref(), member.thread_id.as_deref()));
            let history = member
                .account_history
                .iter()
                .flatten()
                .map(|entry| (entry.account_key.as_deref(), entry.thread_id.as_deref()));
            current.chain(history).filter_map(move |(account, thread)| {
                Some((
                    agent.clone(),
                    account.unwrap_or(DEFAULT_ACCOUNT).to_owned(),
                    thread?.to_owned(),
                ))
            })
        })
        .collect()
}

fn final_receipt(raw: &str) -> StoreResult<bool> {
    #[cfg(test)]
    RECEIPT_READS.update(|count| count + 1);
    let observation =
        studio_runtime_schema::usage::ledger::UsageLedgerRow::observation_from_typed_analytics_str(
            raw,
        )
        .map_err(|error| StoreError::SchemaValidation(error.to_string()))?;
    Ok(matches!(
        observation,
        UsageObservation::Response {
            evidence: UsageEvidence::Live | UsageEvidence::Recovered,
            ..
        }
    ))
}

/// Reuse validated receipt evidence while applying committed usage mutations by revision.
/// The cache belongs to the retained read-only connection and its transaction snapshot.
fn partition_journal_receipts_cached<'a>(
    db: &Connection,
    receipts: &BTreeSet<(&str, &str, &str)>,
    members: &[(String, CostMember)],
    cache: &'a mut JournalReceiptCache,
) -> StoreResult<&'a JournalPartition> {
    let scopes = receipt_scopes(members);
    let revision: i64 = db.query_row(
        "SELECT revision FROM analytics_usage_revision_clock",
        [],
        |row| row.get(0),
    )?;
    let rebuild = cache.entry.revision.is_none_or(|old| old > revision)
        || cache.entry.scopes != scopes
        || !cache
            .entry
            .receipts
            .iter()
            .all(|(a, t, r)| receipts.contains(&(a.as_str(), t.as_str(), r.as_str())));
    let added = if rebuild {
        receipts
            .iter()
            .map(|&(a, t, r)| (a.to_owned(), t.to_owned(), r.to_owned()))
            .collect::<BTreeSet<CostReceipt>>()
    } else {
        let mut previous = cache.entry.receipts.iter().peekable();
        receipts
            .iter()
            .filter_map(|&(a, t, r)| {
                while previous
                    .peek()
                    .is_some_and(|(x, y, z)| (x.as_str(), y.as_str(), z.as_str()) < (a, t, r))
                {
                    previous.next();
                }
                if previous
                    .peek()
                    .is_some_and(|(x, y, z)| (x.as_str(), y.as_str(), z.as_str()) == (a, t, r))
                {
                    None
                } else {
                    Some((a.to_owned(), t.to_owned(), r.to_owned()))
                }
            })
            .collect()
    };
    if !rebuild && cache.entry.revision == Some(revision) && added.is_empty() {
        return Ok(&cache.entry.partition);
    }
    cache.entry.deltas = None;
    let mut updates: HashMap<String, Vec<(CostReceipt, bool)>> = HashMap::new();
    if !added.is_empty() {
        let mut statement = db.prepare_cached(JOURNAL_RECEIPT_SQL)?;
        let mut rows = statement.query(params![
            serde_json::to_string(&added)?,
            serde_json::to_string(&scopes)?
        ])?;
        while let Some(row) = rows.next()? {
            let receipt = (row.get(0)?, row.get(1)?, row.get(2)?);
            let final_evidence =
                final_receipt(row.get_ref(3)?.as_str().map_err(rusqlite::Error::from)?)?;
            let id: String = row.get(4)?;
            updates
                .entry(id.clone())
                .or_insert_with(|| {
                    if rebuild {
                        Vec::new()
                    } else {
                        cache.facts.get(&id).cloned().unwrap_or_default()
                    }
                })
                .push((receipt, final_evidence));
        }
    }
    if !rebuild && cache.entry.revision != Some(revision) {
        let mut statement = db.prepare_cached("SELECT v.id,u.agent,u.thread,json_extract(u.record,'$.responseId'),json_type(u.record,'$.studioUsage'),u.record
            FROM analytics_usage_revisions v LEFT JOIN analytics_usage u ON u.id=v.id WHERE v.revision>?1 AND v.revision<=?2")?;
        let mut rows = statement.query(params![cache.entry.revision, revision])?;
        while let Some(row) = rows.next()? {
            let id: String = row.get(0)?;
            let agent: Option<String> = row.get(1)?;
            let thread: Option<String> = row.get(2)?;
            let response: Option<String> = row.get(3)?;
            let typed: Option<String> = row.get(4)?;
            let mut facts = Vec::new();
            if let (Some(agent), Some(thread), Some(response), Some(_)) =
                (agent, thread, response, typed)
            {
                for (_, account, _) in scopes
                    .iter()
                    .filter(|(a, _, t)| a == &agent && t == &thread)
                {
                    let receipt = (account.clone(), thread.clone(), response.clone());
                    if receipts.contains(&(
                        receipt.0.as_str(),
                        receipt.1.as_str(),
                        receipt.2.as_str(),
                    )) {
                        let final_evidence = final_receipt(
                            row.get_ref(5)?.as_str().map_err(rusqlite::Error::from)?,
                        )?;
                        facts.push((receipt, final_evidence));
                    }
                }
            }
            updates.insert(id, facts);
        }
    }
    if rebuild {
        cache.facts.clear();
        cache.final_counts.clear();
        cache.entry.partition = JournalPartition {
            accepted: added.clone(),
            superseded: BTreeSet::new(),
        };
    } else {
        cache.entry.partition.accepted.extend(added);
    }
    for (id, facts) in updates {
        if let Some(previous) = cache.facts.remove(&id) {
            for (receipt, final_evidence) in previous {
                if final_evidence && let Some(count) = cache.final_counts.get_mut(&receipt) {
                    *count -= 1;
                    if *count == 0 {
                        cache.final_counts.remove(&receipt);
                        cache.entry.partition.superseded.remove(&receipt);
                        cache.entry.partition.accepted.insert(receipt);
                    }
                }
            }
        }
        if !facts.is_empty() {
            for (receipt, final_evidence) in &facts {
                if *final_evidence {
                    *cache.final_counts.entry(receipt.clone()).or_default() += 1;
                    cache.entry.partition.accepted.remove(receipt);
                    cache.entry.partition.superseded.insert(receipt.clone());
                }
            }
            cache.facts.insert(id, facts);
        }
    }
    cache.entry.scopes = scopes;
    cache.entry.receipts = receipts
        .iter()
        .map(|&(a, t, r)| (a.to_owned(), t.to_owned(), r.to_owned()))
        .collect();
    cache.entry.revision = Some(revision);
    Ok(&cache.entry.partition)
}

/// Read response evidence for exactly these agents in stable observation order.
pub fn recorded_responses(
    db: &Connection,
    agent_ids: &[String],
) -> StoreResult<Vec<RecordedResponse>> {
    recorded_responses_bounded(db, agent_ids, usize::MAX).map(|(rows, _)| rows)
}

/// Bound response decoding; the boolean reports whether additional matching rows exist.
pub fn recorded_responses_bounded(
    db: &Connection,
    agent_ids: &[String],
    limit: usize,
) -> StoreResult<(Vec<RecordedResponse>, bool)> {
    let ids = serde_json::to_string(agent_ids)?;
    let sql_limit = i64::try_from(limit.saturating_add(1)).unwrap_or(i64::MAX);
    let mut statement = db.prepare_cached(
        "SELECT agent,at,record FROM analytics_usage WHERE agent IN (SELECT value FROM json_each(?1))
         AND COALESCE(json_extract(record,'$.studioUsage.kind'),'response')='response'
         AND COALESCE(json_extract(record,'$.studioUsage.responseId'),json_extract(record,'$.responseId'),'')<>''
         ORDER BY at,seq LIMIT ?2",
    )?;
    let mut source = statement.query(params![ids, sql_limit])?;
    let mut rows = Vec::new();
    let mut inspected = 0;
    while let Some(row) = source.next()? {
        if inspected == limit {
            return Ok((rows, true));
        }
        inspected += 1;
        let value: serde_json::Value = serde_json::from_str(&row.get::<_, String>(2)?)?;
        if let DecodedUsageEvidence::Response(response) =
            decode_usage_evidence(db, row.get(0)?, row.get(1)?, &value)?
        {
            rows.push(response);
        }
    }
    Ok((rows, false))
}

enum DecodedUsageEvidence {
    Response(RecordedResponse),
    Observation(UsageObservation),
    Ignored,
}

/// Shared interpretation for the existing response reader and the bounded
/// reconciliation source window. Legacy unknown categories stay unknown.
fn decode_usage_evidence(
    db: &Connection,
    agent_id: String,
    at: f64,
    value: &serde_json::Value,
) -> StoreResult<DecodedUsageEvidence> {
    let fields = UsageCostFields::deserialize(value)?;
    let (response_id, model, tokens, evidence) = match cost_observation(db, value)? {
        Some(UsageObservation::Response {
            response_id,
            model,
            tokens,
            evidence,
            ..
        }) => (
            response_id,
            (!model.is_empty()).then_some(model),
            Some(tokens),
            evidence,
        ),
        Some(observation) => return Ok(DecodedUsageEvidence::Observation(observation)),
        None => {
            let Some(response) = studio_runtime_schema::usage::persisted::legacy_response_id(value)
            else {
                return Ok(DecodedUsageEvidence::Ignored);
            };
            (
                response.to_owned(),
                fields.model.as_deref().map(str::to_owned),
                None,
                UsageEvidence::Legacy,
            )
        }
    };
    Ok(DecodedUsageEvidence::Response(RecordedResponse {
        agent_id,
        response_id,
        model,
        tokens,
        evidence,
        at,
    }))
}

/// One diagnostic snapshot classified from a single indexed source window.
pub struct ReconciliationUsageWindow {
    pub responses: Vec<RecordedResponse>,
    pub observations: Vec<UsageObservation>,
    /// At least one source row was excluded by the shared examination budget.
    pub truncated: bool,
}

/// Examine at most `max_examined` source rows using the existing (agent,seq)
/// index, before any usage-kind filtering. One row is reserved as lookahead;
/// responses, gaps and cumulative observations share the remaining window.
pub fn reconciliation_usage_window(
    db: &Connection,
    agent: &str,
    max_examined: usize,
) -> StoreResult<ReconciliationUsageWindow> {
    let mut window = ReconciliationUsageWindow {
        responses: Vec::new(),
        observations: Vec::new(),
        truncated: max_examined == 0,
    };
    if max_examined == 0 {
        return Ok(window);
    }
    let sql_limit = i64::try_from(max_examined).unwrap_or(i64::MAX);
    let mut statement = db.prepare_cached("SELECT seq,agent,at,record FROM analytics_usage INDEXED BY analytics_usage_migration WHERE agent=?1 ORDER BY seq DESC LIMIT ?2")?;
    let mut source = statement.query(params![agent, sql_limit])?;
    let mut decoded = Vec::new();
    let mut examined = 0;
    while let Some(row) = source.next()? {
        examined += 1;
        if examined == max_examined {
            window.truncated = true;
            break;
        }
        let at: f64 = row.get(2)?;
        let sequence: i64 = row.get(0)?;
        let value: serde_json::Value = serde_json::from_str(&row.get::<_, String>(3)?)?;
        decoded.push((
            at,
            sequence,
            decode_usage_evidence(db, row.get(1)?, at, &value)?,
        ));
    }
    // Keep the prior observation ordering inside the bounded recent source
    // window, including delayed imports with earlier observation timestamps.
    decoded.sort_by(|left, right| left.0.total_cmp(&right.0).then(left.1.cmp(&right.1)));
    for (_, _, evidence) in decoded {
        match evidence {
            DecodedUsageEvidence::Response(response) => window.responses.push(response),
            DecodedUsageEvidence::Observation(observation) => window.observations.push(observation),
            DecodedUsageEvidence::Ignored => {}
        }
    }
    Ok(window)
}

/// Group identity; field order is the summation order of a priced total.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct GroupKey {
    model: Option<String>,
    account: String,
    input: Option<u64>,
    cached: Option<u64>,
    cache_write: Option<u64>,
    cache_write_1h: Option<u64>,
    output: Option<u64>,
    uncached: bool,
}

fn amount_key(value: Option<f64>) -> Option<u64> {
    value.map(|number| if number == 0.0 { 0.0_f64 } else { number }.to_bits())
}

fn amount(key: Option<u64>) -> Option<f64> {
    key.map(f64::from_bits)
}

/// The stored cost facts of one usage row.
#[derive(Clone, Debug)]
struct CostRow {
    id: String,
    agent: String,
    thread: Option<String>,
    turn: Option<String>,
    seq: i64,
    at: f64,
    model: Option<String>,
    account_key: Option<String>,
    response: Option<String>,
    tokens: CostTokens,
    uncached: bool,
    /// The contribution currently added to `analytics_cost_groups`.
    counted: Option<Option<String>>,
    /// NULL preserves pre-contract derived counting; new rows explicitly mark response eligibility.
    eligible: Option<bool>,
    final_evidence: bool,
}

impl CostRow {
    fn from_sql(row: &Row<'_>) -> rusqlite::Result<Self> {
        Self::from_sql_at(row, 0)
    }

    /// Decode [`ROW_COLUMNS`] starting at column `base`.
    fn from_sql_at(row: &Row<'_>, base: usize) -> rusqlite::Result<Self> {
        Ok(Self {
            eligible: row.get(base + 16)?,
            final_evidence: row.get(base + 17)?,
            id: row.get(base)?,
            agent: row.get(base + 1)?,
            thread: row.get(base + 2)?,
            turn: row.get(base + 3)?,
            seq: row.get(base + 4)?,
            at: row.get(base + 5)?,
            model: row.get(base + 6)?,
            account_key: row.get(base + 7)?,
            response: row.get(base + 8)?,
            tokens: CostTokens {
                input_tokens: row.get(base + 9)?,
                cached_input_tokens: row.get(base + 10)?,
                cache_write_input_tokens: row.get(base + 11)?,
                cache_write_input_1h_tokens: row.get(base + 18)?,
                output_tokens: row.get(base + 12)?,
            },
            uncached: row.get(base + 13)?,
            counted: row
                .get::<_, bool>(base + 14)?
                .then(|| row.get(base + 15))
                .transpose()?,
        })
    }

    fn group_key(&self, model: Option<String>) -> GroupKey {
        GroupKey {
            model,
            account: self
                .account_key
                .clone()
                .unwrap_or_else(|| DEFAULT_ACCOUNT.to_owned()),
            input: amount_key(self.tokens.input_tokens),
            cached: amount_key(self.tokens.cached_input_tokens),
            cache_write: amount_key(self.tokens.cache_write_input_tokens),
            cache_write_1h: amount_key(self.tokens.cache_write_input_1h_tokens),
            output: amount_key(self.tokens.output_tokens),
            uncached: self.uncached,
        }
    }
}

/// The contribution of every row of a closed scope that `included` keeps:
/// `None` when the row is not counted, else the model it is counted under.
fn contributions(
    rows: &[CostRow],
    included: impl Fn(&CostRow) -> bool,
) -> Vec<Option<Option<String>>> {
    // Identity is (agent, response), across turns/threads and legacy key versions.
    let mut winners = HashMap::<(&str, &str), &CostRow>::new();
    for row in rows
        .iter()
        .filter(|row| included(row) && row.eligible == Some(true))
    {
        let Some(response) = row.response.as_deref() else {
            continue;
        };
        let candidate = winners.entry((&row.agent, response)).or_insert(row);
        if studio_runtime_schema::usage::ledger::supersedes(
            if candidate.final_evidence {
                UsageEvidence::Live
            } else {
                UsageEvidence::Legacy
            },
            if row.final_evidence {
                UsageEvidence::Live
            } else {
                UsageEvidence::Legacy
            },
        ) || (row.final_evidence == candidate.final_evidence && row.seq < candidate.seq)
        {
            *candidate = row;
        }
    }
    let responded_turns = rows
        .iter()
        .filter(|row| included(row) && row.eligible != Some(false) && row.response.is_some())
        .map(|row| {
            (
                row.agent.as_str(),
                row.thread.as_deref(),
                row.turn.as_deref(),
            )
        })
        .collect::<HashSet<_>>();
    let suppressed = |row: &CostRow| {
        match row.eligible {
            Some(false) => true,
            Some(true) => row.response.as_deref().is_none_or(|response| {
                winners
                    .get(&(row.agent.as_str(), response))
                    .is_none_or(|winner| winner.id != row.id)
            }),
            // NULL marks a pre-contract derived row: preserve its old interpretation
            // until new response evidence supersedes it.
            None => match row.response.as_deref() {
                Some(response) => winners.contains_key(&(row.agent.as_str(), response)),
                None => responded_turns.contains(&(
                    row.agent.as_str(),
                    row.thread.as_deref(),
                    row.turn.as_deref(),
                )),
            },
        }
    };
    let mut turn_models = HashMap::<(&str, Option<&str>), (f64, i64, &str)>::new();
    let mut thread_models = HashMap::<(&str, Option<&str>), (f64, i64, &str)>::new();
    if rows.iter().any(|row| row.model.is_none()) {
        for row in rows.iter().filter(|row| included(row) && !suppressed(row)) {
            let Some(model) = row.model.as_deref() else {
                continue;
            };
            for (models, key) in [
                (&mut turn_models, (row.agent.as_str(), row.turn.as_deref())),
                (
                    &mut thread_models,
                    (row.agent.as_str(), row.thread.as_deref()),
                ),
            ] {
                if models
                    .get(&key)
                    .is_none_or(|current| (row.at, row.seq) > (current.0, current.1))
                {
                    models.insert(key, (row.at, row.seq, model));
                }
            }
        }
    }
    rows.iter()
        .map(|row| {
            if !included(row) || suppressed(row) {
                return None;
            }
            let model = row.model.clone().or_else(|| {
                turn_models
                    .get(&(row.agent.as_str(), row.turn.as_deref()))
                    .or_else(|| thread_models.get(&(row.agent.as_str(), row.thread.as_deref())))
                    .map(|(_, _, model)| (*model).to_owned())
            });
            Some(model)
        })
        .collect()
}

/// Create the aggregate on first open and fill it from the existing usage
/// rows, in one transaction.
#[cfg(test)]
fn ensure_cost_aggregate(path: &Path) -> StoreResult<()> {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let tx = db.transaction()?;
    crate::migrations::m2026_10_22_analytics::migrate_2026_10_22_drop_python_cost_projection(&tx)?;
    crate::migrations::m2026_10_22_analytics::aggregate_columns(&tx)?;
    install(&tx)?;
    tx.commit()?;
    Ok(())
}

/// Install the current derived aggregate. Historical layouts are upgraded only
/// by the dated migration before this current-format owner runs.
pub fn install(db: &Connection) -> StoreResult<()> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_cost_rows')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        db.execute_batch(TABLES)?;
        backfill(db)?;
    }
    db.execute_batch(SCOPE_INDEXES)?;
    db.execute_batch(crate::migrations::m2026_10_27_cost_index::INDEX)?;
    Ok(())
}

/// Fill the aggregate from every usage row with a root: load the facts in
/// storage order (a sequential read of the large usage records), index them,
/// then count each (root, agent) scope.
fn backfill(db: &Connection) -> StoreResult<()> {
    let mut source = db.prepare(
        "SELECT id,root,agent,thread,turn,seq,at,record FROM analytics_usage WHERE root IS NOT NULL",
    )?;
    let mut rows = source.query([])?;
    while let Some(source_row) = rows.next()? {
        let raw = source_row
            .get_ref(7)?
            .as_str()
            .map_err(rusqlite::Error::from)?;
        let record: serde_json::Value = serde_json::from_str(raw)?;
        let fields = UsageCostFields::deserialize(&record)?;
        let observation = cost_observation(db, &record)?;
        let root: String = source_row.get(1)?;
        let row = cost_row(
            source_row.get(0)?,
            source_row.get(2)?,
            source_row.get(3)?,
            source_row.get(4)?,
            source_row.get(5)?,
            source_row.get(6)?,
            &fields,
            observation.as_ref(),
        );
        insert_row(db, &root, &row)?;
    }
    db.execute_batch(INDEXES)?;

    let mut groups = BTreeMap::<(String, GroupKey), i64>::new();
    let mut count_scope = |root: &str, scope: &[CostRow]| -> rusqlite::Result<()> {
        for (row, contribution) in scope.iter().zip(contributions(scope, |_| true)) {
            let Some(model) = contribution else {
                continue;
            };
            db.execute_cached(
                "UPDATE analytics_cost_rows SET counted=1,counted_model=?2 WHERE id=?1",
                params![row.id, model],
            )?;
            *groups
                .entry((root.to_owned(), row.group_key(model)))
                .or_default() += 1;
        }
        Ok(())
    };
    let mut statement = db.prepare(&format!(
        "SELECT root,{ROW_COLUMNS} FROM analytics_cost_rows ORDER BY root,agent"
    ))?;
    let mut facts = statement.query([])?;
    let mut scope_root = String::new();
    let mut scope = Vec::<CostRow>::new();
    while let Some(fact) = facts.next()? {
        let root = fact.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
        let row = CostRow::from_sql_at(fact, 1)?;
        if root != scope_root || scope.first().is_some_and(|first| first.agent != row.agent) {
            count_scope(&scope_root, &scope)?;
            scope.clear();
            root.clone_into(&mut scope_root);
        }
        scope.push(row);
    }
    count_scope(&scope_root, &scope)?;
    for ((root, key), count) in groups {
        db.execute_cached(
            "INSERT INTO analytics_cost_groups VALUES (?,?,?,?,?,?,?,?,?,?)",
            params![
                root,
                key.model,
                key.account,
                amount(key.input),
                amount(key.cached),
                amount(key.cache_write),
                amount(key.output),
                key.uncached,
                count,
                amount(key.cache_write_1h)
            ],
        )?;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cost_row(
    id: String,
    agent: String,
    thread: Option<String>,
    turn: Option<String>,
    seq: i64,
    at: f64,
    fields: &UsageCostFields<'_>,
    observation: Option<&UsageObservation>,
) -> CostRow {
    let response = match observation {
        Some(UsageObservation::Response {
            tokens, evidence, ..
        }) => Some((tokens, evidence)),
        _ => None,
    };
    CostRow {
        eligible: Some(response.is_some()),
        final_evidence: response.is_some_and(|(_, evidence)| *evidence != UsageEvidence::Legacy),
        id,
        agent,
        thread,
        turn,
        seq,
        at,
        model: match observation {
            Some(UsageObservation::Response { model, .. }) if !model.is_empty() => {
                Some(model.clone())
            }
            _ => fields.model.as_deref().map(str::to_owned),
        },
        account_key: fields.account_key.as_deref().map(str::to_owned),
        response: match observation {
            Some(UsageObservation::Response { response_id, .. }) => Some(response_id.clone()),
            _ => fields
                .response_id
                .as_deref()
                .filter(|id| !id.is_empty())
                .map(str::to_owned),
        },
        tokens: response
            .map(|(tokens, _)| CostTokens::from(tokens))
            .unwrap_or_default(),
        uncached: true,
        counted: None,
    }
}

fn insert_row(db: &Connection, root: &str, row: &CostRow) -> rusqlite::Result<usize> {
    db.execute_cached(
        "INSERT INTO analytics_cost_rows VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
        params![
            row.id,
            root,
            row.agent,
            row.thread,
            row.turn,
            row.seq,
            row.at,
            row.model,
            row.account_key,
            row.response,
            row.tokens.input_tokens,
            row.tokens.cached_input_tokens,
            row.tokens.cache_write_input_tokens,
            row.tokens.output_tokens,
            row.uncached,
            row.counted.is_some(),
            row.counted.clone().flatten(),
            row.eligible,
            row.final_evidence,
            row.tokens.cache_write_input_1h_tokens
        ],
    )
}

/// Add `delta` rows to one group of `root`, deleting a group that reaches zero.
fn add_to_group(db: &Connection, root: &str, key: &GroupKey, delta: i64) -> rusqlite::Result<()> {
    const MATCH: &str = "root=?2 AND model IS ?3 AND account=?4 AND input IS ?5 AND cached IS ?6 \
        AND cache_write IS ?7 AND output IS ?8 AND uncached=?9 AND cache_write_1h IS ?10";
    let values = params![
        delta,
        root,
        key.model,
        key.account,
        amount(key.input),
        amount(key.cached),
        amount(key.cache_write),
        amount(key.output),
        key.uncached,
        amount(key.cache_write_1h)
    ];
    let updated = db.execute_cached(
        &format!("UPDATE analytics_cost_groups SET count=count+?1 WHERE {MATCH}"),
        values,
    )?;
    if updated == 0 {
        db.execute_cached(
            "INSERT INTO analytics_cost_groups VALUES (?2,?3,?4,?5,?6,?7,?8,?9,?1,?10)",
            values,
        )?;
    } else if delta < 0 {
        db.execute_cached(
            &format!("DELETE FROM analytics_cost_groups WHERE count<=0 AND ?1<0 AND {MATCH}"),
            values,
        )?;
    }
    Ok(())
}

/// Changed turn and response identities; duplicate responses can span legacy turns.
type TurnScopes = BTreeSet<(Option<String>, Option<String>, Option<String>)>;

/// The rows whose contributions a change in `turns` of `agent` in `root` can
/// affect: changed turns and duplicate response identities, or the whole agent for model-less rows.
fn scope_rows(
    db: &Connection,
    root: &str,
    agent: &str,
    turns: &TurnScopes,
) -> rusqlite::Result<Vec<CostRow>> {
    let unmodeled: bool = db.query_row_cached("SELECT EXISTS(SELECT 1 FROM analytics_cost_rows WHERE root=?1 AND agent=?2 AND model IS NULL AND eligible IS NOT 0)",params![root,agent],|row|row.get(0))?;
    if unmodeled {
        return db
            .prepare_cached(&format!(
                "SELECT {ROW_COLUMNS} FROM analytics_cost_rows WHERE root=?1 AND agent=?2"
            ))?
            .query_map(params![root, agent], CostRow::from_sql)?
            .collect();
    }
    if turns.is_empty() {
        return Ok(Vec::new());
    }
    // Every selected response must bring all its counterparts, including responses
    // merely sharing the touched turn; otherwise a suppressed legacy row can revive.
    // One statement serves every touched turn: each turn seeks the scope index and
    // each response seeks the agent/response index, so no turn repeats an agent scan.
    let requested = serde_json::to_string(turns)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let mut statement = db.prepare_cached(&format!(
        "WITH requested(thread,turn,response) AS MATERIALIZED (
            SELECT json_extract(value,'$[0]'),json_extract(value,'$[1]'),
                   json_extract(value,'$[2]') FROM json_each(?3)
         ), touched(id,response) AS MATERIALIZED (
            SELECT c.id,c.response FROM requested t CROSS JOIN analytics_cost_rows c
            WHERE c.root=?1 AND c.agent=?2 AND c.thread IS t.thread AND c.turn IS t.turn
         ), affected(response) AS MATERIALIZED (
            SELECT response FROM touched WHERE response IS NOT NULL
            UNION SELECT response FROM requested WHERE response IS NOT NULL
         )
         SELECT {ROW_COLUMNS} FROM analytics_cost_rows WHERE id IN (SELECT id FROM touched)
         UNION ALL SELECT {ROW_COLUMNS} FROM analytics_cost_rows WHERE root=?1 AND agent=?2
         AND response IN (SELECT response FROM affected)"
    ))?;
    let mut rows = BTreeMap::new();
    for row in statement.query_map(params![root, agent, requested], CostRow::from_sql)? {
        let row = row?;
        rows.insert(row.id.clone(), row);
    }
    Ok(rows.into_values().collect())
}

/// Recompute the scopes of `turns` and move every changed contribution between groups.
fn refresh_scope(
    db: &Connection,
    root: &str,
    agent: &str,
    turns: &TurnScopes,
) -> rusqlite::Result<()> {
    let rows = scope_rows(db, root, agent, turns)?;
    for (row, contribution) in rows.iter().zip(contributions(&rows, |_| true)) {
        if row.counted == contribution {
            continue;
        }
        if let Some(model) = &row.counted {
            add_to_group(db, root, &row.group_key(model.clone()), -1)?;
        }
        if let Some(model) = &contribution {
            add_to_group(db, root, &row.group_key(model.clone()), 1)?;
        }
        db.execute_cached(
            "UPDATE analytics_cost_rows SET counted=?2,counted_model=?3 WHERE id=?1",
            params![
                row.id,
                contribution.is_some(),
                contribution.clone().flatten()
            ],
        )?;
    }
    Ok(())
}

/// Bring the aggregate up to date with the `analytics_usage` row `id` that the
/// caller just upserted from `record` in this transaction.
pub(crate) fn apply_usage_write(
    db: &Connection,
    id: &str,
    record: &serde_json::Value,
) -> StoreResult<()> {
    let fields = UsageCostFields::deserialize(record)?;
    let observation = cost_observation(db, record)?;
    let (root, agent, thread, turn, seq, at) = db.query_row_cached(
        "SELECT root,agent,thread,turn,seq,at FROM analytics_usage WHERE id=?1",
        [id],
        |row| {
            Ok((
                row.get::<_, Option<String>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, f64>(5)?,
            ))
        },
    )?;
    let previous = db
        .query_row_cached(
            &format!("SELECT root,{ROW_COLUMNS} FROM analytics_cost_rows WHERE id=?1"),
            [id],
            |row| Ok((row.get::<_, String>(0)?, CostRow::from_sql_at(row, 1)?)),
        )
        .optional()?;
    if let Some((previous_root, previous)) = &previous {
        if let Some(model) = &previous.counted {
            add_to_group(db, previous_root, &previous.group_key(model.clone()), -1)?;
        }
        db.execute_cached("DELETE FROM analytics_cost_rows WHERE id=?1", [id])?;
    }
    let mut scopes = BTreeMap::<(String, String), TurnScopes>::new();
    if let Some(root) = &root {
        let row = cost_row(
            id.to_owned(),
            agent,
            thread,
            turn,
            seq,
            at,
            &fields,
            observation.as_ref(),
        );
        insert_row(db, root, &row)?;
        scopes
            .entry((root.clone(), row.agent))
            .or_default()
            .insert((row.thread, row.turn, row.response));
    }
    if let Some((previous_root, previous)) = previous {
        scopes
            .entry((previous_root, previous.agent))
            .or_default()
            .insert((previous.thread, previous.turn, previous.response));
    }
    for ((root, agent), turns) in scopes {
        refresh_scope(db, &root, &agent, &turns)?;
    }
    Ok(())
}

/// The counted groups of `root` in summation order, without the analytics rows
/// that the accepted journal partition replaces (each removal re-applies the
/// counting rules to the rows it affects). Reuse the same partition to filter journal costs.
pub fn cost_groups(
    db: &Connection,
    root: &str,
    partition: &JournalPartition,
) -> Result<Vec<CostGroup>, StoreError> {
    let mut groups = db
        .prepare_cached(
            "SELECT model,account,input,cached,cache_write,output,uncached,count,cache_write_1h
             FROM analytics_cost_groups WHERE root=?1",
        )?
        .query_map([root], |row| {
            Ok((
                GroupKey {
                    model: row.get(0)?,
                    account: row.get(1)?,
                    input: amount_key(row.get(2)?),
                    cached: amount_key(row.get(3)?),
                    cache_write: amount_key(row.get(4)?),
                    cache_write_1h: amount_key(row.get(8)?),
                    output: amount_key(row.get(5)?),
                    uncached: row.get(6)?,
                },
                row.get::<_, i64>(7)?,
            ))
        })?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    for (key, delta) in journal_group_deltas(db, root, partition)? {
        *groups.entry(key).or_default() += delta;
    }
    Ok(groups
        .into_iter()
        .filter(|(_, count)| *count > 0)
        .map(|(key, count)| CostGroup {
            model: key.model,
            account_key: key.account,
            tokens: CostTokens {
                input_tokens: amount(key.input),
                cached_input_tokens: amount(key.cached),
                cache_write_input_tokens: amount(key.cache_write),
                cache_write_input_1h_tokens: amount(key.cache_write_1h),
                output_tokens: amount(key.output),
            },
            input_uncached: key.uncached,
            count: count as u64,
        })
        .collect())
}

/// Original and journal-adjusted groups for identities affected by receipts.
pub type CostGroupAdjustments = Vec<(Option<CostGroup>, Option<CostGroup>)>;

pub fn cost_group_adjustments(
    db: &Connection,
    root: &str,
    partition: &JournalPartition,
) -> StoreResult<CostGroupAdjustments> {
    let deltas = journal_group_deltas(db, root, partition)?;
    adjustments_for_deltas(db, root, &deltas)
}

/// Partition receipts and use the delta tied to that exact cache marker.
pub fn journal_partition_and_adjustments_cached<'a>(
    db: &Connection,
    root: &str,
    receipts: &BTreeSet<(&str, &str, &str)>,
    members: &[(String, CostMember)],
    cache: &'a mut JournalReceiptCache,
) -> StoreResult<(&'a JournalPartition, CostGroupAdjustments)> {
    partition_journal_receipts_cached(db, receipts, members, cache)?;
    if cache.entry.root.as_deref() != Some(root) {
        cache.entry.root = Some(root.to_owned());
        cache.entry.deltas = None;
    }
    if cache.entry.deltas.is_none() {
        cache.entry.deltas = Some(journal_group_deltas(db, root, &cache.entry.partition)?);
    }
    let deltas = cache.entry.deltas.as_ref().expect("calculated deltas");
    Ok((
        &cache.entry.partition,
        adjustments_for_deltas(db, root, deltas)?,
    ))
}

fn adjustments_for_deltas(
    db: &Connection,
    root: &str,
    deltas: &BTreeMap<GroupKey, i64>,
) -> StoreResult<CostGroupAdjustments> {
    let mut result = Vec::new();
    let mut query = db.prepare_cached("SELECT count FROM analytics_cost_groups WHERE root=?1 AND model IS ?2 AND account=?3 AND input IS ?4 AND cached IS ?5 AND cache_write IS ?6 AND output IS ?7 AND uncached=?8 AND cache_write_1h IS ?9")?;
    for (key, &delta) in deltas {
        if delta == 0 {
            continue;
        }
        let count = query
            .query_row(
                params![
                    root,
                    key.model,
                    key.account,
                    amount(key.input),
                    amount(key.cached),
                    amount(key.cache_write),
                    amount(key.output),
                    key.uncached,
                    amount(key.cache_write_1h)
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0);
        let group = |count: i64| {
            (count > 0).then(|| CostGroup {
                model: key.model.clone(),
                account_key: key.account.clone(),
                tokens: CostTokens {
                    input_tokens: amount(key.input),
                    cached_input_tokens: amount(key.cached),
                    cache_write_input_tokens: amount(key.cache_write),
                    cache_write_input_1h_tokens: amount(key.cache_write_1h),
                    output_tokens: amount(key.output),
                },
                input_uncached: key.uncached,
                count: count as u64,
            })
        };
        result.push((group(count), group(count + delta)));
    }
    Ok(result)
}

fn journal_group_deltas(
    db: &Connection,
    root: &str,
    partition: &JournalPartition,
) -> StoreResult<BTreeMap<GroupKey, i64>> {
    #[cfg(test)]
    JOURNAL_DELTA_RUNS.update(|count| count + 1);
    let mut changes = BTreeMap::<GroupKey, i64>::new();
    let mut replaced = HashSet::new();
    let mut scopes = BTreeMap::<String, TurnScopes>::new();
    for (account, thread, response) in &partition.accepted {
        let mut statement = db.prepare_cached(&format!(
            "SELECT {ROW_COLUMNS} FROM analytics_cost_rows
             WHERE root=?1 AND thread=?2 AND response=?3 AND account_key=?4"
        ))?;
        for row in
            statement.query_map(params![root, thread, response, account], CostRow::from_sql)?
        {
            let row = row?;
            scopes
                .entry(row.agent)
                .or_default()
                .insert((row.thread, row.turn, row.response));
            replaced.insert(row.id);
        }
    }
    for (agent, turns) in &scopes {
        let rows = scope_rows(db, root, agent, turns)?;
        let kept = contributions(&rows, |row| !replaced.contains(&row.id));
        for (row, contribution) in rows.iter().zip(kept) {
            if row.counted == contribution {
                continue;
            }
            if let Some(model) = &row.counted {
                *changes.entry(row.group_key(model.clone())).or_default() -= 1;
            }
            if let Some(model) = contribution {
                *changes.entry(row.group_key(model)).or_default() += 1;
            }
        }
    }
    Ok(changes)
}

#[cfg(test)]
mod tests {
    fn borrowed_receipts(
        receipts: &std::collections::BTreeSet<super::CostReceipt>,
    ) -> std::collections::BTreeSet<(&str, &str, &str)> {
        receipts
            .iter()
            .map(|(a, t, r)| (a.as_str(), t.as_str(), r.as_str()))
            .collect()
    }

    fn partition_journal_receipts_cached<'a>(
        db: &rusqlite::Connection,
        receipts: &std::collections::BTreeSet<super::CostReceipt>,
        members: &[(String, super::CostMember)],
        cache: &'a mut super::JournalReceiptCache,
    ) -> crate::StoreResult<&'a super::JournalPartition> {
        super::partition_journal_receipts_cached(db, &borrowed_receipts(receipts), members, cache)
    }
    fn partition_journal_receipts(
        db: &rusqlite::Connection,
        receipts: &std::collections::BTreeSet<super::CostReceipt>,
        members: &[(String, super::CostMember)],
    ) -> crate::StoreResult<super::JournalPartition> {
        partition_journal_receipts_cached(db, receipts, members, &mut Default::default()).cloned()
    }
    #[tokio::test]
    async fn provider_identity_orphan_legacy_cost_is_invalid_but_typed_fact_is_preserved() {
        use serde_json::json;
        let root = tempfile::tempdir().expect("state");
        let store = crate::RuntimeStore::open(Some(root.path()))
            .await
            .expect("store");
        store.analytics_transaction::<_, crate::StoreError>(|tx, _| {
            let mut row = json!({"id":"response","agentId":"orphan","rootId":"orphan","threadId":"thread","at":1,"responseId":"response","last":{"inputTokens":12,"cachedInputTokens":3,"cacheWriteInputTokens":2,"outputTokens":7},"total":{}});
            assert!(super::cost_observation(tx, &row).is_err());
            row["studioUsage"] = json!({"kind":"response","responseId":"response","model":"model","evidence":"live","tokens":{"uncachedInput":12,"cacheReadInput":3,"cacheWriteInput":2,"output":7}});
            assert!(matches!(super::cost_observation(tx, &row)?, Some(studio_runtime_schema::usage::UsageObservation::Response { .. })));
            Ok(())
        }).await.expect("orphan evidence boundary");
    }

    #[test]
    fn provider_identity_cost_members_reject_unknown_and_malformed_providers() {
        use serde_json::json;
        for provider in [json!("future"), json!(""), json!(7), json!({})] {
            assert!(
                serde_json::from_value::<super::CostMember>(json!({"provider":provider})).is_err()
            );
            assert!(
                serde_json::from_value::<super::CostMember>(
                    json!({"accountHistory":[{"provider":provider}]})
                )
                .is_err()
            );
        }
        for provider in [json!("codex"), json!("claude")] {
            assert!(
                serde_json::from_value::<super::CostMember>(json!({"provider":provider})).is_ok()
            );
        }
        assert!(serde_json::from_value::<super::CostMember>(json!({})).is_err());
        assert!(serde_json::from_value::<super::CostMember>(json!({"provider":null})).is_err());
    }
    use super::{JournalPartition, ensure_cost_aggregate};
    use crate::analytics_capture::{AnalyticsCaptureRepository, CaptureTable};
    use crate::{RuntimeStore, StoreError};
    use rusqlite::Connection;
    use serde_json::json;
    use std::path::Path;

    fn scope_fixture(size: usize) -> Connection {
        use serde::Deserialize;
        let db = Connection::open_in_memory().expect("cost facts");
        db.execute_batch(super::TABLES).expect("tables");
        db.execute_batch(super::INDEXES).expect("indexes");
        db.execute_batch(super::SCOPE_INDEXES)
            .expect("scope indexes");
        let value = json!({"model":"m"});
        let fields = super::UsageCostFields::deserialize(&value).expect("fields");
        for index in 0..size {
            let mut row = super::cost_row(
                index.to_string(),
                "agent".into(),
                Some("thread".into()),
                Some(index.to_string()),
                index as i64,
                index as f64,
                &fields,
                None,
            );
            row.eligible = Some(true);
            row.response = Some(format!("r{index}"));
            super::insert_row(&db, "root", &row).expect("fact");
        }
        db
    }

    #[test]
    fn typed_usage_scope_ignores_ineligible_unmodeled_history() {
        let db = scope_fixture(8);
        db.execute(
            "UPDATE analytics_cost_rows SET model=NULL,eligible=0 WHERE id='7'",
            [],
        )
        .expect("non-response observation");
        let turns =
            super::TurnScopes::from([(Some("thread".into()), Some("0".into()), Some("r0".into()))]);
        let rows = super::scope_rows(&db, "root", "agent", &turns).expect("affected facts");
        assert_eq!(
            rows.len(),
            1,
            "an ineligible observation cannot widen a response refresh"
        );
        db.execute(
            "UPDATE analytics_cost_rows SET eligible=NULL WHERE id='7'",
            [],
        )
        .expect("migrated row");
        let migrated =
            super::scope_rows(&db, "root", "agent", &turns).expect("legacy model inference");
        assert_eq!(
            migrated.len(),
            8,
            "migrated model-less rows still depend on the agent history"
        );
        let kept = super::contributions(&migrated, |_| true);
        let unmodeled = migrated
            .iter()
            .position(|row| row.id == "7")
            .expect("migrated row");
        assert_eq!(kept[unmodeled], Some(Some("m".into())));
    }

    #[test]
    fn typed_usage_scope_seeks_turn_and_cross_thread_response() {
        use std::sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        };
        let mut measured = Vec::new();
        for size in [4, 8, 16] {
            let db = scope_fixture(size);
            db.execute("UPDATE analytics_cost_rows SET thread='old',turn='old',response='r0',final_evidence=1 WHERE id='1'", [])
                .expect("cross-thread counterpart");
            db.execute(
                "UPDATE analytics_cost_rows SET model=NULL,eligible=0 WHERE seq>=2",
                [],
            )
            .expect("unrelated cumulative observations");
            let calls = Arc::new(AtomicU64::new(0));
            let counter = Arc::clone(&calls);
            db.progress_handler(
                1,
                Some(move || {
                    counter.fetch_add(1, Ordering::Relaxed);
                    false
                }),
            )
            .expect("VM counter");
            let turns = super::TurnScopes::from([(Some("thread".into()), Some("0".into()), None)]);
            let rows = super::scope_rows(&db, "root", "agent", &turns).expect("closed scope");
            db.progress_handler(1, None::<fn() -> bool>)
                .expect("remove counter");
            assert_eq!(
                rows.len(),
                2,
                "turn selection closes over cross-thread response counterparts"
            );
            assert_eq!(
                super::contributions(&rows, |_| true),
                vec![None, Some(Some("m".into()))]
            );
            measured.push(calls.load(Ordering::Relaxed));
        }
        assert_eq!(
            measured[0], measured[2],
            "unrelated turns must not add SQLite VM work: {measured:?}"
        );
    }

    #[tokio::test]
    async fn typed_usage_scope_refreshes_an_unchanged_identity_once() {
        thread_local! { static SCOPES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            let mut record = json!({"id":"response","agentId":"agent","rootId":"root","threadId":"thread","turnId":"turn","at":1,
                "last":{},"total":{},"studioUsage":{"kind":"response","responseId":"response","model":"m","evidence":"legacy",
                    "tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}}});
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &record)?;
            SCOPES.set(0);
            tx.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, Some(|event| {
                if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event
                    && sql.contains("WITH requested(thread,turn,response)") {
                    SCOPES.set(SCOPES.get() + 1);
                }
            }));
            record["studioUsage"]["evidence"] = json!("live");
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &record)?;
            tx.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
            assert_eq!(SCOPES.get(), 1, "the same old/new scope must be recomputed once");
            assert_eq!(super::cost_groups(tx, "root", &Default::default())?.iter().map(|group| group.count).sum::<u64>(), 1);
            Ok(())
        }).await.expect("coalesced refresh");
    }

    #[tokio::test]
    async fn typed_usage_receipt_lookup_excludes_unrelated_agents() {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            let member = json!({"provider":"claude","accountKey":"new","threadId":"new-thread","accountHistory":[{"provider":"claude","accountKey":"acct","threadId":"thread"}]});
            tx.execute("INSERT INTO analytics_agents(id,record) VALUES('agent',?1)", [member.to_string()])?;
            tx.execute("INSERT INTO analytics_agents(id,record) VALUES('unrelated',?1)", [json!({"accountHistory":["invalid history outside the selected tree"]}).to_string()])?;
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &json!({
                "id":"final","agentId":"agent","rootId":"root","threadId":"thread","at":1,
                "responseId":"final","accountKey":"acct","last":{},"total":{},
                "studioUsage":{"kind":"response","responseId":"final","model":"m","evidence":"live",
                    "tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}}
            }))?;
            let receipts = ["final", "missing"].map(|id| ("acct".to_owned(), "thread".to_owned(), id.to_owned())).into();
            let members = [("agent".to_owned(), serde_json::from_value(member)?)];
            let partition = partition_journal_receipts(tx, &receipts, &members)?;
            assert_eq!(partition.accepted.len(), 1);
            assert_eq!(partition.superseded.len(), 1, "historical account scope preserves final authority");
            Ok(())
        }).await.expect("selected receipt lookup must not decode unrelated agent records");
    }

    fn receipt_members() -> [(String, super::CostMember); 1] {
        [(
            "agent".to_owned(),
            super::CostMember {
                provider: super::ProviderKind::Claude,
                account_key: Some("acct".into()),
                thread_id: Some("thread".into()),
                account_history: None,
            },
        )]
    }

    #[tokio::test]
    async fn typed_usage_receipt_scope_batches_modeled_turns_without_repeated_agent_scans() {
        use std::collections::BTreeSet;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicUsize, Ordering};

        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store
            .analytics_transaction::<_, StoreError>(|tx, _| {
                let write = |id: &str, root: &str, agent: &str, account: &str, index: usize, input: u64| {
                    AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &json!({
                        "id":id,"agentId":agent,"rootId":root,"threadId":"thread",
                        "turnId":format!("turn-{index}"),"at":1,"accountKey":account,
                        "responseId":format!("r-{index}"),"model":"m","last":{},"total":{},
                        "studioUsage":{"kind":"response","responseId":format!("r-{index}"),
                            "model":"m","evidence":"legacy","tokens":{
                                "uncachedInput":input,"cacheReadInput":3,"cacheWriteInput":2,"output":1}}
                    }))
                };
                for index in 0..16 {
                    write(&format!("row-{index}"), "root", "agent", "acct", index, 10)?;
                }
                let scope = |index| {
                    (Some("thread".to_owned()), Some(format!("turn-{index}")), Some(format!("r-{index}")))
                };
                let steps = Arc::new(AtomicUsize::new(0));
                let observed = Arc::clone(&steps);
                tx.progress_handler(1, Some(move || {
                    observed.fetch_add(1, Ordering::Relaxed);
                    false
                })).expect("install fixture VM-step counter");
                let measure = |steps: &AtomicUsize| -> rusqlite::Result<(Vec<super::CostRow>, usize)> {
                    steps.store(0, Ordering::Relaxed);
                    let rows = super::scope_rows(tx, "root", "agent", &(0..16).map(scope).collect())?;
                    Ok((rows, steps.load(Ordering::Relaxed)))
                };
                let (all, batch_steps) = measure(&steps)?;
                let single = super::scope_rows(tx, "root", "agent", &BTreeSet::from([scope(0)]))?;
                assert_eq!(single.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(), vec!["row-0"]);
                assert_eq!(all.iter().map(|row| row.id.clone()).collect::<BTreeSet<_>>(),
                    (0..16).map(|index| format!("row-{index}")).collect());

                // Receipt replacement must retain another account's contribution and another root.
                write("foreign-account", "root", "foreign-agent", "foreign", 0, 20)?;
                write("other-root", "other-root", "agent", "acct", 0, 30)?;
                for (agent, account) in [("agent", "acct"), ("foreign-agent", "foreign")] {
                    tx.execute("INSERT INTO analytics_agents(id,record) VALUES(?1,?2)",
                        rusqlite::params![agent, json!({"id":agent,"accountKey":account,"threadId":"thread"}).to_string()])?;
                }
                let groups = super::cost_groups(tx, "root", &JournalPartition {
                    accepted: BTreeSet::from([
                        ("acct".to_owned(), "thread".to_owned(), "r-0".to_owned())
                    ]),
                    superseded: BTreeSet::new(),
                })?;
                assert_eq!(groups.len(), 2);
                let own = groups.iter().find(|group| group.account_key == "acct").expect("own account");
                let foreign = groups.iter().find(|group| group.account_key == "foreign").expect("other account");
                assert_eq!(own.count, 15);
                assert_eq!(foreign.count, 1);
                assert_eq!(own.tokens, super::CostTokens {
                    input_tokens: Some(10.0), cached_input_tokens: Some(3.0),
                    cache_write_input_tokens: Some(2.0), cache_write_input_1h_tokens: None,
                    output_tokens: Some(1.0),
                });
                assert_eq!(foreign.tokens.input_tokens, Some(20.0));
                let other_root = super::cost_groups(tx, "other-root", &Default::default())?;
                assert_eq!(other_root.len(), 1);
                assert_eq!(other_root[0].count, 1);
                assert_eq!(other_root[0].tokens.input_tokens, Some(30.0));
                // Untouched turns of the same agent must not add work to the 16 touched turns.
                for index in 16..64 {
                    write(&format!("row-{index}"), "root", "agent", "acct", index, 10)?;
                }
                let (larger, larger_steps) = measure(&steps)?;
                tx.progress_handler(0, None::<fn() -> bool>).expect("remove fixture VM-step counter");
                assert_eq!(larger.len(), 16);
                assert!(batch_steps > 0);
                // Index depth may add a few steps; any agent scan adds at least one per added row.
                assert!(larger_steps < batch_steps + 48,
                    "16 touched turns must seek their scopes, not scan the agent: {batch_steps} -> {larger_steps}");
                Ok(())
            })
            .await
            .expect("bounded receipt scope");
    }

    #[test]
    fn typed_usage_receipt_scope_preserves_nulls_duplicate_responses_and_unmodeled_fallback() {
        use std::collections::BTreeSet;
        let db = Connection::open_in_memory().expect("database");
        db.execute_batch(super::TABLES).expect("aggregate tables");
        db.execute_batch(super::INDEXES)
            .expect("existing aggregate indexes");
        for (id, root, agent, thread, turn, response) in [
            (
                "seed",
                "root",
                "agent",
                Some("thread"),
                Some("turn"),
                Some("r1"),
            ),
            (
                "same-turn",
                "root",
                "agent",
                Some("thread"),
                Some("turn"),
                Some("r2"),
            ),
            (
                "duplicate",
                "root",
                "agent",
                Some("other-thread"),
                Some("other-turn"),
                Some("r1"),
            ),
            ("peer-duplicate", "root", "agent", None, None, Some("r2")),
            ("null-response", "root", "agent", None, None, None),
            (
                "unrelated",
                "root",
                "agent",
                Some("elsewhere"),
                Some("elsewhere"),
                Some("r3"),
            ),
            (
                "other-agent",
                "root",
                "other-agent",
                Some("thread"),
                Some("turn"),
                Some("r1"),
            ),
            (
                "other-root",
                "other-root",
                "agent",
                Some("thread"),
                Some("turn"),
                Some("r1"),
            ),
        ] {
            db.execute("INSERT INTO analytics_cost_rows(id,root,agent,thread,turn,seq,at,model,response,uncached,counted,final_evidence) VALUES(?1,?2,?3,?4,?5,1,1,'m',?6,1,0,0)",
                rusqlite::params![id,root,agent,thread,turn,response]).expect("scope row");
        }
        let ids = |turns| {
            super::scope_rows(&db, "root", "agent", &turns)
                .expect("scope")
                .into_iter()
                .map(|row| row.id)
                .collect::<BTreeSet<_>>()
        };
        assert_eq!(
            ids(BTreeSet::from([(
                Some("thread".into()),
                Some("turn".into()),
                None
            )])),
            ["seed", "same-turn", "duplicate", "peer-duplicate"]
                .map(str::to_owned)
                .into()
        );
        assert_eq!(
            ids(BTreeSet::from([(None, None, None)])),
            ["peer-duplicate", "null-response", "same-turn"]
                .map(str::to_owned)
                .into()
        );
        assert_eq!(
            ids(BTreeSet::from([(
                Some("missing".into()),
                Some("missing".into()),
                Some("r3".into())
            )])),
            ["unrelated"].map(str::to_owned).into()
        );
        assert!(ids(BTreeSet::new()).is_empty());
        db.execute(
            "UPDATE analytics_cost_rows SET model=NULL WHERE id='unrelated'",
            [],
        )
        .expect("unmodeled evidence");
        assert_eq!(
            ids(BTreeSet::from([(
                Some("thread".into()),
                Some("turn".into()),
                None
            )])),
            [
                "seed",
                "same-turn",
                "duplicate",
                "peer-duplicate",
                "null-response",
                "unrelated"
            ]
            .map(str::to_owned)
            .into()
        );
    }

    #[tokio::test]
    async fn typed_usage_cache_write_1h_separates_cost_groups() {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_,StoreError>(|tx,_| {
            for (id,split) in [("legacy",None),("zero",Some(0)),("hour",Some(10))] {
                let mut tokens = json!({"uncachedInput":1,"cacheReadInput":2,"cacheWriteInput":10,"output":3});
                if let Some(split) = split { tokens["cacheWriteInput1h"] = json!(split); }
                AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&json!({"id":id,"agentId":"agent","rootId":"root","at":1,"last":{},"total":{},
                    "studioUsage":{"kind":"response","responseId":id,"model":"claude-test","evidence":"live","tokens":tokens}}))?;
            }
            let groups = super::cost_groups(tx,"root",&Default::default())?;
            assert_eq!(groups.len(),3,"absence, zero and one-hour costs have distinct identities");
            assert!(groups.iter().all(|group| group.count==1));
            Ok(())
        }).await.expect("split groups");
    }

    #[test]
    fn typed_usage_migrated_suppression_uses_linear_predicate_work() {
        use serde::Deserialize;
        use std::cell::Cell;
        let facts = json!({"last":{}});
        let fields = super::UsageCostFields::deserialize(&facts).expect("metadata");
        let mut measured = Vec::new();
        for size in [4, 8, 16] {
            let rows = (0..size)
                .map(|index| {
                    let mut row = super::cost_row(
                        index.to_string(),
                        "agent".into(),
                        Some("thread".into()),
                        Some(index.to_string()),
                        index,
                        1.0,
                        &fields,
                        None,
                    );
                    row.eligible = None;
                    row
                })
                .collect::<Vec<_>>();
            let calls = Cell::new(0);
            let kept = super::contributions(&rows, |_| {
                calls.set(calls.get() + 1);
                true
            });
            assert!(
                kept.iter().all(Option::is_some),
                "keep the prior migrated-row interpretation"
            );
            measured.push((size, calls.get()));
        }
        assert!(
            measured.iter().all(|(size, calls)| *calls <= 5 * size),
            "suppression must do linear predicate work: {measured:?}"
        );
    }

    #[tokio::test]
    async fn typed_usage_unknown_legacy_cache_categories_are_not_priced() {
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_,StoreError>(|tx,_| {
            AnalyticsCaptureRepository::write(tx, CaptureTable::Agents, &json!({"id":"agent","provider":"codex"}))?;
            for (index, (category, unknown)) in [
                ("cachedInputTokens",None), ("cachedInputTokens",Some(serde_json::Value::Null)),
                ("cacheWriteInputTokens",None), ("cacheWriteInputTokens",Some(serde_json::Value::Null)),
            ].into_iter().enumerate() {
                let mut last = json!({"inputTokens":10,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":2});
                match unknown {
                    Some(value) => {last[category]=value;}
                    None => {last.as_object_mut().expect("tokens").remove(category);}
                }
                AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&json!({
                    "id":format!("unknown{index}"),"agentId":"agent","rootId":"root","threadId":"thread",
                    "turnId":format!("turn{index}"),"at":1,"model":"m","responseId":format!("r{index}"),
                    "last":last,"total":{}}))?;
            }
            assert!(super::cost_groups(tx,"root",&Default::default())?.is_empty(),
                "unknown cache categories must not become zero-valued priced responses");
            assert!(super::recorded_responses(tx,&["agent".to_owned()])?.iter().all(|row| row.tokens.is_none()),
                "diagnostic response evidence must also retain unknown categories");
            AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&json!({
                "id":"known","agentId":"agent","rootId":"root","threadId":"thread","turnId":"known","at":1,
                "model":"m","responseId":"known","last":{"inputTokens":10,"cachedInputTokens":0,"cacheWriteInputTokens":0,"outputTokens":2},"total":{}}))?;
            assert_eq!(super::cost_groups(tx,"root",&Default::default())?.iter().map(|group|group.count).sum::<u64>(),1,
                "explicitly reported zero cache categories remain priceable");
            Ok(())
        }).await.expect("cost eligibility");
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // The fixture builds a legacy record and its ledger form.
    async fn typed_usage_receipt_decoding_keeps_legacy_and_ledger_identity_checks() {
        use studio_runtime_schema::usage::ledger::UsageLedgerRow;
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            let mut record = json!({"id":"receipt","agentId":"agent","rootId":"root","threadId":"thread","at":1,
                "responseId":"response","accountKey":"acct","last":{},"total":{},
                "studioUsage":{"kind":"response","responseId":"response","model":"m","evidence":"live",
                    "tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}}});
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &record)?;
            let receipts = [("acct".into(), "thread".into(), "response".into())].into();
            assert_eq!(partition_journal_receipts(tx, &receipts, &receipt_members())?.superseded.len(), 1);
            let observation = serde_json::from_value(record["studioUsage"].clone())?;
            let ledger = UsageLedgerRow::capture("agent", Some("thread"), observation);
            record.as_object_mut().expect("row").extend(serde_json::to_value(ledger)?.as_object().expect("ledger").clone());
            tx.execute("UPDATE analytics_usage SET record=?1 WHERE id='receipt'", [record.to_string()])?;
            assert_eq!(partition_journal_receipts(tx, &receipts, &receipt_members())?.superseded.len(), 1);
            for pointer in ["/receiptKey", "/usageIdentity/agent", "/usageIdentity/responseId", "/studioUsage/tokens/output"] {
                for invalid in [json!(null), json!(""), json!({})] {
                    let mut malformed = record.clone();
                    *malformed.pointer_mut(pointer).expect("field") = invalid;
                    tx.execute("UPDATE analytics_usage SET record=?1 WHERE id='receipt'", [malformed.to_string()])?;
                    assert!(partition_journal_receipts(tx, &receipts, &receipt_members()).is_err(), "{pointer}: {malformed}");
                }
            }
            record.as_object_mut().expect("row").remove("usageIdentity");
            tx.execute("UPDATE analytics_usage SET record=?1 WHERE id='receipt'", [record.to_string()])?;
            assert!(partition_journal_receipts(tx, &receipts, &receipt_members()).is_err(), "receipt metadata requires its persisted identity");
            record.as_object_mut().expect("row").remove("receiptKey");
            record["studioUsage"]["evidence"] = json!("legacy");
            tx.execute("UPDATE analytics_usage SET record=?1 WHERE id='receipt'", [record.to_string()])?;
            assert_eq!(partition_journal_receipts(tx, &receipts, &receipt_members())?.accepted, receipts);
            record.as_object_mut().expect("row").remove("studioUsage");
            tx.execute("UPDATE analytics_usage SET record=?1 WHERE id='receipt'", [record.to_string()])?;
            assert_eq!(partition_journal_receipts(tx, &receipts, &receipt_members())?.accepted, receipts);
            Ok(())
        }).await.expect("typed receipt identity");
    }

    #[tokio::test]
    async fn journal_deltas_recompute_only_for_changed_marker() {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            AnalyticsCaptureRepository::write(tx, CaptureTable::Agents, &json!({"id":"agent","rootId":"root","provider":"codex"}))?;
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &json!({"id":"receipt","agentId":"agent","rootId":"root","threadId":"thread","at":1,"responseId":"response","accountKey":"acct","model":"m","last":{"inputTokens":10,"outputTokens":2},"total":{}}))?;
            Ok(())
        }).await.expect("usage");
        let db = crate::AnalyticsRepository::new(store.root())
            .open_read_only()
            .expect("reader");
        let mut cache = super::JournalReceiptCache::default();
        let mut receipts = [("acct".into(), "thread".into(), "response".into())].into();
        let mut members = receipt_members();
        super::JOURNAL_DELTA_RUNS.take();
        let cached_partition = std::ptr::addr_of!(cache.entry.partition);
        let (partition, first) = super::journal_partition_and_adjustments_cached(
            &db,
            "root",
            &borrowed_receipts(&receipts),
            &members,
            &mut cache,
        )
        .expect("first");
        assert!(
            std::ptr::eq(partition, cached_partition),
            "the caller should borrow the cached partition instead of cloning its receipt keys"
        );
        let partition = partition.clone();
        assert_eq!(super::JOURNAL_DELTA_RUNS.take(), 1);
        let (_, second) = super::journal_partition_and_adjustments_cached(
            &db,
            "root",
            &borrowed_receipts(&receipts),
            &members,
            &mut cache,
        )
        .expect("repeat");
        assert_eq!(
            super::JOURNAL_DELTA_RUNS.take(),
            0,
            "repeat must skip delta computation"
        );
        assert_eq!(format!("{first:?}"), format!("{second:?}"));
        assert_eq!(
            format!("{first:?}"),
            format!(
                "{:?}",
                super::cost_group_adjustments(&db, "root", &partition).expect("uncached")
            )
        );
        super::JOURNAL_DELTA_RUNS.take();
        receipts.insert(("acct".into(), "thread".into(), "another".into()));
        super::journal_partition_and_adjustments_cached(
            &db,
            "root",
            &borrowed_receipts(&receipts),
            &members,
            &mut cache,
        )
        .expect("receipt change");
        assert_eq!(super::JOURNAL_DELTA_RUNS.take(), 1);
        members[0].1.thread_id = Some("other-thread".into());
        super::journal_partition_and_adjustments_cached(
            &db,
            "root",
            &borrowed_receipts(&receipts),
            &members,
            &mut cache,
        )
        .expect("scope change");
        assert_eq!(super::JOURNAL_DELTA_RUNS.take(), 1);
        store
            .analytics_transaction::<_, StoreError>(|tx, _| {
                tx.execute(
                    "UPDATE analytics_usage SET record=record WHERE id='receipt'",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("advance revision");
        super::journal_partition_and_adjustments_cached(
            &db,
            "root",
            &borrowed_receipts(&receipts),
            &members,
            &mut cache,
        )
        .expect("revision change");
        assert_eq!(super::JOURNAL_DELTA_RUNS.take(), 1);
    }

    #[tokio::test]
    async fn typed_usage_unchanged_receipt_reuses_validated_evidence() {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            let record = json!({"id":"receipt","agentId":"agent","rootId":"root","threadId":"thread","at":1,
                "responseId":"response","accountKey":"acct","last":{},"total":{},
                "studioUsage":{"kind":"response","responseId":"response","model":"m","evidence":"live",
                    "tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}}});
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &record)?;
            Ok(())
        }).await.expect("record usage");
        let db = crate::AnalyticsRepository::new(store.root())
            .open_read_only()
            .expect("reader");
        let mut cache = super::JournalReceiptCache::default();
        let receipts = [("acct".into(), "thread".into(), "response".into())].into();
        assert_eq!(
            partition_journal_receipts_cached(&db, &receipts, &receipt_members(), &mut cache)
                .expect("first partition")
                .superseded
                .len(),
            1
        );
        super::RECEIPT_READS.take();
        assert_eq!(
            partition_journal_receipts_cached(&db, &receipts, &receipt_members(), &mut cache)
                .expect("second partition")
                .superseded
                .len(),
            1
        );
        assert_eq!(
            super::RECEIPT_READS.take(),
            0,
            "unchanged receipts must reuse validated evidence"
        );
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            tx.execute("UPDATE analytics_usage SET record=json_set(record,'$.studioUsage.evidence','legacy') WHERE id='receipt'", [])?;
            Ok(())
        }).await.expect("update usage in place");
        let updated =
            partition_journal_receipts_cached(&db, &receipts, &receipt_members(), &mut cache)
                .expect("updated partition");
        assert_eq!(
            updated.accepted, receipts,
            "legacy evidence accepts the journal"
        );
        assert_eq!(
            super::RECEIPT_READS.take(),
            1,
            "only the changed receipt is decoded"
        );
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            tx.execute("UPDATE analytics_usage SET record=json_set(record,'$.studioUsage.tokens.output',null) WHERE id='receipt'", [])?;
            Ok(())
        }).await.expect("corrupt usage in place");
        assert!(
            partition_journal_receipts_cached(&db, &receipts, &receipt_members(), &mut cache)
                .is_err(),
            "changed malformed evidence must fail closed"
        );
        store
            .analytics_transaction::<_, StoreError>(|tx, _| {
                tx.execute("DELETE FROM analytics_usage WHERE id='receipt'", [])?;
                Ok(())
            })
            .await
            .expect("delete usage");
        let deleted =
            partition_journal_receipts_cached(&db, &receipts, &receipt_members(), &mut cache)
                .expect("deleted partition");
        assert_eq!(
            deleted.accepted, receipts,
            "deleted evidence accepts the journal"
        );
    }

    #[tokio::test]
    async fn typed_usage_receipt_cache_keeps_duplicate_final_evidence() {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            for id in ["one", "two"] {
                let record = json!({"id":id,"agentId":"agent","rootId":"root","threadId":"thread","at":1,
                    "responseId":"response","accountKey":"acct","last":{},"total":{},
                    "studioUsage":{"kind":"response","responseId":"response","model":"m","evidence":"live",
                        "tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}}});
                AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &record)?;
            }
            Ok(())
        }).await.expect("record usage");
        let db = crate::AnalyticsRepository::new(store.root())
            .open_read_only()
            .expect("reader");
        let mut cache = super::JournalReceiptCache::default();
        let receipts = [("acct".into(), "thread".into(), "response".into())].into();
        let first =
            partition_journal_receipts_cached(&db, &receipts, &receipt_members(), &mut cache)
                .expect("initial");
        assert_eq!(first.superseded, receipts);
        for (id, remaining) in [("one", true), ("two", false)] {
            store
                .analytics_transaction::<_, StoreError>(move |tx, _| {
                    tx.execute("DELETE FROM analytics_usage WHERE id=?1", [id])?;
                    Ok(())
                })
                .await
                .expect("delete duplicate");
            let cached =
                partition_journal_receipts_cached(&db, &receipts, &receipt_members(), &mut cache)
                    .expect("cached");
            let fresh =
                partition_journal_receipts(&db, &receipts, &receipt_members()).expect("fresh");
            assert_eq!(cached.accepted, fresh.accepted);
            assert_eq!(cached.superseded, fresh.superseded);
            assert_eq!(!cached.superseded.is_empty(), remaining);
        }
    }

    #[tokio::test]
    async fn typed_usage_group_revision_tracks_changes_and_deletions() {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &json!({"id":"receipt","agentId":"agent","rootId":"root","threadId":"thread","at":1,
                "responseId":"response","accountKey":"acct","last":{},"total":{},
                "studioUsage":{"kind":"response","responseId":"response","model":"m","evidence":"live",
                    "tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}}}))?;
            let (id, first): (i64, i64) = tx.query_row("SELECT id,revision FROM analytics_cost_group_revisions", [], |row| Ok((row.get(0)?,row.get(1)?)))?;
            let (snapshot, rows) = super::cost_group_changes(tx, "root", None)?;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].1.as_ref().expect("group").count, 1);
            tx.execute("UPDATE analytics_cost_groups SET count=count+1 WHERE rowid=?1", [id])?;
            let second: i64 = tx.query_row("SELECT revision FROM analytics_cost_group_revisions WHERE id=?1", [id], |row| row.get(0))?;
            assert!(second > first, "in-place count changes advance the revision");
            let (changed, rows) = super::cost_group_changes(tx, "root", Some(snapshot))?;
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].1.as_ref().expect("updated group").count, 2);
            tx.execute("DELETE FROM analytics_cost_groups WHERE rowid=?1", [id])?;
            let third: i64 = tx.query_row("SELECT revision FROM analytics_cost_group_revisions WHERE id=?1", [id], |row| row.get(0))?;
            assert!(third > second, "deletions retain a newer tombstone");
            let (_, rows) = super::cost_group_changes(tx, "root", Some(changed))?;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].1.is_none());
            Ok(())
        }).await.expect("group revision tracking");
    }

    #[tokio::test]
    async fn typed_usage_revision_migrates_old_database_without_changing_groups() {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            let record = json!({"id":"receipt","agentId":"agent","rootId":"root","threadId":"thread","at":1,
                "responseId":"response","accountKey":"acct","last":{},"total":{},
                "studioUsage":{"kind":"response","responseId":"response","model":"m","evidence":"live",
                    "tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}}});
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &record)?;
            Ok(())
        }).await.expect("record usage");
        let db = crate::AnalyticsRepository::new(store.root())
            .open_read_only()
            .expect("reader");
        let before = super::cost_groups(&db, "root", &Default::default()).expect("groups");
        drop(db);
        drop(store);
        let db = rusqlite::Connection::open(temp.path().join("analytics.sqlite3"))
            .expect("old database");
        db.execute_batch("DROP TRIGGER analytics_cost_group_revision_insert; DROP TRIGGER analytics_cost_group_revision_update;
            DROP TRIGGER analytics_cost_group_revision_delete; DROP TABLE analytics_cost_group_revisions;
            DROP TRIGGER analytics_usage_revision_insert; DROP TRIGGER analytics_usage_revision_update;
            DROP TRIGGER analytics_usage_revision_delete; DROP TABLE analytics_usage_revisions;
            DROP TABLE analytics_usage_revision_clock;").expect("restore old schema");
        drop(db);
        let store = RuntimeStore::open(Some(temp.path()))
            .await
            .expect("migrate old database");
        let db = crate::AnalyticsRepository::new(store.root())
            .open_read_only()
            .expect("reader");
        assert_eq!(
            super::cost_groups(&db, "root", &Default::default()).expect("groups after migration"),
            before
        );
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM analytics_cost_group_revisions",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("group revisions"),
            0
        );
        assert_eq!(
            db.query_row(
                "SELECT revision FROM analytics_usage_revision_clock",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("clock"),
            0
        );
    }

    #[tokio::test]
    async fn typed_usage_cost_index_migration_preserves_groups_on_reopen() {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        drop(store);
        let db =
            rusqlite::Connection::open(temp.path().join("analytics.sqlite3")).expect("database");
        db.execute_batch("DROP INDEX analytics_cost_groups_cover;
            CREATE INDEX analytics_cost_groups_key ON analytics_cost_groups(root,model,account,input,cached,cache_write,output,uncached);
            INSERT INTO analytics_cost_groups VALUES('root','m','acct',10,3,2,1,1,2,NULL);")
            .expect("previous group index and retained facts");
        let expected =
            super::cost_groups(&db, "root", &Default::default()).expect("before migration");
        drop(db);
        {
            let store = RuntimeStore::open(Some(temp.path()))
                .await
                .expect("upgrade and reopen");
            let db = crate::AnalyticsRepository::new(store.root())
                .open_read_only()
                .expect("reader");
            assert_eq!(
                super::cost_groups(&db, "root", &Default::default()).expect("retained groups"),
                expected
            );
            let old: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name='analytics_cost_groups_key')", [], |row| row.get(0)).expect("old index");
            assert!(!old, "retire the redundant index");
            let version: i64 = db
                .pragma_query_value(None, "schema_version", |row| row.get(0))
                .expect("schema version");
            drop(db);
            drop(store);
            let store = RuntimeStore::open(Some(temp.path()))
                .await
                .expect("idempotent startup");
            let db = crate::AnalyticsRepository::new(store.root())
                .open_read_only()
                .expect("reader");
            assert_eq!(
                db.pragma_query_value::<i64, _>(None, "schema_version", |row| row.get(0))
                    .expect("schema version"),
                version
            );
        }
    }

    #[tokio::test]
    async fn typed_usage_cost_groups_read_is_covered() {
        let temp = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            let plan = tx.prepare("EXPLAIN QUERY PLAN SELECT model,account,input,cached,cache_write,output,uncached,count,cache_write_1h FROM analytics_cost_groups WHERE root=?1")?
                .query_map(["root"], |row| row.get::<_, String>(3))?
                .collect::<rusqlite::Result<Vec<_>>>()?;
            eprintln!("cost group read plan: {plan:?}");
            assert!(plan.iter().any(|line| line.contains("USING COVERING INDEX")), "{plan:?}");
            Ok(())
        }).await.expect("covered group read");
    }

    #[tokio::test]
    async fn typed_usage_receipt_lookup_uses_existing_response_index() {
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store
            .analytics_transaction::<_, StoreError>(|tx, _| {
                let mut query = tx.prepare(&format!(
                    "EXPLAIN QUERY PLAN {}",
                    super::JOURNAL_RECEIPT_SQL
                ))?;
                let count = query.parameter_count();
                let details = query
                    .query_map(
                        rusqlite::params_from_iter(std::iter::repeat_n(
                            rusqlite::types::Value::Null,
                            count,
                        )),
                        |row| row.get::<_, String>(3),
                    )?
                    .collect::<Result<Vec<_>, _>>()?;
                assert!(
                    details
                        .iter()
                        .any(|line| line.contains("SEARCH")
                            && line.contains("analytics_usage_response")),
                    "receipt lookup must seek existing usage identity index: {details:?}"
                );
                assert!(
                    !details
                        .iter()
                        .any(|line| line.contains("SCAN analytics_usage")),
                    "no repeated usage-record scans: {details:?}"
                );
                assert!(
                    !details.iter().any(|line| line.contains("analytics_agents")),
                    "receipt lookup consumes selected members without expanding all agents: {details:?}"
                );
                for (sql, index) in [
                    (
                        crate::repositories::analytics_capture::CAPTURE_RECORD_BY_ID_SQL
                            .replace("{table}", "analytics_usage"),
                        "sqlite_autoindex_analytics_usage_1",
                    ),
                    (
                        crate::repositories::analytics_capture::USAGE_RESPONSE_SQL.to_owned(),
                        "analytics_usage_response",
                    ),
                ] {
                    let mut query = tx.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
                    let parameters = query.parameter_count();
                    let plan = query
                        .query_map(
                            rusqlite::params_from_iter(std::iter::repeat_n(
                                rusqlite::types::Value::Null,
                                parameters,
                            )),
                            |row| row.get::<_, String>(3),
                        )?
                        .collect::<Result<Vec<_>, _>>()?;
                    assert!(
                        plan.iter()
                            .any(|line| line.contains("SEARCH") && line.contains(index)),
                        "capture lookup must seek its existing index: {plan:?}"
                    );
                }

                Ok(())
            })
            .await
            .expect("query plan");
    }

    #[test]
    // AuthAction is non-exhaustive; only reads of the guarded table are denied.
    #[allow(clippy::wildcard_enum_match_arm)]
    fn typed_usage_legacy_migration_keeps_totals_without_reading_usage_rows() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,id TEXT UNIQUE,agent TEXT,root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT);
            CREATE TABLE analytics_cost_rows(id TEXT PRIMARY KEY,root TEXT NOT NULL,agent TEXT NOT NULL,thread TEXT,turn TEXT,seq INTEGER NOT NULL,at REAL NOT NULL,model TEXT,account_key TEXT,response TEXT,input REAL,cached REAL,cache_write REAL,output REAL,uncached INTEGER NOT NULL,counted INTEGER NOT NULL,counted_model TEXT);
            CREATE TABLE analytics_cost_groups(root TEXT NOT NULL,model TEXT,account TEXT NOT NULL,input REAL,cached REAL,cache_write REAL,output REAL,uncached INTEGER NOT NULL,count INTEGER NOT NULL);
            CREATE INDEX analytics_cost_rows_unmodeled ON analytics_cost_rows(root,agent) WHERE model IS NULL;
            WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<512)
            INSERT INTO analytics_usage SELECT x,CAST(x AS TEXT),'agent',NULL,'thread','turn',1,'{}' FROM n;
            INSERT INTO analytics_cost_rows VALUES('legacy','root','agent','thread','turn',1,1,'m','acct',NULL,10,0,0,2,0,1,'m');
            INSERT INTO analytics_cost_groups VALUES('root','m','acct',10,0,0,2,0,1);").expect("pre-upgrade fixture");
        db.authorizer(Some(|context: AuthContext<'_>| match context.action {
            AuthAction::Read {
                table_name: "analytics_usage",
                ..
            } => Authorization::Deny,
            _ => Authorization::Allow,
        }))
        .expect("usage-read guard");
        crate::migrations::m2026_10_22_analytics::aggregate_columns(&db)
            .expect("bounded additive migration does not inspect usage records");
        super::install(&db).expect("current aggregate");
        let scope_indexes: Vec<String> = db.prepare("SELECT name FROM sqlite_master WHERE type='index' AND name IN ('analytics_cost_rows_unmodeled','analytics_cost_rows_eligible_unmodeled') ORDER BY name")
            .expect("index migration").query_map([], |row| row.get(0)).expect("index names")
            .collect::<Result<_, _>>().expect("indexes");
        assert_eq!(
            scope_indexes,
            vec!["analytics_cost_rows_eligible_unmodeled"]
        );
        let groups = super::cost_groups(&db, "root", &Default::default()).expect("legacy groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tokens.input_tokens, Some(10.0));
        assert_eq!(groups[0].tokens.output_tokens, Some(2.0));
        assert_eq!(groups[0].count, 1);
        assert_eq!(groups[0].tokens.cache_write_input_1h_tokens, None);
        crate::migrations::m2026_10_22_analytics::aggregate_columns(&db)
            .expect("migration is idempotent");
        super::install(&db).expect("current aggregate");
        assert_eq!(
            super::cost_groups(&db, "root", &Default::default()).expect("groups"),
            groups
        );
        let split: Option<f64> = db
            .query_row(
                "SELECT cache_write_1h FROM analytics_cost_rows WHERE id='legacy'",
                [],
                |row| row.get(0),
            )
            .expect("legacy fact");
        assert_eq!(split, None, "migration never invents a legacy lifetime");
    }

    #[tokio::test]
    async fn typed_usage_final_deduplicates_legacy_without_merging_request_tiers() {
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_,StoreError>(|tx,_| {
            for (id,response,evidence,input) in [("old","r1","legacy",900),("final","r1","live",10),("large","r2","recovered",250001),("later","r3","live",20)] {
                AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&json!({"id":id,"agentId":"agent","rootId":"root","threadId":"thread","turnId":if id=="later"{"old"}else{id},"at":1,"last":{},"total":{},
                    "studioUsage":{"kind":"response","responseId":response,"model":"m","evidence":evidence,"tokens":{"uncachedInput":input,"cacheReadInput":3,"cacheWriteInput":2,"output":1}}}))?;
            }
            let groups=super::cost_groups(tx,"root",&Default::default())?;
            let mut inputs=groups.iter().map(|group| { assert_eq!(group.count,1); assert!(group.input_uncached); group.tokens.input_tokens.expect("normalized input") as u64 }).collect::<Vec<_>>();
            inputs.sort();
            assert_eq!(inputs,vec![10,20,250001],"keep separate request sizes for pricing thresholds, count each final response once");
            Ok(())
        }).await.expect("typed request costs");
    }

    #[tokio::test]
    async fn typed_usage_evidence_queries_bound_rows_and_partition_journal() {
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            tx.execute("INSERT INTO analytics_agents(id,record) VALUES('agent',?1)", [json!({"id":"agent","provider":"claude","accountKey":"acct","threadId":"thread"}).to_string()])?;

            for (id, evidence) in [("legacy", "legacy"), ("final", "recovered")] {
                let row = json!({"id":id,"agentId":"agent","rootId":"root","threadId":"thread","at":1,
                    "responseId":id,"model":"m","accountKey":"acct","last":{},"total":{},
                    "studioUsage":{"kind":"response","responseId":id,"model":"m","evidence":evidence,
                        "tokens":{"uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2}}});
                AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &row)?;
            }
            let agents = ["agent".to_owned()];
            let (rows, truncated) = super::recorded_responses_bounded(tx, &agents, 1)?;
            assert!(truncated);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].tokens.as_ref().expect("known tokens").output(), 2);
            assert_eq!(super::recorded_responses(tx, &agents)?.len(), 2);
            let receipts = ["legacy", "final", "missing"].map(|id| ("acct".to_owned(), "thread".to_owned(), id.to_owned())).into();
            let partition = partition_journal_receipts(tx, &receipts, &receipt_members())?;
            assert_eq!(partition.accepted.len(), 2);
            assert_eq!(partition.superseded.len(), 1);
            assert!(partition.superseded.contains(&("acct".to_owned(),"thread".to_owned(),"final".to_owned())));
            Ok(())
        }).await.expect("typed evidence query");
    }

    #[tokio::test]
    async fn typed_usage_cumulative_never_contributes_response_costs() {
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            AnalyticsCaptureRepository::write(tx, CaptureTable::Agents, &json!({"id":"agent","provider":"codex"}))?;
            let row = json!({
                "id":"cumulative", "agentId":"agent", "rootId":"root", "threadId":"thread",
                "turnId":"turn", "at":1, "model":"gpt-5", "last":{"inputTokens":10,"outputTokens":2},
                "total":{"inputTokens":100,"outputTokens":20},
                "studioUsage":{"kind":"cumulative","scope":"codexThread","total":{
                    "uncachedInput":100,"cacheReadInput":0,"cacheWriteInput":0,"output":20
                }}
            });
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &row)?;
            let mut legacy = row;
            legacy.as_object_mut().expect("usage row").remove("studioUsage");
            legacy["id"] = json!("legacy-notice");
            legacy["responseId"] = json!("associated-response");
            legacy["counterDomain"] = json!("nativeNotice");
            AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &legacy)?;
            let groups = super::cost_groups(tx, "root", &Default::default())?;
            assert!(groups.is_empty(), "running totals must never become response cost samples");
            assert!(super::recorded_responses(tx, &["agent".to_owned()])?.is_empty());
            Ok(())
        }).await.expect("verify cumulative accounting");
    }

    #[tokio::test]
    async fn typed_usage_journal_replaces_legacy_but_preserves_final_response() {
        thread_local! { static PARTITIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            tx.execute("INSERT INTO analytics_agents(id,record) VALUES('agent',?1)", [json!({"id":"agent","provider":"claude","accountKey":"acct","threadId":"thread"}).to_string()])?;

            for (id, evidence) in [("legacy", "legacy"), ("final", "live")] {
                let row = json!({
                    "id":id,"agentId":"agent","rootId":"root","threadId":"thread",
                    "turnId":id,"at":1,"model":"claude-sonnet-4-5","accountKey":"acct",
                    "responseId":id,"last":{"inputTokens":10,"outputTokens":2},"total":{},
                    "studioUsage":{"kind":"response","responseId":id,"model":"claude-sonnet-4-5",
                        "evidence":evidence,"tokens":{
                            "uncachedInput":10,"cacheReadInput":0,"cacheWriteInput":0,"output":2
                        }}
                });
                AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &row)?;
            }
            let receipts = ["legacy", "final"].map(|id| ("acct".to_owned(), "thread".to_owned(), id.to_owned())).into();
            PARTITIONS.set(0);
            tx.trace_v2(rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT, Some(|event| {
                if let rusqlite::trace::TraceEvent::Stmt(_, sql) = event
                    && sql.contains("WITH receipts AS MATERIALIZED") {
                    PARTITIONS.set(PARTITIONS.get() + 1);
                }
            }));
            let partition = partition_journal_receipts(tx, &receipts, &receipt_members())?;
            let groups = super::cost_groups(tx, "root", &partition)?;
            let adjustments = super::cost_group_adjustments(tx, "root", &partition)?;
            assert_eq!(adjustments.len(), 1);
            assert_eq!(adjustments[0].0.as_ref().map(|g| g.count), Some(2));
            assert_eq!(adjustments[0].1.as_ref().map(|g| g.count), Some(1));
            tx.trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
            assert_eq!(PARTITIONS.get(), 1, "one receipt partition supplies both journal filtering and grouped costs");
            assert_eq!(groups.iter().map(|group| group.count).sum::<u64>(), 1,
                "journal evidence replaces provisional legacy only; the final response remains counted");
            Ok(())
        }).await.expect("verify journal replacement");
    }

    /// Every aggregate row, ordered, without SQLite row ids.
    fn dump(path: &Path) -> (Vec<String>, Vec<String>) {
        let db = Connection::open(path).expect("analytics database");
        let rows = |sql: &str| -> Vec<String> {
            db.prepare(sql)
                .expect("dump query")
                .query_map([], |row| row.get::<_, String>(0))
                .expect("dump rows")
                .collect::<Result<_, _>>()
                .expect("dump values")
        };
        (
            rows("SELECT json_array(root,model,account,input,cached,cache_write,output,uncached,count)
                  FROM analytics_cost_groups ORDER BY 1"),
            rows("SELECT json_array(id,root,counted,counted_model) FROM analytics_cost_rows ORDER BY 1"),
        )
    }

    /// Incremental maintenance over upserts that move rows between turns and
    /// roots, answer provisional turns and leave models out ends where a
    /// rebuild from the usage rows ends.
    #[tokio::test]
    async fn incremental_writes_match_a_rebuild() {
        let temp = tempfile::tempdir().expect("temporary state");
        let mut seed = 0x2545_f491_u64;
        let mut next = move |bound: u64| {
            seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1);
            (seed >> 33) % bound
        };
        let mut writes = Vec::new();
        for step in 0..400_u64 {
            let agent = format!("agent-{}", next(4));
            let model = match next(4) {
                0 => json!(null),
                1 => json!("gpt-5"),
                2 => json!("gpt-5-mini"),
                _ => json!("claude-sonnet-4-5"),
            };
            let response = if next(2) == 0 {
                json!(null)
            } else {
                json!(format!("r{}", next(6)))
            };
            let tokens = json!({"inputTokens":next(3) * 10,"cachedInputTokens":next(2),
                "cacheWriteInputTokens":0,"outputTokens":next(3),"totalTokens":5});
            let root = if next(5) == 0 { "root-b" } else { "root-a" };
            writes.push(json!({
                "id": format!("u{}", next(60)),
                "agentId": agent,
                "rootId": root,
                "threadId": format!("thread-{}", next(2)),
                "turnId": format!("turn-{}", next(5)),
                "at": (step % 37) as f64,
                "model": model,
                "accountKey": if next(3) == 0 { json!(null) } else { json!("acct") },
                "responseId": response,
                "inputTokensAreUncached": next(2) == 0,
                "last": tokens,
                "total": tokens,
            }));
        }
        // Preserve a real commit/reopen boundary with later overwrites of the same ids.
        for records in writes.chunks(200) {
            let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
            let records = records.to_vec();
            store
                .analytics_transaction::<_, StoreError>(move |tx, _| {
                    for index in 0..4 {
                        AnalyticsCaptureRepository::write(
                            tx,
                            CaptureTable::Agents,
                            &json!({"id":format!("agent-{index}"),"provider":"codex"}),
                        )?;
                    }
                    for record in records {
                        AnalyticsCaptureRepository::write(tx, CaptureTable::Usage, &record)?;
                    }
                    Ok(())
                })
                .await
                .expect("usage writes");
        }
        let path = temp.path().join("analytics.sqlite3");
        let incremental = dump(&path);
        assert!(!incremental.0.is_empty());
        Connection::open(&path)
            .expect("analytics database")
            .execute_batch("DROP TABLE analytics_cost_rows; DROP TABLE analytics_cost_groups;")
            .expect("drop aggregate");
        ensure_cost_aggregate(&path).expect("rebuild");
        assert_eq!(dump(&path), incremental);
    }
}
