#[path = "support/prepared_python.rs"]
mod python_test_support;
use futures_util::future::BoxFuture;
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

    fn read<'a>(
        &'a mut self,
        s: &ConnectionScope,
        m: HistoryMethod,
        params: Value,
        _: Instant,
    ) -> BoxFuture<'a, Result<HistoryReceipt, HistoryError>> {
        let mut scope = s.clone();
        Box::pin(async move {
            assert!(!self.barrier);
            self.methods.push(m);
            self.calls
                .push(json!({"method":m.as_str(),"params":params}));
            if self.foreign {
                scope.generation += 1;
            }
            Ok(HistoryReceipt {
                scope,
                body: self.replies.pop_front().expect("no extra native read")?,
            })
        })
    }
    fn barrier<'a>(
        &'a mut self,
        _: &ConnectionScope,
        _: Instant,
    ) -> BoxFuture<'a, Result<(), HistoryError>> {
        Box::pin(async move {
            self.barrier = true;
            Ok(())
        })
    }
}
fn turn() -> Value {
    json!({"id":"turn","status":"completed","items":[{"id":"item","type":"agentMessage","text":"complete"}]})
}
fn page(turn: Value) -> Value {
    json!({"data":[turn],"nextCursor":null})
}
async fn read(replies: Vec<Result<Value, HistoryError>>) -> Result<Option<Value>, HistoryError> {
    let mut reader = Reader {
        replies: replies.into(),
        ..Default::default()
    };
    HistoryRead::new(&mut reader, scope(), Instant::now() + READ_DEADLINE)
        .exact_turn("thread", "turn")
        .await
}
#[allow(clippy::disallowed_methods)]
async fn oracle(script: &'static str) -> std::process::Output {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/tests/recovery_history",
        move || {
            let source =
                std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
            let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests/recovery")
                .join(script);
            std::process::Command::new(python_test_support::prepared_python(&source))
                .arg(path)
                .output()
        },
    )
    .await
    .expect("oracle process task")
    .expect("oracle process")
}
#[tokio::test]
async fn exact_turn_keeps_complete_text_and_coalesces_identical_items() {
    let mut t = turn();
    let duplicate = t["items"][0].clone();
    t["items"]
        .as_array_mut()
        .expect("valid recovery fixture")
        .push(duplicate);
    assert_eq!(
        read(vec![Ok(page(t))])
            .await
            .expect("valid recovery fixture"),
        Some(turn())
    );
}
#[tokio::test]
async fn conflicting_duplicate_items_are_not_a_receipt() {
    let mut t = turn();
    t["items"]
        .as_array_mut()
        .expect("valid recovery fixture")
        .push(json!({"id":"item","type":"agentMessage","text":"changed"}));
    assert_eq!(read(vec![Ok(page(t))]).await, Err(HistoryError::Identity));
}
#[tokio::test]
async fn summary_items_are_paginated_and_terminal_metadata_is_rechecked() {
    let summary = json!({"id":"turn","status":"completed","items":[],"itemsView":"notLoaded"});
    let result = read(vec![
        Ok(page(summary.clone())),
        Ok(json!({"data":[turn()["items"][0]],"nextCursor":null})),
        Ok(page(summary)),
    ])
    .await
    .expect("valid recovery fixture")
    .expect("valid recovery fixture");
    assert_eq!(result["items"], turn()["items"]);
    assert_eq!(result["itemsView"], "full");
}
#[tokio::test]
async fn changed_terminal_metadata_is_refused() {
    let summary = json!({"id":"turn","status":"completed","items":[],"itemsView":"notLoaded"});
    let mut changed = summary.clone();
    changed["status"] = json!("failed");
    assert_eq!(
        read(vec![
            Ok(page(summary)),
            Ok(json!({"data":[]})),
            Ok(page(changed))
        ])
        .await,
        Err(HistoryError::Identity)
    );
}
#[tokio::test]
async fn unsupported_items_allows_only_full_target_fallback() {
    let summary = json!({"id":"turn","status":"completed","items":[],"itemsView":"summary"});
    assert_eq!(
        read(vec![
            Ok(page(summary)),
            Err(HistoryError::Unsupported),
            Ok(page(turn()))
        ])
        .await
        .expect("valid recovery fixture"),
        Some(turn())
    );
}
#[tokio::test]
async fn repeated_cursor_and_foreign_item_are_refused() {
    let empty = json!({"data":[],"nextCursor":"same"});
    assert!(read(vec![Ok(empty.clone()), Ok(empty)]).await.is_err());
    let mut t = turn();
    t["items"][0]["threadId"] = json!("foreign");
    assert_eq!(read(vec![Ok(page(t))]).await, Err(HistoryError::Identity));
}
#[tokio::test]
async fn foreign_transport_receipt_is_refused_before_use() {
    let mut reader = Reader {
        replies: vec![Ok(page(turn()))].into(),
        foreign: true,
        ..Default::default()
    };
    assert_eq!(
        HistoryRead::new(&mut reader, scope(), Instant::now() + READ_DEADLINE)
            .exact_turn("thread", "turn")
            .await,
        Err(HistoryError::Identity)
    );
}
#[tokio::test]
async fn expired_read_does_not_call_provider() {
    let mut reader = Reader::default();
    assert_eq!(
        HistoryRead::new(&mut reader, scope(), Instant::now())
            .exact_turn("thread", "turn")
            .await,
        Err(HistoryError::Deadline)
    );
    assert!(reader.methods.is_empty());
}
struct Effects {
    actions: Vec<RecoveryAction>,
}
impl RecoveryEffects for Effects {
    fn apply<'a>(
        &'a mut self,
        _: &'a RecoverySnapshot,
        _: &'a ConnectionScope,
        action: RecoveryAction,
    ) -> BoxFuture<'a, Result<studio_runtime_services::recovery::RecoveryEffectResult, RecoveryError>>
    {
        Box::pin(async move {
            self.actions.push(action);
            Ok(
                studio_runtime_services::recovery::RecoveryEffectResult::plain(
                    RecoveryOutcome::Applied(json!({"status":"reconciled"})),
                ),
            )
        })
    }
}
fn agent() -> RecoverySnapshot {
    RecoverySnapshot::from_record(json!({"id":"agent","rootId":"agent","epoch":0,"status":"running","accountKey":"default","threadId":"thread","turnId":"turn","inFlight":true,"autoWake":true})).expect("valid recovery fixture")
}
#[tokio::test]
async fn observed_turn_consumer_applies_after_barrier_and_never_submits_input() {
    let idle = json!({"thread":{"id":"thread","status":{"type":"idle"}}});
    let mut reader = Reader {
        replies: vec![Ok(idle.clone()), Ok(page(turn())), Ok(idle)].into(),
        ..Default::default()
    };
    let mut effects = Effects { actions: vec![] };
    assert!(matches!(
        recovery::recover_turn(&agent(), scope(), &mut reader, &mut effects)
            .await
            .expect("valid recovery fixture"),
        studio_runtime_services::recovery::RecoveryEffectResult {
            outcome: RecoveryOutcome::Applied(_),
            ..
        }
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
#[tokio::test]
async fn unreadable_turn_retains_every_effect_and_active_turn_is_not_completion() {
    for result in [
        Err(HistoryError::Unavailable),
        Ok(json!({"thread":{"id":"thread","status":{"type":"active"}}})),
    ] {
        let mut reader = Reader {
            replies: vec![result].into(),
            ..Default::default()
        };
        let mut effects = Effects { actions: vec![] };
        let outcome = recovery::recover_turn(&agent(), scope(), &mut reader, &mut effects).await;
        assert!(
            outcome.is_err()
                || matches!(
                    outcome.expect("valid recovery fixture").outcome,
                    RecoveryOutcome::Unconfirmed
                )
        );
        assert!(effects.actions.is_empty());
    }
}

#[tokio::test]
async fn complete_history_outputs_and_calls_match_python() {
    let output = oracle("history_oracle.py").await;
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
            .exact_turn("thread", "turn")
            .await;
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
#[tokio::test]
async fn exact_start_requires_unique_complete_client_receipt() {
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
                .await
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
            .exact_start("thread", "event")
            .await,
        Err(HistoryError::Identity)
    );
}

#[tokio::test]
async fn exact_start_reports_only_explicit_not_applied_receipts_as_negative_proof() {
    let mut reader = Reader {
        replies: vec![Ok(json!({"data":[{
            "id":"rejected",
            "threadId":"thread",
            "status":"failed",
            "items":[],
            "clientUserMessageId":"event",
            "startOutcome":"not_applied"
        }]}))]
        .into(),
        ..Default::default()
    };
    assert_eq!(
        HistoryRead::new(&mut reader, scope(), Instant::now() + READ_DEADLINE)
            .exact_start_outcome("thread", "event")
            .await
            .expect("explicit negative receipt"),
        studio_runtime_services::recovery_history::ExactStart::NotApplied
    );
    let mut unavailable = Reader {
        replies: vec![Err(HistoryError::Unavailable)].into(),
        ..Default::default()
    };
    assert_eq!(
        HistoryRead::new(&mut unavailable, scope(), Instant::now() + READ_DEADLINE)
            .exact_start_outcome("thread", "event")
            .await,
        Err(HistoryError::Unavailable)
    );
}

#[tokio::test]
async fn legacy_start_matches_python_complete_history() {
    let output = oracle("start_oracle.py").await;
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
            .exact_start("thread", "event")
            .await;
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
