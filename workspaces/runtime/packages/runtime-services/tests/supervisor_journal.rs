#![allow(clippy::unwrap_used)]
use serde_json::{Value, json};
use std::{fs, path::Path};
use studio_runtime_providers::supervisor_protocol::{
    HANDLE_LIMIT, HandleSnapshot, NativeId, ReceiptSnapshot, WriteDecision,
};
use studio_runtime_store::{
    python_json_with_separators, sha256_hex,
    supervisor_journal::{AppendResult, SupervisorJournal},
};

fn disposable_root() -> tempfile::TempDir {
    match std::env::var_os("STUDIO_SUPERVISOR_TEST_ROOT") {
        Some(root) => tempfile::tempdir_in(root).unwrap(),
        None => tempfile::tempdir().unwrap(),
    }
}

enum Seed {
    Corpus,
    Empty,
    NativeIds,
    Requests,
}

fn template(corpus: bool) -> tempfile::TempDir {
    template_mode(if corpus { Seed::Corpus } else { Seed::Empty })
}
fn template_mode(seed: Seed) -> tempfile::TempDir {
    let root = disposable_root();
    let directory = match seed {
        Seed::Corpus if std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() == Ok("1") => {
            "corpus-full"
        }
        Seed::Corpus => "corpus",
        Seed::Empty => "template-only",
        Seed::NativeIds => "native-id-template",
        Seed::Requests => "request-cwd",
    };
    let source = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/supervisor/frozen")
        .join(directory);
    for entry in fs::read_dir(source).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_str().unwrap();
        match name {
            "supervisor.sql" => crate::recovery_fixtures::database(
                root.path(),
                "supervisor.sqlite3",
                &fs::read_to_string(&path).unwrap(),
            ),
            "expected.sql" => crate::recovery_fixtures::database(
                &root.path().join("expected"),
                "supervisor.sqlite3",
                &fs::read_to_string(&path).unwrap(),
            ),
            _ => {
                fs::copy(&path, root.path().join(name)).unwrap();
            }
        }
    }
    root
}
#[test]
fn supervisor_python_written_decision_corpus() {
    let root = template(true);
    let cases: Vec<Value> =
        serde_json::from_slice(&fs::read(root.path().join("cases.json")).unwrap()).unwrap();
    let full = std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() == Ok("1");
    assert_eq!(cases.len(), if full { 159 } else { 51 });
    for case in cases {
        let state = HandleSnapshot::from_record(case["row"].clone()).unwrap();
        if case["action"] == "cursor" {
            assert_eq!(
                state
                    .cursor(
                        case["cursor"].as_u64().unwrap(),
                        case["ack"].as_bool().unwrap()
                    )
                    .err()
                    .map(|e| e.to_string()),
                case["error"].as_str().map(str::to_owned),
                "{}",
                case["name"]
            );
            continue;
        }
        let prior = if case["prior"].is_null() {
            None
        } else {
            Some(ReceiptSnapshot::from_record(case["prior"].clone()).unwrap())
        };
        let mut canonical = case["message"].clone();
        canonical.as_object_mut().unwrap().remove("id");
        let digest = sha256_hex(python_json_with_separators(&canonical, ",", ":").as_bytes());
        let result = state.write(
            prior.as_ref(),
            &digest,
            serde_json::from_value::<Option<NativeId>>(case["native"].clone())
                .unwrap()
                .as_ref(),
            &case["message"],
            0,
        );
        let (actual, outbound) = match result {
            Ok(WriteDecision::Duplicate { remote_id }) => (
                json!({"result":{"accepted":true,"duplicate":true,"remoteId":remote_id,"durableMs":0.0}}),
                Value::Null,
            ),
            Ok(WriteDecision::Accept {
                remote_id,
                outbound,
                ..
            }) => (
                json!({"result":{"accepted":true,"duplicate":false,"remoteId":remote_id,"durableMs":0.0}}),
                outbound,
            ),
            Err(error) => (json!({"error":error.to_string()}), Value::Null),
        };
        assert_eq!(actual, case["expected"], "{}", case["name"]);
        assert_eq!(outbound, case["outbound"], "{}", case["name"]);
    }
}
#[test]
fn supervisor_journal_receipts_replay_ack_and_restart() {
    let root = template(false);
    let journal = SupervisorJournal::open(root.path()).unwrap();
    let message = json!({"method":"echo","params":{"text":"é😀"},"id":23});
    let result = journal
        .accept(
            "fixture",
            "monitor:one",
            Some(&NativeId::Integer(23)),
            &message,
            100.0,
        )
        .unwrap();
    assert!(matches!(
        result,
        WriteDecision::Accept {
            remote_id: Some(NativeId::Integer(1)),
            ..
        }
    ));
    assert_eq!(
        journal
            .append(
                "fixture",
                1,
                1,
                "stdout",
                &json!({"id":1,"result":{"echo":"é😀"}})
            )
            .unwrap(),
        AppendResult::Saved
    );
    // Lost confirmation cannot append again, including after ACK pruning.
    assert_eq!(
        journal
            .append(
                "fixture",
                1,
                1,
                "stdout",
                &json!({"id":1,"result":{"echo":"é😀"}})
            )
            .unwrap(),
        AppendResult::Saved
    );
    assert!(journal.operation_status("fixture", "monitor:one").unwrap()["response"].is_null());
    journal.ack("fixture", 1).unwrap();
    assert_eq!(journal.handle("fixture").unwrap().unwrap().buffered, 0);
    assert_eq!(
        journal.operation_status("fixture", "monitor:one").unwrap()["response"]["result"]["echo"],
        "é😀"
    );
    let pending = json!({"notification":"still buffered"});
    journal.append("fixture", 1, 2, "stdout", &pending).unwrap();
    let pending_bytes = serde_json::to_string(&pending).unwrap().len() as u64;
    assert_eq!(
        journal.handle("fixture").unwrap().unwrap().buffered,
        pending_bytes
    );
    drop(journal);
    let journal = SupervisorJournal::open(root.path()).unwrap();
    assert_eq!(
        journal.handle("fixture").unwrap().unwrap().buffered,
        pending_bytes
    );
    journal.ack("fixture", 2).unwrap();
    assert_eq!(journal.handle("fixture").unwrap().unwrap().buffered, 0);
    assert!(matches!(
        journal
            .accept(
                "fixture",
                "monitor:one",
                Some(&NativeId::Integer(99)),
                &message,
                200.0
            )
            .unwrap(),
        WriteDecision::Duplicate {
            remote_id: Some(NativeId::Integer(1))
        }
    ));
    assert!(journal.events("fixture", 0, false).is_err());
    assert_eq!(
        journal.events("fixture", 2, false).unwrap()["events"],
        json!([])
    );
    assert!(
        journal
            .accept(
                "fixture",
                "monitor:one",
                Some(&NativeId::Integer(23)),
                &json!({"method":"different"}),
                200.0
            )
            .is_err()
    );
    journal.opened("fixture", "new", 456, "new-start").unwrap();
    assert_eq!(
        journal.operation_status("fixture", "monitor:one").unwrap(),
        json!({"accepted":false,"reason":"native_generation_changed"})
    );
    assert_eq!(
        journal
            .append("fixture", 1, 2, "stdout", &json!({"stale":true}))
            .unwrap(),
        AppendResult::Obsolete
    );
}
#[test]
fn supervisor_journal_sql_failpoints_roll_back_before_restart() {
    let source = template(false);
    let stages = [
        ("accept", "handles", "UPDATE"),
        ("accept", "operations", "INSERT"),
        ("append", "handles", "UPDATE"),
        ("append", "events", "INSERT"),
        ("append", "operations", "UPDATE"),
        ("ack", "events", "DELETE"),
        ("ack", "handles", "UPDATE"),
    ];
    for (action, table, event) in stages {
        let root = disposable_root();
        fs::copy(
            source.path().join("supervisor.sqlite3"),
            root.path().join("supervisor.sqlite3"),
        )
        .unwrap();
        let journal = SupervisorJournal::open(root.path()).unwrap();
        let message = json!({"method":"echo"});
        if action != "accept" {
            journal
                .accept(
                    "fixture",
                    "monitor:one",
                    Some(&NativeId::Integer(9)),
                    &message,
                    100.0,
                )
                .unwrap();
        }
        if action == "ack" {
            journal
                .append("fixture", 1, 1, "stdout", &json!({"id":1,"result":"done"}))
                .unwrap();
        }
        let before = durable_rows(root.path());
        let db = rusqlite::Connection::open(root.path().join("supervisor.sqlite3")).unwrap();
        db.execute_batch(&format!("CREATE TRIGGER stop BEFORE {event} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END;")).unwrap();
        let failed = match action {
            "accept" => journal
                .accept(
                    "fixture",
                    "monitor:one",
                    Some(&NativeId::Integer(9)),
                    &message,
                    100.0,
                )
                .is_err(),
            "append" => journal
                .append("fixture", 1, 1, "stdout", &json!({"id":1,"result":"done"}))
                .is_err(),
            _ => journal.ack("fixture", 1).is_err(),
        };
        assert!(failed, "{action}/{table}");
        drop(journal);
        db.execute_batch("DROP TRIGGER stop").unwrap();
        drop(db);
        let journal = SupervisorJournal::open(root.path()).unwrap();
        assert_eq!(durable_rows(root.path()), before, "{action}/{table}");
        if action == "accept" {
            assert!(matches!(
                journal
                    .accept(
                        "fixture",
                        "monitor:one",
                        Some(&NativeId::Integer(9)),
                        &message,
                        100.0
                    )
                    .unwrap(),
                WriteDecision::Accept {
                    remote_id: Some(NativeId::Integer(1)),
                    ..
                }
            ));
        }
    }
}
#[test]
fn supervisor_malformed_snapshots_and_capacity_refuse() {
    for row in [
        json!(null),
        json!({}),
        json!({"sequence":-1,"acknowledged":0,"rpc_sequence":0,"generation":1}),
        json!({"sequence":0,"acknowledged":1,"rpc_sequence":0,"generation":1}),
        json!({"sequence":0,"acknowledged":0,"rpc_sequence":i64::MAX,"generation":1}),
    ] {
        assert!(HandleSnapshot::from_record(row).is_err());
    }
    let state = HandleSnapshot::from_record(
        json!({"sequence":0,"acknowledged":0,"rpc_sequence":0,"generation":1}),
    )
    .unwrap();
    assert!(
        state
            .write(
                None,
                "digest",
                Some(&NativeId::Integer(3)),
                &json!({"method":"echo"}),
                HANDLE_LIMIT
            )
            .is_err()
    );
    assert!(state.retire("account", 1, true, true).is_err());
    assert!(state.retire("server-command:x", 2, true, true).is_err());
    assert!(state.retire("server-command:x", 1, false, true).is_err());
    assert!(state.retire("server-command:x", 1, true, false).is_err());
    assert!(state.retire("server-command:x", 1, true, true).is_ok());
}

fn durable_rows(path: &Path) -> Vec<String> {
    let db = rusqlite::Connection::open(path.join("supervisor.sqlite3")).unwrap();
    [
        "handles",
        "operations",
        "events",
        "child_identities",
        "degraded_handles",
        "recovery_events",
        "supervisor_state",
    ]
    .into_iter()
    .map(|table| {
        let mut statement = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .unwrap();
        let columns = statement.column_count();
        let rows = statement
            .query_map([], |row| {
                Ok((0..columns)
                    .map(|i| format!("{:?}", row.get_ref(i).unwrap()))
                    .collect::<Vec<_>>()
                    .join("|"))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        format!("{table}:{rows:?}")
    })
    .collect()
}
#[test]
fn supervisor_journal_lifecycle_failpoints_preserve_all_rows() {
    let source = template(false);
    let stages = [
        ("reserve", "handles", "INSERT"),
        ("reserve-existing", "handles", "UPDATE"),
        ("reserve-existing", "child_identities", "DELETE"),
        ("opened", "handles", "UPDATE"),
        ("opened", "child_identities", "INSERT"),
        ("opened", "degraded_handles", "DELETE"),
        ("close", "handles", "UPDATE"),
        ("close-unknown", "handles", "UPDATE"),
        ("close-unknown", "supervisor_state", "INSERT"),
        ("recover", "recovery_events", "INSERT"),
        ("recover", "recovery_events", "DELETE"),
        ("recover", "handles", "UPDATE"),
        ("recover", "degraded_handles", "INSERT"),
        ("retire", "events", "DELETE"),
        ("retire", "operations", "DELETE"),
        ("retire", "child_identities", "DELETE"),
        ("retire", "degraded_handles", "DELETE"),
        ("retire", "handles", "DELETE"),
        ("recovery", "supervisor_state", "INSERT"),
    ];
    for (action, table, event) in stages {
        let root = disposable_root();
        fs::copy(
            source.path().join("supervisor.sqlite3"),
            root.path().join("supervisor.sqlite3"),
        )
        .unwrap();
        let db = rusqlite::Connection::open(root.path().join("supervisor.sqlite3")).unwrap();
        db.execute_batch("UPDATE handles SET id='server-command:fixture'; UPDATE child_identities SET handle='server-command:fixture'; INSERT INTO degraded_handles VALUES ('server-command:fixture',123,'fixture-start',10); INSERT INTO operations(handle,operation_id,digest,accepted,generation) VALUES ('server-command:fixture','one','digest',10,1); INSERT INTO events VALUES ('server-command:fixture',0,'stdout','null',4,1);").unwrap();
        if action == "recover" && event == "DELETE" {
            for _ in 0..16 {
                db.execute("INSERT INTO recovery_events(at,handle,pid,start_time,outcome,detail) VALUES (1,'server-command:fixture',123,'fixture-start','missing-identity','old')", []).unwrap();
            }
        }
        let before = durable_rows(root.path());
        db.execute_batch(&format!("CREATE TRIGGER stop BEFORE {event} ON {table} BEGIN SELECT RAISE(ABORT,'injected'); END;")).unwrap();
        let journal = SupervisorJournal::open(root.path()).unwrap();
        let handle = "server-command:fixture";
        let failed = match action {
            "reserve" => journal.reserve("new", "signature", 100.0).is_err(),
            "reserve-existing" => journal.reserve(handle, "new", 100.0).is_err(),
            "opened" => journal.opened(handle, "new", 456, "new-start").is_err(),
            "close" => journal.close(handle, "closed", 100.0).is_err(),
            "close-unknown" => journal
                .close_unknown(
                    &journal.handle(handle).unwrap().unwrap(),
                    100.0,
                    &json!({"blocked":null}),
                )
                .is_err(),
            "recover" => journal
                .recovered(
                    &journal.handle(handle).unwrap().unwrap(),
                    "terminated",
                    "done",
                    100.0,
                )
                .is_err(),
            "retire" => journal.retire(handle, 1, true, true).is_err(),
            _ => journal.save_recovery(&json!({"blocked":null})).is_err(),
        };
        assert!(failed, "{action}/{table}");
        drop(journal);
        db.execute_batch("DROP TRIGGER stop").unwrap();
        drop(db);
        let restarted = SupervisorJournal::open(root.path()).unwrap();
        assert_eq!(durable_rows(root.path()), before, "{action}/{table}");
        drop(restarted);
    }
}

#[test]
fn supervisor_backend_admission_rejection_never_releases_the_owner() {
    use studio_runtime_providers::supervisor_protocol::{ConnectionIdentity, Ownership};
    let mut ownership = Ownership::default();
    let root = std::path::Path::new("/fixture");
    assert_eq!(
        ownership
            .attach(root, ConnectionIdentity::Backend("one"), root)
            .unwrap()
            .backend_owner(),
        Some("one")
    );
    assert!(
        ownership
            .attach(root, ConnectionIdentity::Backend("two"), root)
            .is_err()
    );
    ownership.detach(None);
    assert!(
        ownership
            .attach(root, ConnectionIdentity::Backend("two"), root)
            .is_err()
    );
    assert!(
        ownership
            .attach(
                root,
                ConnectionIdentity::Backend("one"),
                std::path::Path::new("/wrong")
            )
            .is_err()
    );
    ownership.detach(Some("one"));
    assert!(
        ownership
            .attach(root, ConnectionIdentity::Backend("two"), root)
            .is_ok()
    );
}

#[test]
fn supervisor_checked_records_reject_each_malformed_field() {
    let valid = json!({"sequence":0,"acknowledged":0,"rpc_sequence":0,"generation":1});
    for field in ["sequence", "acknowledged", "rpc_sequence", "generation"] {
        let mut missing = valid.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(
            HandleSnapshot::from_record(missing).is_err(),
            "missing {field}"
        );
        for bad in [
            Value::Null,
            json!(true),
            json!("1"),
            json!(-1),
            json!([]),
            json!({}),
        ] {
            let mut record = valid.clone();
            record[field] = bad;
            assert!(
                HandleSnapshot::from_record(record).is_err(),
                "invalid {field}"
            );
        }
    }
    assert!(ReceiptSnapshot::from_record(json!({"digest":"x","response":"invalid JSON"})).is_err());
    assert!(ReceiptSnapshot::from_record(json!({"digest":"x","generation":-1})).is_err());
    let exhausted = HandleSnapshot::from_record(
        json!({"sequence":0,"acknowledged":0,"rpc_sequence":i64::MAX-1,"generation":1}),
    )
    .unwrap();
    assert!(
        exhausted
            .write(
                None,
                "x",
                Some(&NativeId::Integer(1)),
                &json!({"method":"echo"}),
                0
            )
            .is_err()
    );
}

#[test]
fn supervisor_reopen_reservation_survives_crash_without_old_identity() {
    let root = template(false);
    let journal = SupervisorJournal::open(root.path()).unwrap();
    journal
        .close("fixture", "previous child exited", 50.0)
        .unwrap();
    journal.reserve("fixture", "new signature", 100.0).unwrap();
    drop(journal);
    let journal = SupervisorJournal::open(root.path()).unwrap();
    let reserved = journal.handle("fixture").unwrap().unwrap();
    assert!(reserved.closed_at.is_none());
    assert_eq!(reserved.pid, 0);
    assert!(reserved.start_time.is_none());
    assert!(reserved.pgid.is_none());
    assert_eq!(reserved.signature, "new signature");
    assert_eq!(reserved.snapshot.generation(), 1);
    // A reservation is not a spawned generation. Only the opened receipt binds it.
    assert_eq!(
        journal
            .opened("fixture", "new signature", 456, "new-start")
            .unwrap(),
        2
    );
}

#[test]
fn supervisor_inconsistent_child_identity_refuses_before_recovery() {
    let root = template(false);
    let db = rusqlite::Connection::open(root.path().join("supervisor.sqlite3")).unwrap();
    db.execute("UPDATE child_identities SET pid=456", [])
        .unwrap();
    drop(db);
    assert!(matches!(
        SupervisorJournal::open(root.path()),
        Err(studio_runtime_store::supervisor_journal::JournalError::Refused(_))
    ));
}

#[test]
fn supervisor_uncertain_recovery_remains_visible_after_each_restart() {
    let source = template(false);
    for outcome in ["missing-identity", "termination-failed"] {
        let root = disposable_root();
        fs::copy(
            source.path().join("supervisor.sqlite3"),
            root.path().join("supervisor.sqlite3"),
        )
        .unwrap();
        for restart in 0..18 {
            let journal = SupervisorJournal::open(root.path()).unwrap();
            let handles = journal.handles(true).unwrap();
            assert_eq!(
                handles.len(),
                1,
                "{outcome}: restart {restart} must not forget unknown ownership"
            );
            journal
                .recovered(&handles[0], outcome, "unknown", 100.0)
                .unwrap();
        }
        let journal = SupervisorJournal::open(root.path()).unwrap();
        assert!(
            journal
                .handle("fixture")
                .unwrap()
                .unwrap()
                .closed_at
                .is_none()
        );
        let db = rusqlite::Connection::open(root.path().join("supervisor.sqlite3")).unwrap();
        let count: i64 = db
            .query_row("SELECT count(*) FROM recovery_events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 16);
        journal
            .close("fixture", "operator acknowledgement", 200.0)
            .unwrap();
        drop(journal);
        assert!(
            SupervisorJournal::open(root.path())
                .unwrap()
                .handles(true)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn supervisor_kernel_identity_rejects_same_second_collision() {
    use studio_runtime_providers::supervisor_protocol::{StartMatch, compare_start};
    let old = "linux:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:123400";
    let new = "linux:aaaaaaaa-aaaa-aaaa-aaaa-aaaaaaaaaaaa:123401";
    assert_eq!(compare_start(old, Some(new)), StartMatch::Different);
    assert_eq!(compare_start(old, Some(old)), StartMatch::Exact);
    assert_eq!(
        compare_start(
            old,
            Some("linux:bbbbbbbb-bbbb-bbbb-bbbb-bbbbbbbbbbbb:123400")
        ),
        StartMatch::Different
    );
    assert_eq!(
        compare_start("Tue Oct 13 12:00:00 2026", Some(new)),
        StartMatch::Different
    );
    assert_eq!(compare_start(old, None), StartMatch::Gone);
}

#[test]
fn supervisor_open_requires_cwd_but_accepts_null() {
    use studio_runtime_providers::supervisor_protocol::Request;
    let root = template_mode(Seed::Requests);
    let cases: Vec<Value> =
        serde_json::from_slice(&fs::read(root.path().join("requests.json")).unwrap()).unwrap();
    assert_eq!(cases.len(), 3);
    for case in cases {
        assert_eq!(
            serde_json::from_value::<Request>(case["request"].clone()).is_ok(),
            case["accepted"].as_bool().unwrap(),
            "{case}"
        );
    }
}

/// Parity check for the `NativeId` contract: the Python supervisor journals Claude bridge
/// replies with text ids. The Rust supervisor must open that journal and keep every receipt.
#[test]
fn supervisor_python_written_text_native_ids_stay_exact() {
    let root = template_mode(Seed::NativeIds);
    let saved: Value =
        serde_json::from_slice(&fs::read(root.path().join("native-ids.json")).unwrap()).unwrap();
    let journal = SupervisorJournal::open(root.path()).unwrap();
    // A retry of each Python-accepted write is a duplicate with the exact Python remote id.
    for write in saved["writes"].as_array().unwrap() {
        let operation = write["operation"].as_str().unwrap();
        let native: Option<NativeId> = serde_json::from_value(write["native"].clone()).unwrap();
        let decision = journal
            .accept(
                "fixture",
                operation,
                native.as_ref(),
                &write["message"],
                200.0,
            )
            .unwrap();
        let WriteDecision::Duplicate { remote_id } = decision else {
            panic!("{operation}: a Python receipt must make the retry a duplicate");
        };
        assert_eq!(
            json!(remote_id),
            saved["results"][operation]["remoteId"],
            "{operation}"
        );
    }
    for (operation, expected) in saved["status"].as_object().unwrap() {
        assert_eq!(
            &journal.operation_status("fixture", operation).unwrap(),
            expected,
            "{operation}"
        );
    }
    // A Rust-accepted text id survives a restart exactly.
    let reply = NativeId::Text("claude:rust-reply".into());
    let message = json!({"id":"claude:rust-reply","result":{}});
    let WriteDecision::Accept {
        remote_id,
        outbound,
        ..
    } = journal
        .accept("fixture", "reply:rust", Some(&reply), &message, 300.0)
        .unwrap()
    else {
        panic!("a new reply must be accepted");
    };
    assert_eq!(remote_id, Some(reply.clone()));
    assert_eq!(outbound, message);
    // INTEGER affinity would store numeric text as a number; refuse it before the commit.
    let numeric = NativeId::Text("42".into());
    assert!(
        journal
            .accept(
                "fixture",
                "reply:numeric",
                Some(&numeric),
                &json!({"id":"42","result":{}}),
                300.0
            )
            .is_err()
    );
    drop(journal);
    let journal = SupervisorJournal::open(root.path()).unwrap();
    assert!(matches!(
        journal
            .accept("fixture", "reply:rust", Some(&reply), &message, 400.0)
            .unwrap(),
        WriteDecision::Duplicate { remote_id: Some(id) } if id == reply
    ));
    assert!(
        journal
            .accept(
                "fixture",
                "reply:numeric",
                Some(&numeric),
                &json!({"id":"42","result":{}}),
                400.0
            )
            .is_err()
    );
    let db = rusqlite::Connection::open(root.path().join("supervisor.sqlite3")).unwrap();
    let missing: i64 = db
        .query_row(
            "SELECT COUNT(*) FROM operations WHERE operation_id='reply:numeric'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(missing, 0);
}

#[test]
fn supervisor_refuses_a_native_id_it_cannot_read_exactly() {
    let root = template(false);
    let db = rusqlite::Connection::open(root.path().join("supervisor.sqlite3")).unwrap();
    db.execute(
        "INSERT INTO operations(handle,operation_id,digest,native_id,accepted,generation) VALUES ('fixture','monitor:real','digest',1.5,1,1)",
        [],
    )
    .unwrap();
    drop(db);
    assert!(SupervisorJournal::open(root.path()).is_err());
}
