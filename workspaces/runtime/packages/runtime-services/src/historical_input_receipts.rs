//! Reconstruct legacy Python text-only batches from saved transcript records.
//! A supervisor acceptance and one exact native user item are both required.
use crate::context_repair::{InputRow, MAX_CAPTURE_BYTES, RepairError, string_default, validate};
use chrono::{DateTime, SecondsFormat};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use studio_runtime_store::{python_json, sha256_hex, truthy};

const TEXT_LIMIT: usize = 64 * 1024;
const OPERATION_LIMIT: usize = 64;
#[derive(Clone, Debug, PartialEq)]
pub struct HistoricalBatch {
    agent: String,
    epoch: i64,
    account: String,
    thread: String,
    primary: String,
    members: Vec<InputRow>,
    params: Value,
    digest: String,
    capture: Value,
}
impl HistoricalBatch {
    /// Saved rows in, checked proof or no proof out. Absent evidence is not rejection.
    pub fn from_records(agent: &Value, source: &Value) -> Result<Option<Self>, RepairError> {
        validate("PersistedAgentRecord", agent, "batch.agent")?;
        if serde_json::to_vec(source)
            .map_err(|_| RepairError::Invalid("batch"))?
            .len()
            > MAX_CAPTURE_BYTES
        {
            return Err(RepairError::Invalid("batch.bounds"));
        }
        let item = &source["item"];
        validate("PersistedTranscriptRecord", item, "batch.item")?;
        let raw = source["row"]["record"]
            .as_str()
            .ok_or(RepairError::Invalid("batch.row"))?;
        if raw.len() > TEXT_LIMIT {
            return Ok(None);
        }
        if serde_json::from_str::<Value>(raw).map_err(|_| RepairError::Invalid("batch.row"))?
            != *item
            || source["row"]["id"] != item["id"]
            || source["row"]["agent"] != agent["id"]
        {
            return Err(RepairError::Invalid("batch.row.scope"));
        }
        let Some(inputs) = item["inputs"].as_array() else {
            return Ok(None);
        };
        let id = agent["id"]
            .as_str()
            .ok_or(RepairError::Invalid("batch.agent.id"))?;
        let Some(key) = item["id"].as_str() else {
            return Ok(None);
        };
        let Some(primary) = key.strip_prefix(&format!("{id}:")) else {
            return Ok(None);
        };
        if inputs.is_empty()
            || inputs.len() > 32
            || inputs[0]["id"] != primary
            || item["role"] != "user"
            || truthy(item.get("assets"))
        {
            return Ok(None);
        }
        let text = if truthy(item.get("truncated")) {
            source["fulltext"].as_str()
        } else {
            if !source["fulltext"].is_null() && source["fulltext"] != item["text"] {
                return Ok(None);
            }
            item["text"].as_str()
        };
        let Some(text) = text.filter(|text| text.len() <= TEXT_LIMIT) else {
            return Ok(None);
        };
        let Some(saved) = source["members"].as_array() else {
            return Ok(None);
        };
        let mut members = Vec::new();
        let mut ids = std::collections::BTreeSet::new();
        for input in inputs {
            let Some(event_id) = input["id"].as_str().filter(|id| !id.is_empty()) else {
                return Ok(None);
            };
            if !ids.insert(event_id)
                || truthy(input.get("truncated"))
                || truthy(input.get("assets"))
            {
                return Ok(None);
            }
            let Some(value) = saved.iter().find(|event| event["id"] == event_id) else {
                return Ok(None);
            };
            let row: InputRow = serde_json::from_value(value.clone())
                .map_err(|_| RepairError::Invalid("batch.members"))?;
            if row.text.len() > TEXT_LIMIT {
                return Ok(None);
            }
            let Some(meta) = &row.metadata else {
                return Ok(None);
            };
            validate("RuntimeEventMetadataRecord", meta, "batch.metadata")?;
            let scope = &meta["contextManifest"]["epoch"];
            if row.agent != id
                || agent["epoch"] != row.epoch
                || !matches!(
                    row.status.as_str(),
                    "reserved" | "dispatching" | "uncertain" | "delivered"
                )
                || input["text"] != row.text
                || input["at"] != row.created
                || input["kind"] != row.kind
                || truthy(meta.get("assets"))
                || meta["transcriptItemId"] != key
                || meta["modelEventProjection"] != 1
                || (!scope.is_null()
                    && scope
                        != &json!([
                            agent["threadId"],
                            scope.get(1).cloned().unwrap_or(Value::Null)
                        ]))
            {
                return Ok(None);
            }
            if truthy(meta.get("native"))
                && [
                    ("agent", agent["id"].clone()),
                    ("epoch", agent["epoch"].clone()),
                    (
                        "accountKey",
                        json!(string_default(agent, "accountKey", "default")?),
                    ),
                    ("threadId", agent["threadId"].clone()),
                ]
                .iter()
                .any(|(field, value)| meta["native"][field] != *value)
            {
                return Ok(None);
            }
            members.push(row);
        }
        if members
            .first()
            .and_then(|member| member.metadata.as_ref())
            .and_then(|meta| meta["contextManifest"]["epoch"].as_array())
            .is_none_or(|scope| scope.len() != 2 || scope[0] != agent["threadId"])
        {
            return Ok(None);
        }
        let Some(params) = parameters(agent, primary, text, &members)? else {
            return Ok(None);
        };
        // Historical Python uses ensure_ascii=True with compact separators. Reuse
        // its escaped string encoder while removing only structural whitespace.
        let canonical = json!({"method":"turn/start", "params":params});
        let digest = sha256_hex(ascii_compact(&canonical).as_bytes());
        let member_proofs = members.iter().map(|member| {
            json!({"event":{"id":member.id,"agent":member.agent,"epoch":member.epoch,"kind":member.kind,"text":member.text,"status":member.status,"created":member.created,"turn_id":member.turn_id,"error":member.error},"metadata":member.metadata})
        }).collect::<Vec<_>>();
        let capture = json!({"agent":id,"epoch":agent["epoch"],"threadId":agent["threadId"],"accountKey":string_default(agent,"accountKey","default")?,"primary":primary,"item":source["row"],"fulltext":source["fulltext"],"members":member_proofs,"params":params,"digest":digest});
        if python_json(&capture).len() > MAX_CAPTURE_BYTES {
            return Ok(None);
        }
        Ok(Some(Self {
            capture,
            agent: id.to_owned(),
            epoch: agent["epoch"]
                .as_i64()
                .ok_or(RepairError::Invalid("batch.epoch"))?,
            account: string_default(agent, "accountKey", "default")?.to_owned(),
            thread: agent["threadId"]
                .as_str()
                .ok_or(RepairError::Invalid("batch.thread"))?
                .to_owned(),
            primary: primary.to_owned(),
            members,
            params,
            digest,
        }))
    }
    pub fn capture(&self) -> &Value {
        &self.capture
    }
    pub fn params(&self) -> &Value {
        &self.params
    }
    pub fn digest(&self) -> &str {
        &self.digest
    }
    pub fn accepted(
        &self,
        turns: &[Value],
        operations: &[Value],
        handle: &str,
    ) -> BTreeMap<String, String> {
        let mut result = BTreeMap::new();
        if handle != format!("account:{}", self.account) {
            return result;
        }
        let matches = turns
            .iter()
            .filter(|turn| {
                turn["id"].as_str().is_some_and(|id| !id.is_empty())
                    && (turn["startOutcome"].is_null() || turn["startOutcome"] == "accepted")
                    && (turn["clientUserMessageId"].is_null()
                        || turn["clientUserMessageId"] == self.primary)
                    && turn["items"].as_array().is_some_and(|items| {
                        items
                            .iter()
                            .filter(|item| {
                                item["type"] == "userMessage"
                                    && item["clientId"] == self.primary
                                    && item["id"].as_str().is_some_and(|id| !id.is_empty())
                                    && content(&item["content"])
                                        == Some(self.params["input"].clone())
                            })
                            .count()
                            == 1
                    })
            })
            .collect::<Vec<_>>();
        if matches.len() != 1 {
            return result;
        }
        let prefix = format!("turn:{}:{}:attempt:", self.agent, self.primary);
        let rows = operations
            .iter()
            .filter(|row| {
                row["handle"] == handle
                    && row["operation_id"]
                        .as_str()
                        .is_some_and(|id| id.starts_with(&prefix))
            })
            .collect::<Vec<_>>();
        if rows.len() > OPERATION_LIMIT
            || rows
                .iter()
                .filter(|row| {
                    row["operation_id"]
                        .as_str()
                        .and_then(|id| id.strip_prefix(&prefix))
                        .is_some_and(|suffix| {
                            uuid::Uuid::parse_str(suffix).is_ok_and(|id| id.to_string() == suffix)
                        })
                        && row["digest"] == self.digest
                        && row["native_id"].as_u64().is_some_and(|id| id > 0)
                        && row["accepted"]
                            .as_f64()
                            .is_some_and(|at| at.is_finite() && at > 0.0)
                })
                .count()
                != 1
        {
            return result;
        }
        if let Some(turn) = matches[0]["id"].as_str() {
            for member in &self.members {
                if member.status != "delivered" {
                    result.insert(member.id.clone(), turn.to_owned());
                }
            }
        }
        result
    }
}
fn parameters(
    agent: &Value,
    primary: &str,
    text: &str,
    members: &[InputRow],
) -> Result<Option<Value>, RepairError> {
    if string_default(agent, "provider", "codex")? != "codex"
        || agent["model"].as_str().is_none_or(str::is_empty)
    {
        return Ok(None);
    }
    let mut text = text.to_owned();
    let mut clocks = Vec::new();
    for member in members {
        let at = member
            .metadata
            .as_ref()
            .and_then(|meta| meta.get("acceptedAt"));
        if matches!(member.kind.as_str(), "user" | "followup") && at.is_some_and(|at| !at.is_null())
        {
            let Some(at) = at.and_then(Value::as_f64).filter(|at| at.is_finite()) else {
                return Ok(None);
            };
            let Some(stamp) = DateTime::from_timestamp_millis(
                ((at * 1_000_000.0).round_ties_even() / 1000.0).floor() as i64,
            ) else {
                return Ok(None);
            };
            clocks.push(format!(
                "Message {} accepted at {}",
                member.id,
                stamp.to_rfc3339_opts(SecondsFormat::Millis, true)
            ));
        }
    }
    if !clocks.is_empty() {
        text.push_str("\n\n[Time awareness, message receipt time]\n");
        text.push_str(&clocks.join("\n"));
    }
    let mut params = json!({"threadId":agent["threadId"], "model":agent["model"], "clientUserMessageId":primary,"input":[{"type":"text","text":text}],"serviceTier":if agent["fastMode"] == true { "priority" } else { "default" }});
    match agent["yoloMode"].as_bool() {
        Some(true) => {
            params["approvalPolicy"] = json!("never");
            params["sandboxPolicy"] = json!({"type":"dangerFullAccess"});
        }
        Some(false) => {
            params["approvalPolicy"] = json!("on-request");
            params["sandboxPolicy"] = if agent["role"] == "reviewer" {
                json!({"type":"readOnly"})
            } else {
                if agent["isLead"] == true || !agent["cwd"].is_string() {
                    return Ok(None);
                }
                json!({"type":"workspaceWrite","writableRoots":[agent["cwd"]],"networkAccess":false})
            };
        }
        None => {}
    }
    params["cyberAccessProgram"] = if agent["daybreakEnabled"] == true {
        if !matches!(
            agent["cyberAccessProgram"].as_str(),
            Some("daybreakBlue" | "daybreakRed")
        ) {
            return Ok(None);
        }
        agent["cyberAccessProgram"].clone()
    } else {
        json!("standard")
    };
    let effort = agent.get("nativeEffort").or_else(|| agent.get("effort"));
    if let Some(effort) = effort.filter(|v| !v.is_null()) {
        params["effort"] = effort.clone();
    }
    Ok(Some(params))
}
fn content(value: &Value) -> Option<Value> {
    let mut parts = Vec::new();
    for part in value.as_array()? {
        let object = part.as_object()?;
        if part["type"] != "text"
            || !part["text"].is_string()
            || object.keys().any(|key| {
                !matches!(
                    key.as_str(),
                    "type" | "text" | "text_elements" | "textElements"
                )
            })
            || ["text_elements", "textElements"]
                .iter()
                .any(|key| object.get(*key).is_some_and(|value| value != &json!([])))
        {
            return None;
        }
        parts.push(json!({"type":"text","text":part["text"]}));
    }
    Some(Value::Array(parts))
}
fn ascii_compact(value: &Value) -> String {
    match value {
        Value::Object(fields) => format!(
            "{{{}}}",
            fields
                .iter()
                .map(|(key, value)| format!(
                    "{}:{}",
                    python_json(&json!(key)),
                    ascii_compact(value)
                ))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Value::Array(values) => format!(
            "[{}]",
            values
                .iter()
                .map(ascii_compact)
                .collect::<Vec<_>>()
                .join(",")
        ),
        _ => python_json(value),
    }
}
