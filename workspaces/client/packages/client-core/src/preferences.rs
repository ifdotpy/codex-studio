//! Durable local preference edits and their exact remote batches.
use crate::protocol::{Request, Response};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use studio_client_domain::preferences as policy;

const MODEL_KEY: &str = studio_runtime_schema::shared_values::MODEL_WHITELIST_PREFERENCE_KEY;
pub(crate) fn initialize_for_migration(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS client_local_view_revisions(topic TEXT PRIMARY KEY,revision INTEGER NOT NULL CHECK(revision>=0));
        CREATE TABLE IF NOT EXISTS client_preference_fields(scope TEXT PRIMARY KEY,fields TEXT NOT NULL,intent TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_preference_owner(id INTEGER PRIMARY KEY CHECK(id=1),writer TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_preference_batches(scope TEXT PRIMARY KEY,body TEXT NOT NULL,journal TEXT NOT NULL,attempted INTEGER NOT NULL,error TEXT);
        CREATE TABLE IF NOT EXISTS client_preference_ack_barriers(scope TEXT PRIMARY KEY,projection_seq INTEGER NOT NULL CHECK(projection_seq>=0));
        CREATE TABLE IF NOT EXISTS client_preference_network(id INTEGER PRIMARY KEY CHECK(id=1),generation INTEGER NOT NULL);
        INSERT OR IGNORE INTO client_preference_network VALUES(1,0);")
}

pub(crate) fn initialize(db: &Connection) -> rusqlite::Result<()> {
    initialize_for_migration(db)?;
    // A closed owner cannot prove an in-flight HTTP result. Keep its exact batch.
    let transaction = db.unchecked_transaction()?;
    let changed=transaction.execute("UPDATE client_preference_batches SET error='The remote preference write is not confirmed.' WHERE attempted=1 AND error IS NULL",[])?;
    if changed > 0 {
        bump(&transaction, "preferences").map_err(rusqlite::Error::InvalidParameterName)?;
    }
    transaction.commit()?;
    Ok(())
}
fn sql<T>(v: rusqlite::Result<T>) -> Result<T, String> {
    v.map_err(|e| e.to_string())
}
pub(crate) fn revision(db: &Connection, topic: &str) -> Result<String, String> {
    let n: i64 = sql(db
        .query_row(
            "SELECT revision FROM client_local_view_revisions WHERE topic=?1",
            [topic],
            |r| r.get(0),
        )
        .optional())?
    .unwrap_or(0);
    if n < 0 {
        return Err("The local view revision is invalid.".into());
    }
    Ok(n.to_string())
}
pub(crate) fn bump(db: &Connection, topic: &str) -> Result<(), String> {
    let n: i64 = revision(db, topic)?
        .parse()
        .map_err(|_| "Invalid local revision.")?;
    let next = n
        .checked_add(1)
        .ok_or("The local view revision limit was reached.")?;
    sql(db.execute("INSERT INTO client_local_view_revisions VALUES(?1,?2) ON CONFLICT(topic) DO UPDATE SET revision=excluded.revision",params![topic,next]))?;
    Ok(())
}
pub(crate) fn raw(db: &Connection, key: &str) -> Result<Option<String>, String> {
    sql(db
        .query_row(
            "SELECT value FROM client_preferences WHERE key=?1",
            [key],
            |r| r.get(0),
        )
        .optional())
}
pub(crate) fn put_raw(db: &Connection, key: &str, value: Option<&str>) -> Result<bool, String> {
    if raw(db, key)?.as_deref() == value {
        return Ok(false);
    }
    match value {
        Some(value) => {
            sql(db.execute("INSERT INTO client_preferences VALUES(?1,?2) ON CONFLICT(key) DO UPDATE SET value=excluded.value",params![key,value]))?;
        }
        None => {
            sql(db.execute("DELETE FROM client_preferences WHERE key=?1", [key]))?;
        }
    }
    Ok(true)
}
fn fields(db: &Connection, scope: &str) -> Result<(Value, Value), String> {
    let row: Option<(String, String)> = sql(db
        .query_row(
            "SELECT fields,intent FROM client_preference_fields WHERE scope=?1",
            [scope],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional())?;
    row.map(|(f, i)| {
        Ok((
            serde_json::from_str(&f)
                .map_err(|e| format!("Invalid native preference fields: {e}"))?,
            serde_json::from_str(&i)
                .map_err(|e| format!("Invalid native preference intent: {e}"))?,
        ))
    })
    .unwrap_or_else(|| Ok((json!({}), json!({}))))
}
fn save_fields(db: &Connection, scope: &str, fields: &Value, intent: &Value) -> Result<(), String> {
    sql(db.execute("INSERT INTO client_preference_fields VALUES(?1,?2,?3) ON CONFLICT(scope) DO UPDATE SET fields=excluded.fields,intent=excluded.intent",params![scope,fields.to_string(),intent.to_string()]))?;
    Ok(())
}
fn workspace(db: &Connection) -> Result<Option<String>, String> {
    let raw: Option<String> = sql(db
        .query_row(
            "SELECT value FROM client_identity WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional())?;
    raw.map(|s| {
        serde_json::from_str::<Value>(&s)
            .map(|v| v["workspaceId"].as_str().map(str::to_owned))
            .map_err(|e| e.to_string())
    })
    .transpose()
    .map(Option::flatten)
}
fn materialize(db: &Connection, fields: &Value) -> Result<(), String> {
    for (name, field) in fields.as_object().ok_or("Invalid preference fields.")? {
        let Ok(path) = serde_json::from_str::<Vec<String>>(name) else {
            continue;
        };
        let Some(key) = path.first() else {
            continue;
        };
        if key.starts_with("server-") || policy::scope(key, MODEL_KEY).is_none() {
            continue;
        }
        if path.len() == 1 {
            let encoded = field["value"].to_string();
            put_raw(
                db,
                key,
                (!field["value"].is_null()).then_some(encoded.as_str()),
            )?;
        } else if path.len() == 2 {
            let mut record = if key
                == policy::contract()["appearance"]["storageKey"]
                    .as_str()
                    .expect("appearance key")
            {
                policy::appearance_defaults(cfg!(target_os = "macos"))
                    .as_object()
                    .expect("appearance defaults")
                    .clone()
            } else {
                Map::new()
            };
            record.extend(
                raw(db, key)?
                    .and_then(|s| serde_json::from_str::<Value>(&s).ok())
                    .and_then(|v| v.as_object().cloned())
                    .unwrap_or_default(),
            );
            if field["value"].is_null() {
                record.remove(&path[1]);
            } else {
                record.insert(path[1].clone(), field["value"].clone());
            }
            put_raw(db, key, Some(&json!(record).to_string()))?;
        }
    }
    Ok(())
}
fn adopt(db: &Connection) -> Result<(), String> {
    let workspace = workspace(db)?;
    for scope in ["user", "server"] {
        let (old, intent) = fields(db, scope)?;
        let mut current = old.clone();
        // Only native-imported legacy metadata can reach this reserved key.
        if sql(db
            .query_row(
                "SELECT 1 FROM client_preference_fields WHERE scope=?1",
                [scope],
                |r| r.get::<_, i64>(0),
            )
            .optional())?
        .is_none()
        {
            let key = format!(
                "{}:{scope}",
                policy::contract()["fieldCacheKey"]
                    .as_str()
                    .expect("field key")
            );
            if let Some(value) = raw(db, &key)?.and_then(|s| serde_json::from_str::<Value>(&s).ok())
            {
                current = policy::merge(&current, &value);
            }
        }
        if let Some(workspace) = &workspace {
            let source: Option<(String, u64)> = sql(db
                .query_row(
                    "SELECT document,seq FROM client_projections WHERE workspace=?1 AND scope=?2",
                    params![workspace, format!("entity:uiPreferences:{scope}")],
                    |r| Ok((r.get(0)?, crate::store::unsigned(r, 1)?)),
                )
                .optional())?;
            if let Some((encoded, seq)) = source {
                let barrier: Option<u64> = sql(db
                    .query_row(
                        "SELECT projection_seq FROM client_preference_ack_barriers WHERE scope=?1",
                        [scope],
                        |r| crate::store::unsigned(r, 0),
                    )
                    .optional())?;
                if barrier.is_some_and(|through| seq <= through) {
                    continue;
                }
                sql(db.execute(
                    "DELETE FROM client_preference_ack_barriers WHERE scope=?1",
                    [scope],
                ))?;
                let entity: Value = serde_json::from_str(&encoded).map_err(|e| e.to_string())?;
                if entity["collection"] != "uiPreferences" || entity["id"] != scope {
                    return Err("The committed preference source has a different owner.".into());
                }
                if entity["op"] == "upsert" {
                    current = policy::merge(&current, &entity["value"]["fields"]);
                }
            }
        }
        current = policy::prune(&current, MODEL_KEY);
        if current != old {
            materialize(db, &current)?;
            save_fields(db, scope, &current, &intent)?;
        }
    }
    Ok(())
}
fn sync_status(db: &Connection, scope: &str) -> Result<Value, String> {
    let batch: Option<(String, Option<String>)> = sql(db
        .query_row(
            "SELECT journal,error FROM client_preference_batches WHERE scope=?1",
            [scope],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional())?;
    if let Some((entry, Some(error))) = &batch {
        let entry: Value = serde_json::from_str(entry).map_err(|e| e.to_string())?;
        return Ok(
            json!({"state":"error","error":error,"uncertain":entry["uncertain"],"requestId":entry["requestId"]}),
        );
    }
    if batch.is_some()
        || fields(db, scope)?
            .1
            .as_object()
            .is_some_and(|v| !v.is_empty())
    {
        Ok(json!({"state":"saving"}))
    } else {
        Ok(json!({"state":"saved"}))
    }
}
fn snapshot(db: &Connection) -> Result<Value, String> {
    let mut values = Map::new();
    let mut q = sql(db.prepare("SELECT key,value FROM client_preferences ORDER BY key"))?;
    for row in sql(q.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))))? {
        let (key, raw) = sql(row)?;
        if !policy::reserved_metadata_key(&key) && !key.starts_with("browser:") {
            values.insert(key, json!(raw));
        }
    }
    let sync = json!({"user":sync_status(db,"user")?,"server":sync_status(db,"server")?});
    if sync
        .as_object()
        .expect("scope status")
        .values()
        .any(|status| !policy::valid_remote_status(status))
    {
        return Err("The native preference sync status is invalid.".into());
    }
    Ok(
        json!({"values":values,"fields":{"user":fields(db,"user")?.0,"server":fields(db,"server")?.0},"revision":revision(db,"preferences")?,"sync":sync}),
    )
}
fn fingerprint(db: &Connection) -> Result<Value, String> {
    let mut value = snapshot(db)?;
    value.as_object_mut().expect("snapshot").remove("revision");
    Ok(value)
}
fn writer(db: &Connection) -> Result<String, String> {
    if let Some(writer) = sql(db
        .query_row(
            "SELECT writer FROM client_preference_owner WHERE id=1",
            [],
            |r| r.get(0),
        )
        .optional())?
    {
        return Ok(writer);
    }
    let writer = crate::server_keys::random_id()?;
    sql(db.execute(
        "INSERT INTO client_preference_owner VALUES(1,?1)",
        [&writer],
    ))?;
    Ok(writer)
}
/// Capture a fresh public edit. Private imports and exact receipt replay do not
/// enter this owner, so an archive seed cannot invent a user edit clock.
pub(crate) fn capture_raw(
    db: &Connection,
    key: &str,
    previous: Option<&str>,
    next: Option<&str>,
) -> Result<(), String> {
    let Some(scope) = policy::scope(key, MODEL_KEY) else {
        return Ok(());
    };
    let old = policy::storage_fields(key, previous);
    let next = policy::storage_fields(key, next);
    let (mut current, mut intent) = fields(db, scope)?;
    let names = old
        .as_object()
        .expect("old fields")
        .keys()
        .chain(next.as_object().expect("next fields").keys())
        .cloned()
        .collect::<std::collections::BTreeSet<_>>();
    let changed = names
        .into_iter()
        .filter(|name| {
            old.get(name) != next.get(name)
                && current
                    .get(name)
                    .is_none_or(|f| f["value"] != next.get(name).cloned().unwrap_or(Value::Null))
        })
        .collect::<Vec<_>>();
    if changed.is_empty() {
        return Ok(());
    }
    let writer = writer(db)?;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_millis();
    let mut timestamp =
        u64::try_from(now).map_err(|_| "The preference clock exceeds its limit.")?;
    for scope in ["user", "server"] {
        for field in fields(db, scope)?
            .0
            .as_object()
            .into_iter()
            .flatten()
            .map(|(_, v)| v)
        {
            timestamp = timestamp.max(
                field["timestamp"]
                    .as_u64()
                    .unwrap_or(0)
                    .checked_add(1)
                    .ok_or("Invalid preference clock.")?,
            );
        }
    }
    if timestamp > studio_client_domain::MAX_SAFE_INTEGER as u64 {
        return Err("The preference clock exceeds its limit.".into());
    }
    for name in changed {
        let field = json!({"value":next.get(&name).cloned().unwrap_or(Value::Null),"timestamp":timestamp,"writer":writer});
        current[&name] = field.clone();
        intent[&name] = field;
    }
    current = policy::prune(&current, MODEL_KEY);
    save_fields(db, scope, &current, &intent)
}
fn edit(db: &Connection, p: &Value) -> Result<Value, String> {
    if p.as_object().is_none_or(|p| p.len() != 3) {
        return Err("Invalid preference edit fields.".into());
    }
    let key = p["key"]
        .as_str()
        .filter(|k| {
            crate::store::valid_raw_preference(k, "")
                && !k.starts_with("browser:")
                && !policy::reserved_metadata_key(k)
        })
        .ok_or("Invalid preference key.")?;
    policy::scope(key, MODEL_KEY).ok_or("This preference has no synchronized scope.")?;
    let previous = p["previous"]
        .as_object()
        .ok_or("Previous preference values are required.")?;
    let next = p["next"]
        .as_object()
        .ok_or("Next preference values are required.")?;
    let latest = raw(db, key)?
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.as_object().cloned())
        .unwrap_or_default();
    let value = json!(policy::edit(previous, next, &latest));
    if !crate::store::valid_raw_preference(key, &value.to_string()) {
        return Err("The preference value exceeds its limit.".into());
    }
    capture_raw(
        db,
        key,
        Some(&json!(latest).to_string()),
        Some(&value.to_string()),
    )?;
    put_raw(db, key, Some(&value.to_string()))?;
    Ok(value)
}
fn null_or_string<'a>(p: &'a Value, key: &str) -> Result<Option<&'a str>, String> {
    match p.get(key) {
        Some(Value::Null) => Ok(None),
        Some(Value::String(v)) => Ok(Some(v)),
        _ => Err(format!("{key} must be a string or null.")),
    }
}
fn compare(db: &Connection, p: &Value) -> Result<Value, String> {
    if p.as_object().is_none_or(|v| v.len() != 3) {
        return Err("Invalid compare exchange fields.".into());
    }
    let key = p["key"]
        .as_str()
        .filter(|k| {
            crate::store::valid_raw_preference(k, "")
                && !k.starts_with("browser:")
                && !policy::reserved_metadata_key(k)
        })
        .ok_or("Invalid preference key.")?;
    let expected = null_or_string(p, "expectedRaw")?;
    let next = null_or_string(p, "raw")?;
    if next.is_some_and(|v| !crate::store::valid_raw_preference(key, v)) {
        return Err("The preference value exceeds its limit.".into());
    }
    let old = raw(db, key)?;
    let applied = old.as_deref() == expected;
    if applied {
        capture_raw(db, key, old.as_deref(), next)?;
        put_raw(db, key, next)?;
    }
    Ok(json!({"applied":applied,"raw":if applied {next.map(str::to_owned)}else{old}}))
}
pub(crate) fn stored(db: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Response, String> {
        // Reject foreign selectors before any receipt or adoption write.
        match request.method.as_str() {
            "preferences.view" | "preferences.adopt_fields"
                if request.params.as_object().is_none_or(|v| !v.is_empty()) =>
            {
                return Err("This preference read accepts no owner or field claims.".into());
            }
            "preferences.raw_compare_exchange" => {
                if request.params.as_object().is_none_or(|p| {
                    p.len() != 3
                        || !p.contains_key("key")
                        || !p.contains_key("expectedRaw")
                        || !p.contains_key("raw")
                }) {
                    return Err("Invalid compare exchange fields.".into());
                }
            }
            _ => {}
        }
        let tx = sql(db.transaction_with_behavior(rusqlite::TransactionBehavior::Immediate))?;
        let durable = matches!(
            request.method.as_str(),
            "preferences.edit" | "preferences.adopt_fields" | "preferences.raw_compare_exchange"
        );
        if durable && let Some(reply) = sql(crate::store::replay(&tx, request))? {
            return Ok(reply);
        }
        let before = fingerprint(&tx)?;
        adopt(&tx)?;
        let mut result = match request.method.as_str() {
            "preferences.view" | "preferences.adopt_fields" => Value::Null,
            "preferences.edit" => edit(&tx, &request.params)?,
            "preferences.raw_compare_exchange" => compare(&tx, &request.params)?,
            method if method.starts_with("preference_sync.") => sync_apply(&tx, request)?,
            _ => return Err("Unknown preference command.".into()),
        };
        if fingerprint(&tx)? != before {
            bump(&tx, "preferences")?;
        }
        let view = snapshot(&tx)?;
        match request.method.as_str() {
            "preferences.view" | "preferences.adopt_fields" => result = view,
            "preferences.edit" => {
                let value = result;
                result = view;
                result["value"] = value;
            }
            "preferences.raw_compare_exchange" => result["revision"] = view["revision"].clone(),
            _ => {}
        }
        let response = Response::success(request.id.clone(), result);
        if durable {
            sql(crate::store::record(&tx, request, &response))?;
        }
        sql(tx.commit())?;
        Ok(response)
    })();
    result.unwrap_or_else(|e| Response::failure(request.id.clone(), "preference_state", e))
}

fn journal(db: &Connection, method: &str, params: Value) -> Result<Value, String> {
    crate::mutation_journal::apply_in_transaction(
        db,
        &Request {
            id: "preference-journal".into(),
            method: method.into(),
            params,
        },
    )
}
fn batch(db: &Connection, scope: &str) -> Result<Option<Value>, String> {
    let row: Option<(String, String, i64)> = sql(db
        .query_row(
            "SELECT body,journal,attempted FROM client_preference_batches WHERE scope=?1",
            [scope],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional())?;
    row.map(|(body,journal,attempted)|Ok(json!({"body":serde_json::from_str::<Value>(&body).map_err(|e|e.to_string())?,"journal":serde_json::from_str::<Value>(&journal).map_err(|e|e.to_string())?,"attempted":attempted!=0}))).transpose()
}
fn sync_apply(db: &Connection, request: &Request) -> Result<Value, String> {
    let p = &request.params;
    if request.method == "preference_sync.network_ready" {
        let old: i64 = sql(db.query_row(
            "SELECT generation FROM client_preference_network WHERE id=1",
            [],
            |r| r.get(0),
        ))?;
        let next = old
            .checked_add(1)
            .ok_or("The connection generation limit was reached.")?;
        sql(db.execute(
            "UPDATE client_preference_network SET generation=?1 WHERE id=1",
            [next],
        ))?;
        return Ok(Value::Null);
    }
    if request.method == "preference_sync.next" {
        for scope in ["user", "server"] {
            if batch(db, scope)?.is_none()
                && fields(db, scope)?
                    .1
                    .as_object()
                    .is_some_and(|v| !v.is_empty())
            {
                let (mut current, intent) = fields(db, scope)?;
                // A projected server winner does not acknowledge a local intent.
                // Submit its exact version and settle from the server's ACK.
                current
                    .as_object_mut()
                    .ok_or("Invalid preference fields.")?
                    .extend(
                        intent
                            .as_object()
                            .ok_or("Invalid preference intent.")?
                            .clone(),
                    );
                let body = json!({"scope":scope,"fields":current});
                let serialized = policy::canonical(&body);
                let id = format!(
                    "ui-preferences:{}",
                    crate::mutation_journal::digest(serialized.as_bytes())
                );
                let entry = journal(
                    db,
                    "mutation.begin",
                    json!({"path":"/api/sync/preferences","serializedPayload":serialized,"explicitRequestId":id}),
                )?;
                sql(db.execute(
                    "INSERT INTO client_preference_batches VALUES(?1,?2,?3,0,NULL)",
                    params![scope, serialized, entry.to_string()],
                ))?;
            }
            if let Some(mut batch) = batch(db, scope)? {
                batch["generation"] = json!(
                    sql(db.query_row(
                        "SELECT generation FROM client_preference_network WHERE id=1",
                        [],
                        |r| r.get::<_, i64>(0)
                    ))?
                    .to_string()
                );
                if batch["journal"]["requestId"]
                    .as_str()
                    .is_some_and(|id| p["attempted"].get(id) == Some(&batch["generation"]))
                {
                    continue;
                }
                batch["workspaceId"] = json!(workspace(db)?);
                return Ok(batch);
            }
        }
        return Ok(Value::Null);
    }
    let scope = p["scope"]
        .as_str()
        .filter(|s| matches!(*s, "user" | "server"))
        .ok_or("Invalid preference batch scope.")?;
    let Some(saved) = batch(db, scope)? else {
        return Err("The preference batch is no longer pending.".into());
    };
    if saved["journal"]["requestId"] != p["requestId"]
        || saved["journal"]["payloadDigest"] != p["payloadDigest"]
    {
        return Err("The preference batch identity changed.".into());
    }
    let params = json!({"requestId":p["requestId"],"payloadDigest":p["payloadDigest"]});
    match request.method.as_str() {
        "preference_sync.dispatch" | "preference_sync.failure" => {
            journal(db, "mutation.unknown", params)?;
            let mut entry = saved["journal"].clone();
            entry["uncertain"] = json!(true);
            let error = if request.method == "preference_sync.failure" {
                Some(
                    p["error"]
                        .as_str()
                        .filter(|s| !s.is_empty())
                        .ok_or("A preference failure message is required.")?,
                )
            } else {
                None
            };
            sql(db.execute("UPDATE client_preference_batches SET journal=?2,attempted=1,error=COALESCE(?3,error) WHERE scope=?1",params![scope,entry.to_string(),error]))?;
        }
        "preference_sync.ack" => {
            let accepted = p["fields"]
                .as_object()
                .ok_or("The remote preference acknowledgement has no fields.")?;
            if accepted.values().any(|f| !policy::valid_field(f)) {
                return Err("The remote preference acknowledgement has invalid fields.".into());
            }
            let (current, mut intent) = fields(db, scope)?;
            for (name, submitted) in saved["body"]["fields"]
                .as_object()
                .ok_or("Invalid preference batch.")?
            {
                if intent.get(name) != Some(submitted) {
                    continue;
                }
                intent
                    .as_object_mut()
                    .ok_or("Invalid preference intent.")?
                    .remove(name);
            }
            let next = policy::acknowledge(&current, &saved["body"]["fields"], &p["fields"]);
            let removed = Value::Object(
                current
                    .as_object()
                    .ok_or("Invalid preference fields.")?
                    .keys()
                    .filter(|name| next.get(*name).is_none())
                    .map(|name| (name.clone(), json!({"value":null})))
                    .collect(),
            );
            // Remove derived values without creating clocks or outbound edits.
            materialize(db, &removed)?;
            save_fields(db, scope, &next, &intent)?;
            materialize(db, &next)?;
            if let Some(workspace) = workspace(db)? {
                let through: Option<i64> = sql(db
                    .query_row(
                        "SELECT seq FROM client_projections WHERE workspace=?1 AND scope=?2",
                        params![workspace, format!("entity:uiPreferences:{scope}")],
                        |r| r.get(0),
                    )
                    .optional())?;
                if let Some(through) = through {
                    sql(db.execute("INSERT INTO client_preference_ack_barriers VALUES(?1,?2) ON CONFLICT(scope) DO UPDATE SET projection_seq=excluded.projection_seq",params![scope,through]))?;
                }
            }
            if journal(db, "mutation.settle", params)? != true {
                return Err("The preference request receipt was not retained.".into());
            }
            sql(db.execute(
                "DELETE FROM client_preference_batches WHERE scope=?1",
                [scope],
            ))?;
        }
        _ => return Err("Unknown preference replication command.".into()),
    }
    Ok(Value::Null)
}
