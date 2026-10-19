//! A host Codex thread request registers the Chrome skill root with its
//! connection first (Python `codex_browser.ensure_skill`), and carries the
//! node_repl server and the browser guidance.
use super::*;

async fn ready_worker(
    peer: &Peer,
    state: &State,
    homes: &crate::browser::BrowserHomes,
) -> AccountHandle {
    let mut resources = test_resources().unwrap();
    resources.account_home = homes.account.to_string_lossy().into_owned();
    resources.shared_home = homes.shared.to_string_lossy().into_owned();
    let worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;
    let mut ready = peer.ready.clone();
    while !*ready.borrow_and_update() {
        ready.changed().await.unwrap();
    }
    worker
}

/// Answer `skills/extraRoots/set` with `result` or, when `None`, an error.
async fn skill_roots(peer: &mut Peer, homes: &crate::browser::BrowserHomes, result: Option<Value>) {
    let request = peer.write("skills/extraRoots/set").await;
    assert_eq!(
        request["params"],
        json!({"extraRoots": [homes.skills.to_string_lossy()]})
    );
    match result {
        Some(result) => peer.reply(&request, result).await,
        None => {
            let cursor = peer
                .frame(json!({"id":request["id"],"error":{"code":-32603,"message":"not now"}}))
                .await;
            peer.acknowledged(cursor).await;
        }
    }
}

/// Answer the thread request and the turn it opens; check its browser setup.
async fn browser_turn(peer: &mut Peer, homes: &crate::browser::BrowserHomes, turn_id: &str) {
    let resume = peer.write("thread/resume").await;
    let params = &resume["params"];
    assert_eq!(
        params["config"]["mcp_servers.node_repl"]["command"],
        json!(homes.node)
    );
    assert!(
        params["developerInstructions"]
            .as_str()
            .unwrap()
            .ends_with(&format!("\n{}", crate::browser::BROWSER_GUIDANCE)),
        "Python appends the browser guidance last"
    );
    peer.reply(&resume, json!({"thread":{"id":"fake-thread-0"}}))
        .await;
    let turn = peer.write("turn/start").await;
    peer.reply(&turn, json!({"turn":{"id":turn_id}})).await;
    let cursor = peer
        .frame(
            json!({"method":"turn/completed","params":{"threadId":"fake-thread-0",
            "turn":{"id":turn_id,"status":"completed"}}}),
        )
        .await;
    peer.acknowledged(cursor).await;
}

/// Changed profile instructions make the loaded thread need a new `thread/resume`.
async fn change_settings(state: &State, instructions: &str, input: &str) {
    let instructions = instructions.to_owned();
    let agent_id = state.agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                        .expect("test agent exists");
                    agent
                        .extra
                        .insert("profileInstructions".into(), json!(instructions));
                    AgentRepository::put(tx, &agent)
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    state.enqueue(input).await;
}

#[tokio::test]
async fn thread_requests_register_the_chrome_skill_until_one_reply_confirms_it() {
    let homes = crate::browser::browser_homes();
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = ready_worker(&peer, &state, &homes).await;

    // An error leaves the roots unconfirmed; the thread request still goes out.
    state.wake(&worker).await;
    skill_roots(&mut peer, &homes, None).await;
    browser_turn(&mut peer, &homes, "turn-1").await;

    // The next thread request of this connection sends them again.
    change_settings(&state, "Second profile.", "second").await;
    state.wake(&worker).await;
    skill_roots(&mut peer, &homes, Some(json!({}))).await;
    browser_turn(&mut peer, &homes, "turn-2").await;

    // A confirmed reply covers every later thread request of the connection.
    change_settings(&state, "Third profile.", "third").await;
    state.wake(&worker).await;
    browser_turn(&mut peer, &homes, "turn-3").await;
    worker.shutdown().await.unwrap();
}
