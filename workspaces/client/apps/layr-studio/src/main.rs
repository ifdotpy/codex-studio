//! `layr-studio servers list [--json]` and `layr-studio servers forget <serverId>`.
use serde_json::{Value, json};
use std::path::PathBuf;

const USAGE: &str =
    "Usage: layr-studio servers list [--json] | layr-studio servers forget <serverId>";

/// Same profile folder as the app: CODEX_DESKTOP_PROFILE, else the app data folder.
fn profile() -> Result<PathBuf, String> {
    if let Some(path) = std::env::var_os("CODEX_DESKTOP_PROFILE").filter(|v| !v.is_empty()) {
        return Ok(PathBuf::from(path));
    }
    let home = std::env::var_os("HOME").ok_or("HOME is not set.")?;
    Ok(PathBuf::from(home).join("Library/Application Support/local.codex.agents"))
}

async fn run(args: &[String]) -> Result<String, String> {
    let words: Vec<&str> = args.iter().map(String::as_str).collect();
    let directory = profile()?;
    match words.as_slice() {
        ["servers", "list", rest @ ..] if rest.is_empty() || rest == ["--json"] => {
            let list =
                studio_client_core::operator::run(&directory, "servers.list", &json!({})).await?;
            if !rest.is_empty() {
                return Ok(list.to_string());
            }
            Ok(list
                .as_array()
                .into_iter()
                .flatten()
                .map(|s: &Value| {
                    format!(
                        "{}\t{}\t{}",
                        s["id"].as_str().unwrap_or(""),
                        s["origin"].as_str().unwrap_or(""),
                        s["label"].as_str().unwrap_or("")
                    )
                })
                .collect::<Vec<_>>()
                .join("\n"))
        }
        ["servers", "forget", id] => {
            studio_client_core::operator::run(
                &directory,
                "servers.forget",
                &json!({"serverId":id}),
            )
            .await?;
            Ok(format!("Forgot server {id}."))
        }
        _ => Err(USAGE.into()),
    }
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match run(&args).await {
        Ok(text) => {
            if !text.is_empty() {
                println!("{text}");
            }
        }
        Err(error) => {
            eprintln!("{error}");
            std::process::exit(1);
        }
    }
}
