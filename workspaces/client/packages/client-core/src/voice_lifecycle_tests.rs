use super::*;

struct PairingMemory(std::sync::Mutex<Value>);
impl crate::server_keys::Vault for PairingMemory {
    fn read(&self) -> Result<Value, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&self, value: &Value) -> Result<(), String> {
        *self.0.lock().unwrap() = value.clone();
        Ok(())
    }
}

fn revoked_pairing() -> Backend {
    let credential = crate::server_keys::new_pair(
        &json!({"protocol":1,"origin":"https://cleanup-fixture.ts.net","serverId":"remote","inviteId":"fixture","token":"fixture","publicKey":"fixture","tailscaleUser":"fixture"}),
        "fixture-pair",
    ).unwrap();
    let vault = Arc::new(PairingMemory(std::sync::Mutex::new(
        json!({"servers":{"remote":credential}}),
    )));
    let remote = Arc::new(crate::server_transport::RemoteClient::new(vault.clone()).unwrap());
    let backend = Backend::from_remote(remote.bind("remote").unwrap()).unwrap();
    assert!(backend.validate_identity().is_ok());
    vault.0.lock().unwrap()["servers"] = json!({});
    assert!(backend.validate_identity().is_err());
    backend
}

async fn stop_owned_session_with_failed_end(backend: Backend, sender: &StoreSender) -> String {
    let (reply, receive) = tokio::sync::oneshot::channel();
    sender
        .send((
            Request {
                id: "original-start".into(),
                method: "voice_journal.begin".into(),
                params: json!({"agent":"chat-a","muted":false}),
            },
            reply,
        ))
        .await
        .unwrap();
    let claim = receive.await.unwrap().result.unwrap()["row"].clone();
    let (view, _) =
        watch::channel(json!({"active":true,"sessionId":claim["sessionId"],"status":"Listening"}));
    // The fixture owns lifecycle state, not a physical microphone or a peer.
    let session = Arc::new(Session {
        store: sender.clone(),
        backend: backend.clone(),
        claim,
        view,
        closed: AtomicBool::new(false),
        submitted: AtomicBool::new(true),
        remote_set: AtomicBool::new(false),
        ready: AtomicBool::new(true),
        muted: AtomicBool::new(false),
        reconnect: AtomicU64::new(0),
        done: Notify::new(),
        signaling: Mutex::new(()),
    });
    assert!(session_owner().lock().await.is_none());
    *session_owner().lock().await = Some(session.clone());
    let request = |agent: &str, session_id: &str| Request {
        id: "original-stop".into(),
        method: "voice.sessionEnd".into(),
        params: json!({"agent":agent,"sessionId":session_id}),
    };
    let (sibling, _sibling_input) = tokio::sync::mpsc::channel(1);
    let sibling_reply = dispatch(&backend, &sibling, request("chat-a", session.id())).await;
    let wrong_agent = dispatch(&backend, sender, request("chat-b", session.id())).await;
    let wrong_session = dispatch(&backend, sender, request("chat-a", "replaced-session")).await;
    let kept_owner = !session.closed.load(Ordering::Acquire)
        && state_for(sender, "chat-a").await["active"] == true;
    let stopped = dispatch(&backend, sender, request("chat-a", session.id())).await;
    let replay = dispatch(&backend, sender, request("chat-a", session.id())).await;
    let no_owner = session_owner().lock().await.is_none();
    // The original Start receipt must remain Unknown after an unconfirmed End.
    let (reply, receive) = tokio::sync::oneshot::channel();
    sender
        .send((
            Request {
                id: "original-start".into(),
                method: "voice_journal.begin".into(),
                params: json!({"agent":"chat-a","muted":false}),
            },
            reply,
        ))
        .await
        .unwrap();
    let original = receive.await.unwrap().result.unwrap();

    assert_eq!(sibling_reply.result.unwrap()["active"], false);
    assert_eq!(wrong_agent.result.unwrap()["active"], false);
    assert!(wrong_session.error.is_some());
    assert!(
        kept_owner,
        "A sibling or changed session cannot stop the original owner."
    );
    assert!(stopped.error.is_none());
    let stopped = stopped.result.unwrap();
    assert_eq!(stopped["active"], false);
    assert!(
        stopped["error"]
            .as_str()
            .is_some_and(|error| !error.is_empty())
    );
    assert_eq!(replay.result.unwrap()["active"], false);
    assert!(session.closed.load(Ordering::Acquire));
    assert!(no_owner);
    assert_eq!(original["fresh"], false);
    assert_eq!(original["row"]["sessionId"], session.id());
    assert_eq!(original["row"]["phase"], "unknown");
    session.id().to_owned()
}

fn with_voice_store<T>(run: impl AsyncFnOnce(StoreSender) -> T) -> T {
    let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut store = crate::store::Store::open(&directory.path().join("client.sqlite")).unwrap();
    assert!(
        store
            .dispatch(Request {
                id: "fixture-identity".into(),
                method: "identity.set".into(),
                params: json!({"workspaceId":"a".repeat(32)}),
            })
            .error
            .is_none()
    );
    let (sender, mut input) = tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(16);
    let worker = std::thread::spawn(move || {
        while let Some((request, reply)) = input.blocking_recv() {
            let _ = reply.send(store.dispatch(request));
        }
    });
    let result = tokio::runtime::Runtime::new()
        .unwrap()
        .block_on(run(sender));
    worker.join().unwrap();
    result
}

#[test]
fn native_voice_stop_keeps_original_owner_after_pairing_revoke() {
    with_voice_store(async |sender| {
        let backend = revoked_pairing();
        let fresh = dispatch(
            &backend,
            &sender,
            Request {
                id: "new-start".into(),
                method: "voice.sessionStart".into(),
                params: json!({"agent":"chat-a"}),
            },
        )
        .await;
        assert!(
            fresh.error.is_some(),
            "Revoke must not authorize a fresh Start."
        );
        assert!(
            !stop_owned_session_with_failed_end(backend, &sender)
                .await
                .is_empty()
        );
    });
}

#[test]
fn native_voice_session_ended_timestamp_closes_only_the_exact_owned_session() {
    let check = with_voice_store(async |sender| {
        let backend = Backend::new("http://127.0.0.1:9").unwrap();
        let owned = test_owned_session(&backend, &sender, "chat-a", "timestamp-start", true)
            .await
            .unwrap();
        let session_id = owned.session_id().to_owned();
        let (sibling, _input) = tokio::sync::mpsc::channel(1);
        let ended = json!({"session_id":session_id,"state":"ended","sdp":null,"error":null,"ended":1792846998.125});
        apply_session(&backend, &sibling, "chat-a", &ended)
            .await
            .unwrap();
        apply_session(&backend, &sender, "chat-b", &ended)
            .await
            .unwrap();
        apply_session(
            &backend,
            &sender,
            "chat-a",
            &json!({"session_id":"replaced-session","state":"ended","ended":1792846998.125}),
        )
        .await
        .unwrap();
        let refused_siblings = !owned.closed() && owned.state()["active"] == true;
        apply_session(
            &backend,
            &sender,
            "chat-a",
            &json!({"session_id":session_id,"state":"ready","sdp":null,"error":null,"ended":null}),
        )
        .await
        .unwrap();
        let kept_live_session = !owned.closed() && owned.state()["active"] == true;
        apply_session(&backend, &sender, "chat-a", &ended)
            .await
            .unwrap();
        let closed = owned.closed();
        let stopped = owned.state();
        apply_session(&backend, &sender, "chat-a", &ended)
            .await
            .unwrap();
        let replayed = owned.state();
        owned.clear().await;
        let (reply, receive) = tokio::sync::oneshot::channel();
        sender
            .send((
                Request {
                    id: "timestamp-start".into(),
                    method: "voice_journal.begin".into(),
                    params: json!({"agent":"chat-a","muted":false}),
                },
                reply,
            ))
            .await
            .unwrap();
        let receipt = receive.await.unwrap().result.unwrap();
        move || {
            assert!(refused_siblings);
            assert!(
                kept_live_session,
                "A null ended timestamp must not stop a live session."
            );
            assert!(
                closed,
                "A server ended timestamp must stop the matching native session."
            );
            assert_eq!(stopped["active"], false);
            assert_eq!(replayed, stopped);
            assert_eq!(receipt["fresh"], false);
            assert_eq!(receipt["row"]["phase"], "ended");
            assert_eq!(receipt["row"]["sessionId"], session_id);
        }
    });
    check();
}

#[test]
fn native_voice_stop_preserves_unknown_when_the_end_backend_is_offline() {
    with_voice_store(async |sender| {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let failed_end = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(5), async {
                let mut end_body = Value::Null;
                for (expected, reply) in [
                    (b"GET /api/session HTTP/1.1\r\n".as_slice(), b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 19\r\nConnection: close\r\n\r\n{\"token\":\"fixture\"}".as_slice()),
                    (b"POST /api/voice/end HTTP/1.1\r\n".as_slice(), b"HTTP/1.1 503 Service Unavailable\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}".as_slice()),
                ] {
                    let (mut stream, _) = listener.accept().await.unwrap();
                    let mut header = Vec::new();
                    while !header.ends_with(b"\r\n\r\n") {
                        assert!(header.len() < 16 * 1024);
                        header.push(stream.read_u8().await.unwrap());
                    }
                    assert!(header.starts_with(expected));
                    if expected.starts_with(b"POST ") {
                        let header = String::from_utf8(header).unwrap();
                        let length: usize = header.lines().find_map(|line| {
                            let (name, value) = line.split_once(':')?;
                            name.eq_ignore_ascii_case("content-length").then(|| value.trim().parse().unwrap())
                        }).unwrap();
                        assert!(length <= 16 * 1024);
                        let mut body = vec![0; length];
                        stream.read_exact(&mut body).await.unwrap();
                        end_body = serde_json::from_slice(&body).unwrap();
                    }
                    stream.write_all(reply).await.unwrap();
                }
                end_body
            }).await.unwrap()
        });
        let session_id = stop_owned_session_with_failed_end(backend, &sender).await;
        assert_eq!(
            failed_end.await.unwrap(),
            json!({"agent":"chat-a","session_id":session_id})
        );
    });
}

#[test]
fn native_voice_old_end_publisher_cannot_replace_the_new_session_view() {
    with_voice_store(async |sender| {
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let (end_entered, held_end) = tokio::sync::oneshot::channel();
        let (release_end, end_release) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut socket).await;
            assert!(headers.starts_with("GET /api/session "), "{headers}");
            let body = json!({"token":"fixture"}).to_string();
            socket.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            drop(socket);
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, body) = crate::backend::tests::read_http(&mut socket).await;
            assert!(headers.starts_with("POST /api/voice/end "), "{headers}");
            end_entered.send(body).unwrap();
            end_release.await.unwrap();
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        });
        let a = test_owned_session(&backend, &sender, "chat-a", "old-start", true)
            .await
            .unwrap();
        let old_id = a.session_id().to_owned();
        let mut receipts = test_forwarder_receipts().subscribe();
        let output = watch_session(&sender, "chat-a").await;
        let observed = async |receipts: &mut tokio::sync::broadcast::Receiver<(String, Value)>,
                              id: &str,
                              active: bool| {
            tokio::time::timeout(Duration::from_secs(5), async {
                loop {
                    let (session, value) = receipts
                        .recv()
                        .await
                        .expect("the forwarder receipt remains open");
                    if session == id && value["active"] == active {
                        break;
                    }
                }
            })
            .await
            .expect("the exact session frame reaches its forwarder receipt");
        };
        observed(&mut receipts, &old_id, true).await;
        assert_eq!(output.borrow()["sessionId"], old_id);
        let ending = tokio::spawn(end(a.session.clone(), None, true));
        let body = tokio::time::timeout(Duration::from_secs(5), held_end)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(body, json!({"agent":"chat-a","session_id":old_id}));
        assert!(
            a.closed(),
            "End stops native capture before it waits for the server"
        );
        let b = test_owned_session(&backend, &sender, "chat-a", "new-start", false)
            .await
            .unwrap();
        let new_id = b.session_id().to_owned();
        publish_waiters(&b.session).await;
        observed(&mut receipts, &new_id, true).await;
        assert_eq!(output.borrow()["sessionId"], new_id);
        publish_waiters(&a.session).await;
        assert!(watchers().lock().await.iter().any(|row| {
            row.agent == "chat-a"
                && row
                    .store
                    .upgrade()
                    .is_some_and(|owner| owner.same_channel(&sender))
                && row.session.as_deref() == Some(new_id.as_str())
        }));
        release_end.send(()).unwrap();
        ending.await.unwrap().unwrap();
        observed(&mut receipts, &old_id, false).await;
        let final_view = output.borrow().clone();
        apply_session(
            &backend,
            &sender,
            "chat-a",
            &json!({"session_id":new_id,"ended":true}),
        )
        .await
        .unwrap();
        observed(&mut receipts, &new_id, false).await;
        assert_eq!(output.borrow()["sessionId"], new_id);
        assert_eq!(
            output.borrow()["active"],
            false,
            "the current owner's final state still reaches its watcher"
        );
        a.clear().await;
        b.clear().await;
        drop(output);
        server.await.unwrap();
        assert_eq!(
            final_view["sessionId"], new_id,
            "a completed old publisher cannot replace the new session view"
        );
        assert_eq!(final_view["active"], true);
    });
}

#[test]
fn native_voice_callbacks_cannot_end_a_sibling_context_or_changed_session() {
    let _serial = crate::media::AUDIO_TEST_MUTEX.lock().unwrap();
    let directory = tempfile::tempdir().unwrap();
    let mut store = crate::store::Store::open(&directory.path().join("client.sqlite")).unwrap();
    let row = store
        .dispatch(Request {
            id: "start-a".into(),
            method: "voice_journal.begin".into(),
            params: json!({"agent":"chat-a","muted":false}),
        })
        .result
        .unwrap()["row"]
        .clone();
    let (sender, mut input) = tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(16);
    let worker = std::thread::spawn(move || {
        while let Some((request, reply)) = input.blocking_recv() {
            let _ = reply.send(store.dispatch(request));
        }
    });
    let runtime = tokio::runtime::Runtime::new().unwrap();
    runtime.block_on(async{
        let backend=Backend::new("http://127.0.0.1:1").unwrap();
        let (view,_)=watch::channel(json!({"active":true,"sessionId":row["sessionId"],"status":"Connecting"}));
        let session=Arc::new(Session{store:sender.clone(),backend:backend.clone(),claim:row.clone(),view,closed:AtomicBool::new(false),submitted:AtomicBool::new(false),remote_set:AtomicBool::new(false),ready:AtomicBool::new(false),muted:AtomicBool::new(false),reconnect:AtomicU64::new(0),done:Notify::new(),signaling:Mutex::new(())});
        *session_owner().lock().await=Some(session.clone());
        let (sibling,_receive)=tokio::sync::mpsc::channel(1);
        let ended=json!({"session_id":session.id(),"ended":true});
        apply_session(&backend,&sibling,"chat-a",&ended).await.unwrap();
        apply_session(&backend,&sender,"chat-b",&ended).await.unwrap();
        apply_session(&backend,&sender,"chat-a",&json!({"session_id":"replaced","ended":true})).await.unwrap();
        transport_closed(&backend,&sibling,"chat-a").await.unwrap();
        assert!(!session.closed.load(Ordering::Acquire));
        assert_eq!(state_for(&sender,"chat-a").await["active"],true);
        apply_session(&backend,&sender,"chat-a",&ended).await.unwrap();
        assert!(session.closed.load(Ordering::Acquire));
        assert_eq!(session.view.borrow()["active"],false);
        assert!(session_owner().lock().await.is_none());
        let (reply,receive)=tokio::sync::oneshot::channel();
        sender.send((Request{id:"start-a".into(),method:"voice_journal.begin".into(),params:json!({"agent":"chat-a","muted":false})},reply)).await.unwrap();
        let saved=receive.await.unwrap().result.unwrap();
        assert_eq!(saved["fresh"],false);
        assert_eq!(saved["row"]["phase"],"ended");
        // A settlement cannot rewrite the original claim's scope.
        assert!(state(&sender,"voice_journal.settle",json!({"requestId":"start-a","sessionId":row["sessionId"],"agent":"chat-b","phase":"running"})).await.is_err());
    });
    drop(sender);
    worker.join().unwrap();
}
