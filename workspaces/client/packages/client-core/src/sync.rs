//! Workspace verification and projection pulls run in Rust.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    projections::Document,
    protocol::{CommandError, Request},
};
use serde::{Deserialize, Deserializer};
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use studio_client_domain::{
    json_object::JsonObject,
    sync_entity::{EntityChange, SYNC_ENTITY_PAGE_LIMIT},
};

pub fn error(code: &str, message: impl ToString) -> CommandError {
    CommandError {
        code: code.into(),
        message: message.to_string(),
        status: None,
        details: None,
    }
}

pub async fn http(backend: &Backend, path: &str) -> Result<Value, CommandError> {
    let response = backend
        .dispatch(Request {
            id: "core-sync".into(),
            method: "api.request".into(),
            params: json!({"path":path,"timeoutMs":15000}),
        })
        .await;
    response.result.ok_or_else(|| {
        response
            .error
            .unwrap_or_else(|| error("invalid_response", "Missing server response."))
    })
}

pub async fn identity(backend: &Backend) -> Result<Value, CommandError> {
    let value = http(backend, "/api/sync/identity").await?;
    if !value["workspaceId"]
        .as_str()
        .is_some_and(crate::projections::valid_workspace)
    {
        return Err(error(
            "workspace_identity",
            "Invalid server workspace identity.",
        ));
    }
    Ok(value)
}

pub async fn open(backend: &Backend, sender: &StoreSender) -> Result<Value, CommandError> {
    let cached = state(sender, "identity.get", Value::Null)
        .await
        .map_err(|e| error("storage_error", e))?;
    let timeout = if cached.is_object() {
        Duration::from_millis(500)
    } else {
        Duration::from_secs(15)
    };
    match tokio::time::timeout(timeout, identity(backend)).await {
        Ok(Ok(value)) => state(sender, "identity.set", value)
            .await
            .map_err(|e| error("storage_error", e)),
        Ok(Err(failure)) if failure.status == Some(404) => Err(error(
            "unsupported_sync",
            "This server does not support synchronization.",
        )),
        Ok(Err(failure))
            if cached.is_null()
                || failure
                    .status
                    .is_some_and(|s| s < 500 && s != 408 && s != 429) =>
        {
            Err(failure)
        }
        _ if !cached.is_null() => Ok(cached),
        Ok(Err(failure)) => Err(failure),
        Err(_) => Err(error("timeout", "The server did not respond in time.")),
    }
}

// Historical payload fields may have types that must be ignored.
#[derive(Deserialize)]
#[serde(untagged)]
pub(crate) enum LenientField<T> {
    Value(T),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
#[serde(untagged)]
enum TranscriptPayload {
    Object(JsonObject<TranscriptObject>),
    Other(serde::de::IgnoredAny),
}

#[derive(Deserialize)]
struct TranscriptObject {
    #[serde(default)]
    delta: Option<LenientField<bool>>,
    #[serde(default)]
    items: Option<LenientField<Vec<LenientField<JsonObject<TranscriptItem>>>>>,
    #[serde(default)]
    removed: Option<LenientField<Vec<LenientField<String>>>>,
    #[serde(default)]
    order: Option<LenientField<Vec<LenientField<String>>>>,
    #[serde(flatten)]
    fields: serde_json::Map<String, Value>,
}
#[derive(Deserialize)]
struct TranscriptItem {
    // Missing and null identities both leave the item unidentified.
    id: Option<LenientField<String>>,
    #[serde(flatten)]
    fields: serde_json::Map<String, Value>,
}
impl TranscriptItem {
    fn identified(mut self) -> Option<(String, Value)> {
        let Some(LenientField::Value(id)) = self.id else {
            return None;
        };
        self.fields.insert("id".into(), Value::String(id.clone()));
        Some((id, Value::Object(self.fields)))
    }
}
impl TranscriptObject {
    fn is_delta(&self) -> bool {
        matches!(self.delta, Some(LenientField::Value(true)))
    }
}
fn merge_transcript_delta(
    previous: &str,
    incoming: TranscriptObject,
) -> Result<String, CommandError> {
    let base: TranscriptPayload =
        serde_json::from_str(previous).map_err(|e| error("invalid_projection", e))?;
    let TranscriptPayload::Object(JsonObject(base)) = base else {
        return Err(error(
            "invalid_projection",
            "Transcript projection base has no items.",
        ));
    };
    let Some(LenientField::Value(base_items)) = base.items else {
        return Err(error(
            "invalid_projection",
            "Transcript projection base has no items.",
        ));
    };
    let mut items = BTreeMap::new();
    let mut base_order = Vec::new();
    for entry in base_items {
        if let LenientField::Value(JsonObject(item)) = entry
            && let Some((id, item)) = item.identified()
        {
            base_order.push(id.clone());
            items.insert(id, item);
        }
    }
    if let Some(LenientField::Value(removed)) = incoming.removed {
        for id in removed {
            if let LenientField::Value(id) = id {
                items.remove(&id);
            }
        }
    }
    if let Some(LenientField::Value(changed)) = incoming.items {
        for item in changed {
            let LenientField::Value(JsonObject(item)) = item else {
                return Err(error(
                    "invalid_projection",
                    "Transcript delta item has no identity.",
                ));
            };
            let (id, item) = item.identified().ok_or_else(|| {
                error(
                    "invalid_projection",
                    "Transcript delta item has no identity.",
                )
            })?;
            items.insert(id, item);
        }
    }
    let order = match incoming.order {
        Some(LenientField::Value(order)) => order
            .into_iter()
            .filter_map(|id| match id {
                LenientField::Value(id) => Some(id),
                LenientField::Other(_) => None,
            })
            .collect(),
        _ => base_order,
    };
    let mut merged = base.fields;
    merged.extend(incoming.fields);
    merged.remove("itemRevisions");
    merged.insert(
        "items".into(),
        Value::Array(
            order
                .iter()
                .filter_map(|id| items.get(id).cloned())
                .collect(),
        ),
    );
    serde_json::to_string(&merged).map_err(|e| error("invalid_projection", e))
}

pub async fn pull(
    backend: &Backend,
    sender: &StoreSender,
    workspace: &str,
    scope: &str,
) -> Result<Value, CommandError> {
    let current = identity(backend).await?;
    if current["workspaceId"] != workspace {
        return Err(error(
            "workspace_changed",
            "The server workspace changed. Reload to synchronize.",
        ));
    }
    if scope == "state" {
        return pull_entities(backend, sender, workspace).await;
    }
    pull_document(backend, sender, workspace, scope).await
}

#[derive(Deserialize)]
struct DocumentPull {
    documents: Vec<DocumentEntry>,
}
#[derive(Deserialize)]
#[serde(untagged)]
enum DocumentEntry {
    Valid(Document),
    Invalid(Value),
}
impl DocumentEntry {
    fn checked(self) -> Result<Document, CommandError> {
        match self {
            Self::Valid(document) => Ok(document),
            Self::Invalid(value) => {
                Document::deserialize(value).map_err(|e| error("invalid_projection", e))
            }
        }
    }
}

async fn pull_document(
    backend: &Backend,
    sender: &StoreSender,
    workspace: &str,
    scope: &str,
) -> Result<Value, CommandError> {
    let current = identity(backend).await?;
    if current["workspaceId"] != workspace {
        return Err(error(
            "workspace_changed",
            "The server workspace changed. Reload to synchronize.",
        ));
    }
    let previous = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":scope}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let after = previous["seq"].as_u64().unwrap_or(0);
    let url = reqwest::Url::parse_with_params(
        "http://127.0.0.1/api/sync/pull",
        &[
            ("scope", scope.to_owned()),
            ("after", after.to_string()),
            ("limit", "100".into()),
        ],
    )
    .map_err(|e| error("invalid_scope", e))?;
    // parse_with_params above appends at least scope, after and limit.
    let result = http(
        backend,
        &format!(
            "/api/sync/pull?{}",
            url.query().expect("sync cursor query was appended")
        ),
    )
    .await?;
    let JsonObject(server) = JsonObject::<EntityWorkspace>::deserialize(&result).map_err(|_| {
        error(
            "workspace_changed",
            "The server workspace changed during synchronization.",
        )
    })?;
    if server.workspace_id != workspace {
        return Err(error(
            "workspace_changed",
            "The server workspace changed during synchronization.",
        ));
    }
    let pull: DocumentPull =
        serde_json::from_value(result).map_err(|e| error("invalid_projection", e))?;
    let mut documents = Vec::with_capacity(pull.documents.len());
    for entry in pull.documents {
        let mut doc = entry.checked()?;
        if doc.id != scope {
            return Err(error(
                "invalid_projection",
                "The server returned a different projection scope.",
            ));
        }
        if scope.starts_with("transcript:") && !doc.deleted {
            let incoming: TranscriptPayload =
                serde_json::from_str(&doc.payload).map_err(|e| error("invalid_projection", e))?;
            if let TranscriptPayload::Object(JsonObject(incoming)) = incoming
                && incoming.is_delta()
            {
                if previous.is_null() || previous["_deleted"] == true {
                    return Err(error(
                        "invalid_projection",
                        "Transcript projection delta has no valid base.",
                    ));
                }
                if doc.seq <= previous["seq"].as_u64().unwrap_or(0) {
                    continue;
                }
                doc.payload =
                    merge_transcript_delta(previous["payload"].as_str().unwrap_or(""), incoming)?;
            }
        }
        documents.push(
            crate::projections::check_document(scope, doc)
                .map_err(|e| error("storage_error", e))?,
        );
    }
    crate::delivery::put_documents(sender, workspace, scope, documents)
        .await
        .map_err(|e| error("storage_error", e))?;
    state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":scope}),
    )
    .await
    .map_err(|e| error("storage_error", e))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct EntityWorkspace {
    workspace_id: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EntityPull {
    Reset {
        #[serde(rename = "reset")]
        _reset: TrueFlag,
    },
    Page {
        documents: Vec<EntityChange>,
        checkpoint: JsonObject<EntityCheckpoint>,
        #[serde(rename = "maxSeq", default, deserialize_with = "optional_cursor")]
        max_seq: Option<u64>,
        #[serde(rename = "initialHigh", default, deserialize_with = "optional_cursor")]
        initial_high: Option<u64>,
    },
}
#[derive(Deserialize)]
struct EntityCheckpoint {
    seq: u64,
}
struct TrueFlag;
impl<'de> Deserialize<'de> for TrueFlag {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        if bool::deserialize(deserializer)? {
            Ok(Self)
        } else {
            Err(serde::de::Error::custom("not reset"))
        }
    }
}
fn optional_cursor<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Option<u64>, D::Error> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Cursor {
        Number(u64),
        Other(serde::de::IgnoredAny),
    }
    Ok(match Option::<Cursor>::deserialize(deserializer)? {
        Some(Cursor::Number(value)) => Some(value),
        _ => None,
    })
}

fn entity_ready(marker: &Value) -> bool {
    marker["payload"]
        .as_str()
        .and_then(|payload| serde_json::from_str::<String>(payload).ok())
        .as_deref()
        == Some("ready")
        && marker["_deleted"] != true
}

async fn pull_entities(
    backend: &Backend,
    sender: &StoreSender,
    workspace: &str,
) -> Result<Value, CommandError> {
    // Read the local revision before the cursor so a concurrent application
    // makes this HTTP interval stale rather than allowing a late reset to win.
    let previous = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":"state"}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let cached = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":"state:entities:checkpoint"}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let ready_marker = state(
        sender,
        "projection.get",
        json!({"workspace":workspace,"scope":"state:entities:ready"}),
    )
    .await
    .map_err(|e| error("storage_error", e))?;
    let ready = entity_ready(&ready_marker);
    let mut replace = !ready;
    let mut after = if ready {
        cached["seq"].as_u64().unwrap_or(0)
    } else {
        0
    };
    let mut initial_high = (!ready).then_some(0);
    let mut changes = Vec::new();
    loop {
        let mut query = vec![
            ("scope", "state:entities:v1".to_owned()),
            ("after", after.to_string()),
            ("limit", SYNC_ENTITY_PAGE_LIMIT.to_string()),
            ("reset", "1".to_owned()),
        ];
        if let Some(high) = initial_high {
            query.push(("fresh", "1".into()));
            query.push(("initialHigh", high.to_string()));
        }
        let url = reqwest::Url::parse_with_params("http://127.0.0.1/api/sync/pull", &query)
            .map_err(|e| error("invalid_scope", e))?;
        // parse_with_params above appends at least scope, after and limit.
        let result = http(
            backend,
            &format!(
                "/api/sync/pull?{}",
                url.query().expect("sync cursor query was appended")
            ),
        )
        .await?;
        let JsonObject(server) =
            JsonObject::<EntityWorkspace>::deserialize(&result).map_err(|_| {
                error(
                    "workspace_changed",
                    "The server workspace changed during synchronization.",
                )
            })?;
        if server.workspace_id != workspace {
            return Err(error(
                "workspace_changed",
                "The server workspace changed during synchronization.",
            ));
        }
        let pull: EntityPull =
            serde_json::from_value(result).map_err(|e| error("invalid_projection", e))?;
        let (rows, checkpoint, max, bound) = match pull {
            EntityPull::Reset { _reset: _ } => {
                if initial_high.is_some() {
                    return Err(error(
                        "invalid_projection",
                        "The server reset a fresh entity pull.",
                    ));
                }
                replace = true;
                after = 0;
                initial_high = Some(0);
                changes.clear();
                continue;
            }
            EntityPull::Page {
                documents,
                checkpoint,
                max_seq,
                initial_high,
            } => (documents, checkpoint.0.seq, max_seq, initial_high),
        };
        if rows.len() > SYNC_ENTITY_PAGE_LIMIT as usize {
            return Err(error(
                "invalid_projection",
                "The server returned too many entity documents.",
            ));
        }
        if initial_high.is_some() {
            initial_high = Some(bound.ok_or_else(|| {
                error(
                    "invalid_projection",
                    "The server returned no initial entity bound.",
                )
            })?);
        }
        let next = checkpoint;
        if next < after || (!rows.is_empty() && next == after) {
            return Err(error(
                "invalid_projection",
                "The entity checkpoint did not advance.",
            ));
        }
        after = next;
        let max = max.ok_or_else(|| {
            error(
                "invalid_projection",
                "The server returned no entity sequence bound.",
            )
        })?;
        let count = rows.len();
        changes.extend(rows);
        if count < SYNC_ENTITY_PAGE_LIMIT as usize || after >= max {
            break;
        }
    }
    crate::delivery::apply_entities(
        sender,
        workspace,
        changes,
        replace,
        Some(after),
        Some(previous["seq"].as_u64().unwrap_or(0)),
    )
    .await
    .map_err(|e| error("storage_error", e))
}

/// The UI receives a projection payload as an escaped JSON string in one
/// message. Refuse a payload that cannot fit, with the envelope reserve, so the
/// UI gets one actionable error instead of a transport response_size error.
pub fn projection_error(payload: &str) -> Option<CommandError> {
    // Bytes reserved for the response envelope around the escaped projection payload.
    const PROJECTION_ENVELOPE_RESERVE_BYTES: usize = 4096;
    projection_error_with_limit(
        payload,
        crate::protocol::MAX_MESSAGE.saturating_sub(PROJECTION_ENVELOPE_RESERVE_BYTES),
    )
}

fn projection_error_with_limit(payload: &str, limit: usize) -> Option<CommandError> {
    let encoded = serde_json::to_string(payload).map_or(usize::MAX, |text| text.len());
    (encoded > limit).then(|| {
        error(
            "projection_too_large",
            "The workspace state is too large to open. Update the Studio server; it removes old background tasks from sync.",
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::Response;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use tokio::{io::AsyncWriteExt, net::TcpListener, sync::mpsc};
    async fn apply_test(sender: &StoreSender, workspace: &str, documents: Value) -> Response {
        let changes = serde_json::from_value(documents).unwrap();
        match crate::delivery::apply_entities(sender, workspace, changes, false, None, None).await {
            Ok(value) => Response::success("apply".into(), value),
            Err(error) => Response::failure("apply".into(), "storage_error", error),
        }
    }
    #[test]
    fn entity_pull_consumed_fields_track_openapi_owner() {
        let openapi: Value = serde_json::from_str(include_str!(
            "../../../../runtime/packages/runtime-http/openapi.json"
        ))
        .unwrap();
        let schemas = &openapi["components"]["schemas"];
        let page = &schemas["SyncPullResponse"];
        assert_eq!(page["properties"]["workspaceId"]["type"], "string");
        assert_eq!(page["properties"]["documents"]["type"], "array");
        assert!(
            page["properties"]["documents"]["items"]["oneOf"]
                .as_array()
                .unwrap()
                .contains(&json!({"$ref":"#/components/schemas/SyncEntity"}))
        );
        assert_eq!(
            page["properties"]["checkpoint"]["$ref"],
            "#/components/schemas/SyncCheckpoint"
        );
        assert_eq!(schemas["SyncCheckpoint"]["required"], json!(["seq"]));
        assert_eq!(
            schemas["SyncCheckpoint"]["properties"]["seq"]["type"],
            "integer"
        );
        for field in ["maxSeq", "initialHigh"] {
            assert_eq!(page["properties"][field]["anyOf"][0]["type"], "integer");
            assert_eq!(
                page["properties"][field]["anyOf"][0]["maximum"],
                studio_client_domain::MAX_SAFE_INTEGER
            );
            assert_eq!(page["properties"][field]["anyOf"][1]["type"], "null");
        }
        assert_eq!(
            schemas["SyncPullResetResponse"]["properties"]["workspaceId"]["type"],
            "string"
        );
        assert_eq!(
            schemas["SyncPullResetResponse"]["properties"]["reset"]["const"],
            true
        );
    }

    #[test]
    fn transcript_identity_treats_absent_null_and_non_strings_as_unidentified() {
        for input in [r#"{}"#, r#"{"id":null}"#, r#"{"id":false}"#] {
            let JsonObject(item): JsonObject<TranscriptItem> = serde_json::from_str(input).unwrap();
            assert!(item.identified().is_none(), "{input}");
        }
        let JsonObject(item): JsonObject<TranscriptItem> =
            serde_json::from_str(r#"{"id":"ok"}"#).unwrap();
        assert_eq!(item.identified().unwrap().0, "ok");
    }

    #[test]
    fn entity_pull_requires_object_workspace_and_checkpoint() {
        assert!(JsonObject::<EntityWorkspace>::deserialize(json!(["workspace"])).is_err());
        assert!(
            serde_json::from_value::<EntityPull>(
                json!({"documents":[],"checkpoint":[1],"maxSeq":1})
            )
            .is_err()
        );
        assert!(serde_json::from_value::<EntityPull>(json!({"reset":true})).is_ok());
        let page: EntityPull = serde_json::from_str(r#"{"reset":"ignored","documents":[],"checkpoint":{"seq":0},"maxSeq":1,"initialHigh":[]}"#).unwrap();
        assert!(matches!(
            page,
            EntityPull::Page {
                max_seq: Some(1),
                initial_high: None,
                ..
            }
        ));
    }

    #[test]
    fn ready_marker_requires_a_live_json_string() {
        assert!(entity_ready(&json!({"payload":"\"ready\""})));
        for marker in [
            json!(null),
            json!({}),
            json!({"payload":true}),
            json!({"payload":"true"}),
            json!({"payload":"[\"ready\"]"}),
            json!({"payload":"ready"}),
            json!({"payload":"\"ready\"", "_deleted":true}),
        ] {
            assert!(!entity_ready(&marker), "{marker}");
        }
    }

    #[test]
    fn transcript_delta_merges_legacy_cache_without_retaining_item_revisions() {
        let base = json!({"payload":json!({"items":[{"id":"a","v":1},{"id":"b","v":2}],"itemRevisions":{"a":1,"b":1},"title":"old"}).to_string()});
        let delta = json!({"items":[{"id":"a","v":3},{"id":"c","v":4}],"removed":["b"],"order":["c","a"],"delta":true,"title":"new"});
        let TranscriptPayload::Object(JsonObject(delta)) = serde_json::from_value(delta).unwrap()
        else {
            panic!("delta object")
        };
        let actual: Value = serde_json::from_str(
            &merge_transcript_delta(base["payload"].as_str().unwrap(), delta).unwrap(),
        )
        .unwrap();
        assert_eq!(
            actual,
            json!({"items":[{"id":"c","v":4},{"id":"a","v":3}],"title":"new"})
        );
    }

    #[test]
    fn oversized_materialized_state_returns_one_actionable_projection_error() {
        let snapshot = json!({"runtime":{"tasks":["x".repeat(44 * 1024 * 1024)]}});
        let failure = crate::projections::serialize_state(&snapshot).unwrap_err();
        assert_eq!(failure.code, "projection_too_large");
        assert!(failure.message.contains("Update the Studio server"));
    }

    #[test]
    fn projection_limit_counts_json_escaping() {
        // Quotes occupy two JSON bytes; plain text occupies one, plus delimiters.
        assert_eq!(
            projection_error_with_limit(&"\"".repeat(32), 63)
                .unwrap()
                .code,
            "projection_too_large"
        );
        assert!(projection_error_with_limit(&"x".repeat(61), 63).is_none());
        assert!(projection_error_with_limit(&"x".repeat(62), 63).is_some());
    }

    #[tokio::test]
    async fn entity_batches_with_out_of_order_sequences_remain_visible() {
        let workspace = "b".repeat(32);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let worker_path = path.clone();
        let worker = tokio::spawn(async move {
            let mut store = crate::store::Store::open(&worker_path).unwrap();
            while let Some(command) = receive.recv().await {
                command.execute(&mut store);
            }
        });
        let mut previous = 0;
        for (id, seq, count) in [("A", 11, 1), ("B", 10, 2)] {
            let response = apply_test(
                &sender,
                &workspace,
                json!([{"op":"upsert","collection":"task","id":id,"seq":seq,"value":{"id":id}}]),
            )
            .await;
            assert!(response.error.is_none(), "{:?}", response.error);
            let doc = response.result.unwrap();
            let snapshot: Value = serde_json::from_str(doc["payload"].as_str().unwrap()).unwrap();
            assert_eq!(
                snapshot["runtime"]["tasks"].as_array().unwrap().len(),
                count
            );
            assert!(doc["seq"].as_u64().unwrap() > previous);
            previous = doc["seq"].as_u64().unwrap();
        }
        drop(sender);
        worker.await.unwrap();
        let mut store = crate::store::Store::open(&path).unwrap();
        let doc = store
            .dispatch(Request {
                id: "read".into(),
                method: "projection.get".into(),
                params: json!({"workspace":workspace,"scope":"state"}),
            })
            .result
            .unwrap();
        assert_eq!(doc["seq"], previous);
        let snapshot: Value = serde_json::from_str(doc["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["runtime"]["tasks"], json!([{"id":"A"},{"id":"B"}]));
    }

    #[test]
    fn transcript_and_document_consumed_fields_track_openapi() {
        let openapi: Value = serde_json::from_str(include_str!(
            "../../../../runtime/packages/runtime-http/openapi.json"
        ))
        .unwrap();
        let schemas = &openapi["components"]["schemas"];
        let document = &schemas["SyncDocument"];
        assert_eq!(
            document["required"],
            json!(["id", "payload", "seq", "_deleted"])
        );
        assert_eq!(document["properties"]["id"]["type"], "string");
        assert_eq!(document["properties"]["payload"]["type"], "string");
        assert_eq!(document["properties"]["seq"]["type"], "integer");
        assert_eq!(
            document["properties"]["seq"]["maximum"],
            studio_client_domain::MAX_SAFE_INTEGER
        );
        assert_eq!(document["properties"]["_deleted"]["type"], "boolean");
        let delta = &schemas["TranscriptStreamDelta"];
        let keys = delta["properties"]
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();
        let expected = "agent delta historyVersion items nextAfterCursor nextCursor order removed tail truncated unavailable".split_whitespace().collect::<std::collections::BTreeSet<_>>();
        assert_eq!(keys, expected);
        assert_eq!(delta["properties"]["delta"]["const"], true);
        for field in ["items", "order", "removed"] {
            assert_eq!(delta["properties"][field]["type"], "array");
        }
    }

    #[tokio::test]
    async fn transcript_pull_merges_order_and_removals_and_keeps_old_value_on_error() {
        let workspace = "b".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            for index in 0..6 {
                let (mut stream, _) = listener.accept().await.unwrap();
                crate::backend::tests::read_http(&mut stream).await;
                let body = if index == 2 {
                    let delta = json!({"delta":true,"items":[{"id":"a","text":"new","opaque":[null,1]},{"id":"c","text":"third"}],"removed":["b",false],"order":["c",7,"a","a"],"title":"after","vendor":{"keep":true}});
                    json!({"workspaceId":server_workspace,"documents":[{"id":"transcript:a","seq":2,"payload":delta.to_string()}]})
                } else if index == 5 {
                    let delta = json!({"delta":true,"items":[{"text":"missing id"}],"removed":[],"order":[]});
                    json!({"workspaceId":server_workspace,"documents":[{"id":"transcript:a","seq":3,"payload":delta.to_string()}]})
                } else { json!({"workspaceId":server_workspace,"chatState":true}) }.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        let base = json!({"items":[{"id":"a","text":"old"},false,{"id":5},{"id":"b","text":"removed"}],"title":"before","itemRevisions":{"a":1},"legacy":null});
        let response = store.dispatch(Request { id:"seed".into(), method:"projection.put".into(), params:json!({"workspace":workspace,"scope":"transcript:a","document":{"id":"transcript:a","seq":1,"payload":base.to_string()}}) });
        assert!(response.error.is_none());
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let worker = tokio::spawn(async move {
            while let Some(command) = receive.recv().await {
                command.execute(&mut store);
            }
        });
        let updated = pull(&backend, &sender, &workspace, "transcript:a")
            .await
            .unwrap();
        assert_eq!(updated["seq"], 2);
        let payload: Value = serde_json::from_str(updated["payload"].as_str().unwrap()).unwrap();
        assert_eq!(
            payload,
            json!({"items":[{"id":"c","text":"third"},{"id":"a","text":"new","opaque":[null,1]},{"id":"a","text":"new","opaque":[null,1]}],"title":"after","legacy":null,"vendor":{"keep":true}})
        );
        assert!(
            pull(&backend, &sender, &workspace, "transcript:a")
                .await
                .is_err()
        );
        assert_eq!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"transcript:a"})
            )
            .await
            .unwrap(),
            updated
        );
        drop(sender);
        worker.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn transcript_pull_rejects_later_bad_document_without_partial_write() {
        let workspace = "a".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                crate::backend::tests::read_http(&mut stream).await;
                let body = if index < 2 { json!({"workspaceId":server_workspace,"chatState":true}) } else {
                    json!({"workspaceId":server_workspace,"documents":[{"id":"transcript:a","seq":1,"payload":"{\"items\":[]}"},{"id":"transcript:other","seq":2,"payload":"{}"}]})
                }.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let store = crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let worker = tokio::spawn(async move {
            let mut store = store;
            while let Some(command) = receive.recv().await {
                command.execute(&mut store);
            }
        });
        assert!(
            pull(&backend, &sender, &workspace, "transcript:a")
                .await
                .is_err()
        );
        assert!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"transcript:a"})
            )
            .await
            .unwrap()
            .is_null()
        );
        drop(sender);
        worker.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn entity_pull_resets_pruned_tombstones_and_replaces_cache() {
        let workspace = "a".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            let mut reset_supported = false;
            for index in 0..6 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, _) = crate::backend::tests::read_http(&mut stream).await;
                let path = headers
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let body = if matches!(index, 0 | 3) {
                    json!({"workspaceId":server_workspace,"chatState":true})
                } else {
                    let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
                    reset_supported |= url.query_pairs().any(|(k, v)| k == "reset" && v == "1");
                    if matches!(index, 1 | 4) {
                        assert!(url.query_pairs().any(|(k, v)| k == "after" && v == "5"));
                        json!({"workspaceId":server_workspace,"reset":true,"floor":8,"maxSeq":12})
                    } else {
                        assert!(url.query_pairs().any(|(k, v)| k == "after" && v == "0"));
                        assert!(url.query_pairs().any(|(k, v)| k == "fresh" && v == "1"));
                        json!({"workspaceId":server_workspace,"documents":[{"op":"upsert","collection":"task","id":"current","seq":12,"value":{"id":"current"}}],"checkpoint":{"seq":12},"maxSeq":12,"initialHigh":12})
                    }
                };
                let body = body.to_string();
                let status = if index == 2 {
                    "503 Service Unavailable"
                } else {
                    "200 OK"
                };
                stream.write_all(format!("HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            }
            assert!(reset_supported, "entity pulls must request reset support");
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let mut store = crate::store::Store::open(&path).unwrap();
        assert!(store.dispatch(Request { id: "seed".into(), method: "projection.putEntity".into(), params: json!({"workspace":workspace,"change":{"op":"upsert","collection":"task","id":"deleted","seq":5,"value":{"id":"deleted"}}}) }).error.is_none());
        for (scope, payload) in [
            ("state:entities:checkpoint", "{}"),
            ("state:entities:ready", "\"ready\""),
            ("transcript:kept", "{}"),
            ("state", "{}"),
        ] {
            assert!(store.dispatch(Request { id: "seed".into(), method: "projection.put".into(), params: json!({"workspace":workspace,"scope":scope,"document":{"id":scope,"seq":5,"payload":payload}}) }).error.is_none());
        }
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let worker = tokio::spawn(async move {
            while let Some(command) = receive.recv().await {
                command.execute(&mut store);
            }
        });
        let failure = pull(&backend, &sender, &workspace, "state")
            .await
            .unwrap_err();
        assert_eq!(failure.status, Some(503));
        let retained = state(
            &sender,
            "projection.get",
            json!({"workspace":workspace,"scope":"entity:task:deleted"}),
        )
        .await
        .unwrap();
        assert_eq!(
            retained["seq"], 5,
            "a failed reset keeps the previous cache"
        );
        let retained = state(
            &sender,
            "projection.get",
            json!({"workspace":workspace,"scope":"state:entities:checkpoint"}),
        )
        .await
        .unwrap();
        assert_eq!(
            retained["seq"], 5,
            "a failed reset keeps the previous cursor"
        );
        let doc = pull(&backend, &sender, &workspace, "state").await.unwrap();
        let snapshot: Value = serde_json::from_str(doc["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["runtime"]["tasks"], json!([{"id":"current"}]));
        assert!(doc["seq"].as_u64().unwrap() > 5);
        drop(sender);
        worker.await.unwrap();
        server.await.unwrap();
        let mut store = crate::store::Store::open(&path).unwrap();
        let get = |scope| Request {
            id: "read".into(),
            method: "projection.get".into(),
            params: json!({"workspace":workspace,"scope":scope}),
        };
        assert_eq!(
            store.dispatch(get("entity:task:deleted")).result,
            Some(Value::Null)
        );
        assert_eq!(
            store
                .dispatch(get("state:entities:checkpoint"))
                .result
                .unwrap()["seq"],
            12
        );
        assert_eq!(
            store.dispatch(get("transcript:kept")).result.unwrap()["seq"],
            5
        );
    }

    #[tokio::test]
    async fn cancelled_fresh_replacement_keeps_old_entities_and_cursor() {
        let workspace = "a".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server_workspace = workspace.clone();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            for index in 0..3 {
                let (mut stream, _) = listener.accept().await.unwrap();
                crate::backend::tests::read_http(&mut stream).await;
                if index == 2 {
                    started.send(()).unwrap();
                    drop(released.await);
                    break;
                }
                let body = if index == 0 {
                    json!({"workspaceId":server_workspace,"chatState":true})
                } else {
                    json!({"workspaceId":server_workspace,"reset":true,"floor":8,"maxSeq":12})
                }
                .to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let mut store =
            crate::store::Store::open(&directory.path().join("client.sqlite3")).unwrap();
        assert!(store.dispatch(Request { id:"seed".into(), method:"projection.putEntity".into(), params:json!({"workspace":workspace,"change":{"op":"upsert","collection":"task","id":"old","seq":5,"value":{"id":"old"}}}) }).error.is_none());
        for (scope, payload) in [
            ("state:entities:checkpoint", "{}"),
            ("state:entities:ready", "\"ready\""),
            ("state", "{}"),
        ] {
            assert!(store.dispatch(Request { id:"seed".into(), method:"projection.put".into(), params:json!({"workspace":workspace,"scope":scope,"document":{"id":scope,"seq":5,"payload":payload}}) }).error.is_none());
        }
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let worker = tokio::spawn(async move {
            while let Some(command) = receive.recv().await {
                command.execute(&mut store);
            }
        });
        let pull_sender = sender.clone();
        let pull_workspace = workspace.clone();
        let pending =
            tokio::spawn(
                async move { pull(&backend, &pull_sender, &pull_workspace, "state").await },
            );
        waiting.await.unwrap();
        pending.abort();
        assert!(pending.await.unwrap_err().is_cancelled());
        release.send(()).unwrap();
        assert_eq!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"entity:task:old"})
            )
            .await
            .unwrap()["seq"],
            5
        );
        assert_eq!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"state:entities:checkpoint"})
            )
            .await
            .unwrap()["seq"],
            5
        );
        drop(sender);
        worker.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn entity_pull_rejects_interval_racing_a_newer_application() {
        let workspace = "a".repeat(32);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (started, waiting) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::backend::tests::read_http(&mut stream).await;
            let body = json!({"workspaceId":server_workspace,"chatState":true}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::backend::tests::read_http(&mut stream).await;
            started.send(()).unwrap();
            released.await.unwrap();
            let body = json!({"workspaceId":server_workspace,"documents":[{"op":"upsert","collection":"task","id":"A","seq":1,"value":{"id":"A"}}],"checkpoint":{"seq":1},"maxSeq":1,"initialHigh":1}).to_string();
            stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", body.len(), body).as_bytes()).await.unwrap();
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let worker = tokio::spawn(async move {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some(command) = receive.recv().await {
                command.execute(&mut store);
            }
            drop(directory);
        });
        let pull_sender = sender.clone();
        let pull_workspace = workspace.clone();
        let pending =
            tokio::spawn(
                async move { pull(&backend, &pull_sender, &pull_workspace, "state").await },
            );
        waiting.await.unwrap();
        let deleted = state(&sender, "projection.applyEntities", json!({"workspace":workspace,"changes":[{"op":"delete","collection":"task","id":"A","seq":2}]})).await.unwrap();
        release.send(()).unwrap();
        let failure = pending.await.unwrap().unwrap_err();
        assert!(failure.message.contains("Entity state changed"));
        assert_eq!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"state"})
            )
            .await
            .unwrap(),
            deleted
        );
        assert_eq!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"entity:task:A"})
            )
            .await
            .unwrap()["op"],
            "delete"
        );
        assert!(
            state(
                &sender,
                "projection.get",
                json!({"workspace":workspace,"scope":"state:entities:checkpoint"})
            )
            .await
            .unwrap()
            .is_null()
        );
        drop(sender);
        worker.await.unwrap();
        server.await.unwrap();
    }

    #[tokio::test]
    async fn entity_v4_batch_applies_structured_upserts_and_deletes_once() {
        let workspace = "b".repeat(32);
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let state_puts = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&state_puts);
        let commands = Arc::new(AtomicUsize::new(0));
        let observed_commands = Arc::clone(&commands);
        let worker = tokio::spawn(async move {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some(command) = receive.recv().await {
                observed_commands.fetch_add(1, Ordering::SeqCst);
                let batch = matches!(
                    &command,
                    crate::delivery::StoreCommand::ApplyEntities { .. }
                );
                command.execute(&mut store);
                if batch {
                    let revision = store
                        .connection
                        .query_row(
                            "SELECT seq FROM client_projections WHERE scope='state' LIMIT 1",
                            [],
                            |row| row.get::<_, i64>(0),
                        )
                        .unwrap_or(0) as usize;
                    observed.store(revision, Ordering::SeqCst);
                }
            }
            drop(directory);
        });
        let task = json!({"op":"upsert","collection":"task","id":"one","seq":10,"value":{"id":"one","status":"running"}});
        let other = json!({"op":"upsert","collection":"task","id":"two","seq":11,"value":{"id":"two","status":"running"}});
        let first = apply_test(&sender, &workspace, json!([task, other])).await;
        assert!(first.error.is_none(), "{:?}", first.error);
        let snapshot: Value =
            serde_json::from_str(first.result.unwrap()["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["runtime"]["tasks"].as_array().unwrap().len(), 2);
        assert_eq!(state_puts.load(Ordering::SeqCst), 1);
        assert_eq!(
            commands.load(Ordering::SeqCst),
            1,
            "one storage command applies and materializes the entire batch"
        );
        let removed = apply_test(
            &sender,
            &workspace,
            json!([
                {"op":"delete","collection":"task","id":"one","seq":12},
                {"op":"delete","collection":"task","id":"two","seq":13}
            ]),
        )
        .await;
        assert!(removed.error.is_none(), "{:?}", removed.error);
        let removed = removed.result.unwrap();
        let snapshot: Value = serde_json::from_str(removed["payload"].as_str().unwrap()).unwrap();
        assert!(snapshot["runtime"]["tasks"].as_array().unwrap().is_empty());
        assert_eq!(state_puts.load(Ordering::SeqCst), 2);
        assert_eq!(commands.load(Ordering::SeqCst), 2);
        let stale = apply_test(&sender, &workspace, json!([task])).await;
        assert_eq!(stale.result, Some(removed));
        assert_eq!(commands.load(Ordering::SeqCst), 3);
        assert_eq!(state_puts.load(Ordering::SeqCst), 2);
        let checkpoint = state(
            &sender,
            "projection.get",
            json!({"workspace":workspace,"scope":"state:entities:checkpoint"}),
        )
        .await
        .unwrap();
        assert!(
            checkpoint.is_null(),
            "notifications cannot advance the pull cursor"
        );
        drop(sender);
        worker.await.unwrap();
    }

    #[tokio::test]
    async fn entity_pull_paginates_and_materializes_the_browser_snapshot() {
        let workspace = "a".repeat(32);
        let mut entities = Vec::new();
        for index in 0..700 {
            let id = format!("agent-{index:03}");
            entities.push(json!({"collection":"agent","id":id,"value":{"id":id,"name":id}}));
        }
        entities.extend([
            json!({"collection":"chat","id":"chat-1","value":{"id":"chat-1","name":"Chat"}}),
            json!({"collection":"edge","id":"edge-1","value":{"id":"edge-1","source":"agent-000","target":"chat-1"}}),
            json!({"collection":"workspace","id":"current","value":{"stateDir":"isolated-state"}}),
            json!({"collection":"task","id":"task-1","value":{"id":"task-1","status":"running"}}),
            json!({"collection":"rateLimit","id":"default","value":{"accountKey":"default","at":100.0,"data":{"accountId":"fixture"}}}),
        ]);
        let docs: Vec<Value> = entities.iter().enumerate().map(|(index, entity)| {
            json!({"op":"upsert","collection":entity["collection"],"id":entity["id"],"value":entity["value"],"seq":index+1})
        }).collect();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server_docs = docs.clone();
        let server_workspace = workspace.clone();
        let server = tokio::spawn(async move {
            for request_index in 0..5 {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, _) = crate::backend::tests::read_http(&mut stream).await;
                let path = headers
                    .lines()
                    .next()
                    .unwrap()
                    .split_whitespace()
                    .nth(1)
                    .unwrap();
                let body = if matches!(request_index, 0 | 3) {
                    assert!(path.starts_with("/api/sync/identity"));
                    json!({"workspaceId":server_workspace,"chatState":true})
                } else {
                    let url = reqwest::Url::parse(&format!("http://localhost{path}")).unwrap();
                    assert_eq!(
                        url.query_pairs().find(|(key, _)| key == "scope").unwrap().1,
                        "state:entities:v1"
                    );
                    let after: usize = url
                        .query_pairs()
                        .find(|(key, _)| key == "after")
                        .unwrap()
                        .1
                        .parse()
                        .unwrap();
                    let fresh = url
                        .query_pairs()
                        .any(|(key, value)| key == "fresh" && value == "1");
                    if after < 705 {
                        assert!(
                            fresh,
                            "every first-sync page must retain the fresh high-water mark"
                        );
                        assert_eq!(
                            url.query_pairs().find(|(key, _)| key == "limit").unwrap().1,
                            "500"
                        );
                    } else {
                        assert!(!fresh, "incremental entity pulls must use the checkpoint");
                    }
                    let rows = server_docs
                        .iter()
                        .filter(|doc| doc["seq"].as_u64().unwrap() as usize > after)
                        .take(500)
                        .cloned()
                        .collect::<Vec<_>>();
                    let checkpoint = rows
                        .last()
                        .map(|doc| doc["seq"].as_u64().unwrap())
                        .unwrap_or(after as u64);
                    json!({"workspaceId":server_workspace,"documents":rows,"checkpoint":{"seq":checkpoint},"maxSeq":server_docs.len(),"initialHigh":if fresh { server_docs.len() } else { 0 }})
                };
                let encoded = body.to_string();
                stream.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", encoded.len(), encoded).as_bytes()).await.unwrap();
            }
        });

        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let (sender, mut receive) = mpsc::channel::<crate::delivery::StoreCommand>(32);
        let state_puts = Arc::new(AtomicUsize::new(0));
        let observed_state_puts = Arc::clone(&state_puts);
        tokio::spawn(async move {
            let mut store = crate::store::Store::open(&path).unwrap();
            while let Some(command) = receive.recv().await {
                let batch = matches!(
                    &command,
                    crate::delivery::StoreCommand::ApplyEntities { .. }
                );
                command.execute(&mut store);
                if batch {
                    let revision = store
                        .connection
                        .query_row(
                            "SELECT seq FROM client_projections WHERE scope='state' LIMIT 1",
                            [],
                            |row| row.get::<_, i64>(0),
                        )
                        .unwrap_or(0) as usize;
                    observed_state_puts.store(revision, Ordering::SeqCst);
                }
            }
            drop(directory);
        });
        let backend = Backend::new(&origin).unwrap();
        let document = pull(&backend, &sender, &workspace, "state").await.unwrap();
        let snapshot: Value = serde_json::from_str(document["payload"].as_str().unwrap()).unwrap();
        assert_eq!(snapshot["threads"].as_array().unwrap().len(), 700);
        assert_eq!(snapshot["chats"][0]["id"], "chat-1");
        assert_eq!(snapshot["nodes"].as_array().unwrap().len(), 701);
        assert_eq!(snapshot["edges"][0]["id"], "edge-1");
        assert_eq!(snapshot["runtime"]["tasks"][0]["id"], "task-1");
        assert_eq!(snapshot["runtime"]["stateDir"], "isolated-state");
        assert_eq!(
            snapshot["runtime"]["rateLimitsByAccount"]["default"]["at"],
            100.0
        );
        assert_eq!(
            snapshot["runtime"]["rateLimits"],
            snapshot["runtime"]["rateLimitsByAccount"]["default"]
        );
        assert_eq!(state_puts.load(Ordering::SeqCst), 1);
        let unchanged = pull(&backend, &sender, &workspace, "state").await.unwrap();
        assert_eq!(unchanged, document);
        assert_eq!(
            state_puts.load(Ordering::SeqCst),
            1,
            "an unchanged entity refresh must not rewrite the full state document"
        );
        let checkpoint = state(
            &sender,
            "projection.get",
            json!({"workspace":workspace,"scope":"state:entities:checkpoint"}),
        )
        .await
        .unwrap();
        assert_eq!(checkpoint["seq"], 705);
        let tombstone = json!({"op":"delete","collection":"task","id":"task-1","seq":706});
        let retired = state(
            &sender,
            "projection.applyEntities",
            json!({"workspace":workspace,"changes":[tombstone]}),
        )
        .await
        .unwrap();
        let retired: Value = serde_json::from_str(retired["payload"].as_str().unwrap()).unwrap();
        assert!(
            retired["runtime"]["tasks"].as_array().unwrap().is_empty(),
            "a normal entity tombstone must remove the task from materialized state"
        );
        server.await.unwrap();
    }
}
