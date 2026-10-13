use serde_json::json;
use studio_runtime_services::voice::policy::{
    self, SessionSignal, VoiceSession, VoiceSessionRow, VoiceSessionState,
};
use studio_runtime_services::voice::provider::notification_requires_stop;

fn connecting_session() -> VoiceSession {
    VoiceSession::from_row(VoiceSessionRow {
        id: "voice-session".into(),
        agent: "agent".into(),
        created: 1.0,
        ended: None,
        offer: Some("offer".into()),
        answer: None,
        native_thread: Some("thread".into()),
        account_key: Some("account".into()),
        connection_id: Some("connection".into()),
        state: Some("connecting".into()),
        error: None,
    })
    .expect("valid scripted voice session")
}

#[test]
fn provider_error_callback_stops_and_keeps_failure_terminal() {
    let session = connecting_session();
    let transition = policy::transition_session(
        &session,
        SessionSignal::ProviderFailed(Some("provider failed".into())),
        2.0,
    )
    .expect("provider error transition");
    assert!(transition.stop_provider);
    assert_eq!(transition.session.state(), Some(VoiceSessionState::Failed));
    assert_eq!(transition.session.error(), Some("provider failed"));
}

#[test]
fn realtime_error_notifications_require_stop_rpc() {
    assert!(notification_requires_stop(&json!({
        "method": "thread/realtime/error",
        "params": {"threadId": "thread"}
    })));
    assert!(!notification_requires_stop(&json!({
        "method": "thread/realtime/closed",
        "params": {"threadId": "thread"}
    })));
}
