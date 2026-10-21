//! Durable owner-paired envelopes. A transaction claims each exact request
//! before transport or execution. Stable-identity peer deliveries may replay
//! after an unknown reply; provider commands remain one-shot.
use futures_util::future::BoxFuture;
use std::sync::Arc;
use studio_runtime_schema::delivery::DeliveryMoment;

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_store::{RuntimeStore, StoreError};

use crate::multi_server_auth::{AccessError, digest, refused, schema, stored, text};
use crate::remote_transport::{
    MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, MultiServerRequest, PairedRequest, canonical_json,
    exchange_paired_request, learn_paired_request_link, load_multi_server_identity,
    multi_server_signed_headers, new_nonce, verify_multi_server_response,
};

#[derive(Clone, Debug, PartialEq)]
pub enum ExchangeOutcome {
    Applied(Value),
    NotApplied(String),
    Unknown,
    /// The receiver has not crossed the effect boundary and will wait for its
    /// prerequisite. Unlike `Unknown`, this claim is safe to retry later.
    PreEffectWaiting,
}
impl ExchangeOutcome {
    pub fn to_value(&self, key: &str) -> Value {
        match self {
            Self::Applied(v) => json!({"requestId":key,"outcome":"applied","value":v}),
            Self::NotApplied(e) => json!({"requestId":key,"outcome":"not_applied","error":e}),
            Self::Unknown => json!({"requestId":key,"outcome":"unknown"}),
            Self::PreEffectWaiting => {
                json!({"requestId":key,"outcome":"unknown","waitingFor":"The stop or parent update"})
            }
        }
    }
    fn from_value(value: &Value, key: &str, now: i64) -> Result<Self, AccessError> {
        if text(value, "requestId") != key || value.to_string().len() > MAX_RESPONSE_BYTES {
            return Err(refused(
                502,
                "invalid_receipt",
                "The paired server returned an invalid receipt",
            ));
        }
        if value
            .get("value")
            .and_then(|v| v.get("outputExpiresAt"))
            .and_then(Value::as_f64)
            .is_some_and(|v| v <= now as f64)
        {
            return Ok(Self::Unknown);
        }
        match text(value, "outcome") {
            "applied" => value
                .get("value")
                .cloned()
                .map(Self::Applied)
                .ok_or_else(|| {
                    refused(
                        502,
                        "invalid_receipt",
                        "The paired server returned an invalid receipt",
                    )
                }),
            "not_applied" => value
                .get("error")
                .and_then(Value::as_str)
                .map(|s| Self::NotApplied(s.to_owned()))
                .ok_or_else(|| {
                    refused(
                        502,
                        "invalid_receipt",
                        "The paired server returned an invalid receipt",
                    )
                }),
            "unknown" => Ok(Self::Unknown),
            _ => Err(refused(
                502,
                "invalid_receipt",
                "The paired server returned an invalid receipt",
            )),
        }
    }
}
/// The transport must use the persisted envelope's requestId and return only a
/// response verified against that server's saved credential. It must complete
/// within the supplied deadline; a lost response returns Unknown, never retries.
pub trait PairedTransport: Send + Sync {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        envelope: &'a Value,
        timeout: std::time::Duration,
    ) -> BoxFuture<'a, Result<Value, AccessError>>;
}

/// Production adapter for existing paired-server credential rows. The shared
/// signed-request exchange owns QUIC-first transport and safe HTTPS fallback;
/// retry scheduling remains owned by the durable outbox driver.
pub struct HttpsPairedTransport {
    store: Arc<RuntimeStore>,
}

impl HttpsPairedTransport {
    pub fn new(store: Arc<RuntimeStore>) -> Self {
        Self { store }
    }

    fn paired_server_identity(
        connection: &rusqlite::Connection,
        server: &str,
    ) -> Result<(String, String), AccessError> {
        let raw: Option<String> = connection
            .query_row(
                "SELECT record FROM runtime_access_clients WHERE id=?1",
                [server],
                |row| row.get(0),
            )
            .optional()?;
        let value = raw
            .map(|raw| {
                stored(
                    "PersistedAccessClient",
                    "runtime_access_clients",
                    server,
                    &raw,
                )
            })
            .transpose()?
            .ok_or_else(|| refused(403, "client_revoked", "The server is not paired"))?;
        if text(&value, "id") != server
            || text(&value, "status") != "paired"
            || text(&value, "kind") != "server"
        {
            return Err(refused(403, "client_revoked", "The server is not paired"));
        }
        Ok((
            text(&value, "origin").to_owned(),
            text(&value, "publicKey").to_owned(),
        ))
    }
}

impl PairedTransport for HttpsPairedTransport {
    fn exchange<'a>(
        &'a self,
        server: &'a str,
        envelope: &'a Value,
        timeout: std::time::Duration,
    ) -> BoxFuture<'a, Result<Value, AccessError>> {
        Box::pin(async move {
            let store = Arc::clone(&self.store);
            let server = server.to_owned();
            let envelope = envelope.clone();
            let server_for_lookup = server.clone();
            let (origin, public_key) = store
                .read_async(
                    "runtime-services/multi_server_orchestration",
                    move |connection| Self::paired_server_identity(connection, &server_for_lookup),
                )
                .await?;
            let root = store.root().to_path_buf();
            let identity = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/multi_server_orchestration",
                move || load_multi_server_identity(&root),
            )
            .await
            .map_err(|_| {
                refused(
                    503,
                    "identity_unavailable",
                    "The local server identity is unavailable",
                )
            })?
            .map_err(|_| {
                refused(
                    503,
                    "identity_unavailable",
                    "The local server identity is unavailable",
                )
            })?;
            let request_id = text(&envelope, "requestId").to_owned();
            let url = format!("{}/api/servers/orchestration", origin.trim_end_matches('/'));
            let body = canonical_json(&envelope)
                .map_err(|_| refused(400, "invalid_request", "Invalid cross-server request"))?
                .into_bytes();
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| refused(503, "clock_unavailable", "The system clock is unavailable"))?
                .as_secs() as i64;
            let nonce = new_nonce()
                .map_err(|_| refused(503, "nonce_unavailable", "A request nonce is unavailable"))?;
            let headers = multi_server_signed_headers(
                &identity,
                &MultiServerRequest {
                    server_id: &server,
                    method: "POST",
                    url: &url,
                    request_id: &request_id,
                    timestamp: now,
                    nonce: &nonce,
                    body: &body,
                },
                now,
            )
            .map_err(|_| {
                refused(
                    503,
                    "signing_unavailable",
                    "The paired request could not be signed",
                )
            })?;
            let mut owned_headers = headers;
            owned_headers.push(("Accept-Encoding".to_owned(), "identity".to_owned()));
            if !body.is_empty() {
                owned_headers.push(("Content-Type".to_owned(), "application/json".to_owned()));
            }
            let public_key_for_call = public_key.clone();
            let server_for_call = server.clone();
            let url_for_call = url.clone();
            let response = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/multi_server_orchestration",
                move || {
                    let headers = owned_headers
                        .iter()
                        .map(|(name, value)| (name.as_str(), value.as_str()))
                        .collect::<Vec<_>>();
                    exchange_paired_request(
                        &store,
                        PairedRequest {
                            server_id: &server_for_call,
                            public_key: &public_key_for_call,
                            method: "POST",
                            url: &url_for_call,
                            headers: &headers,
                            body: &body,
                            timeout,
                            response_limit: MAX_RESPONSE_BYTES,
                        },
                    )
                },
            )
            .await
            .map_err(|_| {
                refused(
                    503,
                    "exchange_unknown",
                    "The paired delivery outcome is unknown",
                )
            })?
            .map_err(|_| {
                refused(
                    503,
                    "exchange_unknown",
                    "The paired delivery outcome is unknown",
                )
            })?;
            if !(200..300).contains(&response.status) {
                return Err(refused(
                    response.status,
                    "peer_rejected",
                    "The paired server refused the request",
                ));
            }
            let value = verify_multi_server_response(&response.body, &server, &public_key)
                .map_err(|_| {
                    refused(
                        503,
                        "invalid_receipt",
                        "The paired server returned an invalid receipt",
                    )
                })?;
            let advert = response
                .headers
                .iter()
                .find(|(name, _)| name.eq_ignore_ascii_case("x-studio-link"))
                .map(|(_, value)| value.as_str());
            learn_paired_request_link(&self.store, &server, advert);
            Ok(value)
        })
    }
}
/// Missing effects are refused before claiming a receipt. Implementations must
/// derive actor permissions from the authenticated principal, not the payload.
pub trait OrchestrationEffects: Send + Sync {
    fn supports(&self, action: &str) -> bool;
    fn execute<'a>(
        &'a self,
        principal: &'a str,
        envelope: &'a Value,
    ) -> BoxFuture<'a, Result<ExchangeOutcome, AccessError>>;
}

pub trait BrowserCallbackEffect: Send + Sync {
    fn receive<'a>(&'a self, envelope: &'a Value) -> BoxFuture<'a, Result<Value, AccessError>>;
}

/// Idempotent receiver effects shared by the signed HTTP route and tests.
/// Additional actions are enabled only when their production adapters are
/// installed; unsupported actions remain a no-write refusal.
pub struct RuntimeOrchestrationEffects {
    store: Arc<RuntimeStore>,
    browser_callback: Arc<dyn BrowserCallbackEffect>,
    default_account: String,
    known_accounts: Vec<Value>,
    lifecycle_ports: Option<Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>>,
}

impl RuntimeOrchestrationEffects {
    pub fn new(store: Arc<RuntimeStore>) -> Self {
        let browser_callback = Arc::new(crate::browser::BrowserCallbackReceiver::new(Arc::clone(
            &store,
        )));
        Self {
            store,
            browser_callback,
            default_account: "default".to_owned(),
            known_accounts: Vec::new(),
            lifecycle_ports: None,
        }
    }

    pub fn with_lifecycle_ports(
        store: Arc<RuntimeStore>,
        lifecycle_ports: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
    ) -> Self {
        let browser_callback = Arc::new(crate::browser::BrowserCallbackReceiver::new(Arc::clone(
            &store,
        )));
        Self {
            default_account: "default".to_owned(),
            known_accounts: Vec::new(),
            store,
            browser_callback,
            lifecycle_ports: Some(lifecycle_ports),
        }
    }

    pub fn with_browser_callback(
        store: Arc<RuntimeStore>,
        browser_callback: Arc<dyn BrowserCallbackEffect>,
    ) -> Self {
        Self {
            store,
            browser_callback,
            default_account: "default".to_owned(),
            known_accounts: Vec::new(),
            lifecycle_ports: None,
        }
    }

    pub fn with_project_accounts(
        store: Arc<RuntimeStore>,
        browser_callback: Arc<dyn BrowserCallbackEffect>,
        default_account: String,
        known_accounts: Vec<Value>,
    ) -> Self {
        Self {
            store,
            browser_callback,
            default_account,
            known_accounts,
            lifecycle_ports: None,
        }
    }

    pub fn with_project_accounts_and_lifecycle_ports(
        store: Arc<RuntimeStore>,
        browser_callback: Arc<dyn BrowserCallbackEffect>,
        default_account: String,
        known_accounts: Vec<Value>,
        lifecycle_ports: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
    ) -> Self {
        Self {
            store,
            browser_callback,
            default_account,
            known_accounts,
            lifecycle_ports: Some(lifecycle_ports),
        }
    }

    async fn stop_worker(
        &self,
        principal: &str,
        envelope: &Value,
    ) -> Result<ExchangeOutcome, AccessError> {
        let Some(ports) = self.lifecycle_ports.as_ref() else {
            return Err(refused(
                501,
                "orchestration_effect",
                "The remote stop lifecycle is unavailable",
            ));
        };
        let payload = &envelope["payload"];
        let request_id = text(envelope, "requestId");
        let link = text(payload, "link");
        let worker = text(payload, "worker");
        let control_epoch = payload
            .get("controlEpoch")
            .and_then(Value::as_i64)
            .ok_or_else(|| refused(400, "invalid_request", "Invalid remote stop request"))?;
        let reason = payload
            .get("reason")
            .and_then(Value::as_str)
            .unwrap_or("Stopped by the parent")
            .to_owned();
        if request_id.is_empty() || link.is_empty() || worker.is_empty() {
            return Err(refused(
                400,
                "invalid_request",
                "Invalid remote stop request",
            ));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| refused(503, "clock_unavailable", "The system clock is unavailable"))?
            .as_secs_f64();
        let action = crate::agent_management::Action::RemoteStop {
            principal: principal.to_owned(),
            link: link.to_owned(),
            target: worker.to_owned(),
            request_id: request_id.to_owned(),
            control_epoch,
            reason,
        };
        crate::agent_lifecycle_runtime::execute(
            Arc::clone(&self.store),
            &action,
            now,
            Arc::clone(ports),
        )
        .await
        .map(ExchangeOutcome::Applied)
        .map_err(|error| match error {
            crate::agent_management::ManagementError::Unavailable(name) => {
                refused(501, "orchestration_dependency", name)
            }
            crate::agent_management::ManagementError::Invalid(message) => {
                refused(409, "stop_refused", message)
            }
            crate::agent_management::ManagementError::Stored { .. }
            | crate::agent_management::ManagementError::Store(_) => refused(
                503,
                "stop_unavailable",
                "The remote stop could not be committed",
            ),
        })
    }

    async fn project_location(
        &self,
        principal: &str,
        envelope: &Value,
    ) -> Result<ExchangeOutcome, AccessError> {
        let action = text(envelope, "action");
        let payload = &envelope["payload"];
        let home_project = text(payload, "project");
        let request_id = text(envelope, "requestId");
        if home_project.is_empty() || request_id.is_empty() {
            return Err(refused(400, "invalid_request", "Invalid project location"));
        }
        if action == "add_location" {
            let path = text(payload, "path");
            let name = text(payload, "name");
            if path.is_empty() || path.len() > 4096 || name.is_empty() || name.len() > 255 {
                return Err(refused(400, "invalid_request", "Invalid project location"));
            }
            let registered = crate::projects::write(
                Arc::clone(&self.store),
                json!({"action":"register","path":path,"name":name}),
                self.default_account.clone(),
                self.known_accounts.clone(),
            )
            .await
            .map_err(|_| {
                refused(
                    400,
                    "location_unavailable",
                    "The project folder is unavailable",
                )
            })?;
            let project_id = text(&registered, "id");
            let canonical_path = text(&registered, "path");
            if project_id.is_empty() || canonical_path.is_empty() {
                return Err(refused(
                    503,
                    "invalid_project",
                    "The project folder is unavailable",
                ));
            }
            let store_for_identity = Arc::clone(&self.store);
            let local_server = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/multi_server_orchestration",
                move || store_for_identity.local_server_id(),
            )
            .await
            .map_err(|_| {
                refused(
                    503,
                    "identity_unavailable",
                    "The server identity is unavailable",
                )
            })??
            .ok_or_else(|| {
                refused(
                    503,
                    "identity_unavailable",
                    "The server identity is unavailable",
                )
            })?;
            let canonical_path_for_info = canonical_path.to_owned();
            let git_origin = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/multi_server_orchestration",
                move || crate::projects::peer_location_info(&canonical_path_for_info),
            )
            .await
            .map_err(|_| {
                refused(
                    400,
                    "location_unavailable",
                    "The project folder is unavailable",
                )
            })?
            .ok()
            .and_then(|value| value.get("gitOrigin").cloned())
            .unwrap_or(Value::Null);
            let project_id_for_write = project_id.to_owned();
            let principal_for_write = principal.to_owned();
            let home_project_for_write = home_project.to_owned();
            let name_for_write = name.to_owned();
            let local_server_for_write = local_server.clone();
            self.store.transaction_with(move |tx| {
                let (_, mut project) = studio_runtime_store::ProjectsRepository::get(
                    tx, &project_id_for_write,
                )?
                .ok_or_else(|| refused(503, "project_unavailable", "The project is unavailable"))?;
                let aliases = project
                    .get("projectAliases")
                    .and_then(Value::as_array)
                    .cloned()
                    .unwrap_or_default();
                let mut aliases = aliases
                    .into_iter()
                    .filter(|alias| {
                        alias.get("serverId").and_then(Value::as_str) != Some(principal_for_write.as_str())
                            || alias.get("projectId").and_then(Value::as_str) != Some(home_project_for_write.as_str())
                    })
                    .collect::<Vec<_>>();
                if principal_for_write != local_server_for_write || home_project_for_write != project_id_for_write {
                    aliases
                        .push(json!({"serverId":principal_for_write,"projectId":home_project_for_write,"name":name_for_write}));
                }
                project["projectAliases"] = Value::Array(aliases);
                studio_runtime_store::ProjectsRepository::put_project(tx, &project)?;
                Ok::<_, AccessError>(())
            }).await?;
            return Ok(ExchangeOutcome::Applied(json!({
                "serverId":local_server,"path":path,"canonicalPath":canonical_path,
                "projectId":project_id,"gitOrigin":git_origin,
                "homeProjectId":home_project,"requestId":request_id
            })));
        }
        let location = &payload["location"];
        let project_id = text(location, "projectId");
        if !project_id.is_empty() {
            let project_id_for_write = project_id.to_owned();
            let principal_for_write = principal.to_owned();
            let home_project_for_write = home_project.to_owned();
            self.store
                .transaction_with(move |tx| {
                    if let Some((_, mut project)) =
                        studio_runtime_store::ProjectsRepository::get(tx, &project_id_for_write)?
                    {
                        let aliases = project
                            .get("projectAliases")
                            .and_then(Value::as_array)
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .filter(|alias| {
                                alias.get("serverId").and_then(Value::as_str)
                                    != Some(principal_for_write.as_str())
                                    || alias.get("projectId").and_then(Value::as_str)
                                        != Some(home_project_for_write.as_str())
                            })
                            .collect::<Vec<_>>();
                        project["projectAliases"] = Value::Array(aliases);
                        studio_runtime_store::ProjectsRepository::put_project(tx, &project)?;
                    }
                    Ok::<_, AccessError>(())
                })
                .await?;
        }
        let store_for_identity = Arc::clone(&self.store);
        let local_server = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/multi_server_orchestration",
            move || store_for_identity.local_server_id(),
        )
        .await
        .map_err(|_| {
            refused(
                503,
                "identity_unavailable",
                "The server identity is unavailable",
            )
        })??
        .unwrap_or_default();
        Ok(ExchangeOutcome::Applied(
            json!({"removed":true,"serverId":local_server}),
        ))
    }

    async fn worker_call(
        &self,
        principal: &str,
        envelope: &Value,
    ) -> Result<ExchangeOutcome, AccessError> {
        let action = text(envelope, "action");
        let key = text(envelope, "requestId");
        let payload = &envelope["payload"];
        let link_id = text(payload, "link");
        let worker_id = text(payload, "worker");
        let epoch = payload
            .get("epoch")
            .and_then(Value::as_i64)
            .ok_or_else(|| refused(400, "invalid_request", "Invalid cross-server request"))?;
        let parent_epoch = payload
            .get("parentEpoch")
            .and_then(Value::as_i64)
            .ok_or_else(|| refused(400, "invalid_request", "Invalid cross-server request"))?;
        let args = &payload["args"];
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| refused(503, "clock_unavailable", "The system clock is unavailable"))?
            .as_secs_f64();
        let move_admission = if action == "move_send" {
            let target_id = args
                .get("agent_id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .ok_or_else(|| refused(400, "invalid_request", "Invalid move-send request"))?;
            let store = Arc::clone(&self.store);
            let target_id = target_id.to_owned();
            Some(
                self.store
                    .read_async(
                        "runtime-services/multi_server_orchestration",
                        move |connection| {
                            crate::message_admission::MessageIngressAdmission::read(
                                Arc::clone(&store),
                                connection,
                                &target_id,
                            )
                            .map_err(|_| {
                                refused(503, "send_admission", "The message could not be admitted")
                            })
                        },
                    )
                    .await?
                    .prepare()
                    .await
                    .map_err(|_| {
                        refused(503, "send_admission", "The message could not be admitted")
                    })?,
            )
        } else {
            None
        };
        let link_id = link_id.to_owned();
        let principal = principal.to_owned();
        let worker_id = worker_id.to_owned();
        let key = key.to_owned();
        let action = action.to_owned();
        let args = args.clone();
        self.store
            .transaction_with(move |tx| {
                let link_raw: Option<String> = tx
                    .query_row(
                        "SELECT record FROM runtime_server_links WHERE id=?1",
                        [&link_id],
                        |row| row.get(0),
                    )
                    .optional()?;
                let link: Value = serde_json::from_str(link_raw.as_deref().ok_or_else(|| {
                    refused(
                        404,
                        "link_unavailable",
                        "The remote-parent link is unavailable",
                    )
                })?)
                .map_err(|_| refused(503, "invalid_record", "The remote-parent link is invalid"))?;
                if link["side"] != "home"
                    || link["server"] != principal
                    || link["parentEpoch"].as_i64() != Some(parent_epoch)
                    || !link["workers"].as_array().is_some_and(|workers| {
                        workers
                            .iter()
                            .any(|worker| worker.as_str() == Some(worker_id.as_str()))
                    })
                {
                    return Err(refused(
                        403,
                        "link_owner",
                        "The remote-parent link is invalid",
                    ));
                }
                let parent_id = link.get("parent").and_then(Value::as_str).ok_or_else(|| {
                    refused(503, "invalid_record", "The remote-parent link is invalid")
                })?;
                let parent = studio_runtime_store::AgentRepository::get(
                    tx,
                    &studio_runtime_store::AgentId::from(parent_id),
                )?
                .ok_or_else(|| {
                    refused(404, "parent_unavailable", "The parent agent is unavailable")
                })?;
                if parent.epoch as i64 != parent_epoch
                    || parent.extra.get("autoWake") != Some(&Value::Bool(true))
                    || studio_runtime_store::truthy(parent.extra.get("deletedAt"))
                {
                    return Err(refused(409, "parent_stopped", "The home team was stopped"));
                }
                let worker = studio_runtime_store::AgentRepository::get(
                    tx,
                    &studio_runtime_store::AgentId::from(worker_id.as_str()),
                )?
                .ok_or_else(|| {
                    refused(
                        404,
                        "worker_unavailable",
                        "The remote worker is unavailable",
                    )
                })?;
                let expected_remote = json!({"server":principal,"link":link_id});
                if worker.epoch != epoch
                    || worker.extra.get("remoteWorker") != Some(&expected_remote)
                    || !studio_runtime_store::truthy(worker.extra.get("movedTo"))
                    || worker.extra.get("autoWake") != Some(&Value::Bool(true))
                    || studio_runtime_store::truthy(worker.extra.get("deletedAt"))
                {
                    return Err(refused(409, "worker_stopped", "The worker was stopped"));
                }
                match action.as_str() {
                    "message" => {
                        crate::provider_callbacks::tools::services::messages::send_remote_worker(
                            tx, &worker_id, &args, &key, now,
                        )
                        .map(ExchangeOutcome::Applied)
                        .map_err(callback_error)
                    }
                    "move_send" => {
                        let target_id = args
                            .get("agent_id")
                            .and_then(Value::as_str)
                            .filter(|id| !id.is_empty())
                            .ok_or_else(|| {
                                refused(400, "invalid_request", "Invalid move-send request")
                            })?;
                        let message = args
                            .get("text")
                            .and_then(Value::as_str)
                            .map(crate::event_write::python_strip)
                            .filter(|text| {
                                !text.is_empty()
                                    && crate::coordination::message_text_within_limit(text)
                            })
                            .ok_or_else(|| {
                                refused(400, "invalid_request", "Invalid move-send request")
                            })?;
                        let delivery = delivery_moment(&args)?;
                        let target =
                            crate::event_write::checked_agent(tx, target_id).map_err(|_| {
                                refused(
                                    404,
                                    "target_unavailable",
                                    "The target agent is unavailable",
                                )
                            })?;
                        if target.root_id != worker.root_id
                            || target.id == worker.id
                            || !is_descendant_of(tx, target_id, &worker_id)?
                            || target.extra.get("autoWake") != Some(&Value::Bool(true))
                        {
                            return Err(refused(
                                403,
                                "target_forbidden",
                                "The worker cannot message this agent",
                            ));
                        }
                        crate::coordination::send_followup(
                            tx,
                            crate::coordination::AgentFollowup {
                                sender_id: &worker_id,
                                sender_epoch: epoch,
                                recipient_id: target_id,
                                id: &key,
                                text: message,
                                delivery,
                                now,
                            },
                            move_admission.as_ref().ok_or_else(|| {
                                refused(503, "send_admission", "The message could not be admitted")
                            })?,
                        )
                        .map(|result| ExchangeOutcome::Applied(result.receipt))
                        .map_err(|_| refused(403, "send_refused", "The message was refused"))
                    }
                    _ => Err(refused(
                        501,
                        "orchestration_effect",
                        "The cross-server action is unavailable",
                    )),
                }
            })
            .await
    }
}

async fn receive_remote_anchor_input(
    store: Arc<RuntimeStore>,
    principal: &str,
    envelope: &Value,
) -> Result<ExchangeOutcome, AccessError> {
    let payload = &envelope["payload"];
    let key = text(envelope, "requestId");
    let link_id = text(payload, "link");
    let kind = text(payload, "kind");
    let event_text = text(payload, "text");
    let parent_epoch = payload
        .get("parentEpoch")
        .and_then(Value::as_i64)
        .ok_or_else(|| refused(400, "invalid_request", "Invalid remote anchor input"))?;
    let delivery = delivery_moment(payload)?;
    if key.is_empty()
        || link_id.is_empty()
        || !crate::coordination::message_text_within_limit(event_text)
        || kind != "user"
    {
        return Err(refused(
            400,
            "invalid_request",
            "Invalid remote anchor input",
        ));
    }

    let store_for_read = Arc::clone(&store);
    let link_id_for_read = link_id.to_owned();
    let principal_for_read = principal.to_owned();
    let parent_epoch_for_read = parent_epoch;
    let (parent_id, admission) = store
        .read_async(
            "runtime-services/multi_server_orchestration",
            move |connection| {
                let raw: Option<String> = connection
                    .query_row(
                        "SELECT record FROM runtime_server_links WHERE id=?1",
                        [link_id_for_read],
                        |row| row.get(0),
                    )
                    .optional()?;
                let link: Value = raw
                    .as_deref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .ok_or_else(|| {
                        refused(
                            404,
                            "link_unavailable",
                            "The remote-parent link is unavailable",
                        )
                    })?;
                if link.get("side").and_then(Value::as_str) != Some("home")
                    || link.get("server").and_then(Value::as_str)
                        != Some(principal_for_read.as_str())
                    || link.get("parentEpoch").and_then(Value::as_i64)
                        != Some(parent_epoch_for_read)
                {
                    return Err(refused(
                        403,
                        "link_owner",
                        "The remote-parent link is invalid",
                    ));
                }
                let parent_id = link.get("parent").and_then(Value::as_str).ok_or_else(|| {
                    refused(503, "invalid_record", "The remote-parent link is invalid")
                })?;
                let parent = studio_runtime_store::AgentRepository::get(
                    connection,
                    &studio_runtime_store::AgentId::from(parent_id),
                )?
                .ok_or_else(|| {
                    refused(404, "parent_unavailable", "The home agent is unavailable")
                })?;
                if parent.epoch != parent_epoch {
                    return Err(refused(409, "parent_changed", "The home agent changed"));
                }
                let admission = crate::message_admission::MessageIngressAdmission::read(
                    Arc::clone(&store_for_read),
                    connection,
                    parent_id,
                )
                .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
                Ok::<_, AccessError>((parent_id.to_owned(), admission))
            },
        )
        .await?;

    let admission = admission
        .prepare()
        .await
        .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
    let key = key.to_owned();
    let link_id = link_id.to_owned();
    let principal = principal.to_owned();
    let event_text = event_text.to_owned();
    let receipt = store
        .transaction_with(move |tx| {
            let current_link: Option<String> = tx
                .query_row(
                    "SELECT record FROM runtime_server_links WHERE id=?1",
                    [&link_id],
                    |row| row.get(0),
                )
                .optional()?;
            let current_link: Value = current_link
                .as_deref()
                .and_then(|raw| serde_json::from_str(raw).ok())
                .ok_or_else(|| {
                    refused(
                        404,
                        "link_unavailable",
                        "The remote-parent link is unavailable",
                    )
                })?;
            if current_link.get("side").and_then(Value::as_str) != Some("home")
                || current_link.get("server").and_then(Value::as_str) != Some(principal.as_str())
                || current_link.get("parent").and_then(Value::as_str) != Some(parent_id.as_str())
                || current_link.get("parentEpoch").and_then(Value::as_i64) != Some(parent_epoch)
            {
                return Err(refused(
                    403,
                    "link_owner",
                    "The remote-parent link is invalid",
                ));
            }
            let current = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from(parent_id.as_str()),
            )?
            .ok_or_else(|| refused(404, "parent_unavailable", "The home agent is unavailable"))?;
            if current.epoch != parent_epoch {
                return Err(refused(409, "parent_changed", "The home agent changed"));
            }
            let posted = crate::coordination::post_message_with_admission(
                tx,
                crate::coordination::PostManagedMessage {
                    room: &parent_id,
                    id: &key,
                    text: &event_text,
                    delivery,
                    assets: &[],
                },
                &admission,
            )
            .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
            Ok::<_, AccessError>(posted.receipt)
        })
        .await?;
    Ok(ExchangeOutcome::Applied(receipt))
}

fn durable_anchor_input_receipt(
    transaction: &Transaction<'_>,
    principal: &str,
    envelope: &Envelope,
) -> Result<Option<Value>, AccessError> {
    let payload = &envelope.payload;
    if envelope.action != "input"
        || payload
            .get(REMOTE_ANCHOR_INPUT_FIELD)
            .and_then(Value::as_bool)
            != Some(true)
    {
        return Ok(None);
    }
    let link_id = text(payload, "link");
    let event_text = text(payload, "text");
    let Ok(delivery) = delivery_moment(payload) else {
        return Ok(None);
    };
    let Some(parent_epoch) = payload.get("parentEpoch").and_then(Value::as_i64) else {
        return Ok(None);
    };
    if envelope.key.is_empty()
        || link_id.is_empty()
        || text(payload, "kind") != "user"
        || !crate::coordination::message_text_within_limit(event_text)
    {
        return Ok(None);
    }
    let raw: Option<String> = transaction
        .query_row(
            "SELECT record FROM runtime_server_links WHERE id=?1",
            [link_id],
            |row| row.get(0),
        )
        .optional()?;
    let Some(link) = raw
        .as_deref()
        .and_then(|raw| serde_json::from_str::<Value>(raw).ok())
    else {
        return Ok(None);
    };
    if link.get("side").and_then(Value::as_str) != Some("home")
        || link.get("server").and_then(Value::as_str) != Some(principal)
        || link.get("parentEpoch").and_then(Value::as_i64) != Some(parent_epoch)
    {
        return Ok(None);
    }
    let Some(parent_id) = link.get("parent").and_then(Value::as_str) else {
        return Ok(None);
    };
    crate::coordination::remote_message_receipt(
        transaction,
        parent_id,
        &envelope.key,
        event_text,
        delivery,
    )
    .map_err(|_| {
        refused(
            503,
            "invalid_record",
            "Stored remote message receipt is invalid",
        )
    })
}

async fn receive_remote_input(
    store: Arc<RuntimeStore>,
    principal: &str,
    envelope: &Value,
) -> Result<ExchangeOutcome, AccessError> {
    let payload = &envelope["payload"];
    let key = text(envelope, "requestId");
    let link_id = text(payload, "link");
    let worker_id = text(payload, "worker");
    let kind = text(payload, "kind");
    let event_text = text(payload, "text");
    let parent_epoch = payload
        .get("parentEpoch")
        .and_then(Value::as_i64)
        .ok_or_else(|| refused(400, "invalid_request", "Invalid remote input"))?;
    let control_epoch = payload
        .get("controlEpoch")
        .and_then(Value::as_i64)
        .ok_or_else(|| refused(400, "invalid_request", "Invalid remote input"))?;
    let remote_epoch = payload
        .get("epoch")
        .and_then(Value::as_i64)
        .ok_or_else(|| refused(400, "invalid_request", "Invalid remote input"))?;
    let delivery = delivery_moment(payload)?;
    if key.is_empty()
        || link_id.is_empty()
        || worker_id.is_empty()
        || (if matches!(kind, "user" | "followup") {
            !crate::coordination::message_text_within_limit(event_text)
        } else {
            event_text.len() > 32_000
        })
        || !matches!(
            kind,
            "user"
                | "followup"
                | "child_result"
                | "child_stopped"
                | "agent_message"
                | "work_review"
        )
    {
        return Err(refused(400, "invalid_request", "Invalid remote input"));
    }
    let store_for_read = Arc::clone(&store);
    let link_id_for_read = link_id.to_owned();
    let principal_for_read = principal.to_owned();
    let worker_id_for_read = worker_id.to_owned();
    let parent_epoch_for_read = parent_epoch;
    let control_epoch_for_read = control_epoch;
    let remote_epoch_for_read = remote_epoch;
    let kind_for_read = kind.to_owned();
    let preflight = store
        .read_async(
            "runtime-services/multi_server_orchestration",
            move |connection| {
                let raw: Option<String> = connection
                    .query_row(
                        "SELECT record FROM runtime_server_links WHERE id=?1",
                        [link_id_for_read],
                        |row| row.get(0),
                    )
                    .optional()?;
                let link: Value = raw
                    .as_deref()
                    .and_then(|raw| serde_json::from_str(raw).ok())
                    .ok_or_else(|| {
                        refused(
                            404,
                            "link_unavailable",
                            "The remote-parent link is unavailable",
                        )
                    })?;
                if link["side"] != "remote"
                    || link["server"] != principal_for_read
                    || link["parentEpoch"].as_i64() != Some(parent_epoch_for_read)
                    || !link["workers"].as_array().is_some_and(|workers| {
                        workers
                            .iter()
                            .any(|worker| worker.as_str() == Some(worker_id_for_read.as_str()))
                    })
                {
                    return Err(refused(
                        403,
                        "link_owner",
                        "The remote-parent link is invalid",
                    ));
                }
                let worker = studio_runtime_store::AgentRepository::get(
                    connection,
                    &studio_runtime_store::AgentId::from(worker_id_for_read.as_str()),
                )?
                .ok_or_else(|| {
                    refused(
                        404,
                        "worker_unavailable",
                        "The remote worker is unavailable",
                    )
                })?;
                let observed_control = worker
                    .extra
                    .get("remoteControlEpoch")
                    .and_then(Value::as_i64)
                    .unwrap_or(0);
                if observed_control < control_epoch_for_read {
                    return Ok((worker, None, Some(ExchangeOutcome::PreEffectWaiting)));
                }
                if observed_control != control_epoch_for_read
                    || worker
                        .extra
                        .get("remoteEpoch")
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        > remote_epoch_for_read
                {
                    return Ok((
                        worker,
                        None,
                        Some(ExchangeOutcome::NotApplied(
                            "The remote worker epoch changed".to_owned(),
                        )),
                    ));
                }
                let admission = if matches!(kind_for_read.as_str(), "user" | "followup") {
                    Some(
                        crate::message_admission::MessageIngressAdmission::read(
                            Arc::clone(&store_for_read),
                            connection,
                            worker_id_for_read.as_str(),
                        )
                        .map_err(|_| {
                            refused(400, "input_rejected", "The remote input was not admitted")
                        })?,
                    )
                } else {
                    None
                };
                Ok::<_, AccessError>((worker, admission, None))
            },
        )
        .await?;
    let (worker, admission, early_result) = preflight;
    let admission =
        match admission {
            Some(snapshot) => Some(snapshot.prepare().await.map_err(|_| {
                refused(400, "input_rejected", "The remote input was not admitted")
            })?),
            None => None,
        };
    if let Some(result) = early_result {
        return Ok(result);
    }
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| refused(503, "clock_unavailable", "The system clock is unavailable"))?
        .as_secs_f64();
    let key = key.to_owned();
    let worker_id = worker_id.to_owned();
    let kind = kind.to_owned();
    let event_text = event_text.to_owned();
    let result = store
        .transaction_with(move |tx| {
            let current = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from(worker_id.as_str()),
            )?
            .ok_or_else(|| {
                refused(
                    404,
                    "worker_unavailable",
                    "The remote worker is unavailable",
                )
            })?;
            if current.epoch != worker.epoch
                || current
                    .extra
                    .get("remoteControlEpoch")
                    .and_then(Value::as_i64)
                    .unwrap_or(0)
                    != control_epoch
            {
                return Err(refused(409, "worker_changed", "The remote worker changed"));
            }
            if studio_runtime_store::QueueEventRepository::get(
                tx,
                &studio_runtime_store::QueueEventId::from(key.as_str()),
            )?
            .is_none()
            {
                studio_runtime_store::QueueWriteRepository::put_metadata(
                    tx,
                    &key,
                    &json!({"delivery":delivery}),
                )?;
            }
            if studio_runtime_store::truthy(current.extra.get("moveImportPending"))
                || studio_runtime_store::truthy(current.extra.get("movedTo"))
            {
                crate::event_write::enqueue_recovery_event(
                    tx,
                    &current,
                    key.to_owned(),
                    &kind,
                    event_text.to_owned(),
                    now,
                )
                .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
                return Ok::<Value, AccessError>(json!({"eventId":key}));
            }
            if matches!(kind.as_str(), "user" | "followup") {
                let admission = admission.as_ref().ok_or_else(|| {
                    refused(400, "input_rejected", "The remote input was not admitted")
                })?;
                let posted = crate::coordination::post_message_with_admission(
                    tx,
                    crate::coordination::PostManagedMessage {
                        room: &worker_id,
                        id: &key,
                        text: &event_text,
                        delivery,
                        assets: &[],
                    },
                    admission,
                )
                .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
                return Ok(posted.receipt);
            }
            crate::event_write::enqueue_recovery_event(
                tx,
                &current,
                key.to_owned(),
                &kind,
                event_text.to_owned(),
                now,
            )
            .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
            Ok(json!({"eventId":key}))
        })
        .await?;
    Ok(ExchangeOutcome::Applied(result))
}

fn callback_error(error: crate::provider_callbacks::CallbackError) -> AccessError {
    use crate::provider_callbacks::CallbackError as Callback;
    match error {
        Callback::Store(error) => AccessError::Store(error),
        Callback::Invalid(message) => refused(400, "message_refused", message),
        Callback::Dependency(message) => refused(501, "orchestration_dependency", message),
        Callback::Conflict => refused(
            409,
            "request_conflict",
            "This request conflicts with a saved message",
        ),
        Callback::Stale => refused(409, "worker_stopped", "The worker was stopped"),
        Callback::Unknown => refused(503, "message_unknown", "The message outcome is unknown"),
        Callback::Stored(_) => refused(503, "invalid_record", "A stored message record is invalid"),
    }
}

fn is_descendant_of(
    tx: &rusqlite::Transaction<'_>,
    target_id: &str,
    ancestor_id: &str,
) -> Result<bool, AccessError> {
    let mut current = studio_runtime_store::AgentRepository::get(
        tx,
        &studio_runtime_store::AgentId::from(target_id),
    )?;
    let mut seen = std::collections::HashSet::new();
    while let Some(agent) = current {
        let parent = agent
            .extra
            .get("parentId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if parent == ancestor_id {
            return Ok(true);
        }
        if parent.is_empty() || !seen.insert(parent.to_owned()) {
            return Ok(false);
        }
        current = studio_runtime_store::AgentRepository::get(
            tx,
            &studio_runtime_store::AgentId::from(parent),
        )?;
    }
    Ok(false)
}

impl OrchestrationEffects for RuntimeOrchestrationEffects {
    fn supports(&self, action: &str) -> bool {
        matches!(
            action,
            "event"
                | "message"
                | "move_send"
                | "input"
                | "browser_callback"
                | "folders"
                | "add_location"
                | "remove_location"
                | "location_info"
                | "location_matches"
                | EXEC_EVENT_ACTION
        ) || GIT_CHANNEL_ACTIONS.contains(&action)
            || (cfg!(unix) && SERVER_COMMAND_ACTIONS.contains(&action))
            || action == "projects"
            || (action == "stop" && self.lifecycle_ports.is_some())
    }

    fn execute<'a>(
        &'a self,
        principal: &'a str,
        envelope: &'a Value,
    ) -> BoxFuture<'a, Result<ExchangeOutcome, AccessError>> {
        Box::pin(self.execute_async(principal, envelope))
    }
}

/// Git channel actions that the receiving server answers from its own
/// repositories and bundle exports (Python `_git_action`).
const GIT_CHANNEL_ACTIONS: &[&str] = &["git", "export", "chunk", "release"];

/// Command actions that this server runs for a lead (Python `_receive`).
const SERVER_COMMAND_ACTIONS: &[&str] = &[EXEC_ACTION, "exec_read", "exec_input", "exec_cancel"];

/// Python `exec_completed`: a command's exit notice for its lead. The lead of
/// this server gets its event in the same transaction; a paired server's lead
/// gets one durable `exec_event` envelope.
struct ExecNotices {
    store: Arc<RuntimeStore>,
}

impl crate::server_exec::ExecEvents for ExecNotices {
    fn completed<'a>(
        &'a self,
        principal: &'a str,
        actor: &'a str,
        record: &'a Value,
    ) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let handle = text(record, "handle").to_owned();
            let status = text(record, "status").to_owned();
            let key = crate::projects::cross_server_identity(&["command-exit", &handle, &status]);
            let payload = json!({"actor": actor, "handle": handle, "record": record});
            let principal = principal.to_owned();
            let store = Arc::clone(&self.store);
            let local = store.local_server_id().ok().flatten().unwrap_or_default();
            let Ok(now) = crate::event_write::unix_time() else {
                return;
            };
            let result = self
                .store
                .transaction_with(move |tx| -> Result<(), AccessError> {
                    if principal == local {
                        receive_exec_event(tx, &principal, &payload, key, now)?;
                        return Ok(());
                    }
                    let queued: Option<i64> = tx
                        .query_row(
                            "SELECT 1 FROM runtime_server_outbox WHERE id=?1 UNION SELECT 1 FROM runtime_server_retired WHERE id=?1 AND direction='out'",
                            [&key],
                            |row| row.get(0),
                        )
                        .optional()?;
                    if queued.is_none() {
                        enqueue_in_transaction(tx, &principal, EXEC_EVENT_ACTION, &payload, &key, now as i64)?;
                    }
                    Ok(())
                })
                .await;
            if let Err(error) = result {
                eprintln!("server command exit notice failed: {error}");
            }
        })
    }
}

impl RuntimeOrchestrationEffects {
    /// Python `_receive` for `exec`, `exec_read`, `exec_input` and `exec_cancel`.
    async fn server_command(
        &self,
        principal: &str,
        envelope: &Value,
    ) -> Result<ExchangeOutcome, AccessError> {
        let unknown = || refused(503, "command_unknown", "The command outcome is unknown");
        let runner = crate::server_exec::ServerExec::for_store(
            &self.store,
            Arc::new(ExecNotices {
                store: Arc::clone(&self.store),
            }),
        )
        .await
        .map_err(|_| unknown())?;
        let action = text(envelope, "action");
        let key = text(envelope, "requestId");
        let payload = &envelope["payload"];
        let result = match action {
            EXEC_ACTION => {
                let local = self.store.local_server_id()?.unwrap_or_default();
                runner.start(principal, &local, payload, key).await
            }
            "exec_read" => runner.read(principal, payload).await,
            _ => runner.control(principal, action, payload, key).await,
        };
        match result {
            Ok(value) => Ok(ExchangeOutcome::Applied(value)),
            Err(crate::server_exec::ExecError::Refused(message)) => {
                Ok(ExchangeOutcome::NotApplied(message))
            }
            Err(crate::server_exec::ExecError::Failed(_)) => Err(unknown()),
        }
    }

    /// Python `runtime.projects()`: every project ordered by creation time and ID.
    async fn projects(&self) -> Result<ExchangeOutcome, AccessError> {
        let mut listed = crate::projects::list(&self.store).await.map_err(|_| {
            refused(
                503,
                "projects_unavailable",
                "The project list is unavailable",
            )
        })?;
        if let Some(items) = listed.get_mut("items").and_then(Value::as_array_mut) {
            items.sort_by(|left, right| {
                let created =
                    |value: &Value| value.get("created").and_then(Value::as_f64).unwrap_or(0.0);
                created(left)
                    .total_cmp(&created(right))
                    .then_with(|| text(left, "id").cmp(text(right, "id")))
            });
        }
        Ok(ExchangeOutcome::Applied(listed))
    }

    /// Python `_git_action` for Git reads and branch bundles. A refusal is a
    /// not-applied receipt; a process failure leaves the outcome unknown.
    async fn git_channel(&self, envelope: &Value) -> Result<ExchangeOutcome, AccessError> {
        let root = self.store.root().to_path_buf();
        let action = text(envelope, "action").to_owned();
        let key = text(envelope, "requestId").to_owned();
        let payload = envelope["payload"].clone();
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/multi_server_orchestration",
            move || crate::server_git::receive(&root, &action, &payload, &key),
        )
        .await
        .map_err(|_| refused(503, "git_unavailable", "The Git read could not run"))?;
        match result {
            Ok(value) => Ok(ExchangeOutcome::Applied(value)),
            Err(crate::server_git::GitError::Refused(message)) => {
                Ok(ExchangeOutcome::NotApplied(message))
            }
            Err(crate::server_git::GitError::Failed(_)) => Err(refused(
                503,
                "git_unavailable",
                "The Git read could not run",
            )),
        }
    }

    async fn execute_async(
        &self,
        principal: &str,
        envelope: &Value,
    ) -> Result<ExchangeOutcome, AccessError> {
        let action = text(envelope, "action");
        if action == "browser_callback" {
            return self
                .browser_callback
                .receive(envelope)
                .await
                .map(ExchangeOutcome::Applied);
        }
        if action == "stop" {
            return self.stop_worker(principal, envelope).await;
        }
        if action == "projects" {
            return self.projects().await;
        }
        if GIT_CHANNEL_ACTIONS.contains(&action) {
            return self.git_channel(envelope).await;
        }
        if SERVER_COMMAND_ACTIONS.contains(&action) {
            return self.server_command(principal, envelope).await;
        }
        if action == "folders" {
            let path = envelope["payload"].get("cwd").and_then(Value::as_str);
            let path = path.map(str::to_owned);
            let result = studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/multi_server_orchestration",
                move || crate::system_reads::directories(path.as_deref()),
            )
            .await
            .map_err(|_| refused(400, "folder_unavailable", "This directory is unavailable"))?
            .map_err(|_| refused(400, "folder_unavailable", "This directory is unavailable"))?;
            return serde_json::to_value(result)
                .map(ExchangeOutcome::Applied)
                .map_err(|_| {
                    refused(503, "folder_unavailable", "The directory could not be read")
                });
        }
        if matches!(action, "add_location" | "remove_location") {
            return self.project_location(principal, envelope).await;
        }
        if action == "location_info" {
            let path = text(&envelope["payload"], "cwd").to_owned();
            return studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-services/multi_server_orchestration",
                move || crate::projects::peer_location_info(&path),
            )
            .await
            .map_err(|_| {
                refused(
                    400,
                    "location_unavailable",
                    "The project folder is unavailable",
                )
            })?
            .map(ExchangeOutcome::Applied)
            .map_err(|_| {
                refused(
                    400,
                    "location_unavailable",
                    "The project folder is unavailable",
                )
            });
        }
        if action == "location_matches" {
            let payload = &envelope["payload"];
            let origin = text(payload, "origin").to_owned();
            let cwd = payload
                .get("cwd")
                .and_then(Value::as_str)
                .map(str::to_owned);
            return crate::projects::peer_location_matches(&self.store, origin, cwd)
                .await
                .map(ExchangeOutcome::Applied)
                .map_err(|_| {
                    refused(
                        400,
                        "location_unavailable",
                        "Project location matches are unavailable",
                    )
                });
        }
        if action == "input" {
            if envelope["payload"]
                .get(REMOTE_ANCHOR_INPUT_FIELD)
                .and_then(Value::as_bool)
                == Some(true)
            {
                return receive_remote_anchor_input(Arc::clone(&self.store), principal, envelope)
                    .await;
            }
            return receive_remote_input(Arc::clone(&self.store), principal, envelope).await;
        }
        if matches!(action, "message" | "move_send") {
            return self.worker_call(principal, envelope).await;
        }
        if action == EXEC_EVENT_ACTION {
            let key = text(envelope, "requestId").to_owned();
            let payload = envelope["payload"].clone();
            let principal = principal.to_owned();
            let now = crate::event_write::unix_time().map_err(|_| {
                refused(503, "clock_unavailable", "The system clock is unavailable")
            })?;
            return self
                .store
                .transaction_with(move |tx| receive_exec_event(tx, &principal, &payload, key, now))
                .await;
        }
        if action != "event" {
            return Err(refused(
                501,
                "orchestration_effect",
                "The cross-server action is unavailable",
            ));
        }
        let key = text(envelope, "requestId");
        let payload = &envelope["payload"];
        let link_id = text(payload, "link");
        let kind = text(payload, "kind");
        let event_text = text(payload, "text");
        let delivery = delivery_moment(payload)?;
        let parent_epoch = payload
            .get("parentEpoch")
            .and_then(Value::as_i64)
            .ok_or_else(|| refused(400, "invalid_request", "Invalid cross-server event"))?;
        if key.is_empty()
            || link_id.is_empty()
            || event_text.len() > 12000
            || !matches!(
                kind,
                "child_result" | "child_stopped" | "agent_message" | "work_review"
            )
        {
            return Err(refused(
                400,
                "invalid_request",
                "Invalid cross-server event",
            ));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| refused(503, "clock_unavailable", "The system clock is unavailable"))?
            .as_secs_f64();
        let key_for_write = key.to_owned();
        let link_id_for_write = link_id.to_owned();
        let principal_for_write = principal.to_owned();
        let kind_for_write = kind.to_owned();
        let event_text_for_write = event_text.to_owned();
        let result = self
            .store
            .transaction_with(move |tx| {
                let raw: Option<String> = tx
                    .query_row(
                        "SELECT record FROM runtime_server_links WHERE id=?1",
                        [link_id_for_write],
                        |row| row.get(0),
                    )
                    .optional()?;
                let raw = raw.ok_or_else(|| {
                    refused(
                        404,
                        "link_unavailable",
                        "The remote-parent link is unavailable",
                    )
                })?;
                let link: Value = serde_json::from_str(&raw).map_err(|_| {
                    refused(503, "invalid_record", "The remote-parent link is invalid")
                })?;
                if link.get("side").and_then(Value::as_str) != Some("home")
                    || link.get("server").and_then(Value::as_str)
                        != Some(principal_for_write.as_str())
                {
                    return Err(refused(
                        403,
                        "link_owner",
                        "The remote-parent link belongs to another server",
                    ));
                }
                if link.get("parentEpoch").and_then(Value::as_i64) != Some(parent_epoch) {
                    return Ok(ExchangeOutcome::Applied(
                        json!({"stored":false,"reason":"The parent epoch was superseded"}),
                    ));
                }
                let parent = link.get("parent").and_then(Value::as_str).ok_or_else(|| {
                    refused(503, "invalid_record", "The remote-parent link is invalid")
                })?;
                // Match Python `_receive`'s event owner lookup: resolve the parent
                // id from runtime_server_links, then load that durable agent row
                // before enqueueing recovery work (codex_multi_server_orchestration.py:1090-1098).
                let typed = studio_runtime_store::AgentRepository::get(
                    tx,
                    &studio_runtime_store::AgentId::from(parent),
                )?
                .ok_or_else(|| {
                    refused(404, "parent_unavailable", "The parent agent is unavailable")
                })?;
                if typed.epoch != parent_epoch
                    || typed.extra.get("autoWake") != Some(&Value::Bool(true))
                    || studio_runtime_store::truthy(typed.extra.get("deletedAt"))
                {
                    return Ok(ExchangeOutcome::Applied(
                        json!({"stored":false,"reason":"The parent was stopped"}),
                    ));
                }
                if kind_for_write == "child_result" {
                    let details: Value =
                        serde_json::from_str(&event_text_for_write).map_err(|_| {
                            refused(400, "invalid_request", "Invalid child result event")
                        })?;
                    let child_id =
                        details
                            .get("agent_id")
                            .and_then(Value::as_str)
                            .ok_or_else(|| {
                                refused(400, "invalid_request", "Invalid child result event")
                            })?;
                    if !link["workers"].as_array().is_some_and(|workers| {
                        workers
                            .iter()
                            .any(|worker| worker.as_str() == Some(child_id))
                    }) {
                        return Err(refused(
                            403,
                            "child_unlinked",
                            "The result is outside this remote-parent link",
                        ));
                    }
                    let mut proxy = studio_runtime_store::AgentRepository::get(
                        tx,
                        &studio_runtime_store::AgentId::from(child_id),
                    )?
                    .ok_or_else(|| {
                        refused(
                            404,
                            "worker_unavailable",
                            "The result worker is unavailable",
                        )
                    })?;
                    proxy.extra.insert(
                        "lastAnswer".to_owned(),
                        details
                            .get("result")
                            .or_else(|| details.get("reason"))
                            .cloned()
                            .unwrap_or_else(|| json!("")),
                    );
                    proxy
                        .extra
                        .insert("lastCompletedTurn".to_owned(), json!(key_for_write));
                    studio_runtime_store::AgentRepository::put(tx, &proxy)?;
                }
                let existing: Option<(String, String, String, f64, i64)> = tx
                    .query_row(
                        "SELECT agent,kind,text,created,epoch FROM runtime_events WHERE id=?1",
                        [&key_for_write],
                        |row| {
                            Ok((
                                row.get(0)?,
                                row.get(1)?,
                                row.get(2)?,
                                row.get(3)?,
                                row.get(4)?,
                            ))
                        },
                    )
                    .optional()?;
                if let Some((agent, old_kind, old_text, _, epoch)) = existing {
                    if agent != parent
                        || old_kind != kind_for_write
                        || old_text != event_text_for_write
                        || epoch != typed.epoch as i64
                    {
                        return Err(refused(
                            409,
                            "request_conflict",
                            "This event identity has different content",
                        ));
                    }
                } else {
                    studio_runtime_store::QueueWriteRepository::put_metadata(
                        tx,
                        &key_for_write,
                        &json!({"delivery":delivery}),
                    )?;
                    crate::event_write::enqueue_recovery_event(
                        tx,
                        &typed,
                        key_for_write.clone(),
                        &kind_for_write,
                        event_text_for_write,
                        now,
                    )
                    .map_err(|_| {
                        refused(
                            503,
                            "event_unavailable",
                            "The remote event could not be stored",
                        )
                    })?;
                }
                Ok(ExchangeOutcome::Applied(json!({"eventId":key_for_write})))
            })
            .await?;
        Ok(result)
    }
}

// Cross-server command receipts. These names and texts are the wire contract
// between paired servers.

/// The one-shot action that starts a command on a paired server.
pub const EXEC_ACTION: &str = "exec";
/// The effect-free probe that reads the stored receipt of one `exec` request.
pub const EXEC_RECEIPT_ACTION: &str = "exec_receipt";
/// The command-exit notice that the executing server sends to the owner.
pub const EXEC_EVENT_ACTION: &str = "exec_event";
/// First name part of the probe identity: `identity('exec-receipt', handle)`.
const EXEC_RECEIPT_IDENTITY_PART: &str = "exec-receipt";
/// The payload field that replaces an `exec` command in its outbox body after
/// the first delivery attempt. Its value is the receiver's request signature
/// of the original command: SHA-256 hex of `[sender, "exec", payload]`.
const EXEC_RECEIPT_DIGEST_FIELD: &str = "receiptDigest";
/// Recovery event kind that `_exec_event` writes for the owning lead.
const EXEC_EXIT_EVENT_KIND: &str = "monitor_exit";
const EXEC_EXIT_EVENT_DETAIL: &str = "Read output with orchestration_servers action=exec_read.";
const EXEC_OWNER_REFUSAL: &str = "The command handle belongs to another actor or server";
const EXEC_RECEIPT_OWNER_REFUSAL: &str = "The command receipt belongs to another actor or server";
const EXEC_RECEIPT_DIGEST_REFUSAL: &str = "The receipt fingerprint belongs to another request";
const EXPIRED_RECEIPT_DETAIL: &str = "The receipt has expired. This identity cannot execute again.";

/// Python `deliver`: an `exec` row in receipt form never sends its command
/// again. It sends this probe with a stable identity instead.
fn exec_receipt_probe(key: &str, payload: &Value) -> Option<Value> {
    let digest = payload.get(EXEC_RECEIPT_DIGEST_FIELD)?;
    if !studio_runtime_store::truthy(Some(digest)) {
        return None;
    }
    Some(json!({
        "requestId": crate::projects::cross_server_identity(&[EXEC_RECEIPT_IDENTITY_PART, key]),
        "action": EXEC_RECEIPT_ACTION,
        "payload": {
            "handle": key,
            "actor": payload.get("actor").cloned().unwrap_or(Value::Null),
            "digest": digest,
        },
    }))
}

/// Python `_compact_exec`: the outbox body after the first `exec` attempt.
/// It keeps the actor for ownership checks and drops the command itself.
fn exec_receipt_body(envelope: &Envelope, local_server: &str) -> Result<String, AccessError> {
    let digest = envelope.incoming_signature(local_server)?;
    canonical_json(&json!({
        "requestId": envelope.key,
        "action": envelope.action,
        "payload": {
            "actor": envelope.payload.get("actor").cloned().unwrap_or(Value::Null),
            EXEC_RECEIPT_DIGEST_FIELD: digest,
        },
    }))
    .map_err(|_| refused(400, "invalid_request", "Invalid cross-server request"))
}

/// Python compares `stored != payload.get('actor')`: a missing or null actor
/// matches only a NULL column.
fn payload_actor_is(payload: &Value, stored: Option<&str>) -> bool {
    match payload.get("actor") {
        None | Some(Value::Null) => stored.is_none(),
        Some(Value::String(actor)) => stored == Some(actor.as_str()),
        Some(_) => false,
    }
}

/// Python `_exec_owner`: the handle is this server's `exec` outbox row for the
/// same paired server and actor. The receipt form keeps the actor, so the
/// check holds before and after `_compact_exec`.
fn exec_owner(
    tx: &Transaction<'_>,
    server: &str,
    actor: &Value,
    handle: &Value,
) -> Result<bool, AccessError> {
    let Some(handle) = handle.as_str() else {
        return Ok(false);
    };
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT server,body FROM runtime_server_outbox WHERE id=?1",
            [handle],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((saved_server, body)) = row else {
        return Ok(false);
    };
    let envelope: Value = serde_json::from_str(&body).map_err(|_| {
        refused(
            503,
            "invalid_record",
            "Stored cross-server envelope is invalid",
        )
    })?;
    Ok(saved_server == server
        && envelope.get("action").and_then(Value::as_str) == Some(EXEC_ACTION)
        && envelope.pointer("/payload/actor").unwrap_or(&Value::Null) == actor)
}

/// Python `_exec_owner` for the caller side of `exec_read`, `exec_input` and
/// `exec_cancel`: the handle names this lead's `exec` row for that server.
pub(crate) fn exec_handle_owned(
    tx: &Transaction<'_>,
    server: &str,
    actor: &Value,
    handle: &Value,
) -> Result<bool, AccessError> {
    exec_owner(tx, server, actor, handle)
}

/// Python `_exec_event`, with the `_evidence` lookup that answers a replayed
/// claim: an existing event with this identity proves the effect.
fn receive_exec_event(
    tx: &Transaction<'_>,
    principal: &str,
    payload: &Value,
    key: String,
    now: f64,
) -> Result<ExchangeOutcome, AccessError> {
    let invalid = || refused(400, "invalid_request", "Invalid cross-server command event");
    let (Some(actor), Some(handle)) = (payload.get("actor"), payload.get("handle")) else {
        return Err(invalid());
    };
    if !exec_owner(tx, principal, actor, handle)? {
        return Ok(ExchangeOutcome::NotApplied(EXEC_OWNER_REFUSAL.to_owned()));
    }
    let stored: Option<i64> = tx
        .query_row("SELECT 1 FROM runtime_events WHERE id=?1", [&key], |row| {
            row.get(0)
        })
        .optional()?;
    if stored.is_some() {
        return Ok(ExchangeOutcome::Applied(json!({"stored": true})));
    }
    let actor = actor.as_str().ok_or_else(invalid)?;
    let Some(lead) = studio_runtime_store::AgentRepository::get(
        tx,
        &studio_runtime_store::AgentId::from(actor),
    )?
    else {
        return Ok(ExchangeOutcome::Applied(
            json!({"stored": false, "detail": "The lead no longer exists"}),
        ));
    };
    if studio_runtime_store::truthy(lead.extra.get("deletedAt")) {
        return Ok(ExchangeOutcome::Applied(
            json!({"stored": false, "detail": "The lead was deleted"}),
        ));
    }
    let record = payload
        .get("record")
        .and_then(Value::as_object)
        .ok_or_else(invalid)?;
    let status = record.get("status").ok_or_else(invalid)?;
    let field = |name: &str| record.get(name).cloned().unwrap_or(Value::Null);
    let event_text = canonical_json(&json!({
        "id": handle,
        "serverId": principal,
        "status": status,
        "exitCode": field("exitCode"),
        "signal": field("signal"),
        "duration": field("duration"),
        "timedOut": record.get("timedOut").cloned().unwrap_or(Value::Bool(false)),
        "error": field("error"),
        "detail": EXEC_EXIT_EVENT_DETAIL,
    }))
    .map_err(|_| invalid())?;
    crate::event_write::enqueue_recovery_event(
        tx,
        &lead,
        key,
        EXEC_EXIT_EVENT_KIND,
        event_text,
        now,
    )
    .map_err(|_| {
        refused(
            503,
            "event_unavailable",
            "The command event could not be stored",
        )
    })?;
    Ok(ExchangeOutcome::Applied(json!({"stored": true})))
}

/// The `exec_receipt` branch of Python `_receive`. `None` is an unexpected
/// payload (Python raises outside its refusal types), so the probe claim stays
/// running and the reply is unknown. `Some(Err(text))` is a refusal.
fn exec_receipt_answer(
    tx: &Transaction<'_>,
    principal: &str,
    payload: &Value,
    now: i64,
) -> Result<Option<Result<Value, &'static str>>, AccessError> {
    type Original = (String, Option<String>, Option<String>, Option<String>);
    let Some(handle) = payload.get("handle").and_then(Value::as_str) else {
        return Ok(None);
    };
    let unknown = json!({"requestId": handle, "outcome": "unknown"});
    let original: Option<Original> = tx
        .query_row(
            "SELECT signature,result,principal,actor FROM runtime_server_inbox WHERE id=?1",
            [handle],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )
        .optional()?;
    let Some((signature, result, owner, actor)) = original else {
        return Ok(Some(Ok(unknown)));
    };
    if owner.is_none() && actor.is_none() {
        // Receipts from the previous schema still need ownership proof before
        // a new effect-free probe can read them.
        let job: Option<(String, String)> = tx
            .query_row(
                "SELECT owner,actor FROM runtime_server_exec WHERE id=?1",
                [handle],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        if !job.is_some_and(|(owner, actor)| {
            owner == principal && payload_actor_is(payload, Some(&actor))
        }) {
            return Ok(Some(Err(EXEC_RECEIPT_OWNER_REFUSAL)));
        }
    } else if owner.as_deref() != Some(principal) || !payload_actor_is(payload, actor.as_deref()) {
        return Ok(Some(Err(EXEC_RECEIPT_OWNER_REFUSAL)));
    }
    if payload.get("digest").and_then(Value::as_str) != Some(signature.as_str()) {
        return Ok(Some(Err(EXEC_RECEIPT_DIGEST_REFUSAL)));
    }
    let invalid = || refused(503, "invalid_record", "Stored delivery result is invalid");
    if let Some(raw) = result.filter(|raw| !raw.is_empty()) {
        let receipt: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
        // Python `live_receipt`: expired command output reads as unknown.
        let expired = receipt
            .get("value")
            .filter(|value| value.is_object())
            .and_then(|value| value.get("outputExpiresAt"))
            .and_then(Value::as_f64)
            .is_some_and(|at| at <= now as f64);
        return Ok(Some(Ok(if expired {
            json!({
                "requestId": receipt.get("requestId").cloned().unwrap_or(Value::Null),
                "outcome": "unknown",
                "expired": true,
                "detail": EXPIRED_RECEIPT_DETAIL,
            })
        } else {
            receipt
        })));
    }
    // Python `_evidence(db, 'exec', ...)`: a settled job record proves the
    // start when the claim lost its receipt.
    let record: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_server_exec WHERE id=?1",
            [handle],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(raw) = record {
        let record: Value = serde_json::from_str(&raw).map_err(|_| invalid())?;
        let settled = match record.get("status") {
            None => false,
            Some(Value::String(status)) => !matches!(status.as_str(), "starting" | "unknown"),
            Some(_) => true,
        };
        if settled {
            return Ok(Some(Ok(
                json!({"requestId": handle, "outcome": "applied", "value": record}),
            )));
        }
    }
    Ok(Some(Ok(unknown)))
}

/// The saved state of an `exec` outbox row that is already in receipt form.
enum SavedExecReceipt {
    /// Not settled: deliver this probe.
    Pending(Value),
    /// Settled: the saved receipt of the original command.
    Settled(ExchangeOutcome),
}

/// Python `deliver` and `_retry` for an `exec` row: a known command outcome
/// completes the row; an unknown one keeps it in `state` and schedules the
/// next probe.
fn settle_exec_row(
    tx: &Transaction<'_>,
    key: &str,
    state: &str,
    attempts: i64,
    result: &ExchangeOutcome,
    now: i64,
) -> Result<(), AccessError> {
    match result {
        ExchangeOutcome::Applied(_) | ExchangeOutcome::NotApplied(_) => tx.execute(
            "UPDATE runtime_server_outbox SET state='complete',result=?1,error=NULL,completed_at=?2 WHERE id=?3",
            params![result.to_value(key).to_string(), now, key],
        )?,
        ExchangeOutcome::Unknown | ExchangeOutcome::PreEffectWaiting => tx.execute(
            "UPDATE runtime_server_outbox SET state=?1,attempts=attempts+1,next_at=?2,error='unknown' WHERE id=?3",
            params![state, now.saturating_add(peer_retry_delay(attempts)), key],
        )?,
    };
    Ok(())
}

/// Python `compact_receipt('exec_receipt', ...)`: the probe's own receipt does
/// not keep a second copy of the command output.
fn compact_exec_receipt(result: &ExchangeOutcome, key: &str) -> Value {
    match result {
        ExchangeOutcome::Applied(value) => json!({
            "requestId": key,
            "outcome": "applied",
            "value": {
                "requestId": value.get("requestId").cloned().unwrap_or(Value::Null),
                "outcome": value.get("outcome").cloned().unwrap_or(Value::Null),
            },
        }),
        other => other.to_value(key),
    }
}

#[derive(Clone)]
pub struct Envelope {
    key: String,
    action: String,
    payload: Value,
    body: String,
    fingerprint: String,
}
impl Envelope {
    pub fn from_value(value: &Value) -> Result<Self, AccessError> {
        schema("ServerOperationRequest", value)?;
        let body = canonical_json(value)
            .map_err(|_| refused(400, "invalid_request", "Invalid cross-server request"))?;
        if body.len() > MAX_REQUEST_BYTES {
            return Err(refused(
                413,
                "request_size",
                "The remote request exceeds 256 KiB",
            ));
        }
        let action = text(value, "action");
        if matches!(action, "move_validate" | "move_begin")
            && value.get("payload").and_then(|v| v.get("packed")).is_some()
        {
            return Err(refused(
                501,
                "move_payload",
                "Not implemented: requires multi-server-runtime/packed-move-payload",
            ));
        }
        Ok(Self {
            key: text(value, "requestId").to_owned(),
            action: action.to_owned(),
            payload: value["payload"].clone(),
            fingerprint: digest(body.as_bytes()),
            body,
        })
    }
    pub fn value(&self) -> Value {
        json!({"requestId":self.key,"action":self.action,"payload":self.payload})
    }
    fn incoming_signature(&self, principal: &str) -> Result<String, AccessError> {
        Ok(digest(
            canonical_json(&json!([principal, self.action, self.payload]))
                .map_err(|_| refused(400, "invalid_request", "Invalid cross-server request"))?
                .as_bytes(),
        ))
    }
}

/// A checked constructor is the only path from Python-written SQL rows into a
/// delivery decision. The raw result is validated against the envelope identity.
pub struct DeliverySnapshot {
    state: String,
    signature: String,
    server: Option<String>,
    result: Option<ExchangeOutcome>,
    next_at: f64,
}
impl DeliverySnapshot {
    pub fn from_record(
        table: &'static str,
        id: &str,
        raw: &str,
        now: i64,
    ) -> Result<Self, AccessError> {
        let value = stored("PersistedServerDelivery", table, id, raw)?;
        if text(&value, "id") != id {
            return Err(refused(
                503,
                "invalid_record",
                "Stored delivery identity differs from its row",
            ));
        }
        let body = value.get("body").and_then(Value::as_str);
        let signature = value
            .get("signature")
            .and_then(Value::as_str)
            .or_else(|| value.get("fingerprint").and_then(Value::as_str))
            .map(str::to_owned)
            .or_else(|| body.map(|b| digest(b.as_bytes())))
            .ok_or_else(|| refused(503, "invalid_record", "Stored delivery has no identity"))?;
        let result = value
            .get("result")
            .and_then(Value::as_str)
            .map(|s| {
                serde_json::from_str::<Value>(s)
                    .map_err(|_| {
                        refused(503, "invalid_record", "Stored delivery result is invalid")
                    })
                    .and_then(|v| ExchangeOutcome::from_value(&v, id, now))
            })
            .transpose()?;
        if text(&value, "state") == "complete" && result.is_none() {
            return Err(refused(
                503,
                "invalid_record",
                "Completed delivery has no receipt",
            ));
        }
        Ok(Self {
            state: text(&value, "state").to_owned(),
            signature,
            server: value
                .get("server")
                .and_then(Value::as_str)
                .map(str::to_owned),
            result,
            next_at: value.get("next_at").and_then(Value::as_f64).unwrap_or(0.0),
        })
    }
}
pub enum DeliveryDecision {
    Execute,
    Return(ExchangeOutcome),
}
const PEER_REPLAY_SAFE_ACTIONS: &[&str] = &[
    "event",
    "input",
    "message",
    "move_send",
    "location_info",
    "location_matches",
    "stop",
    // The event row identity is the request identity, so a replay finds it.
    EXEC_EVENT_ACTION,
    // Effect-free reads (Python `receive` runs them again for a running claim).
    "projects",
    "folders",
    "git",
    "chunk",
    "exec_read",
    // Removing a released bundle again has no further effect.
    "release",
];

/// Python `receive` and `deliver` never answer a bundle chunk from its saved
/// receipt: that receipt keeps no data, so each delivery reads the chunk again.
const REREAD_ACTIONS: &[&str] = &["chunk"];

/// Python `compact_receipt`: a stored chunk receipt keeps no chunk data.
fn compact_receipt(action: &str, result: &ExchangeOutcome, key: &str) -> Value {
    let mut value = result.to_value(key);
    if REREAD_ACTIONS.contains(&action)
        && let Some(applied) = value.get_mut("value").and_then(Value::as_object_mut)
    {
        applied.remove("data");
    }
    value
}

/// Worker calls that a remote worker sends to its home server (Python
/// `worker_call`). The home server binds each request identity to a durable
/// inbox claim, so the sender delivers the same envelope again after a lost
/// reply, as Python `_retry` does. The receiver never runs a claimed write
/// twice, so these actions are not replay-safe on the receiving side.
pub(crate) const WORKER_CALL_RETRY_ACTIONS: &[&str] = &[
    "task",
    "complaint",
    "context",
    "directory",
    "chat_read",
    "move_manage",
    "move_interrupt",
    "move_spawn",
];

/// The sender may deliver this queued or crashed outbox envelope again.
fn outgoing_retry(action: &str) -> bool {
    peer_replay_safe(action) || WORKER_CALL_RETRY_ACTIONS.contains(&action)
}

fn outgoing_retry_sql_values() -> String {
    PEER_REPLAY_SAFE_ACTIONS
        .iter()
        .chain(WORKER_CALL_RETRY_ACTIONS)
        .map(|action| format!("'{action}'"))
        .collect::<Vec<_>>()
        .join(",")
}

fn peer_replay_safe(action: &str) -> bool {
    // Every input variant binds its stable request identity to a durable
    // message receipt or event row before reporting an applied effect.
    PEER_REPLAY_SAFE_ACTIONS.contains(&action)
}

fn peer_retry_delay(attempts: i64) -> i64 {
    (5_u64
        .saturating_mul(1_u64 << attempts.clamp(0, 6) as u32)
        .min(300)) as i64
}

pub fn decide_delivery(
    snapshot: Option<&DeliverySnapshot>,
    signature: &str,
    server: Option<&str>,
    outgoing: bool,
    action: &str,
    now: i64,
) -> Result<DeliveryDecision, AccessError> {
    let Some(snapshot) = snapshot else {
        return Ok(DeliveryDecision::Execute);
    };
    if snapshot.signature != signature || snapshot.server.as_deref() != server {
        return Err(refused(
            409,
            "request_conflict",
            "This request id has different content",
        ));
    }
    if snapshot.state == "complete" && REREAD_ACTIONS.contains(&action) {
        return Ok(DeliveryDecision::Execute);
    }
    if snapshot.state == "complete" {
        return Ok(DeliveryDecision::Return(
            snapshot.result.clone().ok_or_else(|| {
                refused(503, "invalid_record", "Completed delivery has no receipt")
            })?,
        ));
    }
    // A running input claim is ambiguous: only an exact durable-effect lookup
    // may resolve it. The generic effect adapter cannot prove that lookup, so
    // leave the receipt unknown instead of executing the input again.
    if !outgoing
        && (snapshot.state == "waiting"
            || (peer_replay_safe(action) && action != "input" && snapshot.state == "running"))
    {
        return Ok(DeliveryDecision::Execute);
    }
    if outgoing
        && outgoing_retry(action)
        && matches!(snapshot.state.as_str(), "queued" | "running")
        && snapshot.next_at <= now as f64
    {
        return Ok(DeliveryDecision::Execute);
    }
    Ok(DeliveryDecision::Return(ExchangeOutcome::Unknown))
}

pub struct OrchestrationDriver {
    store: Arc<RuntimeStore>,
    transport: Arc<dyn PairedTransport>,
    effects: Arc<dyn OrchestrationEffects>,
}
const MAX_OUTBOX_BATCH: usize = 32;
pub(crate) const REMOTE_ANCHOR_INPUT_FIELD: &str = "anchor";

/// Persist a peer-delivery intent in the caller's owning transaction. The
/// caller sends it only after that transaction commits.
pub fn enqueue_in_transaction(
    transaction: &Transaction<'_>,
    server: &str,
    action: &str,
    payload: &Value,
    request_id: &str,
    now: i64,
) -> Result<(), AccessError> {
    let envelope =
        Envelope::from_value(&json!({"requestId":request_id,"action":action,"payload":payload}))?;
    require_paired(transaction, server)?;
    let prior = transaction
        .query_row(
            "SELECT server,body,fingerprint FROM runtime_server_outbox WHERE id=?1",
            [request_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            },
        )
        .optional()?;
    if let Some((saved_server, saved_body, saved_fingerprint)) = prior {
        let fingerprint = saved_fingerprint.unwrap_or_else(|| digest(saved_body.as_bytes()));
        if saved_server != server || fingerprint != envelope.fingerprint {
            return Err(refused(
                409,
                "request_conflict",
                "This request id has different content",
            ));
        }
        return Ok(());
    }
    transaction.execute(
        "INSERT INTO runtime_server_outbox(id,server,body,state,created_at,next_at,fingerprint,attempts) VALUES(?1,?2,?3,'queued',?4,0,?5,0)",
        params![request_id, server, envelope.body, now, envelope.fingerprint],
    )?;
    Ok(())
}

impl OrchestrationDriver {
    pub fn new(
        store: Arc<RuntimeStore>,
        transport: Arc<dyn PairedTransport>,
        effects: Arc<dyn OrchestrationEffects>,
    ) -> Self {
        Self {
            store,
            transport,
            effects,
        }
    }
    pub async fn exchange(
        &self,
        server: &str,
        action: &str,
        payload: &Value,
        request_id: &str,
        now: i64,
    ) -> Result<ExchangeOutcome, AccessError> {
        self.exchange_with_timeout(
            server,
            action,
            payload,
            request_id,
            now,
            std::time::Duration::from_secs(20),
        )
        .await
    }

    pub async fn exchange_with_timeout(
        &self,
        server: &str,
        action: &str,
        payload: &Value,
        request_id: &str,
        now: i64,
        timeout: std::time::Duration,
    ) -> Result<ExchangeOutcome, AccessError> {
        let store_for_local = Arc::clone(&self.store);
        let local_server = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/multi_server_orchestration",
            move || store_for_local.local_server_id(),
        )
        .await
        .map_err(|_| {
            refused(
                503,
                "identity_unavailable",
                "The local server identity is unavailable",
            )
        })??
        .unwrap_or_default();
        let is_local = !local_server.is_empty() && server == local_server;
        let envelope = Envelope::from_value(
            &json!({"requestId":request_id,"action":action,"payload":payload}),
        )?;
        if envelope.action == EXEC_ACTION {
            match self.saved_exec_receipt(server, &envelope, now).await? {
                Some(SavedExecReceipt::Settled(result)) => return Ok(result),
                Some(SavedExecReceipt::Pending(probe)) => {
                    return self
                        .deliver_exec_receipt(
                            server,
                            is_local.then_some(local_server.as_str()),
                            &envelope.key,
                            &probe,
                            timeout,
                            now,
                        )
                        .await;
                }
                None if local_server.is_empty() => {
                    // The receipt digest names this server as the sender.
                    return Err(refused(
                        503,
                        "identity_unavailable",
                        "The local server identity is unavailable",
                    ));
                }
                None => {}
            }
        }
        let envelope_for_claim = envelope.clone();
        let server_for_claim = server.to_owned();
        let in_flight_until = now.saturating_add(timeout.as_secs().max(1) as i64);
        let decision=self.store.transaction_with(move |tx| {
            if !is_local { require_paired(tx,&server_for_claim)?; }
            if retired(tx,&envelope_for_claim.key,"out",&envelope_for_claim.fingerprint,Some(&server_for_claim))? {return Ok(DeliveryDecision::Return(ExchangeOutcome::Unknown));}
            let saved=outbox(tx,&envelope_for_claim.key,now)?;
            let decision=decide_delivery(saved.as_ref(),&envelope_for_claim.fingerprint,Some(&server_for_claim),true,&envelope_for_claim.action,now)?;
            if matches!(decision,DeliveryDecision::Execute) {
                // An in-flight claim is not due: the pump replays a running peer
                // operation only after its exchange deadline has passed.
                tx.execute("INSERT INTO runtime_server_outbox(id,server,body,state,created_at,fingerprint,attempts,next_at) VALUES(?1,?2,?3,'running',?4,?5,0,?6) ON CONFLICT(id) DO UPDATE SET state='running',next_at=?6",params![envelope_for_claim.key,server_for_claim,envelope_for_claim.body,now,envelope_for_claim.fingerprint,in_flight_until])?;
            }
            Ok::<_,AccessError>(decision)
        }).await?;
        if let DeliveryDecision::Return(result) = decision {
            return Ok(result);
        }
        // The claim is committed before crossing the transport boundary.
        let exchange = if is_local {
            self.receive(&local_server, &envelope.value(), now).await
        } else {
            self.transport
                .exchange(server, &envelope.value(), timeout)
                .await
                .and_then(|value| ExchangeOutcome::from_value(&value, &envelope.key, now))
        };
        let result = match exchange {
            Ok(value) => value,
            Err(_) => ExchangeOutcome::Unknown,
        };
        if envelope.action == EXEC_ACTION {
            return self
                .finish_exec(server, &envelope, &local_server, result, now)
                .await;
        }
        self.finish(
            "outbox",
            &envelope.key,
            &envelope.action,
            &envelope.fingerprint,
            Some(server),
            &result,
            now,
        )
        .await?;
        Ok(result)
    }

    /// Python `deliver` for an `exec` row: a row already in receipt form only
    /// sends the probe; a completed row returns its saved receipt. The caller
    /// must present the original command (its fingerprint) or the exact saved
    /// receipt body.
    async fn saved_exec_receipt(
        &self,
        server: &str,
        envelope: &Envelope,
        now: i64,
    ) -> Result<Option<SavedExecReceipt>, AccessError> {
        let key = envelope.key.clone();
        type Row = (String, String, Option<String>, String, Option<String>);
        let row: Option<Row> = self.store
        .read_async("runtime-services/multi_server_orchestration", move |tx| {
                tx.query_row(
                    "SELECT server,body,fingerprint,state,result FROM runtime_server_outbox WHERE id=?1",
                    [key],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
                )
                .optional()
                .map_err(StoreError::from)
            })
        .await?;
        let Some((saved_server, body, fingerprint, state, result)) = row else {
            return Ok(None);
        };
        let saved: Value = serde_json::from_str(&body).map_err(|_| {
            refused(
                503,
                "invalid_record",
                "Stored cross-server envelope is invalid",
            )
        })?;
        let Some(probe) = exec_receipt_probe(&envelope.key, &saved["payload"]) else {
            return Ok(None);
        };
        if saved_server != server
            || (body != envelope.body && fingerprint.as_deref() != Some(&envelope.fingerprint))
        {
            return Err(refused(
                409,
                "request_conflict",
                "This request id has different content",
            ));
        }
        if state != "complete" {
            return Ok(Some(SavedExecReceipt::Pending(probe)));
        }
        let receipt = result
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
            .ok_or_else(|| refused(503, "invalid_record", "Completed delivery has no receipt"))?;
        ExchangeOutcome::from_value(&receipt, &envelope.key, now)
            .map(|result| Some(SavedExecReceipt::Settled(result)))
    }

    /// Send the effect-free `exec_receipt` probe for one `exec` row in receipt
    /// form. An applied probe carries the original receipt; any other reply
    /// keeps the command outcome unknown and schedules another probe.
    async fn deliver_exec_receipt(
        &self,
        server: &str,
        local_server: Option<&str>,
        key: &str,
        probe: &Value,
        timeout: std::time::Duration,
        now: i64,
    ) -> Result<ExchangeOutcome, AccessError> {
        let probe_key = text(probe, "requestId");
        let reply = match local_server {
            Some(local_server) => self.receive(local_server, probe, now).await,
            None => self
                .transport
                .exchange(server, probe, timeout)
                .await
                .and_then(|value| ExchangeOutcome::from_value(&value, probe_key, now)),
        };
        let result = match reply {
            Ok(ExchangeOutcome::Applied(receipt)) => {
                ExchangeOutcome::from_value(&receipt, key, now).unwrap_or(ExchangeOutcome::Unknown)
            }
            _ => ExchangeOutcome::Unknown,
        };
        let key = key.to_owned();
        self.store
            .transaction_with(move |tx| {
                let row: Option<(String, Option<String>, i64)> = tx
                    .query_row(
                        "SELECT state,result,attempts FROM runtime_server_outbox WHERE id=?1",
                        [&key],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
                    )
                    .optional()?;
                let (state, saved, attempts) = row.ok_or_else(|| {
                    refused(409, "receipt_missing", "The delivery claim is missing")
                })?;
                if state == "complete" {
                    // A concurrent probe already settled this command.
                    let saved = saved
                        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
                        .ok_or_else(|| {
                            refused(503, "invalid_record", "Completed delivery has no receipt")
                        })?;
                    return ExchangeOutcome::from_value(&saved, &key, now);
                }
                settle_exec_row(tx, &key, &state, attempts, &result, now)?;
                Ok(result)
            })
            .await
    }

    /// Python `_compact_exec` after the first `exec` attempt, whatever its
    /// result: the body drops the command and keeps the original fingerprint,
    /// so a retry can only probe for the receipt and never runs it again.
    async fn finish_exec(
        &self,
        server: &str,
        envelope: &Envelope,
        local_server: &str,
        result: ExchangeOutcome,
        now: i64,
    ) -> Result<ExchangeOutcome, AccessError> {
        let body = exec_receipt_body(envelope, local_server)?;
        let key = envelope.key.clone();
        let fingerprint = envelope.fingerprint.clone();
        let server = server.to_owned();
        self.store
            .transaction_with(move |tx| {
                let snapshot = outbox(tx, &key, now)?.ok_or_else(|| {
                    refused(409, "receipt_missing", "The delivery claim is missing")
                })?;
                if snapshot.signature != fingerprint
                    || snapshot.server.as_deref() != Some(server.as_str())
                    || snapshot.state != "running"
                {
                    return Err(refused(
                        409,
                        "stale_receipt",
                        "The delivery receipt does not match its claim",
                    ));
                }
                tx.execute(
                    "UPDATE runtime_server_outbox SET body=?1,fingerprint=COALESCE(fingerprint,?2) WHERE id=?3",
                    params![body, fingerprint, key],
                )?;
                let attempts: i64 = tx.query_row(
                    "SELECT attempts FROM runtime_server_outbox WHERE id=?1",
                    [&key],
                    |row| row.get(0),
                )?;
                // Python keeps an unsettled command row queued for the probe.
                settle_exec_row(tx, &key, "queued", attempts, &result, now)?;
                Ok(result)
            })
            .await
    }
    pub async fn receive(
        &self,
        principal: &str,
        value: &Value,
        now: i64,
    ) -> Result<ExchangeOutcome, AccessError> {
        let store_for_local = Arc::clone(&self.store);
        let local_server = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/multi_server_orchestration",
            move || store_for_local.local_server_id(),
        )
        .await
        .map_err(|_| {
            refused(
                503,
                "identity_unavailable",
                "The local server identity is unavailable",
            )
        })??
        .unwrap_or_default();
        let is_local = !local_server.is_empty() && principal == local_server;
        let envelope = Envelope::from_value(value)?;
        if envelope.action == EXEC_RECEIPT_ACTION {
            return self
                .receive_exec_receipt(principal, is_local, envelope, now)
                .await;
        }
        if !self.effects.supports(&envelope.action) {
            return Err(refused(
                501,
                "orchestration_effect",
                "Not implemented: requires multi-server-runtime/orchestration-effects",
            ));
        }
        let signature = envelope.incoming_signature(principal)?;
        let envelope_for_claim = envelope.clone();
        let signature_for_claim = signature.clone();
        let principal_for_claim = principal.to_owned();
        let (decision, replaying_claim) = self.store.transaction_with(move |tx| {
            if !is_local {
                require_paired(tx, &principal_for_claim)?;
            }
            if retired(tx, &envelope_for_claim.key, "in", &signature_for_claim, None)? {
                return Ok((DeliveryDecision::Return(ExchangeOutcome::Unknown), false));
            }
            let saved = inbox(tx, &envelope_for_claim.key, now)?;
            let replaying_claim = saved.is_some();
            let decision = decide_delivery(
                saved.as_ref(),
                &signature_for_claim,
                None,
                false,
                &envelope_for_claim.action,
                now,
            )?;
            if matches!(decision, DeliveryDecision::Return(ExchangeOutcome::Unknown))
                && saved.as_ref().is_some_and(|snapshot| {
                    snapshot.state == "running"
                        && snapshot.signature == signature_for_claim
                        && snapshot.server.is_none()
                })
                && let Some(receipt) = durable_anchor_input_receipt(tx, &principal_for_claim, &envelope_for_claim)?
            {
                let result = ExchangeOutcome::Applied(receipt);
                let stored_result = result.to_value(&envelope_for_claim.key).to_string();
                if stored_result.len() > MAX_RESPONSE_BYTES {
                    return Ok((
                        DeliveryDecision::Return(ExchangeOutcome::Unknown),
                        replaying_claim,
                    ));
                }
                let updated = tx.execute(
                    "UPDATE runtime_server_inbox SET state='complete',result=?1,completed_at=?2 WHERE id=?3 AND signature=?4 AND state='running' AND principal=?5",
                    params![stored_result, now, envelope_for_claim.key, signature_for_claim, principal_for_claim],
                )?;
                if updated != 1 {
                    return Err(refused(
                        409,
                        "stale_receipt",
                        "The delivery receipt does not match its claim",
                    ));
                }
                return Ok((DeliveryDecision::Return(result), replaying_claim));
            }
            if matches!(decision, DeliveryDecision::Execute) {
                if saved.is_some() {
                    tx.execute(
                        "UPDATE runtime_server_inbox SET state='running' WHERE id=?1 AND (state IN ('waiting','running') OR (state='complete' AND ?2))",
                        params![&envelope_for_claim.key, REREAD_ACTIONS.contains(&envelope_for_claim.action.as_str())],
                    )?;
                } else {
                    tx.execute(
                        "INSERT INTO runtime_server_inbox(id,signature,state,result,created_at,principal,actor) VALUES(?1,?2,'running',NULL,?3,?4,?5)",
                        params![envelope_for_claim.key, signature_for_claim, now, principal_for_claim, envelope_for_claim.payload.get("actor").and_then(Value::as_str)],
                    )?;
                }
            }
            Ok::<_, AccessError>((decision, replaying_claim))
        }).await?;
        if let DeliveryDecision::Return(result) = decision {
            return Ok(result);
        }
        let mut result = match self.effects.execute(principal, &envelope.value()).await {
            Ok(result) => result,
            Err(error) if envelope.action == "browser_callback" && error.status() < 500 => {
                ExchangeOutcome::NotApplied(error.to_string())
            }
            Err(error)
                if envelope.action == "input" && !replaying_claim && error.status() < 500 =>
            {
                // Input refusals are validated before the durable write; keep
                // fresh permanent 4xx failures from retrying forever as unknown.
                ExchangeOutcome::NotApplied(error.to_string())
            }
            Err(_) => ExchangeOutcome::Unknown,
        };
        if envelope.action == "input"
            && replaying_claim
            && matches!(result, ExchangeOutcome::NotApplied(_))
        {
            // A prior claim may already have committed the durable input. Do
            // not turn a later 4xx (or stale-epoch refusal) into a false
            // negative receipt after response loss.
            result = ExchangeOutcome::Unknown;
        }
        if result.to_value(&envelope.key).to_string().len() > MAX_RESPONSE_BYTES {
            self.finish(
                "inbox",
                &envelope.key,
                &envelope.action,
                &signature,
                None,
                &ExchangeOutcome::Unknown,
                now,
            )
            .await?;
            return Ok(ExchangeOutcome::Unknown);
        }
        self.finish(
            "inbox",
            &envelope.key,
            &envelope.action,
            &signature,
            None,
            &result,
            now,
        )
        .await?;
        Ok(result)
    }

    /// Python `receive` for the effect-free `exec_receipt` probe. Unlike other
    /// claims, a completed probe receipt is not replayed: each probe reads the
    /// original command receipt again, so a later settlement becomes visible.
    async fn receive_exec_receipt(
        &self,
        principal: &str,
        is_local: bool,
        envelope: Envelope,
        now: i64,
    ) -> Result<ExchangeOutcome, AccessError> {
        let signature = envelope.incoming_signature(principal)?;
        let principal = principal.to_owned();
        self.store
            .transaction_with(move |tx| {
                if !is_local {
                    require_paired(tx, &principal)?;
                }
                if retired(tx, &envelope.key, "in", &signature, None)? {
                    return Ok(ExchangeOutcome::Unknown);
                }
                let saved: Option<String> = tx
                    .query_row(
                        "SELECT signature FROM runtime_server_inbox WHERE id=?1",
                        [&envelope.key],
                        |row| row.get(0),
                    )
                    .optional()?;
                if saved.is_some_and(|saved| saved != signature) {
                    return Err(refused(
                        409,
                        "request_conflict",
                        "This request id has different content",
                    ));
                }
                tx.execute(
                    "INSERT OR IGNORE INTO runtime_server_inbox(id,signature,state,result,created_at,principal,actor) VALUES(?1,?2,'running',NULL,?3,?4,?5)",
                    params![envelope.key, signature, now, principal, envelope.payload.get("actor").and_then(Value::as_str)],
                )?;
                let Some(answer) = exec_receipt_answer(tx, &principal, &envelope.payload, now)?
                else {
                    return Ok(ExchangeOutcome::Unknown);
                };
                let result = match answer {
                    Ok(receipt) => ExchangeOutcome::Applied(receipt),
                    Err(refusal) => ExchangeOutcome::NotApplied(refusal.to_owned()),
                };
                if result.to_value(&envelope.key).to_string().len() > MAX_RESPONSE_BYTES {
                    return Ok(ExchangeOutcome::Unknown);
                }
                tx.execute(
                    "UPDATE runtime_server_inbox SET state='complete',result=?1,completed_at=?2 WHERE id=?3",
                    params![compact_exec_receipt(&result, &envelope.key).to_string(), now, envelope.key],
                )?;
                Ok(result)
            })
            .await
    }

    /// Deliver a bounded snapshot of due intents. Each exchange claims and
    /// records its result in short transactions around the transport call.
    pub async fn pump_due(&self, now: i64) -> Result<usize, AccessError> {
        let pending = self
            .store
            .read_async("runtime-services/multi_server_orchestration", move |tx| {
                let sql = format!(
                    "SELECT server,body,state FROM runtime_server_outbox \
                 WHERE next_at<=?1 AND (state='queued' OR (state='running' \
                   AND json_extract(body,'$.action') IN ({}))) \
                 ORDER BY rowid LIMIT ?2",
                    outgoing_retry_sql_values()
                );
                let mut statement = tx.prepare(&sql)?;
                let rows = statement.query_map(params![now, MAX_OUTBOX_BATCH as i64], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?;
                rows.collect::<Result<Vec<_>, _>>()
                    .map_err(StoreError::from)
            })
            .await?;
        let mut count = 0;
        for (server, body, state) in pending {
            let value = serde_json::from_str::<Value>(&body).map_err(|_| {
                refused(
                    503,
                    "invalid_record",
                    "Stored cross-server envelope is invalid",
                )
            })?;
            let envelope = Envelope::from_value(&value)?;
            // A crash after claiming leaves a row running. Stable-identity
            // peer operations can safely replay; provider commands remain
            // one-shot and deliberately stay unknown after that crash point.
            if state == "running" && !outgoing_retry(&envelope.action) {
                continue;
            }
            // One row that cannot be delivered must not hold back the rows after it
            // (Python delivers each queued row on its own).
            match self
                .exchange(
                    &server,
                    &envelope.action,
                    &envelope.payload,
                    &envelope.key,
                    now,
                )
                .await
            {
                Ok(_) => count += 1,
                Err(error) => {
                    eprintln!(
                        "orchestration delivery of {} ({}) failed: {error}",
                        envelope.key, envelope.action
                    );
                }
            }
        }
        Ok(count)
    }

    /// Return the next persisted delivery deadline without scanning on a timer.
    /// Unsafe operations stranded in `running` remain unknown and are excluded;
    /// peer operations can replay with their existing durable identity.
    pub async fn next_due_at(&self) -> Result<Option<i64>, AccessError> {
        Ok(self
            .store
            .read_async("runtime-services/multi_server_orchestration", |tx| {
                outbox_next_due_at(tx)
            })
            .await?)
    }
    #[allow(clippy::too_many_arguments)]
    async fn finish(
        &self,
        table: &str,
        key: &str,
        action: &str,
        signature: &str,
        server: Option<&str>,
        result: &ExchangeOutcome,
        now: i64,
    ) -> Result<(), AccessError> {
        if matches!(
            result,
            ExchangeOutcome::Unknown | ExchangeOutcome::PreEffectWaiting
        ) {
            if outgoing_retry(action) && table == "outbox" {
                let key_for_read = key.to_owned();
                let attempts = self
                    .store
                    .read_async("runtime-services/multi_server_orchestration", move |tx| {
                        tx.query_row(
                            "SELECT attempts FROM runtime_server_outbox WHERE id=?1",
                            [key_for_read],
                            |row| row.get::<_, i64>(0),
                        )
                        .optional()
                        .map(|value| value.unwrap_or_default().max(0) as u64)
                        .map_err(StoreError::from)
                    })
                    .await?;
                let retry_delay = peer_retry_delay(attempts as i64);
                let key_for_update = key.to_owned();
                self.store.transaction_with(move |tx| {
                    tx.execute("UPDATE runtime_server_outbox SET state='queued',attempts=attempts+1,next_at=?1,error='unknown' WHERE id=?2 AND state='running'",params![now.saturating_add(retry_delay),key_for_update])?;
                    Ok::<_,AccessError>(())
                }).await?;
            } else if *result == ExchangeOutcome::PreEffectWaiting
                && action == "input"
                && table == "inbox"
            {
                let key_for_update = key.to_owned();
                self.store.transaction_with(move |tx| {
                    tx.execute("UPDATE runtime_server_inbox SET state='waiting' WHERE id=?1 AND state='running'", [key_for_update])?;
                    Ok::<_, AccessError>(())
                }).await?;
            // A generic unknown input can follow a committed message/event.
            // Keep its claim running; only the typed pre-effect result above
            // proves that retrying the input is safe.
            } else if peer_replay_safe(action) && action != "input" && table == "inbox" {
                let key_for_update = key.to_owned();
                self.store.transaction_with(move |tx| {
                    tx.execute("UPDATE runtime_server_inbox SET state='waiting' WHERE id=?1 AND state='running'", [key_for_update])?;
                    Ok::<_,AccessError>(())
                }).await?;
            }
            return Ok(());
        }
        let table = table.to_owned();
        let key = key.to_owned();
        let action = action.to_owned();
        let signature = signature.to_owned();
        let server = server.map(str::to_owned);
        let result = result.clone();
        self.store.transaction_with(move |tx| {
            let saved=if table=="outbox"{outbox(tx,&key,now)?}else{inbox(tx,&key,now)?};
            let snapshot=saved.ok_or_else(||refused(409,"receipt_missing","The delivery claim is missing"))?;
            if snapshot.signature!=signature || snapshot.server.as_deref()!=server.as_deref() || snapshot.state!="running" { return Err(refused(409,"stale_receipt","The delivery receipt does not match its claim")); }
            tx.execute(&format!("UPDATE runtime_server_{table} SET state='complete',result=?1,completed_at=?2 WHERE id=?3"),params![compact_receipt(&action,&result,&key).to_string(),now,key])?;
            Ok(())
        }).await
    }
}
fn require_paired(tx: &Transaction<'_>, id: &str) -> Result<(), AccessError> {
    let raw: Option<String> = tx
        .query_row(
            "SELECT record FROM runtime_access_clients WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()?;
    let value = raw
        .map(|s| stored("PersistedAccessClient", "runtime_access_clients", id, &s))
        .transpose()?
        .ok_or_else(|| refused(403, "client_revoked", "The server is not paired"))?;
    if text(&value, "id") != id
        || text(&value, "status") != "paired"
        || text(&value, "kind") != "server"
    {
        return Err(refused(403, "client_revoked", "The server is not paired"));
    }
    Ok(())
}
fn outbox(
    tx: &Transaction<'_>,
    key: &str,
    now: i64,
) -> Result<Option<DeliverySnapshot>, AccessError> {
    let raw:Option<String>=tx.query_row("SELECT json_object('id',id,'server',server,'body',body,'state',state,'result',result,'fingerprint',fingerprint,'attempts',attempts,'next_at',next_at,'created_at',created_at,'completed_at',completed_at) FROM runtime_server_outbox WHERE id=?1",[key],|r|r.get(0)).optional()?;
    raw.map(|s| DeliverySnapshot::from_record("runtime_server_outbox", key, &s, now))
        .transpose()
}
fn inbox(
    tx: &Transaction<'_>,
    key: &str,
    now: i64,
) -> Result<Option<DeliverySnapshot>, AccessError> {
    let raw:Option<String>=tx.query_row("SELECT json_object('id',id,'signature',signature,'state',state,'result',result,'next_at',0,'created_at',created_at,'completed_at',completed_at) FROM runtime_server_inbox WHERE id=?1",[key],|r|r.get(0)).optional()?;
    raw.map(|s| DeliverySnapshot::from_record("runtime_server_inbox", key, &s, now))
        .transpose()
}
fn retired(
    tx: &Transaction<'_>,
    key: &str,
    direction: &str,
    signature: &str,
    server: Option<&str>,
) -> Result<bool, AccessError> {
    let row = tx
        .query_row(
            "SELECT signature,server FROM runtime_server_retired WHERE id=?1 AND direction=?2",
            params![key, direction],
            |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
        )
        .optional()?;
    if let Some((stored_signature, stored_server)) = row {
        if stored_signature != signature || stored_server.as_deref() != server {
            return Err(refused(
                409,
                "request_conflict",
                "This request id has different content",
            ));
        }
        return Ok(true);
    }
    Ok(false)
}

fn outbox_next_due_at(
    connection: &rusqlite::Connection,
) -> studio_runtime_store::StoreResult<Option<i64>> {
    let sql = format!(
        "SELECT MIN(next_at) FROM runtime_server_outbox \
         WHERE state='queued' OR (state='running' AND \
           json_extract(body,'$.action') IN ({}))",
        outgoing_retry_sql_values()
    );
    // Python stores next_at as float seconds. Round up so the scheduler never wakes
    // before the row is due and spins until the next whole second.
    connection
        .query_row(&sql, [], |row| row.get::<_, Option<f64>>(0))
        .map(|at| at.map(|at| at.ceil() as i64))
        .map_err(Into::into)
}

/// Normalize a paired peer's wire value once before service admission.
fn delivery_moment(payload: &Value) -> Result<DeliveryMoment, AccessError> {
    DeliveryMoment::for_event(text(payload, "kind"), text(payload, "text"), Some(payload))
        .map_err(|_| refused(400, "invalid_request", "Invalid delivery moment"))
}

#[cfg(test)]
mod tests {
    use super::{
        DeliveryDecision, DeliverySnapshot, ExchangeOutcome, HttpsPairedTransport, decide_delivery,
        outbox_next_due_at, peer_retry_delay,
    };
    use rusqlite::params;
    use std::sync::Arc;
    use studio_runtime_store::RuntimeStore;

    fn saved(state: &str, next_at: f64) -> DeliverySnapshot {
        DeliverySnapshot {
            state: state.to_owned(),
            signature: "same-content".to_owned(),
            server: Some("peer".to_owned()),
            result: None,
            next_at,
        }
    }

    #[test]
    fn idempotent_peer_delivery_replays_after_unknown_only_when_due() {
        let queued = saved("queued", 105.0);
        assert!(matches!(
            decide_delivery(
                Some(&queued),
                "same-content",
                Some("peer"),
                true,
                "event",
                104
            ),
            Ok(DeliveryDecision::Return(ExchangeOutcome::Unknown))
        ));
        assert!(matches!(
            decide_delivery(
                Some(&queued),
                "same-content",
                Some("peer"),
                true,
                "event",
                105
            ),
            Ok(DeliveryDecision::Execute)
        ));
        let running = saved("running", 100.0);
        assert!(matches!(
            decide_delivery(
                Some(&running),
                "same-content",
                Some("peer"),
                true,
                "move_send",
                100
            ),
            Ok(DeliveryDecision::Execute)
        ));
    }

    #[test]
    fn one_shot_action_and_changed_identity_content_are_never_replayed() {
        let running = saved("running", 0.0);
        assert!(matches!(
            decide_delivery(
                Some(&running),
                "same-content",
                Some("peer"),
                true,
                "exec",
                200
            ),
            Ok(DeliveryDecision::Return(ExchangeOutcome::Unknown))
        ));
        assert!(
            decide_delivery(
                Some(&running),
                "changed-content",
                Some("peer"),
                true,
                "event",
                200,
            )
            .is_err()
        );
    }

    #[test]
    fn running_peer_input_remains_unknown_without_durable_effect_evidence() {
        let mut running = saved("running", 0.0);
        running.server = None;
        assert!(matches!(
            decide_delivery(Some(&running), "same-content", None, false, "input", 200),
            Ok(DeliveryDecision::Return(ExchangeOutcome::Unknown))
        ));
    }

    #[test]
    fn peer_retry_schedule_matches_python_and_caps_at_five_minutes() {
        assert_eq!(
            [0, 1, 2, 3, 4, 5, 6, 7].map(peer_retry_delay),
            [5, 10, 20, 40, 80, 160, 300, 300]
        );
        assert_eq!(peer_retry_delay(-1), 5);
    }

    #[test]
    fn next_due_excludes_unknown_one_shot_running_rows() {
        let connection = rusqlite::Connection::open_in_memory().expect("memory database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_server_outbox(server TEXT,body TEXT,state TEXT,next_at INTEGER);",
            )
            .expect("outbox table");
        for (action, state, due) in [
            ("exec", "running", 1),
            ("input", "running", 9),
            ("message", "queued", 7),
            ("event", "completed", 0),
        ] {
            connection
                .execute(
                    "INSERT INTO runtime_server_outbox(server,body,state,next_at) VALUES('peer',?1,?2,?3)",
                    params![serde_json::json!({"action":action}).to_string(), state, due],
                )
                .expect("outbox row");
        }
        assert_eq!(
            outbox_next_due_at(&connection).expect("deadline query"),
            Some(7)
        );
    }

    #[test]
    fn next_due_reads_python_float_deadlines() {
        let connection = rusqlite::Connection::open_in_memory().expect("memory database");
        connection
            .execute_batch(
                "CREATE TABLE runtime_server_outbox(server TEXT,body TEXT,state TEXT,next_at REAL);",
            )
            .expect("outbox table");
        connection
            .execute(
                "INSERT INTO runtime_server_outbox(server,body,state,next_at) VALUES('peer',?1,'queued',?2)",
                params![serde_json::json!({"action":"message"}).to_string(), 1792224926.4847271],
            )
            .expect("Python-written outbox row");
        assert_eq!(
            outbox_next_due_at(&connection).expect("deadline query"),
            Some(1792224927)
        );
    }

    #[tokio::test]
    async fn paired_identity_reads_the_paired_server_record() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open store"),
        );
        let record = serde_json::json!({
            "id": "peer-a",
            "clientId": "client-a",
            "kind": "server",
            "label": "Peer",
            "created": 1,
            "publicKey": "public-key",
            "origin": "https://peer.invalid",
            "status": "paired"
        });
        store
            .transaction(move |tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES (?1,?2)",
                    params!["peer-a", record.to_string()],
                )?;
                Ok(())
            })
            .await
            .expect("insert paired peer");

        assert_eq!(
            HttpsPairedTransport::paired_server_identity(
                &store.test_reader().expect("test reader"),
                "peer-a"
            )
            .expect("peer identity"),
            ("https://peer.invalid".to_owned(), "public-key".to_owned())
        );
    }
}
