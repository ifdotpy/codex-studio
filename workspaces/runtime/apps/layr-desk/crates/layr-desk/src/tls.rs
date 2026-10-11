//! The gateway's certificate: PEM files that it reads again when they change, so a renewed
//! certificate needs no restart. They come from systemd credentials (certbot's files), from
//! paths given as flags, or from `tailscale cert` for a host without a domain of its own.

use anyhow::{anyhow, bail, Context, Result};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::pem::PemObject;
use rustls::pki_types::{CertificateDer, PrivateKeyDer};
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::SystemTime;

/// The modification times of the certificate and the key.
type Stamp = (Option<SystemTime>, Option<SystemTime>);

pub struct Reloading {
    cert: PathBuf,
    key: PathBuf,
    provider: Arc<CryptoProvider>,
    /// The loaded key, and the stamp that was last tried.
    state: Mutex<(Arc<CertifiedKey>, Stamp)>,
}

impl std::fmt::Debug for Reloading {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Reloading")
            .field("cert", &self.cert)
            .field("key", &self.key)
            .finish()
    }
}

fn stamp(cert: &Path, key: &Path) -> Stamp {
    let time = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    (time(cert), time(key))
}

pub fn provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

fn load(cert: &Path, key: &Path, provider: &CryptoProvider) -> Result<CertifiedKey> {
    let chain = CertificateDer::pem_file_iter(cert)
        .with_context(|| format!("reading {}", cert.display()))?
        .collect::<Result<Vec<_>, _>>()
        .with_context(|| format!("reading {}", cert.display()))?;
    if chain.is_empty() {
        bail!("{} has no certificate", cert.display());
    }
    let key_der = PrivateKeyDer::from_pem_file(key)
        .with_context(|| format!("reading the key {}", key.display()))?;
    CertifiedKey::from_der(chain, key_der, provider)
        .map_err(|e| anyhow!("{} and {}: {e}", cert.display(), key.display()))
}

impl Reloading {
    pub fn new(cert: PathBuf, key: PathBuf, provider: Arc<CryptoProvider>) -> Result<Self> {
        let at = stamp(&cert, &key);
        let loaded = load(&cert, &key, &provider)?;
        Ok(Reloading {
            cert,
            key,
            provider,
            state: Mutex::new((Arc::new(loaded), at)),
        })
    }

    /// The current key, read again when a file changed. A pair that does not load (one file
    /// written, the other not yet) keeps the old key until the files change again.
    pub fn current(&self) -> Arc<CertifiedKey> {
        let at = stamp(&self.cert, &self.key);
        let mut state = self.state.lock().unwrap();
        if at != state.1 {
            state.1 = at;
            match load(&self.cert, &self.key, &self.provider) {
                Ok(loaded) => {
                    state.0 = Arc::new(loaded);
                    eprintln!(
                        "layr-desk serve: loaded the certificate {}",
                        self.cert.display()
                    );
                }
                Err(e) => eprintln!("layr-desk serve: kept the old certificate: {e:#}"),
            }
        }
        state.0.clone()
    }
}

impl ResolvesServerCert for Reloading {
    fn resolve(&self, _hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

/// A TLS acceptor for HTTP/1.1 with this certificate.
pub fn acceptor(resolver: Arc<Reloading>) -> Result<tokio_rustls::TlsAcceptor> {
    let mut config = rustls::ServerConfig::builder_with_provider(resolver.provider.clone())
        .with_safe_default_protocol_versions()?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(tokio_rustls::TlsAcceptor::from(Arc::new(config)))
}

/// The ts.net name of this machine from `tailscale status --json`, without the final dot.
pub fn tailscale_name(status: &str) -> Result<String> {
    let status: serde_json::Value =
        serde_json::from_str(status).context("tailscale status is not JSON")?;
    let name = status["Self"]["DNSName"]
        .as_str()
        .unwrap_or_default()
        .trim_end_matches('.');
    if name.is_empty() {
        bail!("tailscale gives this machine no DNS name (MagicDNS off?)");
    }
    Ok(name.to_string())
}

/// The files that `tailscale cert <name>` writes in `dir`.
pub fn tailscale_files(dir: &Path) -> Result<(PathBuf, PathBuf)> {
    let output = std::process::Command::new("tailscale")
        .args(["status", "--json"])
        .output()
        .context("running tailscale status")?;
    if !output.status.success() {
        bail!(
            "tailscale status: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let name = tailscale_name(&String::from_utf8_lossy(&output.stdout))?;
    Ok((
        dir.join(format!("{name}.crt")),
        dir.join(format!("{name}.key")),
    ))
}
