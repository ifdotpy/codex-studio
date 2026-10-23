//! APFS `clonefile(2)` for host_exec slots. A clone shares its blocks with the
//! origin; a failed clone is an error and never becomes a byte copy.

use std::io;
use std::path::Path;

/// `CLONE_NOFOLLOW` from `<sys/clonefile.h>`: do not follow a link at the source.
#[cfg(target_os = "macos")]
const CLONE_NOFOLLOW: u32 = 0x0001;

/// Clone one regular file without following a link at `source`.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub(crate) fn clone_file(source: &Path, copy: &Path) -> io::Result<()> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;
    let invalid = || io::Error::new(io::ErrorKind::InvalidInput, "the path contains a NUL byte");
    let source_text = CString::new(source.as_os_str().as_bytes()).map_err(|_| invalid())?;
    let copy_text = CString::new(copy.as_os_str().as_bytes()).map_err(|_| invalid())?;
    // SAFETY: both arguments are NUL-terminated paths that live until the call
    // returns; clonefile reads them and writes no caller memory.
    let result =
        unsafe { libc::clonefile(source_text.as_ptr(), copy_text.as_ptr(), CLONE_NOFOLLOW) };
    if result == 0 {
        Ok(())
    } else {
        let error = io::Error::last_os_error();
        Err(io::Error::new(
            error.kind(),
            format!("{error}: {}", source.display()),
        ))
    }
}

/// Other systems have no clonefile: each clone fails, and the transfer sends the file.
#[cfg(not(target_os = "macos"))]
pub(crate) fn clone_file(_source: &Path, _copy: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "clonefile is not available",
    ))
}
