//! Clear deletion flags only through pinned private import descriptors.
#[cfg(target_os = "macos")]
#[allow(
    unsafe_code,
    reason = "Clear deletion flags only through a pinned private entry descriptor"
)]
pub(super) fn unseal(fd: &std::os::fd::OwnedFd) -> std::io::Result<()> {
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
