//! Owned process groups: exit without reaping, and verification after reaping.
//! This never sends a signal.
//!
//! A group is signalled by its ID, which is the leader's PID. Once the leader
//! is reaped and the group is empty, the system can reuse that ID for another
//! group. While an exited leader is not reaped, its PID stays allocated, so
//! the ID cannot name another group. Callers therefore wait for the leader's
//! exit with [`exited_unreaped`], signal the group, and only then reap.

/// Whether the owned child `pid` has exited, without reaping it (`waitid` with
/// `WNOWAIT`). `Err` when `pid` is not an unreaped child of this process: then
/// its group ID may already name another group, and the caller must not signal it.
#[cfg(unix)]
pub fn exited_unreaped_now(pid: u32) -> std::io::Result<bool> {
    use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
    let pid = i32::try_from(pid)
        .ok()
        .and_then(Pid::from_raw)
        .ok_or_else(|| std::io::Error::from(std::io::ErrorKind::InvalidInput))?;
    loop {
        match waitid(
            WaitId::Pid(pid),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        ) {
            Ok(status) => return Ok(status.is_some()),
            Err(rustix::io::Errno::INTR) => {}
            Err(error) => return Err(error.into()),
        }
    }
}

/// Wait until the owned child `pid` exits, without reaping it. Each SIGCHLD
/// wakes one check; the signal stream exists before the first check, so no
/// exit is missed.
#[cfg(unix)]
pub async fn exited_unreaped(pid: u32) -> std::io::Result<()> {
    let mut child_signals = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::child())?;
    while !exited_unreaped_now(pid)? {
        if child_signals.recv().await.is_none() {
            return Err(std::io::Error::other("the child signal stream ended"));
        }
    }
    Ok(())
}

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

/// [`vanished_after_reap`], polled until `deadline`. A Darwin group can keep an
/// exited member that is not reaped yet for a moment after the caller reaps its
/// leader, also after a clean exit; the group then answers EPERM until it is gone.
pub(crate) async fn vanishes_after_reap(
    pid: Option<u32>,
    error: &std::io::Error,
    deadline: tokio::time::Instant,
) -> bool {
    /// Time between two probes of the reaped group.
    const GROUP_POLL_INTERVAL: std::time::Duration = std::time::Duration::from_millis(5);
    loop {
        if vanished_after_reap(pid, error) {
            return true;
        }
        if !cfg!(target_os = "macos")
            || error.raw_os_error() != Some(rustix::io::Errno::PERM.raw_os_error())
            || tokio::time::Instant::now() >= deadline
        {
            return false;
        }
        tokio::time::sleep(GROUP_POLL_INTERVAL).await;
    }
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::{vanished_after_reap, vanishes_after_reap};
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

    #[tokio::test]
    async fn permission_error_waits_until_the_exited_group_is_reaped() {
        use rustix::io::Errno;
        use rustix::process::{Pid, test_kill_process_group};
        use std::time::Duration;
        use tokio::time::Instant;

        let mut child = Command::new("/usr/bin/true")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0)
            .spawn()
            .expect("owned child");
        let pid = Some(child.id());
        let group = i32::try_from(child.id())
            .ok()
            .and_then(Pid::from_raw)
            .expect("owned group");
        // A Darwin group whose only member exited but is not reaped answers EPERM.
        let exited_by = Instant::now() + Duration::from_secs(10);
        while test_kill_process_group(group) != Err(Errno::PERM) {
            assert!(Instant::now() < exited_by, "the owned child exits");
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        let denied = std::io::Error::from_raw_os_error(Errno::PERM.raw_os_error());
        assert!(
            !vanished_after_reap(pid, &denied),
            "one probe of the unreaped group refuses"
        );
        assert!(
            !vanishes_after_reap(pid, &denied, Instant::now() + Duration::from_millis(20)).await,
            "an unreaped group does not vanish by the deadline"
        );
        let (vanished, reaped) = tokio::join!(
            vanishes_after_reap(pid, &denied, Instant::now() + Duration::from_secs(10)),
            async {
                tokio::time::sleep(Duration::from_millis(50)).await;
                child.wait()
            }
        );
        reaped.expect("reap owned child");
        assert!(vanished, "the group vanishes once its member is reaped");
    }
}
