//! Filesystem policy for file previews in local workspaces.

use std::fs::{self, File};
use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::Value;
use thiserror::Error;

pub const MAX_PREVIEW_BYTES: u64 = 20 * 1024 * 1024;
const MAX_PATH_CHARS: usize = 4096;
const PYTHON_MIME_OVERRIDES: &[(&str, &str)] = &[
    ("ts", "text/vnd.trolltech.linguist"),
    ("wav", "audio/x-wav"),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkspaceContext {
    pub cwd: String,
    pub execution_backed: bool,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileInfo {
    pub path: String,
    pub name: String,
    pub mime: String,
    pub size: u64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct FileContent {
    pub name: String,
    pub mime: String,
    pub bytes: Vec<u8>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum WorkspaceFilesError {
    #[error("Invalid stored object agents/{0}")]
    InvalidAgent(String),
    #[error("This file does not exist")]
    MissingFile,
    #[error("This file exceeds the 20 MiB preview limit")]
    PreviewLimit,
    #[error("Supply a path")]
    MissingPath,
    #[error("Invalid path")]
    InvalidPath,
}

impl WorkspaceContext {
    pub fn from_agent(record: &Value, row_id: &str) -> Result<Self, WorkspaceFilesError> {
        let invalid = || WorkspaceFilesError::InvalidAgent(row_id.to_owned());
        if record.get("id").and_then(Value::as_str) != Some(row_id) {
            return Err(invalid());
        }
        let cwd = record
            .get("cwd")
            .and_then(Value::as_str)
            .filter(|cwd| !cwd.is_empty())
            .ok_or_else(invalid)?
            .to_owned();
        let execution_mode = record.get("executionMode").and_then(Value::as_str);
        let workspace_mode = record.get("workspaceMode").and_then(Value::as_str);
        let execution_backed = record.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
            || execution_mode == Some("vm")
            || matches!(workspace_mode, Some("image" | "layr"));
        Ok(Self {
            cwd,
            execution_backed,
        })
    }
}

pub fn file_info(path: &str, root: Option<&str>) -> Result<FileInfo, WorkspaceFilesError> {
    let file = resolve_file(path, root)?;
    let metadata = file
        .metadata()
        .map_err(|_| WorkspaceFilesError::MissingFile)?;
    if !metadata.is_file() {
        return Err(WorkspaceFilesError::MissingFile);
    }
    Ok(FileInfo {
        path: file.to_string_lossy().into_owned(),
        name: file
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or(WorkspaceFilesError::MissingFile)?,
        mime: mime_type(&file),
        size: metadata.len(),
    })
}

pub fn file_content(path: &str, root: Option<&str>) -> Result<FileContent, WorkspaceFilesError> {
    let file_path = resolve_file(path, root)?;
    let path_metadata = fs::metadata(&file_path).map_err(|_| WorkspaceFilesError::MissingFile)?;
    if !path_metadata.is_file() {
        return Err(WorkspaceFilesError::MissingFile);
    }
    let file = File::open(&file_path).map_err(|_| WorkspaceFilesError::MissingFile)?;
    let metadata = file
        .metadata()
        .map_err(|_| WorkspaceFilesError::MissingFile)?;
    if !metadata.is_file() {
        return Err(WorkspaceFilesError::MissingFile);
    }
    if metadata.len() > MAX_PREVIEW_BYTES {
        return Err(WorkspaceFilesError::PreviewLimit);
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_PREVIEW_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| WorkspaceFilesError::MissingFile)?;
    if bytes.len() as u64 > MAX_PREVIEW_BYTES {
        return Err(WorkspaceFilesError::PreviewLimit);
    }
    Ok(FileContent {
        name: file_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .ok_or(WorkspaceFilesError::MissingFile)?,
        mime: mime_type(&file_path),
        bytes,
    })
}

fn resolve_file(path: &str, root: Option<&str>) -> Result<PathBuf, WorkspaceFilesError> {
    if path.is_empty() {
        return Err(WorkspaceFilesError::MissingPath);
    }
    if path.chars().count() > MAX_PATH_CHARS {
        return Err(WorkspaceFilesError::InvalidPath);
    }
    let expanded = expand_home(path);
    let supplied = Path::new(&expanded);
    let candidate = if supplied.is_absolute() {
        supplied.to_path_buf()
    } else {
        let root = root.ok_or(WorkspaceFilesError::MissingPath)?;
        Path::new(root).join(supplied)
    };
    // Match Python's workspace_path: absolute paths and symlinks remain valid,
    // with no containment check against the workspace root.
    candidate
        .canonicalize()
        .map_err(|_| WorkspaceFilesError::MissingFile)
}

fn expand_home(path: &str) -> String {
    if (path == "~" || path.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        let suffix = path.strip_prefix('~').unwrap_or_default();
        return format!("{}{suffix}", PathBuf::from(home).display());
    }
    path.to_owned()
}

pub fn mime_type(path: &Path) -> String {
    let extension = path
        .extension()
        .and_then(|extension| extension.to_str())
        .map(str::to_ascii_lowercase);
    // Python's mimetypes uses this legacy spelling; the HTTP contract pins it.
    if let Some((_, mime)) = PYTHON_MIME_OVERRIDES
        .iter()
        .find(|(override_extension, _)| Some(*override_extension) == extension.as_deref())
    {
        return (*mime).to_owned();
    };
    mime_guess::from_path(path)
        .first_or_octet_stream()
        .essence_str()
        .to_owned()
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::PathBuf;

    use serde_json::json;

    use super::{
        MAX_PREVIEW_BYTES, WorkspaceContext, WorkspaceFilesError, file_content, file_info,
    };

    #[test]
    fn local_file_metadata_and_content_follow_symlinks() {
        let root = tempfile_dir();
        let target = root.join("outside.md");
        fs::write(&target, "content").expect("target writes");
        let workspace = root.join("workspace");
        fs::create_dir(&workspace).expect("workspace creates");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&target, workspace.join("link.md")).expect("symlink creates");
        let context = WorkspaceContext {
            cwd: workspace.to_string_lossy().into_owned(),
            execution_backed: false,
        };

        let info = file_info("link.md", Some(&context.cwd)).expect("metadata reads");
        assert_eq!(info.path, target.to_string_lossy());
        assert_eq!(info.name, "outside.md");
        assert_eq!(info.size, 7);
        assert_eq!(info.mime, "text/markdown");
        assert_eq!(
            file_content("link.md", Some(&context.cwd))
                .expect("content reads")
                .bytes,
            b"content"
        );
        assert_eq!(
            file_info(&target.to_string_lossy(), Some(&context.cwd)).expect("absolute path reads"),
            info
        );
        fs::remove_dir_all(root).expect("temporary directory removes");
    }

    #[test]
    fn file_content_is_bounded_and_metadata_is_not_limited() {
        let root = tempfile_dir();
        let file = root.join("large.bin");
        let content = fs::File::create(&file).expect("file creates");
        content
            .set_len(MAX_PREVIEW_BYTES + 1)
            .expect("large sparse file creates");
        let path = file.to_string_lossy();
        assert_eq!(
            file_info(&path, None).expect("metadata reads").size,
            MAX_PREVIEW_BYTES + 1
        );
        assert_eq!(
            file_content(&path, None),
            Err(WorkspaceFilesError::PreviewLimit)
        );
        fs::remove_dir_all(root).expect("temporary directory removes");
    }

    #[test]
    fn rejects_invalid_agent_identity_and_detects_execution_workspaces() {
        assert_eq!(
            WorkspaceContext::from_agent(&json!({"id":"different","cwd":"/tmp"}), "agent"),
            Err(WorkspaceFilesError::InvalidAgent("agent".to_owned()))
        );
        let context = WorkspaceContext::from_agent(
            &json!({"id":"agent","cwd":"/workspace","executionMode":"vm"}),
            "agent",
        )
        .expect("agent context parses");
        assert!(context.execution_backed);
    }

    fn tempfile_dir() -> std::path::PathBuf {
        let root = std::env::var_os("CARGO_TARGET_DIR")
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir);
        let path = root.join("workspace-files-tests").join(format!(
            "studio-workspace-files-{}-{}",
            std::process::id(),
            std::thread::current().name().unwrap_or("test")
        ));
        fs::create_dir_all(&path).expect("temporary directory creates");
        path
    }
}
