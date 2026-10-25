//! layr: local version control on btrfs.

// Unsafe code is allowed only in `sys` (system calls without safe wrappers).
#![deny(unsafe_code)]

#[macro_use]
mod ctx;
mod access;
#[cfg(target_os = "macos")]
mod apfs_commit;
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
mod apfs_stream;
#[cfg(any(target_os = "macos", feature = "overlayfs"))]
mod stream_fs;
#[cfg(all(feature = "overlayfs", not(target_os = "linux")))]
compile_error!("the overlayfs feature requires Linux");
mod args;
mod changes;
mod cmd;
mod db;
mod diff;
mod disk;
mod fsutil;
mod gitbridge;
mod ids;
use layr_store::http;
mod ignore_rules;
mod lineops;
mod merge;
mod model;
mod oci;
mod oplog;
mod overlay;
mod overlay_managers;
mod person;
mod privs;
mod probes;
mod radar;
mod recv;
mod repo;
mod revs;
mod secrets;
#[cfg(target_os = "linux")]
mod send_cache;
mod service;
#[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
#[path = "btrfs.rs"]
mod storage;
#[cfg(feature = "overlayfs")]
#[path = "overlayfs.rs"]
mod storage;
#[cfg(target_os = "macos")]
#[path = "apfs.rs"]
mod storage;
mod store;
mod store_remote;
mod stream;
#[allow(unsafe_code)]
mod sys;
#[cfg(target_os = "macos")]
mod thin;
mod trace;
mod tree;

use std::collections::HashMap;

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
    if argv.first().map(String::as_str) == Some("__registry-http") {
        let result = (|| -> anyhow::Result<()> {
            anyhow::ensure!(argv.len() == 3, "invalid internal registry arguments");
            anyhow::ensure!(!rustix::process::geteuid().is_root(), "registry HTTP process must not run as root");
            let fd = rustix::io::dup(std::io::stdin())?;
            crate::oci::registry::handle(std::net::TcpStream::from(fd), std::path::Path::new(&argv[1]), &argv[2])
        })();
        if let Err(e) = result {
            eprintln!("layr registry HTTP: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    #[cfg(any(target_os = "macos", feature = "overlayfs"))]
    if argv.first().map(String::as_str) == Some("__apfs-send") {
        let result = (|| -> anyhow::Result<()> {
            if argv.len() != 2 {
                anyhow::bail!("invalid internal send arguments");
            }
            crate::apfs_stream::send(std::path::Path::new(&argv[1]), &mut std::io::stdout().lock())
        })();
        if let Err(e) = result {
            eprintln!("fatal: {e:#}");
            std::process::exit(1);
        }
        return;
    }
    // Global options before the command.
    while let Some(first) = argv.first().cloned() {
        match first.as_str() {
            "-C" if argv.len() > 1 => {
                if let Err(e) = std::env::set_current_dir(&argv[1]) {
                    eprintln!("fatal: cannot change to '{}': {e}", argv[1]);
                    std::process::exit(128);
                }
                argv.drain(..2);
            }
            "--root" if argv.len() > 1 => {
                std::env::set_var("LAYR_ROOT", &argv[1]);
                argv.drain(..2);
            }
            "--project" if argv.len() > 1 => {
                std::env::set_var("LAYR_PROJECT", &argv[1]);
                argv.drain(..2);
            }
            "--line" if argv.len() > 1 => {
                std::env::set_var("LAYR_LINE", &argv[1]);
                argv.drain(..2);
            }
            "--no-pager" | "--no-optional-locks" | "-P" => {
                argv.remove(0);
            }
            "-c" if argv.len() > 1 => {
                argv.drain(..2);
            }
            _ => break,
        }
    }
    if argv.first().map(String::as_str) == Some("person") {
        match person::run(&argv[1..]) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("layr person: {error:#}");
                std::process::exit(1);
            }
        }
    }
    if argv.first().map(String::as_str) == Some("mount") {
        match disk::run(&argv[1..]) {
            Ok(code) => std::process::exit(code),
            Err(error) => {
                eprintln!("layr mount: {error:#}");
                std::process::exit(1);
            }
        }
    }
    if argv.first().map(|s| s.as_str()) == Some("daemon") {
        match service::daemon(&argv[1..]) {
            Ok(c) => std::process::exit(c),
            Err(e) => {
                eprintln!("fatal: {e:#}");
                std::process::exit(128);
            }
        }
    }
    let direct = (cfg!(target_os = "macos") && std::env::var_os("LAYR_SOCKET").is_none())
        || rustix::process::geteuid().is_root()
        || std::env::var("LAYR_DIRECT").map(|v| v == "1").unwrap_or(false);
    let code = if direct {
        let uid = rustix::process::getuid().as_raw();
        let cwd = std::env::current_dir().unwrap_or_else(|_| "/".into());
        let env: HashMap<String, String> = std::env::vars().collect();
        service::run_as(argv, ctx::Caller::from_uid(uid), cwd, env, &store::default_root())
    } else {
        match service::forward(&argv) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("fatal: {e:#}");
                128
            }
        }
    };
    std::process::exit(code);
}
