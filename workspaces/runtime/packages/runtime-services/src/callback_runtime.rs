//! Retained-session adapter for native cards. A port receives only committed
//! replies and must enforce the supplied deadline; losing its receipt leaves
//! the exact attempt Unknown.
use std::{future::Future, pin::Pin, sync::Arc, time::Duration};

use serde_json::{Value, json};
use studio_runtime_providers::{Frame, RequestId};
use studio_runtime_store::RuntimeStore;

use crate::provider_callbacks::{
    self as callbacks, CallbackContext, CallbackDecision, CallbackError, DeliveryOutcome, Reply,
    ReplyIdentity,
};

/// HTTP-to-worker committed-reply mailbox, distinct from Python's incoming callback queue.
const MAX_QUEUED_REPLIES: usize = 128;
pub const REPLY_DEADLINE: Duration = Duration::from_secs(5);
/// How often the server looks for approvals whose expiry timer fired. The
/// shortest timer is minutes long, so seconds of delay do not matter.
const EXPIRY_TICK: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReplyOutcome {
    Written,
    NotSent,
    Unknown,
}

pub struct ReplyReceipt {
    pub identity: ReplyIdentity,
    pub outcome: ReplyOutcome,
}

pub type CallbackReplyFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ReplyReceipt, CallbackError>> + Send + 'a>>;

/// The account owner checks connection AND generation immediately before
/// submission, preserves identity.attempt in the transport operation, and
/// returns Written only from its transport receipt. Timeout is Unknown, never
/// NotSent. Implementations must not wait on the worker's own event-loop thread.
pub trait CallbackReplyPort: Send + Sync {
    /// Read-only preflight used before preparing a native-question delete.
    /// Implementations must verify the exact account connection generation.
    fn has_live_session(&self, context: &CallbackContext) -> bool;
    fn deliver<'a>(&'a self, reply: &'a Reply, deadline: Duration) -> CallbackReplyFuture<'a>;
}

#[derive(Default)]
struct LifecycleGate(tokio::sync::Mutex<()>);

impl LifecycleGate {
    async fn with<T, F, Fut>(&self, action: F) -> Result<T, CallbackError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, CallbackError>>,
    {
        let _guard = self.0.lock().await;
        action().await
    }
}

pub struct CallbackRuntime {
    store: Arc<RuntimeStore>,
    port: Arc<dyn CallbackReplyPort>,
    lifecycle: LifecycleGate,
}
impl CallbackRuntime {
    pub fn new(store: Arc<RuntimeStore>, port: Arc<dyn CallbackReplyPort>) -> Self {
        Self {
            store,
            port,
            lifecycle: LifecycleGate::default(),
        }
    }

    /// Shared by the retained owner for resolution, stop, delete, completion
    /// and transport acceptance. The gate spans the async operation so a
    /// competing lifecycle transition cannot overtake its committed receipt.
    pub async fn with_lifecycle<T, F, Fut>(&self, action: F) -> Result<T, CallbackError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Result<T, CallbackError>>,
    {
        self.lifecycle.with(action).await
    }

    /// Run on the retained owner immediately before bounded submit_frame admission.
    /// The submit closure checks the exact live connection/generation under the gate.
    /// The read transaction ends BEFORE submit; the lifecycle gate spans both.
    /// None means a resolution/completion proof cancelled this queued reply.
    pub async fn submit_committed<T, Fut>(
        &self,
        reply: &Reply,
        submit: impl FnOnce() -> Fut,
    ) -> Result<Option<T>, CallbackError>
    where
        Fut: Future<Output = Result<T, CallbackError>>,
    {
        self.with_lifecycle(|| async {
            if !callbacks::submission_ready(&self.store, reply).await? {
                return Ok(None);
            }
            submit().await.map(Some)
        })
        .await
    }

    pub async fn dispatch(
        &self,
        context: &CallbackContext,
        frame: &Frame,
        now: f64,
    ) -> Result<CallbackDecision, CallbackError> {
        callbacks::dispatch(Arc::clone(&self.store), context, frame, now).await
    }

    /// Commit Submitted before entering the port. False means a Written or
    /// retired receipt already proves completion; the caller only owes its ACK.
    pub async fn deliver(&self, reply: &Reply) -> Result<bool, CallbackError> {
        let Some(receipt) = self
            .submit_committed(reply, || async {
                if !callbacks::delivery(&self.store, &reply.identity, DeliveryOutcome::Submitted)
                    .await?
                {
                    return Ok(None);
                }
                self.port.deliver(reply, REPLY_DEADLINE).await.map(Some)
            })
            .await?
        else {
            return Ok(false);
        };
        let Some(receipt) = receipt else {
            return Ok(false);
        };
        let outcome = if receipt.identity == reply.identity {
            receipt.outcome
        } else {
            ReplyOutcome::Unknown
        };
        callbacks::delivery(
            &self.store,
            &reply.identity,
            match outcome {
                ReplyOutcome::Written => DeliveryOutcome::Written,
                ReplyOutcome::NotSent => DeliveryOutcome::NotSent,
                ReplyOutcome::Unknown => DeliveryOutcome::Unknown,
            },
        )
        .await?;
        match outcome {
            ReplyOutcome::Written => Ok(true),
            ReplyOutcome::NotSent => Err(CallbackError::Invalid(
                "The provider rejected the reply before submission",
            )),
            ReplyOutcome::Unknown => Err(CallbackError::Unknown),
        }
    }

    pub async fn approve_full_access_pending(
        &self,
        root: &str,
        now: f64,
    ) -> Result<(), CallbackError> {
        let root = root.to_owned();
        let ids = self.store.read_async::<_, CallbackError>("runtime-services/callback_runtime/permissions", move |db| {
            let mut query = db.prepare(
                "SELECT r.id FROM approvals r JOIN runtime_agents a ON a.id=r.agent
                 WHERE r.state='pending' AND r.kind IN ('exec','patch','permissions')
                 AND json_extract(a.record,'$.yoloMode')=1 AND json_extract(a.record,'$.provider')='codex'
                 AND (a.id=?1 OR json_extract(a.record,'$.rootId')=?1)"
            )?;
            Ok(query.query_map([root], |row| row.get::<_, String>(0))?.collect::<Result<Vec<_>,_>>()?)
        }).await?;
        // Use the user-click path independently for each card; one failure does not undo Full.
        for id in ids {
            if let Err(error) = self.answer(&id, &json!({"decision":"accept"}), now).await {
                eprintln!("Approval remains available through its user card: {error}");
            }
        }
        Ok(())
    }

    /// Decline the approvals whose expiry timer fired (a Claude agent in full
    /// access mode that nobody answered). The decline is committed to the
    /// outbox with the expiry, then delivered like any other reply. A reply
    /// that cannot be delivered now stays in the outbox.
    pub async fn expire_due(&self, now: f64) -> Result<usize, CallbackError> {
        let replies = self
            .with_lifecycle(|| callbacks::expire_due(&self.store, now))
            .await?;
        for reply in &replies {
            if let Err(error) = self.deliver(reply).await {
                eprintln!("expired approval decline is not delivered: {error}");
            }
        }
        Ok(replies.len())
    }

    /// Check the expiry timers until shutdown. The timers are rows, so a restart
    /// keeps them.
    pub async fn run_expiry(self: Arc<Self>, mut shutdown: tokio::sync::watch::Receiver<bool>) {
        loop {
            tokio::select! {
                _ = shutdown.wait_for(|stopped| *stopped) => return,
                _ = tokio::time::sleep(EXPIRY_TICK) => {}
            }
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0.0, |elapsed| elapsed.as_secs_f64());
            if let Err(error) = self.expire_due(now).await {
                eprintln!("approval expiry check failed: {error}");
            }
        }
    }

    pub async fn answer(&self, id: &str, data: &Value, now: f64) -> Result<Value, CallbackError> {
        if let Some(context) = callbacks::pending_native_delete_context(&self.store, id).await?
            && !self.port.has_live_session(&context)
        {
            return Err(CallbackError::Dependency(
                "provider-session/question-answer",
            ));
        }
        self.answer_committed(id, data, now).await
    }

    /// Answer through a caller that has independently verified the exact live
    /// account connection and generation. The callback mailbox itself has no
    /// registry view, so its generic live-session probe is intentionally false.
    pub async fn answer_on_live_session(
        &self,
        id: &str,
        data: &Value,
        now: f64,
        live_context: &CallbackContext,
    ) -> Result<Value, CallbackError> {
        if let Some(context) = callbacks::pending_native_delete_context(&self.store, id).await?
            && context != *live_context
        {
            return Err(CallbackError::Stale);
        }
        self.answer_committed(id, data, now).await
    }

    async fn answer_committed(
        &self,
        id: &str,
        data: &Value,
        now: f64,
    ) -> Result<Value, CallbackError> {
        match callbacks::prepare_user_answer(&self.store, id, data, now, false).await? {
            Some(reply) => {
                self.deliver(&reply).await?;
                Ok(json!({"status":"answered"}))
            }
            None => Ok(json!({"status":"answered","replayed":true})),
        }
    }

    pub async fn delete_question(&self, id: &str, now: f64) -> Result<Value, CallbackError> {
        if let Some(context) = callbacks::pending_native_delete_context(&self.store, id).await?
            && !self.port.has_live_session(&context)
        {
            return Err(CallbackError::Dependency(
                "provider-session/question-answer",
            ));
        }
        if let Some(reply) = self
            .with_lifecycle(|| callbacks::prepare_delete(&self.store, id, now))
            .await?
        {
            self.deliver(&reply).await?;
        }
        Ok(json!({"id":id,"status":"deleted"}))
    }

    pub async fn resolve(
        &self,
        context: &CallbackContext,
        rpc: &RequestId,
        now: f64,
    ) -> Result<usize, CallbackError> {
        self.with_lifecycle(|| callbacks::resolve(&self.store, context, rpc, now))
            .await
    }

    pub async fn recover_connection(
        &self,
        account: &str,
        connection: &str,
        now: f64,
    ) -> Result<usize, CallbackError> {
        self.with_lifecycle(|| callbacks::recover_connection(&self.store, account, connection, now))
            .await
    }
}

/// Bounded HTTP-to-worker bridge. The retained account owner drains Receiver
/// after wake and sends the receipt only after its exact transport outcome.
/// There is no detached task and timeout never frees an attempt for retry.
pub struct CallbackMailbox {
    sender: std::sync::mpsc::SyncSender<CommittedReply>,
    wake: Arc<dyn Fn() + Send + Sync>,
}
pub struct CommittedReply {
    pub reply: Reply,
    pub receipt: tokio::sync::oneshot::Sender<ReplyReceipt>,
}
impl CallbackMailbox {
    pub fn new(
        wake: Arc<dyn Fn() + Send + Sync>,
    ) -> (Arc<Self>, std::sync::mpsc::Receiver<CommittedReply>) {
        let (sender, receiver) = std::sync::mpsc::sync_channel(MAX_QUEUED_REPLIES);
        (Arc::new(Self { sender, wake }), receiver)
    }
}
impl CallbackReplyPort for CallbackMailbox {
    fn has_live_session(&self, _: &CallbackContext) -> bool {
        false
    }

    fn deliver<'a>(&'a self, reply: &'a Reply, deadline: Duration) -> CallbackReplyFuture<'a> {
        Box::pin(async move {
            let (receipt, receiver) = tokio::sync::oneshot::channel();
            if self
                .sender
                .try_send(CommittedReply {
                    reply: reply.clone(),
                    receipt,
                })
                .is_err()
            {
                return Ok(ReplyReceipt {
                    identity: reply.identity.clone(),
                    outcome: ReplyOutcome::NotSent,
                });
            }
            (self.wake)();
            tokio::time::timeout(deadline.min(REPLY_DEADLINE), receiver)
                .await
                .map_err(|_| CallbackError::Unknown)?
                .map_err(|_| CallbackError::Unknown)
        })
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use std::sync::Mutex;
    struct FakeReplyPort {
        store: Arc<RuntimeStore>,
        outcome: ReplyOutcome,
        delivered: Mutex<Vec<Reply>>,
    }

    impl CallbackReplyPort for FakeReplyPort {
        fn has_live_session(&self, _: &CallbackContext) -> bool {
            true
        }

        fn deliver<'a>(&'a self, reply: &'a Reply, _: Duration) -> CallbackReplyFuture<'a> {
            Box::pin(async move {
                let submitted: i64 = self.store.test_reader()?.query_row(
                    "SELECT count(*) FROM runtime_outbox WHERE kind='provider_callback' AND status='submitted'",
                    [],
                    |row| row.get(0),
                )?;
                assert_eq!(submitted, 1, "durable intent commits before provider I/O");
                self.delivered.lock().unwrap().push(reply.clone());
                Ok(ReplyReceipt {
                    identity: reply.identity.clone(),
                    outcome: self.outcome,
                })
            })
        }
    }

    async fn pending_native_question() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
        let actor = json!({"compactions":0,"provider":studio_runtime_schema::ProviderKind::Codex,
            "id":"delete-agent", "rootId":"delete-agent", "epoch":0,
            "status":"running", "autoWake":true, "isLead":true,
            "accountKey":"default", "threadId":"thread", "turnId":"turn",
        });
        let card = json!({
            "id":"native-question", "agent":"delete-agent", "epoch":0,
            "method":"item/tool/requestUserInput", "status":"pending",
            "createdAt":1.0, "accountKey":"default", "connectionId":"connection",
            "supervisor":{"generation":1}, "rpcId":"rpc-question",
            "params":{"questions":[{"id":"value","question":"Value?"}],
                      "threadId":"thread","turnId":"turn"},
        });
        let identity = crate::provider_callbacks::ReplyIdentity {
            context: crate::provider_callbacks::CallbackContext {
                agent: "delete-agent".into(),
                account: "default".into(),
                stream_id: "default".into(),
                connection: "connection".into(),
                generation: 1,
                epoch: 0,
                thread: "thread".into(),
                turn: "turn".into(),
            },
            rpc_id: studio_runtime_providers::RequestId::String("rpc-question".into()),
            attempt: String::new(),
        };

        store
            .transaction_with::<_, CallbackError>(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params!["delete-agent", actor.to_string()],
                )?;
                crate::provider_callbacks::legacy_view::insert(tx, &card)?;
                studio_runtime_store::receipt_schema::create(tx)?;
                crate::provider_callbacks::storage::seed_card_owner(tx, &identity, &card)?;
                Ok(())
            })
            .await
            .unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn deleting_native_question_uses_durable_provider_answer_and_never_resends_unknown() {
        // Python codex_questions.py:86-90 answers pending native questions on
        // delete; codex_runtime.py:11300-11304 sends the empty answers object.
        for (outcome, expected_status) in [
            (ReplyOutcome::Written, "deleted"),
            (ReplyOutcome::Unknown, "uncertain"),
        ] {
            let (_directory, store) = pending_native_question().await;
            let port = Arc::new(FakeReplyPort {
                store: store.clone(),
                outcome,
                delivered: Mutex::new(Vec::new()),
            });
            let runtime = CallbackRuntime::new(store.clone(), port.clone());
            let first = runtime.delete_question("native-question", 10.0).await;
            if outcome == ReplyOutcome::Written {
                assert_eq!(
                    first.unwrap(),
                    json!({"id":"native-question","status":"deleted"})
                );
            } else {
                assert!(matches!(first, Err(CallbackError::Unknown)));
            }

            {
                let delivered = port.delivered.lock().unwrap();
                assert_eq!(delivered.len(), 1);
                assert_eq!(
                    delivered[0].body,
                    studio_runtime_providers::ResponseBody::Result(json!({"answers":{}}),)
                );
            }

            if outcome == ReplyOutcome::Unknown {
                assert!(matches!(
                    runtime.delete_question("native-question", 11.0).await,
                    Err(CallbackError::Invalid(
                        "Question delivery is uncertain; wait for its receipt"
                    ))
                ));
                assert_eq!(port.delivered.lock().unwrap().len(), 1);
            }

            let request = crate::provider_callbacks::legacy_view::stored(
                &store.test_reader().unwrap(),
                "native-question",
            )
            .unwrap();
            assert_eq!(request["status"], expected_status);
            let attempts: i64 = store
                .test_reader()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM runtime_outbox WHERE kind='provider_callback'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                attempts,
                i64::from(outcome == ReplyOutcome::Unknown),
                "unknown answers stay durable; written answers retire"
            );
            if outcome == ReplyOutcome::Written {
                let results: i64 = store
                    .test_reader()
                    .unwrap()
                    .query_row("SELECT count(*) FROM request_results", [], |row| row.get(0))
                    .unwrap();
                assert!(results > 0, "the written answer has a durable result");
            }
        }
    }

    /// One lead agent of `provider` in full access mode, and the context of its turn.
    async fn full_access_agent(
        provider: studio_runtime_schema::ProviderKind,
    ) -> (tempfile::TempDir, Arc<RuntimeStore>, CallbackContext) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
        let actor = json!({"provider":provider, "yoloMode":true,
            "id":"full-agent", "rootId":"full-agent", "epoch":0,
            "status":"running", "autoWake":true, "isLead":true, "inFlight":true,
            "accountKey":"default", "threadId":"thread", "turnId":"turn",
        });
        store
            .transaction_with::<_, CallbackError>(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params!["full-agent", actor.to_string()],
                )?;
                studio_runtime_store::receipt_schema::create(tx)?;
                Ok(())
            })
            .await
            .unwrap();
        let context = CallbackContext {
            agent: "full-agent".into(),
            account: "default".into(),
            stream_id: "default".into(),
            connection: "connection".into(),
            generation: 1,
            epoch: 0,
            thread: "thread".into(),
            turn: "turn".into(),
        };
        (directory, store, context)
    }

    fn command_approval() -> Frame {
        Frame::Request(studio_runtime_providers::RequestFrame {
            id: RequestId::String("approval-1".into()),
            method: studio_runtime_providers::Method::ItemCommandExecutionRequestApproval,
            params: Some(
                json!({"threadId":"thread","turnId":"turn","itemId":"toolu_bash",
                "command":"rm -rf target","cwd":"/project","reason":"Claude requests permission"}),
            ),
            fields: Default::default(),
        })
    }

    #[tokio::test]
    async fn a_claude_approval_in_full_access_is_declined_by_the_server_when_nobody_answers() {
        let (_directory, store, context) =
            full_access_agent(studio_runtime_schema::ProviderKind::Claude).await;
        let admitted = callbacks::admit_native(store.clone(), &context, &command_approval(), 100.0)
            .await
            .unwrap();
        let callbacks::NativeAdmission::Card(id) = admitted else {
            panic!("a Claude approval waits as a card");
        };
        let reader = store.test_reader().unwrap();
        let row = studio_runtime_store::approvals::get(
            &reader,
            &studio_runtime_schema::approvals::ApprovalId(id.clone()),
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            row.fire_at,
            Some(studio_runtime_schema::approvals::Timestamp(
                100.0 + callbacks::CLAUDE_FULL_ACCESS_APPROVAL_SECONDS
            )),
            "the timer is a row, so it survives a restart"
        );

        let port = Arc::new(FakeReplyPort {
            store: store.clone(),
            outcome: ReplyOutcome::Written,
            delivered: Mutex::new(Vec::new()),
        });
        let runtime = CallbackRuntime::new(store.clone(), port.clone());
        assert_eq!(runtime.expire_due(699.0).await.unwrap(), 0, "not due yet");
        assert!(port.delivered.lock().unwrap().is_empty());
        assert_eq!(runtime.expire_due(701.0).await.unwrap(), 1);
        {
            let delivered = port.delivered.lock().unwrap();
            assert_eq!(delivered.len(), 1);
            assert_eq!(
                delivered[0].body,
                studio_runtime_providers::ResponseBody::Result(json!({"decision":"decline"}))
            );
        }
        let reader = store.test_reader().unwrap();
        let card = crate::provider_callbacks::legacy_view::stored(&reader, &id).unwrap();
        assert_eq!(card["status"], "expired");
        let actor = studio_runtime_store::agent_volatile::merged_record(&reader, "full-agent")
            .unwrap()
            .unwrap();
        assert_eq!(actor["status"], "running", "the agent waits no more");
        assert_eq!(runtime.expire_due(702.0).await.unwrap(), 0, "once");
        assert_eq!(port.delivered.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_codex_approval_in_full_access_is_a_row_answered_by_the_policy() {
        use studio_runtime_schema::approvals::{Answer, ApprovalState, Decision, Principal};
        let (_directory, store, context) =
            full_access_agent(studio_runtime_schema::ProviderKind::Codex).await;
        let frame = command_approval();
        let mut bodies = Vec::new();
        for now in [100.0, 101.0] {
            let admitted = callbacks::admit_native(store.clone(), &context, &frame, now)
                .await
                .unwrap();
            let callbacks::NativeAdmission::Answer(body) = admitted else {
                panic!("full access answers a Codex approval without the user");
            };
            bodies.push(body);
        }
        assert_eq!(
            bodies[0],
            studio_runtime_providers::ResponseBody::Result(json!({"decision":"accept"}))
        );
        assert_eq!(bodies[0], bodies[1], "a repeat answers the same");
        let rows = studio_runtime_store::approvals::all(&store.test_reader().unwrap()).unwrap();
        assert_eq!(rows.len(), 1, "a repeat writes no second row");
        assert!(matches!(
            &rows[0].state,
            ApprovalState::Answered {
                answer: Answer::Decision {
                    decision: Decision::Accept
                },
                by: Principal::Policy,
                ..
            }
        ));
    }

    #[tokio::test]
    async fn lifecycle_gate_is_held_during_the_action() {
        let gate = LifecycleGate::default();
        gate.with(|| async {
            assert!(gate.0.try_lock().is_err());
            Ok(())
        })
        .await
        .unwrap();
        assert!(gate.0.try_lock().is_ok());
    }
}
