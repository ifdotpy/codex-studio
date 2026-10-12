use std::io::BufReader;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use crate::protocol::*;
use crate::transport::{SupervisorTransport, read_frame, write_frame};

const PROTOCOL_VERSION: u64 = 1;
const IO_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

pub struct SupervisorClient<T: SupervisorTransport> {
    stream: BufReader<T>,
    next_request_id: u64,
    backend_identity: BackendIdentity,
}

impl<T: SupervisorTransport> SupervisorClient<T> {
    /// Perform the protocol-v1 identity handshake over an already-connected
    /// transport. The transport is closed by dropping the client.
    pub fn from_transport(
        transport: T,
        identity: BackendIdentity,
    ) -> Result<Self, SupervisorError> {
        Self::from_transport_with_operator(transport, identity, false)
    }

    fn from_transport_with_operator(
        mut transport: T,
        identity: BackendIdentity,
        operator: bool,
    ) -> Result<Self, SupervisorError> {
        let mut handshake = serde_json::json!({
            "protocol": PROTOCOL_VERSION,
            "stateDir": identity.state_dir,
            "backendId": identity.backend_id,
        });
        if operator && let Some(object) = handshake.as_object_mut() {
            object.remove("backendId");
            object.insert("operator".into(), Value::Bool(true));
        }
        write_frame(&mut transport, &handshake)?;
        let mut stream = BufReader::new(transport);
        let response = read_frame(&mut stream)?.ok_or(SupervisorError::ConnectionLost)?;
        let value: Value = serde_json::from_str(&response)?;
        if value.get("ok") == Some(&Value::Bool(true)) {
            Ok(Self {
                stream,
                next_request_id: 0,
                backend_identity: identity,
            })
        } else {
            let detail = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("unexpected handshake response")
                .to_owned();
            Err(SupervisorError::ProtocolMismatch {
                expected: PROTOCOL_VERSION,
                received: None,
                detail,
            })
        }
    }

    pub fn backend_identity(&self) -> &BackendIdentity {
        &self.backend_identity
    }

    pub fn health(&mut self, request: HealthRequest) -> Result<HealthResult, SupervisorError> {
        self.call(SupervisorAction::Health, request)
    }

    pub fn open(&mut self, request: OpenRequest) -> Result<OpenResult, SupervisorError> {
        self.call(SupervisorAction::Open, request)
    }

    pub fn next(&mut self, request: CursorRequest) -> Result<NextResult, SupervisorError> {
        self.call(SupervisorAction::Next, request)
    }

    pub fn finish_fallback(
        &mut self,
        request: FinishFallbackRequest,
    ) -> Result<FinishedFallback, SupervisorError> {
        self.call(SupervisorAction::FinishFallback, request)
    }

    pub fn admin_close_handle(
        &mut self,
        request: AdminCloseHandleRequest,
    ) -> Result<AdminCloseHandleResult, SupervisorError> {
        self.call(SupervisorAction::AdminCloseHandle, request)
    }

    pub fn retire_command(
        &mut self,
        request: RetireCommandRequest,
    ) -> Result<RetireCommandResult, SupervisorError> {
        self.call(SupervisorAction::RetireCommand, request)
    }

    pub fn write(&mut self, request: WriteRequest) -> Result<WriteResult, SupervisorError> {
        self.call(SupervisorAction::Write, request)
    }

    pub fn operation_status(
        &mut self,
        request: OperationStatusRequest,
    ) -> Result<OperationStatusResult, SupervisorError> {
        self.call(SupervisorAction::OperationStatus, request)
    }

    pub fn ack(&mut self, request: AckRequest) -> Result<AckResult, SupervisorError> {
        self.call(SupervisorAction::Ack, request)
    }

    pub fn replay(&mut self, request: CursorRequest) -> Result<ReplayResult, SupervisorError> {
        self.call(SupervisorAction::Replay, request)
    }

    pub fn status(&mut self, request: HandleRequest) -> Result<StatusResult, SupervisorError> {
        self.call(SupervisorAction::Status, request)
    }

    pub fn detach(&mut self, request: HandleRequest) -> Result<DetachedResult, SupervisorError> {
        self.call(SupervisorAction::Detach, request)
    }

    fn call<R: Serialize, O: for<'de> Deserialize<'de>>(
        &mut self,
        action: SupervisorAction,
        request: R,
    ) -> Result<O, SupervisorError> {
        self.next_request_id = self.next_request_id.checked_add(1).ok_or_else(|| {
            SupervisorError::InvalidFrame("request identity space exhausted".into())
        })?;
        let request_id = self.next_request_id;
        let mut value = serde_json::to_value(request)?;
        let object = value
            .as_object_mut()
            .ok_or_else(|| SupervisorError::InvalidFrame("request must be an object".into()))?;
        object.insert("requestId".into(), Value::from(request_id));
        object.insert("action".into(), Value::from(action.wire_name()));
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
        if let Err(error) = write_frame(self.stream.get_mut(), &value) {
            return match error {
                SupervisorError::Io(_) => {
                    Err(SupervisorError::UnknownOutcome { action, request_id })
                }
                other => Err(other),
            };
        }
        let response = match read_frame(&mut self.stream) {
            Ok(Some(line)) => line,
            Ok(None) | Err(SupervisorError::Io(_)) => {
                return Err(SupervisorError::UnknownOutcome { action, request_id });
            }
            Err(_) => return Err(SupervisorError::UnknownOutcome { action, request_id }),
        };
        let response: Value = match serde_json::from_str(&response) {
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
}

#[cfg(unix)]
impl SupervisorClient<std::os::unix::net::UnixStream> {
    /// Connect to the supervisor socket in a state directory or to a socket
    /// path directly. The caller owns path validation before connecting.
    pub fn connect(
        path: impl AsRef<Path>,
        identity: BackendIdentity,
    ) -> Result<Self, SupervisorError> {
        let path = path.as_ref();
        let endpoint = if path.is_dir() {
            path.join("supervisor.sock")
        } else {
            path.to_path_buf()
        };
        let stream = std::os::unix::net::UnixStream::connect(endpoint)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Self::from_transport(stream, identity)
    }

    pub fn connect_operator(
        path: impl AsRef<Path>,
        state_dir: PathBuf,
    ) -> Result<Self, SupervisorError> {
        let path = path.as_ref();
        let endpoint = if path.is_dir() {
            path.join("supervisor.sock")
        } else {
            path.to_path_buf()
        };
        let stream = std::os::unix::net::UnixStream::connect(endpoint)?;
        stream.set_read_timeout(Some(IO_TIMEOUT))?;
        stream.set_write_timeout(Some(IO_TIMEOUT))?;
        Self::from_transport_with_operator(
            stream,
            BackendIdentity {
                backend_id: BackendId(String::new()),
                state_dir,
            },
            true,
        )
    }
}

#[cfg(windows)]
impl SupervisorClient<std::fs::File> {
    /// Windows named-pipe support is intentionally deferred until the supported
    /// server entry points are inventoried and a transport is approved.
    pub fn connect(
        _path: impl AsRef<Path>,
        _identity: BackendIdentity,
    ) -> Result<Self, SupervisorError> {
        Err(SupervisorError::UnsupportedTransport("Windows"))
    }
}
