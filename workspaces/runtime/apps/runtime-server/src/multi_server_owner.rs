//! Tailscale owner observations used by the signed paired-server ingress.
//! Mirrors Python's `codex_multi_server._owner_login` and `_peer_details`:
//! local ownership comes from `tailscale status --json`, while peer ownership
//! comes from `tailscale whois --json <forwarded address>` and is compared to
//! the optional Tailscale-User-Login header.

use futures_util::future::BoxFuture;
use serde_json::Value;
use std::{
    io::Read,
    process::{Command, Stdio},
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};
use studio_runtime_services::multi_server_auth::tailscale_observation::{
    Node, Observation, Profile, Status, object,
};
use studio_runtime_services::multi_server_auth::{AccessError, OwnerObservation, refused};
use studio_runtime_services::multi_server_pairing::OwnerLookup;

const LOOKUP_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_OUTPUT: usize = 1024 * 1024;

pub struct TailscaleOwnerLookup;

pub(crate) fn whoami_projection(raw: Option<&Value>, status: Option<&Value>) -> Value {
    let observation = raw.and_then(object::<Observation>);
    let user = observation
        .as_ref()
        .and_then(|v| v.profile.as_ref().or(v.user.as_ref()))
        .and_then(object::<Profile>)
        .and_then(|profile| profile.login.or(profile.name))
        .and_then(|login| {
            login
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_ascii_lowercase())
        });
    let node = observation
        .as_ref()
        .and_then(|v| v.local.as_ref().or(v.machine.as_ref()))
        .and_then(object::<Node>)
        .and_then(|node| node.id);
    let owner_login = status.and_then(object::<Status>).and_then(|status| {
        let local: Node = status.local.as_ref().and_then(object)?;
        let users = status.users.as_ref()?.as_object()?;
        let profile: Profile = users
            .get(&local.user_id.as_ref()?.to_string())
            .and_then(object)?;
        if status.backend.as_ref().and_then(Value::as_str) != Some("Running")
            || local
                .tags
                .as_ref()
                .and_then(Value::as_array)
                .is_some_and(|tags| !tags.is_empty())
        {
            return None;
        }
        profile.login.and_then(|login| {
            login
                .as_str()
                .filter(|s| !s.trim().is_empty() && s.len() <= 254)
                .map(|s| s.trim().to_ascii_lowercase())
        })
    });
    serde_json::json!({"status":if user.is_some() {"verified"} else {"unavailable"}, "user":user, "node":node, "ownerLogin":owner_login})
}

impl OwnerLookup for TailscaleOwnerLookup {
    fn observe<'a>(
        &'a self,
        address: &'a str,
        supplied_logins: &'a [String],
        _auto_pair_origin: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(OwnerObservation, Option<Value>), AccessError>> {
        let address = address.to_owned();
        let supplied_logins = supplied_logins.to_vec();
        Box::pin(async move {
            studio_runtime_process::blocking_pool::spawn_blocking(
                "runtime-server/multi-server-owner",
                move || {
                    let status = tailscale_json(&["status", "--json"])?;
                    let whois = tailscale_json(&["whois", "--json", &address])?;
                    let owner =
                        OwnerObservation::from_json(&address, &supplied_logins, &status, &whois)?;
                    Ok((owner, None))
                },
            )
            .await
            .map_err(|_| unavailable())?
        })
    }
}

pub(crate) fn tailscale_json(args: &[&str]) -> Result<Value, AccessError> {
    tailscale_json_from(std::path::Path::new("tailscale"), args)
}

#[allow(
    clippy::disallowed_methods,
    reason = "Tailscale observations run on labeled multi-server blocking workers"
)]
pub(crate) fn tailscale_json_from(
    executable: &std::path::Path,
    args: &[&str],
) -> Result<Value, AccessError> {
    let mut command = Command::new(executable);
    command
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) if executable == std::path::Path::new("tailscale") => {
            #[cfg(target_os = "macos")]
            {
                let bundled =
                    std::path::Path::new("/Applications/Tailscale.app/Contents/MacOS/Tailscale");
                if bundled.is_file() {
                    Command::new(bundled)
                        .args(args)
                        .stdin(Stdio::null())
                        .stdout(Stdio::piped())
                        .stderr(Stdio::null())
                        .spawn()
                        .map_err(|_| unavailable())?
                } else {
                    return Err(unavailable());
                }
            }
            #[cfg(not(target_os = "macos"))]
            {
                return Err(unavailable());
            }
        }
        Err(_) => return Err(unavailable()),
    };
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn whoami_projection_matches_legacy_aliases_and_policy() {
        fn legacy(raw: &Value, status: &Value) -> Value {
            let user = raw
                .get("UserProfile")
                .or_else(|| raw.get("User"))
                .and_then(|v| v.get("LoginName").or_else(|| v.get("Name")))
                .and_then(Value::as_str)
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.trim().to_ascii_lowercase());
            let node = raw
                .get("Self")
                .or_else(|| raw.get("Machine"))
                .and_then(|v| v.get("ID"))
                .cloned();
            let owner = (|| {
                let local = status.get("Self")?;
                let users = status.get("User")?.as_object()?;
                let profile = users.get(&local.get("UserID")?.to_string())?;
                if status.get("BackendState").and_then(Value::as_str) != Some("Running")
                    || local
                        .get("Tags")
                        .and_then(Value::as_array)
                        .is_some_and(|tags| !tags.is_empty())
                {
                    return None;
                }
                profile
                    .get("LoginName")
                    .and_then(Value::as_str)
                    .filter(|s| !s.trim().is_empty() && s.len() <= 254)
                    .map(|s| s.trim().to_ascii_lowercase())
            })();
            json!({"status":if user.is_some(){"verified"}else{"unavailable"},"user":user,"node":node,"ownerLogin":owner})
        }
        let raws = [
            json!({"UserProfile":{"LoginName":" ÄBCD "},"Self":{"ID":{"opaque":1}}}),
            json!({"UserProfile":null,"User":{"Name":"fallback"},"Self":null,"Machine":{"ID":1}}),
            json!({"User":{"Name":"User"},"Machine":{"ID":[1]}}),
            json!({"UserProfile":{"LoginName":false,"Name":"fallback"}}),
            json!([]),
        ];
        let statuses = [
            json!({"Self":{"UserID":1,"Tags":false},"User":{"1":{"LoginName":" ÄBCD "}},"BackendState":"Running"}),
            json!({"Self":{"UserID":"1","Tags":[]},"User":{"\"1\"":{"LoginName":"OWNER"}},"BackendState":"Running"}),
            json!({"Self":{"UserID":1,"Tags":["tag"]},"User":{"1":{"LoginName":"owner"}},"BackendState":"Running"}),
            json!({}),
        ];
        for raw in &raws {
            for status in &statuses {
                assert_eq!(
                    whoami_projection(Some(raw), Some(status)),
                    legacy(raw, status),
                    "{raw} {status}"
                );
            }
        }
    }
}
