#[path = "support/prepared_python.rs"]
mod python_support;
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};
use std::{fs, path::Path, process::Command};
use studio_runtime_services::{
    host_project_runtime::PreparedHost,
    projects::ProjectsError,
    radio_runtime::{self, RadioIngress},
};
use studio_runtime_store::{AgentId, QueueEvent, QueueEventId, QueueEventRepository, RuntimeStore};

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
fn corpus(root: &Path) -> Vec<Value> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"));
    let mut command = Command::new(python_support::prepared_python(
        &manifest.join("../../apps/server/src"),
    ));
    command
        .arg(manifest.join("tests/wave7_p19/oracle.py"))
        .arg(root);
    if std::env::var("STUDIO_RUNTIME_CORPUS_FULL").as_deref() == Ok("1") {
        command.arg("--full");
    }
    let output = command.output().expect("oracle starts");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("corpus JSON")
}
fn seed(store: &RuntimeStore, snapshot: &Value) {
    let identity = store.root().join("multi-server");
    fs::create_dir_all(&identity).expect("identity directory");
    fs::write(
        identity.join("identity.json"),
        r#"{"serverId":"local-test","label":"fixture","publicKey":"fixture"}"#,
    )
    .expect("identity");
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
        .transaction(|db| {
            for (table, rows) in snapshot.as_object().expect("snapshot") {
                db.execute(&format!("DELETE FROM runtime_{table}"), [])?;
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
                        &format!("INSERT INTO runtime_{table}({names}) VALUES({placeholders})"),
                        rusqlite::params_from_iter(values),
                    )?;
                }
            }
            Ok(())
        })
        .expect("Python records seed");
}
fn state(store: &RuntimeStore, tables: &Value) -> Value {
    let db = store.read().expect("read");
    let mut result = serde_json::Map::new();
    for table in tables.as_object().expect("tables").keys() {
        let order = if table == "chat_messages" {
            "seq"
        } else {
            "id"
        };
        let mut query = db
            .prepare(&format!("SELECT * FROM runtime_{table} ORDER BY {order}"))
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
                if ["record", "result"].contains(&key.as_str())
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
        }
        Value::Array(values) => {
            for value in values {
                normalize(value)
            }
        }
        _ => {}
    }
}
fn run(store: &RuntimeStore, case: &Value) -> Result<Value, ProjectsError> {
    let prepared = PreparedHost::load(store)?;
    match case["mode"].as_str().expect("mode") {
        "convert" => studio_runtime_services::peer_conversion::convert(store, &case["body"]),
        "create" => {
            let path = case["body"]["path"].as_str().expect("project path");
            fs::create_dir_all(path).expect("project directory");
            let drafts: [Value; 2] = serde_json::from_value(case["drafts"].clone()).expect("Python drafts");
            let result = radio_runtime::create_with(store, &case["body"], &mut Creation(drafts));
            fs::remove_dir(path).expect("remove fixture project directory");
            result
        },
        "project-binding" => store.transaction_with(|db| {
            let path=case["body"]["path"].as_str().expect("project path");
            let binding=prepared.chat_binding(db,path,Some(path),Some("local-test"))?;
            Ok(json!({"binding":binding,"settings":prepared.settings_key(db,path,binding.as_ref())?}))
        }),
        "tick" => store.transaction_with(|db| {
            let effects = radio_runtime::tick(db, &prepared, &mut Ingress, 1000.0)?;
            Ok(json!(
                effects.into_iter().map(|e| e.turn_id).collect::<Vec<_>>()
            ))
        }),
        "answer" => store.transaction_with(|db| {
            radio_runtime::route_question_answer(
                db,
                &prepared,
                &case["body"]["question"],
                case["body"]["text"].as_str().unwrap_or("answer"),
                false,
                1000.0,
            )
            .map(|v| json!(v))
        }),
        "bind" => store.transaction_with(|db| {
            radio_runtime::bind_reserved(
                db,
                &prepared,
                "a",
                case["body"]["event"].as_str().expect("event"),
                1000.0,
            )?;
            Ok(Value::Null)
        }),
        _ => radio_runtime::manage(store, &case["body"]),
    }
}
#[test]
fn python_written_radio_corpus_and_restart_replay() {
    let template = tempfile::tempdir().expect("template");
    let cases = corpus(template.path());
    for case in &cases {
        let root = tempfile::tempdir().expect("case");
        for db in ["canvas.sqlite3", "analytics.sqlite3"] {
            fs::copy(template.path().join(db), root.path().join(db)).expect("copy");
        }
        let store = RuntimeStore::open(Some(root.path())).expect("open");
        seed(&store, &case["before"]);
        let result = run(&store, case);
        assert_eq!(
            result.is_err(),
            case["error"] == true,
            "{}: {result:?}",
            case["name"]
        );
        if case["error"] == true {
            continue;
        }
        let mut actual = state(&store, &case["before"]);
        let mut expected = case["after"].clone();
        normalize(&mut actual);
        normalize(&mut expected);
        assert_eq!(actual, expected, "{}", case["name"]);
        let mut actual = result.expect("success");
        let mut expected = case["result"].clone();
        normalize(&mut actual);
        normalize(&mut expected);
        assert_eq!(actual, expected, "{} result", case["name"]);
        drop(store);
        let restarted = RuntimeStore::open(Some(root.path())).expect("restart");
        if !["tick", "bind", "project-binding"].contains(&case["mode"].as_str().expect("mode")) {
            let before = state(&restarted, &case["before"]);
            run(&restarted, case).expect("replay");
            assert_eq!(
                state(&restarted, &case["before"]),
                before,
                "{} replay",
                case["name"]
            );
            let mut changed = case.clone();
            if changed["mode"] == "convert" {
                changed["body"]["target"] = json!("b");
            } else {
                changed["body"]["text"] = json!("different answer or message");
            }
            let all_before = database_rows(&restarted);
            assert!(
                run(&restarted, &changed).is_err(),
                "changed identity must fail: {}",
                case["name"]
            );
            assert_eq!(
                database_rows(&restarted),
                all_before,
                "changed retry writes nothing"
            );
        }
    }
}

#[test]
fn failpoints_rollback_floor_binding_answer_and_receipt_then_restart() {
    let template = tempfile::tempdir().expect("template");
    let cases = corpus(template.path());
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
        let case = cases.iter().find(|c| c["name"] == name).expect("case");
        for table in tables {
            for boundary in ["BEFORE", "AFTER"] {
                let root = tempfile::tempdir().expect("case");
                for db in ["canvas.sqlite3", "analytics.sqlite3"] {
                    fs::copy(template.path().join(db), root.path().join(db)).expect("copy");
                }
                let store = RuntimeStore::open(Some(root.path())).expect("open");
                seed(&store, &case["before"]);
                let before = state(&store, &case["before"]);
                let all_before = database_rows(&store);
                store.transaction(|db| {db.execute_batch(&format!("CREATE TRIGGER p19_fail_insert {boundary} INSERT ON {table} BEGIN SELECT RAISE(ABORT,'P19 failpoint'); END; CREATE TRIGGER p19_fail_update {boundary} UPDATE ON {table} BEGIN SELECT RAISE(ABORT,'P19 failpoint'); END;"))?;Ok(())}).expect("failpoint");
                assert!(
                    run(&store, case).is_err(),
                    "{name} {boundary} {table} reached"
                );
                assert_eq!(
                    state(&store, &case["before"]),
                    before,
                    "{name} {boundary} {table} rollback"
                );
                assert_eq!(database_rows(&store), all_before, "all tables rollback");
                drop(store);
                let store = RuntimeStore::open(Some(root.path())).expect("restart");
                assert_eq!(
                    state(&store, &case["before"]),
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
                    .expect("remove failpoint");
                run(&store, case).expect("retry after rollback");
                let committed = state(&store, &case["before"]);
                drop(store);
                let store = RuntimeStore::open(Some(root.path())).expect("restart after commit");
                run(&store, case).expect("retry after lost response");
                assert_eq!(
                    state(&store, &case["before"]),
                    committed,
                    "{name} no duplicate after commit"
                );
            }
        }
    }
}

fn database_rows(store: &RuntimeStore) -> Vec<(String, Vec<Vec<SqlValue>>)> {
    let db = store.read().expect("database snapshot");
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
                .prepare(&format!("SELECT * FROM \"{name}\""))
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

#[test]
fn answered_question_releases_one_continuation_and_malformed_rows_write_nothing() {
    let template = tempfile::tempdir().expect("template");
    let cases = corpus(template.path());
    let case = cases
        .iter()
        .find(|c| c["name"] == "answer")
        .expect("answer");
    let root = tempfile::tempdir().expect("state");
    for db in ["canvas.sqlite3", "analytics.sqlite3"] {
        fs::copy(template.path().join(db), root.path().join(db)).expect("copy");
    }
    let store = RuntimeStore::open(Some(root.path())).expect("open");
    seed(&store, &case["before"]);
    assert!(radio_runtime::holds_floor(&store.read().expect("read"), "a").expect("floor"));
    let prepared = PreparedHost::load(&store).expect("prepare");
    store.transaction_with(|db|->Result<(),ProjectsError> {
        assert!(radio_runtime::route_question_answer(db,&prepared,&case["body"]["question"],"answer",false,1000.0)?);
        db.execute("UPDATE runtime_requests SET record=json_set(record,'$.status','answered') WHERE id='q'",[])?;
        Ok(())
    }).expect("P04 caller commits both writes");
    drop(store);
    let store = RuntimeStore::open(Some(root.path())).expect("restart");
    assert!(!radio_runtime::holds_floor(&store.read().expect("read"), "a").expect("released"));
    let prepared = PreparedHost::load(&store).expect("prepare");
    store
        .transaction_with(|db| radio_runtime::tick(db, &prepared, &mut Ingress, 1000.0))
        .expect("next input");
    let committed = database_rows(&store);
    let prepared = PreparedHost::load(&store).expect("prepare");
    store
        .transaction_with(|db| radio_runtime::tick(db, &prepared, &mut Ingress, 1000.0))
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
        .expect("malformed Python-shaped input");
    let before = database_rows(&store);
    let error = run(&store, case).expect_err("malformed room refused");
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

struct Creation([Value; 2]);
impl radio_runtime::RadioCreation for Creation {
    fn prepare(
        &mut self,
        _body: &Value,
        _path: &str,
        _ids: &[String; 2],
    ) -> Result<[Value; 2], ProjectsError> {
        Ok(self.0.clone())
    }
    fn recheck(
        &mut self,
        _db: &rusqlite::Transaction<'_>,
        _drafts: &[Value; 2],
    ) -> Result<(), ProjectsError> {
        Ok(())
    }
}
