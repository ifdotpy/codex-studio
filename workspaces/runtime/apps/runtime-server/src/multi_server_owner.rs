//! Tailscale owner observations used by the signed paired-server ingress.
//! Mirrors Python's `codex_multi_server._owner_login` and `_peer_details`:
//! local ownership comes from `tailscale status --json`, while peer ownership
//! comes from `tailscale whois --json <forwarded address>` and is compared to
//! the optional Tailscale-User-Login header.

use serde_json::Value;
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use studio_runtime_services::multi_server_auth::{AccessError, OwnerObservation, refused};
use studio_runtime_services::multi_server_pairing::OwnerLookup;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT: usize = 1024 * 1024;

pub struct TailscaleOwnerLookup;

impl OwnerLookup for TailscaleOwnerLookup {
    fn observe(
        &self,
        address: &str,
        supplied_logins: &[String],
        _auto_pair_origin: Option<&str>,
    ) -> Result<(OwnerObservation, Option<Value>), AccessError> {
        let status = tailscale_json(&["status", "--json"])?;
        let whois = tailscale_json(&["whois", "--json", address])?;
        let owner = OwnerObservation::from_json(address, supplied_logins, &status, &whois)?;
        Ok((owner, None))
    }
}

fn tailscale_json(args: &[&str]) -> Result<Value, AccessError> {
    let mut child = Command::new("tailscale")
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| unavailable())?;
    let mut stdout = child.stdout.take().ok_or_else(unavailable)?;
    let (sender, receiver) = mpsc::sync_channel(1);
    let reader = thread::spawn(move || {
        let mut bytes = Vec::with_capacity(16 * 1024);
        let mut buffer = [0_u8; 8192];
        let mut too_large = false;
        loop {
            match stdout.read(&mut buffer) {
                Ok(0) => break,
                Ok(count) => {
                    let keep = count.min(MAX_OUTPUT.saturating_add(1).saturating_sub(bytes.len()));
                    bytes.extend_from_slice(&buffer[..keep]);
                    too_large |= bytes.len() > MAX_OUTPUT;
                }
                Err(_) => break,
            }
        }
        drop(sender.send((bytes, too_large)));
    });
    let deadline = Instant::now() + LOOKUP_TIMEOUT;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() < deadline => {
                thread::sleep(
                    Duration::from_millis(10)
                        .min(deadline.saturating_duration_since(Instant::now())),
                );
            }
            Ok(None) | Err(_) => {
                drop(child.kill());
                drop(child.wait());
                drop(reader);
                return Err(unavailable());
            }
        }
    };
    if !status.success() {
        drop(reader.join());
        return Err(unavailable());
    }
    let (bytes, too_large) = receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .map_err(|_| unavailable())?;
    reader.join().map_err(|_| unavailable())?;
    if too_large || Instant::now() >= deadline {
        return Err(unavailable());
    }
    serde_json::from_slice(&bytes).map_err(|_| unavailable())
}

fn unavailable() -> AccessError {
    refused(
        503,
        "identity_unavailable",
        "The Tailscale owner identity is unavailable",
    )
}
