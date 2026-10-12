use std::env;
use std::net::SocketAddr;

use studio_runtime_http::{HttpState, router};
use tokio::net::TcpListener;

const DEFAULT_PORT: u16 = 4620;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let port = parse_port(env::args().skip(1))?;
    let listener = TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], port))).await?;
    let address = listener.local_addr()?;
    let mut token = [0_u8; 32];
    getrandom::fill(&mut token).map_err(|error| std::io::Error::other(error.to_string()))?;
    let token = token
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let mut workspace = [0_u8; 16];
    getrandom::fill(&mut workspace).map_err(|error| std::io::Error::other(error.to_string()))?;
    let workspace_id = workspace
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let state_dir = env::var("CODEX_AGENTS_STATE_DIR").unwrap_or_default();
    let native_path = env::var("CODEX_BIN").ok();
    let public_origin = env::var("CODEX_CANVAS_PUBLIC_ORIGIN").ok();
    let app = router(HttpState::new(
        token,
        workspace_id,
        native_path,
        state_dir,
        address.port(),
        public_origin,
    ));
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
