use std::path::PathBuf;
use std::process::{Command, Stdio};

use serde_json::{Value, json};
use studio_runtime_services::payload_blobs::{
    decide_externalize_json, decide_externalize_result_json, resolve_record, trim_task,
};
use studio_runtime_services::safety_buffering::{
    SafetyDecision, SafetySnapshot, active, decide_action, decide_restart,
};

#[path = "support/prepared_python.rs"]
mod prepared_python;

fn python_oracle() -> (Value, tempfile::TempDir) {
    let package = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let tests = package.join("tests/turn_lifecycle");
    let script = tests.join("payload_safety_oracle.py");
    let cases = tests.join("payload_safety_cases.json");
    let server = package.join("../../apps/server/src");
    let state = tempfile::tempdir().expect("Python oracle state directory");
    let output = Command::new(prepared_python::prepared_python(&server))
        .arg(script)
        .arg(cases)
        .arg(server)
        .arg(state.path())
        .env("PYTHONPATH", package.join("../../apps/server/src"))
        .env("PYTHONDONTWRITEBYTECODE", "1")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("launch isolated Python payload oracle");
    assert!(
        output.status.success(),
        "Python payload oracle failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    (
        serde_json::from_slice(&output.stdout).expect("parse Python payload oracle result"),
        state,
    )
}

#[test]
fn payload_decisions_and_reads_match_python_written_records() {
    let (oracle, state) = python_oracle();
    let cases = oracle["payload"].as_array().expect("payload cases");
    let expected_count = if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
        8
    } else {
        5
    };
    assert_eq!(cases.len(), expected_count);
    for expected in cases {
        let name = expected["name"].as_str().expect("case name");
        let input_json = expected["inputJson"].as_str().expect("Python input JSON");
        let decision = if expected["table"] == "results" {
            decide_externalize_result_json(input_json).expect("result externalization decision")
        } else {
            decide_externalize_json(expected["table"].as_str().expect("table"), input_json)
                .expect("record externalization decision")
        };
        assert_eq!(
            decision.record, expected["record"],
            "externalized record {name}"
        );
        let resolved = resolve_record(state.path(), &expected["record"])
            .expect("read Python-written content-addressed blobs");
        assert_eq!(
            resolved, expected["resolved"],
            "Python blob read for {name}"
        );
    }
    let preview = &cases[3]["record"]["_payloadBlobs"]["result"]["preview"]["contentItems"][0];
    assert!(preview.is_object());
    assert!(
        preview.get("key5").is_none(),
        "the ninth producer key is omitted"
    );
}

#[test]
fn task_preview_and_safety_snapshot_match_python_corpus() {
    let (oracle, _) = python_oracle();
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

    let agent = json!({
        "id":"agent-5", "epoch":4, "threadId":"thread", "turnId":"turn",
        "accountKey":"acct", "inFlight":true, "autoWake":true,
        "nativeSafetyBuffering":{"threadId":"thread", "turnId":"turn", "connectionId":"connection",
            "showBufferingUi":true, "fasterModel":"model"}
    });
    let snapshot = SafetySnapshot::from_agent(&agent, true, true).expect("action snapshot");
    let actions = &oracle["actions"];
    assert_eq!(
        decide_action(&snapshot, "wait", "turn", None, "", "", 123.5),
        Ok(SafetyDecision::Waiting {
            agent_id: "agent-5".into(),
            turn_id: "turn".into()
        }),
        "Python action wait decision: {}",
        actions["wait"]
    );
    let SafetyDecision::BeginRetry { receipt } = decide_action(
        &snapshot,
        "retry",
        "turn",
        None,
        "agent-5:turn",
        "attempt-fixed",
        123.5,
    )
    .expect("retry decision") else {
        panic!("expected retry decision")
    };
    assert_eq!(public_receipt(&receipt), actions["retry"]["result"]);
    assert_eq!(receipt["sourceThreadId"], "thread");

    let existing = json!({
        "id":"agent-5:turn", "agent":"agent-5", "epoch":4, "turnId":"turn",
        "accountKey":"acct", "model":"model", "stage":"fork", "created":12.0,
        "updated":12.0, "private":"not-public"
    });
    let replay = decide_action(
        &snapshot,
        "retry",
        "turn",
        Some(&existing),
        "agent-5:turn",
        "",
        123.5,
    )
    .expect("replay decision");
    let SafetyDecision::Replay { receipt } = replay else {
        panic!("expected replay")
    };
    assert_eq!(receipt, actions["replay"]["result"]);
    let cancelled = decide_action(
        &snapshot,
        "cancel",
        "turn",
        Some(&existing),
        "agent-5:turn",
        "",
        123.5,
    )
    .expect("cancel decision");
    let SafetyDecision::Cancelled { receipt } = cancelled else {
        panic!("expected cancellation")
    };
    assert_eq!(receipt, actions["cancel"]["result"]);

    for case in oracle["restarts"].as_array().expect("restart cases") {
        let agent = &case["inputAgent"];
        let saved = &case["saved"];
        let rust = decide_restart(agent, Some(saved), 123.5).expect("restart decision");
        if case["restarted"] == true {
            assert_eq!(
                rust.expect("Python restart produced hold"),
                case["operation"]
            );
        } else {
            assert_eq!(
                rust, None,
                "Python restart decision for stage {}",
                case["stage"]
            );
        }
    }
}

fn public_receipt(receipt: &Value) -> Value {
    let keys = [
        "id",
        "stage",
        "model",
        "turnId",
        "created",
        "updated",
        "epoch",
        "accountKey",
        "error",
        "newThreadId",
        "acceptedTurnId",
        "requestId",
        "rpcMethod",
    ];
    let mut view = serde_json::Map::new();
    for key in keys {
        view.insert(key.into(), receipt.get(key).cloned().unwrap_or(Value::Null));
    }
    Value::Object(view)
}
