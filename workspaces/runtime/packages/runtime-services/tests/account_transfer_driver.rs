use serde_json::{Value, json};
use std::sync::atomic::{AtomicUsize, Ordering};
use studio_runtime_services::{
    account_session_runtime::{ConnectionScope, RequestCertainty},
    account_transfer::{Action, Ticket},
    account_transfer_driver::{
        self as driver, DispatchOutcome, ProviderReceipt, TransferDispatch, TransferFuture,
        TransferTransport,
    },
};
use studio_runtime_store::{AccountTransferRepository, RuntimeStore};
const OP: &str = "41e0b552-0b0c-4c5c-9c37-2e7483cd05c9";
pub(crate) async fn state() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("isolated state");
    let path = root.path().display().to_string();
    let store = RuntimeStore::open(Some(root.path())).await.expect("open");
    let agent = json!({"id":"lead","rootId":"lead","epoch":3,"status":"idle","isLead":true,"autoWake":false,"accountKey":"source","threadId":"source-thread","cwd":path,"name":"Lead","model":"gpt-6-astra","accountTransferId":OP}).to_string();
    let operation = json!({"id":OP,"leadId":"lead","targetAccountKey":"target","status":"pending","scope":"team","created":1.0,"members":{"lead":{"phase":"waiting","sourceAccountKey":"source","sourceThreadId":"source-thread","name":"Lead","provider":"codex","lazy":false}},"requests":{}}).to_string();
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES('lead',?1)",
                [&agent],
            )?;
            tx.execute(
                "INSERT INTO runtime_account_transfers(id,record) VALUES(?1,?2)",
                [OP, operation.as_str()],
            )?;
            Ok(())
        })
        .await
        .expect("seed transfer state");
    drop(store);
    root
}
fn ticket() -> Ticket {
    Ticket {
        operation: OP.into(),
        agent: "lead".into(),
        epoch: 3,
        account: "target".into(),
        connection: "connection-1".into(),
        request: "request-1".into(),
    }
}
fn operation(store: &RuntimeStore) -> Value {
    serde_json::from_str(
        &AccountTransferRepository::get(&store.test_reader().expect("read"), OP)
            .expect("get")
            .expect("exists"),
    )
    .expect("json")
}
struct Native<'a> {
    store: &'a RuntimeStore,
    calls: AtomicUsize,
    result: Option<Value>,
}
impl TransferTransport for Native<'_> {
    fn send<'a>(
        &'a self,
        ticket: &'a Ticket,
        scope: &'a ConnectionScope,
        method: &'a str,
        _params: &'a Value,
    ) -> TransferFuture<'a, TransferDispatch> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            // Acquiring the writer here would deadlock if native work ran in a transaction.
            self.store
                .transaction(|_| Ok(()))
                .await
                .expect("no transaction held during effect");
            TransferDispatch {
                receipt: json!({
                    "operation": ticket.request,
                    "scope": scope,
                    "certainty": if self.result.is_some() {"written"} else {"unknown"},
                    "ticket": {"generation": scope.generation, "id": "request-1", "method": method}
                }),
                result: self.result.clone(),
                certainty: if self.result.is_some() {
                    RequestCertainty::Written
                } else {
                    RequestCertainty::Unknown
                },
            }
        })
    }
}
async fn durable_reservation_precedes_native_and_unknown_is_never_resent() {
    let root = state().await;
    let store = RuntimeStore::open(Some(root.path())).await.expect("open");
    driver::begin_read(&store, OP, "lead", 10.0)
        .await
        .expect("prepare");
    let native = Native {
        store: &store,
        calls: AtomicUsize::new(0),
        result: None,
    };
    assert_eq!(
        driver::send_once(
            &store,
            &native,
            &ticket(),
            ConnectionScope {
                account: "target".into(),
                connection: "connection-1".into(),
                generation: 1
            },
            "thread/fork",
            json!({"threadId":"source-thread"}),
            11.0
        )
        .await
        .expect("unknown saved"),
        DispatchOutcome::Unknown
    );
    assert_eq!(operation(&store)["members"]["lead"]["phase"], "unknown");
    assert!(
        driver::send_once(
            &store,
            &native,
            &ticket(),
            ConnectionScope {
                account: "target".into(),
                connection: "connection-1".into(),
                generation: 1
            },
            "thread/fork",
            json!({}),
            12.0
        )
        .await
        .is_err()
    );
    assert_eq!(native.calls.load(Ordering::SeqCst), 1);
    drop(native);
    drop(store);
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart");
    driver::recover(&store, 13.0).await.expect("recover");
    let scope = ConnectionScope {
        account: "target".into(),
        connection: "connection-1".into(),
        generation: 1,
    };
    driver::provider_receipt(
        &store,
        ProviderReceipt {
            operation: OP,
            agent: "lead",
            provider_operation: "request-1",
            receipt: json!({"operation":"request-1","scope":scope,"certainty":"written","ticket":{"generation":1,"id":"request-1","method":"thread/fork"}}),
            result: Some(json!({"thread":{"id":"saved-fork"}})),
            certainty: RequestCertainty::Written,
        },
        14.0,
    )
    .await
    .expect("exact late receipt");
    assert_eq!(
        operation(&store)["members"]["lead"]["result"]["thread"]["id"],
        "saved-fork"
    );
    driver::provider_receipt(
        &store,
        ProviderReceipt {
            operation: OP,
            agent: "lead",
            provider_operation: "request-1",
            receipt: json!({"operation":"request-1","scope":scope,"certainty":"written","ticket":{"generation":1,"id":"request-1","method":"thread/fork"}}),
            result: Some(json!({"thread":{"id":"saved-fork"}})),
            certainty: RequestCertainty::Written,
        },
        15.0,
    )
    .await
    .expect("replay saved receipt");
    assert_eq!(operation(&store)["updated"], 14.0);
}
async fn transaction_failpoints_roll_back_operation_agents_and_sync() {
    let root = state().await;
    let store = RuntimeStore::open(Some(root.path())).await.expect("open");
    let tables = if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
        vec![
            "runtime_account_transfers",
            "runtime_agents",
            "sync_entities",
        ]
    } else {
        vec!["runtime_agents"]
    };
    for table in tables {
        for timing in ["BEFORE", "AFTER"] {
            let table = table.to_owned();
            let timing = timing.to_owned();
            let trigger_table = table.clone();
            let trigger_timing = timing.clone();
            let before = operation(&store);
            let agent =
                AccountTransferRepository::agent(&store.test_reader().expect("read"), "lead")
                    .expect("agent");
            store.transaction(move |tx| {
                for (index,event) in ["INSERT","UPDATE"].iter().enumerate() {
                    tx.execute_batch(&format!("CREATE TEMP TRIGGER transfer_fail_{index} {trigger_timing} {event} ON {trigger_table} BEGIN SELECT RAISE(ABORT,'injected'); END"))?;
                }
                Ok(())
            }).await.expect("failpoint");
            assert!(
                driver::action(&store, OP, Action::Cancel, 20.0)
                    .await
                    .is_err(),
                "{timing}/{table}"
            );
            assert_eq!(operation(&store), before, "{timing}/{table}");
            assert_eq!(
                AccountTransferRepository::agent(&store.test_reader().expect("read"), "lead")
                    .expect("agent"),
                agent
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER transfer_fail_0; DROP TRIGGER transfer_fail_1;")
                        .map_err(Into::into)
                })
                .await
                .expect("clear failpoints");
        }
    }
    drop(store);
    let store = RuntimeStore::open(Some(root.path()))
        .await
        .expect("restart");
    driver::recover(&store, 21.0).await.expect("recover");
    driver::action(&store, OP, Action::Cancel, 22.0)
        .await
        .expect("resume action");
    assert_eq!(operation(&store)["status"], "cancelled");
}

async fn replay_and_conflict_have_no_writes() {
    let root = state().await;
    let store = RuntimeStore::open(Some(root.path())).await.expect("open");
    let before = operation(&store);
    driver::request(&store, OP, "lead", "target", "team", "codex", 2.0)
        .await
        .expect("exact replay");
    assert_eq!(operation(&store), before);
    assert!(
        driver::request(&store, OP, "lead", "changed", "team", "codex", 3.0)
            .await
            .is_err()
    );
    assert_eq!(operation(&store), before);
    assert!(
        driver::request(
            &store,
            "52e0b552-0b0c-4c5c-9c37-2e7483cd05c9",
            "lead",
            "target",
            "team",
            "codex",
            4.0
        )
        .await
        .is_err()
    );
    assert_eq!(operation(&store), before);
}

#[tokio::test]
async fn durable_adapter_failpoint_matrix() {
    if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
        durable_reservation_precedes_native_and_unknown_is_never_resent().await;
        transaction_failpoints_roll_back_operation_agents_and_sync().await;
        replay_and_conflict_have_no_writes().await;
    } else {
        // The default witness checks transaction rollback without a filesystem;
        // real fsync/restart and publication failpoints run in the full corpus.
        let mut db = rusqlite::Connection::open_in_memory().expect("SQLite");
        db.execute_batch(
            "CREATE TABLE runtime_account_transfers(id TEXT PRIMARY KEY,record TEXT NOT NULL)",
        )
        .expect("Python table shape");
        let raw = json!({"id":OP,"leadId":"lead","targetAccountKey":"target","status":"pending","created":1.0,"members":{}}).to_string();
        {
            let tx = db.transaction().expect("transaction");
            AccountTransferRepository::put(&tx, OP, &raw).expect("validated write");
        }
        assert!(
            AccountTransferRepository::get(&db, OP)
                .expect("read after rollback")
                .is_none()
        );
        let tx = db.transaction().expect("transaction");
        AccountTransferRepository::put(&tx, OP, &raw).expect("validated write");
        tx.commit().expect("commit");
        assert_eq!(
            AccountTransferRepository::get(&db, OP).expect("read committed row"),
            Some(raw)
        );
    }
}
