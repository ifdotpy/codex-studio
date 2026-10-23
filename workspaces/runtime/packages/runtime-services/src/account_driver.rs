//! Refreshable account registry backed by Codex and Claude auth metadata.
//!
//! The registry is loaded once. Each `get` refreshes its provider metadata,
//! validates the resulting document, and atomically persists it while holding
//! the driver's mutex. Credential material is never retained in this module.

pub mod auth_cache;
pub mod claude;
pub(crate) mod claude_identity;

use crate::account_policy::{AccountsSnapshot, RegistryOrder, decode_registry, encode_registry};
use crate::event_write::python_strip;
use base64::Engine as _;
#[cfg(unix)]
use process_wrap::std::ChildWrapper;
use process_wrap::std::CommandWrap;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use serde::de::{IgnoredAny, MapAccess, Visitor};
use serde::{Deserialize, Deserializer};
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use studio_runtime_store::path_identity::CanonicalPath;

/// The account's provider server directory: the state root for `default`,
/// otherwise `<state>/account-servers/<account>`. Python owns this layout
/// (`codex_runtime.py` and `codex_native_runtime.py`); the Claude bridge keeps
/// its session snapshots in `<directory>/sessions`, so an agent that Python
/// started resumes only from this directory.
pub fn account_server_root(state_root: &Path, account_key: &str) -> PathBuf {
    if account_key == "default" {
        state_root.to_path_buf()
    } else {
        state_root.join("account-servers").join(account_key)
    }
}
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use studio_runtime_process::child_priority::ChildPriority;
use thiserror::Error;

const AUTH_FILE_LIMIT: u64 = 1024 * 1024;
const CLAUDE_OUTPUT_LIMIT: usize = 1024 * 1024;
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(8);
const CLAUDE_BIN_OVERRIDE: &str = "STUDIO_CLAUDE_BIN";
const AUTH_ERROR: &str = "Cannot read this profile's authentication";
/// Error text of Python `codex_accounts.auth_metadata` for an unreadable,
/// malformed or oversized Codex `auth.json`.
const CODEX_AUTH_ERROR: &str = "Cannot read this Codex profile's authentication";
/// Refusal text of a profile whose live login is a different account.
pub const CHANGED_ERROR: &str =
    "This profile's account changed. Restore its original login or add a separate profile.";
const UNVERIFIED_ERROR: &str =
    "Cannot verify this profile's account. Sign in to its original account again.";

fn truthy_value(value: &Value) -> bool {
    studio_runtime_store::truthy(Some(value))
}

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

#[derive(Clone, Eq, PartialEq)]
pub struct AuthMetadata {
    pub status: AuthStatus,
    pub account_id: Option<String>,
    pub email: Option<String>,
    pub plan: Option<String>,
    pub credential_identity: Option<String>,
    pub error: Option<&'static str>,
    pub error_kind: Option<&'static str>,
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
            error_kind: None,
        }
    }

    fn error(kind: Option<&'static str>) -> Self {
        Self {
            status: AuthStatus::Error,
            error: Some(AUTH_ERROR),
            error_kind: kind,
            ..Self::signed_out()
        }
    }

    fn codex_error() -> Self {
        Self {
            error: Some(CODEX_AUTH_ERROR),
            ..Self::error(None)
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
pub struct AuthProfile {
    pub provider: studio_runtime_providers::ProviderKind,
    pub home: PathBuf,
    pub claude_options: Option<Value>,
}

pub trait ProviderAuthReader: Send + Sync {
    fn read(&self, profile: &AuthProfile) -> AuthMetadata;

    /// Forget any cached status of `profile`; the next read probes once.
    fn invalidate(&self, _profile: &AuthProfile) {}

    /// The profiles of the registry changed: watch exactly their credential files.
    fn watch(&self, _profiles: &[AuthProfile]) {}

    /// Wall time of the last write to the profile's Codex credential file, in
    /// Unix seconds. `None` when it is unknown or the profile is not Codex.
    fn codex_credentials_written_at(&self, _profile: &AuthProfile) -> Option<f64> {
        None
    }
}

/// Reads auth status through the profile's provider adapter and the shared
/// per-account status cache.
pub struct SystemProviderAuthReader;

impl ProviderAuthReader for SystemProviderAuthReader {
    fn read(&self, profile: &AuthProfile) -> AuthMetadata {
        crate::provider_adapter::adapter(profile.provider).read_auth(profile)
    }

    fn invalidate(&self, profile: &AuthProfile) {
        auth_cache::shared().invalidate(
            &crate::provider_adapter::adapter(profile.provider).credential_location(profile),
        );
    }

    fn codex_credentials_written_at(&self, profile: &AuthProfile) -> Option<f64> {
        (profile.provider == studio_runtime_providers::ProviderKind::Codex)
            .then(|| codex_credentials_written_at(&profile.home))
            .flatten()
    }

    fn watch(&self, profiles: &[AuthProfile]) {
        auth_cache::shared().watch(
            profiles
                .iter()
                .map(|profile| {
                    crate::provider_adapter::adapter(profile.provider).credential_location(profile)
                })
                .collect(),
        );
    }
}

/// Account record fields that identify the credentials a session runs with.
const AUTH_PIN_FIELDS: [&str; 10] = [
    "id",
    "provider",
    "home",
    "claudeOptions",
    "accountId",
    "_credentialIdentity",
    "deleted",
    "disconnected",
    "duplicateOf",
    "updatedAt",
];

/// Advances when a cached auth status of this process may have changed.
pub fn auth_status_changes() -> tokio::sync::watch::Receiver<u64> {
    auth_cache::shared().changes()
}

/// Codex status: `<home>/auth.json` through the shared cache, never a spawn.
pub(crate) fn read_codex_auth_cached(profile: &AuthProfile) -> AuthMetadata {
    auth_cache::shared().read(&codex_location(profile), || {
        codex_auth_metadata(&profile.home)
    })
}

/// Claude status: `claude auth status` through the shared cache.
pub(crate) fn read_claude_auth_cached(profile: &AuthProfile) -> AuthMetadata {
    ClaudeCliAuthReader.read(profile)
}

/// Credential files whose change ends a cached Claude status.
const CLAUDE_CREDENTIAL_FILES: &[auth_cache::CredentialFile] = &[
    auth_cache::CredentialFile {
        name: ".credentials.json",
        format: auth_cache::CredentialFormat::ClaudeToken,
    },
    auth_cache::CredentialFile {
        name: ".claude.json",
        format: auth_cache::CredentialFormat::ClaudeAccount,
    },
];
/// Credential files whose change ends a cached Codex status.
const CODEX_CREDENTIAL_FILES: &[auth_cache::CredentialFile] = &[auth_cache::CredentialFile {
    name: "auth.json",
    format: auth_cache::CredentialFormat::CodexAuth,
}];

pub(crate) fn codex_location(profile: &AuthProfile) -> auth_cache::CredentialLocation {
    auth_cache::CredentialLocation {
        identity: "codex".to_owned(),
        directories: vec![profile.home.clone()],
        credential_files: CODEX_CREDENTIAL_FILES,
    }
}

/// The directory that `claude auth status` reads: the profile's `configDir`,
/// else `CLAUDE_CONFIG_DIR`, else `~/.claude` (whose account file is `~/.claude.json`).
pub(crate) fn claude_location(profile: &AuthProfile) -> auth_cache::CredentialLocation {
    let options = profile.claude_options.as_ref();
    let configured = |name: &str| {
        options
            .and_then(|value| value.get(name))
            .and_then(Value::as_str)
            .filter(|value| !value.trim().is_empty())
            .map(|value| value.trim().to_owned())
    };
    let home = env::var_os("HOME").map(PathBuf::from);
    let config_dir = configured("configDir")
        .map(PathBuf::from)
        .or_else(|| env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from));
    let (directories, source) = match (config_dir, home) {
        (Some(config), _) => (vec![config.clone()], config),
        (None, Some(home)) => (vec![home.join(".claude"), home.clone()], home),
        (None, None) => (Vec::new(), PathBuf::new()),
    };
    auth_cache::CredentialLocation {
        identity: format!(
            "claude\0{}\0{}",
            configured("binaryPath").unwrap_or_default(),
            source.display()
        ),
        directories,
        credential_files: CLAUDE_CREDENTIAL_FILES,
    }
}

/// Codex auth metadata is derived from `<home>/auth.json`; the file is never
/// copied, returned or included in an error.
pub fn codex_auth_metadata(home: &Path) -> AuthMetadata {
    match read_codex_auth(home) {
        CodexAuth::Missing => AuthMetadata::signed_out(),
        CodexAuth::Unreadable => AuthMetadata::codex_error(),
        CodexAuth::Parsed(metadata) => metadata,
    }
}

/// Modification time of `<home>/auth.json` in Unix seconds. Only file metadata
/// is read; the credentials themselves are not opened.
pub fn codex_credentials_written_at(home: &Path) -> Option<f64> {
    let modified = fs::metadata(home.join("auth.json")).ok()?.modified().ok()?;
    let seconds = modified
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs_f64();
    seconds.is_finite().then_some(seconds)
}

/// Python `auth_metadata` writes a different field set for each of these cases.
enum CodexAuth {
    Missing,
    Unreadable,
    Parsed(AuthMetadata),
}

fn read_codex_auth(home: &Path) -> CodexAuth {
    let path = home.join("auth.json");
    let file = match File::open(&path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return CodexAuth::Missing;
        }
        Err(_) => return CodexAuth::Unreadable,
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
        return CodexAuth::Unreadable;
    };
    codex_metadata_from_value(&auth).map_or(CodexAuth::Unreadable, CodexAuth::Parsed)
}

fn codex_metadata_from_value(auth: &Value) -> Result<AuthMetadata, ()> {
    let auth = auth.as_object().ok_or(())?;
    let tokens = object_if_truthy(auth.get("tokens"))?;
    let token = tokens.and_then(|tokens| {
        tokens
            .get("id_token")
            .filter(|value| studio_runtime_store::truthy(Some(value)))
            .or_else(|| {
                tokens
                    .get("access_token")
                    .filter(|value| studio_runtime_store::truthy(Some(value)))
            })
    });
    let claims = match token.and_then(Value::as_str) {
        Some(token) => token
            .split('.')
            .nth(1)
            .and_then(base64url_decode)
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .map(|value| value.as_object().cloned().ok_or(()))
            .transpose()?
            .unwrap_or_default(),
        None => serde_json::Map::new(),
    };
    let details = object_if_truthy(claims.get("https://api.openai.com/auth"))?;
    let profile = object_if_truthy(claims.get("https://api.openai.com/profile"))?;
    let account_id = tokens
        .and_then(|tokens| tokens.get("account_id"))
        .filter(|value| studio_runtime_store::truthy(Some(value)))
        .or_else(|| {
            details
                .and_then(|details| details.get("chatgpt_account_id"))
                .filter(|value| studio_runtime_store::truthy(Some(value)))
        })
        .map(|value| value.as_str().map(str::to_owned).ok_or(()))
        .transpose()?;
    let email = claims
        .get("email")
        .filter(|value| studio_runtime_store::truthy(Some(value)))
        .or_else(|| {
            profile
                .and_then(|profile| profile.get("email"))
                .filter(|value| studio_runtime_store::truthy(Some(value)))
        })
        .map(|value| value.as_str().map(str::to_owned).ok_or(()))
        .transpose()?;
    let api_key = auth
        .get("OPENAI_API_KEY")
        .filter(|value| studio_runtime_store::truthy(Some(value)));
    if api_key.is_some_and(|value| value.as_str().is_none()) {
        return Err(());
    }
    let api_key = api_key.and_then(Value::as_str);
    let ready = (account_id.as_ref().is_some_and(|value| !value.is_empty())
        && tokens
            .and_then(|tokens| tokens.get("access_token"))
            .is_some_and(truthy_value))
        || api_key.is_some();
    let credential_identity = account_id
        .as_ref()
        .filter(|value| !value.is_empty())
        .map(|value| format!("chatgpt:{value}"))
        .or_else(|| api_key.map(|value| format!("api:{}", hex_sha256(value.as_bytes()))));
    let plan = details
        .and_then(|details| details.get("chatgpt_plan_type"))
        .filter(|value| studio_runtime_store::truthy(Some(value)))
        .map(|value| value.as_str().map(str::to_owned).ok_or(()))
        .transpose()?
        .or_else(|| api_key.map(|_| "API key".to_owned()));
    Ok(AuthMetadata {
        status: if ready {
            AuthStatus::Ready
        } else {
            AuthStatus::SignedOut
        },
        account_id,
        email,
        plan,
        credential_identity,
        error: None,
        error_kind: None,
    })
}

fn object_if_truthy(value: Option<&Value>) -> Result<Option<&serde_json::Map<String, Value>>, ()> {
    match value.filter(|value| studio_runtime_store::truthy(Some(value))) {
        None => Ok(None),
        Some(value) => value.as_object().map(Some).ok_or(()),
    }
}

struct ClaudeCliAuthReader;

impl ProviderAuthReader for ClaudeCliAuthReader {
    fn invalidate(&self, profile: &AuthProfile) {
        auth_cache::shared().invalidate(&claude_location(profile));
    }

    fn read(&self, profile: &AuthProfile) -> AuthMetadata {
        Self::read_native(profile)
    }
}

impl ClaudeCliAuthReader {
    /// `claude auth status --json` for this profile, through the shared cache.
    fn read_native(profile: &AuthProfile) -> AuthMetadata {
        let environment = env::var(CLAUDE_BIN_OVERRIDE)
            .ok()
            .map(|value| BTreeMap::from([(CLAUDE_BIN_OVERRIDE.to_owned(), value)]))
            .unwrap_or_default();
        Self::read_with_environment(profile, &environment)
    }

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
        let Some(binary) = resolve_claude_binary(
            configured_binary,
            environment.get(CLAUDE_BIN_OVERRIDE).map(String::as_str),
            path.as_deref(),
            home.as_deref(),
        ) else {
            return AuthMetadata::signed_out();
        };
        auth_cache::shared().read(&claude_location(profile), || {
            match run_claude_status(&binary, options) {
                Ok((stdout, succeeded)) => {
                    let metadata = parse_claude_status(&stdout, succeeded);
                    let location = claude_location(profile);
                    location
                        .directories
                        .last()
                        .map_or(metadata.clone(), |directory| {
                            claude_identity::with_native_identity(
                                metadata,
                                &directory.join(".claude.json"),
                            )
                        })
                }
                Err(kind) => AuthMetadata::error(Some(kind)),
            }
        })
    }
}

fn resolve_claude_binary(
    configured: Option<&str>,
    override_binary: Option<&str>,
    path: Option<&OsStr>,
    home: Option<&OsStr>,
) -> Option<String> {
    let setting = configured
        .filter(|value| !value.is_empty())
        .or_else(|| override_binary.filter(|value| !value.is_empty()));
    if let Some(binary) = setting {
        return find_executable(binary, path).map(|path| path.to_string_lossy().into_owned());
    }
    if let Some(binary) = find_executable("claude", path) {
        return Some(binary.to_string_lossy().into_owned());
    }
    home.map(PathBuf::from)
        .map(|home| home.join(".local/bin/claude"))
        .filter(|candidate| is_executable(candidate))
        .map(|binary| binary.to_string_lossy().into_owned())
}

fn find_executable(binary: &str, path: Option<&OsStr>) -> Option<PathBuf> {
    let candidate = PathBuf::from(binary);
    if candidate.components().count() > 1 || candidate.is_absolute() {
        return is_executable(&candidate).then_some(candidate);
    }
    path.and_then(|path| {
        env::split_paths(path)
            .map(|directory| directory.join(binary))
            .find(|candidate| is_executable(candidate))
    })
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

fn run_claude_status(
    binary: &str,
    options: Option<&Value>,
) -> Result<(Vec<u8>, bool), &'static str> {
    run_claude_status_with_timeout(binary, options, CLAUDE_TIMEOUT)
}

#[allow(
    clippy::disallowed_methods,
    reason = "auth probes execute on account-driver blocking workers"
)]
fn run_claude_status_with_timeout(
    binary: &str,
    options: Option<&Value>,
    timeout: Duration,
) -> Result<(Vec<u8>, bool), &'static str> {
    let mut command = CommandWrap::with_new(binary, |command| {
        configure_claude_status_command(command, options);
    });
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    ChildPriority::Normal.apply_before_spawn(&mut command);
    let mut child = command.spawn().map_err(|_| "spawn")?;
    let Some(stdout) = child.stdout().take() else {
        return Err("spawn");
    };
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
    #[cfg(unix)]
    let status = loop {
        if exceeded.load(std::sync::atomic::Ordering::Relaxed) {
            terminate_claude_process_group(child);
            return Err("parser");
        }
        // The leader stays unreaped, so its group can still be signalled. When
        // its exit cannot be read, it may be reaped already: signal nothing.
        let Ok(exited) =
            studio_runtime_process::owned_process_group::exited_unreaped_now(child.id())
        else {
            return Err("spawn");
        };
        if exited && finished.load(std::sync::atomic::Ordering::Acquire) {
            break child
                .inner_mut()
                .try_wait()
                .map_err(|_| "spawn")?
                .ok_or("spawn")?;
        }
        if started.elapsed() >= timeout {
            terminate_claude_process_group(child);
            return Err("timeout");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    #[cfg(windows)]
    let status = loop {
        if exceeded.load(std::sync::atomic::Ordering::Relaxed) {
            let _kill_result = child.kill();
            return Err("parser");
        }
        if let Some(status) = child.try_wait().map_err(|_| "spawn")? {
            if finished.load(std::sync::atomic::Ordering::Acquire) {
                break status;
            }
        }
        if started.elapsed() >= timeout {
            let _kill_result = child.kill();
            return Err("timeout");
        }
        std::thread::sleep(Duration::from_millis(20));
    };
    while !finished.load(std::sync::atomic::Ordering::Acquire) {
        if started.elapsed() >= timeout {
            return Err("timeout");
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let output = reader.join().map_err(|_| "parser")?;
    if output.len() <= CLAUDE_OUTPUT_LIMIT {
        Ok((output, status.success()))
    } else {
        Err("parser")
    }
}

#[cfg(unix)]
fn terminate_claude_process_group(mut child: Box<dyn ChildWrapper>) {
    if child.start_kill().is_err() {
        let _direct_kill_result = child.inner_mut().start_kill();
    }
    let deadline = Instant::now() + Duration::from_millis(100);
    while Instant::now() < deadline {
        if child.inner_mut().try_wait().ok().flatten().is_some() {
            return;
        }
        std::thread::park_timeout(Duration::from_millis(10));
    }
    let _reaper = std::thread::Builder::new()
        .name("claude-status-reaper".to_owned())
        .spawn(move || {
            loop {
                if child.inner_mut().try_wait().ok().flatten().is_some() {
                    break;
                }
                std::thread::park_timeout(Duration::from_millis(25));
            }
        });
}

fn configure_claude_status_command(command: &mut Command, options: Option<&Value>) {
    command.args(["auth", "status", "--json"]);
    for name in claude::SUBSCRIPTION_ENV_REMOVALS {
        command.env_remove(name);
    }
    if let Some(config_dir) = options
        .and_then(|value| value.get("configDir"))
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        command.env("CLAUDE_CONFIG_DIR", config_dir.trim());
    }
    // `Command` inherits other environment values, matching Python's
    // subscription_env while ensuring the credential environment is cleared.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
}

#[derive(Default)]
struct ClaudeStatusOutput {
    logged_in: Option<bool>,
    auth_method: Option<String>,
    email: Option<String>,
    subscription_type: Option<String>,
    organization_uuid: Option<String>,
    account_uuid: Option<String>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum Lenient<T> {
    Expected(T),
    Other(IgnoredAny),
}

fn lenient_value<T>(value: Lenient<T>) -> Option<T> {
    match value {
        Lenient::Expected(value) => Some(value),
        Lenient::Other(_) => None,
    }
}

impl<'de> Deserialize<'de> for ClaudeStatusOutput {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        struct ClaudeStatusVisitor;
        impl<'de> Visitor<'de> for ClaudeStatusVisitor {
            type Value = ClaudeStatusOutput;

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a Claude auth status object")
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: MapAccess<'de>,
            {
                let mut value = ClaudeStatusOutput::default();
                while let Some(key) = map.next_key::<String>()? {
                    match key.as_str() {
                        "loggedIn" => {
                            value.logged_in = lenient_value(map.next_value::<Lenient<bool>>()?);
                        }
                        "authMethod" => {
                            value.auth_method = lenient_value(map.next_value::<Lenient<String>>()?);
                        }
                        "email" => {
                            value.email = lenient_value(map.next_value::<Lenient<String>>()?);
                        }
                        "orgId" | "organizationUuid" => {
                            value.organization_uuid =
                                lenient_value(map.next_value::<Lenient<String>>()?);
                        }
                        "accountUuid" => {
                            value.account_uuid =
                                lenient_value(map.next_value::<Lenient<String>>()?);
                        }
                        "subscriptionType" => {
                            value.subscription_type =
                                lenient_value(map.next_value::<Lenient<String>>()?);
                        }
                        _ => {
                            let _ignored = map.next_value::<IgnoredAny>()?;
                        }
                    }
                }
                Ok(value)
            }
        }
        deserializer.deserialize_map(ClaudeStatusVisitor)
    }
}

fn parse_claude_status(stdout: &[u8], succeeded: bool) -> AuthMetadata {
    parse_claude_status_with_outcome(stdout, succeeded).0
}

/// Parse the native status result and report whether it is conclusive enough
/// to tell the user that Claude rejected the completed sign-in.
pub(crate) fn parse_claude_status_with_outcome(
    stdout: &[u8],
    succeeded: bool,
) -> (AuthMetadata, bool) {
    let Ok(value) = serde_json::from_slice::<ClaudeStatusOutput>(stdout) else {
        return (AuthMetadata::error(Some("parser")), false);
    };
    let metadata = if !succeeded
        || value.logged_in != Some(true)
        || value.auth_method.as_deref() != Some("claude.ai")
    {
        AuthMetadata::signed_out()
    } else {
        let Some(email) = value.email.as_deref().filter(|email| !email.is_empty()) else {
            return (AuthMetadata::error(Some("parser")), false);
        };
        let account_id = format!("claude:{email}");
        AuthMetadata {
            status: AuthStatus::Ready,
            account_id: Some(account_id.clone()),
            email: Some(email.to_owned()),
            plan: value.subscription_type.filter(|plan| !plan.is_empty()),
            credential_identity: Some(claude_identity::identity(
                email,
                value.organization_uuid.as_deref(),
                value.account_uuid.as_deref(),
            )),
            error: None,
            error_kind: None,
        }
    };
    let conclusive = if !succeeded || metadata.status == AuthStatus::Error {
        false
    } else {
        match value.logged_in {
            Some(false) => true,
            Some(true) => value
                .auth_method
                .as_deref()
                .filter(|method| !method.is_empty())
                .is_some_and(|method| {
                    method != "claude.ai" || metadata.status == AuthStatus::Ready
                }),
            None => false,
        }
    };
    (metadata, conclusive)
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
    #[error("Registry replacement completed but directory durability is unconfirmed")]
    DurabilityUnconfirmed,
    #[error("Account registry lock is unavailable")]
    Poisoned,
    #[error("Sign in to this account first")]
    SignInRequired,
    #[error("This account was deleted")]
    DeletedAccount,
    #[error("{0}")]
    InvalidAccountOrder(&'static str),
    #[error("This name request id has different content")]
    NameRequestConflict,
    #[error("This delete request id has different content")]
    DeleteRequestConflict,
    #[error("Connect another account before disconnecting the application default")]
    NoDisconnectReplacement,
    #[error("Add another connected account before deleting the application default")]
    NoDeleteReplacement,
    #[error("This directory has no readable Codex auth.json")]
    InvalidCodexProfile,
    #[error("The sign-in response is still unknown. Check its status first.")]
    LoginResponseUnknown,
    #[error("Unknown sign-in request")]
    UnknownLogin,
    #[error("Not implemented: requires provider-session/codex-login-cancel")]
    LoginCancellationUnavailable,
}

pub struct AccountDriver<R = SystemProviderAuthReader> {
    path: PathBuf,
    state: Mutex<RegistryState>,
    reader: R,
    change_signal: AccountChangeSignal,
}

struct RegistryState {
    value: Value,
    order: RegistryOrder,
    /// Account groups and the default account order, kept beside the registry.
    account_order: crate::account_preference::AccountOrder,
    revision: u64,
    change_signal: AccountChangeSignal,
}

impl RegistryState {
    /// The public projection with the account order applied.
    fn public_snapshot(&self) -> Result<AccountsSnapshot, AccountDriverError> {
        let mut snapshot = project_snapshot(&self.value, &self.order)?;
        self.account_order.apply(&mut snapshot);
        Ok(snapshot)
    }
}

/// Coalescing signal for public profile and retained-session changes.
/// Its generation is independent of the persisted account revision.
#[derive(Clone)]
pub struct AccountChangeSignal {
    sender: tokio::sync::watch::Sender<u64>,
}

impl AccountChangeSignal {
    pub fn new() -> Self {
        let (sender, _) = tokio::sync::watch::channel(0);
        Self { sender }
    }

    pub fn subscribe(&self) -> tokio::sync::watch::Receiver<u64> {
        self.sender.subscribe()
    }

    /// Publishes a retained-session change after its registry guard is released.
    pub fn publish_session_change(&self) {
        self.sender
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

impl Default for AccountChangeSignal {
    fn default() -> Self {
        Self::new()
    }
}

impl RegistryState {
    fn publish_profile_change(&mut self) {
        self.revision = self.revision.wrapping_add(1);
        self.change_signal
            .sender
            .send_modify(|generation| *generation = generation.wrapping_add(1));
    }
}

#[derive(Clone, serde::Deserialize)]
pub struct CodexLoginRequest {
    pub login_id: String,
    pub account_key: Option<String>,
    pub email: Option<String>,
    pub label: Option<String>,
}

#[derive(Debug, Error)]
pub enum AccountMutationError {
    #[error(transparent)]
    Driver(#[from] AccountDriverError),
    #[error("{0}")]
    Refused(&'static str),
}

pub enum LoginReservation {
    Dispatch(Value),
    Replay(Value),
}

fn login_terminal(receipt: &Value) -> bool {
    matches!(
        receipt["status"].as_str(),
        Some("ready" | "duplicate" | "cancelled" | "error")
    )
}

/// The user-readable error of a sign-in request that the provider never received.
pub const LOGIN_NOT_SENT_ERROR: &str = "Could not start sign-in. Try again.";

/// Lifetime of a Codex sign-in receipt whose provider reply gave no expiry, in
/// seconds after its `createdAt`. A receipt that is still active after this
/// time is expired, so a lost provider completion cannot keep it active.
pub const CODEX_LOGIN_RECEIPT_LIFETIME_SECONDS: f64 = 30.0 * 60.0;

/// The user-readable error of a Codex sign-in receipt that expired.
pub const LOGIN_EXPIRED_ERROR: &str = "Sign-in expired. Start it again.";

/// Wall time, in Unix seconds, after which an active receipt is expired: the
/// provider's `expiresAt` when it gave one, else `createdAt` plus
/// [`CODEX_LOGIN_RECEIPT_LIFETIME_SECONDS`]. `None` without a valid time.
fn login_deadline(receipt: &Value) -> Option<f64> {
    receipt
        .get("expiresAt")
        .and_then(Value::as_f64)
        .or_else(|| {
            receipt
                .get("createdAt")
                .and_then(Value::as_f64)
                .map(|created| created + CODEX_LOGIN_RECEIPT_LIFETIME_SECONDS)
        })
        .filter(|deadline| deadline.is_finite())
}

/// A sign-in account that never received a provider identity and whose every
/// sign-in request has ended (error or cancelled). Such a row is hidden from
/// the account list; a later reservation removes it so failed or abandoned
/// sign-ins do not accumulate. A row with an identity, a live or uncertain
/// request, or a place in the archive is never a candidate.
fn abandoned_login_account(value: &Value, key: &str) -> bool {
    if !key.starts_with("login-") || value["defaultAccountKey"] == key {
        return false;
    }
    let Ok(row) = registry_account(value, key) else {
        return false;
    };
    if row.get("accountId").is_some_and(truthy_value)
        || row.get("duplicateOf").is_some_and(truthy_value)
        || row.get("deleted").is_some_and(truthy_value)
        || row.get("disconnected").is_some_and(truthy_value)
        || row.get("status").and_then(Value::as_str) == Some("ready")
    {
        return false;
    }
    let mut receipts = value
        .get("logins")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .map(|(_, receipt)| receipt)
        .filter(|receipt| receipt["accountKey"] == key)
        .peekable();
    receipts.peek().is_some()
        && receipts.all(|receipt| matches!(receipt["status"].as_str(), Some("cancelled" | "error")))
}

fn existing_login(
    value: &Value,
    request: &CodexLoginRequest,
) -> Result<Option<Value>, AccountMutationError> {
    let Some(receipt) = value
        .get("logins")
        .and_then(|logins| logins.get(&request.login_id))
    else {
        return Ok(None);
    };
    for (field, expected) in [
        ("reauthAccountKey", &request.account_key),
        ("emailHint", &request.email),
        ("requestedLabel", &request.label),
    ] {
        if receipt.get(field).and_then(Value::as_str) != expected.as_deref() {
            return Err(AccountMutationError::Refused(
                "This sign-in request belongs to a different account",
            ));
        }
    }
    Ok(Some(receipt.clone()))
}

impl<R: ProviderAuthReader> AccountDriver<R> {
    /// Inspect before preparing an isolated login home. Reserving below repeats
    /// the identity check under the registry lock, so concurrent callers send once.
    pub fn codex_login_receipt(
        &self,
        request: &CodexLoginRequest,
    ) -> Result<Option<Value>, AccountMutationError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        existing_login(&state.value, request)
    }

    /// Reconcile local Codex auth without sending a request. A still-pending
    /// receipt writes nothing, including when the retained session is absent.
    pub fn reconcile_codex_login(
        &self,
        request: &str,
    ) -> Result<Option<Value>, AccountMutationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let Some(receipt) = state.value.get("logins").and_then(|rows| rows.get(request)) else {
            return Ok(None);
        };
        if login_terminal(receipt) {
            return Ok(Some(receipt.clone()));
        }
        let account = receipt["accountKey"]
            .as_str()
            .ok_or(AccountDriverError::InvalidRegistry)?
            .to_owned();
        if studio_runtime_providers::ProviderKind::key_of(registry_account(&state.value, &account)?)
            != Some("codex")
        {
            return Err(AccountMutationError::Refused(
                "Select a Codex sign-in request",
            ));
        }
        let mut value = state.value.clone();
        if self.refresh_value(&mut value, &account, true)? != AccountStatus::Ready {
            return Ok(None);
        }
        reconcile_local_login(&mut value, &state.order, request, &account)?;
        let result = value["logins"][request].clone();
        if !login_terminal(&result) {
            return Ok(None);
        }
        if value != state.value {
            self.persist(&mut state, value)?;
        }
        Ok(Some(result))
    }

    /// Remove each [`abandoned_login_account`] from `value` and `order`. The
    /// credentials are read again first: a home that gained an identity after
    /// its request ended stays registered. The home directory stays on disk.
    fn remove_abandoned_logins(&self, value: &mut Value, order: &mut RegistryOrder) {
        let candidates: Vec<String> = order
            .accounts
            .iter()
            .filter(|key| abandoned_login_account(value, key))
            .cloned()
            .collect();
        for key in candidates {
            if self.refresh_value(value, &key, true).is_err()
                || !abandoned_login_account(value, &key)
            {
                continue;
            }
            if let Some(accounts) = value.get_mut("accounts").and_then(Value::as_object_mut) {
                accounts.remove(&key);
            }
            order.accounts.retain(|account| account != &key);
        }
    }

    /// The registry row of a Codex account whose sign-in request is being
    /// dispatched now (receipt status `starting`), without `_` fields. Such an
    /// account may start a provider session before it is ready, because the
    /// sign-in itself runs in that session. `None` for every other account.
    pub fn login_launch_record(&self, account: &str) -> Result<Option<Value>, AccountDriverError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let Ok(row) = registry_account(&state.value, account) else {
            return Ok(None);
        };
        let starting = state
            .value
            .get("logins")
            .and_then(Value::as_object)
            .is_some_and(|logins| {
                logins.values().any(|receipt| {
                    receipt["accountKey"] == account && receipt["status"] == "starting"
                })
            });
        if !starting
            || studio_runtime_providers::ProviderKind::key_of(row) != Some("codex")
            || row.get("deleted").is_some_and(truthy_value)
            || row.get("disconnected").is_some_and(truthy_value)
        {
            return Ok(None);
        }
        let mut row = row
            .as_object()
            .ok_or(AccountDriverError::InvalidRegistry)?
            .clone();
        row.retain(|field, _| !field.starts_with('_'));
        Ok(Some(Value::Object(row)))
    }

    /// `home` is prepared by the caller without holding this registry lock or a
    /// store transaction. A successful return is the durable intent commit point.
    pub fn reserve_codex_login(
        &self,
        request: &CodexLoginRequest,
        home: &Path,
        now: f64,
    ) -> Result<LoginReservation, AccountMutationError> {
        if uuid::Uuid::parse_str(&request.login_id).is_err() || !now.is_finite() {
            return Err(AccountMutationError::Refused("Supply a UUID request_id"));
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        if let Some(receipt) = existing_login(&state.value, request)? {
            return Ok(LoginReservation::Replay(receipt));
        }
        let mut value = state.value.clone();
        let mut order = state.order.clone();
        self.remove_abandoned_logins(&mut value, &mut order);
        let key = request
            .account_key
            .clone()
            .unwrap_or_else(|| format!("login-{}", request.login_id));
        let mut receipt = serde_json::json!({"requestId":request.login_id,"accountKey":key,"status":"starting","createdAt":now});
        if let Some(account) = &request.account_key {
            let row = registry_account(&value, account)?;
            if studio_runtime_providers::ProviderKind::key_of(row) != Some("codex")
                || !row.get("accountId").is_some_and(truthy_value)
                || row.get("duplicateOf").is_some_and(truthy_value)
            {
                return Err(AccountMutationError::Refused(
                    "Select a saved Codex subscription account",
                ));
            }
            if value
                .get("logins")
                .and_then(Value::as_object)
                .is_some_and(|logins| {
                    logins.values().any(|receipt| {
                        receipt["accountKey"] == *account
                            && matches!(
                                receipt["status"].as_str(),
                                Some("starting" | "pending" | "uncertain")
                            )
                    })
                })
            {
                return Err(AccountMutationError::Refused(
                    "Sign-in is already active for this account. Check its status first.",
                ));
            }
            receipt["reauthAccountKey"] = serde_json::json!(account);
            receipt["expectedAccountId"] = row["accountId"].clone();
            receipt["email"] = row.get("email").cloned().unwrap_or(Value::Null);
        } else {
            value["accounts"][&key] = serde_json::json!({"id":key,"home":home,"label":request.label.as_deref().unwrap_or("New account"),"source":"Codex Agents","accountId":null,"email":null,"plan":null,"status":"pending"});
            order.accounts.push(key);
            receipt["emailHint"] = serde_json::json!(request.email);
            receipt["requestedLabel"] = serde_json::json!(request.label);
        }
        if value.get("logins").is_none() {
            value["logins"] = serde_json::json!({});
        }
        value["logins"][&request.login_id] = receipt.clone();
        order.logins.push(request.login_id.clone());
        self.persist_registry(&mut state, value, &order)?;
        Ok(LoginReservation::Dispatch(receipt))
    }

    /// `not_sent_cause` names, in user-readable words, why a request with
    /// `sent == false` never reached the provider; it is added to the error.
    pub fn finish_codex_login(
        &self,
        request: &str,
        account: &str,
        response: Option<&Value>,
        sent: bool,
        not_sent_cause: Option<&str>,
        now: f64,
    ) -> Result<Value, AccountMutationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut value = state.value.clone();
        let receipt = value
            .get_mut("logins")
            .and_then(|logins| logins.get_mut(request))
            .ok_or(AccountDriverError::UnknownLogin)?;
        if receipt["accountKey"] != account {
            return Err(AccountMutationError::Refused(
                "The sign-in receipt belongs to another account",
            ));
        }
        if login_terminal(receipt)
            || receipt["status"] == "pending"
            || (receipt["status"] == "uncertain"
                && receipt.get("loginId").is_some_and(truthy_value))
        {
            return Ok(receipt.clone());
        }
        if let Some(response) = response.filter(|response| {
            response["type"] == "chatgptDeviceCode"
                && ["loginId", "verificationUrl", "userCode"]
                    .iter()
                    .all(|key| {
                        response
                            .get(*key)
                            .and_then(Value::as_str)
                            .is_some_and(|value| !value.is_empty() && value.len() <= 8192)
                    })
        }) {
            receipt["status"] = serde_json::json!("pending");
            if let Some(object) = receipt.as_object_mut() {
                object.remove("error");
            }
            for key in ["loginId", "verificationUrl", "userCode"] {
                receipt[key] = response[key].clone();
            }
            let expiry = response
                .get("expiresAt")
                .and_then(Value::as_f64)
                .or_else(|| {
                    response
                        .get("expiresIn")
                        .and_then(Value::as_f64)
                        .filter(|seconds| *seconds > 0.0)
                        .map(|seconds| now + seconds)
                });
            if let Some(expiry) = expiry.filter(|expiry| expiry.is_finite() && *expiry > now) {
                receipt["expiresAt"] = serde_json::json!(expiry);
            }
        } else {
            receipt["status"] = serde_json::json!(if sent { "uncertain" } else { "error" });
            receipt["error"] = serde_json::json!(match (sent, not_sent_cause) {
                (true, _) => {
                    "Sign-in response is unconfirmed. Check its status before starting again."
                        .to_owned()
                }
                (false, Some(cause)) => format!("Could not start sign-in: {cause}. Try again."),
                (false, None) => LOGIN_NOT_SENT_ERROR.to_owned(),
            });
        }
        let result = receipt.clone();
        if !sent && result.get("reauthAccountKey").is_none() {
            value["accounts"][account]["status"] = serde_json::json!("error");
            value["accounts"][account]["error"] = result["error"].clone();
        }
        self.persist(&mut state, value)?;
        Ok(result)
    }

    /// Latch cancellation before hand-off. Uncertain cancellation is inspected,
    /// never automatically sent again, including after restarting the process.
    pub fn reserve_codex_cancel(
        &self,
        request: &str,
    ) -> Result<LoginReservation, AccountMutationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut value = state.value.clone();
        let receipt = value
            .get_mut("logins")
            .and_then(|logins| logins.get_mut(request))
            .ok_or(AccountDriverError::UnknownLogin)?;
        if login_terminal(receipt) {
            return Ok(LoginReservation::Replay(receipt.clone()));
        }
        if !receipt.get("loginId").is_some_and(truthy_value) {
            return Err(AccountDriverError::LoginResponseUnknown.into());
        }
        if receipt["status"] == "uncertain" {
            return Ok(LoginReservation::Replay(receipt.clone()));
        }
        receipt["status"] = serde_json::json!("uncertain");
        receipt["error"] = serde_json::json!(
            "Cancellation is pending confirmation. Check status for a late receipt or completed sign-in."
        );
        let result = receipt.clone();
        self.persist(&mut state, value)?;
        Ok(LoginReservation::Dispatch(result))
    }

    pub fn finish_codex_cancel(
        &self,
        request: &str,
        account: &str,
        confirmed: bool,
    ) -> Result<Value, AccountMutationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut value = state.value.clone();
        let receipt = value
            .get_mut("logins")
            .and_then(|logins| logins.get_mut(request))
            .ok_or(AccountDriverError::UnknownLogin)?;
        if receipt["accountKey"] != account {
            return Err(AccountMutationError::Refused(
                "The sign-in receipt belongs to another account",
            ));
        }
        if login_terminal(receipt) || !confirmed {
            return Ok(receipt.clone());
        }
        receipt["status"] = serde_json::json!("cancelled");
        if let Some(object) = receipt.as_object_mut() {
            for field in ["error", "userCode", "verificationUrl"] {
                object.remove(field);
            }
        }
        let result = receipt.clone();
        if result.get("reauthAccountKey").is_none() {
            value["accounts"][account]["status"] = serde_json::json!("signedOut");
        }
        self.persist(&mut state, value)?;
        Ok(result)
    }

    /// Restore a cancellation whose retained-session request is known not to
    /// have been sent. The pending receipt may safely be retried; Unknown stays
    /// latched by `finish_codex_cancel` and is never dispatched again.
    pub(crate) fn finish_codex_cancel_not_sent(
        &self,
        request: &str,
        account: &str,
    ) -> Result<Value, AccountMutationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut value = state.value.clone();
        let receipt = value
            .get_mut("logins")
            .and_then(|logins| logins.get_mut(request))
            .ok_or(AccountDriverError::UnknownLogin)?;
        if receipt["accountKey"] != account {
            return Err(AccountMutationError::Refused(
                "The sign-in receipt belongs to another account",
            ));
        }
        if login_terminal(receipt) {
            return Ok(receipt.clone());
        }
        if receipt["status"] == "uncertain" {
            receipt["status"] = serde_json::json!("pending");
            receipt["error"] = serde_json::json!(
                "Cancellation is unconfirmed. Check status or retry cancellation."
            );
        }
        let result = receipt.clone();
        if value != state.value {
            self.persist(&mut state, value)?;
        }
        Ok(result)
    }

    pub fn recover_codex_logins(&self) -> Result<(), AccountMutationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut value = state.value.clone();
        if let Some(logins) = value.get_mut("logins").and_then(Value::as_object_mut) {
            for receipt in logins
                .values_mut()
                .filter(|receipt| receipt["status"] == "starting")
            {
                receipt["status"] = serde_json::json!("uncertain");
                receipt["error"] = serde_json::json!(
                    "Sign-in response is unconfirmed. Check status or start a separate attempt."
                );
            }
        }
        if value != state.value {
            self.persist(&mut state, value)?;
        }
        Ok(())
    }

    /// The retained-session notification adapter supplies the exact operation
    /// and verified metadata obtained outside any store/registry transaction.
    pub fn complete_codex_login(
        &self,
        request: &str,
        account: &str,
        native_id: &str,
        success: bool,
        metadata: Option<&AuthMetadata>,
    ) -> Result<Value, AccountMutationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut value = state.value.clone();
        let receipt = value
            .get_mut("logins")
            .and_then(|logins| logins.get_mut(request))
            .ok_or(AccountDriverError::UnknownLogin)?;
        if native_id.is_empty()
            || receipt["accountKey"] != account
            || receipt
                .get("loginId")
                .and_then(Value::as_str)
                .is_some_and(|saved| saved != native_id)
        {
            return Err(AccountMutationError::Refused(
                "The sign-in receipt belongs to another native request",
            ));
        }
        if matches!(
            receipt["status"].as_str(),
            Some("ready" | "duplicate" | "cancelled")
        ) {
            return Ok(receipt.clone());
        }
        receipt["loginId"] = serde_json::json!(native_id);
        if success {
            if receipt.get("reauthAccountKey").is_some() {
                receipt["nativeCompleted"] = serde_json::json!(true);
            }
            if let Some(metadata) = metadata {
                let row = registry_account_mut(&mut value, account)?
                    .as_object_mut()
                    .ok_or(AccountDriverError::InvalidRegistry)?;
                apply_metadata(row, metadata);
                if metadata.status == AuthStatus::Ready {
                    reconcile_local_login(&mut value, &state.order, request, account)?;
                }
            }
        } else {
            receipt["status"] = serde_json::json!("error");
            receipt["error"] = serde_json::json!("Sign-in failed or expired. Try again.");
            if let Some(object) = receipt.as_object_mut() {
                object.remove("userCode");
                object.remove("verificationUrl");
            }
            if receipt.get("reauthAccountKey").is_none() {
                value["accounts"][account]["status"] = serde_json::json!("error");
                value["accounts"][account]["error"] =
                    serde_json::json!("Sign-in failed or expired. Try again.");
            }
        }
        let result = value["logins"][request].clone();
        if value != state.value {
            self.persist(&mut state, value)?;
        }
        Ok(result)
    }

    /// Apply the provider's `account/login/completed` notification that the
    /// retained session of `account` delivered. Only an unfinished receipt of
    /// the same account whose native `loginId` is the notification's (or still
    /// unknown on a receipt that is still open) changes. Returns the receipts.
    pub fn codex_login_completed(
        &self,
        account: &str,
        params: &Value,
    ) -> Result<Vec<Value>, AccountMutationError> {
        let (Some(native_id), Some(success)) = (
            params
                .get("loginId")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty()),
            params.get("success").and_then(Value::as_bool),
        ) else {
            return Ok(Vec::new());
        };
        let (requests, profile) = {
            let state = self
                .state
                .lock()
                .map_err(|_| AccountDriverError::Poisoned)?;
            let requests: Vec<String> = state
                .value
                .get("logins")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter(|(_, receipt)| {
                    receipt["accountKey"] == account
                        && match receipt.get("loginId").and_then(Value::as_str) {
                            // A late completion may still finish an expired receipt.
                            Some(saved) => {
                                saved == native_id
                                    && !matches!(
                                        receipt["status"].as_str(),
                                        Some("ready" | "duplicate" | "cancelled")
                                    )
                            }
                            // The start reply was lost; the request is still open.
                            None => !login_terminal(receipt),
                        }
                })
                .map(|(request, _)| request.clone())
                .collect();
            if requests.is_empty() {
                return Ok(Vec::new());
            }
            (
                requests,
                auth_profile(registry_account(&state.value, account)?)?,
            )
        };
        // The provider has just written the credentials; read them again.
        let metadata = success.then(|| {
            self.reader.invalidate(&profile);
            self.reader.read(&profile)
        });
        requests
            .iter()
            .map(|request| {
                self.complete_codex_login(request, account, native_id, success, metadata.as_ref())
            })
            .collect()
    }

    /// Settle each unfinished Codex sign-in receipt whose provider completion
    /// is lost. A sign-in again request whose account credentials were written
    /// after the request started and before its deadline completed natively:
    /// it is ready only when those credentials hold the expected account id.
    /// An unfinished receipt after its deadline is expired with
    /// [`LOGIN_EXPIRED_ERROR`]. Returns whether the registry changed.
    pub fn reconcile_codex_logins(&self, now: f64) -> Result<bool, AccountMutationError> {
        if !now.is_finite() {
            return Ok(false);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let requests: Vec<String> = state
            .value
            .get("logins")
            .and_then(Value::as_object)
            .into_iter()
            .flatten()
            .filter(|(_, receipt)| !login_terminal(receipt))
            .map(|(request, _)| request.clone())
            .collect();
        if requests.is_empty() {
            return Ok(false);
        }
        let mut value = state.value.clone();
        for request in &requests {
            let receipt = value["logins"][request].clone();
            let Some(account) = receipt["accountKey"].as_str().map(str::to_owned) else {
                continue;
            };
            let Ok(row) = registry_account(&value, &account) else {
                continue;
            };
            if studio_runtime_providers::ProviderKind::key_of(row) != Some("codex") {
                continue;
            }
            let profile = auth_profile(row)?;
            let deadline = login_deadline(&receipt);
            if receipt.get("reauthAccountKey").is_some()
                && receipt.get("nativeCompleted") != Some(&Value::Bool(true))
                && receipt.get("loginId").is_some_and(truthy_value)
                && let (Some(created), Some(deadline)) =
                    (receipt.get("createdAt").and_then(Value::as_f64), deadline)
                && self
                    .reader
                    .codex_credentials_written_at(&profile)
                    .is_some_and(|written| written > created && written <= deadline)
            {
                value["logins"][request]["nativeCompleted"] = Value::Bool(true);
            }
            // Credentials of another account than the pinned one read as
            // Changed; a completed "sign in again" request then ends in error.
            let status = self.refresh_value(&mut value, &account, true)?;
            if status == AccountStatus::Ready
                || (status == AccountStatus::Changed
                    && value["logins"][request]["nativeCompleted"] == true)
            {
                reconcile_local_login(&mut value, &state.order, request, &account)?;
            }
            if login_terminal(&value["logins"][request])
                || deadline.is_none_or(|deadline| now <= deadline)
            {
                continue;
            }
            let expired = value["logins"][request]
                .as_object_mut()
                .ok_or(AccountDriverError::InvalidRegistry)?;
            expired.insert("status".into(), Value::String("error".into()));
            expired.insert("error".into(), Value::String(LOGIN_EXPIRED_ERROR.into()));
            expired.remove("userCode");
            expired.remove("verificationUrl");
            if receipt.get("reauthAccountKey").is_none() {
                let row = registry_account_mut(&mut value, &account)?;
                row["status"] = Value::String("error".into());
                row["error"] = Value::String(LOGIN_EXPIRED_ERROR.into());
            }
        }
        // Refreshed account rows alone are not persisted, as in a listing.
        if value.get("logins") == state.value.get("logins") {
            return Ok(false);
        }
        self.persist(&mut state, value)?;
        Ok(true)
    }

    pub fn update_claude_settings(
        &self,
        key: &str,
        options: Value,
        label: Option<&str>,
    ) -> Result<(), AccountMutationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut value = state.value.clone();
        let row = registry_account_mut(&mut value, key)?;
        if row["provider"] != "claude" {
            return Err(AccountMutationError::Refused(
                "Select a Claude Code profile",
            ));
        }
        if ["binaryPath", "configDir"]
            .iter()
            .any(|field| row["claudeOptions"].get(*field) != options.get(*field))
        {
            return Err(AccountMutationError::Refused(
                "Add a separate Claude profile to change the executable or config directory",
            ));
        }
        if label.is_some_and(|label| python_strip(label).is_empty() || label.chars().count() > 200)
        {
            return Err(AccountMutationError::Refused(
                "Invalid Claude profile label",
            ));
        }
        row["claudeOptions"] = options;
        if let Some(label) = label {
            row["label"] = serde_json::json!(python_strip(label));
        }
        if value != state.value {
            self.persist(&mut state, value)?;
        }
        Ok(())
    }

    /// Register local accounts: Codex homes, then the default Claude profile
    /// at `<user_home>/.claude` when the Claude CLI is installed. Registration
    /// reads the credentials as they are now, never a cached status.
    pub fn discover_local(&self, user_home: &Path) -> Result<(), AccountMutationError> {
        self.discover_codex()?;
        if AccountDriver::<SystemProviderAuthReader>::claude_discovery_available() {
            let home = user_home.join(".claude");
            let profile = AuthProfile {
                provider: studio_runtime_providers::ProviderKind::Claude,
                home: home.clone(),
                claude_options: None,
            };
            self.reader.invalidate(&profile);
            let metadata = self.reader.read(&profile);
            self.discover_claude_verified(&home, &metadata)?;
        }
        Ok(())
    }

    pub fn discover_claude_verified(
        &self,
        home: &Path,
        metadata: &AuthMetadata,
    ) -> Result<(), AccountMutationError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        if state.value["accounts"].get("claude-local").is_some() {
            return Ok(());
        }
        let mut value = state.value.clone();
        let mut row = serde_json::json!({"id":"claude-local","provider":"claude","home":home,"label":"Claude Code","source":"Claude Code","status":"signedOut"});
        apply_metadata(
            row.as_object_mut()
                .ok_or(AccountDriverError::InvalidRegistry)?,
            metadata,
        );
        value["accounts"]["claude-local"] = row;
        let mut order = state.order.clone();
        order.accounts.push("claude-local".into());
        self.persist_registry(&mut state, value, &order)?;
        Ok(())
    }
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
        let (value, order) =
            decode_registry(&bytes).map_err(|_| AccountDriverError::InvalidRegistry)?;
        let change_signal = AccountChangeSignal::new();
        reader.watch(&live_profiles(&value));
        let account_order =
            crate::account_preference::AccountOrder::read(&account_order_path(&path));
        Ok(Self {
            path,
            state: Mutex::new(RegistryState {
                value,
                order,
                account_order,
                revision: 0,
                change_signal: change_signal.clone(),
            }),
            reader,
            change_signal,
        })
    }

    pub fn change_signal(&self) -> AccountChangeSignal {
        self.change_signal.clone()
    }

    /// Revision advances only after a registry replacement has taken effect.
    pub fn revision(&self) -> Result<u64, AccountDriverError> {
        self.state
            .lock()
            .map(|state| state.revision)
            .map_err(|_| AccountDriverError::Poisoned)
    }

    /// Refresh one account and persist safe auth metadata before returning its
    /// admission status. Duplicate profiles return their stored status.
    pub fn get(&self, account: &str) -> Result<AccountStatus, AccountDriverError> {
        // The provider read can take seconds, so it runs on a copy outside the lock.
        let copied = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?
            .value
            .clone();
        let mut refreshed = copied.clone();
        let read_status = self.refresh_value(&mut refreshed, account, false)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        let row_unchanged =
            registry_account(&state.value, account)? == registry_account(&copied, account)?;
        let status = if row_unchanged {
            // Only this row was read, so its result still applies to the registry.
            *registry_account_mut(&mut updated, account)? =
                registry_account(&refreshed, account)?.clone();
            read_status
        } else {
            // The row changed during the read, so its result may describe an old profile.
            self.refresh_value(&mut updated, account, false)?
        };
        if updated != state.value {
            self.persist(&mut state, updated)?;
        }
        Ok(status)
    }

    /// Inspect auth state without persisting metadata when the caller may refuse the write.
    pub fn inspect(&self, account: &str) -> Result<AccountStatus, AccountDriverError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        self.refresh_value(&mut updated, account, false)
    }

    /// Return the current registry as the same public account projection used
    /// by startup reads. This does not refresh provider state.
    pub fn snapshot(&self) -> Result<AccountsSnapshot, AccountDriverError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        state.public_snapshot()
    }

    /// Internal selected-account record, including the non-secret identity pin.
    pub(crate) fn internal_record(&self, account: &str) -> Result<Value, AccountDriverError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        Ok(registry_account(&state.value, account)?.clone())
    }

    /// The account's turn start throttle, read under the registry lock
    /// without copying the row; the Codex defaults for an unknown account,
    /// like a row without a provider.
    pub fn start_throttle(&self, account: &str) -> crate::account_start_gate::StartThrottle {
        let state = self
            .state
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        crate::account_start_gate::StartThrottle::of_record(
            registry_account(&state.value, account).unwrap_or(&Value::Null),
        )
    }

    /// Stable, non-secret identity selected for a retained account session.
    pub fn auth_pin(&self, account: &str) -> Result<Value, AccountDriverError> {
        let record = self.internal_record(account)?;
        Ok(AUTH_PIN_FIELDS
            .iter()
            .map(|key| {
                (
                    (*key).to_owned(),
                    record.get(*key).cloned().unwrap_or(Value::Null),
                )
            })
            .collect())
    }

    /// Refresh the response projection from credentials without persisting it.
    pub fn listing_snapshot(&self) -> Result<AccountsSnapshot, AccountDriverError> {
        // Provider reads run on this copy, so the registry lock is not held during I/O.
        let (mut updated, order, account_order) = {
            let state = self
                .state
                .lock()
                .map_err(|_| AccountDriverError::Poisoned)?;
            (
                state.value.clone(),
                state.order.clone(),
                state.account_order.clone(),
            )
        };
        for key in &order.accounts {
            let stored = registry_account(&updated, key)?.clone();
            if self.refresh_value(&mut updated, key, false)? == AccountStatus::Error {
                // A listing cannot replace persisted status with an error when
                // the auth reader could not determine the account's state.
                *registry_account_mut(&mut updated, key)? = stored;
            }
        }
        let mut snapshot = project_snapshot(&updated, &order)?;
        account_order.apply(&mut snapshot);
        Ok(snapshot)
    }

    pub fn set_default(
        &self,
        account: Option<&str>,
    ) -> Result<AccountsSnapshot, AccountDriverError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let Some(account) = account else {
            return state.public_snapshot();
        };
        let mut updated = state.value.clone();
        self.refresh_value(&mut updated, account, true)?;
        if !eligible_default(&updated, account) {
            return Err(AccountDriverError::SignInRequired);
        }
        updated["defaultAccountKey"] = Value::String(account.to_owned());
        self.persist(&mut state, updated)?;
        state.public_snapshot()
    }

    pub fn set_name(
        &self,
        account: &str,
        label: &str,
        request_id: &str,
    ) -> Result<AccountsSnapshot, AccountDriverError> {
        let request_id = request_id.to_ascii_lowercase();
        let label = python_strip(label);
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        let content = serde_json::json!({"accountKey": account, "label": label});
        let previous = updated
            .get("nameReceipts")
            .and_then(|receipts| receipts.get(&request_id));
        if let Some(previous) = previous {
            if previous != &content {
                return Err(AccountDriverError::NameRequestConflict);
            }
        } else {
            let row = registry_account_mut(&mut updated, account)?;
            if row.get("deleted") == Some(&Value::Bool(true)) {
                return Err(AccountDriverError::DeletedAccount);
            }
            row["label"] = Value::String(label.to_owned());
            updated
                .as_object_mut()
                .ok_or(AccountDriverError::InvalidRegistry)?
                .entry("nameReceipts")
                .or_insert_with(|| Value::Object(Default::default()));
            updated["nameReceipts"][&request_id] = content;
            self.persist(&mut state, updated)?;
        }
        drop(state);
        self.snapshot()
    }

    /// Set or clear (`None`) the group of `account`. A repeated request sets the
    /// same value. Groups live in `account-order.json` beside the registry, so an
    /// older build that does not know them still reads the registry.
    pub fn set_group(
        &self,
        account: &str,
        group: Option<&str>,
    ) -> Result<AccountsSnapshot, AccountDriverError> {
        if let Some(group) = group {
            crate::account_preference::validate_groups(&[group.to_owned()])
                .map_err(AccountDriverError::InvalidAccountOrder)?;
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let row = registry_account(&state.value, account)?;
        if row.get("deleted") == Some(&Value::Bool(true)) {
            return Err(AccountDriverError::DeletedAccount);
        }
        let mut updated = state.account_order.clone();
        match group {
            Some(group) => {
                updated.groups.insert(account.to_owned(), group.to_owned());
            }
            None => {
                updated.groups.remove(account);
            }
        }
        self.persist_account_order(&mut state, updated)?;
        state.public_snapshot()
    }

    /// Replace the default account order of projects without their own order.
    /// An empty list removes it. A repeated request sets the same value.
    pub fn set_default_groups(
        &self,
        groups: &[String],
    ) -> Result<AccountsSnapshot, AccountDriverError> {
        crate::account_preference::validate_groups(groups)
            .map_err(AccountDriverError::InvalidAccountOrder)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.account_order.clone();
        updated.default_groups = groups.to_vec();
        self.persist_account_order(&mut state, updated)?;
        state.public_snapshot()
    }

    fn persist_account_order(
        &self,
        state: &mut RegistryState,
        updated: crate::account_preference::AccountOrder,
    ) -> Result<(), AccountDriverError> {
        if updated == state.account_order {
            return Ok(());
        }
        let bytes = updated.encode();
        let result = atomic_replace(&account_order_path(&self.path), &bytes);
        match result {
            Err(ReplaceError::NotReplaced) => return Err(AccountDriverError::WriteRegistry),
            Ok(()) | Err(ReplaceError::ReplacedNotDurable) => {
                state.account_order = updated;
                state.publish_profile_change();
            }
        }
        if matches!(result, Err(ReplaceError::ReplacedNotDurable)) {
            return Err(AccountDriverError::DurabilityUnconfirmed);
        }
        Ok(())
    }

    /// Replace the turn start throttle of `account`. A repeated request sets
    /// the same values; the change signal reaches waiting starts at once.
    pub fn set_start_throttle(
        &self,
        account: &str,
        throttle: crate::account_start_gate::StartThrottle,
    ) -> Result<AccountsSnapshot, AccountDriverError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        let row = registry_account_mut(&mut updated, account)?;
        if row.get("deleted") == Some(&Value::Bool(true)) {
            return Err(AccountDriverError::DeletedAccount);
        }
        row[crate::account_start_gate::REGISTRY_FIELD] = throttle.to_record();
        if updated != state.value {
            self.persist(&mut state, updated)?;
        }
        drop(state);
        self.snapshot()
    }

    pub fn delete(
        &self,
        account: &str,
        request_id: &str,
    ) -> Result<AccountsSnapshot, AccountDriverError> {
        let request_id = request_id.to_ascii_lowercase();
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        if let Some(previous) = state
            .value
            .get("deleteReceipts")
            .and_then(|receipts| receipts.get(&request_id))
        {
            if previous.get("accountKey").and_then(Value::as_str) != Some(account) {
                return Err(AccountDriverError::DeleteRequestConflict);
            }
            return state.public_snapshot();
        }
        registry_account(&state.value, account)?;
        let mut updated = state.value.clone();
        self.refresh_selectable(&mut updated, &state.order)?;
        let default = updated.get("defaultAccountKey").and_then(Value::as_str);
        let replacement = if default == Some(account) {
            ready_replacement(&updated, &state.order, account)
        } else {
            None
        };
        if default == Some(account) && replacement.is_none() {
            return Err(AccountDriverError::NoDeleteReplacement);
        }
        if let Some(replacement) = replacement {
            updated["defaultAccountKey"] = Value::String(replacement.to_owned());
        }
        registry_account_mut(&mut updated, account)?["deleted"] = Value::Bool(true);
        updated
            .as_object_mut()
            .ok_or(AccountDriverError::InvalidRegistry)?
            .entry("deleteReceipts")
            .or_insert_with(|| Value::Object(Default::default()));
        updated["deleteReceipts"][&request_id] = serde_json::json!({
            "accountKey": account,
            "deletedAt": std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| AccountDriverError::InvalidRegistry)?
                .as_secs_f64(),
        });
        self.persist(&mut state, updated)?;
        state.public_snapshot()
    }

    pub fn disconnect(&self, account: &str) -> Result<AccountsSnapshot, AccountDriverError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        registry_account(&updated, account)?;
        self.refresh_selectable(&mut updated, &state.order)?;
        let row = registry_account(&updated, account)?;
        let is_disconnected = row.get("disconnected") == Some(&Value::Bool(true));
        let default = updated.get("defaultAccountKey").and_then(Value::as_str);
        if !is_disconnected && default == Some(account) {
            let replacement = ready_replacement(&updated, &state.order, account)
                .ok_or(AccountDriverError::NoDisconnectReplacement)?;
            updated["defaultAccountKey"] = Value::String(replacement.to_owned());
        }
        if !is_disconnected {
            registry_account_mut(&mut updated, account)?["disconnected"] = Value::Bool(true);
            self.persist(&mut state, updated)?;
        } else if updated != state.value {
            self.persist(&mut state, updated)?;
        }
        state.public_snapshot()
    }

    pub fn reconnect(&self, account: &str) -> Result<AccountsSnapshot, AccountDriverError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        self.refresh_value(&mut updated, account, true)?;
        let row = registry_account_mut(&mut updated, account)?;
        if row.get("status").and_then(Value::as_str) != Some("ready") {
            return Err(AccountDriverError::SignInRequired);
        }
        row.as_object_mut()
            .ok_or(AccountDriverError::InvalidRegistry)?
            .remove("disconnected");
        self.persist(&mut state, updated)?;
        state.public_snapshot()
    }

    pub fn register(&self, home: &str) -> Result<AccountsSnapshot, AccountDriverError> {
        if python_strip(home).is_empty() {
            return Err(AccountDriverError::InvalidCodexProfile);
        }
        let path = CanonicalPath::existing(expanded_profile_path(home))
            .map_err(|_| AccountDriverError::InvalidCodexProfile)?;
        let metadata = codex_auth_metadata(&path);
        if metadata.status != AuthStatus::Ready {
            return Err(AccountDriverError::InvalidCodexProfile);
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        let mut order = state.order.clone();
        if register_metadata(&mut updated, &mut order, &path, &metadata, true)? {
            self.persist_registry(&mut state, updated, &order)?;
        }
        state.public_snapshot()
    }

    pub fn discover_codex(&self) -> Result<AccountsSnapshot, AccountDriverError> {
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let codex_home = resolve_codex_home(env::var_os("CODEX_HOME").as_deref(), &home)
            .map_err(|_| AccountDriverError::InvalidCodexProfile)?;
        let mut candidates = vec![codex_home.clone(), home.join(".codex")];
        for (base, pattern) in [
            (home.join("Projects"), "*/.codex-profile"),
            (home.join("Projects"), "*/.codex"),
            (home.join(".codex/profiles"), "*"),
            (home.join(".codex-profiles"), "*"),
        ] {
            if base.is_dir() {
                let mut matches = fs::read_dir(&base)
                    .map_err(|_| AccountDriverError::InvalidCodexProfile)?
                    .filter_map(Result::ok)
                    .map(|entry| entry.path())
                    .filter(|path| match pattern {
                        "*/.codex-profile" => path.join(".codex-profile").is_dir(),
                        "*/.codex" => path.join(".codex").is_dir(),
                        "*" => true,
                        _ => false,
                    })
                    .collect::<Vec<_>>();
                matches.sort();
                candidates.extend(matches.into_iter().take(1000).map(|path| match pattern {
                    "*/.codex-profile" => path.join(".codex-profile"),
                    "*/.codex" => path.join(".codex"),
                    _ => path,
                }));
            }
        }
        let mut registrations = Vec::new();
        for candidate in candidates {
            if candidate.join("auth.json").is_file()
                && let Ok(path) = CanonicalPath::existing(&candidate)
            {
                let metadata = codex_auth_metadata(&path);
                if metadata.status == AuthStatus::Ready {
                    registrations.push((path, metadata));
                }
            }
        }
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        let mut order = state.order.clone();
        let mut changed = false;
        for (path, metadata) in registrations {
            changed |= register_metadata(&mut updated, &mut order, &path, &metadata, false)?;
        }
        if changed {
            self.persist_registry(&mut state, updated, &order)?;
        }
        state.public_snapshot()
    }

    pub fn claude_discovery_available() -> bool {
        let configured = env::var_os(CLAUDE_BIN_OVERRIDE).map(PathBuf::from);
        let local = env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join(".local/bin/claude");
        let Some(configured) = configured else {
            return find_executable("claude", env::var_os("PATH").as_deref()).is_some()
                || is_executable(&local);
        };
        find_executable(
            configured.to_string_lossy().as_ref(),
            env::var_os("PATH").as_deref(),
        )
        .is_some()
    }

    pub fn terminal_login_receipt(&self, login_id: &str) -> Result<Value, AccountDriverError> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let receipt = state
            .value
            .get("logins")
            .and_then(|logins| logins.get(login_id))
            .cloned()
            .ok_or(AccountDriverError::UnknownLogin)?;
        if matches!(
            receipt.get("status").and_then(Value::as_str),
            Some("ready" | "duplicate" | "cancelled" | "error")
        ) {
            return Ok(receipt);
        }
        let mut updated = state.value.clone();
        let account = receipt
            .get("accountKey")
            .and_then(Value::as_str)
            .ok_or(AccountDriverError::InvalidRegistry)?
            .to_owned();
        let status = self.refresh_value(&mut updated, &account, true)?;
        if status == AccountStatus::Ready {
            reconcile_local_login(&mut updated, &state.order, login_id, &account)?;
        }
        if updated != state.value {
            self.persist(&mut state, updated)?;
        }
        let receipt = state
            .value
            .get("logins")
            .and_then(|logins| logins.get(login_id))
            .cloned()
            .ok_or(AccountDriverError::UnknownLogin)?;
        match receipt.get("status").and_then(Value::as_str) {
            Some("ready" | "duplicate" | "cancelled" | "error") => Ok(receipt),
            _ if receipt.get("loginId").is_none_or(Value::is_null) => {
                Err(AccountDriverError::LoginResponseUnknown)
            }
            _ => Err(AccountDriverError::LoginCancellationUnavailable),
        }
    }

    fn refresh_value(
        &self,
        value: &mut Value,
        account: &str,
        include_inactive: bool,
    ) -> Result<AccountStatus, AccountDriverError> {
        let profile = {
            let row = registry_account(value, account)?;
            if !include_inactive
                && (row.get("deleted") == Some(&Value::Bool(true))
                    || row.get("disconnected") == Some(&Value::Bool(true)))
            {
                return Ok(AccountStatus::Unavailable);
            }
            if row.get("duplicateOf").is_some_and(truthy_value) {
                return Ok(status_from_value(row.get("status")));
            }
            auth_profile(row)?
        };
        // Python reads the status again on every refresh, so a stored error
        // never outlives a valid login.
        let metadata =
            crate::diagnostics::measure_runtime_stage("account_driver.auth_metadata", || {
                self.reader.read(&profile)
            });
        let row = registry_account_mut(value, account)?
            .as_object_mut()
            .ok_or(AccountDriverError::InvalidRegistry)?;
        apply_metadata(row, &metadata);
        Ok(status_from_value(row.get("status")))
    }

    fn refresh_selectable(
        &self,
        value: &mut Value,
        order: &RegistryOrder,
    ) -> Result<(), AccountDriverError> {
        for key in &order.accounts {
            let row = registry_account(value, key)?;
            if row.get("deleted") != Some(&Value::Bool(true))
                && row.get("disconnected") != Some(&Value::Bool(true))
            {
                self.refresh_value(value, key, false)?;
            }
        }
        Ok(())
    }

    fn persist(&self, state: &mut RegistryState, updated: Value) -> Result<(), AccountDriverError> {
        let order = state.order.clone();
        self.persist_registry(state, updated, &order)
    }

    /// [`persist_registry`], then the credential watch follows the new profiles.
    fn persist_registry(
        &self,
        state: &mut RegistryState,
        updated: Value,
        order: &RegistryOrder,
    ) -> Result<(), AccountDriverError> {
        let result = persist_registry(&self.path, state, updated, order);
        self.reader.watch(&live_profiles(&state.value));
        result
    }
}

fn eligible_default(value: &Value, account: &str) -> bool {
    let Some(row) = registry_account(value, account)
        .ok()
        .and_then(Value::as_object)
    else {
        return false;
    };
    (row.get("status").and_then(Value::as_str) == Some("ready")
        || crate::account_policy::can_attempt_native_proof(Some(row)))
        && row.get("disconnected") != Some(&Value::Bool(true))
        && row.get("deleted") != Some(&Value::Bool(true))
}

fn apply_registration_metadata(row: &mut serde_json::Map<String, Value>, metadata: &AuthMetadata) {
    apply_metadata(row, metadata);
    row.entry("accountId").or_insert_with(|| {
        metadata
            .account_id
            .clone()
            .map_or(Value::Null, Value::String)
    });
    row.entry("email")
        .or_insert_with(|| metadata.email.clone().map_or(Value::Null, Value::String));
    row.entry("plan")
        .or_insert_with(|| metadata.plan.clone().map_or(Value::Null, Value::String));
}

fn register_metadata(
    value: &mut Value,
    order: &mut RegistryOrder,
    path: &Path,
    metadata: &AuthMetadata,
    restore_deleted: bool,
) -> Result<bool, AccountDriverError> {
    let home = path
        .to_str()
        .ok_or(AccountDriverError::InvalidCodexProfile)?
        .to_owned();
    let existing = order.accounts.iter().find_map(|key| {
        let row = value.get("accounts")?.get(key)?;
        (row.get("home").and_then(Value::as_str) == Some(&home)
            || metadata
                .account_id
                .as_deref()
                .is_some_and(|id| row.get("accountId").and_then(Value::as_str) == Some(id)))
        .then(|| key.clone())
    });
    if let Some(key) = existing {
        let row = registry_account_mut(value, &key)?
            .as_object_mut()
            .ok_or(AccountDriverError::InvalidRegistry)?;
        if restore_deleted && row.get("deleted") == Some(&Value::Bool(true)) {
            row.remove("deleted");
            return Ok(true);
        }
        return Ok(false);
    }
    let digest = hex_sha256(home.as_bytes());
    let account = format!("profile-{}", &digest[..20]);
    let label = path
        .file_name()
        .and_then(OsStr::to_str)
        .filter(|name| *name != ".codex" && *name != ".codex-profile")
        .or_else(|| {
            path.parent()
                .and_then(Path::file_name)
                .and_then(OsStr::to_str)
        })
        .unwrap_or("Codex profile");
    let mut row = serde_json::Map::new();
    row.insert("id".into(), Value::String(account.clone()));
    row.insert("home".into(), Value::String(home));
    row.insert("label".into(), Value::String(label.to_owned()));
    row.insert("source".into(), Value::String("Local profile".into()));
    apply_registration_metadata(&mut row, metadata);
    value
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .ok_or(AccountDriverError::InvalidRegistry)?
        .insert(account.clone(), Value::Object(row));
    order.accounts.push(account);
    Ok(true)
}

fn reconcile_local_login(
    value: &mut Value,
    order: &RegistryOrder,
    login_id: &str,
    account: &str,
) -> Result<(), AccountDriverError> {
    let receipt = value
        .get("logins")
        .and_then(|logins| logins.get(login_id))
        .cloned()
        .ok_or(AccountDriverError::UnknownLogin)?;
    if receipt.get("reauthAccountKey").is_some() {
        if receipt.get("nativeCompleted") != Some(&Value::Bool(true)) {
            return Ok(());
        }
        let row = registry_account(value, account)?;
        if row.get("status").and_then(Value::as_str) != Some("ready")
            || row.get("accountId") != receipt.get("expectedAccountId")
        {
            let mutable = value
                .get_mut("logins")
                .and_then(|logins| logins.get_mut(login_id))
                .and_then(Value::as_object_mut)
                .ok_or(AccountDriverError::InvalidRegistry)?;
            mutable.insert("status".into(), Value::String("error".into()));
            mutable.insert(
                "error".into(),
                Value::String("Sign in with this profile's original account.".into()),
            );
            return Ok(());
        }
        let mutable = value
            .get_mut("logins")
            .and_then(|logins| logins.get_mut(login_id))
            .and_then(Value::as_object_mut)
            .ok_or(AccountDriverError::InvalidRegistry)?;
        mutable.insert("status".into(), Value::String("ready".into()));
        mutable.insert(
            "resolvedAccountKey".into(),
            Value::String(account.to_owned()),
        );
        mutable.remove("error");
        mutable.remove("userCode");
        mutable.remove("verificationUrl");
        return Ok(());
    }

    let row = registry_account(value, account)?;
    let email = row.get("email").and_then(Value::as_str).map(str::to_owned);
    if let (Some(expected), Some(actual)) = (
        receipt.get("emailHint").and_then(Value::as_str),
        email.as_deref(),
    ) && !expected.eq_ignore_ascii_case(actual)
    {
        let mutable = value
            .get_mut("logins")
            .and_then(|logins| logins.get_mut(login_id))
            .and_then(Value::as_object_mut)
            .ok_or(AccountDriverError::InvalidRegistry)?;
        mutable.insert("status".into(), Value::String("error".into()));
        mutable.insert("email".into(), Value::String(actual.to_owned()));
        mutable.insert(
            "error".into(),
            Value::String("A different account signed in.".into()),
        );
        return Ok(());
    }
    let account_id = registry_account(value, account)?
        .get("accountId")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let duplicate = account_id
        .as_deref()
        .and_then(|account_id| {
            order.accounts.iter().find(|other| {
                other.as_str() != account
                    && value
                        .get("accounts")
                        .and_then(|accounts| accounts.get(other.as_str()))
                        .is_some_and(|candidate| {
                            candidate.get("status").and_then(Value::as_str) == Some("ready")
                                && candidate.get("duplicateOf").is_none()
                                && candidate.get("accountId").and_then(Value::as_str)
                                    == Some(account_id)
                        })
            })
        })
        .cloned();
    if let Some(primary) = duplicate.as_deref() {
        if let Some(row) = value
            .get_mut("accounts")
            .and_then(|accounts| accounts.get_mut(primary))
            .and_then(Value::as_object_mut)
        {
            row.remove("deleted");
        }
        let row = registry_account_mut(value, account)?
            .as_object_mut()
            .ok_or(AccountDriverError::InvalidRegistry)?;
        row.insert("status".into(), Value::String("duplicate".into()));
        row.insert("duplicateOf".into(), Value::String(primary.to_owned()));
    } else {
        let row = registry_account_mut(value, account)?
            .as_object_mut()
            .ok_or(AccountDriverError::InvalidRegistry)?;
        row.insert(
            "label".into(),
            Value::String(
                receipt
                    .get("requestedLabel")
                    .and_then(Value::as_str)
                    .or(email.as_deref())
                    .unwrap_or("Codex account")
                    .to_owned(),
            ),
        );
    }
    let mutable = value
        .get_mut("logins")
        .and_then(|logins| logins.get_mut(login_id))
        .and_then(Value::as_object_mut)
        .ok_or(AccountDriverError::InvalidRegistry)?;
    mutable.insert(
        "status".into(),
        Value::String(
            if duplicate.is_some() {
                "duplicate"
            } else {
                "ready"
            }
            .into(),
        ),
    );
    mutable.insert(
        "resolvedAccountKey".into(),
        Value::String(duplicate.as_deref().unwrap_or(account).to_owned()),
    );
    mutable.remove("error");
    mutable.remove("userCode");
    mutable.remove("verificationUrl");
    Ok(())
}

fn project_snapshot(
    value: &Value,
    order: &RegistryOrder,
) -> Result<AccountsSnapshot, AccountDriverError> {
    crate::account_policy::project_registry_value(value, order)
        .map_err(|_| AccountDriverError::InvalidRegistry)
}

fn persist_registry(
    path: &Path,
    state: &mut RegistryState,
    mut updated: Value,
    order: &RegistryOrder,
) -> Result<(), AccountDriverError> {
    let accounts_changed =
        project_snapshot(&state.value, &state.order)? != project_snapshot(&updated, order)?;
    if accounts_changed {
        updated["pendingAccountsChange"] = Value::Bool(true);
    }
    let encoded =
        encode_registry(&updated, order).map_err(|_| AccountDriverError::InvalidRegistry)?;
    let (checked, order) =
        decode_registry(&encoded).map_err(|_| AccountDriverError::InvalidRegistry)?;
    match atomic_replace(path, &encoded) {
        Ok(()) => {
            state.value = checked;
            state.order = order;
            if accounts_changed {
                state.publish_profile_change();
            }
            Ok(())
        }
        Err(ReplaceError::NotReplaced) => Err(AccountDriverError::WriteRegistry),
        Err(ReplaceError::ReplacedNotDurable) => {
            state.value = checked;
            state.order = order;
            if accounts_changed {
                state.publish_profile_change();
            }
            Err(AccountDriverError::DurabilityUnconfirmed)
        }
    }
}

fn auth_profile(row: &Value) -> Result<AuthProfile, AccountDriverError> {
    Ok(AuthProfile {
        provider: studio_runtime_providers::ProviderKind::of_record(row)
            .map_err(|_| AccountDriverError::InvalidRegistry)?,
        home: PathBuf::from(row.get("home").and_then(Value::as_str).unwrap_or_default()),
        claude_options: row.get("claudeOptions").cloned(),
    })
}

/// The profiles whose credentials the registry still uses.
fn live_profiles(value: &Value) -> Vec<AuthProfile> {
    value
        .get("accounts")
        .and_then(Value::as_object)
        .into_iter()
        .flatten()
        .filter(|(_, row)| row.get("deleted") != Some(&Value::Bool(true)))
        .filter_map(|(_, row)| auth_profile(row).ok())
        .collect()
}

fn registry_account<'a>(value: &'a Value, account: &str) -> Result<&'a Value, AccountDriverError> {
    value
        .get("accounts")
        .and_then(Value::as_object)
        .and_then(|accounts| accounts.get(account))
        .ok_or(AccountDriverError::UnknownAccount)
}

fn registry_account_mut<'a>(
    value: &'a mut Value,
    account: &str,
) -> Result<&'a mut Value, AccountDriverError> {
    value
        .get_mut("accounts")
        .and_then(Value::as_object_mut)
        .and_then(|accounts| accounts.get_mut(account))
        .ok_or(AccountDriverError::UnknownAccount)
}

fn ready_replacement<'a>(
    value: &'a Value,
    order: &'a RegistryOrder,
    excluded: &str,
) -> Option<&'a str> {
    order.accounts.iter().find_map(|key| {
        let row = value.get("accounts")?.get(key)?;
        (key != excluded
            && row.get("status").and_then(Value::as_str) == Some("ready")
            && row.get("deleted") != Some(&Value::Bool(true))
            && row.get("disconnected") != Some(&Value::Bool(true))
            && !row.get("duplicateOf").is_some_and(truthy_value))
        .then_some(key.as_str())
    })
}

fn expanded_profile_path(home: &str) -> PathBuf {
    let root = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home));
    expand_user_path(home, &root)
}

fn expand_user_path(configured: &str, home: &Path) -> PathBuf {
    if configured == "~" {
        return home.to_path_buf();
    }
    if let Some(suffix) = configured.strip_prefix("~/") {
        return home.join(suffix);
    }
    PathBuf::from(configured)
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum CodexHomeError {
    #[error("Codex agent data must be outside .claude: {}", .0.display())]
    InsideClaude(PathBuf),
    #[error("Cannot resolve a relative CODEX_HOME without a working directory")]
    WorkingDirectory,
}

/// The base Codex home, as Python `codex_state.codex_home()` computes it: a
/// non-empty `CODEX_HOME`, else `<home>/.codex`, with `~` expanded, made
/// absolute and resolved like `Path.resolve()`. A path inside `.claude` is
/// refused.
pub fn resolve_codex_home(
    configured: Option<&OsStr>,
    home: &Path,
) -> Result<PathBuf, CodexHomeError> {
    let selected = match configured.filter(|value| !value.is_empty()) {
        Some(value) => expand_user_path(&value.to_string_lossy(), home),
        None => home.join(".codex"),
    };
    let resolved = CanonicalPath::profile_nonstrict(&selected)
        .map_err(|_| CodexHomeError::WorkingDirectory)?
        .into_path_buf();
    if resolved
        .components()
        .any(|component| component.as_os_str() == ".claude")
    {
        return Err(CodexHomeError::InsideClaude(resolved));
    }
    Ok(resolved)
}

/// Create Python `AccountStore`'s first-start registry when `path` has no
/// entry. Returns whether this call created the file. An existing entry is
/// never replaced, also when another process creates it during this call.
///
/// Python writes `json.dump` output (ASCII, `", "` and `": "` separators, no
/// trailing newline) in insertion order to a `mkstemp` file (mode 0600) in a
/// directory made with mode 0700. These bytes are the same.
pub fn create_default_registry(path: &Path, codex_home: &Path) -> Result<bool, AccountDriverError> {
    match fs::symlink_metadata(path) {
        Ok(_) => return Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(_) => return Err(AccountDriverError::ReadRegistry),
    }
    let bytes = default_registry_bytes(codex_home)?;
    decode_registry(&bytes).map_err(|_| AccountDriverError::InvalidRegistry)?;
    let parent = path.parent().ok_or(AccountDriverError::WriteRegistry)?;
    let mut directory = fs::DirBuilder::new();
    directory.recursive(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        directory.mode(0o700);
    }
    directory
        .create(parent)
        .map_err(|_| AccountDriverError::WriteRegistry)?;
    let mut temp = tempfile::Builder::new()
        .prefix("registry-")
        .tempfile_in(parent)
        .map_err(|_| AccountDriverError::WriteRegistry)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        temp.as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))
            .map_err(|_| AccountDriverError::WriteRegistry)?;
    }
    temp.write_all(&bytes)
        .and_then(|()| temp.as_file().sync_all())
        .map_err(|_| AccountDriverError::WriteRegistry)?;
    match temp.persist_noclobber(path) {
        Ok(_) => {}
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            return Ok(false);
        }
        Err(_) => return Err(AccountDriverError::WriteRegistry),
    }
    File::open(parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|_| AccountDriverError::DurabilityUnconfirmed)?;
    Ok(true)
}

/// Python `AccountStore.__init__` default document with `auth_metadata(home)`.
fn default_registry_bytes(codex_home: &Path) -> Result<Vec<u8>, AccountDriverError> {
    let home = codex_home
        .to_str()
        .ok_or(AccountDriverError::InvalidRegistry)?;
    let text = |value: &str| studio_runtime_store::python_json(&Value::String(value.to_owned()));
    let optional = |value: Option<&String>| value.map_or_else(|| "null".to_owned(), |v| text(v));
    let mut row = vec![
        ("id", text("default")),
        ("home", text(home)),
        ("label", text("Codex")),
        ("source", text("Codex CLI")),
    ];
    match read_codex_auth(codex_home) {
        CodexAuth::Missing => row.extend([
            ("accountId", "null".to_owned()),
            ("email", "null".to_owned()),
            ("plan", "null".to_owned()),
            ("status", text("signedOut")),
        ]),
        CodexAuth::Unreadable => row.extend([
            ("accountId", "null".to_owned()),
            ("email", "null".to_owned()),
            ("plan", "null".to_owned()),
            ("status", text("error")),
            ("error", text(CODEX_AUTH_ERROR)),
        ]),
        CodexAuth::Parsed(metadata) => row.extend([
            ("accountId", optional(metadata.account_id.as_ref())),
            ("email", optional(metadata.email.as_ref())),
            ("plan", optional(metadata.plan.as_ref())),
            (
                "status",
                text(if metadata.status == AuthStatus::Ready {
                    "ready"
                } else {
                    "signedOut"
                }),
            ),
            (
                "_credentialIdentity",
                optional(metadata.credential_identity.as_ref()),
            ),
        ]),
    }
    let document = python_object(&[
        ("version", "1".to_owned()),
        ("defaultAccountKey", text("default")),
        (
            "accounts",
            python_object(&[("default", python_object(&row))]),
        ),
        ("logins", "{}".to_owned()),
        ("deleteReceipts", "{}".to_owned()),
    ]);
    Ok(document.into_bytes())
}

/// `json.dumps` of a dict whose values are already encoded, in entry order.
fn python_object(entries: &[(&str, String)]) -> String {
    let body = entries
        .iter()
        .map(|(key, value)| {
            format!(
                "{}: {value}",
                studio_runtime_store::python_json(&Value::String((*key).to_owned()))
            )
        })
        .collect::<Vec<_>>()
        .join(", ");
    format!("{{{body}}}")
}

fn apply_metadata(row: &mut serde_json::Map<String, Value>, metadata: &AuthMetadata) {
    let expected_account = row
        .get("accountId")
        .and_then(nonempty_str)
        .map(str::to_owned);
    let expected_credential = row
        .get("_credentialIdentity")
        .and_then(nonempty_str)
        .map(str::to_owned);
    let source = row
        .get("source")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned();
    let old_status = status_from_value(row.get("status"));
    row.remove("_authErrorKind");
    if let Some(kind) = metadata.error_kind {
        row.insert("_authErrorKind".into(), Value::String(kind.into()));
    }
    if expected_account
        .as_deref()
        .zip(
            metadata
                .account_id
                .as_deref()
                .filter(|value| !value.is_empty()),
        )
        .is_some_and(|(a, b)| a != b)
        || expected_credential
            .as_deref()
            .zip(
                metadata
                    .credential_identity
                    .as_deref()
                    .filter(|v| !v.is_empty()),
            )
            .is_some_and(|(a, b)| {
                if row.get("provider").and_then(Value::as_str) == Some("claude") {
                    !claude_identity::accepts(a, b)
                } else {
                    a != b
                }
            })
    {
        row.insert("status".into(), Value::String("changed".into()));
        row.insert("error".into(), Value::String(CHANGED_ERROR.into()));
        return;
    }
    if metadata.status == AuthStatus::Ready
        && ((expected_account.is_some()
            && metadata
                .account_id
                .as_deref()
                .filter(|v| !v.is_empty())
                .is_none())
            || (expected_credential.is_some()
                && metadata
                    .credential_identity
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .is_none())
            || (metadata
                .account_id
                .as_deref()
                .filter(|v| !v.is_empty())
                .is_none()
                && metadata
                    .credential_identity
                    .as_deref()
                    .filter(|v| !v.is_empty())
                    .is_none()))
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
        row.insert(
            "error".into(),
            Value::String(metadata.error.unwrap_or(AUTH_ERROR).into()),
        );
        return;
    }
    let fields = [
        ("accountId", metadata.account_id.as_ref()),
        ("_credentialIdentity", metadata.credential_identity.as_ref()),
        ("email", metadata.email.as_ref()),
        ("plan", metadata.plan.as_ref()),
    ];
    for (key, value) in fields {
        if let Some(value) = value.filter(|value| !value.is_empty()) {
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

fn nonempty_str(value: &Value) -> Option<&str> {
    value.as_str().filter(|value| !value.is_empty())
}

/// The account order file beside `registry` (see [`crate::account_preference`]).
fn account_order_path(registry: &Path) -> PathBuf {
    registry.with_file_name(crate::account_preference::ACCOUNT_ORDER_FILE)
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

enum ReplaceError {
    NotReplaced,
    ReplacedNotDurable,
}

fn atomic_replace(path: &Path, bytes: &[u8]) -> Result<(), ReplaceError> {
    atomic_replace_with(
        path,
        bytes,
        |temp, target| fs::rename(temp, target),
        |parent| File::open(parent).and_then(|directory| directory.sync_all()),
    )
}

fn atomic_replace_with(
    path: &Path,
    bytes: &[u8],
    replace: impl FnOnce(&Path, &Path) -> std::io::Result<()>,
    sync_parent: impl FnOnce(&Path) -> std::io::Result<()>,
) -> Result<(), ReplaceError> {
    let parent = path.parent().ok_or(ReplaceError::NotReplaced)?;
    // Registry rows may contain login codes and verification URLs. Unix
    // permissions must be private even when the previous file or directory was
    // created with broader permissions. Other platforms rely on their default
    // temporary-file ACLs; this code does not explicitly rewrite those ACLs.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(parent, fs::Permissions::from_mode(0o700))
            .map_err(|_| ReplaceError::NotReplaced)?;
    }
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| ReplaceError::NotReplaced)?;
    (|| {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temp.as_file()
                .set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|_| ReplaceError::NotReplaced)?;
        }
        temp.write_all(bytes)
            .map_err(|_| ReplaceError::NotReplaced)?;
        temp.as_file()
            .sync_all()
            .map_err(|_| ReplaceError::NotReplaced)?;
        replace(temp.path(), path).map_err(|_| ReplaceError::NotReplaced)?;
        sync_parent(parent).map_err(|_| ReplaceError::ReplacedNotDurable)?;
        Ok(())
    })()
}

fn hex_sha256(value: &[u8]) -> String {
    studio_runtime_store::sha256_hex(value)
}

fn base64url_decode(value: &str) -> Option<Vec<u8>> {
    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value)
        .ok()
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    const PLACEHOLDER_KEY: &str = "placeholder-secret-do-not-persist";

    #[cfg(unix)]
    #[test]
    fn claude_profile_binary_path_precedes_environment_override() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("isolated Claude binary fixture");
        let profile_binary = directory.path().join("profile-claude");
        let override_binary = directory.path().join("override-claude");
        for binary in [&profile_binary, &override_binary] {
            fs::write(binary, "#!/bin/sh\nexit 0\n").expect("write fake executable");
            fs::set_permissions(binary, fs::Permissions::from_mode(0o700))
                .expect("make fake executable runnable");
        }
        let environment = BTreeMap::from([(
            OsStr::new(CLAUDE_BIN_OVERRIDE).to_owned(),
            override_binary.as_os_str().to_owned(),
        )]);
        let profile = serde_json::json!({
            "claudeOptions": {"binaryPath": profile_binary.to_string_lossy()}
        });

        assert_eq!(
            claude::resolve_claude_binary_with_environment(&profile, &environment),
            Some(profile_binary.to_string_lossy().into_owned()),
        );
    }

    #[test]
    fn codex_metadata_requires_strict_unpadded_base64url_jwt_payloads() {
        let metadata = |payload: &str| {
            codex_metadata_from_value(&serde_json::json!({
                "tokens": { "id_token": format!("header.{payload}.signature") }
            }))
            .expect("well-formed auth object")
        };
        let valid = base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(br#"{"https://api.openai.com/profile":{"email":"person@example.com"}}"#);
        assert_eq!(
            metadata(&valid).email.as_deref(),
            Some("person@example.com")
        );
        for rejected in [
            "eyJlbWFpbCI6IngifQ==",
            "eyJlbWFpbCI6IngifR",
            "A",
            "AAAAA",
            "e30=garbage",
        ] {
            assert_eq!(metadata(rejected).email, None, "payload {rejected}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_status_probe_preserves_inherited_priority() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("isolated status fixture");
        let command = directory.path().join("status-probe");
        fs::write(
            &command,
            r#"#!/bin/sh
# Report the inherited nice value and I/O priority after a finite prefix.
printf '%8192s' ''
ps -o ni= -p $$
ionice -p $$
"#,
        )
        .expect("probe script");
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700)).expect("executable");
        let (output, success) = run_claude_status(command.to_str().expect("fixture path"), None)
            .expect("status caller");
        assert!(success);
        let text = String::from_utf8(output).expect("status output");
        let lines = text.lines().map(str::trim).collect::<Vec<_>>();
        assert_eq!(
            lines[0],
            rustix::process::getpriority_process(None)
                .expect("parent nice")
                .to_string()
        );
        let inherited_io = Command::new("ionice")
            .args(["-p", &std::process::id().to_string()])
            .output()
            .expect("parent I/O priority");
        assert!(inherited_io.status.success());
        assert_eq!(
            lines[1],
            String::from_utf8_lossy(&inherited_io.stdout).trim()
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_status_timeout_kills_group_after_parent_exits() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("create Claude command fixture");
        let command = directory.path().join("claude");
        let pid_file = directory.path().join("descendant-pid");
        let script = format!(
            "#!/bin/sh\n(exec sleep 30) &\necho $! > '{}'\nexit 0\n",
            pid_file.display(),
        );
        fs::write(&command, script).expect("write Claude command fixture");
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700))
            .expect("make Claude command executable");

        assert!(matches!(
            run_claude_status_with_timeout(
                command.to_str().expect("fixture path is UTF-8"),
                None,
                Duration::from_millis(500),
            ),
            Err("timeout")
        ));
        assert!(
            pid_file.is_file(),
            "descendant started and recorded its pid"
        );
        let descendant_pid: i32 = fs::read_to_string(&pid_file)
            .expect("read descendant pid")
            .trim()
            .parse()
            .expect("valid descendant pid");
        let pid = rustix::process::Pid::from_raw(descendant_pid).expect("valid pid");
        let exited = match rustix::process::pidfd_open(pid, rustix::process::PidfdFlags::empty()) {
            Ok(pidfd) => {
                let timeout = rustix::event::Timespec::try_from(Duration::from_secs(2))
                    .expect("two second pidfd wait fits timespec");
                let mut poll_fd = rustix::event::PollFd::new(&pidfd, rustix::event::PollFlags::IN);
                rustix::event::poll(std::slice::from_mut(&mut poll_fd), Some(&timeout))
                    .expect("wait for descendant pidfd")
                    == 1
                    && poll_fd.revents().contains(rustix::event::PollFlags::IN)
            }
            Err(rustix::io::Errno::SRCH) => true,
            Err(error) => panic!("open descendant pidfd: {error}"),
        };
        assert!(exited, "group descendant exited after timeout cleanup");
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn claude_status_timeout_is_bounded_when_child_leaves_group() {
        use std::os::unix::fs::PermissionsExt;

        let directory = tempfile::tempdir().expect("create Claude command fixture");
        let command = directory.path().join("claude");
        let pid_file = directory.path().join("escaped-pid");
        let script = format!(
            r#"#!/bin/sh
(setsid sh -c 'echo $$ > "{}"; exec sleep 30') &
exit 0
"#,
            pid_file.display(),
        );
        fs::write(&command, script).expect("write Claude command fixture");
        fs::set_permissions(&command, fs::Permissions::from_mode(0o700))
            .expect("make Claude command executable");
        let started = Instant::now();
        assert!(matches!(
            run_claude_status_with_timeout(
                command.to_str().expect("fixture path is UTF-8"),
                None,
                Duration::from_secs(2),
            ),
            Err("timeout")
        ));
        assert!(pid_file.is_file(), "escaped child started");
        let escaped_pid: i32 = fs::read_to_string(&pid_file)
            .expect("read escaped pid")
            .trim()
            .parse()
            .expect("valid escaped pid");
        let pid = rustix::process::Pid::from_raw(escaped_pid).expect("valid pid");
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "timeout cleanup is bounded"
        );
        assert!(
            rustix::process::test_kill_process(pid).is_ok(),
            "escaped group is not signalled"
        );
        let _cleanup_result = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_running(pid) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(!process_is_running(pid), "fixture child cleaned up");
    }

    #[cfg(target_os = "linux")]
    fn process_is_running(pid: rustix::process::Pid) -> bool {
        fs::read_to_string(format!("/proc/{}/stat", pid.as_raw_pid()))
            .ok()
            .and_then(|status| status.rsplit_once(')').map(|(_, status)| status.to_owned()))
            .and_then(|status| status.trim_start().chars().next())
            .is_some_and(|state| state != 'Z')
    }

    struct FakeReader(AuthMetadata);

    impl ProviderAuthReader for FakeReader {
        fn read(&self, _profile: &AuthProfile) -> AuthMetadata {
            self.0.clone()
        }
    }

    /// Hang guard for deterministic authentication-read synchronization.
    const AUTH_READ_GUARD: Duration = Duration::from_secs(10);

    /// Announces each read start, then returns the next metadata the test sends.
    struct GatedReader {
        started: Mutex<std::sync::mpsc::Sender<()>>,
        released: Mutex<std::sync::mpsc::Receiver<AuthMetadata>>,
    }

    impl ProviderAuthReader for GatedReader {
        fn read(&self, _profile: &AuthProfile) -> AuthMetadata {
            self.started
                .lock()
                .expect("read start sender")
                .send(())
                .expect("report read start");
            self.released
                .lock()
                .expect("read release receiver")
                .recv_timeout(AUTH_READ_GUARD)
                .expect("test releases the auth read")
        }
    }

    fn gated_reader() -> (
        GatedReader,
        std::sync::mpsc::Receiver<()>,
        std::sync::mpsc::Sender<AuthMetadata>,
    ) {
        let (started_tx, started_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        (
            GatedReader {
                started: Mutex::new(started_tx),
                released: Mutex::new(release_rx),
            },
            started_rx,
            release_tx,
        )
    }

    struct TestDir {
        path: PathBuf,
        _directory: tempfile::TempDir,
    }

    impl TestDir {
        fn new() -> Self {
            let root = env::var_os("CS_RUST_ACCOUNT_DRIVER_TEST_ROOT")
                .map(PathBuf::from)
                .unwrap_or_else(env::temp_dir);
            fs::create_dir_all(&root).expect("create fixture parent directory");
            let directory = tempfile::Builder::new()
                .prefix("account-driver-test-")
                .tempdir_in(root)
                .expect("create disposable fixture directory");
            Self {
                path: directory.path().to_path_buf(),
                _directory: directory,
            }
        }
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
            error_kind: None,
        }
    }

    #[test]
    fn refresh_reads_codex_placeholder_credentials_and_never_persists_token_values() {
        let fixture = TestDir::new();
        let home = fixture.path.join("codex-home");
        fs::create_dir_all(&home).expect("create fake Codex home");
        fs::write(
            home.join("auth.json"),
            format!(r#"{{"OPENAI_API_KEY":"{PLACEHOLDER_KEY}"}}"#),
        )
        .expect("write placeholder auth file");
        let path = fixture.path.join("state/accounts/registry.json");
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
    fn default_accepts_original_claude_identity_with_keychain_error() {
        let fixture = TestDir::new();
        let home = fixture.path.join("claude-home");
        fs::create_dir_all(&home).expect("create disposable Claude home");
        let mut value: Value = serde_json::from_slice(&registry(&home)).expect("fixture registry");
        value["accounts"]["default"]["provider"] = Value::String("claude".into());
        value["accounts"]["default"]["source"] = Value::String("Claude Code".into());
        value["accounts"]["default"]["status"] = Value::String("error".into());
        value["accounts"]["default"]["email"] = Value::String("fixture@example.invalid".into());
        value["accounts"]["default"]["accountId"] =
            Value::String("claude:fixture@example.invalid".into());
        value["accounts"]["default"]["_credentialIdentity"] =
            Value::String("claude:fixture@example.invalid".into());
        value["accounts"]["default"]["_authErrorKind"] = Value::String("keychain".into());
        let path = fixture.path.join("state/accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("serialize fixture"),
        )
        .expect("write registry");
        let reader = FakeReader(AuthMetadata {
            status: AuthStatus::Error,
            account_id: None,
            email: None,
            plan: None,
            credential_identity: None,
            error: Some(AUTH_ERROR),
            error_kind: Some("keychain"),
        });
        let driver = AccountDriver::open_with_reader(&path, reader).expect("open fixture");
        let snapshot = driver
            .set_default(Some("default"))
            .expect("eligible Claude default");
        assert_eq!(
            snapshot.default_account_key,
            Value::String("default".into())
        );
    }

    /// A reader that answers every read with `metadata`.
    struct FixedReader(AuthMetadata);

    impl ProviderAuthReader for FixedReader {
        fn read(&self, _profile: &AuthProfile) -> AuthMetadata {
            self.0.clone()
        }
    }

    fn claude_registry(fixture: &TestDir, status: &str, error_kind: Option<&str>) -> PathBuf {
        let home = fixture.path.join("claude-home");
        fs::create_dir_all(&home).expect("create disposable Claude home");
        let mut value: Value = serde_json::from_slice(&registry(&home)).expect("fixture registry");
        let row = &mut value["accounts"]["default"];
        row["provider"] = Value::String("claude".into());
        row["source"] = Value::String("Claude Code".into());
        row["status"] = Value::String(status.into());
        row["email"] = Value::String("fixture@example.invalid".into());
        row["accountId"] = Value::String("claude:fixture@example.invalid".into());
        row["_credentialIdentity"] = Value::String("claude:fixture@example.invalid".into());
        if let Some(kind) = error_kind {
            row["error"] = Value::String("Cannot read Claude Code sign-in status".into());
            row["_authErrorKind"] = Value::String(kind.into());
        }
        let path = fixture.path.join("state/accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        fs::write(
            &path,
            serde_json::to_vec(&value).expect("serialize fixture"),
        )
        .expect("write registry");
        path
    }

    fn ready_claude() -> AuthMetadata {
        AuthMetadata {
            status: AuthStatus::Ready,
            account_id: Some("claude:fixture@example.invalid".into()),
            email: Some("fixture@example.invalid".into()),
            plan: Some("max".into()),
            credential_identity: Some("claude:fixture@example.invalid".into()),
            error: None,
            error_kind: None,
        }
    }

    #[test]
    fn a_stale_claude_error_with_a_valid_login_becomes_ready() {
        // Live cutover 4: Python's passive reader stored one "parser" error.
        // Python recovered on its next read; Rust kept the error and refused
        // every turn with "Sign in with claude auth login first".
        let fixture = TestDir::new();
        let path = claude_registry(&fixture, "error", Some("parser"));
        let driver = AccountDriver::open_with_reader(&path, FixedReader(ready_claude()))
            .expect("open fixture");
        assert_eq!(driver.get("default").expect("status"), AccountStatus::Ready);
        let saved: Value =
            serde_json::from_slice(&fs::read(&path).expect("read registry")).expect("registry");
        let row = &saved["accounts"]["default"];
        assert_eq!(row["status"], "ready");
        assert!(row.get("error").is_none_or(Value::is_null));
        assert!(row.get("_authErrorKind").is_none());
    }

    /// A stored ready status does not outlive a sign-out on any host.
    #[test]
    fn a_signed_out_read_blocks_a_ready_claude_account() {
        let fixture = TestDir::new();
        let path = claude_registry(&fixture, "ready", None);
        let driver =
            AccountDriver::open_with_reader(&path, FixedReader(AuthMetadata::signed_out()))
                .expect("open fixture");
        assert_ne!(driver.get("default").expect("status"), AccountStatus::Ready);
    }

    #[test]
    fn codex_auth_metadata_uses_access_token_when_id_token_is_empty() {
        let fixture = TestDir::new();
        let home = fixture.path.join("codex-home");
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
    fn refresh_reads_registry_and_auth_written_by_a_seeded_store() {
        let fixture = TestDir::new();
        let home = fixture.path.join("codex-home");
        fs::create_dir_all(&home).expect("create fixture home");
        fs::write(
            home.join("auth.json"),
            serde_json::to_vec(&serde_json::json!({"tokens": {
                "account_id": "python-account", "access_token": "placeholder-access-token",
            }}))
            .expect("auth JSON"),
        )
        .expect("write auth");
        let path = fixture.path.join("state/accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent")).expect("create state");
        fs::write(&path, registry(&home)).expect("write registry");
        let driver = AccountDriver::open(path.clone()).expect("load seeded registry");
        assert_eq!(
            driver.get("default").expect("refresh Python account"),
            AccountStatus::Ready
        );
        let persisted = fs::read_to_string(path).expect("read Rust-refreshed registry");
        assert!(persisted.contains("python-account"));
        assert!(!persisted.contains("placeholder-access-token"));
    }

    #[test]
    fn account_group_and_default_order_persist_and_reload() {
        let fixture = TestDir::new();
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        fs::write(&path, registry(&fixture.path)).expect("write registry");
        let driver = AccountDriver::open_with_reader(
            &path,
            FakeReader(metadata(AuthStatus::Ready, Some("a"), Some("chatgpt:a"))),
        )
        .expect("open registry");
        let registry_before = fs::read(&path).expect("read registry");
        let snapshot = driver
            .set_group("default", Some("personal"))
            .expect("set group");
        assert_eq!(snapshot.accounts[0]["group"], "personal");
        let order = vec!["personal".to_owned(), "work".to_owned()];
        let snapshot = driver.set_default_groups(&order).expect("set order");
        assert_eq!(snapshot.default_account_groups, order);
        // A repeated request sets the same values.
        assert_eq!(driver.set_default_groups(&order).expect("repeat"), snapshot);
        let reopened = AccountDriver::open_with_reader(
            &path,
            FakeReader(metadata(AuthStatus::Ready, Some("a"), Some("chatgpt:a"))),
        )
        .expect("reopen persisted registry");
        let snapshot = reopened.snapshot().expect("snapshot");
        assert_eq!(snapshot.accounts[0]["group"], "personal");
        assert_eq!(snapshot.default_account_groups, order);
        // Rollback: the registry stays byte for byte what an older build reads,
        // and its strict schema (unchanged by this feature) still accepts it.
        let registry_after = fs::read(&path).expect("read registry");
        assert_eq!(registry_after, registry_before);
        assert!(crate::account_policy::decode_registry(&registry_after).is_ok());
        assert!(
            fs::read_to_string(path.with_file_name("account-order.json"))
                .expect("account order file")
                .contains("personal")
        );
        // Invalid names and repeats are refused without a write.
        for bad in [
            vec!["Personal".to_owned()],
            vec!["a".to_owned(), "a".to_owned()],
        ] {
            assert!(matches!(
                reopened.set_default_groups(&bad),
                Err(AccountDriverError::InvalidAccountOrder(_))
            ));
        }
        assert!(matches!(
            reopened.set_group("default", Some("no spaces")),
            Err(AccountDriverError::InvalidAccountOrder(_))
        ));
        let cleared = reopened.set_group("default", None).expect("clear group");
        assert!(cleared.accounts[0].get("group").is_none());
        let cleared = reopened.set_default_groups(&[]).expect("clear order");
        assert!(cleared.default_account_groups.is_empty());
        // An unreadable order file gives no order instead of failing account reads.
        fs::write(path.with_file_name("account-order.json"), b"{not json")
            .expect("corrupt order file");
        let reopened = AccountDriver::open_with_reader(
            &path,
            FakeReader(metadata(AuthStatus::Ready, Some("a"), Some("chatgpt:a"))),
        )
        .expect("a corrupt order file does not block the registry");
        assert!(
            reopened
                .snapshot()
                .expect("snapshot")
                .default_account_groups
                .is_empty()
        );
    }

    #[test]
    fn changed_identity_is_persisted_and_blocks_admission() {
        let fixture = TestDir::new();
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        let mut value: Value =
            serde_json::from_slice(&registry(&fixture.path)).expect("registry fixture");
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
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        let mut value: Value =
            serde_json::from_slice(&registry(&fixture.path)).expect("registry fixture");
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

    /// Records the profile set the driver asks to have watched.
    struct WatchRecorder(Mutex<Vec<usize>>);

    impl ProviderAuthReader for WatchRecorder {
        fn read(&self, _profile: &AuthProfile) -> AuthMetadata {
            metadata(AuthStatus::Ready, Some("x"), Some("chatgpt:x"))
        }

        fn watch(&self, profiles: &[AuthProfile]) {
            self.0.lock().expect("watch log").push(profiles.len());
        }
    }

    #[test]
    fn the_watched_profile_set_follows_the_registry() {
        let fixture = TestDir::new();
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        let mut value: Value =
            serde_json::from_slice(&registry(&fixture.path)).expect("registry fixture");
        value["accounts"]["other"] = serde_json::json!({
            "id": "other", "home": fixture.path.join("other"), "label": "Other",
            "source": "Codex CLI", "status": "signedOut"
        });
        fs::write(&path, serde_json::to_vec(&value).expect("encoded")).expect("write registry");
        let driver = AccountDriver::open_with_reader(&path, WatchRecorder(Mutex::default()))
            .expect("open registry");
        assert_eq!(*driver.reader.0.lock().expect("log"), [2]);
        driver
            .delete("other", "00000000-0000-4000-8000-000000000001")
            .expect("delete");
        assert_eq!(driver.reader.0.lock().expect("log").last(), Some(&1));
    }

    #[test]
    fn pending_auth_read_does_not_block_other_registry_reads() {
        let fixture = TestDir::new();
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        fs::write(&path, registry(&fixture.path)).expect("write registry");
        let (reader, started, release) = gated_reader();
        let driver = AccountDriver::open_with_reader(&path, reader).expect("open registry");
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        std::thread::scope(|scope| {
            let getter = scope.spawn(|| driver.get("default"));
            started
                .recv_timeout(AUTH_READ_GUARD)
                .expect("auth read started");
            scope.spawn(|| done_tx.send(driver.snapshot().is_ok()));
            let progressed = done_rx.recv_timeout(AUTH_READ_GUARD);
            release
                .send(metadata(
                    AuthStatus::Ready,
                    Some("original"),
                    Some("chatgpt:original"),
                ))
                .expect("release auth read");
            assert!(progressed.expect("snapshot finished during the auth read"));
            assert_eq!(getter.join().expect("get thread"), Ok(AccountStatus::Ready));
        });
    }

    #[test]
    fn disconnect_during_pending_auth_read_discards_the_stale_result() {
        let fixture = TestDir::new();
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        fs::write(&path, registry(&fixture.path)).expect("write registry");
        let (reader, started, release) = gated_reader();
        let driver = AccountDriver::open_with_reader(&path, reader).expect("open registry");
        std::thread::scope(|scope| {
            let getter = scope.spawn(|| driver.get("default"));
            started
                .recv_timeout(AUTH_READ_GUARD)
                .expect("auth read started");
            // Set the flag directly: disconnect() would re-read through this gated reader.
            driver.state.lock().expect("registry lock").value["accounts"]["default"]["disconnected"] =
                Value::Bool(true);
            release
                .send(metadata(
                    AuthStatus::Ready,
                    Some("stale"),
                    Some("chatgpt:stale"),
                ))
                .expect("release stale auth read");
            assert_eq!(
                getter.join().expect("get thread"),
                Ok(AccountStatus::Unavailable)
            );
        });
        let row = driver.internal_record("default").expect("account row");
        assert_eq!(row["disconnected"], Value::Bool(true));
        assert!(row.get("accountId").is_none_or(Value::is_null));
    }

    #[test]
    fn profile_change_during_pending_auth_read_rereads_the_current_profile() {
        let fixture = TestDir::new();
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        fs::write(&path, registry(&fixture.path)).expect("write registry");
        let (reader, started, release) = gated_reader();
        let driver = AccountDriver::open_with_reader(&path, reader).expect("open registry");
        let moved_home = fixture.path.join("moved-home");
        std::thread::scope(|scope| {
            let getter = scope.spawn(|| driver.get("default"));
            started
                .recv_timeout(AUTH_READ_GUARD)
                .expect("auth read started");
            driver.state.lock().expect("registry lock").value["accounts"]["default"]["home"] =
                Value::String(moved_home.to_string_lossy().into_owned());
            release
                .send(metadata(
                    AuthStatus::Ready,
                    Some("stale"),
                    Some("chatgpt:stale"),
                ))
                .expect("release stale auth read");
            // The stale read is discarded, so the get repeats the read under the lock.
            release
                .send(metadata(
                    AuthStatus::Ready,
                    Some("current"),
                    Some("chatgpt:current"),
                ))
                .expect("release current auth read");
            assert_eq!(getter.join().expect("get thread"), Ok(AccountStatus::Ready));
        });
        let row = driver.internal_record("default").expect("account row");
        assert_eq!(row["accountId"], Value::String("current".into()));
    }

    #[test]
    fn refresh_preserves_python_account_insertion_order() {
        let fixture = TestDir::new();
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent"))
            .expect("create registry directory");
        let bytes = format!(
            r#"{{"version":1,"defaultAccountKey":"z","accounts":{{"z":{{"id":"z","home":"{}","label":"Z","source":"Codex CLI","status":"signedOut"}},"a":{{"id":"a","home":"{}","label":"A","source":"Codex CLI","status":"signedOut"}}}},"logins":{{}},"deleteReceipts":{{}}}}"#,
            fixture.path.display(),
            fixture.path.display()
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
        let path = fixture.path.join("accounts/registry.json");
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
        let binary = fixture.path.join("claude-fake");
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
                provider: studio_runtime_providers::ProviderKind::Claude,
                home: fixture.path.clone(),
                claude_options: Some(serde_json::json!({"configDir":fixture.path})),
            };
            fs::write(fixture.path.join(".claude.json"), serde_json::json!({"oauthAccount":{
                "emailAddress":"claude@example.invalid", "organizationUuid":"org-a", "accountUuid":"account-a"
            }}).to_string()).expect("isolated native account metadata");
            let metadata = ClaudeCliAuthReader::read_with_environment(&profile, &environment);
            assert_eq!(metadata.status, AuthStatus::Ready);
            assert_eq!(
                metadata.account_id.as_deref(),
                Some("claude:claude@example.invalid")
            );
            assert_eq!(
                metadata.credential_identity,
                Some(claude_identity::identity(
                    "claude@example.invalid",
                    Some("org-a"),
                    Some("account-a")
                ))
            );
            assert_eq!(parse_claude_status(b"[]", true).status, AuthStatus::Error);

            let fallback_home = fixture.path.join("fake-home");
            let fallback_binary = fallback_home.join(".local/bin/claude");
            fs::create_dir_all(fallback_binary.parent().expect("fallback parent"))
                .expect("create fallback bin directory");
            fs::copy(&binary, &fallback_binary).expect("copy fake CLI to fallback path");
            fs::set_permissions(&fallback_binary, fs::Permissions::from_mode(0o700))
                .expect("make fallback CLI executable");
            let empty_path = fixture.path.join("empty-path");
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

    #[test]
    fn auth_status_probe_marks_only_successful_parseable_results_conclusive() {
        use AuthStatus::{Ready, SignedOut};

        let cases = [
            (None, br#"{"loggedIn":false}"#.as_slice(), SignedOut, false),
            (
                Some(false),
                br#"{"loggedIn":false}"#.as_slice(),
                SignedOut,
                false,
            ),
            (Some(true), b"not-json".as_slice(), AuthStatus::Error, false),
            (Some(true), b"{}".as_slice(), SignedOut, false),
            (
                Some(true),
                br#"{"loggedIn":1}"#.as_slice(),
                SignedOut,
                false,
            ),
            (
                Some(true),
                br#"{"loggedIn":true}"#.as_slice(),
                SignedOut,
                false,
            ),
            (
                Some(true),
                br#"{"loggedIn":true,"authMethod":7}"#.as_slice(),
                SignedOut,
                false,
            ),
            (
                Some(true),
                br#"{"loggedIn":true,"authMethod":"claude.ai","email":7}"#.as_slice(),
                AuthStatus::Error,
                false,
            ),
            (
                Some(true),
                br#"{"loggedIn":false,"futureProviderField":{"value":1}}"#.as_slice(),
                SignedOut,
                true,
            ),
            (
                Some(true),
                br#"{"loggedIn":true,"loggedIn":false}"#.as_slice(),
                SignedOut,
                true,
            ),
            (
                Some(true),
                br#"{"loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test"}"#
                    .as_slice(),
                Ready,
                true,
            ),
        ];
        for (exit, output, status, conclusive) in cases {
            let (metadata, actual_conclusive) = claude::auth_status_probe_from_output(output, exit);
            assert_eq!(metadata.status, status);
            assert_eq!(actual_conclusive, conclusive);
        }
        assert_eq!(parse_claude_status(b"{}", true).status, SignedOut);
        assert_eq!(
            parse_claude_status(br#"{"loggedIn":true}"#, true).status,
            SignedOut
        );
    }

    #[test]
    fn malformed_and_unreadable_registry_loads_are_refused() {
        let fixture = TestDir::new();
        let malformed = fixture.path.join("malformed.json");
        fs::write(&malformed, b"{").expect("write malformed fixture");
        assert_eq!(
            AccountDriver::open(&malformed).err(),
            Some(AccountDriverError::InvalidRegistry)
        );
        let unreadable = fixture.path.join("directory.json");
        fs::create_dir(&unreadable).expect("create unreadable fixture");
        assert_eq!(
            AccountDriver::open(&unreadable).err(),
            Some(AccountDriverError::ReadRegistry)
        );
    }

    #[test]
    fn failed_replacement_does_not_change_original_and_sync_failure_is_distinct() {
        let fixture = TestDir::new();
        let path = fixture.path.join("registry.json");
        fs::write(&path, b"before").expect("write original");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&path, fs::Permissions::from_mode(0o644))
                .expect("set original file permissions");
            fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o755))
                .expect("set parent directory permissions");
        }
        let fail = |_: &Path, _: &Path| Err(std::io::Error::other("refused"));
        assert!(matches!(
            atomic_replace_with(&path, b"after", fail, |_| Ok(())),
            Err(ReplaceError::NotReplaced)
        ));
        assert_eq!(fs::read(&path).expect("read unchanged registry"), b"before");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&fixture.path)
                    .expect("read parent after failed replacement")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
        assert!(matches!(atomic_replace(&path, b"after"), Ok(())));
        assert_eq!(fs::read(&path).expect("read replaced registry"), b"after");
        assert!(matches!(
            atomic_replace_with(
                &path,
                b"after",
                |temp, target| fs::rename(temp, target),
                |_| Err(std::io::Error::other("sync"))
            ),
            Err(ReplaceError::ReplacedNotDurable)
        ));
        assert_eq!(fs::read(&path).expect("read replaced registry"), b"after");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&path)
                    .expect("read registry permissions")
                    .permissions()
                    .mode()
                    & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&fixture.path)
                    .expect("read directory permissions")
                    .permissions()
                    .mode()
                    & 0o777,
                0o700
            );
        }
    }

    #[test]
    fn re_register_restores_only_a_true_deleted_marker() {
        let fixture = TestDir::new();
        let home = fixture.path.join("profile");
        let canonical = CanonicalPath::existing(&home)
            .map(CanonicalPath::into_path_buf)
            .unwrap_or_else(|_| home.clone());
        let metadata = metadata(
            AuthStatus::Ready,
            Some("stable-account"),
            Some("chatgpt:stable-account"),
        );
        let mut value = serde_json::json!({
            "version": 1,
            "defaultAccountKey": "existing",
            "accounts": {"existing": {
                "id": "existing", "home": canonical, "label": "Profile",
                "source": "Local profile", "status": "ready",
                "accountId": "stable-account"
            }},
            "logins": {}
        });
        let mut order = RegistryOrder {
            accounts: vec!["existing".into()],
            logins: Vec::new(),
        };
        for marker in [Value::Bool(false), Value::Null] {
            value["accounts"]["existing"]["deleted"] = marker.clone();
            assert!(
                !register_metadata(&mut value, &mut order, &canonical, &metadata, true)
                    .expect("same account remains unchanged")
            );
            assert_eq!(value["accounts"]["existing"]["deleted"], marker);
        }
        value["accounts"]["existing"]["deleted"] = Value::Bool(true);
        assert!(
            register_metadata(&mut value, &mut order, &canonical, &metadata, true)
                .expect("deleted row is restored")
        );
        assert!(value["accounts"]["existing"].get("deleted").is_none());
    }

    #[test]
    fn configured_codex_home_expands_and_resolves_like_python() {
        let fixture = TestDir::new();
        let home = fixture.path.join("home");
        let selected = home.join("selected-profile");
        fs::create_dir_all(&selected).expect("create selected profile");
        assert_eq!(
            resolve_codex_home(Some(OsStr::new("~/selected-profile")), &home),
            Ok(CanonicalPath::existing(&selected)
                .expect("resolve selected profile")
                .into_path_buf())
        );
        let canonical_home = CanonicalPath::existing(&home).expect("resolve fixture home");
        // An empty CODEX_HOME falls back like Python's `or`; a missing
        // directory keeps its written name below the resolved parent.
        assert_eq!(
            resolve_codex_home(Some(OsStr::new("")), &home),
            Ok(canonical_home.join(".codex"))
        );
        assert_eq!(
            resolve_codex_home(None, &home.join("missing/../selected-profile/x/..")),
            Ok(canonical_home.join("selected-profile/.codex"))
        );
        assert!(matches!(
            resolve_codex_home(Some(OsStr::new("~/.claude/codex")), &home),
            Err(CodexHomeError::InsideClaude(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn codex_home_resolves_existing_and_dangling_symlinks_like_python() {
        let fixture = TestDir::new();
        let real = fixture.path.join("real");
        fs::create_dir_all(&real).expect("create link target");
        std::os::unix::fs::symlink(&real, fixture.path.join("link")).expect("create link");
        std::os::unix::fs::symlink(fixture.path.join("absent"), fixture.path.join("dangling"))
            .expect("create dangling link");
        let base = CanonicalPath::existing(&fixture.path).expect("resolve fixture");
        assert_eq!(
            resolve_codex_home(Some(fixture.path.join("link/profile").as_os_str()), &real),
            Ok(base.join("real/profile"))
        );
        assert_eq!(
            resolve_codex_home(Some(fixture.path.join("dangling").as_os_str()), &real),
            Ok(base.join("absent"))
        );
    }

    #[test]
    fn default_registry_matches_python_bytes_and_is_never_replaced() {
        let fixture = TestDir::new();
        let home = fixture.path.join("codex-\u{e9}");
        let path = fixture.path.join("state/accounts/registry.json");
        assert_eq!(create_default_registry(&path, &home), Ok(true));
        let expected = format!(
            "{{\"version\": 1, \"defaultAccountKey\": \"default\", \"accounts\": {{\"default\": \
             {{\"id\": \"default\", \"home\": \"{}\", \"label\": \"Codex\", \"source\": \
             \"Codex CLI\", \"accountId\": null, \"email\": null, \"plan\": null, \"status\": \
             \"signedOut\"}}}}, \"logins\": {{}}, \"deleteReceipts\": {{}}}}",
            fixture.path.join("codex-\\u00e9").display()
        );
        let written = fs::read(&path).expect("read created registry");
        assert_eq!(String::from_utf8_lossy(&written), expected);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode =
                |path: &Path| fs::metadata(path).expect("metadata").permissions().mode() & 0o777;
            assert_eq!(mode(&path), 0o600);
            assert_eq!(mode(path.parent().expect("registry directory")), 0o700);
        }

        fs::write(&path, b"{\"keep\": true}").expect("replace with sentinel");
        fs::create_dir_all(&home).expect("create auth home");
        fs::write(home.join("auth.json"), br#"{"OPENAI_API_KEY":"fixture"}"#)
            .expect("write auth fixture");
        assert_eq!(create_default_registry(&path, &home), Ok(false));
        assert_eq!(
            fs::read(&path).expect("read kept file"),
            b"{\"keep\": true}"
        );
        let entries = fs::read_dir(path.parent().expect("registry directory"))
            .expect("list registry directory")
            .count();
        assert_eq!(entries, 1, "no temporary registry file remains");
    }

    #[test]
    fn default_registry_records_unreadable_auth_with_python_error_text() {
        let fixture = TestDir::new();
        let home = fixture.path.join("codex-home");
        fs::create_dir_all(&home).expect("create auth home");
        fs::write(home.join("auth.json"), b"not json").expect("write malformed auth");
        let bytes = default_registry_bytes(&home).expect("default registry encodes");
        let value: Value = serde_json::from_slice(&bytes).expect("registry is JSON");
        let row = &value["accounts"]["default"];
        assert_eq!(row["status"], "error");
        assert_eq!(row["error"], CODEX_AUTH_ERROR);
        assert!(row.get("_credentialIdentity").is_none());
        assert_eq!(codex_auth_metadata(&home).error, Some(CODEX_AUTH_ERROR));
    }

    #[test]
    fn revision_tracks_public_changes_not_internal_receipts_or_replays() {
        let fixture = TestDir::new();
        let home = fixture.path.join("profile");
        fs::create_dir_all(&home).expect("create profile");
        fs::write(home.join("auth.json"), br#"{"OPENAI_API_KEY":"fixture"}"#)
            .expect("write disposable auth file");
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent")).expect("create registry dir");
        let mut value: Value = serde_json::from_slice(&registry(&home)).expect("registry fixture");
        value["accounts"]["default"]["status"] = Value::String("ready".into());
        value["accounts"]["default"]["accountId"] = Value::String("stable".into());
        value["accounts"]["default"]["_credentialIdentity"] =
            Value::String("chatgpt:stable".into());
        fs::write(&path, serde_json::to_vec(&value).expect("encode fixture"))
            .expect("write registry");
        let driver = AccountDriver::open_with_reader(
            &path,
            FakeReader(metadata(
                AuthStatus::Ready,
                Some("stable"),
                Some("chatgpt:stable"),
            )),
        )
        .expect("open registry");
        let mut changes = driver.change_signal().subscribe();
        assert_eq!(driver.revision().expect("read revision"), 0);
        driver
            .set_name("default", "Codex", "abc")
            .expect("same visible label is valid");
        assert_eq!(driver.revision().expect("read revision"), 0);
        assert!(
            !changes
                .has_changed()
                .expect("profile change signal remains open")
        );
        let persisted: Value = serde_json::from_slice(&fs::read(&path).expect("read registry"))
            .expect("decode registry");
        assert_ne!(
            persisted.get("pendingAccountsChange"),
            Some(&Value::Bool(true))
        );
        driver
            .set_name("default", "Renamed", "def")
            .expect("change visible label");
        assert_eq!(driver.revision().expect("read revision"), 1);
        assert!(
            changes
                .has_changed()
                .expect("public profile change publishes")
        );
        let _observed_generation = *changes.borrow_and_update();
        driver
            .set_name("default", "Renamed", "def")
            .expect("replay name receipt");
        assert_eq!(driver.revision().expect("read revision"), 1);
        assert!(
            !changes
                .has_changed()
                .expect("receipt replay does not publish a profile change")
        );
    }

    #[test]
    fn re_registering_false_or_null_deleted_markers_is_a_true_noop() {
        let fixture = TestDir::new();
        let home = fixture.path.join("profile");
        fs::create_dir_all(&home).expect("create profile");
        fs::write(home.join("auth.json"), br#"{"OPENAI_API_KEY":"fixture"}"#)
            .expect("write disposable auth file");
        let canonical = CanonicalPath::existing(&home).expect("resolve profile");
        let path = fixture.path.join("accounts/registry.json");
        fs::create_dir_all(path.parent().expect("registry parent")).expect("create registry dir");
        let mut value: Value = serde_json::from_slice(&registry(&canonical)).expect("fixture");
        value["accounts"]["default"]["accountId"] = Value::String("stable".into());
        for marker in [Value::Bool(false), Value::Null] {
            value["accounts"]["default"]["deleted"] = marker.clone();
            fs::write(&path, serde_json::to_vec(&value).expect("encode registry"))
                .expect("write registry");
            let before = fs::read(&path).expect("registry bytes before registration");
            let driver = AccountDriver::open(&path).expect("open registry");
            driver
                .register(canonical.to_str().expect("profile path is UTF-8"))
                .expect("register existing account");
            assert_eq!(
                fs::read(&path).expect("registry bytes after registration"),
                before
            );
            let saved: Value = serde_json::from_slice(&before).expect("saved registry");
            assert_eq!(saved["accounts"]["default"]["deleted"], marker);
            assert_eq!(driver.revision().expect("public revision"), 0);
        }
    }

    #[test]
    fn post_refresh_validation_failure_leaves_registry_untouched() {
        let fixture = TestDir::new();
        let path = fixture.path.join("registry.json");
        let original = registry(&fixture.path);
        fs::write(&path, &original).expect("write valid registry");
        let (mut candidate, order) = decode_registry(&original).expect("decode original");
        candidate["accounts"]["default"]["status"] = Value::String("invalid".into());
        let encoded = encode_registry(&candidate, &order).expect("encode candidate");
        assert!(decode_registry(&encoded).is_err());
        assert_eq!(fs::read(path).expect("read unchanged registry"), original);
    }

    #[test]
    fn refresh_keeps_unknown_fields_and_signed_out_identity() {
        let fixture = TestDir::new();
        let path = fixture.path.join("registry.json");
        let mut value: Value =
            serde_json::from_slice(&registry(&fixture.path)).expect("fixture JSON");
        let row = value["accounts"]["default"]
            .as_object_mut()
            .expect("account row");
        row.insert("accountId".into(), Value::String("remember-me".into()));
        row.insert(
            "_credentialIdentity".into(),
            Value::String("chatgpt:remember-me".into()),
        );
        row.insert("provider".into(), Value::String("claude".into()));
        row.insert(
            "claudeOptions".into(),
            serde_json::json!({"futureField":"preserve"}),
        );
        fs::write(&path, serde_json::to_vec(&value).expect("encode")).expect("write registry");
        let driver = AccountDriver::open_with_reader(
            &path,
            FakeReader(metadata(AuthStatus::SignedOut, None, None)),
        )
        .expect("open");
        assert_eq!(
            driver.get("default").expect("refresh"),
            AccountStatus::SignedOut
        );
        let saved: Value = serde_json::from_slice(&fs::read(path).expect("read refreshed"))
            .expect("parse refreshed");
        assert_eq!(saved["accounts"]["default"]["accountId"], "remember-me");
        assert_eq!(
            saved["accounts"]["default"]["_credentialIdentity"],
            "chatgpt:remember-me"
        );
        assert_eq!(
            saved["accounts"]["default"]["claudeOptions"]["futureField"],
            "preserve"
        );
    }

    #[test]
    fn refresh_statuses_follow_the_reported_auth_metadata() {
        let fixture = TestDir::new();
        let home = fixture.path.join("codex-home");
        fs::create_dir_all(&home).expect("create fixture home");
        let mut base: Value = serde_json::from_slice(&registry(&home)).expect("registry");
        base["accounts"]["default"]["provider"] = Value::String("claude".into());
        base["accounts"]["default"]["accountId"] = Value::String("stable".into());
        base["accounts"]["default"]["_credentialIdentity"] = Value::String("chatgpt:stable".into());
        for (index, auth) in [
            serde_json::json!({"status":"ready", "accountId":"stable", "_credentialIdentity":"chatgpt:stable", "email":"fixture@example.invalid", "plan":"plus"}),
            serde_json::json!({"status":"signedOut", "accountId":null, "_credentialIdentity":null, "email":null, "plan":null}),
            serde_json::json!({"status":"error", "error":"Cannot read this profile's authentication"}),
        ].into_iter().enumerate() {
            let rust_path = fixture.path.join(format!("rust-{index}.json"));
            fs::write(&rust_path, serde_json::to_vec(&base).expect("Rust fixture")).expect("write Rust fixture");
            let md = AuthMetadata {
                status: match auth["status"].as_str().unwrap_or_default() { "ready" => AuthStatus::Ready, "error" => AuthStatus::Error, _ => AuthStatus::SignedOut },
                account_id: auth.get("accountId").and_then(Value::as_str).map(str::to_owned),
                email: auth.get("email").and_then(Value::as_str).map(str::to_owned),
                plan: auth.get("plan").and_then(Value::as_str).map(str::to_owned),
                credential_identity: auth.get("_credentialIdentity").and_then(Value::as_str).map(str::to_owned),
                error: auth.get("error").and_then(Value::as_str).map(|_| AUTH_ERROR),
                error_kind: None,
            };
            AccountDriver::open_with_reader(&rust_path, FakeReader(md)).expect("open Rust fixture").get("default").expect("Rust refresh");
            let rust: Value = serde_json::from_slice(&fs::read(rust_path).expect("read Rust result")).expect("parse Rust result");
            for field in ["status", "accountId", "_credentialIdentity", "email", "plan"] {
                if let Some(expected) = auth.get(field).filter(|value| !value.is_null() || field == "email" || field == "plan") {
                    assert_eq!(&rust["accounts"]["default"][field], expected, "case {index}, field {field}");
                }
            }
        }
    }
}
