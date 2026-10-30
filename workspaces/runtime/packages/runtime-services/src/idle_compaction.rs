//! Claude Code 2.1.302 idle compaction. Rust owns deadlines and durable fire receipts.
//! The host uses the ordinary native action owner, including its uncertain hand-off rules.
use crate::native_actions::{Action, Admission, NativeError};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::BTreeMap, future::Future, pin::Pin, sync::Arc, time::Duration};
use studio_runtime_providers::ProviderKind;
use studio_runtime_schema::persistence::{IdleEpisodeOutcome, RequestIdentity};
use studio_runtime_schema::{
    idle_compaction::{Mode, Settings},
    shared_values::*,
};
use studio_runtime_store::{
    AgentId, AgentRepository, OrchestrationSettingsRepository, RuntimeStore, StoreError,
};
use tokio::sync::{Notify, watch};

pub const NOTICE: &str = "Compacted while idle, before the prompt cache expired. To turn it off, set Claude idle compaction to Off in Settings > Agent orchestration.";
pub const ON_RETURN_NOTICE: &str = "Compacted on return with a summary prepared while idle. To turn it off, set Claude idle compaction to Off in Settings > Agent orchestration.";
const FAILURE_RETRY_SECONDS: u64 = 30;

pub use studio_runtime_schema::idle_compaction::CacheReport;

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
    #[serde(default)]
    pub fired: bool,
    #[serde(default)]
    pub outcome: Option<IdleEpisodeOutcome>,
    #[serde(default)]
    pub fired_at: Option<f64>,
    #[serde(default)]
    pub before: Option<Value>,
    #[serde(default)]
    pub limits: Option<Value>,
    /// Actual account-worker unload deadline, in Unix seconds.
    #[serde(default)]
    pub unload_at: Option<f64>,
}

fn cache_seconds(ttl: &str) -> Option<f64> {
    match ttl {
        "1h" => Some(IDLE_COMPACTION_CACHE_SECONDS),
        "5m" => Some(IDLE_COMPACTION_SHORT_CACHE_SECONDS),
        _ => None,
    }
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
pub fn observe(
    connection: &Connection,
    agent: &mut Value,
    raw: &Value,
    previous: Option<&Episode>,
) -> Result<(), StoreError> {
    if agent["provider"] != ProviderKind::Claude.key() {
        return Ok(());
    }
    let cache: CacheReport = CacheReport::deserialize(raw)
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
    if let Some(previous) = previous
        && previous.cache.request_id == cache.request_id
        && previous.cache.ttl == cache.ttl
        && previous.identity == identity(agent)
    {
        return Ok(());
    }
    if let Some(fields) = agent.as_object_mut() {
        fields.remove("idleSummaryPrepared");
    }
    let settings = OrchestrationSettingsRepository::idle_compaction_for(connection, agent)?;
    let lifetime = cache_seconds(&cache.ttl);
    if lifetime.is_none() || settings.mode == Mode::Off {
        if let Some(fields) = agent.as_object_mut() {
            fields.remove("idleCompactionEpisode");
        }
        return Ok(());
    }
    let activity = cache.last_activity_at / 1000.0;
    let expiry = activity + lifetime.unwrap_or_default();
    let id = RequestIdentity::issue_root();
    agent["idleCompactionEpisode"] = json!(Episode {
        id,
        cache,
        fire_at: fire_at(activity, expiry, IDLE_COMPACTION_FIRE_FRACTION),
        expires_at: expiry,
        identity: identity(agent),
        prefix_inputs: prefix_inputs(agent),
        attempt: agent["startAttempt"]["id"].clone(),
        fired: false,
        outcome: None,
        fired_at: None,
        before: None,
        limits: None,
        unload_at: None,
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
) -> Option<IdleEpisodeOutcome> {
    if settings.mode == Mode::Off {
        return Some(IdleEpisodeOutcome::Disabled);
    }
    if !episode.cache.compaction_enabled {
        return Some(IdleEpisodeOutcome::CompactionOff);
    }
    if !episode.cache.idle_compaction_enabled {
        return Some(IdleEpisodeOutcome::IdleCompactionOff);
    }
    if prefix_inputs(agent) != episode.prefix_inputs || identity(agent) != episode.identity {
        return Some(IdleEpisodeOutcome::PrefixChanged);
    }
    if cache_seconds(&episode.cache.ttl).is_none() || agent["idleCache"]["ttl"] != episode.cache.ttl
    {
        return Some(IdleEpisodeOutcome::TtlChanged);
    }
    if (now - episode.fire_at).max(0.0) > IDLE_COMPACTION_MAX_LATE_SECONDS
        || !episode.cache.warm
        || now >= episode.expires_at
    {
        return Some(IdleEpisodeOutcome::Lapsed);
    }
    if episode.cache.context_tokens < settings.min_tokens {
        return Some(IdleEpisodeOutcome::Small);
    }
    if agent["idleCache"]["requestId"] != episode.cache.request_id
        || agent["startAttempt"]["id"] != episode.attempt
    {
        return Some(IdleEpisodeOutcome::NewerRequest);
    }
    if !allowed {
        return Some(IdleEpisodeOutcome::NearLimit);
    }
    if agent["inFlight"] == true
        || agent["turnId"].as_str().is_some_and(|id| !id.is_empty())
        || matches!(
            agent["status"].as_str(),
            Some("queued" | "starting" | "running" | "approval" | "compacting")
        )
    {
        return Some(IdleEpisodeOutcome::Active);
    }
    if queued {
        return Some(IdleEpisodeOutcome::QueuedInput);
    }
    if agent["autoWake"] != true
        || studio_runtime_store::truthy(agent.get("deletedAt"))
        || studio_runtime_store::truthy(agent.get("nativeFailureHold"))
        || !matches!(agent["status"].as_str(), Some("idle" | "completed"))
    {
        return Some(IdleEpisodeOutcome::Stopped);
    }
    None
}

/// The account worker owns these monotonic unload clocks. Publish their wall
/// equivalents without resetting a receipt or creating another episode.
pub fn sync_unload_deadlines(
    connection: &Connection,
    account: &str,
    deadlines: &BTreeMap<String, f64>,
    now: f64,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare("SELECT id,record FROM runtime_agents INDEXED BY runtime_idle_compaction_deadline WHERE idle_episode_type='object' AND provider='claude' AND account_key=?1 AND coalesce(json_extract(record,'$.accountKey'),'default')=?1")?;
    let records = statement
        .query_map([account], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, raw) in records {
        let mut agent: Value = serde_json::from_str(&raw)?;
        let mut episode: Episode = Episode::deserialize(&agent["idleCompactionEpisode"])
            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
        if settled(connection, &episode.id)? {
            continue;
        }
        let unload_at = agent["threadId"]
            .as_str()
            .and_then(|thread| deadlines.get(thread))
            .copied();
        if episode.unload_at == unload_at {
            continue;
        }
        episode.unload_at = unload_at;
        let cache_fire = fire_at(
            episode.cache.last_activity_at / 1000.0,
            episode.expires_at,
            IDLE_COMPACTION_FIRE_FRACTION,
        );
        episode.fire_at = unload_at.map_or(cache_fire, |at| {
            cache_fire.min((at - IDLE_COMPACTION_UNLOAD_LEAD_SECONDS).max(now))
        });
        agent["idleCompactionEpisode"] = json!(episode);
        AgentRepository::put(
            connection,
            &studio_runtime_store::AgentRecord::from_value(agent)
                .map_err(|error| StoreError::SchemaMismatch(format!("{id}: {error}")))?,
        )?;
    }
    Ok(())
}

/// An immediate unload waits for the same timer admission as ordinary sleep
/// compaction. Its fired flag settles refusals too, so this cannot retry a query.
pub fn pending_before_unload(
    connection: &Connection,
    account: &str,
    thread: &str,
    now: f64,
) -> Result<bool, StoreError> {
    if OrchestrationSettingsRepository::idle_compaction(connection)?.mode == Mode::Off {
        return Ok(false);
    }
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_agents a WHERE json_extract(a.record,'$.provider')='claude' AND coalesce(json_extract(a.record,'$.accountKey'),'default')=?1 AND json_extract(a.record,'$.threadId')=?2 AND json_extract(a.record,'$.idleCompactionEpisode.fireAt')<=?3 AND COALESCE(json_extract(a.record,'$.idleCompactionEpisode.fired'),0)=0)", params![account,thread,now], |row| row.get(0))?)
}

fn queued(connection: &Connection, id: &str) -> Result<bool, StoreError> {
    Ok(connection.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND status IN ('pending','reserved','dispatching','uncertain'))", [id], |row| row.get(0))?)
}
pub fn settled(connection: &Connection, id: &str) -> Result<bool, StoreError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE idle_episode_type='object' AND idle_episode_id=?1 AND json_extract(record,'$.idleCompactionEpisode.fired')=1)",
        [id], |row| row.get(0),
    )?)
}
fn record(
    connection: &Connection,
    episode: &Episode,
    outcome: IdleEpisodeOutcome,
    now: f64,
) -> Result<(), StoreError> {
    connection.execute("UPDATE runtime_agents SET record=json_set(record,'$.idleCompactionEpisode.fired',json('true'),'$.idleCompactionEpisode.outcome',?2,'$.idleCompactionEpisode.firedAt',?3) WHERE idle_episode_type='object' AND idle_episode_id=?1 AND COALESCE(json_extract(record,'$.idleCompactionEpisode.fired'),0)=0", params![episode.id,outcome,now])?;
    Ok(())
}

/// Rechecks eligibility under the same writer transaction as native admission.
/// The native obligation and the current episode fire commit together or neither commits.
pub struct IdleAdmission {
    pub inner: Arc<dyn Admission>,
    pub episode: Episode,
    pub limits: Value,
}
impl Admission for IdleAdmission {
    fn idle_episode_id(&self) -> Option<&str> {
        Some(&self.episode.id)
    }
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
            return Err(NativeError::Invalid(reason.as_str()));
        }
        if OrchestrationSettingsRepository::idle_compaction(tx)?.mode == Mode::Log {
            return Err(NativeError::Invalid("would_compact"));
        }
        self.inner.check(tx, agent, action)?;
        record(tx, &self.episode, IdleEpisodeOutcome::Started, now)?;
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.idleCompactionEpisode.limits',json(?2),'$.idleCompactionEpisode.before',json(?3)) WHERE id=?1", params![agent["id"].as_str(),self.limits.to_string(),json!({"status":agent["status"],"startAttempt":agent["startAttempt"],"turnEpoch":agent["turnEpoch"]}).to_string()])?;
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
            let saved = crate::native_actions::idle_provenance(tx, &request_id)?
                .ok_or(NativeError::Changed)?;
            crate::native_actions::fail_pending(tx, &request_id, &message)?;
            value["status"] = Value::String(saved.before.status.as_str().to_owned());
            value["startAttempt"] = saved.before.start_attempt;
            value["turnEpoch"] = json!(saved.before.turn_epoch);
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
) -> Result<Option<IdleEpisodeOutcome>, StoreError> {
    if agent["idleCompactionEpisode"]["id"] != request {
        return Ok(Some(IdleEpisodeOutcome::NewerRequest));
    }
    let episode: Episode = Episode::deserialize(&agent["idleCompactionEpisode"])
        .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
    let receipt = &agent["idleCompactionEpisode"];
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
    let settings = OrchestrationSettingsRepository::idle_compaction_for(tx, agent)?;
    Ok((settings.mode == Mode::Log).then_some(IdleEpisodeOutcome::WouldCompact))
}

pub fn limits_allowed(limits: &Value, agent: &Value, now: f64) -> bool {
    // B2-6 owns freshness and model-specific window eligibility. A native
    // warning is stricter than ordinary admission: only `allowed` may compact.
    let Ok(info) =
        studio_runtime_schema::provider_rate_limits::checked_claude_rate_info(&limits["data"])
    else {
        return false;
    };
    info.is_none_or(|info| info["status"] == "allowed")
        && crate::usage_resume::eligibility(limits, agent, now)["canRun"] == true
}
fn check(
    connection: &Connection,
    episode: &Episode,
    agent: &Value,
    limits: &Value,
    now: f64,
) -> Result<Option<IdleEpisodeOutcome>, StoreError> {
    let settings =
        OrchestrationSettingsRepository::idle_compaction_for(connection, agent)?.with_environment();
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

const NEXT_EPISODE_SQL: &str = "SELECT a.id,a.record FROM runtime_agents a WHERE a.idle_episode_type='object' AND a.idle_episode_id NOT IN (SELECT value FROM json_each(?1)) AND COALESCE(json_extract(a.record,'$.idleCompactionEpisode.fired'),0)=0 ORDER BY a.idle_episode_fire_at,a.id LIMIT 1";

#[derive(Deserialize)]
struct EpisodeRecord {
    #[serde(rename = "idleCompactionEpisode")]
    episode: Episode,
}

/// Due timers are read from SQLite, never from a bridge's process lifetime.
pub async fn next(store: &RuntimeStore) -> Result<Option<(String, Episode)>, StoreError> {
    next_excluding(store, Vec::new()).await
}
async fn next_excluding(
    store: &RuntimeStore,
    running: Vec<String>,
) -> Result<Option<(String, Episode)>, StoreError> {
    store
        .read_async("runtime-services/idle-compaction/deadline", move |db| {
            let raw: Option<(String, String)> = db
                .query_row(NEXT_EPISODE_SQL, [json!(running).to_string()], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
                .optional()?;
            raw.map(|(agent, raw)| {
                serde_json::from_str::<EpisodeRecord>(&raw)
                    .map(|record| (agent, record.episode))
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))
            })
            .transpose()
        })
        .await
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
                record(tx, &candidate, IdleEpisodeOutcome::NoHost, now)?;
                return Ok(false);
            };
            let agent = agent.value();
            let settings = OrchestrationSettingsRepository::idle_compaction_for(tx, &agent)?
                .with_environment();
            let reason = check(tx, &candidate, &agent, &observation, now)?;
            if let Some(reason) = reason {
                record(tx, &candidate, reason, now)?;
                return Ok(false);
            }
            if settings.mode == Mode::Log {
                record(tx, &candidate, IdleEpisodeOutcome::WouldCompact, now)?;
                return Ok(false);
            }
            Ok(true)
        })
        .await?;
    if start {
        let result = host.compact(&agent_id, episode.clone(), limits).await;
        let outcome = if result.is_ok() {
            IdleEpisodeOutcome::Dispatched
        } else {
            IdleEpisodeOutcome::Failed
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

    #[test]
    fn unload_deadline_scan_skips_json_for_non_claude_agents() {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch(studio_runtime_store::agent_columns::agent_table_sql())
            .unwrap();
        db.execute_batch("CREATE INDEX runtime_idle_compaction_deadline ON runtime_agents(idle_episode_type,idle_episode_fire_at,id,idle_episode_id) WHERE idle_episode_type='object'").unwrap();
        for n in 0..20 {
            let id = format!("codex-{n}");
            let raw = json!({"id":id,"rootId":id,"provider":"codex","epoch":1,"status":"idle","accountKey":"default"});
            db.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                rusqlite::params![id, raw.to_string()],
            )
            .unwrap();
        }
        let calls = Arc::new(AtomicUsize::new(0));
        let counted = Arc::clone(&calls);
        db.create_scalar_function(
            "json_extract",
            2,
            rusqlite::functions::FunctionFlags::SQLITE_DETERMINISTIC
                | rusqlite::functions::FunctionFlags::SQLITE_INNOCUOUS,
            move |_| {
                counted.fetch_add(1, Ordering::Relaxed);
                Ok(None::<i64>)
            },
        )
        .unwrap();
        sync_unload_deadlines(&db, "default", &BTreeMap::new(), 0.0).unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn unload_deadline_sync_updates_only_matching_account_and_only_on_change() {
        let db = studio_runtime_store::test_canvas();
        let (_, episode, mut agent, _) = fixture();
        agent["rootId"] = json!("agent");
        agent["idleCompactionEpisode"] = json!(episode);
        AgentRepository::put(
            &db,
            &studio_runtime_store::AgentRecord::from_value(agent.clone()).unwrap(),
        )
        .unwrap();
        agent["id"] = json!("other");
        agent["rootId"] = json!("other");
        agent["accountKey"] = json!("other-account");
        agent["idleCompactionEpisode"]["id"] = json!("other-episode");
        AgentRepository::put(
            &db,
            &studio_runtime_store::AgentRecord::from_value(agent).unwrap(),
        )
        .unwrap();
        let other_before = AgentRepository::get_raw(&db, &AgentId::from("other")).unwrap();
        let deadlines = BTreeMap::from([("thread".to_owned(), 1200.0)]);
        sync_unload_deadlines(&db, "default", &deadlines, 10.0).unwrap();
        let saved = AgentRepository::get(&db, &AgentId::from("agent"))
            .unwrap()
            .unwrap()
            .value();
        assert_eq!(saved["idleCompactionEpisode"]["unloadAt"], 1200.0);
        assert!(saved["idleCompactionEpisode"]["fireAt"].as_f64().unwrap() <= 1200.0);
        assert_eq!(
            AgentRepository::get_raw(&db, &AgentId::from("other")).unwrap(),
            other_before
        );
        let unchanged = AgentRepository::get_raw(&db, &AgentId::from("agent")).unwrap();
        sync_unload_deadlines(&db, "default", &deadlines, 10.0).unwrap();
        assert_eq!(
            AgentRepository::get_raw(&db, &AgentId::from("agent")).unwrap(),
            unchanged
        );
    }

    #[test]
    fn receipt_domain_idle_repeat_keeps_fired_episode_through_disabled_observation() {
        let db = studio_runtime_store::test_canvas();
        let (settings, episode, mut agent, _) = fixture();
        OrchestrationSettingsRepository::set_idle_compaction(&db, &settings).expect("settings");
        observe(&db, &mut agent, &json!(episode.cache), None).expect("first observation");
        let root = agent["idleCompactionEpisode"]["id"].clone();
        agent["idleCompactionEpisode"]["fired"] = json!(true);
        agent["idleCompactionEpisode"]["outcome"] = json!(IdleEpisodeOutcome::Started);
        let mut disabled = settings;
        disabled.mode = Mode::Off;
        OrchestrationSettingsRepository::set_idle_compaction(&db, &disabled).expect("disable");
        observe(&db, &mut agent, &json!(episode.cache), Some(&episode)).expect("same episode");
        assert_eq!(agent["idleCompactionEpisode"]["id"], root);
        assert_eq!(agent["idleCompactionEpisode"]["fired"], true);
        disabled.mode = Mode::Compact;
        OrchestrationSettingsRepository::set_idle_compaction(&db, &disabled).expect("enable");
        observe(&db, &mut agent, &json!(episode.cache), Some(&episode)).expect("repeat");
        assert_eq!(agent["idleCompactionEpisode"]["id"], root);
        assert_eq!(agent["idleCompactionEpisode"]["fired"], true);
        let tx = db.unchecked_transaction().expect("tx");
        agent["idleCompactionEpisode"] = Value::Null;
        assert_eq!(
            handoff_refusal(&tx, &agent, root.as_str().expect("root")).expect("superseded"),
            Some(IdleEpisodeOutcome::NewerRequest)
        );
    }

    #[test]
    fn receipt_domain_idle_fires_on_owner_once_without_receipt_table() {
        let db = studio_runtime_store::test_canvas();
        let (_, episode, mut agent, _) = fixture();
        agent["rootId"] = json!("agent");
        agent["idleCompactionEpisode"] = json!(episode);
        AgentRepository::put(
            &db,
            &studio_runtime_store::AgentRecord::from_value(agent).expect("agent"),
        )
        .expect("save owner");
        record(&db, &episode, IdleEpisodeOutcome::Started, 3241.0).expect("fire");
        let saved = AgentRepository::get(&db, &AgentId::from("agent"))
            .expect("read")
            .expect("owner")
            .value();
        assert_eq!(saved["idleCompactionEpisode"]["fired"], true);
        assert_eq!(saved["idleCompactionEpisode"]["outcome"], "started");
        assert!(settled(&db, &episode.id).expect("settled"));
    }
    #[test]
    fn episode_lookup_searches_deadline_index() {
        let db = studio_runtime_store::test_canvas();
        let plan = db
            .prepare(&format!("EXPLAIN QUERY PLAN {NEXT_EPISODE_SQL}"))
            .expect("plan")
            .query_map(["[]"], |row| row.get::<_, String>(3))
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows");
        eprintln!("episode lookup plan: {plan:?}");
        assert!(
            plan.iter()
                .any(|line| line.contains("SEARCH a USING INDEX runtime_idle_compaction_deadline")),
            "{plan:?}"
        );
        assert!(
            !plan
                .iter()
                .any(|line| line.contains("SCAN a") || line.contains("TEMP B-TREE")),
            "{plan:?}"
        );
    }

    #[tokio::test]
    async fn episode_lookup_keeps_order_exclusions_and_updates() {
        let root = tempfile::tempdir().expect("state");
        let store = RuntimeStore::open(Some(root.path())).await.expect("store");
        store
            .transaction(|tx| {
                let (_, mut episode, mut agent, _) = fixture();
                agent["rootId"] = json!("root");
                agent["events"] = json!(0);
                for (id, deadline) in [("later", 20.0), ("first", 10.0), ("second", 10.0)] {
                    agent["id"] = json!(id);
                    episode.id = id.into();
                    episode.fire_at = deadline;
                    agent["idleCompactionEpisode"] = json!(episode);
                    AgentRepository::put(
                        tx,
                        &studio_runtime_store::AgentRecord::from_value(agent.clone())?,
                    )?;
                }
                Ok(())
            })
            .await
            .expect("episodes");
        assert_eq!(
            next(&store).await.expect("next").expect("episode").0,
            "first"
        );
        assert_eq!(
            next_excluding(&store, vec!["first".into()])
                .await
                .expect("next")
                .expect("episode")
                .0,
            "second"
        );
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.idleCompactionEpisode.fired',json('true')) WHERE id='first'", [])?;
            tx.execute("UPDATE runtime_agents SET record=json_remove(record,'$.idleCompactionEpisode') WHERE id='second'", [])?;
            Ok(())
        }).await.expect("settle and remove");
        assert_eq!(
            next(&store).await.expect("next").expect("episode").0,
            "later"
        );
        assert!(
            next_excluding(&store, vec!["later".into()])
                .await
                .expect("next")
                .is_none()
        );
        store.transaction(|tx| {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.idleCompactionEpisode.fireAt','invalid') WHERE id='later'", [])?;
            Ok(())
        }).await.expect("invalid episode fixture");
        assert!(matches!(
            next(&store).await,
            Err(StoreError::SchemaMismatch(_))
        ));
    }

    #[test]
    fn unsupported_rate_evidence_never_proves_resume_or_compaction() {
        let agent =
            json!({"id":"agent", "accountKey":"default", "provider":"claude", "model":"sonnet"});
        let known = json!({"status":"allowed", "rateLimitType":"five_hour", "utilization":0.2, "resetsAt":2000});
        let base = json!({"accountKey":"default", "at":1000, "data":{"rateLimits":{"limitId":"claude", "primary":{"usedPercent":20, "resetsAt":2000}}, "claudeRateLimit":known}});
        assert_eq!(
            crate::usage_resume::eligibility(&base, &agent, 1000.0)["canRun"],
            true
        );
        assert!(limits_allowed(&base, &agent, 1000.0));
        for raw in [
            json!({"type":"unsupported", "raw":{"status":"future"}}),
            json!({"status":"future", "rateLimitType":"five_hour"}),
            json!({"status":"allowed", "rateLimitType":"future_scope"}),
            json!({"status":"allowed", "rateLimitType":"five_hour", "overageStatus":"future"}),
            json!({"status":"allowed", "rateLimitType":"five_hour", "resetsAt":"soon"}),
            Value::Null,
        ] {
            for bucket in [false, true] {
                let mut observation = base.clone();
                if bucket {
                    observation["data"]["rateLimits"]["claudeRateLimit"] = raw.clone();
                } else {
                    observation["data"]["claudeRateLimit"] = raw.clone();
                }
                let before = observation.clone();
                let decision = crate::usage_resume::eligibility(&observation, &agent, 1000.0);
                assert_eq!(decision["fresh"], false, "{raw}; bucket={bucket}");
                assert_eq!(decision["canRun"], false);
                assert_eq!(decision["allowed"], false);
                assert!(!limits_allowed(&observation, &agent, 1000.0));
                assert_eq!(observation, before);
            }
        }
    }
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
            fired: false,
            outcome: None,
            fired_at: None,
            before: None,
            limits: None,
            unload_at: None,
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
            "ttl_changed",
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
                "ttl_changed" => a["idleCache"]["ttl"] = json!("5m"),
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
                refusal(&s, &e, &a, c.0, allowed, queued).map(IdleEpisodeOutcome::as_str),
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
            Some(IdleEpisodeOutcome::Lapsed)
        );
        episode.cache.warm = true;
        episode.expires_at = clock.0;
        assert_eq!(
            refusal(&settings, &episode, &agent, clock.0, true, false),
            Some(IdleEpisodeOutcome::Lapsed)
        );
        episode.expires_at += 1.0;
        agent["startAttempt"]["id"] = json!("new-turn");
        assert_eq!(
            refusal(&settings, &episode, &agent, clock.0, true, false),
            Some(IdleEpisodeOutcome::NewerRequest)
        );
    }
}
