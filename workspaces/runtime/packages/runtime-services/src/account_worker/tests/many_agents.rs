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

/// The scripted provider's per-run state.
struct Script<'a> {
    state: &'a State,
    agents: Vec<String>,
    frames: mpsc::UnboundedSender<Value>,
    turns: Vec<usize>,
    done: usize,
}
impl Script<'_> {
    /// Reply to `request`; a turn also completes, and its first turn queues
    /// the agent's after-turn input for the second exchange.
    async fn answer(&mut self, request: &Value) {
        let thread = request["params"]["threadId"].as_str().unwrap().to_owned();
        match request["method"].as_str().unwrap() {
            "thread/resume" => self
                .frames
                .send(json!({"id":request["id"],"result":{"thread":{"id":thread}}}))
                .unwrap(),
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
                self.frames
                    .send(json!({"id":request["id"],"result":{"turn":{"id":turn}}}))
                    .unwrap();
                self.frames
                    .send(json!({"method":"turn/completed","params":{"threadId":thread,"turn":{"id":turn,"status":"completed"}}}))
                    .unwrap();
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
    };
    let mut withheld = Vec::new();
    let run = async {
        while script.done < count * EXCHANGES_PER_AGENT {
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
    thirty_two_agents_complete_two_exchanges: 32,
    /// About half a minute of serialized store work in a debug build: run it
    /// with `just test-rust-many-agents` (manual heavy tier).
    #[ignore = "manual heavy tier: just test-rust-many-agents"]
    two_hundred_agents_complete_two_exchanges: 200,
);
