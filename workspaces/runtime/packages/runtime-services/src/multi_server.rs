//! Local multi-server access projections and state mutations.
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use studio_runtime_store::{MultiServerRepository, RuntimeStore, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum MultiServerError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),
    #[error("multi-server action refused: {0}")]
    Refused(String),
    #[error("multi-server request conflict: {0}")]
    Conflict(String),
    #[error("multi-server action requires a signed remote request")]
    NeedsSignedRequest,
}

pub fn audit(store: &RuntimeStore) -> Result<Value, MultiServerError> {
    Ok(store.read_transaction(|tx| MultiServerRepository::audit(tx))?)
}

pub fn snapshot(store: &RuntimeStore, identity: &Value) -> Result<Value, MultiServerError> {
    Ok(store.read_transaction(|tx| {
        let clients=project_records(tx,"runtime_access_clients")?;
        let discovered=project_records(tx,"runtime_access_discovered")?;
        let invites=tx.prepare("SELECT id,record FROM runtime_access_invites ORDER BY rowid DESC LIMIT 100")?
            .query_map([],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?)))?
            .map(|r| { let (id,raw)=r?; let v=parse("runtime_access_invites",&id,&raw)?; let invite_id=v.get("inviteId").and_then(Value::as_str).ok_or_else(||invalid("runtime_access_invites",&id,"inviteId"))?; let expires=v.get("expires").and_then(Value::as_i64).ok_or_else(||invalid("runtime_access_invites",&id,"expires"))?; let created=v.get("created").and_then(Value::as_i64).ok_or_else(||invalid("runtime_access_invites",&id,"created"))?; let status=v.get("status").and_then(Value::as_str).ok_or_else(||invalid("runtime_access_invites",&id,"status"))?; if !["open","used"].contains(&status){return Err(invalid("runtime_access_invites",&id,"status"));} Ok(json!({"inviteId":invite_id,"expires":expires,"created":created,"status":status})) })
            .collect::<Result<Vec<_>,StoreError>>()?;
        let auto_pair=tx.query_row("SELECT auto_pair FROM runtime_access_settings WHERE id=1",[],|r|r.get::<_,i64>(0)).optional()?.unwrap_or(1)!=0;
        let local_alias=tx.query_row("SELECT alias FROM runtime_access_local_alias WHERE id=1",[],|r|r.get::<_,String>(0)).optional()?.unwrap_or_else(||"LOC".to_owned());
        let mut aliases=serde_json::Map::new(); aliases.insert("local".into(),json!(local_alias)); aliases.insert(identity["serverId"].as_str().unwrap_or_default().to_owned(),json!(local_alias));
        for c in &clients { if c.get("kind").and_then(Value::as_str)==Some("server") && let (Some(id),Some(alias))=(c.get("id").and_then(Value::as_str),c.get("alias").and_then(Value::as_str)) { aliases.insert(id.into(),json!(alias)); } }
        let mut servers=clients.iter().filter(|v|v.get("kind").and_then(Value::as_str)==Some("server")).cloned().collect::<Vec<_>>();
        for peer in discovered {if !servers.iter().any(|saved|saved.get("id")==peer.get("id")){servers.push(peer);}}

        Ok(json!({"protocol":1,"identity":identity,"clients":clients,"servers":servers,"invites":invites,"settings":{"autoPair":auto_pair,"aliases":aliases}}))
    })?)
}

pub fn manage(
    store: &RuntimeStore,
    identity: &Value,
    body: &Value,
    actor: &str,
) -> Result<Value, MultiServerError> {
    let action = body
        .get("action")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let server = body
        .get("serverId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let target = body
        .get("clientId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let request = body
        .get("requestId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let alias = body
        .get("alias")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let auto = body
        .get("autoPair")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    store.transaction_with(|tx| -> Result<(), MultiServerError> {
        match action.as_str() {
            "settings" => { tx.execute("UPDATE runtime_access_settings SET auto_pair=?1 WHERE id=1",[i64::from(auto)])?; audit_write(tx,"local",actor,"settings")?; }
            "revoke" => { let id=body.get("clientId").and_then(Value::as_str).unwrap_or_default(); let mut client=stored(tx,"runtime_access_clients",id)?.ok_or_else(||MultiServerError::Refused("client does not exist".into()))?; if client.get("status").and_then(Value::as_str)!=Some("revoked") { client["status"]=json!("revoked"); client["revoked"]=json!(unix_time()?); tx.execute("UPDATE runtime_access_clients SET record=?1 WHERE id=?2",rusqlite::params![client.to_string(),id])?; audit_write(tx,id,actor,"revoke")?; } }
            "alias" => { let local=server=="local"||server==identity["serverId"].as_str().unwrap_or_default(); if !local {return Err(MultiServerError::NeedsSignedRequest);} else if !valid_alias(&alias) {return Err(MultiServerError::Refused("invalid server alias".into()));} let prior=tx.query_row("SELECT server,alias FROM runtime_access_alias_requests WHERE id=?1",[&request],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?; if let Some((s,a))=prior {if s!="local"||a!=alias{return Err(MultiServerError::Conflict("request ID has different content".into()));}} else {tx.execute("INSERT INTO runtime_access_local_alias VALUES(1,?) ON CONFLICT(id) DO UPDATE SET alias=excluded.alias",[&alias])?;tx.execute("INSERT INTO runtime_access_alias_requests VALUES(?,?,?)",rusqlite::params![request,"local",alias])?;audit_write(tx,"local",actor,"alias")?;} }
            "unrevoke" => { let prior=tx.query_row("SELECT client FROM runtime_access_unrevokes WHERE id=?1",[&request],|r|r.get::<_,String>(0)).optional()?; if let Some(saved)=prior {if saved!=target{return Err(MultiServerError::Conflict("request ID selected another server".into()));}} else {let v=stored(tx,"runtime_access_clients",&target)?.ok_or_else(||MultiServerError::Refused("select a revoked server".into()))?;if v.get("status").and_then(Value::as_str)!=Some("revoked")||v.get("kind").and_then(Value::as_str)!=Some("server"){return Err(MultiServerError::Refused("select a revoked server".into()));}tx.execute("DELETE FROM runtime_access_clients WHERE id=?1",[&target])?;tx.execute("DELETE FROM runtime_access_auto_attempts WHERE id=?1",[&target])?;tx.execute("INSERT INTO runtime_access_unrevokes VALUES(?1,?2)",rusqlite::params![request,target])?;audit_write(tx,&target,actor,"unrevoke")?;} }
            _ => return Err(MultiServerError::Refused(format!("unsupported local multi-server action: {action}"))),
        }
        let clients=project_records(tx,"runtime_access_clients")?;
        let records=MultiServerRepository::audit(tx)?;
        let _=(clients,records);
        Ok(())
    })?;
    snapshot(store, identity)
}

fn project_records(tx: &rusqlite::Transaction<'_>, table: &str) -> Result<Vec<Value>, StoreError> {
    let q = format!("SELECT id,record FROM {table} ORDER BY id");
    let mut st = tx.prepare(&q)?;
    let rows = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?;
    rows.map(|row| {
        let (id, raw) = row?;
        let mut v = parse(table, &id, &raw)?;
        let obj = v
            .as_object_mut()
            .ok_or_else(|| invalid(table, &id, "record"))?;
        let allowed = [
            "id",
            "clientId",
            "serverId",
            "label",
            "kind",
            "origin",
            "publicKey",
            "tailscaleUser",
            "status",
            "created",
            "lastAccess",
            "revoked",
            "reachability",
            "lastSeen",
            "autoPair",
            "alias",
        ];
        obj.retain(|k, _| allowed.contains(&k.as_str()));
        for key in ["id", "clientId", "label", "kind", "publicKey", "status"] {
            if !obj.get(key).is_some_and(Value::is_string) {
                return Err(invalid(table, &id, key));
            }
        }
        if !["ui", "server"].contains(&obj["kind"].as_str().unwrap_or_default())
            || !["paired", "revoked", "discovered", "unreachable"]
                .contains(&obj["status"].as_str().unwrap_or_default())
        {
            return Err(invalid(table, &id, "kind/status"));
        }
        for key in [
            "serverId",
            "origin",
            "tailscaleUser",
            "reachability",
            "alias",
        ] {
            if obj.get(key).is_some_and(|v| !v.is_null() && !v.is_string()) {
                return Err(invalid(table, &id, key));
            }
        }
        for key in ["created", "lastAccess", "revoked", "lastSeen"] {
            if obj
                .get(key)
                .is_some_and(|v| !v.is_null() && v.as_f64().is_none_or(|n| !n.is_finite()))
            {
                return Err(invalid(table, &id, key));
            }
        }
        if obj
            .get("autoPair")
            .is_some_and(|v| !v.is_null() && !v.is_boolean())
        {
            return Err(invalid(table, &id, "autoPair"));
        }
        if obj
            .get("reachability")
            .and_then(Value::as_str)
            .is_some_and(|v| !["reachable", "unreachable", "unknown"].contains(&v))
        {
            return Err(invalid(table, &id, "reachability"));
        }
        Ok(v)
    })
    .collect()
}
fn stored(
    tx: &rusqlite::Transaction<'_>,
    table: &str,
    id: &str,
) -> Result<Option<Value>, StoreError> {
    tx.query_row(
        &format!("SELECT record FROM {table} WHERE id=?1"),
        [id],
        |r| r.get::<_, String>(0),
    )
    .optional()?
    .map(|raw| parse(table, id, &raw))
    .transpose()
}
fn parse(table: &str, id: &str, raw: &str) -> Result<Value, StoreError> {
    let v: Value = serde_json::from_str(raw).map_err(|_| invalid(table, id, "record"))?;
    if !v.is_object() {
        return Err(invalid(table, id, "record"));
    }
    Ok(v)
}
fn invalid(t: &str, id: &str, field: &str) -> StoreError {
    StoreError::SchemaMismatch(format!("invalid stored object {t}/{id}: {field}"))
}
fn audit_write(
    tx: &rusqlite::Transaction<'_>,
    client: &str,
    actor: &str,
    action: &str,
) -> Result<(), StoreError> {
    tx.execute(
        "INSERT INTO runtime_access_audit(client,actor,action,created) VALUES(?1,?2,?3,?4)",
        rusqlite::params![client, actor, action, unix_time()?],
    )?;
    Ok(())
}
fn unix_time() -> Result<f64, StoreError> {
    Ok(std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| StoreError::SchemaMismatch(e.to_string()))?
        .as_secs_f64())
}
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty() && alias.len() <= 3 && alias.bytes().all(|b| b.is_ascii_uppercase())
}
