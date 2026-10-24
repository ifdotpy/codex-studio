//! Manual worker recovery and tool release through the retained account owner.
use crate::account_session_runtime::AccountSessionPort;
use crate::managed_host_tools::{Actor, DispatchedOperation, Error};
use serde_json::Value;
use std::sync::Arc;
use studio_runtime_store::RuntimeStore;

pub(crate) async fn execute(
    _store: Arc<RuntimeStore>,
    _session: Arc<dyn AccountSessionPort>,
    _actor: &Actor,
    _args: &Value,
    _request_id: &str,
    _now: f64,
) -> Result<DispatchedOperation, Error> {
    Err(Error::Unavailable("native-release/tool-reset"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::account_session_runtime::{
        AccountSessionFuture, ConnectionScope, ProviderRequest, ProviderResponse, RequestCertainty,
        SessionReceipt,
    };
    use serde_json::json;
    use std::sync::Mutex;
    use studio_runtime_providers::{Method, RequestId, RequestTicket};
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository};

    struct Session {
        store: Arc<RuntimeStore>,
        calls: Mutex<Vec<(Method, String)>>,
        unknown: bool,
        unloaded: bool,
        stop_lead: bool,
        terminal: bool,
    }
    impl Session {
        fn scope() -> ConnectionScope {
            ConnectionScope {
                account: "default".into(),
                connection: "connection".into(),
                generation: 1,
            }
        }
        fn unsubscribes(&self) -> usize {
            self.calls
                .lock()
                .expect("calls")
                .iter()
                .filter(|(method, _)| *method == Method::ThreadUnsubscribe)
                .count()
        }
    }
    impl AccountSessionPort for Session {
        fn ensure_session<'a>(&'a self, _: &'a str) -> AccountSessionFuture<'a, ConnectionScope> {
            Box::pin(async { Ok(Self::scope()) })
        }
        fn current_scope(&self, _: &str) -> Option<ConnectionScope> {
            Some(Self::scope())
        }
        fn callback_barrier<'a>(
            &'a self,
            _: &'a str,
            _: &'a ConnectionScope,
            _: std::time::Instant,
        ) -> AccountSessionFuture<'a, ()> {
            Box::pin(async { Ok(()) })
        }
        fn provider_request<'a>(
            &'a self,
            request: ProviderRequest,
        ) -> AccountSessionFuture<'a, SessionReceipt> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("calls")
                    .push((request.method, request.operation.clone()));
                if self.stop_lead && request.method == Method::ThreadQueueList {
                    self.store
                        .transaction(|tx| {
                            let mut lead =
                                AgentRepository::get(tx, &AgentId::from("lead-00000000"))?
                                    .expect("lead");
                            lead.epoch += 1;
                            lead.extra.insert("autoWake".into(), json!(false));
                            AgentRepository::put(tx, &lead)
                        })
                        .await
                        .expect("stop lead");
                }
                let body = match request.method {
                    Method::ThreadRead => {
                        json!({"thread":{"id":"worker-thread","status":{"type":if self.unloaded {"notLoaded"} else {"idle"}}}})
                    }
                    Method::ThreadBackgroundTerminalsList if self.terminal => {
                        json!({"data":[{"id":"live-command"}],"nextCursor":null})
                    }
                    Method::ThreadBackgroundTerminalsList | Method::ThreadQueueList => {
                        json!({"data":[],"nextCursor":null})
                    }
                    Method::ThreadUnsubscribe => json!({"status":"unsubscribed"}),
                    method => panic!("unexpected native mutation or read: {method:?}"),
                };
                let unknown = self.unknown && request.method == Method::ThreadUnsubscribe;
                Ok(SessionReceipt {
                    scope: Self::scope(),
                    operation: request.operation.clone(),
                    ticket: Some(RequestTicket {
                        id: RequestId::String(request.operation),
                        generation: 1,
                        method: request.method,
                    }),
                    certainty: if unknown {
                        RequestCertainty::Unknown
                    } else {
                        RequestCertainty::Written
                    },
                    result: (!unknown).then_some(ProviderResponse::Reply(body)),
                    not_sent_reason: None,
                    replayed: false,
                })
            })
        }
    }
    async fn fixture() -> (tempfile::TempDir, Arc<RuntimeStore>, Actor, Session) {
        let directory = tempfile::tempdir().expect("temporary state");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        let lead = json!({"id":"lead-00000000","rootId":"lead-00000000","parentId":null,"epoch":1,
            "status":"running","isLead":true,"autoWake":true,"inFlight":true,"threadId":"lead-thread",
            "turnId":"lead-turn","accountKey":"default","provider":"codex"});
        let worker = json!({"id":"worker-00000000","rootId":"lead-00000000","parentId":"lead-00000000","epoch":3,
            "status":"completed","isLead":false,"autoWake":true,"inFlight":false,"threadId":"worker-thread",
            "turnId":null,"accountKey":"default","provider":"codex","executionMode":"native","activeTools":[]});
        let actor = Actor::from_record(&lead, "lead-00000000", 1).expect("actor");
        store
            .transaction(move |tx| {
                for value in [lead, worker] {
                    AgentRepository::put(tx, &AgentRecord::from_value(value)?)?;
                }
                Ok(())
            })
            .await
            .expect("seed team");
        let session = Session {
            store: Arc::clone(&store),
            calls: Mutex::new(Vec::new()),
            unknown: false,
            unloaded: false,
            stop_lead: false,
            terminal: false,
        };
        (directory, store, actor, session)
    }
    fn args() -> Value {
        json!({"action":"reset_tools","agent_id":"worker-00000000","reason":"Release stale tool memory"})
    }
    async fn worker(store: &RuntimeStore) -> Value {
        store
            .read_async("manual-recovery-test", |db| {
                Ok::<_, studio_runtime_store::StoreError>(AgentRepository::get(db, &AgentId::from("worker-00000000"))?
                    .expect("worker")
                    .value())
            })
            .await
            .expect("worker read")
    }

    #[tokio::test]
    async fn manual_tool_reset_unsubscribes_once_and_reports_unconfirmed_closure() {
        let (_directory, store, actor, session) = fixture().await;
        let session = Arc::new(session);
        let first = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "reset-1",
            10.0,
        )
        .await
        .expect("manual reset");
        assert_eq!(first.result.value["status"], "released");
        assert!(
            first.result.value["loss"]
                .as_str()
                .expect("loss warning")
                .contains("JavaScript")
        );
        assert!(
            first.result.value["loss"]
                .as_str()
                .expect("loss warning")
                .contains("MCP")
        );
        let current = worker(&store).await;
        assert_eq!(current["nativeRelease"]["resetPending"], true);
        assert!(
            current["nativeRelease"]["closedAt"].is_null(),
            "unsubscribe is not closure"
        );
        let replay = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "reset-1",
            11.0,
        )
        .await
        .expect("lost response replay");
        assert_eq!(first.result.value, replay.result.value);
        assert_eq!(session.unsubscribes(), 1);
        let mut changed = args();
        changed["reason"] = json!("Changed request content");
        assert!(
            execute(store, session.clone(), &actor, &changed, "reset-1", 12.0)
                .await
                .is_err()
        );
        assert_eq!(session.unsubscribes(), 1);
    }

    #[tokio::test]
    async fn an_unknown_manual_tool_reset_is_not_sent_again() {
        let (_directory, store, actor, mut session) = fixture().await;
        session.unknown = true;
        let session = Arc::new(session);
        let first = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "unknown-1",
            10.0,
        )
        .await
        .expect("unknown result");
        assert_eq!(
            first.certainty,
            crate::managed_host_tools::DeliveryCertainty::Unknown
        );
        assert_eq!(first.result.value["status"], "unknown");
        for request in ["unknown-1", "unknown-2"] {
            let result = execute(
                Arc::clone(&store),
                session.clone(),
                &actor,
                &args(),
                request,
                11.0,
            )
            .await
            .expect("inspect uncertain release");
            assert_eq!(result.result.value["status"], "unknown");
        }
        assert_eq!(session.unsubscribes(), 1);
        assert_eq!(worker(&store).await["nativeRelease"]["resetPending"], true);
    }

    #[tokio::test]
    async fn manual_tool_reset_rechecks_the_lead_after_native_reads() {
        let (_directory, store, actor, mut session) = fixture().await;
        session.stop_lead = true;
        let session = Arc::new(session);
        let result = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "stopped-lead",
            10.0,
        )
        .await;
        assert!(result.is_err() || result.expect("result").result.value["status"] == "blocked");
        assert_eq!(session.unsubscribes(), 0);
    }

    #[tokio::test]
    async fn manual_tool_reset_keeps_native_commands_and_uncertain_input() {
        for native_command in [false, true] {
            let (_directory, store, actor, mut session) = fixture().await;
            session.terminal = native_command;
            if !native_command {
                store.transaction(|tx| {
                    tx.execute("INSERT INTO runtime_events(id,agent,epoch,kind,text,status,created) VALUES('unknown-input','worker-00000000',3,'followup','original input','uncertain',1)", [])?;
                    Ok(())
                }).await.expect("uncertain original input");
            }
            let session = Arc::new(session);
            let result = execute(
                Arc::clone(&store),
                session.clone(),
                &actor,
                &args(),
                "blocked-reset",
                10.0,
            )
            .await
            .expect("blocked reset");
            assert_eq!(result.result.value["status"], "blocked");
            assert_eq!(session.unsubscribes(), 0);
        }
    }

    #[tokio::test]
    async fn a_confirmed_native_closure_needs_no_unsubscribe() {
        let (_directory, store, actor, mut session) = fixture().await;
        session.unloaded = true;
        let session = Arc::new(session);
        let result = execute(
            Arc::clone(&store),
            session.clone(),
            &actor,
            &args(),
            "already-closed",
            10.0,
        )
        .await
        .expect("confirmed closure");
        assert_eq!(result.result.value["status"], "released");
        let current = worker(&store).await;
        assert_eq!(current["nativeRelease"]["resetPending"], false);
        assert_eq!(current["nativeRelease"]["closedAt"], 10.0);
        assert_eq!(session.unsubscribes(), 0);
    }
}
