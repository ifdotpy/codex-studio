#![allow(clippy::unwrap_used)]

use super::*;
use crate::transport::{MAX_FRAME_BYTES, read_frame, write_frame};
use serde_json::Value;
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
