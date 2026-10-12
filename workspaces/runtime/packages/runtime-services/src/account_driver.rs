//! Refreshable account registry backed by Codex and Claude auth metadata.
//!
//! The registry is loaded once. Each `get` refreshes its provider metadata,
//! validates the resulting document, and atomically persists it while holding
//! the driver's mutex. Credential material is never retained in this module.

use serde::Deserialize;
use serde::Deserializer;
use serde::de::{DeserializeSeed, MapAccess, Visitor};
use serde::ser::{Serialize, SerializeMap, Serializer};
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fmt;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};
use thiserror::Error;

const REGISTRY_SCHEMA: &str = include_str!("../../runtime-http/account-registry.schema.json");
const AUTH_FILE_LIMIT: u64 = 1024 * 1024;
const CLAUDE_OUTPUT_LIMIT: usize = 1024 * 1024;
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(8);
const CLAUDE_BIN_OVERRIDE: &str = "STUDIO_CLAUDE_BIN";
const AUTH_ERROR: &str = "Cannot read this account's authentication";
const CHANGED_ERROR: &str =
    "This profile's account changed. Restore its original login or add a separate profile.";
const UNVERIFIED_ERROR: &str =
    "Cannot verify this profile's account. Sign in to its original account again.";

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AccountStatus {
    Ready,
    SignedOut,
    Pending,
    Error,
    Changed,
    Duplicate,
    Unavailable,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AuthStatus {
    Ready,
    SignedOut,
    Error,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthMetadata {
    pub status: AuthStatus,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub credential_identity: Option<String>,
    pub error: Option<&'static str>,
}

impl AuthMetadata {
    fn signed_out() -> Self {
        Self {
            status: AuthStatus::SignedOut,
            account_id: None,
            email: None,
            plan: None,
            credential_identity: None,
            error: None,
        }
    }

    fn error() -> Self {
        Self {
            status: AuthStatus::Error,
            error: Some(AUTH_ERROR),
            ..Self::signed_out()
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AuthProfile {
    pub provider: String,
    pub home: PathBuf,
    pub claude_options: Option<Value>,
}

pub trait ProviderAuthReader: Send + Sync {
    fn read(&self, profile: &AuthProfile) -> AuthMetadata;
}

/// Reads Codex auth files and runs the native Claude metadata command.
pub struct SystemProviderAuthReader;

impl ProviderAuthReader for SystemProviderAuthReader {
    fn read(&self, profile: &AuthProfile) -> AuthMetadata {
        if profile.provider == "claude" {
            ClaudeCliAuthReader.read(profile)
        } else {
            codex_auth_metadata(&profile.home)
        }
    }
}

/// Codex auth metadata is derived from `<home>/auth.json`; the file is never
/// copied, returned or included in an error.
pub fn codex_auth_metadata(home: &Path) -> AuthMetadata {
    let path = home.join("auth.json");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return AuthMetadata::signed_out();
        }
        Err(_) => return AuthMetadata::error(),
    };
    let auth = (|| -> Result<Value, ()> {
        if file.metadata().map_err(|_| ())?.len() > AUTH_FILE_LIMIT {
            return Err(());
        }
        let mut bytes = Vec::new();
        file.take(AUTH_FILE_LIMIT + 1)
            .read_to_end(&mut bytes)
            .map_err(|_| ())?;
        if bytes.len() as u64 > AUTH_FILE_LIMIT {
            return Err(());
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|_| ())?;
        value.as_object().ok_or(())?;
        Ok(value)
    })();
    let Ok(auth) = auth else {
        return AuthMetadata::error();
    };
    codex_metadata_from_value(&auth)
}

fn codex_metadata_from_value(auth: &Value) -> AuthMetadata {
    let tokens = auth.get("tokens").and_then(Value::as_object);
    let token = tokens.and_then(|value| {
        value
            .get("id_token")
            .and_then(nonempty_string)
            .or_else(|| value.get("access_token").and_then(nonempty_string))
    });
    let claims = token
        .as_deref()
        .and_then(|value| value.split('.').nth(1))
        .and_then(base64url_decode)
        .and_then(|value| serde_json::from_slice::<Value>(&value).ok())
        .unwrap_or(Value::Null);
    let details = claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object);
    let profile = claims
        .get("https://api.openai.com/profile")
        .and_then(Value::as_object);
    let account_id = tokens
        .and_then(|value| value.get("account_id"))
        .and_then(nonempty_string)
        .or_else(|| {
            details
                .and_then(|value| value.get("chatgpt_account_id"))
                .and_then(nonempty_string)
        });
    let email = claims.get("email").and_then(nonempty_string).or_else(|| {
        profile
            .and_then(|value| value.get("email"))
            .and_then(nonempty_string)
    });
    let api_key = auth
        .get("OPENAI_API_KEY")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let is_ready = (account_id.is_some()
        && tokens
            .and_then(|value| value.get("access_token"))
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty()))
        || api_key.is_some();
    if !is_ready {
        return AuthMetadata::signed_out();
    }
    let credential_identity = account_id
        .as_ref()
        .map(|value| format!("chatgpt:{value}"))
        .or_else(|| api_key.map(|value| format!("api:{}", hex_sha256(value.as_bytes()))));
    let plan = details
        .and_then(|value| value.get("chatgpt_plan_type"))
        .and_then(nonempty_string)
        .or_else(|| api_key.map(|_| "API key".to_owned()));
    AuthMetadata {
        status: AuthStatus::Ready,
        account_id,
        email,
        plan,
        credential_identity,
        error: None,
    }
}

struct ClaudeCliAuthReader;

impl ProviderAuthReader for ClaudeCliAuthReader {
    fn read(&self, profile: &AuthProfile) -> AuthMetadata {
        let environment = env::var(CLAUDE_BIN_OVERRIDE)
            .ok()
            .map(|value| BTreeMap::from([(CLAUDE_BIN_OVERRIDE.to_owned(), value)]))
            .unwrap_or_default();
        Self::read_with_environment(profile, &environment)
    }
}

impl ClaudeCliAuthReader {
    fn read_with_environment(
        profile: &AuthProfile,
        environment: &BTreeMap<String, String>,
    ) -> AuthMetadata {
        let options = profile.claude_options.as_ref();
        let configured_binary = options
            .and_then(|value| value.get("binaryPath"))
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty());
        let path = environment
            .get("PATH")
            .map(OsStr::new)
            .map(OsStr::to_os_string)
            .or_else(|| env::var_os("PATH"));
        let home = environment
            .get("HOME")
            .map(OsStr::new)
            .map(OsStr::to_os_string)
            .or_else(|| env::var_os("HOME"));
        let binary = resolve_claude_binary(
            configured_binary,
            environment.get(CLAUDE_BIN_OVERRIDE).map(String::as_str),
            path.as_deref(),
            home.as_deref(),
        );
        let output = run_claude_status(&binary, options);
        let Ok((stdout, succeeded)) = output else {
            return AuthMetadata::error();
        };
        parse_claude_status(&stdout, succeeded)
    }
}

fn resolve_claude_binary(
    configured: Option<&str>,
    override_binary: Option<&str>,
    path: Option<&OsStr>,
    home: Option<&OsStr>,
) -> String {
    if let Some(binary) = configured
        .filter(|value| !value.is_empty())
        .or_else(|| override_binary.filter(|value| !value.is_empty()))
    {
        return binary.to_owned();
    }
    if let Some(binary) = path.and_then(|path| {
        env::split_paths(path)
            .map(|directory| directory.join("claude"))
            .find(|candidate| is_executable(candidate))
    }) {
        return binary.to_string_lossy().into_owned();
    }
    if let Some(binary) = home
        .map(PathBuf::from)
        .map(|home| home.join(".local/bin/claude"))
        .filter(|candidate| is_executable(candidate))
    {
        return binary.to_string_lossy().into_owned();
    }
    "claude".to_owned()
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

fn run_claude_status(binary: &str, options: Option<&Value>) -> Result<(Vec<u8>, bool), ()> {
    let mut command = Command::new(binary);
    command.args(["auth", "status", "--json"]);
    for name in [
        "ANTHROPIC_API_KEY",
        "ANTHROPIC_AUTH_TOKEN",
        "ANTHROPIC_BASE_URL",
        "CLAUDE_CODE_OAUTH_TOKEN",
        "CLAUDE_CODE_OAUTH_TOKEN_FILE_DESCRIPTOR",
        "CLAUDE_CODE_API_KEY_FILE_DESCRIPTOR",
        "CLAUDE_CODE_USE_BEDROCK",
        "CLAUDE_CODE_USE_VERTEX",
        "CLAUDE_CODE_USE_FOUNDRY",
    ] {
        command.env_remove(name);
    }
    if let Some(config_dir) = options
        .and_then(|value| value.get("configDir"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        command.env("CLAUDE_CONFIG_DIR", config_dir.trim());
    }
    // `Command` inherits all other environment values, matching Python's
    // subscription_env while ensuring the credential environment is cleared.
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| ())?;
    let stdout = child.stdout.take().ok_or(())?;
    let exceeded = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let finished = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let reader_exceeded = exceeded.clone();
    let reader_finished = finished.clone();
    let reader = std::thread::spawn(move || {
        let mut output = Vec::new();
        let mut source = stdout.take((CLAUDE_OUTPUT_LIMIT + 1) as u64);
        let result = source.read_to_end(&mut output);
        if output.len() > CLAUDE_OUTPUT_LIMIT || result.is_err() {
            reader_exceeded.store(true, std::sync::atomic::Ordering::Relaxed);
        }
        reader_finished.store(true, std::sync::atomic::Ordering::Release);
        output
    });
    let started = Instant::now();
    let status = loop {
        if exceeded.load(std::sync::atomic::Ordering::Relaxed) {
            let _kill_result = child.kill();
            break child.wait().map_err(|_| ())?;
        }
        if let Some(status) = child.try_wait().map_err(|_| ())? {
            break status;
        }
        if started.elapsed() >= CLAUDE_TIMEOUT {
            let _kill_result = child.kill();
            let _wait_result = child.wait();
            // If a descendant inherited stdout, the reader may not reach EOF
            // even after the direct child is reaped. Detach it at the bound.
            return Err(());
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    while !finished.load(std::sync::atomic::Ordering::Acquire) {
        if started.elapsed() >= CLAUDE_TIMEOUT {
            return Err(());
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = reader.join().map_err(|_| ())?;
    if output.len() <= CLAUDE_OUTPUT_LIMIT {
        Ok((output, status.success()))
    } else {
        Err(())
    }
}

fn parse_claude_status(stdout: &[u8], succeeded: bool) -> AuthMetadata {
    let Ok(value) = serde_json::from_slice::<Value>(stdout) else {
        return AuthMetadata::error();
    };
    if !value.is_object() {
        return AuthMetadata::error();
    }
    if !succeeded
        || value.get("loggedIn").and_then(Value::as_bool) != Some(true)
        || value.get("authMethod").and_then(Value::as_str) != Some("claude.ai")
    {
        return AuthMetadata::signed_out();
    }
    let Some(email) = value.get("email").and_then(nonempty_string) else {
        return AuthMetadata::error();
    };
    let account_id = format!("claude:{email}");
    AuthMetadata {
        status: AuthStatus::Ready,
        account_id: Some(account_id.clone()),
        email: Some(email),
        plan: value.get("subscriptionType").and_then(nonempty_string),
        credential_identity: Some(account_id),
        error: None,
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum AccountDriverError {
    #[error("Cannot read state/accounts/registry.json")]
    ReadRegistry,
    #[error("Cannot validate state/accounts/registry.json")]
    InvalidRegistry,
    #[error("Unknown account")]
    UnknownAccount,
    #[error("Cannot write state/accounts/registry.json")]
    WriteRegistry,
    #[error("Account registry lock is unavailable")]
    Poisoned,
}

pub struct AccountDriver<R = SystemProviderAuthReader> {
    path: PathBuf,
    registry: Mutex<Value>,
    account_order: Mutex<Vec<String>>,
    login_order: Mutex<Vec<String>>,
    reader: R,
}

impl AccountDriver<SystemProviderAuthReader> {
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, AccountDriverError> {
        Self::open_with_reader(path, SystemProviderAuthReader)
    }
}

impl<R: ProviderAuthReader> AccountDriver<R> {
    pub fn open_with_reader(
        path: impl Into<PathBuf>,
        reader: R,
    ) -> Result<Self, AccountDriverError> {
        let path = path.into();
        let bytes = read_registry(&path)?;
        let (registry, account_order, login_order) = parse_registry(&bytes)?;
        Ok(Self {
            path,
            registry: Mutex::new(registry),
            account_order: Mutex::new(account_order),
            login_order: Mutex::new(login_order),
            reader,
        })
    }

    /// Refresh one account and persist safe auth metadata before returning its
    /// admission status. Duplicate profiles return their stored status.
    pub fn get(&self, account: &str) -> Result<AccountStatus, AccountDriverError> {
        let mut registry = self
            .registry
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut account_order = self
            .account_order
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut login_order = self
            .login_order
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let row = registry
            .get("accounts")
            .and_then(Value::as_object)
            .and_then(|accounts| accounts.get(account))
            .and_then(Value::as_object)
            .ok_or(AccountDriverError::UnknownAccount)?;
        if row.get("deleted") == Some(&Value::Bool(true))
            || row.get("disconnected") == Some(&Value::Bool(true))
        {
            return Ok(AccountStatus::Unavailable);
        }
        if row.get("duplicateOf").is_some_and(|value| !value.is_null()) {
            return Ok(status_from_value(row.get("status")));
        }
        let profile = AuthProfile {
            provider: row
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("codex")
                .to_owned(),
            home: PathBuf::from(row.get("home").and_then(Value::as_str).unwrap_or_default()),
            claude_options: row.get("claudeOptions").cloned(),
        };
        let old_account_id = row
            .get("accountId")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let old_credential = row
            .get("_credentialIdentity")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let old_source = row
            .get("source")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        let old_status = status_from_value(row.get("status"));
        let metadata = self.reader.read(&profile);
        let mut updated = registry.clone();
        let accounts = updated
            .get_mut("accounts")
            .and_then(Value::as_object_mut)
            .ok_or(AccountDriverError::InvalidRegistry)?;
        let row = accounts
            .get_mut(account)
            .and_then(Value::as_object_mut)
            .ok_or(AccountDriverError::UnknownAccount)?;
        apply_metadata(
            row,
            &metadata,
            old_account_id.as_deref(),
            old_credential.as_deref(),
            &old_source,
            old_status,
        );
        let encoded = serialize_registry(&updated, &account_order, &login_order)?;
        let (checked, checked_order, checked_login_order) = parse_registry(&encoded)?;
        atomic_replace(&self.path, &encoded)?;
        let status = checked
            .get("accounts")
            .and_then(Value::as_object)
            .and_then(|accounts| accounts.get(account))
            .and_then(Value::as_object)
            .and_then(|row| row.get("status"));
        let status = status_from_value(status);
        *registry = checked;
        *account_order = checked_order;
        *login_order = checked_login_order;
        Ok(status)
    }
}

fn apply_metadata(
    row: &mut serde_json::Map<String, Value>,
    metadata: &AuthMetadata,
    expected_account: Option<&str>,
    expected_credential: Option<&str>,
    source: &str,
    old_status: AccountStatus,
) {
    if metadata.status != AuthStatus::Error {
        row.remove("_authErrorKind");
    }
    if expected_account
        .zip(metadata.account_id.as_deref())
        .is_some_and(|(a, b)| a != b)
        || expected_credential
            .zip(metadata.credential_identity.as_deref())
            .is_some_and(|(a, b)| a != b)
    {
        row.insert("status".into(), Value::String("changed".into()));
        row.insert("error".into(), Value::String(CHANGED_ERROR.into()));
        return;
    }
    if metadata.status == AuthStatus::Ready
        && ((expected_account.is_some() && metadata.account_id.is_none())
            || (expected_credential.is_some() && metadata.credential_identity.is_none())
            || (metadata.account_id.is_none() && metadata.credential_identity.is_none()))
    {
        row.insert("status".into(), Value::String("error".into()));
        row.insert("error".into(), Value::String(UNVERIFIED_ERROR.into()));
        return;
    }
    if source == "Codex Agents"
        && matches!(old_status, AccountStatus::Pending | AccountStatus::Error)
        && metadata.status == AuthStatus::SignedOut
    {
        return;
    }
    if metadata.status == AuthStatus::Error {
        row.insert("status".into(), Value::String("error".into()));
        if let Some(error) = metadata.error {
            row.insert("error".into(), Value::String(error.into()));
        }
        return;
    }
    let fields = [
        ("accountId", metadata.account_id.as_ref()),
        ("_credentialIdentity", metadata.credential_identity.as_ref()),
        ("email", metadata.email.as_ref()),
        ("plan", metadata.plan.as_ref()),
    ];
    for (key, value) in fields {
        if let Some(value) = value {
            row.insert(key.into(), Value::String(value.clone()));
        }
    }
    row.insert(
        "status".into(),
        Value::String(
            match metadata.status {
                AuthStatus::Ready => "ready",
                AuthStatus::SignedOut => "signedOut",
                AuthStatus::Error => "error",
            }
            .into(),
        ),
    );
    if let Some(error) = metadata.error {
        row.insert("error".into(), Value::String(error.into()));
    } else if metadata.status != AuthStatus::Error {
        row.remove("error");
    }
}

fn status_from_value(value: Option<&Value>) -> AccountStatus {
    match value.and_then(Value::as_str) {
        Some("ready") => AccountStatus::Ready,
        Some("pending") => AccountStatus::Pending,
        Some("changed") => AccountStatus::Changed,
        Some("duplicate") => AccountStatus::Duplicate,
        Some("error") => AccountStatus::Error,
        _ => AccountStatus::SignedOut,
    }
}

fn parse_registry(bytes: &[u8]) -> Result<(Value, Vec<String>, Vec<String>), AccountDriverError> {
    if bytes.len() > studio_runtime_store::account_state::MAX_PERSISTED_DOCUMENT_BYTES
        || !studio_runtime_store::account_state::persisted_document_within_bounds(bytes)
    {
        return Err(AccountDriverError::InvalidRegistry);
    }
    let value: Value =
        serde_json::from_slice(bytes).map_err(|_| AccountDriverError::InvalidRegistry)?;
    static VALIDATOR: OnceLock<Result<jsonschema::Validator, ()>> = OnceLock::new();
    let valid = VALIDATOR
        .get_or_init(|| {
            let schema = serde_json::from_str(REGISTRY_SCHEMA).map_err(|_| ())?;
            jsonschema::validator_for(&schema).map_err(|_| ())
        })
        .as_ref()
        .map_err(|_| AccountDriverError::InvalidRegistry)?
        .is_valid(&value);
    if !valid {
        return Err(AccountDriverError::InvalidRegistry);
    }
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let order = RegistryOrder::deserialize(&mut deserializer)
        .map_err(|_| AccountDriverError::InvalidRegistry)?;
    Ok((value, order.accounts, order.logins))
}

#[derive(Default)]
struct RegistryOrder {
    accounts: Vec<String>,
    logins: Vec<String>,
}

impl<'de> Deserialize<'de> for RegistryOrder {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct RootVisitor;
        impl<'de> Visitor<'de> for RootVisitor {
            type Value = RegistryOrder;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("validated account registry")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut order = RegistryOrder::default();
                let mut seen = std::collections::HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate registry field"));
                    }
                    if key == "accounts" {
                        order.accounts = map.next_value_seed(AccountKeys)?;
                    } else if key == "logins" {
                        order.logins = map.next_value_seed(AccountKeys)?;
                    } else {
                        map.next_value::<serde::de::IgnoredAny>()?;
                    }
                }
                Ok(order)
            }
        }
        deserializer.deserialize_map(RootVisitor)
    }
}

struct AccountKeys;

impl<'de> DeserializeSeed<'de> for AccountKeys {
    type Value = Vec<String>;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct AccountsVisitor;
        impl<'de> Visitor<'de> for AccountsVisitor {
            type Value = Vec<String>;

            fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str("ordered account object")
            }

            fn visit_map<M>(self, mut map: M) -> Result<Self::Value, M::Error>
            where
                M: MapAccess<'de>,
            {
                let mut keys = Vec::new();
                let mut seen = std::collections::HashSet::new();
                while let Some(key) = map.next_key::<String>()? {
                    if !seen.insert(key.clone()) {
                        return Err(serde::de::Error::custom("duplicate registry key"));
                    }
                    map.next_value::<serde::de::IgnoredAny>()?;
                    keys.push(key);
                }
                Ok(keys)
            }
        }
        deserializer.deserialize_map(AccountsVisitor)
    }
}

fn serialize_registry(
    registry: &Value,
    account_order: &[String],
    login_order: &[String],
) -> Result<Vec<u8>, AccountDriverError> {
    let Some(root) = registry.as_object() else {
        return Err(AccountDriverError::InvalidRegistry);
    };
    let Some(accounts) = root.get("accounts").and_then(Value::as_object) else {
        return Err(AccountDriverError::InvalidRegistry);
    };
    let logins = root.get("logins").and_then(Value::as_object);
    let mut output = Vec::new();
    let mut serializer = serde_json::Serializer::new(&mut output);
    let mut map = serializer
        .serialize_map(Some(root.len()))
        .map_err(|_| AccountDriverError::InvalidRegistry)?;
    for (key, value) in root {
        if key == "accounts" {
            map.serialize_entry(
                key,
                &OrderedAccounts {
                    accounts,
                    order: account_order,
                },
            )
            .map_err(|_| AccountDriverError::InvalidRegistry)?;
        } else if key == "logins" {
            let Some(logins) = logins else {
                return Err(AccountDriverError::InvalidRegistry);
            };
            map.serialize_entry(
                key,
                &OrderedAccounts {
                    accounts: logins,
                    order: login_order,
                },
            )
            .map_err(|_| AccountDriverError::InvalidRegistry)?;
        } else {
            map.serialize_entry(key, value)
                .map_err(|_| AccountDriverError::InvalidRegistry)?;
        }
    }
    map.end().map_err(|_| AccountDriverError::InvalidRegistry)?;
    Ok(output)
}

fn read_registry(path: &Path) -> Result<Vec<u8>, AccountDriverError> {
    let file = File::open(path).map_err(|_| AccountDriverError::ReadRegistry)?;
    let limit = studio_runtime_store::account_state::MAX_PERSISTED_DOCUMENT_BYTES;
    if file
        .metadata()
        .map_err(|_| AccountDriverError::ReadRegistry)?
        .len()
        > limit as u64
    {
        return Err(AccountDriverError::InvalidRegistry);
    }
    let mut bytes = Vec::new();
    file.take(limit as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| AccountDriverError::ReadRegistry)?;
    if bytes.len() > limit {
        return Err(AccountDriverError::InvalidRegistry);
    }
    Ok(bytes)
}

struct OrderedAccounts<'a> {
    accounts: &'a serde_json::Map<String, Value>,
    order: &'a [String],
}

impl Serialize for OrderedAccounts<'_> {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        let mut map = serializer.serialize_map(Some(self.accounts.len()))?;
        let mut emitted = std::collections::HashSet::new();
        for key in self.order {
            if let Some(value) = self.accounts.get(key) {
                map.serialize_entry(key, value)?;
                emitted.insert(key);
            }
        }
        for (key, value) in self.accounts {
            if !emitted.contains(key) {
                map.serialize_entry(key, value)?;
            }
        }
        map.end()
    }
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), AccountDriverError> {
    static NEXT_TEMP: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let parent = path.parent().ok_or(AccountDriverError::WriteRegistry)?;
    fs::create_dir_all(parent).map_err(|_| AccountDriverError::WriteRegistry)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| AccountDriverError::WriteRegistry)?;
    }
    let sequence = NEXT_TEMP.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let temp = parent.join(format!("registry-{}-{sequence}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options
            .open(&temp)
            .map_err(|_| AccountDriverError::WriteRegistry)?;
        file.write_all(bytes)
            .map_err(|_| AccountDriverError::WriteRegistry)?;
        file.sync_all()
            .map_err(|_| AccountDriverError::WriteRegistry)?;
        fs::rename(&temp, path).map_err(|_| AccountDriverError::WriteRegistry)?;
        File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| AccountDriverError::WriteRegistry)?;
        Ok(())
    })();
    if result.is_err() {
        let _cleanup_result = fs::remove_file(temp);
    }
    result
}

fn nonempty_string(value: &Value) -> Option<String> {
    value
        .as_str()
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
}

fn hex_sha256(value: &[u8]) -> String {
    studio_runtime_store::sha256_hex(value)
}

fn base64url_decode(value: &str) -> Option<Vec<u8>> {
    let mut output = Vec::with_capacity(value.len() * 3 / 4);
    let mut accumulator = 0_u32;
    let mut bits = 0_u8;
    for byte in value.bytes() {
        let digit = match byte {
            b'A'..=b'Z' => byte - b'A',
            b'a'..=b'z' => byte - b'a' + 26,
            b'0'..=b'9' => byte - b'0' + 52,
            b'-' => 62,
            b'_' => 63,
            b'=' => break,
            _ => return None,
        };
        accumulator = (accumulator << 6) | u32::from(digit);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            output.push(((accumulator >> bits) & 0xff) as u8);
        }
    }
    Some(output)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);
    const PLACEHOLDER_KEY: &str = "placeholder-secret-do-not-persist";

    struct FakeReader(AuthMetadata);

    impl ProviderAuthReader for FakeReader {
        fn read(&self, _profile: &AuthProfile) -> AuthMetadata {
            self.0.clone()
        }
    }

    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            let root = env::var_os("CS_RUST_ACCOUNT_DRIVER_TEST_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(env::temp_dir);
            let path = root.join(format!(
                "account-driver-test-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            fs::create_dir_all(&path).expect("create disposable fixture directory");
            Self(path)
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _cleanup_result = fs::remove_dir_all(&self.0);
        }
    }

    fn isolated_python() -> PathBuf {
        if let Some(path) = env::var_os("CODEX_AGENTS_PYTHON") {
            return PathBuf::from(path);
        }
        let server = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let output = Command::new("python3")
            .arg(server.join("codex_python.py"))
            .output()
            .expect("resolve isolated runtime interpreter");
        assert!(
            output.status.success(),
            "isolated Python missing; run install-cli.py"
        );
        PathBuf::from(
            String::from_utf8(output.stdout)
                .expect("interpreter path UTF-8")
                .trim(),
        )
    }

    fn registry(home: &Path) -> Vec<u8> {
        serde_json::to_vec(&serde_json::json!({
            "version": 1,
            "defaultAccountKey": "default",
            "accounts": {"default": {
                "id": "default", "home": home, "label": "Codex", "source": "Codex CLI", "status": "signedOut"
            }},
            "logins": {}, "deleteReceipts": {}
        }))
        .expect("fixture registry JSON")
    }

    fn metadata(
        status: AuthStatus,
        account_id: Option<&str>,
        identity: Option<&str>,
    ) -> AuthMetadata {
        AuthMetadata {
            status,
            account_id: account_id.map(str::to_owned),
            email: Some("fixture@example.invalid".into()),
            plan: Some("plus".into()),
            credential_identity: identity.map(str::to_owned),
            error: None,
        }
    }

    #[test]
    fn refresh_reads_codex_placeholder_credentials_and_never_persists_token_values() {
        let fixture = TestDir::new();
        let home = fixture.0.join("codex-home");
        fs::create_dir_all(&home).expect("create fake Codex home");
        fs::write(
            home.join("auth.json"),
            format!(r#"{{"OPENAI_API_KEY":"{PLACEHOLDER_KEY}"}}"#),
        )
        .expect("write placeholder auth file");
        let path = fixture.0.join("state/accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        fs::write(&path, registry(&home)).expect("write Python-shaped registry");
        let driver = AccountDriver::open(&path).expect("open validated registry");
        assert_eq!(
            driver.get("default").expect("refresh account"),
            AccountStatus::Ready
        );
        let persisted = fs::read_to_string(path).expect("read refreshed registry");
        assert!(persisted.contains("_credentialIdentity"));
        assert!(!persisted.contains(PLACEHOLDER_KEY));
        assert!(persisted.contains(&hex_sha256(PLACEHOLDER_KEY.as_bytes())));
    }

    #[test]
    fn codex_auth_metadata_uses_access_token_when_id_token_is_empty() {
        let fixture = TestDir::new();
        let home = fixture.0.join("codex-home");
        fs::create_dir_all(&home).expect("create fake Codex home");
        fs::write(
            home.join("auth.json"),
            r#"{"tokens":{"id_token":"","access_token":"a.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjZXNzLWFjY291bnQifX0.x"}}"#,
        )
        .expect("write placeholder token claims");
        let metadata = codex_auth_metadata(&home);
        assert_eq!(metadata.status, AuthStatus::Ready);
        assert_eq!(metadata.account_id.as_deref(), Some("access-account"));
    }

    #[test]
    fn refresh_reads_registry_and_auth_written_by_python_account_store() {
        let fixture = TestDir::new();
        let server = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let script = r#"
import json, os, sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
root = Path(sys.argv[2])
home = root / 'codex-home'
home.mkdir(parents=True)
(home / 'auth.json').write_text(json.dumps({'tokens': {
    'account_id': 'python-account', 'access_token': 'placeholder-access-token',
}}), encoding='utf-8')
os.environ['CODEX_HOME'] = str(home)
from codex_accounts import AccountStore
store = AccountStore(root / 'state')
print(store.path)
"#;
        let output = Command::new(isolated_python())
            .args(["-c", script])
            .arg(&server)
            .arg(&fixture.0)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("TMPDIR", &fixture.0)
            .env("HOME", fixture.0.join("fake-user"))
            .env("XDG_CONFIG_HOME", fixture.0.join("fake-config"))
            .env("XDG_STATE_HOME", fixture.0.join("fake-state"))
            .output()
            .expect("run isolated Python AccountStore writer");
        assert!(output.status.success(), "Python AccountStore writer failed");
        let path = PathBuf::from(
            String::from_utf8(output.stdout)
                .expect("writer path UTF-8")
                .trim(),
        );
        let driver = AccountDriver::open(path.clone()).expect("load Python-written registry");
        assert_eq!(
            driver.get("default").expect("refresh Python account"),
            AccountStatus::Ready
        );
        let persisted = fs::read_to_string(path).expect("read Rust-refreshed registry");
        assert!(persisted.contains("python-account"));
        assert!(!persisted.contains("placeholder-access-token"));
    }

    #[test]
    fn changed_identity_is_persisted_and_blocks_admission() {
        let fixture = TestDir::new();
        let path = fixture.0.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        let mut value: Value =
            serde_json::from_slice(&registry(&fixture.0)).expect("registry fixture");
        let row = value["accounts"]["default"]
            .as_object_mut()
            .expect("account row");
        row.insert("accountId".into(), Value::String("old-account".into()));
        row.insert(
            "_credentialIdentity".into(),
            Value::String("chatgpt:old-account".into()),
        );
        fs::write(&path, serde_json::to_vec(&value).expect("encoded registry"))
            .expect("write registry");
        let reader = FakeReader(metadata(
            AuthStatus::Ready,
            Some("new-account"),
            Some("chatgpt:new-account"),
        ));
        let driver = AccountDriver::open_with_reader(&path, reader).expect("open registry");
        assert_eq!(
            driver.get("default").expect("refresh"),
            AccountStatus::Changed
        );
        let persisted = fs::read_to_string(path).expect("read changed registry");
        assert!(persisted.contains(CHANGED_ERROR));
    }

    #[test]
    fn disconnected_or_deleted_accounts_are_never_admissible() {
        let fixture = TestDir::new();
        let path = fixture.0.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        let mut value: Value =
            serde_json::from_slice(&registry(&fixture.0)).expect("registry fixture");
        let row = value["accounts"]["default"]
            .as_object_mut()
            .expect("account row");
        row.insert("status".into(), Value::String("ready".into()));
        row.insert("disconnected".into(), Value::Bool(true));
        fs::write(&path, serde_json::to_vec(&value).expect("encoded registry"))
            .expect("write registry");
        let driver = AccountDriver::open_with_reader(
            &path,
            FakeReader(metadata(
                AuthStatus::Ready,
                Some("original"),
                Some("chatgpt:original"),
            )),
        )
        .expect("open registry");
        assert_eq!(
            driver.get("default").expect("admission status"),
            AccountStatus::Unavailable
        );
    }

    #[test]
    fn refresh_preserves_python_account_insertion_order() {
        let fixture = TestDir::new();
        let path = fixture.0.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        let bytes = format!(
            r#"{{"version":1,"defaultAccountKey":"z","accounts":{{"z":{{"id":"z","home":"{}","label":"Z","source":"Codex CLI","status":"signedOut"}},"a":{{"id":"a","home":"{}","label":"A","source":"Codex CLI","status":"signedOut"}}}},"logins":{{}},"deleteReceipts":{{}}}}"#,
            fixture.0.display(),
            fixture.0.display()
        );
        fs::write(&path, bytes).expect("write ordered registry");
        let driver = AccountDriver::open_with_reader(
            &path,
            FakeReader(metadata(AuthStatus::SignedOut, None, None)),
        )
        .expect("open ordered registry");
        assert_eq!(driver.get("z").expect("refresh"), AccountStatus::SignedOut);
        let persisted = fs::read_to_string(path).expect("read refreshed registry");
        assert!(
            persisted.find("\"z\":{").expect("z key") < persisted.find("\"a\":{").expect("a key")
        );
    }

    #[test]
    fn refused_registry_is_not_written_and_errors_do_not_echo_contents() {
        let fixture = TestDir::new();
        let path = fixture.0.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        let invalid = format!(r#"{{"sensitive":"{PLACEHOLDER_KEY}"}}"#);
        fs::write(&path, &invalid).expect("write invalid registry");
        let before = fs::read(&path).expect("read fixture registry");
        let error = match AccountDriver::open(&path) {
            Err(error) => error,
            Ok(_) => panic!("invalid registry unexpectedly opened"),
        };
        assert_eq!(error, AccountDriverError::InvalidRegistry);
        assert!(!error.to_string().contains(PLACEHOLDER_KEY));
        assert!(
            fs::read(&path).expect("registry remains") == before,
            "refused registry bytes changed"
        );
    }

    #[test]
    fn fake_claude_executable_uses_override_and_parses_only_metadata() {
        let fixture = TestDir::new();
        let binary = fixture.0.join("claude-fake");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::write(&binary, "#!/bin/sh\nprintf '%s' '{\"loggedIn\":true,\"authMethod\":\"claude.ai\",\"email\":\"claude@example.invalid\",\"subscriptionType\":\"pro\"}'\n")
                .expect("write fake CLI");
            fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
                .expect("make fake CLI executable");
            let override_value = binary.to_str().expect("fixture path").to_owned();
            let environment = BTreeMap::from([(CLAUDE_BIN_OVERRIDE.to_owned(), override_value)]);
            let profile = AuthProfile {
                provider: "claude".into(),
                home: fixture.0.clone(),
                claude_options: None,
            };
            let metadata = ClaudeCliAuthReader::read_with_environment(&profile, &environment);
            assert_eq!(metadata.status, AuthStatus::Ready);
            assert_eq!(
                metadata.account_id.as_deref(),
                Some("claude:claude@example.invalid")
            );
            assert_eq!(parse_claude_status(b"[]", true).status, AuthStatus::Error);

            let fallback_home = fixture.0.join("fake-home");
            let fallback_binary = fallback_home.join(".local/bin/claude");
            fs::create_dir_all(fallback_binary.parent().expect("fallback parent"))
                .expect("create fallback bin directory");
            fs::copy(&binary, &fallback_binary).expect("copy fake CLI to fallback path");
            fs::set_permissions(&fallback_binary, fs::Permissions::from_mode(0o700))
                .expect("make fallback CLI executable");
            let empty_path = fixture.0.join("empty-path");
            fs::create_dir_all(&empty_path).expect("create isolated PATH");
            let environment = BTreeMap::from([
                (
                    "HOME".to_owned(),
                    fallback_home.to_string_lossy().into_owned(),
                ),
                ("PATH".to_owned(), empty_path.to_string_lossy().into_owned()),
            ]);
            let metadata = ClaudeCliAuthReader::read_with_environment(&profile, &environment);
            assert_eq!(metadata.status, AuthStatus::Ready);
        }
    }
}
