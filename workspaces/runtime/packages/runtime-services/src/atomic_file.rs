//! Replace a file in one rename. A failure at any step, a full disk included,
//! removes the temporary file and keeps the previous file.

use std::fs::{self, File};
use std::io::{self, Write};
use std::path::Path;

/// Write `bytes` to `path` through a temporary file in the same directory.
///
/// `mode` sets Unix permissions on the new file. `durable` syncs the file and
/// its directory. Each call has its own temporary name, so parallel writers do
/// not collide, and a failed call leaves no temporary file.
pub fn replace(path: &Path, bytes: &[u8], mode: Option<u32>, durable: bool) -> io::Result<()> {
    replace_with(path, bytes, mode, durable, |temporary, target| {
        temporary.persist(target).map(|_| ()).map_err(|error| {
            // Dropping the returned temporary file removes it.
            drop(error.file);
            error.error
        })
    })
}

fn replace_with(
    path: &Path,
    bytes: &[u8],
    mode: Option<u32>,
    durable: bool,
    publish: impl FnOnce(tempfile::NamedTempFile, &Path) -> io::Result<()>,
) -> io::Result<()> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."));
    let prefix = format!(
        ".{}.",
        path.file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("file")
    );
    let mut temporary = tempfile::Builder::new()
        .prefix(&prefix)
        .suffix(".tmp")
        .tempfile_in(parent)?;
    temporary.write_all(bytes)?;
    #[cfg(unix)]
    if let Some(mode) = mode {
        use std::os::unix::fs::PermissionsExt;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    let _ = mode;
    if durable {
        temporary.as_file().sync_all()?;
    }
    publish(temporary, path)?;
    if durable {
        File::open(parent)?.sync_all()?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(directory: &Path) -> Vec<String> {
        let mut names = fs::read_dir(directory)
            .expect("directory lists")
            .map(|entry| {
                entry
                    .expect("entry reads")
                    .file_name()
                    .to_string_lossy()
                    .into_owned()
            })
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn a_disk_full_rename_keeps_the_previous_file_and_no_temporary() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("state.json");
        replace(&target, b"{\"good\":true}", Some(0o600), true).expect("first write");
        let error = replace_with(&target, b"{\"good\":false}", Some(0o600), true, |_, _| {
            Err(io::Error::from(rustix::io::Errno::NOSPC))
        })
        .expect_err("injected ENOSPC fails the write");
        assert_eq!(
            error.raw_os_error(),
            Some(rustix::io::Errno::NOSPC.raw_os_error())
        );
        assert_eq!(
            fs::read(&target).expect("previous file"),
            b"{\"good\":true}"
        );
        assert_eq!(names(directory.path()), vec!["state.json".to_owned()]);
    }

    #[test]
    fn a_failed_publish_into_a_directory_removes_the_temporary_file() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("occupied");
        fs::create_dir(&target).expect("directory in the way");
        fs::write(target.join("child"), b"keep").expect("child file");
        assert!(replace(&target, b"new", None, false).is_err());
        assert_eq!(names(directory.path()), vec!["occupied".to_owned()]);
        assert_eq!(fs::read(target.join("child")).expect("child"), b"keep");
    }

    #[cfg(unix)]
    #[test]
    fn the_new_file_has_the_requested_mode() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().expect("temporary directory");
        let target = directory.path().join("private.json");
        replace(&target, b"{}", Some(0o600), false).expect("write");
        assert_eq!(
            fs::metadata(&target)
                .expect("metadata")
                .permissions()
                .mode()
                & 0o777,
            0o600
        );
    }
}
