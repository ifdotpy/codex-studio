use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use axum::middleware as axum_middleware;
use axum::response::Response;
use axum::routing::any;
use serde_json::Value;
use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use studio_runtime_process::operational_log::OperationalLog;
use studio_runtime_services::change_notifications::ChangeNotificationHub;
use studio_runtime_services::token_rate::TokenRateHub;
use studio_runtime_store::RuntimeStore;
use tokio::sync::Notify;
use tower_http::compression::{
    CompressionLayer, CompressionLevel,
    predicate::{Predicate, SizeAbove},
};

const COST_REFRESH_SHUTDOWN_WAIT: std::time::Duration = std::time::Duration::from_secs(9);
/// Minimum final JSON representation size, in bytes, worth compressing over HTTP.
const MIN_JSON_COMPRESSION_BYTES: u16 = 1024;
/// Brotli quality level (0–11), balancing JSON size savings and server CPU work.
const HTTP_BROTLI_QUALITY: i32 = 4;

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

pub(crate) const BROWSER_HOST_FOLDER_NAME: &str =
    studio_runtime_services::browser_environment::BROWSER_HOST_FOLDER;
pub(crate) const BROWSER_HOST_CONTROL_SOCKET_NAME: &str =
    studio_runtime_services::browser_host::CONTROL_SOCKET;

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
    pub(crate) state_dir: String,
    pub(crate) cost_scan_cancelled: Arc<std::sync::atomic::AtomicBool>,
    cost_refresh_workers: Arc<CostRefreshWorkers>,
    pub(crate) vm_state_dir: std::path::PathBuf,
    pub(crate) app_server_log: Option<OperationalLog>,
    pub(crate) port: u16,
    pub(crate) public_origin: Option<String>,
    /// The live Serve configuration. `None` keeps `public_origin` fixed (tests).
    remote_access: Option<Arc<studio_runtime_services::remote_access::RemoteAccess>>,
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
    pub(crate) sync_sessions: Arc<routes::SyncSessions>,
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
        self.model_catalog =
            studio_runtime_services::model_catalog::ModelCatalogService::with_change_callback(
                crate::model_entities::change_callback(self.store.clone()),
            );
        self.app_server_log = log;
        self
    }

    /// Start the source-owned summaries once, after ports are wired and before serving.
    pub async fn initialize_bounded_entities(
        &self,
    ) -> Result<(), studio_runtime_store::StoreError> {
        crate::bounded_entities::start(self).await
    }

    #[allow(clippy::too_many_arguments)]
    pub fn new(
        token: String,
        workspace_id: String,
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
        let model_catalog =
            studio_runtime_services::model_catalog::ModelCatalogService::with_change_callback(
                crate::model_entities::change_callback(store.clone()),
            );
        let resource_notifications = Arc::new(
            studio_runtime_services::resource_notify::ResourceNotifications::new(Arc::clone(
                &sync_changes,
            )),
        );
        if let Some(store) = &store {
            let notifications = Arc::clone(&resource_notifications);
            store.add_resource_change_observer(move |change| {
                notifications.notify_store_commit(change)
            });
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
            state_dir,
            cost_scan_cancelled: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            cost_refresh_workers: Arc::new(CostRefreshWorkers::default()),
            vm_state_dir,
            app_server_log: None,
            port,
            public_origin,
            remote_access: None,
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
            resource_notifications,
            sync_sessions: Arc::new(Default::default()),
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

    /// Recheck the Serve configuration that produced `public_origin` for each
    /// remote request, so disabling remote access needs no restart.
    pub fn with_remote_access(
        mut self,
        remote_access: Arc<studio_runtime_services::remote_access::RemoteAccess>,
    ) -> Self {
        self.remote_access = Some(remote_access);
        self
    }

    /// The Serve origin that a remote request may use now.
    pub(crate) fn serve_origin(&self) -> Option<String> {
        match &self.remote_access {
            Some(access) => access.still_allows(self.public_origin.as_deref()),
            None => self.public_origin.clone(),
        }
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

    /// Attach the optional host tool used for vault setup.
    pub fn with_vault_host_command(mut self) -> Self {
        self.vault_host_command = Some(studio_runtime_services::vault::VaultHostCommand::new());
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
        self.credentials.shutdown().await;
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
        self.credentials.close_admission();
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

    /// The state directory's provider stderr log, once opened; one writer owns
    /// its rotation, so every provider child shares this handle.
    pub fn app_server_log_handle(&self) -> Option<OperationalLog> {
        self.app_server_log.clone()
    }

    /// Cancellation shared with blocking local cost scans during shutdown.
    pub fn cost_scan_cancellation_handle(&self) -> Arc<std::sync::atomic::AtomicBool> {
        Arc::clone(&self.cost_scan_cancelled)
    }

    pub(crate) fn track_cost_refresh_worker(&self) -> CostRefreshWorkerGuard {
        self.cost_refresh_workers.track()
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

    /// The browser bridges whose sign-in links become request cards
    /// (docs/vm-browser-bridge.md): native chats on every server, and VM chats on a Mac,
    /// as the Python server's `codex_browser_bridge.tick`.
    pub fn browser_card_sources(&self) -> Vec<studio_runtime_services::browser_cards::CardSource> {
        let mut sources = Vec::new();
        if let Some(guest) = &self.browser_answer_host_guest {
            sources.push(studio_runtime_services::browser_cards::CardSource {
                source: "host",
                guest: Arc::clone(guest),
                forwarder: self.browser_host_local_forwarder.clone(),
                http_providers: Default::default(),
            });
        }
        if cfg!(target_os = "macos")
            && let Some(guest) = &self.browser_answer_guest
        {
            sources.push(studio_runtime_services::browser_cards::CardSource {
                source: "vm",
                guest: Arc::clone(guest),
                forwarder: self.browser_local_forwarder.clone(),
                http_providers: Default::default(),
            });
        }
        sources
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
    requested: &[(&'static str, &'static str)],
) -> Result<Router, Box<dyn std::error::Error>> {
    let authority = operations().map_err(|error| {
        std::io::Error::other(format!("cannot load committed OpenAPI operations: {error}"))
    })?;
    let document = crate::schema::openapi_document();
    let mut registrations = Vec::with_capacity(requested.len());
    for &(method, path) in requested {
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
        let status = crate::routes::manifest::OperationStatus::Implemented;
        let registration = match method {
            "GET" => crate::routes::manifest::get(path, status, handler),
            "POST" => crate::routes::manifest::post(path, status, handler),
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
        // Encode only after ETag hashes and 304 handling are final.
        .layer(
            CompressionLayer::new()
                .no_gzip()
                .no_deflate()
                .no_zstd()
                .quality(CompressionLevel::Precise(HTTP_BROTLI_QUALITY))
                .compress_when(SizeAbove::new(u64::from(MIN_JSON_COMPRESSION_BYTES)).and(
                    |_, _, headers: &axum::http::HeaderMap, _: &axum::http::Extensions| {
                        headers
                            .get(axum::http::header::CONTENT_TYPE)
                            .and_then(|value| value.to_str().ok())
                            .is_some_and(|value| {
                                value.split(';').next().is_some_and(|media_type| {
                                    media_type.trim().eq_ignore_ascii_case("application/json")
                                })
                            })
                    },
                )),
        )
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

    use studio_runtime_store::{RuntimeStore, SyncDocumentsRepository};

    use super::{HttpState, router};

    fn probe_app(requested: &[(&'static str, &'static str)]) -> (Router, tempfile::TempDir) {
        let (state, directory) = state();
        (
            super::validation_probe_router(state, requested).expect("probe router starts"),
            directory,
        )
    }

    fn state() -> (HttpState, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("isolated state directory");
        (
            HttpState::new(
                "token".to_owned(),
                "workspace".to_owned(),
                directory.path().to_string_lossy().into_owned(),
                4620,
                None,
                None,
                None,
            ),
            directory,
        )
    }

    async fn state_with_store() -> (HttpState, Arc<RuntimeStore>, tempfile::TempDir) {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
        let state = HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            directory.path().to_string_lossy().into_owned(),
            4620,
            None,
            Some(Arc::clone(&store)),
            None,
        );
        (state, store, directory)
    }

    async fn compression_response(
        app: &Router,
        path: &str,
        encoding: &str,
        etag: Option<&str>,
    ) -> axum::response::Response {
        let mut request = Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:4620")
            .header("accept-encoding", encoding)
            .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))));
        if let Some(etag) = etag {
            request = request.header("if-none-match", etag);
        }
        app.clone()
            .oneshot(request.body(Body::empty()).expect("request"))
            .await
            .expect("response")
    }

    #[tokio::test]
    async fn brotli_json_preserves_bytes_etags_and_excludes_small_json_and_sse() {
        use axum::{http::header, routing::get};
        use std::io::Read;
        let (mut state, _directory) = state();
        state
            .etag_operations
            .insert(("GET".into(), "/api/rules".into()));
        let payload = serde_json::json!({"rules": ["rule content ".repeat(512)]});
        let expected = serde_json::to_vec(&payload).expect("JSON");
        let sse = format!("data: {}\n\n", "event".repeat(1024));
        let app = super::with_boundary(
            Router::new()
                .route("/api/rules", get(move || async move { Json(payload) }))
                .route(
                    "/api/session",
                    get(|| async { Json(serde_json::json!({"ok": true})) }),
                )
                .route(
                    "/api/sync/stream",
                    get({
                        let sse = sse.clone();
                        move || async move { ([(header::CONTENT_TYPE, "text/event-stream")], sse) }
                    }),
                )
                .route(
                    "/other-events",
                    get({
                        let sse = sse.clone();
                        move || async move { ([(header::CONTENT_TYPE, "text/event-stream")], sse) }
                    }),
                ),
            state,
        );
        let identity = compression_response(&app, "/api/rules", "identity", None).await;
        assert_eq!(identity.status(), StatusCode::OK);
        let etag = identity.headers()[header::ETAG]
            .to_str()
            .expect("ETag")
            .to_owned();
        assert_eq!(
            etag,
            format!("\"{}\"", studio_runtime_store::sha256_hex(&expected))
        );
        assert!(!identity.headers().contains_key(header::CONTENT_ENCODING));
        assert_eq!(
            to_bytes(identity.into_body(), usize::MAX)
                .await
                .expect("body"),
            expected
        );
        let compressed = compression_response(&app, "/api/rules", "br", None).await;
        assert_eq!(
            compressed
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("br")
        );
        assert_eq!(compressed.headers()[header::ETAG], etag);
        assert!(compressed.headers().get_all(header::VARY).iter().any(|v| {
            v.to_str()
                .expect("Vary")
                .split(',')
                .any(|v| v.trim().eq_ignore_ascii_case("accept-encoding"))
        }));
        let bytes = to_bytes(compressed.into_body(), usize::MAX)
            .await
            .expect("body");
        let mut decoded = Vec::new();
        brotli::Decompressor::new(bytes.as_ref(), 4096)
            .read_to_end(&mut decoded)
            .expect("Brotli");
        assert_eq!(decoded, expected);
        for encoding in ["br", "identity"] {
            let response = compression_response(&app, "/api/rules", encoding, Some(&etag)).await;
            assert_eq!(response.status(), StatusCode::NOT_MODIFIED);
            assert_eq!(response.headers()[header::ETAG], etag);
            assert!(!response.headers().contains_key(header::CONTENT_ENCODING));
            assert!(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("body")
                    .is_empty()
            );
        }
        for (path, expected) in [
            ("/api/session", b"{\"ok\":true}".as_slice()),
            ("/api/sync/stream", sse.as_bytes()),
            ("/other-events", sse.as_bytes()),
        ] {
            let response = compression_response(&app, path, "br", None).await;
            assert_eq!(response.status(), StatusCode::OK);
            assert!(
                !response.headers().contains_key(header::CONTENT_ENCODING),
                "{path}"
            );
            assert_eq!(
                to_bytes(response.into_body(), usize::MAX)
                    .await
                    .expect("body"),
                expected
            );
        }
    }

    #[tokio::test]
    async fn brotli_threshold_media_type_and_negotiation_are_exact() {
        use axum::{http::header, routing::get};
        let minimum = usize::from(super::MIN_JSON_COMPRESSION_BYTES);
        for size in [minimum - 1, minimum, minimum + 1] {
            for content_type in [
                "application/json",
                "application/json; charset=utf-8",
                "text/plain",
                "application/jsonp",
            ] {
                let (state, _directory) = state();
                let payload = serde_json::to_string(&"x".repeat(size - 2)).expect("JSON string");
                assert_eq!(payload.len(), size);
                let app =
                    super::with_boundary(
                        Router::new().route(
                            "/payload",
                            get(move || async move {
                                ([(header::CONTENT_TYPE, content_type)], payload)
                            }),
                        ),
                        state,
                    );
                for encoding in ["br", "identity", "gzip", "br;q=0"] {
                    let response = compression_response(&app, "/payload", encoding, None).await;
                    assert_eq!(
                        response.headers().contains_key(header::CONTENT_ENCODING),
                        size >= minimum
                            && content_type.starts_with("application/json")
                            && content_type != "application/jsonp"
                            && encoding == "br",
                        "size={size} type={content_type} encoding={encoding}"
                    );
                }
            }
        }
    }

    #[tokio::test]
    async fn brotli_compresses_mutation_results_without_entity_envelopes() {
        use axum::http::header;
        use std::io::Read;
        let mut bodies = Vec::new();
        for encoding in ["identity", "br"] {
            let (mut state, store, _directory) = state_with_store().await;
            state.validators =
                crate::validation::Registry::compile(&[("POST".into(), "/api/messages".into())])
                    .expect("validator");
            let app = super::with_boundary(Router::new().route("/api/messages", post(move || async move {
                store.transaction(|tx| {
                    SyncDocumentsRepository::put_entity(tx, "agent", "agent", serde_json::json!({"id": "agent", "name":"Agent", "status":"idle", "source":"managed", "tail": "entity content ".repeat(512)}), false)?;
                    Ok(())
                }).await.expect("entity mutation");
                Json(serde_json::json!({
                    "ok": true,
                    "result": "mutation result ".repeat(512),
                }))
            })), state);
            let request = Request::builder()
                .method("POST")
                .uri("/api/messages")
                .header("host", "127.0.0.1:4620")
                .header("x-canvas-token", "token")
                .header(header::CONTENT_TYPE, "application/json")
                .header(header::ACCEPT_ENCODING, encoding)
                .extension(ConnectInfo(SocketAddr::from(([127, 0, 0, 1], 12345))))
                .body(Body::from(
                    r#"{"id":"00000000-0000-4000-8000-000000000000","room":"fixture","text":"hello"}"#,
                ))
                .expect("request");
            let response = app.oneshot(request).await.expect("mutation response");
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response.headers().contains_key(header::CONTENT_ENCODING),
                encoding == "br"
            );
            let bytes = to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("body");
            let bytes = if encoding == "br" {
                let mut decoded = Vec::new();
                brotli::Decompressor::new(bytes.as_ref(), 4096)
                    .read_to_end(&mut decoded)
                    .expect("Brotli");
                decoded
            } else {
                bytes.to_vec()
            };
            let result: Value = serde_json::from_slice(&bytes).expect("mutation result");
            assert_eq!(result["ok"], true);
            assert!(
                result["result"]
                    .as_str()
                    .is_some_and(|value| !value.is_empty())
            );
            let result_object = result.as_object().expect("mutation result is an object");
            assert!(!result_object.contains_key("_syncEntities"));
            assert!(!result_object.contains_key("_syncEntitiesAfter"));
            bodies.push(bytes);
        }
        assert_eq!(bodies[0], bodies[1]);
    }

    #[tokio::test]
    async fn brotli_real_directory_responses_save_bytes() {
        use axum::http::header;
        use std::io::Read;
        let (state, directory) = state();
        let app = router(state).expect("production router");
        let root = directory.path().join("listing");
        tokio::fs::create_dir(&root).await.expect("listing root");
        for index in 0..32 {
            tokio::fs::create_dir(root.join(format!("project-{index:04}-fixture-workspace")))
                .await
                .expect("fixture folder");
        }
        let path = format!("/api/directories?path={}", root.display());
        let plain = compression_response(&app, &path, "identity", None).await;
        assert_eq!(plain.status(), StatusCode::OK);
        let expected = to_bytes(plain.into_body(), usize::MAX)
            .await
            .expect("JSON bytes");
        let response = compression_response(&app, &path, "br", None).await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response
                .headers()
                .get(header::CONTENT_ENCODING)
                .and_then(|v| v.to_str().ok()),
            Some("br")
        );
        let compressed = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("compressed bytes");
        let mut decoded = Vec::new();
        brotli::Decompressor::new(compressed.as_ref(), 4096)
            .read_to_end(&mut decoded)
            .expect("Brotli");
        assert_eq!(decoded, expected);
        assert!(compressed.len() < expected.len());
    }

    #[tokio::test]
    async fn entity_only_commits_preserve_volatile_resources_and_refresh_entity_refs() {
        let (state, store, _directory) = state_with_store().await;
        let volatile = [
            serde_json::json!({"kind":"accounts"}),
            serde_json::json!({"kind":"costs"}),
            serde_json::json!({"kind":"desktop"}),
            serde_json::json!({"kind":"queue","agentId":"agent"}),
            serde_json::json!({"kind":"session-cost","agentId":"agent"}),
            serde_json::json!({"kind":"terminals"}),
        ];
        let entities = vec![
            serde_json::json!({"kind":"state"}),
            serde_json::json!({"kind":"transcript","agentId":"agent"}),
            serde_json::json!({"kind":"transcripts"}),
            serde_json::json!({"kind":"room","roomId":"room"}),
            serde_json::json!({"kind":"tasks","agentId":"agent"}),
            serde_json::json!({"kind":"task","taskId":"task"}),
            serde_json::json!({"kind":"workspace","agentId":"agent"}),
        ];
        let resources = volatile
            .iter()
            .chain(&entities)
            .cloned()
            .collect::<Vec<_>>();
        store
            .transaction(|tx| {
                studio_runtime_store::SyncDocumentsRepository::put_entity(
                    tx,
                    "agent",
                    "agent",
                    serde_json::json!({"id":"agent"}),
                    false,
                )?;
                studio_runtime_store::SyncDocumentsRepository::put_entity(
                    tx,
                    "transcript:agent",
                    "item",
                    serde_json::json!({"text":"hello"}),
                    false,
                )?;
                Ok(())
            })
            .await
            .expect("entity-only commit");
        assert_eq!(state.sync_changes.snapshot().revision, 1);
        assert_eq!(
            state.sync_changes.snapshot().resource_revision,
            0,
            "entity-only commit must not invalidate every resource"
        );
        let view = state
            .resource_notifications
            .changes_since(&resources, Some(0))
            .expect("entity changes");
        assert_eq!(view.resources, entities);
        assert!(!view.entity_reset, "known entity writes remain incremental");
        store.transaction(|tx| {
            studio_runtime_store::SyncDocumentsRepository::put_entity(tx, "agent", "agent", serde_json::json!({"id":"agent","status":"idle"}), false)?;
            tx.execute("INSERT INTO runtime_items(id,agent,created,record) VALUES('mixed','agent',1,'{}')", [])?;
            Ok(())
        }).await.expect("mixed commit");
        assert_eq!(state.sync_changes.snapshot().resource_revision, 2);
        assert_eq!(
            state
                .resource_notifications
                .changes_since(&resources, Some(1))
                .expect("mixed changes")
                .resources,
            resources
        );
        store.transaction(|tx| {
            tx.execute_batch("CREATE TABLE unknown_resource_writer(id INTEGER); INSERT INTO unknown_resource_writer VALUES(1)")?;
            Ok(())
        }).await.expect("unknown writer");
        assert_eq!(state.sync_changes.snapshot().resource_revision, 3);
    }

    #[tokio::test]
    async fn internal_receipt_journal_commits_do_not_invalidate_browser_resources() {
        let (state, store, _directory) = state_with_store().await;

        store
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3)",
                    ("operation", "signature", "{}"),
                )?;
                transaction.execute(
                    "INSERT INTO runtime_native_action_receipts(id,signature,receipt,outcome) VALUES(?1,?2,?3,?4)",
                    ("native", "signature", "{}", "accepted"),
                )?;
                transaction.execute(
                    "INSERT INTO runtime_access_receipts(client,request_id,hash,session,status,record) VALUES(?1,?2,?3,?4,?5,?6)",
                    ("client", "request", "hash", "session", "complete", "{}"),
                )?;
                Ok(())
            })
            .await
            .expect("internal receipt journals commit");

        assert_eq!(state.sync_changes.snapshot().resource_revision, 0);
        assert_eq!(state.sync_changes.snapshot().revision, 0);
    }

    #[tokio::test]
    async fn message_receipt_table_commits_still_invalidate_browser_resources() {
        let (state, store, _directory) = state_with_store().await;

        for (table, statement) in [
            (
                "runtime_events",
                "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES('event','agent','message','text','done',1,1)",
            ),
            (
                "runtime_event_meta",
                "INSERT INTO runtime_event_meta(id,record) VALUES('event','{}')",
            ),
            (
                "runtime_items",
                "INSERT INTO runtime_items(id,agent,created,record) VALUES('item','agent',1,'{}')",
            ),
        ] {
            let statement = statement.to_owned();
            store
                .transaction(move |transaction| {
                    transaction.execute(&statement, [])?;
                    Ok(())
                })
                .await
                .unwrap_or_else(|error| panic!("{table} UI write commits: {error}"));
        }

        assert_eq!(state.sync_changes.snapshot().resource_revision, 3);
        assert_eq!(state.sync_changes.snapshot().revision, 3);
    }

    #[tokio::test]
    async fn mixed_writes_and_named_resource_publications_keep_their_invalidation_paths() {
        let (state, store, _directory) = state_with_store().await;

        store
            .transaction(move |transaction| {
                transaction.execute(
                    "INSERT INTO runtime_operation_receipts(id,signature,result) VALUES(?1,?2,?3)",
                    ("operation", "signature", "{}"),
                )?;
                transaction.execute(
                    "INSERT INTO runtime_events(id,agent,kind,text,status,created,epoch) VALUES(?1,?2,?3,?4,?5,?6,?7)",
                    ("event", "agent", "message", "text", "done", 1, 1),
                )?;
                Ok(())
            })
            .await
            .expect("mixed internal and UI writes commit");
        assert_eq!(state.sync_changes.snapshot().resource_revision, 1);

        let notifications = state.resource_notifications_handle();
        notifications
            .publish_once(&serde_json::json!({
                "requestId": "named-resource",
                "resources": [{"kind": "multi-server"}],
            }))
            .expect("named resource publication succeeds");
        let view = notifications
            .changes_since(&[serde_json::json!({"kind": "multi-server"})], Some(0))
            .expect("named resource cursor reads");
        assert_eq!(
            view.resources,
            vec![serde_json::json!({"kind": "multi-server"})]
        );
        assert_eq!(view.cursor.resource_revision, 1);
        assert_eq!(view.cursor.revision, 2);
    }

    #[tokio::test]
    async fn startup_compiles_registered_validators_and_enforces_documented_methods() {
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
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
        let state = HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
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

        let reader = store.test_reader().expect("stored preferences can be read");
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
        let (app, _directory) =
            probe_app(&[("GET", "/api/session"), ("POST", "/api/accounts/login")]);
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
        let (app, _directory) = probe_app(&[("POST", "/api/agents"), ("GET", "/api/agent-chat")]);
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
        let (app, _directory) = probe_app(&[("POST", "/api/federation")]);
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
        let (first_state, _directory) = state();
        first_state.close_mutation_admission();
        let app = super::validation_probe_router(
            first_state,
            &[("POST", "/api/agents"), ("POST", "/api/accounts/discover")],
        )
        .expect("probe router starts");
        let request = Request::builder()
            .method("POST")
            .uri("/api/agents")
            .header("host", "127.0.0.1:4620")
            .header("x-studio-request-id", "request-preserved")
            .body(Body::empty())
            .expect("request builds");

        let response = app.clone().oneshot(request).await.expect("response");
        assert_eq!(response.status(), StatusCode::FORBIDDEN);
        assert_eq!(
            response.headers()["x-studio-request-id"],
            "request-preserved"
        );

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
    async fn accepted_mutation_can_start_credentials_after_http_close_and_drains_first() {
        let directory = tempfile::tempdir().expect("isolated state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("runtime store opens"),
        );
        let state = Arc::new(HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            directory.path().to_string_lossy().into_owned(),
            4620,
            None,
            Some(Arc::clone(&store)),
            None,
        ));
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let credentials = Arc::clone(&state.credentials);
        let operation_store = Arc::clone(&store);
        let operation = async move {
            started_tx
                .send(())
                .expect("test observes credential operation start");
            release_rx
                .await
                .expect("test releases credential operation");
            let result = credentials.identity(operation_store, None).await;
            axum::response::Response::builder()
                .status(if result.is_ok() {
                    StatusCode::OK
                } else {
                    StatusCode::INTERNAL_SERVER_ERROR
                })
                .body(Body::empty())
                .expect("mutation response builds")
        };
        let response_waiter = {
            let state = Arc::clone(&state);
            tokio::spawn(async move {
                state
                    .run_tracked_mutation(
                        "POST /api/federation/credentials/rename".to_owned(),
                        Some("credential-lifecycle-fixture".to_owned()),
                        operation,
                    )
                    .await
            })
        };
        started_rx
            .await
            .expect("accepted mutation enters its credential-free preflight");

        state.close_mutation_admission();
        let shutdown_state = Arc::clone(&state);
        let shutdown = tokio::spawn(async move { shutdown_state.shutdown_mutations().await });
        tokio::task::yield_now().await;
        assert!(
            !shutdown.is_finished(),
            "shutdown must wait while the accepted mutation is suspended"
        );
        release_tx
            .send(())
            .expect("accepted mutation is awaiting its credential phase");
        shutdown.await.expect("mutation drain task completes");
        let response = response_waiter
            .await
            .expect("accepted mutation response task completes");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(
            state
                .credentials
                .identity(Arc::clone(&store), None)
                .await
                .is_err(),
            "credential admission closes only after the accepted mutation has drained"
        );
        state
            .shutdown_services()
            .await
            .expect("services stop after credentials drain");
    }

    #[tokio::test]
    async fn optional_required_and_chunked_json_bodies_follow_openapi_contract() {
        let (app, _directory) =
            probe_app(&[("POST", "/api/agents"), ("POST", "/api/accounts/discover")]);
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

        let streamed = Body::from_stream(futures_util::stream::iter([
            Ok::<_, std::convert::Infallible>(axum::body::Bytes::from_static(b"{\"prompt\":")),
            Ok(axum::body::Bytes::from_static(b"\"x\"}")),
        ]));
        assert_eq!(axum::body::HttpBody::size_hint(&streamed).exact(), None);
        let chunked = app
            .oneshot(build(
                "POST",
                "/api/agents",
                streamed,
                Some("application/json"),
            ))
            .await
            .expect("chunked body response");
        assert_eq!(chunked.status(), StatusCode::OK);
        let value: Value = serde_json::from_slice(
            &to_bytes(chunked.into_body(), 1024)
                .await
                .expect("streamed response"),
        )
        .expect("probe JSON");
        assert_eq!(value["body"], serde_json::json!({"prompt":"x"}));
    }

    #[tokio::test]
    async fn encoded_repeated_scalar_query_parameter_is_refused_by_router() {
        let (app, _directory) = probe_app(&[("GET", "/api/agent-chat")]);
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
    async fn boundary_accepts_each_route_body_limit_and_rejects_the_next_byte() {
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
            let (app, _directory) = probe_app(&[("POST", path)]);
            // The probe must return the validated body at the exact limit.
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
                    assert_eq!(response.status(), StatusCode::OK, "{path} at {limit} bytes");
                    let bytes = to_bytes(response.into_body(), 4096)
                        .await
                        .expect("probe response");
                    let value: Value = serde_json::from_slice(&bytes).expect("probe JSON");
                    assert_eq!(
                        value["body"],
                        serde_json::from_str::<Value>(valid_json).expect("valid fixture JSON")
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
