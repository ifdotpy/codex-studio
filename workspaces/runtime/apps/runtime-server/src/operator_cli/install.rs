//! Replace only links owned by the selected checkout; preflight every conflict.
use super::{Result, home};
use std::{
    fs,
    path::{Path, PathBuf},
    process::ExitCode,
};
use studio_runtime_store::path_identity::CanonicalPath;

/// Commands formerly installed from the removed Python backend.
const LEGACY_COMMANDS: &[&str] = &[
    "codex-chat",
    "codex-control",
    "codex-supervisor",
    "codex-daemon",
    "codex-graph",
    "codex-models",
    "codex-report",
    "codex-steer",
    "codex-stop",
    "codex-swarm.mjs",
    "codex-watch",
    "luna",
];
/// Public command names with a real Rust implementation.
const RUST_COMMANDS: &[(&str, &str)] = &[
    ("studio-runtime-server", "studio-runtime-server"),
    ("codex-canvas", "studio-runtime-server"),
    ("codex-process-supervisor", "codex-process-supervisor"),
    ("codex-diagnostics", "codex-diagnostics"),
];
/// Unix execute bits accepted for a command binary.
const EXECUTABLE_PERMISSION_MASK: u32 = 0o111;

pub(super) fn run(args: &[String]) -> Result<ExitCode> {
    #[cfg(not(unix))]
    {
        let _unsupported = args;
        Err("command link installation requires Unix".into())
    }
    #[cfg(unix)]
    {
        let executable = CanonicalPath::existing(std::env::current_exe()?)?.into_path_buf();
        let binaries = executable.parent().ok_or("binary has no parent")?;
        let mut destination = std::env::var_os("CODEX_AGENTS_BIN_DIR")
            .map(PathBuf::from)
            .unwrap_or(home()?.join(".local/bin"));
        let mut project = None;
        let mut args = args.iter();
        while let Some(flag) = args.next() {
            let value = args
                .next()
                .ok_or("install-cli options require a directory")?;
            match flag.as_str() {
                "--bin-dir" => destination = PathBuf::from(value),
                "--project" => project = Some(PathBuf::from(value)),
                _ => return Err(format!("unknown install-cli option: {flag}").into()),
            }
        }
        let project = CanonicalPath::existing(
            project.unwrap_or_else(|| binaries.parent().unwrap_or(binaries).to_path_buf()),
        )?
        .into_path_buf();
        let destination = if destination.is_absolute() {
            destination
        } else {
            std::env::current_dir()?.join(destination)
        };
        let mut replacements = Vec::new();
        let mut removals = Vec::new();
        // Complete the preflight before changing any command link.
        for (name, binary) in RUST_COMMANDS {
            let source = binaries.join(binary);
            use std::os::unix::fs::PermissionsExt;
            if !source.is_file()
                || source.metadata()?.permissions().mode() & EXECUTABLE_PERMISSION_MASK == 0
            {
                return Err(format!(
                    "missing executable: {}; build with just install",
                    source.display()
                )
                .into());
            }
            let target = destination.join(name);
            match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    let link = link_destination(&target)?;
                    if link != source && !legacy_link(&project, name, &link) {
                        return Err(format!(
                            "refusing unrelated command link: {}",
                            target.display()
                        )
                        .into());
                    }
                    if link == source {
                        continue;
                    }
                }
                Ok(_) => {
                    return Err(
                        format!("refusing existing executable: {}", target.display()).into(),
                    );
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
            replacements.push((source, target));
        }
        for name in LEGACY_COMMANDS {
            let target = destination.join(name);
            match fs::symlink_metadata(&target) {
                Ok(metadata) if metadata.file_type().is_symlink() => {
                    if legacy_link(&project, name, &link_destination(&target)?) {
                        removals.push(target);
                    }
                }
                Ok(_) => (), // Unrelated legacy names remain the operator's property.
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => (),
                Err(error) => return Err(error.into()),
            }
        }
        fs::create_dir_all(&destination)?;
        for (source, target) in replacements {
            let temporary = destination.join(format!(
                ".{}.{}.tmp",
                target
                    .file_name()
                    .ok_or("invalid command name")?
                    .to_string_lossy(),
                std::process::id()
            ));
            std::os::unix::fs::symlink(source, &temporary)?;
            if let Err(error) = fs::rename(&temporary, &target) {
                fs::remove_file(&temporary)?;
                return Err(error.into());
            }
            println!("Installed {}", target.display());
        }
        for target in removals {
            fs::remove_file(&target)?;
            println!("Removed legacy Python command {}", target.display());
        }
        Ok(ExitCode::SUCCESS)
    }
}

fn link_destination(target: &Path) -> Result<PathBuf> {
    let link = fs::read_link(target)?;
    Ok(if link.is_absolute() {
        link
    } else {
        target.parent().ok_or("link has no parent")?.join(link)
    })
}

fn legacy_link(project: &Path, name: &str, destination: &Path) -> bool {
    ["workspaces/runtime/apps/server/src", "scripts"]
        .iter()
        .any(|directory| destination == project.join(directory).join(name))
}
