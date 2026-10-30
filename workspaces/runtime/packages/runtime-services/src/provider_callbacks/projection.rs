//! Bounded provider view; the complete result remains in request_results.
use super::{CallbackError, tool_result};
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_schema::json_object::JsonObject;

enum TextKind {
    InputText,
}
impl<'de> Deserialize<'de> for TextKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = <&str>::deserialize(deserializer)?;
        match value {
            "inputText" => Ok(Self::InputText),
            _ => Err(serde::de::Error::custom("not an input text item")),
        }
    }
}

#[derive(Deserialize)]
struct TextItem<'a> {
    #[serde(rename = "type")]
    _kind: TextKind,
    text: &'a str,
}

#[derive(Deserialize)]
struct OutputPage {
    #[serde(rename = "outputRef")]
    _output_ref: String,
    #[serde(rename = "offset")]
    _offset: i64,
    #[serde(rename = "totalChars")]
    _total_chars: i64,
    #[serde(rename = "text")]
    _text: String,
}

fn text_item(item: &Value) -> Option<&str> {
    item.as_object()?;
    let item = TextItem::deserialize(item).ok()?;
    Some(item.text)
}

fn is_output_page(text: &str) -> bool {
    serde_json::from_str::<JsonObject<OutputPage>>(text).is_ok()
}

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
    let classified = content
        .iter()
        .map(|item| {
            let text = text_item(item).filter(|text| {
                !text.starts_with("[Time awareness]") && !text.starts_with("[Studio agent mode,")
            });
            (item, text)
        })
        .collect::<Vec<_>>();
    let texts = classified
        .iter()
        .filter_map(|(item, text)| text.map(|text| (*item, text)))
        .collect::<Vec<_>>();
    for (_, text) in &texts {
        if is_output_page(text) {
            return bounded(result.clone(), key, outcome);
        }
    }
    if serde_json::to_vec(&texts.iter().map(|(item, _)| *item).collect::<Vec<_>>())?.len() <= 16000
    {
        return bounded(result.clone(), key, outcome);
    }
    let raw = texts
        .iter()
        .map(|(_, text)| *text)
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
    items.extend(
        classified
            .iter()
            .filter(|(_, text)| text.is_none())
            .map(|(item, _)| (*item).clone()),
    );
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn typed_text_and_page_checks_preserve_lenient_filtering() {
        for item in [
            json!(null),
            json!([]),
            json!({"type": "inputText", "text": 1}),
            json!({"type": ["inputText"], "text": "x"}),
            json!({"type": {"inputText": null}, "text": "x"}),
            json!({"type": "image", "text": "x"}),
        ] {
            assert!(text_item(&item).is_none());
        }
        assert_eq!(
            text_item(&json!({"type":"inputText","text":"x","vendor":42})),
            Some("x")
        );
        assert!(is_output_page(
            r#" {"outputRef":"r","offset":0,"totalChars":1,"text":"x"}"#
        ));
        for page in [
            r#"["r",0,1,"x"]"#,
            r#"{"outputRef":"r","offset":0.0,"totalChars":1,"text":"x"}"#,
            r#"{"outputRef":"r","offset":9223372036854775808,"totalChars":1,"text":"x"}"#,
        ] {
            assert!(!is_output_page(page));
        }
    }

    #[test]
    fn projection_keeps_malformed_and_nontext_items() {
        let result = json!({"success":true,"contentItems":[{"type":"inputText","text":"x".repeat(17000)}, null, {"type":"inputText","text":7}, {"type":"image","data":"opaque"}]});
        let projected = project_tool_result(&result, "id", &json!("written")).unwrap();
        assert_eq!(
            &projected["contentItems"].as_array().unwrap()[1..],
            &result["contentItems"].as_array().unwrap()[1..]
        );
        assert!(
            projected["contentItems"][0]["text"]
                .as_str()
                .unwrap()
                .contains("excerpt")
        );
    }
}
