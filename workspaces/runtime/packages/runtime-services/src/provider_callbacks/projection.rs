//! Bounded provider view; the complete result remains in runtime_tool_results.
use super::{CallbackError, tool_result};
use serde_json::{Value, json};

fn clip(text: &str, size: usize) -> &str {
    &text[..text.floor_char_boundary(size.min(text.len()))]
}

pub fn project_tool_result(
    result: &Value,
    key: &str,
    outcome: &Value,
) -> Result<Value, CallbackError> {
    let content = result["contentItems"]
        .as_array()
        .ok_or(CallbackError::Stored("tool result/contentItems".into()))?;
    let is_text = |item: &&Value| {
        item["type"] == "inputText"
            && item["text"].as_str().is_some_and(|text| {
                !text.starts_with("[Time awareness]") && !text.starts_with("[Studio agent mode,")
            })
    };
    let texts = content.iter().filter(is_text).collect::<Vec<_>>();
    for item in &texts {
        if let Some(page) = item["text"]
            .as_str()
            .and_then(|s| serde_json::from_str::<Value>(s).ok())
            && page.is_object()
            && page["outputRef"].is_string()
            && page["offset"].is_i64()
            && page["totalChars"].is_i64()
            && page["text"].is_string()
        {
            return bounded(result.clone(), key, outcome);
        }
    }
    if serde_json::to_vec(&texts)?.len() <= 16000 {
        return bounded(result.clone(), key, outcome);
    }
    let raw = texts
        .iter()
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    let mut preview = json!({"requestId":key,"outputRef":key,"success":result["success"],"truncated":true,"textBytes":raw.len(),"excerpt":clip(&raw,4000),"read":{"tool":"orchestration_read","output_ref":key},"outcome":outcome});
    if let Ok(value) = serde_json::from_str::<Value>(&raw)
        && value.is_object()
    {
        let mut summary = serde_json::Map::new();
        for field in [
            "id",
            "requestId",
            "title",
            "owner",
            "stage",
            "outcome",
            "status",
            "exitCode",
            "version",
            "blockedBy",
            "nextCursor",
            "nextBefore",
            "revision",
            "total",
            "nextOffset",
            "agentIds",
            "agent_ids",
        ] {
            if let Some(v) = value.get(field) {
                summary.insert(field.into(), v.clone());
            }
        }
        if let Some(agents) = value["agents"].as_array() {
            summary.insert(
                "agents".into(),
                json!(
                    agents
                        .iter()
                        .filter(|a| a.is_object())
                        .map(|a| Value::Object(
                            ["id", "status"]
                                .into_iter()
                                .filter_map(|k| a.get(k).map(|v| (k.into(), v.clone())))
                                .collect()
                        ))
                        .collect::<Vec<_>>()
                ),
            );
        }
        if let Some(error) = value["error"].as_str().filter(|s| !s.is_empty()) {
            summary.insert("error".into(), json!(clip(error, 600)));
        }
        preview["summary"] = Value::Object(summary);
        if value["status"] == "failed" || (!value["exitCode"].is_null() && value["exitCode"] != 0) {
            preview["failureTail"] =
                json!(&raw[raw.ceil_char_boundary(raw.len().saturating_sub(1200))..]);
        }
    }
    if result["success"] == false {
        preview["failureTail"] =
            json!(&raw[raw.ceil_char_boundary(raw.len().saturating_sub(1200))..]);
    }
    let mut projected = result.clone();
    let mut items = vec![json!({"type":"inputText","text":preview.to_string()})];
    items.extend(content.iter().filter(|item| !is_text(item)).cloned());
    projected["contentItems"] = json!(items);
    bounded(projected, key, outcome)
}

fn bounded(result: Value, key: &str, outcome: &Value) -> Result<Value, CallbackError> {
    if result.to_string().len() <= 128000 {
        return Ok(result);
    }
    // Large non-text content or summaries still receive a durable recovery page.
    Ok(tool_result(
        json!({"requestId":key,"outputRef":key,"success":result["success"],"outcome":outcome,"truncated":true,"read":{"tool":"orchestration_read","output_ref":key}}),
        result["success"] != false,
    ))
}
