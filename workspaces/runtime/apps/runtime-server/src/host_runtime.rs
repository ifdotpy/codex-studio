//! Binary composition of retained account sessions. The supervisor is an
//! independently installed service; backend shutdown never takes its children.
use crate::account_registry::{AccountLaunch, AccountLauncher, AccountRegistry, LaunchFuture};
use serde_json::json;
use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
    sync::Arc,
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
}

impl HostRuntime {
    #[cfg(test)]
    #[allow(dead_code)] // Integration tests use these constructors from a separate crate.
    pub async fn start(
        state: &mut HttpState,
        store: Arc<RuntimeStore>,
        accounts: Arc<AccountDriver>,
        native_path: Option<String>,
    ) -> Result<Self, WorkerError> {
        Self::start_with_monitor_wake(
            state,
            store,
            accounts,
            native_path,
            Arc::new(tokio::sync::Notify::new()),
        )
        .await
    }

    pub async fn start_with_monitor_wake(
        state: &mut HttpState,
        store: Arc<RuntimeStore>,
        accounts: Arc<AccountDriver>,
        native_path: Option<String>,
        monitor_dispatch_wake: Arc<tokio::sync::Notify>,
    ) -> Result<Self, WorkerError> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| WorkerError::TaskFailed)?;
        let identity = BackendIdentity {
            backend_id: BackendId(data_encoding::HEXLOWER.encode(&nonce)),
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
        *state = state.clone().with_tool_catalog(CatalogSnapshot {
            source: CatalogSource::Current,
            definitions: resources.tools.clone(),
            saved_thread_id: None,
            saved_digest: None,
        });
        let launcher = Arc::new(HostLauncher {
            factory: SupervisorProviderTaskFactory::new(identity),
            supervisor_mode: std::env::var("CODEX_AGENTS_SUPERVISOR_MODE")
                .is_ok_and(|value| value == "1"),
            child_priority: ChildPriority::from_environment(),
            accounts,
            store: store.clone(),
            root: store.root().to_owned(),
            native_path,
            resources,
            claude_bridge_dir,
            environment: std::env::vars().collect(),
        });
        Self::with_launcher_and_monitor_wake(
            store,
            launcher,
            state.event_wake_handle(),
            monitor_dispatch_wake,
        )
        .await
    }

    #[cfg(test)]
    #[allow(dead_code)] // Integration tests use these constructors from a separate crate.
    pub async fn with_launcher(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        wake: Arc<tokio::sync::Notify>,
    ) -> Result<Self, WorkerError> {
        Self::with_launcher_and_monitor_wake(
            store,
            launcher,
            wake,
            Arc::new(tokio::sync::Notify::new()),
        )
        .await
    }

    pub async fn with_launcher_and_monitor_wake(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        wake: Arc<tokio::sync::Notify>,
        monitor_dispatch_wake: Arc<tokio::sync::Notify>,
    ) -> Result<Self, WorkerError> {
        let registry =
            AccountRegistry::new_with_monitor_wake(store, launcher, monitor_dispatch_wake).await?;
        let shutdown = Shutdown::default();
        let task = tokio::spawn(registry.run(wake, shutdown.clone()));
        Ok(Self { shutdown, task })
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

struct HostLauncher {
    factory: SupervisorProviderTaskFactory,
    supervisor_mode: bool,
    child_priority: ChildPriority,
    accounts: Arc<AccountDriver>,
    store: Arc<RuntimeStore>,
    root: PathBuf,
    native_path: Option<String>,
    resources: PreparationResources,
    claude_bridge_dir: PathBuf,
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
            let mut resources = self.resources.clone();
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
            let launch = if let Some(profile) = claude_profile {
                claude_launch(
                    profile,
                    &self.claude_bridge_dir,
                    self.root.clone(),
                    self.environment.clone(),
                )?
            } else {
                codex_launch(
                    self.native_path.as_deref(),
                    self.environment.clone(),
                    home,
                    isolated,
                    self.root.clone(),
                )
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
            })
        })
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

pub(crate) fn claude_launch(
    profile: serde_json::Value,
    bridge_dir: &Path,
    root: PathBuf,
    mut environment: BTreeMap<String, String>,
) -> Result<ProviderLaunchSpec, WorkerError> {
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
    snapshot
        .launch(
            ResolvedBridge {
                node,
                executable: claude,
                bridge,
                root,
                sdk_present,
            },
            &metadata,
            std::mem::take(&mut environment),
        )
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))
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
