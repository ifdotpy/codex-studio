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
    for case in cases {
        let mut store = RuntimeStore::open(Some(Path::new(case["path"].as_str().expect("path"))))
            .expect("Python store");
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
        if let Some(expected) = case.get("pull") {
            // Fail before and after each sync entity insert. No response cursor
            // may escape a transaction whose item/hash/order/meta writes failed.
            let item_count = case["pages"][0]["value"]["items"]
                .as_array()
                .expect("items")
                .len();
            let boundaries = if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
                (0..item_count + 3).collect::<Vec<_>>()
            } else {
                vec![0, item_count, item_count + 2]
            };
            let before = sync_snapshot(&store);
            for boundary in boundaries {
                for timing in ["BEFORE", "AFTER"] {
                    store.transaction(|tx| {
                        tx.execute_batch(&format!("CREATE TEMP TRIGGER p13_fail {timing} INSERT ON sync_entities WHEN NEW.collection='transcript:agent' AND (SELECT count(*) FROM sync_entities WHERE collection='transcript:agent')>={boundary} BEGIN SELECT RAISE(ABORT,'p13 failpoint'); END;"))?;
                        Ok(())
                    }).expect("install failpoint");
                    assert!(
                        transcript::pull_scope(&store, "transcript:agent", "agent", 0).is_err(),
                        "{timing} {boundary}"
                    );
                    drop(store);
                    store =
                        RuntimeStore::open(Some(Path::new(case["path"].as_str().expect("path"))))
                            .expect("restart after failed sync write");
                    assert_eq!(
                        sync_snapshot(&store),
                        before,
                        "partial sync state escaped {timing} {boundary}"
                    );
                }
            }
            let got = transcript::pull_scope(&store, "transcript:agent", "agent", 0).expect("pull");
            assert_pull(&got, expected);
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
