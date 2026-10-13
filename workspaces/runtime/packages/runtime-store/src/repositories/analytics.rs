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
            use std::fmt::Write as _;
            write!(uri, "%{byte:02X}").expect("write to string");
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
    let usage_where = usage_filter.sql.as_str();
    let usage_sql = format!("SELECT record FROM analytics_usage {usage_where} ORDER BY at,seq");
    let usage_rows = records(
        db,
        &usage_sql,
        params_from_iter(usage_filter.params.clone()),
        "analytics_usage",
    )?;
    let turn_filter = scope_filter(scope, agent_id, &root, None, None, None);
    let authoritative: HashSet<(String, String, String)> = {
        let mut statement = db.prepare(&format!(
            "SELECT DISTINCT agent,COALESCE(thread,''),COALESCE(turn,'') FROM analytics_usage {} AND json_extract(record,'$.responseId') IS NOT NULL",
            turn_filter.sql
        ))?;
        statement
            .query_map(params_from_iter(turn_filter.params), |r| {
                Ok((r.get(0)?, r.get(1)?, r.get(2)?))
            })?
            .collect::<Result<_, _>>()?
    };
    let mut provisional = Vec::new();
    let mut timeline = Vec::new();
    let mut chart_points: HashMap<i64, Value> = HashMap::new();
    let mut chart_width = 3600.0;
    let mut turns_seen = HashSet::new();
    for row in usage_rows {
        let key = (
            string(&row, "agentId"),
            string(&row, "threadId"),
            string(&row, "turnId"),
        );
        if !truthy(row.get("responseId")) && authoritative.contains(&key) {
            provisional.push(row);
            continue;
        }
        let agent = key.0.clone();
        if let Some(turn) = row.get("turnId").and_then(Value::as_str) {
            turns_seen.insert((agent.clone(), turn.to_owned()));
        }
        timeline.push(row.clone());
        if !options.export && timeline.len() > 500 {
            timeline.remove(0);
        }
        if let (Some(at), Some(percent)) = (number_at(&row, "at"), context_percent(&row)) {
            let bucket = (at / chart_width) as i64;
            let replace = chart_points
                .get(&bucket)
                .is_none_or(|old| context_percent(old).unwrap_or(0.0) < percent);
            if replace {
                chart_points.insert(bucket, row);
            }
            if !options.export && chart_points.len() > 500 {
                chart_width *= 2.0;
                let mut compacted = HashMap::new();
                for point in chart_points.into_values() {
                    let key = (number_at(&point, "at").unwrap_or_default() / chart_width) as i64;
                    let replace = compacted.get(&key).is_none_or(|old: &Value| {
                        context_percent(old).unwrap_or(0.0) < context_percent(&point).unwrap_or(0.0)
                    });
                    if replace {
                        compacted.insert(key, point);
                    }
                }
                chart_points = compacted;
            }
        }
    }
    let usage_totals = usage_aggregate(db, &usage_filter, None)?;
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
    let mut chart_rows: Vec<_> = chart_points.into_values().collect();
    chart_rows.sort_by(|a, b| {
        number_at(a, "at")
            .partial_cmp(&number_at(b, "at"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let chart = chart_rows;
    let provisional_total = provisional.len();
    if !options.export && provisional.len() > 100 {
        provisional.drain(..provisional.len() - 100);
    }

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
    let items = usage_filter.page(db, "analytics_items", "at DESC,id", -1, 0)?;
    let turns = usage_filter.page(
        db,
        "analytics_turns",
        "at DESC,id DESC",
        if options.export { -1 } else { 100 },
        0,
    )?;
    for row in &items {
        if let (Some(agent), Some(turn)) = (
            row.get("agentId").and_then(Value::as_str),
            row.get("turnId").and_then(Value::as_str),
        ) {
            turns_seen.insert((agent.to_owned(), turn.to_owned()));
        }
    }
    let selected_ids: BTreeSet<String> = agents
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let total_turns = usage_filter.count(db, "analytics_turns")?;
    let tracking_raw = db
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='trackingSince'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let tracking = match tracking_raw {
        Some(raw) => raw
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .ok_or_else(|| AnalyticsError::InvalidStoredRow {
                table: "analytics_meta",
                id: "trackingSince".into(),
            })?,
        None => 0.0,
    };
    let capture_raw = db
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='captureErrors'",
            [],
            |r| r.get::<_, String>(0),
        )
        .optional()?;
    let capture = match capture_raw {
        Some(raw) => {
            let value: Value =
                serde_json::from_str(&raw).map_err(|_| AnalyticsError::InvalidStoredRow {
                    table: "analytics_meta",
                    id: "captureErrors".into(),
                })?;
            if !value.is_object() || value.get("count").and_then(Value::as_u64).is_none() {
                return Err(AnalyticsError::InvalidStoredRow {
                    table: "analytics_meta",
                    id: "captureErrors".into(),
                });
            }
            value
        }
        None => json!({"count":0,"last":null}),
    };
    let histories = optional_records(
        db,
        "analytics_history",
        "SELECT record FROM analytics_history ORDER BY rowid",
        "analytics_history",
    )?;
    let history: Vec<Value> = histories
        .iter()
        .filter(|r| {
            !truthy(r.get("agent")) && !truthy(r.get("agentId"))
                || selected_ids.contains(&string(r, "agent"))
                || selected_ids.contains(&string(r, "agentId"))
        })
        .cloned()
        .collect();
    let monitors = optional_canvas_records(db, "runtime_monitors")?;
    let requests_table = table_exists(db, "canvas.runtime_requests")?;
    let mut visible_monitors = Vec::new();
    for m in monitors.iter().filter(|r| {
        selected_ids.contains(&string(r, "agent"))
            && within_range(number_at(r, "created"), options.from, options.to)
    }) {
        let mut item = Map::new();
        for k in [
            "id",
            "agent",
            "status",
            "created",
            "finished",
            "bytes",
            "exitCode",
            "error",
            "timeout_ms",
        ] {
            if let Some(v) = m.get(k) {
                item.insert(k.to_owned(), v.clone());
            }
        }
        visible_monitors.push(Value::Object(item));
    }
    let selected_placeholders = std::iter::repeat_n("?", selected_ids.len())
        .collect::<Vec<_>>()
        .join(",");
    let mut event_params: Vec<SqlValue> =
        selected_ids.iter().cloned().map(SqlValue::Text).collect();
    let mut event_where = format!("agent IN ({selected_placeholders})");
    if let Some(from) = options.from {
        event_where.push_str(" AND created>=?");
        event_params.push(SqlValue::Real(from));
    }
    if let Some(to) = options.to {
        event_where.push_str(" AND created<=?");
        event_params.push(SqlValue::Real(to));
    }
    let event_counts: BTreeMap<String, usize> = if selected_ids.is_empty() {
        BTreeMap::new()
    } else {
        let mut stmt = db.prepare(&format!("SELECT status||':'||kind,COUNT(*) FROM canvas.runtime_events WHERE {event_where} GROUP BY status,kind"))?;
        stmt.query_map(params_from_iter(event_params), |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as usize))
        })?
        .collect::<Result<_, _>>()?
    };
    let approval_counts: Map<String, Value> = if !requests_table || selected_ids.is_empty() {
        Map::new()
    } else {
        let mut stmt = db.prepare(&format!("SELECT json_extract(record,'$.status'),COUNT(*) FROM canvas.runtime_requests WHERE json_extract(record,'$.agent') IN ({selected_placeholders}) GROUP BY json_extract(record,'$.status')"))?;
        stmt.query_map(
            params_from_iter(selected_ids.iter().cloned().map(SqlValue::Text)),
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)),
        )?
        .map(|row| row.map(|(status, count)| (status, json!(count))))
        .collect::<Result<_, _>>()?
    };
    let mut notification_rows = Vec::new();
    let mut notification_stmt =
        db.prepare("SELECT id,agent,root,method,hour,count,bytes FROM analytics_notifications")?;
    let notifications = notification_stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, Option<String>>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, Option<i64>>(4)?,
            r.get::<_, i64>(5)?,
            r.get::<_, i64>(6)?,
        ))
    })?;
    for row in notifications {
        let (id, agent, root, method, hour, count, bytes) = row?;
        if agent.as_ref().is_some_and(|id| selected_ids.contains(id))
            && hour.is_some_and(|hour| {
                options
                    .from
                    .is_none_or(|start| (hour + 3600) as f64 > start)
                    && options.to.is_none_or(|end| hour as f64 <= end)
            })
        {
            notification_rows.push(json!({"id":id,"agent":agent,"root":root,"method":method,"hour":hour,"count":count,"bytes":bytes}));
        }
    }
    let mut by_tool: BTreeMap<(String, String), ItemTotals> = BTreeMap::new();
    let mut tool_order = Vec::new();
    let mut agent_calls: BTreeMap<(String, String), ItemTotals> = BTreeMap::new();
    let mut non_tool = Vec::new();
    let mut non_tool_total = 0usize;
    let mut compactions = Vec::new();
    let mut snapshots = Vec::new();
    let mut all_tool = ItemTotals::default();
    let mut model_tool = ItemTotals::default();
    let mut protocol_tool = ItemTotals::default();
    for row in &items {
        let kind = string_default(row, "type", "");
        let boundary = string_default(row, "payloadBoundary", "protocol");
        if kind == "contextCompaction" && row.get("finishedAt").is_some_and(|v| !v.is_null()) {
            compactions.push(row.clone());
        }
        if kind == "compactionSnapshot" {
            snapshots.push(row.clone());
        }
        if row.get("isTool").and_then(Value::as_bool) != Some(true) {
            non_tool_total += 1;
            if options.export || non_tool.len() < 100 {
                non_tool.push(row.clone());
            }
            continue;
        }
        let name = string_default(row, "name", "");
        let key = (name.clone(), kind.clone());
        if !by_tool.contains_key(&key) {
            tool_order.push(key.clone());
        }
        by_tool.entry(key).or_default().add(row);
        let is_model = boundary == "model";
        if options
            .tool
            .as_deref()
            .filter(|tool| !tool.is_empty())
            .is_none_or(|tool| tool == name)
        {
            all_tool.add(row);
            if is_model {
                model_tool.add(row)
            } else {
                protocol_tool.add(row)
            }
            agent_calls
                .entry((
                    string(row, "agentId"),
                    if is_model { "model" } else { "protocol" }.into(),
                ))
                .or_default()
                .add(row);
        }
    }
    let native_compactions: HashSet<(String, String)> = compactions
        .iter()
        .map(|r| (string(r, "agentId"), string(r, "turnId")))
        .collect();
    compactions.extend(
        snapshots
            .iter()
            .filter(|r| !native_compactions.contains(&(string(r, "agentId"), string(r, "turnId"))))
            .cloned(),
    );
    let mut tools: Vec<Value> = tool_order
        .iter()
        .filter_map(|key| {
            by_tool.get(key).map(|v| {
                v.tool(
                    key.0.clone(),
                    key.1.clone(),
                    string_default(
                        v.first.as_ref().unwrap_or(&Value::Null),
                        "payloadBoundary",
                        "protocol",
                    ),
                )
            })
        })
        .collect();
    tools.sort_by(|a, b| {
        b["outputBytes"]
            .as_f64()
            .unwrap_or(0.0)
            .partial_cmp(&a["outputBytes"].as_f64().unwrap_or(0.0))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let item_counts: Vec<Value> = {
        let mut stmt = db.prepare(&format!("SELECT COALESCE(json_extract(record,'$.type'),''),COUNT(*),CASE WHEN COUNT(json_extract(record,'$.input.bytes'))+COUNT(json_extract(record,'$.output.bytes'))=0 THEN NULL ELSE COALESCE(SUM(json_extract(record,'$.input.bytes')),0)+COALESCE(SUM(json_extract(record,'$.output.bytes')),0) END,CASE WHEN COUNT(json_extract(record,'$.input.chars'))+COUNT(json_extract(record,'$.output.chars'))=0 THEN NULL ELSE COALESCE(SUM(json_extract(record,'$.input.chars')),0)+COALESCE(SUM(json_extract(record,'$.output.chars')),0) END,MIN(at),MIN(id) FROM analytics_items {usage_where} GROUP BY json_extract(record,'$.type') ORDER BY MIN(at),MIN(id)"))?;
        stmt.query_map(params_from_iter(usage_filter.params.clone()), |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?, r.get::<_, Option<f64>>(2)?, r.get::<_, Option<f64>>(3)?)))?
            .map(|row| row.map(|(kind,count,bytes,chars)| json!({"type":kind,"count":count,"bytes":number_value(bytes),"chars":number_value(chars)})))
            .collect::<Result<Vec<_>, _>>()?
    };
    let breakdown = item_breakdown(db, &usage_filter)?;
    overlay_agents(db, &mut agents)?;
    let rate_filter = account_filter(&agents, options);
    let total_limits = rate_filter.count(db, "analytics_limits")?;
    let rates = rate_filter.rates(db, if options.export { -1 } else { 100 }, 0)?;
    let observed_agents: HashSet<String> = per_agent
        .keys()
        .cloned()
        .chain(items.iter().map(|row| string(row, "agentId")))
        .collect();
    let mut summary = json!({
        "agents": observed_agents.len(), "turns": turns_seen.len(), "usageSamples": usage_totals.count,
        "provisionalUsageSamples": provisional_total, "exactResponseSamples": usage_totals.exact,
        "legacyUsageSamples": usage_totals.legacy, "tokens": usage_totals.tokens(),
        "tokenObservations": usage_totals.observations(), "peakContextTokens":usage_totals.peak_context,
        "peakContextPercent":usage_totals.peak_percent,"baselineMissingSamples":usage_totals.baseline_missing,
        "modelToolCalls":model_tool.count,"protocolToolCalls":protocol_tool.count,"observedToolRows":all_tool.count,"toolCalls":model_tool.count,"failedToolCalls":model_tool.failed,
        "modelFailedToolCalls":model_tool.failed,"protocolFailedToolCalls":protocol_tool.failed,"compactions":compactions.len(),
        "cacheHitRateSamples":usage_totals.cache_pairs,"cacheHitRateTotalSamples":usage_totals.count
    });
    let mut output_agents = Vec::new();
    for record in &agents {
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
        let mut row = record.as_object().cloned().unwrap_or_default();
        row.insert("tokens".into(), totals.tokens());
        row.insert("usageSamples".into(), json!(totals.count));
        row.insert("toolCalls".into(), json!(model.count));
        row.insert("modelToolCalls".into(), json!(model.count));
        row.insert("protocolToolCalls".into(), json!(protocol.count));
        row.insert("failedToolCalls".into(), json!(model.failed));
        row.insert("protocolFailedToolCalls".into(), json!(protocol.failed));
        row.insert(
            "compactions".into(),
            json!(
                compactions
                    .iter()
                    .filter(|r| string(r, "agentId") == agent)
                    .count()
            ),
        );
        row.insert("duration".into(), model.duration());
        row.insert("protocolDuration".into(), protocol.duration());
        output_agents.push(Value::Object(row));
    }
    let model_rows:Vec<Value>=per_model.iter().map(|(key,t)|json!({"model":if key.is_empty(){Value::Null}else{json!(key)},"samples":t.count,"tokens":t.tokens()})).collect();
    let account_rows: Vec<Value> = per_account
        .iter()
        .map(|(key, t)| json!({"accountKey":key,"samples":t.count,"tokens":t.tokens()}))
        .collect();
    let hit_rate =
        if usage_totals.cache_pairs == usage_totals.count && usage_totals.cache_input > 0.0 {
            usage_totals
                .tokens()
                .get("cachedInputTokens")
                .and_then(Value::as_f64)
                .map(|n| n / usage_totals.cache_input)
        } else {
            None
        };
    summary["cacheHitRate"] = json!(hit_rate);
    summary["inputBytes"] = number_value(model_tool.size("input", "bytes"));
    summary["outputBytes"] = number_value(model_tool.size("output", "bytes"));
    summary["modelInputBytes"] = summary["inputBytes"].clone();
    summary["modelOutputBytes"] = summary["outputBytes"].clone();
    summary["protocolInputBytes"] = number_value(protocol_tool.size("input", "bytes"));
    summary["protocolOutputBytes"] = number_value(protocol_tool.size("output", "bytes"));
    summary["durationMs"] = json!(model_tool.duration_total());
    summary["modelDurationMs"] = summary["durationMs"].clone();
    summary["protocolDurationMs"] = json!(protocol_tool.duration_total());
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
        timeline_total: usage_totals.count,
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
struct AnalyticsResponse<'a> {
    version: u8,
    generated_at: f64,
    filters: AnalyticsFiltersResponse<'a>,
    coverage: AnalyticsCoverageResponse,
    summary: Value,
    agents: Vec<Value>,
    agent_totals: Vec<Value>,
    tools: Vec<Value>,
    model_totals: Vec<Value>,
    account_totals: Vec<Value>,
    operations: AnalyticsOperations,
    notifications: Vec<Value>,
    history: Vec<Value>,
    rate_limits: Vec<Value>,
    turns: Vec<Value>,
    timeline: Vec<Value>,
    chart_buckets: Vec<Value>,
    timeline_total: usize,
    provisional_usage: Vec<Value>,
    calls: Vec<Value>,
    item_records: Vec<Value>,
    item_records_total: usize,
    item_breakdown: Vec<Value>,
    compactions: Vec<Value>,
    compaction_snapshots: Vec<Value>,
    pagination: AnalyticsPagination,
    items: Vec<Value>,
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
    capture_errors: Value,
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
    event_counts: BTreeMap<String, usize>,
    approval_counts: Map<String, Value>,
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

#[derive(Clone, Default)]
struct UsageTotals {
    count: usize,
    exact: usize,
    legacy: usize,
    baseline_missing: usize,
    cache_pairs: usize,
    cache_input: f64,
    peak_context: Option<f64>,
    peak_percent: Option<f64>,
    sums: BTreeMap<String, f64>,
    observations: BTreeMap<String, usize>,
}
impl UsageTotals {
    fn tokens(&self) -> Value {
        let mut m = Map::new();
        for field in TOKEN_FIELDS {
            m.insert(
                field.into(),
                self.sums
                    .get(field)
                    .copied()
                    .map_or(Value::Null, |n| json!(n)),
            );
        }
        Value::Object(m)
    }
    fn observations(&self) -> Value {
        let mut m = Map::new();
        for field in TOKEN_FIELDS {
            m.insert(
                field.into(),
                json!(self.observations.get(field).copied().unwrap_or(0)),
            );
        }
        Value::Object(m)
    }
}

fn usage_aggregate(
    db: &Connection,
    filter: &ScopeFilter,
    group: Option<&str>,
) -> Result<UsageTotals, AnalyticsError> {
    usage_grouped(db, filter, group.unwrap_or("''"))
        .map(|mut groups| groups.remove("").unwrap_or_default())
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
        "SELECT {group},COUNT(*),SUM(CASE WHEN {exact} THEN 1 ELSE 0 END),SUM(CASE WHEN json_extract(record,'$.baselineMissing')=1 THEN 1 ELSE 0 END),MAX(json_extract(record,'$.last.totalTokens')),MAX(CASE WHEN json_extract(record,'$.modelContextWindow')>0 THEN json_extract(record,'$.last.totalTokens')*1.0/json_extract(record,'$.modelContextWindow')*100 END),SUM(CASE WHEN {cache_ok} THEN 1 ELSE 0 END),SUM(CASE WHEN {cache_ok} THEN {cache_input} ELSE 0 END),{sums} FROM analytics_usage {0} AND ({exact} OR NOT EXISTS(SELECT 1 FROM analytics_usage auth WHERE auth.agent=analytics_usage.agent AND auth.thread IS analytics_usage.thread AND auth.turn IS analytics_usage.turn AND NULLIF(json_extract(auth.record,'$.responseId'),'') IS NOT NULL)) GROUP BY {group} ORDER BY {group}",
        filter.sql
    );
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(filter.params.clone()), |row| {
        let mut totals = UsageTotals::default();
        totals.count = row.get::<_, i64>(1)?.max(0) as usize;
        totals.exact = row.get::<_, i64>(2)?.max(0) as usize;
        totals.legacy = totals.count.saturating_sub(totals.exact);
        totals.baseline_missing = row.get::<_, i64>(3)?.max(0) as usize;
        totals.peak_context = row.get(4)?;
        totals.peak_percent = row.get(5)?;
        totals.cache_pairs = row.get::<_, i64>(6)?.max(0) as usize;
        totals.cache_input = row.get::<_, Option<f64>>(7)?.unwrap_or_default();
        for (index, field) in TOKEN_FIELDS.iter().enumerate() {
            let sum: Option<f64> = row.get(8 + index * 2)?;
            let count: i64 = row.get(9 + index * 2)?;
            if let Some(sum) = sum {
                totals.sums.insert((*field).into(), sum);
            }
            totals
                .observations
                .insert((*field).into(), count.max(0) as usize);
        }
        Ok((row.get::<_, String>(0)?, totals))
    })?;
    rows.collect::<Result<BTreeMap<_, _>, _>>()
        .map_err(AnalyticsError::from)
}

fn item_breakdown(db: &Connection, filter: &ScopeFilter) -> Result<Vec<Value>, AnalyticsError> {
    let sql = format!(
        "SELECT COALESCE(json_extract(record,'$.type'),''),COALESCE(json_extract(record,'$.payloadBoundary'),'protocol'),json_extract(record,'$.category'),json_extract(record,'$.role'),COUNT(*),SUM(json_extract(record,'$.input.bytes')),SUM(json_extract(record,'$.output.bytes')),COUNT(json_extract(record,'$.input')),COUNT(json_extract(record,'$.output')) FROM analytics_items {} GROUP BY 1,2,3,4 ORDER BY 1,2,3,4",
        filter.sql
    );
    let mut statement = db.prepare(&sql)?;
    let rows = statement.query_map(params_from_iter(filter.params.clone()), |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, Option<String>>(2)?,
            r.get::<_, Option<String>>(3)?,
            r.get::<_, i64>(4)?,
            r.get::<_, Option<f64>>(5)?,
            r.get::<_, Option<f64>>(6)?,
            r.get::<_, i64>(7)?,
            r.get::<_, i64>(8)?,
        ))
    })?;
    rows.map(|row| row.map(|(kind,boundary,category,role,count,input,output,input_n,output_n)| json!({
        "type":kind,"payloadBoundary":boundary,"category":category,"role":role,"count":count,
        "inputBytes":number_value(input),"outputBytes":number_value(output),"inputMeasurements":input_n,"outputMeasurements":output_n,
    }))).collect::<Result<Vec<_>,_>>().map_err(AnalyticsError::from)
}

#[derive(Clone, Default)]
struct ItemTotals {
    count: usize,
    failed: usize,
    input_measurements: usize,
    output_measurements: usize,
    values: BTreeMap<(String, String), f64>,
    durations: Vec<f64>,
    duration_total: Vec<f64>,
    first: Option<Value>,
}
impl ItemTotals {
    fn add(&mut self, row: &Value) {
        if self.first.is_none() {
            self.first = Some(row.clone())
        }
        self.count += 1;
        self.failed += usize::from(row.get("status").and_then(Value::as_str) == Some("failed"));
        for dir in ["input", "output"] {
            let value = row.get(dir).filter(|v| !v.is_null());
            if value.is_some() {
                if dir == "input" {
                    self.input_measurements += 1
                } else {
                    self.output_measurements += 1
                }
            }
            for field in ["bytes", "chars", "imageCount"] {
                if let Some(n) = value.and_then(|v| v.get(field)).and_then(Value::as_f64) {
                    *self.values.entry((dir.into(), field.into())).or_default() += n;
                }
            }
        }
        if let Some(n) = row.get("durationMs").and_then(Value::as_f64) {
            self.duration_total.push(n);
            if n.is_finite() && n >= 0.0 {
                self.durations.push(n)
            }
        }
    }
    fn size(&self, dir: &str, field: &str) -> Option<f64> {
        self.values.get(&(dir.into(), field.into())).copied()
    }
    fn duration_total(&self) -> Option<f64> {
        if self.duration_total.is_empty() {
            None
        } else {
            Some(self.duration_total.iter().sum())
        }
    }
    fn duration(&self) -> Value {
        let mut v = self.durations.clone();
        v.sort_by(f64::total_cmp);
        let n = v.len();
        let mean = if n == 0 {
            None
        } else {
            Some(v.iter().sum::<f64>() / n as f64)
        };
        let median = if n == 0 {
            None
        } else if n % 2 == 1 {
            Some(v[n / 2])
        } else {
            Some((v[n / 2 - 1] + v[n / 2]) / 2.0)
        };
        let p95 = if n == 0 {
            None
        } else {
            Some(v[((n as f64 * 0.95).ceil() as usize).saturating_sub(1)])
        };
        json!({"count":n,"min":v.first(),"max":v.last(),"mean":mean,"p50":median,"p95":p95})
    }
    fn tool(&self, name: String, kind: String, boundary: String) -> Value {
        json!({"name":name,"type":kind,"payloadBoundary":boundary,"calls":self.count,"failed":self.failed,"inputBytes":number_value(self.size("input","bytes")),"outputBytes":number_value(self.size("output","bytes")),"modelInputBytes":if kind=="modelToolCall"{number_value(self.size("input","bytes"))}else{Value::Null},"modelOutputBytes":if kind=="modelToolCall"{number_value(self.size("output","bytes"))}else{Value::Null},"durationMs":self.duration_total(),"imageCount":number_value(nullable_sum([self.size("input","imageCount"),self.size("output","imageCount")])),"inputMeasurements":self.input_measurements,"outputMeasurements":self.output_measurements,"duration":self.duration()})
    }
}
fn nullable_sum(values: [Option<f64>; 2]) -> Option<f64> {
    let known: Vec<_> = values.into_iter().flatten().collect();
    if known.is_empty() {
        None
    } else {
        Some(known.iter().sum())
    }
}
fn number_value(value: Option<f64>) -> Value {
    match value {
        Some(number) if number.fract() == 0.0 && number >= 0.0 && number <= i64::MAX as f64 => {
            json!(number as i64)
        }
        Some(number) => json!(number),
        None => Value::Null,
    }
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

fn valid_stored_shape(table: &str, value: &Value) -> bool {
    let required: &[&str] = match table {
        "analytics_agents" => &["id"],
        "analytics_usage" => &["agentId", "at"],
        "analytics_items" => &["agentId", "type"],
        "analytics_turns" => &["agentId", "turnId", "status"],
        "analytics_history" => &["status"],
        "runtime_monitors" => &["id", "agent"],
        "runtime_requests" => &["id", "agent", "status"],
        _ => &[],
    };
    required
        .iter()
        .all(|field| value.get(field).is_some_and(|item| !item.is_null()))
}
fn optional_records(
    db: &Connection,
    table: &'static str,
    sql: &str,
    object: &'static str,
) -> Result<Vec<Value>, AnalyticsError> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?;
    if exists {
        records(db, sql, [], object)
    } else {
        Ok(Vec::new())
    }
}
fn optional_canvas_records(
    db: &Connection,
    table: &'static str,
) -> Result<Vec<Value>, AnalyticsError> {
    let exists: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM canvas.sqlite_master WHERE type='table' AND name=?1)",
        [table],
        |r| r.get(0),
    )?;
    if exists {
        records(db, &format!("SELECT record FROM canvas.{table}"), [], table)
    } else {
        Ok(Vec::new())
    }
}
fn within_range(value: Option<f64>, from: Option<f64>, to: Option<f64>) -> bool {
    value.is_some_and(|n| n.is_finite())
        && from.is_none_or(|f| value.is_some_and(|n| n >= f))
        && to.is_none_or(|t| value.is_some_and(|n| n <= t))
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
    ) -> Result<Vec<Value>, AnalyticsError> {
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
            Ok(json!({"accountKey":account,"at":at,"data":data}))
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
    let page = if detail == "turns" {
        filter.page(db, table, "at DESC,id DESC", limit, offset)?
    } else {
        filter.rates(db, limit, offset)?
    };
    Ok(
        json!({detail:page,"pagination":{"limit":limit,"offset":offset,"total":total,"hasMore":offset+(page.len() as i64)<total}}),
    )
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
    let projected: Vec<Value> = rows
        .iter()
        .map(|row| {
            let mut value = Map::new();
            for field in fields {
                if let Some(v) = row.get(field) {
                    value.insert(field.into(), v.clone());
                }
            }
            Value::Object(value)
        })
        .collect();
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
