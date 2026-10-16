use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware as axum_middleware;
use axum::response::Response;
use axum::routing::any;
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use studio_runtime_process::operational_log::OperationalLog;
use studio_runtime_services::change_notifications::ChangeNotificationHub;
use studio_runtime_services::token_rate::TokenRateHub;
use studio_runtime_store::RuntimeStore;
use tokio::sync::Notify;

const COST_REFRESH_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(9);

#[derive(Default)]
struct CostRefreshWorkers {
    active: AtomicUsize,
    idle: Notify,
}

pub(crate) struct CostRefreshWorkerGuard(Arc<CostRefreshWorkers>);

impl Drop for CostRefreshWorkerGuard {
    fn drop(&mut self) {
        if self.0.active.fetch_sub(1, Ordering::AcqRel) == 1 {
            self.0.idle.notify_waiters();
        }
    }
}

impl CostRefreshWorkers {
    fn track(self: &Arc<Self>) -> CostRefreshWorkerGuard {
        self.active.fetch_add(1, Ordering::AcqRel);
        CostRefreshWorkerGuard(Arc::clone(self))
    }

    async fn wait_idle(&self) {
        loop {
            let notified = self.idle.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if self.active.load(Ordering::Acquire) == 0 {
                return;
            }
            notified.await;
        }
    }
}

#[cfg(unix)]
use studio_runtime_services::terminal_driver::TerminalDriver;
#[cfg(unix)]
use studio_runtime_services::terminal_pty::PtyTerminalSession;

use crate::error::error_response;
use crate::middleware;
use crate::routes::{self, compose};
use crate::schema::{matching_template, operations};
use crate::validation::Registry;

pub(crate) const BROWSER_HOST_FOLDER_NAME: &str = "browser-host";
pub(crate) const BROWSER_HOST_CONTROL_SOCKET_NAME: &str = "control.sock";

#[derive(Clone)]
pub struct HttpState {
    /// Host policy; absent uses pre-resolved workspace paths and the named budget refusal.
    pub message_admission:
        Option<Arc<dyn studio_runtime_services::message_admission::MessageAdmission>>,
    pub(crate) callback_runtime:
        Option<Arc<studio_runtime_services::callback_runtime::CallbackRuntime>>,
    pub(crate) browser_forwarder: Option<Arc<studio_runtime_services::browser::BrowserForwarder>>,
    pub(crate) browser_local_forwarder:
        Option<Arc<studio_runtime_services::browser::BrowserForwarder>>,
    pub(crate) browser_answer_guest:
        Option<Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc>>,
    pub(crate) browser_answer_host_guest:
        Option<Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc>>,
    pub(crate) browser_host_local_forwarder:
        Option<Arc<studio_runtime_services::browser::BrowserForwarder>>,
    pub(crate) orchestration_driver:
        Option<Arc<studio_runtime_services::multi_server_orchestration::OrchestrationDriver>>,
    pub(crate) server_discovery:
        Option<Arc<dyn studio_runtime_services::multi_server_discovery::ServerDiscoveryPort>>,
    pub(crate) tailscale_whoami: Option<
        Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, Option<Value>> + Send + Sync>,
    >,
    pub(crate) project_secrets_guest:
        Option<Arc<dyn studio_runtime_services::project_secrets::GuestRpc>>,
    pub(crate) vault_job_client: Option<crate::routes::VaultJobClient>,
    pub(crate) vault_host_command: Option<studio_runtime_services::vault::VaultHostCommand>,
    pub account_session:
        Option<Arc<dyn studio_runtime_services::account_session_runtime::AccountSessionPort>>,
    pub claude_login: Option<Arc<studio_runtime_services::claude_login::service::LoginService>>,
    pub(crate) multi_server_boundary:
        Option<Arc<studio_runtime_services::multi_server_pairing::MultiServerBoundary>>,
    pub(crate) peer_link_advert: Arc<std::sync::RwLock<Option<String>>>,
    pub account_driver:
        Option<std::sync::Arc<studio_runtime_services::account_driver::AccountDriver>>,
    pub provider_versions:
        Option<Arc<studio_runtime_services::provider_versions::ProviderVersionMonitor>>,
    pub(crate) native_runtime: Option<Arc<studio_runtime_services::native_runtime::NativeRuntime>>,
    pub(crate) title_request:
        Option<Arc<dyn studio_runtime_services::title_generation::TitleRequestPort>>,
    pub(crate) voice_request_port:
        Option<Arc<dyn studio_runtime_services::voice::runtime::VoiceRequestPort>>,
    pub(crate) credentials: Arc<studio_runtime_services::federation_credentials::Credentials>,
    pub(crate) token: String,
    pub(crate) workspace_id: String,
    pub(crate) native_path: Option<String>,
    pub(crate) state_dir: String,
    pub(crate) cost_scan_cancelled: Arc<std::sync::atomic::AtomicBool>,
    cost_refresh_workers: Arc<CostRefreshWorkers>,
    pub(crate) cost_scanner_build_script: Option<PathBuf>,
    pub(crate) vm_state_dir: std::path::PathBuf,
    pub(crate) app_server_log: Option<OperationalLog>,
    pub(crate) port: u16,
    pub(crate) public_origin: Option<String>,
    pub(crate) validators: Registry,
    pub(crate) store: Option<Arc<RuntimeStore>>,
    pub(crate) diagnostics_provider:
        Option<Arc<dyn studio_runtime_services::diagnostics::ProviderDiagnosticsPort>>,
    pub(crate) diagnostics_supervisor:
        Option<Arc<dyn studio_runtime_services::diagnostics::SupervisorDiagnosticsPort>>,
    #[cfg(unix)]
    terminal_driver: Option<Arc<TerminalDriver<PtyTerminalSession>>>,
    #[cfg(unix)]
    terminal_write_gate:
        Option<Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, bool> + Send + Sync>>,
    pub(crate) account_policy:
        Option<Arc<studio_runtime_services::account_policy::AccountPolicyState>>,
    pub(crate) tool_catalog: Option<Arc<studio_runtime_services::thread_prepare::CatalogSnapshot>>,
    pub(crate) model_catalog: studio_runtime_services::model_catalog::ModelCatalogService,
    pub(crate) capabilities_cache: Arc<studio_runtime_services::catalog_reads::CapabilitiesCache>,
    pub(crate) resource_notifications:
        Arc<studio_runtime_services::resource_notify::ResourceNotifications>,
    pub(crate) sync_changes: Arc<ChangeNotificationHub>,
    pub(crate) token_rates: Arc<TokenRateHub>,
    pub(crate) event_wake: Arc<Notify>,
    pub(crate) continuation_wake: Arc<Notify>,
    pub(crate) etag_operations: BTreeSet<(String, String)>,
    mutation_tasks: Arc<crate::mutation_tasks::MutationTasks>,
}

impl HttpState {
    /// Open provider stderr logging at retained app-server construction time,
    /// after the HTTP state itself has been assembled.
    pub fn open_app_server_log(mut self) -> Self {
        let log = OperationalLog::app_server(std::path::Path::new(&self.state_dir)).ok();
        let changes = Arc::clone(&self.sync_changes);
        self.model_catalog = studio_runtime_services::model_catalog::ModelCatalogService::with_change_callback_and_log(
            self.native_path.clone(),
            Arc::new(move || changes.notify_one()),
            log.clone(),
        );
        self.app_server_log = log;
        self
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        token: String,
        workspace_id: String,
        native_path: Option<String>,
        state_dir: String,
        port: u16,
        public_origin: Option<String>,
        store: Option<Arc<RuntimeStore>>,
        account_policy: Option<Arc<studio_runtime_services::account_policy::AccountPolicyState>>,
    ) -> Self {
        let vm_state_dir =
            studio_runtime_services::vault::linux_vm_state_dir(std::path::Path::new(&state_dir));
        let sync_changes = Arc::new(ChangeNotificationHub::new());
        let project_accounts = account_policy.as_ref().map(|policy| policy.accounts());
        let orchestration_driver = store.as_ref().map(|store| {
            Arc::new(studio_runtime_services::multi_server_orchestration::OrchestrationDriver::new(
                Arc::clone(store),
                Arc::new(studio_runtime_services::multi_server_orchestration::HttpsPairedTransport::new(Arc::clone(store))),
                Arc::new(studio_runtime_services::multi_server_orchestration::RuntimeOrchestrationEffects::with_project_accounts(
                    Arc::clone(store),
                    Arc::new(studio_runtime_services::browser::BrowserCallbackReceiver::new(Arc::clone(store))),
                    project_accounts.as_ref()
                        .and_then(|snapshot| snapshot.default_account_key.as_str())
                        .unwrap_or("default")
                        .to_owned(),
                    project_accounts.as_ref().map(|snapshot| snapshot.accounts.clone()).unwrap_or_default(),
                )),
            ))
        });
        let browser_forwarder = orchestration_driver
            .as_ref()
            .map(|driver| crate::routes::browser::new_forwarder(Arc::clone(driver)));
        let browser_answer_guest: Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc> =
            Arc::new(
                studio_runtime_services::linux_vm_guest::UnixGuestRpc::for_runtime_state(
                    &vm_state_dir,
                ),
            );
        let browser_answer_host_guest: Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc> =
            Arc::new(
                studio_runtime_services::linux_vm_guest::UnixSocketGuestRpc::new(
                    std::path::Path::new(&state_dir)
                        .join(BROWSER_HOST_FOLDER_NAME)
                        .join(BROWSER_HOST_CONTROL_SOCKET_NAME),
                ),
            );
        let browser_local_forwarder = Some(crate::routes::browser::new_local_forwarder(
            Arc::clone(&browser_answer_guest),
        ));
        let browser_host_local_forwarder = Some(crate::routes::browser::new_local_forwarder(
            Arc::clone(&browser_answer_host_guest),
        ));
        let model_changes = Arc::clone(&sync_changes);
        let model_catalog =
            studio_runtime_services::model_catalog::ModelCatalogService::with_change_callback_and_log(
                native_path.clone(),
                Arc::new(move || model_changes.notify_one()),
                None,
            );
        if let Some(store) = &store {
            let changes = Arc::clone(&sync_changes);
            store.set_commit_notifier(move || changes.notify_one());
            store.set_diagnostics_callback(Some(
                studio_runtime_services::diagnostics::observe_store_event,
            ));
        }
        Self {
            message_admission: None,
            callback_runtime: None,
            browser_forwarder,
            browser_local_forwarder,
            browser_answer_guest: Some(browser_answer_guest),
            browser_answer_host_guest: Some(browser_answer_host_guest),
            browser_host_local_forwarder,
            orchestration_driver,
            server_discovery: None,
            tailscale_whoami: None,
            project_secrets_guest: None,
            vault_job_client: None,
            vault_host_command: None,
            account_session: None,
            account_driver: None,
            provider_versions: None,
            native_runtime: None,
            title_request: None,
            claude_login: None,
            voice_request_port: None,
            credentials: Arc::default(),
            multi_server_boundary: None,
            peer_link_advert: Arc::new(std::sync::RwLock::new(None)),
            token,
            workspace_id,
            native_path,
            state_dir,
            cost_scan_cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cost_refresh_workers: Arc::new(CostRefreshWorkers::default()),
            cost_scanner_build_script: None,
            vm_state_dir,
            app_server_log: None,
            port,
            public_origin,
            validators: Registry::default(),
            store,
            diagnostics_provider: None,
            diagnostics_supervisor: None,
            #[cfg(unix)]
            terminal_driver: None,
            #[cfg(unix)]
            terminal_write_gate: None,
            account_policy,
            tool_catalog: None,
            model_catalog,
            capabilities_cache: Arc::new(Default::default()),
            resource_notifications: Arc::new(
                studio_runtime_services::resource_notify::ResourceNotifications::new(Arc::clone(
                    &sync_changes,
                )),
            ),
            sync_changes: Arc::clone(&sync_changes),
            token_rates: Arc::new(TokenRateHub::with_change_hub(Arc::clone(&sync_changes))),
            event_wake: Arc::new(Notify::new()),
            continuation_wake: Arc::new(Notify::new()),
            etag_operations: BTreeSet::new(),
            mutation_tasks: Arc::new(crate::mutation_tasks::MutationTasks::new()),
        }
    }

    pub fn with_multi_server_boundary(
        mut self,
        boundary: Arc<studio_runtime_services::multi_server_pairing::MultiServerBoundary>,
    ) -> Self {
        self.multi_server_boundary = Some(boundary);
        self
    }

    /// Attach the host Tailscale status and discovery probe adapter.
    pub fn with_server_discovery(
        mut self,
        discovery: Arc<dyn studio_runtime_services::multi_server_discovery::ServerDiscoveryPort>,
    ) -> Self {
        self.server_discovery = Some(discovery);
        self
    }

    /// Attach the host Tailscale whoami probe used when creating federation invites.
    pub fn with_tailscale_whoami(
        mut self,
        whoami: Arc<
            dyn Fn() -> futures_util::future::BoxFuture<'static, Option<Value>> + Send + Sync,
        >,
    ) -> Self {
        self.tailscale_whoami = Some(whoami);
        self
    }

    /// Advertise the running QUIC link to authenticated paired-server requests.
    pub fn with_peer_link_advert(mut self, advert: Arc<std::sync::RwLock<Option<String>>>) -> Self {
        self.peer_link_advert = advert;
        self
    }

    /// Attach the Linux VM guest client used by project-secrets routes.
    pub fn with_project_secrets_guest_rpc(
        mut self,
        guest: Arc<dyn studio_runtime_services::project_secrets::GuestRpc>,
    ) -> Self {
        self.project_secrets_guest = Some(guest);
        self
    }

    /// Attach the guest bridge used to answer browser sign-in requests.
    pub fn with_browser_answer_guest_rpc(
        mut self,
        guest: Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc>,
    ) -> Self {
        self.browser_answer_guest = Some(guest);
        self.browser_local_forwarder = self
            .browser_answer_guest
            .as_ref()
            .map(|guest| crate::routes::browser::new_local_forwarder(Arc::clone(guest)));
        self
    }

    pub fn with_browser_answer_host_guest_rpc(
        mut self,
        guest: Arc<dyn studio_runtime_services::linux_vm_guest::GuestRpc>,
    ) -> Self {
        self.browser_answer_host_guest = Some(guest.clone());
        self.browser_host_local_forwarder =
            Some(crate::routes::browser::new_local_forwarder(guest));
        self
    }

    /// Attach the runtime-owned vault job client used by setup and login routes.
    pub fn with_vault_job_client(mut self, client: crate::routes::VaultJobClient) -> Self {
        self.vault_job_client = Some(client);
        self
    }

    /// Attach the optional host tool location used for vault setup.
    pub fn with_vault_host_command(mut self, server_source: std::path::PathBuf) -> Self {
        self.vault_host_command = Some(studio_runtime_services::vault::VaultHostCommand::new(
            server_source,
        ));
        self
    }

    /// Override the VM data directory for an isolated runtime or test fixture.
    pub fn with_vm_state_dir(mut self, path: std::path::PathBuf) -> Self {
        self.vm_state_dir = path;
        self
    }

    /// Attach the cached provider and loaded-thread diagnostics source.
    pub fn with_diagnostics_provider(
        mut self,
        provider: Arc<dyn studio_runtime_services::diagnostics::ProviderDiagnosticsPort>,
    ) -> Self {
        self.diagnostics_provider = Some(provider);
        self
    }

    /// Attach runtime-server's current supervisor state.
    pub fn with_diagnostics_supervisor(
        mut self,
        supervisor: Arc<dyn studio_runtime_services::diagnostics::SupervisorDiagnosticsPort>,
    ) -> Self {
        self.diagnostics_supervisor = Some(supervisor);
        self
    }

    /// Attach the retained provider callback reply adapter.
    pub fn with_callback_runtime(
        mut self,
        runtime: Arc<studio_runtime_services::callback_runtime::CallbackRuntime>,
    ) -> Self {
        self.callback_runtime = Some(runtime);
        self
    }

    /// Attach the retained account refresh driver used by account admission.
    pub fn with_account_driver(
        mut self,
        driver: Arc<studio_runtime_services::account_driver::AccountDriver>,
    ) -> Self {
        self.account_driver = Some(driver);
        self
    }

    /// Attach the single retained provider-version monitor used by desktop reads.
    pub fn with_provider_versions(
        mut self,
        monitor: Arc<studio_runtime_services::provider_versions::ProviderVersionMonitor>,
    ) -> Self {
        self.provider_versions = Some(monitor);
        self
    }

    /// Attach the native executable runtime used for lease and desktop status.
    pub fn with_native_runtime(
        mut self,
        runtime: Arc<studio_runtime_services::native_runtime::NativeRuntime>,
    ) -> Self {
        self.native_runtime = Some(runtime);
        self
    }

    pub fn with_credentials(
        mut self,
        credentials: Arc<studio_runtime_services::federation_credentials::Credentials>,
    ) -> Self {
        self.credentials = credentials;
        self
    }

    pub fn with_title_request_port(
        mut self,
        port: Arc<dyn studio_runtime_services::title_generation::TitleRequestPort>,
    ) -> Self {
        self.title_request = Some(port);
        self
    }

    /// Attach the account worker's generic provider-session voice adapter.
    pub fn with_voice_request_port(
        mut self,
        provider: Arc<dyn studio_runtime_services::voice::runtime::VoiceRequestPort>,
    ) -> Self {
        self.voice_request_port = Some(provider);
        self
    }

    pub fn voice_request_port_handle(
        &self,
    ) -> Option<Arc<dyn studio_runtime_services::voice::runtime::VoiceRequestPort>> {
        self.voice_request_port.clone()
    }

    /// Attach the full selected native tool catalog for capabilities reads.
    pub fn with_tool_catalog(
        mut self,
        catalog: studio_runtime_services::thread_prepare::CatalogSnapshot,
    ) -> Self {
        self.tool_catalog = Some(Arc::new(catalog));
        self
    }

    pub async fn shutdown_services(&self) -> std::io::Result<()> {
        self.cost_scan_cancelled
            .store(true, std::sync::atomic::Ordering::Release);
        let cost_refresh = tokio::time::timeout(
            COST_REFRESH_SHUTDOWN_WAIT,
            self.cost_refresh_workers.wait_idle(),
        )
        .await
        .map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                "local cost refresh workers did not stop within their shutdown bound",
            )
        });
        let model = self.model_catalog.shutdown().await;
        if let Some(monitor) = &self.provider_versions {
            monitor.shutdown().await;
        }
        if let Some(login) = &self.claude_login {
            login.shutdown().await.map_err(std::io::Error::other)?;
        }
        cost_refresh?;
        model
    }

    /// Stop admitting API mutations after the server begins graceful shutdown.
    pub fn close_mutation_admission(&self) {
        self.mutation_tasks.close_admission();
    }

    /// Close admission and wait for accepted HTTP mutations before stopping their dependencies.
    pub async fn shutdown_mutations(&self) {
        self.mutation_tasks.shutdown().await;
    }

    /// Admit a previously validated signed mutation into the shared request-lifetime owner.
    pub(crate) async fn run_tracked_mutation<F>(
        &self,
        route: String,
        request_id: Option<String>,
        future: F,
    ) -> Response
    where
        F: std::future::Future<Output = Response> + Send + 'static,
    {
        self.mutation_tasks
            .run_response(future, route, request_id)
            .await
    }

    /// Cancellation shared with blocking local cost scans during shutdown.
    pub fn cost_scan_cancellation_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.cost_scan_cancelled)
    }

    pub(crate) fn track_cost_refresh_worker(&self) -> CostRefreshWorkerGuard {
        self.cost_refresh_workers.track()
    }

    pub fn with_cost_scanner_build_script(mut self, path: PathBuf) -> Self {
        self.cost_scanner_build_script = Some(path);
        self
    }

    /// Shared post-commit signal for durable event writers and the scheduler.
    pub fn event_wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.event_wake)
    }

    /// Share the continuation scheduler wake with owners that persist durable
    /// work before the HTTP state has finished being assembled.
    pub fn with_continuation_wake_handle(mut self, wake: Arc<Notify>) -> Self {
        self.continuation_wake = wake;
        self
    }

    pub fn continuation_wake_handle(&self) -> Arc<Notify> {
        Arc::clone(&self.continuation_wake)
    }

    pub fn orchestration_driver_handle(
        &self,
    ) -> Option<Arc<studio_runtime_services::multi_server_orchestration::OrchestrationDriver>> {
        self.orchestration_driver.clone()
    }

    pub fn with_lifecycle_ports(
        mut self,
        lifecycle_ports: Arc<dyn studio_runtime_services::agent_lifecycle_runtime::LifecyclePorts>,
    ) -> Self {
        if let Some(store) = self.store.as_ref() {
            let accounts = self.account_policy.as_ref().map(|policy| policy.accounts());
            let default_account = accounts
                .as_ref()
                .and_then(|snapshot| snapshot.default_account_key.as_str())
                .unwrap_or("default")
                .to_owned();
            let known_accounts = accounts
                .as_ref()
                .map(|snapshot| snapshot.accounts.clone())
                .unwrap_or_default();
            let driver = Arc::new(
                studio_runtime_services::multi_server_transport::OrchestrationDriver::new(
                    Arc::clone(store),
                    Arc::new(
                        studio_runtime_services::multi_server_transport::HttpsPairedTransport::new(
                            Arc::clone(store),
                        ),
                    ),
                    Arc::new(
                        studio_runtime_services::multi_server_orchestration::RuntimeOrchestrationEffects::with_project_accounts_and_lifecycle_ports(
                            Arc::clone(store),
                            Arc::new(studio_runtime_services::browser::BrowserCallbackReceiver::new(Arc::clone(store))),
                            default_account,
                            known_accounts,
                            lifecycle_ports,
                        ),
                    ),
                ),
            );
            self.browser_forwarder =
                Some(crate::routes::browser::new_forwarder(Arc::clone(&driver)));
            self.orchestration_driver = Some(driver);
        }
        self
    }

    pub fn model_catalog_handle(
        &self,
    ) -> studio_runtime_services::model_catalog::ModelCatalogService {
        self.model_catalog.clone()
    }

    pub fn resource_notifications_handle(
        &self,
    ) -> Arc<studio_runtime_services::resource_notify::ResourceNotifications> {
        Arc::clone(&self.resource_notifications)
    }

    pub fn sync_changes_handle(&self) -> Arc<ChangeNotificationHub> {
        Arc::clone(&self.sync_changes)
    }

    #[cfg(unix)]
    pub fn with_terminal_driver(mut self, driver: Arc<TerminalDriver<PtyTerminalSession>>) -> Self {
        self.terminal_driver = Some(driver);
        self
    }

    #[cfg(unix)]
    pub fn with_terminal_write_gate(
        mut self,
        gate: Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, bool> + Send + Sync>,
    ) -> Self {
        self.terminal_write_gate = Some(gate);
        self
    }

    /// Shared publisher used by the provider account worker to publish token rates.
    pub fn token_rate_handle(&self) -> Arc<TokenRateHub> {
        Arc::clone(&self.token_rates)
    }

    #[cfg(unix)]
    pub(crate) fn terminal_driver_handle(&self) -> Option<Arc<TerminalDriver<PtyTerminalSession>>> {
        self.terminal_driver.clone()
    }

    #[cfg(unix)]
    pub(crate) fn terminal_write_gate_handle(
        &self,
    ) -> Option<Arc<dyn Fn() -> futures_util::future::BoxFuture<'static, bool> + Send + Sync>> {
        self.terminal_write_gate.clone()
    }
}

pub fn router(mut state: HttpState) -> Result<Router, Box<dyn std::error::Error>> {
    let authority = operations().map_err(|error| {
        std::io::Error::other(format!("cannot load committed OpenAPI operations: {error}"))
    })?;
    let (router, registered) = compose::compose(routes::groups(), authority).map_err(|error| {
        std::io::Error::other(format!("invalid HTTP route registration: {error}"))
    })?;
    state.validators =
        Registry::compile(&registered.iter().map(|item| item.key()).collect::<Vec<_>>())?;
    state.etag_operations = registered
        .iter()
        .filter_map(|operation| {
            let key = operation.key();
            crate::schema::operation_has_etag(&key.0, &key.1).then_some(key)
        })
        .collect();

    let router = router.merge(routes::voice::fallback());
    Ok(with_boundary(router, state))
}

#[cfg(test)]
pub(crate) fn validation_probe_router(
    mut state: HttpState,
) -> Result<Router, Box<dyn std::error::Error>> {
    let authority = operations().map_err(|error| {
        std::io::Error::other(format!("cannot load committed OpenAPI operations: {error}"))
    })?;
    let document = crate::schema::openapi_document();
    let mut registrations = Vec::with_capacity(authority.len());
    for (method, path) in authority {
        let operation = document
            .pointer(&format!(
                "/paths/{}/{}",
                path.replace('~', "~0").replace('/', "~1"),
                method.to_lowercase()
            ))
            .ok_or_else(|| {
                std::io::Error::other(format!("missing probe operation {method} {path}"))
            })?;
        let body_schema = operation.pointer("/requestBody/content/application~1json/schema");
        let required = operation
            .pointer("/requestBody/required")
            .and_then(serde_json::Value::as_bool)
            == Some(true);
        let handler = match (body_schema.is_some(), required) {
            (true, true) => probe_required_body as ProbeHandler,
            (true, false) => probe_optional_body as ProbeHandler,
            (false, _) => probe_without_body as ProbeHandler,
        };
        let path: &'static str = Box::leak(path.clone().into_boxed_str());
        let status = crate::routes::manifest::OperationStatus::Implemented;
        let registration = match method.as_str() {
            "GET" => crate::routes::manifest::get(path, status, handler),
            "POST" => crate::routes::manifest::post(path, status, handler),
            "PUT" => crate::routes::manifest::put(path, status, handler),
            "DELETE" => crate::routes::manifest::delete(path, status, handler),
            "PATCH" => crate::routes::manifest::patch(path, status, handler),
            "OPTIONS" => crate::routes::manifest::options(path, status, handler),
            "HEAD" => crate::routes::manifest::head(path, status, handler),
            "TRACE" => crate::routes::manifest::trace(path, status, handler),
            _ => {
                return Err(
                    std::io::Error::other(format!("unsupported probe method {method}")).into(),
                );
            }
        };
        registrations.push(registration);
    }
    let probe_group = crate::routes::manifest::RouteGroup::new(registrations);
    let (router, registered) = compose::compose(vec![probe_group], authority).map_err(|error| {
        std::io::Error::other(format!("invalid HTTP route registration: {error}"))
    })?;
    state.validators =
        Registry::compile(&registered.iter().map(|item| item.key()).collect::<Vec<_>>())?;
    state.etag_operations = registered
        .iter()
        .filter_map(|operation| {
            let key = operation.key();
            crate::schema::operation_has_etag(&key.0, &key.1).then_some(key)
        })
        .collect();
    Ok(with_boundary(router, state))
}

#[cfg(test)]
type ProbeHandler = fn(
    axum::extract::State<HttpState>,
    Request<Body>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>>;

#[cfg(test)]
fn probe_required_body(
    state: axum::extract::State<HttpState>,
    request: Request<Body>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move { probe_echo(state.0, request, 1).await })
}

#[cfg(test)]
fn probe_optional_body(
    state: axum::extract::State<HttpState>,
    request: Request<Body>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move { probe_echo(state.0, request, 2).await })
}

#[cfg(test)]
fn probe_without_body(
    state: axum::extract::State<HttpState>,
    request: Request<Body>,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Response> + Send>> {
    Box::pin(async move { probe_echo(state.0, request, 0).await })
}

#[cfg(test)]
async fn probe_echo(state: HttpState, request: Request<Body>, body_kind: u8) -> Response {
    use axum::extract::{FromRequest, FromRequestParts};
    use axum::response::IntoResponse;
    use serde_json::{Map, Value};

    let (mut parts, body) = request.into_parts();
    let query = match crate::routes::extractors::QueryParams::from_request_parts(&mut parts, &state)
        .await
    {
        Ok(query) => query,
        Err(error) => return error.into_response(),
    };
    let headers =
        match crate::routes::extractors::ApiHeaders::from_request_parts(&mut parts, &state).await {
            Ok(headers) => headers,
            Err(error) => return error.into_response(),
        };
    let request = Request::from_parts(parts, body);
    let body_value: Option<Value> = match body_kind {
        1 => match crate::routes::extractors::ApiJson::<Value>::from_request(request, &state).await
        {
            Ok(value) => Some(value.0),
            Err(error) => return error.into_response(),
        },
        2 => {
            match crate::routes::extractors::ApiJsonOptional::<Value>::from_request(request, &state)
                .await
            {
                Ok(value) => value.0,
                Err(error) => return error.into_response(),
            }
        }
        _ => None,
    };
    let mut value = Map::new();
    if let Some(body) = body_value {
        value.insert("body".to_owned(), body);
    }
    value.insert(
        "query".to_owned(),
        Value::Object((*query.into_typed()).clone()),
    );
    value.insert("headers".to_owned(), Value::Object((*headers.0).clone()));
    axum::Json(Value::Object(value)).into_response()
}

fn with_boundary(router: Router<HttpState>, state: HttpState) -> Router {
    router
        .method_not_allowed_fallback(any(dispatch))
        .fallback(any(dispatch))
        .layer(axum_middleware::from_fn(
            middleware::diagnostics_http_timing,
        ))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            crate::mutation_sync::envelope,
        ))
        .layer(axum_middleware::from_fn_with_state(
            Arc::clone(&state.mutation_tasks),
            crate::mutation_tasks::own_mutations,
        ))
        // Boundary schema validation runs first, so malformed object requests are not sampled.
        // Python samples before route validation, but the difference is unobservable: both
        // responses are errors without envelopes, and Rust envelope reads never reject them.
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::boundary,
        ))
        .layer(axum_middleware::from_fn_with_state(
            state.clone(),
            middleware::response_safety,
        ))
        .with_state(state)
}

async fn dispatch(request: Request<Body>) -> Response {
    let method = request.method().as_str().to_owned();
    let path = request.uri().path().to_owned();
    let documented = operations().is_ok_and(|operations| {
        matching_template(
            operations
                .iter()
                .map(|(operation_method, template)| (operation_method.as_str(), template.as_str())),
            Some(&method),
            &path,
        )
        .is_some()
    });
    let hidden_voice = method == "POST"
        && path
            .strip_prefix("/api/voice/")
            .is_some_and(|action| !action.is_empty());
    if documented || hidden_voice {
        return error_response(StatusCode::NOT_IMPLEMENTED, "Not implemented");
    }
    let documented_path = operations().is_ok_and(|operations| {
        matching_template(
            operations
                .iter()
                .map(|(operation_method, template)| (operation_method.as_str(), template.as_str())),
            None,
            &path,
        )
        .is_some()
    });
    if documented_path {
        return crate::error::method_not_allowed_response(&path);
    }
    error_response(StatusCode::NOT_FOUND, "Not found")
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::Arc;

    use axum::body::Body;
    use axum::body::to_bytes;
    use axum::extract::ConnectInfo;
    use axum::http::{Request, StatusCode};
    use axum::routing::post;
    use axum::{Json, Router};
    use serde_json::Value;
    use tower::ServiceExt;

    use crate::middleware;
    use studio_runtime_store::{RuntimeStore, SyncDocumentsRepository};

    use super::{HttpState, router};

    fn probe_app() -> (Router, tempfile::TempDir) {
        let (state, directory) = state();
        (
            super::validation_probe_router(state).expect("probe router starts"),
            directory,
        )
    }

    async fn value_handler(
        crate::routes::extractors::ApiJson(value): crate::routes::extractors::ApiJson<Value>,
    ) -> Json<Value> {
        Json(value)
    }

    fn state() -> (HttpState, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("isolated state directory");
        (
            HttpState::new(
                "token".to_owned(),
                "workspace".to_owned(),
                None,
                directory.path().to_string_lossy().into_owned(),
                4620,
                None,
                None,
                None,
            ),
            directory,
        )
    }

    #[tokio::test]
    async fn startup_compiles_registered_validators_and_matches_python_methods() {
        let (state, _directory) = state();
        let app = router(state).expect("router and registered OpenAPI validators start");
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let request = Request::builder()
            .method("GET")
            .uri("/api/session?extra=1")
            .header("host", "127.0.0.1:4620")
            .extension(peer)
            .body(Body::empty())
            .expect("request builds");
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);

        let request = Request::builder()
            .method("HEAD")
            .uri("/api/session")
            .header("host", "127.0.0.1:4620")
            .extension(peer)
            .body(Body::empty())
            .expect("request builds");
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::METHOD_NOT_ALLOWED);
    }

    #[tokio::test]
    async fn encoded_dynamic_voice_fallback_keeps_raw_route_and_request_identity() {
        let (state, _directory) = state();
        let app = router(state).expect("router starts");
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let request = Request::builder()
            .method("POST")
            .uri("/api/voice/unknown%2Faction")
            .header("host", "127.0.0.1:4620")
            .header("x-canvas-token", "token")
            .header("content-type", "application/json")
            .header("x-studio-request-id", "encoded-route-request")
            .extension(peer)
            .body(Body::from("{}"))
            .expect("request builds");

        let response = app.oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::NOT_FOUND);
        assert_eq!(
            response.headers()["x-studio-request-id"],
            "encoded-route-request"
        );
    }

    #[tokio::test]
    async fn sync_preferences_write_is_registered_in_the_production_router() {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store =
            Arc::new(RuntimeStore::open(Some(directory.path())).expect("runtime store opens"));
        let state = HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            None,
            directory.path().to_string_lossy().into_owned(),
            4620,
            Some("https://studio-test.ts.net".to_owned()),
            Some(Arc::clone(&store)),
            None,
        );
        let app = router(state).expect("production router starts");
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let forwarded_request =
            |request_id: &str, origin: &str, token: Option<&str>, body: &str| {
                let builder = Request::builder()
                    .method("POST")
                    .uri("/api/sync/preferences")
                    .header("host", "127.0.0.1:4620")
                    .header("origin", origin)
                    .header("x-forwarded-for", "100.64.0.1")
                    .header("x-forwarded-host", "studio-test.ts.net")
                    .header("x-forwarded-proto", "https")
                    .header("content-type", "application/json")
                    .header("x-studio-request-id", request_id)
                    .header(
                        crate::schema::API_SCHEMA_HASH_HEADER,
                        crate::schema::api_schema_hash(),
                    );
                let builder = match token {
                    Some(token) => builder.header("x-canvas-token", token),
                    None => builder,
                };
                builder
                    .extension(peer)
                    .body(Body::from(body.to_owned()))
                    .expect("forwarded preference request builds")
            };
        let request = forwarded_request(
            "request-1",
            "https://studio-test.ts.net",
            Some("token"),
            r#"{"scope":"user","fields":{"[\"codex-studio-preferences-v1\",\"theme\"]":{"value":"dark","timestamp":1,"writer":"router-test"}}}"#,
        );
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        let response_body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("response body reads"),
        )
        .expect("response body is JSON");
        let theme_key = r#"["codex-studio-preferences-v1","theme"]"#;
        assert_eq!(response_body["fields"][theme_key]["value"], "dark");

        let reader = store.read().expect("stored preferences can be read");
        let stored = SyncDocumentsRepository::preference_payload(&reader, "user")
            .expect("preference payload reads")
            .expect("preference payload was persisted");
        let stored: Value = serde_json::from_str(&stored).expect("stored payload is JSON");
        assert_eq!(stored["value"]["fields"][theme_key]["value"], "dark");
        drop(reader);

        let generation_before_invalid = store.committed_change_generation();
        let invalid_request = forwarded_request(
            "request-2",
            "https://studio-test.ts.net",
            Some("token"),
            r#"{"scope":"user","fields":{"[\"unknown\"]":{"value":true,"timestamp":2,"writer":"router-test"}}}"#,
        );
        let response = app
            .clone()
            .oneshot(invalid_request)
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        assert_eq!(
            store.committed_change_generation(),
            generation_before_invalid
        );

        let request_without_token = forwarded_request(
            "request-3",
            "https://studio-test.ts.net",
            None,
            r#"{"scope":"user","fields":{}}"#,
        );
        let response = app
            .clone()
            .oneshot(request_without_token)
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);

        let request_with_invalid_origin = forwarded_request(
            "request-4",
            "https://attacker.ts.net",
            Some("token"),
            r#"{"scope":"user","fields":{}}"#,
        );
        let response = app
            .clone()
            .oneshot(request_with_invalid_origin)
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
    }

    #[tokio::test]
    async fn boundary_early_returns_include_api_headers() {
        let (state, _directory) = state();
        let app = router(state).expect("router starts");
        let request = Request::builder()
            .method("GET")
            .uri("/api/session")
            .header("host", "example.invalid")
            .body(Body::empty())
            .expect("request builds");
        let response = app.clone().oneshot(request).await.expect("response");

        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers()[crate::schema::API_SCHEMA_HASH_HEADER],
            crate::schema::api_schema_hash()
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(
            response.headers()["content-security-policy"],
            crate::error::CSP
        );

        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let preflight = Request::builder()
            .method("OPTIONS")
            .uri("/api/accounts/login")
            .header("host", "studio-a.localhost:4620")
            .header("origin", "http://studio-a.localhost:4620")
            .header("access-control-request-method", "POST")
            .header("access-control-request-headers", "content-type")
            .extension(peer)
            .body(Body::empty())
            .expect("preflight request builds");
        let response = app.oneshot(preflight).await.expect("preflight response");
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            response.headers()[crate::schema::API_SCHEMA_HASH_HEADER],
            crate::schema::api_schema_hash()
        );
        assert_eq!(response.headers()["cache-control"], "no-store");
        assert_eq!(response.headers()["x-content-type-options"], "nosniff");
        assert_eq!(response.headers()["referrer-policy"], "no-referrer");
        assert_eq!(
            response.headers()["content-security-policy"],
            crate::error::CSP
        );
    }

    #[tokio::test]
    async fn huge_integer_body_and_query_return_located_client_errors() {
        let (app, _directory) = probe_app();
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let huge = "9".repeat(401);
        let body = format!("{{\"prompt\":\"x\",\"concurrency\":{huge}}}");
        let request = Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("host", "127.0.0.1:4620")
            .header("content-type", "application/json")
            .header("x-canvas-token", "token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(peer)
            .body(Body::from(body))
            .expect("request builds");
        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("error is JSON");
        assert_eq!(body["error"], "Invalid request");
        assert_eq!(body["location"], serde_json::json!(["body"]));

        let request = Request::builder()
            .method("GET")
            .uri(format!("/api/agent-chat?before={huge}"))
            .header("host", "127.0.0.1:4620")
            .extension(peer)
            .body(Body::empty())
            .expect("request builds");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("error is JSON");
        assert_eq!(body["error"], "Invalid request");
        assert_eq!(body["location"], serde_json::json!(["query", "before"]));
    }

    #[tokio::test]
    async fn deeply_nested_registered_json_body_is_rejected_before_value_extraction() {
        let authority = crate::schema::operations().expect("schema parses");
        let (mut app_state, _directory) = state();
        app_state.validators =
            crate::validation::Registry::compile(&authority.iter().cloned().collect::<Vec<_>>())
                .expect("registered schema validators compile");
        let app = Router::new()
            .route("/api/federation", post(value_handler))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                middleware::response_safety,
            ))
            .layer(axum::middleware::from_fn_with_state(
                app_state.clone(),
                middleware::boundary,
            ))
            .with_state(app_state);
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let nested = format!("{}0{}", "[".repeat(10_000), "]".repeat(10_000));
        let request = Request::builder()
            .method("POST")
            .uri("/api/federation")
            .header("host", "127.0.0.1:4620")
            .header("content-type", "application/json")
            .header("x-canvas-token", "token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(peer)
            .body(Body::from(nested))
            .expect("request builds");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("validation envelope is JSON");
        assert_eq!(body["error"], "Invalid request");
        assert_eq!(body["location"], serde_json::json!(["body"]));
    }

    #[tokio::test]
    async fn boundary_refusal_precedes_closed_mutation_admission_and_keeps_request_id() {
        let (state, _directory) = state();
        state.close_mutation_admission();
        let app = super::validation_probe_router(state).expect("probe router starts");
        let request = Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("host", "127.0.0.1:4620")
            .header("x-studio-request-id", "request-preserved")
            .body(Body::empty())
            .expect("request builds");

        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers()["x-studio-request-id"],
            "request-preserved"
        );

        let (state, _directory) = state();
        state.close_mutation_admission();
        let app = super::validation_probe_router(state).expect("probe router starts");
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let accepted_request = Request::builder()
            .method("POST")
            .uri("/api/accounts/discover")
            .header("host", "127.0.0.1:4620")
            .header("x-canvas-token", "token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .header("x-studio-request-id", "request-closed")
            .extension(peer)
            .body(Body::empty())
            .expect("request builds");
        let response = app.oneshot(accepted_request).await.expect("response");
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(response.headers()["x-studio-request-id"], "request-closed");
    }

    #[tokio::test]
    async fn optional_required_and_chunked_json_bodies_follow_openapi_contract() {
        let (app, _directory) = probe_app();
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let build = |method: &str, uri: &str, body: Body, content_type: Option<&str>| {
            let mut request = Request::builder()
                .method(method)
                .uri(uri)
                .header("host", "127.0.0.1:4620")
                .header("x-canvas-token", "token")
                .header(
                    crate::schema::API_SCHEMA_HASH_HEADER,
                    crate::schema::api_schema_hash(),
                )
                .extension(peer);
            if let Some(content_type) = content_type {
                request = request.header("content-type", content_type);
            }
            request.body(body).expect("request builds")
        };

        let optional = app
            .clone()
            .oneshot(build("POST", "/api/accounts/discover", Body::empty(), None))
            .await
            .expect("optional body response");
        assert_eq!(optional.status(), StatusCode::OK);

        let unknown_length_empty = Body::from_stream(futures_util::stream::empty::<
            Result<axum::body::Bytes, std::convert::Infallible>,
        >());
        let empty_chunked = app
            .clone()
            .oneshot(build(
                "POST",
                "/api/accounts/discover",
                unknown_length_empty,
                None,
            ))
            .await
            .expect("empty chunked optional body response");
        assert_eq!(empty_chunked.status(), StatusCode::OK);

        let required = app
            .clone()
            .oneshot(build("POST", "/api/agents", Body::empty(), None))
            .await
            .expect("required body response");
        assert_eq!(required.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(required.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("validation error JSON");
        assert_eq!(body["location"], serde_json::json!(["body"]));

        // A Body with no Content-Length models a chunked request in the in-process harness.
        let chunked = app
            .oneshot(build(
                "POST",
                "/api/agents",
                Body::from(r#"{"prompt":"x"}"#),
                Some("application/json"),
            ))
            .await
            .expect("chunked body response");
        assert_eq!(chunked.status(), StatusCode::OK);
    }

    #[tokio::test]
    async fn encoded_repeated_scalar_query_parameter_is_refused_by_router() {
        let (app, _directory) = probe_app();
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        let request = Request::builder()
            .method("GET")
            .uri("/api/agent-chat?%62efore=1&before=1")
            .header("host", "127.0.0.1:4620")
            .extension(peer)
            .body(Body::empty())
            .expect("request builds");
        let response = app.oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
        let body: Value = serde_json::from_slice(
            &to_bytes(response.into_body(), 1024)
                .await
                .expect("body reads"),
        )
        .expect("error envelope parses");
        assert_eq!(body["location"], serde_json::json!(["query", "before"]));
    }

    #[tokio::test]
    async fn boundary_accepts_each_python_body_limit_and_rejects_the_next_byte() {
        let cases = [
            ("/api/agents", r#"{"prompt":"x"}"#, 256 * 1024),
            (
                "/api/assets",
                r#"{"agent":"x","name":"x"}"#,
                28 * 1024 * 1024,
            ),
            (
                "/api/projects",
                r#"{"action":"add_location","project":"x","request_id":"00000000-0000-4000-8000-000000000001","server":"x"}"#,
                6 * 1024 * 1024,
            ),
            (
                "/api/voice/audio",
                r#"{"agent":"x","audio":"x","chunk_id":"x","mime":"x","session_id":"x"}"#,
                6 * 1024 * 1024,
            ),
            (
                "/api/federation",
                r#"{"action":"accept_peer","invitation":{"expires":1,"inviteId":"x","label":"x","origin":"x","protocol":1,"publicKey":"x","stateId":"x","token":"x"}}"#,
                256 * 1024,
            ),
        ];
        let peer = ConnectInfo(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 12345));
        for (path, valid_json, limit) in cases {
            let (state, _directory) = state();
            let app = router(state).expect("router starts");
            // At the exact limit, assert the size layer accepts the body and let
            // the route return its fixture-dependent status. One byte above the
            // limit must be rejected by the size layer with 413.
            for (length, over_limit) in [(limit, false), (limit + 1, true)] {
                let opaque_scans_before = crate::validation::opaque_body_scan_count();
                let mut body = Vec::with_capacity(length);
                body.extend_from_slice(valid_json.as_bytes());
                body.resize(length, b' ');
                let request = Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("host", "127.0.0.1:4620")
                    .header("content-type", "application/json")
                    .header("content-length", length.to_string())
                    .header("x-canvas-token", "token")
                    .header(
                        crate::schema::API_SCHEMA_HASH_HEADER,
                        crate::schema::api_schema_hash(),
                    )
                    .extension(peer)
                    .body(Body::from(body))
                    .expect("request builds");
                let response = app.clone().oneshot(request).await.expect("response");
                if over_limit {
                    assert_eq!(
                        response.status(),
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "{path} with {length} bytes"
                    );
                } else {
                    assert_ne!(
                        response.status(),
                        StatusCode::PAYLOAD_TOO_LARGE,
                        "size layer rejected {path} at its {limit}-byte limit"
                    );
                }
                if path == "/api/assets" && length == limit {
                    assert_eq!(
                        crate::validation::opaque_body_scan_count(),
                        opaque_scans_before,
                        "asset upload schema has no opaque JSON positions and must not run the raw-token scan"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod cost_refresh_worker_tests {
    use super::HttpState;
    use std::sync::{Arc, atomic::Ordering};

    #[tokio::test]
    async fn shutdown_waits_until_cost_refresh_worker_finishes_cleanup() {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let state = HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            None,
            directory.path().to_string_lossy().into_owned(),
            4620,
            None,
            None,
            None,
        );
        let cancelled = Arc::clone(&state.cost_scan_cancelled);
        let guard = state.track_cost_refresh_worker();
        let shutdown = tokio::spawn(async move { state.shutdown_services().await });
        tokio::task::yield_now().await;
        assert!(cancelled.load(Ordering::Acquire));
        assert!(
            !shutdown.is_finished(),
            "shutdown waits for the refresh worker"
        );
        drop(guard);
        tokio::time::timeout(std::time::Duration::from_secs(1), shutdown)
            .await
            .expect("shutdown waiter completes")
            .expect("shutdown task succeeds")
            .expect("shutdown services succeed");
    }
}
