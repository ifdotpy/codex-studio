//! Atomic application of scoped subscription values and their durable cursors.

use crate::{projections, protocol::Request};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use studio_runtime_schema::{shared_values, sync_protocol::SubscriptionEntityScope};

pub(crate) fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_subscription_views (
        workspace TEXT NOT NULL, resource_key TEXT NOT NULL, document TEXT NOT NULL,
        PRIMARY KEY(workspace,resource_key));
        CREATE TABLE IF NOT EXISTS client_subscription_transfers (
        workspace TEXT NOT NULL, resource_key TEXT NOT NULL, metadata TEXT NOT NULL,
        page_index INTEGER NOT NULL, part_index INTEGER NOT NULL, part_count INTEGER NOT NULL,
        interval_after INTEGER, through_seq INTEGER, result TEXT, final_frame TEXT,
        PRIMARY KEY(workspace,resource_key));
        CREATE TABLE IF NOT EXISTS client_subscription_fragments (
        workspace TEXT NOT NULL, resource_key TEXT NOT NULL, page_index INTEGER NOT NULL,
        part_index INTEGER NOT NULL, frame TEXT NOT NULL,
        PRIMARY KEY(workspace,resource_key,page_index,part_index));
        CREATE TABLE IF NOT EXISTS client_subscription_stage_rows (
        workspace TEXT NOT NULL, resource_key TEXT NOT NULL, entity_key TEXT NOT NULL,
        seq INTEGER NOT NULL, document TEXT NOT NULL,
        PRIMARY KEY(workspace,resource_key,entity_key));",
    )
}

fn validate(name: &str, value: &Value) -> Result<(), String> {
    match studio_runtime_schema::first_error(name, value)? {
        None => Ok(()),
        Some((path, detail)) => Err(format!("Invalid {name} at {path}: {detail}")),
    }
}

/// The renderer hashes JSON entry pairs, not a JSON object. Resource field names
/// are ASCII identifiers in the canonical ResourceRef schema.
fn resource_key(resource: &Value) -> Result<String, String> {
    validate("ResourceRef", resource)?;
    let fields = resource.as_object().ok_or("Missing resource identity.")?;
    let sorted: BTreeMap<_, _> = fields.iter().collect();
    let pairs: Vec<_> = sorted
        .into_iter()
        .map(|(key, value)| json!([key, value]))
        .collect();
    let encoded = serde_json::to_vec(&pairs).map_err(|error| error.to_string())?;
    let digest = Sha256::digest(encoded);
    let digest: String = digest.iter().map(|byte| format!("{byte:02x}")).collect();
    Ok(format!("subscription:cursor:{digest}"))
}

fn read(
    connection: &Connection,
    workspace: &str,
    key: &str,
    resource: &Value,
) -> Result<Value, String> {
    let saved: Option<String> = connection
        .query_row(
            "SELECT document FROM client_subscription_views WHERE workspace=?1 AND resource_key=?2",
            params![workspace, key],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    saved
        .map(|saved| serde_json::from_str(&saved).map_err(|error| error.to_string()))
        .unwrap_or_else(|| {
            Ok(json!({"resource":resource,"version":null,"data":null,"status":"recovery_required"}))
        })
}

fn save(connection: &Connection, workspace: &str, key: &str, result: &Value) -> Result<(), String> {
    connection
        .execute(
            "INSERT INTO client_subscription_views VALUES(?1,?2,?3)
        ON CONFLICT(workspace,resource_key) DO UPDATE SET document=excluded.document",
            params![workspace, key, result.to_string()],
        )
        .map_err(|error| error.to_string())?;
    if let Some(cursor) = result.get("cursor") {
        let seq = number(cursor, "seq")?;
        let document = projections::Document {
            id: key.into(),
            payload: cursor.to_string(),
            seq,
            deleted: false,
        };
        connection
            .execute(
                "INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,?2,?3,?4)
            ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq",
                params![
                    workspace,
                    key,
                    serde_json::to_string(&document).map_err(|error| error.to_string())?,
                    seq as i64
                ],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn recovery(
    connection: &Connection,
    workspace: &str,
    key: &str,
    previous: &Value,
) -> Result<Value, String> {
    let mut result = previous.clone();
    result["status"] = json!("recovery_required");
    save(connection, workspace, key, &result)?;
    Ok(result)
}

fn number(value: &Value, field: &str) -> Result<u64, String> {
    value[field]
        .as_u64()
        .filter(|value| *value <= studio_client_domain::MAX_SAFE_INTEGER as u64)
        .ok_or_else(|| format!("Invalid {field} sequence."))
}

fn committed(resource: &Value, epoch: &str, entry: &Value, data: Value) -> Value {
    let mut version = entry.clone();
    version["epoch"] = json!(epoch);
    version["resource"] = resource.clone();
    let mut result = json!({"resource":resource,"version":version,"data":data,"status":"ready"});
    if let Some(cursor) = entry.get("cursor") {
        result["cursor"] = cursor.clone();
    }
    result
}

fn cursor_seq(value: &Value) -> Option<u64> {
    value["cursor"]["seq"].as_u64()
}

struct Selection<'a> {
    resource: &'a Value,
    scope: &'a SubscriptionEntityScope,
    agents: BTreeSet<String>,
}

impl<'a> Selection<'a> {
    fn new(connection: &Connection, workspace: &str, resource: &'a Value) -> Result<Self, String> {
        let kind = resource["kind"].as_str().ok_or("Missing resource kind.")?;
        let scope = studio_runtime_schema::sync_protocol::subscription_entity_scopes()
            .get(kind)
            .ok_or("This resource has no entity scope.")?;
        let mut agents = BTreeSet::new();
        if let Some(agent) = scope
            .agent_field
            .as_ref()
            .and_then(|field| resource[field].as_str())
        {
            agents.insert(agent.into());
            if scope.include_team_members {
                for row in projections::entities(connection, workspace)? {
                    if row["collection"] == "agentSummary"
                        && row["op"] != "delete"
                        && row["value"]["rootId"] == agent
                    {
                        if let Some(id) = row["id"].as_str() {
                            agents.insert(id.into());
                        }
                    }
                }
            }
        }
        Ok(Self {
            resource,
            scope,
            agents,
        })
    }
    fn belongs(&self, row: &Value) -> bool {
        if !self
            .scope
            .collections
            .iter()
            .any(|collection| row["collection"] == *collection)
        {
            return false;
        }
        if self
            .scope
            .id_field
            .as_ref()
            .is_some_and(|field| row["id"] != self.resource[field])
        {
            return false;
        }
        let Some(agent) = self
            .scope
            .agent_field
            .as_ref()
            .and_then(|field| self.resource[field].as_str())
        else {
            return true;
        };
        let Some(value) = row.get("value") else {
            return true;
        };
        value["rootId"] == agent
            || (self.scope.include_team_members
                && value["agent"]
                    .as_str()
                    .is_some_and(|agent| self.agents.contains(agent)))
    }
}

fn write_entity(
    connection: &Connection,
    workspace: &str,
    selection: &Selection<'_>,
    row: &Value,
) -> Result<String, String> {
    let key = projections::entity_scope(row)?;
    if !selection.belongs(row) {
        return Err("Entity is outside the subscription scope.".into());
    }
    if row["op"] == "delete" {
        if let Some(previous) = projections::get(connection, workspace, &key)? {
            if !selection.belongs(&previous) {
                return Err("Entity deletion is outside the subscription scope.".into());
            }
        }
    }
    connection
        .execute(
            "INSERT INTO client_projections(workspace,scope,document,seq) VALUES(?1,?2,?3,?4)
        ON CONFLICT(workspace,scope) DO UPDATE SET document=excluded.document,seq=excluded.seq
        WHERE excluded.seq>client_projections.seq",
            params![workspace, key, row.to_string(), number(row, "seq")? as i64],
        )
        .map_err(|error| error.to_string())?;
    Ok(key)
}

fn prune(
    connection: &Connection,
    workspace: &str,
    selection: &Selection<'_>,
    through: u64,
    mut contains: impl FnMut(&str) -> Result<bool, String>,
) -> Result<(), String> {
    for row in projections::entities(connection, workspace)? {
        let key = projections::entity_scope(&row)?;
        if number(&row, "seq")? <= through && selection.belongs(&row) && !contains(&key)? {
            // Preserve the absence fence for overlapping resource cursors.
            let tombstone =
                json!({"op":"delete","collection":row["collection"],"id":row["id"],"seq":through});
            connection.execute("UPDATE client_projections SET document=?3,seq=?4 WHERE workspace=?1 AND scope=?2",
                params![workspace,key,tombstone.to_string(),through as i64]).map_err(|error|error.to_string())?;
        }
    }
    Ok(())
}

fn entity_data(
    connection: &Connection,
    workspace: &str,
    selection: &Selection<'_>,
) -> Result<Value, String> {
    let rows: Vec<Value> = projections::entities(connection, workspace)?
        .into_iter()
        .filter(|row| row["op"] != "delete" && selection.belongs(row))
        .collect();
    let values: Vec<_> = rows.iter().map(|row| row["value"].clone()).collect();
    match selection.resource["kind"].as_str().unwrap_or("") {
        "agentDetail" | "task" | "session-cost" | "desktop" | "terminal" => {
            Ok(values.into_iter().next().unwrap_or(Value::Null))
        }
        "tasks" | "monitors" | "costs" | "terminals" => Ok(json!(values)),
        "work" => Ok(work_data(
            &rows,
            selection.resource["agentId"].as_str().unwrap_or(""),
        )),
        _ => Ok(json!(
            rows.into_iter()
                .map(|mut row| {
                    row.as_object_mut().unwrap().remove("op");
                    row.as_object_mut().unwrap().remove("seq");
                    row
                })
                .collect::<Vec<_>>()
        )),
    }
}

fn work_data(rows: &[Value], root: &str) -> Value {
    let accepted: BTreeSet<_> = rows
        .iter()
        .filter(|row| row["collection"] == "workDetail" && row["value"]["status"] == "accepted")
        .filter_map(|row| row["id"].as_str())
        .collect();
    let mut tasks = Vec::new();
    for row in rows.iter().filter(|row| row["collection"] == "workDetail") {
        let mut task = row["value"].clone();
        let blocked: Vec<_> = task["dependencies"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|id| id.as_str().is_none_or(|id| !accepted.contains(id)))
            .cloned()
            .collect();
        task["displayStatus"] = if matches!(task["status"].as_str(), Some("ready" | "running"))
            && !blocked.is_empty()
        {
            json!("blocked")
        } else {
            task["status"].clone()
        };
        task["blockedBy"] = json!(blocked);
        let mut history: Vec<_> = rows
            .iter()
            .filter(|item| item["value"]["taskId"] == row["id"])
            .collect();
        history.sort_by(|left, right| {
            left["value"]["index"]
                .as_u64()
                .cmp(&right["value"]["index"].as_u64())
                .then_with(|| {
                    left["collection"]
                        .as_str()
                        .cmp(&right["collection"].as_str())
                })
        });
        for (collection, field) in [
            ("workResult", "results"),
            ("workDecision", "decisions"),
            ("workRelease", "releases"),
        ] {
            task[field] = json!(
                history
                    .iter()
                    .filter(|row| row["collection"] == collection)
                    .map(|row| row["value"]["value"].clone())
                    .collect::<Vec<_>>()
            );
        }
        tasks.push(task);
    }
    json!({"rootId":root,"items":tasks,"tasks":tasks})
}

fn checked_interval(entry: &Value, batch: &Value) -> Result<(u64, u64, bool), String> {
    let after = number(batch, "after")?;
    let through = number(batch, "through")?;
    let snapshot = entry["replyKind"] == "snapshot";
    if through < after
        || cursor_seq(entry) != Some(through)
        || number(entry, "revision")? != through
        || (snapshot && after != 0)
    {
        return Err("Invalid subscription interval.".into());
    }
    Ok((after, through, snapshot))
}

fn apply_entity_batch(
    connection: &Connection,
    workspace: &str,
    resource: &Value,
    previous: &Value,
    entry: &Value,
) -> Result<Option<Value>, String> {
    let batch = &entry["entityChanges"];
    let (after, through, snapshot) = checked_interval(entry, batch)?;
    if !snapshot && cursor_seq(previous).is_none_or(|seq| seq < after) {
        return Ok(None);
    }
    let selection = Selection::new(connection, workspace, resource)?;
    let mut incoming = BTreeSet::new();
    let mut sequence = after;
    for row in batch["documents"]
        .as_array()
        .ok_or("Missing entity documents.")?
    {
        let seq = number(row, "seq")?;
        if seq <= sequence || seq > through {
            return Err("Entity is outside its complete interval.".into());
        }
        sequence = seq;
        let key = write_entity(connection, workspace, &selection, row)?;
        if !incoming.insert(key) {
            return Err("Duplicate entity in subscription interval.".into());
        }
    }
    if snapshot {
        prune(connection, workspace, &selection, through, |key| {
            Ok(incoming.contains(key))
        })?;
    }
    projections::refresh_entity_state(connection, workspace)?;
    entity_data(connection, workspace, &selection).map(Some)
}

fn transcript(previous: &Value, entry: &Value) -> Result<Option<Value>, String> {
    if let Some(snapshot) = entry
        .get("transcriptSnapshot")
        .filter(|value| !value.is_null())
    {
        if entry["replyKind"] != "snapshot" || cursor_seq(entry) != entry["revision"].as_u64() {
            return Err("Invalid transcript snapshot interval.".into());
        }
        return Ok(Some(snapshot.clone()));
    }
    let batch = &entry["transcriptChanges"];
    let (after, _, snapshot) = checked_interval(entry, batch)?;
    if snapshot || cursor_seq(previous) != Some(after) || !previous["data"]["items"].is_array() {
        return Ok(None);
    }
    let delta = &batch["delta"];
    let old = previous["data"]["items"].as_array().unwrap();
    let removed: BTreeSet<_> = delta["removed"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(Value::as_str)
        .collect();
    let mut items: BTreeMap<String, Value> = old
        .iter()
        .filter(|item| !removed.contains(item["id"].as_str().unwrap_or("")))
        .map(|item| (item["id"].as_str().unwrap_or("").into(), item.clone()))
        .collect();
    for item in delta["items"].as_array().unwrap() {
        items.insert(item["id"].as_str().unwrap_or("").into(), item.clone());
    }
    let mut order: Vec<String> = if let Some(order) = delta["order"].as_array() {
        order.iter().map(|id| id.as_str().unwrap().into()).collect()
    } else {
        old.iter()
            .filter_map(|item| item["id"].as_str())
            .filter(|id| !removed.contains(id))
            .map(str::to_owned)
            .collect()
    };
    if delta.get("order").is_none() {
        for item in delta["items"].as_array().unwrap() {
            let id = item["id"].as_str().unwrap().to_owned();
            if !order.contains(&id) {
                order.push(id);
            }
        }
    }
    let mut truncated = false;
    if previous["data"]["truncated"] == true {
        let start = order
            .iter()
            .rposition(|id| !items.contains_key(id))
            .map_or(0, |index| index + 1);
        truncated = start > 0 || delta.get("order").is_none();
        order = order.split_off(start);
    }
    let unique: BTreeSet<_> = order.iter().collect();
    if unique.len() != order.len() || order.iter().any(|id| !items.contains_key(id)) {
        return Ok(None);
    }
    let mut data = previous["data"].clone();
    for (field, value) in delta.as_object().unwrap() {
        if !matches!(field.as_str(), "delta" | "items" | "removed" | "order") {
            data[field] = value.clone();
        }
    }
    data["items"] = json!(order.iter().map(|id| items[id].clone()).collect::<Vec<_>>());
    if truncated {
        data["truncated"] = json!(true);
        if let Some(first) = order.first() {
            data["nextCursor"] = json!(first);
        }
    }
    Ok(Some(data))
}

fn terminal(previous: &Value, entry: &Value) -> Result<Option<Value>, String> {
    let incoming = &entry["viewSnapshot"]["terminalOutput"];
    let chars: Vec<char> = incoming["text"]
        .as_str()
        .ok_or("Missing terminal output.")?
        .chars()
        .collect();
    let offset = number(incoming, "offset")?;
    let Some(start) = offset.checked_sub(chars.len() as u64) else {
        return Err("Invalid terminal output interval.".into());
    };
    if cursor_seq(entry) != Some(offset) {
        return Err("Invalid terminal output cursor.".into());
    }
    let mut joined = Vec::new();
    if entry["replyKind"] == "delta" {
        let base = &previous["data"];
        let Some(old_offset) = base["offset"].as_u64() else {
            return Ok(None);
        };
        let old: Vec<char> = base["text"].as_str().unwrap_or("").chars().collect();
        let Some(old_start) = old_offset.checked_sub(old.len() as u64) else {
            return Ok(None);
        };
        if old_offset < start || old_start > start || old_offset > offset {
            return Ok(None);
        }
        joined.extend(old.into_iter().take((start - old_start) as usize));
    } else if entry["replyKind"] != "snapshot" {
        return Ok(None);
    }
    joined.extend(chars);
    let length = joined.len();
    let mut data = incoming.clone();
    data["text"] = json!(
        joined
            .into_iter()
            .skip(length.saturating_sub(shared_values::SYNC_TERMINAL_STREAM_MAX_CHARS))
            .collect::<String>()
    );
    data["truncated"] = json!(
        incoming["truncated"] == true
            || (entry["replyKind"] == "delta" && previous["data"]["truncated"] == true)
            || length > shared_values::SYNC_TERMINAL_STREAM_MAX_CHARS
    );
    Ok(Some(data))
}

fn apply(
    connection: &Connection,
    workspace: &str,
    resource: &Value,
    key: &str,
    epoch: &str,
    entry: &Value,
) -> Result<Value, String> {
    validate("ResourceRevisionEntry", entry)?;
    validate_payload_owner(resource, entry)?;
    if let Some(reference) = entry.get("resource").filter(|value| !value.is_null()) {
        if reference != resource {
            return Err("Subscription resource identity changed.".into());
        }
    }
    if let Some(cursor) = entry.get("cursor") {
        if cursor["workspaceId"] != workspace {
            return Err("Foreign subscription cursor.".into());
        }
    }
    let previous = read(connection, workspace, key, resource)?;
    let kind = resource["kind"].as_str().unwrap();
    if matches!(kind, "multi-server" | "voice") {
        let field = if kind == "multi-server" {
            "discoverySnapshot"
        } else {
            "voiceRecords"
        };
        if let Some(data) = entry.get(field).filter(|value| !value.is_null()) {
            // These owners explicitly prohibit durable client-cache storage.
            let mut result = committed(resource, epoch, entry, data.clone());
            result.as_object_mut().unwrap().remove("cursor");
            result["version"].as_object_mut().unwrap().remove("cursor");
            return Ok(result);
        }
    }
    let same_epoch = previous["version"]["epoch"] == epoch;
    if (same_epoch
        && previous["version"]["revision"]
            .as_u64()
            .is_some_and(|revision| revision > entry["revision"].as_u64().unwrap()))
        || (kind != "terminal-output"
            && cursor_seq(entry)
                .zip(cursor_seq(&previous))
                .is_some_and(|(incoming, old)| incoming < old))
    {
        return recovery(connection, workspace, key, &previous);
    }
    if entry["entitySequenceReset"] == true && entry["replyKind"] != "snapshot" {
        return recovery(connection, workspace, key, &previous);
    }
    let data = if !entry["entityChanges"].is_null() {
        apply_entity_batch(connection, workspace, resource, &previous, entry)?
    } else if kind == "drafts" && !entry["draftChanges"].is_null() {
        let batch = &entry["draftChanges"];
        let (after, _, snapshot) = checked_interval(entry, batch)?;
        if !snapshot && cursor_seq(&previous) != Some(after) {
            None
        } else {
            crate::draft_store::apply_in_transaction(connection, workspace, batch)?;
            Some(batch.clone())
        }
    } else if kind == "transcript"
        && (!entry["transcriptSnapshot"].is_null() || !entry["transcriptChanges"].is_null())
    {
        transcript(&previous, entry)?
    } else if kind == "terminal-output" && !entry["viewSnapshot"]["terminalOutput"].is_null() {
        terminal(&previous, entry)?
    } else if kind == "room"
        && resource["legacy"] != true
        && !entry["viewSnapshot"]["room"].is_null()
    {
        let room = &entry["viewSnapshot"]["room"];
        let messages = room["messages"].as_array().unwrap();
        if entry["replyKind"] != "snapshot"
            || cursor_seq(entry)
                != Some(
                    messages
                        .last()
                        .and_then(|message| message["seq"].as_u64())
                        .unwrap_or(0),
                )
            || room["room"]["id"] != resource["roomId"]
            || messages
                .iter()
                .any(|message| message["room"] != resource["roomId"])
        {
            return Err("Invalid room snapshot boundary.".into());
        }
        Some(room.clone())
    } else {
        let field = match kind {
            "processes" => "processes",
            "complaint" => "complaint",
            "room" if resource["legacy"] == true => "legacyMessages",
            _ => "",
        };
        entry["viewSnapshot"].get(field).cloned()
    };
    let Some(data) = data else {
        // A replay with no records can confirm only an already committed cursor.
        if entry["replyKind"] == "delta"
            && cursor_seq(entry).is_some()
            && cursor_seq(entry) == cursor_seq(&previous)
            && previous["data"] != Value::Null
        {
            let mut result = previous;
            result["status"] = json!("ready");
            save(connection, workspace, key, &result)?;
            return Ok(result);
        }
        return recovery(connection, workspace, key, &previous);
    };
    let result = committed(resource, epoch, entry, data);
    save(connection, workspace, key, &result)?;
    Ok(result)
}

pub(crate) fn execute(
    connection: &Connection,
    workspace: &str,
    request: &Request,
) -> Result<Value, String> {
    let transaction = Transaction::new_unchecked(connection, TransactionBehavior::Immediate)
        .map_err(|error| error.to_string())?;
    let identity: Option<String> = transaction
        .query_row(
            "SELECT value FROM client_identity WHERE singleton=1",
            [],
            |row| row.get(0),
        )
        .optional()
        .map_err(|error| error.to_string())?;
    if let Some(identity) = identity {
        let identity: Value = serde_json::from_str(&identity).map_err(|error| error.to_string())?;
        if identity["workspaceId"] != workspace {
            return Err("The server workspace changed.".into());
        }
    }
    let result = if request.method == "projection.stageSubscriptionFragment" {
        stage(&transaction, workspace, &request.params["frame"])?
    } else {
        let resource = &request.params["resource"];
        let key = resource_key(resource)?;
        if request.method == "projection.readSubscription" {
            let mut saved = read(&transaction, workspace, &key, resource)?;
            saved["status"] = json!("recovery_required");
            saved
        } else {
            let epoch = request.params["epoch"]
                .as_str()
                .filter(|value| !value.is_empty())
                .ok_or("Missing subscription epoch.")?;
            apply(
                &transaction,
                workspace,
                resource,
                &key,
                epoch,
                &request.params["entry"],
            )?
        }
    };
    transaction.commit().map_err(|error| error.to_string())?;
    Ok(result)
}

fn validate_payload_owner(resource: &Value, entry: &Value) -> Result<(), String> {
    let kind = resource["kind"].as_str().ok_or("Missing resource kind.")?;
    for field in [
        "entityChanges",
        "transcriptChanges",
        "transcriptSnapshot",
        "draftChanges",
        "discoverySnapshot",
        "voiceRecords",
        "viewSnapshot",
    ] {
        if entry.get(field).is_none_or(Value::is_null) {
            continue;
        }
        let allowed = match field {
            "entityChanges" => studio_runtime_schema::sync_protocol::subscription_entity_scopes()
                .contains_key(kind),
            "transcriptChanges" | "transcriptSnapshot" => kind == "transcript",
            "draftChanges" => kind == "drafts",
            "discoverySnapshot" => kind == "multi-server",
            "voiceRecords" => kind == "voice",
            "viewSnapshot" => {
                matches!(kind, "processes" | "complaint" | "room" | "terminal-output")
            }
            _ => false,
        };
        if !allowed {
            return Err("Inline payload belongs to another resource owner.".into());
        }
    }
    if let Some(view) = entry["viewSnapshot"].as_object() {
        let field = match kind {
            "processes" => "processes",
            "complaint" => "complaint",
            "room" if resource["legacy"] == true => "legacyMessages",
            "room" => "room",
            "terminal-output" => "terminalOutput",
            _ => "",
        };
        if view.keys().any(|key| key != field) {
            return Err("Inline view belongs to another resource owner.".into());
        }
    }
    if !entry["transcriptSnapshot"].is_null() && !entry["transcriptChanges"].is_null() {
        return Err("Transcript snapshot and delta cannot share an interval.".into());
    }
    Ok(())
}

/// Decode one page from SQLite without collecting its serialized fragments.
/// Completed pages become individual staged rows, never one in-memory interval.
struct FragmentReader<'a> {
    connection: &'a Connection,
    workspace: &'a str,
    key: &'a str,
    page: u64,
    count: u64,
    next: u64,
    chunk: std::io::Cursor<Vec<u8>>,
}

impl std::io::Read for FragmentReader<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        if buffer.is_empty() {
            return Ok(0);
        }
        loop {
            let read = std::io::Read::read(&mut self.chunk, buffer)?;
            if read != 0 || self.next == self.count {
                return Ok(read);
            }
            let data:String=self.connection.query_row(
                "SELECT json_extract(frame,'$.data') FROM client_subscription_fragments WHERE workspace=?1 AND resource_key=?2 AND page_index=?3 AND part_index=?4",
                params![self.workspace,self.key,self.page as i64,self.next as i64],|row|row.get(0))
                .map_err(|error|std::io::Error::other(error.to_string()))?;
            self.chunk = std::io::Cursor::new(data.into_bytes());
            self.next += 1;
        }
    }
}

fn stage(connection: &Connection, workspace: &str, frame: &Value) -> Result<Value, String> {
    validate("ResourceEntityTransferEvent", frame)?;
    if serde_json::to_vec(frame)
        .map_err(|error| error.to_string())?
        .len()
        + "event: entity-transfer\ndata: \n\n".len()
        > shared_values::SYNC_STREAM_EVENT_MAX_BYTES
    {
        return Err("Entity transfer frame exceeds the stream byte limit.".into());
    }
    if frame["workspaceId"] != workspace || frame["cursor"]["workspaceId"] != workspace {
        return Err("Foreign entity transfer.".into());
    }
    let resource = &frame["resource"];
    let key = resource_key(resource)?;
    let selection = Selection::new(connection, workspace, resource)?;
    let page = number(frame, "pageIndex")?;
    let part = number(frame, "partIndex")?;
    let count = number(frame, "partCount")?;
    if part >= count || (frame["complete"] == true && part + 1 != count) {
        return Err("Invalid entity transfer part boundary.".into());
    }
    let mut metadata = frame.clone();
    for field in ["pageIndex", "partIndex", "partCount", "complete", "data"] {
        metadata.as_object_mut().unwrap().remove(field);
    }
    let saved:Option<(String,u64,u64,u64,Option<u64>,Option<u64>,Option<String>,Option<String>)>=connection.query_row(
        "SELECT metadata,page_index,part_index,part_count,interval_after,through_seq,result,final_frame FROM client_subscription_transfers WHERE workspace=?1 AND resource_key=?2",
        params![workspace,key],|row|Ok((row.get(0)?,crate::store::unsigned(row,1)?,crate::store::unsigned(row,2)?,crate::store::unsigned(row,3)?,optional_unsigned(row,4)?,optional_unsigned(row,5)?,row.get(6)?,row.get(7)?))).optional().map_err(|error|error.to_string())?;
    let mut state = match saved {
        Some((saved, page, part, count, after, through, result, final_frame))
            if saved == metadata.to_string() =>
        {
            if let Some(result) = result {
                if final_frame.as_deref() != Some(frame.to_string().as_str()) {
                    return Err("Completed entity transfer identity changed.".into());
                }
                let result: Value =
                    serde_json::from_str(&result).map_err(|error| error.to_string())?;
                let current = read(connection, workspace, &key, resource)?;
                if cursor_seq(&current)
                    .zip(cursor_seq(&result))
                    .is_some_and(|(current, replayed)| current > replayed)
                {
                    return recovery(connection, workspace, &key, &current);
                }
                return Ok(result);
            }
            (page, part, count, after, through)
        }
        Some((saved, ..))
            if serde_json::from_str::<Value>(&saved).map_err(|error| error.to_string())?["transferId"]
                == frame["transferId"] =>
        {
            return Err("Entity transfer identity changed.".into());
        }
        _ => {
            if page != 0 || part != 0 {
                return Err("Entity transfer has no initial page.".into());
            }
            let active:u64=connection.query_row("SELECT count(*) FROM client_subscription_transfers WHERE workspace=?1 AND result IS NULL AND resource_key!=?2",params![workspace,key],|row|crate::store::unsigned(row,0)).map_err(|error|error.to_string())?;
            if active >= shared_values::MAX_SYNC_RESOURCE_COUNT as u64 {
                return Err("Too many incomplete entity transfers.".into());
            }
            for table in [
                "client_subscription_fragments",
                "client_subscription_stage_rows",
                "client_subscription_transfers",
            ] {
                connection
                    .execute(
                        &format!("DELETE FROM {table} WHERE workspace=?1 AND resource_key=?2"),
                        params![workspace, key],
                    )
                    .map_err(|error| error.to_string())?;
            }
            connection.execute("INSERT INTO client_subscription_transfers(workspace,resource_key,metadata,page_index,part_index,part_count) VALUES(?1,?2,?3,0,0,0)",params![workspace,key,metadata.to_string()]).map_err(|error|error.to_string())?;
            (0, 0, 0, None, None)
        }
    };
    if let Some(saved)=connection.query_row("SELECT frame FROM client_subscription_fragments WHERE workspace=?1 AND resource_key=?2 AND page_index=?3 AND part_index=?4",params![workspace,key,page as i64,part as i64],|row|row.get::<_,String>(0)).optional().map_err(|error|error.to_string())? {
        if saved!=frame.to_string() {return Err("Conflicting entity transfer replay.".into());}
        return Ok(Value::Null);
    }
    if page != state.0 || part != state.1 || (part != 0 && count != state.2) {
        return Err("Entity transfer part is out of order.".into());
    }
    if part == 0 {
        state.2 = count;
    }
    connection
        .execute(
            "INSERT INTO client_subscription_fragments VALUES(?1,?2,?3,?4,?5)",
            params![workspace, key, page as i64, part as i64, frame.to_string()],
        )
        .map_err(|error| error.to_string())?;
    state.1 += 1;
    if state.1 == count {
        let reader = FragmentReader {
            connection,
            workspace,
            key: &key,
            page,
            count,
            next: 0,
            chunk: std::io::Cursor::new(Vec::new()),
        };
        let batch: Value = serde_json::from_reader(reader).map_err(|error| error.to_string())?;
        validate("EntityChangeBatch", &batch)?;
        let after = number(&batch, "after")?;
        let through = number(&batch, "through")?;
        let final_seq = number(&frame["cursor"], "seq")?;
        if through < after
            || through > final_seq
            || state.4.is_some_and(|previous| previous != after)
            || (state.3.is_none() && frame["replyKind"] == "snapshot" && after != 0)
        {
            return Err("Invalid entity transfer page interval.".into());
        }
        state.3.get_or_insert(after);
        let mut sequence = after;
        for row in batch["documents"].as_array().unwrap() {
            let seq = number(row, "seq")?;
            if seq <= sequence || seq > through || !selection.belongs(row) {
                return Err("Entity is outside its transfer interval or scope.".into());
            }
            sequence = seq;
            let entity_key = projections::entity_scope(row)?;
            connection
                .execute(
                    "INSERT INTO client_subscription_stage_rows VALUES(?1,?2,?3,?4,?5)",
                    params![workspace, key, entity_key, seq as i64, row.to_string()],
                )
                .map_err(|error| error.to_string())?;
        }
        state.4 = Some(through);
        state.0 += 1;
        state.1 = 0;
    }
    connection.execute("UPDATE client_subscription_transfers SET page_index=?3,part_index=?4,part_count=?5,interval_after=?6,through_seq=?7 WHERE workspace=?1 AND resource_key=?2",params![workspace,key,state.0 as i64,state.1 as i64,state.2 as i64,state.3.map(|value|value as i64),state.4.map(|value|value as i64)]).map_err(|error|error.to_string())?;
    if frame["complete"] != true {
        return Ok(Value::Null);
    }
    let through = number(&frame["cursor"], "seq")?;
    if state.4 != Some(through) {
        return Err("Entity transfer interval is incomplete.".into());
    }
    let previous = read(connection, workspace, &key, resource)?;
    let snapshot = frame["replyKind"] == "snapshot";
    let result = if cursor_seq(&previous).is_some_and(|old| old > through) {
        recovery(connection, workspace, &key, &previous)?
    } else if !snapshot && cursor_seq(&previous).is_none_or(|old| old < state.3.unwrap()) {
        recovery(connection, workspace, &key, &previous)?
    } else {
        let mut statement=connection.prepare("SELECT document FROM client_subscription_stage_rows WHERE workspace=?1 AND resource_key=?2 ORDER BY seq").map_err(|error|error.to_string())?;
        let rows = statement
            .query_map(params![workspace, key], |row| row.get::<_, String>(0))
            .map_err(|error| error.to_string())?;
        for row in rows {
            let row: Value = serde_json::from_str(&row.map_err(|error| error.to_string())?)
                .map_err(|error| error.to_string())?;
            write_entity(connection, workspace, &selection, &row)?;
        }
        if snapshot {
            prune(connection, workspace, &selection, through, |entity_key| {
                connection.query_row("SELECT EXISTS(SELECT 1 FROM client_subscription_stage_rows WHERE workspace=?1 AND resource_key=?2 AND entity_key=?3)",params![workspace,key,entity_key],|row|row.get(0)).map_err(|error|error.to_string())
            })?;
        }
        projections::refresh_entity_state(connection, workspace)?;
        let entry = json!({"resource":resource,"revision":through,"cursor":frame["cursor"],"replyKind":frame["replyKind"]});
        let result = committed(
            resource,
            frame["epoch"].as_str().unwrap(),
            &entry,
            entity_data(connection, workspace, &selection)?,
        );
        save(connection, workspace, &key, &result)?;
        result
    };
    connection.execute("UPDATE client_subscription_transfers SET result=?3,final_frame=?4 WHERE workspace=?1 AND resource_key=?2",params![workspace,key,result.to_string(),frame.to_string()]).map_err(|error|error.to_string())?;
    for table in [
        "client_subscription_fragments",
        "client_subscription_stage_rows",
    ] {
        connection
            .execute(
                &format!("DELETE FROM {table} WHERE workspace=?1 AND resource_key=?2"),
                params![workspace, key],
            )
            .map_err(|error| error.to_string())?;
    }
    Ok(result)
}

fn optional_unsigned(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    row.get::<_, Option<i64>>(index)?
        .map(|_| crate::store::unsigned(row, index))
        .transpose()
}

#[cfg(test)]
mod tests {
    use crate::{protocol::Request, store::Store};
    use serde_json::{Value, json};
    use std::path::Path;

    fn command(store: &mut Store, method: &str, params: Value) -> crate::protocol::Response {
        store.dispatch(Request {
            id: "subscription-fixture".into(),
            method: method.into(),
            params,
        })
    }
    fn workspace() -> String {
        "a".repeat(32)
    }
    fn apply(store: &mut Store, resource: Value, epoch: &str, entry: Value) -> Value {
        let response = command(
            store,
            "projection.applySubscription",
            json!({"workspace":workspace(),"resource":resource,"epoch":epoch,"entry":entry}),
        );
        assert!(response.error.is_none(), "{:?}", response.error);
        response.result.unwrap()
    }
    fn read(store: &mut Store, resource: Value) -> Value {
        let response = command(
            store,
            "projection.readSubscription",
            json!({"workspace":workspace(),"resource":resource}),
        );
        assert!(response.error.is_none(), "{:?}", response.error);
        response.result.unwrap()
    }
    fn cached(mut result: Value) -> Value {
        result["status"] = json!("recovery_required");
        result
    }
    fn task(id: &str, seq: u64, name: &str) -> Value {
        json!({"op":"upsert","collection":"task","id":id,"seq":seq,"value":{"id":id,"name":name}})
    }
    fn entities(
        resource: Value,
        after: u64,
        through: u64,
        snapshot: bool,
        documents: Value,
    ) -> Value {
        json!({"resource":resource,"revision":through,"cursor":{"workspaceId":workspace(),"seq":through},"replyKind":if snapshot {"snapshot"} else {"delta"},"entityChanges":{"after":after,"through":through,"documents":documents}})
    }

    #[test]
    fn subscription_commit_keeps_rows_cursor_and_view_atomic_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let resource = json!({"kind":"tasks"});
        let mut store = Store::open(&path).unwrap();
        let original = apply(
            &mut store,
            resource.clone(),
            "one",
            entities(resource.clone(), 0, 5, true, json!([task("A", 5, "old")])),
        );
        assert_eq!(original["data"], json!([{"id":"A","name":"old"}]));
        assert_eq!(original["cursor"]["seq"], 5);
        store.connection.execute_batch("CREATE TEMP TRIGGER fail_subscription BEFORE UPDATE ON client_subscription_views BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
        let failed = command(
            &mut store,
            "projection.applySubscription",
            json!({"workspace":workspace(),"resource":resource,"epoch":"one","entry":entities(resource.clone(),5,6,false,json!([task("A",6,"new")]))}),
        );
        assert!(failed.error.is_some());
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(read(&mut store, resource.clone()), cached(original));
        let rows = command(
            &mut store,
            "projection.entities",
            json!({"workspace":workspace(),"scope":"entity:*"}),
        )
        .result
        .unwrap();
        assert_eq!(rows[0]["value"]["name"], "old");
        let updated = apply(
            &mut store,
            resource.clone(),
            "two",
            entities(resource, 5, 6, false, json!([task("A", 6, "new")])),
        );
        assert_eq!(updated["cursor"]["seq"], 6);
        assert_eq!(updated["data"][0]["name"], "new");
    }

    #[test]
    fn subscription_scoped_snapshot_never_erases_another_resource_or_newer_row() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let a = json!({"kind":"task","taskId":"A"});
        let b = json!({"kind":"task","taskId":"B"});
        apply(
            &mut store,
            a.clone(),
            "one",
            entities(a.clone(), 0, 8, true, json!([task("A", 8, "A")])),
        );
        apply(
            &mut store,
            b.clone(),
            "one",
            entities(b.clone(), 0, 9, true, json!([task("B", 9, "B")])),
        );
        let cleared = apply(
            &mut store,
            a.clone(),
            "one",
            entities(a.clone(), 0, 10, true, json!([])),
        );
        assert!(cleared["data"].is_null());
        assert_eq!(read(&mut store, b)["data"]["name"], "B");
        let stale = apply(
            &mut store,
            a.clone(),
            "one",
            entities(a.clone(), 0, 7, true, json!([task("A", 7, "stale")])),
        );
        assert_eq!(stale["cursor"]["seq"], 10);
        assert!(stale["data"].is_null());
        assert_eq!(
            stale["status"], "recovery_required",
            "an ignored old snapshot cannot confirm a new stream"
        );
        let hint = apply(&mut store, a, "new-epoch", json!({"revision":100}));
        assert_eq!(hint["status"], "recovery_required");
        assert_eq!(hint["cursor"]["seq"], 10);
        assert_eq!(
            hint["version"]["epoch"], "one",
            "a cursorless hint is not an applied epoch"
        );
    }

    #[test]
    fn subscription_gaps_foreign_cursors_and_hints_never_create_a_baseline() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let resource = json!({"kind":"tasks"});
        let gap = apply(
            &mut store,
            resource.clone(),
            "one",
            entities(resource.clone(), 5, 6, false, json!([task("A", 6, "gap")])),
        );
        assert_eq!(gap["status"], "recovery_required");
        assert!(gap.get("cursor").is_none());
        let hint = apply(&mut store, resource.clone(), "one", json!({"revision":100}));
        assert_eq!(hint["status"], "recovery_required");
        assert!(hint.get("cursor").is_none());
        let original = apply(
            &mut store,
            resource.clone(),
            "one",
            entities(resource.clone(), 0, 5, true, json!([task("A", 5, "safe")])),
        );
        let mut foreign = entities(
            resource.clone(),
            5,
            6,
            false,
            json!([task("A", 6, "foreign")]),
        );
        foreign["cursor"]["workspaceId"] = json!("b".repeat(32));
        let rejected = command(
            &mut store,
            "projection.applySubscription",
            json!({"workspace":workspace(),"resource":resource,"epoch":"one","entry":foreign}),
        );
        assert!(rejected.error.is_some());
        assert_eq!(read(&mut store, resource.clone()), cached(original.clone()));
        let mut malformed = entities(resource.clone(), 5, 6, false, json!([task("A", 6, "bad")]));
        malformed["entityChanges"]["documents"][0]["value"]["name"] = json!(7);
        assert!(
            command(
                &mut store,
                "projection.applySubscription",
                json!({"workspace":workspace(),"resource":resource,"epoch":"one","entry":malformed})
            )
            .error
            .is_some()
        );
        assert_eq!(read(&mut store, resource), cached(original));
    }

    #[test]
    fn subscription_transcript_delta_needs_exact_item_base_and_order() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let resource = json!({"kind":"transcript","agentId":"A"});
        let first = json!({"id":"m1","role":"user","text":"first"});
        let second = json!({"id":"m2","role":"assistant","text":"second"});
        apply(
            &mut store,
            resource.clone(),
            "one",
            json!({"resource":resource,"revision":5,"cursor":{"workspaceId":workspace(),"seq":5},"replyKind":"snapshot","transcriptSnapshot":{"items":[first],"truncated":false,"nextCursor":"older-boundary"}}),
        );
        let entry = json!({"resource":resource,"revision":6,"cursor":{"workspaceId":workspace(),"seq":6},"replyKind":"delta","transcriptChanges":{"after":5,"through":6,"delta":{"delta":true,"items":[second],"removed":[],"order":["m1","m2"],"truncated":false}}});
        let result = apply(&mut store, resource.clone(), "two", entry.clone());
        assert_eq!(result["data"]["items"], json!([first, second]));
        assert_eq!(result["data"]["nextCursor"], "older-boundary");
        let mut gap = entry;
        gap["revision"] = json!(8);
        gap["cursor"]["seq"] = json!(8);
        gap["transcriptChanges"]["after"] = json!(7);
        gap["transcriptChanges"]["through"] = json!(8);
        let rejected = apply(&mut store, resource.clone(), "two", gap);
        assert_eq!(rejected["status"], "recovery_required");
        assert_eq!(rejected["cursor"]["seq"], 6);
        assert_eq!(read(&mut store, resource)["data"], result["data"]);
    }

    #[test]
    fn subscription_terminal_output_uses_unicode_scalar_intervals() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let resource = json!({"kind":"terminal-output","terminalId":"A"});
        let frame = |text: &str, offset, kind| json!({"resource":resource,"revision":offset,"cursor":{"workspaceId":workspace(),"seq":offset},"replyKind":kind,"viewSnapshot":{"terminalOutput":{"text":text,"offset":offset,"status":"running","truncated":false}}});
        apply(
            &mut store,
            resource.clone(),
            "one",
            frame("a🙂", 2, "snapshot"),
        );
        let result = apply(&mut store, resource.clone(), "one", frame("中", 3, "delta"));
        assert_eq!(result["data"]["text"], "a🙂中");
        let gap = apply(&mut store, resource.clone(), "one", frame("z", 5, "delta"));
        assert_eq!(gap["status"], "recovery_required");
        assert_eq!(gap["cursor"]["seq"], 3);
        assert_eq!(read(&mut store, resource)["data"]["text"], "a🙂中");
    }

    #[test]
    fn subscription_entity_fragments_stay_invisible_until_complete_and_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let resource = json!({"kind":"tasks"});
        let page = json!({"after":0,"through":5,"documents":[task("A",5,"🙂")]});
        let encoded = page.to_string();
        let split = encoded.find("🙂").unwrap();
        let frame = |part, data, complete| json!({"protocol":4,"workspaceId":workspace(),"epoch":"one","revision":8,"reason":"initial","resource":resource,"cursor":{"workspaceId":workspace(),"seq":5},"replyKind":"snapshot","transferId":"transfer","pageIndex":0,"partIndex":part,"partCount":2,"complete":complete,"data":data});
        let first = frame(0, &encoded[..split], false);
        let last = frame(1, &encoded[split..], true);
        let mut store = Store::open(&path).unwrap();
        let staged = command(
            &mut store,
            "projection.stageSubscriptionFragment",
            json!({"workspace":workspace(),"frame":first.clone()}),
        );
        assert!(staged.error.is_none(), "{:?}", staged.error);
        assert_eq!(staged.result, Some(Value::Null));
        assert!(read(&mut store, resource.clone()).get("cursor").is_none());
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            command(
                &mut store,
                "projection.stageSubscriptionFragment",
                json!({"workspace":workspace(),"frame":first})
            )
            .result,
            Some(Value::Null)
        );
        let committed = command(
            &mut store,
            "projection.stageSubscriptionFragment",
            json!({"workspace":workspace(),"frame":last.clone()}),
        );
        assert!(committed.error.is_none(), "{:?}", committed.error);
        let result = committed.result.unwrap();
        assert_eq!(result["cursor"]["seq"], 5);
        assert_eq!(result["data"][0]["name"], "🙂");
        assert_eq!(
            command(
                &mut store,
                "projection.stageSubscriptionFragment",
                json!({"workspace":workspace(),"frame":last})
            )
            .result,
            Some(result.clone())
        );
        let resource = json!({"kind":"tasks"});
        apply(
            &mut store,
            resource.clone(),
            "one",
            entities(resource, 0, 6, true, json!([])),
        );
        let late = command(
            &mut store,
            "projection.stageSubscriptionFragment",
            json!({"workspace":workspace(),"frame":last}),
        )
        .result
        .unwrap();
        assert_eq!(
            late["cursor"]["seq"], 6,
            "an exact old transfer replay cannot return an older view"
        );
        assert_eq!(late["status"], "recovery_required");
        assert_eq!(late["data"], json!([]));
    }

    #[test]
    fn subscription_snapshot_absence_fences_old_overlapping_resources() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let single = json!({"kind":"task","taskId":"A"});
        let list = json!({"kind":"tasks"});
        apply(
            &mut store,
            single.clone(),
            "one",
            entities(single.clone(), 0, 8, true, json!([task("A", 8, "old")])),
        );
        apply(
            &mut store,
            single.clone(),
            "one",
            entities(single, 0, 10, true, json!([])),
        );
        let old = apply(
            &mut store,
            list.clone(),
            "one",
            entities(list, 0, 9, true, json!([task("A", 9, "late")])),
        );
        assert_eq!(
            old["data"],
            json!([]),
            "an older resource cannot restore a row absent at sequence10"
        );
    }

    #[test]
    fn subscription_transfer_identity_cannot_change_its_interval() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let frame = json!({"protocol":4,"workspaceId":workspace(),"epoch":"one","revision":8,"reason":"initial","resource":{"kind":"tasks"},"cursor":{"workspaceId":workspace(),"seq":5},"replyKind":"snapshot","transferId":"same","pageIndex":0,"partIndex":0,"partCount":2,"complete":false,"data":"{"});
        assert!(
            command(
                &mut store,
                "projection.stageSubscriptionFragment",
                json!({"workspace":workspace(),"frame":frame.clone()})
            )
            .error
            .is_none()
        );
        let mut changed = frame;
        changed["cursor"]["seq"] = json!(6);
        assert!(
            command(
                &mut store,
                "projection.stageSubscriptionFragment",
                json!({"workspace":workspace(),"frame":changed})
            )
            .error
            .is_some()
        );
    }

    #[test]
    fn subscription_state_snapshot_decodes_committed_state_in_rust() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let empty = command(
            &mut store,
            "projection.stateSnapshot",
            json!({"workspace":workspace()}),
        );
        assert!(empty.error.is_none(), "{:?}", empty.error);
        assert_eq!(empty.result, Some(Value::Null));
        let resource = json!({"kind":"state"});
        apply(
            &mut store,
            resource.clone(),
            "one",
            entities(
                resource,
                0,
                1,
                true,
                json!([{"op":"upsert","collection":"workspace","id":"current","seq":1,"value":{"stateDir":"test-state"}}]),
            ),
        );
        let state = command(
            &mut store,
            "projection.stateSnapshot",
            json!({"workspace":workspace()}),
        );
        assert!(state.error.is_none(), "{:?}", state.error);
        let state = state.result.unwrap();
        assert_eq!(state["stateDir"], "test-state");
        assert!(state.get("token").is_none());
    }

    #[test]
    fn subscription_payload_fields_cannot_cross_resource_owners() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let resource = json!({"kind":"tasks"});
        let mut entry = entities(resource.clone(), 0, 5, true, json!([task("A", 5, "safe")]));
        entry["viewSnapshot"] = json!({"terminalOutput":{"text":"unrelated private output","offset":24,"status":"running","truncated":false}});
        let rejected = command(
            &mut store,
            "projection.applySubscription",
            json!({"workspace":workspace(),"resource":resource,"epoch":"one","entry":entry}),
        );
        assert!(rejected.error.is_some());
        assert!(read(&mut store, resource).get("cursor").is_none());
    }

    #[test]
    fn subscription_draft_owner_and_cursor_commit_or_roll_back_together() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let resource = json!({"kind":"drafts"});
        let payload =
            json!({"id":"phone:chat","device":"phone","session":"chat","text":"saved","updated":7})
                .to_string();
        let entry = json!({"resource":resource,"revision":5,"cursor":{"workspaceId":workspace(),"seq":5},"replyKind":"snapshot","draftChanges":{"after":0,"through":5,"documents":[{"id":"phone:chat","seq":5,"payload":payload,"_deleted":false}]}});
        let result = apply(&mut store, resource.clone(), "one", entry.clone());
        assert_eq!(result["status"], "ready");
        assert_eq!(result["cursor"]["seq"], 5);
        let view = command(&mut store, "drafts.view", json!({"workspace":workspace()}))
            .result
            .unwrap();
        assert_eq!(view["drafts"]["chat"], "saved");
        store.connection.execute_batch("CREATE TEMP TRIGGER fail_draft_subscription BEFORE UPDATE ON client_subscription_views BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
        let mut next = entry;
        next["revision"] = json!(6);
        next["cursor"]["seq"] = json!(6);
        next["replyKind"] = json!("delta");
        next["draftChanges"]["after"] = json!(5);
        next["draftChanges"]["through"] = json!(6);
        next["draftChanges"]["documents"][0]["seq"] = json!(6);
        let mut changed: Value = serde_json::from_str(&payload).unwrap();
        changed["text"] = json!("must roll back");
        changed["updated"] = json!(8);
        next["draftChanges"]["documents"][0]["payload"] = json!(changed.to_string());
        assert!(
            command(
                &mut store,
                "projection.applySubscription",
                json!({"workspace":workspace(),"resource":resource,"epoch":"one","entry":next})
            )
            .error
            .is_some()
        );
        assert_eq!(read(&mut store, resource)["cursor"]["seq"], 5);
        assert_eq!(
            command(&mut store, "drafts.view", json!({"workspace":workspace()}))
                .result
                .unwrap()["drafts"]["chat"],
            "saved"
        );
    }

    #[test]
    fn subscription_multi_page_completion_rolls_back_every_visible_write() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let resource = json!({"kind":"tasks"});
        let original = apply(
            &mut store,
            resource.clone(),
            "one",
            entities(
                resource.clone(),
                0,
                1,
                true,
                json!([task("old", 1, "original")]),
            ),
        );
        let frame = |page, complete, batch: Value| json!({"protocol":4,"workspaceId":workspace(),"epoch":"one","revision":9,"reason":"change","resource":resource,"cursor":{"workspaceId":workspace(),"seq":4},"replyKind":"snapshot","transferId":"pages","pageIndex":page,"partIndex":0,"partCount":1,"complete":complete,"data":batch.to_string()});
        let first = frame(
            0,
            false,
            json!({"after":0,"through":2,"documents":[task("A",2,"first")]}),
        );
        let last = frame(
            1,
            true,
            json!({"after":2,"through":4,"documents":[task("B",4,"last")]}),
        );
        assert_eq!(
            command(
                &mut store,
                "projection.stageSubscriptionFragment",
                json!({"workspace":workspace(),"frame":first})
            )
            .result,
            Some(Value::Null)
        );
        assert_eq!(read(&mut store, resource.clone()), cached(original.clone()));
        store.connection.execute_batch("CREATE TEMP TRIGGER fail_transfer_commit BEFORE UPDATE ON client_subscription_views BEGIN SELECT RAISE(ABORT,'fixture storage failure'); END;").unwrap();
        assert!(
            command(
                &mut store,
                "projection.stageSubscriptionFragment",
                json!({"workspace":workspace(),"frame":last.clone()})
            )
            .error
            .is_some()
        );
        assert_eq!(read(&mut store, resource.clone()), cached(original));
        let state = command(
            &mut store,
            "projection.stateSnapshot",
            json!({"workspace":workspace()}),
        )
        .result
        .unwrap();
        assert_eq!(
            state["runtime"]["tasks"],
            json!([{"id":"old","name":"original"}])
        );
        store
            .connection
            .execute_batch("DROP TRIGGER fail_transfer_commit")
            .unwrap();
        let completed = command(
            &mut store,
            "projection.stageSubscriptionFragment",
            json!({"workspace":workspace(),"frame":last}),
        );
        assert!(completed.error.is_none(), "{:?}", completed.error);
        let result = completed.result.unwrap();
        assert_eq!(result["cursor"]["seq"], 4);
        assert_eq!(
            result["data"],
            json!([{"id":"A","name":"first"},{"id":"B","name":"last"}])
        );
        let fragments: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM client_subscription_fragments",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let rows: i64 = store
            .connection
            .query_row(
                "SELECT count(*) FROM client_subscription_stage_rows",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((fragments, rows), (0, 0));
    }

    #[test]
    fn subscription_room_snapshot_keeps_original_history_boundaries() {
        let mut store = Store::open(Path::new(":memory:")).unwrap();
        let resource = json!({"kind":"room","roomId":"chat"});
        let room = json!({"room":{"id":"chat"},"messages":[{"seq":8,"id":"m","room":"chat","sender":"user","senderName":"User","text":"saved","created":1,"deliveries":{}}],"nextBefore":8,"nextAfter":null});
        let result = apply(
            &mut store,
            resource.clone(),
            "one",
            json!({"resource":resource,"revision":90,"cursor":{"workspaceId":workspace(),"seq":8},"replyKind":"snapshot","viewSnapshot":{"room":room}}),
        );
        assert_eq!(result["data"], room);
        assert_eq!(read(&mut store, resource)["cursor"]["seq"], 8);
    }
}
