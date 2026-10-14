//! Binary composition of retained account sessions. The supervisor is an
//! independently installed service; backend shutdown never takes its children.
use crate::account_registry::{
    AccountLaunch, AccountLauncher, AccountRegistry, AccountRegistryConfig, AccountSessionBroker,
    LaunchFuture,
};
use serde_json::json;
use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use studio_runtime_http::HttpState;
use studio_runtime_process::{
    BackendId, BackendIdentity, Generation, OperationId, ProviderHandle, ProviderLaunchSpec,
    child_priority::ChildPriority,
    provider_direct::DirectProviderTaskFactory,
    provider_seam::{OpenOptions, ProviderTaskFactory, SessionClock, Shutdown},
    provider_task::SupervisorProviderTaskFactory,
};
use studio_runtime_services::{
    account_driver::{AccountDriver, AccountStatus, AuthMetadata, AuthStatus, ProviderAuthReader},
    account_worker::{WorkerError, preparation::PreparationResources},
    claude_bridge::{BridgeSnapshot, ResolvedBridge},
    native_runtime::{NativeExecutableLease, NativeRuntime},
    provider_session_task::SessionTaskConfig,
    thread_prepare::{CatalogSnapshot, CatalogSource},
};
use studio_runtime_store::RuntimeStore;
use tokio::task::JoinHandle;

const REQUEST_TIMEOUT_MS: u64 = 60_000;
const SERVER_SOURCE_RELATIVE: &str = "workspaces/runtime/apps/server/src";
const CLAUDE_BRIDGE_RELATIVE: &str = "workspaces/providers/apps/claude-bridge";

/// Installed root shared by host-side runtime components.
#[derive(Clone, Debug)]
pub(crate) struct InstallRoot(PathBuf);

impl InstallRoot {
    pub(crate) fn workspace_root(&self) -> &Path {
        &self.0
    }
    fn server_source(&self) -> PathBuf {
        let packaged = self.0.join("scripts");
        if packaged.join("codex_python.py").is_file() {
            packaged
        } else {
            self.0.join(SERVER_SOURCE_RELATIVE)
        }
    }

    pub(crate) fn claude_bridge_dir(&self) -> PathBuf {
        let server_source = self.server_source();
        if server_source
            .file_name()
            .is_some_and(|name| name == "scripts")
        {
            server_source.join("claude_bridge")
        } else {
            self.0.join(CLAUDE_BRIDGE_RELATIVE)
        }
    }
}

/// Resolve the package root from the installed executable or this source build.
/// Python derives the equivalent root from codex_layout.py's installed scripts directory.
pub(crate) fn resolve_install_root() -> Result<InstallRoot, WorkerError> {
    let located = || {
        WorkerError::PreparationRefused(
            "runtime-server/install-root: expected codex_python.py and the codex_layout Claude bridge directory (scripts/claude_bridge when installed, workspaces/providers/apps/claude-bridge in a source checkout) beside the executable or under the source checkout".into(),
        )
    };
    let is_root = |path: &Path| {
        let root = InstallRoot(path.to_path_buf());
        (root.server_source().join("codex_python.py").is_file())
            && root.claude_bridge_dir().join("bridge.mjs").is_file()
    };

    if let Ok(executable) = std::env::current_exe() {
        for ancestor in executable.ancestors() {
            let candidate = if ancestor.file_name().is_some_and(|name| name == "bin") {
                ancestor.parent().unwrap_or(ancestor)
            } else {
                ancestor
            };
            if is_root(candidate) {
                return Ok(InstallRoot(candidate.to_path_buf()));
            }
        }
    }

    let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
    let source_root = source_root.canonicalize().map_err(|_| located())?;
    if is_root(&source_root) {
        return Ok(InstallRoot(source_root));
    }
    Err(located())
}

pub(crate) struct HostRuntime {
    shutdown: Shutdown,
    task: JoinHandle<Result<(), WorkerError>>,
    preparation_resources: Option<Arc<PreparationResources>>,
}

impl HostRuntime {
    #[cfg(test)]
    #[allow(dead_code)] // Integration tests use these constructors from a separate crate.
    pub async fn start(
        state: &mut HttpState,
        store: Arc<RuntimeStore>,
        accounts: Arc<AccountDriver>,
        native_path: Option<String>,
        config: AccountRegistryConfig,
    ) -> Result<Self, WorkerError> {
        Self::start_with_monitor_wake(
            state,
            store,
            accounts,
            native_path,
            Arc::new(tokio::sync::Notify::new()),
            AccountSessionBroker::new(),
            config,
        )
        .await
    }

    #[cfg(test)]
    pub async fn start_with_monitor_wake(
        state: &mut HttpState,
        store: Arc<RuntimeStore>,
        accounts: Arc<AccountDriver>,
        native_path: Option<String>,
        monitor_dispatch_wake: Arc<tokio::sync::Notify>,
        session_broker: AccountSessionBroker,
        config: AccountRegistryConfig,
    ) -> Result<Self, WorkerError> {
        Self::start_with_monitor_wake_and_native_runtime(
            state,
            store,
            accounts,
            native_path,
            monitor_dispatch_wake,
            session_broker,
            None,
            Arc::new(Mutex::new(BTreeMap::new())),
            config,
        )
        .await
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn start_with_monitor_wake_and_native_runtime(
        state: &mut HttpState,
        store: Arc<RuntimeStore>,
        accounts: Arc<AccountDriver>,
        native_path: Option<String>,
        monitor_dispatch_wake: Arc<tokio::sync::Notify>,
        session_broker: AccountSessionBroker,
        native_runtime: Option<Arc<NativeRuntime>>,
        session_binaries: Arc<Mutex<BTreeMap<String, crate::provider_ports::SessionExecutables>>>,
        config: AccountRegistryConfig,
    ) -> Result<Self, WorkerError> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| WorkerError::TaskFailed)?;
        let backend_id = data_encoding::HEXLOWER.encode(&nonce);
        studio_runtime_services::analytics_capture::start_token_rate_process(&store, &backend_id)
            .map_err(|_| WorkerError::TaskFailed)?;
        let identity = BackendIdentity {
            backend_id: BackendId(backend_id.clone()),
            state_dir: store.root().to_owned(),
        };
        let install_root = resolve_install_root()?;
        let python = resolve_python(
            std::env::var_os("CODEX_AGENTS_PYTHON").map(PathBuf::from),
            &install_root,
        )
        .await?;
        let catalog = state.model_catalog_handle();
        let resource_root = install_root.workspace_root().to_owned();
        let server_source = install_root.server_source();
        *state = state
            .clone()
            .with_vault_host_command(python.clone(), server_source.clone());
        let mut resources = tokio::task::spawn_blocking(move || {
            PreparationResources::bundled_at(
                String::new(),
                catalog,
                python,
                resource_root,
                server_source,
            )
        })
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
        resources.account_driver = Some(accounts.clone());
        let claude_bridge_dir = install_root.claude_bridge_dir();
        let resources = resources.with_account_session(state.account_session.clone());
        let preparation_resources = Arc::new(resources.clone());
        let callback_runtime = Arc::new(
            studio_runtime_services::callback_runtime::CallbackRuntime::new(
                store.clone(),
                session_broker.callback_reply_port(tokio::runtime::Handle::current()),
            ),
        );
        *state = install_host_voice_port(state.clone(), store.clone(), callback_runtime.clone())
            .with_tool_catalog(CatalogSnapshot {
                source: CatalogSource::Current,
                definitions: resources.tools.clone(),
                saved_thread_id: None,
                saved_digest: None,
            })
            .with_callback_runtime(callback_runtime);
        let launcher = Arc::new(HostLauncher {
            factory: SupervisorProviderTaskFactory::new(identity),
            supervisor_mode: std::env::var("CODEX_AGENTS_SUPERVISOR_MODE")
                .is_ok_and(|value| value == "1"),
            child_priority: ChildPriority::from_environment(),
            accounts,
            store: store.clone(),
            root: store.root().to_owned(),
            native_path,
            native_runtime,
            session_binaries,
            resources: Arc::clone(&preparation_resources),
            claude_bridge_dir,
            publication_sink: Arc::new(
                studio_runtime_services::notification_runtime_effects::RuntimeNotificationPostCommitSink {
                    token_rates: state.token_rate_handle(),
                    resources: state.resource_notifications_handle(),
                    analytics_store: Arc::clone(&store),
                    process_id: backend_id,
                },
            ),
            environment: std::env::vars().collect(),
        });
        let mut runtime = Self::with_launcher_broker(
            store,
            launcher,
            state.event_wake_handle(),
            monitor_dispatch_wake,
            session_broker,
            Some(Arc::clone(&preparation_resources)),
            config,
        )
        .await?;
        runtime.preparation_resources = Some(preparation_resources);
        Ok(runtime)
    }

    #[cfg(test)]
    #[allow(dead_code)] // Integration tests use these constructors from a separate crate.
    pub async fn with_launcher(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        wake: Arc<tokio::sync::Notify>,
        config: AccountRegistryConfig,
    ) -> Result<Self, WorkerError> {
        Self::with_launcher_and_monitor_wake(
            store,
            launcher,
            wake,
            Arc::new(tokio::sync::Notify::new()),
            config,
        )
        .await
    }

    #[cfg(test)]
    pub async fn with_launcher_and_monitor_wake(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        wake: Arc<tokio::sync::Notify>,
        monitor_dispatch_wake: Arc<tokio::sync::Notify>,
        config: AccountRegistryConfig,
    ) -> Result<Self, WorkerError> {
        Self::with_launcher_broker(
            store,
            launcher,
            wake,
            monitor_dispatch_wake,
            AccountSessionBroker::new(),
            None,
            config,
        )
        .await
    }

    async fn with_launcher_broker(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        wake: Arc<tokio::sync::Notify>,
        monitor_dispatch_wake: Arc<tokio::sync::Notify>,
        session_broker: AccountSessionBroker,
        preparation_resources: Option<Arc<PreparationResources>>,
        config: AccountRegistryConfig,
    ) -> Result<Self, WorkerError> {
        let registry = AccountRegistry::new_with_broker(
            store,
            launcher,
            monitor_dispatch_wake,
            session_broker,
            config,
        )
        .await?;
        let shutdown = Shutdown::default();
        let task = tokio::spawn(registry.run(wake, shutdown.clone()));
        Ok(Self {
            shutdown,
            task,
            preparation_resources,
        })
    }

    pub fn preparation_resources(&self) -> Option<Arc<PreparationResources>> {
        self.preparation_resources.clone()
    }

    pub fn stop_intake(&self) {
        self.shutdown.cancel();
    }

    pub fn shutdown_handle(&self) -> Shutdown {
        self.shutdown.clone()
    }

    pub async fn shutdown(self) -> Result<(), WorkerError> {
        self.shutdown.cancel();
        self.task.await.map_err(|_| WorkerError::TaskFailed)?
    }
}

/// The HTTP adapter shares the retained account session and committed callback
/// reply lane used by the account worker registry.
///
/// Lifecycle deletion integration hook: in the same transaction that tombstones
/// the agent and deletes its voice rows, call
/// `voice::provider::stage_delete_sessions(tx, agent_id)` before row cleanup.
/// After commit, pass the returned intent's `id` and `sessions` to
/// `voice::provider::end_deleted_sessions` using this installed voice port.
fn install_host_voice_port(
    state: HttpState,
    store: Arc<RuntimeStore>,
    callbacks: Arc<studio_runtime_services::callback_runtime::CallbackRuntime>,
) -> HttpState {
    match state.account_session.clone() {
        Some(account_session) => state.with_voice_request_port(Arc::new(
            studio_runtime_services::voice::account_session::AccountSessionVoicePort::new(
                store,
                account_session,
                callbacks,
            ),
        )),
        None => state,
    }
}

struct HostLauncher {
    factory: SupervisorProviderTaskFactory,
    supervisor_mode: bool,
    child_priority: ChildPriority,
    accounts: Arc<AccountDriver>,
    store: Arc<RuntimeStore>,
    root: PathBuf,
    native_path: Option<String>,
    native_runtime: Option<Arc<NativeRuntime>>,
    session_binaries: Arc<Mutex<BTreeMap<String, crate::provider_ports::SessionExecutables>>>,
    resources: Arc<PreparationResources>,
    claude_bridge_dir: PathBuf,
    publication_sink:
        Arc<dyn studio_runtime_services::notification_runtime_effects::NotificationPostCommitSink>,
    environment: BTreeMap<String, String>,
}

impl AccountLauncher for HostLauncher {
    fn launch(&self, account: String) -> LaunchFuture<'_> {
        Box::pin(async move {
            let accounts = self.accounts.clone();
            let key = account.clone();
            let (provider, home, isolated, claude_profile) =
                tokio::task::spawn_blocking(move || selected_account(&accounts, &key))
                    .await
                    .map_err(|_| WorkerError::TaskFailed)??;
            let mut resources = (*self.resources).clone();
            resources.account_home.clone_from(&home);
            let accounts = self.accounts.clone();
            let key = account.clone();
            resources.claude_account_pin =
                tokio::task::spawn_blocking(move || accounts.auth_pin(&key))
                    .await
                    .map_err(|_| WorkerError::TaskFailed)?
                    .map_err(|_| {
                        WorkerError::PreparationRefused("account-driver/unavailable".into())
                    })?;
            let (launch, executable, claude_executable, lease) = if let Some(profile) =
                claude_profile
            {
                let (launch, executable) = claude_launch_with_executable(
                    profile,
                    &self.claude_bridge_dir,
                    self.root.clone(),
                    self.environment.clone(),
                )?;
                (
                    launch,
                    None,
                    Some(executable.to_string_lossy().into_owned()),
                    None,
                )
            } else {
                let lease = if let Some(runtime) = &self.native_runtime {
                    let status = runtime.read_state().map_err(|_| {
                        WorkerError::PreparationRefused("native-runtime/status-read".into())
                    })?;
                    if status.as_ref().and_then(|state| state.selected()).is_some() {
                        Some(runtime.retain_selected().map_err(|_| {
                            WorkerError::PreparationRefused("native-runtime/selected-lease".into())
                        })?)
                    } else {
                        None
                    }
                } else {
                    None
                };
                let executable = lease
                    .as_ref()
                    .and_then(|lease| lease.path().to_str())
                    .map(str::to_owned)
                    .or_else(|| self.native_path.clone());
                let launch = codex_launch(
                    executable.as_deref(),
                    self.environment.clone(),
                    home,
                    isolated,
                    self.root.clone(),
                );
                (launch, executable, None, lease)
            };
            // Existing catalog refresh owns its bounded one-off child; both
            // HTTP reads and preparation observe this same retained service.
            let _catalog_status = self
                .resources
                .model_catalog
                .read(&account, &resources.account_home, &provider, false)
                .await;
            let clock = SessionClock::default();
            let handle = ProviderHandle::for_account(&account);
            let open = OpenOptions {
                clock: clock.clone(),
                launch: launch.clone(),
                expected_generation: None,
            };
            let (factory, generation): (Arc<dyn ProviderTaskFactory>, _) = if self.supervisor_mode {
                let prepared = self
                    .factory
                    .prepare(handle.clone(), open, Shutdown::default())
                    .await
                    .map_err(
                        studio_runtime_services::provider_session_task::SessionTaskError::from,
                    )?;
                let generation = prepared.generation();
                (Arc::new(prepared), generation)
            } else {
                let generation = next_direct_generation(&self.store, &account)?;
                let prepared = DirectProviderTaskFactory::new(self.child_priority)
                    .prepare(
                        handle.clone(),
                        OpenOptions {
                            expected_generation: Some(generation),
                            ..open
                        },
                        &Shutdown::default(),
                    )
                    .map_err(
                        studio_runtime_services::provider_session_task::SessionTaskError::from,
                    )?;
                debug_assert_eq!(generation, prepared.generation());
                (Arc::new(prepared), generation)
            };
            let factory = match lease {
                Some(lease) => Arc::new(LeasedProviderTaskFactory {
                    inner: factory,
                    _lease: lease,
                }) as Arc<dyn ProviderTaskFactory>,
                None => factory,
            };
            if let Ok(mut binaries) = self.session_binaries.lock() {
                binaries.insert(
                    account.clone(),
                    crate::provider_ports::SessionExecutables {
                        generation: generation.0,
                        codex: executable,
                        claude: claude_executable,
                    },
                );
            }
            Ok(AccountLaunch {
                factory,
                options: OpenOptions {
                    clock,
                    launch,
                    expected_generation: Some(generation),
                },
                config: SessionTaskConfig {
                    initialize_params: json!({"clientInfo":{"name":"codex_agents_canvas","version":"1.0.0"},"capabilities":{"experimentalApi":true}}),
                    generation,
                    claude: provider == "claude",
                    timeout_ms: REQUEST_TIMEOUT_MS,
                    initialize_operation: OperationId(format!(
                        "account:{account}:initialize:{}",
                        generation.0
                    )),
                    request_id_prefix: None,
                    recovered: vec![],
                },
                resources,
                publication_sink: Arc::clone(&self.publication_sink),
            })
        })
    }
}

/// Keeps the selected immutable native bundle alive as long as the retained
/// account session factory can still launch or serve that provider session.
struct LeasedProviderTaskFactory {
    inner: Arc<dyn ProviderTaskFactory>,
    _lease: NativeExecutableLease,
}

impl ProviderTaskFactory for LeasedProviderTaskFactory {
    fn open(
        &self,
        handle: ProviderHandle,
        options: OpenOptions,
        shutdown: Shutdown,
    ) -> studio_runtime_process::provider_seam::OpenFuture<'_> {
        self.inner.open(handle, options, shutdown)
    }
}

async fn resolve_python(
    configured: Option<PathBuf>,
    install_root: &InstallRoot,
) -> Result<PathBuf, WorkerError> {
    let invalid = || {
        WorkerError::PreparationRefused(
            "host-runtime/python-interpreter: set CODEX_AGENTS_PYTHON to an absolute interpreter or install Python 3.11+ with the server API dependencies".into(),
        )
    };
    if let Some(path) = configured {
        return if path.is_absolute() && path.is_file() {
            Ok(path)
        } else {
            Err(invalid())
        };
    }
    let bootstrap = install_root.server_source().join("codex_python.py");
    if !bootstrap.is_file() {
        return Err(WorkerError::PreparationRefused(format!(
            "host-runtime/python-bootstrap: installed source file is missing at {}",
            bootstrap.display()
        )));
    }
    let mut command = tokio::process::Command::new("python3");
    command.arg("-B").arg(&bootstrap).kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
        .await
        .map_err(|_| invalid())?
        .map_err(|_| invalid())?;
    if !output.status.success() {
        let detail = String::from_utf8_lossy(&output.stderr);
        return Err(WorkerError::PreparationRefused(format!(
            "host-runtime/python-bootstrap: {} exited unsuccessfully: {}",
            bootstrap.display(),
            detail.trim().chars().take(800).collect::<String>()
        )));
    }
    let path = PathBuf::from(
        std::str::from_utf8(&output.stdout)
            .map_err(|_| invalid())?
            .trim(),
    );
    if path.is_absolute() {
        Ok(path)
    } else {
        Err(invalid())
    }
}

pub(crate) fn codex_launch(
    native_path: Option<&str>,
    mut environment: BTreeMap<String, String>,
    home: String,
    isolated: bool,
    root: PathBuf,
) -> ProviderLaunchSpec {
    let mut command = vec![
        native_path.unwrap_or("codex").to_owned(),
        "app-server".into(),
        "--listen".into(),
        "stdio://".into(),
    ];
    environment.insert("CODEX_HOME".into(), home);
    if isolated {
        command.extend(["-c".into(), "cli_auth_credentials_store=\"file\"".into()]);
        environment.remove("OPENAI_API_KEY");
        environment.remove("CODEX_API_KEY");
    }
    ProviderLaunchSpec::new(command, environment, root).with_inherited_cwd()
}

#[cfg(test)]
#[allow(clippy::items_after_test_module, clippy::unwrap_used)]
mod install_root_tests {
    use super::InstallRoot;
    use std::{
        fs,
        path::{Path, PathBuf},
        process::Command,
    };

    fn python_layout(source: &Path) -> (PathBuf, PathBuf) {
        let output = Command::new("python3")
            .args([
                "-B",
                "-c",
                "from codex_layout import SERVER_SOURCE_ROOT, CLAUDE_BRIDGE_ROOT; print(f'{SERVER_SOURCE_ROOT}\\n{CLAUDE_BRIDGE_ROOT}')",
            ])
            .env("PYTHONPATH", source)
            .output()
            .expect("python3 should be available for layout parity");
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).unwrap();
        let mut lines = stdout.lines();
        (
            PathBuf::from(lines.next().expect("Python server source root")),
            PathBuf::from(lines.next().expect("Python Claude bridge root")),
        )
    }

    #[test]
    fn bridge_and_server_source_match_python_layout_in_both_install_modes() {
        let source_checkout = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../..")
            .canonicalize()
            .unwrap();
        let python_source = source_checkout.join("workspaces/runtime/apps/server/src");
        let source_layout = InstallRoot(source_checkout.clone());
        let (python_server_source, python_bridge) = python_layout(&python_source);
        assert_eq!(source_layout.server_source(), python_server_source);
        assert_eq!(source_layout.claude_bridge_dir(), python_bridge);

        let temporary = tempfile::tempdir().unwrap();
        let packaged_root = temporary.path().join("workspace");
        let packaged_scripts = packaged_root.join("scripts");
        let packaged_bridge = packaged_scripts.join("claude_bridge");
        fs::create_dir_all(&packaged_bridge).unwrap();
        fs::copy(
            python_source.join("codex_layout.py"),
            packaged_scripts.join("codex_layout.py"),
        )
        .unwrap();
        fs::write(packaged_scripts.join("codex_python.py"), "").unwrap();
        fs::write(packaged_bridge.join("bridge.mjs"), "").unwrap();
        let packaged_layout = InstallRoot(packaged_root);
        let (python_server_source, python_bridge) = python_layout(&packaged_scripts);
        assert_eq!(packaged_layout.server_source(), python_server_source);
        assert_eq!(packaged_layout.claude_bridge_dir(), python_bridge);
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod voice_composition_tests {
    use super::install_host_voice_port;
    use axum::{
        body::{Body, to_bytes},
        extract::ConnectInfo,
        http::{Request, StatusCode},
    };
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        fs,
        path::Path,
        sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        },
    };
    use studio_runtime_http::{HttpState, router};
    use studio_runtime_process::{
        EventCursor, Generation, OperationId, ProviderHandle, ProviderLaunchSpec,
        provider_seam::{
            AdapterCommand, AttachmentInfo, AttachmentStatus, CommandReceipt, InitializationStatus,
            JournalEntry, JournalPayload, LANE_CAPACITY, OpenFuture, OpenOptions,
            ProviderAttachment, ProviderTaskFactory, SessionClock, Shutdown, TaskExit,
        },
    };
    use studio_runtime_providers::{Frame, RequestId, ResponseBody};
    use studio_runtime_services::{
        account_driver::AccountDriver,
        account_session_runtime::{
            AccountSessionError, AccountSessionRuntime, ConnectionScope, NotSentReason,
            ProviderRequest, ProviderResponse, RequestCertainty, RetainedAccountSession,
            SessionFuture, SessionReceipt as AccountSessionReceipt,
        },
        account_worker::{
            AccountHandle, StoreBridge,
            preparation::PreparationResources,
            retained::{PortError, RequestResult, SessionRequest},
            start_account_worker_with_preparation,
        },
        callback_runtime::{CallbackReplyPort, CallbackRuntime, ReplyReceipt},
        model_catalog::ModelCatalogService,
        provider_callbacks::{CallbackError, Reply},
        provider_session_task::SessionTaskConfig,
    };
    use studio_runtime_store::RuntimeStore;
    use tokio::sync::Notify;
    use tower::ServiceExt;

    struct WorkerSessionAdapter {
        port: studio_runtime_services::account_worker::retained::RetainedSessionPort,
    }

    impl RetainedAccountSession for WorkerSessionAdapter {
        fn current_scope(&self, account: &str) -> Option<ConnectionScope> {
            let scope = self.port.scope().clone();
            (scope.account == account).then_some(scope)
        }

        fn request(&self, request: ProviderRequest) -> SessionFuture {
            let port = self.port.clone();
            Box::pin(async move {
                let scope = port.scope().clone();
                if request.account != scope.account {
                    return Err(AccountSessionError::Refused(
                        "Provider session identity changed",
                    ));
                }
                let operation = request.operation.clone();
                let retained = port
                    .request(SessionRequest {
                        scope,
                        expected_scope: request.expected_scope,
                        operation,
                        actor: request.actor,
                        transfer: None,
                        deleted_actor_session: request.deleted_actor_session,
                        method: request.method,
                        params: request.params,
                        deadline: request.deadline,
                    })
                    .await
                    .map_err(|error| match error {
                        PortError::Unknown => AccountSessionError::Unknown,
                        PortError::Capacity => AccountSessionError::Persistence,
                        PortError::Identity
                        | PortError::ScopeMismatch
                        | PortError::Invalid
                        | PortError::NotSent
                        | PortError::Deadline => {
                            AccountSessionError::Refused("Provider request was refused")
                        }
                    })?;
                let (certainty, result) = match retained.result {
                    RequestResult::Reply(value) => (
                        RequestCertainty::Written,
                        Some(ProviderResponse::Reply(value)),
                    ),
                    RequestResult::Rejected(value) => (
                        RequestCertainty::Written,
                        Some(ProviderResponse::Rejected(value)),
                    ),
                    RequestResult::NotSent => (RequestCertainty::NotSent, None),
                    RequestResult::Unknown => (RequestCertainty::Unknown, None),
                };
                Ok(AccountSessionReceipt {
                    scope: retained.scope,
                    operation: retained.operation,
                    ticket: Some(retained.ticket),
                    certainty,
                    result,
                    not_sent_reason: (certainty == RequestCertainty::NotSent)
                        .then_some(NotSentReason::WorkerNotSent),
                })
            })
        }
    }

    struct UnusedCallbackReplyPort;

    impl CallbackReplyPort for UnusedCallbackReplyPort {
        fn deliver(
            &self,
            _reply: &Reply,
            _deadline: std::time::Duration,
        ) -> Result<ReplyReceipt, CallbackError> {
            Err(CallbackError::Dependency(
                "test callback reply lane is unused",
            ))
        }
    }

    struct VoiceFactory {
        stop_writes: Arc<AtomicUsize>,
        unknown_stop_at: Option<usize>,
        unknown_stop_written: Arc<Notify>,
    }

    impl ProviderTaskFactory for VoiceFactory {
        fn open(
            &self,
            handle: ProviderHandle,
            _options: OpenOptions,
            shutdown: Shutdown,
        ) -> OpenFuture<'_> {
            let generation = Generation(42);
            let (commands, mut inbox) = tokio::sync::mpsc::channel(LANE_CAPACITY);
            let (events, journal) = tokio::sync::mpsc::channel(LANE_CAPACITY);
            let (status, receiver) = tokio::sync::watch::channel(AttachmentStatus {
                handle: handle.clone(),
                open_attempted: true,
                info: Some(AttachmentInfo {
                    handle: handle.clone(),
                    generation,
                    pid: 42,
                    acknowledged: EventCursor(0),
                    resumed: false,
                }),
                initialization: InitializationStatus::AwaitingReply,
            });
            let mut stop = shutdown.subscribe();
            let stop_writes = Arc::clone(&self.stop_writes);
            let unknown_stop_at = self.unknown_stop_at;
            let unknown_stop_written = Arc::clone(&self.unknown_stop_written);
            let task = tokio::spawn(async move {
                let mut cursor = 0_u64;
                loop {
                    let command = tokio::select! {
                        _ = stop.changed() => break,
                        command = inbox.recv() => command,
                    };
                    let Some(command) = command else { break };
                    match command {
                        AdapterCommand::Write {
                            write,
                            reply,
                            observed,
                        } => {
                            let message = write.message();
                            let method = message
                                .get("method")
                                .and_then(Value::as_str)
                                .unwrap_or_default();
                            if method == "initialize" {
                                let _ignored =
                                    reply.send(CommandReceipt::CachedInitialize(json!({})));
                                let _ = observed.await;
                                continue;
                            }
                            let _ignored = reply.send(CommandReceipt::Written {
                                remote_id: None,
                                duplicate: false,
                            });
                            if observed.await.is_err() {
                                break;
                            }
                            let Some(id) = message
                                .get("id")
                                .cloned()
                                .and_then(|id| serde_json::from_value::<RequestId>(id).ok())
                            else {
                                continue;
                            };
                            if method == "thread/realtime/stop" {
                                let stop_number = stop_writes.fetch_add(1, Ordering::SeqCst) + 1;
                                if unknown_stop_at == Some(stop_number) {
                                    unknown_stop_written.notify_one();
                                    continue;
                                }
                            }
                            let result = match method {
                                "account/read" => json!({"account":{"type":"chatgpt"}}),
                                _ => json!({}),
                            };
                            cursor += 1;
                            if events
                                .send(JournalEntry {
                                    received_at_ms: cursor,
                                    cursor: EventCursor(cursor),
                                    generation,
                                    payload: JournalPayload::Frame(Frame::Response {
                                        id,
                                        body: ResponseBody::Result(result),
                                        fields: Default::default(),
                                    }),
                                })
                                .await
                                .is_err()
                            {
                                break;
                            }
                        }
                        AdapterCommand::Ack { cursor, reply } => {
                            let _ignored = reply.send(CommandReceipt::Acknowledged(cursor));
                        }
                    }
                }
                drop(status);
                TaskExit {
                    result: Ok(()),
                    cleanup_error: None,
                    recovery_needed: None,
                }
            });
            Box::pin(async move {
                Ok(ProviderAttachment::from_task(
                    commands, journal, receiver, shutdown, task,
                ))
            })
        }
    }

    async fn start_voice_worker(
        directory: &Path,
        store: Arc<RuntimeStore>,
        factory: Arc<VoiceFactory>,
    ) -> (
        AccountHandle,
        Arc<AccountSessionRuntime>,
        Arc<CallbackRuntime>,
    ) {
        let bridge = StoreBridge::new(store.clone()).await.unwrap();
        let resources = PreparationResources {
            instructions: String::new(),
            progress_template: String::new(),
            thread_config: Default::default(),
            tools: Vec::new(),
            legacy_tools: Vec::new(),
            skills_root: directory.to_path_buf(),
            role_guidance: BTreeMap::new(),
            model_catalog: ModelCatalogService::new(None),
            account_home: String::new(),
            server_source_root: directory.to_path_buf(),
            python_executable: "/usr/bin/python3".into(),
            claude_account_pin: serde_json::Value::Null,
            account_driver: None,
            account_session: None,
        };
        let worker = start_account_worker_with_preparation(
            bridge,
            "voice-account".into(),
            factory,
            OpenOptions {
                clock: SessionClock::default(),
                launch: ProviderLaunchSpec::new(
                    vec!["fake".into()],
                    BTreeMap::new(),
                    "/tmp".into(),
                ),
                expected_generation: None,
            },
            SessionTaskConfig {
                initialize_params: json!({}),
                generation: Generation(42),
                claude: false,
                timeout_ms: 2_000,
                initialize_operation: OperationId("initialize:voice-account".into()),
                request_id_prefix: Some("voice-account".into()),
                recovered: Vec::new(),
            },
            resources,
        )
        .await
        .unwrap();
        let account_root = directory.join("accounts");
        fs::create_dir_all(&account_root).unwrap();
        let registry_path = account_root.join("registry.json");
        if !registry_path.exists() {
            fs::write(
                &registry_path,
                json!({
                    "version":1,"defaultAccountKey":"voice-account",
                    "accounts":{"voice-account":{"id":"voice-account","home":directory,"label":"voice","source":"test","status":"signedOut"}},
                    "logins":{},"deleteReceipts":{}
                })
                .to_string(),
            )
            .unwrap();
        }
        let driver = Arc::new(AccountDriver::open(registry_path).unwrap());
        let retained: Arc<dyn RetainedAccountSession> = Arc::new(WorkerSessionAdapter {
            port: worker.retained_session(),
        });
        let session = Arc::new(AccountSessionRuntime::new(
            driver,
            Some(retained),
            tokio::runtime::Handle::current(),
            account_root,
            directory.to_path_buf(),
            directory.to_path_buf(),
        ));
        let callbacks = Arc::new(CallbackRuntime::new(
            store,
            Arc::new(UnusedCallbackReplyPort),
        ));
        (worker, session, callbacks)
    }

    #[tokio::test]
    async fn composed_runtime_server_routes_voice_through_the_shared_account_session() {
        let directory = tempfile::tempdir().unwrap();
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).unwrap());
        let agent_record = json!({
            "id":"voice-composed-agent", "rootId":"voice-composed-agent", "epoch":3,
            "status":"idle", "isLead":true, "threadId":"voice-thread", "accountKey":"voice-account"
        })
        .to_string();
        store
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    ("voice-composed-agent", agent_record.as_str()),
                )?;
                Ok(())
            })
            .unwrap();

        let factory = Arc::new(VoiceFactory {
            stop_writes: Arc::new(AtomicUsize::new(0)),
            unknown_stop_at: Some(2),
            unknown_stop_written: Arc::new(Notify::new()),
        });
        let (mut worker, session, callbacks) =
            start_voice_worker(directory.path(), store.clone(), Arc::clone(&factory)).await;
        let mut state = HttpState::new(
            "token".into(),
            "workspace".into(),
            None,
            directory.path().display().to_string(),
            4620,
            None,
            Some(store.clone()),
            None,
        );
        state.account_session = Some(session);
        let app = router(install_host_voice_port(
            state,
            Arc::clone(&store),
            callbacks,
        ))
        .unwrap();
        let mut request = Request::builder()
            .method("POST")
            .uri("/api/voice/start")
            .header("host", "127.0.0.1:4620")
            .header("x-canvas-token", "token")
            .header("content-type", "application/json")
            .body(Body::from(
                r#"{"agent":"voice-composed-agent","session_id":"voice-session","sdp":"v=0\r\n"}"#,
            ))
            .unwrap();
        request.extensions_mut().insert(ConnectInfo(
            "127.0.0.1:32100".parse::<std::net::SocketAddr>().unwrap(),
        ));
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 100_000).await.unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response["session_id"], "voice-session");
        assert_eq!(response["state"], "connecting");

        // The deletion transaction persists stop authorization before removing
        // voice rows, matching Python's delete-before-post-commit-stop ordering.
        let delete_intent = store
            .transaction_with_error(|transaction| -> Result<_, studio_runtime_services::voice::provider::VoiceProviderError> {
                transaction.execute(
                    "INSERT INTO voice_sessions(id,agent,created,native_thread,account_key,connection_id,state) \
                     SELECT 'voice-session-unknown',agent,created+1,native_thread,account_key,connection_id,state \
                     FROM voice_sessions WHERE id='voice-session' AND agent='voice-composed-agent'",
                    [],
                )?;
                transaction.execute(
                    "UPDATE runtime_agents SET record=json_set(record,'$.deletedAt',1) WHERE id='voice-composed-agent'",
                    [],
                )?;
                let intent = studio_runtime_services::voice::provider::stage_delete_sessions(
                    transaction,
                    "voice-composed-agent",
                )?;
                transaction.execute(
                    "DELETE FROM voice_sessions WHERE agent='voice-composed-agent'",
                    [],
                )?;
                Ok(intent)
            })
            .unwrap();
        assert_eq!(delete_intent.sessions.len(), 2);

        // Simulate a crash after the transaction commits but before its caller
        // can issue the provider stops. A restarted worker sends the two saved
        // operations once; the second request becomes Unknown when the worker
        // is shut down before its provider response.
        worker.shutdown().await.unwrap();
        let (mut post_delete_worker, post_delete_session, post_delete_callbacks) =
            start_voice_worker(directory.path(), Arc::clone(&store), Arc::clone(&factory)).await;
        let delete_provider: Arc<dyn studio_runtime_services::voice::runtime::VoiceRequestPort> =
            Arc::new(
                studio_runtime_services::voice::account_session::AccountSessionVoicePort::new(
                    Arc::clone(&store),
                    post_delete_session,
                    post_delete_callbacks,
                ),
            );
        let unknown_written = factory.unknown_stop_written.notified();
        tokio::pin!(unknown_written);
        let intent_for_delivery = delete_intent.clone();
        let delivery = tokio::spawn(async move {
            studio_runtime_services::voice::provider::end_deleted_sessions(
                delete_provider,
                intent_for_delivery.id,
                intent_for_delivery.sessions,
                2.0,
            )
            .await
        });
        unknown_written.await;
        post_delete_worker.shutdown().await.unwrap();
        let receipts = delivery.await.unwrap().unwrap();
        assert_eq!(receipts.len(), 2);
        assert_eq!(
            receipts[0].certainty,
            studio_runtime_services::voice::runtime::VoiceWriteCertainty::Written
        );
        assert_eq!(
            receipts[1].certainty,
            studio_runtime_services::voice::runtime::VoiceWriteCertainty::Unknown
        );

        // Recovery replays the known receipt and retains Unknown without another
        // provider write, even after another worker restart.
        let (mut recovered_worker, recovered_session, recovered_callbacks) =
            start_voice_worker(directory.path(), Arc::clone(&store), Arc::clone(&factory)).await;
        let recovered_provider: Arc<dyn studio_runtime_services::voice::runtime::VoiceRequestPort> =
            Arc::new(
                studio_runtime_services::voice::account_session::AccountSessionVoicePort::new(
                    Arc::clone(&store),
                    recovered_session,
                    recovered_callbacks,
                ),
            );
        let replay = studio_runtime_services::voice::provider::end_deleted_sessions(
            recovered_provider,
            delete_intent.id,
            delete_intent.sessions,
            3.0,
        )
        .await
        .unwrap();
        assert_eq!(replay, receipts);
        assert_eq!(factory.stop_writes.load(Ordering::SeqCst), 2);
        recovered_worker.shutdown().await.unwrap();
    }
}

fn next_direct_generation(store: &RuntimeStore, account: &str) -> Result<Generation, WorkerError> {
    store.transaction_with(|tx| {
        let invalid = || {
            WorkerError::PreparationRefused("account-worker/direct-generation".into())
        };
        let counter_id = format!("account-worker:direct-generation:{account}:next");
        let counter_exists = tx.query_row(
            "SELECT EXISTS(SELECT 1 FROM runtime_operation_receipts WHERE id=?1)",
            [&counter_id],
            |row| row.get::<_, bool>(0),
        )?;
        let mut latest = if counter_exists {
            let raw = tx.query_row(
                "SELECT result FROM runtime_operation_receipts WHERE id=?1",
                [&counter_id],
                |row| row.get::<_, String>(0),
            )?;
            serde_json::from_str::<u64>(&raw).map_err(|_| invalid())?
        } else {
            0
        };
        for kind in ["cursor", "journal", "callback", "recovery", "account"] {
            let prefix = format!("account-worker:{kind}:{account}:");
            let upper = format!("{prefix}~");
            let mut query = tx.prepare(
                "SELECT id,result FROM runtime_operation_receipts WHERE id>=?1 AND id<?2",
            )?;
            let rows = query
                .query_map((prefix.as_str(), upper.as_str()), |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for (id, raw) in rows {
                let value: serde_json::Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
                let generation = if kind == "account" {
                    value
                        .pointer("/request/generation")
                        .or_else(|| value.get("generation"))
                        .and_then(serde_json::Value::as_u64)
                } else {
                    id.strip_prefix(&prefix)
                        .and_then(|tail| tail.split(':').next())
                        .and_then(|number| number.parse::<u64>().ok())
                };
                if let Some(generation) = generation {
                    latest = latest.max(generation);
                }
            }
        }
        let next = latest.checked_add(1).ok_or_else(invalid)?;
        let result = serde_json::to_string(&next).map_err(|_| invalid())?;
        tx.execute(
            "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,'direct-provider-generation',?2) ON CONFLICT(id) DO UPDATE SET result=excluded.result",
            (counter_id.as_str(), result.as_str()),
        )?;
        Ok(Generation(next))
    })
}

pub(crate) fn account_home<R: ProviderAuthReader>(
    accounts: &AccountDriver<R>,
    key: &str,
) -> Result<(String, bool), WorkerError> {
    let initial = accounts
        .snapshot()
        .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    let initial_profile = initial
        .accounts
        .iter()
        .find(|profile| profile["id"] == key)
        .ok_or_else(|| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    if initial_profile
        .get("provider")
        .and_then(|value| value.as_str())
        .unwrap_or("codex")
        != "codex"
    {
        return Err(WorkerError::PreparationRefused(
            "provider-session/claude-runtime-launch".into(),
        ));
    }
    if accounts
        .get(key)
        .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?
        != AccountStatus::Ready
    {
        return Err(WorkerError::PreparationRefused(
            "account-driver/unavailable".into(),
        ));
    }
    let snapshot = accounts
        .snapshot()
        .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    let profile = snapshot
        .accounts
        .iter()
        .find(|profile| profile["id"] == key)
        .ok_or_else(|| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    if profile
        .get("provider")
        .and_then(|value| value.as_str())
        .unwrap_or("codex")
        != "codex"
    {
        return Err(WorkerError::PreparationRefused(
            "provider-session/claude-runtime-launch".into(),
        ));
    }
    if profile["disconnected"] == true || profile["deleted"] == true || profile["status"] != "ready"
    {
        return Err(WorkerError::PreparationRefused(
            "account-driver/unavailable".into(),
        ));
    }
    let home = profile["home"]
        .as_str()
        .filter(|home| !home.is_empty())
        .ok_or_else(|| WorkerError::PreparationRefused("account-driver/profile-home".into()))?
        .to_owned();
    Ok((home, key != "default"))
}

fn selected_account(
    accounts: &AccountDriver,
    key: &str,
) -> Result<(String, String, bool, Option<serde_json::Value>), WorkerError> {
    let snapshot = accounts
        .snapshot()
        .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    let profile = snapshot
        .accounts
        .iter()
        .find(|profile| profile["id"] == key)
        .ok_or_else(|| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    let provider = profile
        .get("provider")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("codex")
        .to_owned();
    if provider == "codex" {
        let (home, isolated) = account_home(accounts, key)?;
        return Ok((provider, home, isolated, None));
    }
    if provider != "claude" {
        return Err(WorkerError::PreparationRefused(
            "provider-session/unsupported-provider".into(),
        ));
    }
    let profile = accounts
        .claude_profile(key)
        .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    let home = profile
        .get("home")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| WorkerError::PreparationRefused("account-driver/profile-home".into()))?
        .to_owned();
    Ok((provider, home, false, Some(profile)))
}

#[cfg(test)]
pub(crate) fn claude_launch(
    profile: serde_json::Value,
    bridge_dir: &Path,
    root: PathBuf,
    environment: BTreeMap<String, String>,
) -> Result<ProviderLaunchSpec, WorkerError> {
    claude_launch_with_executable(profile, bridge_dir, root, environment).map(|(launch, _)| launch)
}

fn claude_launch_with_executable(
    profile: serde_json::Value,
    bridge_dir: &Path,
    root: PathBuf,
    mut environment: BTreeMap<String, String>,
) -> Result<(ProviderLaunchSpec, PathBuf), WorkerError> {
    let account_id = profile
        .get("accountId")
        .and_then(serde_json::Value::as_str)
        .filter(|value| value.starts_with("claude:"))
        .ok_or_else(|| WorkerError::PreparationRefused("claude/account-identity".into()))?;
    let email = profile
        .get("email")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| WorkerError::PreparationRefused("claude/account-identity".into()))?;
    let credential_identity = profile
        .get("_credentialIdentity")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| WorkerError::PreparationRefused("claude/account-identity".into()))?;
    if account_id != format!("claude:{email}") || credential_identity != account_id {
        return Err(WorkerError::PreparationRefused(
            "claude/account-identity".into(),
        ));
    }
    let options = profile
        .get("claudeOptions")
        .unwrap_or(&serde_json::Value::Null);
    let configured_node = environment.get("STUDIO_NODE_BIN").map(String::as_str);
    let node = if let Some(configured) = configured_node {
        executable(configured, environment.get("PATH").map(String::as_str))
    } else {
        executable("node", environment.get("PATH").map(String::as_str)).or_else(|| {
            let home = environment.get("HOME").map(PathBuf::from);
            [
                PathBuf::from("/opt/homebrew/bin/node"),
                PathBuf::from("/usr/local/bin/node"),
            ]
            .into_iter()
            .chain(home.into_iter().flat_map(|home| {
                [
                    home.join(".local/share/fnm/aliases/default/bin/node"),
                    home.join("Library/Application Support/fnm/aliases/default/bin/node"),
                    home.join(".volta/bin/node"),
                    home.join(".local/share/mise/shims/node"),
                ]
            }))
            .find_map(|path| executable(&path.to_string_lossy(), None))
        })
    }
    .ok_or_else(|| WorkerError::PreparationRefused("claude/node-unavailable".into()))?;
    let binary_path = options
        .get("binaryPath")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| environment.get("STUDIO_CLAUDE_BIN").map(String::as_str));
    let claude = match binary_path {
        Some(binary) => executable(binary, environment.get("PATH").map(String::as_str)),
        None => executable("claude", environment.get("PATH").map(String::as_str)).or_else(|| {
            environment
                .get("HOME")
                .map(|home| PathBuf::from(home).join(".local/bin/claude"))
                .and_then(|path| executable(&path.to_string_lossy(), None))
        }),
    }
    .ok_or_else(|| WorkerError::PreparationRefused("claude/cli-unavailable".into()))?;
    let bridge = bridge_dir.join("bridge.mjs");
    let sdk_present = bridge_dir
        .join("node_modules/@anthropic-ai/claude-agent-sdk/package.json")
        .is_file();
    let metadata = AuthMetadata {
        status: AuthStatus::Ready,
        account_id: Some(account_id.to_owned()),
        email: Some(email.to_owned()),
        plan: profile
            .get("plan")
            .and_then(serde_json::Value::as_str)
            .map(str::to_owned),
        credential_identity: Some(credential_identity.to_owned()),
        error: None,
        error_kind: None,
    };
    let snapshot = BridgeSnapshot::from_record(profile)
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
    let launch = snapshot
        .launch(
            ResolvedBridge {
                node,
                executable: claude.clone(),
                bridge,
                root,
                sdk_present,
            },
            &metadata,
            std::mem::take(&mut environment),
        )
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
    Ok((launch, claude))
}

fn executable(configured: &str, path: Option<&str>) -> Option<PathBuf> {
    let candidate = PathBuf::from(configured);
    let candidate = if candidate.is_absolute() || candidate.components().count() > 1 {
        candidate
    } else {
        env::split_paths(path?)
            .map(|directory| directory.join(candidate.clone()))
            .find(|path| path.is_file() && is_executable(path))?
    };
    if candidate.is_file() && is_executable(&candidate) {
        candidate.canonicalize().ok()
    } else {
        None
    }
}

fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}
