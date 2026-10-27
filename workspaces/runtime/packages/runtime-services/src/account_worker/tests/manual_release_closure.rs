//! A completed worker still journals exact closure of its manual native release.
use super::*;
use studio_runtime_store::AgentStatus;

async fn completed_worker(state: &State, peer: &Peer) -> AccountHandle {
    let id = state.agent.clone();
    state.bridge.run(move |store| async move {
        store.transaction(move |tx| {
            let mut agent = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
            agent.status = AgentStatus::Completed;
            agent.extra.insert("isLead".into(), json!(false));
            agent.extra.insert("parentId".into(), json!("lead"));
            tx.execute("UPDATE runtime_events SET status='delivered' WHERE agent=?1 AND status='pending'", [&id])?;
            AgentRepository::put(tx, &agent)
        }).await?;
        Ok(())
    }).await.unwrap();
    peer.worker(state.bridge.clone()).await
}

async fn save_release(state: &State, worker: &AccountHandle, changes: Value) {
    let scope = worker.retained_session().scope().clone();
    let id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction(move |tx| {
                    let mut agent = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                    let record = agent.value();
                    let mut release = json!({
                        "id":"manual-release:closure-test", "phase":"released",
                        "targetEpoch":record["epoch"], "targetRootId":record["rootId"],
                        "targetParentId":record["parentId"], "threadId":record["threadId"],
                        "accountKey":record["accountKey"], "connectionId":scope.connection,
                        "scope":scope, "resetPending":true, "resetActorEpoch":0,
                        "at":1.0, "submittedAt":1.0
                    });
                    for (key, value) in changes.as_object().unwrap() {
                        if key == "scope" && value.is_object() {
                            release["scope"]
                                .as_object_mut()
                                .unwrap()
                                .extend(value.as_object().unwrap().clone());
                        } else {
                            release[key] = value.clone();
                        }
                    }
                    agent.extra.insert("nativeRelease".into(), release);
                    AgentRepository::put(tx, &agent)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
}

async fn close(peer: &mut Peer, worker: &AccountHandle, status_notice: bool) {
    let frame = if status_notice {
        json!({"method":"thread/status/changed","params":{"threadId":"fake-thread-0","status":{"type":"notLoaded"}}})
    } else {
        json!({"method":"thread/closed","params":{"threadId":"fake-thread-0"}})
    };
    let cursor = peer.frame(frame).await;
    peer.acknowledged(cursor).await;
    worker_barrier(worker).await;
}

#[tokio::test]
async fn completed_worker_records_exact_manual_release_closure_without_a_turn() {
    for status_notice in [false, true] {
        let state = State::new().await;
        let mut peer = Peer::new();
        let mut worker = completed_worker(&state, &peer).await;
        save_release(&state, &worker, json!({})).await;
        close(&mut peer, &worker, status_notice).await;
        let record = state.record().await;
        assert_eq!(record["nativeRelease"]["resetPending"], false);
        assert!(record["nativeRelease"]["closedAt"].as_f64().is_some());
        assert_eq!(record["nativeRelease"]["phase"], "released");
        assert_eq!(record["status"], "completed");
        assert!(
            peer.writes.try_recv().is_err(),
            "closure starts no native request"
        );
        close(&mut peer, &worker, status_notice).await;
        assert_eq!(
            state.record().await["nativeRelease"],
            record["nativeRelease"],
            "a repeated closure changes no receipt"
        );
        worker.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn completed_worker_rejects_stale_manual_release_closure_scope() {
    for changes in [
        json!({"targetEpoch":99}),
        json!({"targetRootId":"other-root"}),
        json!({"targetParentId":"other-parent"}),
        json!({"threadId":"other-thread"}),
        json!({"accountKey":"other-account"}),
        json!({"connectionId":"old-connection"}),
        json!({"scope":{"connection":"old-connection"}}),
        json!({"scope":{"generation":99}}),
        json!({"scope":{"account":"other-account"}}),
        json!({"scope":null}),
        json!({"submittedAt":null}),
        json!({"phase":"checking"}),
    ] {
        let state = State::new().await;
        let mut peer = Peer::new();
        let mut worker = completed_worker(&state, &peer).await;
        save_release(&state, &worker, changes.clone()).await;
        close(&mut peer, &worker, false).await;
        let record = state.record().await;
        assert_eq!(record["nativeRelease"]["resetPending"], true, "{changes}");
        assert!(record["nativeRelease"]["closedAt"].is_null(), "{changes}");
        assert!(peer.writes.try_recv().is_err());
        worker.shutdown().await.unwrap();
    }
}

#[tokio::test]
async fn exact_closure_does_not_replace_an_unknown_unsubscribe_receipt() {
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = completed_worker(&state, &peer).await;
    save_release(&state, &worker, json!({"phase":"unknown"})).await;
    close(&mut peer, &worker, false).await;
    let record = state.record().await;
    assert_eq!(record["nativeRelease"]["resetPending"], false);
    assert!(record["nativeRelease"]["closedAt"].as_f64().is_some());
    assert_eq!(record["nativeRelease"]["phase"], "unknown");
    state.enqueue("after-unknown-release").await;
    state.wake(&worker).await;
    worker_barrier(&worker).await;
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.unwrap();
}
