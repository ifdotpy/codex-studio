//! Local, database-only federation state operations.

use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use ring::rand::{SecureRandom, SystemRandom};
use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::{FederationRepository, RuntimeStore, StoreError};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FederationError {
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("federation action refused: {0}")]
    Refused(String),
    #[error("local federation identity is required")]
    NeedsIdentity,
    #[error(transparent)]
    Credentials(#[from] crate::federation_credentials::CredentialError),
}

pub fn snapshot(store: &RuntimeStore) -> Result<Value, FederationError> {
    Ok(store.read_transaction(|transaction| FederationRepository::snapshot(transaction))?)
}

pub async fn manage(store: Arc<RuntimeStore>, body: &Value) -> Result<Value, FederationError> {
    manage_with_origin(store, body, None).await
}

pub async fn manage_with_origin(
    store: Arc<RuntimeStore>,
    body: &Value,
    public_origin: Option<&str>,
) -> Result<Value, FederationError> {
    manage_with_origin_and_whoami(store, body, public_origin, None).await
}

pub async fn manage_with_origin_and_whoami(
    store: Arc<RuntimeStore>,
    body: &Value,
    public_origin: Option<&str>,
    whoami: Option<&Value>,
) -> Result<Value, FederationError> {
    if body.get("action").and_then(Value::as_str) == Some("create_invite") {
        return create_invitation(store, body, public_origin, whoami).await;
    }
    if body.get("action").and_then(Value::as_str) == Some("accept_peer") {
        return accept_peer(store, body, public_origin).await;
    }
    let body = body.clone();
    store
        .transaction_with(move |tx| {
            let action = body
                .get("action")
                .and_then(Value::as_str)
                .unwrap_or_default();
            match action {
                "set_enabled" => FederationRepository::set_enabled(
                    tx,
                    body["enabled"].as_bool().unwrap_or(false),
                )?,
                "approve_peer" => FederationRepository::approve_peer(
                    tx,
                    body["state_id"].as_str().unwrap_or_default(),
                    body["accept_missing_whois"].as_bool().unwrap_or(false),
                )?,
                "revoke_peer" => FederationRepository::revoke_peer(
                    tx,
                    body["state_id"].as_str().unwrap_or_default(),
                )?,
                "room_options" => FederationRepository::room_options(
                    tx,
                    body["room_id"].as_str().unwrap_or_default(),
                    &body["disclosure"],
                )?,
                "create_room" => FederationRepository::room_action(tx, &body, false)?,
                "approve_room" => FederationRepository::room_action(tx, &body, true)?,
                _ => {
                    return Err(FederationError::Refused(format!(
                        "unsupported local federation action: {action}"
                    )));
                }
            }
            FederationRepository::snapshot(tx).map_err(FederationError::from)
        })
        .await
        .map_err(|error| match error {
            FederationError::Store(StoreError::SchemaMismatch(message))
                if message.starts_with("missing local federation identity") =>
            {
                FederationError::NeedsIdentity
            }
            FederationError::Store(StoreError::SchemaMismatch(message))
                if !message.starts_with("invalid stored object") =>
            {
                FederationError::Refused(message)
            }
            other => other,
        })
}

async fn create_invitation(
    store: Arc<RuntimeStore>,
    body: &Value,
    public_origin: Option<&str>,
    whoami: Option<&Value>,
) -> Result<Value, FederationError> {
    let origin = public_origin
        .and_then(|value| crate::remote_transport::validate_federation_origin(value).ok())
        .ok_or_else(|| {
            FederationError::Refused(
                "Configure Tailscale Serve before creating a pairing invitation".to_owned(),
            )
        })?;
    let store_for_read = Arc::clone(&store);
    let enabled = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/federation",
        move || {
            store_for_read.read_transaction(|tx| {
                let snapshot = FederationRepository::snapshot(tx)?;
                Ok::<_, StoreError>(snapshot["enabled"].as_bool() == Some(true))
            })
        },
    )
    .await
    .map_err(|_| FederationError::Refused("federation settings lookup failed".to_owned()))??;
    if !enabled {
        return Err(FederationError::Refused(
            "Enable federation before creating an invitation".to_owned(),
        ));
    }

    let label = body
        .get("label")
        .and_then(Value::as_str)
        .unwrap_or("Studio server")
        .trim();
    if label.is_empty() || label.chars().count() > 80 {
        return Err(FederationError::Refused(
            "Server label must be 1 to 80 characters".to_owned(),
        ));
    }
    let expected_user = body.get("expected_user").and_then(Value::as_str);
    if body
        .get("expected_user")
        .is_some_and(|value| !value.is_null())
        && expected_user.is_none_or(|value| value.chars().count() > 254)
    {
        return Err(FederationError::Refused(
            "Invalid expected Tailscale user".to_owned(),
        ));
    }
    let now = unix_time()?;
    let store_for_read = Arc::clone(&store);
    let lead_name = studio_runtime_process::blocking_pool::spawn_blocking("runtime-services/federation", move || {
        store_for_read.read_transaction(|connection| {
            connection
                .query_row(
                    "SELECT json_extract(record,'$.name') FROM runtime_agents WHERE json_extract(record,'$.isLead')=1 AND (json_extract(record,'$.deletedAt') IS NULL OR json_extract(record,'$.deletedAt')=0) ORDER BY id LIMIT 1",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map(|value| value.flatten())
                .map_err(StoreError::from)
        })
    })
    .await
    .map_err(|_| FederationError::Refused("federation identity lookup failed".to_owned()))??
    .filter(|name| !name.is_empty())
    .unwrap_or_else(|| "Lead".to_owned());
    let identity = crate::federation_credentials::Credentials::ensure_federation_identity(
        Arc::clone(&store),
        label,
        &lead_name,
        now,
    )
    .await?;
    let mut token_bytes = [0_u8; 32];
    SystemRandom::new()
        .fill(&mut token_bytes)
        .map_err(|_| FederationError::Refused("Ed25519 operation failed".to_owned()))?;
    let token = URL_SAFE_NO_PAD.encode(token_bytes);
    let invite_id = uuid::Uuid::new_v4().to_string();
    let expires = now as i64 + crate::federation_limits::INVITATION_TTL_SECONDS as i64;
    let expected_user = expected_user
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_ascii_lowercase);
    let tailscale_user = whoami
        .and_then(|value| value.get("user"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let tailscale_status = if tailscale_user.is_some()
        && whoami
            .and_then(|value| value.get("status"))
            .and_then(Value::as_str)
            == Some("verified")
    {
        "verified"
    } else {
        "unavailable"
    };
    let record = json!({
        "id": invite_id,
        "tokenHash": studio_runtime_store::sha256_hex(token.as_bytes()),
        "created": now,
        "expires": expires,
        "status": "open",
        "expectedUser": expected_user,
    });
    let invite_id_for_write = invite_id.clone();
    let record_for_write = record.clone();
    let label_for_write = label.to_owned();
    let lead_name_for_write = lead_name.clone();
    let tailscale_user_for_write = tailscale_user.map(str::to_owned);
    let tailscale_status_for_write = tailscale_status.to_owned();
    store.transaction_with(move |tx| {
        tx.execute(
            "UPDATE runtime_federation_identity SET record=json_set(record,'$.label',?1,'$.leadName',?2,'$.tailscaleUser',?3,'$.tailscaleStatus',?4) WHERE id='local'",
            rusqlite::params![label_for_write, lead_name_for_write, tailscale_user_for_write, tailscale_status_for_write],
        )?;
        tx.execute(
            "INSERT INTO runtime_federation_invites(id,record) VALUES(?1,?2)",
            rusqlite::params![invite_id_for_write, record_for_write.to_string()],
        )?;
        Ok::<_, StoreError>(())
    }).await?;
    let invitation = json!({
        "protocol": 1,
        "inviteId": invite_id,
        "token": token,
        "stateId": identity.id(),
        "label": label,
        "leadName": lead_name,
        "origin": origin,
        "publicKey": identity.public_key_pem(),
        "tailscaleUser": tailscale_user,
        "tailscaleStatus": tailscale_status,
        "expires": expires,
    });
    Ok(json!({
        "invitation": invitation,
        "expires": expires,
        "warning": "Share this invitation privately. It expires in 15 minutes and can be used once.",
    }))
}

async fn accept_peer(
    store: Arc<RuntimeStore>,
    body: &Value,
    public_origin: Option<&str>,
) -> Result<Value, FederationError> {
    let invitation = body
        .get("invitation")
        .filter(|value| value.is_object())
        .ok_or_else(|| FederationError::Refused("Invalid federation invitation".to_owned()))?;
    let protocol = invitation["protocol"].as_u64();
    let invite_id = invitation["inviteId"].as_str().unwrap_or_default();
    let token = invitation["token"].as_str().unwrap_or_default();
    let state_id = invitation["stateId"].as_str().unwrap_or_default();
    let label = invitation["label"].as_str().unwrap_or_default();
    let public_key = invitation["publicKey"].as_str().unwrap_or_default();
    let expires = invitation["expires"].as_i64().unwrap_or_default();
    let origin = crate::remote_transport::validate_federation_origin(
        invitation["origin"].as_str().unwrap_or_default(),
    )
    .map_err(|_| {
        FederationError::Refused("Federation invitation has an invalid server URL".to_owned())
    })?;
    let now = unix_time()?;
    if protocol != Some(crate::federation_limits::PROTOCOL_VERSION)
        || invite_id.is_empty()
        || token.is_empty()
        || token.len() > 128
        || state_id.is_empty()
        || label.chars().count() > 80
        || public_key.is_empty()
        || public_key.len() > 4096
        || expires < now as i64
        || expires > now as i64 + crate::federation_limits::INVITATION_TTL_SECONDS as i64
    {
        return Err(FederationError::Refused(
            "Invalid or expired federation invitation".to_owned(),
        ));
    }
    uuid::Uuid::parse_str(state_id)
        .map_err(|_| FederationError::Refused("Peer state identity is invalid".to_owned()))?;
    let store_for_read = Arc::clone(&store);
    let enabled = studio_runtime_process::blocking_pool::spawn_blocking("runtime-services/federation", move || {
        store_for_read.read_transaction(|tx| {
            tx.query_row(
                "SELECT COALESCE(json_extract(record,'$.enabled'),0) FROM runtime_federation_settings WHERE id='global'",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
        })
    })
    .await
    .map_err(|_| FederationError::Refused("federation settings lookup failed".to_owned()))??;
    if !enabled {
        return Err(FederationError::Refused(
            "Enable federation before pairing".to_owned(),
        ));
    }
    let lead_name = local_lead_name(Arc::clone(&store)).await?;
    let local = crate::federation_credentials::Credentials::ensure_federation_identity(
        Arc::clone(&store),
        "Studio server",
        &lead_name,
        now,
    )
    .await?;
    if local.id() == state_id {
        return Err(FederationError::Refused(
            "Cannot pair a server with itself".to_owned(),
        ));
    }
    let local_origin = public_origin
        .and_then(|value| crate::remote_transport::validate_federation_origin(value).ok());
    let record = json!({
        "stateId": state_id,
        "label": label.trim(),
        "leadName": invitation.get("leadName").cloned().unwrap_or(Value::Null),
        "origin": origin,
        "publicKey": public_key,
        "inviteId": invite_id,
        "inviteToken": token,
        "localOrigin": local_origin,
        "localLeadName": lead_name,
        "status": "pending",
        "localApproved": true,
        "remoteApproved": false,
        "whoisStatus": "pending",
        "whoisUser": invitation.get("tailscaleUser").cloned().unwrap_or(Value::Null),
        "created": now,
        "updated": now,
        "protocol": crate::federation_limits::PROTOCOL_VERSION,
    });
    let state_id_for_write = state_id.to_owned();
    let invite_id_for_write = invite_id.to_owned();
    let token_for_write = token.to_owned();
    let public_key_for_write = public_key.to_owned();
    let origin_for_write = origin.clone();
    let record_for_write = record.clone();
    store
        .transaction_with(move |tx| {
            let existing: Option<String> = tx
                .query_row(
                    "SELECT record FROM runtime_federation_peers WHERE id=?1",
                    [&state_id_for_write],
                    |row| row.get(0),
                )
                .optional()
                .map_err(StoreError::from)?;
            if let Some(existing) = existing {
                let existing: Value = serde_json::from_str(&existing).map_err(|_| {
                    StoreError::SchemaMismatch("invalid federation peer".to_owned())
                })?;
                if existing["inviteId"] != invite_id_for_write
                    || existing["inviteToken"] != token_for_write
                    || existing["publicKey"] != public_key_for_write
                    || existing["origin"] != origin_for_write
                {
                    return Err(FederationError::Refused(
                        "A different pairing invitation already exists for this server".to_owned(),
                    ));
                }
            } else {
                tx.execute(
                    "INSERT INTO runtime_federation_peers(id,record) VALUES(?1,?2)",
                    rusqlite::params![state_id_for_write, record_for_write.to_string()],
                )
                .map_err(StoreError::from)?;
            }
            Ok::<_, FederationError>(())
        })
        .await?;

    let transport = crate::federation_delivery::HttpsFederationTransport;
    let cancelled = std::sync::atomic::AtomicBool::new(false);
    if crate::federation_delivery::pair_peer(
        Arc::clone(&store),
        state_id,
        now,
        &transport,
        Arc::new(cancelled),
    )
    .await
    .is_err()
    {
        let state_id = state_id.to_owned();
        store.transaction_with(move |tx| {
            tx.execute(
                "UPDATE runtime_federation_peers SET record=json_set(record,'$.lastError',?1) WHERE id=?2",
                rusqlite::params!["Connection unavailable; the pairing attempt will retry", state_id],
            )
            .map_err(StoreError::from)?;
            Ok::<_, StoreError>(())
        }).await?;
    }
    snapshot(&store)
}

async fn local_lead_name(store: Arc<RuntimeStore>) -> Result<String, FederationError> {
    let value = studio_runtime_process::blocking_pool::spawn_blocking("runtime-services/federation", move || {
        store.read_transaction(|connection| {
            connection
                .query_row(
                    "SELECT json_extract(record,'$.name') FROM runtime_agents WHERE json_extract(record,'$.isLead')=1 AND (json_extract(record,'$.deletedAt') IS NULL OR json_extract(record,'$.deletedAt')=0) ORDER BY id LIMIT 1",
                    [],
                    |row| row.get::<_, Option<String>>(0),
                )
                .optional()
                .map(|value| value.flatten())
                .map_err(StoreError::from)
        })
    })
    .await
    .map_err(|_| FederationError::Refused("federation identity lookup failed".to_owned()))??;
    Ok(value
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "Lead".to_owned()))
}

fn unix_time() -> Result<f64, FederationError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .map_err(|_| FederationError::Refused("System clock is invalid".to_owned()))
}

#[cfg(test)]
mod tests {
    use super::manage_with_origin_and_whoami;
    use serde_json::json;

    #[tokio::test]
    async fn create_invite_uses_injected_tailscale_whoami_without_host_access() {
        let root = tempfile::tempdir().expect("isolated federation state");
        let store = std::sync::Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store opens"),
        );
        manage_with_origin_and_whoami(
            std::sync::Arc::clone(&store),
            &json!({"action":"set_enabled","enabled":true}),
            None,
            None,
        )
        .await
        .expect("federation can be enabled locally");
        let whoami = json!({"status":"verified","user":"owner@example.test","node":"node-1"});
        let result = manage_with_origin_and_whoami(
            std::sync::Arc::clone(&store),
            &json!({"action":"create_invite","label":"Test server"}),
            Some("https://studio.example.ts.net"),
            Some(&whoami),
        )
        .await
        .expect("fake whoami invitation succeeds");
        assert_eq!(result["invitation"]["tailscaleUser"], "owner@example.test");
        assert_eq!(result["invitation"]["tailscaleStatus"], "verified");
        let store_for_read = std::sync::Arc::clone(&store);
        let identity = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/federation",
            move || {
                store_for_read.read_transaction(|tx| {
                    tx.query_row(
                        "SELECT record FROM runtime_federation_identity WHERE id='local'",
                        [],
                        |row| row.get::<_, String>(0),
                    )
                    .map_err(studio_runtime_store::StoreError::from)
                })
            },
        )
        .await
        .expect("identity read worker completes")
        .expect("identity row reads");
        let identity: serde_json::Value = serde_json::from_str(&identity).expect("identity JSON");
        assert_eq!(identity["tailscaleUser"], "owner@example.test");
        assert_eq!(identity["tailscaleStatus"], "verified");
    }
}
