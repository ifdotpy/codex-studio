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
//! agent, so an agent with model-less rows is refreshed whole.
//!
//! Pricing is not stored: a reader prices the few groups with the current
//! catalog, so a catalog change needs no invalidation.

use std::borrow::Cow;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::path::Path;

use rusqlite::{Connection, OpenFlags, OptionalExtension, Row, Transaction, params};
use serde::Deserialize;
use studio_runtime_schema::usage::{
    UsageEvidence, UsageObservation, UsageProvider, UsageTokens, parse_analytics_usage_row,
};

use crate::{CachedSql, StoreError, StoreResult};

/// Tables of the aggregate; created by [`ensure_cost_aggregate`].
const TABLES: &str = "
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
CREATE INDEX analytics_cost_rows_unmodeled ON analytics_cost_rows(root, agent) WHERE model IS NULL;
CREATE INDEX analytics_cost_rows_response ON analytics_cost_rows(root, thread, response)
    WHERE response IS NOT NULL;
CREATE INDEX analytics_cost_groups_key ON analytics_cost_groups(
    root, model, account, input, cached, cache_write, output, uncached);
";

const ROW_COLUMNS: &str = "id,agent,thread,turn,seq,at,model,account_key,response,\
    input,cached,cache_write,output,uncached,counted,counted_model,eligible,final_evidence,cache_write_1h";

/// The account key of a usage row or agent record without `accountKey`.
pub const DEFAULT_ACCOUNT: &str = "default";

/// Batched receipt lookup through current/history agent scopes and the existing usage index.
/// Parameters: serialized receipt tuples and the default account owner.
const JOURNAL_RECEIPT_SQL: &str = "
WITH receipts AS MATERIALIZED (
 SELECT json_extract(value,'$[0]') account,json_extract(value,'$[1]') thread,
        json_extract(value,'$[2]') response FROM json_each(?1)
), scopes AS MATERIALIZED (
 SELECT id agent,COALESCE(json_extract(record,'$.accountKey'),?2) account,
        json_extract(record,'$.threadId') thread FROM analytics_agents
 UNION
 SELECT a.id,COALESCE(json_extract(h.value,'$.accountKey'),?2),
        json_extract(h.value,'$.threadId')
 FROM analytics_agents a,json_each(a.record,'$.accountHistory') h
)
SELECT r.account,r.thread,r.response,u.record
FROM receipts r JOIN scopes s ON s.account=r.account AND s.thread=r.thread
CROSS JOIN analytics_usage u INDEXED BY analytics_usage_response
WHERE u.agent=s.agent AND u.thread=s.thread AND json_extract(u.record,'$.responseId')=r.response";

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
            input_tokens: Some(tokens.uncached_input as f64),
            cached_input_tokens: Some(tokens.cache_read_input as f64),
            cache_write_input_tokens: Some(tokens.cache_write_input as f64),
            cache_write_input_1h_tokens: tokens.cache_write_input_1h.map(|n| n as f64),
            output_tokens: Some(tokens.output as f64),
        }
    }
}

fn cost_observation(record: &serde_json::Value) -> StoreResult<Option<UsageObservation>> {
    studio_runtime_schema::usage::persisted::parse_analytics_cost_usage_row(
        UsageProvider::Codex,
        record,
    )
    .map_err(|error| StoreError::SchemaValidation(error.to_string()))
}

/// The members of an agent record that decide how its usage is priced and
/// where its Claude session logs are.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostMember {
    #[serde(default)]
    pub account_key: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
    #[serde(default)]
    pub thread_id: Option<String>,
    #[serde(default)]
    pub account_history: Option<Vec<CostAccountHistory>>,
}

/// One earlier account of an agent.
#[derive(Clone, Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CostAccountHistory {
    #[serde(default)]
    pub account_key: Option<String>,
    #[serde(default)]
    pub provider: Option<String>,
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
            decode_usage_evidence(row.get(0)?, row.get(1)?, &value)?
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
    agent_id: String,
    at: f64,
    value: &serde_json::Value,
) -> StoreResult<DecodedUsageEvidence> {
    let fields = UsageCostFields::deserialize(value)?;
    let (response_id, model, tokens, evidence) = match cost_observation(value)? {
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
            decode_usage_evidence(row.get(1)?, at, &value)?,
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

/// Preserve final live/recovered responses; only missing or legacy evidence accepts journal recovery.
pub fn partition_journal_receipts(
    db: &Connection,
    receipts: &BTreeSet<CostReceipt>,
) -> StoreResult<JournalPartition> {
    let mut result = JournalPartition {
        accepted: receipts.clone(),
        superseded: BTreeSet::new(),
    };
    if receipts.is_empty() {
        return Ok(result);
    }
    let encoded = serde_json::to_string(receipts)?;
    let mut statement = db.prepare_cached(JOURNAL_RECEIPT_SQL)?;
    let mut rows = statement.query(params![encoded, DEFAULT_ACCOUNT])?;
    while let Some(row) = rows.next()? {
        let value: serde_json::Value = serde_json::from_str(&row.get::<_, String>(3)?)?;
        match parse_analytics_usage_row(UsageProvider::Claude, &value) {
            Ok(UsageObservation::Response {
                evidence: UsageEvidence::Live | UsageEvidence::Recovered,
                ..
            }) => {
                let receipt = (row.get(0)?, row.get(1)?, row.get(2)?);
                result.accepted.remove(&receipt);
                result.superseded.insert(receipt);
            }
            Err(error) if value.get("studioUsage").is_some() => {
                return Err(StoreError::SchemaValidation(error.to_string()));
            }
            _ => {}
        }
    }
    Ok(result)
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
        if (row.final_evidence && !candidate.final_evidence)
            || (row.final_evidence == candidate.final_evidence && row.seq < candidate.seq)
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
pub(crate) fn ensure_cost_aggregate(path: &Path) -> StoreResult<()> {
    let mut db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)?;
    let tx = db.transaction()?;
    migrate_2026_10_22_drop_python_cost_projection(&tx)?;
    install(&tx)?;
    tx.commit()?;
    Ok(())
}

/// Retire the Python JSON projection after Rust took ownership of cost writes.
/// The projection table owns its three triggers and index; the six usage
/// triggers must go first because SQLite does not remove cross-table triggers.
/// IF EXISTS makes a retry after a committed open perform no schema changes.
fn migrate_2026_10_22_drop_python_cost_projection(db: &Transaction<'_>) -> StoreResult<()> {
    db.execute_batch(
        "DROP TRIGGER IF EXISTS analytics_cost_usage_before_insert_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_before_update_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_before_delete_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_insert_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_update_v1;
         DROP TRIGGER IF EXISTS analytics_cost_usage_delete_v1;
         DROP TABLE IF EXISTS analytics_cost_projection_v1;",
    )?;
    Ok(())
}

/// Create a missing aggregate, or add compatibility markers without reading usage
/// records or changing existing derived groups.
pub fn install(db: &Connection) -> StoreResult<()> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_cost_rows')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        db.execute_batch(TABLES)?;
        backfill(db)?;
    } else {
        let columns = db
            .prepare("PRAGMA table_info(analytics_cost_rows)")?
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<HashSet<_>, _>>()?;
        if !columns.contains("eligible") {
            db.execute_batch("ALTER TABLE analytics_cost_rows ADD COLUMN eligible INTEGER")?;
        }
        if !columns.contains("final_evidence") {
            db.execute_batch("ALTER TABLE analytics_cost_rows ADD COLUMN final_evidence INTEGER NOT NULL DEFAULT 0")?;
        }
        // Nullable columns are metadata-only additions: preserve every old fact/group.
        for table in ["analytics_cost_rows", "analytics_cost_groups"] {
            let has_split = db
                .prepare(&format!("PRAGMA table_info({table})"))?
                .query_map([], |row| row.get::<_, String>(1))?
                .collect::<Result<HashSet<_>, _>>()?
                .contains("cache_write_1h");
            if !has_split {
                db.execute_batch(&format!(
                    "ALTER TABLE {table} ADD COLUMN cache_write_1h REAL"
                ))?;
            }
        }
    }
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
        let observation = cost_observation(&record)?;
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
    let unmodeled: bool = db.query_row_cached("SELECT EXISTS(SELECT 1 FROM analytics_cost_rows WHERE root=?1 AND agent=?2 AND model IS NULL)",params![root,agent],|row|row.get(0))?;
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
    // Seek touched turns through the existing scope index, then scan this agent
    // once for their response counterparts, not once for every receipt.
    let requested = serde_json::to_string(turns)
        .map_err(|error| rusqlite::Error::ToSqlConversionFailure(Box::new(error)))?;
    let mut statement = db.prepare_cached(&format!(
        "WITH requested(thread,turn,response) AS MATERIALIZED (
            SELECT json_extract(value,'$[0]'),json_extract(value,'$[1]'),
                   json_extract(value,'$[2]') FROM json_each(?3)
         ), touched(id,response) AS MATERIALIZED (
            SELECT c.id,c.response FROM requested t CROSS JOIN analytics_cost_rows c
            WHERE c.root=?1 AND c.agent=?2 AND c.thread IS t.thread AND c.turn IS t.turn
         ), affected(response) AS (
            SELECT response FROM touched UNION SELECT response FROM requested
         )
         SELECT {ROW_COLUMNS} FROM analytics_cost_rows WHERE root=?1 AND agent=?2
         AND (id IN (SELECT id FROM touched) OR response IN (SELECT response FROM affected))"
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
    let observation = cost_observation(record)?;
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
        refresh_scope(
            db,
            root,
            &row.agent,
            &TurnScopes::from([(row.thread, row.turn, row.response)]),
        )?;
    }
    if let Some((previous_root, previous)) = previous {
        let turns = TurnScopes::from([(previous.thread, previous.turn, previous.response)]);
        refresh_scope(db, &previous_root, &previous.agent, &turns)?;
    }
    Ok(())
}

/// The counted groups of `root` in summation order, without the analytics rows
/// that `receipts` replace (each removal re-applies the counting rules to the
/// rows it affects).
pub fn cost_groups(
    db: &Connection,
    root: &str,
    receipts: &BTreeSet<CostReceipt>,
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
    let mut replaced = HashSet::new();
    let mut scopes = BTreeMap::<String, TurnScopes>::new();
    let accepted = partition_journal_receipts(db, receipts)?.accepted;
    for (account, thread, response) in &accepted {
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
            if let Some(model) = &row.counted {
                *groups.entry(row.group_key(model.clone())).or_default() -= 1;
            }
            if let Some(model) = contribution {
                *groups.entry(row.group_key(model)).or_default() += 1;
            }
        }
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

#[cfg(test)]
mod tests {
    use super::ensure_cost_aggregate;
    use crate::analytics_capture::{AnalyticsCaptureRepository, CaptureTable};
    use crate::{RuntimeStore, StoreError};
    use rusqlite::Connection;
    use serde_json::json;
    use std::path::Path;

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
                let single = super::scope_rows(tx, "root", "agent", &BTreeSet::from([scope(0)]))?;
                let single_steps = steps.swap(0, Ordering::Relaxed);
                let all = super::scope_rows(tx, "root", "agent", &(0..16).map(scope).collect())?;
                let batch_steps = steps.load(Ordering::Relaxed);
                tx.progress_handler(0, None::<fn() -> bool>).expect("remove fixture VM-step counter");
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
                let groups = super::cost_groups(tx, "root", &BTreeSet::from([
                    ("acct".to_owned(), "thread".to_owned(), "r-0".to_owned())
                ]))?;
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
                let other_root = super::cost_groups(tx, "other-root", &BTreeSet::new())?;
                assert_eq!(other_root.len(), 1);
                assert_eq!(other_root[0].count, 1);
                assert_eq!(other_root[0].tokens.input_tokens, Some(30.0));
                assert!(single_steps > 0);
                assert!(batch_steps <= 6 * single_steps,
                    "16 touched turns must batch the agent scan, not repeat it: single={single_steps}, batch={batch_steps}");
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
    fn typed_usage_legacy_migration_keeps_totals_without_reading_usage_rows() {
        use rusqlite::hooks::{AuthAction, AuthContext, Authorization};
        let db = Connection::open_in_memory().expect("database");
        db.execute_batch("CREATE TABLE analytics_usage(seq INTEGER PRIMARY KEY,id TEXT UNIQUE,agent TEXT,root TEXT,thread TEXT,turn TEXT,at REAL,record TEXT);
            CREATE TABLE analytics_cost_rows(id TEXT PRIMARY KEY,root TEXT NOT NULL,agent TEXT NOT NULL,thread TEXT,turn TEXT,seq INTEGER NOT NULL,at REAL NOT NULL,model TEXT,account_key TEXT,response TEXT,input REAL,cached REAL,cache_write REAL,output REAL,uncached INTEGER NOT NULL,counted INTEGER NOT NULL,counted_model TEXT);
            CREATE TABLE analytics_cost_groups(root TEXT NOT NULL,model TEXT,account TEXT NOT NULL,input REAL,cached REAL,cache_write REAL,output REAL,uncached INTEGER NOT NULL,count INTEGER NOT NULL);
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
        super::install(&db).expect("bounded additive migration does not inspect usage records");
        let groups = super::cost_groups(&db, "root", &Default::default()).expect("legacy groups");
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].tokens.input_tokens, Some(10.0));
        assert_eq!(groups[0].tokens.output_tokens, Some(2.0));
        assert_eq!(groups[0].count, 1);
        assert_eq!(groups[0].tokens.cache_write_input_1h_tokens, None);
        super::install(&db).expect("migration is idempotent");
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
            tx.execute("INSERT INTO analytics_agents(id,record) VALUES('agent',?1)", [json!({"id":"agent","accountKey":"acct","threadId":"thread"}).to_string()])?;

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
            assert_eq!(rows[0].tokens.as_ref().expect("known tokens").output, 2);
            assert_eq!(super::recorded_responses(tx, &agents)?.len(), 2);
            let receipts = ["legacy", "final", "missing"].map(|id| ("acct".to_owned(), "thread".to_owned(), id.to_owned())).into();
            let partition = super::partition_journal_receipts(tx, &receipts)?;
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
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
            tx.execute("INSERT INTO analytics_agents(id,record) VALUES('agent',?1)", [json!({"id":"agent","accountKey":"acct","threadId":"thread"}).to_string()])?;

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
            let groups = super::cost_groups(tx, "root", &receipts)?;
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
