//! Read-only analytics projections over the Python runtime's persisted rows.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::RuntimeStore;
use crate::{python_json_compact, sha256_hex};
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, params_from_iter, types::Value as SqlValue,
};
use serde_json::{Map, Value, json};
use thiserror::Error;

const TOKEN_FIELDS: [&str; 6] = [
    "inputTokens",
    "cachedInputTokens",
    "cacheWriteInputTokens",
    "outputTokens",
    "reasoningOutputTokens",
    "totalTokens",
];

#[derive(Debug, Error)]
pub enum AnalyticsStoreError {
    #[error("analytics state path is not valid UTF-8")]
    InvalidPath,
    #[error("analytics SQLite read failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
}

/// Read-only analytics and attached Canvas paths in one application state.
#[derive(Clone, Debug)]
pub struct AnalyticsRepository {
    state_root: std::path::PathBuf,
}

impl AnalyticsRepository {
    pub fn new(state_root: impl AsRef<Path>) -> Self {
        Self {
            state_root: state_root.as_ref().to_path_buf(),
        }
    }

    pub fn open_read_only(&self) -> Result<Connection, AnalyticsStoreError> {
        let connection = Connection::open_with_flags(
            self.state_root.join("analytics.sqlite3"),
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        connection.busy_timeout(std::time::Duration::from_secs(3))?;
        connection.pragma_update(None, "query_only", "ON")?;
        let path = self.state_root.join("canvas.sqlite3");
        let uri = read_only_uri(&path)?;
        connection.execute("ATTACH DATABASE ?1 AS canvas", [uri])?;
        Ok(connection)
    }
}

fn read_only_uri(path: &Path) -> Result<String, AnalyticsStoreError> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .map_err(|_| AnalyticsStoreError::InvalidPath)?
            .join(path)
    };
    let raw = absolute.to_str().ok_or(AnalyticsStoreError::InvalidPath)?;
    let mut uri = String::from("file:");
    for byte in raw.bytes() {
        if byte.is_ascii_alphanumeric() || b"/:_-~.".contains(&byte) {
            uri.push(char::from(byte))
        } else {
            uri.push_str(&format!("%{byte:02X}"));
        }
    }
    uri.push_str("?mode=ro");
    Ok(uri)
}

#[derive(Debug, Error)]
pub enum AnalyticsError {
    #[error("Invalid request: {0}")]
    InvalidRequest(&'static str),
    #[error("Invalid stored object {0}")]
    InvalidStoredObject(&'static str),
    #[error("Invalid stored object {table}/{id}")]
    InvalidStoredRow { table: &'static str, id: String },
    #[error("Unknown chat")]
    UnknownAgent,
    #[error("analytics read failed: {0}")]
    Read(#[from] AnalyticsStoreError),
    #[error("SQLite read failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("analytics response serialization failed: {0}")]
    Serialization(#[from] serde_json::Error),
}

#[derive(Clone, Debug, Default)]
pub struct AnalyticsOptions {
    pub agent: Option<String>,
    pub scope: Option<String>,
    pub from: Option<f64>,
    pub to: Option<f64>,
    pub limit: Option<i64>,
    pub offset: Option<i64>,
    pub tool: Option<String>,
    pub export: bool,
    pub view: Option<String>,
    pub detail: Option<String>,
    pub item: Option<String>,
    pub turn: Option<String>,
    pub thread: Option<String>,
    pub turns: Option<String>,
    pub timing: Option<String>,
}

/// Produce the route's JSON representation using one read-only SQLite snapshot.
pub fn read(store: &RuntimeStore, options: &AnalyticsOptions) -> Result<Value, AnalyticsError> {
    let scope = options.scope.as_deref().unwrap_or("agent");
    if !matches!(scope, "agent" | "team" | "all") {
        return Err(AnalyticsError::InvalidRequest("Unknown analytics scope"));
    }
    if let (Some(start), Some(end)) = (options.from, options.to)
        && start > end
    {
        return Err(AnalyticsError::InvalidRequest(
            "Analytics start is after end",
        ));
    }
    let repo = AnalyticsRepository::new(store.root());
    let mut db = repo.open_read_only()?;
    let transaction = db.transaction()?;
    let value = match options.view.as_deref() {
        Some("detail") => detail(&transaction, options, scope)?,
        Some("turn-errors") => turn_errors(&transaction, options, scope)?,
        Some("message-info") if scope != "agent" => {
            return Err(AnalyticsError::InvalidRequest(
                "Select one agent for message metadata",
            ));
        }
        Some("message-info") => message_info(&transaction, store.root(), options)?,
        Some(_) => return Err(AnalyticsError::InvalidRequest("Unknown analytics view")),
        None => rollup(&transaction, options, scope)?,
    };
    transaction.commit()?;
    Ok(value)
}

pub fn export(store: &RuntimeStore, options: &AnalyticsOptions) -> Result<Vec<u8>, AnalyticsError> {
    let value = read(store, options)?;
    Ok(serde_json::to_vec(&value)?)
}

fn rollup(
    db: &Connection,
    options: &AnalyticsOptions,
    scope: &str,
) -> Result<Value, AnalyticsError> {
    let agent_id = options.agent.as_deref();
    let (root, mut agents) = selected_agents(db, options, scope, false)?;
    let limit = options.limit.unwrap_or(50).clamp(1, 500);
    let offset = options.offset.unwrap_or(0).max(0);
    let usage_filter = scope_filter(scope, agent_id, &root, options.from, options.to, None);
    usage_filter.validate(db, "analytics_usage")?;
    usage_filter.validate(db, "analytics_items")?;
    let usage_where = usage_filter.sql.as_str();
    let billable = usage_filter.clone().and(BILLABLE_USAGE, []);
    let provisional_filter = usage_filter
        .clone()
        .and(&format!("NOT ({BILLABLE_USAGE})"), []);
    let provisional_total = provisional_filter.count(db, "analytics_usage")? as usize;
    let mut provisional = provisional_filter.page(
        db,
        "analytics_usage",
        "at DESC,seq DESC",
        if options.export { -1 } else { 100 },
        0,
    )?;
    provisional.reverse();
    let mut timeline = billable.page(
        db,
        "analytics_usage",
        "at DESC,seq DESC",
        if options.export { -1 } else { 500 },
        0,
    )?;
    timeline.reverse();
    let chart = chart_rows(db, &billable, options.export)?;
    let usage_totals = usage_grouped(db, &usage_filter, "''")?
        .remove("")
        .unwrap_or_default();
    let per_model = usage_grouped(
        db,
        &usage_filter,
        "COALESCE(json_extract(record,'$.model'),'')",
    )?;
    let per_account = usage_grouped(
        db,
        &usage_filter,
        "COALESCE(json_extract(record,'$.accountKey'),'default')",
    )?;
    let per_agent = usage_grouped(db, &usage_filter, "agent")?;
    let call_filter = scope_filter(
        scope,
        agent_id,
        &root,
        options.from,
        options.to,
        Some(options.tool.as_deref().unwrap_or_default()),
    );
    let calls_total = call_filter.count(db, "analytics_items")?;
    let calls = call_filter.page(
        db,
        "analytics_items",
        "at DESC,id",
        if options.export { -1 } else { limit },
        if options.export { 0 } else { offset },
    )?;
    let turns = usage_filter.page(
        db,
        "analytics_turns",
        "at DESC,id DESC",
        if options.export { -1 } else { 100 },
        0,
    )?;
    let (observed_agents, turns_seen) = observed_counts(db, &usage_filter)?;
    let selected_ids: BTreeSet<String> = agents
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let total_turns = usage_filter.count(db, "analytics_turns")?;
    let tracking: f64 = metadata(db, "trackingSince")?.unwrap_or(0.0);
    let capture: CaptureErrors = metadata(db, "captureErrors")?.unwrap_or_default();
    let selected = ScopeFilter::new().members("agent", selected_ids.iter().cloned());
    let selected_json = json!(selected_ids).to_string();
    let history_filter = ScopeFilter::new().and("(COALESCE(NULLIF(json_extract(record,'$.agent'),''),NULLIF(json_extract(record,'$.agentId'),'')) IS NULL OR json_extract(record,'$.agent') IN (SELECT value FROM json_each(?1)) OR json_extract(record,'$.agentId') IN (SELECT value FROM json_each(?1)))",[SqlValue::Text(selected_json)]);
    let history = history_filter.optional_page(db, "analytics_history", "rowid")?;
    let monitors = ScopeFilter::new()
        .members(
            "json_extract(record,'$.agent')",
            selected_ids.iter().cloned(),
        )
        .range("json_extract(record,'$.created')", options.from, options.to)
        .and("json_extract(record,'$.created')>=0", [])
        .optional_page(db, "canvas.runtime_monitors", "rowid")?;
    let visible_monitors = monitors
        .iter()
        .map(|row| {
            project(
                row,
                &[
                    "id",
                    "agent",
                    "status",
                    "created",
                    "finished",
                    "bytes",
                    "exitCode",
                    "error",
                    "timeout_ms",
                ],
            )
        })
        .collect();
    let event_counts = selected
        .clone()
        .range("created", options.from, options.to)
        .counts(db, "canvas.runtime_events", "status||':'||kind")?;
    let approval_counts = ScopeFilter::new()
        .members(
            "json_extract(record,'$.agent')",
            selected_ids.iter().cloned(),
        )
        .counts(
            db,
            "canvas.runtime_requests",
            "json_extract(record,'$.status')",
        )?;
    let mut notification_filter = selected.and("hour IS NOT NULL", []);
    if let Some(from) = options.from {
        notification_filter = notification_filter.and("hour+3600>?", [SqlValue::Real(from)]);
    }
    notification_filter = notification_filter.range("hour", None, options.to);
    let notification_rows = query(
        db,
        &format!(
            "SELECT id,agent,root,method,hour,count,bytes FROM analytics_notifications {}",
            notification_filter.sql
        ),
        params_from_iter(notification_filter.params),
        |r| {
            Ok(Notification {
                id: r.get(0)?,
                agent: r.get(1)?,
                root: r.get(2)?,
                method: r.get(3)?,
                hour: r.get(4)?,
                count: r.get(5)?,
                bytes: r.get(6)?,
            })
        },
    )?;
    let tool_filter = usage_filter.clone().and("is_tool=1", []);
    let tools = item_grouped(
        db,
        &tool_filter,
        "COALESCE(json_extract(record,'$.name'),'')",
        "COALESCE(json_extract(record,'$.type'),'')",
    )?
    .into_iter()
    .map(|((name, kind), totals)| Tool {
        name,
        model_input_bytes: if kind == "modelToolCall" {
            totals.input_bytes
        } else {
            None
        },
        model_output_bytes: if kind == "modelToolCall" {
            totals.output_bytes
        } else {
            None
        },
        kind,
        totals,
    })
    .collect::<Vec<_>>();
    let agent_calls: BTreeMap<_, _> = item_grouped(db, &call_filter, "agent", TOOL_BOUNDARY)?
        .into_iter()
        .collect();
    let boundary_totals: BTreeMap<_, _> = item_grouped(db, &call_filter, "''", TOOL_BOUNDARY)?
        .into_iter()
        .collect();
    let model_tool = boundary_totals
        .get(&(String::new(), "model".into()))
        .cloned()
        .unwrap_or_default();
    let protocol_tool = boundary_totals
        .get(&(String::new(), "protocol".into()))
        .cloned()
        .unwrap_or_default();
    let non_tools = usage_filter.clone().and("is_tool!=1", []);
    let non_tool_total = non_tools.count(db, "analytics_items")? as usize;
    let non_tool = non_tools.page(
        db,
        "analytics_items",
        "at DESC,id",
        if options.export { -1 } else { 100 },
        0,
    )?;
    let native_filter = usage_filter.clone().and(
        "type='contextCompaction' AND json_extract(record,'$.finishedAt') IS NOT NULL",
        [],
    );
    let mut compactions = native_filter.page(db, "analytics_items", "at DESC,id", -1, 0)?;
    let snapshot_filter = usage_filter.clone().and("type='compactionSnapshot'", []);
    let snapshots = snapshot_filter.page(db, "analytics_items", "at DESC,id", -1, 0)?;
    let native_compactions: HashSet<_> = compactions
        .iter()
        .map(|r| (string(r, "agentId"), string(r, "turnId")))
        .collect();
    compactions.extend(
        snapshots
            .iter()
            .filter(|r| !native_compactions.contains(&(string(r, "agentId"), string(r, "turnId"))))
            .cloned(),
    );
    let item_counts = query(
        db,
        &format!(
            "SELECT COALESCE(json_extract(record,'$.type'),''),COUNT(*),CASE WHEN COUNT(json_extract(record,'$.input.bytes'))+COUNT(json_extract(record,'$.output.bytes'))=0 THEN NULL ELSE COALESCE(SUM(json_extract(record,'$.input.bytes')),0)+COALESCE(SUM(json_extract(record,'$.output.bytes')),0) END,CASE WHEN COUNT(json_extract(record,'$.input.chars'))+COUNT(json_extract(record,'$.output.chars'))=0 THEN NULL ELSE COALESCE(SUM(json_extract(record,'$.input.chars')),0)+COALESCE(SUM(json_extract(record,'$.output.chars')),0) END FROM analytics_items {usage_where} GROUP BY json_extract(record,'$.type') ORDER BY MIN(at),MIN(id)"
        ),
        params_from_iter(&usage_filter.params),
        |r| {
            Ok(ItemCount {
                kind: r.get(0)?,
                count: r.get(1)?,
                bytes: r.get(2)?,
                chars: r.get(3)?,
            })
        },
    )?;
    let breakdown = item_breakdown(db, &usage_filter)?;
    overlay_agents(db, &mut agents)?;
    let rate_filter = account_filter(&agents, options);
    let total_limits = rate_filter.count(db, "analytics_limits")?;
    let rates = rate_filter.rates(db, if options.export { -1 } else { 100 }, 0)?;
    let hit_rate = if usage_totals.cache_hit_rate_samples == usage_totals.usage_samples
        && usage_totals.cache_input > 0.0
    {
        usage_totals
            .tokens
            .cached_input_tokens
            .map(|n| n / usage_totals.cache_input)
    } else {
        None
    };
    let summary = Summary {
        agents: observed_agents,
        turns: turns_seen,
        provisional_usage_samples: provisional_total,
        model_tool_calls: model_tool.count,
        protocol_tool_calls: protocol_tool.count,
        observed_tool_rows: calls_total,
        tool_calls: model_tool.count,
        failed_tool_calls: model_tool.failed,
        model_failed_tool_calls: model_tool.failed,
        protocol_failed_tool_calls: protocol_tool.failed,
        compactions: compactions.len(),
        cache_hit_rate: hit_rate,
        cache_hit_rate_total_samples: usage_totals.usage_samples,
        input_bytes: model_tool.input_bytes,
        output_bytes: model_tool.output_bytes,
        model_input_bytes: model_tool.input_bytes,
        model_output_bytes: model_tool.output_bytes,
        protocol_input_bytes: protocol_tool.input_bytes,
        protocol_output_bytes: protocol_tool.output_bytes,
        duration_ms: model_tool.duration_total,
        model_duration_ms: model_tool.duration_total,
        protocol_duration_ms: protocol_tool.duration_total,
        usage: usage_totals.clone(),
    };
    let output_agents = agents
        .iter()
        .map(|record| {
            let agent = string(record, "id");
            let totals = per_agent.get(&agent).cloned().unwrap_or_default();
            let model = agent_calls
                .get(&(agent.clone(), "model".into()))
                .cloned()
                .unwrap_or_default();
            let protocol = agent_calls
                .get(&(agent.clone(), "protocol".into()))
                .cloned()
                .unwrap_or_default();
            AgentTotals {
                record: record.clone(),
                tokens: totals.tokens,
                usage_samples: totals.usage_samples,
                tool_calls: model.count,
                model_tool_calls: model.count,
                protocol_tool_calls: protocol.count,
                failed_tool_calls: model.failed,
                protocol_failed_tool_calls: protocol.failed,
                compactions: compactions
                    .iter()
                    .filter(|r| string(r, "agentId") == agent)
                    .count(),
                duration: model.duration,
                protocol_duration: protocol.duration,
            }
        })
        .collect();
    let model_rows = per_model
        .into_iter()
        .map(|(model, t)| ModelUsage {
            model: if model.is_empty() { None } else { Some(model) },
            samples: t.usage_samples,
            tokens: t.tokens,
        })
        .collect();
    let account_rows = per_account
        .into_iter()
        .map(|(account_key, t)| AccountUsage {
            account_key,
            samples: t.usage_samples,
            tokens: t.tokens,
        })
        .collect();
    let generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    let response = AnalyticsResponse {
        version: 1,
        generated_at,
        filters: AnalyticsFiltersResponse {
            agent: agent_id,
            scope,
            from: options.from,
            to: options.to,
            tool: options.tool.as_deref().filter(|tool| !tool.is_empty()),
        },
        coverage: AnalyticsCoverageResponse {
            tracking_since: tracking,
            capture_errors: capture,
            history_errors: history
                .iter()
                .filter(|row| row["status"] == "error")
                .cloned()
                .collect(),
            provisional_usage_samples: provisional_total,
            token_attribution: "provider_usage_only",
            payload_measurement: "observed_protocol_payload",
            history: "live_and_stored_history",
            notes: vec![
                "Tool filters affect tool calls only. Provider usage is scoped to the selected agents and time.",
                "Item date filters use start time when known, otherwise completion time or the first observation. History can establish an earlier start.",
                "Input and output sizes measure UTF-8 text or compact JSON observed at the protocol boundary, not context tokens or billed tokens.",
                "Cached input is part of input. Reasoning output is part of output. Do not add these subsets twice.",
                "Token totals use response-id records when available for a turn. Other notices in that turn remain provisional and are excluded.",
                "Turns without response records use deduplicated last-usage notices. Current requests can remain pending until rollout import catches up. Missing requests are not inferred.",
                "Reasoning items expose only protocol-visible content. Hidden reasoning text and unreported context overhead are unavailable.",
                "Turn duration and first-output delay include tool execution and waits, not model-only latency.",
                "Notification histogram buckets are full UTC hours and can overlap the selected time range.",
                "Approval counts describe current stored requests across all time. Monitor and delivery-event filters use creation time.",
                "Model and native tool calls, failures, and durations are separate populations. Main aliases describe model calls only.",
                "Duration sums include parallel calls and are not elapsed session time. Missing measurements remain null.",
            ],
        },
        summary,
        agents,
        agent_totals: output_agents,
        tools,
        model_totals: model_rows,
        account_totals: account_rows,
        operations: AnalyticsOperations {
            monitors: visible_monitors,
            event_counts,
            approval_counts,
        },
        notifications: notification_rows,
        history,
        rate_limits: rates,
        turns,
        timeline,
        chart_buckets: chart,
        timeline_total: usage_totals.usage_samples,
        provisional_usage: provisional,
        calls,
        item_records: non_tool,
        item_records_total: non_tool_total,
        item_breakdown: breakdown,
        compactions,
        compaction_snapshots: snapshots,
        pagination: AnalyticsPagination {
            limit,
            offset,
            total: calls_total,
            has_more: !options.export && offset + limit < calls_total,
        },
        items: item_counts,
        detail_pagination: AnalyticsDetailPagination {
            rate_limits: AnalyticsPagination {
                limit: if options.export { total_limits } else { 100 },
                offset: 0,
                total: total_limits,
                has_more: !options.export && total_limits > 100,
            },
            turns: AnalyticsPagination {
                limit: if options.export { total_turns } else { 100 },
                offset: 0,
                total: total_turns,
                has_more: !options.export && total_turns > 100,
            },
        },
    };
    Ok(serde_json::to_value(response)?)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Summary {
    #[serde(flatten)]
    usage: UsageTotals,
    agents: i64,
    turns: i64,
    provisional_usage_samples: usize,
    model_tool_calls: i64,
    protocol_tool_calls: i64,
    observed_tool_rows: i64,
    tool_calls: i64,
    failed_tool_calls: i64,
    model_failed_tool_calls: i64,
    protocol_failed_tool_calls: i64,
    compactions: usize,
    cache_hit_rate: Option<f64>,
    cache_hit_rate_total_samples: usize,
    input_bytes: Option<i64>,
    output_bytes: Option<i64>,
    model_input_bytes: Option<i64>,
    model_output_bytes: Option<i64>,
    protocol_input_bytes: Option<i64>,
    protocol_output_bytes: Option<i64>,
    duration_ms: Option<f64>,
    model_duration_ms: Option<f64>,
    protocol_duration_ms: Option<f64>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AgentTotals {
    #[serde(flatten)]
    record: Value,
    tokens: Tokens<Option<f64>>,
    usage_samples: usize,
    tool_calls: i64,
    model_tool_calls: i64,
    protocol_tool_calls: i64,
    failed_tool_calls: i64,
    protocol_failed_tool_calls: i64,
    compactions: usize,
    duration: Duration,
    protocol_duration: Duration,
}
#[derive(serde::Serialize)]
struct ModelUsage {
    model: Option<String>,
    samples: usize,
    tokens: Tokens<Option<f64>>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AccountUsage {
    account_key: String,
    samples: usize,
    tokens: Tokens<Option<f64>>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Tool {
    name: String,
    #[serde(rename = "type")]
    kind: String,
    #[serde(flatten)]
    totals: ItemTotals,
    model_input_bytes: Option<i64>,
    model_output_bytes: Option<i64>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsResponse<'a> {
    version: u8,
    generated_at: f64,
    filters: AnalyticsFiltersResponse<'a>,
    coverage: AnalyticsCoverageResponse,
    summary: Summary,
    agents: Vec<Value>,
    agent_totals: Vec<AgentTotals>,
    tools: Vec<Tool>,
    model_totals: Vec<ModelUsage>,
    account_totals: Vec<AccountUsage>,
    operations: AnalyticsOperations,
    notifications: Vec<Notification>,
    history: Vec<Value>,
    rate_limits: Vec<RateLimit>,
    turns: Vec<Value>,
    timeline: Vec<Value>,
    chart_buckets: Vec<Value>,
    timeline_total: usize,
    provisional_usage: Vec<Value>,
    calls: Vec<Value>,
    item_records: Vec<Value>,
    item_records_total: usize,
    item_breakdown: Vec<ItemBreakdown>,
    compactions: Vec<Value>,
    compaction_snapshots: Vec<Value>,
    pagination: AnalyticsPagination,
    items: Vec<ItemCount>,
    detail_pagination: AnalyticsDetailPagination,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsFiltersResponse<'a> {
    agent: Option<&'a str>,
    scope: &'a str,
    from: Option<f64>,
    to: Option<f64>,
    tool: Option<&'a str>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsCoverageResponse {
    tracking_since: f64,
    capture_errors: CaptureErrors,
    history_errors: Vec<Value>,
    provisional_usage_samples: usize,
    token_attribution: &'static str,
    payload_measurement: &'static str,
    history: &'static str,
    notes: Vec<&'static str>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsOperations {
    monitors: Vec<Value>,
    event_counts: BTreeMap<String, i64>,
    approval_counts: BTreeMap<String, i64>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsPagination {
    limit: i64,
    offset: i64,
    total: i64,
    has_more: bool,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct AnalyticsDetailPagination {
    rate_limits: AnalyticsPagination,
    turns: AnalyticsPagination,
}

#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct Tokens<T> {
    input_tokens: T,
    cached_input_tokens: T,
    cache_write_input_tokens: T,
    output_tokens: T,
    reasoning_output_tokens: T,
    total_tokens: T,
}
#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct UsageTotals {
    usage_samples: usize,
    exact_response_samples: usize,
    legacy_usage_samples: usize,
    baseline_missing_samples: usize,
    cache_hit_rate_samples: usize,
    #[serde(skip)]
    cache_input: f64,
    peak_context_tokens: Option<f64>,
    peak_context_percent: Option<f64>,
    tokens: Tokens<Option<f64>>,
    token_observations: Tokens<i64>,
}

fn usage_grouped(
    db: &Connection,
    filter: &ScopeFilter,
    group: &str,
) -> Result<BTreeMap<String, UsageTotals>, AnalyticsError> {
    let exact = "NULLIF(json_extract(record,'$.responseId'),'') IS NOT NULL";
    let cache_ok = "json_extract(record,'$.delta.inputTokens') IS NOT NULL AND json_extract(record,'$.delta.cachedInputTokens') IS NOT NULL AND (json_extract(record,'$.inputTokensAreUncached')=1 OR json_extract(record,'$.delta.cachedInputTokens')<=json_extract(record,'$.delta.inputTokens'))";
    let cache_input = "CASE WHEN json_extract(record,'$.inputTokensAreUncached')=1 THEN json_extract(record,'$.delta.inputTokens')+json_extract(record,'$.delta.cachedInputTokens')+COALESCE(json_extract(record,'$.delta.cacheWriteInputTokens'),0) ELSE json_extract(record,'$.delta.inputTokens') END";
    let sums = TOKEN_FIELDS.map(|field| format!("SUM(json_extract(record,'$.delta.{field}')),COUNT(json_extract(record,'$.delta.{field}'))")).join(",");
    let sql = format!(
        "SELECT {group},COUNT(*),SUM(CASE WHEN {exact} THEN 1 ELSE 0 END),SUM(CASE WHEN json_extract(record,'$.baselineMissing')=1 THEN 1 ELSE 0 END),MAX(json_extract(record,'$.last.totalTokens')),MAX(CASE WHEN json_extract(record,'$.modelContextWindow')>0 THEN json_extract(record,'$.last.totalTokens')*1.0/json_extract(record,'$.modelContextWindow')*100 END),SUM(CASE WHEN {cache_ok} THEN 1 ELSE 0 END),SUM(CASE WHEN {cache_ok} THEN {cache_input} ELSE 0 END),{sums} FROM analytics_usage {0} AND {BILLABLE_USAGE} GROUP BY {group} ORDER BY {group}",
        filter.sql
    );
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(filter.params.clone()), |row| {
        let count = row.get::<_, i64>(1)?.max(0) as usize;
        let exact = row.get::<_, i64>(2)?.max(0) as usize;
        let totals = UsageTotals {
            usage_samples: count,
            exact_response_samples: exact,
            legacy_usage_samples: count.saturating_sub(exact),
            baseline_missing_samples: row.get::<_, i64>(3)?.max(0) as usize,
            peak_context_tokens: row.get(4)?,
            peak_context_percent: row.get(5)?,
            cache_hit_rate_samples: row.get::<_, i64>(6)?.max(0) as usize,
            cache_input: row.get::<_, Option<f64>>(7)?.unwrap_or_default(),
            tokens: Tokens {
                input_tokens: row.get(8)?,
                cached_input_tokens: row.get(10)?,
                cache_write_input_tokens: row.get(12)?,
                output_tokens: row.get(14)?,
                reasoning_output_tokens: row.get(16)?,
                total_tokens: row.get(18)?,
            },
            token_observations: Tokens {
                input_tokens: row.get(9)?,
                cached_input_tokens: row.get(11)?,
                cache_write_input_tokens: row.get(13)?,
                output_tokens: row.get(15)?,
                reasoning_output_tokens: row.get(17)?,
                total_tokens: row.get(19)?,
            },
        };
        Ok((row.get::<_, String>(0)?, totals))
    })?;
    rows.collect::<Result<BTreeMap<_, _>, _>>()
        .map_err(AnalyticsError::from)
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemBreakdown {
    #[serde(rename = "type")]
    kind: String,
    payload_boundary: String,
    category: Option<String>,
    role: Option<String>,
    count: i64,
    input_bytes: Option<i64>,
    output_bytes: Option<i64>,
    input_measurements: i64,
    output_measurements: i64,
}
#[derive(serde::Serialize)]
struct ItemCount {
    #[serde(rename = "type")]
    kind: String,
    count: i64,
    bytes: Option<i64>,
    chars: Option<i64>,
}
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct RateLimit {
    account_key: String,
    at: f64,
    data: Value,
}
fn item_breakdown(
    db: &Connection,
    filter: &ScopeFilter,
) -> Result<Vec<ItemBreakdown>, AnalyticsError> {
    query(
        db,
        &format!(
            "SELECT COALESCE(json_extract(record,'$.type'),''),COALESCE(json_extract(record,'$.payloadBoundary'),'protocol'),json_extract(record,'$.category'),json_extract(record,'$.role'),COUNT(*),SUM(json_extract(record,'$.input.bytes')),SUM(json_extract(record,'$.output.bytes')),COUNT(json_extract(record,'$.input')),COUNT(json_extract(record,'$.output')) FROM analytics_items {} GROUP BY 1,2,3,4 ORDER BY 1,2,3,4",
            filter.sql
        ),
        params_from_iter(&filter.params),
        |r| {
            Ok(ItemBreakdown {
                kind: r.get(0)?,
                payload_boundary: r.get(1)?,
                category: r.get(2)?,
                role: r.get(3)?,
                count: r.get(4)?,
                input_bytes: r.get(5)?,
                output_bytes: r.get(6)?,
                input_measurements: r.get(7)?,
                output_measurements: r.get(8)?,
            })
        },
    )
}
const TOOL_BOUNDARY: &str =
    "CASE WHEN json_extract(record,'$.payloadBoundary')='model' THEN 'model' ELSE 'protocol' END";
const BILLABLE_USAGE: &str = "(NULLIF(json_extract(record,'$.responseId'),'') IS NOT NULL OR NOT EXISTS(SELECT 1 FROM analytics_usage auth WHERE auth.agent=analytics_usage.agent AND auth.thread IS analytics_usage.thread AND auth.turn IS analytics_usage.turn AND NULLIF(json_extract(auth.record,'$.responseId'),'') IS NOT NULL))";

#[derive(Clone, Default, serde::Serialize)]
struct Duration {
    count: i64,
    min: Option<f64>,
    max: Option<f64>,
    mean: Option<f64>,
    p50: Option<f64>,
    p95: Option<f64>,
}
#[derive(Clone, Default, serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct ItemTotals {
    #[serde(rename = "calls")]
    count: i64,
    failed: i64,
    input_measurements: i64,
    output_measurements: i64,
    input_bytes: Option<i64>,
    output_bytes: Option<i64>,
    image_count: Option<i64>,
    #[serde(rename = "durationMs")]
    duration_total: Option<f64>,
    duration: Duration,
    #[serde(rename = "payloadBoundary")]
    boundary: String,
}
fn item_grouped(
    db: &Connection,
    filter: &ScopeFilter,
    key: &str,
    kind: &str,
) -> Result<Vec<((String, String), ItemTotals)>, AnalyticsError> {
    let sql = format!("WITH population AS (
        SELECT {key} AS k,{kind} AS t,record,ROW_NUMBER() OVER(ORDER BY at DESC,id) AS position,
            json_extract(record,'$.durationMs') AS duration,
            FIRST_VALUE(COALESCE(json_extract(record,'$.payloadBoundary'),'protocol')) OVER(PARTITION BY {key},{kind} ORDER BY at DESC,id) AS boundary
        FROM analytics_items {}),
        ordered AS (SELECT k,t,duration,ROW_NUMBER() OVER(PARTITION BY k,t ORDER BY duration) AS rank,
            COUNT(*) OVER(PARTITION BY k,t) AS n FROM population WHERE duration>=0),
        durations AS (SELECT k,t,COUNT(*) AS n,MIN(duration) AS low,MAX(duration) AS high,AVG(duration) AS mean,
            AVG(CASE WHEN rank IN ((n+1)/2,(n+2)/2) THEN duration END) AS median,
            MAX(CASE WHEN rank=(n*95+99)/100 THEN duration END) AS p95 FROM ordered GROUP BY k,t)
        SELECT p.k,p.t,COUNT(*),SUM(json_extract(record,'$.status')='failed'),
            COUNT(json_extract(record,'$.input')),COUNT(json_extract(record,'$.output')),
            SUM(json_extract(record,'$.input.bytes')),SUM(json_extract(record,'$.output.bytes')),
            CASE WHEN COUNT(json_extract(record,'$.input.imageCount'))+COUNT(json_extract(record,'$.output.imageCount'))>0
                THEN COALESCE(SUM(json_extract(record,'$.input.imageCount')),0)+COALESCE(SUM(json_extract(record,'$.output.imageCount')),0) END,
            SUM(duration),boundary,COALESCE(d.n,0),d.low,d.high,d.mean,d.median,d.p95
        FROM population p LEFT JOIN durations d ON p.k=d.k AND p.t=d.t GROUP BY p.k,p.t
        ORDER BY COALESCE(SUM(json_extract(record,'$.output.bytes')),0) DESC,MIN(position)",filter.sql);
    let mut stmt = db.prepare(&sql)?;
    Ok(stmt
        .query_map(params_from_iter(&filter.params), |r| {
            Ok((
                (r.get(0)?, r.get(1)?),
                ItemTotals {
                    count: r.get(2)?,
                    failed: r.get::<_, Option<i64>>(3)?.unwrap_or_default(),
                    input_measurements: r.get(4)?,
                    output_measurements: r.get(5)?,
                    input_bytes: r.get(6)?,
                    output_bytes: r.get(7)?,
                    image_count: r.get(8)?,
                    duration_total: r.get(9)?,
                    boundary: r.get(10)?,
                    duration: Duration {
                        count: r.get(11)?,
                        min: r.get(12)?,
                        max: r.get(13)?,
                        mean: r.get(14)?,
                        p50: r.get(15)?,
                        p95: r.get(16)?,
                    },
                },
            ))
        })?
        .collect::<Result<_, _>>()?)
}
fn observed_counts(db: &Connection, filter: &ScopeFilter) -> Result<(i64, i64), AnalyticsError> {
    let sql = format!("WITH observations AS (
        SELECT agent,turn FROM analytics_usage {} AND {BILLABLE_USAGE}
        UNION SELECT agent,turn FROM analytics_items {})
        SELECT COUNT(DISTINCT agent),(SELECT COUNT(*) FROM observations WHERE turn IS NOT NULL) FROM observations",filter.sql,filter.sql);
    Ok(db.query_row(
        &sql,
        params_from_iter(filter.params.iter().chain(&filter.params)),
        |r| Ok((r.get(0)?, r.get(1)?)),
    )?)
}
fn chart_rows(
    db: &Connection,
    filter: &ScopeFilter,
    export: bool,
) -> Result<Vec<Value>, AnalyticsError> {
    let sql = format!("SELECT id,record FROM (SELECT id,record,at,seq,
        ROW_NUMBER() OVER(PARTITION BY CAST(at/3600 AS INTEGER) ORDER BY json_extract(record,'$.last.totalTokens')*1.0/json_extract(record,'$.modelContextWindow') DESC,at,seq) AS rank
        FROM analytics_usage {} AND at>=0 AND json_extract(record,'$.modelContextWindow')!=0 AND json_extract(record,'$.last.totalTokens') IS NOT NULL)
        WHERE rank=1 ORDER BY at,seq",filter.sql);
    let points = records(
        db,
        &sql,
        params_from_iter(&filter.params),
        "analytics_usage",
    )?;
    let mut buckets: HashMap<i64, Value> = HashMap::new();
    let mut width = 3600.0;
    for point in points {
        let bucket = (number_at(&point, "at").unwrap_or_default() / width) as i64;
        if buckets
            .get(&bucket)
            .is_none_or(|old| context_percent(old) < context_percent(&point))
        {
            buckets.insert(bucket, point);
        }
        if !export && buckets.len() > 500 {
            width *= 2.0;
            let mut compacted: HashMap<i64, Value> = HashMap::new();
            for point in buckets.into_values() {
                let key = (number_at(&point, "at").unwrap_or_default() / width) as i64;
                if compacted
                    .get(&key)
                    .is_none_or(|old| context_percent(old) < context_percent(&point))
                {
                    compacted.insert(key, point);
                }
            }
            buckets = compacted;
        }
    }
    let mut points: Vec<_> = buckets.into_values().collect();
    points.sort_by(|a, b| {
        number_at(a, "at")
            .partial_cmp(&number_at(b, "at"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(points)
}
#[derive(Default, serde::Serialize, serde::Deserialize)]
struct CaptureErrors {
    count: u64,
    #[serde(default)]
    last: Value,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}
fn metadata<T: serde::de::DeserializeOwned>(
    db: &Connection,
    key: &str,
) -> Result<Option<T>, AnalyticsError> {
    let raw: Option<String> = db
        .query_row("SELECT value FROM analytics_meta WHERE key=?", [key], |r| {
            r.get(0)
        })
        .optional()?;
    raw.map(|raw| {
        serde_json::from_str(&raw).map_err(|_| AnalyticsError::InvalidStoredRow {
            table: "analytics_meta",
            id: key.into(),
        })
    })
    .transpose()
}
fn query<T, P: rusqlite::Params>(
    db: &Connection,
    sql: &str,
    params: P,
    read: impl FnMut(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Vec<T>, AnalyticsError> {
    Ok(db
        .prepare(sql)?
        .query_map(params, read)?
        .collect::<Result<_, _>>()?)
}
#[derive(serde::Serialize)]
struct Notification {
    id: String,
    agent: Option<String>,
    root: Option<String>,
    method: Option<String>,
    hour: Option<i64>,
    count: i64,
    bytes: i64,
}
fn project(row: &Value, fields: &[&str]) -> Value {
    let mut row = row.clone();
    if let Some(object) = row.as_object_mut() {
        object.retain(|key, _| fields.contains(&key.as_str()));
    }
    row
}
fn records<P: rusqlite::Params>(
    db: &Connection,
    sql: &str,
    params: P,
    table: &'static str,
) -> Result<Vec<Value>, AnalyticsError> {
    let sql = sql.replacen("SELECT record FROM", "SELECT id,record FROM", 1);
    let mut stmt = db.prepare(&sql)?;
    let rows = stmt.query_map(params, |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (id, raw) = row?;
        let value: Value =
            serde_json::from_str(&raw).map_err(|_| AnalyticsError::InvalidStoredRow {
                table,
                id: id.clone(),
            })?;
        if !value.is_object() || !valid_stored_shape(table, &value) {
            return Err(AnalyticsError::InvalidStoredRow { table, id });
        }
        Ok(value)
    })
    .collect()
}

fn required_fields(table: &str) -> &[&str] {
    match table.strip_prefix("canvas.").unwrap_or(table) {
        "analytics_agents" => &["id"],
        "analytics_usage" => &["agentId", "at"],
        "analytics_items" => &["agentId", "type"],
        "analytics_turns" => &["agentId", "turnId", "status"],
        "analytics_history" => &["status"],
        "runtime_monitors" => &["id", "agent"],
        "runtime_requests" => &["id", "agent", "status"],
        _ => &[],
    }
}
fn valid_stored_shape(table: &str, value: &Value) -> bool {
    required_fields(table)
        .iter()
        .all(|field| value.get(field).is_some_and(|item| !item.is_null()))
}
#[derive(Clone)]
struct ScopeFilter {
    sql: String,
    params: Vec<SqlValue>,
}
impl ScopeFilter {
    fn new() -> Self {
        Self {
            sql: "WHERE 1".into(),
            params: Vec::new(),
        }
    }
    fn and(mut self, predicate: &str, params: impl IntoIterator<Item = SqlValue>) -> Self {
        self.sql.push_str(" AND ");
        self.sql.push_str(predicate);
        self.params.extend(params);
        self
    }
    fn members(self, column: &str, values: impl IntoIterator<Item = String>) -> Self {
        let values: Vec<_> = values.into_iter().collect();
        self.and(
            &format!("{column} IN (SELECT value FROM json_each(?))"),
            [SqlValue::Text(json!(values).to_string())],
        )
    }
    fn range(mut self, column: &str, from: Option<f64>, to: Option<f64>) -> Self {
        for (op, value) in [(">=", from), ("<=", to)] {
            if let Some(value) = value {
                self = self.and(&format!("{column}{op}?"), [SqlValue::Real(value)]);
            }
        }
        self
    }
    fn validate(&self, db: &Connection, table: &'static str) -> Result<(), AnalyticsError> {
        let missing = required_fields(table)
            .iter()
            .map(|field| format!("json_extract(record,'$.{field}') IS NULL"))
            .collect::<Vec<_>>()
            .join(" OR ");
        let invalid = format!(
            "CASE WHEN NOT json_valid(record) THEN 1 WHEN json_type(record)!='object' THEN 1 ELSE {missing} END"
        );
        let id: Option<String> = db
            .query_row(
                &format!(
                    "SELECT id FROM {table} {} AND ({invalid}) LIMIT 1",
                    self.sql
                ),
                params_from_iter(&self.params),
                |r| r.get(0),
            )
            .optional()?;
        if let Some(id) = id {
            return Err(AnalyticsError::InvalidStoredRow { table, id });
        }
        Ok(())
    }
    fn optional_page(
        &self,
        db: &Connection,
        table: &'static str,
        order: &str,
    ) -> Result<Vec<Value>, AnalyticsError> {
        if table_exists(db, table)? {
            self.page(db, table, order, -1, 0)
        } else {
            Ok(Vec::new())
        }
    }
    fn counts(
        &self,
        db: &Connection,
        table: &str,
        key: &str,
    ) -> Result<BTreeMap<String, i64>, AnalyticsError> {
        if !table_exists(db, table)? {
            return Ok(BTreeMap::new());
        }
        Ok(query(
            db,
            &format!(
                "SELECT {key},COUNT(*) FROM {table} {} GROUP BY {key}",
                self.sql
            ),
            params_from_iter(&self.params),
            |r| Ok((r.get(0)?, r.get(1)?)),
        )?
        .into_iter()
        .collect())
    }
    fn count(&self, db: &Connection, table: &str) -> Result<i64, AnalyticsError> {
        Ok(db.query_row(
            &format!("SELECT COUNT(*) FROM {table} {}", self.sql),
            params_from_iter(&self.params),
            |r| r.get(0),
        )?)
    }
    fn page(
        &self,
        db: &Connection,
        table: &'static str,
        order: &str,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<Value>, AnalyticsError> {
        let mut params = self.params.clone();
        params.extend([SqlValue::Integer(limit), SqlValue::Integer(offset)]);
        records(
            db,
            &format!(
                "SELECT record FROM {table} {} ORDER BY {order} LIMIT ? OFFSET ?",
                self.sql
            ),
            params_from_iter(params),
            table,
        )
    }
    fn rates(
        &self,
        db: &Connection,
        limit: i64,
        offset: i64,
    ) -> Result<Vec<RateLimit>, AnalyticsError> {
        let mut params = self.params.clone();
        params.extend([SqlValue::Integer(limit), SqlValue::Integer(offset)]);
        let mut stmt = db.prepare(&format!("SELECT account,at,record FROM analytics_limits {} ORDER BY at DESC,id DESC LIMIT ? OFFSET ?",self.sql))?;
        let rows = stmt.query_map(params_from_iter(params), |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, f64>(1)?,
                r.get::<_, String>(2)?,
            ))
        })?;
        rows.map(|row| {
            let (account, at, raw) = row?;
            let data: Value = serde_json::from_str(&raw)
                .map_err(|_| AnalyticsError::InvalidStoredObject("analytics_limits"))?;
            Ok(RateLimit {
                account_key: account,
                at,
                data,
            })
        })
        .collect()
    }
}
fn scope_filter(
    scope: &str,
    agent: Option<&str>,
    root: &str,
    from: Option<f64>,
    to: Option<f64>,
    tool: Option<&str>,
) -> ScopeFilter {
    let mut filter = ScopeFilter::new().range("at", from, to);
    filter = match scope {
        "agent" => filter.and(
            "agent=?",
            [SqlValue::Text(agent.unwrap_or_default().into())],
        ),
        "team" => filter.and("root=?", [SqlValue::Text(root.into())]),
        _ => filter,
    };
    if let Some(tool) = tool {
        filter = filter.and("is_tool=1", []);
        if !tool.is_empty() {
            filter = filter.and("name=?", [SqlValue::Text(tool.into())]);
        }
    }
    filter
}
fn account_filter(agents: &[Value], options: &AnalyticsOptions) -> ScopeFilter {
    ScopeFilter::new()
        .members(
            "account",
            agents
                .iter()
                .map(|a| string_default(a, "accountKey", "default")),
        )
        .range("at", options.from, options.to)
}
fn selected_agents(
    db: &Connection,
    options: &AnalyticsOptions,
    scope: &str,
    overlay_first: bool,
) -> Result<(String, Vec<Value>), AnalyticsError> {
    let mut agents = records(
        db,
        "SELECT record FROM analytics_agents",
        [],
        "analytics_agents",
    )?;
    if overlay_first {
        overlay_agents(db, &mut agents)?;
    }
    let selected = options
        .agent
        .as_ref()
        .and_then(|id| agents.iter().find(|row| row["id"] == *id));
    if scope != "all" && selected.is_none() {
        return Err(AnalyticsError::InvalidRequest(
            "Select an agent for analytics",
        ));
    }
    let root = selected
        .and_then(|row| row["rootId"].as_str())
        .or(options.agent.as_deref())
        .unwrap_or_default()
        .to_owned();
    agents.retain(|row| match scope {
        "all" => true,
        "agent" => row["id"].as_str() == options.agent.as_deref(),
        _ => row["rootId"].as_str().or(row["id"].as_str()) == Some(&root),
    });
    Ok((root, agents))
}
fn overlay_agents(db: &Connection, agents: &mut [Value]) -> Result<(), AnalyticsError> {
    for record in agents {
        if let Some(runtime) = read_runtime_agent(db, &string(record, "id"))? {
            for key in [
                "name",
                "deletedAt",
                "model",
                "effort",
                "fastMode",
                "threadId",
                "accountKey",
                "rootId",
                "parentId",
                "cwd",
            ] {
                if let Some(value) = runtime.get(key) {
                    record[key] = value.clone();
                }
            }
        }
    }
    Ok(())
}
fn read_runtime_agent(db: &Connection, id: &str) -> Result<Option<Value>, AnalyticsError> {
    let raw = db
        .query_row(
            "SELECT record FROM canvas.runtime_agents WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    raw.map(|s| {
        serde_json::from_str(&s).map_err(|_| AnalyticsError::InvalidStoredObject("runtime_agents"))
    })
    .transpose()
}
fn string(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}
fn string_default(row: &Value, key: &str, default: &str) -> String {
    row.get(key)
        .and_then(Value::as_str)
        .unwrap_or(default)
        .to_owned()
}
fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(v)) => *v,
        Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) != 0.0,
        Some(Value::String(s)) => !s.is_empty(),
        Some(Value::Array(a)) => !a.is_empty(),
        Some(Value::Object(o)) => !o.is_empty(),
    }
}
fn number_at(row: &Value, key: &str) -> Option<f64> {
    row.get(key)
        .and_then(Value::as_f64)
        .filter(|n| n.is_finite() && *n >= 0.0)
}
fn context_percent(row: &Value) -> Option<f64> {
    Some(row.pointer("/last/totalTokens")?.as_f64()? / row.get("modelContextWindow")?.as_f64()?)
        .filter(|n| n.is_finite())
}
#[derive(serde::Serialize)]
#[serde(untagged)]
enum DetailPage {
    Turns {
        turns: Vec<Value>,
        pagination: AnalyticsPagination,
    },
    Limits {
        #[serde(rename = "rateLimits")]
        rate_limits: Vec<RateLimit>,
        pagination: AnalyticsPagination,
    },
}
fn detail(
    db: &Connection,
    options: &AnalyticsOptions,
    scope: &str,
) -> Result<Value, AnalyticsError> {
    let detail = options.detail.as_deref().unwrap_or("");
    if !matches!(detail, "rateLimits" | "turns") {
        return Err(AnalyticsError::InvalidRequest(
            "Unknown analytics detail page",
        ));
    }
    let (root, agents) = selected_agents(db, options, scope, true)?;
    let limit = options.limit.unwrap_or(100).clamp(1, 100);
    let offset = options.offset.unwrap_or(0).max(0);
    let filter = if detail == "turns" {
        scope_filter(
            scope,
            options.agent.as_deref(),
            &root,
            options.from,
            options.to,
            None,
        )
    } else {
        account_filter(&agents, options)
    };
    let table = if detail == "turns" {
        "analytics_turns"
    } else {
        "analytics_limits"
    };
    let total = filter.count(db, table)?;
    let pagination = AnalyticsPagination {
        limit,
        offset,
        total,
        has_more: offset + limit < total,
    };
    let page = if detail == "turns" {
        DetailPage::Turns {
            turns: filter.page(db, table, "at DESC,id DESC", limit, offset)?,
            pagination,
        }
    } else {
        DetailPage::Limits {
            rate_limits: filter.rates(db, limit, offset)?,
            pagination,
        }
    };
    Ok(serde_json::to_value(page)?)
}

fn turn_errors(
    db: &Connection,
    options: &AnalyticsOptions,
    scope: &str,
) -> Result<Value, AnalyticsError> {
    if scope != "agent" || options.thread.as_deref().unwrap_or_default().is_empty() {
        return Err(AnalyticsError::InvalidRequest(
            "Select one thread and up to 120 turns",
        ));
    }
    let agent = options.agent.as_deref().unwrap_or_default();
    let known: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM canvas.runtime_agents WHERE id=?1) OR EXISTS(SELECT 1 FROM analytics_agents WHERE id=?1)",
        [agent], |r| r.get(0)
    )?;
    if !known {
        return Err(AnalyticsError::UnknownAgent);
    }
    let list: Vec<_> = options
        .turns
        .as_deref()
        .unwrap_or_default()
        .split(',')
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if list.is_empty() || list.len() > 120 || list.iter().any(|s| s.is_empty()) {
        return Err(AnalyticsError::InvalidRequest(
            "Select one thread and up to 120 turns",
        ));
    }
    let keys: Vec<_> = list
        .iter()
        .map(|turn| {
            format!(
                "{}:{}:{}",
                options.agent.as_deref().unwrap_or_default(),
                options.thread.as_deref().unwrap_or_default(),
                turn
            )
        })
        .collect();
    let placeholders = vec!["?"; keys.len()].join(",");
    let sql = format!("SELECT record FROM analytics_turns WHERE id IN ({placeholders})");
    let fields = ["agentId", "threadId", "turnId", "status", "error"];
    let rows = records(
        db,
        &sql,
        params_from_iter(keys.into_iter().map(SqlValue::Text)),
        "analytics_turns",
    )?;
    let projected: Vec<_> = rows.iter().map(|row| project(row, &fields)).collect();
    Ok(json!({"turns":projected}))
}

fn message_info(
    db: &Connection,
    root: &Path,
    options: &AnalyticsOptions,
) -> Result<Value, AnalyticsError> {
    let agent = options.agent.as_deref().unwrap_or_default();
    let item_id = options.item.as_deref().unwrap_or_default();
    if item_id.is_empty() || item_id.len() > 512 {
        return Err(AnalyticsError::InvalidRequest("Select a message item"));
    }
    let known:bool=db.query_row("SELECT EXISTS(SELECT 1 FROM analytics_agents WHERE id=?1) OR EXISTS(SELECT 1 FROM canvas.runtime_agents WHERE id=?1)",[agent],|r|r.get(0))?;
    if !known {
        return Err(AnalyticsError::UnknownAgent);
    }
    let mut result = Map::new();
    let mut item_query=db.prepare("SELECT record FROM analytics_items WHERE agent=?1 AND json_extract(record,'$.itemId')=?2 AND (?3 IS NULL OR thread=?3) LIMIT 2")?;
    let found: Vec<String> = item_query
        .query_map(rusqlite::params![agent, item_id, options.thread], |r| {
            r.get(0)
        })?
        .collect::<Result<_, _>>()?;
    let item = if found.len() == 1 {
        Some(parse_object(&found[0], "analytics_items")?)
    } else {
        None
    };
    let mut thread = options.thread.clone();
    let mut turn_id = options.turn.clone();
    let item = if let Some(item) = item.filter(|i| {
        i.get("type").and_then(Value::as_str) == Some("agentMessage")
            && turn_id
                .as_deref()
                .is_none_or(|t| i.get("turnId").and_then(Value::as_str) == Some(t))
    }) {
        thread = item
            .get("threadId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        turn_id = item
            .get("turnId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        if let Some(at) = item
            .get("finishedAt")
            .filter(|v| !v.is_null())
            .or_else(|| item.get("startedAt"))
            .or_else(|| item.get("at"))
        {
            result.insert("at".into(), at.clone());
        }
        Some(item)
    } else {
        None
    };
    let Some(turn_id) = turn_id else {
        return Ok(Value::Object(result));
    };
    let mut turn_query=db.prepare("SELECT record FROM analytics_turns WHERE agent=?1 AND json_extract(record,'$.turnId')=?2 AND (?3 IS NULL OR json_extract(record,'$.threadId')=?3) LIMIT 2")?;
    let found: Vec<String> = turn_query
        .query_map(rusqlite::params![agent, turn_id, thread], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if found.len() != 1 {
        return Ok(Value::Object(result));
    }
    let turn = parse_object(&found[0], "analytics_turns")?;
    thread = turn
        .get("threadId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let mut runs=db.prepare("SELECT record FROM runtime_execution_runs WHERE agent=?1 AND thread IS ?2 AND turn=?3 LIMIT 2")?;
    let run_rows: Vec<String> = runs
        .query_map(rusqlite::params![agent, thread, turn_id], |r| r.get(0))?
        .collect::<Result<_, _>>()?;
    if run_rows.len() == 1 {
        let run = parse_object(&run_rows[0], "runtime_execution_runs")?;
        result.insert(
            "runId".into(),
            run.get("id").cloned().unwrap_or(Value::Null),
        );
        if let Some(id) = run.get("rootAttemptId") {
            result.insert("attemptId".into(), id.clone());
        }
    }
    for field in ["model", "effort", "accountKey"] {
        if let Some(v) = turn.get(field).filter(|v| truthy(Some(v))) {
            result.insert(field.into(), v.clone());
        }
    }
    if let Some(account_key) = result.get("accountKey").and_then(Value::as_str) {
        let path = root.join("registry.json");
        if let Ok(raw) = fs::read(path)
            && let Ok(registry) = serde_json::from_slice::<Value>(&raw)
            && let Some(account) = registry.pointer(&format!("/accounts/{account_key}"))
        {
            if let Some(label) = account.get("label") {
                result.insert("accountLabel".into(), label.clone());
            }
            if account.is_object() {
                result.insert(
                    "provider".into(),
                    account.get("provider").cloned().unwrap_or(json!("codex")),
                );
            }
        }
    }
    if let Some(duration) = turn
        .get("nativeDurationMs")
        .or_else(|| turn.get("durationMs"))
    {
        result.insert("turnDurationMs".into(), duration.clone());
    }
    let response = message_usage(
        db,
        agent,
        thread.as_deref(),
        &turn_id,
        item_id,
        item.as_ref(),
        &turn,
    )?;
    if let Some(response) = response {
        if let Some(model) = response.get("model").filter(|v| truthy(Some(v))) {
            result.insert("model".into(), model.clone());
        }
        if let Some(tokens) = response
            .get("requestUsage")
            .or_else(|| response.get("last"))
        {
            result.insert("tokens".into(), tokens.clone());
        }
    }
    result.retain(|_, value| !value.is_null());
    Ok(Value::Object(result))
}

fn parse_object(raw: &str, table: &'static str) -> Result<Value, AnalyticsError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| AnalyticsError::InvalidStoredObject(table))?;
    if !value.is_object() {
        return Err(AnalyticsError::InvalidStoredObject(table));
    }
    Ok(value)
}
fn message_usage(
    db: &Connection,
    agent: &str,
    thread: Option<&str>,
    turn: &str,
    item_id: &str,
    item: Option<&Value>,
    turn_record: &Value,
) -> Result<Option<Value>, AnalyticsError> {
    let usage = raw_records(
        db,
        "SELECT record FROM analytics_usage WHERE agent=?1 AND thread IS ?2 AND turn=?3 AND json_extract(record,'$.responseId') IS NOT NULL LIMIT 2",
        rusqlite::params![agent, thread, turn],
        "analytics_usage",
    )?;
    let exact = raw_records(
        db,
        "SELECT record FROM analytics_usage WHERE agent=?1 AND thread IS ?2 AND turn=?3 AND json_extract(record,'$.responseId')=?4 LIMIT 2",
        rusqlite::params![agent, thread, turn, item_id],
        "analytics_usage",
    )?;
    if exact.len() == 1 {
        return Ok(exact.into_iter().next());
    }
    if item.is_none()
        || usage.len() != 1
        || !matches!(
            turn_record.get("status").and_then(Value::as_str),
            Some("completed" | "failed" | "interrupted")
        )
    {
        return Ok(None);
    }
    let all: i64 = db.query_row(
        "SELECT COUNT(*) FROM analytics_usage WHERE agent=?1 AND thread IS ?2 AND turn=?3",
        rusqlite::params![agent, thread, turn],
        |r| r.get(0),
    )?;
    if all != 1 {
        return Ok(None);
    }
    let messages = raw_records(
        db,
        "SELECT record FROM analytics_items WHERE agent=?1 AND thread IS ?2 AND turn=?3 AND type='agentMessage' LIMIT 2",
        rusqlite::params![agent, thread, turn],
        "analytics_items",
    )?;
    let tool:Option<i64>=db.query_row("SELECT 1 FROM analytics_items WHERE agent=?1 AND thread IS ?2 AND turn=?3 AND is_tool=1 LIMIT 1",rusqlite::params![agent,thread,turn],|r|r.get(0)).optional()?;
    if tool.is_none()
        && messages.len() == 1
        && messages[0].get("itemId").and_then(Value::as_str) == Some(item_id)
    {
        Ok(usage.into_iter().next())
    } else {
        Ok(None)
    }
}
fn raw_records<P: rusqlite::Params>(
    db: &Connection,
    sql: &str,
    params: P,
    table: &'static str,
) -> Result<Vec<Value>, AnalyticsError> {
    let sql = sql.replacen("SELECT record FROM", "SELECT id,record FROM", 1);
    let mut stmt = db.prepare(&sql)?;
    let values = stmt.query_map(params, |r| {
        Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
    })?;
    values
        .map(|row| {
            let (id, raw) = row?;
            parse_object(&raw, table).map_err(|_| AnalyticsError::InvalidStoredRow { table, id })
        })
        .collect()
}

/// Return a signature-checked estimate persisted by the Python refresh worker.
/// `None` means the caller must answer the declared refresh dependency.
pub fn persisted_session_cost(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<Option<Value>, SessionCostError> {
    if !store.root().join("analytics.sqlite3").is_file() {
        return Err(SessionCostError::RefreshRequired);
    }
    let repo = AnalyticsRepository::new(store.root());
    let mut db = repo.open_read_only()?;
    let tx = db.transaction()?;
    let agent = tx
        .query_row(
            "SELECT record FROM analytics_agents WHERE id=?1",
            [agent_id],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let record = if let Some(raw) = agent {
        serde_json::from_str::<Value>(&raw)
            .map_err(|_| SessionCostError::InvalidStored("analytics_agents"))?
    } else {
        let raw = tx
            .query_row(
                "SELECT record FROM canvas.runtime_agents WHERE id=?1",
                [agent_id],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(SessionCostError::UnknownAgent)?;
        serde_json::from_str::<Value>(&raw)
            .map_err(|_| SessionCostError::InvalidStored("runtime_agents"))?
    };
    let root = record
        .get("rootId")
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
        .unwrap_or(agent_id)
        .to_owned();
    let members = cost_agents(&tx, agent_id, &root)?;
    if members.iter().any(|(_, r)| has_claude_history(r)) {
        tx.commit()?;
        return Ok(None);
    }
    let usage = usage_signature(&tx, &root)?;
    let cost = cost_projection_signature(&tx, &root, store.root())?;
    let agents_sig = hash_json(&agent_signature_json(&members))?;
    let claude_sig = hash_json(&json!({"configs":[],"files":[]}))?;
    let pricing_sig = pricing_signature(store.root());
    tx.commit()?;
    let source = json!({"usage":usage,"pricingSignature":pricing_sig,"agentsSignature":agents_sig,"claudeSignature":claude_sig,"costProjection":cost});
    let digest = sha256_hex(root.as_bytes());
    let cache_path = store
        .root()
        .join("session-costs")
        .join(format!("cost-{digest}.json"));
    let Ok(raw) = fs::read(&cache_path) else {
        return Ok(None);
    };
    let Ok(saved) = serde_json::from_slice::<Value>(&raw) else {
        return Ok(None);
    };
    if saved.get("version").and_then(Value::as_i64) != Some(2)
        || saved.get("rootId").and_then(Value::as_str) != Some(&root)
        || saved.get("source") != Some(&source)
        || saved
            .get("source")
            .and_then(Value::as_object)
            .is_none_or(|m| m.len() != source.as_object().map_or(0, Map::len))
    {
        return Ok(None);
    }
    let Some(cached_at) = saved.get("cachedAt").and_then(Value::as_f64) else {
        return Ok(None);
    };
    let Some(mut result) = saved
        .get("result")
        .filter(|v| valid_cost_result(v, &root))
        .cloned()
    else {
        return Ok(None);
    };
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    result["cacheAgeSeconds"] = json!(((now - cached_at).max(0.0) * 10.0).round() / 10.0);
    result["refreshing"] = json!(false);
    Ok(Some(result))
}

#[derive(Debug, Error)]
pub enum SessionCostError {
    #[error("Not implemented: requires usage-policy/session-cost-refresh")]
    RefreshRequired,
    #[error("Unknown chat")]
    UnknownAgent,
    #[error("Invalid stored object {0}")]
    InvalidStored(&'static str),
    #[error("Invalid stored object {table}/{id}")]
    InvalidStoredRow { table: &'static str, id: String },
    #[error("session-cost read failed: {0}")]
    Read(#[from] AnalyticsStoreError),
    #[error("SQLite read failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("Invalid cache JSON")]
    Json(#[from] serde_json::Error),
}
fn valid_cost_result(v: &Value, root: &str) -> bool {
    v.is_object()
        && v.get("rootId").and_then(Value::as_str) == Some(root)
        && v.get("pricingState").and_then(Value::as_str) == Some("ready")
        && v.get("unknownModels").is_some_and(Value::is_array)
        && v.get("pricedSamples").and_then(Value::as_i64).is_some()
        && v.get("estimated").and_then(Value::as_bool).is_some()
        && v.pointer("/breakdown/providers")
            .is_some_and(Value::is_object)
        && v.pointer("/breakdown/models").is_some_and(Value::is_object)
        && v.get("totalUSD")
            .is_some_and(|x| x.is_null() || x.is_number())
}
fn has_claude_history(record: &Value) -> bool {
    record.get("provider").and_then(Value::as_str) == Some("claude")
        || record
            .get("accountHistory")
            .and_then(Value::as_array)
            .is_some_and(|a| {
                a.iter()
                    .any(|h| h.get("provider").and_then(Value::as_str) == Some("claude"))
            })
}
fn cost_agents(
    db: &Connection,
    agent: &str,
    root: &str,
) -> Result<Vec<(String, Value)>, SessionCostError> {
    let rows:Vec<(String,String)>=db.prepare("SELECT id,record FROM analytics_agents WHERE id=?1 OR json_extract(record,'$.rootId')=?2 ORDER BY id")?.query_map([root,root],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
    let mut out = Vec::new();
    for (id, raw) in rows {
        let v: Value = serde_json::from_str(&raw)
            .map_err(|_| SessionCostError::InvalidStored("analytics_agents"))?;
        if id == root || v.get("rootId").and_then(Value::as_str) == Some(root) {
            out.push((id, v));
        }
    }
    if !out.iter().any(|(id, _)| id == agent) {
        let raw = db
            .query_row(
                "SELECT record FROM canvas.runtime_agents WHERE id=?1",
                [agent],
                |r| r.get::<_, String>(0),
            )
            .optional()?
            .ok_or(SessionCostError::UnknownAgent)?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|_| SessionCostError::InvalidStored("runtime_agents"))?;
        out.push((agent.into(), v));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
fn usage_signature(db: &Connection, root: &str) -> Result<Value, SessionCostError> {
    let max: i64 = if table_exists(db, "analytics_usage")? {
        db.query_row(
            "SELECT COALESCE(MAX(seq),0) FROM analytics_usage WHERE root=?1",
            [root],
            |r| r.get(0),
        )?
    } else {
        0
    };
    let generation: i64 = if table_exists(db, "analytics_usage_roots")? {
        db.query_row(
            "SELECT COALESCE((SELECT generation FROM analytics_usage_roots WHERE root=?1),0)",
            [root],
            |r| r.get(0),
        )?
    } else if table_exists(db, "analytics_meta")? {
        db.query_row("SELECT COALESCE((SELECT CAST(value AS INTEGER) FROM analytics_meta WHERE key='usageGeneration'),0)",[],|r|r.get(0))?
    } else {
        0
    };
    Ok(json!({"maxSeq":max,"generation":generation}))
}
fn cost_projection_signature(
    db: &Connection,
    chat_root: &str,
    state_root: &Path,
) -> Result<Option<Value>, SessionCostError> {
    if !table_exists(db, "analytics_cost_state_v1")?
        || !table_exists(db, "analytics_cost_roots_v1")?
    {
        return Ok(None);
    }
    let Some((token,revision,floor))=db.query_row("SELECT token,COALESCE((SELECT revision FROM analytics_cost_roots_v1 WHERE root=?1),0),COALESCE((SELECT floor FROM analytics_cost_roots_v1 WHERE root=?1),0) FROM analytics_cost_state_v1 WHERE singleton=1",[chat_root],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?))).optional()? else{return Ok(None)};
    let meta = fs::metadata(state_root.join("analytics.sqlite3")).ok();
    let identity = file_identity(meta);
    Ok(Some(json!([token, revision, identity, floor])))
}
#[cfg(unix)]
fn file_identity(meta: Option<std::fs::Metadata>) -> Value {
    use std::os::unix::fs::MetadataExt;
    meta.map(|m| json!([m.dev(), m.ino()]))
        .unwrap_or(Value::Null)
}
#[cfg(not(unix))]
fn file_identity(_: Option<std::fs::Metadata>) -> Value {
    Value::Null
}
fn table_exists(db: &Connection, name: &str) -> Result<bool, rusqlite::Error> {
    let (schema, table) = name.split_once('.').unwrap_or(("main", name));
    db.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {schema}.sqlite_master WHERE type='table' AND name=?1)"
        ),
        [table],
        |r| r.get(0),
    )
}
fn agent_signature_json(agents: &[(String, Value)]) -> Value {
    let fields = [
        "rootId",
        "accountKey",
        "provider",
        "threadId",
        "accountHistory",
    ];
    let mut rows = agents
        .iter()
        .map(|(id, r)| {
            let mut item = Map::new();
            for field in fields {
                item.insert(field.into(), r.get(field).cloned().unwrap_or(Value::Null));
            }
            json!([id, item])
        })
        .collect::<Vec<_>>();
    rows.sort_by(|a, b| a[0].as_str().cmp(&b[0].as_str()));
    Value::Array(rows)
}
fn pricing_signature(root: &Path) -> Value {
    let path = root.join("model-pricing/models-dev-v1.json");
    let Ok(raw) = fs::read(path) else {
        return Value::Null;
    };
    let Ok(artifact) = serde_json::from_slice::<Value>(&raw) else {
        return Value::Null;
    };
    if artifact.get("version").and_then(Value::as_i64) != Some(1) {
        return Value::Null;
    }
    let Some(catalog) = artifact.get("catalog").filter(|value| valid_catalog(value)) else {
        return Value::Null;
    };
    hash_json(catalog).map(Value::String).unwrap_or(Value::Null)
}
fn valid_catalog(value: &Value) -> bool {
    let Some(providers) = value.get("providers").and_then(Value::as_object) else {
        return false;
    };
    ["openai", "anthropic"].into_iter().all(|provider| {
        providers
            .get(provider)
            .and_then(|value| value.get("models"))
            .and_then(Value::as_object)
            .is_some_and(|models| {
                models.values().any(|model| {
                    let Some(cost) = model.get("cost") else {
                        return false;
                    };
                    ["input", "output"].into_iter().all(|field| {
                        cost.get(field)
                            .and_then(Value::as_f64)
                            .is_some_and(|amount| amount.is_finite() && amount >= 0.0)
                    })
                })
            })
    })
}
fn hash_json(value: &Value) -> Result<String, serde_json::Error> {
    Ok(sha256_hex(python_json_compact(value).as_bytes()))
}
