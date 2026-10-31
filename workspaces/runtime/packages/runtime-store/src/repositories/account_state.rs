//! Durable per-account usage observations and their compact sync projections.

use rusqlite::{Connection, OptionalExtension};
use serde_json::Value;

use crate::records::{RecordOrder, RecordPredicate, Table, UsageResumeRecord};
use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult, Transaction};
use studio_runtime_schema::persistence::ProviderScope;

pub const MAX_PERSISTED_DOCUMENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_PERSISTED_DOCUMENT_DEPTH: usize = 128;
/// Current Claude account sign-in refusal, also retained by the retired auth-wait migration.
pub const CLAUDE_SIGN_IN_MESSAGE: &str = "Sign in with claude auth login first";
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
            name: "runtime_account_state",
            columns: &[
                "account",
                "next_generation",
                "connection_generation",
                "journal_ack",
                "recovery_required",
                "streams",
                "limits_observation",
            ],
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

/// Freshness fields stored privately between content changes. Unit: Unix seconds.
pub const LIMITS_OBSERVATION_TIME_FIELDS: &[&str] = &["at", "readAt", "checkedAt", "processedAt"];

/// Move observation times onto a decoded account snapshot without cloning provider data.
pub fn apply_limits_observation(snapshot: &mut Value, observation: &mut Value) {
    let (Some(snapshot), Some(observation)) =
        (snapshot.as_object_mut(), observation.as_object_mut())
    else {
        return;
    };
    if !LIMITS_OBSERVATION_TIME_FIELDS
        .iter()
        .any(|field| observation.contains_key(*field))
    {
        return;
    }
    for field in LIMITS_OBSERVATION_TIME_FIELDS {
        if let Some((key, value)) = observation.remove_entry(*field) {
            snapshot.insert(key, value);
        } else {
            snapshot.remove(*field);
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum ScopeError {
    #[error("Provider scope is unavailable")]
    Unavailable,
    #[error(transparent)]
    Store(#[from] StoreError),
}

/// Current generation remains fenced even when its connection is unknown.
#[derive(Debug)]
pub enum ProviderState {
    Connected { generation: u64, connection: String },
    Held { generation: u64 },
}

impl ProviderState {
    pub fn generation(&self) -> u64 {
        match self {
            Self::Connected { generation, .. } | Self::Held { generation } => *generation,
        }
    }
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AgentScopeFields {
    provider_stream: Option<StreamFields>,
}
#[derive(serde::Deserialize)]
struct StreamFields {
    generation: Option<u64>,
    connection: Option<String>,
    ack: Option<u64>,
}
impl StreamFields {
    fn into_state(self) -> StoreResult<Option<(ProviderState, u64)>> {
        match (self.generation, self.connection, self.ack) {
            (Some(generation), connection, ack) => {
                Ok(Some((state(generation, connection), ack.unwrap_or(0))))
            }
            (None, None, None) => Ok(None),
            (None, _, _) => Err(StoreError::RecordContract(
                "provider connection or ACK has no generation",
            )),
        }
    }
}

fn agent_scope_fields(agent: &crate::AgentRecord) -> StoreResult<AgentScopeFields> {
    use serde::Deserialize;
    use serde::de::IntoDeserializer;
    let fields = serde::de::value::MapDeserializer::<_, serde_json::Error>::new(
        agent
            .extra
            .iter()
            .map(|(key, value)| (key.as_str().into_deserializer(), value)),
    );
    Ok(AgentScopeFields::deserialize(fields)?)
}

fn state(generation: u64, connection: Option<String>) -> ProviderState {
    match connection {
        Some(connection) if !connection.is_empty() => ProviderState::Connected {
            generation,
            connection,
        },
        _ => ProviderState::Held { generation },
    }
}

/// Source owner for the current provider generation and journal ACK.
#[derive(Clone, Copy)]
pub enum ProviderOwner<'a> {
    Account(&'a str),
    Agent(&'a crate::AgentRecord),
}

/// Read the host account row or the VM agent's current stream exactly once.
pub fn provider_state(
    db: &Connection,
    owner: ProviderOwner<'_>,
) -> StoreResult<Option<(ProviderState, u64)>> {
    let account = match owner {
        ProviderOwner::Account(account) => std::borrow::Cow::Borrowed(account),
        ProviderOwner::Agent(agent) => {
            let fields = agent_scope_fields(agent)?;
            if matches!(agent.execution_mode, Some(crate::AgentExecutionMode::Vm)) {
                return fields
                    .provider_stream
                    .map(StreamFields::into_state)
                    .transpose()
                    .map(Option::flatten);
            }
            std::borrow::Cow::Borrowed(agent.account_key.as_str())
        }
    };
    let row: Option<(Option<i64>, Option<String>, i64)> = db.query_row(
        "SELECT connection_generation,connection_id,journal_ack FROM runtime_account_state WHERE account=?1",
        [account.as_ref()], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    ).optional()?;
    match row {
        None | Some((None, None, _)) => Ok(None),
        Some((None, Some(_), _)) => Err(StoreError::RecordContract(
            "provider connection has no generation",
        )),
        Some((Some(generation), connection, ack)) => Ok(Some((
            state(
                u64::try_from(generation)
                    .map_err(|_| StoreError::RecordContract("provider generation is negative"))?,
                connection,
            ),
            u64::try_from(ack)
                .map_err(|_| StoreError::RecordContract("provider journal ACK is negative"))?,
        ))),
    }
}

/// Read the current host or VM provider identity in the owner's transaction.
pub fn current_provider_scope(
    tx: &Transaction<'_>,
    agent: &crate::AgentRecord,
) -> Result<ProviderScope, ScopeError> {
    match provider_state(tx, ProviderOwner::Agent(agent))? {
        Some((
            ProviderState::Connected {
                generation,
                connection,
            },
            _,
        )) => Ok(ProviderScope {
            generation,
            connection,
        }),
        Some((ProviderState::Held { .. }, _)) | None => Err(ScopeError::Unavailable),
    }
}

/// Initialize only current account state; migrations seed historical generations once.
pub fn ensure_account(tx: &Transaction<'_>, account: &str) -> StoreResult<()> {
    tx.execute("INSERT INTO runtime_account_state(account,next_generation,journal_ack,recovery_required,streams) VALUES(?1,0,0,0,'null') ON CONFLICT(account) DO NOTHING", [account])?;
    Ok(())
}

/// Last allocated direct-provider generation. Increment in the writer transaction.
pub fn allocate_next_generation(tx: &Transaction<'_>, account: &str) -> StoreResult<u64> {
    ensure_account(tx, account)?;
    let generation: i64 = tx.query_row("UPDATE runtime_account_state SET next_generation=next_generation+1 WHERE account=?1 AND next_generation>=0 AND next_generation<?2 RETURNING next_generation", (account, i64::MAX), |row| row.get(0))?;
    Ok(generation as u64)
}

/// Contiguous applied journal prefix for the current host provider generation.
pub fn provider_checkpoint(
    connection: &Connection,
    account: &str,
    generation: u64,
) -> StoreResult<u64> {
    let generation = i64::try_from(generation).map_err(|_| {
        StoreError::RecordContract("provider generation exceeds SQLite integer range")
    })?;
    let cursor: i64 = connection.query_row("SELECT journal_ack FROM runtime_account_state WHERE account=?1 AND connection_generation=?2", rusqlite::params![account,generation], |row| row.get(0)).optional()?.unwrap_or(0);
    u64::try_from(cursor)
        .map_err(|_| StoreError::RecordContract("provider journal ACK is negative"))
}

/// A generation transition is explicit; an ACK can never replace another generation.
pub fn set_provider_checkpoint(
    tx: &Transaction<'_>,
    account: &str,
    generation: u64,
    cursor: u64,
) -> StoreResult<()> {
    let generation = i64::try_from(generation).map_err(|_| {
        StoreError::RecordContract("provider generation exceeds SQLite integer range")
    })?;
    let cursor = i64::try_from(cursor).map_err(|_| {
        StoreError::RecordContract("provider journal ACK exceeds SQLite integer range")
    })?;
    ensure_account(tx, account)?;
    let changed = tx.execute("UPDATE runtime_account_state SET connection_generation=?2,journal_ack=MAX(journal_ack,?3) WHERE account=?1 AND (connection_generation IS NULL OR connection_generation=?2)", rusqlite::params![account,generation,cursor])?;
    if changed == 0 {
        return Err(StoreError::RecordContract(
            "provider checkpoint belongs to a different generation",
        ));
    }
    Ok(())
}

/// Serialized stream metadata owned by the account worker; pending flushes live in outbox.
pub fn streams(connection: &Connection, account: &str) -> StoreResult<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT streams FROM runtime_account_state WHERE account=?1 AND streams!='null'",
            [account],
            |row| row.get(0),
        )
        .optional()?)
}

pub fn put_streams(tx: &Transaction<'_>, account: &str, streams: &str) -> StoreResult<()> {
    ensure_account(tx, account)?;
    tx.execute(
        "UPDATE runtime_account_state SET streams=?2 WHERE account=?1",
        (account, streams),
    )?;
    Ok(())
}

/// Ordered observation metadata without a second copy of the provider snapshot.
pub fn limits_observation(connection: &Connection, account: &str) -> StoreResult<Option<String>> {
    Ok(connection
        .query_row(
            "SELECT limits_observation FROM runtime_account_state WHERE account=?1",
            [account],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten())
}

pub fn put_limits_observation(
    tx: &Transaction<'_>,
    account: &str,
    observation: &str,
) -> StoreResult<()> {
    ensure_account(tx, account)?;
    tx.execute(
        "UPDATE runtime_account_state SET limits_observation=?2 WHERE account=?1",
        (account, observation),
    )?;
    Ok(())
}

/// Read the freshest durable observation used by resume checks and startup restoration.
pub fn latest_limits_snapshot(
    connection: &Connection,
    account: &str,
) -> StoreResult<Option<Value>> {
    let mut observation = limits_observation(connection, account)?
        .map(|raw| serde_json::from_str::<Value>(&raw))
        .transpose()?
        .unwrap_or(Value::Null);
    limits_snapshot_with_observation(connection, account, &mut observation)
}

/// Join one already-decoded observation with its account entity; each document parses once.
pub fn limits_snapshot_with_observation(
    connection: &Connection,
    account: &str,
    observation: &mut Value,
) -> StoreResult<Option<Value>> {
    let raw: Option<String> = connection.query_row(
        "SELECT payload FROM sync_entities WHERE collection='rateLimit' AND id=?1 AND deleted=0",
        [account], |row| row.get(0),
    ).optional()?;
    let mut snapshot = raw
        .map(|raw| serde_json::from_str::<Value>(&raw))
        .transpose()?
        .and_then(|mut envelope| envelope.get_mut("value").map(Value::take));
    if let Some(snapshot) = &mut snapshot {
        apply_limits_observation(snapshot, observation);
    }
    Ok(snapshot)
}

/// Move persisted v3 workspace snapshots once, without changing their account identity.
/// Restore bounded, validated per-account observations; invalid accounts remain isolated.
pub fn limits_snapshots(connection: &Connection) -> StoreResult<LimitsRead> {
    let mut statement = connection.prepare(
        "SELECT e.id,substr(CAST(e.payload AS BLOB),1,?1),length(CAST(e.payload AS BLOB)),substr(CAST(a.limits_observation AS BLOB),1,?1),length(CAST(a.limits_observation AS BLOB)) FROM sync_entities e LEFT JOIN runtime_account_state a ON a.account=e.id WHERE e.collection='rateLimit' AND e.deleted=0",
    )?;
    let mut rows = statement.query(rusqlite::params![MAX_PERSISTED_DOCUMENT_BYTES as i64 + 1])?;
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
            let mut receipt = receipt
                .map(|receipt| decode_limits_document(&receipt, receipt_length))
                .transpose()?
                .unwrap_or(Value::Null);
            let bytes = bytes.ok_or_else(|| ("/value".to_owned(), "type".to_owned()))?;
            let mut envelope = decode_limits_document(&bytes, length)?;
            let mut snapshot = envelope
                .get_mut("value")
                .map(Value::take)
                .ok_or_else(|| ("/value".to_owned(), "type".to_owned()))?;
            apply_limits_observation(&mut snapshot, &mut receipt);
            Ok(snapshot)
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
                .account_key
                .value()
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

    #[test]
    fn metadata_only_vm_stream_has_no_provider_state() {
        let fields = super::StreamFields {
            generation: None,
            connection: None,
            ack: None,
        };
        assert!(fields.into_state().unwrap().is_none());
        for stream in [
            r#"{"streams":{},"connection":"c"}"#,
            r#"{"streams":{},"ack":0}"#,
        ] {
            let fields: super::StreamFields = serde_json::from_str(stream).unwrap();
            assert!(fields.into_state().is_err());
        }
    }

    #[test]
    fn receipt_account_checkpoint_fences_generations_and_never_regresses() {
        let mut db = Connection::open_in_memory().expect("SQLite");
        crate::receipt_schema::create(&db).expect("target schema");
        let tx = db.transaction().expect("transaction");
        assert_eq!(
            super::provider_checkpoint(&tx, "alpha", 7).expect("empty"),
            0
        );
        super::set_provider_checkpoint(&tx, "alpha", 7, 10).expect("initialize");
        super::set_provider_checkpoint(&tx, "alpha", 7, 4).expect("repeat earlier ACK");
        assert_eq!(
            super::provider_checkpoint(&tx, "alpha", 7).expect("current"),
            10
        );
        assert_eq!(
            super::provider_checkpoint(&tx, "alpha", 6).expect("old generation"),
            0
        );
        assert!(super::set_provider_checkpoint(&tx, "alpha", 6, 99).is_err());
        assert!(super::set_provider_checkpoint(&tx, "alpha", 8, 1).is_err());
        assert!(super::set_provider_checkpoint(&tx, "alpha", 7, u64::MAX).is_err());
        assert_eq!(
            super::provider_checkpoint(&tx, "alpha", 7).expect("unchanged"),
            10
        );
        assert_eq!(super::streams(&tx, "alpha").expect("empty streams"), None);
        super::put_streams(&tx, "alpha", "{\"revision\":\"r\",\"metadata\":{}}").expect("streams");
        assert!(
            super::streams(&tx, "alpha")
                .expect("stream state")
                .is_some()
        );
        assert_eq!(
            super::allocate_next_generation(&tx, "alpha").expect("first generation"),
            1
        );
        tx.commit().expect("commit");
        assert_eq!(
            super::provider_checkpoint(&db, "alpha", 7).expect("persisted"),
            10
        );
    }

    #[test]
    fn receipt_direct_counter_rollback_and_exhaustion_preserve_other_state() {
        let mut db = Connection::open_in_memory().expect("SQLite");
        crate::receipt_schema::create(&db).expect("target schema");
        db.execute("INSERT INTO runtime_account_state VALUES('alpha',41,7,'connection',10,1,'null',NULL,NULL)", []).expect("migrated counter");
        {
            let tx = db.transaction().expect("transaction");
            assert_eq!(
                super::allocate_next_generation(&tx, "alpha").expect("allocate"),
                42
            );
            tx.rollback().expect("rollback");
        }
        let tx = db.transaction().expect("transaction");
        assert_eq!(
            super::allocate_next_generation(&tx, "alpha").expect("allocate after rollback"),
            42
        );
        assert_eq!(
            super::provider_checkpoint(&tx, "alpha", 7).expect("ACK preserved"),
            10
        );
        tx.execute(
            "UPDATE runtime_account_state SET next_generation=?1 WHERE account='alpha'",
            [i64::MAX],
        )
        .expect("exhausted counter");
        assert!(super::allocate_next_generation(&tx, "alpha").is_err());
        assert_eq!(
            super::provider_checkpoint(&tx, "alpha", 7).expect("ACK preserved"),
            10
        );
    }

    #[test]
    fn receipt_account_observation_restores_without_receipt_table() {
        let db = Connection::open_in_memory().expect("SQLite");
        crate::receipt_schema::create(&db).expect("target schema");
        db.execute_batch(
            "CREATE TABLE sync_entities(collection TEXT,id TEXT,payload TEXT,deleted INTEGER)",
        )
        .expect("entities");
        db.execute(
            "INSERT INTO sync_entities VALUES('rateLimit','alpha',?1,0)",
            [
                json!({"value":{"accountKey":"alpha","at":1.0,"data":{"accountId":"A"}}})
                    .to_string(),
            ],
        )
        .expect("snapshot");
        db.execute(
            "INSERT INTO runtime_account_state VALUES('alpha',7,NULL,NULL,0,0,'null',?1,NULL)",
            [json!({"scope":null,"cursor":0,"at":2.0}).to_string()],
        )
        .expect("observation");
        let restored = limits_snapshots(&db).expect("read current account owner only");
        assert!(restored.diagnostics.is_empty());
        assert_eq!(restored.snapshots[0].1["at"], 2.0);
        assert_eq!(restored.snapshots[0].1["data"]["accountId"], "A");
        assert_eq!(
            super::latest_limits_snapshot(&db, "alpha").expect("latest"),
            Some(restored.snapshots[0].1.clone())
        );
    }

    fn database(payload: &Value) -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory SQLite");
        connection.execute_batch("CREATE TABLE sync_entities(collection TEXT, id TEXT, payload TEXT, deleted INTEGER)").expect("fixture tables");
        crate::receipt_schema::create(&connection).expect("account state");
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
        let alpha = json!({"accountKey":"alpha","at":1.0,"data":{"status":"ready",
            "rateLimits":{"primary":{"usedPercent":12,"providerWindow":{"x":1}}}}
        });
        let db = database(
            &json!({"value":{"rateLimitsByAccount":{"alpha":alpha},"rateLimits":{"accountKey":"default","at":null,"data":{"status":"ready"}}}}),
        );
        let restored = limits_snapshots(&db).expect("read");
        assert!(restored.diagnostics.is_empty());
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
        crate::receipt_schema::create(&db).expect("account state");
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
                "data":{"rateLimits":{"primary":{"usedPercent":"1","placeholder":"x"}}}
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
    fn durable_observation_times_restore_freshness_without_duplicating_provider_data() {
        let db = database(
            &json!({"value":{"rateLimitsByAccount":{"alpha":{"accountKey":"alpha","at":1.0,"readAt":1.0,"data":{"accountId":"A"}}}}}),
        );
        db.execute(
            "INSERT INTO runtime_account_state VALUES('alpha',0,NULL,NULL,0,0,'null',?1,NULL)",
            [json!({"scope":null,"cursor":0,"at":2.0}).to_string()],
        )
        .expect("account observation");
        let restored = limits_snapshots(&db).expect("restore");
        assert!(restored.diagnostics.is_empty());
        assert_eq!(restored.snapshots[0].1["at"], 2.0);
        assert!(restored.snapshots[0].1.get("readAt").is_none());
        assert_eq!(restored.snapshots[0].1["data"]["accountId"], "A");
        db.execute(
            "UPDATE runtime_account_state SET limits_observation=?1",
            [json!({"at":"private-corrupt-timestamp"}).to_string()],
        )
        .expect("invalid metadata");
        let restored = limits_snapshots(&db).expect("isolated diagnostic");
        assert!(restored.snapshots.is_empty());
        assert_eq!(restored.diagnostics[0].account_key, "alpha");
        assert_eq!(restored.diagnostics[0].pointer, "/at");
        assert!(
            !restored.diagnostics[0]
                .keyword
                .contains("private-corrupt-timestamp")
        );
    }

    #[test]
    fn reads_due_authentication_recovery_keys() {
        let db = Connection::open_in_memory().expect("SQLite");
        db.execute_batch("CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL)").expect("table");
        db.execute("INSERT INTO runtime_usage_resumes VALUES('due','agent',?1)", [json!({"status":"scheduled","cause":"auth","accountKey":"default","failedAt":100.0}).to_string()]).expect("row");
        db.execute("INSERT INTO runtime_usage_resumes VALUES('opaque','agent',?1)", [json!({"status":"scheduled","cause":"auth","accountKey":[1],"failedAt":100.0,"epoch":null,"taskClaims":{"old":true}}).to_string()]).expect("historical row");
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
