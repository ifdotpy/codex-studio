//! Regression callers for the canonical recovery and analytics staging ports.
use super::*;
use crate::notification_dispatch::stream::{EnqueueResult, StreamKey};

#[test]
fn p06_empty_deltas_arm_timer_and_coalesce_without_refusal() {
    let mut streams = NotificationStreamBuffer::default();
    let key = StreamKey {
        agent_id: "agent".into(),
        account: "default".into(),
        generation: 7,
        thread: "thread".into(),
        turn: Some("turn".into()),
        item: "answer".into(),
        method: "item/agentMessage/delta".into(),
    };
    let params = json!({"threadId":"thread","turnId":"turn","itemId":"answer","delta":""});
    for cursor in 1..=8193 {
        streams
            .stage_analytics(
                key.clone(),
                NotificationObservation {
                    agent: None,
                    method: key.method.clone(),
                    params: params.clone(),
                    delivery: ProviderDeliveryIdentity {
                        provider_handle: "account:default".into(),
                        generation: 7,
                        cursor,
                    },
                    observed_at: 1234.5,
                    provider_at: 1234.5,
                },
            )
            .expect("analytics never refuses empty delta");
        assert!(matches!(
            streams.enqueue_delta(key.clone(), "", &params),
            EnqueueResult::Buffered
        ));
    }
    assert_eq!(
        streams.pending_bytes(),
        1,
        "empty pending stream arms worker timer"
    );
    let serialized = serde_json::to_vec(&streams).expect("test invariant");
    assert!(
        serialized.len() < 2048,
        "coalesced metadata does not grow with empty deltas"
    );
    let mut restored: NotificationStreamBuffer =
        serde_json::from_slice(&serialized).expect("test invariant");
    restored.flush_analytics(&key, &json!({"id":"agent"}));
    let batch = restored.take_post_commit().expect("test invariant");
    assert_eq!(batch.sequence, 1);
    assert_eq!(batch.analytics_overflow, 0);
    assert_eq!(batch.effects.len(), 1);
    let NotificationPostCommit::AnalyticsStream { metrics, .. } = &batch.effects[0] else {
        panic!("coalesced capture")
    };
    assert_eq!(metrics.hours[0].count, 8193);
    assert_eq!(metrics.bytes, 0);
}

#[test]
fn p06_recovery_message_retains_buffered_text_questions_and_atomicity() {
    let mut db = super::tests::database();
    db.execute_batch("CREATE TABLE runtime_requests(id TEXT PRIMARY KEY,record TEXT NOT NULL); UPDATE runtime_agents SET record=json_set(record,'$.isLead',json('true')) WHERE id='agent';").expect("test invariant");
    let tx = db.transaction().expect("test invariant");
    let frame = super::tests::frame(
        &json!({"method":"item/agentMessage/delta","params":{"threadId":"thread","turnId":"turn","itemId":"answer","delta":"retained"}}),
    );
    let staged = dispatch_notification(
        &tx,
        "agent",
        &PublicationContext::default(),
        &NotificationStreamBuffer::default(),
        &ProviderDeliveryIdentity {
            provider_handle: "account:default".into(),
            generation: 7,
            cursor: 1,
        },
        &frame,
        1234.5,
    )
    .expect("test invariant");
    let streams = staged.stream_update.expect("test invariant");
    let mut agent = NotificationDispatchRepository::agent_record(&tx, "agent")
        .expect("test invariant")
        .expect("test invariant");
    let original = agent.clone();
    let item = json!({"id":"answer","type":"agentMessage","text":"","questions":[{"title":"Which option?","options":["A","B"]}]});
    tx.execute_batch("CREATE TEMP TRIGGER fail_question BEFORE INSERT ON runtime_requests BEGIN SELECT RAISE(ABORT,'question failpoint'); END;").expect("test invariant");
    assert!(
        streams
            .recover_agent_message_in_transaction(
                &tx,
                &mut agent,
                "default",
                7,
                &json!({"id":"turn"}),
                &item,
                1234.5
            )
            .is_err()
    );
    assert_eq!(
        agent, original,
        "failed accessor must not mutate caller agent"
    );
    assert_eq!(
        tx.query_row("SELECT count(*) FROM runtime_items", [], |r| r
            .get::<_, i64>(0))
            .expect("test invariant"),
        0
    );
    tx.execute_batch("DROP TRIGGER fail_question")
        .expect("test invariant");
    let next = streams
        .recover_agent_message_in_transaction(
            &tx,
            &mut agent,
            "default",
            7,
            &json!({"id":"turn"}),
            &item,
            1234.5,
        )
        .expect("test invariant")
        .expect("test invariant");
    let record = NotificationDispatchRepository::notification_item(&tx, "agent", "agent:answer")
        .expect("test invariant")
        .expect("test invariant")
        .0;
    assert_eq!(record["text"], "retained");
    assert_eq!(record["streaming"], false);
    assert_eq!(agent["events"], 2);
    assert_eq!(agent["lastAnswer"], "retained");
    assert_eq!(next.pending_bytes(), 0);
    assert!(
        streams.pending_bytes() > 0,
        "caller buffer remains staged until commit"
    );
    assert_eq!(tx.query_row("SELECT count(*) FROM runtime_operation_receipts WHERE id LIKE 'provider-notification:%'",[],|r|r.get::<_,i64>(0)).expect("test invariant"),1,"recovery must not invent a provider receipt");
    let request: String = tx
        .query_row(
            "SELECT record FROM runtime_requests WHERE id='agent:question:answer'",
            [],
            |r| r.get(0),
        )
        .expect("test invariant");
    assert_eq!(
        serde_json::from_str::<Value>(&request).expect("test invariant")["params"]["questions"][0]
            ["question"],
        "Which option?"
    );
    tx.rollback().expect("test invariant");
    assert_eq!(
        db.query_row("SELECT count(*) FROM runtime_items", [], |r| r
            .get::<_, i64>(0))
            .expect("test invariant"),
        0
    );
}
