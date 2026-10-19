//! One provider adapter per `ProviderKind`.
//!
//! The JSON-RPC wire (`thread/*`, `turn/*`, `model/list`, `account/read`) is shared
//! by both peers and stays in the account worker. An adapter owns only what sits
//! beside that wire: how the peer process is launched and the provider-specific
//! decisions that callers used to branch on with provider strings.

use crate::account_driver::auth_cache::CredentialLocation;
use crate::account_driver::{
    AccountStatus, AuthMetadata, AuthProfile, AuthStatus, account_server_root,
};
use crate::account_worker::WorkerError;
use crate::claude_bridge::{BridgeSnapshot, ResolvedBridge};
use crate::continuation_runtime::classify_usage_failure;
use crate::thread_prepare::{Snapshot, TurnInput};
use crate::usage_resume::Cause as UsageCause;
use serde_json::{Map, Value, json};
use std::{
    collections::BTreeMap,
    env,
    path::{Path, PathBuf},
};
use studio_runtime_process::ProviderLaunchSpec;
use studio_runtime_providers::Method;
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
    /// Registry key of the account that the session serves.
    pub account: &'a str,
    /// The state root.
    pub root: PathBuf,
    pub environment: BTreeMap<String, String>,
}

/// A launch spec and the provider executable that the launched session runs.
pub struct ProviderLaunch {
    pub spec: ProviderLaunchSpec,
    pub executable: Option<String>,
}

/// Closed, typed provider capabilities that preparation reads instead of
/// testing for a provider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capabilities {
    /// The provider writes a native rollout per thread that Studio can read
    /// and repair; otherwise history is the live transcript only.
    pub native_history: bool,
    /// `dynamicTools` is resent on every resume and turn, not only on thread start.
    pub tools_on_every_turn: bool,
    /// `orchestration_speak` and `orchestration_review` are offered.
    pub voice_and_review_tools: bool,
    /// Daybreak cyber-access grants apply.
    pub daybreak: bool,
    /// An unready image workspace gets a writable temporary sandbox root
    /// (otherwise it runs read-only).
    pub writable_sandbox_roots: bool,
}

pub trait ProviderAdapter: Send + Sync + 'static {
    fn kind(&self) -> ProviderKind;

    fn capabilities(&self) -> Capabilities;

    /// Provider-specific parts of `thread/start` and `thread/resume` params.
    fn shape_thread_params(
        &self,
        s: &Snapshot,
        params: &mut Map<String, Value>,
        instructions: &mut String,
    );

    /// Provider-specific parts of the `thread/start` params of an ephemeral title thread.
    fn shape_title_thread_params(&self, params: &mut Map<String, Value>);

    /// Native request that reports the provider session's loaded state for diagnostics.
    fn diagnostics_method(&self) -> Method;

    /// Provider-specific parts of `turn/start` params.
    fn shape_turn_params(&self, input: &TurnInput, params: &mut Map<String, Value>);

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

    /// The model id or alias belongs to this provider's model family.
    fn owns_model(&self, model: &str) -> bool;

    /// Model of a new agent when nothing selects one.
    fn default_model(&self) -> &'static str;

    /// Vendor key of this provider's models in the pricing catalog.
    fn pricing_vendor(&self) -> &'static str;

    /// Auth status of one account profile from the provider's local source,
    /// through the shared per-account status cache. It never mutates the registry.
    fn read_auth(&self, profile: &AuthProfile) -> AuthMetadata;

    /// Credential files whose change ends the cached status of `profile`.
    fn credential_location(&self, profile: &AuthProfile) -> CredentialLocation;

    /// The account's cached auth status holds a turn start before any provider write.
    fn auth_blocks_start(&self, status: AccountStatus) -> bool;

    /// Hold reason of a start blocked because the account is not signed in.
    fn sign_in_message(&self) -> &'static str;

    /// Hold reason of a start whose account identity changed under the session.
    fn account_changed_message(&self) -> &'static str;
}

/// The agent's provider keeps a native rollout history. An unknown provider
/// does not: its history is never read or repaired.
#[must_use]
pub fn native_history(agent: &Value) -> bool {
    ProviderKind::of_record(agent).is_ok_and(|kind| adapter(kind).capabilities().native_history)
}

/// The provider whose model family contains `model`, if any.
#[must_use]
pub fn model_provider(model: &str) -> Option<ProviderKind> {
    ProviderKind::ALL
        .into_iter()
        .find(|kind| adapter(*kind).owns_model(model))
}

/// Default model of a provider.
#[must_use]
pub fn default_model(provider: ProviderKind) -> &'static str {
    adapter(provider).default_model()
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

/// Codex CLI looked up in `PATH` when no executable is selected.
const CODEX_EXECUTABLE: &str = "codex";
/// Codex app-server peer over stdio.
const CODEX_APP_SERVER_ARGS: [&str; 3] = ["app-server", "--listen", "stdio://"];
/// Names of the MCP servers that the Codex home's `config.toml` defines. The
/// app-server merges per-thread `config` into that file, so a server can only
/// be switched off by name (`enabled = false`), never by an empty table.
#[must_use]
pub fn codex_user_mcp_servers(home: &str) -> Vec<String> {
    std::fs::read_to_string(Path::new(home).join("config.toml"))
        .ok()
        .and_then(|text| text.parse::<toml::Table>().ok())
        .and_then(|config| match config.get("mcp_servers") {
            Some(toml::Value::Table(servers)) => Some(servers.keys().cloned().collect()),
            _ => None,
        })
        .unwrap_or_default()
}

/// Codex config override that keeps credentials in the account's own home.
const CODEX_FILE_CREDENTIALS: [&str; 2] = ["-c", "cli_auth_credentials_store=\"file\""];
/// Developer instructions added to every Claude session.
const CLAUDE_SESSION_INSTRUCTIONS: &str = "\nThis session uses Claude Code and its native tools. The native Agent tool is off. Studio managed agents replace it, and native agent type lists do not apply. Use Studio command monitors for long-running commands that need output, input, or cancellation. Use Bash for other commands. Studio voice is unavailable. Do not tell the user that an MCP server or connector needs authentication unless the user asks for work that needs it.\n";
/// JSON-RPC error the Claude bridge returns for a request on a busy thread.
const CLAUDE_BUSY_CODE: i64 = -32000;
const CLAUDE_BUSY_MESSAGE: &str = "Claude is still working";
/// Claude Code's own default-model selector.
const CLAUDE_DEFAULT_MODEL: &str = "default";
/// Claude Code model aliases (besides `claude-*` ids).
const CLAUDE_MODEL_ALIASES: [&str; 4] = [CLAUDE_DEFAULT_MODEL, "sonnet", "opus", "haiku"];
/// Guest path of the Claude bridge inside the VM image.
const VM_CLAUDE_BRIDGE: &str = "/opt/codex-studio/claude_bridge/bridge.mjs";

impl ProviderAdapter for CodexAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Codex
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_history: true,
            tools_on_every_turn: false,
            voice_and_review_tools: true,
            daybreak: true,
            writable_sandbox_roots: true,
        }
    }

    fn shape_thread_params(&self, _: &Snapshot, _: &mut Map<String, Value>, _: &mut String) {}

    fn shape_title_thread_params(&self, params: &mut Map<String, Value>) {
        params.insert("approvalPolicy".into(), json!("never"));
        params.insert("sandbox".into(), json!("read-only"));
    }

    fn diagnostics_method(&self) -> Method {
        Method::ThreadLoadedList
    }

    fn shape_turn_params(&self, _input: &TurnInput, params: &mut Map<String, Value>) {
        params.insert("cyberAccessProgram".into(), json!("standard"));
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
        let mut command = vec![codex_executable.unwrap_or(CODEX_EXECUTABLE).to_owned()];
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
        let mut command = vec![CODEX_EXECUTABLE.to_owned()];
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

    fn owns_model(&self, model: &str) -> bool {
        model.starts_with("gpt-")
    }

    fn default_model(&self) -> &'static str {
        crate::agents_create::DEFAULT_LEAD_MODEL
    }

    fn pricing_vendor(&self) -> &'static str {
        "openai"
    }

    fn read_auth(&self, profile: &AuthProfile) -> AuthMetadata {
        crate::account_driver::read_codex_auth_cached(profile)
    }

    fn credential_location(&self, profile: &AuthProfile) -> CredentialLocation {
        crate::account_driver::codex_location(profile)
    }

    /// Only a missing or signed-out `auth.json` holds a start: the app-server
    /// refreshes an expired token itself, so any other status may still run.
    fn auth_blocks_start(&self, status: AccountStatus) -> bool {
        status == AccountStatus::SignedOut
    }

    fn sign_in_message(&self) -> &'static str {
        "Sign in to this Codex account first"
    }

    fn account_changed_message(&self) -> &'static str {
        "The Codex account changed during authentication"
    }
}

impl ProviderAdapter for ClaudeAdapter {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Claude
    }

    fn capabilities(&self) -> Capabilities {
        Capabilities {
            native_history: false,
            tools_on_every_turn: true,
            voice_and_review_tools: false,
            daybreak: false,
            writable_sandbox_roots: false,
        }
    }

    fn shape_thread_params(
        &self,
        s: &Snapshot,
        params: &mut Map<String, Value>,
        instructions: &mut String,
    ) {
        instructions.push_str(CLAUDE_SESSION_INSTRUCTIONS);
        params.insert("claude".into(), json!(s.settings.claude_options));
        // A managed worker (a child agent) runs without the user's MCP servers.
        if s.actor.parent_id.is_some() {
            params.insert("userMcp".into(), json!(false));
        }
    }

    fn shape_title_thread_params(&self, _: &mut Map<String, Value>) {}

    fn diagnostics_method(&self) -> Method {
        Method::ClaudeDiagnostics
    }

    /// A single-line `/` or `$` user message is passed to Claude Code as a command.
    fn shape_turn_params(&self, input: &TurnInput, params: &mut Map<String, Value>) {
        if let [event] = input.events.as_slice()
            && event.kind == "user"
            && !input.has_assets
            && !event.text.contains('\n')
        {
            let command = crate::event_write::python_strip(&event.text);
            if command.starts_with(['/', '$']) {
                params.insert("claudeCommand".into(), json!(command));
            }
        }
    }

    fn host_launch(&self, input: HostLaunch<'_>) -> Result<ProviderLaunch, WorkerError> {
        // The bridge runs in the account's server directory, as in Python, because
        // its session snapshots there let an agent that Python started resume.
        let (spec, executable) = claude_launch(
            input.profile,
            input.claude_bridge_dir,
            account_server_root(&input.root, input.account),
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

    fn owns_model(&self, model: &str) -> bool {
        model.starts_with("claude-") || CLAUDE_MODEL_ALIASES.contains(&model)
    }

    fn default_model(&self) -> &'static str {
        CLAUDE_DEFAULT_MODEL
    }

    fn pricing_vendor(&self) -> &'static str {
        "anthropic"
    }

    fn read_auth(&self, profile: &AuthProfile) -> AuthMetadata {
        crate::account_driver::read_claude_auth_cached(profile)
    }

    fn credential_location(&self, profile: &AuthProfile) -> CredentialLocation {
        crate::account_driver::claude_location(profile)
    }

    fn auth_blocks_start(&self, status: AccountStatus) -> bool {
        status != AccountStatus::Ready
    }

    fn sign_in_message(&self) -> &'static str {
        "Sign in with claude auth login first"
    }

    fn account_changed_message(&self) -> &'static str {
        "The Claude account changed during authentication"
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
    // The bridge's working directory is the account's server root. A profile added
    // after startup has none yet, and a spawn in a missing cwd fails with ENOENT.
    std::fs::create_dir_all(&root).map_err(|_| refused("claude/account-server-root"))?;
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

    #[test]
    fn model_families_and_defaults_belong_to_one_provider() {
        use super::{default_model, model_provider};
        for (model, owner) in [
            ("gpt-6-astra", Some(ProviderKind::Codex)),
            ("gpt-6-luna", Some(ProviderKind::Codex)),
            ("gpt-5.6-luna", Some(ProviderKind::Codex)),
            ("claude-sonnet-4", Some(ProviderKind::Claude)),
            ("default", Some(ProviderKind::Claude)),
            ("haiku", Some(ProviderKind::Claude)),
            ("o3", None),
        ] {
            assert_eq!(model_provider(model), owner, "{model}");
        }
        for kind in ProviderKind::ALL {
            let default = adapter(kind).default_model();
            assert_eq!(model_provider(default), Some(kind));
            assert_eq!(super::default_model(kind), default);
        }
        assert_eq!(default_model(ProviderKind::Claude), "default");
        assert_eq!(default_model(ProviderKind::Codex), "gpt-6-astra");
    }

    #[test]
    fn auth_sources_belong_to_the_profile_provider() {
        use crate::account_driver::{AuthProfile, AuthStatus};
        let home = tempfile::tempdir().expect("home");
        let profile = |provider| AuthProfile {
            provider,
            home: home.path().to_path_buf(),
            claude_options: Some(serde_json::json!({"configDir": home.path()})),
        };
        let codex = adapter(ProviderKind::Codex).credential_location(&profile(ProviderKind::Codex));
        assert_eq!(codex.directories, vec![home.path().to_path_buf()]);
        assert_eq!(codex.credential_files, ["auth.json"]);
        let claude =
            adapter(ProviderKind::Claude).credential_location(&profile(ProviderKind::Claude));
        assert_eq!(claude.directories, vec![home.path().to_path_buf()]);
        assert!(claude.credential_files.contains(&".credentials.json"));
        assert_ne!(codex.identity, claude.identity);
        assert_eq!(
            adapter(ProviderKind::Codex)
                .read_auth(&profile(ProviderKind::Codex))
                .status,
            AuthStatus::SignedOut
        );
    }

    /// A profile added after startup has no account server directory yet; the
    /// bridge launch creates it because the bridge spawns with it as its cwd.
    #[cfg(unix)]
    #[test]
    fn claude_launch_creates_a_missing_account_server_root() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("temp dir");
        let binary = dir.path().join("claude");
        std::fs::write(&binary, "#!/bin/sh\n").expect("fake claude");
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o700))
            .expect("make executable");
        let sdk = dir
            .path()
            .join("node_modules/@anthropic-ai/claude-agent-sdk");
        std::fs::create_dir_all(&sdk).expect("sdk dir");
        std::fs::write(sdk.join("package.json"), "{}").expect("sdk package");
        let profile = json!({
            "id": "added", "provider": "claude", "status": "ready", "home": dir.path(), "email": "a@example.test",
            "accountId": "claude:a@example.test",
            "_credentialIdentity": "claude:a@example.test",
            "claudeOptions": {"binaryPath": binary, "configDir": dir.path(), "customModels": []}
        });
        let root = dir.path().join("account-servers/added");
        let environment = std::collections::BTreeMap::from([(
            "STUDIO_NODE_BIN".to_owned(),
            binary.to_string_lossy().into_owned(),
        )]);
        super::claude_launch(&profile, dir.path(), root.clone(), environment).expect("launch spec");
        assert!(root.is_dir());
    }
}
