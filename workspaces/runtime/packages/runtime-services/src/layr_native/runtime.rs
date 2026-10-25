//! Native effects run outside SQLite. Durable receipts prevent uncertain retries.
use super::*;
use crate::bounded_child::{self, ChildError, Request};
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    sync::Arc,
    time::Duration,
};
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
    let state_dir = fs::canonicalize(state_dir)?;
    let binary = binary_path(&state_dir);
    let root = store_path(&state_dir);
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
    let root_env = format!("LAYR_ROOT={}", root.display());
    argv.extend([
        &root_env,
        "LAYR_DIRECT=1",
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
        return match receipt["status"].as_str() {
            Some("completed") => Ok(receipt["result"].clone()),
            Some("failed") => Err(NativeLayrError::Refused(
                receipt["error"]
                    .as_str()
                    .unwrap_or("operation failed")
                    .into(),
            )),
            _ => Err(NativeLayrError::Unknown(path.display().to_string())),
        };
    }
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&path)?;
    let pending = json!({"id":id,"intent":intent,"status":"pending"});
    file.write_all(pending.to_string().as_bytes())?;
    file.sync_all()?;
    File::open(&folder)?.sync_all()?;
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

pub(super) fn prepare_files(state_dir: &Path, record: &Value) -> Result<Value> {
    super::install::ensure_binary(state_dir)?;
    let project = project_id(record)?;
    let _project_lock = lock(state_dir, &format!("project:{project}"))?;
    let main = main_path(state_dir, &project);
    let ready_path = state_dir
        .join(NATIVE_LAYR_DIRECTORY)
        .join(format!("{project}.json"));
    if !ready_path.exists() {
        let source = fs::canonicalize(text(record, "hostProjectPath")?)?;
        if state_dir.starts_with(&source) {
            return Err(NativeLayrError::Refused(
                "The Studio state directory must be outside the project".into(),
            ));
        }
        operation(
            state_dir,
            &format!("import:{project}"),
            &json!({"project":project,"source":source}),
            || {
                let archive = crate::vm_preparation::vm_project_archive(&source)?;
                let staging = tempfile::tempdir_in(state_dir.join(NATIVE_LAYR_DIRECTORY))?;
                let decoder = flate2::read::GzDecoder::new(File::open(archive.file.path())?);
                tar::Archive::new(decoder).unpack(staging.path())?;
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
                let receipt = json!({"projectId":project,"source":source,"mainPath":main});
                crate::atomic_file::replace(
                    &ready_path,
                    receipt.to_string().as_bytes(),
                    Some(0o600),
                    true,
                )?;
                Ok(receipt)
            },
        )?;
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
        let canonical = fs::canonicalize(&scratch)?;
        let expected = fs::canonicalize(state_dir)?
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
fn associated_cwd(state_dir: &Path, record: &Value) -> Result<PathBuf> {
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
    let actual = fs::canonicalize(text(record, "cwd")?)?;
    if actual != fs::canonicalize(expected)? {
        return Err(NativeLayrError::Refused(
            "The native Layr cwd association changed".into(),
        ));
    }
    Ok(actual)
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
            Ok(
                json!({"stateId":run(state_dir,&cwd,&["rev-parse",if record["role"] == "reviewer" { "head" } else { "head@{0}" }])?}),
            )
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
                let line = text(record, "layrLine")?;
                if line == "main" {
                    return Err(NativeLayrError::Refused("Cannot delete main".into()));
                }
                run(
                    state_dir,
                    &main_path(state_dir, &project),
                    &["line", "delete", "-D", line],
                )?;
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
