//! Scripted provider consumer: real SQLite and Python-written actors, no provider.
#![allow(clippy::unwrap_used)]
use crate::prepared_python_support as prepared_python;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::{Command, Stdio},
    sync::OnceLock,
};
use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId, ResponseBody};
use studio_runtime_services::provider_callbacks::*;
use studio_runtime_store::RuntimeStore;

type CaseFuture = std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send>>;
type Case = (&'static str, fn() -> CaseFuture);

struct Template {
    files: prepared_python::StateTemplate,
    value: Value,
}
fn template() -> &'static Template {
    static TEMPLATE: OnceLock<Template> = OnceLock::new();
    TEMPLATE.get_or_init(|| {
        let scratch = tempfile::tempdir().unwrap();
        let script =
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/provider_callbacks/oracle.py");
        let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let environment = prepared_python::IsolatedTestEnvironment::new();
        let mut command = Command::new(prepared_python::prepared_python(&source));
        environment.apply(&mut command);
        command
            .arg(&script)
            .arg(scratch.path())
            .stdin(Stdio::null());
        let output = prepared_python::output_with_timeout(
            &mut command,
            &script,
            prepared_python::PYTHON_ORACLE_TIMEOUT,
        )
        .unwrap_or_else(|error| panic!("provider callbacks Python oracle: {error}"));
        assert!(
            output.status.success(),
            "oracle: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: Value = serde_json::from_slice(&output.stdout).unwrap();
        Template {
            files: prepared_python::StateTemplate::capture(&PathBuf::from(
                value["template"].as_str().unwrap(),
            )),
            value,
        }
    })
}
async fn fixture() -> (
    tempfile::TempDir,
    std::sync::Arc<RuntimeStore>,
    CallbackContext,
) {
    let t = template();
    let directory = tempfile::tempdir().unwrap();
    t.files.write(directory.path());
    let store = std::sync::Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let actor = &t.value["agents"][0];
    let context = CallbackContext {
        agent: actor["id"].as_str().unwrap().into(),
        account: "default".into(),
        connection: "connection".into(),
        generation: 7,
        epoch: 3,
        thread: "thread-0".into(),
        turn: "turn-0".into(),
    };
    (directory, store, context)
}
fn request(method: Method, id: RequestId, params: Value) -> Frame {
    Frame::Request(RequestFrame {
        id,
        method,
        params: Some(params),
        fields: Default::default(),
    })
}
fn call(context: &CallbackContext, id: &str, name: &str, args: Value) -> Frame {
    request(
        Method::ItemToolCall,
        RequestId::String(format!("rpc-{id}")),
        json!({"threadId":context.thread,"turnId":context.turn,"callId":id,"tool":name,"arguments":args}),
    )
}
fn reply(value: CallbackDecision) -> Reply {
    match value {
        CallbackDecision::Answer(r) => r,
        other => panic!("expected answer, got {other:?}"),
    }
}
fn body(reply: &Reply) -> &Value {
    match &reply.body {
        ResponseBody::Result(v) => v,
        _ => panic!("expected result"),
    }
}
fn tool_value(reply: &Reply) -> Value {
    serde_json::from_str(body(reply)["contentItems"][0]["text"].as_str().unwrap()).unwrap()
}

async fn provider_callbacks_python_answer_corpus() {
    for case in template().value["answers"].as_array().unwrap() {
        let (actual, history) = answer_value(
            case["method"].as_str().unwrap(),
            &case["params"],
            &case["data"],
        )
        .unwrap();
        assert_eq!(actual, case["expected"], "{}", case["method"]);
        assert_eq!(history, case["history"]);
    }
}
async fn provider_callbacks_durable_tool_replay_and_content_conflict() {
    let (_dir, store, context) = fixture().await;
    let frame = call(
        &context,
        "task",
        "orchestration_task",
        json!({"action":"create","title":"One task"}),
    );
    let first = reply(
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
            .await
            .unwrap(),
    );
    assert_eq!(body(&first)["success"], true);
    assert_eq!(
        body(&first)["contentItems"][1]["text"],
        "[Time awareness] Tool result finalized at 1970-01-01T00:00:10.000Z"
    );
    let second = reply(
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 11.0)
            .await
            .unwrap(),
    );
    assert_eq!(first, second);
    assert_eq!(
        store
            .test_reader()
            .unwrap()
            .query_row("SELECT count(*) FROM runtime_work", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let conflict = call(
        &context,
        "task",
        "orchestration_task",
        json!({"action":"create","title":"Changed"}),
    );
    assert!(
        dispatch(std::sync::Arc::clone(&store), &context, &conflict, 11.0)
            .await
            .is_err()
    );
    let mut next = context.clone();
    next.generation = 8;
    next.connection = "replacement".into();
    let replay = reply(
        dispatch(std::sync::Arc::clone(&store), &next, &frame, 12.0)
            .await
            .unwrap(),
    );
    assert_eq!(first.body, replay.body);
    assert_ne!(first.identity, replay.identity);
    assert!(!tool_value(&first)["id"].as_str().unwrap().is_empty());
}
async fn provider_callbacks_human_pending_and_write_outcomes() {
    let (_dir, store, context) = fixture().await;
    let frame = request(
        Method::ItemToolRequestUserInput,
        RequestId::Number(1),
        json!({"threadId":context.thread,"turnId":context.turn,"questions":[{"id":"q","question":"Secret?","isSecret":true}]}),
    );
    let CallbackDecision::Pending(id) =
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
            .await
            .unwrap()
    else {
        panic!("pending")
    };
    assert_eq!(
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 11.0)
            .await
            .unwrap(),
        CallbackDecision::Pending(id.clone())
    );
    let data = json!({"answers":{"q":{"answers":["do-not-publish"]}}});
    assert!(
        answer(&store, &context, &id, AnswerAuthority::Agent, &data, 12.0)
            .await
            .is_err()
    );
    let prepared = answer(&store, &context, &id, AnswerAuthority::User, &data, 12.0)
        .await
        .unwrap();
    let card: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_requests WHERE id=?1",
            [&id],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!card.contains("do-not-publish"));
    assert!(card.contains("[redacted]"));
    assert!(
        delivery(&store, &prepared.identity, DeliveryOutcome::Submitted)
            .await
            .unwrap()
    );
    assert!(
        answer(&store, &context, &id, AnswerAuthority::User, &data, 12.0)
            .await
            .is_err()
    );
    delivery(&store, &prepared.identity, DeliveryOutcome::Written)
        .await
        .unwrap();
    assert!(
        !delivery(&store, &prepared.identity, DeliveryOutcome::Submitted)
            .await
            .unwrap()
    );
    assert_eq!(
        answer(&store, &context, &id, AnswerAuthority::User, &data, 14.0)
            .await
            .unwrap(),
        prepared
    );
}
async fn provider_callbacks_stale_and_worker_authority() {
    let (_dir, store, context) = fixture().await;
    let mut stale = context.clone();
    stale.epoch += 1;
    let frame = call(
        &stale,
        "stale",
        "orchestration_task",
        json!({"action":"list"}),
    );
    assert!(
        dispatch(std::sync::Arc::clone(&store), &stale, &frame, 10.0)
            .await
            .is_err()
    );
    let actor = &template().value["agents"][1];
    let mut worker = context.clone();
    worker.agent = actor["id"].as_str().unwrap().into();
    worker.thread = "thread-1".into();
    worker.turn = "turn-1".into();
    let question = request(
        Method::ItemToolRequestUserInput,
        RequestId::Number(5),
        json!({"threadId":worker.thread,"turnId":worker.turn}),
    );
    assert!(matches!(
        reply(
            dispatch(std::sync::Arc::clone(&store), &worker, &question, 10.0)
                .await
                .unwrap()
        )
        .body,
        ResponseBody::Error(_)
    ));
    let task = call(
        &worker,
        "worker-create",
        "orchestration_task",
        json!({"action":"create","title":"Forbidden"}),
    );
    assert_eq!(
        body(&reply(
            dispatch(std::sync::Arc::clone(&store), &worker, &task, 10.0)
                .await
                .unwrap()
        ))["success"],
        false
    );
}

async fn provider_callbacks_transaction_failpoint_matrix() {
    // Admission and claim failpoints leave no committed running claim, so retry
    // may execute. Execution failpoints happen after claim commit: they roll
    // back effects and result, and replay must not execute the call again.
    for timing in ["BEFORE", "AFTER"] {
        for (table, operation, predicate) in [
            ("runtime_tool_requests", "INSERT", "1"),
            (
                "runtime_tool_requests",
                "UPDATE",
                "json_extract(NEW.record,'$.stage')='running'",
            ),
            ("runtime_work", "INSERT", "1"),
            (
                "runtime_operation_receipts",
                "INSERT",
                "NEW.id NOT LIKE 'provider-callback:%'",
            ),
            (
                "runtime_tool_requests",
                "UPDATE",
                "json_extract(NEW.record,'$.stage')='completed'",
            ),
            ("runtime_tool_results", "INSERT", "1"),
        ] {
            if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none()
                && !(timing == "AFTER"
                    && matches!(
                        table,
                        "runtime_work" | "runtime_tool_results" | "runtime_operation_receipts"
                    ))
            {
                continue;
            }
            let (dir, store, context) = fixture().await;
            store.transaction_with::<_,studio_runtime_store::StoreError>(move |tx|{
                tx.execute_batch(&format!("CREATE TRIGGER callback_failpoint {timing} {operation} ON {table} WHEN {predicate} BEGIN SELECT RAISE(ABORT,'callback failpoint'); END"))?;Ok(())
            }).await.unwrap();
            let frame = call(
                &context,
                "matrix",
                "orchestration_task",
                json!({"action":"create","title":"Atomic task"}),
            );
            let dispatch_result =
                dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0).await;
            assert!(dispatch_result.is_err(), "{timing} {table} {operation}");
            let work_ids = store
                .test_reader()
                .unwrap()
                .prepare("SELECT id FROM runtime_work ORDER BY id")
                .unwrap()
                .query_map([], |row| row.get::<_, String>(0))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            assert!(
                work_ids.is_empty(),
                "{timing} {table} {operation}; dispatch={dispatch_result:?}"
            );
            store
                .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
                    tx.execute_batch("DROP TRIGGER callback_failpoint")?;
                    Ok(())
                })
                .await
                .unwrap();
            drop(store);
            let store = std::sync::Arc::new(RuntimeStore::open(Some(dir.path())).await.unwrap());
            let result = reply(
                dispatch(std::sync::Arc::clone(&store), &context, &frame, 11.0)
                    .await
                    .unwrap(),
            );
            let failed_before_claim = table == "runtime_tool_requests"
                && (operation == "INSERT"
                    || predicate == "json_extract(NEW.record,'$.stage')='running'");
            if failed_before_claim {
                assert_eq!(body(&result)["success"], true);
                assert_eq!(
                    store
                        .test_reader()
                        .unwrap()
                        .query_row("SELECT count(*) FROM runtime_work", [], |r| r
                            .get::<_, i64>(0))
                        .unwrap(),
                    1
                );
            } else if table != "runtime_tool_results" {
                // The execution transaction body failed and rolled back, so
                // the claim was settled as not applied with a saved result.
                assert_eq!(body(&result)["success"], false);
                let settled = store
                    .test_reader()
                    .unwrap()
                    .query_row(
                        "SELECT json_extract(record,'$.stage'),json_extract(record,'$.outcome'),json_extract(record,'$.finished') IS NOT NULL FROM runtime_tool_requests WHERE json_extract(record,'$.callId')='matrix'",
                        [],
                        |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, bool>(2)?)),
                    )
                    .unwrap();
                assert_eq!(settled, ("failed".into(), "not_applied".into(), true));
                assert_eq!(
                    store
                        .test_reader()
                        .unwrap()
                        .query_row("SELECT count(*) FROM runtime_work", [], |r| r
                            .get::<_, i64>(0))
                        .unwrap(),
                    0,
                    "a rolled-back execution is not repeated"
                );
            } else {
                // The failpoint also blocked the settlement, so the durable
                // claim stays running until startup recovery; the protocol
                // response itself succeeds with recovery data.
                assert_eq!(body(&result)["success"], true);
                let recovery = tool_value(&result);
                assert_eq!(recovery["stage"], "running");
                assert_eq!(recovery["outcome"], "pending");
                assert!(recovery["recovery"].as_str().is_some());
                assert_eq!(
                    store
                        .test_reader()
                        .unwrap()
                        .query_row("SELECT count(*) FROM runtime_work", [], |r| r
                            .get::<_, i64>(0))
                        .unwrap(),
                    0,
                    "an execution failure after claim must remain unknown and not repeat"
                );
            }
            drop(store);
            let store = std::sync::Arc::new(RuntimeStore::open(Some(dir.path())).await.unwrap());
            assert_eq!(
                reply(
                    dispatch(std::sync::Arc::clone(&store), &context, &frame, 12.0)
                        .await
                        .unwrap()
                ),
                result
            );
            assert_eq!(
                store
                    .test_reader()
                    .unwrap()
                    .query_row("SELECT count(*) FROM runtime_work", [], |r| r
                        .get::<_, i64>(0))
                    .unwrap(),
                i64::from(failed_before_claim),
                "replaying the saved callback response must not alter execution state"
            );
        }
    }
}
async fn provider_callbacks_not_sent_and_expiry() {
    let (_dir, store, context) = fixture().await;
    let frame = request(
        Method::ItemCommandExecutionRequestApproval,
        RequestId::Number(77),
        json!({"threadId":context.thread,"turnId":context.turn,"command":"fixture"}),
    );
    let CallbackDecision::Pending(id) =
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
            .await
            .unwrap()
    else {
        panic!("pending")
    };
    let prepared = answer(
        &store,
        &context,
        &id,
        AnswerAuthority::User,
        &json!({"decision":"accept"}),
        11.0,
    )
    .await
    .unwrap();
    delivery(&store, &prepared.identity, DeliveryOutcome::Submitted)
        .await
        .unwrap();
    delivery(&store, &prepared.identity, DeliveryOutcome::NotSent)
        .await
        .unwrap();
    let declined = answer(
        &store,
        &context,
        &id,
        AnswerAuthority::User,
        &json!({"decision":"decline"}),
        12.0,
    )
    .await
    .unwrap();
    assert_eq!(body(&declined), &json!({"decision":"decline"}));
    expire(&store, &context.account, &context.connection)
        .await
        .unwrap();
    assert!(
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 13.0)
            .await
            .is_err()
    );
}
async fn provider_callbacks_malformed_and_foreign_records_are_refused() {
    let (_dir, store, context) = fixture().await;
    for field in ["epoch", "accountKey", "threadId", "turnId"] {
        let mut actor = template().value["agents"][0].clone();
        actor[field] = json!([]);
        assert!(
            CallbackSnapshot::from_record(&actor.to_string(), &context).is_err(),
            "{field}"
        );
    }
    let mut prior_turn = template().value["agents"][0].clone();
    prior_turn.as_object_mut().unwrap().remove("accountKey");
    assert!(CallbackSnapshot::from_record(&prior_turn.to_string(), &context).is_ok());
    prior_turn["accountKey"] = Value::Null;
    assert!(CallbackSnapshot::from_record(&prior_turn.to_string(), &context).is_err());
    prior_turn.as_object_mut().unwrap().remove("accountKey");
    prior_turn["turnEpoch"] = json!(2);
    assert!(CallbackSnapshot::from_record(&prior_turn.to_string(), &context).is_err());
    let foreign = template().value["agents"][2]["id"].as_str().unwrap();
    let frame = call(
        &context,
        "foreign",
        "orchestration_message",
        json!({"target":foreign,"text":"No cross-team side effect"}),
    );
    assert_eq!(
        body(&reply(
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
                .await
                .unwrap()
        ))["success"],
        false
    );
    assert_eq!(
        store
            .test_reader()
            .unwrap()
            .query_row("SELECT count(*) FROM runtime_chat_messages", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}
async fn provider_callbacks_local_message_once_and_worker_chat() {
    let (_dir, store, context) = fixture().await;
    let worker = template().value["agents"][1]["id"].as_str().unwrap();
    let frame = call(
        &context,
        "message",
        "orchestration_message",
        json!({"target":worker,"text":"A real stored message"}),
    );
    let first = reply(
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
            .await
            .unwrap(),
    );
    assert_eq!(body(&first)["success"], true);
    assert_eq!(
        reply(
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 11.0)
                .await
                .unwrap()
        ),
        first
    );
    assert_eq!(
        store
            .test_reader()
            .unwrap()
            .query_row("SELECT count(*) FROM runtime_chat_messages", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    let room = tool_value(&first)["room"].as_str().unwrap().to_owned();
    let read = call(
        &context,
        "read",
        "orchestration_chat_read",
        json!({"room_id":room}),
    );
    assert_eq!(
        body(&reply(
            dispatch(std::sync::Arc::clone(&store), &context, &read, 12.0)
                .await
                .unwrap()
        ))["success"],
        true
    );
}

async fn provider_callbacks_worker_versioned_progress_reaches_the_parent_once() {
    // Live cutover 4: every versioned progress update was refused with
    // "requires coordination/versioned-progress-delivery".
    let (_dir, store, context) = fixture().await;
    let lead = template().value["agents"][0]["id"].as_str().unwrap();
    let actor = &template().value["agents"][1];
    let mut worker = context.clone();
    worker.agent = actor["id"].as_str().unwrap().into();
    worker.thread = "thread-1".into();
    worker.turn = "turn-1".into();
    let progress = |id: &str, version: u64| {
        call(
            &worker,
            id,
            "orchestration_message",
            json!({"target":"parent","text":format!("step {version}"),"importance":"progress",
                "progress_key":"task-7","progress_version":version}),
        )
    };
    let first_frame = progress("progress-1", 1);
    let first = reply(
        dispatch(std::sync::Arc::clone(&store), &worker, &first_frame, 10.0)
            .await
            .unwrap(),
    );
    assert_eq!(body(&first)["success"], true, "{:?}", body(&first));
    assert_eq!(tool_value(&first)["deliveries"][lead], "queued");
    // A retried callback answers from its receipt and delivers nothing again.
    assert_eq!(
        reply(
            dispatch(std::sync::Arc::clone(&store), &worker, &first_frame, 11.0)
                .await
                .unwrap()
        ),
        first
    );
    let second = reply(
        dispatch(
            std::sync::Arc::clone(&store),
            &worker,
            &progress("progress-2", 2),
            12.0,
        )
        .await
        .unwrap(),
    );
    assert_eq!(body(&second)["success"], true, "{:?}", body(&second));
    {
        let reader = store.test_reader().unwrap();
        let mut statement = reader
        .prepare(
            "SELECT text FROM runtime_events WHERE agent=?1 AND kind='agent_message' ORDER BY created, id",
        )
        .unwrap();
        let delivered = statement
            .query_map([lead], |row| row.get::<_, String>(0))
            .unwrap()
            .map(|text| serde_json::from_str::<Value>(&text.unwrap()).unwrap())
            .filter(|payload| payload["progress_key"] == "task-7")
            .collect::<Vec<_>>();
        assert_eq!(
            delivered.len(),
            2,
            "one input per update, none for the retry"
        );
        for (payload, version) in delivered.iter().zip([1, 2]) {
            assert_eq!(payload["importance"], "progress");
            assert_eq!(payload["sender"], worker.agent.as_str());
            assert_eq!(payload["progress_version"], version);
            assert_eq!(payload["text"], format!("step {version}"));
        }
        let chats: i64 = reader
            .query_row(
                "SELECT count(*) FROM runtime_chat_messages WHERE sender=?1",
                [worker.agent.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(chats, 2);
    }
    let refused = call(
        &worker,
        "progress-bad",
        "orchestration_message",
        json!({"target":"parent","text":"bad","importance":"message",
            "progress_key":"task-7","progress_version":3}),
    );
    assert_eq!(
        body(&reply(
            dispatch(std::sync::Arc::clone(&store), &worker, &refused, 13.0)
                .await
                .unwrap()
        ))["success"],
        false,
        "versioned fields still require importance progress"
    );
}

async fn provider_callbacks_python_reservations_and_restart() {
    let (_dir, store, context) = fixture().await;
    recover_connection(&store, &context.account, &context.connection, 20.0)
        .await
        .unwrap();
    for (index, case) in template().value["reservations"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        let id = case["record"]["id"].as_str().unwrap();
        let raw: String = store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT record FROM runtime_tool_requests WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .unwrap();
        let record: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            record["outcome"],
            if index == 0 { "unknown" } else { "not_applied" }
        );
    }
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
            tx.execute("DELETE FROM runtime_tool_requests", [])?;
            tx.execute("DELETE FROM runtime_tool_request_aliases", [])?;
            Ok(())
        })
        .await
        .unwrap();
    for case in template().value["reservations"].as_array().unwrap() {
        let frame = Frame::Request(RequestFrame {
            id: serde_json::from_value(case["message"]["id"].clone()).unwrap(),
            method: Method::ItemToolCall,
            params: Some(case["message"]["params"].clone()),
            fields: Default::default(),
        });
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 21.0)
            .await
            .unwrap();
        let raw: String = store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT record FROM runtime_tool_requests WHERE id=?1",
                [case["record"]["id"].as_str().unwrap()],
                |r| r.get(0),
            )
            .unwrap();
        let actual: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(
            actual["outcome"],
            if actual["tool"] == "orchestration_task" {
                "applied"
            } else {
                "not_applied"
            }
        );
        for field in [
            "id",
            "agent",
            "threadId",
            "callId",
            "tool",
            "signature",
            "rpcId",
            "turnId",
            "epoch",
            "accountKey",
            "connectionId",
            "readOnly",
            "request_id",
        ] {
            assert_eq!(
                actual[field], case["record"][field],
                "{field}: {}",
                case["record"]["tool"]
            );
        }
    }
}

async fn provider_callbacks_utility_invalid_and_deleted() {
    let (_dir, store, context) = fixture().await;
    let time = request(Method::CurrentTimeRead, RequestId::Number(0), json!({}));
    assert_eq!(
        body(&reply(
            dispatch(std::sync::Arc::clone(&store), &context, &time, 10.7)
                .await
                .unwrap()
        )),
        &json!({"currentTimeAt":10})
    );
    let refresh = Frame::UnknownMethod(studio_runtime_providers::UnknownMethodFrame {
        method: "account/chatgptAuthTokens/refresh".into(),
        fields: serde_json::from_value(json!({"id":1,"params":{}})).unwrap(),
    });
    assert!(matches!(
        reply(
            dispatch(std::sync::Arc::clone(&store), &context, &refresh, 11.0)
                .await
                .unwrap()
        )
        .body,
        ResponseBody::Error(_)
    ));
    let malformed = call(&context, "bad", "orchestration_task", Value::Null);
    assert!(
        dispatch(std::sync::Arc::clone(&store), &context, &malformed, 11.0)
            .await
            .is_err()
    );
    let mut actor = template().value["agents"][0].clone();
    actor["deletedAt"] = json!(12.0);
    let actor_json = actor.to_string();
    let agent_id = context.agent.clone();
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(move |tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=?1 WHERE id=?2",
                rusqlite::params![actor_json, agent_id],
            )?;
            Ok(())
        })
        .await
        .unwrap();
    let approval = request(
        Method::ItemCommandExecutionRequestApproval,
        RequestId::Number(2),
        json!({"threadId":context.thread,"turnId":context.turn}),
    );
    let denial = reply(
        dispatch(std::sync::Arc::clone(&store), &context, &approval, 13.0)
            .await
            .unwrap(),
    );
    assert_eq!(body(&denial), &json!({"decision":"decline"}));
    assert!(
        delivery(&store, &denial.identity, DeliveryOutcome::Submitted)
            .await
            .unwrap()
    );
}

async fn provider_callbacks_concurrent_duplicate_runs_once() {
    let (_dir, store, context) = fixture().await;
    let gate = std::sync::Arc::new(tokio::sync::Barrier::new(2));
    let frame = call(
        &context,
        "concurrent",
        "orchestration_task",
        json!({"action":"create","title":"One concurrent task"}),
    );
    let mut children = Vec::new();
    for _ in 0..2 {
        let (store, gate, context, frame) =
            (store.clone(), gate.clone(), context.clone(), frame.clone());
        children.push(tokio::spawn(async move {
            gate.wait().await;
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
                .await
                .unwrap()
        }));
    }
    let mut replies = Vec::new();
    for child in children {
        replies.push(reply(child.await.expect("concurrent callback task")));
    }
    assert_eq!(replies[0], replies[1]);
    assert_eq!(
        store
            .test_reader()
            .unwrap()
            .query_row("SELECT count(*) FROM runtime_work", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
}

async fn provider_callbacks_answer_commit_failpoint() {
    let (_dir, store, context) = fixture().await;
    let frame = request(
        Method::ItemPermissionsRequestApproval,
        RequestId::Number(21),
        json!({"threadId":context.thread,"turnId":context.turn,"permissions":{"network":true}}),
    );
    let CallbackDecision::Pending(id) =
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
            .await
            .unwrap()
    else {
        panic!("pending")
    };
    store.transaction_with::<_,studio_runtime_store::StoreError>(|tx| {
        tx.execute_batch("CREATE TRIGGER answer_fail AFTER UPDATE ON runtime_requests WHEN json_extract(NEW.record,'$.status')='answering' BEGIN SELECT RAISE(ABORT,'answer failpoint'); END")?; Ok(())
    }).await.unwrap();
    let data = json!({"decision":"accept"});
    assert!(
        answer(&store, &context, &id, AnswerAuthority::User, &data, 11.0)
            .await
            .is_err()
    );
    assert_eq!(
        store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT json_extract(record,'$.status') FROM runtime_requests WHERE id=?1",
                [&id],
                |r| r.get::<_, String>(0)
            )
            .unwrap(),
        "pending"
    );
    assert_eq!(store.test_reader().unwrap().query_row("SELECT count(*) FROM runtime_operation_receipts WHERE json_extract(result,'$.delivery') IS NOT NULL",[],|r|r.get::<_,i64>(0)).unwrap(),0);
    store.transaction_with::<_,studio_runtime_store::StoreError>(|tx| {
        tx.execute_batch("DROP TRIGGER answer_fail; CREATE TRIGGER submitted_fail AFTER UPDATE ON runtime_operation_receipts WHEN json_extract(NEW.result,'$.delivery')='submitted' BEGIN SELECT RAISE(ABORT,'submission failpoint'); END")?; Ok(())
    }).await.unwrap();
    let prepared = answer(&store, &context, &id, AnswerAuthority::User, &data, 12.0)
        .await
        .unwrap();
    assert!(
        delivery(&store, &prepared.identity, DeliveryOutcome::Submitted)
            .await
            .is_err()
    );
    assert_eq!(
        reply(
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 13.0)
                .await
                .unwrap()
        ),
        prepared
    );
    store
        .transaction_with::<_, studio_runtime_store::StoreError>(|tx| {
            tx.execute_batch("DROP TRIGGER submitted_fail")?;
            Ok(())
        })
        .await
        .unwrap();
    delivery(&store, &prepared.identity, DeliveryOutcome::Submitted)
        .await
        .unwrap();
    recover_connection(&store, &context.account, &context.connection, 14.0)
        .await
        .unwrap();
    assert!(matches!(
        dispatch(std::sync::Arc::clone(&store), &context, &frame, 15.0).await,
        Err(CallbackError::Unknown)
    ));
}

async fn provider_callbacks_python_durable_answers() {
    let (_dir, store, context) = fixture().await;
    for (index, case) in template().value["answers"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none() && index % 6 != 0 {
            continue;
        }
        let method = case["method"].as_str().unwrap();
        let id = RequestId::Number(1000 + index as u64);
        let frame = if let Some(method) = Method::from_wire(method) {
            request(method, id, case["params"].clone())
        } else {
            Frame::UnknownMethod(studio_runtime_providers::UnknownMethodFrame {
                method: method.into(),
                fields: serde_json::from_value(json!({"id":id,"params":case["params"]})).unwrap(),
            })
        };
        let CallbackDecision::Pending(id) =
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 10.0)
                .await
                .unwrap()
        else {
            panic!("pending")
        };
        let result = answer(
            &store,
            &context,
            &id,
            AnswerAuthority::User,
            &case["data"],
            20.0,
        )
        .await
        .unwrap();
        assert_eq!(body(&result), &case["expected"]);
        let raw: String = store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT record FROM runtime_requests WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .unwrap();
        let card: Value = serde_json::from_str(&raw).unwrap();
        assert_eq!(card["answerHistory"], case["history"]);
        assert_eq!(card["answerSignature"], case["signature"]);
        assert_eq!(card["answeredBy"], "user");
        assert_eq!(card["answeredAt"], 20.0);
    }
}

async fn provider_callbacks_python_dynamic_read_and_title() {
    let (_dir, store, context) = fixture().await;
    for case in template().value["dynamic"].as_array().unwrap() {
        let frame = request(
            Method::ItemToolCall,
            serde_json::from_value(case["message"]["id"].clone()).unwrap(),
            case["message"]["params"].clone(),
        );
        let result = reply(
            dispatch(std::sync::Arc::clone(&store), &context, &frame, 20.0)
                .await
                .unwrap(),
        );
        let expected = &case["expected"];
        assert_eq!(
            review_regressions::canonical_response(body(&result)),
            review_regressions::canonical_response(expected)
        );
    }
    // Python `orchestration_title` puts the agent row, not only its projection.
    let lead: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT record FROM runtime_agents WHERE id=?1",
            [&context.agent],
            |row| row.get(0),
        )
        .unwrap();
    let lead: Value = serde_json::from_str(&lead).unwrap();
    assert_eq!(lead["name"], "Unicode 🦀 title");
    assert_eq!(lead["needsTitle"], false);
}

async fn provider_callbacks_python_visible_id_resolution() {
    let (_dir, store, context) = fixture().await;
    for (index, case) in template().value["resolutions"]
        .as_array()
        .unwrap()
        .iter()
        .enumerate()
    {
        for (tool, args) in [
            (
                "orchestration_task",
                json!({"action":"list","owner":case["supplied"]}),
            ),
            (
                "orchestration_message",
                json!({"target":case["supplied"],"text":"Resolved within caller scope"}),
            ),
        ] {
            let count = || {
                store
                    .test_reader()
                    .unwrap()
                    .query_row("SELECT count(*) FROM runtime_chat_messages", [], |r| {
                        r.get::<_, i64>(0)
                    })
                    .unwrap()
            };
            let before = count();
            let frame = call(&context, &format!("resolver-{index}-{tool}"), tool, args);
            let result = reply(
                dispatch(std::sync::Arc::clone(&store), &context, &frame, 20.0)
                    .await
                    .unwrap(),
            );
            if let Some(expected) = case.get("result") {
                assert_eq!(body(&result)["success"], true, "{case}: {result:?}");
                if tool == "orchestration_message" {
                    let value = tool_value(&result);
                    assert_eq!(
                        value["deliveries"]
                            .as_object()
                            .unwrap()
                            .keys()
                            .collect::<Vec<_>>(),
                        vec![expected.as_str().unwrap()]
                    );
                    assert_eq!(count(), before + 1);
                }
            } else {
                assert_eq!(body(&result)["success"], false, "{case}");
                let python_error = case["error"].as_str().unwrap();
                let label = python_error.split(" agent ID").next().unwrap();
                assert!(
                    body(&result)["contentItems"][0]["text"]
                        .as_str()
                        .unwrap()
                        .contains(label),
                    "{case}: {result:?}"
                );
                assert_eq!(count(), before);
            }
        }
    }
}

async fn provider_callbacks_existing_python_card_uses_stored_identity() {
    let (_dir, store, context) = fixture().await;
    let card = &template().value["legacyCard"];
    let result = answer(
        &store,
        &context,
        card["id"].as_str().unwrap(),
        AnswerAuthority::User,
        &json!({"decision":"decline"}),
        20.0,
    )
    .await
    .unwrap();
    assert_eq!(
        result.identity.rpc_id,
        serde_json::from_value(card["rpcId"].clone()).unwrap()
    );
    assert_eq!(body(&result), &json!({"decision":"decline"}));
    delivery(&store, &result.identity, DeliveryOutcome::Submitted)
        .await
        .unwrap();
    delivery(&store, &result.identity, DeliveryOutcome::Written)
        .await
        .unwrap();
    let status: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT json_extract(record,'$.status') FROM runtime_requests WHERE id=?1",
            [card["id"].as_str().unwrap()],
            |r| r.get(0),
        )
        .unwrap();
    assert_eq!(status, "answered");
}

#[path = "provider_callbacks/review.rs"]
mod review_regressions;

#[tokio::test]
async fn provider_callbacks_corpus() {
    let cases: &[Case] = &[
        ("python_answer_corpus", || {
            Box::pin(provider_callbacks_python_answer_corpus())
        }),
        ("durable_tool_replay_and_content_conflict", || {
            Box::pin(provider_callbacks_durable_tool_replay_and_content_conflict())
        }),
        ("human_pending_and_write_outcomes", || {
            Box::pin(provider_callbacks_human_pending_and_write_outcomes())
        }),
        ("stale_and_worker_authority", || {
            Box::pin(provider_callbacks_stale_and_worker_authority())
        }),
        ("transaction_failpoint_matrix", || {
            Box::pin(provider_callbacks_transaction_failpoint_matrix())
        }),
        ("not_sent_and_expiry", || {
            Box::pin(provider_callbacks_not_sent_and_expiry())
        }),
        ("malformed_and_foreign_records_are_refused", || {
            Box::pin(provider_callbacks_malformed_and_foreign_records_are_refused())
        }),
        ("local_message_once_and_worker_chat", || {
            Box::pin(provider_callbacks_local_message_once_and_worker_chat())
        }),
        ("worker_versioned_progress_reaches_the_parent_once", || {
            Box::pin(provider_callbacks_worker_versioned_progress_reaches_the_parent_once())
        }),
        ("python_reservations_and_restart", || {
            Box::pin(provider_callbacks_python_reservations_and_restart())
        }),
        ("utility_invalid_and_deleted", || {
            Box::pin(provider_callbacks_utility_invalid_and_deleted())
        }),
        ("concurrent_duplicate_runs_once", || {
            Box::pin(provider_callbacks_concurrent_duplicate_runs_once())
        }),
        ("answer_commit_failpoint", || {
            Box::pin(provider_callbacks_answer_commit_failpoint())
        }),
        ("python_durable_answers", || {
            Box::pin(provider_callbacks_python_durable_answers())
        }),
        ("python_dynamic_read_and_title", || {
            Box::pin(provider_callbacks_python_dynamic_read_and_title())
        }),
        ("python_visible_id_resolution", || {
            Box::pin(provider_callbacks_python_visible_id_resolution())
        }),
        ("existing_python_card_uses_stored_identity", || {
            Box::pin(provider_callbacks_existing_python_card_uses_stored_identity())
        }),
        ("review regressions", || {
            Box::pin(review_regressions::run_corpus())
        }),
    ];
    let mut failures = Vec::new();
    for (name, case) in cases {
        if let Err(payload) =
            futures_util::FutureExt::catch_unwind(std::panic::AssertUnwindSafe(case())).await
        {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("non-string panic");
            failures.push(format!("{name}: {message}"));
        }
    }
    assert!(
        failures.is_empty(),
        "provider callback failures:\n{}",
        failures.join("\n")
    );
}
