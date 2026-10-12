use std::env;
use std::path::{Component, Path, PathBuf};

use crate::{StoreError, StoreResult};

const STATE_DIRECTORY_NAME: &str = "codex-agents";
#[cfg(windows)]
const WINDOWS_STATE_DIRECTORY: &str = "CodexStudio/state";

pub fn resolve_state_dir() -> StoreResult<PathBuf> {
    if let Some(configured) = env::var_os("CODEX_AGENTS_STATE_DIR") {
        return resolve_explicit_state_dir(Path::new(&configured));
    }

    #[cfg(windows)]
    let candidate = if let Some(local_app_data) = env::var_os("LOCALAPPDATA") {
        PathBuf::from(local_app_data).join(WINDOWS_STATE_DIRECTORY)
    } else {
        state_home()?.join(STATE_DIRECTORY_NAME)
    };

    #[cfg(not(windows))]
    let candidate = state_home()?.join(STATE_DIRECTORY_NAME);

    canonical_existing_dir(candidate)
}

fn state_home() -> StoreResult<PathBuf> {
    if let Some(configured) = env::var_os("XDG_STATE_HOME") {
        return Ok(PathBuf::from(configured));
    }
    let home = env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                "home directory is unavailable",
            )
        })?;
    Ok(home.join(".local").join("state"))
}

pub(crate) fn resolve_explicit_state_dir(path: &Path) -> StoreResult<PathBuf> {
    let expanded = expand_home(path)?;
    canonical_existing_dir(expanded)
}

fn expand_home(path: &Path) -> StoreResult<PathBuf> {
    if path == Path::new("~") || path.starts_with("~/") || path.starts_with("~\\") {
        let home = env::var_os("HOME")
            .or_else(|| env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .ok_or_else(|| {
                std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    "home directory is unavailable",
                )
            })?;
        let tail = path.strip_prefix("~").unwrap_or(Path::new(""));
        return Ok(home.join(tail));
    }
    Ok(path.to_path_buf())
}

fn canonical_existing_dir(path: PathBuf) -> StoreResult<PathBuf> {
    let resolved = path.canonicalize()?;
    if resolved
        .components()
        .any(|part| matches!(part, Component::Normal(value) if value == ".claude"))
    {
        return Err(StoreError::UnsafeStateDirectory);
    }
    if !resolved.is_dir() {
        return Err(StoreError::StateDirectory(std::io::Error::new(
            std::io::ErrorKind::NotADirectory,
            "state path is not a directory",
        )));
    }
    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::expand_home;
    use std::path::Path;

    #[test]
    fn explicit_paths_expand_only_a_leading_tilde() {
        let home = std::env::var_os("HOME").expect("test host has HOME");
        assert_eq!(
            expand_home(Path::new("~/studio")).expect("home expansion succeeds"),
            Path::new(&home).join("studio")
        );
        assert_eq!(
            expand_home(Path::new("/tmp/~studio")).expect("absolute path is unchanged"),
            Path::new("/tmp/~studio")
        );
    }
}
