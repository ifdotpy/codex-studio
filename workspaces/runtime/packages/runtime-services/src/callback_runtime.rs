//! Retained-session adapter for native cards. All methods run on a blocking
//! store bridge. A port receives only committed replies and must enforce the
//! supplied deadline; losing its receipt leaves the exact attempt Unknown.
use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

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

/// The account owner checks connection AND generation immediately before
/// submission, preserves identity.attempt in the transport operation, and
/// returns Written only from its transport receipt. Timeout is Unknown, never
/// NotSent. Implementations must not wait on the worker's own event-loop thread.
pub trait CallbackReplyPort: Send + Sync {
    /// Read-only preflight used before preparing a native-question delete.
    /// Implementations must verify the exact account connection generation.
    fn has_live_session(&self, context: &CallbackContext) -> bool;
    fn deliver(&self, reply: &Reply, deadline: Duration) -> Result<ReplyReceipt, CallbackError>;
}

#[derive(Default)]
struct LifecycleGate(Mutex<()>);

impl LifecycleGate {
    fn with<T>(
        &self,
        action: impl FnOnce() -> Result<T, CallbackError>,
    ) -> Result<T, CallbackError> {
        let _guard = self.0.lock().map_err(|_| CallbackError::Unknown)?;
        action()
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
    /// and transport acceptance. Run on the blocking bridge; never await the
    /// HTTP mailbox receipt or final write receipt here. Bound acceptance by
    /// REPLY_DEADLINE; the closure must return before the owner handles events.
    pub fn with_lifecycle<T>(
        &self,
        action: impl FnOnce() -> Result<T, CallbackError>,
    ) -> Result<T, CallbackError> {
        self.lifecycle.with(action)
    }

    /// Run on the retained owner immediately before bounded submit_frame admission.
    /// The submit closure checks the exact live connection/generation under the gate.
    /// The read transaction ends BEFORE submit; the lifecycle gate spans both.
    /// None means a resolution/completion proof cancelled this queued reply.
    pub fn submit_committed<T>(
        &self,
        reply: &Reply,
        submit: impl FnOnce() -> Result<T, CallbackError>,
    ) -> Result<Option<T>, CallbackError> {
        self.with_lifecycle(|| {
            if !callbacks::submission_ready(&self.store, reply)? {
                return Ok(None);
            }
            submit().map(Some)
        })
    }

    pub fn dispatch(
        &self,
        context: &CallbackContext,
        frame: &Frame,
        now: f64,
    ) -> Result<CallbackDecision, CallbackError> {
        callbacks::dispatch(&self.store, context, frame, now)
    }

    /// Commit Submitted before entering the port. False means a Written or
    /// retired receipt already proves completion; the caller only owes its ACK.
    pub fn deliver(&self, reply: &Reply) -> Result<bool, CallbackError> {
        let Some(receipt) = self.submit_committed(reply, || {
            if !callbacks::delivery(&self.store, &reply.identity, DeliveryOutcome::Submitted)? {
                return Ok(None);
            }
            self.port.deliver(reply, REPLY_DEADLINE).map(Some)
        })?
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
        )?;
        if outcome == ReplyOutcome::Written {
            self.store.transaction_with(|tx| {
                callbacks::retire_checkpointed(
                    tx,
                    &reply.identity.context.account,
                    reply.identity.context.generation,
                )
            })?;
        }
        match outcome {
            ReplyOutcome::Written => Ok(true),
            ReplyOutcome::NotSent => Err(CallbackError::Invalid(
                "The provider rejected the reply before submission",
            )),
            ReplyOutcome::Unknown => Err(CallbackError::Unknown),
        }
    }

    pub fn answer(&self, id: &str, data: &Value, now: f64) -> Result<Value, CallbackError> {
        if let Some(context) = callbacks::pending_native_delete_context(&self.store, id)?
            && !self.port.has_live_session(&context)
        {
            return Err(CallbackError::Dependency(
                "provider-session/question-answer",
            ));
        }
        self.answer_committed(id, data, now)
    }

    /// Answer through a caller that has independently verified the exact live
    /// account connection and generation. The callback mailbox itself has no
    /// registry view, so its generic live-session probe is intentionally false.
    pub fn answer_on_live_session(
        &self,
        id: &str,
        data: &Value,
        now: f64,
        live_context: &CallbackContext,
    ) -> Result<Value, CallbackError> {
        if let Some(context) = callbacks::pending_native_delete_context(&self.store, id)?
            && context != *live_context
        {
            return Err(CallbackError::Stale);
        }
        self.answer_committed(id, data, now)
    }

    fn answer_committed(&self, id: &str, data: &Value, now: f64) -> Result<Value, CallbackError> {
        match callbacks::prepare_user_answer(&self.store, id, data, now, false)? {
            Some(reply) => {
                self.deliver(&reply)?;
                Ok(json!({"status":"answered"}))
            }
            None => Ok(json!({"status":"answered","replayed":true})),
        }
    }

    pub fn delete_question(&self, id: &str, now: f64) -> Result<Value, CallbackError> {
        if let Some(context) = callbacks::pending_native_delete_context(&self.store, id)?
            && !self.port.has_live_session(&context)
        {
            return Err(CallbackError::Dependency(
                "provider-session/question-answer",
            ));
        }
        if let Some(reply) =
            self.with_lifecycle(|| callbacks::prepare_delete(&self.store, id, now))?
        {
            self.deliver(&reply)?;
        }
        Ok(json!({"id":id,"status":"deleted"}))
    }

    pub fn resolve(
        &self,
        context: &CallbackContext,
        rpc: &RequestId,
        now: f64,
    ) -> Result<usize, CallbackError> {
        self.with_lifecycle(|| callbacks::resolve(&self.store, context, rpc, now))
    }

    pub fn recover_connection(
        &self,
        account: &str,
        connection: &str,
        now: f64,
    ) -> Result<usize, CallbackError> {
        self.with_lifecycle(|| callbacks::recover_connection(&self.store, account, connection, now))
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
    pub receipt: std::sync::mpsc::SyncSender<ReplyReceipt>,
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

    fn deliver(&self, reply: &Reply, deadline: Duration) -> Result<ReplyReceipt, CallbackError> {
        let (receipt, receiver) = std::sync::mpsc::sync_channel(1);
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
        receiver
            .recv_timeout(deadline.min(REPLY_DEADLINE))
            .map_err(|_| CallbackError::Unknown)
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    struct FakeReplyPort {
        store: Arc<RuntimeStore>,
        outcome: ReplyOutcome,
        delivered: Mutex<Vec<Reply>>,
    }

    impl CallbackReplyPort for FakeReplyPort {
        fn has_live_session(&self, _: &CallbackContext) -> bool {
            true
        }

        fn deliver(&self, reply: &Reply, _: Duration) -> Result<ReplyReceipt, CallbackError> {
            let submitted: i64 = self.store.read()?.query_row(
                "SELECT count(*) FROM runtime_operation_receipts \
                 WHERE signature='provider-callback' \
                   AND json_extract(result,'$.delivery')='submitted'",
                [],
                |row| row.get(0),
            )?;
            assert_eq!(submitted, 1, "durable intent commits before provider I/O");
            self.delivered.lock().unwrap().push(reply.clone());
            Ok(ReplyReceipt {
                identity: reply.identity.clone(),
                outcome: self.outcome,
            })
        }
    }

    fn pending_native_question() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).unwrap());
        let actor = json!({
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
        store
            .transaction_with::<_, CallbackError>(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    rusqlite::params!["delete-agent", actor.to_string()],
                )?;
                tx.execute(
                    "INSERT INTO runtime_requests(id,record) VALUES(?1,?2)",
                    rusqlite::params!["native-question", card.to_string()],
                )?;
                Ok(())
            })
            .unwrap();
        (directory, store)
    }

    #[test]
    fn deleting_native_question_uses_durable_provider_answer_and_never_resends_unknown() {
        // Python codex_questions.py:86-90 answers pending native questions on
        // delete; codex_runtime.py:11300-11304 sends the empty answers object.
        for (outcome, expected_status) in [
            (ReplyOutcome::Written, "deleted"),
            (ReplyOutcome::Unknown, "uncertain"),
        ] {
            let (_directory, store) = pending_native_question();
            let port = Arc::new(FakeReplyPort {
                store: store.clone(),
                outcome,
                delivered: Mutex::new(Vec::new()),
            });
            let runtime = CallbackRuntime::new(store.clone(), port.clone());
            let first = runtime.delete_question("native-question", 10.0);
            if outcome == ReplyOutcome::Written {
                assert_eq!(
                    first.unwrap(),
                    json!({"id":"native-question","status":"deleted"})
                );
            } else {
                assert!(matches!(first, Err(CallbackError::Unknown)));
            }

            let delivered = port.delivered.lock().unwrap();
            assert_eq!(delivered.len(), 1);
            assert_eq!(
                delivered[0].body,
                studio_runtime_providers::ResponseBody::Result(json!({"answers":{}}),)
            );
            drop(delivered);

            if outcome == ReplyOutcome::Unknown {
                assert!(matches!(
                    runtime.delete_question("native-question", 11.0),
                    Err(CallbackError::Invalid(
                        "Question delivery is uncertain; wait for its receipt"
                    ))
                ));
                assert_eq!(port.delivered.lock().unwrap().len(), 1);
            }

            let request: String = store
                .read()
                .unwrap()
                .query_row(
                    "SELECT record FROM runtime_requests WHERE id='native-question'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&request).unwrap()["status"],
                expected_status
            );
            let attempts: i64 = store
                .read()
                .unwrap()
                .query_row(
                    "SELECT count(*) FROM runtime_operation_receipts WHERE signature='provider-callback'",
                    [],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(attempts, 1, "the answer intent is durable");
        }
    }

    #[test]
    fn lifecycle_gate_is_held_during_the_action() {
        let gate = LifecycleGate::default();
        gate.with(|| {
            std::thread::scope(|scope| {
                scope
                    .spawn(|| assert!(gate.0.try_lock().is_err()))
                    .join()
                    .unwrap();
            });
            Ok(())
        })
        .unwrap();
        assert!(gate.0.try_lock().is_ok());
    }
}
