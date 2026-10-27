//! Transport delivery uses the canonical ProviderCallback obligation.
use super::retained::{PortError, RetainedSessionPort};
use super::*;
use crate::recovery_history::ConnectionScope;
use tokio::sync::oneshot;

pub struct FrameCommand {
    scope: ConnectionScope,
    operation: String,
    frame: Frame,
    deadline: std::time::Instant,
    reply: oneshot::Sender<Result<WriteOutcome, PortError>>,
}
pub(super) struct PendingFrame {
    key: String,
    reply: oneshot::Sender<Result<WriteOutcome, PortError>>,
}
impl RetainedSessionPort {
    /// Whether this live session is the provider process that sent the
    /// callback of `context`: the same account and provider generation. A
    /// replacement worker attaches to that same process with a new connection
    /// id (a backend restart in supervisor mode), and the process still waits
    /// for the answer, so the connection id is not part of the match. Another
    /// generation is another process that never sent the request.
    pub fn serves_callback(&self, context: &crate::provider_callbacks::CallbackContext) -> bool {
        self.is_live()
            && self.scope.account == context.account
            && self.scope.generation == context.generation
    }

    /// Write one committed callback reply on this session and return its
    /// typed transport receipt. `CallbackRuntime` commits Submitted first.
    pub async fn deliver_callback_reply(
        &self,
        reply: &crate::provider_callbacks::Reply,
        deadline: std::time::Duration,
    ) -> Result<crate::callback_runtime::ReplyReceipt, crate::provider_callbacks::CallbackError>
    {
        use crate::callback_runtime::{ReplyOutcome, ReplyReceipt};
        let identity = reply.identity.clone();
        if !self.serves_callback(&identity.context) {
            return Ok(ReplyReceipt {
                identity,
                outcome: ReplyOutcome::NotSent,
            });
        }
        let outcome = self
            .submit_committed_frame(
                self.scope(),
                identity.operation_id(),
                reply.frame(),
                std::time::Instant::now() + deadline,
            )
            .await;
        let outcome = match outcome {
            Ok(WriteOutcome::Written) => ReplyOutcome::Written,
            Ok(WriteOutcome::NotSent) => ReplyOutcome::NotSent,
            Ok(WriteOutcome::Unknown) | Err(PortError::Unknown) => ReplyOutcome::Unknown,
            Err(PortError::Identity | PortError::NotSent | PortError::Deadline) => {
                ReplyOutcome::NotSent
            }
            Err(PortError::Capacity) => {
                return Err(crate::provider_callbacks::CallbackError::Invalid(
                    "Callback response queue is full",
                ));
            }
            Err(_) => return Err(crate::provider_callbacks::CallbackError::Unknown),
        };
        Ok(ReplyReceipt { identity, outcome })
    }

    /// The canonical callback owner must commit Submitted before this call.
    /// Repeated operation identities return the saved receipt and never resend.
    pub async fn submit_committed_frame(
        &self,
        scope: &ConnectionScope,
        operation: String,
        frame: Frame,
        deadline: std::time::Instant,
    ) -> Result<WriteOutcome, PortError> {
        if scope != self.scope() || !self.is_live() {
            return Err(PortError::Identity);
        }
        if !matches!(frame, Frame::Response { .. })
            || operation.is_empty()
            || operation.len() > 4096
        {
            return Err(PortError::Invalid);
        }
        let at = tokio::time::Instant::from_std(deadline);
        if at <= tokio::time::Instant::now() {
            return Ok(WriteOutcome::NotSent);
        }
        let (reply, receipt) = oneshot::channel();
        tokio::time::timeout_at(
            at,
            self.commands
                .send(AccountCommand::CommittedFrame(Box::new(FrameCommand {
                    scope: scope.clone(),
                    operation,
                    frame,
                    deadline,
                    reply,
                }))),
        )
        .await
        .map_err(|_| PortError::NotSent)?
        .map_err(|_| PortError::NotSent)?;
        tokio::time::timeout_at(at, receipt)
            .await
            .map_err(|_| PortError::Unknown)?
            .map_err(|_| PortError::Unknown)?
    }
}
impl Worker {
    pub(super) async fn committed_frame(
        &mut self,
        command: FrameCommand,
    ) -> Result<(), WorkerError> {
        let FrameCommand {
            scope,
            operation,
            frame,
            deadline,
            reply,
        } = command;
        if scope.account != self.account
            || scope.generation != self.generation.0
            || scope.connection != self.connection_id
        {
            let _ignored = reply.send(Err(PortError::Identity));
            return Ok(());
        }
        if reply.is_closed()
            || tokio::time::Instant::from_std(deadline) <= tokio::time::Instant::now()
        {
            let _ignored = reply.send(Ok(WriteOutcome::NotSent));
            return Ok(());
        }
        let key = operation.clone();
        let prepared = match self.control.prepare_frame(OperationId(key.clone()), &frame) {
            Ok(prepared) => prepared,
            Err(_) => {
                let _ignored = reply.send(Err(PortError::Invalid));
                return Ok(());
            }
        };
        let stream = self.account.clone();
        let generation = self.generation.0;
        let replay = self
            .db("retained_frame_intent", move |store| async move {
                store
                    .transaction_with(move |tx| {
                        crate::provider_callbacks::begin_frame(
                            tx, &stream, generation, &operation, &frame,
                        )
                        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
                    })
                    .await
            })
            .await?;
        match replay {
            crate::provider_callbacks::FrameAdmission::Send => {}
            crate::provider_callbacks::FrameAdmission::Written => {
                let _ignored = reply.send(Ok(WriteOutcome::Written));
                return Ok(());
            }
            crate::provider_callbacks::FrameAdmission::Unknown => {
                let _ignored = reply.send(Ok(WriteOutcome::Unknown));
                return Ok(());
            }
        }
        self.retained_frames.insert(
            key.clone(),
            PendingFrame {
                key: key.clone(),
                reply,
            },
        );
        if self.closing || tokio::time::Instant::from_std(deadline) <= tokio::time::Instant::now() {
            self.retained_frame_outcome(&key, WriteOutcome::NotSent)
                .await?;
            return Ok(());
        }
        let control = self.control.clone();
        let request = async {
            tokio::time::timeout_at(
                tokio::time::Instant::from_std(deadline),
                control.submit_frame(prepared),
            )
            .await
            .unwrap_or(Err(SessionTaskError::AdmissionUnknown))
        };
        if let Err(error) = self.drive(request).await {
            self.retained_frame_outcome(
                &key,
                if definite_rejection(&error) {
                    WriteOutcome::NotSent
                } else {
                    WriteOutcome::Unknown
                },
            )
            .await?;
        }
        Ok(())
    }
    pub(super) async fn retained_frame_outcome(
        &mut self,
        operation: &str,
        outcome: WriteOutcome,
    ) -> Result<bool, WorkerError> {
        let Some(pending) = self.retained_frames.remove(operation) else {
            return Ok(false);
        };
        self.db("retained_frame_written", move |s| async move {
            s.transaction_with(move |tx| {
                let delivery = match outcome {
                    WriteOutcome::Written => crate::provider_callbacks::DeliveryOutcome::Written,
                    WriteOutcome::NotSent => crate::provider_callbacks::DeliveryOutcome::NotSent,
                    WriteOutcome::Unknown => crate::provider_callbacks::DeliveryOutcome::Unknown,
                };
                crate::provider_callbacks::record_frame_outcome(tx, &pending.key, delivery)
                    .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
            })
            .await
        })
        .await?;
        let _ignored = pending.reply.send(Ok(outcome));
        Ok(true)
    }
}
