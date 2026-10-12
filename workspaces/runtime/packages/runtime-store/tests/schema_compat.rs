use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, params};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, ChatMessageId, MessageRepository, QueueEvent, QueueEventId,
    QueueEventRepository, RuntimeStore, StoreError, TaskId, TaskRepository,
};

const LIVE_SCHEMA: &str = include_str!("fixtures/live-state-schema.json");

#[test]
fn live_schema_opens_and_supported_repositories_preserve_the_catalog() {
    let state = temporary_state();
    create_from_live_schema(&state);
    seed_records(&state.join("canvas.sqlite3"));
    let before = catalog(&state.join("canvas.sqlite3"));
    let analytics_before = catalog(&state.join("analytics.sqlite3"));

    let store = RuntimeStore::open(Some(&state)).expect("schema-only live fixture opens");
    let reader = store.read().expect("reader opens");
    assert!(
        AgentRepository::get(&reader, &AgentId::from("fixture-agent"))
            .expect("agent reads")
            .is_some()
    );
    assert!(
        TaskRepository::get(&reader, &TaskId::from("fixture-task"))
            .expect("task reads")
            .is_some()
    );
    assert!(
        QueueEventRepository::get(&reader, &QueueEventId::from("fixture-event"))
            .expect("event reads")
            .is_none()
    );
    assert!(
        MessageRepository::get(&reader, &ChatMessageId::from("fixture-message"))
            .expect("message reads")
            .is_some()
    );
    drop(reader);

    store
        .transaction(|tx| {
            let mut agent = AgentRepository::get(tx, &AgentId::from("fixture-agent"))?
                .expect("seeded agent exists");
            agent.extra.insert("rustWrite".into(), json!(true));
            AgentRepository::put(tx, &agent)?;

            let mut task = TaskRepository::get(tx, &TaskId::from("fixture-task"))?
                .expect("seeded task exists");
            task.extra.insert("rustWrite".into(), json!(true));
            TaskRepository::put(tx, &task)?;

            QueueEventRepository::put(
                tx,
                &QueueEvent {
                    id: QueueEventId::from("fixture-event"),
                    agent: AgentId::from("fixture-agent"),
                    kind: "work_review".into(),
                    text: "{}".into(),
                    status: "running".into(),
                    created: 5.0,
                    epoch: 0,
                    turn_id: None,
                    error: None,
                },
            )?;

            let mut message = MessageRepository::get(tx, &ChatMessageId::from("fixture-message"))?
                .expect("seeded message exists");
            message.deliveries["rustWrite"] = json!(true);
            MessageRepository::put(tx, &message)?;
            Ok(())
        })
        .expect("all supported repositories write on the live schema");
    let reader = store.read().expect("reader opens after writes");
    assert_eq!(
        QueueEventRepository::get(&reader, &QueueEventId::from("fixture-event"))
            .expect("written event reads")
            .expect("event exists")
            .status,
        "running"
    );
    drop(reader);
    store
        .verify_schema()
        .expect("required schema remains valid");
    drop(store);

    assert_eq!(before, catalog(&state.join("canvas.sqlite3")));
    assert_eq!(analytics_before, catalog(&state.join("analytics.sqlite3")));
    let reopened = RuntimeStore::open(Some(&state)).expect("Python-shaped schema remains openable");
    let reader = reopened.read().expect("reader opens after Rust writes");
    assert_eq!(
        AgentRepository::get(&reader, &AgentId::from("fixture-agent"))
            .expect("agent reads")
            .expect("agent exists")
            .extra["rustWrite"],
        true
    );
    drop(reader);
    drop(reopened);
    cleanup(state);
}

#[test]
fn missing_required_objects_are_typed_and_name_the_object() {
    for (sql, expected) in [
        ("DROP TABLE runtime_tasks", "runtime_tasks"),
        (
            "DROP TABLE runtime_tasks; CREATE TABLE runtime_tasks(id TEXT PRIMARY KEY)",
            "runtime_tasks",
        ),
        (
            "DROP TRIGGER sync_entity_event_UPDATE",
            "sync_entity_event_UPDATE",
        ),
    ] {
        let state = temporary_state();
        create_from_live_schema(&state);
        Connection::open(state.join("canvas.sqlite3"))
            .expect("database opens")
            .execute_batch(sql)
            .expect("negative case mutates disposable schema");
        let error = match RuntimeStore::open(Some(&state)) {
            Ok(_) => panic!("incompatible required schema was accepted"),
            Err(error) => error,
        };
        assert!(
            matches!(error, StoreError::SchemaObjectMismatch { ref object, .. } if object == expected),
            "expected typed refusal naming {expected}, got {error:?}"
        );
        cleanup(state);
    }
}

fn create_from_live_schema(root: &Path) {
    let schema: Value = serde_json::from_str(LIVE_SCHEMA).expect("schema-only fixture parses");
    for database in ["canvas.sqlite3", "analytics.sqlite3"] {
        let connection = Connection::open(root.join(database)).expect("database file opens");
        connection
            .execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL;")
            .expect("WAL and FULL are enabled");
        let mut objects = schema[database]["sqlite_master"]
            .as_array()
            .expect("fixture contains SQLite objects")
            .iter()
            .filter_map(|object| {
                let name = object["name"].as_str()?;
                let sql = object["sql"].as_str()?;
                // FTS5 creates these shadow tables as part of CREATE VIRTUAL TABLE.
                if [
                    "runtime_search_next_config",
                    "runtime_search_next_data",
                    "runtime_search_next_docsize",
                    "runtime_search_next_idx",
                ]
                .contains(&name)
                {
                    return None;
                }
                Some((
                    object["type"].as_str().unwrap_or_default().to_owned(),
                    name.to_owned(),
                    sql.to_owned(),
                ))
            })
            .collect::<Vec<_>>();
        objects.sort_by(|left, right| {
            object_order(&left.0)
                .cmp(&object_order(&right.0))
                .then(left.1.cmp(&right.1))
        });
        for (_, _, sql) in objects {
            connection.execute_batch(&sql).unwrap_or_else(|error| {
                panic!("live fixture DDL failed for {database}: {error}; SQL={sql}")
            });
        }
        connection
            .pragma_update(None, "user_version", 0)
            .expect("version matches supplied state");
    }
}

fn object_order(kind: &str) -> u8 {
    match kind {
        "table" => 0,
        "index" => 1,
        "trigger" => 2,
        "view" => 3,
        _ => 4,
    }
}

fn seed_records(path: &Path) {
    let db = Connection::open(path).expect("canvas opens");
    db.execute(
        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
        params!["fixture-agent", r#"{"id":"fixture-agent","rootId":"fixture-agent","epoch":0,"status":"idle","futureField":{"x":true}}"#],
    )
    .expect("agent fixture inserts");
    db.execute(
        "INSERT INTO runtime_tasks(id,record) VALUES(?1,?2)",
        params!["fixture-task", r#"{"id":"fixture-task","rootId":"fixture-agent","status":"ready","title":"t","description":"","owner":null,"created":1.0,"updated":2.0,"createdBy":"fixture-agent","version":1,"dependencies":[],"decisions":[],"results":[],"futureField":true}"#],
    )
    .expect("task fixture inserts");
    db.execute(
        "INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES('fixture-message','fixture-room','fixture-agent','hello',1.0,'{}')",
        [],
    )
    .expect("message fixture inserts");
}

fn catalog(path: &Path) -> Vec<(String, String, Option<String>)> {
    let db = Connection::open(path).expect("database opens");
    let mut statement = db
        .prepare("SELECT type,name,sql FROM sqlite_master WHERE name NOT LIKE 'sqlite_%' ORDER BY type,name")
        .expect("catalog query prepares");
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("catalog query executes")
        .collect::<Result<Vec<_>, _>>()
        .expect("catalog reads")
}

fn temporary_state() -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock is after Unix epoch")
        .as_nanos();
    let root = std::env::temp_dir().join(format!(
        "runtime-store-schema-{}-{nonce}",
        std::process::id()
    ));
    fs::create_dir_all(&root).expect("temporary state directory creates");
    root
}

fn cleanup(root: PathBuf) {
    fs::remove_dir_all(root).expect("temporary state directory is removed");
}
