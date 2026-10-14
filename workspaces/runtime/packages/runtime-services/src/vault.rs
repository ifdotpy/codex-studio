//! Host-side project vault records and setup facts. Guest RPC and the local Desk executable are
//! separate effect boundaries; paper words and private key material are never read here.

use serde_json::{Map, Value, json};
use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::Duration;
use thiserror::Error;

const DEFAULT_VAULT_REGION: &str = "eu-central-1";
pub const DESK_COMMAND_TIMEOUT: Duration = Duration::from_secs(60);
pub const VAULT_LOGIN_TIMEOUT: Duration = Duration::from_secs(900);
pub const VAULT_SETUP_TIMEOUT: Duration = Duration::from_secs(3600);
const DESK_OUTPUT_LIMIT: usize = 32 * 1024;

/// Resolve the Linux VM data directory used by Python's `codex_linux_vm.Client.state_dir`.
pub fn linux_vm_state_dir(runtime_state_dir: &Path) -> PathBuf {
    std::env::var_os("CODEX_LINUX_VM_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| runtime_state_dir.join("linux-vm"))
}

#[derive(Debug, Error)]
pub enum VaultError {
    #[error("invalid project id")]
    InvalidProjectId,
    #[error("The project has a vault already")]
    AlreadyConfigured,
    #[error("The project has no Mac folder")]
    ProjectSourceMissing,
    #[error("A vault name has up to 44 letters, digits, - and _, and is not api or assets")]
    InvalidPrefix,
    #[error("The project vault record is invalid")]
    InvalidStoredRecord(&'static str),
    #[error("The Desk site must be an https URL")]
    InvalidSite,
    #[error("The layr-desk command is not installed (cargo install --path crates/layr-desk)")]
    DeskMissing,
    #[error("layr-desk command timed out")]
    DeskTimedOut,
    #[error("layr-desk command failed")]
    DeskFailed,
    #[error("Make the paper key of the vault first")]
    MissingRecipient,
    #[error("Name an AWS profile")]
    InvalidProfile,
    #[error("Name an AWS profile and a region")]
    InvalidProfileOrRegion,
}

#[derive(Debug)]
pub struct DeskResult {
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
    pub timed_out: bool,
}

/// Host-local resources already resolved by runtime-server's HostRuntime.
#[derive(Clone, Debug)]
pub struct VaultHostCommand {
    python: PathBuf,
    server_source: PathBuf,
}

impl VaultHostCommand {
    pub fn new(python: PathBuf, server_source: PathBuf) -> Self {
        Self {
            python,
            server_source,
        }
    }

    pub fn writer(&self, source: &str) -> String {
        writer_command(
            &self.python,
            &self.server_source.join("codex_vault.py"),
            source,
        )
    }
}

/// Run the host's layr-desk command through runtime-process's owned, bounded child.
pub async fn run_desk<F>(
    args: &[String],
    timeout: Duration,
    on_output: &mut F,
) -> Result<DeskResult, VaultError>
where
    F: FnMut(bool, &str) + Send,
{
    let environment: BTreeMap<OsString, OsString> = std::env::vars_os().collect();
    run_desk_with_environment(args, timeout, &environment, on_output).await
}

async fn run_desk_with_environment<F>(
    args: &[String],
    timeout: Duration,
    environment: &BTreeMap<OsString, OsString>,
    on_output: &mut F,
) -> Result<DeskResult, VaultError>
where
    F: FnMut(bool, &str) + Send,
{
    use studio_runtime_process::stream_child::{
        ChildEvent, ChildSpec, ExitReason, OutputStream, StreamChild,
    };

    let program = resolve_desk_program(environment)?;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let mut child = StreamChild::spawn(ChildSpec {
        program,
        args: args.iter().map(OsString::from).collect(),
        cwd,
        environment: environment.clone(),
        timeout,
        inherited_lease: None,
    })
    .await
    .map_err(|_| VaultError::DeskFailed)?;
    child
        .write(Vec::new(), true)
        .await
        .map_err(|_| VaultError::DeskFailed)?;
    let mut stdout = String::new();
    let mut stderr = String::new();
    let mut code = None;
    let mut timed_out = false;
    while let Some(event) = child.next_event().await {
        match event {
            ChildEvent::Output { stream, bytes } => {
                let is_stdout = stream == OutputStream::Stdout;
                let text = String::from_utf8_lossy(&bytes);
                on_output(is_stdout, &text);
                let output = if is_stdout { &mut stdout } else { &mut stderr };
                output.push_str(&text);
                if output.len() > DESK_OUTPUT_LIMIT {
                    let excess = output.len() - DESK_OUTPUT_LIMIT;
                    let boundary = output
                        .char_indices()
                        .find_map(|(index, _)| (index >= excess).then_some(index))
                        .unwrap_or(output.len());
                    output.drain(..boundary);
                }
            }
            ChildEvent::Exited {
                code: exit_code,
                reason,
            } => {
                code = exit_code;
                timed_out = reason == ExitReason::TimedOut;
            }
        }
    }
    child.close().await.map_err(|_| VaultError::DeskFailed)?;
    Ok(DeskResult {
        code,
        stdout,
        stderr,
        timed_out,
    })
}

fn resolve_desk_program(environment: &BTreeMap<OsString, OsString>) -> Result<PathBuf, VaultError> {
    if let Some(program) = environment.get(OsStr::new("STUDIO_LAYR_DESK_BIN")) {
        return Ok(PathBuf::from(program));
    }
    if let Some(program) = bundled_desk_program()
        && program.is_file()
    {
        return Ok(program);
    }
    if let Some(path) = environment.get(OsStr::new("PATH")) {
        for folder in std::env::split_paths(path) {
            let candidate = folder.join("layr-desk");
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }
    if let Some(home) = environment.get(OsStr::new("HOME")) {
        let candidate = PathBuf::from(home).join(".cargo/bin/layr-desk");
        if candidate.is_file() {
            return Ok(candidate);
        }
    }
    Err(VaultError::DeskMissing)
}

fn bundled_desk_program() -> Option<PathBuf> {
    let executable = std::env::current_exe().ok()?;
    for ancestor in executable.ancestors() {
        let root = if ancestor.file_name().is_some_and(|name| name == "bin") {
            ancestor.parent().unwrap_or(ancestor)
        } else {
            ancestor
        };
        let scripts = root.join("scripts");
        if scripts.join("codex_vm_vault.py").is_file() {
            return Some(root.join("bin/layr-desk"));
        }
        let source = root.join("workspaces/runtime/apps/server/src");
        if source.join("codex_vm_vault.py").is_file() {
            return Some(root.join("workspaces/runtime/apps/server/bin/layr-desk"));
        }
    }
    None
}

/// Read the cached writer report without contacting the VM.
pub fn view(state_dir: &Path, project_id: &str) -> Result<Option<Value>, VaultError> {
    let folder = project_folder(state_dir, project_id)?;
    let status = match load_view_object(&folder.join("vault.json")) {
        Ok(status) => status,
        Err(_) => return Ok(Some(unreadable_view(project_id))),
    };
    let site_record = match load_view_object(&folder.join("vault-site.json")) {
        Ok(record) => record,
        Err(_) => return Ok(Some(unreadable_view(project_id))),
    };
    if status.is_empty() {
        return Ok(None);
    }
    if status.get("configured").and_then(Value::as_bool) != Some(true) {
        return Ok(Some(json!({"projectId":project_id,"state":"none"})));
    }
    let prefix = status
        .get("prefix")
        .and_then(Value::as_str)
        .ok_or(VaultError::InvalidStoredRecord("vault.json"))?;
    if !valid_prefix(prefix) {
        return Err(VaultError::InvalidStoredRecord("vault.json"));
    }
    let last_error = status
        .get("lastError")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty());
    let mut result = Map::from_iter([
        ("projectId".to_owned(), Value::String(project_id.to_owned())),
        (
            "state".to_owned(),
            Value::String(if last_error.is_some() { "error" } else { "ok" }.to_owned()),
        ),
        ("prefix".to_owned(), Value::String(prefix.to_owned())),
    ]);
    if let Some(last_push) = status.get("lastPush").filter(|value| !value.is_null()) {
        result.insert("lastPush".to_owned(), last_push.clone());
    }
    if let Some(reason) = last_error {
        result.insert(
            "reason".to_owned(),
            Value::String(reason.chars().take(300).collect()),
        );
    }
    if let Some(site) = site_record.get("site").and_then(Value::as_str) {
        result.insert("site".to_owned(), Value::String(site.to_owned()));
    }
    let mac = load_view_object(&folder.join("vault-mac.json")).unwrap_or_default();
    if let Some(unlocked) = mac.get("unlocked").and_then(Value::as_bool) {
        result.insert("unlocked".to_owned(), Value::Bool(unlocked));
    }
    if let Some(disk) = mac.get("disk").and_then(Value::as_str) {
        result.insert("disk".to_owned(), Value::String(disk.to_owned()));
    }
    Ok(Some(Value::Object(result)))
}

fn unreadable_view(project_id: &str) -> Value {
    json!({
        "projectId": project_id,
        "state": "error",
        "reason": "The vault records are unreadable"
    })
}

fn load_view_object(path: &Path) -> Result<Map<String, Value>, ()> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Map::new()),
        Err(_) => return Err(()),
    };
    let value = serde_json::from_slice::<Value>(&bytes).map_err(|_| ())?;
    Ok(value.as_object().cloned().unwrap_or_default())
}

/// Store the Desk site after validating and normalizing its scheme and trailing slash.
pub fn store_site(state_dir: &Path, project_id: &str, site: &str) -> Result<(), VaultError> {
    if !site.starts_with("https://") || site.chars().any(char::is_whitespace) {
        return Err(VaultError::InvalidSite);
    }
    let folder = project_folder(state_dir, project_id)?;
    fs::create_dir_all(&folder).map_err(|_| VaultError::InvalidStoredRecord("vault-site.json"))?;
    let normalized = format!("{}/", site.trim_end_matches('/'));
    write_json(&folder.join("vault-site.json"), &json!({"site":normalized}))
}

/// Setup-dialog facts derived from the imported project and local Desk settings.
/// `profiles` is the parsed `layr-desk aws-profiles --json` result.
pub fn setup_facts(
    state_dir: &Path,
    settings_dir: &Path,
    project_id: &str,
    requested_prefix: Option<&str>,
    profiles: &Value,
) -> Result<Value, VaultError> {
    let prefix = prepare_setup_facts(state_dir, project_id, requested_prefix)?;
    let gateway = load_optional_object(&settings_dir.join("gateway.json"), "gateway.json")?;
    let site = gateway
        .get("host")
        .and_then(Value::as_str)
        .map(|host| format!("https://{host}/"));
    let region = gateway
        .get("region")
        .and_then(Value::as_str)
        .unwrap_or(DEFAULT_VAULT_REGION);
    let gateway_profile = gateway.get("profile").and_then(Value::as_str);
    let profiles = profiles
        .get("profiles")
        .and_then(Value::as_array)
        .ok_or(VaultError::InvalidStoredRecord("aws-profiles"))?;
    Ok(json!({
        "prefix": prefix,
        "keys": settings_dir.join(&prefix).join("recipient.txt").is_file(),
        "site": site,
        "region": region,
        "gatewayProfile": gateway_profile,
        "profiles": profiles,
    }))
}

/// Validate the project and selected prefix before starting the host command.
pub fn prepare_setup_facts(
    state_dir: &Path,
    project_id: &str,
    requested_prefix: Option<&str>,
) -> Result<String, VaultError> {
    if view(state_dir, project_id)?
        .is_some_and(|view| view.get("state").and_then(Value::as_str) != Some("none"))
    {
        return Err(VaultError::AlreadyConfigured);
    }
    let folder = project_folder(state_dir, project_id)?;
    let project = load_object(&folder.join("project.json"), "project.json")?;
    let source = project
        .get("source")
        .and_then(Value::as_str)
        .filter(|source| !source.is_empty())
        .ok_or(VaultError::ProjectSourceMissing)?;
    let prefix = requested_prefix
        .filter(|prefix| !prefix.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| suggested_prefix(source));
    if !valid_prefix(&prefix) || matches!(prefix.as_str(), "api" | "assets") {
        return Err(VaultError::InvalidPrefix);
    }
    Ok(prefix)
}

/// Validate a new vault request and return the source folder and selected prefix.
pub fn prepare_new_vault(
    state_dir: &Path,
    settings_dir: &Path,
    project_id: &str,
    requested_prefix: Option<&str>,
    profile: &str,
    region: &str,
) -> Result<(String, String), VaultError> {
    if !valid_profile(profile) || !valid_region(region) {
        return Err(VaultError::InvalidProfileOrRegion);
    }
    if view(state_dir, project_id)?
        .is_some_and(|view| view.get("state").and_then(Value::as_str) != Some("none"))
    {
        return Err(VaultError::AlreadyConfigured);
    }
    let folder = project_folder(state_dir, project_id)?;
    let project = load_object(&folder.join("project.json"), "project.json")?;
    let source = project
        .get("source")
        .and_then(Value::as_str)
        .filter(|source| !source.is_empty())
        .ok_or(VaultError::ProjectSourceMissing)?
        .to_owned();
    let prefix = requested_prefix
        .filter(|prefix| !prefix.is_empty())
        .map(str::to_owned)
        .unwrap_or_else(|| suggested_prefix(&source));
    if !valid_prefix(&prefix) || matches!(prefix.as_str(), "api" | "assets") {
        return Err(VaultError::InvalidPrefix);
    }
    if !settings_dir.join(&prefix).join("recipient.txt").is_file() {
        return Err(VaultError::MissingRecipient);
    }
    Ok((source, prefix))
}

pub fn valid_profile(profile: &str) -> bool {
    let mut chars = profile.chars();
    chars
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric())
        && profile.len() <= 128
        && chars.all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '.' | '@' | '+' | '-')
        })
}

pub fn valid_region(region: &str) -> bool {
    let mut parts = region.split('-');
    let Some(prefix) = parts.next() else {
        return false;
    };
    let Some(last) = region.rsplit('-').next() else {
        return false;
    };
    prefix.len() == 2
        && prefix.bytes().all(|byte| byte.is_ascii_lowercase())
        && last.len() == 1
        && last.bytes().all(|byte| byte.is_ascii_digit())
        && parts.count() >= 2
        && region
            .split('-')
            .skip(1)
            .take_while(|part| *part != last)
            .all(|part| !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_lowercase()))
}

/// Build the same writer command string Python gives to `layr-desk deploy`.
pub fn writer_command(python: &Path, script: &Path, source: &str) -> String {
    [
        python.as_os_str().to_string_lossy().into_owned(),
        script.as_os_str().to_string_lossy().into_owned(),
        "configure".to_owned(),
        source.to_owned(),
    ]
    .iter()
    .map(|argument| shell_quote(argument))
    .collect::<Vec<_>>()
    .join(" ")
}

fn shell_quote(argument: &str) -> String {
    if !argument.is_empty()
        && argument
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&byte))
    {
        argument.to_owned()
    } else {
        format!("'{}'", argument.replace('\'', "'\\''"))
    }
}

pub fn suggested_prefix(source: &str) -> String {
    let name = Path::new(source)
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let mut output = String::with_capacity(name.len().min(44));
    let mut separator = false;
    for character in name.chars() {
        if character.is_ascii_lowercase()
            || character.is_ascii_digit()
            || matches!(character, '-' | '_')
        {
            if output.len() < 44 {
                output.push(character);
                separator = false;
            }
        } else if !output.is_empty() && !separator && output.len() < 44 {
            output.push('-');
            separator = true;
        }
    }
    let trimmed = output.trim_matches(['-', '_']);
    if trimmed.is_empty() {
        "project".to_owned()
    } else {
        trimmed.to_owned()
    }
}

pub fn valid_prefix(prefix: &str) -> bool {
    let bytes = prefix.as_bytes();
    (1..=44).contains(&bytes.len())
        && bytes[0].is_ascii_alphanumeric()
        && bytes
            .iter()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn project_folder(state_dir: &Path, project_id: &str) -> Result<PathBuf, VaultError> {
    let mut components = Path::new(project_id).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(VaultError::InvalidProjectId);
    }
    Ok(state_dir.join("layr-projects").join(project_id))
}

fn load_object(path: &Path, object: &'static str) -> Result<Map<String, Value>, VaultError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or(VaultError::InvalidStoredRecord(object)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(_) => Err(VaultError::InvalidStoredRecord(object)),
    }
}

fn load_optional_object(
    path: &Path,
    object: &'static str,
) -> Result<Map<String, Value>, VaultError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
            .ok()
            .and_then(|value| value.as_object().cloned())
            .ok_or(VaultError::InvalidStoredRecord(object)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Map::new()),
        Err(_) => Err(VaultError::InvalidStoredRecord(object)),
    }
}

fn write_json(path: &Path, value: &Value) -> Result<(), VaultError> {
    let temporary = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec(value)
        .map_err(|_| VaultError::InvalidStoredRecord("vault-site.json"))?;
    fs::write(&temporary, bytes).map_err(|_| VaultError::InvalidStoredRecord("vault-site.json"))?;
    fs::rename(&temporary, path).map_err(|_| VaultError::InvalidStoredRecord("vault-site.json"))
}

#[cfg(test)]
mod tests {
    use super::{
        VaultError, run_desk_with_environment, setup_facts, store_site, suggested_prefix,
        valid_prefix, valid_profile, valid_region, view, writer_command,
    };
    use serde_json::{Value, json};
    use std::collections::BTreeMap;
    use std::ffi::OsString;
    use std::fs;

    fn state() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        let project = root.path().join("layr-projects/p1");
        fs::create_dir_all(&project).unwrap();
        fs::write(
            project.join("project.json"),
            br#"{"projectId":"p1","source":"/tmp/Project One"}"#,
        )
        .unwrap();
        root
    }

    #[test]
    fn view_reads_python_shaped_records_without_exposing_other_fields() {
        let root = state();
        let project = root.path().join("layr-projects/p1");
        fs::write(project.join("vault.json"), br#"{"configured":true,"prefix":"project","lastPush":"2026-10-14T00:00:00Z","accessKeyId":"do-not-return"}"#).unwrap();
        fs::write(
            project.join("vault-site.json"),
            br#"{"site":"https://desk.example/"}"#,
        )
        .unwrap();
        fs::write(
            project.join("vault-mac.json"),
            br#"{"unlocked":true,"disk":"/Volumes/Desk/project"}"#,
        )
        .unwrap();

        let result = view(root.path(), "p1").unwrap().unwrap();

        assert_eq!(
            result,
            json!({"projectId":"p1","state":"ok","prefix":"project","lastPush":"2026-10-14T00:00:00Z","site":"https://desk.example/","unlocked":true,"disk":"/Volumes/Desk/project"})
        );
        assert!(!result.to_string().contains("do-not-return"));
    }

    #[test]
    fn view_preserves_python_epoch_timestamp_values() {
        let root = state();
        let project = root.path().join("layr-projects/p1");
        fs::write(
            project.join("vault.json"),
            br#"{"configured":true,"prefix":"project","lastPush":1791720000}"#,
        )
        .unwrap();

        let result = view(root.path(), "p1").unwrap().unwrap();

        assert_eq!(result["lastPush"], 1791720000);
    }

    #[test]
    fn view_is_absent_before_a_cached_record_and_reports_unreadable_records() {
        let root = state();
        assert_eq!(view(root.path(), "p1").unwrap(), None);

        let project = root.path().join("layr-projects/p1");
        fs::write(project.join("vault.json"), b"not-json").unwrap();
        assert_eq!(
            view(root.path(), "p1").unwrap(),
            Some(
                json!({"projectId":"p1","state":"error","reason":"The vault records are unreadable"})
            )
        );
        fs::remove_file(project.join("vault.json")).unwrap();
        fs::create_dir(project.join("vault.json")).unwrap();
        let result = view(root.path(), "p1").unwrap().unwrap();

        assert_eq!(
            result,
            json!({"projectId":"p1","state":"error","reason":"The vault records are unreadable"})
        );
    }

    #[test]
    fn setup_facts_resolve_prefix_and_gateway_without_opening_key_material() {
        let root = state();
        let settings = root.path().join("settings");
        fs::create_dir_all(settings.join("project-one")).unwrap();
        fs::write(
            settings.join("gateway.json"),
            br#"{"host":"desk.example","profile":"personal","region":"eu-west-1"}"#,
        )
        .unwrap();
        fs::write(
            settings.join("project-one/recipient.txt"),
            b"generated recipient fixture",
        )
        .unwrap();

        let facts = setup_facts(
            root.path(),
            &settings,
            "p1",
            None,
            &json!({"profiles":[{"name":"personal"}]}),
        )
        .unwrap();

        assert_eq!(facts["prefix"], "project-one");
        assert_eq!(facts["keys"], true);
        assert_eq!(facts["site"], "https://desk.example/");
        assert_eq!(facts["region"], "eu-west-1");
        assert_eq!(facts["gatewayProfile"], "personal");
    }

    #[test]
    fn invalid_prefix_and_path_traversal_fail_closed() {
        let root = state();
        let settings = root.path().join("settings");
        fs::create_dir_all(&settings).unwrap();
        fs::write(settings.join("gateway.json"), b"{}").unwrap();

        assert!(matches!(
            setup_facts(
                root.path(),
                &settings,
                "p1",
                Some("api"),
                &json!({"profiles":[]})
            ),
            Err(VaultError::InvalidPrefix)
        ));
        assert!(matches!(
            view(root.path(), "../p1"),
            Err(VaultError::InvalidProjectId)
        ));
        assert!(valid_prefix("A_1"));
        assert!(!valid_prefix("a b"));
    }

    #[test]
    fn store_site_normalizes_the_slash_and_rejects_non_https() {
        let root = state();

        store_site(root.path(), "p1", "https://desk.example///").unwrap();

        let saved: Value = serde_json::from_slice(
            &fs::read(root.path().join("layr-projects/p1/vault-site.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(saved["site"], "https://desk.example/");
        assert!(matches!(
            store_site(root.path(), "p1", "http://desk.example"),
            Err(VaultError::InvalidSite)
        ));
    }

    #[test]
    fn default_region_has_fast_parity_with_the_python_owner() {
        let python = include_str!("../../../apps/server/src/codex_vm_vault.py");
        assert!(python.contains("else 'eu-central-1'"));
    }

    #[test]
    fn source_folder_suggestion_matches_python_normalization() {
        assert_eq!(suggested_prefix("/work/Project One!!"), "project-one");
        assert_eq!(suggested_prefix("/work/---"), "project");
    }

    #[test]
    fn aws_profile_and_region_validation_matches_python_patterns() {
        assert!(valid_profile("personal+test@example.com"));
        assert!(!valid_profile("-invalid"));
        assert!(valid_region("eu-central-1"));
        assert!(!valid_region("EU-central-1"));
        assert!(!valid_region("us-east"));
    }

    #[test]
    fn writer_command_uses_shell_join_quoting() {
        assert_eq!(
            writer_command(
                std::path::Path::new("/tmp/python3"),
                std::path::Path::new("/tmp/server scripts/codex_vault.py"),
                "/tmp/a'b"
            ),
            "/tmp/python3 '/tmp/server scripts/codex_vault.py' configure '/tmp/a'\\''b'"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn bounded_desk_runner_uses_only_a_fake_executable_from_path() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().unwrap();
        let bin = root.path().join("bin");
        fs::create_dir(&bin).unwrap();
        let fake = bin.join("layr-desk");
        fs::write(
            &fake,
            "#!/bin/sh\n[ \"$1\" = aws-profiles ] && [ \"$2\" = --json ] || exit 91\ncat >/dev/null\nprintf '{\"profiles\":[]}\\n'\n",
        )
        .unwrap();
        fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
        let environment = BTreeMap::from([
            (OsString::from("PATH"), bin.as_os_str().to_owned()),
            (OsString::from("HOME"), root.path().as_os_str().to_owned()),
        ]);
        let args = vec!["aws-profiles".to_owned(), "--json".to_owned()];
        let mut ignore = |_: bool, _: &str| {};

        let result = run_desk_with_environment(
            &args,
            super::DESK_COMMAND_TIMEOUT,
            &environment,
            &mut ignore,
        )
        .await
        .unwrap();

        assert_eq!(result.code, Some(0));
        assert_eq!(
            serde_json::from_str::<Value>(&result.stdout).unwrap(),
            json!({"profiles":[]})
        );
    }
}
