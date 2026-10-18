//! One provider adapter per `ProviderKind`.
//!
//! The JSON-RPC wire (`thread/*`, `turn/*`, `model/list`, `account/read`) is shared
//! by both peers and stays in the account worker. An adapter owns only what sits
//! beside that wire: how the peer process is launched and the provider-specific
//! decisions that callers used to branch on with provider strings.

use crate::account_driver::{AuthMetadata, AuthStatus};
use crate::account_worker::WorkerError;
use crate::claude_bridge::{BridgeSnapshot, ResolvedBridge};
use crate::continuation_runtime::classify_usage_failure;
use crate::usage_resume::Cause as UsageCause;
use serde_json::Value;
use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
};
use studio_runtime_process::ProviderLaunchSpec;
pub use studio_runtime_providers::ProviderKind;

/// Inputs for launching the retained provider session of one host account.
pub struct HostLaunch<'a> {
    /// The account's registry profile (Claude reads its pinned identity and options).
    pub profile: &'a Value,
    /// Provider home (`CODEX_HOME` for Codex).
    pub home: String,
    /// The account has its own home and must not use ambient API keys.
    pub isolated: bool,
    /// Selected Codex executable; `None` uses `codex` from `PATH`.
    pub codex_executable: Option<&'a str>,
    /// Directory holding the Claude bridge (`bridge.mjs` and its SDK).
    pub claude_bridge_dir: &'a Path,
    pub root: PathBuf,
    pub environment: BTreeMap<String, String>,
}

/// A launch spec and the provider executable that the launched session runs.
pub struct ProviderLaunch {
    pub spec: ProviderLaunchSpec,
    pub executable: Option<String>,
}

pub trait ProviderAdapter: Send + Sync + 'static {
    fn kind(&self) -> ProviderKind;

    /// Launch spec for the retained session of one host account.
    fn host_launch(&self, input: HostLaunch<'_>) -> Result<ProviderLaunch, WorkerError>;

    /// Command line of the guest provider peer for one VM agent.
    fn vm_guest_command(&self, guest_home: &str, agent_id: &str) -> Vec<String>;

    /// Profile directory name under a home (`.codex` or `.claude`).
    fn profile_dir(&self) -> &'static str;

    /// Credential file copied into a VM guest profile.
    fn guest_credential_file(&self) -> &'static str;

    /// Usage, rate-limit or auth cause of a provider turn error, if any.
    fn classify_error(&self, error: &Value) -> Option<UsageCause>;

    /// The peer refused a request because the thread is still running a turn.
    fn is_busy_error(&self, error: &Value) -> bool;
}

struct CodexAdapter;
struct ClaudeAdapter;

#[must_use]
pub fn adapter(kind: ProviderKind) -> &'static dyn ProviderAdapter {
    match kind {
        ProviderKind::Codex => &CodexAdapter,
        ProviderKind::Claude => &ClaudeAdapter,
    }
}

/// Codex app-server peer over stdio.
const CODEX_APP_SERVER_ARGS: [&str; 3] = ["app-server", "--listen", "stdio://"];
/// Codex config override that keeps credentials in the account's own home.
const CODEX_FILE_CREDENTIALS: [&str; 2] = ["-c", "cli_auth_credentials_store=\"file\""];
/// JSON-RPC error the Claude bridge returns for a request on a busy thread.
const CLAUDE_BUSY_CODE: i64 = -32000;
const CLAUDE_BUSY_MESSAGE: &str = "Claude is still working";
/// Guest path of the Claude bridge inside the VM image.
const VM_CLAUDE_BRIDGE: &str = "/opt/codex-studio/claude_bridge/bridge.mjs";

impl ProviderAdapter for CodexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    fn host_launch(&self, input: HostLaunch<'_>) -> Result<ProviderLaunch, WorkerError> {
        let HostLaunch {
            home,
            isolated,
            codex_executable,
            root,
            mut environment,
            ..
        } = input;
        let mut command = vec![codex_executable.unwrap_or("codex").to_owned()];
        command.extend(CODEX_APP_SERVER_ARGS.map(str::to_owned));
        environment.insert("CODEX_HOME".into(), home);
        if isolated {
            command.extend(CODEX_FILE_CREDENTIALS.map(str::to_owned));
            environment.remove("OPENAI_API_KEY");
            environment.remove("CODEX_API_KEY");
        }
        Ok(ProviderLaunch {
            spec: ProviderLaunchSpec::new(command, environment, root).with_inherited_cwd(),
            executable: codex_executable.map(str::to_owned),
        })
    }

    fn vm_guest_command(&self, _guest_home: &str, _agent_id: &str) -> Vec<String> {
        let mut command = vec!["codex".to_owned()];
        command.extend(CODEX_APP_SERVER_ARGS.map(str::to_owned));
        command.extend(CODEX_FILE_CREDENTIALS.map(str::to_owned));
        command
    }

    fn profile_dir(&self) -> &'static str {
        ".codex"
    }

    fn guest_credential_file(&self) -> &'static str {
        "auth.json"
    }

    fn classify_error(&self, error: &Value) -> Option<UsageCause> {
        classify_usage_failure(error, None)
    }

    /// Codex queues or steers input into a running turn; it has no busy refusal.
    fn is_busy_error(&self, _error: &Value) -> bool {
        false
    }
}

impl ProviderAdapter for ClaudeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    fn host_launch(&self, input: HostLaunch<'_>) -> Result<ProviderLaunch, WorkerError> {
        let (spec, executable) = claude_launch(
            input.profile,
            input.claude_bridge_dir,
            input.root,
            input.environment,
        )?;
        Ok(ProviderLaunch {
            spec,
            executable: Some(executable.to_string_lossy().into_owned()),
        })
    }

    fn vm_guest_command(&self, guest_home: &str, agent_id: &str) -> Vec<String> {
        vec![
            "node".into(),
            VM_CLAUDE_BRIDGE.into(),
            format!("{guest_home}/.codex/studio-bridges/{agent_id}"),
        ]
    }

    fn profile_dir(&self) -> &'static str {
        ".claude"
    }

    fn guest_credential_file(&self) -> &'static str {
        ".credentials.json"
    }

    fn classify_error(&self, error: &Value) -> Option<UsageCause> {
        static AUTH: std::sync::OnceLock<regex::Regex> = std::sync::OnceLock::new();
        let auth = AUTH.get_or_init(|| {
            regex::Regex::new(r"(?i)oauth session expired|failed to authenticate")
                .expect("static Claude auth pattern")
        });
        classify_usage_failure(error, Some(auth))
    }

    fn is_busy_error(&self, error: &Value) -> bool {
        *error == serde_json::json!({"code": CLAUDE_BUSY_CODE, "message": CLAUDE_BUSY_MESSAGE})
    }
}

fn refused(code: &str) -> WorkerError {
    WorkerError::PreparationRefused(code.into())
}

fn claude_launch(
    profile: &Value,
    bridge_dir: &Path,
    root: PathBuf,
    environment: BTreeMap<String, String>,
) -> Result<(ProviderLaunchSpec, PathBuf), WorkerError> {
    let text = |key: &str| profile.get(key).and_then(Value::as_str);
    let account_id = text("accountId")
        .filter(|value| value.starts_with("claude:"))
        .ok_or_else(|| refused("claude/account-identity"))?;
    let email = text("email")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| refused("claude/account-identity"))?;
    let credential_identity =
        text("_credentialIdentity").ok_or_else(|| refused("claude/account-identity"))?;
    if account_id != format!("claude:{email}") || credential_identity != account_id {
        return Err(refused("claude/account-identity"));
    }
    let options = profile.get("claudeOptions").unwrap_or(&Value::Null);
    let path = environment.get("PATH").map(String::as_str);
    let home = environment.get("HOME").map(PathBuf::from);
    let node = if let Some(configured) = environment.get("STUDIO_NODE_BIN") {
        executable(configured, path)
    } else {
        executable("node", path).or_else(|| {
            [
                PathBuf::from("/opt/homebrew/bin/node"),
                PathBuf::from("/usr/local/bin/node"),
            ]
            .into_iter()
            .chain(home.iter().flat_map(|home| {
                [
                    home.join(".local/share/fnm/aliases/default/bin/node"),
                    home.join("Library/Application Support/fnm/aliases/default/bin/node"),
                    home.join(".volta/bin/node"),
                    home.join(".local/share/mise/shims/node"),
                ]
            }))
            .find_map(|path| executable(&path.to_string_lossy(), None))
        })
    }
    .ok_or_else(|| refused("claude/node-unavailable"))?;
    let binary_path = options
        .get("binaryPath")
        .and_then(Value::as_str)
        .filter(|value| !value.trim().is_empty())
        .or_else(|| environment.get("STUDIO_CLAUDE_BIN").map(String::as_str));
    let claude = match binary_path {
        Some(binary) => executable(binary, path),
        None => executable("claude", path).or_else(|| {
            home.as_ref()
                .map(|home| home.join(".local/bin/claude"))
                .and_then(|path| executable(&path.to_string_lossy(), None))
        }),
    }
    .ok_or_else(|| refused("claude/cli-unavailable"))?;
    let metadata = AuthMetadata {
        status: AuthStatus::Ready,
        account_id: Some(account_id.to_owned()),
        email: Some(email.to_owned()),
        plan: text("plan").map(str::to_owned),
        credential_identity: Some(credential_identity.to_owned()),
        error: None,
        error_kind: None,
    };
    let resolved = ResolvedBridge {
        node,
        executable: claude.clone(),
        bridge: bridge_dir.join("bridge.mjs"),
        root,
        sdk_present: bridge_dir
            .join("node_modules/@anthropic-ai/claude-agent-sdk/package.json")
            .is_file(),
    };
    let launch = BridgeSnapshot::from_record(profile.clone())
        .and_then(|snapshot| snapshot.launch(resolved, &metadata, environment))
        .map_err(|error| WorkerError::PreparationRefused(error.to_string()))?;
    Ok((launch, claude))
}

/// Resolve `configured` (a path, or a name searched in `path`) to a canonical executable.
fn executable(configured: &str, path: Option<&str>) -> Option<PathBuf> {
    let candidate = PathBuf::from(configured);
    let candidate = if candidate.is_absolute() || candidate.components().count() > 1 {
        candidate
    } else {
        env::split_paths(path?)
            .map(|directory| directory.join(&candidate))
            .find(|path| is_executable(path))?
    };
    is_executable(&candidate)
        .then(|| candidate.canonicalize().ok())
        .flatten()
}

/// A regular file with an execute bit (any file off Unix).
pub fn is_executable(path: &Path) -> bool {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
    }
    #[cfg(not(unix))]
    {
        path.is_file()
    }
}

#[cfg(test)]
mod tests {
    use super::{ProviderKind, UsageCause, adapter};
    use serde_json::json;

    #[test]
    fn classifiers_share_the_core_and_differ_only_in_provider_evidence() {
        let shared = json!({"message": "HTTP 401 from service"});
        let claude_only = json!({"message": "OAuth session expired"});
        let busy = json!({"code": -32000, "message": "Claude is still working"});
        for kind in ProviderKind::ALL {
            assert_eq!(adapter(kind).kind(), kind);
            assert_eq!(
                adapter(kind).classify_error(&shared),
                Some(UsageCause::Auth)
            );
        }
        let codex = adapter(ProviderKind::Codex);
        let claude = adapter(ProviderKind::Claude);
        assert_eq!(codex.classify_error(&claude_only), None);
        assert_eq!(claude.classify_error(&claude_only), Some(UsageCause::Auth));
        assert!(!codex.is_busy_error(&busy));
        assert!(claude.is_busy_error(&busy));
    }
}
