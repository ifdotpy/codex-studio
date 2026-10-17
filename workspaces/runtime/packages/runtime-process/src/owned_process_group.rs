//! Verification after reaping an owned child. This never sends a signal.

pub(crate) fn vanished_after_reap(pid: Option<u32>, error: &std::io::Error) -> bool {
    #[cfg(target_os = "macos")]
    {
        // Darwin can reject signals to an unreaped, exited group with EPERM.
        // Only forgive that result after the caller reaps and the group is gone.
        if error.raw_os_error() != Some(rustix::io::Errno::PERM.raw_os_error()) {
            return false;
        }
        pid.and_then(|pid| i32::try_from(pid).ok())
            .and_then(rustix::process::Pid::from_raw)
            .is_some_and(|group| {
                rustix::process::test_kill_process_group(group) == Err(rustix::io::Errno::SRCH)
            })
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = (pid, error);
        false
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::vanished_after_reap;
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};

    #[test]
    fn permission_error_is_only_ignorable_after_the_owned_group_disappears() {
        let mut child = Command::new("/bin/cat")
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("owned child");
        let pid = Some(child.id());
        let denied = std::io::Error::from_raw_os_error(rustix::io::Errno::PERM.raw_os_error());
        let live = vanished_after_reap(pid, &denied);
        child.kill().expect("stop owned child");
        child.wait().expect("reap owned child");
        assert!(!live, "permission errors for a live group must survive");
        assert!(vanished_after_reap(pid, &denied));
        assert!(!vanished_after_reap(None, &denied));
        assert!(!vanished_after_reap(
            pid,
            &std::io::Error::other("unrelated failure")
        ));
    }
}
