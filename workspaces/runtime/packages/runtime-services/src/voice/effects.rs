//! One durable stop effect per voice session.
use std::sync::Arc;

use serde_json::{Value, json};
use studio_runtime_providers::Method;
use studio_runtime_schema::persistence::{
    CapturedScope, OutboxKind, OutboxStatus, OwnerKind, RequestIdentity, VoiceStopOutcome,
};
use studio_runtime_store::{
    AgentId, AgentRepository, Connection, RuntimeStore, Transaction, VoiceEffect,
    VoiceEffectAuthority, VoiceEffectDisposition,
    outbox::{Outbox, OutboxRow},
    request_results::{Admission, AdmittedRequest, RequestResults},
};

use super::provider::{VoiceProviderError, VoiceRpcCall, request, request_deadline, saved_scope};
use super::runtime::{VoiceConnectionScope, VoiceRequestPort, VoiceWriteCertainty};

const STOP_SIGNATURE: &str = "lifecycle.voice-stop.v1";
const DELETE_SIGNATURE: &str = "voice-agent-delete-v1";
const PENDING_BATCH: u32 = 500;

pub(crate) fn hold_unclean_effects(tx: &Transaction<'_>) -> Result<(), VoiceProviderError> {
    for kind in [OutboxKind::VoiceStop, OutboxKind::VoiceDelete] {
        let mut query = tx.prepare("SELECT id FROM runtime_outbox WHERE kind=?1 AND status=?2")?;
        let ids = query
            .query_map(rusqlite::params![kind, OutboxStatus::Pending], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(query);
        for id in ids {
            let mut row =
                Outbox::get::<VoiceEffect>(tx, &id)?.ok_or(VoiceProviderError::ContextChanged)?;
            let VoiceEffectAuthority::Admitted { outcome, .. } = &mut row.payload.authority else {
                return Err(VoiceProviderError::ContextChanged);
            };
            *outcome = VoiceStopOutcome::Unknown;
            row.status = OutboxStatus::Unknown;
            row.next_at = None;
            Outbox::put(tx, &row)?;
        }
    }
    Ok(())
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceDeleteSession {
    pub session_id: String,
    pub agent_id: String,
    pub epoch: i64,
    pub account_key: String,
    pub thread_id: String,
    pub connection_id: String,
    pub generation: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VoiceDeleteIntent {
    pub id: String,
    pub session: VoiceDeleteSession,
}

fn operation(kind: OutboxKind, session: &str) -> Result<String, VoiceProviderError> {
    let (operation, _) = kind_fields(kind)?;
    Ok(format!("voice:{operation}:{session}"))
}

fn signature(kind: OutboxKind) -> Result<&'static str, VoiceProviderError> {
    Ok(kind_fields(kind)?.1)
}

fn kind_fields(kind: OutboxKind) -> Result<(&'static str, &'static str), VoiceProviderError> {
    if kind == OutboxKind::VoiceStop {
        Ok(("stop", STOP_SIGNATURE))
    } else if kind == OutboxKind::VoiceDelete {
        Ok(("delete-stop", DELETE_SIGNATURE))
    } else {
        Err(VoiceProviderError::ContextChanged)
    }
}

fn current(
    tx: &Transaction<'_>,
    agent: &studio_runtime_store::AgentRecord,
    scope: &CapturedScope,
) -> Result<bool, VoiceProviderError> {
    let CapturedScope::Known(saved) = scope else {
        return Ok(false);
    };
    match studio_runtime_store::account_state::current_provider_scope(tx, agent) {
        Ok(scope) => Ok(scope == *saved),
        Err(studio_runtime_store::account_state::ScopeError::Unavailable) => Ok(false),
        Err(studio_runtime_store::account_state::ScopeError::Store(error)) => Err(error.into()),
    }
}

fn captured(effect: &VoiceEffect) -> CapturedScope {
    match &effect.authority {
        VoiceEffectAuthority::Admitted { scope, .. } => CapturedScope::Known(scope.clone()),
        VoiceEffectAuthority::Held { scope } => scope.clone(),
    }
}

pub fn capture_delete_sessions(
    connection: &Connection,
    agent_id: &str,
) -> Result<Vec<VoiceEffect>, VoiceProviderError> {
    let agent = AgentRepository::get(connection, &AgentId::from(agent_id))?
        .ok_or(VoiceProviderError::ContextUnavailable)?;
    let account = agent.account_key.as_str();
    let Some(thread) = agent.thread_id.as_deref() else {
        return Ok(Vec::new());
    };
    let mut query = connection.prepare("SELECT id,account_key,native_thread,connection_id FROM voice_sessions WHERE agent=?1 AND ended IS NULL AND state IS NOT NULL AND account_key=?2 AND native_thread=?3 ORDER BY created,id")?;
    let rows = query
        .query_map(rusqlite::params![agent_id, account, thread], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    rows.into_iter().map(|(session_id, account, thread, connection_id)| {
        let scope = saved_scope(connection, &session_id)?;
        if matches!(&scope, CapturedScope::Known(saved) if Some(saved.connection.as_str()) != connection_id.as_deref()) { return Err(VoiceProviderError::ContextChanged); }
        Ok(VoiceEffect { session_id, agent: agent_id.into(), epoch: agent.epoch, account, thread, disposition: VoiceEffectDisposition::Active, authority: VoiceEffectAuthority::Held { scope } })
    }).collect()
}

fn reserved(
    tx: &Transaction<'_>,
    mut effect: VoiceEffect,
    kind: OutboxKind,
    root: String,
    owner: &AdmittedRequest,
) -> Result<OutboxRow<VoiceEffect>, VoiceProviderError> {
    let CapturedScope::Known(scope) = captured(&effect) else {
        return Err(VoiceProviderError::ContextUnavailable);
    };
    let intent = crate::account_worker::retained::ProviderCallIntent {
        root_request_id: root.clone(),
        admitted_request: None,
        scope: crate::account_session_runtime::ConnectionScope {
            account: effect.account.clone(),
            connection: scope.connection.clone(),
            generation: scope.generation,
        },
        operation: operation(kind, &effect.session_id)?,
        actor: Some(crate::account_worker::retained::RequestActor {
            agent: effect.agent.clone(),
            epoch: effect.epoch,
        }),
        transfer: None,
        deleted_actor_session: (kind == OutboxKind::VoiceDelete).then(|| effect.session_id.clone()),
        method: Method::ThreadRealtimeStop,
        params: json!({"threadId":effect.thread}),
    };
    let admission = crate::account_worker::retained::reserve_provider_call(tx, owner, intent)
        .map_err(|error| VoiceProviderError::Provider(Box::new(error)))?;
    let id = Outbox::effect_id(
        owner.id(),
        kind,
        OwnerKind::Agent,
        &effect.agent,
        &[&effect.session_id],
    );
    effect.authority = VoiceEffectAuthority::Admitted {
        scope,
        root_request_id: root,
        owner: owner.clone(),
        admission: admission.clone(),
        outcome: VoiceStopOutcome::NotSent,
    };
    Ok(OutboxRow {
        id,
        kind,
        owner_kind: OwnerKind::Agent,
        owner_id: effect.agent.clone(),
        request_id: Some(admission.id().into()),
        signature: signature(kind)?.into(),
        status: OutboxStatus::Pending,
        account: None,
        stream_id: None,
        generation: None,
        ordinal: None,
        next_at: Some(0),
        payload: effect,
        created: owner.created(),
    })
}

fn new_row(
    tx: &Transaction<'_>,
    effect: VoiceEffect,
    kind: OutboxKind,
) -> Result<OutboxRow<VoiceEffect>, VoiceProviderError> {
    let root = RequestIdentity::issue_root();
    let identity = RequestIdentity::new(
        "voice",
        &[&effect.agent],
        &root,
        &[kind.as_str(), &effect.session_id],
    )
    .map_err(|_| VoiceProviderError::ContextChanged)?;
    let owner = match RequestResults::admit(tx, &identity, signature(kind)?, identity.issued_ms())?
    {
        Admission::Fresh(owner) => owner,
        Admission::Replay(_) => return Err(VoiceProviderError::ContextChanged),
    };
    reserved(tx, effect, kind, root, &owner)
}

fn held_row(
    effect: VoiceEffect,
    kind: OutboxKind,
) -> Result<OutboxRow<VoiceEffect>, VoiceProviderError> {
    let id = Outbox::effect_id(
        "voice-held",
        kind,
        OwnerKind::Agent,
        &effect.agent,
        &[&effect.epoch.to_string(), &effect.session_id],
    );
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| VoiceProviderError::Operation)?
        .as_millis()
        .try_into()
        .map_err(|_| VoiceProviderError::Operation)?;
    Ok(OutboxRow {
        id,
        kind,
        owner_kind: OwnerKind::Agent,
        owner_id: effect.agent.clone(),
        request_id: None,
        signature: signature(kind)?.into(),
        status: OutboxStatus::Unknown,
        account: None,
        stream_id: None,
        generation: None,
        ordinal: None,
        next_at: None,
        payload: effect,
        created,
    })
}

fn remove_unsent(
    tx: &Transaction<'_>,
    row: &OutboxRow<VoiceEffect>,
) -> Result<(), VoiceProviderError> {
    if let VoiceEffectAuthority::Admitted { admission, .. } = &row.payload.authority {
        crate::account_worker::retained::discard_unsent_provider_call(tx, admission)
            .map_err(|error| VoiceProviderError::Provider(Box::new(error)))?;
    }
    Outbox::delete_settled(tx, &row.id, &row.signature)?;
    Ok(())
}

pub(crate) fn stage_ordinary_stop(
    tx: &Transaction<'_>,
    agent_id: &str,
    session_id: &str,
) -> Result<Option<VoiceDeleteIntent>, VoiceProviderError> {
    if existing_stop(tx, agent_id, session_id)? {
        return Ok(None);
    }
    let agent = AgentRepository::get(tx, &AgentId::from(agent_id))?
        .ok_or(VoiceProviderError::ContextUnavailable)?;
    let effect = capture_delete_sessions(tx, agent_id)?
        .into_iter()
        .find(|effect| effect.session_id == session_id)
        .ok_or(VoiceProviderError::ContextUnavailable)?;
    if !current(tx, &agent, &captured(&effect))? {
        let row = held_row(effect, OutboxKind::VoiceStop)?;
        Outbox::put(tx, &row)?;
        tx.execute("UPDATE voice_sessions SET state='unknown',error='Voice stop outcome is unresolved' WHERE id=?1 AND agent=?2", rusqlite::params![session_id,agent_id])?;
        return Ok(None);
    }
    let row = new_row(tx, effect, OutboxKind::VoiceStop)?;
    Outbox::put(tx, &row)?;
    Ok(Some(intent(&row)?))
}

pub(crate) fn existing_stop(
    tx: &Transaction<'_>,
    agent_id: &str,
    session_id: &str,
) -> Result<bool, VoiceProviderError> {
    for row in
        Outbox::for_owner::<VoiceEffect>(tx, OwnerKind::Agent, agent_id, OutboxKind::VoiceStop)?
    {
        if row.payload.session_id == session_id {
            if matches!(
                row.payload.authority,
                VoiceEffectAuthority::Admitted {
                    outcome: VoiceStopOutcome::Expired,
                    ..
                }
            ) {
                return Err(VoiceProviderError::Expired);
            }
            return Ok(true);
        }
    }
    Ok(false)
}

pub(crate) fn stage_voice_stop_sessions(
    tx: &Transaction<'_>,
    agent_id: &str,
    epoch: i64,
) -> Result<(), VoiceProviderError> {
    let agent = AgentRepository::get(tx, &AgentId::from(agent_id))?
        .ok_or(VoiceProviderError::ContextUnavailable)?;
    let existing =
        Outbox::for_owner::<VoiceEffect>(tx, OwnerKind::Agent, agent_id, OutboxKind::VoiceStop)?;
    for mut effect in capture_delete_sessions(tx, agent_id)? {
        if let Some(old) = existing
            .iter()
            .find(|row| row.payload.session_id == effect.session_id)
        {
            if old.payload.epoch >= epoch
                || !matches!(
                    old.payload.authority,
                    VoiceEffectAuthority::Admitted {
                        outcome: VoiceStopOutcome::NotSent,
                        ..
                    }
                )
            {
                continue;
            }
            remove_unsent(tx, old)?;
        }
        effect.epoch = epoch;
        let row = if current(tx, &agent, &captured(&effect))? {
            new_row(tx, effect, OutboxKind::VoiceStop)?
        } else {
            held_row(effect, OutboxKind::VoiceStop)?
        };
        Outbox::put(tx, &row)?;
    }
    Ok(())
}

pub fn stage_delete_sessions(
    tx: &Transaction<'_>,
    agent_id: &str,
) -> Result<Vec<VoiceDeleteIntent>, VoiceProviderError> {
    let agent = AgentRepository::get(tx, &AgentId::from(agent_id))?
        .ok_or(VoiceProviderError::ContextUnavailable)?;
    let existing =
        Outbox::for_owner::<VoiceEffect>(tx, OwnerKind::Agent, agent_id, OutboxKind::VoiceStop)?;
    let mut intents = Vec::new();
    for mut effect in capture_delete_sessions(tx, agent_id)? {
        if let Some(old) = existing
            .iter()
            .find(|row| row.payload.session_id == effect.session_id)
        {
            if !current(tx, &agent, &captured(&old.payload))? {
                if let VoiceEffectAuthority::Admitted {
                    admission,
                    outcome: VoiceStopOutcome::NotSent,
                    ..
                } = &old.payload.authority
                {
                    crate::account_worker::retained::discard_unsent_provider_call(tx, admission)
                        .map_err(|error| VoiceProviderError::Provider(Box::new(error)))?;
                }
                RequestResults::settle_abandoned(tx, old)?;
                continue;
            }
            if matches!(
                old.payload.authority,
                VoiceEffectAuthority::Admitted {
                    outcome: VoiceStopOutcome::NotSent | VoiceStopOutcome::Rejected,
                    ..
                }
            ) {
                remove_unsent(tx, old)?;
            } else {
                let mut held = old.clone();
                held.payload.disposition = VoiceEffectDisposition::Deleted { epoch: agent.epoch };
                Outbox::put(tx, &held)?;
                continue;
            }
        }
        effect.epoch = agent.epoch;
        effect.disposition = VoiceEffectDisposition::Deleted { epoch: agent.epoch };
        if !current(tx, &agent, &captured(&effect))? {
            let row = held_row(effect, OutboxKind::VoiceDelete)?;
            Outbox::put(tx, &row)?;
            RequestResults::settle_abandoned(tx, &row)?;
            continue;
        }
        let row = new_row(tx, effect, OutboxKind::VoiceDelete)?;
        Outbox::put(tx, &row)?;
        intents.push(intent(&row)?);
    }
    Ok(intents)
}

fn intent(row: &OutboxRow<VoiceEffect>) -> Result<VoiceDeleteIntent, VoiceProviderError> {
    let VoiceEffectAuthority::Admitted { scope, .. } = &row.payload.authority else {
        return Err(VoiceProviderError::ContextUnavailable);
    };
    Ok(VoiceDeleteIntent {
        id: row.id.clone(),
        session: VoiceDeleteSession {
            session_id: row.payload.session_id.clone(),
            agent_id: row.payload.agent.clone(),
            epoch: row.payload.epoch,
            account_key: row.payload.account.clone(),
            thread_id: row.payload.thread.clone(),
            connection_id: scope.connection.clone(),
            generation: scope.generation,
        },
    })
}

pub fn deleted_voice_sessions(
    connection: &Connection,
    agent: &str,
    epoch: i64,
) -> Result<Vec<VoiceDeleteSession>, VoiceProviderError> {
    Outbox::for_owner::<VoiceEffect>(connection, OwnerKind::Agent, agent, OutboxKind::VoiceDelete)?
        .iter()
        .filter(|row| {
            row.payload.epoch == epoch
                && matches!(row.payload.authority, VoiceEffectAuthority::Admitted { .. })
        })
        .map(|row| intent(row).map(|intent| intent.session))
        .collect()
}
pub async fn pending_delete_intents(
    store: &RuntimeStore,
) -> Result<Vec<VoiceDeleteIntent>, VoiceProviderError> {
    pending_voice_intents(store, OutboxKind::VoiceDelete).await
}
pub(crate) async fn pending_voice_intent(
    store: &RuntimeStore,
    id: String,
) -> Result<Option<VoiceDeleteIntent>, VoiceProviderError> {
    store
        .read_async("runtime-services/voice/effects", move |connection| {
            let row = Outbox::get::<VoiceEffect>(connection, &id)?;
            row.filter(|row| row.status == OutboxStatus::Pending)
                .map(|row| intent(&row))
                .transpose()
        })
        .await
}
pub(crate) async fn pending_voice_intents(
    store: &RuntimeStore,
    kind: OutboxKind,
) -> Result<Vec<VoiceDeleteIntent>, VoiceProviderError> {
    store
        .read_async("runtime-services/voice/effects", move |connection| {
            Outbox::pending::<VoiceEffect>(connection, kind, i64::MAX, PENDING_BATCH)?
                .iter()
                .map(intent)
                .collect()
        })
        .await
}

pub fn settle_voice_stop(
    tx: &Transaction<'_>,
    call: &crate::account_worker::retained::ProviderCallIntent,
    result: &crate::account_worker::retained::RequestResult,
) -> Result<(), VoiceProviderError> {
    use crate::account_worker::retained::RequestResult;
    let next = match result {
        RequestResult::Reply(_) => VoiceStopOutcome::Written,
        RequestResult::Rejected(_) => VoiceStopOutcome::Rejected,
        RequestResult::NotSent => VoiceStopOutcome::NotSent,
        RequestResult::Unknown => VoiceStopOutcome::Unknown,
    };
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default();
    settle_outcome(tx, call, next, now)
}

fn settle_outcome(
    tx: &Transaction<'_>,
    call: &crate::account_worker::retained::ProviderCallIntent,
    next: VoiceStopOutcome,
    now: f64,
) -> Result<(), VoiceProviderError> {
    if call.method != Method::ThreadRealtimeStop {
        return Ok(());
    }
    let Some(actor) = &call.actor else {
        return Ok(());
    };
    let kind = if call.operation.starts_with("voice:delete-stop:") {
        OutboxKind::VoiceDelete
    } else if call.operation.starts_with("voice:stop:") {
        OutboxKind::VoiceStop
    } else {
        return Ok(());
    };
    let thread = call
        .params
        .as_object()
        .filter(|params| params.len() == 1)
        .and_then(|params| params.get("threadId"))
        .and_then(Value::as_str)
        .ok_or(VoiceProviderError::ContextChanged)?;
    for mut row in Outbox::for_owner::<VoiceEffect>(tx, OwnerKind::Agent, &actor.agent, kind)? {
        let VoiceEffectAuthority::Admitted {
            scope,
            root_request_id,
            owner,
            admission,
            outcome,
        } = &mut row.payload.authority
        else {
            continue;
        };
        if row.payload.epoch != actor.epoch
            || row.payload.account != call.scope.account
            || scope.connection != call.scope.connection
            || scope.generation != call.scope.generation
            || operation(kind, &row.payload.session_id)? != call.operation
            || call.admitted_request.as_ref() != Some(admission)
            || thread != row.payload.thread
        {
            continue;
        }
        if *outcome == VoiceStopOutcome::Expired
            && !matches!(next, VoiceStopOutcome::Written | VoiceStopOutcome::Rejected)
        {
            continue;
        }
        if kind == OutboxKind::VoiceStop {
            match next {
                VoiceStopOutcome::Written => {
                    tx.execute("UPDATE voice_sessions SET state='ended',ended=COALESCE(ended,?3) WHERE id=?1 AND agent=?2 AND account_key=?4 AND native_thread=?5 AND connection_id=?6", rusqlite::params![row.payload.session_id,row.payload.agent,now,row.payload.account,row.payload.thread,scope.connection])?;
                }
                VoiceStopOutcome::Unknown | VoiceStopOutcome::Rejected => {
                    tx.execute("UPDATE voice_sessions SET state='unknown',error='Voice stop outcome is unresolved' WHERE id=?1 AND agent=?2 AND account_key=?3 AND native_thread=?4 AND connection_id=?5", rusqlite::params![row.payload.session_id,row.payload.agent,row.payload.account,row.payload.thread,scope.connection])?;
                }
                VoiceStopOutcome::Expired => {
                    tx.execute("UPDATE voice_sessions SET state='failed',error='Request id expired' WHERE id=?1 AND agent=?2", rusqlite::params![row.payload.session_id,row.payload.agent])?;
                }
                VoiceStopOutcome::NotSent | VoiceStopOutcome::Submitted => {}
            }
        }
        if kind == OutboxKind::VoiceStop
            && matches!(next, VoiceStopOutcome::NotSent | VoiceStopOutcome::Rejected)
            && let VoiceEffectDisposition::Deleted { epoch } = row.payload.disposition
        {
            let root = root_request_id.clone();
            let owner = owner.clone();
            let scope = scope.clone();
            let scope_current =
                match AgentRepository::get(tx, &AgentId::from(row.payload.agent.as_str()))? {
                    Some(agent) => current(tx, &agent, &CapturedScope::Known(scope.clone()))?,
                    None => false,
                };
            if !scope_current {
                crate::account_worker::retained::discard_unsent_provider_call(tx, admission)
                    .map_err(|error| VoiceProviderError::Provider(Box::new(error)))?;
                RequestResults::settle_abandoned(tx, &row)?;
                continue;
            }
            remove_unsent(tx, &row)?;
            row.payload.epoch = epoch;
            row.payload.authority = VoiceEffectAuthority::Held {
                scope: CapturedScope::Known(scope),
            };
            let replacement = reserved(tx, row.payload, OutboxKind::VoiceDelete, root, &owner)?;
            Outbox::put(tx, &replacement)?;
        } else if matches!(next, VoiceStopOutcome::Written | VoiceStopOutcome::Rejected) {
            Outbox::delete_settled(tx, &row.id, &row.signature)?;
        } else {
            *outcome = next;
            row.status = if next == VoiceStopOutcome::NotSent {
                OutboxStatus::Pending
            } else {
                OutboxStatus::Unknown
            };
            row.next_at = (row.status == OutboxStatus::Pending).then_some(0);
            Outbox::put(tx, &row)?;
        }
    }
    Ok(())
}

pub(crate) async fn end_voice_sessions_tracked(
    store: Arc<RuntimeStore>,
    provider: Arc<dyn VoiceRequestPort>,
    id: String,
    kind: OutboxKind,
    session: VoiceDeleteSession,
    now: f64,
) -> Result<Vec<VoiceWriteCertainty>, VoiceProviderError> {
    if !now.is_finite() || now < 0.0 {
        return Err(super::policy::VoiceError::InvalidClock.into());
    }
    let claim_id = id.clone();
    let session_id = session.session_id.clone();
    let claim = store
        .transaction_with_error(move |tx| -> Result<_, VoiceProviderError> {
            let Some(mut row) = Outbox::get::<VoiceEffect>(tx, &claim_id)? else {
                return Ok(None);
            };
            if row.kind != kind
                || row.status != OutboxStatus::Pending
                || row.payload.session_id != session_id
            {
                return Ok(None);
            }
            let VoiceEffectAuthority::Admitted {
                scope,
                root_request_id,
                admission,
                outcome,
                ..
            } = &mut row.payload.authority
            else {
                return Err(VoiceProviderError::ContextChanged);
            };
            let call = VoiceRpcCall {
                operation_id: operation(kind, &row.payload.session_id)?,
                root_request_id: root_request_id.clone(),
                admitted_request: Some(admission.clone()),
                context: VoiceConnectionScope {
                    agent_id: row.payload.agent.clone(),
                    epoch: row.payload.epoch,
                    account_key: row.payload.account.clone(),
                    thread_id: row.payload.thread.clone(),
                    connection_id: scope.connection.clone(),
                    generation: scope.generation,
                },
                method: Method::ThreadRealtimeStop,
                params: json!({"threadId":row.payload.thread}),
                deadline: request_deadline(),
            };
            *outcome = VoiceStopOutcome::Submitted;
            row.status = OutboxStatus::Unknown;
            row.next_at = None;
            Outbox::put(tx, &row)?;
            Ok(Some(call))
        })
        .await?;
    let Some(call) = claim else {
        return Ok(Vec::new());
    };
    let receipt = request(provider.as_ref(), call.clone()).await;
    let result = match receipt.certainty {
        VoiceWriteCertainty::Written => VoiceStopOutcome::Written,
        VoiceWriteCertainty::Rejected => VoiceStopOutcome::Rejected,
        VoiceWriteCertainty::NotSent => VoiceStopOutcome::NotSent,
        VoiceWriteCertainty::Unknown => VoiceStopOutcome::Unknown,
        VoiceWriteCertainty::Expired => VoiceStopOutcome::Expired,
    };
    let intent = crate::account_worker::retained::ProviderCallIntent {
        root_request_id: call.root_request_id,
        admitted_request: call.admitted_request,
        scope: crate::account_session_runtime::ConnectionScope {
            account: call.context.account_key,
            connection: call.context.connection_id,
            generation: call.context.generation,
        },
        operation: call.operation_id,
        actor: Some(crate::account_worker::retained::RequestActor {
            agent: call.context.agent_id,
            epoch: call.context.epoch,
        }),
        transfer: None,
        deleted_actor_session: (kind == OutboxKind::VoiceDelete).then_some(session.session_id),
        method: call.method,
        params: call.params,
    };
    store
        .transaction_with_error(move |tx| settle_outcome(tx, &intent, result, now))
        .await?;
    Ok(vec![receipt.certainty])
}

pub async fn end_deleted_sessions_tracked(
    store: Arc<RuntimeStore>,
    provider: Arc<dyn VoiceRequestPort>,
    id: String,
    session: VoiceDeleteSession,
    now: f64,
) -> Result<(), VoiceProviderError> {
    end_voice_sessions_tracked(store, provider, id, OutboxKind::VoiceDelete, session, now)
        .await
        .map(|_| ())
}
