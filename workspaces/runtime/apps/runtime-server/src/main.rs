#[cfg(unix)]
use studio_runtime_server::account_registry;
#[cfg(unix)]
mod claude_login;
#[cfg(unix)]
mod history_runtime;
#[cfg(unix)]
use studio_runtime_server::host_runtime;
#[cfg(unix)]
mod io_runtime;
mod monitor_port;
mod multi_server_discovery;
mod multi_server_owner;
mod native_probe;
mod operator_cli;
#[cfg(unix)]
mod peer_link;
use studio_runtime_server::provider_ports;
#[cfg(unix)]
mod runtime_shutdown;
mod web_assets;
use std::collections::BTreeMap;
use std::env;
use std::fs::{self, File};
use std::future::Future;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;
use studio_runtime_store::path_identity::CanonicalPath;

use axum::Extension;
use rusqlite::OptionalExtension;
use studio_runtime_http::{HttpState, VaultJobOwner, router};
use studio_runtime_services::account_driver::{
    AccountDriver, create_default_registry, resolve_codex_home,
};
use studio_runtime_services::account_policy::AccountPolicyState;
use studio_runtime_services::account_session_runtime::AccountSessionPort;
use studio_runtime_services::diagnostics::log_stderr;
use studio_runtime_store::{ObservedTable, RuntimeStore, account_state};
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};

const SERVER_SHUTDOWN_GRACE: Duration = Duration::from_secs(20);
/// Default ceiling for Tokio's async executor workers, measured in threads.
const DEFAULT_ASYNC_EXECUTOR_MAX_WORKER_THREADS: usize = 4;
// Allow the worker and retained-service bounds to complete before process exit.
const SHUTDOWN_JOIN_GRACE: Duration = Duration::from_secs(20);
// Retry failed durable scheduler work after this bounded delay; commits can wake sooner.
const SCHEDULER_FAILURE_RETRY: Duration = Duration::from_secs(30);
const CONTINUATION_FAILURE_RETRY_SECONDS: f64 = 60.0;
/// Maximum simultaneous account limits reads in one continuation tick. Unit: accounts.
const CONTINUATION_LIMIT_READ_CONCURRENCY: usize = 4;
// Maximum agents staged per continuation tick, shared by all continuation kinds.
const CONTINUATION_TICK_AGENT_LIMIT: i64 = 32;
const AUTH_RESUME_PROOF_PENDING_PREDICATE: &str = "json_type(record,'$.authResumeProofPending')='object' AND json(json_extract(record,'$.authResumeProofPending')) <> '{}'";
const CURRENT_USAGE_RESUME_DUE_ROWS: &str = "SELECT resume.agent AS agent,CAST(json_extract(resume.record,'$.dueAt') AS REAL) AS due FROM runtime_usage_resumes AS resume JOIN runtime_agents AS owner ON owner.id=resume.agent AND json_extract(owner.record,'$.usageResume.id')=resume.id WHERE json_extract(resume.record,'$.status')='scheduled' AND json_type(resume.record,'$.dueAt') IN ('integer','real')";
const CURRENT_CAPACITY_RETRY_DUE_ROWS: &str = "SELECT retry.agent AS agent,CAST(json_extract(retry.record,'$.dueAt') AS REAL) AS due FROM runtime_capacity_retries AS retry JOIN runtime_agents AS owner ON owner.id=retry.agent AND json_extract(owner.record,'$.capacityRetry.id')=retry.id WHERE json_extract(retry.record,'$.status')='scheduled' AND json_type(retry.record,'$.dueAt') IN ('integer','real')";
const EXIT_ON_STDIN_CLOSE_ENV: &str = "CODEX_AGENTS_SERVER_EXIT_ON_STDIN_CLOSE";
const STARTUP_TRACE_ENV: &str = "CODEX_STUDIO_STARTUP_TRACE";

/// Durable inputs that can enqueue or unblock a server-to-server delivery.
const ORCHESTRATION_WAKE_TABLES: [ObservedTable; 3] = [
    ObservedTable::ServerOutbox,
    ObservedTable::AccessClients,
    ObservedTable::ServerLinks,
];
/// Durable inputs used by the federation snapshot, pairing and delivery queues.
const FEDERATION_WAKE_TABLES: [ObservedTable; 6] = [
    ObservedTable::FederationSettings,
    ObservedTable::FederationIdentity,
    ObservedTable::FederationPeers,
    ObservedTable::FederationInvites,
    ObservedTable::FederationRooms,
    ObservedTable::FederationOutbox,
];

fn observe_table_wakes(store: &RuntimeStore, tables: &[ObservedTable], wake: Arc<Notify>) {
    for table in tables {
        let wake = Arc::clone(&wake);
        store.add_table_change_observer(*table, move || wake.notify_one());
    }
}

fn startup_trace(checkpoint: &'static str) {
    if env::var_os(STARTUP_TRACE_ENV).as_deref() == Some(std::ffi::OsStr::new("1")) {
        log_stderr(format_args!(
            "runtime server startup checkpoint: {checkpoint}"
        ));
    }
}

fn async_executor_worker_threads(
    available_parallelism: Option<usize>,
    tokio_worker_threads_override_is_set: bool,
) -> Option<usize> {
    if tokio_worker_threads_override_is_set {
        None
    } else {
        Some(
            available_parallelism
                .unwrap_or(1)
                .clamp(1, DEFAULT_ASYNC_EXECUTOR_MAX_WORKER_THREADS),
        )
    }
}

fn main() -> std::process::ExitCode {
    // The launcher of agent sign-in links runs this binary (`browser_host::write_launcher`).
    #[cfg(unix)]
    if let Some(code) = studio_runtime_services::browser_host::shim_main(env::args_os()) {
        std::process::exit(code);
    }
    // The Linux VM helper in its own session (`linux_vm_runtime::TrampolineLauncher`).
    #[cfg(unix)]
    if let Some(code) = studio_runtime_services::linux_vm_runtime::helper_main(env::args_os()) {
        std::process::exit(code);
    }
    // The Mac host_exec service of a VM state folder (`host_exec_service::lifecycle`).
    #[cfg(unix)]
    if let Some(code) = studio_runtime_services::host_exec_service::main(env::args_os()) {
        std::process::exit(code);
    }
    if let Some(code) = operator_cli::dispatch() {
        return code;
    }
    std::panic::set_hook(Box::new(|panic| {
        log_stderr(format_args!("{}", panic_observed_log(panic.location())));
    }));
    let mut runtime_builder = tokio::runtime::Builder::new_multi_thread();
    if let Some(worker_threads) = async_executor_worker_threads(
        std::thread::available_parallelism().ok().map(usize::from),
        env::var_os("TOKIO_WORKER_THREADS").is_some(),
    ) {
        runtime_builder.worker_threads(worker_threads);
    }
    runtime_builder
        .max_blocking_threads(studio_runtime_process::blocking_pool::MAX_THREADS)
        .thread_keep_alive(studio_runtime_process::blocking_pool::THREAD_KEEP_ALIVE);
    let runtime = match runtime_builder.enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("{}", runtime_initialization_exit_log(&error.to_string()));
            std::process::exit(1);
        }
    };
    let (shutdown_requested, shutdown_finished) = match start_shutdown_watchdog() {
        Ok(watchdog) => watchdog,
        Err(error) => {
            eprintln!("runtime server could not start shutdown watchdog: {error}");
            std::process::exit(1);
        }
    };
    let result = runtime.block_on(run(shutdown_requested));
    // A spawn_blocking closure cannot be cancelled once it is running. Detach
    // any remainder after the owned shutdown steps have reached their bounds;
    // process exit below prevents those detached threads from delaying exit.
    eprintln!("runtime server stopping without waiting for uncancellable blocking-pool work");
    runtime.shutdown_timeout(Duration::ZERO);
    let _finished = shutdown_finished.send(());
    if let Err(error) = result {
        eprintln!("{}", error_exit_log(&error.to_string()));
        std::process::exit(1);
    }
    std::process::exit(0);
}

fn clear_owned_canvas_pid(state_dir: &Path, pid: u32) -> std::io::Result<bool> {
    let path = state_dir.join("canvas.pid");
    let contents = match fs::read_to_string(&path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error),
    };
    if contents.trim().parse::<u32>().ok() != Some(pid) {
        return Ok(false);
    }
    fs::remove_file(path)?;
    Ok(true)
}

/// The registry path and its bounded startup bytes (`None` when absent).
struct StartupRegistry {
    path: PathBuf,
    bytes: Option<Vec<u8>>,
}

/// Read the account registry for startup. Like Python `AccountStore`, a state
/// without a registry first gets the default Codex registry for `codex_home`.
fn startup_registry(
    state_root: &Path,
    codex_home: &Path,
) -> Result<StartupRegistry, Box<dyn std::error::Error>> {
    let registry_path = state_root.join("accounts/registry.json");
    create_default_registry(&registry_path, codex_home).map_err(|error| {
        format!(
            "Cannot create the account registry at {}: {error}",
            registry_path.display()
        )
    })?;
    let registry_file = match CanonicalPath::existing(&registry_path) {
        Ok(path) if !path.starts_with(state_root) => {
            return Err("Cannot read the account registry outside the state directory".into());
        }
        Ok(path) => Some(File::open(path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            return Err(format!(
                "Cannot read the account registry at {}",
                registry_path.display()
            )
            .into());
        }
    };
    let registry_bytes = match registry_file {
        Some(file) => {
            let mut bytes = Vec::new();
            file.take((account_state::MAX_PERSISTED_DOCUMENT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            Some(bytes)
        }
        None => None,
    };
    Ok(StartupRegistry {
        path: registry_path,
        bytes: registry_bytes,
    })
}

struct CanvasPidCleanup {
    state_dir: PathBuf,
    pid: u32,
}

impl Drop for CanvasPidCleanup {
    fn drop(&mut self) {
        if let Err(error) = clear_owned_canvas_pid(&self.state_dir, self.pid) {
            eprintln!("runtime server could not clear its canvas.pid: {error}");
        }
    }
}

fn start_shutdown_watchdog() -> std::io::Result<(Sender<()>, Sender<()>)> {
    let (requested_tx, requested_rx) = mpsc::channel();
    let (finished_tx, finished_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("runtime-shutdown-watchdog".into())
        .spawn(move || shutdown_watchdog(requested_rx, finished_rx))?;
    Ok((requested_tx, finished_tx))
}

fn shutdown_watchdog(requested: Receiver<()>, finished: Receiver<()>) {
    if requested.recv().is_err() {
        return;
    }
    if finished.recv_timeout(SERVER_SHUTDOWN_GRACE).is_err() {
        eprintln!(
            "runtime server exiting because shutdown exceeded its grace; last step: {}",
            shutdown_progress_label()
        );
        std::process::exit(1);
    }
}

static SHUTDOWN_PROGRESS: std::sync::OnceLock<Mutex<&'static str>> = std::sync::OnceLock::new();

fn shutdown_progress_label() -> &'static str {
    SHUTDOWN_PROGRESS
        .get_or_init(|| Mutex::new("HTTP graceful drain"))
        .lock()
        .map(|phase| *phase)
        .unwrap_or("unknown shutdown step")
}

async fn shutdown_join<T>(
    phase: &'static str,
    future: impl Future<Output = T>,
) -> Result<T, Box<dyn std::error::Error>> {
    if let Ok(mut progress) = SHUTDOWN_PROGRESS
        .get_or_init(|| Mutex::new("HTTP graceful drain"))
        .lock()
    {
        *progress = phase;
    }
    eprintln!("runtime server shutdown step started: {phase}");
    tokio::time::timeout(SHUTDOWN_JOIN_GRACE, future)
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "runtime server shutdown step exceeded its {:.0}s bound: {phase}",
                    SHUTDOWN_JOIN_GRACE.as_secs_f64()
                ),
            )
            .into()
        })
}

fn runtime_initialization_exit_log(reason: &str) -> String {
    format!("runtime server exiting because Tokio runtime initialization failed: {reason}")
}

fn error_exit_log(reason: &str) -> String {
    format!("runtime server exiting after error: {reason}")
}

fn panic_observed_log(location: Option<&std::panic::Location<'_>>) -> String {
    let location = location
        .map(|location| {
            let file = Path::new(location.file())
                .file_name()
                .and_then(|file| file.to_str())
                .unwrap_or("unknown source");
            format!(" at {file}:{}", location.line())
        })
        .unwrap_or_default();
    format!("runtime server observed panic{location} (payload redacted)")
}

/// The vault writer layr-desk runs: configure the VM writer from the configuration on standard input.
#[cfg(unix)]
fn vault_configure(args: &[String]) -> Result<(), Box<dyn std::error::Error>> {
    let [vm_state_dir, source] = args else {
        return Err("vault-configure requires a VM state directory and a project folder".into());
    };
    let given: serde_json::Value = serde_json::from_reader(std::io::stdin())?;
    let guest = studio_runtime_services::linux_vm_guest::UnixGuestRpc::new(vm_state_dir);
    let result =
        studio_runtime_services::vault::configure(&guest, Path::new(vm_state_dir), source, &given)?;
    println!("{}", serde_json::to_string_pretty(&result)?);
    Ok(())
}

async fn run(shutdown_requested: Sender<()>) -> Result<(), Box<dyn std::error::Error>> {
    studio_runtime_process::child_priority::ChildPriority::from_environment();
    studio_runtime_services::diagnostics::mark_startup_memory(
        studio_runtime_services::diagnostics::StartupMemoryMark::ProcessStarted,
    );
    let args = env::args().skip(1).collect::<Vec<_>>();
    #[cfg(unix)]
    if args.first().map(String::as_str) == Some(studio_runtime_services::vault::WRITER_SUBCOMMAND) {
        return vault_configure(&args[1..]);
    }
    if args.first().map(String::as_str) == Some("state-check") {
        let state_dir = args
            .get(1)
            .ok_or("state-check requires a state directory")?;
        if args.len() != 2 {
            return Err("state-check accepts exactly one state directory".into());
        }
        let report =
            studio_runtime_store::check_state_readonly(PathBuf::from(state_dir).as_path())?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    if args.first().map(String::as_str) == Some("mounts") {
        // Read-only `check`, or `repair` that only attaches at empty mount points.
        let (report, ok) = studio_runtime_services::image_workspace::mounts_command(&args[1..])?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        std::process::exit(if ok { 0 } else { 1 });
    }
    let port = parse_port(args.into_iter())?;
    studio_runtime_schema::initialize()?;
    startup_trace("backend-identity-started");
    let backend_build = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/backend-identity",
        studio_runtime_http::initialize_backend_identity,
    )
    .await??;
    startup_trace("backend-identity-completed");
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let address = listener.local_addr()?;
    startup_trace("listener-bound");
    let mut token = [0_u8; 32];
    getrandom::fill(&mut token).map_err(|error| std::io::Error::other(error.to_string()))?;
    let token = data_encoding::HEXLOWER.encode(&token);
    let configured_state_dir = env::var_os("CODEX_AGENTS_STATE_DIR").map(PathBuf::from);
    startup_trace("store-open-started");
    // Serving claims the state for Rust before the first write, whatever launched this process.
    let owner = studio_runtime_store::state_owner::StateOwnerClaim { backend_build };
    let store =
        Arc::new(RuntimeStore::open_claiming(configured_state_dir.as_deref(), &owner).await?);
    // Before the first runtime write: every writer transaction publishes the chats and
    // cards it changed.
    studio_runtime_services::entity_publication::install(&store).await?;
    store
        .set_diagnostics_callback(Some(
            studio_runtime_services::diagnostics::observe_store_event,
        ))
        .await?;
    startup_trace("store-open-completed");
    // This guard drops before `store`, so the runtime lock remains held while
    // the PID file is checked and removed on every post-open exit path.
    let _canvas_pid_cleanup = CanvasPidCleanup {
        state_dir: store.root().to_path_buf(),
        pid: std::process::id(),
    };
    studio_runtime_services::diagnostics::mark_startup_memory(
        studio_runtime_services::diagnostics::StartupMemoryMark::StoreOpened,
    );
    let startup_recovery_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| "system clock unavailable")?
        .as_secs_f64();
    studio_runtime_services::voice::provider::recover_after_restart(&store, startup_recovery_time)
        .await?;
    studio_runtime_services::maintenance::remove_review_assignments(&store).await?;
    let credentials =
        Arc::new(studio_runtime_services::federation_credentials::Credentials::default());
    #[cfg(unix)]
    {
        credentials.identity(Arc::clone(&store), None).await?;
    }
    studio_runtime_services::workspace_operations_effects::startup_recovery(&store).await?;
    studio_runtime_services::monitor_runtime::recover_on_startup(&store, startup_recovery_time)
        .await?;
    studio_runtime_services::provider_callbacks::recover_tool_requests_on_startup(
        &store,
        startup_recovery_time,
    )
    .await?;
    // Tasks that settled turns left running; a running turn keeps its tasks.
    // A failure leaves them running and does not stop the backend.
    match studio_runtime_services::task_settlement::recover_on_startup(
        &store,
        startup_recovery_time,
    )
    .await
    {
        Ok(0) => {}
        Ok(closed) => eprintln!("closed {closed} tasks of settled turns"),
        Err(error) => eprintln!("tasks of settled turns remain running: {error}"),
    }
    startup_trace("store-recovery-completed");
    // Report stranded or mismatched image workspace mounts; never repair them here.
    std::thread::Builder::new()
        .name("image-mount-check".to_owned())
        .spawn(studio_runtime_services::image_workspace::run_startup_mount_check)?;
    let (workspace_id, limits_read) = store
        .read_async("runtime-server/startup", |db| {
            Ok::<_, studio_runtime_store::StoreError>((
                studio_runtime_store::SyncDocumentsRepository::workspace_id(db)?,
                account_state::limits_snapshots(db)?,
            ))
        })
        .await?;
    let state_root = CanonicalPath::existing(store.root())?;
    // One disk gate watches state, worker workspace and build-output filesystems.
    studio_runtime_services::disk_space::install(
        state_root.clone().to_path_buf(),
        studio_runtime_services::disk_space::WorkerStorageConfig::from_environment()?,
    );
    let state_dir = state_root.to_string_lossy().into_owned();
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&state_dir));
    let base_home = resolve_codex_home(env::var_os("CODEX_HOME").as_deref(), &home)?;
    let StartupRegistry {
        path: registry_path,
        bytes: registry_bytes,
    } = startup_registry(&state_root, &base_home)?;
    let account_policy = Arc::new(
        AccountPolicyState::from_store(registry_bytes.as_deref(), limits_read).map_err(
            |error| {
                format!(
                    "Cannot read the account registry at {}: {error}",
                    registry_path.display()
                )
            },
        )?,
    );
    startup_trace("account-registry-open-started");
    let account_driver = Arc::new(AccountDriver::open(&registry_path)?);
    startup_trace("account-registry-open-completed");
    for diagnostic in account_policy.limits_diagnostics() {
        eprintln!(
            "invalid persisted limits at {}/{}/{} ({})",
            diagnostic.workspace_row,
            diagnostic.account_key,
            diagnostic.pointer,
            diagnostic.keyword
        );
    }
    let native_path = env::var("CODEX_BIN").ok();
    // The operator override, or `remote-access.json` in the state directory.
    let remote_access = Arc::new(
        studio_runtime_services::remote_access::RemoteAccess::from_environment(&state_root)?,
    );
    let public_origin = remote_access.origin();
    #[cfg(unix)]
    let peer_link_advert = Arc::new(std::sync::RwLock::new(None));
    let cache_root = env::var_os("XDG_CACHE_HOME")
        .or_else(|| env::var_os("CODEX_AGENTS_CACHE_DIR"))
        .or_else(|| {
            env::var_os("HOME").map(|home| PathBuf::from(home).join(".cache").into_os_string())
        })
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&state_dir).join("cache"));
    let native_runtime = Arc::new(studio_runtime_services::native_runtime::NativeRuntime::new(
        &state_root,
        Arc::new(native_probe::SystemCompatibilityProbe::new(
            tokio::runtime::Handle::current(),
        )),
    )?);
    let runtime_for_check = Arc::clone(&native_runtime);
    let discovery = native_discovery_inputs(native_path.as_deref());
    startup_trace("native-runtime-check-started");
    if let Some(platform) = native_platform() {
        match studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/main",
            move || runtime_for_check.check(discovery, platform),
        )
        .await
        {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => eprintln!("native runtime compatibility check failed: {error}"),
            Err(_) => eprintln!("native runtime compatibility check task failed"),
        }
    } else {
        eprintln!("native runtime compatibility check skipped: unsupported platform");
    }
    startup_trace("native-runtime-check-completed");
    let account_change_signal = account_driver.change_signal();
    let account_session_broker =
        account_registry::AccountSessionBroker::new_with_change_signal(account_change_signal);
    let continuation_wake = Arc::new(Notify::new());
    let retained_session: Arc<
        dyn studio_runtime_services::account_session_runtime::RetainedAccountSession,
    > = Arc::new(account_session_broker.clone());
    let account_session: Arc<
        dyn studio_runtime_services::account_session_runtime::AccountSessionPort,
    > = Arc::new(
        studio_runtime_services::account_session_runtime::AccountSessionRuntime::new(
            account_driver.clone(),
            Some(retained_session),
            tokio::runtime::Handle::current(),
            PathBuf::from(&state_dir).join("accounts"),
            base_home.clone(),
            home,
        )
        .with_limits(account_policy.clone())
        .with_store(Arc::clone(&store))
        .with_continuation_wake(Arc::clone(&continuation_wake)),
    );
    let session_binaries = Arc::new(Mutex::new(BTreeMap::<
        String,
        provider_ports::SessionExecutables,
    >::new()));
    let active_connections = Arc::new(provider_ports::LiveProviderConnections {
        accounts: account_driver.clone(),
        session: Arc::clone(&account_session),
        session_binaries: Arc::clone(&session_binaries),
        changes: account_session_broker.subscribe_changes(),
    });
    startup_trace("provider-version-monitor-started");
    let provider_versions =
        studio_runtime_services::provider_versions::ProviderVersionMonitor::start(
            studio_runtime_services::provider_versions::runtime_provider_version_store(
                store.clone(),
            ),
            Some(active_connections),
            cache_root.join("provider-upstream-releases-v1.json"),
            Arc::new(studio_runtime_services::provider_versions::SystemVersionProbe),
            Arc::new(studio_runtime_services::provider_versions::NpmReleaseFetcher),
            Arc::new(studio_runtime_services::provider_versions::SystemClock),
        )
        .await?;
    startup_trace("provider-version-monitor-completed");
    let multi_server_boundary = if let Some(origin) = public_origin.as_deref() {
        let identity = credentials
            .identity(Arc::clone(&store), Some(origin))
            .await?;
        let boundary = studio_runtime_services::multi_server_pairing::MultiServerBoundary::new(
            identity,
            Arc::new(multi_server_owner::TailscaleOwnerLookup),
        )?;
        let credentials = Arc::clone(&credentials);
        let origin = origin.to_owned();
        let identity_store = Arc::clone(&store);
        Some(boundary.with_identity_source(move |_| {
            let credentials = Arc::clone(&credentials);
            let store = Arc::clone(&identity_store);
            let origin = origin.clone();
            Box::pin(async move {
                credentials
                    .identity(store, Some(&origin))
                    .await
                    .map_err(|_| {
                        studio_runtime_services::multi_server_auth::refused(
                            503,
                            "identity_unavailable",
                            "The server identity is unavailable",
                        )
                    })
            })
        }))
    } else {
        None
    };
    let mut state = HttpState::new(
        token,
        workspace_id,
        state_dir.clone(),
        address.port(),
        public_origin.clone(),
        Some(store.clone()),
        Some(account_policy.clone()),
    )
    .with_remote_access(Arc::clone(&remote_access))
    .with_continuation_wake_handle(Arc::clone(&continuation_wake))
    .with_account_driver(account_driver.clone())
    .with_provider_versions(provider_versions)
    .with_credentials(credentials)
    .with_tailscale_whoami(Arc::new(|| {
        Box::pin(async {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/tailscale-whoami",
                || {
                    let raw = multi_server_owner::tailscale_json(&["whoami", "--json"]).ok();
                    let user = raw.as_ref().and_then(|value| {
                        value
                            .get("UserProfile")
                            .or_else(|| value.get("User"))
                            .and_then(|profile| {
                                profile.get("LoginName").or_else(|| profile.get("Name"))
                            })
                            .and_then(serde_json::Value::as_str)
                            .filter(|login| !login.trim().is_empty())
                            .map(|login| login.trim().to_ascii_lowercase())
                    });
                    let node = raw.as_ref().and_then(|value| {
                        value
                            .get("Self")
                            .or_else(|| value.get("Machine"))
                            .and_then(|self_node| self_node.get("ID"))
                            .cloned()
                    });
                    let owner_login = multi_server_owner::tailscale_json(&["status", "--json"])
                        .ok()
                        .and_then(|status| {
                            let local = status.get("Self")?;
                            let users = status.get("User")?.as_object()?;
                            let profile = users.get(&local.get("UserID")?.to_string())?;
                            if status
                                .get("BackendState")
                                .and_then(serde_json::Value::as_str)
                                != Some("Running")
                                || local
                                    .get("Tags")
                                    .and_then(serde_json::Value::as_array)
                                    .is_some_and(|tags| !tags.is_empty())
                            {
                                return None;
                            }
                            profile
                                .get("LoginName")
                                .and_then(serde_json::Value::as_str)
                                .filter(|login| !login.trim().is_empty() && login.len() <= 254)
                                .map(|login| login.trim().to_ascii_lowercase())
                        });
                    Some(serde_json::json!({
                        "status": if user.is_some() { "verified" } else { "unavailable" },
                        "user": user,
                        "node": node,
                        "ownerLogin": owner_login,
                    }))
                },
            )
            .await
            .ok()
            .flatten()
        })
    }));
    {
        state = state.with_server_discovery(Arc::new(
            multi_server_discovery::TailscaleServerDiscovery::new(PathBuf::from(&state_dir)),
        ));
    }
    if let Some(boundary) = multi_server_boundary {
        state = state.with_multi_server_boundary(Arc::new(boundary));
    }
    #[cfg(unix)]
    {
        state = state.with_peer_link_advert(Arc::clone(&peer_link_advert));
    }
    #[cfg(unix)]
    {
        let guest = studio_runtime_services::linux_vm_guest::UnixGuestRpc::for_runtime_state(
            &PathBuf::from(&state_dir),
        );
        state = state.with_project_secrets_guest_rpc(Arc::new(guest));
    }
    account_driver.recover_codex_logins()?;
    state = state.with_native_runtime(Arc::clone(&native_runtime));
    state.account_session = Some(Arc::clone(&account_session));
    state = state
        .with_title_request_port(Arc::new(provider_ports::ProviderTitleRequestPort::new(
            Arc::clone(&account_session),
        )))
        .with_diagnostics_provider(Arc::new(provider_ports::ProviderDiagnostics {
            accounts: account_driver.clone(),
            session: Arc::clone(&account_session),
            store: Arc::clone(&store),
        }))
        .with_diagnostics_supervisor(Arc::new(provider_ports::SupervisorDiagnostics {
            mode: env::var("CODEX_AGENTS_SUPERVISOR_MODE").is_ok_and(|value| value == "1"),
            fallback: env::var("CODEX_AGENTS_SUPERVISOR_FALLBACK")
                .is_ok_and(|value| value == "1"),
            notice: env::var("CODEX_AGENTS_SUPERVISOR_FALLBACK")
                .is_ok_and(|value| value == "1")
                .then(|| "The process supervisor stopped unexpectedly. Studio applied normal recovery to turns, monitors, and terminals with reduced restart protection; accepted or uncertain operations were not resubmitted.".to_owned()),
            state_dir: PathBuf::from(&state_dir),
        }));
    #[cfg(unix)]
    startup_trace("io-runtime-started");
    #[cfg(unix)]
    let io_runtime = Arc::new(io_runtime::IoRuntime::start(store.clone()).await?);
    #[cfg(unix)]
    startup_trace("io-runtime-completed");
    #[cfg(unix)]
    {
        let runtime = Arc::clone(&io_runtime);
        state = state
            .with_terminal_driver(io_runtime.terminal_driver())
            .with_terminal_write_gate(Arc::new(move || {
                let runtime = Arc::clone(&runtime);
                Box::pin(async move { runtime.reconcile_before_write().await })
            }));
    }
    #[cfg(unix)]
    startup_trace("claude-login-attach-started");
    #[cfg(unix)]
    claude_login::attach(
        &mut state,
        PathBuf::from(&state_dir),
        store.clone(),
        account_driver.clone(),
    )
    .await?;
    startup_trace("provider-and-io-setup-completed");
    let registry_wake = state.event_wake_handle();
    studio_runtime_services::image_workspace_agents::install_wake(Arc::clone(&registry_wake));
    // Python `resume_read_only_image_bases`: an image agent still in its
    // read-only phase gets its base build and copy again after a restart.
    #[cfg(target_os = "macos")]
    match studio_runtime_services::image_workspace_agents::resume_read_only(&store).await {
        Ok(0) => {}
        Ok(count) => eprintln!("resumed the image workspace of {count} read-only agent(s)"),
        Err(error) => eprintln!("image workspace resume failed: {error}"),
    }
    let orchestration_wake = Arc::new(Notify::new());
    let federation_wake = Arc::new(Notify::new());
    observe_table_wakes(
        &store,
        &ORCHESTRATION_WAKE_TABLES,
        Arc::clone(&orchestration_wake),
    );
    observe_table_wakes(
        &store,
        &FEDERATION_WAKE_TABLES,
        Arc::clone(&federation_wake),
    );
    let continuation_scheduler_wake = state.continuation_wake_handle();
    let monitor_port = monitor_port::RetainedMonitorPort::new(
        Arc::clone(&store),
        state
            .account_session
            .clone()
            .ok_or("account session runtime is unavailable")?,
    );
    let monitor_port_trait: Arc<dyn studio_runtime_services::monitor_runtime_wiring::MonitorPort> =
        monitor_port.clone();
    let monitor_runtime = Arc::new(
        studio_runtime_services::monitor_runtime_wiring::MonitorRuntime::new(
            Arc::clone(&store),
            monitor_port_trait,
            Arc::clone(&registry_wake),
        ),
    );
    monitor_port.set_dispatch_wake(monitor_runtime.dispatch_wake_handle())?;
    state = state.open_app_server_log();
    let lifecycle_ports: Arc<dyn studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts> =
        Arc::new(
            studio_runtime_services::lifecycle_effects::ProductionLifecyclePorts::new(
                Arc::clone(&store),
                monitor_runtime.dispatch_wake_handle(),
                Arc::clone(&registry_wake),
                Arc::clone(&account_session),
                state.voice_request_port_handle(),
            )
            .with_continuation_wake(Arc::clone(&continuation_scheduler_wake)),
        );
    let creation_environment =
        studio_runtime_http::routes::managed_creation_environment(Arc::clone(&account_driver), {
            let catalog = state.model_catalog_handle();
            catalog.set_account_session(Some(Arc::clone(&account_session)));
            catalog
        });
    // Python `model_work`: task claim, submit and accept use the `/api/work` owner.
    let work_action = studio_runtime_http::routes::managed_work_port(
        state.clone(),
        Some(Arc::clone(&monitor_runtime)),
    );
    let managed_tool_runtime =
        studio_runtime_services::provider_callbacks::ManagedToolRuntime::with_creation_and_work(
            Arc::clone(&monitor_runtime),
            Arc::clone(&lifecycle_ports),
            creation_environment,
            work_action,
        );
    #[cfg(unix)]
    startup_trace("host-runtime-started");
    #[cfg(unix)]
    let runtime = match host_runtime::HostRuntime::start_with_monitor_wake_and_native_runtime(
        &mut state,
        Arc::clone(&store),
        Arc::clone(&account_driver),
        native_path,
        monitor_runtime.dispatch_wake_handle(),
        account_session_broker.clone(),
        Some(Arc::clone(&native_runtime)),
        Arc::clone(&session_binaries),
        account_registry::AccountRegistryConfig::default(),
        Some(Arc::clone(&managed_tool_runtime)),
        Some(base_home.clone()),
    )
    .await
    {
        Ok(runtime) => runtime,
        Err(error) => {
            let terminal_cleanup = shutdown_io_runtime(Arc::clone(&io_runtime)).await;
            let services_cleanup = state.shutdown_services().await;
            terminal_cleanup?;
            services_cleanup?;
            return Err(error.into());
        }
    };
    #[cfg(unix)]
    startup_trace("host-runtime-completed");
    monitor_port.set_preparation_resources(
        runtime
            .preparation_resources()
            .ok_or("monitor preparation resources are unavailable")?,
    )?;
    let lifecycle_ports: Arc<dyn studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts> =
        Arc::new(
            studio_runtime_services::lifecycle_effects::ProductionLifecyclePorts::new(
                Arc::clone(&store),
                monitor_runtime.dispatch_wake_handle(),
                Arc::clone(&registry_wake),
                Arc::clone(&account_session),
                state.voice_request_port_handle(),
            )
            .with_continuation_wake(Arc::clone(&continuation_scheduler_wake)),
        );
    match studio_runtime_services::voice::provider::purge_settled_delete_intents(&store).await {
        Ok(purged) if purged > 0 => eprintln!("removed {purged} settled voice deletion receipts"),
        Ok(_) => {}
        Err(error) => eprintln!("settled voice deletion receipts remain: {error}"),
    }
    if let Err(error) = studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts::committed(
        lifecycle_ports.as_ref(),
    )
    .await
    {
        eprintln!("lifecycle startup reconciliation remains pending: {error}");
    }
    state = state.with_lifecycle_ports(Arc::clone(&lifecycle_ports));
    // The paired-server driver exists only now; agent tools that use it
    // (`orchestration_servers`) refuse with a named dependency until here.
    if let Some(driver) = state.orchestration_driver_handle() {
        managed_tool_runtime.attach_multi_server(Arc::clone(&store), driver);
    }
    // The host runtime installed the provider-session voice adapter above.
    if let Some(voice) = state.voice_request_port_handle() {
        managed_tool_runtime.attach_voice(voice);
    }
    let sync_changes = state.sync_changes_handle();
    let cost_scan_cancelled = state.cost_scan_cancellation_handle();
    let vault_job_owner = VaultJobOwner::start();
    state = state.with_vault_job_client(vault_job_owner.client());
    let app = match router(state.clone()) {
        Ok(app) => {
            let app = app
                .layer(Extension(Arc::clone(&monitor_runtime)))
                .layer(Extension(lifecycle_ports));
            #[cfg(unix)]
            let app = if let Some(resources) = runtime.preparation_resources() {
                let repair: Arc<dyn studio_runtime_http::NativeContextRepairPort> =
                    Arc::new(NativeContextRepairAdapter);
                app.layer(Extension(resources)).layer(Extension(repair))
            } else {
                app
            };
            app
        }
        Err(error) => {
            #[cfg(unix)]
            {
                let terminal_cleanup = shutdown_io_runtime(Arc::clone(&io_runtime)).await;
                let runtime_cleanup = runtime_shutdown::close(runtime, &state).await;
                let vault_jobs_cleanup = vault_job_owner.shutdown().await;
                terminal_cleanup?;
                runtime_cleanup?;
                vault_jobs_cleanup?;
            }
            #[cfg(not(unix))]
            {
                let services_cleanup = state.shutdown_services().await;
                let vault_jobs_cleanup = vault_job_owner.shutdown().await;
                services_cleanup?;
                vault_jobs_cleanup?;
            }
            return Err(error);
        }
    };
    let app = app.merge(web_assets::router(web_assets::WebAssets::discover()));
    startup_trace("http-router-completed");
    // Start the peer child only after listener bind and all fallible router
    // construction have succeeded. The pre-created advert is already shared
    // with HttpState, so routes observe the child's later published address.
    #[cfg(unix)]
    let mut peer_link_runtime = public_origin
        .as_ref()
        .map(|origin| {
            peer_link::PeerLinkRuntime::start(
                Arc::clone(&store),
                address.port(),
                origin.clone(),
                state_root.clone().to_path_buf(),
                Arc::clone(&peer_link_advert),
            )
        })
        .transpose()?;
    studio_runtime_services::diagnostics::mark_startup_memory(
        studio_runtime_services::diagnostics::StartupMemoryMark::HttpReady,
    );
    let (monitor_shutdown, _) = watch::channel(false);
    #[cfg(unix)]
    let history_task =
        history_runtime::HistoryRuntime::new(Arc::clone(&store), Arc::clone(&account_driver))
            .spawn(monitor_shutdown.subscribe());
    let monitor_task = studio_runtime_services::monitor_runtime_wiring::start(
        monitor_runtime,
        monitor_shutdown.subscribe(),
    );
    // Sign-in links of agents (docs/vm-browser-bridge.md): the host bridge of native chats
    // serves the launcher and control sockets; one poll for each source writes the cards.
    #[cfg(unix)]
    let browser_host = match studio_runtime_services::browser_host::HostBridgeService::start(
        &state_root,
        Arc::new(studio_runtime_services::browser_host::StoreChats {
            store: Arc::clone(&store),
        }),
        studio_runtime_services::browser_host::HostBridgeOptions::for_this_host(),
        env::current_exe().ok().as_deref(),
    ) {
        Ok(service) => Some(service),
        Err(error) => {
            eprintln!("browser bridge for native chats: {error}");
            None
        }
    };
    let browser_card_tasks: Vec<_> = state
        .browser_card_sources()
        .into_iter()
        .map(|source| {
            tokio::spawn(studio_runtime_services::browser_cards::run(
                Arc::clone(&store),
                source,
                Some(state.event_wake_handle()),
                monitor_shutdown.subscribe(),
            ))
        })
        .collect();
    // Links that agents give for a sign-in (docs/vm-browser-bridge.md, section 7): the
    // `orchestration_sign_in` tool and sign-in links in completed messages reach the same
    // bridges and cards.
    let sign_in_sources =
        studio_runtime_services::sign_in::SignInSources::new(state.browser_card_sources());
    managed_tool_runtime.attach_sign_in(sign_in_sources.clone());
    let sign_in_task = tokio::spawn(studio_runtime_services::sign_in::run_messages(
        Arc::clone(&store),
        sign_in_sources,
        monitor_shutdown.subscribe(),
    ));
    // Sign-in alerts on the owner's phones (docs/web-push.md).
    let web_push_task = tokio::spawn(studio_runtime_services::web_push::run(
        Arc::clone(&store),
        state_root.to_path_buf(),
        monitor_shutdown.subscribe(),
    ));
    let studio_errors_task = tokio::spawn(studio_runtime_services::studio_errors::run(
        Arc::clone(&store),
        monitor_shutdown.subscribe(),
    ));
    let retention_task = tokio::spawn(studio_runtime_services::storage_retention::run(
        Arc::clone(&store),
        monitor_shutdown.subscribe(),
    ));
    let analytics_projection_task =
        tokio::spawn(studio_runtime_services::analytics_projection::run(
            Arc::clone(&store),
            monitor_shutdown.subscribe(),
        ));
    let auth_release_task = tokio::spawn(studio_runtime_services::account_auth_gate::run_release(
        Arc::clone(&store),
        Arc::clone(&account_driver),
        Arc::clone(&registry_wake),
        monitor_shutdown.subscribe(),
    ));
    #[cfg(unix)]
    let idle_compaction_task = runtime.preparation_resources().map(|resources| {
        let host = Arc::new(studio_runtime_http::routes::IdleCompactionHost {
            state: state.clone(),
            resources,
            repair: Arc::new(NativeContextRepairAdapter),
        });
        tokio::spawn(studio_runtime_services::idle_compaction::run(
            Arc::clone(&store),
            Arc::clone(&account_session),
            host,
            monitor_shutdown.subscribe(),
        ))
    });
    let continuation_task = tokio::spawn(continuation_scheduler(
        Arc::clone(&store),
        continuation_scheduler_wake,
        Arc::clone(&account_session),
        Arc::clone(&registry_wake),
        monitor_shutdown.subscribe(),
    ));
    let orchestration_task = state.orchestration_driver_handle().map(|driver| {
        let wake = Arc::clone(&orchestration_wake);
        let mut shutdown = monitor_shutdown.subscribe();
        tokio::spawn(async move {
            let mut failure_retry_at = None;
            loop {
                if *shutdown.borrow() {
                    break;
                }
                if let Some(retry_at) = failure_retry_at
                    && retry_at > tokio::time::Instant::now()
                {
                    if !wait_for_scheduler_wake(&wake, &mut shutdown, Some(retry_at)).await {
                        break;
                    }
                    continue;
                }
                let now = epoch_seconds() as i64;
                match driver.pump_due(now).await {
                    Ok(_) => failure_retry_at = None,
                    Err(error) => {
                        eprintln!("orchestration delivery pass failed: {error}");
                        failure_retry_at =
                            Some(tokio::time::Instant::now() + SCHEDULER_FAILURE_RETRY);
                    }
                }
                let (due, deadline_read_failed) = match driver.next_due_at().await {
                    Ok(Some(at)) => (
                        Some(
                            tokio::time::Instant::now()
                                + Duration::from_secs(scheduler_delay_seconds(
                                    at,
                                    epoch_seconds() as i64,
                                )),
                        ),
                        false,
                    ),
                    Ok(None) => (None, false),
                    Err(error) => {
                        eprintln!("orchestration deadline read failed: {error}");
                        (
                            Some(tokio::time::Instant::now() + SCHEDULER_FAILURE_RETRY),
                            true,
                        )
                    }
                };
                if deadline_read_failed {
                    failure_retry_at = Some(tokio::time::Instant::now() + SCHEDULER_FAILURE_RETRY);
                }
                let due = due.into_iter().chain(failure_retry_at).min();
                if !wait_for_scheduler_wake(&wake, &mut shutdown, due).await {
                    break;
                }
            }
        })
    });
    let federation_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let federation_store = Arc::clone(&store);
    let federation_stop = Arc::clone(&federation_cancelled);
    let federation_wake_task = Arc::clone(&federation_wake);
    let mut federation_shutdown = monitor_shutdown.subscribe();
    let federation_task = tokio::spawn(async move {
        let mut pairing_retries = BTreeMap::<String, (tokio::time::Instant, Duration)>::new();
        let mut delivery_retries = BTreeMap::<String, (tokio::time::Instant, Duration)>::new();
        let mut read_failure_retry_at = None;
        loop {
            if *federation_shutdown.borrow() {
                federation_stop.store(true, std::sync::atomic::Ordering::Release);
                break;
            }
            if let Some(retry_at) = read_failure_retry_at
                && retry_at > tokio::time::Instant::now()
            {
                if !wait_for_retry_or_shutdown(&mut federation_shutdown, retry_at).await {
                    federation_stop.store(true, std::sync::atomic::Ordering::Release);
                    break;
                }
                read_failure_retry_at = None;
                continue;
            }
            let cancelled = Arc::clone(&federation_stop);
            let now = tokio::time::Instant::now();
            let wall_now = epoch_seconds();
            let mut read_retry_due = None;
            let pairings =
                studio_runtime_services::federation_delivery::pending_pairings(&federation_store)
                    .await;
            let pairings = match pairings {
                Ok(pairings) => {
                    let pending = pairings
                        .iter()
                        .cloned()
                        .collect::<std::collections::BTreeSet<_>>();
                    pairing_retries.retain(|id, _| pending.contains(id));
                    pairings
                }
                Err(error) => {
                    eprintln!("federation pairing scan failed: {error}");
                    read_retry_due = Some(now + SCHEDULER_FAILURE_RETRY);
                    Vec::new()
                }
            };
            for id in pairings {
                if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                    break;
                }
                if pairing_retry_is_pending(&pairing_retries, &id, now) {
                    continue;
                }
                let transport =
                    studio_runtime_services::federation_delivery::HttpsFederationTransport;
                match studio_runtime_services::federation_delivery::pair_peer(
                    Arc::clone(&federation_store),
                    &id,
                    wall_now,
                    &transport,
                    Arc::clone(&cancelled),
                )
                .await
                {
                    Ok(updated) => {
                        if let Some(delay) = successful_pair_retry_delay(
                            &updated,
                            pairing_retries.get(&id).map(|(_, previous)| *previous),
                        ) {
                            pairing_retries
                                .insert(id, (tokio::time::Instant::now() + delay, delay));
                        } else {
                            pairing_retries.remove(&id);
                        }
                    }
                    Err(error) => {
                        eprintln!("federation pairing failed: {error}");
                        let delay = next_federation_retry_delay(
                            pairing_retries.get(&id).map(|(_, previous)| *previous),
                        );
                        pairing_retries.insert(id, (tokio::time::Instant::now() + delay, delay));
                    }
                }
            }
            match studio_runtime_services::federation::snapshot(&federation_store).await {
                Ok(snapshot) => {
                    let approved = snapshot["peers"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|peer| peer["status"] == "approved")
                        .filter_map(|peer| peer["id"].as_str().map(str::to_owned))
                        .collect::<std::collections::BTreeSet<_>>();
                    delivery_retries.retain(|id, _| approved.contains(id));
                    for id in approved {
                        if delivery_retries.get(&id).is_some_and(|(at, _)| *at > now) {
                            continue;
                        }
                        let transport =
                            studio_runtime_services::federation_delivery::HttpsFederationTransport;
                        if let Err(error) = studio_runtime_services::federation_delivery::pump_peer(
                            Arc::clone(&federation_store),
                            &id,
                            wall_now,
                            &transport,
                            Arc::clone(&cancelled),
                        )
                        .await
                        {
                            eprintln!("federation delivery failed: {error}");
                            let delay = next_federation_retry_delay(
                                delivery_retries.get(&id).map(|(_, previous)| *previous),
                            );
                            delivery_retries
                                .insert(id, (tokio::time::Instant::now() + delay, delay));
                        } else {
                            delivery_retries.remove(&id);
                        }
                        if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                            break;
                        }
                    }
                }
                Err(error) => {
                    eprintln!("federation peer snapshot failed: {error}");
                    read_retry_due = Some(tokio::time::Instant::now() + SCHEDULER_FAILURE_RETRY);
                }
            }
            let delivery_deadlines =
                studio_runtime_services::federation_delivery::delivery_deadlines_by_peer(
                    &federation_store,
                )
                .await;
            let delivery_due = match delivery_deadlines {
                Ok(values) => values
                    .into_iter()
                    .filter_map(|(peer, at)| {
                        let retry = delivery_retries.get(&peer).map(|(retry_at, _)| *retry_at);
                        if retry.is_some_and(|retry_at| retry_at > now) {
                            retry
                        } else {
                            Some(
                                tokio::time::Instant::now()
                                    + Duration::from_secs_f64((at - epoch_seconds()).max(0.0)),
                            )
                        }
                    })
                    .min(),
                Err(error) => {
                    eprintln!("federation delivery deadline read failed: {error}");
                    read_retry_due = Some(tokio::time::Instant::now() + SCHEDULER_FAILURE_RETRY);
                    Some(tokio::time::Instant::now() + SCHEDULER_FAILURE_RETRY)
                }
            };
            read_failure_retry_at = read_retry_due;
            let due = pairing_retries
                .values()
                .map(|(at, _)| *at)
                .chain(delivery_retries.values().map(|(at, _)| *at))
                .chain(delivery_due)
                .chain(read_retry_due)
                .min();
            if !wait_for_scheduler_wake(&federation_wake_task, &mut federation_shutdown, due).await
            {
                federation_stop.store(true, std::sync::atomic::Ordering::Release);
                break;
            }
        }
    });
    startup_trace("readiness-output-started");
    println!("Codex Canvas: http://{}:{}", address.ip(), address.port());
    #[cfg(unix)]
    let stop_runtime = runtime.shutdown_handle();
    let mutation_shutdown = state.clone();
    let shutdown_sync_changes = Arc::clone(&sync_changes);
    let shutdown_cost_scans = Arc::clone(&cost_scan_cancelled);
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let reason = shutdown_signal().await;
        mutation_shutdown.close_mutation_admission();
        shutdown_sync_changes.close();
        eprintln!("{}", shutdown_exit_log(reason));
        shutdown_cost_scans.store(true, std::sync::atomic::Ordering::Release);
        if let Err(error) = shutdown_requested.send(()) {
            eprintln!("runtime server shutdown watchdog could not start: {error}");
        }
    })
    .await;
    // Keep account/provider dependencies live while HTTP mutations and durable
    // login/cancel work drain. The existing 20-second watchdog bounds this
    // sequence; if it forces exit, existing receipt, unknown-outcome, and
    // recovery rules remain authoritative without automatic request replay.
    state.shutdown_mutations().await;
    let durable_session_cleanup = shutdown_join(
        "provider login and cancellation operations",
        account_session.shutdown_durable_operations(),
    )
    .await?;
    durable_session_cleanup.map_err(std::io::Error::other)?;
    monitor_shutdown.send_replace(true);
    #[cfg(unix)]
    stop_runtime.cancel();
    #[cfg(unix)]
    if let Some(peer_link) = peer_link_runtime.as_mut() {
        peer_link.shutdown().await;
    }
    federation_cancelled.store(true, std::sync::atomic::Ordering::Release);
    let vault_jobs_cleanup = shutdown_join("vault job owner", vault_job_owner.shutdown()).await?;
    vault_jobs_cleanup?;
    #[cfg(unix)]
    let history_cleanup = shutdown_join("history runtime task", history_task).await?;
    #[cfg(unix)]
    history_cleanup.map_err(std::io::Error::other)??;
    shutdown_join("federation delivery task", federation_task).await??;
    shutdown_join("continuation scheduler task", continuation_task).await??;
    #[cfg(unix)]
    if let Some(task) = idle_compaction_task {
        shutdown_join("idle compaction scheduler task", task).await??;
    }

    shutdown_join("held turn release task", auth_release_task).await??;
    for task in browser_card_tasks {
        shutdown_join("browser card poll", task).await??;
    }
    shutdown_join("sign-in message links", sign_in_task).await??;
    shutdown_join("web push", web_push_task).await??;
    #[cfg(unix)]
    if let Some(service) = browser_host {
        shutdown_join("browser host bridge", service.stop()).await?;
    }
    shutdown_join("Studio error publication task", studio_errors_task).await??;
    shutdown_join("storage retention task", retention_task).await??;
    shutdown_join("analytics projection task", analytics_projection_task).await??;
    if let Some(task) = orchestration_task {
        shutdown_join("orchestration driver task", task).await??;
    }
    let monitor_result = shutdown_join("monitor runtime task", monitor_task).await?;
    monitor_result.map_err(std::io::Error::other)??;
    #[cfg(unix)]
    {
        let terminal_cleanup = shutdown_join(
            "terminal IO runtime",
            shutdown_io_runtime(Arc::clone(&io_runtime)),
        )
        .await?;
        terminal_cleanup?;
        let runtime_cleanup = shutdown_join(
            "host workers and retained services",
            runtime_shutdown::close(runtime, &state),
        )
        .await?;
        runtime_cleanup?;
    }
    #[cfg(not(unix))]
    shutdown_join("retained services", state.shutdown_services()).await??;
    result?;
    Ok(())
}

async fn continuation_scheduler(
    store: Arc<RuntimeStore>,
    wake: Arc<Notify>,
    account_session: Arc<dyn AccountSessionPort>,
    event_wake: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut retry_not_before = None;
    let mut tick_state = ContinuationTickState::default();
    loop {
        if *shutdown.borrow() {
            return;
        }
        if let Some(retry_at) = retry_not_before {
            let now = epoch_seconds();
            if retry_at > now {
                let deadline =
                    tokio::time::Instant::now() + Duration::from_secs_f64(retry_at - now);
                if !wait_for_retry_or_shutdown(&mut shutdown, deadline).await {
                    return;
                }
                retry_not_before = None;
                continue;
            }
            retry_not_before = None;
        }
        let now = epoch_seconds();
        let next_due = match store
            .read_async(
                "runtime-server/continuation-scheduler",
                move |db| -> studio_runtime_store::StoreResult<Option<f64>> {
                    let due = continuation_next_due(db)?;
                    let auth_proof_pending = auth_resume_proof_exists(db)?;
                    Ok(if auth_proof_pending { Some(now) } else { due })
                },
            )
            .await
        {
            Ok(value) => value,
            Err(error) => {
                eprintln!("continuation scheduler due-time read failed: {error}");
                let retry_at = now + CONTINUATION_FAILURE_RETRY_SECONDS;
                retry_not_before = Some(retry_at);
                Some(retry_at)
            }
        };
        let next_due = next_due
            .map(|due| retry_not_before.map_or(due, |retry_at: f64| due.max(retry_at)))
            .or(retry_not_before);
        let delay = next_due.map(|due| Duration::from_secs_f64((due - now).max(0.0)));
        match wait_for_continuation(delay, &wake, &mut shutdown).await {
            ContinuationWait::Due => {
                let store_for_tick = Arc::clone(&store);
                let tick_now = epoch_seconds();
                let result = run_continuation_tick(
                    store_for_tick,
                    tick_now,
                    Some(account_session.as_ref()),
                    &mut tick_state,
                )
                .await;
                match result {
                    Ok(tick) => {
                        retry_not_before = tick
                            .failed
                            .then(|| epoch_seconds() + CONTINUATION_FAILURE_RETRY_SECONDS);
                        if tick.changed {
                            event_wake.notify_one();
                        }
                    }
                    Err(error) => {
                        eprintln!("continuation scheduler tick failed: {error}");
                        retry_not_before =
                            Some(epoch_seconds() + CONTINUATION_FAILURE_RETRY_SECONDS);
                    }
                }
            }
            ContinuationWait::Wake => {}
            ContinuationWait::Shutdown => return,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContinuationWait {
    Due,
    Wake,
    Shutdown,
}

async fn wait_for_continuation(
    delay: Option<Duration>,
    wake: &Notify,
    shutdown: &mut watch::Receiver<bool>,
) -> ContinuationWait {
    tokio::select! {
        _ = async {
            match delay {
                Some(delay) => tokio::time::sleep(delay).await,
                None => std::future::pending::<()>().await,
            }
        } => ContinuationWait::Due,
        _ = wake.notified() => ContinuationWait::Wake,
        changed = shutdown.changed() => {
            if changed.is_err() || *shutdown.borrow() {
                ContinuationWait::Shutdown
            } else {
                ContinuationWait::Wake
            }
        }
    }
}

#[derive(Default)]
struct ContinuationTickState {
    after_agent: Option<String>,
    errors: BTreeMap<String, String>,
}

impl ContinuationTickState {
    fn record_error(&mut self, agent: &str, error: String) -> bool {
        if self.errors.get(agent) == Some(&error) {
            return false;
        }
        self.errors.insert(agent.to_owned(), error);
        true
    }
}

#[derive(Default)]
struct ContinuationTick {
    changed: bool,
    failed: bool,
}

async fn run_continuation_tick(
    store: Arc<RuntimeStore>,
    now: f64,
    account_session: Option<&dyn AccountSessionPort>,
    state: &mut ContinuationTickState,
) -> studio_runtime_store::StoreResult<ContinuationTick> {
    use futures_util::StreamExt;
    let started = std::time::Instant::now();
    let after_agent = state.after_agent.clone();
    let error_agents = state.errors.keys().cloned().collect::<Vec<_>>();
    let (agents, read_after, removed_agents) = store
        .read_async("runtime-server/continuation-inputs", move |db| {
            let mut agents = Vec::new();
            let mut read_after = BTreeMap::<String, f64>::new();
            for agent in continuation_tick_agents(db, now, after_agent.as_deref())? {
                let account: String = db.query_row(
                    "SELECT COALESCE(json_extract(record,'$.accountKey'),'default') FROM runtime_agents WHERE id=?1",
                    [&agent],
                    |row| row.get(0),
                )?;
                let usage_after: Option<f64> = db.query_row(
                    "SELECT MAX(COALESCE(CAST(json_extract(resume.record,'$.failedAt') AS REAL),0),COALESCE(CAST(json_extract(resume.record,'$.lastCheckedAt') AS REAL),0)) FROM runtime_usage_resumes AS resume JOIN runtime_agents AS owner ON owner.id=resume.agent AND json_extract(owner.record,'$.usageResume.id')=resume.id WHERE resume.agent=?1 AND json_extract(resume.record,'$.status')='scheduled' AND json_extract(resume.record,'$.dueAt')<=?2",
                    rusqlite::params![agent, now],
                    |row| row.get(0),
                ).optional()?;
                if let Some(after) = usage_after {
                    read_after
                        .entry(account.clone())
                        .and_modify(|previous| *previous = previous.max(after))
                        .or_insert(after);
                }
                agents.push((agent, account));
            }
            let mut removed_agents = Vec::new();
            for agent in error_agents {
                let exists: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE id=?1)",
                    [&agent],
                    |row| row.get(0),
                )?;
                if !exists {
                    removed_agents.push(agent);
                }
            }
            Ok::<_, studio_runtime_store::StoreError>((agents, read_after, removed_agents))
        })
        .await?;
    for agent in removed_agents {
        state.errors.remove(&agent);
    }
    // Read once per due usage-resume account before staging. A failed read
    // supplies no observation. Capacity retries and completed auth proofs need no read.
    let fresh = futures_util::stream::iter(read_after)
        .map(|(account, after)| async move {
            let value = match account_session {
                Some(session) => session.read_limits_for_resume(&account, after).await.ok(),
                None => None,
            };
            (account, value)
        })
        .buffer_unordered(CONTINUATION_LIMIT_READ_CONCURRENCY)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .filter_map(|(account, value)| value.map(|value| (account, value)))
        .collect::<BTreeMap<_, _>>();
    let mut tick = ContinuationTick::default();
    for (agent_id, account) in agents {
        state.after_agent = Some(agent_id.clone());
        let fresh = fresh.get(&account).cloned();
        // A read can move the due time. Use the time after it commits.
        let tick_now = now + started.elapsed().as_secs_f64();
        let agent = agent_id.clone();
        let staged = store.transaction_with(move |tx| {
            let current_account: Option<String> = tx
                .query_row(
                    "SELECT COALESCE(json_extract(record,'$.accountKey'),'default') FROM runtime_agents WHERE id=?1",
                    [&agent],
                    |row| row.get(0),
                )
                .optional()?;
            if current_account.as_deref() != Some(account.as_str()) {
                return Ok(false);
            }
            let observation = if let Some(mut value) = fresh {
                // A notification can commit between the read and this transaction.
                // Prefer that newer snapshot, including a new rejection.
                if let Some(current) = account_state::latest_limits_snapshot(tx, &account)?
                    && current["at"].as_f64().unwrap_or(0.0) >= value["at"].as_f64().unwrap_or(0.0)
                {
                    value = current;
                }
                studio_runtime_services::usage_resume::LimitsObservation::from_record(value, None)
                    .ok()
                    .filter(|observation| observation.account_key() == account)
            } else {
                None
            };
            let stage = studio_runtime_services::continuation_runtime::stage_tick(
                tx,
                &agent,
                tick_now,
                observation.as_ref(),
            )
            .map_err(|error| studio_runtime_store::StoreError::SchemaMismatch(error.to_string()))?;
            Ok::<_, studio_runtime_store::StoreError>(stage.changed())
        }).await;
        // One agent's bad data must not hold back the other due agents.
        match staged {
            Ok(staged) => {
                tick.changed |= staged;
                state.errors.remove(&agent_id);
            }
            Err(error) => {
                tick.failed = true;
                if state.record_error(&agent_id, error.to_string()) {
                    eprintln!("continuation tick skipped agent {agent_id}: {error}");
                }
            }
        }
    }
    Ok(tick)
}

fn auth_resume_proof_exists(
    connection: &rusqlite::Connection,
) -> studio_runtime_store::StoreResult<bool> {
    connection
        .query_row(
            &format!(
                "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE {AUTH_RESUME_PROOF_PENDING_PREDICATE})"
            ),
            [],
            |row| row.get(0),
        )
        .map_err(Into::into)
}

fn continuation_next_due(
    connection: &rusqlite::Connection,
) -> studio_runtime_store::StoreResult<Option<f64>> {
    let sql = format!(
        "SELECT MIN(due) FROM ({CURRENT_USAGE_RESUME_DUE_ROWS} UNION ALL {CURRENT_CAPACITY_RETRY_DUE_ROWS})"
    );
    connection
        .query_row(&sql, [], |row| row.get(0))
        .map_err(Into::into)
}

fn continuation_tick_agents(
    connection: &rusqlite::Connection,
    now: f64,
    after_agent: Option<&str>,
) -> studio_runtime_store::StoreResult<Vec<String>> {
    // Rotate the combined ready set: neither a failed cursor nor a busy kind
    // can monopolize the bounded batch. Advance past every attempted agent.
    let sql = format!(
        "SELECT agent FROM (
            SELECT agent FROM ({CURRENT_USAGE_RESUME_DUE_ROWS} UNION ALL {CURRENT_CAPACITY_RETRY_DUE_ROWS}) WHERE due<=?1
            UNION SELECT id AS agent FROM runtime_agents WHERE {AUTH_RESUME_PROOF_PENDING_PREDICATE}
         ) ORDER BY CASE WHEN ?2 IS NULL OR agent>?2 THEN 0 ELSE 1 END, agent LIMIT ?3"
    );
    let mut statement = connection.prepare(&sql)?;
    statement
        .query_map(
            rusqlite::params![now, after_agent, CONTINUATION_TICK_AGENT_LIMIT],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

async fn wait_for_scheduler_wake(
    wake: &Notify,
    shutdown: &mut watch::Receiver<bool>,
    due: Option<tokio::time::Instant>,
) -> bool {
    if *shutdown.borrow() {
        return false;
    }
    tokio::select! {
        changed = shutdown.changed() => changed.is_ok() && !*shutdown.borrow(),
        _ = wake.notified() => true,
        _ = async {
            match due {
                Some(deadline) => tokio::time::sleep_until(deadline).await,
                None => std::future::pending::<()>().await,
            }
        } => true,
    }
}

/// A persisted read failure blocks all work until its retry deadline. Commit
/// notifications are intentionally not an input here, so they cannot bypass
/// the bounded error backoff.
async fn wait_for_retry_or_shutdown(
    shutdown: &mut watch::Receiver<bool>,
    retry_at: tokio::time::Instant,
) -> bool {
    loop {
        if *shutdown.borrow() {
            return false;
        }
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return false;
                }
            }
            _ = tokio::time::sleep_until(retry_at) => return true,
        }
    }
}

fn next_federation_retry_delay(previous: Option<Duration>) -> Duration {
    previous.map_or(Duration::from_secs(5), |delay| {
        delay.saturating_mul(2).min(Duration::from_secs(300))
    })
}

fn successful_pair_retry_delay(
    updated: &serde_json::Value,
    previous: Option<Duration>,
) -> Option<Duration> {
    (updated["localApproved"] == true && updated["remoteApproved"] != true)
        .then(|| next_federation_retry_delay(previous))
}

fn pairing_retry_is_pending(
    retries: &BTreeMap<String, (tokio::time::Instant, Duration)>,
    peer_id: &str,
    now: tokio::time::Instant,
) -> bool {
    retries
        .get(peer_id)
        .is_some_and(|(retry_at, _)| *retry_at > now)
}

fn scheduler_delay_seconds(deadline: i64, now: i64) -> u64 {
    deadline.saturating_sub(now).max(0) as u64
}

fn epoch_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |duration| duration.as_secs_f64())
}

#[cfg(unix)]
struct NativeContextRepairAdapter;

#[cfg(unix)]
impl studio_runtime_http::NativeContextRepairPort for NativeContextRepairAdapter {
    fn repair_idle(
        &self,
        store: Arc<RuntimeStore>,
        agent_id: String,
        provider_home: PathBuf,
        broker: Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
        deadline: std::time::Instant,
    ) -> std::pin::Pin<
        Box<
            dyn std::future::Future<
                    Output = studio_runtime_services::native_actions::Result<serde_json::Value>,
                > + Send
                + 'static,
        >,
    > {
        Box::pin(async move {
            studio_runtime_services::context_repair::repair_idle_with_broker(
                store,
                &agent_id,
                provider_home.as_path(),
                broker.as_ref(),
                deadline,
            )
            .await
            .map_err(|_| {
                studio_runtime_services::native_actions::NativeError::Unavailable(
                    "context-repair/repair-idle-broker",
                )
            })
        })
    }
}

#[cfg(unix)]
async fn shutdown_io_runtime(
    runtime: Arc<io_runtime::IoRuntime>,
) -> Result<(), Box<dyn std::error::Error>> {
    runtime.shutdown().await?;
    Ok(())
}

fn native_discovery_inputs(
    explicit: Option<&str>,
) -> studio_runtime_services::provider_binaries::DiscoveryInputs {
    use studio_runtime_services::provider_binaries::DiscoveryInputs;

    let pinned_path = env::var_os("CODEX_AGENTS_NATIVE_RUNTIME_PIN").map(PathBuf::from);
    if pinned_path.is_some() {
        return DiscoveryInputs {
            pinned_path,
            ..DiscoveryInputs::default()
        };
    }
    let mut candidates = Vec::new();
    if let Some(value) = explicit {
        let path = PathBuf::from(value);
        if path.components().count() > 1 {
            candidates.push(path);
        } else if let Some(found) = env::var_os("PATH").and_then(|path| {
            env::split_paths(&path)
                .map(|entry| entry.join(value))
                .find(|candidate| candidate.is_file())
        }) {
            candidates.push(found);
        }
    }
    let path_entries = env::var_os("PATH")
        .map(|path| env::split_paths(&path).collect::<Vec<_>>())
        .unwrap_or_default()
        .into_iter()
        .map(|entry| entry.join("codex"))
        .filter(|candidate| candidate.is_file())
        .collect();
    #[cfg(target_os = "macos")]
    let chatgpt_bundle = Some(PathBuf::from(
        "/Applications/ChatGPT.app/Contents/Resources/codex",
    ));
    #[cfg(not(target_os = "macos"))]
    let chatgpt_bundle = None;
    DiscoveryInputs {
        explicit_path: candidates.first().cloned(),
        path_entries,
        chatgpt_bundle,
        ..DiscoveryInputs::default()
    }
}

fn native_platform() -> Option<studio_runtime_services::provider_binaries::NativePlatform> {
    use studio_runtime_services::provider_binaries::NativePlatform;
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("linux", "x86_64") => Some(NativePlatform::LinuxX64),
        ("linux", "aarch64") => Some(NativePlatform::LinuxArm64),
        ("macos", "x86_64") => Some(NativePlatform::MacX64),
        ("macos", "aarch64") => Some(NativePlatform::MacArm64),
        ("windows", "x86_64") => Some(NativePlatform::WindowsX64),
        ("windows", "aarch64") => Some(NativePlatform::WindowsArm64),
        _ => None,
    }
}

fn parse_port(mut args: impl Iterator<Item = String>) -> Result<u16, Box<dyn std::error::Error>> {
    let mut port = operator_cli::service_layout()?.port;
    while let Some(argument) = args.next() {
        if argument == "--port" {
            port = args.next().ok_or("--port requires a number")?.parse()?;
        } else if let Some(value) = argument.strip_prefix("--port=") {
            port = value.parse()?;
        } else {
            return Err(format!("unknown argument: {argument}").into());
        }
    }
    Ok(port)
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ShutdownReason {
    CtrlC,
    Sigterm,
    StdinClosed,
}

fn shutdown_exit_log(reason: ShutdownReason) -> &'static str {
    match reason {
        ShutdownReason::CtrlC => "runtime server shutdown requested by Ctrl-C",
        ShutdownReason::Sigterm => "runtime server shutdown requested by SIGTERM",
        ShutdownReason::StdinClosed => "runtime server shutdown requested because stdin closed",
    }
}

async fn wait_for_stdin_close() {
    let mut stdin = tokio::io::stdin();
    let mut sink = tokio::io::sink();
    if let Err(error) = tokio::io::copy(&mut stdin, &mut sink).await {
        eprintln!("runtime server stdin close monitor ended with an error: {error}");
    }
}

async fn shutdown_signal() -> ShutdownReason {
    let watch_stdin_close = env::var_os(EXIT_ON_STDIN_CLOSE_ENV).is_some();
    let stdin_closed = wait_for_stdin_close();
    tokio::pin!(stdin_closed);
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => tokio::select! {
                result = tokio::signal::ctrl_c() => {
                    if let Err(error) = result { eprintln!("failed to listen for Ctrl-C: {error}"); }
                    ShutdownReason::CtrlC
                }
                _ = terminate.recv() => ShutdownReason::Sigterm,
                _ = &mut stdin_closed, if watch_stdin_close => ShutdownReason::StdinClosed
            },
            Err(error) => {
                eprintln!("failed to listen for SIGTERM: {error}");
                tokio::select! {
                    result = tokio::signal::ctrl_c() => {
                        if let Err(error) = result { eprintln!("failed to listen for Ctrl-C: {error}"); }
                        ShutdownReason::CtrlC
                    }
                    _ = &mut stdin_closed, if watch_stdin_close => ShutdownReason::StdinClosed
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            result = tokio::signal::ctrl_c() => {
                if let Err(error) = result { eprintln!("failed to listen for Ctrl-C: {error}"); }
                ShutdownReason::CtrlC
            }
            _ = &mut stdin_closed, if watch_stdin_close => ShutdownReason::StdinClosed
        }
    }
}

#[cfg(test)]
mod targeted_scheduler_wake_tests {
    use super::{FEDERATION_WAKE_TABLES, ORCHESTRATION_WAKE_TABLES, observe_table_wakes};
    use std::sync::Arc;
    use std::time::Duration;
    use studio_runtime_store::RuntimeStore;
    use tokio::sync::Notify;

    #[tokio::test]
    async fn unrelated_event_commit_does_not_wake_orchestration_but_outbox_commit_does() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store");
        let orchestration = Arc::new(Notify::new());
        let federation = Arc::new(Notify::new());
        observe_table_wakes(
            &store,
            &ORCHESTRATION_WAKE_TABLES,
            Arc::clone(&orchestration),
        );
        observe_table_wakes(&store, &FEDERATION_WAKE_TABLES, Arc::clone(&federation));

        let orchestration_waiter = tokio::spawn({
            let orchestration = Arc::clone(&orchestration);
            async move { orchestration.notified().await }
        });
        let federation_waiter = tokio::spawn({
            let federation = Arc::clone(&federation);
            async move { federation.notified().await }
        });
        tokio::task::yield_now().await;
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch,turn_id,error) \
                     VALUES('unrelated-event','agent','user','input','pending',1,0,NULL,NULL)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("commit unrelated event input");
        tokio::task::yield_now().await;
        assert!(
            !orchestration_waiter.is_finished(),
            "event-journal writes must not wake the outbox scheduler"
        );
        assert!(
            !federation_waiter.is_finished(),
            "event-journal writes must not wake the federation scheduler"
        );
        orchestration_waiter.abort();
        federation_waiter.abort();

        let orchestration_waiter = tokio::spawn({
            let orchestration = Arc::clone(&orchestration);
            async move { orchestration.notified().await }
        });
        let federation_waiter = tokio::spawn({
            let federation = Arc::clone(&federation);
            async move { federation.notified().await }
        });
        tokio::task::yield_now().await;
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_server_outbox(\
                        id,server,body,state,created_at,next_at,fingerprint,attempts\
                     ) VALUES('outbox-event','peer','{}','queued',1,1,'fingerprint',0)",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("commit durable outbox input");
        tokio::time::timeout(Duration::from_secs(1), orchestration_waiter)
            .await
            .expect("orchestration observes outbox input")
            .expect("orchestration waiter");
        tokio::task::yield_now().await;
        assert!(
            !federation_waiter.is_finished(),
            "orchestration outbox writes must not wake federation"
        );
        federation_waiter.abort();

        let federation_waiter = tokio::spawn({
            let federation = Arc::clone(&federation);
            async move { federation.notified().await }
        });
        tokio::task::yield_now().await;
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_federation_peers(id,record) VALUES('peer-input','{}')",
                    [],
                )?;
                Ok(())
            })
            .await
            .expect("commit federation peer input");
        tokio::time::timeout(Duration::from_secs(1), federation_waiter)
            .await
            .expect("federation observes peer input")
            .expect("federation waiter");
    }
}

#[cfg(test)]
mod exit_logging_tests {
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    use tokio::sync::{Notify, watch};

    use super::{
        CONTINUATION_FAILURE_RETRY_SECONDS, ContinuationTickState, ContinuationWait,
        ShutdownReason, auth_resume_proof_exists, continuation_next_due, continuation_scheduler,
        continuation_tick_agents, error_exit_log, next_federation_retry_delay,
        pairing_retry_is_pending, panic_observed_log, run_continuation_tick,
        runtime_initialization_exit_log, scheduler_delay_seconds, shutdown_exit_log, shutdown_join,
        successful_pair_retry_delay, wait_for_continuation, wait_for_retry_or_shutdown,
        wait_for_scheduler_wake,
    };

    #[test]
    fn every_graceful_exit_signal_has_a_reason_log() {
        assert_eq!(
            shutdown_exit_log(ShutdownReason::CtrlC),
            "runtime server shutdown requested by Ctrl-C"
        );
        assert_eq!(
            shutdown_exit_log(ShutdownReason::Sigterm),
            "runtime server shutdown requested by SIGTERM"
        );
        assert_eq!(
            shutdown_exit_log(ShutdownReason::StdinClosed),
            "runtime server shutdown requested because stdin closed"
        );
    }

    #[test]
    fn startup_and_error_exit_paths_log_their_reason() {
        assert_eq!(
            runtime_initialization_exit_log("resource limit reached"),
            "runtime server exiting because Tokio runtime initialization failed: resource limit reached"
        );
        assert_eq!(
            error_exit_log("state lock is busy"),
            "runtime server exiting after error: state lock is busy"
        );
    }

    #[test]
    fn panic_log_redacts_payload_and_keeps_source_location() {
        let log = panic_observed_log(Some(std::panic::Location::caller()));
        assert!(log.starts_with("runtime server observed panic at "));
        assert!(log.ends_with(" (payload redacted)"));
        assert!(!log.contains("projection invariant failed"));
        assert!(!log.contains("exiting"));
    }

    #[tokio::test(start_paused = true)]
    async fn shutdown_join_detaches_a_stuck_task_with_its_name() {
        let task = tokio::spawn(std::future::pending::<()>());
        let error = shutdown_join("active account worker", task)
            .await
            .expect_err("shutdown joins must have a finite bound");
        assert!(error.to_string().contains("active account worker"));
    }

    struct TickLimits {
        calls: std::sync::atomic::AtomicUsize,
        allowed: bool,
        newer_rejection: Option<Arc<studio_runtime_store::RuntimeStore>>,
    }
    impl studio_runtime_services::account_session_runtime::AccountSessionPort for TickLimits {
        fn read_limits<'a>(
            &'a self,
            account: &'a str,
        ) -> studio_runtime_services::account_session_runtime::AccountSessionFuture<
            'a,
            serde_json::Value,
        > {
            self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async move {
                if !self.allowed {
                    return Err(studio_runtime_services::account_session_runtime::AccountSessionError::Unknown);
                }
                if let Some(store) = &self.newer_rejection {
                    store.transaction_with(|tx| {
                        studio_runtime_store::SyncDocumentsRepository::put_entity(tx,"rateLimit","default",serde_json::json!({"accountKey":"default","at":1001.0,"data":{"rateLimits":{"primary":{"usedPercent":100.0,"resetsAt":2000.0}}}}),false)
                    }).await.expect("rate-limit fixture writes");
                }
                Ok(
                    serde_json::json!({"accountKey":account,"at":1000.0,"data":{"rateLimits":{"primary":{"usedPercent":10.0,"resetsAt":2000.0}}}}),
                )
            })
        }
    }

    #[tokio::test]
    async fn continuation_tick_reads_fresh_limits_once_per_account_and_fails_closed() {
        for (allowed, newer_rejection) in [(true, false), (false, false), (true, true)] {
            let directory = tempfile::tempdir().expect("temporary state directory creates");
            let store = Arc::new(
                studio_runtime_store::RuntimeStore::open(Some(directory.path()))
                    .await
                    .expect("runtime store opens"),
            );
            store.transaction_with(move |tx| {
                for id in ["a", "b"] {
                    let resume = serde_json::json!({"id":id,"accountKey":"default","threadId":"thread","epoch":1,"turnId":"turn","cause":"rate_limit","status":"scheduled","failedAt":900.0,"dueAt":990.0,"taskClaims":[],"updatedAt":980.0});
                    let agent = serde_json::json!({"id":id,"rootId":id,"epoch":1,"status":"failed","accountKey":"default","threadId":"thread","lastCompletedTurn":"turn","autoWake":true,"nativeFailureHold":true,"cwd":"/fixture","isLead":true,"usageResume":resume});
                    tx.execute("INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",[id,&agent.to_string()])?;
                    tx.execute("INSERT INTO runtime_usage_resumes(id,agent,record) VALUES (?1,?1,?2)",[id,&resume.to_string()])?;
                }
                // A recent cache entry cannot replace the provider read at the due time.
                studio_runtime_store::SyncDocumentsRepository::put_entity(tx,"rateLimit","default",serde_json::json!({"accountKey":"default","at":999.0,"data":{"rateLimits":{"primary":{"usedPercent":0.0}}}}),false)?;
                Ok::<_,studio_runtime_store::StoreError>(())
            }).await.expect("usage resume fixture writes");
            let port = TickLimits {
                calls: std::sync::atomic::AtomicUsize::new(0),
                allowed,
                newer_rejection: newer_rejection.then(|| store.clone()),
            };
            run_continuation_tick(
                store.clone(),
                1000.0,
                Some(&port),
                &mut ContinuationTickState::default(),
            )
            .await
            .expect("continuation tick completes");
            assert_eq!(port.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            let db = store.test_reader().expect("test reader opens");
            let record: String = db
                .query_row(
                    "SELECT record FROM runtime_usage_resumes WHERE id='a'",
                    [],
                    |row| row.get(0),
                )
                .expect("usage resume record loads");
            let record: serde_json::Value =
                serde_json::from_str(&record).expect("usage resume JSON parses");
            assert_eq!(
                record["status"],
                if allowed && !newer_rejection {
                    "started"
                } else {
                    "scheduled"
                }
            );
            if !allowed || newer_rejection {
                assert!(
                    record["dueAt"]
                        .as_f64()
                        .expect("resume deadline is numeric")
                        > 1000.0
                );
            }
            drop(db);
            run_continuation_tick(
                store.clone(),
                1000.0,
                Some(&port),
                &mut ContinuationTickState::default(),
            )
            .await
            .expect("replayed continuation tick completes");
            assert_eq!(port.calls.load(std::sync::atomic::Ordering::SeqCst), 1);
            let count:i64=store.test_reader().expect("test reader opens").query_row("SELECT count(*) FROM runtime_events WHERE id IN ('usage-resume:a','usage-resume:b')",[],|row|row.get(0)).expect("resume event count loads");
            assert_eq!(count, if allowed && !newer_rejection { 2 } else { 0 });
        }
    }

    #[tokio::test]
    async fn continuation_tick_reads_its_inputs_in_one_checkout() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store"),
        );
        let changed =
            run_continuation_tick(store, 0.0, None, &mut ContinuationTickState::default())
                .await
                .expect("a tick with nothing due succeeds");
        assert!(!changed.changed);
    }

    #[tokio::test]
    async fn continuation_tick_skips_a_failing_agent_and_stages_the_others() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store"),
        );
        store
            .transaction_with(|tx| {
                // The first agent's proof cursor has no proofAt, so its stage fails.
                for (id, pending) in [
                    ("a-broken", serde_json::json!({"afterId":""})),
                    ("b-valid", serde_json::json!({"afterId":"","proofAt":1.0})),
                ] {
                    let record = serde_json::json!({
                        "id":id, "rootId":id, "role":"main", "status":"idle", "epoch":1,
                        "authResumeProofPending":{"default":pending},
                    });
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                        [id, &record.to_string()],
                    )?;
                }
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed agents");
        let changed = run_continuation_tick(
            Arc::clone(&store),
            2.0,
            None,
            &mut ContinuationTickState::default(),
        )
        .await
        .expect("one failing agent does not fail the tick");
        assert!(changed.changed, "the valid agent is staged");
        assert!(
            changed.failed,
            "the malformed agent requests scheduler backoff"
        );
        let db = store.test_reader().expect("reader");
        let pending = |id: &str| -> Option<String> {
            db.query_row(
                "SELECT json_extract(record,'$.authResumeProofPending') FROM runtime_agents WHERE id=?1",
                [id],
                |row| row.get(0),
            )
            .expect("agent row")
        };
        assert_eq!(pending("b-valid"), None, "the valid agent's cursor is done");
        assert!(
            pending("a-broken").is_some(),
            "the failing agent keeps its cursor"
        );
    }

    async fn proof_cursor_store(
        broken: usize,
        valid: usize,
    ) -> (tempfile::TempDir, Arc<studio_runtime_store::RuntimeStore>) {
        let directory = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        store
            .transaction_with(move |tx| {
                for index in 0..broken + valid {
                    let id = format!("agent-{index:02}");
                    let pending = if index < broken {
                        serde_json::json!({"afterId":""})
                    } else {
                        serde_json::json!({"afterId":"", "proofAt":1.0})
                    };
                    let record = serde_json::json!({
                        "id":id, "rootId":id, "role":"main", "status":"idle", "epoch":1,
                        "authResumeProofPending":{"default":pending},
                    });
                    tx.execute(
                        "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                        rusqlite::params![id, record.to_string()],
                    )?;
                }
                Ok::<_, studio_runtime_store::StoreError>(())
            })
            .await
            .expect("seed cursors");
        (directory, store)
    }

    #[tokio::test(start_paused = true)]
    async fn continuation_malformed_cursor_uses_backoff_despite_many_wakes() {
        let (_directory, store) = proof_cursor_store(1, 1).await;
        let baseline = store.writer_admissions();
        let wake = Arc::new(Notify::new());
        let event_wake = Arc::new(Notify::new());
        let (shutdown, shutdown_rx) = watch::channel(false);
        // Proof cursors need no limit read; a refusing port adds no observation.
        let account_session: Arc<dyn super::AccountSessionPort> = Arc::new(TickLimits {
            calls: std::sync::atomic::AtomicUsize::new(0),
            allowed: false,
            newer_rejection: None,
        });
        let scheduler = tokio::spawn(continuation_scheduler(
            Arc::clone(&store),
            Arc::clone(&wake),
            account_session,
            Arc::clone(&event_wake),
            shutdown_rx,
        ));
        // The valid cursor's notification proves that the whole first tick ran,
        // including the earlier malformed cursor's failed staging transaction.
        event_wake.notified().await;
        assert_eq!(store.writer_admissions() - baseline, 2);
        for _ in 0..1000 {
            wake.notify_one();
            tokio::task::yield_now().await;
        }
        tokio::time::advance(Duration::from_secs_f64(
            CONTINUATION_FAILURE_RETRY_SECONDS - 1.0,
        ))
        .await;
        tokio::task::yield_now().await;
        assert_eq!(
            store.writer_admissions() - baseline,
            2,
            "queued wakes cannot retry the malformed cursor before backoff"
        );
        shutdown.send_replace(true);
        scheduler.await.expect("scheduler stops during backoff");
    }

    #[tokio::test]
    async fn continuation_round_robin_reaches_all_forty_failing_cursors() {
        let (_directory, store) = proof_cursor_store(40, 0).await;
        let mut state = ContinuationTickState::default();
        let first = run_continuation_tick(Arc::clone(&store), 2.0, None, &mut state)
            .await
            .expect("first batch");
        assert!(first.failed);
        assert_eq!(
            state.errors.len(),
            usize::try_from(super::CONTINUATION_TICK_AGENT_LIMIT).expect("positive batch limit")
        );
        let second = run_continuation_tick(
            store,
            2.0 + CONTINUATION_FAILURE_RETRY_SECONDS,
            None,
            &mut state,
        )
        .await
        .expect("next batch");
        assert!(second.failed);
        assert_eq!(
            state.errors.len(),
            40,
            "every cursor was attempted even though the early ids remain pending"
        );
    }

    #[tokio::test]
    async fn continuation_round_robin_stages_valid_cursors_after_a_full_failing_batch() {
        let (_directory, store) = proof_cursor_store(32, 8).await;
        let mut state = ContinuationTickState::default();
        let first = run_continuation_tick(Arc::clone(&store), 2.0, None, &mut state)
            .await
            .expect("first batch");
        assert!(first.failed);
        assert!(!first.changed);
        let second = run_continuation_tick(
            Arc::clone(&store),
            2.0 + CONTINUATION_FAILURE_RETRY_SECONDS,
            None,
            &mut state,
        )
        .await
        .expect("second batch");
        assert!(
            second.changed,
            "later valid cursors progress while early cursors keep failing"
        );
        let reader = store.test_reader().expect("reader");
        let pending: i64 = reader.query_row(
            "SELECT COUNT(*) FROM runtime_agents WHERE json_extract(record,'$.authResumeProofPending') IS NOT NULL",
            [], |row| row.get(0),
        ).expect("remaining pending cursors");
        assert_eq!(pending, 32, "all eight valid cursors were completed");
    }

    #[tokio::test]
    async fn continuation_recovery_clears_the_recorded_error() {
        let (_directory, store) = proof_cursor_store(1, 0).await;
        let mut state = ContinuationTickState::default();
        let failed = run_continuation_tick(Arc::clone(&store), 2.0, None, &mut state)
            .await
            .expect("malformed cursor tick");
        assert!(failed.failed);
        assert_eq!(state.errors.len(), 1);
        store.transaction_with(|tx| {
            tx.execute(
                "UPDATE runtime_agents SET record=json_set(record,'$.authResumeProofPending.default.proofAt',1.0)",
                [],
            )?;
            Ok::<_, studio_runtime_store::StoreError>(())
        }).await.expect("repair cursor");
        let recovered = run_continuation_tick(
            store,
            2.0 + CONTINUATION_FAILURE_RETRY_SECONDS,
            None,
            &mut state,
        )
        .await
        .expect("repaired cursor tick");
        assert!(!recovered.failed);
        assert!(recovered.changed);
        assert!(
            state.errors.is_empty(),
            "successful staging clears the old failure"
        );
    }

    #[test]
    fn continuation_errors_are_reported_only_when_the_error_changes() {
        let mut state = ContinuationTickState::default();
        assert!(state.record_error("agent", "malformed cursor".to_owned()));
        for _ in 0..1000 {
            assert!(!state.record_error("agent", "malformed cursor".to_owned()));
        }
        assert!(state.record_error("agent", "different error".to_owned()));
        state.errors.remove("agent");
        assert!(
            state.record_error("agent", "different error".to_owned()),
            "a recovered agent can fail again visibly"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn continuation_waits_for_real_deadline_or_event() {
        let wake = Arc::new(Notify::new());
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let timer_wake = Arc::clone(&wake);
        let timer = tokio::spawn(async move {
            wait_for_continuation(Some(Duration::from_secs(10)), &timer_wake, &mut shutdown_rx)
                .await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(!timer.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(timer.await.expect("timer task"), ContinuationWait::Due);

        let (shutdown_tx2, mut shutdown_rx2) = watch::channel(false);
        let timer_wake = Arc::clone(&wake);
        let wake_task = tokio::spawn(async move {
            wait_for_continuation(None, &timer_wake, &mut shutdown_rx2).await
        });
        tokio::task::yield_now().await;
        wake.notify_one();
        assert_eq!(wake_task.await.expect("wake task"), ContinuationWait::Wake);

        let timer_wake = Arc::clone(&wake);
        let shutdown_for_task = shutdown_tx.clone();
        let shutdown_task = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_for_task.subscribe();
            wait_for_continuation(None, &timer_wake, &mut shutdown_rx).await
        });
        tokio::task::yield_now().await;
        shutdown_tx.send_replace(true);
        assert_eq!(
            shutdown_task.await.expect("shutdown task"),
            ContinuationWait::Shutdown
        );
        drop(shutdown_tx2);
    }

    #[tokio::test(start_paused = true)]
    async fn continuation_idle_wait_has_no_24_hour_recheck() {
        let wake = Arc::new(Notify::new());
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let waiter_wake = Arc::clone(&wake);
        let waiter = tokio::spawn(async move {
            wait_for_continuation(None, &waiter_wake, &mut shutdown_rx).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(24 * 60 * 60 + 1)).await;
        assert!(!waiter.is_finished());
        wake.notify_one();
        assert_eq!(waiter.await.expect("idle wake"), ContinuationWait::Wake);
        drop(shutdown_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn continuation_failure_backoff_ignores_a_queued_work_wake() {
        let wake = Notify::new();
        wake.notify_one();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let retry_at = tokio::time::Instant::now() + Duration::from_secs(60);
        let waiter =
            tokio::spawn(
                async move { wait_for_retry_or_shutdown(&mut shutdown_rx, retry_at).await },
            );
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(59)).await;
        assert!(!waiter.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(waiter.await.expect("failure retry deadline"));
        let (shutdown_tx2, mut shutdown_rx) = watch::channel(false);
        assert_eq!(
            wait_for_continuation(None, &wake, &mut shutdown_rx).await,
            ContinuationWait::Wake,
            "failure cooldown leaves the queued work notification for the regular wait"
        );
        drop((shutdown_tx, shutdown_tx2));
    }

    #[test]
    fn auth_resume_proof_tick_skips_empty_objects_before_real_pending_proof() {
        let connection = rusqlite::Connection::open_in_memory().expect("memory database");
        connection
            .execute_batch(&studio_runtime_store::agent_columns::fixture_sql(
                "CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
                 CREATE TABLE runtime_capacity_retries(id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);",
            ))
            .expect("agents table");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        for index in 0..40 {
            connection
                .execute(
                    "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                    rusqlite::params![
                        format!("empty-{index:02}"),
                        serde_json::json!({"authResumeProofPending":{}}).to_string()
                    ],
                )
                .expect("empty proof row");
        }
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('real-proof',?1)",
                [
                    serde_json::json!({"authResumeProofPending":{"requestId":"stable-request"}})
                        .to_string(),
                ],
            )
            .expect("pending proof row");

        assert!(auth_resume_proof_exists(&connection).expect("pending proof exists"));
        assert_eq!(
            continuation_tick_agents(&connection, 100.0, None).expect("pending proof batch"),
            vec!["real-proof".to_owned()]
        );
    }

    #[test]
    fn stale_due_children_cannot_starve_a_real_auth_resume_proof() {
        let connection = rusqlite::Connection::open_in_memory().expect("memory database");
        connection
            .execute_batch(
                &studio_runtime_store::agent_columns::fixture_sql("CREATE TABLE runtime_agents(id TEXT PRIMARY KEY, record TEXT NOT NULL);
                 CREATE TABLE runtime_usage_resumes(id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);
                 CREATE TABLE runtime_capacity_retries(id TEXT PRIMARY KEY, agent TEXT NOT NULL, record TEXT NOT NULL);"),
            )
            .expect("continuation tables");
        connection
            .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
            .expect("volatile agent table creates");
        for index in 0..32 {
            let agent = format!("agent-{index:02}");
            connection
                .execute(
                    "INSERT INTO runtime_agents(id,record) VALUES (?1,?2)",
                    rusqlite::params![
                        agent,
                        serde_json::json!({"usageResume":{"id":"new-current-id"}}).to_string()
                    ],
                )
                .expect("owner row");
            connection
                .execute(
                    "INSERT INTO runtime_usage_resumes(id,agent,record) VALUES (?1,?2,?3)",
                    rusqlite::params![
                        format!("stale-resume-{index:02}"),
                        agent,
                        serde_json::json!({"status":"scheduled","dueAt":1.0}).to_string()
                    ],
                )
                .expect("stale due child");
        }
        connection
            .execute(
                "INSERT INTO runtime_agents(id,record) VALUES ('real-proof',?1)",
                [serde_json::json!({"authResumeProofPending":{"account":{"proofAt":1.0,"afterId":""}}}).to_string()],
            )
            .expect("pending proof owner");

        assert_eq!(
            continuation_next_due(&connection).expect("current child deadline"),
            None
        );
        assert_eq!(
            continuation_tick_agents(&connection, 100.0, None).expect("tick agent batch"),
            vec!["real-proof".to_owned()]
        );
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_wakes_survive_pre_wait_commits_and_due_deadlines() {
        let registry_wake = Notify::new();
        let orchestration_wake = Notify::new();
        registry_wake.notify_one();
        orchestration_wake.notify_one();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        assert!(wait_for_scheduler_wake(&registry_wake, &mut shutdown_rx, None).await);
        assert!(wait_for_scheduler_wake(&orchestration_wake, &mut shutdown_rx, None).await);

        let (shutdown_idle_tx, mut shutdown_idle_rx) = watch::channel(false);
        let idle_wake = Arc::new(Notify::new());
        let idle_wake_task = Arc::clone(&idle_wake);
        let idle_waiter = tokio::spawn(async move {
            wait_for_scheduler_wake(&idle_wake_task, &mut shutdown_idle_rx, None).await
        });
        tokio::task::yield_now().await;
        assert!(!idle_waiter.is_finished());
        idle_wake.notify_one();
        assert!(idle_waiter.await.expect("idle wake waiter"));

        let (shutdown_due_tx, mut shutdown_due_rx) = watch::channel(false);
        let wake = Notify::new();
        let due = tokio::time::Instant::now() + Duration::from_secs(12);
        let waiter = tokio::spawn(async move {
            wait_for_scheduler_wake(&wake, &mut shutdown_due_rx, Some(due)).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(12)).await;
        assert!(waiter.await.expect("deadline waiter"));
        drop((shutdown_tx, shutdown_idle_tx, shutdown_due_tx));
    }

    #[tokio::test(start_paused = true)]
    async fn read_failure_retry_gate_does_not_run_on_a_pending_commit_wake() {
        let commit_wake = Notify::new();
        commit_wake.notify_one();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let retry_at = tokio::time::Instant::now() + Duration::from_secs(30);
        let waiter =
            tokio::spawn(
                async move { wait_for_retry_or_shutdown(&mut shutdown_rx, retry_at).await },
            );
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(29)).await;
        assert!(!waiter.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert!(waiter.await.expect("retry gate waiter"));
        drop(shutdown_tx);
    }

    #[tokio::test(start_paused = true)]
    async fn committed_change_wakes_both_independent_scheduler_subscribers() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = studio_runtime_store::RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store");
        store
            .transaction(|tx| {
                tx.execute_batch("CREATE TABLE scheduler_wake_fixture(id INTEGER PRIMARY KEY);")?;
                Ok(())
            })
            .await
            .expect("wake fixture table");
        let registry = Arc::new(Notify::new());
        let orchestration = Arc::new(Notify::new());
        {
            let registry = Arc::clone(&registry);
            store.add_commit_observer(move || registry.notify_one());
        }
        {
            let orchestration = Arc::clone(&orchestration);
            store.add_commit_observer(move || orchestration.notify_one());
        }
        store
            .transaction(|tx| {
                tx.execute("INSERT INTO scheduler_wake_fixture DEFAULT VALUES", [])?;
                Ok(())
            })
            .await
            .expect("committed change");
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        assert!(wait_for_scheduler_wake(&registry, &mut shutdown_rx, None).await);
        assert!(wait_for_scheduler_wake(&orchestration, &mut shutdown_rx, None).await);
        drop(shutdown_tx);
    }

    #[test]
    fn pairing_failure_backoff_is_bounded() {
        let delays = (0..8)
            .scan(None, |previous, _| {
                let next = next_federation_retry_delay(*previous);
                *previous = Some(next);
                Some(next)
            })
            .collect::<Vec<_>>();
        assert_eq!(
            delays,
            [5, 10, 20, 40, 80, 160, 300, 300].map(Duration::from_secs)
        );
    }

    #[test]
    fn successful_pending_pairing_waits_before_retry_after_its_own_commit() {
        let now = tokio::time::Instant::now();
        let response = serde_json::json!({"localApproved": true, "remoteApproved": false});
        let delay = successful_pair_retry_delay(&response, None)
            .expect("pending remote approval has a retry deadline");
        assert_eq!(delay, Duration::from_secs(5));
        let mut retries = BTreeMap::new();
        retries.insert("peer-id".to_owned(), (now + delay, delay));

        // pair_peer commits the same pending response before the scheduler waits;
        // the resulting wake cannot bypass this peer's retry deadline.
        assert!(pairing_retry_is_pending(&retries, "peer-id", now));
        assert!(!pairing_retry_is_pending(&retries, "peer-id", now + delay));
        assert_eq!(
            successful_pair_retry_delay(
                &serde_json::json!({"localApproved": true, "remoteApproved": true}),
                None,
            ),
            None
        );
    }

    #[test]
    fn overdue_scheduler_deadlines_are_immediate_not_wrapped_durations() {
        assert_eq!(scheduler_delay_seconds(9, 10), 0);
        assert_eq!(scheduler_delay_seconds(10, 10), 0);
        assert_eq!(scheduler_delay_seconds(14, 10), 4);
    }

    #[tokio::test(start_paused = true)]
    async fn scheduler_wait_exits_on_shutdown() {
        let wake = Notify::new();
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let waiter =
            tokio::spawn(
                async move { wait_for_scheduler_wake(&wake, &mut shutdown_rx, None).await },
            );
        tokio::task::yield_now().await;
        shutdown_tx.send_replace(true);
        assert!(!waiter.await.expect("shutdown waiter"));
    }
}

#[cfg(test)]
mod executor_worker_tests {
    use super::async_executor_worker_threads;

    #[test]
    fn default_async_worker_count_is_capped_by_available_parallelism() {
        assert_eq!(async_executor_worker_threads(Some(1), false), Some(1));
        assert_eq!(async_executor_worker_threads(Some(2), false), Some(2));
        assert_eq!(async_executor_worker_threads(Some(32), false), Some(4));
    }

    #[test]
    fn explicit_tokio_worker_override_leaves_configuration_to_tokio() {
        assert_eq!(async_executor_worker_threads(Some(32), true), None);
    }
}

#[cfg(test)]
mod canvas_pid_tests {
    use super::clear_owned_canvas_pid;
    use std::fs;

    #[test]
    fn shutdown_removes_only_the_pid_owned_by_this_process() {
        let state = tempfile::tempdir().expect("isolated state directory");
        let pid_file = state.path().join("canvas.pid");

        fs::write(&pid_file, "1234\n").expect("foreign PID fixture written");
        assert!(!clear_owned_canvas_pid(state.path(), 5678).expect("cleanup succeeds"));
        assert_eq!(
            fs::read_to_string(&pid_file).expect("foreign PID remains"),
            "1234\n"
        );

        fs::write(&pid_file, "5678\n").expect("owned PID fixture written");
        assert!(clear_owned_canvas_pid(state.path(), 5678).expect("owned PID removed"));
        assert!(!pid_file.exists());
    }
}

#[cfg(test)]
mod startup_registry_tests {
    use super::startup_registry;
    use std::fs;
    use std::path::Path;
    use studio_runtime_services::account_driver::{
        AccountDriver, AccountStatus, resolve_codex_home,
    };
    use studio_runtime_services::account_policy::AccountPolicyState;
    use studio_runtime_store::RuntimeStore;
    use studio_runtime_store::supervisor_journal::SupervisorJournal;

    /// The fresh-state path of the rehearsal: the supervisor creates its
    /// journal first, then the server opens the store and the registry.
    #[tokio::test]
    async fn fresh_state_startup_creates_the_default_registry_and_reads_it() {
        let state = tempfile::tempdir().expect("isolated state directory");
        let codex = tempfile::tempdir().expect("isolated CODEX_HOME");
        fs::write(
            codex.path().join("auth.json"),
            br#"{"OPENAI_API_KEY":"placeholder-not-a-credential"}"#,
        )
        .expect("auth fixture writes");
        let supervisor = SupervisorJournal::open(state.path()).expect("supervisor starts first");
        let store = RuntimeStore::open(Some(state.path()))
            .await
            .expect("store opens beside the supervisor journal");
        let state_root = studio_runtime_store::path_identity::CanonicalPath::existing(store.root())
            .expect("state root resolves");
        let codex_home = resolve_codex_home(Some(codex.path().as_os_str()), Path::new("/"))
            .expect("CODEX_HOME resolves");

        let registry =
            startup_registry(&state_root, &codex_home).expect("startup creates the registry");
        let path = registry.path;
        assert_eq!(path, state_root.join("accounts/registry.json"));
        let bytes = registry.bytes.expect("startup reads the created registry");
        let policy =
            AccountPolicyState::from_startup(Some(&bytes), []).expect("startup validates it");
        let accounts = policy.accounts();
        assert_eq!(accounts.default_account_key, "default");
        assert_eq!(accounts.accounts.len(), 1);
        assert_eq!(accounts.accounts[0]["status"], "ready");
        assert_eq!(accounts.accounts[0]["plan"], "API key");
        assert_eq!(
            accounts.accounts[0]["home"],
            codex_home.to_str().expect("UTF-8 fixture path")
        );
        let driver = AccountDriver::open(&path).expect("the account driver opens it");
        assert_eq!(driver.get("default"), Ok(AccountStatus::Ready));

        let refreshed = fs::read(&path).expect("refreshed registry reads");
        let again = startup_registry(&state_root, &codex_home).expect("second startup");
        assert_eq!(
            again.bytes,
            Some(refreshed),
            "an existing registry is never replaced"
        );
        drop(store);
        drop(supervisor);
    }
}
