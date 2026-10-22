use serde_json::{Map, Value, json};
use studio_runtime_providers::{
    CodecError, DecodedJournalPayload, Frame, LineDecoder, MAX_FRAME_BYTES, Method,
    NotificationFrame, PendingBook, RequestFrame, RequestId, RequestState, ResponseBody,
    decode_journal_entry, decode_journal_entry_borrowed, decode_journal_value, encode_line,
    encode_line_into,
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

fn padded_unknown_method_frame(payload_bytes: usize, delimiter: &[u8]) -> Vec<u8> {
    let prefix = br#"{"method":"unknown","padding":""#;
    let suffix = b"\"}";
    let fixed_bytes = prefix.len() + suffix.len();
    assert!(payload_bytes >= fixed_bytes);
    let mut frame = Vec::with_capacity(payload_bytes + delimiter.len());
    frame.extend_from_slice(prefix);
    frame.resize(payload_bytes - suffix.len(), b'x');
    frame.extend_from_slice(suffix);
    frame.extend_from_slice(delimiter);
    frame
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
        let mut decoder = LineDecoder::new();
        assert_eq!(
            decoder.push(&encoded).expect("known method decodes"),
            vec![frame]
        );
    }
}

#[test]
fn line_decoder_handles_every_split_and_concatenation() {
    let frames = [
        method(
            Method::Initialize,
            Some(RequestId::Number(1)),
            Some(json!({"capabilities": {}})),
        ),
        method(
            Method::TurnStarted,
            None,
            Some(json!({"turn": {"id": "t"}})),
        ),
        Frame::Response {
            id: RequestId::String("claude:uuid".to_owned()),
            body: ResponseBody::Result(json!({"thread": {"id": "c"}})),
            fields: Map::new(),
        },
    ];
    let encoded = frames
        .iter()
        .map(|frame| encode_line(frame).expect("encode frame"))
        .collect::<Vec<_>>();
    let stream = encoded.concat();

    for split in 0..=stream.len() {
        let mut decoder = LineDecoder::new();
        let mut decoded = decoder.push(&stream[..split]).expect("first partial read");
        decoded.extend(decoder.push(&stream[split..]).expect("second partial read"));
        decoder.finish().expect("complete stream");
        assert_eq!(decoded, frames);
    }
    for width in 1..=stream.len().min(97) {
        let mut decoder = LineDecoder::new();
        let mut decoded = Vec::new();
        for chunk in stream.chunks(width) {
            decoded.extend(decoder.push(chunk).expect("chunked stream"));
        }
        decoder.finish().expect("complete stream");
        assert_eq!(decoded, frames);
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
    let mut decoder = LineDecoder::new();
    let decoded = decoder
        .push(&[bytes, b"\n".to_vec()].concat())
        .expect("decode unknown");
    decoder.finish().expect("complete line");
    assert!(
        matches!(&decoded[0], Frame::UnknownMethod(frame) if frame.method == "future/newNotification")
    );
    let round_trip: Value =
        serde_json::from_slice(&encode_line(&decoded[0]).expect("encode unknown")).expect("json");
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
fn decoder_reports_malformed_oversized_and_truncated_frames() {
    let mut malformed = LineDecoder::new();
    assert!(matches!(
        malformed.push(b"{nope}\n"),
        Err(CodecError::MalformedJson(_))
    ));

    let mut oversized = LineDecoder::new();
    assert!(matches!(
        oversized.push(&vec![b'x'; MAX_FRAME_BYTES + 1]),
        Err(CodecError::FrameTooLarge { .. })
    ));

    let mut truncated = LineDecoder::new();
    truncated
        .push(b"{\"method\":\"initialized\"")
        .expect("partial frame");
    assert!(matches!(
        truncated.finish(),
        Err(CodecError::TruncatedFrame { .. })
    ));
}

#[test]
fn line_decoder_limits_each_lf_or_crlf_frame_independently() {
    let frame_limit = MAX_FRAME_BYTES;
    let mut concatenated = padded_unknown_method_frame(frame_limit / 2, b"\n");
    concatenated.extend_from_slice(&padded_unknown_method_frame(frame_limit / 2, b"\n"));
    assert!(concatenated.len() > frame_limit);
    let mut decoder = LineDecoder::new();
    assert_eq!(
        decoder
            .push(&concatenated)
            .expect("two bounded frames")
            .len(),
        2
    );
    decoder.finish().expect("both frames consumed");

    let exact_lf = padded_unknown_method_frame(frame_limit, b"\n");
    let mut decoder = LineDecoder::new();
    assert_eq!(decoder.push(&exact_lf).expect("maximum LF frame").len(), 1);
    decoder.finish().expect("LF frame consumed");

    let exact_crlf = padded_unknown_method_frame(frame_limit, b"\r\n");
    let split = exact_crlf.len() - 1;
    let mut decoder = LineDecoder::new();
    assert!(
        decoder
            .push(&exact_crlf[..split])
            .expect("maximum CRLF frame before delimiter completion")
            .is_empty()
    );
    let mut bare_cr_eof = LineDecoder::new();
    bare_cr_eof
        .push(&exact_crlf[..split])
        .expect("pending CR is allowed until the delimiter resolves");
    assert!(matches!(
        bare_cr_eof.finish(),
        Err(CodecError::FrameTooLarge { limit }) if limit == frame_limit
    ));
    assert_eq!(
        decoder
            .push(&exact_crlf[split..])
            .expect("split CRLF completes maximum frame")
            .len(),
        1
    );
    decoder.finish().expect("CRLF frame consumed");

    let oversized = padded_unknown_method_frame(frame_limit + 1, b"\n");
    let mut decoder = LineDecoder::new();
    assert!(matches!(
        decoder.push(&oversized),
        Err(CodecError::FrameTooLarge { limit }) if limit == frame_limit
    ));

    let oversized_crlf = padded_unknown_method_frame(frame_limit + 1, b"\r\n");
    let mut decoder = LineDecoder::new();
    assert!(matches!(
        decoder.push(&oversized_crlf),
        Err(CodecError::FrameTooLarge { limit }) if limit == frame_limit
    ));
}

#[test]
fn line_decoder_compacts_once_and_keeps_tail_after_frame_error() {
    let following = encode_line(&method(Method::TurnStarted, None, Some(json!({"id":"t"}))))
        .expect("following frame encodes");
    let mut input = b"{nope}\r\n".to_vec();
    input.extend_from_slice(&following);
    let mut decoder = LineDecoder::new();

    assert!(matches!(
        decoder.push(&input),
        Err(CodecError::MalformedJson(_))
    ));
    assert_eq!(
        decoder.push(&[]).expect("tail remains decodable"),
        vec![method(Method::TurnStarted, None, Some(json!({"id":"t"})))]
    );
    decoder.finish().expect("tail fully consumed");
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
fn response_defaults_and_error_precedence_match_python_client() {
    let mut decoder = LineDecoder::new();
    let decoded = decoder
        .push(b"{\"id\":1}\n{\"id\":2,\"result\":{},\"error\":{\"code\":9}}\n")
        .expect("Python response forms");
    assert_eq!(
        decoded[0],
        Frame::Response {
            id: RequestId::Number(1),
            body: ResponseBody::Result(json!({})),
            fields: Map::new(),
        }
    );
    assert_eq!(
        decoded[1],
        Frame::Response {
            id: RequestId::Number(2),
            body: ResponseBody::Error(json!({"code": 9})),
            fields: Map::new(),
        }
    );
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
fn recorded_provider_fixtures_round_trip_and_keep_declared_order_and_ids() {
    let root = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../apps/server/tests/fixtures/provider-replay"
    );
    let fixture_dir = std::path::Path::new(root);
    let mut names = std::fs::read_dir(fixture_dir)
        .expect("fixtures directory")
        .map(|entry| entry.expect("fixture entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect::<Vec<_>>();
    names.sort();
    assert!(names.len() >= 6, "expected provider replay fixtures");

    for path in names {
        let contents = std::fs::read(&path).expect("read fixture");
        let fixture: Value = serde_json::from_slice(&contents).expect("parse fixture");
        round_trip_protocol_objects(&fixture, &path.display().to_string(), None);
        check_declared_rpc_assertions(&fixture, &path.display().to_string());
    }
}

fn round_trip_protocol_objects(value: &Value, context: &str, parent_field: Option<&str>) {
    match value {
        Value::Array(items) => {
            let expected_order = method_frame_order(items);
            if !expected_order.is_empty() {
                let actual_order = items
                    .iter()
                    .filter(|item| item.get("method").is_some_and(Value::is_string))
                    .map(|item| {
                        let line = [
                            serde_json::to_vec(item).expect("serialize frame"),
                            vec![b'\n'],
                        ]
                        .concat();
                        let mut decoder = LineDecoder::new();
                        let frame = decoder.push(&line).expect("decode ordered frame").remove(0);
                        serde_json::from_slice::<Value>(
                            &encode_line(&frame).expect("encode ordered frame"),
                        )
                        .expect("parse ordered frame")
                    })
                    .collect::<Vec<_>>();
                assert_eq!(
                    method_frame_order(&actual_order),
                    expected_order,
                    "{context}: frame order or IDs changed"
                );
            }
            items
                .iter()
                .for_each(|item| round_trip_protocol_objects(item, context, parent_field));
        }
        Value::Object(object) => {
            let looks_like_frame = object.get("method").is_some_and(Value::is_string)
                || (parent_field == Some("message")
                    && (object.contains_key("result") || object.contains_key("error")));
            if looks_like_frame {
                let mut frame = object.clone();
                if parent_field == Some("message") && !frame.contains_key("id") {
                    frame.insert("id".to_owned(), json!(1));
                }
                let frame_value = Value::Object(frame);
                let input = serde_json::to_vec(&frame_value).expect("encode fixture object");
                let mut decoder = LineDecoder::new();
                let frames = decoder
                    .push(&[input, vec![b'\n']].concat())
                    .unwrap_or_else(|error| panic!("{context}: {error}"));
                decoder.finish().expect("fixture frame line complete");
                assert_eq!(frames.len(), 1, "{context}");
                let output = encode_line(&frames[0]).expect("re-encode fixture frame");
                let mut actual: Value =
                    serde_json::from_slice(&output).expect("parse re-encoded frame");
                if parent_field == Some("message") && !object.contains_key("id") {
                    actual.as_object_mut().expect("frame object").remove("id");
                }
                let expected = Value::Object(object.clone());
                assert_eq!(actual, expected, "{context}");
            }
            for (key, child) in object {
                round_trip_protocol_objects(child, context, Some(key));
            }
        }
        _ => {}
    }
}

fn method_frame_order(items: &[Value]) -> Vec<(String, Option<Value>)> {
    items
        .iter()
        .filter_map(|item| {
            let object = item.as_object()?;
            let method = object.get("method")?.as_str()?.to_owned();
            Some((method, object.get("id").cloned()))
        })
        .collect()
}

fn check_declared_rpc_assertions(fixture: &Value, context: &str) {
    if let Some(replies) = fixture.get("rpcReplies").and_then(Value::as_object) {
        for (method, reply) in replies {
            assert_eq!(
                reply.get("direction").and_then(Value::as_str),
                Some("in"),
                "{context}: {method}"
            );
            assert_eq!(
                reply.get("responseFor").and_then(Value::as_str),
                Some(method.as_str()),
                "{context}: {method}"
            );
            assert!(
                reply.get("message").is_some(),
                "{context}: {method} reply message missing"
            );
        }
    }
    if let Some(missing) = fixture
        .get("expectedMissingReplies")
        .and_then(Value::as_array)
    {
        let mut ids = std::collections::BTreeSet::new();
        for item in missing {
            let method_name = item
                .get("method")
                .and_then(Value::as_str)
                .expect("missing reply method");
            let id = item.get("id").expect("missing reply ID");
            let method = Method::from_wire(method_name).unwrap_or_else(|| {
                panic!("{context}: unknown asserted request method {method_name}")
            });
            let id: RequestId = serde_json::from_value(id.clone())
                .expect("request ID must be a JSON-RPC string or integer");
            assert!(
                ids.insert(id.clone()),
                "{context}: duplicate missing reply ID {id:?}"
            );
            let mut book = PendingBook::new();
            book.register(id.clone(), method)
                .expect("register asserted request");
            book.mark_connection_lost();
            assert_eq!(book.state(&id), Some(RequestState::Unknown), "{context}");
        }
    }
    if let Some(orphaned) = fixture
        .get("allowedOrphanResponses")
        .and_then(Value::as_array)
    {
        for response in orphaned {
            assert!(
                response.get("id").is_some(),
                "{context}: orphan response requires ID"
            );
            assert!(
                response.get("result").is_some() || response.get("error").is_some(),
                "{context}: orphan requires result or error"
            );
        }
    }
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
