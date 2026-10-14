//! Binary composition of retained account sessions. The supervisor is an
//! independently installed service; backend shutdown never takes its children.
use crate::account_registry::{AccountLaunch, AccountLauncher, AccountRegistry, LaunchFuture};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
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
    provider_session_task::SessionTaskConfig,
    thread_prepare::{CatalogSnapshot, CatalogSource},
};
use studio_runtime_store::RuntimeStore;
use tokio::task::JoinHandle;

const REQUEST_TIMEOUT_MS: u64 = 60_000;

pub(crate) struct HostRuntime {
    shutdown: Shutdown,
    task: JoinHandle<Result<(), WorkerError>>,
}

impl HostRuntime {
    pub async fn start(
        state: &mut HttpState,
        store: Arc<RuntimeStore>,
        accounts: Arc<AccountDriver>,
        native_path: Option<String>,
    ) -> Result<Self, WorkerError> {
        let mut nonce = [0_u8; 16];
        getrandom::fill(&mut nonce).map_err(|_| WorkerError::TaskFailed)?;
        let identity = BackendIdentity {
            backend_id: BackendId(data_encoding::HEXLOWER.encode(&nonce)),
            state_dir: store.root().to_owned(),
        };
        let python =
            resolve_python(std::env::var_os("CODEX_AGENTS_PYTHON").map(PathBuf::from)).await?;
        let provider_python = python.clone();
        let catalog = state.model_catalog_handle();
        let resources = tokio::task::spawn_blocking(move || {
            PreparationResources::bundled(String::new(), catalog, python)
        })
        .await
        .map_err(|_| WorkerError::TaskFailed)??;
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
            python: provider_python,
            native_path,
            resources,
            environment: std::env::vars().collect(),
        });
        Self::with_launcher(store, launcher, state.event_wake_handle()).await
    }

    pub async fn with_launcher(
        store: Arc<RuntimeStore>,
        launcher: Arc<dyn AccountLauncher>,
        wake: Arc<tokio::sync::Notify>,
    ) -> Result<Self, WorkerError> {
        let registry = AccountRegistry::new(store, launcher).await?;
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
    python: PathBuf,
    native_path: Option<String>,
    resources: PreparationResources,
    environment: BTreeMap<String, String>,
}

impl AccountLauncher for HostLauncher {
    fn launch(&self, account: String) -> LaunchFuture<'_> {
        Box::pin(async move {
            let accounts = self.accounts.clone();
            let key = account.clone();
            let (home, isolated) =
                tokio::task::spawn_blocking(move || account_home(&accounts, &key))
                    .await
                    .map_err(|_| WorkerError::TaskFailed)??;
            let mut resources = self.resources.clone();
            resources.account_home.clone_from(&home);
            let launch = codex_launch(
                self.native_path.as_deref(),
                self.environment.clone(),
                home,
                isolated,
                self.root.clone(),
            );
            let launch = provider_process_command(&self.python, launch).await?;
            // Existing catalog refresh owns its bounded one-off child; both
            // HTTP reads and preparation observe this same retained service.
            let _catalog_status = self
                .resources
                .model_catalog
                .read(&account, &resources.account_home, "codex", false)
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
                    claude: false,
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

async fn resolve_python(configured: Option<PathBuf>) -> Result<PathBuf, WorkerError> {
    let invalid = || WorkerError::PreparationRefused("preparation/resolved-python-required".into());
    if let Some(path) = configured {
        return if path.is_absolute() {
            Ok(path)
        } else {
            Err(invalid())
        };
    }
    let mut command = tokio::process::Command::new("python3");
    command
        .arg("-B")
        .arg(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src/codex_python.py"))
        .kill_on_drop(true);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), command.output())
        .await
        .map_err(|_| invalid())?
        .map_err(|_| invalid())?;
    if !output.status.success() {
        return Err(invalid());
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

async fn provider_process_command(
    python: &std::path::Path,
    launch: ProviderLaunchSpec,
) -> Result<ProviderLaunchSpec, WorkerError> {
    run_provider_process_command(python, launch, None).await
}

#[cfg(test)]
#[allow(
    dead_code,
    reason = "the integration test injects the Python wrapper prefix"
)]
pub(crate) async fn provider_process_command_with_prefix(
    python: &std::path::Path,
    launch: ProviderLaunchSpec,
    prefix: Vec<String>,
) -> Result<ProviderLaunchSpec, WorkerError> {
    run_provider_process_command(python, launch, Some(prefix)).await
}

async fn run_provider_process_command(
    python: &std::path::Path,
    mut launch: ProviderLaunchSpec,
    injected_prefix: Option<Vec<String>>,
) -> Result<ProviderLaunchSpec, WorkerError> {
    use tokio::io::AsyncWriteExt;

    let invalid =
        || WorkerError::PreparationRefused("provider-session/provider-process-command".into());
    let source = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../server/src");
    let (script, extra_args) = if let Some(prefix) = injected_prefix {
        let prefix = serde_json::to_string(&prefix).map_err(|_| invalid())?;
        (
            "import json,shutil,sys,types\n".to_owned()
                + "module=types.ModuleType('codex_workspace_images')\n"
                + "module.exec_prefix=lambda:json.loads(sys.argv[2])\n"
                + "sys.modules['codex_workspace_images']=module\n"
                + "shutil.which=lambda value:value if value==json.loads(sys.argv[2])[0] else None\n"
                + "sys.path.insert(0,sys.argv[1])\n"
                + "from codex_runtime import provider_process_command\n"
                + "print(json.dumps(provider_process_command(json.load(sys.stdin))))\n",
            vec![prefix],
        )
    } else {
        (
            "import json,sys\nsys.path.insert(0,sys.argv[1])\nfrom codex_runtime import provider_process_command\nprint(json.dumps(provider_process_command(json.load(sys.stdin))))\n".into(),
            vec![],
        )
    };
    let mut child = tokio::process::Command::new(python)
        .args(["-B", "-c", &script])
        .arg(source)
        .args(extra_args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .map_err(|_| invalid())?;
    let input = serde_json::to_vec(&launch.command).map_err(|_| invalid())?;
    let mut stdin = child.stdin.take().ok_or_else(invalid)?;
    stdin.write_all(&input).await.map_err(|_| invalid())?;
    drop(stdin);
    let output = tokio::time::timeout(std::time::Duration::from_secs(10), child.wait_with_output())
        .await
        .map_err(|_| invalid())?
        .map_err(|_| invalid())?;
    if !output.status.success() || output.stdout.len() > 64 * 1024 {
        return Err(invalid());
    }
    launch.command = serde_json::from_slice(&output.stdout).map_err(|_| invalid())?;
    if launch.command.is_empty() || launch.command.iter().any(|arg| arg.contains('\0')) {
        return Err(invalid());
    }
    Ok(launch)
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
