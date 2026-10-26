//! Host Layr installation and migration do not open a second Studio backend.
use super::Result;
use std::{path::PathBuf, process::ExitCode};
use studio_runtime_store::path_identity::CanonicalPath;

pub(super) fn run(args: &[String]) -> Result<ExitCode> {
    let mut args = args.iter();
    let command = args
        .next()
        .map(String::as_str)
        .ok_or("Use native-layr <install|migrate|paths>")?;
    let mut state = None;
    let mut source = None;
    let mut dry_run = false;
    let mut projects = Vec::new();
    let mut without_git = std::collections::BTreeSet::new();
    let mut skip_worktrees = Vec::new();
    let mut worktrees = studio_runtime_services::layr_native::migration::Worktrees::All;
    let mut worktrees_explicit = false;
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--state" => {
                state = Some(PathBuf::from(
                    args.next().ok_or("--state requires a directory")?,
                ))
            }
            "--source" => {
                source = Some(PathBuf::from(
                    args.next().ok_or("--source requires a directory")?,
                ))
            }
            "--dry-run" => dry_run = true,
            "--skip-worktree" => {
                let path = PathBuf::from(
                    args.next()
                        .ok_or("--skip-worktree requires a worktree path")?,
                );
                let path = if path.is_absolute() {
                    path
                } else {
                    std::env::current_dir()?.join(path)
                };
                skip_worktrees.push(
                    CanonicalPath::existing(&path)
                        .map(CanonicalPath::into_path_buf)
                        .unwrap_or(path),
                );
            }
            "--worktrees" => {
                worktrees_explicit = true;
                worktrees = studio_runtime_services::layr_native::migration::Worktrees::parse(
                    args.next().ok_or("--worktrees requires dirty or all")?,
                )
                .ok_or("--worktrees requires dirty or all")?;
            }
            "--without-git" => {
                let path = PathBuf::from(
                    args.next()
                        .ok_or("--without-git requires a project directory")?,
                );
                without_git
                    .insert(CanonicalPath::existing(&path).map(CanonicalPath::into_path_buf)?);
                projects.push(path);
            }
            value if value.starts_with('-') => {
                return Err(format!("Unknown native-layr option: {value}").into());
            }
            value => projects.push(PathBuf::from(value)),
        }
    }
    if command != "migrate"
        && (!without_git.is_empty() || !skip_worktrees.is_empty() || worktrees_explicit)
    {
        return Err(
            "--without-git, --skip-worktree and --worktrees are available only for native-layr migrate".into(),
        );
    }
    let state = state.unwrap_or(studio_runtime_store::resolve_state_dir()?.into_path_buf());
    let state = if state.is_absolute() {
        state
    } else {
        std::env::current_dir()?.join(state)
    };
    match command {
        "install" => {
            if let Some(source) = source {
                studio_runtime_services::layr_native::install::install(&state, &source)?;
            } else {
                studio_runtime_services::layr_native::install::ensure_binary(&state)?;
            }
            println!(
                "{}",
                studio_runtime_services::layr_native::binary_path(&state).display()
            );
        }
        "paths" => println!(
            "{}",
            serde_json::json!({
                "store":studio_runtime_services::layr_native::store_path(&state),
                "binary":studio_runtime_services::layr_native::binary_path(&state),
                "socket":studio_runtime_services::layr_native::socket_path(&state),
            })
        ),
        "migrate" => {
            if projects.is_empty() {
                return Err("Supply at least one project folder".into());
            }
            let mut seen = std::collections::BTreeSet::new();
            for project in projects {
                let project = CanonicalPath::existing(project).map(CanonicalPath::into_path_buf)?;
                if !seen.insert(project.clone()) {
                    continue;
                }
                let options = studio_runtime_services::layr_native::migration::Options {
                    without_git: without_git.contains(&project),
                    worktrees,
                    skip_worktrees: skip_worktrees.clone(),
                };
                println!(
                    "{}",
                    serde_json::to_string_pretty(
                        &studio_runtime_services::layr_native::migration::migrate_with_options(
                            &state, &project, dry_run, options
                        )?
                    )?
                );
            }
        }
        _ => return Err("Use native-layr <install|migrate|paths>".into()),
    }
    Ok(ExitCode::SUCCESS)
}
