use rusqlite::{Connection, Transaction};
use serde_json::{Value, json};
use std::{path::Path, process::Command, sync::OnceLock};
use studio_runtime_services::workspace_operations::ProviderTicket;
use studio_runtime_services::{
    context_repair::{HistoryRewrite, RepairError, RepairEventSummary},
    native_actions::*,
    native_review::{self, ReviewPlan},
};
#[path = "support/prepared_python.rs"]
mod prepared_python;

fn corpus() -> &'static Value {
    static CORPUS: OnceLock<Value> = OnceLock::new();
    CORPUS.get_or_init(|| {
        let source = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let output = Command::new(prepared_python::prepared_python(&source))
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/wave7_p11_native/oracle.py"))
            .output()
            .expect("start real Python oracle");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        serde_json::from_slice(&output.stdout).expect("corpus JSON")
    })
}
struct Gate;
impl Admission for Gate {
    fn check(&self, _: &Transaction<'_>, _: &Value, _: Action) -> Result<()> {
        Ok(())
    }
}
struct Accept;
impl Acceptance for Accept {
    fn apply(&self, tx: &Transaction<'_>, _: &NativeRequest, _: &Delivery) -> Result<()> {
        tx.execute("INSERT INTO effects VALUES('accepted')", [])?;
        Ok(())
    }
}
fn db(case: &Value) -> Connection {
    let db = Connection::open_in_memory().expect("fixture db");
    db.execute_batch("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE runtime_native_action_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,receipt TEXT NOT NULL,outcome TEXT NOT NULL); CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,signature TEXT NOT NULL,result TEXT NOT NULL); CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY,record TEXT NOT NULL); CREATE TABLE effects(value TEXT);").expect("Python table shapes");
    db.execute(
        "INSERT INTO runtime_agents VALUES('lead',?1)",
        [case["agent"].to_string()],
    )
    .expect("Python-written agent");
    db
}
fn row(db: &Connection) -> Value {
    db.query_row(
        "SELECT id,signature,receipt,outcome FROM runtime_native_action_receipts",
        [],
        |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, String>(3)?,
            ))
        },
    )
    .map(|(id, sig, r, o)| {
        json!([
            id,
            sig,
            serde_json::from_str::<Value>(&r).expect("receipt"),
            serde_json::from_str::<Value>(&o).expect("outcome")
        ])
    })
    .expect("saved row")
}
fn actor(db: &Connection) -> Value {
    serde_json::from_str(
        &db.query_row(
            "SELECT record FROM runtime_agents WHERE id='lead'",
            [],
            |r| r.get::<_, String>(0),
        )
        .expect("agent"),
    )
    .expect("agent JSON")
}
fn ticket() -> ProviderTicket {
    ProviderTicket {
        operation: "attempt".into(),
        agent: "lead".into(),
        epoch: 3,
        account: "default".into(),
        thread: Some("thread".into()),
        connection: "connection".into(),
        generation: 1,
        request: "action-request".into(),
    }
}
fn reserve_case(db: &mut Connection, case: &Value) {
    let request: Request = serde_json::from_value(case["request"].clone()).expect("request");
    let tx = db.transaction().expect("begin");
    reserve(&tx, &request, "attempt", 1234567.5, &Gate).expect("reserve");
    tx.commit().expect("commit");
}
#[test]
fn complete_python_native_admission_rows_match() {
    for case in corpus()["actions"].as_array().expect("actions") {
        let mut db = db(case);
        reserve_case(&mut db, case);
        assert_eq!(row(&db), case["row"]);
        assert_eq!(actor(&db), case["after"]);
        let before = row(&db);
        let request: Request = serde_json::from_value(case["request"].clone()).expect("request");
        let tx = db.transaction().expect("begin");
        reserve(&tx, &request, "new-attempt", 555.0, &Gate)
            .expect("replay old result despite fresh time");
        tx.commit().expect("commit");
        assert_eq!(row(&db), before);
    }
}
#[test]
fn crash_matrix_never_reclaims_committed_handoff() {
    let case = &corpus()["actions"][0];
    // Each durable boundary is interrupted on both sides by transaction rollback
    // or commit. Reopening the file proves the hold survives process lifetime.
    for stop in 0..6 {
        let temp = tempfile::tempdir().expect("tmpfs fixture");
        let path = temp.path().join("state.sqlite3");
        let mut db = db(case);
        if stop == 0 {
            let tx = db.transaction().expect("begin");
            let req = serde_json::from_value(case["request"].clone()).expect("request");
            reserve(&tx, &req, "attempt", 1.0, &Gate).expect("reserve");
            drop(tx);
            assert_eq!(
                db.query_row(
                    "SELECT count(*) FROM runtime_native_action_receipts",
                    [],
                    |r| r.get::<_, u32>(0)
                )
                .expect("count"),
                0
            );
            continue;
        }
        reserve_case(&mut db, case);
        if stop == 1 {
            let tx = db.transaction().expect("begin");
            claim(&tx, "action-request", &ticket()).expect("claim");
            drop(tx);
            assert_eq!(row(&db)[3]["status"], "pending");
            continue;
        }
        let tx = db.transaction().expect("begin");
        let request = claim(&tx, "action-request", &ticket()).expect("claim");
        tx.commit().expect("commit");
        if stop >= 3 {
            let tx = db.transaction().expect("begin");
            let receipt = TransportReceipt {
                ticket: request.ticket.clone(),
                delivery: Delivery::Written(json!({"ok":true})),
            };
            settle(&tx, "action-request", &receipt, &Accept).expect("settle");
            if stop == 3 {
                drop(tx);
            } else {
                tx.commit().expect("commit");
            }
        }
        db.execute("VACUUM INTO ?1", [path.to_str().expect("path")])
            .expect("persist restart fixture");
        drop(db);
        let mut db = Connection::open(path).expect("restart");
        let tx = db.transaction().expect("begin");
        assert!(matches!(
            claim(&tx, "action-request", &ticket()),
            Err(NativeError::Unknown)
        ));
        drop(tx);
        assert_eq!(
            row(&db)[3]["status"],
            if stop >= 4 { "acknowledged" } else { "unknown" }
        );
        if stop >= 4 {
            let tx = db.transaction().expect("begin");
            settle(
                &tx,
                "action-request",
                &TransportReceipt {
                    ticket: request.ticket,
                    delivery: Delivery::Written(json!({"ok":true})),
                },
                &Accept,
            )
            .expect("duplicate receipt");
            tx.commit().expect("commit");
        }
        assert_eq!(
            db.query_row("SELECT count(*) FROM effects", [], |r| r.get::<_, u32>(0))
                .expect("count"),
            u32::from(stop >= 4)
        );
    }
}
#[test]
fn changed_payload_and_foreign_receipts_leave_intent_held() {
    let case = &corpus()["actions"][0];
    let mut db = db(case);
    reserve_case(&mut db, case);
    let tx = db.transaction().expect("begin");
    let mut req: Request = serde_json::from_value(case["request"].clone()).expect("request");
    req.action = Action::Review;
    assert!(matches!(
        reserve(&tx, &req, "next", 1.0, &Gate),
        Err(NativeError::Conflict)
    ));
    drop(tx);
    let tx = db.transaction().expect("begin");
    claim(&tx, "action-request", &ticket()).expect("claim");
    tx.commit().expect("commit");
    for field in 0..7 {
        let mut wrong = ticket();
        match field {
            0 => wrong.agent = "foreign".into(),
            1 => wrong.epoch += 1,
            2 => wrong.account = "foreign".into(),
            3 => wrong.connection = "foreign".into(),
            4 => wrong.request = "foreign".into(),
            5 => wrong.generation += 1,
            _ => wrong.operation = "foreign".into(),
        };
        let tx = db.transaction().expect("begin");
        assert!(
            settle(
                &tx,
                "action-request",
                &TransportReceipt {
                    ticket: wrong,
                    delivery: Delivery::Written(json!({}))
                },
                &Accept
            )
            .is_err()
        );
        drop(tx);
        assert_eq!(row(&db)[3]["status"], "unknown");
    }
}
#[test]
fn review_worker_identity_and_saved_response_match_python() {
    for case in corpus()["reviews"].as_array().expect("reviews") {
        let plan = ReviewPlan::from_records(
            case["actor"].clone(),
            case["root"].clone(),
            case["args"].clone(),
            case["key"].as_str().expect("key").into(),
            case["directory"].as_str().expect("directory").into(),
        )
        .expect("review plan");
        let mut actual = plan.spec().clone();
        let mut expected = case["spec"].clone();
        for value in [&mut actual, &mut expected] {
            let text = value["prompt"]
                .as_str()
                .expect("prompt")
                .strip_prefix("Run a native code review: ")
                .expect("generated prefix");
            value["prompt"] = serde_json::from_str(text).expect("generated target JSON");
        }
        assert_eq!(actual, expected);
        assert_eq!(
            plan.existing(&case["child"]).expect("saved response"),
            case["result"]
        );
        let mut child = case["child"].clone();
        child["nativeReview"]["actorId"] = json!("foreign");
        assert!(plan.existing(&child).is_err());
        assert_eq!(
            native_review::target(&case["args"]).expect("target"),
            case["args"]["target"]
        );
    }
}
#[test]
fn review_request_ids_match_python_whitespace_and_control_validation() {
    for request_id in [" padded", "padded ", "line\nbreak", "tab\tkey"] {
        let args = json!({"request_id":request_id});
        assert!(
            native_review::target(&args).is_err(),
            "accepted {request_id:?}"
        );
    }
    assert!(native_review::target(&json!({"request_id":"valid-key"})).is_ok());
}
struct Summaries(Value);
impl RepairEventSummary for Summaries {
    fn summarize(&self, event: &Value) -> std::result::Result<String, RepairError> {
        self.0[event["id"].as_str().ok_or(RepairError::Invalid("id"))?]
            .as_str()
            .map(str::to_owned)
            .ok_or(RepairError::RewriteUnavailable)
    }
}
#[test]
fn full_scoped_history_copy_matches_real_python_rewriter() {
    for case in corpus()["rewrites"].as_array().expect("rewrites") {
        let snapshot = HistoryRewrite::from_records(
            serde_json::from_value(case["records"].clone()).expect("records"),
            serde_json::from_value(case["events"].clone()).expect("events"),
            "thread".into(),
            "turn".into(),
        )
        .expect("checked rewrite");
        let actual = snapshot.rewrite("import", &Summaries(case["summaries"].clone()));
        if case["result"]["error"] == true {
            assert!(actual.is_err());
            continue;
        }
        let (records, ids) = actual.expect("rewrite");
        assert_eq!(json!(ids), case["result"]["ids"]);
        if !ids.is_empty() {
            assert_eq!(json!(records), case["result"]["records"]);
        } else {
            let mut original = case["records"].clone();
            original[0]["payload"]["id"] = json!("import");
            assert_eq!(json!(records), original);
        }
    }
}
#[test]
fn native_command_input_and_process_item_scope_match_python() {
    for case in corpus()["commands"].as_array().expect("commands") {
        let snapshot = CommandSnapshot::from_records(case["task"].clone(), case["agent"].clone())
            .expect("checked command");
        assert_eq!(
            snapshot
                .input(case["text"].as_str().expect("text"))
                .expect("instruction"),
            case["result"]["text"]
        );
        assert!(
            !snapshot
                .listed(&json!({"data":[{"itemId":"foreign","processId":"17"}]}))
                .expect("page")
        );
        assert!(
            snapshot
                .listed(&json!({"data":[{"itemId":"item","processId":"17"}]}))
                .expect("page")
        );
    }
}
#[test]
fn native_command_cancel_list_terminate_and_replay_match_python() {
    for case in corpus()["cancellations"].as_array().expect("cancellations") {
        let mut db = db(case);
        db.execute(
            "INSERT INTO runtime_tasks VALUES(?1,?2)",
            [
                case["task"]["id"].as_str().expect("task id"),
                case["task"].to_string().as_str(),
            ],
        )
        .expect("task");
        let snapshot = CommandSnapshot::from_records(case["task"].clone(), case["agent"].clone())
            .expect("snapshot");
        let mut cancel_ticket = ticket();
        cancel_ticket.operation = case["task"]["id"].as_str().expect("task id").to_owned();
        cancel_ticket.request = "cancel-corpus".into();
        let mut list_ticket = cancel_ticket.clone();
        list_ticket.request.push_str(":list");
        let list = TransportReceipt {
            ticket: list_ticket,
            delivery: Delivery::Written(
                json!({"data":case["pages"].as_array().expect("pages").last().expect("last page")}),
            ),
        };
        let tx = db.transaction().expect("begin");
        let CommandCancelClaim::Send(request) =
            command_cancel_claim(&tx, &snapshot, &cancel_ticket, &list).expect("claim")
        else {
            panic!("first cancellation must send")
        };
        tx.commit().expect("commit intent");
        let python_terminate = case["calls"]
            .as_array()
            .expect("calls")
            .last()
            .expect("terminate")[1]
            .clone();
        assert_eq!(request.params, python_terminate);
        let settled = TransportReceipt {
            ticket: request.ticket,
            delivery: Delivery::Written(case["result"].clone()),
        };
        let tx = db.transaction().expect("begin");
        assert_eq!(
            command_cancel_settle(&tx, &settled).expect("settle"),
            case["result"]
        );
        tx.commit().expect("commit result");
        let tx = db.transaction().expect("begin");
        assert!(matches!(
            command_cancel_claim(&tx, &snapshot, &cancel_ticket, &list),
            Ok(CommandCancelClaim::Replay(ref value)) if value == &case["result"]
        ));
    }
}
struct Hanging;
impl RetainedSession for Hanging {
    fn request<'a>(&'a self, _: &'a NativeRequest, _: tokio::time::Instant) -> NativeFuture<'a> {
        Box::pin(std::future::pending())
    }
}
#[tokio::test(start_paused = true)]
async fn deadline_holds_unknown_and_expired_request_is_not_sent() {
    let request = NativeRequest {
        ticket: ticket(),
        method: "thread/compact/start".into(),
        params: json!({"threadId":"thread"}),
    };
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(10);
    assert_eq!(
        send(&Hanging, &request, deadline)
            .await
            .expect("bounded timeout")
            .delivery,
        Delivery::Unknown
    );
    assert_eq!(
        send(&Hanging, &request, deadline)
            .await
            .expect("expired")
            .delivery,
        Delivery::NotSent
    );
}

struct Creator(Value);
impl native_review::ReviewCreator for Creator {
    fn child(&self, _: &Transaction<'_>, _: &Value, _: &Value) -> Result<Value> {
        let mut child = self.0.clone();
        child.as_object_mut().expect("child").remove("nativeReview");
        Ok(child)
    }
    fn receipt(&self, tx: &Transaction<'_>, _: &str, _: &Value) -> Result<()> {
        tx.execute("INSERT INTO effects VALUES('review-created')", [])?;
        Ok(())
    }
}
#[test]
fn review_reservation_rolls_back_and_replays_without_second_child() {
    let case = &corpus()["reviews"][0];
    let plan = ReviewPlan::from_records(
        case["actor"].clone(),
        case["root"].clone(),
        case["args"].clone(),
        "review-request".into(),
        case["directory"].as_str().expect("directory").into(),
    )
    .expect("plan");
    let mut db = db(&json!({"agent":case["actor"]}));
    {
        let tx = db.transaction().expect("begin");
        native_review::reserve(&tx, &plan, &Creator(case["child"].clone())).expect("reserve");
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM runtime_agents", [], |r| r
            .get::<_, u32>(0))
            .expect("count"),
        1
    );
    for _ in 0..2 {
        let tx = db.transaction().expect("begin");
        assert_eq!(
            native_review::reserve(&tx, &plan, &Creator(case["child"].clone()))
                .expect("reserve/replay"),
            case["result"]
        );
        tx.commit().expect("commit");
    }
    assert_eq!(
        db.query_row("SELECT count(*) FROM effects", [], |r| r.get::<_, u32>(0))
            .expect("count"),
        1
    );
    let raw: String = db
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [native_review::worker_id("review-request")],
            |r| r.get(0),
        )
        .expect("child");
    assert_eq!(
        serde_json::from_str::<Value>(&raw).expect("JSON"),
        case["child"]
    );
}
#[test]
fn fork_persisted_ticket_holds_and_accepts_only_exact_receipt() {
    use studio_runtime_services::thread_fork_runtime as fork;
    let case = &corpus()["forks"][0];
    let mut db = db(case);
    db.execute_batch(
        "CREATE TABLE runtime_workspace_operations(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
    )
    .expect("operation table");
    db.execute(
        "INSERT INTO runtime_workspace_operations VALUES('branch-request',?1)",
        [case["operation"].to_string()],
    )
    .expect("Python operation");
    let mut ticket = ticket();
    ticket.operation = "branch-request".into();
    {
        let tx = db.transaction().expect("begin");
        fork::claim(
            &tx,
            &ticket,
            json!({"threadId":"thread","lastTurnId":"turn"}),
        )
        .expect("claim");
    }
    let tx = db.transaction().expect("begin");
    fork::claim(
        &tx,
        &ticket,
        json!({"threadId":"thread","lastTurnId":"turn"}),
    )
    .expect("claim after rollback");
    tx.commit().expect("commit");
    let tx = db.transaction().expect("begin");
    assert!(matches!(
        fork::claim(
            &tx,
            &ticket,
            json!({"threadId":"thread","lastTurnId":"changed"})
        ),
        Err(NativeError::Conflict)
    ));
    drop(tx);
    let tx = db.transaction().expect("begin");
    assert!(matches!(
        fork::claim(
            &tx,
            &ticket,
            json!({"threadId":"thread","lastTurnId":"turn"})
        ),
        Err(NativeError::Unknown)
    ));
    drop(tx);
    let result = json!({"thread":{"id":"forked"},"model":"gpt-6"});
    let mut wrong = ticket.clone();
    wrong.connection = "other".into();
    let tx = db.transaction().expect("begin");
    assert!(
        fork::settle(
            &tx,
            &TransportReceipt {
                ticket: wrong,
                delivery: Delivery::Written(result.clone())
            }
        )
        .is_err()
    );
    drop(tx);
    {
        let tx = db.transaction().expect("begin");
        fork::settle(
            &tx,
            &TransportReceipt {
                ticket: ticket.clone(),
                delivery: Delivery::Written(result.clone()),
            },
        )
        .expect("settle before crash");
    }
    for _ in 0..2 {
        let tx = db.transaction().expect("begin");
        assert_eq!(
            fork::settle(
                &tx,
                &TransportReceipt {
                    ticket: ticket.clone(),
                    delivery: Delivery::Written(result.clone())
                }
            )
            .expect("settle/replay"),
            result
        );
        tx.commit().expect("commit");
    }
    assert_eq!(
        actor(&db),
        case["agent"],
        "fork receipt cannot itself mutate local workspace"
    );
}
struct Enqueue;
impl CommandAdmission for Enqueue {
    fn enqueue(
        &self,
        tx: &Transaction<'_>,
        _: &Value,
        key: &str,
        instruction: &str,
    ) -> Result<Value> {
        tx.execute("INSERT INTO effects VALUES('queued')", [])?;
        Ok(json!({"id":key,"text":instruction}))
    }
}
#[test]
fn commands_commit_before_send_and_reject_changed_input() {
    let case = &corpus()["commands"][0];
    let mut db = db(case);
    db.execute(
        "INSERT INTO runtime_tasks VALUES('task',?1)",
        [case["task"].to_string()],
    )
    .expect("task");
    let snapshot = CommandSnapshot::from_records(case["task"].clone(), case["agent"].clone())
        .expect("snapshot");
    {
        let tx = db.transaction().expect("begin");
        command_input(&tx, "input-key", &snapshot, "one", &Enqueue).expect("enqueue rollback");
    }
    for _ in 0..2 {
        let tx = db.transaction().expect("begin");
        command_input(&tx, "input-key", &snapshot, "one", &Enqueue).expect("enqueue/replay");
        tx.commit().expect("commit");
    }
    let tx = db.transaction().expect("begin");
    assert!(matches!(
        command_input(&tx, "input-key", &snapshot, "two", &Enqueue),
        Err(NativeError::Conflict)
    ));
    drop(tx);
    assert_eq!(
        db.query_row("SELECT count(*) FROM effects", [], |r| r.get::<_, u32>(0))
            .expect("count"),
        1
    );
    let mut t = ticket();
    t.operation = "task".into();
    t.request = "cancel-key".into();
    let mut list = t.clone();
    list.request = "cancel-key:list".into();
    let receipt = TransportReceipt {
        ticket: list,
        delivery: Delivery::Written(json!({"data":[{"itemId":"item","processId":"17"}]})),
    };
    let tx = db.transaction().expect("begin");
    let CommandCancelClaim::Send(request) =
        command_cancel_claim(&tx, &snapshot, &t, &receipt).expect("persist handoff")
    else {
        panic!("first cancel must send")
    };
    tx.commit().expect("commit");
    let tx = db.transaction().expect("begin");
    assert!(matches!(
        command_cancel_claim(&tx, &snapshot, &t, &receipt),
        Err(NativeError::Unknown)
    ));
    drop(tx);
    let response = TransportReceipt {
        ticket: request.ticket,
        delivery: Delivery::Written(json!({"terminated":true})),
    };
    {
        let tx = db.transaction().expect("begin");
        command_cancel_settle(&tx, &response).expect("rollback response");
    }
    for _ in 0..2 {
        let tx = db.transaction().expect("begin");
        assert_eq!(
            command_cancel_settle(&tx, &response).expect("exact receipt"),
            json!({"terminated":true})
        );
        tx.commit().expect("commit");
    }
    let tx = db.transaction().expect("begin");
    assert!(matches!(
        command_cancel_claim(&tx, &snapshot, &t, &receipt),
        Ok(CommandCancelClaim::Replay(ref value)) if value == &json!({"terminated":true})
    ));
}

#[test]
fn stored_native_action_errors_name_the_table_and_row() {
    let case = &corpus()["actions"][0];
    let db = db(case);
    assert!(matches!(
        checked_agent(&json!({"id":"broken-agent"})),
        Err(NativeError::InvalidStored { table: "runtime_agents", ref id, .. }) if id == "broken-agent"
    ));
    db.execute(
        "INSERT INTO runtime_native_action_receipts(id,signature,receipt,outcome) VALUES('broken-receipt','sig','{','{}')",
        [],
    )
    .expect("corrupt receipt fixture");
    let tx = db.unchecked_transaction().expect("begin");
    assert!(matches!(
        claim(&tx, "broken-receipt", &ticket()),
        Err(NativeError::InvalidStored { table: "runtime_native_action_receipts", ref id, .. }) if id == "broken-receipt"
    ));
}
#[test]
fn malformed_agent_receipt_and_scope_fail_without_panicking() {
    let case = &corpus()["actions"][0];
    for (key, value) in [
        ("epoch", json!(-1)),
        ("epoch", json!("3")),
        ("autoWake", json!("yes")),
        ("threadId", json!([])),
        ("status", json!(17)),
    ] {
        let mut agent = case["agent"].clone();
        agent[key] = value;
        assert!(
            Snapshot::from_records(agent, case["row"][2].clone(), case["row"][3].clone()).is_err(),
            "{key}"
        );
    }
    for (key, value) in [
        ("epoch", json!(-1)),
        ("threadId", json!("")),
        ("acceptedAt", json!(null)),
        ("status", json!("other")),
    ] {
        let mut receipt = case["row"][2].clone();
        receipt[key] = value;
        assert!(
            Snapshot::from_records(case["agent"].clone(), receipt, case["row"][3].clone()).is_err(),
            "{key}"
        );
    }
}

struct Release;
impl Acceptance for Release {
    fn apply(&self, tx: &Transaction<'_>, _: &NativeRequest, delivery: &Delivery) -> Result<()> {
        assert_eq!(delivery, &Delivery::NotSent);
        tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.status','idle','$.inFlight',json('false')) WHERE id='lead'",[])?;
        Ok(())
    }
}
#[test]
fn definitely_unsent_action_releases_owner_in_same_commit() {
    let case = &corpus()["actions"][0];
    let mut db = db(case);
    reserve_case(&mut db, case);
    let tx = db.transaction().expect("begin");
    claim(&tx, "action-request", &ticket()).expect("claim");
    tx.commit().expect("commit");
    let tx = db.transaction().expect("begin");
    assert_eq!(
        settle(
            &tx,
            "action-request",
            &TransportReceipt {
                ticket: ticket(),
                delivery: Delivery::NotSent
            },
            &Release
        )
        .expect("release"),
        Outcome::Failed { error: None }
    );
    tx.commit().expect("commit");
    assert_eq!(actor(&db)["inFlight"], false);
    assert_eq!(row(&db)[3]["status"], "failed");
}

#[test]
fn review_receipt_survives_later_delegation_and_default_changes() {
    let case = &corpus()["reviews"][0];
    let mut actor = case["actor"].clone();
    actor["concurrency"] = json!(0);
    actor["daybreakEnabled"] = json!(true);
    actor["reviewDefaults"] = json!({"model":"new-model","effort":null});
    let plan = ReviewPlan::from_records(
        actor.clone(),
        actor,
        case["args"].clone(),
        "review-request".into(),
        case["directory"].as_str().expect("directory").into(),
    )
    .expect("replay plan");
    assert_eq!(
        plan.existing(&case["child"])
            .expect("immutable saved response"),
        case["result"]
    );
}

#[test]
fn python_failed_and_unknown_receipt_errors_are_not_lost() {
    for case in corpus()["outcomes"].as_array().expect("outcomes") {
        let snapshot = Snapshot::from_records(
            case["agent"].clone(),
            case["receipt"].clone(),
            case["outcome"].clone(),
        )
        .expect("Python outcome");
        assert_eq!(
            serde_json::to_value(snapshot.outcome()).expect("outcome"),
            case["outcome"]
        );
    }
}
