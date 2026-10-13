//! Transactional lifecycle consumer; injected effects share this writer transaction.
use crate::agent_management::{
    Action, ManagementError, ManagementSnapshot, Records, decide, decide_with_effects,
};
use rusqlite::OptionalExtension;
use serde_json::Value;
use studio_runtime_store::{RuntimeStore, Transaction, agent_management as repository};

/// HTTP stop/delete and the tool dispatcher's archive/restore share this entry.
/// Preconditions, decision, writes and renderer changes commit together. No
/// caller can replay an unknown external effect because none is submitted here.
pub fn execute(store: &RuntimeStore, action: &Action, now: f64) -> Result<Value, ManagementError> {
    store.transaction_with_error(|transaction| execute_in_transaction(transaction, action, now))
}

pub fn execute_in_transaction(
    transaction: &Transaction<'_>,
    action: &Action,
    now: f64,
) -> Result<Value, ManagementError> {
    execute_decision(transaction, action, now, None)
}

pub(crate) fn execute_with_ports(
    transaction: &Transaction<'_>,
    action: &Action,
    now: f64,
    ports: &dyn crate::agent_lifecycle_runtime::LifecyclePorts,
) -> Result<Value, ManagementError> {
    execute_decision(transaction, action, now, Some(ports))
}

fn execute_decision(
    transaction: &Transaction<'_>,
    action: &Action,
    now: f64,
    ports: Option<&dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
) -> Result<Value, ManagementError> {
    let records = Records {
        tables: repository::read(transaction)?,
    };
    let snapshot = ManagementSnapshot::from_records(records.clone())?;
    let decision = if ports.is_some() {
        decide_with_effects(&snapshot, action, now)?
    } else {
        decide(&snapshot, action, now)?
    };
    if decision.writes.is_empty() && decision.cancel_pending.is_empty() {
        return Ok(decision.result);
    }
    let mut fallback_rooms = Vec::new();
    // Python retains a deleted team's broadcast projection, including on later
    // unrelated writes. Preserve those views through the shared rebuild.
    for (_, raw) in crate::sync_policy::stored_records(transaction, "runtime_rooms")? {
        let room: Value =
            serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
        let deleted_root = records
            .tables
            .get("runtime_agents")
            .into_iter()
            .flatten()
            .any(|agent| {
                agent.get("id") == room.get("rootId")
                    && crate::event_write::truthy(agent.get("deletedAt"))
            });
        if room.get("kind").and_then(Value::as_str) != Some("broadcast") || !deleted_root {
            continue;
        }
        let id = room["id"]
            .as_str()
            .ok_or(ManagementError::Invalid("Invalid room identity"))?;
        let prior: Option<String> = transaction
            .query_row(
                "SELECT payload FROM sync_entities WHERE collection='room' AND id=?1 AND deleted=0",
                [id],
                |row| row.get(0),
            )
            .optional()
            .map_err(studio_runtime_store::StoreError::from)?;
        if let Some(prior) = prior {
            let payload: Value =
                serde_json::from_str(&prior).map_err(studio_runtime_store::StoreError::from)?;
            if let Some(view) = payload.get("value") {
                fallback_rooms.push((id.to_owned(), view.clone()));
            }
        }
    }
    if let Action::Delete { .. } = action {
        let deleted = decision.result["deleted"]
            .as_array()
            .ok_or(ManagementError::Invalid("Invalid deletion decision"))?;
        let room_directory = crate::sync_entity_context::RoomDirectory::load(transaction)?;
        for (_, raw) in crate::sync_policy::stored_records(transaction, "runtime_rooms")? {
            let room: Value =
                serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
            if room.get("kind").and_then(Value::as_str) != Some("broadcast")
                || !room
                    .get("rootId")
                    .is_some_and(|root| deleted.contains(root))
            {
                continue;
            }
            let context = room_directory.for_room(transaction, &room, true)?;
            if let Some(mut view) = crate::sync_views::room_entity_view(&room, &context) {
                if let Some(members) = view.get_mut("members").and_then(Value::as_array_mut) {
                    members.retain(|member| {
                        !deleted.contains(member) || Some(member) == room.get("rootId")
                    });
                }
                fallback_rooms.push((
                    room["id"]
                        .as_str()
                        .ok_or(ManagementError::Invalid("Invalid room identity"))?
                        .to_owned(),
                    view,
                ));
            }
        }
    }
    for write in &decision.writes {
        repository::write(transaction, &write.table, &write.record)?;
    }
    for agent in &decision.cancel_pending {
        repository::cancel_pending(transaction, agent)?;
    }
    if let Some(ports) = ports {
        for effect in &decision.effects {
            ports.stage(transaction, effect)?;
        }
        // An adapter cannot resume or replace an agent stopped by this action.
        for write in &decision.writes {
            if write.table != "runtime_agents" {
                continue;
            }
            let id = write.record["id"]
                .as_str()
                .ok_or(ManagementError::Invalid("Invalid stop identity"))?;
            let current = studio_runtime_store::AgentRepository::get(
                transaction,
                &studio_runtime_store::AgentId::from(id),
            )?
            .ok_or(ManagementError::Invalid(
                "Lifecycle adapter removed stopped agent",
            ))?;
            let current =
                serde_json::to_value(current).map_err(studio_runtime_store::StoreError::from)?;
            for field in [
                "epoch",
                "autoWake",
                "status",
                "deletedAt",
                "startAttempt",
                "threadId",
                "accountKey",
                "connectionId",
                "connectionGeneration",
                "provider",
            ] {
                if current.get(field) != write.record.get(field) {
                    return Err(ManagementError::Invalid(
                        "Lifecycle adapter changed stop identity",
                    ));
                }
            }
        }
    }
    // Reuse the canonical views, including room membership and tombstones.
    let state_dir = transaction
        .path()
        .and_then(|p| std::path::Path::new(p).parent())
        .and_then(|p| p.to_str())
        .unwrap_or_default();
    crate::sync_policy::maintain_entity_projections(transaction, state_dir, |_, _, _, _| true)?;
    for (id, view) in fallback_rooms {
        studio_runtime_store::SyncDocumentsRepository::put_entity(
            transaction,
            "room",
            &id,
            view,
            false,
        )?;
    }
    Ok(decision.result)
}
