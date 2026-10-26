//! Bounded state-directory logs shared by provider and runtime adapters.
//!
//! Rotation follows `codex_log_rotation.RotatingLog`: `<name>.1` is newest,
//! writes are split at the byte limit, and an oversized existing current file
//! moves to `.1` before new data is appended. Provider readers enqueue without
//! waiting; a full queue drops the offered line and increments `dropped_lines`.

use file_rotate::{ContentLimit, FileRotate, compression::Compression, suffix::AppendCount};
use std::{
    fs::{self, OpenOptions},
    io::{self, Write},
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
        mpsc::{self, SyncSender},
    },
    thread,
};
#[cfg(test)]
use studio_runtime_store::path_identity::CanonicalPath;

pub const APP_SERVER_LOG: LogConfig = LogConfig {
    filename: "app-server.log",
    max_bytes: 50 * 1024 * 1024,
    backups: 4,
    private: false,
};
pub const ORCHESTRATION_ERRORS_LOG: LogConfig = LogConfig {
    filename: "orchestration-errors.log",
    max_bytes: 1024 * 1024,
    backups: 2,
    private: true,
};
pub const ROTATIONS_LOG: LogConfig = LogConfig {
    filename: "rotations.jsonl",
    max_bytes: 1024 * 1024,
    backups: 3,
    private: false,
};
const DEFAULT_QUEUE_LINES: usize = 128;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LogConfig {
    pub filename: &'static str,
    pub max_bytes: usize,
    pub backups: usize,
    pub private: bool,
}

#[derive(Clone)]
pub struct OperationalLog {
    sender: SyncSender<WriterCommand>,
    dropped_lines: Arc<AtomicU64>,
}

enum WriterCommand {
    Write(Vec<u8>),
    Flush(SyncSender<io::Result<()>>),
}

impl OperationalLog {
    pub fn app_server(state_dir: &Path) -> io::Result<Self> {
        Self::for_config(state_dir.join(APP_SERVER_LOG.filename), APP_SERVER_LOG)
    }

    pub fn orchestration_errors(state_dir: &Path) -> io::Result<Self> {
        Self::for_config(
            state_dir.join(ORCHESTRATION_ERRORS_LOG.filename),
            ORCHESTRATION_ERRORS_LOG,
        )
    }

    pub fn rotations(state_dir: &Path) -> io::Result<Self> {
        Self::for_config(
            state_dir
                .join("native-runtime")
                .join(ROTATIONS_LOG.filename),
            ROTATIONS_LOG,
        )
    }

    pub fn for_config(path: PathBuf, config: LogConfig) -> io::Result<Self> {
        Self::open_with_mode(
            path,
            config.max_bytes,
            config.backups,
            DEFAULT_QUEUE_LINES,
            config.private,
        )
    }

    pub fn open(
        path: PathBuf,
        max_bytes: usize,
        backups: usize,
        queue_lines: usize,
    ) -> io::Result<Self> {
        Self::open_with_mode(path, max_bytes, backups, queue_lines, false)
    }

    fn open_with_mode(
        path: PathBuf,
        max_bytes: usize,
        backups: usize,
        queue_lines: usize,
        private: bool,
    ) -> io::Result<Self> {
        #[cfg(not(unix))]
        let _ = private;
        if max_bytes == 0 || queue_lines == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "log limits must be positive",
            ));
        }
        let parent = path.parent().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "log path needs a parent")
        })?;
        fs::create_dir_all(parent)?;
        let mut options = OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&path)?;
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::PermissionsExt;
            file.set_permissions(fs::Permissions::from_mode(0o600))?;
        }
        drop(file);

        let mut rotation_options = OpenOptions::new();
        rotation_options.read(true).create(true).append(true);
        #[cfg(unix)]
        if private {
            use std::os::unix::fs::OpenOptionsExt;
            rotation_options.mode(0o600);
        }

        let mut writer = FileRotate::new(
            &path,
            AppendCount::new(backups),
            ContentLimit::Bytes(max_bytes),
            Compression::None,
            Some(rotation_options),
        );
        if fs::metadata(&path)?.len() > max_bytes as u64 {
            writer.rotate()?;
        }
        Self::from_writer(writer, queue_lines)
    }

    pub(crate) fn from_writer<W: Write + Send + 'static>(
        writer: W,
        queue_lines: usize,
    ) -> io::Result<Self> {
        let (sender, receiver) = mpsc::sync_channel(queue_lines);
        let dropped_lines = Arc::new(AtomicU64::new(0));
        thread::Builder::new()
            .name("operational-log-writer".to_owned())
            .spawn(move || writer_task(receiver, writer))?;
        Ok(Self {
            sender,
            dropped_lines,
        })
    }

    /// Enqueue one raw log line without waiting for disk or queue capacity.
    pub fn write_line(&self, line: &[u8]) {
        if line.is_empty() {
            return;
        }
        if self
            .sender
            .try_send(WriterCommand::Write(line.to_vec()))
            .is_err()
        {
            self.dropped_lines.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[must_use]
    pub fn dropped_lines(&self) -> u64 {
        self.dropped_lines.load(Ordering::Relaxed)
    }

    pub fn note_dropped_line(&self) {
        self.dropped_lines.fetch_add(1, Ordering::Relaxed);
    }

    /// Wait until previously queued records reach the underlying writer.
    /// Provider readers must use `write_line`, never this blocking operation.
    pub fn flush(&self) -> io::Result<()> {
        let (reply, result) = mpsc::sync_channel(1);
        self.sender
            .send(WriterCommand::Flush(reply))
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "log writer stopped"))?;
        result
            .recv()
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "log writer stopped"))?
    }
}

fn writer_task<W: Write>(receiver: mpsc::Receiver<WriterCommand>, mut writer: W) {
    while let Ok(command) = receiver.recv() {
        let result = match command {
            WriterCommand::Write(bytes) => writer.write_all(&bytes),
            WriterCommand::Flush(reply) => {
                let result = writer.flush();
                drop(reply.send(result));
                continue;
            }
        };
        if result.is_err() {
            return;
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;
    use std::{io, process::Command, sync::mpsc as std_mpsc};
    use tempfile::tempdir;

    #[test]
    fn counted_rotation_keeps_the_exact_bound_and_python_backup_order() {
        let directory = tempdir().expect("isolated test directory");
        let path = directory.path().join("app-server.log");
        let writer = OperationalLog::open(path.clone(), 8, 2, 8).expect("log writer");
        writer.write_line(b"abcdefghijklmnopqrstuvwxyz");
        writer.flush().expect("flush");

        let current = fs::read(&path).expect("current log");
        let newest = fs::read(path.with_extension("log.1")).expect("newest backup");
        let older = fs::read(path.with_extension("log.2")).expect("older backup");
        assert_eq!(current, b"yz");
        assert_eq!(newest, b"qrstuvwx");
        assert_eq!(older, b"ijklmnop");
        assert!(current.len() <= 8 && newest.len() <= 8 && older.len() <= 8);
    }

    #[test]
    fn standard_logs_use_the_python_state_directory_paths_and_limits() {
        let directory = tempdir().expect("isolated test directory");
        let app = OperationalLog::app_server(directory.path()).expect("app log");
        let errors = OperationalLog::orchestration_errors(directory.path()).expect("error log");
        let rotations = OperationalLog::rotations(directory.path()).expect("rotation log");
        app.write_line(b"app\n");
        errors.write_line(b"error\n");
        rotations.write_line(b"{}\n");
        app.flush().expect("app log flush");
        errors.flush().expect("error log flush");
        rotations.flush().expect("rotation log flush");

        assert_eq!(APP_SERVER_LOG.max_bytes, 50 * 1024 * 1024);
        assert_eq!(APP_SERVER_LOG.backups, 4);
        assert_eq!(ORCHESTRATION_ERRORS_LOG.max_bytes, 1024 * 1024);
        assert_eq!(ORCHESTRATION_ERRORS_LOG.backups, 2);
        assert_eq!(ROTATIONS_LOG.max_bytes, 1024 * 1024);
        assert_eq!(ROTATIONS_LOG.backups, 3);
        assert_eq!(
            fs::read(directory.path().join("app-server.log")).expect("app log"),
            b"app\n"
        );
        assert_eq!(
            fs::read(directory.path().join("orchestration-errors.log"))
                .expect("orchestration error log"),
            b"error\n"
        );
        assert_eq!(
            fs::read(directory.path().join("native-runtime/rotations.jsonl"))
                .expect("rotation log"),
            b"{}\n"
        );
    }

    #[test]
    fn appends_to_python_written_current_and_backup_files_with_identical_bytes() {
        let directory = tempdir().expect("isolated test directory");
        let python_source = CanonicalPath::existing(
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        )
        .expect("Python source directory");
        let seed = directory.path().join("seed");
        fs::create_dir_all(&seed).expect("seed directory");
        let python = |script: &str, root: &Path| {
            Command::new("python3")
                .arg("-c")
                .arg(script)
                .arg(&python_source)
                .arg(root)
                .output()
                .expect("run Python log oracle")
        };
        let seed_result = python(
            "import sys; from pathlib import Path; sys.path.insert(0, sys.argv[1]); from codex_log_rotation import RotatingLog; p=Path(sys.argv[2])/'app-server.log'; w=RotatingLog(p,max_bytes=8,backups=2); w.write(b'python-seed-current-and-backups'); w.close()",
            &seed,
        );
        assert!(seed_result.status.success(), "Python seed writer failed");

        let rust_root = directory.path().join("rust");
        let python_root = directory.path().join("python");
        fs::create_dir_all(&rust_root).expect("Rust directory");
        fs::create_dir_all(&python_root).expect("Python directory");
        for name in ["app-server.log", "app-server.log.1", "app-server.log.2"] {
            let source = seed.join(name);
            if source.exists() {
                fs::copy(&source, rust_root.join(name)).expect("copy seed for Rust");
                fs::copy(&source, python_root.join(name)).expect("copy seed for Python");
            }
        }

        let rust = OperationalLog::open(rust_root.join("app-server.log"), 8, 2, 16)
            .expect("Rust writer opens existing logs");
        let chunks = [b"-continued".as_slice(), b"-and-rotated"];
        for chunk in chunks {
            rust.write_line(chunk);
        }
        rust.flush().expect("Rust writer flush");
        let python_result = python(
            "import sys; from pathlib import Path; sys.path.insert(0, sys.argv[1]); from codex_log_rotation import RotatingLog; p=Path(sys.argv[2])/'app-server.log'; w=RotatingLog(p,max_bytes=8,backups=2); w.write(b'-continued'); w.write(b'-and-rotated'); w.close()",
            &python_root,
        );
        assert!(python_result.status.success(), "Python continuation failed");

        for name in ["app-server.log", "app-server.log.1", "app-server.log.2"] {
            assert_eq!(
                fs::read(rust_root.join(name)).expect("Rust result"),
                fs::read(python_root.join(name)).expect("Python result"),
                "on-disk bytes differ for {name}"
            );
        }
    }

    #[test]
    fn multiple_writers_preserve_each_record() {
        let directory = tempdir().expect("isolated test directory");
        let path = directory.path().join("concurrent.log");
        let writer = OperationalLog::open(path.clone(), 1024 * 1024, 2, 1024).expect("log writer");
        let threads = (0..8)
            .map(|thread| {
                let writer = writer.clone();
                std::thread::spawn(move || {
                    for index in 0..100 {
                        writer.write_line(format!("{thread}:{index}\n").as_bytes());
                    }
                })
            })
            .collect::<Vec<_>>();
        for thread in threads {
            thread.join().expect("writer thread");
        }
        writer.flush().expect("flush");
        let text = fs::read_to_string(path).expect("log contents");
        let lines = text.lines().collect::<std::collections::HashSet<_>>();
        assert_eq!(lines.len(), 800);
        assert_eq!(text.lines().count(), 800);
    }

    struct SlowWriter {
        started: std_mpsc::Sender<()>,
        release: std_mpsc::Receiver<()>,
        has_blocked: bool,
    }

    impl Write for SlowWriter {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            if !self.has_blocked {
                self.has_blocked = true;
                self.started.send(()).map_err(io::Error::other)?;
                self.release.recv().map_err(io::Error::other)?;
            }
            Ok(bytes.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    #[tokio::test]
    async fn provider_reader_enqueue_does_not_wait_for_a_slow_disk() {
        let (started_tx, started_rx) = std_mpsc::channel();
        let (release_tx, release_rx) = std_mpsc::channel();
        let writer = OperationalLog::from_writer(
            SlowWriter {
                started: started_tx,
                release: release_rx,
                has_blocked: false,
            },
            1,
        )
        .expect("slow writer task");

        let (mut input, mut stderr) = tokio::io::duplex(128);
        let log = writer.clone();
        let reader = tokio::spawn(async move {
            crate::provider_stdio::drain_stderr(&mut stderr, Some(&log)).await
        });
        use tokio::io::AsyncWriteExt;
        input.write_all(b"first\n").await.expect("first line");
        crate::blocking_pool::spawn_blocking("runtime-process/operational_log", move || {
            started_rx.recv()
        })
        .await
        .expect("wait for fake disk")
        .expect("fake disk started");
        input
            .write_all(b"queued\ndropped\n")
            .await
            .expect("remaining lines");
        input.shutdown().await.expect("stderr EOF");
        reader.await.expect("reader task").expect("stderr drained");
        assert_eq!(writer.dropped_lines(), 1);
        release_tx.send(()).expect("release fake disk");
        writer.flush().expect("flush after release");
    }
}
