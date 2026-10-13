//! Read-only analytics projections over the Python runtime's persisted rows.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{
    Connection, OpenFlags, OptionalExtension, params_from_iter, types::Value as SqlValue,
};
use serde_json::{Value, json};
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
    #[error("Not implemented: requires usage-policy/session-cost-refresh")]
    RefreshRequired,
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

pub fn rollup<'a>(
    db: &Connection,
    options: &'a AnalyticsOptions,
    scope: &'a str,
) -> Result<AnalyticsResponse<'a>, AnalyticsError> {
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
    let chart = chart_rows(db, &billable)?;
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
    Ok(response)
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
pub struct AnalyticsResponse<'a> {
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
    pub chart_buckets: Vec<Value>,
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
fn chart_rows(db: &Connection, filter: &ScopeFilter) -> Result<Vec<Value>, AnalyticsError> {
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
pub fn detail(
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

fn parse_object(raw: &str, table: &'static str) -> Result<Value, AnalyticsError> {
    let value: Value =
        serde_json::from_str(raw).map_err(|_| AnalyticsError::InvalidStoredObject(table))?;
    if !value.is_object() {
        return Err(AnalyticsError::InvalidStoredObject(table));
    }
    Ok(value)
}
pub fn message_item(
    db: &Connection,
    agent: &str,
    item: &str,
    thread: Option<&str>,
) -> Result<Option<Value>, AnalyticsError> {
    unique_record(
        db,
        "SELECT record FROM analytics_items WHERE agent=?1 AND json_extract(record,'$.itemId')=?2 AND (?3 IS NULL OR thread=?3) LIMIT 2",
        rusqlite::params![agent, item, thread],
        "analytics_items",
    )
}
pub fn message_turn(
    db: &Connection,
    agent: &str,
    turn: &str,
    thread: Option<&str>,
) -> Result<Option<Value>, AnalyticsError> {
    unique_record(
        db,
        "SELECT record FROM analytics_turns WHERE agent=?1 AND json_extract(record,'$.turnId')=?2 AND (?3 IS NULL OR json_extract(record,'$.threadId')=?3) LIMIT 2",
        rusqlite::params![agent, turn, thread],
        "analytics_turns",
    )
}
pub fn message_run(
    db: &Connection,
    agent: &str,
    turn: &str,
    thread: Option<&str>,
) -> Result<Option<Value>, AnalyticsError> {
    unique_record(
        db,
        "SELECT record FROM runtime_execution_runs WHERE agent=?1 AND thread IS ?2 AND turn=?3 LIMIT 2",
        rusqlite::params![agent, thread, turn],
        "runtime_execution_runs",
    )
}
fn unique_record<P: rusqlite::Params>(
    db: &Connection,
    sql: &str,
    params: P,
    table: &'static str,
) -> Result<Option<Value>, AnalyticsError> {
    let rows: Vec<String> = query(db, sql, params, |r| r.get(0))?;
    if rows.len() == 1 {
        Ok(Some(parse_object(&rows[0], table)?))
    } else {
        Ok(None)
    }
}
pub fn message_usage(
    db: &Connection,
    agent: &str,
    thread: Option<&str>,
    turn: &str,
    response: Option<&str>,
) -> Result<Vec<Value>, AnalyticsError> {
    raw_records(
        db,
        "SELECT record FROM analytics_usage WHERE agent=?1 AND thread IS ?2 AND turn=?3 AND json_extract(record,'$.responseId') IS NOT NULL AND (?4 IS NULL OR json_extract(record,'$.responseId')=?4) LIMIT 2",
        rusqlite::params![agent, thread, turn, response],
        "analytics_usage",
    )
}
pub fn unambiguous_message(
    db: &Connection,
    agent: &str,
    thread: Option<&str>,
    turn: &str,
    item: &str,
) -> Result<bool, AnalyticsError> {
    Ok(db.query_row("SELECT
        (SELECT COUNT(*) FROM analytics_usage WHERE agent=?1 AND thread IS ?2 AND turn=?3)=1 AND
        NOT EXISTS(SELECT 1 FROM analytics_items WHERE agent=?1 AND thread IS ?2 AND turn=?3 AND is_tool=1) AND
        (SELECT COUNT(*)=1 AND MIN(json_extract(record,'$.itemId'))=?4 FROM analytics_items WHERE agent=?1 AND thread IS ?2 AND turn=?3 AND type='agentMessage')",
        rusqlite::params![agent,thread,turn,item],|r|r.get(0))?)
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

pub type SessionCostError = AnalyticsError;
pub fn cost_agents(
    db: &Connection,
    agent: &str,
    root: &str,
) -> Result<Vec<(String, Value)>, SessionCostError> {
    let rows:Vec<(String,String)>=db.prepare("SELECT id,record FROM analytics_agents WHERE id=?1 OR json_extract(record,'$.rootId')=?2 ORDER BY id")?.query_map([root,root],|r|Ok((r.get(0)?,r.get(1)?)))?.collect::<Result<_,_>>()?;
    let mut out = Vec::new();
    for (id, raw) in rows {
        let v: Value = serde_json::from_str(&raw)
            .map_err(|_| SessionCostError::InvalidStoredObject("analytics_agents"))?;
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
            .ok_or(SessionCostError::InvalidRequest("Unknown chat"))?;
        let v: Value = serde_json::from_str(&raw)
            .map_err(|_| SessionCostError::InvalidStoredObject("runtime_agents"))?;
        out.push((agent.into(), v));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}
pub fn usage_signature(db: &Connection, root: &str) -> Result<UsageState, SessionCostError> {
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
    Ok(UsageState {
        max_seq: max,
        generation,
    })
}
pub fn cost_projection(
    db: &Connection,
    root: &str,
) -> Result<Option<(String, i64, i64)>, SessionCostError> {
    if !table_exists(db, "analytics_cost_state_v1")?
        || !table_exists(db, "analytics_cost_roots_v1")?
    {
        return Ok(None);
    }
    Ok(db.query_row("SELECT token,COALESCE((SELECT revision FROM analytics_cost_roots_v1 WHERE root=?1),0),COALESCE((SELECT floor FROM analytics_cost_roots_v1 WHERE root=?1),0) FROM analytics_cost_state_v1 WHERE singleton=1",[root],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional()?)
}
#[derive(serde::Serialize, serde::Deserialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct UsageState {
    max_seq: i64,
    generation: i64,
}
pub fn cost_root(db: &Connection, agent: &str) -> Result<String, SessionCostError> {
    let (table,raw): (String,String) = db.query_row("SELECT 'analytics_agents',record FROM analytics_agents WHERE id=?1 UNION ALL SELECT 'runtime_agents',record FROM canvas.runtime_agents WHERE id=?1 LIMIT 1",[agent],|r|Ok((r.get(0)?,r.get(1)?))).optional()?.ok_or(SessionCostError::InvalidRequest("Unknown chat"))?;
    let table = if table == "analytics_agents" {
        "analytics_agents"
    } else {
        "runtime_agents"
    };
    let record: Value =
        serde_json::from_str(&raw).map_err(|_| SessionCostError::InvalidStoredObject(table))?;
    Ok(record["rootId"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or(agent)
        .into())
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
#[derive(serde::Serialize)]
struct TurnErrors {
    turns: Vec<Value>,
}
pub fn require_agent(db: &Connection, agent: &str) -> Result<(), AnalyticsError> {
    let known: bool = db.query_row("SELECT EXISTS(SELECT 1 FROM analytics_agents WHERE id=?1) OR EXISTS(SELECT 1 FROM canvas.runtime_agents WHERE id=?1)",[agent],|r|r.get(0))?;
    if known {
        Ok(())
    } else {
        Err(AnalyticsError::UnknownAgent)
    }
}
pub fn turn_errors(db: &Connection, keys: Vec<String>) -> Result<Value, AnalyticsError> {
    let rows =
        ScopeFilter::new()
            .members("id", keys)
            .page(db, "analytics_turns", "rowid", -1, 0)?;
    Ok(serde_json::to_value(TurnErrors {
        turns: rows
            .iter()
            .map(|row| project(row, &["agentId", "threadId", "turnId", "status", "error"]))
            .collect(),
    })?)
}
