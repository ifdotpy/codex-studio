//! Pure supervisor-v1 framing and retained-child launch policy shared by both adapters.
use crate::protocol::{
    AckRequest, AdminCloseHandleRequest, CursorRequest, FinishFallbackRequest, HandleRequest,
    HealthRequest, NextBatchRequest, OpenRequest, OperationStatusRequest, RetireCommandRequest,
    WriteRequest,
};
use crate::shared_values_generated::PROCESS_SUPERVISOR_PROTOCOL_VERSION;
use crate::{
    BackendIdentity, OperationId, SUPERVISOR_PROTOCOL_V2, SupervisorAction, SupervisorError,
    SupervisorHandle, SupervisorHandshakeV2, SupervisorHandshakeV2Result,
};
use serde::{Deserialize, Serialize};
use serde_json::{Value, value::RawValue};
use std::collections::BTreeMap;
use std::io::{self, Write};

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
        "protocol": PROCESS_SUPERVISOR_PROTOCOL_VERSION,
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
            expected: PROCESS_SUPERVISOR_PROTOCOL_VERSION,
            received: None,
            detail: value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unexpected handshake response")
                .to_owned(),
        })
    }
}

pub(crate) fn handshake_request_v2(identity: &BackendIdentity) -> SupervisorHandshakeV2 {
    SupervisorHandshakeV2 {
        protocol: SUPERVISOR_PROTOCOL_V2,
        state_dir: identity.state_dir.clone(),
        backend_id: Some(identity.backend_id.clone()),
        probe: false,
        operator: false,
    }
}

pub(crate) fn validate_handshake_v2(response: &str) -> Result<(), SupervisorError> {
    let value: SupervisorHandshakeV2Result = serde_json::from_str(response)?;
    let received = value.protocol;
    if value.ok && received == Some(SUPERVISOR_PROTOCOL_V2) {
        Ok(())
    } else {
        Err(SupervisorError::ProtocolMismatch {
            expected: SUPERVISOR_PROTOCOL_V2,
            received,
            detail: value
                .error
                .unwrap_or_else(|| "supervisor did not confirm protocol v2".to_owned()),
        })
    }
}

struct RequestEnvelope<R> {
    request: R,
    action: String,
    request_id: u64,
}

pub(crate) trait SupervisorRequest: Serialize {
    fn has_handle(&self) -> bool;
}

impl<T: SupervisorRequest + ?Sized> SupervisorRequest for &T {
    fn has_handle(&self) -> bool {
        (**self).has_handle()
    }
}

macro_rules! handle_request {
    ($($request:ty),+ $(,)?) => {$ (
        impl SupervisorRequest for $request {
            fn has_handle(&self) -> bool { true }
        }
    )+ };
}

handle_request!(
    AckRequest,
    AdminCloseHandleRequest,
    CursorRequest,
    HandleRequest,
    OpenRequest,
    OperationStatusRequest,
    RetireCommandRequest,
    WriteRequest,
    NextBatchRequest,
);

impl SupervisorRequest for HealthRequest {
    fn has_handle(&self) -> bool {
        false
    }
}

impl SupervisorRequest for FinishFallbackRequest {
    fn has_handle(&self) -> bool {
        false
    }
}

impl SupervisorRequest for Value {
    fn has_handle(&self) -> bool {
        self.as_object()
            .is_some_and(|object| object.contains_key("handle"))
    }
}

impl<R: Serialize> Serialize for RequestEnvelope<R> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(Serialize)]
        struct Fields<'a, R> {
            #[serde(flatten)]
            request: &'a R,
            action: &'a str,
            #[serde(rename = "requestId")]
            request_id: u64,
        }
        Fields {
            request: &self.request,
            action: &self.action,
            request_id: self.request_id,
        }
        .serialize(serializer)
    }
}

struct BoundedFrameWriter {
    bytes: Vec<u8>,
}

impl Write for BoundedFrameWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.bytes.len().saturating_add(bytes.len()) + 1 > crate::transport::MAX_FRAME_BYTES {
            return Err(io::Error::other("supervisor request exceeds frame limit"));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(crate) fn request_frame<R: SupervisorRequest>(
    action: SupervisorAction,
    request_id: u64,
    request: R,
) -> Result<Vec<u8>, SupervisorError> {
    if !matches!(
        action,
        SupervisorAction::Health
            | SupervisorAction::FinishFallback
            | SupervisorAction::AdminCloseHandle
    ) && !request.has_handle()
    {
        return Err(SupervisorError::InvalidFrame(format!(
            "{} request requires handle",
            action
        )));
    }
    let mut writer = BoundedFrameWriter {
        bytes: Vec::with_capacity(256),
    };
    let envelope = RequestEnvelope {
        request,
        action: action.to_string(),
        request_id,
    };
    serde_json::to_writer(&mut writer, &envelope).map_err(|error| {
        if error.is_io() {
            SupervisorError::InvalidFrame("request exceeds 1 MiB".into())
        } else {
            SupervisorError::Json(error)
        }
    })?;
    writer.bytes.push(b'\n');
    Ok(writer.bytes)
}

#[derive(Deserialize)]
struct ResponseEnvelope {
    #[serde(rename = "requestId")]
    request_id: Option<Box<RawValue>>,
    error: Option<Box<RawValue>>,
    result: Option<Box<RawValue>>,
}

pub(crate) fn response_value<O: for<'de> Deserialize<'de>>(
    action: SupervisorAction,
    request_id: u64,
    response: &str,
) -> Result<O, SupervisorError> {
    let response: ResponseEnvelope = match serde_json::from_str(response) {
        Ok(response) => response,
        Err(_) => return Err(SupervisorError::UnknownOutcome { action, request_id }),
    };
    if response
        .request_id
        .as_deref()
        .and_then(|raw| serde_json::from_str::<u64>(raw.get()).ok())
        != Some(request_id)
    {
        return Err(SupervisorError::UnknownOutcome { action, request_id });
    }
    if let Some(message) = response
        .error
        .as_deref()
        .and_then(|raw| serde_json::from_str::<String>(raw.get()).ok())
    {
        return Err(SupervisorError::Remote { action, message });
    }
    match response.result {
        Some(result) => serde_json::from_str(result.get()),
        None => serde_json::from_str("{}"),
    }
    .map_err(|_| SupervisorError::UnknownOutcome { action, request_id })
}

pub(crate) fn serialize_frame(value: &impl Serialize) -> Result<Vec<u8>, SupervisorError> {
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
