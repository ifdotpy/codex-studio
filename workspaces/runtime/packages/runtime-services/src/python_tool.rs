//! Optional Python tool discovery owned by the runtime-services layer.

use std::path::PathBuf;

/// Resolve Python without starting it. An explicit absolute path takes precedence;
/// otherwise search PATH for `python3`.
pub fn resolve_python_tool() -> Option<PathBuf> {
    resolve_python_tool_from(
        std::env::var_os("CODEX_AGENTS_PYTHON").as_deref(),
        std::env::var_os("PATH").as_deref(),
    )
}

fn resolve_python_tool_from(
    configured: Option<&std::ffi::OsStr>,
    path: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    if let Some(configured) = configured {
        let candidate = PathBuf::from(configured);
        if candidate.is_absolute() && executable_file(&candidate) {
            return Some(candidate);
        }
    }

    let path = path?;
    std::env::split_paths(path)
        .map(|directory| directory.join(python_executable_name()))
        .find(|candidate| executable_file(candidate))
}

fn executable_file(path: &std::path::Path) -> bool {
    let Ok(metadata) = path.metadata() else {
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

fn python_executable_name() -> &'static str {
    if cfg!(windows) {
        "python3.exe"
    } else {
        "python3"
    }
}

#[cfg(test)]
mod tests {
    use super::resolve_python_tool_from;
    use std::{ffi::OsStr, fs};

    #[test]
    fn explicit_absolute_tool_precedes_path_and_does_not_run() {
        let directory = tempfile::tempdir().expect("temporary Python tools directory");
        let configured = directory.path().join("configured-python");
        write_tool(&configured);
        let path_dir = directory.path().join("path");
        fs::create_dir(&path_dir).expect("PATH fixture directory creates");
        let path_tool = path_dir.join(if cfg!(windows) {
            "python3.exe"
        } else {
            "python3"
        });
        write_tool(&path_tool);
        let path = std::env::join_paths([&path_dir]).expect("PATH fixture encodes");

        assert_eq!(
            resolve_python_tool_from(Some(configured.as_os_str()), Some(&path)),
            Some(configured)
        );
        assert_eq!(resolve_python_tool_from(None, Some(&path)), Some(path_tool));
        assert_eq!(resolve_python_tool_from(None, Some(OsStr::new(""))), None);
    }

    #[test]
    fn configured_relative_or_missing_tool_falls_back_to_path() {
        let directory = tempfile::tempdir().expect("temporary Python tools directory");
        let path_dir = directory.path().join("path");
        fs::create_dir(&path_dir).expect("PATH fixture directory creates");
        let path_tool = path_dir.join(if cfg!(windows) {
            "python3.exe"
        } else {
            "python3"
        });
        write_tool(&path_tool);
        let path = std::env::join_paths([&path_dir]).expect("PATH fixture encodes");

        assert_eq!(
            resolve_python_tool_from(Some(OsStr::new("python3")), Some(&path)),
            Some(path_tool)
        );
    }

    fn write_tool(path: &std::path::Path) {
        fs::write(path, "not executed").expect("tool fixture writes");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(path, fs::Permissions::from_mode(0o755))
                .expect("tool fixture is executable");
        }
    }
}
