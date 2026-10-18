//! Local multi-server access projections and state mutations.
use crate::multi_server_protocol_generated::ACCESS_PROTOCOL_VERSION;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::hmac;
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use studio_runtime_store::{MultiServerRepository, RuntimeStore, StoreError};
use thiserror::Error;

const INVITATION_TTL_SECONDS: i64 = 15 * 60;

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

pub async fn snapshot(store: &RuntimeStore, identity: &Value) -> Result<Value, MultiServerError> {
    let identity = identity.clone();
    Ok(store
        .transaction_with(move |tx| snapshot_in_transaction(tx, &identity, true))
        .await?)
}

/// Read a snapshot-shaped view without persisting default aliases.
/// Use for side-effect-free comparisons before authenticating signed writes.
pub async fn snapshot_for_comparison(
    store: &RuntimeStore,
    identity: &Value,
) -> Result<Value, MultiServerError> {
    let identity = identity.clone();
    Ok(store
        .transaction_with(move |tx| snapshot_in_transaction(tx, &identity, false))
        .await?)
}

fn snapshot_in_transaction(
    tx: &rusqlite::Transaction<'_>,
    identity: &Value,
    persist_default_aliases: bool,
) -> Result<Value, StoreError> {
    let aliases = server_aliases(tx, identity, persist_default_aliases)?;
    let clients = project_records(tx, "runtime_access_clients")?;
    let discovered = project_records(tx, "runtime_access_discovered")?;
    let invites = tx
        .prepare_cached(
            "SELECT id,record FROM runtime_access_invites ORDER BY rowid DESC LIMIT 100",
        )?
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .map(|r| {
            let (id, raw) = r?;
            let v = parse("runtime_access_invites", &id, &raw)?;
            let invite_id = v
                .get("inviteId")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("runtime_access_invites", &id, "inviteId"))?;
            let expires = v
                .get("expires")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid("runtime_access_invites", &id, "expires"))?;
            let created = v
                .get("created")
                .and_then(Value::as_i64)
                .ok_or_else(|| invalid("runtime_access_invites", &id, "created"))?;
            let status = v
                .get("status")
                .and_then(Value::as_str)
                .ok_or_else(|| invalid("runtime_access_invites", &id, "status"))?;
            if !["open", "used"].contains(&status) {
                return Err(invalid("runtime_access_invites", &id, "status"));
            }
            Ok(json!({"inviteId":invite_id,"expires":expires,"created":created,"status":status}))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    let auto_pair = tx
        .query_row(
            "SELECT auto_pair FROM runtime_access_settings WHERE id=1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(1)
        != 0;
    let auto_update = peer_auto_update(tx)?;
    let identity_alerts = identity_alerts(tx, identity, &clients, &discovered)?;
    let servers = server_list(&clients, discovered);

    Ok(json!({
        "protocol": ACCESS_PROTOCOL_VERSION,
        "identity": identity,
        "clients": clients,
        "servers": servers,
        "invites": invites,
        "settings": {"autoPair": auto_pair, "autoUpdate": auto_update, "aliases": aliases},
        "identityAlerts": identity_alerts,
    }))
}

/// Paired and discovered servers by ID. A saved server takes its activity from its discovery
/// record. The rules are those of `ServerDiscovery.servers` in codex_server_discovery.py.
fn server_list(clients: &[Value], discovered: Vec<Value>) -> Vec<Value> {
    let mut known = discovered
        .into_iter()
        .map(|peer| (peer["id"].as_str().unwrap_or_default().to_owned(), peer))
        .collect::<BTreeMap<_, _>>();
    for client in clients
        .iter()
        .filter(|client| client["kind"].as_str() == Some("server"))
    {
        let id = client["id"].as_str().unwrap_or_default().to_owned();
        let found = known.get(&id);
        let reachability = match found {
            Some(peer) if peer["status"].as_str() == Some("unreachable") => "unreachable",
            Some(_) => "reachable",
            None => "unknown",
        };
        let mut server = client.clone();
        server["lastSeen"] = found.map_or(Value::Null, |peer| peer["lastSeen"].clone());
        server["autoPair"] = found.map_or(Value::Null, |peer| peer["autoPair"].clone());
        server["reachability"] = json!(reachability);
        known.insert(id, server);
    }
    known
        .into_values()
        .map(|mut peer| {
            if peer.get("reachability").is_none() {
                let unreachable = peer["status"].as_str() == Some("unreachable");
                peer["reachability"] = json!(if unreachable {
                    "unreachable"
                } else {
                    "reachable"
                });
            }
            peer
        })
        .collect()
}

/// The peer auto-update setting (docs/peer-updates.md). The Python backend creates its table;
/// without the table the setting has its contract default, on.
fn peer_auto_update(tx: &rusqlite::Transaction<'_>) -> Result<bool, StoreError> {
    if !table_exists(tx, "runtime_peer_update_settings")? {
        return Ok(true);
    }
    Ok(tx
        .query_row(
            "SELECT auto_update FROM runtime_peer_update_settings WHERE id=1",
            [],
            |r| r.get::<_, i64>(0),
        )
        .optional()?
        .is_none_or(|value| value != 0))
}

fn table_exists(tx: &rusqlite::Transaction<'_>, table: &str) -> Result<bool, StoreError> {
    Ok(tx
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
            [table],
            |_| Ok(()),
        )
        .optional()?
        .is_some())
}

/// Identity changes that leave live references to a lost server. They are derived on each
/// read and never stored. The rules are those of `identity_alerts` in codex_multi_server.py
/// (docs/multi-server.md, "Replace a lost server identity").
fn identity_alerts(
    tx: &rusqlite::Transaction<'_>,
    identity: &Value,
    clients: &[Value],
    discovered: &[Value],
) -> Result<Vec<Value>, StoreError> {
    let id_of = |record: &Value| record["id"].as_str().unwrap_or_default().to_owned();
    let servers = clients
        .iter()
        .filter(|record| record["kind"].as_str() == Some("server"))
        .collect::<Vec<_>>();
    let replaced = servers
        .iter()
        .filter(|record| {
            record["replacedBy"]
                .as_str()
                .is_some_and(|id| !id.is_empty())
        })
        .map(|record| id_of(record))
        .collect::<BTreeSet<_>>();
    let mut known = servers
        .iter()
        .map(|record| (id_of(record), *record))
        .collect::<BTreeMap<_, _>>();
    for record in discovered {
        known.entry(id_of(record)).or_insert(record);
    }
    let mut successors = servers
        .iter()
        .filter(|record| record["status"].as_str() == Some("paired"))
        .collect::<Vec<_>>();
    successors.sort_by_key(|record| id_of(record));
    let mut alerts = Vec::new();
    for successor in successors {
        let new = id_of(successor);
        let Some(origin) = successor["origin"]
            .as_str()
            .filter(|origin| !origin.is_empty())
        else {
            continue;
        };
        for (old, peer) in &known {
            if *old != new && !replaced.contains(old) && peer["origin"].as_str() == Some(origin) {
                alerts.push(json!({
                    "code": "peer_identity_changed",
                    "oldServerId": old,
                    "newServerId": new,
                    "origin": origin,
                    "detail": format!(
                        "{origin} now presents server {new}. Run replace_server with oldServerId {old} after the old identity is gone."
                    ),
                }));
            }
        }
    }
    let local = identity["serverId"].as_str().unwrap_or_default();
    for old in previous_local_identities(tx, local)?.difference(&replaced) {
        alerts.push(json!({
            "code": "local_identity_changed",
            "oldServerId": old,
            "newServerId": local,
            "origin": identity["origin"],
            "detail": format!(
                "Local state still names this server {old}. Run replace_server here, and on each paired server that names {old}, with newServerId {local}."
            ),
        }));
    }
    Ok(alerts)
}

/// Server IDs that local state uses for this server itself, other than its current ID: a
/// home-side link names this server as its home, and a project names its own server as its
/// home. The rules are those of `previous_local_identities` in codex_server_replacement.py.
fn previous_local_identities(
    tx: &rusqlite::Transaction<'_>,
    local: &str,
) -> Result<BTreeSet<String>, StoreError> {
    let queries = [
        (
            "runtime_server_links",
            "SELECT DISTINCT json_extract(record,'$.home') FROM runtime_server_links WHERE json_extract(record,'$.side')='home'",
        ),
        (
            "runtime_projects",
            "SELECT DISTINCT json_extract(record,'$.homeServerId') FROM runtime_projects",
        ),
    ];
    let mut found = BTreeSet::new();
    for (table, query) in queries {
        if !table_exists(tx, table)? {
            continue;
        }
        let mut statement = tx.prepare_cached(query)?;
        let rows = statement.query_map([], |row| row.get::<_, rusqlite::types::Value>(0))?;
        for value in rows {
            if let rusqlite::types::Value::Text(id) = value? {
                found.insert(id);
            }
        }
    }
    found.remove(local);
    Ok(found)
}

pub async fn manage(
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
    let identity = identity.clone();
    let body = body.clone();
    let actor = actor.to_owned();
    store.transaction_with(move |tx| -> Result<Value, MultiServerError> {
        match action.as_str() {
            "settings" => { tx.execute("UPDATE runtime_access_settings SET auto_pair=?1 WHERE id=1",[i64::from(auto)])?; audit_write(tx,"local",&actor,"settings")?; }
            "revoke" => { let id=body.get("clientId").and_then(Value::as_str).unwrap_or_default(); let mut client=stored(tx,"runtime_access_clients",id)?.ok_or_else(||MultiServerError::Refused("client does not exist".into()))?; if client.get("status").and_then(Value::as_str)!=Some("revoked") { client["status"]=json!("revoked"); client["revoked"]=json!(unix_time()?); tx.execute("UPDATE runtime_access_clients SET record=?1 WHERE id=?2",rusqlite::params![client.to_string(),id])?; audit_write(tx,id,&actor,"revoke")?; } }
            "alias" => {
                if !valid_identifier(&server) || !valid_identifier(&request) || !valid_alias(&alias) {
                    return Err(MultiServerError::Refused("invalid server alias request".into()));
                }
                let local = server == "local" || server == identity["serverId"].as_str().unwrap_or_default();
                let target = if local { "local" } else { server.as_str() };
                let prior=tx.query_row("SELECT server,alias FROM runtime_access_alias_requests WHERE id=?1",[&request],|r|Ok((r.get::<_,String>(0)?,r.get::<_,String>(1)?))).optional()?;
                if let Some((saved_target,saved_alias)) = prior {
                    if saved_target != target || saved_alias != alias {
                        return Err(MultiServerError::Conflict("request ID has different content".into()));
                    }
                } else {
                    let aliases = ensure_server_aliases(tx, &identity)?;
                    let local_id = identity["serverId"].as_str().unwrap_or_default();
                    if !local && !aliases.contains_key(&server) {
                        return Err(MultiServerError::Refused("server does not exist".into()));
                    }
                    let collision = aliases.iter().any(|(id, existing)| {
                        existing == &alias && id != target && !(local && id == local_id)
                    });
                    if collision {
                        return Err(MultiServerError::Conflict("server alias is already in use".into()));
                    }
                    if local {
                        tx.execute("INSERT INTO runtime_access_local_alias VALUES(1,?) ON CONFLICT(id) DO UPDATE SET alias=excluded.alias", [&alias])?;
                    } else {
                        let mut peer = stored(tx, "runtime_access_clients", &server)?
                            .ok_or_else(|| MultiServerError::Refused("server does not exist".into()))?;
                        peer["alias"] = json!(alias);
                        tx.execute("UPDATE runtime_access_clients SET record=?1 WHERE id=?2", rusqlite::params![peer.to_string(), server])?;
                    }
                    tx.execute("INSERT INTO runtime_access_alias_requests VALUES(?,?,?)", rusqlite::params![request,target,alias])?;
                    audit_write(tx,target,&actor,"alias")?;
                }
            }
            "unrevoke" => { let prior=tx.query_row("SELECT client FROM runtime_access_unrevokes WHERE id=?1",[&request],|r|r.get::<_,String>(0)).optional()?; if let Some(saved)=prior {if saved!=target{return Err(MultiServerError::Conflict("request ID selected another server".into()));}} else {let v=stored(tx,"runtime_access_clients",&target)?.ok_or_else(||MultiServerError::Refused("select a revoked server".into()))?;if v.get("status").and_then(Value::as_str)!=Some("revoked")||v.get("kind").and_then(Value::as_str)!=Some("server"){return Err(MultiServerError::Refused("select a revoked server".into()));}tx.execute("DELETE FROM runtime_access_clients WHERE id=?1",[&target])?;tx.execute("DELETE FROM runtime_access_auto_attempts WHERE id=?1",[&target])?;tx.execute("INSERT INTO runtime_access_unrevokes VALUES(?1,?2)",rusqlite::params![request,target])?;audit_write(tx,&target,&actor,"unrevoke")?;} }
            _ => return Err(MultiServerError::Refused(format!("unsupported local multi-server action: {action}"))),
        }
        let clients=project_records(tx,"runtime_access_clients")?;
        let records=MultiServerRepository::audit(tx)?;
        let _=(clients,records);
        snapshot_in_transaction(tx, &identity, true).map_err(MultiServerError::from)
    }).await
}

/// Create a single-use signed pairing invitation. The token is deterministically
/// recoverable from the saved invite and request ID, so an exact retry returns
/// the same secret without replacing the durable invitation.
pub async fn create_invite(
    store: Arc<RuntimeStore>,
    identity: &Value,
    body: &Value,
    actor: &str,
    tailscale_user: Option<&str>,
) -> Result<Value, MultiServerError> {
    let request_id = body
        .get("requestId")
        .and_then(Value::as_str)
        .filter(|value| valid_identifier(value))
        .ok_or_else(|| MultiServerError::Refused("Invalid request ID".into()))?;
    let origin = identity
        .get("origin")
        .and_then(Value::as_str)
        .and_then(|value| crate::remote_transport::validate_federation_origin(value).ok())
        .ok_or_else(|| {
            MultiServerError::Refused("Configure Tailscale Serve HTTPS before pairing".into())
        })?;
    let saved_label = identity
        .get("label")
        .and_then(Value::as_str)
        .ok_or_else(|| MultiServerError::Refused("The local server label is unavailable".into()))?;
    let label = match body.get("label") {
        None | Some(Value::Null) => saved_label,
        Some(Value::String(label)) => {
            let label = label.trim();
            if label.is_empty() || label.chars().count() > 80 {
                return Err(MultiServerError::Refused(
                    "Server label must be 1 to 80 characters".into(),
                ));
            }
            label
        }
        Some(_) => {
            return Err(MultiServerError::Refused(
                "Server label must be 1 to 80 characters".into(),
            ));
        }
    };
    let tailscale_user = tailscale_user
        .filter(|user| !user.is_empty() && user.len() <= 254)
        .ok_or_else(|| {
            MultiServerError::Refused("The Tailscale owner identity is unavailable".into())
        })?;
    let identity_root = store.root().to_path_buf();
    let stored = tokio::task::spawn_blocking(move || {
        crate::remote_transport::read_multi_server_identity(&identity_root)
    })
    .await
    .map_err(|_| MultiServerError::Refused("The local server credentials are unavailable".into()))?
    .map_err(|_| {
        MultiServerError::Refused("The local server credentials are unavailable".into())
    })?;
    let fingerprint = studio_runtime_store::sha256_hex(
        crate::remote_transport::canonical_json(body)
            .map_err(|_| MultiServerError::Refused("Invalid pairing request".into()))?
            .as_bytes(),
    );
    let store_for_read = Arc::clone(&store);
    let request_id_for_read = request_id.to_owned();
    let prior = tokio::task::spawn_blocking(move || {
        store_for_read.read_transaction(|tx| {
            let saved: Option<(String, String)> = tx
                .query_row(
                    "SELECT hash,record FROM runtime_access_invites WHERE request_id=?1",
                    [&request_id_for_read],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            saved
                .map(|(hash, raw)| {
                    let record: Value = serde_json::from_str(&raw).map_err(|_| {
                        StoreError::SchemaMismatch(format!(
                            "invalid stored object runtime_access_invites/{request_id_for_read}"
                        ))
                    })?;
                    Ok((hash, record))
                })
                .transpose()
        })
    })
    .await
    .map_err(|_| MultiServerError::Refused("The invitation store is unavailable".into()))??;
    let invite = if let Some(invite) = prior {
        if invite.0 != fingerprint {
            return Err(MultiServerError::Conflict(
                "The request ID has different content".into(),
            ));
        }
        invite.1
    } else {
        let now = unix_time()?;
        let invite_id = uuid::Uuid::new_v4().simple().to_string();
        let token_data = format!("invite-v1\n{invite_id}\n{request_id}");
        let token = URL_SAFE_NO_PAD.encode(
            hmac::sign(
                &hmac::Key::new(hmac::HMAC_SHA256, stored.private_key.as_bytes()),
                token_data.as_bytes(),
            )
            .as_ref(),
        );
        let invite = json!({
            "protocol": ACCESS_PROTOCOL_VERSION,
            "inviteId": invite_id,
            "serverId": identity["serverId"],
            "label": label,
            "origin": origin,
            "publicKey": identity["publicKey"],
            "tailscaleUser": tailscale_user,
            "expires": now as i64 + INVITATION_TTL_SECONDS,
            "created": now,
            "status": "open",
            "tokenHash": studio_runtime_store::sha256_hex(token.as_bytes()),
        });
        let invite_id = invite_id.clone();
        let request_id = request_id.to_owned();
        let fingerprint = fingerprint.clone();
        let invite_for_write = invite.clone();
        let actor = actor.to_owned();
        store
            .transaction_with(move |tx| {
                tx.execute(
                "INSERT INTO runtime_access_invites(id,request_id,hash,record) VALUES(?1,?2,?3,?4)",
                rusqlite::params![invite_id, request_id, fingerprint, invite_for_write.to_string()],
            )?;
                audit_write(tx, "local", &actor, "create_invite")?;
                Ok::<_, StoreError>(())
            })
            .await?;
        invite
    };
    let invite_id = invite["inviteId"].as_str().unwrap_or_default();
    let token_data = format!("invite-v1\n{invite_id}\n{request_id}");
    let token = URL_SAFE_NO_PAD.encode(
        hmac::sign(
            &hmac::Key::new(hmac::HMAC_SHA256, stored.private_key.as_bytes()),
            token_data.as_bytes(),
        )
        .as_ref(),
    );
    let invitation = json!({
        "protocol": invite["protocol"], "inviteId": invite["inviteId"],
        "serverId": invite["serverId"], "label": invite["label"],
        "origin": invite["origin"], "publicKey": invite["publicKey"],
        "tailscaleUser": invite["tailscaleUser"], "expires": invite["expires"],
        "token": token,
    });
    Ok(json!({
        "invitation": invitation,
        "expires": invite["expires"]
    }))
}

fn project_records(tx: &rusqlite::Transaction<'_>, table: &str) -> Result<Vec<Value>, StoreError> {
    let q = format!("SELECT id,record FROM {table} ORDER BY id");
    let mut st = tx.prepare_cached(&q)?;
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
            "replacedBy",
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
            "replacedBy",
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
fn ensure_server_aliases(
    tx: &rusqlite::Transaction<'_>,
    identity: &Value,
) -> Result<BTreeMap<String, String>, StoreError> {
    server_aliases(tx, identity, true)
}

fn server_aliases(
    tx: &rusqlite::Transaction<'_>,
    identity: &Value,
    persist_default_aliases: bool,
) -> Result<BTreeMap<String, String>, StoreError> {
    let local_alias = tx
        .query_row(
            "SELECT alias FROM runtime_access_local_alias WHERE id=1",
            [],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .unwrap_or_else(|| "LOC".to_owned());
    let mut clients = project_records(tx, "runtime_access_clients")?;
    let mut aliases = BTreeMap::from([
        ("local".to_owned(), local_alias.to_owned()),
        (
            identity["serverId"].as_str().unwrap_or_default().to_owned(),
            local_alias.to_owned(),
        ),
    ]);
    let mut used = BTreeSet::from([local_alias.to_owned()]);
    for client in clients
        .iter()
        .filter(|client| client.get("kind").and_then(Value::as_str) == Some("server"))
    {
        if let Some(alias) = client.get("alias").and_then(Value::as_str) {
            used.insert(alias.to_owned());
        }
    }
    for client in &mut clients {
        if client.get("kind").and_then(Value::as_str) != Some("server") {
            continue;
        }
        let Some(id) = client.get("id").and_then(Value::as_str).map(str::to_owned) else {
            continue;
        };
        let alias = if let Some(alias) = client.get("alias").and_then(Value::as_str) {
            alias.to_owned()
        } else {
            let alias = default_server_alias(client, &mut used);
            client["alias"] = json!(alias);
            if persist_default_aliases {
                // Set only the alias: the stored record keeps the fields that the projection omits.
                tx.execute(
                    "UPDATE runtime_access_clients SET record=json_set(record,'$.alias',?1) WHERE id=?2",
                    rusqlite::params![alias, id],
                )?;
            }
            alias
        };
        aliases.insert(id, alias);
    }
    Ok(aliases)
}

fn default_server_alias(server: &Value, used: &mut BTreeSet<String>) -> String {
    let hostname = server
        .get("origin")
        .and_then(Value::as_str)
        .and_then(|origin| url::Url::parse(origin).ok())
        .and_then(|origin| origin.host_str().map(str::to_owned))
        .unwrap_or_default();
    let preferred = [
        hostname.split('.').next().unwrap_or_default(),
        server["label"].as_str().unwrap_or_default(),
    ]
    .into_iter()
    .map(|value| {
        value
            .chars()
            .filter(char::is_ascii_alphabetic)
            .take(3)
            .collect::<String>()
            .to_ascii_uppercase()
    })
    .find(|value| !value.is_empty())
    .unwrap_or_else(|| "SRV".to_owned());
    if used.insert(preferred.clone()) {
        return preferred;
    }
    for number in 0..26usize.pow(3) {
        let alias = [number / 676, (number / 26) % 26, number % 26]
            .into_iter()
            .map(|letter| char::from(b'A' + letter as u8))
            .collect::<String>();
        if used.insert(alias.clone()) {
            return alias;
        }
    }
    "SRV".to_owned()
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

fn valid_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value.as_bytes()[0].is_ascii_alphanumeric()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_.:-".contains(&byte))
}
fn valid_alias(alias: &str) -> bool {
    !alias.is_empty() && alias.len() <= 3 && alias.bytes().all(|b| b.is_ascii_uppercase())
}

#[cfg(test)]
mod tests {
    use super::{create_invite, manage};
    use crate::federation_credentials::Credentials;
    use serde_json::json;
    use std::sync::Arc;
    use studio_runtime_store::RuntimeStore;

    #[tokio::test]
    async fn invitation_uses_injected_owner_identity_and_exact_retry_is_stable() {
        let root = tempfile::tempdir().expect("isolated multi-server state");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store opens"),
        );
        let credentials = Credentials::default();
        let identity = credentials
            .identity(Arc::clone(&store), Some("https://studio.example.ts.net"))
            .await
            .expect("fixture identity is created");
        let mut identity = identity;
        identity["label"] = json!("Saved server label");
        let request =
            json!({"action":"create_invite","requestId":"invite-test","label":"Test server"});
        let first = create_invite(
            Arc::clone(&store),
            &identity,
            &request,
            "local",
            Some("owner@example.test"),
        )
        .await
        .expect("fixture owner can create invitation");
        let retry = create_invite(
            Arc::clone(&store),
            &identity,
            &request,
            "local",
            Some("owner@example.test"),
        )
        .await
        .expect("exact invitation retry reuses durable invite");
        assert_eq!(first, retry);
        assert_eq!(first["invitation"]["tailscaleUser"], "owner@example.test");
        assert!(
            !first["invitation"]["token"]
                .as_str()
                .unwrap_or_default()
                .is_empty()
        );
        let stored_token_hash = store
            .read_transaction(|tx| {
                tx.query_row(
                    "SELECT record FROM runtime_access_invites WHERE request_id='invite-test'",
                    [],
                    |row| row.get::<_, String>(0),
                )
                .map_err(studio_runtime_store::StoreError::from)
            })
            .expect("stored invitation reads");
        let stored_token_hash: serde_json::Value =
            serde_json::from_str(&stored_token_hash).expect("stored invitation JSON");
        assert_eq!(
            stored_token_hash["tokenHash"],
            studio_runtime_store::sha256_hex(
                first["invitation"]["token"]
                    .as_str()
                    .expect("token")
                    .as_bytes()
            )
        );
        let conflict = json!({"action":"create_invite","requestId":"invite-test","label":"Other"});
        assert!(
            create_invite(
                Arc::clone(&store),
                &identity,
                &conflict,
                "local",
                Some("owner@example.test"),
            )
            .await
            .is_err()
        );
        let count = store
            .read_transaction(|tx| {
                tx.query_row("SELECT count(*) FROM runtime_access_invites", [], |row| {
                    row.get::<_, i64>(0)
                })
                .map_err(studio_runtime_store::StoreError::from)
            })
            .expect("invite count reads");
        assert_eq!(count, 1);
        let default_label = create_invite(
            Arc::clone(&store),
            &identity,
            &json!({"action":"create_invite","requestId":"invite-default-label"}),
            "local",
            Some("owner@example.test"),
        )
        .await
        .expect("Python default label is used when label is omitted");
        assert_eq!(default_label["invitation"]["label"], identity["label"]);
        let null_label = create_invite(
            Arc::clone(&store),
            &identity,
            &json!({"action":"create_invite","requestId":"invite-null-label","label":null}),
            "local",
            Some("owner@example.test"),
        )
        .await
        .expect("null label uses the saved identity label");
        assert_eq!(null_label["invitation"]["label"], identity["label"]);
    }

    #[tokio::test]
    async fn peer_alias_mutation_is_local_idempotent_and_rejects_collisions() {
        let root = tempfile::tempdir().expect("isolated multi-server state");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store opens"),
        );
        let credentials = Credentials::default();
        let identity = credentials
            .identity(Arc::clone(&store), Some("https://local.example.ts.net"))
            .await
            .expect("fixture identity is created");
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2),(?3,?4)",
                    rusqlite::params![
                        "peer-a",
                        json!({"id":"peer-a","clientId":"peer-a","kind":"server","status":"paired","label":"Peer A","created":1,"publicKey":"peer-key-a","alias":"PEE"}).to_string(),
                        "peer-b",
                        json!({"id":"peer-b","clientId":"peer-b","kind":"server","status":"paired","label":"Peer B","created":1,"publicKey":"peer-key-b","alias":"OTH"}).to_string(),
                    ],
                )?;
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        "peer-c",
                        json!({"id":"peer-c","clientId":"peer-c","kind":"server","status":"paired","label":"One","origin":"https://one.example.ts.net","created":1,"publicKey":"peer-key-c"}).to_string(),
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("paired peer fixtures insert");

        let request =
            json!({"action":"alias","serverId":"peer-a","alias":"NEW","requestId":"peer-alias"});
        let first = manage(&store, &identity, &request, "local")
            .await
            .expect("a paired peer alias updates local state");
        assert_eq!(first["settings"]["aliases"]["peer-a"], "NEW");
        assert_eq!(first["settings"]["aliases"]["peer-c"], "ONE");
        let retry = manage(&store, &identity, &request, "local")
            .await
            .expect("exact alias retry returns the saved state");
        assert_eq!(retry, first);

        let changed =
            json!({"action":"alias","serverId":"peer-a","alias":"TWO","requestId":"peer-alias"});
        assert!(matches!(
            manage(&store, &identity, &changed, "local").await,
            Err(super::MultiServerError::Conflict(_))
        ));
        let collision = json!({"action":"alias","serverId":"peer-a","alias":"OTH","requestId":"peer-alias-collision"});
        assert!(matches!(
            manage(&store, &identity, &collision, "local").await,
            Err(super::MultiServerError::Conflict(_))
        ));
        let default_collision = json!({"action":"alias","serverId":"peer-a","alias":"ONE","requestId":"peer-alias-default-collision"});
        assert!(matches!(
            manage(&store, &identity, &default_collision, "local").await,
            Err(super::MultiServerError::Conflict(_))
        ));
        let missing = json!({"action":"alias","serverId":"unknown-peer","alias":"NEW","requestId":"peer-alias-missing"});
        assert!(manage(&store, &identity, &missing, "local").await.is_err());

        let audit_count = store
            .read_transaction(|tx| {
                tx.query_row(
                    "SELECT count(*) FROM runtime_access_audit WHERE client='peer-a' AND action='alias'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .map_err(studio_runtime_store::StoreError::from)
            })
            .expect("alias audit count reads");
        assert_eq!(audit_count, 1);
    }

    #[tokio::test]
    async fn default_peer_alias_is_persisted_before_a_lexically_earlier_peer_arrives() {
        let root = tempfile::tempdir().expect("isolated multi-server state");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store opens"),
        );
        let identity = crate::federation_credentials::Credentials::default()
            .identity(Arc::clone(&store), Some("https://local.example.ts.net"))
            .await
            .expect("fixture identity is created");
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        "peer-b",
                        json!({"id":"peer-b","clientId":"peer-b","kind":"server","status":"paired","label":"Bob","created":1,"publicKey":"peer-key-b","origin":"https://bob.example.ts.net"}).to_string(),
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("first paired peer inserts");

        let first = super::snapshot(&store, &identity)
            .await
            .expect("snapshot seeds peer alias");
        assert_eq!(first["settings"]["aliases"]["peer-b"], "BOB");
        let saved: String = store
            .read_transaction(|tx| {
                tx.query_row(
                    "SELECT json_extract(record,'$.alias') FROM runtime_access_clients WHERE id='peer-b'",
                    [],
                    |row| row.get(0),
                )
                .map_err(studio_runtime_store::StoreError::from)
            })
            .expect("default alias is durably saved");
        assert_eq!(saved, "BOB");

        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        "peer-a",
                        json!({"id":"peer-a","clientId":"peer-a","kind":"server","status":"paired","label":"Bob earlier","created":1,"publicKey":"peer-key-a","origin":"https://bob.example.ts.net"}).to_string(),
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("lexically earlier peer inserts");

        let later = super::snapshot(&store, &identity)
            .await
            .expect("snapshot preserves saved aliases");
        assert_eq!(later["settings"]["aliases"]["peer-b"], "BOB");
        assert_eq!(later["settings"]["aliases"]["peer-a"], "AAA");
    }

    #[tokio::test]
    async fn comparison_snapshot_does_not_persist_default_aliases() {
        let root = tempfile::tempdir().expect("isolated multi-server state");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store opens"),
        );
        let identity = crate::federation_credentials::Credentials::default()
            .identity(Arc::clone(&store), Some("https://local.example.ts.net"))
            .await
            .expect("fixture identity is created");
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)",
                    rusqlite::params![
                        "peer-b",
                        json!({"id":"peer-b","clientId":"peer-b","kind":"server","status":"paired","label":"Bob","created":1,"publicKey":"peer-key-b","origin":"https://bob.example.ts.net"}).to_string(),
                    ],
                )?;
                Ok(())
            })
            .await
            .expect("fixture peer inserts");

        let snapshot = super::snapshot_for_comparison(&store, &identity)
            .await
            .expect("comparison snapshot computes default alias");
        assert_eq!(snapshot["settings"]["aliases"]["peer-b"], "BOB");
        let saved: Option<String> = store
            .read_transaction(|tx| {
                tx.query_row(
                    "SELECT json_extract(record,'$.alias') FROM runtime_access_clients WHERE id='peer-b'",
                    [],
                    |row| row.get(0),
                )
                .map_err(studio_runtime_store::StoreError::from)
            })
            .expect("alias field remains absent");
        assert_eq!(saved, None);
    }
}
