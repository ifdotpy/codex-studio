//! Monitor admission uses current ownership while retaining prepared permissions.
use crate::TestJson;
use crate::monitor_fixtures as fixtures;
use serde_json::{Value, json};
use std::sync::Mutex;
use studio_runtime_services::{
    monitor_driver::{self, MonitorNative, Submission},
    monitor_policy::{Effect, Operation},
    monitor_runtime::PreparedStart,
};
use studio_runtime_store::{AgentRecord, AgentRepository, MonitorsRepository, RuntimeStore};

#[derive(Default)]
struct Port(Mutex<Vec<Effect>>);
impl MonitorNative for Port {
    fn submit<'a>(
        &'a self,
        _: &'a Operation,
        effect: &'a Effect,
    ) -> futures_util::future::BoxFuture<'a, Submission> {
        Box::pin(async move {
            self.0.lock().expect("submissions").push(effect.clone());
            Submission::Written
        })
    }
}

async fn fixture() -> (tempfile::TempDir, RuntimeStore, PreparedStart) {
    let root = fixtures::tempdir();
    let mut case = fixtures::corpus(root.path()).remove(0);
    let operation: Operation =
        serde_json::from_value(case["before"]["operation"].clone()).expect("operation");
    case["before"]
        .as_object_mut()
        .expect("monitor")
        .remove("operation");
    case["before"]["status"] = json!("starting");
    case["before"]["turnId"] = json!("created-turn");
    case["actor"]["status"] = json!("running");
    case["actor"]["threadId"] = json!("thread");
    case["actor"]["turnId"] = json!("turn");
    let store = fixtures::store(root.path(), "start").await;
    fixtures::seed(&store, &case).await;
    let prepared = PreparedStart {
        monitor: serde_json::from_value(case["before"].clone()).expect("valid fixture"),
        actor: case["actor"].clone(),
        operation,
        params: json!({"processId":case["before"]["id"], "cwd":case["before"]["cwd"],
            "timeoutMs":case["before"]["timeout_ms"], "sandboxPolicy":{"type":"readOnly"}}),
    };
    (root, store, prepared)
}

async fn change_actor(store: &RuntimeStore, patch: Value) {
    store
        .transaction(move |tx| {
            let mut actor = AgentRepository::get(tx, &"agent".into())?
                .expect("actor")
                .value();
            for (key, value) in patch.as_object().expect("patch") {
                actor[key] = value.clone();
            }
            AgentRepository::put(tx, &AgentRecord::from_value(actor)?)
        })
        .await
        .expect("actor change");
}

#[tokio::test]
async fn monitor_start_survives_actor_progress_and_submits_once() {
    let (_root, store, prepared) = fixture().await;
    let id = prepared.monitor.id.as_str();
    change_actor(
        &store,
        json!({
            "updated":2001.0,"status":"waiting","inFlight":true,"turnId":null,"rootId":"other-root",
            "startAttempt":{"id":"new-progress","epoch":2},"timing":{"fastTokens":10},
            "profile":{"label":"Renamed profile"},
        }),
    )
    .await;
    let port = Port::default();
    assert_eq!(fixtures::events(&store), json!([]));
    assert!(matches!(
        monitor_driver::start(&store, &port, id, &prepared, 2001.0)
            .await
            .expect("start"),
        Submission::Written
    ));
    let db = store.test_reader().expect("reader");
    assert_eq!(
        MonitorsRepository::get(&db, id)
            .expect("read")
            .expect("monitor")
            .value()["status"],
        "running"
    );
    assert_eq!(
        MonitorsRepository::get(&db, id)
            .expect("read")
            .expect("monitor")
            .value()["turnId"],
        "created-turn"
    );
    assert!(
        monitor_driver::start(&store, &port, id, &prepared, 2002.0)
            .await
            .is_err()
    );
    let submissions = port.0.lock().expect("submissions");
    assert!(
        matches!(submissions.as_slice(), [Effect::Native { method: "command/exec", params, .. }] if params == &prepared.params)
    );
}

#[tokio::test]
async fn monitor_start_refuses_changed_ownership_permissions_or_monitor() {
    let (_root, store, prepared) = fixture().await;
    let id = prepared.monitor.id.as_str();
    let baseline = json!({"actor": prepared.actor, "before": prepared.monitor, "priorEvents": []});
    let baseline_actor =
        AgentRepository::get(&store.test_reader().expect("reader"), &"agent".into())
            .expect("read actor")
            .expect("actor")
            .value();
    let baseline_monitor = MonitorsRepository::get(&store.test_reader().expect("reader"), id)
        .expect("read monitor")
        .expect("monitor")
        .value();
    for patch in [
        json!({"epoch":3}),
        json!({"autoWake":false}),
        json!({"deletedAt":2001}),
        json!({"threadId":"new-thread"}),
        json!({"cwd":"/changed"}),
        json!({"accountKey":"other"}),
        json!({"yoloMode":true}),
        json!({"approvalPolicy":"never"}),
        json!({"sandbox":{"type":"dangerFullAccess"}}),
        json!({"profile":{"id":"other"}}),
        json!({"role":"reviewer"}),
        json!({"isLead":true}),
        json!({"executionMode":"vm"}),
        json!({"imageWorkspace":true}),
        json!({"workspaceMode":"image"}),
        json!({"layrScratch":"/other"}),
        json!({"provider":"claude"}),
        json!({"remoteWorker":true}),
        json!({"remoteAnchor":true}),
    ] {
        fixtures::seed(&store, &baseline).await;
        assert_eq!(
            AgentRepository::get(&store.test_reader().expect("reader"), &"agent".into())
                .expect("read actor")
                .expect("actor")
                .value(),
            baseline_actor
        );
        assert_eq!(
            MonitorsRepository::get(&store.test_reader().expect("reader"), id)
                .expect("read monitor")
                .expect("monitor")
                .value(),
            baseline_monitor
        );
        assert_eq!(fixtures::events(&store), json!([]));
        change_actor(&store, patch.clone()).await;
        let changed_actor =
            AgentRepository::get(&store.test_reader().expect("reader"), &"agent".into())
                .expect("read actor")
                .expect("actor")
                .value();
        let port = Port::default();
        assert!(
            monitor_driver::start(&store, &port, id, &prepared, 2001.0)
                .await
                .is_err(),
            "{patch}"
        );
        assert!(port.0.lock().expect("submissions").is_empty(), "{patch}");
        assert_eq!(fixtures::events(&store), json!([]), "{patch}");
        assert_eq!(
            AgentRepository::get(&store.test_reader().expect("reader"), &"agent".into())
                .expect("read actor")
                .expect("actor")
                .value(),
            changed_actor
        );
        assert_eq!(
            MonitorsRepository::get(&store.test_reader().expect("reader"), id)
                .expect("read")
                .expect("monitor")
                .value(),
            baseline_monitor
        );
    }
    for sql in [
        "DELETE FROM runtime_monitors",
        "UPDATE runtime_monitors SET record=json_set(record,'$.command','changed')",
    ] {
        fixtures::seed(&store, &baseline).await;
        assert_eq!(
            AgentRepository::get(&store.test_reader().expect("reader"), &"agent".into())
                .expect("read actor")
                .expect("actor")
                .value(),
            baseline_actor
        );
        assert_eq!(
            MonitorsRepository::get(&store.test_reader().expect("reader"), id)
                .expect("read monitor")
                .expect("monitor")
                .value(),
            baseline_monitor
        );
        assert_eq!(fixtures::events(&store), json!([]));
        store
            .transaction(move |tx| {
                tx.execute(sql, [])?;
                Ok(())
            })
            .await
            .expect("monitor change");
        let changed_monitor = MonitorsRepository::get(&store.test_reader().expect("reader"), id)
            .expect("read monitor")
            .map(|monitor| monitor.value());
        let port = Port::default();
        assert!(
            monitor_driver::start(&store, &port, id, &prepared, 2001.0)
                .await
                .is_err()
        );
        assert!(port.0.lock().expect("submissions").is_empty());
        assert_eq!(fixtures::events(&store), json!([]));
        assert_eq!(
            AgentRepository::get(&store.test_reader().expect("reader"), &"agent".into())
                .expect("read actor")
                .expect("actor")
                .value(),
            baseline_actor
        );
        assert_eq!(
            MonitorsRepository::get(&store.test_reader().expect("reader"), id)
                .expect("read monitor")
                .map(|monitor| monitor.value()),
            changed_monitor
        );
    }
}
