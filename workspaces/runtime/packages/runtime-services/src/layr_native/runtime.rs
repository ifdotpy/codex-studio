//! Native effects run outside SQLite. Durable receipts prevent uncertain retries.
use super::*;
use crate::bounded_child::{self, ChildError, Request};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    sync::Arc,
    time::Duration,
};
use studio_runtime_store::path_identity::CanonicalPath;
use studio_runtime_store::{AgentId, AgentRecord, AgentRepository, RuntimeStore, StoreError};

#[derive(Debug, thiserror::Error)]
pub enum NativeLayrError {
    #[error("Native Layr: {0}")]
    Refused(String),
    #[error("Native Layr operation has an unknown outcome; inspect its receipt: {0}")]
    Unknown(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error("Native Layr worker failed")]
    Worker,
}

type Result<T> = std::result::Result<T, NativeLayrError>;
const COMMAND_TIMEOUT: Duration = Duration::from_secs(300);
const OUTPUT_BYTES: usize = 2 * 1024 * 1024;

fn text<'a>(record: &'a Value, key: &str) -> Result<&'a str> {
    record[key]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or_else(|| NativeLayrError::Refused(format!("Missing {key}")))
}

/// Run only the Studio-owned binary. Ambient Layr and Git selectors never escape this store.
pub fn run(state_dir: &Path, cwd: &Path, args: &[&str]) -> Result<String> {
    // APFS clonefile traversal refuses symlink ancestors, including macOS /var and /tmp.
    let state_dir = CanonicalPath::existing(state_dir).map(CanonicalPath::into_path_buf)?;
    let binary = binary_path(&state_dir);
    let mut argv = vec![
        "-u",
        "LAYR_SOCKET",
        "-u",
        "LAYR_PROJECT",
        "-u",
        "LAYR_LINE",
        "-u",
        "LAYR_SESSION",
    ];
    let variables: Vec<String> =
        super::transport_environment(&state_dir, cfg!(target_os = "linux"))
            .into_iter()
            .map(|(name, value)| format!("{name}={}", value.as_str().unwrap_or_default()))
            .collect();
    argv.extend(variables.iter().map(String::as_str));
    argv.extend([
        binary
            .to_str()
            .ok_or_else(|| NativeLayrError::Refused("Non-UTF-8 binary path".into()))?,
        "-C",
        cwd.to_str()
            .ok_or_else(|| NativeLayrError::Refused("Non-UTF-8 cwd".into()))?,
    ]);
    argv.extend(args);
    let output = bounded_child::run_without_git_environment(Request {
        program: std::ffi::OsStr::new("/usr/bin/env"),
        git_directory: None,
        args: &argv,
        environment: &[],
        input: None,
        stdout_limit: OUTPUT_BYTES,
        stderr_limit: OUTPUT_BYTES,
        timeout: COMMAND_TIMEOUT,
    })
    .map_err(|error| match error {
        ChildError::Failed(detail) => NativeLayrError::Refused(format!(
            "{}: {detail}",
            args.first().copied().unwrap_or("layr")
        )),
        ChildError::Launch(detail) => NativeLayrError::Unknown(detail),
        ChildError::TimedOut => NativeLayrError::Unknown("command deadline".into()),
    })?;
    if output.stdout.truncated {
        return Err(NativeLayrError::Refused(
            "Command output exceeds the native Layr limit".into(),
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout.bytes)
        .trim()
        .to_owned())
}

pub(super) fn lock(state_dir: &Path, name: &str) -> Result<File> {
    let folder = state_dir.join(NATIVE_LAYR_DIRECTORY).join("locks");
    fs::create_dir_all(&folder)?;
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(folder.join(studio_runtime_store::sha256_hex(name.as_bytes())))?;
    file.lock()?;
    Ok(file)
}

/// Pending receipts survive process exit. A retry returns the same result or stays held.
pub fn operation(
    state_dir: &Path,
    id: &str,
    intent: &Value,
    effect: impl FnOnce() -> Result<Value>,
) -> Result<Value> {
    let _lock = lock(state_dir, id)?;
    let folder = state_dir.join(NATIVE_LAYR_DIRECTORY).join("receipts");
    fs::create_dir_all(&folder)?;
    let path = folder.join(format!(
        "{}.json",
        studio_runtime_store::sha256_hex(id.as_bytes())
    ));
    if path.exists() {
        let receipt: Value = serde_json::from_slice(&fs::read(&path)?)
            .map_err(|error| NativeLayrError::Refused(error.to_string()))?;
        if receipt["intent"] != *intent {
            return Err(NativeLayrError::Refused(
                "Request ID has different content".into(),
            ));
        }
        match receipt["status"].as_str() {
            Some("completed") => return Ok(receipt["result"].clone()),
            // A save is an idempotent quiet-point snapshot. An explicit failure can retry;
            // pending or unknown effects still never run again.
            Some("failed") if intent["kind"] == "save" => {}
            Some("failed") => {
                return Err(NativeLayrError::Refused(
                    receipt["error"]
                        .as_str()
                        .unwrap_or("operation failed")
                        .into(),
                ));
            }
            _ => return Err(NativeLayrError::Unknown(path.display().to_string())),
        }
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let pending = json!({"id":id,"intent":intent,"status":"pending"});
    if path.exists() {
        crate::atomic_file::replace(&path, pending.to_string().as_bytes(), Some(0o600), true)?;
    } else {
        let mut file = options.open(&path)?;
        file.write_all(pending.to_string().as_bytes())?;
        file.sync_all()?;
        File::open(&folder)?.sync_all()?;
    }
    let outcome = effect();
    let receipt = match &outcome {
        Ok(result) => json!({"id":id,"intent":intent,"status":"completed","result":result}),
        Err(NativeLayrError::Unknown(_)) => return outcome,
        Err(error) => json!({"id":id,"intent":intent,"status":"failed","error":error.to_string()}),
    };
    crate::atomic_file::replace(&path, receipt.to_string().as_bytes(), Some(0o600), true)?;
    outcome
}

pub fn project_id(record: &Value) -> Result<String> {
    if let Some(id) = record["layrProjectId"].as_str() {
        if id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Ok(id.into());
        }
        return Err(NativeLayrError::Refused(
            "Invalid native Layr project ID".into(),
        ));
    }
    let source = record["hostProjectPath"]
        .as_str()
        .ok_or_else(|| NativeLayrError::Refused("Missing project identity".into()))?;
    Ok(studio_runtime_store::sha256_hex(source.as_bytes()))
}

pub fn main_path(state_dir: &Path, project: &str) -> PathBuf {
    store_path(state_dir)
        .join("projects")
        .join(project)
        .join("lines/main")
}

pub(super) fn admit_git_policy(state_dir: &Path, project: &str, without_git: bool) -> Result<()> {
    let ready_path = state_dir
        .join(NATIVE_LAYR_DIRECTORY)
        .join(format!("{project}.json"));
    if without_git && ready_path.exists() {
        let receipt: Value = serde_json::from_slice(&fs::read(&ready_path)?)
            .map_err(|error| NativeLayrError::Refused(error.to_string()))?;
        if receipt["withoutGit"] != true {
            return Err(NativeLayrError::Refused("This project was already imported with Git metadata. The --without-git option cannot change an existing import.".into()));
        }
    }
    Ok(())
}

pub(super) fn prepare_files(state_dir: &Path, record: &Value) -> Result<Value> {
    prepare_files_with_git_policy(state_dir, record, false)
}

pub(super) fn prepare_files_with_git_policy(
    state_dir: &Path,
    record: &Value,
    without_git: bool,
) -> Result<Value> {
    let project = project_id(record)?;
    let main = main_path(state_dir, &project);
    let ready_path = state_dir
        .join(NATIVE_LAYR_DIRECTORY)
        .join(format!("{project}.json"));
    admit_git_policy(state_dir, &project, without_git)?;
    let plan = if !ready_path.exists() {
        let source = CanonicalPath::existing(text(record, "hostProjectPath")?)
            .map(CanonicalPath::into_path_buf)?;
        if state_dir.starts_with(&source) {
            return Err(NativeLayrError::Refused(
                "The Studio state directory must be outside the project".into(),
            ));
        }
        let plan = super::import::Plan::new_with_git_policy(
            &source,
            false,
            &state_dir.join(NATIVE_LAYR_DIRECTORY),
            without_git,
        )?;
        plan.admit()?;
        plan.size
            .preflight(&state_dir.join(NATIVE_LAYR_DIRECTORY))?;
        Some((source, plan))
    } else {
        None
    };
    super::install::ensure_binary(state_dir)?;
    let _project_lock = lock(state_dir, &format!("project:{project}"))?;
    admit_git_policy(state_dir, &project, without_git)?;
    if !ready_path.exists() {
        let (source, plan) =
            plan.ok_or_else(|| NativeLayrError::Refused("Native import readiness changed".into()))?;
        let mut input = json!({"project":project,"source":source});
        if without_git {
            input["withoutGit"] = json!(true);
        }
        operation(state_dir, &format!("import:{project}"), &input, || {
            let staging = tempfile::tempdir_in(state_dir.join(NATIVE_LAYR_DIRECTORY))?;
            plan.stage(staging.path())?;
            run(
                state_dir,
                state_dir,
                &[
                    "init",
                    &project,
                    "--from",
                    staging.path().to_str().unwrap_or_default(),
                    "--include-ignored",
                ],
            )?;
            run(state_dir, &main, &["protect", "main", "--direct"])?;
            plan.warm_caches(state_dir, &main, Path::new(""))?;
            let receipt = json!({"projectId":project,"source":source,"mainPath":main,"withoutGit":without_git});
            crate::atomic_file::replace(
                &ready_path,
                receipt.to_string().as_bytes(),
                Some(0o600),
                true,
            )?;
            Ok(receipt)
        })?;
    }
    let reviewer = record["role"] == "reviewer";
    let lead = record["isLead"] == true;
    let line = if lead || reviewer {
        "main".to_owned()
    } else {
        format!(
            "agent-{}",
            studio_runtime_store::sha256_hex(text(record, "id")?.as_bytes())
        )
    };
    let cwd = main
        .parent()
        .ok_or_else(|| NativeLayrError::Refused("Invalid main path".into()))?
        .join(&line);
    if !lead && !reviewer {
        operation(
            state_dir,
            &format!("line:{}", text(record, "id")?),
            &json!({"project":project,"line":line}),
            || {
                run(state_dir, &main, &["line", "new", &line, "main"])?;
                Ok(json!({"path":cwd,"line":line}))
            },
        )?;
    }
    super::git_guard::install_hook(state_dir, &cwd)?;
    let state = run(state_dir, &cwd, &["rev-parse", "head"])?;
    let mut result = json!({"cwd":cwd,"layrProjectId":project,"layrLine":line,"layrStateId":state,"layrReady":true});
    if reviewer {
        let scratch = state_dir.join(NATIVE_LAYR_DIRECTORY).join("scratch").join(
            studio_runtime_store::sha256_hex(text(record, "id")?.as_bytes()),
        );
        fs::create_dir_all(&scratch)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            fs::set_permissions(&scratch, fs::Permissions::from_mode(0o700))?;
        }
        let canonical = CanonicalPath::existing(&scratch).map(CanonicalPath::into_path_buf)?;
        let expected = CanonicalPath::existing(state_dir)
            .map(CanonicalPath::into_path_buf)?
            .join(NATIVE_LAYR_DIRECTORY)
            .join("scratch")
            .join(studio_runtime_store::sha256_hex(
                text(record, "id")?.as_bytes(),
            ));
        if canonical != expected {
            return Err(NativeLayrError::Refused(
                "Reviewer scratch folder has a symlink ancestor".into(),
            ));
        }
        result["layrScratch"] = json!(canonical);
        result["layrReadCwd"] = result["cwd"].clone();
        result["cwd"] = result["layrScratch"].clone();
    }
    Ok(result)
}

pub async fn prepare(store: Arc<RuntimeStore>, mut record: Value) -> Result<Value> {
    if record["deletedAt"].is_string()
        || record["deletedAt"].is_number()
        || record["autoWake"] != true
    {
        return Err(NativeLayrError::Refused("The agent is not active".into()));
    }
    // The next turn cannot enter its line before the previous quiet-point save.
    let pending: Vec<String> = record["layrTurnSaves"]
        .as_object()
        .into_iter()
        .flat_map(|turns| turns.iter())
        .filter(|(_, save)| save["status"] != "saved")
        .map(|(turn, _)| turn.clone())
        .collect();
    for turn in pending {
        let id = text(&record, "id")?.to_owned();
        save_turn(&store, &id, &turn).await?;
        record = agent_value(&store, &id).await?;
    }
    let state_dir = store.root().to_owned();
    let expected = record.clone();
    let prepared = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/layr-native/prepare",
        move || prepare_files(&state_dir, &expected),
    )
    .await
    .map_err(|_| NativeLayrError::Worker)??;
    store
        .transaction_with_error(move |tx| {
            let id = text(&record, "id")?;
            let mut current = AgentRepository::get(tx, &AgentId::from(id))?
                .ok_or_else(|| NativeLayrError::Refused("The agent no longer exists".into()))?
                .value();
            if studio_runtime_store::workspace_ready::workspace_generation(&current)
                != studio_runtime_store::workspace_ready::workspace_generation(&record)
            {
                return Err(NativeLayrError::Refused(
                    "The workspace changed during preparation".into(),
                ));
            }
            for (key, value) in prepared
                .as_object()
                .ok_or_else(|| NativeLayrError::Refused("Invalid native workspace".into()))?
            {
                current[key] = value.clone();
            }
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(current.clone()).map_err(StoreError::from)?,
            )?;
            Ok(current)
        })
        .await
}

/// Confirm the stored association against the owned project path before any effect.
pub(super) fn associated_cwd(state_dir: &Path, record: &Value) -> Result<PathBuf> {
    if record["layrReady"] != true {
        return Err(NativeLayrError::Refused(
            "The native Layr workspace is not ready".into(),
        ));
    }
    let project = project_id(record)?;
    let expected_line = if record["isLead"] == true || record["role"] == "reviewer" {
        "main".to_owned()
    } else {
        format!(
            "agent-{}",
            studio_runtime_store::sha256_hex(text(record, "id")?.as_bytes())
        )
    };
    if text(record, "layrLine")? != expected_line {
        return Err(NativeLayrError::Refused(
            "The native Layr line association changed".into(),
        ));
    }
    let expected = main_path(state_dir, &project)
        .parent()
        .ok_or_else(|| NativeLayrError::Refused("Invalid main path".into()))?
        .join(expected_line);
    let reviewer = record["role"] == "reviewer";
    if reviewer {
        let scratch = CanonicalPath::existing(state_dir)
            .map(CanonicalPath::into_path_buf)?
            .join(NATIVE_LAYR_DIRECTORY)
            .join("scratch")
            .join(studio_runtime_store::sha256_hex(
                text(record, "id")?.as_bytes(),
            ));
        if CanonicalPath::existing(text(record, "cwd")?).map(CanonicalPath::into_path_buf)?
            != scratch
            || CanonicalPath::existing(text(record, "layrScratch")?)
                .map(CanonicalPath::into_path_buf)?
                != scratch
        {
            return Err(NativeLayrError::Refused(
                "The reviewer scratch association changed".into(),
            ));
        }
    }
    let actual =
        CanonicalPath::existing(text(record, if reviewer { "layrReadCwd" } else { "cwd" })?)
            .map(CanonicalPath::into_path_buf)?;
    if actual != CanonicalPath::existing(expected).map(CanonicalPath::into_path_buf)? {
        return Err(NativeLayrError::Refused(
            "The native Layr cwd association changed".into(),
        ));
    }
    Ok(actual)
}

fn working_state(state_dir: &Path, cwd: &Path) -> Result<String> {
    match run(state_dir, cwd, &["rev-parse", "head@{0}"]) {
        Ok(state) => Ok(state),
        Err(NativeLayrError::Refused(detail)) if detail.contains("has only 0 automatic states") => {
            run(state_dir, cwd, &["rev-parse", "head"])
        }
        Err(error) => Err(error),
    }
}

pub fn save(state_dir: &Path, record: &Value, request_id: &str) -> Result<Value> {
    let cwd = associated_cwd(state_dir, record)?;
    operation(
        state_dir,
        request_id,
        &json!({"agent":record["id"],"cwd":cwd,"kind":"save"}),
        || {
            if record["role"] != "reviewer" {
                run(state_dir, &cwd, &["cache", "repair"])?;
                run(state_dir, &cwd, &["save"])?;
            }
            let state = if record["role"] == "reviewer" {
                run(state_dir, &cwd, &["rev-parse", "head"])?
            } else {
                working_state(state_dir, &cwd)?
            };
            Ok(json!({"stateId":state}))
        },
    )
}

pub fn remove(state_dir: &Path, record: &Value, request_id: &str) -> Result<Value> {
    operation(
        state_dir,
        request_id,
        &json!({"agent":record["id"],"line":record["layrLine"],"kind":"remove"}),
        || {
            if record["isLead"] != true
                && record["role"] != "reviewer"
                && record["layrReady"] == true
            {
                let project = project_id(record)?;
                let cwd = associated_cwd(state_dir, record)?;
                let line = text(record, "layrLine")?;
                if line == "main" {
                    return Err(NativeLayrError::Refused("Cannot delete main".into()));
                }
                let state = working_state(state_dir, &cwd)?;
                let archive_ref = format!(
                    "pins/studio/archive/{}",
                    studio_runtime_store::sha256_hex(text(record, "id")?.as_bytes())
                );
                run(
                    state_dir,
                    &main_path(state_dir, &project),
                    &["ref", "set", &archive_ref, &state],
                )?;
                run(
                    state_dir,
                    &main_path(state_dir, &project),
                    &["line", "delete", "-D", line],
                )?;
                return Ok(json!({"state":"removed","archiveRef":archive_ref,"stateId":state}));
            }
            Ok(json!({"state":"removed"}))
        },
    )
}

pub fn evidence(state_dir: &Path, record: &Value, revision: &str) -> Result<Value> {
    let cwd = associated_cwd(state_dir, record)?;
    let resolved = run(state_dir, &cwd, &["rev-parse", revision])?;
    if revision != resolved || run(state_dir, &cwd, &["rev-parse", "head"])? != resolved {
        return Err(NativeLayrError::Refused(
            "Submit the full Layr state ID".into(),
        ));
    }
    Ok(json!({"stateId":resolved,"projectId":record["layrProjectId"],"line":record["layrLine"]}))
}

pub fn merge(
    state_dir: &Path,
    lead: &Value,
    worker: &Value,
    revision: &str,
    request_id: &str,
) -> Result<Value> {
    if lead["isLead"] != true
        || worker["role"] == "reviewer"
        || lead["layrProjectId"] != worker["layrProjectId"]
    {
        return Err(NativeLayrError::Refused(
            "Invalid native Layr merge owner".into(),
        ));
    }
    let main = associated_cwd(state_dir, lead)?;
    associated_cwd(state_dir, worker)?;
    operation(
        state_dir,
        request_id,
        &json!({"lead":lead["id"],"worker":worker["id"],"revision":revision}),
        || {
            run(
                state_dir,
                &main,
                &["merge", text(worker, "layrLine")?, "--expect", revision],
            )?;
            Ok(json!({"stateId":run(state_dir,&main,&["rev-parse","head"])?}))
        },
    )
}

pub async fn agent_value(store: &RuntimeStore, id: &str) -> Result<Value> {
    let id = id.to_owned();
    store
        .read_async("runtime-services/layr-native/agent", move |tx| {
            AgentRepository::get(tx, &AgentId::from(id.as_str()))?
                .map(|r| r.value())
                .ok_or_else(|| NativeLayrError::Refused("Unknown native Layr agent".into()))
        })
        .await
}

pub async fn save_turn(store: &Arc<RuntimeStore>, agent_id: &str, turn_id: &str) -> Result<()> {
    let record = agent_value(store, agent_id).await?;
    if !is_native(&record) || record["layrReady"] != true {
        return Ok(());
    }
    let state_dir = store.root().to_owned();
    let request_id = format!("turn-save:{agent_id}:{turn_id}");
    let outcome = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-services/layr-native/save",
        move || save(&state_dir, &record, &request_id),
    )
    .await
    .map_err(|_| NativeLayrError::Worker)??;
    let agent_id = agent_id.to_owned();
    let turn_id = turn_id.to_owned();
    store
        .transaction_with_error(move |tx| {
            let mut record = AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?
                .ok_or_else(|| NativeLayrError::Refused("Unknown agent after save".into()))?
                .value();
            record["layrStateId"] = outcome["stateId"].clone();
            if !record["layrTurnSaves"].is_object() {
                record["layrTurnSaves"] = json!({});
            }
            record["layrTurnSaves"][&turn_id] =
                json!({"status":"saved","stateId":outcome["stateId"]});
            AgentRepository::put(
                tx,
                &AgentRecord::from_value(record).map_err(StoreError::from)?,
            )?;
            Ok(())
        })
        .await
}

/// The terminal transaction stages a save before it releases the next turn.
pub(crate) fn stage_turn_save(
    tx: &studio_runtime_store::Transaction<'_>,
    agent_id: &str,
    turn_id: &str,
) -> std::result::Result<(), StoreError> {
    let Some(agent) = AgentRepository::get(tx, &AgentId::from(agent_id))? else {
        return Ok(());
    };
    let mut record = agent.value();
    if !is_native(&record) || record["layrReady"] != true {
        return Ok(());
    }
    if !record["layrTurnSaves"].is_object() {
        record["layrTurnSaves"] = json!({});
    }
    if record["layrTurnSaves"][turn_id]["status"] == "saved" {
        return Ok(());
    }
    record["layrTurnSaves"][turn_id] = json!({"status":"pending"});
    AgentRepository::put(
        tx,
        &AgentRecord::from_value(record).map_err(StoreError::from)?,
    )?;
    Ok(())
}
