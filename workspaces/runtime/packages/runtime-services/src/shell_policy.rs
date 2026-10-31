//! Background-shell maintenance on the retained provider connection.
//! Native output observation stays in the bridge; notices enter the journal.
use crate::{
    account_session_runtime::{AccountSessionPort, ProviderRequest},
    account_worker::retained::RequestActor,
    shell_pressure::PressureSource,
};
use rusqlite::Connection;
use serde::{Deserialize, Deserializer};
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

// The sweep deliberately reads the raw agent row, before volatile fields are
// merged. Older rows admitted here can be incomplete or contain mistyped
// optional fields, unlike the full AgentRecord used at the hand-off fence.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ShellPolicyAgent {
    #[serde(default, deserialize_with = "string_or_missing")]
    id: Option<String>,
    #[serde(default, deserialize_with = "string_or_missing")]
    root_id: Option<String>,
    #[serde(default, deserialize_with = "string_or_missing")]
    account_key: Option<String>,
    #[serde(default, deserialize_with = "integer_or_missing")]
    epoch: Option<i64>,
    #[serde(default, deserialize_with = "string_or_missing")]
    status: Option<String>,
    #[serde(default, deserialize_with = "string_or_missing")]
    turn_id: Option<String>,
    #[serde(default, deserialize_with = "string_or_missing")]
    execution_mode: Option<String>,
    #[serde(default, deserialize_with = "true_only")]
    in_flight: bool,
    #[serde(default, deserialize_with = "true_only")]
    auto_wake: bool,
    thread_id: Option<Value>,
    deleted_at: Option<Value>,
    account_transfer_id: Option<Value>,
    remote_worker: Option<Value>,
    remote_anchor: Option<Value>,
}

fn string_or_missing<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Value::deserialize(deserializer).map(|value| value.as_str().map(str::to_owned))
}
fn integer_or_missing<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<i64>, D::Error> {
    Value::deserialize(deserializer).map(|value| value.as_i64())
}
fn true_only<'de, D: Deserializer<'de>>(deserializer: D) -> Result<bool, D::Error> {
    Value::deserialize(deserializer).map(|value| value == Value::Bool(true))
}

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
    if !agent.is_object() {
        return Ok(false);
    }
    let view = ShellPolicyAgent::deserialize(agent).map_err(StoreError::from)?;
    eligible_view(db, &view)
}

fn eligible_view(db: &Connection, agent: &ShellPolicyAgent) -> Result<bool, StoreError> {
    if agent.in_flight
        || !agent.auto_wake
        || agent.turn_id.as_deref().is_some_and(|id| !id.is_empty())
        || !matches!(
            agent.status.as_deref(),
            Some("idle" | "completed" | "waiting_on_monitor")
        )
        || studio_runtime_store::truthy(agent.deleted_at.as_ref())
        || studio_runtime_store::truthy(agent.account_transfer_id.as_ref())
        || studio_runtime_store::truthy(agent.remote_worker.as_ref())
        || studio_runtime_store::truthy(agent.remote_anchor.as_ref())
        || agent.execution_mode.as_deref() == Some("vm")
    {
        return Ok(false);
    }
    let id = agent.id.as_deref().unwrap_or_default();
    let blocked: bool = db.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND kind!='shell_policy' AND status IN ('pending','reserved','dispatching','uncertain')) OR EXISTS(SELECT 1 FROM runtime_agents child WHERE json_extract(child.record,'$.rootId')=?2 AND child.id!=?1 AND COALESCE(json_extract(child.record,'$.deletedAt'),0)=0 AND (json_extract(child.record,'$.inFlight')=1 OR json_extract(child.record,'$.status') IN ('queued','starting','running','approval','compacting')))",
        (id, agent.root_id.as_deref().unwrap_or(id)), |row| row.get(0))?;
    Ok(!blocked)
}

/// Recheck at the native cancellation boundary, including new input/output.
pub fn monitor_cleanup_allowed(
    db: &Connection,
    agent: &Value,
    monitor: &studio_runtime_store::MonitorRecord,
    now: f64,
) -> Result<bool, StoreError> {
    let Some(marker) = &monitor.shell_pressure_cleanup else {
        return Ok(true);
    };
    let settings = OrchestrationSettingsRepository::shell_policy(db)?;
    Ok(settings.pressure_cleanup
        && eligible(db, agent)?
        && monitor.activity_generation.get() == marker.generation
        && now >= marker.at
        && (now - marker.at) * 1000.0 <= TICK_MS as f64
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
    use studio_runtime_schema::{
        monitor_record::{MonitorCancelDispatch, MonitorCancelStatus, ShellPressureCleanup},
        studio_errors::MonitorStatus,
    };
    if monitor.epoch.get() != actor.epoch {
        return Ok(());
    }
    if let Some(cleanup) = &monitor.shell_pressure_cleanup
        && monitor
            .cancel_dispatch
            .as_ref()
            .is_some_and(|dispatch| dispatch.status == MonitorCancelStatus::Unknown)
    {
        let report = json!({"id":format!("monitor:{}:{}:cleanup_unknown", before.id(), cleanup.generation),"kind":"cleanup_unknown","message":format!("Critical memory pressure: the stop outcome for monitor {} is unknown. Inspect the monitor before another stop.",before.id())});
        observe(tx, &agent, &report, now)
            .map_err(|_| MonitorError::Refused("Could not record the shell cleanup outcome"))?;
    }
    if monitor.status != MonitorStatus::Running
        || monitor.cancel_requested == Some(true)
        || monitor.rule_id.is_some()
        || monitor.reattach_recovery.is_some()
        || monitor.operation.is_none()
        || !eligible(tx, &agent)?
    {
        return Ok(());
    }
    let generation = monitor.activity_generation.get();
    let activity = monitor.activity_at.seconds();
    let quiet_ms = (now - activity).max(0.0) * 1000.0;
    let session_idle_ms =
        (now - agent["shellPolicyLastActiveAt"].as_f64().unwrap_or(now)).max(0.0) * 1000.0;
    let last_reap = agent["shellPolicyLastReapAt"].as_f64();
    let restart_guard = last_reap.is_some_and(|last| {
        last > agent["shellPolicyLastActiveAt"].as_f64().unwrap_or(now)
            && monitor.created.seconds() > last
            && (monitor.created.seconds() - last) * 1000.0
                < studio_runtime_schema::shell_policy::PRESSURE_RESTART_GUARD_MS as f64
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
            || monitor.stall_wake_generation.get() >= generation)
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
        crate::notification_dispatch::NotificationDispatchError::RequestResult(error) => {
            MonitorError::Store(StoreError::RequestResult(error))
        }
        crate::notification_dispatch::NotificationDispatchError::Store(error) => {
            MonitorError::Store(error)
        }
        crate::notification_dispatch::NotificationDispatchError::InvalidNotification { .. }
        | crate::notification_dispatch::NotificationDispatchError::InvalidAccountState { .. }
        | crate::notification_dispatch::NotificationDispatchError::StreamCapacity
        | crate::notification_dispatch::NotificationDispatchError::InvalidStoredObject { .. }
        | crate::notification_dispatch::NotificationDispatchError::ConflictingDelivery
        | crate::notification_dispatch::NotificationDispatchError::ConflictingInput
        | crate::notification_dispatch::NotificationDispatchError::UnauthorizedSubmission
        | crate::notification_dispatch::NotificationDispatchError::Dependency(_)
        | crate::notification_dispatch::NotificationDispatchError::Move(_)
        | crate::notification_dispatch::NotificationDispatchError::Json(_) => {
            MonitorError::Refused("Could not record the shell policy notice")
        }
    })?;
    decision
        .effects
        .retain(|effect| !matches!(effect, Effect::StallEvent { .. } | Effect::Probe { .. }));
    decision.record.stall_wake_generation = generation
        .try_into()
        .map_err(|_| MonitorError::Refused("Invalid monitor generation"))?;
    if reap {
        decision.record.cancel_requested = Some(true);
        decision.record.cancel_dispatch = Some(MonitorCancelDispatch {
            status: MonitorCancelStatus::Pending,
            attempts: Some(0),
            updated_at: None,
            next_attempt_at: None,
            error: None,
        });
        decision.record.shell_pressure_cleanup = Some(ShellPressureCleanup {
            at: now,
            generation,
        });
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
        .ok_or(crate::notification_dispatch::NotificationDispatchError::invalid())?;
    let text = report["message"]
        .as_str()
        .filter(|text| text.len() <= 8192)
        .ok_or(crate::notification_dispatch::NotificationDispatchError::invalid())?;
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
        return Err(crate::notification_dispatch::NotificationDispatchError::invalid());
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
        crate::event_write::EventWriteError::InvalidRequest(_)
        | crate::event_write::EventWriteError::InvalidResultSchema(_)
        | crate::event_write::EventWriteError::Conflict(_)
        | crate::event_write::EventWriteError::RemoteEventsUnavailable
        | crate::event_write::EventWriteError::WorkerAssignmentUnavailable
        | crate::event_write::EventWriteError::InvalidStoredObject { .. }
        | crate::event_write::EventWriteError::RequestResult(_)
        | crate::event_write::EventWriteError::Clock => {
            crate::notification_dispatch::NotificationDispatchError::invalid()
        }
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
) -> Result<(), crate::account_worker::WorkerError> {
    let (settings, agents) = store.read_async("runtime-services/shell-policy", move |db| {
        let settings = OrchestrationSettingsRepository::shell_policy(db)?;
        if settings.quiet_threshold_ms == 0 && !critical { return Ok((settings,Vec::new())); }
        let mut statement = db.prepare("SELECT record FROM runtime_agents a WHERE json_extract(record,'$.provider')='claude' AND json_extract(record,'$.threadId') IS NOT NULL AND a.id IN (SELECT json_extract(record,'$.agent') FROM runtime_tasks WHERE json_extract(record,'$.status')='running' AND json_extract(record,'$.name')='local_bash') ORDER BY a.id")?;
        let agents = statement.query_map([], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        // Parse through a map so duplicate keys retain the historical last value.
        let agents = agents.into_iter().map(|raw| {
            let value: Value = serde_json::from_str(&raw)?;
            ShellPolicyAgent::deserialize(value).map_err(StoreError::from)
        }).collect::<Result<Vec<_>, _>>()?;
        Ok::<_, StoreError>((settings, agents))
    }).await?;
    // Calls are bounded and do not create or resume a provider session.
    for agent in agents {
        let account = agent.account_key.as_deref().unwrap_or("default");
        let Some(scope) = port.current_scope(account) else {
            continue;
        };
        let id = agent.id.as_deref().unwrap_or_default().to_owned();
        let epoch = agent.epoch.unwrap_or(-1);
        let account = account.to_owned();
        let policy = settings.clone();
        let request = store.transaction_with(move |tx| {
            use crate::account_worker::retained::{ProviderCallIntent, reserve_provider_call};
            use studio_runtime_schema::persistence::RequestIdentity;
            use studio_runtime_store::request_results::{Admission, RequestResults};
            let allowed = eligible_view(tx, &agent)?;
            let root_request_id = RequestIdentity::issue_root();
            let operation = format!("shell-policy:{id}:{root_request_id}");
            let params = json!({"threadId":agent.thread_id,"settings":policy,"eligible":allowed,"critical":critical});
            let identity = RequestIdentity::new("shell-policy", &[&id], &root_request_id, &[])
                .map_err(studio_runtime_store::request_results::RequestResultError::from).map_err(StoreError::from)?;
            let owner = match RequestResults::admit(tx, &identity, &params.to_string(), chrono::Utc::now().timestamp_millis()).map_err(StoreError::from)? {
                Admission::Fresh(owner) => owner,
                Admission::Replay(_) => return Err(crate::account_worker::WorkerError::Store(StoreError::RequestResult(
                    studio_runtime_store::request_results::RequestResultError::Conflict {id: identity.id().to_owned()}
                ))),
            };
            let actor = Some(RequestActor {agent: id, epoch});
            let method = studio_runtime_providers::Method::ClaudeShellPolicy;
            let admitted = reserve_provider_call(tx, &owner, ProviderCallIntent {
                root_request_id: root_request_id.clone(), admitted_request: None,
                scope: scope.clone(), operation: operation.clone(), actor: actor.clone(),
                transfer: None, deleted_actor_session: None, method, params: params.clone(),
            })?;
            Ok::<_,crate::account_worker::WorkerError>(ProviderRequest {
                root_request_id, admitted_request: Some(admitted), account, operation, actor,
                expected_scope: Some(scope), deleted_actor_session: None, method, params,
                deadline: Instant::now() + Duration::from_secs(2),
            })
        }).await?;
        let admitted = request.admitted_request.clone();
        let expected_scope = request.expected_scope.clone();
        let operation = request.operation.clone();
        if let (Some(admitted), Some(expected_scope), Ok(receipt)) = (
            admitted,
            expected_scope,
            port.provider_request(request).await,
        ) && receipt.scope == expected_scope
            && receipt.operation == operation
            && receipt.certainty() == crate::account_session_runtime::RequestCertainty::NotSent
        {
            store
                .transaction_with(move |tx| {
                    crate::account_worker::retained::discard_unsent_provider_call(tx, &admitted)
                })
                .await?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod receipt_tests {
    use super::*;
    use crate::account_session_runtime::{
        AccountSessionFuture, ConnectionScope, SessionOutcome, SessionReceipt,
    };

    struct NotSent;
    impl AccountSessionPort for NotSent {
        fn current_scope(&self, _: &str) -> Option<ConnectionScope> {
            Some(ConnectionScope {
                account: "default".into(),
                connection: "connection".into(),
                generation: 1,
            })
        }
        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            Box::pin(async move {
                Ok(SessionReceipt {
                    scope: request.expected_scope.unwrap(),
                    operation: request.operation,
                    outcome: SessionOutcome::NoLiveAccountWorker,
                    replayed: false,
                })
            })
        }
    }

    struct NoScope;
    impl AccountSessionPort for NoScope {
        fn current_scope(&self, _: &str) -> Option<ConnectionScope> {
            None
        }
        fn provider_request<'a>(
            &'a self,
            _: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            panic!("a missing scope never submits")
        }
    }

    struct WrongReceipt;
    impl AccountSessionPort for WrongReceipt {
        fn current_scope(&self, _: &str) -> Option<ConnectionScope> {
            Some(ConnectionScope {
                account: "default".into(),
                connection: "connection".into(),
                generation: 1,
            })
        }
        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            Box::pin(async move {
                let mut scope = request.expected_scope.unwrap();
                scope.generation += 1;
                Ok(SessionReceipt {
                    scope,
                    operation: request.operation,
                    outcome: SessionOutcome::NoLiveAccountWorker,
                    replayed: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn mismatched_unsent_scope_does_not_discard_reservation() {
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", ("agent", json!({"compactions":0,"accountKey":"default","id":"agent","rootId":"agent","provider":"claude","threadId":"thread","epoch":1,"autoWake":true,"status":"idle"}).to_string()))?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)", ("task", json!({"id":"task","agent":"agent","status":"running","name":"local_bash"}).to_string()))?;
            Ok::<_, StoreError>(())
        }).await.unwrap();
        sweep(&store, &WrongReceipt, true).await.unwrap();
        let pending: i64 = store.read_async("shell-policy/test", |db| Ok::<_, StoreError>(db.query_row("SELECT count(*) FROM runtime_outbox WHERE kind='provider_call' AND status='pending'", [], |row| row.get(0))?)).await.unwrap();
        assert_eq!(pending, 1);
    }

    #[tokio::test]
    async fn absent_scope_skips_partial_agent_before_strict_admission() {
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            let partial = json!({"id":"agent","provider":"claude","threadId":"thread","status":"idle","autoWake":true});
            assert!(AgentRecord::from_value(partial.clone()).is_err());
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", ("agent", partial.to_string()))?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)", ("task", json!({"id":"task","agent":"agent","status":"running","name":"local_bash"}).to_string()))?;
            Ok::<_, StoreError>(())
        }).await.unwrap();
        sweep(&store, &NoScope, true).await.unwrap();
    }

    #[tokio::test]
    async fn absent_scope_partial_agent_does_not_stop_later_valid_agent() {
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            for (id, value) in [
                ("a-partial", json!({"id":"a-partial","provider":"claude","threadId":"t","accountKey":"inactive","status":"idle","autoWake":true})),
                ("z-valid", json!({"compactions":0,"id":"z-valid","rootId":"z-valid","epoch":1,"provider":"claude","threadId":"t","accountKey":"default","status":"idle","autoWake":true})),
            ] {
                tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", (id, value.to_string()))?;
                tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)", (format!("task-{id}"), json!({"id":format!("task-{id}"),"agent":id,"status":"running","name":"local_bash"}).to_string()))?;
            }
            Ok::<_, StoreError>(())
        }).await.unwrap();
        let capture = Capture::default();
        sweep(&store, &capture, true).await.unwrap();
        let requests = capture.0.lock().unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].0, "z-valid");
    }

    #[derive(Default)]
    struct Capture(std::sync::Mutex<Vec<(String, i64, Value, ConnectionScope)>>);
    impl AccountSessionPort for Capture {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            (account == "default").then(|| ConnectionScope {
                account: account.into(),
                connection: "connection".into(),
                generation: 1,
            })
        }
        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            Box::pin(async move {
                let actor = request.actor.as_ref().unwrap();
                let scope = request.expected_scope.clone().unwrap();
                self.0.lock().unwrap().push((
                    actor.agent.clone(),
                    actor.epoch,
                    request.params.clone(),
                    scope.clone(),
                ));
                Ok(SessionReceipt {
                    scope,
                    operation: request.operation,
                    outcome: SessionOutcome::NoLiveAccountWorker,
                    replayed: false,
                })
            })
        }
    }

    #[tokio::test]
    async fn malformed_partial_agent_is_refused_before_provider_dispatch() {
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            let partial = json!({"id":"agent","provider":"claude","threadId":["opaque"],"accountKey":7,"epoch":1.0,"autoWake":true,"status":"idle"});
            assert!(AgentRecord::from_value(partial.clone()).is_err());
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", ("agent", partial.to_string()))?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)", ("task", json!({"id":"task","agent":"agent","status":"running","name":"local_bash"}).to_string()))?;
            Ok::<_, StoreError>(())
        }).await.unwrap();
        let capture = Capture::default();
        assert!(sweep(&store, &capture, true).await.is_err());
        let requests = capture.0.lock().unwrap();
        assert!(requests.is_empty());
    }

    #[tokio::test]
    async fn sweep_reserves_scoped_actor_then_discards_exact_unsent_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", ("agent", json!({"compactions":0,"id":"agent","rootId":"agent","provider":"claude","threadId":"thread","accountKey":"default","epoch":7,"autoWake":true,"status":"idle"}).to_string()))?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)", ("task", json!({"id":"task","agent":"agent","status":"running","name":"local_bash"}).to_string()))?;
            Ok::<_, StoreError>(())
        }).await.unwrap();
        let capture = Capture::default();
        sweep(&store, &capture, true).await.unwrap();
        {
            let requests = capture.0.lock().unwrap();
            assert_eq!(requests.len(), 1);
            assert_eq!((&requests[0].0, requests[0].1), (&"agent".to_owned(), 7));
            assert_eq!(requests[0].2["threadId"], "thread");
            assert_eq!(requests[0].2["eligible"], true);
            assert_eq!(
                (
                    requests[0].3.account.as_str(),
                    requests[0].3.connection.as_str(),
                    requests[0].3.generation
                ),
                ("default", "connection", 1)
            );
        }
        let pending: i64 = store.read_async("shell-policy/test", |db| Ok::<_, StoreError>(db.query_row("SELECT count(*) FROM runtime_outbox WHERE kind='provider_call' AND status='pending'", [], |row| row.get(0))?)).await.unwrap();
        assert_eq!(pending, 0);
    }

    #[test]
    fn partial_projection_keeps_legacy_defaults_and_truthiness() {
        let db = rusqlite::Connection::open_in_memory().unwrap();
        db.execute_batch("CREATE TABLE runtime_agents(id TEXT,record TEXT); CREATE TABLE runtime_events(agent TEXT,kind TEXT,status TEXT)").unwrap();
        let base = json!({"autoWake":true,"status":"idle","threadId":null,"id":false,"rootId":7});
        assert!(eligible(&db, &base).unwrap());
        for (field, value) in [
            ("inFlight", json!(true)),
            ("deletedAt", json!([1])),
            ("remoteWorker", json!("x")),
            ("executionMode", json!("vm")),
            ("turnId", json!("turn")),
        ] {
            let mut blocked = base.clone();
            blocked[field] = value;
            assert!(!eligible(&db, &blocked).unwrap(), "{field}");
        }
        let mut lenient = base;
        lenient["turnId"] = json!(["not a string"]);
        lenient["inFlight"] = json!(1);
        assert!(eligible(&db, &lenient).unwrap());
        let partial = json!({"id":"agent","provider":"claude","threadId":["opaque"],"accountKey":7,"epoch":1.0,"autoWake":true,"status":"idle"});
        let view = ShellPolicyAgent::deserialize(partial).unwrap();
        assert_eq!(view.account_key.as_deref().unwrap_or("default"), "default");
        assert_eq!(view.epoch.unwrap_or(-1), -1);
        assert_eq!(view.thread_id, Some(json!(["opaque"])));
    }

    #[test]
    fn shell_projection_agrees_with_persisted_agent_owner_on_valid_fields() {
        let value = json!({"compactions":0,"id":"a","rootId":"r","epoch":3,"status":"idle","accountKey":"default","threadId":"t","turnId":"","autoWake":true,"inFlight":false,"executionMode":"host","deletedAt":null,"accountTransferId":"transfer","remoteWorker":false,"remoteAnchor":null});
        let owner = studio_runtime_store::AgentRecord::from_value(value.clone()).unwrap();
        let partial = ShellPolicyAgent::deserialize(value).unwrap();
        assert_eq!(partial.id.as_deref(), Some(owner.id.0.as_str()));
        assert_eq!(partial.root_id.as_deref(), Some(owner.root_id.0.as_str()));
        assert_eq!(partial.epoch, Some(owner.epoch));
        assert_eq!(partial.status.as_deref(), Some(owner.status.as_str()));
        assert_eq!(
            partial.account_key.as_deref(),
            Some(owner.account_key.as_str())
        );
        assert_eq!(partial.turn_id.as_deref(), owner.turn_id.as_deref());
        assert_eq!(partial.execution_mode.as_deref(), Some("host"));
        assert_eq!(
            owner.execution_mode,
            Some(studio_runtime_store::AgentExecutionMode::Host)
        );
        assert_eq!(partial.auto_wake, owner.auto_wake.unwrap_or(false));
        assert_eq!(partial.in_flight, owner.in_flight.unwrap_or(false));
        assert_eq!(
            partial.thread_id.as_ref().and_then(Value::as_str),
            owner.thread_id.as_deref()
        );
        assert_eq!(
            partial.account_transfer_id,
            owner.extra.get("accountTransferId").cloned()
        );
        assert_eq!(
            partial.remote_worker,
            owner.extra.get("remoteWorker").cloned()
        );
        assert!(partial.remote_anchor.is_none());
        assert!(partial.deleted_at.is_none());
        assert!(owner.deleted_at.is_none());
        assert_eq!(owner.extra.get("remoteAnchor"), Some(&Value::Null));
    }

    #[tokio::test]
    async fn repeated_unsent_sweeps_do_not_leave_pending_provider_rows() {
        let directory = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        store.transaction_with(|tx| {
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES(?1,?2)", ("agent", json!({"compactions":0,"id":"agent","rootId":"agent","provider":"claude","threadId":"thread","accountKey":"default","epoch":1,"autoWake":true,"status":"idle"}).to_string()))?;
            tx.execute("INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)", ("task", json!({"id":"task","agent":"agent","status":"running","name":"local_bash"}).to_string()))?;
            Ok::<_, StoreError>(())
        }).await.unwrap();
        sweep(&store, &NotSent, true).await.unwrap();
        drop(store);
        let store = RuntimeStore::open(Some(directory.path())).await.unwrap();
        for _ in 0..2 {
            sweep(&store, &NotSent, true).await.unwrap();
        }
        let pending: i64 = store.read_async("shell-policy/test", |db| Ok::<_, StoreError>(db.query_row("SELECT count(*) FROM runtime_outbox WHERE kind='provider_call' AND status='pending'", [], |row| row.get(0))?)).await.unwrap();
        assert_eq!(pending, 0, "unsent periodic checks must not accumulate");
    }
}
