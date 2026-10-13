//! Git-backed checkpoint capture and preview for local workspaces.

use std::fs::{self, File, FileTimes, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};
use studio_runtime_store::{
    CheckpointsRepository, RuntimeStore, StoreError, StoredCheckpointObject, WorkspaceSnapshot,
};
use thiserror::Error;

const GIT_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_PATCH_CHARS: usize = 300_000;
const MAX_PATCH_BYTES: usize = MAX_PATCH_CHARS * 4 + 4;
const ACTIVE_OPERATION_PHASES: &[&str] = &[
    "capture_pending",
    "capture_running",
    "local_mutation",
    "provider_pending",
    "provider_ready",
    "recovery_required",
];

#[derive(Debug, Error)]
pub enum CheckpointsError {
    #[error("Unknown managed agent")]
    UnknownAgent,
    #[error("Unknown checkpoint")]
    UnknownCheckpoint,
    #[error("Checkpoint belongs to another agent")]
    WrongAgent,
    #[error("This agent was deleted")]
    DeletedAgent,
    #[error("Workspace checkpoint state is not initialized")]
    MissingSchema,
    #[error("Studio Git checkpoints are unavailable for image workspaces")]
    ExecutionWorkspace,
    #[error("Workspace activity changed before checkpoint capture")]
    ActivityChanged,
    #[error(
        "Checkpoint capture failed ({capture_error}); reservation settlement failed ({settlement_error})"
    )]
    SettlementFailed {
        capture_error: String,
        settlement_error: String,
    },
    #[error("Workspace recovery is required before using this workspace")]
    RecoveryRequired,
    #[error("An agent is using this workspace")]
    AgentBusy,
    #[error("A monitor is using this workspace")]
    MonitorBusy,
    #[error("A command or tool is still active")]
    TaskBusy,
    #[error("Git operation failed")]
    GitFailed,
    #[error("Git operation timed out")]
    GitTimeout,
    #[error("The workspace cannot be accessed")]
    WorkspaceUnavailable,
    #[error("Invalid stored object {object}/{id}")]
    InvalidStoredObject { object: &'static str, id: String },
    #[error(transparent)]
    Store(#[from] StoreError),
}

#[derive(Clone, Debug, PartialEq)]
pub struct PreviewContext {
    pub agent: StoredCheckpointObject,
    pub checkpoint: StoredCheckpointObject,
    pub recovery_expected_tree: Option<String>,
}

pub fn capture_context(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<WorkspaceSnapshot, CheckpointsError> {
    let connection = store.read()?;
    if !CheckpointsRepository::tables_available(&connection)? {
        return Err(CheckpointsError::MissingSchema);
    }
    CheckpointsRepository::workspace_snapshot(&connection, agent_id)?
        .ok_or(CheckpointsError::UnknownAgent)
}

pub fn preview_context(
    store: &RuntimeStore,
    agent_id: &str,
    checkpoint_id: &str,
) -> Result<PreviewContext, CheckpointsError> {
    let connection = store.read()?;
    if !CheckpointsRepository::checkpoint_table_available(&connection)? {
        return Err(CheckpointsError::MissingSchema);
    }
    let agent = CheckpointsRepository::agent(&connection, agent_id)?
        .ok_or(CheckpointsError::UnknownAgent)?;
    let checkpoint = CheckpointsRepository::checkpoint(&connection, checkpoint_id)?
        .ok_or(CheckpointsError::UnknownCheckpoint)?;
    if let Some(owner) = checkpoint.record.get("agent").and_then(Value::as_str)
        && owner != agent_id
    {
        return Err(CheckpointsError::WrongAgent);
    }
    let recovery_expected_tree =
        CheckpointsRepository::restore_expected_tree(&connection, agent_id, checkpoint_id)?;
    Ok(PreviewContext {
        agent,
        checkpoint,
        recovery_expected_tree,
    })
}

pub fn capture(
    store: &RuntimeStore,
    snapshot: WorkspaceSnapshot,
    label: &str,
) -> Result<Value, CheckpointsError> {
    ensure_agent_available(&snapshot.agent)?;
    ensure_workspace_idle(&snapshot, None)?;
    let checkpoint_id = new_uuid()?;
    let operation_id = format!("capture:{checkpoint_id}");
    let now = now_seconds();
    store.transaction_with(|transaction| {
        if !CheckpointsRepository::reserve_capture(transaction, &snapshot, &operation_id, now)? {
            return Err(CheckpointsError::ActivityChanged);
        }
        Ok::<_, CheckpointsError>(())
    })?;

    let result = capture_reserved(store, &snapshot, label, &checkpoint_id, &operation_id);
    match result {
        Ok(checkpoint) => Ok(checkpoint_summary(checkpoint)),
        Err(error) => {
            let message = error.to_string();
            let settlement = store.transaction_with(|transaction| {
                CheckpointsRepository::settle_capture(
                    transaction,
                    &operation_id,
                    Some(&message),
                    now_seconds(),
                )?;
                Ok::<_, CheckpointsError>(())
            });
            match settlement {
                Ok(()) => Err(error),
                Err(settlement_error) => Err(CheckpointsError::SettlementFailed {
                    capture_error: error.to_string(),
                    settlement_error: settlement_error.to_string(),
                }),
            }
        }
    }
}

fn capture_reserved(
    store: &RuntimeStore,
    snapshot: &WorkspaceSnapshot,
    label: &str,
    checkpoint_id: &str,
    operation_id: &str,
) -> Result<Value, CheckpointsError> {
    let snapshot = capture_context(store, &snapshot.agent.id)?;
    if snapshot
        .agent
        .record
        .get("workspaceReservationId")
        .and_then(Value::as_str)
        != Some(operation_id)
        || snapshot
            .agent
            .record
            .get("workspaceOperation")
            .and_then(Value::as_str)
            != Some("capture")
    {
        return Err(CheckpointsError::ActivityChanged);
    }
    ensure_workspace_idle(&snapshot, Some(operation_id))?;
    let started = store.transaction_with(|transaction| {
        CheckpointsRepository::set_capture_running(
            transaction,
            &snapshot,
            operation_id,
            now_seconds(),
        )
        .map_err(CheckpointsError::from)
    })?;
    if !started {
        return Err(CheckpointsError::ActivityChanged);
    }
    let agent_id = &snapshot.agent.id;
    let cwd = agent_string(&snapshot, "cwd")?;
    let tree = snapshot_tree(store, cwd)?;
    let commit_output = git(
        cwd,
        &["commit-tree", tree.as_str(), "-p", "HEAD"],
        &[
            ("GIT_AUTHOR_NAME", "Codex Agents"),
            ("GIT_AUTHOR_EMAIL", "local@codex-agents.invalid"),
            ("GIT_COMMITTER_NAME", "Codex Agents"),
            ("GIT_COMMITTER_EMAIL", "local@codex-agents.invalid"),
        ],
        Some(b"Codex Agents workspace checkpoint\n"),
    )?;
    let commit = String::from_utf8_lossy(&commit_output).trim().to_owned();
    let reference = format!("refs/codex-agents/checkpoints/{agent_id}/{checkpoint_id}");
    git(cwd, &["update-ref", &reference, &commit], &[], None)?;

    let parent_id = snapshot
        .agent
        .record
        .get("checkpointHistoryHead")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let parent_boundary = snapshot
        .parent_checkpoint
        .as_ref()
        .and_then(|parent| parent.record.get("historyBoundary"))
        .and_then(Value::as_i64);
    store.transaction_with(|transaction| {
        let (history_delta, history_boundary) =
            CheckpointsRepository::history_delta(transaction, agent_id, parent_boundary)?;
        let record = json!({
            "historyParent": parent_id,
            "historyDelta": history_delta,
            "historyBoundary": history_boundary,
            "id": checkpoint_id,
            "agent": agent_id,
            "rootId": snapshot.agent.record.get("rootId"),
            "label": label,
            "tree": tree,
            "commit": commit,
            "ref": reference,
            "threadId": snapshot.agent.record.get("threadId"),
            "turnId": snapshot.agent.record.get("lastCompletedTurn"),
            "created": now_seconds(),
            "cwd": cwd,
        });
        if !CheckpointsRepository::complete_capture(
            transaction,
            agent_id,
            operation_id,
            parent_id.as_deref(),
            &record,
            now_seconds(),
        )? {
            return Err(CheckpointsError::ActivityChanged);
        }
        Ok::<_, CheckpointsError>(record)
    })
}

pub fn preview(store: &RuntimeStore, context: PreviewContext) -> Result<Value, CheckpointsError> {
    ensure_agent_available(&context.agent)?;
    let cwd = context
        .agent
        .record
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or_else(|| invalid_agent(&context.agent.id))?;
    let checkpoint = &context.checkpoint.record;
    let tree = checkpoint
        .get("tree")
        .and_then(Value::as_str)
        .ok_or_else(|| invalid_checkpoint(&context.checkpoint.id))?;
    if !is_full_object_id(tree) {
        return Err(invalid_checkpoint(&context.checkpoint.id));
    }
    let current_tree = snapshot_tree(store, cwd)?;
    let expected_tree = context
        .recovery_expected_tree
        .as_deref()
        .unwrap_or(&current_tree);
    if !is_full_object_id(expected_tree) {
        return Err(invalid_checkpoint(&context.checkpoint.id));
    }
    let (bytes, output_truncated) = git_limited(
        cwd,
        &[
            "diff",
            "--no-ext-diff",
            "--no-color",
            "--end-of-options",
            expected_tree,
            tree,
            "--",
        ],
        &[],
        None,
        MAX_PATCH_BYTES,
    )?;
    let raw_patch = String::from_utf8_lossy(&bytes).into_owned();
    let truncated = output_truncated || raw_patch.chars().count() > MAX_PATCH_CHARS;
    let patch = raw_patch.chars().take(MAX_PATCH_CHARS).collect::<String>();
    let can_restore = false;
    let mut summary = checkpoint.clone();
    remove_summary_payload(&mut summary);
    Ok(json!({
        "checkpoint": summary,
        "expectedTree": expected_tree,
        "diff": patch,
        "patch": patch,
        "truncated": truncated,
        "canRestore": can_restore,
    }))
}

fn ensure_agent_available(agent: &StoredCheckpointObject) -> Result<(), CheckpointsError> {
    if agent.record.get("id").and_then(Value::as_str) != Some(agent.id.as_str()) {
        return Err(invalid_agent(&agent.id));
    }
    if truthy(agent.record.get("deletedAt")) {
        return Err(CheckpointsError::DeletedAgent);
    }
    if agent.record.get("imageWorkspace").and_then(Value::as_bool) == Some(true)
        || agent.record.get("executionMode").and_then(Value::as_str) == Some("vm")
        || matches!(
            agent.record.get("workspaceMode").and_then(Value::as_str),
            Some("image" | "layr")
        )
    {
        return Err(CheckpointsError::ExecutionWorkspace);
    }
    Ok(())
}

fn ensure_workspace_idle(
    snapshot: &WorkspaceSnapshot,
    reservation_id: Option<&str>,
) -> Result<(), CheckpointsError> {
    let cwd = agent_string(snapshot, "cwd")?;
    let target = Path::new(cwd)
        .canonicalize()
        .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
    let agents = snapshot
        .agents
        .iter()
        .map(|agent| {
            let record = agent
                .record
                .as_object()
                .ok_or_else(|| invalid_agent(&agent.id))?;
            if record.get("id").and_then(Value::as_str) != Some(agent.id.as_str()) {
                return Err(invalid_agent(&agent.id));
            }
            Ok((agent.id.as_str(), record))
        })
        .collect::<Result<Vec<_>, CheckpointsError>>()?;
    let by_id = agents
        .iter()
        .map(|(id, record)| (*id, *record))
        .collect::<std::collections::BTreeMap<_, _>>();
    for operation in &snapshot.operations {
        validate_internal_record(operation, "workspace_operations")?;
        if reservation_id == Some(operation.id.as_str()) {
            continue;
        }
        if !operation
            .record
            .get("phase")
            .and_then(Value::as_str)
            .is_some_and(|phase| ACTIVE_OPERATION_PHASES.contains(&phase))
        {
            continue;
        }
        let Some(operation_agent) = operation.record.get("agent").and_then(Value::as_str) else {
            continue;
        };
        if let Some(record) = by_id.get(operation_agent)
            && canonical_cwd(record).is_ok_and(|path| path == target)
        {
            return Err(CheckpointsError::RecoveryRequired);
        }
    }
    for (id, record) in &agents {
        let active = truthy(record.get("inFlight")) || truthy(record.get("workspaceOperation"));
        let own_reservation = reservation_id.is_some_and(|reservation| {
            *id == snapshot.agent.id
                && record.get("workspaceReservationId").and_then(Value::as_str) == Some(reservation)
                && record.get("workspaceOperation").and_then(Value::as_str) == Some("capture")
        });
        if active
            && canonical_cwd(record).is_ok_and(|path| path == target)
            && (truthy(record.get("inFlight")) || !own_reservation)
        {
            return Err(CheckpointsError::AgentBusy);
        }
    }
    for monitor in &snapshot.monitors {
        validate_internal_record(monitor, "monitors")?;
        if matches!(
            monitor.record.get("status").and_then(Value::as_str),
            Some("running" | "starting" | "approval")
        ) {
            let path = monitor
                .record
                .get("cwd")
                .and_then(Value::as_str)
                .ok_or_else(|| CheckpointsError::InvalidStoredObject {
                    object: "monitors",
                    id: monitor.id.clone(),
                })?;
            if Path::new(path)
                .canonicalize()
                .is_ok_and(|path| path == target)
            {
                return Err(CheckpointsError::MonitorBusy);
            }
        }
    }
    for task in &snapshot.tasks {
        validate_internal_record(task, "tasks")?;
        if task.record.get("status").and_then(Value::as_str) == Some("running")
            && task
                .record
                .get("agent")
                .and_then(Value::as_str)
                .and_then(|id| by_id.get(id))
                .is_some_and(|record| canonical_cwd(record).is_ok_and(|path| path == target))
        {
            return Err(CheckpointsError::TaskBusy);
        }
    }
    Ok(())
}

fn validate_internal_record(
    record: &StoredCheckpointObject,
    object: &'static str,
) -> Result<(), CheckpointsError> {
    let Some(value) = record.record.as_object() else {
        return Err(CheckpointsError::InvalidStoredObject {
            object,
            id: record.id.clone(),
        });
    };
    if value
        .get("id")
        .and_then(Value::as_str)
        .is_some_and(|id| id != record.id)
    {
        return Err(CheckpointsError::InvalidStoredObject {
            object,
            id: record.id.clone(),
        });
    }
    Ok(())
}

fn canonical_cwd(record: &serde_json::Map<String, Value>) -> Result<PathBuf, CheckpointsError> {
    let path = record
        .get("cwd")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .ok_or(CheckpointsError::WorkspaceUnavailable)?;
    Path::new(path)
        .canonicalize()
        .map_err(|_| CheckpointsError::WorkspaceUnavailable)
}

fn snapshot_tree(store: &RuntimeStore, cwd: &str) -> Result<String, CheckpointsError> {
    snapshot_tree_at(cwd, store.root())
}

fn snapshot_tree_at(cwd: &str, root: &Path) -> Result<String, CheckpointsError> {
    let directory = root.join(format!("checkpoint-{}", new_uuid()?));
    fs::create_dir(&directory).map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
    let index = directory.join("index");
    let temporary = TemporaryDirectory(directory);
    let raw_path = git(cwd, &["rev-parse", "--git-path", "index"], &[], None)?;
    let git_index = PathBuf::from(String::from_utf8_lossy(&raw_path).trim());
    let source = if git_index.is_absolute() {
        git_index
    } else {
        Path::new(cwd).join(git_index)
    };
    let index_value = index.to_string_lossy().into_owned();
    if source.is_file() {
        let source_file =
            File::open(&source).map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
        let metadata = source_file
            .metadata()
            .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
        let mut target = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&index)
            .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
        let mut source_file = source_file;
        std::io::copy(&mut source_file, &mut target)
            .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
        let mut times = FileTimes::new();
        if let Ok(accessed) = metadata.accessed() {
            times = times.set_accessed(accessed);
        }
        if let Ok(modified) = metadata.modified() {
            times = times.set_modified(modified);
        }
        target
            .set_times(times)
            .map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
    } else {
        git(
            cwd,
            &["read-tree", "HEAD"],
            &[("GIT_INDEX_FILE", index_value.as_str())],
            None,
        )?;
    }
    git(
        cwd,
        &["add", "-A", "--", "."],
        &[("GIT_INDEX_FILE", index_value.as_str())],
        None,
    )?;
    let tree = git(
        cwd,
        &["write-tree"],
        &[("GIT_INDEX_FILE", index_value.as_str())],
        None,
    )?;
    drop(temporary);
    Ok(String::from_utf8_lossy(&tree).trim().to_owned())
}

struct TemporaryDirectory(PathBuf);

impl Drop for TemporaryDirectory {
    fn drop(&mut self) {
        drop(fs::remove_dir_all(&self.0));
    }
}

fn git(
    cwd: &str,
    args: &[&str],
    environment: &[(&str, &str)],
    input: Option<&[u8]>,
) -> Result<Vec<u8>, CheckpointsError> {
    git_with_output_limit(cwd, args, environment, input, None).map(|(stdout, _)| stdout)
}

fn git_limited(
    cwd: &str,
    args: &[&str],
    environment: &[(&str, &str)],
    input: Option<&[u8]>,
    max_output_bytes: usize,
) -> Result<(Vec<u8>, bool), CheckpointsError> {
    git_with_output_limit(cwd, args, environment, input, Some(max_output_bytes))
}

fn git_with_output_limit(
    cwd: &str,
    args: &[&str],
    environment: &[(&str, &str)],
    input: Option<&[u8]>,
    max_output_bytes: Option<usize>,
) -> Result<(Vec<u8>, bool), CheckpointsError> {
    git_with_output_limit_and_timeout(
        "git",
        cwd,
        args,
        environment,
        input,
        max_output_bytes,
        GIT_TIMEOUT,
    )
}

fn git_with_output_limit_and_timeout(
    program: &str,
    cwd: &str,
    args: &[&str],
    environment: &[(&str, &str)],
    input: Option<&[u8]>,
    max_output_bytes: Option<usize>,
    timeout: Duration,
) -> Result<(Vec<u8>, bool), CheckpointsError> {
    let mut command = Command::new(program);
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .envs(environment.iter().copied())
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        command.process_group(0);
    }
    let mut child = command.spawn().map_err(|_| CheckpointsError::GitFailed)?;
    let stdout = child.stdout.take().ok_or(CheckpointsError::GitFailed)?;
    let stderr = child.stderr.take().ok_or(CheckpointsError::GitFailed)?;
    let stdout_reader = thread::spawn(move || {
        let mut bytes = Vec::new();
        let mut truncated = false;
        let mut buffer = [0; 8192];
        let mut stdout = stdout;
        loop {
            let count = stdout.read(&mut buffer)?;
            if count == 0 {
                break;
            }
            let retained = max_output_bytes
                .map_or(count, |limit| count.min(limit.saturating_sub(bytes.len())));
            bytes.extend_from_slice(&buffer[..retained]);
            truncated |= retained < count;
        }
        Ok::<_, std::io::Error>((bytes, truncated))
    });
    let stderr_reader = thread::spawn(move || {
        let mut buffer = [0; 8192];
        let mut stderr = stderr;
        loop {
            if stderr.read(&mut buffer)? == 0 {
                break;
            }
        }
        Ok::<_, std::io::Error>(())
    });
    if let Some(input) = input
        && let Some(mut stdin) = child.stdin.take()
        && stdin.write_all(input).is_err()
    {
        terminate_git_process_tree(&mut child);
        drop(child.wait());
        join_git_output_readers(stdout_reader, stderr_reader);
        return Err(CheckpointsError::GitFailed);
    }
    let started = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) if started.elapsed() < timeout => {
                thread::sleep(Duration::from_millis(20));
            }
            Ok(None) => {
                terminate_git_process_tree(&mut child);
                drop(child.wait());
                join_git_output_readers(stdout_reader, stderr_reader);
                return Err(CheckpointsError::GitTimeout);
            }
            Err(_) => {
                terminate_git_process_tree(&mut child);
                drop(child.wait());
                join_git_output_readers(stdout_reader, stderr_reader);
                return Err(CheckpointsError::GitFailed);
            }
        }
    };
    terminate_git_process_tree(&mut child);
    let (stdout, truncated) = stdout_reader
        .join()
        .map_err(|_| CheckpointsError::GitFailed)?
        .map_err(|_| CheckpointsError::GitFailed)?;
    stderr_reader
        .join()
        .map_err(|_| CheckpointsError::GitFailed)?
        .map_err(|_| CheckpointsError::GitFailed)?;
    if started.elapsed() >= timeout {
        return Err(CheckpointsError::GitTimeout);
    }
    if status.success() {
        Ok((stdout, truncated))
    } else {
        Err(CheckpointsError::GitFailed)
    }
}

fn terminate_git_process_tree(child: &mut Child) {
    #[cfg(any(unix, windows))]
    let process_id = child.id().to_string();
    #[cfg(unix)]
    {
        let group_id = format!("-{process_id}");
        drop(
            Command::new("/bin/kill")
                .args(["-KILL", group_id.as_str()])
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .status(),
        );
    }
    #[cfg(windows)]
    {
        if let Some(system_root) = std::env::var_os("SystemRoot") {
            let taskkill = Path::new(&system_root)
                .join("System32")
                .join("taskkill.exe");
            drop(
                Command::new(taskkill)
                    .args(["/PID", process_id.as_str(), "/T", "/F"])
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status(),
            );
        }
    }
    drop(child.kill());
}

fn join_git_output_readers(
    stdout_reader: thread::JoinHandle<Result<(Vec<u8>, bool), std::io::Error>>,
    stderr_reader: thread::JoinHandle<Result<(), std::io::Error>>,
) {
    drop(stdout_reader.join());
    drop(stderr_reader.join());
}

fn is_full_object_id(value: &str) -> bool {
    matches!(value.len(), 40 | 64) && value.bytes().all(|byte| byte.is_ascii_hexdigit())
}

fn checkpoint_summary(mut checkpoint: Value) -> Value {
    remove_summary_payload(&mut checkpoint);
    checkpoint
}

fn remove_summary_payload(checkpoint: &mut Value) {
    if let Some(object) = checkpoint.as_object_mut() {
        object.remove("items");
        object.remove("historyDelta");
        object.remove("_payloadBlobs");
    }
}

fn agent_string<'a>(
    snapshot: &'a WorkspaceSnapshot,
    name: &str,
) -> Result<&'a str, CheckpointsError> {
    snapshot
        .agent
        .record
        .get(name)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_agent(&snapshot.agent.id))
}

fn invalid_agent(id: &str) -> CheckpointsError {
    CheckpointsError::InvalidStoredObject {
        object: "agents",
        id: id.to_owned(),
    }
}

fn invalid_checkpoint(id: &str) -> CheckpointsError {
    CheckpointsError::InvalidStoredObject {
        object: "checkpoints",
        id: id.to_owned(),
    }
}

fn truthy(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Null) => false,
        Some(Value::Bool(value)) => *value,
        Some(Value::Number(value)) => value.as_f64().is_some_and(|number| number != 0.0),
        Some(Value::String(value)) => !value.is_empty(),
        Some(Value::Array(value)) => !value.is_empty(),
        Some(Value::Object(value)) => !value.is_empty(),
    }
}

fn new_uuid() -> Result<String, CheckpointsError> {
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes).map_err(|_| CheckpointsError::WorkspaceUnavailable)?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(format!(
        "{:02x}{:02x}{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}-{:02x}{:02x}{:02x}{:02x}{:02x}{:02x}",
        bytes[0],
        bytes[1],
        bytes[2],
        bytes[3],
        bytes[4],
        bytes[5],
        bytes[6],
        bytes[7],
        bytes[8],
        bytes[9],
        bytes[10],
        bytes[11],
        bytes[12],
        bytes[13],
        bytes[14],
        bytes[15]
    ))
}

fn now_seconds() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::{CheckpointsError, ensure_workspace_idle};
    #[cfg(unix)]
    use super::{TemporaryDirectory, git_with_output_limit_and_timeout};
    use serde_json::json;
    #[cfg(unix)]
    use std::fs;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    #[cfg(unix)]
    use std::process::Command;
    #[cfg(unix)]
    use std::thread;
    #[cfg(unix)]
    use std::time::{Duration, Instant};
    use studio_runtime_store::{StoredCheckpointObject, WorkspaceSnapshot};

    #[test]
    fn capture_reservation_allows_its_owner_but_keeps_other_activity_blocked() -> std::io::Result<()>
    {
        let cwd = std::env::current_dir()?.to_string_lossy().into_owned();
        let agent = StoredCheckpointObject {
            id: "checkpoint-agent".to_owned(),
            record: json!({
                "id": "checkpoint-agent",
                "cwd": cwd,
                "inFlight": false,
                "workspaceOperation": "capture",
                "workspaceReservationId": "capture:checkpoint",
            }),
        };
        let operation = StoredCheckpointObject {
            id: "capture:checkpoint".to_owned(),
            record: json!({
                "id": "capture:checkpoint",
                "kind": "capture",
                "agent": "checkpoint-agent",
                "phase": "capture_pending",
            }),
        };
        let missing_cwd = std::env::temp_dir()
            .join(format!("codex-checkpoint-missing-{}", std::process::id()))
            .to_string_lossy()
            .into_owned();
        let unavailable_agent = StoredCheckpointObject {
            id: "unavailable-agent".to_owned(),
            record: json!({
                "id": "unavailable-agent",
                "cwd": missing_cwd,
                "workspaceOperation": "checkpoint",
                "inFlight": false,
            }),
        };
        let unavailable_operation = StoredCheckpointObject {
            id: "checkpoint:unavailable".to_owned(),
            record: json!({
                "id": "checkpoint:unavailable",
                "kind": "checkpoint",
                "agent": "unavailable-agent",
                "phase": "capture_running",
            }),
        };
        let snapshot = WorkspaceSnapshot {
            agent: agent.clone(),
            parent_checkpoint: None,
            agents: vec![agent, unavailable_agent],
            operations: vec![operation, unavailable_operation],
            monitors: Vec::new(),
            tasks: Vec::new(),
        };

        assert!(ensure_workspace_idle(&snapshot, Some("capture:checkpoint")).is_ok());
        assert!(matches!(
            ensure_workspace_idle(&snapshot, None),
            Err(CheckpointsError::RecoveryRequired)
        ));

        let mut busy_snapshot = snapshot;
        busy_snapshot.monitors.push(StoredCheckpointObject {
            id: "active-monitor".to_owned(),
            record: json!({
                "id": "active-monitor",
                "cwd": cwd,
                "status": "running",
            }),
        });
        assert!(matches!(
            ensure_workspace_idle(&busy_snapshot, Some("capture:checkpoint")),
            Err(CheckpointsError::MonitorBusy)
        ));
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn git_timeout_kills_descendants_and_drains_pipes() -> Result<(), Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!(
            "checkpoint-git-timeout-{}-{}",
            std::process::id(),
            super::new_uuid()?
        ));
        fs::create_dir(&directory)?;
        let _temporary = TemporaryDirectory(directory.clone());
        let child_pid_file = directory.join("child-pid");
        let program = directory.join("git-wrapper");
        fs::write(
            &program,
            "#!/bin/sh\n/bin/sleep 60 &\necho $! > \"$CHECKPOINT_TEST_CHILD_PID\"\nwait\n",
        )?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;

        let directory_string = directory.to_string_lossy().into_owned();
        let program_string = program.to_string_lossy().into_owned();
        let child_pid_path = child_pid_file.to_string_lossy().into_owned();
        let started = Instant::now();
        let result = git_with_output_limit_and_timeout(
            &program_string,
            &directory_string,
            &[],
            &[("CHECKPOINT_TEST_CHILD_PID", &child_pid_path)],
            None,
            None,
            Duration::from_millis(150),
        );
        assert!(matches!(result, Err(CheckpointsError::GitTimeout)));
        assert!(started.elapsed() < Duration::from_secs(3));

        assert_process_exits(&fs::read_to_string(child_pid_file)?)?;
        Ok(())
    }

    #[cfg(unix)]
    #[test]
    fn git_exit_kills_descendants_holding_output_pipes() -> Result<(), Box<dyn std::error::Error>> {
        let directory = std::env::temp_dir().join(format!(
            "checkpoint-git-exit-{}-{}",
            std::process::id(),
            super::new_uuid()?
        ));
        fs::create_dir(&directory)?;
        let _temporary = TemporaryDirectory(directory.clone());
        let child_pid_file = directory.join("child-pid");
        let program = directory.join("git-wrapper");
        fs::write(
            &program,
            "#!/bin/sh\n/bin/sleep 60 &\necho $! > \"$CHECKPOINT_TEST_CHILD_PID\"\nexit 0\n",
        )?;
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755))?;

        let directory_string = directory.to_string_lossy().into_owned();
        let program_string = program.to_string_lossy().into_owned();
        let child_pid_path = child_pid_file.to_string_lossy().into_owned();
        let started = Instant::now();
        let result = git_with_output_limit_and_timeout(
            &program_string,
            &directory_string,
            &[],
            &[("CHECKPOINT_TEST_CHILD_PID", &child_pid_path)],
            None,
            None,
            Duration::from_secs(2),
        );
        assert!(result.is_ok());
        assert!(started.elapsed() < Duration::from_secs(3));
        assert_process_exits(&fs::read_to_string(child_pid_file)?)?;
        Ok(())
    }

    #[cfg(unix)]
    fn assert_process_exits(pid: &str) -> Result<(), Box<dyn std::error::Error>> {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let status = Command::new("/bin/kill")
                .args(["-0", pid.trim()])
                .status()?;
            if !status.success() {
                return Ok(());
            }
            assert!(Instant::now() < deadline, "Git descendant remained alive");
            thread::sleep(Duration::from_millis(20));
        }
    }
}
