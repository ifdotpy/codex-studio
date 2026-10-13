//! Bounded, typed attachment boundary. The session owns protocol initialization
//! and durable settlement; receiving an entry never acknowledges it.
use std::{future::Future, pin::Pin};

use serde_json::Value;
pub use studio_runtime_providers::InitializationStatus;
use studio_runtime_providers::{CodecError, Frame, encode_line};
use tokio::{
    sync::{mpsc, oneshot, watch},
    task::JoinHandle,
};

use crate::{EventCursor, Generation, OperationId, ProviderHandle, ProviderLaunchSpec};

/// Each lane has at most 16 serialized MiB, plus one bounded in-flight RPC.
/// Decoded JSON has proportional allocation overhead. No replay batch is retained.
pub const LANE_CAPACITY: usize = 16;
pub const MAX_PENDING_RESPONSES: usize = 32;

/// Shared monotonic epoch, injected into an attachment and its session.
/// Tokio's clock makes arrival/deadline tests independent of wall time.
#[derive(Clone)]
pub struct SessionClock(tokio::time::Instant);
impl Default for SessionClock {
    fn default() -> Self {
        Self(tokio::time::Instant::now())
    }
}
impl SessionClock {
    pub fn from_origin(origin: tokio::time::Instant) -> Self {
        Self(origin)
    }
    pub fn now_ms(&self) -> u64 {
        self.millis_at(tokio::time::Instant::now())
    }
    pub(crate) fn millis_at(&self, at: tokio::time::Instant) -> u64 {
        u64::try_from(at.saturating_duration_since(self.0).as_millis()).unwrap_or(u64::MAX)
    }
    pub fn instant_at(&self, milliseconds: u64) -> tokio::time::Instant {
        self.0 + std::time::Duration::from_millis(milliseconds)
    }
}

pub struct JournalEntry {
    pub received_at_ms: u64,
    pub cursor: EventCursor,
    pub generation: Generation,
    pub payload: JournalPayload,
}

pub enum JournalPayload {
    Frame(Frame),
    RawStdout {
        bytes: Vec<u8>,
        error: CodecError,
    },
    Stderr(Value),
    Exit(Value),
    /// Preserve future supervisor event kinds with their settlement identity.
    Other {
        kind: String,
        payload: Value,
    },
}

/// Prepared before bounded command admission (and before a caller commits its
/// Submitted marker). Private fields prevent arbitrarily large queued writes.
pub struct PreparedWrite {
    pub(crate) operation_id: OperationId,
    pub(crate) message: Value,
}

impl PreparedWrite {
    pub fn message(&self) -> &Value {
        &self.message
    }

    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    pub fn new(operation_id: OperationId, frame: &Frame) -> Result<Self, AdapterError> {
        let bytes = encode_line(frame).map_err(|_| AdapterError::InvalidConfiguration)?;
        let message =
            serde_json::from_slice(&bytes).map_err(|_| AdapterError::InvalidConfiguration)?;
        let prepared = Self {
            operation_id,
            message,
        };
        // Leave space for the supervisor handle, request ID and envelope.
        if prepared.operation_id.0.is_empty()
            || prepared.operation_id.0.len() > 1024
            || bytes.len() + prepared.operation_id.0.len() + 1024
                > crate::transport::MAX_FRAME_BYTES
        {
            return Err(AdapterError::InvalidConfiguration);
        }
        Ok(prepared)
    }
}

pub enum AdapterCommand {
    Write {
        write: PreparedWrite,
        reply: oneshot::Sender<CommandReceipt>,
        observed: oneshot::Receiver<()>,
    },
    /// Only the durable consumer may authorize a contiguous prefix.
    Ack {
        cursor: EventCursor,
        reply: oneshot::Sender<CommandReceipt>,
    },
}

impl AdapterCommand {
    pub fn write(write: PreparedWrite) -> (Self, Receipt) {
        let (reply, receiver) = oneshot::channel();
        let (observation, observed) = oneshot::channel();
        (
            Self::Write {
                write,
                reply,
                observed,
            },
            Receipt {
                receiver,
                observation: Some(observation),
            },
        )
    }

    pub fn ack(cursor: EventCursor) -> (Self, Receipt) {
        let (reply, receiver) = oneshot::channel();
        (
            Self::Ack { cursor, reply },
            Receipt {
                receiver,
                observation: None,
            },
        )
    }

    pub(crate) fn reject(self) {
        let reply = match self {
            Self::Write { reply, .. } | Self::Ack { reply, .. } => reply,
        };
        let _ignored = reply.send(CommandReceipt::NotSent);
    }
}

pub struct Receipt {
    receiver: oneshot::Receiver<CommandReceipt>,
    observation: Option<oneshot::Sender<()>>,
}
impl Receipt {
    /// A lost receipt is uncertainty, including task panic. Never resubmit it.
    pub async fn receive(mut self) -> CommandReceipt {
        let result = (&mut self.receiver)
            .await
            .unwrap_or(CommandReceipt::Unknown);
        if let Some(observation) = self.observation.take() {
            let _ignored = observation.send(());
        }
        result
    }

    #[cfg(test)]
    pub(crate) async fn take_buffered_without_observing(&mut self) -> CommandReceipt {
        (&mut self.receiver)
            .await
            .unwrap_or(CommandReceipt::Unknown)
    }
}

#[derive(Clone)]
pub enum CommandReceipt {
    Written {
        remote_id: Option<u64>,
        duplicate: bool,
    },
    Acknowledged(EventCursor),
    /// Reserved control result: never sits behind the replay channel.
    CachedInitialize(Value),
    NotSent,
    Unknown,
}

/// Sticky and independent of both bounded lanes. Clones can only set shutdown.
#[derive(Clone)]
pub struct Shutdown(watch::Sender<bool>);
impl Default for Shutdown {
    fn default() -> Self {
        Self(watch::channel(false).0)
    }
}
impl Shutdown {
    pub async fn cancelled(stop: &mut watch::Receiver<bool>) {
        loop {
            if *stop.borrow_and_update() || stop.changed().await.is_err() {
                return;
            }
        }
    }
    pub fn cancel(&self) {
        self.0.send_replace(true);
    }
    pub fn subscribe(&self) -> watch::Receiver<bool> {
        self.0.subscribe()
    }
}

#[derive(Clone, Debug)]
pub struct AttachmentInfo {
    pub handle: ProviderHandle,
    pub generation: Generation,
    pub pid: u32,
    pub acknowledged: EventCursor,
    pub resumed: bool,
}

#[derive(Clone, Debug)]
pub struct AttachmentStatus {
    pub handle: ProviderHandle,
    pub open_attempted: bool,
    pub info: Option<AttachmentInfo>,
    pub initialization: InitializationStatus,
}

impl AttachmentStatus {
    /// Derived from current readiness, including uncertain open before metadata.
    pub fn initialization_recovery(&self) -> Option<InitializationRecovery> {
        (self.open_attempted && self.initialization != InitializationStatus::Ready).then(|| {
            InitializationRecovery {
                handle: self.handle.clone(),
                generation: self.info.as_ref().map(|info| info.generation),
                phase: self.initialization,
            }
        })
    }
}

#[derive(Clone, Debug)]
pub struct InitializationRecovery {
    pub handle: ProviderHandle,
    pub generation: Option<Generation>,
    pub phase: InitializationStatus,
}

#[derive(Debug)]
pub struct TaskExit {
    pub result: Result<(), AdapterError>,
    /// Some means socket cleanup finished but protocol cleanup did not. Persist
    /// this exact handle/generation and do not manufacture a successful close.
    pub recovery_needed: Option<InitializationRecovery>,
}

#[derive(Debug, thiserror::Error)]
pub enum AdapterError {
    #[error("invalid provider attachment configuration")]
    InvalidConfiguration,
    #[error("provider attachment cancelled")]
    Cancelled,
    #[error("provider attachment operation timed out")]
    Timeout,
    #[error("retained provider identity could not be verified")]
    UnverifiableChild,
    #[error("provider attachment consumer closed")]
    ConsumerClosed,
    #[error("provider task failed")]
    TaskFailed,
    #[error(transparent)]
    Supervisor(#[from] crate::SupervisorError),
}

pub struct ProviderAttachment {
    pub commands: mpsc::Sender<AdapterCommand>,
    pub journal: mpsc::Receiver<JournalEntry>,
    pub status: watch::Receiver<AttachmentStatus>,
    pub(crate) shutdown: Shutdown,
    pub(crate) task: Option<JoinHandle<TaskExit>>,
}

impl ProviderAttachment {
    /// Construct another bounded implementation or a deterministic test factory.
    /// The task must own its socket and drop the status sender on completion;
    /// sessions can select `status.changed()` alongside journal reception and
    /// call `completion()` when that watch closes, even with a full journal.
    pub fn from_task(
        commands: mpsc::Sender<AdapterCommand>,
        journal: mpsc::Receiver<JournalEntry>,
        status: watch::Receiver<AttachmentStatus>,
        shutdown: Shutdown,
        task: JoinHandle<TaskExit>,
    ) -> Self {
        Self {
            commands,
            journal,
            status,
            shutdown,
            task: Some(task),
        }
    }

    /// Returns only after the task has dropped its socket. Completion never
    /// enters the journal lane, so a full consumer cannot deadlock shutdown.
    pub async fn shutdown(&mut self) -> TaskExit {
        self.shutdown.cancel();
        self.completion().await
    }

    pub async fn completion(&mut self) -> TaskExit {
        let result = match self.task.as_mut() {
            Some(task) => task.await.unwrap_or_else(|_| TaskExit {
                result: Err(AdapterError::TaskFailed),
                recovery_needed: self.status.borrow().initialization_recovery(),
            }),
            None => TaskExit {
                result: Err(AdapterError::TaskFailed),
                recovery_needed: None,
            },
        };
        self.task.take();
        result
    }
}

impl Drop for ProviderAttachment {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Launch resolution runs at the caller's bounded blocking boundary. The async
/// factory never runs an arbitrary synchronous resolver on a runtime worker.
pub struct OpenOptions {
    pub clock: SessionClock,
    pub launch: ProviderLaunchSpec,
    /// Persisted identity from a previous attachment, if available.
    pub expected_generation: Option<Generation>,
}

pub type OpenFuture<'a> =
    Pin<Box<dyn Future<Output = Result<ProviderAttachment, AdapterError>> + Send + 'a>>;

/// Object safe without async-trait. Returns before socket startup; observe
/// `status` and `completion` while draining replay and sending session commands.
pub trait ProviderTaskFactory: Send + Sync {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> OpenFuture<'_>;
}

/// Stable Python-compatible identity for the session's initialized notification.
pub fn initialized_operation(handle: &ProviderHandle, generation: Generation) -> OperationId {
    crate::provider_shared::initialized_operation_id(
        &crate::SupervisorHandle(handle.as_str().into()),
        generation,
    )
}
