//! The owner's Studio error list reads Studio-caused endings and hides a
//! dismissed row.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{AgentRecord, AgentRepository, MonitorRecord, RuntimeStore, Table};
use tower::ServiceExt;

async fn call(app: &axum::Router, method: &str, path: &str, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", "127.0.0.1:49171")
        .header("content-type", "application/json")
        .header("x-canvas-token", "fixture-token")
        .header(
            studio_runtime_http::API_SCHEMA_HASH_HEADER,
            studio_runtime_http::api_schema_hash(),
        )
        .extension(ConnectInfo(
            "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
        ))
        .body(body.map_or_else(Body::empty, |body| Body::from(body.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    (status, serde_json::from_slice(&bytes).unwrap())
}

fn monitor(
    root: &std::path::Path,
    id: &str,
    status: &str,
    exit: Value,
    error: &str,
    now: f64,
) -> Value {
    json!({"id":id,"agent":"owner","epoch":0,"turnId":null,"requestId":id,
        "command":"cargo test","cwd":root.to_string_lossy(),
        "timeout_ms":60000,"stallTimeoutSeconds":1800,"livenessCommand":"",
        "activityAt":now,"activityGeneration":0,"stallWakeGeneration":-1,"wakeOn":"exit",
        "successExitCodes":[0],"outputPatterns":[],"outputEventSequence":0,
        "outputEventCount":0,"outputEventIds":[],"status":status,"created":now,
        "finished":now,"exitCode":exit,"error":error,"tail":"","bytes":0,"interactive":false,
        "ruleId":null,"log":root.join(format!("{id}.log")).to_string_lossy(),"cancelRequested":false})
}

#[tokio::test]
async fn studio_errors_list_studio_caused_endings_and_hide_a_dismissed_row() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    let path = root.path().to_path_buf();
    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,
                    "status":"waiting","autoWake":true,"cwd":path.to_string_lossy(),
                    "accountKey":"default","provider":"codex","name":"Fixture worker"}))?,
            )?;
            for record in [
                monitor(
                    &path,
                    "lost-watch",
                    "lost",
                    Value::Null,
                    "Command outcome unknown after losing the provider result; not rerun.",
                    now,
                ),
                monitor(&path, "own-exit", "failed", json!(1), "", now),
                monitor(
                    &path,
                    "stopped",
                    "failed",
                    Value::Null,
                    "The agent turn was stopped",
                    now,
                ),
            ] {
                Table::new(tx).put(&MonitorRecord::from_value(record)?)?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).unwrap();

    let (status, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(status, 200, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(items.len(), 1, "only the Studio-caused ending: {body}");
    assert_eq!(items[0]["monitorId"], "lost-watch");
    assert_eq!(items[0]["count"], 1);
    let row = items[0]["id"].as_str().unwrap().to_owned();
    assert_eq!(items[0]["kind"], "monitor_outcome_lost");
    assert_eq!(items[0]["action"], "check");
    assert_eq!(items[0]["agentName"], "Fixture worker");

    for _ in 0..2 {
        let (status, body) = call(
            &app,
            "POST",
            "/api/studio-errors/dismiss",
            Some(json!({"id":row})),
        )
        .await;
        assert_eq!(status, 200, "{body}");
        assert_eq!(body["items"], json!([]), "a dismissed row stays hidden");
    }
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(body["items"], json!([]));
}

/// One run of rule `rule` or of a plain command, created at `at`.
fn run(
    root: &std::path::Path,
    id: &str,
    rule: Option<&str>,
    command: &str,
    status: &str,
    error: &str,
    at: f64,
) -> Value {
    let mut record = monitor(
        root,
        id,
        status,
        if status == "completed" {
            json!(0)
        } else {
            Value::Null
        },
        error,
        at,
    );
    record["ruleId"] = json!(rule);
    record["command"] = json!(command);
    record
}

/// A rule that checks every few minutes while its owner cannot run fails the
/// same way each time. The list shows one row per rule or command and kind,
/// with the count and the time range, and drops failures that a later run of
/// the same rule followed with a success.
#[tokio::test]
async fn repeated_failures_of_one_rule_or_command_are_one_row() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    let path = root.path().to_path_buf();
    let no_session = "No live provider session";
    let put = |records: Vec<Value>| {
        let store = Arc::clone(&store);
        async move {
            store
                .transaction(move |tx| {
                    for record in records {
                        Table::new(tx).put(&MonitorRecord::from_value(record)?)?;
                    }
                    Ok::<_, studio_runtime_store::StoreError>(())
                })
                .await
                .unwrap();
        }
    };
    let owner_path = path.clone();
    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,
                    "status":"failed","autoWake":true,"cwd":owner_path.to_string_lossy(),
                    "accountKey":"default","provider":"codex","name":"Fixture lead"}))?,
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    put(vec![
        // A later success of the rule clears these three failures.
        run(
            &path,
            "rule-1",
            Some("watch-rule"),
            "check.sh",
            "failed",
            no_session,
            now - 600.0,
        ),
        run(
            &path,
            "rule-2",
            Some("watch-rule"),
            "check.sh",
            "failed",
            no_session,
            now - 500.0,
        ),
        run(
            &path,
            "rule-3",
            Some("watch-rule"),
            "check.sh",
            "failed",
            no_session,
            now - 400.0,
        ),
        run(
            &path,
            "rule-ok",
            Some("watch-rule"),
            "check.sh",
            "completed",
            "",
            now - 300.0,
        ),
        run(
            &path,
            "rule-4",
            Some("watch-rule"),
            "check.sh",
            "failed",
            no_session,
            now - 200.0,
        ),
        run(
            &path,
            "rule-5",
            Some("watch-rule"),
            "check.sh",
            "failed",
            no_session,
            now - 100.0,
        ),
        // A plain command that failed twice, and once with another kind.
        run(
            &path,
            "cmd-1",
            None,
            "cargo build",
            "failed",
            no_session,
            now - 90.0,
        ),
        run(
            &path,
            "cmd-2",
            None,
            "cargo build",
            "failed",
            no_session,
            now - 80.0,
        ),
        run(
            &path,
            "cmd-lost",
            None,
            "cargo build",
            "lost",
            "result lost",
            now - 70.0,
        ),
    ])
    .await;
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).unwrap();

    let (status, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(status, 200, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        3,
        "one row per rule or command and kind: {body}"
    );
    let row = |monitor: &str| {
        items
            .iter()
            .find(|item| item["monitorId"] == monitor)
            .unwrap_or_else(|| panic!("no row ends with {monitor}: {body}"))
            .clone()
    };
    let rule = row("rule-5");
    assert_eq!(
        rule["count"], 2,
        "failures before the success are gone: {rule}"
    );
    assert_eq!(rule["firstAt"], json!(now - 200.0), "{rule}");
    assert_eq!(rule["at"], json!(now - 100.0), "{rule}");
    assert_eq!(rule["ruleId"], "watch-rule", "{rule}");
    assert_eq!(rule["kind"], "monitor_not_recorded", "{rule}");
    let command = row("cmd-2");
    assert_eq!(command["count"], 2, "{command}");
    assert_eq!(command["firstAt"], json!(now - 90.0), "{command}");
    assert_eq!(command["subject"], "cargo build", "{command}");
    let lost = row("cmd-lost");
    assert_eq!(lost["count"], 1, "{lost}");
    assert_eq!(lost["kind"], "monitor_outcome_lost", "{lost}");

    // Dismissing a row hides its failures so far; a later failure returns
    // as a new row of one.
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(json!({"id":rule["id"]})),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["items"].as_array().unwrap().len(), 2, "{body}");
    put(vec![run(
        &path,
        "rule-6",
        Some("watch-rule"),
        "check.sh",
        "failed",
        no_session,
        now + 60.0,
    )])
    .await;
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    let again = body["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["ruleId"] == "watch-rule")
        .unwrap_or_else(|| panic!("the new failure returns: {body}"))
        .clone();
    assert_eq!(again["count"], 1, "{again}");
    assert_eq!(again["monitorId"], "rule-6", "{again}");
}

/// Insert raw rows that their owners write: an event, a usage resume and an
/// account transfer.
fn seed_rows(tx: &studio_runtime_store::Transaction<'_>, now: f64) -> rusqlite::Result<()> {
    tx.execute(
        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES \
         ('idle-message','idle','user','please continue','pending',?1,0), \
         ('stopped-message','stopped','user','are you there','pending',?1,0), \
         ('queued-message','queued','user','start now','pending',?1,0), \
         ('young-message','young','user','just sent','pending',?2,0)",
        rusqlite::params![now - 180.0, now - 30.0],
    )?;
    tx.execute(
        "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES ('resume-1','resuming',?1)",
        [json!({"id":"resume-1","status":"scheduled","accountKey":"default","threadId":"t-resuming",
            "epoch":0,"turnId":"turn-r","cause":"rate_limit","dueAt":now - 1200.0,
            "updatedAt":now - 1300.0,"reason":"Waiting for the usage limit to reset"})
        .to_string()],
    )?;
    tx.execute(
        "INSERT INTO runtime_account_transfers(id,record) VALUES ('transfer-1',?1)",
        [json!({"id":"transfer-1","leadId":"lead","targetAccountKey":"second","status":"pending",
            "scope":"team","created":now - 7200.0,"updated":now - 7200.0,"requests":{},
            "members":{"lead":{"phase":"blocked","sourceAccountKey":"default",
                "error":"Model catalog is pending; no workers were created"}}})
        .to_string()],
    )?;
    Ok(())
}

/// The liveness checker's invariants are rows of the list: a running chat
/// whose provider sent nothing for 10 minutes, an owner message that waits
/// more than 2 minutes while its chat is idle (or held, with the reason), an
/// account session that did not start while chats wait for it, and a
/// schedule that did not move 15 minutes after its due time.
#[tokio::test]
async fn liveness_rows_name_each_stuck_state() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    let path = root.path().to_path_buf();
    store
        .transaction(move |tx| {
            let agent = |id: &str, name: &str, fields: Value| {
                let mut record = json!({"id":id,"rootId":id,"epoch":0,"status":"completed",
                    "autoWake":true,"cwd":path.to_string_lossy(),"accountKey":"default",
                    "provider":"codex","name":name});
                for (key, value) in fields.as_object().unwrap() {
                    record[key] = value.clone();
                }
                AgentRecord::from_value(record)
            };
            for record in [
                agent("silent", "Silent chat", json!({"status":"running","provider":"claude",
                    "accountKey":"claude-local","inFlight":true,"turnId":"turn-s","threadId":"t-silent",
                    "activity":{"phase":"thinking","at":now - 700.0},"activeTools":[]}))?,
                agent("silent-codex", "Silent Codex chat", json!({"status":"running",
                    "inFlight":true,"turnId":"turn-c","threadId":"t-codex",
                    "created":now - 900.0,
                    "activity":{"phase":"thinking","at":now - 700.0},"activeTools":[]}))?,
                agent("busy", "Busy chat", json!({"status":"running","provider":"claude",
                    "accountKey":"claude-local","inFlight":true,"turnId":"turn-b","threadId":"t-busy",
                    "activity":{"phase":"tool","at":now - 700.0},
                    "activeTools":[{"id":"tool-1","name":"Bash"}]}))?,
                agent("idle", "Idle chat", json!({}))?,
                agent("stopped", "Stopped chat", json!({"status":"paused","autoWake":false,
                    "error":"Stopped by user"}))?,
                agent("young", "Young chat", json!({}))?,
                agent("queued", "Queued chat", json!({"status":"queued","accountKey":"second",
                    "startWait":{"reason":"provider_session","epoch":0}}))?,
                agent("resuming", "Resuming chat", json!({"status":"waiting",
                    "usageResume":{"id":"resume-1","status":"scheduled"}}))?,
                agent("lead", "Transfer lead", json!({"isLead":true}))?,
            ] {
                AgentRepository::put(tx, &record)?;
            }
            seed_rows(tx, now)?;
            studio_runtime_services::liveness::record_session_start_failure(
                tx,
                "second",
                "the account is signed out",
                now - 200.0,
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    let state = HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    );
    let app = router(state).unwrap();
    let (status, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(status, 200, "{body}");
    let items = body["items"].as_array().unwrap();
    let row = |kind: &str, agent: &str| {
        items
            .iter()
            .find(|item| item["kind"] == kind && item["agentId"] == agent)
            .unwrap_or_else(|| panic!("no {kind} row for {agent}: {body}"))
            .clone()
    };
    let silent = row("turn_stalled", "silent");
    assert_eq!(silent["action"], "check", "{silent}");
    let codex = row("turn_stalled", "silent-codex");
    for item in [&silent, &codex] {
        let detail = item["detail"].as_str().unwrap();
        for label in ["Account:", "Start:", "Last provider event:"] {
            assert!(detail.contains(label), "missing {label}: {item}");
        }
    }
    assert!(
        items
            .iter()
            .all(|item| item["agentId"] != "busy" && item["agentId"] != "young"),
        "a running tool and a young message are not stuck: {body}"
    );
    let idle = row("input_not_started", "idle");
    assert_eq!(idle["eventId"], "idle-message", "{idle}");
    let stopped = row("input_not_started", "stopped");
    assert!(
        stopped["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("stopped")),
        "a held chat says why: {stopped}"
    );
    let session = row("session_not_started", "queued");
    assert_eq!(session["subject"], "second", "{session}");
    assert!(
        session["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("signed out")),
        "{session}"
    );
    let resume = row("schedule_stalled", "resuming");
    assert!(
        resume["subject"]
            .as_str()
            .is_some_and(|subject| subject.contains("Usage resume")),
        "{resume}"
    );
    let transfer = row("schedule_stalled", "lead");
    assert!(
        transfer["detail"]
            .as_str()
            .is_some_and(|detail| detail.contains("Model catalog is pending")),
        "{transfer}"
    );
}

async fn batch_fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, axum::Router) {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let path = root.path().to_path_buf();
    let now = studio_runtime_services::event_write::unix_time().unwrap();
    store
        .transaction(move |tx| {
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(json!({"id":"owner","rootId":"owner","epoch":0,
            "status":"waiting","autoWake":true,"cwd":path.to_string_lossy(),
            "accountKey":"default","provider":"codex","name":"Fixture worker"}))?,
            )?;
            for id in ["one", "two", "three"] {
                let mut record = monitor(
                    &path,
                    id,
                    "lost",
                    Value::Null,
                    "Command outcome unknown after losing the provider result; not rerun.",
                    now,
                );
                record["command"] = json!(format!("cargo test {id}"));
                Table::new(tx).put(&MonitorRecord::from_value(record)?)?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    let app = router(HttpState::new(
        "fixture-token".into(),
        "fixture".into(),
        store.root().to_string_lossy().into(),
        49171,
        None,
        Some(store.clone()),
        None,
    ))
    .unwrap();
    (root, store, app)
}

async fn batch_row_ids(app: &axum::Router) -> [String; 3] {
    let (status, body) = call(app, "GET", "/api/studio-errors", None).await;
    assert_eq!(status, 200, "{body}");
    let items = body["items"].as_array().unwrap();
    assert_eq!(
        items.len(),
        3,
        "the fixture has three command groups: {body}"
    );
    ["one", "two", "three"].map(|monitor| {
        let row = items
            .iter()
            .find(|row| row["monitorId"] == monitor)
            .unwrap_or_else(|| panic!("no row for {monitor}: {body}"));
        assert_eq!(row["count"], 1, "{row}");
        row["id"].as_str().unwrap().to_owned()
    })
}

#[tokio::test]
async fn studio_errors_batch_exact_ids_unknown_and_idempotent_repeat() {
    let (_root, _store, app) = batch_fixture().await;
    let [one, two, three] = batch_row_ids(&app).await;
    let request = json!({"ids":[one, "missing", three, one]});
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(request.clone()),
    )
    .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["dismissedIds"], json!([one, three]));
    assert_eq!(body["unknownIds"], json!(["missing"]));
    assert_eq!(body["items"].as_array().unwrap().len(), 1);
    assert_eq!(body["items"][0]["id"], two);
    let (status, repeat) = call(&app, "POST", "/api/studio-errors/dismiss", Some(request)).await;
    assert_eq!(status, 200, "{repeat}");
    assert_eq!(repeat["dismissedIds"], json!([]));
    assert_eq!(repeat["unknownIds"], json!([one, "missing", three]));
    assert_eq!(repeat["items"], body["items"]);
}

#[tokio::test]
async fn studio_errors_batch_rejects_oversize_without_dismissing_anything() {
    let (_root, _store, app) = batch_fixture().await;
    let [one, _, _] = batch_row_ids(&app).await;
    let ids = vec![one; studio_runtime_schema::shared_values::STUDIO_ERRORS_DISMISS_MAX_IDS + 1];
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(json!({"ids":ids})),
    )
    .await;
    assert_eq!(status, 400, "{body}");
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(body["items"].as_array().unwrap().len(), 3);
}

#[tokio::test]
async fn studio_errors_batch_rolls_back_all_receipts_if_one_write_fails() {
    let (_root, store, app) = batch_fixture().await;
    let [one, two, _] = batch_row_ids(&app).await;
    let failing_receipt = format!("studio-errors:dismissed:{two}");
    store
        .transaction(move |tx| {
            tx.execute_batch(&format!(
                "CREATE TRIGGER fail_second_dismissal BEFORE INSERT ON runtime_operation_receipts
            WHEN NEW.id='{failing_receipt}'
            BEGIN SELECT RAISE(ABORT, 'fixture dismissal failure'); END;"
            ))?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
    let (status, body) = call(
        &app,
        "POST",
        "/api/studio-errors/dismiss",
        Some(json!({"ids":[one, two]})),
    )
    .await;
    assert_eq!(status, 500, "{body}");
    let (_, body) = call(&app, "GET", "/api/studio-errors", None).await;
    assert_eq!(
        body["items"].as_array().unwrap().len(),
        3,
        "the first dismissal must roll back"
    );
}
