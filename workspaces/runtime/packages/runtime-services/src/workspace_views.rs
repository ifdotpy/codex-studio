use std::collections::BTreeMap;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use process_wrap::std::CommandWrap;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use serde::Deserialize;
use serde_json::{Value, json};

const MAX_SAFE_INTEGER: u64 = 9_007_199_254_740_991;

#[derive(Clone, Debug, PartialEq)]
pub struct TaskFeedCursor {
    pub id: String,
    pub time: f64,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CursorKind {
    Current,
    Before,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct InvalidCursor;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AgentScopeError {
    MissingAgent,
    MissingRoot,
}

#[derive(Default)]
pub struct WorkspaceViewData {
    pub work: Vec<Value>,
    pub annotations: Vec<Value>,
    pub checkpoints: Vec<Value>,
    pub plans: Vec<Value>,
    pub rules: Vec<Value>,
    pub inbox: Vec<Value>,
    pub tasks: Vec<Value>,
    pub monitors: Vec<Value>,
}

pub fn workspace_view_response(view: &str, data: WorkspaceViewData) -> Value {
    match view {
        "work" => json!({"work":data.work}),
        "inbox" => json!({"inbox":data.inbox}),
        "annotations" => json!({"annotations":data.annotations}),
        _ => json!({
            "work":data.work, "annotations":data.annotations, "checkpoints":data.checkpoints,
            "plans":data.plans, "rules":data.rules, "inbox":data.inbox, "tasks":data.tasks,
            "tasksHistoryLimit":100, "monitors":data.monitors,
        }),
    }
}

pub fn deleted_agent(agent: &Value) -> bool {
    agent
        .get("deletedAt")
        .is_some_and(|value| !value.is_null() && value != false && value != 0 && value != "")
}

pub fn agent_root(
    agents: &[Value],
    agent_id: Option<&str>,
) -> Result<Option<String>, AgentScopeError> {
    let Some(agent_id) = agent_id else {
        return Ok(None);
    };
    let agent = agents
        .iter()
        .find(|agent| agent["id"] == agent_id)
        .ok_or(AgentScopeError::MissingAgent)?;
    if deleted_agent(agent) {
        return Err(AgentScopeError::MissingAgent);
    }
    agent
        .get("rootId")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .map(Some)
        .ok_or(AgentScopeError::MissingRoot)
}

pub fn agents_in_root<'a>(agents: &'a [Value], root: Option<&str>) -> Vec<&'a Value> {
    agents
        .iter()
        .filter(|agent| !deleted_agent(agent) && root.is_none_or(|root| agent["rootId"] == root))
        .collect()
}

#[derive(Debug)]
pub enum GitError {
    Failed(String),
    TimedOut,
    Other,
    OutputTooLarge,
}

#[derive(Debug)]
struct PipeOutput {
    bytes: Vec<u8>,
    truncated: bool,
}

/// Owns the Git command sequence and workspace changes response projection.
pub fn git_changes(cwd: &str) -> Result<Value, GitError> {
    match run_git(cwd, &["rev-parse", "--show-toplevel"], 1024) {
        Ok(_) => {}
        Err(GitError::Failed(error)) => {
            return Ok(json!({"files": [], "patch": "", "git": false, "error": error}));
        }
        Err(error) => return Err(error),
    }
    let status = match run_git(cwd, &["status", "--porcelain=v1", "-z"], 16_000_000) {
        Ok(status) if !status.truncated => status,
        Ok(_) => return Err(GitError::OutputTooLarge),
        Err(GitError::Failed(error)) => return Ok(git_unavailable(error)),
        Err(error) => return Err(error),
    };
    let entries = String::from_utf8_lossy(&status.bytes)
        .split('\0')
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let mut files = Vec::new();
    let mut index = 0;
    while index < entries.len() {
        let entry = &entries[index];
        index += 1;
        if entry.is_empty() {
            continue;
        }
        let change = entry.get(..2).unwrap_or_default().trim();
        files.push(json!({"path":entry.get(3..).unwrap_or_default(),"status":change}));
        if change.contains('R') || change.contains('C') {
            index += 1;
        }
    }
    let patch_output = match run_git(
        cwd,
        &["diff", "HEAD", "--no-ext-diff", "--no-color", "--unified=3"],
        1_200_000,
    ) {
        Ok(patch) => patch,
        Err(GitError::Failed(error)) => return Ok(git_unavailable(error)),
        Err(error) => return Err(error),
    };
    let patch_text = String::from_utf8_lossy(&patch_output.bytes).into_owned();
    let truncated = patch_output.truncated || patch_text.chars().count() > 300_000;
    let patch = patch_text.chars().take(300_000).collect::<String>();
    let revision = match run_git(cwd, &["rev-parse", "HEAD"], 1024) {
        Ok(revision) if !revision.truncated => revision,
        Ok(_) => return Err(GitError::OutputTooLarge),
        Err(GitError::Failed(error)) => return Ok(git_unavailable(error)),
        Err(error) => return Err(error),
    };
    let revision = String::from_utf8_lossy(&revision.bytes).trim().to_owned();
    Ok(
        json!({"files":files,"diff":patch,"patch":patch,"truncated":truncated,"revision":revision,"git":true}),
    )
}

fn git_unavailable(error: String) -> Value {
    json!({"files": [], "patch": "", "git": false, "error": error})
}

fn run_git(cwd: &str, args: &[&str], stdout_limit: usize) -> Result<PipeOutput, GitError> {
    run_git_program("git", cwd, args, stdout_limit)
}

fn run_git_program(
    program: impl AsRef<std::ffi::OsStr>,
    cwd: &str,
    args: &[&str],
    stdout_limit: usize,
) -> Result<PipeOutput, GitError> {
    run_git_program_timeout(program, cwd, args, stdout_limit, Duration::from_secs(30))
}

fn run_git_program_timeout(
    program: impl AsRef<std::ffi::OsStr>,
    cwd: &str,
    args: &[&str],
    stdout_limit: usize,
    timeout: Duration,
) -> Result<PipeOutput, GitError> {
    let mut command = Command::new(program);
    command
        .arg("-C")
        .arg(cwd)
        .args(args)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut command = CommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    let deadline = Instant::now() + timeout;
    let mut child = command.spawn().map_err(|_| GitError::Other)?;
    let stdout = child.stdout().take().ok_or(GitError::Other)?;
    let stderr = child.stderr().take().ok_or(GitError::Other)?;
    let (stdout_sender, stdout_receiver) = mpsc::sync_channel(1);
    let (stderr_sender, stderr_receiver) = mpsc::sync_channel(1);
    let stdout_reader =
        thread::spawn(move || drop(stdout_sender.send(read_bounded(stdout, stdout_limit))));
    let stderr_reader = thread::spawn(move || drop(stderr_sender.send(read_bounded(stderr, 4800))));
    let status = loop {
        match child.try_wait() {
            Err(_) => {
                drop(child.kill());
                drop(stdout_reader.join());
                drop(stderr_reader.join());
                return Err(GitError::Other);
            }
            Ok(Some(status)) => break status,
            Ok(None) if Instant::now() >= deadline => {
                drop(child.kill());
                drop(stdout_reader.join());
                drop(stderr_reader.join());
                return Err(GitError::TimedOut);
            }
            Ok(None) => thread::sleep(Duration::from_millis(10)),
        }
    };
    let stdout = stdout_receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let stderr = stderr_receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    if matches!(stdout, Err(mpsc::RecvTimeoutError::Timeout))
        || matches!(stderr, Err(mpsc::RecvTimeoutError::Timeout))
    {
        drop(child.kill());
        drop(stdout_reader.join());
        drop(stderr_reader.join());
        return Err(GitError::TimedOut);
    }
    let stdout = stdout.ok().and_then(Result::ok).ok_or(GitError::Other)?;
    let stderr = stderr.ok().and_then(Result::ok).ok_or(GitError::Other)?;
    drop(stdout_reader.join());
    drop(stderr_reader.join());
    if status.success() {
        return Ok(stdout);
    }
    let message = String::from_utf8_lossy(&stderr.bytes)
        .trim()
        .chars()
        .take(1200)
        .collect::<String>();
    Err(GitError::Failed(if message.is_empty() {
        "Git operation failed".to_owned()
    } else {
        message
    }))
}

fn read_bounded(mut pipe: impl Read, limit: usize) -> std::io::Result<PipeOutput> {
    let mut bytes = Vec::with_capacity(limit.min(16_384));
    let mut chunk = [0_u8; 8192];
    let mut truncated = false;
    loop {
        let count = pipe.read(&mut chunk)?;
        if count == 0 {
            break;
        }
        let retained = count.min(limit.saturating_sub(bytes.len()));
        bytes.extend_from_slice(&chunk[..retained]);
        truncated |= retained < count;
    }
    Ok(PipeOutput { bytes, truncated })
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct TaskFeedCursorInput {
    id: String,
    #[serde(default, deserialize_with = "deserialize_cursor_number")]
    updated: Option<CursorNumber>,
    #[serde(default, deserialize_with = "deserialize_cursor_number")]
    created: Option<CursorNumber>,
}

#[derive(Debug, Deserialize)]
#[serde(try_from = "serde_json::Number")]
struct CursorNumber(f64);

impl TryFrom<serde_json::Number> for CursorNumber {
    type Error = &'static str;

    fn try_from(value: serde_json::Number) -> Result<Self, Self::Error> {
        if value
            .as_i64()
            .is_some_and(|integer| integer < 0 || integer as u64 > MAX_SAFE_INTEGER)
            || value
                .as_u64()
                .is_some_and(|integer| integer > MAX_SAFE_INTEGER)
        {
            return Err("integer is outside the safe range");
        }
        let number = value.as_f64().ok_or("invalid cursor number")?;
        number
            .is_finite()
            .then_some(Self(number))
            .ok_or("number is not finite")
    }
}

fn deserialize_cursor_number<'de, D>(deserializer: D) -> Result<Option<CursorNumber>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    CursorNumber::deserialize(deserializer).map(Some)
}

pub fn parse_task_cursor(
    value: Option<&str>,
    kind: CursorKind,
) -> Result<Option<TaskFeedCursor>, InvalidCursor> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let parsed: TaskFeedCursorInput = serde_json::from_str(value).map_err(|_| InvalidCursor)?;
    let time = match kind {
        CursorKind::Current if parsed.created.is_none() => parsed.updated,
        CursorKind::Before if parsed.updated.is_none() => parsed.created,
        _ => return Err(InvalidCursor),
    }
    .ok_or(InvalidCursor)?;
    Ok(Some(TaskFeedCursor {
        id: parsed.id,
        time: time.0,
    }))
}

pub fn task_feed_response(
    tasks: Vec<Value>,
    has_more: bool,
    cursor: Option<&TaskFeedCursor>,
    before: Option<&TaskFeedCursor>,
) -> Value {
    let latest = tasks
        .iter()
        .map(task_cursor)
        .max_by(|left, right| {
            left.time
                .total_cmp(&right.time)
                .then_with(|| left.id.cmp(&right.id))
        })
        .unwrap_or(TaskFeedCursor {
            id: String::new(),
            time: 0.0,
        });
    let next_before = has_more.then(|| tasks.last()).flatten().map(|task| {
        json!({
            "created": task.get("created").cloned().unwrap_or(json!(0)),
            "id": task.get("id").cloned().unwrap_or(Value::Null),
        })
    });
    if before.is_some() {
        json!({"tasks": tasks, "hasMore": has_more, "nextBefore": next_before})
    } else if let Some(cursor) = cursor {
        let next = tasks
            .last()
            .map(task_cursor)
            .unwrap_or_else(|| cursor.clone());
        json!({
            "tasks": tasks, "cursor": {"updated": next.time, "id": next.id},
            "hasMore": false, "hasMoreChanges": has_more, "nextBefore": null, "reset": false,
        })
    } else {
        json!({
            "tasks": tasks, "cursor": {"updated": latest.time, "id": latest.id},
            "hasMore": has_more, "nextBefore": next_before, "reset": false,
        })
    }
}

fn task_cursor(task: &Value) -> TaskFeedCursor {
    let created = task.get("created").and_then(Value::as_f64).unwrap_or(0.0);
    let finished = task.get("finished").and_then(Value::as_f64).unwrap_or(0.0);
    TaskFeedCursor {
        id: task
            .get("id")
            .and_then(Value::as_str)
            .unwrap_or("")
            .to_owned(),
        time: created.max(finished),
    }
}

pub fn reported_change_files(diff: &str) -> Vec<Value> {
    let mut files = Vec::<Value>::new();
    let mut old_lines = 0_u64;
    let mut new_lines = 0_u64;
    let mut previous: Option<String> = None;
    for line in diff.lines() {
        if line.starts_with("@@ ") {
            let mut parts = line.split_whitespace();
            let _ = parts.next();
            old_lines = hunk_lines(parts.next().unwrap_or_default());
            new_lines = hunk_lines(parts.next().unwrap_or_default());
            previous = None;
        } else if old_lines > 0 || new_lines > 0 {
            if line.starts_with('-') || line.starts_with(' ') {
                old_lines = old_lines.saturating_sub(1);
            }
            if line.starts_with('+') || line.starts_with(' ') {
                new_lines = new_lines.saturating_sub(1);
            }
        } else if let Some(path) = line.strip_prefix("--- ") {
            previous = header_path(path);
        } else if let Some(path) = line.strip_prefix("+++ ") {
            if let Some(previous_path) = previous.take()
                && let Some(current_path) = header_path(path)
            {
                let is_deleted = current_path == "/dev/null";
                let path = if is_deleted {
                    previous_path.as_str()
                } else {
                    current_path.as_str()
                };
                let path = path
                    .strip_prefix("a/")
                    .or_else(|| path.strip_prefix("b/"))
                    .unwrap_or(path);
                if path != "/dev/null" {
                    let status = if is_deleted {
                        "D"
                    } else if previous_path == "/dev/null" {
                        "A"
                    } else {
                        "M"
                    };
                    if let Some(file) = files.iter_mut().find(|file| file["path"] == path) {
                        file["status"] = json!(status);
                    } else {
                        files.push(json!({"path":path,"status":status}));
                    }
                }
            }
        } else {
            previous = None;
        }
    }
    files
}

fn hunk_lines(value: &str) -> u64 {
    let value = value.trim_start_matches(['-', '+']);
    value
        .split_once(',')
        .map_or("1", |(_, count)| count)
        .parse()
        .unwrap_or(0)
}

fn header_path(path: &str) -> Option<String> {
    if !path.starts_with('"') {
        return path
            .split('\t')
            .next()
            .filter(|path| !path.is_empty())
            .map(str::to_owned);
    }
    let inner = path.strip_prefix('"')?.strip_suffix('"')?;
    let mut output = Vec::new();
    let mut bytes = inner.as_bytes().iter().copied();
    while let Some(byte) = bytes.next() {
        if byte != b'\\' {
            output.push(byte);
            continue;
        }
        let escaped = bytes.next()?;
        let decoded = match escaped {
            b'a' => 7,
            b'b' => 8,
            b'f' => 12,
            b'n' => b'\n',
            b'r' => b'\r',
            b't' => b'\t',
            b'v' => 11,
            b'\\' => b'\\',
            b'"' => b'"',
            b'0'..=b'3' => {
                let second = bytes.next()?;
                let third = bytes.next()?;
                if !(b'0'..=b'7').contains(&second) || !(b'0'..=b'7').contains(&third) {
                    return None;
                }
                (escaped - b'0') * 64 + (second - b'0') * 8 + (third - b'0')
            }
            _ => return None,
        };
        output.push(decoded);
    }
    String::from_utf8(output).ok()
}

pub struct WorkspaceInboxData<'a> {
    pub agents: &'a [Value],
    pub all_agents: &'a [Value],
    pub requests: &'a [Value],
    pub complaints: &'a [Value],
    pub work: &'a [Value],
    pub statuses: &'a BTreeMap<String, String>,
    pub monitors: &'a [Value],
    pub rules: &'a [Value],
}

pub fn workspace_inbox(data: WorkspaceInboxData<'_>) -> Vec<Value> {
    let WorkspaceInboxData {
        agents,
        all_agents,
        requests,
        complaints,
        work,
        statuses,
        monitors,
        rules,
    } = data;
    let mut inbox = Vec::new();
    for request in requests {
        if request["status"] == "pending" && !crate::event_write::truthy(request.get("deferred")) {
            inbox.push(json!({
                "id":request["id"],"kind":"request","agent":request["agent"],
                "title":"Answer required","text":request["method"],"request":request,
            }));
        }
    }
    let mut complaints = complaints.to_vec();
    complaints.sort_by(|left, right| {
        let left_needs_response = complaint_needs_response(left);
        let right_needs_response = complaint_needs_response(right);
        right_needs_response
            .cmp(&left_needs_response)
            .then_with(|| {
                right
                    .get("updated")
                    .and_then(Value::as_f64)
                    .unwrap_or_default()
                    .total_cmp(
                        &left
                            .get("updated")
                            .and_then(Value::as_f64)
                            .unwrap_or_default(),
                    )
            })
    });
    for complaint in &complaints {
        if !complaint_needs_response(complaint) {
            continue;
        }
        let Some(text) = complaint["text"].as_str() else {
            continue;
        };
        let id = complaint["id"].as_str().unwrap_or_default();
        let author = complaint["author"].as_str().unwrap_or_default();
        let lead_id = complaint["leadId"].as_str().unwrap_or_default();
        let author_record = all_agents.iter().find(|agent| agent["id"] == author);
        let lead = all_agents.iter().find(|agent| agent["id"] == lead_id);
        let mut projection = complaint.clone();
        projection["title"] = json!(text.chars().take(140).collect::<String>());
        projection["recipient"] = complaint.get("recipient").cloned().unwrap_or(Value::Null);
        projection["needsResponse"] = json!(true);
        projection["authorName"] = author_record
            .and_then(|agent| agent.get("name"))
            .cloned()
            .unwrap_or_else(|| json!(if author == "user" { "You" } else { author }));
        projection["leadName"] = lead
            .and_then(|agent| agent.get("name"))
            .cloned()
            .unwrap_or_else(|| json!(lead_id));
        projection["leadStopped"] = json!(
            !lead
                .and_then(|agent| agent.get("autoWake"))
                .and_then(Value::as_bool)
                .unwrap_or(false)
        );
        projection["leadDeleted"] = json!(lead.is_some_and(deleted_agent));
        inbox.push(json!({
            "id":id,"kind":"complaint","agent":lead_id,"title":text.chars().take(140).collect::<String>(),
            "text":"The lead must read and respond","complaint":projection,
        }));
    }
    for item in work.iter().filter(|item| item["status"] == "review") {
        inbox.push(json!({
            "id":item["id"],"kind":"work","agent":item["rootId"],"title":item["title"],
            "text":"Result awaits acceptance","work":crate::work_policy::work_item_view(item, statuses),
        }));
    }
    for agent in agents
        .iter()
        .filter(|agent| matches!(agent["status"].as_str(), Some("failed" | "interrupted")))
    {
        inbox.push(json!({
            "id":agent["id"],"kind":"agent","agent":agent["id"],"title":agent["name"],
            "text":agent.get("error").and_then(Value::as_str).filter(|text| !text.is_empty()).unwrap_or(agent["status"].as_str().unwrap_or("")),
        }));
    }
    for monitor in monitors.iter().filter(|monitor| {
        matches!(monitor["status"].as_str(), Some("failed" | "lost"))
            && monitor.get("ruleId").is_none_or(Value::is_null)
    }) {
        inbox.push(json!({
            "id":monitor["id"],"kind":"monitor","agent":monitor["agent"],"title":monitor["command"],
            "text":monitor.get("error").and_then(Value::as_str).filter(|text| !text.is_empty())
                .map(str::to_owned).unwrap_or_else(|| format!("Exit {}", monitor.get("exitCode").and_then(Value::as_i64).map_or_else(|| "None".into(), |code| code.to_string()))),
            "monitor":monitor,
        }));
    }
    for rule in rules
        .iter()
        .filter(|rule| crate::event_write::truthy(rule.get("error")))
    {
        inbox.push(json!({
            "id":rule["id"],"kind":"rule","agent":rule["agent"],"title":rule["name"],"text":rule["error"],
        }));
    }
    inbox
}
fn complaint_needs_response(complaint: &Value) -> bool {
    if matches!(complaint["status"].as_str(), Some("resolved" | "declined")) {
        return false;
    }
    if complaint["sourceType"] == "user_task" && complaint["status"] == "open" {
        return true;
    }
    let responsible = if complaint["recipient"] == "user" {
        "user"
    } else {
        complaint["leadId"].as_str().unwrap_or_default()
    };
    !complaint["responses"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|response| response["author"] == responsible)
}

#[cfg(test)]
mod tests {
    use super::{
        CursorKind, GitError, parse_task_cursor, read_bounded, run_git_program,
        run_git_program_timeout, task_feed_response,
    };
    use serde_json::json;
    use std::io::Cursor;
    use std::time::Duration;

    #[test]
    fn typed_cursor_rejects_wrong_fields_unknown_fields_and_unsafe_integers() {
        assert_eq!(
            parse_task_cursor(Some(r#"{"id":"task-1","updated":2}"#), CursorKind::Current)
                .expect("valid current cursor parses")
                .expect("cursor present")
                .time,
            2.0
        );
        assert!(parse_task_cursor(Some("{"), CursorKind::Current).is_err());
        assert!(
            parse_task_cursor(Some(""), CursorKind::Current)
                .expect("empty cursor is valid")
                .is_none()
        );
        assert!(parse_task_cursor(Some(r#"{"id":"x","created":2}"#), CursorKind::Current).is_err());
        assert!(
            parse_task_cursor(
                Some(r#"{"id":"x","updated":2,"extra":true}"#),
                CursorKind::Current
            )
            .is_err()
        );
        assert!(
            parse_task_cursor(
                Some(r#"{"id":"x","updated":9007199254740992}"#),
                CursorKind::Current
            )
            .is_err()
        );
        assert!(
            parse_task_cursor(Some(r#"{"id":"x","updated":-1}"#), CursorKind::Current).is_err()
        );
        assert!(
            parse_task_cursor(
                Some(r#"{"id":"x","updated":null,"created":2}"#),
                CursorKind::Before
            )
            .is_err()
        );
    }

    #[test]
    fn changes_cursor_advances_to_the_last_row_and_empty_poll_preserves_it() {
        let task = json!({"id":"a","created":1,"finished":2});
        let first = task_feed_response(
            vec![task],
            false,
            Some(&super::TaskFeedCursor {
                id: String::new(),
                time: 0.0,
            }),
            None,
        );
        let cursor = super::TaskFeedCursor {
            id: "a".into(),
            time: 2.0,
        };
        assert_eq!(first["cursor"], json!({"updated":2.0,"id":"a"}));
        let second = task_feed_response(vec![], false, Some(&cursor), None);
        assert_eq!(second["cursor"], first["cursor"]);
        assert!(second["tasks"].as_array().expect("tasks array").is_empty());
    }

    #[test]
    fn first_page_cursor_and_history_cursor_follow_python_contract() {
        let response = task_feed_response(
            vec![json!({"id":"a","created":2.0,"finished":5.0})],
            true,
            None,
            None,
        );
        assert_eq!(response["cursor"], json!({"updated":5.0,"id":"a"}));
        assert_eq!(response["nextBefore"], json!({"created":2.0,"id":"a"}));
        assert_eq!(response["hasMore"], true);
    }

    #[test]
    fn bounded_pipe_reader_drains_and_retains_only_its_prefix() {
        let input = vec![b'x'; 32_000];
        let output = read_bounded(Cursor::new(input), 1024).expect("pipe bytes read");
        assert_eq!(output.bytes, vec![b'x'; 1024]);
        assert!(output.truncated);
    }

    #[cfg(unix)]
    #[test]
    fn git_command_inherits_environment_preserves_arguments_and_bounds_errors() {
        use std::os::unix::fs::PermissionsExt;

        let root = std::env::temp_dir().join(format!("workspace-views-git-{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).expect("previous Git fixture removed");
        }
        std::fs::create_dir_all(&root).expect("fixture directory created");
        let executable = root.join("git-fake");
        std::fs::write(&executable, "#!/bin/sh\nprintf '%s\\n' \"$PATH\" \"$@\" >&2\nhead -c 2400 /dev/zero | tr '\\000' x >&2\nexit 1\n")
            .expect("fake Git command written");
        let mut permissions = std::fs::metadata(&executable)
            .expect("fake command metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).expect("fake command executable");
        let error = run_git_program(
            &executable,
            "/tmp/workspace",
            &["status", "--porcelain=v1", "-z"],
            1024,
        )
        .expect_err("fake command exits unsuccessfully");
        let GitError::Failed(message) = error else {
            panic!("unexpected Git error")
        };
        assert_eq!(message.chars().count(), 1200);
        assert!(message.starts_with(&std::env::var("PATH").expect("test PATH exists")));
        assert!(message.contains("--porcelain=v1"));
        std::fs::remove_dir_all(root).expect("Git fixture removed");
    }

    #[cfg(unix)]
    #[test]
    fn git_timeout_kills_a_descendant_that_keeps_a_pipe_open() {
        use std::os::unix::fs::PermissionsExt;

        let root =
            std::env::temp_dir().join(format!("workspace-views-timeout-{}", std::process::id()));
        if root.exists() {
            std::fs::remove_dir_all(&root).expect("old timeout fixture removed");
        }
        std::fs::create_dir_all(&root).expect("timeout fixture directory created");
        let executable = root.join("git-fake");
        std::fs::write(&executable, "#!/bin/sh\nsleep 60 &\nexit 0\n")
            .expect("fake Git command written");
        let mut permissions = std::fs::metadata(&executable)
            .expect("fake command metadata")
            .permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&executable, permissions).expect("fake command executable");

        let result = run_git_program_timeout(
            &executable,
            "/tmp/workspace",
            &["status"],
            1024,
            Duration::from_secs(2),
        );
        assert!(matches!(result, Err(GitError::TimedOut)));
        std::fs::remove_dir_all(root).expect("timeout fixture removed");
    }
}
