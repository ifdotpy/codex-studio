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
    fn deliver(&self, reply: &Reply, deadline: Duration) -> Result<ReplyReceipt, CallbackError>;
}

pub struct CallbackRuntime {
    store: Arc<RuntimeStore>,
    port: Arc<dyn CallbackReplyPort>,
    lifecycle: Mutex<()>,
}
impl CallbackRuntime {
    pub fn new(store: Arc<RuntimeStore>, port: Arc<dyn CallbackReplyPort>) -> Self {
        Self {
            store,
            port,
            lifecycle: Mutex::new(()),
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
        let _guard = self.lifecycle.lock().map_err(|_| CallbackError::Unknown)?;
        action()
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
        if !callbacks::delivery(&self.store, &reply.identity, DeliveryOutcome::Submitted)? {
            return Ok(false);
        }
        let receipt = self.port.deliver(reply, REPLY_DEADLINE);
        let outcome = match receipt {
            Ok(receipt) if receipt.identity == reply.identity => receipt.outcome,
            _ => ReplyOutcome::Unknown,
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
        match callbacks::prepare_user_answer(&self.store, id, data, now, false)? {
            Some(reply) => {
                self.deliver(&reply)?;
                Ok(json!({"status":"answered"}))
            }
            None => Ok(json!({"status":"answered","replayed":true})),
        }
    }

    pub fn delete_question(&self, id: &str, now: f64) -> Result<Value, CallbackError> {
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
