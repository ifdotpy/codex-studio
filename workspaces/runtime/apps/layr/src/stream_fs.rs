//! Metadata operations for the portable send-stream implementation.
#[cfg(target_os = "macos")]
pub use crate::sys::{
    apfs_attribute as attribute, apfs_clear_acl as clear_acl, apfs_has_acl as has_acl, apfs_mtime as mtime,
    apfs_publish as publish, apfs_remove_attribute as remove_attribute, apfs_xattrs as xattrs,
};
#[cfg(feature = "overlayfs")]
pub use crate::sys::{
    overlay_attribute as attribute, overlay_mtime as mtime, overlay_remove_attribute as remove_attribute,
    overlay_xattrs as xattrs,
};
#[cfg(feature = "overlayfs")]
pub fn has_acl(path: &std::path::Path) -> std::io::Result<bool> {
    Ok(xattrs(path)?.iter().any(|(n, _)| n.starts_with(b"system.posix_acl_") || n.starts_with(b"security.")))
}
#[cfg(feature = "overlayfs")]
pub fn publish(from: &std::path::Path, to: &std::path::Path) -> anyhow::Result<()> {
    crate::storage::rename_tree(from, to)
}
