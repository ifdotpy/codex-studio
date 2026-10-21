//! `orchestration_agent_manage`, Python `codex_agent_management.manage_agent`.
//!
//! Event waits, lists and inspection commit in the callback transaction.
//! Archive and restore use the shared lifecycle owner after the claim commits
//! (`runtime_dispatch`), because workspace cleanup runs outside a transaction.
use super::*;
use crate::managed_host_tools::WakeSignal;

/// Python `manage_agent` action names, in its order.
const ACTIONS: &[&str] = &[
    "inspect",
    "list",
    "recover",
    "reset_tools",
    "archive",
    "archive_finished",
    "restore",
    "list_archived",
    "maintenance_report",
    "park",
    "list_parked",
    "cancel_park",
    "emit_event",
];

/// `agent_id` values that Python `Runtime.dynamic` does not resolve.
const UNRESOLVED_TARGETS: &[&str] = &["parent", "lead", "broadcast", "all", "workspace"];

/// Maximum characters of an event name (Python `_event_name`).
const MAX_EVENT_CHARS: usize = 120;

/// Maximum characters of an `emit_event` request id.
const MAX_REQUEST_ID_CHARS: usize = 200;

/// Statuses of current-epoch input that Python refuses to park over.
const OPEN_INPUT: &str = "status IN ('pending','reserved','dispatching','uncertain')";

/// Python `manage_agent` checks before any read: the action name and
/// `unassign_work`. Returns the action and the `unassign_work` value.
pub(crate) fn checked_action(args: &Value) -> Result<(&str, bool), CallbackError> {
    let action = args["action"]
        .as_str()
        .filter(|action| ACTIONS.contains(action))
        .ok_or(CallbackError::Invalid("Unknown agent management action"))?;
    let unassign_work = match args.get("unassign_work") {
        None => false,
        Some(Value::Bool(value)) => *value,
        Some(_) => return Err(CallbackError::Invalid("unassign_work must be a boolean")),
    };
    if unassign_work && !matches!(action, "archive" | "archive_finished") {
        return Err(CallbackError::Invalid(
            "unassign_work applies only to archive or archive_finished",
        ));
    }
    Ok((action, unassign_work))
}

/// Actions that the async lifecycle owner runs (`runtime_dispatch`).
pub(crate) fn lifecycle_action(args: &Value) -> bool {
    matches!(
        args["action"].as_str(),
        Some("archive" | "archive_finished" | "restore")
    )
}

/// Python `Runtime.dynamic` resolves a string `agent_id` with
/// `resolve_visible_agent_id(..., include_archived=True)`: team members only,
/// archived members included. Other values reach `manage_agent` unchanged.
pub(crate) fn resolved_args(
    tx: &rusqlite::Connection,
    actor: &Value,
    args: &Value,
) -> Result<Value, CallbackError> {
    let mut args = args.clone();
    if let Some(supplied) = args["agent_id"].as_str()
        && !UNRESOLVED_TARGETS.contains(&supplied)
    {
        let root = text(actor, "rootId")?;
        let ids = team(tx, root, true)?
            .into_iter()
            .filter_map(|agent| agent["id"].as_str().map(str::to_owned));
        args["agent_id"] = json!(messages::resolve_visible_agent_prefix(ids, supplied)?);
    }
    Ok(args)
}

/// Python `rt.agent(args.get('agent_id'))`.
fn target(tx: &Transaction<'_>, args: &Value) -> Result<Value, CallbackError> {
    let id = args["agent_id"]
        .as_str()
        .ok_or(CallbackError::Invalid("Unknown managed agent"))?;
    records::get(tx, "runtime_agents", id)?.ok_or(CallbackError::Invalid("Unknown managed agent"))
}

/// Python `Runtime.team_agents`, in row order.
fn team(
    tx: &rusqlite::Connection,
    root: &str,
    include_deleted: bool,
) -> Result<Vec<Value>, CallbackError> {
    let mut statement =
        tx.prepare_cached("SELECT id,record FROM runtime_agents WHERE root_id=?1 ORDER BY rowid")?;
    let rows = statement
        .query_map([root], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut agents = Vec::with_capacity(rows.len());
    for (id, raw) in rows {
        let mut agent = records::decode("runtime_agents", &id, &raw)?;
        studio_runtime_store::agent_volatile::merge(tx, &id, &mut agent)?;
        if include_deleted || !studio_runtime_store::truthy(agent.get("deletedAt")) {
            agents.push(agent);
        }
    }
    Ok(agents)
}

/// The local actions of `manage_agent`. Returns the tool value and the wake
/// signals that the caller releases after its transaction commits.
pub(super) fn execute(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
    now: f64,
) -> Result<(Value, Vec<WakeSignal>), CallbackError> {
    let actor = snapshot.actor();
    let args = &resolved_args(tx, actor, args)?;
    let (action, _) = checked_action(args)?;
    match action {
        "park" | "list_parked" | "cancel_park" | "emit_event" => park_action(tx, actor, args, now),
        "list" | "list_archived" => Ok((list(tx, snapshot, args, action)?, Vec::new())),
        "inspect" => {
            let target = target(tx, args)?;
            let value = crate::agent_management_store::execute_in_transaction(
                tx,
                &crate::agent_management::Action::Inspect {
                    actor: snapshot.context().agent.clone(),
                    epoch: snapshot.context().epoch as i64,
                    target: text(&target, "id")?.to_owned(),
                },
                now,
            )
            .map_err(management_error)?;
            Ok((value, Vec::new()))
        }
        // Python reads native history and the account transfer store for
        // recovery, and releases native tool sessions for a tool reset.
        "recover" => Err(CallbackError::Dependency(
            "account-worker/agent-input-recovery",
        )),
        "reset_tools" => Err(CallbackError::Dependency("native-release/tool-reset")),
        "maintenance_report" => Err(CallbackError::Dependency(
            "agent-management/maintenance-report",
        )),
        _ => Err(CallbackError::Invalid(
            "Agent lifecycle dispatch missed an action",
        )),
    }
}

pub(crate) fn management_error(error: crate::agent_management::ManagementError) -> CallbackError {
    use crate::agent_management::ManagementError as E;
    match error {
        E::Invalid(message) => CallbackError::Invalid(message),
        E::Unavailable(name) => CallbackError::Dependency(name),
        E::Stored { table, id } => CallbackError::Stored(format!("{table}/{id}")),
        E::Store(error) => CallbackError::Store(error),
    }
}

/// Python `_authorize` without a target.
fn authorize_lead(actor: &Value) -> Result<(), CallbackError> {
    if actor["isLead"] != true
        || studio_runtime_store::truthy(actor.get("deletedAt"))
        || actor["autoWake"] != true
    {
        return Err(CallbackError::Invalid(
            "Only the active orchestrator can manage its workers",
        ));
    }
    Ok(())
}

fn list(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    args: &Value,
    action: &str,
) -> Result<Value, CallbackError> {
    let actor = snapshot.actor();
    authorize_lead(actor)?;
    let root = text(actor, "rootId")?;
    let archived = action == "list_archived";
    let mut rows = team(tx, root, archived)?
        .into_iter()
        .filter(|agent| {
            if archived {
                studio_runtime_store::truthy(agent.get("agentArchive"))
                    && studio_runtime_store::truthy(agent.get("deletedAt"))
            } else {
                agent["id"] != actor["id"]
            }
        })
        .map(|agent| crate::agent_management::brief(&agent))
        .collect::<Vec<_>>();
    rows.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
    let scope = if archived {
        "archived_workers"
    } else {
        "workers"
    };
    model_page(rows, args, json!([root, scope]), None)
}

/// Python `_event_name`: 1 to 120 visible ASCII characters.
fn event_name(value: &Value) -> Result<&str, CallbackError> {
    value
        .as_str()
        .filter(|name| {
            (1..=MAX_EVENT_CHARS).contains(&name.chars().count())
                && name
                    .chars()
                    .all(|character| ('!'..='~').contains(&character))
        })
        .ok_or(CallbackError::Invalid(
            "Use an event name with 1 to 120 visible ASCII characters",
        ))
}

/// Python `_park_target`: a live worker of this team that is the caller or
/// one of its descendants.
fn park_target(tx: &Transaction<'_>, actor: &Value, target: &Value) -> Result<(), CallbackError> {
    if studio_runtime_store::truthy(target.get("deletedAt"))
        || studio_runtime_store::truthy(target.get("agentArchive"))
        || studio_runtime_store::truthy(target.get("isLead"))
    {
        return Err(CallbackError::Invalid("Choose a live worker"));
    }
    if target["rootId"] != actor["rootId"] {
        return Err(CallbackError::Invalid(
            "This worker belongs to another team",
        ));
    }
    if target["id"] == actor["id"] {
        return Ok(());
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut current = target.clone();
    while current["parentId"] != actor["id"] {
        let parent = current["parentId"]
            .as_str()
            .filter(|parent| !parent.is_empty())
            .ok_or(CallbackError::Invalid("This worker is not your descendant"))?
            .to_owned();
        if !seen.insert(text(&current, "id")?.to_owned()) {
            return Err(CallbackError::Invalid("This worker is not your descendant"));
        }
        current = records::get(tx, "runtime_agents", &parent)?
            .ok_or(CallbackError::Invalid("Unknown managed agent"))?;
    }
    Ok(())
}

fn receipt(
    tx: &Transaction<'_>,
    key: &str,
    signature: &str,
) -> Result<Option<Value>, CallbackError> {
    Ok(event_write::read_receipt(
        tx,
        Some(key),
        signature,
        "runtime_operation_receipts",
        key,
        Value::is_object,
    )?)
}

fn save_receipt(
    tx: &Transaction<'_>,
    key: &str,
    signature: &str,
    value: Value,
) -> Result<Value, CallbackError> {
    event_write::save_receipt(tx, Some(key), signature, &value)?;
    Ok(value)
}

/// Save a changed agent row with its projection, then queue its wake input.
fn wake(
    tx: &Transaction<'_>,
    agent: &Value,
    id: String,
    text: String,
    now: f64,
    signals: &mut Vec<WakeSignal>,
) -> Result<(), CallbackError> {
    super::super::super::storage::save_actor(tx, agent)?;
    let typed = studio_runtime_store::AgentRepository::get(
        tx,
        &studio_runtime_store::AgentId::from(super::text(agent, "id")?),
    )?
    .ok_or(CallbackError::Stale)?;
    if event_write::enqueue_event(tx, &typed, id, "event_wake", text, now)? {
        signals.push(WakeSignal::new("account-worker/queued-event"));
    }
    Ok(())
}

/// Python `_park_action`: park, list_parked, cancel_park and emit_event.
fn park_action(
    tx: &Transaction<'_>,
    actor: &Value,
    args: &Value,
    now: f64,
) -> Result<(Value, Vec<WakeSignal>), CallbackError> {
    let action = text(args, "action")?;
    let root = text(actor, "rootId")?;
    let actor_id = text(actor, "id")?;
    let mut signals = Vec::new();
    if action == "list_parked" {
        let mut rows = Vec::new();
        for agent in team(tx, root, false)? {
            if !studio_runtime_store::truthy(agent.get("parkedEvent")) {
                continue;
            }
            if actor["isLead"] != true {
                match park_target(tx, actor, &agent) {
                    Ok(()) => {}
                    Err(CallbackError::Invalid(_)) => continue,
                    Err(error) => return Err(error),
                }
            }
            rows.push(crate::agent_management::brief(&agent));
        }
        rows.sort_by(|left, right| left["id"].as_str().cmp(&right["id"].as_str()));
        let value = model_page(rows, args, json!([root, actor_id, "parked_workers"]), None)?;
        return Ok((value, signals));
    }
    if action == "emit_event" {
        if actor["isLead"] != true {
            return Err(CallbackError::Invalid("Only the lead can emit an event"));
        }
        let name = event_name(&args["event"])?;
        let request_id = args["request_id"]
            .as_str()
            .filter(|id| (1..=MAX_REQUEST_ID_CHARS).contains(&id.chars().count()))
            .ok_or(CallbackError::Invalid("Supply a stable request_id"))?;
        let body = json!({"action":action,"event":name});
        let signature = event_write::request_signature(&body);
        let request_key = format!("park-request:{root}:{request_id}");
        if let Some(previous) = receipt(tx, &request_key, &signature)? {
            return Ok((previous, signals));
        }
        let event_key = format!("park-event:{root}:{name}");
        if let Some(mut fired) = receipt(tx, &event_key, &signature)? {
            fired["replayed"] = json!(true);
            return Ok((save_receipt(tx, &request_key, &signature, fired)?, signals));
        }
        let mut woken = Vec::new();
        for mut agent in team(tx, root, false)? {
            let park = agent.get("parkReceipt").cloned().unwrap_or(Value::Null);
            if agent["rootId"] != root
                || agent["parkedEvent"] != name
                || park["event"] != name
                || park["epoch"] != agent["epoch"]
                || park["sequence"] != agent["parkSequence"]
            {
                continue;
            }
            let id = text(&agent, "id")?.to_owned();
            resume(&mut agent)?;
            wake(
                tx,
                &agent,
                format!("park-wake:{root}:{name}:{id}"),
                format!("Event {name} occurred. Continue your assigned work."),
                now,
                &mut signals,
            )?;
            woken.push(json!(id));
        }
        let result = json!({"event":name,"count":woken.len(),"woken":woken});
        save_receipt(tx, &event_key, &signature, result.clone())?;
        return Ok((save_receipt(tx, &request_key, &signature, result)?, signals));
    }
    let mut target = target(tx, args)?;
    park_target(tx, actor, &target)?;
    let target_id = text(&target, "id")?.to_owned();
    if action == "park" {
        let name = event_name(&args["event"])?;
        let fired = json!({"action":"emit_event","event":name});
        if receipt(
            tx,
            &format!("park-event:{root}:{name}"),
            &event_write::request_signature(&fired),
        )?
        .is_some()
        {
            return Err(CallbackError::Invalid("This event already occurred"));
        }
        if studio_runtime_store::truthy(target.get("parkedEvent")) {
            if target["parkedEvent"] != name {
                return Err(CallbackError::Invalid(
                    "Cancel the current event wait first",
                ));
            }
            let value = json!({"status":target["status"],"agent":crate::agent_management::brief(&target),"replayed":true});
            return Ok((value, signals));
        }
        let pending: bool = tx.query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND epoch=?2 AND {OPEN_INPUT})"
            ),
            params![target_id, target["epoch"].as_i64()],
            |row| row.get(0),
        )?;
        if pending {
            return Err(CallbackError::Invalid(
                "Deliver or resolve pending input before parking",
            ));
        }
        if studio_runtime_store::truthy(target.get("inFlight")) {
            target["parkAfterTurn"] = json!(true);
        } else {
            target["autoWake"] = json!(false);
            target["status"] = json!("parked");
        }
        let sequence = target["parkSequence"].as_i64().unwrap_or(0) + 1;
        target["parkSequence"] = json!(sequence);
        target["parkedEvent"] = json!(name);
        target["parkReceipt"] = json!({"event":name,"epoch":target["epoch"],"sequence":sequence});
        super::super::super::storage::save_actor(tx, &target)?;
        let value =
            json!({"status":target["status"],"agent":crate::agent_management::brief(&target)});
        return Ok((value, signals));
    }
    // cancel_park
    if !studio_runtime_store::truthy(target.get("parkedEvent")) {
        let value = json!({"status":target["status"],"agent":crate::agent_management::brief(&target),"replayed":true});
        return Ok((value, signals));
    }
    let old = text(&target, "parkedEvent")?.to_owned();
    resume(&mut target)?;
    let sequence = target["parkSequence"].as_i64().unwrap_or(0);
    wake(
        tx,
        &target,
        format!("park-cancel:{target_id}:{sequence}"),
        format!("The wait for event {old} was cancelled. Continue your assigned work."),
        now,
        &mut signals,
    )?;
    // Python returns the record after `enqueue` updated its status.
    let current = records::get(tx, "runtime_agents", &target_id)?.ok_or(CallbackError::Stale)?;
    let value =
        json!({"status":current["status"],"agent":crate::agent_management::brief(&current)});
    Ok((value, signals))
}

/// End an event wait: Python clears the wait fields and turns a parked worker
/// back into a queued one before it enqueues the wake input.
fn resume(agent: &mut Value) -> Result<(), CallbackError> {
    let object = agent
        .as_object_mut()
        .ok_or(CallbackError::Invalid("Invalid agent record"))?;
    object.remove("parkedEvent");
    object.remove("parkAfterTurn");
    object.remove("parkReceipt");
    object.insert("autoWake".to_owned(), json!(true));
    if object.get("status").and_then(Value::as_str) == Some("parked") {
        object.insert("status".to_owned(), json!("queued"));
    }
    Ok(())
}
