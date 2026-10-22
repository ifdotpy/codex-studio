//! Bounded Claude task output reads, relative to the owning agent's workspace.
use crate::workspace_files::WorkspaceContext;
use serde_json::{Value, json};
use std::fs::File;
use std::io::Read;
use std::path::{Component, Path};

/// Maximum file bytes returned by one task output request. Unit: bytes.
pub const MAX_TASK_OUTPUT_BYTES: u64 = 65_536;

/// Open each component relative to a pinned directory descriptor. Never follow a
/// symlink, including the final file, so a rename cannot redirect the read outside.
#[cfg(unix)]
fn open_output(root: &Path, path: &Path) -> Result<File, &'static str> {
    use rustix::fs::{Mode, OFlags, open, openat};
    let supplied_root = root;
    let root = root
        .canonicalize()
        .map_err(|_| "The task workspace is unavailable")?;
    let relative = if path.is_absolute() {
        path.strip_prefix(supplied_root)
            .or_else(|_| path.strip_prefix(&root))
            .map_err(|_| "The output file is outside the task workspace")?
    } else {
        path
    };
    let parts = relative.components().collect::<Vec<_>>();
    if parts.is_empty()
        || parts
            .iter()
            .any(|part| !matches!(part, Component::Normal(_)))
    {
        return Err("The output file is outside the task workspace");
    }
    let directory = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut descriptor =
        open(&root, directory, Mode::empty()).map_err(|_| "The task workspace is unavailable")?;
    for (index, part) in parts.iter().enumerate() {
        let flags = if index + 1 == parts.len() {
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::CLOEXEC | OFlags::NONBLOCK
        } else {
            directory
        };
        descriptor = openat(&descriptor, part.as_os_str(), flags, Mode::empty())
            .map_err(|_| "The output file is unavailable or contains a symlink")?;
    }
    Ok(File::from(descriptor))
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
    if context.execution_backed && !(context.image_workspace && context.image_workspace_ready) {
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
