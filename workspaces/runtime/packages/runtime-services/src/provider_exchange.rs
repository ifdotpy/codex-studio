//! One bounded, short-lived provider session shared by one-off consumers.

use std::{collections::BTreeMap, future::Future, path::Path, pin::Pin, sync::Arc, time::Duration};

use studio_runtime_process::{
    Generation, OperationId, ProviderHandle, ProviderLaunchSpec,
    provider_seam::{OpenOptions, ProviderTaskFactory, SessionClock, Shutdown},
};
use tokio::time::Instant;

use crate::provider_session_task::{SessionHandle, SessionTaskConfig, SessionTaskError};

const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

pub(crate) type ExchangeFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ()>> + Send + 'a>>;

/// Launch one provider process, run one bounded consumer exchange, then reap it.
pub(crate) async fn run<T, F>(
    executable: Option<&Path>,
    home: &str,
    isolated: bool,
    timeout: Duration,
    shutdown: Shutdown,
    initialize_operation: OperationId,
    initialize_params: serde_json::Value,
    exchange: F,
) -> Result<T, ()>
where
    T: Send,
    F: for<'a> FnOnce(&'a mut SessionHandle, Instant) -> ExchangeFuture<'a, T> + Send,
{
    let started = Instant::now();
    let executable = executable.unwrap_or_else(|| Path::new("codex"));
    let mut command = vec![
        executable.to_string_lossy().into_owned(),
        "app-server".into(),
        "--listen".into(),
        "stdio://".into(),
    ];
    let mut environment: BTreeMap<String, String> = std::env::vars().collect();
    environment.insert("CODEX_HOME".into(), home.to_owned());
    if isolated {
        command.extend(["-c".into(), "cli_auth_credentials_store=\"file\"".into()]);
        environment.remove("OPENAI_API_KEY");
        environment.remove("CODEX_API_KEY");
    }
    let launch = ProviderLaunchSpec::new(
        command,
        environment,
        std::env::current_dir().map_err(|_| ())?,
    );
    #[cfg(unix)]
    {
        let factory = Arc::new(
            studio_runtime_process::provider_stdio::StdioProviderTaskFactory {
                deadline: started + timeout,
                max_frame_bytes: MAX_FRAME_BYTES,
                max_output_bytes: MAX_OUTPUT_BYTES,
            },
        );
        run_with_factory(
            factory,
            launch,
            started,
            timeout,
            shutdown,
            initialize_operation,
            initialize_params,
            exchange,
        )
        .await
    }
    #[cfg(not(unix))]
    {
        let _ = (
            launch,
            shutdown,
            initialize_operation,
            initialize_params,
            exchange,
        );
        Err(())
    }
}

pub(crate) async fn run_with_factory<T, F>(
    factory: Arc<dyn ProviderTaskFactory>,
    launch: ProviderLaunchSpec,
    started: Instant,
    timeout: Duration,
    shutdown: Shutdown,
    initialize_operation: OperationId,
    initialize_params: serde_json::Value,
    exchange: F,
) -> Result<T, ()>
where
    T: Send,
    F: for<'a> FnOnce(&'a mut SessionHandle, Instant) -> ExchangeFuture<'a, T> + Send,
{
    let deadline = started + timeout;
    let mut stop = shutdown.subscribe();
    if *stop.borrow() || Instant::now() >= deadline {
        return Err(());
    }
    let mut session = SessionHandle::open(
        factory,
        ProviderHandle::for_account("one-off-provider"),
        OpenOptions {
            clock: SessionClock::from_origin(started),
            launch,
            expected_generation: Some(Generation(1)),
        },
        SessionTaskConfig {
            generation: Generation(1),
            claude: false,
            timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
            initialize_operation,
            request_id_prefix: None,
            recovered: vec![],
            initialize_params,
        },
    )
    .await
    .map_err(|_| ())?;
    let result = tokio::select! {
        biased;
        _ = Shutdown::cancelled(&mut stop) => Err(()),
        result = tokio::time::timeout_at(deadline, exchange(&mut session, started)) => {
            result.unwrap_or(Err(()))
        }
    };
    let closed = session.shutdown().await;
    if !matches!(closed.result, Ok(()) | Err(SessionTaskError::Closed))
        || closed.adapter.is_none_or(|exit| exit.result.is_err())
    {
        return Err(());
    }
    result
}
