//! Durable owner-paired envelopes. A transaction claims each exact request
//! before transport or execution. Stable-identity peer deliveries may replay
//! after an unknown reply; provider commands remain one-shot.
use futures_util::future::BoxFuture;
use std::sync::Arc;
use studio_runtime_schema::delivery::DeliveryMoment;

use rusqlite::{OptionalExtension, Transaction, params};
use serde_json::{Value, json};
use studio_runtime_schema::persistence::{ServerDeliveryState, ServerOperationKind};
use studio_runtime_store::multi_server_delivery::{
    RemoteReply, SavedRemoteReply, remote_operation_key,
};
use studio_runtime_store::request_results::{AdmittedRequest, RequestResultError};
use studio_runtime_store::{RuntimeStore, StoreError};

pub(crate) fn replay_error(error: RequestResultError) -> AccessError {
    match error {
        RequestResultError::Identity(
            studio_runtime_schema::persistence::RequestIdentityError::FutureTimestamp { .. },
        ) => refused(
            400,
            "request_future",
            "The request ID timestamp is too far in the future",
        ),
        RequestResultError::Expired
        | RequestResultError::Identity(
            studio_runtime_schema::persistence::RequestIdentityError::Expired { .. }
            | studio_runtime_schema::persistence::RequestIdentityError::InvalidUuidV7(_),
        ) => refused(410, "request_expired", "The request ID expired"),
        RequestResultError::Conflict { .. } => refused(
            409,
            "request_conflict",
            "This request id has different content",
        ),
        RequestResultError::Sql(error) => error.into(),
        RequestResultError::Json(_) => {
            refused(503, "invalid_record", "Stored delivery result is invalid")
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Direction {
    Incoming,
    Outgoing,
}
impl Direction {
    fn table(self) -> &'static str {
        match self {
            Self::Incoming => "runtime_server_inbox",
            Self::Outgoing => "runtime_server_outbox",
        }
    }
    fn namespace(self) -> &'static str {
        match self {
            Self::Incoming => "server-inbox",
            Self::Outgoing => "server-outbox",
        }
    }
}

fn replay_delivery(
    db: &rusqlite::Connection,
    direction: Direction,
    authority: &str,
    envelope: &Envelope,
    now: i64,
) -> Result<Option<ExchangeOutcome>, AccessError> {
    let signature = match direction {
        Direction::Incoming => envelope.incoming_signature(authority)?,
        Direction::Outgoing => envelope.fingerprint.clone(),
    };
    let scoped = envelope.clone().scoped(direction, authority).key;
    let raw = if direction == Direction::Incoming && envelope.operation.is_empty() {
        &envelope.root
    } else {
        &scoped
    };
    let saved = crate::event_write::replay_request::<SavedRemoteReply>(
        db,
        raw,
        &signature,
        chrono::Utc::now().timestamp_millis(),
    )
    .map_err(replay_error)?
    .or(crate::event_write::replay_request::<SavedRemoteReply>(
        db,
        &scoped,
        &signature,
        chrono::Utc::now().timestamp_millis(),
    )
    .map_err(replay_error)?);
    saved
        .map(|saved| {
            if direction == Direction::Incoming
                && saved.reply.principal.as_deref() != Some(authority)
            {
                return Err(refused(
                    403,
                    "receipt_owner",
                    "The receipt belongs to another caller",
                ));
            }
            if direction == Direction::Outgoing && saved.reply.server.as_deref() != Some(authority)
            {
                return Err(refused(
                    403,
                    "receipt_owner",
                    "The receipt belongs to another server",
                ));
            }
            ExchangeOutcome::from_reply(saved.reply.response, &envelope.root, now)
        })
        .transpose()
}

fn admit_delivery(
    tx: &Transaction<'_>,
    direction: Direction,
    authority: &str,
    envelope: &Envelope,
    _now: i64,
) -> Result<AdmittedRequest, AccessError> {
    use crate::event_write::{CallerAdmission, RequestKey, begin_request};
    let signature = match direction {
        Direction::Incoming => envelope.incoming_signature(authority)?,
        Direction::Outgoing => envelope.fingerprint.clone(),
    };
    match begin_request::<SavedRemoteReply>(
        tx,
        RequestKey::new(direction.namespace(), &[authority], &envelope.root)
            .raw(
                if direction == Direction::Incoming && envelope.operation.is_empty() {
                    &envelope.root
                } else {
                    &envelope.key
                },
            )
            .operation(&envelope.operation_parts()),
        &signature,
        chrono::Utc::now().timestamp_millis(),
    )
    .map_err(replay_error)?
    {
        CallerAdmission::Fresh(admitted) => Ok(admitted),
        CallerAdmission::Replay(_) => Err(refused(
            409,
            "receipt_changed",
            "The saved delivery changed",
        )),
    }
}

fn pending_key(
    tx: &rusqlite::Connection,
    direction: Direction,
    authority: &str,
    envelope: &Envelope,
) -> Result<String, AccessError> {
    let column = match direction {
        Direction::Incoming => "principal",
        Direction::Outgoing => "server",
    };
    let exists: bool = tx.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM {} WHERE id=?1 AND {column}=?2)",
            direction.table()
        ),
        params![envelope.root, authority],
        |r| r.get(0),
    )?;
    Ok(if exists && envelope.operation.is_empty() {
        envelope.root.clone()
    } else {
        envelope.clone().scoped(direction, authority).key
    })
}

fn saved_remote(
    tx: &rusqlite::Connection,
    direction: Direction,
    authority: &str,
    root: &str,
) -> Result<Option<studio_runtime_store::request_results::StoredReply<SavedRemoteReply>>, AccessError>
{
    let operation = &[ServerOperationKind::Exec.as_str()];
    let scoped = remote_operation_key(direction.namespace(), &[authority], root, operation);
    Ok(studio_runtime_store::request_results::RequestResults::read(
        tx,
        if direction == Direction::Incoming {
            root
        } else {
            &scoped
        },
        direction.namespace(),
        &[authority],
        root,
        operation,
        chrono::Utc::now().timestamp_millis(),
    )?)
}

fn settle_delivery(
    tx: &Transaction<'_>,
    direction: Direction,
    key: &str,
    root: &str,
    signature: &str,
    result: &ExchangeOutcome,
) -> Result<(), AccessError> {
    let (admission, principal, server, actor, body): (String, Option<String>,Option<String>, Option<String>, Option<String>) = match direction {
        Direction::Incoming => tx.query_row("SELECT admission,principal,NULL,actor,NULL FROM runtime_server_inbox WHERE id=?1 AND signature=?2", params![key,signature], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?,
        Direction::Outgoing => tx.query_row("SELECT admission,NULL,server,json_extract(body,'$.payload.actor'),body FROM runtime_server_outbox WHERE id=?1 AND fingerprint=?2", params![key,signature], |r| Ok((r.get(0)?,r.get(1)?,r.get(2)?,r.get(3)?,r.get(4)?)))?,
    };
    let admission: AdmittedRequest = serde_json::from_str(&admission).map_err(|_| {
        refused(
            503,
            "invalid_record",
            "Stored delivery admission is invalid",
        )
    })?;
    if admission.signature() != signature {
        return Err(refused(
            409,
            "request_conflict",
            "This request id has different content",
        ));
    }
    let response = result.to_reply(root);
    let saved = SavedRemoteReply {
        principal,
        server,
        actor: actor.clone(),
        body,
        response,
    };
    crate::event_write::settle_request(tx, &admission, actor.as_deref(), &saved)
        .map_err(replay_error)?;
    if direction == Direction::Outgoing
        && let Some(body) = saved.body.as_deref()
    {
        let envelope: Value = serde_json::from_str(body).map_err(|_| {
            refused(
                503,
                "invalid_record",
                "Stored cross-server envelope is invalid",
            )
        })?;
        if matches!(
            text(&envelope, "action"),
            "add_location" | "remove_location"
        ) {
            let local = RuntimeStore::transaction_local_server_id(tx)?.ok_or_else(|| {
                refused(
                    503,
                    "identity_unavailable",
                    "The local server identity is unavailable",
                )
            })?;
            let now = crate::event_write::unix_time().map_err(|_| {
                refused(503, "clock_unavailable", "The system clock is unavailable")
            })?;
            crate::project_locations::complete_snapshot_in_transaction(
                tx,
                key,
                saved.server.as_deref().ok_or_else(invalid_receipt)?,
                body,
                &saved.response,
                &local,
                now,
            )
            .map_err(|_| {
                refused(
                    409,
                    "project_location_changed",
                    "The project location changed before settlement",
                )
            })?;
        }
    }
    tx.execute(
        &format!("DELETE FROM {} WHERE id=?1", direction.table()),
        [key],
    )?;
    Ok(())
}

use crate::multi_server_auth::{AccessError, digest, refused, schema, stored, text};
use crate::remote_transport::{
    MAX_REQUEST_BYTES, MAX_RESPONSE_BYTES, MultiServerRequest, PairedRequest, canonical_json,
    exchange_paired_request, learn_paired_request_link, load_multi_server_identity,
    multi_server_signed_headers, new_nonce, verify_multi_server_reply,
};

#[derive(Clone, Debug)]
pub enum ExchangeOutcome {
    Applied(AppliedReply),
    NotApplied(String),
    Unknown,
    /// The receiver has not crossed the effect boundary and will wait for its
    /// prerequisite. Unlike `Unknown`, this claim is safe to retry later.
    PreEffectWaiting,
}
#[derive(Clone, Debug)]
pub struct AppliedReply {
    pub value: Box<serde_json::value::RawValue>,
    pub output_expires_at: Option<f64>,
    pub handle: bool,
    pub output_ref: bool,
}
impl AppliedReply {
    pub fn new(value: Box<serde_json::value::RawValue>) -> Self {
        #[derive(serde::Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Metadata<'a> {
            #[serde(borrow)]
            output_expires_at: Option<&'a serde_json::value::RawValue>,
            #[serde(default, deserialize_with = "present")]
            handle: bool,
            #[serde(default, deserialize_with = "present")]
            output_ref: bool,
            #[serde(borrow)]
            value: Option<&'a serde_json::value::RawValue>,
            #[serde(borrow)]
            result: Option<&'a serde_json::value::RawValue>,
        }
        fn present<'de, D: serde::Deserializer<'de>>(d: D) -> Result<bool, D::Error> {
            <serde::de::IgnoredAny as serde::Deserialize>::deserialize(d).map(|_| true)
        }
        fn metadata(raw: &serde_json::value::RawValue) -> Option<(f64, bool, bool)> {
            let item: Metadata<'_> = serde_json::from_str(raw.get()).ok()?;
            if let Some(expires) = item
                .output_expires_at
                .and_then(|v| serde_json::from_str(v.get()).ok())
                && (item.handle || item.output_ref)
            {
                return Some((expires, item.handle, item.output_ref));
            }
            item.value
                .and_then(metadata)
                .or_else(|| item.result.and_then(metadata))
        }
        let (output_expires_at, handle, output_ref) = metadata(&value)
            .map_or((None, false, false), |(expiry, handle, output_ref)| {
                (Some(expiry), handle, output_ref)
            });
        Self {
            value,
            output_expires_at,
            handle,
            output_ref,
        }
    }
}
impl PartialEq for ExchangeOutcome {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Applied(a), Self::Applied(b)) => a.value.get() == b.value.get(),
            (Self::NotApplied(a), Self::NotApplied(b)) => a == b,
            (Self::Unknown, Self::Unknown) | (Self::PreEffectWaiting, Self::PreEffectWaiting) => {
                true
            }
            _ => false,
        }
    }
}
impl ExchangeOutcome {
    pub fn to_reply(&self, key: &str) -> RemoteReply {
        use studio_runtime_schema::persistence::RemoteOutcome;
        let (outcome, value, error, waiting_for) = match self {
            Self::Applied(value) => (
                RemoteOutcome::Applied,
                Some(value.value.clone()),
                None,
                None,
            ),
            Self::NotApplied(error) => (RemoteOutcome::NotApplied, None, Some(error.clone()), None),
            Self::Unknown => (RemoteOutcome::Unknown, None, None, None),
            Self::PreEffectWaiting => (
                RemoteOutcome::Unknown,
                None,
                None,
                Some("The stop or parent update".to_owned()),
            ),
        };
        RemoteReply {
            request_id: key.to_owned(),
            outcome,
            value,
            error,
            waiting_for,
        }
    }
    fn from_reply(reply: RemoteReply, key: &str, now: i64) -> Result<Self, AccessError> {
        use studio_runtime_schema::persistence::RemoteOutcome;
        if reply.request_id != key {
            return Err(invalid_receipt());
        }
        match reply.outcome {
            RemoteOutcome::Applied => {
                #[derive(serde::Deserialize)]
                struct ReplyExpiry {
                    #[serde(rename = "outputExpiresAt")]
                    output_expires_at: Option<f64>,
                }
                let value = reply.value.ok_or_else(invalid_receipt)?;
                if serde_json::from_str::<ReplyExpiry>(value.get())
                    .ok()
                    .and_then(|reply| reply.output_expires_at)
                    .is_some_and(|expires| expires <= now as f64)
                {
                    Ok(Self::Unknown)
                } else {
                    Ok(Self::Applied(AppliedReply::new(value)))
                }
            }
            RemoteOutcome::NotApplied => reply
                .error
                .map(Self::NotApplied)
                .ok_or_else(invalid_receipt),
            RemoteOutcome::Unknown => Ok(Self::Unknown),
        }
    }
}
fn invalid_receipt() -> AccessError {
    refused(
        502,
        "invalid_receipt",
        "The paired server returned an invalid receipt",
    )
}
fn applied(value: &impl serde::Serialize) -> Result<ExchangeOutcome, AccessError> {
    serde_json::value::to_raw_value(value)
        .map(AppliedReply::new)
        .map(ExchangeOutcome::Applied)
        .map_err(|_| invalid_receipt())
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
    ) -> BoxFuture<'a, Result<RemoteReply, AccessError>>;
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
    ) -> BoxFuture<'a, Result<RemoteReply, AccessError>> {
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
            let value =
                verify_multi_server_reply(&response.body, &server, &public_key).map_err(|_| {
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
    known_accounts: Vec<crate::account_policy::PublicAccount>,
    lifecycle_ports: Option<Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>>,
    /// The installed app that this server offers to paired servers (docs/peer-updates.md).
    update_offer: Arc<crate::peer_updates::Offer>,
    /// Where a paired server's `projects.changed` notice goes; no hook, no refresh.
    projects_changed: Option<Arc<crate::peer_projects::PeerProjectsWake>>,
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
            update_offer: Arc::new(crate::peer_updates::Offer::installed()),
            projects_changed: None,
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
            update_offer: Arc::new(crate::peer_updates::Offer::installed()),
            projects_changed: None,
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
            update_offer: Arc::new(crate::peer_updates::Offer::installed()),
            projects_changed: None,
        }
    }

    pub fn with_project_accounts(
        store: Arc<RuntimeStore>,
        browser_callback: Arc<dyn BrowserCallbackEffect>,
        default_account: String,
        known_accounts: Vec<crate::account_policy::PublicAccount>,
    ) -> Self {
        Self {
            store,
            browser_callback,
            default_account,
            known_accounts,
            lifecycle_ports: None,
            update_offer: Arc::new(crate::peer_updates::Offer::installed()),
            projects_changed: None,
        }
    }

    pub fn with_project_accounts_and_lifecycle_ports(
        store: Arc<RuntimeStore>,
        browser_callback: Arc<dyn BrowserCallbackEffect>,
        default_account: String,
        known_accounts: Vec<crate::account_policy::PublicAccount>,
        lifecycle_ports: Arc<dyn crate::agent_lifecycle_runtime::LifecyclePorts>,
    ) -> Self {
        Self {
            store,
            browser_callback,
            default_account,
            known_accounts,
            lifecycle_ports: Some(lifecycle_ports),
            update_offer: Arc::new(crate::peer_updates::Offer::installed()),
            projects_changed: None,
        }
    }

    pub fn with_projects_changed(
        mut self,
        wake: Arc<crate::peer_projects::PeerProjectsWake>,
    ) -> Self {
        self.projects_changed = Some(wake);
        self
    }

    #[cfg(test)]
    pub(crate) fn with_update_offer(mut self, offer: crate::peer_updates::Offer) -> Self {
        self.update_offer = Arc::new(offer);
        self
    }

    /// Python `_receive` for the `update.*` actions: reads of the installed app.
    /// A refusal is a not-applied receipt; a file failure leaves the outcome unknown.
    async fn peer_update(
        &self,
        principal: &str,
        envelope: &Value,
    ) -> Result<ExchangeOutcome, AccessError> {
        let result = Arc::clone(&self.update_offer)
            .receive(
                self.store.root().to_path_buf(),
                principal.to_owned(),
                text(envelope, "action").to_owned(),
                envelope["payload"].clone(),
            )
            .await;
        match result {
            Ok(value) => applied(&value),
            Err(crate::peer_updates::UpdateError::Refused(message)) => {
                Ok(ExchangeOutcome::NotApplied(message))
            }
            Err(crate::peer_updates::UpdateError::Failed(_)) => Err(refused(
                503,
                "update_unavailable",
                "The update read could not run",
            )),
        }
    }

    /// `backup.put`: one chunk of a backup file from a paired server (docs/backup.md).
    async fn peer_backup(
        &self,
        principal: &str,
        envelope: &Value,
    ) -> Result<ExchangeOutcome, AccessError> {
        let root = self.store.root().to_path_buf();
        let (principal, payload) = (principal.to_owned(), envelope["payload"].clone());
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/peer_backup",
            move || crate::peer_backup::receive(&root, &principal, &payload),
        )
        .await
        .map_err(|_| refused(503, "backup_unavailable", "The backup write could not run"))?;
        match result {
            Ok(value) => applied(&value),
            Err(crate::peer_backup::ReceiveError::Refused(message)) => {
                Ok(ExchangeOutcome::NotApplied(message))
            }
            Err(crate::peer_backup::ReceiveError::Failed(_)) => Err(refused(
                503,
                "backup_unavailable",
                "The backup write could not run",
            )),
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
        .map_err(|error| match error {
            crate::agent_management::ManagementError::Expired => {
                refused(410, "request_expired", "The request ID expired")
            }
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
        .and_then(|value| applied(&value))
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
            return applied(&json!({
                "serverId":local_server,"path":path,"canonicalPath":canonical_path,
                "projectId":project_id,"gitOrigin":git_origin,
                "homeProjectId":home_project,"requestId":request_id
            }));
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
        applied(&json!({"removed":true,"serverId":local_server}))
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
            let target_id = target_id.to_owned();
            Some(
                self.store
                    .read_async(
                        "runtime-services/multi_server_orchestration",
                        move |connection| {
                            crate::message_admission::MessageIngressAdmission::read(
                                connection, &target_id,
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
                    || parent.auto_wake != Some(true)
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
                    || worker.auto_wake != Some(true)
                    || studio_runtime_store::truthy(worker.extra.get("deletedAt"))
                {
                    return Err(refused(409, "worker_stopped", "The worker was stopped"));
                }
                match action.as_str() {
                    "message" => {
                        crate::provider_callbacks::tools::services::messages::send_remote_worker(
                            tx, &worker_id, &args, &key, now,
                        )
                        .map(|value| ExchangeOutcome::Applied(AppliedReply::new(value)))
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
                            || target.auto_wake != Some(true)
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
                        .map_err(|_| refused(403, "send_refused", "The message was refused"))
                        .and_then(|result| applied(&result.receipt))
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
                let admission =
                    crate::message_admission::MessageIngressAdmission::read(connection, parent_id)
                        .map_err(|_| {
                            refused(400, "input_rejected", "The remote input was not admitted")
                        })?;
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
    applied(&receipt)
}

fn durable_anchor_input_receipt(
    transaction: &Transaction<'_>,
    principal: &str,
    envelope: &Envelope,
) -> Result<Option<Box<serde_json::value::RawValue>>, AccessError> {
    let payload = &envelope.payload;
    if envelope.action.as_str() != "input"
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
    if envelope.root.is_empty()
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
        &envelope.root,
        event_text,
        delivery,
    )
    .map_err(|_| {
        refused(
            503,
            "invalid_record",
            "Stored remote message receipt is invalid",
        )
    })?
    .map(|receipt| serde_json::value::to_raw_value(&receipt).map_err(|_| invalid_receipt()))
    .transpose()
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
                | "work_result_invalid"
        )
    {
        return Err(refused(400, "invalid_request", "Invalid remote input"));
    }
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
                return serde_json::value::to_raw_value(&json!({"eventId":key}))
                    .map_err(|_| invalid_receipt());
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
                return serde_json::value::to_raw_value(&posted.receipt)
                    .map_err(|_| invalid_receipt());
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
            serde_json::value::to_raw_value(&json!({"eventId":key})).map_err(|_| invalid_receipt())
        })
        .await?;
    Ok(ExchangeOutcome::Applied(AppliedReply::new(result)))
}

fn callback_error(error: crate::provider_callbacks::CallbackError) -> AccessError {
    use crate::provider_callbacks::CallbackError as Callback;
    match error {
        Callback::Expired => refused(410, "request_expired", "The request ID expired"),
        Callback::Store(error) => AccessError::Store(error),
        Callback::InvalidResultSchema(_) => {
            refused(400, "invalid_result_schema", "Invalid result_schema")
        }
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
            || action == crate::peer_projects::CHANGED_ACTION
            || crate::peer_updates::ACTIONS.contains(&action)
            || action == crate::peer_backup::ACTION
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
                    enqueue_in_transaction(
                        tx,
                        &principal,
                        EXEC_EVENT_ACTION,
                        &payload,
                        &handle,
                        now as i64,
                    )?;
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
            Ok(value) => applied(&value),
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
        applied(&listed)
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
            Ok(value) => applied(&value),
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
                .and_then(|value| applied(&value));
        }
        if action == "stop" {
            return self.stop_worker(principal, envelope).await;
        }
        if action == "projects" {
            return self.projects().await;
        }
        if action == crate::peer_projects::CHANGED_ACTION {
            if let Some(wake) = &self.projects_changed {
                wake.peer_changed(principal);
            }
            return Ok(ExchangeOutcome::Applied(AppliedReply::new(
                serde_json::value::RawValue::from_string("{}".to_owned()).expect("empty reply"),
            )));
        }
        if crate::peer_updates::ACTIONS.contains(&action) {
            return self.peer_update(principal, envelope).await;
        }
        if action == crate::peer_backup::ACTION {
            return self.peer_backup(principal, envelope).await;
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
            return applied(&result);
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
            .map_err(|_| {
                refused(
                    400,
                    "location_unavailable",
                    "The project folder is unavailable",
                )
            })
            .and_then(|value| applied(&value));
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
                .map_err(|_| {
                    refused(
                        400,
                        "location_unavailable",
                        "Project location matches are unavailable",
                    )
                })
                .and_then(|value| applied(&value));
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
            let payload = envelope["payload"].clone();
            let key = crate::projects::cross_server_identity(&[
                "command-exit",
                text(&payload, "handle"),
                text(&payload["record"], "status"),
            ]);
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
                "child_result"
                    | "child_stopped"
                    | "agent_message"
                    | "work_review"
                    | "work_result_invalid"
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
                    return applied(
                        &json!({"stored":false,"reason":"The parent epoch was superseded"}),
                    );
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
                    || typed.auto_wake != Some(true)
                    || studio_runtime_store::truthy(typed.extra.get("deletedAt"))
                {
                    return applied(&json!({"stored":false,"reason":"The parent was stopped"}));
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
                applied(&json!({"eventId":key_for_write}))
            })
            .await?;
        Ok(result)
    }
}

// Cross-server command receipts. These names and texts are the wire contract
// between paired servers.

/// The one-shot action that starts a command on a paired server.
pub const EXEC_ACTION: &str = ServerOperationKind::Exec.as_str();
/// The effect-free probe that reads the stored receipt of one `exec` request.
pub const EXEC_RECEIPT_ACTION: &str = ServerOperationKind::ExecReceipt.as_str();
/// The command-exit notice that the executing server sends to the owner.
pub const EXEC_EVENT_ACTION: &str = ServerOperationKind::ExecEvent.as_str();
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

/// Python `deliver`: an `exec` row in receipt form never sends its command
/// again. It sends this probe with a stable identity instead.
fn exec_receipt_probe(key: &str, payload: &Value) -> Option<Value> {
    let digest = payload.get(EXEC_RECEIPT_DIGEST_FIELD)?;
    if !studio_runtime_store::truthy(Some(digest)) {
        return None;
    }
    Some(json!({
        "requestId": key,
        "operation":[EXEC_RECEIPT_IDENTITY_PART],
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
        "requestId": envelope.root,
        "operation":envelope.operation,
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
    let scoped = remote_operation_key(
        Direction::Outgoing.namespace(),
        &[server],
        handle,
        &[ServerOperationKind::Exec.as_str()],
    );
    let row: Option<(String, String)> = tx
        .query_row(
            "SELECT server,body FROM runtime_server_outbox WHERE id IN (?1,?2)",
            params![handle, scoped],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (saved_server, body) = match row {
        Some(row) => row,
        None => {
            let Some(saved) = saved_remote(tx, Direction::Outgoing, server, handle)? else {
                return Ok(false);
            };
            if saved.reply.server.as_deref() != Some(server) {
                return Ok(false);
            }
            let Some(body) = saved.reply.body else {
                return Ok(false);
            };
            (server.to_owned(), body)
        }
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
        return applied(&json!({"stored": true}));
    }
    let actor = actor.as_str().ok_or_else(invalid)?;
    let Some(lead) = studio_runtime_store::AgentRepository::get(
        tx,
        &studio_runtime_store::AgentId::from(actor),
    )?
    else {
        return applied(&json!({"stored": false, "detail": "The lead no longer exists"}));
    };
    if studio_runtime_store::truthy(lead.extra.get("deletedAt")) {
        return applied(&json!({"stored": false, "detail": "The lead was deleted"}));
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
    applied(&json!({"stored": true}))
}

/// The `exec_receipt` branch of Python `_receive`. `None` is an unexpected
/// payload (Python raises outside its refusal types), so the probe claim stays
/// running and the reply is unknown. `Some(Err(text))` is a refusal.
fn exec_receipt_answer(
    tx: &Transaction<'_>,
    principal: &str,
    payload: &Value,
    now: i64,
) -> Result<Option<Result<RemoteReply, &'static str>>, AccessError> {
    let Some(handle) = payload.get("handle").and_then(Value::as_str) else {
        return Ok(None);
    };
    let unknown = ExchangeOutcome::Unknown.to_reply(handle);
    let invalid = || refused(503, "invalid_record", "Stored delivery result is invalid");
    if let Some(saved) = saved_remote(tx, Direction::Incoming, principal, handle)? {
        let signature = saved.signature;
        let saved = saved.reply;
        if saved.principal.as_deref() != Some(principal)
            || !payload_actor_is(payload, saved.actor.as_deref())
        {
            return Ok(Some(Err(EXEC_RECEIPT_OWNER_REFUSAL)));
        }
        if payload.get("digest").and_then(Value::as_str) != Some(signature.as_str()) {
            return Ok(Some(Err(EXEC_RECEIPT_DIGEST_REFUSAL)));
        }
        return Ok(Some(Ok(
            match ExchangeOutcome::from_reply(saved.response.clone(), handle, now)? {
                ExchangeOutcome::Unknown => unknown,
                ExchangeOutcome::Applied(_)
                | ExchangeOutcome::NotApplied(_)
                | ExchangeOutcome::PreEffectWaiting => saved.response,
            },
        )));
    }
    let probe_source = Envelope::from_value(
        &json!({"requestId":handle,"operation":[],"action":ServerOperationKind::Exec,"payload":{}}),
    )?;
    let key = pending_key(tx, Direction::Incoming, principal, &probe_source)?;
    let original: Option<(String, Option<String>, Option<String>)> = tx
        .query_row(
            "SELECT signature,principal,actor FROM runtime_server_inbox WHERE id=?1",
            [key],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
        )
        .optional()?;
    let Some((signature, owner, actor)) = original else {
        return Ok(Some(Ok(unknown)));
    };
    if owner.as_deref() != Some(principal) || !payload_actor_is(payload, actor.as_deref()) {
        return Ok(Some(Err(EXEC_RECEIPT_OWNER_REFUSAL)));
    }
    if payload.get("digest").and_then(Value::as_str) != Some(signature.as_str()) {
        return Ok(Some(Err(EXEC_RECEIPT_DIGEST_REFUSAL)));
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
            return Ok(Some(Ok(applied(&record)?.to_reply(handle))));
        }
    }
    Ok(Some(Ok(unknown)))
}

/// The saved state of an `exec` outbox row that is already in receipt form.
enum SavedExecReceipt {
    /// Not settled: deliver this probe.
    Pending(Value, String, String),
    /// Settled: the saved receipt of the original command.
    Settled(ExchangeOutcome),
}

/// Python `deliver` and `_retry` for an `exec` row: a known command outcome
/// completes the row; an unknown one keeps it in `state` and schedules the
/// next probe.
fn settle_exec_row(
    tx: &Transaction<'_>,
    key: &str,
    root: &str,
    state: ServerDeliveryState,
    attempts: i64,
    result: &ExchangeOutcome,
    now: i64,
) -> Result<(), AccessError> {
    match result {
        ExchangeOutcome::Applied(_) | ExchangeOutcome::NotApplied(_) => {
            let signature: String = tx.query_row(
                "SELECT fingerprint FROM runtime_server_outbox WHERE id=?1",
                [key],
                |row| row.get(0),
            )?;
            settle_delivery(tx, Direction::Outgoing, key, root, &signature, result)?;
        }
        ExchangeOutcome::Unknown | ExchangeOutcome::PreEffectWaiting => {
            tx.execute(
            "UPDATE runtime_server_outbox SET state=?1,attempts=attempts+1,next_at=?2,error='unknown' WHERE id=?3",
            params![state, now.saturating_add(peer_retry_delay(attempts)), key],
        )?;
        }
    }
    Ok(())
}

#[derive(Clone)]
pub struct Envelope {
    key: String,
    root: String,
    action: ServerOperationKind,
    operation: Vec<String>,
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
            root: text(value, "requestId").to_owned(),
            action: action.parse().map_err(|_|refused(400,"invalid_request","Unknown cross-server operation"))?,
            operation: value["operation"].as_array().and_then(|items| items.iter().map(|item| item.as_str().map(str::to_owned)).collect()).ok_or_else(||refused(400,"invalid_request","Invalid cross-server operation identity"))?,
            payload: value["payload"].clone(),
            fingerprint: digest(canonical_json(&json!({"requestId":text(value,"requestId"),"action":action,"payload":value["payload"]})).map_err(|_|refused(400,"invalid_request","Invalid cross-server request"))?.as_bytes()),
            body,
        })
    }
    pub fn value(&self) -> Value {
        json!({"requestId":self.root,"operation":self.operation,"action":self.action,"payload":self.payload})
    }
    fn scoped(mut self, direction: Direction, peer: &str) -> Self {
        self.key = remote_operation_key(
            direction.namespace(),
            &[peer],
            &self.root,
            &self.operation_parts(),
        );
        self
    }
    fn operation_parts(&self) -> Vec<&str> {
        std::iter::once(self.action.as_str())
            .chain(self.operation.iter().map(String::as_str))
            .collect()
    }
    fn incoming_signature(&self, principal: &str) -> Result<String, AccessError> {
        Ok(digest(
            canonical_json(&json!([principal, self.action, self.payload]))
                .map_err(|_| refused(400, "invalid_request", "Invalid cross-server request"))?
                .as_bytes(),
        ))
    }
}

/// The enum codec decodes the retained delivery columns once at the SQL boundary.
pub struct DeliverySnapshot {
    state: ServerDeliveryState,
    signature: String,
    server: Option<String>,
    next_at: f64,
}
impl DeliverySnapshot {
    fn from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<Self> {
        Ok(Self {
            state: row.get(0)?,
            signature: row.get(1)?,
            server: row.get(2)?,
            next_at: row.get(3)?,
        })
    }
}
pub enum DeliveryDecision {
    Execute,
    Return(ExchangeOutcome),
}
const PEER_REPLAY_SAFE_ACTIONS: &[ServerOperationKind] = &[
    ServerOperationKind::Event,
    ServerOperationKind::Input,
    ServerOperationKind::Message,
    ServerOperationKind::MoveSend,
    ServerOperationKind::LocationInfo,
    ServerOperationKind::LocationMatches,
    ServerOperationKind::Stop,
    // The event row identity is the request identity, so a replay finds it.
    ServerOperationKind::ExecEvent,
    // Effect-free reads (Python `receive` runs them again for a running claim).
    ServerOperationKind::Projects,
    ServerOperationKind::ProjectsChanged,
    ServerOperationKind::Folders,
    ServerOperationKind::Git,
    ServerOperationKind::Chunk,
    ServerOperationKind::ExecRead,
    // Removing a released bundle again has no further effect.
    ServerOperationKind::Release,
    // Reads of the installed app and its archive (Python `UPDATE_ACTIONS`).
    ServerOperationKind::UpdateOffer,
    ServerOperationKind::UpdateExport,
    ServerOperationKind::UpdateChunk,
    ServerOperationKind::UpdateRelease,
    // A chunk lands at its offset, so the same chunk again changes nothing (docs/backup.md).
    ServerOperationKind::BackupPut,
];

/// Worker calls that a remote worker sends to its home server (Python
/// `worker_call`). The home server binds each request identity to a durable
/// inbox claim, so the sender delivers the same envelope again after a lost
/// reply, as Python `_retry` does. The receiver never runs a claimed write
/// twice, so these actions are not replay-safe on the receiving side.
pub(crate) const WORKER_CALL_RETRY_ACTIONS: &[ServerOperationKind] = &[
    ServerOperationKind::Task,
    ServerOperationKind::Complaint,
    ServerOperationKind::Context,
    ServerOperationKind::Directory,
    ServerOperationKind::ChatRead,
    ServerOperationKind::MoveManage,
    ServerOperationKind::MoveInterrupt,
    ServerOperationKind::MoveSpawn,
];

/// The sender may deliver this queued or crashed outbox envelope again.
fn outgoing_retry(action: ServerOperationKind) -> bool {
    action == ServerOperationKind::Export
        || peer_replay_safe(action)
        || WORKER_CALL_RETRY_ACTIONS.contains(&action)
}

fn outgoing_retry_sql_values() -> String {
    PEER_REPLAY_SAFE_ACTIONS
        .iter()
        .chain(WORKER_CALL_RETRY_ACTIONS)
        .chain(std::iter::once(&ServerOperationKind::Export))
        .map(|action| format!("'{}'", action.as_str()))
        .collect::<Vec<_>>()
        .join(",")
}

fn peer_replay_safe(action: ServerOperationKind) -> bool {
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
    let action: ServerOperationKind = <ServerOperationKind as serde::Deserialize>::deserialize(
        serde::de::value::StrDeserializer::<serde_json::Error>::new(action),
    )
    .map_err(|_| refused(400, "invalid_request", "Unknown cross-server operation"))?;
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
    // A running input claim is ambiguous: only an exact durable-effect lookup
    // may resolve it. The generic effect adapter cannot prove that lookup, so
    // leave the receipt unknown instead of executing the input again.
    if !outgoing
        && (snapshot.state == ServerDeliveryState::Waiting
            || (peer_replay_safe(action)
                && action != ServerOperationKind::Input
                && snapshot.state == ServerDeliveryState::Running))
    {
        return Ok(DeliveryDecision::Execute);
    }
    if outgoing
        && outgoing_retry(action)
        && matches!(
            snapshot.state,
            ServerDeliveryState::Queued | ServerDeliveryState::Running
        )
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

fn check_project_reservation(
    tx: &Transaction<'_>,
    action: &str,
    payload: &Value,
) -> Result<(), AccessError> {
    if matches!(action, "add_location" | "remove_location") {
        crate::project_locations::check_location_admission(tx, payload).map_err(
            |error| match error {
                crate::project_locations::LocationError::Store(error) => AccessError::from(error),
                crate::project_locations::LocationError::InvalidStored { .. }
                | crate::project_locations::LocationError::Refused(_)
                | crate::project_locations::LocationError::Requires(_)
                | crate::project_locations::LocationError::Sqlite(_) => refused(
                    409,
                    "project_location_changed",
                    "The project or caller changed before location reservation",
                ),
            },
        )?;
    }
    Ok(())
}

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
    let mut envelope = Envelope::from_value(
        &json!({"requestId":request_id,"operation":[],"action":action,"payload":payload}),
    )?
    .scoped(Direction::Outgoing, server);
    require_paired(transaction, server)?;
    if replay_delivery(transaction, Direction::Outgoing, server, &envelope, now)?.is_some() {
        return Ok(());
    }
    envelope.key = pending_key(transaction, Direction::Outgoing, server, &envelope)?;
    let prior = transaction
        .query_row(
            "SELECT server,fingerprint FROM runtime_server_outbox WHERE id=?1",
            [&envelope.key],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    if let Some((saved_server, fingerprint)) = prior {
        if saved_server != server || fingerprint != envelope.fingerprint {
            return Err(refused(
                409,
                "request_conflict",
                "This request id has different content",
            ));
        }
        return Ok(());
    }
    check_project_reservation(transaction, action, payload)?;
    let admitted = admit_delivery(transaction, Direction::Outgoing, server, &envelope, now)?;
    transaction.execute(
        "INSERT INTO runtime_server_outbox(id,server,body,state,created_at,next_at,fingerprint,attempts,admission) VALUES(?1,?2,?3,?7,?4,0,?5,0,?6)",
        params![envelope.key, server, envelope.body, now, envelope.fingerprint, serde_json::to_string(&admitted).map_err(|_| refused(503,"invalid_record","Delivery admission could not be saved"))?,ServerDeliveryState::Queued],
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
        self.exchange_request(
            server,
            &json!({"requestId":request_id,"operation":[],"action":action,"payload":payload}),
            now,
            timeout,
        )
        .await
    }

    pub async fn exchange_operation(
        &self,
        server: &str,
        request: &Value,
        now: i64,
    ) -> Result<ExchangeOutcome, AccessError> {
        self.exchange_request(server, request, now, std::time::Duration::from_secs(20))
            .await
    }

    async fn exchange_request(
        &self,
        server: &str,
        request: &Value,
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
        let mut envelope = Envelope::from_value(request)?.scoped(Direction::Outgoing, server);

        if envelope.action.as_str() == EXEC_ACTION {
            match self
                .saved_exec_receipt(server, is_local, &envelope, now)
                .await?
            {
                Some(SavedExecReceipt::Settled(result)) => return Ok(result),
                Some(SavedExecReceipt::Pending(probe, key, signature)) => {
                    envelope.key = key;
                    envelope.fingerprint = signature;
                    return self
                        .deliver_exec_receipt(
                            server,
                            is_local.then_some(local_server.as_str()),
                            &envelope,
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
        let mut envelope_for_claim = envelope.clone();
        let server_for_claim = server.to_owned();
        let in_flight_until = now.saturating_add(timeout.as_secs().max(1) as i64);
        let (decision,key)=self.store.transaction_with(move |tx| {
            if !is_local { require_paired(tx,&server_for_claim)?; }
            if let Some(result) = replay_delivery(tx,Direction::Outgoing,&server_for_claim,&envelope_for_claim,now)? {
                return Ok((DeliveryDecision::Return(result),envelope_for_claim.key));
            }
            envelope_for_claim.key=pending_key(tx,Direction::Outgoing,&server_for_claim,&envelope_for_claim)?;
            let saved=outbox(tx,&envelope_for_claim.key)?;
            let decision=decide_delivery(saved.as_ref(),&envelope_for_claim.fingerprint,Some(&server_for_claim),true,envelope_for_claim.action.as_str(),now)?;
            if matches!(decision,DeliveryDecision::Execute) {
                check_project_reservation(tx,envelope_for_claim.action.as_str(),&envelope_for_claim.value()["payload"])?;
                // An in-flight claim is not due: the pump replays a running peer
                // operation only after its exchange deadline has passed.
                if saved.is_some() {
                    tx.execute("UPDATE runtime_server_outbox SET state=?1,next_at=?2 WHERE id=?3",params![ServerDeliveryState::Running,in_flight_until,envelope_for_claim.key])?;
                } else {
                    let admitted = admit_delivery(tx,Direction::Outgoing,&server_for_claim,&envelope_for_claim,now)?;
                    let admission = serde_json::to_string(&admitted).map_err(|_| refused(503,"invalid_record","Delivery admission could not be saved"))?;
                    tx.execute("INSERT INTO runtime_server_outbox(id,server,body,state,created_at,fingerprint,attempts,next_at,admission) VALUES(?1,?2,?3,?7,?4,?5,0,?6,?8)",params![envelope_for_claim.key,server_for_claim,envelope_for_claim.body,now,envelope_for_claim.fingerprint,in_flight_until,ServerDeliveryState::Running,admission])?;
                }
            }
            Ok::<_,AccessError>((decision,envelope_for_claim.key))
        }).await?;
        envelope.key = key;
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
                .and_then(|value| ExchangeOutcome::from_reply(value, &envelope.root, now))
        };
        let result = match exchange {
            Ok(value) => value,
            Err(_) => ExchangeOutcome::Unknown,
        };
        if envelope.action.as_str() == EXEC_ACTION {
            return self
                .finish_exec(server, &envelope, &local_server, result, now)
                .await;
        }
        self.finish(
            Direction::Outgoing,
            &envelope.key,
            &envelope.root,
            envelope.action,
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
        is_local: bool,
        envelope: &Envelope,
        now: i64,
    ) -> Result<Option<SavedExecReceipt>, AccessError> {
        let envelope_for_read = envelope.clone();
        let server_for_read = server.to_owned();
        type Row = (String, String, String, String);
        let (replayed, row): (Option<ExchangeOutcome>, Option<Row>) = self
            .store
            .transaction_with(move |tx| {
                if !is_local {
                    require_paired(tx, &server_for_read)?;
                }
                let replayed = replay_delivery(
                    tx,
                    Direction::Outgoing,
                    &server_for_read,
                    &envelope_for_read,
                    now,
                )?;
                let key = pending_key(
                    tx,
                    Direction::Outgoing,
                    &server_for_read,
                    &envelope_for_read,
                )?;
                let row = tx
                    .query_row(
                        "SELECT server,body,fingerprint,id FROM runtime_server_outbox WHERE id=?1",
                        [key],
                        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                    )
                    .optional()?;
                Ok::<_, AccessError>((replayed, row))
            })
            .await?;
        if let Some(result) = replayed {
            return Ok(Some(SavedExecReceipt::Settled(result)));
        }
        let Some((saved_server, body, fingerprint, key)) = row else {
            return Ok(None);
        };
        let saved: Value = serde_json::from_str(&body).map_err(|_| {
            refused(
                503,
                "invalid_record",
                "Stored cross-server envelope is invalid",
            )
        })?;
        let Some(probe) = exec_receipt_probe(&envelope.root, &saved["payload"]) else {
            return Ok(None);
        };
        if saved_server != server || (body != envelope.body && fingerprint != envelope.fingerprint)
        {
            return Err(refused(
                409,
                "request_conflict",
                "This request id has different content",
            ));
        }
        Ok(Some(SavedExecReceipt::Pending(probe, key, fingerprint)))
    }

    /// Send the effect-free `exec_receipt` probe for one `exec` row in receipt
    /// form. An applied probe carries the original receipt; any other reply
    /// keeps the command outcome unknown and schedules another probe.
    async fn deliver_exec_receipt(
        &self,
        server: &str,
        local_server: Option<&str>,
        envelope: &Envelope,
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
                .and_then(|value| ExchangeOutcome::from_reply(value, probe_key, now)),
        };
        let result = match reply {
            Ok(ExchangeOutcome::Applied(receipt)) => {
                serde_json::from_str::<RemoteReply>(receipt.value.get())
                    .map_err(|_| invalid_receipt())
                    .and_then(|reply| ExchangeOutcome::from_reply(reply, &envelope.root, now))
                    .unwrap_or(ExchangeOutcome::Unknown)
            }
            _ => ExchangeOutcome::Unknown,
        };
        let envelope = envelope.clone();
        let key = envelope.key.clone();
        let root = envelope.root.clone();
        let server = server.to_owned();
        self.store
            .transaction_with(move |tx| {
                if let Some(saved) =
                    replay_delivery(tx, Direction::Outgoing, &server, &envelope, now)?
                {
                    return Ok(saved);
                }
                let row: Option<(ServerDeliveryState, i64)> = tx
                    .query_row(
                        "SELECT state,attempts FROM runtime_server_outbox WHERE id=?1",
                        [&key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let (state, attempts) = row.ok_or_else(|| {
                    refused(409, "receipt_missing", "The delivery claim is missing")
                })?;
                settle_exec_row(tx, &key, &root, state, attempts, &result, now)?;
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
        let root = envelope.root.clone();
        let fingerprint = envelope.fingerprint.clone();
        let server = server.to_owned();
        self.store
            .transaction_with(move |tx| {
                let snapshot = outbox(tx, &key)?.ok_or_else(|| {
                    refused(409, "receipt_missing", "The delivery claim is missing")
                })?;
                if snapshot.signature != fingerprint
                    || snapshot.server.as_deref() != Some(server.as_str())
                    || snapshot.state != ServerDeliveryState::Running
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
                settle_exec_row(tx, &key,&root, ServerDeliveryState::Queued, attempts, &result, now)?;
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
        let envelope = Envelope::from_value(value)?.scoped(Direction::Incoming, principal);
        if envelope.action.as_str() == EXEC_RECEIPT_ACTION {
            return self
                .receive_exec_receipt(principal, is_local, envelope, now)
                .await;
        }
        if !self.effects.supports(envelope.action.as_str()) {
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
        let (decision, replaying_claim, claim_key) = self.store.transaction_with(move |tx| {
            if !is_local {
                require_paired(tx, &principal_for_claim)?;
            }
            let key = pending_key(tx,Direction::Incoming,&principal_for_claim,&envelope_for_claim)?;
            if let Some(result) = replay_delivery(tx,Direction::Incoming,&principal_for_claim,&envelope_for_claim,now)? {
                return Ok((DeliveryDecision::Return(result), false, key));
            }
            let clock=chrono::Utc::now().timestamp_millis();
            let identity=studio_runtime_schema::persistence::RequestIdentity::new(Direction::Incoming.namespace(),&[&principal_for_claim],&envelope_for_claim.root,&envelope_for_claim.operation_parts())
                .map_err(|_|replay_error(RequestResultError::Expired))?;
            identity.validate(clock,studio_runtime_store::request_results::RequestResults::expired_before_ms(tx).map_err(replay_error)?).map_err(|error|replay_error(error.into()))?;
            let saved = inbox(tx, &key)?;
            let replaying_claim = saved.is_some();
            let decision = decide_delivery(
                saved.as_ref(),
                &signature_for_claim,
                None,
                false,
                envelope_for_claim.action.as_str(),
                now,
            )?;
            if matches!(decision, DeliveryDecision::Return(ExchangeOutcome::Unknown))
                && saved.as_ref().is_some_and(|snapshot| {
                    snapshot.state == ServerDeliveryState::Running
                        && snapshot.signature == signature_for_claim
                        && snapshot.server.is_none()
                })
                && let Some(receipt) = durable_anchor_input_receipt(tx, &principal_for_claim, &envelope_for_claim)?
            {
                let result = ExchangeOutcome::Applied(AppliedReply::new(receipt));
                let stored_result = serde_json::to_string(&result.to_reply(&envelope_for_claim.root)).map_err(|_| invalid_receipt())?;
                if stored_result.len() > MAX_RESPONSE_BYTES {
                    return Ok((
                        DeliveryDecision::Return(ExchangeOutcome::Unknown),
                        replaying_claim,
                        key,
                    ));
                }
                settle_delivery(tx,Direction::Incoming,&key,&envelope_for_claim.root,&signature_for_claim,&result)?;
                return Ok((DeliveryDecision::Return(result), replaying_claim,key));
            }
            if matches!(decision, DeliveryDecision::Execute) {
                if saved.is_some() {
                    tx.execute(
                        "UPDATE runtime_server_inbox SET state=?2 WHERE id=?1 AND state IN (?3,?2)",
                        params![&key, ServerDeliveryState::Running,ServerDeliveryState::Waiting],
                    )?;
                } else {
                    let admitted = admit_delivery(tx,Direction::Incoming,&principal_for_claim,&envelope_for_claim,now)?;
                    let admission = serde_json::to_string(&admitted).map_err(|_| refused(503,"invalid_record","Delivery admission could not be saved"))?;
                    tx.execute(
                        "INSERT INTO runtime_server_inbox(id,signature,state,result,created_at,principal,actor,admission) VALUES(?1,?2,?7,NULL,?3,?4,?5,?6)",
                        params![key, signature_for_claim, now, principal_for_claim, envelope_for_claim.payload.get("actor").and_then(Value::as_str),admission,ServerDeliveryState::Running],
                    )?;
                }
            }
            Ok::<_, AccessError>((decision, replaying_claim,key))
        }).await?;
        if let DeliveryDecision::Return(result) = decision {
            return Ok(result);
        }
        let mut result = match self.effects.execute(principal, &envelope.value()).await {
            Ok(result) => result,
            Err(error)
                if envelope.action.as_str() == "browser_callback" && error.status() < 500 =>
            {
                ExchangeOutcome::NotApplied(error.to_string())
            }
            Err(error)
                if envelope.action.as_str() == "input"
                    && !replaying_claim
                    && error.status() < 500 =>
            {
                // Input refusals are validated before the durable write; keep
                // fresh permanent 4xx failures from retrying forever as unknown.
                ExchangeOutcome::NotApplied(error.to_string())
            }
            Err(_) => ExchangeOutcome::Unknown,
        };
        if envelope.action.as_str() == "input"
            && replaying_claim
            && matches!(result, ExchangeOutcome::NotApplied(_))
        {
            // A prior claim may already have committed the durable input. Do
            // not turn a later 4xx (or stale-epoch refusal) into a false
            // negative receipt after response loss.
            result = ExchangeOutcome::Unknown;
        }
        if serde_json::to_string(&result.to_reply(&envelope.root))
            .map_err(|_| invalid_receipt())?
            .len()
            > MAX_RESPONSE_BYTES
        {
            self.finish(
                Direction::Incoming,
                &claim_key,
                &envelope.root,
                envelope.action,
                &signature,
                None,
                &ExchangeOutcome::Unknown,
                now,
            )
            .await?;
            return Ok(ExchangeOutcome::Unknown);
        }
        self.finish(
            Direction::Incoming,
            &claim_key,
            &envelope.root,
            envelope.action,
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
        let principal = principal.to_owned();
        self.store
            .transaction_with(move |tx| {
                if !is_local {
                    require_paired(tx, &principal)?;
                }
                let Some(answer) = exec_receipt_answer(tx, &principal, &envelope.payload, now)?
                else {
                    return Ok(ExchangeOutcome::Unknown);
                };
                Ok(match answer {
                    Ok(receipt) => applied(&receipt)?,
                    Err(refusal) => ExchangeOutcome::NotApplied(refusal.to_owned()),
                })
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
                        row.get::<_, ServerDeliveryState>(2)?,
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
            if state == ServerDeliveryState::Running && !outgoing_retry(envelope.action) {
                continue;
            }
            // One row that cannot be delivered must not hold back the rows after it
            // (Python delivers each queued row on its own).
            match self
                .exchange_operation(&server, &envelope.value(), now)
                .await
            {
                Ok(_) => count += 1,
                Err(error) => {
                    eprintln!(
                        "orchestration delivery of {} ({}) failed: {error}",
                        envelope.key,
                        envelope.action.as_str()
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
        direction: Direction,
        key: &str,
        root: &str,
        action: ServerOperationKind,
        signature: &str,
        server: Option<&str>,
        result: &ExchangeOutcome,
        now: i64,
    ) -> Result<(), AccessError> {
        if matches!(
            result,
            ExchangeOutcome::Unknown | ExchangeOutcome::PreEffectWaiting
        ) {
            if outgoing_retry(action) && direction == Direction::Outgoing {
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
                    tx.execute("UPDATE runtime_server_outbox SET state=?3,attempts=attempts+1,next_at=?1,error='unknown' WHERE id=?2 AND state=?4",params![now.saturating_add(retry_delay),key_for_update,ServerDeliveryState::Queued,ServerDeliveryState::Running])?;
                    Ok::<_,AccessError>(())
                }).await?;
            // Generic unknown input stays running: only a pre-effect proof permits retry.
            } else if direction == Direction::Incoming
                && if action == ServerOperationKind::Input {
                    *result == ExchangeOutcome::PreEffectWaiting
                } else {
                    peer_replay_safe(action)
                }
            {
                let key_for_update = key.to_owned();
                self.store
                    .transaction_with(move |tx| {
                        tx.execute(
                            "UPDATE runtime_server_inbox SET state=?2 WHERE id=?1 AND state=?3",
                            params![
                                key_for_update,
                                ServerDeliveryState::Waiting,
                                ServerDeliveryState::Running
                            ],
                        )?;
                        Ok::<_, AccessError>(())
                    })
                    .await?;
            }
            return Ok(());
        }
        let key = key.to_owned();
        let root = root.to_owned();
        let signature = signature.to_owned();
        let server = server.map(str::to_owned);
        let result = result.clone();
        self.store
            .transaction_with(move |tx| {
                let saved = if direction == Direction::Outgoing {
                    outbox(tx, &key)?
                } else {
                    inbox(tx, &key)?
                };
                let snapshot = saved.ok_or_else(|| {
                    refused(409, "receipt_missing", "The delivery claim is missing")
                })?;
                if snapshot.signature != signature
                    || snapshot.server.as_deref() != server.as_deref()
                    || snapshot.state != ServerDeliveryState::Running
                {
                    return Err(refused(
                        409,
                        "stale_receipt",
                        "The delivery receipt does not match its claim",
                    ));
                }
                settle_delivery(tx, direction, &key, &root, &signature, &result)?;
                Ok(())
            })
            .await
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
fn outbox(tx: &Transaction<'_>, key: &str) -> Result<Option<DeliverySnapshot>, AccessError> {
    Ok(tx
        .query_row(
            "SELECT state,fingerprint,server,next_at FROM runtime_server_outbox WHERE id=?1",
            [key],
            DeliverySnapshot::from_row,
        )
        .optional()?)
}
fn inbox(tx: &Transaction<'_>, key: &str) -> Result<Option<DeliverySnapshot>, AccessError> {
    Ok(tx
        .query_row(
            "SELECT state,signature,NULL,0 FROM runtime_server_inbox WHERE id=?1",
            [key],
            DeliverySnapshot::from_row,
        )
        .optional()?)
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
    struct CountEffects(std::sync::atomic::AtomicUsize);
    impl super::OrchestrationEffects for CountEffects {
        fn supports(&self, _: &str) -> bool {
            true
        }
        fn execute<'a>(
            &'a self,
            principal: &'a str,
            envelope: &'a serde_json::Value,
        ) -> futures_util::future::BoxFuture<
            'a,
            Result<super::ExchangeOutcome, crate::multi_server_auth::AccessError>,
        > {
            Box::pin(async move {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                if envelope["payload"]["unknown"] == true {
                    return Ok(super::ExchangeOutcome::Unknown);
                }
                super::applied(
                    &serde_json::json!({"owner":principal,"root":envelope["requestId"],"operation":envelope["operation"]}),
                )
            })
        }
    }

    #[tokio::test]
    async fn completed_location_and_transport_reply_with_the_same_source_id_replay_separately() {
        use studio_runtime_store::receipt_migration_contract::import_result;
        use studio_runtime_store::request_results::ResultEnvelope;
        let directory = tempfile::tempdir().expect("state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        let root =
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"completed-location").to_string();
        let envelope = super::Envelope::from_value(&serde_json::json!({"requestId":root,"operation":[],"action":"add_location","payload":{"actor":"lead","project":"project","path":"/peer"}})).expect("source envelope");
        let canonical =
            super::remote_operation_key("server-outbox", &["peer"], &root, &["add_location"]);
        let imported_root = root.clone();
        store.transaction(move|tx| {
            let peer=serde_json::json!({"id":"peer","clientId":"peer","kind":"server","label":"peer","created":1,"publicKey":"key","origin":"https://peer.invalid","status":"paired"});
            tx.execute("INSERT INTO runtime_access_clients(id,record) VALUES('peer',?1)",[peer.to_string()])?;
            let created=chrono::Utc::now().timestamp_millis();
            let public=crate::project_locations::LocationResult{request_id:imported_root.clone(),outcome:studio_runtime_schema::persistence::RemoteOutcome::Applied,project_id:Some("project".into()),value:None,error:None};
            import_result(tx,&imported_root,"public-location-signature",&ResultEnvelope{actor:Some("lead".into()),reply:public,references:Vec::new()},created).expect("public result");
            let remote=super::SavedRemoteReply{principal:None,server:Some("peer".into()),actor:Some("lead".into()),body:Some(envelope.body),response:super::RemoteReply{request_id:imported_root,outcome:studio_runtime_schema::persistence::RemoteOutcome::Applied,value:Some(serde_json::value::to_raw_value(&serde_json::json!({"transport":"exact"})).expect("opaque payload")),error:None,waiting_for:None}};
            import_result(tx,&canonical,&envelope.fingerprint,&ResultEnvelope{actor:Some("lead".into()),reply:remote,references:Vec::new()},created).expect("transport result");
            Ok(())
        }).await.expect("classified imports");
        let effects = Arc::new(CountEffects(std::sync::atomic::AtomicUsize::new(0)));
        let driver = super::OrchestrationDriver::new(
            store.clone(),
            Arc::new(super::HttpsPairedTransport::new(store.clone())),
            effects.clone(),
        );
        for _ in 0..2 {
            assert_eq!(
                driver
                    .exchange(
                        "peer",
                        "add_location",
                        &serde_json::json!({"actor":"lead","project":"project","path":"/peer"}),
                        &root,
                        chrono::Utc::now().timestamp()
                    )
                    .await
                    .expect("only canonical transport replay"),
                super::applied(&serde_json::json!({"transport":"exact"})).expect("reply")
            );
        }
        assert_eq!(effects.0.load(std::sync::atomic::Ordering::SeqCst), 0);
        assert_eq!(
            store
                .test_reader()
                .expect("reader")
                .query_row("SELECT count(*) FROM runtime_server_outbox", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .expect("pending count"),
            0
        );
    }

    #[tokio::test]
    async fn same_request_from_two_peers_replays_within_each_principal() {
        let directory = tempfile::tempdir().expect("state directory");
        let store = std::sync::Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(directory.path()))
                .await
                .expect("store"),
        );
        store.transaction(|tx| {
            for peer in ["peer-a", "peer-b"] {
                let record = serde_json::json!({"id":peer,"clientId":peer,"kind":"server","label":peer,"created":1,"publicKey":"key","origin":"https://peer.invalid","status":"paired"});
                tx.execute("INSERT INTO runtime_access_clients(id,record) VALUES(?1,?2)", rusqlite::params![peer,record.to_string()])?;
            }
            Ok(())
        }).await.expect("paired peers");
        let effects = std::sync::Arc::new(CountEffects(std::sync::atomic::AtomicUsize::new(0)));
        let driver = super::OrchestrationDriver::new(
            store.clone(),
            std::sync::Arc::new(super::HttpsPairedTransport::new(store.clone())),
            effects.clone(),
        );
        let root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let request =
            serde_json::json!({"requestId":root,"operation":[],"action":"projects","payload":{}});
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock")
            .as_secs() as i64;
        for peer in ["peer-a", "peer-b"] {
            assert_eq!(
                driver
                    .receive(peer, &request, now)
                    .await
                    .expect("scoped reply"),
                super::applied(&serde_json::json!({"owner":peer,"root":root,"operation":[]}))
                    .expect("reply")
            );
        }
        assert_eq!(effects.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        drop(driver);
        drop(store);
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("restart"),
        );
        let driver = super::OrchestrationDriver::new(
            store.clone(),
            Arc::new(super::HttpsPairedTransport::new(store.clone())),
            effects.clone(),
        );
        for peer in ["peer-a", "peer-b"] {
            assert_eq!(
                driver
                    .receive(peer, &request, now)
                    .await
                    .expect("original reply after restart"),
                super::applied(&serde_json::json!({"owner":peer,"root":root,"operation":[]}))
                    .expect("reply")
            );
        }
        assert_eq!(effects.0.load(std::sync::atomic::Ordering::SeqCst), 2);
        for offset in ["0", "0", "4096", "4096"] {
            let chunk = serde_json::json!({"requestId":root,"operation":[offset],"action":"chunk","payload":{"offset":offset}});
            assert_eq!(
                driver
                    .receive("peer-a", &chunk, now)
                    .await
                    .expect("chunk reply"),
                super::applied(
                    &serde_json::json!({"owner":"peer-a","root":root,"operation":[offset]})
                )
                .expect("reply")
            );
        }
        assert_eq!(
            effects.0.load(std::sync::atomic::Ordering::SeqCst),
            4,
            "same-offset chunk retries replay; a new offset executes once"
        );
        let conflict = serde_json::json!({"requestId":root,"operation":["0"],"action":"chunk","payload":{"offset":"different"}});
        assert_eq!(
            driver
                .receive("peer-a", &conflict, now)
                .await
                .expect_err("content conflicts")
                .status(),
            409
        );
        let unknown_root = studio_runtime_schema::persistence::RequestIdentity::issue_root();
        let unknown = serde_json::json!({"requestId":unknown_root,"operation":[],"action":"input","payload":{"unknown":true}});
        for _ in 0..2 {
            assert_eq!(
                driver
                    .receive("peer-a", &unknown, now)
                    .await
                    .expect("unknown held"),
                super::ExchangeOutcome::Unknown
            );
        }
        assert_eq!(effects.0.load(std::sync::atomic::Ordering::SeqCst), 5);
        let pending = store
            .test_reader()
            .expect("reader")
            .query_row("SELECT count(*) FROM runtime_server_inbox", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("pending rows");
        assert_eq!(pending, 1, "only unresolved obligations remain");
        let legacy =
            uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, b"retained-peer-request").to_string();
        let retained =
            serde_json::json!({"requestId":legacy,"operation":[],"action":"projects","payload":{}});
        let signature = super::Envelope::from_value(&retained)
            .expect("retained envelope")
            .incoming_signature("peer-a")
            .expect("signature");
        let legacy_for_import = legacy.clone();
        store
            .transaction(move |tx| {
                let reply = studio_runtime_store::multi_server_delivery::SavedRemoteReply {
                    principal: Some("peer-a".into()),
                    server: None,
                    actor: None,
                    body: None,
                    response: studio_runtime_store::multi_server_delivery::RemoteReply {
                        request_id: legacy_for_import.clone(),
                        outcome: studio_runtime_schema::persistence::RemoteOutcome::Applied,
                        value: Some(
                            serde_json::value::to_raw_value(&serde_json::json!({"retained":true}))
                                .expect("original reply"),
                        ),
                        error: None,
                        waiting_for: None,
                    },
                };
                studio_runtime_store::receipt_migration_contract::import_result(
                    tx,
                    &legacy_for_import,
                    &signature,
                    &studio_runtime_store::request_results::ResultEnvelope {
                        actor: None,
                        reply,
                        references: Vec::new(),
                    },
                    chrono::Utc::now().timestamp_millis(),
                )
                .expect("classified import");
                Ok(())
            })
            .await
            .expect("retained result");
        assert_eq!(
            driver
                .receive("peer-a", &retained, now)
                .await
                .expect("retained v5 replay"),
            super::applied(&serde_json::json!({"retained":true})).expect("reply")
        );
        let mut changed = retained.clone();
        changed["payload"] = serde_json::json!({"changedAfterCutover":true});
        assert_eq!(
            driver
                .receive("peer-a", &changed, now)
                .await
                .expect_err("changed imported fingerprint never executes")
                .status(),
            409
        );
        let missing = serde_json::json!({"requestId":uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID,b"missing-peer-request").to_string(),"operation":[],"action":"projects","payload":{}});
        assert_eq!(
            driver
                .receive("peer-a", &missing, now)
                .await
                .expect_err("unretained v5 expired")
                .status(),
            410
        );
        let missing_for_import = missing.clone();
        store.transaction(move|tx|{
            let envelope=super::Envelope::from_value(&missing_for_import).expect("pending source envelope");
            let signature=envelope.incoming_signature("peer-a").expect("source signature");
            let token=serde_json::json!({"id":envelope.root,"signature":signature,"created":chrono::Utc::now().timestamp_millis()});
            tx.execute("INSERT INTO runtime_server_inbox(id,signature,state,result,created_at,principal,actor,admission) VALUES(?1,?2,?3,NULL,?4,'peer-a',NULL,?5)",rusqlite::params![envelope.root,signature,studio_runtime_schema::persistence::ServerDeliveryState::Waiting,now,token.to_string()])?;
            Ok(())
        }).await.expect("classified pending source");
        assert_eq!(
            driver
                .receive("peer-a", &missing, now)
                .await
                .expect_err("pending v5 never starts another effect")
                .status(),
            410
        );
        assert_eq!(
            effects.0.load(std::sync::atomic::Ordering::SeqCst),
            5,
            "v5 cache hit and miss both execute no effect"
        );
        let root_for_expiry = root.clone();
        store.transaction(move|tx|{tx.execute("UPDATE request_results SET created=?1 WHERE json_extract(result,'$.reply.response.requestId')=?2",rusqlite::params![chrono::Utc::now().timestamp_millis()-studio_runtime_schema::persistence::REQUEST_RESULT_TTL_MS-1,root_for_expiry])?;Ok(())}).await.expect("expired cache fixture");
        assert_eq!(
            driver
                .receive("peer-a", &request, now)
                .await
                .expect_err("expired reply never resends")
                .status(),
            410
        );
        assert_eq!(effects.0.load(std::sync::atomic::Ordering::SeqCst), 5);
    }

    use super::{
        DeliveryDecision, DeliverySnapshot, ExchangeOutcome, HttpsPairedTransport, decide_delivery,
        outbox_next_due_at, peer_retry_delay,
    };
    use rusqlite::params;
    use std::sync::Arc;
    use studio_runtime_store::RuntimeStore;

    #[test]
    fn expired_applied_reply_is_unknown() {
        let reply: super::RemoteReply = serde_json::from_str(
            r#"{"requestId":"r","outcome":"applied","value":{"outputExpiresAt":10}}"#,
        )
        .expect("reply");
        assert!(matches!(
            ExchangeOutcome::from_reply(reply, "r", 10),
            Ok(ExchangeOutcome::Unknown)
        ));
    }

    #[test]
    fn applied_metadata_follows_nested_results_and_counts_null_presence() {
        let raw = serde_json::value::RawValue::from_string(
            r#"{"outputExpiresAt":"ignored","value":{"result":{"handle":null,"outputExpiresAt":25}}}"#.into(),
        ).expect("payload");
        let reply = super::AppliedReply::new(raw);
        assert_eq!(reply.output_expires_at, Some(25.0));
        assert!(reply.handle);
        assert!(!reply.output_ref);
    }

    fn saved(state: &str, next_at: f64) -> DeliverySnapshot {
        DeliverySnapshot {
            state: <studio_runtime_schema::persistence::ServerDeliveryState as serde::Deserialize>::deserialize(serde::de::value::StrDeserializer::<serde_json::Error>::new(state)).expect("state"),
            signature: "same-content".to_owned(),
            server: Some("peer".to_owned()),
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

    /// The Python puller of a paired server (docs/peer-updates.md) asks this
    /// server for its build and archive. Without these actions the paired server
    /// never gets an offer, keeps another API schema, and its projects stay
    /// hidden in the shared project list.
    #[cfg(unix)]
    #[tokio::test]
    async fn paired_server_pulls_the_installed_app_and_replays_exact_chunk_receipts() {
        use super::{OrchestrationDriver, RuntimeOrchestrationEffects};
        use base64::Engine;
        use serde_json::json;

        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = Arc::new(
            RuntimeStore::open(Some(directory.path()))
                .await
                .expect("open store"),
        );
        let record = json!({
            "id": "peer-a", "clientId": "peer-a", "kind": "server", "label": "Peer",
            "created": 1, "publicKey": "public-key", "origin": "https://peer.invalid",
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
        let tools = tempfile::tempdir().expect("fixture app folder");
        let offer = crate::peer_updates::fixture::offer(tools.path(), b"app archive");
        let driver = OrchestrationDriver::new(
            Arc::clone(&store),
            Arc::new(HttpsPairedTransport::new(Arc::clone(&store))),
            Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&store)).with_update_offer(offer)),
        );
        let request = |_: &str, action: &str, payload: serde_json::Value| json!({"requestId": studio_runtime_schema::persistence::RequestIdentity::issue_root(), "operation":[], "action": action, "payload": payload});

        let offered = driver
            .receive(
                "peer-a",
                &request("offer-1", "update.offer", json!({})),
                100,
            )
            .await
            .expect("offer receipt");
        assert_eq!(
            offered,
            super::applied(&json!({"build": crate::peer_updates::fixture::build()}))
                .expect("reply")
        );
        let other = driver
            .receive(
                "peer-a",
                &request("export-0", "update.export", json!({"builtAt": 1.5})),
                100,
            )
            .await
            .expect("refused export receipt");
        assert_eq!(
            other,
            ExchangeOutcome::NotApplied("This server runs another build".into())
        );
        let ExchangeOutcome::Applied(meta) = driver
            .receive(
                "peer-a",
                &request(
                    "export-1",
                    "update.export",
                    json!({"builtAt": 1792647971.037}),
                ),
                101,
            )
            .await
            .expect("export receipt")
        else {
            panic!("the export is applied");
        };
        let meta: serde_json::Value =
            serde_json::from_str(meta.value.get()).expect("export metadata");
        assert_eq!(meta["size"], json!(b"app archive".len()));
        let chunk = request(
            "chunk-1",
            "update.chunk",
            json!({"export": meta["export"], "offset": 0}),
        );
        for now in [102, 103] {
            let ExchangeOutcome::Applied(value) = driver
                .receive("peer-a", &chunk, now)
                .await
                .expect("chunk receipt")
            else {
                panic!("the chunk is applied");
            };
            let value: serde_json::Value =
                serde_json::from_str(value.value.get()).expect("chunk payload");
            let data = base64::engine::general_purpose::STANDARD
                .decode(value["data"].as_str().expect("chunk data"))
                .expect("base64 chunk");
            assert_eq!(
                data, b"app archive",
                "a replay returns the exact saved chunk"
            );
        }
        let saved: String = store
            .test_reader()
            .expect("test reader")
            .query_row(
                "SELECT result FROM request_results WHERE id=?1",
                [super::remote_operation_key(
                    "server-inbox",
                    &["peer-a"],
                    chunk["requestId"].as_str().unwrap(),
                    &["update.chunk"],
                )],
                |row| row.get(0),
            )
            .expect("stored chunk receipt");
        let saved: serde_json::Value = serde_json::from_str(&saved).expect("receipt JSON");
        assert_eq!(saved["reply"]["response"]["outcome"], "applied");
        assert!(saved["reply"]["response"]["value"]["data"].is_string());
        assert_eq!(
            saved["reply"]["response"]["value"]["nextOffset"],
            json!(b"app archive".len())
        );
        assert_eq!(
            store
                .test_reader()
                .unwrap()
                .query_row("SELECT count(*) FROM runtime_server_inbox", [], |row| row
                    .get::<_, i64>(
                    0
                ))
                .unwrap(),
            0
        );
        assert!(
            driver
                .receive(
                    "stranger",
                    &request("offer-2", "update.offer", json!({})),
                    104
                )
                .await
                .is_err(),
            "only a paired server gets an offer"
        );
    }
}
