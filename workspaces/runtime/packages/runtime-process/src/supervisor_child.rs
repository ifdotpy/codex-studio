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
        Arc, Condvar, Mutex,
        atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering},
        mpsc,
    },
    thread,
    time::Duration,
};
use studio_runtime_providers::supervisor_v1::{Command as SupervisorCommand, HANDLE_LIMIT};
use studio_runtime_store::supervisor_journal::{AppendResult, SupervisorJournal};

const WRITE_TIMEOUT: Duration = Duration::from_secs(5);

#[repr(u8)]
enum ExitEventState {
    Pending = 0,
    Persisting = 1,
    Persisted = 2,
    Failed = 3,
}

/// Result of a child start. An `Err` from `OwnedChild::spawn` means a child
/// may remain; `NotStarted` proves that no child process remains.
pub(super) enum SpawnOutcome {
    Started(Arc<OwnedChild>),
    NotStarted(Error),
}

pub(super) struct OwnedChild {
    pub pid: u32,
    pub signature: String,
    pub start: String,
    pub generation: u64,
    pub stopping: AtomicBool,
    pub stdout_alive: AtomicBool,
    pub stderr_alive: AtomicBool,
    pub notify: Arc<tokio::sync::Notify>,
    pub paused: AtomicBool,
    pub errors: Mutex<BTreeMap<String, String>>,
    pub stdout_error: Mutex<Option<String>>,
    process: Mutex<Box<dyn ChildWrapper>>,
    exit_status: Mutex<Option<std::result::Result<i32, String>>>,
    exit_changed: Condvar,
    exit_published: AtomicBool,
    exit_event_state: AtomicU8,
    exit_event_sequence: AtomicU64,
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
        request: &SupervisorCommand,
        notify: Arc<tokio::sync::Notify>,
        signature: String,
    ) -> Result<SpawnOutcome> {
        let SupervisorCommand::Open {
            handle,
            command,
            env,
            cwd,
        } = request
        else {
            return Err(Error::Refused("Invalid supervisor command"));
        };
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
        let mut process = match CommandWrap::from(command_builder)
            .wrap(ProcessGroup::leader())
            .spawn()
        {
            Ok(process) => process,
            // The OS refused the start (for example ENOENT or EPERM), so no
            // child process exists.
            Err(error) => return Ok(SpawnOutcome::NotStarted(error.into())),
        };
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
                // `kill` signals the whole process group and reaps the leader.
                // If it fails, the child may remain, so the reservation stays
                // open and recovery treats its outcome as unknown.
                process.kill()?;
                return Ok(SpawnOutcome::NotStarted(error));
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
            notify,
            paused: AtomicBool::new(false),
            errors: Mutex::new(BTreeMap::new()),
            stdout_error: Mutex::new(None),
            process: Mutex::new(process),
            exit_status: Mutex::new(None),
            exit_changed: Condvar::new(),
            exit_published: AtomicBool::new(false),
            exit_event_state: AtomicU8::new(ExitEventState::Pending as u8),
            exit_event_sequence: AtomicU64::new(0),
            append_lock: Mutex::new(()),
            write_lock: Mutex::new(()),
            writer,
            journal,
            root: root.to_owned(),
            handle: handle.to_owned(),
        });
        let copy = child.clone();
        thread::spawn(move || {
            use std::os::unix::process::ExitStatusExt;
            let status = lock(&copy.process)
                .and_then(|mut process| process.wait().map_err(Error::from))
                .map(|status| {
                    status
                        .code()
                        .unwrap_or_else(|| -status.signal().unwrap_or(1))
                })
                .map_err(|error| error.to_string());
            if let Ok(mut saved) = copy.exit_status.lock() {
                *saved = Some(status);
                copy.exit_changed.notify_all();
            }
            copy.publish_exit();
            copy.notify.notify_waiters();
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
            copy.publish_exit();
            copy.notify.notify_waiters();
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
            copy.publish_exit();
            copy.notify.notify_waiters();
        });
        Ok(SpawnOutcome::Started(child))
    }
    pub fn code(&self) -> Result<Option<i32>> {
        match lock(&self.exit_status)?.as_ref() {
            Some(Ok(code)) => Ok(Some(*code)),
            Some(Err(_)) => Err(Error::Refused("Could not wait for native child")),
            None => Ok(None),
        }
    }
    pub fn durable_exit_sequence(&self) -> Result<Option<u64>> {
        match self.exit_event_state.load(Ordering::Acquire) {
            state if state == ExitEventState::Persisted as u8 => {
                let sequence = self.exit_event_sequence.load(Ordering::Acquire);
                if sequence == 0 {
                    Err(Error::Refused(
                        "Native child exit event receipt is incomplete",
                    ))
                } else {
                    Ok(Some(sequence))
                }
            }
            state if state == ExitEventState::Failed as u8 => Err(Error::Refused(
                "Native child exit event could not be recorded durably",
            )),
            _ => Ok(None),
        }
    }
    /// Exit sequence whose journal row has committed. A reader can receive
    /// the committed exit row before `publish_exit` marks it persisted, so a
    /// decision made after that delivery reads the journal, not that mark.
    pub fn committed_exit_sequence(&self) -> Result<Option<u64>> {
        if self.exit_event_state.load(Ordering::Acquire) != ExitEventState::Persisting as u8 {
            return self.durable_exit_sequence();
        }
        // `append` records the exit sequence before its commit can be visible.
        let sequence = self.exit_event_sequence.load(Ordering::Acquire);
        if sequence == 0 {
            return Ok(None);
        }
        let row = self
            .journal
            .handle(&self.handle)?
            .ok_or(Error::Refused("Unknown supervisor handle"))?;
        Ok(
            (row.snapshot.generation() == self.generation && row.snapshot.sequence() >= sequence)
                .then_some(sequence),
        )
    }
    /// Only the Linux deadline tests hold the append lock.
    #[cfg(all(test, target_os = "linux"))]
    pub(super) fn lock_append_for_test(&self) -> Result<std::sync::MutexGuard<'_, ()>> {
        lock(&self.append_lock)
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
        self.publish_exit();
        Ok(())
    }
    #[allow(
        clippy::disallowed_methods,
        reason = "journal appends run on dedicated child reader/reaper threads"
    )]
    fn append(&self, kind: &str, payload: Value) -> Result<(AppendResult, u64)> {
        let _append = lock(&self.append_lock)?;
        let row = self
            .journal
            .handle(&self.handle)?
            .ok_or(Error::Refused("Unknown supervisor handle"))?;
        let sequence = row.snapshot.sequence() + 1;
        if kind == "exit" {
            // Record the sequence before its commit can become visible.
            // Readers trust it only after `Persisted` or a journal commit.
            self.exit_event_sequence.store(sequence, Ordering::Release);
        }
        while !self.stopping.load(Ordering::Acquire) {
            let result = free_space(&self.root, 1024 * 1024).and_then(|()| {
                Ok(self
                    .journal
                    .append(&self.handle, self.generation, sequence, kind, &payload)?)
            });
            match result {
                Ok(AppendResult::Saved) => {
                    let mut errors = lock(&self.errors)?;
                    errors.remove(kind);
                    self.paused.store(!errors.is_empty(), Ordering::Release);
                    self.notify.notify_waiters();
                    return Ok((AppendResult::Saved, sequence));
                }
                Ok(AppendResult::Obsolete) => {
                    let mut errors = lock(&self.errors)?;
                    errors.remove(kind);
                    self.paused.store(!errors.is_empty(), Ordering::Release);
                    self.notify.notify_waiters();
                    return Ok((AppendResult::Obsolete, sequence));
                }
                Ok(AppendResult::Full) => {}
                Err(Error::Journal(error)) if !error.retryable() => {
                    return Err(Error::Journal(error));
                }
                Err(ref error) => {
                    lock(&self.errors)?.insert(kind.into(), error.to_string());
                }
            }
            if !self.paused.swap(true, Ordering::AcqRel) {
                self.notify.notify_waiters();
            }
            thread::sleep(RETRY);
        }
        Ok((AppendResult::Obsolete, sequence))
    }
    pub fn reap(&self) -> Result<()> {
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut status = lock(&self.exit_status)?;
        while status.is_none() {
            let remaining = deadline
                .checked_duration_since(std::time::Instant::now())
                .ok_or(Error::Refused("Child did not reap before deadline"))?;
            let (next, timeout) = self
                .exit_changed
                .wait_timeout(status, remaining)
                .map_err(|_| Error::Refused("Supervisor lock poisoned"))?;
            status = next;
            if timeout.timed_out() && status.is_none() {
                return Err(Error::Refused("Child did not reap before deadline"));
            }
        }
        match status.as_ref() {
            Some(Ok(_)) => Ok(()),
            Some(Err(_)) => Err(Error::Refused("Could not wait for native child")),
            None => unreachable!(),
        }
    }

    fn publish_exit(&self) {
        if self.stopping.load(Ordering::Acquire)
            || self.stdout_alive.load(Ordering::Acquire)
            || self.stderr_alive.load(Ordering::Acquire)
        {
            return;
        }
        let status = match lock(&self.exit_status)
            .ok()
            .and_then(|status| status.clone())
        {
            Some(status) => status,
            None => return,
        };
        let code = match status {
            Ok(code) => code,
            Err(error) => {
                if let Ok(mut errors) = self.errors.lock() {
                    errors.insert("exit".into(), error);
                }
                self.exit_event_state
                    .store(ExitEventState::Failed as u8, Ordering::Release);
                self.notify.notify_waiters();
                return;
            }
        };
        if self.exit_published.swap(true, Ordering::AcqRel) {
            return;
        }
        self.exit_event_state
            .store(ExitEventState::Persisting as u8, Ordering::Release);
        match self.append("exit", json!({"returnCode":code})) {
            Ok((AppendResult::Saved, sequence)) => {
                self.exit_event_sequence.store(sequence, Ordering::Release);
                self.exit_event_state
                    .store(ExitEventState::Persisted as u8, Ordering::Release);
            }
            Ok((AppendResult::Full | AppendResult::Obsolete, _)) => {
                if let Ok(mut errors) = self.errors.lock() {
                    errors.insert(
                        "exit".into(),
                        "Native child exit event was not committed".into(),
                    );
                }
                self.exit_event_state
                    .store(ExitEventState::Failed as u8, Ordering::Release);
            }
            Err(error) => {
                if let Ok(mut errors) = self.errors.lock() {
                    errors.insert("exit".into(), error.to_string());
                }
                self.exit_event_state
                    .store(ExitEventState::Failed as u8, Ordering::Release);
            }
        }
        if let Ok(_status) = self.exit_status.lock() {
            self.exit_changed.notify_all();
        }
        self.notify.notify_waiters();
    }
}

// Retained child identity is currently implemented by the Rust supervisor on Linux.
#[cfg(all(test, target_os = "linux"))]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn exit_is_not_terminal_until_its_journal_append_commits() {
        let root = tempfile::tempdir().unwrap();
        let journal = Arc::new(SupervisorJournal::open(root.path()).unwrap());
        let handle = "server-command:exit-event-gate";
        journal
            .reserve(handle, "exit-event-test", now().unwrap())
            .unwrap();
        let command = vec![
            "/bin/sh".to_owned(),
            "-c".to_owned(),
            "IFS= read -r line; exit 0".to_owned(),
        ];
        let cwd = root.path().to_string_lossy().into_owned();
        let SpawnOutcome::Started(child) = OwnedChild::spawn(
            journal.clone(),
            root.path(),
            &SupervisorCommand::Open {
                handle: handle.to_owned(),
                command,
                env: BTreeMap::new(),
                cwd: Some(cwd.clone()),
            },
            Arc::new(tokio::sync::Notify::new()),
            "exit-event-test".to_owned(),
        )
        .unwrap() else {
            panic!("the test child starts");
        };

        let append_guard = child.append_lock.lock().unwrap();
        child.write(&json!({"finish":true})).unwrap();
        child.reap().unwrap();

        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while child.stdout_alive.load(Ordering::Acquire)
            || child.stderr_alive.load(Ordering::Acquire)
        {
            assert!(
                std::time::Instant::now() < deadline,
                "child readers did not finish"
            );
            thread::yield_now();
        }
        assert_eq!(child.code().unwrap(), Some(0));
        assert!(child.durable_exit_sequence().unwrap().is_none());
        assert!(child.committed_exit_sequence().unwrap().is_none());
        assert!(
            journal.events(handle, 0, false).unwrap()["events"]
                .as_array()
                .unwrap()
                .is_empty()
        );

        drop(append_guard);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        while child.durable_exit_sequence().unwrap().is_none() {
            assert!(
                std::time::Instant::now() < deadline,
                "exit event did not become durable"
            );
            thread::yield_now();
        }
        let exit_sequence = child.durable_exit_sequence().unwrap().unwrap();
        let events = journal.events(handle, 0, false).unwrap();
        assert_eq!(events["events"][0]["kind"], "exit");
        assert_eq!(events["events"][0]["sequence"], exit_sequence);

        // A nextBatch reader can deliver the committed exit row before
        // `publish_exit` marks it persisted. Restore that window: an open or
        // retire decided after the delivery must still see the commit.
        child
            .exit_event_state
            .store(ExitEventState::Persisting as u8, Ordering::Release);
        assert!(child.durable_exit_sequence().unwrap().is_none());
        assert_eq!(
            child.committed_exit_sequence().unwrap(),
            Some(exit_sequence)
        );
    }
}
