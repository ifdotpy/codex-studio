//! Replace ordinary files without removing the btrfs cache subvolume roots.
#[cfg(test)]
use std::fs;
use std::os::unix::ffi::OsStrExt;
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use super::super::private_tree::{Directory, flags as directory_flags};

#[cfg(test)]
pub(super) fn clear_import_destination(
    destination: &Path,
    cache_roots: &BTreeSet<PathBuf>,
) -> std::io::Result<()> {
    let fd = Directory::open(destination)?;
    let relative = cache_roots
        .iter()
        .map(|p| {
            p.strip_prefix(destination)
                .map(Path::to_owned)
                .map_err(std::io::Error::other)
        })
        .collect::<std::io::Result<_>>()?;
    clear_at(&fd, &relative)
}
pub(super) fn clear_at(
    destination: &Directory,
    cache_roots: &BTreeSet<PathBuf>,
) -> std::io::Result<()> {
    clear_directory(&destination.0, Path::new(""), cache_roots)
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

/// Drop only an inherited working copy. The parent's saved cache remains intact.
#[cfg(target_os = "macos")]
pub(super) fn discard_inherited_cache(line: &Directory, relative: &Path) -> std::io::Result<()> {
    let cache = line.child(relative, false)?;
    clear_at(&cache, &BTreeSet::new())?;
    let (parent, name) = line.parent(relative, false)?;
    rustix::fs::unlinkat(&parent.0, name, rustix::fs::AtFlags::REMOVEDIR)?;
    Ok(())
}
#[cfg(not(target_os = "macos"))]
pub(super) fn discard_inherited_cache(_line: &Directory, relative: &Path) -> std::io::Result<()> {
    Err(std::io::Error::other(format!(
        "Cannot convert inherited cache {} to a versioned folder on btrfs; layr cache materialize is needed",
        relative.display()
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn review_clear_refuses_symlink_destination_and_preserves_source() {
        let fixture = tempfile::tempdir().expect("fixture operation succeeds");
        let source = fixture.path().join("source");
        let line = fixture.path().join("line");
        fs::create_dir_all(source.join("repo")).expect("fixture operation succeeds");
        fs::create_dir(&line).expect("fixture operation succeeds");
        fs::write(source.join("repo/keep"), "original").expect("fixture operation succeeds");
        std::os::unix::fs::symlink(&source, line.join("link")).expect("fixture operation succeeds");
        let result = clear_import_destination(&line.join("link/repo"), &BTreeSet::new());
        assert!(
            result.is_err(),
            "A symlink ancestor must refuse before clearing"
        );
        assert_eq!(
            fs::read_to_string(source.join("repo/keep")).expect("fixture operation succeeds"),
            "original"
        );
        clear_import_destination(&line, &BTreeSet::new())
            .expect("Unlink the inherited link itself");
        assert!(!line.join("link").exists());
        assert_eq!(
            fs::read_to_string(source.join("repo/keep")).expect("fixture operation succeeds"),
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
