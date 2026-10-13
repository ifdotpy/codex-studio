//! Closed local service table. Every entry checks actor authority in the same
//! transaction as its effects. No HTTP adapters and no user-authored substitutes.
mod messages;
mod reads;
mod work;
use super::*;
use crate::{event_write, work_policy};
use studio_runtime_store::WorkItemsRepository;

impl From<event_write::EventWriteError> for CallbackError {
    fn from(error: event_write::EventWriteError) -> Self {
        use event_write::EventWriteError as E;
        match error {
            E::Store(error) => Self::Store(error),
            E::InvalidRequest(message) | E::Conflict(message) => Self::Invalid(message),
            E::WorkerAssignmentUnavailable => Self::Dependency("worker-driver/work-assignment"),
            _ => Self::Dependency("event-write/required-runtime-driver"),
        }
    }
}
impl From<crate::coordination::CoordinationError> for CallbackError {
    fn from(error: crate::coordination::CoordinationError) -> Self {
        use crate::coordination::CoordinationError as E;
        match error {
            E::Store(error) => Self::Store(error),
            E::InvalidRequest(message) | E::Conflict(message) => Self::Invalid(message),
            E::RequiresDependency(name) => Self::Dependency(name),
            E::InvalidStored { object, id } => Self::Stored(format!("{object}/{id}")),
        }
    }
}
pub(super) fn read_only(name: &str, args: &Value) -> bool {
    matches!(
        name,
        "orchestration_read"
            | "orchestration_peers"
            | "orchestration_search"
            | "orchestration_chat_read"
    ) || name == "orchestration_task"
        && matches!(
            args["action"].as_str().unwrap_or("list"),
            "list" | "get" | "read" | "history"
        )
        || name == "orchestration_request"
            && matches!(args["action"].as_str().unwrap_or("list"), "list" | "get")
}
pub(super) fn execute(
    tx: &Transaction<'_>,
    snapshot: &CallbackSnapshot,
    kind: ToolKind,
    name: &str,
    args: &Value,
    id: &str,
    now: f64,
) -> Result<Value, CallbackError> {
    let actor = snapshot.actor();
    if studio_runtime_store::truthy(actor.get("executionMove"))
        && !matches!(
            name,
            "orchestration_move" | "orchestration_teleport" | "orchestration_request"
        )
    {
        return Err(CallbackError::Invalid(
            "The move is accepted. Finish this turn before the next task",
        ));
    }
    if studio_runtime_store::truthy(actor.get("remoteOrigin")) && name != "orchestration_request" {
        return Err(CallbackError::Dependency("multi-server/worker-call"));
    }
    match kind {
        ToolKind::Request => Ok(tool_requests::execute(
            tx,
            &snapshot.context().agent,
            match args["action"].as_str().unwrap_or("list") {
                "list" => tool_requests::Action::List,
                "get" => tool_requests::Action::Get(text(args, "request_id")?),
                "cancel" => tool_requests::Action::Cancel(text(args, "request_id")?),
                _ => return Err(CallbackError::Invalid("Invalid request action")),
            },
            now,
        )?),
        ToolKind::Task => work::execute(tx, snapshot, args, id),
        ToolKind::Message => messages::send(tx, snapshot, args, id, now),
        ToolKind::ChatRead => reads::chat(tx, snapshot, args),
        ToolKind::Context if args["topic"] == "plan" => {
            let plan = WorkItemsRepository::plan(tx, text(actor, "rootId")?)?
                .and_then(|r| r.record)
                .map(|raw| serde_json::from_str::<Value>(&raw))
                .transpose()?;
            let content = plan.filter(|p| p["native"].is_object()).map(|p| json!({
                "id":p["id"], "rootId":p["rootId"],
                "steps":p["native"].get("plan").cloned().unwrap_or(json!([])),
                "explanation":p["native"].get("explanation").filter(|v| studio_runtime_store::truthy(Some(v))).cloned().unwrap_or(json!(""))
            })).unwrap_or(Value::Null);
            let version =
                studio_runtime_store::sha256_hex(content.to_string().as_bytes())[..24].to_owned();
            Ok(json!({"topic":"plan","version":version,"content":content}))
        }
        ToolKind::Peers => messages::peers(tx, snapshot, args),
        ToolKind::Status => Err(CallbackError::Dependency(
            "model-directory/status-and-dispatch-capacity",
        )),
        ToolKind::Read => reads::read(tx, snapshot, args, now),
        ToolKind::Search => Err(CallbackError::Dependency(
            "model-search/authorized-cross-source-search",
        )),
        ToolKind::Context => Err(CallbackError::Dependency("model-context/requested-topic")),
        ToolKind::Send => Err(CallbackError::Dependency(
            "agent-management/resume-and-stop-revocation",
        )),
        ToolKind::Spawn => Err(CallbackError::Dependency("agent-create/worker-driver")),
        ToolKind::Review => Err(CallbackError::Dependency("native-review/worker-driver")),
        ToolKind::Monitor | ToolKind::MonitorInput | ToolKind::CancelMonitor => {
            Err(CallbackError::Dependency("monitors/runtime-driver"))
        }
        ToolKind::Move | ToolKind::Teleport => Err(CallbackError::Dependency(
            "agent-move/exact-native-history-and-paired-driver",
        )),
        ToolKind::Servers => Err(CallbackError::Dependency("multi-server/tool-coordinator")),
        ToolKind::Interrupt | ToolKind::AgentManage => {
            Err(CallbackError::Dependency("agent-management/runtime-driver"))
        }
        ToolKind::Watch => Err(CallbackError::Dependency("rules/runtime-driver")),
        ToolKind::Complaint => Err(CallbackError::Dependency("complaints/agent-authority")),
        ToolKind::Speak => Err(CallbackError::Dependency("voice/provider-session")),
        ToolKind::HostExec => Err(CallbackError::Dependency("host-exec/layr-bridge")),
        ToolKind::Title => {
            if actor["isLead"] != true {
                return Err(CallbackError::Invalid(
                    "Only the orchestrator can set the conversation title",
                ));
            }
            let title = text(args, "title")?.trim();
            if title.is_empty() || title.chars().count() > 80 {
                return Err(CallbackError::Invalid(
                    "Only a lead can set a title of 1 to 80 characters",
                ));
            }
            let mut updated = actor.clone();
            if updated["manualName"] != true {
                updated["name"] = json!(title);
            }
            updated["needsTitle"] = json!(false);
            crate::agents_create::publish_agent(tx, &updated)?;
            Ok(json!({"title":updated["name"]}))
        }
        ToolKind::Unknown => Err(CallbackError::Invalid(
            "Unknown orchestration tool; use a tool from the session catalog",
        )),
    }
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str, CallbackError> {
    v[key]
        .as_str()
        .ok_or(CallbackError::Invalid("Missing required text argument"))
}
fn brief(value: &Value) -> Value {
    let fields = ["id", "title", "owner", "status", "version", "blockedBy"];
    Value::Object(
        fields
            .into_iter()
            .map(|key| (key.into(), value[key].clone()))
            .collect(),
    )
}
fn page(rows: Vec<Value>, args: &Value, scope: Value) -> Result<Value, CallbackError> {
    use base64::Engine;
    let limit = match args.get("limit") {
        None => 20,
        Some(v) => v
            .as_u64()
            .ok_or(CallbackError::Invalid("limit must be 1 to 50"))? as usize,
    };
    if !(1..=50).contains(&limit) {
        return Err(CallbackError::Invalid("limit must be 1 to 50"));
    }
    let mut revision_rows = rows.clone();
    for row in &mut revision_rows {
        if row["entry"] == "agent"
            && let Some(value) = row["value"].as_object_mut()
        {
            value.remove("status");
        }
    }
    let revision =
        studio_runtime_store::sha256_hex(json!([scope, revision_rows]).to_string().as_bytes())
            [..24]
            .to_owned();
    let offset = if let Some(cursor) = args["cursor"].as_str() {
        let invalid = || {
            CallbackError::Invalid("List changed or cursor is invalid. Read the first page again.")
        };
        let raw = base64::engine::general_purpose::URL_SAFE
            .decode(cursor)
            .map_err(|_| invalid())?;
        let cursor: Value = serde_json::from_slice(&raw).map_err(|_| invalid())?;
        let offset = cursor["offset"].as_u64().ok_or_else(invalid)? as usize;
        if offset > rows.len() || cursor["revision"] != revision {
            return Err(invalid());
        }
        offset
    } else {
        0
    };
    let mut end = (offset + limit).min(rows.len());
    while end > offset + 1
        && json!([{"type":"inputText","text":json!(rows[offset..end]).to_string()}])
            .to_string()
            .len()
            > 13000
    {
        end -= 1;
    }
    let cursor = (end < rows.len()).then(|| {
        base64::engine::general_purpose::URL_SAFE
            .encode(json!({"offset":end,"revision":revision}).to_string())
    });
    Ok(
        json!({"apiVersion":2,"items":rows[offset..end],"total":rows.len(),"revision":revision,"nextCursor":cursor}),
    )
}
