//! One bounded, short-lived provider session shared by one-off consumers.

use std::{
    collections::BTreeMap,
    future::Future,
    path::Path,
    pin::Pin,
    sync::{Arc, LazyLock},
    time::Duration,
};

use studio_runtime_process::{
    Generation, OperationId, ProviderHandle, ProviderLaunchSpec,
    provider_seam::{OpenOptions, ProviderTaskFactory, SessionClock, Shutdown},
};
use tokio::time::Instant;

use crate::provider_session_task::{SessionHandle, SessionTaskConfig, SessionTaskError};

const MAX_FRAME_BYTES: usize = 2 * 1024 * 1024;
const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;
pub(crate) const MAX_CHILDREN: usize = 4;
static ADMISSION: LazyLock<AdmissionPool> = LazyLock::new(|| AdmissionPool::new(MAX_CHILDREN));

/// Shared admission limit for every one-off provider child, including model refreshes.
pub(crate) struct AdmissionPool(Arc<tokio::sync::Semaphore>);

impl AdmissionPool {
    pub(crate) fn new(limit: usize) -> Self {
        Self(Arc::new(tokio::sync::Semaphore::new(limit)))
    }

    pub(crate) fn try_admit(&self) -> Option<ExchangeAdmission> {
        self.0
            .clone()
            .try_acquire_owned()
            .ok()
            .map(|permit| ExchangeAdmission { _permit: permit })
    }
}

pub(crate) struct ExchangeAdmission {
    _permit: tokio::sync::OwnedSemaphorePermit,
}

pub(crate) fn try_admit() -> Option<ExchangeAdmission> {
    ADMISSION.try_admit()
}

pub(crate) type ExchangeFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, ()>> + Send + 'a>>;

pub(crate) struct ExchangeSpec {
    pub cleanup_report: Option<Arc<std::sync::Mutex<Option<String>>>>,
    pub timeout: Duration,
    pub shutdown: Shutdown,
    pub initialize_operation: OperationId,
    pub initialize_params: serde_json::Value,
}

/// Launch one provider process, run one bounded consumer exchange, then reap it.
pub(crate) async fn run<T, F>(
    executable: Option<&Path>,
    home: &str,
    isolated: bool,
    spec: ExchangeSpec,
    exchange: F,
) -> Result<T, ()>
where
    T: Send,
    F: for<'a> FnOnce(&'a mut SessionHandle, Instant) -> ExchangeFuture<'a, T> + Send,
{
    let admission = try_admit().ok_or(())?;
    run_admitted(admission, executable, home, isolated, spec, exchange).await
}

pub(crate) async fn run_admitted<T, F>(
    _admission: ExchangeAdmission,
    executable: Option<&Path>,
    home: &str,
    isolated: bool,
    spec: ExchangeSpec,
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
                deadline: started + spec.timeout,
                max_frame_bytes: MAX_FRAME_BYTES,
                max_output_bytes: MAX_OUTPUT_BYTES,
            },
        );
        run_with_factory_admitted(_admission, factory, launch, started, spec, exchange).await
    }
    #[cfg(not(unix))]
    {
        let _ = (launch, spec, exchange);
        Err(())
    }
}

#[cfg(test)]
pub(crate) async fn run_with_factory<T, F>(
    factory: Arc<dyn ProviderTaskFactory>,
    launch: ProviderLaunchSpec,
    started: Instant,
    spec: ExchangeSpec,
    exchange: F,
) -> Result<T, ()>
where
    T: Send,
    F: for<'a> FnOnce(&'a mut SessionHandle, Instant) -> ExchangeFuture<'a, T> + Send,
{
    // Scripted sessions use an isolated pool so concurrently running tests cannot
    // consume the production process budget; the pool's bound has its own burst test.
    let admission = AdmissionPool::new(MAX_CHILDREN).try_admit().ok_or(())?;
    run_with_factory_admitted(admission, factory, launch, started, spec, exchange).await
}

pub(crate) async fn run_with_factory_admitted<T, F>(
    _admission: ExchangeAdmission,
    factory: Arc<dyn ProviderTaskFactory>,
    launch: ProviderLaunchSpec,
    started: Instant,
    spec: ExchangeSpec,
    exchange: F,
) -> Result<T, ()>
where
    T: Send,
    F: for<'a> FnOnce(&'a mut SessionHandle, Instant) -> ExchangeFuture<'a, T> + Send,
{
    let deadline = started + spec.timeout;
    let mut stop = spec.shutdown.subscribe();
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
            timeout_ms: u64::try_from(spec.timeout.as_millis()).unwrap_or(u64::MAX),
            initialize_operation: spec.initialize_operation,
            request_id_prefix: None,
            recovered: vec![],
            initialize_params: spec.initialize_params,
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
    if let Some(report) = spec.cleanup_report {
        record_cleanup_failure(&report, closed.adapter.as_ref());
    }
    if !matches!(closed.result, Ok(()) | Err(SessionTaskError::Closed))
        || closed
            .adapter
            .is_none_or(|exit| exit.result.is_err() || exit.cleanup_error.is_some())
    {
        return Err(());
    }
    result
}

fn record_cleanup_failure(
    report: &std::sync::Mutex<Option<String>>,
    exit: Option<&studio_runtime_process::provider_seam::TaskExit>,
) {
    if let Some(error) = exit.and_then(|exit| exit.cleanup_error.as_ref()) {
        *report.lock().expect("provider cleanup report") = Some(error.clone());
    }
}

#[cfg(test)]
mod tests {
    use super::AdmissionPool;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };

    #[test]
    fn request_failure_does_not_become_a_catalog_cleanup_failure() {
        use studio_runtime_process::provider_seam::{AdapterError, TaskExit};
        let report = std::sync::Mutex::new(None);
        let request_failed = TaskExit {
            result: Err(AdapterError::Timeout),
            cleanup_error: None,
            recovery_needed: None,
        };
        super::record_cleanup_failure(&report, Some(&request_failed));
        assert!(report.lock().expect("report lock").is_none());
        let cleanup_failed = TaskExit {
            result: Err(AdapterError::Timeout),
            cleanup_error: Some("termination unconfirmed".into()),
            recovery_needed: None,
        };
        super::record_cleanup_failure(&report, Some(&cleanup_failed));
        assert_eq!(
            report.lock().expect("report lock").as_deref(),
            Some("termination unconfirmed")
        );
    }

    #[tokio::test]
    async fn burst_admission_never_exceeds_shared_child_bound() {
        let pool = Arc::new(AdmissionPool::new(super::MAX_CHILDREN));
        let active = Arc::new(AtomicUsize::new(0));
        let maximum = Arc::new(AtomicUsize::new(0));
        let (release, _) = tokio::sync::watch::channel(false);
        let barrier = Arc::new(tokio::sync::Barrier::new(super::MAX_CHILDREN + 1));
        let mut tasks = Vec::new();
        for _ in 0..64 {
            let pool = Arc::clone(&pool);
            let active = Arc::clone(&active);
            let maximum = Arc::clone(&maximum);
            let mut release = release.subscribe();
            let barrier = Arc::clone(&barrier);
            tasks.push(tokio::spawn(async move {
                let Some(_admission) = pool.try_admit() else {
                    return;
                };
                let count = active.fetch_add(1, Ordering::SeqCst) + 1;
                maximum.fetch_max(count, Ordering::SeqCst);
                barrier.wait().await;
                while !*release.borrow() {
                    if release.changed().await.is_err() {
                        break;
                    }
                }
                active.fetch_sub(1, Ordering::SeqCst);
            }));
        }
        barrier.wait().await;
        assert_eq!(maximum.load(Ordering::SeqCst), super::MAX_CHILDREN);
        release.send(true).expect("release admissions");
        for task in tasks {
            task.await.expect("admission task");
        }
    }
}
