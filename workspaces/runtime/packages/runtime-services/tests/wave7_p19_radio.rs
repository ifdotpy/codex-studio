use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};
use std::{fs, path::Path};
use studio_runtime_services::{
    host_project_runtime::PreparedHost,
    projects::ProjectsError,
    radio_runtime::{
        self, Interrupt, InterruptOutcome, InterruptReceipt, InterruptTransport, RadioIngress,
    },
};
use studio_runtime_store::path_identity::CanonicalPath;
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
    databases: Vec<(String, Vec<u8>)>,
    cases: Vec<Value>,
    /// Holds the project folder that the `create` case makes.
    _projects: tempfile::TempDir,
}
fn corpus() -> RadioCorpus {
    let bytes =
        crate::media_auth_fixtures::state_bytes(include_str!("wave7_p19/initial-state.json"));
    let expected = if std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() == Ok("1") {
        include_str!("wave7_p19/expected-full.json")
    } else {
        include_str!("wave7_p19/expected.json")
    };
    let projects = tempfile::tempdir().expect("radio project root");
    let mut cases: Vec<Value> = serde_json::from_str(expected).expect("frozen radio corpus");
    for case in &mut cases {
        relocate_project(case, projects.path());
    }
    RadioCorpus {
        databases: ["canvas.sqlite3", "analytics.sqlite3"]
            .into_iter()
            .zip(bytes)
            .map(|(name, bytes)| (name.to_owned(), bytes))
            .collect(),
        cases,
        _projects: projects,
    }
}
/// The Python oracle ran on Linux, where its temporary project path is
/// canonical. Project keys resolve links (macOS links its system temporary
/// folders below `/private`), so the stored rows, drafts and results of a case
/// use this host's key for that path. The request body keeps the path as the
/// client sent it.
///
/// The `create` case makes its project folder, so its path moves below
/// `projects`. Its request receipt signs the body: the frozen signature must
/// match the frozen body, and the expected receipt then signs the moved body.
fn relocate_project(case: &mut Value, projects: &Path) {
    let frozen = case["body"]["path"]
        .as_str()
        .expect("case project path")
        .to_owned();
    let key = if case["mode"] == "create" {
        let frozen_path = Path::new(&frozen);
        let oracle_folder = frozen_path
            .ancestors()
            .nth(2)
            .expect("frozen project below its oracle folder");
        let moved = projects.join(
            frozen_path
                .strip_prefix(oracle_folder)
                .expect("frozen project tail"),
        );
        let signature = |body: &Value| {
            studio_runtime_services::event_write::request_signature(
                &json!({"operation":"radio","body":body}),
            )
        };
        let frozen_signature = signature(&case["body"]);
        assert!(
            case["after"].to_string().contains(&frozen_signature),
            "the frozen create receipt signs the frozen body"
        );
        case["body"]["path"] = json!(moved);
        let moved_signature = signature(&case["body"]);
        replace_text(&mut case["after"], &frozen_signature, &moved_signature);
        host_key(&moved.to_string_lossy())
    } else {
        host_key(&frozen)
    };
    if key == frozen {
        return;
    }
    for part in ["before", "after", "result", "drafts"] {
        if let Some(value) = case.get_mut(part) {
            replace_text(value, &frozen, &key);
        }
    }
}
/// This host's project key for a client path: links in its existing prefix
/// resolve, as in `projects::project_key`.
fn host_key(path: &str) -> String {
    CanonicalPath::ancestor_nonstrict(Path::new(path))
        .expect("host project key")
        .to_string_lossy()
        .into_owned()
}
fn replace_text(value: &mut Value, from: &str, to: &str) {
    match value {
        Value::String(text) => {
            if text.contains(from) {
                *text = text.replace(from, to);
            }
        }
        Value::Array(values) => values
            .iter_mut()
            .for_each(|value| replace_text(value, from, to)),
        Value::Object(map) => map
            .values_mut()
            .for_each(|value| replace_text(value, from, to)),
        _ => {}
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
    super::turn_fixtures::assert_agent_view_projections(&db);
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
                    // View projections are checked against their OpenAPI owners above.
                    rows.retain(|row| {
                        !super::turn_fixtures::is_agent_view_collection(&row["collection"])
                    });
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
    // Binding callers resolve the client path to a project key before SQL.
    let project_key = host_key(case["body"]["path"].as_str().expect("project path"));
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
            // Recreate the oracle project below the corpus's temporary root.
            {
                let path = case["body"]["path"].as_str().expect("project path");
                fs::create_dir_all(path).expect("project directory");
                let values: [Value; 2] = serde_json::from_value(case["drafts"].clone()).expect("Python drafts");
                let drafts = values.map(|value| AgentRecord::from_value(value).expect("checked Python agent draft"));
                radio_runtime::create_with(std::sync::Arc::clone(&store), &case["body"], Box::new(Creation(drafts))).await
            }
        },
        "project-binding" => store.transaction_with(move |db| {
            let path=project_key.as_str();
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
        let root = tempfile::tempdir().expect("radio state directory");
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

// AFTER failures include the attempted write. BEFORE failures exercise the same
// SQLite rollback contract with less work performed. Reuse the rolled-back store
// across tables, then cross each operation's restart boundary once.
async fn failpoints_rollback_then_restart(name: &str, tables: &[&str]) {
    let corpus = corpus();
    let case = corpus
        .cases
        .iter()
        .find(|case| case["name"] == name)
        .expect("case");
    let root = tempfile::tempdir().expect("radio state directory");
    copy_template(&corpus, root.path());
    let store = std::sync::Arc::new(RuntimeStore::open(Some(root.path())).await.expect("open"));
    seed(&store, &case["before"]).await;
    let before = database_rows(&store);
    for table in tables {
        let trigger_table = table.to_string();
        store.transaction(move |db| {
            db.execute_batch(&format!("CREATE TRIGGER p19_fail_insert AFTER INSERT ON {trigger_table} BEGIN SELECT RAISE(ABORT,'P19 failpoint'); END; CREATE TRIGGER p19_fail_update AFTER UPDATE ON {trigger_table} BEGIN SELECT RAISE(ABORT,'P19 failpoint'); END;"))?;
            Ok(())
        }).await.expect("failpoint");
        assert!(
            run(std::sync::Arc::clone(&store), case).await.is_err(),
            "{name} {table} reached"
        );
        assert_eq!(
            database_rows(&store),
            before,
            "{name} {table}: all tables roll back"
        );
        store
            .transaction(|db| {
                db.execute_batch("DROP TRIGGER p19_fail_insert; DROP TRIGGER p19_fail_update")?;
                Ok(())
            })
            .await
            .expect("remove failpoint");
    }
    drop(store);
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart after rollback"),
    );
    assert_eq!(database_rows(&store), before, "rollback survives restart");
    run(std::sync::Arc::clone(&store), case)
        .await
        .expect("retry after rollback");
    let committed = database_rows(&store);
    drop(store);
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("restart after commit"),
    );
    run(std::sync::Arc::clone(&store), case)
        .await
        .expect("retry after lost response");
    assert_eq!(
        database_rows(&store),
        committed,
        "{name}: no duplicate after commit"
    );
}

macro_rules! radio_failpoints {
    ($($test:ident: $case:literal => [$($table:literal),+]),+ $(,)?) => {$ (
        #[tokio::test]
        async fn $test() {
            failpoints_rollback_then_restart($case, &[$($table),+]).await;
        }
    )+};
}
radio_failpoints! {
    create_rollback_and_restart: "create" => ["runtime_agents", "runtime_rooms", "sync_entities", "runtime_operation_receipts"],
    convert_rollback_and_restart: "convert" => ["runtime_projects", "runtime_agents", "sync_entities", "runtime_operation_receipts"],
    stop_rollback_and_restart: "stop-pending" => ["runtime_events", "runtime_rooms", "sync_entities", "runtime_operation_receipts"],
    interrupt_rollback_and_restart: "interrupt" => ["runtime_rooms", "sync_entities"],
    floor_rollback_and_restart: "floor" => ["runtime_events", "runtime_rooms", "sync_entities"],
    binding_rollback_and_restart: "binding" => ["runtime_rooms", "sync_entities"],
    answer_rollback_and_restart: "answer" => ["runtime_chat_messages", "runtime_rooms", "sync_entities"],
    send_rollback_and_restart: "send" => ["runtime_chat_messages", "runtime_rooms", "sync_entities", "runtime_operation_receipts"],
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
    let root = tempfile::tempdir().expect("radio state directory");
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
    let root = tempfile::tempdir().expect("radio state directory");
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
    let crash_root = tempfile::tempdir().expect("radio state directory");
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
