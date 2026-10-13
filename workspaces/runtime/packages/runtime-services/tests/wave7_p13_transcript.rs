//! Complete differential reader outputs from a Python notification replay, after restart.
#[path = "support/prepared_python.rs"]
mod prepared_python;
use serde_json::{Value, json};
use std::{path::Path, process::Command};
use studio_runtime_services::{
    transcript,
    transcript_runtime::{self, PageRequest},
};
use studio_runtime_store::RuntimeStore;

#[test]
fn python_turn_readers_survive_restart_with_identical_cursors_and_receipts() {
    let root = tempfile::tempdir().expect("isolated state");
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(prepared_python::prepared_python(
        &manifest.join("../../apps/server/src"),
    ))
    .arg(manifest.join("tests/wave7_p13_transcript/oracle.py"))
    .arg(root.path())
    .env("PYTHONDONTWRITEBYTECODE", "1")
    .output()
    .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("corpus JSON");
    eprintln!(
        "P13 corpus: {} states, {} complete pages, {} refused/invalid pages",
        cases.len(),
        cases
            .iter()
            .map(|c| c["pages"].as_array().expect("pages").len())
            .sum::<usize>(),
        cases
            .iter()
            .filter(|c| c.get("pageError").is_some())
            .count()
    );
    let mut fault_count = 0;
    for case in cases {
        let mut store = RuntimeStore::open(Some(Path::new(case["path"].as_str().expect("path"))))
            .expect("Python store");
        if let Some(expected) = case["pageError"].as_str() {
            let before = sync_snapshot(&store);
            let error =
                transcript_runtime::page_at(&store, "agent", &PageRequest::default(), 1000.0)
                    .expect_err("located/refusal error");
            assert_eq!(
                error.to_string(),
                expected,
                "phase {} Python {}",
                case["phase"],
                case["pythonOutcome"]
            );
            assert_eq!(sync_snapshot(&store), before, "refusal wrote state");
        }
        if let Some(identity) = case["unknownCursor"].as_str() {
            let request = PageRequest {
                around: Some(identity.to_owned()),
                limit: Some(1),
                ..Default::default()
            };
            assert!(matches!(
                transcript_runtime::page_at(&store, "agent", &request, 1000.0),
                Err(transcript::TranscriptError::UnknownMessage)
            ));
        }
        if let Some(entries) = case["searchItems"].as_object() {
            for (identity, expected) in entries {
                assert_eq!(
                    &transcript::search_item(&store, identity).expect("search item"),
                    expected,
                    "search item {identity}"
                );
            }
        }
        if let Some(entries) = case["unavailableItems"].as_array() {
            for identity in entries {
                assert!(matches!(
                    transcript::search_item(&store, identity.as_str().expect("id")),
                    Err(transcript::TranscriptError::ChatUnavailable)
                ));
            }
        }
        if let Some(append) = case.get("append") {
            let barrier = std::sync::Barrier::new(2);
            let request = PageRequest {
                after: append["request"]["after"].as_str().map(str::to_owned),
                limit: Some(1),
                ..Default::default()
            };
            std::thread::scope(|scope| {
                let writer = scope.spawn(|| {
                    barrier.wait();
                    store.transaction(|tx| {
                        tx.execute("INSERT INTO runtime_items(id,agent,created,record) VALUES (?1,'agent',?2,?3)", rusqlite::params![append["record"]["id"].as_str(), append["created"].as_f64(), append["record"].to_string()])?;
                        Ok(())
                    }).expect("concurrent append");
                });
                barrier.wait();
                let page = transcript_runtime::page_at(&store, "agent", &request, 1000.0)
                    .expect("snapshot page");
                assert!(
                    page == append["before"] || page == append["after"],
                    "page mixed two committed snapshots: {page}"
                );
                writer.join().expect("writer joined");
            });
            assert_eq!(
                transcript_runtime::page_at(&store, "agent", &request, 1000.0)
                    .expect("after append"),
                append["after"]
            );
        }
        for page in case["pages"].as_array().expect("pages") {
            let req = &page["request"];
            let request = PageRequest {
                before: req["before"].as_str().map(str::to_owned),
                around: req["around"].as_str().map(str::to_owned),
                after: req["after"].as_str().map(str::to_owned),
                limit: req["limit"].as_i64(),
            };
            assert_eq!(
                native_text(
                    transcript_runtime::page_at(
                        &store,
                        "agent",
                        &request,
                        case["now"].as_f64().unwrap_or(1000.0)
                    )
                    .expect("page")
                ),
                native_text(page["value"].clone()),
                "phase {} request {req}",
                case["phase"]
            );
        }
        if case.get("search").is_some() {
            assert_eq!(
                transcript::search_work(&store, "needle", None, 50).expect("global search"),
                case["search"]
            );
            assert_eq!(
                transcript::search_work(&store, "needle", Some("agent"), 50)
                    .expect("scoped search"),
                case["scoped"]
            );
            assert_eq!(
                transcript::search_item(&store, "agent:answer").expect("search item"),
                case["item"]
            );
            assert_eq!(
                transcript::search_item(&store, "chat").expect("room item"),
                case["roomItem"]
            );
        }
        if case["phase"] == "reasoning" {
            let first = transcript::pull_scope(&store, "transcript:agent", "agent", 0)
                .expect("first reasoning pull");
            let committed = sync_snapshot(&store);
            drop(store);
            store = RuntimeStore::open(Some(Path::new(case["path"].as_str().expect("path"))))
                .expect("reasoning restart");
            let replay = transcript::pull_scope(&store, "transcript:agent", "agent", 0)
                .expect("lost response retry");
            assert_eq!(
                replay, first,
                "original cursor must replay exact observed result"
            );
            assert_eq!(
                sync_snapshot(&store),
                committed,
                "original cursor retry must write nothing"
            );
        }
        if let Some(expected) = case.get("pull") {
            let after = case["pullAfter"].as_i64().unwrap_or(0);
            let mut boundaries = case["pages"][0]["value"]["items"]
                .as_array()
                .expect("items")
                .iter()
                .map(|v| ("INSERT", format!("item:{}", v["id"].as_str().expect("id"))))
                .collect::<Vec<_>>();
            boundaries.extend(
                ["@order", "@meta", "@revision", "@floor"]
                    .into_iter()
                    .map(|id| ("INSERT", id.to_owned())),
            );
            boundaries.push(("UPDATE", "@revision".to_owned())); // cached observation result
            if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none() {
                boundaries.retain(|(_, id)| id == "item:agent:old-0" || id.starts_with('@'));
            }
            if let Some(faults) = case["faults"].as_array() {
                boundaries = faults
                    .iter()
                    .map(|v| {
                        (
                            v[0].as_str().expect("operation"),
                            v[1].as_str().expect("identity").to_owned(),
                        )
                    })
                    .collect();
            }
            let before = sync_snapshot(&store);
            for (operation, identity) in boundaries {
                for timing in ["BEFORE", "AFTER"] {
                    fault_count += 1;
                    store.transaction(|tx| {
                        let literal = identity.replace('\'', "''");
                        let row = if operation == "DELETE" { "OLD" } else { "NEW" };
                        let sql_operation = if operation == "CACHE" { "UPDATE" } else { operation };
                        let condition = if operation == "CACHE" { " AND NEW.payload IS NOT NULL" } else { "" };
                        tx.execute_batch(&format!("CREATE TEMP TRIGGER p13_fail {timing} {sql_operation} ON sync_entities WHEN {row}.collection='transcript:agent' AND {row}.id='{literal}'{condition} BEGIN SELECT RAISE(ABORT,'p13 failpoint'); END;"))?;
                        Ok(())
                    }).expect("install failpoint");
                    assert!(
                        transcript::pull_scope(&store, "transcript:agent", "agent", after).is_err(),
                        "{timing} {operation} {identity}"
                    );
                    drop(store);
                    store =
                        RuntimeStore::open(Some(Path::new(case["path"].as_str().expect("path"))))
                            .expect("restart after failed sync write");
                    assert_eq!(
                        sync_snapshot(&store),
                        before,
                        "partial sync state escaped {timing} {operation} {identity}"
                    );
                }
            }
            fault_count += 1;
            // Deferred constraint fails the actual COMMIT, after all result writes.
            store.transaction(|tx| {
                tx.execute_batch("CREATE TEMP TABLE p13_parent(id INTEGER PRIMARY KEY); CREATE TEMP TABLE p13_commit(id INTEGER REFERENCES p13_parent(id) DEFERRABLE INITIALLY DEFERRED); CREATE TEMP TRIGGER p13_commit_fail AFTER UPDATE ON sync_entities WHEN NEW.collection='transcript:agent' AND NEW.id='@revision' BEGIN INSERT INTO p13_commit VALUES(1); END;")?;
                Ok(())
            }).expect("commit fault");
            assert!(transcript::pull_scope(&store, "transcript:agent", "agent", after).is_err());
            drop(store);
            store = RuntimeStore::open(Some(Path::new(case["path"].as_str().expect("path"))))
                .expect("restart after commit failure");
            assert_eq!(sync_snapshot(&store), before, "commit failure leaked rows");
            let got =
                transcript::pull_scope(&store, "transcript:agent", "agent", after).expect("pull");
            assert_pull(&got, expected);
            let committed = sync_snapshot(&store);
            let replay = transcript::pull_scope(&store, "transcript:agent", "agent", after)
                .expect("original cursor retry");
            assert_eq!(replay, got, "original cursor response changed");
            assert_eq!(
                sync_snapshot(&store),
                committed,
                "original cursor retry wrote state"
            );
            let seq = got["checkpoint"]["seq"].as_i64().expect("cursor");
            drop(store);
            let store = RuntimeStore::open(Some(Path::new(case["path"].as_str().expect("path"))))
                .expect("restart");
            let next =
                transcript::pull_scope(&store, "transcript:agent", "agent", seq).expect("replay");
            assert_eq!(next["documents"], json!([]));
            assert_eq!(next["checkpoint"], got["checkpoint"]);
        }
    }
    eprintln!("P13 injected write/commit boundaries: {fault_count}");
}
fn assert_pull(got: &Value, expected: &Value) {
    let decode = |v: &Value| {
        let mut v = v.clone();
        for doc in v["documents"].as_array_mut().expect("documents") {
            doc["payload"] =
                serde_json::from_str(doc["payload"].as_str().expect("payload")).expect("JSON");
        }
        v
    };
    assert_eq!(decode(got), decode(expected));
}

fn sync_snapshot(store: &RuntimeStore) -> Value {
    let db = store.read().expect("reader");
    let mut statement=db.prepare("SELECT collection,id,seq,hash,payload,deleted FROM sync_entities ORDER BY collection,id").expect("snapshot");
    let rows = statement
        .query_map([], |r| {
            Ok(json!([
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, i64>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, Option<String>>(4)?,
                r.get::<_, i64>(5)?
            ]))
        })
        .expect("rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("valid rows");
    json!({"rows":rows,"sequence":studio_runtime_store::SyncDocumentsRepository::entity_sequence(&db).expect("sequence")})
}

// The tool projection compiles this JSON text; object key order is not protocol meaning.
fn native_text(mut value: Value) -> Value {
    if let Some(items) = value["items"].as_array_mut() {
        for item in items {
            if item["title"] == "dynamicToolCall" {
                item["text"] = serde_json::from_str(item["text"].as_str().expect("tool text"))
                    .expect("compiled JSON");
            }
        }
    }
    value
}
