//! Bounded Claude task output reads from workspaces and the current user's CLI directory.
use crate::workspace_files::WorkspaceContext;
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;
use std::path::Path;

#[cfg(unix)]
use rustix::{
    fd::OwnedFd,
    fs::{Mode, OFlags, fstat, open, openat},
};
#[cfg(unix)]
use std::path::{Component, PathBuf};
#[cfg(unix)]
use studio_runtime_store::path_identity::CanonicalPath;

/// Maximum file bytes returned by one task output request. Unit: bytes.
pub const MAX_TASK_OUTPUT_BYTES: u64 = 65_536;

#[cfg(unix)]
const DIRECTORY_FLAGS: OFlags = OFlags::RDONLY
    .union(OFlags::DIRECTORY)
    .union(OFlags::NOFOLLOW)
    .union(OFlags::CLOEXEC);
/// Claude CLI kdt permits task IDs of 1 to 20 ASCII bytes.
#[cfg(unix)]
const MAX_CLAUDE_TASK_ID_BYTES: usize = 20;
#[cfg(unix)]
const OUTSIDE_OUTPUT_PATH: &str =
    "The output file is outside the task workspace and the Claude task directory";

/// Open each component relative to a pinned directory descriptor. Never follow a
/// symlink, including the final file, so a rename cannot redirect the read outside.
#[cfg(unix)]
fn open_relative(mut descriptor: OwnedFd, relative: &Path) -> Result<File, &'static str> {
    let parts = relative.components().collect::<Vec<_>>();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err(OUTSIDE_OUTPUT_PATH);
    }
    for (index, part) in parts.iter().enumerate() {
        let flags = if index + 1 == parts.len() {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK
        } else {
            DIRECTORY_FLAGS
        };
        descriptor = openat(&descriptor, part.as_os_str(), flags, Mode::empty())
            .map_err(|_| "The output file is unavailable or contains a symlink")?;
    }
    Ok(File::from(descriptor))
}

#[cfg(unix)]
fn open_workspace_output(root: &Path, path: &Path) -> Result<File, &'static str> {
    let supplied_root = root;
    let root = CanonicalPath::existing(root).map_err(|_| "The task workspace is unavailable")?;
    let relative = if path.is_absolute() {
        path.strip_prefix(supplied_root)
            .or_else(|_| path.strip_prefix(root.as_path()))
            .map_err(|_| OUTSIDE_OUTPUT_PATH)?
    } else {
        path
    };
    let descriptor = open(root.as_path(), DIRECTORY_FLAGS, Mode::empty())
        .map_err(|_| "The task workspace is unavailable")?;
    open_relative(descriptor, relative)
}

#[cfg(unix)]
fn open_claude_output(path: &Path) -> Result<File, &'static str> {
    // Claude CLI 2.1.302 um() selects this base. The bridge preserves this variable.
    let supplied_temp = std::env::var_os("CLAUDE_CODE_TMPDIR")
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| "/tmp".into());
    if !supplied_temp.is_absolute() {
        return Err("The Claude temporary directory is unavailable");
    }
    let temporary = CanonicalPath::existing(&supplied_temp)
        .map_err(|_| "The Claude temporary directory is unavailable")?;
    let uid = nix::unistd::getuid().as_raw();
    let directory = format!("claude-{uid}");
    let relative = path
        .strip_prefix(&supplied_temp)
        .or_else(|_| path.strip_prefix(temporary.as_path()))
        .map_err(|_| OUTSIDE_OUTPUT_PATH)?
        .strip_prefix(&directory)
        .map_err(|_| OUTSIDE_OUTPUT_PATH)?;
    let parts = relative.components().collect::<Vec<_>>();
    if parts.len() != 4
        || parts
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
        || parts[2].as_os_str() != "tasks"
    {
        return Err(OUTSIDE_OUTPUT_PATH);
    }
    let id = parts[3]
        .as_os_str()
        .to_str()
        .and_then(|name| name.strip_suffix(".output"))
        .ok_or(OUTSIDE_OUTPUT_PATH)?;
    if !(1..=MAX_CLAUDE_TASK_ID_BYTES).contains(&id.len())
        || !id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(OUTSIDE_OUTPUT_PATH);
    }
    let temporary = open(temporary.as_path(), DIRECTORY_FLAGS, Mode::empty())
        .map_err(|_| "The Claude temporary directory is unavailable")?;
    let root = openat(
        &temporary,
        directory.as_str(),
        DIRECTORY_FLAGS,
        Mode::empty(),
    )
    .map_err(|_| "The Claude output directory is unavailable or contains a symlink")?;
    // Check the pinned descriptor so a path swap cannot replace the checked directory.
    if fstat(&root)
        .map_err(|_| "The Claude output directory is unavailable")?
        .st_uid
        != uid
    {
        return Err("The Claude output directory belongs to another user");
    }
    open_relative(root, relative)
}

#[cfg(unix)]
fn open_output(root: &Path, path: &Path) -> Result<File, &'static str> {
    if path.components().any(|part| part == Component::ParentDir) {
        return Err(OUTSIDE_OUTPUT_PATH);
    }
    open_workspace_output(root, path).or_else(|error| {
        if path.is_absolute() {
            open_claude_output(path)
        } else {
            Err(error)
        }
    })
}

#[cfg(not(unix))]
fn open_output(_root: &Path, _path: &Path) -> Result<File, &'static str> {
    Err("Secure task output reads are unavailable on this platform")
}

pub fn read(task: &Value, agent: &Value, requested_agent: &str) -> Result<Value, &'static str> {
    if task["agent"] != requested_agent
        || agent["id"] != requested_agent
        || studio_runtime_store::truthy(agent.get("deletedAt"))
    {
        return Err("This task does not belong to the requested agent");
    }
    if task["sourceProvider"]
        .as_str()
        .unwrap_or(agent["provider"].as_str().unwrap_or(""))
        != "claude"
        || !task["itemId"]
            .as_str()
            .is_some_and(|id| id.starts_with("task:"))
    {
        return Err("This task has no Claude background output file");
    }
    let context = WorkspaceContext::from_agent(agent, requested_agent)
        .map_err(|_| "The task workspace is unavailable")?;
    if context.execution_backed {
        return Err("The task workspace is not available on this server");
    }
    let path = task["outputFile"]
        .as_str()
        .filter(|p| !p.is_empty())
        .ok_or("This task has no output file")?;
    let file = open_output(Path::new(&context.cwd), Path::new(path))?;
    if !file
        .metadata()
        .map_err(|_| "The output file is unavailable")?
        .is_file()
    {
        return Err("The output path is not a regular file");
    }
    let mut bytes = Vec::new();
    file.take(MAX_TASK_OUTPUT_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| "The output file cannot be read")?;
    let truncated = bytes.len() as u64 > MAX_TASK_OUTPUT_BYTES;
    bytes.truncate(MAX_TASK_OUTPUT_BYTES as usize);
    Ok(
        json!({"id":task["id"],"outputFile":path,"text":String::from_utf8_lossy(&bytes),"truncated":truncated}),
    )
}
