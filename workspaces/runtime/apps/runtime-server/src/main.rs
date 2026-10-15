#[cfg(unix)]
mod account_registry;
#[cfg(unix)]
mod claude_login;
#[cfg(unix)]
mod history_runtime;
#[cfg(unix)]
mod host_runtime;
#[cfg(unix)]
mod io_runtime;
mod monitor_port;
mod multi_server_discovery;
mod multi_server_owner;
mod native_probe;
#[cfg(unix)]
mod peer_link;
mod provider_ports;
#[cfg(unix)]
mod runtime_shutdown;
use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::future::Future;
use std::io::Read;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::Duration;

use axum::Extension;
use rusqlite::OptionalExtension;
use studio_runtime_http::{HttpState, VaultJobOwner, router};
use studio_runtime_services::account_driver::AccountDriver;
use studio_runtime_services::account_policy::AccountPolicyState;
use studio_runtime_store::{RuntimeStore, account_state};
use tokio::net::TcpListener;
use tokio::sync::{Notify, watch};

const DEFAULT_PORT: u16 = 4620;
const SERVER_SHUTDOWN_GRACE: Duration = Duration::from_secs(20);
// Allow the worker and retained-service bounds to complete before process exit.
const SHUTDOWN_JOIN_GRACE: Duration = Duration::from_secs(20);
const CONTINUATION_FAILURE_RETRY_SECONDS: f64 = 60.0;
const CONTINUATION_IDLE_RECHECK_SECONDS: u64 = 24 * 60 * 60;
const EXIT_ON_STDIN_CLOSE_ENV: &str = "CODEX_AGENTS_SERVER_EXIT_ON_STDIN_CLOSE";

fn main() {
    std::panic::set_hook(Box::new(|panic| {
        eprintln!("{}", panic_observed_log(panic.location()));
    }));
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
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

async fn run(shutdown_requested: Sender<()>) -> Result<(), Box<dyn std::error::Error>> {
    studio_runtime_process::child_priority::ChildPriority::from_environment();
    studio_runtime_services::diagnostics::mark_startup_memory(
        studio_runtime_services::diagnostics::StartupMemoryMark::ProcessStarted,
    );
    let args = env::args().skip(1).collect::<Vec<_>>();
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
    let port = parse_port(args.into_iter())?;
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let address = listener.local_addr()?;
    let mut token = [0_u8; 32];
    getrandom::fill(&mut token).map_err(|error| std::io::Error::other(error.to_string()))?;
    let token = data_encoding::HEXLOWER.encode(&token);
    let configured_state_dir = env::var_os("CODEX_AGENTS_STATE_DIR").map(PathBuf::from);
    let store = Arc::new(RuntimeStore::open(configured_state_dir.as_deref())?);
    studio_runtime_services::diagnostics::mark_startup_memory(
        studio_runtime_services::diagnostics::StartupMemoryMark::StoreOpened,
    );
    let voice_recovery_time = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or_default();
    studio_runtime_services::voice::provider::recover_after_restart(&store, voice_recovery_time)?;
    studio_runtime_services::maintenance::remove_review_assignments(&store)?;
    let credentials =
        Arc::new(studio_runtime_services::federation_credentials::Credentials::default());
    #[cfg(unix)]
    {
        credentials.identity(&store, None)?;
    }
    studio_runtime_services::workspace_operations_effects::startup_recovery(&store)?;
    let workspace_id =
        studio_runtime_store::SyncDocumentsRepository::workspace_id(&*store.read()?)?;
    let state_root = store.root().canonicalize()?;
    let state_dir = state_root.to_string_lossy().into_owned();
    let registry_path = state_root.join("accounts/registry.json");
    let registry_file = match registry_path.canonicalize() {
        Ok(path) if !path.starts_with(&state_root) => {
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
    let limits_read = account_state::limits_snapshots(&*store.read()?)?;
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
    let account_driver = Arc::new(AccountDriver::open(&registry_path)?);
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
    let public_origin = env::var("CODEX_CANVAS_PUBLIC_ORIGIN").ok();
    #[cfg(unix)]
    let mut peer_link_runtime = public_origin.as_ref().map(|origin| {
        peer_link::PeerLinkRuntime::start(
            Arc::clone(&store),
            address.port(),
            origin.clone(),
            state_root.clone(),
        )
    });
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
    let home = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&state_dir));
    let base_home = env::var_os("CODEX_HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(".codex"));
    let account_session_broker = account_registry::AccountSessionBroker::new();
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
            base_home,
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
    });
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
        )?;
    let multi_server_boundary = public_origin
        .as_deref()
        .map(|origin| {
            let identity = credentials.identity(&store, Some(origin))?;
            let boundary = studio_runtime_services::multi_server_pairing::MultiServerBoundary::new(
                identity,
                Arc::new(multi_server_owner::TailscaleOwnerLookup),
            )?;
            let credentials = Arc::clone(&credentials);
            let origin = origin.to_owned();
            Ok::<_, Box<dyn std::error::Error>>(boundary.with_identity_source(move |store| {
                credentials.identity(store, Some(&origin)).map_err(|_| {
                    studio_runtime_services::multi_server_auth::refused(
                        503,
                        "identity_unavailable",
                        "The server identity is unavailable",
                    )
                })
            }))
        })
        .transpose()?;
    let mut state = HttpState::new(
        token,
        workspace_id,
        native_path.clone(),
        state_dir.clone(),
        address.port(),
        public_origin,
        Some(store.clone()),
        Some(account_policy.clone()),
    )
    .with_continuation_wake_handle(Arc::clone(&continuation_wake))
    .with_account_driver(account_driver.clone())
    .with_provider_versions(provider_versions)
    .with_credentials(credentials)
    .with_tailscale_whoami(Arc::new(|| {
        let raw = multi_server_owner::tailscale_json(&["whoami", "--json"]).ok();
        let user = raw.as_ref().and_then(|value| {
            value
                .get("UserProfile")
                .or_else(|| value.get("User"))
                .and_then(|profile| profile.get("LoginName").or_else(|| profile.get("Name")))
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
        let advert = peer_link_runtime
            .as_ref()
            .map(peer_link::PeerLinkRuntime::advert_handle)
            .unwrap_or_else(|| Arc::new(std::sync::RwLock::new(None)));
        state = state.with_peer_link_advert(advert);
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
    let io_runtime = Arc::new(io_runtime::IoRuntime::start(store.clone())?);
    #[cfg(unix)]
    {
        let runtime = Arc::clone(&io_runtime);
        state = state
            .with_terminal_driver(io_runtime.terminal_driver())
            .with_terminal_write_gate(Arc::new(move || runtime.reconcile_before_write()));
    }
    #[cfg(unix)]
    claude_login::attach(
        &mut state,
        PathBuf::from(&state_dir),
        store.clone(),
        account_driver.clone(),
    )
    .await?;
    let monitor_wake = state.event_wake_handle();
    let continuation_scheduler_wake = state.continuation_wake_handle();
    let monitor_port = Arc::new(monitor_port::RetainedMonitorPort::new(
        Arc::clone(&store),
        state
            .account_session
            .clone()
            .ok_or("account session runtime is unavailable")?,
    ));
    let monitor_port_trait: Arc<dyn studio_runtime_services::monitor_runtime_wiring::MonitorPort> =
        monitor_port.clone();
    let monitor_runtime = Arc::new(
        studio_runtime_services::monitor_runtime_wiring::MonitorRuntime::new(
            Arc::clone(&store),
            monitor_port_trait,
            Arc::clone(&monitor_wake),
        ),
    );
    state = state.open_app_server_log();
    let lifecycle_ports: Arc<dyn studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts> =
        Arc::new(
            studio_runtime_services::lifecycle_effects::ProductionLifecyclePorts::new(
                Arc::clone(&store),
                monitor_runtime.dispatch_wake_handle(),
                Arc::clone(&monitor_wake),
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
    let managed_tool_runtime =
        studio_runtime_services::provider_callbacks::ManagedToolRuntime::with_creation(
            Arc::clone(&monitor_runtime),
            Arc::clone(&lifecycle_ports),
            creation_environment,
        );
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
        Some(managed_tool_runtime),
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
    monitor_port.set_preparation_resources(
        runtime
            .preparation_resources()
            .ok_or("monitor preparation resources are unavailable")?,
    );
    let lifecycle_ports: Arc<dyn studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts> =
        Arc::new(
            studio_runtime_services::lifecycle_effects::ProductionLifecyclePorts::new(
                Arc::clone(&store),
                monitor_runtime.dispatch_wake_handle(),
                Arc::clone(&monitor_wake),
                Arc::clone(&account_session),
                state.voice_request_port_handle(),
            )
            .with_continuation_wake(Arc::clone(&continuation_scheduler_wake)),
        );
    if let Err(error) = studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts::committed(
        lifecycle_ports.as_ref(),
    ) {
        eprintln!("lifecycle startup reconciliation remains pending: {error}");
    }
    state = state.with_lifecycle_ports(Arc::clone(&lifecycle_ports));
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
    let continuation_task = tokio::spawn(continuation_scheduler(
        Arc::clone(&store),
        continuation_scheduler_wake,
        Arc::clone(&monitor_wake),
        monitor_shutdown.subscribe(),
    ));
    let orchestration_task = state.orchestration_driver_handle().map(|driver| {
        let mut shutdown = monitor_shutdown.subscribe();
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                tokio::select! {
                    _ = shutdown.changed() => break,
                    _ = tick.tick() => {
                        let driver = Arc::clone(&driver);
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|value| value.as_secs() as i64)
                            .unwrap_or_default();
                        drop(studio_runtime_process::blocking_pool::spawn_blocking("runtime-server/main", move || driver.pump_due(now)).await);
                    }
                }
            }
        })
    });
    let federation_cancelled = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let federation_store = Arc::clone(&store);
    let federation_stop = Arc::clone(&federation_cancelled);
    let mut federation_shutdown = monitor_shutdown.subscribe();
    let federation_task = tokio::spawn(async move {
        let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
        loop {
            tokio::select! {
                _ = federation_shutdown.changed() => {
                    federation_stop.store(true, std::sync::atomic::Ordering::Release);
                    break;
                }
                _ = tick.tick() => {
                    let store = Arc::clone(&federation_store);
                    let cancelled = Arc::clone(&federation_stop);
                    if let Err(error) = studio_runtime_process::blocking_pool::spawn_blocking("runtime-server/main", move || {
                        let Ok(snapshot) = studio_runtime_services::federation::snapshot(&store) else { return };
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|value| value.as_secs_f64())
                            .unwrap_or_default();
                        if let Ok(pairings) =
                            studio_runtime_services::federation_delivery::pending_pairings(&store)
                        {
                            for id in pairings {
                                let mut transport = studio_runtime_services::federation_delivery::HttpsFederationTransport;
                                if let Err(error) = studio_runtime_services::federation_delivery::pair_peer(
                                    &store, &id, now, &mut transport, &cancelled,
                                ) {
                                    eprintln!("federation pairing failed: {error}");
                                }
                                if cancelled.load(std::sync::atomic::Ordering::Acquire) {
                                    break;
                                }
                            }
                        }
                        for peer in snapshot["peers"].as_array().into_iter().flatten() {
                            if peer["status"] != "approved" { continue; }
                            let Some(id) = peer["id"].as_str() else { continue };
                            let mut transport = studio_runtime_services::federation_delivery::HttpsFederationTransport;
                            if let Err(error) = studio_runtime_services::federation_delivery::pump_peer(
                                &store, id, now, &mut transport, &cancelled,
                            ) {
                                eprintln!("federation delivery failed: {error}");
                            }
                            if cancelled.load(std::sync::atomic::Ordering::Acquire) { break; }
                        }
                    }).await {
                        eprintln!("federation delivery worker failed: {error}");
                    }
                }
            }
        }
    });
    println!("Codex Canvas: http://{}:{}", address.ip(), address.port());
    #[cfg(unix)]
    let stop_runtime = runtime.shutdown_handle();
    let shutdown_monitor_task = monitor_shutdown.clone();
    let shutdown_cost_scans = Arc::clone(&cost_scan_cancelled);
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        let reason = shutdown_signal().await;
        eprintln!("{}", shutdown_exit_log(reason));
        shutdown_cost_scans.store(true, std::sync::atomic::Ordering::Release);
        if let Err(error) = shutdown_requested.send(()) {
            eprintln!("runtime server shutdown watchdog could not start: {error}");
        }
        shutdown_monitor_task.send_replace(true);
        #[cfg(unix)]
        stop_runtime.cancel();
        sync_changes.close();
    })
    .await;
    monitor_shutdown.send_replace(true);
    #[cfg(unix)]
    if let Some(peer_link) = peer_link_runtime.as_mut() {
        peer_link.shutdown();
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
    event_wake: Arc<Notify>,
    mut shutdown: watch::Receiver<bool>,
) {
    let mut retry_not_before = None;
    loop {
        if *shutdown.borrow() {
            return;
        }
        let store_for_due = Arc::clone(&store);
        let now = epoch_seconds();
        let next_due = match studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/continuation-scheduler",
            move || -> studio_runtime_store::StoreResult<Option<f64>> {
                let db = store_for_due.read()?;
                let due: Option<f64> = db.query_row(
                    "SELECT MIN(due) FROM (\
                     SELECT CAST(json_extract(record,'$.dueAt') AS REAL) due FROM runtime_usage_resumes WHERE json_extract(record,'$.status')='scheduled' AND json_type(record,'$.dueAt') IN ('integer','real') \
                     UNION ALL SELECT CAST(json_extract(record,'$.dueAt') AS REAL) due FROM runtime_capacity_retries WHERE json_extract(record,'$.status')='scheduled' AND json_type(record,'$.dueAt') IN ('integer','real'))",
                    [],
                    |row| row.get(0),
                )?;
                let auth_proof_pending: bool = db.query_row(
                    "SELECT EXISTS(SELECT 1 FROM runtime_agents WHERE json_type(record,'$.authResumeProofPending')='object' AND json(json_extract(record,'$.authResumeProofPending')) <> '{}')",
                    [],
                    |row| row.get(0),
                )?;
                Ok(if auth_proof_pending { Some(now) } else { due })
            },
        )
        .await
        {
            Ok(Ok(value)) => value,
            Ok(Err(error)) => {
                eprintln!("continuation scheduler due-time read failed: {error}");
                Some(now + 60.0)
            }
            Err(error) => {
                eprintln!("continuation scheduler blocking read failed: {error}");
                Some(now + 60.0)
            }
        };
        let next_due = next_due
            .map(|due| retry_not_before.map_or(due, |retry_at: f64| due.max(retry_at)))
            .or(retry_not_before);
        let delay = next_due
            .map(|due| std::time::Duration::from_secs_f64((due - now).max(0.0)))
            .unwrap_or(std::time::Duration::from_secs(
                CONTINUATION_IDLE_RECHECK_SECONDS,
            ));
        match wait_for_continuation(delay, &wake, &mut shutdown).await {
            ContinuationWait::Due => {
                let store_for_tick = Arc::clone(&store);
                let tick_now = epoch_seconds();
                let result = studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-server/continuation-scheduler",
                    move || run_continuation_tick(&store_for_tick, tick_now),
                )
                .await;
                match result {
                    Ok(Ok(changed)) => {
                        retry_not_before = None;
                        if changed {
                            wake.notify_one();
                            event_wake.notify_one();
                        }
                    }
                    Ok(Err(error)) => {
                        eprintln!("continuation scheduler tick failed: {error}");
                        retry_not_before =
                            Some(epoch_seconds() + CONTINUATION_FAILURE_RETRY_SECONDS);
                    }
                    Err(error) => {
                        eprintln!("continuation scheduler blocking tick failed: {error}");
                        retry_not_before =
                            Some(epoch_seconds() + CONTINUATION_FAILURE_RETRY_SECONDS);
                    }
                }
            }
            ContinuationWait::Wake => {
                retry_not_before = None;
            }
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
    delay: Duration,
    wake: &Notify,
    shutdown: &mut watch::Receiver<bool>,
) -> ContinuationWait {
    tokio::select! {
        _ = tokio::time::sleep(delay) => ContinuationWait::Due,
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

fn run_continuation_tick(
    store: &RuntimeStore,
    now: f64,
) -> studio_runtime_store::StoreResult<bool> {
    let db = store.read()?;
    let mut statement = db.prepare_cached(
        "SELECT DISTINCT agent FROM (\
         SELECT agent,CAST(json_extract(record,'$.dueAt') AS REAL) due FROM runtime_usage_resumes WHERE json_extract(record,'$.status')='scheduled' AND json_type(record,'$.dueAt') IN ('integer','real') \
         UNION ALL SELECT agent,CAST(json_extract(record,'$.dueAt') AS REAL) due FROM runtime_capacity_retries WHERE json_extract(record,'$.status')='scheduled' AND json_type(record,'$.dueAt') IN ('integer','real')) \
         WHERE due<=?1 ORDER BY agent LIMIT 32",
    )?;
    let mut agents = statement
        .query_map([now], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    if agents.is_empty() {
        let mut statement = db.prepare_cached(
            "SELECT id FROM runtime_agents WHERE json_type(record,'$.authResumeProofPending')='object' LIMIT 32",
        )?;
        agents = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
    }
    drop(db);
    let limits_reader = store.read()?;
    let limits = account_state::limits_snapshots(&limits_reader)?
        .snapshots
        .into_iter()
        .filter_map(|(account_key, value)| {
            let mut value = value;
            if value.get("accountKey").is_none() {
                value["accountKey"] = serde_json::json!(account_key);
            }
            studio_runtime_services::usage_resume::LimitsObservation::from_record(value, None)
                .ok()
                .map(|limits| (account_key, limits))
        })
        .collect::<BTreeMap<_, _>>();
    drop(limits_reader);
    let mut changed = false;
    for agent_id in agents {
        let observations = limits.clone();
        let staged = store.transaction_with(|tx| {
            let account: Option<String> = tx
                .query_row(
                    "SELECT COALESCE(json_extract(record,'$.accountKey'),'default') FROM runtime_agents WHERE id=?1",
                    [&agent_id],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(account) = account else { return Ok(false) };
            let observation = observations.get(&account);
            let stage = studio_runtime_services::continuation_runtime::stage_tick(
                tx,
                &agent_id,
                now,
                observation,
            )
            .map_err(|error| studio_runtime_store::StoreError::SchemaMismatch(error.to_string()))?;
            Ok::<_, studio_runtime_store::StoreError>(stage.changed())
        })?;
        changed |= staged;
    }
    Ok(changed)
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
    ) -> studio_runtime_services::native_actions::Result<serde_json::Value> {
        studio_runtime_services::context_repair::repair_idle_with_broker(
            store.as_ref(),
            &agent_id,
            provider_home.as_path(),
            broker.as_ref(),
            deadline,
        )
        .map_err(|_| {
            studio_runtime_services::native_actions::NativeError::Unavailable(
                "context-repair/repair-idle-broker",
            )
        })
    }
}

#[cfg(unix)]
async fn shutdown_io_runtime(
    runtime: Arc<io_runtime::IoRuntime>,
) -> Result<(), Box<dyn std::error::Error>> {
    let result =
        studio_runtime_process::blocking_pool::spawn_blocking("runtime-server/main", move || {
            runtime.shutdown()
        })
        .await?;
    result?;
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
    let mut port = DEFAULT_PORT;
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
mod exit_logging_tests {
    use std::{sync::Arc, time::Duration};
    use tokio::sync::{Notify, watch};

    use super::{
        ContinuationWait, ShutdownReason, error_exit_log, panic_observed_log,
        runtime_initialization_exit_log, shutdown_exit_log, shutdown_join, wait_for_continuation,
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

    #[tokio::test(start_paused = true)]
    async fn continuation_timer_waits_for_due_time_and_wakes_on_enable_or_cancel() {
        let wake = Arc::new(Notify::new());
        let (shutdown_tx, mut shutdown_rx) = watch::channel(false);
        let timer_wake = Arc::clone(&wake);
        let timer = tokio::spawn(async move {
            wait_for_continuation(Duration::from_secs(10), &timer_wake, &mut shutdown_rx).await
        });
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(9)).await;
        assert!(!timer.is_finished());
        tokio::time::advance(Duration::from_secs(1)).await;
        assert_eq!(timer.await.expect("timer task"), ContinuationWait::Due);

        let (shutdown_tx2, mut shutdown_rx2) = watch::channel(false);
        let timer_wake = Arc::clone(&wake);
        let wake_task = tokio::spawn(async move {
            wait_for_continuation(Duration::from_secs(10_000), &timer_wake, &mut shutdown_rx2).await
        });
        tokio::task::yield_now().await;
        wake.notify_one();
        assert_eq!(wake_task.await.expect("wake task"), ContinuationWait::Wake);

        let timer_wake = Arc::clone(&wake);
        let shutdown_for_task = shutdown_tx.clone();
        let shutdown_task = tokio::spawn(async move {
            let mut shutdown_rx = shutdown_for_task.subscribe();
            wait_for_continuation(Duration::from_secs(10_000), &timer_wake, &mut shutdown_rx).await
        });
        tokio::task::yield_now().await;
        shutdown_tx.send_replace(true);
        assert_eq!(
            shutdown_task.await.expect("shutdown task"),
            ContinuationWait::Shutdown
        );
        drop(shutdown_tx2);
    }
}
