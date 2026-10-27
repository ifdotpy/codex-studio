//! A proven unsent queue retries through the ordinary claim, never a native start.
#![allow(clippy::unwrap_used)]
use axum::{
    body::{Body, to_bytes},
    extract::ConnectInfo,
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, QueueEvent, QueueEventId, QueueEventRepository,
    QueueWriteRepository, RuntimeStore, SchedulerRepository, StoreError, TurnAttemptIdentity,
};
use tower::ServiceExt;

const ACTOR: &str = "failed-worker";
const USER: &str = "original-user";
const FIRST: &str = "held-preparation";
const REASON: &str = "Provider thread resume failed (invalid transport); input remains pending.";
const RETRY_REFUSAL: &str = "This failed agent has no current unsent batch that can be retried";

async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, axum::Router) {
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    store.transaction(|tx|{
        let actor=AgentRecord::from_value(json!({"id":ACTOR,"rootId":ACTOR,"parentId":null,"isLead":true,"name":"fixture","epoch":0,"status":"queued","provider":"codex","accountKey":"default","threadId":"original-thread","autoWake":true,"inFlight":false}))?;
        AgentRepository::put(tx,&actor)?;
        for(index,kind)in ["followup","followup","work_ready","followup","user"].into_iter().enumerate(){
            let id=if kind=="user"{USER.into()}else{format!("original-{index}")};
            QueueEventRepository::put(tx,&QueueEvent{id:QueueEventId::from(id.as_str()),agent:AgentId::from(ACTOR),kind:kind.into(),text:format!("Original payload {index} 🦀"),status:"pending".into(),created:index as f64+1.0,epoch:0,turn_id:None,error:None})?;
            QueueWriteRepository::put_metadata(tx,&id,&json!({"delivery":"current_turn","acceptedAt":index+1,"assets":[]}))?;
        }
        let claim=SchedulerRepository::claim_for_preparation(tx,ACTOR,0,FIRST,10.0,32)?.unwrap();
        assert!(claim.refusal_reason.is_none(),"{:?}",claim.refusal_reason);
        assert_eq!(claim.events.len(),5);
        SchedulerRepository::settle_start_outcome(tx,&TurnAttemptIdentity{agent_id:ACTOR.into(),epoch:0,attempt_id:FIRST.into()},"held",Some(REASON))?;
        // The account preparation owner releases its settled live pointer. The
        // execution ledger, including its exact claim and unsent receipt, stays.
        let mut actor=AgentRepository::get(tx,&AgentId::from(ACTOR))?.unwrap();
        actor.extra.remove("startAttempt");
        AgentRepository::put(tx,&actor)?;
        Ok(())
    }).await.unwrap();
    let app = router(HttpState::new(
        "fixture-token".into(),
        ACTOR.into(),
        store.root().display().to_string(),
        49171,
        None,
        Some(store.clone()),
        None,
    ))
    .unwrap();
    (directory, store, app)
}
async fn call(app: &axum::Router, body: Option<Value>) -> (u16, Value) {
    let request = Request::builder()
        .method(if body.is_some() { "POST" } else { "GET" })
        .uri(if body.is_some() {
            "/api/queue".into()
        } else {
            format!("/api/queue?agent={ACTOR}")
        })
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
        .body(body.map_or_else(Body::empty, |value| Body::from(value.to_string())))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    let status = response.status().as_u16();
    (
        status,
        serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap())
            .unwrap(),
    )
}
async fn promotion(app: &axum::Router) -> Value {
    let (status, queue) = call(app, None).await;
    assert_eq!(status, 200, "{queue}");
    json!({"agent":ACTOR,"action":"send_now","message_id":USER,"request_id":"retry-original-input","expected_revision":queue["revision"]})
}
fn snapshot(store: &RuntimeStore) -> Vec<Vec<String>> {
    let db = store.test_reader().unwrap();
    ["SELECT record FROM runtime_agents ORDER BY id","SELECT json_array(id,agent,kind,text,status,created,epoch,turn_id,error) FROM runtime_events ORDER BY id","SELECT record FROM runtime_event_meta ORDER BY id","SELECT record FROM runtime_execution_attempts ORDER BY id","SELECT result FROM runtime_operation_receipts ORDER BY id"].into_iter().map(|sql|db.prepare(sql).unwrap().query_map([],|row|row.get(0)).unwrap().collect::<Result<Vec<String>,_>>().unwrap()).collect()
}

#[tokio::test]
async fn send_now_recovers_proven_unsent_failed_batch_and_replays_lost_http_success() {
    let (_directory, store, app) = fixture().await;
    let before = store
        .test_reader()
        .unwrap()
        .prepare("SELECT id,text FROM runtime_events ORDER BY id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    let body = promotion(&app).await;
    let (status, receipt) = call(&app, Some(body.clone())).await;
    assert_eq!(status, 200, "{receipt}");
    assert!(receipt.get("_syncEntities").is_none(), "{receipt}");
    assert!(receipt.get("_syncEntitiesAfter").is_none(), "{receipt}");
    let queued: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT json_extract(record,'$.status') FROM runtime_agents WHERE id=?1",
            [ACTOR],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(queued, "queued", "the source transaction queues the agent");
    let claim = store
        .transaction_with(|tx| {
            SchedulerRepository::claim_for_preparation(tx, ACTOR, 0, "ordinary-retry", 20.0, 32)
        })
        .await
        .unwrap();
    let claim = claim.expect("send_now must queue the original batch for the ordinary scheduler");
    let mut actual = claim
        .events
        .iter()
        .map(|row| (row.id.0.clone(), row.text.clone()))
        .collect::<Vec<_>>();
    actual.sort();
    assert_eq!(actual, before);
    assert_eq!(claim.events[0].id.0, USER);
    let state = snapshot(&store);
    let (status, replay) = call(&app, Some(body.clone())).await;
    assert_eq!(status, 200, "{replay}");
    assert_eq!(replay, receipt, "the durable receipt replays unchanged");
    assert_eq!(snapshot(&store), state);
    let mut conflicting = body;
    conflicting["message_id"] = json!("original-0");
    let (status, result) = call(&app, Some(conflicting)).await;
    assert_eq!(status, 400, "{result}");
    assert_eq!(
        result,
        json!({"error":"This request id has different content"})
    );
    assert_eq!(snapshot(&store), state);
    assert!(
        store
            .transaction_with(|tx| SchedulerRepository::claim_for_preparation(
                tx,
                ACTOR,
                0,
                "second-retry",
                21.0,
                32
            ))
            .await
            .unwrap()
            .is_none()
    );
}

#[tokio::test]
async fn send_now_refuses_unproven_failed_batches_without_any_write() {
    for case in [
        "unknown",
        "submitted",
        "epoch",
        "account",
        "thread",
        "actor",
        "provider",
        "payload",
        "metadata",
        "extra-event",
        "uncertain-event",
        "missing-receipt",
        "later-run",
    ] {
        let (_directory, store, app) = fixture().await;
        store.transaction_with(move|tx|{
            match case{
                "unknown"=>{tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.executionOutcome','unknown','$.submission','unknown') WHERE id=?1",[FIRST])?;},
                "submitted"=>{tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.submitted',json('true')) WHERE id=?1",[FIRST])?;},
                "epoch"=>{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',1) WHERE id=?1",[ACTOR])?;tx.execute("UPDATE runtime_events SET epoch=1 WHERE agent=?1",[ACTOR])?;},
                "account"=>{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.accountKey','changed') WHERE id=?1",[ACTOR])?;},
                "thread"=>{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.threadId','changed') WHERE id=?1",[ACTOR])?;},
                "actor"=>{tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.claimResult.agent.id','other') WHERE id=?1",[FIRST])?;},
                "provider"=>{tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.provider','claude') WHERE id=?1",[ACTOR])?;},
                "payload"=>{tx.execute("UPDATE runtime_events SET text='changed' WHERE id=?1",[USER])?;},
                "metadata"=>{tx.execute("UPDATE runtime_event_meta SET record=json_set(record,'$.delivery','next_turn') WHERE id=?1",[USER])?;},
                "extra-event"|"uncertain-event"=>{
                    let uncertain=case=="uncertain-event";
                    QueueEventRepository::put(tx,&QueueEvent{
                        id:QueueEventId::from("extra"),agent:AgentId::from(ACTOR),
                        kind:if uncertain{"user"}else{"followup"}.into(),
                        text:if uncertain{"unknown earlier input"}else{"new input"}.into(),
                        status:if uncertain{"uncertain"}else{"pending"}.into(),
                        created:6.0,epoch:0,turn_id:None,
                        error:uncertain.then(||"unknown outcome".into()),
                    })?;
                    QueueWriteRepository::put_metadata(tx,"extra",&json!({"delivery":"current_turn","acceptedAt":6,"assets":[]}))?;
                },
                "missing-receipt"=>{tx.execute("UPDATE runtime_execution_attempts SET record=json_remove(record,'$.receipts') WHERE id=?1",[FIRST])?;},
                "later-run"=>{tx.execute("INSERT INTO runtime_execution_runs SELECT 'later',agent,account,epoch,thread,turn,created+1,json_set(record,'$.id','later','$.status','unknown') FROM runtime_execution_runs LIMIT 1",[])?;},
                _=>unreachable!(),
            }Ok::<_,StoreError>(())
        }).await.unwrap();
        let body = promotion(&app).await;
        let before = snapshot(&store);
        let (status, result) = call(&app, Some(body)).await;
        assert_eq!(status, 400, "{case}: {result}");
        assert_eq!(result, json!({"error":RETRY_REFUSAL}), "{case}");
        assert_eq!(snapshot(&store), before, "{case}");
    }
}

#[tokio::test]
async fn send_now_keeps_explicit_stop_and_other_start_owners_held() {
    for (key, value) in [
        ("autoWake", json!(false)),
        ("nativeFailureHold", json!(true)),
        (
            "capacityRetry",
            json!({"id":"capacity","status":"auth_wait"}),
        ),
        ("usageResume", json!({"id":"usage","status":"auth_wait"})),
        ("accountTransferId", json!("transfer")),
        ("contextRepairWait", json!({"phase":"unknown"})),
        ("nativeThreadBlock", json!({"threadId":"original-thread"})),
        ("startAttempt", json!({"id":"other","submitted":true})),
        ("startOutcomeHold", json!({"phase":"unknown"})),
    ] {
        let (_directory, store, app) = fixture().await;
        store
            .transaction_with(move |tx| {
                tx.execute(
                    "UPDATE runtime_agents SET record=json_set(record,?1,json(?2)) WHERE id=?3",
                    [format!("$.{key}"), value.to_string(), ACTOR.into()],
                )?;
                Ok::<_, StoreError>(())
            })
            .await
            .unwrap();
        let body = promotion(&app).await;
        let before = snapshot(&store);
        let (status, result) = call(&app, Some(body)).await;
        assert_eq!(status, 400, "{key}: {result}");
        assert_eq!(result, json!({"error":RETRY_REFUSAL}), "{key}");
        assert_eq!(snapshot(&store), before, "{key}");
    }
}

#[tokio::test]
async fn send_now_does_not_release_owned_account_auth_holds() {
    let mut reasons = vec![studio_runtime_services::account_driver::CHANGED_ERROR];
    for provider in studio_runtime_providers::ProviderKind::ALL {
        let adapter = studio_runtime_services::provider_adapter::adapter(provider);
        reasons.extend([adapter.sign_in_message(), adapter.account_changed_message()]);
    }
    for reason in reasons {
        let (_directory, store, app) = fixture().await;
        store.transaction_with(move|tx|{
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.error',?1) WHERE id=?2",[reason,ACTOR])?;
            tx.execute("UPDATE runtime_events SET error=?1 WHERE agent=?2",[reason,ACTOR])?;
            tx.execute("UPDATE runtime_execution_attempts SET record=json_set(record,'$.receipts.startResult.input.input.reason',?1) WHERE id=?2",[reason,FIRST])?;
            Ok::<_,StoreError>(())
        }).await.unwrap();
        let body = promotion(&app).await;
        let before = snapshot(&store);
        let (status, result) = call(&app, Some(body)).await;
        assert_eq!(status, 400, "{reason}: {result}");
        assert_eq!(result, json!({"error":RETRY_REFUSAL}), "{reason}");
        assert_eq!(snapshot(&store), before);
    }
}
