//! Minimal parent-side priority calls missing from safe OS crates.

/// Set Linux idle I/O priority for an owned, positive process group ID.
#[cfg(target_os = "linux")]
#[allow(unsafe_code)]
pub fn set_group_io_idle(pgid: i32) -> std::io::Result<()> {
    const IOPRIO_WHO_PGRP: libc::c_int = 2;
    const IOPRIO_CLASS_IDLE: libc::c_int = 3 << 13;
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
            IOPRIO_CLASS_IDLE,
        )
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

/// Set Darwin background policy for an owned, positive process ID.
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
pub fn set_process_background(pid: i32) -> std::io::Result<()> {
    if pid <= 0 {
        return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
    }
    // SAFETY: setpriority takes only scalar integers, with a checked positive pid,
    // so it cannot target the caller implicitly. The ABI does not access memory.
    let result = unsafe {
        libc::setpriority(
            libc::PRIO_DARWIN_PROCESS,
            pid as libc::id_t,
            libc::PRIO_DARWIN_BG,
        )
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}
