//! Differential radio-turn policy cases from Python-created room state.
use serde_json::{Value, json};
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use studio_runtime_services::radio::{
    RadioAuthorizationRecords, RadioAuthorizationSnapshot, RadioDenial, RadioTurnDecision,
};

struct Oracle {
    child: Child,
    input: ChildStdin,
    output: BufReader<ChildStdout>,
}

impl Oracle {
    fn new() -> Self {
        let python = std::env::var_os("CODEX_AGENTS_PYTHON")
            .expect("set CODEX_AGENTS_PYTHON to the prepared isolated interpreter");
        for key in ["HOME", "CODEX_HOME", "CLAUDE_CONFIG_DIR"] {
            assert!(
                std::env::var(key)
                    .expect("disposable environment")
                    .starts_with("/var/tmp/")
            );
        }
        let mut child = Command::new(python)
            .arg("-B")
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/radio_policy/oracle.py"))
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("guarded radio Python oracle");
        Self {
            input: child.stdin.take().expect("oracle stdin"),
            output: BufReader::new(child.stdout.take().expect("oracle stdout")),
            child,
        }
    }

    fn call(&mut self, case: &Value) -> Value {
        writeln!(self.input, "{case}").expect("write oracle case");
        self.input.flush().expect("flush oracle case");
        let mut line = String::new();
        self.output
            .read_line(&mut line)
            .expect("read oracle result");
        let result: Value = serde_json::from_str(&line).expect("oracle result JSON");
        assert!(result.get("oracle_error").is_none(), "{result}");
        result
    }
}

impl Drop for Oracle {
    fn drop(&mut self) {
        if self.child.try_wait().expect("check oracle exit").is_some() {
            return;
        }
        let _ignored = writeln!(self.input, "{{\"close\":true}}");
        let _ignored = self.input.flush();
        assert!(self.child.wait().expect("reap oracle").success());
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
