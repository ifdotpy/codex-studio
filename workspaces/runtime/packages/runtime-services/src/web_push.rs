//! Web Push of sign-in alerts to the owner's phone (docs/web-push.md).
//!
//! The home-screen web app of a phone subscribes with the standard Push API. The server keeps
//! one VAPID key (RFC 8292) and the subscriptions in `<state>/web-push`, both owner-only. When
//! a sign-in card is still pending after the alert grace, the server sends one encrypted push
//! (RFC 8291, aes128gcm) to every subscription, and one reminder before the card closes. The
//! payload holds only the alert text: no link, token or code. A push service that answers 404
//! or 410 ended the subscription, so the server removes it.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use base64::Engine as _;
use ring::{aead, agreement, hkdf, rand, signature};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use studio_runtime_schema::shared_values::{SIGN_IN_ALERT_GRACE_SECONDS, SIGN_IN_REMINDER_SECONDS};

/// The folder in the server state directory.
pub const FOLDER: &str = "web-push";
/// The server's VAPID key, PKCS#8, mode 0600.
pub const KEY_FILE: &str = "vapid.pkcs8";
/// The subscriptions of the owner's devices, mode 0600.
pub const SUBSCRIPTIONS_FILE: &str = "subscriptions.json";
/// The alerts already pushed, by card, so a restart does not push them again.
const SENT_FILE: &str = "sent.json";
/// The record size of the one aes128gcm record, in bytes (RFC 8188).
const RECORD_SIZE: u32 = 4096;
/// A VAPID token lives this long, in seconds (RFC 8292 allows at most 24 hours).
const VAPID_TOKEN_SECONDS: u64 = 12 * 3600;
/// A push that the phone did not receive in this time is dropped (`TTL`), in seconds.
const PUSH_TTL_SECONDS: u64 = 600;
/// One push request may take this long.
const PUSH_TIMEOUT: Duration = Duration::from_secs(10);
/// The longest push endpoint, in characters.
const ENDPOINT_LIMIT: usize = 2048;
/// The most subscriptions that one server keeps.
const SUBSCRIPTION_LIMIT: usize = 32;
/// The longest device name, in characters.
const DEVICE_LIMIT: usize = 80;
/// The sign-in check runs this often.
const CHECK_SECONDS: u64 = 5;

#[derive(Debug, thiserror::Error)]
pub enum WebPushError {
    #[error("{0}")]
    Invalid(String),
    #[error("Web Push crypto failed")]
    Crypto,
    #[error(transparent)]
    Io(#[from] std::io::Error),
}

fn invalid<T>(message: impl Into<String>) -> Result<T, WebPushError> {
    Err(WebPushError::Invalid(message.into()))
}

fn base64url(data: &[u8]) -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(data)
}

fn unbase64url(text: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(text.trim_end_matches('='))
        .ok()
}

fn private_folder(path: &Path) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(path)?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
}

/// Write a file of the owner only (0600) in one rename.
fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let folder = path.parent().unwrap_or(Path::new("."));
    private_folder(folder)?;
    let mut file = tempfile::Builder::new()
        .prefix(".web-push-")
        .tempfile_in(folder)?;
    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))?;
    file.write_all(bytes)?;
    file.as_file().sync_all()?;
    file.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// The server's VAPID key pair (ES256, P-256).
pub struct ServerKey {
    pair: signature::EcdsaKeyPair,
    public: Vec<u8>,
}

impl ServerKey {
    /// Read `<state>/web-push/vapid.pkcs8`, or make the key once. The key never leaves the file
    /// and the process; only its public half is served.
    pub fn load_or_create(state: &Path) -> Result<Self, WebPushError> {
        let folder = state.join(FOLDER);
        private_folder(&folder)?;
        let path = folder.join(KEY_FILE);
        let rng = rand::SystemRandom::new();
        let pkcs8 = match std::fs::read(&path) {
            Ok(bytes) => bytes,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let document = signature::EcdsaKeyPair::generate_pkcs8(
                    &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
                    &rng,
                )
                .map_err(|_| WebPushError::Crypto)?;
                write_private(&path, document.as_ref())?;
                document.as_ref().to_vec()
            }
            Err(error) => return Err(error.into()),
        };
        let pair = signature::EcdsaKeyPair::from_pkcs8(
            &signature::ECDSA_P256_SHA256_FIXED_SIGNING,
            &pkcs8,
            &rng,
        )
        .map_err(|_| WebPushError::Crypto)?;
        let public = signature::KeyPair::public_key(&pair).as_ref().to_vec();
        Ok(Self { pair, public })
    }

    /// The public key for `PushManager.subscribe` (`applicationServerKey`), base64url.
    pub fn public_key(&self) -> String {
        base64url(&self.public)
    }

    /// The `Authorization` header of one push (RFC 8292): a token for the endpoint's origin,
    /// signed with the server key, valid `VAPID_TOKEN_SECONDS` from `now`.
    pub fn authorization(
        &self,
        endpoint: &str,
        subject: &str,
        now: u64,
    ) -> Result<String, WebPushError> {
        let url = url::Url::parse(endpoint).map_err(|_| WebPushError::Crypto)?;
        let audience = url.origin().ascii_serialization();
        let head = base64url(br#"{"typ":"JWT","alg":"ES256"}"#);
        let claims = base64url(
            json!({"aud": audience, "exp": now + VAPID_TOKEN_SECONDS, "sub": subject})
                .to_string()
                .as_bytes(),
        );
        let signed = format!("{head}.{claims}");
        let rng = rand::SystemRandom::new();
        let signature = self
            .pair
            .sign(&rng, signed.as_bytes())
            .map_err(|_| WebPushError::Crypto)?;
        Ok(format!(
            "vapid t={signed}.{}, k={}",
            base64url(signature.as_ref()),
            self.public_key()
        ))
    }
}

struct Length(usize);
impl hkdf::KeyType for Length {
    fn len(&self) -> usize {
        self.0
    }
}

fn hkdf_bytes(
    salt: &[u8],
    ikm: &[u8],
    info: &[u8],
    length: usize,
) -> Result<Vec<u8>, WebPushError> {
    let prk = hkdf::Salt::new(hkdf::HKDF_SHA256, salt).extract(ikm);
    let mut out = vec![0u8; length];
    prk.expand(&[info], Length(length))
        .and_then(|okm| okm.fill(&mut out))
        .map_err(|_| WebPushError::Crypto)?;
    Ok(out)
}

/// Encrypt one payload for a subscription (RFC 8291 with RFC 8188 aes128gcm): one record with
/// a new server ECDH key and salt.
pub fn encrypt(payload: &[u8], p256dh: &[u8], auth: &[u8]) -> Result<Vec<u8>, WebPushError> {
    if payload.len() + 17 > RECORD_SIZE as usize {
        return invalid("The push payload is too large");
    }
    let rng = rand::SystemRandom::new();
    let private = agreement::EphemeralPrivateKey::generate(&agreement::ECDH_P256, &rng)
        .map_err(|_| WebPushError::Crypto)?;
    let server = private
        .compute_public_key()
        .map_err(|_| WebPushError::Crypto)?
        .as_ref()
        .to_vec();
    let secret = agreement::agree_ephemeral(
        private,
        &agreement::UnparsedPublicKey::new(&agreement::ECDH_P256, p256dh),
        <[u8]>::to_vec,
    )
    .map_err(|_| WebPushError::Crypto)?;
    let mut info = b"WebPush: info\0".to_vec();
    info.extend_from_slice(p256dh);
    info.extend_from_slice(&server);
    let ikm = hkdf_bytes(auth, &secret, &info, 32)?;
    let mut salt = [0u8; 16];
    rand::SecureRandom::fill(&rng, &mut salt).map_err(|_| WebPushError::Crypto)?;
    let key = hkdf_bytes(&salt, &ikm, b"Content-Encoding: aes128gcm\0", 16)?;
    let nonce = hkdf_bytes(&salt, &ikm, b"Content-Encoding: nonce\0", 12)?;
    let key = aead::LessSafeKey::new(
        aead::UnboundKey::new(&aead::AES_128_GCM, &key).map_err(|_| WebPushError::Crypto)?,
    );
    let mut record = payload.to_vec();
    // The delimiter of the last record (RFC 8188 section 2).
    record.push(2);
    key.seal_in_place_append_tag(
        aead::Nonce::try_assume_unique_for_key(&nonce).map_err(|_| WebPushError::Crypto)?,
        aead::Aad::empty(),
        &mut record,
    )
    .map_err(|_| WebPushError::Crypto)?;
    let mut body = salt.to_vec();
    body.extend_from_slice(&RECORD_SIZE.to_be_bytes());
    body.push(server.len() as u8);
    body.extend_from_slice(&server);
    body.extend_from_slice(&record);
    Ok(body)
}

/// A push endpoint of a public push service: https, a host name or public address, no user
/// part. `loopback_http` lets a test use a fake endpoint on the loopback.
pub fn push_endpoint(endpoint: &str, loopback_http: bool) -> Result<url::Url, WebPushError> {
    let refuse = || invalid("The push endpoint must be an https address of a push service");
    if endpoint.chars().count() > ENDPOINT_LIMIT {
        return refuse();
    }
    let Ok(url) = url::Url::parse(endpoint) else {
        return refuse();
    };
    if !url.username().is_empty() || url.password().is_some() {
        return refuse();
    }
    let host = url.host();
    let loopback = match host {
        Some(url::Host::Ipv4(address)) => address.is_loopback(),
        Some(url::Host::Ipv6(address)) => address.is_loopback(),
        Some(url::Host::Domain(name)) => name.eq_ignore_ascii_case("localhost"),
        None => return refuse(),
    };
    if loopback_http && loopback && url.scheme() == "http" {
        return Ok(url);
    }
    let private = match host {
        Some(url::Host::Ipv4(address)) => {
            address.is_loopback()
                || address.is_private()
                || address.is_link_local()
                || address.is_unspecified()
        }
        Some(url::Host::Ipv6(address)) => {
            address.is_loopback()
                || address.is_unspecified()
                || (address.segments()[0] & 0xfe00) == 0xfc00
        }
        _ => loopback,
    };
    if url.scheme() != "https" || private {
        return refuse();
    }
    Ok(url)
}

/// A subscription that a device sends (`PushSubscription.toJSON()` plus a device name).
#[derive(Clone, Debug, Deserialize)]
pub struct NewSubscription {
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub device: String,
}

/// One device of the owner that gets sign-in pushes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Subscription {
    pub id: String,
    pub endpoint: String,
    pub p256dh: String,
    pub auth: String,
    pub device: String,
    pub created: f64,
}

/// Serializes the read and write of the subscription file: the routes and the sender change it.
static SUBSCRIPTIONS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// The owner's subscriptions in `<state>/web-push/subscriptions.json`.
pub struct Subscriptions {
    path: PathBuf,
}

impl Subscriptions {
    pub fn new(state: &Path) -> Self {
        Self {
            path: state.join(FOLDER).join(SUBSCRIPTIONS_FILE),
        }
    }

    pub fn list(&self) -> Result<Vec<Subscription>, WebPushError> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| WebPushError::Invalid("The subscription file is invalid".into())),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(error.into()),
        }
    }

    fn save(&self, rows: &[Subscription]) -> Result<(), WebPushError> {
        let text = serde_json::to_vec(rows).map_err(|_| WebPushError::Crypto)?;
        write_private(&self.path, &text)?;
        Ok(())
    }

    /// Keep a device's subscription; the same endpoint keeps its first ID.
    pub fn add(
        &self,
        new: NewSubscription,
        loopback_http: bool,
        now: f64,
    ) -> Result<Subscription, WebPushError> {
        push_endpoint(&new.endpoint, loopback_http)?;
        let point = unbase64url(&new.p256dh).filter(|key| key.len() == 65 && key[0] == 4);
        let secret = unbase64url(&new.auth).filter(|secret| secret.len() == 16);
        if point.is_none() || secret.is_none() {
            return invalid("The push subscription keys are invalid");
        }
        let device: String = new.device.trim().chars().take(DEVICE_LIMIT).collect();
        let _guard = SUBSCRIPTIONS_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut rows = self.list()?;
        if let Some(row) = rows.iter_mut().find(|row| row.endpoint == new.endpoint) {
            row.p256dh = new.p256dh;
            row.auth = new.auth;
            row.device = device;
            let row = row.clone();
            self.save(&rows)?;
            return Ok(row);
        }
        if rows.len() >= SUBSCRIPTION_LIMIT {
            return invalid("This server has the most push subscriptions it keeps");
        }
        let row = Subscription {
            id: uuid::Uuid::new_v4().to_string(),
            endpoint: new.endpoint,
            p256dh: new.p256dh,
            auth: new.auth,
            device: if device.is_empty() {
                "Device".into()
            } else {
                device
            },
            created: now,
        };
        rows.push(row.clone());
        self.save(&rows)?;
        Ok(row)
    }

    /// Remove one subscription; returns whether it existed.
    pub fn remove(&self, id: &str) -> Result<bool, WebPushError> {
        let _guard = SUBSCRIPTIONS_LOCK
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        let mut rows = self.list()?;
        let before = rows.len();
        rows.retain(|row| row.id != id);
        if rows.len() == before {
            return Ok(false);
        }
        self.save(&rows)?;
        Ok(true)
    }
}

/// Sends one push request.
pub trait PushTransport: Send + Sync {
    /// POST the body to the endpoint; returns the HTTP status.
    fn post(
        &self,
        endpoint: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<u16, String>;
}

/// The push transport over https with the platform's root certificates.
pub struct UreqTransport {
    agent: ureq::Agent,
}

impl UreqTransport {
    pub fn new() -> Self {
        use ureq::tls::{RootCerts, TlsConfig, TlsProvider};
        let tls = TlsConfig::builder()
            .provider(TlsProvider::Rustls)
            .unversioned_rustls_crypto_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .root_certs(RootCerts::PlatformVerifier)
            .build();
        let config = ureq::Agent::config_builder()
            .https_only(true)
            .http_status_as_error(false)
            .max_redirects(0)
            .tls_config(tls)
            .timeout_global(Some(PUSH_TIMEOUT))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }

    /// A transport for a fake push endpoint on the loopback (http).
    #[cfg(test)]
    pub fn for_test_loopback() -> Self {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(PUSH_TIMEOUT))
            .build();
        Self {
            agent: ureq::Agent::new_with_config(config),
        }
    }
}

impl Default for UreqTransport {
    fn default() -> Self {
        Self::new()
    }
}

impl PushTransport for UreqTransport {
    fn post(
        &self,
        endpoint: &str,
        headers: &[(String, String)],
        body: &[u8],
    ) -> Result<u16, String> {
        let mut request = self.agent.post(endpoint);
        for (name, value) in headers {
            request = request.header(name.as_str(), value.as_str());
        }
        request
            .send(body)
            .map(|response| response.status().as_u16())
            .map_err(|error| error.to_string())
    }
}

/// A pending sign-in card, as the push needs it.
#[derive(Clone, Debug)]
pub struct DueCard {
    pub id: String,
    pub host: String,
    pub chat: String,
    pub created_at: f64,
    pub expires_at: Option<f64>,
    /// A phone sign-in waits for its pasted result: no reminder.
    pub waiting: bool,
}

/// What one card needs now: its alert, its reminder, or nothing.
fn due(card: &DueCard, now: f64) -> Vec<&'static str> {
    let alert_at = card.created_at + SIGN_IN_ALERT_GRACE_SECONDS as f64;
    let mut kinds = Vec::new();
    if now >= alert_at {
        kinds.push("alert");
    }
    if let Some(expires) = card.expires_at
        && !card.waiting
    {
        let reminder_at = expires - SIGN_IN_REMINDER_SECONDS as f64;
        if reminder_at > alert_at && now >= reminder_at && now <= expires {
            kinds.push("reminder");
        }
    }
    kinds
}

/// Sends the due pushes of the sign-in cards to every subscription.
pub struct Sender {
    pub state: PathBuf,
    pub key: ServerKey,
    /// The VAPID contact (`sub`): the server's https origin.
    pub subject: String,
}

impl Sender {
    fn sent_path(&self) -> PathBuf {
        self.state.join(FOLDER).join(SENT_FILE)
    }

    fn sent(&self) -> BTreeMap<String, BTreeSet<String>> {
        std::fs::read(self.sent_path())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .unwrap_or_default()
    }

    /// Push each alert that is due for the pending cards, once. A subscription that the push
    /// service ended (404 or 410) is removed. Cards that are no longer pending leave the record.
    pub fn deliver(
        &self,
        transport: &dyn PushTransport,
        cards: &[DueCard],
        now: f64,
    ) -> Result<(), WebPushError> {
        let mut sent = self.sent();
        let open: BTreeSet<&str> = cards.iter().map(|card| card.id.as_str()).collect();
        let before = sent.len();
        sent.retain(|card, _| open.contains(card.as_str()));
        let mut changed = sent.len() != before;
        let store = Subscriptions::new(&self.state);
        for card in cards {
            for kind in due(card, now) {
                if sent.get(&card.id).is_some_and(|kinds| kinds.contains(kind)) {
                    continue;
                }
                let (title, body) = if kind == "alert" {
                    ("Sign-in needed", format!("{} for {}", card.host, card.chat))
                } else {
                    (
                        "Sign-in card closes soon",
                        format!(
                            "{} for {}: about {} minutes left",
                            card.host,
                            card.chat,
                            SIGN_IN_REMINDER_SECONDS / 60
                        ),
                    )
                };
                let payload = json!({"title": title, "body": body,
                                     "tag": format!("studio-sign-in:{}", card.id)})
                .to_string();
                for subscription in store.list()? {
                    self.push(transport, &store, &subscription, payload.as_bytes(), now);
                }
                sent.entry(card.id.clone())
                    .or_default()
                    .insert(kind.to_owned());
                changed = true;
            }
        }
        if changed {
            write_private(
                &self.sent_path(),
                &serde_json::to_vec(&sent).map_err(|_| WebPushError::Crypto)?,
            )?;
        }
        Ok(())
    }

    fn push(
        &self,
        transport: &dyn PushTransport,
        store: &Subscriptions,
        subscription: &Subscription,
        payload: &[u8],
        now: f64,
    ) {
        let (Some(p256dh), Some(auth)) = (
            unbase64url(&subscription.p256dh),
            unbase64url(&subscription.auth),
        ) else {
            return;
        };
        let attempt = (|| {
            let body = encrypt(payload, &p256dh, &auth)?;
            let authorization =
                self.key
                    .authorization(&subscription.endpoint, &self.subject, now as u64)?;
            let headers = vec![
                ("Authorization".to_owned(), authorization),
                ("Content-Encoding".to_owned(), "aes128gcm".to_owned()),
                (
                    "Content-Type".to_owned(),
                    "application/octet-stream".to_owned(),
                ),
                ("TTL".to_owned(), PUSH_TTL_SECONDS.to_string()),
                ("Urgency".to_owned(), "high".to_owned()),
            ];
            Ok::<_, WebPushError>(transport.post(&subscription.endpoint, &headers, &body))
        })();
        match attempt {
            Ok(Ok(404 | 410)) => {
                if let Err(error) = store.remove(&subscription.id) {
                    eprintln!("web push: the ended subscription stays: {error}");
                }
            }
            Ok(Ok(status)) if (200..300).contains(&status) => {}
            // The device name only: the endpoint is a capability.
            Ok(Ok(status)) => {
                eprintln!("web push to {} failed: HTTP {status}", subscription.device)
            }
            Ok(Err(_)) | Err(_) => {
                eprintln!("web push to {} failed", subscription.device);
            }
        }
    }
}

/// The pending sign-in cards of the store, with their chat names. It reads the stored cards,
/// so a push does not wait for a client to receive the card.
pub async fn pending_cards(
    store: &studio_runtime_store::RuntimeStore,
) -> Result<Vec<DueCard>, studio_runtime_store::StoreError> {
    store
        .read_async("runtime-services/web_push", move |connection| {
            let mut statement = connection.prepare(
                "SELECT r.id, r.record, a.record FROM runtime_requests r \
                 LEFT JOIN runtime_agents a ON a.id = json_extract(r.record,'$.agent') \
                 WHERE json_extract(r.record,'$.method') = ?1 \
                 AND json_extract(r.record,'$.status') = 'pending'",
            )?;
            let rows = statement
                .query_map([crate::browser_cards::METHOD], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            Ok::<_, studio_runtime_store::StoreError>(
                rows.into_iter()
                    .filter_map(|(id, request, agent)| {
                        let request: Value = serde_json::from_str(&request).ok()?;
                        let agent: Value = agent
                            .and_then(|agent| serde_json::from_str(&agent).ok())
                            .unwrap_or(Value::Null);
                        if request.get("deferred").is_some_and(|value| value == true) {
                            return None;
                        }
                        let params = &request["params"];
                        Some(DueCard {
                            id,
                            host: params["host"]
                                .as_str()
                                .unwrap_or("a sign-in page")
                                .to_owned(),
                            chat: agent["name"].as_str().unwrap_or("a chat").to_owned(),
                            created_at: request["createdAt"].as_f64()?,
                            expires_at: params["expiresAt"].as_f64(),
                            waiting: params["phase"] == "waiting",
                        })
                    })
                    .collect(),
            )
        })
        .await
}

/// The VAPID contact of this server: its remote access origin (`remote-access.json`).
pub fn subject(state: &Path) -> Option<String> {
    let bytes =
        std::fs::read(state.join(studio_runtime_schema::multi_server_protocol::REMOTE_ACCESS_FILE))
            .ok()?;
    let value: Value = serde_json::from_slice(&bytes).ok()?;
    let origin = value["origin"].as_str()?;
    origin.starts_with("https://").then(|| origin.to_owned())
}

/// Push the due sign-in alerts until shutdown. Nothing runs while no device subscribed or
/// the server has no https remote access origin.
pub async fn run(
    store: Arc<studio_runtime_store::RuntimeStore>,
    state: PathBuf,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) {
    let transport: Arc<dyn PushTransport> = Arc::new(UreqTransport::new());
    loop {
        tokio::select! {
            _ = shutdown.wait_for(|stopped| *stopped) => return,
            () = tokio::time::sleep(Duration::from_secs(CHECK_SECONDS)) => {}
        }
        let subscribed = Subscriptions::new(&state)
            .list()
            .is_ok_and(|rows| !rows.is_empty());
        let Some(subject) = subject(&state).filter(|_| subscribed) else {
            continue;
        };
        let Ok(cards) = pending_cards(&store).await else {
            continue;
        };
        let (state, transport) = (state.clone(), Arc::clone(&transport));
        let result = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/web_push",
            move || {
                let sender = Sender {
                    key: ServerKey::load_or_create(&state)?,
                    state,
                    subject,
                };
                sender.deliver(transport.as_ref(), &cards, now_seconds())
            },
        )
        .await;
        if let Ok(Err(error)) = result {
            eprintln!("web push: {error}");
        }
    }
}

/// The clock of subscriptions and pushes, in epoch seconds.
pub fn now_seconds() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|value| value.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
#[path = "web_push_tests.rs"]
mod tests;
