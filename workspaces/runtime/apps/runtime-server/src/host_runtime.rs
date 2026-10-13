//! Binary composition of retained account sessions. The supervisor is an
//! independently installed service; backend shutdown never takes its children.
use crate::account_registry::{AccountLaunch, AccountLauncher, AccountRegistry, LaunchFuture};
use serde_json::json;
use std::{collections::BTreeMap, path::PathBuf, sync::Arc};
use studio_runtime_http::HttpState;
use studio_runtime_process::{
    BackendId, BackendIdentity, OperationId, ProviderHandle, ProviderLaunchSpec,
    provider_seam::{OpenOptions, SessionClock, Shutdown},
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
            accounts,
            root: store.root().to_owned(),
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
    accounts: Arc<AccountDriver>,
    root: PathBuf,
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
            // Existing catalog refresh owns its bounded one-off child; both
            // HTTP reads and preparation observe this same retained service.
            let _catalog_status = self
                .resources
                .model_catalog
                .read(&account, &resources.account_home, "codex", false)
                .await;
            let clock = SessionClock::default();
            let prepared = self
                .factory
                .prepare(
                    ProviderHandle::for_account(&account),
                    OpenOptions {
                        clock: clock.clone(),
                        launch: launch.clone(),
                        expected_generation: None,
                    },
                    Shutdown::default(),
                )
                .await
                .map_err(studio_runtime_services::provider_session_task::SessionTaskError::from)?;
            let generation = prepared.generation();
            Ok(AccountLaunch {
                factory: Arc::new(prepared),
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
