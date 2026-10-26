//! Replace ordinary files without removing the btrfs cache subvolume roots.
use std::os::unix::ffi::OsStrExt;
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

fn directory_flags() -> rustix::fs::OFlags {
    use rustix::fs::OFlags;
    OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC
}

/// Open each ancestor without following links, including the destination itself.
fn open_directory(path: &Path) -> std::io::Result<std::os::fd::OwnedFd> {
    use rustix::fs::{Mode, open, openat};
    let path = if cfg!(target_os = "macos") {
        ["/var", "/tmp"]
            .iter()
            .find_map(|prefix| {
                path.strip_prefix(prefix).ok().map(|suffix| {
                    Path::new("/private")
                        .join(prefix.trim_start_matches('/'))
                        .join(suffix)
                })
            })
            .unwrap_or_else(|| path.to_owned())
    } else {
        path.to_owned()
    };
    if !path.is_absolute() {
        return Err(std::io::Error::other("Private line path must be absolute"));
    }
    let mut parent = open("/", directory_flags(), Mode::empty())?;
    for part in path.components() {
        match part {
            std::path::Component::RootDir => {}
            std::path::Component::Normal(name) => {
                parent = openat(&parent, name, directory_flags(), Mode::empty())?
            }
            _ => {
                return Err(std::io::Error::other(
                    "Private line path contains a parent traversal",
                ));
            }
        }
    }
    Ok(parent)
}

pub(super) fn validate_destination(line: &Path, destination: &Path) -> std::io::Result<()> {
    use rustix::fs::{Mode, openat};
    let relative = destination
        .strip_prefix(line)
        .map_err(std::io::Error::other)?;
    let mut parent = open_directory(line)?;
    for part in relative.components() {
        let std::path::Component::Normal(name) = part else {
            return Err(std::io::Error::other(
                "Destination escapes the private line",
            ));
        };
        match openat(&parent, name, directory_flags(), Mode::empty()) {
            Ok(fd) => parent = fd,
            Err(rustix::io::Errno::NOENT) => return Ok(()),
            Err(error) => return Err(error.into()),
        }
    }
    Ok(())
}

pub(super) fn clear_import_destination(
    destination: &Path,
    cache_roots: &BTreeSet<PathBuf>,
) -> std::io::Result<()> {
    match open_directory(destination) {
        Ok(fd) => clear_directory(&fd, destination, cache_roots),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn clear_directory(
    fd: &std::os::fd::OwnedFd,
    path: &Path,
    cache_roots: &BTreeSet<PathBuf>,
) -> std::io::Result<()> {
    use rustix::fs::{AtFlags, Dir, FileType, Mode, fchmod, fstat, openat, statat, unlinkat};
    fchmod(fd, Mode::from_raw_mode(fstat(fd)?.st_mode | 0o700))?;
    for entry in Dir::read_from(fd)? {
        let entry = entry?;
        let name = entry.file_name();
        if matches!(name.to_bytes(), b"." | b"..") {
            continue;
        }
        let child = path.join(std::ffi::OsStr::from_bytes(name.to_bytes()));
        let stat = statat(fd, name, AtFlags::SYMLINK_NOFOLLOW)?;
        if FileType::from_raw_mode(stat.st_mode) == FileType::Directory {
            let child_fd = openat(fd, name, directory_flags(), Mode::empty())?;
            clear_directory(&child_fd, &child, cache_roots)?;
            if !cache_roots.iter().any(|cache| cache.starts_with(&child)) {
                unlinkat(fd, name, AtFlags::REMOVEDIR)?;
            }
        } else {
            unlinkat(fd, name, AtFlags::empty())?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn review_clear_refuses_symlink_destination_and_preserves_source() {
        let fixture = tempfile::tempdir().unwrap();
        let source = fixture.path().join("source");
        let line = fixture.path().join("line");
        fs::create_dir_all(source.join("repo")).unwrap();
        fs::create_dir(&line).unwrap();
        fs::write(source.join("repo/keep"), "original").unwrap();
        std::os::unix::fs::symlink(&source, line.join("link")).unwrap();
        let result = clear_import_destination(&line.join("link/repo"), &BTreeSet::new());
        assert!(
            result.is_err(),
            "A symlink ancestor must refuse before clearing"
        );
        assert_eq!(
            fs::read_to_string(source.join("repo/keep")).unwrap(),
            "original"
        );
        clear_import_destination(&line, &BTreeSet::new())
            .expect("Unlink the inherited link itself");
        assert!(!line.join("link").exists());
        assert_eq!(
            fs::read_to_string(source.join("repo/keep")).unwrap(),
            "original"
        );
    }

    #[test]
    fn migration_clear_keeps_cache_roots_and_removes_read_only_contents() {
        let fixture = tempfile::tempdir().expect("fixture");
        let cache = fixture.path().join("nested/node_modules");
        fs::create_dir_all(cache.join("package")).expect("cache");
        fs::write(cache.join("package/stale"), "old cache").expect("file");
        fs::set_permissions(cache.join("package"), fs::Permissions::from_mode(0o555))
            .expect("read-only directory");
        let roots = BTreeSet::from([cache.clone()]);
        let result = clear_import_destination(fixture.path(), &roots);
        // Restore write access even when the regression fails, so the fixture can be removed.
        if cache.join("package").exists() {
            fs::set_permissions(cache.join("package"), fs::Permissions::from_mode(0o755))
                .expect("restore mode");
        }
        result.expect("clear read-only cache contents");
        assert!(cache.is_dir());
        assert_eq!(fs::read_dir(cache).expect("cache entries").count(), 0);
    }
}

/// Drop only an inherited working copy. The parent's saved cache remains intact.
#[cfg(target_os = "macos")]
pub(super) fn discard_inherited_cache(path: &Path) -> std::io::Result<()> {
    clear_import_destination(path, &BTreeSet::new())?;
    fs::remove_dir(path)
}

#[cfg(not(target_os = "macos"))]
pub(super) fn discard_inherited_cache(path: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(format!(
        "Cannot convert inherited cache {} to a versioned folder on btrfs; layr cache materialize is needed",
        path.display()
    )))
}
