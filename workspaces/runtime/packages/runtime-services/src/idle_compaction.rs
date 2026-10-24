//! Claude Code 2.1.302 idle compaction. Rust owns deadlines and durable fire receipts.
//! The host uses the ordinary native action owner, including its uncertain hand-off rules.
use crate::native_actions::{Action, Admission, NativeError};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};
use studio_runtime_schema::{
    idle_compaction::{Mode, Settings},
    shared_values::*,
};
use studio_runtime_store::{
    AgentId, AgentRepository, OrchestrationSettingsRepository, RuntimeStore, StoreError,
};
use tokio::sync::{Notify, watch};

pub const NOTICE: &str = "Compacted while idle, before the prompt cache expired. To turn it off, set Claude idle compaction to Off in Settings > Agent orchestration.";
pub const REQUEST_PREFIX: &str = "idle-compaction:";
const FAILURE_RETRY_SECONDS: u64 = 30;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CacheReport {
    pub request_id: String,
    pub ttl: String,
    pub warm: bool,
    /// Provider request activity time, in Unix milliseconds.
    pub last_activity_at: f64,
    pub context_tokens: u64,
    pub prefix: String,
    #[serde(default = "enabled")]
    pub compaction_enabled: bool,
    #[serde(default = "enabled")]
    pub idle_compaction_enabled: bool,
}
fn enabled() -> bool {
    true
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Episode {
    pub id: String,
    pub cache: CacheReport,
    /// Unix seconds, persisted so a new bridge cannot reset the deadline.
    pub fire_at: f64,
    pub expires_at: f64,
    pub identity: Value,
    pub prefix_inputs: Value,
    pub attempt: Value,
}

pub fn fire_at(activity: f64, expiry: f64, fraction: f64) -> f64 {
    let fraction = if fraction.is_finite() {
        fraction.clamp(
            IDLE_COMPACTION_MIN_FIRE_FRACTION,
            IDLE_COMPACTION_MAX_FIRE_FRACTION,
        )
    } else {
        IDLE_COMPACTION_FIRE_FRACTION
    };
    activity + (expiry - activity) * fraction
}

fn identity(agent: &Value) -> Value {
    json!({"accountKey": agent.get("accountKey").unwrap_or(&json!("default")), "threadId":agent["threadId"], "epoch":agent["epoch"]})
}
fn prefix_inputs(agent: &Value) -> Value {
    json!([
        agent["cwd"],
        agent["model"],
        agent["effort"],
        agent["fastMode"],
        agent["claude"],
        agent["claudeSettings"],
        agent["profileInstructions"],
        agent["developerInstructions"],
        agent["permissionMode"],
        agent["yoloMode"],
        agent["pendingSettings"],
        agent["capabilityProfile"],
        agent["dynamicTools"]
    ])
}

/// Called in the dedicated cache notification transaction. Only request-local
/// assistant usage can arm a timer; a cumulative SDK result cannot arm it.
pub fn observe(connection: &Connection, agent: &mut Value, raw: &Value) -> Result<(), StoreError> {
    if agent["provider"] != "claude" {
        return Ok(());
    }
    let cache: CacheReport = serde_json::from_value(raw.clone())
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
    if cache.request_id.is_empty()
        || cache.prefix.is_empty()
        || !cache.last_activity_at.is_finite()
        || cache.last_activity_at < 0.0
    {
        return Err(StoreError::SchemaMismatch(
            "invalid idle cache observation".into(),
        ));
    }
    agent["idleCache"] = json!(cache);
    let settings = OrchestrationSettingsRepository::idle_compaction(connection)?;
    if cache.ttl != "1h" || settings.mode == Mode::Off {
        if let Some(fields) = agent.as_object_mut() {
            fields.remove("idleCompactionEpisode");
        }
        return Ok(());
    }
    let activity = cache.last_activity_at / 1000.0;
    let expiry = activity + IDLE_COMPACTION_CACHE_SECONDS;
    let id = format!(
        "{REQUEST_PREFIX}{}",
        studio_runtime_store::sha256_hex(
            json!([agent["id"], identity(agent), cache.request_id])
                .to_string()
                .as_bytes()
        )
    );
    agent["idleCompactionEpisode"] = json!(Episode {
        id,
        cache,
        fire_at: fire_at(activity, expiry, IDLE_COMPACTION_FIRE_FRACTION),
        expires_at: expiry,
        identity: identity(agent),
        prefix_inputs: prefix_inputs(agent),
        attempt: agent["startAttempt"]["id"].clone(),
    });
    Ok(())
}

/// The pure Claude skip checks. `now` is a supplied clock, in Unix seconds.
pub fn refusal(
    settings: &Settings,
    episode: &Episode,
    agent: &Value,
    now: f64,
    allowed: bool,
    queued: bool,
) -> Option<&'static str> {
    if settings.mode == Mode::Off {
        return Some("disabled");
    }
    if !episode.cache.compaction_enabled {
        return Some("compaction_off");
    }
    if !episode.cache.idle_compaction_enabled {
        return Some("idle_compaction_off");
    }
    if prefix_inputs(agent) != episode.prefix_inputs || identity(agent) != episode.identity {
        return Some("prefix_changed");
    }
    if agent["idleCache"]["ttl"] != "1h" {
        return Some("not_one_hour");
    }
    if (now - episode.fire_at).max(0.0) > IDLE_COMPACTION_MAX_LATE_SECONDS
        || !episode.cache.warm
        || now >= episode.expires_at
    {
        return Some("lapsed");
    }
    if episode.cache.context_tokens < settings.min_tokens {
        return Some("small");
    }
    if agent["idleCache"]["requestId"] != episode.cache.request_id
        || agent["startAttempt"]["id"] != episode.attempt
    {
        return Some("newer_request");
    }
    if !allowed {
        return Some("near_limit");
    }
    if agent["inFlight"] == true
        || agent["turnId"].as_str().is_some_and(|id| !id.is_empty())
        || matches!(
            agent["status"].as_str(),
            Some("queued" | "starting" | "running" | "approval" | "compacting")
        )
    {
        return Some("active");
    }
    if queued {
        return Some("queued_input");
    }
    if agent["autoWake"] != true
        || studio_runtime_store::truthy(agent.get("deletedAt"))
        || studio_runtime_store::truthy(agent.get("nativeFailureHold"))
        || !matches!(agent["status"].as_str(), Some("idle" | "completed"))
    {
        return Some("stopped");
    }
    None
}

fn queued(connection: &Connection, id: &str) -> Result<bool, StoreError> {
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND status IN ('pending','reserved','dispatching','uncertain'))", [id], |row| row.get(0))?)
}
pub fn settled(connection: &Connection, id: &str) -> Result<bool, StoreError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts WHERE id=?1)",
        [id],
        |row| row.get(0),
    )?)
}
fn record(
    connection: &Connection,
    episode: &Episode,
    outcome: &str,
    now: f64,
) -> Result<(), StoreError> {
    connection.execute("INSERT OR IGNORE INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'idle-compaction',?2)", params![episode.id, json!({"outcome":outcome,"at":now,"fireAt":episode.fire_at,"contextTokens":episode.cache.context_tokens,"lateMs":((now-episode.fire_at).max(0.0)*1000.0)}).to_string()])?;
    eprintln!(
        "idle_compaction_fired request={} outcome={} contextTokens={} lateMs={}",
        episode.id,
        outcome,
        episode.cache.context_tokens,
        (now - episode.fire_at).max(0.0) * 1000.0
    );
    Ok(())
}

/// Rechecks eligibility under the same writer transaction as native admission.
/// The native receipt and this fire receipt commit together or neither commits.
pub struct IdleAdmission {
    pub inner: Arc<dyn Admission>,
    pub episode: Episode,
    pub limits: Value,
}
impl Admission for IdleAdmission {
    fn check(
        &self,
        tx: &Transaction<'_>,
        agent: &Value,
        action: Action,
    ) -> Result<(), NativeError> {
        let now = wall_now();
        if settled(tx, &self.episode.id)? {
            return Err(NativeError::Changed);
        }
        if let Some(reason) = check(tx, &self.episode, agent, &self.limits, now)? {
            return Err(NativeError::Invalid(reason));
        }
        if OrchestrationSettingsRepository::idle_compaction(tx)?.mode == Mode::Log {
            return Err(NativeError::Invalid("would_compact"));
        }
        self.inner.check(tx, agent, action)?;
        record(tx, &self.episode, "started", now)?;
        tx.execute("UPDATE runtime_operation_receipts SET result=json_set(result,'$.limits',json(?2),'$.before',json(?3)) WHERE id=?1", params![self.episode.id,self.limits.to_string(),json!({"status":agent["status"],"startAttempt":agent["startAttempt"],"turnEpoch":agent["turnEpoch"]}).to_string()])?;
        Ok(())
    }
}

/// Release only a proven unsubmitted idle reservation. An uncertain native
/// hand-off keeps its existing recovery owner and cannot be retried.
pub async fn release_unsubmitted(
    store: &RuntimeStore,
    actor: String,
    request_id: String,
    message: String,
) -> Result<(), NativeError> {
    store
        .transaction_with(move |tx| {
            let Some(record) = AgentRepository::get(tx, &AgentId::from(actor.as_str()))? else {
                return Ok(());
            };
            let mut value = record.value();
            if value["startAttempt"]["actionRequestId"] != request_id
                || value["autoWake"] != true
                || value["startAttempt"]["epoch"] != value["epoch"]
                || value["startAttempt"]["submitted"] != false
                || !matches!(
                    crate::native_actions::outcome(tx, &request_id)?,
                    Some(crate::native_actions::Outcome::Pending { .. })
                )
            {
                return Ok(());
            }
            let saved: String = tx.query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                [&request_id],
                |row| row.get(0),
            )?;
            let saved: Value = serde_json::from_str(&saved)?;
            crate::native_actions::fail_pending(tx, &request_id, &message)?;
            for field in ["status", "startAttempt", "turnEpoch"] {
                value[field] = saved["before"][field].clone();
            }
            value["inFlight"] = json!(false);
            studio_runtime_store::AgentRepository::put(
                tx,
                &studio_runtime_store::AgentRecord::from_value(value)?,
            )?;
            Ok(())
        })
        .await
}

/// Final writer boundary, after native preparation and before provider hand-off.
/// Ignore only the active reservation that this idle action itself owns.
pub fn handoff_refusal(
    tx: &Transaction<'_>,
    agent: &Value,
    request: &str,
) -> Result<Option<&'static str>, StoreError> {
    let episode: Episode = serde_json::from_value(agent["idleCompactionEpisode"].clone())
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
    if episode.id != request {
        return Ok(Some("newer_request"));
    }
    let raw: String = tx.query_row(
        "SELECT result FROM runtime_operation_receipts WHERE id=?1",
        [request],
        |row| row.get(0),
    )?;
    let receipt: Value = serde_json::from_str(&raw)
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
    let account = episode.identity["accountKey"].as_str().unwrap_or("default");
    let current: Option<String> = tx.query_row("SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0", [], |row| row.get(0)).optional()?;
    let current: Value = current
        .map(|raw| serde_json::from_str(&raw))
        .transpose()
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
        .unwrap_or(Value::Null);
    let limits = current
        .pointer("/value/rateLimitsByAccount")
        .and_then(|all| all.get(account))
        .unwrap_or(&receipt["limits"]);
    let mut idle = agent.clone();
    idle["startAttempt"]["id"] = episode.attempt.clone();
    idle["inFlight"] = json!(false);
    idle["status"] = json!("idle");
    let reason = check(tx, &episode, &idle, limits, wall_now())?;
    if reason.is_some() {
        return Ok(reason);
    }
    let settings = OrchestrationSettingsRepository::idle_compaction(tx)?;
    Ok((settings.mode == Mode::Log).then_some("would_compact"))
}

pub fn limits_allowed(limits: &Value, agent: &Value, now: f64) -> bool {
    // B2-6 owns freshness and model-specific window eligibility. A native
    // warning is stricter than ordinary admission: only `allowed` may compact.
    let status = limits
        .pointer("/data/claudeRateLimit/status")
        .and_then(Value::as_str);
    status.is_none_or(|status| status == "allowed")
        && crate::usage_resume::eligibility(limits, agent, now)["canRun"] == true
}
fn check(
    connection: &Connection,
    episode: &Episode,
    agent: &Value,
    limits: &Value,
    now: f64,
) -> Result<Option<&'static str>, StoreError> {
    let settings = OrchestrationSettingsRepository::idle_compaction(connection)?.with_environment();
    Ok(refusal(
        &settings,
        episode,
        agent,
        now,
        limits_allowed(limits, agent, now),
        queued(connection, agent["id"].as_str().unwrap_or_default())?,
    ))
}

pub type HostFuture<'a> = Pin<Box<dyn Future<Output = Result<(), String>> + Send + 'a>>;
pub trait Host: Send + Sync + 'static {
    fn compact<'a>(&'a self, agent: &'a str, episode: Episode, limits: Value) -> HostFuture<'a>;
}

/// Due timers are read from SQLite, never from a bridge's process lifetime.
pub async fn next(store: &RuntimeStore) -> Result<Option<(String, Episode)>, StoreError> {
    next_excluding(store, Vec::new()).await
}
async fn next_excluding(
    store: &RuntimeStore,
    running: Vec<String>,
) -> Result<Option<(String, Episode)>, StoreError> {
    store.read_async("runtime-services/idle-compaction/deadline", move |db| {
        let raw: Option<(String, String)> = db.query_row("SELECT a.id,json_extract(a.record,'$.idleCompactionEpisode') FROM runtime_agents a WHERE json_type(a.record,'$.idleCompactionEpisode')='object' AND json_extract(a.record,'$.idleCompactionEpisode.id') NOT IN (SELECT value FROM json_each(?1)) AND NOT EXISTS(SELECT 1 FROM runtime_operation_receipts r WHERE r.id=json_extract(a.record,'$.idleCompactionEpisode.id')) ORDER BY json_extract(a.record,'$.idleCompactionEpisode.fireAt'),a.id LIMIT 1", [json!(running).to_string()], |row| Ok((row.get(0)?, row.get(1)?))).optional()?;
        raw.map(|(agent, raw)| serde_json::from_str(&raw).map(|episode| (agent,episode)).map_err(|error| StoreError::SchemaMismatch(error.to_string()))).transpose()
    }).await
}

pub async fn fire(
    store: Arc<RuntimeStore>,
    agent_id: String,
    episode: Episode,
    limits: Value,
    now: f64,
    host: &dyn Host,
) -> Result<(), StoreError> {
    let id = agent_id.clone();
    let candidate = episode.clone();
    let observation = limits.clone();
    let start = store
        .transaction(move |tx| {
            if settled(tx, &candidate.id)? {
                return Ok(false);
            }
            let Some(agent) = AgentRepository::get(tx, &AgentId::from(id.as_str()))? else {
                record(tx, &candidate, "no_host", now)?;
                return Ok(false);
            };
            let agent = agent.value();
            let settings = OrchestrationSettingsRepository::idle_compaction(tx)?.with_environment();
            let reason = check(tx, &candidate, &agent, &observation, now)?;
            if let Some(reason) = reason {
                record(tx, &candidate, reason, now)?;
                return Ok(false);
            }
            if settings.mode == Mode::Log {
                record(tx, &candidate, "would_compact", now)?;
                return Ok(false);
            }
            Ok(true)
        })
        .await?;
    if start {
        let result = host.compact(&agent_id, episode.clone(), limits).await;
        let outcome = if result.is_ok() {
            "dispatched"
        } else {
            "failed"
        };
        if let Err(error) = result {
            eprintln!("idle compaction {}: {error}", episode.id);
        }
        store
            .transaction(move |tx| record(tx, &episode, outcome, now))
            .await?;
    }
    Ok(())
}

pub fn wall_now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

/// One server task, with one persisted deadline per session. Table observers
/// wake it when a new request replaces the previous session deadline.
pub async fn run(
    store: Arc<RuntimeStore>,
    port: Arc<dyn crate::account_session_runtime::AccountSessionPort>,
    host: Arc<dyn Host>,
    mut shutdown: watch::Receiver<bool>,
) {
    let wake = Arc::new(Notify::new());
    for table in [
        studio_runtime_store::ObservedTable::Agents,
        studio_runtime_store::ObservedTable::OrchestrationSettings,
    ] {
        let weak = Arc::downgrade(&wake);
        store.add_table_change_observer(table, move || {
            if let Some(wake) = weak.upgrade() {
                wake.notify_one();
            }
        });
    }
    let mut jobs: tokio::task::JoinSet<(String, Result<(), StoreError>)> =
        tokio::task::JoinSet::new();
    let mut running = std::collections::BTreeSet::new();
    loop {
        if *shutdown.borrow() {
            break;
        }
        let notified = wake.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        let candidate = match next_excluding(&store, running.iter().cloned().collect()).await {
            Ok(candidate) => candidate,
            Err(error) => {
                eprintln!("idle compaction deadline: {error}");
                tokio::select! { _ = shutdown.changed() => break, _ = tokio::time::sleep(Duration::from_secs(FAILURE_RETRY_SECONDS)) => {} }
                continue;
            }
        };
        let delay = candidate
            .as_ref()
            .map(|(_, episode)| Duration::from_secs_f64((episode.fire_at - wall_now()).max(0.0)));
        tokio::select! {
            _ = shutdown.changed() => break,
            completed = jobs.join_next(), if !jobs.is_empty() => {
                if let Some(Ok((id, result))) = completed {
                    running.remove(&id);
                    if let Err(error) = result { eprintln!("idle compaction fire: {error}"); }
                }
                continue;
            }
            _ = &mut notified => continue,
            _ = async { match delay { Some(delay) => tokio::time::sleep(delay).await, None => std::future::pending().await } } => {}
        }
        if let Some((agent, episode)) = candidate {
            running.insert(episode.id.clone());
            let store = store.clone();
            let port = port.clone();
            let host = host.clone();
            jobs.spawn(async move {
                let id = episode.id.clone();
                let account = episode.identity["accountKey"].as_str().unwrap_or("default");
                let limits = port
                    .read_limits_for_resume(account, wall_now())
                    .await
                    .unwrap_or(Value::Null);
                let result = fire(store, agent, episode, limits, wall_now(), host.as_ref()).await;
                (id, result)
            });
        }
    }
    // Native hand-off commits Unknown before transport. Cancellation cannot
    // cause a second dispatch after the server starts again.
    jobs.shutdown().await;
}

#[cfg(test)]
mod tests {
    use super::*;
    struct Clock(f64);
    impl Clock {
        fn advance(&mut self, seconds: f64) {
            self.0 += seconds;
        }
    }
    fn fixture() -> (Settings, Episode, Value, Clock) {
        let settings = Settings {
            mode: Mode::Compact,
            min_tokens: IDLE_COMPACTION_DEFAULT_MIN_TOKENS,
        };
        let agent = json!({"id":"agent","provider":"claude","epoch":1,"accountKey":"default","threadId":"thread","status":"idle","autoWake":true,"inFlight":false,"startAttempt":{"id":"request"},"idleCache":{"ttl":"1h","requestId":"response"}});
        let episode = Episode {
            id: "idle-compaction:episode".into(),
            cache: CacheReport {
                request_id: "response".into(),
                ttl: "1h".into(),
                warm: true,
                last_activity_at: 1000.0,
                context_tokens: IDLE_COMPACTION_DEFAULT_MIN_TOKENS,
                prefix: "prefix".into(),
                compaction_enabled: true,
                idle_compaction_enabled: true,
            },
            fire_at: 3241.0,
            expires_at: 3601.0,
            identity: identity(&agent),
            prefix_inputs: prefix_inputs(&agent),
            attempt: json!("request"),
        };
        (settings, episode, agent, Clock(3241.0))
    }
    #[test]
    fn fire_time_uses_the_cache_interval_and_clamps_the_fraction() {
        assert_eq!(
            fire_at(100.0, 3700.0, IDLE_COMPACTION_FIRE_FRACTION),
            3340.0
        );
        assert_eq!(fire_at(100.0, 3700.0, -1.0), 1900.0);
        assert_eq!(fire_at(100.0, 3700.0, 2.0), 3520.0);
        assert_eq!(fire_at(100.0, 3700.0, f64::NAN), 3340.0);
    }
    #[test]
    fn every_claude_skip_reason_and_studio_idle_boundary() {
        let (settings, episode, agent, clock) = fixture();
        assert_eq!(
            refusal(&settings, &episode, &agent, clock.0, true, false),
            None
        );
        for reason in [
            "disabled",
            "compaction_off",
            "idle_compaction_off",
            "prefix_changed",
            "not_one_hour",
            "lapsed",
            "small",
            "newer_request",
            "near_limit",
            "active",
            "queued_input",
            "stopped",
        ] {
            let (mut s, mut e, mut a, mut c) = fixture();
            let mut allowed = true;
            let mut queued = false;
            match reason {
                "disabled" => s.mode = Mode::Off,
                "compaction_off" => e.cache.compaction_enabled = false,
                "idle_compaction_off" => e.cache.idle_compaction_enabled = false,
                "prefix_changed" => a["model"] = json!("changed"),
                "not_one_hour" => a["idleCache"]["ttl"] = json!("5m"),
                "lapsed" => c.advance(60.001),
                "small" => e.cache.context_tokens = s.min_tokens - 1,
                "newer_request" => a["idleCache"]["requestId"] = json!("new"),
                "near_limit" => allowed = false,
                "active" => a["inFlight"] = json!(true),
                "queued_input" => queued = true,
                "stopped" => a["autoWake"] = json!(false),
                _ => unreachable!(),
            }
            assert_eq!(
                refusal(&s, &e, &a, c.0, allowed, queued),
                Some(reason),
                "{reason}"
            );
        }
    }
    #[test]
    fn late_boundary_cache_cold_and_new_turn_are_checked_with_the_clock() {
        let (settings, mut episode, mut agent, mut clock) = fixture();
        clock.advance(60.0);
        assert_eq!(
            refusal(&settings, &episode, &agent, clock.0, true, false),
            None
        );
        episode.cache.warm = false;
        assert_eq!(
            refusal(&settings, &episode, &agent, clock.0, true, false),
            Some("lapsed")
        );
        episode.cache.warm = true;
        episode.expires_at = clock.0;
        assert_eq!(
            refusal(&settings, &episode, &agent, clock.0, true, false),
            Some("lapsed")
        );
        episode.expires_at += 1.0;
        agent["startAttempt"]["id"] = json!("new-turn");
        assert_eq!(
            refusal(&settings, &episode, &agent, clock.0, true, false),
            Some("newer_request")
        );
    }
}
