//! Pure native notice decisions. The adapter supplies checked stored records and time.
use serde_json::{Value, json};
use studio_runtime_store::sha256_hex;

use super::{NotificationDispatchError, format_utc};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum NoticeKind {
    Warning,
    Guardian,
    Config,
    Deprecation,
    McpStartup,
    McpLogin,
    StrictReview,
    HookStarted,
    HookCompleted,
    Safety,
    Verification,
    ReviewStarted,
    ReviewCompleted,
    Error,
    AuthStarted,
    AuthCompleted,
}
impl NoticeKind {
    pub(super) fn parse(method: &str) -> Option<Self> {
        Some(match method {
            "warning" => Self::Warning,
            "guardianWarning" => Self::Guardian,
            "configWarning" => Self::Config,
            "deprecationNotice" => Self::Deprecation,
            "mcpServer/startupStatus/updated" => Self::McpStartup,
            "mcpServer/oauthLogin/completed" => Self::McpLogin,
            "autoApprovalReview/strictReviewRequired" => Self::StrictReview,
            "hook/started" => Self::HookStarted,
            "hook/completed" => Self::HookCompleted,
            "model/safetyBuffering/updated" => Self::Safety,
            "model/verification" => Self::Verification,
            "item/autoApprovalReview/started" => Self::ReviewStarted,
            "item/autoApprovalReview/completed" => Self::ReviewCompleted,
            "error" => Self::Error,
            "modelProvider/authRecoveryStarted" => Self::AuthStarted,
            "modelProvider/authRecoveryCompleted" => Self::AuthCompleted,
            _ => return None,
        })
    }
}

#[derive(Clone, Debug)]
pub(super) struct NoticeItem {
    pub key: String,
    pub text: String,
    pub metadata: Value,
}
#[derive(Default)]
pub(super) struct NoticeDecision {
    pub agent: Option<Value>,
    pub item: Option<NoticeItem>,
    pub refresh_limits: bool,
}

fn nonempty<'a>(value: &'a Value, key: &str, default: &'a str) -> &'a str {
    value
        .get(key)
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(default)
}
fn notice(key: String, text: String, severity: &str, mut metadata: Value) -> NoticeItem {
    metadata["nativeNotice"] = json!(severity);
    NoticeItem {
        key: format!("native-notice:{key}"),
        text,
        metadata,
    }
}

pub(super) fn warning_text(kind: NoticeKind, params: &Value) -> String {
    let text = nonempty(params, "message", nonempty(params, "summary", ""));
    match kind {
        NoticeKind::StrictReview => {
            "Codex requires additional safety checks. Some tools may take longer.".into()
        }
        NoticeKind::McpLogin if params["success"] == false => format!(
            "{}: {}",
            nonempty(params, "name", "Connected tool"),
            nonempty(params, "error", "Sign-in failed.")
        ),
        NoticeKind::McpLogin => String::new(),
        NoticeKind::McpStartup
            if matches!(params["status"].as_str(), Some("failed" | "cancelled")) =>
        {
            format!(
                "{}: {}",
                nonempty(params, "name", "Connected tool"),
                nonempty(params, "error", nonempty(params, "status", ""))
            )
        }
        NoticeKind::McpStartup => String::new(),
        NoticeKind::Guardian if text.starts_with("Automatic approval review approved (") => {
            String::new()
        }
        _ => text.into(),
    }
}

pub(super) fn decide(
    snapshot: &super::snapshot::NotificationSnapshot,
    method: &str,
    kind: NoticeKind,
    params: &Value,
    account: &str,
    generation: u64,
    at: f64,
) -> Result<NoticeDecision, NotificationDispatchError> {
    let original = snapshot.agent();
    let previous_item = snapshot.item();
    let mut decision = NoticeDecision::default();
    let turn = &params["turnId"];
    if matches!(kind, NoticeKind::HookStarted | NoticeKind::HookCompleted) {
        if params["threadId"].as_str().is_some_and(|s| !s.is_empty()) {
            decision.item = hook(kind, params, previous_item);
        }
        return Ok(decision);
    }
    if turn.as_str().is_some_and(|s| !s.is_empty())
        && (turn != &original["turnId"] || original["inFlight"] != true)
    {
        return Ok(decision);
    }
    if matches!(
        kind,
        NoticeKind::Safety
            | NoticeKind::Verification
            | NoticeKind::ReviewStarted
            | NoticeKind::ReviewCompleted
    ) && (params["threadId"].as_str().is_none_or(str::is_empty)
        || turn.as_str().is_none_or(str::is_empty))
    {
        return Ok(decision);
    }
    let mut agent = original.clone();
    match kind {
        NoticeKind::Safety => {
            let previous = &original["nativeSafetyBuffering"];
            let same = previous["turnId"] == *turn;
            let mut buffering = params.clone();
            buffering["accountKey"] = json!(account);
            buffering["connectionId"] = json!(format!("connection-{generation}"));
            buffering["at"] = if same {
                previous.get("at").cloned().unwrap_or(json!(at))
            } else {
                json!(at)
            };
            buffering["dismissed"] = json!(same && previous["dismissed"] == true);
            buffering["responseStarted"] = json!(
                (same && previous["responseStarted"] == true)
                    || original["nativeResponseTurn"] == *turn
            );
            if params["showBufferingUi"] == true && buffering["responseStarted"] != true {
                agent["nativeStatus"] = json!({"phase":"safety","turnId":turn,"at":at,"message":"Codex is checking this request"});
                agent["activity"] = json!({"phase":"safety","at":at});
            } else if agent["nativeStatus"]["phase"] == "safety" {
                remove(&mut agent, "nativeStatus");
                agent["activity"] = json!({"phase":"thinking","at":at});
            }
            agent["nativeSafetyBuffering"] = buffering;
        }
        NoticeKind::Verification => {
            if params["verifications"]
                .as_array()
                .is_some_and(|v| v.contains(&json!("trustedAccessForCyber")))
            {
                decision.item = Some(notice(
                    "model-verification".into(),
                    "Codex reports additional cybersecurity checks. Responses may take longer."
                        .into(),
                    "warning",
                    json!({"turnId":turn,"details":"Trusted Access: https://chatgpt.com/cyber"}),
                ));
            }
        }
        NoticeKind::ReviewStarted | NoticeKind::ReviewCompleted => {
            let review = &params["review"];
            let status = nonempty(review, "status", "");
            let label = match status {
                "inProgress" => "in progress",
                "timedOut" => "timed out",
                value => value,
            };
            let completed = kind == NoticeKind::ReviewCompleted;
            let approved = completed && status == "approved";
            let text = if approved {
                String::new()
            } else if completed {
                format!("Tool safety review: {label}")
            } else {
                "Checking tool permissions".into()
            };
            decision.item = Some(notice(
                format!("review:{}", params["reviewId"].as_str().unwrap_or("None")),
                text,
                if completed && !approved {
                    "warning"
                } else {
                    "info"
                },
                json!({"turnId":turn,"details":review.get("rationale").filter(|v| !v.is_null() && **v != "").unwrap_or(&review["reason"]),"nativeReview":params,"nativeHookQuiet":approved}),
            ));
        }
        NoticeKind::Error => {
            if params["threadId"].as_str().is_none_or(str::is_empty)
                || turn.as_str().is_none_or(str::is_empty)
            {
                return Ok(decision);
            }
            let fallback = json!({"message":"Codex reported an error."});
            let error = params
                .get("error")
                .filter(|v| !v.is_null() && **v != json!({}))
                .unwrap_or(&fallback);
            let error_kind = error_kind(error);
            let policy = matches!(error_kind, "cyberPolicy" | "misalignmentPolicyViolation");
            if params["willRetry"] == true && !policy {
                agent["nativeStatus"] =
                    json!({"phase":"retrying","error":error,"turnId":turn,"at":at});
                agent["activity"] = json!({"phase":"retrying","at":at});
            } else if error_kind == "activeTurnNotSteerable" {
                decision.item = Some(notice(
                    format!("steer:{}", turn.as_str().unwrap_or_default()),
                    error_message(error),
                    "warning",
                    json!({"turnId":turn,"nativeError":error}),
                ));
            } else {
                if policy {
                    agent["nativeFailureHold"] = json!(true);
                }
                remove(&mut agent, "nativeStatus");
                remove(&mut agent, "nativeSafetyBuffering");
                if error["codexErrorInfo"] == "misalignmentPolicyViolation" {
                    agent["nativeThreadBlock"] =
                        json!({"threadId":agent["threadId"],"error":error});
                }
                decision.refresh_limits =
                    matches!(error_kind, "usageLimitExceeded" | "rateLimitExceeded");
                agent["nativeTurnError"] = json!({"turnId":turn,"error":error});
                agent["error"] = error.clone();
                agent["activity"] = json!({"phase":"error","at":at});
                decision.item = Some(notice(
                    format!("error:{}", turn.as_str().unwrap_or_default()),
                    error_message(error),
                    "error",
                    json!({"turnId":turn,"nativeError":error,"cyberAccessProgram":agent["cyberAccessProgram"]}),
                ));
            }
        }
        NoticeKind::AuthStarted | NoticeKind::AuthCompleted => {
            let started = kind == NoticeKind::AuthStarted;
            if started {
                agent["nativeStatus"] =
                    json!({"phase":"auth","message":params["message"],"turnId":turn,"at":at});
                agent["activity"] = json!({"phase":"auth","at":at});
            } else if agent["nativeStatus"]["phase"] == "auth" {
                remove(&mut agent, "nativeStatus");
                agent["activity"] = json!({"phase":"thinking","at":at});
            }
            decision.item = Some(notice(
                format!("auth:{}", turn.as_str().unwrap_or("None")),
                nonempty(
                    params,
                    "message",
                    if started {
                        "Restoring sign-in."
                    } else {
                        "Sign-in restored."
                    },
                )
                .into(),
                "info",
                json!({"turnId":turn}),
            ));
        }
        _ => {
            let text = warning_text(kind, params);
            if text.is_empty() {
                return Ok(decision);
            }
            let identity = json!([method, text, params["details"]]);
            let key = sha256_hex(json_text(&identity).as_bytes());
            decision.item = Some(notice(
                key[..24].into(),
                text,
                "warning",
                json!({"details":params["details"],"accountWide":params["threadId"].as_str().is_none_or(str::is_empty)}),
            ));
        }
    }
    agent["events"] = json!(
        original["events"]
            .as_u64()
            .unwrap_or_default()
            .saturating_add(1)
    );
    agent["lastEvent"] = json!(format_utc(at)?);
    decision.agent = Some(agent);
    Ok(decision)
}

fn hook(kind: NoticeKind, params: &Value, previous: Option<&Value>) -> Option<NoticeItem> {
    let mut run = params.get("run")?.as_object()?.clone();
    let id = run
        .get("id")?
        .as_str()
        .filter(|s| !s.is_empty())?
        .to_owned();
    if kind == NoticeKind::HookStarted
        && previous
            .and_then(|p| p["nativeHook"]["status"].as_str())
            .is_some_and(|s| !s.is_empty() && s != "running")
    {
        return None;
    }
    let mut warning_seen = false;
    let entries: Vec<Value> = run
        .get("entries")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|entry| {
            if entry["kind"] == "context" {
                return false;
            }
            if entry["kind"] == "warning" {
                if warning_seen {
                    return false;
                }
                warning_seen = true;
            }
            true
        })
        .cloned()
        .collect();
    let status = run
        .get("status")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        .unwrap_or(if kind == NoticeKind::HookStarted {
            "running"
        } else {
            "completed"
        })
        .to_owned();
    run.insert("status".into(), json!(status));
    run.insert("entries".into(), json!(entries));
    if kind == NoticeKind::HookCompleted && status == "completed" && entries.is_empty() {
        return Some(notice(
            format!("hook:{id}"),
            String::new(),
            "info",
            json!({"turnId":params["turnId"],"nativeHook":run,"nativeHookQuiet":true}),
        ));
    }
    let mut text = match status.as_str() {
        "running" => "Hook running".into(),
        "completed" => "Hook completed".into(),
        "failed" => "Hook failed".into(),
        "blocked" => "Blocked by hook".into(),
        "stopped" => "Hook stopped".into(),
        _ => format!("Hook {status}"),
    };
    if let Some(event) = run
        .get("eventName")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
    {
        text += &format!(": {event}");
    }
    let details = run
        .get("statusMessage")
        .into_iter()
        .chain(entries.iter().filter_map(|e| e.get("text")))
        .filter_map(Value::as_str)
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("\n\n");
    Some(notice(
        format!("hook:{id}"),
        text,
        if matches!(status.as_str(), "failed" | "blocked" | "stopped") {
            "error"
        } else {
            "info"
        },
        json!({"turnId":params["turnId"],"details":if details.is_empty(){Value::Null}else{json!(details)},"nativeHook":run}),
    ))
}

pub(super) fn remove(value: &mut Value, key: &str) {
    if let Some(fields) = value.as_object_mut() {
        fields.remove(key);
    }
}
fn error_kind(error: &Value) -> &str {
    error["codexErrorInfo"]
        .as_str()
        .or_else(|| {
            error["codexErrorInfo"]
                .as_object()
                .and_then(|v| v.keys().next())
                .map(String::as_str)
        })
        .unwrap_or("")
}
pub(crate) fn error_message(error: &Value) -> String {
    crate::account_worker::history::message(error)
}

pub(super) use studio_runtime_store::python_json_utf8 as json_text;

/// Preserve generated object's insertion order, including fields appended by a transition.
pub(super) fn ordered_text(value: &Value, keys: impl IntoIterator<Item = String>) -> String {
    ordered_text_with(value, keys, json_text)
}

pub(super) fn ordered_ascii(value: &Value, keys: impl IntoIterator<Item = String>) -> String {
    ordered_text_with(value, keys, studio_runtime_store::python_json)
}

fn ordered_text_with(
    value: &Value,
    keys: impl IntoIterator<Item = String>,
    render: fn(&Value) -> String,
) -> String {
    let Some(fields) = value.as_object() else {
        return render(value);
    };
    let mut ordered = Vec::new();
    for key in keys.into_iter().chain(fields.keys().cloned()) {
        if fields.contains_key(&key) && !ordered.contains(&key) {
            ordered.push(key);
        }
    }
    format!(
        "{{{}}}",
        ordered
            .iter()
            .map(|key| format!("{}: {}", render(&json!(key)), render(&fields[key])))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub(super) fn object_keys(text: &str) -> Option<Vec<String>> {
    use serde::de::{Deserializer, MapAccess, Visitor};
    struct Keys;
    impl<'de> Visitor<'de> for Keys {
        type Value = Vec<String>;
        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("JSON object")
        }
        fn visit_map<M: MapAccess<'de>>(self, mut map: M) -> Result<Self::Value, M::Error> {
            let mut keys = Vec::new();
            while let Some((key, _)) = map.next_entry::<String, serde::de::IgnoredAny>()? {
                keys.push(key);
            }
            Ok(keys)
        }
    }
    serde_json::Deserializer::from_str(text)
        .deserialize_map(Keys)
        .ok()
}
