//! Tailscale host adapter for owner-only server discovery.
use std::{path::PathBuf, sync::Mutex, time::Duration};

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
    discovery_lock: Mutex<()>,
}

impl TailscaleServerDiscovery {
    pub fn new(state_root: PathBuf) -> Self {
        Self::with_executable(state_root, PathBuf::from("tailscale"))
    }

    pub fn with_executable(state_root: PathBuf, executable: PathBuf) -> Self {
        Self {
            state_root,
            executable,
            discovery_lock: Mutex::new(()),
        }
    }

    fn probe(&self, origin: &str, timeout: Duration) -> Result<Value, AccessError> {
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

    fn exchange_auto_pair(
        &self,
        store: &RuntimeStore,
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
        let request_id = reserve_auto_pair_id(store, peer_id, identity, local_origin)?;
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
        let remote_identity = remote_transport::load_multi_server_identity(&self.state_root)
            .map_err(|_| unavailable("The local server identity is unavailable"))?;
        let nonce = remote_transport::new_nonce()
            .map_err(|_| unavailable("The auto-pair request could not be signed"))?;
        let url = format!("{origin}{AUTO_PAIR_PATH}");
        let reply = remote_transport::exchange_multi_server(
            store,
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
        save_auto_pair(store, peer, owner, &request_id, &raw, &reply, now)
    }
}

impl ServerDiscoveryPort for TailscaleServerDiscovery {
    fn discover(
        &self,
        store: &RuntimeStore,
        identity: &Value,
        public_origin: &str,
    ) -> Result<Value, AccessError> {
        let _discovery_guard = self
            .discovery_lock
            .try_lock()
            .map_err(|_| refused(409, "discovery_busy", "A discovery pass is already running"))?;
        let status = crate::multi_server_owner::tailscale_json_from(
            &self.executable,
            &["status", "--json"],
        )?;
        let snapshot =
            DiscoverySnapshot::from_status(&status, text(identity, "serverId"), public_origin)?;
        mark_missing_peers_unreachable(store, &snapshot)?;
        let candidates = snapshot
            .candidates()
            .into_iter()
            .map(str::to_owned)
            .collect::<std::collections::VecDeque<_>>();
        let queue = std::sync::Mutex::new(candidates);
        let deadline = std::time::Instant::now() + DISCOVERY_PASS_TIMEOUT;
        std::thread::scope(|scope| {
            let mut workers = Vec::with_capacity(MAX_PROBE_BATCH);
            for _ in 0..MAX_PROBE_BATCH {
                workers.push(scope.spawn(|| -> Result<(), AccessError> {
                    loop {
                        if std::time::Instant::now() >= deadline {
                            break;
                        }
                        let origin = queue.lock().ok().and_then(|mut queue| queue.pop_front());
                        let Some(origin) = origin else { break };
                        let remaining =
                            deadline.saturating_duration_since(std::time::Instant::now());
                        let mut peer = match self.probe(&origin, PROBE_TIMEOUT.min(remaining)) {
                            Ok(peer) => peer,
                            Err(_) => {
                                mark_origin_unreachable(store, &origin)?;
                                continue;
                            }
                        };
                        self.refresh_paired_identity(store, &mut peer, &origin, remaining);
                        let now = unix_time()?;
                        let decision = match multi_server_discovery::record_probe(
                            store, &snapshot, &origin, &peer, now,
                        ) {
                            Ok(decision) => decision,
                            Err(_) => {
                                mark_origin_unreachable(store, &origin)?;
                                continue;
                            }
                        };
                        if decision.auto_pair() && std::time::Instant::now() < deadline {
                            let timeout = AUTO_PAIR_TIMEOUT
                                .min(deadline.saturating_duration_since(std::time::Instant::now()));
                            // The durable request ID is retained on every failure. The
                            // signed exchange layer turns an uncertain send into Unknown.
                            if self
                                .exchange_auto_pair(
                                    store,
                                    identity,
                                    &origin,
                                    &peer,
                                    snapshot.owner(),
                                    timeout,
                                )
                                .is_err()
                            {
                                eprintln!("automatic server pairing exchange did not complete");
                            }
                        }
                    }
                    Ok(())
                }));
            }
            for worker in workers {
                worker
                    .join()
                    .map_err(|_| unavailable("The discovery worker failed"))??;
            }
            Ok::<(), AccessError>(())
        })?;
        multi_server::snapshot(store, identity)
            .map_err(|_| unavailable("The server discovery snapshot is unavailable"))
    }
}

impl TailscaleServerDiscovery {
    /// Python refreshes the display name of already-paired peers through the
    /// signed status endpoint before publishing discovery results. A failed
    /// refresh leaves the previously trusted name intact.
    fn refresh_paired_identity(
        &self,
        store: &RuntimeStore,
        peer: &mut Value,
        origin: &str,
        remaining: Duration,
    ) {
        let id = text(peer, "serverId");
        let paired = store
            .read_transaction(|tx| {
                tx.query_row(
                    "SELECT record FROM runtime_access_clients WHERE id=?1",
                    [id],
                    |row| row.get::<_, String>(0),
                )
                .optional()
                .map_err(Into::into)
            })
            .ok()
            .flatten()
            .and_then(|record| serde_json::from_str::<Value>(&record).ok())
            .is_some_and(|record| text(&record, "status") == "paired");
        if !paired {
            return;
        }
        let Ok(identity) = remote_transport::load_multi_server_identity(&self.state_root) else {
            return;
        };
        let request_id = uuid::Uuid::new_v4().to_string();
        let Ok(nonce) = remote_transport::new_nonce() else {
            return;
        };
        let Ok(now) = unix_time() else { return };
        let url = format!("{origin}{STATUS_PATH}");
        let Ok(status) = remote_transport::exchange_multi_server(
            store,
            &identity,
            &MultiServerExchangeInput {
                server_id: id,
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
        ) else {
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
        let saved = store.transaction_with(|tx| {
            let raw = tx
                .query_row(
                    "SELECT record FROM runtime_access_clients WHERE id=?1",
                    [id],
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
                || text(&client, "publicKey") != text(peer, "publicKey")
            {
                return Ok(false);
            }
            client["label"] = label.clone();
            tx.execute(
                "UPDATE runtime_access_clients SET record=?1 WHERE id=?2",
                params![client.to_string(), id],
            )?;
            Ok(true)
        });
        if matches!(saved, Ok(true)) {
            peer["label"] = label;
        }
    }
}

fn reserve_auto_pair_id(
    store: &RuntimeStore,
    peer_id: &str,
    identity: &Value,
    origin: &str,
) -> Result<String, AccessError> {
    let request_content = json!({
        "protocol": 1,
        "serverId": identity["serverId"],
        "label": identity["label"],
        "origin": origin,
        "publicKey": identity["publicKey"],
    });
    store.transaction_with(|tx| {
        let saved = tx
            .query_row(
                "SELECT record FROM runtime_access_auto_attempts WHERE id=?1",
                [peer_id],
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
    })
    .map_err(|_| unavailable("The auto-pair intent could not be saved"))
}

fn save_auto_pair(
    store: &RuntimeStore,
    peer: &Value,
    owner: &str,
    request_id: &str,
    raw: &[u8],
    reply: &Value,
    now: i64,
) -> Result<(), AccessError> {
    let id = text(peer, "serverId");
    let url = format!("{}{AUTO_PAIR_PATH}", text(peer, "origin"));
    let fingerprint = remote_transport::multi_server_request_fingerprint("POST", &url, raw)
        .map_err(|_| unavailable("The auto-pair response identity is invalid"))?;
    store.transaction_with(|tx| {
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
                [id],
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
                || text(saved, "publicKey") != text(peer, "publicKey")
                || text(saved, "origin") != text(peer, "origin"))
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
    })
    .map_err(|_| unavailable("The auto-paired server could not be registered"))?
    .then_some(())
    .ok_or_else(|| refused(403, "auto_pair_disabled", "Automatic pairing is disabled"))
}

fn mark_missing_peers_unreachable(
    store: &RuntimeStore,
    snapshot: &DiscoverySnapshot,
) -> Result<(), AccessError> {
    let origins = store
        .read_transaction(|tx| {
            let mut statement = tx.prepare("SELECT record FROM runtime_access_discovered")?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<_>, _>>()?)
        })
        .map_err(|_| unavailable("Stored discovery state is unavailable"))?;
    for raw in origins {
        let peer: Value = serde_json::from_str(&raw)
            .map_err(|_| unavailable("Stored discovery identity is invalid"))?;
        let origin = text(&peer, "origin");
        if !snapshot.has_candidate(origin) {
            mark_origin_unreachable(store, origin)?;
        }
    }
    Ok(())
}

fn mark_origin_unreachable(store: &RuntimeStore, origin: &str) -> Result<(), AccessError> {
    store
        .transaction_with(|tx| {
            let mut statement = tx.prepare(
                "SELECT id,record FROM runtime_access_discovered WHERE json_extract(record,'$.origin')=?1",
            )?;
            let rows = statement
                .query_map([origin], |row| {
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
        })
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

    #[test]
    fn auto_pair_request_identity_is_persisted_before_network_send() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).unwrap();
        let identity = json!({"serverId":"local","label":"Local","publicKey":"key"});
        let first =
            reserve_auto_pair_id(&store, "peer", &identity, "https://peer.example").unwrap();
        drop(store);
        let store = RuntimeStore::open(Some(root.path())).unwrap();
        let second =
            reserve_auto_pair_id(&store, "peer", &identity, "https://peer.example").unwrap();
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

    #[test]
    fn auto_pair_registration_uses_absolute_url_and_rechecks_setting_atomically() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).unwrap();
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
            .expect("disable auto-pair during exchange");
        let denied = save_auto_pair(
            &store,
            &peer,
            "owner@example.test",
            "auto-request",
            raw,
            &reply,
            100,
        );
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

    #[test]
    fn overlapping_discovery_returns_busy_before_probing() {
        let root = tempfile::tempdir().unwrap();
        let store = RuntimeStore::open(Some(root.path())).unwrap();
        let discovery = TailscaleServerDiscovery::with_executable(
            root.path().to_owned(),
            root.path().join("must-not-run-tailscale"),
        );
        let _guard = discovery.discovery_lock.lock().unwrap();
        let error = discovery
            .discover(
                &store,
                &json!({"serverId":"local"}),
                "https://local.example",
            )
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
