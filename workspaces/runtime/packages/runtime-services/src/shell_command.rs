//! The shell argv of a command that runs outside the provider's own shell tool.
//!
//! Port of `codex_shell.monitor_command` (codex_shell.py:41-91). Monitors and
//! server commands (`orchestration_servers` action `exec`) use it.

use std::path::{Path, PathBuf};

use serde_json::Value;

/// Port of `codex_shell.monitor_command` (codex_shell.py:41-91): command/exec applies account shell
/// environment policy but accepts argv and does not perform the native exec
/// shell startup. Apply Codex's login/rc startup inside the command sandbox.
pub fn monitor_command(command: &str, config: &Value, guest: bool) -> Result<Vec<String>, String> {
    let shell = if guest {
        PathBuf::from("/bin/bash")
    } else {
        default_shell()
            .ok_or_else(|| "No supported shell is available for the command".to_owned())?
    };
    let name = shell
        .file_stem()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(name.as_str(), "pwsh" | "powershell") {
        return Ok(vec![
            shell.to_string_lossy().into_owned(),
            "-NoProfile".into(),
            "-Command".into(),
            command.into(),
        ]);
    }
    let login = config["allow_login_shell"].as_bool().unwrap_or(true);
    if !login {
        return Ok(vec![
            shell.to_string_lossy().into_owned(),
            "-c".into(),
            command.into(),
        ]);
    }
    let snapshot = config["features"]["shell_snapshot"]
        .as_bool()
        .unwrap_or(true);
    if !snapshot || !matches!(name.as_str(), "bash" | "zsh") {
        return Ok(vec![
            shell.to_string_lossy().into_owned(),
            "-lc".into(),
            command.into(),
        ]);
    }
    let startup = if name == "zsh" {
        "if [ -r \"${ZDOTDIR:-$HOME}/.zshrc\" ]; then . \"${ZDOTDIR:-$HOME}/.zshrc\"; fi"
    } else {
        "if [ -z \"$BASH_ENV\" ] && [ -r \"$HOME/.bashrc\" ]; then . \"$HOME/.bashrc\"; fi"
    };
    let mut lines = vec![format!("{{ {startup}; }} >/dev/null 2>&1")];
    if let Some(set) = config["shell_environment_policy"]["set"].as_object() {
        for (key, value) in set {
            if valid_environment_key(key) {
                lines.push(format!(
                    "export {key}={}",
                    shell_quote(&value_as_text(value))
                ));
            }
        }
    }
    #[cfg(target_os = "macos")]
    lines.push(
        "if [ -z \"${SDKROOT+x}\" ]; then if SDKROOT=\"$(/usr/bin/xcrun --sdk macosx --show-sdk-path 2>/dev/null)\" && [ -d \"$SDKROOT\" ]; then export SDKROOT; else unset SDKROOT; fi; fi".into(),
    );
    lines.push(format!(
        "exec {} -c {}",
        shell_quote(&shell.to_string_lossy()),
        shell_quote(command)
    ));
    Ok(vec![
        shell.to_string_lossy().into_owned(),
        "-lc".into(),
        lines.join("\n"),
    ])
}

fn valid_environment_key(key: &str) -> bool {
    let mut chars = key.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn value_as_text(value: &Value) -> String {
    value
        .as_str()
        .map(str::to_owned)
        .unwrap_or_else(|| value.to_string())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(unix)]
fn default_shell() -> Option<PathBuf> {
    let preferred = nix::unistd::User::from_uid(nix::unistd::Uid::current())
        .ok()
        .flatten()
        .map(|user| user.shell);
    if let Some(path) = preferred.filter(|path| supported_shell(path) && path.is_file()) {
        return Some(path);
    }
    let names = if cfg!(target_os = "macos") {
        ["zsh", "bash", "sh"]
    } else {
        ["bash", "zsh", "sh"]
    };
    for name in names {
        if let Some(path) = executable_in_path(name) {
            return Some(path);
        }
        for prefix in ["/bin", "/usr/bin"] {
            let path = Path::new(prefix).join(name);
            if path.is_file() {
                return Some(path);
            }
        }
    }
    None
}

#[cfg(not(unix))]
fn default_shell() -> Option<PathBuf> {
    ["pwsh", "powershell"]
        .iter()
        .find_map(|name| executable_in_path(name))
}

fn supported_shell(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .is_some_and(|name| matches!(name, "zsh" | "bash" | "sh" | "pwsh" | "powershell"))
}

fn executable_in_path(name: &str) -> Option<PathBuf> {
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|directory| directory.join(name))
        .find(|path| path.is_file())
}
