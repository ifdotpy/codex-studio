//! Host-only preparation of a sealed archive. This never seals current WebView data.
use crate::{profile::SealedSource, service_context::Context};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use studio_client_core::{client::Client, protocol::Request};

/// Maximum encoded record bytes in one preparation plan. Unit: bytes.
pub(crate) const MAX_PREPARE_RECORD_BYTES: usize = 64 * 1024 * 1024;
fn hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}
fn text<'a>(value: &'a Value, key: &str) -> Result<&'a str, String> {
    value[key]
        .as_str()
        .ok_or_else(|| format!("Invalid archive {key}."))
}
enum Saved {
    Json(Value),
    Object(BTreeMap<String, Saved>),
    Array(Vec<Saved>),
    Bytes(Vec<u8>, Option<String>),
}
impl Saved {
    fn json(self) -> Result<Value, String> {
        match self {
            Self::Json(value) => Ok(value),
            Self::Array(values) => values
                .into_iter()
                .map(Self::json)
                .collect::<Result<Vec<_>, _>>()
                .map(Value::Array),
            Self::Object(values) => values
                .into_iter()
                .map(|(k, v)| Ok((k, v.json()?)))
                .collect::<Result<Map<_, _>, String>>()
                .map(Value::Object),
            Self::Bytes(..) => Err("Unsupported binary archive metadata.".into()),
        }
    }
    fn object(self) -> Result<BTreeMap<String, Saved>, String> {
        if let Self::Object(value) = self {
            Ok(value)
        } else {
            Err("Invalid archive object.".into())
        }
    }
    fn bytes(self, view: Option<&str>) -> Result<Vec<u8>, String> {
        match self {
            Self::Bytes(bytes, actual) if view.is_none() || actual.as_deref() == view => Ok(bytes),
            _ => Err("The archive byte representation differs.".into()),
        }
    }
}
fn decode(value: &Value) -> Result<Saved, String> {
    let tag = value.as_array().ok_or("Invalid archive tag.")?;
    match tag.first().and_then(Value::as_str) {
        Some("value") if tag.len() == 2 && !tag[1].is_array() && !tag[1].is_object() => {
            Ok(Saved::Json(tag[1].clone()))
        }
        Some("object") if tag.len() == 2 => {
            let mut fields = BTreeMap::new();
            for field in tag[1].as_array().ok_or("Invalid archive object fields.")? {
                let pair = field
                    .as_array()
                    .filter(|p| p.len() == 2)
                    .ok_or("Invalid archive field.")?;
                let key = pair[0].as_str().ok_or("Invalid archive field name.")?;
                if fields.insert(key.into(), decode(&pair[1])?).is_some() {
                    return Err("Duplicate archive field.".into());
                }
            }
            Ok(Saved::Object(fields))
        }
        Some("array") if tag.len() == 2 => Ok(Saved::Array(
            tag[1]
                .as_array()
                .ok_or("Invalid archive array.")?
                .iter()
                .map(decode)
                .collect::<Result<_, _>>()?,
        )),
        Some("bytes") if tag.len() == 2 => Ok(Saved::Bytes(
            STANDARD
                .decode(tag[1].as_str().ok_or("Invalid archive bytes.")?)
                .map_err(|_| "Invalid archive base64.")?,
            None,
        )),
        Some("view")
            if tag.len() == 3
                && matches!(
                    tag[1].as_str(),
                    Some(
                        "Int8Array"
                            | "Uint8Array"
                            | "Uint8ClampedArray"
                            | "Int16Array"
                            | "Uint16Array"
                            | "Int32Array"
                            | "Uint32Array"
                            | "Float32Array"
                            | "Float64Array"
                            | "DataView"
                    )
                ) =>
        {
            let bytes = decode(&tag[2])?.bytes(None)?;
            Ok(Saved::Bytes(bytes, tag[1].as_str().map(str::to_owned)))
        }
        Some("blob") if tag.len() == 3 && tag[1].is_string() => decode(&tag[2]),
        Some("file")
            if tag.len() == 5
                && tag[1].is_string()
                && tag[2].is_string()
                && tag[3].as_u64().is_some() =>
        {
            decode(&tag[4])
        }
        _ => Err("Unsupported archive value tag.".into()),
    }
}
struct Artifact {
    entry: Value,
    bytes: Vec<u8>,
}
struct Plan {
    metadata: Value,
    artifacts: Vec<Artifact>,
    caches: Vec<Value>,
}

#[derive(Clone)]
pub(crate) struct SourceOwner {
    pub(crate) server_id: String,
    pub(crate) workspace_id: String,
    pub(crate) source_identity: String,
}
impl SourceOwner {
    fn from_context(context: &Context) -> Self {
        Self {
            server_id: context.server_id.clone(),
            workspace_id: context.workspace_id.clone(),
            source_identity: context.source_identity.clone(),
        }
    }
}

pub(crate) struct ValidatedSource {
    owner: SourceOwner,
    plan: Plan,
    archive: Value,
    metadata_prefix: &'static str,
}
impl ValidatedSource {
    pub(crate) fn inventory(&self) -> Value {
        json!({"metadataSha256":hash(self.plan.metadata.to_string().as_bytes()),"bytes":self.plan.artifacts.iter().map(|artifact|artifact.entry.clone()).collect::<Vec<_>>(),"caches":self.plan.caches,"archive":self.archive})
    }
    pub(crate) async fn execute(self, context: &Context) -> Result<Value, String> {
        if self.owner.server_id != context.server_id
            || self.owner.workspace_id != context.workspace_id
            || self.owner.source_identity != context.source_identity
        {
            return Err("The validated source belongs to another native owner.".into());
        }
        if let Some(backend) = &context.backend {
            backend.validate_identity()?;
        }
        execute_plan(context, self.plan, self.archive, self.metadata_prefix).await
    }
}

pub(crate) fn validated_source(
    context: &SourceOwner,
    capture: &crate::legacy_capture::CapturedSource,
    native_preferences: &Value,
    archive: Option<&SealedSource<'_>>,
    replayed: &BTreeSet<String>,
) -> Result<ValidatedSource, String> {
    let mut current = current_plan_for_owner(context, capture, native_preferences)?;
    let pending = current.metadata["pendingPreferences"]
        .as_array()
        .ok_or("The current preference intent inventory is incomplete.")?;
    if pending
        .iter()
        .any(|write| write["id"].as_str().is_none_or(|id| !replayed.contains(id)))
    {
        return Err(
            "The original native preference owner has not confirmed every retained request.".into(),
        );
    }
    current.metadata["pendingPreferences"] = json!([]);
    let Some(archive) = archive else {
        current.metadata["expectedByteImports"] = json!(
            (0..current.artifacts.len())
                .map(|index| format!("current-byte:v1:{}:{index}", context.source_identity))
                .collect::<Vec<_>>()
        );
        return Ok(ValidatedSource {
            owner: context.clone(),
            plan: current,
            archive: Value::Null,
            metadata_prefix: "current-metadata",
        });
    };
    let archived = plan_for_owner(context, archive)?;
    let meaningful_current = capture.snapshot["preferences"]
        .as_object()
        .ok_or("The current preference inventory is incomplete.")?
        .iter()
        .any(|(key, value)| {
            !key.ends_with("studio-native-preferences-journal-v1") || value != "[]"
        })
        || capture.snapshot["databases"]
            .as_array()
            .ok_or("The current database inventory is incomplete.")?
            .iter()
            .any(|db| {
                db["stores"].as_array().is_none_or(|stores| {
                    stores.iter().any(|store| {
                        store["records"]
                            .as_array()
                            .is_none_or(|records| !records.is_empty())
                    })
                })
            })
        || !native_preferences["preferences"]
            .as_object()
            .ok_or("The original native preferences are incomplete.")?
            .is_empty()
        || !native_preferences["preferenceReceipts"]
            .as_array()
            .ok_or("The original native receipts are incomplete.")?
            .is_empty();
    fn semantic(plan: &Plan) -> Value {
        let mut metadata = plan.metadata.clone();
        for key in [
            "sourceIdentity",
            "archiveManifest",
            "expectedByteImports",
            "preferenceReceipts",
        ] {
            metadata.as_object_mut().unwrap().remove(key);
        }
        json!({"metadata":metadata,"artifacts":plan.artifacts.iter().map(|artifact|artifact.entry.clone()).collect::<Vec<_>>(),"caches":plan.caches})
    }
    if meaningful_current && semantic(&current) != semantic(&archived) {
        return Err(
            "The saved archive and current sources differ. Both sources remain held for review."
                .into(),
        );
    }
    Ok(ValidatedSource {
        owner: context.clone(),
        plan: archived,
        archive: archive.identity.clone(),
        metadata_prefix: "archive-metadata",
    })
}

pub(crate) fn pending_preferences(
    capture: &crate::legacy_capture::CapturedSource,
    server: &str,
) -> Result<Vec<Value>, String> {
    let key = if server == "local" {
        "studio-native-preferences-journal-v1".to_owned()
    } else {
        format!(":server:{server}:studio-native-preferences-journal-v1")
    };
    let Some(raw) = capture.snapshot["preferences"].get(&key) else {
        return Ok(Vec::new());
    };
    let values: Vec<Value> = serde_json::from_str(
        raw.as_str()
            .ok_or("The original preference journal is invalid.")?,
    )
    .map_err(|_| "The original preference journal is invalid.")?;
    if values.len() > 10_000 {
        return Err("The original preference journal exceeds its supported request count.".into());
    }
    Ok(values)
}

fn stored_database_owner(name: &str) -> Result<String, String> {
    for prefix in ["codex-studio-uploads", "codex-studio-dictation"] {
        if name == prefix {
            return Ok("local".into());
        }
        if let Some(server) = name.strip_prefix(&format!("{prefix}:server:")) {
            return checked_server(server);
        }
    }
    if let Some(rest) = name.strip_prefix("rxdb-dexie-studio") {
        let rest = rest.strip_prefix("-entity-projection-").unwrap_or(rest);
        let suffix = rest
            .get(32..)
            .ok_or("The saved database workspace is incomplete.")?;
        if !rest[..32].bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("The saved database workspace is invalid.".into());
        }
        if suffix.starts_with("--") || suffix.starts_with('-') {
            return Ok("local".into());
        }
        let encoded = suffix
            .strip_prefix("server")
            .and_then(|suffix| suffix.split_once('-').map(|(server, _)| server))
            .ok_or("The saved database server scope is unknown.")?;
        if encoded.len() % 2 != 0 || !encoded.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return Err("The saved database server scope is invalid.".into());
        }
        let bytes = (0..encoded.len())
            .step_by(2)
            .map(|index| {
                u8::from_str_radix(&encoded[index..index + 2], 16)
                    .map_err(|_| "The saved database server encoding is invalid.")
            })
            .collect::<Result<Vec<_>, _>>()?;
        return checked_server(
            &String::from_utf8(bytes).map_err(|_| "The saved database server scope is invalid.")?,
        );
    }
    Err("The saved database has no maintained source owner. Its source remains held.".into())
}
fn checked_server(server: &str) -> Result<String, String> {
    if server.is_empty()
        || server.len() > 128
        || !server
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err("The saved server scope is invalid.".into());
    }
    Ok(server.into())
}

pub(crate) fn current_owners(
    capture: &crate::legacy_capture::CapturedSource,
) -> Result<BTreeSet<String>, String> {
    let mut owners = BTreeSet::from(["local".to_owned()]);
    for key in capture.snapshot["preferences"]
        .as_object()
        .ok_or("The current preference inventory is incomplete.")?
        .keys()
    {
        if let Some(rest) = key.strip_prefix(":server:") {
            owners.insert(checked_server(
                rest.split_once(':')
                    .ok_or("The saved preference server scope is incomplete.")?
                    .0,
            )?);
        }
    }
    for db in capture.snapshot["databases"]
        .as_array()
        .ok_or("The current database inventory is incomplete.")?
    {
        let name = text(db, "name")?;
        if name == "studio-monitor-input-journal-v1" {
            for store in db["stores"]
                .as_array()
                .ok_or("The monitor source is incomplete.")?
            {
                if text(store, "name")? != "inputs" {
                    return Err("The monitor source has an unknown store.".into());
                }
                for row in store["records"]
                    .as_array()
                    .ok_or("The monitor source records are incomplete.")?
                {
                    let key = decode(&row[0])?.json()?;
                    owners.insert(checked_server(
                        key[0]
                            .as_str()
                            .ok_or("The monitor source has no server identity.")?,
                    )?);
                }
            }
        } else {
            owners.insert(stored_database_owner(name)?);
        }
    }
    Ok(owners)
}

pub(crate) fn owner_capture(
    capture: &crate::legacy_capture::CapturedSource,
    server: &str,
) -> Result<crate::legacy_capture::CapturedSource, String> {
    let mut snapshot = capture.snapshot.clone();
    snapshot["preferences"]
        .as_object_mut()
        .ok_or("The current preference inventory is incomplete.")?
        .retain(|key, _| {
            if server == "local" {
                !key.starts_with(":server:")
            } else {
                key.starts_with(&format!(":server:{server}:"))
            }
        });
    let mut databases = Vec::new();
    for db in snapshot["databases"]
        .as_array()
        .ok_or("The current database inventory is incomplete.")?
    {
        let name = text(db, "name")?;
        if name == "studio-monitor-input-journal-v1" {
            let mut db = db.clone();
            for store in db["stores"]
                .as_array_mut()
                .ok_or("The monitor source is incomplete.")?
            {
                let records = store["records"]
                    .as_array()
                    .ok_or("The monitor source records are incomplete.")?;
                let mut selected = Vec::new();
                for record in records {
                    if decode(&record[0])?.json()?[0] == server {
                        selected.push(record.clone());
                    }
                }
                store["records"] = json!(selected);
            }
            databases.push(db);
        } else if stored_database_owner(name)? == server {
            databases.push(db.clone());
        }
    }
    snapshot["databases"] = json!(databases);
    Ok(crate::legacy_capture::CapturedSource {
        top_origin: capture.top_origin.clone(),
        origin: capture.origin.clone(),
        document_token: capture.document_token.clone(),
        snapshot,
        digest: capture.digest.clone(),
    })
}

// The fixed private-world reader owns this snapshot. This adapter never accepts
// renderer JSON or a renderer-selected archive path.
fn current_plan(
    context: &Context,
    capture: &crate::legacy_capture::CapturedSource,
    native_preferences: &Value,
) -> Result<Plan, String> {
    current_plan_for_owner(
        &SourceOwner::from_context(context),
        capture,
        native_preferences,
    )
}
fn current_plan_for_owner(
    context: &SourceOwner,
    capture: &crate::legacy_capture::CapturedSource,
    native_preferences: &Value,
) -> Result<Plan, String> {
    if capture.snapshot["version"] != 1 {
        return Err("Unsupported current browser source format.".into());
    }
    let owner = if context.server_id == "local" {
        "root".to_owned()
    } else {
        format!("server:{}", context.server_id)
    };
    if native_preferences["version"] != 1
        || native_preferences["legacyPreferenceScope"] != owner
        || !native_preferences["preferenceReceipts"].is_array()
    {
        return Err("The original native preference owner differs.".into());
    }
    let native = native_preferences["preferences"]
        .as_object()
        .ok_or("The original native preference snapshot is incomplete.")?;
    if native.values().any(|value| !value.is_string()) {
        return Err("The original native preference values are invalid.".into());
    }
    let prefix = if context.server_id == "local" {
        String::new()
    } else {
        format!(":server:{}:", context.server_id)
    };
    let mut snapshot = capture.snapshot.clone();
    let physical = snapshot["preferences"]
        .as_object()
        .ok_or("The physical browser preferences are incomplete.")?;
    if physical.keys().any(|key| {
        if prefix.is_empty() {
            key.starts_with(":server:")
        } else {
            !key.starts_with(&prefix)
        }
    }) {
        return Err("The physical preference inventory contains another owner.".into());
    }
    let ready_key = format!("{prefix}studio-native-preferences-ready-v1");
    if physical.get(&ready_key).is_some_and(|value| value != "1") {
        return Err("The original native preference adoption marker is invalid.".into());
    }
    // This is the original adapter's authority rule. Its physical preference
    // cache can be stale, including after the native owner clears every value.
    if physical.get(&ready_key).is_some()
        || !native.is_empty()
        || !native_preferences["preferenceReceipts"]
            .as_array()
            .unwrap()
            .is_empty()
    {
        let mut logical = Map::new();
        for (key, value) in physical {
            let key_without_scope = key
                .strip_prefix(&prefix)
                .ok_or("The physical preference belongs to another owner.")?;
            if key_without_scope == "studio-native-preferences-journal-v1"
                || key_without_scope == "studio-native-preferences-ready-v1"
                || key_without_scope == "studio-browser-profile-import-v1"
                || key_without_scope.starts_with("studio-native-draft-journal-v1:")
            {
                logical.insert(key.clone(), value.clone());
            }
        }
        for (key, value) in native {
            logical.insert(format!("{prefix}{key}"), value.clone());
        }
        snapshot["preferences"] = Value::Object(logical);
    }
    let archive = json!({"version":1,"id":format!("host-current:{}",capture.digest),"owners":{owner.clone():capture.origin},"snapshots":[snapshot]});
    // Version one has inline records and does not read a chunk path.
    let selected = crate::profile::select_sealed(&archive, std::path::Path::new(""), &owner)?;
    let mut plan = plan_for_owner(context, &selected)?;
    plan.metadata
        .as_object_mut()
        .unwrap()
        .remove("archiveManifest");
    plan.metadata["preferenceReceipts"] = native_preferences["preferenceReceipts"].clone();
    Ok(plan)
}

fn plan(context: &Context, source: &SealedSource<'_>) -> Result<Plan, String> {
    plan_for_owner(&SourceOwner::from_context(context), source)
}
fn plan_for_owner(context: &SourceOwner, source: &SealedSource<'_>) -> Result<Plan, String> {
    let owner = if context.server_id == "local" {
        "root".into()
    } else {
        format!("server:{}", context.server_id)
    };
    if source.identity["owner"] != owner {
        return Err("The sealed source belongs to another server owner.".into());
    }
    let raw = source.snapshot["preferences"]
        .as_object()
        .ok_or("Invalid archived preferences.")?;
    let prefix = if context.server_id == "local" {
        String::new()
    } else {
        format!(":server:{}:", context.server_id)
    };
    let mut preferences = Map::new();
    let mut pending_preferences = Vec::new();
    let mut pending_drafts = Vec::new();
    let journal_prefix = format!("studio-native-draft-journal-v1:{}:", context.workspace_id);
    let mut ordered_drafts = Vec::new();
    for (name, value) in raw {
        let name = name
            .strip_prefix(&prefix)
            .ok_or("Unsupported foreign archive preference scope.")?;
        if prefix.is_empty() && name.starts_with(":server:") {
            return Err("Unsupported foreign archive preference scope.".into());
        }
        let value = value.as_str().ok_or("Invalid archived preference value.")?;
        if name == "studio-native-preferences-journal-v1" {
            let writes: Vec<Value> = serde_json::from_str(value).map_err(|e| e.to_string())?;
            for mut write in writes {
                let method = text(&write, "method")?.to_owned();
                let params = write["params"]
                    .as_object_mut()
                    .ok_or("Invalid archived preference write.")?;
                let namespace = format!("browser:{}:", json!(owner));
                if method == "preferences.import" {
                    let entries = params["entries"]
                        .as_object()
                        .ok_or("Invalid preference import.")?;
                    let mapped: Map<String, Value> = entries
                        .iter()
                        .map(|(key, value)| (format!("{namespace}{key}"), value.clone()))
                        .collect();
                    params.insert("entries".into(), Value::Object(mapped));
                } else {
                    if !matches!(
                        method.as_str(),
                        "preferences.raw_set" | "preferences.raw_remove"
                    ) {
                        return Err("Unsupported archived preference write.".into());
                    }
                    let key = params["key"].as_str().ok_or("Invalid preference key.")?;
                    params.insert("key".into(), json!(format!("{namespace}{key}")));
                }
                pending_preferences.push(write);
            }
        } else if name.starts_with("studio-native-draft-journal-v1:") {
            let row: Value = serde_json::from_str(value).map_err(|e| e.to_string())?;
            let id = text(&row["edit"], "id")?;
            let sequence = row["sequence"]
                .as_u64()
                .filter(|n| *n > 0)
                .ok_or("Invalid draft journal sequence.")?;
            if row["version"] != 1
                || row["workspace"] != context.workspace_id
                || name != format!("{journal_prefix}{id}")
            {
                return Err("The draft journal scope differs.".into());
            }
            ordered_drafts.push((
                sequence,
                id.to_owned(),
                json!({"workspace":context.workspace_id,"id":id,"changes":row["edit"]["changes"]}),
            ));
        } else if name.starts_with("codex-drafts:")
            || name.starts_with("codex-chat-draft:")
            || name == "studio-browser-profile-import-v1"
        {
            // These sources can contain divergent current-browser branches.
            return Err("Unsupported browser draft snapshot or prior archive adoption. Its source remains held.".into());
        } else if name != "studio-native-preferences-ready-v1" {
            preferences.insert(name.into(), json!(value));
        }
    }
    ordered_drafts.sort_by(|a, b| (&a.0, &a.1).cmp(&(&b.0, &b.1)));
    pending_drafts.extend(ordered_drafts.into_iter().map(|(_, _, v)| v));
    let suffix = if context.server_id == "local" {
        String::new()
    } else {
        format!(":server:{}", context.server_id)
    };
    let rx_suffix = if context.server_id == "local" {
        String::new()
    } else {
        format!(
            "server{}",
            context
                .server_id
                .as_bytes()
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect::<String>()
        )
    };
    let rx = format!(
        "rxdb-dexie-studio{}{}--0--",
        context.workspace_id, rx_suffix
    );
    let mut uploads = BTreeMap::new();
    let mut contents = BTreeMap::new();
    let mut recordings = BTreeMap::new();
    let mut pcm: BTreeMap<String, BTreeMap<u64, Vec<u8>>> = BTreeMap::new();
    let mut outbox = Vec::new();
    let mut drafts = Vec::new();
    let mut monitor = Vec::new();
    let mut databases = BTreeSet::new();
    let mut budget = 0usize;
    for db in source.snapshot["databases"]
        .as_array()
        .ok_or("Invalid archived database inventory.")?
    {
        let name = text(db, "name")?;
        if !databases.insert(name) {
            return Err("Duplicate archive database.".into());
        }
        let upload_db = name == format!("codex-studio-uploads{suffix}");
        let recording_db = name == format!("codex-studio-dictation{suffix}");
        let draft_db = name == format!("{rx}drafts");
        let outbox_db = name == format!("{rx}outbox");
        let monitor_db = name == "studio-monitor-input-journal-v1";
        if !(upload_db || recording_db || draft_db || outbox_db || monitor_db) {
            return Err(format!("Unsupported archive database {name}."));
        }
        let mut stores = BTreeSet::new();
        for store in db["stores"].as_array().ok_or("Invalid archived stores.")? {
            let store_name = text(store, "name")?;
            if !stores.insert(store_name) {
                return Err("Duplicate archive store.".into());
            }
            if !((upload_db && matches!(store_name, "uploads" | "contents"))
                || (recording_db && matches!(store_name, "recordings" | "chunks"))
                || ((draft_db || outbox_db) && store_name == "docs")
                || (monitor_db && store_name == "inputs"))
            {
                return Err(format!("Unsupported archive store {store_name}."));
            }
            let mut keys = BTreeSet::new();
            for record in source.records(store)? {
                budget=budget.checked_add(record.to_string().len()).filter(|n|*n<=MAX_PREPARE_RECORD_BYTES).ok_or("The host archive preparation record budget was exceeded. Its source remains held.")?;
                let pair = record
                    .as_array()
                    .filter(|p| p.len() == 2)
                    .ok_or("Invalid archive record.")?;
                let key = decode(&pair[0])?.json()?;
                if !keys.insert(key.to_string()) {
                    return Err("Duplicate archive record key.".into());
                }
                let mut fields = decode(&pair[1])?.object()?;
                let binary = if upload_db {
                    fields.remove("bytes").or_else(|| fields.remove("file"))
                } else {
                    None
                };
                let audio = if recording_db && store_name == "chunks" {
                    fields.remove("pcm")
                } else {
                    None
                };
                let row = Saved::Object(fields).json()?;
                if upload_db {
                    if row["id"] != key {
                        return Err("The upload identity differs.".into());
                    }
                    let id = key.as_str().ok_or("Invalid upload key.")?.to_owned();
                    if store_name == "uploads" && uploads.insert(id.clone(), row).is_some() {
                        return Err("Duplicate archived upload.".into());
                    }
                    if let Some(bytes) = binary {
                        if contents.insert(id, bytes.bytes(None)?).is_some() {
                            return Err("Duplicate archived upload bytes.".into());
                        }
                    }
                } else if recording_db {
                    if store_name == "recordings" {
                        if row["id"] != key
                            || recordings
                                .insert(
                                    key.as_str().ok_or("Invalid recording key.")?.to_owned(),
                                    row,
                                )
                                .is_some()
                        {
                            return Err("The recording identity differs.".into());
                        }
                    } else {
                        let pair = key
                            .as_array()
                            .filter(|p| p.len() == 2)
                            .ok_or("Invalid PCM key.")?;
                        let id = text(&row, "id")?;
                        let index = row["index"].as_u64().ok_or("Invalid PCM index.")?;
                        if pair[0] != id || pair[1] != index {
                            return Err("The PCM key differs.".into());
                        }
                        let bytes = audio
                            .ok_or("Missing PCM bytes.")?
                            .bytes(Some("Int16Array"))?;
                        if bytes.len() % 2 != 0
                            || pcm
                                .entry(id.into())
                                .or_default()
                                .insert(index, bytes)
                                .is_some()
                        {
                            return Err("Invalid or duplicate PCM chunk.".into());
                        }
                    }
                } else if draft_db || outbox_db {
                    if row["id"] != key {
                        return Err("The archived document identity differs.".into());
                    }
                    let payload = text(&row, "payload")?;
                    let mut document: Value =
                        serde_json::from_str(payload).map_err(|e| e.to_string())?;
                    if draft_db {
                        if document["id"] != key {
                            return Err("The draft payload differs.".into());
                        }
                        let deleted = match row["_deleted"].as_bool() {
                            Some(value) => value,
                            None => match row["_deleted"].as_u64() {
                                Some(0) => false,
                                Some(1) => true,
                                _ => return Err("Invalid archived draft deletion state.".into()),
                            },
                        };
                        drafts.push(
                            json!({"id":key,"payload":payload,"seq":row["seq"],"_deleted":deleted}),
                        );
                    } else {
                        if !(row["_deleted"] == false || row["_deleted"] == 0)
                            || document["body"]["id"] != key
                        {
                            return Err("Unsupported deleted or conflicting outbox row.".into());
                        }
                        let object = document.as_object_mut().ok_or("Invalid outbox payload.")?;
                        object.insert("id".into(), key);
                        object.insert("workspace".into(), json!(context.workspace_id));
                        object.entry("displayPending").or_insert(json!(true));
                        object.entry("attachments").or_insert(json!([]));
                        outbox.push(document);
                    }
                } else {
                    let pair = key
                        .as_array()
                        .filter(|p| p.len() == 2)
                        .ok_or("Invalid monitor input key.")?;
                    if pair[0] != context.server_id {
                        return Err("The monitor input belongs to another server.".into());
                    }
                    let mut row = row.as_object().ok_or("Invalid monitor input.")?.clone();
                    row.insert("monitorId".into(), pair[1].clone());
                    monitor.push(Value::Object(row));
                }
            }
        }
    }
    let mut artifacts = Vec::new();
    for (id, document) in uploads {
        let bytes = contents
            .remove(&id)
            .ok_or("The upload bytes are missing.")?;
        if document["workspace"] != context.workspace_id
            || document["size"].as_u64() != Some(bytes.len() as u64)
        {
            return Err("The upload scope or byte length differs.".into());
        }
        artifacts.push(Artifact { entry:json!({"kind":"upload","id":id,"bytes":bytes.len(),"sha256":hash(&bytes),"document":document}),bytes });
    }
    if !contents.is_empty() {
        return Err("Unsupported orphan upload bytes.".into());
    }
    for (id, document) in recordings {
        let mut bytes = Vec::new();
        for (expected, (index, chunk)) in
            pcm.remove(&id).unwrap_or_default().into_iter().enumerate()
        {
            if index != expected as u64 {
                return Err("The recording chunk interval is incomplete.".into());
            }
            bytes.extend(chunk);
        }
        if document["samples"].as_u64().and_then(|n| n.checked_mul(2)) != Some(bytes.len() as u64) {
            return Err("The recording byte length differs.".into());
        }
        artifacts.push(Artifact { entry:json!({"kind":"recording","id":id,"bytes":bytes.len(),"sha256":hash(&bytes),"document":document}),bytes });
    }
    if !pcm.is_empty() {
        return Err("Unsupported orphan PCM chunks.".into());
    }
    for (key, value) in &preferences {
        for (prefix, cancelled) in [
            ("studio-upload-cancelled:", true),
            ("studio-upload-settled:", false),
        ] {
            if let Some(id) = key.strip_prefix(prefix) {
                if value != "true" {
                    return Err("Invalid archived upload settlement.".into());
                }
                artifacts.push(Artifact {entry:json!({"kind":"uploadTombstone","id":id,"bytes":0,"sha256":hash(&[]),"cancelled":cancelled}),bytes:Vec::new()});
            }
        }
    }
    artifacts.sort_by(|a, b| {
        (a.entry["kind"].as_str(), a.entry["id"].as_str())
            .cmp(&(b.entry["kind"].as_str(), b.entry["id"].as_str()))
    });
    let mut ids = BTreeSet::new();
    if artifacts
        .iter()
        .any(|a| !ids.insert(a.entry["id"].as_str().unwrap().to_owned()))
    {
        return Err("Conflicting archived byte identities.".into());
    }
    let expected: Vec<_> = (0..artifacts.len())
        .map(|i| format!("archive-byte:v1:{}:{i}", context.source_identity))
        .collect();
    Ok(Plan {
        artifacts,
        caches: Vec::new(),
        metadata: json!({"version":1,"sourceIdentity":context.source_identity,"expectedByteImports":expected,
        "archiveManifest":{"version":1,"archive":{"id":source.identity["id"],"owner":owner,"sha256":source.identity["sha256"],"sourceIdentity":context.source_identity},"excludedCaches":[]},
        "preferences":preferences,"legacyPreferenceScope":owner,"pendingPreferences":pending_preferences,"pendingDrafts":pending_drafts,
        "draftDocuments":if drafts.is_empty(){json!([])}else{json!([{"workspace":context.workspace_id,"documents":drafts}])},"monitorInputs":monitor,"outbox":outbox}),
    })
}
async fn call(client: &Client, method: &str, params: Value) -> Result<Value, String> {
    let response = client
        .request(Request {
            id: studio_client_core::server_keys::random_id()?,
            method: method.into(),
            params,
        })
        .await?;
    response.result.ok_or_else(|| {
        response
            .error
            .map(|e| e.message)
            .unwrap_or("The archive import response is missing.".into())
    })
}
pub(crate) async fn prepare(context: &Context, source: &SealedSource<'_>) -> Result<Value, String> {
    if let Some(backend) = &context.backend {
        backend.validate_identity()?;
    }
    // Decode every supported source before creating even the first import stage.
    let plan = plan(context, source)?;
    execute_plan(context, plan, source.identity.clone(), "archive-metadata").await
}

pub(crate) fn validate_current(
    context: &Context,
    capture: &crate::legacy_capture::CapturedSource,
    native_preferences: &Value,
) -> Result<(), String> {
    current_plan(context, capture, native_preferences).map(|_| ())
}

pub(crate) async fn prepare_current(
    context: &Context,
    capture: &crate::legacy_capture::CapturedSource,
    native_preferences: &Value,
) -> Result<Value, String> {
    if let Some(backend) = &context.backend {
        backend.validate_identity()?;
    }
    let mut plan = current_plan(context, capture, native_preferences)?;
    plan.metadata["expectedByteImports"] = json!(
        (0..plan.artifacts.len())
            .map(|index| format!("current-byte:v1:{}:{index}", context.source_identity))
            .collect::<Vec<_>>()
    );
    execute_plan(context, plan, Value::Null, "current-metadata").await
}

async fn execute_plan(
    context: &Context,
    plan: Plan,
    archive: Value,
    metadata_prefix: &str,
) -> Result<Value, String> {
    let mut receipts = Vec::new();
    for (index, artifact) in plan.artifacts.iter().enumerate() {
        let id = plan.metadata["expectedByteImports"][index]
            .as_str()
            .unwrap();
        let manifest = json!({"version":1,"id":id,"source":context.source_identity,"workspace":context.workspace_id,"entries":[artifact.entry]});
        let begin = call(
            &context.client,
            "files.importBegin",
            json!({"manifest":manifest}),
        )
        .await?;
        if begin["manifestId"] != id {
            return Err("The byte import manifest identity differs.".into());
        }
        match begin["status"].as_str() {
            Some("pending") => {
                let size = begin["chunkBytes"]
                    .as_u64()
                    .and_then(|n| usize::try_from(n).ok())
                    .filter(|n| *n > 0)
                    .ok_or("Invalid native import chunk bound.")?;
                for (chunk, bytes) in artifact.bytes.chunks(size).enumerate() {
                    let offset = chunk * size;
                    let reply=call(&context.client,"files.importChunk",json!({"manifestId":id,"entry":0,"offset":offset,"base64":STANDARD.encode(bytes)})).await?;
                    if reply["nextOffset"].as_u64() != Some((offset + bytes.len()) as u64) {
                        return Err("The imported byte offset differs.".into());
                    }
                }
            }
            Some("complete") => {}
            _ => return Err("Invalid native byte import phase.".into()),
        }
        let receipt = call(
            &context.client,
            "files.importCommit",
            json!({"manifestId":id}),
        )
        .await?;
        if call(
            &context.client,
            "files.importReadback",
            json!({"manifestId":id}),
        )
        .await?
            != receipt
            || receipt["manifestHash"] != begin["manifestHash"]
            || receipt["source"] != context.source_identity
            || receipt["workspace"] != context.workspace_id
        {
            return Err("The native byte import readback differs.".into());
        }
        receipts.push(json!({"manifestId":id,"receipt":receipt}));
    }
    let serialized = serde_json::to_string(&plan.metadata).map_err(|e| e.to_string())?;
    let id = format!("{metadata_prefix}:v1:{}", context.source_identity);
    let receipt = call(
        &context.client,
        "client_storage.import",
        json!({"manifestId":id,"serializedSource":serialized}),
    )
    .await?;
    if call(
        &context.client,
        "client_storage.status",
        json!({"manifestId":id}),
    )
    .await?
        != receipt
        || receipt["sourceSha256"] != hash(serialized.as_bytes())
        || receipt["expectedByteImports"] != plan.metadata["expectedByteImports"]
        || receipt["archiveManifest"] != plan.metadata["archiveManifest"]
    {
        return Err("The native metadata import readback differs.".into());
    }
    Ok(
        json!({"prepared":true,"complete":false,"sourceIdentity":context.source_identity,"archive":if archive.is_null(){Value::Null}else{json!({"id":archive["id"],"owner":archive["owner"],"sha256":archive["sha256"]})},"metadata":[{"manifestId":id,"sourceSha256":receipt["sourceSha256"]}],"bytes":receipts}),
    )
}

#[cfg(test)]
#[path = "archive_import_tests.rs"]
mod tests;
