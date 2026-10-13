//! Refreshable account registry backed by Codex and Claude auth metadata.
//!
//! The registry is loaded once. Each `get` refreshes its provider metadata,
//! validates the resulting document, and atomically persists it while holding
//! the driver's mutex. Credential material is never retained in this module.

pub mod claude;
#[cfg(test)]
#[allow(clippy::duplicate_mod)]
#[path = "../tests/support/prepared_python.rs"]
mod prepared_python_test_support;

use crate::account_policy::{AccountsSnapshot, RegistryOrder, decode_registry, encode_registry};
use crate::event_write::python_strip;
use base64::Engine as _;
#[cfg(unix)]
use process_wrap::std::ChildWrapper;
#[cfg(unix)]
use process_wrap::std::CommandWrap;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use serde_json::Value;
use std::collections::BTreeMap;
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::{Duration, Instant};
use thiserror::Error;

const AUTH_FILE_LIMIT: u64 = 1024 * 1024;
const CLAUDE_OUTPUT_LIMIT: usize = 1024 * 1024;
const CLAUDE_TIMEOUT: Duration = Duration::from_secs(8);
const CLAUDE_BIN_OVERRIDE: &str = "STUDIO_CLAUDE_BIN";
const AUTH_ERROR: &str = "Cannot read this profile's authentication";
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
    Unchanged,
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

    #[cfg(target_os = "macos")]
    fn unchanged(reason: &'static str) -> Self {
        Self {
            status: AuthStatus::Unchanged,
            error_kind: Some(reason),
            ..Self::signed_out()
        }
    }
}

#[derive(Clone, Eq, PartialEq)]
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
        Err(_) => return AuthMetadata::error(None),
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
        return AuthMetadata::error(None);
    };
    codex_metadata_from_value(&auth).unwrap_or_else(|_| AuthMetadata::error(None))
}

fn codex_metadata_from_value(auth: &Value) -> Result<AuthMetadata, ()> {
    let auth = auth.as_object().ok_or(())?;
    let tokens = object_if_truthy(auth.get("tokens"))?;
    let token = tokens.and_then(|tokens| {
        tokens
            .get("id_token")
            .filter(|value| python_truthy(value))
            .or_else(|| {
                tokens
                    .get("access_token")
                    .filter(|value| python_truthy(value))
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
        .filter(|value| python_truthy(value))
        .or_else(|| {
            details
                .and_then(|details| details.get("chatgpt_account_id"))
                .filter(|value| python_truthy(value))
        })
        .map(|value| value.as_str().map(str::to_owned).ok_or(()))
        .transpose()?;
    let email = claims
        .get("email")
        .filter(|value| python_truthy(value))
        .or_else(|| {
            profile
                .and_then(|profile| profile.get("email"))
                .filter(|value| python_truthy(value))
        })
        .map(|value| value.as_str().map(str::to_owned).ok_or(()))
        .transpose()?;
    let api_key = auth
        .get("OPENAI_API_KEY")
        .filter(|value| python_truthy(value));
    if api_key.is_some_and(|value| value.as_str().is_none()) {
        return Err(());
    }
    let api_key = api_key.and_then(Value::as_str);
    let ready = (account_id.as_ref().is_some_and(|value| !value.is_empty())
        && tokens
            .and_then(|tokens| tokens.get("access_token"))
            .is_some_and(python_truthy))
        || api_key.is_some();
    let credential_identity = account_id
        .as_ref()
        .filter(|value| !value.is_empty())
        .map(|value| format!("chatgpt:{value}"))
        .or_else(|| api_key.map(|value| format!("api:{}", hex_sha256(value.as_bytes()))));
    let plan = details
        .and_then(|details| details.get("chatgpt_plan_type"))
        .filter(|value| python_truthy(value))
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
    match value.filter(|value| python_truthy(value)) {
        None => Ok(None),
        Some(value) => value.as_object().map(Some).ok_or(()),
    }
}

fn python_truthy(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Number(value) => value.as_f64().is_none_or(|number| number != 0.0),
        Value::String(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::Object(value) => !value.is_empty(),
    }
}

struct ClaudeCliAuthReader;

impl ProviderAuthReader for ClaudeCliAuthReader {
    fn read(&self, profile: &AuthProfile) -> AuthMetadata {
        #[cfg(target_os = "macos")]
        {
            let _ = profile;
            return AuthMetadata::unchanged("macos-passive-auth-unavailable");
        }
        #[cfg(not(target_os = "macos"))]
        {
            let environment = env::var(CLAUDE_BIN_OVERRIDE)
                .ok()
                .map(|value| BTreeMap::from([(CLAUDE_BIN_OVERRIDE.to_owned(), value)]))
                .unwrap_or_default();
            Self::read_with_environment(profile, &environment)
        }
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
        let Some(binary) = resolve_claude_binary(
            configured_binary,
            environment.get(CLAUDE_BIN_OVERRIDE).map(String::as_str),
            path.as_deref(),
            home.as_deref(),
        ) else {
            return AuthMetadata::signed_out();
        };
        let output = run_claude_status(&binary, options);
        let (stdout, succeeded) = match output {
            Ok(output) => output,
            Err(kind) => return AuthMetadata::error(Some(kind)),
        };
        parse_claude_status(&stdout, succeeded)
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

fn run_claude_status_with_timeout(
    binary: &str,
    options: Option<&Value>,
    timeout: Duration,
) -> Result<(Vec<u8>, bool), &'static str> {
    #[cfg(unix)]
    let mut child = CommandWrap::with_new(binary, |command| {
        configure_claude_status_command(command, options);
    })
    .wrap(ProcessGroup::leader())
    .spawn()
    .map_err(|_| "spawn")?;
    #[cfg(windows)]
    let mut child = {
        let mut command = Command::new(binary);
        configure_claude_status_command(&mut command, options);
        command.spawn().map_err(|_| "spawn")?
    };
    #[cfg(unix)]
    let Some(stdout) = child.stdout().take() else {
        return Err("spawn");
    };
    #[cfg(windows)]
    let Some(stdout) = child.stdout.take() else {
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
        let pid = rustix::process::Pid::from_raw(child.id() as i32).ok_or("spawn")?;
        let exited = match rustix::process::waitid(
            rustix::process::WaitId::Pid(pid),
            rustix::process::WaitIdOptions::EXITED
                | rustix::process::WaitIdOptions::NOHANG
                | rustix::process::WaitIdOptions::NOWAIT,
        ) {
            Ok(status) => status.is_some(),
            Err(rustix::io::Errno::INTR) => continue,
            Err(_) => {
                terminate_claude_process_group(child);
                return Err("spawn");
            }
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

fn parse_claude_status(stdout: &[u8], succeeded: bool) -> AuthMetadata {
    let Ok(value) = serde_json::from_slice::<Value>(stdout) else {
        return AuthMetadata::error(Some("parser"));
    };
    if !value.is_object() {
        return AuthMetadata::error(Some("parser"));
    }
    if !succeeded
        || value.get("loggedIn").and_then(Value::as_bool) != Some(true)
        || value.get("authMethod").and_then(Value::as_str) != Some("claude.ai")
    {
        return AuthMetadata::signed_out();
    }
    let Some(email) = value.get("email").and_then(nonempty_str) else {
        return AuthMetadata::error(Some("parser"));
    };
    let account_id = format!("claude:{email}");
    AuthMetadata {
        status: AuthStatus::Ready,
        account_id: Some(account_id.clone()),
        email: Some(email.to_owned()),
        plan: value
            .get("subscriptionType")
            .and_then(nonempty_str)
            .map(str::to_owned),
        credential_identity: Some(account_id),
        error: None,
        error_kind: None,
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
    #[error("Registry replacement completed but directory durability is unconfirmed")]
    DurabilityUnconfirmed,
    #[error("Account registry lock is unavailable")]
    Poisoned,
    #[error("Sign in to this account first")]
    SignInRequired,
    #[error("This account was deleted")]
    DeletedAccount,
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
}

struct RegistryState {
    value: Value,
    order: RegistryOrder,
    revision: u64,
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
        let key = request
            .account_key
            .clone()
            .unwrap_or_else(|| format!("login-{}", request.login_id));
        let mut receipt = serde_json::json!({"requestId":request.login_id,"accountKey":key,"status":"starting","createdAt":now});
        if let Some(account) = &request.account_key {
            let row = registry_account(&value, account)?;
            if row
                .get("provider")
                .and_then(Value::as_str)
                .unwrap_or("codex")
                != "codex"
                || !row.get("accountId").is_some_and(python_truthy)
                || row.get("duplicateOf").is_some_and(python_truthy)
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
        persist_registry(&self.path, &mut state, value, &order)?;
        Ok(LoginReservation::Dispatch(receipt))
    }

    pub fn finish_codex_login(
        &self,
        request: &str,
        account: &str,
        response: Option<&Value>,
        sent: bool,
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
                && receipt.get("loginId").is_some_and(python_truthy))
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
            receipt["error"] = serde_json::json!(if sent {
                "Sign-in response is unconfirmed. Check its status before starting again."
            } else {
                "Could not start sign-in. Try again."
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
        if !receipt.get("loginId").is_some_and(python_truthy) {
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
        persist_registry(&self.path, &mut state, value, &order)?;
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
        Ok(Self {
            path,
            state: Mutex::new(RegistryState {
                value,
                order,
                revision: 0,
            }),
            reader,
        })
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
        let mut state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        let status = self.refresh_value(&mut updated, account, false)?;
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
        project_snapshot(&state.value, &state.order)
    }

    /// Refresh the response projection from credentials without persisting it.
    pub fn listing_snapshot(&self) -> Result<AccountsSnapshot, AccountDriverError> {
        let state = self
            .state
            .lock()
            .map_err(|_| AccountDriverError::Poisoned)?;
        let mut updated = state.value.clone();
        for key in &state.order.accounts {
            let stored = registry_account(&updated, key)?.clone();
            if self.refresh_value(&mut updated, key, false)? == AccountStatus::Error {
                // A listing cannot replace persisted status with an error when
                // the auth reader could not determine the account's state.
                *registry_account_mut(&mut updated, key)? = stored;
            }
        }
        project_snapshot(&updated, &state.order)
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
            return project_snapshot(&state.value, &state.order);
        };
        let mut updated = state.value.clone();
        self.refresh_value(&mut updated, account, true)?;
        if !eligible_default(&updated, account) {
            return Err(AccountDriverError::SignInRequired);
        }
        updated["defaultAccountKey"] = Value::String(account.to_owned());
        self.persist(&mut state, updated)?;
        project_snapshot(&state.value, &state.order)
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
            return project_snapshot(&state.value, &state.order);
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
        project_snapshot(&state.value, &state.order)
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
        project_snapshot(&state.value, &state.order)
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
        project_snapshot(&state.value, &state.order)
    }

    pub fn register(&self, home: &str) -> Result<AccountsSnapshot, AccountDriverError> {
        if python_strip(home).is_empty() {
            return Err(AccountDriverError::InvalidCodexProfile);
        }
        let path = expanded_profile_path(home)
            .canonicalize()
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
            persist_registry(&self.path, &mut state, updated, &order)?;
        }
        project_snapshot(&state.value, &state.order)
    }

    pub fn discover_codex(&self) -> Result<AccountsSnapshot, AccountDriverError> {
        let home = env::var_os("HOME").map(PathBuf::from).unwrap_or_default();
        let codex_home = env::var_os("CODEX_HOME")
            .map(|value| {
                let configured = PathBuf::from(value);
                resolve_configured_home(&configured.to_string_lossy(), &home)
            })
            .unwrap_or_else(|| home.join(".codex"));
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
                && let Ok(path) = candidate.canonicalize()
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
            persist_registry(&self.path, &mut state, updated, &order)?;
        }
        project_snapshot(&state.value, &state.order)
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
            if row.get("duplicateOf").is_some_and(python_truthy) {
                return Ok(status_from_value(row.get("status")));
            }
            AuthProfile {
                provider: row
                    .get("provider")
                    .and_then(Value::as_str)
                    .unwrap_or("codex")
                    .to_owned(),
                home: PathBuf::from(row.get("home").and_then(Value::as_str).unwrap_or_default()),
                claude_options: row.get("claudeOptions").cloned(),
            }
        };
        let metadata = self.reader.read(&profile);
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
        persist_registry(&self.path, state, updated, &order)
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
                state.revision = state.revision.wrapping_add(1);
            }
            Ok(())
        }
        Err(ReplaceError::NotReplaced) => Err(AccountDriverError::WriteRegistry),
        Err(ReplaceError::ReplacedNotDurable) => {
            state.value = checked;
            state.order = order;
            if accounts_changed {
                state.revision = state.revision.wrapping_add(1);
            }
            Err(AccountDriverError::DurabilityUnconfirmed)
        }
    }
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
            && !row.get("duplicateOf").is_some_and(python_truthy))
        .then_some(key.as_str())
    })
}

fn expanded_profile_path(home: &str) -> PathBuf {
    let root = env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(home));
    expand_user_path(home, &root)
}

fn resolve_configured_home(configured: &str, home: &Path) -> PathBuf {
    let expanded = expand_user_path(configured, home);
    expanded.canonicalize().unwrap_or(expanded)
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

fn apply_metadata(row: &mut serde_json::Map<String, Value>, metadata: &AuthMetadata) {
    if metadata.status == AuthStatus::Unchanged {
        return;
    }
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
            .is_some_and(|(a, b)| a != b)
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
                AuthStatus::Unchanged => return,
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
    let permissions = fs::metadata(path)
        .map_err(|_| ReplaceError::NotReplaced)?
        .permissions();
    let mut temp =
        tempfile::NamedTempFile::new_in(parent).map_err(|_| ReplaceError::NotReplaced)?;
    (|| {
        temp.as_file()
            .set_permissions(permissions)
            .map_err(|_| ReplaceError::NotReplaced)?;
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
mod tests {
    use super::*;
    const PLACEHOLDER_KEY: &str = "placeholder-secret-do-not-persist";

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

        let started = Instant::now();
        assert!(matches!(
            run_claude_status_with_timeout(
                command.to_str().expect("fixture path is UTF-8"),
                None,
                Duration::from_secs(2),
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
        assert!(
            started.elapsed() < Duration::from_secs(4),
            "timeout cleanup is bounded"
        );
        let pid = rustix::process::Pid::from_raw(descendant_pid).expect("valid pid");
        let deadline = Instant::now() + Duration::from_secs(2);
        while process_is_running(pid) && Instant::now() < deadline {
            std::thread::yield_now();
        }
        assert!(!process_is_running(pid), "group descendant terminated");
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

    fn isolated_python(_fixture: &Path) -> PathBuf {
        let server = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        super::prepared_python_test_support::prepared_python(&server)
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
        let home = fixture.path.join("python-home");
        let cache = fixture.path.join("python-cache");
        let temp = fixture.path.join("python-tmp");
        for directory in [&home, &cache, &temp] {
            fs::create_dir_all(directory).expect("create disposable Python environment");
        }
        let output = Command::new(isolated_python(&fixture.path))
            .args(["-c", script])
            .arg(&server)
            .arg(&fixture.path)
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .env("TMPDIR", &temp)
            .env("HOME", &home)
            .env("XDG_CACHE_HOME", &cache)
            .env("XDG_CONFIG_HOME", fixture.path.join("fake-config"))
            .env("XDG_STATE_HOME", fixture.path.join("fake-state"))
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
                provider: "claude".into(),
                home: fixture.path.clone(),
                claude_options: None,
            };
            let metadata = ClaudeCliAuthReader::read_with_environment(&profile, &environment);
            assert_eq!(metadata.status, AuthStatus::Ready);
            assert_eq!(
                metadata.account_id.as_deref(),
                Some("claude:claude@example.invalid")
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
            fs::set_permissions(&path, fs::Permissions::from_mode(0o640))
                .expect("set original file permissions");
            fs::set_permissions(&fixture.path, fs::Permissions::from_mode(0o750))
                .expect("set parent directory permissions");
        }
        let parent_permissions = fs::metadata(&fixture.path)
            .expect("read parent metadata")
            .permissions();
        let fail = |_: &Path, _: &Path| Err(std::io::Error::other("refused"));
        assert!(matches!(
            atomic_replace_with(&path, b"after", fail, |_| Ok(())),
            Err(ReplaceError::NotReplaced)
        ));
        assert_eq!(fs::read(&path).expect("read unchanged registry"), b"before");
        assert_eq!(
            fs::metadata(&fixture.path)
                .expect("read parent after failed replacement")
                .permissions(),
            parent_permissions
        );
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
                0o640
            );
        }
    }

    #[test]
    fn re_register_restores_only_a_true_deleted_marker() {
        let fixture = TestDir::new();
        let home = fixture.path.join("profile");
        let canonical = home.canonicalize().unwrap_or_else(|_| home.clone());
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
            resolve_configured_home("~/selected-profile", &home),
            selected.canonicalize().expect("resolve selected profile")
        );
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
        assert_eq!(driver.revision().expect("read revision"), 0);
        driver
            .set_name("default", "Codex", "abc")
            .expect("same visible label is valid");
        assert_eq!(driver.revision().expect("read revision"), 0);
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
        driver
            .set_name("default", "Renamed", "def")
            .expect("replay name receipt");
        assert_eq!(driver.revision().expect("read revision"), 1);
    }

    #[test]
    fn re_registering_false_or_null_deleted_markers_is_a_true_noop() {
        let fixture = TestDir::new();
        let home = fixture.path.join("profile");
        fs::create_dir_all(&home).expect("create profile");
        fs::write(home.join("auth.json"), br#"{"OPENAI_API_KEY":"fixture"}"#)
            .expect("write disposable auth file");
        let canonical = home.canonicalize().expect("resolve profile");
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
    fn refresh_statuses_match_python_account_store_on_shared_fixture() {
        let fixture = TestDir::new();
        let server = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src");
        let home = fixture.path.join("codex-home");
        fs::create_dir_all(&home).expect("create fixture home");
        let root = fixture.path.join("python-state");
        let py_registry = root.join("accounts/registry.json");
        fs::create_dir_all(py_registry.parent().expect("parent")).expect("create Python state");
        let mut base: Value = serde_json::from_slice(&registry(&home)).expect("registry");
        base["accounts"]["default"]["provider"] = Value::String("claude".into());
        base["accounts"]["default"]["accountId"] = Value::String("stable".into());
        base["accounts"]["default"]["_credentialIdentity"] = Value::String("chatgpt:stable".into());
        for (index, auth) in [
            serde_json::json!({"status":"ready", "accountId":"stable", "_credentialIdentity":"chatgpt:stable", "email":"fixture@example.invalid", "plan":"plus"}),
            serde_json::json!({"status":"signedOut", "accountId":null, "_credentialIdentity":null, "email":null, "plan":null}),
            serde_json::json!({"status":"error", "error":"Cannot read this profile's authentication"}),
        ].into_iter().enumerate() {
            fs::write(&py_registry, serde_json::to_vec(&base).expect("Python registry bytes")).expect("write Python fixture");
            let home_dir = fixture.path.join(format!("py-home-{index}"));
            let cache = fixture.path.join(format!("py-cache-{index}"));
            let temp = fixture.path.join(format!("py-tmp-{index}"));
            for path in [&home_dir, &cache, &temp] { fs::create_dir_all(path).expect("isolate Python"); }
            let script = "import json,sys; sys.path.insert(0,sys.argv[1]); from codex_accounts import AccountStore; store=AccountStore(sys.argv[2]); store.refresh('default', json.loads(sys.argv[3])); print(json.dumps(store._row('default')))";
            let py = Command::new(isolated_python(&fixture.path)).args(["-c", script]).arg(&server).arg(&root).arg(auth.to_string())
                .env("HOME", &home_dir).env("XDG_CACHE_HOME", &cache).env("TMPDIR", &temp)
                .env("PYTHONDONTWRITEBYTECODE", "1").output().expect("run Python refresh");
            assert!(py.status.success(), "Python AccountStore.refresh failed");
            let oracle: Value = serde_json::from_slice(&py.stdout).expect("Python row");
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
            for field in ["status", "error", "accountId", "_credentialIdentity", "email", "plan"] {
                assert_eq!(rust["accounts"]["default"].get(field), oracle.get(field), "case {index}, field {field}");
            }
        }
    }
}
