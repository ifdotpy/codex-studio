//! layr: local version control for agent workspaces on btrfs.

// Unsafe code is allowed only in `sys` (system calls without safe wrappers).
#![deny(unsafe_code)]

#[macro_use]
mod ctx;
mod args;
mod btrfs;
mod changes;
mod cmd;
mod db;
mod diff;
mod fsutil;
mod gitbridge;
mod ids;
mod ignore_rules;
mod merge;
mod model;
mod oplog;
mod privs;
mod repo;
mod revs;
mod service;
mod store;
mod stream;
#[allow(unsafe_code)]
mod sys;
mod tree;

use std::collections::HashMap;

fn main() {
    let mut argv: Vec<String> = std::env::args().skip(1).collect();
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
    if argv.first().map(|s| s.as_str()) == Some("daemon") {
        match service::daemon(&argv[1..]) {
            Ok(c) => std::process::exit(c),
            Err(e) => {
                eprintln!("fatal: {e:#}");
                std::process::exit(128);
            }
        }
    }
    let direct = rustix::process::geteuid().is_root() || std::env::var("LAYR_DIRECT").map(|v| v == "1").unwrap_or(false);
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
