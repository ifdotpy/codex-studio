//! Pure analytics capture decisions and the synchronous notification consumer.
//!
//! Prepared payload measurements happen before the writer is held. A checked
//! snapshot selects only the affected rows; no unbounded history is loaded.
use crate::analytics_measurements::{item_sizes, payload_size};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_store::analytics_capture::{AnalyticsCaptureRepository as Repo, CaptureTable};
use studio_runtime_store::{RuntimeStore, StoreError, python_json_compact, sha256_hex};
use thiserror::Error;

pub const MAX_CAPTURE_BYTES: usize = 64 * 1024 * 1024;
pub(crate) const TOKEN_FIELDS: [&str; 6] = [
    "inputTokens",
    "cachedInputTokens",
    "cacheWriteInputTokens",
    "outputTokens",
    "reasoningOutputTokens",
    "totalTokens",
];
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CaptureSource {
    Live,
    Rollout,
}
impl CaptureSource {
    pub(crate) fn name(self) -> &'static str {
        match self {
            Self::Live => "live",
            Self::Rollout => "rollout",
        }
    }
}
#[derive(Debug, Error)]
pub enum CaptureError {
    #[error("invalid analytics input at {0}")]
    Invalid(&'static str),
    #[error("analytics capture belongs to a stale agent account, epoch or thread")]
    StaleIdentity,
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("analytics SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
}
#[derive(Clone)]
pub struct PreparedCapture {
    agent: Value,
    method: String,
    params: Value,
    at: f64,
    recorded_at: f64,
    source: CaptureSource,
    input: Value,
    output: Value,
    stream: Value,
    model: bool,
}
impl PreparedCapture {
    pub fn event(
        agent: &Value,
        method: &str,
        params: &Value,
        at: f64,
        recorded_at: f64,
        source: CaptureSource,
    ) -> Result<Self, CaptureError> {
        CaptureTable::Agents.validate(agent)?;
        if !params.is_object() {
            return Err(CaptureError::Invalid("/params"));
        }
        if !at.is_finite() || at < 0. || !recorded_at.is_finite() || recorded_at < 0. {
            return Err(CaptureError::Invalid("/at"));
        }
        if params.to_string().len() > MAX_CAPTURE_BYTES {
            return Err(CaptureError::Invalid("/params/size"));
        }
        if let Some((pointer, _)) = studio_runtime_schema::first_error("AnalyticsNotice", params)
            .map_err(StoreError::SchemaValidation)?
        {
            return Err(StoreError::SchemaValidation(format!("analytics_notice{pointer}")).into());
        }
        let (input, output) = item_sizes(
            params["item"]["type"].as_str().unwrap_or(""),
            &params["item"],
        );
        Ok(Self {
            agent: agent.clone(),
            method: method.to_owned(),
            params: params.clone(),
            at,
            recorded_at,
            source,
            input,
            output,
            stream: payload_size(&params["delta"]),
            model: false,
        })
    }
    pub fn model_payload(
        agent: &Value,
        params: &Value,
        at: f64,
        recorded_at: f64,
    ) -> Result<Self, CaptureError> {
        let mut result = Self::event(
            agent,
            "model/payload",
            params,
            at,
            recorded_at,
            CaptureSource::Rollout,
        )?;
        result.model = true;
        let kind = params["type"].as_str().unwrap_or("unknown");
        if is_model_input(kind) {
            result.input = payload_size(
                params
                    .get("arguments")
                    .or_else(|| params.get("input"))
                    .unwrap_or(&Value::Null),
            );
        } else if is_model_output(kind) {
            result.output = payload_size(
                params
                    .get("output")
                    .or_else(|| params.get("content"))
                    .unwrap_or(&Value::Null),
            );
        } else {
            let size = payload_size(
                params
                    .get("content")
                    .or_else(|| params.get("summary"))
                    .unwrap_or(&Value::Null),
            );
            if matches!(
                params["role"].as_str(),
                Some("user" | "system" | "developer")
            ) {
                result.input = size
            } else {
                result.output = size
            }
        }
        Ok(result)
    }
    pub fn agent(&self) -> &Value {
        &self.agent
    }
    fn meta(&self) -> Value {
        let a = &self.agent;
        let mut meta = project(
            a,
            &[
                "accountKey",
                "accountHistory",
                "provider",
                "threadId",
                "model",
                "effort",
                "fastMode",
                "daybreakEnabled",
                "cyberAccessProgram",
            ],
        );
        meta["agentId"] = a["id"].clone();
        meta["agentName"] = a["name"].clone();
        meta["rootId"] = first(&[&a["rootId"], &a["id"]]);
        meta["accountKey"] = a.get("accountKey").cloned().unwrap_or(json!("default"));
        if !meta["accountHistory"].is_array() {
            meta["accountHistory"] = json!([])
        }
        if self.source != CaptureSource::Live && !self.model {
            meta["daybreakEnabled"] = Value::Null;
            meta["cyberAccessProgram"] = Value::Null;
        }
        meta["threadId"] = first(&[&self.params["threadId"], &a["threadId"]]);
        meta["turnId"] = if self.model {
            self.params["_analyticsTurnId"].clone()
        } else {
            first(&[
                &self.params["turnId"],
                &self.params["turn"]["id"],
                &a["turnId"],
            ])
        };
        meta
    }
    fn item_key(&self, meta: &Value) -> Option<String> {
        if self.method == "analytics/compaction" {
            return Some(key(
                meta,
                &first(&[
                    &self.params["id"],
                    &self.params["_analyticsId"],
                    &json!(self.at.to_string()),
                ]),
            ));
        }
        if self.model {
            let p = &self.params;
            let call = first(&[&p["call_id"], &p["callId"], &p["id"], &p["_analyticsId"]]);
            if !truthy(&call) {
                return None;
            }
            let category = model_category(p);
            let identity =
                if category == "compactionReplacement" || category == "compactionGuardian" {
                    first(&[&p["_analyticsId"], &call])
                } else {
                    call
                };
            Some(format!(
                "{}:{}:model:{}:{}",
                text(&meta["agentId"]),
                text(&meta["threadId"]),
                text(&identity),
                category
            ))
        } else {
            let id = first(&[&self.params["item"]["id"], &self.params["itemId"]]);
            truthy(&id).then(|| key(meta, &id))
        }
    }
}
/// Raw database rows are accepted only through `from_records`.
#[derive(Default)]
struct CaptureRows {
    pub turn: Option<Value>,
    pub item: Option<Value>,
    pub usage: Option<Value>,
    pub collision_usage: Option<Value>,
    pub exact_response: Option<Value>,
    pub prior_usage: Option<Value>,
    pub last_limit: Option<Value>,
}
pub struct CaptureSnapshot {
    prepared: PreparedCapture,
    meta: Value,
    rows: CaptureRows,
}
impl CaptureSnapshot {
    fn from_records(prepared: PreparedCapture, rows: CaptureRows) -> Result<Self, CaptureError> {
        for (table, row) in [
            (CaptureTable::Turns, &rows.turn),
            (CaptureTable::Items, &rows.item),
            (CaptureTable::Usage, &rows.usage),
            (CaptureTable::Usage, &rows.collision_usage),
            (CaptureTable::Usage, &rows.exact_response),
            (CaptureTable::Usage, &rows.prior_usage),
        ] {
            if let Some(row) = row {
                table.validate(row)?;
            }
        }
        let meta = prepared.meta();
        for row in [
            &rows.turn,
            &rows.item,
            &rows.usage,
            &rows.collision_usage,
            &rows.exact_response,
            &rows.prior_usage,
        ]
        .into_iter()
        .flatten()
        {
            if row["agentId"] != meta["agentId"]
                || row["threadId"] != meta["threadId"]
                || row["rootId"] != meta["rootId"]
            {
                return Err(CaptureError::Invalid("/snapshot/row/identity"));
            }
        }
        for row in [&rows.turn, &rows.item].into_iter().flatten() {
            if row["turnId"] != meta["turnId"] {
                return Err(CaptureError::Invalid("/snapshot/row/turnId"));
            }
        }
        Ok(Self {
            prepared,
            meta,
            rows,
        })
    }
    pub fn load(db: &Connection, prepared: PreparedCapture) -> Result<Self, CaptureError> {
        let meta = prepared.meta();
        let turn = truthy(&meta["turnId"])
            .then(|| Repo::read(db, CaptureTable::Turns, &key(&meta, &meta["turnId"])))
            .transpose()?
            .flatten();
        let item = prepared
            .item_key(&meta)
            .map(|id| Repo::read(db, CaptureTable::Items, &id))
            .transpose()?
            .flatten();
        let mut rows = CaptureRows {
            turn,
            item,
            ..CaptureRows::default()
        };
        if prepared.method == "thread/tokenUsage/updated" {
            let mut id = usage_key(&meta, &prepared.params);
            rows.usage = Repo::read(db, CaptureTable::Usage, &id)?;
            if rows.usage.is_none()
                && prepared.params["usageSource"] != "claudeResponse"
                && number(&prepared.params["tokenUsage"]["total"]["totalTokens"]).is_none()
            {
                let total = prepared.params["tokenUsage"]
                    .get("total")
                    .filter(|v| v.is_object())
                    .cloned()
                    .unwrap_or(json!({}));
                rows.usage = Repo::usage_without_total(
                    db,
                    text(&meta["agentId"]),
                    &meta["threadId"],
                    &meta["turnId"],
                    &total,
                )?;
                if let Some(row) = &rows.usage {
                    id = text(&row["id"]).to_owned();
                }
            }
            let response = &prepared.params["responseId"];
            if truthy(response) {
                rows.exact_response =
                    Repo::usage_response(db, text(&meta["agentId"]), &meta["threadId"], response)?;
            }
            if rows.exact_response.is_none()
                && rows.usage.as_ref().is_some_and(|v| {
                    truthy(response) && truthy(&v["responseId"]) && v["responseId"] != *response
                })
            {
                rows.collision_usage =
                    Repo::read(db, CaptureTable::Usage, &format!("{id}:{}", text(response)))?;
            }
            rows.prior_usage = Repo::usage_prior(
                db,
                text(&meta["agentId"]),
                &meta["threadId"],
                prepared.at,
                domain(&prepared.params),
            )?;
        }
        if prepared.method == "analytics/rateLimits" {
            rows.last_limit = Repo::last_limit(db, &meta["accountKey"])?;
        }
        Self::from_records(prepared, rows)
    }
}
#[derive(Clone, Debug, PartialEq)]
pub struct ResponseRateAssociation {
    pub agent: String,
    pub thread: Option<String>,
    pub turn: Option<String>,
    pub response: String,
    pub output_tokens: f64,
}
#[derive(Default)]
pub struct CaptureDecision {
    pub rows: Vec<(CaptureTable, Value)>,
    pub notifications: Vec<Value>,
    pub limit: Option<Value>,
    pub generation_roots: Vec<String>,
    pub response_rate: Option<ResponseRateAssociation>,
}
impl CaptureDecision {
    pub fn apply(&self, tx: &Transaction<'_>) -> Result<(), CaptureError> {
        for (table, row) in &self.rows {
            Repo::write(tx, *table, row)?;
        }
        for n in &self.notifications {
            tx.execute("INSERT INTO analytics_notifications VALUES (?,?,?,?,?,1,?) ON CONFLICT(id) DO UPDATE SET count=count+1,bytes=bytes+excluded.bytes",params![n["id"].as_str(),n["agent"].as_str(),n["root"].as_str(),n["method"].as_str(),n["hour"].as_i64(),n["bytes"].as_i64()])?;
        }
        if let Some(n) = &self.limit {
            tx.execute(
                "INSERT INTO analytics_limits(account,at,record) VALUES (?,?,?)",
                params![
                    n["account"].as_str(),
                    n["at"].as_f64(),
                    n["record"].to_string()
                ],
            )?;
        }
        if !self.generation_roots.is_empty() {
            Repo::generation(tx, &self.generation_roots)?;
        }
        Ok(())
    }
}
pub fn capture(
    store: &RuntimeStore,
    prepared: &PreparedCapture,
) -> Result<CaptureDecision, CaptureError> {
    store.analytics_transaction(|tx, canvas| {
        check_identity(canvas, prepared.agent())?;
        let decision = decide(CaptureSnapshot::load(tx, prepared.clone())?)?;
        decision.apply(tx)?;
        Ok(decision)
    })
}
pub(crate) fn check_identity(canvas: &Connection, agent: &Value) -> Result<(), CaptureError> {
    let raw: Option<String> = canvas
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?",
            [agent["id"].as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let current: Value = raw
        .map(|r| serde_json::from_str(&r).map_err(StoreError::from))
        .transpose()?
        .ok_or(CaptureError::StaleIdentity)?;
    if ["threadId", "accountKey", "epoch"]
        .iter()
        .any(|k| current.get(k) != agent.get(k))
    {
        return Err(CaptureError::StaleIdentity);
    }
    Ok(())
}
pub fn decide(snapshot: CaptureSnapshot) -> Result<CaptureDecision, CaptureError> {
    let CaptureSnapshot {
        prepared: q,
        mut meta,
        rows,
    } = snapshot;
    let p = &q.params;
    let method = q.method.as_str();
    let at = q.at;
    let mut result = CaptureDecision::default();
    let mut agent = project(
        &q.agent,
        &[
            "id",
            "name",
            "rootId",
            "parentId",
            "accountKey",
            "accountHistory",
            "provider",
            "threadId",
            "model",
            "effort",
            "fastMode",
            "daybreakEnabled",
            "cyberAccessProgram",
            "cwd",
            "deletedAt",
        ],
    );
    if !agent["accountHistory"].is_array() {
        agent["accountHistory"] = json!([])
    }
    result.rows.push((CaptureTable::Agents, agent));
    if q.source == CaptureSource::Live && !q.model {
        let hour = (at / 3600.).floor() as i64 * 3600;
        result.notifications.push(
            json!({"id":python_json_compact(&json!([q.agent["id"],method,hour])),"agent":q.agent["id"],"root":meta["rootId"],"method":method,"hour":hour,"bytes":python_json_compact(p).len()}),
        );
    }
    if method == "thread/tokenUsage/updated" {
        usage(&q, &mut meta, &rows, &mut result);
        return Ok(result);
    }
    if q.model {
        model_item(&q, &meta, rows.item, &mut result);
        return Ok(result);
    }
    if truthy(&meta["turnId"])
        && matches!(
            method,
            "turn/started"
                | "turn/completed"
                | "item/agentMessage/delta"
                | "item/started"
                | "item/completed"
        )
    {
        let mut row=rows.turn.clone().unwrap_or_else(||merge(&meta,json!({"id":key(&meta,&meta["turnId"]),"at":at,"startedAt":null,"finishedAt":null,"firstOutputAt":null,"durationMs":null,"firstOutputDelayMs":null,"status":"unknown","source":q.source.name()})));
        match method {
            "turn/started" => {
                if q.source == CaptureSource::Live {
                    row["daybreakEnabled"] = meta["daybreakEnabled"].clone();
                    row["cyberAccessProgram"] = meta["cyberAccessProgram"].clone();
                }
                row["startedAt"] = json!(number(&row["startedAt"]).map_or(at, |n| n.min(at)));
                if row["finishedAt"].is_null() {
                    row["status"] = json!("running")
                }
            }
            "turn/completed" => {
                let error = &p["turn"]["error"];
                let status = if truthy(error) {
                    json!("failed")
                } else {
                    p["turn"].get("status").cloned().unwrap_or(json!("unknown"))
                };
                let previous = truthy(&row["error"]) || row["status"] == "failed";
                let incoming = truthy(error) || status == "failed";
                if !previous
                    || incoming
                        && (!truthy(&row["error"])
                            || q.source == CaptureSource::Live
                            || row.get("terminalSource").unwrap_or(&row["source"]) != "live")
                {
                    row["finishedAt"] = json!(at);
                    row["status"] = status;
                    row["error"] = if incoming {
                        first(&[error, &row["error"]])
                    } else {
                        Value::Null
                    };
                    row["terminalSource"] = json!(q.source.name());
                }
                for (source, target) in [
                    ("durationMs", "nativeDurationMs"),
                    ("timeToFirstTokenMs", "nativeTimeToFirstTokenMs"),
                ] {
                    if let Some(n) = number(&p[source]) {
                        row[target] = json!(n)
                    }
                }
            }
            _ => {
                if (method == "item/agentMessage/delta" && truthy(&p["delta"])
                    || p["item"]["type"] == "agentMessage" && truthy(&p["item"]["text"]))
                    && row["firstOutputAt"].is_null()
                {
                    row["firstOutputAt"] = json!(at)
                }
            }
        }
        timings(&mut row);
        result.rows.push((CaptureTable::Turns, row));
    }
    if method == "analytics/rateLimits" {
        if rows.last_limit.as_ref().map(limit_content) != Some(limit_content(p)) {
            result.limit = Some(json!({"account":meta["accountKey"],"at":at,"record":p}));
        }
        return Ok(result);
    }
    if method == "analytics/compaction" {
        let mut next = q.clone();
        next.method = "item/completed".into();
        next.params["item"] = json!({"id":first(&[&p["id"],&p["_analyticsId"],&json!(at.to_string())]),"type":"compactionSnapshot","compactionMetadata":p});
        let recursive = decide(CaptureSnapshot::from_records(next, rows)?)?;
        result.rows.extend(
            recursive
                .rows
                .into_iter()
                .filter(|(table, _)| *table != CaptureTable::Agents),
        );
        result.notifications.extend(recursive.notifications);
        return Ok(result);
    }
    protocol_item(&q, &meta, rows.item, &mut result);
    Ok(result)
}
fn protocol_item(
    q: &PreparedCapture,
    meta: &Value,
    existing: Option<Value>,
    result: &mut CaptureDecision,
) {
    let p = &q.params;
    let item = &p["item"];
    let method = q.method.as_str();
    let Some(id) = q.item_key(meta).filter(|_| method.starts_with("item/")) else {
        return;
    };
    let row = if !matches!(method, "item/started" | "item/completed") {
        let Some(mut row) = existing else { return };
        if !row["finishedAt"].is_null()
            || !(method.ends_with("Delta") || method.ends_with("/delta"))
        {
            return;
        }
        let Some(delta) = p["delta"].as_str() else {
            return;
        };
        let mut stream = row
            .get("stream")
            .filter(|s| s.is_object())
            .cloned()
            .unwrap_or(json!({"bytes":0,"chars":0,"lines":0,"deltas":0}));
        for k in ["bytes", "chars", "lines"] {
            let size = if k == "lines" {
                delta.bytes().filter(|c| *c == b'\n').count() as u64
            } else {
                q.stream[k].as_u64().unwrap_or(0)
            };
            stream[k] = json!(stream[k].as_u64().unwrap_or(0).saturating_add(size));
        }
        stream["deltas"] = json!(stream["deltas"].as_u64().unwrap_or(0).saturating_add(1));
        row["stream"] = stream;
        row
    } else {
        if existing
            .as_ref()
            .is_some_and(|r| !r["finishedAt"].is_null())
            && method == "item/started"
        {
            return;
        }
        let mut row=existing.unwrap_or_else(||merge(meta,json!({"id":id,"itemId":first(&[&item["id"],&p["itemId"]]),"startedAt":if method=="item/started"{json!(q.at)}else{Value::Null},"at":q.at,"source":q.source.name(),"input":null,"output":null,"payloadBoundary":"protocol"})));
        if row["source"] == "live" && q.source != CaptureSource::Live {
            for (src, dst) in [("startedAt", "startedAt"), ("completedAt", "finishedAt")] {
                if let Some(n) = number(&p[src]) {
                    row[dst] = json!(n)
                }
            }
            if let (Some(start), Some(end)) =
                (number(&row["startedAt"]), number(&row["finishedAt"]))
                && row["durationSource"] != "provider"
            {
                row["durationMs"] = json!(((end - start) * 1000.).max(0.));
                row["durationSource"] = json!("native_item_timestamps")
            }
        } else {
            let kind = first(&[&item["type"], &row["type"], &json!("unknown")]);
            let kind_name = kind.as_str().unwrap_or("unknown");
            row["name"] = first(&[&item["tool"], &item["name"], &kind]);
            row["isTool"] = json!(!matches!(
                kind_name,
                "userMessage"
                    | "agentMessage"
                    | "reasoning"
                    | "plan"
                    | "contextCompaction"
                    | "compactionSnapshot"
            ));
            row["type"] = kind;
            row["recordedAt"] = json!(q.recorded_at);
            row["source"] = json!(q.source.name());
            if let Some(n) = number(&p["startedAt"]) {
                row["startedAt"] = json!(n)
            }
            row["timestampSource"] = timestamp_source(q);
            if method == "item/completed" {
                row["finishedAt"] = json!(number(&p["completedAt"]).unwrap_or(q.at));
            }
            row["status"] = json!(if method != "item/completed" {
                "running"
            } else if item["success"] == false
                || truthy(&item["error"])
                || matches!(item["status"].as_str(), Some("failed" | "declined"))
                || !item["exitCode"].is_null() && item["exitCode"] != 0
            {
                "failed"
            } else {
                "completed"
            });
            for k in ["command", "cwd", "server", "exitCode", "processId"] {
                if let Some(v) = item.get(k) {
                    row[k] = v.clone()
                }
            }
            if truthy(&item["compactionMetadata"]) {
                row["compactionMetadata"] = item["compactionMetadata"].clone();
            }
            if truthy(&item["error"]) {
                let s = item["error"]
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| python_json_compact(&item["error"]));
                row["error"] = json!(s.chars().take(2000).collect::<String>());
            }
            let supplied = number(&item["durationMs"]);
            let duration = supplied.or_else(|| {
                if method == "item/completed" {
                    number(&row["startedAt"])
                        .map(|s| ((number(&row["finishedAt"]).unwrap_or(q.at) - s) * 1000.).max(0.))
                } else {
                    None
                }
            });
            row["durationMs"] = json!(duration);
            row["durationSource"] = json!(if supplied.is_some() {
                Some("provider")
            } else if duration.is_some() {
                Some("observed_wall_time")
            } else {
                None
            });
            if !q.input.is_null() {
                row["input"] = q.input.clone()
            }
            if !q.output.is_null() {
                row["output"] = q.output.clone()
            }
            row["coverage"] = json!(if q.source == CaptureSource::Live {
                "protocol_payload"
            } else {
                q.source.name()
            });
            row["payloadTruncated"] = item["outputTruncated"].clone();
        }
        row
    };
    result
        .rows
        .push((CaptureTable::Items, finish_item(row, q.recorded_at)));
}
fn model_item(
    q: &PreparedCapture,
    meta: &Value,
    existing: Option<Value>,
    result: &mut CaptureDecision,
) {
    let p = &q.params;
    let Some(id) = q.item_key(meta) else { return };
    let kind = p["type"].as_str().unwrap_or("unknown");
    let category = model_category(p);
    let tool = category == "tool" && (is_model_input(kind) || is_model_output(kind));
    let call = first(&[&p["call_id"], &p["callId"], &p["id"], &p["_analyticsId"]]);
    let mut row=existing.unwrap_or_else(||merge(meta,json!({"id":id,"itemId":call,"at":q.at,"startedAt":if is_model_input(kind){json!(q.at)}else{Value::Null},"finishedAt":null,"type":if tool{"modelToolCall"}else{"modelMessage"},"name":first(&[&p["name"],&json!(kind)]),"isTool":tool,"input":null,"output":null,"durationMs":null,"payloadBoundary":"model","category":category,"role":p["role"],"callId":call,"snapshotItemId":if matches!(category,"compactionReplacement"|"compactionGuardian"){p["_analyticsId"].clone()}else{Value::Null}})));
    row["source"] = json!(q.source.name());
    row["recordedAt"] = json!(q.recorded_at);
    row["coverage"] = json!("model_protocol_payload");
    row["timestampSource"] = p
        .get("_analyticsTimestampSource")
        .cloned()
        .unwrap_or(json!("record"));
    if truthy(&p["name"]) {
        row["name"] = json!(format!(
            "{}{}",
            p["namespace"]
                .as_str()
                .filter(|s| !s.is_empty())
                .map(|s| format!("{s}."))
                .unwrap_or_default(),
            text(&p["name"])
        ));
        row["namespace"] = p["namespace"].clone();
    }
    if is_model_input(kind) {
        row["input"] = q.input.clone();
        row["modelInput"] = q.input.clone();
        row["startedAt"] = json!(q.at);
        row["status"] = json!(if row["finishedAt"].is_null() {
            "running"
        } else {
            "completed"
        })
    } else if is_model_output(kind) {
        row["output"] = q.output.clone();
        row["modelOutput"] = q.output.clone();
        row["finishedAt"] = json!(q.at);
        row["status"] = json!(if p["is_error"] == true {
            "failed"
        } else {
            "completed"
        })
    } else {
        let direction = if matches!(p["role"].as_str(), Some("user" | "system" | "developer")) {
            "input"
        } else {
            "output"
        };
        row[direction] = if direction == "input" {
            q.input.clone()
        } else {
            q.output.clone()
        };
        row["status"] = json!("completed");
        row["finishedAt"] = json!(q.at)
    }
    if let (Some(start), Some(end)) = (number(&row["startedAt"]), number(&row["finishedAt"])) {
        row["durationMs"] = json!(((end - start) * 1000.).max(0.));
        row["durationSource"] = json!("observed_wall_time");
    }
    result
        .rows
        .push((CaptureTable::Items, finish_item(row, q.recorded_at)));
}
fn finish_item(mut row: Value, now: f64) -> Value {
    if row.get("firstRecordedAt").is_none() {
        row["firstRecordedAt"] = row.get("recordedAt").cloned().unwrap_or(json!(now));
    }
    if row.get("firstSourceAt").is_none() {
        row["firstSourceAt"] = row["at"].clone();
    }
    row["at"] = json!(
        number(&row["startedAt"])
            .or_else(|| number(&row["finishedAt"]))
            .or_else(|| number(&row["at"]))
    );
    row
}
fn timings(row: &mut Value) {
    if let Some(start) = number(&row["startedAt"]) {
        for (event, metric) in [
            ("finishedAt", "durationMs"),
            ("firstOutputAt", "firstOutputDelayMs"),
        ] {
            if let Some(end) = number(&row[event]) {
                row[metric] = json!(((end - start) * 1000.).max(0.));
            }
        }
    }
}
fn is_model_input(kind: &str) -> bool {
    matches!(kind, "function_call" | "custom_tool_call" | "tool_call")
}
fn is_model_output(kind: &str) -> bool {
    matches!(
        kind,
        "function_call_output" | "custom_tool_call_output" | "tool_result"
    )
}
fn model_category(p: &Value) -> &str {
    p["_analyticsCategory"]
        .as_str()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            if is_model_input(text(&p["type"])) || is_model_output(text(&p["type"])) {
                "tool"
            } else {
                "message"
            }
        })
}
fn limit_content(p: &Value) -> Value {
    match p.as_object() {
        Some(m) => Value::Object(
            m.iter()
                .filter(|(k, _)| {
                    !matches!(k.as_str(), "at" | "processedAt" | "checkedAt") && !k.starts_with('_')
                })
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
        ),
        None => p.clone(),
    }
}
pub(crate) fn number(v: &Value) -> Option<f64> {
    v.as_f64().filter(|n| n.is_finite() && *n >= 0.)
}
pub(crate) fn text(v: &Value) -> &str {
    v.as_str().unwrap_or("None")
}
pub(crate) fn first(values: &[&Value]) -> Value {
    values
        .iter()
        .find(|v| truthy(v))
        .map(|v| (*v).clone())
        .unwrap_or_default()
}
pub(crate) fn project(v: &Value, keys: &[&str]) -> Value {
    Value::Object(keys.iter().map(|k| ((*k).into(), v[*k].clone())).collect())
}
pub(crate) fn merge(meta: &Value, extra: Value) -> Value {
    let mut value = meta.clone();
    if let (Some(v), Some(e)) = (value.as_object_mut(), extra.as_object()) {
        v.extend(e.clone());
    }
    value
}
fn key(meta: &Value, id: &Value) -> String {
    format!(
        "{}:{}:{}",
        text(&meta["agentId"]),
        text(&meta["threadId"]),
        text(id)
    )
}
fn timestamp_source(q: &PreparedCapture) -> Value {
    q.params
        .get("_analyticsTimestampSource")
        .cloned()
        .unwrap_or(json!(if q.source == CaptureSource::Live {
            "observed"
        } else {
            "record"
        }))
}
fn hash(v: &Value) -> String {
    sha256_hex(python_json_compact(v).as_bytes())
}
fn domain(p: &Value) -> &str {
    if truthy(&p["rawTokenUsageRecord"]) {
        "response"
    } else {
        "nativeNotice"
    }
}
fn fingerprint(meta: &Value, p: &Value) -> String {
    let total = &p["tokenUsage"]["total"];
    hash(&json!([
        meta["turnId"],
        if number(&total["totalTokens"]).is_some() {
            total["totalTokens"].clone()
        } else {
            total.clone()
        }
    ]))
}
fn usage_key(meta: &Value, p: &Value) -> String {
    if p["usageSource"] == "claudeResponse" {
        hash(&json!([meta["agentId"], meta["threadId"], p["responseId"]]))
    } else {
        hash(&json!([
            meta["agentId"],
            meta["threadId"],
            fingerprint(meta, p)
        ]))
    }
}
fn token_numbers(v: &Value) -> Value {
    Value::Object(
        TOKEN_FIELDS
            .iter()
            .map(|k| {
                (
                    (*k).into(),
                    if number(&v[*k]).is_some() {
                        v[*k].clone()
                    } else {
                        Value::Null
                    },
                )
            })
            .collect(),
    )
}
fn usage(q: &PreparedCapture, meta: &mut Value, rows: &CaptureRows, out: &mut CaptureDecision) {
    let p = &q.params;
    let raw = if p["tokenUsage"].is_object() {
        p["tokenUsage"].clone()
    } else {
        json!({})
    };
    let total = raw
        .get("total")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let last = raw
        .get("last")
        .filter(|v| v.is_object())
        .cloned()
        .unwrap_or(json!({}));
    let response = &p["responseId"];
    if let Some(turn) = &rows.turn
        && turn["accountKey"] == meta["accountKey"]
        && !turn["cyberAccessProgram"].is_null()
    {
        meta["daybreakEnabled"] = turn["daybreakEnabled"].clone();
        meta["cyberAccessProgram"] = turn["cyberAccessProgram"].clone();
    }
    if q.source == CaptureSource::Rollout && truthy(response) {
        let usage = p.get("requestUsage").filter(|v| truthy(v)).unwrap_or(&last);
        if let Some(n) = number(&usage["outputTokens"]) {
            out.response_rate = Some(ResponseRateAssociation {
                agent: text(&meta["agentId"]).into(),
                thread: meta["threadId"].as_str().map(str::to_owned),
                turn: meta["turnId"].as_str().map(str::to_owned),
                response: text(response).into(),
                output_tokens: n,
            });
        }
    }
    let mut id = rows
        .usage
        .as_ref()
        .map(|v| text(&v["id"]).to_owned())
        .unwrap_or_else(|| usage_key(meta, p));
    let mut existing = rows.usage.clone();
    if rows.exact_response.is_none()
        && existing.as_ref().is_some_and(|r| {
            truthy(response) && truthy(&r["responseId"]) && r["responseId"] != *response
        })
    {
        id.push(':');
        id.push_str(text(response));
        existing = rows.collision_usage.clone();
    }
    if p["usageSource"] == "claudeResponse" {
        if !truthy(response) || !p["requestUsage"].is_object() {
            return;
        }
        let usage = &p["requestUsage"];
        let mut delta = token_numbers(usage);
        delta["totalTokens"] = json!(
            [
                "inputTokens",
                "cachedInputTokens",
                "cacheWriteInputTokens",
                "outputTokens"
            ]
            .iter()
            .map(|k| number(&usage[*k]).unwrap_or(0.))
            .sum::<f64>()
        );
        let mut row = merge(
            meta,
            json!({"id":id,"at":q.at,"recordedAt":q.recorded_at,"source":q.source.name(),"timestampSource":timestamp_source(q),"model":first(&[&p["model"],&meta["model"]]),"last":last,"total":total,"delta":delta,"inputTokensAreUncached":true,"raw":raw,"cumulativeDelta":token_numbers(&Value::Null),"counterDomain":"response","modelContextWindow":number(&raw["modelContextWindow"]),"reset":false,"baselineMissing":false,"fingerprint":hash(&json!([response,usage])),"responseId":response,"usageSource":"claudeResponse","requestUsage":usage}),
        );
        if let Some(previous) = existing.as_ref() {
            if row["modelContextWindow"].is_null() {
                row["modelContextWindow"] = json!(
                    number(&previous["modelContextWindow"])
                        .or_else(|| number(&previous["raw"]["modelContextWindow"]))
                );
                if !previous["raw"]["modelContextWindow"].is_null() {
                    row["raw"]["modelContextWindow"] =
                        previous["raw"]["modelContextWindow"].clone();
                }
            }
            if same_content(&response_content(previous), &response_content(&row)) {
                return;
            }
        }
        mark_generation(meta, existing.as_ref(), out);
        out.rows.push((CaptureTable::Usage, row));
        return;
    }
    if let Some(exact) = rows.exact_response.as_ref() {
        if !truthy(&p["rawTokenUsageRecord"]) {
            let mut row = exact.clone();
            row["noticeTotal"] = total;
            row["noticeLast"] = last;
            row["noticeAt"] = json!(q.at);
            if row["modelContextWindow"].is_null() {
                row["modelContextWindow"] = json!(number(&raw["modelContextWindow"]));
            }
            out.rows.push((CaptureTable::Usage, row));
            return;
        }
        existing = Some(exact.clone());
        id = text(&exact["id"]).into();
    }
    if let Some(mut row) = existing {
        if truthy(response)
            && (!truthy(&row["responseId"])
                || truthy(&p["rawTokenUsageRecord"]) && !truthy(&row["rawTokenUsageRecord"]))
        {
            let extra = json!({"responseId":response,"source":q.source.name(),"at":q.at,"recordedAt":q.recorded_at,"rawTokenUsageRecord":p["rawTokenUsageRecord"],"turnUsage":p["turnUsage"],"requestUsage":p["requestUsage"],"model":first(&[&p["model"],&meta["model"]]),"last":last,"total":total,"delta":token_numbers(&last),"counterDomain":domain(p),"cumulativeDelta":token_numbers(&Value::Null),"reset":null,"baselineMissing":true});
            row = merge(&row, extra);
            out.rows.push((CaptureTable::Usage, row));
            mark_generation(meta, None, out);
        }
        return;
    }
    if truthy(response) && rows.usage.is_none() && rows.exact_response.is_none() { /* key collision checked by the indexed response lookup in load */
    }
    let prior = rows.prior_usage.as_ref();
    let reset = prior.is_some_and(|r| {
        TOKEN_FIELDS.iter().any(|k| {
            number(&total[*k])
                .zip(number(&r["total"][*k]))
                .is_some_and(|(a, b)| a < b)
        })
    });
    let cumulative: serde_json::Map<String, Value> = TOKEN_FIELDS
        .iter()
        .map(|k| {
            (
                (*k).into(),
                json!(if reset {
                    None
                } else {
                    prior.and_then(|r| {
                        number(&total[*k])
                            .zip(number(&r["total"][*k]))
                            .map(|(a, b)| a - b)
                    })
                }),
            )
        })
        .collect();
    let row = merge(
        meta,
        json!({"id":id,"at":q.at,"recordedAt":q.recorded_at,"source":q.source.name(),"timestampSource":timestamp_source(q),"model":first(&[&p["model"],&meta["model"]]),"last":last,"total":total,"delta":token_numbers(&last),"raw":raw,"cumulativeDelta":cumulative,"counterDomain":domain(p),"modelContextWindow":number(&raw["modelContextWindow"]),"reset":reset,"baselineMissing":prior.is_none()||reset,"fingerprint":fingerprint(meta,p),"responseId":response,"rawTokenUsageRecord":p["rawTokenUsageRecord"],"turnUsage":p["turnUsage"],"requestUsage":p["requestUsage"]}),
    );
    out.rows.push((CaptureTable::Usage, row));
    mark_generation(meta, None, out);
}
fn mark_generation(meta: &Value, previous: Option<&Value>, out: &mut CaptureDecision) {
    let mut roots = vec![text(&meta["rootId"]).to_owned()];
    if let Some(old) = previous.and_then(|v| v["rootId"].as_str())
        && !roots.iter().any(|r| r == old)
    {
        roots.push(old.into())
    }
    out.generation_roots = roots;
}
fn response_content(v: &Value) -> Value {
    let mut value = v.clone();
    if let Some(m) = value.as_object_mut() {
        // Fingerprints describe content, not response identity. Keeping the
        // persisted row on semantic replay also keeps legacy Python hashes.
        for k in [
            "at",
            "recordedAt",
            "noticeTotal",
            "noticeLast",
            "noticeAt",
            "fingerprint",
        ] {
            m.remove(k);
        }
        if let Some(raw) = m.get_mut("raw").and_then(Value::as_object_mut) {
            raw.remove("modelContextWindow");
        }
    }
    value
}

pub(crate) fn truthy(v: &Value) -> bool {
    studio_runtime_store::truthy(Some(v))
}

/// The notification dispatcher can commit one bounded delta batch. Preparation
/// remains outside the transaction; each item observes prior items in order.
pub fn capture_batch(
    store: &RuntimeStore,
    prepared: &[PreparedCapture],
) -> Result<Vec<ResponseRateAssociation>, CaptureError> {
    if prepared.len() > 128 {
        return Err(CaptureError::Invalid("/capture/batch"));
    }
    store.analytics_transaction(|tx, canvas| {
        let mut effects = Vec::new();
        for prepared in prepared {
            check_identity(canvas, prepared.agent())?;
            let decision = decide(CaptureSnapshot::load(tx, prepared.clone())?)?;
            decision.apply(tx)?;
            if let Some(effect) = decision.response_rate {
                effects.push(effect)
            }
        }
        Ok(effects)
    })
}
/// Account-level rate-limit capture uses the same change-only series as events.
pub fn capture_limit(
    store: &RuntimeStore,
    account: &str,
    value: &Value,
    at: f64,
) -> Result<(), CaptureError> {
    if !at.is_finite()
        || at < 0.
        || account.is_empty()
        || value.to_string().len() > MAX_CAPTURE_BYTES
    {
        return Err(CaptureError::Invalid("/rateLimits"));
    }
    store.analytics_transaction(|tx, _| {
        let previous = Repo::last_limit(tx, &json!(account))?;
        if previous.as_ref().map(limit_content) != Some(limit_content(value)) {
            tx.execute(
                "INSERT INTO analytics_limits(account,at,record) VALUES (?,?,?)",
                params![account, at, value.to_string()],
            )?;
        }
        Ok(())
    })
}

// JSON integer/float representations compare numerically, as Python does.
fn same_content(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => {
            a == b
                || (a
                    .as_f64()
                    .is_some_and(|n| n.abs() <= 9_007_199_254_740_991.)
                    && b.as_f64()
                        .is_some_and(|n| n.abs() <= 9_007_199_254_740_991.)
                    && a.as_f64() == b.as_f64())
        }
        (Value::Array(a), Value::Array(b)) => {
            a.len() == b.len() && a.iter().zip(b).all(|(a, b)| same_content(a, b))
        }
        (Value::Object(a), Value::Object(b)) => {
            a.len() == b.len()
                && a.iter()
                    .all(|(k, a)| b.get(k).is_some_and(|b| same_content(a, b)))
        }
        _ => left == right,
    }
}
