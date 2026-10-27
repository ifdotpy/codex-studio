use serde_json::{Map, Value, json};
use studio_runtime_providers::{
    CodecError, DecodedJournalPayload, Frame, Method, NotificationFrame, PendingBook, RequestFrame,
    RequestId, RequestState, ResponseBody, decode_journal_entry, decode_journal_entry_borrowed,
    decode_journal_value, encode_line, encode_line_into,
};

fn method(method: Method, id: Option<RequestId>, params: Option<Value>) -> Frame {
    if let Some(id) = id {
        Frame::Request(RequestFrame {
            id,
            method,
            params,
            fields: Map::new(),
        })
    } else {
        Frame::Notification(NotificationFrame {
            method,
            params,
            fields: Map::new(),
        })
    }
}

#[test]
fn known_method_set_has_unique_reversible_wire_names() {
    let names = Method::all()
        .iter()
        .map(|method| method.as_str())
        .collect::<std::collections::BTreeSet<_>>();
    assert_eq!(
        names.len(),
        Method::all().len(),
        "method wire names must be unique"
    );
    for method in Method::all() {
        assert_eq!(Method::from_wire(method.as_str()), Some(*method));
    }
}

#[test]
fn one_off_catalog_and_terminal_methods_keep_their_wire_names() {
    let methods = [
        (Method::SkillsList, "skills/list"),
        (Method::McpServerStatusList, "mcpServerStatus/list"),
        (
            Method::ThreadBackgroundTerminalsList,
            "thread/backgroundTerminals/list",
        ),
        (
            Method::ThreadBackgroundTerminalsTerminate,
            "thread/backgroundTerminals/terminate",
        ),
        (
            Method::ThreadBackgroundTerminalsClean,
            "thread/backgroundTerminals/clean",
        ),
    ];
    for (operation, wire) in methods {
        assert_eq!(operation.as_str(), wire);
        assert_eq!(Method::from_wire(wire), Some(operation));
        let frame = method(
            operation,
            Some(RequestId::String("request".into())),
            Some(json!({})),
        );
        let encoded = encode_line(&frame).expect("known method encodes");
        assert_eq!(
            decode_journal_entry_borrowed(1, 1, &encoded).payload,
            DecodedJournalPayload::Frame(frame)
        );
    }
}

#[test]
fn unknown_method_and_extension_fields_round_trip_structurally() {
    let original = json!({
        "id": "future-id",
        "method": "future/newNotification",
        "params": {"nested": [1, true, null]},
        "futureField": {"opaque": "kept"}
    });
    let bytes = serde_json::to_vec(&original).expect("fixture json");
    let DecodedJournalPayload::Frame(decoded) = decode_journal_entry(1, 1, bytes).payload else {
        panic!("unknown method must decode");
    };
    assert!(
        matches!(&decoded, Frame::UnknownMethod(frame) if frame.method == "future/newNotification")
    );
    let round_trip: Value =
        serde_json::from_slice(&encode_line(&decoded).expect("encode unknown")).expect("json");
    assert_eq!(round_trip, original);
}

#[test]
fn borrowed_encoder_preserves_exact_bytes_and_reuses_output_storage() {
    let mut fields = Map::new();
    fields.insert("extension".into(), json!({"nested":[1,2]}));
    fields.insert("id".into(), json!(99));
    fields.insert("method".into(), json!("old"));
    fields.insert("params".into(), json!({"old":true}));
    let frame = Frame::Request(RequestFrame {
        id: RequestId::Number(7),
        method: Method::Initialize,
        params: Some(json!({"ok":true})),
        fields,
    });
    let expected = b"{\"extension\":{\"nested\":[1,2]},\"id\":7,\"method\":\"initialize\",\"params\":{\"ok\":true}}\n";
    let mut output = Vec::new();

    encode_line_into(&frame, &mut output).expect("first borrowed encode");
    assert_eq!(output, expected);
    let allocation = (output.as_ptr(), output.capacity());

    output.extend_from_slice(b"discarded prior contents");
    encode_line_into(&frame, &mut output).expect("reused borrowed encode");
    assert_eq!(output, expected);
    assert_eq!((output.as_ptr(), output.capacity()), allocation);
    assert_eq!(
        encode_line(&frame).expect("compatibility encoder"),
        expected
    );
}

#[test]
fn journal_decode_keeps_cursor_generation_and_malformed_stdout_bytes() {
    let raw = b"provider diagnostic, not JSON\r\n".to_vec();
    let decoded = decode_journal_entry(41, 9, raw.clone());
    assert_eq!(decoded.cursor, 41);
    assert_eq!(decoded.generation, 9);
    assert!(matches!(
        decoded.payload,
        DecodedJournalPayload::RawStdout { bytes, error: CodecError::MalformedJson(_) }
            if bytes == raw
    ));

    let frame = decode_journal_entry(42, 9, b"{\"method\":\"turn/started\"}\n".to_vec());
    assert_eq!(frame.cursor, 42);
    assert_eq!(frame.generation, 9);
    assert!(matches!(
        frame.payload,
        DecodedJournalPayload::Frame(Frame::Notification(NotificationFrame {
            method: Method::TurnStarted,
            ..
        }))
    ));
}

#[test]
fn borrowed_journal_decode_matches_owned_valid_and_malformed_records() {
    for bytes in [
        b"{\"method\":\"turn/started\"}\r\n".as_slice(),
        b"provider diagnostic, not JSON\n".as_slice(),
    ] {
        assert_eq!(
            decode_journal_entry_borrowed(17, 4, bytes),
            decode_journal_entry(17, 4, bytes.to_vec()),
        );
    }
}

#[test]
fn journal_value_decoder_matches_the_wire_decoder_for_valid_and_malformed_values() {
    let valid = json!({
        "method": "turn/started",
        "params": {"threadId": "thread"},
        "extension": {"future": true}
    });
    let valid_bytes = serde_json::to_vec(&valid).expect("value serializes");
    assert_eq!(
        decode_journal_value(42, 9, &valid),
        decode_journal_entry(42, 9, valid_bytes),
    );

    let malformed = json!({"method": 17, "extension": [1, 2]});
    let malformed_bytes = serde_json::to_vec(&malformed).expect("value serializes");
    // A parsed value has no source position, so only the error text differs.
    assert!(matches!(
        decode_journal_value(43, 9, &malformed).payload,
        DecodedJournalPayload::RawStdout { bytes, error: CodecError::MalformedJson(_) }
            if bytes == malformed_bytes
    ));
}

#[test]
fn response_requires_exactly_one_body_and_preserves_explicit_null() {
    for value in [
        json!({"id":1}),
        json!({"id":2,"result":null,"error":{"code":9}}),
    ] {
        assert!(
            matches!(
                decode_journal_value(1, 1, &value).payload,
                DecodedJournalPayload::RawStdout {
                    error: CodecError::MalformedFrame(_),
                    ..
                }
            ),
            "{value}"
        );
    }
    assert!(matches!(
        decode_journal_value(1, 1, &json!({"id":1,"result":null})).payload,
        DecodedJournalPayload::Frame(Frame::Response {
            body: ResponseBody::Result(Value::Null),
            ..
        })
    ));
}

#[test]
fn pending_book_keeps_lost_outcomes_unknown_and_detects_bad_responses() {
    let mut book = PendingBook::new();
    let id = book
        .allocate_numeric(Method::TurnStart)
        .expect("allocate ID");
    assert_eq!(id, RequestId::Number(1));
    let second_id = book
        .allocate_numeric(Method::ThreadRead)
        .expect("allocate second ID");
    book.register(
        RequestId::String("claude:uuid".to_owned()),
        Method::ThreadStart,
    )
    .expect("register external ID");
    assert_eq!(
        book.register(id.clone(), Method::TurnStart),
        Err(studio_runtime_providers::BookError::DuplicateId)
    );
    assert_eq!(
        book.observe_response(&RequestId::Number(99)),
        Err(studio_runtime_providers::BookError::UnknownId)
    );

    assert_eq!(
        book.observe_response(&second_id),
        Ok(studio_runtime_providers::ResponseMatch::Pending {
            method: Method::ThreadRead
        })
    );
    assert_eq!(
        book.observe_response(&RequestId::Number(1)),
        Ok(studio_runtime_providers::ResponseMatch::Pending {
            method: Method::TurnStart
        })
    );
    assert_eq!(
        book.observe_response(&RequestId::Number(1)),
        Err(studio_runtime_providers::BookError::DuplicateResponse)
    );
    book.mark_connection_lost();
    assert_eq!(
        book.state(&RequestId::String("claude:uuid".to_owned())),
        Some(RequestState::Unknown)
    );
    assert_eq!(book.unknown_count(), 1);
    assert_eq!(
        book.observe_response(&RequestId::String("claude:uuid".to_owned())),
        Ok(studio_runtime_providers::ResponseMatch::Late {
            method: Method::ThreadStart
        })
    );
    assert_eq!(
        book.state(&RequestId::String("claude:uuid".to_owned())),
        Some(RequestState::Replied)
    );
    assert_eq!(
        book.allocate_numeric(Method::ThreadRead),
        Err(studio_runtime_providers::BookError::ConnectionClosed)
    );
}

#[test]
fn wire_envelope_round_trips_valid_objects_and_refuses_malformed_ones() {
    let valid: &[&[u8]] = &[
        br#"{"method":"item/agentMessage/delta","params":{"threadId":"t","delta":"hi"}}"#,
        br#"{"id":"claude:1","method":"item/tool/call","params":{"x":1},"jsonrpc":"2.0"}"#,
        br#"{"id":7,"result":{"ok":true},"params":{"kept":1}}"#,
        br#"{"id":8,"error":{"code":9}}"#,
        br#"{"id":9,"result":null}"#,
        br#"{"method":"turn/started","params":null}"#,
        br#"{"method":"future/method","id":3,"params":[1],"result":2,"error":3}"#,
        br#"{"method":"turn/started","result":1,"error":2,"extension":{"a":[1,2]}}"#,
        br#"{"\u006dethod":"turn/started","p\u0061rams":{"k":"\u00e9"}}"#,
    ];
    for line in valid {
        let DecodedJournalPayload::Frame(frame) = decode_journal_entry_borrowed(1, 2, line).payload
        else {
            panic!("refused {}", String::from_utf8_lossy(line));
        };
        let encoded: Value =
            serde_json::from_slice(&encode_line(&frame).expect("frame encodes")).expect("JSON");
        let original: Value = serde_json::from_slice(line).expect("test line is JSON");
        assert_eq!(encoded, original, "{}", String::from_utf8_lossy(line));
    }
    type Expected = fn(&CodecError) -> bool;
    let refused: &[(&[u8], Expected)] = &[
        (br#"{"result":1}"#.as_slice(), |error| {
            *error == CodecError::MalformedFrame("expected a method or response id")
        }),
        (br#"{"id":null,"result":1}"#, |error| {
            matches!(error, CodecError::MalformedFrame(_))
        }),
        (br#"{"method":17,"id":1}"#, |error| {
            matches!(error, CodecError::MalformedJson(_))
        }),
        (br#"{"method":"turn/start","id":-1}"#, |error| {
            matches!(error, CodecError::MalformedJson(_))
        }),
        (br#"{"method":"turn/start","id":1.5}"#, |error| {
            matches!(error, CodecError::MalformedJson(_))
        }),
        (
            br#"{"method":"future/method","id":{"odd":true}}"#,
            |error| matches!(error, CodecError::MalformedJson(_)),
        ),
        (
            br#"{"method":"turn/started","method":"turn/completed"}"#,
            |error| matches!(error, CodecError::MalformedJson(_)),
        ),
        (br#"[1,2]"#, |error| {
            matches!(error, CodecError::MalformedJson(_))
        }),
    ];
    for (line, expected) in refused {
        match decode_journal_entry_borrowed(1, 2, line).payload {
            DecodedJournalPayload::RawStdout { bytes, error } => {
                assert_eq!(bytes, *line);
                assert!(
                    expected(&error),
                    "{error:?} for {}",
                    String::from_utf8_lossy(line)
                );
            }
            DecodedJournalPayload::Frame(frame) => {
                panic!("accepted {}: {frame:?}", String::from_utf8_lossy(line))
            }
        }
    }
}

#[test]
fn claude_failure_metadata_is_bounded_and_redacted_before_retention() {
    let wire = json!({"method":"turn/completed","params":{"turn":{"id":"t","status":"failed","error":{"codexErrorInfo":"unauthorized","claudeFailure":{"apiError":"provider_credentials","apiErrorStatus":401,"apiErrorCode":"new_gate","provider":"bedrock","remedy":"refresh_credentials","promptTokens":210001,"maxContextTokens":200000,"error_details":"Bearer secret","token":"sk-secret","message":"private prompt"}}}}});
    let DecodedJournalPayload::Frame(frame) = decode_journal_entry(
        1,
        1,
        serde_json::to_vec(&wire).expect("wire fixture serializes"),
    )
    .payload
    else {
        panic!("frame")
    };
    let encoded =
        String::from_utf8(encode_line(&frame).expect("frame encodes")).expect("frame is UTF-8");
    assert!(!encoded.contains("secret"));
    assert!(!encoded.contains("private prompt"));
    let value: Value = serde_json::from_str(&encoded).expect("encoded frame is JSON");
    assert_eq!(
        value["params"]["turn"]["error"]["claudeFailure"],
        json!({"apiError":"provider_credentials","apiErrorStatus":401,"apiErrorCode":"new_gate","provider":"bedrock","remedy":"refresh_credentials","promptTokens":210001,"maxContextTokens":200000})
    );
    assert_eq!(
        value["params"]["turn"]["error"]["codexErrorInfo"],
        "unauthorized"
    );
}

#[test]
fn claude_failure_sanitizer_preserves_unknown_methods_and_handles_turn_history() {
    let raw = json!({"apiError":"media_removed","apiErrorStatus":400,"token":"secret"});
    let future = json!({"method":"future/error","params":{"turn":{"error":{"claudeFailure":raw}}}});
    let DecodedJournalPayload::Frame(frame) = decode_journal_entry(
        1,
        1,
        serde_json::to_vec(&future).expect("future fixture serializes"),
    )
    .payload
    else {
        panic!("frame")
    };
    assert_eq!(
        serde_json::from_slice::<Value>(&encode_line(&frame).expect("frame encodes"))
            .expect("encoded frame is JSON"),
        future
    );
    for result in [
        json!({"thread":{"turns":[{"error":{"claudeFailure":raw}}]}}),
        json!({"turns":[{"error":{"claudeFailure":raw}}]}),
    ] {
        let wire = json!({"id":1,"result":result});
        let DecodedJournalPayload::Frame(frame) = decode_journal_entry(
            1,
            1,
            serde_json::to_vec(&wire).expect("wire fixture serializes"),
        )
        .payload
        else {
            panic!("frame")
        };
        let encoded =
            String::from_utf8(encode_line(&frame).expect("frame encodes")).expect("frame is UTF-8");
        assert!(!encoded.contains("secret"));
        assert!(encoded.contains("media_removed"));
    }
}
