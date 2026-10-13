//! Close intake before workers, then one-off children and retained service clients.
//! The caller drops HTTP/store references last, preserving the state lock throughout.
use crate::host_runtime::HostRuntime;
use studio_runtime_http::HttpState;

pub(crate) async fn close(
    runtime: HostRuntime,
    state: &HttpState,
) -> Result<(), Box<dyn std::error::Error>> {
    runtime.stop_intake();
    let workers = runtime.shutdown().await;
    // Always reap other services, even when a worker reports unknown cleanup.
    let services = state.shutdown_services().await;
    workers?;
    services?;
    Ok(())
}
