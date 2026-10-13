//! Local federation settings and projections over Python-written records.

use rusqlite::{Connection, OptionalExtension, Transaction};
use serde_json::{Map, Value, json};

use crate::schema::{ConditionalTableRequirement, SchemaRequirements, TableRequirement};
use crate::{StoreError, StoreResult};

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

const CONDITIONAL_TABLES: &[ConditionalTableRequirement] = &[ConditionalTableRequirement {
    table_exists: "runtime_federation_settings",
    tables: TABLES,
}];

pub(crate) const REQUIREMENTS: SchemaRequirements = SchemaRequirements {
    tables: &[],
    conditional_tables: CONDITIONAL_TABLES,
    triggers: &[],
    conditional_triggers: &[],
    connection_functions: &[],
    bootstrap_tables: &[],
    bootstrap_triggers: &[],
    bootstrap_connection_functions: &[],
};

#[derive(Clone, Copy, Debug, Default)]
pub struct FederationRepository;

impl FederationRepository {
    pub fn snapshot(connection: &Connection) -> StoreResult<Value> {
        let settings = record(connection, "runtime_federation_settings", "global")?
            .unwrap_or_else(|| json!({"enabled": false}));
        let enabled = settings
            .get("enabled")
            .and_then(Value::as_bool)
            .ok_or_else(|| invalid("settings.enabled", "global"))?;

        let peers = records(connection, "runtime_federation_peers", "ORDER BY id")?
            .iter()
            .map(|(id, value)| project_peer(id, value))
            .collect::<StoreResult<Vec<_>>>()?;
        let invites = records(connection, "runtime_federation_invites", "")?
            .iter()
            .map(|(id, value)| project_invite(id, value))
            .collect::<StoreResult<Vec<_>>>()?;
        let rooms = records(connection, "runtime_federation_rooms", "ORDER BY id")?
            .iter()
            .map(|(id, value)| project_room(id, value))
            .collect::<StoreResult<Vec<_>>>()?;
        let queued: i64 = connection.query_row(
            "SELECT count(*) FROM runtime_federation_outbox WHERE json_extract(record,'$.status')='queued'",
            [],
            |row| row.get(0),
        )?;
        let identity = record(connection, "runtime_federation_identity", "local")?
            .map(|value| project_identity(&value))
            .transpose()?;

        Ok(json!({
            "version": 1,
            "enabled": enabled,
            "identity": identity,
            "peers": peers,
            "invites": invites,
            "rooms": rooms,
            "queued": queued,
        }))
    }

    pub fn set_enabled(transaction: &Transaction<'_>, enabled: bool) -> StoreResult<()> {
        let updated = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?
            .as_secs_f64();
        let settings = serde_json::to_string(&json!({"enabled": enabled, "updated": updated}))?;
        transaction.execute(
            "INSERT INTO runtime_federation_settings(id,record) VALUES('global',?1) \
             ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            [settings],
        )?;
        Ok(())
    }
}

fn records(connection: &Connection, table: &str, order: &str) -> StoreResult<Vec<(String, Value)>> {
    let query = format!("SELECT id,record FROM {table} {order}");
    let mut statement = connection.prepare(&query)?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (id, raw) = row?;
        parse_record(&id, &raw).map(|value| (id, value))
    })
    .collect()
}

fn record(connection: &Connection, table: &str, id: &str) -> StoreResult<Option<Value>> {
    let query = format!("SELECT record FROM {table} WHERE id=?1");
    let raw = connection
        .query_row(&query, [id], |row| row.get::<_, String>(0))
        .optional()?;
    raw.map(|text| parse_record(id, &text)).transpose()
}

fn parse_record(id: &str, raw: &str) -> StoreResult<Value> {
    let value: Value = serde_json::from_str(raw).map_err(|_| invalid("record", id))?;
    if !value.is_object() {
        return Err(invalid("record", id));
    }
    Ok(value)
}

fn required_string<'a>(
    object: &'a Map<String, Value>,
    key: &str,
    id: &str,
) -> StoreResult<&'a str> {
    object
        .get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| invalid(key, id))
}

fn project_peer(id: &str, value: &Value) -> StoreResult<Value> {
    let object = value.as_object().ok_or_else(|| invalid("peer", id))?;
    let mut result = Map::new();
    for key in ["stateId", "label", "origin", "publicKey", "status"] {
        result.insert(
            key.to_owned(),
            Value::String(required_string(object, key, id)?.to_owned()),
        );
    }
    for key in ["whoisStatus", "whoisUser", "lastError"] {
        let value = object.get(key).unwrap_or(&Value::Null);
        if !value.is_null() && !value.is_string() {
            return Err(invalid(key, id));
        }
        result.insert(key.to_owned(), value.clone());
    }
    for key in ["localApproved", "remoteApproved"] {
        let value = object.get(key).ok_or_else(|| invalid(key, id))?;
        if !value.is_boolean() {
            return Err(invalid(key, id));
        }
        result.insert(key.to_owned(), value.clone());
    }
    for key in ["created", "updated"] {
        let value = object.get(key).unwrap_or(&Value::Null);
        if !value.is_null() && !value.is_number() {
            return Err(invalid(key, id));
        }
        result.insert(key.to_owned(), value.clone());
    }
    Ok(Value::Object(result))
}

fn project_invite(id: &str, value: &Value) -> StoreResult<Value> {
    let object = value.as_object().ok_or_else(|| invalid("invite", id))?;
    let mut result = Map::new();
    for key in ["id", "status"] {
        result.insert(
            key.to_owned(),
            Value::String(required_string(object, key, id)?.to_owned()),
        );
    }
    for key in ["created", "expires"] {
        let value = object.get(key).ok_or_else(|| invalid(key, id))?;
        if !value.is_number() {
            return Err(invalid(key, id));
        }
        result.insert(key.to_owned(), value.clone());
    }
    let expected_user = object.get("expectedUser").unwrap_or(&Value::Null);
    if !expected_user.is_null() && !expected_user.is_string() {
        return Err(invalid("expectedUser", id));
    }
    result.insert("expectedUser".to_owned(), expected_user.clone());
    Ok(Value::Object(result))
}

fn project_room(id: &str, value: &Value) -> StoreResult<Value> {
    let object = value.as_object().ok_or_else(|| invalid("room", id))?;
    let mut result = Map::new();
    for key in [
        "id",
        "peerId",
        "peerLabel",
        "name",
        "status",
        "localMembers",
        "remoteMembers",
        "shareNames",
        "shareStatus",
        "created",
    ] {
        let value = object.get(key).ok_or_else(|| invalid(key, id))?;
        let valid = match key {
            "id" | "peerId" | "peerLabel" | "name" | "status" => value.is_string(),
            "localMembers" | "remoteMembers" => value.is_array(),
            "shareNames" | "shareStatus" => value.is_boolean(),
            "created" => value.is_number(),
            _ => false,
        };
        if !valid {
            return Err(invalid(key, id));
        }
        result.insert(key.to_owned(), value.clone());
    }
    for member in object
        .get("localMembers")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("localMembers", id))?
    {
        if !member.is_string() {
            return Err(invalid("localMembers", id));
        }
    }
    for member in object
        .get("remoteMembers")
        .and_then(Value::as_array)
        .ok_or_else(|| invalid("remoteMembers", id))?
    {
        let Some(member) = member.as_object() else {
            return Err(invalid("remoteMembers", id));
        };
        required_string(member, "id", id)?;
        required_string(member, "role", id)?;
        for key in ["name", "status"] {
            let value = member.get(key).unwrap_or(&Value::Null);
            if !value.is_null() && !value.is_string() {
                return Err(invalid(key, id));
            }
        }
    }
    Ok(Value::Object(result))
}

fn project_identity(value: &Value) -> StoreResult<Value> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid("identity", "local"))?;
    let state_id = required_string(object, "stateId", "local")?;
    let label = required_string(object, "label", "local")?;
    let public_key = required_string(object, "publicKey", "local")?;
    Ok(json!({
        "stateId": state_id,
        "label": label,
        "fingerprint": crate::sha256_hex(public_key.as_bytes())[..32].to_owned(),
    }))
}

fn invalid(field: &str, id: &str) -> StoreError {
    StoreError::SchemaMismatch(format!("invalid stored federation object {field}/{id}"))
}
