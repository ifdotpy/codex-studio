use crate::{
    federation_delivery as delivery, federation_effects as effects, remote_transport as transport,
};
use rusqlite::params;
use serde_json::{Value, json};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};
use studio_runtime_store::RuntimeStore;
use studio_runtime_store::federation_messaging::{
    FederationMessagingRepository as Repo, FederationTable as Table,
};

const NOW: f64 = 1_700_000_000.0;
const LOCAL: &str = "11111111-1111-4111-8111-111111111111";
const PEER: &str = "22222222-2222-4222-8222-222222222222";
const ROOM: &str = "federated:33333333-3333-4333-8333-333333333333";

struct Corpus {
    root: tempfile::TempDir,
    data: Value,
}
impl Corpus {
    async fn new() -> Self {
        let root = match std::env::var_os("STUDIO_RUNTIME_CORPUS_SCRATCH") {
            Some(root) => tempfile::tempdir_in(root),
            None => tempfile::tempdir(),
        }
        .expect("isolated fixture scratch directory");
        let local = json!({"stateId":LOCAL,"label":"Local",
            "privateKey":"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIEwJtsrcWZOtVmTk9vLgk3Zl+8Q58joMUuCCp/nGwy/x\n-----END PRIVATE KEY-----\n",
            "publicKey":"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEA04iOMwqCMPTabJroC0ZiAMqMk1I8hnJ3fDGB9jM+n9Q=\n-----END PUBLIC KEY-----\n"});
        let remote = json!({"stateId":PEER,"label":"Peer",
            "privateKey":"-----BEGIN PRIVATE KEY-----\nMC4CAQAwBQYDK2VwBCIEIIqXlM698JXMmwsvhwSaoWuttKfmIUFYBr3RoCsa4eTc\n-----END PRIVATE KEY-----\n",
            "publicKey":"-----BEGIN PUBLIC KEY-----\nMCowBQYDK2VwAyEAGmnFFqggqjGtTZG67eDJH3j+m/LrPuPrNSLJyXG83Es=\n-----END PUBLIC KEY-----\n"});
        let peer = json!({"stateId":PEER,"label":"Peer","origin":"https://127.0.0.1",
            "publicKey":remote["publicKey"],"status":"approved","localApproved":true,
            "remoteApproved":true,"whoisUser":null});
        let agents: Vec<Value> = ["lead", "helper"]
            .iter()
            .map(|name| {
                json!({"id":name,"rootId":"lead","epoch":0,"status":"idle","name":name,
                "autoWake":false,"isLead":*name == "lead"})
            })
            .collect();
        let room = json!({"created":NOW,"id":ROOM,"kind":"federated","localApproved":true,
            "localMembers":["helper","lead"],
            "localParticipants":[{"id":"helper","name":"helper","role":"agent"},{"id":"lead","name":"lead","role":"lead"}],
            "name":"Peer \u{b7} Shared room","peerId":PEER,"peerLabel":"Peer","remoteApproved":true,
            "remoteMembers":[{"id":"remote-lead","role":"lead","name":"Remote"}],
            "remoteSequence":0,"sequence":0,"shareNames":true,"shareStatus":false,
            "status":"approved","updated":NOW});
        let mut corpus = Self {
            root,
            data: json!({"seed":{"local":local,"remote":remote,"peer":peer,"agents":agents,
                "room":room,"now":NOW},"cases":[]}),
        };
        let signer = corpus.signing_store().await;
        let identity = transport::load_federation_identity(&signer)
            .await
            .expect("remote signing identity");
        let mut cases = vec![];
        let chat = |name: &str, fields: Value| {
            let mut payload = json!({"sender":"remote-lead","senderName":"Remote","text":"hello",
                "created":NOW,"sequence":1});
            for (key, value) in fields.as_object().expect("fields") {
                payload[key] = value.clone();
            }
            let mut envelope = json!({"protocol":1,"senderServer":PEER,"id":format!("message-{name}"),
                "kind":"chat-message","room":ROOM,"payload":payload,"created":NOW,
                "timestamp":NOW as i64,"nonce":format!("unique-nonce-{name}-12345")});
            envelope["signature"] = json!(identity.sign(
                &transport::federation_message_signing_bytes(&envelope).expect("signing bytes")
            ));
            json!({"name":name,"envelope":envelope})
        };
        let long = "a".repeat(12001);
        for (name, fields) in [
            ("chat", json!({})),
            (
                "unicode",
                json!({"text":"Za\u{17c}\u{f3}\u{142}\u{107} \u{1f408}"}),
            ),
            ("no-name", json!({"senderName":null})),
            ("wrong-name", json!({"senderName":"spoof"})),
            ("wrong-sender", json!({"sender":"other"})),
            ("ordering-zero", json!({"sequence":0})),
            ("empty-text", json!({"text":""})),
            ("timestamp-zero", json!({"created":0})),
            ("large-text", json!({"text":long})),
        ] {
            cases.push(chat(name, fields));
        }
        for kind in ["room-invite", "room-accept"] {
            let prior = if kind == "room-invite" {
                Value::Null
            } else {
                let mut prior = corpus.data["seed"]["room"].clone();
                prior["status"] = json!("pending");
                prior["remoteApproved"] = json!(false);
                prior
            };
            let mut envelope = json!({"protocol":1,"senderServer":PEER,"id":format!("message-{kind}"),
                "kind":kind,"room":ROOM,
                "payload":{"room":ROOM,"participants":[{"id":"remote-lead","role":"lead","name":"Remote"}],
                    "shareNames":false,"shareStatus":false},
                "created":NOW,"timestamp":NOW as i64,"nonce":format!("unique-nonce-{kind}-12345")});
            envelope["signature"] = json!(identity.sign(
                &transport::federation_message_signing_bytes(&envelope).expect("signing bytes")
            ));
            cases.push(json!({"name":kind,"envelope":envelope,"room":prior}));
        }
        corpus.data["cases"] = Value::Array(cases);
        corpus
    }
    async fn store(&self, name: &str, reverse: bool) -> (PathBuf, std::sync::Arc<RuntimeStore>) {
        let root = self.root.path().join(name);
        std::fs::remove_dir_all(&root).ok();
        std::fs::create_dir_all(&root).expect("fixture operation");
        let store = std::sync::Arc::new(
            RuntimeStore::open(Some(&root))
                .await
                .expect("fixture operation"),
        );
        let seed = self.data["seed"].clone();
        let identity = if reverse {
            seed["remote"].clone()
        } else {
            seed["local"].clone()
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
        let agents = seed["agents"].clone();
        store
            .transaction(move |tx| {
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
                for a in agents.as_array().expect("fixture operation") {
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                        params![a["id"].as_str().expect("fixture operation"), a.to_string()],
                    )?;
                }
                Ok(())
            })
            .await
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
    async fn signing_store(&self) -> std::sync::Arc<RuntimeStore> {
        self.store("signer", true).await.1
    }
}
fn rows(store: &RuntimeStore, table: &str) -> Vec<Value> {
    let db = store.test_reader().expect("fixture operation");
    let mut stmt = db
        .prepare(&format!("SELECT record FROM {table} ORDER BY rowid"))
        .expect("fixture operation");
    stmt.query_map([], |r| r.get::<_, String>(0))
        .expect("fixture operation")
        .map(|r| serde_json::from_str(&r.expect("fixture operation")).expect("fixture operation"))
        .collect()
}
fn counts(store: &RuntimeStore) -> Value {
    let db = store.test_reader().expect("fixture operation");
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
    let db = store.test_reader().expect("fixture operation");
    let mut stmt = db
        .prepare("SELECT seq,id,room,sender,text,created,deliveries FROM runtime_chat_messages ORDER BY seq")
        .expect("fixture operation");
    let chat = stmt
        .query_map([], |r| {
            Ok(json!({"seq":r.get::<_,i64>(0)?,"id":r.get::<_,String>(1)?,"room":r.get::<_,String>(2)?,"sender":r.get::<_,String>(3)?,"text":r.get::<_,String>(4)?,"created":r.get::<_,f64>(5)?,"deliveries":serde_json::from_str::<Value>(&r.get::<_,String>(6)?).expect("fixture operation")}))
        })
        .expect("fixture operation")
        .collect::<Result<Vec<_>, _>>()
        .expect("fixture operation");
    let mut stmt = db
        .prepare("SELECT id,agent,kind,text FROM runtime_events ORDER BY rowid")
        .expect("fixture operation");
    let events = stmt
        .query_map([], |r| {
            Ok(json!({"id":r.get::<_,String>(0)?,"agent":r.get::<_,String>(1)?,"kind":r.get::<_,String>(2)?,"text":serde_json::from_str::<Value>(&r.get::<_,String>(3)?).expect("fixture operation")}))
        })
        .expect("fixture operation")
        .collect::<Result<Vec<_>, _>>()
        .expect("fixture operation");
    (chat, events)
}
fn resign(envelope: &mut Value, identity: &transport::RemoteIdentity, nonce: &str) {
    envelope["nonce"] = json!(nonce);
    envelope["signature"] =
        json!(identity.sign(
            &transport::federation_message_signing_bytes(envelope).expect("fixture operation")
        ));
}

#[tokio::test]
async fn federation_corpus() {
    let corpus = Corpus::new().await;
    for case in corpus.data["cases"].as_array().expect("fixture operation") {
        let name = case["name"].as_str().expect("fixture operation");
        let (_, store) = corpus.store(name, false).await;
        if let Some(prior) = case.get("room") {
            let prior = prior.clone();
            let room_id = corpus.room().to_owned();
            store
                .transaction(move |tx| {
                    tx.execute("DELETE FROM runtime_federation_rooms", [])?;
                    if !prior.is_null() {
                        Repo::put(tx, Table::Room, &room_id, "", &prior)?;
                    }
                    Ok(())
                })
                .await
                .expect("prior room state");
        }
        let before = counts(&store);
        let result = effects::receive(&store, corpus.peer(), &case["envelope"], corpus.now()).await;
        if result.is_err() {
            assert_eq!(counts(&store), before, "{name} refusal writes nothing");
        }
        if ["chat", "unicode", "room-invite", "room-accept"].contains(&name) {
            result.unwrap_or_else(|e| panic!("{name}: {e}"));
        }
    }
    replay_and_nonce(&corpus).await;
    authenticated_consumer(&corpus).await;
    malformed_records(&corpus).await;
    spoofed_origin(&corpus).await;
    delivery_failures(&corpus).await;
    oversized_batches(&corpus).await;
    revoked_and_ordered(&corpus).await;
    rollback_matrix(&corpus).await;
    outbound_rollback(&corpus).await;
    lost_push_response(&corpus).await;
    callback_remote_message_uses_the_existing_federation_outbox(&corpus).await;
}

async fn callback_remote_message_uses_the_existing_federation_outbox(corpus: &Corpus) {
    use crate::multi_server_orchestration::{
        ExchangeOutcome, OrchestrationEffects, RuntimeOrchestrationEffects,
    };
    use std::sync::Arc;

    let (_, store) = corpus.store("callback-remote-message", false).await;
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
        .transaction_with(move |tx| {
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
        .await
        .expect("moved callback actor and paired link fixture");
    let effects = RuntimeOrchestrationEffects::new(Arc::clone(&store));
    let envelope = json!({
        "requestId":"stable-callback-remote-message",
        "action":"message",
        "payload":{"link":"callback-link","worker":"lead","epoch":0,"parentEpoch":0,"args":args}
    });
    let queued = effects
        .execute("peer-server", &envelope)
        .await
        .expect("moved worker callback uses the federation service");
    let ExchangeOutcome::Applied(queued) = queued else {
        panic!("remote callback should be queued")
    };
    assert_eq!(queued["status"], "queued");
    assert_eq!(rows(&store, "runtime_federation_outbox").len(), 1);

    let replay = effects
        .execute("peer-server", &envelope)
        .await
        .expect("stable remote callback replays its receipt");
    assert_eq!(replay, ExchangeOutcome::Applied(queued.clone()));
    assert_eq!(rows(&store, "runtime_federation_outbox").len(), 1);

    let mut changed = envelope.clone();
    changed["payload"]["args"]["text"] = json!("changed");
    assert!(effects.execute("peer-server", &changed).await.is_err());
    assert_eq!(rows(&store, "runtime_federation_outbox").len(), 1);
}

async fn replay_and_nonce(corpus: &Corpus) {
    let signer = corpus.signing_store().await;
    let identity = transport::load_federation_identity(&signer)
        .await
        .expect("fixture operation");
    let (path, store) = corpus.store("replay", false).await;
    let mut envelope = corpus.data["cases"][0]["envelope"].clone();
    let first = effects::receive(&store, corpus.peer(), &envelope, corpus.now())
        .await
        .expect("fixture operation");
    let before = counts(&store);
    assert!(
        effects::receive(&store, corpus.peer(), &envelope, corpus.now())
            .await
            .is_err(),
        "same nonce is refused"
    );
    assert_eq!(before, counts(&store));
    drop(store);
    let store = std::sync::Arc::new(
        RuntimeStore::open(Some(&path))
            .await
            .expect("fixture operation"),
    );
    resign(&mut envelope, &identity, "replay-transport-nonce-12345");
    assert_eq!(
        effects::receive(&store, corpus.peer(), &envelope, corpus.now())
            .await
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
    assert!(
        effects::receive(&store, corpus.peer(), &envelope, corpus.now())
            .await
            .is_err()
    );
    assert_eq!(counts(&store), before);
    let pull = effects::pull(
        std::sync::Arc::clone(&store),
        corpus.peer(),
        &json!({"protocol":1,"acks":[]}),
        corpus.now(),
    )
    .await
    .expect("fixture operation");
    assert_eq!(pull["acks"].as_array().expect("fixture operation").len(), 1);
    assert_eq!(rows(&store, "runtime_federation_inbox")[0]["ack"], "sent");
}
async fn authenticated_consumer(corpus: &Corpus) {
    let (_, store) = corpus.store("auth", false).await;
    let (_, signer) = corpus.store("auth-signer", true).await;
    let identity = transport::load_federation_identity(&signer)
        .await
        .expect("fixture operation");
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
            std::sync::Arc::clone(&store),
            request(),
            effects::OriginObservation::unavailable(),
            corpus.now()
        )
        .await
        .expect_err("missing trusted origin must refuse")
        .to_string()
        .contains("trusted-origin-observation")
    );
    assert_eq!(before, counts(&store));
    let response = effects::handle(
        std::sync::Arc::clone(&store),
        request(),
        effects::OriginObservation::observed(None),
        corpus.now(),
    )
    .await
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
            std::sync::Arc::clone(&store),
            request(),
            effects::OriginObservation::observed(None),
            corpus.now()
        )
        .await
        .is_err(),
        "reused request nonce is refused"
    );
    assert_eq!(counts(&store), before_replay, "reused nonce writes nothing");
}

#[tokio::test]
async fn federation_inbound_authentication_refusals() {
    // Freshness, trusted peer identity,
    // signature, nonce replay, and whois ownership are checked before effects.
    let corpus = Corpus::new().await;
    let (_, signer) = corpus.store("auth-refusal-signer", true).await;
    let identity = transport::load_federation_identity(&signer)
        .await
        .expect("fixture operation");
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

    let (_, store) = corpus.store("bad-request-signature", false).await;
    let before = counts(&store);
    let mut bad_signature = sign(corpus.now() as i64, "bad-authenticated-nonce-12345");
    bad_signature.push('x');
    assert!(
        effects::handle(
            std::sync::Arc::clone(&store),
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
        .await
        .is_err(),
        "bad signature is refused"
    );
    assert_eq!(counts(&store), before, "bad signature writes nothing");

    let (_, store) = corpus.store("stale-request-timestamp", false).await;
    let before = counts(&store);
    let stale = corpus.now() as i64 - 301;
    let stale_nonce = "stale-authenticated-nonce-12345";
    let stale_signature = sign(stale, stale_nonce);
    assert!(
        effects::handle(
            std::sync::Arc::clone(&store),
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
        .await
        .is_err(),
        "timestamp outside the five-minute window is refused"
    );
    assert_eq!(counts(&store), before, "stale timestamp writes nothing");

    let (_, store) = corpus.store("wrong-request-peer", false).await;
    let before = counts(&store);
    let unknown_peer = "99999999-9999-4999-8999-999999999999";
    let wrong_peer_nonce = "wrong-peer-authenticated-nonce-123";
    let wrong_peer_signature = sign(corpus.now() as i64, wrong_peer_nonce);
    assert!(
        effects::handle(
            std::sync::Arc::clone(&store),
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
        .await
        .is_err(),
        "unknown peer identity is refused"
    );
    assert_eq!(counts(&store), before, "unknown peer writes nothing");

    let (_, store) = corpus.store("wrong-whois-owner", false).await;
    let peer_id = corpus.peer().to_owned();
    store
        .transaction(move |tx| {
            let mut peer = Repo::record(tx, Table::Peer, &peer_id, "")?.expect("peer fixture");
            peer["whoisUser"] = json!("owner@example.test");
            Repo::put(tx, Table::Peer, &peer_id, "", &peer)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("set trusted whois owner");
    let before = counts(&store);
    let whois_nonce = "wrong-whois-authenticated-nonce-123";
    let whois_signature = sign(corpus.now() as i64, whois_nonce);
    assert!(
        effects::handle(
            std::sync::Arc::clone(&store),
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
        .await
        .is_err(),
        "mismatched whois owner is refused"
    );
    assert_eq!(counts(&store), before, "wrong whois owner writes nothing");
}

#[tokio::test]
async fn federation_pair_consumes_one_invitation_and_replays_by_peer_identity() {
    let corpus = Corpus::new().await;
    let (_, receiver) = corpus.store("pair-receiver", false).await;
    let (_, signer) = corpus.store("pair-signer", true).await;
    let identity = transport::load_federation_identity(&signer)
        .await
        .expect("fixture identity");
    let peer_id = corpus.peer();
    let invite_id = "44444444-4444-4444-8444-444444444444";
    let token = "fake-peer-invitation-token";
    let now = corpus.now();
    receiver
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_federation_invites(id,record) VALUES(?1,?2)",
                params![invite_id, json!({"id":invite_id,"tokenHash":studio_runtime_store::sha256_hex(token.as_bytes()),"created":now,"expires":now as i64 + 900,"status":"open","expectedUser":"peer@example.test"}).to_string()],
            )?;
            Ok(())
        })
            .await
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
    macro_rules! request {
        ($nonce:expr) => {{
            let nonce = $nonce;
            let signature = identity.sign(&transport::federation_signing_bytes(
                "POST",
                "/api/federation/v1/pair",
                corpus.now() as i64,
                nonce,
                &raw,
            ));
            effects::handle(
                std::sync::Arc::clone(&receiver),
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
            .await
        }};
    }
    let first = request!("pair-nonce-initial-12345").expect("pair request");
    let local = transport::load_federation_identity(&receiver)
        .await
        .expect("local signing identity");
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
    request!("pair-nonce-retry-67890").expect("same invitation and peer are replay safe");
    assert_eq!(
        rows(&receiver, "runtime_federation_invites")[0],
        consumed_invite
    );
    assert!(request!("pair-nonce-initial-12345").is_err());

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
            std::sync::Arc::clone(&receiver),
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
        .await
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
    let now = corpus.now();
    receiver
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_federation_invites(id,record) VALUES(?1,?2)",
                params![
                    expired_id,
                    json!({
                        "id": expired_id,
                        "tokenHash": studio_runtime_store::sha256_hex(expired_token.as_bytes()),
                        "created": now - 1000.0,
                        "expires": now as i64 - 1,
                        "status": "open",
                        "expectedUser": "peer@example.test"
                    })
                    .to_string()
                ],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
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
            std::sync::Arc::clone(&receiver),
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
        .await
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

#[tokio::test]
async fn federation_user_message_uses_the_local_lead_and_stable_idempotency_key() {
    let corpus = Corpus::new().await;
    let (_, store) = corpus.store("user-message", false).await;
    let first = delivery::user_message(
        std::sync::Arc::clone(&store),
        corpus.room(),
        "user message",
        "stable-user-message-key",
        studio_runtime_schema::delivery::DeliveryMoment::NextTurn,
        corpus.now(),
    )
    .await
    .expect("federated user message");
    let replay = delivery::user_message(
        std::sync::Arc::clone(&store),
        corpus.room(),
        "user message",
        "stable-user-message-key",
        studio_runtime_schema::delivery::DeliveryMoment::NextTurn,
        corpus.now() + 1.0,
    )
    .await
    .expect("idempotent federated user message");
    assert_eq!(first, replay);
    assert!(
        matches!(
            delivery::user_message(
                std::sync::Arc::clone(&store),
                corpus.room(),
                "user message",
                "stable-user-message-key",
                studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                corpus.now() + 2.0,
            )
            .await,
            Err(crate::federation_messaging::FederationMessageError::Conflict(_))
        ),
        "changed timing must remain a caller-visible conflict"
    );
    let queued = rows(&store, "runtime_federation_outbox");
    assert_eq!(queued.len(), 1);
    assert_eq!(
        queued[0]["payload"]["delivery"],
        json!(studio_runtime_schema::delivery::DeliveryMoment::NextTurn)
    );
    assert!(
        delivery::user_message(
            std::sync::Arc::clone(&store),
            corpus.room(),
            "changed text",
            "stable-user-message-key",
            studio_runtime_schema::delivery::DeliveryMoment::NextTurn,
            corpus.now(),
        )
        .await
        .is_err()
    );
}

async fn malformed_records(corpus: &Corpus) {
    for (index, value) in [Value::Null, json!([]), json!({}), json!({"enabled":"true"})]
        .into_iter()
        .enumerate()
    {
        let (_, store) = corpus.store(&format!("bad-{index}"), false).await;
        store
            .transaction(move |tx| {
                tx.execute(
                    "UPDATE runtime_federation_settings SET record=?1",
                    [value.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("fixture operation");
        let before = counts(&store);
        assert!(
            effects::receive(
                &store,
                corpus.peer(),
                &corpus.data["cases"][0]["envelope"],
                corpus.now()
            )
            .await
            .is_err()
        );
        assert_eq!(before, counts(&store));
    }
}

async fn revoked_and_ordered(corpus: &Corpus) {
    let (_, store) = corpus.store("stale-order", false).await;
    let room_id = corpus.room().to_owned();
    store
        .transaction(move |tx| {
            let mut room = Repo::record(tx, Table::Room, &room_id, "")?.expect("room exists");
            room["remoteSequence"] = json!(1);
            Repo::put(tx, Table::Room, &room_id, "", &room)?;
            Ok(())
        })
        .await
        .expect("advance prior room sequence");
    let before = counts(&store);
    assert!(
        effects::receive(
            &store,
            corpus.peer(),
            &corpus.data["cases"][0]["envelope"],
            corpus.now()
        )
        .await
        .is_err(),
        "equal sequence cannot deliver twice"
    );
    assert_eq!(before, counts(&store));

    for (name, column, value) in [
        ("peer-local", "localApproved", json!(false)),
        ("peer-remote", "remoteApproved", json!(false)),
        ("peer-status", "status", json!("revoked")),
    ] {
        let (_, store) = corpus.store(name, false).await;
        let peer_id = corpus.peer().to_owned();
        let column = column.to_owned();
        store
            .transaction(move |tx| {
                let mut peer = Repo::record(tx, Table::Peer, &peer_id, "")?.expect("peer exists");
                peer[column] = value;
                Repo::put(tx, Table::Peer, &peer_id, "", &peer)?;
                Ok(())
            })
            .await
            .expect("revocation fixture");
        let before = counts(&store);
        assert!(
            effects::receive(
                &store,
                corpus.peer(),
                &corpus.data["cases"][0]["envelope"],
                corpus.now()
            )
            .await
            .is_err(),
            "{name}"
        );
        assert_eq!(before, counts(&store));
    }
}

async fn rollback_matrix(corpus: &Corpus) {
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
            let (path, store) = corpus
                .store(&format!("rollback-{table}-{phase}"), false)
                .await;
            let prior = rows(&store, "runtime_federation_rooms");
            let operation = if table == "runtime_federation_rooms" {
                "UPDATE"
            } else {
                "INSERT"
            };
            store.transaction(move |tx|{tx.execute_batch(&format!("CREATE TRIGGER fault {phase} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'fixture crash'); END"))?;Ok(())})
            .await.expect("fixture operation");
            assert!(
                effects::receive(
                    &store,
                    corpus.peer(),
                    &corpus.data["cases"][0]["envelope"],
                    corpus.now()
                )
                .await
                .is_err(),
                "{phase} {table}"
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER fault")?;
                    Ok(())
                })
                .await
                .expect("fixture operation");
            drop(store);
            let store = std::sync::Arc::new(
                RuntimeStore::open(Some(&path))
                    .await
                    .expect("fixture operation"),
            );
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
            .await
            .expect("fixture operation");
            assert_eq!(counts(&store), json!([1, 0, 1, 1, 2]));
        }
    }
}
struct PeerTransport {
    receiver: std::sync::Arc<RuntimeStore>,
    sender: String,
    now: f64,
    state: std::sync::Mutex<PeerTransportState>,
    bad_receipt: Option<&'static str>,
    bad_response_signature: bool,
    link_advert: Option<&'static str>,
    cancel_after: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}
#[derive(Default)]
struct PeerTransportState {
    lose_once: bool,
    calls: usize,
    deadlines: Vec<Instant>,
}
impl delivery::FederationTransport for PeerTransport {
    fn exchange<'a>(
        &'a self,
        request: delivery::FederationExchange,
    ) -> futures_util::future::BoxFuture<
        'a,
        transport::RemoteTransportResult<delivery::FederationTransportResponse>,
    > {
        Box::pin(async move {
            let delivery::FederationExchange {
                url,
                headers,
                body,
                deadline,
                cancelled,
                ..
            } = request;
            if cancelled.load(std::sync::atomic::Ordering::Acquire) || Instant::now() >= deadline {
                return Err(transport::RemoteTransportError::ExchangeUnknown);
            }
            assert!(deadline.saturating_duration_since(Instant::now()) <= Duration::from_secs(8));
            {
                let mut state = self.state.lock().expect("fixture transport state");
                state.deadlines.push(deadline);
            }
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
                std::sync::Arc::clone(&self.receiver),
                effects::SignedRequest {
                    action,
                    peer_id: &self.sender,
                    timestamp: header("X-Studio-Federation-Time")
                        .parse()
                        .expect("fixture operation"),
                    nonce: header("X-Studio-Federation-Nonce"),
                    signature: header("X-Studio-Federation-Signature"),
                    raw: &body,
                },
                effects::OriginObservation::observed(None),
                self.now,
            )
            .await
            .expect("fixture operation");
            let response = if let Some(kind) = self.bad_receipt {
                let identity = transport::load_federation_identity(&self.receiver)
                    .await
                    .expect("fixture identity");
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
            let lose_once = {
                let mut state = self.state.lock().expect("fixture transport state");
                state.calls += 1;
                let lose_once = state.lose_once;
                state.lose_once = false;
                lose_once
            };
            if let Some(cancelled) = &self.cancel_after {
                cancelled.store(true, std::sync::atomic::Ordering::Release);
            }
            if lose_once {
                return Err(transport::RemoteTransportError::ExchangeUnknown);
            }
            Ok(delivery::FederationTransportResponse {
                body: serde_json::to_vec(&response).expect("fixture operation"),
                link_advert: self.link_advert.map(str::to_owned),
            })
        })
    }
}
async fn lost_push_response(corpus: &Corpus) {
    let (sender_path, sender) = corpus.store("sender", false).await;
    let (_, receiver) = corpus.store("receiver", true).await;
    let key = "stable-message-key";
    let room_id = corpus.room().to_owned();
    let now = corpus.now();
    let result = sender
        .transaction_with(move |tx| {
            delivery::send_message_in_transaction(
                tx,
                "lead",
                &room_id,
                "message to peer",
                key,
                studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                now,
            )
        })
        .await
        .expect("fixture operation");
    assert_eq!(
        result,
        delivery::send_message(
            std::sync::Arc::clone(&sender),
            "lead",
            corpus.room(),
            "message to peer",
            key,
            studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
            corpus.now() + 1.0
        )
        .await
        .expect("fixture operation")
    );
    assert!(
        delivery::send_message(
            std::sync::Arc::clone(&sender),
            "lead",
            corpus.room(),
            "changed",
            key,
            studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
            corpus.now()
        )
        .await
        .is_err()
    );
    let local = corpus.data["seed"]["local"]["stateId"]
        .as_str()
        .expect("fixture operation");
    let mut wire = PeerTransport {
        receiver: std::sync::Arc::clone(&receiver),
        sender: local.to_owned(),
        now: corpus.now(),
        state: std::sync::Mutex::new(PeerTransportState {
            lose_once: true,
            ..PeerTransportState::default()
        }),
        bad_receipt: None,
        bad_response_signature: false,
        link_advert: None,
        cancel_after: None,
    };
    assert!(
        delivery::pump_peer(
            std::sync::Arc::clone(&sender),
            corpus.peer(),
            corpus.now(),
            &wire,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
        )
        .await
        .is_err()
    );
    assert_eq!(counts(&receiver), json!([1, 0, 2, 1, 2]));
    drop(sender);
    let sender = std::sync::Arc::new(
        RuntimeStore::open(Some(&sender_path))
            .await
            .expect("fixture operation"),
    );
    wire.now += 2.0;
    assert_eq!(
        delivery::pump_peer(
            std::sync::Arc::clone(&sender),
            corpus.peer(),
            wire.now,
            &wire,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
        )
        .await
        .expect("fixture operation"),
        1
    );
    assert_eq!(
        rows(&sender, "runtime_federation_outbox")[0]["status"],
        "delivered"
    );
    assert_eq!(effect_rows(&receiver).0.len(), 1);
    assert_eq!(effect_rows(&receiver).1.len(), 2);
    assert_eq!(
        wire.state.lock().expect("peer transport state lock").calls,
        3,
        "two pushes and one pull"
    );
    let (push_deadline, pull_deadline) = {
        let state = wire.state.lock().expect("peer transport state lock");
        (state.deadlines[1], state.deadlines[2])
    };
    assert_eq!(
        push_deadline, pull_deadline,
        "push and pull share one absolute deadline"
    );
    let reverse = PeerTransport {
        receiver: std::sync::Arc::clone(&sender),
        sender: corpus.peer().to_owned(),
        now: wire.now,
        state: std::sync::Mutex::new(PeerTransportState::default()),
        bad_receipt: None,
        bad_response_signature: false,
        link_advert: None,
        cancel_after: None,
    };
    assert_eq!(
        delivery::pump_peer(
            std::sync::Arc::clone(&receiver),
            local,
            reverse.now,
            &reverse,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false))
        )
        .await
        .expect("acknowledge receiver inbox"),
        0
    );
    assert_eq!(
        rows(&receiver, "runtime_federation_inbox")[0]["ack"],
        "confirmed"
    );
}

#[cfg(unix)]
#[tokio::test]
async fn bad_federation_response_signature_stays_unknown_and_does_not_learn_route() {
    use std::os::unix::net::UnixListener;

    let corpus = Corpus::new().await;
    // Keep the Unix socket path below macOS's sockaddr_un path limit even
    // when the system temp directory has a long per-user prefix.
    let (_, sender) = corpus.store("s", false).await;
    let (_, receiver) = corpus.store("bad-response-signature-receiver", true).await;
    delivery::send_message(
        std::sync::Arc::clone(&sender),
        "lead",
        corpus.room(),
        "message with an unverified response",
        "bad-response-signature-key",
        studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
        corpus.now(),
    )
    .await
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
    let fake = PeerTransport {
        receiver: std::sync::Arc::clone(&receiver),
        sender: local.to_owned(),
        now: corpus.now(),
        state: std::sync::Mutex::new(PeerTransportState::default()),
        bad_receipt: None,
        bad_response_signature: true,
        link_advert: Some("100.80.1.2:443"),
        cancel_after: None,
    };
    assert!(
        delivery::pump_peer(
            std::sync::Arc::clone(&sender),
            corpus.peer(),
            corpus.now(),
            &fake,
            std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
        )
        .await
        .is_err()
    );
    assert_eq!(
        fake.state.lock().expect("peer transport state lock").calls,
        1
    );

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

async fn outbound_rollback(corpus: &Corpus) {
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
            let (path, store) = corpus
                .store(&format!("send-fault-{table}-{phase}"), false)
                .await;
            let prior = rows(&store, "runtime_federation_rooms");
            store.transaction(move |tx| {tx.execute_batch(&format!("CREATE TRIGGER fault {phase} {operation} ON {table} BEGIN SELECT RAISE(ABORT,'fixture crash'); END"))?;Ok(())})
            .await.expect("sender failpoint");
            assert!(
                delivery::send_message(
                    std::sync::Arc::clone(&store),
                    "lead",
                    corpus.room(),
                    "message",
                    "key",
                    studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                    corpus.now()
                )
                .await
                .is_err(),
                "{table} {phase}"
            );
            store
                .transaction(|tx| {
                    tx.execute_batch("DROP TRIGGER fault")?;
                    Ok(())
                })
                .await
                .expect("remove failpoint");
            drop(store);
            let store = std::sync::Arc::new(
                RuntimeStore::open(Some(&path))
                    .await
                    .expect("sender restarts"),
            );
            assert_eq!(counts(&store), json!([0, 0, 0, 0, 0]));
            assert_eq!(rows(&store, "runtime_federation_rooms"), prior);
            delivery::send_message(
                std::sync::Arc::clone(&store),
                "lead",
                corpus.room(),
                "message",
                "key",
                studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                corpus.now(),
            )
            .await
            .expect("queue after restart");
            assert_eq!(counts(&store), json!([0, 1, 0, 1, 1]));
        }
    }
}

async fn spoofed_origin(corpus: &Corpus) {
    let (_, store) = corpus.store("spoofed-origin", false).await;
    let peer_id = corpus.peer().to_owned();
    store
        .transaction(move |tx| {
            let mut peer = Repo::record(tx, Table::Peer, &peer_id, "")?.expect("peer");
            peer["whoisUser"] = json!("trusted-owner");
            Repo::put(tx, Table::Peer, &peer_id, "", &peer)
        })
        .await
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
            std::sync::Arc::clone(&store),
            request(),
            effects::OriginObservation::observed(Some("spoofed-header".into())),
            corpus.now()
        )
        .await
        .expect_err("spoofed origin")
        .to_string()
        .contains("origin identity mismatch")
    );
    assert!(
        effects::handle(
            std::sync::Arc::clone(&store),
            request(),
            effects::OriginObservation::unavailable(),
            corpus.now()
        )
        .await
        .expect_err("no trusted adapter")
        .to_string()
        .contains("trusted-origin-observation")
    );
    assert_eq!(counts(&store), before);
}

async fn delivery_failures(corpus: &Corpus) {
    for kind in ["schema", "message", "room", "cancel", "expired"] {
        let (_, sender) = corpus.store(&format!("failure-sender-{kind}"), false).await;
        let (_, receiver) = corpus
            .store(&format!("failure-receiver-{kind}"), true)
            .await;
        for key in ["one", "two"] {
            delivery::send_message(
                std::sync::Arc::clone(&sender),
                "lead",
                corpus.room(),
                "message",
                key,
                studio_runtime_schema::delivery::DeliveryMoment::CurrentTurn,
                corpus.now(),
            )
            .await
            .expect("queue");
        }
        let cancelled = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let wire = PeerTransport {
            receiver: std::sync::Arc::clone(&receiver),
            sender: corpus.data["seed"]["local"]["stateId"]
                .as_str()
                .expect("local id")
                .to_owned(),
            now: corpus.now(),
            state: std::sync::Mutex::new(PeerTransportState::default()),
            bad_receipt: if matches!(kind, "schema" | "message" | "room") {
                Some(kind)
            } else {
                None
            },
            bad_response_signature: false,
            link_advert: None,
            cancel_after: if kind == "cancel" {
                Some(std::sync::Arc::clone(&cancelled))
            } else {
                None
            },
        };
        if kind == "expired" {
            let before = counts(&sender);
            assert_eq!(
                delivery::pump_peer_until(
                    std::sync::Arc::clone(&sender),
                    corpus.peer(),
                    corpus.now(),
                    &wire,
                    delivery::PassBudget::new(Instant::now(), std::sync::Arc::clone(&cancelled))
                )
                .await
                .expect("expired pass"),
                0
            );
            assert_eq!(
                wire.state.lock().expect("peer transport state lock").calls,
                0
            );
            assert_eq!(counts(&sender), before);
            continue;
        }
        assert!(
            delivery::pump_peer(
                std::sync::Arc::clone(&sender),
                corpus.peer(),
                corpus.now(),
                &wire,
                std::sync::Arc::clone(&cancelled)
            )
            .await
            .is_err()
        );
        assert_eq!(
            wire.state.lock().expect("peer transport state lock").calls,
            1,
            "pass stops on failure or cancellation"
        );
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

async fn oversized_batches(corpus: &Corpus) {
    for table in [Table::Outbox, Table::Inbox] {
        let (_, store) = corpus
            .store(&format!("oversized-{}", table.name()), false)
            .await;
        let raw=json!({"status":"queued","nextAt":0,"created":0,"ack":"pending","padding":"x".repeat(512*1024+1)}).to_string();
        let peer_id = corpus.peer().to_owned();
        store.transaction(move |tx| {
            match table {
                Table::Outbox => tx.execute("INSERT INTO runtime_federation_outbox(id,peer_id,record) VALUES('large',?1,?2)",params![peer_id,raw])?,
                Table::Inbox => tx.execute("INSERT INTO runtime_federation_inbox(peer_id,message_id,record) VALUES(?1,'large',?2)",params![peer_id,raw])?,
                _ => unreachable!(),
            };
            let error=Repo::batch(tx,table,&peer_id,0.0).expect_err("oversized row refused by SQL projection");
            assert!(matches!(error,studio_runtime_store::StoreError::InvalidStoredObject {..}));
            Ok(())
        })
            .await.expect("batch size fixture");
    }
}
