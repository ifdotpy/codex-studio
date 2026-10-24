//! Close intake before workers, then one-off children and retained service clients.
//! The caller drops HTTP/store references last, preserving the state lock throughout.
use crate::host_runtime::HostRuntime;
use std::time::Duration;
use studio_runtime_http::HttpState;
use tokio::time::timeout;

const SHUTDOWN_COMPONENT_WAIT: Duration = Duration::from_secs(5);
// The supervisor detach must finish, and report, inside this bound.
const _: () = assert!(
    studio_runtime_process::provider_task::SUPERVISOR_DETACH_TIMEOUT.as_millis()
        < SHUTDOWN_COMPONENT_WAIT.as_millis()
);
const SHUTDOWN_SERVICES_WAIT: Duration = Duration::from_secs(12);

pub(crate) async fn close(
    runtime: HostRuntime,
    state: &HttpState,
) -> Result<(), Box<dyn std::error::Error>> {
    runtime.stop_intake();
    eprintln!("runtime server shutdown step started: account registry and workers");
    let workers = timeout(SHUTDOWN_COMPONENT_WAIT, runtime.shutdown())
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "account registry shutdown exceeded its {:.0}s bound",
                    SHUTDOWN_COMPONENT_WAIT.as_secs_f64()
                ),
            )
        });
    // Always reap other services, even when a worker reports unknown cleanup.
    eprintln!("runtime server shutdown step started: retained HTTP services");
    let services = timeout(SHUTDOWN_SERVICES_WAIT, state.shutdown_services())
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "retained HTTP services shutdown exceeded its {:.0}s bound",
                    SHUTDOWN_SERVICES_WAIT.as_secs_f64()
                ),
            )
        });
    workers??;
    services??;
    Ok(())
}
