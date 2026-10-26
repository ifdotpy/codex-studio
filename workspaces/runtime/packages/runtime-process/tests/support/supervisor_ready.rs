use std::io;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::process::Child;
use std::time::{Duration, Instant};

use studio_runtime_process::{BackendIdentity, SupervisorClient};

// A hang guard, never a delay or a condition for success.
pub const HANG_GUARD: Duration = Duration::from_secs(120);

pub fn wait(child: &mut Child, identity: BackendIdentity, stderr: &Path) {
    let socket = identity.state_dir.join("supervisor.sock");
    let deadline = Instant::now() + HANG_GUARD;
    let result = loop {
        match child.try_wait() {
            Ok(None) => {}
            status => break Err(format!("supervisor exited before readiness: {status:?}")),
        }
        if Instant::now() >= deadline {
            break Err(format!("supervisor did not listen within {HANG_GUARD:?}"));
        }
        match UnixStream::connect(&socket) {
            Ok(stream) => {
                let remaining = deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    break Err("readiness deadline expired before handshake".into());
                }
                stream
                    .set_read_timeout(Some(remaining))
                    .expect("read timeout");
                stream
                    .set_write_timeout(Some(remaining))
                    .expect("write timeout");
                // Existence alone precedes listen/chmod/serve_forever. A protocol
                // reply proves initialization completed, for Python and Rust alike.
                break SupervisorClient::from_transport(stream, identity)
                    .map(drop)
                    .map_err(|error| format!("supervisor readiness handshake failed: {error}"));
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::NotFound | io::ErrorKind::ConnectionRefused
                ) =>
            {
                // Condition polling yields the CPU to the child; no fixed startup sleep.
                std::thread::park_timeout(Duration::from_millis(10));
            }
            Err(error) => break Err(format!("supervisor readiness connect failed: {error}")),
        }
    };
    if let Err(error) = result {
        panic!(
            "{error}; socket: {}; child status: {:?}; stderr:\n{}",
            socket.display(),
            child.try_wait(),
            std::fs::read_to_string(stderr).unwrap_or_else(|error| error.to_string())
        );
    }
}
