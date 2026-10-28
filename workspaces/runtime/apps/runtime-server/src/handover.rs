//! Live handover: a new server takes the listening socket from the running one, so the
//! port never refuses a connection while the build changes (docs/live-handover.md).
//!
//! 1. The running server offers a Unix socket `handover.sock` in its state directory.
//! 2. The new server (`--takeover`) connects, sends `GO` and receives the listening TCP
//!    socket (SCM_RIGHTS). The old server drains like a SIGTERM stop, then starts a standby
//!    copy of its own binary, passes it the TCP socket, and exits.
//! 3. The standby listens on `handover-standby.sock`. The new server connects to it. When
//!    the old process has ended, so its database lock is free, the standby sends `DRAINED`.
//!    The new server opens the state, serves, and sends `READY`. The standby exits.
//! 4. If that connection ends without `READY`, the standby starts the old binary again
//!    (`--listener-from-stdin`: the socket is its standard input): the old build serves
//!    again. Passing the socket on once more with SCM_RIGHTS broke accept events on macOS.
//!
//! The state is open in one process at a time: the old server closes it before the standby
//! starts, and the standby never opens it.
use std::net::TcpListener;
use std::path::Path;
use std::time::Duration;
use tokio::sync::oneshot;

/// Start mode: take the listening socket from the running server of this state.
pub const TAKEOVER_ARG: &str = "--takeover";
/// Start mode: the listening socket is standard input.
pub const RESUME_ARG: &str = "--listener-from-stdin";
/// Start mode of the standby copy. Internal.
pub const STANDBY_ARG: &str = "--handover-standby";
/// Offer socket in the state directory.
const SOCKET_FILE: &str = "handover.sock";
/// Socket of the standby, where the new server waits for `DRAINED`.
const STANDBY_SOCKET: &str = "handover-standby.sock";
/// The standby copy of the old binary, in the state directory.
const STANDBY_FILE: &str = "handover-standby";
/// How long the new server waits for the old one to drain. Unit: seconds. It exceeds the
/// 20 s shutdown grace of the old server, which exits by force after that.
const DRAIN_WAIT: Duration = Duration::from_secs(40);

/// The listening socket, taken or bound anew, and a duplicate that keeps the port open
/// while this server drains.
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

/// Resolves when a new server has taken the socket; never when none does.
pub async fn requested(rx: oneshot::Receiver<Handed>) -> Handed {
    match rx.await {
        Ok(handed) => handed,
        Err(_) => std::future::pending().await,
    }
}

#[cfg(not(unix))]
pub use other::{Handed, Taken, from_parent, offer, standby_main, take};
#[cfg(unix)]
pub use unix::{Handed, Taken, from_parent, offer, standby_main, take};

#[cfg(not(unix))]
mod other {
    use super::*;
    pub struct Taken {
        pub listener: TcpListener,
    }
    impl Taken {
        pub fn ready(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    pub struct Handed;
    impl Handed {
        pub fn hand_off(self) -> std::io::Result<()> {
            Ok(())
        }
    }
    pub fn offer(_: &Path, _: TcpListener) -> std::io::Result<oneshot::Receiver<Handed>> {
        Ok(oneshot::channel().1)
    }
    pub fn take(_: &Path) -> std::io::Result<Taken> {
        Err(std::io::Error::other("live handover needs a Unix host"))
    }
    pub fn from_parent() -> std::io::Result<Taken> {
        take(Path::new(""))
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
    use std::mem::MaybeUninit;
    use std::os::fd::{AsFd, BorrowedFd, OwnedFd};
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    use std::os::unix::net::{UnixListener, UnixStream};
    use std::os::unix::process::CommandExt;
    use std::path::PathBuf;
    use std::process::{Command, ExitCode, Stdio};
    use std::time::Instant;

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
            RecvFlags::empty(),
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

    /// What the new server holds: the listening socket and the line to the standby.
    pub struct Taken {
        pub listener: TcpListener,
        stream: Option<UnixStream>,
    }

    impl Taken {
        /// Tell the standby that this server serves. Call it when the server is ready.
        pub fn ready(&mut self) -> std::io::Result<()> {
            match self.stream.take() {
                Some(mut stream) => stream.write_all(b"READY\n"),
                None => Ok(()),
            }
        }
    }

    /// A new server has the listening socket. The old server hands the state off.
    pub struct Handed {
        listener: TcpListener,
        exe: File,
        state_dir: PathBuf,
    }

    /// Offer the listening socket on `handover.sock`. The caller holds the runtime lock, so
    /// a socket file already there is stale.
    pub fn offer(
        state_dir: &Path,
        listener: TcpListener,
    ) -> std::io::Result<oneshot::Receiver<Handed>> {
        // The open file keeps this build's binary, whatever replaces it on disk.
        let exe = File::open(std::env::current_exe()?)?;
        let path = state_dir.join(SOCKET_FILE);
        for stale in [&path, &state_dir.join(STANDBY_SOCKET)] {
            match std::fs::remove_file(stale) {
                Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
                _ => {}
            }
        }
        let socket = UnixListener::bind(&path)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        let (sender, receiver) = oneshot::channel();
        let state_dir = state_dir.to_path_buf();
        std::thread::Builder::new()
            .name("handover-offer".into())
            .spawn(move || {
                // A peer that connects and dies before `GO` is skipped: nothing was given.
                for stream in socket.incoming().flatten() {
                    if give(&stream, &listener).is_ok() {
                        drop(stream);
                        let _ = sender.send(Handed {
                            listener,
                            exe,
                            state_dir,
                        });
                        return;
                    }
                }
            })?;
        Ok(receiver)
    }

    fn give(stream: &UnixStream, listener: &TcpListener) -> std::io::Result<()> {
        stream.set_read_timeout(Some(Duration::from_secs(10)))?;
        let mut line = String::new();
        BufReader::new(stream).take(16).read_line(&mut line)?;
        if line != "GO\n" {
            return Err(std::io::Error::other("no handover request"));
        }
        send_fds(stream, &[listener.as_fd()])?;
        stream.set_read_timeout(None)
    }

    impl Handed {
        /// After the drain: start the standby copy of this binary with the socket and the
        /// line, so it outlives this process. The caller exits right after.
        pub fn hand_off(self) -> std::io::Result<()> {
            let copy = self.state_dir.join(STANDBY_FILE);
            let mut exe = self.exe;
            // A resumed server runs from the previous copy: unlink it, do not overwrite it.
            let _ = std::fs::remove_file(&copy);
            let mut target = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o700)
                .open(&copy)?;
            std::io::copy(&mut exe, &mut target)?;
            drop(target);
            let (ours, theirs) = UnixStream::pair()?;
            Command::new(&copy)
                .arg(STANDBY_ARG)
                .stdin(Stdio::from(OwnedFd::from(theirs)))
                .process_group(0)
                .spawn()?;
            send_fds(&ours, &[self.listener.as_fd()])?;
            // The standby waits for the end of this socket: this process ends it by exiting.
            std::mem::forget(ours);
            Ok(())
        }
    }

    /// Take the listening socket from the running server of `state_dir`. Returns after that
    /// server has drained and ended. Blocking.
    pub fn take(state_dir: &Path) -> std::io::Result<Taken> {
        let mut stream = UnixStream::connect(state_dir.join(SOCKET_FILE)).map_err(|error| {
            std::io::Error::other(format!("no running server offers a handover: {error}"))
        })?;
        stream.write_all(b"GO\n")?;
        let listener = recv_fds(&stream)?.pop().ok_or_else(|| missing("socket"))?;
        drop(stream);
        // The standby binds its socket at its start: wait for it.
        let deadline = Instant::now() + DRAIN_WAIT;
        let stream = loop {
            match UnixStream::connect(state_dir.join(STANDBY_SOCKET)) {
                Ok(stream) => break stream,
                Err(error) if Instant::now() >= deadline => return Err(error),
                Err(_) => std::thread::sleep(Duration::from_millis(10)),
            }
        };
        stream.set_read_timeout(Some(DRAIN_WAIT))?;
        let mut line = String::new();
        BufReader::new(&stream).read_line(&mut line)?;
        if line != "DRAINED\n" {
            return Err(std::io::Error::other(
                "the old server did not finish its drain",
            ));
        }
        stream.set_read_timeout(None)?;
        Ok(Taken {
            listener: TcpListener::from(listener),
            stream: Some(stream),
        })
    }

    /// The socket that the standby left on standard input after a failed handover.
    pub fn from_parent() -> std::io::Result<Taken> {
        let listener = std::io::stdin().as_fd().try_clone_to_owned()?;
        // Standard input must not keep the port open for a command that this server starts.
        rustix::stdio::dup2_stdin(File::open("/dev/null")?)?;
        Ok(Taken {
            listener: TcpListener::from(listener),
            stream: None,
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
        let socket_path = me.with_file_name(STANDBY_SOCKET);
        let _ = std::fs::remove_file(&socket_path);
        let offer = UnixListener::bind(&socket_path)?;
        let parent = UnixStream::from(std::io::stdin().as_fd().try_clone_to_owned()?);
        let listener = recv_fds(&parent)?.pop().ok_or_else(|| missing("socket"))?;
        // The old process ended, so it no longer holds the database.
        std::io::copy(&mut &parent, &mut std::io::sink())?;
        let (mut line, _) = offer.accept()?;
        line.write_all(b"DRAINED\n")?;
        let mut answer = String::new();
        BufReader::new(&line).read_line(&mut answer)?;
        let _ = std::fs::remove_file(&socket_path);
        if answer == "READY\n" {
            return std::fs::remove_file(me);
        }
        // The new server failed before it served: the old build serves again.
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
    use std::io::{Read, Write};
    use std::os::unix::net::UnixListener;

    #[test]
    fn socket_pass_gives_the_same_port_and_ends_with_ready() {
        let dir = tempfile::tempdir().expect("temporary state");
        let kept = TcpListener::bind("127.0.0.1:0").expect("listener");
        let port = kept.local_addr().expect("address").port();
        let offered = offer(dir.path(), kept).expect("offer");
        let state = dir.path().to_path_buf();
        let new_server = std::thread::spawn(move || take(&state));
        // The old server: it got the request. The standby would now send `DRAINED`.
        let _handed = offered.blocking_recv().expect("handover request");
        // The standby: it listens, the new server connects, DRAINED follows.
        let standby = UnixListener::bind(dir.path().join(STANDBY_SOCKET)).expect("standby socket");
        let (mut line_out, _) = standby.accept().expect("new server connects");
        line_out.write_all(b"DRAINED\n").expect("drained");
        let mut taken = new_server.join().expect("join").expect("take");
        assert_eq!(taken.listener.local_addr().expect("address").port(), port);
        // A client of the old port reaches the taken socket.
        let mut client = std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect");
        taken.listener.set_nonblocking(false).expect("blocking");
        let (mut accepted, _) = taken.listener.accept().expect("accept");
        client.write_all(b"x").expect("write");
        let mut byte = [0_u8; 1];
        accepted.read_exact(&mut byte).expect("read");
        assert_eq!(&byte, b"x");
        taken.ready().expect("ready");
        let mut line = String::new();
        std::io::BufRead::read_line(&mut std::io::BufReader::new(&line_out), &mut line)
            .expect("read ready");
        assert_eq!(line, "READY\n");
    }

    #[test]
    fn take_without_an_offer_fails_and_changes_nothing() {
        let dir = tempfile::tempdir().expect("temporary state");
        assert!(take(dir.path()).is_err());
    }
}
