//! Many agents of one account woken together share one provider session.
//! The scripted provider first withholds every reply until each agent's first
//! request is written, then answers every request as soon as it is written.
//! Every agent must reach `turn/start` and finish two exchanges.
use super::*;

/// Bounds the whole many-agent run; unit: wall time.
const MANY_AGENTS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
/// Exchanges each agent completes: its first input and one after-turn input.
const EXCHANGES_PER_AGENT: usize = 2;
const THREAD_PREFIX: &str = "fake-thread-";
/// Streamed answer deltas per scripted turn, between the item start and end.
const DELTAS_PER_TURN: usize = 3;

/// `count` agents on one account: the fixture agent and clones of it, each
/// with its own thread and queued input.
async fn agents(state: &State, count: usize) -> Vec<String> {
    let first = state.agent.clone();
    let clones = (1..count)
        .map(|index| format!("agent-{index}"))
        .collect::<Vec<_>>();
    let rows = clones.clone();
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let template = AgentRepository::get(tx, &AgentId::from(first.as_str()))?.unwrap();
                for (offset, id) in rows.iter().enumerate() {
                    let mut row = template.clone();
                    row.id = AgentId::from(id.as_str());
                    row.root_id = AgentId::from(id.as_str());
                    row.extra.insert(
                        "threadId".into(),
                        json!(format!("{THREAD_PREFIX}{}", offset + 1)),
                    );
                    AgentRepository::put(tx, &row)?;
                }
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
    for id in &clones {
        state.enqueue_for(id, &format!("{id}-first")).await;
    }
    std::iter::once(state.agent.clone()).chain(clones).collect()
}

/// Managed workers unload at once; leads wait for the idle setting.
async fn make_workers(state: &State, agents: &[String]) {
    let agents = agents.to_vec();
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                for id in &agents {
                    let mut row = AgentRepository::get(tx, &AgentId::from(id.as_str()))?.unwrap();
                    row.extra.insert("isLead".into(), json!(false));
                    AgentRepository::put(tx, &row)?;
                }
                Ok(())
            })
            .await
        })
        .await
        .unwrap();
}

/// The scripted provider's per-run state.
struct Script<'a> {
    state: &'a State,
    agents: Vec<String>,
    frames: mpsc::UnboundedSender<Value>,
    turns: Vec<usize>,
    done: usize,
    /// Provider frames sent; unit: journal cursors.
    sent: u64,
}
impl Script<'_> {
    fn send(&mut self, frame: Value) {
        self.frames.send(frame).unwrap();
        self.sent += 1;
    }
    /// Reply to `request`; a turn also completes, and its first turn queues
    /// the agent's after-turn input for the second exchange.
    async fn answer(&mut self, request: &Value) {
        let thread = request["params"]["threadId"].as_str().unwrap().to_owned();
        match request["method"].as_str().unwrap() {
            "thread/resume" => {
                self.send(json!({"id":request["id"],"result":{"thread":{"id":thread}}}))
            }
            "turn/start" => {
                let index: usize = thread[THREAD_PREFIX.len()..].parse().unwrap();
                self.turns[index] += 1;
                let exchange = self.turns[index];
                assert!(exchange <= EXCHANGES_PER_AGENT, "{request}");
                if exchange == 1 {
                    let input = format!("{}-second", self.agents[index]);
                    self.state.enqueue_for(&self.agents[index], &input).await;
                    self.state.deliver_after_turn(&input).await;
                }
                let turn = format!("{thread}-turn-{exchange}");
                self.send(json!({"id":request["id"],"result":{"turn":{"id":turn}}}));
                let item = format!("{turn}-answer");
                let mut notifications = vec![
                    json!({"method":"turn/started","params":{"threadId":thread,"turn":{"id":turn}}}),
                    json!({"method":"item/started","params":{"threadId":thread,"turnId":turn,"item":{"id":item,"type":"agentMessage","text":""}}}),
                ];
                notifications.extend((0..DELTAS_PER_TURN).map(|part| {
                    json!({"method":"item/agentMessage/delta","params":{"threadId":thread,"turnId":turn,"itemId":item,"delta":format!("part-{part};")}})
                }));
                notifications.extend([
                    json!({"method":"item/completed","params":{"threadId":thread,"turnId":turn,"item":{"id":item,"type":"agentMessage","text":"done"}}}),
                    json!({"method":"thread/tokenUsage/updated","params":{"threadId":thread,"turnId":turn,"tokenUsage":{
                        "total":{"inputTokens":20,"outputTokens":100,"totalTokens":120},
                        "last":{"inputTokens":2,"outputTokens":10,"totalTokens":12}}}}),
                    json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":"completed"}}}),
                ]);
                for notification in notifications {
                    self.send(notification);
                }
                self.done += 1;
            }
            method => panic!("unexpected provider request {method}: {request}"),
        }
    }
}

/// Wake `count` agents of `provider` at once on one account.
async fn scenario_many_agents_complete_two_exchanges(provider: &str, count: usize) {
    let provider = match provider {
        "codex" => studio_runtime_providers::ProviderKind::Codex,
        _ => studio_runtime_providers::ProviderKind::Claude,
    };
    let state = State::new().await;
    let mut peer = Peer::new();
    let mut worker = provider_worker(&state, &mut peer, provider).await;
    let agents = agents(&state, count).await;
    for agent in &agents {
        worker
            .send(AccountCommand::Wake {
                agent_id: agent.clone(),
                epoch: state.epoch,
            })
            .await
            .unwrap();
    }
    // Frames go through their own task so the scripted provider never waits
    // for the worker while it answers writes.
    let (frames, mut outbox) = mpsc::unbounded_channel::<Value>();
    let (entries, clock, generation) = (peer.entries.clone(), peer.clock.clone(), peer.generation);
    let sender = tokio::spawn(async move {
        let mut cursor = 0;
        while let Some(frame) = outbox.recv().await {
            cursor += 1;
            if entries
                .send(journal_frame(&clock, generation, cursor, &frame))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let mut script = Script {
        state: &state,
        agents,
        frames,
        turns: vec![0; count],
        done: 0,
        sent: 0,
    };
    let mut withheld = Vec::new();
    let mut acknowledged = 0;
    let run = async {
        while script.done < count * EXCHANGES_PER_AGENT {
            let wire = tokio::select! {
                wire = peer.writes.recv() => wire.expect("worker submits request"),
                ack = peer.acks.recv() => {
                    acknowledged = acknowledged.max(ack.expect("session acknowledges").0);
                    continue;
                }
            };
            wire.receipt
                .send(CommandReceipt::Written {
                    remote_id: None,
                    duplicate: false,
                })
                .ok()
                .expect("write receipt receiver");
            if withheld.len() < count {
                withheld.push(wire.value);
                if withheld.len() == count {
                    for request in &withheld {
                        script.answer(request).await;
                    }
                }
            } else {
                script.answer(&wire.value).await;
            }
        }
        // Every frame is journaled and acknowledged before the count is read.
        while acknowledged < script.sent {
            acknowledged =
                acknowledged.max(peer.acks.recv().await.expect("session acknowledges").0);
        }
    };
    if tokio::time::timeout(MANY_AGENTS_DEADLINE, run)
        .await
        .is_err()
    {
        panic!(
            "{count} agents stalled: {} first requests written, turn/start count per agent {:?}",
            withheld.len(),
            script.turns
        );
    }
    assert!(script.turns.iter().all(|n| *n == EXCHANGES_PER_AGENT));
    assert!(!*worker.completion_signal().borrow());
    worker.cancel();
    sender.abort();
    let _completed = worker.completion().await;
}

macro_rules! many_agents {
    ($($(#[$attribute:meta])* $name:ident: $count:expr),* $(,)?) => {$(
        mod $name {
            $(#[$attribute])*
            #[tokio::test]
            async fn codex() {
                super::scenario_many_agents_complete_two_exchanges("codex", $count).await;
            }
            $(#[$attribute])*
            #[tokio::test]
            async fn claude() {
                super::scenario_many_agents_complete_two_exchanges("claude", $count).await;
            }
        }
    )*};
}

many_agents!(
    journal_capacity_plus_one_agents_complete_two_exchanges:
        studio_runtime_process::provider_seam::LANE_CAPACITY + 1,
    /// About half a minute of serialized store work in a debug build: run it
    /// with `just test-rust-many-agents` (manual heavy tier).
    #[ignore = "manual heavy tier: just test-rust-many-agents"]
    two_hundred_agents_complete_two_exchanges: 200,
);

/// Two settled/resumed agents and two still-running peers share one account.
const UNLOAD_AGENTS: usize = 4;

/// A scripted provider that keeps the loaded-thread set the way the Claude
/// bridge and Codex app-server do: `thread/resume` loads a thread and an
/// accepted `thread/unsubscribe` unloads it (a Claude thread's CLI exits).
#[derive(Default)]
struct LoadedProvider {
    loaded: BTreeSet<String>,
    running: BTreeMap<String, String>,
    /// Every turn/start input text, per thread.
    inputs: BTreeMap<String, Vec<String>>,
    listed: usize,
    unsubscribes: usize,
}
impl LoadedProvider {
    fn answer(&mut self, request: &Value, frames: &mpsc::UnboundedSender<Value>) {
        let params = &request["params"];
        let thread = params["threadId"].as_str().map(str::to_owned);
        let result = match request["method"].as_str().unwrap() {
            "thread/loaded/list" => {
                self.listed += 1;
                json!({"data": self.loaded, "nextCursor": null})
            }
            "thread/resume" => {
                let thread = thread.unwrap();
                self.loaded.insert(thread.clone());
                json!({"thread": {"id": thread}})
            }
            "turn/start" => {
                let thread = thread.unwrap();
                assert!(
                    self.loaded.contains(&thread),
                    "a turn starts only on a loaded thread: {request}"
                );
                assert!(!self.running.contains_key(&thread), "{request}");
                let texts = self.inputs.entry(thread.clone()).or_default();
                texts.push(params["input"].to_string());
                let turn = format!("{thread}-turn-{}", texts.len());
                self.running.insert(thread, turn.clone());
                json!({"turn": {"id": turn}})
            }
            "thread/read" => {
                let thread = thread.unwrap();
                let status = if self.running.contains_key(&thread) {
                    "active"
                } else {
                    "idle"
                };
                json!({"thread": {"id": thread, "status": {"type": status}}})
            }
            "thread/backgroundTerminals/list" | "thread/queue/list" => {
                json!({"data": [], "nextCursor": null})
            }
            "thread/unsubscribe" => {
                let thread = thread.unwrap();
                assert!(
                    !self.running.contains_key(&thread),
                    "a running turn is never unloaded: {request}"
                );
                self.unsubscribes += 1;
                let status = if self.loaded.remove(&thread) {
                    "unsubscribed"
                } else {
                    "notLoaded"
                };
                json!({"status": status})
            }
            method => panic!("unexpected provider request {method}: {request}"),
        };
        frames
            .send(json!({"id": request["id"], "result": result}))
            .unwrap();
    }
    fn complete(&mut self, thread: &str, frames: &mpsc::UnboundedSender<Value>) {
        let turn = self.running.remove(thread).expect("turn is running");
        frames
            .send(json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":"completed"}}}))
            .unwrap();
    }
}

/// Answer provider writes until `done` holds for the scripted provider.
async fn serve_until(
    peer: &mut Peer,
    provider: &mut LoadedProvider,
    frames: &mpsc::UnboundedSender<Value>,
    phase: &str,
    done: impl Fn(&LoadedProvider) -> bool,
) {
    let run = async {
        while !done(provider) {
            let wire = tokio::select! {
                wire = peer.writes.recv() => wire.expect("worker submits request"),
                ack = peer.acks.recv() => { ack.expect("session acknowledges"); continue; }
            };
            wire.receipt
                .send(CommandReceipt::Written {
                    remote_id: None,
                    duplicate: false,
                })
                .ok()
                .expect("write receipt receiver");
            provider.answer(&wire.value, frames);
        }
    };
    if tokio::time::timeout(MANY_AGENTS_DEADLINE, run)
        .await
        .is_err()
    {
        panic!(
            "{phase} stalled: loaded {:?}, running {:?}",
            provider.loaded, provider.running
        );
    }
}

/// With idle unloading on, `UNLOAD_AGENTS` agents of one account keep a
/// thread (a Claude CLI) loaded only while its turn runs: a settled worker turn's
/// thread is unloaded at once, and its next message resumes the same thread
/// and delivers that message exactly once.
async fn scenario_loaded_threads_follow_running_turns(provider: &str) {
    let provider = match provider {
        "codex" => studio_runtime_providers::ProviderKind::Codex,
        _ => studio_runtime_providers::ProviderKind::Claude,
    };
    let state = State::new().await;
    state
        .bridge
        .run(|store| async move {
            store
                .transaction_with(|tx| {
                    studio_runtime_store::OrchestrationSettingsRepository::set(
                        tx,
                        studio_runtime_store::IDLE_SESSION_UNLOAD_SETTING,
                        studio_runtime_store::IDLE_SESSION_UNLOAD_SETTING.default,
                    )
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    let mut peer = Peer::new();
    let mut worker = provider_worker(&state, &mut peer, provider).await;
    let agents = agents(&state, UNLOAD_AGENTS).await;
    make_workers(&state, &agents).await;
    let (frames, mut outbox) = mpsc::unbounded_channel::<Value>();
    let (entries, clock, generation) = (peer.entries.clone(), peer.clock.clone(), peer.generation);
    let sender = tokio::spawn(async move {
        let mut cursor = 0;
        while let Some(frame) = outbox.recv().await {
            cursor += 1;
            if entries
                .send(journal_frame(&clock, generation, cursor, &frame))
                .await
                .is_err()
            {
                break;
            }
        }
    });
    let wake_all = |agents: Vec<String>| {
        let worker = &worker;
        let epoch = state.epoch;
        async move {
            for agent_id in agents {
                worker
                    .send(AccountCommand::Wake { agent_id, epoch })
                    .await
                    .unwrap();
            }
        }
    };
    let mut native = LoadedProvider::default();
    let threads = (0..UNLOAD_AGENTS)
        .map(|index| format!("{THREAD_PREFIX}{index}"))
        .collect::<Vec<_>>();

    // The worker reads the loaded threads once its session is ready.
    serve_until(&mut peer, &mut native, &frames, "attach", |p| p.listed == 1).await;
    // Every agent runs: each running turn holds its loaded thread.
    wake_all(agents.clone()).await;
    serve_until(&mut peer, &mut native, &frames, "first turns", |p| {
        p.running.len() == UNLOAD_AGENTS
    })
    .await;
    assert_eq!(native.loaded.len(), UNLOAD_AGENTS);

    // Half the turns settle: exactly their threads unload, the rest stay.
    let (settled, still_running) = threads.split_at(UNLOAD_AGENTS / 2);
    for thread in settled {
        native.complete(thread, &frames);
    }
    serve_until(&mut peer, &mut native, &frames, "first unload", |p| {
        p.loaded.len() == still_running.len()
    })
    .await;
    assert_eq!(
        native.loaded,
        still_running.iter().cloned().collect::<BTreeSet<_>>()
    );

    // A new message for each settled agent resumes its thread and starts once.
    for (index, agent) in agents.iter().enumerate().take(settled.len()) {
        state
            .enqueue_for(agent, &format!("{agent}-second-{index}"))
            .await;
    }
    wake_all(agents[..settled.len()].to_vec()).await;
    serve_until(&mut peer, &mut native, &frames, "second turns", |p| {
        p.running.len() == UNLOAD_AGENTS
    })
    .await;
    assert_eq!(native.loaded.len(), UNLOAD_AGENTS);

    // Every turn settles: no thread stays loaded without a running turn.
    for thread in &threads {
        native.complete(thread, &frames);
    }
    serve_until(&mut peer, &mut native, &frames, "final unload", |p| {
        p.loaded.is_empty()
    })
    .await;
    assert!(native.running.is_empty());
    assert_eq!(native.unsubscribes, UNLOAD_AGENTS + settled.len());
    for (index, thread) in threads.iter().enumerate() {
        let inputs = &native.inputs[thread];
        let expected = if index < settled.len() { 2 } else { 1 };
        assert_eq!(inputs.len(), expected, "{thread}: {inputs:?}");
        if expected == 2 {
            let second = format!("{}-second-{index}", agents[index]);
            assert!(inputs[1].contains(&second), "{thread}: {inputs:?}");
            assert_eq!(
                inputs.iter().filter(|text| text.contains(&second)).count(),
                1,
                "the resumed turn delivers its message once: {inputs:?}"
            );
        }
    }
    assert!(!*worker.completion_signal().borrow());
    worker.cancel();
    sender.abort();
    let _completed = worker.completion().await;
}

mod settled_and_running_agents_keep_only_running_threads_loaded {
    #[tokio::test]
    async fn codex() {
        super::scenario_loaded_threads_follow_running_turns("codex").await;
    }
    #[tokio::test]
    async fn claude() {
        super::scenario_loaded_threads_follow_running_turns("claude").await;
    }
}
