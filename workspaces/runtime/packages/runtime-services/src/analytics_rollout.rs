//! Pure translation of native rollout records. Context contains identities and
//! token counters only; response bodies live only for the bounded input batch.
use crate::analytics_capture::truthy;
use crate::analytics_capture::{CaptureError, first, number};
use serde_json::{Value, json};

#[derive(Clone, Debug, PartialEq)]
pub enum RolloutAction {
    Event {
        method: String,
        params: Value,
        at: f64,
    },
    Payload {
        params: Value,
        at: f64,
    },
    WrongThread,
}
#[derive(Clone)]
pub struct RolloutContext {
    value: Value,
}
impl RolloutContext {
    pub fn from_record(value: Value) -> Result<Self, CaptureError> {
        if let Some((pointer, _)) = studio_runtime_schema::first_error("AnalyticsContext", &value)
            .map_err(studio_runtime_store::StoreError::SchemaValidation)?
        {
            return Err(studio_runtime_store::StoreError::SchemaValidation(format!(
                "analytics_history/context{pointer}"
            ))
            .into());
        }
        Ok(Self { value })
    }
    pub fn record(&self) -> &Value {
        &self.value
    }
    pub fn allow_sources(&mut self, threads: Vec<String>) {
        self.value["allowedSourceThreadIds"] = json!(threads)
    }
    pub fn actions(
        &mut self,
        record: &Value,
        identity: &str,
        fallback_at: f64,
    ) -> Result<Vec<RolloutAction>, CaptureError> {
        self.parse(record, identity, fallback_at, 0)
    }
    fn parse(
        &mut self,
        record: &Value,
        identity: &str,
        fallback_at: f64,
        depth: usize,
    ) -> Result<Vec<RolloutAction>, CaptureError> {
        if depth > 8 {
            return Err(CaptureError::Invalid("/rollout/compaction/depth"));
        }
        if record.to_string().len() > super::analytics_history::MAX_LINE_BYTES {
            return Err(CaptureError::Invalid("/rollout/record/size"));
        }
        let p = &record["payload"];
        if !p.is_object() {
            return Ok(vec![]);
        }
        let at = timestamp(&record["timestamp"]).unwrap_or(fallback_at);
        let time_source = if timestamp(&record["timestamp"]).is_some() {
            "record"
        } else {
            "fileModifiedEstimate"
        };
        let mut actions = Vec::new();
        match record["type"].as_str().unwrap_or("") {
            "session_meta" => {
                self.value["window"] = p
                    .get("context_window")
                    .unwrap_or(&self.value["window"])
                    .clone();
            }
            "turn_context" => {
                for (src, dst) in [
                    ("turn_id", "turnId"),
                    ("model", "model"),
                    ("effort", "effort"),
                ] {
                    if !p[src].is_null() {
                        self.value[dst] = p[src].clone();
                    }
                }
            }
            "token_usage_record" => {
                if self.wrong_thread(p) {
                    return Ok(vec![RolloutAction::WrongThread]);
                }
                let turn = first(&[&p["turn_id"], &self.value["turnId"]]);
                let usage = tokens(&p["usage"])?;
                self.value["pendingUsage"] =
                    json!({"responseId":p["response_id"],"turnId":turn,"usage":usage});
                self.value["requestUsageAvailable"] = json!(true);
                actions.push(event("thread/tokenUsage/updated",json!({"threadId":self.value["threadId"],"turnId":turn,"responseId":p["response_id"],"tokenUsage":{"total":tokens(&p["thread_token_usage"])?,"last":usage,"modelContextWindow":self.value["window"]},"turnUsage":tokens(&p["turn_token_usage"])?,"requestUsage":usage,"rawTokenUsageRecord":p,"usageSource":"responseRecord","_analyticsUsageKind":"response"}),at));
            }
            "response_item" => {
                let mut body = p.clone();
                body["_analyticsId"] = json!(identity);
                body["_analyticsTurnId"] = first(&[
                    &p["internal_chat_message_metadata_passthrough"]["turn_id"],
                    &self.value["turnId"],
                ]);
                actions.push(RolloutAction::Payload {
                    params: body,
                    at: timestamp(&p["internal_chat_message_metadata_passthrough"]["create_time"])
                        .unwrap_or(at),
                });
            }
            "compacted" => {
                for value in p.as_object().into_iter().flat_map(|v| v.values()) {
                    if value.as_array().is_some_and(|v| v.len() > 4096) {
                        return Err(CaptureError::Invalid("/rollout/compaction/items"));
                    }
                }
                let mut body = json!({"threadId":self.value["threadId"],"turnId":self.value["turnId"],"id":identity});
                for k in [
                    "window_number",
                    "first_window_id",
                    "previous_window_id",
                    "window_id",
                    "compaction_response_id",
                ] {
                    body[k] = p[k].clone()
                }
                actions.push(event("analytics/compaction", body, at));
                for (key, category) in [
                    ("replacement_history", "compactionReplacement"),
                    ("guardian_history", "compactionGuardian"),
                ] {
                    if let Some(items) = p[key].as_array() {
                        for (index, item) in items.iter().enumerate() {
                            if item.is_object() {
                                let mut body = item.clone();
                                body["_analyticsCategory"] = json!(category);
                                body["_analyticsId"] = json!(format!("{identity}:{key}:{index}"));
                                body["_analyticsTurnId"] = self.value["turnId"].clone();
                                actions.push(RolloutAction::Payload { params: body, at })
                            }
                        }
                    }
                }
                if p["latest_token_usage_record"].is_object() {
                    actions.extend(self.parse(&json!({"type":"token_usage_record","payload":p["latest_token_usage_record"],"timestamp":record["timestamp"]}),&format!("{identity}:usage"),at,depth+1)?);
                }
            }
            "event_msg" => {
                if self.wrong_thread(p) {
                    return Ok(vec![RolloutAction::WrongThread]);
                }
                match p["type"].as_str().unwrap_or("") {
                    "task_started" => {
                        self.value["turnId"] = p["turn_id"].clone();
                        self.value["window"] = p
                            .get("model_context_window")
                            .unwrap_or(&self.value["window"])
                            .clone();
                        actions.push(event("turn/started",json!({"threadId":self.value["threadId"],"turnId":p["turn_id"],"turn":{"id":p["turn_id"]}}),at));
                    }
                    "task_complete" | "turn_aborted" => {
                        let turn = first(&[&p["turn_id"], &self.value["turnId"]]);
                        let status = if truthy(&p["error"]) {
                            json!("failed")
                        } else if p["type"] == "turn_aborted" {
                            json!("interrupted")
                        } else {
                            first(&[&p["status"], &json!("completed")])
                        };
                        actions.push(event("turn/completed",json!({"threadId":self.value["threadId"],"turnId":turn,"turn":{"id":turn,"status":status,"error":p["error"]},"durationMs":p["duration_ms"],"timeToFirstTokenMs":p["time_to_first_token_ms"]}),at));
                    }
                    "item_started" | "item_completed" if p["item"].is_object() => {
                        let mut body = json!({"threadId":self.value["threadId"],"turnId":first(&[&p["turn_id"],&self.value["turnId"]]),"item":normalize_item(&p["item"])?});
                        for (src, dst) in [
                            ("started_at_ms", "startedAt"),
                            ("completed_at_ms", "completedAt"),
                        ] {
                            if let Some(n) = number(&p[src]) {
                                body[dst] = json!(n / 1000.)
                            }
                        }
                        actions.push(event(
                            if p["type"] == "item_started" {
                                "item/started"
                            } else {
                                "item/completed"
                            },
                            body,
                            at,
                        ));
                    }
                    "token_count" => {
                        let info = &p["info"];
                        if info.is_object() {
                            let total = tokens(&info["total_token_usage"])?;
                            let last = tokens(&info["last_token_usage"])?;
                            let pending = &self.value["pendingUsage"];
                            let previous = &self.value["noticeAssociation"];
                            let matches = truthy(&pending["responseId"])
                                && pending["turnId"] == self.value["turnId"]
                                && [
                                    "inputTokens",
                                    "cachedInputTokens",
                                    "outputTokens",
                                    "reasoningOutputTokens",
                                    "totalTokens",
                                ]
                                .iter()
                                .all(|k| {
                                    last.get(*k).is_some() && last[*k] == pending["usage"][*k]
                                });
                            let response = if matches {
                                pending["responseId"].clone()
                            } else if previous["turnId"] == self.value["turnId"]
                                && previous["total"] == total
                                && previous["last"] == last
                            {
                                previous["responseId"].clone()
                            } else {
                                Value::Null
                            };
                            if matches {
                                self.value["pendingUsage"] = Value::Null;
                                self.value["noticeAssociation"] = json!({"responseId":response,"turnId":self.value["turnId"],"total":total,"last":last});
                            }
                            actions.push(event("thread/tokenUsage/updated",json!({"threadId":self.value["threadId"],"turnId":self.value["turnId"],"responseId":response,"usageSource":"tokenCount","_analyticsUsageKind":"notice","requestUsageAvailable":self.value.get("requestUsageAvailable").cloned().unwrap_or(json!(false)),"tokenUsage":{"total":total,"last":last,"modelContextWindow":info.get("model_context_window").unwrap_or(&self.value["window"])}}),at));
                        }
                        if !p["rate_limits"].is_null() {
                            actions.push(event(
                                "analytics/rateLimits",
                                json!({"rateLimits":p["rate_limits"]}),
                                at,
                            ));
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        for action in &mut actions {
            match action {
                RolloutAction::Event {
                    params,
                    at: event_at,
                    ..
                }
                | RolloutAction::Payload {
                    params,
                    at: event_at,
                } => {
                    if params.get("_analyticsTimestampSource").is_none() {
                        params["_analyticsTimestampSource"] = json!(if *event_at != at {
                            "metadata"
                        } else {
                            time_source
                        });
                    }
                }
                RolloutAction::WrongThread => {}
            }
        }
        Ok(actions)
    }
    fn wrong_thread(&self, p: &Value) -> bool {
        truthy(&p["thread_id"])
            && p["thread_id"] != self.value["threadId"]
            && !self.value["allowedSourceThreadIds"]
                .as_array()
                .is_some_and(|ids| ids.contains(&p["thread_id"]))
    }
}
fn event(method: &str, params: Value, at: f64) -> RolloutAction {
    RolloutAction::Event {
        method: method.into(),
        params,
        at,
    }
}
pub fn timestamp(value: &Value) -> Option<f64> {
    number(value).filter(|v| *v >= 0.).or_else(|| {
        value
            .as_str()
            .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
            .map(|t| t.timestamp() as f64 + t.timestamp_subsec_nanos() as f64 / 1e9)
    })
}
fn tokens(v: &Value) -> Result<Value, CaptureError> {
    if v.is_null() {
        return Ok(json!({}));
    }
    let Some(m) = v.as_object() else {
        return Err(CaptureError::Invalid("/rollout/tokens"));
    };
    Ok(Value::Object(
        m.iter()
            .map(|(k, v)| {
                (
                    match k.as_str() {
                        "input_tokens" => "inputTokens",
                        "cached_input_tokens" => "cachedInputTokens",
                        "cache_write_input_tokens" => "cacheWriteInputTokens",
                        "output_tokens" => "outputTokens",
                        "reasoning_output_tokens" => "reasoningOutputTokens",
                        "total_tokens" => "totalTokens",
                        _ => k,
                    }
                    .to_owned(),
                    v.clone(),
                )
            })
            .collect(),
    ))
}
fn normalize_item(v: &Value) -> Result<Value, CaptureError> {
    let Some(m) = v.as_object() else {
        return Err(CaptureError::Invalid("/rollout/item"));
    };
    let mut result = Value::Object(
        m.iter()
            .map(|(k, v)| {
                (
                    match k.as_str() {
                        "process_id" => "processId",
                        "parsed_cmd" => "commandActions",
                        "aggregated_output" => "aggregatedOutput",
                        "exit_code" => "exitCode",
                        "content_items" => "contentItems",
                        "client_id" => "clientId",
                        "summary_text" => "summary",
                        "raw_content" => "content",
                        "formatted_output" => "formattedOutput",
                        _ => k,
                    }
                    .to_owned(),
                    v.clone(),
                )
            })
            .collect(),
    );
    let kind = result["type"].as_str().unwrap_or("");
    let mut chars = kind.chars();
    result["type"] = json!(
        chars
            .next()
            .map(|c| c.to_lowercase().collect::<String>() + chars.as_str())
            .unwrap_or_default()
    );
    if let Some(duration) = result.as_object_mut().and_then(|m| m.remove("duration")) {
        result["durationMs"] = if duration.is_object() {
            json!(
                number(&duration["secs"]).unwrap_or(0.) * 1000.
                    + number(&duration["nanos"]).unwrap_or(0.) / 1e6
            )
        } else {
            duration
        };
    }
    if result["type"] == "agentMessage" && result.get("text").is_none() {
        let content = result.get("content").cloned().unwrap_or(json!([]));
        let text = match content {
            Value::String(s) => Some(s),
            Value::Array(a) => Some(
                a.iter()
                    .filter(|v| v.is_object())
                    .map(|v| v["text"].as_str().unwrap_or(""))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ),
            _ => None,
        };
        if let Some(text) = text {
            result["text"] = json!(text);
        }
    }
    Ok(result)
}
