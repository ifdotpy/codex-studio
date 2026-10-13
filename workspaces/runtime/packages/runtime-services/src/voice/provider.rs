//! Voice-session RPC port and durable request adapter.

use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Mutex;

use rusqlite::OptionalExtension;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use studio_runtime_providers::Method;
use studio_runtime_store::{AgentId, Connection, RuntimeStore, VoiceRepository};
use thiserror::Error;

use super::policy::{
    self, SessionSignal, StartDecision, StartRequest, VoiceSession, VoiceSessionRow,
};

pub type ProviderFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
const MAX_TRACKED_VOICE_SESSIONS: usize = 1024;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceProviderContext {
    pub agent_id: String,
    pub epoch: i64,
    pub account_key: String,
    pub thread_id: String,
    pub connection_id: String,
}

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceRpcCall {
    pub operation_id: String,
    pub context: VoiceProviderContext,
    pub method: Method,
    pub params: Value,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum VoiceWriteCertainty {
    NotSent,
    Unknown,
    Rejected(String),
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceProviderFailure {
    pub certainty: VoiceWriteCertainty,
    pub message: String,
}

/// Implemented by the account-session adapter. `prepare` must verify ChatGPT
/// auth and prepare an idle thread; `request` submits through the generic
/// provider session and reports admission certainty, never SDP connectivity.
pub trait VoiceProviderPort: Send + Sync {
    fn prepare<'a>(
        &'a self,
        agent_id: &'a str,
        account_key: &'a str,
        thread_id: &'a str,
        epoch: i64,
    ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoiceProviderFailure>>;

    fn request<'a>(
        &'a self,
        call: VoiceRpcCall,
    ) -> ProviderFuture<'a, Result<(), VoiceProviderFailure>>;
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
    pub fn handle_notification(
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
        let session_id = active_session_id(store, account_key, connection_id, thread_id)?;
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
                if let Some(role) = params.get("role").and_then(Value::as_str) {
                    if let Some(state) = sessions.get_mut(&session_id) {
                        state.tail.remove(role);
                    }
                }
            }
            "thread/realtime/closed" => self.flush_session(store, &session_id)?,
            _ => {}
        }
        handle_notification(store, account_key, connection_id, message, now)
    }

    pub fn handle_disconnect(
        &self,
        store: &RuntimeStore,
        account_key: &str,
        connection_id: &str,
        now: f64,
    ) -> Result<usize, VoiceProviderError> {
        let ids = {
            let connection = store.read()?;
            let mut statement = connection.prepare(
                "SELECT id FROM voice_sessions WHERE account_key=?1 AND connection_id=?2 AND ended IS NULL",
            )?;
            statement
                .query_map(rusqlite::params![account_key, connection_id], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for id in ids {
            self.flush_session(store, &id)?;
        }
        handle_disconnect(store, account_key, connection_id, now)
    }

    fn flush_session(
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
        let session = {
            let connection = store.read()?;
            load_row_any(&connection, session_id)?
                .map(VoiceSession::from_row)
                .transpose()?
        };
        let Some(session) = session else {
            return Ok(());
        };
        for (role, text) in state.tail {
            if text.is_empty() {
                continue;
            }
            let digest = Sha256::digest(text.as_bytes())
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
            record_native_transcript(
                store,
                &session,
                &format!("native:{}:tail:{role}:{digest}", session.id().as_str()),
                &role,
                &text,
                "",
                true,
            )?;
        }
        Ok(())
    }
}

fn active_session_id(
    store: &RuntimeStore,
    account_key: &str,
    connection_id: &str,
    thread_id: &str,
) -> Result<Option<String>, VoiceProviderError> {
    let connection = store.read()?;
    Ok(connection.query_row(
        "SELECT id FROM voice_sessions WHERE native_thread=?1 AND account_key=?2 AND connection_id=?3 AND ended IS NULL ORDER BY created DESC LIMIT 1",
        rusqlite::params![thread_id, account_key, connection_id],
        |row| row.get(0),
    ).optional()?)
}

#[derive(Debug, Error)]
pub enum VoiceProviderError {
    #[error(transparent)]
    Policy(#[from] policy::VoiceError),
    #[error(transparent)]
    Voice(#[from] super::VoiceError),
    #[error(transparent)]
    Store(#[from] studio_runtime_store::StoreError),
    #[error("voice provider context is unavailable")]
    ContextUnavailable,
    #[error("voice provider context changed")]
    ContextChanged,
    #[error("voice provider request was not sent")]
    NotSent,
    #[error("voice provider request outcome is unknown")]
    Unknown,
    #[error("voice provider rejected the request")]
    Rejected(String),
    #[error("voice provider adapter is unavailable")]
    AdapterUnavailable,
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
    let store_for_write = store.clone();
    let write_agent = agent_id.clone();
    let write_session = session_id.clone();
    let (value, context) = tokio::task::spawn_blocking(move || {
        prepare_end(&store_for_write, &write_agent, &write_session, now)
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    let Some(context) = context else {
        return Ok(value);
    };
    let call = VoiceRpcCall {
        operation_id: format!("voice:stop:{session_id}"),
        context: context.clone(),
        method: Method::ThreadRealtimeStop,
        params: json!({"threadId": context.thread_id}),
    };
    if let Err(error) = provider.request(call).await {
        let store_for_update = store.clone();
        let id = session_id.clone();
        let failure = error.clone();
        tokio::task::spawn_blocking(move || {
            update_stop_failure(&store_for_update, &id, &failure, now)
        })
        .await
        .map_err(|_| VoiceProviderError::Operation)??;
        return Err(provider_error(error));
    }
    load_session(&store, &agent_id, &session_id)
}

/// Read-only replay path for terminal sessions when the worker adapter is not
/// attached. Active sessions still require the worker to send realtime stop.
pub fn read_terminal_session(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<Value>, VoiceProviderError> {
    let connection = store.read()?;
    super::checked_agent(&connection, agent_id)
        .map_err(|_| VoiceProviderError::ContextUnavailable)?;
    let Some(row) = load_row(&connection, session_id, agent_id)? else {
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
}

/// Replay an existing speech receipt without dispatching a new provider RPC.
pub fn read_speech_receipt(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    record_id: &str,
) -> Result<Option<Value>, VoiceProviderError> {
    super::checked_agent(&store.read()?, agent_id)
        .map_err(|_| VoiceProviderError::ContextUnavailable)?;
    let request_id = format!("{session_id}:{record_id}");
    Ok(VoiceRepository::speech_receipt(
        &store.read()?,
        &request_id,
    )?)
}

fn prepare_end(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    now: f64,
) -> Result<(Value, Option<VoiceProviderContext>), VoiceProviderError> {
    let id = policy::SessionId::parse(session_id.to_owned())?;
    if !now.is_finite() || now < 0.0 {
        return Err(policy::VoiceError::InvalidClock.into());
    }
    store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
        let agent = super::checked_agent(transaction, agent_id)
            .map_err(|_| VoiceProviderError::ContextUnavailable)?;
        let Some(row) = load_row(transaction, id.as_str(), agent_id)? else {
            transaction.execute(
                "INSERT INTO voice_sessions(id,agent,created,ended,state) VALUES(?1,?2,?3,?3,'ended')",
                rusqlite::params![id.as_str(),agent_id,now],
            )?;
            return Ok((json!({"session_id":id.as_str(),"state":"ended","sdp":null,"error":null,"ended":now}), None));
        };
        let session = VoiceSession::from_row(row)?;
        if session.ended().is_some()
            || matches!(session.state(), Some(policy::VoiceSessionState::Ended | policy::VoiceSessionState::Stopping))
        {
            return Ok((session_value(&session), None));
        }
        let (transition, context) = match (
            session.native_thread(), session.account_key(), session.connection_id(),
        ) {
            (Some(thread), Some(account), Some(connection)) => {
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
                    Some(VoiceProviderContext {
                        agent_id: agent_id.to_owned(),
                        epoch: agent.epoch,
                        account_key: account.to_owned(),
                        thread_id: thread.to_owned(),
                        connection_id: connection.to_owned(),
                    }),
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
        Ok((session_value(&transition.session), context))
    }).map_err(VoiceProviderError::from)
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
        .unwrap_or_else(|| format!("{session_id}:{record_id}"));
    if receipt_id.chars().count() > 480 {
        return Err(VoiceProviderError::Policy(
            policy::VoiceError::InvalidSessionIdentity,
        ));
    }
    let store_for_reserve = store.clone();
    let reserve_agent = agent_id.clone();
    let reserve_session = session_id.clone();
    let reserve_record = record_id.clone();
    let reserve_id = receipt_id.clone();
    let reservation = tokio::task::spawn_blocking(move || {
        reserve_speech(
            &store_for_reserve,
            &reserve_agent,
            &reserve_session,
            &reserve_record,
            &reserve_id,
        )
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    let Some((context, text)) = reservation else {
        let connection = store.read()?;
        return VoiceRepository::speech_receipt(&connection, &receipt_id)?
            .ok_or(VoiceProviderError::Operation);
    };
    let thread_id = context.thread_id.clone();
    let call = VoiceRpcCall {
        operation_id: format!("voice:speech:{receipt_id}"),
        context,
        method: Method::ThreadRealtimeAppendSpeech,
        params: json!({"threadId": thread_id, "text": text}),
    };
    let outcome = provider.request(call).await;
    let state = match &outcome {
        Ok(()) => ("submitted", None),
        Err(error)
            if matches!(
                error.certainty,
                VoiceWriteCertainty::NotSent | VoiceWriteCertainty::Rejected(_)
            ) =>
        {
            ("failed", Some(error.message.clone()))
        }
        Err(error) => ("unknown", Some(error.message.clone())),
    };
    let store_for_update = store.clone();
    let id = receipt_id.clone();
    let state_name = state.0.to_owned();
    let state_error = state.1.clone();
    tokio::task::spawn_blocking(move || {
        update_speech(&store_for_update, &id, &state_name, state_error.as_deref())
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    if let Err(error) = outcome {
        return Err(provider_error(error));
    }
    let connection = store.read()?;
    VoiceRepository::speech_receipt(&connection, &receipt_id)?.ok_or(VoiceProviderError::Operation)
}

fn reserve_speech(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    record_id: &str,
    request_id: &str,
) -> Result<Option<(VoiceProviderContext, String)>, VoiceProviderError> {
    store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
        let agent = super::checked_agent(transaction, agent_id)
            .map_err(|_| VoiceProviderError::ContextUnavailable)?;
        let session = load_row(transaction, session_id, agent_id)?
            .ok_or(policy::VoiceError::UnknownSession)?;
        let session = VoiceSession::from_row(session)?;
        if let Some(old) = VoiceRepository::speech_receipt(transaction, request_id)? {
            if old["session"] != session_id || old["record_id"] != record_id {
                return Err(VoiceProviderError::Policy(policy::VoiceError::SessionIdentityConflict));
            }
            return Ok(None);
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
        let context = VoiceProviderContext {
            agent_id: agent_id.to_owned(), epoch: agent.epoch,
            account_key: account_key.to_owned(),
            thread_id: thread_id.to_owned(),
            connection_id: session.connection_id().ok_or(VoiceProviderError::ContextUnavailable)?.to_owned(),
        };
        let record = VoiceRepository::record(transaction, record_id)?
            .ok_or(policy::VoiceError::UnknownSession)?;
        super::validate_record(&record)?;
        if record.agent != agent_id || record.kind != "orchestrator" {
            return Err(policy::VoiceError::UnknownSession.into());
        }
        transaction.execute(
            "INSERT INTO voice_speech_requests(id,session,record_id,state,error) VALUES(?1,?2,?3,'pending',NULL)",
            rusqlite::params![request_id,session_id,record_id],
        )?;
        Ok(Some((context, record.text)))
    }).map_err(VoiceProviderError::from)
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

fn update_stop_failure(
    store: &RuntimeStore,
    session_id: &str,
    failure: &VoiceProviderFailure,
    now: f64,
) -> Result<(), VoiceProviderError> {
    let (state, ended) = match failure.certainty {
        VoiceWriteCertainty::NotSent => ("ended", Some(now)),
        VoiceWriteCertainty::Unknown | VoiceWriteCertainty::Rejected(_) => ("unknown", None),
    };
    store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
        transaction.execute(
            "UPDATE voice_sessions SET state=?1,error=?2,ended=COALESCE(ended,?3) WHERE id=?4",
            rusqlite::params![state, failure.message, ended, session_id],
        )?;
        Ok(())
    })?;
    Ok(())
}

fn update_speech(
    store: &RuntimeStore,
    request_id: &str,
    state: &str,
    error: Option<&str>,
) -> Result<(), VoiceProviderError> {
    store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
        transaction.execute(
            "UPDATE voice_speech_requests SET state=?1,error=?2 WHERE id=?3",
            rusqlite::params![state, error, request_id],
        )?;
        Ok(())
    })?;
    Ok(())
}

pub fn handle_notification(
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
    let session = {
        let connection = store.read()?;
        connection.query_row(
            "SELECT id,agent,created,ended,sdp,answer,native_thread,account_key,connection_id,state,error \
             FROM voice_sessions WHERE native_thread=?1 AND account_key=?2 AND connection_id=?3 \
             AND ended IS NULL ORDER BY created DESC LIMIT 1",
            rusqlite::params![thread_id,account_key,connection_id], session_from_row,
        ).optional()?.map(VoiceSession::from_row).transpose()?
    };
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
            )?;
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
            let digest = digest
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>();
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
            )?;
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
            store.transaction_with_error(|transaction| {
                update_session(transaction, &transition.session)
            })?;
        }
        "thread/realtime/error" => {
            let reason = params
                .get("message")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let transition = policy::transition_session(
                &session_value,
                SessionSignal::ProviderFailed(reason),
                now,
            )?;
            store.transaction_with_error(|transaction| {
                update_session(transaction, &transition.session)
            })?;
        }
        "thread/realtime/closed" => {
            let reason = params
                .get("reason")
                .and_then(Value::as_str)
                .map(str::to_owned);
            let transition =
                policy::transition_session(&session_value, SessionSignal::PeerClosed(reason), now)?;
            store.transaction_with_error(|transaction| {
                update_session(transaction, &transition.session)
            })?;
        }
        _ => {}
    }
    Ok(true)
}

/// Persist provider-session disconnects for every active voice session owned
/// by that account connection. The account worker calls this from its session
/// disconnect callback before discarding the generic RPC connection.
pub fn handle_disconnect(
    store: &RuntimeStore,
    account_key: &str,
    connection_id: &str,
    now: f64,
) -> Result<usize, VoiceProviderError> {
    let connection = store.read()?;
    let mut statement = connection.prepare(
        "SELECT id,agent,created,ended,sdp,answer,native_thread,account_key,connection_id,state,error \
         FROM voice_sessions WHERE account_key=?1 AND connection_id=?2 AND ended IS NULL",
    )?;
    let rows = statement.query_map(
        rusqlite::params![account_key, connection_id],
        session_from_row,
    )?;
    let sessions = rows
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .map(VoiceSession::from_row)
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    drop(connection);
    let mut count = 0;
    for session in sessions {
        let transition =
            policy::transition_session(&session, SessionSignal::ProviderDisconnected, now)?;
        store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
            update_session(transaction, &transition.session)?;
            Ok(())
        })?;
        count += 1;
    }
    Ok(count)
}

/// Realtime errors require an explicit stop RPC after the actor persists the
/// failure, matching the Python callback's stop behavior.
pub fn notification_requires_stop(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str) == Some("thread/realtime/error")
}

/// Return a stop RPC only for an error correlated to the active session. This
/// prevents a delayed error for an older realtime session from stopping its
/// replacement on the same thread.
pub fn stop_call_for_notification(
    store: &RuntimeStore,
    account_key: &str,
    connection_id: &str,
    message: &Value,
) -> Result<Option<VoiceRpcCall>, VoiceProviderError> {
    if !notification_requires_stop(message) {
        return Ok(None);
    }
    let params = message
        .get("params")
        .filter(|value| value.is_object())
        .ok_or(VoiceProviderError::Operation)?;
    let thread_id = params
        .get("threadId")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let Some(session_id) = active_session_id(store, account_key, connection_id, thread_id)? else {
        return Ok(None);
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
    if event_session.is_some_and(|id| id != session_id) {
        return Ok(None);
    }
    let connection = store.read()?;
    let Some(row) = load_row_any(&connection, &session_id)? else {
        return Ok(None);
    };
    let session = VoiceSession::from_row(row)?;
    let agent = super::checked_agent(&connection, session.agent().0.as_str())
        .map_err(|_| VoiceProviderError::ContextUnavailable)?;
    let thread_id = session
        .native_thread()
        .ok_or(VoiceProviderError::ContextUnavailable)?
        .to_owned();
    if session.ended().is_some()
        || thread_id
            != params
                .get("threadId")
                .and_then(Value::as_str)
                .unwrap_or_default()
        || session.account_key() != Some(account_key)
        || session.connection_id() != Some(connection_id)
    {
        return Ok(None);
    }
    Ok(Some(VoiceRpcCall {
        operation_id: format!("voice:error-stop:{session_id}"),
        context: VoiceProviderContext {
            agent_id: agent.id.0,
            epoch: agent.epoch,
            account_key: account_key.to_owned(),
            thread_id: thread_id.clone(),
            connection_id: connection_id.to_owned(),
        },
        method: Method::ThreadRealtimeStop,
        params: json!({"threadId": thread_id}),
    }))
}

fn record_native_transcript(
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
    )?;
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
    let request = StartRequest::new(
        AgentId::from(agent_id.as_str()),
        session_id.clone(),
        offer.clone(),
    )?;
    let store_for_read = store.clone();
    let agent_for_read = agent_id.clone();
    let session_for_read = session_id.clone();
    let existing = tokio::task::spawn_blocking(move || {
        read_start_rows(&store_for_read, &agent_for_read, &session_for_read, request)
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    if let StartDecision::Replay(session) = existing {
        return Ok(session_value(&session));
    }

    let (account_key, thread_id, epoch) = agent_context(&store, &agent_id)?;
    let context = provider
        .prepare(&agent_id, &account_key, &thread_id, epoch)
        .await
        .map_err(provider_error)?;
    if context.agent_id != agent_id
        || context.account_key != account_key
        || context.thread_id != thread_id
        || context.epoch != epoch
        || context.connection_id.is_empty()
    {
        return Err(VoiceProviderError::ContextChanged);
    }

    let store_for_write = store.clone();
    let write_agent = agent_id.clone();
    let write_session = session_id.clone();
    let write_offer = offer.clone();
    let write_context = context.clone();
    let saved = tokio::task::spawn_blocking(move || {
        create_session(
            &store_for_write,
            &write_agent,
            &write_session,
            &write_offer,
            &write_context,
            now,
        )
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    if let StartDecision::Replay(session) = saved {
        return Ok(session_value(&session));
    }

    let call = start_call(context, &session_id, &offer);
    if let Err(error) = provider.request(call).await {
        let error_for_store = error.clone();
        let store_for_error = store.clone();
        let id = session_id.clone();
        tokio::task::spawn_blocking(move || {
            set_start_failure(&store_for_error, &id, &error_for_store, now)
        })
        .await
        .map_err(|_| VoiceProviderError::Operation)??;
        return Err(provider_error(error));
    }
    load_session(&store, &agent_id, &session_id)
}

fn agent_context(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<(String, String, i64), VoiceProviderError> {
    let connection = store.read()?;
    let agent = super::checked_agent(&connection, agent_id)
        .map_err(|_| VoiceProviderError::ContextUnavailable)?;
    let thread_id = agent
        .extra
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|thread| !thread.is_empty())
        .ok_or(VoiceProviderError::ContextUnavailable)?;
    let account_key = agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .filter(|account| !account.is_empty())
        .unwrap_or("default");
    Ok((account_key.to_owned(), thread_id.to_owned(), agent.epoch))
}

fn read_start_rows(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    request: StartRequest,
) -> Result<StartDecision, VoiceProviderError> {
    let connection = store.read()?;
    super::checked_agent(&connection, agent_id)
        .map_err(|_| VoiceProviderError::ContextUnavailable)?;
    let old = load_row_any(&connection, session_id)?
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

fn create_session(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    offer: &str,
    context: &VoiceProviderContext,
    now: f64,
) -> Result<StartDecision, VoiceProviderError> {
    let request = StartRequest::new(
        AgentId::from(agent_id),
        session_id.to_owned(),
        offer.to_owned(),
    )?;
    store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
        let agent = super::checked_agent(transaction, agent_id)
            .map_err(|_| VoiceProviderError::ContextUnavailable)?;
        let current_thread = agent.extra.get("threadId").and_then(Value::as_str);
        let current_account = agent.extra.get("accountKey").and_then(Value::as_str).unwrap_or("default");
        if agent.epoch != context.epoch || current_thread != Some(context.thread_id.as_str())
            || current_account != context.account_key {
            return Err(VoiceProviderError::ContextChanged);
        }
        let old = load_row_any(transaction, session_id)?
            .map(VoiceSession::from_row).transpose()?;
        let active_row = transaction.query_row(
            "SELECT id,agent,created,ended,sdp,answer,native_thread,account_key,connection_id,state,error \
             FROM voice_sessions WHERE agent=?1 AND state IS NOT NULL AND ended IS NULL \
             ORDER BY created DESC LIMIT 1",
            [agent_id], session_from_row,
        ).optional()?;
        let active = active_row.map(VoiceSession::from_row).transpose()?;
        match policy::decide_start(Some(now), request, old.as_ref(), active.as_ref())? {
            decision @ StartDecision::Replay(_) => Ok(decision),
            StartDecision::Create { created, .. } => {
                transaction.execute(
                    "INSERT INTO voice_sessions(id,agent,created,sdp,native_thread,account_key,connection_id,state) \
                     VALUES(?1,?2,?3,?4,?5,?6,?7,'connecting')",
                    rusqlite::params![session_id,agent_id,created,offer,context.thread_id,context.account_key,context.connection_id],
                )?;
                let row = load_row(transaction, session_id, agent_id)?
                    .ok_or(policy::VoiceError::InvalidStoredSession)?;
                VoiceSession::from_row(row)?;
                Ok(StartDecision::Create {
                    agent: AgentId::from(agent_id),
                    session_id: policy::SessionId::parse(session_id.to_owned())?,
                    offer: offer.to_owned(),
                    created,
                })
            }
        }
    }).map_err(VoiceProviderError::from)
}

fn set_start_failure(
    store: &RuntimeStore,
    session_id: &str,
    failure: &VoiceProviderFailure,
    now: f64,
) -> Result<(), VoiceProviderError> {
    let state = match failure.certainty {
        VoiceWriteCertainty::NotSent | VoiceWriteCertainty::Rejected(_) => "failed",
        VoiceWriteCertainty::Unknown => "unknown",
    };
    store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
        transaction.execute(
            "UPDATE voice_sessions SET state=?1,error=?2,ended=COALESCE(ended,?3) WHERE id=?4",
            rusqlite::params![state, failure.message, now, session_id],
        )?;
        Ok(())
    })?;
    Ok(())
}

fn provider_error(error: VoiceProviderFailure) -> VoiceProviderError {
    match error.certainty {
        VoiceWriteCertainty::NotSent => VoiceProviderError::NotSent,
        VoiceWriteCertainty::Unknown => VoiceProviderError::Unknown,
        VoiceWriteCertainty::Rejected(_) => VoiceProviderError::Rejected(error.message),
    }
}

fn load_session(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
) -> Result<Value, VoiceProviderError> {
    let connection = store.read()?;
    super::checked_agent(&connection, agent_id)
        .map_err(|_| VoiceProviderError::ContextUnavailable)?;
    let row =
        load_row(&connection, session_id, agent_id)?.ok_or(policy::VoiceError::UnknownSession)?;
    Ok(session_value(&VoiceSession::from_row(row)?))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    struct ScriptedProvider {
        calls: Mutex<Vec<VoiceRpcCall>>,
    }

    impl VoiceProviderPort for ScriptedProvider {
        fn prepare<'a>(
            &'a self,
            agent_id: &'a str,
            account_key: &'a str,
            thread_id: &'a str,
            epoch: i64,
        ) -> ProviderFuture<'a, Result<VoiceProviderContext, VoiceProviderFailure>> {
            Box::pin(async move {
                Ok(VoiceProviderContext {
                    agent_id: agent_id.to_owned(),
                    epoch,
                    account_key: account_key.to_owned(),
                    thread_id: thread_id.to_owned(),
                    connection_id: "scripted-connection".to_owned(),
                })
            })
        }

        fn request<'a>(
            &'a self,
            call: VoiceRpcCall,
        ) -> ProviderFuture<'a, Result<(), VoiceProviderFailure>> {
            Box::pin(async move {
                self.calls.lock().expect("scripted calls lock").push(call);
                Ok(())
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
        };
        let call = start_call(context, "session", "v=0\r\n...");
        provider
            .request(call.clone())
            .await
            .expect("scripted provider accepts");
        let calls = provider.calls.lock().expect("scripted calls lock");
        assert_eq!(calls.as_slice(), &[call]);
        assert_eq!(calls[0].method, Method::ThreadRealtimeStart);
        assert_eq!(calls[0].params["transport"]["type"], "webrtc");
        assert_eq!(calls[0].params["transport"]["sdp"], "v=0\r\n...");
        assert_eq!(calls[0].params["clientManagedHandoffs"], false);
    }
}

fn start_call(context: VoiceProviderContext, session_id: &str, offer: &str) -> VoiceRpcCall {
    VoiceRpcCall {
        operation_id: format!("voice:start:{session_id}"),
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
    }
}
