//! Durable server projections. Sequence numbers include deletion tombstones.
use crate::protocol::{Request, Response};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::IgnoredAny};
use serde_json::{Value, json};
use std::collections::{BTreeMap, HashMap};
use studio_client_domain::{
    MAX_SAFE_INTEGER,
    sync_entity::{EntityChange, MAX_ENTITY_SCOPE_BYTES},
};

#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct Document {
    pub id: String,
    pub payload: String,
    pub seq: u64,
    #[serde(default, rename = "_deleted")]
    pub deleted: bool,
}

// Local entity cache format, independent of the server's sync protocol version.
const ENTITY_CACHE_VERSION: i64 = 4;
// Largest local projection key, measured in UTF-8 bytes.
const MAX_PROJECTION_SCOPE_BYTES: usize = MAX_ENTITY_SCOPE_BYTES;

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)?;
    transaction.execute_batch("CREATE TABLE IF NOT EXISTS client_projections (
        workspace TEXT NOT NULL, scope TEXT NOT NULL, document TEXT NOT NULL,
        seq INTEGER NOT NULL, PRIMARY KEY(workspace,scope));
        CREATE TABLE IF NOT EXISTS client_identity (singleton INTEGER PRIMARY KEY CHECK(singleton=1), value TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_projection_cache_version (
            singleton INTEGER PRIMARY KEY CHECK(singleton=1), version INTEGER NOT NULL);")?;
    let version: Option<i64> = transaction
        .query_row(
            "SELECT version FROM client_projection_cache_version WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if version != Some(ENTITY_CACHE_VERSION) {
        // Derived state and its cursors must be cleared together, for every workspace.
        // Keep transcripts, identity, drafts, receipts and other durable local data.
        transaction.execute("DELETE FROM client_projections WHERE scope LIKE 'entity:%' OR scope LIKE 'state:entities:%' OR scope='state'", [])?;
        transaction.execute("INSERT INTO client_projection_cache_version VALUES(1,?1) ON CONFLICT(singleton) DO UPDATE SET version=excluded.version", [ENTITY_CACHE_VERSION])?;
    }
    transaction.commit()
}

fn get(connection: &Connection, workspace: &str, scope: &str) -> Result<Option<Value>, String> {
    let encoded: Option<String> = connection
        .query_row(
            "SELECT document FROM client_projections WHERE workspace=?1 AND scope=?2",
            params![workspace, scope],
            |row| row.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    encoded
        .map(|value| serde_json::from_str(&value).map_err(|e| e.to_string()))
        .transpose()
}

fn entities(connection: &Connection, workspace: &str) -> Result<Vec<EntityChange>, String> {
    let mut statement = connection
        .prepare("SELECT document FROM client_projections WHERE workspace=?1 AND scope LIKE 'entity:%' ORDER BY scope")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([workspace], |row| row.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    rows.map(|row| {
        serde_json::from_str(&row.map_err(|e| e.to_string())?).map_err(|e| e.to_string())
    })
    .collect()
}

// Row namespace of the page's projection cache. Rows are opaque documents that
// the page owns; the prefix names the cache format (the API schema identity),
// so a wire change starts an empty cache and `pruneRows` drops the old one.
const ROW_NAMESPACE: &str = "rows:";
// Largest row id in UTF-8 bytes; the scope also carries the namespace and prefix.
const MAX_ROW_ID_BYTES: usize = 300;
// Largest cache prefix in bytes. The page uses the 64-character API schema hash.
const MAX_ROW_PREFIX_BYTES: usize = 128;
// Largest `getRows` request and `putRows` batch, in rows.
const MAX_ROW_BATCH: usize = 500;
// Largest `rangeRows` reply in bytes of row JSON. One native message holds it.
const MAX_ROWS_REPLY_BYTES: usize = crate::protocol::MAX_MESSAGE / 4;

// Invalid field types retain the historical missing-field behavior at each access.
fn optional_text<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<String>, D::Error> {
    Ok(Value::deserialize(d)?.as_str().map(str::to_owned))
}
fn optional_number<'de, D: serde::Deserializer<'de>>(d: D) -> Result<Option<u64>, D::Error> {
    Ok(Value::deserialize(d)?.as_u64())
}

#[derive(Default, Deserialize)]
struct ProjectionInput {
    #[serde(default, deserialize_with = "optional_text")]
    workspace: Option<String>,
    #[serde(default, deserialize_with = "optional_text")]
    scope: Option<String>,
    #[serde(default, deserialize_with = "optional_text")]
    prefix: Option<String>,
    #[serde(default, deserialize_with = "optional_text")]
    gte: Option<String>,
    #[serde(default, deserialize_with = "optional_text")]
    gt: Option<String>,
    #[serde(default, deserialize_with = "optional_text")]
    lt: Option<String>,
    #[serde(default, deserialize_with = "optional_number")]
    limit: Option<u64>,
}
#[derive(Default, Deserialize)]
struct RowIdentity {
    #[serde(default, deserialize_with = "optional_text")]
    id: Option<String>,
    #[serde(default, deserialize_with = "optional_number")]
    seq: Option<u64>,
}
fn row_prefix(params: &ProjectionInput) -> Result<&str, String> {
    let prefix = params.prefix.as_deref().ok_or("Missing row prefix.")?;
    if prefix.is_empty()
        || prefix.len() > MAX_ROW_PREFIX_BYTES
        || !prefix
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.-".contains(&byte))
    {
        return Err("Invalid row prefix.".into());
    }
    Ok(prefix)
}

#[derive(Default, Deserialize)]
struct IdentityInput {
    #[serde(default, rename = "workspaceId", deserialize_with = "optional_text")]
    workspace_id: Option<String>,
    #[serde(default, rename = "chatState", deserialize_with = "optional_bool")]
    chat_state: bool,
}
fn optional_bool<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
    Ok(Value::deserialize(d)?.as_bool().unwrap_or(false))
}
fn projection<'de, T: Deserialize<'de> + Default>(value: &'de Value) -> T {
    if value.is_object() {
        T::deserialize(value).unwrap_or_default()
    } else {
        T::default()
    }
}

fn row_scope(prefix: &str, id: &str) -> Result<String, String> {
    if id.is_empty() || id.len() > MAX_ROW_ID_BYTES {
        return Err("Invalid row identity.".into());
    }
    Ok(format!("{ROW_NAMESPACE}{prefix}/{id}"))
}

fn get_rows(
    connection: &Connection,
    workspace: &str,
    params: &ProjectionInput,
    raw: &Value,
) -> Result<Value, String> {
    let prefix = row_prefix(params)?;
    let ids = raw["ids"].as_array().ok_or("Missing row identities.")?;
    if ids.len() > MAX_ROW_BATCH {
        return Err("Too many row identities.".into());
    }
    let mut rows = Vec::new();
    for id in ids {
        let scope = row_scope(prefix, id.as_str().ok_or("Invalid row identity.")?)?;
        if let Some(row) = get(connection, workspace, &scope)? {
            rows.push(row);
        }
    }
    Ok(Value::Array(rows))
}

/// Rows with `gte` (or `gt`) <= id < `lt`, in id order. A reply that reaches
/// `limit` or the byte bound sets `more`; the caller continues after its last id.
fn range_rows(
    connection: &Connection,
    workspace: &str,
    params: &ProjectionInput,
) -> Result<Value, String> {
    let prefix = row_prefix(params)?;
    let base = format!("{ROW_NAMESPACE}{prefix}/");
    let (lower, inclusive) = match (params.gte.as_deref(), params.gt.as_deref()) {
        (Some(id), None) => (id, true),
        (None, Some(id)) => (id, false),
        _ => return Err("A range needs exactly one of gte and gt.".into()),
    };
    let upper = params.lt.as_deref().ok_or("A range needs lt.")?;
    let limit = params.limit.map_or(usize::MAX, |limit| limit as usize);
    let mut statement = connection
        .prepare_cached(&format!(
            "SELECT document FROM client_projections WHERE workspace=?1 AND scope{}?2 AND scope<?3 ORDER BY scope",
            if inclusive { ">=" } else { ">" }
        ))
        .map_err(|e| e.to_string())?;
    let mut cursor = statement
        .query(params![
            workspace,
            format!("{base}{lower}"),
            format!("{base}{upper}")
        ])
        .map_err(|e| e.to_string())?;
    let (mut rows, mut bytes, mut more) = (Vec::new(), 0, false);
    while let Some(row) = cursor.next().map_err(|e| e.to_string())? {
        let encoded: String = row.get(0).map_err(|e| e.to_string())?;
        if rows.len() >= limit || (!rows.is_empty() && bytes + encoded.len() > MAX_ROWS_REPLY_BYTES)
        {
            more = true;
            break;
        }
        bytes += encoded.len();
        rows.push(serde_json::from_str::<Value>(&encoded).map_err(|e| e.to_string())?);
    }
    Ok(json!({"rows":rows,"more":more}))
}

/// Store rows unconditionally in one transaction. Ordering by server sequence
/// stays the caller's rule: a reset must be able to lower a row's sequence.
fn put_rows(
    connection: &Connection,
    workspace: &str,
    params: &ProjectionInput,
    raw: &Value,
) -> Result<Value, String> {
    let prefix = row_prefix(params)?;
    let rows = raw["rows"].as_array().ok_or("Missing rows.")?;
    if rows.len() > MAX_ROW_BATCH {
        return Err("Too many rows.".into());
    }
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    for row in rows {
        let identity: RowIdentity = if row.is_object() {
            RowIdentity::deserialize(row).unwrap_or_default()
        } else {
            RowIdentity::default()
        };
        let scope = row_scope(
            prefix,
            identity.id.as_deref().ok_or("Invalid row identity.")?,
        )?;
        let seq = identity
            .seq
            .filter(|seq| *seq <= MAX_SAFE_INTEGER as u64)
            .ok_or("Invalid row sequence.")?;
        transaction
            .execute(
                "INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,?2,?3,?4)
            ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq",
                params![workspace, scope, row.to_string(), seq as i64],
            )
            .map_err(|e| e.to_string())?;
    }
    transaction.commit().map_err(|e| e.to_string())?;
    Ok(json!({"written":rows.len()}))
}

/// Drop every row cache whose prefix is not `prefix` (older formats).
fn prune_rows(
    connection: &Connection,
    workspace: &str,
    params: &ProjectionInput,
) -> Result<Value, String> {
    let prefix = row_prefix(params)?;
    // ';' follows ':' and '0' follows '/': the bounds enclose the namespace and the kept prefix.
    let removed = connection
        .execute(
            "DELETE FROM client_projections WHERE workspace=?1 AND scope>='rows:' AND scope<'rows;'
             AND NOT (scope>=?2 AND scope<?3)",
            params![
                workspace,
                format!("{ROW_NAMESPACE}{prefix}/"),
                format!("{ROW_NAMESPACE}{prefix}0")
            ],
        )
        .map_err(|e| e.to_string())?;
    Ok(json!({"removed":removed}))
}

pub fn valid_workspace(workspace: &str) -> bool {
    workspace.len() == 32
        && workspace
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn entity_snapshot(rows: &[EntityChange]) -> Value {
    let mut collections: HashMap<String, BTreeMap<String, Value>> = HashMap::new();
    for doc in rows {
        if let Some(value) = doc.value() {
            collections
                .entry(doc.collection().into())
                .or_default()
                .insert(doc.id().into(), Value::Object(value.clone()));
        }
    }
    let list = |name: &str| {
        Value::Array(
            collections
                .get(name)
                .into_iter()
                .flat_map(|v| v.values().cloned())
                .collect(),
        )
    };
    let agents = if rows.iter().any(|row| row.collection() == "agentSummary") {
        let summaries = collections.get("agentSummary").cloned().unwrap_or_default();
        Value::Array(
            summaries
                .iter()
                .map(|(id, summary)| {
                    let mut agent = collections
                        .get("agentDetail")
                        .and_then(|details| details.get(id))
                        .and_then(Value::as_object)
                        .cloned()
                        .unwrap_or_default();
                    if let Some(fields) = summary.as_object() {
                        agent.extend(fields.clone());
                    }
                    Value::Object(agent)
                })
                .collect(),
        )
    } else {
        list("agent")
    };
    let chats = list("chat");
    let mut nodes = agents.as_array().cloned().unwrap_or_default();
    nodes.extend(chats.as_array().cloned().unwrap_or_default());
    let mut runtime = json!({"agents":agents,"rooms":list("room"),"tasks":list("task"),"monitors":list("monitor"),"complaints":list("complaint"),"requests":list("request"),"rules":list("rule"),"projects":list("project"),"peerTeams":list("peerTeam"),"events":list("event"),"work":list("work")});
    let workspace_entity = collections
        .get("workspace")
        .and_then(|v| v.get("current"))
        .cloned()
        .unwrap_or_else(|| json!({}));
    if let (Some(target), Some(extra)) = (runtime.as_object_mut(), workspace_entity.as_object()) {
        target.extend(extra.clone());
    }
    let mut errors = list("studioError");
    if let Some(rows) = errors.as_array_mut() {
        rows.sort_by(|a, b| {
            b["at"]
                .as_f64()
                .unwrap_or(0.0)
                .total_cmp(&a["at"].as_f64().unwrap_or(0.0))
                .then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });
    }
    runtime["studioErrors"] = errors;
    let edges = list("edge");
    for (collection, field) in [("accounts", "accountsSummary"), ("desktop", "desktop")] {
        runtime[field] = collections
            .get(collection)
            .and_then(|rows| rows.get("current"))
            .cloned()
            .unwrap_or(Value::Null);
    }
    runtime["terminals"] = list("terminal");
    for (collection, field) in [
        ("accountCost", "accountCostsByAccount"),
        ("sessionCost", "sessionCostsByAgent"),
    ] {
        runtime[field] = Value::Object(
            collections
                .remove(collection)
                .unwrap_or_default()
                .into_iter()
                .collect(),
        );
    }
    runtime["queuesByAgent"] = Value::Object(
        collections
            .remove("queue")
            .unwrap_or_default()
            .into_iter()
            .collect(),
    );
    let limits = collections.remove("rateLimit").unwrap_or_default();
    runtime["rateLimits"] = limits.get("default").cloned().unwrap_or(Value::Null);
    runtime["rateLimitsByAccount"] = Value::Object(limits.into_iter().collect());
    let state_dir = workspace_entity
        .get("stateDir")
        .cloned()
        .unwrap_or_else(|| json!(""));
    json!({"token":"","stateDir":state_dir,"threads":agents,"chats":chats,"nodes":nodes,"edges":edges,"runtime":runtime})
}

pub(crate) fn serialize_state(snapshot: &Value) -> Result<String, crate::protocol::CommandError> {
    let payload = snapshot.to_string();
    match crate::sync::projection_error(&payload) {
        Some(failure) => Err(failure),
        None => Ok(payload),
    }
}

// One store command owns validation, entity writes, cursors and derived state.
// The store worker serializes commands; the transaction also prevents readers
// from observing a partially applied batch or a state built from older rows.
#[derive(Clone, Copy, Default)]
enum CursorArgument {
    #[default]
    Missing,
    Invalid,
    Value(u64),
}
impl CursorArgument {
    fn from_checked(value: Option<u64>) -> Self {
        value.map_or(Self::Missing, Self::Value)
    }
}

impl<'de> Deserialize<'de> for CursorArgument {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        Ok(Value::deserialize(d)?
            .as_u64()
            .map_or(Self::Invalid, Self::Value))
    }
}
#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntityOptions {
    #[serde(default, deserialize_with = "optional_bool")]
    replace: bool,
    #[serde(default)]
    checkpoint: CursorArgument,
    #[serde(default)]
    expected_revision: CursorArgument,
}

fn apply_entities(
    connection: &Connection,
    workspace: &str,
    params: &Value,
) -> Result<Value, String> {
    let changes =
        Vec::<EntityChange>::deserialize(&params["changes"]).map_err(|e| e.to_string())?;
    let options: EntityOptions = projection(params);
    apply_entities_inner(
        connection,
        workspace,
        &changes,
        options.replace,
        options.checkpoint,
        options.expected_revision,
    )
}

pub(crate) fn apply_entity_batch(
    connection: &Connection,
    workspace: &str,
    changes: &[EntityChange],
    replace: bool,
    checkpoint: Option<u64>,
    expected_revision: Option<u64>,
) -> Result<Value, String> {
    if !valid_workspace(workspace) {
        return Err("Invalid projection identity.".into());
    }
    apply_entities_inner(
        connection,
        workspace,
        changes,
        replace,
        CursorArgument::from_checked(checkpoint),
        CursorArgument::from_checked(expected_revision),
    )
}

fn apply_entities_inner(
    connection: &Connection,
    workspace: &str,
    changes: &[EntityChange],
    replace: bool,
    checkpoint: CursorArgument,
    expected_revision: CursorArgument,
) -> Result<Value, String> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    let previous = get(&transaction, workspace, "state")?.unwrap_or(Value::Null);
    // This is a local materialization revision, never a server cursor. It also
    // fences HTTP reads started before a newer application or reset committed.
    let revision = previous["seq"].as_u64().unwrap_or(0);
    match expected_revision {
        CursorArgument::Missing => {}
        CursorArgument::Value(expected) if expected == revision => {}
        CursorArgument::Invalid | CursorArgument::Value(_) => {
            return Err("Entity state changed during synchronization. Retry the pull.".into());
        }
    }
    if replace {
        transaction.execute("DELETE FROM client_projections WHERE workspace=?1 AND (scope LIKE 'entity:%' OR scope LIKE 'state:entities:%')", [workspace]).map_err(|e| e.to_string())?;
    }
    let mut changed = false;
    {
        let mut insert = transaction
            .prepare_cached(
                "INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,?2,?3,?4)
            ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq
            WHERE excluded.seq>client_projections.seq",
            )
            .map_err(|e| e.to_string())?;
        for change in changes {
            let scope = change.scope();
            changed |= insert
                .execute(params![
                    workspace,
                    scope,
                    serde_json::to_string(change).map_err(|e| e.to_string())?,
                    change.seq() as i64
                ])
                .map_err(|e| e.to_string())?
                != 0;
        }
    }
    if !matches!(checkpoint, CursorArgument::Missing) {
        let seq = match checkpoint {
            CursorArgument::Value(seq) if seq <= MAX_SAFE_INTEGER as u64 => seq,
            CursorArgument::Missing | CursorArgument::Invalid | CursorArgument::Value(_) => {
                return Err("Invalid entity checkpoint.".into());
            }
        };
        for (scope, payload) in [
            ("state:entities:checkpoint", "{}"),
            ("state:entities:ready", "\"ready\""),
        ] {
            let document = Document {
                id: scope.into(),
                payload: payload.into(),
                seq,
                deleted: false,
            };
            transaction.execute("INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,?2,?3,?4)
                ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq
                WHERE excluded.seq>client_projections.seq", params![workspace, scope, serde_json::to_string(&document).map_err(|e| e.to_string())?, seq as i64]).map_err(|e| e.to_string())?;
        }
        transaction.execute("DELETE FROM client_projections WHERE workspace=?1 AND scope='state:entities:initial'", [workspace]).map_err(|e| e.to_string())?;
    }
    let result = if changed || replace || previous.is_null() {
        let seq = revision
            .checked_add(1)
            .filter(|seq| *seq <= MAX_SAFE_INTEGER as u64)
            .ok_or("Entity state revision exhausted.")?;
        let payload = serialize_state(&entity_snapshot(&entities(&transaction, workspace)?))
            .map_err(|e| e.message)?;
        let document = Document {
            id: "state".into(),
            payload,
            seq,
            deleted: false,
        };
        let encoded = serde_json::to_string(&document).map_err(|e| e.to_string())?;
        transaction.execute("INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,'state',?2,?3)
            ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq", params![workspace, encoded, seq as i64]).map_err(|e| e.to_string())?;
        serde_json::to_value(document).map_err(|e| e.to_string())?
    } else {
        previous
    };
    transaction.commit().map_err(|e| e.to_string())?;
    Ok(result)
}

/// A document checked against its local scope and payload constraints.
pub struct CheckedDocument(Document);

pub(crate) fn check_document(scope: &str, document: Document) -> Result<CheckedDocument, String> {
    if document.id != scope || document.seq > MAX_SAFE_INTEGER as u64 {
        return Err("Invalid projection document.".into());
    }
    if !document.deleted {
        serde_json::from_str::<IgnoredAny>(&document.payload).map_err(|e| e.to_string())?;
    }
    Ok(CheckedDocument(document))
}

fn write_document(
    connection: &Connection,
    workspace: &str,
    document: &CheckedDocument,
) -> Result<(), String> {
    let document = &document.0;
    connection
        .execute(
            "INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,?2,?3,?4)
        ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq
        WHERE excluded.seq>client_projections.seq",
            params![
                workspace,
                document.id,
                serde_json::to_string(document).map_err(|e| e.to_string())?,
                document.seq as i64
            ],
        )
        .map_err(|e| e.to_string())?;
    Ok(())
}

pub(crate) fn put_documents(
    connection: &Connection,
    workspace: &str,
    scope: &str,
    documents: &[CheckedDocument],
) -> Result<Value, String> {
    if !valid_workspace(workspace) || scope.is_empty() || scope.len() > MAX_PROJECTION_SCOPE_BYTES {
        return Err("Invalid projection identity.".into());
    }
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|e| e.to_string())?;
    for document in documents {
        if document.0.id != scope {
            return Err("Invalid projection document.".into());
        }
        write_document(&transaction, workspace, document)?;
    }
    let value = get(&transaction, workspace, scope)?.unwrap_or(Value::Null);
    transaction.commit().map_err(|e| e.to_string())?;
    Ok(value)
}

fn execute(connection: &Connection, request: &Request) -> Result<Value, String> {
    if request.method == "identity.get" {
        let value: Option<String> = connection
            .query_row(
                "SELECT value FROM client_identity WHERE singleton=1",
                [],
                |row| row.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        return value
            .map(|value| serde_json::from_str(&value).map_err(|e| e.to_string()))
            .transpose()
            .map(|value| value.unwrap_or(Value::Null));
    }
    if request.method == "identity.set" {
        let input: IdentityInput = projection(&request.params);
        let workspace = input
            .workspace_id
            .as_deref()
            .ok_or("Missing workspace identity.")?;
        if !valid_workspace(workspace) {
            return Err("Invalid workspace identity.".into());
        }
        let identity = json!({"workspaceId":workspace,"chatState":input.chat_state});
        // Bind the store atomically, including concurrent opens. A changed
        // backend must not silently replace the identity used for offline data.
        let changed = connection
            .execute(
                "INSERT INTO client_identity VALUES(1,?1)
            ON CONFLICT(singleton) DO UPDATE SET value=excluded.value
            WHERE json_extract(client_identity.value, '$.workspaceId') = ?2",
                params![identity.to_string(), workspace],
            )
            .map_err(|e| e.to_string())?;
        if changed == 0 {
            return Err(
                "The server workspace changed. Use a separate client store for the new workspace."
                    .into(),
            );
        }
        return Ok(identity);
    }
    let input: ProjectionInput = projection(&request.params);
    let workspace = input
        .workspace
        .as_deref()
        .ok_or("Missing workspace identity.")?;
    if !valid_workspace(workspace) {
        return Err("Invalid projection identity.".into());
    }
    if matches!(
        request.method.as_str(),
        "projection.getRows"
            | "projection.rangeRows"
            | "projection.putRows"
            | "projection.pruneRows"
    ) {
        return match request.method.as_str() {
            "projection.getRows" => get_rows(connection, workspace, &input, &request.params),
            "projection.rangeRows" => range_rows(connection, workspace, &input),
            "projection.putRows" => put_rows(connection, workspace, &input, &request.params),
            "projection.pruneRows" => prune_rows(connection, workspace, &input),
            _ => unreachable!(),
        };
    }
    if request.method == "projection.applyEntities" {
        return apply_entities(connection, workspace, &request.params);
    }
    if request.method == "projection.putEntity" {
        let change =
            EntityChange::deserialize(&request.params["change"]).map_err(|e| e.to_string())?;
        let scope = change.scope();
        let seq = change.seq();
        let changed = connection
            .execute(
                "INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,?2,?3,?4)
            ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq
            WHERE excluded.seq>client_projections.seq",
                params![
                    workspace,
                    scope,
                    serde_json::to_string(&change).map_err(|e| e.to_string())?,
                    seq as i64
                ],
            )
            .map_err(|error| error.to_string())?;
        return Ok(Value::Bool(changed != 0));
    }
    let scope = input.scope.as_deref().ok_or("Missing projection scope.")?;
    if scope.is_empty() || scope.len() > MAX_PROJECTION_SCOPE_BYTES {
        return Err("Invalid projection identity.".into());
    }
    match request.method.as_str() {
        "projection.entities" => {
            serde_json::to_value(entities(connection, workspace)?).map_err(|e| e.to_string())
        }
        "projection.get" => Ok(get(connection, workspace, scope)?.unwrap_or(Value::Null)),
        "projection.put" => {
            let document: Document =
                Document::deserialize(&request.params["document"]).map_err(|e| e.to_string())?;
            let document = check_document(scope, document)?;
            write_document(connection, workspace, &document)?;
            serde_json::to_value(get(connection, workspace, scope)?).map_err(|e| e.to_string())
        }
        _ => Err("Unknown projection command.".into()),
    }
}

pub fn dispatch(connection: &Connection, request: &Request) -> Response {
    match execute(connection, request) {
        Ok(value) => Response::success(request.id.clone(), value),
        Err(error) => Response::failure(request.id.clone(), "projection_error", error),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn projection_sender_preserves_validation_order_and_rollback_after_restart() {
        use crate::delivery::StoreCommand;
        use tokio::sync::{mpsc, oneshot};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let worker_path = path.clone();
        let (sender, mut receiver) = mpsc::channel::<StoreCommand>(8);
        let worker = std::thread::spawn(move || {
            let mut store = crate::store::Store::open(&worker_path).unwrap();
            while let Some(command) = receiver.blocking_recv() {
                command.execute(&mut store);
            }
        });
        async fn send(
            sender: &mpsc::Sender<StoreCommand>,
            method: &str,
            params: Value,
        ) -> Response {
            let (reply, receive) = oneshot::channel();
            sender
                .send(StoreCommand::Request(
                    Request {
                        id: "fixture".into(),
                        method: method.into(),
                        params,
                    },
                    reply,
                ))
                .await
                .unwrap();
            receive.await.unwrap()
        }
        let workspace = "a".repeat(32);
        let bad = send(
            &sender,
            "projection.applyEntities",
            json!({"workspace":"bad","changes":null,"checkpoint":null}),
        )
        .await;
        assert_eq!(bad.error.unwrap().message, "Invalid projection identity.");
        let bad = send(
            &sender,
            "projection.unknown",
            json!({"workspace":workspace}),
        )
        .await;
        assert_eq!(bad.error.unwrap().message, "Missing projection scope.");
        let bad = send(
            &sender,
            "projection.applyEntities",
            json!({"workspace":workspace,"changes":null,"expectedRevision":null}),
        )
        .await;
        assert_ne!(
            bad.error.unwrap().message,
            "Entity state changed during synchronization. Retry the pull."
        );
        let change = json!({"op":"upsert","collection":"task","id":"a","seq":1,"value":{"id":"a"}});
        let bad = send(
            &sender,
            "projection.applyEntities",
            json!({"workspace":workspace,"changes":[change],"replace":true,"checkpoint":1.0}),
        )
        .await;
        assert_eq!(bad.error.unwrap().message, "Invalid entity checkpoint.");
        drop(sender);
        worker.join().unwrap();
        let mut store = crate::store::Store::open(&path).unwrap();
        let result = store.dispatch(Request {
            id: "get".into(),
            method: "projection.entities".into(),
            params: json!({"workspace":workspace,"scope":"entity:*"}),
        });
        assert_eq!(result.result, Some(json!([])));
    }

    #[test]
    fn borrowed_document_decoding_keeps_old_errors() {
        for value in [
            json!(null),
            json!([]),
            json!({}),
            json!({"id":3}),
            json!({"id":"x","payload":"{}","seq":1.0}),
        ] {
            let old = serde_json::from_value::<Document>(value.clone()).map_err(|e| e.to_string());
            let new = Document::deserialize(&value).map_err(|e| e.to_string());
            assert_eq!(old.err(), new.err(), "{value}");
        }
    }
    #[test]
    fn agent_summary_renders_without_detail_and_owns_list_fields() {
        let summary: EntityChange = serde_json::from_str(r#"{"op":"upsert","collection":"agentSummary","id":"a","seq":0,"value":{"id":"a","name":"Current","status":"completed","lastCompletedTurn":"turn","readState":{"threadId":"thread","turnId":"turn","read":false,"revision":1}}}"#).expect("summary");
        let snapshot = entity_snapshot(std::slice::from_ref(&summary));
        assert_eq!(snapshot["threads"][0]["name"], "Current");
        assert!(snapshot["threads"][0].get("lastAnswer").is_none());
        let detail: EntityChange = serde_json::from_str(r#"{"op":"upsert","collection":"agentDetail","id":"a","seq":0,"value":{"id":"a","lastAnswer":"Answer"}}"#).expect("detail");
        let snapshot = entity_snapshot(&[summary, detail]);
        assert_eq!(snapshot["threads"][0]["lastAnswer"], "Answer");
        assert_eq!(snapshot["threads"][0]["name"], "Current");
    }

    #[test]
    fn studio_error_entities_reach_native_state_and_dismissal_removes_them() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        let workspace = "a".repeat(32);
        let apply = |changes| Request {
            id: "studio-errors".into(),
            method: "projection.applyEntities".into(),
            params: json!({"workspace":workspace,"changes":changes,"replace":false}),
        };
        let item = json!({"id":"event:one","agentId":"one","at":100});
        let first = store.dispatch(apply(json!([
            {"op":"upsert","collection":"studioError","id":"event:one","seq":1,"value":item}
        ])));
        assert!(first.error.is_none(), "{:?}", first.error);
        let snapshot: Value =
            serde_json::from_str(first.result.unwrap()["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["runtime"]["studioErrors"], json!([item]));
        let removed = store.dispatch(apply(json!([
            {"op":"delete","collection":"studioError","id":"event:one","seq":2}
        ])));
        assert!(removed.error.is_none(), "{:?}", removed.error);
        let snapshot: Value =
            serde_json::from_str(removed.result.unwrap()["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["runtime"]["studioErrors"], json!([]));
    }

    #[test]
    fn document_batch_rolls_back_earlier_writes_on_later_sql_failure() {
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        let workspace = "a".repeat(32);
        store.connection.execute_batch("CREATE TEMP TRIGGER reject_second_document BEFORE UPDATE ON client_projections WHEN NEW.seq=2 BEGIN SELECT RAISE(ABORT, 'injected failure'); END;").unwrap();
        let documents = [
            Document {
                id: "transcript:a".into(),
                payload: "{}".into(),
                seq: 1,
                deleted: false,
            },
            Document {
                id: "transcript:a".into(),
                payload: "{}".into(),
                seq: 2,
                deleted: false,
            },
        ];
        let documents = documents
            .into_iter()
            .map(|document| check_document("transcript:a", document).unwrap())
            .collect::<Vec<_>>();
        assert!(put_documents(&store.connection, &workspace, "transcript:a", &documents).is_err());
        assert!(
            get(&store.connection, &workspace, "transcript:a")
                .unwrap()
                .is_none()
        );
    }

    #[test]
    fn entity_batch_rolls_back_rows_cursors_and_state_on_failure() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        let batch = |changes, replace, checkpoint, expected| Request {
            id: "batch".into(),
            method: "projection.applyEntities".into(),
            params: json!({"workspace":workspace,"changes":changes,"replace":replace,"checkpoint":checkpoint,"expectedRevision":expected}),
        };
        let a = json!({"op":"upsert","collection":"task","id":"A","seq":5,"value":{"id":"A"}});
        let mut store = crate::store::Store::open(&path).unwrap();
        let first = store.dispatch(batch(json!([a]), false, 5, 0));
        assert!(first.error.is_none(), "{:?}", first.error);
        let original = first.result.unwrap();
        let b = json!({"op":"upsert","collection":"task","id":"B","seq":8,"value":{"id":"B"}});
        let invalid =
            json!({"op":"upsert","collection":"task","id":"bad","seq":9,"value":"invalid"});
        let failed = store.dispatch(batch(json!([b, invalid]), true, 9, 1));
        assert!(failed.error.is_some());
        // Fail after the rows and cursors have been written, at materialization.
        store.connection.execute_batch("CREATE TEMP TRIGGER reject_state BEFORE UPDATE ON client_projections WHEN NEW.scope='state' BEGIN SELECT RAISE(ABORT, 'injected state failure'); END;").unwrap();
        let failed = store.dispatch(batch(json!([b]), true, 9, 1));
        assert!(failed.error.is_some());
        drop(store);
        let store = crate::store::Store::open(&path).unwrap();
        assert_eq!(
            serde_json::to_value(entities(&store.connection, &workspace).unwrap()).unwrap(),
            json!([a])
        );
        assert_eq!(
            get(&store.connection, &workspace, "state").unwrap(),
            Some(original)
        );
        assert_eq!(
            get(&store.connection, &workspace, "state:entities:checkpoint")
                .unwrap()
                .unwrap()["seq"],
            5
        );
        assert_eq!(
            get(&store.connection, &workspace, "state:entities:ready")
                .unwrap()
                .unwrap()["seq"],
            5
        );
    }

    #[test]
    fn entity_batch_preserves_missing_invalid_and_revision_error_order() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::store::Store::open(&dir.path().join("client.sqlite3")).unwrap();
        let workspace = "a".repeat(32);
        let call = |params: Value| Request {
            id: "batch".into(),
            method: "projection.applyEntities".into(),
            params,
        };
        let base = json!({"workspace":workspace,"changes":[]});
        assert!(store.dispatch(call(base.clone())).error.is_none());
        let mut both = base.clone();
        both["expectedRevision"] = Value::Null;
        both["checkpoint"] = Value::Null;
        let failure = store.dispatch(call(both)).error.unwrap();
        assert_eq!(
            failure.message,
            "Entity state changed during synchronization. Retry the pull."
        );
        let mut checkpoint = base;
        checkpoint["checkpoint"] = Value::Null;
        let failure = store.dispatch(call(checkpoint)).error.unwrap();
        assert_eq!(failure.message, "Invalid entity checkpoint.");
    }

    #[test]
    fn entity_batch_revision_fences_late_pulls_and_reset_replays() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        let batch = |changes, replace, checkpoint, expected| Request {
            id: "batch".into(),
            method: "projection.applyEntities".into(),
            params: json!({"workspace":workspace,"changes":changes,"replace":replace,"checkpoint":checkpoint,"expectedRevision":expected}),
        };
        let a = json!({"op":"upsert","collection":"task","id":"A","seq":11,"value":{"id":"A"}});
        let mut store = crate::store::Store::open(&path).unwrap();
        let first = store
            .dispatch(batch(json!([a]), false, 11, 0))
            .result
            .unwrap();
        assert_eq!(first["seq"], 1, "state uses a local revision");
        // Repeating a completed interval does not publish another revision.
        assert_eq!(
            store.dispatch(batch(json!([a]), false, 11, 1)).result,
            Some(first)
        );
        let b = json!({"op":"upsert","collection":"task","id":"B","seq":10,"value":{"id":"B"}});
        assert_eq!(
            store
                .dispatch(batch(json!([b]), false, 11, 1))
                .result
                .unwrap()["seq"],
            2
        );
        assert!(store.dispatch(batch(json!([]), true, 0, 1)).error.is_some());
        assert_eq!(entities(&store.connection, &workspace).unwrap().len(), 2);
        drop(store);
        let mut store = crate::store::Store::open(&path).unwrap();
        let reset = store.dispatch(batch(json!([]), true, 0, 2)).result.unwrap();
        assert_eq!(reset["seq"], 3, "reset cannot roll back the local revision");
        assert!(entities(&store.connection, &workspace).unwrap().is_empty());
        assert_eq!(
            get(&store.connection, &workspace, "state:entities:checkpoint")
                .unwrap()
                .unwrap()["seq"],
            0
        );
        assert!(
            store
                .dispatch(batch(json!([a]), false, 11, 2))
                .error
                .is_some()
        );
        assert!(entities(&store.connection, &workspace).unwrap().is_empty());
        let snapshot: Value = serde_json::from_str(reset["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["runtime"]["tasks"], json!([]));
    }

    #[test]
    fn unknown_collection_and_opaque_values_roundtrip_through_cache() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        let change = json!({"op":"upsert","collection":"future","id":"x:y","seq":3,"value":{"nested":[null,{"draft":{"text":"same","number":1.5}}]},"vendor":{"flag":true}});
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            let result = store.dispatch(Request {
                id: "put".into(),
                method: "projection.putEntity".into(),
                params: json!({"workspace":workspace,"change":change}),
            });
            assert!(result.error.is_none(), "{:?}", result.error);
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        let result = store.dispatch(Request {
            id: "get".into(),
            method: "projection.get".into(),
            params: json!({"workspace":workspace,"scope":"entity:future:x:y"}),
        });
        assert_eq!(result.result, Some(change));
    }

    #[test]
    fn entity_v4_upsert_delete_and_stale_replay_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        let put = |change: Value| Request {
            id: "entity".into(),
            method: "projection.putEntity".into(),
            params: json!({"workspace":workspace,"change":change}),
        };
        let upsert = json!({"op":"upsert","collection":"agent","id":"one","seq":10,"value":{"id":"one","name":"First"}});
        let deleted = json!({"op":"delete","collection":"agent","id":"one","seq":12});
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            let applied = store.dispatch(put(upsert.clone()));
            assert!(applied.error.is_none(), "{:?}", applied.error);
            assert_eq!(applied.result, Some(json!(true)));
            assert_eq!(
                store.dispatch(put(upsert.clone())).result,
                Some(json!(false))
            );
            assert_eq!(
                store.dispatch(put(deleted.clone())).result,
                Some(json!(true))
            );
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        let mut stale = upsert.clone();
        stale["seq"] = json!(11);
        assert_eq!(store.dispatch(put(stale)).result, Some(json!(false)));
        let read = store.dispatch(Request {
            id: "read".into(),
            method: "projection.get".into(),
            params: json!({"workspace":workspace,"scope":"entity:agent:one"}),
        });
        assert_eq!(read.result, Some(deleted));
        let mut fresh = upsert;
        fresh["seq"] = json!(13);
        assert_eq!(store.dispatch(put(fresh.clone())).result, Some(json!(true)));
        let rows = store.dispatch(Request {
            id: "read".into(),
            method: "projection.entities".into(),
            params: json!({"workspace":workspace,"scope":"entity:*"}),
        });
        assert_eq!(rows.result, Some(json!([fresh])));
    }

    #[test]
    fn entity_v4_rejects_legacy_and_malformed_changes_without_writing() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        let workspace = "c".repeat(32);
        for change in [
            json!({"id":"entity:agent:one","seq":1,"payload":"{}"}),
            json!({"op":"upsert","collection":"agent","id":"one","seq":1,"value":"{}"}),
            json!({"op":"delete","collection":"agent","id":"one","seq":1,"value":{}}),
            json!({"op":"replace","collection":"agent","id":"one","seq":1,"value":{}}),
            json!({"op":"delete","collection":"agent","id":"","seq":1}),
            json!({"op":"delete","collection":"agent","id":"one","seq":MAX_SAFE_INTEGER as u64 + 1}),
        ] {
            let response = store.dispatch(Request {
                id: "invalid".into(),
                method: "projection.putEntity".into(),
                params: json!({"workspace":workspace,"change":change}),
            });
            assert!(
                response.error.is_some(),
                "accepted invalid change: {change}"
            );
        }
        let rows = store.dispatch(Request {
            id: "read".into(),
            method: "projection.entities".into(),
            params: json!({"workspace":workspace,"scope":"entity:*"}),
        });
        assert_eq!(rows.result, Some(json!([])));
    }

    #[test]
    fn entity_v4_discards_old_cache_and_cursors_but_preserves_other_local_data() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            for scope in [
                "entity:agent:one",
                "state",
                "state:entities:checkpoint",
                "state:entities:initial",
                "state:entities:ready",
                "transcript:one",
            ] {
                let result = store.dispatch(Request {
                    id: "seed".into(), method: "projection.put".into(),
                    params: json!({"workspace":workspace,"scope":scope,"document":{"id":scope,"seq":99,"payload":"{}"}}),
                });
                assert!(result.error.is_none());
            }
            assert!(
                store
                    .dispatch(Request {
                        id: "identity".into(),
                        method: "identity.set".into(),
                        params: json!({"workspaceId":workspace,"chatState":true}),
                    })
                    .error
                    .is_none()
            );
            // An unversioned database is the format shipped before entity v4.
            store
                .connection
                .execute_batch("DROP TABLE IF EXISTS client_projection_cache_version")
                .unwrap();
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        for scope in [
            "entity:agent:one",
            "state",
            "state:entities:checkpoint",
            "state:entities:initial",
            "state:entities:ready",
        ] {
            let result = store.dispatch(Request {
                id: "read".into(),
                method: "projection.get".into(),
                params: json!({"workspace":workspace,"scope":scope}),
            });
            assert_eq!(
                result.result,
                Some(Value::Null),
                "old cache retained: {scope}"
            );
        }
        assert!(
            store
                .dispatch(Request {
                    id: "read".into(),
                    method: "projection.get".into(),
                    params: json!({"workspace":workspace,"scope":"transcript:one"}),
                })
                .result
                .unwrap()
                .is_object()
        );
        assert_eq!(
            store
                .dispatch(Request {
                    id: "identity".into(),
                    method: "identity.get".into(),
                    params: Value::Null,
                })
                .result
                .unwrap()["workspaceId"],
            workspace
        );
    }

    #[test]
    fn workspace_rebind_is_rejected_without_changing_durable_identity() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let set = |workspace: &str, chat_state| Request {
            id: "identity".into(),
            method: "identity.set".into(),
            params: json!({"workspaceId":workspace.repeat(32),"chatState":chat_state}),
        };
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            assert!(store.dispatch(set("a", false)).error.is_none());
            assert!(store.dispatch(set("a", true)).error.is_none());
            assert!(store.dispatch(set("b", false)).error.is_some());
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        assert!(store.dispatch(set("b", false)).error.is_some());
        let result = store.dispatch(Request {
            id: "read".into(),
            method: "identity.get".into(),
            params: Value::Null,
        });
        assert_eq!(
            result.result,
            Some(json!({"workspaceId":"a".repeat(32),"chatState":true}))
        );
    }

    #[test]
    fn tombstone_and_workspace_survive_late_replies_and_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let workspace = "a".repeat(32);
        let put = |seq, deleted, payload: &str| Request {
            id: "put".into(),
            method: "projection.put".into(),
            params: json!({"workspace":workspace,"scope":"transcript:one","document":{"id":"transcript:one","seq":seq,"payload":payload,"_deleted":deleted}}),
        };
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            assert!(
                store
                    .dispatch(put(10, false, "{\"items\":[]}"))
                    .error
                    .is_none()
            );
            assert!(store.dispatch(put(12, true, "")).error.is_none());
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        let late = store.dispatch(put(11, false, "{\"items\":[1]}"));
        assert_eq!(late.result.unwrap()["_deleted"], true);
        let other = store.dispatch(Request {
            id: "get".into(),
            method: "projection.get".into(),
            params: json!({"workspace":"b".repeat(32),"scope":"transcript:one"}),
        });
        assert_eq!(other.result, Some(Value::Null));
        let mut wrong = put(13, false, "{}");
        wrong.params["document"]["id"] = json!("transcript:other");
        assert!(store.dispatch(wrong).error.is_some());
        assert!(
            store
                .dispatch(put(14, false, "{\"unused\":1e400}"))
                .error
                .is_none()
        );
        assert!(
            store
                .dispatch(put(15, false, "{\"unused\":"))
                .error
                .is_some()
        );
    }

    fn rows(store: &mut crate::store::Store, method: &str, params: Value) -> Response {
        let mut params = params;
        params["workspace"] = json!("a".repeat(32));
        store.dispatch(Request {
            id: "rows".into(),
            method: method.into(),
            params,
        })
    }

    #[test]
    fn page_rows_survive_restart_and_a_reset_can_lower_a_sequence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        {
            let mut store = crate::store::Store::open(&path).unwrap();
            let written = rows(
                &mut store,
                "projection.putRows",
                json!({"prefix":"v1","rows":[
                    {"id":"entity:agent:a","seq":9,"payload":"","entity":{"id":"a"}},
                    {"id":"entity:agent:b","seq":7,"payload":"","_deleted":true},
                    {"id":"state:entities:checkpoint","seq":9,"payload":"{}"},
                ]}),
            );
            assert_eq!(written.result, Some(json!({"written":3})));
            // A reset lowers the sequence; the store keeps no ordering rule of its own.
            assert!(
                rows(
                    &mut store,
                    "projection.putRows",
                    json!({"prefix":"v1","rows":[
                {"id":"state:entities:checkpoint","seq":0,"payload":"{}"}]})
                )
                .error
                .is_none()
            );
        }
        let mut store = crate::store::Store::open(&path).unwrap();
        let found = rows(
            &mut store,
            "projection.getRows",
            json!({"prefix":"v1","ids":["state:entities:checkpoint","missing"]}),
        );
        assert_eq!(found.result.unwrap()[0]["seq"], 0);
        let range = rows(
            &mut store,
            "projection.rangeRows",
            json!({"prefix":"v1","gte":"entity:","lt":"entity;"}),
        );
        let range = range.result.unwrap();
        assert_eq!(range["more"], false);
        assert_eq!(range["rows"][0]["id"], "entity:agent:a");
        assert_eq!(range["rows"][1]["_deleted"], true);
        let page = rows(
            &mut store,
            "projection.rangeRows",
            json!({"prefix":"v1","gte":"entity:","lt":"entity;","limit":1}),
        )
        .result
        .unwrap();
        assert_eq!(
            (page["rows"].as_array().unwrap().len(), page["more"].clone()),
            (1, json!(true))
        );
        let next = rows(
            &mut store,
            "projection.rangeRows",
            json!({"prefix":"v1","gt":"entity:agent:a","lt":"entity;"}),
        )
        .result
        .unwrap();
        assert_eq!(next["rows"][0]["id"], "entity:agent:b");
        assert_eq!(next["more"], false);
    }

    #[test]
    fn page_rows_are_separate_per_prefix_and_old_prefixes_are_pruned() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        for prefix in ["old", "new"] {
            assert!(
                rows(
                    &mut store,
                    "projection.putRows",
                    json!({"prefix":prefix,"rows":[
                {"id":"entity:agent:a","seq":1,"payload":""}]})
                )
                .error
                .is_none()
            );
        }
        // A projection of the server's own documents is not a page row.
        assert!(rows(&mut store, "projection.put", json!({"scope":"transcript:one","document":{"id":"transcript:one","seq":1,"payload":"{}"}})).error.is_none());
        let pruned = rows(&mut store, "projection.pruneRows", json!({"prefix":"new"}));
        assert_eq!(pruned.result, Some(json!({"removed":1})));
        let count = |store: &mut crate::store::Store, prefix: &str| {
            rows(
                store,
                "projection.getRows",
                json!({"prefix":prefix,"ids":["entity:agent:a"]}),
            )
            .result
            .unwrap()
            .as_array()
            .unwrap()
            .len()
        };
        assert_eq!((count(&mut store, "old"), count(&mut store, "new")), (0, 1));
        assert!(
            rows(
                &mut store,
                "projection.get",
                json!({"scope":"transcript:one"})
            )
            .result
            .unwrap()
            .is_object()
        );
    }

    #[test]
    fn page_rows_refuse_bad_prefixes_identities_and_sequences() {
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        for params in [
            json!({"prefix":"","rows":[]}),
            json!({"prefix":"a/b","rows":[]}),
            json!({"prefix":"v1","rows":[{"id":"","seq":1}]}),
            json!({"prefix":"v1","rows":[{"id":"x","seq":-1}]}),
            json!({"prefix":"v1","rows":[{"id":"ok","seq":1},{"id":"x","seq":MAX_SAFE_INTEGER as u64 + 1}]}),
        ] {
            assert!(
                rows(&mut store, "projection.putRows", params.clone())
                    .error
                    .is_some(),
                "{params}"
            );
        }
        // The failed batch above wrote nothing, including its valid first row.
        assert_eq!(
            rows(
                &mut store,
                "projection.getRows",
                json!({"prefix":"v1","ids":["ok"]})
            )
            .result,
            Some(json!([]))
        );
        assert!(
            rows(
                &mut store,
                "projection.rangeRows",
                json!({"prefix":"v1","lt":"z"})
            )
            .error
            .is_some()
        );
    }

    #[tokio::test]
    async fn page_rows_sender_preserves_error_order_rollback_and_range_fallback() {
        use crate::delivery::StoreCommand;
        use tokio::sync::{mpsc, oneshot};
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receiver) = mpsc::channel::<StoreCommand>(8);
        let worker = std::thread::spawn(move || {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some(command) = receiver.blocking_recv() {
                command.execute(&mut store);
            }
        });
        async fn send(
            sender: &mpsc::Sender<StoreCommand>,
            method: &str,
            params: Value,
        ) -> Response {
            let (reply, receive) = oneshot::channel();
            sender
                .send(StoreCommand::Request(
                    Request {
                        id: "test".into(),
                        method: method.into(),
                        params,
                    },
                    reply,
                ))
                .await
                .unwrap();
            receive.await.unwrap()
        }
        let workspace = "a".repeat(32);
        let foreign = send(
            &sender,
            "projection.putRows",
            json!({"workspace":"bad","prefix":42,"rows":null}),
        )
        .await;
        assert_eq!(
            foreign.error.as_ref().map(|error| error.message.as_str()),
            Some("Invalid projection identity.")
        );
        for invalid in [
            json!({"id":"bad","seq":null}),
            json!({"id":"bad","seq":1.0}),
            json!({"seq":1}),
            json!(["bad", 1]),
        ] {
            let result = send(
                &sender,
                "projection.putRows",
                json!({"workspace":workspace,"prefix":"v1","rows":[{"id":"good","seq":1},invalid]}),
            )
            .await;
            assert!(result.error.is_some());
        }
        let found = send(
            &sender,
            "projection.getRows",
            json!({"workspace":workspace,"prefix":"v1","ids":["good"]}),
        )
        .await;
        assert_eq!(found.result, Some(json!([])));
        assert!(send(&sender, "projection.putRows", json!({"workspace":workspace,"prefix":"v1","rows":[{"id":"good","seq":1,"opaque":{"value":null}}]})).await.error.is_none());
        for limit in [json!(null), json!("bad"), json!(-1), json!(1.0)] {
            let found = send(&sender, "projection.rangeRows", json!({"workspace":workspace,"prefix":"v1","gte":"a","gt":7,"lt":"z","limit":limit})).await;
            assert_eq!(
                found.result.unwrap()["rows"][0]["opaque"],
                json!({"value":null})
            );
        }
        let other = send(
            &sender,
            "projection.getRows",
            json!({"workspace":"b".repeat(32),"prefix":"v1","ids":["good"]}),
        )
        .await;
        assert_eq!(other.result, Some(json!([])));
        drop(sender);
        worker.join().unwrap();
    }
}
