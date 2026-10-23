pub(super) mod support;

async fn count_rows(
    store: std::sync::Arc<studio_runtime_store::RuntimeStore>,
    table: &'static str,
) -> i64 {
    support::read(store, move |connection| {
        connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("row count")
    })
    .await
}

#[tokio::test]
async fn event_persistence_failure_returns_error_and_rolls_back_voice_delivery() {
    let fixture = support::Fixture::new("voice-persistence-failure").await;
    fixture
        .store
        .transaction(|tx| {
            studio_runtime_store::VoiceRepository::insert_record(
                tx,
                &studio_runtime_store::VoiceRecord {
                    sequence: 0,
                    id: "spoken-failure".into(),
                    agent: "late-operation-recipient".into(),
                    session: "session".into(),
                    kind: "user".into(),
                    text: "spoken input".into(),
                    item_id: String::new(),
                    previous_item_id: String::new(),
                    payload: "{}".into(),
                    created: 1.0,
                },
            )?;
            tx.execute_batch(
                "CREATE TRIGGER fail_voice_event BEFORE INSERT ON runtime_events
                 WHEN NEW.id='voice-failure'
                 BEGIN SELECT RAISE(ABORT,'injected voice event failure'); END;",
            )?;
            Ok(())
        })
        .await
        .expect("seed transcript and arm failure");
    let before = (
        count_rows(fixture.store.clone(), "voice_deliveries").await,
        count_rows(fixture.store.clone(), "runtime_events").await,
    );
    let result = super::submit(
        fixture.store.clone(),
        "late-operation-recipient",
        "voice-failure",
        &["spoken-failure".into()],
        None,
    )
    .await;
    assert!(
        result.is_err(),
        "event persistence failure must reach the service caller"
    );
    assert_eq!(
        (
            count_rows(fixture.store.clone(), "voice_deliveries").await,
            count_rows(fixture.store.clone(), "runtime_events").await,
        ),
        before
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn voice_accepts_unrelated_operation_started_after_preparation() {
    let fixture = support::Fixture::new("voice").await;
    fixture
        .store
        .transaction(|tx| {
            studio_runtime_store::VoiceRepository::insert_record(
                tx,
                &studio_runtime_store::VoiceRecord {
                    sequence: 0,
                    id: "spoken".into(),
                    agent: "late-operation-recipient".into(),
                    session: "session".into(),
                    kind: "user".into(),
                    text: "spoken input".into(),
                    item_id: String::new(),
                    previous_item_id: String::new(),
                    payload: "{}".into(),
                    created: 1.0,
                },
            )
        })
        .await
        .expect("transcript");
    let (ready, release) = support::arm();
    let store = fixture.store.clone();
    let caller = tokio::spawn(async move {
        super::submit(
            store,
            "late-operation-recipient",
            "voice-late",
            &["spoken".into()],
            None,
        )
        .await
    });
    let operation = tokio::spawn(support::start_late_operation(
        fixture.store.clone(),
        ready,
        release,
    ));
    operation.await.expect("late operation task completes");
    let receipt = caller.await.expect("caller joins").expect("voice accepted");
    assert_eq!(receipt["status"], "queued");
    assert!(
        receipt.get("waitingFor").is_none(),
        "unrelated operation is not a blocker"
    );
}
