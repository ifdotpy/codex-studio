use std::env;
use std::fs::File;
use std::io::Read;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use studio_runtime_http::{HttpState, router};
use studio_runtime_services::account_policy::AccountPolicyState;
use studio_runtime_store::{RuntimeStore, account_state};
use tokio::net::TcpListener;

const DEFAULT_PORT: u16 = 4620;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) == Some("state-check") {
        let state_dir = args
            .get(1)
            .ok_or("state-check requires a state directory")?;
        if args.len() != 2 {
            return Err("state-check accepts exactly one state directory".into());
        }
        let report =
            studio_runtime_store::check_state_readonly(PathBuf::from(state_dir).as_path())?;
        println!("{}", serde_json::to_string_pretty(&report)?);
        return Ok(());
    }
    let port = parse_port(args.into_iter())?;
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let address = listener.local_addr()?;
    let mut token = [0_u8; 32];
    getrandom::fill(&mut token).map_err(|error| std::io::Error::other(error.to_string()))?;
    let token = token
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let configured_state_dir = env::var_os("CODEX_AGENTS_STATE_DIR").map(PathBuf::from);
    let store = Arc::new(RuntimeStore::open(configured_state_dir.as_deref())?);
    let workspace_id = studio_runtime_store::SyncDocumentsRepository::workspace_id(&store.read()?)?;
    let state_root = store.root().canonicalize()?;
    let state_dir = state_root.to_string_lossy().into_owned();
    let registry_path = state_root.join("accounts/registry.json");
    let registry_file = match registry_path.canonicalize() {
        Ok(path) if !path.starts_with(&state_root) => {
            return Err("Cannot read the account registry outside the state directory".into());
        }
        Ok(path) => Some(File::open(path)?),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => {
            return Err(format!(
                "Cannot read the account registry at {}",
                registry_path.display()
            )
            .into());
        }
    };
    let registry_bytes = match registry_file {
        Some(file) => {
            let mut bytes = Vec::new();
            file.take((account_state::MAX_PERSISTED_DOCUMENT_BYTES + 1) as u64)
                .read_to_end(&mut bytes)?;
            Some(bytes)
        }
        None => None,
    };
    let limits_read = account_state::limits_snapshots(&store.read()?)?;
    let account_policy = Arc::new(
        AccountPolicyState::from_store(registry_bytes.as_deref(), limits_read).map_err(
            |error| {
                format!(
                    "Cannot read the account registry at {}: {error}",
                    registry_path.display()
                )
            },
        )?,
    );
    for diagnostic in account_policy.limits_diagnostics() {
        eprintln!(
            "invalid persisted limits at {}/{}/{} ({})",
            diagnostic.workspace_row,
            diagnostic.account_key,
            diagnostic.pointer,
            diagnostic.keyword
        );
    }
    let native_path = env::var("CODEX_BIN").ok();
    let public_origin = env::var("CODEX_CANVAS_PUBLIC_ORIGIN").ok();
    let app = router(HttpState::new(
        token,
        workspace_id,
        native_path,
        state_dir,
        address.port(),
        public_origin,
        Some(store),
        Some(account_policy),
    ))?;
    println!("Codex Canvas: http://{}:{}", address.ip(), address.port());
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .with_graceful_shutdown(shutdown_signal())
    .await?;
    Ok(())
}

fn parse_port(mut args: impl Iterator<Item = String>) -> Result<u16, Box<dyn std::error::Error>> {
    let mut port = DEFAULT_PORT;
    while let Some(argument) = args.next() {
        if argument == "--port" {
            port = args.next().ok_or("--port requires a number")?.parse()?;
        } else if let Some(value) = argument.strip_prefix("--port=") {
            port = value.parse()?;
        } else {
            return Err(format!("unknown argument: {argument}").into());
        }
    }
    Ok(port)
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut terminate) => tokio::select! {
                result = tokio::signal::ctrl_c() => { if let Err(error) = result { eprintln!("failed to listen for Ctrl-C: {error}"); } }
                _ = terminate.recv() => {}
            },
            Err(error) => {
                eprintln!("failed to listen for SIGTERM: {error}");
                if let Err(error) = tokio::signal::ctrl_c().await {
                    eprintln!("failed to listen for Ctrl-C: {error}");
                }
            }
        }
    }
    #[cfg(not(unix))]
    if let Err(error) = tokio::signal::ctrl_c().await {
        eprintln!("failed to listen for Ctrl-C: {error}");
    }
}
