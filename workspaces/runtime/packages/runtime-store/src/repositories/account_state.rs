//! Read and validate persisted account usage snapshots from the workspace row.

use jsonschema::Validator;
use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;
use std::sync::OnceLock;

use crate::StoreResult;
use crate::schema::{SchemaRequirements, TableRequirement};

pub const MAX_PERSISTED_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PERSISTED_DOCUMENT_DEPTH: usize = 128;
pub const AUTH_WAIT_NOTICE_SECONDS: f64 = 1800.0;
const WORKSPACE_SCHEMA: &str =
    include_str!("../../../runtime-http/workspace-usage-limits.schema.json");

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    conditional_tables: &[],
    tables: &[TableRequirement {
        name: "runtime_usage_resumes",
        columns: &["id", "agent", "record"],
    }],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[TableRequirement {
        name: "sync_entities",
        columns: &["collection", "id", "payload", "deleted"],
    }],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

pub struct LimitsRead {
    pub snapshots: Vec<(String, Value)>,
    pub diagnostics: Vec<LimitsReadDiagnostic>,
}

pub struct LimitsReadDiagnostic {
    pub workspace_row: &'static str,
    pub account_key: String,
    pub pointer: String,
    pub keyword: String,
}

/// Read the startup workspace envelope with byte/depth bounds and schema
/// validation. Invalid envelopes are reported without exposing stored values.
pub fn limits_snapshots(connection: &Connection) -> StoreResult<LimitsRead> {
    let payload = connection
        .query_row(
            "SELECT substr(CAST(payload AS BLOB),1,?1), length(CAST(payload AS BLOB)) FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0",
            [MAX_PERSISTED_DOCUMENT_BYTES as i64 + 1],
            |row| Ok((row.get::<_, Option<Vec<u8>>>(0)?, row.get::<_, Option<i64>>(1)?)),
        )
        .optional()?;
    let Some(payload) = payload else {
        return Ok(LimitsRead {
            snapshots: Vec::new(),
            diagnostics: Vec::new(),
        });
    };
    let Some(bytes) = payload.0 else {
        return Ok(LimitsRead {
            snapshots: Vec::new(),
            diagnostics: Vec::new(),
        });
    };
    if payload.1.unwrap_or_default() > MAX_PERSISTED_DOCUMENT_BYTES as i64
        || !persisted_document_within_bounds(&bytes)
    {
        return Ok(invalid_envelope("/", "resourceLimit"));
    }
    let document: Value = match serde_json::from_slice(&bytes) {
        Ok(document) => document,
        Err(_) => return Ok(invalid_envelope("/", "parse")),
    };
    let Some(value) = document.get("value").filter(|value| value.is_object()) else {
        return Ok(invalid_envelope("/value", "type"));
    };
    let mut projection = value.clone();
    if projection
        .get("rateLimitsByAccount")
        .is_some_and(Value::is_object)
    {
        projection["rateLimitsByAccount"] = serde_json::json!({});
    }
    if projection
        .get("rateLimits")
        .is_some_and(|value| value.is_object())
    {
        projection["rateLimits"] = Value::Null;
    }
    match validate_workspace(&projection) {
        Ok(Some((pointer, keyword))) => {
            return Ok(invalid_envelope(&format!("/value{pointer}"), &keyword));
        }
        Err(()) => return Ok(invalid_envelope("/value", "schemaCompile")),
        Ok(None) => {}
    }
    let mut snapshots = Vec::new();
    let mut diagnostics = Vec::new();
    let has_default_map_entry = value
        .get("rateLimitsByAccount")
        .and_then(Value::as_object)
        .is_some_and(|map| map.contains_key("default"));
    if let Some(map) = value.get("rateLimitsByAccount").and_then(Value::as_object) {
        for (key, snapshot) in map {
            match validate_account_limits(snapshot) {
                Ok(Some((pointer, keyword))) => diagnostics.push(LimitsReadDiagnostic {
                    workspace_row: "workspace/current",
                    account_key: key.clone(),
                    pointer,
                    keyword,
                }),
                Err(()) => diagnostics.push(LimitsReadDiagnostic {
                    workspace_row: "workspace/current",
                    account_key: key.clone(),
                    pointer: "/".to_owned(),
                    keyword: "schemaCompile".to_owned(),
                }),
                Ok(None) => snapshots.push((key.clone(), snapshot.clone())),
            }
        }
    }
    if !has_default_map_entry
        && let Some(legacy) = value.get("rateLimits").filter(|value| value.is_object())
    {
        match validate_account_limits(legacy) {
            Ok(Some((pointer, keyword))) => diagnostics.push(LimitsReadDiagnostic {
                workspace_row: "workspace/current",
                account_key: "default".to_owned(),
                pointer,
                keyword,
            }),
            Err(()) => diagnostics.push(LimitsReadDiagnostic {
                workspace_row: "workspace/current",
                account_key: "default".to_owned(),
                pointer: "/".to_owned(),
                keyword: "schemaCompile".to_owned(),
            }),
            Ok(None) => snapshots.push(("default".to_owned(), legacy.clone())),
        }
    }
    Ok(LimitsRead {
        snapshots,
        diagnostics,
    })
}

fn invalid_envelope(pointer: &str, keyword: &str) -> LimitsRead {
    LimitsRead {
        snapshots: Vec::new(),
        diagnostics: vec![LimitsReadDiagnostic {
            workspace_row: "workspace/current",
            account_key: "*".to_owned(),
            pointer: pointer.to_owned(),
            keyword: keyword.to_owned(),
        }],
    }
}

fn validate_workspace(value: &Value) -> Result<Option<(String, String)>, ()> {
    static VALIDATOR: OnceLock<Result<Validator, ()>> = OnceLock::new();
    let validator = VALIDATOR
        .get_or_init(|| {
            let schema: Value = serde_json::from_str(WORKSPACE_SCHEMA).map_err(|_| ())?;
            jsonschema::validator_for(&schema).map_err(|_| ())
        })
        .as_ref()
        .map_err(|_| ())?;
    Ok(validator.iter_errors(value).next().map(|error| {
        (
            error.instance_path().to_string(),
            error.kind().keyword().to_owned(),
        )
    }))
}

fn validate_account_limits(value: &Value) -> Result<Option<(String, String)>, ()> {
    let account_key = value
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("");
    let mut snapshots = serde_json::Map::new();
    snapshots.insert(account_key.to_owned(), value.clone());
    let wrapped = serde_json::json!({"rateLimitsByAccount": snapshots});
    validate_workspace(&wrapped).map(|result| {
        result.map(|(pointer, keyword)| {
            let escaped_key = account_key.replace('~', "~0").replace('/', "~1");
            let prefix = format!("/rateLimitsByAccount/{escaped_key}");
            (
                pointer.strip_prefix(&prefix).unwrap_or(&pointer).to_owned(),
                keyword,
            )
        })
    })
}

pub fn persisted_document_within_bounds(bytes: &[u8]) -> bool {
    let mut depth = 0usize;
    let mut quoted = false;
    let mut escaped = false;
    for byte in bytes {
        if quoted {
            if escaped {
                escaped = false;
            } else if *byte == b'\\' {
                escaped = true;
            } else if *byte == b'"' {
                quoted = false;
            }
            continue;
        }
        match *byte {
            b'"' => quoted = true,
            b'{' | b'[' => {
                depth += 1;
                if depth > MAX_PERSISTED_DOCUMENT_DEPTH {
                    return false;
                }
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    true
}

/// Return account keys whose scheduled auth resume has passed the recovery
/// notice threshold. The caller supplies time to keep service policy testable.
pub fn authentication_recovery_account_keys(
    connection: &Connection,
    now: f64,
) -> StoreResult<Vec<String>> {
    let mut statement = connection.prepare(
        "SELECT record FROM runtime_usage_resumes WHERE json_extract(record,'$.status')='scheduled' AND json_extract(record,'$.cause')='auth' AND json_extract(record,'$.failedAt')<=?1",
    )?;
    let rows = statement.query_map([now - AUTH_WAIT_NOTICE_SECONDS], |row| {
        row.get::<_, String>(0)
    })?;
    let mut account_keys = Vec::new();
    for row in rows {
        let record = serde_json::from_str::<Value>(&row?)?;
        if let Some(key) = record.get("accountKey").and_then(Value::as_str) {
            account_keys.push(key.to_owned());
        }
    }
    Ok(account_keys)
}

#[cfg(test)]
mod tests {
    use super::{authentication_recovery_account_keys, limits_snapshots};
    use rusqlite::Connection;
    use serde_json::{Value, json};

    fn database(payload: &Value) -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory SQLite");
        connection.execute("CREATE TABLE sync_entities(collection TEXT, id TEXT, payload TEXT, deleted INTEGER)", []).expect("fixture table");
        connection
            .execute(
                "INSERT INTO sync_entities VALUES('workspace','current',?1,0)",
                [payload.to_string()],
            )
            .expect("fixture row");
        connection
    }

    #[test]
    fn reads_valid_account_map_and_legacy_default() {
        let alpha = json!({"accountKey":"alpha","at":1.0,"data":{"status":"ready"}});
        let db = database(
            &json!({"value":{"rateLimitsByAccount":{"alpha":alpha},"rateLimits":{"accountKey":"default","at":null,"data":{"status":"ready"}}}}),
        );
        let restored = limits_snapshots(&db).expect("read");
        assert!(restored.snapshots.contains(&("alpha".to_owned(), alpha)));
        assert!(restored.snapshots.iter().any(|(key, _)| key == "default"));
        assert!(!restored.snapshots.iter().any(|(key, _)| key == "missing"));
    }

    #[test]
    fn malformed_envelope_reports_safe_location_and_limits_payload_bounds() {
        let db = database(&json!({"value":false}));
        let read = limits_snapshots(&db).expect("read diagnostic");
        assert_eq!(read.diagnostics[0].workspace_row, "workspace/current");
        assert_eq!(read.diagnostics[0].pointer, "/value");
        assert!(read.snapshots.is_empty());

        let deep = format!(
            "{{\"value\":{{\"rateLimitsByAccount\":{{\"default\":{{\"accountKey\":\"default\",\"at\":1,\"data\":{{\"x\":{}0{} }} }} }} }} }}",
            "[".repeat(10_000),
            "]".repeat(10_000)
        );
        let db = Connection::open_in_memory().expect("SQLite");
        db.execute(
            "CREATE TABLE sync_entities(collection TEXT,id TEXT,payload TEXT,deleted INTEGER)",
            [],
        )
        .expect("table");
        db.execute(
            "INSERT INTO sync_entities VALUES('workspace','current',?1,0)",
            [deep],
        )
        .expect("row");
        let read = limits_snapshots(&db).expect("bounded read");
        assert_eq!(read.diagnostics[0].keyword, "resourceLimit");
    }

    #[test]
    fn malformed_account_snapshot_has_account_pointer_and_no_value_text() {
        let marker = "LIMIT_SECRET_93841";
        let db = database(&json!({"value":{"rateLimitsByAccount":{
            "account-a":{"accountKey":"account-a","at":marker,"data":{"status":"ready"}}
        }}}));
        let read = limits_snapshots(&db).expect("read diagnostic");
        assert!(read.snapshots.is_empty());
        assert_eq!(read.diagnostics[0].account_key, "account-a");
        assert_eq!(read.diagnostics[0].pointer, "/at");
        let rendered = format!(
            "{} {}",
            read.diagnostics[0].pointer, read.diagnostics[0].keyword
        );
        assert!(!rendered.contains(marker));
    }

    #[test]
    fn invalid_mapped_default_suppresses_legacy_and_reports_diagnostic() {
        let payload = json!({"value": {
            "rateLimitsByAccount": {"default": {
                "accountKey":"default", "at":1.0,
                "data":{"rateLimits":{"primary":{"usedPercent":1,"placeholder":"x"}}}
            }},
            "rateLimits":{"accountKey":"default","at":2.0,"data":{"status":"legacy"}}
        }});
        let read = limits_snapshots(&database(&payload)).expect("validated store projection");
        assert!(read.snapshots.is_empty());
        assert_eq!(read.diagnostics.len(), 1);
        assert_eq!(read.diagnostics[0].account_key, "default");
        assert_eq!(read.diagnostics[0].pointer, "/data");
    }

    #[test]
    fn reads_due_authentication_recovery_keys() {
        let db = Connection::open_in_memory().expect("SQLite");
        db.execute_batch("CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL)").expect("table");
        db.execute("INSERT INTO runtime_usage_resumes VALUES('due','agent',?1)", [json!({"status":"scheduled","cause":"auth","accountKey":"default","failedAt":100.0}).to_string()]).expect("row");
        assert_eq!(
            authentication_recovery_account_keys(&db, 1900.0).expect("read"),
            vec!["default"]
        );
    }
}
inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "account_state",
        requirements: &REQUIREMENTS,
    }
}
