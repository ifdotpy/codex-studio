//! Transactional lifecycle consumer; injected effects share this writer transaction.
use crate::agent_management::{
    Action, ManagementError, ManagementSnapshot, Records, decide, decide_with_effects,
};
use rusqlite::OptionalExtension;
use serde::{Deserialize, Serialize};
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
    if matches!(action, Action::Delete { .. }) {
        crate::workspace_cleanup_runtime::reconcile_pending_cleanup(std::sync::Arc::clone(&store))
            .await
            .map_err(|_| {
                studio_runtime_store::StoreError::RecordContract(
                    "Lifecycle committed; workspace cleanup remains pending",
                )
            })?;
    }
    if let Action::Archive { target, .. } = action {
        let reply: ArchiveReply = serde_json::from_value(result.clone())
            .map_err(studio_runtime_store::StoreError::from)?;
        if matches!(reply.status, ArchiveStatus::Blocked) {
            return Ok(result);
        }
        // The lifecycle is committed. A cleanup failure is a pending workspace,
        // not an unknown archive; retain its cause in the applied response.
        if let Err(error) = crate::workspace_cleanup_runtime::cleanup_agent_workspace(
            std::sync::Arc::clone(&store),
            target.clone(),
            crate::workspace_cleanup_runtime::CleanupAction::Archive,
        )
        .await
        {
            result["cleanupPending"] = Value::Bool(true);
            result["worktree"] = serde_json::json!(CleanupReply {
                state: CleanupState::Pending,
                bytes: None,
                reason: Some(error.to_string()),
            });
            result["workspace"] = Value::Null;
            return Ok(result);
        }
        let target = studio_runtime_store::AgentId::from(target.as_str());
        let current = store
            .read_async(
                "runtime-services/agent_management/archive_readback",
                move |tx| studio_runtime_store::AgentRepository::get(tx, &target),
            )
            .await?;
        if let Some(current) = current {
            let record = current.value();
            let progress: ArchiveReadback = serde_json::from_value(record.clone())
                .map_err(studio_runtime_store::StoreError::from)?;
            let pending = progress
                .agent_archive
                .as_ref()
                .is_some_and(|archive| archive.cleanup_pending);
            let (worktree, workspace) = if pending {
                (
                    serde_json::json!({"state":"pending","bytes":0}),
                    Value::Null,
                )
            } else {
                crate::agent_management::archived_cleanup_result(&record)
            };
            result["worktree"] =
                if !pending && !progress.worktree_ready && progress.cleaned_worktree.is_none() {
                    serde_json::json!({"state":"none","bytes":0})
                } else {
                    worktree
                };
            result["cleanupPending"] = Value::Bool(pending);
            result["workspace"] = workspace;
            result["agent"] = crate::agent_management::brief(&record);
        }
    }
    Ok(result)
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveReadback {
    agent_archive: Option<ArchiveCleanupProgress>,
    #[serde(default)]
    worktree_ready: bool,
    cleaned_worktree: Option<serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveCleanupProgress {
    #[serde(default)]
    cleanup_pending: bool,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ArchiveFinishedResult {
    pub archived: usize,
    pub freed_bytes: u64,
    pub unknown_bytes: usize,
    pub kept: Vec<KeptWorker>,
    pub unassigned_work: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct KeptWorker {
    pub id: String,
    pub reason: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "snake_case")]
enum ArchiveStatus {
    Archived,
    Blocked,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct ArchiveReply {
    status: ArchiveStatus,
    #[serde(default)]
    blockers: Vec<ArchiveBlocker>,
    #[serde(default)]
    unassigned_work: Vec<String>,
    worktree: Option<CleanupReply>,
    workspace: Option<CleanupReply>,
}

#[derive(Deserialize)]
struct ArchiveBlocker {
    kind: crate::agent_management::ArchiveBlockerKind,
}

#[derive(Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
enum CleanupState {
    None,
    Removed,
    Missing,
    Kept,
    Archived,
    Pending,
    Conflict,
}

#[derive(Deserialize, Serialize)]
struct CleanupReply {
    state: CleanupState,
    bytes: Option<u64>,
    reason: Option<String>,
}

/// Default audit reason for a finished-worker batch whose caller omitted it.
pub(crate) fn finished_archive_reason() -> String {
    "Finished worker cleanup".into()
}

/// Batch archive is orchestration of the existing per-worker owner, never a
/// second deletion path. Partial progress is discoverable on archived records;
/// a tool retry uses the caller's durable tool receipt rather than rerunning it.
pub async fn archive_finished(
    store: std::sync::Arc<RuntimeStore>,
    actor: &str,
    epoch: i64,
    reason: &str,
    unassign_work: bool,
    now: f64,
) -> Result<ArchiveFinishedResult, ManagementError> {
    let actor_id = actor.to_owned();
    let candidates = store
        .read_async(
            "runtime-services/agent_management/archive_finished",
            move |tx| {
                let lead = studio_runtime_store::AgentRepository::get(
                    tx,
                    &studio_runtime_store::AgentId::from(actor_id.as_str()),
                )?
                .ok_or(ManagementError::Invalid("Unknown managed agent"))?;
                let mut statement = tx
                    .prepare_cached("SELECT id FROM runtime_agents WHERE root_id=?1")
                    .map_err(studio_runtime_store::StoreError::from)?;
                let ids = statement
                    .query_map([&lead.root_id.0], |row| row.get::<_, String>(0))
                    .and_then(Iterator::collect::<Result<std::collections::BTreeSet<_>, _>>)
                    .map_err(studio_runtime_store::StoreError::from)?;
                let agents = repository::read_agents(tx, &ids)?;
                ManagementSnapshot::from_records(Records {
                    tables: BTreeMap::from([("runtime_agents".into(), agents)]),
                })?
                .finished_descendants(&actor_id, epoch)
            },
        )
        .await?;
    let mut result = ArchiveFinishedResult::default();
    for (id, record) in candidates {
        let checked = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/archive_finished/preflight",
            move || crate::workspace_cleanup_runtime::check_archive_worktree(&record),
        )
        .await;
        if !matches!(checked, Ok(Ok(()))) {
            result.kept.push(KeptWorker { id, reason: "Worktree is dirty, unverified, missing, or contains another worktree; files were preserved".into() });
            continue;
        }
        let reply = execute(
            std::sync::Arc::clone(&store),
            &Action::Archive {
                actor: actor.to_owned(),
                epoch,
                target: id.clone(),
                reason: reason.to_owned(),
                unassign_work,
            },
            now,
        )
        .await?;
        let reply: ArchiveReply =
            serde_json::from_value(reply).map_err(studio_runtime_store::StoreError::from)?;
        match reply.status {
            ArchiveStatus::Blocked => result.kept.push(KeptWorker {
                id,
                reason: reply
                    .blockers
                    .into_iter()
                    .map(|blocker| blocker.kind.label())
                    .collect::<Vec<_>>()
                    .join(", "),
            }),
            ArchiveStatus::Archived => {
                result.archived += 1;
                result.unassigned_work.extend(reply.unassigned_work);
                if let Some(cleanup) = reply.workspace.or(reply.worktree) {
                    match cleanup.state {
                        CleanupState::Removed | CleanupState::Conflict => match cleanup.bytes {
                            Some(bytes) => {
                                result.freed_bytes = result.freed_bytes.saturating_add(bytes)
                            }
                            None => result.unknown_bytes += 1,
                        },
                        CleanupState::Kept | CleanupState::Pending => {
                            result.kept.push(KeptWorker {
                                id,
                                reason: cleanup.reason.unwrap_or_else(|| {
                                    "Workspace cleanup is held for inspection".into()
                                }),
                            })
                        }
                        CleanupState::None | CleanupState::Missing | CleanupState::Archived => {}
                    }
                }
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
    execute(
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
    .await
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
        let agents = repository::read_agents(transaction, &agents)?;
        // Archive and its inspection release a failed provisioning only after
        // reading the target's journal; other actions never decide from journals.
        if matches!(action, Action::Archive { .. } | Action::Inspect { .. }) {
            let target = agents
                .iter()
                .filter(|agent| agent.get("id").and_then(Value::as_str) == Some(action.target()))
                .cloned()
                .collect::<Vec<_>>();
            tables.insert(
                "runtime_workspace_operations".to_owned(),
                repository::read_reserved_operations(transaction, &target)?,
            );
        }
        tables.insert("runtime_agents".to_owned(), agents);
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
    if let Some(release) = &decision.archive_release {
        repository::settle_archived_inputs(
            transaction,
            &release.agent,
            release.epoch,
            &release.inputs,
        )?;
        if let Some(operation) = &release.provision {
            studio_runtime_store::workspace_ready::WorkspaceReadyRepository::release_archived(
                transaction,
                operation,
                &release.agent,
                now,
            )?;
        }
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

#[cfg(test)]
mod archive_finished_tests {
    use super::*;
    use serde_json::json;
    use std::sync::Arc;
    use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, Table, WorkItemRecord};

    #[tokio::test]
    async fn committed_archive_reports_original_cleanup_failure_and_preserves_identity() {
        const EPOCH: i64 = 1;
        const ARCHIVE_TIME: f64 = 20.0;
        let fixture = tempfile::tempdir().expect("fixture");
        let store = Arc::new(
            RuntimeStore::open(Some(fixture.path()))
                .await
                .expect("store"),
        );
        seed_failed_start_team(
            &store,
            vec![json!({"id":"worker","rootId":"lead",
            "parentId":"lead","epoch":EPOCH,"status":"completed","autoWake":true})],
            vec![],
            vec![],
        )
        .await;
        store
            .transaction(|tx| {
                tx.execute_batch(
                    "CREATE TRIGGER reject_cleanup BEFORE UPDATE ON runtime_agents
                WHEN NEW.id='worker' AND json_extract(NEW.record,'$.agentArchive.cleanupPending')=0
                BEGIN SELECT RAISE(ABORT, 'injected cleanup persistence failure'); END;",
                )?;
                Ok(())
            })
            .await
            .expect("deterministic cleanup failure");
        let result = archive_finished(Arc::clone(&store), "lead", EPOCH, false, ARCHIVE_TIME)
            .await
            .expect("committed archive remains an applied result");
        assert_eq!(result.archived, 1);
        assert!(result.kept.iter().any(|worker| {
            worker.id == "worker"
                && worker
                    .reason
                    .contains("injected cleanup persistence failure")
        }));
        let read = || {
            store.read_async("test/committed-archive", |tx| {
                AgentRepository::get(tx, &AgentId::from("worker"))
            })
        };
        let before = read().await.expect("read").expect("worker").value();
        assert_eq!(before["agentArchive"]["cleanupPending"], true);
        store
            .transaction(|tx| {
                tx.execute_batch("DROP TRIGGER reject_cleanup")?;
                Ok(())
            })
            .await
            .expect("release cleanup failure");
        let action = Action::Archive {
            actor: "lead".into(),
            epoch: EPOCH,
            target: "worker".into(),
            reason: "must not replace committed receipt".into(),
            unassign_work: false,
        };
        let recovered = execute(Arc::clone(&store), &action, ARCHIVE_TIME)
            .await
            .expect("readback");
        assert_eq!(recovered["status"], "archived");
        assert_eq!(recovered["replayed"], true);
        let after = read().await.expect("read").expect("worker").value();
        assert_eq!(after["agentArchive"]["cleanupPending"], false);
        assert_eq!(before["agentArchive"]["at"], after["agentArchive"]["at"]);
        assert_eq!(
            before["agentArchive"]["reason"],
            after["agentArchive"]["reason"]
        );
        assert_eq!(before["epoch"], after["epoch"]);
    }

    #[tokio::test]
    async fn committed_archive_ignores_unrelated_pending_cleanup() {
        let fixture = tempfile::tempdir().expect("fixture");
        let store = Arc::new(
            RuntimeStore::open(Some(fixture.path()))
                .await
                .expect("store"),
        );
        let unrelated = json!({"id":"unrelated","rootId":"lead","parentId":"lead",
            "epoch":1,"status":"completed","autoWake":true});
        seed_failed_start_team(
            &store,
            vec![
                json!({"id":"worker","rootId":"lead",
            "parentId":"lead","epoch":1,"status":"completed","autoWake":true}),
                unrelated,
            ],
            vec![],
            vec![],
        )
        .await;
        execute_prepared(
            Arc::clone(&store),
            &Action::Archive {
                actor: "lead".into(),
                epoch: 1,
                target: "unrelated".into(),
                reason: "Earlier archive".into(),
                unassign_work: false,
            },
            10.0,
            None,
        )
        .await
        .expect("earlier committed archive");
        let unrelated = store
            .read_async("test/pending-unrelated-cleanup", |tx| {
                AgentRepository::get(tx, &AgentId::from("unrelated"))
            })
            .await
            .expect("read")
            .expect("unrelated")
            .value();
        store.transaction(|tx| {
            tx.execute_batch("CREATE TRIGGER reject_unrelated_cleanup BEFORE UPDATE ON runtime_agents
                WHEN NEW.id='unrelated' AND json_extract(NEW.record,'$.agentArchive.cleanupPending')=0
                BEGIN SELECT RAISE(ABORT, 'unrelated cleanup persistence failure'); END;")?;
            Ok(())
        }).await.expect("unrelated cleanup failure");
        let result = archive_finished(Arc::clone(&store), "lead", 1, false, 20.0)
            .await
            .expect("unrelated cleanup cannot fail committed archive");
        assert_eq!(result.archived, 1);
        assert!(result.kept.is_empty());
        let current = store
            .read_async("test/unrelated-cleanup", |tx| {
                AgentRepository::get(tx, &AgentId::from("unrelated"))
            })
            .await
            .expect("read")
            .expect("unrelated")
            .value();
        assert_eq!(
            current["workspaceCleanupIntent"],
            unrelated["workspaceCleanupIntent"]
        );
    }

    #[tokio::test]
    async fn archive_finished_removes_accepted_worktree_and_target_and_keeps_dirty_work() {
        let fixture = tempfile::tempdir().expect("fixture");
        let repository = fixture.path().join("project");
        std::fs::create_dir_all(&repository).expect("repository");
        let git = |args: &[&str]| {
            let output =
                crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
                    program: std::ffi::OsStr::new("git"),
                    git_directory: Some(repository.as_os_str()),
                    args,
                    environment: &[("GIT_TERMINAL_PROMPT", "0")],
                    input: None,
                    stdout_limit: 64 * 1024,
                    stderr_limit: 4096,
                    timeout: std::time::Duration::from_secs(10),
                })
                .expect("bounded fixture Git succeeds");
            assert!(!output.stdout.truncated);
            String::from_utf8(output.stdout.bytes)
                .expect("git UTF8")
                .trim()
                .to_owned()
        };
        git(&["init", "-q"]);
        git(&["config", "user.name", "Disk Fixture"]);
        git(&["config", "user.email", "disk@example.invalid"]);
        std::fs::write(repository.join(".gitignore"), "target/\n.worktrees/\n")
            .expect("ignore build output");
        std::fs::write(repository.join("source.txt"), "committed\n").expect("source");
        git(&["add", ".gitignore", "source.txt"]);
        git(&["commit", "-qm", "Fixture"]);
        let clean = repository.join(".worktrees/codex-agents/clean");
        let dirty = repository.join(".worktrees/codex-agents/dirty");
        git(&[
            "worktree",
            "add",
            "-qb",
            "clean",
            clean.to_str().expect("path"),
        ]);
        git(&[
            "worktree",
            "add",
            "-qb",
            "dirty",
            dirty.to_str().expect("path"),
        ]);
        std::fs::create_dir(clean.join("target")).expect("target");
        std::fs::write(clean.join("target/fixture-output"), "build output").expect("output");
        std::fs::write(dirty.join("source.txt"), "uncommitted work\n").expect("dirty source");
        std::fs::create_dir(fixture.path().join("state")).expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(&fixture.path().join("state")))
                .await
                .expect("store"),
        );
        let clean_cwd = clean.clone();
        let dirty_cwd = dirty.clone();
        store.transaction_with_error(move |tx| {
            for row in [
                json!({"id":"lead", "rootId":"lead", "epoch":1, "status":"running", "isLead":true, "autoWake":true}),
                json!({"id":"clean", "rootId":"lead", "parentId":"lead", "epoch":1, "status":"completed", "role":"implementer", "autoWake":true, "cwd":clean_cwd, "worktreeReady":true}),
                json!({"id":"dirty", "rootId":"lead", "parentId":"lead", "epoch":1, "status":"completed", "autoWake":true, "cwd":dirty_cwd, "worktreeReady":true}),
                json!({"id":"active", "rootId":"lead", "parentId":"lead", "epoch":1, "status":"running", "autoWake":true}),
                json!({"id":"assigned", "rootId":"lead", "parentId":"lead", "epoch":1, "status":"completed", "autoWake":true}),
            ] { AgentRepository::put(tx, &AgentRecord::from_value(row)?)?; }
            for row in [
                json!({"id":"accepted", "rootId":"lead", "owner":"clean", "status":"accepted", "version":1}),
                json!({"id":"open", "rootId":"lead", "owner":"assigned", "status":"running", "version":1}),
            ] { Table::<WorkItemRecord>::new(tx).put(&serde_json::from_value(row)?)?; }
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await.expect("agents and tasks");
        let result = archive_finished(
            Arc::clone(&store),
            "lead",
            1,
            &finished_archive_reason(),
            false,
            20.0,
        )
        .await
        .expect("archive batch");
        assert_eq!(result.archived, 1);
        assert_eq!(
            result.unknown_bytes, 1,
            "Git cleanup does not estimate deleted byte counts"
        );
        assert!(
            !clean.exists(),
            "worktree including ignored target is removed"
        );
        assert_eq!(
            std::fs::read_to_string(dirty.join("source.txt")).expect("preserved source"),
            "uncommitted work\n"
        );
        assert!(result.kept.iter().any(|worker| worker.id == "dirty"));
        assert!(
            result
                .kept
                .iter()
                .any(|worker| worker.id == "assigned" && worker.reason.contains("assigned_work"))
        );
        let archived_head = git(&["rev-parse", "refs/codex-agents/archive/clean"]);
        assert_eq!(archived_head, git(&["rev-parse", "clean"]));
        let repeat = archive_finished(
            Arc::clone(&store),
            "lead",
            1,
            &finished_archive_reason(),
            true,
            21.0,
        )
        .await
        .expect("next batch");
        assert_eq!(repeat.archived, 1, "only newly unassigned worker archives");
        assert_eq!(repeat.unassigned_work, vec!["open"]);
        let active = store
            .read_async("test/archive-active", |tx| {
                AgentRepository::get(tx, &AgentId::from("active"))
            })
            .await
            .expect("read active")
            .expect("active");
        assert!(!active.extra.contains_key("deletedAt"));
        assert!(
            archive_finished(
                Arc::clone(&store),
                "lead",
                0,
                &finished_archive_reason(),
                false,
                22.0
            )
            .await
            .is_err(),
            "stale lead epoch cannot manage workers"
        );
        assert!(
            archive_finished(store, "dirty", 1, &finished_archive_reason(), false, 22.0)
                .await
                .is_err(),
            "worker cannot archive team"
        );
    }

    /// One input row: `(id, worker, kind, status, epoch, error)`.
    type InputRow = (
        &'static str,
        &'static str,
        &'static str,
        &'static str,
        i64,
        Option<&'static str>,
    );

    /// Seed a lead and its workers, each with its own input rows.
    async fn seed_failed_start_team(
        store: &RuntimeStore,
        workers: Vec<Value>,
        inputs: Vec<InputRow>,
        operations: Vec<Value>,
    ) {
        store
            .transaction_with_error(move |tx| {
                let lead = json!({"id":"lead","rootId":"lead","epoch":1,"status":"running",
                    "isLead":true,"autoWake":true});
                for row in std::iter::once(lead).chain(workers) {
                    AgentRepository::put(tx, &AgentRecord::from_value(row)?)?;
                }
                for (id, agent, kind, status, epoch, error) in inputs {
                    tx.execute(
                        "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,error) \
                         VALUES(?1,?2,?3,'input',?4,1,?5,?6)",
                        rusqlite::params![id, agent, kind, status, epoch, error],
                    )?;
                }
                for operation in operations {
                    tx.execute(
                        "INSERT INTO runtime_workspace_operations(id,record) VALUES(?1,?2)",
                        rusqlite::params![operation["id"].as_str(), operation.to_string()],
                    )?;
                }
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed failed-start team");
    }

    async fn archive_worker(store: &Arc<RuntimeStore>, target: &str) -> Value {
        execute(
            Arc::clone(store),
            &Action::Archive {
                actor: "lead".into(),
                epoch: 1,
                target: target.into(),
                reason: "Replacement finished the task".into(),
                unassign_work: false,
            },
            30.0,
        )
        .await
        .expect("archive decides")
    }

    async fn input_rows(
        store: &RuntimeStore,
        agent: &'static str,
    ) -> Vec<(String, String, Option<String>)> {
        store
            .read_async("test/archive-failed-start/inputs", move |tx| {
                let mut statement = tx.prepare(
                    "SELECT id,status,error FROM runtime_events WHERE agent=?1 ORDER BY id",
                )?;
                let rows = statement
                    .query_map([agent], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                Ok::<_, studio_runtime_store::StoreError>(rows)
            })
            .await
            .expect("read inputs")
    }

    fn blocker_kinds(result: &Value) -> Vec<&str> {
        result["blockers"]
            .as_array()
            .map(|blockers| {
                blockers
                    .iter()
                    .filter_map(|blocker| blocker["kind"].as_str())
                    .collect()
            })
            .unwrap_or_default()
    }

    /// Live 2026-10-22: two workers failed before their first submission. One
    /// kept its start prompt and a later lead message as pending input; the
    /// other also kept the reservation of a provisioning that needs recovery.
    /// Archive refused both with `input_delivery` (and `workspace_operation`).
    #[tokio::test]
    async fn archive_settles_the_unsent_input_of_a_worker_that_failed_before_its_first_turn() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        let destination = project.join(".worktrees/codex-agents/provisioning");
        std::fs::create_dir_all(&destination).expect("provisional worktree");
        std::fs::write(destination.join("kept.txt"), "provisional files\n").expect("file");
        std::fs::create_dir(fixture.path().join("state")).expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(&fixture.path().join("state")))
                .await
                .expect("store"),
        );
        let worker = |id: &str| {
            json!({"id":id,"rootId":"lead","parentId":"lead","epoch":0,"status":"failed",
                "autoWake":true,"isLead":false,"role":"implementer","threadId":null,
                "turnId":null,"inFlight":false,"worktree":true,"worktreeReady":false,
                "cwd":project,"error":"workspace-readiness/token-stale"})
        };
        let mut provisioning = worker("provisioning");
        provisioning["error"] =
            json!("WorkspaceRecoveryRequired: Provisioning did not establish a complete worktree");
        provisioning["workspaceOperation"] = json!("provision");
        provisioning["workspaceReservationId"] = json!("provision:provisioning:0");
        provisioning["worktreePreparation"] = json!("preparing");
        seed_failed_start_team(
            &store,
            vec![worker("unstarted"), provisioning],
            vec![
                (
                    "unstarted:initial",
                    "unstarted",
                    "user",
                    "pending",
                    0,
                    Some("workspace-readiness/token-stale"),
                ),
                (
                    "unstarted:lead-message",
                    "unstarted",
                    "followup",
                    "pending",
                    0,
                    None,
                ),
                (
                    "provisioning:initial",
                    "provisioning",
                    "user",
                    "pending",
                    0,
                    None,
                ),
            ],
            vec![
                json!({"id":"provision:provisioning:0","kind":"provision","agent":"provisioning",
                "epoch":0,"phase":"recovery_required","cwd":destination,
                "error":"Workspace Git deadline expired at git worktree list --porcelain -z"}),
            ],
        )
        .await;

        let result = archive_worker(&store, "unstarted").await;
        assert_eq!(result["status"], "archived", "{result}");
        assert_eq!(
            result["settledInputs"],
            json!(["unstarted:initial", "unstarted:lead-message"])
        );
        let rows = input_rows(&store, "unstarted").await;
        assert_eq!(rows.len(), 2, "the input history is kept");
        for (id, status, error) in rows {
            assert_eq!(status, "cancelled", "{id} is settled as not delivered");
            let error = error.unwrap_or_default();
            assert!(
                error.starts_with(studio_runtime_store::agent_management::ARCHIVED_INPUT_ERROR),
                "{id} stores why it was not delivered: {error}"
            );
            if id == "unstarted:initial" {
                assert!(error.contains("workspace-readiness/token-stale"), "{error}");
            }
        }

        let result = archive_worker(&store, "provisioning").await;
        assert_eq!(result["status"], "archived", "{result}");
        assert_eq!(
            result["releasedWorkspaceOperation"],
            "provision:provisioning:0"
        );
        assert_eq!(result["settledInputs"], json!(["provisioning:initial"]));
        let (record, operation) = store
            .read_async("test/archive-failed-start/provisioning", |tx| {
                let record = AgentRepository::get(tx, &AgentId::from("provisioning"))?
                    .expect("archived worker")
                    .value();
                let operation =
                    studio_runtime_store::workspace_ready::WorkspaceReadyRepository::journal(
                        tx,
                        "provision:provisioning:0",
                    )?
                    .expect("provisioning journal is kept");
                Ok::<_, studio_runtime_store::StoreError>((record, operation))
            })
            .await
            .expect("read archived provisioning");
        for field in [
            "workspaceOperation",
            "workspaceReservationId",
            "worktreePreparation",
        ] {
            assert!(
                record.get(field).is_none_or(Value::is_null),
                "{field} is released: {record}"
            );
        }
        assert!(record["agentArchive"].is_object());
        assert_eq!(operation["phase"], "failed", "{operation}");
        assert_eq!(
            operation["error"],
            "Workspace Git deadline expired at git worktree list --porcelain -z"
        );
        assert_eq!(
            operation["release"]["reason"],
            studio_runtime_store::workspace_ready::ARCHIVED_PROVISION_RELEASE
        );
        assert_eq!(
            std::fs::read_to_string(destination.join("kept.txt")).expect("files kept"),
            "provisional files\n",
            "archive never removes a provisional worktree it did not verify"
        );
        let rows = input_rows(&store, "provisioning").await;
        assert_eq!(rows[0].1, "cancelled");
    }

    /// The settlement applies only to input that was never claimed by a
    /// failed worker without an automatic retry. Everything else is refused
    /// exactly as before.
    #[tokio::test]
    async fn archive_still_refuses_submitted_input_retries_and_unfinished_provisioning() {
        let fixture = tempfile::tempdir().expect("fixture");
        let store = Arc::new(
            RuntimeStore::open(Some(fixture.path()))
                .await
                .expect("store"),
        );
        let worker = |id: &str, status: &str| {
            json!({"id":id,"rootId":"lead","parentId":"lead","epoch":0,"status":status,
                "autoWake":true,"isLead":false,"threadId":null,"turnId":null,"inFlight":false})
        };
        let mut retrying = worker("retrying", "failed");
        retrying["usageResume"] = json!({"status":"scheduled","dueAt":100.0});
        let mut mutating = worker("mutating", "failed");
        mutating["workspaceOperation"] = json!("provision");
        mutating["workspaceReservationId"] = json!("provision:mutating:0");
        let mut in_flight = worker("in-flight", "failed");
        in_flight["inFlight"] = json!(true);
        seed_failed_start_team(
            &store,
            vec![
                worker("uncertain", "failed"),
                worker("reserved", "failed"),
                worker("completed", "completed"),
                retrying,
                mutating,
                in_flight,
            ],
            vec![
                ("uncertain:sent", "uncertain", "user", "uncertain", 0, None),
                (
                    "uncertain:queued",
                    "uncertain",
                    "followup",
                    "pending",
                    0,
                    None,
                ),
                ("reserved:claimed", "reserved", "user", "reserved", 0, None),
                (
                    "completed:queued",
                    "completed",
                    "followup",
                    "pending",
                    0,
                    None,
                ),
                ("retrying:queued", "retrying", "user", "pending", 0, None),
                ("in-flight:queued", "in-flight", "user", "pending", 0, None),
            ],
            vec![
                json!({"id":"provision:mutating:0","kind":"provision","agent":"mutating",
                "epoch":0,"phase":"local_mutation","cwd":fixture.path().join("mutating")}),
            ],
        )
        .await;
        for (target, kind) in [
            ("uncertain", "input_delivery"),
            ("reserved", "input_delivery"),
            ("completed", "input_delivery"),
            ("retrying", "input_delivery"),
            ("mutating", "workspace_operation"),
            ("in-flight", "active_turn"),
        ] {
            let result = archive_worker(&store, target).await;
            assert_eq!(result["status"], "blocked", "{target}: {result}");
            assert!(blocker_kinds(&result).contains(&kind), "{target}: {result}");
        }
        for (agent, expected) in [
            ("uncertain", vec!["pending", "uncertain"]),
            ("reserved", vec!["reserved"]),
            ("completed", vec!["pending"]),
            ("retrying", vec!["pending"]),
            ("in-flight", vec!["pending"]),
        ] {
            let mut statuses = input_rows(&store, agent)
                .await
                .into_iter()
                .map(|(_, status, _)| status)
                .collect::<Vec<_>>();
            statuses.sort();
            assert_eq!(statuses, expected, "{agent} input is unchanged");
        }
        let operation = store
            .read_async("test/archive-failed-start/mutating", |tx| {
                studio_runtime_store::workspace_ready::WorkspaceReadyRepository::journal(
                    tx,
                    "provision:mutating:0",
                )
            })
            .await
            .expect("read journal")
            .expect("journal");
        assert_eq!(operation["phase"], "local_mutation");
    }

    #[tokio::test]
    async fn archive_finished_archives_nested_finished_workers_deepest_first() {
        let fixture = tempfile::tempdir().expect("fixture");
        let store = Arc::new(
            RuntimeStore::open(Some(fixture.path()))
                .await
                .expect("store"),
        );
        store.transaction_with_error(|tx| {
            for row in [
                json!({"id":"lead", "rootId":"lead", "epoch":1, "status":"running", "isLead":true, "autoWake":true}),
                json!({"id":"parent", "rootId":"lead", "parentId":"lead", "epoch":1, "status":"completed"}),
                json!({"id":"child", "rootId":"lead", "parentId":"parent", "epoch":1, "status":"completed"}),
            ] { AgentRepository::put(tx, &AgentRecord::from_value(row)?)?; }
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await.expect("team");
        let result = archive_finished(store, "lead", 1, &finished_archive_reason(), false, 20.0)
            .await
            .expect("nested cleanup");
        assert_eq!(result.archived, 2);
        assert!(result.kept.is_empty());
    }
}
