use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::Value;
use studio_runtime_services::payload_blobs::{
    decide_externalize, decide_externalize_result, resolve_record, trim_task, write_blobs,
};
use studio_runtime_services::safety_buffering::{SafetySnapshot, active};

fn python_oracle() -> Value {
    let python = std::env::var_os("CODEX_AGENTS_PYTHON")
        .expect("set CODEX_AGENTS_PYTHON to the prepared isolated interpreter");
    let package = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tests = package.join("tests/turn_lifecycle");
    let script = tests.join("payload_safety_oracle.py");
    let cases = tests.join("payload_safety_cases.json");
    let server = package.join("../../apps/server/src");
    let output = Command::new(python)
        .arg(script)
        .arg(cases)
        .arg(server)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("launch isolated Python payload oracle");
    assert!(
        output.status.success(),
        "Python payload oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("parse Python payload oracle result")
}

#[test]
fn payload_decisions_and_reads_match_python_written_records() {
    let oracle = python_oracle();
    for expected in oracle["payload"].as_array().expect("payload cases") {
        let name = expected["name"].as_str().expect("case name");
        let input = &expected["resolved"];
        let table = if expected["record"].get("items").is_some() {
            "checkpoints"
        } else if expected["record"].get("result").is_some() {
            "tool_requests"
        } else {
            "results"
        };
        let decision = if table == "results" {
            decide_externalize_result(input).expect("result externalization decision")
        } else {
            decide_externalize(table, input).expect("record externalization decision")
        };
        assert_eq!(
            decision.record, expected["record"],
            "externalized record {name}"
        );
        let root = tempfile::tempdir().expect("isolated state root");
        write_blobs(root.path(), &decision.writes).expect("write content-addressed files");
        let resolved =
            resolve_record(root.path(), &decision.record).expect("resolve Rust blob refs");
        assert_eq!(
            &resolved, input,
            "read Python-written semantic input for {name}"
        );
    }
}

#[test]
fn task_preview_and_safety_snapshot_match_python_corpus() {
    let oracle = python_oracle();
    for case in oracle["tasks"].as_array().expect("task cases") {
        assert_eq!(
            trim_task(&case["input"]).expect("trim task record"),
            case["trimmed"],
            "task preview {}",
            case["name"].as_str().expect("name")
        );
    }
    for case in oracle["safety"].as_array().expect("safety cases") {
        let record = &case["record"];
        let rust_active = active(record).expect("checked safety receipt");
        assert_eq!(
            rust_active,
            case["active"].as_bool().expect("Python active result"),
            "active receipt for {}",
            record["id"].as_str().expect("agent id")
        );
        SafetySnapshot::from_agent(record, true, true)
            .expect("checked snapshot from Python-written record");
    }
}
