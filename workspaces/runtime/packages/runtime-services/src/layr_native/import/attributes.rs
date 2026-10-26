//! Preserve attributes through descriptors, without following copied links.
use super::*;
use rustix::fs::{Mode, OFlags, XattrFlags, fgetxattr, flistxattr, fsetxattr, openat};

fn copy_descriptors(source: &File, destination: &File) -> std::io::Result<bool> {
    let count = match flistxattr(source, &mut [] as &mut [u8]) {
        Ok(count) => count,
        Err(error) if std::io::Error::from(error).kind() == std::io::ErrorKind::Unsupported => {
            return Ok(false);
        }
        Err(error) => return Err(error.into()),
    };
    let mut names = vec![0; count];
    let count = flistxattr(source, names.as_mut_slice())?;
    for name in names[..count]
        .split(|byte| *byte == 0)
        .filter(|name| !name.is_empty())
    {
        use std::os::unix::ffi::OsStrExt;
        let name = std::ffi::OsStr::from_bytes(name);
        let count = fgetxattr(source, name, &mut [] as &mut [u8])?;
        let mut value = vec![0; count];
        let count = fgetxattr(source, name, value.as_mut_slice())?;
        let mut existing = vec![0; count];
        if fgetxattr(destination, name, existing.as_mut_slice())
            .is_ok_and(|n| n == count && existing == value)
        {
            continue;
        }
        fsetxattr(destination, name, &value[..count], XattrFlags::empty())?;
    }
    Ok(count != 0)
}

pub(super) fn copy(
    source: &Path,
    relative: &Path,
    destination: &Path,
    target: &Path,
    symlink: bool,
) -> std::io::Result<bool> {
    let (source_parent, source_name) = open_parent(source, relative)?;
    let (destination_parent, destination_name) = open_parent(destination, target)?;
    if !symlink {
        let flags = OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let source = File::from(openat(&source_parent, &source_name, flags, Mode::empty())?);
        let destination = File::from(openat(
            &destination_parent,
            &destination_name,
            flags,
            Mode::empty(),
        )?);
        return copy_descriptors(&source, &destination);
    }
    #[cfg(target_os = "macos")]
    {
        let flags = OFlags::RDONLY | OFlags::SYMLINK | OFlags::NONBLOCK | OFlags::CLOEXEC;
        let source = File::from(openat(&source_parent, &source_name, flags, Mode::empty())?);
        let destination = File::from(openat(
            &destination_parent,
            &destination_name,
            flags,
            Mode::empty(),
        )?);
        if !source.metadata()?.file_type().is_symlink()
            || !destination.metadata()?.file_type().is_symlink()
        {
            return Err(std::io::Error::other("An attribute link changed"));
        }
        copy_descriptors(&source, &destination)
    }
    #[cfg(target_os = "linux")]
    {
        use rustix::fs::{lgetxattr, llistxattr, lsetxattr};
        use std::os::fd::AsRawFd;
        let source =
            PathBuf::from(format!("/proc/self/fd/{}", source_parent.as_raw_fd())).join(source_name);
        let destination =
            PathBuf::from(format!("/proc/self/fd/{}", destination_parent.as_raw_fd()))
                .join(destination_name);
        let count = llistxattr(&source, &mut [] as &mut [u8])?;
        let mut names = vec![0; count];
        let count = llistxattr(&source, names.as_mut_slice())?;
        for name in names[..count]
            .split(|byte| *byte == 0)
            .filter(|name| !name.is_empty())
        {
            use std::os::unix::ffi::OsStrExt;
            let name = std::ffi::OsStr::from_bytes(name);
            let count = lgetxattr(&source, name, &mut [] as &mut [u8])?;
            let mut value = vec![0; count];
            let count = lgetxattr(&source, name, value.as_mut_slice())?;
            lsetxattr(&destination, name, &value[..count], XattrFlags::empty())?;
        }
        Ok(count != 0)
    }
}
