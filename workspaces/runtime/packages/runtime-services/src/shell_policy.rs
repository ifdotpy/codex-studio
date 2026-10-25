//! Background-shell maintenance on the retained provider connection.
//! Native output observation stays in the bridge; notices enter the journal.
use crate::{
    account_session_runtime::{AccountSessionPort, ProviderRequest},
    account_worker::retained::RequestActor,
    shell_pressure::PressureSource,
};
use rusqlite::Connection;
use serde_json::{Value, json};
use std::{
    sync::Arc,
    time::{Duration, Instant},
};
use studio_runtime_schema::shell_policy::{PRESSURE_IDLE_MS, Settings, TICK_MS};
use studio_runtime_store::{
    AgentRecord, AgentRepository, NotificationDispatchRepository, OrchestrationSettingsRepository,
    RuntimeStore, StoreError, Transaction,
};
use tokio::sync::watch;

/// Called by the journal's accepted turn transitions, in their transaction.
pub(crate) fn note_activity(tx: &Transaction<'_>, id: &str, at: f64) -> Result<(), StoreError> {
    if let Some(mut agent) = AgentRepository::get(tx, &id.into())? {
        agent
            .extra
            .insert("shellPolicyLastActiveAt".into(), json!(at));
        AgentRepository::put(tx, &agent)?;
    }
    Ok(())
}

/// Same guard is used at selection and immediately before provider hand-off.
/// Queued input and any live descendant prevent cleanup.
pub fn eligible(db: &Connection, agent: &Value) -> Result<bool, StoreError> {
    if agent["inFlight"] == true
        || agent["autoWake"] != true
        || agent["turnId"].as_str().is_some_and(|id| !id.is_empty())
        || !matches!(
            agent["status"].as_str(),
            Some("idle" | "completed" | "waiting_on_monitor")
        )
        || studio_runtime_store::truthy(agent.get("deletedAt"))
        || studio_runtime_store::truthy(agent.get("accountTransferId"))
        || studio_runtime_store::truthy(agent.get("remoteWorker"))
        || studio_runtime_store::truthy(agent.get("remoteAnchor"))
        || agent["executionMode"] == "vm"
    {
        return Ok(false);
    }
    let id = agent["id"].as_str().unwrap_or_default();
    let blocked: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND kind!='shell_policy' AND status IN ('pending','reserved','dispatching','uncertain')) OR EXISTS(SELECT 1 FROM runtime_agents child WHERE json_extract(child.record,'$.rootId')=?2 AND child.id!=?1 AND COALESCE(json_extract(child.record,'$.deletedAt'),0)=0 AND (json_extract(child.record,'$.inFlight')=1 OR json_extract(child.record,'$.status') IN ('queued','starting','running','approval','compacting')))",
        (id, agent["rootId"].as_str().unwrap_or(id)), |row| row.get(0))?;
    Ok(!blocked)
}

/// Recheck at the native cancellation boundary, including new input/output.
pub fn monitor_cleanup_allowed(
    db: &Connection,
    agent: &Value,
    monitor: &Value,
    now: f64,
) -> Result<bool, StoreError> {
    let Some(marker) = monitor.get("shellPressureCleanup") else {
        return Ok(true);
    };
    let settings = OrchestrationSettingsRepository::shell_policy(db)?;
    Ok(settings.pressure_cleanup
        && eligible(db, agent)?
        && monitor["activityGeneration"] == marker["generation"]
        && marker["at"]
            .as_f64()
            .is_some_and(|at| now >= at && (now - at) * 1000.0 <= TICK_MS as f64)
        && now - agent["shellPolicyLastActiveAt"].as_f64().unwrap_or(now)
            >= PRESSURE_IDLE_MS as f64 / 1000.0)
}

/// Add maintenance intents to the ordinary monitor transition. Existing stall
/// and quiet check-ins share the output generation, so they cannot double-wake.
pub fn maintain_monitor(
    tx: &Transaction<'_>,
    actor: &mut AgentRecord,
    before: &crate::monitor_policy::MonitorSnapshot,
    decision: &mut crate::monitor_policy::Decision,
    settings: &Settings,
    critical: bool,
    now: f64,
) -> Result<(), crate::monitor_policy::MonitorError> {
    use crate::monitor_policy::{Effect, MonitorError};
    let agent = actor.value();
    let monitor = before.record();
    if monitor["epoch"]
        .as_i64()
        .or_else(|| monitor["operation"]["epoch"].as_i64())
        != Some(actor.epoch)
    {
        return Ok(());
    }
    if monitor["shellPressureCleanup"].is_object()
        && monitor["cancelDispatch"]["status"] == "unknown"
    {
        let report = json!({"id":format!("monitor:{}:{}:cleanup_unknown",before.id(),monitor["shellPressureCleanup"]["generation"]),"kind":"cleanup_unknown","message":format!("Critical memory pressure: the stop outcome for monitor {} is unknown. Inspect the monitor before another stop.",before.id())});
        observe(tx, &agent, &report, now)
            .map_err(|_| MonitorError::Refused("Could not record the shell cleanup outcome"))?;
    }
    if monitor["status"] != "running"
        || monitor["cancelRequested"] == true
        || monitor["ruleId"].as_str().is_some()
        || monitor["epoch"]
            .as_i64()
            .or_else(|| monitor["operation"]["epoch"].as_i64())
            != Some(actor.epoch)
        || monitor["reattachRecovery"].is_object()
        || monitor["operation"].is_null()
        || !eligible(tx, &agent)?
    {
        return Ok(());
    }
    let generation = monitor["activityGeneration"].as_i64().unwrap_or(0);
    let activity = monitor["activityAt"]
        .as_f64()
        .or_else(|| monitor["created"].as_f64())
        .unwrap_or(now);
    let quiet_ms = (now - activity).max(0.0) * 1000.0;
    let session_idle_ms =
        (now - agent["shellPolicyLastActiveAt"].as_f64().unwrap_or(now)).max(0.0) * 1000.0;
    let last_reap = agent["shellPolicyLastReapAt"].as_f64();
    let restart_guard = last_reap.is_some_and(|last| {
        last > agent["shellPolicyLastActiveAt"].as_f64().unwrap_or(now)
            && monitor["created"].as_f64().is_some_and(|created| {
                created > last
                    && (created - last) * 1000.0
                        < studio_runtime_schema::shell_policy::PRESSURE_RESTART_GUARD_MS as f64
            })
    });
    let reap = critical
        && settings.pressure_cleanup
        && !restart_guard
        && quiet_ms >= PRESSURE_IDLE_MS as f64
        && session_idle_ms >= PRESSURE_IDLE_MS as f64;
    let kind = if reap { "cleanup_requested" } else { "quiet" };
    if !reap
        && (settings.quiet_threshold_ms == 0
            || quiet_ms < settings.quiet_threshold_ms as f64
            || monitor["stallWakeGeneration"].as_i64().unwrap_or(-1) >= generation)
    {
        return Ok(());
    }
    let message = if reap {
        format!(
            "Critical memory pressure: Studio requested a stop for monitor {} after {} idle minutes. The monitor exit event confirms the outcome.",
            before.id(),
            PRESSURE_IDLE_MS / 60_000
        )
    } else {
        format!(
            "Monitor {} is still running without new output for {} seconds. Read its output and inspect its processes. Leave it running if it is a server or watcher.",
            before.id(),
            (quiet_ms / 1000.0) as u64
        )
    };
    let report = json!({"id":format!("monitor:{}:{generation}:{kind}",before.id()),"kind":kind,"message":message,"monitorId":before.id()});
    observe(tx, &agent, &report, now).map_err(|error| match error {
        crate::notification_dispatch::NotificationDispatchError::Store(error) => {
            MonitorError::Store(error)
        }
        _ => MonitorError::Refused("Could not record the shell policy notice"),
    })?;
    decision
        .effects
        .retain(|effect| !matches!(effect, Effect::StallEvent { .. } | Effect::Probe { .. }));
    decision.record["stallWakeGeneration"] = json!(generation);
    if reap {
        decision.record["cancelRequested"] = json!(true);
        decision.record["cancelDispatch"] = json!({"status":"pending","attempts":0});
        decision.record["shellPressureCleanup"] = json!({"at":now,"generation":generation});
        actor
            .extra
            .insert("shellPolicyLastReapAt".into(), json!(now));
        AgentRepository::put(tx, actor)?;
    }
    Ok(())
}

/// One durable notice and queue event per native report identity. A stop
/// request remains distinct from a confirmed stop or an unknown outcome.
pub fn observe(
    tx: &Transaction<'_>,
    agent: &Value,
    report: &Value,
    at: f64,
) -> Result<(), crate::notification_dispatch::NotificationDispatchError> {
    let id = report["id"]
        .as_str()
        .filter(|id| !id.is_empty() && id.len() <= 1024)
        .ok_or(crate::notification_dispatch::NotificationDispatchError::InvalidNotification)?;
    let text = report["message"]
        .as_str()
        .filter(|text| text.len() <= 8192)
        .ok_or(crate::notification_dispatch::NotificationDispatchError::InvalidNotification)?;
    if !matches!(
        report["kind"].as_str(),
        Some(
            "quiet"
                | "cleanup_requested"
                | "cleanup_stopped"
                | "cleanup_unknown"
                | "cleanup_skipped"
        )
    ) {
        return Err(crate::notification_dispatch::NotificationDispatchError::InvalidNotification);
    }
    let actor = AgentRecord::from_value(agent.clone()).map_err(StoreError::from)?;
    let key = format!("{}:shell-policy:{id}", actor.id.0);
    if NotificationDispatchRepository::notification_item(tx, &actor.id.0, &key)?.is_none() {
        let item = json!({"id":key,"role":"system","title":"Background shell","text":text,"at":at,"nativeNotice":"info","shellPolicy":report});
        NotificationDispatchRepository::put_notification_item(
            tx,
            &key,
            &actor.id.0,
            &item,
            at,
            text,
            true,
        )?;
    }
    if report["kind"] == "cleanup_requested" {
        return Ok(());
    }
    // Do not interrupt an active turn. The scheduler receives the notice on
    // its next turn, including when the report arrives after new input.
    if studio_runtime_store::QueueWriteRepository::metadata(tx, &key)?.is_none() {
        studio_runtime_store::QueueWriteRepository::put_metadata(
            tx,
            &key,
            &json!({"delivery":"next_turn"}),
        )?;
    }
    crate::event_write::enqueue_recovery_event(
        tx,
        &actor,
        key.clone(),
        "shell_policy",
        json!({"message":text,"shellPolicy":report}).to_string(),
        at,
    )
    .map_err(|error| match error {
        crate::event_write::EventWriteError::Store(error) => {
            crate::notification_dispatch::NotificationDispatchError::Store(error)
        }
        _ => crate::notification_dispatch::NotificationDispatchError::InvalidNotification,
    })?;
    Ok(())
}

pub async fn run(
    store: Arc<RuntimeStore>,
    port: Arc<dyn AccountSessionPort>,
    pressure: Arc<dyn PressureSource>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut ticks = tokio::time::interval(Duration::from_millis(TICK_MS));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! { _ = shutdown.changed() => break, _ = ticks.tick() => {} }
        if *shutdown.borrow() {
            break;
        }
        pressure.refresh().await;
        let critical = pressure.critical();
        if let Err(error) = sweep(&store, port.as_ref(), critical).await {
            eprintln!("background shell policy: {error}");
        }
        if let Err(error) =
            sweep_monitors(&store, critical, crate::idle_compaction::wall_now()).await
        {
            eprintln!("monitor shell policy: {error}");
        }
    }
}

async fn sweep_monitors(
    store: &RuntimeStore,
    critical: bool,
    now: f64,
) -> Result<(), crate::monitor_policy::MonitorError> {
    let mut after = String::new();
    let enabled = store
        .read_async("runtime-services/shell-policy/monitor-settings", |db| {
            OrchestrationSettingsRepository::shell_policy(db)
                .map(|settings| settings.quiet_threshold_ms > 0)
        })
        .await?
        || critical;
    loop {
        let cursor = after.clone();
        let ids = store.read_async("runtime-services/shell-policy/monitors", move |db| {
            let mut statement = db.prepare("SELECT id FROM runtime_monitors WHERE id>?1 AND json_extract(record,'$.status')='running' AND (?2 OR json_type(record,'$.shellPressureCleanup')='object') ORDER BY id LIMIT 64")?;
            Ok::<_, StoreError>(statement.query_map((cursor, enabled), |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?)
        }).await?;
        if ids.is_empty() {
            break;
        }
        after = ids.last().cloned().unwrap_or_default();
        store
            .transaction_with(move |tx| {
                let settings = OrchestrationSettingsRepository::shell_policy(tx)?;
                for id in ids {
                    let (before, mut actor) = crate::monitor_runtime::snapshot(tx, &id)?;
                    if !actor.extra.contains_key("shellPolicyLastActiveAt") {
                        actor
                            .extra
                            .insert("shellPolicyLastActiveAt".into(), json!(now));
                        AgentRepository::put(tx, &actor)?;
                    }
                    let mut decision = crate::monitor_policy::Decision {
                        record: before.record().clone(),
                        effects: vec![],
                        response: Value::Null,
                    };
                    maintain_monitor(
                        tx,
                        &mut actor,
                        &before,
                        &mut decision,
                        &settings,
                        critical,
                        now,
                    )?;
                    crate::monitor_runtime::apply(tx, &before, &actor, &decision, now)?;
                }
                Ok::<_, crate::monitor_policy::MonitorError>(())
            })
            .await?;
    }
    Ok(())
}

async fn sweep(
    store: &RuntimeStore,
    port: &dyn AccountSessionPort,
    critical: bool,
) -> Result<(), StoreError> {
    let (settings, agents) = store.read_async("runtime-services/shell-policy", move |db| {
        let settings = OrchestrationSettingsRepository::shell_policy(db)?;
        if settings.quiet_threshold_ms == 0 && !critical { return Ok((settings,Vec::new())); }
        let mut statement = db.prepare("SELECT record FROM runtime_agents a WHERE json_extract(record,'$.provider')='claude' AND json_extract(record,'$.threadId') IS NOT NULL AND a.id IN (SELECT json_extract(record,'$.agent') FROM runtime_tasks WHERE json_extract(record,'$.status')='running' AND json_extract(record,'$.name')='local_bash') ORDER BY a.id")?;
        let agents = statement.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        let agents = agents.into_iter().map(|raw| serde_json::from_str::<Value>(&raw).map_err(StoreError::from)).collect::<Result<Vec<_>, _>>()?;
        Ok::<_, StoreError>((settings, agents))
    }).await?;
    // Calls are bounded and do not create or resume a provider session.
    for agent in agents {
        let account = agent["accountKey"].as_str().unwrap_or("default");
        let Some(scope) = port.current_scope(account) else {
            continue;
        };
        let id = agent["id"].as_str().unwrap_or_default().to_owned();
        let epoch = agent["epoch"].as_i64().unwrap_or(-1);
        let check = agent.clone();
        let allowed = store
            .read_async("runtime-services/shell-policy/eligibility", move |db| {
                eligible(db, &check)
            })
            .await?;
        drop(port.provider_request(ProviderRequest {
            account: account.into(), operation: format!("shell-policy:{id}:{}", uuid::Uuid::new_v4()), actor: Some(RequestActor {agent: id, epoch}), expected_scope: Some(scope), deleted_actor_session: None,
            method: studio_runtime_providers::Method::ClaudeShellPolicy,
            params: json!({"threadId":agent["threadId"],"settings":settings,"eligible":allowed,"critical":critical}),
            deadline: Instant::now() + Duration::from_secs(2),
        }).await);
    }
    Ok(())
}
