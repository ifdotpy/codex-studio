use std::path::{Path, PathBuf};

use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, MessageRepository, QueueEventId, QueueEventRepository,
    RuntimeLock, RuntimeStore, StoreError, Table, TaskId, TaskRecord, TaskRepository,
    WorkItemRecord,
};

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordFixture {
    agent: AgentRecord,
    task: TaskRecord,
    work: WorkItemRecord,
}

// Adversarial stored event timestamps, in seconds, and their expected SQLite REAL values.
const NUMERIC_CREATED: [(&str, f64); 10] = [
    ("numeric-created-precision", 1_791_788_985.520_981_8),
    ("numeric-created-tenth", 0.1),
    ("numeric-created-large-exponent", 1e21),
    ("numeric-created-integer", 7.0),
    ("numeric-created-integer-real", 5.0),
    ("numeric-created-large-integer-real", 1_791_788_985.0),
    ("numeric-created-17-digits", 1.234_567_890_123_456_7),
    ("numeric-created-over-2pow53", 9_007_199_254_740_992.0),
    ("numeric-created-negative", -0.1),
    ("numeric-created-negative-exponent", -1e-7),
];

async fn seed_records(store: &RuntimeStore) {
    let fixture: RecordFixture =
        serde_json::from_str(include_str!("fixtures/record-round-trip.json"))
            .expect("historical record fixture decodes");
    store.transaction(move |tx| {
        AgentRepository::put(tx, &fixture.agent)?;
        TaskRepository::put(tx, &fixture.task)?;
        Table::<WorkItemRecord>::new(tx).put(&fixture.work)?;
        tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('cross-event','fixture-agent','work_review','{\"fixture\":true}','pending',1720000000.5,0)", [])?;
        tx.execute("INSERT INTO runtime_chat_messages(id,room,sender,text,created,deliveries) VALUES('cross-message','room-fixture','fixture-agent','fixture message',1720000000.5,'{\"delivery\":true,\"attempt\":1}')", [])?;
        for (id, created) in NUMERIC_CREATED {
            tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,error) VALUES(?1,'fixture-agent','work_review','{}','pending',?2,0,?3)",
                rusqlite::params![id, created, (id == "numeric-created-precision").then_some("Zażółć 東京\n\0")])?;
        }
        Ok(())
    }).await.expect("fixture records seed");
}

#[tokio::test]
async fn runtime_lock_excludes_a_second_owner_until_release() {
    let root = temporary_state("locks");
    let first = RuntimeLock::acquire(&root).expect("first lock acquired");
    assert!(matches!(
        RuntimeLock::acquire(&root),
        Err(StoreError::AlreadyOwned)
    ));
    drop(first);
    drop(RuntimeLock::acquire(&root).expect("released lock can be reacquired"));
    root.cleanup().expect("temporary lock state is removed");
}

#[tokio::test]
async fn fixture_records_are_readable_and_writable_in_rust() {
    let root = temporary_state("cross-runtime");
    let store = RuntimeStore::open(Some(&root))
        .await
        .expect("fresh store opens");
    seed_records(&store).await;
    let agent_id = "fixture-agent".to_owned();
    let reader = store
        .test_reader()
        .expect("cross-runtime test operation succeeds");
    let agent = AgentRepository::get(&reader, &AgentId::from(agent_id.as_str()))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(agent.value()["fixtureFlag"], json!(true));
    assert_eq!(agent.value()["fixtureInteger"].as_i64(), Some(7));
    assert_eq!(
        agent.value()["fixtureUnicode"],
        json!("Zażółć 東京\n\u{0000}")
    );
    assert_eq!(
        agent.value()["fixtureNested"],
        json!({"items":[0.1,5.0,9007199254740993_i64,true,{"text":"🦀\t"}]})
    );
    let task = TaskRepository::get(&reader, &TaskId::from("cross-task"))
        .expect("cross-runtime test operation succeeds")
        .expect("record fixture contains the task");
    assert_eq!(task.value()["kind"], json!("command"));
    assert_eq!(task.value()["interactive"], json!(true));
    assert_eq!(task.value()["exitCode"].as_i64(), Some(7));
    assert_eq!(task.value()["created"].as_f64(), Some(1e-7));
    let work = Table::<WorkItemRecord>::new(&reader)
        .get(&TaskId::from("cross-work"))
        .expect("work-board record reads")
        .expect("record fixture contains work");
    assert_eq!(work.value()["fixtureFlag"], json!(true));
    assert_eq!(work.value()["fixtureInteger"].as_i64(), Some(7));
    assert_eq!(work.value()["created"].as_f64(), Some(1e-7));
    assert_eq!(work.value()["updated"].as_f64(), Some(1e21));
    assert_eq!(work.value()["futureField"]["fixtureFlag"], json!(true));

    let event = QueueEventRepository::get(&reader, &QueueEventId::from("cross-event"))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(
        serde_json::from_str::<Value>(&event.text).expect("cross-runtime test operation succeeds")
            ["fixture"],
        true
    );
    let message = MessageRepository::get(
        &reader,
        &studio_runtime_store::ChatMessageId::from("cross-message"),
    )
    .expect("cross-runtime test operation succeeds")
    .expect("cross-runtime test operation succeeds");
    assert_eq!(message.deliveries["delivery"], true);
    drop(reader);

    let failure: Result<(), StoreError> = store
        .transaction(move |_| {
            Err(StoreError::SchemaMismatch(
                "original transaction failure".to_owned(),
            ))
        })
        .await;
    assert!(matches!(
        failure,
        Err(StoreError::SchemaMismatch(message)) if message == "original transaction failure"
    ));

    let write_agent_id = agent_id.clone();
    store
        .transaction(move |transaction| {
            let mut event =
                QueueEventRepository::get(transaction, &QueueEventId::from("cross-event"))?
                    .expect("record fixture contains the event");
            event.text = json!({"rust": true}).to_string();
            event.status = "running".to_owned();
            QueueEventRepository::put(transaction, &event)?;
            for event_id in [
                "numeric-created-precision",
                "numeric-created-tenth",
                "numeric-created-large-exponent",
                "numeric-created-integer",
                "numeric-created-integer-real",
                "numeric-created-large-integer-real",
                "numeric-created-17-digits",
                "numeric-created-over-2pow53",
                "numeric-created-negative",
                "numeric-created-negative-exponent",
            ] {
                let mut numeric_event =
                    QueueEventRepository::get(transaction, &QueueEventId::from(event_id))?
                        .expect("record fixture contains each adversarial numeric event");
                numeric_event.status = "running".to_owned();
                QueueEventRepository::put(transaction, &numeric_event)?;
            }
            let mut agent =
                AgentRepository::get(transaction, &AgentId::from(write_agent_id.as_str()))?
                    .expect("record fixture contains the agent");
            agent.extra.insert("rustAgentFlag".into(), json!(true));
            AgentRepository::put(transaction, &agent)?;
            let mut task = TaskRepository::get(transaction, &TaskId::from("cross-task"))?
                .expect("record fixture contains the task");
            task.set("status", json!("running"));
            TaskRepository::put(transaction, &task)?;
            let work_table = Table::<WorkItemRecord>::new(transaction);
            let mut work = work_table
                .get(&TaskId::from("cross-work"))?
                .expect("record fixture contains work");
            work.set("rustFlag", json!(true));
            work_table.put(&work)?;
            let mut message = MessageRepository::get(
                transaction,
                &studio_runtime_store::ChatMessageId::from("cross-message"),
            )?
            .expect("record fixture contains the chat message");
            message.deliveries["rustReceipt"] = json!(true);
            MessageRepository::put(transaction, &message)?;
            Ok(())
        })
        .await
        .expect("cross-runtime test operation succeeds");
    store
        .verify_schema()
        .expect("cross-runtime test operation succeeds");
    let reader = store
        .test_reader()
        .expect("cross-runtime test operation succeeds");
    let rust_numeric_rows = {
        let mut statement = reader
            .prepare("SELECT seq,payload,hash,deleted FROM sync_entities WHERE collection='event' AND id LIKE 'numeric-created-%' ORDER BY id")
            .expect("cross-runtime test operation succeeds");
        statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, i64>(3)?,
                ))
            })
            .expect("cross-runtime test operation succeeds")
            .collect::<Result<Vec<_>, _>>()
            .expect("cross-runtime test operation succeeds")
    };
    let agent = AgentRepository::get(&reader, &AgentId::from(agent_id.as_str()))
        .expect("cross-runtime test operation succeeds")
        .expect("Rust-written agent remains readable in Rust");
    assert_eq!(agent.value()["fixtureFlag"], json!(true));
    assert_eq!(agent.value()["rustAgentFlag"], json!(true));

    let task = TaskRepository::get(&reader, &TaskId::from("cross-task"))
        .expect("cross-runtime test operation succeeds")
        .expect("Rust-written task remains readable in Rust");
    assert_eq!(task.value()["exitCode"], json!(7));
    assert_eq!(task.value()["status"], json!("running"));
    let work = Table::<WorkItemRecord>::new(&reader)
        .get(&TaskId::from("cross-work"))
        .expect("work-board read succeeds")
        .expect("Rust-written work remains readable in Rust");
    assert_eq!(work.value()["fixtureFlag"], json!(true));
    assert_eq!(work.value()["rustFlag"], json!(true));

    let event = QueueEventRepository::get(&reader, &QueueEventId::from("cross-event"))
        .expect("cross-runtime test operation succeeds")
        .expect("Rust-written event remains readable in Rust");
    assert_eq!(event.status, "running");
    assert_eq!(
        serde_json::from_str::<Value>(&event.text).expect("event text is JSON")["rust"],
        true
    );

    let message = MessageRepository::get(
        &reader,
        &studio_runtime_store::ChatMessageId::from("cross-message"),
    )
    .expect("cross-runtime test operation succeeds")
    .expect("Rust-written message remains readable in Rust");
    assert_eq!(message.deliveries["delivery"], true);
    assert_eq!(message.deliveries["rustReceipt"], true);

    assert_eq!(rust_numeric_rows.len(), NUMERIC_CREATED.len());
    let mut numeric_created = std::collections::BTreeMap::new();
    for (_, encoded, _, _) in &rust_numeric_rows {
        let payload: Value = serde_json::from_str(encoded).expect("Rust sync payload is JSON");
        let id = payload["id"].as_str().expect("sync payload has an id");
        let created = payload["value"]["created"]
            .as_f64()
            .expect("sync payload has a numeric created value");
        numeric_created.insert(id.to_owned(), created);
    }
    for (id, expected) in NUMERIC_CREATED {
        assert_eq!(numeric_created.get(id), Some(&expected), "{id}");
    }
    drop(reader);
    drop(store);

    let reopened = RuntimeStore::open(Some(&root))
        .await
        .expect("cross-runtime test operation succeeds");
    let read_back = reopened
        .test_reader()
        .expect("cross-runtime test operation succeeds");
    let work = Table::<WorkItemRecord>::new(&read_back)
        .get(&TaskId::from("cross-work"))
        .expect("cross-runtime test operation succeeds")
        .expect("cross-runtime test operation succeeds");
    assert_eq!(work.value()["rustFlag"], true);
    reopened
        .verify_schema()
        .expect("cross-runtime test operation succeeds");
    drop(read_back);
    drop(reopened);
    root.cleanup().expect("temporary state is removed");
}

#[tokio::test]
async fn analytics_limit_queries_use_initialized_index_and_state_check_allows_either_catalog() {
    let root = temporary_state("analytics-index");
    drop(
        RuntimeStore::open(Some(&root))
            .await
            .expect("analytics schema initializes"),
    );
    let path = root.join("analytics.sqlite3");
    let db = rusqlite::Connection::open(&path).expect("analytics fixture opens");
    assert!(studio_runtime_store::analytics_capture::check_state_readonly(&root).is_ok());
    db.execute("DROP INDEX analytics_limits_account_at", [])
        .expect("fixture index drops");
    assert!(studio_runtime_store::analytics_capture::check_state_readonly(&root).is_ok());
    db.execute(
        "CREATE INDEX IF NOT EXISTS analytics_limits_account_at ON analytics_limits(account, at)",
        [],
    )
    .expect("fixture index restores");
    drop(db);

    populate_analytics_plan_state(&path);
    let db = rusqlite::Connection::open(&path).expect("populated analytics fixture opens");
    db.execute("DROP INDEX analytics_limits_account_at", [])
        .expect("fixture index drops for baseline plan");
    let without_index = limit_query_plans(&db);
    for (name, plan) in ["count", "page", "last-limit"]
        .into_iter()
        .zip(&without_index)
    {
        println!(
            "analytics_limits {name} plan before index: {}",
            plan.join(" | ")
        );
        assert!(
            plan.iter()
                .any(|line| line.contains("SCAN analytics_limits")),
            "{name} plan did not scan without the index: {plan:?}"
        );
    }
    db.execute(
        "CREATE INDEX IF NOT EXISTS analytics_limits_account_at ON analytics_limits(account, at)",
        [],
    )
    .expect("fixture index recreates after baseline plan");
    let with_index = limit_query_plans(&db);
    for (name, plan) in ["count", "page", "last-limit"].into_iter().zip(&with_index) {
        println!(
            "analytics_limits {name} plan after index: {}",
            plan.join(" | ")
        );
        assert!(
            plan.iter()
                .any(|line| line.contains("analytics_limits_account_at")),
            "{name} plan does not use analytics_limits_account_at: {plan:?}"
        );
    }
    for (name, sql, params) in [
        (
            "item lookup",
            "SELECT record FROM analytics_items WHERE agent=?1 AND json_extract(record,'$.itemId')=?2 AND (?3 IS NULL OR thread=?3) LIMIT 2",
            vec!["agent-0", "item-0", ""],
        ),
        (
            "turn lookup",
            "SELECT record FROM analytics_turns WHERE agent=?1 AND json_extract(record,'$.turnId')=?2 AND (?3 IS NULL OR json_extract(record,'$.threadId')=?3) LIMIT 2",
            vec!["agent-0", "turn-0", ""],
        ),
        (
            "repair turnId count",
            "SELECT COUNT(*) FROM analytics_turns WHERE agent=? AND json_extract(record,'$.turnId')=?",
            vec!["agent-0", "turn-0"],
        ),
    ] {
        let values: Vec<&dyn rusqlite::ToSql> = params
            .iter()
            .map(|value| value as &dyn rusqlite::ToSql)
            .collect();
        let plan = explain_plan(&db, sql, &values);
        println!(
            "analytics {name} plan (Rust fixture with {ANALYTICS_PLAN_ROWS} rows): {}",
            plan.join(" | ")
        );
        assert!(
            !plan.iter().any(|line| {
                (line.starts_with("SCAN analytics_items")
                    || line.starts_with("SCAN analytics_turns"))
                    && !line.contains("USING INDEX")
                    && !line.contains("USING COVERING INDEX")
            }),
            "{name} performs a full table scan: {plan:?}"
        );
    }
    root.cleanup().expect("analytics fixture is removed");
}

// Fixed query-planner corpus sizes, measured in rows and distinct agents.
const ANALYTICS_PLAN_ROWS: i64 = 24_000;
const ANALYTICS_PLAN_AGENTS: i64 = 1_200;

fn populate_analytics_plan_state(path: &Path) {
    let mut db = rusqlite::Connection::open(path).expect("analytics fixture opens");
    let tx = db
        .transaction()
        .expect("analytics fixture transaction opens");
    {
        let mut items = tx
            .prepare("INSERT INTO analytics_items VALUES (?,?,?,?,?,?,?,?,?,?)")
            .expect("item insert prepares");
        let mut turns = tx
            .prepare("INSERT INTO analytics_turns VALUES (?,?,?,?,?)")
            .expect("turn insert prepares");
        let mut limits = tx
            .prepare("INSERT INTO analytics_limits(account,at,record) VALUES (?,?,?)")
            .expect("limit insert prepares");
        for seq in 0..ANALYTICS_PLAN_ROWS {
            let agent = format!("agent-{}", seq % ANALYTICS_PLAN_AGENTS);
            let turn = format!("turn-{seq}");
            let item = format!("item-{seq}");
            items.execute(rusqlite::params![item, agent, agent, "thread", turn, seq,
                "agentMessage", Option::<String>::None, false,
                json!({"id":item,"itemId":item,"agentId":agent,"rootId":agent,
                    "threadId":"thread","turnId":turn,"at":seq,"type":"agentMessage","isTool":false}).to_string()])
                .expect("item seeds");
            turns
                .execute(rusqlite::params![
                    turn,
                    agent,
                    agent,
                    seq,
                    json!({"id":turn,"agentId":agent,"rootId":agent,"threadId":"thread",
                    "turnId":turn,"at":seq,"status":"completed"})
                    .to_string()
                ])
                .expect("turn seeds");
            limits
                .execute(rusqlite::params![
                    agent,
                    seq,
                    json!({"account":agent,"at":seq}).to_string()
                ])
                .expect("limit seeds");
        }
    }
    tx.commit().expect("analytics corpus commits");
}

fn limit_query_plans(db: &rusqlite::Connection) -> [Vec<String>; 3] {
    [
        explain_plan(
            db,
            "SELECT COUNT(*) FROM analytics_limits WHERE account IN (?) AND at>=? AND at<=?",
            &[&"agent-0", &0.0_f64, &(ANALYTICS_PLAN_ROWS as f64)],
        ),
        explain_plan(
            db,
            "SELECT account,at,record FROM analytics_limits WHERE account IN (?) AND at>=? AND at<=? ORDER BY at DESC,id DESC LIMIT ? OFFSET ?",
            &[
                &"agent-0",
                &0.0_f64,
                &(ANALYTICS_PLAN_ROWS as f64),
                &100_i64,
                &0_i64,
            ],
        ),
        explain_plan(
            db,
            "SELECT record FROM analytics_limits WHERE account=? ORDER BY at DESC LIMIT 1",
            &[&"agent-0"],
        ),
    ]
}

fn explain_plan(
    db: &rusqlite::Connection,
    sql: &str,
    params: &[&dyn rusqlite::ToSql],
) -> Vec<String> {
    db.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("query plan prepares")
        .query_map(rusqlite::params_from_iter(params.iter().copied()), |row| {
            row.get::<_, String>(3)
        })
        .expect("query plan runs")
        .collect::<Result<Vec<_>, _>>()
        .expect("query plan rows read")
}

struct TemporaryState(PathBuf, tempfile::TempDir);

impl TemporaryState {
    fn cleanup(self) -> std::io::Result<()> {
        self.1.close()
    }
}

impl std::ops::Deref for TemporaryState {
    type Target = Path;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

fn temporary_state(label: &str) -> TemporaryState {
    let directory = tempfile::Builder::new()
        .prefix(&format!("codex-runtime-store-{label}-"))
        .tempdir()
        .expect("cross-runtime test operation succeeds");
    TemporaryState(directory.path().to_path_buf(), directory)
}
#[tokio::test]
async fn server_identity_registry_is_shared_by_store_connections_and_scoped_to_database() {
    let first_root = temporary_state("identity-registry-first");
    let second_root = temporary_state("identity-registry-second");
    for (root, server_id) in [
        (&*first_root, "server-first"),
        (&*second_root, "server-second"),
    ] {
        let directory = root.join("multi-server");
        std::fs::create_dir_all(&directory).expect("identity directory exists");
        std::fs::write(
            directory.join("identity.json"),
            format!(r#"{{"serverId":"{server_id}"}}"#),
        )
        .expect("identity file created");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("safe identity directory");
            std::fs::set_permissions(
                directory.join("identity.json"),
                std::fs::Permissions::from_mode(0o600),
            )
            .expect("safe identity file");
        }
    }

    let first = RuntimeStore::open(Some(&first_root))
        .await
        .expect("first store opens");
    let second = RuntimeStore::open(Some(&second_root))
        .await
        .expect("second store opens");
    assert_eq!(
        first.local_server_id().expect("identity lookup"),
        Some("server-first".to_owned())
    );
    assert_eq!(
        second.local_server_id().expect("identity lookup"),
        Some("server-second".to_owned())
    );
    first
        .transaction(move |tx| {
            assert_eq!(
                RuntimeStore::transaction_local_server_id(tx)?,
                Some("server-first".to_owned())
            );
            Ok(())
        })
        .await
        .expect("writer sees its store identity");
    second
        .transaction(move |tx| {
            assert_eq!(
                RuntimeStore::transaction_local_server_id(tx)?,
                Some("server-second".to_owned())
            );
            Ok(())
        })
        .await
        .expect("second database sees its own identity");
    let reader = first.test_reader().expect("reader opens");
    let tx = reader
        .unchecked_transaction()
        .expect("read transaction begins");
    assert_eq!(
        RuntimeStore::transaction_local_server_id(&tx).expect("reader identity"),
        Some("server-first".to_owned())
    );
    tx.rollback().expect("read transaction ends");
    first
        .set_local_server_id(Some("server-updated".to_owned()))
        .expect("credential loader updates registry");
    let tx = reader
        .unchecked_transaction()
        .expect("second read transaction begins");
    assert_eq!(
        RuntimeStore::transaction_local_server_id(&tx).expect("updated reader identity"),
        Some("server-updated".to_owned())
    );
    tx.rollback().expect("second read transaction ends");

    drop(first);
    let raw =
        rusqlite::Connection::open(first_root.join("canvas.sqlite3")).expect("raw connection");
    let tx = raw.unchecked_transaction().expect("raw transaction begins");
    assert_eq!(
        RuntimeStore::transaction_local_server_id(&tx).expect("unregistered database identity"),
        None
    );
    tx.rollback().expect("raw transaction ends");
    drop(second);
    first_root.cleanup().expect("first fixture removed");
    second_root.cleanup().expect("second fixture removed");
}
