//! Minimal parent-side Linux I/O priority call missing from safe OS crates.

/// Set Linux best-effort I/O priority at its lowest level for an owned, positive process group ID.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn set_group_io_best_effort(pgid: i32) -> std::io::Result<()> {
    const IOPRIO_WHO_PGRP: libc::c_int = 2;
    const IOPRIO_CLASS_BEST_EFFORT: libc::c_int = 2;
    const IOPRIO_BEST_EFFORT_LOWEST_LEVEL: libc::c_int = 7;
    const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
    if pgid <= 0 {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    // SAFETY: ioprio_set takes three scalar integers, no pointers. The positive group
    // identifier cannot mean the caller's current group. No memory is accessed by the ABI.
    let result = unsafe {
        libc::syscall(
            libc::SYS_ioprio_set,
            IOPRIO_WHO_PGRP,
            pgid,
            (IOPRIO_CLASS_BEST_EFFORT << IOPRIO_CLASS_SHIFT) | IOPRIO_BEST_EFFORT_LOWEST_LEVEL,
        )
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
