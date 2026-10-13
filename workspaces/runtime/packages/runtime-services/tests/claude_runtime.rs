use serde_json::Value;
use std::process::Command;
use studio_runtime_services::claude_input_recovery::{
    RecoverySnapshot, Rejection, TransportSnapshot,
};

#[test]
fn python_written_claude_rejection_corpus() {
    let python = std::env::var_os("CODEX_AGENTS_PYTHON").expect("set isolated Python");
    let output = Command::new(python)
        .arg("-B")
        .arg(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/claude_runtime/oracle.py"
        ))
        .output()
        .expect("oracle starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("oracle output");
    assert!(cases.len() >= 40);
    for case in cases {
        let input = &case["input"];
        let transport: TransportSnapshot =
            serde_json::from_value(input["transport"].clone()).expect("transport");
        let snapshot = RecoverySnapshot::from_records(
            input["agent"].clone(),
            input["attempt"].clone(),
            input["saved"].clone(),
            input["events"].as_array().expect("events").clone(),
            transport,
            input["runtimeClosed"].as_bool().expect("closed"),
            input["turnHasWork"].as_bool().expect("work"),
        )
        .expect("Python-written snapshot");
        let rejection: Rejection =
            serde_json::from_value(input["rejection"].clone()).expect("rejection");
        let actual = serde_json::to_value(snapshot.recover(&rejection)).expect("serialize");
        assert_eq!(actual, case["expected"], "{}", case["name"]);
    }
}
