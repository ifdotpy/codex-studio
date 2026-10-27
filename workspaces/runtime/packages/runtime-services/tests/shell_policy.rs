//! Fake clocks and pressure sources exercise durable monitor admission and I/O.
#![allow(clippy::unwrap_used)]
use crate::monitor_fixtures as fixtures;
use serde_json::{Value, json};
use std::sync::{
    Arc,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use studio_runtime_schema::shell_policy::Settings;
use studio_runtime_services::{
    monitor_driver::{self, MonitorNative, Submission},
    monitor_policy::{Decision, MonitorError},
    monitor_runtime, shell_policy as policy,
    shell_pressure::PressureSource,
};
use studio_runtime_store::{
    AgentId, AgentRepository, MonitorsRepository, OrchestrationSettingsRepository, RuntimeStore,
};

struct FakePressure(AtomicBool);
impl PressureSource for FakePressure {
    fn critical(&self) -> bool {
        self.0.load(Ordering::SeqCst)
    }
}

async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, String) {
    let root = fixtures::tempdir();
    let mut case = fixtures::corpus(root.path()).remove(0);
    case["actor"]["shellPolicyLastActiveAt"] = json!(0.0);
    case["before"]["activityAt"] = json!(0.0);
    case["before"]["created"] = json!(0.0);
    let id = case["before"]["id"].as_str().unwrap().to_owned();
    let store = Arc::new(fixtures::store(root.path(), "shell-policy").await);
    fixtures::seed(&store, &case).await;
    (root, store, id)
}

async fn maintain(
    store: &RuntimeStore,
    id: &str,
    now: f64,
    pressure: &dyn PressureSource,
    settings: Settings,
) -> Value {
    let id = id.to_owned();
    let critical = pressure.critical();
    store
        .transaction_with(move |tx| {
            OrchestrationSettingsRepository::set_shell_policy(tx, &settings)?;
            let (before, mut actor) = monitor_runtime::snapshot(tx, &id)?;
            let mut decision = Decision {
                record: before.record().clone(),
                effects: vec![],
                response: Value::Null,
            };
            policy::maintain_monitor(
                tx,
                &mut actor,
                &before,
                &mut decision,
                &settings,
                critical,
                now,
            )?;
            monitor_runtime::apply(tx, &before, &actor, &decision, now)?;
            Ok::<_, MonitorError>(decision.record)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn pressure_requires_thirty_idle_minutes_and_preserves_active_turns() {
    for blocker in [
        "none",
        "turn",
        "input",
        "delegate",
        "disabled",
        "output",
        "recent_input",
        "restart",
    ] {
        let (_root, store, id) = fixture().await;
        let selected = id.clone();
        store.transaction(move |tx| {
            let mut actor = AgentRepository::get(tx, &AgentId::from("agent"))?.unwrap();
            match blocker {
                "turn" => { actor.extra.insert("inFlight".into(), json!(true)); }
                "recent_input" => { actor.extra.insert("shellPolicyLastActiveAt".into(), json!(1.0)); }
                "input" => { tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('user','agent','user','hi','pending',0,2)", [])?; }
                "delegate" => { let mut child = actor.clone(); child.id = "child".into(); child.status = studio_runtime_store::AgentStatus::Running; child.extra.insert("inFlight".into(), json!(true)); AgentRepository::put(tx, &child)?; }
                "output" => { tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.activityAt',1) WHERE id=?1", [&selected])?; }
                "restart" => { actor.extra.insert("shellPolicyLastReapAt".into(), json!(1.0)); tx.execute("UPDATE runtime_monitors SET record=json_set(record,'$.created',2) WHERE id=?1", [&selected])?; }
                _ => {}
            }
            AgentRepository::put(tx, &actor)
        }).await.unwrap();
        let source = FakePressure(AtomicBool::new(false));
        let settings = Settings {
            pressure_cleanup: blocker != "disabled",
            ..Settings::default()
        };
        assert_ne!(
            maintain(&store, &id, 1800.0, &source, settings.clone()).await["cancelRequested"],
            true
        );
        source.0.store(true, Ordering::SeqCst);
        assert_ne!(
            maintain(&store, &id, 1799.999, &source, settings.clone()).await["cancelRequested"],
            true
        );
        let record = maintain(&store, &id, 1800.0, &source, settings).await;
        assert_eq!(
            record["cancelRequested"] == true,
            blocker == "none",
            "{blocker}"
        );
    }
}

#[tokio::test]
async fn checkin_is_durable_once_per_output_generation_and_does_not_double_stall() {
    let (_root, store, id) = fixture().await;
    let source = FakePressure(AtomicBool::new(false));
    let settings = Settings {
        quiet_threshold_ms: 10000,
        ..Settings::default()
    };
    maintain(&store, &id, 9.999, &source, settings.clone()).await;
    assert_eq!(fixtures::events(&store), json!([]));
    maintain(&store, &id, 10.0, &source, settings.clone()).await;
    maintain(&store, &id, 20.0, &source, settings.clone()).await;
    assert_eq!(fixtures::events(&store).as_array().unwrap().len(), 1);
    monitor_runtime::tick_page(&store, 1800.0, None)
        .await
        .unwrap();
    assert_eq!(fixtures::events(&store).as_array().unwrap().len(), 1);
    let conn = store.test_reader().unwrap();
    let raw: String = conn
        .query_row("SELECT record FROM runtime_event_meta LIMIT 1", [], |r| {
            r.get(0)
        })
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&raw).unwrap()["delivery"],
        "next_turn"
    );
    let selected = id.clone();
    store
        .transaction_with(move |tx| {
            tx.execute("UPDATE runtime_events SET status='done'", [])
                .map_err(studio_runtime_store::StoreError::from)?;
            let mut actor = AgentRepository::get(tx, &AgentId::from("agent"))?.unwrap();
            actor.status = studio_runtime_store::AgentStatus::Idle;
            AgentRepository::put(tx, &actor)?;
            let (before, actor) = monitor_runtime::snapshot(tx, &selected)?;
            let decision = studio_runtime_services::monitor_policy::output(
                &before,
                before.operation().unwrap(),
                b"new",
                20.0,
            )?;
            monitor_runtime::apply(tx, &before, &actor, &decision, 20.0)
        })
        .await
        .unwrap();
    maintain(&store, &id, 30.0, &source, settings).await;
    assert_eq!(fixtures::events(&store).as_array().unwrap().len(), 2);
}

struct Native {
    store: Arc<RuntimeStore>,
    id: String,
    calls: AtomicUsize,
}
impl MonitorNative for Native {
    fn submit<'a>(
        &'a self,
        _: &'a studio_runtime_services::monitor_policy::Operation,
        _: &'a studio_runtime_services::monitor_policy::Effect,
    ) -> futures_util::future::BoxFuture<'a, Submission> {
        Box::pin(async move {
            let conn = self.store.test_reader().unwrap();
            let monitor = MonitorsRepository::get(&conn, &self.id).unwrap().unwrap();
            assert_eq!(monitor.value()["cancelRequested"], true);
            assert!(monitor.value()["shellPressureCleanup"].is_object());
            let notices: i64 = conn
                .query_row("SELECT count(*) FROM runtime_items", [], |r| r.get(0))
                .unwrap();
            assert_eq!(notices, 1);
            self.calls.fetch_add(1, Ordering::SeqCst);
            Submission::Unknown
        })
    }
}

#[tokio::test]
async fn cleanup_records_intent_before_native_stop_and_never_repeats_unknown_io() {
    let (_root, store, id) = fixture().await;
    let pressure = FakePressure(AtomicBool::new(true));
    maintain(&store, &id, 1800.0, &pressure, Settings::default()).await;
    assert_eq!(fixtures::events(&store), json!([]));
    let native = Native {
        store: store.clone(),
        id: id.clone(),
        calls: AtomicUsize::new(0),
    };
    monitor_driver::cancel(&store, &native, &id, None, 1800.0)
        .await
        .unwrap();
    monitor_driver::cancel(&store, &native, &id, None, 1801.0)
        .await
        .unwrap();
    assert_eq!(native.calls.load(Ordering::SeqCst), 1);
    maintain(&store, &id, 1802.0, &pressure, Settings::default()).await;
    maintain(&store, &id, 1803.0, &pressure, Settings::default()).await;
    assert_eq!(fixtures::events(&store).as_array().unwrap().len(), 1);
    assert!(
        fixtures::events(&store)[0]["text"]["message"]
            .as_str()
            .unwrap()
            .contains("unknown")
    );
    let conn = store.test_reader().unwrap();
    let actor = AgentRepository::get(&conn, &AgentId::from("agent"))
        .unwrap()
        .unwrap();
    let monitor = MonitorsRepository::get(&conn, &id).unwrap().unwrap();
    let mut active = actor.value();
    active["inFlight"] = json!(true);
    assert!(!policy::monitor_cleanup_allowed(&conn, &active, monitor.value(), 3600.0).unwrap());
}

#[tokio::test]
async fn an_explicit_owner_stop_does_not_inherit_the_pressure_guard() {
    let (_root, store, id) = fixture().await;
    let pressure = FakePressure(AtomicBool::new(true));
    maintain(&store, &id, 1800.0, &pressure, Settings::default()).await;
    store
        .transaction_with(move |tx| {
            let (_, operation) = monitor_driver::reserve_cancel(tx, &id, Some("agent"), 1801.0)?;
            assert!(operation.is_some());
            let (snapshot, actor) = monitor_runtime::snapshot(tx, &id)?;
            assert!(snapshot.record().get("shellPressureCleanup").is_none());
            let mut active = actor.value();
            active["inFlight"] = json!(true);
            assert!(policy::monitor_cleanup_allowed(
                tx,
                &active,
                snapshot.record(),
                1801.0
            )?);
            Ok::<_, MonitorError>(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn new_output_or_an_expired_decision_blocks_pressure_termination() {
    let (_root, store, id) = fixture().await;
    let pressure = FakePressure(AtomicBool::new(true));
    let monitor = maintain(&store, &id, 1800.0, &pressure, Settings::default()).await;
    let conn = store.test_reader().unwrap();
    let actor = AgentRepository::get(&conn, &AgentId::from("agent"))
        .unwrap()
        .unwrap()
        .value();
    assert!(policy::monitor_cleanup_allowed(&conn, &actor, &monitor, 1801.0).unwrap());
    assert!(!policy::monitor_cleanup_allowed(&conn, &actor, &monitor, 1803.0).unwrap());
    let mut output = monitor;
    output["activityGeneration"] = json!(1);
    assert!(!policy::monitor_cleanup_allowed(&conn, &actor, &output, 1801.0).unwrap());
}

struct NotSent;
impl MonitorNative for NotSent {
    fn submit<'a>(
        &'a self,
        _: &'a studio_runtime_services::monitor_policy::Operation,
        _: &'a studio_runtime_services::monitor_policy::Effect,
    ) -> futures_util::future::BoxFuture<'a, Submission> {
        Box::pin(async {
            Submission::NotSent {
                reason: monitor_driver::NotSentReason::Transient,
                error: "policy check refused the stop".into(),
            }
        })
    }
}

#[tokio::test]
async fn a_proved_not_sent_expired_cleanup_withdraws_its_cancel_intent() {
    let (_root, store, id) = fixture().await;
    let pressure = FakePressure(AtomicBool::new(true));
    maintain(&store, &id, 1800.0, &pressure, Settings::default()).await;
    monitor_driver::cancel(&store, &NotSent, &id, None, 1801.0)
        .await
        .unwrap();
    let conn = store.test_reader().unwrap();
    let monitor = MonitorsRepository::get(&conn, &id).unwrap().unwrap();
    assert_eq!(monitor.value()["cancelRequested"], false);
    assert!(monitor.value().get("shellPressureCleanup").is_none());
    assert_eq!(
        fixtures::events(&store)[0]["text"]["shellPolicy"]["kind"],
        "cleanup_skipped"
    );
}
