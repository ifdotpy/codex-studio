//! Pure analytics capture decisions and the synchronous notification consumer.
//!
//! Prepared payload measurements happen before the writer is held. A checked
//! snapshot selects only the affected rows; no unbounded history is loaded.
use crate::analytics_measurements::{item_sizes, payload_size};
use crate::notification_dispatch::NotificationStreamMetrics;
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_schema::shared_values::MAX_SAFE_INTEGER;
use studio_runtime_store::CachedSql;
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
    agent: Arc<Value>,
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
        Self::event_validated(
            Arc::new(agent.clone()),
            method,
            params,
            at,
            recorded_at,
            source,
        )
    }

    pub(crate) fn notification_event(
        agent: &Value,
        method: &str,
        params: &Value,
        at: f64,
        recorded_at: f64,
    ) -> Result<Self, CaptureError> {
        Self::event_validated(
            Arc::new(agent.clone()),
            method,
            params,
            at,
            recorded_at,
            CaptureSource::Live,
        )
    }

    pub(crate) fn event_shared(
        agent: Arc<Value>,
        method: &str,
        params: &Value,
        at: f64,
        recorded_at: f64,
        source: CaptureSource,
    ) -> Result<Self, CaptureError> {
        CaptureTable::Agents.validate(&agent)?;
        Self::event_validated(agent, method, params, at, recorded_at, source)
    }

    fn event_validated(
        agent: Arc<Value>,
        method: &str,
        params: &Value,
        at: f64,
        recorded_at: f64,
        source: CaptureSource,
    ) -> Result<Self, CaptureError> {
        if !params.is_object() {
            return Err(CaptureError::Invalid("/params"));
        }
        if !at.is_finite() || at < 0. || !recorded_at.is_finite() || recorded_at < 0. {
            return Err(CaptureError::Invalid("/at"));
        }
        if encoded_len(params) > MAX_CAPTURE_BYTES {
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
            agent,
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
        Self::model_payload_shared(Arc::new(agent.clone()), params, at, recorded_at)
    }
    pub(crate) fn model_payload_shared(
        agent: Arc<Value>,
        params: &Value,
        at: f64,
        recorded_at: f64,
    ) -> Result<Self, CaptureError> {
        let mut result = Self::event_shared(
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
            if !studio_runtime_store::truthy(Some(&call)) {
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
            studio_runtime_store::truthy(Some(&id)).then(|| key(meta, &id))
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
    prepared: std::sync::Arc<PreparedCapture>,
    meta: Value,
    rows: CaptureRows,
}
impl CaptureSnapshot {
    fn from_records(
        prepared: std::sync::Arc<PreparedCapture>,
        rows: CaptureRows,
    ) -> Result<Self, CaptureError> {
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
        // Python keys items by agent, thread and item id (`analytics_event`):
        // a command started in one turn still receives terminal input and
        // output deltas in later turns. Only the turn row is turn-scoped.
        if let Some(row) = &rows.turn
            && row["turnId"] != meta["turnId"]
        {
            return Err(CaptureError::Invalid("/snapshot/row/turnId"));
        }
        Ok(Self {
            prepared,
            meta,
            rows,
        })
    }
    pub fn load(
        db: &Connection,
        prepared: std::sync::Arc<PreparedCapture>,
    ) -> Result<Self, CaptureError> {
        let meta = prepared.meta();
        let turn = studio_runtime_store::truthy(Some(&meta["turnId"]))
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
            if studio_runtime_store::truthy(Some(response)) {
                rows.exact_response =
                    Repo::usage_response(db, text(&meta["agentId"]), &meta["threadId"], response)?;
            }
            if rows.exact_response.is_none()
                && rows.usage.as_ref().is_some_and(|v| {
                    studio_runtime_store::truthy(Some(response))
                        && studio_runtime_store::truthy(Some(&v["responseId"]))
                        && v["responseId"] != *response
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
            tx.execute_cached("INSERT INTO analytics_notifications VALUES (?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET count=count+excluded.count,bytes=bytes+excluded.bytes",params![n["id"].as_str(),n["agent"].as_str(),n["root"].as_str(),n["method"].as_str(),n["hour"].as_i64(),n["count"].as_i64().unwrap_or(1),n["bytes"].as_i64()])?;
        }
        if let Some(n) = &self.limit {
            tx.execute_cached(
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
pub async fn capture(
    store: &RuntimeStore,
    prepared: PreparedCapture,
) -> Result<CaptureDecision, CaptureError> {
    store
        .analytics_transaction(move |tx, canvas| {
            check_identity(canvas, prepared.agent())?;
            let decision = decide(CaptureSnapshot::load(tx, std::sync::Arc::new(prepared))?)?;
            decision.apply(tx)?;
            Ok(decision)
        })
        .await
}

/// Apply one logged `Analytics` or `AnalyticsStream` effect inside the
/// projector's analytics transaction. The journal commit checked its agent
/// identity when it logged the effect, so no Canvas read happens here.
pub(crate) fn capture_logged_effect(
    tx: &Transaction<'_>,
    effect: &crate::notification_dispatch::NotificationPostCommit,
) -> Result<(), CaptureError> {
    use crate::notification_dispatch::NotificationPostCommit as Effect;
    let (observation, metrics) = match effect {
        Effect::Analytics { observation, .. } => (observation, None),
        Effect::AnalyticsStream {
            observation,
            metrics,
        } => (observation, Some(metrics)),
        _ => return Err(CaptureError::Invalid("/effect")),
    };
    let agent = observation
        .agent
        .as_ref()
        .ok_or(CaptureError::Invalid("/agent"))?;
    let prepared = std::sync::Arc::new(PreparedCapture::notification_event(
        agent,
        &observation.method,
        &observation.params,
        observation.observed_at,
        observation.observed_at,
    )?);
    let mut decision = decide(CaptureSnapshot::load(tx, std::sync::Arc::clone(&prepared))?)?;
    if let Some(metrics) = metrics {
        apply_stream_metrics(&mut decision, &prepared, metrics);
    }
    decision.apply(tx)?;
    Ok(())
}

/// Add `count` failures to the `captureErrors` coverage metadata that the
/// insights API reports, with the latest failure's operation, code and message.
pub(crate) fn record_capture_errors(
    tx: &Transaction<'_>,
    count: i64,
    operation: &str,
    code: &str,
    error: &str,
) -> Result<(), CaptureError> {
    tx.execute_cached(
        "INSERT INTO analytics_meta(key,value) VALUES ('captureErrors',json_object('count',?1,'last',json_object('at',CAST(strftime('%s','now') AS INTEGER),'operation',?2,'code',?3,'error',?4))) ON CONFLICT(key) DO UPDATE SET value=json_set(analytics_meta.value,'$.count',COALESCE(json_extract(analytics_meta.value,'$.count'),0)+?1,'$.last',json_object('at',CAST(strftime('%s','now') AS INTEGER),'operation',?2,'code',?3,'error',?4))",
        params![count, operation, code, error],
    )?;
    Ok(())
}

/// Retain the legacy process-scoped receipt tables while volatile token-rate
/// publication is owned by `TokenRateHub`.
pub async fn start_token_rate_process(
    store: &RuntimeStore,
    process_id: String,
) -> Result<(), CaptureError> {
    if process_id.is_empty() || process_id.len() > 128 {
        return Err(CaptureError::Invalid("/processId"));
    }
    store.analytics_transaction(move |tx, _| {
        tx.execute_batch(
            "CREATE TABLE IF NOT EXISTS token_rate_receipt_process (id INTEGER PRIMARY KEY CHECK(id=1),process_id TEXT NOT NULL);\
             CREATE TABLE IF NOT EXISTS token_rate_publication_receipts (process_id TEXT NOT NULL,provider TEXT NOT NULL,generation INTEGER NOT NULL,cursor INTEGER NOT NULL,effects INTEGER NOT NULL,PRIMARY KEY(process_id,provider));",
        )?;
        let current: Option<String> = tx
            .query_row_cached(
                "SELECT process_id FROM token_rate_receipt_process WHERE id=1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if current.as_deref() != Some(process_id.as_str()) {
            tx.execute_cached("DELETE FROM token_rate_publication_receipts", [])?;
            tx.execute_cached(
                "INSERT INTO token_rate_receipt_process(id,process_id) VALUES (1,?1) ON CONFLICT(id) DO UPDATE SET process_id=excluded.process_id",
                [process_id.as_str()],
            )?;
        }
        Ok(())
    })
    .await
}

/// Preserve the lossy queue's overflow count in analytics coverage metadata.
/// Python's Runtime.schedule_analytics_captures records queueOverflow instead
/// of failing the already committed provider notification.
pub async fn record_publication_overflow(
    store: &RuntimeStore,
    receipt_id: String,
    analytics_overflow: u64,
    publication_overflow: u64,
) -> Result<(), CaptureError> {
    let dropped = analytics_overflow.saturating_add(publication_overflow);
    if dropped == 0 {
        return Ok(());
    }
    if receipt_id.is_empty() || receipt_id.len() > 1024 {
        return Err(CaptureError::Invalid("/receiptId"));
    }
    store.analytics_transaction(move |tx, _| {
        tx.execute_cached(
            "CREATE TABLE IF NOT EXISTS analytics_publication_overflow_receipts (receipt_id TEXT PRIMARY KEY,analytics_overflow INTEGER NOT NULL,publication_overflow INTEGER NOT NULL)",
            [],
        )?;
        let prior: Option<(i64, i64)> = tx
            .query_row_cached(
                "SELECT analytics_overflow,publication_overflow FROM analytics_publication_overflow_receipts WHERE receipt_id=?1",
                [receipt_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let analytics_overflow = analytics_overflow.min(i64::MAX as u64) as i64;
        let publication_overflow = publication_overflow.min(i64::MAX as u64) as i64;
        let additional_analytics = analytics_overflow.saturating_sub(prior.map_or(0, |row| row.0));
        let additional_publications =
            publication_overflow.saturating_sub(prior.map_or(0, |row| row.1));
        let additional = additional_analytics.saturating_add(additional_publications);
        if additional == 0 {
            return Ok(());
        }
        tx.execute_cached(
            "INSERT INTO analytics_publication_overflow_receipts(receipt_id,analytics_overflow,publication_overflow) VALUES (?1,?2,?3) ON CONFLICT(receipt_id) DO UPDATE SET analytics_overflow=MAX(analytics_publication_overflow_receipts.analytics_overflow,excluded.analytics_overflow),publication_overflow=MAX(analytics_publication_overflow_receipts.publication_overflow,excluded.publication_overflow)",
            params![receipt_id.as_str(), analytics_overflow, publication_overflow],
        )?;
        record_capture_errors(
            tx,
            additional,
            "analytics_capture_queue",
            "queueOverflow",
            "Publication queue limit reached",
        )?;
        Ok(())
    })
    .await
}

fn apply_stream_metrics(
    decision: &mut CaptureDecision,
    prepared: &PreparedCapture,
    metrics: &NotificationStreamMetrics,
) {
    let meta = prepared.meta();
    decision.notifications = metrics
        .hours
        .iter()
        .map(|hour| {
            json!({
                "id": python_json_compact(&json!([meta["agentId"], prepared.method, hour.hour])),
                "agent": meta["agentId"],
                "root": meta["rootId"],
                "method": prepared.method,
                "hour": hour.hour,
                "count": hour.count,
                "bytes": hour.encoded_bytes,
            })
        })
        .collect();

    let synthetic_delta = prepared.params["delta"].as_str().unwrap_or_default();
    let total_deltas = metrics.hours.iter().map(|hour| hour.count).sum::<u64>();
    let extra = [
        total_deltas.saturating_sub(1),
        metrics.bytes.saturating_sub(synthetic_delta.len() as u64),
        metrics
            .characters
            .saturating_sub(synthetic_delta.chars().count() as u64),
        metrics.lines.saturating_sub(
            synthetic_delta
                .bytes()
                .filter(|byte| *byte == b'\n')
                .count() as u64,
        ),
    ];
    for (table, row) in &mut decision.rows {
        if *table != CaptureTable::Items || !row["stream"].is_object() {
            continue;
        }
        for (field, addition) in ["deltas", "bytes", "chars", "lines"].into_iter().zip(extra) {
            row["stream"][field] = json!(
                row["stream"][field]
                    .as_u64()
                    .unwrap_or_default()
                    .saturating_add(addition)
            );
        }
    }
}

/// The compact JSON length of `value` in bytes, counted without building the text.
fn encoded_len(value: &Value) -> usize {
    struct Count(usize);
    impl std::io::Write for Count {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0 += bytes.len();
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value).expect("JSON values serialize");
    count.0
}

/// The identity fields of a stored agent that a capture must still match.
/// A missing field stays `None`; a present `null` is `Some(Value::Null)`.
#[derive(serde::Deserialize)]
struct CaptureIdentity {
    #[serde(rename = "threadId", default, deserialize_with = "present")]
    thread_id: Option<Value>,
    #[serde(rename = "accountKey", default, deserialize_with = "present")]
    account_key: Option<Value>,
    #[serde(default, deserialize_with = "present")]
    epoch: Option<Value>,
}

fn present<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<Option<Value>, D::Error> {
    <Value as serde::Deserialize>::deserialize(deserializer).map(Some)
}

pub(crate) fn check_identity(canvas: &Connection, agent: &Value) -> Result<(), CaptureError> {
    let raw: Option<String> = canvas
        .query_row_cached(
            "SELECT record FROM runtime_agents WHERE id=?",
            [agent["id"].as_str()],
            |r| r.get(0),
        )
        .optional()?;
    let current: CaptureIdentity = raw
        .map(|r| serde_json::from_str(&r).map_err(StoreError::from))
        .transpose()?
        .ok_or(CaptureError::StaleIdentity)?;
    if current.thread_id.as_ref() != agent.get("threadId")
        || current.account_key.as_ref() != agent.get("accountKey")
        || current.epoch.as_ref() != agent.get("epoch")
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
            json!({"id":python_json_compact(&json!([q.agent["id"],method,hour])),"agent":q.agent["id"],"root":meta["rootId"],"method":method,"hour":hour,"count":1,"bytes":python_json_compact(p).len()}),
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
    if studio_runtime_store::truthy(Some(&meta["turnId"]))
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
                let status = if studio_runtime_store::truthy(Some(error)) {
                    json!("failed")
                } else {
                    p["turn"].get("status").cloned().unwrap_or(json!("unknown"))
                };
                let previous =
                    studio_runtime_store::truthy(Some(&row["error"])) || row["status"] == "failed";
                let incoming = studio_runtime_store::truthy(Some(error)) || status == "failed";
                if !previous
                    || incoming
                        && (!studio_runtime_store::truthy(Some(&row["error"]))
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
                if (method == "item/agentMessage/delta"
                    && studio_runtime_store::truthy(Some(&p["delta"]))
                    || p["item"]["type"] == "agentMessage"
                        && studio_runtime_store::truthy(Some(&p["item"]["text"])))
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
        let mut params = q.params.clone();
        params["item"] = json!({"id":first(&[&p["id"],&p["_analyticsId"],&json!(at.to_string())]),"type":"compactionSnapshot","compactionMetadata":p});
        let next = PreparedCapture {
            agent: std::sync::Arc::clone(&q.agent),
            method: "item/completed".into(),
            params,
            at: q.at,
            recorded_at: q.recorded_at,
            source: q.source,
            input: q.input.clone(),
            output: q.output.clone(),
            stream: q.stream.clone(),
            model: q.model,
        };
        let recursive = decide(CaptureSnapshot::from_records(
            std::sync::Arc::new(next),
            rows,
        )?)?;
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
                || studio_runtime_store::truthy(Some(&item["error"]))
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
            if studio_runtime_store::truthy(Some(&item["compactionMetadata"])) {
                row["compactionMetadata"] = item["compactionMetadata"].clone();
            }
            if studio_runtime_store::truthy(Some(&item["error"])) {
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
    if studio_runtime_store::truthy(Some(&p["name"])) {
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
        .find(|v| studio_runtime_store::truthy(Some(v)))
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
    if studio_runtime_store::truthy(Some(&p["rawTokenUsageRecord"])) {
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
    if q.source == CaptureSource::Rollout && studio_runtime_store::truthy(Some(response)) {
        let usage = p
            .get("requestUsage")
            .filter(|v| studio_runtime_store::truthy(Some(v)))
            .unwrap_or(&last);
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
            studio_runtime_store::truthy(Some(response))
                && studio_runtime_store::truthy(Some(&r["responseId"]))
                && r["responseId"] != *response
        })
    {
        id.push(':');
        id.push_str(text(response));
        existing = rows.collision_usage.clone();
    }
    if p["usageSource"] == "claudeResponse" {
        if !studio_runtime_store::truthy(Some(response)) || !p["requestUsage"].is_object() {
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
        if !studio_runtime_store::truthy(Some(&p["rawTokenUsageRecord"])) {
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
        if studio_runtime_store::truthy(Some(response))
            && (!studio_runtime_store::truthy(Some(&row["responseId"]))
                || studio_runtime_store::truthy(Some(&p["rawTokenUsageRecord"]))
                    && !studio_runtime_store::truthy(Some(&row["rawTokenUsageRecord"])))
        {
            let extra = json!({"responseId":response,"source":q.source.name(),"at":q.at,"recordedAt":q.recorded_at,"rawTokenUsageRecord":p["rawTokenUsageRecord"],"turnUsage":p["turnUsage"],"requestUsage":p["requestUsage"],"model":first(&[&p["model"],&meta["model"]]),"last":last,"total":total,"delta":token_numbers(&last),"counterDomain":domain(p),"cumulativeDelta":token_numbers(&Value::Null),"reset":null,"baselineMissing":true});
            row = merge(&row, extra);
            out.rows.push((CaptureTable::Usage, row));
            mark_generation(meta, None, out);
        }
        return;
    }
    if studio_runtime_store::truthy(Some(response))
        && rows.usage.is_none()
        && rows.exact_response.is_none()
    { /* key collision checked by the indexed response lookup in load */
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

/// The notification dispatcher can commit one bounded delta batch. Preparation
/// remains outside the transaction; each item observes prior items in order.
pub async fn capture_batch(
    store: &RuntimeStore,
    prepared: Vec<PreparedCapture>,
) -> Result<Vec<ResponseRateAssociation>, CaptureError> {
    if prepared.len() > 128 {
        return Err(CaptureError::Invalid("/capture/batch"));
    }
    store
        .analytics_transaction(move |tx, canvas| {
            let mut effects = Vec::new();
            for prepared in prepared {
                check_identity(canvas, prepared.agent())?;
                let decision = decide(CaptureSnapshot::load(tx, std::sync::Arc::new(prepared))?)?;
                decision.apply(tx)?;
                if let Some(effect) = decision.response_rate {
                    effects.push(effect)
                }
            }
            Ok(effects)
        })
        .await
}
/// Account-level rate-limit capture uses the same change-only series as events.
pub async fn capture_limit(
    store: &RuntimeStore,
    account: String,
    value: Value,
    at: f64,
) -> Result<(), CaptureError> {
    if !at.is_finite() || at < 0. || account.is_empty() || encoded_len(&value) > MAX_CAPTURE_BYTES {
        return Err(CaptureError::Invalid("/rateLimits"));
    }
    store
        .analytics_transaction(move |tx, _| {
            let previous = Repo::last_limit(tx, &json!(account))?;
            if previous.as_ref().map(limit_content) != Some(limit_content(&value)) {
                tx.execute_cached(
                    "INSERT INTO analytics_limits(account,at,record) VALUES (?,?,?)",
                    params![account, at, value.to_string()],
                )?;
            }
            Ok(())
        })
        .await
}

// JSON integer/float representations compare numerically, as Python does.
fn same_content(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Number(a), Value::Number(b)) => {
            a == b
                || (a
                    .as_f64()
                    .is_some_and(|n| n.abs() <= MAX_SAFE_INTEGER as f64)
                    && b.as_f64()
                        .is_some_and(|n| n.abs() <= MAX_SAFE_INTEGER as f64)
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
