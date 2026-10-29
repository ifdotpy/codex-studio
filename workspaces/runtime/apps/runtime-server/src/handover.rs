//! Live handover: a new server takes the listening socket from the running one, so the
//! port never refuses a connection while the build changes (docs/live-handover.md).
//!
//! 1. The running server offers a Unix socket `handover.sock` in its state directory.
//! 2. The new server (`--takeover`) connects, sends `GO` and receives the listening TCP
//!    socket (SCM_RIGHTS). It keeps the connection and waits for its end. The old server
//!    never closes its end before the process exits (also not when its drain fails), so the
//!    end means that the old process is gone.
//! 3. The old server drains like a SIGTERM stop, then starts a standby: its own executable
//!    file with `--handover-standby`. It gives the standby the listening socket. The standby
//!    listens on `handover-standby.sock` and tells the old server (one byte) that it is bound.
//!    Only then does the old server exit.
//! 4. The end of the first connection tells the new server that the old process ended, so
//!    its database lock is free. The new server connects to the standby and reads
//!    `DRAINED`. It sends `OPEN` just before it opens the state (after that, a failure goes
//!    forward, never back), and `READY` when it serves. The standby exits.
//! 5. If the new server never connects, or the line ends before `OPEN` or fails, the
//!    standby starts the old executable again (`--listener-from-stdin`: the socket is its
//!    standard input) and the old build serves again. EOF after `OPEN` means the state may
//!    have migrated: the standby exits and the recovery loop starts a backend.
//! 6. If the old server exits without a standby (drain past its grace, error, kill), the
//!    new server finds no standby socket. It holds the socket, waits for the runtime lock
//!    and serves itself.
//!
//! The state is open in one process at a time: the old server closes it before the standby
//! says `DRAINED`, and the standby never opens it.
use std::net::TcpListener;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::oneshot;

/// Start mode: take the listening socket from the running server of this state.
pub const TAKEOVER_ARG: &str = "--takeover";
/// Start mode: the listening socket is standard input.
pub const RESUME_ARG: &str = "--listener-from-stdin";
/// Start mode of the standby. Internal.
pub const STANDBY_ARG: &str = "--handover-standby";
/// Offer socket in the state directory.
const SOCKET_FILE: &str = "handover.sock";
/// Socket of the standby in the state directory.
const STANDBY_SOCKET: &str = "handover-standby.sock";
/// How long the old server waits for the standby to bind its socket. Unit: seconds.
const BIND_WAIT: Duration = Duration::from_secs(10);
/// Margin that `take` adds to the shutdown grace of the old server. Unit: seconds.
const LOCK_MARGIN: Duration = Duration::from_secs(10);
/// How long the new server looks for the standby after the old process ended, and how long
/// the standby waits for the new server. Unit: seconds.
const STANDBY_WAIT: Duration = Duration::from_secs(5);
/// How long the old server waits for `GO` from a connected peer. Unit: seconds. A peer that
/// the host starves for longer than this would be skipped and its request would hang.
const REQUEST_WAIT: Duration = Duration::from_secs(10);

/// The listening socket, taken or bound anew, and a duplicate for the offer.
pub fn listener(
    port: u16,
    taken: Option<TcpListener>,
) -> std::io::Result<(tokio::net::TcpListener, TcpListener)> {
    let socket = match taken {
        Some(socket) => socket,
        None => TcpListener::bind(std::net::SocketAddr::from(([127, 0, 0, 1], port)))?,
    };
    socket.set_nonblocking(true)?;
    let kept = socket.try_clone()?;
    Ok((tokio::net::TcpListener::from_std(socket)?, kept))
}

/// The duplicate that the offer keeps. A stop without a handover drops it, so the port
/// closes with the server.
pub type Kept = Arc<Mutex<Option<TcpListener>>>;

/// Resolves when a new server has taken the socket; never when none does.
pub async fn requested(rx: oneshot::Receiver<Handed>) -> Handed {
    match rx.await {
        Ok(handed) => handed,
        Err(_) => std::future::pending().await,
    }
}

#[cfg(not(unix))]
pub use other::{Handed, Line, Taken, from_parent, offer, request, standby_main, take};
#[cfg(unix)]
pub use unix::{Handed, Line, Taken, from_parent, offer, request, standby_main, take};

#[cfg(not(unix))]
mod other {
    use super::*;
    pub struct Line;
    impl Line {
        pub fn open(&mut self) -> std::io::Result<()> {
            Ok(())
        }
        pub fn ready(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    pub struct Taken {
        pub listener: TcpListener,
        pub line: Option<Line>,
    }
    pub struct Handed;
    impl Handed {
        pub fn hand_off(self) -> std::io::Result<()> {
            Ok(())
        }
    }
    pub fn offer(
        _: &Path,
        listener: TcpListener,
    ) -> std::io::Result<(oneshot::Receiver<Handed>, Kept)> {
        Ok((oneshot::channel().1, Arc::new(Mutex::new(Some(listener)))))
    }
    pub fn request(_: &Path) -> std::io::Result<(TcpListener, ())> {
        Err(std::io::Error::other("live handover needs a Unix host"))
    }
    pub fn take(_: &Path, _: Duration) -> std::io::Result<Taken> {
        Err(std::io::Error::other("live handover needs a Unix host"))
    }
    pub fn from_parent() -> std::io::Result<Taken> {
        Err(std::io::Error::other("live handover needs a Unix host"))
    }
    pub fn standby_main() -> std::process::ExitCode {
        std::process::ExitCode::FAILURE
    }
}

#[cfg(unix)]
mod unix {
    use super::*;
    use rustix::net::{
        RecvAncillaryBuffer, RecvAncillaryMessage, RecvFlags, SendAncillaryBuffer,
        SendAncillaryMessage, SendFlags, recvmsg, sendmsg,
    };
    use std::fs::File;
    use std::io::{BufRead, BufReader, IoSlice, IoSliceMut, Read, Write};
    use std::mem::{ManuallyDrop, MaybeUninit};
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, ExitCode, Stdio};

    #[cfg(target_os = "linux")]
    const RECV_FLAGS: RecvFlags = RecvFlags::CMSG_CLOEXEC;
    #[cfg(not(target_os = "linux"))]
    const RECV_FLAGS: RecvFlags = RecvFlags::empty();

    fn send_fds(socket: &UnixStream, fds: &[BorrowedFd<'_>]) -> std::io::Result<()> {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut control = SendAncillaryBuffer::new(&mut space);
        control.push(SendAncillaryMessage::ScmRights(fds));
        sendmsg(
            socket,
            &[IoSlice::new(b"F")],
            &mut control,
            SendFlags::empty(),
        )?;
        Ok(())
    }

    fn recv_fds(socket: &UnixStream) -> std::io::Result<Vec<OwnedFd>> {
        let mut space = [MaybeUninit::uninit(); rustix::cmsg_space!(ScmRights(2))];
        let mut control = RecvAncillaryBuffer::new(&mut space);
        let mut byte = [0_u8; 1];
        recvmsg(
            socket,
            &mut [IoSliceMut::new(&mut byte)],
            &mut control,
            RECV_FLAGS,
        )?;
        let mut fds = Vec::new();
        for message in control.drain() {
            if let RecvAncillaryMessage::ScmRights(received) = message {
                for fd in received {
                    // A provider or command that this server starts must not inherit them.
                    rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC)?;
                    fds.push(fd);
                }
            }
        }
        Ok(fds)
    }

    fn missing(what: &str) -> std::io::Error {
        std::io::Error::other(format!("the handover carries no {what}"))
    }

    /// The line from the new server to the standby. A server never closes it before it
    /// exits, so EOF on the other side means that this process is gone.
    pub struct Line(ManuallyDrop<UnixStream>);

    impl Line {
        /// The state opens next: a failure from here on goes forward, not back.
        pub fn open(&mut self) -> std::io::Result<()> {
            self.0.write_all(b"OPEN\n")
        }
        /// This server serves.
        pub fn ready(&mut self) -> std::io::Result<()> {
            self.0.write_all(b"READY\n")
        }
        /// End the line while this process lives. A server never does: tests do, to stand
        /// for a new server that has exited.
        pub fn close(self) {
            drop(ManuallyDrop::into_inner(self.0));
        }
    }

    /// What the new server holds: the listening socket and, with a standby, the line.
    pub struct Taken {
        pub listener: TcpListener,
        pub line: Option<Line>,
    }

    /// A new server has the listening socket. The old server hands the state off.
    pub struct Handed {
        /// Never closed before the process exits: the new server reads its end as "the old
        /// process ended", also when the drain fails and `run` returns an error.
        pub(super) _stream: ManuallyDrop<UnixStream>,
        listener: TcpListener,
        exe: File,
        state_dir: PathBuf,
    }

    /// Offer the listening socket on `handover.sock`. The caller holds the runtime lock, so
    /// a socket file already there is stale.
    pub fn offer(
        state_dir: &Path,
        listener: TcpListener,
    ) -> std::io::Result<(oneshot::Receiver<Handed>, Kept)> {
        // The open file keeps this build's executable, whatever replaces it on disk.
        let exe = File::open(std::env::current_exe()?)?;
        let path = state_dir.join(SOCKET_FILE);
        match std::fs::remove_file(&path) {
            Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
            _ => {}
        }
        let previous = rustix::process::umask(rustix::fs::Mode::from_raw_mode(0o177));
        let bound = UnixListener::bind(&path);
        rustix::process::umask(previous);
        let socket = bound?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let (sender, receiver) = oneshot::channel();
        let kept: Kept = Arc::new(Mutex::new(Some(listener)));
        let offered = Arc::clone(&kept);
        let state_dir = state_dir.to_path_buf();
        std::thread::Builder::new()
            .name("handover-offer".into())
            .spawn(move || {
                // A peer that connects and dies before `GO` is skipped: nothing was given.
                for stream in socket.incoming().flatten() {
                    if let Some(listener) = give(&stream, &offered) {
                        sender
                            .send(Handed {
                                _stream: ManuallyDrop::new(stream),
                                listener,
                                exe,
                                state_dir,
                            })
                            .ok();
                        return;
                    }
                }
            })?;
        Ok((receiver, kept))
    }

    fn give(stream: &UnixStream, kept: &Kept) -> Option<TcpListener> {
        stream.set_read_timeout(Some(REQUEST_WAIT)).ok()?;
        let mut line = String::new();
        BufReader::new(stream).take(16).read_line(&mut line).ok()?;
        if line != "GO\n" {
            return None;
        }
        stream.set_read_timeout(None).ok()?;
        // A stop without a handover took the listener already.
        let listener = kept.lock().ok()?.take()?;
        send_fds(stream, &[listener.as_fd()]).ok()?;
        Some(listener)
    }

    /// The path of the executable file that this process runs, even after a swap replaced
    /// the path it started from.
    fn executable_path(exe: &File) -> std::io::Result<PathBuf> {
        #[cfg(target_vendor = "apple")]
        {
            use std::os::unix::ffi::OsStrExt;
            let path = rustix::fs::getpath(exe)?;
            Ok(PathBuf::from(std::ffi::OsStr::from_bytes(path.as_bytes())))
        }
        #[cfg(not(target_vendor = "apple"))]
        {
            std::fs::read_link(format!(
                "/proc/self/fd/{}",
                std::os::fd::AsRawFd::as_raw_fd(exe)
            ))
        }
    }

    impl Handed {
        /// After the drain: start the standby from this server's executable file and give
        /// it the socket. The caller exits right after.
        pub fn hand_off(self) -> std::io::Result<()> {
            let (ours, theirs) = UnixStream::pair()?;
            Command::new(executable_path(&self.exe)?)
                .arg(STANDBY_ARG)
                .arg(&self.state_dir)
                .stdin(Stdio::from(OwnedFd::from(theirs)))
                .process_group(0)
                .spawn()?;
            send_fds(&ours, &[self.listener.as_fd()])?;
            // Wait for the standby to bind its socket, so that the new server finds it.
            ours.set_read_timeout(Some(BIND_WAIT))?;
            (&ours).read_exact(&mut [0_u8; 1])?;
            // The standby waits for the end of this socket: this process ends it by exiting,
            // so it must stay open until then.
            std::mem::forget(ours);
            Ok(())
        }
    }

    /// Ask the running server of `state_dir` for its listening socket. Returns at once with
    /// the socket and the connection that ends when the old process does.
    pub fn request(state_dir: &Path) -> std::io::Result<(TcpListener, UnixStream)> {
        let mut stream = UnixStream::connect(state_dir.join(SOCKET_FILE)).map_err(|error| {
            std::io::Error::other(format!("no running server offers a handover: {error}"))
        })?;
        stream.write_all(b"GO\n")?;
        let listener = recv_fds(&stream)?.pop().ok_or_else(|| missing("socket"))?;
        Ok((TcpListener::from(listener), stream))
    }

    /// Take the listening socket from the running server of `state_dir` and wait until the
    /// state is free. Blocking.
    /// `grace` is the shutdown grace of the old server (its owner is `main.rs`): that server
    /// drains for up to `grace` and then exits by force.
    pub fn take(state_dir: &Path, grace: Duration) -> std::io::Result<Taken> {
        let (listener, mut old) = request(state_dir)?;
        // The old server ends this connection when it exits.
        old.set_read_timeout(Some(grace * 2))?;
        std::io::copy(&mut old, &mut std::io::sink())?;
        let deadline = std::time::Instant::now() + STANDBY_WAIT;
        let standby = loop {
            match UnixStream::connect(state_dir.join(STANDBY_SOCKET)) {
                Ok(stream) => break Some(stream),
                Err(_) if std::time::Instant::now() >= deadline => break None,
                // `take` runs on a dedicated blocking thread (spawn_blocking in main).
                #[allow(clippy::disallowed_methods)]
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        if let Some(stream) = standby {
            let mut answer = String::new();
            BufReader::new(&stream).read_line(&mut answer)?;
            if answer == "DRAINED\n" {
                return Ok(Taken {
                    listener,
                    line: Some(Line(ManuallyDrop::new(stream))),
                });
            }
        }
        // The old server ended without a usable standby: this server holds the socket and
        // waits for the state lock.
        eprintln!("live handover: no standby; serving from the old server's socket");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(state_dir.join("runtime.lock"))?;
        let deadline = std::time::Instant::now() + grace + LOCK_MARGIN;
        while lock.try_lock().is_err() {
            if std::time::Instant::now() >= deadline {
                return Err(std::io::Error::other(
                    "the old server still holds the state",
                ));
            }
            #[allow(clippy::disallowed_methods)]
            std::thread::sleep(Duration::from_millis(50));
        }
        lock.unlock()?;
        Ok(Taken {
            listener,
            line: None,
        })
    }

    /// The socket that the standby left on standard input after a failed handover.
    pub fn from_parent() -> std::io::Result<Taken> {
        let listener = std::io::stdin().as_fd().try_clone_to_owned()?;
        // Standard input must not keep the port open for a command that this server starts.
        rustix::stdio::dup2_stdin(File::open("/dev/null")?)?;
        Ok(Taken {
            listener: TcpListener::from(listener),
            line: None,
        })
    }

    /// The standby process. It holds no database.
    pub fn standby_main() -> ExitCode {
        match standby() {
            Ok(()) => ExitCode::SUCCESS,
            Err(error) => {
                eprintln!("live handover standby failed: {error}");
                ExitCode::FAILURE
            }
        }
    }

    fn standby() -> std::io::Result<()> {
        let me = std::env::current_exe()?;
        let state_dir = std::env::args_os()
            .nth(2)
            .map(PathBuf::from)
            .ok_or_else(|| missing("state directory"))?;
        let socket_path = state_dir.join(STANDBY_SOCKET);
        std::fs::remove_file(&socket_path).ok();
        let offer = UnixListener::bind(&socket_path)?;
        offer.set_nonblocking(true)?;
        let parent = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
        let listener = recv_fds(&parent)?.pop().ok_or_else(|| missing("socket"))?;
        // Bound and ready: the old server may exit now.
        (&parent).write_all(b"B")?;
        // The old process ended, so it no longer holds the database.
        std::io::copy(&mut &parent, &mut std::io::sink())?;
        // The new server connects at once. If it never does, it is gone.
        let deadline = std::time::Instant::now() + STANDBY_WAIT;
        let mut opened = false;
        let mut served = false;
        while std::time::Instant::now() < deadline {
            match offer.accept() {
                Ok((mut line, _)) => {
                    line.set_nonblocking(false)?;
                    // Every outcome other than `READY` is a failed new server.
                    if line.write_all(b"DRAINED\n").is_ok() {
                        let mut lines = BufReader::new(&line);
                        loop {
                            let mut answer = String::new();
                            match lines.read_line(&mut answer) {
                                Ok(count) if count > 0 && answer == "OPEN\n" => opened = true,
                                Ok(count) if count > 0 && answer == "READY\n" => {
                                    served = true;
                                    break;
                                }
                                _ => break,
                            }
                        }
                    }
                    break;
                }
                #[allow(clippy::disallowed_methods)]
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        }
        std::fs::remove_file(&socket_path).ok();
        if served {
            return Ok(());
        }
        if opened {
            // The new server may have migrated the state: do not start the old build on it.
            eprintln!(
                "live handover: the new server failed after it opened the state; the recovery loop restarts a backend"
            );
            return Ok(());
        }
        eprintln!("live handover failed: the old build resumes");
        Err(Command::new(me)
            .arg(RESUME_ARG)
            .stdin(Stdio::from(listener))
            .exec())
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    #[test]
    fn socket_pass_gives_the_same_port_and_the_line_carries_open_and_ready() {
        let dir = tempfile::tempdir().expect("temporary state");
        let kept = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = kept.local_addr().expect("address").port();
        let (offered, _kept) = offer(dir.path(), kept).expect("offer");
        let state = dir.path().to_path_buf();
        let new_server = std::thread::spawn(move || take(&state, Duration::from_secs(20)));
        let handed = offered.blocking_recv().expect("handover request");
        // The standby listens before the old server ends its connection.
        let standby = std::os::unix::net::UnixListener::bind(dir.path().join(STANDBY_SOCKET))
            .expect("standby socket");
        // The old process exits: its end of the connection closes.
        drop(std::mem::ManuallyDrop::into_inner(handed._stream));
        let (mut line_out, _) = standby.accept().expect("new server connects");
        line_out.write_all(b"DRAINED\n").expect("drained");
        let mut taken = new_server.join().expect("join").expect("take");
        assert_eq!(taken.listener.local_addr().expect("address").port(), port);
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        taken.listener.set_nonblocking(false).expect("blocking");
        let (mut accepted, _) = taken.listener.accept().expect("accept");
        client.write_all(b"x").expect("write");
        let mut byte = [0_u8; 1];
        accepted.read_exact(&mut byte).expect("read");
        assert_eq!(&byte, b"x");
        let line = taken.line.as_mut().expect("line");
        line.open().expect("open");
        line.ready().expect("ready");
        let mut lines = BufReader::new(&line_out);
        for expected in ["OPEN\n", "READY\n"] {
            let mut got = String::new();
            lines.read_line(&mut got).expect("read line");
            assert_eq!(got, expected);
        }
    }

    #[test]
    fn a_stop_without_a_handover_closes_the_offered_port() {
        let dir = tempfile::tempdir().expect("temporary state");
        let kept = TcpListener::bind("127.0.0.1:0").expect("listener");
        let (_offered, release) = offer(dir.path(), kept).expect("offer");
        release.lock().expect("lock").take();
        // A later request gets nothing.
        assert!(request(dir.path()).is_err());
    }

    #[test]
    fn take_without_an_offer_fails_and_changes_nothing() {
        let dir = tempfile::tempdir().expect("temporary state");
        assert!(take(dir.path(), Duration::from_secs(20)).is_err());
    }
}
