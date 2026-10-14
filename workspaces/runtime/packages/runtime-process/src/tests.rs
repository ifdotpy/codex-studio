#![allow(clippy::unwrap_used)]

use super::*;
use crate::provider_shared::{request_frame, response_value};
use crate::transport::{MAX_FRAME_BYTES, read_frame, write_frame};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Cursor;

#[test]
fn supervisor_action_display_preserves_wire_names() {
    use SupervisorAction::*;
    let cases = [
        (Health, "health"),
        (Open, "open"),
        (Next, "next"),
        (FinishFallback, "finishFallback"),
        (AdminCloseHandle, "adminCloseHandle"),
        (RetireCommand, "retireCommand"),
        (Write, "write"),
        (OperationStatus, "operationStatus"),
        (Ack, "ack"),
        (Replay, "replay"),
        (Status, "status"),
        (Detach, "detach"),
    ];
    for (action, expected) in cases {
        assert_eq!(action.to_string(), expected);
    }
}

#[test]
fn frame_codec_round_trips_compact_json_and_preserves_extensions() {
    let value = serde_json::json!({"action":"status","handle":"h","future":42});
    let mut encoded = Vec::new();
    write_frame(&mut encoded, &value).unwrap();
    let mut reader = Cursor::new(encoded);
    let decoded: Value = serde_json::from_str(&read_frame(&mut reader).unwrap().unwrap()).unwrap();
    assert_eq!(decoded, value);
    let result: DetachedResult =
        serde_json::from_value(serde_json::json!({"detached":true,"newField":7})).unwrap();
    assert_eq!(result.extra.get("newField"), Some(&Value::from(7)));
    let encoded_again = serde_json::to_value(result).unwrap();
    assert_eq!(encoded_again["newField"], 7);
}

#[test]
fn typed_request_serialization_preserves_extensions_and_reserved_field_precedence() {
    let mut extra = BTreeMap::new();
    extra.insert("action".to_owned(), Value::String("wrong".to_owned()));
    extra.insert("requestId".to_owned(), Value::from(999));
    extra.insert("extension".to_owned(), Value::Bool(true));
    let frame = request_frame(
        SupervisorAction::Write,
        17,
        WriteRequest {
            handle: SupervisorHandle("handle".to_owned()),
            operation_id: OperationId("operation".to_owned()),
            native_id: None,
            message: serde_json::json!({"method":"initialize"}),
            extra,
        },
    )
    .unwrap();

    let request: Value = serde_json::from_slice(&frame[..frame.len() - 1]).unwrap();
    assert_eq!(request["action"], "write");
    assert_eq!(request["requestId"], 17);
    assert_eq!(request["handle"], "handle");
    assert_eq!(request["extension"], true);
}

#[test]
fn typed_response_deserialization_preserves_unknown_outcomes_and_remote_errors() {
    let response: AckResult = response_value(
        SupervisorAction::Ack,
        17,
        r#"{"requestId":17,"result":{"acknowledged":9,"extension":true}}"#,
    )
    .unwrap();
    assert_eq!(response.acknowledged, 9);
    assert_eq!(response.extra["extension"], true);

    assert!(matches!(
        response_value::<AckResult>(
            SupervisorAction::Ack,
            17,
            r#"{"requestId":17,"error":"durable refusal","result":null}"#,
        ),
        Err(SupervisorError::Remote { message, .. }) if message == "durable refusal"
    ));
    assert!(matches!(
        response_value::<AckResult>(
            SupervisorAction::Ack,
            17,
            r#"{"requestId":18,"result":{"acknowledged":9}}"#,
        ),
        Err(SupervisorError::UnknownOutcome { request_id: 17, .. })
    ));
}

#[test]
fn event_payload_and_unknown_kind_round_trip() {
    let event: SupervisorEvent = serde_json::from_value(serde_json::json!({
        "sequence": 2, "kind": "future-event", "payload": "{\"x\":1}", "generation": 4, "extension": true
    })).unwrap();
    assert_eq!(event.kind, EventKind::Unknown("future-event".into()));
    assert_eq!(event.payload["x"], 1);
    let value = serde_json::to_value(event).unwrap();
    assert_eq!(value["kind"], "future-event");
    assert_eq!(value["extension"], true);
}

#[test]
fn frame_reader_rejects_oversized_and_incomplete_frames() {
    let mut too_large = Cursor::new(vec![b'x'; MAX_FRAME_BYTES + 1]);
    assert!(matches!(
        read_frame(&mut too_large),
        Err(SupervisorError::InvalidFrame(_))
    ));
    let mut partial = Cursor::new(b"{}".to_vec());
    assert!(matches!(
        read_frame(&mut partial),
        Err(SupervisorError::InvalidFrame(_))
    ));
}
