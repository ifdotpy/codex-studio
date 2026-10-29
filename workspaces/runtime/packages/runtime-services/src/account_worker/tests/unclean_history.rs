use super::*;
use serde::Serialize;

#[derive(Serialize)]
struct Page {
    data: Vec<Turn>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Turn {
    id: &'static str,
    thread_id: &'static str,
    status: &'static str,
    items: Vec<Item>,
}
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Item {
    id: &'static str,
    #[serde(rename = "type")]
    kind: &'static str,
    client_id: &'static str,
}

#[tokio::test]
async fn unclean_held_start_recovers_exact_input_through_worker_history() {
    let state = State::new().await;
    let agent = state.agent.clone();
    let epoch = state.epoch;
    state
        .bridge
        .run(move |store| async move {
            store::claim(Arc::clone(&store), "default", &agent, epoch)
                .await?
                .expect("claimed");
            store
                .transaction_with(|tx| {
                    studio_runtime_store::SchedulerRepository::hold_unclean_starts(tx)?;
                    Ok::<_, WorkerError>(())
                })
                .await?;
            Ok(())
        })
        .await
        .expect("hold input");
    assert_eq!(state.event_status("input").await, "uncertain");
    let mut peer = Peer::new();
    let mut worker = peer.worker(state.bridge.clone()).await;
    let history = peer.write("thread/turns/list").await;
    peer.reply(
        &history,
        studio_runtime_store::projection_value(&Page {
            data: vec![Turn {
                id: "native-turn",
                thread_id: "fake-thread-0",
                status: "completed",
                items: vec![Item {
                    id: "input-item",
                    kind: "userMessage",
                    client_id: "annotation:input",
                }],
            }],
        }),
    )
    .await;
    worker_barrier(&worker).await;
    assert_eq!(state.event_status("input").await, "delivered");
    #[derive(serde::Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Saved {
        start_attempt: SavedAttempt,
        auto_wake: bool,
    }
    #[derive(serde::Deserialize)]
    struct SavedAttempt {
        submitted: bool,
    }
    let record: Saved =
        serde_json::from_str(&state.record().await.to_string()).expect("saved agent");
    assert!(!record.start_attempt.submitted);
    assert!(!record.auto_wake);
    assert!(peer.writes.try_recv().is_err());
    worker.shutdown().await.expect("shutdown");
}
