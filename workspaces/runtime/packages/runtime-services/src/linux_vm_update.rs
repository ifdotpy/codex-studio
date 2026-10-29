//! In-place update of the VM guest files to the installed Studio build (docs/linux-vm-workspaces.md,
//! "Guest updates").
//!
//! The guest broker owns the rules (`vm-guest/guest_update.py`). The host sends the SHA-256 of
//! each guest file of this build, and the broker lists the files that differ. The host sends
//! only those files in parts, signs the manifest with the update key, and waits for the
//! broker's restarts. Every step has an exact request ID. A step with an uncertain result ends
//! the attempt; the next attempt plans again from what the guest reports, so no step repeats
//! on a guess. A file set that the broker cannot take in place is left to the reboot path.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::linux_vm_guest::{GuestError, GuestRpc};

use studio_runtime_schema::shared_values::GUEST_UPDATE_PART_BYTES;

const PLAN_TIMEOUT: Duration = Duration::from_secs(120);
const STAGE_TIMEOUT: Duration = Duration::from_secs(120);
/// The broker's install can rebuild layr (cargo, up to 20 minutes).
const APPLY_TIMEOUT: Duration = Duration::from_secs(1800);
const STATUS_TIMEOUT: Duration = Duration::from_secs(30);
const HEALTH_TIMEOUT: Duration = Duration::from_secs(60);
/// The broker restarts after its drain (at most 10 minutes) and the unit restarts.
const SETTLE_TIMEOUT: Duration = Duration::from_secs(900);
/// Wait between two `update.status` reads, in seconds.
pub const STATUS_POLL_SECONDS: u64 = 5;
/// The detail of a guest that has no update methods (provisioned by an older Studio).
const UNSUPPORTED: &str = "The method is not supported";
/// Provider states that keep an update waiting (`provider.list`).
const ACTIVE_PROVIDER_STATES: [&str; 3] = ["running", "starting", "lost"];
/// The skill files that the guest installs below `.agents/skills`.
const SKILL_FILES: [&str; 4] = [
    "codex-orchestrator/SKILL.md",
    "codex-subagent/SKILL.md",
    "codex-workspace/SKILL.md",
    "codex-workspace/references/layr.md",
];
/// Guest Python and unit files that the guest never installs.
const NOT_INSTALLED: [&str; 2] = ["workspace.py", "namespace_exec.py"];

/// The guest files of one packaged build, by path below `/opt/codex-studio`.
pub struct GuestPayload {
    files: BTreeMap<String, (String, Vec<u8>)>,
}

fn sha256(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

impl GuestPayload {
    /// The guest files of the packaged workspace folder `workspace` (`Resources/workspace`),
    /// or `None` for a folder without `vm/guest/install.sh` (a source checkout never
    /// updates a VM).
    pub fn read(workspace: &Path) -> io::Result<Option<Self>> {
        let guest = workspace.join("vm/guest");
        if !guest.join("install.sh").is_file() {
            return Ok(None);
        }
        let mut files = BTreeMap::new();
        let mut add = |name: String, path: &Path| -> io::Result<()> {
            let bytes = std::fs::read(path)?;
            files.insert(name, (sha256(&bytes), bytes));
            Ok(())
        };
        for entry in std::fs::read_dir(&guest)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            let installed = entry.file_type()?.is_file()
                && [".py", ".sh", ".service", ".apparmor"]
                    .iter()
                    .any(|suffix| name.ends_with(suffix))
                && !name.starts_with("test")
                && !NOT_INSTALLED.contains(&name.as_str());
            if installed {
                add(format!("vm/guest/{name}"), &entry.path())?;
            }
        }
        for source in ["layr", "layr-desk"] {
            let folder = workspace.join("vm").join(source);
            let Ok(manifest) = std::fs::read(folder.join("manifest.json")) else {
                continue;
            };
            let manifest: Value = serde_json::from_slice(&manifest).map_err(io::Error::other)?;
            let names = manifest["files"].as_object().into_iter().flatten();
            for name in names
                .map(|(name, _)| name.as_str())
                .chain(["manifest.json"])
            {
                if Path::new(name)
                    .components()
                    .any(|part| !matches!(part, std::path::Component::Normal(_)))
                {
                    return Err(io::Error::other("A vendored source path is invalid"));
                }
                add(format!("vm/{source}/{name}"), &folder.join(name))?;
            }
        }
        for name in SKILL_FILES {
            let path = workspace.join(".agents/skills").join(name);
            if path.is_file() {
                add(format!(".agents/skills/{name}"), &path)?;
            }
        }
        Ok(Some(Self { files }))
    }

    fn manifest(&self) -> Value {
        Value::Object(
            self.files
                .iter()
                .map(|(name, (digest, _))| (name.clone(), json!(digest)))
                .collect(),
        )
    }

    /// Identifies this file set in operation IDs.
    fn identity(&self) -> String {
        sha256(self.manifest().to_string().as_bytes())[..12].to_owned()
    }
}

/// How an attempt ended.
#[derive(Debug, PartialEq, Eq)]
pub enum GuestUpdate {
    /// The guest has the files of this build.
    Current,
    /// The broker wrote these files and finished its restarts.
    Updated(Vec<String>),
    /// A provider runs in the VM: try again at the next idle moment.
    Deferred,
    /// The guest cannot take this update in place (an older guest, or files that need a reboot).
    Unsupported(String),
    /// The broker stopped the update with a stage that is not `complete`. A later attempt
    /// plans again; the broker includes the unfinished files.
    Failed(String),
}

fn request_id(kind: &str, parts: &[&str]) -> String {
    format!("update-{kind}:{}", parts.join(":"))
}

fn signature(key: &str, operation: &str, manifest: &Value) -> String {
    use ring::hmac;
    // The broker signs the canonical JSON of the operation and the manifest: sorted keys,
    // no spaces. `serde_json` maps keep sorted keys.
    let body = json!({"operationId": operation, "files": manifest}).to_string();
    let tag = hmac::sign(
        &hmac::Key::new(hmac::HMAC_SHA256, key.as_bytes()),
        body.as_bytes(),
    );
    tag.as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

fn changed_files(plan: &Value) -> Vec<String> {
    plan["changed"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|name| name.as_str().map(str::to_owned))
        .collect()
}

fn plan(guest: &dyn GuestRpc, payload: &GuestPayload) -> Result<Value, GuestError> {
    // The payload record names what only provisioning installs. Provisioning is not part of
    // this host, so the record is empty and its mismatch reason is not read.
    guest.call(
        "update.plan",
        &json!({"files": payload.manifest(), "payload": {}}),
        None,
        PLAN_TIMEOUT,
    )
}

fn provider_active(guest: &dyn GuestRpc) -> Result<bool, GuestError> {
    let listed = guest.call("provider.list", &json!({}), None, STATUS_TIMEOUT)?;
    Ok(listed["providers"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|row| {
            row["state"]
                .as_str()
                .is_some_and(|state| ACTIVE_PROVIDER_STATES.contains(&state))
        }))
}

/// Update the guest to `payload` when it differs, and only when no provider runs. An error is
/// an uncertain or failed step; the next attempt plans again.
pub fn update_guest(
    guest: &dyn GuestRpc,
    payload: &GuestPayload,
    key: &str,
    operation: &str,
    poll: Duration,
) -> Result<GuestUpdate, GuestError> {
    let answer = match plan(guest, payload) {
        Err(error) if error.detail() == Some(UNSUPPORTED) => {
            return Ok(GuestUpdate::Unsupported(
                "The guest has no update methods".into(),
            ));
        }
        other => other?,
    };
    let changed = changed_files(&answer);
    if changed.is_empty() {
        return Ok(GuestUpdate::Current);
    }
    if !answer["reboot"].as_array().is_none_or(Vec::is_empty) {
        return Ok(GuestUpdate::Unsupported("The files need a reboot".into()));
    }
    if provider_active(guest)? {
        return Ok(GuestUpdate::Deferred);
    }
    let operation = format!("studio-{}-{operation}", payload.identity());
    let encoder = base64::engine::general_purpose::STANDARD;
    for name in &changed {
        let (digest, bytes) = payload.files.get(name).ok_or(GuestError::InvalidRequest)?;
        // An empty file still sends one empty part.
        let sent: Vec<&[u8]> = if bytes.is_empty() {
            vec![&[]]
        } else {
            bytes.chunks(GUEST_UPDATE_PART_BYTES).collect()
        };
        for (index, part) in sent.into_iter().enumerate() {
            let offset = index * GUEST_UPDATE_PART_BYTES;
            guest.call(
                "update.stage",
                &json!({"operationId": operation, "sha256": digest, "offset": offset,
                        "data": encoder.encode(part)}),
                Some(&request_id(
                    "stage",
                    &[&operation, digest, &offset.to_string()],
                )),
                STAGE_TIMEOUT,
            )?;
        }
    }
    let manifest = payload.manifest();
    let mut status = guest.call(
        "update.apply",
        &json!({"operationId": operation, "files": manifest,
                "signature": signature(key, &operation, &manifest)}),
        Some(&request_id("apply", &[&operation])),
        APPLY_TIMEOUT,
    )?;
    let deadline = Instant::now() + SETTLE_TIMEOUT;
    while !matches!(
        status["stage"].as_str(),
        Some("complete" | "restart_failed" | "install_failed")
    ) {
        if Instant::now() >= deadline {
            return Err(GuestError::Protocol {
                message: "The in-place update did not finish its restarts in time",
                uncertain: true,
            });
        }
        #[expect(
            clippy::disallowed_methods,
            reason = "the update runs on the dedicated VM thread"
        )]
        std::thread::sleep(poll);
        // The broker or the guest service restarts now: a failed read is read again.
        if let Ok(next) = guest.call(
            "update.status",
            &json!({"operationId": operation}),
            None,
            STATUS_TIMEOUT,
        ) {
            status = next;
        }
    }
    if status["stage"] != "complete" {
        return Ok(GuestUpdate::Failed(
            status["error"]
                .as_str()
                .or(status["stage"].as_str())
                .unwrap_or("unknown")
                .to_owned(),
        ));
    }
    let after = changed_files(&plan(guest, payload)?);
    let health = guest.call("layr.health", &json!({}), None, HEALTH_TIMEOUT)?;
    if !after.is_empty() || health["state"] != "ready" {
        return Err(GuestError::Protocol {
            message: "The guest has no updated files or no ready layr after the update",
            uncertain: true,
        });
    }
    Ok(GuestUpdate::Updated(changed))
}

#[cfg(test)]
mod tests;
