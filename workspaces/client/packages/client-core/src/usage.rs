//! Durable usage caches: account limits and session cost estimates.
//! The web page keeps the values it shows in memory. This module keeps the
//! copies that survive a restart and enforces their storage bounds.
use crate::protocol::{Request, Response};
use rusqlite::{Connection, params};
use serde_json::{Value, json};

/// Largest number of saved session cost estimates. Unit: entries.
pub const SESSION_COST_MAX_ENTRIES: usize = 16;
/// Largest total size of saved session cost documents. Unit: UTF-8 bytes.
pub const SESSION_COST_MAX_BYTES: usize = 64 * 1024;

/// Account limits per state directory. Last write wins: the web page sends
/// only snapshots it accepted in memory, and writes keep their order.
pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_limits (
            scope TEXT NOT NULL, account TEXT NOT NULL, document TEXT NOT NULL,
            PRIMARY KEY(scope, account));
        CREATE TABLE IF NOT EXISTS client_session_costs (
            scope TEXT PRIMARY KEY, stored_at REAL NOT NULL, document TEXT NOT NULL);",
    )
}

enum Failure {
    Params(&'static str),
    Storage(String),
}

impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self::Storage(error.to_string())
    }
}

fn scope(params: &Value) -> Result<&str, Failure> {
    params["scope"]
        .as_str()
        .ok_or(Failure::Params("Scope is required."))
}

pub fn dispatch(db: &mut Connection, request: &Request) -> Response {
    let id = request.id.clone();
    let result = match request.method.as_str() {
        "usage.limits.list" => limits_list(db, &request.params),
        "usage.limits.put" => limits_put(db, &request.params),
        "usage.sessionCosts.list" => session_costs_list(db),
        "usage.sessionCosts.put" => session_costs_put(db, &request.params),
        _ => return Response::failure(id, "invalid_request", "Unknown command."),
    };
    match result {
        Ok(value) => Response::success(id, value),
        Err(Failure::Params(message)) => Response::failure(id, "invalid_params", message),
        Err(Failure::Storage(error)) => Response::failure(id, "storage_error", error),
    }
}

fn limits_list(db: &Connection, params: &Value) -> Result<Value, Failure> {
    let mut query = db.prepare("SELECT account, document FROM client_limits WHERE scope=?1")?;
    let rows = query.query_map([scope(params)?], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut accounts = serde_json::Map::new();
    for row in rows {
        let (account, document) = row?;
        accounts.insert(account, serde_json::from_str(&document)?);
    }
    Ok(Value::Object(accounts))
}

fn limits_put(db: &Connection, params: &Value) -> Result<Value, Failure> {
    let scope = scope(params)?;
    let snapshot = params["snapshot"]
        .as_object()
        .ok_or(Failure::Params("Snapshot is required."))?;
    let account = snapshot
        .get("accountKey")
        .and_then(Value::as_str)
        .ok_or(Failure::Params("Snapshot account key is required."))?;
    db.execute(
        "INSERT INTO client_limits(scope,account,document) VALUES(?1,?2,?3)
        ON CONFLICT(scope,account) DO UPDATE SET document=excluded.document",
        params![scope, account, Value::Object(snapshot.clone()).to_string()],
    )?;
    Ok(Value::Null)
}

fn session_costs_list(db: &Connection) -> Result<Value, Failure> {
    let mut query = db.prepare(
        "SELECT scope, stored_at, document FROM client_session_costs ORDER BY stored_at, scope",
    )?;
    let rows = query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, f64>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut costs = Vec::new();
    for row in rows {
        let (scope, stored_at, document) = row?;
        costs.push(json!({
            "scope": scope,
            "storedAt": stored_at,
            "value": serde_json::from_str::<Value>(&document)?,
        }));
    }
    Ok(Value::Array(costs))
}

/// Saves one estimate, then evicts the oldest entries until both bounds hold.
/// The result lists evicted scopes so the page can drop them from memory.
fn session_costs_put(db: &mut Connection, params: &Value) -> Result<Value, Failure> {
    let scope = scope(params)?;
    let stored_at = params["storedAt"]
        .as_f64()
        .filter(|at| at.is_finite())
        .ok_or(Failure::Params("Stored time is required."))?;
    let value = params["value"]
        .as_object()
        .ok_or(Failure::Params("Value is required."))?;
    let document = Value::Object(value.clone()).to_string();
    let transaction = db.transaction()?;
    transaction.execute("DELETE FROM client_session_costs WHERE scope=?1", [scope])?;
    // An oversized estimate replaces the saved one with nothing.
    if document.len() <= SESSION_COST_MAX_BYTES {
        transaction.execute(
            "INSERT INTO client_session_costs(scope,stored_at,document) VALUES(?1,?2,?3)",
            params![scope, stored_at, document],
        )?;
    }
    let mut rows: Vec<(String, usize)> = {
        let mut query = transaction.prepare(
            "SELECT scope, length(CAST(document AS BLOB)) FROM client_session_costs ORDER BY stored_at, scope",
        )?;
        let rows = query.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)? as usize))
        })?;
        rows.collect::<rusqlite::Result<_>>()?
    };
    let mut bytes: usize = rows.iter().map(|(_, size)| size).sum();
    let mut evicted = Vec::new();
    while rows.len() > SESSION_COST_MAX_ENTRIES || bytes > SESSION_COST_MAX_BYTES {
        let (oldest, size) = rows.remove(0);
        transaction.execute("DELETE FROM client_session_costs WHERE scope=?1", [&oldest])?;
        bytes -= size;
        evicted.push(oldest);
    }
    transaction.commit()?;
    Ok(json!({ "evicted": evicted }))
}

#[cfg(test)]
mod tests {
    use crate::{protocol::Request, store::Store};
    use serde_json::{Value, json};

    fn call(store: &mut Store, id: &str, method: &str, params: Value) -> Value {
        let response = store.dispatch(Request {
            id: id.into(),
            method: method.into(),
            params,
        });
        assert!(response.error.is_none(), "{:?}", response.error);
        response.result.unwrap()
    }

    fn refusal(store: &mut Store, method: &str, params: Value) -> String {
        let response = store.dispatch(Request {
            id: "refused".into(),
            method: method.into(),
            params,
        });
        response.error.expect("command must be refused").code
    }

    fn estimate(root: &str, padding: usize) -> Value {
        json!({
            "rootId": root,
            "pricingState": "ready",
            "estimated": true,
            "unknownModels": [],
            "breakdown": {"models": {}, "providers": {}},
            "totalUSD": 1.5,
            "padding": "x".repeat(padding),
        })
    }

    fn put_cost(store: &mut Store, n: usize, padding: usize) -> Value {
        call(
            store,
            &format!("cost-{n}"),
            "usage.sessionCosts.put",
            json!({
                "scope": format!("[\"/state\",\"root{n}\"]"),
                "storedAt": 1000.0 * n as f64,
                "value": estimate(&format!("root{n}"), padding),
            }),
        )
    }

    #[test]
    fn limits_stay_separate_by_state_directory_and_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        {
            let mut store = Store::open(&path).unwrap();
            call(
                &mut store,
                "1",
                "usage.limits.put",
                json!({"scope":"/a","snapshot":{"accountKey":"default","at":10}}),
            );
            call(
                &mut store,
                "2",
                "usage.limits.put",
                json!({"scope":"/a","snapshot":{"accountKey":"work","at":11}}),
            );
            call(
                &mut store,
                "3",
                "usage.limits.put",
                json!({"scope":"/b","snapshot":{"accountKey":"default","at":12}}),
            );
            call(
                &mut store,
                "4",
                "usage.limits.put",
                json!({"scope":"/a","snapshot":{"accountKey":"default","at":20}}),
            );
        }
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            call(&mut store, "5", "usage.limits.list", json!({"scope":"/a"})),
            json!({"default":{"accountKey":"default","at":20},"work":{"accountKey":"work","at":11}})
        );
        assert_eq!(
            call(&mut store, "6", "usage.limits.list", json!({"scope":"/b"})),
            json!({"default":{"accountKey":"default","at":12}})
        );
    }

    #[test]
    fn limits_refuse_a_snapshot_without_an_account_key() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("client.sqlite3")).unwrap();
        assert_eq!(
            refusal(
                &mut store,
                "usage.limits.put",
                json!({"scope":"/a","snapshot":{"at":1}})
            ),
            "invalid_params"
        );
    }

    #[test]
    fn session_costs_keep_the_newest_sixteen_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        {
            let mut store = Store::open(&path).unwrap();
            for n in 0..16 {
                assert_eq!(put_cost(&mut store, n, 0)["evicted"], json!([]));
            }
            assert_eq!(
                put_cost(&mut store, 16, 0)["evicted"],
                json!(["[\"/state\",\"root0\"]"])
            );
        }
        let mut store = Store::open(&path).unwrap();
        let list = call(&mut store, "list", "usage.sessionCosts.list", json!({}));
        let rows = list.as_array().unwrap();
        assert_eq!(rows.len(), 16);
        assert_eq!(rows[0]["storedAt"], json!(1000.0));
        assert_eq!(rows[0]["value"]["rootId"], json!("root1"));
    }

    #[test]
    fn session_costs_evict_oldest_when_total_bytes_exceed_the_limit() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("client.sqlite3")).unwrap();
        for n in 0..2 {
            put_cost(&mut store, n, 30 * 1024);
        }
        assert_eq!(
            put_cost(&mut store, 2, 30 * 1024)["evicted"],
            json!(["[\"/state\",\"root0\"]"])
        );
    }

    #[test]
    fn oversized_session_cost_is_not_saved_and_leaves_other_entries() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("client.sqlite3")).unwrap();
        put_cost(&mut store, 0, 0);
        put_cost(&mut store, 1, 0);
        let scope = "[\"/state\",\"root0\"]";
        call(
            &mut store,
            "oversized",
            "usage.sessionCosts.put",
            json!({"scope": scope, "storedAt": 5000.0, "value": estimate("root0", 70 * 1024)}),
        );
        let list = call(&mut store, "list", "usage.sessionCosts.list", json!({}));
        let scopes: Vec<&str> = list
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["scope"].as_str().unwrap())
            .collect();
        assert_eq!(scopes, ["[\"/state\",\"root1\"]"]);
    }

    #[test]
    fn session_costs_refuse_a_value_that_is_not_an_object() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = Store::open(&directory.path().join("client.sqlite3")).unwrap();
        assert_eq!(
            refusal(
                &mut store,
                "usage.sessionCosts.put",
                json!({"scope":"s","storedAt":1.0,"value":"text"})
            ),
            "invalid_params"
        );
    }
}
