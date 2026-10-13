use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde_json::{Value, json};
use studio_runtime_services::work_policy;
use studio_runtime_services::workspace_files::WorkspaceContext;
use studio_runtime_services::workspace_views::{CursorKind, parse_task_cursor, task_feed_response};
use studio_runtime_store::{
    Connection, StoredRecord, TranscriptRepository, WorkspaceViewsRepository,
};

use crate::error::ApiError;
use crate::router::HttpState;
use crate::validation::Registry;

use super::extractors::QueryParams;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get};

const CHANGES_DEPENDENCIES: [Dependency; 1] = [Dependency::new("workspace-state/image-execution")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        get(
            "/api/changes",
            OperationStatus::partial(&CHANGES_DEPENDENCIES),
            changes,
        ),
        get("/api/task", OperationStatus::Implemented, task_detail),
        get("/api/workspace", OperationStatus::Implemented, workspace),
        get(
            "/api/workspace/tasks",
            OperationStatus::Implemented,
            workspace_tasks,
        ),
    ])
}

async fn changes(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let agent = query.nonempty_str("agent").map(str::to_owned);
    let scope = query.nonempty_str("scope").map(str::to_owned);
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let value = tokio::task::spawn_blocking(move || {
        let mut connection = store.read().map_err(|_| ApiError::internal())?;
        let transaction = connection.transaction().map_err(|_| ApiError::internal())?;
        let actor_id = agent.as_deref().ok_or_else(invalid_request)?;
        let actor = WorkspaceViewsRepository::agent(&transaction, actor_id)
            .map_err(|_| invalid_stored("runtime_agents", actor_id))?
            .ok_or_else(invalid_request)?;
        let actor = checked_stored(
            &state.validators,
            "AgentRecordResponse",
            "runtime_agents",
            actor_id,
            actor,
        )?;
        if deleted(&actor) {
            return Err(invalid_request());
        }
        let result = if scope.as_deref() == Some("chat") {
            reported_changes(&transaction, actor_id)?
        } else if scope.is_some() {
            return Err(ApiError::validation(vec![json!("query"), json!("scope")]));
        } else {
            let context = WorkspaceContext::from_agent(&actor, actor_id)
                .map_err(|_| invalid_stored("runtime_agents", actor_id))?;
            if context.execution_backed {
                return Err(dependency_error(
                    "GET",
                    "/api/changes",
                    &CHANGES_DEPENDENCIES,
                ));
            }
            git_changes(&context.cwd)?
        };
        transaction.commit().map_err(|_| ApiError::internal())?;
        Ok::<Value, ApiError>(result)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json::<
        crate::generated::ChangesResponse,
    >(value)?))
}

async fn workspace(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let agent = query.nonempty_str("agent").map(str::to_owned);
    let view = query.nonempty_str("view").unwrap_or("full").to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let validators = state.validators.clone();
    let value = tokio::task::spawn_blocking(move || {
        let mut connection = store.read().map_err(|_| ApiError::internal())?;
        let transaction = connection.transaction().map_err(|_| ApiError::internal())?;
        let value = workspace_view(&transaction, &validators, agent.as_deref(), &view)?;
        transaction.commit().map_err(|_| ApiError::internal())?;
        Ok::<Value, ApiError>(value)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(super::generated_json::<
        crate::generated::WorkspaceView,
    >(value)?))
}

fn git_changes(cwd: &str) -> Result<Value, ApiError> {
    match run_git(cwd, &["rev-parse", "--show-toplevel"]) {
        Ok(_) => {}
        Err(GitCommandError::Failed(error)) => {
            return Ok(json!({"files": [], "patch": "", "git": false, "error": error}));
        }
        Err(error) => return Err(git_unexpected(error)),
    }
    let raw_status = match run_git(cwd, &["status", "--porcelain=v1", "-z"]) {
        Ok(status) => status,
        Err(GitCommandError::Failed(error)) => return Ok(git_unavailable(error)),
        Err(error) => return Err(git_unexpected(error)),
    };
    let entries = String::from_utf8_lossy(&raw_status)
        .split('\0')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        let entry = &entries[index];
        index += 1;
        if entry.is_empty() {
            continue;
        }
        let status = entry.get(..2).unwrap_or_default().trim();
        let path = entry.get(3..).unwrap_or_default();
        files.push(json!({"path":path,"status":status}));
        if status.contains('R') || status.contains('C') {
            index += 1;
        }
    }
    let patch = match run_git(
        cwd,
        &["diff", "HEAD", "--no-ext-diff", "--no-color", "--unified=3"],
    ) {
        Ok(patch) => patch,
        Err(GitCommandError::Failed(error)) => return Ok(git_unavailable(error)),
        Err(error) => return Err(git_unexpected(error)),
    };
    let patch = String::from_utf8_lossy(&patch).into_owned();
    let revision = match run_git(cwd, &["rev-parse", "HEAD"]) {
        Ok(revision) => revision,
        Err(GitCommandError::Failed(error)) => return Ok(git_unavailable(error)),
        Err(error) => return Err(git_unexpected(error)),
    };
    let revision = String::from_utf8_lossy(&revision).trim().to_owned();
    let truncated = patch.chars().count() > 300_000;
    let patch = patch.chars().take(300_000).collect::<String>();
    Ok(json!({
        "files": files,
        "diff": patch,
        "patch": patch,
        "truncated": truncated,
        "revision": revision,
        "git": true,
    }))
}

fn git_unavailable(error: String) -> Value {
    json!({"files": [], "patch": "", "git": false, "error": error})
}

enum GitCommandError {
    Failed(String),
    TimedOut,
    Other,
}

fn run_git(cwd: &str, args: &[&str]) -> Result<Vec<u8>, GitCommandError> {
    let mut child = Command::new("git")
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|_| GitCommandError::Other)?;
    let stdout = child.stdout.take().ok_or(GitCommandError::Other)?;
    let stderr = child.stderr.take().ok_or(GitCommandError::Other)?;
    let stdout_reader = thread::spawn(move || read_pipe(stdout));
    let stderr_reader = thread::spawn(move || read_pipe(stderr));
    let deadline = Instant::now() + Duration::from_secs(30);
    let status = loop {
        match child.try_wait() {
            Err(_) => {
                drop(child.kill());
                drop(child.wait());
                drop(stdout_reader.join());
                drop(stderr_reader.join());
                return Err(GitCommandError::Other);
            }
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                drop(child.kill());
                drop(child.wait());
                drop(stdout_reader.join());
                drop(stderr_reader.join());
                return Err(GitCommandError::TimedOut);
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = stdout_reader
        .join()
        .map_err(|_| GitCommandError::Other)?
        .map_err(|_| GitCommandError::Other)?;
    let stderr = stderr_reader
        .join()
        .map_err(|_| GitCommandError::Other)?
        .map_err(|_| GitCommandError::Other)?;
    if status.success() {
        Ok(stdout)
    } else {
        let message = String::from_utf8_lossy(&stderr);
        let message = message.trim();
        let message = message.chars().take(1200).collect::<String>();
        Err(GitCommandError::Failed(if message.is_empty() {
            "Git operation failed".to_owned()
        } else {
            message
        }))
    }
}

fn read_pipe(mut pipe: impl Read) -> std::io::Result<Vec<u8>> {
    let mut output = Vec::new();
    pipe.read_to_end(&mut output)?;
    Ok(output)
}

fn git_unexpected(error: GitCommandError) -> ApiError {
    match error {
        GitCommandError::Failed(_) | GitCommandError::TimedOut | GitCommandError::Other => {
            ApiError::internal()
        }
    }
}

fn reported_changes(connection: &Connection, agent: &str) -> Result<Value, ApiError> {
    let prefix = format!("{agent}:turn/diff/updated:");
    let items = TranscriptRepository::items_for_agent(connection, agent)
        .map_err(|_| invalid_stored("runtime_items", agent))?;
    let Some(item) = items.into_iter().find(|item| {
        item.id == format!("{agent}:turn/diff/updated") || item.id.starts_with(&prefix)
    }) else {
        return Ok(empty_reported_changes());
    };
    if truthy(item.record.get("afterRestore")) {
        return Ok(empty_reported_changes());
    }
    let text = TranscriptRepository::full_text(connection, &item)
        .map_err(|_| invalid_stored("runtime_items", &item.id))?;
    if item.record.get("truncated").and_then(Value::as_bool) == Some(true) && text.is_none() {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "The complete reported changes are unavailable",
        ));
    }
    let raw = text.or_else(|| {
        item.record
            .get("text")
            .and_then(Value::as_str)
            .map(str::to_owned)
    });
    let payload: Value = serde_json::from_str(raw.as_deref().unwrap_or_default())
        .map_err(|_| ApiError::new(StatusCode::BAD_REQUEST, "The recorded changes are invalid"))?;
    let diff = payload.get("diff").and_then(Value::as_str).ok_or_else(|| {
        ApiError::new(StatusCode::BAD_REQUEST, "The recorded changes are invalid")
    })?;
    let files = reported_change_files(diff);
    let truncated = diff.chars().count() > 300_000;
    let diff = diff.chars().take(300_000).collect::<String>();
    Ok(json!({
        "scope":"chat", "git":true, "files":files, "diff":diff, "patch":diff,
        "truncated":truncated, "turnId":payload.get("turnId"), "reportedAt":item.record.get("at"),
    }))
}

fn empty_reported_changes() -> Value {
    json!({
        "scope":"chat", "git":true, "files":[], "diff":"", "patch":"",
        "truncated":false, "turnId":null, "reportedAt":null,
    })
}

fn reported_change_files(diff: &str) -> Vec<Value> {
    let mut files = Vec::<Value>::new();
    let mut old_lines = 0_u64;
    let mut new_lines = 0_u64;
    let mut previous: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("@@ ") {
            let mut parts = line.split_whitespace();
            let _ = parts.next();
            old_lines = hunk_lines(parts.next().unwrap_or_default());
            new_lines = hunk_lines(parts.next().unwrap_or_default());
            previous = None;
        } else if old_lines > 0 || new_lines > 0 {
            if line.starts_with('-') || line.starts_with(' ') {
                old_lines = old_lines.saturating_sub(1);
            }
            if line.starts_with('+') || line.starts_with(' ') {
                new_lines = new_lines.saturating_sub(1);
            }
        } else if let Some(path) = line.strip_prefix("--- ") {
            previous = header_path(path);
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if let Some(previous_path) = previous.take()
                && let Some(current_path) = header_path(path)
            {
                let is_deleted = current_path == "/dev/null";
                let path = if is_deleted {
                    previous_path.as_str()
                } else {
                    current_path.as_str()
                };
                let path = path
                    .strip_prefix("a/")
                    .or_else(|| path.strip_prefix("b/"))
                    .unwrap_or(path);
                if path != "/dev/null" {
                    let status = if is_deleted {
                        "D"
                    } else if previous_path == "/dev/null" {
                        "A"
                    } else {
                        "M"
                    };
                    if let Some(file) = files.iter_mut().find(|file| file["path"] == path) {
                        file["status"] = json!(status);
                    } else {
                        files.push(json!({"path":path,"status":status}));
                    }
                }
            }
        } else {
            previous = None;
        }
    }
    files
}

fn hunk_lines(value: &str) -> u64 {
    let value = value.trim_start_matches(['-', '+']);
    value
        .split_once(',')
        .map_or("1", |(_, count)| count)
        .parse()
        .unwrap_or(0)
}

fn header_path(path: &str) -> Option<String> {
    if !path.starts_with('"') {
        return path
            .split('\t')
            .next()
            .filter(|path| !path.is_empty())
            .map(str::to_owned);
    }
    let inner = path.strip_prefix('"')?.strip_suffix('"')?;
    let mut output = Vec::new();
    let mut bytes = inner.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            output.push(byte);
            continue;
        }
        let escaped = bytes.next()?;
        let decoded = match escaped {
            b'a' => 7,
            b'b' => 8,
            b'f' => 12,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 11,
            b'\\' => b'\\',
            b'"' => b'"',
            b'0'..=b'3' => {
                let second = bytes.next()?;
                let third = bytes.next()?;
                if !(b'0'..=b'7').contains(&second) || !(b'0'..=b'7').contains(&third) {
                    return None;
                }
                (escaped - b'0') * 64 + (second - b'0') * 8 + (third - b'0')
            }
            _ => return None,
        };
        output.push(decoded);
    }
    String::from_utf8(output).ok()
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        Some(Value::Null) | None => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|value| value != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn workspace_view(
    connection: &Connection,
    validators: &Registry,
    agent_id: Option<&str>,
    view: &str,
) -> Result<Value, ApiError> {
    if !matches!(view, "full" | "work" | "inbox" | "annotations") {
        return Err(ApiError::validation(vec![json!("query"), json!("view")]));
    }
    let stored_agents = WorkspaceViewsRepository::agents(connection)
        .map_err(|_| invalid_stored("runtime_agents", "unknown"))?;
    let agents = parse_records(
        validators,
        "AgentRecordResponse",
        "runtime_agents",
        stored_agents,
    )?;
    let root = if let Some(agent_id) = agent_id {
        let actor = agents
            .iter()
            .find(|agent| agent["id"] == agent_id)
            .ok_or_else(invalid_request)?;
        if deleted(actor) {
            return Err(invalid_request());
        }
        Some(
            actor["rootId"]
                .as_str()
                .ok_or_else(|| invalid_stored("runtime_agents", agent_id))?
                .to_owned(),
        )
    } else {
        None
    };
    let agents = agents
        .into_iter()
        .filter(|agent| {
            !deleted(agent) && root.as_ref().is_none_or(|root| agent["rootId"] == *root)
        })
        .collect::<Vec<_>>();
    let ids = agents
        .iter()
        .map(|agent| {
            agent["id"]
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| invalid_stored("runtime_agents", "unknown"))
        })
        .collect::<Result<Vec<_>, _>>()?;

    if view == "annotations" {
        let annotations = parse_records(
            validators,
            "AnnotationReceipt",
            "runtime_annotations",
            WorkspaceViewsRepository::records_for_agents(
                connection,
                "runtime_annotations",
                "agent",
                &ids,
            )
            .map_err(|_| invalid_stored("runtime_annotations", "unknown"))?,
        )?;
        return Ok(json!({"annotations": annotations}));
    }

    let all_work = parse_records(
        validators,
        "WorkItem",
        "runtime_work",
        WorkspaceViewsRepository::records(connection, "runtime_work")
            .map_err(|_| invalid_stored("runtime_work", "unknown"))?,
    )?;
    let statuses = all_work
        .iter()
        .filter_map(|item| {
            Some((
                item.get("id")?.as_str()?.to_owned(),
                item.get("status")?.as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    let root_view = root.as_deref().unwrap_or("");
    let work = all_work
        .iter()
        .filter(|item| {
            ids.contains(&item["rootId"].as_str().unwrap_or_default().to_owned())
                && (root.is_none() || item["rootId"] == root_view)
        })
        .map(|item| work_policy::work_item_view(item, &statuses))
        .collect::<Vec<_>>();
    if view == "work" {
        return Ok(json!({"work": work}));
    }

    let requests = parse_records(
        validators,
        "RequestRecordResponse",
        "runtime_requests",
        WorkspaceViewsRepository::records_for_agents(connection, "runtime_requests", "agent", &ids)
            .map_err(|_| invalid_stored("runtime_requests", "unknown"))?,
    )?;
    let complaints = if root.is_some() {
        parse_records(
            validators,
            "ComplaintRecordResponse",
            "runtime_complaints",
            WorkspaceViewsRepository::records_for_agents(
                connection,
                "runtime_complaints",
                "leadId",
                &ids,
            )
            .map_err(|_| invalid_stored("runtime_complaints", "unknown"))?,
        )?
    } else {
        parse_records(
            validators,
            "ComplaintRecordResponse",
            "runtime_complaints",
            WorkspaceViewsRepository::records(connection, "runtime_complaints")
                .map_err(|_| invalid_stored("runtime_complaints", "unknown"))?,
        )?
    };
    let rules = parse_records(
        validators,
        "RuleRecordResponse",
        "runtime_rules",
        WorkspaceViewsRepository::records_for_agents(connection, "runtime_rules", "agent", &ids)
            .map_err(|_| invalid_stored("runtime_rules", "unknown"))?,
    )?;
    let tasks = parse_records(
        validators,
        "TaskDetailResponse",
        "runtime_tasks",
        WorkspaceViewsRepository::snapshot_tasks(connection, root.as_deref())
            .map_err(|_| invalid_stored("runtime_tasks", "unknown"))?,
    )?
    .into_iter()
    .map(super::generated_json::<crate::generated::WorkspaceTaskRecord>)
    .collect::<Result<Vec<_>, _>>()?;
    let monitors = parse_records(
        validators,
        "MonitorEntityDto",
        "runtime_monitors",
        WorkspaceViewsRepository::snapshot_monitors(connection, root.as_deref())
            .map_err(|_| invalid_stored("runtime_monitors", "unknown"))?,
    )?
    .into_iter()
    .filter(|monitor| {
        monitor["agent"]
            .as_str()
            .is_some_and(|agent| ids.iter().any(|id| id == agent))
    })
    .collect::<Vec<_>>();
    let inbox = workspace_inbox(
        &agents,
        &requests,
        &complaints,
        &all_work,
        &monitors,
        &rules,
    )?;
    if view == "inbox" {
        return Ok(json!({"inbox": inbox}));
    }

    let annotations = parse_records(
        validators,
        "AnnotationReceipt",
        "runtime_annotations",
        WorkspaceViewsRepository::records_for_agents(
            connection,
            "runtime_annotations",
            "agent",
            &ids,
        )
        .map_err(|_| invalid_stored("runtime_annotations", "unknown"))?,
    )?;
    let checkpoint_ids = agent_id
        .map(|id| vec![id.to_owned()])
        .unwrap_or_else(|| ids.clone());
    let checkpoints = parse_records(
        validators,
        "CheckpointSummary",
        "runtime_checkpoints",
        WorkspaceViewsRepository::records_for_agents(
            connection,
            "runtime_checkpoints",
            "agent",
            &checkpoint_ids,
        )
        .map_err(|_| invalid_stored("runtime_checkpoints", "unknown"))?,
    )?
    .into_iter()
    .map(|mut record| {
        if let Some(record) = record.as_object_mut() {
            record.remove("items");
            record.remove("historyDelta");
            record.remove("_payloadBlobs");
        }
        record
    })
    .collect::<Vec<_>>();
    let plans = parse_records(
        validators,
        "PlanView",
        "runtime_plans",
        WorkspaceViewsRepository::records_for_agents(connection, "runtime_plans", "id", &ids)
            .map_err(|_| invalid_stored("runtime_plans", "unknown"))?,
    )?
    .into_iter()
    .filter(|plan| root.as_ref().is_none_or(|root| plan["rootId"] == *root))
    .collect::<Vec<_>>();
    Ok(json!({
        "work": work,
        "annotations": annotations,
        "checkpoints": checkpoints,
        "plans": plans,
        "rules": rules,
        "inbox": inbox,
        "tasks": tasks,
        "tasksHistoryLimit": 100,
        "monitors": monitors,
    }))
}

fn parse_records(
    validators: &Registry,
    schema: &str,
    table: &str,
    records: Vec<StoredRecord>,
) -> Result<Vec<Value>, ApiError> {
    records
        .into_iter()
        .map(|row| {
            let raw = row.record.ok_or_else(|| invalid_stored(table, &row.id))?;
            let value: Value =
                serde_json::from_str(&raw).map_err(|_| invalid_stored(table, &row.id))?;
            checked_stored(validators, schema, table, &row.id, value)
        })
        .collect()
}

fn workspace_inbox(
    agents: &[Value],
    requests: &[Value],
    complaints: &[Value],
    work: &[Value],
    monitors: &[Value],
    rules: &[Value],
) -> Result<Vec<Value>, ApiError> {
    let mut inbox = Vec::new();
    for request in requests {
        if request["status"] == "pending" && !truthy(request.get("deferred")) {
            inbox.push(json!({
                "id":request["id"],"kind":"request","agent":request["agent"],
                "title":"Answer required","text":request["method"],"request":request,
            }));
        }
    }
    let mut complaints = complaints.to_vec();
    complaints.sort_by(|left, right| {
        let left_needs_response = complaint_needs_response(left);
        let right_needs_response = complaint_needs_response(right);
        right_needs_response
            .cmp(&left_needs_response)
            .then_with(|| {
                right
                    .get("updated")
                    .and_then(Value::as_f64)
                    .unwrap_or_default()
                    .total_cmp(
                        &left
                            .get("updated")
                            .and_then(Value::as_f64)
                            .unwrap_or_default(),
                    )
            })
    });
    for complaint in &complaints {
        if !complaint_needs_response(complaint) {
            continue;
        }
        let text = complaint["text"].as_str().ok_or_else(|| {
            invalid_stored(
                "runtime_complaints",
                complaint["id"].as_str().unwrap_or("unknown"),
            )
        })?;
        let id = complaint["id"].as_str().unwrap_or_default();
        let author = complaint["author"].as_str().unwrap_or_default();
        let lead_id = complaint["leadId"].as_str().unwrap_or_default();
        let author_record = agents.iter().find(|agent| agent["id"] == author);
        let lead = agents.iter().find(|agent| agent["id"] == lead_id);
        let mut projection = complaint.clone();
        projection["title"] = json!(text.chars().take(140).collect::<String>());
        projection["recipient"] = complaint.get("recipient").cloned().unwrap_or(Value::Null);
        projection["needsResponse"] = json!(true);
        projection["authorName"] = author_record
            .and_then(|agent| agent.get("name"))
            .cloned()
            .unwrap_or_else(|| json!(if author == "user" { "You" } else { author }));
        projection["leadName"] = lead
            .and_then(|agent| agent.get("name"))
            .cloned()
            .unwrap_or_else(|| json!(lead_id));
        projection["leadStopped"] = json!(
            !lead
                .and_then(|agent| agent.get("autoWake"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
        projection["leadDeleted"] = json!(lead.is_some_and(deleted));
        inbox.push(json!({
            "id":id,"kind":"complaint","agent":lead_id,"title":text.chars().take(140).collect::<String>(),
            "text":"The lead must read and respond","complaint":projection,
        }));
    }
    let statuses = work
        .iter()
        .filter_map(|item| {
            Some((
                item["id"].as_str()?.to_owned(),
                item["status"].as_str()?.to_owned(),
            ))
        })
        .collect::<BTreeMap<_, _>>();
    for item in work.iter().filter(|item| item["status"] == "review") {
        inbox.push(json!({
            "id":item["id"],"kind":"work","agent":item["rootId"],"title":item["title"],
            "text":"Result awaits acceptance","work":work_policy::work_item_view(item, &statuses),
        }));
    }
    for agent in agents
        .iter()
        .filter(|agent| matches!(agent["status"].as_str(), Some("failed" | "interrupted")))
    {
        inbox.push(json!({
            "id":agent["id"],"kind":"agent","agent":agent["id"],"title":agent["name"],
            "text":agent.get("error").and_then(Value::as_str).filter(|text| !text.is_empty()).unwrap_or(agent["status"].as_str().unwrap_or("")),
        }));
    }
    for monitor in monitors.iter().filter(|monitor| {
        matches!(monitor["status"].as_str(), Some("failed" | "lost"))
            && monitor.get("ruleId").is_none()
    }) {
        inbox.push(json!({
            "id":monitor["id"],"kind":"monitor","agent":monitor["agent"],"title":monitor["command"],
            "text":monitor.get("error").and_then(Value::as_str).filter(|text| !text.is_empty())
                .map(str::to_owned).unwrap_or_else(|| format!("Exit {}", monitor.get("exitCode").and_then(Value::as_i64).map_or_else(|| "None".into(), |code| code.to_string()))),
            "monitor":monitor,
        }));
    }
    for rule in rules.iter().filter(|rule| truthy(rule.get("error"))) {
        inbox.push(json!({
            "id":rule["id"],"kind":"rule","agent":rule["agent"],"title":rule["name"],"text":rule["error"],
        }));
    }
    Ok(inbox)
}

fn complaint_needs_response(complaint: &Value) -> bool {
    if matches!(complaint["status"].as_str(), Some("resolved" | "declined")) {
        return false;
    }
    if complaint["sourceType"] == "user_task" && complaint["status"] == "open" {
        return true;
    }
    let responsible = if complaint["recipient"] == "user" {
        "user"
    } else {
        complaint["leadId"].as_str().unwrap_or_default()
    };
    !complaint["responses"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|response| response["author"] == responsible)
}

async fn task_detail(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let id = query
        .typed_value("id")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let (store, validators) = (
        state.store.clone().ok_or_else(ApiError::internal)?,
        state.validators.clone(),
    );
    let task = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        let task = WorkspaceViewsRepository::task(&connection, &id)
            .map_err(|_| invalid_stored("runtime_tasks", &id))?
            .ok_or_else(invalid_request)?;
        let task = checked_stored(
            &validators,
            "TaskDetailResponse",
            "runtime_tasks",
            &id,
            task,
        )?;
        let agent_id = task["agent"]
            .as_str()
            .ok_or_else(|| invalid_stored("runtime_tasks", &id))?;
        let agent = WorkspaceViewsRepository::agent(&connection, agent_id)
            .map_err(|_| invalid_stored("runtime_agents", agent_id))?
            .ok_or_else(|| invalid_stored("runtime_agents", agent_id))?;
        let agent = checked_stored(
            &validators,
            "AgentRecordResponse",
            "runtime_agents",
            agent_id,
            agent,
        )?;
        if deleted(&agent) {
            return Err(invalid_request());
        }
        Ok(task)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(task))
}

async fn workspace_tasks(
    query: QueryParams,
    State(state): State<HttpState>,
) -> Result<Json<Value>, ApiError> {
    let agent = query.nonempty_str("agent").map(str::to_owned);
    let limit = query
        .typed_value("limit")
        .and_then(Value::as_u64)
        .unwrap_or(100)
        .clamp(1, 100) as usize;
    let cursor = parse_task_cursor(query.nonempty_str("cursor"), CursorKind::Current)
        .map_err(|_| invalid_cursor("cursor"))?;
    let before = parse_task_cursor(query.nonempty_str("before"), CursorKind::Before)
        .map_err(|_| invalid_cursor("before"))?;
    let (store, validators) = (
        state.store.clone().ok_or_else(ApiError::internal)?,
        state.validators.clone(),
    );
    let result = tokio::task::spawn_blocking(move || {
        let connection = store.read().map_err(|_| ApiError::internal())?;
        if let Some(id) = WorkspaceViewsRepository::first_invalid_task_identity(&connection)
            .map_err(|_| invalid_stored("runtime_tasks", "unknown"))?
        {
            return Err(invalid_stored("runtime_tasks", &id));
        }

        let agents = WorkspaceViewsRepository::agent_ids(&connection)
            .map_err(|_| invalid_stored("runtime_agents", "unknown"))?
            .into_iter()
            .map(|id| {
                let record = WorkspaceViewsRepository::agent(&connection, &id)
                    .map_err(|_| invalid_stored("runtime_agents", &id))?
                    .ok_or_else(|| invalid_stored("runtime_agents", &id))?;
                let record = checked_stored(
                    &validators,
                    "AgentRecordResponse",
                    "runtime_agents",
                    &id,
                    record,
                )?;
                Ok((id, record))
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        let root = agent
            .as_deref()
            .map(|agent_id| {
                let record = agents
                    .iter()
                    .find(|(id, _)| id == agent_id)
                    .map(|(_, record)| record)
                    .ok_or_else(invalid_request)?;
                if deleted(record) {
                    return Err(invalid_request());
                }
                Ok(record["rootId"]
                    .as_str()
                    .ok_or_else(|| invalid_stored("runtime_agents", agent_id))?
                    .to_owned())
            })
            .transpose()?;
        let ids = agents
            .into_iter()
            .filter(|(_, record)| {
                !deleted(record) && root.as_ref().is_none_or(|root| record["rootId"] == *root)
            })
            .map(|(id, _)| id)
            .collect::<Vec<_>>();
        let page = WorkspaceViewsRepository::task_page(
            &connection,
            &ids,
            cursor
                .as_ref()
                .filter(|_| before.is_none())
                .map(|value| (value.time, value.id.as_str())),
            before.as_ref().map(|value| (value.time, value.id.as_str())),
            limit,
        )
        .map_err(|_| invalid_stored("runtime_tasks", "unknown"))?;
        let has_more = page.len() > limit;
        let tasks = page
            .into_iter()
            .take(limit)
            .map(|row| {
                let record = WorkspaceViewsRepository::task(&connection, &row.id)
                    .map_err(|_| invalid_stored("runtime_tasks", &row.id))?
                    .ok_or_else(|| invalid_stored("runtime_tasks", &row.id))?;
                let record = checked_stored(
                    &validators,
                    "TaskDetailResponse",
                    "runtime_tasks",
                    &row.id,
                    record,
                )?;
                let mut summary = record;
                if let Some(record) = summary.as_object_mut() {
                    record.remove("tail");
                    record.remove("arguments");
                    record.remove("error");
                }
                Ok(summary)
            })
            .collect::<Result<Vec<_>, ApiError>>()?;
        Ok(task_feed_response(
            tasks,
            has_more,
            cursor.as_ref(),
            before.as_ref(),
        ))
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(Json(result))
}

fn checked_stored(
    validators: &Registry,
    schema: &str,
    table: &str,
    id: &str,
    value: Value,
) -> Result<Value, ApiError> {
    if !validators.validate_stored(schema, &value)
        || value.get("id").and_then(Value::as_str) != Some(id)
    {
        Err(invalid_stored(table, id))
    } else {
        Ok(value)
    }
}

fn deleted(agent: &Value) -> bool {
    agent
        .get("deletedAt")
        .and_then(Value::as_f64)
        .is_some_and(|value| value != 0.0)
}

fn invalid_request() -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, "Invalid request")
}

fn invalid_stored(object: &str, id: &str) -> ApiError {
    ApiError::new(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!("Invalid stored object {object} row {id}"),
    )
}

fn invalid_cursor(parameter: &str) -> ApiError {
    ApiError::validation(vec![json!("query"), json!(parameter)])
}

fn dependency_error(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}
