//! Tailscale host adapter for owner-only server discovery.
use std::{
    collections::VecDeque,
    path::PathBuf,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use futures_util::future::BoxFuture;
use rusqlite::{OptionalExtension, params};
use serde_json::{Value, json};
use studio_runtime_services::{
    multi_server,
    multi_server_auth::{AccessError, refused},
    multi_server_discovery::{self, DiscoverySnapshot, ServerDiscoveryPort},
    remote_transport::{self, MultiServerExchangeInput, SignedHttpClient},
};
use studio_runtime_store::RuntimeStore;

const IDENTITY_PATH: &str = "/api/multi-server/v1/identity";
const AUTO_PAIR_PATH: &str = "/api/multi-server/v1/auto-pair";
const STATUS_PATH: &str = "/api/multi-server/v1/status";
const PROBE_TIMEOUT: Duration = Duration::from_secs(5);
const AUTO_PAIR_TIMEOUT: Duration = Duration::from_secs(15);
const DISCOVERY_PASS_TIMEOUT: Duration = Duration::from_secs(90);
const MAX_PROBE_RESPONSE: usize = 16 * 1024;
const MAX_PROBE_BATCH: usize = 4;

pub struct TailscaleServerDiscovery {
    state_root: PathBuf,
    executable: PathBuf,
    discovery_lock: Arc<AtomicBool>,
}

impl TailscaleServerDiscovery {
    pub fn new(state_root: PathBuf) -> Self {
        Self::with_executable(state_root, PathBuf::from("tailscale"))
    }

    pub fn with_executable(state_root: PathBuf, executable: PathBuf) -> Self {
        Self {
            state_root,
            executable,
            discovery_lock: Arc::new(AtomicBool::new(false)),
        }
    }

    fn probe(origin: &str, timeout: Duration) -> Result<Value, AccessError> {
        let url = format!("{origin}{IDENTITY_PATH}");
        let response = SignedHttpClient::exchange(
            "GET",
            &url,
            &[("Accept-Encoding", "identity")],
            &[],
            timeout,
            MAX_PROBE_RESPONSE,
        )
        .map_err(|_| unavailable("The peer identity is unavailable"))?;
        if response.status != 200 {
            return Err(unavailable("The peer identity is unavailable"));
        }
        serde_json::from_slice(&response.body)
            .map_err(|_| unavailable("The peer identity is invalid"))
    }

    async fn exchange_auto_pair(
        &self,
        store: Arc<RuntimeStore>,
        identity: &Value,
        origin: &str,
        peer: &Value,
        owner: &str,
        timeout: Duration,
    ) -> Result<(), AccessError> {
        let peer_id = text(peer, "serverId");
        let local_origin = text(identity, "origin");
        // Reuse the persisted request ID. The shared exchange path recovers a
        // completed receipt and refuses an unknown outcome without resending.
        let request_id = reserve_auto_pair_id(&store, peer_id, identity, local_origin).await?;
        let body = json!({
            "protocol": 1,
            "serverId": identity["serverId"],
            "label": identity["label"],
            "origin": local_origin,
            "publicKey": identity["publicKey"],
            "requestId": request_id,
        });
        let raw = remote_transport::canonical_json(&body)
            .map_err(|_| unavailable("The auto-pair request could not be encoded"))?
            .into_bytes();
        let now = unix_time()?;
        let state_root = self.state_root.clone();
        let remote_identity = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/discovery-identity",
            move || remote_transport::load_multi_server_identity(&state_root),
        )
        .await
        .map_err(|_| unavailable("The local server identity is unavailable"))?
        .map_err(|_| unavailable("The local server identity is unavailable"))?;
        let nonce = remote_transport::new_nonce()
            .map_err(|_| unavailable("The auto-pair request could not be signed"))?;
        let url = format!("{origin}{AUTO_PAIR_PATH}");
        let reply = remote_transport::exchange_multi_server(
            Arc::clone(&store),
            &remote_identity,
            &MultiServerExchangeInput {
                server_id: peer_id,
                method: "POST",
                url: &url,
                request_id: &request_id,
                timestamp: now,
                nonce: &nonce,
                body: &raw,
                expected_public_key: text(peer, "publicKey"),
                timeout,
            },
            now,
        )
        .await
        .map_err(|_| unavailable("The auto-pair outcome is unknown; its request was not resent"))?;
        if reply.get("paired") != Some(&Value::Bool(true))
            || text(&reply, "clientId") != text(identity, "serverId")
            || text(&reply, "origin") != origin
            || text(&reply, "tailscaleUser").trim().to_lowercase() != owner
        {
            return Err(refused(
                403,
                "identity_mismatch",
                "The automatic pairing response differs",
            ));
        }
        save_auto_pair(&store, peer, owner, &request_id, &raw, &reply, now).await
    }
}

impl ServerDiscoveryPort for TailscaleServerDiscovery {
    fn discover<'a>(
        &'a self,
        store: Arc<RuntimeStore>,
        identity: &'a Value,
        public_origin: &'a str,
    ) -> BoxFuture<'a, Result<Value, AccessError>> {
        Box::pin(async move {
            let _guard = DiscoveryPassGuard::acquire(Arc::clone(&self.discovery_lock))?;
            let executable = self.executable.clone();
            let status = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/discovery-status",
                move || {
                    crate::multi_server_owner::tailscale_json_from(
                        &executable,
                        &["status", "--json"],
                    )
                },
            )
            .await
            .map_err(|_| unavailable("The Tailscale peers are unavailable"))??;
            let snapshot =
                DiscoverySnapshot::from_status(&status, text(identity, "serverId"), public_origin)?;
            mark_missing_peers_unreachable(&store, &snapshot).await?;
            let mut candidates = snapshot
                .candidates()
                .into_iter()
                .map(str::to_owned)
                .collect::<VecDeque<_>>();
            let deadline = std::time::Instant::now() + DISCOVERY_PASS_TIMEOUT;
            while !candidates.is_empty() && std::time::Instant::now() < deadline {
                let mut probes = tokio::task::JoinSet::new();
                for _ in 0..MAX_PROBE_BATCH {
                    let Some(origin) = candidates.pop_front() else {
                        break;
                    };
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    let probe_origin = origin.clone();
                    probes.spawn(async move {
                        let result = studio_runtime_process::blocking_pool::spawn_blocking(
                            "runtime-server/discovery-probe",
                            move || {
                                TailscaleServerDiscovery::probe(
                                    &probe_origin,
                                    PROBE_TIMEOUT.min(remaining),
                                )
                            },
                        )
                        .await
                        .map_err(|_| unavailable("The peer identity is unavailable"))?;
                        Ok::<_, AccessError>((origin, result))
                    });
                }
                while let Some(result) = probes.join_next().await {
                    let (origin, observed) =
                        result.map_err(|_| unavailable("The discovery worker failed"))??;
                    let mut peer = match observed {
                        Ok(peer) => peer,
                        Err(_) => {
                            mark_origin_unreachable(&store, &origin).await?;
                            continue;
                        }
                    };
                    let remaining = deadline.saturating_duration_since(std::time::Instant::now());
                    self.refresh_paired_identity(Arc::clone(&store), &mut peer, &origin, remaining)
                        .await;
                    let now = unix_time()?;
                    let decision = match multi_server_discovery::record_probe(
                        &store, &snapshot, &origin, &peer, now,
                    )
                    .await
                    {
                        Ok(decision) => decision,
                        Err(_) => {
                            mark_origin_unreachable(&store, &origin).await?;
                            continue;
                        }
                    };
                    if decision.auto_pair() && std::time::Instant::now() < deadline {
                        let timeout = AUTO_PAIR_TIMEOUT
                            .min(deadline.saturating_duration_since(std::time::Instant::now()));
                        if self
                            .exchange_auto_pair(
                                Arc::clone(&store),
                                identity,
                                &origin,
                                &peer,
                                snapshot.owner(),
                                timeout,
                            )
                            .await
                            .is_err()
                        {
                            eprintln!("automatic server pairing exchange did not complete");
                        }
                    }
                }
            }
            multi_server::snapshot(&store, identity)
                .await
                .map_err(|_| unavailable("The server discovery snapshot is unavailable"))
        })
    }
}

struct DiscoveryPassGuard(Arc<AtomicBool>);

impl DiscoveryPassGuard {
    fn acquire(lock: Arc<AtomicBool>) -> Result<Self, AccessError> {
        lock.compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| Self(lock))
            .map_err(|_| refused(409, "discovery_busy", "A discovery pass is already running"))
    }
}

impl Drop for DiscoveryPassGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}

impl TailscaleServerDiscovery {
    /// Python refreshes the display name of already-paired peers through the
    /// signed status endpoint before publishing discovery results. A failed
    /// refresh leaves the previously trusted name intact.
    async fn refresh_paired_identity(
        &self,
        store: Arc<RuntimeStore>,
        peer: &mut Value,
        origin: &str,
        remaining: Duration,
    ) {
        let id = text(peer, "serverId").to_owned();
        let paired_store = Arc::clone(&store);
        let paired_id = id.clone();
        let paired = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/discovery-paired-check",
            move || {
                paired_store.read_transaction(|tx| {
                    tx.query_row(
                        "SELECT record FROM runtime_access_clients WHERE id=?1",
                        [&paired_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()
                    .map_err(Into::into)
                })
            },
        )
        .await
        .ok()
        .and_then(Result::ok)
        .flatten()
        .and_then(|record| serde_json::from_str::<Value>(&record).ok())
        .is_some_and(|record| text(&record, "status") == "paired");
        if !paired {
            return;
        }
        let state_root = self.state_root.clone();
        let Some(identity) = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/discovery-status-identity",
            move || remote_transport::load_multi_server_identity(&state_root),
        )
        .await
        .ok()
        .and_then(Result::ok) else {
            return;
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let Ok(nonce) = remote_transport::new_nonce() else {
            return;
        };
        let Ok(now) = unix_time() else { return };
        let url = format!("{origin}{STATUS_PATH}");
        let Ok(status) = remote_transport::exchange_multi_server(
            Arc::clone(&store),
            &identity,
            &MultiServerExchangeInput {
                server_id: &id,
                method: "GET",
                url: &url,
                request_id: &request_id,
                timestamp: now,
                nonce: &nonce,
                body: &[],
                expected_public_key: text(peer, "publicKey"),
                timeout: PROBE_TIMEOUT.min(remaining),
            },
            now,
        )
        .await
        else {
            return;
        };
        if text(&status, "serverId") != id
            || text(&status, "origin") != origin
            || text(&status, "publicKey") != text(peer, "publicKey")
            || text(&status, "label").trim().is_empty()
        {
            return;
        }
        let label = status["label"].clone();
        let saved_label = label.clone();
        let peer_public_key = text(peer, "publicKey").to_owned();
        let saved = store
            .transaction_with(move |tx| {
                let raw = tx
                    .query_row(
                        "SELECT record FROM runtime_access_clients WHERE id=?1",
                        [&id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?;
                let Some(raw) = raw else {
                    return Ok::<_, studio_runtime_store::StoreError>(false);
                };
                let mut client: Value = serde_json::from_str(&raw).map_err(|_| {
                    studio_runtime_store::StoreError::SchemaMismatch(
                        "invalid stored object runtime_access_clients".into(),
                    )
                })?;
                if text(&client, "status") != "paired"
                    || text(&client, "publicKey") != peer_public_key
                {
                    return Ok(false);
                }
                client["label"] = saved_label.clone();
                tx.execute(
                    "UPDATE runtime_access_clients SET record=?1 WHERE id=?2",
                    params![client.to_string(), id],
                )?;
                Ok(true)
            })
            .await;
        if matches!(saved, Ok(true)) {
            peer["label"] = label;
        }
    }
}

async fn reserve_auto_pair_id(
    store: &RuntimeStore,
    peer_id: &str,
    identity: &Value,
    origin: &str,
) -> Result<String, AccessError> {
    let peer_id = peer_id.to_owned();
    let request_content = json!({
        "protocol": 1,
        "serverId": identity["serverId"],
        "label": identity["label"],
        "origin": origin,
        "publicKey": identity["publicKey"],
    });
    store.transaction_with(move |tx| {
        let saved = tx
            .query_row(
                "SELECT record FROM runtime_access_auto_attempts WHERE id=?1",
                [&peer_id],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if let Some(saved_raw) = &saved {
            let saved: Value = serde_json::from_str(saved_raw).map_err(|_| {
                studio_runtime_store::StoreError::SchemaMismatch(
                    "invalid stored object runtime_access_auto_attempts".into(),
                )
            })?;
            if saved.get("protocol") == request_content.get("protocol")
                && saved.get("serverId") == request_content.get("serverId")
                && saved.get("label") == request_content.get("label")
                && saved.get("origin") == request_content.get("origin")
                && saved.get("publicKey") == request_content.get("publicKey")
                && saved.get("requestId").and_then(Value::as_str).is_some()
            {
                return Ok(saved["requestId"].as_str().unwrap_or_default().to_owned());
            }
        }
        let request_id = uuid::Uuid::new_v4().to_string();
        let mut record = request_content;
        record["requestId"] = json!(request_id);
        record["state"] = json!("attempted");
        tx.execute(
            "INSERT INTO runtime_access_auto_attempts(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![peer_id, record.to_string()],
        )?;
        Ok::<_, studio_runtime_store::StoreError>(request_id)
    }).await
    .map_err(|_| unavailable("The auto-pair intent could not be saved"))
}

async fn save_auto_pair(
    store: &RuntimeStore,
    peer: &Value,
    owner: &str,
    request_id: &str,
    raw: &[u8],
    reply: &Value,
    now: i64,
) -> Result<(), AccessError> {
    let id = text(peer, "serverId").to_owned();
    let url = format!("{}{AUTO_PAIR_PATH}", text(peer, "origin"));
    let fingerprint = remote_transport::multi_server_request_fingerprint("POST", &url, raw)
        .map_err(|_| unavailable("The auto-pair response identity is invalid"))?;
    let peer = peer.clone();
    let reply = reply.clone();
    let owner = owner.to_owned();
    let request_id = request_id.to_owned();
    store.transaction_with(move |tx| {
        let auto_pair_enabled = tx
            .query_row(
                "SELECT auto_pair FROM runtime_access_settings WHERE id=1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(1)
            != 0;
        if !auto_pair_enabled {
            return Ok::<_, studio_runtime_store::StoreError>(false);
        }
        let saved_raw = tx
            .query_row(
                "SELECT record FROM runtime_access_clients WHERE id=?1",
                [id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        let saved = saved_raw
            .map(|raw| serde_json::from_str::<Value>(&raw))
            .transpose()
            .map_err(|_| {
                studio_runtime_store::StoreError::SchemaMismatch(
                    "invalid stored object runtime_access_clients".into(),
                )
            })?;
        if let Some(saved) = &saved
            && (text(saved, "status") == "revoked"
                || text(saved, "publicKey") != text(&peer, "publicKey")
                || text(saved, "origin") != text(&peer, "origin"))
        {
            return Err(studio_runtime_store::StoreError::SchemaMismatch(
                "auto-paired server identity changed".into(),
            ));
        }
        let created = saved
            .as_ref()
            .and_then(|saved| saved.get("created"))
            .cloned()
            .unwrap_or(json!(now));
        let record = json!({
            "id": id,
            "clientId": id,
            "serverId": id,
            "kind": "server",
            "label": peer["label"],
            "origin": peer["origin"],
            "publicKey": peer["publicKey"],
            "tailscaleUser": owner,
            "status": "paired",
            "created": created,
            "lastAccess": null,
            "revoked": null,
            "autoPairingRequestId": request_id,
            "autoPairingHash": fingerprint,
            "autoPairingReceipt": reply,
        });
        tx.execute(
            "INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET record=excluded.record",
            params![id, record.to_string()],
        )?;
        if saved.is_none() {
            tx.execute(
                "INSERT INTO runtime_access_audit(client,actor,action,created) VALUES(?1,'local','auto_pair',?2)",
                params![id, now],
            )?;
        }
        Ok::<_, studio_runtime_store::StoreError>(true)
    }).await
    .map_err(|_| unavailable("The auto-paired server could not be registered"))?
    .then_some(())
    .ok_or_else(|| refused(403, "auto_pair_disabled", "Automatic pairing is disabled"))
}

async fn mark_missing_peers_unreachable(
    store: &Arc<RuntimeStore>,
    snapshot: &DiscoverySnapshot,
) -> Result<(), AccessError> {
    let read_store = Arc::clone(store);
    let origins = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/discovery-read-peers",
        move || {
            read_store.read_transaction(|tx| {
                let mut statement = tx.prepare("SELECT record FROM runtime_access_discovered")?;
                let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
                Ok(rows.collect::<Result<Vec<_>, _>>()?)
            })
        },
    )
    .await
    .map_err(|_| unavailable("Stored discovery state is unavailable"))?
    .map_err(|_| unavailable("Stored discovery state is unavailable"))?;
    for raw in origins {
        let peer: Value = serde_json::from_str(&raw)
            .map_err(|_| unavailable("Stored discovery identity is invalid"))?;
        let origin = text(&peer, "origin");
        if !snapshot.has_candidate(origin) {
            mark_origin_unreachable(store, origin).await?;
        }
    }
    Ok(())
}

async fn mark_origin_unreachable(store: &RuntimeStore, origin: &str) -> Result<(), AccessError> {
    let origin = origin.to_owned();
    store
        .transaction_with(move |tx| {
            let mut statement = tx.prepare(
                "SELECT id,record FROM runtime_access_discovered WHERE json_extract(record,'$.origin')=?1",
            )?;
            let rows = statement
                .query_map([origin.as_str()], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (id, raw) in rows {
                let mut peer: Value = serde_json::from_str(&raw).map_err(|_| {
                    studio_runtime_store::StoreError::SchemaMismatch(
                        "invalid stored object runtime_access_discovered".into(),
                    )
                })?;
                peer["status"] = json!("unreachable");
                tx.execute(
                    "UPDATE runtime_access_discovered SET record=?1 WHERE id=?2",
                    params![peer.to_string(), id],
                )?;
            }
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await
        .map_err(|_| unavailable("Stored discovery state could not be updated"))
}

fn unix_time() -> Result<i64, AccessError> {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs() as i64)
        .map_err(|_| unavailable("The system clock is unavailable"))
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or_default()
}

fn unavailable(message: &'static str) -> AccessError {
    refused(503, "identity_unavailable", message)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[tokio::test]
    async fn auto_pair_request_identity_is_persisted_before_network_send() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).await.unwrap();
        let identity = json!({"serverId":"local","label":"Local","publicKey":"key"});
        let first = reserve_auto_pair_id(&store, "peer", &identity, "https://peer.example")
            .await
            .unwrap();
        drop(store);
        let store = RuntimeStore::open(Some(root.path())).await.unwrap();
        let second = reserve_auto_pair_id(&store, "peer", &identity, "https://peer.example")
            .await
            .unwrap();
        assert_eq!(
            second, first,
            "the durable request identity is recovered after restart"
        );
        let saved: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT json_extract(record,'$.requestId') FROM runtime_access_auto_attempts WHERE id='peer'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved, first);
        let state: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT json_extract(record,'$.state') FROM runtime_access_auto_attempts WHERE id='peer'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(state, "attempted");
    }

    #[tokio::test]
    async fn auto_pair_registration_uses_absolute_url_and_rechecks_setting_atomically() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).await.unwrap();
        let peer = json!({
            "serverId":"peer", "origin":"https://peer.example",
            "publicKey":"peer-key", "label":"Peer"
        });
        let reply = json!({
            "paired":true, "clientId":"local", "origin":"https://local.example",
            "tailscaleUser":"owner@example.test"
        });
        let raw = br#"{"requestId":"auto-request"}"#;

        store
            .transaction(|tx| {
                tx.execute(
                    "UPDATE runtime_access_settings SET auto_pair=0 WHERE id=1",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("disable auto-pair during exchange");
        let denied = save_auto_pair(
            &store,
            &peer,
            "owner@example.test",
            "auto-request",
            raw,
            &reply,
            100,
        )
        .await;
        assert!(matches!(
            denied,
            Err(AccessError::Refused {
                status: 403,
                code: "auto_pair_disabled",
                ..
            })
        ));
        let clients: i64 = store
            .read()
            .unwrap()
            .query_row("SELECT COUNT(*) FROM runtime_access_clients", [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(clients, 0, "disabled setting prevents the grant");

        store
            .transaction(|tx| {
                tx.execute(
                    "UPDATE runtime_access_settings SET auto_pair=1 WHERE id=1",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("enable auto-pair");
        save_auto_pair(
            &store,
            &peer,
            "owner@example.test",
            "auto-request",
            raw,
            &reply,
            100,
        )
        .await
        .expect("register after re-enabling auto-pair");
        let expected = remote_transport::multi_server_request_fingerprint(
            "POST",
            "https://peer.example/api/multi-server/v1/auto-pair",
            raw,
        )
        .expect("absolute HTTPS request fingerprint");
        let saved: String = store
            .read()
            .unwrap()
            .query_row(
                "SELECT json_extract(record,'$.autoPairingHash') FROM runtime_access_clients WHERE id='peer'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(saved, expected);
    }

    #[tokio::test]
    async fn overlapping_discovery_returns_busy_before_probing() {
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
        let discovery = TailscaleServerDiscovery::with_executable(
            root.path().to_owned(),
            root.path().join("must-not-run-tailscale"),
        );
        let _guard = DiscoveryPassGuard::acquire(Arc::clone(&discovery.discovery_lock)).unwrap();
        let error = discovery
            .discover(
                Arc::clone(&store),
                &json!({"serverId":"local"}),
                "https://local.example",
            )
            .await
            .unwrap_err();
        assert!(matches!(
            error,
            AccessError::Refused {
                status: 409,
                code: "discovery_busy",
                ..
            }
        ));
    }

    #[test]
    fn tailscale_status_uses_the_injected_fixture_binary() {
        let root = tempfile::tempdir().unwrap();
        let executable = root.path().join("tailscale-fixture");
        std::fs::write(
            &executable,
            "#!/bin/sh\nprintf '%s' '{\"BackendState\":\"Running\",\"Self\":{\"UserID\":7},\"User\":{\"7\":{\"LoginName\":\"owner@example.test\"}},\"Peer\":{}}'\n",
        )
        .unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o700)).unwrap();
        let value =
            crate::multi_server_owner::tailscale_json_from(&executable, &["status", "--json"])
                .expect("fixture Tailscale status");
        assert_eq!(value["BackendState"], "Running");
        assert_eq!(value["User"]["7"]["LoginName"], "owner@example.test");
    }
}
