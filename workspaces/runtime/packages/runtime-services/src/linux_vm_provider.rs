//! Retained Linux-VM provider sessions over the existing guest RPC channel.
//!
//! The guest's native supervisor owns provider process identity and its event
//! journal. This host adapter owns only the exact start identity and settlement
//! cursor; it does not create another socket or receipt protocol.

/// The guest home of a VM agent whose record has no `layrHome`.
pub(crate) const DEFAULT_GUEST_HOME: &str = "/home/studio";

use crate::event_write::{replay_request, settle_request};
use crate::linux_vm_guest::{GUEST_RPC_TIMEOUT_SECONDS, GuestError, GuestRpc};
use base64::Engine as _;
use rusqlite::Transaction;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::Path,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};
use studio_runtime_process::{
    EventCursor, Generation, ProviderHandle, ProviderLaunchSpec,
    provider_seam::{
        AdapterCommand, AdapterError, AttachmentInfo, AttachmentStatus, CommandReceipt,
        InitializationStatus, JournalEntry, JournalPayload, LANE_CAPACITY, OpenFuture, OpenOptions,
        ProviderAttachment, ProviderTaskFactory, SessionClock, Shutdown, TaskExit,
    },
};
use studio_runtime_providers::{DecodedJournalPayload, decode_journal_value};
use studio_runtime_schema::persistence::{
    OutboxKind, OutboxStatus, OwnerKind, RequestIdentity, WorkEffectStatus,
};
use studio_runtime_schema::shared_values::{
    GUEST_PROVIDER_READ_RETRY_SECONDS, LINUX_VM_PROVIDER_HANDLE_PREFIX,
};
pub use studio_runtime_store::StartIdentity;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, CredentialCopy, ExecutionHistoryRepository,
    RuntimeStore, StoreError, VmAuthBootstrap, VmLaunch,
    outbox::{Outbox, OutboxError, OutboxRow},
    request_results::{AdmittedRequest, RequestResultError},
    sha256_hex as hex_digest,
};
use thiserror::Error;

const START_REQUEST_ID_MAX_BYTES: usize = 128;
/// Maximum provider start socket exchange. Two guest setup commands can each
/// take 120 seconds; the remaining budget covers native startup and transport.
/// An expired exchange stays unknown and never permits a second start.
const PROVIDER_START_TIMEOUT: Duration = Duration::from_secs(300);
const PROVIDER_RPC_TIMEOUT: Duration = Duration::from_secs(15);
const EVENT_PAYLOAD_MAX_BYTES: u64 = 256 * 1024 * 1024;
const EVENT_CHUNK_MAX_BYTES: u64 = 1024 * 1024;
const CREDENTIAL_RECEIPT_PREFIX: &str = "linux-vm-credential-copy:";
pub const MAX_GUEST_CREDENTIAL_BYTES: usize = 1024 * 1024;

/// One owner for the durable actor binding shared by VM journal, credential,
/// and launch paths. The account connection generation is fenced separately by
/// its retained-session scope.
pub fn vm_actor_matches(
    connection: &rusqlite::Connection,
    agent_id: &str,
    epoch: i64,
    account_key: &str,
    provider: Option<&str>,
) -> std::result::Result<bool, StoreError> {
    let Some(record) = AgentRepository::get(connection, &AgentId::from(agent_id))? else {
        return Ok(false);
    };
    Ok(record.epoch == epoch
        && record.extra.get("executionMode").and_then(Value::as_str) == Some("vm")
        && provider.is_none_or(|provider| {
            studio_runtime_providers::ProviderKind::of_field(record.extra.get("provider"))
                .is_ok_and(|kind| kind.key() == provider)
        })
        && record
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .unwrap_or("default")
            == account_key
        && !record
            .extra
            .get("deletedAt")
            .is_some_and(|value| !value.is_null())
        && record.extra.get("deleted") != Some(&json!(true)))
}

/// Actor-scoped destination for an allow-listed credential document copy.
#[derive(Clone, Copy)]
pub struct GuestCredentialTarget<'a> {
    pub agent_id: &'a str,
    pub actor_epoch: i64,
    pub provider: &'a str,
    pub account_key: &'a str,
    pub relative_path: &'a str,
}

#[derive(Clone)]
pub struct GuestProviderIdentity {
    pub agent_id: String,
    pub epoch: i64,
    pub generation: u64,
    pub account_key: String,
    pub provider: String,
}

pub type Result<T> = std::result::Result<T, GuestProviderError>;

#[derive(Debug, Error)]
pub enum GuestProviderError {
    #[error("invalid VM provider identity or launch options")]
    Invalid,
    #[error("VM provider outcome is unknown and must be reconciled")]
    Unknown,
    #[error("VM provider refused the operation")]
    Refused,
    #[error("VM provider response is invalid")]
    InvalidResponse,
    #[error("VM provider persistence failed")]
    Persistence(#[from] StoreError),
    #[error("VM provider obligation persistence failed")]
    Outbox(#[from] OutboxError),
    #[error("VM provider retained response failed")]
    RequestResult(#[from] RequestResultError),
    #[error("VM guest request failed")]
    Guest(#[from] GuestError),
    #[error("VM provider JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error("VM provider worker task failed")]
    Task,
}

#[derive(Serialize, Deserialize)]
struct BootstrapCompletion {
    signature: String,
}

#[derive(Clone, Copy)]
pub struct VmBootstrapSource<'a> {
    pub attempt_id: &'a str,
    pub admission: &'a AdmittedRequest,
}

impl VmBootstrapSource<'_> {
    fn state(
        self,
        agent: &str,
        epoch: i64,
        root: &str,
        operation: &str,
        signature: &str,
    ) -> VmAuthBootstrap {
        VmAuthBootstrap {
            agent_id: agent.into(),
            epoch,
            request_id: operation.into(),
            root_request_id: root.into(),
            admitted_request: self.admission.clone(),
            fingerprint: signature.into(),
            state: WorkEffectStatus::NotSent,
        }
    }
}

/// Old credential identities are hashes; their source has no recoverable actor
/// or destination. They remain held by the exact external request identity.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct CredentialCopyHold {
    request_id: String,
    signature: String,
    status: WorkEffectStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    copy: Option<CredentialCopy>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
enum VmObligation {
    VmLaunch(VmLaunch),
    CredentialCopy(CredentialCopyHold),
    VmAuthBootstrap(VmAuthBootstrap),
}

enum StartDisposition {
    Dispatch,
    Reconcile,
    /// The original successful start metadata, never replaced by a later info read.
    Retained(Value),
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderInfo {
    pub handle: String,
    pub generation: u64,
    pub sequence: u64,
    pub acknowledged: u64,
    pub init_result: Option<Value>,
    pub resumed: bool,
    pub pid: Option<u32>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderEvent {
    pub sequence: u64,
    pub generation: u64,
    pub kind: String,
    pub payload: Value,
}

/// The VM provider's ordered native journal and concurrent write lane.
///
/// Each method crosses the existing guest RPC interface. The guest's retained
/// native supervisor is the authority for process identity, write receipts,
/// stream replay and acknowledgement. Callers invoke `settle_then_ack` only
/// after the associated event has been committed on the host.
#[derive(Clone)]
pub struct GuestProviderSession {
    guest: Arc<dyn GuestRpc>,
    identity: StartIdentity,
    info: ProviderInfo,
    /// Highest guest journal event delivered by this host attachment.
    read_cursor: Arc<AtomicU64>,
    /// Highest event whose durable host effects were acknowledged by the guest.
    cursor: Arc<AtomicU64>,
}

/// A per-agent provider factory. The account worker creates this only after it
/// has selected a VM actor and resolved that actor's launch settings.
pub struct GuestProviderTaskFactory {
    store: Arc<RuntimeStore>,
    guest: Arc<dyn GuestRpc>,
    identity: GuestProviderIdentity,
    source: GuestProviderSource,
}

enum GuestProviderSource {
    Launch(String),
    Existing(Box<GuestProviderSession>),
}

impl GuestProviderTaskFactory {
    pub fn new(
        store: Arc<RuntimeStore>,
        guest: Arc<dyn GuestRpc>,
        identity: GuestProviderIdentity,
        profile: impl Into<String>,
    ) -> Self {
        Self {
            store,
            guest,
            identity,
            source: GuestProviderSource::Launch(profile.into()),
        }
    }

    /// Inspect a proved existing process without resolving or reserving a launch.
    pub(crate) async fn existing(
        store: Arc<RuntimeStore>,
        guest: Arc<dyn GuestRpc>,
        identity: GuestProviderIdentity,
    ) -> Result<Self> {
        let session =
            GuestProviderSession::existing(store.clone(), guest.clone(), &identity).await?;
        Ok(Self {
            store,
            guest,
            identity,
            source: GuestProviderSource::Existing(Box::new(session)),
        })
    }
}

impl ProviderTaskFactory for GuestProviderTaskFactory {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> OpenFuture<'_> {
        Box::pin(async move {
            #[cfg(test)]
            if matches!(&self.source, GuestProviderSource::Existing(_))
                && let Some((held, release)) = self.guest.reattach_gate()
            {
                held.notify_one();
                release.notified().await;
            }
            if handle != ProviderHandle::for_account(&self.identity.agent_id)
                || options.expected_generation != Some(Generation(self.identity.generation))
                || match &self.source {
                    GuestProviderSource::Launch(_) => options.launch.command.is_empty(),
                    GuestProviderSource::Existing(_) => !options.launch.command.is_empty(),
                }
            {
                return Err(AdapterError::InvalidConfiguration);
            }
            let existing_only = matches!(&self.source, GuestProviderSource::Existing(_));
            let session = match &self.source {
                GuestProviderSource::Existing(session) => session.as_ref().clone(),
                GuestProviderSource::Launch(profile) => GuestProviderSession::open(
                    self.store.clone(),
                    self.guest.clone(),
                    self.identity.clone(),
                    profile.clone(),
                    options.launch.clone(),
                )
                .await
                .map_err(|_| AdapterError::TaskFailed)?,
            };
            // The guest supervisor generation is private to its retained
            // process. SessionHandle tickets use the host account worker's
            // generation, supplied by its launch options.
            let generation = options
                .expected_generation
                .unwrap_or(Generation(session.info.generation));
            let handle = ProviderHandle::for_account(&self.identity.agent_id);
            let (commands, inbox) = tokio::sync::mpsc::channel(LANE_CAPACITY);
            let (outbox, journal) = tokio::sync::mpsc::channel(LANE_CAPACITY);
            let (state, status) = tokio::sync::watch::channel(AttachmentStatus {
                handle: handle.clone(),
                open_attempted: true,
                info: Some(AttachmentInfo {
                    handle,
                    generation,
                    pid: session.info.pid.unwrap_or(0),
                    acknowledged: EventCursor(session.cursor()),
                    resumed: session.info.resumed,
                }),
                initialization: InitializationStatus::AwaitingReply,
            });
            let task = tokio::spawn(run_guest_attachment(
                session,
                inbox,
                outbox,
                state,
                shutdown.subscribe(),
                GuestAttachmentContext {
                    clock: options.clock,
                    response_ids: options
                        .launch
                        .retained_response_ids
                        .into_iter()
                        .map(|(remote, local)| (remote, Value::from(local)))
                        .collect(),
                    session_generation: generation,
                    existing_only,
                },
            ));
            Ok(ProviderAttachment::from_task(
                commands, journal, status, shutdown, task,
            ))
        })
    }
}

struct GuestAttachmentContext {
    clock: SessionClock,
    /// Guest supervisor remote request ID to the host's local request ID.
    response_ids: BTreeMap<u64, Value>,
    session_generation: Generation,
    /// The active submitted turn already completed native initialization.
    existing_only: bool,
}

async fn run_guest_attachment(
    session: GuestProviderSession,
    mut commands: tokio::sync::mpsc::Receiver<AdapterCommand>,
    journal: tokio::sync::mpsc::Sender<JournalEntry>,
    status: tokio::sync::watch::Sender<AttachmentStatus>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    mut context: GuestAttachmentContext,
) -> TaskExit {
    // Keep the guest read RPC alive across command arrivals. GuestRpc uses a
    // blocking socket boundary; cancelling its future would orphan an in-flight
    // read while the next attachment starts another one.
    let (read_tx, mut read_rx) = tokio::sync::mpsc::channel(1);
    let reader_session = session.clone();
    let reader_stop = stop.clone();
    let reader = tokio::spawn(async move {
        let mut reader_stop = reader_stop;
        loop {
            if *reader_stop.borrow() {
                break;
            }
            let next = reader_session.next().await;
            let failed = next.is_err();
            tokio::select! {
                biased;
                _ = Shutdown::cancelled(&mut reader_stop) => break,
                sent = read_tx.send(next) => if sent.is_err() { break; },
            }
            if failed {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    });
    let mut unacked = BTreeMap::<u64, ProviderEvent>::new();
    let mut initialization = InitializationStatus::AwaitingReply;
    let mut initialize_id = None;
    let mut cached_initialize = session.info.init_result.clone();
    let mut result = Ok(());
    loop {
        tokio::select! {
            biased;
            _ = Shutdown::cancelled(&mut stop) => break,
            command = commands.recv() => match command {
                Some(AdapterCommand::Write { write, reply, observed }) => {
                    let message = write.message().clone();
                    let method = message.get("method").and_then(Value::as_str).map(str::to_owned);
                    if method.as_deref() == Some("initialize")
                        && let Some(cached) = cached_initialize.take()
                    {
                        initialize_id = message.get("id").cloned();
                        let _sent = reply.send(CommandReceipt::CachedInitialize(cached));
                        if let Err(error) = observed_or_stopped(observed, &mut stop).await {
                            result = Err(error);
                            break;
                        }
                        continue;
                    }
                    let initialize = method.as_deref() == Some("initialize");
                    let local_id = message.get("id").cloned();
                    // The guest supervisor renumbers a request only when its
                    // native ID is an integer, and it saves the reply to remote
                    // ID 1 as the handle's initialize result. Initialize always
                    // gets an integer native ID, as on the local supervisor
                    // path, so a Claude initialize with a string ID is saved too
                    // and a reattach never sends initialize to a live provider.
                    let native_id = match &local_id {
                        Some(Value::String(_)) | None if initialize => Some(Value::from(1)),
                        _ => local_id.clone(),
                    };
                    let receipt = if context.existing_only && method.as_deref() == Some("initialized") {
                        // Complete only the new host session's cached handshake.
                        // Never repeat initialization on the retained guest process.
                        initialization = InitializationStatus::Ready;
                        let mut published = status.borrow().clone();
                        published.initialization = initialization;
                        status.send_replace(published);
                        CommandReceipt::Written { remote_id: None, duplicate: true }
                    } else { match session.write(&write.operation_id().0,
                        native_id.as_ref().unwrap_or(&Value::Null), message).await {
                        Ok(value) if value.get("accepted") == Some(&Value::Bool(true)) => {
                            let remote_id = value.get("remoteId").and_then(Value::as_u64);
                            if let (Some(remote), Some(local), true) = (
                                remote_id,
                                local_id.clone(),
                                native_id.as_ref().is_some_and(Value::is_u64),
                            ) {
                                context.response_ids.insert(remote, local);
                            }
                            if initialize { initialize_id = local_id.or(native_id); }
                            if method.as_deref() == Some("initialized") {
                                initialization = InitializationStatus::Ready;
                                let mut published = status.borrow().clone();
                                published.initialization = initialization;
                                status.send_replace(published);
                            }
                            CommandReceipt::Written { remote_id, duplicate: false }
                        }
                        // A successful but malformed guest reply does not
                        // prove that the provider write was rejected.
                        Ok(_) => CommandReceipt::Unknown,
                        Err(GuestProviderError::Unknown) => CommandReceipt::Unknown,
                        Err(_) => CommandReceipt::NotSent,
                    }};
                    let observe = receipt.requires_observation();
                    let _sent = reply.send(receipt);
                    if observe && let Err(error) = observed_or_stopped(observed, &mut stop).await {
                        result = Err(error);
                        break;
                    }
                }
                Some(AdapterCommand::Ack { cursor, reply }) => {
                    let previous_cursor = session.cursor();
                    let mut acked = previous_cursor;
                    let mut failure = None;
                    while acked < cursor.0 {
                        let sequence = acked.saturating_add(1);
                        let Some(event) = unacked.get(&sequence) else {
                            failure = Some(CommandReceipt::NotSent);
                            break;
                        };
                        match session.settle_then_ack(event, || Ok(())).await {
                            Ok(()) => { acked = sequence; unacked.remove(&sequence); }
                            Err(_) => { failure = Some(CommandReceipt::Unknown); break; }
                        }
                    }
                    if acked > previous_cursor {
                        let mut published = status.borrow().clone();
                        if let Some(info) = published.info.as_mut() {
                            info.acknowledged = EventCursor(acked);
                        }
                        published.initialization = initialization;
                        status.send_replace(published);
                    }
                    let receipt = failure.unwrap_or(CommandReceipt::Acknowledged(EventCursor(acked)));
                    if reply.send(receipt).is_err() {
                        result = Err(AdapterError::ConsumerClosed);
                        break;
                    }
                }
                None => break,
            },
            event = read_rx.recv() => match event.unwrap_or(Err(GuestProviderError::Task)) {
                Ok(Some(mut event)) => {
                    if event.kind == "stdout" {
                        // A server request carries the provider's own ID space;
                        // only a reply maps back to a local request ID.
                        if event.payload.get("method").is_none()
                            && let Some(remote) = event.payload.get("id").and_then(Value::as_u64)
                            && let Some(local) = context.response_ids.get(&remote)
                            && let Some(object) = event.payload.as_object_mut()
                        {
                            object.insert("id".into(), local.clone());
                        }
                        if let Some(id) = &initialize_id
                            && event.payload.get("id") == Some(id)
                            && event.payload.get("method").is_none() {
                                initialization = if event.payload.get("error").is_some() {
                                    InitializationStatus::Refused
                                } else { InitializationStatus::InitializedWritePending };
                                let mut published = status.borrow().clone();
                                published.initialization = initialization;
                                status.send_replace(published);
                        }
                    }
                    unacked.insert(event.sequence, event.clone());
                    let payload = match event.kind.as_str() {
                        "stdout" => match decode_journal_value(event.sequence, event.generation, &event.payload).payload {
                            DecodedJournalPayload::Frame(frame) => JournalPayload::Frame(frame),
                            DecodedJournalPayload::RawStdout { bytes, error } => JournalPayload::RawStdout { bytes, error },
                        },
                        "stderr" => JournalPayload::Stderr(event.payload),
                        "exit" => JournalPayload::Exit(event.payload),
                        kind => JournalPayload::Other { kind: kind.to_owned(), payload: event.payload },
                    };
                    let entry = JournalEntry {
                        received_at_ms: context.clock.now_ms(),
                        cursor: EventCursor(event.sequence),
                        generation: context.session_generation,
                        payload,
                    };
                    // The session task stops reading the journal before it joins
                    // this attachment. The unacknowledged event replays later.
                    let sent = tokio::select! {
                        biased;
                        _ = Shutdown::cancelled(&mut stop) => break,
                        sent = journal.send(entry) => sent,
                    };
                    if sent.is_err() {
                        result = Err(AdapterError::ConsumerClosed);
                        break;
                    }
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(_) => { result = Err(AdapterError::TaskFailed); break; }
            }
        }
    }
    // Let an in-flight blocking guest read reach its RPC deadline, then join
    // the reader before returning so a failed attachment cannot overlap its
    // replacement. Cancellation detaches; explicit actor disposal owns
    // provider.stop.
    let _reader_exit = reader.await;
    TaskExit {
        result,
        cleanup_error: None,
        recovery_needed: None,
        // The account worker disconnects its VM actors when their session ends.
        provider_retained: false,
    }
}

/// Wait until the session task observes a buffered receipt, or until shutdown.
/// On shutdown the session task joins this attachment before it reads the
/// receipt, so a wait for the observation alone cannot end. The receipt stays
/// buffered in its channel and keeps its evidence.
async fn observed_or_stopped(
    observed: tokio::sync::oneshot::Receiver<()>,
    stop: &mut tokio::sync::watch::Receiver<bool>,
) -> std::result::Result<(), AdapterError> {
    tokio::select! {
        biased;
        () = Shutdown::cancelled(stop) => Ok(()),
        seen = observed => seen.map_err(|_| AdapterError::ConsumerClosed),
    }
}

impl GuestProviderSession {
    async fn existing(
        store: Arc<RuntimeStore>,
        guest: Arc<dyn GuestRpc>,
        actor: &GuestProviderIdentity,
    ) -> Result<Self> {
        let expected = actor.clone();
        let (identity, original) = store
            .read_async(
                "runtime-services/linux_vm_provider",
                move |connection| -> Result<_> {
                    if !vm_actor_matches(
                        connection,
                        &expected.agent_id,
                        expected.epoch,
                        &expected.account_key,
                        Some(&expected.provider),
                    )? {
                        return Err(GuestProviderError::Refused);
                    }
                    let agent = AgentRepository::get(
                        connection,
                        &AgentId::from(expected.agent_id.as_str()),
                    )?
                    .ok_or(GuestProviderError::Unknown)?;
                    let saved = agent.vm_launch.ok_or(GuestProviderError::Unknown)?;
                    if saved.status != WorkEffectStatus::Complete {
                        return Err(GuestProviderError::Unknown);
                    }
                    let identity = saved.identity;
                    let info = saved.info.ok_or(GuestProviderError::Unknown)?;
                    if identity.agent_id != expected.agent_id
                        || identity.epoch != expected.epoch
                        || identity.handle
                            != format!("{LINUX_VM_PROVIDER_HANDLE_PREFIX}{}", expected.agent_id)
                        || identity.request_id.is_empty()
                        || identity.request_id.len() > START_REQUEST_ID_MAX_BYTES
                        || identity.launch_fingerprint.len() != 64
                        || !identity
                            .launch_fingerprint
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                        || saved.signature
                            != format!(
                                "agent={};epoch={};launch={}",
                                identity.agent_id, identity.epoch, identity.launch_fingerprint
                            )
                    {
                        return Err(GuestProviderError::Unknown);
                    }
                    Ok((identity, info))
                },
            )
            .await?;
        let current = call_guest(
            guest.clone(),
            "provider.rpc",
            json!({"handle":identity.handle,"action":"info"}),
            None,
            PROVIDER_RPC_TIMEOUT,
        )
        .await?;
        let info = parse_info(&identity.handle, &current, Some(&original))?;
        if current["state"] != "running" || !info.init_result.as_ref().is_some_and(Value::is_object)
        {
            return Err(GuestProviderError::Unknown);
        }
        let cursor = info.acknowledged;
        Ok(Self {
            guest,
            identity,
            info,
            read_cursor: Arc::new(AtomicU64::new(cursor)),
            cursor: Arc::new(AtomicU64::new(cursor)),
        })
    }

    /// Reserve the start identity in the host store before dispatching to guest.
    /// A persisted pending identity is reconciled by `info`; start is never
    /// resent when a previous call may have reached the guest.
    pub async fn open(
        store: Arc<RuntimeStore>,
        guest: Arc<dyn GuestRpc>,
        identity: GuestProviderIdentity,
        profile: impl Into<String>,
        launch: ProviderLaunchSpec,
    ) -> Result<Self> {
        let GuestProviderIdentity {
            agent_id,
            epoch,
            generation: _,
            account_key,
            provider,
        } = identity;
        let profile = profile.into();
        let params = start_params(&agent_id, &provider, &profile, &launch)?;
        let fingerprint = fingerprint(&params)?;
        let (identity, disposition) = reserve_start(
            Arc::clone(&store),
            agent_id.clone(),
            epoch,
            account_key.clone(),
            provider.clone(),
            fingerprint.clone(),
        )
        .await?;

        let started = if matches!(&disposition, StartDisposition::Dispatch) {
            match call_guest(
                guest.clone(),
                "provider.start",
                params,
                Some(identity.request_id.clone()),
                PROVIDER_START_TIMEOUT,
            )
            .await
            {
                Ok(info) => info,
                Err(error) if error.uncertain() => return Err(GuestProviderError::Unknown),
                Err(error) => {
                    persist_start(store.clone(), identity.clone(), None).await?;
                    return Err(error.into());
                }
            }
        } else {
            // An existing reservation means dispatch may have happened before a
            // crash or lost response. Reconcile through the read-only info lane.
            call_guest(
                guest.clone(),
                "provider.rpc",
                json!({"handle": identity.handle, "action": "info"}),
                None,
                PROVIDER_RPC_TIMEOUT,
            )
            .await
            .map_err(|error| {
                if error.uncertain() || error.code() == Some("not_found") {
                    GuestProviderError::Unknown
                } else {
                    error.into()
                }
            })?
        };
        // A known start can retain stdout and exit events after its process ends.
        // An uncertain start still needs running proof before it becomes Started.
        let retained = match &disposition {
            StartDisposition::Retained(info) => Some(info),
            StartDisposition::Dispatch | StartDisposition::Reconcile => None,
        };
        let info = parse_info(&identity.handle, &started, retained)?;
        if retained.is_none() {
            persist_start(store, identity.clone(), Some(started)).await?;
        }
        let cursor = info.acknowledged;
        Ok(Self {
            guest,
            identity,
            info,
            read_cursor: Arc::new(AtomicU64::new(cursor)),
            cursor: Arc::new(AtomicU64::new(cursor)),
        })
    }

    pub fn identity(&self) -> &StartIdentity {
        &self.identity
    }

    pub fn info(&self) -> &ProviderInfo {
        &self.info
    }

    pub fn cursor(&self) -> u64 {
        self.cursor.load(Ordering::Acquire)
    }

    /// Concurrent write lane. A lost response is reconciled by its exact native
    /// operation identity and never submitted again by this adapter.
    pub async fn write(
        &self,
        operation_id: &str,
        native_id: &Value,
        message: Value,
    ) -> Result<Value> {
        if operation_id.is_empty()
            || operation_id.len() > 256
            || !matches!(native_id, Value::Null | Value::String(_) | Value::Number(_))
            || !message.is_object()
        {
            return Err(GuestProviderError::Invalid);
        }
        let params = json!({
            "handle": self.identity.handle,
            "action": "write",
            "operationId": operation_id,
            "nativeId": native_id,
            "message": message,
        });
        match call_guest(
            self.guest.clone(),
            "provider.rpc",
            params,
            None,
            PROVIDER_RPC_TIMEOUT,
        )
        .await
        {
            Ok(receipt) => Ok(receipt),
            // The guest may persist an operation before returning an error
            // (for example, when writing to the provider's stdin fails).
            // Reconcile every post-dispatch failure and keep the outcome
            // unknown unless the guest has a durable accepted receipt.
            Err(_error) => {
                let status = call_guest(
                    self.guest.clone(),
                    "provider.rpc",
                    json!({"handle":self.identity.handle,"action":"operationStatus","operationId":operation_id}),
                    None,
                    PROVIDER_RPC_TIMEOUT,
                )
                .await
                .map_err(|_| GuestProviderError::Unknown)?;
                if status.get("accepted") == Some(&Value::Bool(true)) {
                    Ok(status)
                } else {
                    Err(GuestProviderError::Unknown)
                }
            }
        }
    }

    /// Read the next journal entry on the independent delivery lane. Advancing
    /// this cursor never acknowledges the guest event; only settle_then_ack does.
    pub async fn next(&self) -> Result<Option<ProviderEvent>> {
        let cursor = self.read_cursor.load(Ordering::Acquire);
        let result = read_guest_journal(
            self.guest.clone(),
            json!({"handle":self.identity.handle,"action":"next","cursor":cursor}),
        )
        .await?;
        match result.get("event") {
            None | Some(Value::Null) => Ok(None),
            Some(event) => {
                let event = self.hydrate_event(event).await?;
                if event.sequence != cursor.saturating_add(1)
                    || event.generation != self.info.generation
                {
                    return Err(GuestProviderError::InvalidResponse);
                }
                self.read_cursor
                    .compare_exchange(cursor, event.sequence, Ordering::AcqRel, Ordering::Acquire)
                    .map_err(|_| GuestProviderError::InvalidResponse)?;
                Ok(Some(event))
            }
        }
    }

    /// Re-read journal events after `cursor`; replay never advances host state.
    pub async fn replay(&self, cursor: u64, limit: u8) -> Result<Vec<ProviderEvent>> {
        if cursor < self.info.acknowledged
            || cursor > self.info.sequence
            || !(1..=128).contains(&limit)
        {
            return Err(GuestProviderError::Invalid);
        }
        let result = read_guest_journal(
            self.guest.clone(),
            json!({"handle":self.identity.handle,"action":"replay","cursor":cursor,"limit":limit}),
        )
        .await?;
        let events = result
            .get("events")
            .and_then(Value::as_array)
            .ok_or(GuestProviderError::InvalidResponse)?;
        let total_bytes = events
            .iter()
            .try_fold(0_u64, |total, event| {
                let bytes = event
                    .get("payloadBytes")
                    .and_then(Value::as_u64)
                    .unwrap_or_else(|| {
                        event
                            .get("payload")
                            .and_then(Value::as_str)
                            .map_or(0, |payload| payload.len() as u64)
                    });
                (bytes <= EVENT_PAYLOAD_MAX_BYTES)
                    .then_some(total.saturating_add(bytes))
                    .filter(|total| *total <= EVENT_PAYLOAD_MAX_BYTES)
            })
            .ok_or(GuestProviderError::InvalidResponse)?;
        let _bounded_payload_total = total_bytes;
        let mut expected = cursor.saturating_add(1);
        let mut parsed = Vec::with_capacity(events.len());
        for value in events {
            let event = self.hydrate_event(value).await?;
            if event.sequence != expected || event.generation != self.info.generation {
                return Err(GuestProviderError::InvalidResponse);
            }
            expected = expected.saturating_add(1);
            parsed.push(event);
        }
        Ok(parsed)
    }

    async fn hydrate_event(&self, value: &Value) -> Result<ProviderEvent> {
        let Some(total) = value.get("payloadBytes") else {
            return parse_event(value);
        };
        let total = total.as_u64().ok_or(GuestProviderError::InvalidResponse)?;
        let sequence = value
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or(GuestProviderError::InvalidResponse)?;
        let generation = value
            .get("generation")
            .and_then(Value::as_u64)
            .ok_or(GuestProviderError::InvalidResponse)?;
        if value
            .get("payload")
            .is_none_or(|payload| !payload.is_null())
            || total == 0
            || total > EVENT_PAYLOAD_MAX_BYTES
        {
            return Err(GuestProviderError::InvalidResponse);
        }
        let mut output = Vec::with_capacity(
            usize::try_from(total).map_err(|_| GuestProviderError::InvalidResponse)?,
        );
        let deadline = tokio::time::Instant::now() + Duration::from_secs(300);
        while output.len() < total as usize {
            if tokio::time::Instant::now() >= deadline {
                return Err(GuestProviderError::InvalidResponse);
            }
            let offset = output.len() as u64;
            let max_bytes = EVENT_CHUNK_MAX_BYTES.min(total - offset);
            let chunk = read_guest_journal(
                self.guest.clone(),
                json!({"handle":self.identity.handle,"action":"eventRead","sequence":sequence,"offset":offset,"maxBytes":max_bytes}),
            )
            .await?;
            let data = chunk
                .get("data")
                .and_then(Value::as_str)
                .and_then(|data| {
                    base64::Engine::decode(&base64::engine::general_purpose::STANDARD, data).ok()
                })
                .ok_or(GuestProviderError::InvalidResponse)?;
            let next = offset.saturating_add(data.len() as u64);
            if chunk.get("sequence").and_then(Value::as_u64) != Some(sequence)
                || chunk.get("generation").and_then(Value::as_u64) != Some(generation)
                || chunk.get("offset").and_then(Value::as_u64) != Some(offset)
                || chunk.get("bytes").and_then(Value::as_u64) != Some(total)
                || chunk.get("nextOffset").and_then(Value::as_u64) != Some(next)
                || data.is_empty()
                || data.len() as u64 > max_bytes
                || chunk.get("eof").and_then(Value::as_bool) != Some(next == total)
            {
                return Err(GuestProviderError::InvalidResponse);
            }
            output.extend(data);
        }
        let text = std::str::from_utf8(&output).map_err(|_| GuestProviderError::InvalidResponse)?;
        let payload: Value = serde_json::from_str(text)?;
        if !payload.is_object() {
            return Err(GuestProviderError::InvalidResponse);
        }
        parse_event(&json!({
            "sequence":sequence,
            "generation":generation,
            "kind":value.get("kind"),
            "payload":payload.to_string(),
        }))
    }

    /// Run the durable host settlement first and only then ACK the guest journal.
    pub async fn settle_then_ack<T>(
        &self,
        event: &ProviderEvent,
        settle: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let cursor = self.cursor.load(Ordering::Acquire);
        if event.sequence != cursor.saturating_add(1)
            || event.sequence > self.read_cursor.load(Ordering::Acquire)
            || event.generation != self.info.generation
        {
            return Err(GuestProviderError::Invalid);
        }
        let value = settle()?;
        // The guest answers an ACK at or below its saved cursor without a change.
        read_guest_journal(
            self.guest.clone(),
            json!({"handle":self.identity.handle,"action":"ack","sequence":event.sequence}),
        )
        .await?;
        self.cursor
            .compare_exchange(cursor, event.sequence, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| GuestProviderError::Unknown)?;
        Ok(value)
    }

    /// Stop the exact retained guest handle, using a stable idempotency key.
    pub async fn stop(&self) -> Result<Value> {
        let request_id = format!("{}:stop", self.identity.request_id);
        if request_id.len() > START_REQUEST_ID_MAX_BYTES {
            return Err(GuestProviderError::Invalid);
        }
        call_guest(
            self.guest.clone(),
            "provider.stop",
            json!({"handle":self.identity.handle}),
            Some(request_id),
            PROVIDER_RPC_TIMEOUT,
        )
        .await
        .map_err(Into::into)
    }
}

/// Copy one allow-listed credential document into the guest with a durable,
/// secret-free host intent. The deterministic request identity makes retries
/// idempotent at the existing guest RPC boundary; the owner stores only the
/// destination and content digest, never credential bytes or tokens.
pub async fn copy_guest_credentials(
    store: Arc<RuntimeStore>,
    guest: Arc<dyn GuestRpc>,
    target: GuestCredentialTarget<'_>,
    credential_document: &[u8],
) -> Result<String> {
    let provider = target
        .provider
        .parse()
        .map_err(|_| GuestProviderError::Invalid)?;
    if target.agent_id.is_empty()
        || target.actor_epoch < 0
        || target.account_key.is_empty()
        || target.relative_path.is_empty()
        || target.relative_path.starts_with('/')
        || target.relative_path.contains(['\\', '\0'])
        || target
            .relative_path
            .split('/')
            .any(|part| part == ".." || part == "." || part.is_empty())
        || credential_document.len() > MAX_GUEST_CREDENTIAL_BYTES
    {
        return Err(GuestProviderError::Invalid);
    }
    let digest = hex_digest(credential_document);
    let request_material = serde_json::to_vec(&json!({
        "agentId": target.agent_id,
        "accountKey": target.account_key,
        "path": target.relative_path,
        "digest": digest,
    }))?;
    let signature = hex_digest(&request_material);
    let request_id = format!("{CREDENTIAL_RECEIPT_PREFIX}{}", &signature[..48]);
    let copy = CredentialCopy {
        agent_id: target.agent_id.to_owned(),
        epoch: target.actor_epoch,
        account_key: target.account_key.to_owned(),
        provider,
        relative_path: target.relative_path.to_owned(),
        digest: digest.clone(),
        request_id: request_id.clone(),
        signature,
        status: WorkEffectStatus::NotSent,
    };
    let reserve = copy.clone();
    let dispatch = store
        .transaction_with(move |tx| reserve_credentials(tx, reserve))
        .await?
        .ok_or(GuestProviderError::Unknown)?;
    if !dispatch {
        return Ok(digest);
    }
    let params = json!({
        "files": [{
            "path": target.relative_path,
            "data": base64::engine::general_purpose::STANDARD.encode(credential_document),
        }]
    });
    let result = call_guest(
        guest,
        "credentials.put",
        params,
        Some(request_id.clone()),
        Duration::from_secs(GUEST_RPC_TIMEOUT_SECONDS),
    )
    .await;
    let status = match &result {
        Ok(_) => WorkEffectStatus::Complete,
        Err(error) if error.uncertain() => return Err(GuestProviderError::Unknown),
        Err(_) => WorkEffectStatus::NotSent,
    };
    store
        .transaction_with(move |tx| settle_credentials(tx, copy, status))
        .await?;
    result?;
    Ok(digest)
}

fn start_params(
    agent_id: &str,
    provider: &str,
    profile: &str,
    launch: &ProviderLaunchSpec,
) -> Result<Value> {
    let Some(cwd) = launch.cwd.as_ref().and_then(|cwd| cwd.to_str()) else {
        return Err(GuestProviderError::Invalid);
    };
    if agent_id.is_empty()
        || provider.is_empty()
        || profile.is_empty()
        || launch.command.is_empty()
        || launch.command.iter().any(|part| part.contains('\0'))
        || !Path::new(cwd).is_absolute()
    {
        return Err(GuestProviderError::Invalid);
    }
    Ok(json!({
        "transport":"native",
        "handle":format!("{LINUX_VM_PROVIDER_HANDLE_PREFIX}{agent_id}"),
        "argv":launch.command,
        "cwd":cwd,
        "env":launch.environment,
        "agentId":agent_id,
        "layr":true,
        "provider":provider,
        "profile":profile,
    }))
}

fn fingerprint(params: &Value) -> Result<String> {
    Ok(hex_digest(&serde_json::to_vec(params)?))
}

async fn reserve_start(
    store: Arc<RuntimeStore>,
    agent_id: String,
    epoch: i64,
    account_key: String,
    provider: String,
    fingerprint: String,
) -> Result<(StartIdentity, StartDisposition)> {
    if epoch < 0 || agent_id.is_empty() || fingerprint.len() != 64 {
        return Err(GuestProviderError::Invalid);
    }
    let signature = format!("agent={agent_id};epoch={epoch};launch={fingerprint}");
    store
        .transaction_with(
            move |tx| -> Result<Option<(StartIdentity, StartDisposition)>> {
                if !vm_actor_matches(tx, &agent_id, epoch, &account_key, Some(&provider))? {
                    return Err(GuestProviderError::Refused);
                }
                let mut agent = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                    .ok_or(GuestProviderError::Refused)?;
                if held_for_agent(tx, &agent_id, "vmLaunch")? {
                    return Ok(None);
                }
                if let Some(mut saved) = agent.vm_launch.clone() {
                    if saved.identity.epoch != epoch {
                        if unresolved(saved.status) {
                            hold_obligation(
                                tx,
                                &mut agent,
                                &saved.identity.request_id,
                                &saved.signature,
                                "vmLaunch",
                                &VmObligation::VmLaunch(saved.clone()),
                            )?;
                            return Ok(None);
                        }
                    } else {
                        if saved.signature != signature
                            || saved.identity.agent_id != agent_id
                            || saved.identity.launch_fingerprint != fingerprint
                            || saved.identity.handle
                                != format!("{LINUX_VM_PROVIDER_HANDLE_PREFIX}{agent_id}")
                            || saved.identity.request_id.is_empty()
                            || saved.identity.request_id.len() > START_REQUEST_ID_MAX_BYTES
                        {
                            return Err(GuestProviderError::Unknown);
                        }
                        let disposition = if saved.status == WorkEffectStatus::NotSent {
                            saved.status = WorkEffectStatus::Submitted;
                            agent.vm_launch = Some(saved.clone());
                            AgentRepository::put(tx, &agent)?;
                            StartDisposition::Dispatch
                        } else if saved.status == WorkEffectStatus::Complete {
                            StartDisposition::Retained(
                                saved.info.ok_or(GuestProviderError::Unknown)?,
                            )
                        } else {
                            StartDisposition::Reconcile
                        };
                        return Ok(Some((saved.identity, disposition)));
                    }
                }
                let identity = StartIdentity {
                    agent_id: agent_id.to_owned(),
                    epoch,
                    handle: format!("{LINUX_VM_PROVIDER_HANDLE_PREFIX}{agent_id}"),
                    request_id: RequestIdentity::issue_root(),
                    launch_fingerprint: fingerprint.to_owned(),
                };
                agent.vm_launch = Some(VmLaunch {
                    identity: identity.clone(),
                    signature,
                    status: WorkEffectStatus::Submitted,
                    info: None,
                });
                AgentRepository::put(tx, &agent)?;
                Ok(Some((identity, StartDisposition::Dispatch)))
            },
        )
        .await?
        .ok_or(GuestProviderError::Unknown)
}

async fn persist_start(
    store: Arc<RuntimeStore>,
    identity: StartIdentity,
    info: Option<Value>,
) -> Result<()> {
    let status = if info.is_some() {
        WorkEffectStatus::Complete
    } else {
        WorkEffectStatus::NotSent
    };
    store
        .transaction_with(move |tx| settle_start(tx, &identity, status, info))
        .await
}

fn unresolved(status: WorkEffectStatus) -> bool {
    status != WorkEffectStatus::NotSent && status != WorkEffectStatus::Complete
}

fn held_for_agent(tx: &Transaction<'_>, agent: &str, field: &str) -> Result<bool> {
    tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM runtime_outbox WHERE owner_kind=?1 AND owner_id=?2 AND kind=?3 AND json_type(payload, '$.' || ?4) IS NOT NULL)",
        rusqlite::params![OwnerKind::Agent, agent, OutboxKind::ProviderCall, field], |row| row.get(0),
    ).map_err(StoreError::from).map_err(Into::into)
}

fn hold_obligation(
    tx: &Transaction<'_>,
    agent: &mut AgentRecord,
    request: &str,
    signature: &str,
    field: &str,
    payload: &VmObligation,
) -> Result<()> {
    let id = Outbox::effect_id(
        request,
        OutboxKind::ProviderCall,
        OwnerKind::Agent,
        &agent.id.0,
        &[field],
    );
    Outbox::put(
        tx,
        &OutboxRow {
            id,
            kind: OutboxKind::ProviderCall,
            owner_kind: OwnerKind::Agent,
            owner_id: agent.id.0.clone(),
            request_id: Some(request.to_owned()),
            signature: signature.to_owned(),
            status: OutboxStatus::Unknown,
            account: None,
            stream_id: None,
            generation: None,
            ordinal: None,
            next_at: None,
            payload,
            created: chrono::Utc::now().timestamp_millis(),
        },
    )?;
    match field {
        "vmLaunch" => agent.vm_launch = None,
        "credentialCopy" => agent.credential_copy = None,
        "vmAuthBootstrap" => agent.vm_auth_bootstrap = None,
        _ => return Err(GuestProviderError::Invalid),
    }
    AgentRepository::put(tx, agent)?;
    Ok(())
}

fn settle_start(
    tx: &Transaction<'_>,
    identity: &StartIdentity,
    status: WorkEffectStatus,
    info: Option<Value>,
) -> Result<()> {
    let mut agent = AgentRepository::get(tx, &AgentId::from(identity.agent_id.as_str()))?
        .ok_or(GuestProviderError::Unknown)?;
    let mut saved = agent.vm_launch.clone().ok_or(GuestProviderError::Unknown)?;
    if saved.identity != *identity || agent.epoch != identity.epoch {
        return Err(GuestProviderError::Unknown);
    }
    if saved.status == WorkEffectStatus::Complete && status == WorkEffectStatus::Complete {
        let original = saved.info.as_ref().ok_or(GuestProviderError::Unknown)?;
        parse_info(
            &identity.handle,
            info.as_ref().ok_or(GuestProviderError::Unknown)?,
            Some(original),
        )?;
        return Ok(());
    }
    if saved.status == status && saved.info == info {
        return Ok(());
    }
    if !unresolved(saved.status) {
        return Err(GuestProviderError::Unknown);
    }
    saved.status = status;
    saved.info = info;
    agent.vm_launch = Some(saved);
    AgentRepository::put(tx, &agent)?;
    Ok(())
}

fn reserve_credentials(tx: &Transaction<'_>, mut copy: CredentialCopy) -> Result<Option<bool>> {
    if !vm_actor_matches(
        tx,
        &copy.agent_id,
        copy.epoch,
        &copy.account_key,
        Some(copy.provider.key()),
    )? {
        return Err(GuestProviderError::Refused);
    }
    let mut agent = AgentRepository::get(tx, &AgentId::from(copy.agent_id.as_str()))?
        .ok_or(GuestProviderError::Refused)?;
    if held_for_agent(tx, &copy.agent_id, "credentialCopy")? {
        return Ok(None);
    }
    let held_id = Outbox::effect_id(
        &copy.request_id,
        OutboxKind::ProviderCall,
        OwnerKind::Vm,
        &copy.request_id,
        &["credentialCopy"],
    );
    if Outbox::get::<VmObligation>(tx, &held_id)?.is_some() {
        return Ok(None);
    }
    if let Some(saved) = agent.credential_copy.clone() {
        copy.status = saved.status;
        if saved == copy {
            if saved.status == WorkEffectStatus::Complete {
                return Ok(Some(false));
            }
            if saved.status != WorkEffectStatus::NotSent {
                return Ok(None);
            }
        } else if unresolved(saved.status) {
            hold_obligation(
                tx,
                &mut agent,
                &saved.request_id,
                &saved.signature,
                "credentialCopy",
                &VmObligation::CredentialCopy(CredentialCopyHold {
                    request_id: saved.request_id.clone(),
                    signature: saved.signature.clone(),
                    status: saved.status,
                    copy: Some(saved.clone()),
                }),
            )?;
            return Ok(None);
        }
    }
    if let Some(saved) = replay_request::<WorkEffectStatus>(
        tx,
        &copy.request_id,
        &copy.signature,
        chrono::Utc::now().timestamp_millis(),
    )? {
        if saved.reply != WorkEffectStatus::Complete {
            return Err(GuestProviderError::Unknown);
        }
        copy.status = WorkEffectStatus::Complete;
        agent.credential_copy = Some(copy);
        AgentRepository::put(tx, &agent)?;
        return Ok(Some(false));
    }
    // A fresh operation or a proved pre-transport failure is the only permission
    // to dispatch. Commit uncertainty before the guest can observe the request.
    copy.status = WorkEffectStatus::Submitted;
    agent.credential_copy = Some(copy);
    AgentRepository::put(tx, &agent)?;
    Ok(Some(true))
}

fn settle_credentials(
    tx: &Transaction<'_>,
    mut copy: CredentialCopy,
    status: WorkEffectStatus,
) -> Result<()> {
    let mut agent = AgentRepository::get(tx, &AgentId::from(copy.agent_id.as_str()))?
        .ok_or(GuestProviderError::Unknown)?;
    let saved = agent
        .credential_copy
        .clone()
        .ok_or(GuestProviderError::Unknown)?;
    copy.status = saved.status;
    if saved != copy || agent.epoch != copy.epoch {
        return Err(GuestProviderError::Unknown);
    }
    if saved.status == status {
        return Ok(());
    }
    if !unresolved(saved.status) {
        return Err(GuestProviderError::Unknown);
    }
    copy.status = status;
    agent.credential_copy = Some(copy);
    AgentRepository::put(tx, &agent)?;
    Ok(())
}

/// A submitted authentication bootstrap is never re-sent after response loss.
/// The boolean reports a retained completion; false reserves the first send.
pub async fn vm_bootstrap_admission(
    store: &RuntimeStore,
    agent_id: &str,
    epoch: i64,
    root_request_id: &str,
    source: VmBootstrapSource<'_>,
    operation: &str,
    signature: &str,
) -> Result<bool> {
    let state = source.state(agent_id, epoch, root_request_id, operation, signature);
    with_bootstrap(store, source, state, |tx, mut agent, mut state, fresh| {
        if let Some(saved) = replay_request::<BootstrapCompletion>(
            tx,
            state.admitted_request.id(),
            state.admitted_request.signature(),
            chrono::Utc::now().timestamp_millis(),
        )? {
            if saved.reply.signature != state.fingerprint {
                return Err(GuestProviderError::Unknown);
            }
            return Ok(Some(true));
        }
        if held_for_agent(tx, &state.agent_id, "vmAuthBootstrap")? {
            return Ok(None);
        }
        if let Some(saved) = agent.vm_auth_bootstrap.clone() {
            if saved.epoch == state.epoch
                && saved.admitted_request.id() == state.admitted_request.id()
            {
                if saved != state {
                    return Ok(None);
                }
                state.state = WorkEffectStatus::Submitted;
                agent.vm_auth_bootstrap = Some(state);
                AgentRepository::put(tx, &agent)?;
                return Ok(Some(false));
            }
            if !fresh {
                return Ok(None);
            }
            if unresolved(saved.state) {
                hold_obligation(
                    tx,
                    &mut agent,
                    saved.admitted_request.id(),
                    &saved.fingerprint,
                    "vmAuthBootstrap",
                    &VmObligation::VmAuthBootstrap(saved.clone()),
                )?;
                return Ok(None);
            }
        }
        if !fresh {
            return Ok(None);
        }
        state.state = WorkEffectStatus::Submitted;
        agent.vm_auth_bootstrap = Some(state);
        AgentRepository::put(tx, &agent)?;
        Ok(Some(false))
    })
    .await
}

pub async fn vm_bootstrap_settled(
    store: &RuntimeStore,
    agent_id: &str,
    epoch: i64,
    root_request_id: &str,
    source: VmBootstrapSource<'_>,
    operation: &str,
    signature: &str,
) -> Result<()> {
    let state = source.state(agent_id, epoch, root_request_id, operation, signature);
    with_bootstrap(store, source, state, |tx, mut agent, mut state, fresh| {
        if fresh {
            return Err(GuestProviderError::Unknown);
        }
        state.state = WorkEffectStatus::Complete;
        let current = agent.vm_auth_bootstrap.clone();
        let held_id = Outbox::effect_id(
            state.admitted_request.id(),
            OutboxKind::ProviderCall,
            OwnerKind::Agent,
            &state.agent_id,
            &["vmAuthBootstrap"],
        );
        let held = Outbox::get::<VmObligation>(tx, &held_id)?;
        let saved = current
            .as_ref()
            .filter(|saved| saved.admitted_request == state.admitted_request)
            .or_else(|| {
                held.as_ref().and_then(|held| match &held.payload {
                    VmObligation::VmAuthBootstrap(saved) => Some(saved),
                    VmObligation::VmLaunch(_) | VmObligation::CredentialCopy(_) => None,
                })
            })
            .ok_or(GuestProviderError::Unknown)?;
        let mut expected = state.clone();
        expected.state = saved.state;
        if saved != &expected {
            return Err(GuestProviderError::Unknown);
        }
        settle_request(
            tx,
            &state.admitted_request,
            Some(&state.agent_id),
            &BootstrapCompletion {
                signature: state.fingerprint.clone(),
            },
        )?;
        if current.is_some_and(|saved| saved.admitted_request == state.admitted_request) {
            agent.vm_auth_bootstrap = Some(state);
            AgentRepository::put(tx, &agent)?;
        }
        if let Some(held) = held {
            Outbox::delete_settled(tx, &held.id, &held.signature)?;
        }
        Ok(Some(()))
    })
    .await
}

async fn with_bootstrap<T: Send + 'static>(
    store: &RuntimeStore,
    source: VmBootstrapSource<'_>,
    mut state: VmAuthBootstrap,
    apply: impl FnOnce(&Transaction<'_>, AgentRecord, VmAuthBootstrap, bool) -> Result<Option<T>>
    + Send
    + 'static,
) -> Result<T> {
    let attempt = source.attempt_id.to_owned();
    let source = source.admission.clone();
    store
        .transaction_with(move |tx| -> Result<Option<T>> {
            let agent = bootstrap_agent(tx, &state.agent_id, state.epoch)?;
            if state.root_request_id.is_empty()
                || ExecutionHistoryRepository::admission(tx, &attempt)?.as_ref() != Some(&source)
            {
                return Err(GuestProviderError::Unknown);
            }
            let previous = ExecutionHistoryRepository::vm_auth_bootstrap_admission(tx, &attempt)?;
            let fresh = previous.is_none();
            state.admitted_request = previous.unwrap_or_else(|| {
                source.derive_result(
                    "vm-auth-bootstrap",
                    &[&state.request_id],
                    &state.fingerprint,
                )
            });
            let final_effect = state.admitted_request.clone();
            let result = apply(tx, agent, state, fresh)?;
            if fresh && result.is_some() {
                ExecutionHistoryRepository::set_vm_auth_bootstrap_admission(
                    tx,
                    &attempt,
                    &source,
                    &final_effect,
                )?;
            }
            Ok(result)
        })
        .await?
        .ok_or(GuestProviderError::Unknown)
}

fn bootstrap_agent(tx: &Transaction<'_>, agent_id: &str, epoch: i64) -> Result<AgentRecord> {
    let agent =
        AgentRepository::get(tx, &AgentId::from(agent_id))?.ok_or(GuestProviderError::Refused)?;
    let account = agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .unwrap_or("default");
    if !vm_actor_matches(tx, agent_id, epoch, account, None)? {
        return Err(GuestProviderError::Refused);
    }
    Ok(agent)
}

async fn call_guest(
    guest: Arc<dyn GuestRpc>,
    method: &'static str,
    params: Value,
    request_id: Option<String>,
    timeout: Duration,
) -> std::result::Result<Value, GuestError> {
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/linux_vm_provider",
        move || guest.call(method, &params, request_id.as_deref(), timeout),
    )
    .await
    .map_err(|_| GuestError::Protocol {
        message: "guest provider worker task failed",
        uncertain: true,
    })?
}

/// Send one idempotent provider journal request: `next`, `replay` and `eventRead`
/// change nothing, and the guest answers a repeated `ack` without a change. The
/// guest relays each one to its native supervisor with short deadlines and
/// reports `outcome_unknown` when the supervisor stalls. A stall must not end a
/// live provider connection, so these requests are sent again until
/// `GUEST_PROVIDER_READ_RETRY_SECONDS` pass. A refusal with a proven result is
/// returned at once. Writes never use this path: their identity is reconciled.
async fn read_guest_journal(
    guest: Arc<dyn GuestRpc>,
    params: Value,
) -> std::result::Result<Value, GuestError> {
    let deadline =
        tokio::time::Instant::now() + Duration::from_secs(GUEST_PROVIDER_READ_RETRY_SECONDS);
    let mut delay = Duration::from_millis(250);
    loop {
        match call_guest(
            guest.clone(),
            "provider.rpc",
            params.clone(),
            None,
            PROVIDER_RPC_TIMEOUT,
        )
        .await
        {
            Err(error)
                if (error.uncertain() || matches!(error, GuestError::Transport { .. }))
                    && tokio::time::Instant::now() + delay < deadline =>
            {
                tokio::time::sleep(delay).await;
                delay = (delay * 2).min(Duration::from_secs(2));
            }
            result => return result,
        }
    }
}

fn parse_info(handle: &str, value: &Value, retained: Option<&Value>) -> Result<ProviderInfo> {
    let state = value.get("state").and_then(Value::as_str);
    if state != Some("running") && !(state == Some("exited") && retained.is_some()) {
        return Err(GuestProviderError::Unknown);
    }
    let parsed = parse_info_fields(handle, value)?;
    if let Some(original) = retained {
        // Old versions could persist terminal metadata before validating it.
        // Only a valid running receipt proves the original native identity.
        if original.get("state").and_then(Value::as_str) != Some("running") {
            return Err(GuestProviderError::Unknown);
        }
        let original =
            parse_info_fields(handle, original).map_err(|_| GuestProviderError::Unknown)?;
        if original.generation != parsed.generation
            || original.pid.is_none_or(|pid| pid == 0)
            || original.pid != parsed.pid
        {
            return Err(GuestProviderError::Unknown);
        }
    }
    Ok(parsed)
}

fn parse_info_fields(handle: &str, value: &Value) -> Result<ProviderInfo> {
    let parsed = ProviderInfo {
        handle: value
            .get("handle")
            .and_then(Value::as_str)
            .ok_or(GuestProviderError::InvalidResponse)?
            .to_owned(),
        generation: value
            .get("generation")
            .and_then(Value::as_u64)
            .ok_or(GuestProviderError::InvalidResponse)?,
        sequence: value
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or(GuestProviderError::InvalidResponse)?,
        acknowledged: value
            .get("acknowledged")
            .and_then(Value::as_u64)
            .ok_or(GuestProviderError::InvalidResponse)?,
        init_result: value
            .get("initResult")
            .cloned()
            .filter(|value| !value.is_null()),
        resumed: value
            .get("resumed")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        pid: value
            .get("pid")
            .and_then(Value::as_u64)
            .and_then(|pid| u32::try_from(pid).ok()),
    };
    if parsed.handle != handle || parsed.generation == 0 || parsed.acknowledged > parsed.sequence {
        return Err(GuestProviderError::InvalidResponse);
    }
    Ok(parsed)
}

fn parse_event(value: &Value) -> Result<ProviderEvent> {
    let payload = match value.get("payload") {
        Some(Value::String(encoded)) => serde_json::from_str(encoded)?,
        Some(payload) => payload.clone(),
        None => return Err(GuestProviderError::InvalidResponse),
    };
    let kind = value
        .get("kind")
        .and_then(Value::as_str)
        .ok_or(GuestProviderError::InvalidResponse)?;
    Ok(ProviderEvent {
        sequence: value
            .get("sequence")
            .and_then(Value::as_u64)
            .ok_or(GuestProviderError::InvalidResponse)?,
        generation: value
            .get("generation")
            .and_then(Value::as_u64)
            .ok_or(GuestProviderError::InvalidResponse)?,
        kind: kind.to_owned(),
        payload,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use crate::linux_vm_guest::GuestError;
    use serde_json::json;
    use std::{collections::BTreeMap, sync::Mutex};
    use studio_runtime_process::{
        Generation, OperationId, ProviderHandle,
        provider_seam::{
            AdapterCommand, CommandReceipt, OpenOptions, PreparedWrite, SessionClock, Shutdown,
        },
    };
    use studio_runtime_providers::{Frame, Method, RequestFrame, RequestId};
    use studio_runtime_schema::persistence::REQUEST_RESULT_TTL_MS;
    use studio_runtime_store::request_results::{RequestResults, ResultEnvelope};

    struct FakeGuest {
        starts: Mutex<usize>,
        writes: Mutex<usize>,
        acks: Mutex<Vec<u64>>,
        stops: Mutex<Vec<Value>>,
        credential_puts: Mutex<Vec<(Option<String>, Value)>>,
        credential_failure: Mutex<Option<GuestError>>,
        uncertain_start_once: Mutex<bool>,
        definite_start_failure_once: Mutex<bool>,
        malformed_write_once: Mutex<bool>,
        committed_internal_write_once: Mutex<bool>,
        start_ids: Mutex<Vec<Option<String>>>,
        held_write: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        write_entered: tokio::sync::Notify,
        endless_events: Mutex<bool>,
        next_calls: Mutex<usize>,
        next_changed: tokio::sync::Notify,
        /// `next` answers with these guest errors first, in order.
        next_failures: Mutex<Vec<GuestError>>,
        next_attempts: Mutex<usize>,
    }

    impl FakeGuest {
        fn new(uncertain_start_once: bool) -> Self {
            Self {
                starts: Mutex::new(0),
                writes: Mutex::new(0),
                acks: Mutex::new(Vec::new()),
                stops: Mutex::new(Vec::new()),
                credential_puts: Mutex::new(Vec::new()),
                credential_failure: Mutex::new(None),
                uncertain_start_once: Mutex::new(uncertain_start_once),
                definite_start_failure_once: Mutex::new(false),
                malformed_write_once: Mutex::new(false),
                committed_internal_write_once: Mutex::new(false),
                start_ids: Mutex::new(Vec::new()),
                held_write: Mutex::new(None),
                write_entered: tokio::sync::Notify::new(),
                endless_events: Mutex::new(false),
                next_calls: Mutex::new(0),
                next_changed: tokio::sync::Notify::new(),
                next_failures: Mutex::new(Vec::new()),
                next_attempts: Mutex::new(0),
            }
        }

        fn info() -> Value {
            json!({"handle":"linux-worker:agent-1","generation":3,"sequence":1,
                "lastSeq":1,"acknowledged":0,"pid":42,"resumed":false,
                "initResult":null,"state":"running"})
        }
    }

    impl GuestRpc for FakeGuest {
        fn stream(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            _timeout: Duration,
            _on_frame: &mut dyn FnMut(Value) -> std::result::Result<(), GuestError>,
        ) -> std::result::Result<Value, GuestError> {
            if method == "provider.rpc" && params["action"] == "next" {
                *self.next_attempts.lock().unwrap() += 1;
                let mut failures = self.next_failures.lock().unwrap();
                if !failures.is_empty() {
                    return Err(failures.remove(0));
                }
            }
            match method {
                "provider.start" => {
                    *self.starts.lock().unwrap() += 1;
                    self.start_ids
                        .lock()
                        .unwrap()
                        .push(request_id.map(str::to_owned));
                    let mut definite = self.definite_start_failure_once.lock().unwrap();
                    if *definite {
                        *definite = false;
                        return Err(GuestError::Rejected {
                            code: Some("invalid_params".into()),
                            detail: None,
                            uncertain: false,
                        });
                    }
                    let mut uncertain = self.uncertain_start_once.lock().unwrap();
                    if *uncertain {
                        *uncertain = false;
                        return Err(GuestError::Transport {
                            message: "simulated lost response".into(),
                            uncertain: true,
                        });
                    }
                    assert_eq!(params["handle"], "linux-worker:agent-1");
                    Ok(Self::info())
                }
                "provider.rpc" => match params["action"].as_str().unwrap() {
                    "info" => Ok(Self::info()),
                    "next" if *self.endless_events.lock().unwrap() => {
                        *self.next_calls.lock().unwrap() += 1;
                        self.next_changed.notify_one();
                        let sequence = params["cursor"].as_u64().unwrap() + 1;
                        let payload = json!({"method":"item/started","params":{}});
                        Ok(
                            json!({"event":{"sequence":sequence,"generation":3,"kind":"stdout",
                            "payload":payload.to_string()}}),
                        )
                    }
                    "next" if params["cursor"] == 0 => {
                        let payload = json!({"id":1,"method":"initialize","params":{}});
                        Ok(json!({"event":{"sequence":1,"generation":3,"kind":"stdout",
                            "payload":payload.to_string()}}))
                    }
                    "next" => Ok(json!({"event":null})),
                    "write" => {
                        *self.writes.lock().unwrap() += 1;
                        let held = self.held_write.lock().unwrap().take();
                        if let Some(release) = held {
                            self.write_entered.notify_one();
                            release.recv().unwrap();
                        }
                        let mut internal = self.committed_internal_write_once.lock().unwrap();
                        if *internal {
                            *internal = false;
                            return Err(GuestError::Rejected {
                                code: Some("internal".into()),
                                detail: None,
                                uncertain: false,
                            });
                        }
                        let mut malformed = self.malformed_write_once.lock().unwrap();
                        if *malformed {
                            *malformed = false;
                            return Ok(json!({"accepted":"yes"}));
                        }
                        Ok(json!({"accepted":true,"operationId":params["operationId"]}))
                    }
                    "operationStatus" => {
                        Ok(json!({"accepted":true,"operationId":params["operationId"]}))
                    }
                    "ack" => {
                        self.acks
                            .lock()
                            .unwrap()
                            .push(params["sequence"].as_u64().unwrap());
                        Ok(json!({"acknowledged":params["sequence"]}))
                    }
                    _ => Err(GuestError::Rejected {
                        code: Some("invalid_params".into()),
                        detail: None,
                        uncertain: false,
                    }),
                },
                "provider.stop" => {
                    self.stops.lock().unwrap().push(params.clone());
                    Ok(json!({"handle":params["handle"],"state":"exited"}))
                }
                "credentials.put" => {
                    self.credential_puts
                        .lock()
                        .unwrap()
                        .push((request_id.map(str::to_owned), params.clone()));
                    if let Some(error) = self.credential_failure.lock().unwrap().take() {
                        return Err(error);
                    }
                    Ok(json!({"files":[{"path":params["files"][0]["path"],"bytes":16}]}))
                }
                _ => Err(GuestError::Rejected {
                    code: Some("invalid_request".into()),
                    detail: None,
                    uncertain: false,
                }),
            }
        }
    }

    fn launch() -> ProviderLaunchSpec {
        ProviderLaunchSpec::new(
            vec!["codex".into(), "app-server".into()],
            BTreeMap::from([("CODEX_HOME".into(), "/home/studio/.codex".into())]),
            "/projects/demo".into(),
        )
    }

    async fn open_store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
        store
            .transaction(|tx| {
                studio_runtime_store::receipt_schema::create(tx)?;
                tx.execute_batch("DROP TABLE IF EXISTS runtime_operation_receipts")?;
                AgentRepository::put(
                    tx,
                    &studio_runtime_store::AgentRecord::from_value(json!({
                        "id":"agent-1","rootId":"agent-1","epoch":9,"status":"running",
                        "provider":"codex","accountKey":"default","executionMode":"vm"
                    }))?,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        (directory, store)
    }

    #[tokio::test]
    async fn legacy_deleted_flag_fences_vm_actor_identity() {
        let (_directory, store) = open_store().await;
        store
            .transaction(|tx| {
                let mut record =
                    AgentRepository::get(tx, &AgentId::from("agent-1"))?.expect("fixture agent");
                record.extra.insert("deleted".into(), json!(true));
                AgentRepository::put(tx, &record)
            })
            .await
            .unwrap();
        let connection = store.test_reader().unwrap();
        assert!(!vm_actor_matches(&connection, "agent-1", 9, "default", Some("codex")).unwrap());
    }

    async fn open_session(
        store: Arc<RuntimeStore>,
        guest: Arc<dyn GuestRpc>,
    ) -> Result<GuestProviderSession> {
        GuestProviderSession::open(
            store,
            guest,
            GuestProviderIdentity {
                agent_id: "agent-1".into(),
                epoch: 9,
                generation: 3,
                account_key: "default".into(),
                provider: "codex".into(),
            },
            "profiles/default",
            launch(),
        )
        .await
    }

    #[tokio::test]
    async fn vm_launch_receipt_is_owned_by_the_agent() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        let session = open_session(store.clone(), guest.clone()).await.unwrap();
        let connection = store.test_reader().unwrap();
        let agent = AgentRepository::get(&connection, &AgentId::from("agent-1"))
            .unwrap()
            .unwrap();
        let launch = agent.vm_launch.as_ref().unwrap();
        assert_eq!(launch.status, WorkEffectStatus::Complete);
        assert_eq!(&launch.identity, session.identity());
        drop(connection);
        drop(session);
        open_session(store, guest.clone()).await.unwrap();
        assert_eq!(*guest.starts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn vm_launch_old_epoch_uncertainty_is_held_without_dispatch() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(true));
        assert!(matches!(
            open_session(store.clone(), guest.clone()).await,
            Err(GuestProviderError::Unknown)
        ));
        store
            .transaction(|tx| {
                let mut agent = AgentRepository::get(tx, &AgentId::from("agent-1"))?.unwrap();
                agent.epoch = 10;
                AgentRepository::put(tx, &agent)
            })
            .await
            .unwrap();
        for _ in 0..2 {
            let result = GuestProviderSession::open(
                store.clone(),
                guest.clone(),
                GuestProviderIdentity {
                    agent_id: "agent-1".into(),
                    epoch: 10,
                    generation: 4,
                    account_key: "default".into(),
                    provider: "codex".into(),
                },
                "profiles/default",
                launch(),
            )
            .await;
            assert!(matches!(result, Err(GuestProviderError::Unknown)));
        }
        let connection = store.test_reader().unwrap();
        let held = Outbox::for_owner::<VmObligation>(
            &connection,
            OwnerKind::Agent,
            "agent-1",
            OutboxKind::ProviderCall,
        )
        .unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].status, OutboxStatus::Unknown);
        assert_eq!(held[0].next_at, None);
        assert!(
            Outbox::pending::<VmObligation>(&connection, OutboxKind::ProviderCall, i64::MAX, 10)
                .unwrap()
                .is_empty()
        );
        assert_eq!(*guest.starts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn vm_launch_settlement_keeps_the_first_proof_and_never_downgrades_it() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        let session = open_session(store.clone(), guest).await.unwrap();
        let original = saved_start(&store).await;
        assert!(matches!(
            persist_start(store.clone(), session.identity().clone(), None).await,
            Err(GuestProviderError::Unknown)
        ));
        let mut later_info = FakeGuest::info();
        later_info["sequence"] = json!(2);
        persist_start(store.clone(), session.identity().clone(), Some(later_info))
            .await
            .unwrap();
        assert_eq!(saved_start(&store).await, original);
    }

    async fn bootstrap_source(store: &RuntimeStore, root: &str) -> AdmittedRequest {
        let identity =
            RequestIdentity::new("vm-bootstrap-source", &["agent-1", "9"], root, &[]).unwrap();
        store
            .transaction_with(move |tx| -> Result<AdmittedRequest> {
                match RequestResults::admit(
                    tx,
                    &identity,
                    "source",
                    chrono::Utc::now().timestamp_millis(),
                )? {
                    studio_runtime_store::request_results::Admission::Fresh(token) => {
                        tx.execute("INSERT INTO runtime_execution_attempts(id,run,record) VALUES(?1,'run',?2)",
                            rusqlite::params![token.id(), json!({"id":token.id(),"runId":"run","epoch":9,"events":[],"admission":token}).to_string()])
                            .map_err(StoreError::from)?;
                        Ok(token)
                    },
                    studio_runtime_store::request_results::Admission::Replay(_) => {
                        panic!("fixture source was already completed")
                    }
                }
            })
            .await
            .unwrap()
    }

    async fn admit_bootstrap(
        store: &RuntimeStore,
        root: &str,
        source: &AdmittedRequest,
        operation: &str,
        signature: &str,
    ) -> Result<bool> {
        vm_bootstrap_admission(
            store,
            "agent-1",
            9,
            root,
            VmBootstrapSource {
                attempt_id: source.id(),
                admission: source,
            },
            operation,
            signature,
        )
        .await
    }

    async fn settle_bootstrap(
        store: &RuntimeStore,
        root: &str,
        source: &AdmittedRequest,
        operation: &str,
        signature: &str,
    ) -> Result<()> {
        vm_bootstrap_settled(
            store,
            "agent-1",
            9,
            root,
            VmBootstrapSource {
                attempt_id: source.id(),
                admission: source,
            },
            operation,
            signature,
        )
        .await
    }

    #[tokio::test]
    async fn vm_bootstrap_owner_retains_completion_and_holds_uncertainty() {
        let (_directory, store) = open_store().await;
        let root = RequestIdentity::issue_root();
        let source = bootstrap_source(&store, &root).await;
        assert!(
            !admit_bootstrap(&store, &root, &source, "bootstrap-1", "signature-1")
                .await
                .unwrap()
        );
        assert!(matches!(
            admit_bootstrap(&store, &root, &source, "bootstrap-1", "signature-1").await,
            Err(GuestProviderError::Unknown)
        ));
        assert!(matches!(
            admit_bootstrap(&store, &root, &source, "bootstrap-1", "changed").await,
            Err(GuestProviderError::Unknown)
        ));
        settle_bootstrap(&store, &root, &source, "bootstrap-1", "signature-1")
            .await
            .unwrap();
        settle_bootstrap(&store, &root, &source, "bootstrap-1", "signature-1")
            .await
            .unwrap();
        assert!(
            admit_bootstrap(&store, &root, &source, "bootstrap-1", "signature-1")
                .await
                .unwrap()
        );
        let root2 = RequestIdentity::issue_root();
        let source2 = bootstrap_source(&store, &root2).await;
        let root3 = RequestIdentity::issue_root();
        let source3 = bootstrap_source(&store, &root3).await;
        assert!(
            !admit_bootstrap(&store, &root2, &source2, "bootstrap-2", "signature-2")
                .await
                .unwrap()
        );
        assert!(matches!(
            admit_bootstrap(&store, &root3, &source3, "bootstrap-3", "signature-3").await,
            Err(GuestProviderError::Unknown)
        ));
        let connection = store.test_reader().unwrap();
        let held = Outbox::for_owner::<VmObligation>(
            &connection,
            OwnerKind::Agent,
            "agent-1",
            OutboxKind::ProviderCall,
        )
        .unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].status, OutboxStatus::Unknown);
        assert_eq!(
            held[0].request_id.as_deref(),
            Some(
                source2
                    .derive_result("vm-auth-bootstrap", &["bootstrap-2"], "signature-2")
                    .id()
            )
        );
        drop(connection);
        settle_bootstrap(&store, &root2, &source2, "bootstrap-2", "signature-2")
            .await
            .unwrap();
        assert!(
            admit_bootstrap(&store, &root2, &source2, "bootstrap-2", "signature-2")
                .await
                .unwrap()
        );
        assert!(
            Outbox::for_owner::<VmObligation>(
                &store.test_reader().unwrap(),
                OwnerKind::Agent,
                "agent-1",
                OutboxKind::ProviderCall
            )
            .unwrap()
            .is_empty()
        );
    }

    fn credential_target() -> GuestCredentialTarget<'static> {
        GuestCredentialTarget {
            agent_id: "agent-1",
            actor_epoch: 9,
            provider: "codex",
            account_key: "default",
            relative_path: ".codex/auth.json",
        }
    }

    #[tokio::test]
    async fn vm_bootstrap_refresh_replay_keeps_the_prior_completed_identity() {
        let (_directory, store) = open_store().await;
        let root = RequestIdentity::issue_root();
        let source = bootstrap_source(&store, &root).await;
        let root2 = RequestIdentity::issue_root();
        let source2 = bootstrap_source(&store, &root2).await;
        for (root, source, operation, signature) in [
            (&root, &source, "bootstrap-1", "signature-1"),
            (&root2, &source2, "bootstrap-2", "signature-2"),
        ] {
            assert!(
                !admit_bootstrap(&store, root, source, operation, signature)
                    .await
                    .unwrap()
            );
            settle_bootstrap(&store, root, source, operation, signature)
                .await
                .unwrap();
        }
        assert!(
            admit_bootstrap(&store, &root, &source, "bootstrap-1", "signature-1")
                .await
                .unwrap()
        );
    }

    #[tokio::test]
    async fn vm_bootstrap_replays_the_exact_migrated_source_token_after_a_refresh() {
        let (_directory, store) = open_store().await;
        let root = RequestIdentity::issue_root();
        let source = bootstrap_source(&store, &root).await;
        let old_root = RequestIdentity::issue_root();
        let old_source = bootstrap_source(&store, &old_root).await;
        let imported: AdmittedRequest = serde_json::from_str(&format!(
            r#"{{"id":"account-worker:vm-auth-bootstrap:agent-1:9:old:complete","signature":"complete","created":{}}}"#,
            chrono::Utc::now().timestamp_millis()
        ))
        .unwrap();
        let imported_for_save = imported.clone();
        let old_source_for_save = old_source.clone();
        store
            .transaction_with(move |tx| -> Result<()> {
                ExecutionHistoryRepository::set_vm_auth_bootstrap_admission(
                    tx,
                    old_source_for_save.id(),
                    &old_source_for_save,
                    &imported_for_save,
                )?;
                RequestResults::settle(
                    tx,
                    &imported_for_save,
                    &ResultEnvelope {
                        actor: None,
                        reply: BootstrapCompletion {
                            signature: "old-payload".into(),
                        },
                        references: vec![],
                    },
                )?;
                Ok(())
            })
            .await
            .unwrap();
        assert!(
            !admit_bootstrap(&store, &root, &source, "new-operation", "new-payload")
                .await
                .unwrap()
        );
        settle_bootstrap(&store, &root, &source, "new-operation", "new-payload")
            .await
            .unwrap();
        assert!(
            vm_bootstrap_admission(
                &store,
                "agent-1",
                9,
                &old_root,
                VmBootstrapSource {
                    attempt_id: old_source.id(),
                    admission: &old_source
                },
                "old-operation",
                "old-payload"
            )
            .await
            .unwrap()
        );
        assert!(matches!(
            vm_bootstrap_admission(
                &store,
                "agent-1",
                9,
                &old_root,
                VmBootstrapSource {
                    attempt_id: old_source.id(),
                    admission: &old_source
                },
                "old-operation",
                "changed"
            )
            .await,
            Err(GuestProviderError::Unknown)
        ));
        let agent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from("agent-1"))
            .unwrap()
            .unwrap();
        assert_eq!(
            agent.vm_auth_bootstrap.as_ref().unwrap().request_id,
            "new-operation"
        );
    }

    #[tokio::test]
    async fn vm_bootstrap_expired_source_never_becomes_a_fresh_send_after_gc() {
        let (_directory, store) = open_store().await;
        let root = RequestIdentity::issue_root();
        let source = bootstrap_source(&store, &root).await;
        let expired: AdmittedRequest = serde_json::from_str(&format!(
            r#"{{"id":"expired-bootstrap-effect","signature":"payload","created":{}}}"#,
            chrono::Utc::now().timestamp_millis() - REQUEST_RESULT_TTL_MS - 1000
        ))
        .unwrap();
        let expired_for_save = expired.clone();
        let source_for_save = source.clone();
        store
            .transaction_with(move |tx| -> Result<()> {
                ExecutionHistoryRepository::set_vm_auth_bootstrap_admission(
                    tx,
                    source_for_save.id(),
                    &source_for_save,
                    &expired_for_save,
                )?;
                RequestResults::settle(
                    tx,
                    &expired_for_save,
                    &ResultEnvelope {
                        actor: Some("agent-1".into()),
                        reply: BootstrapCompletion {
                            signature: "payload".into(),
                        },
                        references: vec![],
                    },
                )?;
                assert_eq!(
                    RequestResults::gc(tx, chrono::Utc::now().timestamp_millis())?,
                    1
                );
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(
            vm_bootstrap_admission(
                &store,
                "agent-1",
                9,
                &root,
                VmBootstrapSource {
                    attempt_id: source.id(),
                    admission: &source
                },
                "operation",
                "payload"
            )
            .await,
            Err(GuestProviderError::Unknown)
        ));
        let agent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from("agent-1"))
            .unwrap()
            .unwrap();
        assert!(agent.vm_auth_bootstrap.is_none());
    }

    #[tokio::test]
    async fn vm_bootstrap_gc_then_clock_rollback_never_repeats_a_prior_source() {
        let (_directory, store) = open_store().await;
        let root = RequestIdentity::issue_root();
        let source = bootstrap_source(&store, &root).await;
        let root2 = RequestIdentity::issue_root();
        let source2 = bootstrap_source(&store, &root2).await;
        for (root, source, operation) in [
            (&root, &source, "bootstrap-A"),
            (&root2, &source2, "bootstrap-B"),
        ] {
            assert!(
                !admit_bootstrap(&store, root, source, operation, "payload")
                    .await
                    .unwrap()
            );
            settle_bootstrap(&store, root, source, operation, "payload")
                .await
                .unwrap();
        }
        store
            .transaction_with(|tx| -> Result<()> {
                // GC observes a later clock; the adapter then observes the current
                // clock again. No process clock or external service is changed.
                assert_eq!(
                    RequestResults::gc(
                        tx,
                        chrono::Utc::now().timestamp_millis() + REQUEST_RESULT_TTL_MS + 1000
                    )?,
                    2
                );
                Ok(())
            })
            .await
            .unwrap();
        assert!(matches!(
            admit_bootstrap(&store, &root, &source, "bootstrap-A", "payload").await,
            Err(GuestProviderError::Unknown)
        ));
        let agent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from("agent-1"))
            .unwrap()
            .unwrap();
        assert_eq!(
            agent.vm_auth_bootstrap.as_ref().unwrap().request_id,
            "bootstrap-B"
        );
    }

    #[tokio::test]
    async fn credential_copy_only_retries_a_proven_unsent_request() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        *guest.credential_failure.lock().unwrap() = Some(GuestError::Rejected {
            code: Some("invalid_params".into()),
            detail: None,
            uncertain: false,
        });
        assert!(matches!(
            copy_guest_credentials(store.clone(), guest.clone(), credential_target(), b"{}").await,
            Err(GuestProviderError::Guest(_))
        ));
        copy_guest_credentials(store.clone(), guest.clone(), credential_target(), b"{}")
            .await
            .unwrap();
        copy_guest_credentials(store, guest.clone(), credential_target(), b"{}")
            .await
            .unwrap();
        let puts = guest.credential_puts.lock().unwrap();
        assert_eq!(puts.len(), 2);
        assert_eq!(puts[0].0, puts[1].0);
    }

    #[tokio::test]
    async fn credential_copy_lost_response_holds_the_exact_and_replaced_identity() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        *guest.credential_failure.lock().unwrap() = Some(GuestError::Transport {
            message: "lost reply".into(),
            uncertain: true,
        });
        for document in [b"{}".as_slice(), b"{}", b"{\"changed\":true}", b"{}"] {
            assert!(matches!(
                copy_guest_credentials(store.clone(), guest.clone(), credential_target(), document)
                    .await,
                Err(GuestProviderError::Unknown)
            ));
        }
        assert_eq!(guest.credential_puts.lock().unwrap().len(), 1);
        let connection = store.test_reader().unwrap();
        let held = Outbox::for_owner::<VmObligation>(
            &connection,
            OwnerKind::Agent,
            "agent-1",
            OutboxKind::ProviderCall,
        )
        .unwrap();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].status, OutboxStatus::Unknown);
        assert_eq!(held[0].next_at, None);
    }

    #[tokio::test]
    async fn credential_copy_imported_pending_and_completed_rows_never_dispatch() {
        for pending in [true, false] {
            let (_directory, store) = open_store().await;
            let guest = Arc::new(FakeGuest::new(false));
            let target = credential_target();
            let material = serde_json::to_vec(&json!({
                "agentId":target.agent_id,"accountKey":target.account_key,
                "path":target.relative_path,"digest":hex_digest(b"{}"),
            }))
            .unwrap();
            let signature = hex_digest(&material);
            let request = format!("{CREDENTIAL_RECEIPT_PREFIX}{}", &signature[..48]);
            store.transaction_with(move |tx| -> Result<()> {
                if pending {
                    Outbox::put(tx, &OutboxRow {
                        id: Outbox::effect_id(&request, OutboxKind::ProviderCall, OwnerKind::Vm, &request, &["credentialCopy"]),
                        kind: OutboxKind::ProviderCall, owner_kind: OwnerKind::Vm, owner_id: request.clone(),
                        request_id: Some(request.clone()), signature: signature.clone(), status: OutboxStatus::Unknown,
                        account: None, stream_id: None, generation: None, ordinal: None, next_at: None,
                        payload: VmObligation::CredentialCopy(CredentialCopyHold {
                            request_id: request, signature, status: WorkEffectStatus::Submitted, copy: None,
                        }), created: chrono::Utc::now().timestamp_millis(),
                    })?;
                } else {
                    let result = ResultEnvelope {actor: None, reply: WorkEffectStatus::Complete, references: vec![]};
                    tx.execute("INSERT INTO request_results(id,signature,result,created) VALUES(?1,?2,?3,?4)",
                        rusqlite::params![request, signature, serde_json::to_string(&result)?, chrono::Utc::now().timestamp_millis()])
                        .map_err(StoreError::from)?;
                }
                Ok(())
            }).await.unwrap();
            let result = copy_guest_credentials(store.clone(), guest.clone(), target, b"{}").await;
            if pending {
                assert!(matches!(result, Err(GuestProviderError::Unknown)));
            } else {
                assert_eq!(result.unwrap(), hex_digest(b"{}"));
                let agent =
                    AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from("agent-1"))
                        .unwrap()
                        .unwrap();
                assert_eq!(
                    agent.credential_copy.as_ref().unwrap().status,
                    WorkEffectStatus::Complete
                );
            }
            assert!(guest.credential_puts.lock().unwrap().is_empty());
            store
                .transaction(|tx| {
                    let mut agent = AgentRepository::get(tx, &AgentId::from("agent-1"))?.unwrap();
                    agent.extra.insert("deletedAt".into(), json!(1));
                    AgentRepository::put(tx, &agent)
                })
                .await
                .unwrap();
            assert!(matches!(
                copy_guest_credentials(store, guest, target, b"{}").await,
                Err(GuestProviderError::Refused)
            ));
        }
    }

    struct TerminalInfoGuest {
        inner: FakeGuest,
        state: &'static str,
    }

    struct RetainedJournalGuest {
        inner: FakeGuest,
        info: Mutex<Value>,
        events: Vec<Value>,
    }

    impl RetainedJournalGuest {
        fn new() -> Self {
            let mut info = FakeGuest::info();
            info["sequence"] = json!(0);
            info["lastSeq"] = json!(0);
            info["initResult"] = json!({"capabilities": {}});
            Self {
                inner: FakeGuest::new(false),
                info: Mutex::new(info),
                events: vec![
                    json!({"sequence":1,"generation":3,"kind":"stdout",
                        "payload":json!({"id":7,"result":{"tail":"saved reply"}}).to_string()}),
                    json!({"sequence":2,"generation":3,"kind":"exit",
                        "payload":json!({"returnCode":0}).to_string()}),
                ],
            }
        }

        fn exit(&self) {
            let mut info = self.info.lock().unwrap();
            info["state"] = json!("exited");
            info["exitCode"] = json!(0);
            info["sequence"] = json!(self.events.len());
            info["lastSeq"] = json!(self.events.len());
        }
    }

    impl GuestRpc for RetainedJournalGuest {
        fn stream(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            timeout: Duration,
            on_frame: &mut dyn FnMut(Value) -> std::result::Result<(), GuestError>,
        ) -> std::result::Result<Value, GuestError> {
            if method == "provider.rpc" && params["action"] == "info" {
                return Ok(self.info.lock().unwrap().clone());
            }
            if method == "provider.rpc" && params["action"] == "next" {
                let cursor = params["cursor"].as_u64().unwrap() as usize;
                return Ok(json!({"event":self.events.get(cursor),"returnCode":0}));
            }
            let result = self
                .inner
                .stream(method, params, request_id, timeout, on_frame)?;
            if method == "provider.start" {
                Ok(self.info.lock().unwrap().clone())
            } else {
                Ok(result)
            }
        }
    }

    impl GuestRpc for TerminalInfoGuest {
        fn stream(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            timeout: Duration,
            on_frame: &mut dyn FnMut(Value) -> std::result::Result<(), GuestError>,
        ) -> std::result::Result<Value, GuestError> {
            let mut value = self
                .inner
                .stream(method, params, request_id, timeout, on_frame)?;
            if method == "provider.start" || params["action"] == "info" {
                value["state"] = json!(self.state);
            }
            Ok(value)
        }
    }

    async fn saved_start(store: &RuntimeStore) -> String {
        store
            .read_async("linux_vm_provider/test_start_receipt", |connection| {
                let agent = AgentRepository::get(connection, &AgentId::from("agent-1"))?.unwrap();
                Ok::<_, StoreError>(serde_json::to_string(&agent.vm_launch).unwrap())
            })
            .await
            .unwrap()
    }

    #[tokio::test]
    async fn a_proven_started_provider_drains_its_exit_tail_without_relaunch() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(RetainedJournalGuest::new());
        let first = open_session(store.clone(), guest.clone()).await.unwrap();
        let identity = first.identity().clone();
        let started = saved_start(&store).await;
        drop(first);
        guest.exit();
        let reopened = open_session(store.clone(), guest.clone())
            .await
            .expect("the proven generation keeps its retained journal after exit");
        assert_eq!(reopened.identity(), &identity);
        assert_eq!(
            reopened.info().init_result,
            Some(json!({"capabilities":{}}))
        );
        let reply = reopened.next().await.unwrap().expect("retained stdout");
        assert_eq!(reply.kind, "stdout");
        assert_eq!(
            reply.payload,
            json!({"id":7,"result":{"tail":"saved reply"}})
        );
        reopened.settle_then_ack(&reply, || Ok(())).await.unwrap();
        let exit = reopened.next().await.unwrap().expect("retained exit");
        assert_eq!(exit.kind, "exit");
        assert_eq!(exit.payload, json!({"returnCode":0}));
        reopened.settle_then_ack(&exit, || Ok(())).await.unwrap();
        assert!(reopened.next().await.unwrap().is_none());
        assert_eq!(*guest.inner.starts.lock().unwrap(), 1);
        assert_eq!(*guest.inner.writes.lock().unwrap(), 0);
        assert_eq!(*guest.inner.acks.lock().unwrap(), vec![1, 2]);
        assert_eq!(
            saved_start(&store).await,
            started,
            "retain the original running proof"
        );
    }

    #[tokio::test]
    async fn retained_terminal_reconciliation_requires_the_original_running_identity() {
        for refusal in [
            "lost",
            "generation",
            "pid",
            "legacy lost proof",
            "legacy exited proof",
        ] {
            let (_directory, store) = open_store().await;
            let guest = Arc::new(RetainedJournalGuest::new());
            drop(open_session(store.clone(), guest.clone()).await.unwrap());
            guest.exit();
            match refusal {
                "lost" => guest.info.lock().unwrap()["state"] = json!("lost"),
                "generation" => guest.info.lock().unwrap()["generation"] = json!(4),
                "pid" => guest.info.lock().unwrap()["pid"] = json!(43),
                _ => {
                    let mut saved: VmLaunch =
                        serde_json::from_str(&saved_start(&store).await).unwrap();
                    saved.info.as_mut().unwrap()["state"] =
                        json!(if refusal == "legacy lost proof" {
                            "lost"
                        } else {
                            "exited"
                        });
                    store
                        .transaction(move |tx| {
                            let mut agent =
                                AgentRepository::get(tx, &AgentId::from("agent-1"))?.unwrap();
                            agent.vm_launch = Some(saved);
                            AgentRepository::put(tx, &agent)
                        })
                        .await
                        .unwrap();
                }
            }
            let saved = saved_start(&store).await;
            assert!(
                matches!(
                    open_session(store.clone(), guest.clone()).await,
                    Err(GuestProviderError::Unknown)
                ),
                "refuse {refusal}"
            );
            assert_eq!(saved_start(&store).await, saved);
            assert_eq!(*guest.inner.starts.lock().unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn terminal_guest_info_keeps_the_reserved_start_without_relaunch() {
        for state in ["lost", "exited"] {
            let (_directory, store) = open_store().await;
            let guest = Arc::new(TerminalInfoGuest {
                inner: FakeGuest::new(true),
                state,
            });
            assert!(matches!(
                open_session(store.clone(), guest.clone()).await,
                Err(GuestProviderError::Unknown)
            ));
            let reserved = saved_start(&store).await;
            assert_eq!(
                serde_json::from_str::<Value>(&reserved).unwrap()["status"],
                "submitted"
            );
            assert!(
                matches!(
                    open_session(store.clone(), guest.clone()).await,
                    Err(GuestProviderError::Unknown)
                ),
                "{state} is not proof of a running provider"
            );
            assert_eq!(saved_start(&store).await, reserved);
            assert_eq!(*guest.inner.starts.lock().unwrap(), 1);
        }
    }

    #[tokio::test]
    async fn a_terminal_start_reply_never_becomes_a_started_receipt() {
        for state in ["lost", "exited"] {
            let (_directory, store) = open_store().await;
            let guest = Arc::new(TerminalInfoGuest {
                inner: FakeGuest::new(false),
                state,
            });
            assert!(
                matches!(
                    open_session(store.clone(), guest.clone()).await,
                    Err(GuestProviderError::Unknown)
                ),
                "{state} cannot complete a start"
            );
            let reserved = saved_start(&store).await;
            assert_eq!(
                serde_json::from_str::<Value>(&reserved).unwrap()["status"],
                "submitted"
            );
            assert!(open_session(store.clone(), guest.clone()).await.is_err());
            assert_eq!(saved_start(&store).await, reserved);
            assert_eq!(*guest.inner.starts.lock().unwrap(), 1);
        }
    }

    #[cfg(unix)]
    mod start_transport {
        use super::*;
        use crate::{
            linux_vm_guest::UnixGuestRpc,
            provider_session_task::{
                SessionHandle, SessionOutcome, SessionTaskConfig, WriteOutcome,
            },
        };
        use std::{
            io::{BufRead, BufReader, Write},
            os::unix::net::UnixListener,
        };

        // Inject only the socket deadline. Production still supplies its own
        // start budget, which every real transport witness checks below.
        struct ShortSocketDeadline {
            inner: UnixGuestRpc,
            observed: Mutex<Vec<Duration>>,
        }

        impl GuestRpc for ShortSocketDeadline {
            fn stream(
                &self,
                method: &str,
                params: &Value,
                request_id: Option<&str>,
                timeout: Duration,
                on_frame: &mut dyn FnMut(Value) -> std::result::Result<(), GuestError>,
            ) -> std::result::Result<Value, GuestError> {
                self.observed.lock().unwrap().push(timeout);
                self.inner.stream(
                    method,
                    params,
                    request_id,
                    timeout.min(Duration::from_secs(1)),
                    on_frame,
                )
            }
        }

        enum Reply {
            Unknown,
            Lost,
            Withheld,
        }

        async fn failed_start(reply: Reply) {
            let (directory, store) = open_store().await;
            let listener = UnixListener::bind(directory.path().join("guest.sock")).unwrap();
            let (release, held) = std::sync::mpsc::channel();
            let server = std::thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(5)))
                    .unwrap();
                let mut line = String::new();
                BufReader::new(&mut socket).read_line(&mut line).unwrap();
                let request: Value = serde_json::from_str(&line).unwrap();
                assert_eq!(request["method"], "provider.start");
                match reply {
                    Reply::Unknown => {
                        let response = json!({"id":request["id"],"error":{"code":"outcome_unknown","message":"native result is unknown"}});
                        writeln!(socket, "{response}").unwrap();
                    }
                    Reply::Lost => {}
                    Reply::Withheld => {
                        let _released = held.recv_timeout(Duration::from_secs(5));
                    }
                }
                drop(socket);
                listener.set_nonblocking(true).unwrap();
                assert_eq!(
                    listener.accept().unwrap_err().kind(),
                    std::io::ErrorKind::WouldBlock,
                    "the failed session must not dispatch another start"
                );
                request
            });
            let guest = Arc::new(ShortSocketDeadline {
                inner: UnixGuestRpc::new(directory.path()),
                observed: Mutex::new(Vec::new()),
            });
            let factory = Arc::new(GuestProviderTaskFactory::new(
                store.clone(),
                guest.clone(),
                GuestProviderIdentity {
                    agent_id: "agent-1".into(),
                    epoch: 9,
                    generation: 3,
                    account_key: "default".into(),
                    provider: "codex".into(),
                },
                "profiles/default",
            ));
            let mut session = SessionHandle::open(
                factory,
                ProviderHandle::for_account("agent-1"),
                OpenOptions {
                    clock: SessionClock::default(),
                    launch: launch(),
                    expected_generation: Some(Generation(3)),
                },
                SessionTaskConfig {
                    initialize_params: json!({}),
                    generation: Generation(3),
                    wire: studio_runtime_providers::ProviderKind::Codex.wire(),
                    timeout_ms: 1000,
                    initialize_operation: OperationId("vm-test-initialize".into()),
                    request_id_prefix: None,
                    recovered: vec![],
                },
            )
            .await
            .unwrap();
            let completed = tokio::time::timeout(Duration::from_secs(4), async {
                assert!(
                    session.events.recv().await.is_none(),
                    "a failed open emits no provider journal"
                );
                assert!(
                    session.status.changed().await.is_err(),
                    "initialization must close after the open failure"
                );
                session.completion().await
            })
            .await;
            let _released = release.send(());
            let request = server.join().unwrap();
            let exit = completed.expect("the actual socket deadline ends startup");
            assert!(exit.result.is_err());
            assert!(exit.adapter.is_none());
            assert!(exit.outcomes.iter().any(|outcome| matches!(
                outcome,
                SessionOutcome::Write {
                    outcome: WriteOutcome::NotSent,
                    ..
                }
            )));
            assert!(exit.outcomes.iter().any(|outcome| matches!(
                outcome,
                SessionOutcome::GenerationClosed { generation: 3 }
            )));
            let reserved = saved_start(&store).await;
            let saved: Value = serde_json::from_str(&reserved).unwrap();
            assert_eq!(saved["status"], "submitted");
            assert_eq!(saved["identity"]["requestId"], request["id"]);
            assert_eq!(
                guest.observed.lock().unwrap().as_slice(),
                &[Duration::from_secs(300)],
                "the production start budget must not inherit the one-hour general RPC timeout"
            );
            assert_eq!(saved_start(&store).await, reserved);
        }

        #[tokio::test]
        async fn unknown_start_reply_closes_initialization_without_retry() {
            failed_start(Reply::Unknown).await;
        }

        #[tokio::test]
        async fn lost_start_reply_closes_initialization_without_retry() {
            failed_start(Reply::Lost).await;
        }

        #[tokio::test]
        async fn withheld_start_reply_hits_the_socket_deadline_without_retry() {
            failed_start(Reply::Withheld).await;
        }
    }

    #[tokio::test]
    async fn lifecycle_writes_and_acknowledges_only_after_durable_settlement() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        let session = open_session(store, guest.clone()).await.unwrap();
        assert_eq!(session.info().generation, 3);
        let event = session.next().await.unwrap().unwrap();
        assert_eq!(event.kind, "stdout");
        assert_eq!(guest.acks.lock().unwrap().len(), 0);
        let settled = std::cell::Cell::new(false);
        session
            .settle_then_ack(&event, || {
                settled.set(true);
                Ok(())
            })
            .await
            .unwrap();
        assert!(settled.get());
        assert_eq!(*guest.acks.lock().unwrap(), vec![1]);
        session
            .write(
                "turn-op-1",
                &json!(7),
                json!({"id":7,"method":"turn/start"}),
            )
            .await
            .unwrap();
        session.stop().await.unwrap();
        assert_eq!(*guest.starts.lock().unwrap(), 1);
        assert_eq!(*guest.writes.lock().unwrap(), 1);
        assert_eq!(
            guest.stops.lock().unwrap()[0]["handle"],
            "linux-worker:agent-1"
        );
    }

    #[tokio::test]
    async fn an_unproven_journal_read_is_read_again_and_keeps_the_connection() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        let session = open_session(store, guest.clone()).await.unwrap();
        // The guest supervisor stalled: the guest could not prove the read result.
        *guest.next_failures.lock().unwrap() = vec![
            GuestError::Rejected {
                code: Some("outcome_unknown".into()),
                detail: Some("The native supervisor operation has no proven result".into()),
                uncertain: true,
            },
            GuestError::Transport {
                message: "timed out".into(),
                uncertain: true,
            },
        ];
        tokio::time::pause();
        let event = session.next().await.unwrap().unwrap();
        tokio::time::resume();
        assert_eq!(event.sequence, 1);
        assert_eq!(*guest.next_attempts.lock().unwrap(), 3);
        assert_eq!(*guest.writes.lock().unwrap(), 0);
    }

    #[tokio::test]
    async fn a_proven_journal_read_refusal_is_not_sent_again() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        let session = open_session(store, guest.clone()).await.unwrap();
        *guest.next_failures.lock().unwrap() = vec![GuestError::Rejected {
            code: Some("not_found".into()),
            detail: None,
            uncertain: false,
        }];
        assert!(session.next().await.is_err());
        assert_eq!(*guest.next_attempts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn uncertain_start_is_reconciled_without_resending_after_restart() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(true));
        assert!(matches!(
            open_session(store.clone(), guest.clone()).await,
            Err(GuestProviderError::Unknown)
        ));
        let session = open_session(store, guest.clone()).await.unwrap();
        assert_eq!(session.identity().handle, "linux-worker:agent-1");
        assert_eq!(*guest.starts.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn proven_unsent_start_retries_same_identity_without_info_reconciliation() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        *guest.definite_start_failure_once.lock().unwrap() = true;
        assert!(matches!(
            open_session(store.clone(), guest.clone()).await,
            Err(GuestProviderError::Guest(_))
        ));
        let session = open_session(store, guest.clone()).await.unwrap();
        assert_eq!(session.identity().handle, "linux-worker:agent-1");
        let ids = guest.start_ids.lock().unwrap();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
        assert_eq!(*guest.starts.lock().unwrap(), 2);
    }

    #[tokio::test]
    async fn proven_unsent_retry_reserves_before_transport_and_never_repeats_unknown() {
        struct ReceiptObservingGuest {
            inner: FakeGuest,
            store: Arc<RuntimeStore>,
            dispatch_receipts: Mutex<Vec<VmLaunch>>,
        }

        impl GuestRpc for ReceiptObservingGuest {
            fn stream(
                &self,
                method: &str,
                params: &Value,
                request_id: Option<&str>,
                timeout: Duration,
                on_frame: &mut dyn FnMut(Value) -> std::result::Result<(), GuestError>,
            ) -> std::result::Result<Value, GuestError> {
                if method == "provider.start" {
                    let agent = AgentRepository::get(
                        &self.store.test_reader().unwrap(),
                        &AgentId::from("agent-1"),
                    )
                    .unwrap()
                    .unwrap();
                    self.dispatch_receipts
                        .lock()
                        .unwrap()
                        .push(agent.vm_launch.unwrap());
                }
                self.inner
                    .stream(method, params, request_id, timeout, on_frame)
            }
        }

        let (_directory, store) = open_store().await;
        let guest = Arc::new(ReceiptObservingGuest {
            inner: FakeGuest::new(true),
            store: store.clone(),
            dispatch_receipts: Mutex::new(Vec::new()),
        });
        *guest.inner.definite_start_failure_once.lock().unwrap() = true;
        assert!(matches!(
            open_session(store.clone(), guest.clone()).await,
            Err(GuestProviderError::Guest(_))
        ));
        let not_sent: VmLaunch = serde_json::from_str(&saved_start(&store).await).unwrap();
        assert_eq!(not_sent.status, WorkEffectStatus::NotSent);
        assert!(matches!(
            open_session(store.clone(), guest.clone()).await,
            Err(GuestProviderError::Unknown)
        ));
        let after_unknown: VmLaunch = serde_json::from_str(&saved_start(&store).await).unwrap();
        let reopened = open_session(store, guest.clone()).await.unwrap();

        assert_eq!(*guest.inner.starts.lock().unwrap(), 2);
        let dispatched = guest.dispatch_receipts.lock().unwrap();
        assert_eq!(dispatched.len(), 2);
        for receipt in dispatched.iter() {
            assert_eq!(receipt.status, WorkEffectStatus::Submitted);
            assert_eq!(receipt.identity, not_sent.identity);
        }
        assert_eq!(after_unknown.status, WorkEffectStatus::Submitted);
        assert_eq!(after_unknown.identity, not_sent.identity);
        assert_eq!(reopened.identity(), &not_sent.identity);
        let ids = guest.inner.start_ids.lock().unwrap();
        assert_eq!(ids[0], ids[1]);
        assert_eq!(
            ids[0].as_deref(),
            Some(not_sent.identity.request_id.as_str())
        );
    }

    #[tokio::test]
    async fn committed_write_with_guest_internal_error_reconciles_without_resend() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        let session = open_session(store, guest.clone()).await.unwrap();
        *guest.committed_internal_write_once.lock().unwrap() = true;

        let receipt = session
            .write(
                "turn-op-committed-before-error",
                &json!(8),
                json!({"id":8,"method":"turn/start"}),
            )
            .await
            .expect("operationStatus proves the committed write");

        assert_eq!(receipt["accepted"], true);
        assert_eq!(receipt["operationId"], "turn-op-committed-before-error");
        assert_eq!(*guest.writes.lock().unwrap(), 1);
    }

    #[tokio::test]
    async fn failed_settlement_does_not_ack_guest_event() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        let session = open_session(store, guest.clone()).await.unwrap();
        let event = session.next().await.unwrap().unwrap();
        let result = session
            .settle_then_ack(&event, || Err::<(), _>(GuestProviderError::Unknown))
            .await;
        assert!(matches!(result, Err(GuestProviderError::Unknown)));
        assert!(guest.acks.lock().unwrap().is_empty());
        assert_eq!(session.cursor(), 0);
    }

    #[tokio::test]
    async fn credential_copy_receipt_contains_no_secret_material() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        store
            .transaction(|tx| {
                AgentRepository::put(
                    tx,
                    &studio_runtime_store::AgentRecord::from_value(json!({
                        "id":"agent-1","rootId":"agent-1","epoch":9,"status":"running",
                        "provider":"claude","accountKey":"account-1","executionMode":"vm"
                    }))?,
                )?;
                Ok(())
            })
            .await
            .unwrap();
        let secret = b"{\"accessToken\":\"private-token\"}";
        let digest = copy_guest_credentials(
            store.clone(),
            guest.clone(),
            GuestCredentialTarget {
                agent_id: "agent-1",
                actor_epoch: 9,
                provider: "claude",
                account_key: "account-1",
                relative_path: ".claude/studio-accounts/key/.credentials.json",
            },
            secret,
        )
        .await
        .unwrap();
        let puts = guest.credential_puts.lock().unwrap();
        assert_eq!(puts.len(), 1);
        assert!(
            puts[0]
                .0
                .as_ref()
                .unwrap()
                .starts_with(CREDENTIAL_RECEIPT_PREFIX)
        );
        drop(puts);
        let agent = AgentRepository::get(&store.test_reader().unwrap(), &AgentId::from("agent-1"))
            .unwrap()
            .unwrap();
        let receipt = agent.credential_copy.unwrap();
        assert_eq!(receipt.status, WorkEffectStatus::Complete);
        assert!(
            !serde_json::to_string(&receipt)
                .unwrap()
                .contains("private-token")
        );
        assert_eq!(
            digest,
            "740707bd18f54cdc7ed7de3947165486e8cc3dcf7b4d1f6c300cd5c0d641d85d"
        );
        assert_eq!(
            receipt.signature,
            "ce9eaf46470ff0819976ff38033a4601097be67230e404dfb0dddf2672a8e90f"
        );
    }

    #[tokio::test]
    async fn provider_task_factory_exposes_guest_turn_lanes() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        let factory = GuestProviderTaskFactory::new(
            store,
            guest.clone(),
            GuestProviderIdentity {
                agent_id: "agent-1".into(),
                epoch: 9,
                generation: 3,
                account_key: "default".into(),
                provider: "codex".into(),
            },
            "profiles/default",
        );
        let handle = ProviderHandle::for_account("agent-1");
        let shutdown = Shutdown::default();
        let mut attachment = factory
            .open(
                handle,
                OpenOptions {
                    clock: SessionClock::default(),
                    launch: launch(),
                    expected_generation: Some(Generation(3)),
                },
                shutdown,
            )
            .await
            .unwrap();

        let initialize = Frame::Request(RequestFrame {
            id: RequestId::Number(1),
            method: Method::Initialize,
            params: Some(json!({})),
            fields: Default::default(),
        });
        let write = PreparedWrite::new(OperationId("initialize-op".into()), &initialize).unwrap();
        let (command, receipt) = AdapterCommand::write(write);
        attachment.commands.send(command).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), receipt.receive())
                .await
                .unwrap(),
            CommandReceipt::Written { .. }
        ));

        let entry = tokio::time::timeout(Duration::from_secs(2), attachment.journal.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(entry.cursor.0, 1);
        let (command, receipt) = AdapterCommand::ack(entry.cursor);
        attachment.commands.send(command).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), receipt.receive())
                .await
                .unwrap(),
            CommandReceipt::Acknowledged(_)
        ));
        assert_eq!(*guest.acks.lock().unwrap(), vec![1]);

        let turn = Frame::Request(RequestFrame {
            id: RequestId::Number(2),
            method: Method::TurnStart,
            params: Some(json!({"threadId":"thread-1","input":[]})),
            fields: Default::default(),
        });
        let write = PreparedWrite::new(OperationId("turn-op".into()), &turn).unwrap();
        let (command, receipt) = AdapterCommand::write(write);
        attachment.commands.send(command).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), receipt.receive())
                .await
                .unwrap(),
            CommandReceipt::Written { .. }
        ));
        assert_eq!(*guest.writes.lock().unwrap(), 2);

        let exit = tokio::time::timeout(Duration::from_secs(3), attachment.shutdown())
            .await
            .unwrap();
        assert_eq!(exit.cleanup_error, None);
        assert!(guest.stops.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn malformed_successful_guest_write_reply_remains_unknown() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        *guest.malformed_write_once.lock().unwrap() = true;
        let factory = GuestProviderTaskFactory::new(
            store,
            guest.clone(),
            GuestProviderIdentity {
                agent_id: "agent-1".into(),
                epoch: 9,
                generation: 3,
                account_key: "default".into(),
                provider: "codex".into(),
            },
            "profiles/default",
        );
        let mut attachment = factory
            .open(
                ProviderHandle::for_account("agent-1"),
                OpenOptions {
                    clock: SessionClock::default(),
                    launch: launch(),
                    expected_generation: Some(Generation(3)),
                },
                Shutdown::default(),
            )
            .await
            .unwrap();
        let request = Frame::Request(RequestFrame {
            id: RequestId::Number(41),
            method: Method::Initialize,
            params: Some(json!({})),
            fields: Default::default(),
        });
        let write = PreparedWrite::new(OperationId("malformed-write".into()), &request).unwrap();
        let (command, receipt) = AdapterCommand::write(write);
        attachment.commands.send(command).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), receipt.receive())
                .await
                .unwrap(),
            CommandReceipt::Unknown
        ));
        assert_eq!(*guest.writes.lock().unwrap(), 1);
        let _exit = attachment.shutdown().await;
    }

    async fn open_attachment(
        store: Arc<RuntimeStore>,
        guest: Arc<FakeGuest>,
        shutdown: Shutdown,
    ) -> ProviderAttachment {
        GuestProviderTaskFactory::new(
            store,
            guest,
            GuestProviderIdentity {
                agent_id: "agent-1".into(),
                epoch: 9,
                generation: 3,
                account_key: "default".into(),
                provider: "codex".into(),
            },
            "profiles/default",
        )
        .open(
            ProviderHandle::for_account("agent-1"),
            OpenOptions {
                clock: SessionClock::default(),
                launch: launch(),
                expected_generation: Some(Generation(3)),
            },
            shutdown,
        )
        .await
        .unwrap()
    }

    /// A guest that applies the guest supervisor's rules
    /// (`codex_process_supervisor.py`, `Child.write` and `_append_frame`):
    /// only an integer native ID with a method is renumbered from the handle's
    /// RPC sequence, and a stdout reply with ID 1 and a result is saved as the
    /// handle's initialize result, which `info` returns on reattach.
    #[derive(Default)]
    struct SupervisorRulesGuest {
        rpc_sequence: Mutex<u64>,
        events: Mutex<Vec<Value>>,
        init_result: Mutex<Option<Value>>,
        writes: Mutex<Vec<Value>>,
    }

    impl SupervisorRulesGuest {
        fn info(&self) -> Value {
            let events = self.events.lock().unwrap();
            json!({"handle":"linux-worker:agent-1","generation":3,
                "sequence":events.len(),"lastSeq":events.len(),"acknowledged":0,
                "pid":42,"resumed":true,"initResult":*self.init_result.lock().unwrap(),
                "state":"running"})
        }
    }

    impl GuestRpc for SupervisorRulesGuest {
        fn stream(
            &self,
            method: &str,
            params: &Value,
            _request_id: Option<&str>,
            _timeout: Duration,
            _on_frame: &mut dyn FnMut(Value) -> std::result::Result<(), GuestError>,
        ) -> std::result::Result<Value, GuestError> {
            match (method, params["action"].as_str()) {
                ("provider.start", _) | ("provider.rpc", Some("info")) => Ok(self.info()),
                ("provider.rpc", Some("write")) => {
                    self.writes.lock().unwrap().push(params.clone());
                    let message = &params["message"];
                    let mut remote = params["nativeId"].clone();
                    if remote.is_i64() && message.get("method").is_some() {
                        let mut sequence = self.rpc_sequence.lock().unwrap();
                        *sequence += 1;
                        remote = Value::from(*sequence);
                    }
                    if message["method"] == "initialize" {
                        // The bridge answers on stdout with the ID it received.
                        let result = json!({"userAgent":"claude-bridge/1"});
                        if remote == json!(1) {
                            *self.init_result.lock().unwrap() = Some(result.clone());
                        }
                        let mut events = self.events.lock().unwrap();
                        let sequence = events.len() + 1;
                        events.push(json!({"sequence":sequence,"generation":3,"kind":"stdout",
                            "payload":json!({"id":remote,"result":result}).to_string()}));
                    }
                    Ok(json!({"accepted":true,"duplicate":false,"remoteId":remote}))
                }
                ("provider.rpc", Some("next")) => {
                    let cursor = usize::try_from(params["cursor"].as_u64().unwrap()).unwrap();
                    let events = self.events.lock().unwrap();
                    Ok(json!({"event":events.get(cursor).cloned(),"returnCode":null}))
                }
                ("provider.rpc", Some("ack")) => Ok(json!({"acknowledged":params["sequence"]})),
                _ => Err(GuestError::Rejected {
                    code: Some("invalid_params".into()),
                    detail: None,
                    uncertain: false,
                }),
            }
        }
    }

    // Rust Studio gives Claude bridge requests string IDs. The saved result must
    // still let a reattach answer initialize without writing it to the live
    // bridge again, as the Codex path and the Python host do.
    #[tokio::test]
    async fn claude_initialize_result_is_saved_with_the_handle_and_reused_on_reattach() {
        let (_directory, store) = open_store().await;
        store
            .transaction(|tx| {
                let mut record =
                    AgentRepository::get(tx, &AgentId::from("agent-1"))?.expect("fixture agent");
                record.extra.insert("provider".into(), json!("claude"));
                AgentRepository::put(tx, &record)
            })
            .await
            .unwrap();
        let guest = Arc::new(SupervisorRulesGuest::default());
        let factory = GuestProviderTaskFactory::new(
            store.clone(),
            guest.clone(),
            GuestProviderIdentity {
                agent_id: "agent-1".into(),
                epoch: 9,
                generation: 3,
                account_key: "default".into(),
                provider: "claude".into(),
            },
            "profiles/default",
        );
        let open = |shutdown: Shutdown| {
            factory.open(
                ProviderHandle::for_account("agent-1"),
                OpenOptions {
                    clock: SessionClock::default(),
                    launch: launch(),
                    expected_generation: Some(Generation(3)),
                },
                shutdown,
            )
        };
        let local_id = RequestId::String(format!("claude:{:032x}", 1));
        let initialize = |operation: &str| {
            let frame = Frame::Request(RequestFrame {
                id: local_id.clone(),
                method: Method::Initialize,
                params: Some(json!({})),
                fields: Default::default(),
            });
            PreparedWrite::new(OperationId(operation.into()), &frame).unwrap()
        };

        let mut first = open(Shutdown::default()).await.unwrap();
        let (command, receipt) = AdapterCommand::write(initialize("initialize-1"));
        first.commands.send(command).await.unwrap();
        assert!(matches!(
            tokio::time::timeout(Duration::from_secs(3), receipt.receive())
                .await
                .unwrap(),
            CommandReceipt::Written { .. }
        ));
        let entry = tokio::time::timeout(Duration::from_secs(3), first.journal.recv())
            .await
            .unwrap()
            .unwrap();
        // The reply reaches the session under the local string ID.
        assert!(
            matches!(&entry.payload, JournalPayload::Frame(Frame::Response { id, .. }) if *id == local_id),
            "the initialize reply keeps the local request ID"
        );
        assert_eq!(
            *guest.init_result.lock().unwrap(),
            Some(json!({"userAgent":"claude-bridge/1"})),
            "the guest supervisor saves the Claude initialize result with the handle"
        );
        // A provider server request has its own ID space. An ID equal to a
        // remote request ID is not a reply and keeps its value.
        guest
            .events
            .lock()
            .unwrap()
            .push(json!({"sequence":2,"generation":3,"kind":"stdout",
            "payload":json!({"id":1,"method":"item/tool/call","params":{}}).to_string()}));
        let entry = tokio::time::timeout(Duration::from_secs(3), first.journal.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            matches!(&entry.payload, JournalPayload::Frame(Frame::Request(request))
                if request.id == RequestId::Number(1)),
            "a server request keeps the provider's request ID"
        );
        let _exit = first.shutdown().await;

        let mut second = open(Shutdown::default()).await.unwrap();
        let (command, receipt) = AdapterCommand::write(initialize("initialize-2"));
        second.commands.send(command).await.unwrap();
        let cached = tokio::time::timeout(Duration::from_secs(3), receipt.receive())
            .await
            .unwrap();
        assert!(
            matches!(&cached, CommandReceipt::CachedInitialize(result)
                if *result == json!({"userAgent":"claude-bridge/1"})),
            "a reattach reuses the saved initialize result"
        );
        assert_eq!(
            guest.writes.lock().unwrap().len(),
            1,
            "initialize is never written again to the live bridge"
        );
        let _exit = second.shutdown().await;
    }

    // The session task stops serving at shutdown and reads a pending receipt
    // only after this attachment completes. A wait for that observation after
    // shutdown hung the account worker while it joined the VM event forwarder.
    #[tokio::test]
    async fn shutdown_during_guest_write_does_not_wait_for_receipt_observation() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        let (release, held) = std::sync::mpsc::channel();
        *guest.held_write.lock().unwrap() = Some(held);
        let shutdown = Shutdown::default();
        let mut attachment = open_attachment(store, guest.clone(), shutdown.clone()).await;
        let turn = Frame::Request(RequestFrame {
            id: RequestId::Number(2),
            method: Method::ThreadStart,
            params: Some(json!({})),
            fields: Default::default(),
        });
        let write = PreparedWrite::new(OperationId("thread-start-op".into()), &turn).unwrap();
        let (command, receipt) = AdapterCommand::write(write);
        attachment.commands.send(command).await.unwrap();
        tokio::time::timeout(Duration::from_secs(3), guest.write_entered.notified())
            .await
            .expect("guest write starts");
        shutdown.cancel();
        release.send(()).unwrap();

        let exit = tokio::time::timeout(Duration::from_secs(3), attachment.shutdown())
            .await
            .expect("attachment shutdown does not wait for the consumer to observe a receipt");
        assert!(exit.result.is_ok());
        assert!(matches!(
            receipt.receive().await,
            CommandReceipt::Written { .. }
        ));
        assert_eq!(*guest.writes.lock().unwrap(), 1);
    }

    // The session task stops reading the journal before it joins this
    // attachment, so a full journal lane must not hold shutdown.
    #[tokio::test]
    async fn shutdown_with_full_journal_lane_does_not_wait_for_the_consumer() {
        let (_directory, store) = open_store().await;
        let guest = Arc::new(FakeGuest::new(false));
        *guest.endless_events.lock().unwrap() = true;
        let mut attachment = open_attachment(store, guest.clone(), Shutdown::default()).await;
        // The reader asks for an event only after the attachment took the
        // previous one. Read LANE_CAPACITY + 3 thus proves that the lane is
        // full and the attachment waits to send entry LANE_CAPACITY + 1.
        let blocked_reads = LANE_CAPACITY + 3;
        tokio::time::pause();
        tokio::time::timeout(Duration::from_secs(10), async {
            while *guest.next_calls.lock().unwrap() < blocked_reads {
                guest.next_changed.notified().await;
            }
        })
        .await
        .expect("the attachment fills its journal lane");
        tokio::time::resume();

        let exit = tokio::time::timeout(Duration::from_secs(3), attachment.shutdown())
            .await
            .expect("attachment shutdown does not wait for journal capacity");
        assert!(exit.result.is_ok());
        assert_eq!(*guest.next_calls.lock().unwrap(), blocked_reads);
        let mut delivered = 0;
        while attachment.journal.try_recv().is_ok() {
            delivered += 1;
        }
        assert_eq!(delivered, LANE_CAPACITY);
        assert!(guest.acks.lock().unwrap().is_empty());
    }
}
