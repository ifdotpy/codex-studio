//! Host Layr installation and migration do not open a second Studio backend.
use super::Result;
use std::{path::PathBuf, process::ExitCode};

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
            value if value.starts_with('-') => {
                return Err(format!("Unknown native-layr option: {value}").into());
            }
            value => projects.push(PathBuf::from(value)),
        }
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
            for project in projects {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&studio_runtime_services::layr_native::migration::migrate(
                        &state, &project, dry_run
                    )?)?
                );
            }
        }
        _ => return Err("Use native-layr <install|migrate|paths>".into()),
    }
    Ok(ExitCode::SUCCESS)
}
