//! Transactional lifecycle consumer; injected effects share this writer transaction.
use crate::agent_management::{
    Action, ManagementError, ManagementSnapshot, Records, decide, decide_with_effects,
};
use rusqlite::OptionalExtension;
use serde_json::Value;
use std::collections::BTreeMap;
use studio_runtime_store::{RuntimeStore, Transaction, agent_management as repository};

/// HTTP stop/delete and the tool dispatcher's archive/restore share this entry.
/// Preconditions, decision, writes and renderer changes commit together. No
/// caller can replay an unknown external effect because none is submitted here.
pub async fn execute(
    store: std::sync::Arc<RuntimeStore>,
    action: &Action,
    now: f64,
) -> Result<Value, ManagementError> {
    let mut result = execute_prepared(std::sync::Arc::clone(&store), action, now, None).await?;
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
            let agent_id = studio_runtime_store::AgentId::from(agent_id);
            let agent_archive = store
                .read_async(
                    "runtime-services/agent_management/archive_readback",
                    move |tx| {
                        Ok::<_, studio_runtime_store::StoreError>(
                            studio_runtime_store::AgentRepository::get(tx, &agent_id)?
                                .and_then(|agent| agent.value().get("agentArchive").cloned()),
                        )
                    },
                )
                .await?;
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
) -> Result<Value, ManagementError> {
    let action = action.clone();
    store
        .transaction_with_error(move |transaction| {
            execute_decision(transaction, &action, now, ports.as_deref())
        })
        .await
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
    let generation = crate::sync_policy::agent_record_generation(transaction)?;
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
    // Every action decides from its target's descendant tree, the target's
    // ancestor chain and the acting agent. Reading every agent record held the
    // sole writer for about a second on a state with thousands of tombstones.
    let tables = {
        let topology = repository::read_agent_topology(transaction)?;
        let (owners, agents) = lifecycle_scope(&topology, action);
        let mut tables = repository::read_for_agents(transaction, Some(&owners))?;
        tables.insert(
            "runtime_agents".to_owned(),
            repository::read_agents(transaction, &agents)?,
        );
        tables
    };
    let snapshot = ManagementSnapshot::from_records(Records { tables })?;
    let decision = if ports.is_some() {
        decide_with_effects(&snapshot, action, now)?
    } else {
        decide(&snapshot, action, now)?
    };
    if decision.writes.is_empty() && decision.cancel_pending.is_empty() {
        return Ok(decision.result);
    }
    // Python keeps a deleted team's broadcast room, listing only the root.
    // Build that view from the pre-delete directory before the rows change.
    let mut retained_broadcasts = Vec::new();
    if let Action::Delete { .. } = action {
        let deleted = decision.result["deleted"]
            .as_array()
            .ok_or(ManagementError::Invalid("Invalid deletion decision"))?;
        let mut directory = None;
        // The runtime_room_root index serves this lookup.
        let mut statement = transaction
            .prepare_cached(
                "SELECT record FROM runtime_rooms WHERE json_extract(record,'$.rootId')=?1 \
                 AND json_extract(record,'$.kind')='broadcast' ORDER BY rowid",
            )
            .map_err(studio_runtime_store::StoreError::from)?;
        for root in deleted.iter().filter_map(Value::as_str) {
            let rooms = statement
                .query_map([root], |row| row.get::<_, String>(0))
                .and_then(Iterator::collect::<Result<Vec<_>, _>>)
                .map_err(studio_runtime_store::StoreError::from)?;
            for raw in rooms {
                let room: Value =
                    serde_json::from_str(&raw).map_err(studio_runtime_store::StoreError::from)?;
                if directory.is_none() {
                    directory = Some(crate::sync_entity_context::RoomDirectory::load_live(
                        transaction,
                    )?);
                }
                let context = directory
                    .as_ref()
                    .expect("directory loaded above")
                    .for_room(transaction, &room, true)?;
                if let Some(mut view) = crate::sync_views::room_entity_view(&room, &context) {
                    if let Some(members) = view.get_mut("members").and_then(Value::as_array_mut) {
                        members.retain(|member| !deleted.contains(member) || *member == root);
                    }
                    let id = room["id"]
                        .as_str()
                        .ok_or(ManagementError::Invalid("Invalid room identity"))?;
                    retained_broadcasts.push((id.to_owned(), view));
                }
            }
        }
    }
    // Release effects clear the owner of these rows, so capture them first.
    let mut changed_work = written_ids(&decision.writes, "runtime_work");
    changed_work.extend(work_owned_by(
        transaction,
        &written_ids(&decision.writes, "runtime_agents"),
    )?);
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
    // Publish only the changed rows and the views that depend on them.
    let changes = crate::sync_policy::EntityChanges {
        agents: crate::sync_policy::agents_changed_since(transaction, generation)?
            .into_iter()
            .map(|(_, id, _)| id)
            .collect(),
        requests: written_ids(&decision.writes, "runtime_requests"),
        work: changed_work,
        checked_agents: Vec::new(),
    };
    let server_id = RuntimeStore::transaction_local_server_id(transaction)?;
    crate::sync_policy::publish_entity_changes(
        transaction,
        &changes,
        server_id.as_deref(),
        &|_, _, _, _| true,
    )?;
    for (id, view) in retained_broadcasts {
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

/// Descendant tree of the action target (the owners whose history is read) and
/// the agents whose records the decision needs: that tree, the target's
/// ancestor chain and the acting agent. An unknown target scopes to itself.
fn lifecycle_scope(
    topology: &[(String, Option<String>)],
    action: &Action,
) -> (
    std::collections::BTreeSet<String>,
    std::collections::BTreeSet<String>,
) {
    let parents: BTreeMap<&str, Option<&str>> = topology
        .iter()
        .map(|(id, parent)| (id.as_str(), parent.as_deref()))
        .collect();
    let target = action.target();
    let mut owners = std::collections::BTreeSet::from([target.to_owned()]);
    loop {
        let count = owners.len();
        for (id, parent) in &parents {
            if parent.is_some_and(|parent| owners.contains(parent)) {
                owners.insert((*id).to_owned());
            }
        }
        if count == owners.len() {
            break;
        }
    }
    let mut agents = owners.clone();
    let mut current = parents.get(target).copied().flatten();
    while let Some(parent) = current {
        if !agents.insert(parent.to_owned()) {
            break;
        }
        current = parents.get(parent).copied().flatten();
    }
    agents.extend(action.acting_agent().map(str::to_owned));
    (owners, agents)
}

fn written_ids(
    writes: &[crate::agent_management::RowWrite],
    table: &str,
) -> std::collections::BTreeSet<String> {
    writes
        .iter()
        .filter(|write| write.table == table)
        .filter_map(|write| write.record.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect()
}

/// Work item ids owned by `agents`; the runtime_work_owner_status index serves it.
fn work_owned_by(
    transaction: &Transaction<'_>,
    agents: &std::collections::BTreeSet<String>,
) -> Result<std::collections::BTreeSet<String>, ManagementError> {
    let mut ids = std::collections::BTreeSet::new();
    if agents.is_empty() || !crate::sync_policy::table_exists(transaction, "runtime_work")? {
        return Ok(ids);
    }
    let mut statement = transaction
        .prepare_cached("SELECT id FROM runtime_work WHERE json_extract(record,'$.owner')=?1")
        .map_err(studio_runtime_store::StoreError::from)?;
    for agent in agents {
        let rows = statement
            .query_map([agent], |row| row.get::<_, String>(0))
            .and_then(Iterator::collect::<Result<Vec<_>, _>>)
            .map_err(studio_runtime_store::StoreError::from)?;
        ids.extend(rows);
    }
    Ok(ids)
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
mod lifecycle_scope_tests {
    use super::lifecycle_scope;
    use crate::agent_management::Action;

    #[test]
    fn scope_is_the_tree_the_ancestors_and_the_actor() {
        let topology = [
            ("root", None),
            ("child", Some("root")),
            ("grandchild", Some("child")),
            ("sibling", Some("root")),
            ("other", None),
            ("other-child", Some("other")),
        ]
        .map(|(id, parent)| (id.to_owned(), parent.map(str::to_owned)));
        let ids = |ids: &[&str]| ids.iter().map(|id| (*id).to_owned()).collect();
        let (owners, agents) = lifecycle_scope(
            &topology,
            &Action::Archive {
                actor: "root".into(),
                epoch: 1,
                target: "child".into(),
                reason: String::new(),
                unassign_work: false,
            },
        );
        assert_eq!(owners, ids(&["child", "grandchild"]));
        assert_eq!(agents, ids(&["child", "grandchild", "root"]));
        let (owners, agents) = lifecycle_scope(
            &topology,
            &Action::Delete {
                target: "missing".into(),
            },
        );
        assert_eq!(owners, ids(&["missing"]));
        assert_eq!(agents, ids(&["missing"]));
    }
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
