//! Pure task and non-text item transitions; the transaction adapter applies the intents.
use super::notices::{json_text, remove};
use super::{NotificationDispatchError, suffix};
use serde_json::{Value, json};

pub(super) const TOOL_OUTPUT_CHARS: usize = 12_000;
const TASK_TYPES: &[&str] = &[
    "commandExecution",
    "dynamicToolCall",
    "mcpToolCall",
    "webSearch",
    "fileChange",
    "contextCompaction",
];

#[derive(Default)]
pub(super) struct ItemDecision {
    pub task: Option<Value>,
    pub item: Option<Value>,
    pub compacted: bool,
}

pub(super) fn task_transition(
    agent: &Value,
    method: &str,
    params: &Value,
    previous: Option<&Value>,
    stale: bool,
    at: f64,
) -> Result<Option<Value>, NotificationDispatchError> {
    let item = &params["item"];
    let kind = item["type"].as_str().unwrap_or_default();
    let item_id = item["id"]
        .as_str()
        .or_else(|| params["itemId"].as_str())
        .filter(|s| !s.is_empty());
    if item_id.is_none()
        || (method != "item/commandExecution/outputDelta" && !TASK_TYPES.contains(&kind))
    {
        return Ok(None);
    }
    let item_id = item_id.unwrap_or_default();
    let historical_tool = stale
        && method == "item/completed"
        && previous.is_some_and(|t| {
            t["kind"] == "tool"
                && t["agent"] == agent["id"]
                && t["itemId"] == item_id
                && t["turnId"] == params["turnId"]
                && t["type"] == kind
        });
    if stale
        && !historical_tool
        && !previous.is_some_and(|t| t["kind"] == "command" && t["turnId"] == params["turnId"])
    {
        return Ok(None);
    }
    if method == "item/commandExecution/outputDelta" {
        let Some(mut task) = previous.cloned() else {
            return Ok(None);
        };
        let output = format!(
            "{}{}",
            task["tail"].as_str().unwrap_or_default(),
            params["delta"].as_str().unwrap_or_default()
        );
        task["tail"] = json!(suffix(&output, TOOL_OUTPUT_CHARS));
        task["outputTruncated"] =
            json!(task["outputTruncated"] == true || output.chars().count() > TOOL_OUTPUT_CHARS);
        return Ok(Some(task));
    }
    if let Some(task) = previous
        && task["status"] != "running"
    {
        return Ok(Some(task.clone()));
    }
    let mut task = previous.cloned().unwrap_or_else(||json!({"id":format!("{}:{item_id}",agent["id"].as_str().unwrap_or_default()),"agent":agent["id"],"itemId":item_id,"turnId":params.get("turnId").filter(|t|!t.is_null() && **t != "").unwrap_or(&agent["turnId"]),"created":at,"kind":if kind=="commandExecution"{"command"}else{"tool"}}));
    task["type"] = json!(kind);
    task["name"] = item
        .get("tool")
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        .cloned()
        .unwrap_or(json!(kind));
    task["status"] = json!("running");
    for key in [
        "command",
        "cwd",
        "processId",
        "durationMs",
        "exitCode",
        "query",
        "server",
    ] {
        if let Some(value) = item.get(key).filter(|v| !v.is_null()) {
            task[key] = value.clone();
        }
    }
    for key in ["startedAtMs", "completedAtMs"] {
        if let Some(value) = params.get(key).filter(|v| v.is_number()) {
            task[key] = value.clone();
        }
    }
    if let (Some(start), Some(end)) = (task["startedAtMs"].as_f64(), task["completedAtMs"].as_f64())
    {
        task["durationMs"] = duration(&task["startedAtMs"], &task["completedAtMs"], start, end);
    }
    for key in ["arguments", "error"] {
        if let Some(value) = item.get(key).filter(|v| !v.is_null()) {
            task[key] = json!(
                value
                    .as_str()
                    .map(str::to_owned)
                    .unwrap_or_else(|| json_text(value))
                    .chars()
                    .take(TOOL_OUTPUT_CHARS)
                    .collect::<String>()
            );
        }
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
        task["tail"] = json!(suffix(&output, TOOL_OUTPUT_CHARS));
        task["outputTruncated"] = json!(output.chars().count() > TOOL_OUTPUT_CHARS);
    }
    if method == "item/completed" {
        if !task["durationMs"].is_number() {
            let elapsed = (at - task["created"].as_f64().unwrap_or(at)) * 1000.0;
            task["durationMs"] = if elapsed <= 0.0 {
                json!(0)
            } else {
                json!(elapsed)
            };
        }
        task["status"] = json!(tool_status(item, false));
        task["finished"] = json!(at);
    }
    Ok(Some(task))
}

pub(super) fn non_text(
    agent: &mut Value,
    method: &str,
    params: &Value,
    previous_task: Option<&Value>,
    already_compacted: bool,
    at: f64,
) -> Result<ItemDecision, NotificationDispatchError> {
    let item = &params["item"];
    let kind = item["type"]
        .as_str()
        .ok_or(NotificationDispatchError::InvalidNotification)?;
    let started = method == "item/started";
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
        displayed["aggregatedOutput"] = json!(suffix(
            task["tail"].as_str().unwrap_or_default(),
            TOOL_OUTPUT_CHARS
        ));
        displayed["outputTruncated"] = json!(task["outputTruncated"] == true);
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
                    decision
                        .task
                        .as_ref()
                        .or(previous_task)
                        .and_then(|t| t.get("durationMs"))
                        .cloned()
                }
            }),
    };
    if let Some(value) = duration {
        displayed["durationMs"] = value;
    }
    decision.item = Some(displayed);
    Ok(decision)
}

pub(super) fn tool_status(item: &Value, started: bool) -> &'static str {
    if started {
        "running"
    } else if matches!(item["status"].as_str(), Some("failed" | "declined"))
        || item["success"] == false
        || (!item["exitCode"].is_null() && item["exitCode"] != 0)
        || crate::event_write::truthy(item.get("error"))
    {
        "failed"
    } else {
        "completed"
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
