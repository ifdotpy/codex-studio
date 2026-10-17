//! Transactional lifecycle consumer; injected effects share this writer transaction.
use crate::agent_management::{
    Action, ManagementError, ManagementSnapshot, Records, decide, decide_with_effects,
};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::collections::BTreeMap;
use studio_runtime_store::{
    ProjectsRepository, RuntimeStore, Transaction, agent_management as repository,
};

/// Maximum complete lifecycle preparation attempts before returning a conflict.
/// Unit: attempts (including the first capture).
const MAX_PREPARED_INPUT_ATTEMPTS: usize = 3;

/// HTTP stop/delete and the tool dispatcher's archive/restore share this entry.
/// Preconditions, decision, writes and renderer changes commit together. No
/// caller can replay an unknown external effect because none is submitted here.
pub async fn execute(
    store: std::sync::Arc<RuntimeStore>,
    action: &Action,
    now: f64,
) -> Result<Value, ManagementError> {
    let mut result =
        execute_prepared(std::sync::Arc::clone(&store), action, now, None, || {}).await?;
    if matches!(action, Action::Delete { .. } | Action::Archive { .. }) {
        crate::workspace_cleanup_runtime::reconcile_pending_cleanup(std::sync::Arc::clone(&store))
            .await
            .map_err(|_| {
                studio_runtime_store::StoreError::RecordContract(
                    "Lifecycle committed; workspace cleanup remains pending",
                )
            })?;
        if let Some(agent_id) = result
            .get("agent")
            .and_then(|agent| agent.get("id"))
            .and_then(Value::as_str)
        {
            let read_store = std::sync::Arc::clone(&store);
            let agent_id = agent_id.to_owned();
            let agent_archive = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/agent_management/archive_readback",
                move || {
                    let reader = read_store.read()?;
                    Ok::<_, studio_runtime_store::StoreError>(
                        studio_runtime_store::AgentRepository::get(
                            &reader,
                            &studio_runtime_store::AgentId::from(agent_id),
                        )?
                        .and_then(|agent| agent.value().get("agentArchive").cloned()),
                    )
                },
            )
            .await
            .map_err(|_| {
                studio_runtime_store::StoreError::RecordContract(
                    "Agent archive readback worker failed",
                )
            })??;
            if let Some(agent_archive) = agent_archive {
                result["agent"]["agentArchive"] = agent_archive;
            }
        }
    }
    Ok(result)
}

pub(crate) async fn execute_prepared(
    store: std::sync::Arc<RuntimeStore>,
    action: &Action,
    now: f64,
    ports: Option<std::sync::Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>>,
    mut on_attempt_boundary: impl FnMut() + Send,
) -> Result<Value, ManagementError> {
    for attempt_index in 0..MAX_PREPARED_INPUT_ATTEMPTS {
        if attempt_index + 1 == MAX_PREPARED_INPUT_ATTEMPTS {
            // The final bounded capture stays inside the sync actor callback.
            // This preserves the original guarantee that a continuously
            // changing database cannot invalidate every filesystem-prepared
            // snapshot before the lifecycle transaction gets writer access.
            on_attempt_boundary();
            let action = action.clone();
            let ports = ports.clone();
            let state_dir = store.root().to_path_buf();
            let outcome = store
                .transaction_with_error(move |transaction| {
                    let rooms = crate::sync_entity_context::RoomDirectory::load(transaction)?;
                    let projects = ProjectsRepository::list(transaction)?;
                    let vault_snapshot =
                        crate::sync_policy::ProjectVaultSnapshot::capture(&state_dir, &projects);
                    match ports.as_deref() {
                        Some(ports) => crate::agent_lifecycle_runtime::execute_in_transaction(
                            transaction,
                            &action,
                            now,
                            ports,
                            &rooms,
                            &vault_snapshot,
                        ),
                        None => execute_in_transaction(
                            transaction,
                            &action,
                            now,
                            &rooms,
                            &vault_snapshot,
                        ),
                    }
                })
                .await;
            return match outcome {
                Err(ManagementError::Store(studio_runtime_store::StoreError::RecordContract(
                    "agent management inputs changed after filesystem preparation",
                ))) => Err(ManagementError::Invalid(
                    "Agent state kept changing while preparing stop; reload and retry",
                )),
                result => result,
            };
        }

        let capture_store = std::sync::Arc::clone(&store);
        let state_dir = store.root().to_path_buf();
        let (rooms, vault_snapshot) = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/agent_management/prepared_inputs",
            move || {
                let reader = capture_store.read()?;
                let rooms = crate::sync_entity_context::RoomDirectory::prepare(&reader)?;
                let projects = ProjectsRepository::list(&reader)?;
                let vault_snapshot =
                    crate::sync_policy::ProjectVaultSnapshot::capture(&state_dir, &projects);
                drop(reader);
                Ok::<_, studio_runtime_store::StoreError>((rooms, vault_snapshot))
            },
        )
        .await
        .map_err(|_| {
            studio_runtime_store::StoreError::RecordContract(
                "Agent management input preparation worker failed",
            )
        })??;
        on_attempt_boundary();

        let action = action.clone();
        let ports = ports.clone();
        let attempt = store
            .transaction_with_error(move |transaction| match ports.as_deref() {
                Some(ports) => crate::agent_lifecycle_runtime::execute_in_transaction(
                    transaction,
                    &action,
                    now,
                    ports,
                    &rooms,
                    &vault_snapshot,
                ),
                None => execute_in_transaction(transaction, &action, now, &rooms, &vault_snapshot),
            })
            .await;
        match attempt {
            Err(ManagementError::Store(studio_runtime_store::StoreError::RecordContract(
                "agent management inputs changed after filesystem preparation",
            ))) => {
                // Repeat only the read-only store/filesystem capture. The
                // rejected transaction detected the changed snapshot before
                // any writes or lifecycle effects were staged.
            }
            result => return result,
        }
    }
    unreachable!("the final prepared-input attempt returns from its writer transaction")
}

/// Archive a worker after its accepted result has been merged and verified on
/// main by the work acceptance service. The caller owns that repository check;
/// this function performs the shared authorized lifecycle transaction.
pub async fn archive_accepted_result(
    store: std::sync::Arc<RuntimeStore>,
    actor: &str,
    epoch: i64,
    target: &str,
    now: f64,
) -> Result<Value, ManagementError> {
    let result = execute(
        std::sync::Arc::clone(&store),
        &Action::Archive {
            actor: actor.to_owned(),
            epoch,
            target: target.to_owned(),
            reason: "Accepted task result is on main".to_owned(),
            unassign_work: false,
        },
        now,
    )
    .await?;
    crate::workspace_cleanup_runtime::reconcile_pending_cleanup(std::sync::Arc::clone(&store))
        .await
        .map_err(|_| {
            studio_runtime_store::StoreError::RecordContract(
                "Archive committed; workspace cleanup remains pending",
            )
        })?;
    Ok(result)
}

pub fn execute_in_transaction(
    transaction: &Transaction<'_>,
    action: &Action,
    now: f64,
    rooms: &crate::sync_entity_context::RoomDirectory,
    vault_snapshot: &crate::sync_policy::ProjectVaultSnapshot,
) -> Result<Value, ManagementError> {
    execute_decision(transaction, action, now, rooms, vault_snapshot, None)
}

pub(crate) fn execute_with_ports(
    transaction: &Transaction<'_>,
    action: &Action,
    now: f64,
    rooms: &crate::sync_entity_context::RoomDirectory,
    vault_snapshot: &crate::sync_policy::ProjectVaultSnapshot,
    ports: &dyn crate::agent_lifecycle_runtime::LifecyclePorts,
) -> Result<Value, ManagementError> {
    execute_decision(transaction, action, now, rooms, vault_snapshot, Some(ports))
}

fn execute_decision(
    transaction: &Transaction<'_>,
    action: &Action,
    now: f64,
    rooms: &crate::sync_entity_context::RoomDirectory,
    vault_snapshot: &crate::sync_policy::ProjectVaultSnapshot,
    ports: Option<&dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
) -> Result<Value, ManagementError> {
    if !rooms.matches(transaction)? {
        return Err(studio_runtime_store::StoreError::RecordContract(
            "agent management inputs changed after filesystem preparation",
        )
        .into());
    }
    if let Action::RemoteStop {
        principal,
        link,
        target,
        ..
    } = action
    {
        let raw: Option<String> = transaction
            .query_row(
                "SELECT record FROM runtime_server_links WHERE id=?1",
                [link],
                |row| row.get(0),
            )
            .optional()
            .map_err(studio_runtime_store::StoreError::from)?;
        let link_record: Value = raw
            .as_deref()
            .and_then(|raw| serde_json::from_str(raw).ok())
            .ok_or(ManagementError::Invalid(
                "The remote-parent link is unavailable",
            ))?;
        if link_record.get("side").and_then(Value::as_str) != Some("remote")
            || link_record.get("home").and_then(Value::as_str) != Some(principal)
            || !link_record
                .get("workers")
                .and_then(Value::as_array)
                .is_some_and(|workers| workers.iter().any(|worker| worker.as_str() == Some(target)))
        {
            return Err(ManagementError::Invalid(
                "The remote-parent link is invalid",
            ));
        }
    }
    // Every action decides only from the complete agent directory and the
    // history owned by the target's descendant tree. Reading unrelated task
    // history held the sole writer for seconds on large state directories.
    let tables = {
        let mut agents = BTreeMap::new();
        agents.insert(
            "runtime_agents".to_owned(),
            repository::read_agent_directory(transaction)?,
        );
        let directory = ManagementSnapshot::from_records(Records { tables: agents })?;
        let target = action.target();
        // An unknown target keeps the decision's own refusal and its ordering.
        let owners = directory
            .owner_scope(target)
            .unwrap_or_else(|_| std::collections::BTreeSet::from([target.to_owned()]));
        repository::read_for_agents(transaction, Some(&owners))?
    };
    let records = Records { tables };
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
            let context = rooms.for_room(transaction, &room, true)?;
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
        repository::settle_inputs(transaction, agent)?;
    }
    if let Some(ports) = ports {
        for effect in &decision.effects {
            ports.stage(transaction, effect)?;
        }
    }
    if matches!(action, Action::Delete { .. }) {
        let deleted = decision.result["deleted"]
            .as_array()
            .ok_or(ManagementError::Invalid("Invalid deletion decision"))?;
        for agent in deleted.iter().filter_map(Value::as_str) {
            if let Some(ports) = ports {
                // Save exact provider/session identity before deleting the
                // voice rows; committed() dispatches this durable intent later.
                ports.stage_voice_delete(transaction, agent)?;
            }
            studio_runtime_store::VoiceRepository::delete_agent(transaction, agent)?;
        }
    }
    if ports.is_some() {
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
            // Interrupt preparation is the one permitted amendment to the
            // saved attempt: it durably records the ticket before transport.
            // All other attempt fields still match the lifecycle decision.
            if !same_attempt_except_interrupt(
                current.get("startAttempt").unwrap_or(&Value::Null),
                write.record.get("startAttempt").unwrap_or(&Value::Null),
            ) {
                return Err(ManagementError::Invalid(
                    "Lifecycle adapter changed stop identity",
                ));
            }
        }
    }
    // Reuse the canonical views, including room membership and tombstones.
    let state_dir = transaction
        .path()
        .and_then(|p| std::path::Path::new(p).parent())
        .and_then(|p| p.to_str())
        .unwrap_or_default();
    crate::sync_policy::maintain_entity_projections(
        transaction,
        state_dir,
        vault_snapshot,
        rooms,
        |_, _, _, _| true,
    )?;
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

fn same_attempt_except_interrupt(current: &Value, expected: &Value) -> bool {
    let expected_had_interrupt = expected
        .as_object()
        .is_some_and(|object| object.contains_key("interrupt"));
    let current_had_interrupt = current
        .as_object()
        .is_some_and(|object| object.contains_key("interrupt"));
    if expected_had_interrupt && !current_had_interrupt {
        return false;
    }
    let mut current = current.clone();
    let mut expected = expected.clone();
    if let Some(object) = current.as_object_mut() {
        object.remove("interrupt");
    }
    if let Some(object) = expected.as_object_mut() {
        object.remove("interrupt");
    }
    current == expected
}

#[cfg(test)]
mod lifecycle_identity_tests {
    use super::same_attempt_except_interrupt;
    use serde_json::json;

    #[test]
    fn lifecycle_guard_allows_only_interrupt_intent_change() {
        let expected = json!({"id":"attempt","submitted":true,"turnId":"turn"});
        let prepared = json!({"id":"attempt","submitted":true,"turnId":"turn","interrupt":{"requestTicket":"ticket","outcome":"unknown"}});
        assert!(same_attempt_except_interrupt(&prepared, &expected));

        let updated = json!({"id":"attempt","submitted":true,"turnId":"turn","interrupt":{"requestTicket":"new-ticket","outcome":"unknown"}});
        assert!(same_attempt_except_interrupt(&updated, &prepared));
        assert!(!same_attempt_except_interrupt(&expected, &prepared));

        let modified = json!({"id":"other-attempt","submitted":true,"turnId":"turn","interrupt":{"requestTicket":"ticket","outcome":"unknown"}});
        assert!(!same_attempt_except_interrupt(&modified, &expected));
    }
}
