//! `layr-desk serve`: the gateway process (docs/design.md, "Gateway"). The gateway host writes
//! the configuration and runs it under systemd: the certificate comes as credentials, and it
//! listens on the tailnet address only.

use crate::aws::{Aws, S3};
use crate::routes::{Config, Gateway};
use crate::tls;
use crate::vault::{system_clock, Bucket, Vault};
use anyhow::{anyhow, bail, Context, Result};
use clap::{Args, ValueEnum};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper_util::rt::{TokioIo, TokioTimer};
use serde::Deserialize;
use std::collections::HashMap;
use std::net::IpAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::net::TcpListener;
use tokio_rustls::TlsAcceptor;

#[derive(Args)]
pub struct Options {
    /// The configuration that the gateway host writes: {host, listen, projects}
    #[arg(long, default_value = "/etc/vault-gateway/config.json")]
    config: PathBuf,
    /// The folder of the built site; `site` next to this program by default
    #[arg(long)]
    site: Option<PathBuf>,
    #[arg(long, default_value_t = 443)]
    port: u16,
    /// Where the certificate comes from: PEM files (--cert and --key, by default tls.crt and
    /// tls.key of the systemd credentials), the files of `tailscale cert`, or none (plain HTTP
    /// on a loopback address, for a local check)
    #[arg(long, value_enum, default_value_t = TlsMode::Files)]
    tls: TlsMode,
    #[arg(long)]
    cert: Option<PathBuf>,
    #[arg(long)]
    key: Option<PathBuf>,
    /// With --tls tailscale: the folder where `tailscale cert` wrote <name>.crt and <name>.key
    #[arg(long, default_value = "/etc/vault-gateway/tls")]
    tls_dir: PathBuf,
}

#[derive(Clone, ValueEnum)]
enum TlsMode {
    Files,
    Tailscale,
    None,
}

/// The configuration file, read again when its modification time changes.
pub struct ConfigFile {
    path: PathBuf,
    loaded: Mutex<Option<(SystemTime, Arc<Config>)>>,
}

impl ConfigFile {
    pub fn new(path: PathBuf) -> Self {
        ConfigFile {
            path,
            loaded: Mutex::new(None),
        }
    }

    pub fn get(&self) -> Result<Arc<Config>> {
        let mtime = std::fs::metadata(&self.path)
            .and_then(|m| m.modified())
            .with_context(|| format!("reading {}", self.path.display()))?;
        let mut loaded = self.loaded.lock().unwrap();
        if let Some((at, config)) = loaded.as_ref() {
            if *at == mtime {
                return Ok(config.clone());
            }
        }
        let text = std::fs::read(&self.path)?;
        let config: Arc<Config> = Arc::new(
            serde_json::from_slice(&text)
                .with_context(|| format!("{} is not valid", self.path.display()))?,
        );
        *loaded = Some((mtime, config.clone()));
        Ok(config)
    }
}

/// One vault of the configuration.
#[derive(Deserialize)]
struct Project {
    bucket: String,
    region: String,
    role: String,
    recipient: String,
}

fn certificate(options: &Options) -> Result<Option<TlsAcceptor>> {
    let (cert, key) = match options.tls {
        TlsMode::None => return Ok(None),
        TlsMode::Tailscale => tls::tailscale_files(&options.tls_dir)?,
        TlsMode::Files => {
            let credentials = std::env::var_os("CREDENTIALS_DIRECTORY").map(PathBuf::from);
            let default = |name: &str| credentials.as_ref().map(|c| c.join(name));
            match (
                options.cert.clone().or_else(|| default("tls.crt")),
                options.key.clone().or_else(|| default("tls.key")),
            ) {
                (Some(cert), Some(key)) => (cert, key),
                _ => bail!(
                    "give --cert and --key, or run with systemd credentials tls.crt and tls.key"
                ),
            }
        }
    };
    let resolver = Arc::new(tls::Reloading::new(cert, key, tls::provider())?);
    Ok(Some(tls::acceptor(resolver)?))
}

pub async fn run(options: Options) -> Result<()> {
    let file = Arc::new(ConfigFile::new(options.config.clone()));
    let first = file.get()?;
    let listen = first
        .listen
        .clone()
        .filter(|l| !l.is_empty())
        .ok_or_else(|| anyhow!("{} has no listen address yet", options.config.display()))?;
    let address: IpAddr = listen
        .parse()
        .with_context(|| format!("the listen address {listen:?}"))?;
    let tls = certificate(&options)?;
    if tls.is_none() && !address.is_loopback() {
        bail!("--tls none serves plain HTTP, on a loopback address only");
    }
    let site = match options.site.clone() {
        Some(site) => site,
        None => std::env::current_exe()?
            .parent()
            .ok_or_else(|| anyhow!("this program is in no folder"))?
            .join("site"),
    };

    let aws = Aws::from_env()?;
    let mut secret = [0u8; 32];
    getrandom::getrandom(&mut secret).map_err(|e| anyhow!("no random secret: {e}"))?;
    let secret = Arc::new(secret);
    let vaults: Mutex<HashMap<String, Arc<Vault<S3>>>> = Mutex::default();
    let vault_for = move |name: &str, config: &Config| -> Result<Option<Arc<Vault<S3>>>> {
        let Some(value) = config.projects.get(name) else {
            return Ok(None);
        };
        // A changed entry makes a new vault.
        let key = serde_json::json!([name, value]).to_string();
        let mut vaults = vaults.lock().unwrap();
        if let Some(vault) = vaults.get(&key) {
            return Ok(Some(vault.clone()));
        }
        let project: Project = serde_json::from_value(value.clone())
            .with_context(|| format!("the vault {name} in the configuration"))?;
        let vault = Arc::new(Vault::new(
            aws.bucket(&project.bucket, &project.region, &project.role),
            name,
            &project.recipient,
            secret.clone(),
            system_clock(),
        ));
        vaults.insert(key, vault.clone());
        Ok(Some(vault))
    };
    let config = file.clone();
    let gateway = Arc::new(Gateway {
        config: Box::new(move || config.get()),
        vault_for: Box::new(vault_for),
        site,
    });

    let listener = TcpListener::bind((address, options.port))
        .await
        .with_context(|| format!("listening on {listen} port {}", options.port))?;
    let scheme = if tls.is_some() { "https" } else { "http" };
    eprintln!(
        "layr-desk serve: listening on {scheme}://{} for host {}",
        listener.local_addr()?,
        first.host.as_deref().unwrap_or("(any)")
    );
    serve(listener, tls, gateway).await
}

/// Answer connections on `listener` until the process stops.
pub async fn serve<B: Bucket>(
    listener: TcpListener,
    tls: Option<TlsAcceptor>,
    gateway: Arc<Gateway<B>>,
) -> Result<()> {
    loop {
        let stream = match listener.accept().await {
            Ok((stream, _)) => stream,
            Err(e) => {
                eprintln!("layr-desk serve: accept: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let (tls, gateway) = (tls.clone(), gateway.clone());
        tokio::spawn(async move {
            let service = service_fn(move |request| gateway.clone().handle(request));
            let mut builder = http1::Builder::new();
            builder
                .timer(TokioTimer::new())
                .header_read_timeout(Duration::from_secs(30));
            // A closed tab or a failed handshake ends its connection, nothing more.
            let _ = match tls {
                Some(tls) => {
                    let Ok(Ok(stream)) =
                        tokio::time::timeout(Duration::from_secs(10), tls.accept(stream)).await
                    else {
                        return;
                    };
                    builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                }
                None => {
                    builder
                        .serve_connection(TokioIo::new(stream), service)
                        .await
                }
            };
        });
    }
}
