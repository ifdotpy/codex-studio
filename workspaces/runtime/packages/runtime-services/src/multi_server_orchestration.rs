//! Durable owner-paired envelopes. A transaction claims each exact request
//! before transport or execution. Stable-identity peer deliveries may replay
//! after an unknown reply; provider commands remain one-shot.
use std::sync::Arc;

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_store::RuntimeStore;

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
    fn exchange(
        &self,
        server: &str,
        envelope: &Value,
        timeout: std::time::Duration,
    ) -> Result<Value, AccessError>;
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
        store: &RuntimeStore,
        server: &str,
    ) -> Result<(String, String), AccessError> {
        let connection = store.read()?;
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
    fn exchange(
        &self,
        server: &str,
        envelope: &Value,
        timeout: std::time::Duration,
    ) -> Result<Value, AccessError> {
        // The helper returns owned fields, so its pooled reader is dropped before
        // identity loading, signing, or the potentially 20-second network exchange.
        let (origin, public_key) = Self::paired_server_identity(&self.store, server)?;
        let identity = load_multi_server_identity(self.store.root()).map_err(|_| {
            refused(
                503,
                "identity_unavailable",
                "The local server identity is unavailable",
            )
        })?;
        let request_id = text(envelope, "requestId");
        let url = format!("{}/api/servers/orchestration", origin.trim_end_matches('/'));
        let body = canonical_json(envelope)
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
                server_id: server,
                method: "POST",
                url: &url,
                request_id,
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
        let mut headers = headers
            .iter()
            .map(|(name, value)| (name.as_str(), value.as_str()))
            .collect::<Vec<_>>();
        headers.push(("Accept-Encoding", "identity"));
        if !body.is_empty() {
            headers.push(("Content-Type", "application/json"));
        }
        let response = exchange_paired_request(
            &self.store,
            PairedRequest {
                server_id: server,
                public_key: &public_key,
                method: "POST",
                url: &url,
                headers: &headers,
                body: &body,
                timeout,
                response_limit: MAX_RESPONSE_BYTES,
            },
        )
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
        let value =
            verify_multi_server_response(&response.body, server, &public_key).map_err(|_| {
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
        learn_paired_request_link(&self.store, server, advert);
        Ok(value)
    }
}
/// Missing effects are refused before claiming a receipt. Implementations must
/// derive actor permissions from the authenticated principal, not the payload.
pub trait OrchestrationEffects: Send + Sync {
    fn supports(&self, action: &str) -> bool;
    fn execute(&self, principal: &str, envelope: &Value) -> Result<ExchangeOutcome, AccessError>;
}

pub trait BrowserCallbackEffect: Send + Sync {
    fn receive(&self, envelope: &Value) -> Result<Value, AccessError>;
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

    fn stop_worker(
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
        crate::agent_lifecycle_runtime::execute(&self.store, &action, now, ports.as_ref())
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

    fn project_location(
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
                &self.store,
                &json!({"action":"register","path":path,"name":name}),
                &self.default_account,
                &self.known_accounts,
            )
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
            let local_server = self.store.local_server_id()?.ok_or_else(|| {
                refused(
                    503,
                    "identity_unavailable",
                    "The server identity is unavailable",
                )
            })?;
            let git_origin = crate::projects::peer_location_info(canonical_path)
                .ok()
                .and_then(|value| value.get("gitOrigin").cloned())
                .unwrap_or(Value::Null);
            self.store.transaction_with(|tx| {
                let (_, mut project) = studio_runtime_store::ProjectsRepository::get(
                    tx, project_id,
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
                        alias.get("serverId").and_then(Value::as_str) != Some(principal)
                            || alias.get("projectId").and_then(Value::as_str) != Some(home_project)
                    })
                    .collect::<Vec<_>>();
                if principal != local_server || home_project != project_id {
                    aliases
                        .push(json!({"serverId":principal,"projectId":home_project,"name":name}));
                }
                project["projectAliases"] = Value::Array(aliases);
                studio_runtime_store::ProjectsRepository::put_project(tx, &project)?;
                Ok::<_, AccessError>(())
            })?;
            return Ok(ExchangeOutcome::Applied(json!({
                "serverId":local_server,"path":path,"canonicalPath":canonical_path,
                "projectId":project_id,"gitOrigin":git_origin,
                "homeProjectId":home_project,"requestId":request_id
            })));
        }
        let location = &payload["location"];
        let project_id = text(location, "projectId");
        if !project_id.is_empty() {
            self.store.transaction_with(|tx| {
                if let Some((_, mut project)) =
                    studio_runtime_store::ProjectsRepository::get(tx, project_id)?
                {
                    let aliases = project
                        .get("projectAliases")
                        .and_then(Value::as_array)
                        .cloned()
                        .unwrap_or_default()
                        .into_iter()
                        .filter(|alias| {
                            alias.get("serverId").and_then(Value::as_str) != Some(principal)
                                || alias.get("projectId").and_then(Value::as_str)
                                    != Some(home_project)
                        })
                        .collect::<Vec<_>>();
                    project["projectAliases"] = Value::Array(aliases);
                    studio_runtime_store::ProjectsRepository::put_project(tx, &project)?;
                }
                Ok::<_, AccessError>(())
            })?;
        }
        let local_server = self.store.local_server_id()?.unwrap_or_default();
        Ok(ExchangeOutcome::Applied(
            json!({"removed":true,"serverId":local_server}),
        ))
    }

    fn worker_call(
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
            let connection = self.store.read()?;
            Some(
                crate::message_admission::MessageIngressAdmission::load(
                    &self.store,
                    &connection,
                    target_id,
                )
                .map_err(|_| refused(503, "send_admission", "The message could not be admitted"))?,
            )
        } else {
            None
        };
        self.store.transaction_with(|tx| {
            let link_raw: Option<String> = tx
                .query_row(
                    "SELECT record FROM runtime_server_links WHERE id=?1",
                    [link_id],
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
                        .any(|worker| worker.as_str() == Some(worker_id))
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
            .ok_or_else(|| refused(404, "parent_unavailable", "The parent agent is unavailable"))?;
            if parent.epoch as i64 != parent_epoch
                || parent.extra.get("autoWake") != Some(&Value::Bool(true))
                || studio_runtime_store::truthy(parent.extra.get("deletedAt"))
            {
                return Err(refused(409, "parent_stopped", "The home team was stopped"));
            }
            let worker = studio_runtime_store::AgentRepository::get(
                tx,
                &studio_runtime_store::AgentId::from(worker_id),
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
            match action {
                "message" => {
                    crate::provider_callbacks::tools::services::messages::send_remote_worker(
                        tx, worker_id, args, key, now,
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
                            !text.is_empty() && crate::coordination::message_text_within_limit(text)
                        })
                        .ok_or_else(|| {
                            refused(400, "invalid_request", "Invalid move-send request")
                        })?;
                    let delivery = args
                        .get("delivery")
                        .and_then(Value::as_str)
                        .unwrap_or("queue");
                    if !matches!(delivery, "queue" | "steer" | "after_tool" | "after_turn") {
                        return Err(refused(
                            400,
                            "invalid_request",
                            "Invalid move-send delivery mode",
                        ));
                    }
                    let target =
                        crate::event_write::checked_agent(tx, target_id).map_err(|_| {
                            refused(404, "target_unavailable", "The target agent is unavailable")
                        })?;
                    if target.root_id != worker.root_id
                        || target.id == worker.id
                        || !is_descendant_of(tx, target_id, worker_id)?
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
                            sender_id: worker_id,
                            sender_epoch: epoch,
                            recipient_id: target_id,
                            id: key,
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
    }
}

fn receive_remote_anchor_input(
    store: &RuntimeStore,
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
    let delivery = payload
        .get("delivery")
        .and_then(Value::as_str)
        .unwrap_or("queue");
    if key.is_empty()
        || link_id.is_empty()
        || !crate::coordination::message_text_within_limit(event_text)
        || kind != "user"
        || !matches!(delivery, "queue" | "steer" | "after_tool" | "after_turn")
    {
        return Err(refused(
            400,
            "invalid_request",
            "Invalid remote anchor input",
        ));
    }

    let connection = store.read()?;
    let raw: Option<String> = connection
        .query_row(
            "SELECT record FROM runtime_server_links WHERE id=?1",
            [link_id],
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
        || link.get("server").and_then(Value::as_str) != Some(principal)
        || link.get("parentEpoch").and_then(Value::as_i64) != Some(parent_epoch)
    {
        return Err(refused(
            403,
            "link_owner",
            "The remote-parent link is invalid",
        ));
    }
    let parent_id = link
        .get("parent")
        .and_then(Value::as_str)
        .ok_or_else(|| refused(503, "invalid_record", "The remote-parent link is invalid"))?;
    let parent = studio_runtime_store::AgentRepository::get(
        &connection,
        &studio_runtime_store::AgentId::from(parent_id),
    )?
    .ok_or_else(|| refused(404, "parent_unavailable", "The home agent is unavailable"))?;
    if parent.epoch != parent_epoch {
        return Err(refused(409, "parent_changed", "The home agent changed"));
    }
    let admission =
        crate::message_admission::MessageIngressAdmission::load(store, &connection, parent_id)
            .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
    drop(connection);

    let receipt = store.transaction_with(|tx| {
        let current_link: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_server_links WHERE id=?1",
                [link_id],
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
            || current_link.get("server").and_then(Value::as_str) != Some(principal)
            || current_link.get("parent").and_then(Value::as_str) != Some(parent_id)
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
            &studio_runtime_store::AgentId::from(parent_id),
        )?
        .ok_or_else(|| refused(404, "parent_unavailable", "The home agent is unavailable"))?;
        if current.epoch != parent_epoch {
            return Err(refused(409, "parent_changed", "The home agent changed"));
        }
        let posted = crate::coordination::post_message_with_admission(
            tx,
            crate::coordination::PostManagedMessage {
                room: parent_id,
                id: key,
                text: event_text,
                delivery,
                assets: &[],
            },
            &admission,
        )
        .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
        Ok::<_, AccessError>(posted.receipt)
    })?;
    Ok(ExchangeOutcome::Applied(receipt))
}

fn receive_remote_input(
    store: &RuntimeStore,
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
    let delivery = payload
        .get("delivery")
        .and_then(Value::as_str)
        .unwrap_or("queue");
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
    let connection = store.read()?;
    let raw: Option<String> = connection
        .query_row(
            "SELECT record FROM runtime_server_links WHERE id=?1",
            [link_id],
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
        || link["server"] != principal
        || link["parentEpoch"].as_i64() != Some(parent_epoch)
        || !link["workers"].as_array().is_some_and(|workers| {
            workers
                .iter()
                .any(|worker| worker.as_str() == Some(worker_id))
        })
    {
        return Err(refused(
            403,
            "link_owner",
            "The remote-parent link is invalid",
        ));
    }
    let worker = studio_runtime_store::AgentRepository::get(
        &connection,
        &studio_runtime_store::AgentId::from(worker_id),
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
    if observed_control < control_epoch {
        return Ok(ExchangeOutcome::PreEffectWaiting);
    }
    if observed_control != control_epoch
        || worker
            .extra
            .get("remoteEpoch")
            .and_then(Value::as_i64)
            .unwrap_or(0)
            > remote_epoch
    {
        return Ok(ExchangeOutcome::NotApplied(
            "The remote worker epoch changed".to_owned(),
        ));
    }
    let admission = if matches!(kind, "user" | "followup") {
        Some(
            crate::message_admission::MessageIngressAdmission::load(store, &connection, worker_id)
                .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?,
        )
    } else {
        None
    };
    drop(connection);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| refused(503, "clock_unavailable", "The system clock is unavailable"))?
        .as_secs_f64();
    let result = store.transaction_with(|tx| {
        let current = studio_runtime_store::AgentRepository::get(
            tx,
            &studio_runtime_store::AgentId::from(worker_id),
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
        if studio_runtime_store::truthy(current.extra.get("moveImportPending"))
            || studio_runtime_store::truthy(current.extra.get("movedTo"))
        {
            crate::event_write::enqueue_recovery_event(
                tx,
                &current,
                key.to_owned(),
                kind,
                event_text.to_owned(),
                now,
            )
            .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
            return Ok::<Value, AccessError>(json!({"eventId":key}));
        }
        if matches!(kind, "user" | "followup") {
            if !matches!(delivery, "queue" | "steer" | "after_tool" | "after_turn") {
                return Err(refused(
                    400,
                    "invalid_request",
                    "Invalid remote input delivery",
                ));
            }
            let admission = admission.as_ref().ok_or_else(|| {
                refused(400, "input_rejected", "The remote input was not admitted")
            })?;
            let posted = crate::coordination::post_message_with_admission(
                tx,
                crate::coordination::PostManagedMessage {
                    room: worker_id,
                    id: key,
                    text: event_text,
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
            kind,
            event_text.to_owned(),
            now,
        )
        .map_err(|_| refused(400, "input_rejected", "The remote input was not admitted"))?;
        Ok(json!({"eventId":key}))
    })?;
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
        ) || (action == "stop" && self.lifecycle_ports.is_some())
    }

    fn execute(&self, principal: &str, envelope: &Value) -> Result<ExchangeOutcome, AccessError> {
        let action = text(envelope, "action");
        if action == "browser_callback" {
            return self
                .browser_callback
                .receive(envelope)
                .map(ExchangeOutcome::Applied);
        }
        if action == "stop" {
            return self.stop_worker(principal, envelope);
        }
        if action == "folders" {
            let path = envelope["payload"].get("cwd").and_then(Value::as_str);
            let result = crate::system_reads::directories(path)
                .map_err(|_| refused(400, "folder_unavailable", "This directory is unavailable"))?;
            return serde_json::to_value(result)
                .map(ExchangeOutcome::Applied)
                .map_err(|_| {
                    refused(503, "folder_unavailable", "The directory could not be read")
                });
        }
        if matches!(action, "add_location" | "remove_location") {
            return self.project_location(principal, envelope);
        }
        if action == "location_info" {
            let path = text(&envelope["payload"], "cwd");
            return crate::projects::peer_location_info(path)
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
            let origin = text(payload, "origin");
            let cwd = payload.get("cwd").and_then(Value::as_str);
            return crate::projects::peer_location_matches(&self.store, origin, cwd)
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
                return receive_remote_anchor_input(&self.store, principal, envelope);
            }
            return receive_remote_input(&self.store, principal, envelope);
        }
        if matches!(action, "message" | "move_send") {
            return self.worker_call(principal, envelope);
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
        let result = self.store.transaction_with(|tx| {
            let raw: Option<String> = tx
                .query_row(
                    "SELECT record FROM runtime_server_links WHERE id=?1",
                    [link_id],
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
            let link: Value = serde_json::from_str(&raw)
                .map_err(|_| refused(503, "invalid_record", "The remote-parent link is invalid"))?;
            if link.get("side").and_then(Value::as_str) != Some("home")
                || link.get("server").and_then(Value::as_str) != Some(principal)
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
            .ok_or_else(|| refused(404, "parent_unavailable", "The parent agent is unavailable"))?;
            if typed.epoch != parent_epoch
                || typed.extra.get("autoWake") != Some(&Value::Bool(true))
                || studio_runtime_store::truthy(typed.extra.get("deletedAt"))
            {
                return Ok(ExchangeOutcome::Applied(
                    json!({"stored":false,"reason":"The parent was stopped"}),
                ));
            }
            if kind == "child_result" {
                let details: Value = serde_json::from_str(event_text)
                    .map_err(|_| refused(400, "invalid_request", "Invalid child result event"))?;
                let child_id = details
                    .get("agent_id")
                    .and_then(Value::as_str)
                    .ok_or_else(|| refused(400, "invalid_request", "Invalid child result event"))?;
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
                    .insert("lastCompletedTurn".to_owned(), json!(key));
                studio_runtime_store::AgentRepository::put(tx, &proxy)?;
            }
            let existing: Option<(String, String, String, f64, i64)> = tx
                .query_row(
                    "SELECT agent,kind,text,created,epoch FROM runtime_events WHERE id=?1",
                    [key],
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
                    || old_kind != kind
                    || old_text != event_text
                    || epoch != typed.epoch as i64
                {
                    return Err(refused(
                        409,
                        "request_conflict",
                        "This event identity has different content",
                    ));
                }
            } else {
                crate::event_write::enqueue_recovery_event(
                    tx,
                    &typed,
                    key.to_owned(),
                    kind,
                    event_text.to_owned(),
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
            Ok(ExchangeOutcome::Applied(json!({"eventId":key})))
        })?;
        Ok(result)
    }
}

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
fn peer_replay_safe(action: &str) -> bool {
    // Every input variant binds its stable request identity to a durable
    // message receipt or event row before reporting an applied effect.
    matches!(
        action,
        "event" | "input" | "message" | "move_send" | "location_info" | "location_matches" | "stop"
    )
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
        && peer_replay_safe(action)
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
    pub fn exchange(
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
    }

    pub fn exchange_with_timeout(
        &self,
        server: &str,
        action: &str,
        payload: &Value,
        request_id: &str,
        now: i64,
        timeout: std::time::Duration,
    ) -> Result<ExchangeOutcome, AccessError> {
        let local_server = self.store.local_server_id()?.unwrap_or_default();
        let is_local = !local_server.is_empty() && server == local_server;
        let envelope = Envelope::from_value(
            &json!({"requestId":request_id,"action":action,"payload":payload}),
        )?;
        let decision=self.store.transaction_with(|tx| {
            if !is_local { require_paired(tx,server)?; }
            if retired(tx,&envelope.key,"out",&envelope.fingerprint,Some(server))? {return Ok(DeliveryDecision::Return(ExchangeOutcome::Unknown));}
            let saved=outbox(tx,&envelope.key,now)?;
            let decision=decide_delivery(saved.as_ref(),&envelope.fingerprint,Some(server),true,&envelope.action,now)?;
            if matches!(decision,DeliveryDecision::Execute) {
                tx.execute("INSERT INTO runtime_server_outbox(id,server,body,state,created_at,fingerprint,attempts,next_at) VALUES(?1,?2,?3,'running',?4,?5,0,0) ON CONFLICT(id) DO UPDATE SET state='running'",params![envelope.key,server,envelope.body,now,envelope.fingerprint])?;
            }
            Ok::<_,AccessError>(decision)
        })?;
        if let DeliveryDecision::Return(result) = decision {
            return Ok(result);
        }
        // The claim is committed before crossing the transport boundary.
        let exchange = if is_local {
            self.receive(&local_server, &envelope.value(), now)
        } else {
            self.transport
                .exchange(server, &envelope.value(), timeout)
                .and_then(|value| ExchangeOutcome::from_value(&value, &envelope.key, now))
        };
        let result = match exchange {
            Ok(value) => value,
            Err(_) => ExchangeOutcome::Unknown,
        };
        self.finish(
            "outbox",
            &envelope.key,
            &envelope.action,
            &envelope.fingerprint,
            Some(server),
            &result,
            now,
        )?;
        Ok(result)
    }
    pub fn receive(
        &self,
        principal: &str,
        value: &Value,
        now: i64,
    ) -> Result<ExchangeOutcome, AccessError> {
        let local_server = self.store.local_server_id()?.unwrap_or_default();
        let is_local = !local_server.is_empty() && principal == local_server;
        let envelope = Envelope::from_value(value)?;
        if !self.effects.supports(&envelope.action) {
            return Err(refused(
                501,
                "orchestration_effect",
                "Not implemented: requires multi-server-runtime/orchestration-effects",
            ));
        }
        let signature = envelope.incoming_signature(principal)?;
        let (decision, replaying_claim) = self.store.transaction_with(|tx| {
            if !is_local {
                require_paired(tx, principal)?;
            }
            if retired(tx, &envelope.key, "in", &signature, None)? {
                return Ok((DeliveryDecision::Return(ExchangeOutcome::Unknown), false));
            }
            let saved = inbox(tx, &envelope.key, now)?;
            let replaying_claim = saved.is_some();
            let decision = decide_delivery(
                saved.as_ref(),
                &signature,
                None,
                false,
                &envelope.action,
                now,
            )?;
            if matches!(decision,DeliveryDecision::Execute) {
                if saved.is_some() {
                    tx.execute(
                        "UPDATE runtime_server_inbox SET state='running' WHERE id=?1 AND state IN ('waiting','running')",
                        [&envelope.key],
                    )?;
                } else {
                    tx.execute(
                        "INSERT INTO runtime_server_inbox(id,signature,state,result,created_at,principal,actor) VALUES(?1,?2,'running',NULL,?3,?4,?5)",
                        params![envelope.key, signature, now, principal, envelope.payload.get("actor").and_then(Value::as_str)],
                    )?;
                }
            }
            Ok::<_, AccessError>((decision, replaying_claim))
        })?;
        if let DeliveryDecision::Return(result) = decision {
            return Ok(result);
        }
        let mut result = match self.effects.execute(principal, &envelope.value()) {
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
            )?;
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
        )?;
        Ok(result)
    }

    /// Deliver a bounded snapshot of due intents. Each exchange claims and
    /// records its result in short transactions around the transport call.
    pub fn pump_due(&self, now: i64) -> Result<usize, AccessError> {
        let pending = self.store.read_transaction(|tx| {
            let mut statement = tx.prepare(
                "SELECT server,body,state FROM runtime_server_outbox WHERE state IN ('queued','running') AND next_at<=?1 ORDER BY rowid LIMIT ?2",
            )?;
            let rows = statement.query_map(params![now, MAX_OUTBOX_BATCH as i64], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?;
            rows.collect::<Result<Vec<_>, _>>().map_err(Into::into)
        })?;
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
            if state == "running" && !peer_replay_safe(&envelope.action) {
                continue;
            }
            let _ = self.exchange(
                &server,
                &envelope.action,
                &envelope.payload,
                &envelope.key,
                now,
            )?;
            count += 1;
        }
        Ok(count)
    }
    #[allow(clippy::too_many_arguments)]
    fn finish(
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
            if peer_replay_safe(action) && table == "outbox" {
                let attempts = self.store.read_transaction(|tx| {
                    tx.query_row(
                        "SELECT attempts FROM runtime_server_outbox WHERE id=?1",
                        [key],
                        |row| row.get::<_, i64>(0),
                    )
                    .optional()
                    .map(|value| value.unwrap_or_default().max(0) as u64)
                    .map_err(Into::into)
                })?;
                let retry_delay = peer_retry_delay(attempts as i64);
                self.store.transaction_with(|tx| {
                    tx.execute("UPDATE runtime_server_outbox SET state='queued',attempts=attempts+1,next_at=?1,error='unknown' WHERE id=?2 AND state='running'",params![now.saturating_add(retry_delay),key])?;
                    Ok::<_,AccessError>(())
                })?;
            } else if *result == ExchangeOutcome::PreEffectWaiting
                && action == "input"
                && table == "inbox"
            {
                self.store.transaction_with(|tx| {
                    tx.execute("UPDATE runtime_server_inbox SET state='waiting' WHERE id=?1 AND state='running'", [key])?;
                    Ok::<_, AccessError>(())
                })?;
            // A generic unknown input can follow a committed message/event.
            // Keep its claim running; only the typed pre-effect result above
            // proves that retrying the input is safe.
            } else if peer_replay_safe(action) && action != "input" && table == "inbox" {
                self.store.transaction_with(|tx| {
                    tx.execute("UPDATE runtime_server_inbox SET state='waiting' WHERE id=?1 AND state='running'", [key])?;
                    Ok::<_,AccessError>(())
                })?;
            }
            return Ok(());
        }
        self.store.transaction_with(|tx| {
            let saved=if table=="outbox"{outbox(tx,key,now)?}else{inbox(tx,key,now)?};
            let snapshot=saved.ok_or_else(||refused(409,"receipt_missing","The delivery claim is missing"))?;
            if snapshot.signature!=signature || snapshot.server.as_deref()!=server || snapshot.state!="running" { return Err(refused(409,"stale_receipt","The delivery receipt does not match its claim")); }
            tx.execute(&format!("UPDATE runtime_server_{table} SET state='complete',result=?1,completed_at=?2 WHERE id=?3"),params![result.to_value(key).to_string(),now,key])?;
            Ok(())
        })
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

#[cfg(test)]
mod tests {
    use super::{
        DeliveryDecision, DeliverySnapshot, ExchangeOutcome, HttpsPairedTransport, decide_delivery,
        peer_retry_delay,
    };
    use rusqlite::params;
    use std::sync::{Arc, mpsc};
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
    fn paired_identity_read_lease_is_released_before_transport() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(RuntimeStore::open(Some(directory.path())).expect("open store"));
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
            .transaction(|tx| {
                tx.execute(
                    "INSERT INTO runtime_access_clients(id,record) VALUES (?1,?2)",
                    params!["peer-a", record.to_string()],
                )?;
                Ok(())
            })
            .expect("insert paired peer");

        assert_eq!(
            HttpsPairedTransport::paired_server_identity(&store, "peer-a").expect("peer identity"),
            ("https://peer.invalid".to_owned(), "public-key".to_owned())
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        const READ_POOL_SLOTS: usize = 4;
        let mut release_senders = Vec::with_capacity(READ_POOL_SLOTS);
        std::thread::scope(|scope| {
            for _ in 0..READ_POOL_SLOTS {
                let store = Arc::clone(&store);
                let ready_tx = ready_tx.clone();
                let (release_tx, release_rx) = mpsc::channel();
                release_senders.push(release_tx);
                scope.spawn(move || {
                    let _reader = store.read().expect("reader slot remains available");
                    ready_tx.send(()).expect("report available slot");
                    release_rx.recv().expect("release slot");
                });
            }
            drop(ready_tx);
            for _ in 0..READ_POOL_SLOTS {
                ready_rx
                    .recv_timeout(std::time::Duration::from_secs(2))
                    .expect("every pooled slot is available after credential lookup");
            }
            for release in release_senders {
                release.send(()).expect("release reader slot");
            }
        });
    }
}
