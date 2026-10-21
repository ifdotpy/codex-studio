//! Read-only macOS process queries missing from safe OS crates.
//!
//! The supervisor and the PTY host verify a process before they signal it.
//! Linux reads `/proc`; macOS has no such file system, so this leaf crate
//! wraps the libSystem calls that give the same facts: `proc_pidinfo` with
//! `PROC_PIDTBSDINFO` (start time, process group, state), `proc_listpids` with
//! `PROC_ALL_PIDS` (process enumeration), `sysctl` `KERN_PROCARGS2` (launch
//! environment, as `/proc/<pid>/environ`) and `sysctlbyname` for
//! `kern.bootsessionuuid` (boot identity). No call writes kernel state.

#[cfg(target_os = "macos")]
mod macos {
    #![allow(unsafe_code)]

    use std::ffi::c_int;
    use std::io;
    use std::mem::{MaybeUninit, size_of};

    /// `PROC_ALL_PIDS` from `<sys/proc_info.h>`; libc does not export it.
    const PROC_ALL_PIDS: u32 = 1;
    /// Upper bound for a `kern.bootsessionuuid` value (36 characters and NUL).
    const BOOT_SESSION_UUID_BYTES: usize = 64;
    /// Bounded retries when the process table grows between size and read.
    const LIST_ATTEMPTS: usize = 4;

    /// The `PROC_PIDTBSDINFO` fields the runtime uses, copied out of the
    /// kernel record. `start_*` is calendar time of `fork` in the Unix epoch.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct BsdInfo {
        pub pid: u32,
        pub process_group: u32,
        /// The kernel `p_stat` value; see [`BsdInfo::is_zombie`].
        pub status: u32,
        pub start_seconds: u64,
        pub start_microseconds: u64,
    }

    impl BsdInfo {
        /// `SZOMB`: the process exited and waits for its parent to reap it.
        pub fn is_zombie(&self) -> bool {
            self.status == libc::SZOMB
        }
    }

    /// Read the BSD record of one process. `Ok(None)` means the kernel reported
    /// `ESRCH`. Without `include_zombie` the kernel also reports `ESRCH` for a
    /// zombie; with it, a zombie record is returned with `SZOMB` status.
    /// Another user's process gives `EPERM`, as in Python's libproc read.
    pub fn bsd_info(pid: u32, include_zombie: bool) -> io::Result<Option<BsdInfo>> {
        let raw = c_int::try_from(pid)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let size = c_int::try_from(size_of::<libc::proc_bsdinfo>())
            .map_err(|_| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut record = MaybeUninit::<libc::proc_bsdinfo>::zeroed();
        // SAFETY: `record` is a writable, properly aligned `proc_bsdinfo` of
        // exactly `size` bytes that lives across the call. PROC_PIDTBSDINFO
        // writes at most `size` bytes. The argument is a plain scalar flag.
        let read = unsafe {
            libc::proc_pidinfo(
                raw,
                libc::PROC_PIDTBSDINFO,
                u64::from(include_zombie),
                record.as_mut_ptr().cast(),
                size,
            )
        };
        if read <= 0 {
            let error = io::Error::last_os_error();
            if error.raw_os_error() == Some(libc::ESRCH) {
                return Ok(None);
            }
            return Err(error);
        }
        if read != size {
            return Err(io::Error::other("short libproc BSD record"));
        }
        // SAFETY: the buffer started zeroed and every field of `proc_bsdinfo`
        // is an integer or an integer array, so each bit pattern is valid.
        let record = unsafe { record.assume_init() };
        if record.pbi_pid != pid {
            return Err(io::Error::other("libproc returned another process"));
        }
        Ok(Some(BsdInfo {
            pid: record.pbi_pid,
            process_group: record.pbi_pgid,
            status: record.pbi_status,
            start_seconds: record.pbi_start_tvsec,
            start_microseconds: record.pbi_start_tvusec,
        }))
    }

    /// List every positive process ID in the system.
    pub fn all_pids() -> io::Result<Vec<u32>> {
        for _ in 0..LIST_ATTEMPTS {
            // SAFETY: a null buffer with zero size asks only for the byte
            // estimate; the kernel writes no memory.
            let estimate =
                unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, std::ptr::null_mut(), 0) };
            if estimate <= 0 {
                return Err(io::Error::last_os_error());
            }
            let capacity = usize::try_from(estimate)
                .map_err(|_| io::Error::other("invalid libproc estimate"))?
                / size_of::<c_int>()
                + 64;
            let mut pids = vec![0 as c_int; capacity];
            let bytes = c_int::try_from(capacity * size_of::<c_int>())
                .map_err(|_| io::Error::other("process table is too large"))?;
            // SAFETY: `pids` is a live, writable `c_int` buffer of `bytes`
            // bytes; the kernel writes at most `bytes` bytes into it.
            let written =
                unsafe { libc::proc_listpids(PROC_ALL_PIDS, 0, pids.as_mut_ptr().cast(), bytes) };
            if written <= 0 {
                return Err(io::Error::last_os_error());
            }
            let count = usize::try_from(written)
                .map_err(|_| io::Error::other("invalid libproc count"))?
                / size_of::<c_int>();
            if count >= capacity {
                // A full buffer may be truncated; ask again with a new size.
                continue;
            }
            pids.truncate(count);
            return Ok(pids
                .into_iter()
                .filter_map(|pid| u32::try_from(pid).ok().filter(|pid| *pid > 0))
                .collect());
        }
        Err(io::Error::other(
            "process table kept growing during listing",
        ))
    }

    /// Read the raw `KERN_PROCARGS2` record of one process: a native-endian
    /// `argc`, the executable path, padding, the argument strings, the
    /// environment strings and the Apple strings. The kernel allows this only
    /// for a process of the same user. A record above `limit` bytes is refused.
    pub fn process_arguments(pid: u32, limit: usize) -> io::Result<Vec<u8>> {
        let raw = c_int::try_from(pid)
            .ok()
            .filter(|value| *value > 0)
            .ok_or_else(|| io::Error::from(io::ErrorKind::InvalidInput))?;
        let mut name = [libc::CTL_KERN, libc::KERN_PROCARGS2, raw];
        let mut size = 0_usize;
        // SAFETY: `name` is a live array of three MIB integers, matching the
        // length argument. A null old buffer asks only for the size, which the
        // kernel writes into `size`, a valid `size_t`. No new value is set.
        let result = unsafe {
            libc::sysctl(
                name.as_mut_ptr(),
                3,
                std::ptr::null_mut(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        if !(size_of::<c_int>()..=limit).contains(&size) {
            return Err(io::Error::other("process launch record exceeds its limit"));
        }
        let mut buffer = vec![0_u8; size];
        // SAFETY: `buffer` is a live, writable allocation of `size` bytes and
        // `size` is a valid in/out `size_t`; the kernel writes at most `size`
        // bytes and stores the written length back into `size`.
        let result = unsafe {
            libc::sysctl(
                name.as_mut_ptr(),
                3,
                buffer.as_mut_ptr().cast(),
                &mut size,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        buffer.truncate(size);
        Ok(buffer)
    }

    /// The random UUID the kernel creates at each boot (`kern.bootsessionuuid`).
    pub fn boot_session_uuid() -> io::Result<String> {
        let mut buffer = [0_u8; BOOT_SESSION_UUID_BYTES];
        let mut length = buffer.len();
        // SAFETY: the name is a NUL-terminated C string literal; `buffer` is
        // writable for `length` bytes and `length` is a valid in/out pointer.
        // A null new value with zero length makes this a read only.
        let result = unsafe {
            libc::sysctlbyname(
                c"kern.bootsessionuuid".as_ptr(),
                buffer.as_mut_ptr().cast(),
                &mut length,
                std::ptr::null_mut(),
                0,
            )
        };
        if result != 0 {
            return Err(io::Error::last_os_error());
        }
        let value = buffer
            .get(..length.min(buffer.len()))
            .unwrap_or_default()
            .split(|byte| *byte == 0)
            .next()
            .unwrap_or_default();
        let value = std::str::from_utf8(value)
            .map_err(|_| io::Error::other("boot session UUID is not text"))?;
        if value.len() != 36 || !value.bytes().all(|c| c.is_ascii_hexdigit() || c == b'-') {
            return Err(io::Error::other("boot session UUID has an unknown format"));
        }
        Ok(value.to_owned())
    }

    #[cfg(test)]
    mod tests {
        #![allow(clippy::unwrap_used, clippy::disallowed_methods)]
        use super::*;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        #[test]
        fn own_process_record_matches_pid_and_group() {
            let pid = std::process::id();
            let info = bsd_info(pid, false).unwrap().unwrap();
            assert_eq!(info.pid, pid);
            assert!(!info.is_zombie());
            assert!(info.start_seconds > 0);
            assert!(info.start_microseconds < 1_000_000);
            let group = current_group();
            assert_eq!(info.process_group, group);
            assert!(all_pids().unwrap().contains(&pid));
        }

        fn current_group() -> u32 {
            // SAFETY: getpgrp takes no arguments and cannot fail.
            u32::try_from(unsafe { libc::getpgrp() }).unwrap()
        }

        #[test]
        fn zombie_is_esrch_without_flag_and_szomb_with_flag() {
            let mut child = Command::new("/usr/bin/true")
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            let pid = child.id();
            // The test never waits until the child is a zombie, so the parent
            // owns the PID and it cannot be reused during the check.
            let deadline = Instant::now() + Duration::from_secs(5);
            let zombie = loop {
                if let Some(info) = bsd_info(pid, true).unwrap()
                    && info.is_zombie()
                {
                    break info;
                }
                assert!(Instant::now() < deadline, "child did not become a zombie");
                std::thread::sleep(Duration::from_millis(5));
            };
            assert_eq!(zombie.pid, pid);
            assert_eq!(bsd_info(pid, false).unwrap(), None);
            child.wait().unwrap();
            assert_eq!(bsd_info(pid, true).unwrap(), None);
        }

        #[test]
        fn boot_session_uuid_is_stable() {
            let first = boot_session_uuid().unwrap();
            assert_eq!(first, boot_session_uuid().unwrap());
            assert_eq!(first.len(), 36);
        }
    }
}

#[cfg(target_os = "macos")]
pub use macos::{BsdInfo, all_pids, boot_session_uuid, bsd_info, process_arguments};
