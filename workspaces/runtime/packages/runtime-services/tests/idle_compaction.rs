//! Production timer admission and native hand-off, with a scripted provider.

#![allow(clippy::unwrap_used)]
use rusqlite::Transaction;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use studio_runtime_providers::{Method, RequestId, RequestTicket};
use studio_runtime_schema::idle_compaction::{Mode, Settings};
use studio_runtime_services::account_session_runtime::SessionOutcome as ReceiptOutcome;
use studio_runtime_services::{
    account_session_runtime::*,
    idle_compaction::{self as idle, Episode, Host},
    native_actions::{self as native, Action, Admission},
};
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, OrchestrationSettingsRepository, RuntimeStore,
    StoreError,
};

#[derive(Default)]
struct Session {
    calls: Mutex<Vec<Value>>,
    finished: tokio::sync::Notify,
    /// The account worker exited after its idle deadline: no live scope until a
    /// caller starts it again with `ensure_session`.
    dormant: std::sync::atomic::AtomicBool,
    ensured: std::sync::atomic::AtomicUsize,
}
impl Session {
    fn scope() -> ConnectionScope {
        ConnectionScope {
            account: "default".into(),
            connection: "connection".into(),
            generation: 1,
        }
    }
}
impl AccountSessionPort for Session {
    fn read_limits_for_resume<'a>(
        &'a self,
        _account: &'a str,
        _after: f64,
    ) -> AccountSessionFuture<'a, Value> {
        Box::pin(async { Ok(limits(idle::wall_now())) })
    }
    fn current_scope(&self, _: &str) -> Option<ConnectionScope> {
        (!self.dormant.load(std::sync::atomic::Ordering::SeqCst)).then(Self::scope)
    }
    fn ensure_session<'a>(&'a self, _: &'a str) -> AccountSessionFuture<'a, ConnectionScope> {
        self.ensured
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.dormant
            .store(false, std::sync::atomic::Ordering::SeqCst);
        Box::pin(async { Ok(Self::scope()) })
    }
    fn provider_request<'a>(
        &'a self,
        request: ProviderRequest,
    ) -> AccountSessionFuture<'a, SessionReceipt> {
        Box::pin(async move {
            assert_eq!(request.method, Method::ThreadCompactStart);
            self.calls.lock().unwrap().push(request.params);
            Ok(SessionReceipt {
                scope: Self::scope(),
                operation: request.operation.clone(),
                replayed: false,
                outcome: ReceiptOutcome::Written {
                    ticket: RequestTicket {
                        id: RequestId::String(format!(
                            "retained:default:{}",
                            studio_runtime_store::sha256_hex(request.operation.as_bytes())
                        )),
                        generation: 1,
                        method: request.method,
                    },
                    response: ProviderResponse::Reply(
                        json!({"turn":{"id":"compact-turn","status":"inProgress"}}),
                    ),
                },
            })
        })
    }
}
struct Gate;
impl Admission for Gate {
    fn check(&self, _: &Transaction<'_>, _: &Value, _: Action) -> native::Result<()> {
        Ok(())
    }
}
struct HostPort {
    store: Arc<RuntimeStore>,
    session: Arc<Session>,
}
impl Host for HostPort {
    fn compact<'a>(
        &'a self,
        agent: &'a str,
        episode: Episode,
        limits: Value,
    ) -> idle::HostFuture<'a> {
        Box::pin(async move {
            let result = native::execute_provider(
                &self.store,
                self.session.as_ref(),
                &native::Request {
                    agent: agent.into(),
                    request_id: episode.id.clone(),
                    action: Action::Compact,
                    context: episode.identity.clone(),
                },
                idle::wall_now(),
                Arc::new(idle::IdleAdmission {
                    inner: Arc::new(Gate),
                    episode,
                    limits,
                }),
                Arc::new(studio_runtime_services::native_action_ports::NativeActionAcceptance),
            )
            .await
            .map(|_| ())
            .map_err(|error| error.to_string());
            self.session.finished.notify_one();
            result
        })
    }
}
fn limits(now: f64) -> Value {
    json!({"accountKey":"default","at":now,"data":{"rateLimits":{"limitId":"claude","primary":{"usedPercent":10.0,"resetsAt":now+1000.0}},"claudeRateLimit":{"status":"allowed"}}})
}
/// Bounds only a hang of a wait in this file; each wait ends at its own condition.
const HANG_BOUND: std::time::Duration = std::time::Duration::from_secs(60);
/// Open the state again as a restarted server does. Stopping the timer can cancel a fire
/// job after it submitted its last writer job. That job keeps the state lease until the
/// writer thread has run it (`SqliteActor::call_admitted_with_completion`), so the new
/// owner waits for the lease, as a new server process waits for the old one to exit.
async fn reopen(path: &std::path::Path) -> Arc<RuntimeStore> {
    let deadline = tokio::time::Instant::now() + HANG_BOUND;
    loop {
        match RuntimeStore::open(Some(path)).await {
            Ok(store) => return Arc::new(store),
            Err(StoreError::AlreadyOwned) if tokio::time::Instant::now() < deadline => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(error) => panic!("the state did not open again: {error}"),
        }
    }
}
async fn fixture(path: &std::path::Path, mode: Mode) -> (Arc<RuntimeStore>, Episode) {
    let store = Arc::new(RuntimeStore::open(Some(path)).await.unwrap());
    let episode=store.transaction(move|tx| {
        OrchestrationSettingsRepository::set_idle_compaction(tx,&Settings { mode,min_tokens:200000 })?;
        let mut agent=json!({"id":"claude","rootId":"claude","epoch":1,"events":0,"provider":"claude","status":"idle","autoWake":true,"inFlight":false,"cwd":"/work","accountKey":"default","threadId":"thread","startAttempt":{"id":"previous"}});
        idle::observe(tx,&mut agent,&json!({"requestId":"response","ttl":"1h","warm":true,"lastActivityAt":(idle::wall_now()-3240.0)*1000.0,"contextTokens":210000,"prefix":"prefix"}))?;
        AgentRepository::put(tx,&AgentRecord::from_value(agent.clone()).unwrap())?;
        Episode::deserialize(&agent["idleCompactionEpisode"]).map_err(|error|StoreError::SchemaMismatch(error.to_string()))
    }).await.unwrap();
    (store, episode)
}

#[tokio::test]
async fn episode_index_upgrade_preserves_deadlines_and_rolls_back_on_failure() {
    let root = tempfile::tempdir().unwrap();
    let (store, episode) = fixture(root.path(), Mode::Compact).await;
    let ddl: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='runtime_idle_compaction_deadline'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    drop(store);
    let db = rusqlite::Connection::open(root.path().join("canvas.sqlite3")).unwrap();
    db.execute_batch("DROP INDEX runtime_idle_compaction_deadline; ALTER TABLE runtime_agents DROP COLUMN idle_episode_type; ALTER TABLE runtime_agents DROP COLUMN idle_episode_fire_at; ALTER TABLE runtime_agents DROP COLUMN idle_episode_id; CREATE TABLE runtime_idle_compaction_deadline(id TEXT);").unwrap();
    drop(db);
    assert!(RuntimeStore::open(Some(root.path())).await.is_err());
    let db = rusqlite::Connection::open(root.path().join("canvas.sqlite3")).unwrap();
    let columns: i64 = db
        .query_row(
            "SELECT count(*) FROM pragma_table_xinfo('runtime_agents') WHERE name IN ('idle_episode_type','idle_episode_fire_at','idle_episode_id')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(columns, 0, "failed migration rolls back the column rebuild");
    db.execute_batch("DROP TABLE runtime_idle_compaction_deadline")
        .unwrap();
    drop(db);
    for _ in 0..2 {
        let store = reopen(root.path()).await;
        let current: String = store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name='runtime_idle_compaction_deadline'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(current, ddl, "fresh and migrated index DDL agree");
        assert_eq!(idle::next(&store).await.unwrap().unwrap().1.id, episode.id);
    }
}

#[tokio::test]
async fn five_minute_sleep_episode_uses_actual_ttl_and_unload_deadline() {
    let root = tempfile::tempdir().unwrap();
    let (store, _) = fixture(root.path(), Mode::Compact).await;
    store
        .transaction(|tx| {
            let mut agent = AgentRepository::get(tx, &AgentId::from("claude"))?
                .unwrap()
                .value();
            idle::observe(
                tx,
                &mut agent,
                &json!({"requestId":"short", "ttl":"5m", "warm":true,
            "lastActivityAt":100000.0,"contextTokens":150000,"prefix":"prefix"}),
            )?;
            let episode: Episode = Episode::deserialize(&agent["idleCompactionEpisode"]).unwrap();
            assert_eq!(episode.expires_at, 400.0);
            assert_eq!(episode.fire_at, 370.0);
            agent["idleCache"]["ttl"] = json!("5m");
            let settings = Settings {
                mode: Mode::Compact,
                min_tokens: 150000,
            };
            assert_eq!(
                idle::refusal(&settings, &episode, &agent, 370.0, true, false),
                None
            );
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn actual_unload_deadline_advances_one_episode_and_its_receipt_releases_unload() {
    use std::collections::BTreeMap;
    let root = tempfile::tempdir().unwrap();
    let (store, _) = fixture(root.path(), Mode::Log).await;
    let now = idle::wall_now();
    let episode = store
        .transaction(move |tx| {
            let mut agent = AgentRepository::get(tx, &AgentId::from("claude"))?
                .unwrap()
                .value();
            idle::observe(
                tx,
                &mut agent,
                &json!({"requestId":"warm", "ttl":"1h", "warm":true,
            "lastActivityAt":now*1000.0,"contextTokens":210000,"prefix":"prefix"}),
            )?;
            AgentRepository::put(tx, &AgentRecord::from_value(agent).unwrap())?;
            let deadlines = BTreeMap::from([("thread".into(), now + 120.0)]);
            idle::sync_unload_deadlines(tx, "default", &deadlines, now)?;
            let agent = AgentRepository::get(tx, &AgentId::from("claude"))?
                .unwrap()
                .value();
            let episode: Episode = Episode::deserialize(&agent["idleCompactionEpisode"]).unwrap();
            assert!((episode.fire_at - (now + 90.0)).abs() < 0.001);
            assert!(!idle::pending_before_unload(
                tx,
                "default",
                "thread",
                now + 89.0
            )?);
            assert!(idle::pending_before_unload(
                tx,
                "default",
                "thread",
                now + 90.0
            )?);
            assert!(!idle::pending_before_unload(
                tx,
                "other",
                "thread",
                now + 90.0
            )?);
            idle::sync_unload_deadlines(tx, "default", &deadlines, now + 5.0)?;
            assert_eq!(
                AgentRepository::get(tx, &AgentId::from("claude"))?
                    .unwrap()
                    .value(),
                agent
            );
            idle::sync_unload_deadlines(tx, "default", &BTreeMap::new(), now + 5.0)?;
            let reset = AgentRepository::get(tx, &AgentId::from("claude"))?
                .unwrap()
                .value();
            assert_eq!(reset["idleCompactionEpisode"]["id"], episode.id);
            assert!(
                (reset["idleCompactionEpisode"]["fireAt"].as_f64().unwrap() - (now + 3240.0)).abs()
                    < 0.001
            );
            idle::sync_unload_deadlines(
                tx,
                "default",
                &BTreeMap::from([("thread".into(), now)]),
                now,
            )?;
            let immediate = AgentRepository::get(tx, &AgentId::from("claude"))?
                .unwrap()
                .value();
            let immediate: Episode =
                Episode::deserialize(&immediate["idleCompactionEpisode"]).unwrap();
            assert_eq!(immediate.fire_at, now);
            Ok::<_, StoreError>(immediate)
        })
        .await
        .unwrap();
    let host = HostPort {
        store: store.clone(),
        session: Arc::new(Session::default()),
    };
    idle::fire(
        store.clone(),
        "claude".into(),
        episode,
        limits(now),
        now,
        &host,
    )
    .await
    .unwrap();
    assert!(host.session.calls.lock().unwrap().is_empty());
    store
        .read_async("sleep-unload-settled", move |connection| {
            assert!(!idle::pending_before_unload(
                connection, "default", "thread", now
            )?);
            let agent = AgentRepository::get(connection, &AgentId::from("claude"))?
                .unwrap()
                .value();
            assert!(
                agent.get("idleCompactionEpisode").is_none(),
                "the agent change wakes the blocked unload owner"
            );
            Ok::<_, StoreError>(())
        })
        .await
        .unwrap();
}

#[tokio::test]
async fn idle_claude_dispatches_once_and_restart_does_not_repeat() {
    let root = tempfile::tempdir().unwrap();
    let (store, episode) = fixture(root.path(), Mode::Compact).await;
    let session = Arc::new(Session::default());
    let host = HostPort {
        store: store.clone(),
        session: session.clone(),
    };
    assert_eq!(idle::next(&store).await.unwrap().unwrap().1.id, episode.id);
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let timer = tokio::spawn(idle::run(
        store.clone(),
        session.clone(),
        Arc::new(HostPort {
            store: store.clone(),
            session: session.clone(),
        }),
        stopped,
    ));
    tokio::time::timeout(HANG_BOUND, session.finished.notified())
        .await
        .unwrap();
    stop.send(true).unwrap();
    timer.await.unwrap();
    // Shutdown can cancel the final fire write after the provider notification.
    // Drain the writer actor before releasing its state-directory lock for restart.
    store
        .transaction(|_| Ok::<_, StoreError>(()))
        .await
        .unwrap();

    assert_eq!(session.calls.lock().unwrap().len(), 1);
    assert_eq!(
        session.calls.lock().unwrap()[0]["idleCompaction"]["requestId"],
        "response"
    );
    assert!(idle::next(&store).await.unwrap().is_none());
    drop(host);
    drop(store);
    let store = reopen(root.path()).await;
    let host = HostPort {
        store: store.clone(),
        session: session.clone(),
    };
    idle::fire(
        store.clone(),
        "claude".into(),
        episode.clone(),
        limits(idle::wall_now()),
        idle::wall_now(),
        &host,
    )
    .await
    .unwrap();
    assert!(idle::next(&store).await.unwrap().is_none());
    assert_eq!(session.calls.lock().unwrap().len(), 1);
    let receipt: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT outcome FROM runtime_native_action_receipts WHERE id=?1",
            [episode.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&receipt).unwrap()["status"],
        "acknowledged"
    );
}

#[tokio::test]
async fn a_dormant_account_worker_is_started_before_the_native_action() {
    // The account worker exits after its idle deadline. A native action (idle
    // compaction here, Compact or Review from /api/action through the same owner)
    // must start it again instead of answering "Not implemented".
    let root = tempfile::tempdir().unwrap();
    let (store, episode) = fixture(root.path(), Mode::Compact).await;
    let session = Arc::new(Session::default());
    session
        .dormant
        .store(true, std::sync::atomic::Ordering::SeqCst);
    let host = HostPort {
        store: store.clone(),
        session: session.clone(),
    };
    idle::fire(
        store.clone(),
        "claude".into(),
        episode,
        limits(idle::wall_now()),
        idle::wall_now(),
        &host,
    )
    .await
    .unwrap();
    assert_eq!(
        session.ensured.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "the worker is started once"
    );
    assert_eq!(
        session.calls.lock().unwrap().len(),
        1,
        "the compaction is sent"
    );
}

#[tokio::test]
async fn active_and_queued_agents_never_reach_the_provider() {
    for queued in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (store, episode) = fixture(root.path(), Mode::Compact).await;
        store.transaction(move|tx| {
            if queued { tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('input','claude','user','continue','pending',1,1)",[])?; }
            else { let mut value=AgentRepository::get(tx,&AgentId::from("claude"))?.unwrap().value();value["inFlight"]=json!(true);AgentRepository::put(tx,&AgentRecord::from_value(value).unwrap())?; }
            Ok::<_,StoreError>(())
        }).await.unwrap();
        let session = Arc::new(Session::default());
        let host = HostPort {
            store: store.clone(),
            session: session.clone(),
        };
        idle::fire(
            store.clone(),
            "claude".into(),
            episode.clone(),
            limits(idle::wall_now()),
            idle::wall_now(),
            &host,
        )
        .await
        .unwrap();
        assert!(session.calls.lock().unwrap().is_empty());
        let receipt: String = store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                [episode.id],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&receipt).unwrap()["outcome"],
            if queued { "queued_input" } else { "active" }
        );
    }
}

#[tokio::test]
async fn log_mode_has_a_durable_would_compact_receipt_and_sends_nothing() {
    let root = tempfile::tempdir().unwrap();
    let (store, episode) = fixture(root.path(), Mode::Log).await;
    let session = Arc::new(Session::default());
    let host = HostPort {
        store: store.clone(),
        session: session.clone(),
    };
    idle::fire(
        store.clone(),
        "claude".into(),
        episode.clone(),
        limits(idle::wall_now()),
        idle::wall_now(),
        &host,
    )
    .await
    .unwrap();
    assert!(session.calls.lock().unwrap().is_empty());
    assert!(idle::next(&store).await.unwrap().is_none());
    let receipt: String = store
        .test_reader()
        .unwrap()
        .query_row(
            "SELECT result FROM runtime_operation_receipts WHERE id=?1",
            [episode.id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Value>(&receipt).unwrap()["outcome"],
        "would_compact"
    );
}

struct ChangeDuringPreparation(bool);
impl native::Preparation for ChangeDuringPreparation {
    fn prepare<'a>(
        &'a self,
        store: &'a RuntimeStore,
        _port: &'a dyn AccountSessionPort,
        _request: &'a native::Request,
        _ticket: &'a studio_runtime_services::workspace_operations::ProviderTicket,
        _scope: &'a ConnectionScope,
    ) -> native::PreparationFuture<'a> {
        let queued = self.0;
        Box::pin(async move {
            store.transaction(move |tx| {
                if queued {
                    tx.execute("INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('racing-input','claude','user','continue','pending',1,1)",[])?;
                } else {
                    OrchestrationSettingsRepository::set_idle_compaction(tx,&Settings { mode:Mode::Log,min_tokens:200000 })?;
                }
                Ok::<_,StoreError>(())
            }).await?;
            Ok(())
        })
    }
}

#[tokio::test]
async fn input_during_preparation_refuses_handoff_and_restores_the_idle_agent() {
    let root = tempfile::tempdir().unwrap();
    let (store, episode) = fixture(root.path(), Mode::Compact).await;
    let session = Session::default();
    let request = native::Request {
        agent: "claude".into(),
        request_id: episode.id.clone(),
        action: Action::Compact,
        context: episode.identity.clone(),
    };
    let result = native::execute_provider_prepared(
        &store,
        &session,
        &request,
        idle::wall_now(),
        Arc::new(idle::IdleAdmission {
            inner: Arc::new(Gate),
            episode: episode.clone(),
            limits: limits(idle::wall_now()),
        }),
        Arc::new(studio_runtime_services::native_action_ports::NativeActionAcceptance),
        &ChangeDuringPreparation(true),
    )
    .await;
    assert!(matches!(
        result,
        Err(native::NativeError::Invalid("queued_input"))
    ));
    assert!(session.calls.lock().unwrap().is_empty());
    idle::release_unsubmitted(&store, "claude".into(), episode.id, "input arrived".into())
        .await
        .unwrap();
    let agent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from("claude"))
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(agent["status"], "idle");
    assert_eq!(agent["inFlight"], false);
    assert_eq!(agent["startAttempt"]["id"], "previous");
    assert_eq!(
        store
            .test_reader()
            .unwrap()
            .query_row(
                "SELECT status FROM runtime_events WHERE id='racing-input'",
                [],
                |row| row.get::<_, String>(0)
            )
            .unwrap(),
        "pending"
    );
}

#[tokio::test]
async fn compact_boundary_writes_the_notice_once_and_advances_the_roster_epoch() {
    for deferred in [false, true] {
        let root = tempfile::tempdir().unwrap();
        let (store, episode) = fixture(
            root.path(),
            if deferred {
                Mode::CompactOnReturn
            } else {
                Mode::Compact
            },
        )
        .await;
        let session = Arc::new(Session::default());
        let host = HostPort {
            store: store.clone(),
            session,
        };
        idle::fire(
            store.clone(),
            "claude".into(),
            episode.clone(),
            limits(idle::wall_now()),
            idle::wall_now(),
            &host,
        )
        .await
        .unwrap();
        assert_eq!(
            host.session.calls.lock().unwrap()[0]["idleCompaction"]["mode"],
            if deferred {
                "compact_on_return"
            } else {
                "compact"
            }
        );
        let id = episode.id.clone();
        store.transaction_with(move|tx| {
        let mut agent=AgentRepository::get(tx,&AgentId::from("claude"))?.unwrap().value();
        agent["turnId"]=json!("compact-turn");agent["status"]=json!("running");
        if deferred { agent["startAttempt"]=json!({"id":"return-turn"}); }
        AgentRepository::put(tx,&AgentRecord::from_value(agent).unwrap())?;
        let mut value=json!({"method":"item/completed","params":{"threadId":"thread","turnId":"compact-turn","item":{"id":"compact-boundary","type":"contextCompaction","status":"completed"}}});
        if deferred { value["params"]["item"]["idleCompactionRequestId"]=json!(id);value["params"]["item"]["idleCompactionOnReturn"]=json!(true); }
        let frame=match studio_runtime_providers::decode_journal_entry_borrowed(1, 1, format!("{value}\n").as_bytes()).payload { studio_runtime_providers::DecodedJournalPayload::Frame(frame) => frame, other @ studio_runtime_providers::DecodedJournalPayload::RawStdout { .. } => panic!("fixture must decode: {other:?}"), };
        for _ in 0..2 {
            studio_runtime_services::notification_dispatch::dispatch_notification(tx,"claude",
                &studio_runtime_services::notification_dispatch::PublicationContext::default(),
                &studio_runtime_services::notification_dispatch::NotificationStreamBuffer::default(),
                &studio_runtime_services::notification_dispatch::ProviderDeliveryIdentity {provider_handle:"account:default".into(),generation:1,cursor:1},&frame,idle::wall_now())?;
        }
        let agent=AgentRepository::get(tx,&AgentId::from("claude"))?.unwrap().value();
        assert_eq!(agent["compactions"],1);
        let notices:i64=tx.query_row("SELECT count(*) FROM runtime_items WHERE json_extract(record,'$.idleCompaction')=1 AND json_extract(record,'$.text')=?1",[if deferred {idle::ON_RETURN_NOTICE} else {idle::NOTICE}],|row|row.get(0)).map_err(StoreError::from)?;
        assert_eq!(notices,1);
        let receipt:String=tx.query_row("SELECT result FROM runtime_operation_receipts WHERE id=?1",[id],|row|row.get(0)).map_err(StoreError::from)?;
        assert_eq!(serde_json::from_str::<Value>(&receipt).unwrap()["outcome"],"compacted");
        Ok::<_,studio_runtime_services::notification_dispatch::NotificationDispatchError>(())
    }).await.unwrap();
    }
}

#[tokio::test]
async fn log_selected_during_preparation_cannot_dispatch_compaction() {
    let root = tempfile::tempdir().unwrap();
    let (store, episode) = fixture(root.path(), Mode::Compact).await;
    let session = Session::default();
    let request = native::Request {
        agent: "claude".into(),
        request_id: episode.id.clone(),
        action: Action::Compact,
        context: episode.identity.clone(),
    };
    let result = native::execute_provider_prepared(
        &store,
        &session,
        &request,
        idle::wall_now(),
        Arc::new(idle::IdleAdmission {
            inner: Arc::new(Gate),
            episode: episode.clone(),
            limits: limits(idle::wall_now()),
        }),
        Arc::new(studio_runtime_services::native_action_ports::NativeActionAcceptance),
        &ChangeDuringPreparation(false),
    )
    .await;
    assert!(matches!(
        result,
        Err(native::NativeError::Invalid("would_compact"))
    ));
    assert!(session.calls.lock().unwrap().is_empty());
    idle::release_unsubmitted(&store, "claude".into(), episode.id, "mode changed".into())
        .await
        .unwrap();
    let agent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from("claude"))
        .unwrap()
        .unwrap()
        .value();
    assert_eq!(agent["status"], "idle");
    assert_eq!(agent["inFlight"], false);
}

#[tokio::test]
async fn cache_notification_replaces_the_timer_without_changing_token_accounting() {
    let root = tempfile::tempdir().unwrap();
    let (store, episode) = fixture(root.path(), Mode::Compact).await;
    let old = episode.id;
    store.transaction_with(move|tx| {
        let mut agent=AgentRepository::get(tx,&AgentId::from("claude"))?.unwrap().value();
        agent["tokensUsed"]=json!(123);AgentRepository::put(tx,&AgentRecord::from_value(agent).unwrap())?;
        for (cursor,ttl) in [(1,"1h"),(2,"5m"),(3,"unknown")] {
            let value=json!({"method":"thread/idleCompaction/updated","params":{"threadId":"thread","report":{
                "requestId":"next-response","ttl":ttl,"warm":true,"lastActivityAt":idle::wall_now()*1000.0,"contextTokens":220000,"prefix":"prefix"}}});
            let frame=match studio_runtime_providers::decode_journal_entry_borrowed(1, 1, format!("{value}\n").as_bytes()).payload { studio_runtime_providers::DecodedJournalPayload::Frame(frame) => frame, other @ studio_runtime_providers::DecodedJournalPayload::RawStdout { .. } => panic!("fixture must decode: {other:?}"), };
            studio_runtime_services::notification_dispatch::dispatch_notification(tx,"claude",
                &studio_runtime_services::notification_dispatch::PublicationContext::default(),
                &studio_runtime_services::notification_dispatch::NotificationStreamBuffer::default(),
                &studio_runtime_services::notification_dispatch::ProviderDeliveryIdentity {provider_handle:"account:default".into(),generation:1,cursor},&frame,idle::wall_now())?;
            let agent=AgentRepository::get(tx,&AgentId::from("claude"))?.unwrap().value();
            assert_eq!(agent["tokensUsed"],123);
            if ttl!="unknown" { assert_ne!(agent["idleCompactionEpisode"]["id"],old); }
            else { assert!(agent.get("idleCompactionEpisode").is_none()); }
        }
        Ok::<_,studio_runtime_services::notification_dispatch::NotificationDispatchError>(())
    }).await.unwrap();
    assert!(idle::next(&store).await.unwrap().is_none());
}

#[tokio::test]
async fn default_mode_arms_leads_only_and_an_explicit_mode_wins() {
    let root = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(root.path())).await.unwrap());
    let armed = |lead: bool| {
        let store = store.clone();
        async move {
            store.transaction(move |tx| {
                let mut agent = json!({"id":"claude","rootId":"claude","epoch":1,"events":0,"provider":"claude","status":"idle","autoWake":true,"inFlight":false,"cwd":"/work","accountKey":"default","threadId":"thread","isLead":lead,"startAttempt":{"id":"previous"}});
                idle::observe(tx,&mut agent,&json!({"requestId":"response","ttl":"1h","warm":true,"lastActivityAt":(idle::wall_now()-3240.0)*1000.0,"contextTokens":210000,"prefix":"prefix"}))?;
                Ok(agent["idleCompactionEpisode"].is_object())
            }).await.unwrap()
        }
    };
    // No stored setting: the default applies.
    assert!(armed(true).await, "a lead is armed by default");
    assert!(!armed(false).await, "a worker is off by default");
    // Explicit Off wins for a lead.
    store
        .transaction(|tx| {
            OrchestrationSettingsRepository::set_idle_compaction(
                tx,
                &Settings {
                    mode: Mode::Off,
                    min_tokens: 200000,
                },
            )
        })
        .await
        .unwrap();
    assert!(!armed(true).await, "explicit Off stays Off for a lead");
    // Explicit Compact wins for a worker.
    store
        .transaction(|tx| {
            OrchestrationSettingsRepository::set_idle_compaction(
                tx,
                &Settings {
                    mode: Mode::Compact,
                    min_tokens: 200000,
                },
            )
        })
        .await
        .unwrap();
    assert!(armed(false).await, "explicit Compact arms a worker");
}
