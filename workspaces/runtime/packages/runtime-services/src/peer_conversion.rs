//! User-only local peer conversion. All idle checks, ownership moves and receipt
//! writes share one transaction. No provider, process or filesystem effects.
use crate::event_write::{self, truthy};
use crate::host_project_runtime::PreparedHost;
use crate::projects::operations::{
    is_project_lead, live_teams, peer_team_projection, project_agents, replay,
    save_operation_receipt,
};
use crate::projects::{ProjectsError, project_key};
use rusqlite::{Connection, Transaction, params};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use studio_runtime_store::{
    ProjectsRepository, RuntimeStore, SyncDocumentsRepository, system_reads as records,
};

const MOVE_FIELDS: &[(&str, &[&str])] = &[
    ("runtime_work", &["rootId"]),
    ("runtime_plans", &["rootId"]),
    ("runtime_annotations", &["rootId"]),
    ("runtime_complaints", &["leadId"]),
    ("runtime_event_meta", &["rootId", "leadId"]),
    ("runtime_rules", &["rootId"]),
];

/// Called only by the authenticated user HTTP route, never by model tools.
pub fn convert(store: &RuntimeStore, body: &Value) -> Result<Value, ProjectsError> {
    let allowed = [
        "action",
        "path",
        "member",
        "target",
        "expected_revision",
        "request_id",
    ];
    if body
        .as_object()
        .is_none_or(|object| object.keys().any(|key| !allowed.contains(&key.as_str())))
    {
        return Err(ProjectsError::Invalid("Unknown chat conversion field"));
    }
    let path = project_key(body.get("path"), false)?;
    let source = field(body, "member")?;
    let target = field(body, "target")?;
    if source == target {
        return Err(ProjectsError::Invalid("Select two different chats"));
    }
    let request = field(body, "request_id")?;
    let revision = body["expected_revision"]
        .as_u64()
        .ok_or(ProjectsError::Invalid(
            "Supply the current peer team revision",
        ))?;
    let key = format!("peer-convert:{request}");
    let signature =
        event_write::request_signature(&json!({"operation":"peer-convert","body":body}));
    if let Some(result) =
        store.transaction_with(|db| replay(db, &key, &signature, "peer conversion"))?
    {
        return Ok(result);
    }
    let next_revision = revision
        .checked_add(1)
        .ok_or(ProjectsError::Invalid("peer team revision exhausted"))?;
    let prepared = PreparedHost::load(store)?;
    prepared.recheck_paths()?;
    store.transaction_with(|db| {
        if let Some(result)=replay(db,&key,&signature,"peer conversion")? { return Ok(result); }
        prepared.recheck(db)?;
        let all=project_agents(db)?;
        let src=all.get(source).ok_or(ProjectsError::Invalid("source chat missing"))?;
        let dst=all.get(target).ok_or(ProjectsError::Invalid("destination chat missing"))?;
        for a in [src,dst] {
            if !is_project_lead(a,&path,&prepared) || truthy(a.get("sharedRoomId")) || truthy(a.get("archived")) { return Err(ProjectsError::Invalid("Select two live lead chats from this project only")); }
            if ["remote", "remoteWorker", "remoteAnchor"].iter().any(|field|truthy(a.get(*field))) { return Err(ProjectsError::Requires("cross-server-driver/peer-team-transport")); }
        }
        let actor=event_write::checked_agent(db,target).map_err(event_error)?;
        event_write::assert_delegation(&actor).map_err(event_error)?;
        let mut project=ProjectsRepository::get(db,&path)?.ok_or(ProjectsError::Invalid("select an existing project"))?.1;
        if project.get("peerTeamsRevision").map_or(Some(0),Value::as_u64)!=Some(revision) { return Err(ProjectsError::Invalid("Peer teams changed. Reload before moving")); }
        let teams=live_teams(project["peerTeams"].as_array().map(Vec::as_slice).unwrap_or_default(),&all,&path,&prepared)?;
        if !teams.iter().any(|t|t["members"].as_array().is_some_and(|m|m.contains(&json!(source)))) { return Err(ProjectsError::Invalid("The source chat must belong to a peer team")); }
        let tree=records::all(db,"runtime_agents")?.into_iter().filter(|a|[Some(source),Some(target)].contains(&a["rootId"].as_str())).collect::<Vec<_>>();
        idle(db,&tree)?;
        let moving=tree.iter().filter(|a|a["rootId"]==source).collect::<Vec<_>>();
        let moving_ids=moving.iter().filter_map(|a|a["id"].as_str()).collect::<Vec<_>>();
        let affected=records::all(db,"runtime_rooms")?.into_iter().filter(|r| [Some(source),Some(target)].contains(&r["rootId"].as_str()) || r["members"].as_array().is_some_and(|m|m.iter().any(|id|moving_ids.contains(&id.as_str().unwrap_or_default()) || teams.iter().any(|t|t["members"].as_array().is_some_and(|members|members.contains(id)))))).collect::<Vec<_>>();
        let now=crate::projects::now_seconds();
        let mut next_teams=Vec::new();
        for team in &teams {
            let mut team=team.clone();
            let members=team["members"].as_array_mut().ok_or(ProjectsError::Invalid("stored members invalid"))?;
            members.retain(|m|m!=source);
            if members.len()>=2 { next_teams.push(team); }
        }
        project["peerTeams"]=json!(next_teams); project["peerTeamsRevision"]=json!(next_revision); project["updated"]=json!(now);
        ProjectsRepository::put_project(db,&project)?;
        for original in &moving {
            let mut agent=(*original).clone();
            agent["rootId"]=json!(target); agent["updated"]=json!(now);
            let object=agent.as_object_mut().ok_or(ProjectsError::Invalid("stored agent invalid"))?;
            object.remove("concurrency");
            for field in ["maxAgents","tokenBudget"] { if let Some(v)=dst.get(field) {object.insert(field.to_owned(),v.clone());} }
            if original["id"]==source {
                for field in ["subagentConcurrencyVersion","maxAgentsExplicit","agentMode","agentModeRevision","agentModeSupported","projectFolder","pinned"] {object.remove(field);}
                agent["isLead"]=json!(false); agent["parentId"]=json!(target); agent["role"]=json!("implementer"); agent["status"]=json!("paused"); agent["autoWake"]=json!(false); agent["worktree"]=json!(false);
                agent["epoch"]=json!(original["epoch"].as_u64().unwrap_or(0).checked_add(1).ok_or(ProjectsError::Invalid("agent epoch exhausted"))?);
                agent["convertedFromLead"]=json!({"requestId":request,"by":"user","at":now,"oldRootId":source,"rootId":target});
            }
            put(db,"runtime_agents",&agent)?;
            let context=crate::sync_entity_context::agent_view_context(db,&agent,prepared.server().ok())?;
            let view=crate::sync_views::agent_entity_view(&agent,&context).ok_or(ProjectsError::Invalid("agent projection unavailable"))?;
            SyncDocumentsRepository::put_entity(db,"agent",agent["id"].as_str().unwrap_or_default(),view,truthy(agent.get("deletedAt")))?;
        }
        for (table,fields) in MOVE_FIELDS {
            for mut record in records::all(db,table)? {
                if !fields.iter().any(|field|record[*field]==source) {continue;}
                for field in *fields { if record[*field]==source {record[*field]=json!(target);} }
                if *table=="runtime_complaints" && record.get("leadName").is_some() {record["leadName"]=dst.get("name").filter(|v|truthy(Some(v))).cloned().unwrap_or(json!(target));}
                if *table=="runtime_rules" && record["agent"]==source {record["status"]=json!("paused");record["epoch"]=json!(src["epoch"].as_u64().unwrap_or(0).checked_add(1).ok_or(ProjectsError::Invalid("agent epoch exhausted"))?);record["error"]=json!("The lead became a subagent. Resume this watch explicitly.");}
                put(db,table,&record)?;
            }
        }
        for mut room in affected {
            if room["rootId"]==source {
                room["rootId"]=json!(target); room["updated"]=json!(now);
                if room["kind"]=="broadcast" {room["kind"]=json!("private");room["members"]=json!(moving.iter().filter(|a|!truthy(a.get("deletedAt"))).map(|a|a["id"].clone()).collect::<Vec<_>>());room["customName"]=json!(format!("{} (previous broadcast)",src["name"].as_str().filter(|v|!v.is_empty()).unwrap_or(source)));}
                put(db,"runtime_rooms",&room)?;
            }
            crate::radio_runtime::publish_room(db,&room,&prepared)?;
        }
        for team in &teams {
            let id=team["id"].as_str().ok_or(ProjectsError::Invalid("team ID invalid"))?;
            let next=next_teams.iter().find(|t|t["id"]==id);
            let view=next.map(|t|peer_team_projection(t,&path,next_revision)).transpose()?;
            SyncDocumentsRepository::put_entity(db,"peerTeam",id,view.unwrap_or(json!({})),next.is_none())?;
        }
        let result=json!({"id":source,"parentId":target,"rootId":target,"movedAgents":moving_ids,"peerTeamsRevision":next_revision});
        save_operation_receipt(db,&key,&signature,&result)?;
        Ok(result)
    })
}
fn field<'a>(body: &'a Value, field: &str) -> Result<&'a str, ProjectsError> {
    body[field]
        .as_str()
        .map(event_write::python_strip)
        .filter(|v| !v.is_empty() && v.chars().count() <= 255)
        .ok_or(ProjectsError::Invalid("chat conversion field invalid"))
}
fn event_error(error: event_write::EventWriteError) -> ProjectsError {
    match error {
        event_write::EventWriteError::Store(error) => ProjectsError::Store(error),
        event_write::EventWriteError::InvalidRequest(reason)
        | event_write::EventWriteError::Conflict(reason) => ProjectsError::Invalid(reason),
        _ => ProjectsError::Requires("cross-server-driver/peer-team-transport"),
    }
}
fn put(db: &Transaction<'_>, table: &'static str, value: &Value) -> Result<(), ProjectsError> {
    let id = value["id"]
        .as_str()
        .ok_or(ProjectsError::Invalid("record ID missing"))?;
    records::validate(table, id, value.clone())?;
    db.execute(
        &format!("UPDATE {table} SET record=?1 WHERE id=?2"),
        params![value.to_string(), id],
    )?;
    Ok(())
}
fn idle(db: &Connection, agents: &[Value]) -> Result<(), ProjectsError> {
    let ids = agents
        .iter()
        .filter_map(|a| a["id"].as_str())
        .collect::<BTreeSet<_>>();
    for a in agents {
        if truthy(a.get("inFlight"))
            || truthy(a.get("turnId"))
            || matches!(
                a["status"].as_str(),
                Some("running" | "starting" | "approval")
            )
            || (a["status"] == "queued" && truthy(a.get("autoWake")))
            || truthy(a.get("startAttempt"))
            || truthy(a.get("prepareAttempt"))
        {
            return Err(ProjectsError::Invalid(
                "Finish the active or queued turn before moving",
            ));
        }
        if truthy(a.get("workspaceOperation")) {
            return Err(ProjectsError::Invalid(
                "Finish the pending workspace operation before moving",
            ));
        }
        if truthy(a.get("accountTransferId"))
            || truthy(a.get("lazyAccountTransfer"))
            || a["accountTransfer"]["status"] == "pending"
        {
            return Err(ProjectsError::Invalid(
                "Finish the account transfer before moving",
            ));
        }
        if matches!(
            a["nativeRelease"]["phase"].as_str(),
            Some("pending" | "checking" | "releasing")
        ) {
            return Err(ProjectsError::Invalid(
                "Finish the native release before moving",
            ));
        }
        if db.query_row("SELECT EXISTS(SELECT 1 FROM runtime_events WHERE agent=?1 AND status IN ('pending','reserved','dispatching','uncertain'))",[a["id"].as_str().unwrap_or_default()],|r|r.get::<_,bool>(0))? {return Err(ProjectsError::Invalid("Finish pending input before moving these chats"));}
    }
    for table in [
        "runtime_tasks",
        "runtime_monitors",
        "runtime_requests",
        "runtime_tool_requests",
        "runtime_rules",
        "runtime_workspace_operations",
        "runtime_account_transfers",
        "runtime_rooms",
    ] {
        for r in records::all(db, table)? {
            let belongs = ids.contains(r["agent"].as_str().unwrap_or_default());
            let busy = match table {
                "runtime_tasks" | "runtime_monitors" => {
                    belongs
                        && matches!(
                            r["status"].as_str(),
                            Some("running" | "starting" | "approval")
                        )
                }
                "runtime_requests" => belongs && r["status"] == "pending",
                "runtime_tool_requests" => {
                    belongs
                        && !truthy(r.get("readOnly"))
                        && matches!(r["outcome"].as_str(), Some("pending" | "unknown"))
                }
                "runtime_rules" => belongs && r["inFlight"] == true,
                "runtime_workspace_operations" => {
                    belongs
                        && !matches!(
                            r["phase"].as_str(),
                            Some("complete" | "completed" | "failed" | "cancelled")
                        )
                }
                "runtime_account_transfers" => {
                    r["status"] == "pending"
                        && (ids.contains(r["leadId"].as_str().unwrap_or_default())
                            || r["members"]
                                .as_object()
                                .is_some_and(|m| m.keys().any(|id| ids.contains(id.as_str()))))
                }
                "runtime_rooms" => {
                    r["members"].as_array().is_some_and(|m| {
                        m.iter()
                            .any(|id| ids.contains(id.as_str().unwrap_or_default()))
                    }) && (truthy(r["radio"].get("active")) || truthy(r["radio"].get("next")))
                }
                _ => false,
            };
            if busy {
                return Err(ProjectsError::Invalid(
                    "Finish pending operations before moving these chats",
                ));
            }
        }
    }
    Ok(())
}
