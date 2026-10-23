//! Durable per-account usage observations and their compact sync projections.

use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::Value;

use crate::records::{RecordOrder, RecordPredicate, Table, UsageResumeRecord};
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult, SyncDocumentsRepository};

pub const MAX_PERSISTED_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PERSISTED_DOCUMENT_DEPTH: usize = 128;
/// Namespace of the single durable observation receipt per account. Unit: key prefix.
const LIMITS_OBSERVATION_PREFIX: &str = "account-limits-observation:";
pub const AUTH_WAIT_NOTICE_SECONDS: f64 = 1800.0;
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    conditional_tables: &[],
    tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[
        TableRequirement {
            name: "sync_entities",
            columns: &["collection", "id", "payload", "deleted"],
        },
        TableRequirement {
            name: "runtime_operation_receipts",
            columns: &["id", "result"],
        },
    ],
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

/// One ordered observation receipt per account; cursor generations are connection-local.
pub fn limits_observation_key(account: &str) -> String {
    format!("{LIMITS_OBSERVATION_PREFIX}{account}")
}

/// Read the freshest durable observation used by resume checks and startup restoration.
pub fn latest_limits_snapshot(
    connection: &Connection,
    account: &str,
) -> StoreResult<Option<Value>> {
    let raw: Option<String> = connection
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [limits_observation_key(account)],
            |row| row.get(0),
        )
        .optional()?;
    let receipt = raw
        .map(|raw| serde_json::from_str::<Value>(&raw))
        .transpose()?;
    if let Some(mut receipt) = receipt
        && let Some(snapshot) = receipt.get_mut("snapshot")
    {
        return Ok(Some(snapshot.take()));
    }
    let raw: Option<String> = connection.query_row(
        "SELECT payload FROM sync_entities WHERE collection='rateLimit' AND id=?1 AND deleted=0",
        [account], |row| row.get(0),
    ).optional()?;
    Ok(raw
        .map(|raw| serde_json::from_str::<Value>(&raw))
        .transpose()?
        .and_then(|mut envelope| envelope.get_mut("value").map(Value::take)))
}

/// Move persisted v3 workspace snapshots once, without changing their account identity.
pub(crate) fn migrate_workspace_limits(transaction: &Transaction<'_>) -> StoreResult<()> {
    let raw: Option<String> = transaction.query_row(
        "SELECT payload FROM sync_entities WHERE collection='workspace' AND id='current' AND deleted=0",
        [], |row| row.get(0),
    ).optional()?;
    let Some(raw) = raw else {
        return Ok(());
    };
    let mut envelope: Value = serde_json::from_str(&raw)?;
    let Some(value) = envelope.get_mut("value").and_then(Value::as_object_mut) else {
        return Ok(());
    };
    let accounts = value.remove("rateLimitsByAccount");
    let default = value.remove("rateLimits");
    if accounts.is_none() && default.is_none() {
        return Ok(());
    }
    if let Some(Value::Object(accounts)) = accounts {
        for (key, snapshot) in accounts {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM sync_entities WHERE collection='rateLimit' AND id=?1)",
                [&key],
                |row| row.get(0),
            )?;
            if !exists {
                SyncDocumentsRepository::put_entity(
                    transaction,
                    "rateLimit",
                    &key,
                    snapshot,
                    false,
                )?;
            }
        }
    }
    let value = envelope
        .get_mut("value")
        .map(Value::take)
        .ok_or_else(|| StoreError::SchemaMismatch("workspace value missing".into()))?;
    SyncDocumentsRepository::put_entity(transaction, "workspace", "current", value, false)?;
    Ok(())
}

/// Restore bounded, validated per-account observations; invalid accounts remain isolated.
pub fn limits_snapshots(connection: &Connection) -> StoreResult<LimitsRead> {
    let mut statement = connection.prepare(
        "SELECT e.id,substr(CAST(e.payload AS BLOB),1,?1),length(CAST(e.payload AS BLOB)),substr(CAST(r.result AS BLOB),1,?1),length(CAST(r.result AS BLOB)) FROM sync_entities e LEFT JOIN runtime_operation_receipts r ON r.id=?2 || e.id WHERE e.collection='rateLimit' AND e.deleted=0",
    )?;
    let mut rows = statement.query(rusqlite::params![
        MAX_PERSISTED_DOCUMENT_BYTES as i64 + 1,
        LIMITS_OBSERVATION_PREFIX
    ])?;
    let mut result = LimitsRead {
        snapshots: Vec::new(),
        diagnostics: Vec::new(),
    };
    while let Some(row) = rows.next()? {
        let key: String = row.get(0)?;
        let bytes: Option<Vec<u8>> = row.get(1)?;
        let length: Option<i64> = row.get(2)?;
        let receipt: Option<Vec<u8>> = row.get(3)?;
        let receipt_length: Option<i64> = row.get(4)?;
        let decoded = (|| {
            if let Some(receipt) = receipt {
                let mut receipt = decode_limits_document(&receipt, receipt_length)?;
                if let Some(snapshot) = receipt.get_mut("snapshot") {
                    return Ok(snapshot.take());
                }
            }
            let bytes = bytes.ok_or_else(|| ("/value".to_owned(), "type".to_owned()))?;
            let mut envelope = decode_limits_document(&bytes, length)?;
            envelope
                .get_mut("value")
                .map(Value::take)
                .ok_or_else(|| ("/value".to_owned(), "type".to_owned()))
        })();
        let (snapshot, invalid) = match decoded {
            Ok(snapshot) => {
                let invalid = if snapshot
                    .get("accountKey")
                    .is_some_and(|account| account != &key)
                {
                    Some(("/accountKey".to_owned(), "identity".to_owned()))
                } else {
                    match validate_account_limits(&snapshot) {
                        Ok(error) => error,
                        Err(()) => Some(("/".to_owned(), "schemaCompile".to_owned())),
                    }
                };
                (Some(snapshot), invalid)
            }
            Err(error) => (None, Some(error)),
        };
        if let Some((pointer, keyword)) = invalid {
            result.diagnostics.push(LimitsReadDiagnostic {
                workspace_row: "rateLimit",
                account_key: key,
                pointer,
                keyword,
            });
        } else if let Some(snapshot) = snapshot {
            result.snapshots.push((key, snapshot));
        }
    }
    Ok(result)
}

fn decode_limits_document(bytes: &[u8], length: Option<i64>) -> Result<Value, (String, String)> {
    if length.unwrap_or_default() > MAX_PERSISTED_DOCUMENT_BYTES as i64
        || !persisted_document_within_bounds(bytes)
    {
        return Err(("/".to_owned(), "resourceLimit".to_owned()));
    }
    serde_json::from_slice(bytes).map_err(|_| ("/".to_owned(), "parse".to_owned()))
}

fn validate_account_limits(value: &Value) -> Result<Option<(String, String)>, ()> {
    studio_runtime_schema::first_error("workspace-usage-limits/UsageLimitsResponse", value)
        .map_err(|_| ())
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
    Ok(Table::<UsageResumeRecord>::new(connection)
        .list_matching(
            Some(RecordPredicate::UsageResumeAuthRecoveryBefore(
                now - AUTH_WAIT_NOTICE_SECONDS,
            )),
            RecordOrder::RowId,
        )?
        .into_iter()
        .filter_map(|record| {
            record
                .get("accountKey")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::{authentication_recovery_account_keys, limits_snapshots};
    use rusqlite::Connection;
    use serde_json::{Value, json};

    fn database(payload: &Value) -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory SQLite");
        connection.execute_batch("CREATE TABLE sync_entities(collection TEXT, id TEXT, payload TEXT, deleted INTEGER); CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,result TEXT);").expect("fixture tables");
        if let Some(accounts) = payload
            .pointer("/value/rateLimitsByAccount")
            .and_then(Value::as_object)
        {
            for (key, snapshot) in accounts {
                connection
                    .execute(
                        "INSERT INTO sync_entities VALUES('rateLimit',?1,?2,0)",
                        rusqlite::params![key, json!({"value":snapshot}).to_string()],
                    )
                    .expect("fixture row");
            }
        } else if payload["value"] == false {
            connection
                .execute(
                    "INSERT INTO sync_entities VALUES('rateLimit','default',?1,0)",
                    [payload.to_string()],
                )
                .expect("invalid fixture row");
        }
        connection
    }

    #[test]
    fn account_limits_ignore_python_single_account_fallback() {
        let db = database(&json!({"value":{"rateLimits":{
            "accountKey":"default","at":1.0,"data":{"status":"ready"}
        }}}));
        let read = limits_snapshots(&db).expect("read current account map");
        assert!(
            read.snapshots.is_empty(),
            "only rateLimitsByAccount restores accounts"
        );
    }

    #[test]
    fn reads_valid_account_map() {
        let alpha = json!({"accountKey":"alpha","at":1.0,"data":{"status":"ready"}});
        let db = database(
            &json!({"value":{"rateLimitsByAccount":{"alpha":alpha},"rateLimits":{"accountKey":"default","at":null,"data":{"status":"ready"}}}}),
        );
        let restored = limits_snapshots(&db).expect("read");
        assert!(restored.snapshots.contains(&("alpha".to_owned(), alpha)));
        assert!(!restored.snapshots.iter().any(|(key, _)| key == "default"));
        assert!(!restored.snapshots.iter().any(|(key, _)| key == "missing"));
    }

    #[test]
    fn malformed_envelope_reports_safe_location_and_limits_payload_bounds() {
        let db = database(&json!({"value":false}));
        let read = limits_snapshots(&db).expect("read diagnostic");
        assert_eq!(read.diagnostics[0].workspace_row, "rateLimit");
        assert_eq!(read.diagnostics[0].pointer, "");
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
        db.execute_batch(
            "CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,result TEXT)",
        )
        .expect("receipts");
        db.execute(
            "INSERT INTO sync_entities VALUES('rateLimit','default',?1,0)",
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
    fn invalid_mapped_default_reports_diagnostic() {
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
    fn workspace_limits_migrate_once_without_overwriting_account_entities() {
        let mut db = Connection::open_in_memory().expect("SQLite");
        db.execute_batch("CREATE TABLE sync_entities(collection TEXT,id TEXT,seq INTEGER,hash TEXT,payload TEXT,deleted INTEGER,PRIMARY KEY(collection,id)); CREATE TABLE sync_documents(seq INTEGER); CREATE TABLE sync_versions(seq INTEGER); CREATE TABLE runtime_operation_receipts(id TEXT PRIMARY KEY,result TEXT);").expect("migration fixture");
        let tx = db.transaction().expect("transaction");
        let newer = json!({"accountKey":"alpha","at":2.0,"data":{"accountId":"newer"}});
        let beta = json!({"accountKey":"beta","at":1.0,"data":{"accountId":"beta"}});
        crate::SyncDocumentsRepository::put_entity(&tx,"workspace","current",json!({"stateDir":"fixture","rateLimitsByAccount":{"alpha":{"accountKey":"alpha","at":1.0},"beta":beta},"rateLimits":{"accountKey":"default","at":1.0}}),false).expect("workspace");
        crate::SyncDocumentsRepository::put_entity(&tx, "rateLimit", "alpha", newer.clone(), false)
            .expect("existing account");
        super::migrate_workspace_limits(&tx).expect("migration");
        let seq = crate::SyncDocumentsRepository::entity_sequence(&tx).expect("sequence");
        super::migrate_workspace_limits(&tx).expect("repeat migration");
        assert_eq!(
            crate::SyncDocumentsRepository::entity_sequence(&tx).expect("sequence"),
            seq
        );
        let raw: String = tx
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='workspace'",
                [],
                |row| row.get(0),
            )
            .expect("workspace");
        assert_eq!(
            serde_json::from_str::<Value>(&raw).expect("JSON")["value"],
            json!({"stateDir":"fixture"})
        );
        let restored = limits_snapshots(&tx).expect("restored accounts");
        assert!(restored.diagnostics.is_empty());
        assert_eq!(restored.snapshots.len(), 2);
        assert!(restored.snapshots.contains(&("alpha".to_owned(), newer)));
        assert!(restored.snapshots.contains(&("beta".to_owned(), beta)));
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
