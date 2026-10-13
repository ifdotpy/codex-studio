use serde_json::{Value, json};
use studio_runtime_services::claude_input_recovery::RecoverySnapshot;
#[test]
fn python_frozen_batch_full_input_held_resume_and_stop() {
    let output =
        std::process::Command::new(std::env::var_os("CODEX_AGENTS_PYTHON").expect("valid fixture"))
            .args([
                "-B",
                concat!(
                    env!("CARGO_MANIFEST_DIR"),
                    "/tests/claude_runtime/oracle.py"
                ),
                "--followups",
            ])
            .output()
            .expect("valid fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let corpus: Vec<Value> = serde_json::from_slice(&output.stdout).expect("valid fixture");
    for case in corpus {
        let input = &case["input"];
        let snapshot = RecoverySnapshot::from_records(
            input["agent"].clone(),
            input["attempt"].clone(),
            input["saved"].clone(),
            input["events"].as_array().expect("valid fixture").clone(),
            serde_json::from_value(input["transport"].clone()).expect("valid fixture"),
            false,
            false,
        )
        .expect("valid fixture");
        let later = input["laterUser"].as_bool().expect("valid fixture");
        let transcript = &input["transcript"];
        let actual = match case["mode"].as_str().expect("valid fixture") {
            "batch" => json!(snapshot.retry_batch(later)),
            "input" => {
                let result = snapshot.retry_input(later, transcript, None);
                assert_eq!(
                    result.is_err(),
                    case["error"].as_bool().expect("valid fixture"),
                    "{}",
                    case["name"]
                );
                json!(result.ok().flatten())
            }
            "resume" => snapshot
                .resume_held(later, transcript, None)
                .unwrap_or_else(|| input["agent"].clone()),
            "stop" => {
                let retired = snapshot.retire_stopped(None);
                json!({"retired":retired.is_some(),"agent":retired.map(|r|r.agent).unwrap_or_else(||input["agent"].clone())})
            }
            _ => unreachable!(),
        };
        assert_eq!(actual, case["expected"], "{}", case["name"]);
    }
}
