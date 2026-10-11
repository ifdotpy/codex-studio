//! The launchd agent that starts the network disk at login and mounts it in Finder.

use anyhow::{bail, Result};
use clap::Subcommand;
use std::path::PathBuf;
use std::process::Command;

const LABEL: &str = "ws.layr.desk";

#[derive(Subcommand)]
pub enum Action {
    /// Write the agent and start it now
    Install {
        #[arg(long)]
        site: String,
        #[arg(long, default_value_t = 8095)]
        port: u16,
    },
    /// Stop the agent and remove it
    Remove,
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME")?))
}

fn plist() -> Result<PathBuf> {
    Ok(home()?.join(format!("Library/LaunchAgents/{LABEL}.plist")))
}

fn domain() -> Result<String> {
    use std::os::unix::fs::MetadataExt;
    Ok(format!("gui/{}", std::fs::metadata(home()?)?.uid()))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

pub fn run(action: Action) -> Result<()> {
    if !cfg!(target_os = "macos") {
        bail!("the disk service is for macOS");
    }
    let domain = domain()?;
    // A missing agent is not an error here.
    let _ = Command::new("launchctl")
        .args(["bootout", &format!("{domain}/{LABEL}")])
        .output();
    match action {
        Action::Install { site, port } => {
            let exe = std::env::current_exe()?.canonicalize()?;
            let log = home()?.join("Library/Logs/layr-desk.log");
            let at = home()?.join("Desk");
            let arguments = [
                exe.display().to_string(),
                "mount".into(),
                "--site".into(),
                site,
                "--port".into(),
                port.to_string(),
                "--at".into(),
                at.display().to_string(),
            ];
            let arguments: String = arguments
                .iter()
                .map(|a| format!("    <string>{}</string>\n", escape(a)))
                .collect();
            let text = format!(
                r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key>
  <string>{LABEL}</string>
  <key>ProgramArguments</key>
  <array>
{arguments}  </array>
  <key>RunAtLoad</key>
  <true/>
  <key>KeepAlive</key>
  <true/>
  <key>StandardOutPath</key>
  <string>{log}</string>
  <key>StandardErrorPath</key>
  <string>{log}</string>
</dict>
</plist>
"#,
                log = escape(&log.display().to_string())
            );
            let path = plist()?;
            std::fs::create_dir_all(path.parent().expect("a folder"))?;
            std::fs::write(&path, text)?;
            let status = Command::new("launchctl")
                .args(["bootstrap", &domain])
                .arg(&path)
                .status()?;
            if !status.success() {
                bail!("launchctl bootstrap failed");
            }
            println!(
                "the disk starts at login and is mounted at {} ({})",
                at.display(),
                path.display()
            );
        }
        Action::Remove => {
            // A disk that is not mounted is not an error here.
            let _ = Command::new("/sbin/umount")
                .arg(home()?.join("Desk"))
                .output();
            let path = plist()?;
            if path.exists() {
                std::fs::remove_file(&path)?;
            }
            println!("the disk no longer starts at login");
        }
    }
    Ok(())
}
