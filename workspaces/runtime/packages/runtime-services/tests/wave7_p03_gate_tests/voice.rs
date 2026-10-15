pub(super) mod support;

#[test]
fn event_persistence_failure_returns_error_and_rolls_back_voice_delivery() {
    let fixture = support::Fixture::new(
        "voice-persistence-failure",
        crate::agents_create::prepared_python,
    );
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
        .expect("seed transcript and arm failure");
    let counts = |table: &str| {
        fixture
            .store
            .read()
            .expect("read counts")
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("row count")
    };
    let before = (counts("voice_deliveries"), counts("runtime_events"));
    let result = super::submit(
        &fixture.store,
        "late-operation-recipient",
        "voice-failure",
        &["spoken-failure".into()],
        None,
    );
    assert!(
        result.is_err(),
        "event persistence failure must reach the service caller"
    );
    assert_eq!(
        (counts("voice_deliveries"), counts("runtime_events")),
        before
    );
}

#[test]
fn voice_accepts_unrelated_operation_started_after_preparation() {
    let fixture = support::Fixture::new("voice", crate::agents_create::prepared_python);
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
        .expect("transcript");
    let (ready, release) = support::arm();
    let store = fixture.store.clone();
    let caller = std::thread::spawn(move || {
        super::submit(
            &store,
            "late-operation-recipient",
            "voice-late",
            &["spoken".into()],
            None,
        )
    });
    support::start_late_operation(&fixture.store, ready, release);
    let receipt = caller
        .join()
        .expect("caller joins")
        .expect("voice accepted");
    assert_eq!(receipt["status"], "queued");
    assert!(
        receipt.get("waitingFor").is_none(),
        "unrelated operation is not a blocker"
    );
}
