//! Local filesystem reads used by the system routes.

use std::fs;
use std::path::{Path, PathBuf};

use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryEntry {
    pub name: String,
    pub path: String,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DirectoryListing {
    pub path: String,
    pub parent: Option<String>,
    pub directories: Vec<DirectoryEntry>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum SystemReadError {
    #[error("This directory is unavailable")]
    UnavailableDirectory,
}

pub fn directories(path: Option<&str>) -> Result<DirectoryListing, SystemReadError> {
    let raw = path.unwrap_or("");
    let raw = if raw.is_empty() {
        std::env::current_dir().map_err(|_| SystemReadError::UnavailableDirectory)?
    } else {
        expand_home(raw)
    };
    let directory = Path::new(&raw)
        .canonicalize()
        .map_err(|_| SystemReadError::UnavailableDirectory)?;
    if !directory.is_dir() {
        return Err(SystemReadError::UnavailableDirectory);
    }

    let mut directories = Vec::new();
    for entry in fs::read_dir(&directory).map_err(|_| SystemReadError::UnavailableDirectory)? {
        let entry = entry.map_err(|_| SystemReadError::UnavailableDirectory)?;
        let name = entry.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') || !entry.path().is_dir() {
            continue;
        }
        directories.push(DirectoryEntry {
            name,
            path: entry.path().to_string_lossy().into_owned(),
        });
    }
    directories.sort_by_key(|entry| entry.name.to_lowercase());
    directories.truncate(500);

    Ok(DirectoryListing {
        path: directory.to_string_lossy().into_owned(),
        parent: parent(&directory).map(|path| path.to_string_lossy().into_owned()),
        directories,
    })
}

fn parent(directory: &Path) -> Option<PathBuf> {
    let parent = directory.parent()?;
    (parent != directory).then(|| parent.to_path_buf())
}

fn expand_home(path: &str) -> PathBuf {
    if (path == "~" || path.starts_with("~/"))
        && let Some(home) = std::env::var_os("HOME")
    {
        return PathBuf::from(home).join(path.strip_prefix("~/").unwrap_or(""));
    }
    PathBuf::from(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lists_visible_directories_in_case_insensitive_order()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = std::env::temp_dir().join(format!("system-reads-{}", std::process::id()));
        if root.exists() {
            fs::remove_dir_all(&root)?;
        }
        fs::create_dir_all(root.join("zeta"))?;
        fs::create_dir_all(root.join("Alpha"))?;
        fs::create_dir_all(root.join(".hidden"))?;
        fs::write(root.join("file.txt"), "file")?;

        let path = root
            .to_str()
            .ok_or_else(|| std::io::Error::other("temporary directory path is not UTF-8"))?;
        let listing = directories(Some(path))?;
        assert_eq!(
            listing
                .directories
                .iter()
                .map(|entry| entry.name.as_str())
                .collect::<Vec<_>>(),
            ["Alpha", "zeta"]
        );
        let parent = root
            .parent()
            .and_then(Path::to_str)
            .ok_or_else(|| std::io::Error::other("temporary directory has no UTF-8 parent"))?;
        assert_eq!(listing.parent.as_deref(), Some(parent));
        fs::remove_dir_all(root)?;
        Ok(())
    }

    #[test]
    fn rejects_a_missing_directory() {
        assert_eq!(
            directories(Some("/definitely/not/a/studio/directory")),
            Err(SystemReadError::UnavailableDirectory)
        );
    }
}
