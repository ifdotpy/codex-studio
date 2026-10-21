//! Deterministic witnesses for bounded monitor wakes and missed one-shot rules.
#[path = "monitors/fixtures.rs"]
mod fixtures;
use serde_json::{Value, json};
use studio_runtime_services::{
    monitor_policy::{self, Effect, MonitorSnapshot, Operation},
    rules_engine::{RuleSnapshot, TickObservation},
};

fn monitor() -> (Value, Value, Operation) {
    let root = fixtures::tempdir();
    let case = fixtures::corpus(root.path()).remove(0);
    let mut record = case["before"].clone();
    record["outputPatterns"] = json!(["READY"]);
    let operation = serde_json::from_value(record["operation"].clone()).expect("operation");
    (record, case["actor"].clone(), operation)
}

fn output(
    record: &mut Value,
    actor: &Value,
    operation: &Operation,
    stream: &str,
    bytes: &[u8],
    now: f64,
) -> Vec<Value> {
    let snapshot = MonitorSnapshot::checked(record.clone(), actor.clone()).expect("snapshot");
    let decision =
        monitor_policy::output_stream(&snapshot, operation, stream, bytes, now).expect("output");
    *record = decision.record;
    decision
        .effects
        .into_iter()
        .filter_map(|effect| match effect {
            Effect::OutputEvent { payload, .. } => Some(payload),
            _ => None,
        })
        .collect()
}

#[test]
fn monitor_bucket_admits_then_suppresses_and_refills() {
    let (mut record, actor, operation) = monitor();
    let mut events = Vec::new();
    for second in 0..25 {
        events.extend(output(
            &mut record,
            &actor,
            &operation,
            "stdout",
            b"READY\n",
            2000.0 + f64::from(second),
        ));
        record = serde_json::from_str(&record.to_string()).expect("persisted budget");
    }
    assert_eq!(
        events.len(),
        23,
        "22 admitted batches and one initial suppression notice"
    );
    assert_eq!(record["outputWakeBudget"]["totalSuppressedMatches"], 3);
    assert!(
        events
            .iter()
            .any(|event| event["suppressedMatches"].as_u64().unwrap_or(0) > 0)
    );
    let resumed = output(
        &mut record,
        &actor,
        &operation,
        "stdout",
        b"READY\n",
        2100.0,
    );
    assert_eq!(resumed.len(), 1, "a quiet period refills the budget");
}

#[test]
fn sustained_monitor_flood_pauses_only_output_wakes_with_notice() {
    let (mut record, actor, operation) = monitor();
    let mut events = Vec::new();
    for second in 0..100 {
        events.extend(output(
            &mut record,
            &actor,
            &operation,
            "stdout",
            b"READY\n",
            2000.0 + f64::from(second),
        ));
    }
    let notices: Vec<_> = events
        .iter()
        .filter(|event| event["outputWakesPaused"] == true)
        .collect();
    assert_eq!(notices.len(), 1, "one durable flood notice");
    let notice = notices[0];
    assert!(
        notice["suppressedMatches"]
            .as_u64()
            .expect("suppression count")
            > 0
    );
    assert!(
        notice["message"]
            .as_str()
            .expect("notice")
            .contains("resume")
    );
    assert!(
        output(
            &mut record,
            &actor,
            &operation,
            "stdout",
            b"READY\n",
            3000.0
        )
        .is_empty(),
        "pause survives quiet time"
    );
    let snapshot = MonitorSnapshot::checked(record, actor).expect("snapshot");
    let finished =
        monitor_policy::finish(&snapshot, Some(&operation), Some(0), None, 3001.0).expect("finish");
    assert!(
        finished
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::ExitEvent))
    );
    assert!(
        !finished
            .effects
            .iter()
            .any(|effect| matches!(effect, Effect::OutputEvent { .. }))
    );
}

#[test]
fn monitor_payload_has_first_line_excerpt_and_exact_log_offset_across_streams() {
    let (mut record, actor, operation) = monitor();
    assert!(
        output(
            &mut record,
            &actor,
            &operation,
            "stdout",
            b"ignore\nstart RE",
            2000.0
        )
        .is_empty()
    );
    assert!(
        output(
            &mut record,
            &actor,
            &operation,
            "stderr",
            b"other\n",
            2000.1
        )
        .is_empty()
    );
    let events = output(
        &mut record,
        &actor,
        &operation,
        "stdout",
        b"ADY done\nREADY again\n",
        2001.0,
    );
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["matches"][0]["excerpt"], "start READY done");
    assert_eq!(events[0]["matches"][0]["logOffset"], 7);
    assert_eq!(events[0]["matches"][0]["count"], 2);
}

fn once() -> (Value, Value) {
    (
        json!({"id":"once","agent":"owner","epoch":1,"name":"Once","kind":"once","status":"active","intervalSeconds":60,"nextAt":2000.0,"at":2000.0,"created":1900.0,"command":"echo should-run-only-on-time","text":"Check result"}),
        json!({"id":"owner","rootId":"owner","epoch":1,"status":"idle","autoWake":true,"isLead":true}),
    )
}

#[test]
fn overdue_once_rule_emits_missed_event_without_command_and_stays_pending() {
    let (rule, owner) = once();
    let snapshot = RuleSnapshot::from_records(rule, owner.clone()).expect("rule");
    let decision = snapshot
        .tick(2061.0, TickObservation::default())
        .expect("tick");
    assert!(decision.check.is_none(), "a missed one-shot must not run");
    let wake = decision.wake.expect("missed notice");
    assert_eq!(wake.kind, "rule_missed");
    let payload: Value = serde_json::from_str(&wake.text).expect("payload");
    assert_eq!(payload["rule"], "once");
    assert_eq!(payload["dueAt"], 2000.0);
    assert_eq!(payload["delaySeconds"], 61.0);
    let record = decision.record.expect("pending rule");
    assert_eq!(record["status"], "paused");
    assert_eq!(record["nextAt"], 2000.0);
    let snapshot = RuleSnapshot::from_records(record, owner).expect("persisted rule");
    assert_eq!(
        snapshot
            .tick(3000.0, TickObservation::default())
            .expect("later tick"),
        Default::default()
    );
    assert_eq!(
        snapshot.recover(3000.0).expect("restart"),
        Default::default()
    );
}

#[test]
fn once_rule_at_grace_boundary_still_runs() {
    let (rule, owner) = once();
    let snapshot = RuleSnapshot::from_records(rule, owner).expect("rule");
    assert!(
        snapshot
            .tick(2060.0, TickObservation::default())
            .expect("tick")
            .check
            .is_some()
    );
}
#[test]
fn monitor_excerpt_is_bounded_and_invalid_budget_is_rejected() {
    use studio_runtime_services::monitor_matches::{OUTPUT_EXCERPT_BYTES, OUTPUT_WAKE_CAPACITY};
    let (mut record, actor, operation) = monitor();
    let line = format!(
        "{}READY{}\n",
        "猫".repeat(OUTPUT_EXCERPT_BYTES),
        "z".repeat(OUTPUT_EXCERPT_BYTES)
    );
    let events = output(
        &mut record,
        &actor,
        &operation,
        "stdout",
        line.as_bytes(),
        2000.0,
    );
    let excerpt = events[0]["matches"][0]["excerpt"]
        .as_str()
        .expect("excerpt");
    assert!(excerpt.len() <= OUTPUT_EXCERPT_BYTES);
    assert!(excerpt.contains("READY"));
    assert!(
        events[0]["matches"][0]["logOffset"]
            .as_u64()
            .expect("offset")
            > 0
    );
    record["outputWakeBudget"]["tokens"] = json!(OUTPUT_WAKE_CAPACITY + 1);
    assert!(MonitorSnapshot::checked(record, actor).is_err());
}

#[tokio::test]
async fn monitor_output_receipt_commits_excerpt_budget_and_pause_across_restart() {
    use studio_runtime_services::{
        monitor_output::{self, MonitorOutput},
        monitor_runtime,
    };
    use studio_runtime_store::{MonitorsRepository, RuntimeStore};
    let root = fixtures::tempdir();
    let mut case = fixtures::corpus(root.path()).remove(0);
    let store = fixtures::store(root.path(), "flood").await;
    let id = case["before"]["id"].as_str().expect("id").to_owned();
    let operation: Operation =
        serde_json::from_value(case["before"]["operation"].clone()).expect("operation");
    case["before"]["outputPatterns"] = json!(["READY"]);
    case["before"]["log"] = json!(monitor_output::log_root(store.root()).join(format!("{id}.log")));
    fixtures::seed(&store, &case).await;
    let output = MonitorOutput::default();
    for second in 0..60_u64 {
        output
            .append_stream(
                &store,
                &id,
                &operation,
                second * 6,
                "stdout",
                b"READY\n",
                2000.0 + second as f64,
            )
            .await
            .expect("durable output");
    }
    let before = fixtures::events(&store);
    let notices: Vec<_> = before
        .as_array()
        .expect("events")
        .iter()
        .filter(|event| event["text"]["outputWakesPaused"] == true)
        .collect();
    assert_eq!(notices.len(), 1);
    assert_eq!(notices[0]["kind"], "monitor_output");
    for event in before.as_array().expect("events") {
        let matched = &event["text"]["matches"][0];
        assert_eq!(matched["excerpt"], "READY");
        assert_eq!(matched["logOffset"].as_u64().expect("log offset") % 6, 0);
    }
    drop(store);
    let store = RuntimeStore::open(Some(&root.path().join("flood")))
        .await
        .expect("restart");
    // The same receipt replays without budget consumption, byte growth, or a second event.
    output
        .append_stream(
            &store,
            &id,
            &operation,
            59 * 6,
            "stdout",
            b"READY\n",
            2100.0,
        )
        .await
        .expect("replay");
    output
        .append_stream(
            &store,
            &id,
            &operation,
            60 * 6,
            "stdout",
            b"READY\n",
            2200.0,
        )
        .await
        .expect("paused output");
    assert_eq!(fixtures::events(&store), before);
    let monitor = MonitorsRepository::get(&store.test_reader().expect("reader"), &id)
        .expect("read monitor")
        .expect("monitor");
    assert_eq!(monitor.value()["status"], "running");
    assert_eq!(monitor.value()["bytes"], 61 * 6);
    assert_eq!(monitor.value()["outputWakeBudget"]["paused"], true);
    let total = monitor.value()["outputWakeBudget"]["totalSuppressedMatches"].clone();
    store
        .transaction_with(move |tx| {
            monitor_runtime::finish_in_transaction(tx, &id, Some(&operation), Some(0), None, 2201.0)
        })
        .await
        .expect("exit wake");
    let events = fixtures::events(&store);
    let exit = events
        .as_array()
        .expect("events")
        .iter()
        .find(|event| event["kind"] == "monitor_exit")
        .expect("terminal wake");
    assert_eq!(
        exit["text"]["outputMatchSummary"]["suppressedMatches"],
        total
    );
    assert_eq!(
        exit["text"]["outputMatchSummary"]["outputWakesPaused"],
        true
    );
}

#[tokio::test]
async fn missed_once_rule_is_atomic_durable_and_owner_can_resume_or_delete() {
    use studio_runtime_services::{
        managed_status_watch::{self, LocalRuleWatchFiles},
        rules_engine::RuleCheck,
        rules_tick::{self, CheckSubmission, LocalRuleFiles, RuleMonitor, RulesTickError},
    };
    use studio_runtime_store::{
        AgentRecord, AgentRepository, RuleRecord, RulesRepository, RuntimeStore,
    };
    struct Monitor(std::sync::atomic::AtomicUsize);
    impl RuleMonitor for Monitor {
        fn available(&self) -> bool {
            true
        }
        fn launch<'a>(
            &'a self,
            _: &'a RuleCheck,
        ) -> futures_util::future::BoxFuture<'a, CheckSubmission> {
            Box::pin(async move {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                CheckSubmission::Accepted
            })
        }
        fn cancel<'a>(
            &'a self,
            _: &'a str,
        ) -> futures_util::future::BoxFuture<'a, Result<(), RulesTickError>> {
            Box::pin(async { Ok(()) })
        }
    }
    let root = fixtures::tempdir();
    let store = RuntimeStore::open(Some(root.path())).await.expect("store");
    let (rule, mut owner) = once();
    owner["cwd"] = json!(root.path());
    store
        .transaction(move |tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(owner)?)?;
            RulesRepository::put(tx, &RuleRecord::from_value(rule)?)
        })
        .await
        .expect("seed");
    let monitor = Monitor(Default::default());
    store.transaction(|tx| {
        tx.execute_batch("CREATE TRIGGER missed_fail BEFORE INSERT ON runtime_events BEGIN SELECT RAISE(ABORT,'missed-failpoint'); END;")?;
        Ok(())
    }).await.expect("failpoint");
    assert!(
        rules_tick::tick_batch(&store, 0, 2061.0, &LocalRuleFiles, &monitor)
            .await
            .is_err()
    );
    let read = || {
        RulesRepository::get(&store.test_reader().expect("reader"), "once")
            .expect("read rule")
            .expect("rule")
            .value()
            .clone()
    };
    assert_eq!(read()["status"], "active", "event failure rolls back pause");
    assert!(
        fixtures::events(&store)
            .as_array()
            .expect("events")
            .is_empty()
    );
    store
        .transaction(|tx| {
            tx.execute_batch("DROP TRIGGER missed_fail")?;
            Ok(())
        })
        .await
        .expect("clear failpoint");
    rules_tick::tick_batch(&store, 0, 2061.0, &LocalRuleFiles, &monitor)
        .await
        .expect("missed tick");
    assert_eq!(monitor.0.load(std::sync::atomic::Ordering::SeqCst), 0);
    assert_eq!(read()["status"], "paused");
    let original = fixtures::events(&store);
    assert_eq!(original[0]["kind"], "rule_missed");
    assert_eq!(original[0]["text"]["dueAt"], 2000.0);
    drop(store);
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart");
    rules_tick::recover_batch(&store, 0, 3000.0, &monitor)
        .await
        .expect("recover");
    rules_tick::tick_batch(&store, 0, 3000.0, &LocalRuleFiles, &monitor)
        .await
        .expect("paused tick");
    assert_eq!(fixtures::events(&store), original);
    let resumed = managed_status_watch::rules(
        &store,
        "owner",
        1,
        Some(&json!({"action":"resume","id":"once"})),
        "resume-once",
        3000.0,
        &LocalRuleWatchFiles,
    )
    .await
    .expect("owner resume");
    assert_eq!(resumed.value["status"], "active");
    let due = resumed.value["nextAt"].as_f64().expect("new due time");
    rules_tick::tick_batch(&store, 0, due, &LocalRuleFiles, &monitor)
        .await
        .expect("explicit catch-up");
    assert_eq!(monitor.0.load(std::sync::atomic::Ordering::SeqCst), 1);
    let deleted = managed_status_watch::rules(
        &store,
        "owner",
        1,
        Some(&json!({"action":"delete","id":"once"})),
        "delete-once",
        3100.0,
        &LocalRuleWatchFiles,
    )
    .await
    .expect("owner cancel");
    assert_eq!(deleted.value["deleted"], "once");
}

#[test]
fn legacy_matcher_context_never_invents_a_log_offset() {
    use base64::{Engine, engine::general_purpose::STANDARD};
    let (mut record, actor, operation) = monitor();
    record["bytes"] = json!(100);
    record["outputMatcherTail"] = json!({"stdout":STANDARD.encode(b"REA")});
    let events = output(
        &mut record,
        &actor,
        &operation,
        "stdout",
        b"DY done\n",
        2000.0,
    );
    // The old stream prefix has no log location. The new fragment does.
    assert_eq!(events[0]["matches"][0]["excerpt"], "DY done");
    assert_eq!(events[0]["matches"][0]["logOffset"], 100);
    MonitorSnapshot::checked(record, actor).expect("legacy context remains readable");
}

#[test]
fn multiline_pattern_excerpt_contains_the_complete_match() {
    let (mut record, actor, operation) = monitor();
    record["outputPatterns"] = json!(["A\nB"]);
    let events = output(
        &mut record,
        &actor,
        &operation,
        "stdout",
        b"prefix A\nB tail\n",
        2000.0,
    );
    assert_eq!(events[0]["matches"][0]["excerpt"], "prefix A\nB tail");
    assert_eq!(events[0]["matches"][0]["logOffset"], 0);
}
