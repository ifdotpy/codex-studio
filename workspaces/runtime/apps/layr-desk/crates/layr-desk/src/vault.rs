//! One vault as the gateway sees it (docs/design.md, "Gateway"). It has no user database: a caller
//! proves that it holds the vault identity by decrypting a challenge, and gets a short token.

use anyhow::Result;
use base64::engine::general_purpose::{GeneralPurpose, GeneralPurposeConfig, STANDARD};
use base64::engine::{DecodePaddingMode, Engine};
use bytes::Bytes;
use hmac::{Hmac, Mac};
use http_body_util::combinators::UnsyncBoxBody;
use http_body_util::BodyExt;
use sha2::Sha256;
use std::future::Future;
use std::sync::Arc;

/// Tokens live 10 minutes; a restart only asks the browsers for a new challenge.
pub const TOKEN_LIFE_MS: u64 = 10 * 60 * 1000;

/// A response or object body.
pub type Body = UnsyncBoxBody<Bytes, anyhow::Error>;

/// An error that the caller sees with its status.
#[derive(Debug)]
pub struct VaultError {
    pub status: u16,
    pub message: &'static str,
}

impl std::fmt::Display for VaultError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{} {}", self.status, self.message)
    }
}

impl std::error::Error for VaultError {}

pub fn fail<T>(status: u16, message: &'static str) -> Result<T> {
    Err(VaultError { status, message }.into())
}

pub struct Object {
    pub length: Option<u64>,
    pub body: Body,
}

/// The S3 calls of the gateway, on the bucket of one vault.
pub trait Bucket: Send + Sync + 'static {
    /// Every key below `prefix`.
    fn list(&self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> + Send;
    /// One object, or none when there is no such key.
    fn get(&self, key: &str) -> impl Future<Output = Result<Option<Object>>> + Send;
    /// Put an object that must not exist yet: false when it exists.
    fn put_new(&self, key: &str, body: Vec<u8>) -> impl Future<Output = Result<bool>> + Send;
    fn delete(&self, key: &str) -> impl Future<Output = Result<()>> + Send;
}

/// `<seq>.<attempt>.tree.age` or `<seq>.<attempt>.<part>.data.age`.
pub fn state_name(name: &str) -> bool {
    let b = name.as_bytes();
    let digits = |s: &[u8]| s.iter().all(u8::is_ascii_digit);
    let tail = b.get(19..).unwrap_or_default();
    b.len() >= 19
        && digits(&b[..10])
        && b[10] == b'.'
        && b[11..19]
            .iter()
            .all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(c))
        && (tail == b".tree.age"
            || (tail.len() == 14
                && tail[0] == b'.'
                && digits(&tail[1..5])
                && &tail[5..] == b".data.age"))
}

/// `passkey-<id>`: the slots that the gateway may add and remove.
pub fn slot_name(name: &str) -> bool {
    name.strip_prefix("passkey-").is_some_and(|id| {
        (1..=256).contains(&id.len())
            && id
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    })
}

/// A vault name, as the API takes it.
pub fn project_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=44).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b[1..]
            .iter()
            .all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

pub type Clock = Arc<dyn Fn() -> u64 + Send + Sync>;

/// Milliseconds since the Unix epoch.
pub fn system_clock() -> Clock {
    Arc::new(|| {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    })
}

pub struct Vault<B> {
    bucket: B,
    prefix: String,
    recipient: String,
    /// Signs tokens; `prefix` binds them to this vault.
    secret: Arc<[u8; 32]>,
    now: Clock,
}

/// Base64 as browsers and Node write it: either alphabet, padding optional.
const LENIENT: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

impl<B: Bucket> Vault<B> {
    pub fn new(
        bucket: B,
        prefix: &str,
        recipient: &str,
        secret: Arc<[u8; 32]>,
        now: Clock,
    ) -> Self {
        Vault {
            bucket,
            prefix: prefix.to_string(),
            recipient: recipient.to_string(),
            secret,
            now,
        }
    }

    fn mac(&self, expiry: &str) -> Hmac<Sha256> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.secret.as_slice()).expect("any key");
        mac.update(format!("{}:{expiry}", self.prefix).as_bytes());
        mac
    }

    pub fn valid(&self, token: &str) -> bool {
        let Some((expiry, given)) = token.split_once('.') else {
            return false;
        };
        if !expiry.bytes().all(|c| c.is_ascii_digit()) {
            return false;
        }
        let Ok(at) = expiry.parse::<u64>() else {
            return false;
        };
        let left = i128::from(at) - i128::from((self.now)());
        if !(left > 0 && left <= i128::from(TOKEN_LIFE_MS)) {
            return false;
        }
        match hex::decode(given) {
            Ok(given) => self.mac(expiry).verify_slice(&given).is_ok(),
            Err(_) => false,
        }
    }

    pub async fn slots(&self) -> Result<Vec<(String, String)>> {
        let under = format!("{}/slots/", self.prefix);
        let mut slots = Vec::new();
        for key in self.bucket.list(&under).await? {
            let Some(object) = self.bucket.get(&key).await? else {
                continue;
            };
            let data = object.body.collect().await?.to_bytes();
            let name = &key[under.len()..];
            let name = name.strip_suffix(".age").unwrap_or(name);
            slots.push((name.to_string(), STANDARD.encode(&data)));
        }
        Ok(slots)
    }

    pub fn challenge(&self) -> Result<String> {
        let expiry = ((self.now)() + TOKEN_LIFE_MS).to_string();
        let mac = hex::encode(self.mac(&expiry).finalize().into_bytes());
        let sealed =
            desk_core::keys::encrypt(&self.recipient, format!("{expiry}.{mac}").as_bytes())?;
        Ok(STANDARD.encode(sealed))
    }

    /// The public part of vault.json: the recipient, the writer's key and the first signed
    /// `seq` (docs/design.md, "Signatures").
    pub async fn info(&self) -> Result<desk_core::VaultInfo> {
        let Some(object) = self
            .bucket
            .get(&format!("{}/vault.json", self.prefix))
            .await?
        else {
            return fail(404, "the vault has no vault.json");
        };
        let data = object.body.collect().await?.to_bytes();
        match serde_json::from_slice(&data) {
            Ok(info) => Ok(info),
            Err(_) => fail(502, "the vault.json of this vault is not valid"),
        }
    }

    /// Object names below states/, without the prefix.
    pub async fn states(&self) -> Result<Vec<String>> {
        let under = format!("{}/states/", self.prefix);
        Ok(self
            .bucket
            .list(&under)
            .await?
            .into_iter()
            .map(|key| key[under.len()..].to_string())
            .collect())
    }

    /// The body and length of one state object.
    pub async fn object(&self, name: &str) -> Result<Object> {
        if !state_name(name) {
            return fail(400, "not a state object name");
        }
        match self
            .bucket
            .get(&format!("{}/states/{name}", self.prefix))
            .await?
        {
            Some(object) => Ok(object),
            None => fail(404, "no such state object"),
        }
    }

    pub async fn add_slot(&self, name: &str, data: &str) -> Result<()> {
        if !slot_name(name) {
            return fail(400, "only passkey slots change here");
        }
        let text: String = data
            .chars()
            .filter(|c| !c.is_ascii_whitespace())
            .map(|c| match c {
                '-' => '+',
                '_' => '/',
                c => c,
            })
            .collect();
        let body = LENIENT.decode(text).unwrap_or_default();
        if body.is_empty() || body.len() > 4096 {
            return fail(400, "a slot is 1 to 4096 bytes");
        }
        let key = format!("{}/slots/{name}.age", self.prefix);
        if !self.bucket.put_new(&key, body).await? {
            return fail(409, "this passkey already opens the vault");
        }
        Ok(())
    }

    pub async fn remove_slot(&self, name: &str) -> Result<()> {
        if !slot_name(name) {
            return fail(400, "only passkey slots change here");
        }
        self.bucket
            .delete(&format!("{}/slots/{name}.age", self.prefix))
            .await
    }
}
