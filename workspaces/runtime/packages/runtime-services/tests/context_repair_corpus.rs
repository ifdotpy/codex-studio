use serde_json::{Value, json};
use std::fs;
use studio_runtime_services::{
    context_repair::{
        self, ConnectionIdentity, DeliveredManifestWriter, HistoryReadFuture, HistoryReader,
        InputDecision, ManifestWriterUnavailable, NativeHistory, RecoverySnapshot, RepairError,
    },
    historical_input_receipts::HistoricalBatch,
};
use studio_runtime_store::{ContextRepairRepository, RuntimeStore, Transaction};

fn identity() -> ConnectionIdentity {
    ConnectionIdentity {
        account_key: "default".into(),
        connection_id: "connection".into(),
        generation: 1,
    }
}
fn base_agent() -> Value {
    json!({"id":"agent","rootId":"agent","epoch":1,"accountKey":"default","threadId":"thread",
        "provider":"codex","model":"gpt-fixture","yoloMode":true,"status":"waiting","autoWake":true,
        "inFlight":false,
        "startAttempt":{"id":"attempt","events":["event"],"submitted":true},
        "contextRepairWait":{"error":"Context repair waits for a confirmed input receipt: event",
            "source":{"id":"agent","epoch":1,"accountKey":"default","threadId":"thread","attemptId":null}}})
}
fn base_event() -> Value {
    json!({"id":"event","agent":"agent","epoch":1,"kind":"user","text":"hello","status":"uncertain",
        "created":1.0,"turn_id":null,"error":"unknown","metadata":null})
}
/// The recurring-repair case: one uncertain input, one accepted native turn.
fn recurring_case() -> Value {
    json!({"agent":base_agent(),"events":[base_event()],
        "turns":[{"id":"native-turn","startOutcome":null,"clientUserMessageId":"event","items":[]}],
        "state":"idle","operations":[],"batches":[]})
}
/// A delivered primary input and an uncertain secondary input merged in one
/// transcript item, with the native turn and journal row that prove delivery.
fn exact_batch_case() -> Value {
    let agent = base_agent();
    let primary = json!({"id":"primary","agent":"agent","epoch":1,"kind":"user","text":"hello",
        "status":"delivered","created":1.0,"turn_id":"native-turn","error":null});
    let secondary = json!({"id":"event","agent":"agent","epoch":1,"kind":"work_review",
        "text":"secondary","status":"uncertain","created":2.0,"turn_id":null,"error":"unknown"});
    let inputs: Vec<Value> = [&primary, &secondary]
        .iter()
        .map(
            |row| json!({"id":row["id"],"text":row["text"],"at":row["created"],"kind":row["kind"]}),
        )
        .collect();
    let item = json!({"id":"agent:primary","role":"user","text":"hello\nsecondary",
        "inputs":inputs,"assets":[]});
    let members: Vec<Value> = [primary, secondary]
        .into_iter()
        .map(|mut row| {
            let mut metadata = json!({"transcriptItemId":"agent:primary","modelEventProjection":1,
                "contextManifest":{"epoch":["thread",0],"sequence":1,"versions":{}}});
            if row["id"] == "primary" {
                metadata["acceptedAt"] = json!(1.5);
            }
            row["metadata"] = metadata;
            row
        })
        .collect();
    let source = json!({"item":item,"row":{"id":"agent:primary","agent":"agent","record":item.to_string()},
        "fulltext":null,"members":members});
    let batch = HistoricalBatch::from_records(&agent, &source)
        .expect("valid synthetic fixture")
        .expect("captured batch");
    let turns = json!([{"id":"native-turn","startOutcome":"accepted","clientUserMessageId":"primary",
        "items":[{"id":"item","type":"userMessage","clientId":"primary","content":batch.params()["input"]}]}]);
    let operations = json!([{"handle":"account:default",
        "operation_id":"turn:agent:primary:attempt:00000000-0000-4000-8000-000000000001",
        "digest":batch.digest(),"native_id":17,"accepted":2.0}]);
    json!({"agent":agent,"source":source,"turns":turns,"operations":operations,"handle":"account:default"})
}
fn corpus() -> Value {
    json!({"recurring":[recurring_case()],"batches":[exact_batch_case()]})
}
/// Write the single-agent state the adapter tests clone.
async fn seed_template(root: &std::path::Path) {
    fs::create_dir_all(root).expect("valid synthetic fixture");
    let agent = recurring_case()["agent"].clone();
    let store = RuntimeStore::open(Some(root))
        .await
        .expect("template store opens");
    store
        .transaction(move |tx| {
            ContextRepairRepository::put_agent(tx, &agent)?;
            tx.execute(
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) VALUES ('event','agent','user','hello','uncertain',1.0,1,NULL,'unknown')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("template seeds");
}
fn snapshot(case: &Value) -> RecoverySnapshot {
    RecoverySnapshot::from_records(
        case["agent"].clone(),
        case["events"]
            .as_array()
            .expect("valid synthetic fixture")
            .clone(),
        identity(),
        case["batches"]
            .as_array()
            .expect("valid synthetic fixture")
            .clone(),
        None,
    )
    .expect("fixture records validate")
}
fn history(case: &Value) -> NativeHistory {
    NativeHistory::from_records(
        identity(),
        "thread".into(),
        case["turns"]
            .as_array()
            .expect("valid synthetic fixture")
            .clone(),
        case["state"].as_str().map(str::to_owned),
        case["operations"]
            .as_array()
            .expect("valid synthetic fixture")
            .clone(),
    )
    .expect("native fixture validates")
}
#[test]
fn context_repair_unwritten_start_outcomes_are_located() {
    let data = corpus();
    for outcome in [
        "failed",
        "completed",
        "cancelled",
        "interrupted",
        "unknown",
        "",
    ] {
        let mut case = recurring_case();
        case["turns"][0]["startOutcome"] = json!(outcome);
        let case = &case;
        // No writer emits these start outcomes.
        let result = NativeHistory::from_records(
            identity(),
            "thread".into(),
            case["turns"].as_array().expect("turns").clone(),
            Some("idle".into()),
            vec![],
        );
        assert!(
            matches!(
                result,
                Err(RepairError::Invalid("history.turn.startOutcome"))
            ),
            "{outcome}"
        );
    }
    for outcome in [json!(true), json!(3), json!([]), json!({})] {
        let mut turn = data["recurring"][0]["turns"][0].clone();
        turn["startOutcome"] = outcome;
        assert!(matches!(
            NativeHistory::from_records(
                identity(),
                "thread".into(),
                vec![turn],
                Some("idle".into()),
                vec![]
            ),
            Err(RepairError::Invalid("history.turn.startOutcome"))
        ));
    }
}

#[test]
fn context_repair_refuses_changed_scope_and_ambiguous_receipts() {
    let corpus = corpus();
    let base = &corpus["recurring"][0];
    for field in [
        "account",
        "connection",
        "generation",
        "thread",
        "paused",
        "epoch",
        "native",
        "ambiguous",
        "contradiction",
    ] {
        let mut case = base.clone();
        let mut connection = identity();
        let mut thread = "thread".to_owned();
        match field {
            "account" => connection.account_key = "foreign".into(),
            "connection" => connection.connection_id = "foreign".into(),
            "generation" => connection.generation += 1,
            "thread" => thread = "foreign".into(),
            "paused" => case["agent"]["status"] = json!("paused"),
            "epoch" => case["agent"]["contextRepairWait"]["source"]["epoch"] = json!(2),
            "native" => case["events"][0]["metadata"] = json!({"native":{"agent":"foreign"}}),
            "ambiguous" => {
                let turn = case["turns"][0].clone();
                case["turns"].as_array_mut().expect("valid synthetic fixture").push(turn);
            }
            "contradiction" => case["turns"].as_array_mut().expect("valid synthetic fixture").push(
                json!({"id":"rejected","clientUserMessageId":"event","startOutcome":"not_applied"}),
            ),
            _ => unreachable!(),
        }
        let history = NativeHistory::from_records(
            connection,
            thread,
            case["turns"]
                .as_array()
                .expect("valid synthetic fixture")
                .clone(),
            Some("idle".into()),
            vec![],
        )
        .expect("valid synthetic fixture");
        assert!(
            matches!(
                &context_repair::decide(&snapshot(&case), &history)[0],
                InputDecision::Waiting { .. }
            ),
            "{field}"
        );
    }
}
#[test]
fn context_repair_malformed_capture_is_located() {
    let data = corpus();
    let base = &data["recurring"][0];
    for field in ["agent", "events", "duplicate", "metadata", "oversize"] {
        let mut case = base.clone();
        match field {
            "agent" => case["agent"]["startAttempt"] = json!([]),
            "events" => case["events"][0]["epoch"] = json!(2),
            "duplicate" => {
                let row = case["events"][0].clone();
                case["events"]
                    .as_array_mut()
                    .expect("valid synthetic fixture")
                    .push(row);
            }
            "metadata" => case["events"][0]["metadata"] = json!("bad"),
            "oversize" => {
                case["events"][0]["text"] = json!("x".repeat(context_repair::MAX_CAPTURE_BYTES))
            }
            _ => unreachable!(),
        }
        assert!(
            matches!(
                RecoverySnapshot::from_records(
                    case["agent"].clone(),
                    case["events"]
                        .as_array()
                        .expect("valid synthetic fixture")
                        .clone(),
                    identity(),
                    vec![],
                    None
                ),
                Err(RepairError::Invalid(_))
            ),
            "{field}"
        );
    }
}
struct Reader {
    value: NativeHistory,
}
impl HistoryReader for Reader {
    fn identity(&self) -> ConnectionIdentity {
        identity()
    }
    fn read<'a>(&'a mut self, _: &'a RecoverySnapshot) -> HistoryReadFuture<'a> {
        Box::pin(async move { Ok(self.value.clone()) })
    }
}
struct FixtureWriter;
impl DeliveredManifestWriter for FixtureWriter {
    fn remember(
        &self,
        tx: &Transaction<'_>,
        _: &str,
        events: &[String],
    ) -> Result<(), RepairError> {
        // A test-only stand-in verifies delivery-before-promotion. Production
        // integration supplies turn_prepare's shared writer, never this fixture.
        for event in events {
            assert_eq!(
                tx.query_row(
                    "SELECT status FROM runtime_events WHERE id=?1",
                    [event],
                    |r| r.get::<_, String>(0)
                )
                .expect("valid synthetic fixture"),
                "delivered"
            );
        }
        Ok(())
    }
}
async fn clone_store(root: &std::path::Path) -> RuntimeStore {
    seed_template(root).await;
    RuntimeStore::open(Some(root))
        .await
        .expect("seeded store opens")
}
#[tokio::test]
async fn explicit_repair_skips_ready_linux_workspace_and_persists_note() {
    let dir = tempfile::tempdir().expect("valid synthetic fixture");
    let root = dir.path().join("state");
    let store = std::sync::Arc::new(clone_store(&root).await);
    store
        .transaction(|tx| {
            let mut agent =
                ContextRepairRepository::agent(tx, "agent")?.expect("seeded agent exists");
            agent["environment"] = json!("linux");
            agent["imageWorkspaceReady"] = json!(true);
            agent
                .as_object_mut()
                .expect("agent object")
                .remove("contextRepairWait");
            agent["contextRepair"] = Value::Null;
            ContextRepairRepository::put_agent(tx, &agent)
        })
        .await
        .expect("Linux workspace fixture commits");

    let result = context_repair::repair_idle(std::sync::Arc::clone(&store), "agent")
        .await
        .expect("guest history is skipped");
    assert_eq!(result["id"], "agent");
    assert_eq!(
        ContextRepairRepository::agent(&store.read().expect("store readable"), "agent")
            .expect("agent read")
            .expect("agent present")["contextRepairNote"],
        "Automatic context repair is skipped for Linux workers. The rollout stays in the guest. The next turn can proceed."
    );
}
fn state(store: &RuntimeStore) -> Value {
    let db = store.read().expect("valid synthetic fixture");
    let agent = ContextRepairRepository::agent(&db, "agent")
        .expect("valid synthetic fixture")
        .expect("valid synthetic fixture");
    let (status, turn): (String, Option<String>) = db
        .query_row(
            "SELECT status,turn_id FROM runtime_events WHERE id='event'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("valid synthetic fixture");
    json!({"agent":agent,"event":{"status":status,"turn":turn}})
}
#[tokio::test]
async fn context_repair_atomic_restart_and_lost_response_matrix() {
    if std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() != Ok("1") {
        return;
    }
    let dir = tempfile::tempdir().expect("valid synthetic fixture");
    let data = corpus();
    for cut in [
        "before-event",
        "after-event",
        "before-agent",
        "before-sync",
        "success",
        "requeue",
    ] {
        let root = dir.path().join(cut);
        let store = clone_store(&root).await;
        let before = state(&store);
        let mut case = data["recurring"][0].clone();
        if cut == "requeue" {
            case["turns"][0]["startOutcome"] = json!("not_applied");
        }
        let mut reader = Reader {
            value: history(&case),
        };
        if matches!(cut, "before-event" | "before-agent" | "before-sync") {
            let table = if cut == "before-event" {
                "runtime_events"
            } else if cut == "before-agent" {
                "runtime_agents"
            } else {
                "sync_entities"
            };
            store.transaction(move |tx| {tx.execute_batch(&format!("CREATE TRIGGER fixture_fail BEFORE UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'fixture-cut'); END"))?;Ok(())}).await.expect("valid synthetic fixture");
        }
        let result = if cut == "after-event" {
            context_repair::recover(&store, "agent", &mut reader, ManifestWriterUnavailable, 5.0)
                .await
        } else {
            context_repair::recover(&store, "agent", &mut reader, FixtureWriter, 5.0).await
        };
        let success = matches!(cut, "success" | "requeue");
        assert_eq!(result.is_ok(), success, "{cut}: {result:?}");
        if !success {
            assert_eq!(state(&store), before, "{cut}");
        }
        drop(store);
        let store = RuntimeStore::open(Some(&root))
            .await
            .expect("valid synthetic fixture");
        if !success {
            assert_eq!(state(&store), before, "restart {cut}");
            continue;
        }
        let committed = state(&store);
        assert_eq!(
            committed["event"]["status"],
            if cut == "requeue" {
                "pending"
            } else {
                "delivered"
            }
        );
        assert!(committed["agent"].get("contextRepairWait").is_none());
        let retry = context_repair::recover(&store, "agent", &mut reader, FixtureWriter, 6.0)
            .await
            .expect("valid synthetic fixture");
        assert!(retry.is_empty());
        assert_eq!(
            state(&store),
            committed,
            "lost response does not repeat effects"
        );
    }
}

struct RacingReader<'a> {
    store: &'a RuntimeStore,
    history: NativeHistory,
    change: &'static str,
}
impl HistoryReader for RacingReader<'_> {
    fn identity(&self) -> ConnectionIdentity {
        identity()
    }
    fn read<'a>(&'a mut self, _: &'a RecoverySnapshot) -> HistoryReadFuture<'a> {
        Box::pin(async move {
            let change = self.change;
            self.store
                .transaction(move |tx| {
                    match change {
                        "event" => {
                            tx.execute(
                                "UPDATE runtime_events SET text='changed in flight' WHERE id='event'",
                                [],
                            )?;
                        }
                        "agent" => {
                            let mut a = ContextRepairRepository::agent(tx, "agent")?
                                .expect("valid synthetic fixture");
                            a["epoch"] = json!(2);
                            ContextRepairRepository::put_agent(tx, &a)?;
                        }
                        _ => unreachable!(),
                    }
                    Ok(())
                })
                .await?;
            Ok(self.history.clone())
        })
    }
}
#[tokio::test]
async fn context_repair_changed_snapshot_never_acknowledges() {
    if std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() != Ok("1") {
        return;
    }
    let dir = tempfile::tempdir().expect("valid synthetic fixture");
    let data = corpus();
    for change in ["event", "agent"] {
        let root = dir.path().join(change);
        let store = clone_store(&root).await;
        let mut reader = RacingReader {
            store: &store,
            history: history(&data["recurring"][0]),
            change,
        };
        let result = context_repair::recover(&store, "agent", &mut reader, FixtureWriter, 5.0)
            .await
            .expect("valid synthetic fixture");
        assert!(
            matches!(&result[0], InputDecision::Waiting { .. }),
            "{change}"
        );
        assert_eq!(state(&store)["event"]["status"], "uncertain");
        assert!(state(&store)["agent"]["contextRepairWait"].is_object());
    }
}

#[test]
fn context_repair_historical_secondary_preserves_newer_pending_input() {
    let data = corpus();
    let batch = &data["batches"][0];
    let mut agent = batch["agent"].clone();
    agent["startAttempt"]["events"] = json!(["newer"]);
    agent["startAttempt"]["submitted"] = json!(false);
    agent["contextRepairWait"]["source"]["attemptId"] = json!("attempt");
    agent["contextRepairWait"]["events"] = json!(["newer"]);
    let members = batch["source"]["members"].as_array().expect("members");
    let events = vec![
        members
            .iter()
            .find(|row| row["id"] == "event")
            .expect("secondary")
            .clone(),
    ];
    let pending = vec![
        json!({"id":"newer","agent":"agent","epoch":1,"status":"pending","turn_id":null,"text":"new input"}),
    ];
    let snapshot = RecoverySnapshot::from_records(
        agent.clone(),
        events.clone(),
        identity(),
        vec![batch["source"].clone()],
        Some(pending),
    )
    .expect("legacy checked capture");
    let history = NativeHistory::from_records(
        identity(),
        "thread".into(),
        batch["turns"].as_array().expect("turns").clone(),
        Some("idle".into()),
        batch["operations"].as_array().expect("journal").clone(),
    )
    .expect("history");
    assert_eq!(
        serde_json::to_value(context_repair::decide(&snapshot, &history)).expect("decisions"),
        json!([{"id":"event","decision":"delivered","turnId":"native-turn"}])
    );
    let stale = RecoverySnapshot::from_records(
        agent,
        events,
        identity(),
        vec![batch["source"].clone()],
        None,
    )
    .expect("absent pending proof is held");
    assert!(matches!(
        &context_repair::decide(&stale, &history)[0],
        InputDecision::Waiting { .. }
    ));
}

#[test]
fn context_repair_absent_schema_is_named_without_writes() {
    let data = corpus();
    for table in [
        "runtime_event_meta",
        "runtime_items",
        "runtime_item_fulltext",
        "runtime_completed_turns",
    ] {
        let mut db = memory_fixture(&data["recurring"][0]);
        db.execute_batch(&format!("DROP TABLE {table}"))
            .expect("synthetic schema removal");
        let tx = db.transaction().expect("transaction");
        assert!(matches!(
            context_repair::capture(&tx, "agent", identity()),
            Err(RepairError::MissingSchema)
        ));
        assert_eq!(
            tx.query_row(
                "SELECT status FROM runtime_events WHERE id='event'",
                [],
                |row| row.get::<_, String>(0)
            )
            .expect("event retained"),
            "uncertain"
        );
    }
}

fn memory_fixture(case: &Value) -> rusqlite::Connection {
    let db = rusqlite::Connection::open_in_memory().expect("memory fixture");
    db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
        CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,epoch INTEGER,kind TEXT,text TEXT,status TEXT,created REAL,turn_id TEXT,error TEXT);
        CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT);
        CREATE TABLE runtime_completed_turns(id TEXT PRIMARY KEY);
        CREATE TABLE runtime_items(id TEXT PRIMARY KEY,agent TEXT,created REAL,record TEXT);
        CREATE TABLE runtime_item_fulltext(id TEXT PRIMARY KEY,body TEXT);
        CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id));
        CREATE TABLE sync_documents(seq INTEGER);
        CREATE TABLE sync_versions(seq INTEGER);").expect("fixture schema");
    db.execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
        .expect("volatile agent table creates");
    ContextRepairRepository::put_agent(&db, &case["agent"]).expect("record stores");
    let row = &case["events"][0];
    db.execute(
        "INSERT INTO runtime_events VALUES(?1,?2,?3,?4,?5,?6,?7,?8,?9)",
        rusqlite::params![
            row["id"].as_str(),
            row["agent"].as_str(),
            row["epoch"].as_i64(),
            row["kind"].as_str(),
            row["text"].as_str(),
            row["status"].as_str(),
            row["created"].as_f64(),
            row["turn_id"].as_str(),
            row["error"].as_str()
        ],
    )
    .expect("row stores");
    db.execute(
        "INSERT INTO sync_entities VALUES('agent','agent',1,'fixture',?1,0)",
        [json!({"collection":"agent","id":"agent","value":case["agent"]}).to_string()],
    )
    .expect("projection fixture");
    db
}
#[test]
fn context_repair_transaction_failpoints_and_replay_fast_witness() {
    let data = corpus();
    let base = &data["recurring"][0];
    for cut in [
        "before-event",
        "after-event",
        "before-agent",
        "before-sync",
        "changed",
        "success",
        "requeue",
    ] {
        let mut case = base.clone();
        if cut == "requeue" {
            case["turns"][0]["startOutcome"] = json!("not_applied");
        }
        let mut db = memory_fixture(&case);
        let frozen = {
            let tx = db.transaction().expect("capture transaction");
            context_repair::capture(&tx, "agent", identity()).expect("capture")
        };
        if cut == "changed" {
            db.execute("UPDATE runtime_events SET text='changed'", [])
                .expect("interleaved event edit");
        }
        if let Some(table) = match cut {
            "before-event" => Some("runtime_events"),
            "before-agent" => Some("runtime_agents"),
            "before-sync" => Some("sync_entities"),
            _ => None,
        } {
            db.execute_batch(&format!("CREATE TRIGGER fixture_fail BEFORE UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'fixture-cut'); END")).expect("failpoint");
        }
        let history = history(&case);
        let tx = db.transaction().expect("reconcile transaction");
        let result = if cut == "after-event" {
            context_repair::reconcile(
                &tx,
                &frozen,
                &history,
                identity(),
                &ManifestWriterUnavailable,
                5.0,
            )
        } else {
            context_repair::reconcile(&tx, &frozen, &history, identity(), &FixtureWriter, 5.0)
        };
        let success = matches!(cut, "success" | "requeue" | "changed");
        assert_eq!(result.is_ok(), success, "{cut}: {result:?}");
        if success {
            tx.commit().expect("commit");
        } else {
            drop(tx);
        }
        let status: String = db
            .query_row(
                "SELECT status FROM runtime_events WHERE id='event'",
                [],
                |r| r.get(0),
            )
            .expect("stored event");
        assert_eq!(
            status,
            match cut {
                "success" => "delivered",
                "requeue" => "pending",
                _ => "uncertain",
            },
            "{cut}"
        );
        let agent = ContextRepairRepository::agent(&db, "agent")
            .expect("read agent")
            .expect("agent exists");
        if matches!(cut, "success" | "requeue") {
            assert!(agent.get("contextRepairWait").is_none());
            let tx = db.transaction().expect("replay transaction");
            let replay =
                context_repair::reconcile(&tx, &frozen, &history, identity(), &FixtureWriter, 6.0)
                    .expect("replay refuses stale capture");
            assert!(matches!(&replay[0], InputDecision::Waiting { .. }));
        } else {
            assert_eq!(
                agent, case["agent"],
                "{cut}: rollback or changed snapshot preserves agent"
            );
        }
    }
}

#[test]
fn context_repair_empty_restart_does_not_queue_an_active_turn() {
    let data = corpus();
    for active in [false, true] {
        let mut case = data["recurring"][0].clone();
        case["agent"]["inFlight"] = json!(active);
        case["agent"]["status"] = json!(if active { "running" } else { "waiting" });
        case["agent"]["restartRecovery"] = json!({"stage":"held","autoWake":true,"epoch":1,"accountKey":"default","threadId":"thread","startAttempt":{"events":[]}});
        let mut db = memory_fixture(&case);
        db.execute(
            "UPDATE runtime_events SET status='delivered',turn_id='old-turn'",
            [],
        )
        .expect("already delivered");
        let tx = db.transaction().expect("transaction");
        let snapshot = context_repair::capture(&tx, "agent", identity()).expect("restart capture");
        let result = context_repair::reconcile(
            &tx,
            &snapshot,
            &history(&case),
            identity(),
            &FixtureWriter,
            5.0,
        )
        .expect("no input needs recovery");
        assert!(result.is_empty());
        tx.commit().expect("commit");
        let agent = ContextRepairRepository::agent(&db, "agent")
            .expect("agent read")
            .expect("agent");
        assert_eq!(agent["status"], if active { "running" } else { "queued" });
        assert_eq!(agent["inFlight"], active);
        assert_eq!(agent["restartRecovery"]["stage"], "finished");
    }
}

#[test]
fn context_repair_claude_requeue_belongs_to_frozen_retry() {
    let data = corpus();
    let mut case = data["recurring"][0].clone();
    case["agent"]["provider"] = json!("claude");
    case["turns"][0]["startOutcome"] = json!("not_applied");
    assert!(matches!(
        &context_repair::decide(&snapshot(&case), &history(&case))[0],
        InputDecision::Waiting { .. }
    ));
}
