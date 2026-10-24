//! Retained Linux-VM provider sessions over the existing guest RPC channel.
//!
//! The guest's native supervisor owns provider process identity and its event
//! journal. This host adapter owns only the exact start identity and settlement
//! cursor; it does not create another socket or receipt protocol.

/// The guest home of a VM agent whose record has no `layrHome`.
pub(crate) const DEFAULT_GUEST_HOME: &str = "/home/studio";

use crate::linux_vm_guest::{GUEST_RPC_TIMEOUT_SECONDS, GuestError, GuestRpc};
use base64::Engine as _;
use rusqlite::Transaction;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
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
use studio_runtime_schema::shared_values::{
    GUEST_PROVIDER_READ_RETRY_SECONDS, LINUX_VM_PROVIDER_HANDLE_PREFIX,
};
use studio_runtime_store::{
    AgentId, AgentRepository, EventWriteRepository, RuntimeStore, StoreError,
};
use thiserror::Error;

const START_RECEIPT_PREFIX: &str = "linux-vm-provider-start:";
const START_REQUEST_ID_MAX_BYTES: usize = 128;
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
    #[error("invalid Linux VM provider identity or launch options")]
    Invalid,
    #[error("Linux VM provider outcome is unknown and must be reconciled")]
    Unknown,
    #[error("Linux VM provider refused the operation")]
    Refused,
    #[error("Linux VM provider response is invalid")]
    InvalidResponse,
    #[error("Linux VM provider persistence failed")]
    Persistence(#[from] StoreError),
    #[error("Linux VM guest request failed")]
    Guest(#[from] GuestError),
    #[error("Linux VM provider JSON is invalid")]
    Json(#[from] serde_json::Error),
    #[error("Linux VM provider worker task failed")]
    Task,
}

/// Stable host-side identity persisted before the first guest dispatch.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StartIdentity {
    pub agent_id: String,
    pub epoch: i64,
    pub handle: String,
    pub request_id: String,
    pub launch_fingerprint: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "phase", rename_all = "snake_case")]
enum PersistedStart {
    Reserved {
        identity: StartIdentity,
    },
    NotSent {
        identity: StartIdentity,
    },
    Started {
        identity: StartIdentity,
        info: Value,
    },
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
    profile: String,
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
            profile: profile.into(),
        }
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
            if handle != ProviderHandle::for_account(&self.identity.agent_id)
                || options.expected_generation != Some(Generation(self.identity.generation))
                || options.launch.command.is_empty()
            {
                return Err(AdapterError::InvalidConfiguration);
            }
            let session = GuestProviderSession::open(
                self.store.clone(),
                self.guest.clone(),
                self.identity.clone(),
                self.profile.clone(),
                options.launch.clone(),
            )
            .await
            .map_err(|_| AdapterError::TaskFailed)?;
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
                    let receipt = match session.write(&write.operation_id().0,
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
                    };
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
        let (identity, dispatch) = reserve_start(
            Arc::clone(&store),
            agent_id.clone(),
            epoch,
            account_key.clone(),
            provider.clone(),
            fingerprint.clone(),
        )
        .await?;

        let started = if dispatch {
            match call_guest(
                guest.clone(),
                "provider.start",
                params,
                Some(identity.request_id.clone()),
                Duration::from_secs(GUEST_RPC_TIMEOUT_SECONDS),
            )
            .await
            {
                Ok(info) => {
                    persist_started(store.clone(), identity.clone(), info.clone()).await?;
                    info
                }
                Err(error) if error.uncertain() => return Err(GuestProviderError::Unknown),
                Err(error) => {
                    persist_not_sent(store.clone(), identity.clone()).await?;
                    return Err(error.into());
                }
            }
        } else {
            // An existing reservation means dispatch may have happened before a
            // crash or lost response. Reconcile through the read-only info lane.
            let info = call_guest(
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
            })?;
            persist_started(store.clone(), identity.clone(), info.clone()).await?;
            info
        };
        let info = parse_info(&identity.handle, started)?;
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
/// idempotent at the existing guest RPC boundary; the receipt stores only the
/// destination and content digest, never credential bytes or tokens.
pub async fn copy_guest_credentials(
    store: Arc<RuntimeStore>,
    guest: Arc<dyn GuestRpc>,
    target: GuestCredentialTarget<'_>,
    credential_document: &[u8],
) -> Result<String> {
    if target.agent_id.is_empty()
        || target.actor_epoch < 0
        || (target.provider != "codex" && target.provider != "claude")
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
    let receipt_id = request_id.clone();
    let reserve_id = receipt_id.clone();
    let reserve_signature = signature.clone();
    let reserve_agent = target.agent_id.to_owned();
    let reserve_account = target.account_key.to_owned();
    let reserve_provider = target.provider.to_owned();
    let actor_epoch = target.actor_epoch;
    let dispatch = store
        .transaction(move |tx| {
            if !vm_actor_matches(
                tx,
                &reserve_agent,
                actor_epoch,
                &reserve_account,
                Some(&reserve_provider),
            )? {
                return Ok(None);
            }
            match EventWriteRepository::receipt(tx, &reserve_id)? {
                Some(saved) if saved.signature != reserve_signature => {
                    return Err(StoreError::InvalidStoredObject {
                        table: "runtime_operation_receipts",
                        id: reserve_id,
                    });
                }
                Some(saved) if saved.result == "complete" => return Ok(Some(false)),
                Some(_) => return Ok(Some(true)),
                None => EventWriteRepository::put_receipt(
                    tx,
                    &reserve_id,
                    &reserve_signature,
                    "pending",
                )?,
            }
            Ok(Some(true))
        })
        .await?;
    let dispatch = match dispatch {
        Some(dispatch) => dispatch,
        None => return Err(GuestProviderError::Refused),
    };
    if !dispatch {
        return Ok(digest);
    }
    let dispatch_agent = target.agent_id.to_owned();
    let dispatch_account = target.account_key.to_owned();
    let dispatch_provider = target.provider.to_owned();
    let still_current = store
        .read_async("runtime-services/linux_vm_provider", move |connection| {
            vm_actor_matches(
                connection,
                &dispatch_agent,
                actor_epoch,
                &dispatch_account,
                Some(&dispatch_provider),
            )
        })
        .await?;
    if !still_current {
        return Err(GuestProviderError::Refused);
    }
    let params = json!({
        "files": [{
            "path": target.relative_path,
            "data": base64::engine::general_purpose::STANDARD.encode(credential_document),
        }]
    });
    call_guest(
        guest,
        "credentials.put",
        params,
        Some(request_id.clone()),
        Duration::from_secs(GUEST_RPC_TIMEOUT_SECONDS),
    )
    .await
    .map_err(|error| {
        if error.uncertain() {
            GuestProviderError::Unknown
        } else {
            GuestProviderError::Guest(error)
        }
    })?;
    let complete_id = receipt_id;
    let complete_signature = signature;
    store.transaction(move |tx| {
            let Some(saved) = EventWriteRepository::receipt(tx, &complete_id)? else {
                return Err(StoreError::InvalidStoredObject {
                    table: "runtime_operation_receipts",
                    id: complete_id,
                });
            };
            if saved.signature != complete_signature {
                return Err(StoreError::InvalidStoredObject {
                    table: "runtime_operation_receipts",
                    id: complete_id,
                });
            }
            tx.execute(
                "UPDATE runtime_operation_receipts SET result='complete' WHERE id=?1 AND signature=?2",
                rusqlite::params![complete_id, complete_signature],
            )?;
            Ok(())
        }).await?;
    Ok(digest)
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    const HEX: &[u8; 16] = b"0123456789abcdef";
    for byte in digest {
        output.push(HEX[(byte >> 4) as usize] as char);
        output.push(HEX[(byte & 0x0f) as usize] as char);
    }
    output
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
    let encoded = serde_json::to_vec(params)?;
    Ok(data_encoding::HEXLOWER.encode(&Sha256::digest(encoded)))
}

async fn reserve_start(
    store: Arc<RuntimeStore>,
    agent_id: String,
    epoch: i64,
    account_key: String,
    provider: String,
    fingerprint: String,
) -> Result<(StartIdentity, bool)> {
    if epoch < 0 || agent_id.is_empty() || fingerprint.len() != 64 {
        return Err(GuestProviderError::Invalid);
    }
    let receipt_id = format!("{START_RECEIPT_PREFIX}{agent_id}:{epoch}");
    let signature = format!("agent={agent_id};epoch={epoch};launch={fingerprint}");
    store
        .transaction_with(move |tx| -> Result<(StartIdentity, bool)> {
            if !vm_actor_matches(tx, &agent_id, epoch, &account_key, Some(&provider))? {
                return Err(GuestProviderError::Refused);
            }
            if let Some(saved) = EventWriteRepository::receipt(tx, &receipt_id)? {
                if saved.signature != signature {
                    return Err(StoreError::InvalidStoredObject {
                        table: "runtime_operation_receipts",
                        id: receipt_id.clone(),
                    }
                    .into());
                }
                let saved: PersistedStart = serde_json::from_str(&saved.result)?;
                let (identity, dispatch) = match saved {
                    PersistedStart::Reserved { identity } => (identity, false),
                    PersistedStart::NotSent { identity } => (identity, true),
                    PersistedStart::Started { identity, .. } => (identity, false),
                };
                return Ok((identity, dispatch));
            }
            let request_id = uuid::Uuid::new_v4().to_string();
            if request_id.len() > START_REQUEST_ID_MAX_BYTES {
                return Err(GuestProviderError::Invalid);
            }
            let identity = StartIdentity {
                agent_id: agent_id.to_owned(),
                epoch,
                handle: format!("{LINUX_VM_PROVIDER_HANDLE_PREFIX}{agent_id}"),
                request_id,
                launch_fingerprint: fingerprint.to_owned(),
            };
            EventWriteRepository::put_receipt(
                tx,
                &receipt_id,
                &signature,
                &serde_json::to_string(&PersistedStart::Reserved {
                    identity: identity.clone(),
                })?,
            )?;
            Ok((identity, true))
        })
        .await
}

async fn persist_not_sent(store: Arc<RuntimeStore>, identity: StartIdentity) -> Result<()> {
    let receipt_id = format!(
        "{START_RECEIPT_PREFIX}{}:{}",
        identity.agent_id, identity.epoch
    );
    store
        .transaction_with(move |tx: &Transaction<'_>| -> Result<()> {
            let Some(saved) = EventWriteRepository::receipt(tx, &receipt_id)? else {
                return Err(GuestProviderError::Unknown);
            };
            let current: PersistedStart = serde_json::from_str(&saved.result)?;
            let current_identity = match current {
                PersistedStart::Reserved { identity }
                | PersistedStart::NotSent { identity }
                | PersistedStart::Started { identity, .. } => identity,
            };
            if current_identity != identity {
                return Err(GuestProviderError::Unknown);
            }
            let result = serde_json::to_string(&PersistedStart::NotSent { identity })?;
            tx.execute(
                "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2 AND signature=?3",
                rusqlite::params![result, receipt_id, saved.signature],
            )
            .map_err(StoreError::from)?;
            Ok(())
        })
        .await
}

async fn persist_started(
    store: Arc<RuntimeStore>,
    identity: StartIdentity,
    info: Value,
) -> Result<()> {
    let receipt_id = format!(
        "{START_RECEIPT_PREFIX}{}:{}",
        identity.agent_id, identity.epoch
    );
    store
        .transaction_with(move |tx: &Transaction<'_>| -> Result<()> {
            let Some(saved) = EventWriteRepository::receipt(tx, &receipt_id)? else {
                return Err(GuestProviderError::Unknown);
            };
            let current: PersistedStart = serde_json::from_str(&saved.result)?;
            let current_identity = match current {
                PersistedStart::Reserved { identity }
                | PersistedStart::NotSent { identity }
                | PersistedStart::Started { identity, .. } => identity,
            };
            if current_identity != identity {
                return Err(GuestProviderError::Unknown);
            }
            let result = serde_json::to_string(&PersistedStart::Started { identity, info })?;
            tx.execute(
                "UPDATE runtime_operation_receipts SET result=?1 WHERE id=?2 AND signature=?3",
                rusqlite::params![result, receipt_id, saved.signature],
            )
            .map_err(StoreError::from)?;
            Ok(())
        })
        .await
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

fn parse_info(handle: &str, value: Value) -> Result<ProviderInfo> {
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

    struct FakeGuest {
        starts: Mutex<usize>,
        writes: Mutex<usize>,
        acks: Mutex<Vec<u64>>,
        stops: Mutex<Vec<Value>>,
        credential_puts: Mutex<Vec<(Option<String>, Value)>>,
        uncertain_start_once: Mutex<bool>,
        definite_start_failure_once: Mutex<bool>,
        malformed_write_once: Mutex<bool>,
        committed_internal_write_once: Mutex<bool>,
        start_ids: Mutex<Vec<Option<String>>>,
        held_write: Mutex<Option<std::sync::mpsc::Receiver<()>>>,
        write_entered: tokio::sync::Notify,
        endless_events: Mutex<bool>,
        next_calls: Mutex<usize>,
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
                uncertain_start_once: Mutex::new(uncertain_start_once),
                definite_start_failure_once: Mutex::new(false),
                malformed_write_once: Mutex::new(false),
                committed_internal_write_once: Mutex::new(false),
                start_ids: Mutex::new(Vec::new()),
                held_write: Mutex::new(None),
                write_entered: tokio::sync::Notify::new(),
                endless_events: Mutex::new(false),
                next_calls: Mutex::new(0),
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
        let event = session.next().await.unwrap().unwrap();
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
        let receipt_id = guest.credential_puts.lock().unwrap()[0].0.clone().unwrap();
        let receipt = EventWriteRepository::receipt(&store.test_reader().unwrap(), &receipt_id)
            .unwrap()
            .unwrap();
        assert_eq!(receipt.result, "complete");
        assert!(!receipt.result.contains("private-token"));
        assert!(!receipt.signature.contains("private-token"));
        let expected_material = serde_json::to_vec(&json!({
            "agentId": "agent-1",
            "accountKey": "account-1",
            "path": ".claude/studio-accounts/key/.credentials.json",
            "digest": digest,
        }))
        .unwrap();
        assert_eq!(receipt.signature, hex_digest(&expected_material));
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
        tokio::time::timeout(Duration::from_secs(10), async {
            while *guest.next_calls.lock().unwrap() < blocked_reads {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the attachment fills its journal lane");

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
