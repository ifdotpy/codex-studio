//! Payload measurements retain sizes, never prompt, image, or reasoning contents.
use base64::Engine;
use serde_json::{Value, json};
use studio_runtime_store::python_json_compact;

pub fn payload_size(value: &Value) -> Value {
    if value.is_null() {
        return Value::Null;
    }
    let text = value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| python_json_compact(value));
    let mut images = Vec::new();
    image_sizes(value, &mut images);
    let bytes = images.iter().try_fold(0u64, |sum, v| {
        v["bytes"].as_u64().and_then(|n| sum.checked_add(n))
    });
    json!({"bytes":text.len(),"chars":text.chars().count(),"lines":text.bytes().filter(|c|*c==b'\n').count()+usize::from(!text.is_empty()),"imageCount":images.len(),"imageBytes":bytes,"images":images,"format":if value.is_string(){"text"}else{"json"}})
}
fn image_sizes(value: &Value, images: &mut Vec<Value>) {
    match value {
        Value::Array(items) => {
            for item in items {
                image_sizes(item, images)
            }
        }
        Value::Object(items) => {
            if matches!(
                value["type"].as_str(),
                Some("image" | "inputImage" | "input_image" | "image_url" | "localImage")
            ) {
                let url = ["imageUrl", "image_url", "url"]
                    .iter()
                    .find_map(|k| value.get(k).filter(|v| !v.is_null()))
                    .unwrap_or(&Value::Null);
                let url = url.as_str().or_else(|| url["url"].as_str()).unwrap_or("");
                let data = if url.starts_with("data:") {
                    url.split_once(";base64,").map(|(_, s)| s)
                } else {
                    None
                }
                .or_else(|| value["data"].as_str());
                let mut image = json!({"bytes":null,"width":null,"height":null});
                if let Some(data) = data {
                    let compact: String = data.chars().filter(|c| !c.is_whitespace()).collect();
                    image["bytes"] = json!(
                        (compact.len() * 3 / 4)
                            .saturating_sub(compact.len() - compact.trim_end_matches('=').len())
                    );
                    let prefix: String = compact.chars().take(32).collect();
                    if let Ok(header) = base64::engine::general_purpose::STANDARD.decode(prefix)
                        && header.starts_with(b"\x89PNG\r\n\x1a\n")
                        && header.len() >= 24
                    {
                        image["width"] = json!(u32::from_be_bytes([
                            header[16], header[17], header[18], header[19]
                        ]));
                        image["height"] = json!(u32::from_be_bytes([
                            header[20], header[21], header[22], header[23]
                        ]));
                    }
                }
                images.push(image);
            } else {
                for item in items.values() {
                    image_sizes(item, images)
                }
            }
        }
        _ => {}
    }
}
pub(crate) fn item_sizes(kind: &str, item: &Value) -> (Value, Value) {
    let mut input = item
        .get("arguments")
        .or_else(|| item.get("command"))
        .or_else(|| item.get("query"))
        .cloned()
        .unwrap_or_default();
    let mut output = item
        .get("aggregatedOutput")
        .or_else(|| item.get("contentItems"))
        .or_else(|| item.get("result"))
        .cloned()
        .unwrap_or_default();
    match kind {
        "fileChange" => output = item["changes"].clone(),
        "userMessage" => input = item["content"].clone(),
        "agentMessage" => output = item["text"].clone(),
        "reasoning" => {
            let fields: serde_json::Map<String, Value> = ["summary", "content", "text"]
                .into_iter()
                .filter_map(|k| item.get(k).map(|v| (k.to_owned(), v.clone())))
                .collect();
            output = if fields.is_empty() {
                Value::Null
            } else {
                Value::Object(fields)
            };
        }
        _ => {}
    }
    (payload_size(&input), payload_size(&output))
}
