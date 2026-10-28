//! Voice-session RPC port and durable request adapter.
//!
//! Registered differences from Python:
//! - `UnknownVoiceWriteHeld`: an unknown provider write is never resent after
//!   a retry or restart.
//! - `VoiceApprovalContextRechecked`: approval request identity, epoch,
//!   playback, and transcript are revalidated immediately before dispatch.
//! - New Rust speech receipts bind the transcript digest to the Python-compatible
//!   `<session>:<record>` identity and refuse changed text under that identity.

use std::collections::HashMap;
use std::{
    sync::{Mutex, OnceLock, Weak},
    time::{Duration, Instant},
};

use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_providers::Method;
use studio_runtime_schema::persistence::{OutboxKind, RequestIdentity};
#[cfg(test)]
use studio_runtime_schema::persistence::{OutboxStatus, OwnerKind, VoiceStopOutcome};
#[cfg(test)]
use studio_runtime_store::outbox::Outbox;
use studio_runtime_store::{
    AgentId, AgentRepository, Connection, RuntimeStore, VoiceRepository,
    request_results::{Admission, AdmittedRequest, RequestResults},
};
use thiserror::Error;

use super::policy::{
    self, SessionSignal, StartDecision, StartRequest, VoiceSession, VoiceSessionRow,
};
use super::runtime::{
    VoiceConnectionScope as VoiceProviderContext, VoicePrepareFailure, VoiceReply,
    VoiceRequestPort as VoiceProviderPort, VoiceRequestReceipt, VoiceRpcId, VoiceWriteCertainty,
};

const MAX_TRACKED_VOICE_SESSIONS: usize = 1024;
const VOICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const RESTARTED_SESSION_ERROR: &str = "Voice connection ended. The transcript is saved.";

type SessionDispatchLocks = HashMap<(String, String), Weak<tokio::sync::Mutex<()>>>;
static DISPATCH_LOCKS: OnceLock<Mutex<SessionDispatchLocks>> = OnceLock::new();
type SessionStartCompletions = HashMap<(String, String), Weak<tokio::sync::watch::Sender<bool>>>;
static START_COMPLETIONS: OnceLock<Mutex<SessionStartCompletions>> = OnceLock::new();

/// A restarted process cannot restore a browser's peer connection or microphone.
/// Settle accepted sessions before HTTP admission; retain their receipts so an
/// exact retry returns the terminal result without sending another provider RPC.
pub async fn recover_after_restart(
    store: &RuntimeStore,
    now: f64,
) -> Result<usize, VoiceProviderError> {
    let changed = store
        .transaction(move |transaction| {
            Ok(transaction.execute(
                "UPDATE voice_sessions SET state='lost',ended=?1,error=?2 \
             WHERE state IS NOT NULL AND ended IS NULL",
                rusqlite::params![now, RESTARTED_SESSION_ERROR],
            )?)
        })
        .await?;
    Ok(changed)
}

async fn session_dispatch_guard(
    agent_id: &str,
    session_id: &str,
) -> tokio::sync::OwnedMutexGuard<()> {
    let table = DISPATCH_LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let lock = {
        let mut locks = table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        locks.retain(|_, lock| lock.strong_count() > 0);
        let key = (agent_id.to_owned(), session_id.to_owned());
        locks.get(&key).and_then(Weak::upgrade).unwrap_or_else(|| {
            let lock = std::sync::Arc::new(tokio::sync::Mutex::new(()));
            locks.insert(key, std::sync::Arc::downgrade(&lock));
            lock
        })
    };
    lock.lock_owned().await
}

fn register_start_completion(
    agent_id: &str,
    session_id: &str,
) -> std::sync::Arc<tokio::sync::watch::Sender<bool>> {
    let table = START_COMPLETIONS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut completions = table
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    completions.retain(|_, completion| completion.strong_count() > 0);
    let (completion, _) = tokio::sync::watch::channel(false);
    let completion = std::sync::Arc::new(completion);
    completions.insert(
        (agent_id.to_owned(), session_id.to_owned()),
        std::sync::Arc::downgrade(&completion),
    );
    completion
}

fn pending_start_completion(
    agent_id: &str,
    session_id: &str,
) -> Option<std::sync::Arc<tokio::sync::watch::Sender<bool>>> {
    START_COMPLETIONS.get().and_then(|table| {
        table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(&(agent_id.to_owned(), session_id.to_owned()))
            .and_then(Weak::upgrade)
    })
}

fn finish_start_completion(
    agent_id: &str,
    session_id: &str,
    completion: &std::sync::Arc<tokio::sync::watch::Sender<bool>>,
) {
    completion.send_replace(true);
    if let Some(table) = START_COMPLETIONS.get() {
        let mut completions = table
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let key = (agent_id.to_owned(), session_id.to_owned());
        if completions
            .get(&key)
            .and_then(Weak::upgrade)
            .is_some_and(|current| std::sync::Arc::ptr_eq(&current, completion))
        {
            completions.remove(&key);
        }
    }
}

/// Wait for an admitted start's provider setup and start request to settle.
/// End uses this barrier to preserve start-before-stop ordering without holding
/// the per-session dispatch mutex while provider work is in flight.
pub async fn wait_for_start_setup(agent_id: &str, session_id: &str) {
    let Some(completion) = pending_start_completion(agent_id, session_id) else {
        return;
    };
    let mut finished = completion.subscribe();
    if !*finished.borrow() {
        // Holding the sender's Arc guarantees this wait ends with a change.
        finished
            .changed()
            .await
            .expect("registered start completion remains open");
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceRpcCall {
    pub operation_id: String,
    pub root_request_id: String,
    pub admitted_request: Option<AdmittedRequest>,
    pub context: VoiceProviderContext,
    pub method: Method,
    pub params: Value,
    pub deadline: Instant,
}

pub use super::effects::{
    VoiceDeleteIntent, VoiceDeleteSession, capture_delete_sessions, deleted_voice_sessions,
    end_deleted_sessions_tracked, pending_delete_intents, settle_voice_stop, stage_delete_sessions,
};
pub(crate) use super::effects::{
    end_voice_sessions_tracked, existing_stop, pending_voice_intent, pending_voice_intents,
    stage_ordinary_stop, stage_voice_stop_sessions,
};

pub(super) async fn request(
    provider: &dyn VoiceProviderPort,
    call: VoiceRpcCall,
) -> VoiceRequestReceipt {
    provider
        .request(
            call.context,
            super::runtime::VoiceOperation {
                id: call.operation_id,
                root_request_id: call.root_request_id,
                admitted_request: call.admitted_request,
            },
            call.method,
            call.params,
            call.deadline,
        )
        .await
}

pub(super) fn request_deadline() -> Instant {
    Instant::now() + VOICE_REQUEST_TIMEOUT
}

pub(super) fn saved_scope(
    connection: &Connection,
    session: &str,
) -> Result<studio_runtime_schema::persistence::CapturedScope, VoiceProviderError> {
    let raw: Option<String> = connection.query_row(
        "SELECT provider_scope FROM voice_sessions WHERE id=?1",
        [session],
        |row| row.get(0),
    )?;
    match raw {
        Some(raw) => Ok(studio_runtime_schema::persistence::CapturedScope::Known(
            serde_json::from_str(&raw).map_err(|_| VoiceProviderError::ContextChanged)?,
        )),
        None => Ok(studio_runtime_schema::persistence::CapturedScope::Unproven),
    }
}

fn required_scope(
    connection: &Connection,
    session: &str,
) -> Result<studio_runtime_schema::persistence::ProviderScope, VoiceProviderError> {
    match saved_scope(connection, session)? {
        studio_runtime_schema::persistence::CapturedScope::Known(scope) => Ok(scope),
        studio_runtime_schema::persistence::CapturedScope::Unproven => {
            Err(VoiceProviderError::ContextUnavailable)
        }
    }
}

fn certainty_error(certainty: VoiceWriteCertainty) -> VoiceProviderError {
    match certainty {
        VoiceWriteCertainty::Expired => VoiceProviderError::Expired,
        VoiceWriteCertainty::NotSent => VoiceProviderError::NotSent,
        VoiceWriteCertainty::Unknown => VoiceProviderError::Unknown,
        VoiceWriteCertainty::Rejected => VoiceProviderError::Rejected,
        VoiceWriteCertainty::Written => VoiceProviderError::Operation,
    }
}

#[derive(Default)]
struct TranscriptState {
    canonical: bool,
    tail: HashMap<String, String>,
}

/// Per-account-worker callback actor. Keep one instance for the lifetime of
/// the generic provider session and route realtime notifications and its
/// disconnect callback through it so fallback transcript deltas are flushed.
#[derive(Default)]
pub struct VoiceCallbackActor {
    sessions: Mutex<HashMap<String, TranscriptState>>,
}

impl VoiceCallbackActor {
    pub async fn handle_notification(
        &self,
        store: &RuntimeStore,
        account_key: &str,
        connection_id: &str,
        message: &Value,
        now: f64,
    ) -> Result<bool, VoiceProviderError> {
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !method.starts_with("thread/realtime/") {
            return Ok(false);
        }
        let params = message
            .get("params")
            .filter(|value| value.is_object())
            .ok_or(VoiceProviderError::Operation)?;
        let thread_id = params
            .get("threadId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        let session_id = active_session_id(store, account_key, connection_id, thread_id).await?;
        let Some(session_id) = session_id else {
            return Ok(true);
        };
        let item = params.get("item");
        let event_session = params
            .get("realtimeSessionId")
            .and_then(Value::as_str)
            .or_else(|| {
                item.and_then(|item| item.get("realtimeSessionId"))
                    .and_then(Value::as_str)
            });
        if event_session.is_some_and(|id| id != session_id) {
            return Ok(true);
        }

        match method {
            "thread/realtime/item/started"
                if item
                    .and_then(|item| item.get("type"))
                    .and_then(Value::as_str)
                    == Some("transcriptSegment") =>
            {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| VoiceProviderError::Operation)?;
                if !sessions.contains_key(&session_id)
                    && sessions.len() >= MAX_TRACKED_VOICE_SESSIONS
                {
                    return Err(VoiceProviderError::Operation);
                }
                sessions.entry(session_id.to_owned()).or_default().canonical = true;
                return Ok(true);
            }
            "thread/realtime/transcript/delta" => {
                let role = params
                    .get("role")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let delta = params
                    .get("delta")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                if !matches!(role, "user" | "assistant") || delta.is_empty() {
                    return Ok(true);
                }
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| VoiceProviderError::Operation)?;
                if !sessions.contains_key(&session_id)
                    && sessions.len() >= MAX_TRACKED_VOICE_SESSIONS
                {
                    return Err(VoiceProviderError::Operation);
                }
                let state = sessions.entry(session_id.to_owned()).or_default();
                if state.canonical {
                    return Ok(true);
                }
                let tail = state.tail.entry(role.to_owned()).or_default();
                if tail.chars().count().saturating_add(delta.chars().count())
                    > super::MAX_RECORD_TEXT
                {
                    return Err(VoiceProviderError::Operation);
                }
                tail.push_str(delta);
                return Ok(true);
            }
            "thread/realtime/transcript/done" => {
                let mut sessions = self
                    .sessions
                    .lock()
                    .map_err(|_| VoiceProviderError::Operation)?;
                if sessions
                    .get(&session_id)
                    .is_some_and(|state| state.canonical)
                {
                    return Ok(true);
                }
                if let Some(role) = params.get("role").and_then(Value::as_str)
                    && let Some(state) = sessions.get_mut(&session_id)
                {
                    state.tail.remove(role);
                }
            }
            "thread/realtime/closed" => self.flush_session(store, &session_id).await?,
            _ => {}
        }
        handle_notification(store, account_key, connection_id, message, now).await
    }

    pub async fn handle_disconnect(
        &self,
        store: &RuntimeStore,
        account_key: &str,
        connection_id: &str,
        now: f64,
    ) -> Result<usize, VoiceProviderError> {
        let key = (account_key.to_owned(), connection_id.to_owned());
        let ids = store
            .read_async("runtime-services/voice/provider", move |connection| {
                let mut statement = connection.prepare_cached(
                    "SELECT id FROM voice_sessions WHERE account_key=?1 AND connection_id=?2 AND ended IS NULL",
                )?;
                statement
                    .query_map(rusqlite::params![key.0, key.1], |row| row.get::<_, String>(0))?
                    .collect::<Result<Vec<_>, _>>()
                    .map_err(studio_runtime_store::StoreError::from)
            })
            .await?;
        for id in ids {
            self.flush_session(store, &id).await?;
        }
        handle_disconnect(store, account_key, connection_id, now).await
    }

    async fn flush_session(
        &self,
        store: &RuntimeStore,
        session_id: &str,
    ) -> Result<(), VoiceProviderError> {
        let state = self
            .sessions
            .lock()
            .map_err(|_| VoiceProviderError::Operation)?
            .remove(session_id);
        let Some(state) = state.filter(|state| !state.canonical) else {
            return Ok(());
        };
        let session_for_read = session_id.to_owned();
        let session = store
            .read_async("runtime-services/voice/provider", move |connection| {
                Ok::<_, VoiceProviderError>(
                    load_row_any(connection, &session_for_read)?
                        .map(VoiceSession::from_row)
                        .transpose()?,
                )
            })
            .await?;
        let Some(session) = session else {
            return Ok(());
        };
        for (role, text) in state.tail {
            if text.is_empty() {
                continue;
            }
            let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(text.as_bytes()));
            record_native_transcript(
                store,
                &session,
                &format!("native:{}:tail:{role}:{digest}", session.id().as_str()),
                &role,
                &text,
                "",
                true,
            )
            .await?;
        }
        Ok(())
    }
}

async fn active_session_id(
    store: &RuntimeStore,
    account_key: &str,
    connection_id: &str,
    thread_id: &str,
) -> Result<Option<String>, VoiceProviderError> {
    let key = (
        thread_id.to_owned(),
        account_key.to_owned(),
        connection_id.to_owned(),
    );
    Ok(store
        .read_async("runtime-services/voice/provider", move |connection| {
            connection
                .query_row(
                    "SELECT id FROM voice_sessions WHERE native_thread=?1 AND account_key=?2 AND connection_id=?3 AND ended IS NULL ORDER BY created DESC LIMIT 1",
                    rusqlite::params![key.0, key.1, key.2],
                    |row| row.get(0),
                )
                .optional()
                .map_err(studio_runtime_store::StoreError::from)
        })
        .await?)
}

#[derive(Debug, Error)]
pub enum VoiceProviderError {
    #[error("Request id expired")]
    Expired,
    #[error(transparent)]
    Policy(#[from] policy::VoiceError),
    #[error(transparent)]
    Voice(#[from] super::VoiceError),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error(transparent)]
    Outbox(#[from] studio_runtime_store::outbox::OutboxError),
    #[error(transparent)]
    RequestResult(#[from] studio_runtime_store::request_results::RequestResultError),
    #[error(transparent)]
    Provider(Box<crate::account_worker::WorkerError>),
    #[error("voice provider context is unavailable")]
    ContextUnavailable,
    #[error("voice provider context changed")]
    ContextChanged,
    #[error("voice provider request was not sent")]
    NotSent,
    #[error("voice provider request outcome is unknown")]
    Unknown,
    #[error("voice provider rejected the request")]
    Rejected,
    #[error("voice provider adapter is unavailable")]
    AdapterUnavailable,
    #[error("Not implemented: requires {0}")]
    RequiresDependency(&'static str),
    #[error(transparent)]
    Monitor(#[from] crate::monitor_policy::MonitorError),
    #[error("voice operation failed")]
    Operation,
}

impl From<rusqlite::Error> for VoiceProviderError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Store(studio_runtime_store::StoreError::from(error))
    }
}

/// Cancel a session durably before submitting realtime stop. An absent id
/// becomes a tombstone so a later start with that identity cannot capture.
pub async fn end(
    store: std::sync::Arc<RuntimeStore>,
    provider: std::sync::Arc<dyn VoiceProviderPort>,
    agent_id: String,
    session_id: String,
    now: f64,
) -> Result<Value, VoiceProviderError> {
    let dispatch = session_dispatch_guard(&agent_id, &session_id).await;
    let dispatch = if pending_start_completion(&agent_id, &session_id).is_some() {
        drop(dispatch);
        wait_for_start_setup(&agent_id, &session_id).await;
        session_dispatch_guard(&agent_id, &session_id).await
    } else {
        dispatch
    };
    let (value, context) = prepare_end(&store, &agent_id, &session_id, now).await?;
    // The durable Stopping transition serializes duplicate end calls. Keep the
    // per-session guard out of the provider RPC wait.
    drop(dispatch);
    let Some((intent_id, session)) = context else {
        return Ok(value);
    };
    let outcomes = end_voice_sessions_tracked(
        store.clone(),
        provider,
        intent_id,
        OutboxKind::VoiceStop,
        session,
        now,
    )
    .await?;
    if let Some(certainty) = outcomes
        .first()
        .copied()
        .filter(|certainty| *certainty != VoiceWriteCertainty::Written)
    {
        return Err(certainty_error(certainty));
    }
    load_session(&store, &agent_id, &session_id).await
}

/// Read-only replay path for terminal sessions when the worker adapter is not
/// attached. Active sessions still require the worker to send realtime stop.
pub async fn read_terminal_session(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<Value>, VoiceProviderError> {
    let agent_id = agent_id.to_owned();
    let session_id = session_id.to_owned();
    store
        .read_async("runtime-services/voice/provider", move |connection| {
            let agent_id = agent_id.as_str();
            let session_id = session_id.as_str();
            super::checked_agent(connection, agent_id)
                .map_err(|_| VoiceProviderError::ContextUnavailable)?;
            let Some(row) = load_row(connection, session_id, agent_id)? else {
                return Ok(None);
            };
            let session = VoiceSession::from_row(row)?;
            if session.ended().is_some()
                || session.state().is_some_and(|state| {
                    matches!(
                        state,
                        policy::VoiceSessionState::Ended
                            | policy::VoiceSessionState::Lost
                            | policy::VoiceSessionState::Failed
                    )
                })
            {
                Ok(Some(session_value(&session)))
            } else {
                Ok(None)
            }
        })
        .await
}

/// Replay an existing speech receipt without dispatching a new provider RPC.
pub async fn read_speech_receipt(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    record_id: &str,
) -> Result<Option<Value>, VoiceProviderError> {
    let agent_id = agent_id.to_owned();
    let session_id = session_id.to_owned();
    let record_id = record_id.to_owned();
    store
        .read_async("runtime-services/voice/provider", move |connection| {
            let agent_id = agent_id.as_str();
            let session_id = session_id.as_str();
            let record_id = record_id.as_str();
            super::checked_agent(connection, agent_id)
                .map_err(|_| VoiceProviderError::ContextUnavailable)?;
            if load_row(connection, session_id, agent_id)?.is_none() {
                return Ok(None);
            }
            lookup_speech_receipt(
                connection,
                session_id,
                record_id,
                &speech_receipt_id(session_id, record_id),
            )
        })
        .await
}

fn public_speech_receipt(mut receipt: Value) -> Value {
    if let Some(error) = receipt["error"].as_str()
        && error.starts_with("voice-content-sha256:")
    {
        receipt["error"] = error.split_once('|').map_or(Value::Null, |(_, detail)| {
            if detail.is_empty() {
                Value::Null
            } else {
                Value::String(detail.to_owned())
            }
        });
    }
    receipt
}

fn speech_receipt_id(session_id: &str, record_id: &str) -> String {
    format!("{session_id}:{record_id}")
}

fn lookup_speech_receipt(
    connection: &Connection,
    session_id: &str,
    record_id: &str,
    request_id: &str,
) -> Result<Option<Value>, VoiceProviderError> {
    lookup_speech_receipt_raw(connection, session_id, record_id, request_id)
        .map(|receipt| receipt.map(public_speech_receipt))
}

fn lookup_speech_receipt_raw(
    connection: &Connection,
    session_id: &str,
    record_id: &str,
    request_id: &str,
) -> Result<Option<Value>, VoiceProviderError> {
    let receipt = match VoiceRepository::speech_receipt(connection, request_id)? {
        Some(receipt) => Some(receipt),
        None => {
            VoiceRepository::speech_receipt(connection, &speech_receipt_id(session_id, record_id))?
        }
    };
    if let Some(receipt) = receipt {
        if receipt["session"] == session_id && receipt["record_id"] == record_id {
            return Ok(Some(receipt));
        }
        return Err(policy::VoiceError::SessionIdentityConflict.into());
    }
    Ok(None)
}

async fn prepare_end(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    now: f64,
) -> Result<(Value, Option<(String, VoiceDeleteSession)>), VoiceProviderError> {
    let id = policy::SessionId::parse(session_id.to_owned())?;
    if !now.is_finite() || now < 0.0 {
        return Err(policy::VoiceError::InvalidClock.into());
    }
    let agent_id = agent_id.to_owned();
    store.transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
        let agent = super::checked_agent(transaction, &agent_id)
            .map_err(|_| VoiceProviderError::ContextUnavailable)?;
        let Some(row) = load_row(transaction, id.as_str(), &agent_id)? else {
            transaction.execute(
                "INSERT INTO voice_sessions(id,agent,created,ended,state) VALUES(?1,?2,?3,?3,'ended')",
                rusqlite::params![id.as_str(),&agent_id,now],
            )?;
            return Ok((json!({"session_id":id.as_str(),"state":"ended","sdp":null,"error":null,"ended":now}), None));
        };
        let session = VoiceSession::from_row(row)?;
        if existing_stop(transaction, &agent_id, id.as_str())? {
            return Ok((session_value(&session),None));
        }
        if session.ended().is_some()
            || matches!(session.state(), Some(
                policy::VoiceSessionState::Ended
                    | policy::VoiceSessionState::Stopping
                    | policy::VoiceSessionState::Unknown
            ))
        {
            return Ok((session_value(&session), None));
        }
        let (transition, context) = match (
            session.native_thread(), session.account_key(), session.connection_id(),
        ) {
            (Some(thread), Some(account), Some(_connection)) => {
                let current_thread = agent.extra.get("threadId").and_then(Value::as_str);
                let current_account = agent.extra.get("accountKey").and_then(Value::as_str).unwrap_or("default");
                if current_thread != Some(thread) || current_account != account {
                    let transition = policy::transition_session(
                        &session, SessionSignal::ProviderDisconnected, now,
                    )?;
                    update_session(transaction, &transition.session)?;
                    return Ok((session_value(&transition.session), None));
                }
                (
                    policy::transition_session(
                        &session,
                        SessionSignal::StopRequested { start_dispatched: true },
                        now,
                    )?,
                    Some(()),
                )
            }
            _ => (
                policy::transition_session(
                    &session,
                    SessionSignal::StopRequested { start_dispatched: false },
                    now,
                )?,
                None,
            ),
        };
        update_session(transaction, &transition.session)?;
        let context = if context.is_some() {
            stage_ordinary_stop(transaction, &agent_id, id.as_str())?.map(|intent| (intent.id, intent.session))
        } else { None };
        let current = load_row(transaction, id.as_str(), &agent_id)?.ok_or(VoiceProviderError::ContextUnavailable)?;
        Ok((session_value(&VoiceSession::from_row(current)?), context))
    }).await
}

/// Submit orchestrator speech once. The durable receipt is reserved first and
/// survives provider failure or an uncertain request write.
pub async fn speech(
    store: std::sync::Arc<RuntimeStore>,
    provider: std::sync::Arc<dyn VoiceProviderPort>,
    agent_id: String,
    session_id: String,
    record_id: String,
    request_id: Option<String>,
) -> Result<Value, VoiceProviderError> {
    let receipt_id = request_id
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| speech_receipt_id(&session_id, &record_id));
    if receipt_id.chars().count() > 480 {
        return Err(VoiceProviderError::Policy(
            policy::VoiceError::InvalidSessionIdentity,
        ));
    }
    let reservation =
        reserve_speech(&store, &agent_id, &session_id, &record_id, &receipt_id).await?;
    let Some((context, text, root_request_id, admitted_request)) = reservation else {
        return stored_speech_receipt(&store, session_id, record_id, receipt_id).await;
    };
    let thread_id = context.thread_id.clone();
    let call = VoiceRpcCall {
        operation_id: format!("voice:speech:{receipt_id}"),
        root_request_id,
        admitted_request: Some(admitted_request),
        context,
        method: Method::ThreadRealtimeAppendSpeech,
        params: json!({"threadId": thread_id, "text": text}),
        deadline: request_deadline(),
    };
    let outcome = request(provider.as_ref(), call).await;
    let state = match outcome.certainty {
        VoiceWriteCertainty::Expired => ("failed", Some("Request id expired")),
        VoiceWriteCertainty::Written => ("submitted", None),
        VoiceWriteCertainty::NotSent => ("failed", Some("Request was not sent")),
        VoiceWriteCertainty::Unknown => ("unknown", Some("Request outcome is unknown")),
        VoiceWriteCertainty::Rejected => ("failed", Some("Provider rejected request")),
    };
    let id = receipt_id.clone();
    let state_name = state.0.to_owned();
    let state_error = state.1;
    update_speech(&store, &id, &state_name, state_error).await?;
    if outcome.certainty != VoiceWriteCertainty::Written {
        return Err(certainty_error(outcome.certainty));
    }
    stored_speech_receipt(&store, session_id, record_id, receipt_id).await
}

/// The public form of a speech receipt that must already be stored.
async fn stored_speech_receipt(
    store: &RuntimeStore,
    session_id: String,
    record_id: String,
    receipt_id: String,
) -> Result<Value, VoiceProviderError> {
    store
        .read_async("runtime-services/voice/provider", move |connection| {
            lookup_speech_receipt(connection, &session_id, &record_id, &receipt_id)?
                .ok_or(VoiceProviderError::Operation)
        })
        .await
}

enum ApprovalDispatch {
    Replay(Value),
    MonitorUnavailable,
    Monitor {
        request_id: String,
    },
    Send {
        request_id: String,
        context: VoiceProviderContext,
        rpc_id: VoiceRpcId,
    },
}

/// Whether this exact voice approval speech points at a command-watch request.
/// The HTTP adapter uses this to select the monitor transaction and wake lane.
pub async fn is_monitor_approval(
    store: &RuntimeStore,
    agent_id: &str,
    speech_id: &str,
) -> Result<bool, VoiceProviderError> {
    let agent_id = agent_id.to_owned();
    let speech_id = speech_id.to_owned();
    store
        .read_async("runtime-services/voice/provider", move |connection| {
            let agent_id = agent_id.as_str();
            let speech_id = speech_id.as_str();
            let Some(speech) = VoiceRepository::record(connection, speech_id)? else {
                return Ok(false);
            };
            if speech.agent != agent_id || speech.kind != "orchestrator" {
                return Ok(false);
            }
            let metadata: Value =
                serde_json::from_str(&speech.payload).map_err(|_| VoiceProviderError::Operation)?;
            let Some(request_id) = metadata.get("request_id").and_then(Value::as_str) else {
                return Ok(false);
            };
            let request = connection
                .query_row(
                    "SELECT record FROM runtime_requests WHERE id=?1",
                    [request_id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?;
            let Some(request) = request else {
                return Ok(false);
            };
            let request: Value =
                serde_json::from_str(&request).map_err(|_| VoiceProviderError::Operation)?;
            Ok(request.get("method").and_then(Value::as_str) == Some("monitor/approve"))
        })
        .await
}

/// Complete a monitor approval through the monitor owner. The monitor update
/// is itself transactional and idempotent; if it proves no write, release the
/// voice receipt for a same-identity retry.
pub async fn approve_monitor(
    store: std::sync::Arc<RuntimeStore>,
    agent_id: String,
    session_id: String,
    speech_id: String,
    transcript_id: String,
    now: f64,
    start_dispatch_available: bool,
) -> Result<Value, VoiceProviderError> {
    let (request_id, replay) = match reserve_approval(
        &store,
        &agent_id,
        &session_id,
        &speech_id,
        &transcript_id,
        start_dispatch_available,
    )
    .await?
    {
        ApprovalDispatch::Replay(value) => Ok((String::new(), Some(value))),
        ApprovalDispatch::MonitorUnavailable => Err(VoiceProviderError::RequiresDependency(
            "monitor-driver/start-dispatch",
        )),
        ApprovalDispatch::Monitor { request_id } => Ok((request_id, None)),
        ApprovalDispatch::Send { .. } => Err(VoiceProviderError::Operation),
    }?;
    if let Some(value) = replay {
        return Ok(value);
    }
    let request_for_answer = request_id.clone();
    let data = serde_json::json!({"decision":"accept"});
    let answer = crate::monitor_runtime::answer_approval(
        &store,
        &request_for_answer,
        &data,
        now,
        start_dispatch_available,
    )
    .await;
    match answer {
        Ok(Some(answer)) if answer.accepted => {
            let result = answer.response;
            let saved =
                serde_json::to_string(&result).map_err(|_| VoiceProviderError::Operation)?;
            let update_id = request_id.clone();
            let update_saved = saved;
            store.transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
                    transaction.execute(
                        "UPDATE voice_approvals SET status='accepted',result=?1 WHERE id=?2 AND status='uncertain'",
                        rusqlite::params![update_saved, update_id],
                    )?;
                    Ok(())
                })
            .await?;
            Ok(result)
        }
        Ok(_) => {
            release_monitor_approval(&store, &request_id).await?;
            Err(VoiceProviderError::RequiresDependency(
                "monitor-approval/voice",
            ))
        }
        Err(error) => {
            release_monitor_approval(&store, &request_id).await?;
            Err(VoiceProviderError::Monitor(error))
        }
    }
}

async fn release_monitor_approval(
    store: &std::sync::Arc<RuntimeStore>,
    request_id: &str,
) -> Result<(), VoiceProviderError> {
    let id = request_id.to_owned();
    store
        .transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
            transaction.execute(
                "UPDATE voice_approvals SET status='not_sent' WHERE id=?1 AND status='uncertain'",
                [id],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

pub async fn approve(
    store: std::sync::Arc<RuntimeStore>,
    provider: std::sync::Arc<dyn VoiceProviderPort>,
    agent_id: String,
    session_id: String,
    speech_id: String,
    transcript_id: String,
    _now: f64,
) -> Result<Value, VoiceProviderError> {
    let reservation = reserve_approval(
        &store,
        &agent_id,
        &session_id,
        &speech_id,
        &transcript_id,
        false,
    )
    .await?;
    let (request_id, context, rpc_id) = match reservation {
        ApprovalDispatch::Replay(value) => return Ok(value),
        ApprovalDispatch::MonitorUnavailable => {
            return Err(VoiceProviderError::RequiresDependency(
                "monitor-approval/voice",
            ));
        }
        ApprovalDispatch::Monitor { .. } => {
            return Err(VoiceProviderError::RequiresDependency(
                "monitor-approval/voice",
            ));
        }
        ApprovalDispatch::Send {
            request_id,
            context,
            rpc_id,
        } => (request_id, context, rpc_id),
    };

    let receipt = provider
        .reply(
            context,
            format!("voice:approve:{request_id}"),
            VoiceReply {
                rpc_id,
                result: json!({"decision": "accept"}),
            },
            request_deadline(),
        )
        .await;
    let certainty = receipt.certainty;
    let id = request_id.clone();
    let state = match certainty {
        VoiceWriteCertainty::Expired => "expired",
        VoiceWriteCertainty::Written => "accepted",
        VoiceWriteCertainty::NotSent => "not_sent",
        VoiceWriteCertainty::Unknown => "unknown",
        VoiceWriteCertainty::Rejected => "unknown",
    };
    let state_value = state.to_owned();
    let result = json!({"status": "answered"});
    let result_json = (certainty == VoiceWriteCertainty::Written)
        .then(|| serde_json::to_string(&result))
        .transpose()
        .map_err(|_| VoiceProviderError::Operation)?;
    store
        .transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
            transaction.execute(
                "UPDATE voice_approvals SET status=?1,result=?2 WHERE id=?3",
                rusqlite::params![state_value, result_json, id],
            )?;
            Ok(())
        })
        .await?;
    if certainty != VoiceWriteCertainty::Written {
        return Err(certainty_error(certainty));
    }
    Ok(result)
}

async fn reserve_approval(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    speech_id: &str,
    transcript_id: &str,
    monitor_runtime_available: bool,
) -> Result<ApprovalDispatch, VoiceProviderError> {
    let agent_id = agent_id.to_owned();
    let session_id = session_id.to_owned();
    let speech_id = speech_id.to_owned();
    let transcript_id = transcript_id.to_owned();
    store.transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
        let actor = super::checked_agent(transaction, &agent_id)
            .map_err(|_| VoiceProviderError::ContextUnavailable)?;
        let transcript = VoiceRepository::record(transaction, &transcript_id)?
            .ok_or(policy::VoiceError::UnknownSession)?;
        super::validate_record(&transcript)?;
        if transcript.agent != agent_id
            || transcript.session != session_id
            || transcript.kind != "user"
            || transcript
                .text
                .trim()
                .to_lowercase()
                .trim_matches(|character| matches!(character, '.' | '!' | '?' | ' '))
                != "разрешаю"
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        let played = transaction.query_row(
            "SELECT seq,payload FROM voice_records WHERE agent=?1 AND session=?2 AND kind='playback' AND text='played' ORDER BY seq DESC LIMIT 1",
            rusqlite::params![&agent_id, &session_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        ).optional()?;
        let Some((played_seq, played_payload)) = played else {
            return Err(VoiceProviderError::ContextChanged);
        };
        let played_payload: Value = serde_json::from_str(&played_payload)
            .map_err(|_| VoiceProviderError::Operation)?;
        if played_payload.get("record_id").and_then(Value::as_str) != Some(speech_id.as_str())
            || transcript.sequence <= played_seq
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        let speech = VoiceRepository::record(transaction, &speech_id)?
            .ok_or(policy::VoiceError::UnknownSession)?;
        super::validate_record(&speech)?;
        if speech.agent != agent_id || speech.kind != "orchestrator" {
            return Err(policy::VoiceError::UnknownSession.into());
        }
        let metadata: Value = serde_json::from_str(&speech.payload)
            .map_err(|_| VoiceProviderError::Operation)?;
        let request_id = metadata
            .get("request_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(VoiceProviderError::Operation)?
            .to_owned();
        let previous = approval_receipt(transaction, &request_id)?;
        if let Some((owner, old_speech, old_transcript, status, result)) = previous.as_ref() {
            if owner != &agent_id || old_speech != &speech_id || old_transcript != &transcript_id {
                return Err(VoiceProviderError::ContextChanged);
            }
            if status == "expired" { return Err(VoiceProviderError::Expired); }
            if status == "accepted" {
                let value = result.as_ref().ok_or(VoiceProviderError::Operation)?;
                return Ok(ApprovalDispatch::Replay(serde_json::from_str(value).map_err(|_| VoiceProviderError::Operation)?));
            }
            if status == "uncertain" {
                let current: String = transaction.query_row(
                    "SELECT record FROM runtime_requests WHERE id=?1",
                    [&request_id],
                    |row| row.get(0),
                ).optional()?.ok_or(VoiceProviderError::ContextUnavailable)?;
                let current: Value = serde_json::from_str(&current)
                    .map_err(|_| VoiceProviderError::Operation)?;
                if current.get("method").and_then(Value::as_str) != Some("monitor/approve") {
                    return Err(VoiceProviderError::Unknown);
                }
                if current.get("status").and_then(Value::as_str) == Some("answered")
                    && current.get("answerDecision").and_then(Value::as_str) == Some("accept")
                {
                    let replay = json!({"status":"answered","replayed":true});
                    transaction.execute(
                        "UPDATE voice_approvals SET status='accepted',result=?1 WHERE id=?2",
                        rusqlite::params![serde_json::to_string(&replay).map_err(|_| VoiceProviderError::Operation)?, request_id.as_str()],
                    )?;
                    return Ok(ApprovalDispatch::Replay(replay));
                }
                if current.get("status").and_then(Value::as_str) != Some("pending") {
                    return Err(VoiceProviderError::Unknown);
                }
            } else if status != "not_sent" {
                return Err(VoiceProviderError::Unknown);
            }
        }
        let current = transaction.query_row(
            "SELECT record FROM runtime_requests WHERE id=?1",
            [&request_id],
            |row| row.get::<_, String>(0),
        ).optional()?.ok_or(VoiceProviderError::ContextUnavailable)?;
        let current: Value = serde_json::from_str(&current)
            .map_err(|_| VoiceProviderError::Operation)?;
        let method = current.get("method").and_then(Value::as_str);
        let fingerprint = super::approval_fingerprint(&current);
        if current.get("agent").and_then(Value::as_str) != Some(agent_id.as_str())
            || current.get("status").and_then(Value::as_str) != Some("pending")
            || !method.is_some_and(|method| super::APPROVAL_METHODS.contains(&method))
            || metadata.get("fingerprint").and_then(Value::as_str) != Some(fingerprint.as_str())
            || current.get("epoch").and_then(Value::as_i64) != Some(actor.epoch)
            || actor.extra.get("autoWake").and_then(Value::as_bool) != Some(true)
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        if method == Some("monitor/approve") {
            if !monitor_runtime_available {
                return Ok(ApprovalDispatch::MonitorUnavailable);
            }
            let has_native_session: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM voice_sessions WHERE id=?1 AND native_thread IS NOT NULL)",
                [session_id],
                |row| row.get(0),
            )?;
            if has_native_session {
                return Err(VoiceProviderError::ContextChanged);
            }
            if previous.is_some() {
                transaction.execute("UPDATE voice_approvals SET status='uncertain' WHERE id=?1", [&request_id])?;
            } else {
                transaction.execute(
                    "INSERT INTO voice_approvals(id,agent,speech,transcript,status,result) VALUES(?1,?2,?3,?4,'uncertain',NULL)",
                    rusqlite::params![request_id, agent_id, speech_id, transcript_id],
                )?;
            }
            return Ok(ApprovalDispatch::Monitor { request_id });
        }
        let rpc_id = current.get("rpcId").and_then(parse_rpc_id)
            .ok_or(VoiceProviderError::ContextUnavailable)?;
        let session = load_row(transaction, &session_id, &agent_id)?
            .ok_or(policy::VoiceError::UnknownSession)?;
        let session = VoiceSession::from_row(session)?;
        if session.native_thread().is_some() {
            return Err(VoiceProviderError::ContextChanged);
        }
        let thread_id = actor
            .extra
            .get("threadId")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or(VoiceProviderError::ContextUnavailable)?;
        let account_key = actor
            .extra
            .get("accountKey")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .unwrap_or("default");
        if session.account_key() != Some(account_key) {
            return Err(VoiceProviderError::ContextChanged);
        }
        let scope = required_scope(transaction, &session_id)?;
        if session.connection_id() != Some(scope.connection.as_str()) { return Err(VoiceProviderError::ContextChanged); }
        let context = VoiceProviderContext {
            agent_id: agent_id.to_owned(),
            epoch: actor.epoch,
            account_key: account_key.to_owned(),
            thread_id: thread_id.to_owned(),
            connection_id: session.connection_id().ok_or(VoiceProviderError::ContextUnavailable)?.to_owned(),
            generation: scope.generation,
        };
        let request_account = current.get("accountKey").and_then(Value::as_str);
        let request_connection = current.get("connectionId").and_then(Value::as_str);
        let request_thread = current.get("params").and_then(|params| params.get("threadId")).and_then(Value::as_str);
        // Python's connectionId is a generation-scoped identity. Bind the stored
        // RPC to the exact retained connection, thread and account before reply.
        if request_account != Some(context.account_key.as_str())
            || request_connection != Some(context.connection_id.as_str())
            || request_thread != Some(context.thread_id.as_str())
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        if previous.is_some() {
            transaction.execute("UPDATE voice_approvals SET status='uncertain' WHERE id=?1", [&request_id])?;
        } else {
            transaction.execute(
                "INSERT INTO voice_approvals(id,agent,speech,transcript,status,result) VALUES(?1,?2,?3,?4,'uncertain',NULL)",
                rusqlite::params![request_id, agent_id, speech_id, transcript_id],
            )?;
        }
        Ok(ApprovalDispatch::Send {
            request_id,
            context,
            rpc_id,
        })
    }).await
}

type ApprovalReceiptRow = (String, String, String, String, Option<String>);

fn approval_receipt(
    connection: &Connection,
    request_id: &str,
) -> Result<Option<ApprovalReceiptRow>, VoiceProviderError> {
    Ok(connection
        .query_row(
            "SELECT agent,speech,transcript,status,result FROM voice_approvals WHERE id=?1",
            [request_id],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .optional()?)
}

fn parse_rpc_id(value: &Value) -> Option<VoiceRpcId> {
    match value {
        Value::String(value) if !value.is_empty() => Some(VoiceRpcId::String(value.clone())),
        Value::Number(value) => value.as_i64().map(VoiceRpcId::Number),
        Value::Null | Value::Bool(_) | Value::String(_) | Value::Array(_) | Value::Object(_) => {
            None
        }
    }
}

async fn reserve_speech(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    record_id: &str,
    request_id: &str,
) -> Result<Option<(VoiceProviderContext, String, String, AdmittedRequest)>, VoiceProviderError> {
    let agent_id = agent_id.to_owned();
    let session_id = session_id.to_owned();
    let record_id = record_id.to_owned();
    let request_id = request_id.to_owned();
    store.transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
        let agent = super::checked_agent(transaction, &agent_id)
            .map_err(|_| VoiceProviderError::ContextUnavailable)?;
        let session = load_row(transaction, &session_id, &agent_id)?
            .ok_or(policy::VoiceError::UnknownSession)?;
        let session = VoiceSession::from_row(session)?;
        if let Some(old) = lookup_speech_receipt_raw(transaction, &session_id, &record_id, &request_id)? {
            if old["session"] != session_id || old["record_id"] != record_id {
                return Err(VoiceProviderError::Policy(policy::VoiceError::SessionIdentityConflict));
            }
            if let Some(marker) = old["error"].as_str().and_then(|error| error.strip_prefix("voice-content-sha256:")) {
                let stored = marker.split('|').next().unwrap_or_default();
                let record = VoiceRepository::record(transaction, &record_id)?
                    .ok_or(policy::VoiceError::UnknownSession)?;
                super::validate_record(&record)?;
                if record.agent != agent_id
                    || record.kind != "orchestrator"
                    || stored != studio_runtime_store::sha256_hex(record.text.as_bytes())
                {
                    return Err(policy::VoiceError::SessionIdentityConflict.into());
                }
            }
            return Ok(None);
        }
        let record = VoiceRepository::record(transaction, &record_id)?
            .ok_or(policy::VoiceError::UnknownSession)?;
        super::validate_record(&record)?;
        if record.agent != agent_id || record.kind != "orchestrator" {
            return Err(policy::VoiceError::UnknownSession.into());
        }
        if session.ended().is_some() || session.state() != Some(policy::VoiceSessionState::Ready) {
            return Err(VoiceProviderError::Policy(policy::VoiceError::UnknownSession));
        }
        let account_key = session.account_key().ok_or(VoiceProviderError::ContextUnavailable)?;
        let thread_id = session.native_thread().ok_or(VoiceProviderError::ContextUnavailable)?;
        if agent.extra.get("accountKey").and_then(Value::as_str).unwrap_or("default") != account_key
            || agent.extra.get("threadId").and_then(Value::as_str) != Some(thread_id)
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        let scope = required_scope(transaction, &session_id)?;
        if session.connection_id() != Some(scope.connection.as_str()) { return Err(VoiceProviderError::ContextChanged); }
        let context = VoiceProviderContext {
            agent_id: agent_id.to_owned(), epoch: agent.epoch,
            account_key: account_key.to_owned(),
            thread_id: thread_id.to_owned(),
            connection_id: session.connection_id().ok_or(VoiceProviderError::ContextUnavailable)?.to_owned(),
            generation: scope.generation,
        };
        let root = RequestIdentity::issue_root();
        let identity = RequestIdentity::new("voice-speech", &[&agent_id], &root, &[&request_id])
            .map_err(|_| VoiceProviderError::ContextChanged)?;
        let owner = match RequestResults::admit(transaction, &identity, "voice-speech", identity.issued_ms())? {
            Admission::Fresh(owner) => owner,
            Admission::Replay(_) => return Err(VoiceProviderError::ContextChanged),
        };
        let intent = crate::account_worker::retained::ProviderCallIntent {
            root_request_id: root.clone(), admitted_request: None,
            scope: crate::account_session_runtime::ConnectionScope { account: context.account_key.clone(), connection: context.connection_id.clone(), generation: context.generation },
            operation: format!("voice:speech:{request_id}"),
            actor: Some(crate::account_worker::retained::RequestActor { agent: context.agent_id.clone(), epoch: context.epoch }),
            transfer: None, deleted_actor_session: None, method: Method::ThreadRealtimeAppendSpeech,
            params: json!({"threadId": context.thread_id, "text": record.text}),
        };
        let admitted = crate::account_worker::retained::reserve_provider_call(transaction, &owner, intent)
            .map_err(|error| VoiceProviderError::Provider(Box::new(error)))?;
        let marker = format!("voice-content-sha256:{}|", studio_runtime_store::sha256_hex(record.text.as_bytes()));
        transaction.execute(
            "INSERT INTO voice_speech_requests(id,session,record_id,state,error) VALUES(?1,?2,?3,'pending',?4)",
            rusqlite::params![&request_id,&session_id,&record_id,marker],
        )?;
        Ok(Some((context, record.text, root, admitted)))
    }).await
}

fn update_session(
    connection: &Connection,
    session: &VoiceSession,
) -> Result<(), VoiceProviderError> {
    connection.execute(
        "UPDATE voice_sessions SET state=?1,error=?2,ended=?3,answer=?4 WHERE id=?5",
        rusqlite::params![
            session.state().map(policy::VoiceSessionState::as_str),
            session.error(),
            session.ended(),
            session.answer(),
            session.id().as_str()
        ],
    )?;
    Ok(())
}

async fn update_speech(
    store: &RuntimeStore,
    request_id: &str,
    state: &str,
    error: Option<&str>,
) -> Result<(), VoiceProviderError> {
    let request_id = request_id.to_owned();
    let state = state.to_owned();
    let error = error.map(str::to_owned);
    store
        .transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
            let current: Option<String> = transaction
                .query_row(
                    "SELECT error FROM voice_speech_requests WHERE id=?1",
                    [request_id.as_str()],
                    |row| row.get(0),
                )
                .optional()?
                .flatten();
            let marker = current
                .and_then(|value| value.split('|').next().map(str::to_owned))
                .unwrap_or_default();
            let detail = error.as_deref().unwrap_or_default();
            let stored_error = if marker.is_empty() {
                error.clone()
            } else if detail.is_empty() {
                Some(format!("{marker}|"))
            } else {
                Some(format!("{marker}|{detail}"))
            };
            transaction.execute(
                "UPDATE voice_speech_requests SET state=?1,error=?2 WHERE id=?3",
                rusqlite::params![state, stored_error, request_id.as_str()],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

pub async fn handle_notification(
    store: &RuntimeStore,
    account_key: &str,
    connection_id: &str,
    message: &Value,
    now: f64,
) -> Result<bool, VoiceProviderError> {
    let method = message
        .get("method")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if !method.starts_with("thread/realtime/") {
        return Ok(false);
    }
    let params = message
        .get("params")
        .filter(|value| value.is_object())
        .ok_or(VoiceProviderError::Operation)?;
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let key = (
        thread_id.to_owned(),
        account_key.to_owned(),
        connection_id.to_owned(),
    );
    let session = store
        .read_async("runtime-services/voice/provider", move |connection| {
            let (thread_id, account_key, connection_id) = &key;
            Ok::<_, VoiceProviderError>(connection.query_row(
                "SELECT id,agent,created,ended,sdp,answer,native_thread,account_key,connection_id,state,error \
                 FROM voice_sessions WHERE native_thread=?1 AND account_key=?2 AND connection_id=?3 \
                 AND ended IS NULL ORDER BY created DESC LIMIT 1",
                rusqlite::params![thread_id,account_key,connection_id], session_from_row,
            ).optional()?.map(VoiceSession::from_row).transpose()?)
        })
        .await?;
    let Some(session_value) = session else {
        return Ok(true);
    };
    let event_session = params
        .get("realtimeSessionId")
        .and_then(Value::as_str)
        .or_else(|| {
            params
                .get("item")
                .and_then(|item| item.get("realtimeSessionId"))
                .and_then(Value::as_str)
        });
    if event_session.is_some_and(|value| value != session_value.id().as_str()) {
        return Ok(true);
    }
    if method == "thread/realtime/item/started"
        && params
            .get("item")
            .and_then(|item| item.get("type"))
            .and_then(Value::as_str)
            == Some("transcriptSegment")
    {
        return Ok(true);
    }
    if method == "thread/realtime/item/completed" {
        if let Some(item) = params
            .get("item")
            .filter(|item| item.get("type").and_then(Value::as_str) == Some("transcriptSegment"))
        {
            let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
            let role = if item.get("role").and_then(Value::as_str) == Some("user") {
                "user"
            } else {
                "assistant"
            };
            record_native_transcript(
                store,
                &session_value,
                &format!("native:{}:{item_id}", session_value.id().as_str()),
                role,
                item.get("text").and_then(Value::as_str).unwrap_or_default(),
                item_id,
                false,
            )
            .await?;
        }
        return Ok(true);
    }
    if method == "thread/realtime/transcript/done" {
        let role = match params.get("role").and_then(Value::as_str) {
            Some("user") => "user",
            Some("assistant") => "assistant",
            _ => return Ok(true),
        };
        let text = params
            .get("text")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if !text.is_empty() {
            let digest = Sha256::digest(text.as_bytes());
            let digest = data_encoding::HEXLOWER.encode(&digest);
            record_native_transcript(
                store,
                &session_value,
                &format!(
                    "native:{}:fallback:{role}:{digest}",
                    session_value.id().as_str()
                ),
                role,
                text,
                "",
                false,
            )
            .await?;
        }
        return Ok(true);
    }
    match method {
        "thread/realtime/sdp" => {
            let answer = params
                .get("sdp")
                .and_then(Value::as_str)
                .ok_or(VoiceProviderError::Operation)?;
            let transition = policy::transition_session(
                &session_value,
                SessionSignal::AnswerReceived(answer.to_owned()),
                now,
            )?;
            store
                .transaction_with_error(move |transaction| {
                    update_session(transaction, &transition.session)
                })
                .await?;
        }
        "thread/realtime/error" => {
            let transition = policy::transition_session(
                &session_value,
                SessionSignal::ProviderFailed(None),
                now,
            )?;
            store
                .transaction_with_error(move |transaction| {
                    update_session(transaction, &transition.session)
                })
                .await?;
        }
        "thread/realtime/closed" => {
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let transition =
                policy::transition_session(&session_value, SessionSignal::PeerClosed(reason), now)?;
            store
                .transaction_with_error(move |transaction| {
                    update_session(transaction, &transition.session)
                })
                .await?;
        }
        _ => {}
    }
    Ok(true)
}

/// Persist provider-session disconnects for every active voice session owned
/// by that account connection. The account worker calls this from its session
/// disconnect callback before discarding the generic RPC connection.
pub async fn handle_disconnect(
    store: &RuntimeStore,
    account_key: &str,
    connection_id: &str,
    now: f64,
) -> Result<usize, VoiceProviderError> {
    let key = (account_key.to_owned(), connection_id.to_owned());
    let sessions = store
        .read_async("runtime-services/voice/provider", move |connection| {
            let mut statement = connection.prepare_cached(
                "SELECT id,agent,created,ended,sdp,answer,native_thread,account_key,connection_id,state,error \
                 FROM voice_sessions WHERE account_key=?1 AND connection_id=?2 AND ended IS NULL",
            )?;
            let rows = statement.query_map(rusqlite::params![key.0, key.1], session_from_row)?;
            rows.collect::<Result<Vec<_>, _>>()?
                .into_iter()
                .map(|row| VoiceSession::from_row(row).map_err(VoiceProviderError::from))
                .collect::<Result<Vec<_>, _>>()
        })
        .await?;
    let mut count = 0;
    for session in sessions {
        let transition =
            policy::transition_session(&session, SessionSignal::ProviderDisconnected, now)?;
        store
            .transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
                update_session(transaction, &transition.session)?;
                Ok(())
            })
            .await?;
        count += 1;
    }
    Ok(count)
}

/// Realtime errors require an explicit stop RPC after the actor persists the
/// failure, matching the Python callback's stop behavior.
pub fn notification_requires_stop(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str) == Some("thread/realtime/error")
}

async fn record_native_transcript(
    store: &RuntimeStore,
    session: &VoiceSession,
    event_id: &str,
    role: &str,
    text: &str,
    item_id: &str,
    partial: bool,
) -> Result<(), VoiceProviderError> {
    let agent = session.agent().0.clone();
    let mut payload = json!({"native": true});
    if item_id.is_empty() {
        payload["partial"] = Value::Bool(partial);
    }
    super::record(
        store,
        super::VoiceRecordInput {
            agent_id: agent,
            session_id: session.id().as_str().to_owned(),
            event_id: event_id.to_owned(),
            kind: role.to_owned(),
            text: text.to_owned(),
            item_id: item_id.to_owned(),
            previous_item_id: String::new(),
            payload: Some(payload),
            internal: true,
        },
    )
    .await?;
    Ok(())
}

/// Create and submit one realtime session. The durable connecting row is
/// committed before the provider call; retries return it without resending.
pub async fn start(
    store: std::sync::Arc<RuntimeStore>,
    provider: std::sync::Arc<dyn VoiceProviderPort>,
    agent_id: String,
    session_id: String,
    offer: String,
    now: f64,
) -> Result<Value, VoiceProviderError> {
    start_admitted(store, Some(provider), agent_id, session_id, offer, now).await
}

/// Admit a start even when provider wiring is unavailable. Python commits and
/// returns the connecting session before resolving the provider account; a
/// missing adapter is therefore recorded as asynchronous session failure.
pub async fn start_admitted(
    store: std::sync::Arc<RuntimeStore>,
    provider: Option<std::sync::Arc<dyn VoiceProviderPort>>,
    agent_id: String,
    session_id: String,
    offer: String,
    now: f64,
) -> Result<Value, VoiceProviderError> {
    let _dispatch = session_dispatch_guard(&agent_id, &session_id).await;
    let start_request = StartRequest::new(
        AgentId::from(agent_id.as_str()),
        session_id.clone(),
        offer.clone(),
    )?;
    let agent_for_read = agent_id.clone();
    let session_for_read = session_id.clone();
    let (existing, (account_key, thread_id, epoch)) = store
        .read_async("runtime-services/voice/provider", move |connection| {
            let existing = read_start_rows(
                connection,
                &agent_for_read,
                &session_for_read,
                start_request,
            )?;
            Ok::<_, VoiceProviderError>((existing, agent_context(connection, &agent_for_read)?))
        })
        .await?;
    if let StartDecision::Replay(session) = existing {
        return Ok(session_value(&session));
    }

    let saved = create_session(
        std::sync::Arc::clone(&store),
        agent_id.clone(),
        session_id.clone(),
        offer.clone(),
        VoiceStartContext {
            account_key: account_key.clone(),
            thread_id: thread_id.clone(),
            epoch,
            now,
        },
    )
    .await?;
    if let StartDecision::Replay(session) = saved {
        return Ok(session_value(&session));
    }

    // Match Python's durable acceptance boundary: persist connecting before
    // provider setup, then resolve the account and send start off the request.
    let response = load_session(&store, &agent_id, &session_id).await?;
    let start_completion = register_start_completion(&agent_id, &session_id);
    let completed_agent = agent_id.clone();
    let completed_session = session_id.clone();
    drop(_dispatch);
    tokio::spawn(async move {
        if let Err(error) = complete_start(
            store,
            provider,
            agent_id,
            session_id,
            offer,
            VoiceStartContext {
                account_key,
                thread_id,
                epoch,
                now,
            },
        )
        .await
        {
            eprintln!("Voice start setup failed: {error}");
        }
        finish_start_completion(&completed_agent, &completed_session, &start_completion);
    });
    Ok(response)
}

async fn complete_start(
    store: std::sync::Arc<RuntimeStore>,
    provider: Option<std::sync::Arc<dyn VoiceProviderPort>>,
    agent_id: String,
    session_id: String,
    offer: String,
    start: VoiceStartContext,
) -> Result<(), VoiceProviderError> {
    let VoiceStartContext {
        account_key,
        thread_id,
        epoch,
        now,
    } = start;
    let Some(provider) = provider else {
        set_start_failure(&store, &session_id, VoicePrepareFailure::NotSent, now).await?;
        return Err(VoiceProviderError::Operation);
    };
    let context = match provider
        .prepare(&agent_id, &session_id, &account_key, &thread_id, epoch)
        .await
    {
        Ok(context) => context,
        Err(certainty) => {
            set_start_failure(&store, &session_id, certainty, now).await?;
            return Err(certainty_error(certainty.into()));
        }
    };
    if thread_id.is_empty() {
        set_start_failure(&store, &session_id, VoicePrepareFailure::NotSent, now).await?;
        return Err(VoiceProviderError::ContextUnavailable);
    }
    if context.agent_id != agent_id
        || context.account_key != account_key
        || context.thread_id != thread_id
        || context.epoch != epoch
        || context.connection_id.is_empty()
    {
        set_start_failure(&store, &session_id, VoicePrepareFailure::NotSent, now).await?;
        return Err(VoiceProviderError::ContextChanged);
    }

    if let Err(error) = attach_start_context(&store, &agent_id, &session_id, &context).await {
        set_start_failure(&store, &session_id, VoicePrepareFailure::NotSent, now).await?;
        return Err(error);
    }

    let call = start_call(context, &session_id, &offer);
    let receipt = request(provider.as_ref(), call).await;
    let failure = match receipt.certainty {
        VoiceWriteCertainty::Expired => VoicePrepareFailure::Expired,
        VoiceWriteCertainty::Written => return Ok(()),
        VoiceWriteCertainty::NotSent => VoicePrepareFailure::NotSent,
        VoiceWriteCertainty::Unknown => VoicePrepareFailure::Unknown,
        VoiceWriteCertainty::Rejected => VoicePrepareFailure::Rejected,
    };
    set_start_failure(&store, &session_id, failure, now).await?;
    Err(certainty_error(failure.into()))
}

fn agent_context(
    connection: &Connection,
    agent_id: &str,
) -> Result<(String, String, i64), VoiceProviderError> {
    let agent = super::checked_agent(connection, agent_id)
        .map_err(|_| VoiceProviderError::ContextUnavailable)?;
    let thread_id = agent
        .extra
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|thread| !thread.is_empty())
        .unwrap_or_default();
    let account_key = agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .filter(|account| !account.is_empty())
        .unwrap_or("default");
    Ok((account_key.to_owned(), thread_id.to_owned(), agent.epoch))
}

fn read_start_rows(
    connection: &Connection,
    agent_id: &str,
    session_id: &str,
    request: StartRequest,
) -> Result<StartDecision, VoiceProviderError> {
    super::checked_agent(connection, agent_id)
        .map_err(|_| VoiceProviderError::ContextUnavailable)?;
    let old = load_row_any(connection, session_id)?
        .map(VoiceSession::from_row)
        .transpose()?;
    let active_row = connection.query_row(
        "SELECT id,agent,created,ended,sdp,answer,native_thread,account_key,connection_id,state,error \
         FROM voice_sessions WHERE agent=?1 AND state IS NOT NULL AND ended IS NULL \
         ORDER BY created DESC LIMIT 1",
        [agent_id], session_from_row,
    ).optional()?;
    let active = active_row.map(VoiceSession::from_row).transpose()?;
    Ok(policy::decide_start(
        Some(0.0),
        request,
        old.as_ref(),
        active.as_ref(),
    )?)
}

struct VoiceStartContext {
    account_key: String,
    thread_id: String,
    epoch: i64,
    now: f64,
}

async fn create_session(
    store: std::sync::Arc<RuntimeStore>,
    agent_id: String,
    session_id: String,
    offer: String,
    start: VoiceStartContext,
) -> Result<StartDecision, VoiceProviderError> {
    let VoiceStartContext {
        account_key,
        thread_id,
        epoch,
        now,
    } = start;
    let request = StartRequest::new(
        AgentId::from(agent_id.as_str()),
        session_id.clone(),
        offer.clone(),
    )?;
    store.transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
        let agent = super::checked_agent(transaction, &agent_id)
            .map_err(|_| VoiceProviderError::ContextUnavailable)?;
        let current_thread = agent.extra.get("threadId").and_then(Value::as_str).unwrap_or_default();
        let current_account = agent.extra.get("accountKey").and_then(Value::as_str).unwrap_or("default");
        if agent.epoch != epoch || current_thread != thread_id
            || current_account != account_key {
            return Err(VoiceProviderError::ContextChanged);
        }
        crate::context_repair::assert_available(&agent.value())
            .map_err(|_| VoiceProviderError::ContextChanged)?;
        let old = load_row_any(transaction, &session_id)?
            .map(VoiceSession::from_row).transpose()?;
        let active_row = transaction.query_row(
            "SELECT id,agent,created,ended,sdp,answer,native_thread,account_key,connection_id,state,error \
             FROM voice_sessions WHERE agent=?1 AND state IS NOT NULL AND ended IS NULL \
             ORDER BY created DESC LIMIT 1",
            [&agent_id], session_from_row,
        ).optional()?;
        let active = active_row.map(VoiceSession::from_row).transpose()?;
        match policy::decide_start(Some(now), request, old.as_ref(), active.as_ref())? {
            decision @ StartDecision::Replay(_) => Ok(decision),
            StartDecision::Create { created, .. } => {
                let mut agent = agent;
                agent.extra.insert("autoWake".into(), Value::Bool(true));
                agent
                    .extra
                    .insert("turnEpoch".into(), Value::from(agent.epoch));
                AgentRepository::put(transaction, &agent)?;
                transaction.execute(
                    "INSERT INTO voice_sessions(id,agent,created,sdp,native_thread,account_key,connection_id,state) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,'connecting')",
                    rusqlite::params![session_id,agent_id,created,offer,(!thread_id.is_empty()).then_some(thread_id.as_str()),account_key,None::<String>],
                )?;
                let row = load_row(transaction, &session_id, &agent_id)?
                    .ok_or(policy::VoiceError::InvalidStoredSession)?;
                VoiceSession::from_row(row)?;
                Ok(StartDecision::Create {
                    agent: AgentId::from(agent_id.as_str()),
                    session_id: policy::SessionId::parse(session_id.clone())?,
                    offer: offer.clone(),
                    created,
                })
            }
        }
    }).await
}

async fn attach_start_context(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    context: &VoiceProviderContext,
) -> Result<(), VoiceProviderError> {
    let agent_id = agent_id.to_owned();
    let session_id = session_id.to_owned();
    let context = context.clone();
    store
        .transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
            let agent = super::checked_agent(transaction, &agent_id)
                .map_err(|_| VoiceProviderError::ContextUnavailable)?;
            let current_thread = agent
                .extra
                .get("threadId")
                .and_then(Value::as_str)
                .unwrap_or_default();
            let current_account = agent
                .extra
                .get("accountKey")
                .and_then(Value::as_str)
                .unwrap_or("default");
            if agent.epoch != context.epoch
                || current_thread != context.thread_id
                || current_account != context.account_key
            {
                return Err(VoiceProviderError::ContextChanged);
            }
            let scope =
                studio_runtime_store::account_state::current_provider_scope(transaction, &agent)
                    .map_err(|error| match error {
                        studio_runtime_store::account_state::ScopeError::Unavailable => {
                            VoiceProviderError::ContextUnavailable
                        }
                        studio_runtime_store::account_state::ScopeError::Store(error) => {
                            VoiceProviderError::Store(error)
                        }
                    })?;
            if scope.connection != context.connection_id || scope.generation != context.generation {
                return Err(VoiceProviderError::ContextChanged);
            }
            let scope = serde_json::to_string(&scope).map_err(|_| VoiceProviderError::Operation)?;
            let updated = transaction.execute(
                "UPDATE voice_sessions SET connection_id=?1,provider_scope=?6 \
             WHERE id=?2 AND agent=?3 AND native_thread=?4 AND account_key=?5 \
             AND state='connecting' AND ended IS NULL",
                rusqlite::params![
                    context.connection_id,
                    session_id,
                    agent_id,
                    context.thread_id,
                    context.account_key,
                    scope
                ],
            )?;
            if updated != 1 {
                return Err(VoiceProviderError::ContextChanged);
            }
            Ok(())
        })
        .await?;
    Ok(())
}

async fn set_start_failure(
    store: &RuntimeStore,
    session_id: &str,
    certainty: VoicePrepareFailure,
    now: f64,
) -> Result<(), VoiceProviderError> {
    let state = match certainty {
        VoicePrepareFailure::Expired | VoicePrepareFailure::NotSent => "failed",
        VoicePrepareFailure::Unknown => "unknown",
        VoicePrepareFailure::Rejected => "failed",
    };
    let detail = if certainty == VoicePrepareFailure::Expired {
        "Request id expired"
    } else {
        "Voice start outcome is unresolved"
    };
    let session_id = session_id.to_owned();
    store
        .transaction_with_error(move |transaction| -> Result<_, VoiceProviderError> {
            transaction.execute(
                "UPDATE voice_sessions SET state=?1,error=?2,ended=COALESCE(ended,?3) WHERE id=?4",
                rusqlite::params![state, detail, now, session_id],
            )?;
            Ok(())
        })
        .await?;
    Ok(())
}

async fn load_session(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
) -> Result<Value, VoiceProviderError> {
    let (agent_id, session_id) = (agent_id.to_owned(), session_id.to_owned());
    store
        .read_async("runtime-services/voice/provider", move |connection| {
            super::checked_agent(connection, &agent_id)
                .map_err(|_| VoiceProviderError::ContextUnavailable)?;
            let row = load_row(connection, &session_id, &agent_id)?
                .ok_or(policy::VoiceError::UnknownSession)?;
            Ok(session_value(&VoiceSession::from_row(row)?))
        })
        .await
}

fn session_value(session: &VoiceSession) -> Value {
    json!({
        "session_id": session.id().as_str(),
        "state": session.state().map(policy::VoiceSessionState::as_str),
        "sdp": session.answer(),
        "error": session.error(),
        "ended": session.ended(),
    })
}

fn load_row(
    connection: &Connection,
    session: &str,
    agent: &str,
) -> Result<Option<VoiceSessionRow>, VoiceProviderError> {
    Ok(connection.query_row(
        "SELECT id,agent,created,ended,sdp,answer,native_thread,account_key,connection_id,state,error \
         FROM voice_sessions WHERE id=?1 AND agent=?2",
        rusqlite::params![session,agent], session_from_row,
    ).optional()?)
}

fn load_row_any(
    connection: &Connection,
    session: &str,
) -> Result<Option<VoiceSessionRow>, VoiceProviderError> {
    Ok(connection.query_row(
        "SELECT id,agent,created,ended,sdp,answer,native_thread,account_key,connection_id,state,error \
         FROM voice_sessions WHERE id=?1",
        [session], session_from_row,
    ).optional()?)
}

fn session_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<VoiceSessionRow> {
    Ok(VoiceSessionRow {
        id: row.get(0)?,
        agent: row.get(1)?,
        created: row.get(2)?,
        ended: row.get(3)?,
        offer: row.get(4)?,
        answer: row.get(5)?,
        native_thread: row.get(6)?,
        account_key: row.get(7)?,
        connection_id: row.get(8)?,
        state: row.get(9)?,
        error: row.get(10)?,
    })
}

fn start_call(context: VoiceProviderContext, session_id: &str, offer: &str) -> VoiceRpcCall {
    VoiceRpcCall {
        operation_id: format!("voice:start:{session_id}"),
        root_request_id: session_id.to_owned(),
        admitted_request: None,
        context: context.clone(),
        method: Method::ThreadRealtimeStart,
        params: json!({
            "threadId": context.thread_id,
            "version": "v3",
            "outputModality": "audio",
            "realtimeSessionId": session_id,
            "clientManagedHandoffs": false,
            "flushTranscriptTailOnSessionEnd": false,
            "transport": {"type": "webrtc", "sdp": offer},
        }),
        deadline: request_deadline(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::runtime::VoiceFuture;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn voice_outbox_captures_deleted_owner_authority() {
        let (_directory, store) = voice_outbox_fixture().await;
        store
            .transaction_with_error(|tx| -> Result<(), VoiceProviderError> {
                stage_delete_sessions(tx, "agent")?;
                tx.execute("DELETE FROM voice_sessions WHERE agent='agent'", [])?;
                tx.execute("DELETE FROM runtime_agents WHERE id='agent'", [])?;
                Ok(())
            })
            .await
            .expect("stage deletion");
        let reader = store.test_reader().expect("reader");
        let sessions = deleted_voice_sessions(&reader, "agent", 2).expect("captured authority");
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].connection_id, "voice-connection");
        assert_eq!(sessions[0].generation, 7);
    }

    #[tokio::test]
    async fn start_failure_cannot_leave_a_connecting_ended_session() {
        let directory = tempfile::tempdir().expect("voice state");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store");
        store.transaction(|tx| {
            tx.execute("INSERT INTO voice_sessions(id,agent,created,state) VALUES('session','agent',1,'connecting')", [])?;
            Ok(())
        }).await.expect("session");
        set_start_failure(&store, "session", VoicePrepareFailure::NotSent, 2.0)
            .await
            .expect("failure");
        let reader = store.test_reader().expect("reader");
        let (state, ended): (String, Option<f64>) = reader
            .query_row(
                "SELECT state,ended FROM voice_sessions WHERE id='session'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("session");
        assert!(state != "connecting" || ended.is_none());
    }

    async fn voice_outbox_fixture() -> (tempfile::TempDir, Arc<RuntimeStore>) {
        let directory = tempfile::tempdir().expect("voice fixture");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store");
        store.transaction(|tx| {
            studio_runtime_store::receipt_schema::create(tx)?;
            let agent=json!({"provider":"codex","id":"agent","rootId":"agent","epoch":2,"status":"idle","isLead":true,"threadId":"thread","accountKey":"account"});
            tx.execute("INSERT INTO runtime_agents(id,record) VALUES('agent',?1)",[agent.to_string()])?;
            Ok(())
        }).await.expect("seed voice");
        crate::account_worker::store::attach_provider_stream_for_test(
            &store,
            "account",
            None,
            7,
            "voice-connection",
        )
        .await
        .expect("attach provider");
        store.transaction(|tx| {
            let agent = AgentRepository::get(tx, &AgentId::from("agent"))?.expect("agent");
            let scope = studio_runtime_store::account_state::current_provider_scope(tx, &agent).expect("attached scope");
            tx.execute("INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,provider_scope,state) VALUES('session','agent',1,'thread','account',?1,?2,'ready')", rusqlite::params![scope.connection,serde_json::to_string(&scope).expect("scope")])?;
            Ok(())
        }).await.expect("seed voice session");
        (directory, Arc::new(store))
    }

    struct GatedStop {
        calls: Mutex<Vec<VoiceRpcCall>>,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
        certainty: VoiceWriteCertainty,
    }
    impl GatedStop {
        fn new(certainty: VoiceWriteCertainty) -> Self {
            Self {
                calls: Mutex::new(vec![]),
                entered: tokio::sync::Notify::new(),
                release: tokio::sync::Notify::new(),
                certainty,
            }
        }
    }
    impl VoiceProviderPort for GatedStop {
        fn prepare<'a>(
            &'a self,
            _: &'a str,
            _: &'a str,
            _: &'a str,
            _: &'a str,
            _: i64,
        ) -> VoiceFuture<'a, Result<VoiceProviderContext, VoicePrepareFailure>> {
            Box::pin(async { Err(VoicePrepareFailure::NotSent) })
        }
        fn request<'a>(
            &'a self,
            scope: VoiceProviderContext,
            operation: super::super::runtime::VoiceOperation,
            method: Method,
            params: Value,
            deadline: Instant,
        ) -> VoiceFuture<'a, VoiceRequestReceipt> {
            Box::pin(async move {
                self.calls.lock().expect("calls").push(VoiceRpcCall {
                    operation_id: operation.id,
                    root_request_id: operation.root_request_id,
                    admitted_request: operation.admitted_request,
                    context: scope,
                    method,
                    params,
                    deadline,
                });
                self.entered.notify_one();
                self.release.notified().await;
                VoiceRequestReceipt {
                    certainty: self.certainty,
                }
            })
        }
        fn reply<'a>(
            &'a self,
            _: VoiceProviderContext,
            _: String,
            _: VoiceReply,
            _: Instant,
        ) -> VoiceFuture<'a, VoiceRequestReceipt> {
            Box::pin(async {
                VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::NotSent,
                }
            })
        }
    }

    #[tokio::test]
    async fn voice_outbox_capture_ignores_null_state_sessions() {
        let (_directory, store) = voice_outbox_fixture().await;
        store.transaction_with_error(|tx| -> Result<(), VoiceProviderError> {
            tx.execute("INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) VALUES('empty','agent',2,'thread','account','voice-connection',NULL)", [])?;
            let captured = capture_delete_sessions(tx, "agent")?;
            assert_eq!(captured.len(), 1);
            assert_eq!(captured[0].session_id, "session");
            Ok(())
        }).await.expect("capture");
    }
    #[tokio::test]
    async fn voice_outbox_late_not_sent_after_agent_removal_is_abandoned() {
        use studio_runtime_store::request_results::{
            AbandonedOutcome, AbandonedResult, ResultEnvelope,
        };
        let (_directory, store) = voice_outbox_fixture().await;
        let provider = Arc::new(GatedStop::new(VoiceWriteCertainty::NotSent));
        let task_store = store.clone();
        let task_provider = provider.clone();
        let running = tokio::spawn(async move {
            end(
                task_store,
                task_provider,
                "agent".into(),
                "session".into(),
                10.0,
            )
            .await
        });
        provider.entered.notified().await;
        let id = store
            .transaction_with_error(|tx| -> Result<String, VoiceProviderError> {
                let row = Outbox::for_owner::<studio_runtime_store::VoiceEffect>(
                    tx,
                    OwnerKind::Agent,
                    "agent",
                    OutboxKind::VoiceStop,
                )?
                .remove(0);
                assert!(stage_delete_sessions(tx, "agent")?.is_empty());
                tx.execute("DELETE FROM voice_sessions WHERE agent='agent'", [])?;
                tx.execute("DELETE FROM runtime_agents WHERE id='agent'", [])?;
                Ok(row.id)
            })
            .await
            .expect("delete owner");
        provider.release.notify_one();
        drop(running.await.expect("stop task"));
        let reader = store.test_reader().expect("reader");
        assert!(
            Outbox::get::<studio_runtime_store::VoiceEffect>(&reader, &id)
                .unwrap()
                .is_none()
        );
        let raw: String = reader
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [&id],
                |row| row.get(0),
            )
            .expect("abandoned result");
        let result: ResultEnvelope<AbandonedResult> =
            serde_json::from_str(&raw).expect("typed result");
        assert_eq!(
            result.reply,
            AbandonedResult::Abandoned {
                outcome: AbandonedOutcome::Unknown
            }
        );
        assert!(
            Outbox::for_owner::<studio_runtime_store::VoiceEffect>(
                &reader,
                OwnerKind::Agent,
                "agent",
                OutboxKind::VoiceDelete
            )
            .unwrap()
            .is_empty()
        );
        assert_eq!(provider.calls.lock().expect("calls").len(), 1);
    }
    #[tokio::test]
    async fn voice_outbox_late_stop_proof_settles_deleted_owner_atomically() {
        for certainty in [
            VoiceWriteCertainty::NotSent,
            VoiceWriteCertainty::Rejected,
            VoiceWriteCertainty::Written,
            VoiceWriteCertainty::Unknown,
            VoiceWriteCertainty::Expired,
        ] {
            let (_directory, store) = voice_outbox_fixture().await;
            let provider = Arc::new(GatedStop::new(certainty));
            let task_store = store.clone();
            let task_provider = provider.clone();
            let running = tokio::spawn(async move {
                end(
                    task_store,
                    task_provider,
                    "agent".into(),
                    "session".into(),
                    10.0,
                )
                .await
            });
            provider.entered.notified().await;
            store.transaction_with_error(|tx| -> Result<(), VoiceProviderError> {
                tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',3,'$.deletedAt',1) WHERE id='agent'", [])?;
                assert!(stage_delete_sessions(tx, "agent")?.is_empty());
                tx.execute("DELETE FROM voice_sessions WHERE agent='agent'", [])?;
                Ok(())
            }).await.expect("retain uncertain stop");
            provider.release.notify_one();
            drop(running.await.expect("stop task"));
            let reader = store.test_reader().expect("reader");
            let stops = Outbox::for_owner::<studio_runtime_store::VoiceEffect>(
                &reader,
                OwnerKind::Agent,
                "agent",
                OutboxKind::VoiceStop,
            )
            .expect("stops");
            let deletes = Outbox::for_owner::<studio_runtime_store::VoiceEffect>(
                &reader,
                OwnerKind::Agent,
                "agent",
                OutboxKind::VoiceDelete,
            )
            .expect("deletes");
            match certainty {
                VoiceWriteCertainty::NotSent | VoiceWriteCertainty::Rejected => {
                    assert!(stops.is_empty());
                    assert_eq!(deletes.len(), 1);
                    assert_eq!(deletes[0].status, OutboxStatus::Pending);
                    assert_eq!(deletes[0].payload.epoch, 3);
                }
                VoiceWriteCertainty::Written => {
                    assert!(stops.is_empty());
                    assert!(deletes.is_empty());
                }
                VoiceWriteCertainty::Unknown | VoiceWriteCertainty::Expired => {
                    assert_eq!(stops.len(), 1);
                    assert_eq!(stops[0].status, OutboxStatus::Unknown);
                    assert!(deletes.is_empty());
                }
            }
            assert_eq!(provider.calls.lock().expect("calls").len(), 1);
        }
    }
    #[tokio::test]
    async fn voice_outbox_unknown_accepts_only_exact_late_proof_after_expiry() {
        use crate::account_worker::retained::{ProviderCallIntent, RequestActor, RequestResult};
        let (_directory, store) = voice_outbox_fixture().await;
        let provider = Arc::new(GatedStop::new(VoiceWriteCertainty::Unknown));
        provider.release.notify_one();
        assert!(matches!(
            end(
                store.clone(),
                provider.clone(),
                "agent".into(),
                "session".into(),
                10.0
            )
            .await,
            Err(VoiceProviderError::Unknown)
        ));
        let call = provider.calls.lock().expect("calls")[0].clone();
        let before = Outbox::for_owner::<studio_runtime_store::VoiceEffect>(
            &store.test_reader().expect("reader"),
            OwnerKind::Agent,
            "agent",
            OutboxKind::VoiceStop,
        )
        .expect("owner")
        .remove(0);
        assert_eq!(before.status, OutboxStatus::Unknown);
        assert_eq!(before.next_at, None);
        assert!(
            pending_voice_intents(&store, OutboxKind::VoiceStop)
                .await
                .expect("pending")
                .is_empty()
        );
        assert_eq!(
            end(
                store.clone(),
                provider.clone(),
                "agent".into(),
                "session".into(),
                11.0
            )
            .await
            .expect("held retry")["state"],
            "unknown"
        );
        assert_eq!(provider.calls.lock().expect("calls").len(), 1);
        store.transaction_with_error(|tx| -> Result<(), VoiceProviderError> {
            tx.execute("UPDATE runtime_agents SET record=json_set(record,'$.epoch',3,'$.deletedAt',1) WHERE id='agent'", [])?;
            stage_delete_sessions(tx, "agent")?;
            tx.execute("DELETE FROM voice_sessions WHERE agent='agent'", [])?;
            tx.execute("INSERT INTO runtime_orchestration_settings(id,record) VALUES('global',?1) ON CONFLICT(id) DO UPDATE SET record=json_set(record,'$.expired_before_ms',?2)",rusqlite::params![json!({"expired_before_ms":i64::MAX}).to_string(),i64::MAX])?;
            Ok(())
        }).await.expect("delete and expire");
        let captured = ProviderCallIntent {
            root_request_id: String::new(),
            admitted_request: call.admitted_request,
            scope: crate::account_session_runtime::ConnectionScope {
                account: call.context.account_key,
                connection: call.context.connection_id,
                generation: call.context.generation,
            },
            operation: call.operation_id,
            actor: Some(RequestActor {
                agent: call.context.agent_id,
                epoch: call.context.epoch,
            }),
            transfer: None,
            deleted_actor_session: None,
            method: call.method,
            params: call.params,
        };
        let mut wrong = captured.clone();
        wrong.scope.generation += 1;
        store
            .transaction_with_error(move |tx| {
                settle_voice_stop(tx, &wrong, &RequestResult::NotSent)
            })
            .await
            .expect("wrong proof ignored");
        assert!(
            pending_delete_intents(&store)
                .await
                .expect("held")
                .is_empty()
        );
        store
            .transaction_with_error(move |tx| {
                settle_voice_stop(tx, &captured, &RequestResult::NotSent)
            })
            .await
            .expect("exact proof");
        let deletes = pending_delete_intents(&store).await.expect("safe transfer");
        assert_eq!(deletes.len(), 1);
        assert_eq!(provider.calls.lock().expect("calls").len(), 1);
    }
    #[tokio::test]
    async fn voice_outbox_expiry_refuses_retry_without_a_fresh_root() {
        let (_directory, store) = voice_outbox_fixture().await;
        let provider = Arc::new(GatedStop::new(VoiceWriteCertainty::Expired));
        provider.release.notify_one();
        assert!(matches!(
            end(
                store.clone(),
                provider.clone(),
                "agent".into(),
                "session".into(),
                10.0
            )
            .await,
            Err(VoiceProviderError::Expired)
        ));
        let before = Outbox::for_owner::<studio_runtime_store::VoiceEffect>(
            &store.test_reader().expect("reader"),
            OwnerKind::Agent,
            "agent",
            OutboxKind::VoiceStop,
        )
        .expect("owner")
        .remove(0);
        assert_eq!(before.status, OutboxStatus::Unknown);
        assert!(matches!(
            before.payload.authority,
            studio_runtime_store::VoiceEffectAuthority::Admitted {
                outcome: VoiceStopOutcome::Expired,
                ..
            }
        ));
        assert!(matches!(
            end(
                store.clone(),
                provider.clone(),
                "agent".into(),
                "session".into(),
                11.0
            )
            .await,
            Err(VoiceProviderError::Expired)
        ));
        assert_eq!(provider.calls.lock().expect("calls").len(), 1);
        let after = Outbox::get::<studio_runtime_store::VoiceEffect>(
            &store.test_reader().expect("reader"),
            &before.id,
        )
        .expect("same row");
        assert_eq!(after, Some(before));
    }
    #[tokio::test]
    async fn voice_outbox_claim_survives_crash_and_never_sends_twice() {
        let (_directory, store) = voice_outbox_fixture().await;
        let provider = Arc::new(GatedStop::new(VoiceWriteCertainty::Unknown));
        let task_store = store.clone();
        let task_provider = provider.clone();
        let running = tokio::spawn(async move {
            end(
                task_store,
                task_provider,
                "agent".into(),
                "session".into(),
                10.0,
            )
            .await
        });
        provider.entered.notified().await;
        running.abort();
        drop(running.await);
        let row = Outbox::for_owner::<studio_runtime_store::VoiceEffect>(
            &store.test_reader().expect("reader"),
            OwnerKind::Agent,
            "agent",
            OutboxKind::VoiceStop,
        )
        .expect("owner")
        .remove(0);
        assert_eq!(row.status, OutboxStatus::Unknown);
        assert_eq!(row.next_at, None);
        assert!(matches!(
            row.payload.authority,
            studio_runtime_store::VoiceEffectAuthority::Admitted {
                outcome: VoiceStopOutcome::Submitted,
                ..
            }
        ));
        end(
            store.clone(),
            provider.clone(),
            "agent".into(),
            "session".into(),
            11.0,
        )
        .await
        .expect("held retry");
        assert!(
            pending_voice_intents(&store, OutboxKind::VoiceStop)
                .await
                .expect("pending")
                .is_empty()
        );
        assert_eq!(provider.calls.lock().expect("calls").len(), 1);
    }
    #[test]
    fn voice_outbox_pending_and_owner_queries_use_typed_indexes() {
        let db = Connection::open_in_memory().expect("database");
        studio_runtime_store::receipt_schema::create(&db).expect("schema");
        for (sql, expected) in [
            (
                "EXPLAIN QUERY PLAN SELECT id FROM runtime_outbox WHERE kind='voice_delete' AND status='pending' AND next_at<=10 ORDER BY next_at,id LIMIT 500",
                "runtime_outbox_pending",
            ),
            (
                "EXPLAIN QUERY PLAN SELECT id FROM runtime_outbox WHERE owner_kind='agent' AND owner_id='agent' AND kind='voice_stop' ORDER BY id",
                "runtime_outbox_owner",
            ),
        ] {
            let details = db
                .prepare(sql)
                .expect("query plan")
                .query_map([], |row| row.get::<_, String>(3))
                .expect("plan rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("plan");
            assert!(
                details.iter().any(|detail| detail.contains(expected)),
                "{details:?}"
            );
            assert!(
                details
                    .iter()
                    .all(|detail| !detail.contains("request_results")
                        && !detail.contains("runtime_operation_receipts"))
            );
        }
    }

    struct ScriptedProvider {
        calls: Mutex<Vec<VoiceRpcCall>>,
    }

    impl VoiceProviderPort for ScriptedProvider {
        fn prepare<'a>(
            &'a self,
            agent_id: &'a str,
            _session_id: &'a str,
            account_key: &'a str,
            thread_id: &'a str,
            epoch: i64,
        ) -> VoiceFuture<'a, Result<VoiceProviderContext, VoicePrepareFailure>> {
            Box::pin(async move {
                Ok(VoiceProviderContext {
                    agent_id: agent_id.to_owned(),
                    epoch,
                    account_key: account_key.to_owned(),
                    thread_id: thread_id.to_owned(),
                    connection_id: "scripted-connection".to_owned(),
                    generation: 1,
                })
            })
        }

        fn request<'a>(
            &'a self,
            scope: VoiceProviderContext,
            operation: super::super::runtime::VoiceOperation,
            method: Method,
            params: Value,
            deadline: Instant,
        ) -> VoiceFuture<'a, VoiceRequestReceipt> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("scripted calls lock")
                    .push(VoiceRpcCall {
                        context: scope,
                        operation_id: operation.id,
                        root_request_id: operation.root_request_id,
                        admitted_request: operation.admitted_request,
                        method,
                        params,
                        deadline,
                    });
                VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::Written,
                }
            })
        }

        fn reply<'a>(
            &'a self,
            _scope: VoiceProviderContext,
            operation_id: String,
            _reply: VoiceReply,
            _deadline: Instant,
        ) -> VoiceFuture<'a, VoiceRequestReceipt> {
            Box::pin(async move {
                self.calls
                    .lock()
                    .expect("scripted calls lock")
                    .push(VoiceRpcCall {
                        context: VoiceProviderContext {
                            agent_id: String::new(),
                            epoch: 0,
                            account_key: String::new(),
                            thread_id: String::new(),
                            connection_id: String::new(),
                            generation: 0,
                        },
                        operation_id: operation_id.clone(),
                        root_request_id: operation_id,
                        admitted_request: None,
                        method: Method::ItemCommandExecutionRequestApproval,
                        params: Value::Null,
                        deadline: Instant::now(),
                    });
                VoiceRequestReceipt {
                    certainty: VoiceWriteCertainty::Written,
                }
            })
        }
    }

    #[tokio::test]
    async fn scripted_provider_receives_native_webrtc_start_contract() {
        let provider = Arc::new(ScriptedProvider {
            calls: Mutex::new(vec![]),
        });
        let context = VoiceProviderContext {
            agent_id: "agent".into(),
            epoch: 4,
            account_key: "account".into(),
            thread_id: "thread".into(),
            connection_id: "connection".into(),
            generation: 1,
        };
        let call = start_call(context, "session", "v=0\r\n...");
        let receipt = request(provider.as_ref(), call.clone()).await;
        assert_eq!(receipt.certainty, VoiceWriteCertainty::Written);
        let calls = provider.calls.lock().expect("scripted calls lock");
        assert_eq!(calls.as_slice(), &[call]);
        assert_eq!(calls[0].method, Method::ThreadRealtimeStart);
        assert_eq!(calls[0].params["transport"]["type"], "webrtc");
        assert_eq!(calls[0].params["transport"]["sdp"], "v=0\r\n...");
        assert_eq!(calls[0].params["clientManagedHandoffs"], false);
    }

    #[tokio::test]
    async fn restart_loses_orphaned_start_before_replay_or_new_admission() {
        use studio_runtime_store::{AgentRecord, AgentStatus};

        let directory = tempfile::tempdir().expect("voice state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("fresh voice store");
        let mut extra = std::collections::BTreeMap::new();
        extra.insert(
            "provider".into(),
            json!(studio_runtime_schema::ProviderKind::Codex),
        );
        extra.insert("accountKey".into(), json!("default"));
        extra.insert("threadId".into(), json!("voice-thread"));
        extra.insert("isLead".into(), json!(true));
        let agent = AgentRecord {
            title_job: None,
            start_outcome_hold: None,
            fired_events: Default::default(),
            id: AgentId::from("voice-agent"),
            root_id: AgentId::from("voice-agent"),
            epoch: 0,
            status: AgentStatus::Idle,
            extra,
        };
        let old_offer = "v=0\r\nold-offer".to_owned();
        let transaction_offer = old_offer.clone();
        store
            .transaction(move |transaction| {
                AgentRepository::put(transaction, &agent)?;
                transaction.execute(
                    "INSERT INTO voice_sessions(id,agent,created,sdp,native_thread,account_key,connection_id,state) \
                     VALUES('orphan-start','voice-agent',10,?1,'voice-thread','default',NULL,'connecting')",
                    [transaction_offer],
                )?;
                Ok(())
                })
                .await
            .expect("persist accepted start before simulated crash");
        drop(store);

        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("reopened voice store"),
        );
        assert_eq!(
            recover_after_restart(&store, 20.0)
                .await
                .expect("settle persisted peer"),
            1,
        );
        let persisted = VoiceRepository::session(
            &store.test_reader().expect("read reopened store"),
            "orphan-start",
            "voice-agent",
        )
        .expect("read recovered session")
        .expect("recovered session remains addressable");
        assert_eq!(persisted["state"], "lost");
        assert_eq!(persisted["ended"], 20.0);
        assert_eq!(persisted["error"], RESTARTED_SESSION_ERROR);

        let provider = Arc::new(ScriptedProvider {
            calls: Mutex::new(vec![]),
        });
        crate::account_worker::store::attach_provider_stream_for_test(
            &store,
            "default",
            None,
            1,
            "scripted-connection",
        )
        .await
        .expect("attach provider");
        let replay = start_admitted(
            Arc::clone(&store),
            Some(provider.clone()),
            "voice-agent".into(),
            "orphan-start".into(),
            old_offer,
            21.0,
        )
        .await
        .expect("exact replay returns saved terminal session");
        assert_eq!(replay["state"], "lost");
        assert!(provider.calls.lock().expect("provider calls").is_empty());

        let fresh = start_admitted(
            Arc::clone(&store),
            Some(provider.clone()),
            "voice-agent".into(),
            "fresh-session".into(),
            "v=0\r\nfresh-offer".into(),
            22.0,
        )
        .await
        .expect("new identity is admitted after recovered session is terminal");
        assert_eq!(fresh["state"], "connecting");
        wait_for_start_setup("voice-agent", "fresh-session").await;
        let calls = provider.calls.lock().expect("provider calls");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].method, Method::ThreadRealtimeStart);
        assert_eq!(calls[0].params["realtimeSessionId"], "fresh-session");
    }

    #[tokio::test]
    async fn dispatch_guard_serializes_same_session_but_not_other_sessions() {
        let held = session_dispatch_guard("agent", "session-a").await;
        let same = session_dispatch_guard("agent", "session-a");
        tokio::pin!(same);
        assert!(
            futures_util::poll!(&mut same).is_pending(),
            "same-session work must wait"
        );
        let other = session_dispatch_guard("agent", "session-b").await;
        drop(other);
        drop(held);
        tokio::time::timeout(std::time::Duration::from_secs(5), same)
            .await
            .expect("same-session guard releases");
    }

    #[test]
    fn completed_delete_stops_are_not_requeued_but_not_sent_stops_are() {
        let sessions = ["written", "unknown", "rejected", "not-sent", "pending"];
        let delivered = serde_json::from_value::<HashMap<String, VoiceStopOutcome>>(json!({
            "written": "written",
            "unknown": "unknown",
            "rejected": "rejected",
            "not-sent": "not_sent",
        }))
        .expect("valid delivery result map");

        let pending: Vec<_> = sessions
            .iter()
            .filter(|session_id| {
                matches!(
                    delivered.get(**session_id),
                    None | Some(VoiceStopOutcome::NotSent)
                )
            })
            .collect();
        assert_eq!(
            pending.into_iter().copied().collect::<Vec<_>>(),
            ["not-sent", "pending"]
        );
    }

    #[test]
    fn speech_receipt_identity_is_tuple_unambiguous_and_replay_checks_record() {
        assert_eq!(
            speech_receipt_id("session:one", "record"),
            "session:one:record"
        );
        let connection = Connection::open_in_memory().expect("receipt database");
        connection
            .execute(
                "CREATE TABLE voice_speech_requests(id TEXT PRIMARY KEY,session TEXT,record_id TEXT,state TEXT,error TEXT)",
                [],
            )
            .expect("receipt table");
        connection
            .execute(
                "INSERT INTO voice_speech_requests VALUES('session:one:record','session:one','record','unknown','private')",
                [],
            )
            .expect("legacy receipt");
        connection
            .execute(
                "INSERT INTO voice_speech_requests VALUES('new-id','session','different-record','unknown','private')",
                [],
            )
            .expect("canonical receipt with wrong record");
        assert!(
            lookup_speech_receipt(&connection, "session:one", "record", "fresh-id")
                .expect("exact legacy tuple is replayable")
                .is_some()
        );
        assert!(lookup_speech_receipt(&connection, "session", "one:record", "new-id").is_err());
    }
}
