//! layr-desk: the gateway, a reader of vaults through it, a network disk on a Mac, and the writer
//! that pushes project states into a vault.
//!
//!   layr-desk ls      --site https://desk.example.com --project <p> [--paper-file <file>]
//!   layr-desk verify  --site https://desk.example.com --project <p> --source <folder>
//!   layr-desk states  --site https://desk.example.com --project <p> [--json]
//!   layr-desk pull    --site https://desk.example.com --project <p> [--state <seq>] --to <new folder>
//!   layr-desk unlock  --site https://desk.example.com --project <p>   (keeps the key in Keychain)
//!   layr-desk mount   --site https://desk.example.com [--port 8095] [--at ~/Desk]
//!   layr-desk service install|remove --site https://desk.example.com [--port 8095]
//!   layr-desk serve   [--config /etc/vault-gateway/config.json] [--tls files|tailscale|none]
//!   layr-desk push    --project <p> --state-dir <folder> [--config <file>]   (on the project's host)
//!
//! The paper key comes from `--paper-file` (24 words or the age form) or from a hidden prompt.

mod aws;
mod disk;
mod gateway;
mod keystore;
mod push;
mod restore;
mod routes;
mod s3;
mod serve;
mod service;
mod tls;
mod vault;
mod webdav;

#[cfg(test)]
mod serve_tests;
#[cfg(test)]
mod tests;

use anyhow::{bail, Result};
use clap::{Args, Parser, Subcommand};
use desk_core::Reader;
use sha2::{Digest, Sha256};
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Parser)]
#[command(name = "layr-desk", about = "End-to-end encrypted project copies")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Args)]
struct Target {
    /// The Desk site, for example https://desk.layr.ws
    #[arg(long)]
    site: String,
    /// The vault (project prefix)
    #[arg(long)]
    project: String,
    /// A file with the paper key; without it the key is asked for
    #[arg(long)]
    paper_file: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Command {
    /// The vaults that a gateway serves
    Projects {
        #[arg(long)]
        site: String,
    },
    /// The files of the latest state of a vault
    Ls(Target),
    /// Compare the latest state of a vault with a folder, file by file
    Verify {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        source: PathBuf,
    },
    /// The states of a vault, oldest first
    States {
        #[command(flatten)]
        target: Target,
        #[arg(long)]
        json: bool,
    },
    /// Write one state of a vault (the latest by default) into a new folder
    Pull {
        #[command(flatten)]
        target: Target,
        /// The state number (seq); the latest when left out
        #[arg(long)]
        state: Option<u64>,
        /// A folder that does not exist yet
        #[arg(long)]
        to: PathBuf,
        #[arg(long)]
        json: bool,
    },
    /// Open a vault with its paper key and keep the vault key in the Keychain
    Unlock(Target),
    /// Forget the vault key of a vault on this Mac
    Lock {
        #[arg(long)]
        site: String,
        #[arg(long)]
        project: String,
    },
    /// Serve every vault of a gateway as a read-only WebDAV disk on localhost
    Mount {
        #[arg(long)]
        site: String,
        #[arg(long, default_value_t = 8095)]
        port: u16,
        /// Mount the disk at this folder (the volume "Desk" in Finder) once it listens
        #[arg(long)]
        at: Option<PathBuf>,
        /// Seconds between two looks for new states
        #[arg(long, default_value_t = 120)]
        every: u64,
    },
    /// Start the disk at login (a launchd agent), or stop that
    Service {
        #[command(subcommand)]
        action: service::Action,
    },
    /// The gateway: serve the Desk site and the API of every vault in the configuration
    Serve(serve::Options),
    /// Push the main head of a layr project into its vault (the writer)
    Push(push::Push),
}

fn paper(target: &Target) -> Result<String> {
    match &target.paper_file {
        Some(file) => Ok(std::fs::read_to_string(file)?),
        None => Ok(rpassword::prompt_password(
            "Paper key of the vault (hidden): ",
        )?),
    }
}

/// The vault through its gateway: with the key that `unlock` kept, else with the paper key.
async fn open(target: &Target) -> Result<Reader<gateway::Vault>> {
    let mut vault = gateway::Site::new(&target.site)?.vault(&target.project);
    let kept = match (&target.paper_file, keystore::system()) {
        (None, Ok(keys)) => keys.get(&keystore::account(&target.site, &target.project)?)?,
        _ => None,
    };
    match kept {
        Some(identity) => vault = vault.unlocked(&identity),
        None if target.paper_file.is_none() && !std::io::stdin().is_terminal() => bail!(
            "the vault {} is locked on this Mac; run in a terminal: layr-desk unlock --site {} --project {}",
            target.project,
            target.site,
            target.project
        ),
        None => {
            vault.unlock_with_paper(&paper(target)?).await?;
        }
    }
    let names = vault.states().await?;
    let identity = vault.identity()?.to_string();
    Reader::new(vault, &identity, &names)
}

#[tokio::main]
async fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Projects { site } => {
            for name in gateway::Site::new(&site)?.projects().await? {
                println!("{name}");
            }
        }
        Command::Ls(target) => {
            let reader = open(&target).await?;
            for (path, entry) in reader.files(None).await? {
                println!("{} {:>10} {path}", entry.mode(), entry.size());
            }
        }
        Command::Verify { target, source } => {
            let reader = open(&target).await?;
            let (seq, _) = reader.latest()?;
            let files = reader.files(None).await?;
            let mut problems = 0;
            for (path, entry) in &files {
                let data = reader.read(entry).await?;
                let (want, mode) = desk_core::writer::content(&source.join(path))?;
                if Sha256::digest(&data) != Sha256::digest(&want) || mode != entry.mode() {
                    println!("differs: {path}");
                    problems += 1;
                }
            }
            println!("state {seq}: {} files, {problems} differ", files.len());
            if problems > 0 {
                bail!("the vault and the folder differ");
            }
        }
        Command::States { target, json } => {
            let reader = open(&target).await?;
            let mut states = Vec::new();
            for (seq, attempt) in reader.states() {
                let header = reader.header(seq, &attempt).await?;
                states.push(serde_json::json!({"seq": seq, "attempt": attempt, "time": header.time, "state": header.state}));
            }
            if json {
                println!("{}", serde_json::json!({ "states": states }));
            } else {
                for s in &states {
                    println!(
                        "{:>6} {} {}",
                        s["seq"],
                        s["time"],
                        s["state"].as_str().unwrap_or("")
                    );
                }
            }
        }
        Command::Pull {
            target,
            state,
            to,
            json,
        } => {
            let reader = open(&target).await?;
            let state = match state {
                Some(seq) => reader
                    .states()
                    .into_iter()
                    .find(|(s, _)| *s == seq)
                    .ok_or_else(|| anyhow::anyhow!("the vault has no state {seq}"))?,
                None => reader.latest()?,
            };
            let summary = restore::pull(&reader, state, &to).await?;
            if json {
                println!("{}", serde_json::to_string(&summary)?);
            } else {
                println!(
                    "state {}: {} files ({} bytes) in {}",
                    summary.seq,
                    summary.files,
                    summary.bytes,
                    to.display()
                );
            }
        }
        Command::Unlock(target) => {
            let mut vault = gateway::Site::new(&target.site)?.vault(&target.project);
            let identity = vault.unlock_with_paper(&paper(&target)?).await?;
            let account = keystore::account(&target.site, &target.project)?;
            keystore::system()?.set(&account, &identity)?;
            println!("{account}: the vault key is in the Keychain");
        }
        Command::Lock { site, project } => {
            let account = keystore::account(&site, &project)?;
            keystore::system()?.remove(&account)?;
            println!("{account}: the vault key is removed from this Mac");
        }
        Command::Mount {
            site,
            port,
            at,
            every,
        } => {
            let cache = home()?.join("Library/Caches/layr-desk");
            let disk = std::sync::Arc::new(disk::Disk::new(
                &site,
                keystore::system()?,
                cache,
                std::time::Duration::from_secs(every),
            )?);
            serve(disk, port, at).await?;
        }
        Command::Service { action } => service::run(action)?,
        Command::Serve(options) => serve::run(options).await?,
        Command::Push(args) => push::run(args).await?,
    }
    Ok(())
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME")?))
}

/// Serve the disk on 127.0.0.1 until the process stops.
async fn serve(disk: std::sync::Arc<disk::Disk>, port: u16, at: Option<PathBuf>) -> Result<()> {
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", port)).await?;
    let url = format!("http://127.0.0.1:{port}/");
    eprintln!("layr-desk: the disk is at {url} (read only)");
    if let Some(at) = at {
        tokio::task::spawn_blocking(move || {
            if let Err(e) = mount(&url, &at) {
                eprintln!(
                    "layr-desk: the disk is not mounted at {}: {e:#}",
                    at.display()
                );
            }
        });
    }
    run(listener, disk).await
}

/// Answer WebDAV requests on `listener` until the process stops.
async fn run(listener: tokio::net::TcpListener, disk: std::sync::Arc<disk::Disk>) -> Result<()> {
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    loop {
        let (stream, _) = listener.accept().await?;
        let disk = disk.clone();
        tokio::spawn(async move {
            let service = service_fn(move |request| webdav::handle(disk.clone(), request));
            if let Err(e) = http1::Builder::new()
                .serve_connection(TokioIo::new(stream), service)
                .await
            {
                eprintln!("layr-desk: connection: {e}");
            }
        });
    }
}

/// Mount the disk with the macOS WebDAV client, without dialogs; a mounted disk stays.
fn mount(url: &str, at: &Path) -> Result<()> {
    let mounts = std::process::Command::new("/sbin/mount").output()?;
    let line = format!(" on {} (", at.display());
    if String::from_utf8_lossy(&mounts.stdout).contains(&line) {
        return Ok(());
    }
    std::fs::create_dir_all(at)?;
    let status = std::process::Command::new("/sbin/mount_webdav")
        .args(["-S", "-v", "Desk", url])
        .arg(at)
        .status()?;
    if !status.success() {
        bail!("mount_webdav failed");
    }
    eprintln!("layr-desk: mounted at {}", at.display());
    Ok(())
}
