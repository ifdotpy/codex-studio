//! The QUIC link between paired Studio servers (docs/peer-link.md).
//!
//! The backend starts this process and writes the configuration as the first stdin line. Later
//! lines replace the paired keys. The process exits when stdin closes.

mod frame;
mod inbound;
mod outbound;
mod tls;

use std::collections::{HashMap, HashSet};
use std::io::Write;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{Connection, Endpoint, TransportConfig, VarInt};
use rustls::crypto::CryptoProvider;
use rustls::sign::CertifiedKey;
use serde::Deserialize;
use tokio::io::{AsyncBufReadExt, BufReader};

const CONNECT_SECONDS: u64 = 5;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct Config {
    listen: SocketAddr,
    private_key: String,
    public_key: String,
    peers: Vec<String>,
    backend_port: u16,
    forwarded_host: String,
    socket: PathBuf,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Update {
    peers: Vec<String>,
}

/// The paired keys and the open connections. A connection whose key leaves the set closes.
pub struct Links {
    endpoint: Endpoint,
    provider: Arc<CryptoProvider>,
    key: Arc<CertifiedKey>,
    transport: Arc<TransportConfig>,
    keys: tls::PairedKeys,
    inbound: Mutex<Vec<Connection>>,
    outbound: tokio::sync::Mutex<HashMap<(SocketAddr, Vec<u8>), Connection>>,
}

fn peer_key(connection: &Connection) -> Option<Vec<u8>> {
    let identity = connection.peer_identity()?;
    let keys = identity
        .downcast::<Vec<rustls::pki_types::CertificateDer<'static>>>()
        .ok()?;
    keys.first().map(|key| key.as_ref().to_vec())
}

impl Links {
    fn track_inbound(&self, connection: &Connection) {
        if let Ok(mut connections) = self.inbound.lock() {
            connections.retain(|connection| connection.close_reason().is_none());
            connections.push(connection.clone());
        }
    }

    async fn connect(&self, address: SocketAddr, key: Vec<u8>) -> Result<Connection, String> {
        let paired = self
            .keys
            .read()
            .map(|keys| keys.contains(&key))
            .unwrap_or(false);
        if !paired {
            return Err("The server is not paired".to_owned());
        }
        let mut connections = self.outbound.lock().await;
        let slot = (address, key.clone());
        if let Some(connection) = connections
            .get(&slot)
            .filter(|connection| connection.close_reason().is_none())
        {
            return Ok(connection.clone());
        }
        let crypto = tls::client_config(self.provider.clone(), self.key.clone(), key)?;
        let crypto = QuicClientConfig::try_from(crypto).map_err(|error| error.to_string())?;
        let mut config = quinn::ClientConfig::new(Arc::new(crypto));
        config.transport_config(self.transport.clone());
        let connecting = self
            .endpoint
            .connect_with(config, address, "studio-peer")
            .map_err(|error| error.to_string())?;
        let connection = tokio::time::timeout(Duration::from_secs(CONNECT_SECONDS), connecting)
            .await
            .map_err(|_| "The QUIC handshake timed out".to_owned())?
            .map_err(|error| error.to_string())?;
        connections.insert(slot, connection.clone());
        Ok(connection)
    }

    async fn replace_keys(&self, keys: HashSet<Vec<u8>>) {
        let revoked =
            |connection: &Connection| peer_key(connection).is_none_or(|key| !keys.contains(&key));
        if let Ok(connections) = self.inbound.lock() {
            for connection in connections.iter().filter(|connection| revoked(connection)) {
                connection.close(VarInt::from_u32(2), b"revoked");
            }
        }
        self.outbound.lock().await.retain(|(_, key), connection| {
            if keys.contains(key) {
                return true;
            }
            connection.close(VarInt::from_u32(2), b"revoked");
            false
        });
        if let Ok(mut current) = self.keys.write() {
            *current = keys;
        }
    }
}

fn paired_keys(peers: &[String]) -> Result<HashSet<Vec<u8>>, String> {
    peers.iter().map(|peer| tls::spki(peer)).collect()
}

fn transport() -> Result<TransportConfig, String> {
    let mut transport = TransportConfig::default();
    transport
        .keep_alive_interval(Some(Duration::from_secs(10)))
        .max_idle_timeout(Some(
            Duration::from_secs(60)
                .try_into()
                .map_err(|_| "The idle timeout is invalid".to_owned())?,
        ))
        .stream_receive_window(VarInt::from_u32(16 * 1024 * 1024))
        .receive_window(VarInt::from_u32(64 * 1024 * 1024))
        .send_window(64 * 1024 * 1024)
        .max_concurrent_bidi_streams(VarInt::from_u32(128))
        .max_concurrent_uni_streams(VarInt::from_u32(0));
    Ok(transport)
}

fn valid_host(host: &str) -> bool {
    (1..=255).contains(&host.len())
        && host
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b".-:".contains(&byte))
}

async fn run() -> Result<(), String> {
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let first = lines
        .next_line()
        .await
        .map_err(|error| error.to_string())?
        .ok_or("The configuration is missing")?;
    let config: Config =
        serde_json::from_str(&first).map_err(|_| "The configuration is invalid".to_owned())?;
    let loopback = config.listen.ip().is_loopback();
    if !(inbound::tailnet(config.listen.ip()) || loopback) || !valid_host(&config.forwarded_host) {
        return Err("Listen on a Tailscale address and use the Serve host".to_owned());
    }
    let provider = tls::provider();
    let key = tls::certified_key(&provider, &config.private_key, &config.public_key)?;
    let keys: tls::PairedKeys = Arc::new(RwLock::new(paired_keys(&config.peers)?));
    let transport = Arc::new(transport()?);
    let crypto = tls::server_config(provider.clone(), key.clone(), keys.clone())?;
    let crypto = QuicServerConfig::try_from(crypto).map_err(|error| error.to_string())?;
    let mut server = quinn::ServerConfig::with_crypto(Arc::new(crypto));
    server.transport_config(transport.clone());
    let endpoint = Endpoint::server(server, config.listen).map_err(|error| error.to_string())?;
    let listener = outbound::bind(&config.socket).map_err(|error| error.to_string())?;
    let links = Arc::new(Links {
        endpoint: endpoint.clone(),
        provider,
        key,
        transport,
        keys,
        inbound: Mutex::new(Vec::new()),
        outbound: tokio::sync::Mutex::new(HashMap::new()),
    });
    let port = endpoint
        .local_addr()
        .map_err(|error| error.to_string())?
        .port();
    let inbound = Arc::new(inbound::Inbound {
        backend_port: config.backend_port,
        forwarded_host: config.forwarded_host,
        loopback,
    });
    tokio::spawn(inbound::serve(endpoint.clone(), inbound, links.clone()));
    tokio::spawn(outbound::listen(listener, links.clone()));
    {
        let mut stdout = std::io::stdout().lock();
        writeln!(
            stdout,
            "{}",
            serde_json::json!({"ready": true, "port": port})
        )
        .map_err(|error| error.to_string())?;
        stdout.flush().map_err(|error| error.to_string())?;
    }
    while let Some(line) = lines.next_line().await.map_err(|error| error.to_string())? {
        let update: Update =
            serde_json::from_str(&line).map_err(|_| "The key update is invalid".to_owned())?;
        links.replace_keys(paired_keys(&update.peers)?).await;
    }
    endpoint.close(VarInt::from_u32(0), b"exit");
    std::fs::remove_file(&config.socket).ok();
    Ok(())
}

fn main() -> ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(_) => return ExitCode::FAILURE,
    };
    match runtime.block_on(run()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            writeln!(std::io::stderr(), "studio-peer-link: {error}").ok();
            ExitCode::FAILURE
        }
    }
}
