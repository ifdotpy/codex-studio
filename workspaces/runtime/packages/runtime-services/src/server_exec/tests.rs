#![allow(clippy::unwrap_used, clippy::duplicate_mod)]

use super::*;

#[path = "../../tests/support/prepared_python.rs"]
mod prepared_python;

/// Python `codex_server_exec` results for the same inputs
/// (tests/server_exec/oracle.py).
fn oracle() -> Value {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let output = std::process::Command::new(prepared_python::prepared_python(
        &manifest.join("../../apps/server/src"),
    ))
    .arg("-B")
    .arg(manifest.join("tests/server_exec/oracle.py"))
    .output()
    .unwrap();
    assert!(
        output.status.success(),
        "Python oracle: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn hex(value: &Value) -> Vec<u8> {
    data_encoding::HEXLOWER
        .decode(value.as_str().unwrap().as_bytes())
        .unwrap()
}

#[test]
fn pages_redaction_and_request_checks_match_python() {
    let oracle = oracle();
    for case in oracle["buffers"].as_array().unwrap() {
        let mut buffer = Buffer::new(case["limit"].as_u64().unwrap() as usize);
        for chunk in case["chunks"].as_array().unwrap() {
            buffer.append(&hex(chunk));
        }
        assert_eq!(buffer.head, hex(&case["head"]), "{case}");
        assert_eq!(buffer.tail, hex(&case["tail"]), "{case}");
        assert_eq!(json!(buffer.total), case["total"], "{case}");
        for read in case["reads"].as_array().unwrap() {
            let page = buffer.read(
                read["offset"].as_u64().unwrap() as usize,
                read["limit"].as_u64().unwrap() as usize,
                read["final"].as_bool().unwrap(),
            );
            assert_eq!(page, read["page"], "{case} {read}");
        }
    }
    for case in oracle["redactions"].as_array().unwrap() {
        let mut redactor = MarkerRedactor::new(case["secret"].as_str().unwrap());
        let chunks = case["chunks"].as_array().unwrap();
        for (index, (chunk, expected)) in chunks
            .iter()
            .zip(case["out"].as_array().unwrap())
            .enumerate()
        {
            let out = redactor.feed(&hex(chunk), index + 1 == chunks.len());
            assert_eq!(out, hex(expected), "{case} chunk {index}");
        }
    }
    for case in oracle["validate"].as_array().unwrap() {
        let result = crate::multi_server_tools::validate_exec(
            &case["payload"],
            case["foreign"].as_bool().unwrap(),
        );
        match (&result, case.get("value")) {
            (Ok(value), Some(expected)) => assert_eq!(value, expected, "{case}"),
            (Err(message), None) => assert_eq!(json!(message), case["error"], "{case}"),
            _ => panic!("{case}: {result:?}"),
        }
    }
}

#[test]
fn buffer_keeps_head_and_tail_like_python() {
    let mut buffer = Buffer::new(6);
    buffer.append(b"abcdefghij");
    assert_eq!(buffer.head, b"abc");
    assert_eq!(buffer.tail, b"hij");
    assert_eq!(buffer.total, 10);
    let head = buffer.read(0, 32, true);
    assert_eq!(head["text"], "abc");
    assert_eq!(head["nextOffset"], 3);
    assert_eq!(head["truncated"], true);
    let tail = buffer.read(3, 32, true);
    assert_eq!(tail["offset"], 7);
    assert_eq!(tail["gapBytes"], 4);
    assert_eq!(tail["text"], "hij");
}

#[test]
fn a_page_never_splits_a_character_until_the_output_is_final() {
    let mut buffer = Buffer::new(100);
    buffer.append("é".as_bytes());
    buffer.append(&[0xe2, 0x82]);
    let open = buffer.read(0, 32, false);
    assert_eq!(open["text"], "é");
    assert_eq!(open["nextOffset"], 2);
    let last = buffer.read(0, 32, true);
    assert_eq!(last["text"], "é\u{fffd}");
    assert_eq!(last["nextOffset"], 4);
}

#[test]
fn the_private_marker_is_redacted_across_chunks() {
    let mut redactor = MarkerRedactor::new("secret");
    let mut out = redactor.feed(b"a sec", false);
    out.extend(redactor.feed(b"ret b", false));
    out.extend(redactor.feed(b"", true));
    assert_eq!(out, b"a [private marker] b");
    let mut redactor = MarkerRedactor::new("secret");
    let mut out = redactor.feed(b"sec", false);
    out.extend(redactor.feed(b"", true));
    assert_eq!(out, b"sec");
}
