//! Pure supervisor-v1 framing and retained-child launch policy shared by both adapters.
use crate::{BackendIdentity, OperationId, SupervisorAction, SupervisorError, SupervisorHandle};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::io;

const BACKEND_ONLY_ENVIRONMENT: &[&str] = &[
    "CODEX_AGENTS_BACKEND_ID",
    "CODEX_RUNTIME_LOCK_METRICS",
    "CODEX_AGENTS_PROVIDER_CAPTURE",
    "CODEX_AGENTS_PROVIDER_CAPTURE_FILE",
    "CODEX_NODE",
];
#[cfg(target_os = "linux")]
const RETAINED_LAUNCHER_ENVIRONMENT: &[&str] = &[
    "PATH",
    "LANG",
    "LC_CTYPE",
    "LC_ALL",
    "__PYVENV_LAUNCHER__",
    "COMMAND_MODE",
    "MallocNanoZone",
    "XPC_SERVICE_NAME",
    "__CFBundleIdentifier",
];

pub(crate) fn clean_environment(requested: &BTreeMap<String, String>) -> BTreeMap<String, String> {
    let mut clean = requested.clone();
    for key in BACKEND_ONLY_ENVIRONMENT {
        clean.remove(*key);
    }
    clean
}

#[cfg(target_os = "linux")]
pub(crate) fn verify_environment(
    original: &BTreeMap<String, String>,
    clean: &BTreeMap<String, String>,
) -> io::Result<()> {
    let without_launcher = |values: &BTreeMap<String, String>| {
        values
            .iter()
            .filter(|(key, _)| {
                !BACKEND_ONLY_ENVIRONMENT.contains(&key.as_str())
                    && !RETAINED_LAUNCHER_ENVIRONMENT.contains(&key.as_str())
            })
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect::<BTreeMap<_, _>>()
    };
    if without_launcher(original) != without_launcher(clean) {
        return Err(io::Error::other("retained provider environment differs"));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
pub(crate) fn parse_process_environment(raw: &[u8]) -> io::Result<BTreeMap<String, String>> {
    let mut environment = BTreeMap::new();
    for entry in raw
        .split(|byte| *byte == 0)
        .take_while(|entry| !entry.is_empty())
    {
        let Some(separator) = entry.iter().position(|byte| *byte == b'=') else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid process environment",
            ));
        };
        let key = std::str::from_utf8(&entry[..separator])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        let value = std::str::from_utf8(&entry[separator + 1..])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
        environment.insert(key.to_owned(), value.to_owned());
    }
    Ok(environment)
}

pub(crate) fn provider_native_id(message: &Value) -> Option<u64> {
    message
        .get("method")
        .and_then(Value::as_str)
        .and_then(|_| message.get("id"))
        .and_then(Value::as_u64)
}

pub(crate) fn initialized_operation_id(
    handle: &SupervisorHandle,
    generation: crate::Generation,
) -> OperationId {
    let suffix = if generation.0 > 1 {
        format!(":{}", generation.0)
    } else {
        String::new()
    };
    OperationId(format!("initialized:{}{suffix}", handle.0))
}

pub(crate) fn handshake_request(identity: &BackendIdentity, operator: bool) -> Value {
    let mut handshake = serde_json::json!({
        "protocol": 1,
        "stateDir": identity.state_dir,
        "backendId": identity.backend_id,
    });
    if operator && let Some(object) = handshake.as_object_mut() {
        object.remove("backendId");
        object.insert("operator".into(), Value::Bool(true));
    }
    handshake
}

pub(crate) fn validate_handshake(response: &str) -> Result<(), SupervisorError> {
    let value: Value = serde_json::from_str(response)?;
    if value.get("ok") == Some(&Value::Bool(true)) {
        Ok(())
    } else {
        Err(SupervisorError::ProtocolMismatch {
            expected: 1,
            received: None,
            detail: value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unexpected handshake response")
                .to_owned(),
        })
    }
}

pub(crate) fn request_value<R: Serialize>(
    action: SupervisorAction,
    request_id: u64,
    request: R,
) -> Result<Value, SupervisorError> {
    let mut value = serde_json::to_value(request)?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| SupervisorError::InvalidFrame("request must be an object".into()))?;
    object.insert("requestId".into(), Value::from(request_id));
    object.insert("action".into(), Value::from(action.to_string()));
    if !matches!(
        action,
        SupervisorAction::Health
            | SupervisorAction::FinishFallback
            | SupervisorAction::AdminCloseHandle
    ) && !object.contains_key("handle")
    {
        return Err(SupervisorError::InvalidFrame(format!(
            "{} request requires handle",
            action
        )));
    }
    Ok(value)
}

pub(crate) fn response_value<O: for<'de> Deserialize<'de>>(
    action: SupervisorAction,
    request_id: u64,
    response: &str,
) -> Result<O, SupervisorError> {
    let response: Value = match serde_json::from_str(response) {
        Ok(response) => response,
        Err(_) => return Err(SupervisorError::UnknownOutcome { action, request_id }),
    };
    if response.get("requestId").and_then(Value::as_u64) != Some(request_id) {
        return Err(SupervisorError::UnknownOutcome { action, request_id });
    }
    if let Some(message) = response.get("error").and_then(Value::as_str) {
        return Err(SupervisorError::Remote {
            action,
            message: message.to_owned(),
        });
    }
    let result = response
        .get("result")
        .cloned()
        .unwrap_or_else(|| Value::Object(Map::new()));
    serde_json::from_value(result)
        .map_err(|_| SupervisorError::UnknownOutcome { action, request_id })
}

pub(crate) fn serialize_frame(value: &Value) -> Result<Vec<u8>, SupervisorError> {
    let mut frame = serde_json::to_vec(value)?;
    if frame.len() + 1 > crate::transport::MAX_FRAME_BYTES {
        return Err(SupervisorError::InvalidFrame(
            "request exceeds 1 MiB".into(),
        ));
    }
    frame.push(b'\n');
    Ok(frame)
}

pub(crate) fn assign_reply_id(
    payload: &mut Value,
    remote_id: u64,
    local_id: Option<Value>,
) -> io::Result<()> {
    let Some(payload) = payload.as_object_mut() else {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "provider response is not an object",
        ));
    };
    payload.insert(
        "id".into(),
        local_id.unwrap_or_else(|| Value::String(format!("supervisor-orphan-{remote_id}"))),
    );
    Ok(())
}
