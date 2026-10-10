//! The root service (`layr daemon`) and the client side of `layr` for other users.
//!
//! A client sends its arguments, working folder and environment over a Unix socket, with its
//! standard input, output and error and an open handle of its working folder as `SCM_RIGHTS`.
//! The service forks one process per request (the parent stays single threaded), takes the
//! caller's uid from `SO_PEERCRED`, runs the command with the caller's streams and returns the
//! exit code. Commands check the caller against line owners and the project lead.

use crate::ctx::{Caller, Ctx};
use crate::store::Store;
use anyhow::{anyhow, bail, Context, Result};
use std::collections::HashMap;
use std::io::{Read, Write};
use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

pub fn socket_path() -> PathBuf {
    std::env::var_os("LAYR_SOCKET").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/run/layr/layr.sock"))
}

const MAX_REQ: usize = 4 << 20;

fn send_with_fds(sock: &UnixStream, data: &[u8], fds: &[BorrowedFd]) -> Result<()> {
    use rustix::net::{SendAncillaryBuffer, SendAncillaryMessage, SendFlags};
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(4))];
    let mut control = SendAncillaryBuffer::new(&mut space);
    if !control.push(SendAncillaryMessage::ScmRights(fds)) {
        bail!("too many descriptors");
    }
    let n = rustix::net::sendmsg(sock, &[std::io::IoSlice::new(data)], &mut control, SendFlags::empty())?;
    if n < data.len() {
        (&*sock).write_all(&data[n..])?;
    }
    Ok(())
}

fn recv_with_fds(sock: &UnixStream, buf: &mut [u8]) -> Result<(usize, Vec<OwnedFd>)> {
    use rustix::net::{RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags};
    let mut space = [std::mem::MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(8))];
    let mut control = RecvAncillaryBuffer::new(&mut space);
    let r = rustix::net::recvmsg(sock, &mut [std::io::IoSliceMut::new(buf)], &mut control, RecvFlags::CMSG_CLOEXEC)?;
    let mut fds = Vec::new();
    for m in control.drain() {
        if let RecvAncillaryMessage::ScmRights(it) = m {
            fds.extend(it);
        }
    }
    Ok((r.bytes, fds))
}

/// Client: run a command through the service. Returns the exit code.
pub fn forward(argv: &[String]) -> Result<i32> {
    let path = socket_path();
    let sock = UnixStream::connect(&path).map_err(|e| {
        anyhow!(
            "cannot reach the layr service at {} ({e}). Start it as root with 'layr daemon', or run layr as root.",
            path.display()
        )
    })?;
    let cwd = std::env::current_dir()?;
    let env: HashMap<String, String> = std::env::vars().collect();
    let req = serde_json::to_vec(&serde_json::json!({"argv": argv, "cwd": cwd, "env": env}))?;
    let dir = std::fs::File::open(&cwd)?;
    let mut data = (req.len() as u32).to_be_bytes().to_vec();
    data.extend_from_slice(&req);
    // The service answers "B" at once when it is busy, or "R" and the exit code at the end.
    let _ = send_with_fds(&sock, &data, &[std::io::stdin().as_fd(), std::io::stdout().as_fd(), std::io::stderr().as_fd(), dir.as_fd()]);
    let mut kind = [0u8; 1];
    (&sock).read_exact(&mut kind).context("the layr service closed the connection")?;
    if kind[0] == b'B' {
        bail!("the layr service is busy (too many running requests); try again");
    }
    let mut code = [0u8; 4];
    (&sock).read_exact(&mut code).context("the layr service closed the connection")?;
    Ok(i32::from_be_bytes(code))
}

fn peer_uid(sock: &UnixStream) -> Result<(u32, u32, i32)> {
    let c = rustix::net::sockopt::socket_peercred(sock)?;
    Ok((c.uid.as_raw(), c.gid.as_raw(), c.pid.as_raw_nonzero().get()))
}

fn limit(var: &str, default: usize) -> usize {
    std::env::var(var).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
}

/// Child process: serve one request, then exit.
fn handle(sock: UnixStream, root: &Path) -> i32 {
    let res = (|| -> Result<i32> {
        let (uid, _gid, _pid) = peer_uid(&sock)?;
        // A client that does not send its request in time does not hold a process.
        sock.set_read_timeout(Some(std::time::Duration::from_secs(10)))?;
        let mut first = vec![0u8; 65536];
        let (n, fds) = recv_with_fds(&sock, &mut first)?;
        if fds.len() != 4 {
            bail!("bad request: expected 4 descriptors");
        }
        if n < 4 {
            bail!("bad request");
        }
        let len = u32::from_be_bytes([first[0], first[1], first[2], first[3]]) as usize;
        if len > MAX_REQ {
            bail!("request too large");
        }
        let mut req = first[4..n].to_vec();
        while req.len() < len {
            let mut more = vec![0u8; len - req.len()];
            let k = (&sock).read(&mut more)?;
            if k == 0 {
                bail!("short request");
            }
            req.extend_from_slice(&more[..k]);
        }
        let v: serde_json::Value = serde_json::from_slice(&req)?;
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

/// Periodic work: automatic states of busy lines, retention, backups.
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
        if let Ok(lines) = repo.lines() {
            for l in lines {
                if l.machine != repo.p.store.machine.id || !repo.working(&l).exists() {
                    continue;
                }
                let last = repo.last_auto(&l).ok().flatten().map(|s| s.time).unwrap_or(0);
                if now - last >= interval {
                    if let Ok(_lock) = repo.p.lock() {
                        let _ = repo.save(&ctx, &l, "periodic", true, false);
                    }
                }
            }
        }
        if hourly {
            let _ = crate::cmd::admin::run_gc(&ctx, &repo, false);
            if let Ok(Some(dir)) = repo.p.config_value("backup.dir") {
                if let Some(d) = dir.as_str() {
                    // Written as the lead of this machine, who configured the folder.
                    let _ = crate::cmd::transfer::run_backup(&ctx, &repo, Path::new(d), false, repo.lead_ids());
                }
            }
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
    let (max_total, max_per_user) = (limit("LAYR_MAX_REQUESTS", 256), limit("LAYR_MAX_REQUESTS_PER_USER", 32));
    loop {
        // Reap finished children.
        while let Ok(Some((pid, _))) = rustix::process::waitpid(None, rustix::process::WaitOptions::NOHANG) {
            children.remove(&pid.as_raw_nonzero().get());
        }
        let wait = next.saturating_duration_since(std::time::Instant::now()).min(std::time::Duration::from_secs(1));
        let timeout = rustix::event::Timespec { tv_sec: wait.as_secs() as _, tv_nsec: wait.subsec_nanos() as _ };
        let mut pfd = [rustix::event::PollFd::new(&listener, rustix::event::PollFlags::IN)];
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
                        let code = handle(conn, &root);
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
        if std::time::Instant::now() >= next {
            next = std::time::Instant::now() + std::time::Duration::from_secs(tick);
            // Periodic work in a child, so the service stays responsive and single threaded.
            let now = crate::ids::now_ms();
            let hourly = now - last_hourly >= 3_600_000;
            if hourly {
                last_hourly = now;
            }
            if let Ok(crate::sys::Fork::Child) = crate::sys::fork() {
                drop(listener);
                periodic(&root, hourly);
                std::process::exit(0);
            }
        }
    }
}
