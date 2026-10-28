//! Complete reader outputs from frozen notification snapshots, after restart.
use base64::Engine;
use serde_json::{Value, json};
use std::{collections::BTreeMap, path::Path, sync::Arc};
use studio_runtime_schema::studio_errors::{
    MigrationPhase, MigrationRollbackOutcome, MigrationTarget,
};
use studio_runtime_services::{
    transcript,
    transcript_runtime::{self, PageRequest},
};
use studio_runtime_store::{RuntimeStore, StoreError};

#[tokio::test]
#[allow(clippy::disallowed_methods)]
async fn python_turn_readers_survive_restart_with_identical_cursors_and_receipts() {
    let root = tempfile::tempdir().expect("isolated state");
    let mode = if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some() {
        "full"
    } else {
        "default"
    };
    let fixtures = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/wave7_p13_transcript")
        .join(mode);
    let cases: Vec<Value> = serde_json::from_str(
        &std::fs::read_to_string(fixtures.join("cases.json"))
            .expect("frozen transcript corpus")
            .replace("@FIXTURE_ROOT@", &root.path().to_string_lossy()),
    )
    .expect("corpus JSON");
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
    let mut startup_refusals = 0;
    for case in cases {
        let name = case["path"].as_str().expect("snapshot name");
        let state = root.path().join(name);
        restore_snapshot(&fixtures.join(name), &state, root.path());
        if case["phase"] == "failed-turn" {
            // The retired oracle inserted this minimal error row directly, omitting
            // nullable timing fields required by both historical and current models.
            let db = rusqlite::Connection::open(state.join("analytics.sqlite3"))
                .expect("failed-turn fixture database");
            let raw: String = db
                .query_row(
                    "SELECT record FROM analytics_turns WHERE id='agent:thread:turn'",
                    [],
                    |row| row.get(0),
                )
                .expect("failed-turn fixture row");
            let mut record: Value = serde_json::from_str(&raw).expect("failed-turn fixture JSON");
            let object = record.as_object_mut().expect("failed-turn fixture object");
            for field in ["startedAt", "finishedAt", "firstOutputAt"] {
                assert!(
                    object.insert(field.into(), Value::Null).is_none(),
                    "fixture already has {field}"
                );
            }
            studio_runtime_store::analytics_capture::CaptureTable::Turns
                .validate(&record)
                .expect("failed-turn fixture satisfies the owning schema");
            assert_eq!(
                db.execute(
                    "UPDATE analytics_turns SET record=?1 WHERE id='agent:thread:turn'",
                    [record.to_string()],
                )
                .expect("adapt only the disposable failed-turn row"),
                1
            );
        }
        if case["phase"] == "malformed-turn" {
            // The migration now reads this intentionally malformed source before readers run.
            // Keep the frozen Python evidence, and require refusal without rewriting it.
            assert_eq!(
                case["pageError"],
                "invalid stored object analytics_turns/agent:thread:turn"
            );
            let originals = ["canvas.sqlite3", "analytics.sqlite3"].map(|database| {
                (
                    database,
                    std::fs::read(state.join(database)).expect("original database bytes"),
                )
            });
            for _ in 0..2 {
                let error = RuntimeStore::open(Some(&state))
                    .await
                    .err()
                    .expect("malformed historical turn refuses startup");
                assert!(
                    matches!(&error, StoreError::Migration {
                    id: "2026-10-26-terminal-errors",
                    target: MigrationTarget::Analytics,
                    phase: MigrationPhase::Validate,
                    rollback: MigrationRollbackOutcome::NotRequired,
                    source,
                    rollback_error: None,
                } if matches!(source.as_ref(), StoreError::SchemaValidation(object) if object == "analytics_turns")),
                    "{error:?}"
                );
                for (database, bytes) in &originals {
                    assert_eq!(
                        &std::fs::read(state.join(database)).expect("refused database bytes"),
                        bytes,
                        "startup refusal changed {database}"
                    );
                }
            }
            startup_refusals += 1;
            continue;
        }
        let orphan_replies = take_orphan_replies(&state);
        let fixture_sync_high = {
            let db = rusqlite::Connection::open(state.join("canvas.sqlite3"))
                .expect("restored snapshot reader");
            db.query_row(
                "SELECT MAX(seq) FROM (SELECT seq FROM sync_entities UNION ALL SELECT seq FROM sync_documents UNION ALL SELECT seq FROM sync_versions)",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("frozen global sync sequence")
        };
        let mut store = Arc::new(
            RuntimeStore::open(Some(&state))
                .await
                .unwrap_or_else(|error| {
                    panic!(
                        "Python store case {name}, phase {}: {error:?}",
                        case["phase"]
                    )
                }),
        );
        seed_transcript_replies(&store, orphan_replies).await;
        let db = store.test_reader().expect("migrated snapshot reader");
        let workspace_sequence: i64 = db
            .query_row(
                "SELECT seq FROM sync_entities WHERE collection='workspace' AND id='current'",
                [],
                |row| row.get(0),
            )
            .expect("migrated workspace sequence");
        assert_eq!(
            workspace_sequence,
            fixture_sync_high + 1,
            "v4 startup migrates the legacy workspace limits exactly once"
        );
        drop(db);
        if let Some(expected) = case["pageError"].as_str() {
            let before = sync_snapshot(&store);
            let error = transcript_runtime::page_at(
                &store,
                "agent".to_owned(),
                PageRequest::default(),
                1000.0,
            )
            .await
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
                transcript_runtime::page_at(&store, "agent".to_owned(), request, 1000.0).await,
                Err(transcript::TranscriptError::UnknownMessage)
            ));
        }
        if let Some(entries) = case["searchItems"].as_object() {
            for (identity, expected) in entries {
                assert_eq!(
                    &transcript::search_item(&store, identity.clone())
                        .await
                        .expect("search item"),
                    expected,
                    "search item {identity}"
                );
            }
        }
        if let Some(entries) = case["unavailableItems"].as_array() {
            for identity in entries {
                assert!(matches!(
                    transcript::search_item(&store, identity.as_str().expect("id").to_owned())
                        .await,
                    Err(transcript::TranscriptError::ChatUnavailable)
                ));
            }
        }
        if let Some(append) = case.get("append") {
            let barrier = Arc::new(tokio::sync::Barrier::new(2));
            let request = PageRequest {
                after: append["request"]["after"].as_str().map(str::to_owned),
                limit: Some(1),
                ..Default::default()
            };
            let before_snapshot = transcript_runtime::page_at_snapshot(
                &store,
                "agent".to_owned(),
                request.clone(),
                1000.0,
            )
            .await
            .expect("page and revision before append");
            assert_eq!(before_snapshot.0, append["before"]);
            let writer_store = Arc::clone(&store);
            let writer_barrier = Arc::clone(&barrier);
            let record = append["record"].clone();
            let created = append["created"].as_f64().expect("created");
            let writer = tokio::spawn(async move {
                writer_barrier.wait().await;
                writer_store
                    .transaction(move |tx| {
                        tx.execute(
                            "INSERT INTO runtime_items(id,agent,created,record) VALUES (?1,'agent',?2,?3)",
                            rusqlite::params![record["id"].as_str(), created, record.to_string()],
                        )?;
                        Ok(())
                    })
                    .await
                    .expect("concurrent append");
            });
            barrier.wait().await;
            let snapshot = transcript_runtime::page_at_snapshot(
                &store,
                "agent".to_owned(),
                request.clone(),
                1000.0,
            )
            .await
            .expect("snapshot page and revision");
            writer.await.expect("writer joined");
            let after_snapshot = transcript_runtime::page_at_snapshot(
                &store,
                "agent".to_owned(),
                request.clone(),
                1000.0,
            )
            .await
            .expect("page and revision after append");
            assert_eq!(after_snapshot.0, append["after"]);
            assert!(
                snapshot == before_snapshot || snapshot == after_snapshot,
                "page and transcript revision came from different committed snapshots"
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
                        "agent".to_owned(),
                        request,
                        case["now"].as_f64().unwrap_or(1000.0)
                    )
                    .await
                    .expect("page")
                ),
                native_text(page["value"].clone()),
                "phase {} request {req}",
                case["phase"]
            );
        }
        if case.get("search").is_some() {
            assert_eq!(
                transcript::search_work(&store, "needle".to_owned(), None, 50)
                    .await
                    .expect("global search"),
                case["search"]
            );
            assert_eq!(
                transcript::search_work(&store, "needle".to_owned(), Some("agent".to_owned()), 50)
                    .await
                    .expect("scoped search"),
                case["scoped"]
            );
            assert_eq!(
                transcript::search_item(&store, "agent:answer".to_owned())
                    .await
                    .expect("search item"),
                case["item"]
            );
            assert_eq!(
                transcript::search_item(&store, "chat".to_owned())
                    .await
                    .expect("room item"),
                case["roomItem"]
            );
        }
        if case["phase"] == "reasoning" {
            let first = transcript::pull_scope(Arc::clone(&store), "transcript:agent", "agent", 0)
                .await
                .expect("first reasoning pull");
            let committed = sync_snapshot(&store);
            drop(store);
            store = Arc::new(
                RuntimeStore::open(Some(&state))
                    .await
                    .expect("reasoning restart"),
            );
            let replay = transcript::pull_scope(Arc::clone(&store), "transcript:agent", "agent", 0)
                .await
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
                .map(|v| {
                    (
                        "INSERT".to_owned(),
                        format!("item:{}", v["id"].as_str().expect("id")),
                    )
                })
                .collect::<Vec<_>>();
            boundaries.extend(
                ["@order", "@meta", "@revision", "@floor"]
                    .into_iter()
                    .map(|id| ("INSERT".to_owned(), id.to_owned())),
            );
            boundaries.push(("UPDATE".to_owned(), "@revision".to_owned())); // cached observation result
            if std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_none() {
                boundaries.retain(|(_, id)| id == "item:agent:old-0" || id.starts_with('@'));
            }
            if let Some(faults) = case["faults"].as_array() {
                boundaries = faults
                    .iter()
                    .map(|v| {
                        (
                            v[0].as_str().expect("operation").to_owned(),
                            v[1].as_str().expect("identity").to_owned(),
                        )
                    })
                    .collect();
            }
            let before = sync_snapshot(&store);
            for (operation, identity) in boundaries {
                for timing in ["BEFORE", "AFTER"] {
                    fault_count += 1;
                    let operation_for_trigger = operation.clone();
                    let identity_for_trigger = identity.clone();
                    let timing_for_trigger = timing.to_owned();
                    store.transaction(move |tx| {
                        let operation = operation_for_trigger;
                        let identity = identity_for_trigger;
                        let timing = timing_for_trigger;
                        let literal = identity.replace('\'', "''");
                        let row = if operation == "DELETE" { "OLD" } else { "NEW" };
                        let sql_operation = if operation == "CACHE" {
                            "UPDATE"
                        } else {
                            operation.as_str()
                        };
                        let condition = if operation == "CACHE" { " AND NEW.payload IS NOT NULL" } else { "" };
                        tx.execute_batch(&format!("CREATE TEMP TRIGGER p13_fail {timing} {sql_operation} ON sync_entities WHEN {row}.collection='transcript:agent' AND {row}.id='{literal}'{condition} BEGIN SELECT RAISE(ABORT,'p13 failpoint'); END;"))?;
                        Ok(())
                    }).await.expect("install failpoint");
                    assert!(
                        transcript::pull_scope(
                            Arc::clone(&store),
                            "transcript:agent",
                            "agent",
                            after
                        )
                        .await
                        .is_err(),
                        "{timing} {operation} {identity}"
                    );
                    drop(store);
                    store = Arc::new(
                        RuntimeStore::open(Some(&state))
                            .await
                            .expect("restart after failed sync write"),
                    );
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
            }).await.expect("commit fault");
            assert!(
                transcript::pull_scope(Arc::clone(&store), "transcript:agent", "agent", after)
                    .await
                    .is_err()
            );
            drop(store);
            store = Arc::new(
                RuntimeStore::open(Some(&state))
                    .await
                    .expect("restart after commit failure"),
            );
            assert_eq!(sync_snapshot(&store), before, "commit failure leaked rows");
            let got =
                transcript::pull_scope(Arc::clone(&store), "transcript:agent", "agent", after)
                    .await
                    .expect("pull");
            assert_pull(&got, expected, fixture_sync_high, case["phase"] == "prune");
            let committed = sync_snapshot(&store);
            let replay =
                transcript::pull_scope(Arc::clone(&store), "transcript:agent", "agent", after)
                    .await
                    .expect("original cursor retry");
            assert_eq!(replay, got, "original cursor response changed");
            assert_eq!(
                sync_snapshot(&store),
                committed,
                "original cursor retry wrote state"
            );
            let seq = got["checkpoint"]["seq"].as_i64().expect("cursor");
            drop(store);
            let store = Arc::new(RuntimeStore::open(Some(&state)).await.expect("restart"));
            let next = transcript::pull_scope(Arc::clone(&store), "transcript:agent", "agent", seq)
                .await
                .expect("replay");
            assert_eq!(next["documents"], json!([]));
            assert_eq!(next["checkpoint"], got["checkpoint"]);
        }
    }
    assert_eq!(
        startup_refusals, 1,
        "only the malformed turn is refused at startup"
    );
    eprintln!("P13 injected write/commit boundaries: {fault_count}");
}
// The Python transcript-only captures contain replies with no tool owner. Move
// those replies into current completed owners after the real migration has run.
fn take_orphan_replies(state: &Path) -> Vec<(String, String, String)> {
    let db = rusqlite::Connection::open(state.join("canvas.sqlite3")).expect("fixture database");
    let mut statement = db.prepare("SELECT a.agent,a.alias,r.result FROM runtime_tool_request_aliases a JOIN runtime_tool_results r ON r.id=a.request LEFT JOIN runtime_tool_requests t ON t.id=a.request WHERE t.id IS NULL").expect("orphan replies");
    let replies = statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query orphan replies")
        .collect::<Result<Vec<_>, _>>()
        .expect("read orphan replies");
    drop(statement);
    db.execute("DELETE FROM runtime_tool_results WHERE id IN (SELECT a.request FROM runtime_tool_request_aliases a LEFT JOIN runtime_tool_requests t ON t.id=a.request WHERE t.id IS NULL)", []).expect("remove captured orphan result");
    db.execute("DELETE FROM runtime_tool_request_aliases WHERE request NOT IN (SELECT id FROM runtime_tool_requests)", []).expect("remove captured orphan alias");
    replies
}

async fn seed_transcript_replies(store: &RuntimeStore, replies: Vec<(String, String, String)>) {
    use studio_runtime_schema::persistence::{
        RequestIdentity, ToolRequestKind, ToolRequestOutcome, ToolRequestStatus,
    };
    use studio_runtime_store::request_results::{
        Admission, AdmittedRequest, RequestResults, ResultEnvelope,
    };
    #[derive(serde::Serialize)]
    struct Owner<'a> {
        id: &'a str,
        agent: &'a str,
        kind: ToolRequestKind,
        aliases: [&'a str; 1],
        stage: ToolRequestStatus,
        outcome: ToolRequestOutcome,
        admitted: &'a AdmittedRequest,
    }
    store
        .transaction(move |tx| {
            for (agent, alias, raw) in replies {
                let root = RequestIdentity::issue_root();
                let identity =
                    RequestIdentity::new("transcript-fixture", &[&agent], &root, &[&alias])
                        .expect("fixture identity");
                let Admission::Fresh(admitted) = RequestResults::admit(
                    tx,
                    &identity,
                    "transcript-fixture",
                    identity.issued_ms(),
                )
                .map_err(StoreError::from)?
                else {
                    panic!("new fixture identity");
                };
                let owner = Owner {
                    id: identity.id(),
                    agent: &agent,
                    kind: ToolRequestKind::ManagedTool,
                    aliases: [&alias],
                    stage: ToolRequestStatus::Completed,
                    outcome: ToolRequestOutcome::Applied,
                    admitted: &admitted,
                };
                let raw_owner = serde_json::to_string(&owner)?;
                tx.execute(
                    "INSERT INTO runtime_tool_requests(id,record) VALUES(?1,?2)",
                    rusqlite::params![identity.id(), raw_owner],
                )?;
                let result = studio_runtime_store::ToolResultRecord::from_value(
                    serde_json::from_str(&raw)?,
                )?;
                RequestResults::settle(
                    tx,
                    &admitted,
                    &ResultEnvelope {
                        actor: Some(agent),
                        reply: result,
                        references: vec![],
                    },
                )
                .map_err(StoreError::from)?;
            }
            Ok::<_, StoreError>(())
        })
        .await
        .expect("seed current transcript receipts");
}

fn assert_pull(got: &Value, expected: &Value, fixture_sync_high: i64, migrated_execution: bool) {
    let decode = |v: &Value| {
        let mut v = v.clone();
        for doc in v["documents"].as_array_mut().expect("documents") {
            doc["payload"] =
                serde_json::from_str(doc["payload"].as_str().expect("payload")).expect("JSON");
        }
        v
    };
    let mut expected = decode(expected);
    // The frozen Python snapshots predate the v4 workspace limits migration.
    // Its one startup write uses the shared allocator. Shift only newly
    // allocated transcript sequences; persisted cursors and pruning stay exact.
    for document in expected["documents"].as_array_mut().expect("documents") {
        // Preserve the frozen corpus while retiring its unused wire-only map.
        document["payload"]
            .as_object_mut()
            .expect("transcript payload")
            .remove("itemRevisions");
        let sequence = document["seq"].as_i64().expect("document sequence");
        if sequence > fixture_sync_high {
            document["seq"] = json!(sequence + 1);
        }
    }
    let checkpoint = expected["checkpoint"]["seq"]
        .as_i64()
        .expect("checkpoint sequence");
    if checkpoint > fixture_sync_high {
        expected["checkpoint"]["seq"] = json!(checkpoint + 1);
    }
    if migrated_execution {
        // m2026_10_28_receipts_execution.rs:168-189 moves activeExecution,
        // advancing the transcript revision once after the workspace allocation.
        assert_eq!(expected["checkpoint"]["seq"], fixture_sync_high);
        expected["checkpoint"]["seq"] = json!(fixture_sync_high + 2);
        let documents = expected["documents"].as_array_mut().expect("documents");
        assert_eq!(documents.len(), 1);
        assert_eq!(documents[0]["seq"], fixture_sync_high);
        documents[0]["seq"] = json!(fixture_sync_high + 2);
    }
    assert_eq!(decode(got), expected);
}

fn sync_snapshot(store: &RuntimeStore) -> Value {
    let db = store.test_reader().expect("reader");
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

// SQL and blob bytes are frozen once; every test owns fresh writable copies.
#[allow(clippy::disallowed_methods)]
fn restore_snapshot(fixture: &Path, state: &Path, root: &Path) {
    std::fs::create_dir(state).expect("snapshot directory");
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        let sql = std::fs::read_to_string(fixture.join(format!("{name}.sql")))
            .expect("frozen SQLite seed")
            .replace(
                "@FIXTURE_ROOT@",
                &root.to_string_lossy().replace('\'', "''"),
            );
        let db = rusqlite::Connection::open(state.join(name)).expect("snapshot database");
        studio_runtime_store::sqlite_durability::configure_writer(&db)
            .expect("snapshot durability");
        db.execute_batch(&sql).expect("restore frozen SQLite seed");
    }
    let blobs: BTreeMap<String, String> = serde_json::from_str(
        &std::fs::read_to_string(fixture.join("blobs.json")).expect("frozen blobs"),
    )
    .expect("blob map");
    for (relative, encoded) in blobs {
        let target = state.join("blobs").join(relative);
        std::fs::create_dir_all(target.parent().expect("blob parent")).expect("blob directory");
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("frozen blob bytes");
        std::fs::write(target, bytes).expect("restore blob");
    }
}
