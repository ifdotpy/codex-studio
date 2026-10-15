//! Differential radio-turn policy cases from Python-created room state.
#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc::{self, Receiver};
use std::time::{Duration, Instant};
use studio_runtime_services::radio::{
    RadioAuthorizationRecords, RadioAuthorizationSnapshot, RadioDenial, RadioTurnDecision,
};

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: Receiver<Result<String, String>>,
    deadline: Instant,
    _environment: python_test_support::IsolatedTestEnvironment,
}

fn receive_oracle_response(
    output: &Receiver<Result<String, String>>,
    timeout: Duration,
    case_name: &str,
) -> Result<String, String> {
    match output.recv_timeout(timeout) {
        Ok(Ok(line)) => Ok(line),
        Ok(Err(error)) => Err(format!(
            "radio Python oracle failed for {case_name}: {error}"
        )),
        Err(mpsc::RecvTimeoutError::Timeout) => Err(format!(
            "radio Python oracle did not reply to {case_name} within {} seconds",
            timeout.as_secs_f64()
        )),
        Err(mpsc::RecvTimeoutError::Disconnected) => Err(format!(
            "radio Python oracle stopped before replying to {case_name}"
        )),
    }
}

impl Oracle {
    fn new() -> Self {
        let python = python_test_support::prepared_python(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
        let environment = python_test_support::IsolatedTestEnvironment::new();
        let mut command = Command::new(python);
        environment.apply(&mut command);
        let mut child = command
            .arg("-B")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/radio_policy/oracle.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("guarded radio Python oracle");
        let stdout = child.stdout.take().expect("oracle stdout");
        let (sender, output) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(stdout);
            loop {
                let mut line = String::new();
                match reader.read_line(&mut line) {
                    Ok(0) => {
                        let _ = sender.send(Err("oracle closed stdout before replying".to_owned()));
                        return;
                    }
                    Ok(_) => {
                        if sender.send(Ok(line)).is_err() {
                            return;
                        }
                    }
                    Err(error) => {
                        let _ = sender.send(Err(format!("could not read oracle stdout: {error}")));
                        return;
                    }
                }
            }
        });
        Self {
            input: child.stdin.take().expect("oracle stdin"),
            output,
            child,
            deadline: Instant::now() + python_test_support::PYTHON_ORACLE_TIMEOUT,
            _environment: environment,
        }
    }

    fn call(&mut self, case: &Value) -> Value {
        writeln!(self.input, "{case}").expect("write oracle case");
        self.input.flush().expect("flush oracle case");
        let case_name = case["name"].as_str().unwrap_or("unnamed case");
        let remaining = self.deadline.saturating_duration_since(Instant::now());
        let line = receive_oracle_response(&self.output, remaining, case_name)
            .unwrap_or_else(|error| panic!("{error}"));
        let result: Value = serde_json::from_str(&line).expect("oracle result JSON");
        assert!(result.get("oracle_error").is_none(), "{result}");
        result
    }
}

impl Drop for Oracle {
    fn drop(&mut self) {
        match self.child.kill() {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::InvalidInput => {}
            Err(error) => eprintln!("could not stop radio oracle: {error}"),
        }
        if let Err(error) = self.child.wait() {
            eprintln!("could not reap radio oracle: {error}");
        }
    }
}

fn cases() -> Vec<Value> {
    let mut cases: Vec<Value> =
        serde_json::from_str(include_str!("radio_policy/cases.json")).expect("fast radio corpus");
    if std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() == Ok("1") {
        cases.extend(
            serde_json::from_str::<Vec<Value>>(include_str!("radio_policy/cases-full.json"))
                .expect("full radio corpus"),
        );
    }
    cases
}

fn decode_records(value: &Value) -> RadioAuthorizationRecords {
    serde_json::from_value(value.clone()).expect("Python-written radio records")
}

fn python_denial(value: &str) -> RadioDenial {
    match value {
        "The shared turn has no active room receipt" => RadioDenial::NoActiveRoomReceipt,
        "The shared chat participants are no longer available in this project." => {
            RadioDenial::ParticipantsNoLongerAvailable
        }
        "The shared chat membership changed. Its history stays with its original members." => {
            RadioDenial::MembershipChanged
        }
        "A participant is being transferred or its native chat is unavailable." => {
            RadioDenial::ParticipantUnavailable
        }
        "A participant is stopped. Resume that chat before sending." => {
            RadioDenial::ParticipantStopped
        }
        "The shared turn is no longer authorized" => RadioDenial::TurnNoLongerAuthorized,
        other => panic!("unclassified Python denial: {other}"),
    }
}

fn rust_denial(value: &str) -> RadioDenial {
    match value {
        "ambiguous_room_receipt" => RadioDenial::AmbiguousRoomReceipt,
        "no_active_room_receipt" => RadioDenial::NoActiveRoomReceipt,
        "participants_no_longer_available" => RadioDenial::ParticipantsNoLongerAvailable,
        "membership_changed" => RadioDenial::MembershipChanged,
        "participant_unavailable" => RadioDenial::ParticipantUnavailable,
        "participant_stopped" => RadioDenial::ParticipantStopped,
        "turn_no_longer_authorized" => RadioDenial::TurnNoLongerAuthorized,
        other => panic!("unclassified Rust denial override: {other}"),
    }
}

#[test]
fn radio_turn_decisions_match_python_written_room_records() {
    let mut oracle = Oracle::new();
    let cases = cases();
    for case in cases {
        let expected = oracle.call(&case);
        let input = decode_records(&expected["records"]);
        let snapshot = RadioAuthorizationSnapshot::from_records(&input)
            .unwrap_or_else(|error| panic!("{}: {error}", case["name"]));
        let actual = snapshot.authorize_turn(expected["event_id"].as_str().expect("event id"));
        if let Some(denial) = case["rust_denial"].as_str() {
            assert!(
                expected["allowed"].as_bool().expect("Python allow flag"),
                "{}: Python witness must allow",
                case["name"]
            );
            assert_eq!(
                actual,
                RadioTurnDecision::Deny(rust_denial(denial)),
                "{}: declared fail-closed deviation changed",
                case["name"]
            );
            continue;
        }
        if expected["allowed"] == true {
            let RadioTurnDecision::Allow { binding } = actual else {
                panic!("{}: expected allow, got {actual:?}", case["name"]);
            };
            assert_eq!(
                serde_json::to_value(binding).expect("binding JSON"),
                expected["binding"],
                "{}: first-thread binding differs",
                case["name"]
            );
            assert_eq!(
                expected["python_bind_side_effect"], expected["binding"],
                "{}: oracle binding must come from Python's persisted room",
                case["name"]
            );
        } else {
            let RadioTurnDecision::Deny(actual) = actual else {
                panic!("{}: expected denial, got {actual:?}", case["name"]);
            };
            assert_eq!(
                actual,
                python_denial(expected["denial"].as_str().expect("Python denial")),
                "{}: denial differs",
                case["name"]
            );
            if case["expect_python_bind_side_effect"] == true {
                assert!(
                    !expected["python_bind_side_effect"].is_null(),
                    "{}: Python fixture must demonstrate the denied-bind write",
                    case["name"]
                );
                // A deny variant has no binding field, so the adapter cannot persist it.
            }
        }
    }
}

#[test]
fn radio_snapshot_refuses_malformed_consumed_records_with_locations() {
    let mut oracle = Oracle::new();
    let expected = oracle.call(&json!({"name":"ready-peer-team"}));
    let mut input = decode_records(&expected["records"]);

    let mut room: Value =
        serde_json::from_str(input.rooms.values().next().expect("room")).expect("room record");
    room["radio"]["active"]["epoch"] = json!("one");
    let room_id = room["id"].as_str().expect("room id").to_owned();
    input
        .rooms
        .insert(room_id, serde_json::to_string(&room).expect("room JSON"));
    let error = RadioAuthorizationSnapshot::from_records(&input).expect_err("bad active epoch");
    assert!(error.location.ends_with("/radio/active/epoch"), "{error}");

    let mut input = decode_records(&expected["records"]);
    let (agent_id, raw) = input.agents.iter().next().expect("agent");
    let mut agent: Value = serde_json::from_str(raw).expect("agent record");
    agent["autoWake"] = Value::Null;
    input.agents.insert(
        agent_id.clone(),
        serde_json::to_string(&agent).expect("agent JSON"),
    );
    let error = RadioAuthorizationSnapshot::from_records(&input).expect_err("null autoWake");
    assert!(error.location.ends_with("/autoWake"), "{error}");
}

#[test]
fn radio_oracle_response_timeout_is_reported_with_case_identity() {
    let (_sender, receiver) = mpsc::channel();
    let error = receive_oracle_response(&receiver, Duration::from_millis(1), "test-case")
        .expect_err("missing response times out");
    assert!(error.contains("test-case"), "{error}");
    assert!(error.contains("did not reply"), "{error}");
}
