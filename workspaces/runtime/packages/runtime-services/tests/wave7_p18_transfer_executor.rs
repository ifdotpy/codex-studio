use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use studio_runtime_services::{
    account_session_runtime::{ConnectionScope, RequestCertainty},
    account_transfer::{Ticket, TransferError},
    account_transfer_driver::{
        self as driver, DispatchOutcome, PreparedTransfer, ProviderReceipt, TransferDispatch,
        TransferExecutor, TransferFuture, TransferMethod, TransferPreparation, TransferTransport,
    },
};
use studio_runtime_store::{AccountTransferRepository, RuntimeStore};

const OPERATION: &str = "41e0b552-0b0c-4c5c-9c37-2e7483cd05c9";

async fn fixture() -> tempfile::TempDir {
    crate::account_transfer_driver::state().await
}
fn operation(store: &RuntimeStore) -> Value {
    serde_json::from_str(
        &AccountTransferRepository::get(&store.test_reader().expect("read"), OPERATION)
            .expect("get")
            .expect("Python operation"),
    )
    .expect("record")
}
fn ticket() -> Ticket {
    Ticket {
        operation: OPERATION.into(),
        agent: "lead".into(),
        epoch: 3,
        account: "target".into(),
        connection: "retained-connection".into(),
        request: "exact-native-request".into(),
    }
}
fn exact_receipt(ticket: &Ticket, scope: &ConnectionScope, certainty: &str) -> Value {
    json!({
        "operation": ticket.request,
        "scope": scope,
        "certainty": certainty,
        "ticket": {"generation": scope.generation, "id": "fixture", "method": "thread/fork"},
    })
}
#[derive(Clone, Copy)]
enum Behavior {
    Success,
    Unavailable,
    PreparationFailed,
    LostResponse,
    BusyDuringPreparation,
    ForeignTicket,
}
struct Executor<'a> {
    store: &'a RuntimeStore,
    behavior: Behavior,
    calls: AtomicUsize,
}
impl TransferTransport for Executor<'_> {
    fn send<'a>(
        &'a self,
        ticket: &'a Ticket,
        scope: &'a ConnectionScope,
        method: &'a str,
        params: &'a Value,
    ) -> TransferFuture<'a, TransferDispatch> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            self.store
                .transaction(|_| Ok(()))
                .await
                .expect("transport is outside transaction");
            let stored = operation(self.store);
            assert_eq!(stored["members"]["lead"]["phase"], "submitted");
            assert_eq!(
                stored["members"]["lead"]["providerIntent"]["operation"],
                ticket.request
            );
            assert_eq!(stored["members"]["lead"]["nativeMethod"], method);
            assert_eq!(stored["members"]["lead"]["nativeParams"], params.clone());
            let lost_response = matches!(self.behavior, Behavior::LostResponse);
            let certainty = if lost_response {
                RequestCertainty::Unknown
            } else {
                RequestCertainty::Written
            };
            TransferDispatch {
                receipt: exact_receipt(ticket, scope, certainty.as_str()),
                result: (!lost_response).then(|| json!({"thread":{"id":"destination-thread"}})),
                certainty,
            }
        })
    }
}
impl TransferExecutor for Executor<'_> {
    fn available(&self) -> Result<(), TransferError> {
        if matches!(self.behavior, Behavior::Unavailable) {
            Err(TransferError::Requires(
                "account-transfer/provider-history-executor",
            ))
        } else {
            Ok(())
        }
    }
    fn prepare<'a>(
        &'a self,
        input: &'a TransferPreparation,
    ) -> TransferFuture<'a, Result<PreparedTransfer, TransferError>> {
        Box::pin(async move {
            self.store
                .transaction(|_| Ok(()))
                .await
                .expect("history is outside transaction");
            assert_eq!(input.operation()["members"]["lead"]["phase"], "reading");
            assert_eq!(input.agent()["threadId"], "source-thread");
            assert_eq!(operation(self.store)["members"]["lead"]["phase"], "reading");
            if matches!(self.behavior, Behavior::PreparationFailed) {
                return Err(TransferError::Refused("Fixture history unavailable"));
            }
            if matches!(self.behavior, Behavior::BusyDuringPreparation) {
                self.store.transaction(|tx| {
                    tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.inFlight',json('true')) WHERE id='lead'", [])?;
                    Ok(())
                }).await.expect("concurrent turn");
            }
            let mut ticket = ticket();
            if matches!(self.behavior, Behavior::ForeignTicket) {
                ticket.agent = "other".into();
            }
            Ok(PreparedTransfer {
                ticket,
                scope: ConnectionScope {
                    account: "target".into(),
                    connection: "retained-connection".into(),
                    generation: 1,
                },
                method: TransferMethod::Fork,
                params: json!({"threadId":"source-thread"}),
            })
        })
    }
}

#[tokio::test]
async fn executor_commits_intent_before_effect_and_never_resends_after_restart() {
    for behavior in [Behavior::Success, Behavior::LostResponse] {
        let root = fixture().await;
        let store = RuntimeStore::open(Some(root.path())).await.expect("open");
        let executor = Executor {
            store: &store,
            behavior,
            calls: AtomicUsize::new(0),
        };
        let outcome = driver::execute_member(&store, &executor, OPERATION, "lead", 10.0)
            .await
            .expect("execute");
        assert_eq!(
            outcome,
            if matches!(behavior, Behavior::Success) {
                DispatchOutcome::ReceiptSaved
            } else {
                DispatchOutcome::Unknown
            }
        );
        assert!(
            driver::execute_member(&store, &executor, OPERATION, "lead", 11.0)
                .await
                .is_err()
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        drop(store);
        let store = RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart");
        driver::recover(&store, 12.0).await.expect("recover");
        let executor = Executor {
            store: &store,
            behavior,
            calls: AtomicUsize::new(0),
        };
        assert!(
            driver::execute_member(&store, &executor, OPERATION, "lead", 13.0)
                .await
                .is_err()
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        let scope = ConnectionScope {
            account: "target".into(),
            connection: "retained-connection".into(),
            generation: 1,
        };
        let late = exact_receipt(&ticket(), &scope, "written");
        driver::provider_receipt(
            &store,
            ProviderReceipt {
                operation: OPERATION,
                agent: "lead",
                provider_operation: "exact-native-request",
                receipt: late.clone(),
                result: Some(json!({"thread":{"id":"destination-thread"}})),
                certainty: RequestCertainty::Written,
            },
            14.0,
        )
        .await
        .expect("late exact receipt");
        let saved = operation(&store);
        driver::provider_receipt(
            &store,
            ProviderReceipt {
                operation: OPERATION,
                agent: "lead",
                provider_operation: "exact-native-request",
                receipt: late,
                result: Some(json!({"thread":{"id":"destination-thread"}})),
                certainty: RequestCertainty::Written,
            },
            15.0,
        )
        .await
        .expect("same receipt");
        assert_eq!(operation(&store), saved);
        for field in ["operation", "connection", "request"] {
            let mut receipt = exact_receipt(&ticket(), &scope, "written");
            match field {
                "operation" => receipt["operation"] = json!("foreign-operation"),
                "connection" => receipt["scope"]["connection"] = json!("foreign-connection"),
                _ => receipt["ticket"]["id"] = json!("foreign-request"),
            }
            assert!(
                driver::provider_receipt(
                    &store,
                    ProviderReceipt {
                        operation: OPERATION,
                        agent: "lead",
                        provider_operation: "exact-native-request",
                        receipt,
                        result: Some(json!({"thread":{"id":"foreign"}})),
                        certainty: RequestCertainty::Written,
                    },
                    16.0,
                )
                .await
                .is_err()
            );
            assert_eq!(operation(&store), saved);
        }
    }
}

#[tokio::test]
async fn preparation_failures_do_not_send_or_strand_reading_members() {
    for behavior in [
        Behavior::Unavailable,
        Behavior::PreparationFailed,
        Behavior::BusyDuringPreparation,
        Behavior::ForeignTicket,
    ] {
        let root = fixture().await;
        let store = RuntimeStore::open(Some(root.path())).await.expect("open");
        let before = operation(&store);
        let executor = Executor {
            store: &store,
            behavior,
            calls: AtomicUsize::new(0),
        };
        assert!(
            driver::execute_member(&store, &executor, OPERATION, "lead", 10.0)
                .await
                .is_err()
        );
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        if matches!(behavior, Behavior::Unavailable) {
            assert_eq!(operation(&store), before);
        } else {
            assert_eq!(operation(&store)["members"]["lead"]["phase"], "blocked");
        }
    }
}

#[tokio::test]
async fn failpoints_around_each_transfer_write_preserve_restart_uncertainty() {
    for phase in ["reading", "submitted", "ready"] {
        for timing in ["BEFORE", "AFTER"] {
            let root = fixture().await;
            let store = RuntimeStore::open(Some(root.path())).await.expect("open");
            let phase = phase.to_owned();
            let failpoint_phase = phase.clone();
            let timing = timing.to_owned();
            let trigger_timing = timing.clone();
            store.transaction(move |tx| {
                for (index, event) in ["INSERT", "UPDATE"].iter().enumerate() {
                    tx.execute_batch(&format!("CREATE TEMP TRIGGER p18_failure_{index} {trigger_timing} {event} ON runtime_account_transfers WHEN json_extract(NEW.record,'$.members.lead.phase')='{failpoint_phase}' BEGIN SELECT RAISE(ABORT,'injected'); END"))?;
                }
                Ok(())
            }).await.expect("install failpoint");
            let executor = Executor {
                store: &store,
                behavior: Behavior::Success,
                calls: AtomicUsize::new(0),
            };
            assert!(
                driver::execute_member(&store, &executor, OPERATION, "lead", 10.0)
                    .await
                    .is_err(),
                "{timing}/{phase}"
            );
            assert_eq!(
                executor.calls.load(Ordering::SeqCst),
                usize::from(phase == "ready")
            );
            drop(store);
            let store = RuntimeStore::open(Some(root.path()))
                .await
                .expect("restart");
            driver::recover(&store, 11.0).await.expect("recover");
            let expected = match phase.as_str() {
                "reading" => "waiting",
                "submitted" => "blocked",
                _ => "unknown",
            };
            assert_eq!(
                operation(&store)["members"]["lead"]["phase"],
                expected,
                "{timing}/{phase}"
            );
            if phase == "ready" {
                let executor = Executor {
                    store: &store,
                    behavior: Behavior::Success,
                    calls: AtomicUsize::new(0),
                };
                assert!(
                    driver::execute_member(&store, &executor, OPERATION, "lead", 12.0)
                        .await
                        .is_err()
                );
                assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
            }
        }
    }
}
