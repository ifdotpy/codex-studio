//! Binary composition of retained account sessions. The supervisor is an
//! independently installed service; backend shutdown never takes its children.
use crate::account_registry::{
    AccountLaunch, AccountLauncher, AccountRegistry, AccountRegistryConfig, AccountSessionBroker,
    LaunchFuture,
};
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use process_wrap::std::{ChildWrapper, CommandWrap};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    env,
    io::{BufRead, Read, Write},
    path::{Path, PathBuf},
    process::Stdio,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
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
    account_driver::{AccountDriver, AccountStatus, ProviderAuthReader},
    account_worker::{WorkerError, preparation::PreparationResources},
    native_runtime::{NativeExecutableLease, NativeRuntime},
    provider_adapter::{HostLaunch, ProviderKind, ProviderLaunch, adapter, is_executable},
    provider_session_task::SessionTaskConfig,
    thread_prepare::{CatalogSnapshot, CatalogSource},
};
use studio_runtime_store::RuntimeStore;
use tokio::task::JoinHandle;
use tokio::time::timeout;

const REQUEST_TIMEOUT_MS: u64 = 60_000;
const HOST_RUNTIME_SHUTDOWN_WAIT: Duration = Duration::from_secs(5);
// The supervisor detach must finish, and report, inside this bound.
const _: () = assert!(
    studio_runtime_process::provider_task::SUPERVISOR_DETACH_TIMEOUT.as_millis()
        < HOST_RUNTIME_SHUTDOWN_WAIT.as_millis()
);
const CLAUDE_BRIDGE_RELATIVE: &str = "workspaces/providers/apps/claude-bridge";
/// Where `native/linux-vm/build.ts` writes the helper in a source checkout.
const LINUX_VM_HELPER_RELATIVE: &str =
    "workspaces/client/apps/desktop/native/linux-vm/studio-linux-vm";

/// Installed root shared by host-side runtime components.
#[derive(Clone, Debug)]
pub struct InstallRoot(PathBuf);

impl InstallRoot {
    pub fn workspace_root(&self) -> &Path {
        &self.0
    }
    /// The native Linux VM helper: `CODEX_LINUX_VM_HELPER`, the packaged
    /// `Resources/studio-linux-vm`, or the build in a source checkout.
    pub fn linux_vm_helper(&self) -> PathBuf {
        if let Some(helper) =
            env::var_os("CODEX_LINUX_VM_HELPER").filter(|helper| !helper.is_empty())
        {
            return PathBuf::from(helper);
        }
        let packaged = self
            .0
            .parent()
            .map(|resources| resources.join("studio-linux-vm"));
        match packaged {
            Some(packaged) if packaged.is_file() => packaged,
            _ => self.0.join(LINUX_VM_HELPER_RELATIVE),
        }
    }
    pub fn claude_bridge_dir(&self) -> PathBuf {
        let packaged = self.0.join("scripts/claude_bridge");
        if packaged.join("bridge.mjs").is_file() {
            packaged
        } else {
            self.0.join(CLAUDE_BRIDGE_RELATIVE)
        }
    }
}

/// Resolve the package root from the executable, or the source build in debug mode.
pub fn resolve_install_root() -> Result<InstallRoot, WorkerError> {
    resolve_install_root_from_executable(std::env::current_exe().ok().as_deref())
}

fn resolve_install_root_from_executable(
    executable: Option<&Path>,
) -> Result<InstallRoot, WorkerError> {
    let located = || {
        WorkerError::PreparationRefused(
            "runtime-server/install-root: expected the Claude bridge directory (scripts/claude_bridge when installed, workspaces/providers/apps/claude-bridge in a source checkout) beside the executable or under the source checkout".into(),
        )
    };
    let is_root = |path: &Path| {
        let root = InstallRoot(path.to_path_buf());
        root.claude_bridge_dir().join("bridge.mjs").is_file()
    };

    if let Some(executable) = executable {
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

    #[cfg(debug_assertions)]
    {
        let source_root = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..");
        let source_root =
            studio_runtime_store::path_identity::CanonicalPath::existing(&source_root)
                .map_err(|_| located())?;
        if is_root(&source_root) {
            return Ok(InstallRoot(source_root.to_path_buf()));
        }
    }
    Err(located())
}

pub struct HostRuntime {
    shutdown: Shutdown,
    task: JoinHandle<Result<(), WorkerError>>,
    preparation_resources: Option<Arc<PreparationResources>>,
}

struct RegistryRuntimeOptions {
    continuation_wake: Arc<tokio::sync::Notify>,
    monitor_dispatch_wake: Arc<tokio::sync::Notify>,
    session_broker: AccountSessionBroker,
    preparation_resources: Option<Arc<PreparationResources>>,
    config: AccountRegistryConfig,
    managed_tool_runtime:
        Option<Arc<studio_runtime_services::provider_callbacks::ManagedToolRuntime>>,
}

type VmLaunchInputs = (
    String,
    String,
    ProviderLaunchSpec,
    String,
    Vec<u8>,
    Option<Value>,
);

#[derive(Clone, Copy)]
struct VmCodexRefreshIdentity<'a> {
    account_key: &'a str,
    previous_account_id: &'a str,
    actor_id: &'a str,
    actor_epoch: i64,
    generation: u64,
}

struct ClaudeCredentialRequest<'a> {
    path: &'a Path,
    host_config: &'a Path,
    email: &'a str,
    account_key: &'a str,
    original_account: &'a Value,
    claude_bridge_dir: &'a Path,
    explicit_host_config: bool,
}

impl HostRuntime {
    #[doc(hidden)]
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

    #[doc(hidden)]
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
            None,
            None,
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
        managed_tool_runtime: Option<
            Arc<studio_runtime_services::provider_callbacks::ManagedToolRuntime>,
        >,
        // The base Codex home (`resolve_codex_home`); `None` gives host Codex
        // threads no native Chrome browser.
        shared_codex_home: Option<PathBuf>,
    ) -> Result<Self, WorkerError> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| WorkerError::TaskFailed)?;
        let backend_id = data_encoding::HEXLOWER.encode(&nonce);
        let identity = BackendIdentity {
            backend_id: BackendId(backend_id.clone()),
            state_dir: store.root().to_owned(),
        };
        let install_root = resolve_install_root()?;
        let catalog = state.model_catalog_handle();
        let resource_root = install_root.workspace_root().to_owned();
        *state = state.clone().with_vault_host_command();
        let mut resources = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/host_runtime",
            move || PreparationResources::bundled_at(String::new(), catalog, resource_root),
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
        let claude_bridge_dir = install_root.claude_bridge_dir();
        resources.account_driver = Some(accounts.clone());
        if let Some(home) = shared_codex_home {
            resources.shared_home = home.to_string_lossy().into_owned();
        }
        let vm_guest: Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc> = Arc::new(
            studio_runtime_services::linux_vm_guest::UnixGuestRpc::for_runtime_state(store.root()),
        );
        resources.guest_rpc = Some(Arc::clone(&vm_guest));
        let vm_state_dir = studio_runtime_services::vault::linux_vm_state_dir(store.root());
        let program = std::env::current_exe();
        // VM agents run Mac commands through the guest channel and the Mac
        // service, which this executable starts in its own session
        // (docs/host-exec.md).
        let host_exec =
            match &program {
                Ok(program) => Some(studio_runtime_services::host_exec::HostExecRuntime::new(
                    Arc::clone(&vm_guest),
                    Arc::new(
                        studio_runtime_services::host_exec::RustHostExecService::new(
                            program.clone(),
                            vm_state_dir.clone(),
                            studio_runtime_http::backend_build_identity,
                        )
                        // Sign-in links of VM chats' Mac commands reach the host bridge.
                        .with_browser_host(store.root().join(
                            studio_runtime_services::browser_environment::BROWSER_HOST_FOLDER,
                        )),
                    ),
                )),
                Err(error) => {
                    eprintln!("host_exec is unavailable: the executable path is unknown: {error}");
                    None
                }
            };
        // A created VM starts again when the Mac restarted under it, at startup
        // and for the first VM chat (docs/linux-vm-workspaces.md, "VM start").
        #[cfg(unix)]
        let vm_runtime = program.as_ref().ok().map(|program| {
            let vm = studio_runtime_services::linux_vm_runtime::VmRuntime::production(
                vm_state_dir.clone(),
                install_root.linux_vm_helper(),
                program.clone(),
            );
            if let Some(host_exec) = &host_exec {
                vm.attach_host_exec(Arc::clone(host_exec));
            }
            vm.attach_chat_notices(Arc::new(
                studio_runtime_services::linux_vm_runtime::StoreChatNotices::new(store.clone()),
            ));
            vm
        });
        #[cfg(not(unix))]
        let vm_runtime = None;
        #[cfg(target_os = "macos")]
        if let Some(vm) = vm_runtime.clone() {
            tokio::spawn(async move {
                match vm.resume().await {
                    Ok(outcome) if matches!(outcome.as_str(), "absent" | "stopped" | "running") => {
                    }
                    Ok(outcome) => {
                        eprintln!("linux-vm: automatic start at backend start: {outcome}")
                    }
                    Err(error) => {
                        eprintln!("linux-vm: automatic start at backend start failed: {error}")
                    }
                }
                // The watchdog: a failed guest (a kernel panic) restarts within
                // the restart limits (docs/linux-vm-workspaces.md, "VM start").
                let period = std::time::Duration::from_secs(
                    studio_runtime_services::linux_vm_runtime::WATCHDOG_INTERVAL_SECONDS,
                );
                let mut checks =
                    tokio::time::interval_at(tokio::time::Instant::now() + period, period);
                checks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    checks.tick().await;
                    match vm.check().await {
                        Ok(outcome)
                            if matches!(
                                outcome.as_str(),
                                "absent"
                                    | "unknown"
                                    | "stopped"
                                    | "running"
                                    | "starting"
                                    | "unresponsive"
                                    | "busy"
                                    | "limited"
                            ) => {}
                        Ok(outcome) => eprintln!("linux-vm: watchdog check: {outcome}"),
                        Err(error) => eprintln!("linux-vm: watchdog check failed: {error}"),
                    }
                }
            });
        }
        resources.vm_provider_launch = Some(Arc::new(ServerVmProviderLaunchResolver {
            accounts: accounts.clone(),
            account_session: state.account_session.clone(),
            store: store.clone(),
            guest: Arc::clone(&vm_guest),
            claude_bridge_dir: claude_bridge_dir.clone(),
            vm: vm_runtime,
        }));
        let resources = resources.with_account_session(state.account_session.clone());
        let preparation_resources = Arc::new(resources.clone());
        if let Some(managed_tool_runtime) = &managed_tool_runtime {
            managed_tool_runtime.set_preparation_resources(Arc::clone(&preparation_resources));
            managed_tool_runtime.attach_layr_read(Arc::clone(&vm_guest));
            if let Some(host_exec) = host_exec {
                managed_tool_runtime.attach_host_exec(host_exec);
            }
        }
        let callback_runtime = Arc::new(
            studio_runtime_services::callback_runtime::CallbackRuntime::new(
                store.clone(),
                session_broker.callback_reply_port(),
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
            provider_stderr_log: state.app_server_log_handle(),
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
                },
            ),
            environment: std::env::vars().collect(),
        });
        let mut runtime = Self::with_launcher_broker(
            store,
            launcher,
            state.event_wake_handle(),
            RegistryRuntimeOptions {
                continuation_wake: state.continuation_wake_handle(),
                monitor_dispatch_wake,
                session_broker,
                preparation_resources: Some(Arc::clone(&preparation_resources)),
                config,
                managed_tool_runtime,
            },
        )
        .await?;
        runtime.preparation_resources = Some(preparation_resources);
        Ok(runtime)
    }

    #[doc(hidden)]
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

    #[doc(hidden)]
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
            RegistryRuntimeOptions {
                continuation_wake: Arc::new(tokio::sync::Notify::new()),
                monitor_dispatch_wake,
                session_broker: AccountSessionBroker::new(),
                preparation_resources: None,
                config,
                managed_tool_runtime: None,
            },
        )
        .await
    }

    async fn with_launcher_broker(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        wake: Arc<tokio::sync::Notify>,
        options: RegistryRuntimeOptions,
    ) -> Result<Self, WorkerError> {
        let RegistryRuntimeOptions {
            continuation_wake,
            monitor_dispatch_wake,
            session_broker,
            preparation_resources,
            config,
            managed_tool_runtime,
        } = options;
        let registry = AccountRegistry::new_with_broker(
            store,
            launcher,
            monitor_dispatch_wake,
            session_broker,
            config,
            managed_tool_runtime,
        )
        .await?;
        let shutdown = Shutdown::default();
        let task = tokio::spawn(registry.run(wake, continuation_wake, shutdown.clone()));
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
        match timeout(HOST_RUNTIME_SHUTDOWN_WAIT, self.task).await {
            Ok(result) => result.map_err(|_| WorkerError::TaskFailed)?,
            Err(_) => {
                eprintln!(
                    "runtime server detached the account registry task after its {:.0}s shutdown bound",
                    HOST_RUNTIME_SHUTDOWN_WAIT.as_secs_f64()
                );
                Err(WorkerError::TaskFailed)
            }
        }
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
    provider_stderr_log: Option<studio_runtime_process::operational_log::OperationalLog>,
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

/// Resolve the same guest-only command, profile, environment, and credential
/// copy that Python's `codex_linux_workspaces.connect_agent` owns.
struct ServerVmProviderLaunchResolver {
    accounts: Arc<AccountDriver>,
    account_session:
        Option<Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>>,
    store: Arc<RuntimeStore>,
    guest: Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc>,
    claude_bridge_dir: PathBuf,
    /// Starts a stopped VM by the automatic rule before a VM chat uses the guest.
    #[cfg(unix)]
    vm: Option<Arc<studio_runtime_services::linux_vm_runtime::VmRuntime>>,
    #[cfg(not(unix))]
    vm: Option<()>,
}

const CODEX_ACCESS_REFRESH_WINDOW_SECONDS: f64 = 300.0;
const CODEX_ACCESS_MIN_REMAINING_SECONDS: f64 = 30.0;
const CLAUDE_ACCESS_REFRESH_WINDOW_MS: f64 = 300_000.0;
const CLAUDE_ACCESS_REFRESH_TRIGGER_MS: f64 = 315_000.0;
const CLAUDE_REFRESH_READINESS_TIMEOUT: Duration = Duration::from_secs(60);
const CLAUDE_REFRESH_TOTAL_TIMEOUT: Duration = Duration::from_secs(90);
const CLAUDE_REFRESH_READINESS_LINE_LIMIT: usize = 512;
#[cfg(target_os = "macos")]
const CLAUDE_KEYCHAIN_READ_TIMEOUT: Duration = Duration::from_secs(10);
const CLAUDE_ACCOUNT_ID_PREFIX: &str = "claude:";
type VmClaudeAccountLocks = Mutex<BTreeMap<String, Arc<Mutex<()>>>>;
static VM_CLAUDE_ACCOUNT_LOCKS: std::sync::OnceLock<VmClaudeAccountLocks> =
    std::sync::OnceLock::new();

impl studio_runtime_services::account_worker::preparation::VmProviderLaunchResolver
    for ServerVmProviderLaunchResolver
{
    fn resolve(
        &self,
        request: studio_runtime_services::account_worker::preparation::VmProviderLaunchRequest,
    ) -> studio_runtime_services::account_worker::preparation::VmProviderLaunchFuture<'_> {
        Box::pin(async move {
            #[cfg(unix)]
            if let Some(vm) = &self.vm {
                vm.ensure_for_chat().await.map_err(|error| {
                    WorkerError::PreparationPending(
                        studio_runtime_services::account_worker::PreparationBlock::new(
                            studio_runtime_services::account_start_gate::StartWaitReason::ProviderSession,
                            format!("vm-start/{error}"),
                        ),
                    )
                })?;
            }
            let accounts = self.accounts.clone();
            let account_session = self.account_session.clone();
            let claude_bridge_dir = self.claude_bridge_dir.clone();
            let request_for_resolution = request.clone();
            let (provider, profile, launch, credential_path, credential_document, bootstrap) =
                resolve_vm_launch_inputs(
                    Arc::clone(&accounts),
                    account_session.as_deref(),
                    &claude_bridge_dir,
                    request_for_resolution,
                )
                .await?;
            if provider != request.provider {
                return Err(WorkerError::PreparationRefused(
                    "vm-agent/provider-changed".into(),
                ));
            }
            validate_vm_actor_scope(
                Arc::clone(&self.store),
                self.account_session.clone(),
                &request,
            )
            .await?;
            studio_runtime_services::linux_vm_provider::copy_guest_credentials(
                Arc::clone(&self.store),
                Arc::clone(&self.guest),
                studio_runtime_services::linux_vm_provider::GuestCredentialTarget {
                    agent_id: &request.agent_id,
                    actor_epoch: request.epoch,
                    provider: &provider,
                    account_key: &request.account_key,
                    relative_path: &credential_path,
                },
                &credential_document,
            )
            .await
            .map_err(|error| WorkerError::PreparationPending(studio_runtime_services::account_worker::PreparationBlock::new(studio_runtime_services::account_start_gate::StartWaitReason::ProviderSession, format!("vm-credentials/{error}"))))?;
            Ok(
                studio_runtime_services::account_worker::preparation::VmProviderLaunch {
                    provider,
                    profile,
                    launch,
                    bootstrap,
                },
            )
        })
    }

    fn refresh_codex(
        &self,
        account_key: String,
        previous_account_id: String,
        actor_id: String,
        actor_epoch: i64,
        generation: u64,
    ) -> studio_runtime_services::account_worker::preparation::VmProviderAuthFuture<'_> {
        let accounts = Arc::clone(&self.accounts);
        let account_session = self.account_session.clone();
        let store = Arc::clone(&self.store);
        Box::pin(async move {
            refresh_vm_codex_token(
                Arc::clone(&accounts),
                account_session.as_deref(),
                Arc::clone(&store),
                VmCodexRefreshIdentity {
                    account_key: &account_key,
                    previous_account_id: &previous_account_id,
                    actor_id: &actor_id,
                    actor_epoch,
                    generation,
                },
            )
            .await
        })
    }

    fn refresh_claude_credentials(
        &self,
        actors: Vec<studio_runtime_services::account_worker::preparation::VmProviderLaunchRequest>,
    ) -> studio_runtime_services::account_worker::preparation::VmProviderRefreshFuture<'_> {
        Box::pin(async move {
            let Some(first) = actors.first().cloned() else {
                return Ok(());
            };
            if actors.iter().any(|actor| {
                actor.provider != "claude"
                    || actor.account_key != first.account_key
                    || actor.agent_id.is_empty()
                    || actor.epoch < 0
            }) {
                return Err(WorkerError::PreparationRefused(
                    "vm-credentials/actor-scope".into(),
                ));
            }
            let accounts = Arc::clone(&self.accounts);
            let store = Arc::clone(&self.store);
            let account_session = self.account_session.clone();
            let claude_bridge_dir = self.claude_bridge_dir.clone();
            let actors_for_resolution = actors.clone();
            let (_, _, _, credential_path, credential_document, _) = resolve_vm_launch_inputs(
                Arc::clone(&accounts),
                account_session.as_deref(),
                &claude_bridge_dir,
                first,
            )
            .await?;
            let active_actors = store
                .read_async("runtime-server/vm-claude-refresh", move |connection| {
                    let mut active = Vec::new();
                    for actor in actors_for_resolution {
                        if studio_runtime_services::linux_vm_provider::vm_actor_matches(
                            connection,
                            &actor.agent_id,
                            actor.epoch,
                            &actor.account_key,
                            Some("claude"),
                        )
                        .map_err(|_| WorkerError::TaskFailed)?
                        {
                            active.push(actor);
                        }
                    }
                    Ok::<_, WorkerError>(active)
                })
                .await?;
            let mut profile_copy_failed = false;
            for actor in active_actors {
                match validate_vm_actor_scope(
                    Arc::clone(&self.store),
                    self.account_session.clone(),
                    &actor,
                )
                .await
                {
                    Ok(()) => {}
                    Err(WorkerError::PreparationRefused(_)) => continue,
                    Err(error) => return Err(error),
                }
                // A replaced actor cannot receive an old epoch's profile: the
                // worker only supplies currently attached requests and the
                // receipt identity is scoped to agent + account + digest.
                if let Err(error) =
                    studio_runtime_services::linux_vm_provider::copy_guest_credentials(
                        Arc::clone(&self.store),
                        Arc::clone(&self.guest),
                        studio_runtime_services::linux_vm_provider::GuestCredentialTarget {
                            agent_id: &actor.agent_id,
                            actor_epoch: actor.epoch,
                            provider: "claude",
                            account_key: &actor.account_key,
                            relative_path: &credential_path,
                        },
                        &credential_document,
                    )
                    .await
                {
                    match error {
                        studio_runtime_services::linux_vm_provider::GuestProviderError::Task
                        | studio_runtime_services::linux_vm_provider::GuestProviderError::Persistence(_) => {
                            return Err(WorkerError::TaskFailed);
                        }
                        _ => profile_copy_failed = true,
                    }
                }
            }
            if profile_copy_failed {
                return Err(WorkerError::PreparationPending(
                    "vm-credentials/profile-copy-pending".into(),
                ));
            }
            Ok(())
        })
    }
}

async fn validate_vm_actor_scope(
    store: Arc<RuntimeStore>,
    account_session: Option<
        Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    >,
    request: &studio_runtime_services::account_worker::preparation::VmProviderLaunchRequest,
) -> Result<(), WorkerError> {
    if !account_session
        .as_deref()
        .and_then(|session| session.current_scope(&request.account_key))
        .is_some_and(|scope| scope.generation == request.generation)
    {
        return Err(WorkerError::PreparationRefused(
            "vm-agent/account-generation-changed".into(),
        ));
    }
    let request = request.clone();
    store
        .read_async("runtime-server/vm-actor-fence", move |connection| {
            if studio_runtime_services::linux_vm_provider::vm_actor_matches(
                connection,
                &request.agent_id,
                request.epoch,
                &request.account_key,
                Some(&request.provider),
            )
            .map_err(|_| WorkerError::TaskFailed)?
            {
                Ok(())
            } else {
                Err(WorkerError::PreparationRefused(
                    "vm-agent/identity-changed".into(),
                ))
            }
        })
        .await
}

/// This follows Python's `codex_linux_workspaces.py:83-104`: provider comes
/// from the selected account; only guest-specific variables enter the launch.
/// Credential files mirror `codex_linux_vm_credentials.py:22-64`.
async fn resolve_vm_launch_inputs(
    accounts: Arc<AccountDriver>,
    account_session: Option<
        &dyn studio_runtime_services::account_session_runtime::AccountSessionPort,
    >,
    claude_bridge_dir: &Path,
    request: studio_runtime_services::account_worker::preparation::VmProviderLaunchRequest,
) -> Result<VmLaunchInputs, WorkerError> {
    let account_driver = Arc::clone(&accounts);
    let snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/vm-account-snapshot",
        move || account_driver.snapshot(),
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)?
    .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    let account = snapshot
        .accounts
        .iter()
        .find(|profile| {
            profile.get("id").and_then(serde_json::Value::as_str) == Some(&request.account_key)
        })
        .ok_or_else(|| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    if account.get("deleted") == Some(&serde_json::Value::Bool(true))
        || account.get("disconnected") == Some(&serde_json::Value::Bool(true))
        || account.get("status").and_then(serde_json::Value::as_str) != Some("ready")
    {
        return Err(WorkerError::PreparationRefused(
            "account-driver/unavailable".into(),
        ));
    }
    let kind = ProviderKind::of_record(account).map_err(|_| {
        WorkerError::PreparationRefused("provider-session/unsupported-provider".into())
    })?;
    let provider = adapter(kind);
    let account_digest =
        data_encoding::HEXLOWER.encode(&Sha256::digest(request.account_key.as_bytes()));
    let profile_relative = format!(
        "{}/studio-accounts/{}",
        provider.profile_dir(),
        &account_digest[..32]
    );
    let guest_home = if request.guest_home.starts_with('/') {
        request.guest_home
    } else {
        "/home/studio".to_owned()
    };
    let guest_profile = format!("{guest_home}/{profile_relative}");
    let (command, environment, credential_path, credential_document, bootstrap) = if kind
        == ProviderKind::Claude
    {
        let options = account
            .get("claudeOptions")
            .cloned()
            .unwrap_or_else(|| serde_json::json!({}));
        let options =
            studio_runtime_services::claude_bridge::bridge_options(&options).map_err(|_| {
                WorkerError::PreparationRefused("provider-session/claude-runtime-launch".into())
            })?;
        // The host CLI path is not portable and can expose host layout.
        // The guest bridge resolves its own `claude` executable.
        let mut options = options;
        options
            .as_object_mut()
            .ok_or_else(|| {
                WorkerError::PreparationRefused("provider-session/claude-runtime-launch".into())
            })?
            .remove("binaryPath");
        let explicit_host_config = options
            .get("configDir")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|value| !value.is_empty())
            || env::var_os("CLAUDE_CONFIG_DIR").is_some();
        let host_config = options
            .get("configDir")
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .or_else(|| env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
            .or_else(|| {
                env::var_os("HOME")
                    .map(PathBuf::from)
                    .map(|home| home.join(".claude"))
            })
            .ok_or_else(|| {
                WorkerError::PreparationRefused("provider-session/claude-profile-home".into())
            })?;
        let credential_path = format!("{profile_relative}/{}", provider.guest_credential_file());
        let credential_file = host_config.join(".credentials.json");
        let email = account
            .get("email")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .ok_or_else(|| {
                WorkerError::PreparationRefused("vm-credentials/account-identity".into())
            })?;
        let account_driver = Arc::clone(&accounts);
        let account_key = request.account_key.clone();
        let email = email.to_owned();
        let original_account = account.clone();
        let bridge_dir = claude_bridge_dir.to_path_buf();
        let credential_document = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/vm-claude-credential-file",
            move || {
                read_vm_claude_credentials(
                    &account_driver,
                    ClaudeCredentialRequest {
                        path: &credential_file,
                        host_config: &host_config,
                        email: &email,
                        account_key: &account_key,
                        original_account: &original_account,
                        claude_bridge_dir: &bridge_dir,
                        explicit_host_config,
                    },
                )
            },
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
        let environment = BTreeMap::from([
            ("CLAUDE_CONFIG_DIR".into(), guest_profile),
            ("STUDIO_CLAUDE_BIN".into(), "claude".into()),
            ("STUDIO_CLAUDE_OPTIONS".into(), options.to_string()),
            (
                "STUDIO_CLAUDE_ACCOUNT".into(),
                account
                    .get("email")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("")
                    .to_owned(),
            ),
        ]);
        (
            provider.vm_guest_command(&guest_home, &request.agent_id),
            environment,
            credential_path,
            credential_document,
            None,
        )
    } else {
        // Python clears legacy copied Codex credentials before bootstrapping
        // external auth through the retained provider session.
        let credential_path = format!("{profile_relative}/{}", provider.guest_credential_file());
        let bootstrap = read_vm_codex_bootstrap(
            Arc::clone(&accounts),
            account_session,
            &request.account_key,
            account,
            false,
        )
        .await?;
        (
            provider.vm_guest_command(&guest_home, &request.agent_id),
            BTreeMap::from([("CODEX_HOME".into(), guest_profile)]),
            credential_path,
            b"{}".to_vec(),
            Some(bootstrap),
        )
    };
    let launch = ProviderLaunchSpec::new(command, environment, PathBuf::from(&request.cwd));
    Ok((
        kind.key().to_owned(),
        profile_relative,
        launch,
        credential_path,
        credential_document,
        bootstrap,
    ))
}

/// Python's `codex_linux_vm_auth.codex_access` refreshes through the retained
/// host account session when the access token enters its 315-second window,
/// then `bootstrap_codex` sends this payload as `account/login/start`.
async fn read_vm_codex_bootstrap(
    accounts: Arc<AccountDriver>,
    session: Option<&dyn studio_runtime_services::account_session_runtime::AccountSessionPort>,
    account_key: &str,
    account: &Value,
    force_refresh: bool,
) -> Result<Value, WorkerError> {
    use base64::Engine as _;
    let read = || {
        let account = account.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/vm-codex-auth-file",
            move || -> Result<(Value, Option<f64>), WorkerError> {
                let home = account
                    .get("home")
                    .and_then(Value::as_str)
                    .map(PathBuf::from)
                    .or_else(|| {
                        std::env::var_os("HOME")
                            .map(PathBuf::from)
                            .map(|path| path.join(".codex"))
                    })
                    .ok_or_else(|| {
                        WorkerError::PreparationRefused(
                            "provider-session/codex-profile-home".into(),
                        )
                    })?;
                let bytes = std::fs::read(home.join("auth.json")).map_err(|_| {
                    WorkerError::PreparationRefused(
                        "provider-session/codex-auth-unavailable".into(),
                    )
                })?;
                if bytes.len()
                    > studio_runtime_services::linux_vm_provider::MAX_GUEST_CREDENTIAL_BYTES
                {
                    return Err(WorkerError::PreparationRefused(
                        "provider-session/codex-auth-invalid".into(),
                    ));
                }
                let auth: Value = serde_json::from_slice(&bytes).map_err(|_| {
                    WorkerError::PreparationRefused("provider-session/codex-auth-invalid".into())
                })?;
                if let Some(key) = auth
                    .get("OPENAI_API_KEY")
                    .and_then(Value::as_str)
                    .filter(|key| !key.is_empty())
                {
                    let expected = account.get("_credentialIdentity").and_then(Value::as_str);
                    let observed = format!(
                        "api:{}",
                        data_encoding::HEXLOWER.encode(&Sha256::digest(key.as_bytes()))
                    );
                    if expected.is_some_and(|value| value != observed) {
                        return Err(WorkerError::PreparationRefused(
                            "provider-session/codex-account-changed".into(),
                        ));
                    }
                    return Ok((serde_json::json!({"type":"apiKey","apiKey":key}), None));
                }
                let tokens = auth.get("tokens").ok_or_else(|| {
                    WorkerError::PreparationRefused("provider-session/codex-auth-invalid".into())
                })?;
                let access = tokens
                    .get("access_token")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| {
                        WorkerError::PreparationRefused(
                            "provider-session/codex-auth-invalid".into(),
                        )
                    })?;
                let mut parts = access.split('.');
                let _header = parts.next();
                let claims = parts
                    .next()
                    .and_then(|part| {
                        base64::engine::general_purpose::URL_SAFE_NO_PAD
                            .decode(part)
                            .or_else(|_| base64::engine::general_purpose::URL_SAFE.decode(part))
                            .ok()
                    })
                    .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
                    .ok_or_else(|| {
                        WorkerError::PreparationRefused(
                            "provider-session/codex-auth-invalid".into(),
                        )
                    })?;
                let identity = tokens
                    .get("account_id")
                    .and_then(Value::as_str)
                    .or_else(|| {
                        claims
                            .pointer("/https://api.openai.com/auth/chatgpt_account_id")
                            .and_then(Value::as_str)
                    });
                if identity.is_none()
                    || identity != account.get("accountId").and_then(Value::as_str)
                {
                    return Err(WorkerError::PreparationRefused(
                        "provider-session/codex-account-changed".into(),
                    ));
                }
                let expires = claims.get("exp").and_then(Value::as_f64).ok_or_else(|| {
                    WorkerError::PreparationRefused("provider-session/codex-auth-invalid".into())
                })?;
                let payload = serde_json::json!({"type":"chatgptAuthTokens","accessToken":access,"chatgptAccountId":identity});
                Ok((payload, Some(expires)))
            },
        )
    };
    let (mut payload, expires) = read().await.map_err(|_| WorkerError::TaskFailed)??;
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| WorkerError::TaskFailed)?
        .as_secs_f64();
    if force_refresh
        || expires.is_some_and(|value| value <= now + CODEX_ACCESS_REFRESH_WINDOW_SECONDS)
    {
        let port = session.ok_or_else(|| {
            WorkerError::PreparationPending("provider-session/account-session".into())
        })?;
        let scope = port.ensure_session(account_key).await.map_err(|_| {
            WorkerError::PreparationPending("provider-session/account-session".into())
        })?;
        let source_token = payload
            .pointer("/accessToken")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                WorkerError::PreparationRefused("provider-session/codex-auth-invalid".into())
            })?;
        let source_digest = studio_runtime_store::sha256_hex(source_token.as_bytes());
        let source_expiry = expires.ok_or_else(|| {
            WorkerError::PreparationRefused("provider-session/codex-auth-invalid".into())
        })?;
        let operation = format!(
            "vm-codex-token-refresh:{account_key}:{}:{source_expiry}:{}",
            scope.generation,
            &source_digest[..24]
        );
        let receipt = port
            .provider_request(
                studio_runtime_services::account_session_runtime::ProviderRequest {
                    account: account_key.to_owned(),
                    operation,
                    actor: None,
                    expected_scope: Some(scope),
                    deleted_actor_session: None,
                    method: studio_runtime_providers::Method::AccountRead,
                    params: serde_json::json!({"refreshToken":true}),
                    deadline: std::time::Instant::now() + std::time::Duration::from_secs(45),
                },
            )
            .await
            .map_err(|_| WorkerError::PreparationPending("provider-session/account-read".into()))?;
        if receipt.certainty
            != studio_runtime_services::account_session_runtime::RequestCertainty::Written
        {
            return Err(WorkerError::PreparationPending(
                "provider-session/account-read-unknown".into(),
            ));
        }
        if !matches!(
            receipt.result,
            Some(studio_runtime_services::account_session_runtime::ProviderResponse::Reply(_))
        ) {
            return Err(WorkerError::PreparationPending(
                "provider-session/account-read-unresolved".into(),
            ));
        }
        (payload, _) = read().await.map_err(|_| WorkerError::TaskFailed)??;
        let refreshed = payload
            .pointer("/accessToken")
            .and_then(Value::as_str)
            .and_then(|token| token.split('.').nth(1))
            .and_then(|part| {
                base64::engine::general_purpose::URL_SAFE_NO_PAD
                    .decode(part)
                    .ok()
            })
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|claims| claims.get("exp").and_then(Value::as_f64));
        if refreshed.is_none_or(|value| value <= now + CODEX_ACCESS_MIN_REMAINING_SECONDS) {
            return Err(WorkerError::PreparationPending(
                "provider-session/account-read-refresh".into(),
            ));
        }
        let account_driver = Arc::clone(&accounts);
        let current = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-server/vm-account-refresh-snapshot",
            move || account_driver.snapshot(),
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)?
        .map_err(|_| WorkerError::PreparationPending("account-driver/unavailable".into()))?;
        let still_same = current.accounts.iter().any(|row| {
            row.get("id").and_then(Value::as_str) == Some(account_key)
                && row.get("accountId") == account.get("accountId")
                && row.get("provider").and_then(Value::as_str) == Some("codex")
                && row.get("status").and_then(Value::as_str) == Some("ready")
        });
        if !still_same {
            return Err(WorkerError::PreparationRefused(
                "provider-session/codex-account-changed".into(),
            ));
        }
    }
    // The raw credential payload is returned only in memory for guest login;
    // durable receipts use its digest and never store token bytes.
    Ok(payload)
}

async fn refresh_vm_codex_token(
    accounts: Arc<AccountDriver>,
    account_session: Option<
        &dyn studio_runtime_services::account_session_runtime::AccountSessionPort,
    >,
    store: Arc<RuntimeStore>,
    identity: VmCodexRefreshIdentity<'_>,
) -> Result<Value, WorkerError> {
    let VmCodexRefreshIdentity {
        account_key,
        previous_account_id,
        actor_id,
        actor_epoch,
        generation,
    } = identity;
    let scope_is_current = || {
        account_session
            .and_then(|session| session.current_scope(account_key))
            .is_some_and(|scope| scope.generation == generation)
    };
    let actor_is_current = || {
        let store = Arc::clone(&store);
        let actor_id = actor_id.to_owned();
        let account_key = account_key.to_owned();
        async move {
            store
                .read_async("runtime-server/vm-actor-refresh-fence", move |connection| {
                    studio_runtime_services::linux_vm_provider::vm_actor_matches(
                        connection,
                        &actor_id,
                        actor_epoch,
                        &account_key,
                        Some("codex"),
                    )
                    .map_err(|_| WorkerError::TaskFailed)
                })
                .await
        }
    };
    if !scope_is_current() || !actor_is_current().await? {
        return Err(WorkerError::PreparationRefused(
            "vm-auth/actor-changed".into(),
        ));
    }
    let account_driver = Arc::clone(&accounts);
    let snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-server/vm-auth-account-snapshot",
        move || account_driver.snapshot(),
    )
    .await
    .map_err(|_| WorkerError::TaskFailed)?
    .map_err(|_| WorkerError::PreparationPending("account-driver/unavailable".into()))?;
    let account = snapshot
        .accounts
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(account_key))
        .ok_or_else(|| WorkerError::PreparationRefused("vm-auth/account-unavailable".into()))?;
    if account.get("provider").and_then(Value::as_str) != Some("codex")
        || account.get("status").and_then(Value::as_str) != Some("ready")
        || account.get("accountId").and_then(Value::as_str) != Some(previous_account_id)
    {
        return Err(WorkerError::PreparationRefused(
            "vm-auth/account-changed".into(),
        ));
    }
    let payload = read_vm_codex_bootstrap(
        Arc::clone(&accounts),
        account_session,
        account_key,
        account,
        true,
    )
    .await?;
    if payload.get("type").and_then(Value::as_str) != Some("chatgptAuthTokens")
        || payload.get("chatgptAccountId").and_then(Value::as_str) != Some(previous_account_id)
        || payload
            .get("accessToken")
            .and_then(Value::as_str)
            .is_none_or(str::is_empty)
    {
        return Err(WorkerError::PreparationRefused(
            "vm-auth/account-changed".into(),
        ));
    }
    if !scope_is_current() || !actor_is_current().await? {
        return Err(WorkerError::PreparationRefused(
            "vm-auth/actor-changed".into(),
        ));
    }
    Ok(serde_json::json!({
        "accessToken":payload["accessToken"],
        "chatgptAccountId":payload["chatgptAccountId"],
    }))
}

/// Mirror Python `claude_access` (codex_linux_vm_auth.py:168-185): refresh
/// inside the CLI's existing helper, then verify account identity and expiry
/// before the access-only document reaches the durable guest-copy operation.
#[allow(
    clippy::disallowed_methods,
    reason = "runs on blocking_pool through read_vm_claude_credentials"
)]
fn read_vm_claude_credentials(
    accounts: &AccountDriver,
    request: ClaudeCredentialRequest<'_>,
) -> Result<Vec<u8>, WorkerError> {
    let ClaudeCredentialRequest {
        path,
        host_config,
        email,
        account_key,
        original_account,
        claude_bridge_dir,
        explicit_host_config,
    } = request;
    let account_lock = VM_CLAUDE_ACCOUNT_LOCKS
        .get_or_init(|| Mutex::new(BTreeMap::new()))
        .lock()
        .map_err(|_| WorkerError::TaskFailed)?
        .entry(account_key.to_owned())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone();
    let _refresh_guard = account_lock.lock().map_err(|_| WorkerError::TaskFailed)?;
    // `auth_pin` includes the internal credential identity that Python's
    // `_same_account` compares across the refresh.
    let expected_pin = accounts
        .auth_pin(account_key)
        .map_err(|_| WorkerError::PreparationPending("vm-credentials/account-status".into()))?;
    let mut access = read_vm_claude_oauth(path, explicit_host_config)?;
    let mut expiry = claude_expiry_ms(&access)?;
    let mut auth_status_verified = false;
    let now_ms = unix_time_ms()?;
    if expiry <= now_ms + CLAUDE_ACCESS_REFRESH_TRIGGER_MS {
        // Python waits only for the final 15 seconds before allowing the native
        // CLI refresh window to open (codex_linux_vm_auth.py:173-177).
        let wait_deadline = Instant::now() + Duration::from_secs(15);
        while expiry > unix_time_ms()? + CLAUDE_ACCESS_REFRESH_WINDOW_MS {
            if Instant::now() >= wait_deadline {
                return Err(WorkerError::PreparationPending(
                    "vm-credentials/refresh-window".into(),
                ));
            }
            std::thread::sleep(
                Duration::from_millis(100)
                    .min(wait_deadline.saturating_duration_since(Instant::now())),
            );
            access = read_vm_claude_oauth(path, explicit_host_config)?;
            expiry = claude_expiry_ms(&access)?;
        }
        verify_vm_claude_account(
            accounts,
            account_key,
            original_account,
            &expected_pin,
            email,
            false,
        )?;
        let config_dir = host_config.to_string_lossy();
        let binary = studio_runtime_services::account_driver::claude::resolve_claude_binary(
            original_account,
        )
        .ok_or_else(|| {
            WorkerError::PreparationRefused("vm-credentials/claude-unavailable".into())
        })?;
        let node = find_vm_node().ok_or_else(|| {
            WorkerError::PreparationRefused("vm-credentials/node-unavailable".into())
        })?;
        let helper = claude_bridge_dir.join("bridge.mjs");
        if !helper.is_file() {
            return Err(WorkerError::PreparationRefused(
                "vm-credentials/refresh-helper-unavailable".into(),
            ));
        }
        let mut helper_environment =
            studio_runtime_services::account_driver::claude::subscription_environment(
                env::vars_os().collect(),
            );
        helper_environment.insert("CLAUDE_CONFIG_DIR".into(), config_dir.into_owned().into());
        helper_environment.insert("STUDIO_CLAUDE_BIN".into(), binary.into());
        helper_environment.insert("STUDIO_CLAUDE_ACCOUNT".into(), email.into());
        run_vm_claude_refresh_helper(&node, &helper, &helper_environment, || {
            let current = read_vm_claude_oauth(path, explicit_host_config)?;
            Ok(claude_expiry_ms(&current)? > unix_time_ms()? + CLAUDE_ACCESS_REFRESH_WINDOW_MS)
        })?;
        verify_vm_claude_account(
            accounts,
            account_key,
            original_account,
            &expected_pin,
            email,
            true,
        )?;
        auth_status_verified = true;
        access = read_vm_claude_oauth(path, explicit_host_config)?;
        expiry = claude_expiry_ms(&access)?;
    }
    // Python forces native auth metadata even when no refresh was needed
    // (codex_linux_vm_auth.py:182-185).
    verify_vm_claude_account(
        accounts,
        account_key,
        original_account,
        &expected_pin,
        email,
        !auth_status_verified,
    )?;
    if expiry <= unix_time_ms()? + CLAUDE_ACCESS_REFRESH_TRIGGER_MS {
        return Err(WorkerError::PreparationPending(
            "vm-credentials/refresh-not-fresh".into(),
        ));
    }
    encode_vm_claude_credentials(&access)
}

fn encode_vm_claude_credentials(
    access: &serde_json::Map<String, Value>,
) -> Result<Vec<u8>, WorkerError> {
    let mut allowlisted = serde_json::Map::new();
    for name in [
        "accessToken",
        "expiresAt",
        "scopes",
        "subscriptionType",
        "rateLimitTier",
    ] {
        if let Some(value) = access.get(name) {
            allowlisted.insert(name.into(), value.clone());
        }
    }
    if allowlisted
        .get("accessToken")
        .and_then(Value::as_str)
        .is_none_or(str::is_empty)
        || !allowlisted
            .get("scopes")
            .and_then(Value::as_array)
            .is_some_and(|scopes| scopes.iter().any(|scope| scope == "user:inference"))
    {
        return Err(WorkerError::PreparationRefused(
            "vm-credentials/invalid".into(),
        ));
    }
    serde_json::to_vec(&json!({"claudeAiOauth":allowlisted})).map_err(|_| WorkerError::TaskFailed)
}

fn read_vm_claude_oauth(
    path: &Path,
    explicit_config_dir: bool,
) -> Result<serde_json::Map<String, Value>, WorkerError> {
    let max_bytes = studio_runtime_services::linux_vm_provider::MAX_GUEST_CREDENTIAL_BYTES;
    #[cfg(target_os = "macos")]
    let keychain_bytes = read_vm_claude_keychain(path, explicit_config_dir)?;
    #[cfg(not(target_os = "macos"))]
    let keychain_bytes: Option<Vec<u8>> = {
        let _ = explicit_config_dir;
        None
    };
    let bytes = match keychain_bytes {
        Some(bytes) => bytes,
        None => {
            let file = std::fs::File::open(path).map_err(|_| {
                WorkerError::PreparationRefused("vm-credentials/unavailable".into())
            })?;
            if file
                .metadata()
                .map_err(|_| WorkerError::PreparationRefused("vm-credentials/unavailable".into()))?
                .len()
                > max_bytes as u64
            {
                return Err(WorkerError::PreparationRefused(
                    "vm-credentials/size-limit".into(),
                ));
            }
            let mut bytes = Vec::new();
            std::io::Read::take(file, max_bytes as u64 + 1)
                .read_to_end(&mut bytes)
                .map_err(|_| {
                    WorkerError::PreparationRefused("vm-credentials/unavailable".into())
                })?;
            if bytes.len() > max_bytes {
                return Err(WorkerError::PreparationRefused(
                    "vm-credentials/size-limit".into(),
                ));
            }
            bytes
        }
    };
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|_| WorkerError::PreparationRefused("vm-credentials/invalid".into()))?;
    let access = value
        .get("claudeAiOauth")
        .and_then(serde_json::Value::as_object)
        .cloned()
        .ok_or_else(|| WorkerError::PreparationRefused("vm-credentials/invalid".into()))?;
    // Validate required fields without retaining token material in an error.
    let _token = access
        .get("accessToken")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| WorkerError::PreparationRefused("vm-credentials/invalid".into()))?;
    let scopes = access
        .get("scopes")
        .and_then(Value::as_array)
        .filter(|scopes| scopes.iter().any(|scope| scope == "user:inference"))
        .ok_or_else(|| WorkerError::PreparationRefused("vm-credentials/invalid".into()))?;
    let _ = scopes;
    let _expiry = claude_expiry_ms(&access)?;
    Ok(access)
}

#[cfg(target_os = "macos")]
/// Read the same per-config Keychain service as
/// `codex_linux_vm_credentials.py:13-19` and `codex_linux_vm_auth.py:96-120`.
#[allow(
    clippy::disallowed_methods,
    reason = "runs on blocking_pool through read_vm_claude_credentials"
)]
fn read_vm_claude_keychain(
    path: &Path,
    explicit_config_dir: bool,
) -> Result<Option<Vec<u8>>, WorkerError> {
    let config_dir = path.parent().ok_or(WorkerError::TaskFailed)?;
    let config_identity = explicit_config_dir.then(|| config_dir.to_string_lossy());
    let service = vm_claude_keychain_service(config_identity.as_deref());
    let security = Path::new("/usr/bin/security");
    let mut command = CommandWrap::with_new(security.as_os_str(), |command| {
        command
            .args(["find-generic-password", "-s", service.as_str(), "-w"])
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
    });
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    let mut child = command.spawn().map_err(|_| {
        WorkerError::PreparationRefused("vm-credentials/keychain-unavailable".into())
    })?;
    let stdout = child.stdout().take().ok_or(WorkerError::TaskFailed)?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let reader = std::thread::Builder::new()
        .name("vm-claude-keychain-read".into())
        .spawn(move || {
            let limit = studio_runtime_services::linux_vm_provider::MAX_GUEST_CREDENTIAL_BYTES;
            let mut output = Vec::new();
            let result = std::io::Read::take(stdout, limit as u64 + 1)
                .read_to_end(&mut output)
                .map(|_| output)
                .map_err(|_| ());
            let _sent = sender.send(result);
        })
        .map_err(|_| WorkerError::TaskFailed)?;
    let deadline = Instant::now() + CLAUDE_KEYCHAIN_READ_TIMEOUT;
    let status = loop {
        if let Some(status) = child
            .inner_mut()
            .try_wait()
            .map_err(|_| WorkerError::TaskFailed)?
        {
            break status;
        }
        if Instant::now() >= deadline {
            stop_vm_claude_refresh_child(&mut *child)?;
            drop(reader);
            return Err(WorkerError::PreparationPending(
                "vm-credentials/keychain-timeout".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    let output = match receiver.recv_timeout(Duration::from_secs(1)) {
        // The process has exited, but the pipe-reader thread may not yet have
        // been scheduled to report EOF. A credential read timeout can be
        // retried on the next account-level maintenance sweep.
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            drop(reader);
            return Err(WorkerError::PreparationPending(
                "vm-credentials/keychain-read-pending".into(),
            ));
        }
        // A disconnected channel means the reader exited without delivering a
        // result. Join it so a panic remains a worker failure, not a retryable
        // credential condition.
        Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
            reader.join().map_err(|_| WorkerError::TaskFailed)?;
            return Err(WorkerError::TaskFailed);
        }
        Ok(Err(())) => {
            reader.join().map_err(|_| WorkerError::TaskFailed)?;
            return Err(WorkerError::PreparationPending(
                "vm-credentials/keychain-read-pending".into(),
            ));
        }
        Ok(Ok(output)) => output,
    };
    reader.join().map_err(|_| WorkerError::TaskFailed)?;
    if !status.success() {
        return Ok(None);
    }
    if output.len() > studio_runtime_services::linux_vm_provider::MAX_GUEST_CREDENTIAL_BYTES {
        return Err(WorkerError::PreparationRefused(
            "vm-credentials/size-limit".into(),
        ));
    }
    Ok(Some(output))
}

#[cfg(any(target_os = "macos", test))]
fn vm_claude_keychain_service(config_dir: Option<&str>) -> String {
    let mut service = "Claude Code-credentials".to_owned();
    if let Some(config_dir) = config_dir {
        let config_dir = config_dir.trim_end_matches('/');
        let digest = data_encoding::HEXLOWER.encode(&Sha256::digest(config_dir.as_bytes()));
        service.push('-');
        service.push_str(&digest[..8]);
    }
    service
}

#[cfg(test)]
mod vm_claude_keychain_tests {
    use super::vm_claude_keychain_service;

    #[test]
    fn default_claude_keychain_service_stays_unsuffixed() {
        assert_eq!(vm_claude_keychain_service(None), "Claude Code-credentials");
    }

    #[test]
    fn explicitly_selected_claude_config_uses_scoped_service() {
        assert_eq!(
            vm_claude_keychain_service(Some("/tmp/claude-profile/")),
            "Claude Code-credentials-7182514b"
        );
    }
}

fn claude_expiry_ms(access: &serde_json::Map<String, Value>) -> Result<f64, WorkerError> {
    let expiry = access
        .get("expiresAt")
        .and_then(serde_json::Value::as_f64)
        .filter(|expiry| expiry.is_finite())
        .ok_or_else(|| WorkerError::PreparationRefused("vm-credentials/invalid".into()))?;
    Ok(expiry)
}

fn unix_time_ms() -> Result<f64, WorkerError> {
    let now_ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| WorkerError::TaskFailed)?
        .as_secs_f64()
        * 1000.0;
    Ok(now_ms)
}

fn verify_vm_claude_account(
    accounts: &AccountDriver,
    account_key: &str,
    original_account: &Value,
    expected_pin: &Value,
    email: &str,
    refresh_auth_status: bool,
) -> Result<(), WorkerError> {
    if refresh_auth_status
        && accounts
            .get(account_key)
            .map_err(|_| WorkerError::PreparationPending("vm-credentials/account-status".into()))?
            != AccountStatus::Ready
    {
        return Err(WorkerError::PreparationRefused(
            "vm-credentials/account-identity".into(),
        ));
    }
    let snapshot = accounts
        .snapshot()
        .map_err(|_| WorkerError::PreparationPending("account-driver/unavailable".into()))?;
    let current = snapshot
        .accounts
        .iter()
        .find(|row| row.get("id").and_then(Value::as_str) == Some(account_key))
        .ok_or_else(|| WorkerError::PreparationRefused("vm-credentials/account-identity".into()))?;
    if accounts
        .auth_pin(account_key)
        .map_err(|_| WorkerError::PreparationPending("vm-credentials/account-status".into()))?
        != *expected_pin
    {
        return Err(WorkerError::PreparationRefused(
            "vm-credentials/account-changed".into(),
        ));
    }
    for field in [
        "provider",
        "home",
        "accountId",
        "email",
        "claudeOptions",
        "deleted",
        "disconnected",
        "duplicateOf",
        "_credentialIdentity",
    ] {
        if current.get(field) != original_account.get(field) {
            return Err(WorkerError::PreparationRefused(
                "vm-credentials/account-changed".into(),
            ));
        }
    }
    let expected_account_id = format!("{CLAUDE_ACCOUNT_ID_PREFIX}{email}");
    if current.get("status").and_then(Value::as_str) != Some("ready")
        || current.get("provider").and_then(Value::as_str) != Some("claude")
        || current.get("email").and_then(Value::as_str) != Some(email)
        || current.get("accountId").and_then(Value::as_str) != Some(expected_account_id.as_str())
    {
        return Err(WorkerError::PreparationRefused(
            "vm-credentials/account-identity".into(),
        ));
    }
    Ok(())
}

fn find_vm_node() -> Option<PathBuf> {
    let path = env::var_os("PATH").unwrap_or_default();
    for directory in env::split_paths(&path) {
        let candidate = directory.join("node");
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    let homebrew_node = PathBuf::from("/opt/homebrew/bin/node");
    is_executable(&homebrew_node).then_some(homebrew_node)
}

/// Invoke the bridge.mjs launcher in authentication-refresh mode, matching
/// `codex_linux_vm_auth.py:122-156`. It emits one bounded readiness line; the
/// caller releases its held query only after the on-disk access token is
/// outside the native refresh window. Child output never includes secrets.
#[allow(
    clippy::disallowed_methods,
    reason = "runs on blocking_pool through read_vm_claude_credentials"
)]
fn run_vm_claude_refresh_helper(
    node: &Path,
    helper: &Path,
    environment: &BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    mut access_is_fresh: impl FnMut() -> Result<bool, WorkerError>,
) -> Result<(), WorkerError> {
    let mut command = CommandWrap::with_new(node.as_os_str(), |command| {
        command
            .arg(helper)
            .arg("--refresh-auth")
            .env_clear()
            .envs(environment)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
    });
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    let mut child = command
        .spawn()
        .map_err(|_| WorkerError::PreparationPending("vm-credentials/refresh-start".into()))?;
    let stdout = child
        .stdout()
        .take()
        .ok_or_else(|| WorkerError::TaskFailed)?;
    let mut stdin = child
        .stdin()
        .take()
        .ok_or_else(|| WorkerError::TaskFailed)?;
    let (ready_tx, ready_rx) = std::sync::mpsc::sync_channel(1);
    let ready_reader = std::thread::Builder::new()
        .name("vm-claude-refresh-ready".into())
        .spawn(move || {
            let mut line = String::new();
            let result = std::io::BufReader::new(stdout)
                .take((CLAUDE_REFRESH_READINESS_LINE_LIMIT + 1) as u64)
                .read_line(&mut line)
                .map(|_| line);
            let _sent = ready_tx.send(result);
        })
        .map_err(|_| WorkerError::TaskFailed)?;
    let started = Instant::now();
    let ready_deadline = started + CLAUDE_REFRESH_READINESS_TIMEOUT;
    let ready_line = loop {
        if Instant::now() >= ready_deadline {
            stop_vm_claude_refresh_child(&mut *child)?;
            drop(ready_reader);
            return Err(WorkerError::PreparationPending(
                "vm-credentials/refresh-readiness-timeout".into(),
            ));
        }
        match ready_rx.recv_timeout(Duration::from_millis(50)) {
            Ok(Ok(line)) => break line,
            Ok(Err(_)) => {
                stop_vm_claude_refresh_child(&mut *child)?;
                drop(ready_reader);
                return Err(WorkerError::PreparationPending(
                    "vm-credentials/refresh-readiness-failed".into(),
                ));
            }
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                stop_vm_claude_refresh_child(&mut *child)?;
                drop(ready_reader);
                return Err(WorkerError::TaskFailed);
            }
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                let child_status = match child.inner_mut().try_wait() {
                    Ok(status) => status,
                    Err(_) => {
                        let _stopped = stop_vm_claude_refresh_child(&mut *child);
                        drop(ready_reader);
                        return Err(WorkerError::TaskFailed);
                    }
                };
                if child_status.is_some() {
                    drop(ready_reader);
                    return Err(WorkerError::PreparationPending(
                        "vm-credentials/refresh-readiness-failed".into(),
                    ));
                }
            }
        }
    };
    let ready: Value = match serde_json::from_str(ready_line.trim()) {
        Ok(ready) => ready,
        Err(_) => {
            stop_vm_claude_refresh_child(&mut *child)?;
            drop(ready_reader);
            return Err(WorkerError::PreparationPending(
                "vm-credentials/refresh-readiness-failed".into(),
            ));
        }
    };
    if ready != json!({"ready":true}) || ready_line.len() > CLAUDE_REFRESH_READINESS_LINE_LIMIT {
        stop_vm_claude_refresh_child(&mut *child)?;
        drop(ready_reader);
        return Err(WorkerError::PreparationPending(
            "vm-credentials/refresh-readiness-failed".into(),
        ));
    }
    let total_deadline = started + CLAUDE_REFRESH_TOTAL_TIMEOUT;
    loop {
        let access_is_fresh = match access_is_fresh() {
            Ok(fresh) => fresh,
            Err(error) => {
                stop_vm_claude_refresh_child(&mut *child)?;
                drop(ready_reader);
                return Err(error);
            }
        };
        if access_is_fresh {
            if stdin
                .write_all(b"done\n")
                .and_then(|()| stdin.flush())
                .is_err()
            {
                let _stopped = stop_vm_claude_refresh_child(&mut *child);
                drop(ready_reader);
                return Err(WorkerError::PreparationPending(
                    "vm-credentials/refresh-release".into(),
                ));
            }
            drop(stdin);
            break;
        }
        if Instant::now() >= total_deadline {
            stop_vm_claude_refresh_child(&mut *child)?;
            drop(ready_reader);
            return Err(WorkerError::PreparationPending(
                "vm-credentials/refresh-timeout".into(),
            ));
        }
        let child_status = match child.inner_mut().try_wait() {
            Ok(status) => status,
            Err(_) => {
                let _stopped = stop_vm_claude_refresh_child(&mut *child);
                drop(ready_reader);
                return Err(WorkerError::TaskFailed);
            }
        };
        if child_status.is_some() {
            drop(ready_reader);
            return Err(WorkerError::PreparationPending(
                "vm-credentials/refresh-exited-early".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(250));
    }
    loop {
        let status = match child.inner_mut().try_wait() {
            Ok(status) => status,
            Err(_) => {
                let _stopped = stop_vm_claude_refresh_child(&mut *child);
                drop(ready_reader);
                return Err(WorkerError::TaskFailed);
            }
        };
        if let Some(status) = status {
            if !status.success() {
                drop(ready_reader);
                return Err(WorkerError::PreparationRefused(
                    "vm-credentials/refresh-failed".into(),
                ));
            }
            ready_reader.join().map_err(|_| WorkerError::TaskFailed)?;
            return Ok(());
        }
        if Instant::now() >= total_deadline {
            stop_vm_claude_refresh_child(&mut *child)?;
            drop(ready_reader);
            return Err(WorkerError::PreparationPending(
                "vm-credentials/refresh-timeout".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "runs on blocking_pool through read_vm_claude_credentials"
)]
fn stop_vm_claude_refresh_child(child: &mut dyn ChildWrapper) -> Result<(), WorkerError> {
    if child
        .inner_mut()
        .try_wait()
        .map_err(|_| WorkerError::TaskFailed)?
        .is_some()
    {
        return Ok(());
    }
    child.start_kill().map_err(|_| WorkerError::TaskFailed)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if child
            .inner_mut()
            .try_wait()
            .map_err(|_| WorkerError::TaskFailed)?
            .is_some()
        {
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    Err(WorkerError::TaskFailed)
}

impl AccountLauncher for HostLauncher {
    fn launch(&self, account: String) -> LaunchFuture<'_> {
        Box::pin(async move {
            let accounts = self.accounts.clone();
            let key = account.clone();
            let (kind, home, isolated, profile) =
                studio_runtime_process::blocking_pool::spawn_blocking(
                    "runtime-server/host_runtime",
                    move || selected_account(&accounts, &key),
                )
                .await
                .map_err(|_| WorkerError::TaskFailed)??;
            let mut resources = (*self.resources).clone();
            resources.account_home.clone_from(&home);
            let accounts = self.accounts.clone();
            let key = account.clone();
            resources.claude_account_pin = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/host_runtime",
                move || accounts.auth_pin(&key),
            )
            .await
            .map_err(|_| WorkerError::TaskFailed)?
            .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
            // The selected native bundle is a Codex executable; keep its lease
            // for as long as the retained session can launch from it.
            let lease = match &self.native_runtime {
                Some(runtime) if kind == ProviderKind::Codex => {
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
                }
                _ => None,
            };
            let codex_executable = lease
                .as_ref()
                .and_then(|lease| lease.path().to_str())
                .map(str::to_owned)
                .or_else(|| self.native_path.clone());
            let ProviderLaunch {
                spec: launch,
                executable,
            } = adapter(kind).host_launch(HostLaunch {
                profile: &profile,
                home,
                isolated,
                codex_executable: codex_executable.as_deref(),
                claude_bridge_dir: &self.claude_bridge_dir,
                account: &account,
                root: self.root.clone(),
                environment: self.environment.clone(),
            })?;
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
                let generation = next_direct_generation(&self.store, &account).await?;
                let prepared = DirectProviderTaskFactory::new(
                    self.child_priority,
                    self.provider_stderr_log.clone(),
                )
                .prepare(
                    handle.clone(),
                    OpenOptions {
                        expected_generation: Some(generation),
                        ..open
                    },
                    &Shutdown::default(),
                )
                .map_err(studio_runtime_services::provider_session_task::SessionTaskError::from)?;
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
                        kind,
                        executable,
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
                    initialize_params: studio_runtime_providers::initialize_params(
                        studio_runtime_providers::ClientName::AccountSession,
                    ),
                    generation,
                    wire: kind.wire(),
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

#[cfg(test)]
#[allow(clippy::items_after_test_module, clippy::unwrap_used)]
#[allow(clippy::disallowed_methods)]
mod install_root_tests {
    use super::{AccountDriver, CLAUDE_BRIDGE_RELATIVE, HttpState, InstallRoot};
    use std::{fs, path::PathBuf};
    use studio_runtime_store::RuntimeStore;

    #[test]
    fn bridge_matches_install_layout_in_both_modes() {
        let source_checkout = studio_runtime_store::path_identity::CanonicalPath::existing(
            PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.."),
        )
        .unwrap();
        let source_layout = InstallRoot(source_checkout.clone().to_path_buf());
        assert_eq!(
            source_layout.claude_bridge_dir(),
            source_checkout.join(CLAUDE_BRIDGE_RELATIVE)
        );

        let temporary = tempfile::tempdir().unwrap();
        let packaged_root = temporary.path().join("workspace");
        let packaged_scripts = packaged_root.join("scripts");
        let packaged_bridge = packaged_scripts.join("claude_bridge");
        fs::create_dir_all(&packaged_bridge).unwrap();
        fs::write(packaged_bridge.join("bridge.mjs"), "").unwrap();
        let packaged_layout = InstallRoot(packaged_root);
        assert_eq!(
            packaged_layout.claude_bridge_dir(),
            packaged_scripts.join("claude_bridge")
        );
    }

    #[test]
    fn install_root_resolves_packaged_bridge_relative_to_executable() {
        let temporary = tempfile::tempdir().unwrap();
        let root = temporary.path().join("package");
        let bridge = root.join("scripts/claude_bridge");
        fs::create_dir_all(&bridge).unwrap();
        fs::write(bridge.join("bridge.mjs"), "").unwrap();
        let executable = root.join("bin/studio-runtime-server");
        let resolved = super::resolve_install_root_from_executable(Some(&executable)).unwrap();
        assert_eq!(resolved.workspace_root(), root);
    }

    #[test]
    fn install_root_source_fallback_is_debug_only() {
        // No executable location can find a bridge in this case. A release
        // test build must refuse even while its original checkout still exists.
        let resolved = super::resolve_install_root_from_executable(None);
        #[cfg(debug_assertions)]
        assert_eq!(
            resolved.unwrap().workspace_root(),
            studio_runtime_store::path_identity::CanonicalPath::existing(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../..")
            )
            .unwrap()
        );
        #[cfg(not(debug_assertions))]
        assert!(matches!(
            resolved,
            Err(super::WorkerError::PreparationRefused(message))
                if message.starts_with("runtime-server/install-root:")
        ));
    }

    #[tokio::test]
    async fn host_runtime_serves_session_and_accounts() {
        use axum::{
            body::{Body, to_bytes},
            extract::ConnectInfo,
            http::Request,
        };
        use std::sync::Arc;
        use tower::ServiceExt;

        let root = tempfile::tempdir().expect("isolated runtime state creates");
        let store = Arc::new(
            RuntimeStore::open(Some(root.path()))
                .await
                .expect("runtime store opens"),
        );
        let registry_path = root.path().join("accounts/registry.json");
        fs::create_dir_all(registry_path.parent().expect("registry parent"))
            .expect("account registry directory creates");
        fs::write(
            &registry_path,
            r#"{"version":1,"defaultAccountKey":"default","accounts":{},"logins":{},"deleteReceipts":{}}"#,
        )
        .expect("empty account registry writes");
        let registry_bytes = fs::read(&registry_path).expect("account registry reads");
        let accounts =
            Arc::new(AccountDriver::open(registry_path).expect("empty account registry opens"));
        let account_policy = Arc::new(
            studio_runtime_services::account_policy::AccountPolicyState::from_startup(
                Some(&registry_bytes),
                std::iter::empty::<(String, serde_json::Value)>(),
            )
            .expect("empty account policy loads"),
        );
        let mut state = HttpState::new(
            "host-runtime-token".into(),
            "host-runtime-workspace".into(),
            root.path().to_string_lossy().into_owned(),
            4620,
            None,
            Some(store.clone()),
            Some(account_policy),
        )
        .with_account_driver(accounts.clone());
        let runtime = super::HostRuntime::start(
            &mut state,
            store,
            accounts,
            None,
            super::AccountRegistryConfig::default(),
        )
        .await
        .expect("composed host runtime starts");

        let app = studio_runtime_http::router(state).expect("HTTP routes compose");
        async fn get(app: &axum::Router, path: &str) -> (u16, serde_json::Value) {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .uri(path)
                        .header("host", "127.0.0.1:4620")
                        .extension(ConnectInfo(std::net::SocketAddr::from((
                            [127, 0, 0, 1],
                            12345,
                        ))))
                        .body(Body::empty())
                        .expect("request builds"),
                )
                .await
                .expect("composed HTTP response");
            let status = response.status().as_u16();
            let body = to_bytes(response.into_body(), 4096)
                .await
                .expect("JSON body");
            (
                status,
                serde_json::from_slice(&body).expect("response JSON"),
            )
        }

        let (session_status, session) = get(&app, "/api/session").await;
        assert_eq!(session_status, 200);
        assert_eq!(session["token"], "host-runtime-token");
        let (accounts_status, accounts) = get(&app, "/api/accounts").await;
        assert_eq!(accounts_status, 200);
        assert!(accounts["accounts"].as_array().is_some());

        runtime
            .shutdown()
            .await
            .expect("composed host runtime shuts down");
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
        callback_runtime::{CallbackReplyPort, CallbackRuntime},
        model_catalog::ModelCatalogService,
        provider_callbacks::{CallbackError, Reply},
        provider_session_task::SessionTaskConfig,
    };
    use studio_runtime_store::RuntimeStore;
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
                    replayed: false,
                })
            })
        }
    }

    struct UnusedCallbackReplyPort;

    impl CallbackReplyPort for UnusedCallbackReplyPort {
        fn has_live_session(
            &self,
            _: &studio_runtime_services::provider_callbacks::CallbackContext,
        ) -> bool {
            false
        }

        fn deliver<'a>(
            &'a self,
            _reply: &'a Reply,
            _deadline: std::time::Duration,
        ) -> studio_runtime_services::callback_runtime::CallbackReplyFuture<'a> {
            Box::pin(async {
                Err(CallbackError::Dependency(
                    "test callback reply lane is unused",
                ))
            })
        }
    }

    struct VoiceFactory {
        stop_writes: Arc<AtomicUsize>,
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
                                if observed.await.is_err() {
                                    break;
                                }
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
                                stop_writes.fetch_add(1, Ordering::SeqCst);
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
                    provider_retained: false,
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
            thread_config: Default::default(),
            tools: Vec::new(),
            legacy_tools: Vec::new(),
            skills_root: directory.to_path_buf(),
            role_guidance: BTreeMap::new(),
            model_catalog: ModelCatalogService::new(),
            account_home: String::new(),
            shared_home: String::new(),
            filesystem_inputs: None,
            claude_account_pin: serde_json::Value::Null,
            account_driver: None,
            account_session: None,
            guest_rpc: None,
            vm_provider_launch: None,
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
                wire: studio_runtime_providers::ProviderKind::Codex.wire(),
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
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
        let agent_record = json!({
            "id":"voice-composed-agent", "rootId":"voice-composed-agent", "epoch":3,
            "status":"idle", "isLead":true, "threadId":"voice-thread", "accountKey":"voice-account"
        })
        .to_string();
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_agents(id,record) VALUES(?1,?2)",
                    ("voice-composed-agent", agent_record.as_str()),
                )?;
                Ok(())
            })
            .await
            .unwrap();

        let factory = Arc::new(VoiceFactory {
            stop_writes: Arc::new(AtomicUsize::new(0)),
        });
        let (mut worker, session, callbacks) =
            start_voice_worker(directory.path(), store.clone(), Arc::clone(&factory)).await;
        let mut state = HttpState::new(
            "token".into(),
            "workspace".into(),
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
        // The HTTP acceptance boundary persists the connecting session before
        // provider setup completes; wait on its existing completion barrier
        // before staging deletion from the durable rows.
        tokio::time::timeout(
            super::HOST_RUNTIME_SHUTDOWN_WAIT,
            studio_runtime_services::voice::provider::wait_for_start_setup(
                "voice-composed-agent",
                "voice-session",
            ),
        )
        .await
        .expect("voice provider setup did not finish before timeout");

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
            .await
            .unwrap();
        assert_eq!(delete_intent.sessions.len(), 2);

        // Simulate a restart after the deletion transaction commits. Python
        // marks sessions on the vanished connection lost and never sends their
        // stop over a replacement session; Rust must settle both as Unknown.
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
        let receipts = studio_runtime_services::voice::provider::end_deleted_sessions(
            delete_provider,
            delete_intent.id.clone(),
            delete_intent.sessions.clone(),
            2.0,
        )
        .await
        .unwrap();
        post_delete_worker.shutdown().await.unwrap();
        assert_eq!(receipts.len(), 2);
        assert!(receipts.iter().all(|receipt| {
            receipt.certainty
                == studio_runtime_services::voice::runtime::VoiceWriteCertainty::Unknown
        }));
        assert_eq!(factory.stop_writes.load(Ordering::SeqCst), 0);

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
        assert_eq!(factory.stop_writes.load(Ordering::SeqCst), 0);
        recovered_worker.shutdown().await.unwrap();
    }
}

async fn next_direct_generation(
    store: &RuntimeStore,
    account: &str,
) -> Result<Generation, WorkerError> {
    let account = account.to_owned();
    store.transaction_with(move |tx| {
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
    .await
}

/// The home and isolation of a Codex account whose provider session may start:
/// a ready account, or an account whose sign-in request is being dispatched
/// (the sign-in runs in that session, before the account is ready).
pub fn account_home<R: ProviderAuthReader>(
    accounts: &AccountDriver<R>,
    key: &str,
) -> Result<(String, bool), WorkerError> {
    if let Some(record) = accounts
        .login_launch_record(key)
        .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?
    {
        let home = record["home"]
            .as_str()
            .filter(|home| !home.is_empty())
            .ok_or_else(|| WorkerError::PreparationRefused("account-driver/profile-home".into()))?
            .to_owned();
        return Ok((home, key != "default"));
    }
    let initial = accounts
        .snapshot()
        .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    let initial_profile = initial
        .accounts
        .iter()
        .find(|profile| profile["id"] == key)
        .ok_or_else(|| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    if studio_runtime_providers::ProviderKind::key_of(initial_profile) != Some("codex") {
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
    if studio_runtime_providers::ProviderKind::key_of(profile) != Some("codex") {
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

/// The account's provider, home, isolation and registry profile (Claude: the
/// pinned profile the bridge launch verifies).
fn selected_account(
    accounts: &AccountDriver,
    key: &str,
) -> Result<(ProviderKind, String, bool, serde_json::Value), WorkerError> {
    let snapshot = accounts
        .snapshot()
        .map_err(|_| WorkerError::PreparationRefused("account-driver/unavailable".into()))?;
    // A sign-in account is not listed until it has an identity; its sign-in
    // request still launches its session.
    let profile = match snapshot
        .accounts
        .iter()
        .find(|profile| profile["id"] == key)
    {
        Some(profile) => profile.clone(),
        None => accounts
            .login_launch_record(key)
            .ok()
            .flatten()
            .ok_or_else(|| WorkerError::PreparationRefused("account-driver/unavailable".into()))?,
    };
    let kind = ProviderKind::of_record(&profile).map_err(|_| {
        WorkerError::PreparationRefused("provider-session/unsupported-provider".into())
    })?;
    if kind == ProviderKind::Codex {
        let (home, isolated) = account_home(accounts, key)?;
        return Ok((kind, home, isolated, profile));
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
    Ok((kind, home, false, profile))
}

#[doc(hidden)]
pub fn codex_launch(
    native_path: Option<&str>,
    environment: BTreeMap<String, String>,
    home: String,
    isolated: bool,
    root: PathBuf,
) -> ProviderLaunchSpec {
    adapter(ProviderKind::Codex)
        .host_launch(HostLaunch {
            profile: &serde_json::Value::Null,
            home,
            isolated,
            codex_executable: native_path,
            claude_bridge_dir: Path::new(""),
            account: "default",
            root,
            environment,
        })
        .expect("Codex launch is infallible")
        .spec
}

#[doc(hidden)]
pub fn claude_launch(
    profile: serde_json::Value,
    bridge_dir: &Path,
    state_root: &Path,
    account: &str,
    environment: BTreeMap<String, String>,
) -> Result<ProviderLaunchSpec, WorkerError> {
    adapter(ProviderKind::Claude)
        .host_launch(HostLaunch {
            profile: &profile,
            home: String::new(),
            isolated: false,
            codex_executable: None,
            claude_bridge_dir: bridge_dir,
            account,
            root: state_root.to_path_buf(),
            environment,
        })
        .map(|launch| launch.spec)
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod vm_claude_refresh_tests {
    use super::{
        CLAUDE_ACCESS_REFRESH_WINDOW_MS, WorkerError, encode_vm_claude_credentials,
        read_vm_claude_oauth, run_vm_claude_refresh_helper, unix_time_ms, verify_vm_claude_account,
    };
    use serde_json::{Value, json};
    use std::{collections::BTreeMap, fs, os::unix::fs::PermissionsExt};

    #[test]
    fn existing_refresh_helper_handshake_waits_for_a_fresh_access_token() {
        let root = tempfile::tempdir().unwrap();
        let config = root.path().join("claude-profile");
        fs::create_dir_all(&config).unwrap();
        let credential_path = config.join(".credentials.json");
        let now_ms = unix_time_ms().unwrap();
        fs::write(
            &credential_path,
            serde_json::to_vec(&json!({"claudeAiOauth": {
                "accessToken":"expired-fake-token",
                "expiresAt":now_ms + 1000.0,
                "scopes":["user:inference"]
            }}))
            .unwrap(),
        )
        .unwrap();
        let helper = root.path().join("bridge.mjs");
        fs::write(&helper, "// fake helper entrypoint\n").unwrap();
        let node = root.path().join("fake-node");
        fs::write(
            &node,
            r##"#!/bin/sh
[ "${1##*/}" = "bridge.mjs" ] || exit 1
[ "$2" = "--refresh-auth" ] || exit 1
cat > "$CLAUDE_CONFIG_DIR/.credentials.json" <<'JSON'
{"claudeAiOauth":{"accessToken":"fresh-fake-token","expiresAt":4102444800000,"scopes":["user:inference"],"subscriptionType":"pro"}}
JSON
printf '{"ready":true}\n'
IFS= read -r release
[ "$release" = "done" ]
"##,
        )
        .unwrap();
        let mut permissions = fs::metadata(&node).unwrap().permissions();
        permissions.set_mode(0o755);
        fs::set_permissions(&node, permissions).unwrap();
        let environment = BTreeMap::from([
            (
                std::ffi::OsString::from("CLAUDE_CONFIG_DIR"),
                config.as_os_str().to_owned(),
            ),
            (
                std::ffi::OsString::from("STUDIO_CLAUDE_BIN"),
                std::ffi::OsString::from("fake-claude"),
            ),
            (
                std::ffi::OsString::from("STUDIO_CLAUDE_ACCOUNT"),
                std::ffi::OsString::from("fake@example.invalid"),
            ),
        ]);

        run_vm_claude_refresh_helper(&node, &helper, &environment, || {
            let access = read_vm_claude_oauth(&credential_path, true)
                .map_err(|_| WorkerError::TaskFailed)?;
            Ok(access["expiresAt"].as_f64().unwrap()
                > unix_time_ms().unwrap() + CLAUDE_ACCESS_REFRESH_WINDOW_MS)
        })
        .unwrap();

        let access = read_vm_claude_oauth(&credential_path, true).unwrap();
        assert_eq!(access["accessToken"], "fresh-fake-token");
        assert!(access["expiresAt"].as_f64().unwrap() > now_ms);
    }

    #[test]
    fn refreshed_claude_access_requires_the_same_ready_account_identity() {
        let root = tempfile::tempdir().unwrap();
        let registry = root.path().join("accounts/registry.json");
        fs::create_dir_all(registry.parent().unwrap()).unwrap();
        let account = json!({
            "id": "claude-test",
            "provider": "claude",
            "home": root.path().join("home").to_string_lossy(),
            "label": "fixture",
            "source": "test",
            "status": "ready",
            "email": "fixture@example.invalid",
            "accountId": "claude:fixture@example.invalid",
            "claudeOptions": {}
        });
        fs::write(
            &registry,
            json!({
                "version": 1,
                "defaultAccountKey": "claude-test",
                "accounts": {"claude-test": account},
                "logins": {},
                "deleteReceipts": {}
            })
            .to_string(),
        )
        .unwrap();
        let accounts =
            studio_runtime_services::account_driver::AccountDriver::open(registry).unwrap();
        let account = accounts.snapshot().unwrap().accounts[0].clone();

        verify_vm_claude_account(
            &accounts,
            "claude-test",
            &account,
            &accounts.auth_pin("claude-test").unwrap(),
            "fixture@example.invalid",
            false,
        )
        .unwrap();
        assert!(
            verify_vm_claude_account(
                &accounts,
                "claude-test",
                &account,
                &accounts.auth_pin("claude-test").unwrap(),
                "other@example.invalid",
                false,
            )
            .is_err()
        );
    }

    #[test]
    fn guest_credential_copy_excludes_host_refresh_material() {
        let access = json!({
            "accessToken": "fake-access-token",
            "expiresAt": 4102444800000.0,
            "scopes": ["user:inference"],
            "refreshToken": "fake-refresh-token",
            "unknownHostField": "private"
        });
        let copied: Value = serde_json::from_slice(
            &encode_vm_claude_credentials(access.as_object().unwrap()).unwrap(),
        )
        .unwrap();
        assert_eq!(
            copied,
            json!({"claudeAiOauth": {
                "accessToken": "fake-access-token",
                "expiresAt": 4102444800000.0,
                "scopes": ["user:inference"]
            }})
        );
    }
}
