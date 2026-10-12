use serde_json::{Map, Value, json};
use studio_runtime_providers::{
    CodecError, Frame, LineDecoder, MAX_FRAME_BYTES, Method, NotificationFrame, PendingBook,
    RequestFrame, RequestId, RequestState, ResponseBody, encode_line,
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
