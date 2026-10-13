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
    account_transfer::{Action, Ticket},
    account_transfer_driver::{self as driver, DispatchOutcome, TransferTransport},
};
use studio_runtime_store::{AccountTransferRepository, RuntimeStore};
const OP: &str = "41e0b552-0b0c-4c5c-9c37-2e7483cd05c9";
fn template() -> &'static Path {
    static ROOT: OnceLock<PathBuf> = OnceLock::new();
    ROOT.get_or_init(|| {
        let root =
            std::env::temp_dir().join(format!("account-transfer-template-{}", std::process::id()));
        let output = Command::new(studio_runtime_services::agents_create::prepared_python())
            .arg(Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/account_transfer/template.py"))
            .arg(&root)
            .output()
            .expect("Python template");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        root
    })
}
fn state() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("isolated state");
    for db in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template().join(db), root.path().join(db)).expect("clone Python template");
    }
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
        &AccountTransferRepository::get(&store.read().expect("read"), OP)
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
    fn send(&self, _ticket: &Ticket, _method: &str, _params: &Value) -> Option<Value> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        // Acquiring the writer here would deadlock if native work ran in a transaction.
        self.store
            .transaction(|_| Ok(()))
            .expect("no transaction held during effect");
        self.result.clone()
    }
}
fn durable_reservation_precedes_native_and_unknown_is_never_resent() {
    let root = state();
    let store = RuntimeStore::open(Some(root.path())).expect("open");
    driver::begin_read(&store, OP, "lead", 10.0).expect("prepare");
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
            "thread/fork",
            json!({"threadId":"source-thread"}),
            11.0
        )
        .expect("unknown saved"),
        DispatchOutcome::Unknown
    );
    assert_eq!(operation(&store)["members"]["lead"]["phase"], "unknown");
    assert!(driver::send_once(&store, &native, &ticket(), "thread/fork", json!({}), 12.0).is_err());
    assert_eq!(native.calls.load(Ordering::SeqCst), 1);
    drop(native);
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    driver::recover(&store, 13.0).expect("recover");
    driver::receive(
        &store,
        &ticket(),
        Some(json!({"thread":{"id":"saved-fork"}})),
        14.0,
    )
    .expect("exact late receipt");
    assert_eq!(
        operation(&store)["members"]["lead"]["result"]["thread"]["id"],
        "saved-fork"
    );
    driver::receive(
        &store,
        &ticket(),
        Some(json!({"thread":{"id":"saved-fork"}})),
        15.0,
    )
    .expect("replay saved receipt");
    assert_eq!(operation(&store)["updated"], 14.0);
}
fn transaction_failpoints_roll_back_operation_agents_and_sync() {
    let root = state();
    let store = RuntimeStore::open(Some(root.path())).expect("open");
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
            let before = operation(&store);
            let agent = AccountTransferRepository::agent(&store.read().expect("read"), "lead")
                .expect("agent");
            store.transaction(|tx| {
                for (index,event) in ["INSERT","UPDATE"].iter().enumerate() {
                    tx.execute_batch(&format!("CREATE TEMP TRIGGER transfer_fail_{index} {timing} {event} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END"))?;
                }
                Ok(())
            }).expect("failpoint");
            assert!(
                driver::action(&store, OP, Action::Cancel, 20.0).is_err(),
                "{timing}/{table}"
            );
            assert_eq!(operation(&store), before, "{timing}/{table}");
            assert_eq!(
                AccountTransferRepository::agent(&store.read().expect("read"), "lead")
                    .expect("agent"),
                agent
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER transfer_fail_0; DROP TRIGGER transfer_fail_1;")
                        .map_err(Into::into)
                })
                .expect("clear failpoints");
        }
    }
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    driver::recover(&store, 21.0).expect("recover");
    driver::action(&store, OP, Action::Cancel, 22.0).expect("resume action");
    assert_eq!(operation(&store)["status"], "cancelled");
}

fn replay_and_conflict_have_no_writes() {
    let root = state();
    let store = RuntimeStore::open(Some(root.path())).expect("open");
    let before = operation(&store);
    driver::request(&store, OP, "lead", "target", "team").expect("exact replay");
    assert_eq!(operation(&store), before);
    assert!(driver::request(&store, OP, "lead", "changed", "team").is_err());
    assert_eq!(operation(&store), before);
    assert!(
        driver::request(
            &store,
            "52e0b552-0b0c-4c5c-9c37-2e7483cd05c9",
            "lead",
            "target",
            "team"
        )
        .is_err()
    );
    assert_eq!(operation(&store), before);
}

#[test]
fn durable_adapter_failpoint_matrix() {
    if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
        durable_reservation_precedes_native_and_unknown_is_never_resent();
        transaction_failpoints_roll_back_operation_agents_and_sync();
        replay_and_conflict_have_no_writes();
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
