//! The root service (`layr daemon`) and the client side of `layr` for other users.
//!
//! A client sends its arguments, working folder and environment over a Unix socket, with its
//! standard input, output and error and an open handle of its working folder as `SCM_RIGHTS`.
//! The service forks one process per request (the parent stays single threaded), takes the
//! caller's uid from `SO_PEERCRED`, runs the command with the caller's streams and returns the
//! exit code. Commands check the caller against line owners and the project admin.

use crate::ctx::{Caller, Ctx};
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

pub fn socket_path() -> PathBuf {
    std::env::var_os("LAYR_SOCKET").map(PathBuf::from).unwrap_or_else(|| {
        PathBuf::from(if cfg!(target_os = "macos") { "/var/run/layr/layr.sock" } else { "/run/layr/layr.sock" })
    })
}

const MAX_REQ: usize = 4 << 20;

fn send_with_fds(sock: &UnixStream, data: &[u8], fds: &[BorrowedFd]) -> Result<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(fds)) {
        bail!("too many descriptors");
    }
    #[cfg(target_os = "linux")]
    let flags = SendFlags::NOSIGNAL;
    #[cfg(target_os = "macos")]
    let flags = SendFlags::empty();
    let n = rustix::net::sendmsg(sock, &[std::io::IoSlice::new(data)], &mut control, flags)?;
    if n < data.len() {
        (&*sock).write_all(&data[n..])?;
    }
    Ok(())
}

fn recv_with_fds(sock: &UnixStream, buf: &mut [u8]) -> Result<(usize, Vec<OwnedFd>)> {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    #[cfg(target_os = "linux")]
    let flags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(target_os = "macos")]
    let flags = RecvFlags::empty();
    let r = rustix::net::recvmsg(sock, &mut [std::io::IoSliceMut::new(buf)], &mut control, flags)?;
    let mut fds = Vec::new();
    for m in control.drain() {
        if let RecvAncillaryMessage::ScmRights(it) = m {
            fds.extend(it);
        }
    }
    #[cfg(target_os = "macos")]
    for fd in &fds {
        rustix::io::fcntl_setfd(fd, rustix::io::FdFlags::CLOEXEC)?;
    }
    Ok((r.bytes, fds))
}

/// Client: run a command through the service. Returns the exit code.
pub fn forward(argv: &[String]) -> Result<i32> {
    let path = socket_path();
    let sock = UnixStream::connect(&path).map_err(|e| connect_error(&path, &e))?;
    let cwd = std::env::current_dir()?;
    let env: HashMap<String, String> = std::env::vars().collect();
    let req = serde_json::to_vec(&serde_json::json!({"argv": argv, "cwd": cwd, "env": env}))?;
    let dir = std::fs::File::open(&cwd)?;
    let mut data = (req.len() as u32).to_be_bytes().to_vec();
    data.extend_from_slice(&req);
    // The service answers "B" at once when it is busy, or "R" and the exit code at the end. A
    // busy service may close the connection before it reads the request, so the answer is read
    // even when sending failed; without an answer, the send error is the reason.
    let sent = send_with_fds(
        &sock,
        &data,
        &[std::io::stdin().as_fd(), std::io::stdout().as_fd(), std::io::stderr().as_fd(), dir.as_fd()],
    );
    let mut kind = [0u8; 1];
    if let Err(e) = (&sock).read_exact(&mut kind) {
        sent.context("cannot send the request to the layr service")?;
        return Err(anyhow!("the layr service closed the connection ({e})"));
    }
    if kind[0] == b'B' {
        bail!("the layr service is busy (too many running requests); try again");
    }
    sent.context("cannot send the request to the layr service")?;
    if kind[0] != b'R' {
        bail!("unexpected answer from the layr service");
    }
    let mut code = [0u8; 4];
    (&sock).read_exact(&mut code).context("the layr service closed the connection")?;
    Ok(i32::from_be_bytes(code))
}

/// Why a client could not connect, and what to do. The service opens its socket to every local
/// user (mode 0666 in a 0755 folder) and decides each request by the caller's uid; so a refused
/// connection comes from the client's side or from a missing service, never from a role.
fn connect_error(path: &Path, e: &std::io::Error) -> anyhow::Error {
    let at = path.display();
    match e.raw_os_error() {
        // connect() itself refused by a system call filter (seccomp) or a sandbox profile: the
        // service never saw the request. File permissions give EACCES, not EPERM.
        Some(code) if code == rustix::io::Errno::PERM.raw_os_error() => anyhow!(
            "cannot connect to the layr service at {at} ({e}). The socket is open to every user; the \
             connect() system call was refused before it reached the service, usually by a sandbox or \
             seccomp filter of this process. Run layr outside that sandbox, or allow connect() to {at}."
        ),
        Some(code) if code == rustix::io::Errno::ACCESS.raw_os_error() => anyhow!(
            "cannot connect to the layr service at {at} ({e}). This user may not open the socket or a \
             folder above it; the service makes the socket mode 0666 in a folder of mode 0755. Check \
             those modes, or a security module (AppArmor, SELinux) that limits this process."
        ),
        Some(code)
            if code == rustix::io::Errno::NOENT.raw_os_error() || code == rustix::io::Errno::CONNREFUSED.raw_os_error() =>
        {
            anyhow!(
                "cannot reach the layr service at {at} ({e}). Start it as root with 'layr daemon', set \
                 LAYR_SOCKET to its socket, or run layr as root. A sandbox that hides {at} gives the \
                 same error."
            )
        }
        _ => anyhow!("cannot reach the layr service at {at} ({e}). Start it as root with 'layr daemon', or run layr as root."),
    }
}

#[cfg(target_os = "linux")]
fn peer_uid(sock: &UnixStream) -> Result<(u32, u32, i32)> {
    let c = rustix::net::sockopt::socket_peercred(sock)?;
    Ok((c.uid.as_raw(), c.gid.as_raw(), c.pid.as_raw_nonzero().get()))
}

fn limit(var: &str, default: usize) -> usize {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Child process: serve one request, then exit.
fn handle(sock: UnixStream, root: &Path, registry_uids: &[u32]) -> i32 {
    let res = (|| -> Result<i32> {
        let (uid, _gid, _pid) = peer_uid(&sock)?;
        // A client that does not send its request in time does not hold a process.
        sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        let mut first = vec![0u8; 65536];
        let (n, fds) = recv_with_fds(&sock, &mut first)?;
        if n < 4 {
            bail!("bad request");
        }
        let len = u32::from_be_bytes([first[0], first[1], first[2], first[3]]) as usize;
        if len > MAX_REQ || (fds.is_empty() && len > 32768) {
            bail!("request too large");
        }
        let mut req = first[4..n].to_vec();
        if req.len() > len {
            bail!("request exceeds its declared size");
        }
        while req.len() < len {
            let mut more = vec![0u8; len - req.len()];
            let k = (&sock).read(&mut more)?;
            if k == 0 {
                bail!("short request");
            }
            req.extend_from_slice(&more[..k]);
        }
        let v: serde_json::Value = serde_json::from_slice(&req)?;
        if let Some(query) = v.get("registry") {
            if !fds.is_empty() {
                bail!("registry request must not contain descriptors");
            }
            crate::oci::registry::serve_backend(sock.try_clone()?, root, uid, query.clone())?;
            return Ok(0);
        }
        if registry_uids.contains(&uid) {
            bail!("registry user may use only registry read requests");
        }
        if fds.len() != 4 {
            bail!("bad request: expected 4 descriptors");
        }
        let argv: Vec<String> = serde_json::from_value(v["argv"].clone())?;
        let env: HashMap<String, String> = serde_json::from_value(v["env"].clone()).unwrap_or_default();
        rustix::stdio::dup2_stdin(&fds[0])?;
        rustix::stdio::dup2_stdout(&fds[1])?;
        rustix::stdio::dup2_stderr(&fds[2])?;
        rustix::process::fchdir(&fds[3]).context("cannot enter the working folder")?;
        drop(fds);
        let cwd = std::env::current_dir()?;
        Ok(run_as(argv, Caller::from_uid(uid), cwd, env, root))
    })();
    let code = match res {
        Ok(c) => c,
        Err(e) => {
            eprintln!("layr service: {e:#}");
            128
        }
    };
    let mut reply = vec![b'R'];
    reply.extend_from_slice(&code.to_be_bytes());
    let _ = (&sock).write_all(&reply);
    code
}

/// Run a command for a caller and return its exit code (errors are printed to stderr).
pub fn run_as(argv: Vec<String>, caller: Caller, cwd: PathBuf, env: HashMap<String, String>, root: &Path) -> i32 {
    let store = match Store::open(root) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("fatal: cannot open the layr store {}: {e:#}", root.display());
            return 128;
        }
    };
    let out: Box<dyn Write> = Box::new(std::io::BufWriter::new(std::io::stdout()));
    let ctx = Ctx { store, caller, cwd, env, out: std::cell::RefCell::new(out) };
    let r = crate::cmd::run(&ctx, &argv);
    let _ = ctx.out.borrow_mut().flush();
    match r {
        Ok(c) => c,
        Err(e) => {
            if let Some(x) = e.downcast_ref::<crate::cmd::Exit>() {
                if !x.1.is_empty() {
                    eprintln!("{}", x.1);
                }
                x.0
            } else {
                eprintln!("fatal: {e:#}");
                128
            }
        }
    }
}

/// The time of the last periodic check of each line that found no change, in the project's meta
/// folder. It is local to this machine and is never sent to other machines.
const PERIODIC_CHECKS: &str = "periodic-checks.json";

/// Periodic work: automatic states of busy lines, access to the git store, retention.
fn periodic(root: &Path, hourly: bool) {
    let now = crate::ids::now_ms();
    let store = match Store::open(root) {
        Ok(s) => s,
        Err(_) => return,
    };
    let caller = Caller::from_uid(0);
    let names = store.project_names().unwrap_or_default();
    drop(store);
    for name in names {
        let store = match Store::open(root) {
            Ok(s) => s,
            Err(_) => return,
        };
        let ctx = Ctx {
            store,
            caller: caller.clone(),
            cwd: PathBuf::from("/"),
            env: HashMap::new(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let repo = match crate::repo::Repo::open(&ctx, &name) {
            Ok(r) => r,
            Err(_) => continue,
        };
        let interval = repo.p.config_u64("snapshot.interval", 600) as i64 * 1000;
        // A check that found no change saves no state, so the time of the last automatic state
        // stays old. The time of the check keeps an idle line from a snapshot and a comparison
        // on every tick.
        let checks_path = repo.p.meta_dir().join(PERIODIC_CHECKS);
        let mut checks: HashMap<String, i64> =
            std::fs::read(&checks_path).ok().and_then(|data| serde_json::from_slice(&data).ok()).unwrap_or_default();
        let before = checks.clone();
        if let Ok(lines) = repo.lines() {
            checks.retain(|id, _| lines.iter().any(|l| &l.id == id));
            for l in lines {
                if l.machine != repo.p.store.machine.id || !repo.working(&l).exists() {
                    continue;
                }
                let saved = repo.last_auto(&l).ok().flatten().map(|s| s.time).unwrap_or(0);
                let last = saved.max(checks.get(&l.id).copied().unwrap_or(0));
                if now - last >= interval {
                    if let Ok(_lock) = repo.p.lock() {
                        if let Ok(None) = repo.save(&ctx, &l, "periodic", true, false) {
                            checks.insert(l.id.clone(), now);
                        }
                    }
                }
            }
        }
        if checks != before {
            if let Ok(data) = serde_json::to_vec(&checks) {
                let _ = crate::fsutil::atomic_write(&checks_path, &data, 0o600);
            }
        }
        if let Err(error) = crate::cmd::queue::run_configured(&ctx, &repo, None) {
            eprintln!("layr queue ({name}): {error:#}");
        }
        if hourly {
            // Also repairs the git store of a project from an earlier layr, soon after a start.
            if let Ok(_lock) = repo.p.lock() {
                let _ = crate::gitbridge::sync_store_access(&repo);
            }
            let _ = crate::cmd::admin::run_gc(&ctx, &repo, false);
        }
    }
}

pub fn daemon(args: &[String]) -> Result<i32> {
    if !rustix::process::geteuid().is_root() {
        bail!("layr daemon must run as root");
    }
    let mut sock = socket_path();
    let mut tick = 60u64;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--socket" => {
                sock = PathBuf::from(args.get(i + 1).ok_or_else(|| anyhow!("--socket needs a path"))?);
                i += 1;
            }
            "--tick" => {
                tick = args.get(i + 1).ok_or_else(|| anyhow!("--tick needs seconds"))?.parse()?;
                i += 1;
            }
            x => bail!("unknown option {x}"),
        }
        i += 1;
    }
    let root = crate::store::default_root();
    #[cfg(target_os = "macos")]
    {
        let existing = root.ancestors().find(|p| p.exists()).context("store has no existing parent")?;
        if !crate::sys::apfs_owners_enabled(existing)? {
            bail!("the APFS service requires a volume with ownership enabled (diskutil enableOwnership <volume>)");
        }
        use std::os::unix::fs::MetadataExt;
        for path in [&root, &root.join("machine"), &root.join("projects")] {
            if let Ok(m) = std::fs::symlink_metadata(path) {
                if !m.is_dir() || m.uid() != 0 || m.mode() & 0o022 != 0 {
                    bail!(
                        "the APFS service requires root-owned store directories without group or public write access: {}",
                        path.display()
                    );
                }
            }
        }
    }
    Store::open(&root)?;
    if let Some(d) = sock.parent() {
        std::fs::create_dir_all(d)?;
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755))?;
    }
    if sock.exists() {
        if UnixStream::connect(&sock).is_ok() {
            bail!("another layr service is running on {}", sock.display());
        }
        std::fs::remove_file(&sock)?;
    }
    let registry_listeners = crate::oci::registry::listeners(&root)?;
    let registry_uids: Vec<u32> = registry_listeners.iter().map(|(_, _, user)| user.uid).collect();
    let listener = UnixListener::bind(&sock)?;
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&sock, std::fs::Permissions::from_mode(0o666))?;
    }
    eprintln!("layr service: listening on {} (store {})", sock.display(), root.display());
    let mut last_hourly: i64 = 0;
    let mut next = std::time::Instant::now() + std::time::Duration::from_secs(tick);
    // Running request processes, by caller: limits keep one user from exhausting the service.
    let mut children: HashMap<i32, u32> = HashMap::new();
    // One periodic pass at a time: a pass longer than the tick must not start another.
    let mut periodic_child: Option<i32> = None;
    let (max_total, max_per_user) = (limit("LAYR_MAX_REQUESTS", 256), limit("LAYR_MAX_REQUESTS_PER_USER", 32));
    loop {
        // Reap finished children.
        while let Ok(Some((pid, _))) = rustix::process::waitpid(None, rustix::process::WaitOptions::NOHANG) {
            let pid = pid.as_raw_nonzero().get();
            children.remove(&pid);
            if periodic_child == Some(pid) {
                periodic_child = None;
            }
        }
        let wait = next.saturating_duration_since(std::time::Instant::now()).min(std::time::Duration::from_secs(1));
        let timeout = rustix::event::Timespec { tv_sec: wait.as_secs() as _, tv_nsec: wait.subsec_nanos() as _ };
        let mut pfd = vec![rustix::event::PollFd::new(&listener, rustix::event::PollFlags::IN)];
        for (_, l, _) in &registry_listeners {
            pfd.push(rustix::event::PollFd::new(l, rustix::event::PollFlags::IN));
        }
        let ready = rustix::event::poll(&mut pfd, Some(&timeout)).unwrap_or(0);
        if ready > 0 && pfd[0].revents().contains(rustix::event::PollFlags::IN) {
            if let Ok((conn, _)) = listener.accept() {
                let uid = peer_uid(&conn).map(|x| x.0).unwrap_or(u32::MAX);
                let mine = children.values().filter(|u| **u == uid).count();
                if children.len() >= max_total || (uid != 0 && mine >= max_per_user) {
                    let _ = (&conn).write_all(b"B");
                    drop(conn);
                    continue;
                }
                match crate::sys::fork() {
                    Ok(crate::sys::Fork::Child) => {
                        drop(listener);
                        let code = handle(conn, &root, &registry_uids);
                        std::process::exit(if code >= 0 { 0 } else { 1 });
                    }
                    Err(e) => eprintln!("layr service: fork failed: {e}"),
                    Ok(crate::sys::Fork::Parent(pid)) => {
                        children.insert(pid, uid);
                        drop(conn);
                    }
                }
            }
        }
        for (i, (name, registry, user)) in registry_listeners.iter().enumerate() {
            if !pfd[i + 1].revents().contains(rustix::event::PollFlags::IN) {
                continue;
            }
            if let Ok((conn, _)) = registry.accept() {
                if children.len() >= max_total {
                    drop(conn);
                    continue;
                }
                match crate::sys::fork() {
                    Ok(crate::sys::Fork::Child) => {
                        drop(listener);
                        if let Err(e) = crate::oci::registry::exec_http(conn, &sock, name, *user) {
                            eprintln!("layr registry: {e:#}");
                        }
                        std::process::exit(1);
                    }
                    Ok(crate::sys::Fork::Parent(pid)) => {
                        children.insert(pid, u32::MAX);
                    }
                    Err(e) => eprintln!("layr registry: fork failed: {e}"),
                }
            }
        }
        if std::time::Instant::now() >= next {
            next = std::time::Instant::now() + std::time::Duration::from_secs(tick);
            if periodic_child.is_some() {
                continue;
            }
            // Periodic work in a child, so the service stays responsive and single threaded.
            let now = crate::ids::now_ms();
            let hourly = now - last_hourly >= 3_600_000;
            match crate::sys::fork() {
                Ok(crate::sys::Fork::Child) => {
                    drop(listener);
                    periodic(&root, hourly);
                    std::process::exit(0);
                }
                Ok(crate::sys::Fork::Parent(pid)) => {
                    periodic_child = Some(pid);
                    if hourly {
                        last_hourly = now;
                    }
                }
                Err(e) => eprintln!("layr service: fork failed: {e}"),
            }
        }
    }
}

#[cfg(target_os = "macos")]
fn peer_uid(sock: &UnixStream) -> Result<(u32, u32, i32)> {
    let (uid, gid) = crate::sys::peer_credentials(sock)?;
    Ok((uid, gid, 0))
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    #[test]
    fn socket_identity_comes_from_the_kernel() {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        let (uid, gid, _) = super::peer_uid(&a).unwrap();
        assert_eq!(uid, rustix::process::geteuid().as_raw());
        assert_eq!(gid, rustix::process::getegid().as_raw());
    }
}

#[cfg(test)]
mod connect_tests {
    use super::connect_error;
    use std::path::Path;

    fn msg(errno: rustix::io::Errno) -> String {
        connect_error(Path::new("/run/layr/layr.sock"), &std::io::Error::from_raw_os_error(errno.raw_os_error())).to_string()
    }

    #[test]
    fn a_refused_connect_names_its_cause() {
        // EPERM: a filter of the client refused connect(); the socket itself is open.
        assert!(msg(rustix::io::Errno::PERM).contains("sandbox or seccomp filter"));
        // EACCES: file modes or a security module.
        assert!(msg(rustix::io::Errno::ACCESS).contains("mode 0666"));
        // No socket or no listener: the service is not running here.
        assert!(msg(rustix::io::Errno::NOENT).contains("Start it as root"));
        assert!(msg(rustix::io::Errno::CONNREFUSED).contains("Start it as root"));
    }
}
