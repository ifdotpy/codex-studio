//! Atomic client mutation identities. The native host selects the owning store.
use crate::protocol::{Request, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

// Maximum UTF-8 bytes in a local identity group key, before native hashing.
const MAX_IDENTITY_KEY_BYTES: usize = 4096;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Contract {
    monitor_input_path: String,
    uncertain_http_status_floor: u16,
    uncertain_http_statuses: Vec<u16>,
    uncertain_codes: Vec<String>,
    // Maximum UTF-8 bytes before hashing, not a storage quota.
    max_payload_bytes: usize,
}
fn contract() -> &'static Contract {
    static POLICY: std::sync::OnceLock<Contract> = std::sync::OnceLock::new();
    POLICY.get_or_init(|| {
        serde_json::from_str(include_str!("../../../contracts/mutation-journal.json"))
            .expect("valid shared mutation journal contract")
    })
}
pub(crate) fn migration_policy() -> &'static Value {
    static POLICY: std::sync::OnceLock<Value> = std::sync::OnceLock::new();
    POLICY.get_or_init(|| {
        serde_json::from_str(include_str!("../../../contracts/native-migration.json"))
            .expect("valid native migration contract")
    })
}

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct Entry {
    pub request_id: String,
    pub payload_digest: String,
    pub uncertain: bool,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Begin {
    monitor_id: Option<String>,
    path: Option<String>,
    serialized_payload: String,
    explicit_request_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Update {
    monitor_id: Option<String>,
    request_id: String,
    payload_digest: Option<String>,
    status: Option<u16>,
    code: Option<String>,
}

pub(crate) fn digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn sql<T>(result: rusqlite::Result<T>) -> Result<T, String> {
    result.map_err(|e| e.to_string())
}
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128
}
fn valid_digest(digest: &str) -> bool {
    digest.len() == 64
        && digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
fn validate(entry: &Entry) -> Result<(), String> {
    if !valid_id(&entry.request_id) || !valid_digest(&entry.payload_digest) {
        return Err(
            "The saved mutation identity is invalid. Confirm its receipt before another input."
                .into(),
        );
    }
    Ok(())
}
pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_mutation_journal (
        family TEXT NOT NULL, scope TEXT NOT NULL, document TEXT NOT NULL,
        PRIMARY KEY(family,scope));
        CREATE TABLE IF NOT EXISTS client_mutation_identities (
        request_id TEXT PRIMARY KEY, family TEXT NOT NULL, scope TEXT NOT NULL, digest TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_storage_migrations (
        id TEXT PRIMARY KEY, source_sha256 TEXT NOT NULL, receipt TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_storage_cutovers (
        source_identity TEXT NOT NULL, server_id TEXT NOT NULL, inventory TEXT NOT NULL, receipt TEXT NOT NULL,
        PRIMARY KEY(source_identity,server_id));
        CREATE TABLE IF NOT EXISTS client_storage_host_captures (
        source_identity TEXT NOT NULL, server_id TEXT NOT NULL, inventory TEXT NOT NULL, receipt TEXT NOT NULL,
        PRIMARY KEY(source_identity,server_id));
        CREATE TABLE IF NOT EXISTS client_storage_imported_receipts (
        request_id TEXT PRIMARY KEY);",
    )
}
fn read(db: &Connection, family: &str, scope: &str) -> Result<Option<Entry>, String> {
    let raw: Option<String> = sql(db
        .query_row(
            "SELECT document FROM client_mutation_journal WHERE family=?1 AND scope=?2",
            params![family, scope],
            |row| row.get(0),
        )
        .optional())?;
    raw.map(|raw| {
        let entry: Entry =
            serde_json::from_str(&raw).map_err(|_| "The saved mutation journal is invalid.")?;
        validate(&entry)?;
        Ok(entry)
    })
    .transpose()
}
fn write(db: &Connection, family: &str, scope: &str, entry: &Entry) -> Result<(), String> {
    validate(entry)?;
    if matches!(family, "mutation" | "monitor_input") {
        let previous: Option<(String, String, String)> = sql(db
            .query_row(
                "SELECT family,scope,digest FROM client_mutation_identities WHERE request_id=?1",
                [&entry.request_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .optional())?;
        if previous
            .as_ref()
            .is_some_and(|(old_family, old_scope, old_digest)| {
                old_family != family || old_scope != scope || old_digest != &entry.payload_digest
            })
        {
            return Err(
                "The mutation request identity already has different content or scope.".into(),
            );
        }
        sql(db.execute(
            "INSERT OR IGNORE INTO client_mutation_identities VALUES(?1,?2,?3,?4)",
            params![entry.request_id, family, scope, entry.payload_digest],
        ))?;
    }
    sql(db.execute("INSERT INTO client_mutation_journal VALUES(?1,?2,?3) ON CONFLICT(family,scope) DO UPDATE SET document=excluded.document", params![family,scope,serde_json::to_string(entry).unwrap()]))?;
    Ok(())
}
fn uncertain(status: u16, code: Option<&str>) -> bool {
    status >= contract().uncertain_http_status_floor
        || contract().uncertain_http_statuses.contains(&status)
        || code.is_some_and(|code| contract().uncertain_codes.iter().any(|known| known == code))
}
fn begin(db: &Connection, family: &str, p: &Value) -> Result<Value, String> {
    let input: Begin = serde_json::from_value(p.clone()).map_err(|e| e.to_string())?;
    if input.serialized_payload.len() > contract().max_payload_bytes {
        return Err("The mutation payload is too large.".into());
    }
    let payload: Value = serde_json::from_str(&input.serialized_payload)
        .map_err(|_| "A JSON mutation payload is required.")?;
    let path = if family == "monitor_input" {
        if input.path.is_some()
            || input.monitor_id.as_deref().is_none_or(str::is_empty)
            || payload["id"].as_str() != input.monitor_id.as_deref()
        {
            return Err("The monitor input does not match its monitor identity.".into());
        }
        if let Some((path, detail)) = studio_runtime_schema::first_error("MonitorInput", &payload)?
        {
            return Err(format!("Invalid monitor input at {path}: {detail}"));
        }
        contract().monitor_input_path.as_str()
    } else {
        if input.monitor_id.is_some() {
            return Err("Unexpected monitor identity.".into());
        }
        input
            .path
            .as_deref()
            .filter(|p| p.starts_with("/api/") && p.len() <= 4096 && !p.contains(['\n', '\r', '#']))
            .ok_or("Invalid mutation API path.")?
    };
    let payload_digest = digest(format!("{path}\n{}", input.serialized_payload).as_bytes());
    let explicit = input.explicit_request_id.as_deref();
    if explicit.is_some_and(|id| !valid_id(id)) {
        return Err("Invalid explicit mutation identity.".into());
    }
    let mut scope = if family == "monitor_input" {
        input.monitor_id.unwrap()
    } else {
        explicit
            .map(|id| format!("request:{id}"))
            .unwrap_or_else(|| format!("digest:{payload_digest}"))
    };
    // An older owner persisted this operation before explicit IDs became part
    // of the native API. Keep its original scope and uncertainty on recovery.
    if family == "mutation"
        && let Some(explicit) = explicit
    {
        let legacy_scope = format!("digest:{payload_digest}");
        if let Some(saved) = read(db, family, &legacy_scope)? {
            if saved.request_id != explicit || saved.payload_digest != payload_digest {
                return Err("Resolve the pending mutation before sending different input.".into());
            }
            scope = legacy_scope;
        }
    }
    let entry = match read(db, family, &scope)? {
        Some(mut entry) => {
            if entry.payload_digest != payload_digest
                || explicit.is_some_and(|id| id != entry.request_id)
            {
                return Err("Resolve the pending mutation before sending different input.".into());
            }
            entry.uncertain = true;
            entry
        }
        None => Entry {
            request_id: explicit
                .map(str::to_owned)
                .unwrap_or(crate::server_keys::random_id()?),
            payload_digest,
            uncertain: false,
        },
    };
    write(db, family, &scope, &entry)?;
    serde_json::to_value(entry).map_err(|e| e.to_string())
}

fn local_write_identity(db: &Connection, operation: &str, params: &Value) -> Result<Value, String> {
    let p = params
        .as_object()
        .filter(|p| p.len() == 2)
        .ok_or("Invalid local write identity parameters.")?;
    let key = p
        .get("key")
        .and_then(Value::as_str)
        .filter(|key| !key.is_empty() && key.len() <= MAX_IDENTITY_KEY_BYTES)
        .ok_or("A bounded local write identity key is required.")?;
    let scope = digest(key.as_bytes());
    if operation == "clear_write_id" {
        let id = p
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .ok_or("An exact local write identity is required.")?;
        let matches = read(db, "write_id", &scope)?.is_some_and(|old| old.request_id == id);
        if matches {
            sql(db.execute(
                "DELETE FROM client_mutation_journal WHERE family='write_id' AND scope=?1",
                [scope],
            ))?;
        }
        return Ok(json!(matches));
    }
    let serialized = p
        .get("serializedPayload")
        .and_then(Value::as_str)
        .filter(|raw| raw.len() <= contract().max_payload_bytes)
        .ok_or("A bounded local write payload is required.")?;
    let payload: Value =
        serde_json::from_str(serialized).map_err(|_| "A JSON local write payload is required.")?;
    let envelope = payload
        .as_object()
        .filter(|p| p.len() == 2 && p.get("params").is_some_and(Value::is_object))
        .ok_or("A local write method and object parameters are required.")?;
    if !matches!(
        envelope.get("method").and_then(Value::as_str),
        Some(
            "outbox.enqueue"
                | "outbox.change"
                | "outbox.display"
                | "outbox.acknowledge"
                | "outbox.acknowledge_many"
                | "outbox.stop_removed"
        )
    ) {
        return Err("The write identity is only for durable local outbox commands.".into());
    }
    let payload_digest = digest(serialized.as_bytes());
    if let Some(old) = read(db, "write_id", &scope)?
        && old.payload_digest == payload_digest
    {
        return Ok(json!(old.request_id));
    }
    let id = crate::server_keys::random_id()?;
    write(
        db,
        "write_id",
        &scope,
        &Entry {
            request_id: id.clone(),
            payload_digest,
            uncertain: false,
        },
    )?;
    Ok(json!(id))
}

/// Use the existing receipt owner inside a caller's atomic storage transaction.
pub(crate) fn apply_in_transaction(db: &Connection, request: &Request) -> Result<Value, String> {
    apply(db, request)
}

fn apply(db: &Connection, request: &Request) -> Result<Value, String> {
    let (family, operation) = request
        .method
        .split_once('.')
        .ok_or("Invalid mutation command.")?;
    if family == "mutation" && matches!(operation, "write_id" | "clear_write_id") {
        return local_write_identity(db, operation, &request.params);
    }
    if family == "mutation" && matches!(operation, "stable_id" | "clear_stable_id") {
        let p = request
            .params
            .as_object()
            .filter(|p| p.len() == if operation == "stable_id" { 1 } else { 2 })
            .ok_or("Invalid stable identity parameters.")?;
        let key = p
            .get("key")
            .and_then(Value::as_str)
            .filter(|key| !key.is_empty() && key.len() <= MAX_IDENTITY_KEY_BYTES)
            .ok_or("A bounded stable identity key is required.")?;
        let hash = digest(key.as_bytes());
        let old = read(db, "stable_id", &hash)?;
        if operation == "stable_id" {
            if let Some(old) = old {
                return Ok(json!(old.request_id));
            }
            let id = crate::server_keys::random_id()?;
            write(
                db,
                "stable_id",
                &hash,
                &Entry {
                    request_id: id.clone(),
                    payload_digest: hash.clone(),
                    uncertain: false,
                },
            )?;
            return Ok(json!(id));
        }
        let id = p
            .get("requestId")
            .and_then(Value::as_str)
            .filter(|id| valid_id(id))
            .ok_or("An exact stable request identity is required.")?;
        let matches = old.is_some_and(|old| old.request_id == id);
        if matches {
            sql(db.execute(
                "DELETE FROM client_mutation_journal WHERE family='stable_id' AND scope=?1",
                [hash],
            ))?;
        }
        return Ok(json!(matches));
    }
    if operation == "begin" {
        return begin(db, family, &request.params);
    }
    if family == "monitor_input" && operation == "pending" {
        let p = request
            .params
            .as_object()
            .filter(|p| p.len() == 1)
            .ok_or("A monitor identity is required.")?;
        let id = p
            .get("monitorId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or("Invalid monitor identity.")?;
        return serde_json::to_value(read(db, family, id)?).map_err(|e| e.to_string());
    }
    let p: Update = serde_json::from_value(request.params.clone()).map_err(|e| e.to_string())?;
    if !valid_id(&p.request_id)
        || p.payload_digest
            .as_deref()
            .is_some_and(|d| !valid_digest(d))
    {
        return Err("Invalid mutation receipt identity.".into());
    }
    if operation != "reject" && (p.status.is_some() || p.code.is_some()) {
        return Err("Unexpected rejection data.".into());
    }
    if operation != "settle" && p.payload_digest.is_none() {
        return Err("A mutation digest is required.".into());
    }
    let scope = if family == "monitor_input" {
        p.monitor_id
            .as_deref()
            .filter(|id| !id.is_empty())
            .ok_or("Invalid monitor identity.")?
            .to_owned()
    } else {
        if p.monitor_id.is_some() || p.payload_digest.is_none() {
            return Err("Invalid mutation scope.".into());
        }
        let explicit = format!("request:{}", p.request_id);
        if read(db, family, &explicit)?.is_some() {
            explicit
        } else {
            format!("digest:{}", p.payload_digest.as_deref().unwrap())
        }
    };
    let Some(mut entry) = read(db, family, &scope)? else {
        return Ok(json!(false));
    };
    if entry.request_id != p.request_id
        || p.payload_digest
            .as_deref()
            .is_some_and(|d| d != entry.payload_digest)
    {
        return Ok(json!(false));
    }
    match operation {
        "settle" => {
            sql(db.execute(
                "DELETE FROM client_mutation_journal WHERE family=?1 AND scope=?2",
                params![family, scope],
            ))?;
        }
        "unknown" => {
            entry.uncertain = true;
            write(db, family, &scope, &entry)?;
        }
        "reject" => {
            let status = p
                .status
                .filter(|s| (100..=599).contains(s))
                .ok_or("A valid HTTP status is required.")?;
            if uncertain(status, p.code.as_deref()) {
                entry.uncertain = true;
                write(db, family, &scope, &entry)?;
            } else if !entry.uncertain {
                sql(db.execute(
                    "DELETE FROM client_mutation_journal WHERE family=?1 AND scope=?2",
                    params![family, scope],
                ))?;
            }
        }
        _ => return Err("Unknown mutation journal command.".into()),
    }
    Ok(json!(true))
}
pub(crate) fn stored(db: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Response, String> {
        let tx = sql(db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate))?;
        // A receipt retains only the content digest, never the input plaintext.
        let mut fingerprint = Request {
            id: request.id.clone(),
            method: request.method.clone(),
            params: request.params.clone(),
        };
        if let Some(raw) = fingerprint.params.get_mut("serializedPayload") {
            let source = raw.as_str().ok_or("Invalid serialized mutation payload.")?;
            *raw = json!(digest(source.as_bytes()));
        }
        if let Some(receipt) = sql(crate::store::replay(&tx, &fingerprint))? {
            return Ok(receipt);
        }
        let value = apply(&tx, request)?;
        let response = Response::success(request.id.clone(), value);
        sql(crate::store::record(&tx, &fingerprint, &response))?;
        sql(tx.commit())?;
        Ok(response)
    })();
    result.unwrap_or_else(|e| Response::failure(request.id.clone(), "mutation_journal", e))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Migration {
    version: u8,
    archive_manifest: Option<ArchiveManifest>,
    source_identity: Option<String>,
    #[serde(default)]
    expected_byte_imports: Vec<String>,
    #[serde(default)]
    preferences: BTreeMap<String, String>,
    #[serde(default)]
    preference_receipts: Vec<SavedReceipt>,
    legacy_preference_scope: Option<String>,
    #[serde(default)]
    pending_preferences: Vec<SavedWrite>,
    #[serde(default)]
    pending_drafts: Vec<DraftEdit>,
    #[serde(default)]
    drafts: Vec<DraftImport>,
    #[serde(default)]
    draft_documents: Vec<DraftDocuments>,
    #[serde(default)]
    monitor_inputs: Vec<MonitorImport>,
    #[serde(default)]
    mutations: Vec<Entry>,
    #[serde(default)]
    outbox: Vec<crate::outbox::Entry>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchiveManifest {
    version: u8,
    archive: ArchiveIdentity,
    excluded_caches: Vec<ExcludedCache>,
    #[serde(skip_serializing_if = "Option::is_none")]
    legacy_adoption: Option<LegacyArchiveAdoption>,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct LegacyArchiveAdoption {
    marker: String,
    combined_snapshot_sha256: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ArchiveIdentity {
    id: String,
    owner: String,
    sha256: String,
    source_identity: String,
}
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ExcludedCache {
    database: String,
    store: String,
    records: u64,
    sha256: String,
}
impl ArchiveManifest {
    fn validate(&self) -> Result<(), String> {
        if self.version != 1
            || self.archive.id.is_empty()
            || self.archive.id.len() > 512
            || self.archive.owner.is_empty()
            || self.archive.owner.len() > 512
            || !valid_digest(&self.archive.sha256)
            || !valid_digest(&self.archive.source_identity)
            || self.excluded_caches.len() > 10_000
            || self.excluded_caches.iter().any(|cache| {
                cache.database.is_empty()
                    || cache.database.len() > 512
                    || cache.store.is_empty()
                    || cache.store.len() > 512
                    || cache.records > studio_client_domain::MAX_SAFE_INTEGER as u64
                    || !valid_digest(&cache.sha256)
            })
        {
            return Err("The sealed archive migration manifest is invalid.".into());
        }
        if self.legacy_adoption.as_ref().is_some_and(|adoption| {
            !valid_digest(&adoption.combined_snapshot_sha256)
                || adoption.marker
                    != format!(
                        "{}:{}:complete",
                        self.archive.id, adoption.combined_snapshot_sha256
                    )
        }) {
            return Err("The prior browser archive import proof is invalid.".into());
        }
        Ok(())
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedWrite {
    id: String,
    method: String,
    params: Value,
}

/// Build the exact requests used by the original Storage adapter without
/// opening a transaction, changing a receipt, or starting an effect.
pub(crate) fn prepare_original_preferences(
    scope: &str,
    writes: &Value,
) -> Result<Vec<Request>, String> {
    const MAX_LEGACY_PREFERENCE_REQUESTS: usize = 10_000;
    if scope != "root" && !scope.strip_prefix("server:").is_some_and(valid_id) {
        return Err("Invalid legacy preference owner.".into());
    }
    let writes: Vec<SavedWrite> =
        serde_json::from_value(writes.clone()).map_err(|error| error.to_string())?;
    if writes.len() > MAX_LEGACY_PREFERENCE_REQUESTS {
        return Err("Too many pending legacy preference writes.".into());
    }
    let mut requests = Vec::new();
    let mut identities = BTreeMap::new();
    for saved in writes {
        if !valid_id(&saved.id) {
            return Err("Invalid legacy preference request identity.".into());
        }
        let logical = logical_preference_params(&saved.method, &saved.params, None)?;
        let params = if saved.method == "preferences.import" {
            let entries: serde_json::Map<String, Value> = logical["entries"]
                .as_object()
                .unwrap()
                .iter()
                .map(|(key, value)| (format!("{}{key}", prefix(scope)), value.clone()))
                .collect();
            json!({"version":1,"entries":entries})
        } else {
            let mut params = logical;
            params["key"] = json!(format!(
                "{}{}",
                prefix(scope),
                params["key"].as_str().unwrap()
            ));
            params
        };
        let identity = json!({"method":saved.method,"params":params});
        if identities
            .get(&saved.id)
            .is_some_and(|old| old != &identity)
        {
            return Err("The original preference request identity has conflicting content.".into());
        }
        identities.insert(saved.id.clone(), identity);
        requests.push(Request {
            id: saved.id,
            method: saved.method,
            params,
        });
    }
    Ok(requests)
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SavedReceipt {
    id: String,
    method: String,
    params: Value,
    response: Value,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftEdit {
    workspace: String,
    id: String,
    changes: BTreeMap<String, String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftImport {
    workspace: String,
    versions: Value,
    #[serde(rename = "serializedParams")]
    serialized_params: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct DraftDocuments {
    workspace: String,
    documents: Vec<crate::projections::Document>,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MonitorImport {
    monitor_id: String,
    request_id: String,
    payload_digest: String,
    uncertain: bool,
}

fn prefix(scope: &str) -> String {
    format!("browser:{}:", json!(scope))
}

fn legacy_owners(db: &Connection) -> Result<Value, String> {
    fn add(owners: &mut std::collections::BTreeSet<String>, key: &str) -> Result<(), String> {
        let Some(source) = key.strip_prefix("browser:") else {
            return Ok(());
        };
        let mut decoder = serde_json::Deserializer::from_str(source).into_iter::<String>();
        let mut owner = decoder
            .next()
            .ok_or("The original native preference namespace is incomplete.")?
            .map_err(|_| "The original native preference namespace is invalid.")?;
        let logical = source
            .get(decoder.byte_offset()..)
            .and_then(|tail| tail.strip_prefix(':'))
            .ok_or("The original native preference namespace has no key.")?;
        if owner == "root" {
            if let Some(remote) = logical.strip_prefix(":server:") {
                let (server, _) = remote
                    .split_once(':')
                    .ok_or("The original native server preference scope is incomplete.")?;
                owner = format!("server:{server}");
            }
        } else if !owner.starts_with("server:") {
            return Err(
                "The original native preference namespace is unknown. Its source remains held."
                    .into(),
            );
        }
        if let Some(server) = owner.strip_prefix("server:") {
            if server.is_empty()
                || server.len() > 128
                || !server
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            {
                return Err("The original native server preference scope is invalid.".into());
            }
        }
        owners.insert(owner);
        Ok(())
    }
    let mut owners = std::collections::BTreeSet::from(["root".to_owned()]);
    let mut keys = sql(db.prepare(
        "SELECT key FROM client_preferences WHERE substr(key,1,length(?1))=?1 ORDER BY key",
    ))?;
    for key in sql(keys.query_map(["browser:"], |row| row.get::<_, String>(0)))? {
        add(&mut owners, &sql(key)?)?;
    }
    for (_, method, raw, _) in original_preference_receipts(db)? {
        let params: Value = serde_json::from_str(&raw)
            .map_err(|_| "The original native preference receipt is invalid.")?;
        if method == "preferences.import" {
            for key in params["entries"]
                .as_object()
                .ok_or("The original native preference receipt has no entries.")?
                .keys()
            {
                add(&mut owners, key)?;
            }
        } else {
            add(
                &mut owners,
                params["key"]
                    .as_str()
                    .ok_or("The original native preference receipt has no key.")?,
            )?;
        }
    }
    Ok(json!(owners))
}
fn legacy_key<'a>(scope: &str, key: &'a str) -> Option<&'a str> {
    let logical = key.strip_prefix(&prefix(scope)).or_else(|| {
        scope
            .strip_prefix("server:")
            .and_then(|id| key.strip_prefix(&format!("{}:server:{id}:", prefix("root"))))
    })?;
    if scope == "root" && logical.starts_with(":server:") {
        None
    } else {
        Some(logical)
    }
}
fn logical_preference_params(
    method: &str,
    params: &Value,
    scope: Option<&str>,
) -> Result<Value, String> {
    let key = |key: &str| -> Result<String, String> {
        let logical = match scope {
            Some(scope) => {
                legacy_key(scope, key).ok_or("The legacy preference belongs to another owner.")?
            }
            None => key,
        };
        if !crate::store::valid_raw_preference(logical, "") {
            return Err("Invalid saved preference key.".into());
        }
        Ok(logical.to_owned())
    };
    if method == "preferences.import" {
        let entries = params["entries"]
            .as_object()
            .ok_or("Invalid saved preference snapshot.")?;
        if params["version"] != 1 || params.as_object().is_none_or(|p| p.len() != 2) {
            return Err("Invalid saved preference snapshot.".into());
        }
        let mut mapped = serde_json::Map::new();
        for (name, raw) in entries {
            let logical = key(name)?;
            let raw = raw
                .as_str()
                .filter(|raw| crate::store::valid_raw_preference(&logical, raw))
                .ok_or("Invalid saved preference value.")?;
            mapped.insert(logical, json!(raw));
        }
        return Ok(json!({"version":1,"entries":mapped}));
    }
    if !matches!(method, "preferences.raw_set" | "preferences.raw_remove") {
        return Err("Only saved preference writes can be imported as preference receipts.".into());
    }
    let logical = key(params["key"]
        .as_str()
        .ok_or("Missing saved preference key.")?)?;
    if params.as_object().is_none_or(|p| {
        p.len()
            != if method == "preferences.raw_set" {
                2
            } else {
                1
            }
    }) {
        return Err("Invalid saved preference parameters.".into());
    }
    if method == "preferences.raw_set" {
        let raw = params["raw"]
            .as_str()
            .filter(|raw| crate::store::valid_raw_preference(&logical, raw))
            .ok_or("Invalid saved preference value.")?;
        Ok(json!({"key":logical,"raw":raw}))
    } else {
        Ok(json!({"key":logical}))
    }
}
fn apply_preference(db: &Connection, method: &str, p: &Value) -> Result<usize, String> {
    let mut changed = 0;
    if method == "preferences.import" {
        for (key, raw) in p["entries"].as_object().unwrap() {
            changed += import_preference(db, key, raw.as_str().unwrap())?;
        }
    } else if method == "preferences.raw_remove" {
        changed = sql(db.execute(
            "DELETE FROM client_preferences WHERE key=?1",
            [p["key"].as_str().unwrap()],
        ))?;
    } else {
        changed=sql(db.execute("INSERT INTO client_preferences VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![p["key"].as_str().unwrap(),p["raw"].as_str().unwrap()]))?;
    }
    Ok(changed)
}
fn import_preference(db: &Connection, key: &str, raw: &str) -> Result<usize, String> {
    if !crate::store::valid_raw_preference(key, raw) {
        return Err("Invalid imported preference.".into());
    }
    let old: Option<String> = sql(db
        .query_row(
            "SELECT value FROM client_preferences WHERE key=?1",
            [key],
            |r| r.get(0),
        )
        .optional())?;
    if old.as_deref().is_some_and(|old| old != raw) {
        return Err(
            "The imported preference conflicts with native data. Both sources were preserved."
                .into(),
        );
    }
    sql(db.execute(
        "INSERT OR IGNORE INTO client_preferences VALUES(?1,?2)",
        params![key, raw],
    ))
}
fn workspace(db: &Connection, workspace: &str) -> Result<(), String> {
    if !crate::projections::valid_workspace(workspace) {
        return Err("Invalid imported workspace.".into());
    }
    let identity: Option<String> = sql(db
        .query_row(
            "SELECT value FROM client_identity WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional())?;
    let identity: Value = serde_json::from_str(
        identity
            .as_deref()
            .ok_or("Verify the native workspace before importing drafts or messages.")?,
    )
    .map_err(|e| e.to_string())?;
    if identity["workspaceId"] != workspace {
        return Err("The imported data belongs to another workspace.".into());
    }
    Ok(())
}
fn import_entry(db: &Connection, family: &str, scope: &str, entry: &Entry) -> Result<(), String> {
    validate(entry)?;
    if let Some(old) = read(db, family, scope)? {
        if old != *entry {
            return Err(
                "The imported mutation conflicts with native data. Both sources were preserved."
                    .into(),
            );
        }
    } else {
        write(db, family, scope, entry)?;
    }
    Ok(())
}
fn import_source(db: &Connection, source: Migration) -> Result<(), String> {
    if source.version != 1 {
        return Err("Unsupported client storage migration version.".into());
    }
    for (key, raw) in source.preferences {
        import_preference(db, &key, &raw)?;
    }
    for saved in source.preference_receipts {
        if !valid_id(&saved.id) {
            return Err("Invalid saved preference request identity.".into());
        }
        logical_preference_params(
            &saved.method,
            &saved.params,
            source.legacy_preference_scope.as_deref(),
        )?;
        let request = Request {
            id: saved.id.clone(),
            method: saved.method,
            params: saved.params,
        };
        let response: Response =
            serde_json::from_value(saved.response).map_err(|e| e.to_string())?;
        if response.id != saved.id || response.result.is_some() == response.error.is_some() {
            return Err("Invalid saved preference receipt.".into());
        }
        if let Some(existing) = sql(crate::store::replay(db, &request))? {
            if serde_json::to_value(existing).unwrap() != serde_json::to_value(&response).unwrap() {
                return Err("The saved preference receipt conflicts with native data.".into());
            }
        } else {
            sql(crate::store::record(db, &request, &response))?;
            sql(db.execute(
                "INSERT INTO client_storage_imported_receipts VALUES(?1)",
                [&request.id],
            ))?;
        }
    }
    for saved in source.pending_preferences {
        if !valid_id(&saved.id) {
            return Err("Invalid pending preference identity.".into());
        }
        let logical = logical_preference_params(
            &saved.method,
            &saved.params,
            source.legacy_preference_scope.as_deref(),
        )?;
        let request = Request {
            id: saved.id,
            method: saved.method,
            params: saved.params,
        };
        if let Some(receipt) = sql(crate::store::replay(db, &request))? {
            if receipt.error.is_some() {
                return Err(
                    "A pending preference has a rejected or conflicting native receipt.".into(),
                );
            }
        } else {
            apply_preference(db, &request.method, &logical)?;
            sql(crate::store::record(
                db,
                &request,
                &Response::success(request.id.clone(), json!({"saved":true})),
            ))?;
            sql(db.execute(
                "INSERT INTO client_storage_imported_receipts VALUES(?1)",
                [&request.id],
            ))?;
        }
    }
    // Older native adapters saved generic request identities as preferences.
    // Read the post-replay SQLite values, not the stale WebView cache.
    let legacy_writes = {
        let prefix = migration_policy()["legacyMutationStoragePrefix"]
            .as_str()
            .expect("legacy mutation storage prefix owner");
        let mut q=sql(db.prepare("SELECT key,value FROM client_preferences WHERE substr(key,1,length(?1))=?1 OR key LIKE 'request-id:%' ORDER BY key"))?;
        sql(q.query_map([prefix], |r| {
            Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?
    };
    for (key, raw) in legacy_writes {
        if let Some(key) = key.strip_prefix("request-id:") {
            let value: Value =
                serde_json::from_str(&raw).map_err(|_| "Invalid saved stable request identity.")?;
            if value.is_null() {
                continue;
            }
            let id = value
                .as_str()
                .filter(|id| valid_id(id))
                .ok_or("Invalid saved stable request identity.")?;
            let hash = digest(key.as_bytes());
            import_entry(
                db,
                "stable_id",
                &hash,
                &Entry {
                    request_id: id.to_owned(),
                    payload_digest: hash.clone(),
                    uncertain: false,
                },
            )?;
            continue;
        }
        let payload_digest = key
            .strip_prefix(
                migration_policy()["legacyMutationStoragePrefix"]
                    .as_str()
                    .unwrap(),
            )
            .ok_or("Invalid legacy mutation storage key.")?
            .to_owned();
        let entry = match serde_json::from_str::<Value>(&raw) {
            Ok(value) => Entry {
                request_id: value["requestId"]
                    .as_str()
                    .ok_or("Invalid legacy mutation identity.")?
                    .to_owned(),
                payload_digest,
                uncertain: value["uncertain"]
                    .as_bool()
                    .ok_or("Invalid legacy mutation uncertainty.")?,
            },
            Err(_) => Entry {
                request_id: raw,
                payload_digest,
                uncertain: true,
            },
        };
        import_entry(
            db,
            "mutation",
            &format!("digest:{}", entry.payload_digest),
            &entry,
        )?;
    }
    for saved in source.draft_documents {
        workspace(db, &saved.workspace)?;
        crate::draft_store::import_documents(db, &saved.workspace, &saved.documents)?;
    }
    for saved in source.drafts {
        workspace(db, &saved.workspace)?;
        let params: Value =
            serde_json::from_str(&saved.serialized_params).map_err(|e| e.to_string())?;
        if params != json!({"workspace":saved.workspace,"versions":saved.versions}) {
            return Err("The original browser draft bytes do not match their import.".into());
        }
        let request = Request {
            id: format!(
                "browser-drafts:{}",
                digest(saved.serialized_params.as_bytes())
            ),
            method: "drafts.import".into(),
            params,
        };
        if let Some(receipt) = sql(crate::store::replay(db, &request))? {
            if receipt.error.is_some() {
                return Err("The original browser draft import has a conflicting receipt.".into());
            }
        } else {
            let view = crate::draft_store::apply_migration(db, &request)?;
            sql(crate::store::record(
                db,
                &request,
                &Response::success(request.id.clone(), view),
            ))?;
        }
    }
    for saved in source.pending_drafts {
        workspace(db, &saved.workspace)?;
        if !valid_id(&saved.id) {
            return Err("Invalid pending draft identity.".into());
        }
        let request = Request {
            id: saved.id,
            method: "drafts.edit".into(),
            params: json!({"workspace":saved.workspace,"changes":saved.changes}),
        };
        if let Some(receipt) = sql(crate::store::replay(db, &request))? {
            if receipt.error.is_some() {
                return Err("A pending draft has a rejected or conflicting native receipt.".into());
            }
        } else {
            let view = crate::draft_store::apply_migration(db, &request)?;
            sql(crate::store::record(
                db,
                &request,
                &Response::success(request.id.clone(), view),
            ))?;
        }
    }
    for saved in source.monitor_inputs {
        if saved.monitor_id.is_empty() {
            return Err("Invalid imported monitor identity.".into());
        }
        import_entry(
            db,
            "monitor_input",
            &saved.monitor_id,
            &Entry {
                request_id: saved.request_id,
                payload_digest: saved.payload_digest,
                uncertain: saved.uncertain,
            },
        )?;
    }
    for saved in source.mutations {
        import_entry(
            db,
            "mutation",
            &format!("digest:{}", saved.payload_digest),
            &saved,
        )?;
    }
    // Preferences and their pending writes settle before this read. A removed
    // card must stay held even if its queue entry has not been inserted yet.
    let removal_fences = {
        let prefix = studio_runtime_schema::shared_values::REMOVED_SEND_STORAGE_PREFIX;
        let mut statement = sql(db.prepare(
            "SELECT key,value FROM client_preferences WHERE substr(key,1,length(?1))=?1 ORDER BY key",
        ))?;
        sql(statement.query_map([prefix], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|error| error.to_string())?
    };
    for (key, raw) in removal_fences {
        let suffix = key
            .strip_prefix(studio_runtime_schema::shared_values::REMOVED_SEND_STORAGE_PREFIX)
            .ok_or("Invalid imported removal fence.")?;
        let [owner, id]: [String; 2] =
            serde_json::from_str(suffix).map_err(|_| "Invalid imported removal fence identity.")?;
        if raw != "1" {
            return Err("Invalid imported removal fence value.".into());
        }
        workspace(db, &owner)?;
        crate::outbox::import_removal_fence(db, &owner, &id)?;
    }
    for saved in source.outbox {
        workspace(db, &saved.workspace)?;
        crate::outbox::import_entry(db, &saved)?;
    }
    Ok(())
}

// Host-only export. The Tauri service allowlist must not expose this operation
// to a server view. Original receipt parameters retain their original scope.
type OriginalPreferenceReceipt = (String, String, String, String);
fn original_preference_receipts(db: &Connection) -> Result<Vec<OriginalPreferenceReceipt>, String> {
    let has_provenance:bool=sql(db.query_row("SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='client_storage_imported_receipts')",[],|row|row.get(0)))?;
    let provenance = if has_provenance {
        " AND NOT EXISTS(SELECT 1 FROM client_storage_imported_receipts AS imported WHERE imported.request_id=r.request_id)"
    } else {
        ""
    };
    let mut query=sql(db.prepare(&format!("SELECT r.request_id,r.method,r.params,r.response FROM client_receipts AS r WHERE r.method IN ('preferences.raw_set','preferences.raw_remove','preferences.import'){provenance} ORDER BY r.rowid")))?;
    sql(query.query_map([], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    }))?
    .map(sql)
    .collect()
}

fn export_preferences(db: &Connection, scope: &str) -> Result<Value, String> {
    let mut q = sql(db.prepare("SELECT key,value FROM client_preferences ORDER BY key"))?;
    let mut preferences = BTreeMap::new();
    for row in sql(q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))? {
        let (key, value) = sql(row)?;
        if let Some(key) = legacy_key(scope, &key) {
            if preferences.get(key).is_some_and(|old| old != &value) {
                return Err(
                    "The legacy preference namespaces conflict. Both sources were preserved."
                        .into(),
                );
            }
            preferences.insert(key.to_owned(), value);
        }
    }
    let mut receipts = Vec::new();
    for (id, method, p, response) in original_preference_receipts(db)? {
        let p: Value = serde_json::from_str(&p).map_err(|e| e.to_string())?;
        if logical_preference_params(&method, &p, Some(scope)).is_ok() {
            receipts.push(json!({"id":id,"method":method,"params":p,"response":serde_json::from_str::<Value>(&response).map_err(|e|e.to_string())?}));
        }
    }
    Ok(
        json!({"version":1,"preferences":preferences,"preferenceReceipts":receipts,"legacyPreferenceScope":scope}),
    )
}

pub(crate) fn native_legacy_snapshot(db: &Connection) -> Result<Value, String> {
    let mut query = sql(db.prepare(
        "SELECT workspace,room,text,revision FROM client_drafts ORDER BY workspace,room",
    ))?;
    let drafts=sql(query.query_map([],|row|Ok(json!({"workspace":row.get::<_,String>(0)?,"room":row.get::<_,String>(1)?,"text":row.get::<_,String>(2)?,"revision":crate::store::unsigned(row,3)?}))))?.map(sql).collect::<Result<Vec<_>,_>>()?;
    let mut query=sql(db.prepare("SELECT request_id,method,params,response FROM client_receipts WHERE method='draft.set' ORDER BY request_id"))?;
    let mut receipts = Vec::new();
    for row in sql(query.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    }))? {
        let (id, method, params, response) = sql(row)?;
        receipts.push(json!({"id":id,"method":method,"serializedParams":params,"serializedResponse":response}));
    }
    let source_sha256 = digest(
        json!({"drafts":drafts,"receipts":receipts})
            .to_string()
            .as_bytes(),
    );
    let mut preference_receipts = Vec::new();
    let mut original_receipts = original_preference_receipts(db)?;
    original_receipts.sort_by(|a, b| a.0.cmp(&b.0));
    for (id, method, raw_params, raw_response) in original_receipts {
        let params: Value = serde_json::from_str(&raw_params)
            .map_err(|_| "The original native receipt parameters are invalid.")?;
        let original = if method == "preferences.import" {
            params["entries"]
                .as_object()
                .is_some_and(|entries| entries.keys().any(|key| key.starts_with("browser:")))
        } else {
            params["key"]
                .as_str()
                .is_some_and(|key| key.starts_with("browser:"))
        };
        if original {
            preference_receipts.push(json!({"id":id,"method":method,"params":params,"response":serde_json::from_str::<Value>(&raw_response).map_err(|_|"The original native receipt response is invalid.")?}));
        }
    }
    Ok(
        json!({"version":1,"drafts":drafts,"receiptIds":receipts.iter().map(|receipt|receipt["id"].clone()).collect::<Vec<_>>(),"sourceSha256":source_sha256,"preferenceReceipts":preference_receipts}),
    )
}

pub(crate) fn original_native_source(db: &Connection) -> Result<Value, String> {
    let owners = legacy_owners(db)?;
    let mut preferences = serde_json::Map::new();
    for owner in owners
        .as_array()
        .ok_or("The original native owner inventory is invalid.")?
    {
        let owner = owner
            .as_str()
            .ok_or("The original native owner identity is invalid.")?;
        preferences.insert(owner.into(), export_preferences(db, owner)?);
    }
    let has_identity: bool = sql(db.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='table' AND name='client_identity')",
        [],
        |row| row.get(0),
    ))?;
    let identity: Option<String> = if has_identity {
        sql(db
            .query_row(
                "SELECT value FROM client_identity WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional())?
    } else {
        None
    };
    let mut query = sql(db.prepare(
        "SELECT request_id,method,params,response FROM client_receipts ORDER BY request_id",
    ))?;
    let receipts = sql(query.query_map([],|row|Ok(json!({"id":row.get::<_,String>(0)?,"method":row.get::<_,String>(1)?,"serializedParams":row.get::<_,String>(2)?,"serializedResponse":row.get::<_,String>(3)?}))))?.map(sql).collect::<Result<Vec<_>,_>>()?;
    Ok(
        json!({"owners":owners,"preferences":preferences,"nativeLegacy":native_legacy_snapshot(db)?,"receipts":receipts,"identity":identity.map(|raw|serde_json::from_str::<Value>(&raw).map_err(|_|"The original native workspace identity is invalid.")).transpose()?}),
    )
}

fn import_native_legacy(db: &mut Connection, parameters: &Value) -> Result<Value, String> {
    const NATIVE_LEGACY_MANIFEST_PREFIX: &str = "native-legacy:v1:";
    let parameters = parameters
        .as_object()
        .filter(|parameters| parameters.len() == 2)
        .ok_or("The native legacy import accepts only its source identity and expected digest.")?;
    let source = parameters
        .get("sourceIdentity")
        .and_then(Value::as_str)
        .filter(|source| valid_digest(source))
        .ok_or("Invalid native legacy source identity.")?;
    let expected = parameters
        .get("expectedSourceSha256")
        .and_then(Value::as_str)
        .filter(|value| valid_digest(value))
        .ok_or("Invalid native legacy source digest.")?;
    let tx = sql(db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate))?;
    let snapshot = native_legacy_snapshot(&tx)?;
    if snapshot["sourceSha256"] != expected {
        return Err("The original native legacy source changed. Its rows remain preserved.".into());
    }
    let manifest = format!("{NATIVE_LEGACY_MANIFEST_PREFIX}{source}");
    let previous: Option<(String, String)> = sql(tx
        .query_row(
            "SELECT source_sha256,receipt FROM client_storage_migrations WHERE id=?1",
            [&manifest],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional())?;
    if let Some((saved, receipt)) = previous {
        if saved != expected {
            return Err("The native legacy import baseline changed. Its original receipt remains preserved.".into());
        }
        return serde_json::from_str(&receipt).map_err(|error| error.to_string());
    }
    let policy = migration_policy();
    let prefix = policy["nativeDraftIdPrefix"]
        .as_str()
        .ok_or("The native draft identity contract is missing.")?;
    let device = policy["nativeDraftDevice"]
        .as_str()
        .ok_or("The native draft device contract is missing.")?;
    if prefix != format!("{device}:") {
        return Err("The native draft prefix does not match its device identity contract.".into());
    }
    let mut workspaces = BTreeMap::<String, Vec<crate::projections::Document>>::new();
    for draft in snapshot["drafts"]
        .as_array()
        .ok_or("The original native draft inventory is incomplete.")?
    {
        let workspace = draft["workspace"]
            .as_str()
            .filter(|workspace| crate::projections::valid_workspace(workspace))
            .ok_or("The original native draft workspace is invalid.")?;
        let room = draft["room"]
            .as_str()
            .ok_or("The original native draft session is invalid.")?;
        let text = draft["text"]
            .as_str()
            .ok_or("The original native draft text is invalid.")?;
        let revision = draft["revision"]
            .as_u64()
            .ok_or("The original native draft revision is invalid.")?;
        let id = format!(
            "{prefix}{}:{room}",
            digest(
                serde_json::to_vec(&(workspace, room))
                    .map_err(|error| error.to_string())?
                    .as_slice()
            )
        );
        let payload=json!({"id":id,"session":room,"device":device,"text":text,"updated":0,"nativeLegacyRevision":revision.to_string()}).to_string();
        workspaces
            .entry(workspace.into())
            .or_default()
            .push(crate::projections::Document {
                id,
                seq: 0,
                deleted: false,
                payload,
            });
    }
    for (workspace, documents) in workspaces {
        crate::draft_store::import_documents(&tx, &workspace, &documents)?;
    }
    let receipt = json!({"manifestId":manifest,"sourceSha256":expected,"sourceIdentity":source,"expectedByteImports":[],"nativeLegacyReceiptIds":snapshot["receiptIds"],"imported":true});
    sql(tx.execute(
        "INSERT INTO client_storage_migrations VALUES(?1,?2,?3)",
        params![manifest, expected, receipt.to_string()],
    ))?;
    sql(tx.commit())?;
    Ok(receipt)
}

pub(crate) fn migration(db: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Value, String> {
        if request.method == migration_policy()["admitMethod"].as_str().unwrap() {
            let parameters = request
                .params
                .as_object()
                .filter(|parameters| parameters.len() == 1)
                .ok_or("The native runtime admission accepts only its exact saved host receipt.")?;
            let receipt = parameters
                .get("receipt")
                .ok_or("The native host receipt is missing.")?;
            let source = receipt["sourceIdentity"]
                .as_str()
                .filter(|source| valid_digest(source))
                .ok_or("Invalid native admission source.")?;
            let server = receipt["serverId"]
                .as_str()
                .filter(|server| valid_id(server))
                .ok_or("Invalid native admission server.")?;
            if receipt["complete"] != true
                || receipt["hostVerified"] != true
                || receipt["proofVersion"] != migration_policy()["proofVersion"]
                || ["sourceDigest", "inventoryDigest", "ownerInventoryDigest"]
                    .iter()
                    .any(|field| !receipt[*field].as_str().is_some_and(valid_digest))
            {
                return Err(
                    "The native admission receipt lacks its complete private host proof.".into(),
                );
            }
            let saved:Option<String>=sql(db.query_row("SELECT receipt FROM client_storage_host_captures WHERE source_identity=?1 AND server_id=?2",params![source,server],|row|row.get(0)).optional())?;
            let saved: Value = serde_json::from_str(
                saved
                    .as_deref()
                    .ok_or("The native admission receipt is not saved on this Store.")?,
            )
            .map_err(|_| "The saved native host receipt is invalid.")?;
            if &saved != receipt
                || host_workspace(db)?.as_deref() != receipt["workspaceId"].as_str()
                || receipt["workspaceId"].is_null()
            {
                return Err("The native admission receipt or its workspace changed.".into());
            }
            // The saved proof establishes the import baseline once. Legitimate
            // later target edits do not require old import rows to remain live.
            return Ok(json!({"admitted":true}));
        }
        if request.method == migration_policy()["nativeSourceMethod"].as_str().unwrap() {
            if request
                .params
                .as_object()
                .is_none_or(|parameters| !parameters.is_empty())
            {
                return Err(
                    "The native legacy source does not accept a renderer inventory.".into(),
                );
            }
            return native_legacy_snapshot(db);
        }
        if request.method == migration_policy()["nativeImportMethod"].as_str().unwrap() {
            return import_native_legacy(db, &request.params);
        }
        if request.method == migration_policy()["ownerInventoryMethod"].as_str().unwrap() {
            if request
                .params
                .as_object()
                .is_none_or(|params| !params.is_empty())
            {
                return Err("The native owner inventory does not accept a source selector.".into());
            }
            return legacy_owners(db);
        }
        if request.method == migration_policy()["statusMethod"].as_str().unwrap()
            || request.method == migration_policy()["finishMethod"].as_str().unwrap()
        {
            return host_capture(db, request);
        }
        if request.method == "client_storage.cutover_status"
            || request.method == "client_storage.finish_cutover"
        {
            return cutover(db, request);
        }
        if request.method == "client_storage.replay_preferences" {
            let p = request
                .params
                .as_object()
                .filter(|p| p.len() == 2)
                .ok_or("Invalid legacy preference replay.")?;
            let scope = p
                .get("scope")
                .and_then(Value::as_str)
                .filter(|s| *s == "root" || s.starts_with("server:"))
                .ok_or("Invalid legacy preference owner.")?;
            let writes = prepare_original_preferences(
                scope,
                p.get("writes").ok_or("Missing legacy preference writes.")?,
            )?;
            let tx = sql(db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate))?;
            let mut ids = Vec::new();
            for original in writes {
                if let Some(receipt) = sql(crate::store::replay(&tx, &original))? {
                    if receipt.error.is_some() {
                        return Err(
                            "The original preference receipt rejects this recovery request.".into(),
                        );
                    }
                } else {
                    let imported = apply_preference(&tx, &original.method, &original.params)?;
                    let result = if original.method == "preferences.import" {
                        // The original owner reports the complete imported set.
                        json!({"imported":imported,"total":original.params["entries"].as_object().unwrap().len()})
                    } else {
                        json!({"saved":true})
                    };
                    sql(crate::store::record(
                        &tx,
                        &original,
                        &Response::success(original.id.clone(), result),
                    ))?;
                }
                ids.push(original.id);
            }
            let receipt = json!({"requestIds":ids,"sourceSha256":digest(serde_json::to_vec(&request.params).map_err(|e|e.to_string())?.as_slice())});
            sql(tx.commit())?;
            return Ok(receipt);
        }
        if request.method == "client_storage.export_preferences" {
            let p = request
                .params
                .as_object()
                .filter(|p| p.len() == 1)
                .ok_or("A legacy preference owner is required.")?;
            let scope = p
                .get("scope")
                .and_then(Value::as_str)
                .filter(|s| *s == "root" || s.starts_with("server:"))
                .ok_or("Invalid legacy preference owner.")?;
            return export_preferences(db, scope);
        }
        let p = request
            .params
            .as_object()
            .ok_or("Invalid migration parameters.")?;
        let id = p
            .get("manifestId")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty() && id.len() <= 512)
            .ok_or("A migration manifest identity is required.")?;
        let tx = sql(db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate))?;
        let existing: Option<(String, String)> = sql(tx
            .query_row(
                "SELECT source_sha256,receipt FROM client_storage_migrations WHERE id=?1",
                [id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional())?;
        if request.method == "client_storage.status" {
            if p.len() != 1 {
                return Err("Invalid migration status parameters.".into());
            }
            return existing
                .map(|(_, receipt)| serde_json::from_str(&receipt).map_err(|e| e.to_string()))
                .transpose()
                .map(|v| v.unwrap_or(Value::Null));
        }
        if request.method != "client_storage.import" || p.len() != 2 {
            return Err("Unknown migration command or parameters.".into());
        }
        let raw = p
            .get("serializedSource")
            .and_then(Value::as_str)
            .filter(|raw| raw.len() <= 16 * 1024 * 1024)
            .ok_or("A bounded migration source is required.")?;
        let hash = digest(raw.as_bytes());
        if let Some((old, receipt)) = existing {
            if old != hash {
                return Err("The migration source changed. Both sources were preserved.".into());
            }
            return serde_json::from_str(&receipt).map_err(|e| e.to_string());
        }
        let source: Migration = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        if let Some(manifest) = &source.archive_manifest {
            manifest.validate()?;
        }
        if source
            .source_identity
            .as_deref()
            .is_some_and(|id| !valid_digest(id))
            || source.expected_byte_imports.len() > 10_000
            || source
                .expected_byte_imports
                .iter()
                .any(|id| id.is_empty() || id.len() > 512)
            || source
                .expected_byte_imports
                .iter()
                .collect::<std::collections::BTreeSet<_>>()
                .len()
                != source.expected_byte_imports.len()
        {
            return Err("Invalid migration source or byte inventory.".into());
        }
        let source_identity = source.source_identity.clone().or_else(|| {
            source
                .archive_manifest
                .as_ref()
                .map(|m| m.archive.source_identity.clone())
        });
        if source
            .archive_manifest
            .as_ref()
            .is_some_and(|m| source_identity.as_deref() != Some(m.archive.source_identity.as_str()))
        {
            return Err("The archive and migration source identities differ.".into());
        }
        let expected_byte_imports = source.expected_byte_imports.clone();
        let archive_manifest = source.archive_manifest.clone();
        import_source(&tx, source)?;
        let mut receipt = json!({"manifestId":id,"sourceSha256":hash,"imported":true});
        if let Some(source) = source_identity {
            receipt["sourceIdentity"] = json!(source);
            receipt["expectedByteImports"] = json!(expected_byte_imports);
        }
        if let Some(manifest) = archive_manifest {
            receipt["archiveManifest"] =
                serde_json::to_value(manifest).map_err(|e| e.to_string())?;
        }
        sql(tx.execute(
            "INSERT INTO client_storage_migrations VALUES(?1,?2,?3)",
            params![id, hash, receipt.to_string()],
        ))?;
        sql(tx.commit())?;
        Ok(receipt)
    })();
    match result {
        Ok(value) => Response::success(request.id.clone(), value),
        Err(e) => Response::failure(request.id.clone(), "client_storage_migration", e),
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CutoverInventory {
    source_identity: String,
    server_id: String,
    metadata: Vec<MetadataReceipt>,
    bytes: Vec<ByteReceipt>,
    archive: Option<ExpectedArchive>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ExpectedArchive {
    id: String,
    owner: String,
    sha256: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MetadataReceipt {
    manifest_id: String,
    source_sha256: String,
}
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ByteReceipt {
    manifest_id: String,
    receipt: Value,
}

fn cutover(db: &mut Connection, request: &Request) -> Result<Value, String> {
    let p = request
        .params
        .as_object()
        .ok_or("Invalid cutover parameters.")?;
    let source = p
        .get("sourceIdentity")
        .and_then(Value::as_str)
        .filter(|source| valid_digest(source))
        .ok_or("Invalid cutover source identity.")?;
    let server = p
        .get("serverId")
        .and_then(Value::as_str)
        .filter(|server| valid_id(server))
        .ok_or("Invalid cutover server identity.")?;
    let tx = sql(db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate))?;
    let old:Option<(String,String)>=sql(tx.query_row("SELECT inventory,receipt FROM client_storage_cutovers WHERE source_identity=?1 AND server_id=?2",params![source,server],|r|Ok((r.get(0)?,r.get(1)?))).optional())?;
    if request.method == "client_storage.cutover_status" {
        if p.len() != 2 {
            return Err("Invalid cutover status parameters.".into());
        }
        return Ok(json!({"complete":old.is_some()}));
    }
    let inventory: CutoverInventory =
        serde_json::from_value(request.params.clone()).map_err(|e| e.to_string())?;
    if inventory.source_identity != source
        || inventory.server_id != server
        || inventory.metadata.is_empty()
        || inventory.metadata.len() > 10_000
        || inventory.bytes.len() > 10_000
    {
        return Err("A complete bounded migration inventory is required.".into());
    }
    let encoded = request.params.to_string();
    if let Some((existing, receipt)) = old {
        if existing != encoded {
            return Err(
                "The completed cutover inventory changed. The original seal was preserved.".into(),
            );
        }
        return serde_json::from_str(&receipt).map_err(|e| e.to_string());
    }
    verify_cutover_inventory(&tx, &inventory)?;
    let receipt = json!({"complete":true,"sourceIdentity":source,"serverId":server,"inventorySha256":digest(encoded.as_bytes())});
    sql(tx.execute(
        "INSERT INTO client_storage_cutovers VALUES(?1,?2,?3,?4)",
        params![source, server, encoded, receipt.to_string()],
    ))?;
    sql(tx.commit())?;
    Ok(receipt)
}

// Both completion owners use the byte/metadata readback owner. An old public
// completion row is never evidence for the private host capture table.
fn verify_cutover_inventory(db: &Connection, inventory: &CutoverInventory) -> Result<(), String> {
    let source = &inventory.source_identity;
    if !valid_digest(source)
        || !valid_id(&inventory.server_id)
        || inventory.metadata.is_empty()
        || inventory.metadata.len() > 10_000
        || inventory.bytes.len() > 10_000
    {
        return Err("A complete bounded migration inventory is required.".into());
    }
    if crate::uploads::import_pending(db)? {
        return Err("Finish every pending byte import before native cutover.".into());
    }
    let active = {
        let mut q=sql(db.prepare("SELECT id,source_sha256,receipt FROM client_storage_migrations WHERE json_extract(receipt,'$.sourceIdentity')=?1 ORDER BY id"))?;
        sql(q.query_map([source], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
            ))
        }))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?
    };
    let expected_metadata = active
        .iter()
        .map(|(id, _, _)| id.clone())
        .collect::<std::collections::BTreeSet<_>>();
    if expected_metadata
        != inventory
            .metadata
            .iter()
            .map(|receipt| receipt.manifest_id.clone())
            .collect()
    {
        return Err("The cutover does not include every source metadata receipt.".into());
    }
    let mut expected_bytes = std::collections::BTreeSet::new();
    let mut archive_found = inventory.archive.is_none();
    for (_, _, raw) in &active {
        let receipt: Value = serde_json::from_str(raw).map_err(|e| e.to_string())?;
        for id in receipt["expectedByteImports"]
            .as_array()
            .ok_or("The migration receipt lacks its byte inventory.")?
        {
            expected_bytes.insert(
                id.as_str()
                    .ok_or("Invalid expected byte manifest identity.")?
                    .to_owned(),
            );
        }
        if let Some(archive) = &inventory.archive {
            let saved = &receipt["archiveManifest"]["archive"];
            if saved["id"] == archive.id
                && saved["owner"] == archive.owner
                && saved["sha256"] == archive.sha256
                && saved["sourceIdentity"].as_str() == Some(source.as_str())
            {
                archive_found = true;
            }
        }
    }
    if !archive_found {
        return Err("The sealed archive is missing from the native cutover inventory.".into());
    }
    if expected_bytes
        != inventory
            .bytes
            .iter()
            .map(|receipt| receipt.manifest_id.clone())
            .collect()
    {
        return Err("The cutover byte receipt inventory is incomplete or changed.".into());
    }
    let mut ids = std::collections::BTreeSet::new();
    for expected in &inventory.metadata {
        if !ids.insert(("metadata", expected.manifest_id.clone()))
            || !valid_digest(&expected.source_sha256)
        {
            return Err("Invalid or duplicate metadata migration receipt.".into());
        }
        let hash: Option<String> = sql(db
            .query_row(
                "SELECT source_sha256 FROM client_storage_migrations WHERE id=?1",
                [&expected.manifest_id],
                |r| r.get(0),
            )
            .optional())?;
        if hash.as_deref() != Some(&expected.source_sha256) {
            return Err("A metadata migration receipt is missing or changed.".into());
        }
    }
    for expected in &inventory.bytes {
        if !ids.insert(("bytes", expected.manifest_id.clone())) {
            return Err("Duplicate byte migration receipt.".into());
        }
        crate::uploads::verify_import_receipt(db, &expected.manifest_id, &expected.receipt)?;
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostCaptureEvidence {
    proof_version: u64,
    before_digest: String,
    after_digest: String,
    origin_inventory_digest: String,
    owner_inventory_digest: String,
    all_origins_verified: bool,
}

fn host_workspace(db: &Connection) -> Result<Option<String>, String> {
    let value: Option<String> = sql(db
        .query_row(
            "SELECT value FROM client_identity WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional())?;
    value
        .map(|value| {
            let identity: Value = serde_json::from_str(&value)
                .map_err(|_| "The native host workspace identity is invalid.")?;
            identity["workspaceId"]
                .as_str()
                .filter(|workspace| crate::projections::valid_workspace(workspace))
                .map(str::to_owned)
                .ok_or_else(|| "The native host workspace identity is invalid.".into())
        })
        .transpose()
}

fn host_capture(db: &mut Connection, request: &Request) -> Result<Value, String> {
    let mut parameters = request
        .params
        .as_object()
        .cloned()
        .ok_or("Invalid host capture parameters.")?;
    let source = parameters
        .get("sourceIdentity")
        .and_then(Value::as_str)
        .filter(|value| valid_digest(value))
        .ok_or("Invalid host capture source identity.")?
        .to_owned();
    let server = parameters
        .get("serverId")
        .and_then(Value::as_str)
        .filter(|value| valid_id(value))
        .ok_or("Invalid host capture server identity.")?
        .to_owned();
    let tx = sql(db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate))?;
    let previous:Option<(String,String)>=sql(tx.query_row("SELECT inventory,receipt FROM client_storage_host_captures WHERE source_identity=?1 AND server_id=?2",params![source,server],|row|Ok((row.get(0)?,row.get(1)?))).optional())?;
    if request.method == migration_policy()["statusMethod"].as_str().unwrap() {
        let source_digest = parameters
            .get("sourceDigest")
            .and_then(Value::as_str)
            .filter(|value| valid_digest(value))
            .ok_or("Invalid captured source digest.")?;
        if parameters.len() != 3 {
            return Err("Invalid host capture status parameters.".into());
        }
        if let Some((_, receipt)) = previous {
            let receipt: Value =
                serde_json::from_str(&receipt).map_err(|error| error.to_string())?;
            if receipt["sourceDigest"] == source_digest {
                return Ok(receipt);
            }
            return Ok(json!({"complete":false,"held":true}));
        }
        return Ok(json!({"complete":false,"held":false}));
    }
    let evidence: HostCaptureEvidence = serde_json::from_value(
        parameters
            .remove("capture")
            .ok_or("The native host capture proof is required.")?,
    )
    .map_err(|error| error.to_string())?;
    if evidence.before_digest != evidence.after_digest {
        return Err("The captured source changed before completion.".into());
    }
    if !evidence.all_origins_verified {
        return Err("Every saved origin requires native verification.".into());
    }
    if Some(evidence.proof_version) != migration_policy()["proofVersion"].as_u64()
        || [
            &evidence.before_digest,
            &evidence.after_digest,
            &evidence.origin_inventory_digest,
            &evidence.owner_inventory_digest,
        ]
        .iter()
        .any(|value| !valid_digest(value))
    {
        return Err("The native host capture proof is invalid.".into());
    }
    let encoded = request.params.to_string();
    if let Some((saved, receipt)) = previous {
        if saved != encoded {
            return Err(
                "The native host capture inventory changed. Its original proof remains saved."
                    .into(),
            );
        }
        return serde_json::from_str(&receipt).map_err(|error| error.to_string());
    }
    let inventory: CutoverInventory =
        serde_json::from_value(Value::Object(parameters)).map_err(|error| error.to_string())?;
    verify_cutover_inventory(&tx, &inventory)?;
    let receipt = json!({"complete":true,"hostVerified":true,"sourceIdentity":source,"serverId":server,"workspaceId":host_workspace(&tx)?,"proofVersion":evidence.proof_version,"sourceDigest":evidence.after_digest,"ownerInventoryDigest":evidence.owner_inventory_digest,"inventoryDigest":digest(encoded.as_bytes())});
    sql(tx.execute(
        "INSERT INTO client_storage_host_captures VALUES(?1,?2,?3,?4)",
        params![source, server, encoded, receipt.to_string()],
    ))?;
    sql(tx.commit())?;
    Ok(receipt)
}

#[cfg(test)]
mod host_capture_tests {
    use super::*;
    use crate::store::Store;

    fn policy() -> Value {
        serde_json::from_str(include_str!("../../../contracts/native-migration.json")).unwrap()
    }
    fn send(store: &mut Store, method: &str, params: Value) -> Response {
        store.dispatch(Request {
            id: crate::server_keys::random_id().unwrap(),
            method: method.into(),
            params,
        })
    }
    fn metadata(store: &mut Store, expected: Vec<&str>) -> Value {
        send(store,"client_storage.import",json!({"manifestId":"host-capture-metadata","serializedSource":json!({"version":1,"sourceIdentity":"a".repeat(64),"expectedByteImports":expected,"preferences":{"theme":"saved"}}).to_string()})).result.unwrap()
    }
    fn finish(metadata: &Value) -> Value {
        json!({"sourceIdentity":"a".repeat(64),"serverId":"local","metadata":[{"manifestId":"host-capture-metadata","sourceSha256":metadata["sourceSha256"]}],"bytes":[],"archive":null,
            "capture":{"proofVersion":policy()["proofVersion"],"beforeDigest":"b".repeat(64),"afterDigest":"b".repeat(64),"originInventoryDigest":"c".repeat(64),"ownerInventoryDigest":"d".repeat(64),"allOriginsVerified":true}})
    }

    #[test]
    fn native_host_capture_does_not_promote_a_renderer_legacy_completion() {
        let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let metadata = metadata(&mut store, vec![]);
        let mut old = finish(&metadata);
        old.as_object_mut().unwrap().remove("capture");
        assert_eq!(
            send(&mut store, "client_storage.finish_cutover", old)
                .result
                .unwrap()["complete"],
            true
        );
        let response = send(
            &mut store,
            policy()["statusMethod"].as_str().unwrap(),
            json!({"sourceIdentity":"a".repeat(64),"serverId":"local","sourceDigest":"b".repeat(64)}),
        );
        assert_eq!(
            response
                .result
                .expect("Host status must distinguish an old renderer seal")["complete"],
            false
        );
    }

    #[test]
    fn native_host_capture_refuses_changed_partial_or_unproved_source_before_completion() {
        let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let metadata = metadata(&mut store, vec!["unstarted-byte"]);
        let mut changed = finish(&metadata);
        changed["capture"]["afterDigest"] = json!("e".repeat(64));
        let refused = send(
            &mut store,
            policy()["finishMethod"].as_str().unwrap(),
            changed,
        );
        assert!(refused.error.unwrap().message.contains("changed"));
        let mut unknown = finish(&metadata);
        unknown["capture"]["allOriginsVerified"] = json!(false);
        let refused = send(
            &mut store,
            policy()["finishMethod"].as_str().unwrap(),
            unknown,
        );
        assert!(refused.error.unwrap().message.contains("origin"));
        let refused = send(
            &mut store,
            policy()["finishMethod"].as_str().unwrap(),
            finish(&metadata),
        );
        assert!(refused.error.unwrap().message.contains("byte"));
    }

    #[test]
    fn native_host_capture_proof_survives_restart_and_refuses_changed_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("store.sqlite");
        let original = {
            let mut store = Store::open(&path).unwrap();
            let metadata = metadata(&mut store, vec![]);
            let request = finish(&metadata);
            let first = send(
                &mut store,
                policy()["finishMethod"].as_str().unwrap(),
                request.clone(),
            )
            .result
            .expect("The private host completion must verify its full inventory");
            assert_eq!(first["complete"], true);
            assert_eq!(first["proofVersion"], policy()["proofVersion"]);
            assert_eq!(
                send(
                    &mut store,
                    policy()["finishMethod"].as_str().unwrap(),
                    request.clone()
                )
                .result
                .unwrap(),
                first
            );
            (first, request)
        };
        let mut store = Store::open(&path).unwrap();
        let status=send(&mut store,policy()["statusMethod"].as_str().unwrap(),json!({"sourceIdentity":"a".repeat(64),"serverId":"local","sourceDigest":"b".repeat(64)})).result.unwrap();
        assert_eq!(status, original.0);
        let changed=send(&mut store,policy()["statusMethod"].as_str().unwrap(),json!({"sourceIdentity":"a".repeat(64),"serverId":"local","sourceDigest":"e".repeat(64)})).result.unwrap();
        assert_eq!(changed["complete"], false);
        let mut changed = original.1;
        changed["capture"]["ownerInventoryDigest"] = json!("f".repeat(64));
        assert!(
            send(
                &mut store,
                policy()["finishMethod"].as_str().unwrap(),
                changed
            )
            .error
            .is_some()
        );
    }

    #[test]
    fn native_host_capture_receipt_retains_exact_owner_inventory_digest() {
        let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let metadata = metadata(&mut store, vec![]);
        let request = finish(&metadata);
        let receipt = send(
            &mut store,
            policy()["finishMethod"].as_str().unwrap(),
            request.clone(),
        )
        .result
        .unwrap();
        assert_eq!(
            receipt["ownerInventoryDigest"], request["capture"]["ownerInventoryDigest"],
            "A private restart read must bind the current owner inventory before it can skip an import"
        );
    }

    #[test]
    fn native_host_capture_changed_baseline_holds_before_another_import() {
        let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
        let initial = send(&mut store,policy()["statusMethod"].as_str().unwrap(),json!({"sourceIdentity":"a".repeat(64),"serverId":"local","sourceDigest":"b".repeat(64)})).result.unwrap();
        assert_eq!(initial["complete"], false);
        assert_eq!(
            initial["held"], false,
            "A fresh source must explicitly differ from a changed completed baseline"
        );
        let metadata = metadata(&mut store, vec![]);
        let receipt = send(
            &mut store,
            policy()["finishMethod"].as_str().unwrap(),
            finish(&metadata),
        )
        .result
        .unwrap();
        let status = send(&mut store,policy()["statusMethod"].as_str().unwrap(),json!({"sourceIdentity":"a".repeat(64),"serverId":"local","sourceDigest":"e".repeat(64)})).result.unwrap();
        assert_eq!(status["complete"], false);
        assert_eq!(
            status["held"], true,
            "A changed old source cannot look like an unstarted migration"
        );
        assert_eq!(send(&mut store,policy()["statusMethod"].as_str().unwrap(),json!({"sourceIdentity":"a".repeat(64),"serverId":"local","sourceDigest":"b".repeat(64)})).result.unwrap(),receipt);
    }

    #[tokio::test]
    async fn native_host_capture_runtime_admission_requires_own_durable_proof_and_keeps_later_edits()
     {
        use crate::{backend::Backend, client::Client};
        async fn command(client: &Client, id: &str, method: &str, params: Value) -> Response {
            client
                .request(Request {
                    id: id.into(),
                    method: method.into(),
                    params,
                })
                .await
                .unwrap()
        }
        let directory = tempfile::tempdir().unwrap();
        let backend = Backend::new("http://127.0.0.1:9").unwrap();
        let client =
            Client::open_for_migration(directory.path().join("owner"), backend.clone(), |_| {})
                .unwrap();
        assert_eq!(
            command(
                &client,
                "state",
                policy()["runtimeStateMethod"].as_str().unwrap(),
                json!({})
            )
            .await
            .result
            .unwrap(),
            json!({"held":true})
        );
        assert!(
            command(
                &client,
                "identity",
                "identity.set",
                json!({"workspaceId":"a".repeat(32)})
            )
            .await
            .error
            .is_none()
        );
        let forged = json!({"complete":true,"hostVerified":true,"sourceIdentity":"a".repeat(64),"serverId":"local","workspaceId":"a".repeat(32),"proofVersion":policy()["proofVersion"],"sourceDigest":"b".repeat(64),"ownerInventoryDigest":"d".repeat(64),"inventoryDigest":"e".repeat(64)});
        assert!(
            command(
                &client,
                "forged",
                policy()["admitMethod"].as_str().unwrap(),
                json!({"receipt":forged})
            )
            .await
            .error
            .is_some()
        );
        assert_eq!(
            command(&client, "still-held", "outbox.activate", json!({}))
                .await
                .error
                .unwrap()
                .code,
            "migration_held"
        );
        let metadata=command(&client,"metadata","client_storage.import",json!({"manifestId":"host-capture-metadata","serializedSource":json!({"version":1,"sourceIdentity":"a".repeat(64),"expectedByteImports":[],"preferences":{"theme":"baseline"}}).to_string()})).await.result.unwrap();
        let receipt = command(
            &client,
            "finish",
            policy()["finishMethod"].as_str().unwrap(),
            finish(&metadata),
        )
        .await
        .result
        .unwrap();
        let sibling =
            Client::open_for_migration(directory.path().join("sibling"), backend, |_| {}).unwrap();
        assert!(
            command(
                &sibling,
                "foreign",
                policy()["admitMethod"].as_str().unwrap(),
                json!({"receipt":receipt})
            )
            .await
            .error
            .is_some()
        );
        assert_eq!(
            command(&sibling, "sibling-held", "outbox.activate", json!({}))
                .await
                .error
                .unwrap()
                .code,
            "migration_held"
        );
        assert_eq!(
            command(
                &client,
                "admit",
                policy()["admitMethod"].as_str().unwrap(),
                json!({"receipt":receipt})
            )
            .await
            .result
            .unwrap(),
            json!({"admitted":true})
        );
        let edit = command(
            &client,
            "new-native-edit",
            "preferences.raw_set",
            json!({"key":"theme","raw":"newer"}),
        )
        .await;
        assert!(edit.error.is_none());
        assert!(
            command(
                &client,
                "admit-replay",
                policy()["admitMethod"].as_str().unwrap(),
                json!({"receipt":receipt})
            )
            .await
            .error
            .is_none()
        );
        assert_eq!(
            command(&client, "current", "preferences.raw_all", json!({}))
                .await
                .result
                .unwrap()["theme"],
            "newer"
        );
        assert_eq!(
            command(
                &client,
                "new-native-edit",
                "preferences.raw_set",
                json!({"key":"theme","raw":"newer"})
            )
            .await
            .result,
            edit.result
        );
    }

    #[tokio::test]
    async fn native_host_capture_admission_recovers_existing_owners_once_before_releasing_effects()
    {
        use crate::{backend::Backend, client::Client};
        async fn command(client: &Client, id: &str, method: &str, params: Value) -> Response {
            client
                .request(Request {
                    id: id.into(),
                    method: method.into(),
                    params,
                })
                .await
                .unwrap()
        }
        fn retained(path: &std::path::Path) -> Value {
            let db = Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .unwrap();
            let outbox: (String, i64) = db
                .query_row(
                    "SELECT payload,in_flight FROM client_outbox WHERE id='original-message'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let cache: Option<String> = db
                .query_row(
                    "SELECT document FROM client_projections WHERE scope='entity:held'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            let version: i64 = db
                .query_row(
                    "SELECT version FROM client_projection_cache_version WHERE singleton=1",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let batch:(String,String,i64,Option<String>)=db.query_row("SELECT body,journal,attempted,error FROM client_preference_batches WHERE scope='user'",[],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?))).unwrap();
            let unknown:String=db.query_row("SELECT document FROM client_mutation_journal WHERE family='mutation' AND json_extract(document,'$.requestId')='original-http'",[],|row|row.get(0)).unwrap();
            json!({"payload":outbox.0,"inFlight":outbox.1,"cache":cache,"cacheVersion":version,"body":batch.0,"journal":batch.1,"attempted":batch.2,"error":batch.3,"unknown":unknown})
        }
        let directory = tempfile::tempdir().unwrap();
        let owned = directory.path().canonicalize().unwrap();
        let database = owned.join("studio-client.sqlite3");
        let workspace = "a".repeat(32);
        let mut store = Store::open(&database).unwrap();
        assert!(
            send(&mut store, "identity.set", json!({"workspaceId":workspace}))
                .error
                .is_none()
        );
        let body = json!({"id":"original-message","room":"chat","text":"original bytes"});
        assert!(
            send(
                &mut store,
                "outbox.enqueue",
                json!({"workspace":workspace,"body":body})
            )
            .error
            .is_none()
        );
        assert_eq!(
            send(
                &mut store,
                "outbox.claim",
                json!({"workspace":workspace,"id":"original-message"})
            )
            .result
            .unwrap()["claimed"],
            true
        );
        assert!(
            send(
                &mut store,
                "preferences.raw_set",
                json!({"key":"codex-studio-preferences-v1","raw":"{\"theme\":\"dark\"}"})
            )
            .error
            .is_none()
        );
        let batch = send(&mut store, "preference_sync.next", json!({"attempted":{}}))
            .result
            .unwrap();
        let batch_identity = json!({"scope":"user","requestId":batch["journal"]["requestId"],"payloadDigest":batch["journal"]["payloadDigest"]});
        assert!(
            send(
                &mut store,
                "preference_sync.dispatch",
                batch_identity.clone()
            )
            .error
            .is_none()
        );
        let unknown=send(&mut store,"mutation.begin",json!({"path":"/api/messages","serializedPayload":"{\"id\":\"original-http\",\"text\":\"saved\"}","explicitRequestId":"original-http"})).result.unwrap();
        assert!(
            send(
                &mut store,
                "mutation.unknown",
                json!({"requestId":"original-http","payloadDigest":unknown["payloadDigest"]})
            )
            .error
            .is_none()
        );
        store
            .connection
            .execute(
                "INSERT INTO client_projections VALUES(?1,'entity:held','retained-cache',1)",
                [&workspace],
            )
            .unwrap();
        let cache_version: i64 = store
            .connection
            .query_row(
                "SELECT version FROM client_projection_cache_version WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE client_projection_cache_version SET version=version-1",
                [],
            )
            .unwrap();
        drop(store);
        let before = retained(&database);
        assert_eq!(before["inFlight"], 1);
        assert!(before["error"].is_null());
        let client =
            Client::open_for_migration(owned, Backend::new("http://127.0.0.1:9").unwrap(), |_| {})
                .unwrap();
        assert!(
            command(&client, "barrier", "preferences.raw_all", json!({}))
                .await
                .error
                .is_none()
        );
        assert_eq!(
            retained(&database),
            before,
            "No recovery is allowed before full private proof"
        );
        assert_eq!(
            command(
                &client,
                "held-claim",
                "outbox.claim",
                json!({"workspace":workspace,"id":"original-message"})
            )
            .await
            .error
            .unwrap()
            .code,
            "migration_held"
        );
        let metadata=command(&client,"metadata","client_storage.import",json!({"manifestId":"host-capture-metadata","serializedSource":json!({"version":1,"sourceIdentity":"a".repeat(64),"expectedByteImports":[],"preferences":{"font":"saved"}}).to_string()})).await.result.unwrap();
        let receipt = command(
            &client,
            "finish",
            policy()["finishMethod"].as_str().unwrap(),
            finish(&metadata),
        )
        .await
        .result
        .unwrap();
        assert!(
            command(
                &client,
                "admit",
                policy()["admitMethod"].as_str().unwrap(),
                json!({"receipt":receipt})
            )
            .await
            .error
            .is_none()
        );
        let recovered = retained(&database);
        assert_eq!(
            recovered["inFlight"], 0,
            "Only the existing outbox recovery owner releases the old room claim"
        );
        assert_eq!(recovered["payload"], before["payload"]);
        assert_eq!(recovered["unknown"], before["unknown"]);
        assert_eq!(recovered["body"], before["body"]);
        assert_eq!(recovered["journal"], before["journal"]);
        assert_eq!(recovered["attempted"], 1);
        assert!(
            recovered["error"]
                .as_str()
                .is_some_and(|error| !error.is_empty())
        );
        assert!(recovered["cache"].is_null());
        assert_eq!(recovered["cacheVersion"], cache_version);
        assert_eq!(
            command(
                &client,
                "current-claim",
                "outbox.claim",
                json!({"workspace":workspace,"id":"original-message"})
            )
            .await
            .result
            .unwrap()["claimed"],
            true
        );
        let mut ack = batch_identity;
        ack["fields"] = batch["body"]["fields"].clone();
        assert!(
            command(&client, "ack", "preference_sync.ack", ack)
                .await
                .error
                .is_none()
        );
        assert!(
            command(
                &client,
                "new-edit",
                "preferences.raw_set",
                json!({"key":"codex-studio-preferences-v1","raw":"{\"theme\":\"light\"}"})
            )
            .await
            .error
            .is_none()
        );
        let next = command(
            &client,
            "new-batch",
            "preference_sync.next",
            json!({"attempted":{}}),
        )
        .await
        .result
        .unwrap();
        assert!(command(&client,"new-attempt","preference_sync.dispatch",json!({"scope":"user","requestId":next["journal"]["requestId"],"payloadDigest":next["journal"]["payloadDigest"]})).await.error.is_none());
        assert!(command(&client,"new-cache","projection.put",json!({"workspace":workspace,"scope":"entity:held","document":{"id":"entity:held","seq":2,"payload":"{}"}})).await.error.is_none());
        let active = retained(&database);
        assert_eq!(active["inFlight"], 1);
        assert!(active["error"].is_null());
        assert!(active["cache"].is_string());
        assert!(
            command(
                &client,
                "admit-again",
                policy()["admitMethod"].as_str().unwrap(),
                json!({"receipt":receipt})
            )
            .await
            .error
            .is_none()
        );
        assert_eq!(
            retained(&database),
            active,
            "A repeated admission must not recover current connection work again"
        );
    }

    #[test]
    fn native_host_capture_inventory_keeps_original_empty_scopes_and_receipt_owners() {
        let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
        for (id, method, params) in [
            (
                "old-remote",
                "preferences.raw_set",
                json!({"key":"browser:\"root\"::server:remote:theme","raw":"saved"}),
            ),
            (
                "old-remote-remove",
                "preferences.raw_remove",
                json!({"key":"browser:\"root\"::server:remote:theme"}),
            ),
            (
                "old-direct",
                "preferences.raw_set",
                json!({"key":"browser:\"server:direct\":theme","raw":"saved"}),
            ),
            (
                "ordinary-native",
                "preferences.raw_set",
                json!({"key":":server:unrelated:theme","raw":"saved"}),
            ),
        ] {
            assert!(
                store
                    .dispatch(Request {
                        id: id.into(),
                        method: method.into(),
                        params
                    })
                    .error
                    .is_none()
            );
        }
        let exported=send(&mut store,policy()["ownerInventoryMethod"].as_str().unwrap(),json!({})).result.expect("The host must enumerate original namespace and receipt owners before selecting destinations");
        assert_eq!(exported, json!(["root", "server:direct", "server:remote"]));
        assert_eq!(
            send(&mut store, "preferences.raw_all", json!({}))
                .result
                .unwrap()
                .get("browser:\"root\"::server:remote:theme"),
            None
        );
        assert!(
            store
                .dispatch(Request {
                    id: "unknown-namespace".into(),
                    method: "preferences.raw_set".into(),
                    params: json!({"key":"browser:\"unknown\":theme","raw":"saved"})
                })
                .error
                .is_none()
        );
        assert!(
            send(
                &mut store,
                policy()["ownerInventoryMethod"].as_str().unwrap(),
                json!({})
            )
            .error
            .is_some(),
            "Unknown retained native namespaces must hold the migration"
        );
    }
    #[tokio::test]
    async fn native_host_capture_bootstrap_read_and_target_open_hold_original_outbox_and_unknown_identity()
     {
        use crate::{backend::Backend, client::Client};
        let directory = tempfile::tempdir().unwrap();
        let database = directory
            .path()
            .canonicalize()
            .unwrap()
            .join("studio-client.sqlite3");
        let mut store = Store::open(&database).unwrap();
        let workspace = "a".repeat(32);
        let payload=json!({"id":"original-message","workspace":workspace,"body":{"id":"original-message","room":"chat","text":"pending"},"status":"uncertain","attempted":true,"created":1,"displayPending":true,"attachments":[]}).to_string();
        store
            .connection
            .execute(
                "INSERT INTO client_outbox VALUES(?1,'original-message','chat',?2,1)",
                params![workspace, payload],
            )
            .unwrap();
        store
            .connection
            .execute(
                "INSERT INTO client_projections VALUES(?1,'entity:held','retained-cache',1)",
                [&workspace],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE client_projection_cache_version SET version=version-1",
                [],
            )
            .unwrap();
        let original=send(&mut store,"mutation.begin",json!({"path":"/api/messages","serializedPayload":"{\"id\":\"original-http\",\"text\":\"saved\"}","explicitRequestId":"original-http"})).result.unwrap();
        assert_eq!(
            send(
                &mut store,
                "mutation.unknown",
                json!({"requestId":"original-http","payloadDigest":original["payloadDigest"]})
            )
            .result,
            Some(json!(true))
        );
        fn retained(path: &std::path::Path) -> Value {
            let db = Connection::open_with_flags(
                path,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
                    | rusqlite::OpenFlags::SQLITE_OPEN_NOFOLLOW,
            )
            .unwrap();
            let outbox: (String, i64) = db
                .query_row(
                    "SELECT payload,in_flight FROM client_outbox WHERE id='original-message'",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let projection: Option<String> = db
                .query_row(
                    "SELECT document FROM client_projections WHERE scope='entity:held'",
                    [],
                    |row| row.get(0),
                )
                .optional()
                .unwrap();
            let journal: String = db
                .query_row(
                    "SELECT document FROM client_mutation_journal WHERE family='mutation'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            let mut query=db.prepare("SELECT request_id,method,params,response FROM client_receipts ORDER BY request_id").unwrap();
            let receipts = query
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                    ))
                })
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap();
            json!({"outbox":outbox,"projection":projection,"journal":journal,"receipts":receipts})
        }
        drop(store);
        let before = retained(&database);
        let source = crate::store::read_original_migration_source(&database)
            .unwrap()
            .expect("The original native file must be read before target initialization");
        assert!(source["schema"].is_object());
        assert_eq!(
            retained(&database),
            before,
            "The original read cannot run restart recovery or a user effect"
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let client = Client::open_for_migration(
            directory.path().into(),
            Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap(),
            |_| {},
        )
        .unwrap();
        assert!(
            client
                .request(Request {
                    id: "migration-read-barrier".into(),
                    method: "preferences.raw_all".into(),
                    params: json!({})
                })
                .await
                .unwrap()
                .error
                .is_none()
        );
        assert_eq!(
            retained(&database),
            before,
            "Target initialization remains paused until full-owner proof admission"
        );
        for (method, params) in [
            ("outbox.activate", json!({})),
            (
                "view.subscribe",
                json!({"subscription":"held-preferences","topic":"preferences"}),
            ),
            (
                "api.request",
                json!({"path":"/api/messages","body":{"id":"must-not-send","room":"chat","text":"held"},"timeoutMs":100}),
            ),
        ] {
            let response = client
                .request(Request {
                    id: format!("held-{method}"),
                    method: method.into(),
                    params,
                })
                .await
                .unwrap();
            assert_eq!(
                response
                    .error
                    .expect("No user effect is admitted before proof")
                    .code,
                "migration_held"
            );
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        assert_eq!(retained(&database), before);
    }

    #[test]
    fn native_host_capture_imported_receipts_do_not_become_original_native_source() {
        let mut store = Store::open(std::path::Path::new(":memory:")).unwrap();
        assert!(
            store
                .dispatch(Request {
                    id: "original-native".into(),
                    method: "preferences.raw_set".into(),
                    params: json!({"key":"browser:\"root\":theme","raw":"saved"})
                })
                .error
                .is_none()
        );
        let before = send(
            &mut store,
            "client_storage.export_preferences",
            json!({"scope":"root"}),
        )
        .result
        .unwrap();
        let original = Request {
            id: "copied-original-preference".into(),
            method: "preferences.raw_set".into(),
            params: json!({"key":"browser:\"root\":font","raw":"new"}),
        };
        let response = Response::success(original.id.clone(), json!({"saved":true}));
        assert!(send(&mut store,"client_storage.import",json!({"manifestId":"retained-peer-source","serializedSource":json!({"version":1,"legacyPreferenceScope":"root","preferences":{"font":"new"},"preferenceReceipts":[before["preferenceReceipts"][0],{"id":original.id,"method":original.method,"params":original.params,"response":response}]}).to_string()})).error.is_none());
        assert_eq!(
            send(
                &mut store,
                "client_storage.export_preferences",
                json!({"scope":"root"})
            )
            .result
            .unwrap(),
            before,
            "A target receipt copy must not change the original native source capture after an import"
        );
        assert_eq!(
            crate::store::replay(&store.connection, &original)
                .unwrap()
                .unwrap()
                .result,
            response.result,
            "The exact original operation receipt remains available on its target owner"
        );
    }

    #[test]
    fn native_host_capture_native_drafts_keep_all_workspaces_receipts_and_restart_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("original.sqlite");
        let mut store = Store::open(&path).unwrap();
        let first_workspace = "a".repeat(32);
        let second_workspace = "b".repeat(32);
        let original = |id: &str, workspace: &str, text: &str| Request {
            id: id.into(),
            method: "draft.set".into(),
            params: json!({"workspace":workspace,"room":"chat","text":text,"base_revision":0}),
        };
        let first = store.dispatch(original(
            "original-local-write",
            &first_workspace,
            "saved 🦀",
        ));
        assert!(first.error.is_none());
        assert!(
            store
                .dispatch(original(
                    "original-other-workspace",
                    &second_workspace,
                    "other saved text"
                ))
                .error
                .is_none()
        );
        let snapshot = send(
            &mut store,
            policy()["nativeSourceMethod"].as_str().unwrap(),
            json!({}),
        )
        .result
        .expect("The private host source must include original native draft rows");
        assert_eq!(snapshot["drafts"].as_array().unwrap().len(), 2);
        assert_eq!(
            snapshot["receiptIds"],
            json!(["original-local-write", "original-other-workspace"])
        );
        let params = json!({"sourceIdentity":"c".repeat(64),"expectedSourceSha256":snapshot["sourceSha256"]});
        let response = send(
            &mut store,
            policy()["nativeImportMethod"].as_str().unwrap(),
            params.clone(),
        );
        assert!(
            response.error.is_none(),
            "The native owner must adopt its own legacy rows: {:?}",
            response.error
        );
        let first_import = response
            .result
            .expect("The native owner must adopt its own legacy rows without a renderer inventory");
        for (workspace, text) in [
            (&first_workspace, "saved 🦀"),
            (&second_workspace, "other saved text"),
        ] {
            assert_eq!(
                send(&mut store, "drafts.view", json!({"workspace":workspace}))
                    .result
                    .unwrap()["drafts"]["chat"],
                text
            );
        }
        // The caller loses the import response. The new owner must read the
        // exact original receipt instead of another import or draft write.
        drop(store);
        let mut restarted = Store::open(&path).unwrap();
        assert_eq!(
            send(
                &mut restarted,
                policy()["nativeImportMethod"].as_str().unwrap(),
                params.clone()
            )
            .result
            .unwrap(),
            first_import
        );
        assert_eq!(
            restarted
                .dispatch(original(
                    "original-local-write",
                    &first_workspace,
                    "saved 🦀"
                ))
                .result,
            first.result
        );
        assert_eq!(
            send(
                &mut restarted,
                policy()["nativeSourceMethod"].as_str().unwrap(),
                json!({})
            )
            .result
            .unwrap(),
            snapshot
        );
        let mut forged = params;
        forged["expectedSourceSha256"] = json!("d".repeat(64));
        assert!(
            send(
                &mut restarted,
                policy()["nativeImportMethod"].as_str().unwrap(),
                forged
            )
            .error
            .is_some()
        );
        assert!(send(&mut restarted,policy()["nativeImportMethod"].as_str().unwrap(),json!({"sourceIdentity":"c".repeat(64),"expectedSourceSha256":snapshot["sourceSha256"],"drafts":[]})).error.is_some(),"A renderer-like empty row inventory cannot select the native source");
    }
}

#[cfg(test)]
mod write_id_tests {
    use super::*;
    use crate::store::Store;
    use std::path::Path;

    fn call(store: &mut Store, method: &str, params: Value) -> Response {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        store.dispatch(Request {
            id: format!(
                "write-id-test:{}",
                NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ),
            method: method.into(),
            params,
        })
    }

    fn value(store: &mut Store, method: &str, params: Value) -> Value {
        let response = call(store, method, params);
        assert!(
            response.error.is_none(),
            "{}",
            serde_json::to_string(&response).unwrap()
        );
        response.result.unwrap()
    }

    fn payload(action: &str) -> String {
        json!({"method":"outbox.change","params":{"workspace":"w","id":"a","action":action}})
            .to_string()
    }

    fn write_id(store: &mut Store, key: &str, serialized: &str) -> String {
        let id = value(
            store,
            "mutation.write_id",
            json!({"key":key,"serializedPayload":serialized}),
        );
        let id = id.as_str().unwrap();
        assert!(valid_id(id));
        id.into()
    }

    #[test]
    fn native_write_id_reuses_large_current_payload_after_reopen_without_storing_plaintext() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let text = "private native display Ж".repeat(300);
        let serialized =
            json!({"method":"outbox.display","params":{"workspace":"w","id":"a","text":text}})
                .to_string();
        assert!(serialized.len() > 4096);
        assert!(serialized.len() < contract().max_payload_bytes);
        let first = {
            let mut store = Store::open(&path).unwrap();
            let id = write_id(&mut store, "display:w:a", &serialized);
            assert_eq!(write_id(&mut store, "display:w:a", &serialized), id);
            id
        };
        let mut store = Store::open(&path).unwrap();
        assert_eq!(write_id(&mut store, "display:w:a", &serialized), first);
        let receipts: String = store.connection.query_row(
            "SELECT coalesce(group_concat(params),'') FROM client_receipts WHERE method='mutation.write_id'",
            [], |row| row.get(0),
        ).unwrap();
        let documents: String = store
            .connection
            .query_row(
                "SELECT coalesce(group_concat(document),'') FROM client_mutation_journal",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(!receipts.contains("private native display"));
        assert!(!documents.contains("private native display"));
    }

    #[test]
    fn native_write_id_rotates_explicit_content_and_preserves_old_outbox_receipts() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        value(
            &mut store,
            "outbox.enqueue",
            json!({"workspace":"w","body":{"id":"a","room":"r","text":"original"}}),
        );
        let a = write_id(&mut store, "change:w:a", &payload("pause"));
        let pause = |id: &str| Request {
            id: id.into(),
            method: "outbox.change".into(),
            params: json!({"workspace":"w","id":"a","action":"pause"}),
        };
        // The Store commits A, but its caller loses the response and keeps the pointer.
        let saved_a = store.dispatch(pause(&a));
        assert_eq!(saved_a.result.as_ref().unwrap()["status"], "paused");
        let b = write_id(&mut store, "change:w:a", &payload("resume"));
        assert_ne!(b, a);
        assert_eq!(
            store
                .dispatch(Request {
                    id: b.clone(),
                    method: "outbox.change".into(),
                    params: json!({"workspace":"w","id":"a","action":"resume"})
                })
                .result
                .unwrap()["status"],
            "queued"
        );
        let next_a = write_id(&mut store, "change:w:a", &payload("pause"));
        assert_ne!(next_a, a);
        assert_ne!(next_a, b);
        assert_eq!(
            serde_json::to_value(store.dispatch(pause(&a))).unwrap(),
            serde_json::to_value(saved_a).unwrap()
        );
        assert_eq!(
            crate::outbox::list(&store.connection, "w").unwrap()[0].status,
            "queued",
            "old A receipt replay cannot undo B"
        );
        assert_eq!(
            store.dispatch(pause(&next_a)).result.unwrap()["status"],
            "paused"
        );
        let count: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM client_receipts WHERE request_id IN (?1,?2,?3)",
                params![a, b, next_a],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(count, 3);
    }

    #[test]
    fn native_write_id_clear_matches_only_the_current_group_request() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let a = write_id(&mut store, "change:w:a", &payload("pause"));
        let b = write_id(&mut store, "change:w:a", &payload("resume"));
        let sibling = write_id(&mut store, "change:other:a", &payload("resume"));
        assert_ne!(a, b);
        assert_ne!(b, sibling);
        assert_eq!(
            value(
                &mut store,
                "mutation.clear_write_id",
                json!({"key":"change:w:a","requestId":a})
            ),
            false
        );
        assert_eq!(write_id(&mut store, "change:w:a", &payload("resume")), b);
        assert_eq!(
            value(
                &mut store,
                "mutation.clear_write_id",
                json!({"key":"change:other:a","requestId":b})
            ),
            false
        );
        assert_eq!(
            write_id(&mut store, "change:other:a", &payload("resume")),
            sibling
        );
        assert_eq!(
            value(
                &mut store,
                "mutation.clear_write_id",
                json!({"key":"change:w:a","requestId":b})
            ),
            true
        );
        let fresh = write_id(&mut store, "change:w:a", &payload("resume"));
        assert_ne!(fresh, b);
        assert_eq!(
            value(
                &mut store,
                "mutation.clear_write_id",
                json!({"key":"change:w:a","requestId":b})
            ),
            false
        );
        assert_eq!(
            write_id(&mut store, "change:w:a", &payload("resume")),
            fresh
        );
    }

    #[test]
    fn native_write_id_keeps_the_existing_stable_identity_contract() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let stable = value(
            &mut store,
            "mutation.stable_id",
            json!({"key":"shared-key"}),
        );
        let write = write_id(&mut store, "shared-key", &payload("pause"));
        assert_ne!(stable, json!(write));
        assert_eq!(
            value(
                &mut store,
                "mutation.clear_write_id",
                json!({"key":"shared-key","requestId":write})
            ),
            true
        );
        assert_eq!(
            value(
                &mut store,
                "mutation.stable_id",
                json!({"key":"shared-key"})
            ),
            stable
        );
        assert_eq!(
            value(
                &mut store,
                "mutation.clear_stable_id",
                json!({"key":"shared-key","requestId":write})
            ),
            false
        );
        assert_eq!(
            value(
                &mut store,
                "mutation.clear_stable_id",
                json!({"key":"shared-key","requestId":stable})
            ),
            true
        );
    }

    #[test]
    fn native_write_id_invalid_envelope_cannot_replace_the_current_identity() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let original = write_id(&mut store, "change:w:a", &payload("pause"));
        for serialized in [
            "not JSON",
            "[]",
            "{}",
            "{\"method\":\"outbox.change\",\"params\":null}",
            "{\"method\":3,\"params\":{}}",
        ] {
            assert!(
                call(
                    &mut store,
                    "mutation.write_id",
                    json!({"key":"change:w:a","serializedPayload":serialized})
                )
                .error
                .is_some()
            );
            assert_eq!(
                write_id(&mut store, "change:w:a", &payload("pause")),
                original
            );
        }
    }
}
