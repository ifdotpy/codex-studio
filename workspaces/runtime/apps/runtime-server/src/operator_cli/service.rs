//! User service definitions. Installation never changes a running generation.
use super::{Result, ServiceLayout, home, service_layout};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    process::{ExitCode, Stdio},
    time::Duration,
};
use studio_runtime_schema::multi_server_protocol::PUBLIC_ORIGIN_ENVIRONMENT;
use studio_runtime_store::path_identity::CanonicalPath;

/// Maximum time to wait for a user service manager, in seconds.
const MANAGER_TIMEOUT: Duration = Duration::from_secs(30);
/// Delay between automatic server restarts, in seconds.
#[cfg(any(target_os = "linux", test))]
const RESTART_DELAY_SECONDS: u64 = 5;
/// Grace allowed for server shutdown, in seconds.
#[cfg(any(target_os = "linux", test))]
const STOP_TIMEOUT_SECONDS: u64 = 60;
/// User-only service configuration file permissions.
const PRIVATE_SERVICE_FILE_MODE: u32 = 0o600;
const SERVICE_MARKER: &str =
    "Managed by studio-runtime-server service; use just deliver-server for releases.";

#[derive(Clone, Copy)]
enum Action {
    Install,
    Status,
    Uninstall,
}

pub(super) fn run(args: &[String]) -> Result<ExitCode> {
    let (action, options) = args
        .split_first()
        .ok_or("service requires install, status, or uninstall")?;
    let action = match action.as_str() {
        "install" => Action::Install,
        "status" => Action::Status,
        "uninstall" => Action::Uninstall,
        _ => return Err("service requires install, status, or uninstall".into()),
    };
    let layout = service_layout()?;
    let home = home()?;
    let mut state = std::env::var_os("CODEX_AGENTS_STATE_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(&layout.state_relative));
    match options {
        [] => (),
        [flag, value] if flag == "--state" => state = PathBuf::from(value),
        _ => return Err("usage: service <install|status|uninstall> [--state DIRECTORY]".into()),
    }
    // Resolve existing prefixes too, so creating a state after installation
    // cannot change its identity when HOME or a parent directory is a symlink.
    let state = CanonicalPath::missing_tail(&state)
        .ok_or("the state directory needs an absolute path with a resolvable parent")?;
    let root = std::env::var_os("CODEX_STUDIO_SERVER_ROOT")
        .map(PathBuf::from)
        .unwrap_or_else(|| home.join(&layout.root_relative));
    if !root.is_absolute() {
        return Err("CODEX_STUDIO_SERVER_ROOT must be absolute".into());
    }
    #[cfg(target_os = "linux")]
    {
        let config = std::env::var_os("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".config"));
        if !config.is_absolute() {
            return Err("XDG_CONFIG_HOME must be absolute".into());
        }
        let file = config.join("systemd/user").join(&layout.unit_name);
        let content = linux_unit(&layout, &root, &state)?;
        let _lock = match action {
            Action::Status => None,
            Action::Install | Action::Uninstall => Some(service_lock(&file)?),
        };
        match action {
            Action::Status => manager("systemctl", &["--user", "status", &layout.unit_name]),
            Action::Install => {
                write_managed(&file, content.as_bytes())?;
                require_success(manager("systemctl", &["--user", "daemon-reload"])?)?;
                require_success(manager(
                    "systemctl",
                    &["--user", "enable", &layout.unit_name],
                )?)?;
                println!(
                    "Installed {}; no service was started or restarted.",
                    file.display()
                );
                Ok(ExitCode::SUCCESS)
            }
            Action::Uninstall => {
                check_managed(&file, content.as_bytes())?;
                if file.exists() {
                    require_success(manager(
                        "systemctl",
                        &["--user", "disable", &layout.unit_name],
                    )?)?;
                    remove_if_present(&file)?;
                }
                require_success(manager("systemctl", &["--user", "daemon-reload"])?)?;
                println!(
                    "Removed {}; running services were left alone.",
                    file.display()
                );
                Ok(ExitCode::SUCCESS)
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        let label = macos_label(&layout, &state);
        let domain = format!("gui/{}/{}", nix::unistd::Uid::current(), label);
        let file = home
            .join("Library/LaunchAgents")
            .join(format!("{label}.plist"));
        let content = macos_plist(&layout, &root, &state, &label)?;
        let _lock = match action {
            Action::Status => None,
            Action::Install | Action::Uninstall => Some(service_lock(&file)?),
        };
        match action {
            Action::Status => manager("launchctl", &["print", &domain]),
            Action::Install => {
                write_managed(&file, content.as_bytes())?;
                require_success(manager("launchctl", &["enable", &domain])?)?;
                println!(
                    "Installed {}; bootstrap it at an idle boundary. No job was started or restarted.",
                    file.display()
                );
                Ok(ExitCode::SUCCESS)
            }
            Action::Uninstall => {
                check_managed(&file, content.as_bytes())?;
                if file.exists() {
                    require_success(manager("launchctl", &["disable", &domain])?)?;
                    remove_if_present(&file)?;
                }
                println!("Removed {}; running jobs were left alone.", file.display());
                Ok(ExitCode::SUCCESS)
            }
        }
    }
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _unsupported = (action, layout, home, state, root);
        Err("user services support Linux and macOS".into())
    }
}

fn require_success(code: ExitCode) -> Result<()> {
    if code == ExitCode::SUCCESS {
        Ok(())
    } else {
        Err("user service manager failed; inspect its output and retry the same command".into())
    }
}

fn manager(program: &str, args: &[&str]) -> Result<ExitCode> {
    // This synchronous operator entry point runs before the server executor.
    // A current-thread executor bounds the child without polling or opening state.
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?
        .block_on(async {
            let mut child = tokio::process::Command::new(program)
                .args(args)
                .stdin(Stdio::null())
                .kill_on_drop(true)
                .spawn()?;
            match tokio::time::timeout(MANAGER_TIMEOUT, child.wait()).await {
                Ok(status) => Ok(if status?.success() {
                    ExitCode::SUCCESS
                } else {
                    ExitCode::FAILURE
                }),
                Err(_) => {
                    child.kill().await?;
                    Err("user service manager timed out; its outcome is unknown; inspect service status before retrying".into())
                }
            }
        })
}

fn check_managed(path: &Path, content: &[u8]) -> Result<()> {
    match fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => {
            Err(format!("refusing service symlink: {}", path.display()).into())
        }
        Ok(_) if fs::read(path)? == content => Ok(()),
        Ok(_) => Err(format!(
            "service configuration differs: {}; no service was changed",
            path.display()
        )
        .into()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn service_lock(path: &Path) -> Result<fs::File> {
    let parent = path.parent().ok_or("service path has no parent")?;
    fs::create_dir_all(parent)?;
    // Serialize this CLI's writers without opening runtime state or its locks.
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(parent.join(".studio-service.lock"))?;
    lock.try_lock()?;
    Ok(lock)
}

fn write_managed(path: &Path, content: &[u8]) -> Result<()> {
    let parent = path.parent().ok_or("service path has no parent")?;
    check_managed(path, content)?;
    if path.exists() {
        return Ok(());
    }
    let temporary = path.with_extension(format!("{}.tmp", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(PRIVATE_SERVICE_FILE_MODE);
    }
    let result = (|| -> Result<()> {
        let mut file = options.open(&temporary)?;
        file.write_all(content)?;
        file.sync_all()?;
        fs::rename(&temporary, path)?;
        fs::File::open(parent)?.sync_all()?;
        Ok(())
    })();
    if temporary.exists() {
        fs::remove_file(&temporary)?;
    }
    result
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

fn launch_environment(state: &Path) -> BTreeMap<String, String> {
    let mut environment = BTreeMap::from([(
        "CODEX_AGENTS_STATE_DIR".to_owned(),
        state.to_string_lossy().into_owned(),
    )]);
    for key in ["PATH", PUBLIC_ORIGIN_ENVIRONMENT, "CODEX_BIN", "CODEX_NODE"] {
        if let Ok(value) = std::env::var(key) {
            environment.insert(key.to_owned(), value);
        }
    }
    environment
}

fn service_value(value: &str) -> Result<()> {
    if value.contains(['\r', '\n', '\0']) {
        return Err("service values cannot contain newlines or NUL".into());
    }
    Ok(())
}

#[cfg(any(target_os = "linux", test))]
fn systemd_quote(value: &str) -> Result<String> {
    service_value(value)?;
    Ok(format!(
        "\"{}\"",
        value
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
            .replace('%', "%%")
    ))
}

#[cfg(any(target_os = "linux", test))]
fn linux_unit(layout: &ServiceLayout, root: &Path, state: &Path) -> Result<String> {
    let current = root.join(&layout.current_relative);
    let binary =
        systemd_quote(&current.join(&layout.binary_relative).to_string_lossy())?.replace('$', "$$");
    let directory = systemd_quote(&current.to_string_lossy())?;
    let mut environment = String::new();
    // Carry the operator's launch environment, never backend/Python mode flags.
    for (key, value) in launch_environment(state) {
        environment.push_str(&format!(
            "Environment={}\n",
            systemd_quote(&format!("{key}={value}"))?
        ));
    }
    Ok(format!(
        "# {SERVICE_MARKER}\n[Unit]\nDescription=Codex Studio server\nAfter=network-online.target\n\n[Service]\nType=exec\nWorkingDirectory={directory}\n{environment}ExecStart={binary} --port {}\nRestart=always\nRestartSec={RESTART_DELAY_SECONDS}\nTimeoutStopSec={STOP_TIMEOUT_SECONDS}\n\n[Install]\nWantedBy=default.target\n",
        layout.port
    ))
}

#[cfg(any(target_os = "macos", test))]
fn xml(value: &str) -> Result<String> {
    service_value(value)?;
    Ok(value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;"))
}

#[cfg(any(target_os = "macos", test))]
fn macos_label(layout: &ServiceLayout, state: &Path) -> String {
    use sha2::{Digest, Sha256};
    // Full state identity prevents collisions between independent installations.
    format!(
        "local.codex.agents.{}.{}",
        layout.unit_name.trim_end_matches(".service"),
        data_encoding::HEXLOWER.encode(&Sha256::digest(state.as_os_str().as_encoded_bytes()))
    )
}

#[cfg(any(target_os = "macos", test))]
fn macos_plist(layout: &ServiceLayout, root: &Path, state: &Path, label: &str) -> Result<String> {
    let current = root.join(&layout.current_relative);
    let binary = xml(&current.join(&layout.binary_relative).to_string_lossy())?;
    let directory = xml(&current.to_string_lossy())?;
    let mut environment = String::new();
    for (key, value) in launch_environment(state) {
        environment.push_str(&format!(
            "<key>{}</key><string>{}</string>",
            xml(&key)?,
            xml(&value)?
        ));
    }
    let label = xml(label)?;
    Ok(format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n<!-- {SERVICE_MARKER} -->\n<plist version=\"1.0\"><dict><key>Label</key><string>{label}</string><key>ProgramArguments</key><array><string>{binary}</string><string>--port</string><string>{}</string></array><key>WorkingDirectory</key><string>{directory}</string><key>EnvironmentVariables</key><dict>{environment}</dict><key>RunAtLoad</key><true/><key>KeepAlive</key><true/></dict></plist>\n",
        layout.port
    ))
}

#[cfg(test)]
#[path = "../../tests/support/service_templates.rs"]
mod tests;
