#[allow(clippy::duplicate_mod)]
#[path = "../support/prepared_python.rs"]
mod prepared_python_support;
use crate::{
    federation_delivery as delivery, federation_effects as effects, remote_transport as transport,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    process::Command,
    time::{Duration, Instant},
};
use studio_runtime_store::RuntimeStore;
use studio_runtime_store::federation_messaging::{
    FederationMessagingRepository as Repo, FederationTable as Table,
};

struct Corpus {
    root: tempfile::TempDir,
    data: Value,
}
impl Corpus {
    fn new() -> Self {
        let root = match std::env::var_os("STUDIO_RUNTIME_CORPUS_SCRATCH") {
            Some(root) => tempfile::tempdir_in(root),
            None => tempfile::tempdir(),
        }
        .expect("isolated fixture scratch directory");
        let python = prepared_python_support::prepared_python(
            &PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/federation/oracle.py");
        let result = Command::new(python)
            .arg(script)
            .arg(root.path())
            .output()
            .expect("fixture operation");
        assert!(
            result.status.success(),
            "Python oracle: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let data = serde_json::from_slice(
            &std::fs::read(root.path().join("oracle.json")).expect("fixture operation"),
        )
        .expect("fixture operation");
        Self { root, data }
    }
    fn store(&self, name: &str, reverse: bool) -> (PathBuf, RuntimeStore) {
        let root = self.root.path().join(name);
        std::fs::create_dir(&root).expect("fixture operation");
        for file in ["canvas.sqlite3", "analytics.sqlite3"] {
            std::fs::copy(
                self.root.path().join("template").join(file),
                root.join(file),
            )
            .expect("fixture operation");
        }
        let store = RuntimeStore::open(Some(&root)).expect("fixture operation");
        let seed = &self.data["seed"];
        let identity = if reverse {
            &seed["remote"]
        } else {
            &seed["local"]
        };
        let mut peer = seed["peer"].clone();
        let mut room = seed["room"].clone();
        if reverse {
            peer["stateId"] = seed["local"]["stateId"].clone();
            peer["publicKey"] = seed["local"]["publicKey"].clone();
            peer["label"] = json!("Local");
            room["peerId"] = peer["stateId"].clone();
            room["peerLabel"] = peer["label"].clone();
            room["remoteMembers"] = json!([{"id":"lead","role":"lead","name":"lead"}]);
        }
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_federation_identity VALUES('local',?1)",
                    [identity.to_string()],
                )?;
                Repo::put(tx, Table::Settings, "global", "", &json!({"enabled":true}))?;
                Repo::put(
                    tx,
                    Table::Peer,
                    peer["stateId"].as_str().expect("fixture operation"),
                    "",
                    &peer,
                )?;
                Repo::put(
                    tx,
                    Table::Room,
                    room["id"].as_str().expect("fixture operation"),
                    "",
                    &room,
                )?;
                for a in seed["agents"].as_array().expect("fixture operation") {
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        params![a["id"].as_str().expect("fixture operation"), a.to_string()],
                    )?;
                }
                Ok(())
            })
            .expect("fixture operation");
        (root, store)
    }
    fn peer(&self) -> &str {
        self.data["seed"]["peer"]["stateId"]
            .as_str()
            .expect("fixture operation")
    }
    fn room(&self) -> &str {
        self.data["seed"]["room"]["id"]
            .as_str()
            .expect("fixture operation")
    }
    fn now(&self) -> f64 {
        self.data["seed"]["now"]
            .as_f64()
            .expect("fixture operation")
    }
    fn signing_store(&self) -> RuntimeStore {
        self.store("signer", true).1
    }
}
fn rows(store: &RuntimeStore, table: &str) -> Vec<Value> {
    let db = store.read().expect("fixture operation");
    let mut stmt = db
        .prepare(&format!("SELECT record FROM {table} ORDER BY rowid"))
        .expect("fixture operation");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("fixture operation")
        .map(|r| serde_json::from_str(&r.expect("fixture operation")).expect("fixture operation"))
        .collect()
}
fn counts(store: &RuntimeStore) -> Value {
    let db = store.read().expect("fixture operation");
    let tables = [
        "runtime_federation_inbox",
        "runtime_federation_outbox",
        "runtime_federation_nonces",
        "runtime_chat_messages",
        "runtime_events",
    ];
    json!(tables.map(|t| {
        db.query_row(&format!("SELECT count(*) FROM {t}"), [], |r| {
            r.get::<_, i64>(0)
        })
        .expect("fixture operation")
    }))
}
fn effect_rows(store: &RuntimeStore) -> (Vec<Value>, Vec<Value>) {
    let db = store.read().expect("fixture operation");
    let mut stmt=db.prepare("SELECT seq,id,room,sender,text,created,deliveries FROM runtime_chat_messages ORDER BY seq").expect("fixture operation");
    let chat=stmt.query_map([],|r|Ok(json!({"seq":r.get::<_,i64>(0)?,"id":r.get::<_,String>(1)?,"room":r.get::<_,String>(2)?,"sender":r.get::<_,String>(3)?,"text":r.get::<_,String>(4)?,"created":r.get::<_,f64>(5)?,"deliveries":serde_json::from_str::<Value>(&r.get::<_,String>(6)?).expect("fixture operation")}))).expect("fixture operation").collect::<Result<Vec<_>,_>>().expect("fixture operation");
    let mut stmt = db
        .prepare("SELECT id,agent,kind,text FROM runtime_events ORDER BY rowid")
        .expect("fixture operation");
    let events=stmt.query_map([],|r|Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"text":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).expect("fixture operation")}))).expect("fixture operation").collect::<Result<Vec<_>,_>>().expect("fixture operation");
    (chat, events)
}
fn resign(envelope: &mut Value, identity: &transport::RemoteIdentity, nonce: &str) {
    envelope["nonce"] = json!(nonce);
    envelope["signature"] =
        json!(identity.sign(
            &transport::federation_message_signing_bytes(envelope).expect("fixture operation")
        ));
}

#[test]
fn federation_corpus() {
    let corpus = Corpus::new();
    for case in corpus.data["cases"].as_array().expect("fixture operation") {
        let name = case["name"].as_str().expect("fixture operation");
        let (_, store) = corpus.store(name, false);
        if let Some(prior) = case.get("room") {
            store
                .transaction(|tx| {
                    tx.execute("DELETE FROM runtime_federation_rooms", [])?;
                    if !prior.is_null() {
                        Repo::put(tx, Table::Room, corpus.room(), "", prior)?;
                    }
                    Ok(())
                })
                .expect("prior room state");
        }
        let before = counts(&store);
        let result = effects::receive(&store, corpus.peer(), &case["envelope"], corpus.now());
        if case["expected"].get("error").is_some() {
            assert!(result.is_err(), "{name} must refuse");
            assert_eq!(counts(&store), before, "{name} refusal writes nothing");
        } else {
            let receipt = result.unwrap_or_else(|e| panic!("{name}: {e}"));
            let (chat, events) = effect_rows(&store);
            let actual = json!({"receipt":receipt,"room":rows(&store,"runtime_federation_rooms")[0],"inbox":rows(&store,"runtime_federation_inbox"),"chat":chat,"events":events});
            assert_eq!(
                actual, case["expected"],
                "Python receipt/federation-room/inbox/chat/event output differs: {name}"
            );
        }
    }
    replay_and_nonce(&corpus);
    authenticated_consumer(&corpus);
    malformed_records(&corpus);
    spoofed_origin(&corpus);
    delivery_failures(&corpus);
    oversized_batches(&corpus);
    revoked_and_ordered(&corpus);
    rollback_matrix(&corpus);
    outbound_rollback(&corpus);
    lost_push_response(&corpus);
    callback_remote_message_uses_the_existing_federation_outbox(&corpus);
}

fn callback_remote_message_uses_the_existing_federation_outbox(corpus: &Corpus) {
    use crate::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationEffects, RuntimeOrchestrationEffects,
    };
    use std::sync::Arc;

    let (_, store) = corpus.store("callback-remote-message", false);
    let store = Arc::new(store);
    let args = json!({
        "target": format!("remote:{}", corpus.peer()),
        "text": "callback to an approved peer room",
        "importance": "progress",
        "progress_key": "remote-callback",
        "progress_version": 0
    });
    let link = json!({"id":"callback-link","side":"home","server":"peer-server","parent":"lead","parentEpoch":0,"workers":["lead"]});
    store
        .transaction_with(|tx| {
            let mut actor = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from("lead"),
            )?
            .expect("callback actor fixture");
            actor.extra.insert("autoWake".into(), json!(true));
            actor.extra.insert("movedTo".into(), json!("server-two"));
            actor.extra.insert(
                "remoteWorker".into(),
                json!({"server":"peer-server","link":"callback-link"}),
            );
            studio_runtime_store::AgentRepository::put(tx, &actor)?;
            tx.execute(
                "INSERT INTO runtime_server_links(id,record) VALUES('callback-link',?1)",
                [link.to_string()],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("moved callback actor and paired link fixture");
    let effects = RuntimeOrchestrationEffects::new(Arc::clone(&store));
    let envelope = json!({
        "requestId":"stable-callback-remote-message",
        "action":"message",
        "payload":{"link":"callback-link","worker":"lead","epoch":0,"parentEpoch":0,"args":args}
    });
    let queued = effects
        .execute("peer-server", &envelope)
        .expect("moved worker callback uses the federation service");
    let ExchangeOutcome::Applied(queued) = queued else {
        panic!("remote callback should be queued")
    };
    assert_eq!(queued["status"], "queued");
    assert_eq!(rows(&store, "runtime_federation_outbox").len(), 1);

    let replay = effects
        .execute("peer-server", &envelope)
        .expect("stable remote callback replays its receipt");
    assert_eq!(replay, ExchangeOutcome::Applied(queued.clone()));
    assert_eq!(rows(&store, "runtime_federation_outbox").len(), 1);

    let mut changed = envelope.clone();
    changed["payload"]["args"]["text"] = json!("changed");
    assert!(effects.execute("peer-server", &changed).is_err());
    assert_eq!(rows(&store, "runtime_federation_outbox").len(), 1);
}

fn replay_and_nonce(corpus: &Corpus) {
    let signer = corpus.signing_store();
    let identity = transport::load_federation_identity(&signer).expect("fixture operation");
    let (path, store) = corpus.store("replay", false);
    let mut envelope = corpus.data["cases"][0]["envelope"].clone();
    let first = effects::receive(&store, corpus.peer(), &envelope, corpus.now())
        .expect("fixture operation");
    let before = counts(&store);
    assert!(
        effects::receive(&store, corpus.peer(), &envelope, corpus.now()).is_err(),
        "same nonce is refused"
    );
    assert_eq!(before, counts(&store));
    drop(store);
    let store = RuntimeStore::open(Some(&path)).expect("fixture operation");
    resign(&mut envelope, &identity, "replay-transport-nonce-12345");
    assert_eq!(
        effects::receive(&store, corpus.peer(), &envelope, corpus.now())
            .expect("fixture operation"),
        first
    );
    assert_eq!(
        counts(&store),
        json!([1, 0, 2, 1, 2]),
        "one inbox/chat and one event per local member"
    );
    envelope["payload"]["text"] = json!("changed content");
    resign(&mut envelope, &identity, "conflict-transport-nonce-12345");
    let before = counts(&store);
    assert!(effects::receive(&store, corpus.peer(), &envelope, corpus.now()).is_err());
    assert_eq!(counts(&store), before);
    let pull = effects::pull(
        &store,
        corpus.peer(),
        &json!({"protocol":1,"acks":[]}),
        corpus.now(),
    )
    .expect("fixture operation");
    assert_eq!(pull["acks"].as_array().expect("fixture operation").len(), 1);
    assert_eq!(rows(&store, "runtime_federation_inbox")[0]["ack"], "sent");
}
fn authenticated_consumer(corpus: &Corpus) {
    let (_, store) = corpus.store("auth", false);
    let (_, signer) = corpus.store("auth-signer", true);
    let identity = transport::load_federation_identity(&signer).expect("fixture operation");
    let body =
        serde_json::to_vec(&json!({"protocol":1,"envelope":corpus.data["cases"][0]["envelope"]}))
            .expect("fixture operation");
    let nonce = "outer-authenticated-nonce-123";
    let action = effects::FederationAction::Message;
    let signature = identity.sign(&transport::federation_signing_bytes(
        "POST",
        action.path(),
        corpus.now() as i64,
        nonce,
        &body,
    ));
    let request = || effects::SignedRequest {
        action,
        peer_id: corpus.peer(),
        timestamp: corpus.now() as i64,
        nonce,
        signature: &signature,
        raw: &body,
    };
    let before = counts(&store);
    assert!(
        effects::handle(
            &store,
            request(),
            effects::OriginObservation::unavailable(),
            corpus.now()
        )
        .expect_err("missing trusted origin must refuse")
        .to_string()
        .contains("trusted-origin-observation")
    );
    assert_eq!(before, counts(&store));
    let response = effects::handle(
        &store,
        request(),
        effects::OriginObservation::observed(None),
        corpus.now(),
    )
    .expect("fixture operation");
    let local = &corpus.data["seed"]["local"];
    let receipt = transport::verify_federation_response(
        &serde_json::to_vec(&response).expect("fixture operation"),
        local["publicKey"].as_str().expect("fixture operation"),
        Some(local["stateId"].as_str().expect("fixture operation")),
        corpus.now() as i64,
    )
    .expect("fixture operation");
    assert_eq!(receipt["accepted"], true);
    let before_replay = counts(&store);
    assert!(
        effects::handle(
            &store,
            request(),
            effects::OriginObservation::observed(None),
            corpus.now()
        )
        .is_err(),
        "reused request nonce is refused"
    );
    assert_eq!(counts(&store), before_replay, "reused nonce writes nothing");
}

#[test]
fn federation_inbound_authentication_refusals_match_python() {
    // Match codex_federation.py:543-655: freshness, trusted peer identity,
    // signature, nonce replay, and whois ownership are checked before effects.
    let corpus = Corpus::new();
    let (_, signer) = corpus.store("auth-refusal-signer", true);
    let identity = transport::load_federation_identity(&signer).expect("fixture operation");
    let body =
        serde_json::to_vec(&json!({"protocol":1,"envelope":corpus.data["cases"][0]["envelope"]}))
            .expect("fixture operation");
    let action = effects::FederationAction::Message;
    let sign = |timestamp: i64, nonce: &str| {
        identity.sign(&transport::federation_signing_bytes(
            "POST",
            action.path(),
            timestamp,
            nonce,
            &body,
        ))
    };

    let (_, store) = corpus.store("bad-request-signature", false);
    let before = counts(&store);
    let mut bad_signature = sign(corpus.now() as i64, "bad-authenticated-nonce-12345");
    bad_signature.push('x');
    assert!(
        effects::handle(
            &store,
            effects::SignedRequest {
                action,
                peer_id: corpus.peer(),
                timestamp: corpus.now() as i64,
                nonce: "bad-authenticated-nonce-12345",
                signature: &bad_signature,
                raw: &body
            },
            effects::OriginObservation::observed(None),
            corpus.now(),
        )
        .is_err(),
        "bad signature is refused"
    );
    assert_eq!(counts(&store), before, "bad signature writes nothing");

    let (_, store) = corpus.store("stale-request-timestamp", false);
    let before = counts(&store);
    let stale = corpus.now() as i64 - 301;
    let stale_nonce = "stale-authenticated-nonce-12345";
    let stale_signature = sign(stale, stale_nonce);
    assert!(
        effects::handle(
            &store,
            effects::SignedRequest {
                action,
                peer_id: corpus.peer(),
                timestamp: stale,
                nonce: stale_nonce,
                signature: &stale_signature,
                raw: &body
            },
            effects::OriginObservation::observed(None),
            corpus.now(),
        )
        .is_err(),
        "timestamp outside the five-minute window is refused"
    );
    assert_eq!(counts(&store), before, "stale timestamp writes nothing");

    let (_, store) = corpus.store("wrong-request-peer", false);
    let before = counts(&store);
    let unknown_peer = "99999999-9999-4999-8999-999999999999";
    let wrong_peer_nonce = "wrong-peer-authenticated-nonce-123";
    let wrong_peer_signature = sign(corpus.now() as i64, wrong_peer_nonce);
    assert!(
        effects::handle(
            &store,
            effects::SignedRequest {
                action,
                peer_id: unknown_peer,
                timestamp: corpus.now() as i64,
                nonce: wrong_peer_nonce,
                signature: &wrong_peer_signature,
                raw: &body
            },
            effects::OriginObservation::observed(None),
            corpus.now(),
        )
        .is_err(),
        "unknown peer identity is refused"
    );
    assert_eq!(counts(&store), before, "unknown peer writes nothing");

    let (_, store) = corpus.store("wrong-whois-owner", false);
    store
        .transaction(|tx| {
            let mut peer = Repo::record(tx, Table::Peer, corpus.peer(), "")?.expect("peer fixture");
            peer["whoisUser"] = json!("owner@example.test");
            Repo::put(tx, Table::Peer, corpus.peer(), "", &peer)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("set trusted whois owner");
    let before = counts(&store);
    let whois_nonce = "wrong-whois-authenticated-nonce-123";
    let whois_signature = sign(corpus.now() as i64, whois_nonce);
    assert!(
        effects::handle(
            &store,
            effects::SignedRequest {
                action,
                peer_id: corpus.peer(),
                timestamp: corpus.now() as i64,
                nonce: whois_nonce,
                signature: &whois_signature,
                raw: &body
            },
            effects::OriginObservation::observed(Some("other@example.test".into())),
            corpus.now(),
        )
        .is_err(),
        "mismatched whois owner is refused"
    );
    assert_eq!(counts(&store), before, "wrong whois owner writes nothing");
}

#[test]
fn federation_pair_consumes_one_invitation_and_replays_by_peer_identity() {
    let corpus = Corpus::new();
    let (_, receiver) = corpus.store("pair-receiver", false);
    let (_, signer) = corpus.store("pair-signer", true);
    let identity = transport::load_federation_identity(&signer).expect("fixture identity");
    let peer_id = corpus.peer();
    let invite_id = "44444444-4444-4444-8444-444444444444";
    let token = "fake-peer-invitation-token";
    receiver
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runtime_federation_invites(id,record) VALUES(?1,?2)",
                params![invite_id, json!({"id":invite_id,"tokenHash":studio_runtime_store::sha256_hex(token.as_bytes()),"created":corpus.now(),"expires":corpus.now() as i64 + 900,"status":"open","expectedUser":"peer@example.test"}).to_string()],
            )?;
            Ok(())
        })
        .expect("insert disposable invitation");
    let body = json!({
        "protocol": 1,
        "inviteId": invite_id,
        "token": token,
        "stateId": peer_id,
        "label": "Remote fixture",
        "leadName": "Remote lead",
        "origin": "https://peer.fixture.ts.net",
        "publicKey": identity.public_key_pem(),
        "tailscaleUser": "peer@example.test",
        "tailscaleStatus": "verified",
    });
    let raw = transport::canonical_json(&body)
        .expect("canonical pair body")
        .into_bytes();
    let request = |nonce: &str| {
        let signature = identity.sign(&transport::federation_signing_bytes(
            "POST",
            "/api/federation/v1/pair",
            corpus.now() as i64,
            nonce,
            &raw,
        ));
        effects::handle(
            &receiver,
            effects::SignedRequest {
                action: effects::FederationAction::Pair,
                peer_id,
                timestamp: corpus.now() as i64,
                nonce,
                signature: &signature,
                raw: &raw,
            },
            effects::OriginObservation::observed(Some("peer@example.test".into())),
            corpus.now(),
        )
    };
    let first = request("pair-nonce-initial-12345").expect("pair request");
    let local = transport::load_federation_identity(&receiver).expect("local signing identity");
    let receipt = transport::verify_federation_response(
        &serde_json::to_vec(&first).expect("signed response"),
        local.public_key_pem(),
        Some(local.id()),
        corpus.now() as i64,
    )
    .expect("verify pair response");
    assert_eq!(receipt["accepted"], true);
    assert_eq!(receipt["whoisStatus"], "verified");
    assert_eq!(
        rows(&receiver, "runtime_federation_peers")[0]["stateId"],
        peer_id
    );
    assert_eq!(
        rows(&receiver, "runtime_federation_invites")[0]["status"],
        "used"
    );
    let consumed_invite = rows(&receiver, "runtime_federation_invites")[0].clone();
    assert_eq!(consumed_invite["usedBy"], peer_id);
    request("pair-nonce-retry-67890").expect("same invitation and peer are replay safe");
    assert_eq!(
        rows(&receiver, "runtime_federation_invites")[0],
        consumed_invite
    );
    assert!(request("pair-nonce-initial-12345").is_err());

    let mut other_body = body.clone();
    let other_peer = "55555555-5555-4555-8555-555555555555";
    other_body["stateId"] = json!(other_peer);
    let other_raw = transport::canonical_json(&other_body)
        .expect("canonical alternate pair body")
        .into_bytes();
    let other_nonce = "used-invite-other-peer-nonce";
    let other_signature = identity.sign(&transport::federation_signing_bytes(
        "POST",
        effects::FederationAction::Pair.path(),
        corpus.now() as i64,
        other_nonce,
        &other_raw,
    ));
    let before_used_invite = counts(&receiver);
    let peers_before = rows(&receiver, "runtime_federation_peers");
    let invites_before = rows(&receiver, "runtime_federation_invites");
    assert!(
        effects::handle(
            &receiver,
            effects::SignedRequest {
                action: effects::FederationAction::Pair,
                peer_id: other_peer,
                timestamp: corpus.now() as i64,
                nonce: other_nonce,
                signature: &other_signature,
                raw: &other_raw
            },
            effects::OriginObservation::observed(Some("peer@example.test".into())),
            corpus.now(),
        )
        .is_err(),
        "a consumed invitation cannot pair a second peer"
    );
    assert_eq!(counts(&receiver), before_used_invite);
    assert_eq!(rows(&receiver, "runtime_federation_peers"), peers_before);
    assert_eq!(
        rows(&receiver, "runtime_federation_invites"),
        invites_before
    );

    let expired_id = "66666666-6666-4666-8666-666666666666";
    let expired_token = "expired-fake-peer-invitation";
    receiver
        .transaction(|tx| {
            tx.execute(
                "INSERT INTO runtime_federation_invites(id,record) VALUES(?1,?2)",
                params![
                    expired_id,
                    json!({
                        "id": expired_id,
                        "tokenHash": studio_runtime_store::sha256_hex(expired_token.as_bytes()),
                        "created": corpus.now() - 1000.0,
                        "expires": corpus.now() as i64 - 1,
                        "status": "open",
                        "expectedUser": "peer@example.test"
                    })
                    .to_string()
                ],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .expect("insert expired invitation");
    let mut expired_body = body.clone();
    expired_body["inviteId"] = json!(expired_id);
    expired_body["token"] = json!(expired_token);
    expired_body["stateId"] = json!("77777777-7777-4777-8777-777777777777");
    let expired_raw = transport::canonical_json(&expired_body)
        .expect("canonical expired pair body")
        .into_bytes();
    let expired_nonce = "expired-invite-authenticated-nonce";
    let expired_signature = identity.sign(&transport::federation_signing_bytes(
        "POST",
        effects::FederationAction::Pair.path(),
        corpus.now() as i64,
        expired_nonce,
        &expired_raw,
    ));
    let before_expired = counts(&receiver);
    let peers_before = rows(&receiver, "runtime_federation_peers");
    let invites_before = rows(&receiver, "runtime_federation_invites");
    assert!(
        effects::handle(
            &receiver,
            effects::SignedRequest {
                action: effects::FederationAction::Pair,
                peer_id: "77777777-7777-4777-8777-777777777777",
                timestamp: corpus.now() as i64,
                nonce: expired_nonce,
                signature: &expired_signature,
                raw: &expired_raw
            },
            effects::OriginObservation::observed(Some("peer@example.test".into())),
            corpus.now(),
        )
        .is_err(),
        "expired invitation is refused"
    );
    assert_eq!(counts(&receiver), before_expired);
    assert_eq!(rows(&receiver, "runtime_federation_peers"), peers_before);
    assert_eq!(
        rows(&receiver, "runtime_federation_invites"),
        invites_before
    );
}

#[test]
fn federation_user_message_uses_the_local_lead_and_stable_idempotency_key() {
    let corpus = Corpus::new();
    let (_, store) = corpus.store("user-message", false);
    let first = delivery::user_message(
        &store,
        corpus.room(),
        "user message",
        "stable-user-message-key",
        corpus.now(),
    )
    .expect("federated user message");
    let replay = delivery::user_message(
        &store,
        corpus.room(),
        "user message",
        "stable-user-message-key",
        corpus.now() + 1.0,
    )
    .expect("idempotent federated user message");
    assert_eq!(first, replay);
    assert_eq!(rows(&store, "runtime_federation_outbox").len(), 1);
    assert!(
        delivery::user_message(
            &store,
            corpus.room(),
            "changed text",
            "stable-user-message-key",
            corpus.now(),
        )
        .is_err()
    );
}

fn malformed_records(corpus: &Corpus) {
    for (index, value) in [Value::Null, json!([]), json!({}), json!({"enabled":"true"})]
        .into_iter()
        .enumerate()
    {
        let (_, store) = corpus.store(&format!("bad-{index}"), false);
        store
            .transaction(|tx| {
                tx.execute(
                    "UPDATE runtime_federation_settings SET record=?1",
                    [value.to_string()],
                )?;
                Ok(())
            })
            .expect("fixture operation");
        let before = counts(&store);
        assert!(
            effects::receive(
                &store,
                corpus.peer(),
                &corpus.data["cases"][0]["envelope"],
                corpus.now()
            )
            .is_err()
        );
        assert_eq!(before, counts(&store));
    }
}

fn revoked_and_ordered(corpus: &Corpus) {
    let (_, store) = corpus.store("stale-order", false);
    store
        .transaction(|tx| {
            let mut room = Repo::record(tx, Table::Room, corpus.room(), "")?.expect("room exists");
            room["remoteSequence"] = json!(1);
            Repo::put(tx, Table::Room, corpus.room(), "", &room)?;
            Ok(())
        })
        .expect("advance prior room sequence");
    let before = counts(&store);
    assert!(
        effects::receive(
            &store,
            corpus.peer(),
            &corpus.data["cases"][0]["envelope"],
            corpus.now()
        )
        .is_err(),
        "equal sequence cannot deliver twice"
    );
    assert_eq!(before, counts(&store));

    for (name, column, value) in [
        ("peer-local", "localApproved", json!(false)),
        ("peer-remote", "remoteApproved", json!(false)),
        ("peer-status", "status", json!("revoked")),
    ] {
        let (_, store) = corpus.store(name, false);
        store
            .transaction(|tx| {
                let mut peer =
                    Repo::record(tx, Table::Peer, corpus.peer(), "")?.expect("peer exists");
                peer[column] = value.clone();
                Repo::put(tx, Table::Peer, corpus.peer(), "", &peer)?;
                Ok(())
            })
            .expect("revocation fixture");
        let before = counts(&store);
        assert!(
            effects::receive(
                &store,
                corpus.peer(),
                &corpus.data["cases"][0]["envelope"],
                corpus.now()
            )
            .is_err(),
            "{name}"
        );
        assert_eq!(before, counts(&store));
    }
}

fn rollback_matrix(corpus: &Corpus) {
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let tables = if full {
        vec![
            "runtime_federation_nonces",
            "runtime_chat_messages",
            "runtime_events",
            "runtime_federation_rooms",
            "runtime_rooms",
            "runtime_federation_inbox",
        ]
    } else {
        vec!["runtime_events", "runtime_federation_inbox"]
    };
    for table in tables {
        for phase in ["BEFORE", "AFTER"] {
            let (path, store) = corpus.store(&format!("rollback-{table}-{phase}"), false);
            let prior = rows(&store, "runtime_federation_rooms");
            let operation = if table == "runtime_federation_rooms" {
                "UPDATE"
            } else {
                "INSERT"
            };
            store.transaction(|tx|{tx.execute_batch(&format!("CREATE TRIGGER fault {phase} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'fixture crash'); END"))?;Ok(())}).expect("fixture operation");
            assert!(
                effects::receive(
                    &store,
                    corpus.peer(),
                    &corpus.data["cases"][0]["envelope"],
                    corpus.now()
                )
                .is_err(),
                "{phase} {table}"
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER fault")?;
                    Ok(())
                })
                .expect("fixture operation");
            drop(store);
            let store = RuntimeStore::open(Some(&path)).expect("fixture operation");
            assert_eq!(
                counts(&store),
                json!([0, 0, 0, 0, 0]),
                "{phase} {table} rolled back"
            );
            assert_eq!(rows(&store, "runtime_federation_rooms"), prior);
            effects::receive(
                &store,
                corpus.peer(),
                &corpus.data["cases"][0]["envelope"],
                corpus.now(),
            )
            .expect("fixture operation");
            assert_eq!(counts(&store), json!([1, 0, 1, 1, 2]));
        }
    }
}
struct PeerTransport<'a> {
    receiver: &'a RuntimeStore,
    sender: &'a str,
    now: f64,
    lose_once: bool,
    calls: usize,
    bad_receipt: Option<&'static str>,
    bad_response_signature: bool,
    link_advert: Option<&'static str>,
    cancel_after: Option<&'a std::sync::atomic::AtomicBool>,
    deadlines: Vec<Instant>,
}
impl delivery::FederationTransport for PeerTransport<'_> {
    fn exchange(
        &mut self,
        request: delivery::FederationExchange<'_>,
    ) -> transport::RemoteTransportResult<delivery::FederationTransportResponse> {
        let delivery::FederationExchange {
            url,
            headers,
            body,
            budget,
            ..
        } = request;
        assert!(budget.remaining().expect("active budget") <= Duration::from_secs(8));
        self.deadlines.push(budget.deadline());
        assert!(url.starts_with("https://127.0.0.1/"));
        let action = if url.ends_with("message") {
            effects::FederationAction::Message
        } else {
            effects::FederationAction::Pull
        };
        let header = |name: &str| {
            headers
                .iter()
                .find(|(k, _)| k == name)
                .expect("fixture operation")
                .1
                .as_str()
        };
        let response = effects::handle(
            self.receiver,
            effects::SignedRequest {
                action,
                peer_id: self.sender,
                timestamp: header("X-Studio-Federation-Time")
                    .parse()
                    .expect("fixture operation"),
                nonce: header("X-Studio-Federation-Nonce"),
                signature: header("X-Studio-Federation-Signature"),
                raw: body,
            },
            effects::OriginObservation::observed(None),
            self.now,
        )
        .expect("fixture operation");
        let response = if let Some(kind) = self.bad_receipt {
            let identity =
                transport::load_federation_identity(self.receiver).expect("fixture identity");
            let mut receipt = response["result"].clone();
            match kind {
                "schema" => receipt = json!({}),
                "message" => receipt["messageId"] = json!("different-message"),
                "room" => receipt["room"] = json!("different-room"),
                _ => panic!("unknown receipt fixture"),
            }
            effects::signed_response(&identity, receipt, self.now as i64)
                .expect("signed invalid receipt")
        } else {
            response
        };
        let mut response = response;
        if self.bad_response_signature {
            let signature = response["signature"]
                .as_str()
                .expect("signed response has signature");
            response["signature"] = json!(format!("{signature}x"));
        }
        self.calls += 1;
        if let Some(cancelled) = self.cancel_after {
            cancelled.store(true, std::sync::atomic::Ordering::Release);
        }
        if self.lose_once {
            self.lose_once = false;
            return Err(transport::RemoteTransportError::ExchangeUnknown);
        }
        Ok(delivery::FederationTransportResponse {
            body: serde_json::to_vec(&response).expect("fixture operation"),
            link_advert: self.link_advert.map(str::to_owned),
        })
    }
}
fn lost_push_response(corpus: &Corpus) {
    let (sender_path, sender) = corpus.store("sender", false);
    let (_, receiver) = corpus.store("receiver", true);
    let key = "stable-message-key";
    let result = sender
        .transaction_with(|tx| {
            delivery::send_message_in_transaction(
                tx,
                "lead",
                corpus.room(),
                "message to peer",
                key,
                corpus.now(),
            )
        })
        .expect("fixture operation");
    assert_eq!(
        result,
        delivery::send_message(
            &sender,
            "lead",
            corpus.room(),
            "message to peer",
            key,
            corpus.now() + 1.0
        )
        .expect("fixture operation")
    );
    assert!(
        delivery::send_message(&sender, "lead", corpus.room(), "changed", key, corpus.now())
            .is_err()
    );
    let local = corpus.data["seed"]["local"]["stateId"]
        .as_str()
        .expect("fixture operation");
    let mut wire = PeerTransport {
        receiver: &receiver,
        sender: local,
        now: corpus.now(),
        lose_once: true,
        calls: 0,
        bad_receipt: None,
        bad_response_signature: false,
        link_advert: None,
        cancel_after: None,
        deadlines: Vec::new(),
    };
    assert!(
        delivery::pump_peer(
            &sender,
            corpus.peer(),
            corpus.now(),
            &mut wire,
            &std::sync::atomic::AtomicBool::new(false)
        )
        .is_err()
    );
    assert_eq!(counts(&receiver), json!([1, 0, 2, 1, 2]));
    drop(sender);
    let sender = RuntimeStore::open(Some(&sender_path)).expect("fixture operation");
    wire.now += 2.0;
    assert_eq!(
        delivery::pump_peer(
            &sender,
            corpus.peer(),
            wire.now,
            &mut wire,
            &std::sync::atomic::AtomicBool::new(false)
        )
        .expect("fixture operation"),
        1
    );
    assert_eq!(
        rows(&sender, "runtime_federation_outbox")[0]["status"],
        "delivered"
    );
    assert_eq!(effect_rows(&receiver).0.len(), 1);
    assert_eq!(effect_rows(&receiver).1.len(), 2);
    assert_eq!(wire.calls, 3, "two pushes and one pull");
    assert_eq!(
        wire.deadlines[1], wire.deadlines[2],
        "push and pull share one absolute deadline"
    );
    let mut reverse = PeerTransport {
        receiver: &sender,
        sender: corpus.peer(),
        now: wire.now,
        lose_once: false,
        calls: 0,
        bad_receipt: None,
        bad_response_signature: false,
        link_advert: None,
        cancel_after: None,
        deadlines: Vec::new(),
    };
    assert_eq!(
        delivery::pump_peer(
            &receiver,
            local,
            reverse.now,
            &mut reverse,
            &std::sync::atomic::AtomicBool::new(false)
        )
        .expect("acknowledge receiver inbox"),
        0
    );
    assert_eq!(
        rows(&receiver, "runtime_federation_inbox")[0]["ack"],
        "confirmed"
    );
}

#[cfg(unix)]
#[test]
fn bad_federation_response_signature_stays_unknown_and_does_not_learn_route() {
    use std::os::unix::net::UnixListener;

    let corpus = Corpus::new();
    // Keep the Unix socket path below macOS's sockaddr_un path limit even
    // when the system temp directory has a long per-user prefix.
    let (_, sender) = corpus.store("s", false);
    let (_, receiver) = corpus.store("bad-response-signature-receiver", true);
    delivery::send_message(
        &sender,
        "lead",
        corpus.room(),
        "message with an unverified response",
        "bad-response-signature-key",
        corpus.now(),
    )
    .expect("queue message");

    let socket_path = sender.root().join("multi-server").join("peer-link.sock");
    std::fs::create_dir_all(socket_path.parent().expect("socket parent"))
        .expect("create socket directory");
    let listener = UnixListener::bind(&socket_path).expect("bind peer-link fixture");
    listener
        .set_nonblocking(true)
        .expect("nonblocking listener");

    let local = corpus.data["seed"]["local"]["stateId"]
        .as_str()
        .expect("local identity");
    let mut fake = PeerTransport {
        receiver: &receiver,
        sender: local,
        now: corpus.now(),
        lose_once: false,
        calls: 0,
        bad_receipt: None,
        bad_response_signature: true,
        link_advert: Some("100.80.1.2:443"),
        cancel_after: None,
        deadlines: Vec::new(),
    };
    assert!(
        delivery::pump_peer(
            &sender,
            corpus.peer(),
            corpus.now(),
            &mut fake,
            &std::sync::atomic::AtomicBool::new(false),
        )
        .is_err()
    );
    assert_eq!(fake.calls, 1);

    let queued = rows(&sender, "runtime_federation_outbox");
    assert_eq!(queued.len(), 1);
    assert_eq!(queued[0]["status"], "queued");
    assert_eq!(queued[0]["attempts"], 1);
    assert_eq!(
        queued[0]["error"],
        "Federation transport did not return a verified receipt"
    );
    assert_eq!(rows(&receiver, "runtime_federation_inbox").len(), 1);

    let peer = &corpus.data["seed"]["peer"];
    let client = transport::PeerLinkClient::new(sender.root());
    assert!(
        client
            .exchange(transport::PeerLinkExchange {
                server: corpus.peer(),
                public_key: peer["publicKey"].as_str().expect("peer public key"),
                method: "GET",
                path: "/",
                headers: &[],
                body: &[],
                timeout: Duration::from_millis(10),
            })
            .expect("no route learned from unverified response")
            .is_none()
    );
    assert!(matches!(
        listener.accept(),
        Err(error) if error.kind() == std::io::ErrorKind::WouldBlock
    ));
}

fn outbound_rollback(corpus: &Corpus) {
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    let tables = if full {
        vec![
            ("runtime_chat_messages", "INSERT"),
            ("runtime_events", "INSERT"),
            ("runtime_federation_rooms", "UPDATE"),
            ("runtime_federation_outbox", "INSERT"),
        ]
    } else {
        vec![("runtime_federation_outbox", "INSERT")]
    };
    for (table, operation) in tables {
        for phase in ["BEFORE", "AFTER"] {
            let (path, store) = corpus.store(&format!("send-fault-{table}-{phase}"), false);
            let prior = rows(&store, "runtime_federation_rooms");
            store.transaction(|tx| {tx.execute_batch(&format!("CREATE TRIGGER fault {phase} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'fixture crash'); END"))?;Ok(())}).expect("sender failpoint");
            assert!(
                delivery::send_message(
                    &store,
                    "lead",
                    corpus.room(),
                    "message",
                    "key",
                    corpus.now()
                )
                .is_err(),
                "{table} {phase}"
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER fault")?;
                    Ok(())
                })
                .expect("remove failpoint");
            drop(store);
            let store = RuntimeStore::open(Some(&path)).expect("sender restarts");
            assert_eq!(counts(&store), json!([0, 0, 0, 0, 0]));
            assert_eq!(rows(&store, "runtime_federation_rooms"), prior);
            delivery::send_message(
                &store,
                "lead",
                corpus.room(),
                "message",
                "key",
                corpus.now(),
            )
            .expect("queue after restart");
            assert_eq!(counts(&store), json!([0, 1, 0, 1, 1]));
        }
    }
}

fn spoofed_origin(corpus: &Corpus) {
    let (_, store) = corpus.store("spoofed-origin", false);
    store
        .transaction(|tx| {
            let mut peer = Repo::record(tx, Table::Peer, corpus.peer(), "")?.expect("peer");
            peer["whoisUser"] = json!("trusted-owner");
            Repo::put(tx, Table::Peer, corpus.peer(), "", &peer)
        })
        .expect("bind trusted origin");
    let raw = serde_json::to_vec(&json!({"protocol":1})).expect("body");
    let before = counts(&store);
    let request = || effects::SignedRequest {
        action: effects::FederationAction::Status,
        peer_id: corpus.peer(),
        timestamp: corpus.now() as i64,
        nonce: "spoofed-origin-nonce-12345",
        signature: "",
        raw: &raw,
    };
    assert!(
        effects::handle(
            &store,
            request(),
            effects::OriginObservation::observed(Some("spoofed-header".into())),
            corpus.now()
        )
        .expect_err("spoofed origin")
        .to_string()
        .contains("origin identity mismatch")
    );
    assert!(
        effects::handle(
            &store,
            request(),
            effects::OriginObservation::unavailable(),
            corpus.now()
        )
        .expect_err("no trusted adapter")
        .to_string()
        .contains("trusted-origin-observation")
    );
    assert_eq!(counts(&store), before);
}

fn delivery_failures(corpus: &Corpus) {
    for kind in ["schema", "message", "room", "cancel", "expired"] {
        let (_, sender) = corpus.store(&format!("failure-sender-{kind}"), false);
        let (_, receiver) = corpus.store(&format!("failure-receiver-{kind}"), true);
        for key in ["one", "two"] {
            delivery::send_message(&sender, "lead", corpus.room(), "message", key, corpus.now())
                .expect("queue");
        }
        let cancelled = std::sync::atomic::AtomicBool::new(false);
        let mut wire = PeerTransport {
            receiver: &receiver,
            sender: corpus.data["seed"]["local"]["stateId"]
                .as_str()
                .expect("local id"),
            now: corpus.now(),
            lose_once: false,
            calls: 0,
            bad_receipt: if matches!(kind, "schema" | "message" | "room") {
                Some(kind)
            } else {
                None
            },
            bad_response_signature: false,
            link_advert: None,
            cancel_after: if kind == "cancel" {
                Some(&cancelled)
            } else {
                None
            },
            deadlines: Vec::new(),
        };
        if kind == "expired" {
            let before = counts(&sender);
            assert_eq!(
                delivery::pump_peer_until(
                    &sender,
                    corpus.peer(),
                    corpus.now(),
                    &mut wire,
                    delivery::PassBudget::new(Instant::now(), &cancelled)
                )
                .expect("expired pass"),
                0
            );
            assert_eq!(wire.calls, 0);
            assert_eq!(counts(&sender), before);
            continue;
        }
        assert!(
            delivery::pump_peer(&sender, corpus.peer(), corpus.now(), &mut wire, &cancelled)
                .is_err()
        );
        assert_eq!(wire.calls, 1, "pass stops on failure or cancellation");
        let queued = rows(&sender, "runtime_federation_outbox");
        assert_eq!(
            queued
                .iter()
                .filter(|v| v["attempts"] == 1 && v["nextAt"] == corpus.now() + 2.0)
                .count(),
            1,
            "all receipt errors persist backoff"
        );
        assert!(queued.iter().all(|v| v["status"] == "queued"));
    }
}

fn oversized_batches(corpus: &Corpus) {
    for table in [Table::Outbox, Table::Inbox] {
        let (_, store) = corpus.store(&format!("oversized-{}", table.name()), false);
        let raw=json!({"status":"queued","nextAt":0,"created":0,"ack":"pending","padding":"x".repeat(512*1024+1)}).to_string();
        store.transaction(|tx| {
            match table {
                Table::Outbox => tx.execute("INSERT INTO runtime_federation_outbox(id,peer_id,record) VALUES('large',?1,?2)",params![corpus.peer(),raw])?,
                Table::Inbox => tx.execute("INSERT INTO runtime_federation_inbox(peer_id,message_id,record) VALUES(?1,'large',?2)",params![corpus.peer(),raw])?,
                _ => unreachable!(),
            };
            let error=Repo::batch(tx,table,corpus.peer(),corpus.now()).expect_err("oversized row refused by SQL projection");
            assert!(matches!(error,studio_runtime_store::StoreError::InvalidStoredObject {..}));
            Ok(())
        }).expect("batch size fixture");
    }
}
