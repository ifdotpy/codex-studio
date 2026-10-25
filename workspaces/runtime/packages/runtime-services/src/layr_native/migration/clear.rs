//! Replace ordinary files without removing the btrfs cache subvolume roots.
use std::{
    collections::BTreeSet,
    fs,
    path::{Path, PathBuf},
};

pub(super) fn clear_import_destination(
    destination: &Path,
    cache_roots: &BTreeSet<PathBuf>,
) -> std::io::Result<()> {
    if !destination.exists() {
        return Ok(());
    }
    use std::os::unix::fs::PermissionsExt;
    let permissions = fs::metadata(destination)?.permissions().mode();
    fs::set_permissions(destination, fs::Permissions::from_mode(permissions | 0o700))?;
    for entry in fs::read_dir(destination)? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_type()?.is_dir() {
            clear_import_destination(&path, cache_roots)?;
            if !cache_roots.iter().any(|cache| cache.starts_with(&path)) {
                fs::remove_dir(&path)?;
            }
        } else {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
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
