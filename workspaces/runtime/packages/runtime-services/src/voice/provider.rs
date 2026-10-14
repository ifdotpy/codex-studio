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
use studio_runtime_store::{AgentId, AgentRepository, Connection, RuntimeStore, VoiceRepository};
use thiserror::Error;

use super::policy::{
    self, SessionSignal, StartDecision, StartRequest, VoiceSession, VoiceSessionRow,
};
use super::runtime::{
    VoiceConnectionScope as VoiceProviderContext, VoiceReply,
    VoiceRequestPort as VoiceProviderPort, VoiceRequestReceipt, VoiceRpcId, VoiceWriteCertainty,
};

const MAX_TRACKED_VOICE_SESSIONS: usize = 1024;
const VOICE_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);

type SessionDispatchLocks = HashMap<(String, String), Weak<tokio::sync::Mutex<()>>>;
static DISPATCH_LOCKS: OnceLock<Mutex<SessionDispatchLocks>> = OnceLock::new();

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

#[derive(Clone, Debug, PartialEq)]
pub struct VoiceRpcCall {
    pub operation_id: String,
    pub context: VoiceProviderContext,
    pub method: Method,
    pub params: Value,
    pub deadline: Instant,
}

/// Identity saved in an agent deletion intent before its voice rows are
/// removed. Field names serialize to the lifecycle receipt's camelCase shape.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct VoiceDeleteSession {
    pub session_id: String,
    pub agent_id: String,
    pub epoch: i64,
    pub account_key: String,
    pub thread_id: String,
    pub connection_id: String,
    pub generation: u64,
}

/// Durable post-commit voice-stop work staged by an agent deletion
/// transaction. `id` is also the broker authorization receipt key.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceDeleteIntent {
    pub id: String,
    pub agent_id: String,
    pub epoch: i64,
    pub sessions: Vec<VoiceDeleteSession>,
    pub held_uncertain: Vec<VoiceDeleteSession>,
}

const DELETE_VOICE_RECEIPT_SIGNATURE: &str = "voice-agent-delete-v1";

async fn request(provider: &dyn VoiceProviderPort, call: VoiceRpcCall) -> VoiceRequestReceipt {
    provider
        .request(
            call.context,
            call.operation_id,
            call.method,
            call.params,
            call.deadline,
        )
        .await
}

fn request_deadline() -> Instant {
    Instant::now() + VOICE_REQUEST_TIMEOUT
}

/// Snapshot an agent's sessions on its current account/thread for the durable
/// deletion intent. Call inside the deletion transaction, before voice rows
/// are removed. This deliberately reads a tombstoned agent's persisted fields.
pub fn capture_delete_sessions(
    connection: &Connection,
    agent_id: &str,
) -> Result<Vec<VoiceDeleteSession>, VoiceProviderError> {
    let agent = AgentRepository::get(connection, &AgentId::from(agent_id))?
        .filter(|agent| agent.id.0 == agent_id)
        .ok_or(VoiceProviderError::ContextUnavailable)?;
    let thread_id = agent
        .extra
        .get("threadId")
        .and_then(Value::as_str)
        .filter(|thread| !thread.is_empty());
    let Some(thread_id) = thread_id else {
        return Ok(Vec::new());
    };
    let account_key = agent
        .extra
        .get("accountKey")
        .and_then(Value::as_str)
        .filter(|account| !account.is_empty())
        .unwrap_or("default");
    let mut statement = connection.prepare(
        "SELECT id,connection_id FROM voice_sessions \
         WHERE agent=?1 AND ended IS NULL AND account_key=?2 AND native_thread=?3 \
         AND connection_id IS NOT NULL ORDER BY created,id",
    )?;
    let rows = statement.query_map(rusqlite::params![agent_id, account_key, thread_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    rows.map(|row| {
        let (session_id, connection_id) = row?;
        Ok(VoiceDeleteSession {
            session_id,
            agent_id: agent_id.to_owned(),
            epoch: agent.epoch,
            account_key: account_key.to_owned(),
            thread_id: thread_id.to_owned(),
            generation: connection_generation(&connection_id),
            connection_id,
        })
    })
    .collect::<Result<Vec<_>, rusqlite::Error>>()
    .map_err(VoiceProviderError::from)
}

fn capture_delete_voice_sessions(
    connection: &Connection,
    agent_id: &str,
) -> Result<(Vec<VoiceDeleteSession>, Vec<VoiceDeleteSession>), VoiceProviderError> {
    let sessions = capture_delete_sessions(connection, agent_id)?;
    let mut dispatchable = Vec::with_capacity(sessions.len());
    let mut held_uncertain = Vec::new();
    for session in sessions {
        match prior_voice_stop_result(connection, &session)? {
            Some(PriorVoiceStopResult::Submitted | PriorVoiceStopResult::Unknown) => {
                held_uncertain.push(session);
            }
            Some(PriorVoiceStopResult::Written) => {}
            _ => dispatchable.push(session),
        }
    }
    Ok((dispatchable, held_uncertain))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PriorVoiceStopResult {
    Submitted,
    Unknown,
    NotSent,
    Written,
    Other,
}

fn prior_voice_stop_result(
    connection: &Connection,
    session: &VoiceDeleteSession,
) -> Result<Option<PriorVoiceStopResult>, VoiceProviderError> {
    let operation = format!("voice:stop:{}", session.session_id);
    let prefix = format!(
        "account-worker:retained:{}:{}",
        session.account_key,
        studio_runtime_store::sha256_hex(operation.as_bytes())
    );
    let settled_result: Option<Option<String>> = connection
        .query_row(
            "SELECT settled.result FROM runtime_operation_receipts submitted \
         LEFT JOIN runtime_operation_receipts settled ON settled.id=?2 \
         WHERE submitted.id=?1 \
         AND json_extract(submitted.result,'$.call.operation')=?3 \
         AND json_extract(submitted.result,'$.call.method')='thread/realtime/stop' \
         AND json_extract(submitted.result,'$.call.actor.agent')=?4 \
         AND json_extract(submitted.result,'$.call.actor.epoch')<=?5 \
         AND json_extract(submitted.result,'$.call.scope.account')=?6 \
         AND json_extract(submitted.result,'$.call.scope.connection')=?7 \
         AND json_extract(submitted.result,'$.call.scope.generation')=?8 \
         AND json_extract(submitted.result,'$.call.params.threadId')=?9 \
         LIMIT 1",
            rusqlite::params![
                format!("{prefix}:submitted"),
                format!("{prefix}:result"),
                operation,
                session.agent_id,
                session.epoch,
                session.account_key,
                session.connection_id,
                i64::try_from(session.generation)
                    .map_err(|_| VoiceProviderError::ContextChanged)?,
                session.thread_id,
            ],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let Some(settled_result) = settled_result else {
        return Ok(None);
    };
    let Some(settled_result) = settled_result else {
        return Ok(Some(PriorVoiceStopResult::Submitted));
    };
    let receipt: Value =
        serde_json::from_str(&settled_result).map_err(|_| VoiceProviderError::ContextChanged)?;
    let result = serde_json::from_value::<crate::account_worker::retained::RequestResult>(
        receipt
            .get("result")
            .cloned()
            .ok_or(VoiceProviderError::ContextChanged)?,
    )
    .map_err(|_| VoiceProviderError::ContextChanged)?;
    Ok(Some(match result {
        crate::account_worker::retained::RequestResult::Reply(_) => PriorVoiceStopResult::Written,
        crate::account_worker::retained::RequestResult::NotSent => PriorVoiceStopResult::NotSent,
        crate::account_worker::retained::RequestResult::Unknown => PriorVoiceStopResult::Unknown,
        crate::account_worker::retained::RequestResult::Rejected(_) => PriorVoiceStopResult::Other,
    }))
}

/// Stage post-commit provider stops in the same transaction that deletes an
/// agent and its voice rows. The lifecycle transaction must call this before
/// deleting voice sessions, commit the returned intent with the tombstone, then
/// pass that intent to [`end_deleted_sessions`]. Existing receipts are replayed
/// verbatim so a repeated delete cannot lose the original session identities.
pub fn stage_delete_sessions(
    transaction: &Connection,
    agent_id: &str,
) -> Result<VoiceDeleteIntent, VoiceProviderError> {
    let agent = AgentRepository::get(transaction, &AgentId::from(agent_id))?
        .filter(|agent| agent.id.0 == agent_id)
        .ok_or(VoiceProviderError::ContextUnavailable)?;
    let receipt_id = format!("agent-delete-voice:{agent_id}:{}", agent.epoch);
    if let Some((signature, result)) = transaction
        .query_row(
            "SELECT signature,result FROM runtime_operation_receipts WHERE id=?1",
            [&receipt_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?
    {
        let result: Value =
            serde_json::from_str(&result).map_err(|_| VoiceProviderError::ContextChanged)?;
        if signature != DELETE_VOICE_RECEIPT_SIGNATURE
            || result.get("agent").and_then(Value::as_str) != Some(agent_id)
            || result.get("epoch").and_then(Value::as_i64) != Some(agent.epoch)
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        let sessions = serde_json::from_value(
            result
                .get("voiceEndSessions")
                .cloned()
                .ok_or(VoiceProviderError::ContextChanged)?,
        )
        .map_err(|_| VoiceProviderError::ContextChanged)?;
        let held_uncertain: Vec<VoiceDeleteSession> = serde_json::from_value(
            result
                .get("voiceEndHeldUncertain")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .map_err(|_| VoiceProviderError::ContextChanged)?;
        return Ok(VoiceDeleteIntent {
            id: receipt_id,
            agent_id: agent_id.to_owned(),
            epoch: agent.epoch,
            sessions,
            held_uncertain,
        });
    }

    let (sessions, held_uncertain) = capture_delete_voice_sessions(transaction, agent_id)?;
    let result = json!({
        "agent": agent_id,
        "epoch": agent.epoch,
        "voiceEndSessions": sessions,
        "voiceEndHeldUncertain": held_uncertain,
    });
    transaction.execute(
        "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3)",
        rusqlite::params![
            receipt_id,
            DELETE_VOICE_RECEIPT_SIGNATURE,
            result.to_string()
        ],
    )?;
    Ok(VoiceDeleteIntent {
        id: receipt_id,
        agent_id: agent_id.to_owned(),
        epoch: agent.epoch,
        sessions,
        held_uncertain,
    })
}

/// Load durable agent-deletion voice-stop intents for post-commit delivery or
/// restart recovery. The receipt identity and signature are checked before any
/// provider request can be constructed.
pub fn pending_delete_intents(
    store: &RuntimeStore,
) -> Result<Vec<VoiceDeleteIntent>, VoiceProviderError> {
    reconcile_proven_unsent_held_sessions(store)?;
    let connection = store.read()?;
    let mut statement = connection.prepare(
        "SELECT id,signature,result FROM runtime_operation_receipts \
         WHERE id >= 'agent-delete-voice:' AND id < 'agent-delete-voice;' ORDER BY id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    let mut intents = Vec::new();
    for row in rows {
        let (id, signature, raw) = row?;
        let result: Value =
            serde_json::from_str(&raw).map_err(|_| VoiceProviderError::ContextChanged)?;
        let agent_id = result
            .get("agent")
            .and_then(Value::as_str)
            .filter(|agent| !agent.is_empty())
            .ok_or(VoiceProviderError::ContextChanged)?;
        let epoch = result
            .get("epoch")
            .and_then(Value::as_i64)
            .filter(|epoch| *epoch >= 0)
            .ok_or(VoiceProviderError::ContextChanged)?;
        if signature != DELETE_VOICE_RECEIPT_SIGNATURE
            || id != format!("agent-delete-voice:{agent_id}:{epoch}")
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        let sessions: Vec<VoiceDeleteSession> = serde_json::from_value(
            result
                .get("voiceEndSessions")
                .cloned()
                .ok_or(VoiceProviderError::ContextChanged)?,
        )
        .map_err(|_| VoiceProviderError::ContextChanged)?;
        let held_uncertain: Vec<VoiceDeleteSession> = serde_json::from_value(
            result
                .get("voiceEndHeldUncertain")
                .cloned()
                .unwrap_or_else(|| json!([])),
        )
        .map_err(|_| VoiceProviderError::ContextChanged)?;
        if held_uncertain
            .iter()
            .any(|session| session.agent_id != agent_id || session.epoch != epoch)
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        if sessions
            .iter()
            .any(|session| session.agent_id != agent_id || session.epoch != epoch)
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        let sessions = pending_voice_delete_sessions(
            sessions,
            result.get("voiceEndResults").and_then(Value::as_object),
        )?;
        intents.push(VoiceDeleteIntent {
            id,
            agent_id: agent_id.to_owned(),
            epoch,
            sessions,
            held_uncertain,
        });
    }
    Ok(intents)
}

/// If a held ordinary stop settles as proven NotSent after deletion, safely
/// transfer that session to the deletion stop queue. Unknown and unsettled
/// receipts remain held with their original identity; only proven non-delivery
/// permits a new delete-stop identity.
fn reconcile_proven_unsent_held_sessions(store: &RuntimeStore) -> Result<(), VoiceProviderError> {
    store.transaction_with_error(|tx| {
        let mut statement = tx.prepare(
            "SELECT id,signature,result FROM runtime_operation_receipts \
             WHERE id >= 'agent-delete-voice:' AND id < 'agent-delete-voice;' ORDER BY id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        let receipts = rows.collect::<Result<Vec<_>, _>>()?;
        drop(statement);

        for (id, signature, raw) in receipts {
            if signature != DELETE_VOICE_RECEIPT_SIGNATURE {
                continue;
            }
            let mut result: Value =
                serde_json::from_str(&raw).map_err(|_| VoiceProviderError::ContextChanged)?;
            let Some(held) = result
                .get("voiceEndHeldUncertain")
                .and_then(Value::as_array)
                .cloned()
            else {
                continue;
            };
            if held.is_empty() {
                continue;
            }
            let held_len = held.len();
            let mut remaining = Vec::with_capacity(held.len());
            let mut sessions: Vec<VoiceDeleteSession> = serde_json::from_value(
                result
                    .get("voiceEndSessions")
                    .cloned()
                    .ok_or(VoiceProviderError::ContextChanged)?,
            )
            .map_err(|_| VoiceProviderError::ContextChanged)?;
            for value in held {
                let session: VoiceDeleteSession = serde_json::from_value(value.clone())
                    .map_err(|_| VoiceProviderError::ContextChanged)?;
                match prior_voice_stop_result(tx, &session)? {
                    Some(PriorVoiceStopResult::NotSent | PriorVoiceStopResult::Other) => {
                        if !sessions
                            .iter()
                            .any(|queued| queued.session_id == session.session_id)
                        {
                            sessions.push(session);
                        }
                    }
                    Some(PriorVoiceStopResult::Written) => {}
                    _ => remaining.push(value),
                }
            }
            if remaining.len() != held_len {
                result["voiceEndSessions"] = serde_json::to_value(sessions)
                    .map_err(|_| VoiceProviderError::ContextChanged)?;
                result["voiceEndHeldUncertain"] = Value::Array(remaining);
                tx.execute(
                    "UPDATE runtime_operation_receipts SET result=?2 \
                     WHERE id=?1 AND signature=?3",
                    rusqlite::params![id, result.to_string(), DELETE_VOICE_RECEIPT_SIGNATURE],
                )?;
            }
        }
        Ok(())
    })
}

fn pending_voice_delete_sessions(
    sessions: Vec<VoiceDeleteSession>,
    delivered: Option<&serde_json::Map<String, Value>>,
) -> Result<Vec<VoiceDeleteSession>, VoiceProviderError> {
    if let Some(delivered) = delivered {
        for (session_id, status) in delivered {
            if !sessions
                .iter()
                .any(|session| &session.session_id == session_id)
                || !matches!(
                    status.as_str(),
                    Some("written" | "unknown" | "rejected" | "not_sent")
                )
            {
                return Err(VoiceProviderError::ContextChanged);
            }
        }
    }
    Ok(sessions
        .into_iter()
        .filter(|session| {
            matches!(
                delivered
                    .and_then(|results| results.get(&session.session_id))
                    .and_then(Value::as_str),
                None | Some("not_sent")
            )
        })
        .collect())
}

/// Stop sessions captured by `capture_delete_sessions` after the deletion
/// transaction commits. Stable operation identities let the broker replay a
/// saved result and keep Unknown outcomes from being resent.
pub async fn end_deleted_sessions(
    provider: std::sync::Arc<dyn VoiceProviderPort>,
    deletion_intent_id: String,
    sessions: Vec<VoiceDeleteSession>,
    now: f64,
) -> Result<Vec<VoiceRequestReceipt>, VoiceProviderError> {
    if !now.is_finite() || now < 0.0 {
        return Err(policy::VoiceError::InvalidClock.into());
    }
    if sessions.is_empty() {
        return Ok(Vec::new());
    }
    let first = &sessions[0];
    if deletion_intent_id != format!("agent-delete-voice:{}:{}", first.agent_id, first.epoch)
        || sessions.iter().any(|session| {
            session.agent_id != first.agent_id
                || session.epoch != first.epoch
                || session.session_id.is_empty()
                || session.thread_id.is_empty()
                || session.account_key.is_empty()
                || session.connection_id.is_empty()
        })
    {
        return Err(VoiceProviderError::ContextChanged);
    }
    let mut receipts = Vec::with_capacity(sessions.len());
    for session in sessions {
        let context = VoiceProviderContext {
            agent_id: session.agent_id,
            epoch: session.epoch,
            account_key: session.account_key,
            thread_id: session.thread_id.clone(),
            connection_id: session.connection_id,
            generation: session.generation,
        };
        receipts.push(
            request(
                provider.as_ref(),
                VoiceRpcCall {
                    operation_id: format!("voice:delete-stop:{}", session.session_id),
                    context,
                    method: Method::ThreadRealtimeStop,
                    params: json!({"threadId": session.thread_id}),
                    deadline: request_deadline(),
                },
            )
            .await,
        );
    }
    Ok(receipts)
}

/// Deliver deletion stops while recording each receipt's certainty. Missing
/// entries remain retryable after a crash; a proven NotSent can retry, while
/// Written, Unknown, and Rejected are terminal for this operation identity.
pub async fn end_deleted_sessions_tracked(
    store: std::sync::Arc<RuntimeStore>,
    provider: std::sync::Arc<dyn VoiceProviderPort>,
    deletion_intent_id: String,
    sessions: Vec<VoiceDeleteSession>,
    now: f64,
) -> Result<(), VoiceProviderError> {
    for session in sessions {
        let session_id = session.session_id.clone();
        let receipts = end_deleted_sessions(
            std::sync::Arc::clone(&provider),
            deletion_intent_id.clone(),
            vec![session],
            now,
        )
        .await?;
        let certainty = receipts
            .first()
            .map(|receipt| receipt.certainty)
            .ok_or(VoiceProviderError::ContextChanged)?;
        let status = match certainty {
            VoiceWriteCertainty::Written => "written",
            VoiceWriteCertainty::NotSent => "not_sent",
            VoiceWriteCertainty::Unknown => "unknown",
            VoiceWriteCertainty::Rejected => "rejected",
        };
        store.transaction_with_error(|tx| -> Result<(), VoiceProviderError> {
            let mut result: Value = serde_json::from_str(&tx.query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1 AND signature=?2",
                rusqlite::params![deletion_intent_id, DELETE_VOICE_RECEIPT_SIGNATURE],
                |row| row.get::<_, String>(0),
            )?)
            .map_err(|_| VoiceProviderError::ContextChanged)?;
            let results = result
                .as_object_mut()
                .ok_or(VoiceProviderError::ContextChanged)?
                .entry("voiceEndResults")
                .or_insert_with(|| json!({}))
                .as_object_mut()
                .ok_or(VoiceProviderError::ContextChanged)?;
            results.insert(session_id.clone(), Value::String(status.to_owned()));
            tx.execute(
                "UPDATE runtime_operation_receipts SET result=?2 WHERE id=?1 AND signature=?3",
                rusqlite::params![
                    deletion_intent_id,
                    result.to_string(),
                    DELETE_VOICE_RECEIPT_SIGNATURE
                ],
            )?;
            Ok(())
        })?;
    }
    Ok(())
}

/// Stop all current account/thread voice sessions for an agent. Use this for
/// lifecycle stops that retain the voice rows; deletions use saved intents.
pub async fn end_active_for_agent(
    store: std::sync::Arc<RuntimeStore>,
    provider: std::sync::Arc<dyn VoiceProviderPort>,
    agent_id: String,
    now: f64,
) -> Result<Vec<Value>, VoiceProviderError> {
    let store_for_read = store.clone();
    let agent_for_read = agent_id.clone();
    let sessions = tokio::task::spawn_blocking(move || {
        let connection = store_for_read.read()?;
        capture_delete_sessions(&connection, &agent_for_read)
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    let mut ended = Vec::with_capacity(sessions.len());
    for session in sessions {
        ended.push(
            end(
                store.clone(),
                provider.clone(),
                agent_id.clone(),
                session.session_id,
                now,
            )
            .await?,
        );
    }
    Ok(ended)
}

fn connection_generation(connection: &str) -> u64 {
    // The retained account worker uses its generation as connection_id.
    // Non-numeric Python-era ids cannot authorize a generation-bound write.
    connection.parse().unwrap_or(0)
}

fn certainty_error(certainty: VoiceWriteCertainty) -> VoiceProviderError {
    match certainty {
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
                if let Some(role) = params.get("role").and_then(Value::as_str)
                    && let Some(state) = sessions.get_mut(&session_id)
                {
                    state.tail.remove(role);
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
    Rejected,
    #[error("voice start admission denied: {0}")]
    AdmissionDenied(String),
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
    let _dispatch = session_dispatch_guard(&agent_id, &session_id).await;
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
        deadline: request_deadline(),
    };
    let receipt = request(provider.as_ref(), call).await;
    if receipt.certainty != VoiceWriteCertainty::Written {
        let store_for_update = store.clone();
        let id = session_id.clone();
        let certainty = receipt.certainty;
        tokio::task::spawn_blocking(move || {
            update_stop_failure(&store_for_update, &id, certainty, now)
        })
        .await
        .map_err(|_| VoiceProviderError::Operation)??;
        return Err(certainty_error(receipt.certainty));
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
    let connection = store.read()?;
    super::checked_agent(&connection, agent_id)
        .map_err(|_| VoiceProviderError::ContextUnavailable)?;
    if load_row(&connection, session_id, agent_id)?.is_none() {
        return Ok(None);
    }
    lookup_speech_receipt(
        &connection,
        session_id,
        record_id,
        &speech_receipt_id(session_id, record_id),
    )
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
                        generation: connection_generation(connection),
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
    })
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
        return lookup_speech_receipt(&connection, &session_id, &record_id, &receipt_id)?
            .ok_or(VoiceProviderError::Operation);
    };
    let thread_id = context.thread_id.clone();
    let call = VoiceRpcCall {
        operation_id: format!("voice:speech:{receipt_id}"),
        context,
        method: Method::ThreadRealtimeAppendSpeech,
        params: json!({"threadId": thread_id, "text": text}),
        deadline: request_deadline(),
    };
    let outcome = request(provider.as_ref(), call).await;
    let state = match outcome.certainty {
        VoiceWriteCertainty::Written => ("submitted", None),
        VoiceWriteCertainty::NotSent => ("failed", Some("Request was not sent")),
        VoiceWriteCertainty::Unknown => ("unknown", Some("Request outcome is unknown")),
        VoiceWriteCertainty::Rejected => ("failed", Some("Provider rejected request")),
    };
    let store_for_update = store.clone();
    let id = receipt_id.clone();
    let state_name = state.0.to_owned();
    let state_error = state.1;
    tokio::task::spawn_blocking(move || {
        update_speech(&store_for_update, &id, &state_name, state_error)
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    if outcome.certainty != VoiceWriteCertainty::Written {
        return Err(certainty_error(outcome.certainty));
    }
    let connection = store.read()?;
    lookup_speech_receipt(&connection, &session_id, &record_id, &receipt_id)?
        .ok_or(VoiceProviderError::Operation)
}

enum ApprovalDispatch {
    Replay(Value),
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
pub fn is_monitor_approval(
    store: &RuntimeStore,
    agent_id: &str,
    speech_id: &str,
) -> Result<bool, VoiceProviderError> {
    let connection = store.read()?;
    let Some(speech) = VoiceRepository::record(&connection, speech_id)? else {
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
    let store_for_reserve = std::sync::Arc::clone(&store);
    let (request_id, replay) = tokio::task::spawn_blocking(move || {
        match reserve_approval(
            &store_for_reserve,
            &agent_id,
            &session_id,
            &speech_id,
            &transcript_id,
        )? {
            ApprovalDispatch::Replay(value) => Ok((String::new(), Some(value))),
            ApprovalDispatch::Monitor { request_id } => Ok((request_id, None)),
            ApprovalDispatch::Send { .. } => Err(VoiceProviderError::Operation),
        }
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    if let Some(value) = replay {
        return Ok(value);
    }
    if !start_dispatch_available {
        release_monitor_approval(&store, &request_id).await?;
        return Err(VoiceProviderError::RequiresDependency(
            "monitor-driver/start-dispatch",
        ));
    }
    let request_for_answer = request_id.clone();
    let data = serde_json::json!({"decision":"accept"});
    let answer_store = std::sync::Arc::clone(&store);
    let answer = tokio::task::spawn_blocking(move || {
        crate::monitor_runtime::answer_approval(
            &answer_store,
            &request_for_answer,
            &data,
            now,
            start_dispatch_available,
        )
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)?;
    match answer {
        Ok(Some(answer)) if answer.accepted => {
            let result = answer.response;
            let saved =
                serde_json::to_string(&result).map_err(|_| VoiceProviderError::Operation)?;
            let update_store = std::sync::Arc::clone(&store);
            let update_id = request_id.clone();
            tokio::task::spawn_blocking(move || {
                update_store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
                    transaction.execute(
                        "UPDATE voice_approvals SET status='accepted',result=?1 WHERE id=?2 AND status='uncertain'",
                        rusqlite::params![saved, update_id],
                    )?;
                    Ok(())
                })
            })
            .await
            .map_err(|_| VoiceProviderError::Operation)??;
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
    let store = std::sync::Arc::clone(store);
    let id = request_id.to_owned();
    tokio::task::spawn_blocking(move || {
        store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
            transaction.execute(
                "UPDATE voice_approvals SET status='not_sent' WHERE id=?1 AND status='uncertain'",
                [id],
            )?;
            Ok(())
        })
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
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
    let store_for_reserve = store.clone();
    let reserve_agent = agent_id.clone();
    let reserve_session = session_id.clone();
    let reserve_speech = speech_id.clone();
    let reserve_transcript = transcript_id.clone();
    let reservation = tokio::task::spawn_blocking(move || {
        reserve_approval(
            &store_for_reserve,
            &reserve_agent,
            &reserve_session,
            &reserve_speech,
            &reserve_transcript,
        )
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    let (request_id, context, rpc_id) = match reservation {
        ApprovalDispatch::Replay(value) => return Ok(value),
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
    tokio::task::spawn_blocking(move || {
        store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
            transaction.execute(
                "UPDATE voice_approvals SET status=?1,result=?2 WHERE id=?3",
                rusqlite::params![state_value, result_json, id],
            )?;
            Ok(())
        })?;
        Ok::<_, VoiceProviderError>(())
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    if certainty != VoiceWriteCertainty::Written {
        return Err(certainty_error(certainty));
    }
    Ok(result)
}

fn reserve_approval(
    store: &RuntimeStore,
    agent_id: &str,
    session_id: &str,
    speech_id: &str,
    transcript_id: &str,
) -> Result<ApprovalDispatch, VoiceProviderError> {
    store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
        let actor = super::checked_agent(transaction, agent_id)
            .map_err(|_| VoiceProviderError::ContextUnavailable)?;
        let transcript = VoiceRepository::record(transaction, transcript_id)?
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
            rusqlite::params![agent_id, session_id],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        ).optional()?;
        let Some((played_seq, played_payload)) = played else {
            return Err(VoiceProviderError::ContextChanged);
        };
        let played_payload: Value = serde_json::from_str(&played_payload)
            .map_err(|_| VoiceProviderError::Operation)?;
        if played_payload.get("record_id").and_then(Value::as_str) != Some(speech_id)
            || transcript.sequence <= played_seq
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        let speech = VoiceRepository::record(transaction, speech_id)?
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
            if owner != agent_id || old_speech != speech_id || old_transcript != transcript_id {
                return Err(VoiceProviderError::ContextChanged);
            }
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
                        rusqlite::params![serde_json::to_string(&replay).map_err(|_| VoiceProviderError::Operation)?, request_id],
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
        if current.get("agent").and_then(Value::as_str) != Some(agent_id)
            || current.get("status").and_then(Value::as_str) != Some("pending")
            || !method.is_some_and(|method| super::APPROVAL_METHODS.contains(&method))
            || metadata.get("fingerprint").and_then(Value::as_str) != Some(fingerprint.as_str())
            || current.get("epoch").and_then(Value::as_i64) != Some(actor.epoch)
            || actor.extra.get("autoWake").and_then(Value::as_bool) != Some(true)
        {
            return Err(VoiceProviderError::ContextChanged);
        }
        if method == Some("monitor/approve") {
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
        let session = load_row(transaction, session_id, agent_id)?
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
        let context = VoiceProviderContext {
            agent_id: agent_id.to_owned(),
            epoch: actor.epoch,
            account_key: account_key.to_owned(),
            thread_id: thread_id.to_owned(),
            connection_id: session.connection_id().ok_or(VoiceProviderError::ContextUnavailable)?.to_owned(),
            generation: connection_generation(session.connection_id().ok_or(VoiceProviderError::ContextUnavailable)?),
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
    })
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
        _ => None,
    }
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
        if let Some(old) = lookup_speech_receipt_raw(transaction, session_id, record_id, request_id)? {
            if old["session"] != session_id || old["record_id"] != record_id {
                return Err(VoiceProviderError::Policy(policy::VoiceError::SessionIdentityConflict));
            }
            if let Some(marker) = old["error"].as_str().and_then(|error| error.strip_prefix("voice-content-sha256:")) {
                let stored = marker.split('|').next().unwrap_or_default();
                let record = VoiceRepository::record(transaction, record_id)?
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
        let record = VoiceRepository::record(transaction, record_id)?
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
        let context = VoiceProviderContext {
            agent_id: agent_id.to_owned(), epoch: agent.epoch,
            account_key: account_key.to_owned(),
            thread_id: thread_id.to_owned(),
            connection_id: session.connection_id().ok_or(VoiceProviderError::ContextUnavailable)?.to_owned(),
            generation: connection_generation(session.connection_id().ok_or(VoiceProviderError::ContextUnavailable)?),
        };
        let marker = format!("voice-content-sha256:{}|", studio_runtime_store::sha256_hex(record.text.as_bytes()));
        transaction.execute(
            "INSERT INTO voice_speech_requests(id,session,record_id,state,error) VALUES(?1,?2,?3,'pending',?4)",
            rusqlite::params![request_id,session_id,record_id,marker],
        )?;
        Ok(Some((context, record.text)))
    })
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
    certainty: VoiceWriteCertainty,
    now: f64,
) -> Result<(), VoiceProviderError> {
    let (state, ended) = match certainty {
        VoiceWriteCertainty::NotSent => ("ended", Some(now)),
        VoiceWriteCertainty::Unknown => ("unknown", None),
        VoiceWriteCertainty::Rejected => ("unknown", None),
        VoiceWriteCertainty::Written => ("ended", Some(now)),
    };
    store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
        transaction.execute(
            "UPDATE voice_sessions SET state=?1,error=?2,ended=COALESCE(ended,?3) WHERE id=?4",
            rusqlite::params![state, "Voice stop outcome is unresolved", ended, session_id],
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
        let current: Option<String> = transaction
            .query_row(
                "SELECT error FROM voice_speech_requests WHERE id=?1",
                [request_id],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let marker = current
            .and_then(|value| value.split('|').next().map(str::to_owned))
            .unwrap_or_default();
        let detail = error.unwrap_or_default();
        let stored_error = if marker.is_empty() {
            error.map(str::to_owned)
        } else if detail.is_empty() {
            Some(format!("{marker}|"))
        } else {
            Some(format!("{marker}|{detail}"))
        };
        transaction.execute(
            "UPDATE voice_speech_requests SET state=?1,error=?2 WHERE id=?3",
            rusqlite::params![state, stored_error, request_id],
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
            let transition = policy::transition_session(
                &session_value,
                SessionSignal::ProviderFailed(None),
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
            generation: connection_generation(connection_id),
        },
        method: Method::ThreadRealtimeStop,
        params: json!({"threadId": thread_id}),
        deadline: request_deadline(),
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
    let _dispatch = session_dispatch_guard(&agent_id, &session_id).await;
    let start_request = StartRequest::new(
        AgentId::from(agent_id.as_str()),
        session_id.clone(),
        offer.clone(),
    )?;
    let store_for_read = store.clone();
    let agent_for_read = agent_id.clone();
    let session_for_read = session_id.clone();
    let existing = tokio::task::spawn_blocking(move || {
        read_start_rows(
            &store_for_read,
            &agent_for_read,
            &session_for_read,
            start_request,
        )
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    if let StartDecision::Replay(session) = existing {
        return Ok(session_value(&session));
    }

    let (account_key, thread_id, epoch, root_id) = agent_context(&store, &agent_id)?;
    let context = provider
        .prepare(&agent_id, &session_id, &account_key, &thread_id, epoch)
        .await
        .map_err(certainty_error)?;
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
    let histories = tokio::task::spawn_blocking({
        let store = store.clone();
        move || crate::budget_runtime::read_histories(&store, &root_id)
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)?
    .map_err(|_| VoiceProviderError::Operation)?;
    let saved = tokio::task::spawn_blocking(move || {
        create_session(
            &store_for_write,
            &write_agent,
            &write_session,
            &write_offer,
            &write_context,
            &histories,
            now,
        )
    })
    .await
    .map_err(|_| VoiceProviderError::Operation)??;
    if let StartDecision::Replay(session) = saved {
        return Ok(session_value(&session));
    }

    let call = start_call(context, &session_id, &offer);
    let receipt = request(provider.as_ref(), call).await;
    if receipt.certainty != VoiceWriteCertainty::Written {
        let store_for_error = store.clone();
        let id = session_id.clone();
        let certainty = receipt.certainty;
        tokio::task::spawn_blocking(move || {
            set_start_failure(&store_for_error, &id, certainty, now)
        })
        .await
        .map_err(|_| VoiceProviderError::Operation)??;
        return Err(certainty_error(receipt.certainty));
    }
    load_session(&store, &agent_id, &session_id)
}

fn agent_context(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<(String, String, i64, String), VoiceProviderError> {
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
    Ok((
        account_key.to_owned(),
        thread_id.to_owned(),
        agent.epoch,
        agent.root_id.0,
    ))
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
    histories: &std::collections::BTreeMap<String, crate::budget::HistoryObservation>,
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
        crate::context_repair::assert_available(&agent.value())
            .map_err(|_| VoiceProviderError::ContextChanged)?;
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
                let root = AgentRepository::get(transaction, &agent.root_id)?
                    .ok_or(VoiceProviderError::ContextUnavailable)?;
                let admission = crate::budget_runtime::admission(
                    store,
                    transaction,
                    &agent.value(),
                    &root.value(),
                    histories,
                    now,
                )
                .map_err(|_| VoiceProviderError::Operation)?;
                if let Some(denial) = admission.denial {
                    return Err(VoiceProviderError::AdmissionDenied(denial.message));
                }
                let mut agent = agent;
                agent.extra.insert("autoWake".into(), Value::Bool(true));
                agent
                    .extra
                    .insert("turnEpoch".into(), Value::from(agent.epoch));
                AgentRepository::put(transaction, &agent)?;
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
    })
}

fn set_start_failure(
    store: &RuntimeStore,
    session_id: &str,
    certainty: VoiceWriteCertainty,
    now: f64,
) -> Result<(), VoiceProviderError> {
    let state = match certainty {
        VoiceWriteCertainty::NotSent => "failed",
        VoiceWriteCertainty::Unknown => "unknown",
        VoiceWriteCertainty::Rejected => "failed",
        VoiceWriteCertainty::Written => "connecting",
    };
    store.transaction_with_error(|transaction| -> Result<_, VoiceProviderError> {
        transaction.execute(
            "UPDATE voice_sessions SET state=?1,error=?2,ended=COALESCE(ended,?3) WHERE id=?4",
            rusqlite::params![state, "Voice start outcome is unresolved", now, session_id],
        )?;
        Ok(())
    })?;
    Ok(())
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
        deadline: request_deadline(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::runtime::VoiceFuture;
    use std::sync::{Arc, Mutex};

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
        ) -> VoiceFuture<'a, Result<VoiceProviderContext, VoiceWriteCertainty>> {
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
            operation_id: String,
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
                        operation_id,
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
                        operation_id,
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
    async fn dispatch_guard_serializes_same_session_but_not_other_sessions() {
        let held = session_dispatch_guard("agent", "session-a").await;
        let (same_tx, mut same_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _guard = session_dispatch_guard("agent", "session-a").await;
            same_tx.send(()).expect("same-session receiver remains");
        });
        let (other_tx, mut other_rx) = tokio::sync::oneshot::channel();
        tokio::spawn(async move {
            let _guard = session_dispatch_guard("agent", "session-b").await;
            other_tx.send(()).expect("other-session receiver remains");
        });
        tokio::task::yield_now().await;
        assert!(same_rx.try_recv().is_err(), "same-session work must wait");
        assert_eq!(
            other_rx.try_recv(),
            Ok(()),
            "different sessions stay independent"
        );
        drop(held);
        same_rx.await.expect("same-session guard releases");
    }

    #[test]
    fn completed_delete_stops_are_not_requeued_but_not_sent_stops_are() {
        let sessions = ["written", "unknown", "rejected", "not-sent", "pending"]
            .into_iter()
            .map(|session_id| VoiceDeleteSession {
                session_id: session_id.to_owned(),
                agent_id: "agent".to_owned(),
                epoch: 3,
                account_key: "account".to_owned(),
                thread_id: "thread".to_owned(),
                connection_id: "connection".to_owned(),
                generation: 1,
            })
            .collect::<Vec<_>>();
        let delivered = serde_json::from_value::<serde_json::Map<String, Value>>(json!({
            "written": "written",
            "unknown": "unknown",
            "rejected": "rejected",
            "not-sent": "not_sent",
        }))
        .expect("valid delivery result map");

        let pending = pending_voice_delete_sessions(sessions, Some(&delivered))
            .expect("valid saved outcomes");
        assert_eq!(
            pending
                .iter()
                .map(|session| session.session_id.as_str())
                .collect::<Vec<_>>(),
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
