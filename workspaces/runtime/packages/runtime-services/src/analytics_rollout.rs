//! Pure translation of native rollout records. Context contains identities and
//! token counters only; response bodies live only for the bounded input batch.
use crate::analytics_capture::{CaptureError, first, number};
use serde_json::{Value, json};
use studio_runtime_schema::usage::{UsageProvider, parse_token_usage};
use studio_runtime_store::truthy;

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
                let window = p
                    .get("context_window")
                    .unwrap_or(&self.value["window"])
                    .clone();
                self.value["window"] = canonical_context_window(&window)?;
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
                actions.push(usage_event(
                    json!({
                        "threadId":self.value["threadId"], "turnId":turn,
                        "responseId":p["response_id"], "usageSource":"responseRecord",
                        "requestUsage":usage, "rawTokenUsageRecord":p,
                        "tokenUsage":{"modelContextWindow":self.value["window"]}
                    }),
                    at,
                    &self.value["model"],
                )?);
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
                        let window = p
                            .get("model_context_window")
                            .unwrap_or(&self.value["window"])
                            .clone();
                        self.value["window"] = canonical_context_window(&window)?;
                        actions.push(event("turn/started",json!({"threadId":self.value["threadId"],"turnId":p["turn_id"],"turn":{"id":p["turn_id"]}}),at));
                    }
                    "task_complete" | "turn_aborted" => {
                        let turn = first(&[&p["turn_id"], &self.value["turnId"]]);
                        let status = if truthy(Some(&p["error"])) {
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
                            let matches = truthy(Some(&pending["responseId"]))
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
                            let model_context_window = match info.get("model_context_window") {
                                Some(window) => canonical_context_window(window)?,
                                None => self.value["window"].clone(),
                            };
                            actions.push(event("thread/tokenUsage/updated",json!({"threadId":self.value["threadId"],"turnId":self.value["turnId"],"responseId":response,"usageSource":"tokenCount","_analyticsUsageKind":"notice","requestUsageAvailable":self.value.get("requestUsageAvailable").cloned().unwrap_or(json!(false)),"tokenUsage":{"total":total,"last":last,"modelContextWindow":model_context_window}}),at));
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
        truthy(Some(&p["thread_id"]))
            && p["thread_id"] != self.value["threadId"]
            && !self.value["allowedSourceThreadIds"]
                .as_array()
                .is_some_and(|ids| ids.contains(&p["thread_id"]))
    }
}

fn canonical_context_window(value: &Value) -> Result<Value, CaptureError> {
    let context = json!({"threadId":"", "window":value});
    match studio_runtime_schema::first_error("AnalyticsContext", &context)
        .map_err(studio_runtime_store::StoreError::SchemaValidation)?
    {
        None => Ok(value.clone()),
        Some((pointer, _)) if pointer == "/window" => Ok(Value::Null),
        Some((pointer, _)) => Err(studio_runtime_store::StoreError::SchemaValidation(format!(
            "analytics_history/context{pointer}"
        ))
        .into()),
    }
}
fn event(method: &str, params: Value, at: f64) -> RolloutAction {
    RolloutAction::Event {
        method: method.into(),
        params,
        at,
    }
}
fn usage_event(mut params: Value, at: f64, model: &Value) -> Result<RolloutAction, CaptureError> {
    params["model"] = model.clone();
    let observation = parse_token_usage(UsageProvider::Codex, &params)?;
    Ok(event(
        "thread/tokenUsage/updated",
        json!({"threadId":params["threadId"],"turnId":params["turnId"],"studioUsage":observation}),
        at,
    ))
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

#[cfg(test)]
mod tests {
    use super::RolloutContext;
    use serde_json::json;

    #[test]
    fn typed_usage_rollout_preserves_response_association_for_notices() {
        let mut context =
            RolloutContext::from_record(json!({"threadId":"thread","turnId":"turn","model":"m"}))
                .expect("context");
        let usage = json!({"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1,"total_tokens":13});
        context.actions(&json!({"type":"token_usage_record","payload":{"response_id":"response","usage":usage}}),"response-record",1.0).expect("response");
        let notice = json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":usage,"last_token_usage":usage}}});
        for identity in ["notice", "duplicate-notice"] {
            let actions = context.actions(&notice, identity, 2.0).expect("notice");
            let super::RolloutAction::Event { params, .. } = &actions[0] else {
                panic!("notice")
            };
            assert_eq!(
                params["responseId"], "response",
                "preserve the pre-upgrade response association for budget ambiguity handling"
            );
            assert_eq!(params["usageSource"], "tokenCount");
            assert_eq!(params["requestUsageAvailable"], true);
        }
    }

    #[test]
    fn typed_usage_imported_notice_preserves_native_notice_identity() {
        let mut context = RolloutContext::from_record(json!({"threadId":"thread","turnId":"turn"}))
            .expect("context");
        let native = json!({"input_tokens":10,"cached_input_tokens":2,"output_tokens":3,"reasoning_output_tokens":1,"total_tokens":13});
        let actions=context.actions(&json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":native,"last_token_usage":native,"model_context_window":100}}}),"record",1.0).expect("import");
        let super::RolloutAction::Event { params, .. } = &actions[0] else {
            panic!("notice")
        };
        let previous = json!({"_analyticsTimestampSource":"fileModifiedEstimate","threadId":"thread","turnId":"turn","tokenUsage":{"total":super::tokens(&native).expect("total"),"last":super::tokens(&native).expect("last"),"modelContextWindow":100}});
        assert_eq!(
            params["tokenUsage"], previous["tokenUsage"],
            "native counter objects must preserve pre-upgrade notice receipt identity"
        );
        let receipt = |params: &serde_json::Value| {
            let identity = studio_runtime_schema::usage::legacy_notice_identity(
                "agent",
                "account",
                Some("turn"),
                params,
            )
            .expect("native notice identity");
            studio_runtime_store::sha256_hex(
                studio_runtime_store::python_json_with_separators(&identity, ",", ":").as_bytes(),
            )
        };
        assert_eq!(
            receipt(params),
            receipt(&previous),
            "shared parser preserves the exact pre-upgrade receipt hash"
        );
    }

    #[test]
    fn typed_usage_rollout_emits_recovered_response_and_scoped_cumulative() {
        let mut context = RolloutContext::from_record(
            json!({"threadId":"thread","model":"gpt-5","turnId":"turn"}),
        )
        .expect("context");
        for (record, kind) in [
            (
                json!({"type":"token_usage_record","payload":{"response_id":"response","usage":{"input_tokens":10,"cached_input_tokens":3,"output_tokens":2,"reasoning_output_tokens":0,"total_tokens":12}}}),
                "response",
            ),
            (
                json!({"type":"event_msg","payload":{"type":"token_count","info":{"total_token_usage":{"input_tokens":10,"cached_input_tokens":3,"output_tokens":2,"reasoning_output_tokens":0,"total_tokens":12},"last_token_usage":{"input_tokens":10,"cached_input_tokens":3,"output_tokens":2,"reasoning_output_tokens":0,"total_tokens":12}}}}),
                "cumulative",
            ),
        ] {
            let actions = context
                .actions(&record, "record", 1.0)
                .expect("rollout actions");
            let super::RolloutAction::Event { params, .. } = &actions[0] else {
                panic!("usage event")
            };
            let normalized = serde_json::to_value(
                studio_runtime_schema::usage::parse_token_usage(
                    studio_runtime_schema::usage::UsageProvider::Codex,
                    params,
                )
                .expect("shared parser"),
            )
            .expect("observation");
            assert_eq!(normalized["kind"], kind);

            if kind == "response" {
                assert_eq!(normalized["evidence"], "recovered");
                assert!(params.get("tokenUsage").is_none());
                assert_eq!(normalized["tokens"]["uncachedInput"], 7);
            } else {
                assert_eq!(normalized["scope"], "codexThread");
                assert!(params.get("studioUsage").is_none());
            }
        }
    }

    #[test]
    fn context_window_import_accepts_only_schema_owned_number_or_null() {
        let mut context = RolloutContext::from_record(json!({"threadId":"thread","turnId":"turn"}))
            .expect("fixture context is valid");
        context
            .actions(
                &json!({"type":"session_meta","payload":{}}),
                "identity",
                0.5,
            )
            .expect("missing provider window is normalized to null");
        assert!(context.record()["window"].is_null());

        context
            .actions(
                &json!({
                    "type":"session_meta",
                    "payload":{"context_window":{"window_id":"fixture-window"}}
                }),
                "identity",
                1.0,
            )
            .expect("malformed provider window is dropped");
        assert!(context.record()["window"].is_null());

        context
            .actions(
                &json!({"type":"session_meta","payload":{"context_window":128000}}),
                "identity",
                2.0,
            )
            .expect("numeric provider window is preserved");
        assert_eq!(context.record()["window"], json!(128000));

        context
            .actions(
                &json!({
                    "type":"event_msg",
                    "payload":{"type":"task_started","model_context_window":{"window_id":"fixture-window"}}
                }),
                "identity",
                3.0,
            )
            .expect("malformed task window is dropped");
        assert!(context.record()["window"].is_null());

        let actions = context
            .actions(
                &json!({
                    "type":"event_msg",
                    "payload":{"type":"token_count","info":{
                        "total_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":1},
                        "last_token_usage":{"input_tokens":0,"cached_input_tokens":0,"output_tokens":1,"reasoning_output_tokens":0,"total_tokens":1},
                        "model_context_window":{"window_id":"fixture-window"}
                    }}
                }),
                "identity",
                4.0,
            )
            .expect("malformed token-count window is dropped");
        match &actions[0] {
            super::RolloutAction::Event { params, .. } => {
                assert!(params["tokenUsage"]["modelContextWindow"].is_null());
            }
            _ => panic!("token count produces a usage event"),
        }
    }
}
