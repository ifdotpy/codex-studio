pub(super) mod support;

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
