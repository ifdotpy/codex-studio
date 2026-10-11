//! Write one state of a vault into a new folder: files with their executable bit, links as
//! links. The folder appears only when every file is written and checked.

use anyhow::{bail, Context, Result};
use desk_core::{Reader, Store};
use serde::Serialize;
use std::path::Path;

#[derive(Debug, Serialize)]
pub struct Summary {
    pub seq: u64,
    pub state: String,
    pub files: usize,
    pub bytes: u64,
}

pub async fn pull<S: Store>(
    reader: &Reader<S>,
    (seq, attempt): (u64, String),
    to: &Path,
) -> Result<Summary> {
    if std::fs::symlink_metadata(to).is_ok() {
        bail!(
            "{} exists; pull writes only into a new folder",
            to.display()
        );
    }
    let parent = to
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .unwrap_or(Path::new("."));
    let name = to
        .file_name()
        .context("the target folder has no name")?
        .to_string_lossy();
    let partial = parent.join(format!(".{name}.partial-{}", std::process::id()));
    std::fs::create_dir_all(&partial)?;
    let result = write(reader, (seq, &attempt), &partial).await;
    match result {
        Ok(summary) => {
            std::fs::rename(&partial, to)
                .with_context(|| format!("moving the files to {}", to.display()))?;
            Ok(summary)
        }
        Err(e) => {
            let _ = std::fs::remove_dir_all(&partial);
            Err(e)
        }
    }
}

async fn write<S: Store>(
    reader: &Reader<S>,
    (seq, attempt): (u64, &str),
    root: &Path,
) -> Result<Summary> {
    use std::os::unix::fs::PermissionsExt;
    let header = reader.header(seq, attempt).await?;
    let files = reader.files(Some((seq, attempt.to_string()))).await?;
    let mut bytes = 0;
    // Files first and links last, so that no write goes through a link of this state.
    let (links, plain): (Vec<_>, Vec<_>) = files.iter().partition(|(_, e)| e.is_link());
    for (path, entry) in plain.into_iter().chain(links) {
        let data = reader.read(entry).await?;
        bytes += data.len() as u64;
        let target = root.join(path);
        if let Some(folder) = target.parent() {
            std::fs::create_dir_all(folder)?;
        }
        if entry.is_link() {
            use std::os::unix::ffi::OsStrExt;
            std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&data), &target)?;
        } else {
            std::fs::write(&target, &data)?;
            let mode = if entry.is_executable() { 0o755 } else { 0o644 };
            std::fs::set_permissions(&target, std::fs::Permissions::from_mode(mode))?;
        }
    }
    Ok(Summary {
        seq,
        state: header.state,
        files: files.len(),
        bytes,
    })
}
