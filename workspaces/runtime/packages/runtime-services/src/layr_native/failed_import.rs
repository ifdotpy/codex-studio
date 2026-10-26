//! Discard only an explicitly failed import's private project before a fresh import.
use super::{NATIVE_LAYR_DIRECTORY, NativeLayrError, private_tree::Directory, store_path};
use std::path::Path;

type Result<T> = std::result::Result<T, NativeLayrError>;

pub(super) fn discard(state: &Path, project: &str) -> Result<()> {
    if project.len() != super::PROJECT_ID_HEX_BYTES
        || !project.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(NativeLayrError::Refused(
            "Invalid failed import project ID".into(),
        ));
    }
    if state
        .join(NATIVE_LAYR_DIRECTORY)
        .join(format!("{project}.json"))
        .exists()
    {
        return Err(NativeLayrError::Refused(
            "Cannot discard a ready native project".into(),
        ));
    }
    let projects_path = store_path(state).join("projects");
    let projects = match Directory::open(&projects_path) {
        Ok(projects) => projects,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if !projects.exists(std::ffi::OsStr::new(project))? {
        return Ok(());
    }
    #[cfg(target_os = "macos")]
    {
        // Open the project itself without following a link before any mutation.
        let partial = projects.child(Path::new(project), false)?;
        remove_contents(&partial.0).map_err(|error| {
            NativeLayrError::Refused(format!(
                "Cannot discard failed native import at {}: {error}",
                projects_path.join(project).display()
            ))
        })?;
        rustix::fs::unlinkat(&projects.0, project, rustix::fs::AtFlags::REMOVEDIR)
            .map_err(std::io::Error::from)?;
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    Err(NativeLayrError::Refused(format!(
        "Cannot retry this failed native import automatically: the private partial project at {} requires daemon-owned subvolume removal. The source remains unchanged.",
        projects_path.join(project).display()
    )))
}

#[cfg(target_os = "macos")]
fn remove_contents(fd: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use rustix::fs::{
        AtFlags, Dir, FileType, Mode, OFlags, fchmod, fstat, openat, statat, unlinkat,
    };
    unseal(fd)?;
    fchmod(fd, Mode::from_raw_mode(fstat(fd)?.st_mode | 0o700))?;
    for entry in Dir::read_from(fd)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let stat = statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let child = openat(fd, name, super::private_tree::flags(), Mode::empty())?;
            remove_contents(&child)?;
            unlinkat(fd, name, AtFlags::REMOVEDIR)?;
        } else {
            let flags = OFlags::RDONLY | OFlags::NONBLOCK | OFlags::CLOEXEC;
            let flags = if FileType::from_raw_mode(stat.st_mode) == FileType::Symlink {
                // SYMLINK opens the link itself. Combining it with NOFOLLOW returns ELOOP.
                flags | OFlags::SYMLINK
            } else {
                flags | OFlags::NOFOLLOW
            };
            let child = openat(fd, name, flags, Mode::empty())?;
            unseal(&child)?;
            unlinkat(fd, name, AtFlags::empty())?;
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "Clear deletion flags only through a pinned private entry descriptor"
)]
fn unseal(fd: &std::os::fd::OwnedFd) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;
    let stat = rustix::fs::fstat(fd)?;
    let flags = stat.st_flags & !(libc::UF_IMMUTABLE | libc::UF_APPEND);
    if flags != stat.st_flags {
        // SAFETY: fd is live and was opened without following links in the private project.
        if unsafe { libc::fchflags(fd.as_raw_fd(), flags) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn failed_import_cleanup_refuses_ready_projects_and_invalid_ids() {
        let state = tempfile::tempdir().expect("state");
        let project = "b".repeat(64);
        let partial = store_path(state.path()).join("projects").join(&project);
        fs::create_dir_all(&partial).expect("partial project");
        fs::write(partial.join("keep"), "private").expect("private file");
        fs::write(
            state
                .path()
                .join(NATIVE_LAYR_DIRECTORY)
                .join(format!("{project}.json")),
            "{}",
        )
        .expect("readiness marker");
        assert!(
            discard(state.path(), &project)
                .expect_err("ready project")
                .to_string()
                .contains("ready native project")
        );
        for invalid in ["..", "../source", "projects/main", ""] {
            assert!(discard(state.path(), invalid).is_err());
        }
        assert_eq!(
            fs::read(partial.join("keep")).expect("private file"),
            b"private"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn failed_import_cleanup_refuses_project_or_store_symlinks() {
        let state = tempfile::tempdir().expect("state");
        let source = tempfile::tempdir().expect("source");
        fs::write(source.path().join("keep"), "source").expect("source file");
        let project = "c".repeat(64);
        let projects = store_path(state.path()).join("projects");
        fs::create_dir_all(&projects).expect("projects");
        std::os::unix::fs::symlink(source.path(), projects.join(&project)).expect("project link");
        assert!(discard(state.path(), &project).is_err());
        fs::remove_file(projects.join(&project)).expect("remove fixture link");
        fs::remove_dir(&projects).expect("empty projects");
        std::os::unix::fs::symlink(source.path(), &projects).expect("store link");
        assert!(discard(state.path(), &project).is_err());
        assert_eq!(
            fs::read(source.path().join("keep")).expect("source file"),
            b"source"
        );
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn failed_import_cleanup_refuses_daemon_owned_subvolumes() {
        let state = tempfile::tempdir().expect("state");
        let project = "d".repeat(64);
        let partial = store_path(state.path()).join("projects").join(&project);
        fs::create_dir_all(&partial).expect("partial project");
        let error = discard(state.path(), &project)
            .expect_err("daemon cleanup required")
            .to_string();
        assert!(error.contains("daemon-owned subvolume removal"));
        assert!(error.contains(&partial.to_string_lossy().to_string()));
        assert!(partial.exists());
    }
}
