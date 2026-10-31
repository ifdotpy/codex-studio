//! Pure task and non-text item transitions; the transaction adapter applies the intents.
use super::notices::{json_text, remove};
use super::{NotificationDispatchError, suffix};
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_schema::task_record::{NonnegativeNumber, ProcessId, TaskStatus, TaskType};
use studio_runtime_store::TaskRecord;

pub(super) const TOOL_OUTPUT_CHARS: usize = 12_000;

#[derive(Default)]
pub(super) struct ItemDecision {
    pub task: Option<TaskRecord>,
    pub item: Option<Value>,
    pub compacted: bool,
}

pub(super) fn task_transition(
    agent: &Value,
    method: &str,
    params: &Value,
    previous: Option<&TaskRecord>,
    stale: bool,
    at: f64,
) -> Result<Option<TaskRecord>, NotificationDispatchError> {
    let item = &params["item"];
    let kind = item["type"].as_str().unwrap_or_default();
    let task_type = TaskType::deserialize(serde::de::value::StrDeserializer::<
        serde::de::value::Error,
    >::new(kind))
    .ok();
    let item_id = item["id"]
        .as_str()
        .or_else(|| params["itemId"].as_str())
        .filter(|s| !s.is_empty());
    if item_id.is_none() || (method != "item/commandExecution/outputDelta" && task_type.is_none()) {
        return Ok(None);
    }
    let item_id = item_id.unwrap_or_default();
    let historical_tool = stale
        && matches!(method, "item/started" | "item/completed")
        && previous.is_some_and(|t| {
            t.kind() == studio_runtime_schema::task_record::TaskKind::Tool
                && Some(t.agent()) == agent["id"].as_str()
                && t.item_id() == item_id
                && Some(t.turn_id()) == params["turnId"].as_str()
                && t.task_type().as_str() == kind
        });
    if stale
        && !historical_tool
        && !previous.is_some_and(|t| {
            t.kind() == studio_runtime_schema::task_record::TaskKind::Command
                && Some(t.turn_id()) == params["turnId"].as_str()
        })
    {
        return Ok(None);
    }
    if method == "item/commandExecution/outputDelta" {
        let Some(task) = previous else {
            return Ok(None);
        };
        let output = format!(
            "{}{}",
            task.tail().unwrap_or_default(),
            params["delta"].as_str().unwrap_or_default()
        );
        let mut task = task.clone();
        task.set_tail(suffix(&output, TOOL_OUTPUT_CHARS));
        task.set_output_truncated(
            task.output_truncated() == Some(true) || output.chars().count() > TOOL_OUTPUT_CHARS,
        );
        return Ok(Some(task));
    }
    if let Some(task) = previous
        && task.status() != studio_runtime_schema::task_record::TaskStatus::Running
    {
        return Ok(Some(task.clone()));
    }
    let task_type = task_type.ok_or_else(NotificationDispatchError::invalid)?;
    let name = item
        .get("tool")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(kind)
        .to_owned();
    let mut task = if let Some(previous) = previous {
        previous.clone()
    } else {
        let agent_id = agent["id"]
            .as_str()
            .filter(|s| !s.is_empty())
            .ok_or_else(NotificationDispatchError::invalid)?;
        let turn = params["turnId"]
            .as_str()
            .filter(|s| !s.is_empty())
            .or_else(|| agent["turnId"].as_str().filter(|s| !s.is_empty()))
            .ok_or_else(NotificationDispatchError::invalid)?;
        let created = NonnegativeNumber::deserialize(serde::de::value::F64Deserializer::<
            serde::de::value::Error,
        >::new(at))
        .map_err(|_| NotificationDispatchError::invalid())?;
        TaskRecord::new(
            agent_id.to_owned(),
            item_id.to_owned(),
            turn.to_owned(),
            created,
            task_type,
            name.clone(),
        )
    };
    if task.task_type() != task_type {
        return Err(NotificationDispatchError::invalid());
    }
    task.name = name;
    task.set_status(TaskStatus::Running);
    let string = |key: &str| {
        item.get(key)
            .filter(|v| !v.is_null())
            .map(|v| {
                v.as_str()
                    .map(str::to_owned)
                    .ok_or_else(NotificationDispatchError::invalid)
            })
            .transpose()
    };
    if let Some(value) = string("command")? {
        task.command = Some(value);
    }
    if let Some(value) = string("cwd")? {
        task.cwd = Some(value);
    }
    if let Some(value) = item.get("processId").filter(|v| !v.is_null()) {
        task.process_id =
            Some(ProcessId::deserialize(value).map_err(|_| NotificationDispatchError::invalid())?);
    }
    if let Some(value) = string("query")? {
        task.query = Some(value);
    }
    if let Some(value) = string("server")? {
        task.server = Some(value);
    }
    if let Some(value) = string("outputFile")? {
        task.output_file = Some(value);
    }
    if let Some(value) = item.get("exitCode").filter(|v| !v.is_null()) {
        task.exit_code = Some(
            value
                .as_i64()
                .ok_or_else(NotificationDispatchError::invalid)?,
        );
    }
    if let Some(value) = item.get("durationMs").filter(|v| !v.is_null()) {
        task.duration_ms = Some(
            NonnegativeNumber::deserialize(value)
                .map_err(|_| NotificationDispatchError::invalid())?,
        );
    }
    if let Some(value) = params.get("startedAtMs").filter(|v| v.is_number()) {
        task.started_at_ms = Some(
            NonnegativeNumber::deserialize(value)
                .map_err(|_| NotificationDispatchError::invalid())?,
        );
    }
    if let Some(value) = params.get("completedAtMs").filter(|v| v.is_number()) {
        task.completed_at_ms = Some(
            NonnegativeNumber::deserialize(value)
                .map_err(|_| NotificationDispatchError::invalid())?,
        );
    }
    if let (Some(start), Some(end)) = (task.started_at_ms(), task.completed_at_ms()) {
        let integer = task
            .started_at_ms
            .as_ref()
            .and_then(|v| v.number().as_i64())
            .zip(
                task.completed_at_ms
                    .as_ref()
                    .and_then(|v| v.number().as_i64()),
            );
        task.duration_ms = Some(
            if let Some((start, end)) = integer {
                NonnegativeNumber::deserialize(serde::de::value::I64Deserializer::<
                    serde::de::value::Error,
                >::new(
                    end.saturating_sub(start).max(0)
                ))
            } else {
                NonnegativeNumber::deserialize(serde::de::value::F64Deserializer::<
                    serde::de::value::Error,
                >::new((end - start).max(0.0)))
            }
            .map_err(|_| NotificationDispatchError::invalid())?,
        );
    }
    let text = |key: &str| {
        item.get(key).filter(|v| !v.is_null()).map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .unwrap_or_else(|| json_text(value))
                .chars()
                .take(TOOL_OUTPUT_CHARS)
                .collect()
        })
    };
    if let Some(value) = text("arguments") {
        task.arguments = Some(value);
    }
    if let Some(value) = text("error") {
        task.error = Some(value);
    }
    let output = item
        .get("aggregatedOutput")
        .filter(|v| !v.is_null())
        .and_then(Value::as_str)
        .map(str::to_owned)
        .or_else(|| {
            item.get("contentItems")
                .or_else(|| item.get("result"))
                .filter(|v| !v.is_null())
                .map(json_text)
        });
    if let Some(output) = output {
        task.tail = Some(suffix(&output, TOOL_OUTPUT_CHARS));
        task.output_truncated = Some(output.chars().count() > TOOL_OUTPUT_CHARS);
    }
    if method == "item/completed" && item["status"] != "inProgress" {
        if task.duration_ms.is_none() {
            let elapsed = (at - task.created()) * 1000.0;
            task.duration_ms = Some(
                if elapsed <= 0.0 {
                    NonnegativeNumber::deserialize(serde::de::value::I64Deserializer::<
                        serde::de::value::Error,
                    >::new(0))
                } else {
                    NonnegativeNumber::deserialize(serde::de::value::F64Deserializer::<
                        serde::de::value::Error,
                    >::new(elapsed))
                }
                .map_err(|_| NotificationDispatchError::invalid())?,
            );
        }
        task.set_status(tool_status(item, false));
        task.finished = Some(at);
    }
    Ok(Some(task))
}

pub(super) fn non_text(
    agent: &mut Value,
    method: &str,
    params: &Value,
    previous_task: Option<&TaskRecord>,
    already_compacted: bool,
    at: f64,
) -> Result<ItemDecision, NotificationDispatchError> {
    let item = &params["item"];
    let kind = item["type"]
        .as_str()
        .ok_or(NotificationDispatchError::invalid())?;
    let started = method == "item/started" || item["status"] == "inProgress";
    let task = task_transition(agent, method, params, previous_task, false, at)?;
    let mut decision = ItemDecision {
        task,
        ..ItemDecision::default()
    };
    if kind == "reasoning" {
        agent["activity"] = json!({"phase":"thinking","at":at});
        return Ok(decision);
    }
    if kind == "userMessage" {
        return Ok(decision);
    }
    let mut active: Vec<Value> = agent["activeTools"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|t| t["id"] != item["id"])
        .cloned()
        .collect();
    if started {
        active.push(json!({"id":item["id"],"type":kind,"name":item.get("tool").filter(|v|v.as_str().is_some_and(|s|!s.is_empty())).cloned().unwrap_or(json!(kind))}));
    }
    agent["activity"] =
        json!({"phase":if active.is_empty(){"thinking"}else{"tool"},"tools":active,"at":at});
    agent["activeTools"] = json!(active);
    if kind == "contextCompaction" && !started && !already_compacted {
        decision.compacted = true;
        agent["compactions"] = json!(
            agent["compactions"]
                .as_u64()
                .unwrap_or_default()
                .saturating_add(1)
        );
        agent["contextUsage"] = Value::Null;
    }
    if kind == "exitedReviewMode"
        && !started
        && let Some(review) = item["review"].as_str()
    {
        agent["lastAnswer"] = json!(suffix(review, 16_000));
        agent["tail"] = json!(suffix(review, 300));
    }
    let mut displayed = item.clone();
    if kind == "commandExecution"
        && !started
        && item["aggregatedOutput"].is_null()
        && let Some(task) = &decision.task
    {
        displayed["aggregatedOutput"] =
            json!(suffix(task.tail().unwrap_or_default(), TOOL_OUTPUT_CHARS));
        displayed["outputTruncated"] = json!(task.output_truncated() == Some(true));
    }
    for key in ["startedAtMs", "completedAtMs"] {
        if let Some(value) = params.get(key).filter(|v| !v.is_null()) {
            displayed[key] = value.clone();
        }
    }
    let duration = match (
        params["startedAtMs"].as_f64(),
        params["completedAtMs"].as_f64(),
    ) {
        (Some(start), Some(end)) => Some(duration(
            &params["startedAtMs"],
            &params["completedAtMs"],
            start,
            end,
        )),
        _ => item
            .get("durationMs")
            .filter(|v| !v.is_null())
            .cloned()
            .or_else(|| {
                if started {
                    None
                } else {
                    match &decision.task {
                        Some(task) => task.duration_json(),
                        None => previous_task.and_then(TaskRecord::duration_json),
                    }
                }
            }),
    };
    if let Some(value) = duration {
        displayed["durationMs"] = value;
    }
    decision.item = Some(displayed);
    Ok(decision)
}

pub(super) fn tool_status(item: &Value, started: bool) -> TaskStatus {
    if started || item["status"] == "inProgress" {
        TaskStatus::Running
    } else if matches!(item["status"].as_str(), Some("failed" | "declined"))
        || item["success"] == false
        || (!item["exitCode"].is_null() && item["exitCode"] != 0)
        || crate::event_write::truthy(item.get("error"))
    {
        TaskStatus::Failed
    } else {
        TaskStatus::Completed
    }
}

pub(super) fn advance_reasoning(agent: &mut Value, params: &Value, at: f64) {
    if agent["nativeStatus"]["phase"] == "retrying"
        && agent["nativeStatus"]["turnId"] == params["turnId"]
    {
        remove(agent, "nativeStatus");
        agent["activity"] = json!({"phase":"thinking","at":at});
    }
}

fn duration(start_value: &Value, end_value: &Value, start: f64, end: f64) -> Value {
    if let (Some(start), Some(end)) = (start_value.as_i64(), end_value.as_i64()) {
        json!(end.saturating_sub(start).max(0))
    } else {
        json!((end - start).max(0.0))
    }
}
