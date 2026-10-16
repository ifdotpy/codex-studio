//! Mac folder conflict resolution and configuration, using the Linux VM guest RPC boundary.
//!
//! Registered Python compatibility difference: after an uncertain `sync.mac.handoff`, Python
//! retries the same guest operation ID and relies on guest deduplication. Rust persists the
//! request intent and refuses to resend an unknown outcome; a completed saved receipt is used
//! to finish local writes after restart.

use std::fs::{self, File, OpenOptions};
use std::io::{Cursor, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use nix::fcntl::{Flock, FlockArg};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::{
    linux_vm_guest::{GuestError, GuestRpc},
    tar_gzip::TarGzipWriter,
};

const MAX_SYNC_FILE_BYTES: u64 = 256 * 1024 * 1024;
const HANDOFF_TIMEOUT: Duration = Duration::from_secs(600);
const READ_TIMEOUT: Duration = Duration::from_secs(120);
const ABSENT: &str = "absent";

#[derive(Debug, Error)]
pub enum MacSyncError {
    #[error("The project has no Mac folder to sync")]
    NoProject,
    #[error("Invalid project or path")]
    Invalid,
    #[error("A sync round of this project runs; try again")]
    Busy,
    #[error("This path has no conflict to settle: {0}")]
    NoConflict(String),
    #[error("The VM guest operation failed")]
    Guest(#[source] GuestError),
    #[error("The Mac sync handoff outcome is unknown and will not be resent")]
    HandoffUnknown,
    #[error("Mac sync state could not be read or written")]
    Io(#[source] std::io::Error),
    #[error("Mac sync state is invalid")]
    Json(#[source] serde_json::Error),
}

pub struct ResolveResult {
    pub response: Value,
    pub handoff_text: Option<String>,
    pub handoff_id: Option<String>,
}

pub fn configure(
    state_dir: &Path,
    project_id: &str,
    enabled: Option<bool>,
    allow_deletes: bool,
) -> Result<Value, MacSyncError> {
    let project = project_directory(state_dir, project_id)?;
    let source = project_record(&project)?["source"]
        .as_str()
        .ok_or(MacSyncError::NoProject)?
        .to_owned();
    let enabled_changed = enabled.is_some();
    let directory = project.join("sync");
    fs::create_dir_all(&directory).map_err(MacSyncError::Io)?;
    let config_path = directory.join("config.json");
    if let Some(enabled) = enabled {
        let mut config = read_json(&config_path, json!({}))?;
        config["enabled"] = Value::Bool(enabled);
        save_json(&config_path, &config)?;
    }
    let mut status = read_json(&directory.join("status.json"), json!({}))?;
    if allow_deletes {
        let count = status.get("deletes").and_then(Value::as_u64).unwrap_or(0);
        if count == 0 {
            return Err(MacSyncError::Invalid);
        }
        save_json(
            &directory.join("allow-deletes.json"),
            &json!({"count":count}),
        )?;
        status["state"] = json!("syncing");
        status["reason"] = Value::Null;
        status["at"] = json!(now_seconds());
    }
    if let Some(enabled) = enabled {
        if enabled {
            status["state"] = json!("syncing");
            status["reason"] = Value::Null;
        } else {
            status["state"] = json!("paused");
            status["reason"] = json!("Paused by the user");
        }
    }
    let enabled = read_json(&config_path, json!({}))?["enabled"]
        .as_bool()
        .unwrap_or(true);
    if enabled_changed {
        status["at"] = json!(now_seconds());
    }
    if enabled_changed || allow_deletes {
        save_json(&directory.join("status.json"), &status)?;
    }
    status["enabled"] = json!(enabled);
    status["folder"] = json!(source);
    status["conflicts"] = status.get("conflicts").cloned().unwrap_or(json!([]));
    status["choices"] = status.get("choices").cloned().unwrap_or(json!([]));
    status["withAgent"] = status.get("withAgent").cloned().unwrap_or(json!([]));
    status["deletes"] = status.get("deletes").cloned().unwrap_or(json!(0));
    status["state"] = status.get("state").cloned().unwrap_or(json!("syncing"));
    status["reason"] = status.get("reason").cloned().unwrap_or(Value::Null);
    status["at"] = status.get("at").cloned().unwrap_or(Value::Null);
    Ok(status_view(&status, &source, enabled))
}

pub fn resolve(
    state_dir: &Path,
    project_id: &str,
    rel: &str,
    choice: &str,
    guest: &dyn GuestRpc,
) -> Result<ResolveResult, MacSyncError> {
    if !matches!(choice, "mac" | "vm" | "agent") || !safe_relative_path(rel) {
        return Err(MacSyncError::Invalid);
    }
    let project = project_directory(state_dir, project_id)?;
    let project_record = project_record(&project)?;
    let root = PathBuf::from(
        project_record["source"]
            .as_str()
            .ok_or(MacSyncError::NoProject)?,
    );
    let root = fs::canonicalize(root).map_err(MacSyncError::Io)?;
    let directory = project.join("sync");
    fs::create_dir_all(&directory).map_err(MacSyncError::Io)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join("round.lock"))
        .map_err(MacSyncError::Io)?;
    let _lock = Flock::lock(lock, FlockArg::LockExclusiveNonblock).map_err(|(_file, error)| {
        if error == nix::errno::Errno::EWOULDBLOCK || error == nix::errno::Errno::EAGAIN {
            MacSyncError::Busy
        } else {
            MacSyncError::Io(std::io::Error::from_raw_os_error(error as i32))
        }
    })?;

    let manifest_path = directory.join("manifest.json");
    let held_path = directory.join("held.json");
    let status_path = directory.join("status.json");
    let intent_path = directory.join("handoff-intent.json");
    let mut manifest = read_json(&manifest_path, json!({}))?;
    let mut held = read_json(&held_path, json!({}))?;
    let mut status = read_json(&status_path, json!({}))?;
    let mut choices = status
        .get("choices")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if !held.get(rel).is_some_and(Value::is_object)
        && !choices.iter().any(|value| value.as_str() == Some(rel))
    {
        return Err(MacSyncError::NoConflict(rel.to_owned()));
    }
    let head = guest
        .call(
            "sync.mac.head",
            &json!({"projectId":project_id}),
            None,
            Duration::from_secs(30),
        )
        .map_err(MacSyncError::Guest)?;
    let main_state = head
        .get("mainStateId")
        .and_then(Value::as_str)
        .ok_or(MacSyncError::Invalid)?
        .to_owned();
    let (kind, data, mode) = fetch_entry(guest, project_id, &main_state, rel)?;
    let mut response = json!({"path":rel,"choice":choice,"mainStateId":main_state});
    let mut handoff_text = None;
    let mut handoff_id = None;
    match choice {
        "vm" => {
            write_mac(&root, rel, &kind, &data, mode)?;
            if kind == ABSENT {
                manifest
                    .as_object_mut()
                    .ok_or(MacSyncError::Invalid)?
                    .remove(rel);
            } else {
                let mut entry =
                    describe(&contained_path(&root, rel)?)?.ok_or(MacSyncError::Invalid)?;
                entry["b"] = json!(main_state);
                manifest[rel] = entry;
            }
            held.as_object_mut()
                .ok_or(MacSyncError::Invalid)?
                .remove(rel);
        }
        "mac" => {
            if kind == ABSENT {
                manifest
                    .as_object_mut()
                    .ok_or(MacSyncError::Invalid)?
                    .remove(rel);
            } else {
                manifest[rel] =
                    json!({"k":kind,"s":-1,"m":0,"x":mode,"h":hex_sha(&data),"b":main_state});
            }
            held.as_object_mut()
                .ok_or(MacSyncError::Invalid)?
                .remove(rel);
        }
        "agent" => {
            let path = contained_path(&root, rel)?;
            let local = describe(&path)?;
            let content_hash = local
                .as_ref()
                .map(|item| item["h"].as_str().unwrap_or(ABSENT).to_owned())
                .unwrap_or_else(|| ABSENT.to_owned());
            let kind = local
                .as_ref()
                .map(|item| item["k"].as_str().unwrap_or(ABSENT).to_owned())
                .unwrap_or_else(|| ABSENT.to_owned());
            if held[rel]["why"] == "agent"
                && held[rel]["h"] == content_hash
                && held[rel]["stateId"].is_string()
            {
                let saved_line = held[rel]["line"].as_str().unwrap_or("mac-conflict");
                let saved_state = held[rel]["stateId"].as_str().unwrap_or_default();
                response["line"] = json!(saved_line);
                response["stateId"] = json!(saved_state);
                response["kind"] = json!(kind);
                handoff_id = Some(format!(
                    "mac-conflict:{saved_state}:{}",
                    &hex_sha(rel.as_bytes())[..16]
                ));
                handoff_text = Some(handoff_message(rel, saved_state, saved_line, &kind));
            } else {
                let operation = format!(
                    "mac-{}",
                    hex_sha(
                        format!("{project_id}\0{rel}\0{content_hash}\0{main_state}").as_bytes()
                    )[..32]
                        .to_owned()
                );
                let mut params =
                    json!({"projectId":project_id,"operationId":operation,"path":rel,"kind":kind});
                let prior_intent = read_json(&intent_path, Value::Null)?;
                if matches!(prior_intent["state"].as_str(), Some("unknown" | "sending")) {
                    return Err(MacSyncError::HandoffUnknown);
                }
                let prior_result = (prior_intent["operationId"] == operation
                    && prior_intent["state"] == "completed")
                    .then(|| prior_intent["result"].clone());
                let result = if let Some(result) = prior_result {
                    result
                } else {
                    save_json(
                        &intent_path,
                        &json!({"operationId":operation,"state":"sending"}),
                    )?;
                    let sent = (|| {
                        if let Some(entry) = local {
                            let bytes = read_sync_file(&path, &kind)?;
                            let archive =
                                directory.join(format!("handoff-{}.tar.gz", &operation[4..28]));
                            if !archive.exists() {
                                make_archive(
                                    &archive,
                                    rel,
                                    &kind,
                                    &bytes,
                                    entry["x"].as_u64().unwrap_or(0) as u32,
                                    &path,
                                )?;
                            }
                            upload_archive(guest, &archive, &operation)?;
                            params["upload"] = json!(format!(
                                "/var/lib/codex-studio/projects/mac-sync-{}",
                                &operation[4..28]
                            ));
                        }
                        guest
                            .call(
                                "sync.mac.handoff",
                                &params,
                                Some(&operation),
                                HANDOFF_TIMEOUT,
                            )
                            .map_err(MacSyncError::Guest)
                    })();
                    match sent {
                        Ok(result) => {
                            save_json(
                                &intent_path,
                                &json!({"operationId":operation,"state":"completed","result":result}),
                            )?;
                            result
                        }
                        Err(MacSyncError::Guest(error)) => {
                            save_json(
                                &intent_path,
                                &json!({"operationId":operation,"state":if error.uncertain() {"unknown"} else {"not_sent"}}),
                            )?;
                            return Err(MacSyncError::Guest(error));
                        }
                        Err(error) => {
                            save_json(
                                &intent_path,
                                &json!({"operationId":operation,"state":"not_sent"}),
                            )?;
                            return Err(error);
                        }
                    }
                };
                let line = result["line"]
                    .as_str()
                    .ok_or(MacSyncError::Invalid)?
                    .to_owned();
                let state_id = result["stateId"]
                    .as_str()
                    .ok_or(MacSyncError::Invalid)?
                    .to_owned();
                held[rel] = json!({"h":content_hash,"why":"agent","base":result["mainStateId"],"stateId":state_id});
                response["line"] = json!(line);
                response["stateId"] = json!(state_id);
                response["kind"] = json!(kind);
                handoff_id = Some(format!(
                    "mac-conflict:{state_id}:{}",
                    &hex_sha(rel.as_bytes())[..16]
                ));
                handoff_text = Some(handoff_message(rel, &state_id, &line, &kind));
            }
        }
        _ => unreachable!(),
    }
    choices.retain(|value| value.as_str() != Some(rel));
    let held_object = held.as_object().ok_or(MacSyncError::Invalid)?;
    let conflicts = held_object
        .iter()
        .filter_map(|(path, value)| (value["why"] == "conflict").then_some(path.clone()))
        .collect::<Vec<_>>();
    let with_agent = held_object
        .iter()
        .filter_map(|(path, value)| (value["why"] == "agent").then_some(path.clone()))
        .collect::<Vec<_>>();
    let mut rescan = status
        .get("rescan")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    if choice == "mac" && !rescan.iter().any(|value| value.as_str() == Some(rel)) {
        rescan.push(json!(rel));
    }
    status["choices"] = json!(choices.iter().filter_map(Value::as_str).collect::<Vec<_>>());
    status["conflicts"] = json!(conflicts);
    status["withAgent"] = json!(with_agent);
    status["rescan"] = json!(rescan);
    status["state"] = json!(
        if !conflicts.is_empty() || choices.iter().any(Value::is_string) {
            "conflict"
        } else if !with_agent.is_empty() {
            "waiting"
        } else {
            "synced"
        }
    );
    status["at"] = json!(now_seconds());
    save_json(&manifest_path, &manifest)?;
    save_json(&held_path, &held)?;
    save_json(&status_path, &status)?;
    if choice == "agent" {
        match fs::remove_file(&intent_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(MacSyncError::Io(error)),
        }
    }
    let source = project_record["source"]
        .as_str()
        .ok_or(MacSyncError::NoProject)?;
    let enabled = read_json(&directory.join("config.json"), json!({}))?["enabled"]
        .as_bool()
        .unwrap_or(true);
    response["status"] = status_view(&status, source, enabled);
    Ok(ResolveResult {
        response,
        handoff_text,
        handoff_id,
    })
}

fn status_view(status: &Value, source: &str, enabled: bool) -> Value {
    let state = if !enabled {
        "paused"
    } else {
        status["state"].as_str().unwrap_or("syncing")
    };
    json!({
        "state":state,
        "folder":source,
        "enabled":enabled,
        "conflicts":status.get("conflicts").cloned().filter(Value::is_array).unwrap_or(json!([])),
        "choices":status.get("choices").cloned().filter(Value::is_array).unwrap_or(json!([])),
        "withAgent":status.get("withAgent").cloned().filter(Value::is_array).unwrap_or(json!([])),
        "deletes":status.get("deletes").and_then(Value::as_u64).unwrap_or(0),
        "reason":status.get("reason").cloned().unwrap_or(Value::Null),
        "at":status.get("at").cloned().unwrap_or(Value::Null),
    })
}

fn handoff_message(rel: &str, state: &str, line: &str, kind: &str) -> String {
    let mac = if kind == ABSENT {
        "The Mac deleted this file.".to_owned()
    } else {
        format!("Read the Mac version with `layr show {state}:{rel}`.")
    };
    format!(
        "[Studio Mac sync] The user's Mac folder and main changed {rel} in different ways. The user asked you to combine the two versions. The Mac version is layr state {state} in the line {line}. {mac} Put one version that keeps both changes in main and commit it. Studio then writes main's file to the Mac folder."
    )
}

fn fetch_entry(
    guest: &dyn GuestRpc,
    project: &str,
    state: &str,
    rel: &str,
) -> Result<(String, Vec<u8>, u32), MacSyncError> {
    let mut bytes = Vec::new();
    let mut offset = 0usize;
    let (kind, mode) = loop {
        let chunk = guest
            .call(
                "sync.mac.read",
                &json!({"projectId":project,"stateId":state,"path":rel,"offset":offset}),
                None,
                READ_TIMEOUT,
            )
            .map_err(MacSyncError::Guest)?;
        let kind = chunk["kind"]
            .as_str()
            .ok_or(MacSyncError::Invalid)?
            .to_owned();
        if kind == ABSENT {
            return Ok((kind, Vec::new(), 0));
        }
        let mode = chunk["mode"].as_u64().unwrap_or(0) as u32;
        let encoded = chunk["data"].as_str().ok_or(MacSyncError::Invalid)?;
        let part = BASE64.decode(encoded).map_err(|_| MacSyncError::Invalid)?;
        if bytes.len().saturating_add(part.len()) as u64 > MAX_SYNC_FILE_BYTES {
            return Err(MacSyncError::Invalid);
        }
        offset += part.len();
        bytes.extend(part);
        if chunk["eof"].as_bool() == Some(true) {
            break (kind, mode);
        }
    };
    Ok((kind, bytes, mode))
}

fn project_directory(state_dir: &Path, project_id: &str) -> Result<PathBuf, MacSyncError> {
    if project_id.is_empty()
        || project_id.len() > 100
        || !project_id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
    {
        return Err(MacSyncError::Invalid);
    }
    let project = state_dir.join("layr-projects").join(project_id);
    if !project.join("project.json").is_file() {
        return Err(MacSyncError::NoProject);
    }
    Ok(project)
}

fn project_record(project: &Path) -> Result<Value, MacSyncError> {
    let record = read_json(&project.join("project.json"), Value::Null)?;
    if !record["source"].is_string() {
        return Err(MacSyncError::NoProject);
    }
    Ok(record)
}

fn safe_relative_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= 4096
        && Path::new(path)
            .components()
            .all(|component| matches!(component, Component::Normal(_)))
}

fn contained_path(root: &Path, rel: &str) -> Result<PathBuf, MacSyncError> {
    if !safe_relative_path(rel) {
        return Err(MacSyncError::Invalid);
    }
    let path = root.join(rel);
    let mut ancestor = path.parent().ok_or(MacSyncError::Invalid)?;
    while !ancestor.exists() {
        ancestor = ancestor.parent().ok_or(MacSyncError::Invalid)?;
    }
    let resolved = fs::canonicalize(ancestor).map_err(MacSyncError::Io)?;
    if !resolved.starts_with(root) {
        return Err(MacSyncError::Invalid);
    }
    Ok(path)
}

fn describe(path: &Path) -> Result<Option<Value>, MacSyncError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(MacSyncError::Io(error)),
    };
    let modified = meta
        .modified()
        .ok()
        .and_then(|value| value.duration_since(UNIX_EPOCH).ok())
        .map_or(0, |value| value.as_nanos() as i64);
    if meta.file_type().is_symlink() {
        let target = fs::read_link(path).map_err(MacSyncError::Io)?;
        let bytes = target.to_string_lossy().as_bytes().to_vec();
        return Ok(Some(
            json!({"k":"link","s":bytes.len(),"m":modified,"x":0,"h":hex_sha(&bytes)}),
        ));
    }
    if !meta.is_file() {
        return Err(MacSyncError::Invalid);
    }
    if meta.len() > MAX_SYNC_FILE_BYTES {
        return Err(MacSyncError::Invalid);
    }
    let bytes = fs::read(path).map_err(MacSyncError::Io)?;
    #[cfg(unix)]
    let mode = {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o777
    };
    #[cfg(not(unix))]
    let mode = 0o644;
    Ok(Some(
        json!({"k":"file","s":meta.len(),"m":modified,"x":mode,"h":hex_sha(&bytes)}),
    ))
}

fn read_sync_file(path: &Path, kind: &str) -> Result<Vec<u8>, MacSyncError> {
    if kind == "link" {
        return fs::read_link(path)
            .map(|value| value.to_string_lossy().as_bytes().to_vec())
            .map_err(MacSyncError::Io);
    }
    fs::read(path).map_err(MacSyncError::Io)
}

fn write_mac(
    root: &Path,
    rel: &str,
    kind: &str,
    bytes: &[u8],
    mode: u32,
) -> Result<(), MacSyncError> {
    let path = contained_path(root, rel)?;
    let parent = path.parent().ok_or(MacSyncError::Invalid)?;
    fs::create_dir_all(parent).map_err(MacSyncError::Io)?;
    if kind == ABSENT {
        match fs::remove_file(&path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(MacSyncError::Io(error)),
        }
        return Ok(());
    }
    let temp = path.with_file_name(format!(
        ".{}.sync-{}",
        path.file_name()
            .and_then(|s| s.to_str())
            .ok_or(MacSyncError::Invalid)?,
        std::process::id()
    ));
    if kind == "link" {
        let target = std::str::from_utf8(bytes).map_err(|_| MacSyncError::Invalid)?;
        #[cfg(unix)]
        std::os::unix::fs::symlink(target, &temp).map_err(MacSyncError::Io)?;
        #[cfg(not(unix))]
        return Err(MacSyncError::Invalid);
    } else {
        let mut file = File::create(&temp).map_err(MacSyncError::Io)?;
        file.write_all(bytes).map_err(MacSyncError::Io)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(mode & 0o777))
                .map_err(MacSyncError::Io)?;
        }
    }
    fs::rename(temp, path).map_err(MacSyncError::Io)
}

fn make_archive(
    path: &Path,
    rel: &str,
    kind: &str,
    bytes: &[u8],
    mode: u32,
    original: &Path,
) -> Result<(), MacSyncError> {
    let raw = File::create(path).map_err(MacSyncError::Io)?;
    let mut gzip = TarGzipWriter::new(raw);
    if kind == "link" {
        let target = fs::read_link(original).map_err(MacSyncError::Io)?;
        let target = target.to_string_lossy();
        gzip.append_symlink(rel, 0o777, &target)
            .map_err(MacSyncError::Io)?;
    } else {
        let mut input = Cursor::new(bytes);
        gzip.append_file(rel, mode, bytes.len() as u64, &mut input)
            .map_err(MacSyncError::Io)?;
    }
    let _raw = gzip.finish().map_err(MacSyncError::Io)?;
    Ok(())
}

fn upload_archive(guest: &dyn GuestRpc, path: &Path, operation: &str) -> Result<(), MacSyncError> {
    let mut archive = File::open(path).map_err(MacSyncError::Io)?;
    let mut bytes = Vec::new();
    archive.read_to_end(&mut bytes).map_err(MacSyncError::Io)?;
    let upload_id = &operation[4..36];
    let root = format!(
        "/var/lib/codex-studio/projects/mac-sync-{}",
        &operation[4..28]
    );
    let begin = json!({"uploadId":upload_id,"root":root,"totalBytes":bytes.len(),"sha256":hex_sha(&bytes),"mode":"full","deletePaths":[]});
    guest
        .call(
            "upload.begin",
            &begin,
            Some(&format!("{operation}:begin")),
            Duration::from_secs(30),
        )
        .map_err(MacSyncError::Guest)?;
    for (seq, part) in bytes.chunks(256 * 1024).enumerate() {
        let params = json!({"uploadId":upload_id,"seq":seq,"data":BASE64.encode(part)});
        guest
            .call(
                "upload.chunk",
                &params,
                Some(&format!("{operation}:chunk:{seq}")),
                Duration::from_secs(30),
            )
            .map_err(MacSyncError::Guest)?;
    }
    guest
        .call(
            "upload.commit",
            &json!({"uploadId":upload_id}),
            Some(&format!("{operation}:upload")),
            Duration::from_secs(1810),
        )
        .map_err(MacSyncError::Guest)?;
    Ok(())
}

fn read_json(path: &Path, default: Value) -> Result<Value, MacSyncError> {
    match fs::read(path) {
        Ok(bytes) => serde_json::from_slice(&bytes).map_err(MacSyncError::Json),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(default),
        Err(error) => Err(MacSyncError::Io(error)),
    }
}
fn save_json(path: &Path, value: &Value) -> Result<(), MacSyncError> {
    let bytes = serde_json::to_vec(value).map_err(MacSyncError::Json)?;
    let temp = path.with_extension(format!("tmp-{}", std::process::id()));
    fs::write(&temp, bytes).map_err(MacSyncError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temp, fs::Permissions::from_mode(0o600)).map_err(MacSyncError::Io)?;
    }
    fs::rename(temp, path).map_err(MacSyncError::Io)
}
fn hex_sha(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(bytes))
}
fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0.0, |value| value.as_secs_f64())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    #[derive(Default)]
    struct FakeGuest {
        calls: Mutex<Vec<(String, Value, Option<String>)>>,
        unknown_handoff: bool,
    }

    impl GuestRpc for FakeGuest {
        fn stream(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            _timeout: Duration,
            _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
        ) -> Result<Value, GuestError> {
            self.calls
                .lock()
                .expect("fixture operation succeeds")
                .push((
                    method.to_owned(),
                    params.clone(),
                    request_id.map(str::to_owned),
                ));
            if method == "sync.mac.handoff" && self.unknown_handoff {
                return Err(GuestError::Transport {
                    message: "fake endpoint closed after write".to_owned(),
                    uncertain: true,
                });
            }
            Ok(match method {
                "sync.mac.head" => json!({"mainStateId":"main-1"}),
                "sync.mac.read" => {
                    json!({"kind":"file","mode":0o644,"data":BASE64.encode(b"guest version"),"eof":true})
                }
                "sync.mac.handoff" => {
                    json!({"line":"mac-conflict","stateId":"handoff-1","mainStateId":"main-1"})
                }
                _ => json!({"ok":true}),
            })
        }
    }

    fn setup() -> (tempfile::TempDir, PathBuf) {
        let state = tempfile::tempdir().expect("fixture operation succeeds");
        let project = state.path().join("layr-projects/p1");
        let folder = state.path().join("mac-folder");
        fs::create_dir_all(&project).expect("fixture operation succeeds");
        fs::create_dir_all(&folder).expect("fixture operation succeeds");
        fs::write(
            project.join("project.json"),
            json!({"projectId":"p1","source":folder}).to_string(),
        )
        .expect("fixture operation succeeds");
        fs::create_dir_all(project.join("sync")).expect("fixture operation succeeds");
        fs::write(folder.join("file.txt"), b"mac version").expect("fixture operation succeeds");
        fs::write(
            project.join("sync/held.json"),
            json!({"file.txt":{"h":"old","why":"conflict"}}).to_string(),
        )
        .expect("fixture operation succeeds");
        fs::write(project.join("sync/status.json"), json!({"state":"conflict","conflicts":["file.txt"],"choices":[],"withAgent":[],"deletes":0}).to_string()).expect("fixture operation succeeds");
        (state, folder)
    }

    #[test]
    fn vm_resolution_writes_the_guest_version_and_clears_the_conflict() {
        let (state, folder) = setup();
        let result = resolve(state.path(), "p1", "file.txt", "vm", &FakeGuest::default())
            .expect("fixture operation succeeds");
        assert_eq!(
            fs::read(folder.join("file.txt")).expect("fixture operation succeeds"),
            b"guest version"
        );
        assert_eq!(result.response["choice"], "vm");
        assert_eq!(result.response["status"]["state"], "synced");
        assert!(result.handoff_id.is_none());
    }

    #[test]
    fn agent_handoff_uses_a_stable_guest_operation_and_mac_conflict_identity() {
        let (state, _) = setup();
        let guest = FakeGuest::default();
        let result = resolve(state.path(), "p1", "file.txt", "agent", &guest)
            .expect("fixture operation succeeds");
        assert_eq!(result.response["line"], "mac-conflict");
        assert!(
            result
                .handoff_id
                .as_deref()
                .is_some_and(|id| id.starts_with("mac-conflict:handoff-1:"))
        );
        assert!(
            result
                .handoff_text
                .as_deref()
                .expect("fixture operation succeeds")
                .contains("layr state handoff-1")
        );
        let calls = guest.calls.lock().expect("fixture operation succeeds");
        let handoff = calls
            .iter()
            .find(|call| call.0 == "sync.mac.handoff")
            .expect("fixture operation succeeds");
        assert_eq!(handoff.2.as_deref(), handoff.1["operationId"].as_str());
        assert!(calls.iter().any(|call| call.0 == "upload.commit"));
        let identity = result
            .handoff_id
            .clone()
            .expect("fixture operation succeeds");
        drop(calls);
        let repeated = resolve(state.path(), "p1", "file.txt", "agent", &guest)
            .expect("fixture operation succeeds");
        assert_eq!(repeated.handoff_id.as_deref(), Some(identity.as_str()));
        assert_eq!(
            guest
                .calls
                .lock()
                .expect("fixture operation succeeds")
                .iter()
                .filter(|call| call.0 == "sync.mac.handoff")
                .count(),
            1
        );
    }

    #[test]
    fn unknown_handoff_is_durable_and_never_resent_after_restart() {
        let (state, _) = setup();
        let guest = FakeGuest {
            unknown_handoff: true,
            ..FakeGuest::default()
        };
        assert!(matches!(
            resolve(state.path(), "p1", "file.txt", "agent", &guest),
            Err(MacSyncError::Guest(error)) if error.uncertain()
        ));
        let calls_before_restart = guest
            .calls
            .lock()
            .expect("fixture operation succeeds")
            .iter()
            .filter(|call| call.0 == "sync.mac.handoff")
            .count();
        assert_eq!(calls_before_restart, 1);
        fs::write(
            state.path().join("mac-folder/file.txt"),
            b"changed after unknown",
        )
        .expect("fixture operation succeeds");
        assert!(matches!(
            resolve(state.path(), "p1", "file.txt", "agent", &guest),
            Err(MacSyncError::HandoffUnknown)
        ));
        assert_eq!(
            guest
                .calls
                .lock()
                .expect("fixture operation succeeds")
                .iter()
                .filter(|call| call.0 == "sync.mac.handoff")
                .count(),
            1
        );
    }

    #[test]
    fn prepared_intent_does_not_call_guest_and_completed_receipt_recovers_local_state() {
        let (state, _) = setup();
        let directory = state.path().join("layr-projects/p1/sync");
        save_json(
            &directory.join("handoff-intent.json"),
            &json!({"operationId":"unresolved","state":"sending"}),
        )
        .expect("fixture operation succeeds");
        let guest = FakeGuest::default();
        assert!(matches!(
            resolve(state.path(), "p1", "file.txt", "agent", &guest),
            Err(MacSyncError::HandoffUnknown)
        ));
        assert!(
            !guest
                .calls
                .lock()
                .expect("fixture operation succeeds")
                .iter()
                .any(|call| call.0 == "sync.mac.handoff")
        );

        let (state, _) = setup();
        let directory = state.path().join("layr-projects/p1/sync");
        let content = hex_sha(b"mac version");
        let operation = format!(
            "mac-{}",
            &hex_sha(format!("p1\0file.txt\0{content}\0main-1").as_bytes())[..32]
        );
        save_json(
            &directory.join("handoff-intent.json"),
            &json!({"operationId":operation,"state":"completed","result":{"line":"mac-conflict","stateId":"handoff-recovered","mainStateId":"main-1"}}),
        )
        .expect("fixture operation succeeds");
        let guest = FakeGuest::default();
        let result = resolve(state.path(), "p1", "file.txt", "agent", &guest)
            .expect("fixture operation succeeds");
        assert_eq!(result.response["stateId"], "handoff-recovered");
        assert_eq!(
            guest
                .calls
                .lock()
                .expect("fixture operation succeeds")
                .iter()
                .filter(|call| call.0 == "sync.mac.handoff")
                .count(),
            0
        );
        assert_eq!(
            read_json(&directory.join("held.json"), json!({})).expect("fixture operation succeeds")
                ["file.txt"]["stateId"],
            "handoff-recovered"
        );
    }

    #[test]
    fn config_refuses_unrecorded_projects_and_returns_the_public_status_shape() {
        let (state, folder) = setup();
        let status =
            configure(state.path(), "p1", Some(false), false).expect("fixture operation succeeds");
        assert_eq!(status["state"], "paused");
        assert_eq!(status["enabled"], false);
        assert_eq!(status["folder"], folder.to_string_lossy().as_ref());
        assert_eq!(
            status
                .as_object()
                .expect("fixture operation succeeds")
                .len(),
            9
        );
        assert!(matches!(
            configure(state.path(), "absent", None, false),
            Err(MacSyncError::NoProject)
        ));
    }

    #[cfg(unix)]
    #[test]
    fn mac_sync_archive_accepts_long_unicode_names_symlink_targets_and_modes() {
        use std::os::unix::fs::{PermissionsExt, symlink};

        let directory = tempfile::tempdir().expect("fixture operation succeeds");
        let source = directory.path().join("source.link");
        let target = format!("../{}", "target".repeat(24));
        symlink(&target, &source).expect("fixture operation succeeds");
        let relative = format!("目录/{}/café.link", "component".repeat(18));
        let archive_path = directory.path().join("link.tar.gz");
        make_archive(&archive_path, &relative, "link", &[], 0o777, &source)
            .expect("long link archive is serialized");
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
            File::open(&archive_path).expect("fixture operation succeeds"),
        ));
        let entry = archive
            .entries()
            .expect("archive entries")
            .next()
            .expect("symlink entry")
            .expect("valid symlink entry");
        assert_eq!(
            entry
                .path()
                .expect("symlink archive path")
                .to_string_lossy(),
            relative
        );
        assert!(entry.header().entry_type().is_symlink());
        assert_eq!(
            entry
                .link_name()
                .expect("symlink target field")
                .expect("symlink target")
                .to_string_lossy(),
            target
        );

        let file = directory.path().join("file.txt");
        fs::write(&file, b"payload").expect("fixture operation succeeds");
        fs::set_permissions(&file, fs::Permissions::from_mode(0o751))
            .expect("fixture operation succeeds");
        let archive_path = directory.path().join("file.tar.gz");
        make_archive(&archive_path, &relative, "file", b"payload", 0o751, &file)
            .expect("long file archive is serialized");
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(
            File::open(&archive_path).expect("fixture operation succeeds"),
        ));
        let entry = archive
            .entries()
            .expect("archive entries")
            .next()
            .expect("file entry")
            .expect("valid file entry");
        assert_eq!(
            entry.path().expect("file archive path").to_string_lossy(),
            relative
        );
        assert_eq!(entry.header().mode().expect("file mode"), 0o751);
    }
}
