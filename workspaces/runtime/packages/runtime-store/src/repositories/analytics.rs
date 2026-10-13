//! Read-only analytics projections over the Python runtime's persisted rows.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::fs;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

use crate::RuntimeStore;
use rusqlite::{
    Connection, OpenFlags, OptionalExtension, params_from_iter, types::Value as SqlValue,
};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
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
    #[error("Unknown chat")]
    UnknownAgent,
    #[error("analytics read failed: {0}")]
    Read(#[from] AnalyticsStoreError),
    #[error("SQLite read failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
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
        Some("message-info") => message_info(&transaction, store.root(), options)?,
        Some(_) => return Err(AnalyticsError::InvalidRequest("Unknown analytics view")),
        None => rollup(&transaction, options, scope)?,
    };
    transaction.commit()?;
    Ok(value)
}

fn rollup(
    db: &Connection,
    options: &AnalyticsOptions,
    scope: &str,
) -> Result<Value, AnalyticsError> {
    let all_agents = records(
        db,
        "SELECT record FROM analytics_agents",
        [],
        "analytics_agents",
    )?;
    let agent_id = options.agent.as_deref();
    let selected = agent_id.and_then(|id| all_agents.iter().find(|row| row["id"] == id));
    if scope != "all" && selected.is_none() {
        return Err(AnalyticsError::InvalidRequest(
            "Select an agent for analytics",
        ));
    }
    let root = selected
        .and_then(|row| row.get("rootId"))
        .and_then(Value::as_str)
        .or(agent_id)
        .unwrap_or_default();
    let agents: Vec<Value> = all_agents
        .iter()
        .filter(|row| match scope {
            "all" => true,
            "agent" => row.get("id").and_then(Value::as_str) == agent_id,
            _ => {
                row.get("rootId")
                    .and_then(Value::as_str)
                    .unwrap_or_else(|| row["id"].as_str().unwrap_or_default())
                    == root
            }
        })
        .cloned()
        .collect();
    let limit = options.limit.unwrap_or(50).clamp(1, 500);
    let offset = options.offset.unwrap_or(0).max(0);
    let usage_where = clause(scope, agent_id, root, options.from, options.to);
    let usage_sql = format!("SELECT record FROM analytics_usage {usage_where} ORDER BY at,seq");
    let usage_rows = records(
        db,
        &usage_sql,
        params_from_iter(where_values(
            scope,
            agent_id,
            root,
            options.from,
            options.to,
        )),
        "analytics_usage",
    )?;
    let mut authoritative = HashSet::new();
    for row in &usage_rows {
        if truthy(row.get("responseId")) {
            authoritative.insert((
                string(row, "agentId"),
                string(row, "threadId"),
                string(row, "turnId"),
            ));
        }
    }
    let mut usage_totals = UsageTotals::default();
    let mut per_model: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut per_account: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut per_agent: BTreeMap<String, UsageTotals> = BTreeMap::new();
    let mut model_order = Vec::new();
    let mut account_order = Vec::new();
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
        usage_totals.add(&row, true);
        let model = string_default(&row, "model", "");
        if !per_model.contains_key(&model) {
            model_order.push(model.clone());
        }
        per_model.entry(model).or_default().add(&row, false);
        let account = string_default(&row, "accountKey", "default");
        if !per_account.contains_key(&account) {
            account_order.push(account.clone());
        }
        per_account.entry(account).or_default().add(&row, false);
        per_agent.entry(agent).or_default().add(&row, false);
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
    let mut chart_rows: Vec<_> = chart_points.into_values().collect();
    chart_rows.sort_by(|a, b| {
        number_at(a, "at")
            .partial_cmp(&number_at(b, "at"))
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let chart = chart_rows;
    if !options.export && provisional.len() > 100 {
        provisional.drain(..provisional.len() - 100);
    }

    let call_where = clause_with_tool(
        scope,
        agent_id,
        root,
        options.from,
        options.to,
        options.tool.as_deref(),
    );
    let calls_total: i64 = db.query_row(
        &format!("SELECT COUNT(*) FROM analytics_items {call_where}"),
        params_from_iter(where_values_with_tool(
            scope,
            agent_id,
            root,
            options.from,
            options.to,
            options.tool.as_deref(),
        )),
        |r| r.get(0),
    )?;
    let page_sql = format!(
        "SELECT record FROM analytics_items {call_where} ORDER BY at DESC,id{}",
        if options.export {
            String::new()
        } else {
            format!(" LIMIT {limit} OFFSET {offset}")
        }
    );
    let calls = records(
        db,
        &page_sql,
        params_from_iter(where_values_with_tool(
            scope,
            agent_id,
            root,
            options.from,
            options.to,
            options.tool.as_deref(),
        )),
        "analytics_items",
    )?;
    let items = records(
        db,
        &format!("SELECT record FROM analytics_items {usage_where} ORDER BY at DESC,id"),
        params_from_iter(where_values(
            scope,
            agent_id,
            root,
            options.from,
            options.to,
        )),
        "analytics_items",
    )?;
    let turns = records(
        db,
        &format!(
            "SELECT record FROM analytics_turns {usage_where} ORDER BY at DESC,id DESC{}",
            if options.export {
                String::new()
            } else {
                " LIMIT 100".to_owned()
            }
        ),
        params_from_iter(where_values(
            scope,
            agent_id,
            root,
            options.from,
            options.to,
        )),
        "analytics_turns",
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
    let total_turns: i64 = db.query_row(
        &format!("SELECT COUNT(*) FROM analytics_turns {usage_where}"),
        params_from_iter(where_values(
            scope,
            agent_id,
            root,
            options.from,
            options.to,
        )),
        |r| r.get(0),
    )?;
    let tracking = db
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='trackingSince'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
        .unwrap_or(0.0);
    let capture = db
        .query_row(
            "SELECT value FROM analytics_meta WHERE key='captureErrors'",
            [],
            |r| r.get::<_, String>(0),
        )
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| json!({"count":0,"last":null}));
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
    let requests = optional_canvas_records(db, "runtime_requests")?;
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
    let mut event_counts: BTreeMap<String, usize> = BTreeMap::new();
    let mut event_stmt =
        db.prepare("SELECT agent,kind,status,created FROM canvas.runtime_events")?;
    let events = event_stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, f64>(3)?,
        ))
    })?;
    for event in events {
        let (agent, kind, status, created) = event?;
        if selected_ids.contains(&agent) && within_range(Some(created), options.from, options.to) {
            *event_counts.entry(format!("{status}:{kind}")).or_default() += 1;
        }
    }
    let approval_statuses: BTreeSet<String> = requests
        .iter()
        .filter(|r| selected_ids.contains(&string(r, "agent")))
        .filter_map(|r| r.get("status").and_then(Value::as_str).map(str::to_owned))
        .collect();
    let approval_counts: Map<String, Value> = approval_statuses
        .into_iter()
        .map(|status| {
            let count = requests
                .iter()
                .filter(|r| {
                    selected_ids.contains(&string(r, "agent"))
                        && r.get("status").and_then(Value::as_str) == Some(&status)
                })
                .count();
            (status, json!(count))
        })
        .collect();
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
    let mut by_kind: BTreeMap<String, usize> = BTreeMap::new();
    let mut kind_order = Vec::new();
    let mut item_breakdown: BTreeMap<(String, String, String, String), ItemTotals> =
        BTreeMap::new();
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
        if !by_kind.contains_key(&kind) {
            kind_order.push(kind.clone());
        }
        *by_kind.entry(kind.clone()).or_default() += 1;
        let boundary = string_default(row, "payloadBoundary", "protocol");
        let category = string_default(row, "category", "");
        let role = string_default(row, "role", "");
        item_breakdown
            .entry((kind.clone(), boundary.clone(), category, role))
            .or_default()
            .add(row);
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
        all_tool.add(row);
        let is_model = boundary == "model";
        if options.tool.as_deref().is_none_or(|tool| tool == name) {
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
    let item_counts:Vec<Value>=kind_order.iter().filter_map(|kind|by_kind.get(kind).map(|count|json!({"type":kind,"count":count,"bytes":number_value(item_payload_sum(&items,kind,"bytes")),"chars":number_value(item_payload_sum(&items,kind,"chars"))}))).collect();
    let breakdown:Vec<Value>=item_breakdown.iter().map(|((kind,boundary,category,role),totals)|json!({"type":kind,"payloadBoundary":boundary,"category":if category.is_empty(){Value::Null}else{json!(category)},"role":if role.is_empty(){Value::Null}else{json!(role)},"count":totals.count,"inputBytes":number_value(totals.size("input","bytes")),"outputBytes":number_value(totals.size("output","bytes")),"inputMeasurements":totals.input_measurements,"outputMeasurements":totals.output_measurements})).collect();
    let accounts: BTreeSet<String> = agents
        .iter()
        .map(|a| string_default(a, "accountKey", "default"))
        .collect();
    let (limit_clause, limit_params) = limit_filter(&accounts, options.from, options.to);
    let total_limits: i64 = db.query_row(
        &format!("SELECT COUNT(*) FROM analytics_limits {limit_clause}"),
        params_from_iter(limit_params.clone()),
        |r| r.get(0),
    )?;
    let rate_rows: Vec<(i64, String, f64)> = db
        .prepare(&format!(
            "SELECT id,account,at FROM analytics_limits {limit_clause} ORDER BY at DESC,id DESC{}",
            if options.export {
                String::new()
            } else {
                " LIMIT 100".into()
            }
        ))?
        .query_map(params_from_iter(limit_params), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut rates = Vec::new();
    for (id, account, at) in rate_rows {
        let raw = db.query_row(
            "SELECT record FROM analytics_limits WHERE id=?1",
            [id],
            |r| r.get::<_, String>(0),
        )?;
        let data: Value = serde_json::from_str(&raw)
            .map_err(|_| AnalyticsError::InvalidStoredObject("analytics_limits"))?;
        rates.push(json!({"accountKey":account,"at":at,"data":data}));
    }
    let mut summary = json!({
        "agents": selected_ids.len(), "turns": turns_seen.len(), "usageSamples": usage_totals.count,
        "provisionalUsageSamples": provisional.len(), "exactResponseSamples": usage_totals.exact,
        "legacyUsageSamples": usage_totals.legacy, "tokens": usage_totals.tokens(),
        "tokenObservations": usage_totals.observations(), "peakContextTokens":usage_totals.peak_context,
        "peakContextPercent":usage_totals.peak_percent,"baselineMissingSamples":usage_totals.baseline_missing,
        "modelToolCalls":model_tool.count,"protocolToolCalls":protocol_tool.count,"observedToolRows":all_tool.count,"toolCalls":model_tool.count,"failedToolCalls":model_tool.failed,
        "modelFailedToolCalls":model_tool.failed,"protocolFailedToolCalls":protocol_tool.failed,"compactions":compactions.len(),
        "inputBytes":number_value(model_tool.size("input","bytes")),"outputBytes":number_value(model_tool.size("output","bytes")),"modelInputBytes":number_value(model_tool.size("input","bytes")),"modelOutputBytes":number_value(model_tool.size("output","bytes")),
        "protocolInputBytes":number_value(protocol_tool.size("input","bytes")),"protocolOutputBytes":number_value(protocol_tool.size("output","bytes")),"durationMs":model_tool.duration_total(),"modelDurationMs":model_tool.duration_total(),"protocolDurationMs":protocol_tool.duration_total(),
        "cacheHitRate":null,"cacheHitRateSamples":usage_totals.cache_pairs,"cacheHitRateTotalSamples":usage_totals.count
    });
    let mut output_agents = Vec::new();
    for mut record in agents {
        let agent = string(&record, "id");
        if let Some(runtime) = read_runtime_agent(db, &agent)?
            && let (Some(dst), Some(src)) = (record.as_object_mut(), runtime.as_object())
        {
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
                if let Some(value) = src.get(key) {
                    dst.insert(key.to_owned(), value.clone());
                }
            }
        }
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
    summary["inputBytes"] = number_value(model_tool.size("input", "bytes"));
    summary["outputBytes"] = number_value(model_tool.size("output", "bytes"));
    // Page records retain Python's compact API projection from the stored record.
    let generated_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs_f64();
    Ok(json!({
        "version":1,"generatedAt":generated_at,
        "filters":{"agent":agent_id,"scope":scope,"from":options.from,"to":options.to,"tool":options.tool},
        "coverage":{"trackingSince":tracking,"captureErrors":capture,"historyErrors":history.iter().filter(|r|r["status"]=="error").cloned().collect::<Vec<_>>(),"provisionalUsageSamples":provisional.len(),"tokenAttribution":"provider_usage_only","payloadMeasurement":"observed_protocol_payload","history":"live_and_stored_history","notes":["Tool filters affect tool calls only. Provider usage is scoped to the selected agents and time.","Item date filters use start time when known, otherwise completion time or the first observation. History can establish an earlier start.","Input and output sizes measure UTF-8 text or compact JSON observed at the protocol boundary, not context tokens or billed tokens.","Cached input is part of input. Reasoning output is part of output. Do not add these subsets twice.","Token totals use response-id records when available for a turn. Other notices in that turn remain provisional and are excluded.","Turns without response records use deduplicated last-usage notices. Current requests can remain pending until rollout import catches up. Missing requests are not inferred.","Reasoning items expose only protocol-visible content. Hidden reasoning text and unreported context overhead are unavailable.","Turn duration and first-output delay include tool execution and waits, not model-only latency.","Notification histogram buckets are full UTC hours and can overlap the selected time range.","Approval counts describe current stored requests across all time. Monitor and delivery-event filters use creation time.","Model and native tool calls, failures, and durations are separate populations. Main aliases describe model calls only.","Duration sums include parallel calls and are not elapsed session time. Missing measurements remain null."]},
        "summary":summary,"agents":agents_from_response(output_agents.clone()),"agentTotals":output_agents,
        "tools":tools,"modelTotals":model_rows,"accountTotals":account_rows,
        "operations":{"monitors":visible_monitors,"eventCounts":event_counts,"approvalCounts":approval_counts},"notifications":notification_rows,
        "history":history,"rateLimits":rates,"turns":turns,"timeline":timeline,"chartBuckets":chart,
        "timelineTotal":usage_totals.count,"provisionalUsage":provisional,"calls":calls,
        "itemRecords":non_tool,"itemRecordsTotal":non_tool_total,"itemBreakdown":breakdown,"compactions":compactions,"compactionSnapshots":snapshots,
        "pagination":{"limit":limit,"offset":offset,"total":calls_total,"hasMore":!options.export && offset+limit<calls_total},
        "items":item_counts,"detailPagination":{"rateLimits":{"limit":if options.export{total_limits}else{100},"offset":0,"total":total_limits,"hasMore":!options.export && total_limits>100},"turns":{"limit":if options.export{total_turns}else{100},"offset":0,"total":total_turns,"hasMore":!options.export && total_turns>100}}
    }))
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
    fn add(&mut self, row: &Value, metrics: bool) {
        self.count += 1;
        let delta = row.get("delta").unwrap_or(&Value::Null);
        for field in TOKEN_FIELDS {
            if let Some(n) = delta.get(field).and_then(Value::as_f64) {
                *self.sums.entry(field.into()).or_default() += n;
                *self.observations.entry(field.into()).or_default() += 1;
            }
        }
        if !metrics {
            return;
        }
        if truthy(row.get("responseId")) {
            self.exact += 1;
        } else {
            self.legacy += 1;
        }
        self.baseline_missing +=
            usize::from(row.get("baselineMissing").and_then(Value::as_bool) == Some(true));
        if let Some(n) = row.pointer("/last/totalTokens").and_then(Value::as_f64) {
            self.peak_context = Some(self.peak_context.map_or(n, |v| v.max(n)));
            if let Some(w) = row
                .get("modelContextWindow")
                .and_then(Value::as_f64)
                .filter(|w| *w > 0.0)
            {
                let p = n / w * 100.0;
                self.peak_percent = Some(self.peak_percent.map_or(p, |v| v.max(p)));
            }
        }
        if let (Some(input), Some(cached)) = (
            row.pointer("/delta/inputTokens").and_then(Value::as_f64),
            row.pointer("/delta/cachedInputTokens")
                .and_then(Value::as_f64),
        ) {
            if !truthy(row.get("inputTokensAreUncached")) && cached > input {
                return;
            }
            self.cache_pairs += 1;
            self.cache_input += if truthy(row.get("inputTokensAreUncached")) {
                input
                    + cached
                    + row
                        .pointer("/delta/cacheWriteInputTokens")
                        .and_then(Value::as_f64)
                        .unwrap_or(0.0)
            } else {
                input
            };
        }
    }
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
fn item_payload_sum(items: &[Value], kind: &str, field: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut count = 0;
    for row in items.iter().filter(|r| string(r, "type") == kind) {
        for dir in ["input", "output"] {
            if let Some(n) = row
                .get(dir)
                .and_then(|v| v.get(field))
                .and_then(Value::as_f64)
            {
                total += n;
                count += 1;
            }
        }
    }
    if count == 0 { None } else { Some(total) }
}

fn records<P: rusqlite::Params>(
    db: &Connection,
    sql: &str,
    params: P,
    table: &'static str,
) -> Result<Vec<Value>, AnalyticsError> {
    let mut stmt = db.prepare(sql)?;
    let rows = stmt.query_map(params, |row| row.get::<_, String>(0))?;
    rows.map(|raw| {
        let raw = raw?;
        let value: Value =
            serde_json::from_str(&raw).map_err(|_| AnalyticsError::InvalidStoredObject(table))?;
        if !value.is_object() {
            return Err(AnalyticsError::InvalidStoredObject(table));
        }
        Ok(value)
    })
    .collect()
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
fn limit_filter(
    accounts: &BTreeSet<String>,
    from: Option<f64>,
    to: Option<f64>,
) -> (String, Vec<SqlValue>) {
    let mut conditions = Vec::new();
    let mut params = Vec::new();
    if accounts.is_empty() {
        conditions.push("0".into())
    } else {
        conditions.push(format!(
            "account IN ({})",
            std::iter::repeat_n("?", accounts.len())
                .collect::<Vec<_>>()
                .join(",")
        ));
        params.extend(accounts.iter().cloned().map(SqlValue::Text));
    }
    if let Some(n) = from {
        conditions.push("at>=?".into());
        params.push(SqlValue::Real(n));
    }
    if let Some(n) = to {
        conditions.push("at<=?".into());
        params.push(SqlValue::Real(n));
    }
    (format!("WHERE {}", conditions.join(" AND ")), params)
}
fn clause(
    scope: &str,
    _agent: Option<&str>,
    _root: &str,
    from: Option<f64>,
    to: Option<f64>,
) -> String {
    let mut parts = Vec::new();
    match scope {
        "agent" => parts.push("agent=?"),
        "team" => parts.push("root=?"),
        _ => {}
    }
    if from.is_some() {
        parts.push("at>=?")
    }
    if to.is_some() {
        parts.push("at<=?")
    }
    if parts.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", parts.join(" AND "))
    }
}
fn where_values(
    scope: &str,
    agent: Option<&str>,
    root: &str,
    from: Option<f64>,
    to: Option<f64>,
) -> Vec<SqlValue> {
    let mut v = Vec::new();
    match scope {
        "agent" => v.push(SqlValue::Text(agent.unwrap_or_default().into())),
        "team" => v.push(SqlValue::Text(root.into())),
        _ => {}
    }
    if let Some(n) = from {
        v.push(SqlValue::Real(n))
    }
    if let Some(n) = to {
        v.push(SqlValue::Real(n))
    }
    v
}
fn clause_with_tool(
    scope: &str,
    a: Option<&str>,
    r: &str,
    f: Option<f64>,
    t: Option<f64>,
    tool: Option<&str>,
) -> String {
    let mut out = clause(scope, a, r, f, t);
    let parts = if out.is_empty() {
        &mut out
    } else {
        out.push_str(" AND ");
        &mut out
    };
    parts.push_str("is_tool=1");
    if tool.is_some() {
        parts.push_str(" AND name=?")
    }
    out
}
fn where_values_with_tool(
    s: &str,
    a: Option<&str>,
    r: &str,
    f: Option<f64>,
    t: Option<f64>,
    tool: Option<&str>,
) -> Vec<SqlValue> {
    let mut v = where_values(s, a, r, f, t);
    if let Some(x) = tool {
        v.push(SqlValue::Text(x.into()))
    }
    v
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
fn agents_from_response(rows: Vec<Value>) -> Vec<Value> {
    rows.into_iter()
        .map(|mut r| {
            if let Some(o) = r.as_object_mut() {
                for k in [
                    "tokens",
                    "usageSamples",
                    "toolCalls",
                    "modelToolCalls",
                    "protocolToolCalls",
                    "failedToolCalls",
                    "protocolFailedToolCalls",
                    "compactions",
                    "duration",
                    "protocolDuration",
                ] {
                    o.remove(k);
                }
            }
            r
        })
        .collect()
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
    let agents = records(
        db,
        "SELECT record FROM analytics_agents",
        [],
        "analytics_agents",
    )?;
    let agent = options.agent.as_deref();
    let selected = agent.and_then(|id| agents.iter().find(|a| a["id"] == id));
    if scope != "all" && selected.is_none() {
        return Err(AnalyticsError::InvalidRequest(
            "Select an agent for analytics",
        ));
    }
    let limit = options.limit.unwrap_or(100).clamp(1, 100);
    let offset = options.offset.unwrap_or(0).max(0);
    let root = selected
        .and_then(|a| a.get("rootId"))
        .and_then(Value::as_str)
        .or(agent)
        .unwrap_or_default();
    let (clause, params) = if detail == "turns" {
        (
            clause(scope, agent, root, options.from, options.to),
            where_values(scope, agent, root, options.from, options.to),
        )
    } else {
        let relevant = agents
            .iter()
            .filter(|a| match scope {
                "all" => true,
                "agent" => a["id"].as_str() == agent,
                _ => {
                    a.get("rootId")
                        .and_then(Value::as_str)
                        .unwrap_or_else(|| a["id"].as_str().unwrap_or_default())
                        == root
                }
            })
            .collect::<Vec<_>>();
        let accounts: BTreeSet<String> = relevant
            .iter()
            .map(|a| string_default(a, "accountKey", "default"))
            .collect();
        limit_filter(&accounts, options.from, options.to)
    };
    let table = if detail == "turns" {
        "analytics_turns"
    } else {
        "analytics_limits"
    };
    let total: i64 = db.query_row(
        &format!("SELECT COUNT(*) FROM {table} {clause}"),
        params_from_iter(params.clone()),
        |r| r.get(0),
    )?;
    let key = detail;
    let page = if detail == "turns" {
        records(
            db,
            &format!(
                "SELECT record FROM analytics_turns {clause} ORDER BY at DESC,id DESC LIMIT {limit} OFFSET {offset}"
            ),
            params_from_iter(params),
            "analytics_turns",
        )?
    } else {
        let refs:Vec<(i64,String,f64)>=db.prepare(&format!("SELECT id,account,at FROM analytics_limits {clause} ORDER BY at DESC,id DESC LIMIT {limit} OFFSET {offset}"))?.query_map(params_from_iter(params),|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?)))?.collect::<Result<_,_>>()?;
        let mut values = Vec::new();
        for (id, account, at) in refs {
            let raw = db.query_row(
                "SELECT record FROM analytics_limits WHERE id=?1",
                [id],
                |r| r.get::<_, String>(0),
            )?;
            let data: Value = serde_json::from_str(&raw)
                .map_err(|_| AnalyticsError::InvalidStoredObject("analytics_limits"))?;
            values.push(json!({"accountKey":account,"at":at,"data":data}));
        }
        values
    };
    Ok(
        json!({key:page,"pagination":{"limit":limit,"offset":offset,"total":total,"hasMore":offset+(page.len() as i64)<total}}),
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
    let all = raw_records(
        db,
        "SELECT id FROM analytics_usage WHERE agent=?1 AND thread IS ?2 AND turn=?3 LIMIT 2",
        rusqlite::params![agent, thread, turn],
        "analytics_usage",
    )?;
    if all.len() != 1 {
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
    let mut stmt = db.prepare(sql)?;
    let values = stmt.query_map(params, |r| r.get::<_, String>(0))?;
    values.map(|raw| parse_object(&raw?, table)).collect()
}

/// Return a signature-checked estimate persisted by the Python refresh worker.
/// `None` means the caller must answer the declared refresh dependency.
pub fn persisted_session_cost(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<Option<Value>, SessionCostError> {
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
    let cost = cost_projection_signature(&tx, store.root())?;
    let agents_sig = hash_json(&agent_signature_json(&members))?;
    let claude_sig = hash_json(&json!({"configs":[],"files":[]}))?;
    let pricing_sig = pricing_signature(store.root());
    tx.commit()?;
    let source = json!({"usage":usage,"pricingSignature":pricing_sig,"agentsSignature":agents_sig,"claudeSignature":claude_sig,"costProjection":cost});
    let digest = hex(&Sha256::digest(root.as_bytes()));
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
    #[error("Unknown chat")]
    UnknownAgent,
    #[error("Invalid stored object {0}")]
    InvalidStored(&'static str),
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
        && v.get("breakdown").is_some_and(Value::is_object)
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
    root: &Path,
) -> Result<Option<Value>, SessionCostError> {
    if !table_exists(db, "analytics_cost_state_v1")?
        || !table_exists(db, "analytics_cost_roots_v1")?
    {
        return Ok(None);
    }
    let Some((token,revision,floor))=db.query_row("SELECT token,COALESCE((SELECT revision FROM analytics_cost_roots_v1 WHERE root=?1),0),COALESCE((SELECT floor FROM analytics_cost_roots_v1 WHERE root=?1),0) FROM analytics_cost_state_v1 WHERE singleton=1",[root.to_string_lossy().as_ref()],|r|Ok((r.get::<_,String>(0)?,r.get::<_,i64>(1)?,r.get::<_,i64>(2)?))).optional()? else{return Ok(None)};
    let meta = fs::metadata(root.join("analytics.sqlite3")).ok();
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
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1)",
        [name],
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
    let encoded = serde_json::to_vec(value)?;
    Ok(hex(&Sha256::digest(encoded)))
}
fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        write!(out, "{b:02x}").expect("write to string");
    }
    out
}
