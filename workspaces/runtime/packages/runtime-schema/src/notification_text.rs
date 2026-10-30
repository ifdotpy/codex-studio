//! Projections of structured notification text used for scheduler decisions.
//!
//! Parse a full JSON value first: ignored evidence must still obey JSON depth
//! and number limits, and duplicate members retain their last value.
use serde::{Deserialize, Deserializer, de::DeserializeOwned};
use serde_json::Value;

struct Object<T>(T);

impl<'de, T: DeserializeOwned> Deserialize<'de> for Object<T> {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        if !value.is_object() {
            return Err(serde::de::Error::custom("expected object"));
        }
        T::deserialize(value)
            .map(Self)
            .map_err(serde::de::Error::custom)
    }
}

fn parse<T: DeserializeOwned>(text: &str) -> Option<T> {
    let value: Value = serde_json::from_str(text).ok()?;
    if !value.is_object() {
        return None;
    }
    T::deserialize(value).ok()
}

#[derive(Deserialize)]
struct ComplaintBatch {
    complaints: Vec<Value>,
}
#[derive(Deserialize)]
struct ComplaintEntry {
    complaint_id: String,
}

/// Parse the complete text before exposing entries, but decode each entry only
/// when requested so consumers can preserve intervening database error order.
pub fn complaint_entries(text: &str) -> Option<impl Iterator<Item = Option<String>>> {
    let batch: ComplaintBatch = parse(text)?;
    if batch.complaints.is_empty() {
        return None;
    }
    Some(batch.complaints.into_iter().map(|entry| {
        let entry = Object::<ComplaintEntry>::deserialize(entry).ok()?;
        let id = entry.0.complaint_id;
        (!id.is_empty()).then_some(id)
    }))
}

/// Scheduler complaints require at least one nonempty identifier.
pub fn complaint_ids(text: &str) -> Option<Vec<String>> {
    complaint_entries(text)?.collect()
}

/// References used only for continuation prefetch; invalid siblings are ignored.
#[derive(Default, Deserialize)]
pub struct ContinuationReferences {
    #[serde(default, deserialize_with = "continuation_task")]
    pub task: Option<String>,
    #[serde(default, deserialize_with = "continuation_complaints")]
    pub complaints: Vec<String>,
}

fn continuation_task<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<String>, D::Error> {
    Ok(Value::deserialize(deserializer)?
        .as_str()
        .map(str::to_owned))
}

fn continuation_complaints<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Vec<String>, D::Error> {
    let value = Value::deserialize(deserializer)?;
    Ok(match value {
        Value::Array(entries) => entries
            .into_iter()
            .filter_map(|entry| {
                Object::<ComplaintEntry>::deserialize(entry)
                    .ok()
                    .map(|entry| entry.0.complaint_id)
            })
            .collect(),
        Value::Null | Value::Bool(_) | Value::Number(_) | Value::String(_) | Value::Object(_) => {
            Vec::new()
        }
    })
}

pub fn continuation_references(text: &str) -> Option<ContinuationReferences> {
    parse(text)
}

#[derive(Deserialize)]
struct Progress {
    importance: String,
    sender: String,
    room: String,
    progress_key: String,
    progress_version: u64,
}

pub fn progress_update(text: &str) -> Option<((String, String, String), u64)> {
    let Progress {
        importance,
        sender,
        room,
        progress_key,
        progress_version,
    } = parse(text)?;
    if importance != "progress" || sender.is_empty() || room.is_empty() || progress_key.is_empty() {
        return None;
    }
    Some(((sender, room, progress_key), progress_version))
}

#[derive(Deserialize)]
struct WorkReview {
    task: String,
    result: Object<WorkResult>,
}
#[derive(Deserialize)]
struct WorkResult {
    id: String,
}

pub fn work_review_ids(text: &str) -> Option<(String, String)> {
    let review: WorkReview = parse(text)?;
    Some((review.task, review.result.0.id))
}

#[derive(Deserialize)]
pub struct ChatReceipt {
    pub message_id: String,
    pub room: String,
    pub sender: String,
    pub text: String,
}

/// The four fields used to identify a delivered chat notification.
pub fn chat_receipt(text: &str) -> Option<ChatReceipt> {
    let receipt: ChatReceipt = parse(text)?;
    (!receipt.message_id.is_empty() && !receipt.room.is_empty() && !receipt.sender.is_empty())
        .then_some(receipt)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chat_receipt_matches_value_admission() {
        for text in [
            r#"{"message_id":"old","message_id":"m","room":"r","sender":"s","text":""}"#,
            r#"{"message_id":"","room":"r","sender":"s","text":"x"}"#,
            r#"{"message_id":"m","room":null,"sender":"s","text":"x"}"#,
            r#"{"message_id":"m","room":"r","sender":"s"}"#,
            r#"{"message_id":"m","room":"r","sender":"s","text":"x","extra":1e999}"#,
            "[]",
            "{",
        ] {
            let original = (|| {
                let value: Value = serde_json::from_str(text).ok()?;
                let field = |key| value.get(key)?.as_str();
                let (id, room, sender, body) = (
                    field("message_id")?,
                    field("room")?,
                    field("sender")?,
                    field("text")?,
                );
                if id.is_empty() || room.is_empty() || sender.is_empty() {
                    return None;
                }
                Some((
                    id.to_owned(),
                    room.to_owned(),
                    sender.to_owned(),
                    body.to_owned(),
                ))
            })();
            let projected = chat_receipt(text).map(|r| (r.message_id, r.room, r.sender, r.text));
            assert_eq!(projected, original, "{text}");
        }
    }

    #[test]
    fn complaint_entries_validate_each_item_lazily_after_full_json_parse() {
        let mut entries = complaint_entries(
            r#"{"complaints":[{"complaint_id":"old","complaint_id":"first"},{}]}"#,
        )
        .unwrap();
        assert_eq!(entries.next(), Some(Some("first".into())));
        assert_eq!(entries.next(), Some(None));
        assert!(
            complaint_entries(r#"{"complaints":[{"complaint_id":"first"},{}],"extra":1e999}"#)
                .is_none()
        );
        assert!(complaint_entries(r#"{"complaints":[]}"#).is_none());
    }

    #[test]
    fn progress_fields_match_message_tool_contract() {
        let resources: Value = serde_json::from_str(include_str!(
            "../../runtime-services/src/account_worker/preparation-resources.json"
        ))
        .unwrap();
        let tool = resources["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|tool| tool["name"] == "orchestration_message")
            .unwrap();
        let fields = &tool["inputSchema"]["properties"];
        assert_eq!(fields["progress_key"]["type"], "string");
        assert_eq!(fields["progress_version"]["type"], "integer");
        assert_eq!(fields["progress_version"]["minimum"], 0);
        assert!(
            fields["importance"]["enum"]
                .as_array()
                .unwrap()
                .contains(&Value::String("progress".into()))
        );
    }

    #[test]
    fn projections_match_original_value_admission() {
        type Results = (
            Option<Vec<String>>,
            Option<((String, String, String), u64)>,
            Option<(String, String)>,
        );
        fn original(text: &str) -> Results {
            let Ok(v) = serde_json::from_str::<Value>(text) else {
                return (None, None, None);
            };
            let complaints = (|| {
                let entries = v.get("complaints")?.as_array()?;
                if entries.is_empty() {
                    return None;
                }
                entries
                    .iter()
                    .map(|entry| {
                        let id = entry.get("complaint_id")?.as_str()?;
                        (!id.is_empty()).then(|| id.to_owned())
                    })
                    .collect()
            })();
            let progress = (|| {
                if v.get("importance")?.as_str()? != "progress" {
                    return None;
                }
                let field = |key| {
                    v.get(key)?
                        .as_str()
                        .filter(|v| !v.is_empty())
                        .map(str::to_owned)
                };
                Some((
                    (field("sender")?, field("room")?, field("progress_key")?),
                    v.get("progress_version")?.as_u64()?,
                ))
            })();
            let review = (|| {
                Some((
                    v.get("task")?.as_str()?.to_owned(),
                    v.get("result")?.get("id")?.as_str()?.to_owned(),
                ))
            })();
            (complaints, progress, review)
        }
        let mut cases = vec![
            r#"{"complaints":[{"complaint_id":"c"}]}"#.to_owned(),
            r#"{"complaints":[{"complaint_id":""}]}"#.to_owned(),
            r#"{"complaints":[["c"]]}"#.to_owned(),
            r#"{"complaints":[]}"#.to_owned(),
            r#"{"importance":"progress","sender":"s","room":"r","progress_key":"k","progress_version":0}"#.to_owned(),
            r#"{"importance":{"progress":null},"sender":"s","room":"r","progress_key":"k","progress_version":1}"#.to_owned(),
            r#"{"importance":"progress","sender":"s","room":"r","progress_key":"k","progress_version":1.0}"#.to_owned(),
            r#"{"importance":"progress","sender":"s","room":"r","progress_key":"k","progress_version":18446744073709551615}"#.to_owned(),
            r#"{"importance":"progress","sender":"s","room":"r","progress_key":"k","progress_version":-1}"#.to_owned(),
            r#"{"task":"","result":{"id":""}}"#.to_owned(),
            r#"{"task":"t","result":["id"]}"#.to_owned(),
            r#"{"task":"bad","task":"t","result":{"id":"old","id":"new"}}"#.to_owned(),
            r#"{"task":"t","result":{"id":"r"},"extra":1e999}"#.to_owned(),
            "[\"t\",{\"id\":\"r\"}]".to_owned(),
            "{".to_owned(),
        ];
        cases.push(format!(
            "{{\"task\":\"t\",\"result\":{{\"id\":\"r\"}},\"extra\":{}0{}}}",
            "[".repeat(140),
            "]".repeat(140)
        ));
        for text in cases {
            assert_eq!(
                (
                    complaint_ids(&text),
                    progress_update(&text),
                    work_review_ids(&text)
                ),
                original(&text),
                "{text}"
            );
        }
    }
}
