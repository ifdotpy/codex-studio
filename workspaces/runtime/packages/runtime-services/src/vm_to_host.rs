//! Move an idle chat from the virtual machine to this device. The chat keeps its record,
//! transcript and provider thread. A lead moves with its workers; a worker moves alone
//! after its parent. One user action, one request ID, one transaction for the records.
//!
//! Files come first, outside SQLite, and every step is safe to repeat:
//! 1. The project folder: one Mac folder sync round, which transfers only the
//!    differences (docs/vm-mac-sync.md). The move needs the round to end with nothing left.
//! 2. The provider history of each chat: the Claude session file and the bridge session
//!    files, or the Codex rollout files. They are copied byte for byte and verified by hash.
//!
//! Limitation: work in a worker line that main does not contain stays in the VM, so the
//! move refuses until the lead merges or drops it. The move keeps no Layr line on this
//! device; moved chats work in the project folder.
use crate::event_write::{self, truthy};
use crate::linux_vm_guest::GuestRpc;
use crate::mac_sync::{MacSyncError, round::ProjectSync};
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use studio_runtime_store::{
    AgentId, AgentRecord, AgentRepository, RuntimeStore, StoreError, system_reads as records,
};

const READ_CHUNK_BYTES: u64 = 1024 * 1024;
const GUEST_TIMEOUT: Duration = Duration::from_secs(300);
/// Guest fields that name the VM workspace; a native chat has none of them.
const VM_FIELDS: &[&str] = &[
    "layrReady",
    "layrProjectId",
    "layrLine",
    "layrHome",
    "layrOwner",
    "layrStateId",
    "layrHandoffRoot",
    "layrTurnSaves",
    "layrReadCwd",
    "layrStoreRoot",
    "environment",
    // The cached role text names VM paths; the next turn writes the text for this device.
    "nativeRoleGuidance",
];

#[derive(Debug, thiserror::Error)]
pub enum MoveError {
    /// The user can fix the cause and try again.
    #[error("{0}")]
    Refused(String),
    #[error("The move did not finish: {0}")]
    Failed(String),
    #[error(transparent)]
    Store(#[from] StoreError),
}

type Result<T> = std::result::Result<T, MoveError>;

fn failed(text: impl Into<String>) -> MoveError {
    MoveError::Failed(text.into())
}

impl From<std::io::Error> for MoveError {
    fn from(error: std::io::Error) -> Self {
        failed(error.to_string())
    }
}

impl From<base64::DecodeError> for MoveError {
    fn from(error: base64::DecodeError) -> Self {
        failed(error.to_string())
    }
}

impl From<event_write::EventWriteError> for MoveError {
    fn from(error: event_write::EventWriteError) -> Self {
        failed(error.to_string())
    }
}

fn refused(text: impl Into<String>) -> MoveError {
    MoveError::Refused(text.into())
}

/// The project folder step. The production owner is [`MacFiles`].
pub trait ProjectFiles: Send + Sync {
    /// Bytes that [`ProjectFiles::bring`] writes to the folder.
    fn needed_bytes(&self, project_id: &str) -> Result<u64>;
    /// Make the folder equal to the VM main line, or refuse.
    fn bring(&self, project_id: &str) -> Result<()>;
}

/// Mac folder sync of one project (`vm-mac-sync.md`).
pub struct MacFiles {
    pub guest: Arc<dyn GuestRpc>,
    pub vm_state_dir: PathBuf,
}

impl MacFiles {
    fn open(&self, project_id: &str) -> Result<ProjectSync<'_>> {
        ProjectSync::open(self.guest.as_ref(), &self.vm_state_dir, project_id)
            .map_err(|error| sync_error(&error))
    }
}

fn sync_error(error: &MacSyncError) -> MoveError {
    MoveError::Refused(format!(
        "Folder sync is not ready: {}",
        crate::mac_sync::error_reason(error)
    ))
}

impl ProjectFiles for MacFiles {
    fn needed_bytes(&self, project_id: &str) -> Result<u64> {
        let plan = self
            .open(project_id)?
            .plan()
            .map_err(|error| sync_error(&error))?;
        Ok(plan["macWrites"]["bytes"].as_u64().unwrap_or(0))
    }

    fn bring(&self, project_id: &str) -> Result<()> {
        let sync = self.open(project_id)?;
        sync.round(None).map_err(|error| sync_error(&error))?;
        // The round must leave nothing to transfer, hold or decide.
        let plan = sync.plan().map_err(|error| sync_error(&error))?;
        let open: Vec<&str> = [
            "uploads",
            "deletionsToMain",
            "macWrites",
            "macDeletions",
            "conflicts",
            "held",
            "choices",
        ]
        .into_iter()
        .filter(|name| plan[*name]["count"].as_u64().unwrap_or(0) > 0)
        .collect();
        if !open.is_empty() || !plan["pending"].is_null() {
            return Err(refused(format!(
                "Folder sync did not settle ({}). Resolve it in the project menu, then move again",
                open.join(", ")
            )));
        }
        Ok(())
    }
}

/// Everything outside the records that the move uses.
pub struct Host {
    pub guest: Arc<dyn GuestRpc>,
    pub files: Arc<dyn ProjectFiles>,
    /// The Studio state directory: the Claude bridge keeps its sessions below it.
    pub state_dir: PathBuf,
    /// Rows of the account listing; each gives the host profile of an account key.
    pub accounts: Vec<crate::account_policy::PublicAccount>,
    pub free_bytes: fn(&Path) -> Option<u64>,
}

/// The directory of the provider history of an account row on this device.
#[must_use]
pub fn profile_dir(account: &crate::account_policy::PublicAccount) -> Option<PathBuf> {
    let text = |value: Option<&str>| value.filter(|text| !text.is_empty()).map(PathBuf::from);
    if account.provider.as_str() == Some("claude") {
        text(
            account
                .claude_options
                .as_ref()
                .and_then(|options| options.config_dir.as_str()),
        )
        .or_else(|| std::env::var_os("CLAUDE_CONFIG_DIR").map(PathBuf::from))
        .or_else(|| std::env::var_os("HOME").map(|home| Path::new(&home).join(".claude")))
    } else {
        text(account.home.as_str())
    }
}

struct Plan {
    /// The chats that move, parents first.
    members: Vec<Value>,
    host_path: String,
    /// Set for a lead: its project is brought to the folder.
    project_id: Option<String>,
}

use studio_runtime_schema::persistence::VmToHostReply;

/// Move one chat tree. A repeat with the same request ID returns the saved result.
pub async fn move_to_host(
    store: &RuntimeStore,
    host: &Arc<Host>,
    agent_id: &str,
    request_id: &str,
) -> Result<VmToHostReply> {
    if request_id.is_empty() || request_id.len() > 200 {
        return Err(refused("Supply a request ID"));
    }
    let key = format!("vm-to-host:{request_id}");
    let root = request_id.to_owned();
    let signature =
        event_write::request_signature(&json!({"operation":"vm-to-host","id":agent_id}));
    let saved = replay(store, &key, request_id, &signature).await?;
    if let Some(result) = saved {
        return Ok(result);
    }
    let id = agent_id.to_owned();
    let plan = store
        .read_async("runtime-services/vm-to-host", move |tx| admit(tx, &id))
        .await?;
    let members = plan.members.clone();
    let (host_path, project_id) = (plan.host_path.clone(), plan.project_id.clone());
    let tag = event_write::request_signature(&json!(request_id));
    let worker = Arc::clone(host);
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/vm-to-host",
        move || copy_everything(&worker, &members, &host_path, project_id.as_deref(), &tag),
    )
    .await
    .map_err(|_| failed("worker stopped"))??;
    let id = agent_id.to_owned();
    store
        .transaction_with_error(move |tx| {
            let admitted = match event_write::begin_request::<VmToHostReply>(
                tx,
                event_write::RequestKey::new("vm-to-host", &["owner-api"], &root).raw(&key),
                &signature,
                chrono::Utc::now().timestamp_millis(),
            )
            .map_err(|error| receipt_error(&error))?
            {
                event_write::CallerAdmission::Fresh(admitted) => admitted,
                event_write::CallerAdmission::Replay(result) => {
                    return Ok(result);
                }
            };
            let current = admit(tx, &id)?;
            if current.host_path != plan.host_path || !same_chats(&current.members, &plan.members) {
                return Err(refused("The chat changed during the move. Try again"));
            }
            let mut moved = Vec::new();
            for mut record in current.members {
                relocate(&mut record, &plan.host_path);
                let agent = AgentRecord::from_value(record.clone()).map_err(StoreError::from)?;
                AgentRepository::put(tx, &agent)?;
                crate::agents_create::publish_agent(tx, &record)?;
                moved.push(agent.id().0.clone());
            }
            let result = VmToHostReply {
                id,
                execution_mode: "native".into(),
                workspace_mode: "shared".into(),
                cwd: plan.host_path,
                moved_agents: moved,
            };
            event_write::settle_request(tx, &admitted, Some("owner-api"), &result)
                .map_err(|error| receipt_error(&error))?;
            Ok(result)
        })
        .await
}

fn receipt_error(error: &studio_runtime_store::request_results::RequestResultError) -> MoveError {
    if matches!(
        error,
        studio_runtime_store::request_results::RequestResultError::Conflict { .. }
    ) {
        refused("This request ID belongs to another move")
    } else {
        failed(error.to_string())
    }
}

async fn replay(
    store: &RuntimeStore,
    key: &str,
    root: &str,
    signature: &str,
) -> Result<Option<VmToHostReply>> {
    let (key, root, signature) = (key.to_owned(), root.to_owned(), signature.to_owned());
    store
        .read_async("runtime-services/vm-to-host", move |tx| {
            event_write::saved_request::<VmToHostReply>(
                tx,
                event_write::RequestKey::new("vm-to-host", &["owner-api"], &root).raw(&key),
                &signature,
                chrono::Utc::now().timestamp_millis(),
            )
            .map_err(|error| receipt_error(&error))
        })
        .await
}

/// The same chats at the same epoch, thread and account.
fn same_chats(left: &[Value], right: &[Value]) -> bool {
    left.len() == right.len()
        && left.iter().zip(right).all(|(a, b)| {
            ["id", "epoch", "threadId", "accountKey", "executionMode"]
                .iter()
                .all(|key| a[*key] == b[*key])
        })
}

fn relocate(record: &mut Value, host_path: &str) {
    if let Some(object) = record.as_object_mut() {
        for key in VM_FIELDS {
            object.remove(*key);
        }
    }
    record["executionMode"] = json!("native");
    record["workspaceMode"] = json!("shared");
    record["cwd"] = json!(host_path);
    record["hostProjectPath"] = json!(host_path);
    record["worktree"] = json!(false);
    record["worktreeReady"] = json!(false);
}

/// Read the chat tree and refuse unless it can move now.
fn admit(tx: &studio_runtime_store::Transaction<'_>, id: &str) -> Result<Plan> {
    let get = |id: &str| -> Result<Value> {
        Ok(AgentRepository::get(tx, &AgentId::from(id))?
            .ok_or_else(|| refused(format!("Unknown chat {id}")))?
            .value())
    };
    let record = get(id)?;
    if truthy(record.get("deletedAt")) {
        return Err(refused("The chat was deleted"));
    }
    if record["executionMode"] != "vm" {
        return Err(refused(
            "Only a chat that runs in the virtual machine can move",
        ));
    }
    let lead = record["isLead"] == true;
    let all = records::all(tx, "runtime_agents")?;
    let parent_of = |agent: &Value| agent["parentId"].as_str().map(str::to_owned);
    let (tree, host_path): (Vec<Value>, String) = if lead {
        let tree = all
            .into_iter()
            .filter(|agent| agent["rootId"] == record["id"] && !truthy(agent.get("deletedAt")))
            .collect();
        let path = record["hostProjectPath"]
            .as_str()
            .unwrap_or_default()
            .to_owned();
        (tree, path)
    } else {
        let parent = get(parent_of(&record).as_deref().unwrap_or_default())?;
        if parent["executionMode"] == "vm" {
            return Err(refused("Move the lead chat first"));
        }
        let path = parent["cwd"].as_str().unwrap_or_default().to_owned();
        let tree = all
            .into_iter()
            .filter(|agent| {
                !truthy(agent.get("deletedAt"))
                    && (agent["id"] == record["id"] || parent_of(agent).as_deref() == Some(id))
            })
            .collect();
        (tree, path)
    };
    if host_path.is_empty() || !Path::new(&host_path).is_dir() {
        return Err(refused("The project folder is missing on this device"));
    }
    crate::peer_conversion::idle(tx, &tree, false).map_err(|error| refused(error.to_string()))?;
    let mut members: Vec<Value> = tree
        .into_iter()
        .filter(|agent| agent["executionMode"] == "vm")
        .collect();
    members.sort_by_key(|agent| (agent["id"] != record["id"], agent["id"].to_string()));
    let project_id = lead
        .then(|| record["layrProjectId"].as_str().map(str::to_owned))
        .flatten();
    if lead && project_id.is_none() {
        return Err(refused("The chat has no VM project"));
    }
    Ok(Plan {
        members,
        host_path,
        project_id,
    })
}

/// One file of provider history: its guest path, its place here and its guest hash.
struct Transfer {
    agent: String,
    guest: String,
    host: PathBuf,
    bytes: u64,
    sha256: String,
    token: String,
}

fn guest_call(host: &Host, method: &str, params: &Value, request: Option<&str>) -> Result<Value> {
    host.guest
        .call(method, params, request, GUEST_TIMEOUT)
        .map_err(|error| {
            if error.uncertain() {
                failed(format!("the VM did not prove the result of {method}"))
            } else {
                failed(format!("the VM refused {method}: {error}"))
            }
        })
}

/// Paths below `directory` that `find` selects, as the chat's Linux user.
fn guest_find(
    host: &Host,
    agent: &str,
    request: &str,
    directory: &str,
    depth: &str,
    pattern: &str,
) -> Result<Vec<String>> {
    let result = guest_call(
        host,
        "layr.exec",
        &json!({"agentId":agent,"argv":["find",directory,"-maxdepth",depth,"-type","f","-name",pattern,"-print0"]}),
        Some(request),
    )?;
    if result["exitCode"] != 0 {
        return Ok(Vec::new());
    }
    let out = BASE64.decode(result["stdout"].as_str().unwrap_or_default())?;
    Ok(out
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| String::from_utf8_lossy(path).into_owned())
        .collect())
}

fn account_digest(account_key: &str) -> String {
    data_encoding::HEXLOWER
        .encode(&Sha256::digest(account_key.as_bytes()))
        .chars()
        .take(32)
        .collect()
}

/// The history files of one chat. A chat without a thread has none.
fn transfers(host: &Host, member: &Value, tag: &str) -> Result<Vec<Transfer>> {
    let thread = member["threadId"].as_str().unwrap_or_default();
    if thread.is_empty() || thread.contains(['/', '*', '?', '[']) {
        return Ok(Vec::new());
    }
    let id = member["id"].as_str().unwrap_or_default();
    let account_key = member["accountKey"].as_str().unwrap_or("default");
    let guest_home = member["layrHome"]
        .as_str()
        .filter(|home| home.starts_with('/'))
        .ok_or_else(|| refused(format!("Chat {id} has no VM home")))?;
    let claude = member["provider"] == "claude";
    let account = host
        .accounts
        .iter()
        .find(|row| row.id.as_str() == Some(account_key))
        .and_then(profile_dir)
        .ok_or_else(|| refused("The account of this chat has no profile on this device"))?;
    let directory = if claude { ".claude" } else { ".codex" };
    let guest_profile = format!(
        "{guest_home}/{directory}/studio-accounts/{}",
        account_digest(account_key)
    );
    let request = |purpose: &str| format!("vm-to-host:{tag}:{id}:{purpose}");
    let mut wanted: Vec<(String, PathBuf)> = Vec::new();
    let mut place = |root: String, host_root: PathBuf, paths: Vec<String>| -> Result<()> {
        for path in paths {
            let relative = path
                .strip_prefix(&format!("{root}/"))
                .filter(|relative| {
                    !relative
                        .split('/')
                        .any(|part| part == ".." || part.is_empty())
                })
                .ok_or_else(|| failed("the VM listed a path outside its provider folder"))?;
            wanted.push((path.clone(), host_root.join(relative)));
        }
        Ok(())
    };
    if claude {
        let root = format!("{guest_profile}/projects");
        let found = guest_find(
            host,
            id,
            &request("native"),
            &root,
            "2",
            &format!("{thread}.jsonl"),
        )?;
        if found.len() != 1 {
            return Err(refused(
                "The Claude session file of this chat is missing or not unique",
            ));
        }
        place(root, account.join("projects"), found)?;
        let root = format!("{guest_home}/.codex/studio-bridges/{id}/sessions");
        let found = guest_find(
            host,
            id,
            &request("bridge"),
            &root,
            "1",
            &format!("{thread}.*"),
        )?;
        if !found
            .iter()
            .any(|path| path.ends_with(&format!("{thread}.json")))
        {
            return Err(refused("The saved Claude session of this chat is missing"));
        }
        let sessions = crate::account_driver::account_server_root(&host.state_dir, account_key);
        place(root, sessions.join("sessions"), found)?;
    } else {
        let root = format!("{guest_profile}/sessions");
        let found = guest_find(
            host,
            id,
            &request("native"),
            &root,
            "5",
            &format!("*{thread}.jsonl"),
        )?;
        if found.is_empty() {
            return Err(refused("The Codex rollout file of this chat is missing"));
        }
        place(root, account.join("sessions"), found)?;
    }
    wanted
        .into_iter()
        .map(|(guest, destination)| {
            let stat = guest_call(
                host,
                "layr.file.stat",
                &json!({"agentId":id,"path":guest}),
                None,
            )?;
            Ok(Transfer {
                agent: id.to_owned(),
                guest,
                host: destination,
                bytes: stat["bytes"]
                    .as_u64()
                    .ok_or_else(|| failed("no file size"))?,
                sha256: stat["sha256"].as_str().unwrap_or_default().to_owned(),
                token: stat["token"].as_str().unwrap_or_default().to_owned(),
            })
        })
        .collect()
}

/// Refuse what cannot move, then bring the folder and the history.
fn copy_everything(
    host: &Host,
    members: &[Value],
    host_path: &str,
    project_id: Option<&str>,
    tag: &str,
) -> Result<()> {
    let mut files = Vec::new();
    for member in members {
        files.extend(transfers(host, member, tag)?);
        // A line that main does not hold would lose its work in the VM.
        let line = member["layrLine"].as_str().unwrap_or("main");
        if line != "main" {
            let id = member["id"].as_str().unwrap_or_default();
            let check = guest_call(
                host,
                "layr.exec",
                &json!({"agentId":id,"argv":["sh","-c",
                    "test \"$(layr merge-base head main)\" = \"$(layr rev-parse head)\""]}),
                Some(&format!("vm-to-host:{tag}:{id}:contained")),
            )?;
            if check["exitCode"] != 0 {
                return Err(refused(format!(
                    "The work of chat {id} is not in main. Merge or drop it, then move again"
                )));
            }
        }
    }
    let mut needed: u64 = files
        .iter()
        .filter(|file| !file.host.exists())
        .map(|file| file.bytes)
        .sum();
    if let Some(project) = project_id {
        needed += host.files.needed_bytes(project)?;
    }
    let needed =
        needed.saturating_add(studio_runtime_schema::shared_values::DISK_FULL_EXIT_FREE_BYTES);
    for directory in [Path::new(host_path), host.state_dir.as_path()] {
        let free = (host.free_bytes)(directory).unwrap_or(0);
        if free < needed {
            return Err(refused(format!(
                "Not enough free space on this device: {needed} bytes are needed, {free} are free"
            )));
        }
    }
    if let Some(project) = project_id {
        host.files.bring(project)?;
    }
    for file in &files {
        copy_file(host, file, tag)?;
    }
    Ok(())
}

/// Copy one file in chunks and verify it by hash. Equal bytes already in place are kept;
/// different bytes are never replaced.
fn copy_file(host: &Host, file: &Transfer, tag: &str) -> Result<()> {
    if file.host.exists() {
        return if hash_file(&file.host)? == (file.sha256.clone(), file.bytes) {
            Ok(())
        } else {
            Err(refused(format!(
                "This device already has different history at {}",
                file.host.display()
            )))
        };
    }
    let parent = file
        .host
        .parent()
        .ok_or_else(|| failed("history path has no folder"))?;
    std::fs::create_dir_all(parent)?;
    let name = file
        .host
        .file_name()
        .map(|name| name.to_string_lossy().into_owned());
    let short: String = tag.chars().take(16).collect();
    let temporary = parent.join(format!(".{}.{short}.part", name.unwrap_or_default()));
    let mut output = std::fs::File::create(&temporary)?;
    let mut hasher = Sha256::new();
    let mut offset = 0_u64;
    while offset < file.bytes {
        let chunk = guest_call(
            host,
            "layr.file.read",
            &json!({"agentId":file.agent,"path":file.guest,"token":file.token,
                "offset":offset,"maxBytes":READ_CHUNK_BYTES}),
            None,
        )?;
        let data = BASE64.decode(chunk["data"].as_str().unwrap_or_default())?;
        let next = chunk["nextOffset"].as_u64().unwrap_or(0);
        if data.is_empty()
            || offset + data.len() as u64 != next
            || data_encoding::HEXLOWER.encode(&Sha256::digest(&data)) != chunk["sha256"]
        {
            return Err(failed("a history chunk did not match its hash"));
        }
        output.write_all(&data)?;
        hasher.update(&data);
        offset = next;
    }
    output.sync_all()?;
    drop(output);
    let sent = data_encoding::HEXLOWER.encode(&hasher.finalize());
    // Verify the bytes on disk, not only the bytes that were written.
    if sent != file.sha256 || hash_file(&temporary)? != (file.sha256.clone(), file.bytes) {
        std::fs::remove_file(&temporary).ok();
        return Err(failed("copied history differs from the VM file"));
    }
    // A hard link never replaces a file that appeared meanwhile.
    std::fs::hard_link(&temporary, &file.host)?;
    std::fs::remove_file(&temporary)?;
    Ok(())
}

fn hash_file(path: &Path) -> Result<(String, u64)> {
    let mut input = std::fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    let mut total = 0_u64;
    loop {
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(buffer.get(..read).unwrap_or_default());
        total += read as u64;
    }
    Ok((data_encoding::HEXLOWER.encode(&hasher.finalize()), total))
}

#[cfg(test)]
mod tests;
