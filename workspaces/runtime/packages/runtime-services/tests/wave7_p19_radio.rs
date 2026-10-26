use crate::prepared_python_support as python_support;
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
use studio_runtime_services::{
    host_project_runtime::PreparedHost,
    projects::ProjectsError,
    radio_runtime::{
        self, Interrupt, InterruptOutcome, InterruptReceipt, InterruptTransport, RadioIngress,
    },
};
use studio_runtime_store::{
    AgentId, AgentRecord, QueueEvent, QueueEventId, QueueEventRepository, RuntimeStore,
};

struct Ingress;
impl RadioIngress for Ingress {
    fn enqueue(
        &mut self,
        db: &rusqlite::Transaction<'_>,
        agent: &str,
        id: &str,
        text: &str,
        _now: f64,
    ) -> Result<(), ProjectsError> {
        QueueEventRepository::put(
            db,
            &QueueEvent {
                id: QueueEventId::from(id),
                agent: AgentId::from(agent),
                kind: "radio_turn".to_owned(),
                text: text.to_owned(),
                status: "pending".to_owned(),
                created: 0.0,
                epoch: 1,
                turn_id: None,
                error: None,
            },
        )?;
        Ok(())
    }
}
struct RadioCorpus {
    _root: tempfile::TempDir,
    databases: Vec<(String, Vec<u8>)>,
    cases: Vec<Value>,
}
fn corpus() -> RadioCorpus {
    // Oracle paths remain below this test-owned root through restart replay.
    let template = python_support::disposable_state_dir();
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let script = manifest.join("tests/wave7_p19/oracle.py");
    let mut command = Command::new(python_support::prepared_python(
        &manifest.join("../../apps/server/src"),
    ));
    command
        .env("TMPDIR", template.path())
        .arg(&script)
        .arg(template.path());
    if std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() == Ok("1") {
        command.arg("--full");
    }
    let output = python_support::output_with_timeout(
        &mut command,
        &script,
        python_support::PYTHON_ORACLE_TIMEOUT,
    )
    .unwrap_or_else(|error| panic!("could not run P19 Python oracle: {error}"));
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let databases = ["canvas.sqlite3", "analytics.sqlite3"]
        .into_iter()
        .map(|name| {
            (
                name.to_owned(),
                fs::read(template.path().join(name)).expect("template database"),
            )
        })
        .collect();
    RadioCorpus {
        databases,
        cases: serde_json::from_slice(&output.stdout).expect("corpus JSON"),
        _root: template,
    }
}
fn copy_template(corpus: &RadioCorpus, root: &Path) {
    for (name, contents) in &corpus.databases {
        fs::write(root.join(name), contents).expect("copy template database");
    }
}
async fn seed(store: &RuntimeStore, snapshot: &Value) {
    let snapshot = snapshot.clone();
    let identity = store.root().join("multi-server");
    fs::create_dir_all(&identity).expect("identity directory");
    fs::write(
        identity.join("identity.json"),
        r#"{"serverId":"local-test","label":"fixture","publicKey":"fixture"}"#,
    )
    .expect("identity");
    store
        .set_local_server_id(Some("local-test".to_owned()))
        .expect("register fixture server identity");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&identity, fs::Permissions::from_mode(0o700)).expect("private dir");
        fs::set_permissions(
            identity.join("identity.json"),
            fs::Permissions::from_mode(0o600),
        )
        .expect("private identity");
    }

    store
        .transaction(move |db| {
            for (table, rows) in snapshot.as_object().expect("snapshot") {
                let table_name = match table.as_str() {
                    "sync_entities" => "sync_entities".to_owned(),
                    "sidebar_order" => "runtime_sidebar_order".to_owned(),
                    _ => format!("runtime_{table}"),
                };
                db.execute(&format!("DELETE FROM {table_name}"), [])?;
                for row in rows.as_array().expect("rows") {
                    let fields = row.as_object().expect("row");
                    let names = fields.keys().cloned().collect::<Vec<_>>().join(",");
                    let placeholders = vec!["?"; fields.len()].join(",");
                    let values = fields
                        .values()
                        .map(|v| match v {
                            Value::Null => SqlValue::Null,
                            Value::String(s) => SqlValue::Text(s.clone()),
                            Value::Number(n) if n.is_i64() => {
                                SqlValue::Integer(n.as_i64().expect("integer"))
                            }
                            Value::Number(n) => SqlValue::Real(n.as_f64().expect("number")),
                            _ => panic!("SQL value"),
                        })
                        .collect::<Vec<_>>();
                    db.execute(
                        &format!("INSERT INTO {table_name}({names}) VALUES({placeholders})"),
                        rusqlite::params_from_iter(values),
                    )?;
                }
            }
            Ok(())
        })
        .await
        .expect("Python records seed");
}
fn state(store: &RuntimeStore, tables: &Value) -> Value {
    let db = store.test_reader().expect("read");
    let mut result = serde_json::Map::new();
    for table in tables.as_object().expect("tables").keys() {
        let order = if table == "chat_messages" {
            "seq"
        } else if table == "sync_entities" {
            "collection,id"
        } else {
            "id"
        };
        let table_name = match table.as_str() {
            "sync_entities" => "sync_entities".to_owned(),
            "sidebar_order" => "runtime_sidebar_order".to_owned(),
            _ => format!("runtime_{table}"),
        };
        let mut query = db
            .prepare(&format!(
                "{} ORDER BY {order}",
                studio_runtime_store::agent_columns::select_declared_sql(&db, &table_name)
                    .expect("columns")
            ))
            .expect("table");
        let columns = query
            .column_names()
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        let rows = query
            .query_map([], |row| {
                let mut value = serde_json::Map::new();
                for (index, column) in columns.iter().enumerate() {
                    let v: SqlValue = row.get(index)?;
                    value.insert(
                        column.clone(),
                        match v {
                            SqlValue::Null => Value::Null,
                            SqlValue::Text(s) => json!(s),
                            SqlValue::Integer(n) => json!(n),
                            SqlValue::Real(n) => json!(n),
                            _ => panic!("blob"),
                        },
                    );
                }
                Ok(Value::Object(value))
            })
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("rows read");
        result.insert(table.clone(), json!(rows));
    }
    Value::Object(result)
}
fn normalize(value: &mut Value) {
    match value {
        Value::Object(map) => {
            for (key, value) in map.iter_mut() {
                if key == "sync_entities"
                    && let Some(rows) = value.as_array_mut()
                {
                    // Keep every collection written by the scenario. The Python
                    // oracle builds its own projection rows for agent/room/event.
                    rows.sort_by_key(|row| {
                        (
                            row["collection"].as_str().unwrap_or_default().to_owned(),
                            row["id"].as_str().unwrap_or_default().to_owned(),
                        )
                    });
                    for (index, row) in rows.iter_mut().enumerate() {
                        row["seq"] = json!(index + 1);
                        // Compare the decoded canonical DTO payload. Hashes are
                        // computed from each runtime's JSON byte encoding.
                        row.as_object_mut().expect("sync row object").remove("hash");
                    }
                }
                if ["record", "result", "payload"].contains(&key.as_str())
                    && let Some(raw) = value.as_str()
                    && let Ok(parsed) = serde_json::from_str(raw)
                {
                    *value = parsed;
                }
                if ["created", "updated", "at"].contains(&key.as_str()) && value.is_number() {
                    *value = json!(0);
                }
                // JSON object key order is immaterial inside compiler-produced JSON sections.
                if key == "text"
                    && let Some(text) = value.as_str()
                    && text.contains("Shared room (JSON):\n")
                {
                    let (prefix, tail) = text.split_once("Shared room (JSON):\n").expect("context");
                    let (context, messages) = tail
                        .split_once("\nShared messages since your last successful reply (JSON):\n")
                        .expect("messages");
                    let context: Value = serde_json::from_str(context).expect("context JSON");
                    let mut messages: Value =
                        serde_json::from_str(messages).expect("messages JSON");
                    normalize(&mut messages);
                    *value = json!({"prefix":prefix,"context":context,"messages":messages});
                }
                normalize(value);
            }
            if map.remove("interruptState").is_some() {
                // Python stores the handoff intent as interruptRequested; Rust
                // retains the durable pending/unknown delivery state separately.
                map.insert("interruptRequested".to_owned(), json!(true));
            }
        }
        Value::Array(values) => {
            for value in values {
                normalize(value)
            }
        }
        _ => {}
    }
}
async fn run(store: std::sync::Arc<RuntimeStore>, case: &Value) -> Result<Value, ProjectsError> {
    let prepared = PreparedHost::load(&store).await?;
    let case = case.clone();
    let mode = case["mode"].as_str().expect("mode").to_owned();
    match mode.as_str() {
        "convert" => studio_runtime_services::peer_conversion::convert(std::sync::Arc::clone(&store), &case["body"]).await,
        "project" => studio_runtime_services::projects::write(
            std::sync::Arc::clone(&store),
            case["body"].clone(),
            "default".to_owned(),
            vec![json!({"id":"default","status":"ready","canAttemptNativeProof":true})],
        ).await,
        "peer" => studio_runtime_services::projects::peer_team_write(
            std::sync::Arc::clone(&store),
            case["body"].clone(),
            "default".to_owned(),
            vec![json!({"id":"default","status":"ready","canAttemptNativeProof":true})],
        ).await,
        "create" => {
            // Recreate the oracle project below the corpus owner for this test.
            {
                let path = case["body"]["path"].as_str().expect("project path");
                fs::create_dir_all(path).expect("project directory");
                let values: [Value; 2] = serde_json::from_value(case["drafts"].clone()).expect("Python drafts");
                let drafts = values.map(|value| AgentRecord::from_value(value).expect("checked Python agent draft"));
                radio_runtime::create_with(std::sync::Arc::clone(&store), &case["body"], Box::new(Creation(drafts))).await
            }
        },
        "project-binding" => store.transaction_with(move |db| {
            let path=case["body"]["path"].as_str().expect("project path");
            let binding=prepared.chat_binding(db,path,Some(path),Some("local-test"))?;
            Ok(json!({"binding":binding,"settings":prepared.settings_key(db,path,binding.as_ref())?}))
        }).await,
        "tick" => store.transaction_with(move |db| {
            let effects = radio_runtime::tick(db, &prepared, &mut Ingress, 1000.0)?;
            Ok(json!(
                effects.into_iter().map(|e| e.turn_id).collect::<Vec<_>>()
            ))
        }).await,
        "answer" => store.transaction_with(move |db| {
            radio_runtime::route_question_answer(
                db,
                &prepared,
                &case["body"]["question"],
                case["body"]["text"].as_str().unwrap_or("answer"),
                None,
                1000.0,
            )
            .map(|v| json!(v))
        }).await,
        "bind" => store.transaction_with(move |db| {
            radio_runtime::bind_reserved(
                db,
                &prepared,
                "a",
                case["body"]["event"].as_str().expect("event"),
                1000.0,
            )?;
            Ok(Value::Null)
        }).await,
        _ => radio_runtime::manage(std::sync::Arc::clone(&store), &case["body"]).await,
    }
}
#[tokio::test]
async fn python_written_radio_corpus_and_restart_replay() {
    let corpus = corpus();
    for original_case in &corpus.cases {
        let root = python_support::disposable_state_dir();
        copy_template(&corpus, root.path());
        let store = std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
        seed(&store, &original_case["before"]).await;
        let result = run(std::sync::Arc::clone(&store), original_case).await;
        assert_eq!(
            result.is_err(),
            original_case["error"] == true,
            "{}: {result:?}",
            original_case["name"]
        );
        if original_case["error"] == true {
            continue;
        }
        let mut actual = state(&store, &original_case["before"]);
        let mut expected = original_case["after"].clone();
        normalize(&mut actual);
        normalize(&mut expected);
        assert_eq!(actual, expected, "{}", original_case["name"]);
        let mut actual = result.expect("success");
        let mut expected = original_case["result"].clone();
        normalize(&mut actual);
        normalize(&mut expected);
        assert_eq!(actual, expected, "{} result", original_case["name"]);
        drop(store);
        let restarted = std::sync::Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("restart"),
        );
        if !["tick", "bind", "project-binding"]
            .contains(&original_case["mode"].as_str().expect("mode"))
        {
            let before = state(&restarted, &original_case["before"]);
            run(std::sync::Arc::clone(&restarted), original_case)
                .await
                .expect("replay");
            assert_eq!(
                state(&restarted, &original_case["before"]),
                before,
                "{} replay",
                original_case["name"]
            );
            let mut changed = original_case.clone();
            if changed["mode"] == "convert" {
                changed["body"]["target"] = json!("b");
            } else if changed["mode"] == "project" {
                match changed["body"]["action"].as_str().expect("project action") {
                    "set_accounts" => changed["body"]["account_keys"] = json!(["other"]),
                    "set_worker_base" => changed["body"]["base_ref"] = json!("other"),
                    "add_folder" => changed["body"]["name"] = json!("Other"),
                    "reorder" => changed["body"]["groups"] = json!({"projects": []}),
                    _ => unreachable!("covered project operation"),
                }
            } else if changed["mode"] == "peer" {
                match changed["body"]["action"].as_str().expect("peer action") {
                    "save" => changed["body"]["name"] = json!("Other"),
                    "delete" => changed["body"]["team_id"] = json!("different-team"),
                    "move" => changed["body"]["member"] = json!("d"),
                    _ => unreachable!("covered peer operation"),
                }
            } else {
                changed["body"]["text"] = json!("different answer or message");
            }
            let all_before = database_rows(&restarted);
            assert!(
                run(std::sync::Arc::clone(&restarted), &changed)
                    .await
                    .is_err(),
                "changed identity must fail: {}",
                original_case["name"]
            );
            assert_eq!(
                database_rows(&restarted),
                all_before,
                "changed retry writes nothing"
            );
        }
    }
}

#[tokio::test]
async fn failpoints_rollback_floor_binding_answer_and_receipt_then_restart() {
    let corpus = corpus();
    for (name, tables) in [
        (
            "create",
            vec![
                "runtime_agents",
                "runtime_rooms",
                "sync_entities",
                "runtime_operation_receipts",
            ],
        ),
        (
            "convert",
            vec![
                "runtime_projects",
                "runtime_agents",
                "sync_entities",
                "runtime_operation_receipts",
            ],
        ),
        (
            "stop-pending",
            vec![
                "runtime_events",
                "runtime_rooms",
                "sync_entities",
                "runtime_operation_receipts",
            ],
        ),
        ("interrupt", vec!["runtime_rooms", "sync_entities"]),
        (
            "floor",
            vec!["runtime_events", "runtime_rooms", "sync_entities"],
        ),
        ("binding", vec!["runtime_rooms", "sync_entities"]),
        (
            "answer",
            vec!["runtime_chat_messages", "runtime_rooms", "sync_entities"],
        ),
        (
            "send",
            vec![
                "runtime_chat_messages",
                "runtime_rooms",
                "sync_entities",
                "runtime_operation_receipts",
            ],
        ),
    ] {
        let original_case = corpus
            .cases
            .iter()
            .find(|c| c["name"] == name)
            .expect("case");
        for table in tables {
            for boundary in ["BEFORE", "AFTER"] {
                let root = python_support::disposable_state_dir();
                copy_template(&corpus, root.path());
                let store =
                    std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
                seed(&store, &original_case["before"]).await;
                let before = state(&store, &original_case["before"]);
                let all_before = database_rows(&store);
                let table_for_trigger = table.to_string();
                let boundary_for_trigger = boundary.to_string();
                store.transaction(move |db| {db.execute_batch(&format!("CREATE TRIGGER p19_fail_insert {boundary_for_trigger} INSERT ON {table_for_trigger} BEGIN SELECT RAISE(ABORT,'P19 failpoint'); END; CREATE TRIGGER p19_fail_update {boundary_for_trigger} UPDATE ON {table_for_trigger} BEGIN SELECT RAISE(ABORT,'P19 failpoint'); END;"))?;Ok(())}).await.expect("failpoint");
                assert!(
                    run(std::sync::Arc::clone(&store), original_case)
                        .await
                        .is_err(),
                    "{name} {boundary} {table} reached"
                );
                assert_eq!(
                    state(&store, &original_case["before"]),
                    before,
                    "{name} {boundary} {table} rollback"
                );
                assert_eq!(database_rows(&store), all_before, "all tables rollback");
                drop(store);
                let store = std::sync::Arc::new(
                    RuntimeStore::open(Some(root.path()))
                        .await
                        .expect("restart"),
                );
                assert_eq!(
                    state(&store, &original_case["before"]),
                    before,
                    "restart before retry"
                );
                store
                    .transaction(|db| {
                        db.execute_batch(
                            "DROP TRIGGER p19_fail_insert; DROP TRIGGER p19_fail_update",
                        )?;
                        Ok(())
                    })
                    .await
                    .expect("remove failpoint");
                run(std::sync::Arc::clone(&store), original_case)
                    .await
                    .expect("retry after rollback");
                let committed = state(&store, &original_case["before"]);
                drop(store);
                let store = std::sync::Arc::new(
                    RuntimeStore::open(Some(root.path()))
                        .await
                        .expect("restart after commit"),
                );
                run(std::sync::Arc::clone(&store), original_case)
                    .await
                    .expect("retry after lost response");
                assert_eq!(
                    state(&store, &original_case["before"]),
                    committed,
                    "{name} no duplicate after commit"
                );
            }
        }
    }
}

fn database_rows(store: &RuntimeStore) -> Vec<(String, Vec<Vec<SqlValue>>)> {
    let db = store.test_reader().expect("database snapshot");
    let tables = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .expect("tables")
        .query_map([], |r| r.get::<_, String>(0))
        .expect("table names")
        .collect::<Result<Vec<_>, _>>()
        .expect("table list");
    tables
        .into_iter()
        .map(|name| {
            let mut query = db
                .prepare(
                    &studio_runtime_store::agent_columns::select_declared_sql(&db, &name)
                        .expect("columns"),
                )
                .expect("snapshot table");
            let count = query.column_count();
            let rows = query
                .query_map([], |r| {
                    (0..count)
                        .map(|i| r.get::<_, SqlValue>(i))
                        .collect::<Result<Vec<_>, _>>()
                })
                .expect("snapshot rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("snapshot values");
            (name, rows)
        })
        .collect()
}

#[tokio::test]
async fn answered_question_releases_one_continuation_and_malformed_rows_write_nothing() {
    let corpus = corpus();
    let case = corpus
        .cases
        .iter()
        .find(|c| c["name"] == "answer")
        .expect("answer");
    let root = python_support::disposable_state_dir();
    copy_template(&corpus, root.path());
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(&store, &case["before"]).await;
    assert!(radio_runtime::holds_floor(&store.test_reader().expect("read"), "a").expect("floor"));
    let prepared = std::sync::Arc::new(PreparedHost::load(&store).await.expect("prepare"));
    let question = case["body"]["question"].clone();
    store.transaction_with(move |db|->Result<(),ProjectsError> {
        assert!(radio_runtime::route_question_answer(db,&prepared,&question,"answer",None,1000.0)?);
        db.execute("UPDATE runtime_requests SET record=json_set(record,'$.status','answered') WHERE id='q'",[])?;
        Ok(())
    }).await.expect("P04 caller commits both writes");
    drop(store);
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart"),
    );
    assert!(
        !radio_runtime::holds_floor(&store.test_reader().expect("read"), "a").expect("released")
    );
    let prepared = std::sync::Arc::new(PreparedHost::load(&store).await.expect("prepare"));
    let prepared_for_tick = std::sync::Arc::clone(&prepared);
    store
        .transaction_with(move |db| {
            radio_runtime::tick(db, &prepared_for_tick, &mut Ingress, 1000.0)
        })
        .await
        .expect("next input");
    let committed = database_rows(&store);
    let prepared = PreparedHost::load(&store).await.expect("prepare");
    store
        .transaction_with(move |db| radio_runtime::tick(db, &prepared, &mut Ingress, 1000.0))
        .await
        .expect("retry tick");
    assert_eq!(database_rows(&store), committed, "one continuation only");
    store
        .transaction(|db| {
            db.execute(
                "UPDATE runtime_rooms SET record=json_set(record,'$.radio.revision','invalid')",
                [],
            )?;
            Ok(())
        })
        .await
        .expect("malformed Python-shaped input");
    let before = database_rows(&store);
    let error = run(std::sync::Arc::clone(&store), case)
        .await
        .expect_err("malformed room refused");
    assert!(
        matches!(
            error,
            ProjectsError::InvalidStored { .. } | ProjectsError::Store(_)
        ),
        "{error}"
    );
    assert_eq!(
        database_rows(&store),
        before,
        "located error writes nothing"
    );
}

#[tokio::test]
async fn pending_interrupt_recovers_before_claim_and_unknown_is_never_resent() {
    let corpus = corpus();
    let case = corpus
        .cases
        .iter()
        .find(|case| case["name"] == "interrupt")
        .expect("interrupt");
    let root = python_support::disposable_state_dir();
    copy_template(&corpus, root.path());
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(&store, &case["before"]).await;
    let prepared = std::sync::Arc::new(PreparedHost::load(&store).await.expect("prepare"));
    let prepared_for_tick = std::sync::Arc::clone(&prepared);
    let first = store
        .transaction_with(move |db| {
            radio_runtime::tick(db, &prepared_for_tick, &mut Ingress, 1000.0)
        })
        .await
        .expect("persist pending intent");
    assert_eq!(first.len(), 1);
    let event = first[0]
        .request_id
        .strip_prefix("radio-interrupt:")
        .expect("event ID")
        .to_owned();
    drop(store);

    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart with pending"),
    );
    let prepared = std::sync::Arc::new(PreparedHost::load(&store).await.expect("prepare"));
    let prepared_for_tick = std::sync::Arc::clone(&prepared);
    let recovered = store
        .transaction_with(move |db| {
            radio_runtime::tick(db, &prepared_for_tick, &mut Ingress, 1001.0)
        })
        .await
        .expect("recover intent");
    assert_eq!(recovered.len(), 1);
    assert_eq!(recovered[0].request_id, format!("radio-interrupt:{event}"));
    let mut transport = TransportDouble::accepted();
    assert!(
        radio_runtime::handoff_interrupt(
            &store,
            std::sync::Arc::clone(&prepared),
            &recovered[0],
            &mut transport,
            1002.0
        )
        .await
        .expect("handoff")
    );
    assert_eq!(transport.calls, vec![format!("radio-interrupt:{event}")]);
    drop(store);

    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart after claim"),
    );
    let prepared = PreparedHost::load(&store).await.expect("prepare");
    let held = store
        .transaction_with(move |db| radio_runtime::tick(db, &prepared, &mut Ingress, 1003.0))
        .await
        .expect("hold unknown handoff");
    assert!(held.is_empty(), "claimed Unknown interrupt is never resent");

    // Simulate a crash after the durable claim commits but before transport
    // dispatch: recovery observes Unknown and the transport receives nothing.
    let crash_root = python_support::disposable_state_dir();
    copy_template(&corpus, crash_root.path());
    let crash_store = std::sync::Arc::new(
        RuntimeStore::open(Some(crash_root.path()))
            .await
            .expect("open crash state"),
    );
    seed(&crash_store, &case["before"]).await;
    let crash_prepared =
        std::sync::Arc::new(PreparedHost::load(&crash_store).await.expect("prepare"));
    let prepared_for_tick = std::sync::Arc::clone(&crash_prepared);
    let pending = crash_store
        .transaction_with(move |db| {
            radio_runtime::tick(db, &prepared_for_tick, &mut Ingress, 2000.0)
        })
        .await
        .expect("pending");
    let crash_event = pending[0]
        .request_id
        .strip_prefix("radio-interrupt:")
        .expect("interrupt request ID")
        .to_owned();
    assert!(
        crash_store
            .transaction_with({
                let crash_prepared = std::sync::Arc::clone(&crash_prepared);
                move |db| radio_runtime::claim_interrupt(db, &crash_prepared, &crash_event, 2001.0)
            })
            .await
            .expect("claim before simulated crash")
    );
    drop(crash_store);
    let crash_store = std::sync::Arc::new(
        RuntimeStore::open(Some(crash_root.path()))
            .await
            .expect("restart after claim crash"),
    );
    let crash_prepared =
        std::sync::Arc::new(PreparedHost::load(&crash_store).await.expect("prepare"));
    let mut transport = TransportDouble::accepted();
    assert!(
        !radio_runtime::handoff_interrupt(
            &crash_store,
            std::sync::Arc::clone(&crash_prepared),
            &pending[0],
            &mut transport,
            2002.0
        )
        .await
        .expect("held")
    );
    assert!(transport.calls.is_empty());
}

struct TransportDouble {
    calls: Vec<String>,
    outcome: InterruptOutcome,
}
impl TransportDouble {
    fn accepted() -> Self {
        Self {
            calls: vec![],
            outcome: InterruptOutcome::Accepted,
        }
    }
}
impl InterruptTransport for TransportDouble {
    fn dispatch(&mut self, interrupt: &Interrupt) -> Result<InterruptReceipt, ProjectsError> {
        self.calls.push(interrupt.request_id.clone());
        Ok(InterruptReceipt::new(
            interrupt.request_id.clone(),
            self.outcome,
        ))
    }
}

struct Creation([AgentRecord; 2]);
impl radio_runtime::RadioCreation for Creation {
    fn prepare(
        &mut self,
        _body: &Value,
        _path: &Path,
        _ids: [AgentId; 2],
    ) -> Result<[AgentRecord; 2], ProjectsError> {
        Ok(self.0.clone())
    }
    fn recheck(
        &mut self,
        _db: &rusqlite::Transaction<'_>,
        _drafts: &[AgentRecord; 2],
    ) -> Result<(), ProjectsError> {
        Ok(())
    }
}
