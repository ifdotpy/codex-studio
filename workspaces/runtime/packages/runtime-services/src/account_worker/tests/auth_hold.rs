//! A turn whose account is not Ready is held with the auth gate's reason and
//! starts exactly once after the account's auth status changes, for both
//! providers (`account_auth_gate`).
use super::*;
use crate::account_driver::{AccountDriver, CHANGED_ERROR};
use studio_runtime_providers::ProviderKind;

/// Bounds each wait for a held turn; unit: wall time.
const HOLD_DEADLINE: std::time::Duration = std::time::Duration::from_secs(120);
const SIGNED_IN: &str = "fixture@example.test";
const OTHER: &str = "other@example.test";

/// One account of `kind` whose credentials a test signs in, out or swaps.
pub(super) struct Account {
    pub(super) kind: ProviderKind,
    home: PathBuf,
    pub(super) driver: Arc<AccountDriver>,
}

impl Account {
    /// A registry with one account of `kind`, signed in as [`SIGNED_IN`].
    pub(super) fn new(state: &State, kind: ProviderKind) -> Self {
        let home = state.root.join(format!("{}-home", kind.key()));
        let registry = state.root.join("accounts/registry.json");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
        let mut row = json!({"id":"default","home":home,"label":"Fixture","status":"ready","provider":kind.key()});
        if kind == ProviderKind::Claude {
            row["source"] = json!("Claude Code");
            row["claudeOptions"] = json!({"binaryPath": home.join("fake-claude"), "configDir": home, "customModels": []});
        } else {
            row["source"] = json!("Codex CLI");
        }
        std::fs::write(
            &registry,
            json!({"version":1,"defaultAccountKey":"default","accounts":{"default":row},
                "logins":{},"deleteReceipts":{}})
            .to_string(),
        )
        .unwrap();
        let account = Self {
            kind,
            home,
            driver: Arc::new(AccountDriver::open(&registry).unwrap()),
        };
        account.sign_in(SIGNED_IN);
        // The launcher pins the account after its first status read.
        account.driver.get("default").unwrap();
        account
    }

    /// Credentials of `email`, or none. Every state writes credential files of
    /// a distinct size, so the cached status ends as after a real login.
    fn credentials(&self, email: Option<&str>) {
        let state = email.map_or_else(|| "signed-out".to_owned(), |e| format!("in:{e}"));
        match self.kind {
            ProviderKind::Codex => {
                let auth = self.home.join("auth.json");
                match email {
                    Some(email) => std::fs::write(
                        auth,
                        json!({"tokens":{"account_id":email,"access_token":"fixture"}}).to_string(),
                    )
                    .unwrap(),
                    None => std::fs::remove_file(auth).unwrap(),
                }
            }
            ProviderKind::Claude => {
                let status = email.map_or_else(
                    || json!({"loggedIn":false}),
                    |email| json!({"loggedIn":true,"authMethod":"claude.ai","email":email,"subscriptionType":"max"}),
                );
                let binary = self.home.join("fake-claude");
                std::fs::write(&binary, format!("#!/bin/sh\nprintf '%s' '{status}'\n")).unwrap();
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700)).unwrap();
                std::fs::write(self.home.join(".credentials.json"), state).unwrap();
            }
        }
    }

    fn sign_in(&self, email: &str) {
        self.credentials(Some(email));
    }

    fn sign_out(&self) {
        self.credentials(None);
    }

    pub(super) fn resources(&self) -> preparation::PreparationResources {
        let mut resources = super::test_resources().unwrap();
        resources.claude_account_pin = self.driver.internal_record("default").unwrap();
        resources.account_driver = Some(Arc::clone(&self.driver));
        resources
    }

    fn sign_in_message(&self) -> &'static str {
        crate::provider_adapter::adapter(self.kind).sign_in_message()
    }

    /// Release held turns once, as `account_auth_gate::run_release` does.
    async fn release(&self, state: &State) -> Vec<(String, i64)> {
        crate::account_auth_gate::release_held(&state.bridge.store, &self.driver)
            .await
            .unwrap()
    }
}

/// The fixture agent and `others` clones, each on its own thread with its own
/// input, all on the provider of `kind`.
pub(super) async fn agents(state: &State, kind: ProviderKind, others: usize) -> Vec<String> {
    let first = state.agent.clone();
    let clones = (1..=others)
        .map(|index| format!("agent-{index}"))
        .collect::<Vec<_>>();
    let rows = clones.clone();
    state
        .bridge
        .run(move |s| async move {
            scheduler::lifecycle_transaction_with(s.as_ref(), move |tx| {
                let mut template =
                    AgentRepository::get(tx, &AgentId::from(first.as_str()))?.unwrap();
                if kind == ProviderKind::Claude {
                    template.extra.insert("provider".into(), json!("claude"));
                    template
                        .extra
                        .insert("nativeEffort".into(), json!("medium"));
                    template.extra.insert("claudeOptions".into(), json!({}));
                }
                AgentRepository::put(tx, &template)?;
                for (offset, id) in rows.iter().enumerate() {
                    let mut row = template.clone();
                    row.id = AgentId::from(id.as_str());
                    row.root_id = AgentId::from(id.as_str());
                    row.extra.insert(
                        "threadId".into(),
                        json!(format!("fake-thread-{}", offset + 1)),
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
        state.enqueue_for(id, id).await;
    }
    std::iter::once(state.agent.clone()).chain(clones).collect()
}

pub(super) async fn record_of(state: &State, agent: &str) -> Value {
    let agent = agent.to_owned();
    state
        .bridge
        .run(move |store| async move {
            let raw: String = store.test_reader()?.query_row(
                "SELECT record FROM runtime_agents WHERE id=?1",
                [agent],
                |row| row.get(0),
            )?;
            Ok(serde_json::from_str(&raw).unwrap())
        })
        .await
        .unwrap()
}

pub(super) async fn wake_agent(state: &State, worker: &AccountHandle, agent: &str) {
    worker
        .send(AccountCommand::Wake {
            agent_id: agent.to_owned(),
            epoch: state.epoch,
        })
        .await
        .unwrap();
}

/// Wake `agent` and serve its thread resume until its start is held with
/// `reason`; its input stays pending.
async fn hold(state: &State, peer: &mut Peer, worker: &AccountHandle, agent: &str, reason: &str) {
    let mut changes = state.bridge.agent_changes();
    wake_agent(state, worker, agent).await;
    tokio::time::timeout(HOLD_DEADLINE, async {
        while record_of(state, agent).await["error"] != reason {
            let write = tokio::select! {
                wire = peer.writes.recv() => Some(wire.expect("worker writes")),
                changed = changes.changed() => {
                    changed.unwrap();
                    None
                }
            };
            if let Some(wire) = write {
                assert_eq!(
                    wire.value["method"], "thread/resume",
                    "held starts write nothing else"
                );
                let _ignored = wire.receipt.send(CommandReceipt::Written {
                    remote_id: None,
                    duplicate: false,
                });
                let thread = wire.value["params"]["threadId"].clone();
                peer.reply(&wire.value, json!({"thread":{"id":thread}}))
                    .await;
            }
        }
    })
    .await
    .expect("the start is held");
    assert_eq!(record_of(state, agent).await["status"], "failed");
}

/// Wake `agent` and return its one turn/start, resuming its thread first.
async fn start(state: &State, peer: &mut Peer, worker: &AccountHandle, agent: &str) -> Value {
    wake_agent(state, worker, agent).await;
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
    assert_eq!(wire.value["method"], "turn/start");
    let _ignored = wire.receipt.send(CommandReceipt::Written {
        remote_id: None,
        duplicate: false,
    });
    wire.value
}

/// The released start carries its one input once and is the only write.
async fn starts_once(
    state: &State,
    peer: &mut Peer,
    worker: &AccountHandle,
    agent: &str,
    input: &str,
) {
    let start = start(state, peer, worker, agent).await;
    assert_eq!(
        start["params"]["input"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|item| item["text"].as_str())
            .map(|text| text.matches(input).count())
            .sum::<usize>(),
        1,
        "{start}"
    );
    peer.accepted(&start).await;
    wake_agent(state, worker, agent).await;
    worker_barrier(worker).await;
    assert!(peer.writes.try_recv().is_err(), "the input started twice");
}

async fn open(state: &State, account: &Account) -> (Peer, AccountHandle) {
    let mut peer = Peer::new();
    peer.wire = account.kind.wire();
    let worker = peer
        .open_worker_with_resources(state.bridge.clone(), account.resources())
        .await;
    (peer, worker)
}

/// A held start stays held while its account is blocked and starts once
/// after `unblock`; a second release finds nothing.
async fn released_once(
    kind: ProviderKind,
    block: impl Fn(&Account),
    reason: impl Fn(&Account) -> &'static str,
    unblock: impl Fn(&Account),
) {
    let state = State::new().await;
    let agent = agents(&state, kind, 0).await.remove(0);
    let account = Account::new(&state, kind);
    block(&account);
    let (mut peer, mut worker) = open(&state, &account).await;
    hold(&state, &mut peer, &worker, &agent, reason(&account)).await;
    assert_eq!(state.event_status("input").await, "pending");
    assert!(
        account.release(&state).await.is_empty(),
        "blocked: stays held"
    );
    unblock(&account);
    assert_eq!(
        account.release(&state).await,
        vec![(agent.clone(), state.epoch)]
    );
    assert!(account.release(&state).await.is_empty(), "requeued once");
    starts_once(&state, &mut peer, &worker, &agent, "input").await;
    assert_eq!(state.event_status("input").await, "delivered");
    worker.shutdown().await.unwrap();
}

#[cfg(unix)]
#[tokio::test]
async fn signed_out_start_is_held_and_starts_once_after_login() {
    for kind in ProviderKind::ALL {
        released_once(
            kind,
            Account::sign_out,
            Account::sign_in_message,
            |account| account.sign_in(SIGNED_IN),
        )
        .await;
    }
}

#[cfg(unix)]
#[tokio::test]
async fn changed_account_start_is_held_and_starts_once_after_restore() {
    for kind in ProviderKind::ALL {
        released_once(
            kind,
            |account| account.sign_in(OTHER),
            |_| CHANGED_ERROR,
            |account| account.sign_in(SIGNED_IN),
        )
        .await;
    }
}

/// A restart while a start is held keeps it held; the next account change
/// starts it once on the new connection.
#[cfg(unix)]
#[tokio::test]
async fn held_start_survives_restart_and_starts_once_after_login() {
    for kind in ProviderKind::ALL {
        let state = State::new().await;
        let agent = agents(&state, kind, 0).await.remove(0);
        let account = Account::new(&state, kind);
        account.sign_out();
        let (mut peer, mut worker) = open(&state, &account).await;
        hold(
            &state,
            &mut peer,
            &worker,
            &agent,
            account.sign_in_message(),
        )
        .await;
        worker.shutdown().await.unwrap();
        scheduler::recover_on_startup(&state.bridge.store)
            .await
            .unwrap();
        let record = state.record().await;
        assert_eq!(record["status"], "failed");
        assert_eq!(record["error"], account.sign_in_message());
        assert_eq!(state.event_status("input").await, "pending");
        let (mut peer, mut worker) = open(&state, &account).await;
        assert!(account.release(&state).await.is_empty());
        account.sign_in(SIGNED_IN);
        assert_eq!(account.release(&state).await.len(), 1);
        starts_once(&state, &mut peer, &worker, &agent, "input").await;
        worker.shutdown().await.unwrap();
    }
}

/// The retired Claude auth probe wait is read once at startup into the held
/// state, and that turn starts once after the account is Ready.
#[cfg(unix)]
#[tokio::test]
async fn retired_claude_auth_probe_wait_becomes_a_held_start_once() {
    let state = State::new().await;
    let agent = agents(&state, ProviderKind::Claude, 0).await.remove(0);
    let account = Account::new(&state, ProviderKind::Claude);
    let row = agent.clone();
    state
        .bridge
        .run(move |store| async move {
            store
                .transaction_with(move |tx| {
                    tx.execute(
                        "UPDATE runtime_agents SET record=json_set(record,'$.status','waiting',\
                         '$.error','Cannot read Claude Code sign-in status') WHERE id=?1",
                        [&row],
                    )?;
                    tx.execute(
                        "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES(?1,?2,?3)",
                        rusqlite::params![
                            "claude-auth-probe:attempt",
                            row,
                            json!({"status":"auth_probe_wait","accountKey":"default","dueAt":1.0})
                                .to_string()
                        ],
                    )?;
                    Ok::<_, StoreError>(())
                })
                .await?;
            Ok(())
        })
        .await
        .unwrap();
    scheduler::recover_on_startup(&state.bridge.store)
        .await
        .unwrap();
    let migrated = state.record().await;
    assert_eq!(migrated["status"], "failed");
    assert_eq!(migrated["error"], account.sign_in_message());
    let remaining = state
        .bridge
        .run(|store| async move {
            Ok(store.test_reader()?.query_row(
                "SELECT COUNT(*) FROM runtime_usage_resumes WHERE json_extract(record,'$.status')='auth_probe_wait'",
                [],
                |row| row.get::<_, i64>(0),
            )?)
        })
        .await
        .unwrap();
    assert_eq!(remaining, 0, "the retired wait is read once");
    scheduler::recover_on_startup(&state.bridge.store)
        .await
        .unwrap();
    assert_eq!(
        state.record().await,
        migrated,
        "a second startup changes nothing"
    );
    let (mut peer, mut worker) = open(&state, &account).await;
    assert_eq!(
        account.release(&state).await,
        vec![(agent.clone(), state.epoch)]
    );
    starts_once(&state, &mut peer, &worker, &agent, "input").await;
    worker.shutdown().await.unwrap();
}

/// Two held starts of one account are released oldest input first and each
/// starts once.
#[cfg(unix)]
#[tokio::test]
async fn two_held_starts_of_one_account_start_once_in_order() {
    for kind in ProviderKind::ALL {
        let state = State::new().await;
        let agents = agents(&state, kind, 1).await;
        let account = Account::new(&state, kind);
        account.sign_out();
        let (mut peer, mut worker) = open(&state, &account).await;
        for agent in &agents {
            hold(&state, &mut peer, &worker, agent, account.sign_in_message()).await;
        }
        account.sign_in(SIGNED_IN);
        let released = account.release(&state).await;
        assert_eq!(
            released,
            agents
                .iter()
                .map(|agent| (agent.clone(), state.epoch))
                .collect::<Vec<_>>()
        );
        starts_once(&state, &mut peer, &worker, &agents[0], "input").await;
        starts_once(&state, &mut peer, &worker, &agents[1], &agents[1]).await;
        worker.shutdown().await.unwrap();
    }
}

/// While a start is held the release loop sleeps on account and auth change
/// signals only: a week of paused time starts no release pass, and one
/// account change starts exactly one.
#[cfg(unix)]
#[tokio::test(start_paused = true)]
async fn held_start_wakes_nothing_until_an_account_change() {
    for kind in ProviderKind::ALL {
        let state = State::new().await;
        agents(&state, kind, 0).await;
        let account = Account::new(&state, kind);
        account.sign_out();
        // Settle the signed-out status, so its own change is already published.
        account.driver.get("default").unwrap();
        let agent = state.agent.clone();
        let reason = account.sign_in_message();
        state
            .bridge
            .run(move |store| async move {
                scheduler::lifecycle_transaction_with(store.as_ref(), move |tx| {
                    let mut row =
                        AgentRepository::get(tx, &AgentId::from(agent.as_str()))?.unwrap();
                    row.status = studio_runtime_store::AgentStatus::Failed;
                    row.extra.insert("error".into(), json!(reason));
                    Ok(AgentRepository::put(tx, &row)?)
                })
                .await
            })
            .await
            .unwrap();
        let (passes, mut seen) = mpsc::unbounded_channel();
        let (stop, shutdown) = watch::channel(false);
        let task = tokio::spawn(crate::account_auth_gate::release_loop(
            Arc::clone(&state.bridge.store),
            Arc::clone(&account.driver),
            shutdown,
            move |pass| passes.send(pass).unwrap(),
        ));
        assert_eq!(seen.recv().await, Some(None));
        assert_eq!(seen.recv().await, Some(Some(0)), "signed out: stays held");
        tokio::time::sleep(std::time::Duration::from_secs(7 * 24 * 3600)).await;
        assert!(
            seen.try_recv().is_err(),
            "a held start woke the release loop"
        );
        account.sign_in(SIGNED_IN);
        // A status read persists the login and publishes the account change.
        account.driver.get("default").unwrap();
        assert_eq!(seen.recv().await, Some(None));
        assert_eq!(seen.recv().await, Some(Some(1)));
        stop.send(true).unwrap();
        task.await.unwrap();
        assert_eq!(state.record().await["status"], "queued");
    }
}
