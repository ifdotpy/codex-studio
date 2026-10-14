#[cfg(unix)]
mod account_registry;
#[cfg(unix)]
mod claude_login;
#[cfg(unix)]
mod host_runtime;
#[cfg(unix)]
mod io_runtime;
mod monitor_port;
mod multi_server_owner;
mod native_probe;
mod provider_ports;
#[cfg(unix)]
mod runtime_shutdown;
use std::collections::BTreeMap;
use std::env;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use axum::Extension;
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::account_driver::AccountDriver;
use studio_runtime_services::account_policy::AccountPolicyState;
use studio_runtime_store::{RuntimeStore, account_state};
use tokio::net::TcpListener;
use tokio::sync::watch;

const DEFAULT_PORT: u16 = 4620;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
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
    studio_runtime_services::maintenance::remove_review_assignments(&store)?;
    let credentials =
        Arc::new(studio_runtime_services::federation_credentials::Credentials::default());
    #[cfg(unix)]
    {
        credentials.identity(&store, None)?;
    }
    studio_runtime_services::workspace_operations_effects::startup_recovery(&store)?;
    let workspace_id = studio_runtime_store::SyncDocumentsRepository::workspace_id(&store.read()?)?;
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
    let limits_read = account_state::limits_snapshots(&store.read()?)?;
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
        match tokio::task::spawn_blocking(move || runtime_for_check.check(discovery, platform))
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
        .with_store(Arc::clone(&store)),
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
    .with_account_driver(account_driver.clone())
    .with_provider_versions(provider_versions)
    .with_credentials(credentials);
    if let Some(boundary) = multi_server_boundary {
        state = state.with_multi_server_boundary(Arc::new(boundary));
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
    #[cfg(unix)]
    let runtime = match host_runtime::HostRuntime::start_with_monitor_wake_and_native_runtime(
        &mut state,
        Arc::clone(&store),
        account_driver,
        native_path,
        monitor_runtime.dispatch_wake_handle(),
        account_session_broker.clone(),
        Some(Arc::clone(&native_runtime)),
        Arc::clone(&session_binaries),
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
            ),
        );
    if let Err(error) = studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts::committed(
        lifecycle_ports.as_ref(),
    ) {
        eprintln!("lifecycle startup reconciliation remains pending: {error}");
    }
    let sync_changes = state.sync_changes_handle();
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
                terminal_cleanup?;
                runtime_cleanup?;
            }
            #[cfg(not(unix))]
            state.shutdown_services().await?;
            return Err(error);
        }
    };
    studio_runtime_services::diagnostics::mark_startup_memory(
        studio_runtime_services::diagnostics::StartupMemoryMark::HttpReady,
    );
    let (monitor_shutdown, _) = watch::channel(false);
    let monitor_task = studio_runtime_services::monitor_runtime_wiring::start(
        monitor_runtime,
        monitor_shutdown.subscribe(),
    );
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
                        drop(tokio::task::spawn_blocking(move || driver.pump_due(now)).await);
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
                    if let Err(error) = tokio::task::spawn_blocking(move || {
                        let Ok(snapshot) = studio_runtime_services::federation::snapshot(&store) else { return };
                        let now = std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .map(|value| value.as_secs_f64())
                            .unwrap_or_default();
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
    let result = axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(async move {
        shutdown_signal().await;
        shutdown_monitor_task.send_replace(true);
        #[cfg(unix)]
        stop_runtime.cancel();
        sync_changes.close();
    })
    .await;
    monitor_shutdown.send_replace(true);
    federation_cancelled.store(true, std::sync::atomic::Ordering::Release);
    federation_task.await?;
    if let Some(task) = orchestration_task {
        task.await?;
    }
    monitor_task.await??;
    #[cfg(unix)]
    {
        let terminal_cleanup = shutdown_io_runtime(Arc::clone(&io_runtime)).await;
        let runtime_cleanup = runtime_shutdown::close(runtime, &state).await;
        terminal_cleanup?;
        runtime_cleanup?;
    }
    #[cfg(not(unix))]
    state.shutdown_services().await?;
    result?;
    Ok(())
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
    let result = tokio::task::spawn_blocking(move || runtime.shutdown()).await?;
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

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => tokio::select! {
                result = tokio::signal::ctrl_c() => { if let Err(error) = result { eprintln!("failed to listen for Ctrl-C: {error}"); } }
                _ = terminate.recv() => {}
            },
            Err(error) => {
                eprintln!("failed to listen for SIGTERM: {error}");
                if let Err(error) = tokio::signal::ctrl_c().await {
                    eprintln!("failed to listen for Ctrl-C: {error}");
                }
            }
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for Ctrl-C: {error}");
    }
}
