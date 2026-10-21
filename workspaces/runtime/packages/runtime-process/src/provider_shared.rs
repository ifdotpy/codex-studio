//! Pure supervisor-v1 framing and retained-child launch policy shared by both adapters.
use crate::protocol::{
    AckRequest, AdminCloseHandleRequest, CursorRequest, FinishFallbackRequest, HandleRequest,
    HealthRequest, NativeId, NextBatchRequest, OpenRequest, OperationStatusRequest,
    RetireCommandRequest, WriteRequest,
};
use crate::supervisor_contract::PROCESS_SUPERVISOR_PROTOCOL_VERSION;
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
/// The variable a macOS Python launcher stub adds before it re-executes the
/// interpreter; the accepted launch environment does not contain it.
#[cfg(any(target_os = "linux", target_os = "macos"))]
pub(crate) const PYTHON_LAUNCHER_MARKER: &str = "__PYVENV_LAUNCHER__";
#[cfg(any(target_os = "linux", target_os = "macos"))]
const RETAINED_LAUNCHER_ENVIRONMENT: &[&str] = &[
    "PATH",
    "LANG",
    "LC_CTYPE",
    "LC_ALL",
    PYTHON_LAUNCHER_MARKER,
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

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

/// Upper bound in bytes for one retained child's launch environment record.
#[cfg(any(target_os = "linux", target_os = "macos"))]
const PROCESS_ENVIRONMENT_LIMIT_BYTES: usize = 4 * 1024 * 1024;

/// Read a retained child's launch environment from `/proc/<pid>/environ`.
#[cfg(target_os = "linux")]
pub(crate) fn read_process_environment(pid: u32) -> io::Result<BTreeMap<String, String>> {
    use std::io::Read;
    let mut bytes = Vec::new();
    std::fs::File::open(format!("/proc/{pid}/environ"))?
        .take(PROCESS_ENVIRONMENT_LIMIT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)?;
    if bytes.len() > PROCESS_ENVIRONMENT_LIMIT_BYTES {
        return Err(io::Error::other("environment exceeds limit"));
    }
    parse_process_environment(&bytes)
}

/// Read a retained child's launch environment from `KERN_PROCARGS2`, as the
/// Python supervisor's `process_launch_environment` does on macOS.
///
/// The record has no terminator between the environment and the Apple
/// strings. libSystem clears the first Apple strings during the child's
/// startup, so the parse is exact for a started child. A read in the first
/// moments after exec can include Apple strings, and the caller then refuses
/// the child as unverifiable.
#[cfg(target_os = "macos")]
pub(crate) fn read_process_environment(pid: u32) -> io::Result<BTreeMap<String, String>> {
    let raw = studio_runtime_proc_sys::process_arguments(pid, PROCESS_ENVIRONMENT_LIMIT_BYTES)?;
    parse_process_environment(procargs_environment(&raw)?)
}

/// Skip `argc`, the executable path, its NUL padding and `argc` argument
/// strings of a `KERN_PROCARGS2` record. The environment strings follow; an
/// empty string ends them before the Apple strings.
#[cfg(target_os = "macos")]
fn procargs_environment(raw: &[u8]) -> io::Result<&[u8]> {
    const MAX_ARGUMENTS: i32 = 4096;
    let invalid = || io::Error::new(io::ErrorKind::InvalidData, "invalid process launch record");
    let (count, rest) = raw.split_first_chunk::<4>().ok_or_else(invalid)?;
    let count = i32::from_ne_bytes(*count);
    if !(1..=MAX_ARGUMENTS).contains(&count) {
        return Err(invalid());
    }
    let skip_string = |bytes: &[u8]| -> io::Result<usize> {
        bytes
            .iter()
            .position(|byte| *byte == 0)
            .map(|end| end + 1)
            .ok_or_else(invalid)
    };
    let mut position = skip_string(rest)?;
    while rest.get(position) == Some(&0) {
        position += 1;
    }
    for _ in 0..count {
        position += skip_string(rest.get(position..).ok_or_else(invalid)?)?;
    }
    rest.get(position..).ok_or_else(invalid)
}

#[cfg(any(target_os = "linux", target_os = "macos"))]
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

/// The `nativeId` of a Studio write: the id of a request with an integer id, which the
/// supervisor renumbers. A request with a string id (Claude connections use `claude:<hex>`)
/// and a reply to a provider request (the bridge uses `claude:<uuid>`) send none. Their
/// receipts are found by operation id, and the provider sees their ids unchanged.
pub(crate) fn provider_native_id(message: &Value) -> Option<NativeId> {
    message
        .get("method")
        .and_then(Value::as_str)
        .and_then(|_| message.get("id"))
        .and_then(Value::as_u64)
        .and_then(|id| i64::try_from(id).ok())
        .map(NativeId::Integer)
}

/// The supervisor RPC id of an accepted Studio request: a positive integer.
pub(crate) fn rpc_remote_id(remote: Option<&NativeId>) -> Option<u64> {
    remote
        .and_then(NativeId::rpc)
        .and_then(|id| u64::try_from(id).ok())
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

#[cfg(all(test, target_os = "macos"))]
mod macos_environment_tests {
    #![allow(clippy::unwrap_used)]
    use super::{procargs_environment, read_process_environment};
    use std::collections::BTreeMap;
    use std::process::{Command, Stdio};

    #[test]
    fn procargs_record_skips_path_padding_and_arguments() {
        let mut raw = 2_i32.to_ne_bytes().to_vec();
        raw.extend_from_slice(
            b"/bin/tool\0\0\0\0tool\0--flag\0A=1\0B=2\0\0executable_path=/bin/tool\0",
        );
        assert_eq!(
            procargs_environment(&raw).unwrap(),
            b"A=1\0B=2\0\0executable_path=/bin/tool\0"
        );
        assert!(procargs_environment(&0_i32.to_ne_bytes()).is_err());
        let mut truncated = 3_i32.to_ne_bytes().to_vec();
        truncated.extend_from_slice(b"/bin/tool\0tool\0");
        assert!(procargs_environment(&truncated).is_err());
    }

    /// Child half of the test below: report that its startup has finished,
    /// then block until the parent kills it.
    #[test]
    #[ignore = "runs only as the child of live_child_environment_is_read_exactly"]
    fn environment_probe_child() {
        if std::env::var_os("STUDIO_TEST_A").is_some() {
            std::io::Write::write_all(&mut std::io::stdout(), PROBE_READY).unwrap();
            std::io::Read::read(&mut std::io::stdin(), &mut [0]).unwrap();
        }
    }

    /// The line that the probe child writes after its startup.
    const PROBE_READY: &[u8] = b"environment-probe-ready\n";

    #[test]
    fn live_child_environment_is_read_exactly() {
        // macOS omits the environment of restricted platform binaries such as
        // /bin/sleep from KERN_PROCARGS2, so the probe is this test binary, as
        // a provider child is a third-party binary.
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "provider_shared::macos_environment_tests::environment_probe_child",
            ])
            .env_clear()
            .env("STUDIO_TEST_A", "1")
            .env("STUDIO_TEST_B", "two words=and equals")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        // A retained child has finished its startup. Before that, the record
        // can hold Apple strings directly after the environment with no empty
        // string between them; libSystem clears the first ones during startup.
        let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
        let mut line = Vec::new();
        while line != PROBE_READY {
            line.clear();
            let count = std::io::BufRead::read_until(&mut stdout, b'\n', &mut line).unwrap();
            assert_ne!(count, 0, "the probe child exited before it was ready");
        }
        let read = read_process_environment(child.id());
        child.kill().unwrap();
        child.wait().unwrap();
        assert_eq!(
            read.unwrap(),
            BTreeMap::from([
                ("STUDIO_TEST_A".to_owned(), "1".to_owned()),
                (
                    "STUDIO_TEST_B".to_owned(),
                    "two words=and equals".to_owned()
                ),
            ])
        );
    }
}
