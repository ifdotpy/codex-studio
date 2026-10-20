#![allow(dead_code)]
use rusqlite::{Connection, types::Value as SqlValue};
use serde_json::{Value, json};
use std::path::Path;

/// Analytics tables compared before and after a capture.
pub const TABLES: [&str; 9] = [
    "analytics_agents",
    "analytics_turns",
    "analytics_items",
    "analytics_usage",
    "analytics_notifications",
    "analytics_limits",
    "analytics_meta",
    "analytics_usage_roots",
    "analytics_history",
];

pub fn tables() -> Value {
    Value::Object(
        TABLES
            .iter()
            .map(|t| ((*t).to_owned(), json!([])))
            .collect(),
    )
}

pub fn agent() -> Value {
    json!({"id":"a","name":"Agent","rootId":"a","parentId":null,"accountKey":"default","accountHistory":[],"provider":"codex","threadId":"thread","turnId":"turn","model":"gpt-5","effort":"high","fastMode":false,"daybreakEnabled":false,"cyberAccessProgram":false,"cwd":"/disposable","deletedAt":null,"epoch":0,"status":"running"})
}

/// Creates the schema with the Rust store, then seeds the agent in both databases.
pub async fn seed_state(root: &Path) -> Value {
    let agent = agent();
    let store = studio_runtime_store::RuntimeStore::open(Some(root))
        .await
        .expect("fresh store");
    let record = agent.to_string();
    let id = "a".to_owned();
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                [&id, &record],
            )?;
            Ok(())
        })
        .await
        .expect("seed runtime agent");
    drop(store);
    let projection: Value = [
        "id",
        "name",
        "rootId",
        "parentId",
        "accountKey",
        "accountHistory",
        "provider",
        "threadId",
        "model",
        "effort",
        "fastMode",
        "daybreakEnabled",
        "cyberAccessProgram",
        "cwd",
        "deletedAt",
    ]
    .iter()
    .map(|key| ((*key).to_owned(), agent[*key].clone()))
    .collect();
    analytics(root)
        .execute(
            "INSERT INTO analytics_agents VALUES(?1,?2)",
            ["a", projection.to_string().as_str()],
        )
        .expect("seed analytics agent");
    agent
}

pub fn seed_state_blocking(root: &Path) -> Value {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(seed_state(root))
}

pub fn analytics(root: &Path) -> Connection {
    let db = Connection::open(root.join("analytics.sqlite3")).expect("analytics database");
    studio_runtime_store::cost_aggregate::install(&db).expect("cost aggregate");
    db
}
pub fn actual(db: &Connection, expected: &Value) -> Value {
    let mut result = json!({});
    for (table, _) in expected.as_object().expect("tables") {
        let mut stmt = db
            .prepare(&format!("SELECT * FROM {table} ORDER BY rowid"))
            .expect("read");
        let cols = stmt
            .column_names()
            .into_iter()
            .map(str::to_owned)
            .collect::<Vec<_>>();
        let values = stmt
            .query_map([], |r| {
                let mut v = json!({});
                for (i, c) in cols.iter().enumerate() {
                    v[c] = match r.get::<_, SqlValue>(i)? {
                        SqlValue::Null => Value::Null,
                        SqlValue::Text(s) => json!(s),
                        SqlValue::Integer(n) => json!(n),
                        SqlValue::Real(n) => json!(n),
                        SqlValue::Blob(_) => panic!("not blob"),
                    }
                }
                Ok(v)
            })
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("all rows");
        result[table] = json!(values);
    }
    result
}
