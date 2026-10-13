//! Owned pipes and process group. Socket disconnects never stop a child.
use super::supervisor_daemon::{Error, Result, lock, now};
use super::supervisor_identity::{RETRY, free_space, start_time};
use process_wrap::std::{ChildWrapper, CommandWrap, ProcessGroup};
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::{BufRead, BufReader, Read, Write},
    path::{Path, PathBuf},
    process::{Command, Stdio},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use studio_runtime_providers::supervisor_v1::HANDLE_LIMIT;
use studio_runtime_store::supervisor_journal::{AppendResult, SupervisorJournal};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);
pub(super) struct OwnedChild {
    pub pid: u32,
    pub signature: String,
    pub start: String,
    pub generation: u64,
    pub stopping: AtomicBool,
    pub stdout_alive: AtomicBool,
    pub stderr_alive: AtomicBool,
    pub paused: AtomicBool,
    pub errors: Mutex<BTreeMap<String, String>>,
    pub stdout_error: Mutex<Option<String>>,
    process: Mutex<Box<dyn ChildWrapper>>,
    append_lock: Mutex<()>,
    pub write_lock: Mutex<()>,
    writer: mpsc::SyncSender<(Vec<u8>, mpsc::SyncSender<std::io::Result<()>>)>,
    journal: Arc<SupervisorJournal>,
    root: PathBuf,
    handle: String,
}
impl OwnedChild {
    pub fn spawn(
        journal: Arc<SupervisorJournal>,
        root: &Path,
        handle: &str,
        signature: String,
        command: &[String],
        env: &BTreeMap<String, String>,
        cwd: Option<&str>,
    ) -> Result<Arc<Self>> {
        let executable = command
            .first()
            .ok_or(Error::Refused("Invalid supervisor command"))?;
        let mut command_builder = Command::new(executable);
        command_builder
            .args(&command[1..])
            .env_clear()
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        if let Some(cwd) = cwd {
            command_builder.current_dir(cwd);
        }
        let mut process = CommandWrap::from(command_builder)
            .wrap(ProcessGroup::leader())
            .spawn()?;
        let pid = process.id();
        let prepare = (|| {
            let start = start_time(pid, true)?.ok_or(Error::Refused(
                "Cannot prove the native child process identity",
            ))?;
            let generation = journal.opened(handle, &signature, pid, &start)?;
            Ok((start, generation))
        })();
        let (start, generation) = match prepare {
            Ok(v) => v,
            Err(error) => {
                process.kill()?;
                return Err(error);
            }
        };
        let mut stdin = process
            .stdin()
            .take()
            .ok_or(Error::Refused("Missing child stdin"))?;
        let stdout = process
            .stdout()
            .take()
            .ok_or(Error::Refused("Missing child stdout"))?;
        let mut stderr = process
            .stderr()
            .take()
            .ok_or(Error::Refused("Missing child stderr"))?;
        let (writer, receiver) =
            mpsc::sync_channel::<(Vec<u8>, mpsc::SyncSender<std::io::Result<()>>)>(1);
        let child = Arc::new(Self {
            pid,
            signature,
            start,
            generation,
            stopping: AtomicBool::new(false),
            stdout_alive: AtomicBool::new(true),
            stderr_alive: AtomicBool::new(true),
            paused: AtomicBool::new(false),
            errors: Mutex::new(BTreeMap::new()),
            stdout_error: Mutex::new(None),
            process: Mutex::new(process),
            append_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
            writer,
            journal,
            root: root.to_owned(),
            handle: handle.to_owned(),
        });
        thread::spawn(move || {
            while let Ok((bytes, reply)) = receiver.recv() {
                let result = stdin.write_all(&bytes).and_then(|()| stdin.flush());
                let failed = result.is_err();
                if reply.send(result).is_err() { /* The receipt remains durable after timeout. */ }
                if failed {
                    break;
                }
            }
        });
        let copy = child.clone();
        thread::spawn(move || {
            let result = copy.stdout(BufReader::new(stdout));
            if let Err(error) = result
                && let Ok(mut saved) = copy.stdout_error.lock()
            {
                *saved = Some(error.to_string());
            }
            copy.stdout_alive.store(false, Ordering::Release);
        });
        let copy = child.clone();
        thread::spawn(move || {
            let result = (|| -> Result<()> {
                let mut buffer = [0; 65536];
                loop {
                    let n = stderr.read(&mut buffer)?;
                    if n == 0 {
                        break;
                    }
                    copy.append(
                        "stderr",
                        json!({"data":String::from_utf8_lossy(&buffer[..n])}),
                    )?;
                }
                Ok(())
            })();
            if let Err(error) = result
                && let Ok(mut errors) = copy.errors.lock()
            {
                errors.insert("stderr".into(), error.to_string());
            }
            copy.stderr_alive.store(false, Ordering::Release);
        });
        Ok(child)
    }
    pub fn code(&self) -> Result<Option<i32>> {
        use std::os::unix::process::ExitStatusExt;
        Ok(lock(&self.process)?
            .try_wait()?
            .map(|s| s.code().unwrap_or_else(|| -s.signal().unwrap_or(1))))
    }
    pub fn write(&self, message: &Value) -> Result<()> {
        let mut bytes = serde_json::to_vec(message)?;
        bytes.push(b'\n');
        let (sender, receiver) = mpsc::sync_channel(1);
        self.writer.try_send((bytes, sender)).map_err(|_| {
            Error::Refused("Child input unavailable; durable outcome remains unknown")
        })?;
        receiver.recv_timeout(WRITE_TIMEOUT).map_err(|_| {
            Error::Refused("Child input deadline exceeded; durable outcome remains unknown")
        })??;
        Ok(())
    }
    fn stdout(&self, mut reader: impl BufRead) -> Result<()> {
        loop {
            let mut bytes = Vec::new();
            let n = reader
                .by_ref()
                .take(HANDLE_LIMIT + 1)
                .read_until(b'\n', &mut bytes)?;
            if n == 0 {
                break;
            }
            if n as u64 > HANDLE_LIMIT {
                return Err(Error::Refused(
                    "Child stdout frame exceeds journal capacity",
                ));
            }
            let mut value = serde_json::from_slice::<Value>(&bytes)
                .unwrap_or_else(|_| json!({"supervisorRaw":String::from_utf8_lossy(&bytes)}));
            if value.get("method").is_some_and(Value::is_string) {
                value["_studioSupervisorReceivedAt"] = json!(now()?);
            }
            self.append("stdout", value)?;
        }
        loop {
            if self.stopping.load(Ordering::Acquire) {
                return Ok(());
            }
            if let Some(code) = self.code()? {
                return self.append("exit", json!({"returnCode":code}));
            }
            thread::sleep(RETRY);
        }
    }
    fn append(&self, kind: &str, payload: Value) -> Result<()> {
        let _append = lock(&self.append_lock)?;
        let row = self
            .journal
            .handle(&self.handle)?
            .ok_or(Error::Refused("Unknown supervisor handle"))?;
        let sequence = row.snapshot.sequence() + 1;
        while !self.stopping.load(Ordering::Acquire) {
            let result = free_space(&self.root, 1024 * 1024).and_then(|()| {
                Ok(self
                    .journal
                    .append(&self.handle, self.generation, sequence, kind, &payload)?)
            });
            match result {
                Ok(AppendResult::Saved | AppendResult::Obsolete) => {
                    let mut errors = lock(&self.errors)?;
                    errors.remove(kind);
                    self.paused.store(!errors.is_empty(), Ordering::Release);
                    return Ok(());
                }
                Ok(AppendResult::Full) => {}
                Err(Error::Journal(ref error)) if !error.retryable() => return result.map(|_| ()),
                Err(ref error) => {
                    lock(&self.errors)?.insert(kind.into(), error.to_string());
                }
            }
            self.paused.store(true, Ordering::Release);
            thread::sleep(RETRY);
        }
        Ok(())
    }
    pub fn reap(&self) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while self.code()?.is_none() {
            if std::time::Instant::now() >= deadline {
                return Err(Error::Refused("Child did not reap before deadline"));
            }
            thread::sleep(RETRY);
        }
        Ok(())
    }
}
