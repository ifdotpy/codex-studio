//! Local federation settings and projections over Python-written records.

use crate::schema::{SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::{Map, Value, json};

const TABLES: &[TableRequirement] = &[
    TableRequirement {
        name: "runtime_federation_settings",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_federation_identity",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_federation_peers",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_federation_invites",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_federation_rooms",
        columns: &["id", "record"],
    },
    TableRequirement {
        name: "runtime_federation_outbox",
        columns: &["id", "peer_id", "record"],
    },
];
pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: &[],
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: TABLES,
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct FederationRepository;

impl FederationRepository {
    pub fn snapshot(connection: &Connection) -> StoreResult<Value> {
        let settings = record(connection, "runtime_federation_settings", "global")?
            .unwrap_or_else(|| json!({"enabled":false}));
        let enabled = settings
            .get("enabled")
            .and_then(Value::as_bool)
            .ok_or_else(|| invalid("runtime_federation_settings", "global", "enabled"))?;
        let peers = records(connection, "runtime_federation_peers", "ORDER BY id")?
            .iter()
            .map(|(id, v)| project(id, v, "runtime_federation_peers", PEER_FIELDS))
            .collect::<StoreResult<Vec<_>>>()?;
        let invites = records(connection, "runtime_federation_invites", "")?
            .iter()
            .map(|(id, v)| project(id, v, "runtime_federation_invites", INVITE_FIELDS))
            .collect::<StoreResult<Vec<_>>>()?;
        let rooms = records(connection, "runtime_federation_rooms", "ORDER BY id")?
            .iter()
            .map(|(id, v)| project(id, v, "runtime_federation_rooms", ROOM_FIELDS))
            .collect::<StoreResult<Vec<_>>>()?;
        // Validate every stored outbox record, including records excluded from the queue count.
        let outbox = records(connection, "runtime_federation_outbox", "")?;
        let mut queued = 0;
        for (id, value) in outbox {
            if value.get("id").and_then(Value::as_str) != Some(id.as_str()) {
                return Err(invalid("runtime_federation_outbox", &id, "id"));
            }
            for field in ["kind", "room"] {
                if !value.get(field).is_some_and(Value::is_string) {
                    return Err(invalid("runtime_federation_outbox", &id, field));
                }
            }
            if !value.get("payload").is_some_and(Value::is_object) {
                return Err(invalid("runtime_federation_outbox", &id, "payload"));
            }
            for field in ["created", "nextAt"] {
                if value
                    .get(field)
                    .and_then(Value::as_f64)
                    .is_none_or(|n| !n.is_finite())
                {
                    return Err(invalid("runtime_federation_outbox", &id, field));
                }
            }
            if value.get("attempts").and_then(Value::as_u64).is_none() {
                return Err(invalid("runtime_federation_outbox", &id, "attempts"));
            }
            if value
                .get("error")
                .is_some_and(|v| !v.is_null() && !v.is_string())
            {
                return Err(invalid("runtime_federation_outbox", &id, "error"));
            }
            let status = value
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("runtime_federation_outbox", &id, "status"))?;
            if !["queued", "delivered"].contains(&status) {
                return Err(invalid("runtime_federation_outbox", &id, "status"));
            }
            if status == "queued" {
                queued += 1;
            }
        }
        // Select public identity fields directly; never materialize `record` / privateKey.
        let identity = connection.query_row(
            "SELECT json_extract(record,'$.stateId'),json_extract(record,'$.label'),json_extract(record,'$.publicKey') FROM runtime_federation_identity WHERE id='local'",
            [], |row| Ok((row.get::<_, Option<String>>(0)?,row.get::<_, Option<String>>(1)?,row.get::<_, Option<String>>(2)?)),
        ).optional()?.map(|(state,label,key)| {
            let state = state.ok_or_else(|| invalid("runtime_federation_identity", "local", "stateId"))?;
            let label = label.ok_or_else(|| invalid("runtime_federation_identity", "local", "label"))?;
            let key = key.ok_or_else(|| invalid("runtime_federation_identity", "local", "publicKey"))?;
            Ok::<Value, StoreError>(json!({"stateId":state,"label":label,"fingerprint":crate::sha256_hex(key.as_bytes())[..32].to_owned()}))
        }).transpose()?;
        Ok(
            json!({"version":1,"enabled":enabled,"identity":identity,"peers":peers,"invites":invites,"rooms":rooms,"queued":queued}),
        )
    }

    pub fn set_enabled(tx: &Transaction<'_>, enabled: bool) -> StoreResult<()> {
        let updated = now()?;
        tx.execute("INSERT INTO runtime_federation_settings(id,record) VALUES('global',?1) ON CONFLICT(id) DO UPDATE SET record=excluded.record", [json!({"enabled":enabled,"updated":updated}).to_string()])?;
        Ok(())
    }

    pub fn approve_peer(tx: &Transaction<'_>, id: &str, accept_missing: bool) -> StoreResult<()> {
        let mut peer = record(tx, "runtime_federation_peers", id)?
            .ok_or_else(|| StoreError::SchemaMismatch(format!("unknown federation peer {id}")))?;
        if peer.get("status").and_then(Value::as_str) == Some("revoked") {
            return Err(StoreError::SchemaMismatch(format!(
                "unknown pending federation peer {id}"
            )));
        }
        if peer.get("whoisStatus").and_then(Value::as_str) == Some("missing") && !accept_missing {
            return Err(StoreError::SchemaMismatch(format!(
                "Tailscale whois result is missing for federation peer {id}"
            )));
        }
        let obj = peer
            .as_object_mut()
            .ok_or_else(|| invalid("runtime_federation_peers", id, "record"))?;
        obj.insert("localApproved".into(), json!(true));
        let remote = obj
            .get("remoteApproved")
            .and_then(Value::as_bool)
            .ok_or_else(|| invalid("runtime_federation_peers", id, "remoteApproved"))?;
        obj.insert(
            "status".into(),
            json!(if remote { "approved" } else { "pending" }),
        );
        obj.insert("updated".into(), json!(now()?));
        tx.execute(
            "UPDATE runtime_federation_peers SET record=?1 WHERE id=?2",
            params![peer.to_string(), id],
        )?;
        Ok(())
    }

    pub fn revoke_peer(tx: &Transaction<'_>, id: &str) -> StoreResult<()> {
        let mut peer = record(tx, "runtime_federation_peers", id)?
            .ok_or_else(|| StoreError::SchemaMismatch(format!("unknown federation peer {id}")))?;
        let obj = peer
            .as_object_mut()
            .ok_or_else(|| invalid("runtime_federation_peers", id, "record"))?;
        obj.insert("status".into(), json!("revoked"));
        obj.insert("localApproved".into(), json!(false));
        obj.insert("remoteApproved".into(), json!(false));
        obj.insert("revoked".into(), json!(now()?));
        obj.remove("inviteId");
        obj.remove("inviteToken");
        tx.execute(
            "UPDATE runtime_federation_peers SET record=?1 WHERE id=?2",
            params![peer.to_string(), id],
        )?;
        let mut stmt =
            tx.prepare("SELECT id,record FROM runtime_federation_outbox WHERE peer_id=?1")?;
        let items = stmt
            .query_map([id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (out_id, raw) in items {
            let v = parse_record("runtime_federation_outbox", &out_id, &raw)?;
            if v.get("status").and_then(Value::as_str) == Some("queued") {
                tx.execute(
                    "DELETE FROM runtime_federation_outbox WHERE id=?1",
                    [&out_id],
                )?;
                tx.execute("UPDATE runtime_chat_messages SET deliveries=json_set(deliveries,?1,'cancelled') WHERE id=?2",params![format!("$.remote:{id}"),out_id])?;
            }
        }
        let mut stmt=tx.prepare("SELECT id,record FROM runtime_federation_rooms WHERE json_extract(record,'$.peerId')=?1")?;
        let rooms = stmt
            .query_map([id], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(stmt);
        for (room_id, raw) in rooms {
            let mut room = parse_record("runtime_federation_rooms", &room_id, &raw)?;
            room["status"] = json!("revoked");
            tx.execute(
                "UPDATE runtime_federation_rooms SET record=?1 WHERE id=?2",
                params![room.to_string(), room_id],
            )?;
            tx.execute(
                "UPDATE runtime_rooms SET record=json_set(record,'$.status','revoked') WHERE id=?1",
                [&room_id],
            )?;
        }
        Ok(())
    }

    pub fn room_options(tx: &Transaction<'_>, id: &str, disclosure: &Value) -> StoreResult<()> {
        let mut room = record(tx, "runtime_federation_rooms", id)?
            .ok_or_else(|| StoreError::SchemaMismatch(format!("unknown federation room {id}")))?;
        if !matches!(
            room.get("status").and_then(Value::as_str),
            Some("pending" | "approved")
        ) {
            return Err(StoreError::SchemaMismatch(format!(
                "federation room {id} is unavailable"
            )));
        }
        let values = disclosure.as_object().ok_or_else(|| {
            StoreError::SchemaMismatch("invalid federation room disclosure settings".into())
        })?;
        if values
            .keys()
            .any(|k| !["shareNames", "shareStatus"].contains(&k.as_str()))
            || values.values().any(|v| !v.is_boolean())
        {
            return Err(StoreError::SchemaMismatch(
                "invalid federation room disclosure settings".into(),
            ));
        }
        for key in ["shareNames", "shareStatus"] {
            if let Some(value) = values.get(key) {
                room[key] = value.clone();
            }
        }
        room["updated"] = json!(now()?);
        tx.execute(
            "UPDATE runtime_federation_rooms SET record=?1 WHERE id=?2",
            params![room.to_string(), id],
        )?;
        Ok(())
    }

    pub fn room_action(tx: &Transaction<'_>, body: &Value, approve: bool) -> StoreResult<()> {
        let room_id = body
            .get("room_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let peer_id = body
            .get("peer_id")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let member_values = body
            .get("local_members")
            .and_then(Value::as_array)
            .ok_or_else(|| {
                StoreError::SchemaMismatch("invalid local federation room members".into())
            })?;
        if member_values.is_empty()
            || member_values.len() > 50
            || member_values.iter().any(|v| !v.is_string())
        {
            return Err(StoreError::SchemaMismatch(
                "invalid local federation room members".into(),
            ));
        }
        let members = member_values
            .iter()
            .map(|v| v.as_str().unwrap_or_default().to_owned())
            .collect::<Vec<_>>();
        let mut unique = members.clone();
        unique.sort();
        unique.dedup();
        if unique.len() != members.len() {
            return Err(StoreError::SchemaMismatch(
                "federation room members must be unique".into(),
            ));
        }
        let share_names = body
            .get("share_names")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        let share_status = body
            .get("share_status")
            .and_then(Value::as_bool)
            .unwrap_or(false);
        if !tx.query_row("SELECT json_extract(record,'$.enabled') FROM runtime_federation_settings WHERE id='global'",[],|r|r.get::<_,bool>(0)).optional()?.unwrap_or(false){return Err(StoreError::SchemaMismatch("federation must be enabled before room changes".into()));}
        let (id,label,public_key)=tx.query_row("SELECT json_extract(record,'$.stateId'),json_extract(record,'$.label'),json_extract(record,'$.publicKey') FROM runtime_federation_identity WHERE id='local'",[],|r|Ok((r.get::<_,Option<String>>(0)?,r.get::<_,Option<String>>(1)?,r.get::<_,Option<String>>(2)?))).optional()?.ok_or_else(||StoreError::SchemaMismatch("missing local federation identity".into()))?;
        let state_id = id.ok_or_else(|| {
            StoreError::SchemaMismatch("missing local federation identity".into())
        })?;
        if label.is_none() || public_key.is_none() {
            return Err(StoreError::SchemaMismatch(
                "invalid local federation public identity".into(),
            ));
        }
        let (mut room, peer) = if approve {
            let room = record(tx, "runtime_federation_rooms", room_id)?.ok_or_else(|| {
                StoreError::SchemaMismatch("federation room is unavailable".into())
            })?;
            if room.get("status").and_then(Value::as_str) != Some("pending")
                || room.get("localApproved").and_then(Value::as_bool) == Some(true)
            {
                return Err(StoreError::SchemaMismatch(
                    "federation room is unavailable or already approved".into(),
                ));
            }
            let peer_id = room
                .get("peerId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let peer = record(tx, "runtime_federation_peers", peer_id)?.ok_or_else(|| {
                StoreError::SchemaMismatch("federation peer is unavailable".into())
            })?;
            (room, peer)
        } else {
            let peer = record(tx, "runtime_federation_peers", peer_id)?.ok_or_else(|| {
                StoreError::SchemaMismatch("select an approved federation peer".into())
            })?;
            let new_id = format!(
                "federated:{}",
                tx.query_row("SELECT lower(hex(randomblob(16)))", [], |r| r
                    .get::<_, String>(0))?
            );
            (json!({"id":new_id}), peer)
        };
        if peer.get("status").and_then(Value::as_str) != Some("approved")
            || peer.get("localApproved").and_then(Value::as_bool) != Some(true)
            || peer.get("remoteApproved").and_then(Value::as_bool) != Some(true)
        {
            return Err(StoreError::SchemaMismatch(
                "select an approved federation peer".into(),
            ));
        }
        let peer_state = peer
            .get("stateId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let mut agents = Vec::new();
        for member in &unique {
            let raw = tx
                .query_row(
                    "SELECT record FROM runtime_agents WHERE id=?1",
                    [member],
                    |r| r.get::<_, String>(0),
                )
                .optional()?
                .ok_or_else(|| {
                    StoreError::SchemaMismatch(format!("unknown local room member {member}"))
                })?;
            let agent = parse_record("runtime_agents", member, &raw)?;
            if agent
                .get("deletedAt")
                .is_some_and(|v| !v.is_null() && v.as_f64().unwrap_or(0.0) != 0.0)
            {
                return Err(StoreError::SchemaMismatch(format!(
                    "local room member {member} is deleted"
                )));
            }
            agents.push((member.clone(), agent));
        }
        let lead = agents
            .iter()
            .find(|(_, a)| a.get("isLead").and_then(Value::as_bool) == Some(true))
            .ok_or_else(|| {
                StoreError::SchemaMismatch("include the local lead in a shared room".into())
            })?;
        let root = lead
            .1
            .get("rootId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if agents
            .iter()
            .any(|(_, a)| a.get("rootId").and_then(Value::as_str) != Some(root))
        {
            return Err(StoreError::SchemaMismatch(
                "room members must belong to one local team".into(),
            ));
        }
        let mut participants = Vec::new();
        for (agent_id, agent) in &agents {
            let is_lead = agent.get("isLead").and_then(Value::as_bool) == Some(true);
            if !is_lead && !share_names {
                return Err(StoreError::SchemaMismatch(
                    "sharing additional agent membership requires opting in to their names".into(),
                ));
            }
            let mut p = json!({"id":agent_id,"role":if is_lead{"lead"}else{"agent"}});
            if is_lead || share_names {
                p["name"] = agent.get("name").cloned().unwrap_or_else(|| json!("Agent"));
            }
            if share_status {
                p["status"] = agent
                    .get("status")
                    .cloned()
                    .unwrap_or_else(|| json!("idle"));
            }
            participants.push(p);
        }
        let now = now()?;
        if approve {
            room["localMembers"] = json!(unique);
            room["localApproved"] = json!(true);
            room["remoteApproved"] = json!(false);
            room["shareNames"] = json!(share_names);
            room["shareStatus"] = json!(share_status);
            room["localParticipants"] = json!(participants);
            room["status"] = json!("pending");
            room["updated"] = json!(now);
        } else {
            let rid = room["id"].as_str().unwrap_or_default().to_owned();
            let peer_label = peer["label"].as_str().unwrap_or_default().to_owned();
            room = json!({"id":rid,"kind":"federated","peerId":peer_state,"peerLabel":peer_label,"name":format!("{peer_label} · Shared room"),"status":"pending","localMembers":unique,"remoteMembers":[],"shareNames":share_names,"shareStatus":share_status,"remoteApproved":false,"localApproved":true,"created":now,"updated":now,"sequence":0,"remoteSequence":0,"localParticipants":participants});
        }
        let room_id = room["id"].as_str().unwrap_or_default().to_owned();
        let peer_label = room["peerLabel"].as_str().unwrap_or_default();
        tx.execute("INSERT INTO runtime_federation_rooms(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![room_id,room.to_string()])?;
        let local_room = json!({"id":room_id,"kind":"federated","members":unique,"updated":now,"name":room["name"],"federation":true,"peerId":peer_state,"peerLabel":peer_label});
        tx.execute("INSERT INTO runtime_rooms(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",params![room_id,local_room.to_string()])?;
        let key = if approve {
            format!("room-accept:{room_id}")
        } else {
            format!("room:{room_id}")
        };
        let digest = crate::sha256_hex(format!("{state_id}:{key}").as_bytes());
        let message_id = format!(
            "{}-{}-{}-{}-{}",
            &digest[0..8],
            &digest[8..12],
            &digest[12..16],
            &digest[16..20],
            &digest[20..32]
        );
        let kind = if approve {
            "room-accept"
        } else {
            "room-invite"
        };
        let payload = json!({"room":room_id,"participants":room["localParticipants"],"shareNames":share_names,"shareStatus":share_status});
        let queued = json!({"id":message_id,"kind":kind,"room":room_id,"payload":payload,"created":now,"status":"queued","attempts":0,"nextAt":0,"error":null});
        let existing = record(tx, "runtime_federation_outbox", &message_id)?;
        if let Some(existing) = existing {
            if existing.get("kind") != Some(&json!(kind))
                || existing.get("payload") != Some(&payload)
            {
                return Err(StoreError::SchemaMismatch(
                    "federation message ID already has different content".into(),
                ));
            }
        } else {
            let (count,bytes):(i64,i64)=tx.query_row("SELECT count(*),COALESCE(sum(length(record)),0) FROM runtime_federation_outbox WHERE peer_id=?1 AND json_extract(record,'$.status')='queued'",[&peer_state],|r|Ok((r.get(0)?,r.get(1)?)))?;
            if count >= 10000 || bytes + queued.to_string().len() as i64 > 32 * 1024 * 1024 {
                return Err(StoreError::SchemaMismatch(
                    "federation queue is full".into(),
                ));
            }
            tx.execute(
                "INSERT INTO runtime_federation_outbox(id,peer_id,record) VALUES(?1,?2,?3)",
                params![message_id, peer_state, queued.to_string()],
            )?;
        }
        Ok(())
    }
}

const PEER_FIELDS: &[(&str, Kind)] = &[
    ("stateId", Kind::String),
    ("label", Kind::String),
    ("origin", Kind::String),
    ("publicKey", Kind::String),
    ("status", Kind::PeerStatus),
    ("whoisStatus", Kind::OptionalString),
    ("whoisUser", Kind::OptionalString),
    ("lastError", Kind::OptionalString),
    ("localApproved", Kind::Bool),
    ("remoteApproved", Kind::Bool),
    ("created", Kind::OptionalNumber),
    ("updated", Kind::OptionalNumber),
];
const INVITE_FIELDS: &[(&str, Kind)] = &[
    ("id", Kind::String),
    ("status", Kind::String),
    ("created", Kind::Number),
    ("expires", Kind::Number),
    ("expectedUser", Kind::OptionalString),
];
const ROOM_FIELDS: &[(&str, Kind)] = &[
    ("id", Kind::String),
    ("peerId", Kind::String),
    ("peerLabel", Kind::String),
    ("name", Kind::String),
    ("status", Kind::RoomStatus),
    ("localMembers", Kind::Array),
    ("remoteMembers", Kind::Array),
    ("shareNames", Kind::Bool),
    ("shareStatus", Kind::Bool),
    ("created", Kind::Number),
];
#[derive(Clone, Copy)]
enum Kind {
    String,
    OptionalString,
    Number,
    OptionalNumber,
    Bool,
    Array,
    PeerStatus,
    RoomStatus,
}
fn project(id: &str, v: &Value, table: &str, fields: &[(&str, Kind)]) -> StoreResult<Value> {
    let o = v.as_object().ok_or_else(|| invalid(table, id, "record"))?;
    let mut out = Map::new();
    for (key, kind) in fields {
        let val = o.get(*key).unwrap_or(&Value::Null);
        let valid = match kind {
            Kind::String => val.is_string(),
            Kind::OptionalString => val.is_null() || val.is_string(),
            Kind::Number => val.is_number(),
            Kind::OptionalNumber => val.is_null() || val.is_number(),
            Kind::Bool => val.is_boolean(),
            Kind::Array => val.is_array(),
            Kind::PeerStatus => matches!(val.as_str(), Some("pending" | "approved" | "revoked")),
            Kind::RoomStatus => matches!(val.as_str(), Some("pending" | "approved" | "revoked")),
        };
        if !valid
            || (matches!(kind, Kind::Number | Kind::OptionalNumber)
                && !val.is_null()
                && !val.as_f64().is_some_and(f64::is_finite))
        {
            return Err(invalid(table, id, key));
        }
        out.insert((*key).into(), val.clone());
    }
    if table == "runtime_federation_rooms" {
        for field in ["localMembers", "remoteMembers"] {
            if let Some(arr) = out[field].as_array()
                && arr.iter().any(|v| !v.is_string() && !v.is_object())
            {
                return Err(invalid(table, id, field));
            }
        }
    }
    Ok(Value::Object(out))
}
fn records(c: &Connection, t: &str, order: &str) -> StoreResult<Vec<(String, Value)>> {
    let q = format!("SELECT id,record FROM {t} {order}");
    let mut s = c.prepare(&q)?;
    let rows = s.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    rows.map(|r| {
        let (id, raw) = r?;
        Ok((id.clone(), parse_record(t, &id, &raw)?))
    })
    .collect()
}
fn record(c: &Connection, t: &str, id: &str) -> StoreResult<Option<Value>> {
    let q = format!("SELECT record FROM {t} WHERE id=?1");
    c.query_row(&q, [id], |r| r.get::<_, String>(0))
        .optional()?
        .map(|r| parse_record(t, id, &r))
        .transpose()
}
fn parse_record(t: &str, id: &str, raw: &str) -> StoreResult<Value> {
    let v: Value = serde_json::from_str(raw).map_err(|_| invalid(t, id, "record"))?;
    if !v.is_object() {
        return Err(invalid(t, id, "record"));
    }
    Ok(v)
}
fn invalid(t: &str, id: &str, field: &str) -> StoreError {
    StoreError::SchemaMismatch(format!("invalid stored object {t}/{id}: {field}"))
}
fn now() -> StoreResult<f64> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| StoreError::SchemaMismatch(e.to_string()))?
        .as_secs_f64())
}

inventory::submit! {
    crate::repositories::SchemaRegistration {
        repository: "federation",
        requirements: &REQUIREMENTS,
    }
}
