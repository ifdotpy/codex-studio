#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;

use serde_json::{Value, json};
use std::{
    path::{Path, PathBuf},
    process::Command,
    sync::{
        OnceLock,
        atomic::{AtomicUsize, Ordering},
    },
};
use studio_runtime_services::{
    account_transfer::{Ticket, TransferError},
    account_transfer_driver::{
        self as driver, DispatchOutcome, PreparedTransfer, TransferExecutor, TransferMethod,
        TransferPreparation, TransferTransport,
    },
};
use studio_runtime_store::{AccountTransferRepository, RuntimeStore};

const OPERATION: &str = "41e0b552-0b0c-4c5c-9c37-2e7483cd05c9";

fn template() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root = std::env::temp_dir().join(format!("wave7-p18-template-{}", std::process::id()));
        let source = Path::new(env!("CARGO_MANIFEST_DIR"));
        let output = Command::new(prepared_python_support::prepared_python(
            &source.join("../../apps/server/src"),
        ))
        .arg(source.join("tests/account_transfer/template.py"))
        .arg(&root)
        .output()
        .expect("template process");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        root
    })
}
fn fixture() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("isolated state");
    for file in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template().join(file), root.path().join(file)).expect("clone Python state");
    }
    root
}
fn operation(store: &RuntimeStore) -> Value {
    serde_json::from_str(
        &AccountTransferRepository::get(&store.read().expect("read"), OPERATION)
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
    fn send(&self, ticket: &Ticket, _: &str, _: &Value) -> Option<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.store
            .transaction(|_| Ok(()))
            .expect("transport is outside transaction");
        let stored = operation(self.store);
        assert_eq!(stored["members"]["lead"]["phase"], "submitted");
        assert_eq!(stored["members"]["lead"]["transportTicket"], json!(ticket));
        if matches!(self.behavior, Behavior::LostResponse) {
            None
        } else {
            Some(json!({"thread":{"id":"destination-thread"}}))
        }
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
    fn prepare(&self, input: &TransferPreparation) -> Result<PreparedTransfer, TransferError> {
        self.store
            .transaction(|_| Ok(()))
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
            }).expect("concurrent turn");
        }
        let mut ticket = ticket();
        if matches!(self.behavior, Behavior::ForeignTicket) {
            ticket.agent = "other".into();
        }
        Ok(PreparedTransfer {
            ticket,
            method: TransferMethod::Fork,
            params: json!({"threadId":"source-thread"}),
        })
    }
}

#[test]
fn executor_commits_intent_before_effect_and_never_resends_after_restart() {
    for behavior in [Behavior::Success, Behavior::LostResponse] {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("open");
        let executor = Executor {
            store: &store,
            behavior,
            calls: AtomicUsize::new(0),
        };
        let outcome =
            driver::execute_member(&store, &executor, OPERATION, "lead", 10.0).expect("execute");
        assert_eq!(
            outcome,
            if matches!(behavior, Behavior::Success) {
                DispatchOutcome::ReceiptSaved
            } else {
                DispatchOutcome::Unknown
            }
        );
        assert!(driver::execute_member(&store, &executor, OPERATION, "lead", 11.0).is_err());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 1);
        drop(store);
        let store = RuntimeStore::open(Some(root.path())).expect("restart");
        driver::recover(&store, 12.0).expect("recover");
        let executor = Executor {
            store: &store,
            behavior,
            calls: AtomicUsize::new(0),
        };
        assert!(driver::execute_member(&store, &executor, OPERATION, "lead", 13.0).is_err());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        driver::receive(
            &store,
            &ticket(),
            Some(json!({"thread":{"id":"destination-thread"}})),
            14.0,
        )
        .expect("late exact receipt");
        let saved = operation(&store);
        driver::receive(
            &store,
            &ticket(),
            Some(json!({"thread":{"id":"destination-thread"}})),
            15.0,
        )
        .expect("same receipt");
        assert_eq!(operation(&store), saved);
        for field in [
            "operation",
            "agent",
            "epoch",
            "account",
            "connection",
            "request",
        ] {
            let mut foreign = json!(ticket());
            foreign[field] = if field == "epoch" {
                json!(4)
            } else {
                json!("foreign")
            };
            assert!(
                driver::receive(
                    &store,
                    &serde_json::from_value(foreign).expect("ticket"),
                    Some(json!({"thread":{"id":"foreign"}})),
                    16.0
                )
                .is_err()
            );
            assert_eq!(operation(&store), saved);
        }
    }
}

#[test]
fn preparation_failures_do_not_send_or_strand_reading_members() {
    for behavior in [
        Behavior::Unavailable,
        Behavior::PreparationFailed,
        Behavior::BusyDuringPreparation,
        Behavior::ForeignTicket,
    ] {
        let root = fixture();
        let store = RuntimeStore::open(Some(root.path())).expect("open");
        let before = operation(&store);
        let executor = Executor {
            store: &store,
            behavior,
            calls: AtomicUsize::new(0),
        };
        assert!(driver::execute_member(&store, &executor, OPERATION, "lead", 10.0).is_err());
        assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
        if matches!(behavior, Behavior::Unavailable) {
            assert_eq!(operation(&store), before);
        } else {
            assert_eq!(operation(&store)["members"]["lead"]["phase"], "blocked");
        }
    }
}

#[test]
fn failpoints_around_each_transfer_write_preserve_restart_uncertainty() {
    for phase in ["reading", "submitted", "ready"] {
        for timing in ["BEFORE", "AFTER"] {
            let root = fixture();
            let store = RuntimeStore::open(Some(root.path())).expect("open");
            store.transaction(|tx| {
                for (index, event) in ["INSERT", "UPDATE"].iter().enumerate() {
                    tx.execute_batch(&format!("CREATE TEMP TRIGGER p18_failure_{index} {timing} {event} ON runtime_account_transfers WHEN json_extract(NEW.record,'$.members.lead.phase')='{phase}' BEGIN SELECT RAISE(ABORT,'injected'); END"))?;
                }
                Ok(())
            }).expect("install failpoint");
            let executor = Executor {
                store: &store,
                behavior: Behavior::Success,
                calls: AtomicUsize::new(0),
            };
            assert!(
                driver::execute_member(&store, &executor, OPERATION, "lead", 10.0).is_err(),
                "{timing}/{phase}"
            );
            assert_eq!(
                executor.calls.load(Ordering::SeqCst),
                usize::from(phase == "ready")
            );
            drop(store);
            let store = RuntimeStore::open(Some(root.path())).expect("restart");
            driver::recover(&store, 11.0).expect("recover");
            let expected = match phase {
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
                    driver::execute_member(&store, &executor, OPERATION, "lead", 12.0).is_err()
                );
                assert_eq!(executor.calls.load(Ordering::SeqCst), 0);
            }
        }
    }
}
