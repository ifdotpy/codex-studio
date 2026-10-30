//! `backup` operator commands (docs/backup.md). Settings are a file in the state directory,
//! so these commands work with the backend running; the backend reads the file again within
//! a minute.
//!
//! - `backup enable --state DIRECTORY --target SERVER [--retain N]`
//! - `backup disable --state DIRECTORY`
//! - `backup status --state DIRECTORY`
//! - `backup restore --from GENERATION_DIRECTORY --to FILE`

use std::path::Path;
use std::process::ExitCode;

use studio_runtime_services::peer_backup::Settings;
use studio_runtime_store::wal_backup::restore;

use super::Result;

const USAGE: &str = "usage: backup enable --state DIRECTORY --target SERVER [--retain N]\n       backup disable --state DIRECTORY\n       backup status --state DIRECTORY\n       backup restore --from GENERATION_DIRECTORY --to FILE";

fn flag<'a>(args: &'a [String], name: &str) -> Option<&'a str> {
    args.iter()
        .position(|arg| arg == name)
        .and_then(|index| args.get(index + 1))
        .map(String::as_str)
}

pub(super) fn run(args: &[String]) -> Result<ExitCode> {
    let command = args.first().map(String::as_str).ok_or(USAGE)?;
    if command == "restore" {
        let from = flag(args, "--from").ok_or(USAGE)?;
        let to = flag(args, "--to").ok_or(USAGE)?;
        let restored = restore(Path::new(from), Path::new(to))?;
        println!(
            "restored {to}: {} segments in {} WAL files, integrity_check ok, page count {:?} matches the source position",
            restored.segments, restored.wal_files, restored.pages
        );
        return Ok(ExitCode::SUCCESS);
    }
    let state = Path::new(flag(args, "--state").ok_or(USAGE)?);
    let mut settings = Settings::read(state);
    match command {
        "enable" => {
            settings.enabled = true;
            settings.target = Some(flag(args, "--target").ok_or(USAGE)?.to_owned());
            if let Some(retain) = flag(args, "--retain") {
                settings.retain_generations = retain.parse()?;
            }
            settings.write(state)?;
        }
        "disable" => {
            settings.enabled = false;
            settings.write(state)?;
        }
        "status" => {}
        _ => return Err(USAGE.into()),
    }
    println!("{}", serde_json::to_string_pretty(&settings)?);
    if command == "status" {
        if let Ok(position) = std::fs::read_to_string(state.join("backup/state.json")) {
            println!("{position}");
        }
    }
    Ok(ExitCode::SUCCESS)
}
