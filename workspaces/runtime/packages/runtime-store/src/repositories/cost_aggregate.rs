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
    counted INTEGER NOT NULL, counted_model TEXT, eligible INTEGER NOT NULL, final_evidence INTEGER NOT NULL);
CREATE TABLE analytics_cost_groups (
    root TEXT NOT NULL, model TEXT, account TEXT NOT NULL, input REAL, cached REAL,
    cache_write REAL, output REAL, uncached INTEGER NOT NULL, count INTEGER NOT NULL);
";

/// Indexes of the aggregate, built after the backfill loaded its rows.
const INDEXES: &str = "
CREATE INDEX analytics_cost_rows_scope ON analytics_cost_rows(root, agent, thread, turn);
CREATE INDEX analytics_cost_rows_unmodeled ON analytics_cost_rows(root, agent) WHERE model IS NULL;
CREATE INDEX analytics_cost_rows_response ON analytics_cost_rows(root, agent, response)
    WHERE response IS NOT NULL;
CREATE INDEX analytics_cost_groups_key ON analytics_cost_groups(
    root, model, account, input, cached, cache_write, output, uncached);
";

const ROW_COLUMNS: &str = "id,agent,thread,turn,seq,at,model,account_key,response,\
    input,cached,cache_write,output,uncached,counted,counted_model,eligible,final_evidence";

/// The account key of a usage row or agent record without `accountKey`.
pub const DEFAULT_ACCOUNT: &str = "default";

/// Token counts of one usage row as priced; `None` is an unreported count.
#[derive(Clone, Copy, Debug, Default, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CostTokens {
    pub input_tokens: Option<f64>,
    pub cached_input_tokens: Option<f64>,
    pub cache_write_input_tokens: Option<f64>,
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
            output_tokens: Some(tokens.output as f64),
        }
    }
}

fn cost_observation(record: &serde_json::Value) -> StoreResult<Option<UsageObservation>> {
    match parse_analytics_usage_row(UsageProvider::Codex, record) {
        Ok(observation) => Ok(Some(observation)),
        Err(error) if record.get("studioUsage").is_some() => {
            Err(StoreError::SchemaValidation(error.to_string()))
        }
        // Historical rows with absent counts remain readable and unpriced.
        Err(_) => Ok(None),
    }
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
        let fields = UsageCostFields::deserialize(&value)?;
        let parsed = parse_analytics_usage_row(UsageProvider::Codex, &value);
        let (response_id, model, tokens, evidence) = match parsed {
            Ok(UsageObservation::Response {
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
            Ok(UsageObservation::Cumulative { .. } | UsageObservation::Gap { .. }) => continue,
            Err(error) if value.get("studioUsage").is_some() => {
                return Err(StoreError::SchemaValidation(error.to_string()));
            }
            _ => {
                let Some(response) =
                    studio_runtime_schema::usage::persisted::legacy_response_id(&value)
                else {
                    continue;
                };
                (
                    response.to_owned(),
                    fields.model.as_deref().map(str::to_owned),
                    None,
                    UsageEvidence::Legacy,
                )
            }
        };
        rows.push(RecordedResponse {
            agent_id: row.get(0)?,
            response_id,
            model,
            tokens,
            evidence,
            at: row.get(1)?,
        });
    }
    Ok((rows, false))
}

/// Preserve final live/recovered responses; only missing or legacy evidence accepts journal recovery.
pub fn partition_journal_receipts(
    db: &Connection,
    receipts: &BTreeSet<CostReceipt>,
) -> StoreResult<JournalPartition> {
    let mut result = JournalPartition::default();
    let mut statement = db.prepare_cached(
        "SELECT record FROM analytics_usage WHERE thread=?1
         AND COALESCE(json_extract(record,'$.studioUsage.responseId'),json_extract(record,'$.responseId'))=?2
         AND COALESCE(json_extract(record,'$.accountKey'),'default')=?3",
    )?;
    for receipt @ (account, thread, response) in receipts {
        let mut rows = statement.query(params![thread, response, account])?;
        let mut final_response = false;
        while let Some(row) = rows.next()? {
            let value: serde_json::Value = serde_json::from_str(&row.get::<_, String>(0)?)?;
            match parse_analytics_usage_row(UsageProvider::Claude, &value) {
                Ok(UsageObservation::Response {
                    evidence: UsageEvidence::Live | UsageEvidence::Recovered,
                    ..
                }) => final_response = true,
                Err(error) if value.get("studioUsage").is_some() => {
                    return Err(StoreError::SchemaValidation(error.to_string()));
                }
                _ => {}
            }
        }
        if final_response {
            result.superseded.insert(receipt.clone());
        } else {
            result.accepted.insert(receipt.clone());
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
    eligible: bool,
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
    for row in rows.iter().filter(|row| included(row) && row.eligible) {
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
    let suppressed = |row: &CostRow| {
        !row.eligible
            || row.response.as_deref().is_none_or(|response| {
                winners
                    .get(&(row.agent.as_str(), response))
                    .is_none_or(|winner| winner.id != row.id)
            })
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

/// Create the aggregate in `db` (an analytics database or a test copy of its
/// schema) unless it exists, and fill it from the existing usage rows.
pub fn install(db: &Connection) -> StoreResult<()> {
    let old: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_cost_rows') AND NOT EXISTS(SELECT 1 FROM pragma_table_info('analytics_cost_rows') WHERE name='eligible')", [], |row| row.get(0))?;
    if old {
        db.execute_batch("DROP TABLE analytics_cost_groups; DROP TABLE analytics_cost_rows;")?;
    }
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name='analytics_cost_rows')",
        [],
        |row| row.get(0),
    )?;
    if !exists {
        db.execute_batch(TABLES)?;
        backfill(db)?;
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
            "INSERT INTO analytics_cost_groups VALUES (?,?,?,?,?,?,?,?,?)",
            params![
                root,
                key.model,
                key.account,
                amount(key.input),
                amount(key.cached),
                amount(key.cache_write),
                amount(key.output),
                key.uncached,
                count
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
        eligible: response.is_some(),
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
        "INSERT INTO analytics_cost_rows VALUES (?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?,?)",
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
            row.final_evidence
        ],
    )
}

/// Add `delta` rows to one group of `root`, deleting a group that reaches zero.
fn add_to_group(db: &Connection, root: &str, key: &GroupKey, delta: i64) -> rusqlite::Result<()> {
    const MATCH: &str = "root=?2 AND model IS ?3 AND account=?4 AND input IS ?5 AND cached IS ?6 \
        AND cache_write IS ?7 AND output IS ?8 AND uncached=?9";
    let values = params![
        delta,
        root,
        key.model,
        key.account,
        amount(key.input),
        amount(key.cached),
        amount(key.cache_write),
        amount(key.output),
        key.uncached
    ];
    let updated = db.execute_cached(
        &format!("UPDATE analytics_cost_groups SET count=count+?1 WHERE {MATCH}"),
        values,
    )?;
    if updated == 0 {
        db.execute_cached(
            "INSERT INTO analytics_cost_groups VALUES (?2,?3,?4,?5,?6,?7,?8,?9,?1)",
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
    let mut statement = db.prepare_cached(&format!("SELECT {ROW_COLUMNS} FROM analytics_cost_rows WHERE root=?1 AND agent=?2 AND ((thread IS ?3 AND turn IS ?4) OR (response IS NOT NULL AND response=?5))"))?;
    let mut rows = BTreeMap::new();
    for (thread, turn, response) in turns {
        for row in statement.query_map(
            params![root, agent, thread, turn, response],
            CostRow::from_sql,
        )? {
            let row = row?;
            rows.insert(row.id.clone(), row);
        }
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
            "SELECT model,account,input,cached,cache_write,output,uncached,count
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
    async fn typed_usage_final_deduplicates_legacy_without_merging_request_tiers() {
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_,StoreError>(|tx,_| {
            for (id,response,evidence,input) in [("old","r1","legacy",900),("final","r1","live",10),("large","r2","recovered",250001)] {
                AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&json!({"id":id,"agentId":"agent","rootId":"root","threadId":"thread","turnId":id,"at":1,"last":{},"total":{},
                    "studioUsage":{"kind":"response","responseId":response,"model":"m","evidence":evidence,"tokens":{"uncachedInput":input,"cacheReadInput":3,"cacheWriteInput":2,"output":1}}}))?;
            }
            let groups=super::cost_groups(tx,"root",&Default::default())?;
            let mut inputs=groups.iter().map(|group| { assert_eq!(group.count,1); assert!(group.input_uncached); group.tokens.input_tokens.expect("normalized input") as u64 }).collect::<Vec<_>>();
            inputs.sort();
            assert_eq!(inputs,vec![10,250001],"keep separate request sizes for pricing thresholds, count each final response once");
            Ok(())
        }).await.expect("typed request costs");
    }

    #[tokio::test]
    async fn typed_usage_evidence_queries_bound_rows_and_partition_journal() {
        let temp = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(temp.path())).await.expect("store");
        store.analytics_transaction::<_, StoreError>(|tx, _| {
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
                "outputTokens":next(3),"totalTokens":5});
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
