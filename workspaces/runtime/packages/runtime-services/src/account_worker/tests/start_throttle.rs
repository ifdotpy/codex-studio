//! The account start throttle (`account_start_gate`) on the account worker,
//! for both providers: a start over a limit waits with its input queued and
//! a visible `startWait`, and starts exactly once when a slot frees, the
//! settings change, or after a restart. A waiting start is an in-flight
//! preparation, so the worker barrier is used only once nothing waits.
use super::auth_hold::{Account, agents, record_of, wake_agent};
use super::*;
use crate::account_start_gate::StartThrottle;
use studio_runtime_providers::ProviderKind;

/// Bounds each wait for a worker effect; unit: wall time.
const DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
/// A pause no test outlives, so a start behind it waits for the test's change.
const LONG_PAUSE_SECONDS: u64 = 3600;

fn throttle(burst: u64, pause_seconds: u64, max_running: u64) -> StartThrottle {
    StartThrottle::new(burst, pause_seconds, max_running).expect("valid throttle")
}

/// The fixture threads: the first agent runs `fake-thread-0`, its clones
/// `fake-thread-<n>`.
fn thread_of(agents: &[String], agent: &str) -> String {
    let index = agents.iter().position(|id| id == agent).expect("agent");
    format!("fake-thread-{index}")
}

fn agent_of(agents: &[String], thread: &str) -> String {
    let index: usize = thread
        .strip_prefix("fake-thread-")
        .and_then(|index| index.parse().ok())
        .expect("fixture thread");
    agents[index].clone()
}

/// Serve the next turn start (resuming its thread first) and accept it.
/// Returns the agent whose turn started and that start's input.
async fn serve_start(peer: &mut Peer, agents: &[String]) -> (String, Value) {
    let mut wire = peer.next_write().await;
    if wire.value["method"] == "thread/resume" {
        let resume = wire.value;
        let _ignored = wire.receipt.send(CommandReceipt::Written {
            remote_id: None,
            duplicate: false,
        });
        let thread = resume["params"]["threadId"].clone();
        peer.reply(&resume, json!({"thread":{"id":thread}})).await;
        wire = peer.next_write().await;
    }
    assert_eq!(wire.value["method"], "turn/start", "{}", wire.value);
    let _ignored = wire.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    let start = wire.value;
    peer.accepted(&start).await;
    let thread = start["params"]["threadId"].as_str().expect("thread");
    (agent_of(agents, thread), start["params"]["input"].clone())
}

async fn complete(peer: &mut Peer, agents: &[String], agent: &str) {
    let cursor = peer
        .frame(
            json!({"method":"turn/completed","params":{"threadId":thread_of(agents, agent),
            "turn":{"id":"turn","status":"completed"}}}),
        )
        .await;
    peer.acknowledged(cursor).await;
}

/// Wait until at least `count` agents in `waiting` show a start wait for
/// `reason`; all of them stay queued.
async fn shows_wait(state: &State, waiting: &[String], count: usize, reason: &str) {
    let mut changes = state.bridge.agent_changes();
    tokio::time::timeout(DEADLINE, async {
        loop {
            let mut shown = 0;
            for agent in waiting {
                let record = record_of(state, agent).await;
                assert_eq!(record["status"], "queued", "a waiting start stays queued");
                if record["startWait"]["reason"] == reason
                    && record["startWait"]["epoch"] == state.epoch
                {
                    shown += 1;
                }
            }
            if shown >= count {
                return;
            }
            changes.changed().await.expect("agent changes");
        }
    })
    .await
    .expect("the waits are shown");
}

/// Nothing more is written: every wake of `agents` finds its turn started.
async fn no_more_starts(state: &State, peer: &mut Peer, worker: &AccountHandle, agents: &[String]) {
    for agent in agents {
        wake_agent(state, worker, agent).await;
    }
    worker_barrier(worker).await;
    assert!(peer.writes.try_recv().is_err(), "an input started twice");
}

async fn open(state: &State, bridge: StoreBridge, account: &Account) -> (Peer, AccountHandle) {
    let mut peer = Peer::new();
    peer.wire = account.kind.wire();
    let worker = peer
        .open_worker_with_resources(bridge, account.resources())
        .await;
    let _ = state;
    (peer, worker)
}

/// A burst starts at once and the rest waits for the pause; a cap on running
/// turns holds until a turn settles; settings changes apply to waiting starts
/// at once; and each input starts exactly once.
#[cfg(unix)]
#[tokio::test]
async fn throttled_starts_wait_and_each_starts_once() {
    for kind in ProviderKind::ALL {
        let state = State::new().await;
        let agents = agents(&state, kind, 2).await;
        let account = Account::new(&state, kind);
        account
            .driver
            .set_start_throttle("default", throttle(1, LONG_PAUSE_SECONDS, 8))
            .unwrap();
        let (mut peer, mut worker) = open(&state, state.bridge.clone(), &account).await;
        for agent in &agents {
            wake_agent(&state, &worker, agent).await;
        }
        // A burst of one: the first start goes, the others wait for the pause.
        let (first, _) = serve_start(&mut peer, &agents).await;
        let mut waiting = agents
            .iter()
            .filter(|agent| **agent != first)
            .cloned()
            .collect::<Vec<_>>();
        shows_wait(&state, &waiting, waiting.len(), "pause").await;
        assert!(peer.writes.try_recv().is_err(), "a start skipped the pause");
        // No pause, one running turn: the first turn holds the only slot.
        account
            .driver
            .set_start_throttle("default", throttle(8, 0, 1))
            .unwrap();
        // The next start in line shows the slot wait.
        shows_wait(&state, &waiting, 1, "slot").await;
        assert!(peer.writes.try_recv().is_err(), "a start exceeded the cap");
        assert!(
            record_of(&state, &first).await.get("startWait").is_none(),
            "the claim clears the wait"
        );
        // The settled turn frees its slot for exactly one waiting start.
        complete(&mut peer, &agents, &first).await;
        let (second, input) = serve_start(&mut peer, &agents).await;
        assert!(waiting.contains(&second));
        let id = if second == agents[0] {
            "input"
        } else {
            second.as_str()
        };
        assert_eq!(
            input
                .as_array()
                .unwrap()
                .iter()
                .filter_map(|item| item["text"].as_str())
                .map(|text| text.matches(id).count())
                .sum::<usize>(),
            1,
            "{input}"
        );
        waiting.retain(|agent| *agent != second);
        assert!(peer.writes.try_recv().is_err(), "a start exceeded the cap");
        // A larger cap starts the last waiting input at once.
        account
            .driver
            .set_start_throttle("default", throttle(8, 0, 8))
            .unwrap();
        let (third, _) = serve_start(&mut peer, &agents).await;
        assert_eq!(waiting, vec![third]);
        no_more_starts(&state, &mut peer, &worker, &agents).await;
        worker.shutdown().await.unwrap();
    }
}

/// A restart while a start waits keeps its input queued; the restarted
/// backend starts it once.
#[cfg(unix)]
#[tokio::test]
async fn a_waiting_start_survives_a_restart_and_starts_once() {
    for kind in ProviderKind::ALL {
        let state = State::new().await;
        let agents = agents(&state, kind, 1).await;
        let account = Account::new(&state, kind);
        account
            .driver
            .set_start_throttle("default", throttle(1, LONG_PAUSE_SECONDS, 8))
            .unwrap();
        let (mut peer, mut worker) = open(&state, state.bridge.clone(), &account).await;
        for agent in &agents {
            wake_agent(&state, &worker, agent).await;
        }
        let (first, _) = serve_start(&mut peer, &agents).await;
        let waiting = agents
            .iter()
            .find(|agent| **agent != first)
            .cloned()
            .expect("a waiting agent");
        shows_wait(&state, std::slice::from_ref(&waiting), 1, "pause").await;
        worker.shutdown().await.unwrap();
        scheduler::recover_on_startup(&state.bridge.store)
            .await
            .unwrap();
        assert_eq!(record_of(&state, &waiting).await["status"], "queued");
        // A new process has new gates; its burst starts the waiting input.
        let restarted = test_bridge(Arc::clone(&state.bridge.store)).await;
        let (mut peer, mut worker) = open(&state, restarted, &account).await;
        wake_agent(&state, &worker, &waiting).await;
        let (started, input) = serve_start(&mut peer, &agents).await;
        assert_eq!(started, waiting);
        let id = if waiting == agents[0] {
            "input"
        } else {
            waiting.as_str()
        };
        assert_eq!(input.to_string().matches(id).count(), 1, "{input}");
        wake_agent(&state, &worker, &waiting).await;
        worker_barrier(&worker).await;
        assert!(peer.writes.try_recv().is_err(), "the input started twice");
        worker.shutdown().await.unwrap();
    }
}
