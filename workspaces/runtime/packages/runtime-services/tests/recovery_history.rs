#[path = "support/prepared_python.rs"]
mod python_test_support;
use serde_json::{Value, json};
use std::{collections::VecDeque, time::Instant};
use studio_runtime_services::{
    recovery::{self, RecoveryAction, RecoveryEffects, RecoveryError, RecoveryOutcome},
    recovery_history::*,
    recovery_policy::RecoverySnapshot,
};
fn scope() -> ConnectionScope {
    ConnectionScope {
        account: "default".into(),
        connection: "connection".into(),
        generation: 1,
    }
}
#[derive(Default)]
struct Reader {
    replies: VecDeque<Result<Value, HistoryError>>,
    methods: Vec<HistoryMethod>,
    calls: Vec<Value>,
    barrier: bool,
    foreign: bool,
}
impl RecoveryReader for Reader {
    fn observation(
        &mut self,
        scope: &ConnectionScope,
    ) -> Result<studio_runtime_services::recovery_absence::RecoveryObservation, HistoryError> {
        Ok(
            studio_runtime_services::recovery_absence::RecoveryObservation {
                scope: scope.clone(),
                at: 100.0,
                supervisor: None,
                quiet: false,
                journal_drained: false,
                account_reserved: false,
                tools_refreshing: false,
                preparation_pending: false,
                completed_turn: None,
            },
        )
    }

    fn read(
        &mut self,
        s: &ConnectionScope,
        m: HistoryMethod,
        params: Value,
        _: Instant,
    ) -> Result<HistoryReceipt, HistoryError> {
        assert!(!self.barrier);
        self.methods.push(m);
        self.calls
            .push(json!({"method":m.as_str(),"params":params}));
        let mut s = s.clone();
        if self.foreign {
            s.generation += 1;
        }
        Ok(HistoryReceipt {
            scope: s,
            body: self.replies.pop_front().expect("no extra native read")?,
        })
    }
    fn barrier(&mut self, _: &ConnectionScope, _: Instant) -> Result<(), HistoryError> {
        self.barrier = true;
        Ok(())
    }
}
fn turn() -> Value {
    json!({"id":"turn","status":"completed","items":[{"id":"item","type":"agentMessage","text":"complete"}]})
}
fn page(turn: Value) -> Value {
    json!({"data":[turn],"nextCursor":null})
}
fn read(replies: Vec<Result<Value, HistoryError>>) -> Result<Option<Value>, HistoryError> {
    let mut reader = Reader {
        replies: replies.into(),
        ..Default::default()
    };
    HistoryRead::new(&mut reader, scope(), Instant::now() + READ_DEADLINE)
        .exact_turn("thread", "turn")
}
#[test]
fn exact_turn_keeps_complete_text_and_coalesces_identical_items() {
    let mut t = turn();
    let duplicate = t["items"][0].clone();
    t["items"]
        .as_array_mut()
        .expect("valid recovery fixture")
        .push(duplicate);
    assert_eq!(
        read(vec![Ok(page(t))]).expect("valid recovery fixture"),
        Some(turn())
    );
}
#[test]
fn conflicting_duplicate_items_are_not_a_receipt() {
    let mut t = turn();
    t["items"]
        .as_array_mut()
        .expect("valid recovery fixture")
        .push(json!({"id":"item","type":"agentMessage","text":"changed"}));
    assert_eq!(read(vec![Ok(page(t))]), Err(HistoryError::Identity));
}
#[test]
fn summary_items_are_paginated_and_terminal_metadata_is_rechecked() {
    let summary = json!({"id":"turn","status":"completed","items":[],"itemsView":"notLoaded"});
    let result = read(vec![
        Ok(page(summary.clone())),
        Ok(json!({"data":[turn()["items"][0]],"nextCursor":null})),
        Ok(page(summary)),
    ])
    .expect("valid recovery fixture")
    .expect("valid recovery fixture");
    assert_eq!(result["items"], turn()["items"]);
    assert_eq!(result["itemsView"], "full");
}
#[test]
fn changed_terminal_metadata_is_refused() {
    let summary = json!({"id":"turn","status":"completed","items":[],"itemsView":"notLoaded"});
    let mut changed = summary.clone();
    changed["status"] = json!("failed");
    assert_eq!(
        read(vec![
            Ok(page(summary)),
            Ok(json!({"data":[]})),
            Ok(page(changed))
        ]),
        Err(HistoryError::Identity)
    );
}
#[test]
fn unsupported_items_allows_only_full_target_fallback() {
    let summary = json!({"id":"turn","status":"completed","items":[],"itemsView":"summary"});
    assert_eq!(
        read(vec![
            Ok(page(summary)),
            Err(HistoryError::Unsupported),
            Ok(page(turn()))
        ])
        .expect("valid recovery fixture"),
        Some(turn())
    );
}
#[test]
fn repeated_cursor_and_foreign_item_are_refused() {
    let empty = json!({"data":[],"nextCursor":"same"});
    assert!(read(vec![Ok(empty.clone()), Ok(empty)]).is_err());
    let mut t = turn();
    t["items"][0]["threadId"] = json!("foreign");
    assert_eq!(read(vec![Ok(page(t))]), Err(HistoryError::Identity));
}
#[test]
fn foreign_transport_receipt_is_refused_before_use() {
    let mut reader = Reader {
        replies: vec![Ok(page(turn()))].into(),
        foreign: true,
        ..Default::default()
    };
    assert_eq!(
        HistoryRead::new(&mut reader, scope(), Instant::now() + READ_DEADLINE)
            .exact_turn("thread", "turn"),
        Err(HistoryError::Identity)
    );
}
#[test]
fn expired_read_does_not_call_provider() {
    let mut reader = Reader::default();
    assert_eq!(
        HistoryRead::new(&mut reader, scope(), Instant::now()).exact_turn("thread", "turn"),
        Err(HistoryError::Deadline)
    );
    assert!(reader.methods.is_empty());
}
struct Effects {
    actions: Vec<RecoveryAction>,
}
impl RecoveryEffects for Effects {
    fn apply(
        &mut self,
        _: &RecoverySnapshot,
        _: &ConnectionScope,
        action: RecoveryAction,
    ) -> Result<RecoveryOutcome, RecoveryError> {
        self.actions.push(action);
        Ok(RecoveryOutcome::Applied(json!({"status":"reconciled"})))
    }
}
fn agent() -> RecoverySnapshot {
    RecoverySnapshot::from_record(json!({"id":"agent","rootId":"agent","epoch":0,"status":"running","accountKey":"default","threadId":"thread","turnId":"turn","inFlight":true,"autoWake":true})).expect("valid recovery fixture")
}
#[test]
fn observed_turn_consumer_applies_after_barrier_and_never_submits_input() {
    let idle = json!({"thread":{"id":"thread","status":{"type":"idle"}}});
    let mut reader = Reader {
        replies: vec![Ok(idle.clone()), Ok(page(turn())), Ok(idle)].into(),
        ..Default::default()
    };
    let mut effects = Effects { actions: vec![] };
    assert!(matches!(
        recovery::recover_turn(&agent(), scope(), &mut reader, &mut effects)
            .expect("valid recovery fixture"),
        RecoveryOutcome::Applied(_)
    ));
    assert!(reader.barrier);
    assert_eq!(effects.actions.len(), 1);
    assert_eq!(
        reader.methods,
        vec![
            HistoryMethod::Thread,
            HistoryMethod::Turns,
            HistoryMethod::Thread
        ]
    );
}
#[test]
fn unreadable_turn_retains_every_effect_and_active_turn_is_not_completion() {
    for result in [
        Err(HistoryError::Unavailable),
        Ok(json!({"thread":{"id":"thread","status":{"type":"active"}}})),
    ] {
        let mut reader = Reader {
            replies: vec![result].into(),
            ..Default::default()
        };
        let mut effects = Effects { actions: vec![] };
        let outcome = recovery::recover_turn(&agent(), scope(), &mut reader, &mut effects);
        assert!(
            outcome.is_err()
                || matches!(
                    outcome.expect("valid recovery fixture"),
                    RecoveryOutcome::Unconfirmed
                )
        );
        assert!(effects.actions.is_empty());
    }
}

#[test]
fn complete_history_outputs_and_calls_match_python() {
    let output = std::process::Command::new(python_test_support::prepared_python(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    ))
    .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recovery/history_oracle.py"))
    .output()
    .expect("valid recovery fixture");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("valid recovery fixture");
    assert_eq!(cases.len(), 12);
    for case in cases {
        let mut reader = Reader {
            replies: case["replies"]
                .as_array()
                .expect("valid recovery fixture")
                .iter()
                .map(|reply| {
                    if reply.get("rpcError").is_some() {
                        Err(HistoryError::Unsupported)
                    } else {
                        Ok(reply.clone())
                    }
                })
                .collect(),
            ..Default::default()
        };
        let result = HistoryRead::new(&mut reader, scope(), Instant::now() + READ_DEADLINE)
            .exact_turn("thread", "turn");
        assert_eq!(
            result.is_err(),
            case["failed"].as_bool().expect("valid recovery fixture"),
            "{}",
            case["name"]
        );
        if let Ok(result) = result {
            assert_eq!(
                result.unwrap_or(Value::Null),
                case["result"],
                "{}",
                case["name"]
            );
        }
        assert_eq!(json!(reader.calls), case["calls"], "{}", case["name"]);
    }
}
#[test]
fn exact_start_requires_unique_complete_client_receipt() {
    for (rows, accepted) in [
        (
            vec![
                json!({"id":"turn","status":"inProgress","items":[],"clientUserMessageId":"event"}),
            ],
            true,
        ),
        (
            vec![
                json!({"id":"turn","status":"inProgress","items":[],"clientUserMessageId":"foreign"}),
            ],
            false,
        ),
        (
            vec![
                json!({"id":"turn","status":"inProgress","items":[],"clientUserMessageId":"event","startOutcome":"not_applied"}),
            ],
            false,
        ),
    ] {
        let mut reader = Reader {
            replies: vec![Ok(json!({"data":rows}))].into(),
            ..Default::default()
        };
        assert_eq!(
            HistoryRead::new(&mut reader, scope(), Instant::now() + READ_DEADLINE)
                .exact_start("thread", "event")
                .expect("valid recovery fixture")
                .is_some(),
            accepted
        );
    }
    let mut reader = Reader {
        replies: vec![Ok(json!({"data":[
        {"id":"one","items":[],"clientUserMessageId":"event"},
        {"id":"two","items":[],"clientUserMessageId":"event"}]}))]
        .into(),
        ..Default::default()
    };
    assert_eq!(
        HistoryRead::new(&mut reader, scope(), Instant::now() + READ_DEADLINE)
            .exact_start("thread", "event"),
        Err(HistoryError::Identity)
    );
}

#[test]
fn legacy_start_matches_python_complete_history() {
    let output = std::process::Command::new(python_test_support::prepared_python(
        &std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
    ))
    .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/recovery/start_oracle.py"))
    .output()
    .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("cases");
    for case in cases.iter().filter(|c| c["history_case"] == true) {
        let mut reader = Reader {
            replies: case["replies"]
                .as_array()
                .expect("replies")
                .iter()
                .cloned()
                .map(Ok)
                .collect(),
            ..Default::default()
        };
        let actual = HistoryRead::new(&mut reader, scope(), Instant::now() + READ_DEADLINE)
            .exact_start("thread", "event");
        assert_eq!(
            actual.is_err(),
            case["failed"].as_bool().expect("failed"),
            "{}",
            case["name"]
        );
        if let Ok(value) = actual {
            assert_eq!(
                value.unwrap_or(Value::Null),
                case["accepted"],
                "{}",
                case["name"]
            );
        }
        assert_eq!(json!(reader.calls), case["calls"], "{}", case["name"]);
    }
}
