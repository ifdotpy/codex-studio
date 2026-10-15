//! Retained Linux-VM provider sessions over the existing guest RPC channel.
//!
//! The guest's native supervisor owns provider process identity and its event
//! journal. This host adapter owns only the exact start identity and settlement
//! cursor; it does not create another socket or receipt protocol.

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
        ProviderAttachment, ProviderTaskFactory, Shutdown, TaskExit,
    },
};
use studio_runtime_providers::{DecodedJournalPayload, decode_journal_value};
use studio_runtime_store::{EventWriteRepository, RuntimeStore, StoreError};
use thiserror::Error;

const START_RECEIPT_PREFIX: &str = "linux-vm-provider-start:";
const START_REQUEST_ID_MAX_BYTES: usize = 128;
const PROVIDER_HANDLE_PREFIX: &str = "linux-worker:";
const PROVIDER_RPC_TIMEOUT: Duration = Duration::from_secs(15);
const EVENT_PAYLOAD_MAX_BYTES: u64 = 256 * 1024 * 1024;
const EVENT_CHUNK_MAX_BYTES: u64 = 1024 * 1024;
const CREDENTIAL_RECEIPT_PREFIX: &str = "linux-vm-credential-copy:";
pub const MAX_GUEST_CREDENTIAL_BYTES: usize = 1024 * 1024;

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
    agent_id: String,
    epoch: i64,
    provider: String,
    profile: String,
}

impl GuestProviderTaskFactory {
    pub fn new(
        store: Arc<RuntimeStore>,
        guest: Arc<dyn GuestRpc>,
        agent_id: impl Into<String>,
        epoch: i64,
        provider: impl Into<String>,
        profile: impl Into<String>,
    ) -> Self {
        Self {
            store,
            guest,
            agent_id: agent_id.into(),
            epoch,
            provider: provider.into(),
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
            if handle != ProviderHandle::for_account(&self.agent_id)
                || options.launch.command.is_empty()
                || options.launch.retained_response_ids.len()
                    > studio_runtime_process::provider_seam::MAX_PENDING_RESPONSES
            {
                return Err(AdapterError::InvalidConfiguration);
            }
            let session = GuestProviderSession::open(
                self.store.clone(),
                self.guest.clone(),
                self.agent_id.clone(),
                self.epoch,
                self.provider.clone(),
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
            let handle = ProviderHandle::for_account(&self.agent_id);
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
                options.clock,
                options.launch.retained_response_ids,
                generation,
            ));
            Ok(ProviderAttachment::from_task(
                commands, journal, status, shutdown, task,
            ))
        })
    }
}

async fn run_guest_attachment(
    session: GuestProviderSession,
    mut commands: tokio::sync::mpsc::Receiver<AdapterCommand>,
    journal: tokio::sync::mpsc::Sender<JournalEntry>,
    status: tokio::sync::watch::Sender<AttachmentStatus>,
    mut stop: tokio::sync::watch::Receiver<bool>,
    clock: studio_runtime_process::provider_seam::SessionClock,
    mut response_ids: BTreeMap<u64, u64>,
    session_generation: Generation,
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
            tokio::select! {
                biased;
                _ = Shutdown::cancelled(&mut reader_stop) => break,
                sent = read_tx.send(next) => if sent.is_err() { break; },
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
                    if method.as_deref() == Some("initialize") {
                        if let Some(cached) = cached_initialize.take() {
                            initialize_id = message.get("id").cloned();
                            let _ = reply.send(CommandReceipt::CachedInitialize(cached));
                            if observed.await.is_err() {
                                result = Err(AdapterError::ConsumerClosed);
                                break;
                            }
                            continue;
                        }
                    }
                    let native_id = message.get("id").cloned().or_else(|| {
                        (method.as_deref() == Some("initialize")).then_some(Value::from(1))
                    });
                    let receipt = match session.write(&write.operation_id().0,
                        native_id.as_ref().unwrap_or(&Value::Null), message).await {
                        Ok(value) if value.get("accepted") == Some(&Value::Bool(true)) => {
                            let remote_id = value.get("remoteId").and_then(Value::as_u64);
                            if let (Some(remote), Some(local)) = (remote_id, native_id.as_ref().and_then(Value::as_u64)) {
                                response_ids.insert(remote, local);
                            }
                            if method.as_deref() == Some("initialize") { initialize_id = native_id; }
                            if method.as_deref() == Some("initialized") {
                                initialization = InitializationStatus::Ready;
                                let mut published = status.borrow().clone();
                                published.initialization = initialization;
                                status.send_replace(published);
                            }
                            CommandReceipt::Written { remote_id, duplicate: false }
                        }
                        Ok(_) => CommandReceipt::NotSent,
                        Err(GuestProviderError::Unknown) => CommandReceipt::Unknown,
                        Err(_) => CommandReceipt::NotSent,
                    };
                    let observe = matches!(receipt, CommandReceipt::Written { .. } | CommandReceipt::CachedInitialize(_));
                    let _ = reply.send(receipt);
                    if observe && observed.await.is_err() {
                        result = Err(AdapterError::ConsumerClosed);
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
                        if let Some(remote) = event.payload.get("id").and_then(Value::as_u64)
                            && let Some(local) = response_ids.get(&remote) {
                                if let Some(object) = event.payload.as_object_mut() {
                                    object.insert("id".into(), Value::from(*local));
                                }
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
                        "stdout" => match decode_journal_value(event.sequence, event.generation, event.payload) {
                            decoded => match decoded.payload {
                                DecodedJournalPayload::Frame(frame) => JournalPayload::Frame(frame),
                                DecodedJournalPayload::RawStdout { bytes, error } => JournalPayload::RawStdout { bytes, error },
                            }
                        },
                        "stderr" => JournalPayload::Stderr(event.payload),
                        "exit" => JournalPayload::Exit(event.payload),
                        kind => JournalPayload::Other { kind: kind.to_owned(), payload: event.payload },
                    };
                    if journal.send(JournalEntry {
                        received_at_ms: clock.now_ms(),
                        cursor: EventCursor(event.sequence),
                        generation: session_generation,
                        payload,
                    }).await.is_err() {
                        result = Err(AdapterError::ConsumerClosed);
                        break;
                    }
                }
                Ok(None) => tokio::time::sleep(Duration::from_millis(50)).await,
                Err(_) => { result = Err(AdapterError::TaskFailed); break; }
            }
        }
    }
    // The read lane is independent of command cancellation. Stop and join it
    // before returning so a failed attachment cannot leave a concurrent guest
    // read racing its replacement. Cancellation detaches; explicit actor
    // disposal owns provider.stop.
    reader.abort();
    let _reader_exit = reader.await;
    TaskExit {
        result,
        cleanup_error: None,
        recovery_needed: None,
    }
}

impl GuestProviderSession {
    /// Reserve the start identity in the host store before dispatching to guest.
    /// A persisted pending identity is reconciled by `info`; start is never
    /// resent when a previous call may have reached the guest.
    pub async fn open(
        store: Arc<RuntimeStore>,
        guest: Arc<dyn GuestRpc>,
        agent_id: impl Into<String>,
        epoch: i64,
        provider: impl Into<String>,
        profile: impl Into<String>,
        launch: ProviderLaunchSpec,
    ) -> Result<Self> {
        let agent_id = agent_id.into();
        let provider = provider.into();
        let profile = profile.into();
        let params = start_params(&agent_id, &provider, &profile, &launch)?;
        let fingerprint = fingerprint(&params)?;
        let (identity, dispatch) = tokio::task::spawn_blocking({
            let store = store.clone();
            let agent_id = agent_id.clone();
            let fingerprint = fingerprint.clone();
            move || reserve_start(&store, &agent_id, epoch, &fingerprint)
        })
        .await
        .map_err(|_| GuestProviderError::Task)??;

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
            Err(error) if error.uncertain() => {
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
            Err(error) => Err(error.into()),
        }
    }

    /// Read the next journal entry on the independent delivery lane. Advancing
    /// this cursor never acknowledges the guest event; only settle_then_ack does.
    pub async fn next(&self) -> Result<Option<ProviderEvent>> {
        let cursor = self.read_cursor.load(Ordering::Acquire);
        let result = call_guest(
            self.guest.clone(),
            "provider.rpc",
            json!({"handle":self.identity.handle,"action":"next","cursor":cursor}),
            None,
            PROVIDER_RPC_TIMEOUT,
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
        let result = call_guest(
            self.guest.clone(),
            "provider.rpc",
            json!({"handle":self.identity.handle,"action":"replay","cursor":cursor,"limit":limit}),
            None,
            PROVIDER_RPC_TIMEOUT,
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
            let chunk = call_guest(
                self.guest.clone(),
                "provider.rpc",
                json!({"handle":self.identity.handle,"action":"eventRead","sequence":sequence,"offset":offset,"maxBytes":max_bytes}),
                None,
                PROVIDER_RPC_TIMEOUT,
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
        call_guest(
            self.guest.clone(),
            "provider.rpc",
            json!({"handle":self.identity.handle,"action":"ack","sequence":event.sequence}),
            None,
            PROVIDER_RPC_TIMEOUT,
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
    agent_id: &str,
    account_key: &str,
    relative_path: &str,
    credential_document: &[u8],
) -> Result<String> {
    if agent_id.is_empty()
        || account_key.is_empty()
        || relative_path.is_empty()
        || relative_path.starts_with('/')
        || relative_path.contains(['\\', '\0'])
        || relative_path
            .split('/')
            .any(|part| part == ".." || part == "." || part.is_empty())
        || credential_document.len() > MAX_GUEST_CREDENTIAL_BYTES
    {
        return Err(GuestProviderError::Invalid);
    }
    let digest = hex_digest(credential_document);
    let request_material = serde_json::to_vec(&json!({
        "agentId": agent_id,
        "accountKey": account_key,
        "path": relative_path,
        "digest": digest,
    }))?;
    let signature = hex_digest(&request_material);
    let request_id = format!("{CREDENTIAL_RECEIPT_PREFIX}{}", &signature[..48]);
    let receipt_id = request_id.clone();
    let reserve_id = receipt_id.clone();
    let reserve_signature = signature.clone();
    let reserve_store = Arc::clone(&store);
    let dispatch = tokio::task::spawn_blocking(move || {
        reserve_store.transaction(|tx| {
            match EventWriteRepository::receipt(tx, &reserve_id)? {
                Some(saved) if saved.signature != reserve_signature => {
                    return Err(StoreError::InvalidStoredObject {
                        table: "runtime_operation_receipts",
                        id: reserve_id,
                    });
                }
                Some(saved) if saved.result == "complete" => return Ok(false),
                Some(_) => return Ok(true),
                None => EventWriteRepository::put_receipt(
                    tx,
                    &reserve_id,
                    &reserve_signature,
                    "pending",
                )?,
            }
            Ok(true)
        })
    })
    .await
    .map_err(|_| GuestProviderError::Task)??;
    if !dispatch {
        return Ok(digest);
    }
    let params = json!({
        "files": [{
            "path": relative_path,
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
    tokio::task::spawn_blocking(move || {
        store.transaction(|tx| {
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
        })
    })
    .await
    .map_err(|_| GuestProviderError::Task)??;
    Ok(digest)
}

fn hex_digest(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut output = String::with_capacity(digest.len() * 2);
    for byte in digest {
        use std::fmt::Write as _;
        let _ = write!(&mut output, "{byte:02x}");
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
        "handle":format!("{PROVIDER_HANDLE_PREFIX}{agent_id}"),
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

fn reserve_start(
    store: &RuntimeStore,
    agent_id: &str,
    epoch: i64,
    fingerprint: &str,
) -> Result<(StartIdentity, bool)> {
    if epoch < 0 || agent_id.is_empty() || fingerprint.len() != 64 {
        return Err(GuestProviderError::Invalid);
    }
    let receipt_id = format!("{START_RECEIPT_PREFIX}{agent_id}:{epoch}");
    let signature = format!("agent={agent_id};epoch={epoch};launch={fingerprint}");
    store.transaction_with(|tx| -> Result<(StartIdentity, bool)> {
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
            handle: format!("{PROVIDER_HANDLE_PREFIX}{agent_id}"),
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
}

async fn persist_not_sent(store: Arc<RuntimeStore>, identity: StartIdentity) -> Result<()> {
    let receipt_id = format!(
        "{START_RECEIPT_PREFIX}{}:{}",
        identity.agent_id, identity.epoch
    );
    tokio::task::spawn_blocking(move || {
        store.transaction_with(|tx: &Transaction<'_>| -> Result<()> {
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
    })
    .await
    .map_err(|_| GuestProviderError::Task)?
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
    tokio::task::spawn_blocking(move || {
        store.transaction_with(|tx: &Transaction<'_>| -> Result<()> {
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
    })
    .await
    .map_err(|_| GuestProviderError::Task)?
}

async fn call_guest(
    guest: Arc<dyn GuestRpc>,
    method: &'static str,
    params: Value,
    request_id: Option<String>,
    timeout: Duration,
) -> std::result::Result<Value, GuestError> {
    tokio::task::spawn_blocking(move || guest.call(method, &params, request_id.as_deref(), timeout))
        .await
        .map_err(|_| GuestError::Protocol {
            message: "guest provider worker task failed",
            uncertain: true,
        })?
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
        start_ids: Mutex<Vec<Option<String>>>,
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
                start_ids: Mutex::new(Vec::new()),
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
                    "next" if params["cursor"] == 0 => {
                        let payload = json!({"id":1,"method":"initialize","params":{}});
                        Ok(json!({"event":{"sequence":1,"generation":3,"kind":"stdout",
                            "payload":payload.to_string()}}))
                    }
                    "next" => Ok(json!({"event":null})),
                    "write" => {
                        *self.writes.lock().unwrap() += 1;
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

    fn open_store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).unwrap());
        (directory, store)
    }

    async fn open_session(
        store: Arc<RuntimeStore>,
        guest: Arc<dyn GuestRpc>,
    ) -> Result<GuestProviderSession> {
        GuestProviderSession::open(
            store,
            guest,
            "agent-1",
            9,
            "codex",
            "profiles/default",
            launch(),
        )
        .await
    }

    #[tokio::test]
    async fn lifecycle_writes_and_acknowledges_only_after_durable_settlement() {
        let (_directory, store) = open_store();
        let guest = Arc::new(FakeGuest::new(false));
        let mut session = open_session(store, guest.clone()).await.unwrap();
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
    async fn uncertain_start_is_reconciled_without_resending_after_restart() {
        let (_directory, store) = open_store();
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
        let (_directory, store) = open_store();
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
    async fn failed_settlement_does_not_ack_guest_event() {
        let (_directory, store) = open_store();
        let guest = Arc::new(FakeGuest::new(false));
        let mut session = open_session(store, guest.clone()).await.unwrap();
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
        let (_directory, store) = open_store();
        let guest = Arc::new(FakeGuest::new(false));
        let secret = b"{\"accessToken\":\"private-token\"}";
        let digest = copy_guest_credentials(
            store.clone(),
            guest.clone(),
            "agent-1",
            "account-1",
            ".claude/studio-accounts/key/.credentials.json",
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
        let receipt = EventWriteRepository::receipt(&store.read().unwrap(), &receipt_id)
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
        let (_directory, store) = open_store();
        let guest = Arc::new(FakeGuest::new(false));
        let factory = GuestProviderTaskFactory::new(
            store,
            guest.clone(),
            "agent-1",
            9,
            "codex",
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
}
