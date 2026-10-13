use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::provider_shared::{
    handshake_request, request_value, response_value, validate_handshake,
};
use serde::{Deserialize, Serialize};

use crate::protocol::*;
use crate::transport::{SupervisorTransport, read_frame, write_frame};

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
        let handshake = handshake_request(&identity, operator);
        write_frame(&mut transport, &handshake)?;
        let mut stream = BufReader::new(transport);
        let response = read_frame(&mut stream)?.ok_or(SupervisorError::ConnectionLost)?;
        validate_handshake(&response)?;
        Ok(Self {
            stream,
            next_request_id: 0,
            backend_identity: identity,
        })
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

    /// Return the first committed journal event after the cursor. Sequence is
    /// append order; it does not imply order between stdout and stderr readers.
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

    /// Return retained events in journal sequence order. The sequence records
    /// append order, not a guaranteed cross-stream source order.
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
        let value = request_value(action, request_id, request)?;
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
        response_value(action, request_id, &response)
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

    /// Return a handle that can interrupt an in-flight request without taking
    /// the client mutex that owns this stream.
    pub fn cancellation_handle(&self) -> std::io::Result<std::sync::Arc<dyn Fn() + Send + Sync>> {
        use std::net::Shutdown;
        let stream = self.stream.get_ref().try_clone()?;
        Ok(std::sync::Arc::new(move || {
            let _ignored = stream.shutdown(Shutdown::Both);
        }))
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
