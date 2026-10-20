//! Manual cost probe for provider stdout decoding (`cargo test --release -p
//! studio-runtime-providers --test codec_cost -- --ignored --nocapture`).
//! It prints timings and asserts nothing about speed, so it never gates.

use serde_json::{Map, Value};
use std::time::Instant;
use studio_runtime_providers::{
    DecodedJournalPayload, Frame, LineDecoder, RequestId, decode_journal_entry,
    decode_journal_entry_borrowed,
};

/// Lines decoded per profile and line shape.
const LINES_PER_SHAPE: usize = 10_000;

fn shapes(id: &str) -> Vec<(&'static str, Vec<u8>)> {
    let delta = r#"{"method":"item/agentMessage/delta","params":{"threadId":"019a5c1e-7a52-7d10-9b1c-4f3e2a1b0c9d","turnId":"019a5c1e-8b63-7e21-ac2d-5a4f3b2c1d0e","itemId":"msg_0a1b2c3d4e5f60718293a4b5c6d7e8f9","delta":" the next few words of a long answer"}}"#;
    let completed = r#"{"method":"item/completed","params":{"threadId":"019a5c1e-7a52-7d10-9b1c-4f3e2a1b0c9d","turnId":"019a5c1e-8b63-7e21-ac2d-5a4f3b2c1d0e","item":{"id":"call_1","type":"commandExecution","command":"cargo test -p studio-runtime-providers","cwd":"/home/user/project","status":"completed","exitCode":0,"durationMs":1834,"aggregatedOutput":"running 47 tests\ntest result: ok. 47 passed; 0 failed\n"}}}"#;
    let response = format!(
        r#"{{"id":{id},"result":{{"turn":{{"id":"019a5c1e-8b63-7e21-ac2d-5a4f3b2c1d0e","status":"inProgress","items":[]}}}}}}"#
    );
    vec![
        ("delta", delta.as_bytes().to_vec()),
        ("item/completed", completed.as_bytes().to_vec()),
        ("response", response.into_bytes()),
    ]
}

/// The decoder this crate used before its streaming envelope: one `Map`, then
/// member removal and an ID clone. Kept only as this probe's baseline.
fn map_decode(line: &[u8]) -> Map<String, Value> {
    let mut object: Map<String, Value> = serde_json::from_slice(line).expect("probe line");
    if let Some(id) = object.get("id") {
        let _id: Value = serde_json::from_value(id.clone()).expect("probe id");
    }
    let method = object
        .get("method")
        .and_then(Value::as_str)
        .map(str::to_owned);
    object.remove("id");
    object.remove("params");
    object.remove("method");
    drop(method);
    object
}

/// The supervisor v2 stdout path before the single typed decode: the raw
/// payload became a `Value`, the reply ID was rewritten on it, and the value
/// was encoded again for the wire decoder. Kept only as this probe's baseline.
fn value_detour_decode(payload: &str, local: &Value) -> Frame {
    let mut value: Value = serde_json::from_str(payload).expect("probe payload");
    if value.get("method").is_none()
        && value.get("id").and_then(Value::as_u64).is_some()
        && let Some(object) = value.as_object_mut()
    {
        object.insert("id".into(), local.clone());
    }
    let bytes = serde_json::to_vec(&value).expect("probe value encodes");
    match decode_journal_entry(1, 1, bytes).payload {
        DecodedJournalPayload::Frame(frame) => frame,
        DecodedJournalPayload::RawStdout { .. } => panic!("probe payload decodes"),
    }
}

/// The supervisor v2 stdout path now: one typed decode of the raw payload,
/// then the reply ID is swapped on the parsed frame.
fn single_decode(payload: &str, local: &RequestId) -> Frame {
    let DecodedJournalPayload::Frame(mut frame) =
        decode_journal_entry_borrowed(1, 1, payload.as_bytes()).payload
    else {
        panic!("probe payload decodes");
    };
    if let Frame::Response { id, .. } = &mut frame
        && matches!(id, RequestId::Number(_))
    {
        *id = local.clone();
    }
    frame
}

fn time(label: &str, mut run: impl FnMut()) -> f64 {
    let started = Instant::now();
    for _ in 0..LINES_PER_SHAPE {
        run();
    }
    let micros = started.elapsed().as_secs_f64() * 1e6 / LINES_PER_SHAPE as f64;
    println!("{label:<48} {micros:>8.3} us/line");
    micros
}

#[test]
#[ignore = "manual cost probe"]
fn provider_line_decode_cost() {
    for (profile, id) in [
        ("codex", "42"),
        ("claude", r#""claude:0000000000000000000000000000002a""#),
    ] {
        for (shape, line) in shapes(id) {
            let mut decoder = LineDecoder::new();
            let mut wire = line.clone();
            wire.push(b'\n');
            assert_eq!(decoder.push(&wire).expect("probe frame").len(), 1);
            time(&format!("{profile} {shape} map baseline"), || {
                std::hint::black_box(map_decode(std::hint::black_box(&line)));
            });
            time(&format!("{profile} {shape} envelope decode"), || {
                std::hint::black_box(decode_journal_entry_borrowed(
                    1,
                    1,
                    std::hint::black_box(&line),
                ));
            });
            let decoded = decode_journal_entry_borrowed(1, 1, &line);
            let studio_runtime_providers::DecodedJournalPayload::Frame(frame) = decoded.payload
            else {
                panic!("probe line decodes");
            };
            let text = std::str::from_utf8(&line).expect("probe line is UTF-8");
            let local_value: Value = serde_json::from_str(id).expect("probe id");
            let local_id: RequestId = serde_json::from_str(id).expect("probe id");
            assert_eq!(
                value_detour_decode(text, &local_value),
                single_decode(text, &local_id)
            );
            time(
                &format!("{profile} {shape} supervisor v2 value detour"),
                || {
                    std::hint::black_box(value_detour_decode(
                        std::hint::black_box(text),
                        &local_value,
                    ));
                },
            );
            time(
                &format!("{profile} {shape} supervisor v2 single decode"),
                || {
                    std::hint::black_box(single_decode(std::hint::black_box(text), &local_id));
                },
            );
            time(&format!("{profile} {shape} frame clone"), || {
                std::hint::black_box(Frame::clone(std::hint::black_box(&frame)));
            });
        }
    }
}
