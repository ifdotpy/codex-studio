use super::*;
use std::{
    io::{Read, Write},
    net::{SocketAddr, TcpListener, TcpStream},
    sync::{Mutex as StdMutex, atomic::AtomicBool},
};

struct Memory {
    value: StdMutex<Value>,
    writes: AtomicUsize,
    reject: AtomicBool,
}
impl Memory {
    fn empty() -> Arc<Self> {
        Arc::new(Self {
            value: StdMutex::new(json!({"drafts":{},"servers":{}})),
            writes: AtomicUsize::new(0),
            reject: AtomicBool::new(false),
        })
    }
}
impl Vault for Memory {
    fn read(&self) -> Result<Value, String> {
        Ok(self.value.lock().unwrap().clone())
    }
    fn write(&self, value: &Value) -> Result<(), String> {
        if self.reject.load(Ordering::SeqCst) {
            return Err("Synthetic vault write refusal.".into());
        }
        *self.value.lock().unwrap() = value.clone();
        self.writes.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
fn invitation(server: &str, origin: &str) -> Value {
    json!({"protocol":1,"origin":origin,"serverId":server,"inviteId":"synthetic-invite",
        "token":"synthetic-token","publicKey":"synthetic-public","tailscaleUser":"fixture-user"})
}
fn imported(server: &str) -> Value {
    let origin = format!("https://{server}.tailnet.ts.net");
    let mut credential =
        server_keys::new_pair(&invitation(server, &origin), "imported-request").unwrap();
    credential.as_object_mut().unwrap().remove("body");
    json!({"drafts":{},"servers":{server:credential}})
}

struct Wire {
    address: SocketAddr,
    origin: String,
    client: Arc<RemoteClient>,
    stop: Arc<AtomicBool>,
    trace: Arc<StdMutex<Vec<(String, Value)>>>,
    worker: Option<std::thread::JoinHandle<Result<(), String>>>,
}
impl Wire {
    fn new(vault: Arc<Memory>, before_reply: impl FnOnce() + Send + 'static) -> Self {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cert =
            rcgen::generate_simple_self_signed(vec!["registry.tailnet.ts.net".into()]).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let origin = format!("https://registry.tailnet.ts.net:{}", address.port());
        let root = reqwest::Certificate::from_der(cert.cert.der().as_ref()).unwrap();
        let configuration = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::ring::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![cert.cert.der().clone()],
            rustls::pki_types::PrivateKeyDer::Pkcs8(cert.signing_key.serialize_der().into()),
        )
        .unwrap();
        let stop = Arc::new(AtomicBool::new(false));
        let stopping = stop.clone();
        let trace = Arc::new(StdMutex::new(vec![]));
        let recorded = trace.clone();
        let reply_origin = origin.clone();
        let worker = std::thread::spawn(move || -> Result<(), String> {
            let (socket, _) = listener.accept().map_err(|e| e.to_string())?;
            if stopping.load(Ordering::SeqCst) {
                return Ok(());
            }
            socket
                .set_read_timeout(Some(Duration::from_secs(5)))
                .map_err(|e| e.to_string())?;
            socket
                .set_write_timeout(Some(Duration::from_secs(5)))
                .map_err(|e| e.to_string())?;
            let mut stream = rustls::StreamOwned::new(
                rustls::ServerConnection::new(Arc::new(configuration)).unwrap(),
                socket,
            );
            let mut header = vec![];
            while !header.ends_with(b"\r\n\r\n") {
                if header.len() > 64 * 1024 {
                    return Err("Synthetic header exceeds its bound.".into());
                }
                let mut byte = [0];
                stream.read_exact(&mut byte).map_err(|e| e.to_string())?;
                header.push(byte[0]);
            }
            let header = String::from_utf8(header).unwrap();
            let length: usize = header
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .map(|value| value.trim().parse().unwrap())
                })
                .unwrap();
            if length > 1024 * 1024 {
                return Err("Synthetic body exceeds its bound.".into());
            }
            let mut body = vec![0; length];
            stream.read_exact(&mut body).map_err(|e| e.to_string())?;
            let body: Value = serde_json::from_slice(&body).unwrap();
            recorded.lock().unwrap().push((header, body.clone()));
            before_reply();
            let reply = json!({"protocol":1,"paired":true,"serverId":"paired","clientId":body["clientId"],"origin":reply_origin,
                "publicKey":"synthetic-public","tailscaleUser":"fixture-user","label":"Fixture peer"}).to_string();
            write!(stream,"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",reply.len(),reply).map_err(|e|e.to_string())?;
            stream.flush().map_err(|e| e.to_string())
        });
        let mut client = RemoteClient::new(vault).unwrap();
        client.http = reqwest::Client::builder()
            .no_proxy()
            .resolve("registry.tailnet.ts.net", address)
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .tls_certs_only([root])
            .build()
            .unwrap();
        Self {
            address,
            origin,
            client: Arc::new(client),
            stop,
            trace,
            worker: Some(worker),
        }
    }
    fn params(&self) -> Value {
        json!({"origin":self.origin,"invitation":invitation("paired",&self.origin),"requestId":"exact-pair-request"})
    }
    fn close(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.address); // Wake only this fixture's accept.
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}
impl Drop for Wire {
    fn drop(&mut self) {
        self.close();
    }
}

#[tokio::test]
async fn native_registry_actual_pair_uses_only_the_internal_invitation_endpoint_and_replays_receipt()
 {
    let vault = Memory::empty();
    let mut wire = Wire::new(vault.clone(), || {});
    let params = wire.params();
    let first = wire.client.dispatch("servers.pair", &params).await;
    wire.close();
    assert!(
        first.is_ok(),
        "A validated invitation must reach its real pair endpoint: {first:?}"
    );
    let first = first.unwrap();
    assert_eq!(wire.trace.lock().unwrap().len(), 1);
    let trace = wire.trace.lock().unwrap();
    assert!(
        trace[0]
            .0
            .starts_with("POST /api/multi-server/v1/pair HTTP/1.1\r\n")
    );
    assert!(
        trace[0]
            .0
            .to_ascii_lowercase()
            .contains("x-studio-request-id: exact-pair-request\r\n")
    );
    assert_eq!(trace[0].1["requestId"], "exact-pair-request");
    drop(trace);
    assert_eq!(
        wire.client.dispatch("servers.pair", &params).await.unwrap(),
        first
    );
    assert_eq!(
        wire.trace.lock().unwrap().len(),
        1,
        "Receipt replay sends no second pair request."
    );
    let mut changed = params.clone();
    changed["invitation"]["token"] = json!("different-token");
    assert!(
        wire.client
            .dispatch("servers.pair", &changed)
            .await
            .is_err()
    );
    wire.client
        .dispatch("servers.forget", &json!({"serverId":"paired"}))
        .await
        .unwrap();
    assert!(wire.client.dispatch("servers.pair", &params).await.is_err());
    assert!(vault.read().unwrap()["servers"]["paired"].is_null());
    assert_eq!(wire.trace.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn native_registry_public_transport_keeps_management_denied_and_pair_scope_exact() {
    let vault = Memory::empty();
    let wire = Wire::new(vault.clone(), || {});
    for (method, path) in [
        ("POST", "/api/multi-server/v1/pair"),
        ("GET", "/api/multi-server/v1/pair"),
        ("POST", "/api/multi-server"),
    ] {
        let credential =
            server_keys::new_pair(&invitation("paired", &wire.origin), "public-denial").unwrap();
        assert!(
            wire.client
                .signed(
                    &credential,
                    method,
                    &format!("{}{path}", wire.origin),
                    &json!([]),
                    vec![],
                    "public-denial"
                )
                .await
                .is_err()
        );
    }
    let mut wrong = wire.params();
    wrong["origin"] = json!("https://other.tailnet.ts.net");
    assert!(wire.client.dispatch("servers.pair", &wrong).await.is_err());
    assert_eq!(vault.writes.load(Ordering::SeqCst), 0);
    assert!(wire.trace.lock().unwrap().is_empty());
}

#[tokio::test]
async fn native_registry_invalid_pair_request_ids_create_no_draft_or_network_write() {
    for id in [
        "".to_owned(),
        "x".repeat(129),
        "line\nrequest".to_owned(),
        "nul\0request".to_owned(),
    ] {
        let vault = Memory::empty();
        let wire = Wire::new(vault.clone(), || {});
        let mut params = wire.params();
        params["requestId"] = json!(id);
        assert!(wire.client.dispatch("servers.pair", &params).await.is_err());
        assert_eq!(
            vault.writes.load(Ordering::SeqCst),
            0,
            "An invalid request identity cannot reserve a new key."
        );
        assert!(wire.trace.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn native_registry_pair_commit_preserves_an_import_committed_during_its_wire_wait() {
    let vault = Memory::empty();
    let concurrent = vault.clone();
    let mut wire = Wire::new(vault.clone(), move || {
        server_keys::import(concurrent.as_ref(), &imported("other")).unwrap();
    });
    let outcome = wire.client.dispatch("servers.pair", &wire.params()).await;
    wire.close();
    assert!(
        outcome.is_ok(),
        "The real pair must finish before its authority merge is checked: {outcome:?}"
    );
    let state = vault.read().unwrap();
    assert!(state["servers"]["paired"].is_object());
    assert!(
        state["servers"]["other"].is_object(),
        "Pair cannot overwrite authority imported while its reply was pending."
    );
    assert_eq!(
        state[server_keys::registry_contract()["revision"]["vaultField"]
            .as_str()
            .unwrap()],
        "2"
    );
}

#[tokio::test]
async fn native_registry_snapshot_has_only_public_authority_and_a_decimal_revision() {
    let vault = Memory::empty();
    let source = imported("peer");
    let private = source["servers"]["peer"]["privateKey"]
        .as_str()
        .unwrap()
        .to_owned();
    server_keys::import(vault.as_ref(), &source).unwrap();
    let client = RemoteClient::new(vault).unwrap();
    let snapshot = client.registry_snapshot().unwrap();
    assert_eq!(snapshot["revision"], "1");
    let row = snapshot["servers"][0].as_object().unwrap();
    assert_eq!(
        row.keys().map(String::as_str).collect::<Vec<_>>(),
        vec!["credentialId", "id", "label", "origin"]
    );
    assert_eq!(row["credentialId"], source["servers"]["peer"]["clientId"]);
    let serialized = snapshot.to_string();
    for forbidden in [
        private.as_str(),
        "privateKey",
        "publicKey",
        "imports",
        "storageKey",
        "signature",
        "token",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "Public registry metadata cannot expose {forbidden}."
        );
    }
}

#[tokio::test]
async fn native_registry_forget_commits_one_revision_and_notifies_without_polling() {
    let vault = Memory::empty();
    server_keys::import(vault.as_ref(), &imported("peer")).unwrap();
    let client = RemoteClient::new(vault.clone()).unwrap();
    assert_eq!(client.registry_snapshot().unwrap()["revision"], "1");
    let mut changes = client.registry_changes();
    changes.borrow_and_update();
    client
        .dispatch("servers.forget", &json!({"serverId":"peer"}))
        .await
        .unwrap();
    assert!(
        changes.has_changed().unwrap(),
        "The committed forget must emit a native change."
    );
    assert_eq!(*changes.borrow_and_update(), "2");
    assert_eq!(
        client.registry_snapshot().unwrap(),
        json!({"revision":"2","servers":[]})
    );
    client
        .dispatch("servers.forget", &json!({"serverId":"peer"}))
        .await
        .unwrap();
    assert!(
        !changes.has_changed().unwrap(),
        "A no-op forget cannot manufacture another authority revision."
    );
    drop(client);
    assert!(
        changes.changed().await.is_err(),
        "Dropping the sole owner closes its event source."
    );
}

#[tokio::test]
async fn native_registry_refused_vault_commit_emits_no_success_or_revision() {
    let vault = Memory::empty();
    server_keys::import(vault.as_ref(), &imported("peer")).unwrap();
    let client = RemoteClient::new(vault.clone()).unwrap();
    client.registry_snapshot().unwrap();
    let mut changes = client.registry_changes();
    changes.borrow_and_update();
    let before = vault.read().unwrap();
    vault.reject.store(true, Ordering::SeqCst);
    assert!(
        client
            .dispatch("servers.forget", &json!({"serverId":"peer"}))
            .await
            .is_err()
    );
    assert_eq!(vault.read().unwrap(), before);
    assert!(!changes.has_changed().unwrap());
}

#[test]
fn native_registry_import_revision_is_atomic_idempotent_and_preserves_later_authority() {
    let vault = Memory::empty();
    let first = imported("first");
    server_keys::import(vault.as_ref(), &first).unwrap();
    let field = server_keys::registry_contract()["revision"]["vaultField"]
        .as_str()
        .unwrap();
    assert_eq!(vault.read().unwrap()[field], "1");
    server_keys::import(vault.as_ref(), &imported("second")).unwrap();
    server_keys::import(vault.as_ref(), &first).unwrap();
    let state = vault.read().unwrap();
    assert_eq!(state[field], "2");
    assert_eq!(state["servers"].as_object().unwrap().len(), 2);
    let mut conflict = first.clone();
    conflict["servers"]["first"]["clientId"] = json!("foreign-client");
    assert!(server_keys::import(vault.as_ref(), &conflict).is_err());
    assert_eq!(vault.read().unwrap(), state);
}

#[tokio::test]
async fn native_registry_import_notifies_the_existing_owner_and_keeps_other_vaults_private() {
    let vault = Memory::empty();
    let client = RemoteClient::new(vault.clone()).unwrap();
    let other = RemoteClient::new(Memory::empty()).unwrap();
    let mut changes = client.registry_changes();
    changes.borrow_and_update();
    let mut foreign = other.registry_changes();
    foreign.borrow_and_update();
    server_keys::import(vault.as_ref(), &imported("peer")).unwrap();
    assert!(changes.has_changed().unwrap());
    assert_eq!(*changes.borrow_and_update(), "1");
    assert!(!foreign.has_changed().unwrap());
    assert_eq!(
        client.registry_snapshot().unwrap()["servers"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[test]
fn native_registry_concurrent_imports_keep_both_commits_and_revision_bounds() {
    let vault = Memory::empty();
    let first = imported("first");
    let second = imported("second");
    let start = Arc::new(std::sync::Barrier::new(3));
    let threads = [first, second].map(|source| {
        let vault = vault.clone();
        let start = start.clone();
        std::thread::spawn(move || {
            start.wait();
            server_keys::import(vault.as_ref(), &source).unwrap();
        })
    });
    start.wait();
    for thread in threads {
        thread.join().unwrap();
    }
    assert_eq!(vault.read().unwrap()["registryRevision"], "2");
    assert_eq!(
        vault.read().unwrap()["servers"].as_object().unwrap().len(),
        2
    );
    vault.value.lock().unwrap()["registryRevision"] = json!("9223372036854775807");
    let before = vault.read().unwrap();
    assert!(server_keys::import(vault.as_ref(), &imported("third")).is_err());
    assert_eq!(vault.read().unwrap(), before);
    vault.value.lock().unwrap()["registryRevision"] = json!("01");
    assert!(RemoteClient::new(vault).is_err());
}

#[tokio::test]
async fn native_registry_pending_pair_cannot_restore_a_forgotten_or_replaced_identity() {
    for forget in [true, false] {
        let vault = Memory::empty();
        let concurrent = vault.clone();
        let mut wire = Wire::new(vault.clone(), move || {
            if forget {
                let client = RemoteClient::new(concurrent).unwrap();
                tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .unwrap()
                    .block_on(client.dispatch("servers.forget", &json!({"serverId":"paired"})))
                    .unwrap();
            } else {
                server_keys::import(concurrent.as_ref(), &imported("paired")).unwrap();
            }
        });
        assert!(
            wire.client
                .dispatch("servers.pair", &wire.params())
                .await
                .is_err()
        );
        wire.close();
        let state = vault.read().unwrap();
        if forget {
            assert!(state["servers"]["paired"].is_null());
        } else {
            assert_eq!(
                state["servers"]["paired"]["origin"],
                "https://paired.tailnet.ts.net"
            );
        }
        assert_eq!(wire.trace.lock().unwrap().len(), 1);
    }
}

#[tokio::test]
async fn native_registry_lost_pair_commit_keeps_exact_private_draft_without_automatic_resend() {
    let vault = Memory::empty();
    let refusing = vault.clone();
    let mut wire = Wire::new(vault.clone(), move || {
        refusing.reject.store(true, Ordering::SeqCst);
    });
    let params = wire.params();
    assert!(wire.client.dispatch("servers.pair", &params).await.is_err());
    wire.close();
    let saved = vault.read().unwrap();
    let draft = &saved["drafts"]["exact-pair-request"];
    assert!(draft["receipt"].is_null());
    let body: Value = serde_json::from_str(draft["body"].as_str().unwrap()).unwrap();
    assert_eq!(body["requestId"], "exact-pair-request");
    assert_eq!(body["clientId"], draft["clientId"]);
    assert!(saved["servers"]["paired"].is_null());
    assert_eq!(wire.trace.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn native_registry_new_archive_cannot_restore_a_forgotten_pairing() {
    let vault = Memory::empty();
    let source = imported("peer");
    server_keys::import(vault.as_ref(), &source).unwrap();
    let client = RemoteClient::new(vault.clone()).unwrap();
    client
        .dispatch("servers.forget", &json!({"serverId":"peer"}))
        .await
        .unwrap();
    let before = vault.read().unwrap();
    server_keys::import(vault.as_ref(), &source).unwrap(); // Original receipt, no authority write.
    let mut later = source.clone();
    later["servers"]["other"] = imported("other")["servers"]["other"].clone();
    assert!(server_keys::import(vault.as_ref(), &later).is_err());
    assert_eq!(vault.read().unwrap(), before);
}

#[tokio::test]
async fn native_registry_legacy_unknown_pair_keeps_its_original_key_and_sends_nothing() {
    let vault = Memory::empty();
    let wire = Wire::new(vault.clone(), || {});
    let params = wire.params();
    let draft = server_keys::new_pair(&params["invitation"], "exact-pair-request").unwrap();
    vault.value.lock().unwrap()["drafts"]["exact-pair-request"] = draft;
    let before = vault.read().unwrap();
    assert!(wire.client.dispatch("servers.pair", &params).await.is_err());
    assert_eq!(vault.read().unwrap(), before);
    assert!(wire.trace.lock().unwrap().is_empty());
}
