//! Operator entry points run before Tokio or any runtime state is opened.
mod install;
mod linux_vm;
mod mac_sync;
mod service;
mod stored_text;

use serde::Deserialize;
use std::{error::Error, path::PathBuf, process::ExitCode};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

/// Shared with deliver.sh. Paths are relative to HOME or the release root.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct ServiceLayout {
    unit_name: String,
    pub(crate) port: u16,
    binary_relative: PathBuf,
    current_relative: PathBuf,
    root_relative: PathBuf,
    state_relative: PathBuf,
}

pub(crate) fn service_layout() -> Result<ServiceLayout> {
    Ok(serde_json::from_str(include_str!(
        "../service-layout.json"
    ))?)
}

pub(crate) fn dispatch() -> Option<ExitCode> {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let result = match args.first().map(String::as_str) {
        Some("service") => service::run(&args[1..]),
        Some("install-cli") => install::run(&args[1..]),
        Some("linux-vm") => linux_vm::run(&args[1..]),
        Some("stored-text") => stored_text::run(&args[1..]),
        Some("mac-sync") => mac_sync::run(&args[1..]),
        Some("--help" | "-h") => {
            println!(
                "usage: studio-runtime-server [--port PORT]\n       studio-runtime-server service <install|status|uninstall> [--state DIRECTORY]\n       studio-runtime-server install-cli [--bin-dir DIRECTORY] [--project DIRECTORY]\n       studio-runtime-server linux-vm <start|stop|status> [--state DIRECTORY]\n       studio-runtime-server stored-text inflate --state DIRECTORY\n       studio-runtime-server mac-sync plan PROJECT --state DIRECTORY\n\nService installation enables startup without starting or restarting a service.\nUse just deliver-server to change the server release."
            );
            Ok(ExitCode::SUCCESS)
        }
        _ => return None,
    };
    Some(match result {
        Ok(code) => code,
        Err(error) => {
            eprintln!("studio-runtime-server: {error}");
            ExitCode::FAILURE
        }
    })
}

fn home() -> Result<PathBuf> {
    let home = PathBuf::from(std::env::var_os("HOME").ok_or("HOME is required")?);
    if !home.is_absolute() {
        return Err("HOME must be absolute".into());
    }
    Ok(home)
}
