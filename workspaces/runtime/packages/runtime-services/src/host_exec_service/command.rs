//! The Mac command of one operation: start, output events, limits, cancel and
//! the collection of its changes and artifacts.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Map, Value, json};
use tokio::io::AsyncReadExt;

use super::protocol::{self, Failure, HostExecError, INVALID, Manifest, Outcome, require};
use super::{
    ChildHandle, ENV_ARTIFACTS, ENV_DERIVED_DATA, ENV_PACKAGE_CACHE, ENV_STATE, ENV_TREE,
    HostService, Op, integer, now, number, string_list, text,
};

/// Command output that the service keeps, in bytes; more stops the command.
const OUTPUT_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
/// One read of a command pipe, in bytes.
const PIPE_READ_BYTES: usize = 32 * 1024;
/// Wait after SIGTERM before SIGKILL, in seconds.
const TERMINATE_GRACE_SECONDS: u64 = 3;
/// Wait for the output readers after the command ended, in seconds.
const READERS_SECONDS: u64 = 6;
/// Shortest pause of the disk budget watch, in seconds.
const BUDGET_PAUSE_SECONDS: f64 = 1.0;
/// The budget watch pauses this many times as long as its last walk took.
const BUDGET_PAUSE_FACTOR: f64 = 4.0;
/// Largest argument count of a command.
const MAX_ARGUMENTS: usize = 1024;

type Reason = Arc<Mutex<&'static str>>;

fn set_reason(reason: &Reason, value: &'static str) {
    *reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = value;
}

fn get_reason(reason: &Reason) -> &'static str {
    *reason
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn layr_id(value: &str, lengths: std::ops::RangeInclusive<usize>) -> bool {
    lengths.contains(&value.len())
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// For each command leader PID: whether the leader was still an unreaped child
/// of this process at each signal to its group.
#[cfg(test)]
pub(crate) static LEADER_UNREAPED_AT_SIGNAL: Mutex<std::collections::BTreeMap<i32, Vec<bool>>> =
    Mutex::new(std::collections::BTreeMap::new());

/// Signal the command's group unless its leader is reaped; a reaped leader
/// answers like an ended group (ESRCH). The lock orders this with the reap.
fn signal_group(child: &ChildHandle, signal: nix::sys::signal::Signal) -> nix::Result<()> {
    let reaped = child
        .reaped
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *reaped {
        return Err(nix::errno::Errno::ESRCH);
    }
    let pid = child.pid;
    #[cfg(test)]
    LEADER_UNREAPED_AT_SIGNAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .entry(pid)
        .or_default()
        .push(u32::try_from(pid).is_ok_and(|pid| {
            studio_runtime_process::owned_process_group::exited_unreaped_now(pid).is_ok()
        }));
    nix::sys::signal::killpg(nix::unistd::Pid::from_raw(pid), signal)
}

/// Wait for the leader's exit without reaping it, end the other processes of
/// its group while the exited leader still pins the group ID, then reap under
/// the lock that every group signal holds (`owned_process_group`).
async fn reap_command(
    mut child: tokio::process::Child,
    handle: ChildHandle,
) -> std::io::Result<std::process::ExitStatus> {
    let observed = match u32::try_from(handle.pid) {
        Ok(pid) => studio_runtime_process::owned_process_group::exited_unreaped(pid).await,
        Err(_) => Err(std::io::Error::from(std::io::ErrorKind::InvalidInput)),
    };
    if observed.is_err() {
        // The leader may already be reaped elsewhere: send no more group signals.
        *handle
            .reaped
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = true;
        return child.wait().await;
    }
    // Other processes of the command's group end with it.
    signal_group(&handle, nix::sys::signal::Signal::SIGKILL).unwrap_or_default();
    loop {
        {
            let mut reaped = handle
                .reaped
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let status = child.try_wait();
            if !matches!(status, Ok(None)) {
                *reaped = true;
            }
            if let Some(status) = status? {
                return Ok(status);
            }
        }
        // The exit is observed, so the reap succeeds at once.
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

/// Stop a command's process group: SIGTERM, then SIGKILL after the grace time.
pub(crate) async fn terminate(child: &ChildHandle) {
    use nix::sys::signal::Signal;
    if signal_group(child, Signal::SIGTERM) == Err(nix::errno::Errno::ESRCH) {
        return;
    }
    let mut exited = child.exited.clone();
    if tokio::time::timeout(
        Duration::from_secs(TERMINATE_GRACE_SECONDS),
        exited.wait_for(|done| *done),
    )
    .await
    .is_err()
    {
        signal_group(child, Signal::SIGKILL).unwrap_or_default();
        drop(exited.wait_for(|done| *done).await);
    }
}

/// Append `pending` UTF-8 bytes to `text`; an invalid sequence becomes U+FFFD.
/// An incomplete sequence at the end waits for more bytes unless `last`.
fn decode(pending: &mut Vec<u8>, text: &mut String, last: bool) {
    loop {
        match std::str::from_utf8(pending) {
            Ok(valid) => {
                text.push_str(valid);
                pending.clear();
                return;
            }
            Err(error) => {
                let valid = error.valid_up_to();
                text.push_str(&String::from_utf8_lossy(&pending[..valid]));
                match error.error_len() {
                    Some(length) => {
                        text.push('\u{fffd}');
                        pending.drain(..valid + length);
                    }
                    None if last => {
                        text.push('\u{fffd}');
                        pending.clear();
                        return;
                    }
                    None => {
                        pending.drain(..valid);
                        return;
                    }
                }
            }
        }
    }
}

struct Drain {
    service: Arc<HostService>,
    identity: String,
    stream: &'static str,
    slot_path: String,
    line_path: String,
    output: Arc<AtomicU64>,
    reason: Reason,
    child: ChildHandle,
}

impl Drain {
    fn emit(&self, text: &str) -> Outcome<()> {
        let mut event = Map::new();
        event.insert("stream".into(), json!(self.stream));
        event.insert(
            "text".into(),
            json!(protocol::mapped(text, &self.slot_path, &self.line_path)),
        );
        self.service.event(&self.identity, event)
    }

    /// Output events of one pipe. A slot path split across two reads stays whole,
    /// so each event maps it to the line path.
    async fn run(self, mut pipe: impl tokio::io::AsyncRead + Unpin) -> Outcome<()> {
        let aliases = protocol::aliases(&self.slot_path);
        let keep = aliases.iter().map(String::len).max().unwrap_or(0);
        let mut pending = Vec::new();
        let mut tail = String::new();
        let mut block = vec![0_u8; PIPE_READ_BYTES];
        loop {
            let read = pipe.read(&mut block).await?;
            if read == 0 {
                break;
            }
            let total = self.output.fetch_add(read as u64, Ordering::SeqCst) + read as u64;
            if total > OUTPUT_LIMIT_BYTES {
                set_reason(&self.reason, "output_limit");
                terminate(&self.child).await;
                break;
            }
            pending.extend_from_slice(&block[..read]);
            decode(&mut pending, &mut tail, false);
            let mut end = tail.len().saturating_sub(keep);
            while !tail.is_char_boundary(end) {
                end -= 1;
            }
            for path in &aliases {
                let limit = (end + path.len()).min(tail.len());
                let mut limit_boundary = limit;
                while !tail.is_char_boundary(limit_boundary) {
                    limit_boundary -= 1;
                }
                if let Some(start) = tail[..limit_boundary].rfind(path.as_str())
                    && start < end
                    && end < start + path.len()
                {
                    end = start;
                }
            }
            if end > 0 {
                self.emit(&tail[..end])?;
                tail.drain(..end);
            }
        }
        decode(&mut pending, &mut tail, true);
        self.emit(&tail)
    }
}

impl HostService {
    #[allow(
        clippy::too_many_lines,
        reason = "one ordered check sequence of the run request"
    )]
    pub(crate) async fn run(
        self: &Arc<Self>,
        identity: &str,
        op: Op,
        slot: PathBuf,
        mac_only: Vec<String>,
        params: &Map<String, Value>,
    ) -> Outcome<Op> {
        require(
            text(&op, "state") == "leased",
            "The command already started",
            "outcome_unknown",
        )?;
        let argv: Vec<String> = match params.get("argv") {
            Some(Value::Array(items))
                if !items.is_empty()
                    && items.len() <= MAX_ARGUMENTS
                    && items
                        .iter()
                        .all(|item| item.as_str().is_some_and(|item| !item.contains('\0'))) =>
            {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            }
            _ => return Err(HostExecError::new(INVALID, "Invalid command arguments").into()),
        };
        let timeout = match params.get("timeoutSeconds") {
            None => crate::host_exec::DEFAULT_TIMEOUT_SECONDS,
            value => number(value)
                .filter(|seconds| {
                    *seconds > 0.0 && *seconds <= crate::host_exec::MAX_TIMEOUT_SECONDS
                })
                .ok_or_else(|| HostExecError::new(INVALID, "Invalid command timeout"))?,
        };
        // Cache folders of the line: the Mac receives them, and its changes there never return.
        let caches = Self::cache_list(params.get("caches"))?;
        // The layr state that the guest wrote into the slot; an older guest sends none.
        let layr_state = match params.get("layrState") {
            None | Some(Value::Null) => None,
            Some(Value::String(state)) if layr_id(state, 7..=64) => Some(state.clone()),
            Some(_) => return Err(HostExecError::new(INVALID, "Invalid layr state ID").into()),
        };
        // The git tree ID of that state; a guest whose layr cannot name it sends none.
        let layr_tree = match params.get("layrTree") {
            None | Some(Value::Null) => None,
            Some(Value::String(tree)) if layr_state.is_some() && layr_id(tree, 40..=40) => {
                Some(tree.clone())
            }
            Some(_) => return Err(HostExecError::new(INVALID, "Invalid layr tree ID").into()),
        };
        let lock = self.lock_for(&format!("run\0{identity}"));
        let _held = lock.lock().await;
        let source = self.scan_slot_blocking(&slot, mac_only).await?;
        // The scan let other requests run: the operation must still be leased.
        let mut op = self.load(identity)?;
        require(
            text(&op, "state") == "leased",
            "The command already started",
            "outcome_unknown",
        )?;
        let slot = self.slot(&op)?;
        self.refuse_orphan(identity)?;
        require(
            protocol::conflicts(source.keys()).is_empty(),
            "The source names conflict on the host filesystem",
            "name_conflict",
        )?;
        require(
            params.get("manifestSha256").and_then(Value::as_str)
                == Some(protocol::manifest_digest(&source).as_str()),
            "The host sources differ from the layr slot",
            "sync_conflict",
        )?;
        if caches.iter().any(|cache| cache == ".git") {
            self.link_git_objects(&op, &slot.join("source"))?;
        }
        let mut cwd = slot.join("source");
        if studio_runtime_store::truthy(params.get("cwd")) {
            cwd = protocol::inside(&cwd, params.get("cwd"), true)?;
            require(cwd.is_dir(), "The command cwd is not a directory", INVALID)?;
        }
        self.evict(
            slot.parent().unwrap_or(&slot),
            self.budget_of(&op),
            0,
            Some(&slot),
        )?;
        op.insert("state".into(), json!("running"));
        op.insert("startedAt".into(), json!(now()));
        op.insert("caches".into(), json!(caches));
        if let Some(state) = layr_state {
            op.insert("layrState".into(), json!(state));
        }
        if let Some(tree) = layr_tree {
            op.insert("layrTree".into(), json!(tree));
        }
        self.save(identity, &op)?;
        self.inner().commands.insert(identity.to_owned());
        let service = Arc::clone(self);
        let (task_identity, task_op) = (identity.to_owned(), op.clone());
        tokio::spawn(async move {
            service
                .command(task_identity, task_op, argv, cwd, timeout, source)
                .await
        });
        Ok(op)
    }

    async fn command(
        self: Arc<Self>,
        identity: String,
        mut op: Op,
        argv: Vec<String>,
        cwd: PathBuf,
        timeout: f64,
        before: Manifest,
    ) {
        if let Err(error) = self
            .command_steps(&identity, &mut op, &argv, &cwd, timeout, before)
            .await
        {
            let child = self.inner().children.get(&identity).cloned();
            if let Some(child) = child {
                terminate(&child).await;
            }
            let message = match error {
                Failure::Refused(error) => error.message,
                Failure::Unexpected(message) => message,
            };
            op.insert("state".into(), json!("unknown"));
            op.insert("reason".into(), json!("host_command_failed"));
            op.insert(
                "error".into(),
                json!(message.chars().take(super::ERROR_CHARS).collect::<String>()),
            );
            if let Err(error) = self.save(&identity, &op) {
                eprintln!("host_exec: the status of {identity} is not saved: {error:?}");
            }
        }
        let mut inner = self.inner();
        inner.children.remove(&identity);
        inner.commands.remove(&identity);
        #[cfg(test)]
        self.command_finished.notify_one();
    }

    #[allow(clippy::too_many_lines, reason = "one ordered command lifecycle")]
    async fn command_steps(
        self: &Arc<Self>,
        identity: &str,
        op: &mut Op,
        argv: &[String],
        cwd: &Path,
        timeout: f64,
        mut before: Manifest,
    ) -> Outcome<()> {
        let reason: Reason = Arc::new(Mutex::new("exited"));
        let mut code = Value::Null;
        let slot = self.slot(op)?;
        if studio_runtime_store::truthy(self.load(identity)?.get("cancelRequested")) {
            set_reason(&reason, "cancelled");
        } else {
            let mut command = tokio::process::Command::new(&argv[0]);
            command
                .args(&argv[1..])
                .current_dir(cwd)
                .env(ENV_DERIVED_DATA, text(op, "derivedData"))
                .env(ENV_PACKAGE_CACHE, text(op, "packageCache"))
                .env(ENV_ARTIFACTS, text(op, "artifactsPath"))
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .process_group(0);
            if let Some(state) = op
                .get("layrState")
                .and_then(Value::as_str)
                .filter(|state| !state.is_empty())
            {
                command.env(ENV_STATE, state);
            }
            // A sign-in link of the command reaches the host bridge as its calling chat. The
            // inherited environment must not name another chat's session.
            let launcher = self
                .browser_host
                .as_ref()
                .map(|folder| folder.join(crate::browser_environment::BROWSER_LAUNCHER))
                .filter(|launcher| launcher.is_file());
            match (&launcher, &self.browser_host) {
                (Some(launcher), Some(folder)) => {
                    command
                        .env("BROWSER", launcher)
                        .env(
                            "STUDIO_BROWSER_SOCKET",
                            folder.join(crate::browser_environment::BROWSER_OPEN_SOCKET),
                        )
                        .env("STUDIO_BROWSER_SESSION", text(op, "agentId"))
                        // The launcher prefers a guest session name.
                        .env_remove("LAYR_SESSION");
                }
                _ => {
                    command
                        .env_remove("BROWSER")
                        .env_remove("STUDIO_BROWSER_SOCKET")
                        .env_remove("STUDIO_BROWSER_SESSION");
                }
            }
            match op
                .get("layrTree")
                .and_then(Value::as_str)
                .filter(|tree| !tree.is_empty())
            {
                Some(tree) => command.env(ENV_TREE, tree),
                // The inherited environment must not name another tree.
                None => command.env_remove(ENV_TREE),
            };
            let mut child = command.spawn()?;
            let pid = child
                .id()
                .and_then(|pid| i32::try_from(pid).ok())
                .ok_or_else(|| Failure::Unexpected("The command has no process ID".into()))?;
            let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
            let (exited_sender, exited) = tokio::sync::watch::channel(false);
            let (status_sender, status) = tokio::sync::oneshot::channel();
            let handle = ChildHandle {
                pid,
                exited,
                reaped: Arc::new(Mutex::new(false)),
            };
            let reaper = handle.clone();
            tokio::spawn(async move {
                let result = reap_command(child, reaper).await;
                drop(status_sender.send(result));
                exited_sender.send_replace(true);
            });
            self.inner()
                .children
                .insert(identity.to_owned(), handle.clone());
            let saved = self.load(identity)?;
            op.extend(saved);
            op.insert("pid".into(), json!(pid));
            self.save(identity, op)?;
            if studio_runtime_store::truthy(op.get("cancelRequested")) {
                terminate(&handle).await;
            }
            let output = Arc::new(AtomicU64::new(0));
            let mut readers = tokio::task::JoinSet::new();
            for (stream, pipe) in [
                ("stdout", stdout.map(Pipe::Out)),
                ("stderr", stderr.map(Pipe::Err)),
            ] {
                let Some(pipe) = pipe else {
                    continue;
                };
                let drain = Drain {
                    service: Arc::clone(self),
                    identity: identity.to_owned(),
                    stream,
                    slot_path: text(op, "slotPath").to_owned(),
                    line_path: text(op, "linePath").to_owned(),
                    output: Arc::clone(&output),
                    reason: Arc::clone(&reason),
                    child: handle.clone(),
                };
                readers.spawn(drain.run(pipe));
            }
            let watchdog = tokio::spawn(budget_watch(
                Arc::clone(self),
                slot.clone(),
                self.budget_of(op),
                handle.clone(),
                Arc::clone(&reason),
            ));
            let mut exited = handle.exited.clone();
            if tokio::time::timeout(
                Duration::from_secs_f64(timeout),
                exited.wait_for(|done| *done),
            )
            .await
            .is_err()
            {
                set_reason(&reason, "timeout");
                terminate(&handle).await;
            }
            watchdog.abort();
            drop(watchdog.await);
            let drained = tokio::time::timeout(Duration::from_secs(READERS_SECONDS), async {
                let mut result = Ok(());
                while let Some(joined) = readers.join_next().await {
                    let finished = joined
                        .map_err(|error| Failure::Unexpected(error.to_string()))
                        .and_then(|done| done);
                    if result.is_ok() {
                        result = finished;
                    }
                }
                result
            })
            .await
            .map_err(|_| Failure::Unexpected("The command output did not end".into()))?;
            drained?;
            let status = status
                .await
                .map_err(|_| Failure::Unexpected("The command exit is unknown".into()))??;
            use std::os::unix::process::ExitStatusExt;
            code = match (status.code(), status.signal()) {
                (Some(code), _) => json!(code),
                (None, Some(signal)) => json!(-signal),
                (None, None) => Value::Null,
            };
            if studio_runtime_store::truthy(self.load(identity)?.get("cancelRequested")) {
                set_reason(&reason, "cancelled");
            }
        }
        // Mac-only paths and cache folders never return to the line, so they are not compared.
        let caches = string_list(op, "caches");
        let mut skip = Self::mac_only(op);
        skip.extend(caches.iter().cloned());
        let after = self.scan_slot_blocking(&slot, skip).await?;
        before.retain(|path, _| !protocol::under(path, &caches));
        let mut changed = Map::new();
        for path in before.keys().chain(after.keys()) {
            if before.get(path) != after.get(path) {
                changed.insert(
                    path.clone(),
                    after.get(path).cloned().unwrap_or(Value::Null),
                );
            }
        }
        let result_dir = self.opdir(identity).join("changes");
        fs::create_dir(&result_dir)?;
        for (path, entry) in &changed {
            let kind = entry
                .get("kind")
                .and_then(Value::as_str)
                .unwrap_or("directory");
            if kind == "directory" {
                continue;
            }
            let name = protocol::text_value(path);
            let source = protocol::inside(&slot.join("source"), Some(&name), false)?;
            let target = protocol::inside(&result_dir, Some(&name), false)?;
            if let Some(parent) = target.parent() {
                fs::create_dir_all(parent)?;
            }
            if kind == "link" {
                std::os::unix::fs::symlink(text_of(entry, "target"), &target)?;
            } else {
                fs::copy(&source, &target)?;
                let mode = integer(entry.get("mode"))
                    .and_then(|mode| u32::try_from(mode).ok())
                    .unwrap_or(0o600);
                use std::os::unix::fs::PermissionsExt;
                fs::set_permissions(&target, fs::Permissions::from_mode(mode))?;
            }
        }
        let artifacts = protocol::scan(&self.opdir(identity).join("artifacts"), &[], None)?;
        protocol::atomic(
            &self.opdir(identity).join("changes.json"),
            &Value::Object(changed.clone()),
        )?;
        protocol::atomic(
            &self.opdir(identity).join("artifacts.json"),
            &protocol::manifest_value(&artifacts),
        )?;
        // A cancel or a guest recovery saved its flag during the command.
        let latest = self.load(identity)?;
        for field in ["cancelRequested", "orphaned"] {
            if let Some(value) = latest
                .get(field)
                .filter(|value| studio_runtime_store::truthy(Some(value)))
            {
                op.insert(field.into(), value.clone());
            }
        }
        let reason = get_reason(&reason);
        op.insert("state".into(), json!("exited"));
        op.insert("exitCode".into(), code.clone());
        op.insert("reason".into(), json!(reason));
        op.insert("exitedAt".into(), json!(now()));
        op.insert("changedPaths".into(), json!(changed.len()));
        op.insert("artifactFiles".into(), json!(artifacts.len()));
        op.insert(
            "nameConflicts".into(),
            json!(protocol::conflicts(after.keys())),
        );
        self.save(identity, op)?;
        let mut event = Map::new();
        event.insert("state".into(), json!("exited"));
        event.insert("exitCode".into(), code);
        event.insert("reason".into(), json!(reason));
        self.event(identity, event)?;
        if studio_runtime_store::truthy(op.get("orphaned"))
            && let Err(error) =
                self.release_operation(identity, op, &slot, false, false, Some("guest_owner_lost"))
        {
            eprintln!("host_exec: the slot of {identity} stays leased: {error:?}");
        }
        Ok(())
    }

    pub(crate) async fn cancel(&self, identity: &str, op: &mut Op) -> Outcome<Op> {
        let child = self.inner().children.get(identity).cloned();
        if let Some(child) = child {
            op.insert("cancelRequested".into(), json!(true));
            self.save(identity, op)?;
            terminate(&child).await;
        } else if text(op, "state") == "unknown" {
            return Err(HostExecError::new(
                "outcome_unknown",
                "The old host owner is lost; inspect the slot before recovery",
            )
            .into());
        } else {
            op.insert("cancelRequested".into(), json!(true));
            self.save(identity, op)?;
        }
        self.load(identity)
    }
}

fn text_of<'a>(entry: &'a Value, field: &str) -> &'a str {
    entry.get(field).and_then(Value::as_str).unwrap_or_default()
}

enum Pipe {
    Out(tokio::process::ChildStdout),
    Err(tokio::process::ChildStderr),
}

impl tokio::io::AsyncRead for Pipe {
    fn poll_read(
        self: std::pin::Pin<&mut Self>,
        context: &mut std::task::Context<'_>,
        buffer: &mut tokio::io::ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        match self.get_mut() {
            Pipe::Out(pipe) => std::pin::Pin::new(pipe).poll_read(context, buffer),
            Pipe::Err(pipe) => std::pin::Pin::new(pipe).poll_read(context, buffer),
        }
    }
}

/// Stop the command when its project exceeds the disk budget and no idle slot
/// can make room. The walk runs off the request loop, and the pause grows with
/// it, so that the walk takes at most a fifth of the time.
async fn budget_watch(
    service: Arc<HostService>,
    slot: PathBuf,
    budget: u64,
    child: ChildHandle,
    reason: Reason,
) {
    let project = slot
        .parent()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| slot.clone());
    while !*child.exited.borrow() {
        let started = std::time::Instant::now();
        let walked = project.clone();
        let Ok(used) = studio_runtime_store::blocking_pool::spawn_blocking(
            "runtime-services/host-exec-budget",
            move || HostService::size(&walked),
        )
        .await
        else {
            return;
        };
        service.inner().usage.insert(project.clone(), used);
        if used > budget {
            match service.evict(&project, budget, 0, Some(&slot)) {
                Ok(_) => {}
                Err(Failure::Refused(_)) => {
                    set_reason(&reason, "disk_budget");
                    terminate(&child).await;
                    return;
                }
                // A failed walk or removal ends the watch; the command keeps running.
                Err(Failure::Unexpected(error)) => {
                    eprintln!("host_exec: the disk budget watch stopped: {error}");
                    return;
                }
            }
        }
        let pause =
            (BUDGET_PAUSE_FACTOR * started.elapsed().as_secs_f64()).max(BUDGET_PAUSE_SECONDS);
        tokio::time::sleep(Duration::from_secs_f64(pause)).await;
    }
}
