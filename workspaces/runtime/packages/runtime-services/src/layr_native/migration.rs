//! Import project worktrees as durable native lines. Original folders stay intact.
use super::{
    runtime::{self, NativeLayrError},
    *,
};
use serde_json::{Value, json};
use std::{
    collections::BTreeSet,
    fs::{self, File},
    path::Path,
};

type Result<T> = std::result::Result<T, NativeLayrError>;

fn git(directory: &Path, args: &[&str]) -> Result<String> {
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("git"),
        git_directory: Some(directory.as_os_str()),
        args,
        environment: &[("GIT_OPTIONAL_LOCKS", "0"), ("GIT_TERMINAL_PROMPT", "0")],
        input: None,
        stdout_limit: 8 * 1024 * 1024,
        stderr_limit: 64 * 1024,
        timeout: std::time::Duration::from_secs(60),
    })
    .map_err(|error| NativeLayrError::Refused(format!("Migration Git read failed: {error:?}")))?;
    if output.stdout.truncated {
        return Err(NativeLayrError::Refused(
            "Worktree list is too large".into(),
        ));
    }
    String::from_utf8(output.stdout.bytes)
        .map_err(|_| NativeLayrError::Refused("Non-UTF-8 worktree path".into()))
}

fn safe_name(value: &str) -> String {
    let name: String = value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .take(24)
        .collect();
    name.trim_matches('-').to_owned()
}

fn quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn operator_commands(state: &Path, project: &Path, lines: &[Value]) -> Value {
    let state_arg = quote(&state.to_string_lossy());
    let project_arg = quote(&project.to_string_lossy());
    let mut commands = vec![
        format!("git -C {project_arg} worktree list --porcelain"),
        format!("studio-runtime-server native-layr migrate --state {state_arg} --dry-run {project_arg}"),
        format!("studio-runtime-server native-layr migrate --state {state_arg} {project_arg}"),
    ];
    let project_id = studio_runtime_store::sha256_hex(project.to_string_lossy().as_bytes());
    let main = main_path(state, &project_id);
    let environment = super::transport_environment(state, cfg!(target_os = "linux"))
        .into_iter()
        .map(|(name,value)| format!("{name}={}", quote(value.as_str().unwrap_or_default())))
        .collect::<Vec<_>>().join(" ");
    let layr = format!("env {environment} {} -C {}", quote(&binary_path(state).to_string_lossy()), quote(&main.to_string_lossy()));
    commands.push(format!("{layr} line list"));
    for line in lines.iter().filter(|line| line["status"] != "skipped") {
        let name = line["line"].as_str().unwrap_or_default();
        commands.push(format!("{layr} diff {}", quote(&format!("main...{name}"))));
        if let Some(state_id) = line["stateId"].as_str() {
            commands.push(format!("{layr} merge {} --expect {}", quote(name), quote(state_id)));
        }
    }
    json!({
        "steps":[
            "Wait for current WT turns to finish. Pause those chats in Studio before the import.",
            "Each open Git worktree becomes a separate Layr line. Original folders remain intact.",
            "Existing WT chats keep their Git working directories. This command does not pause or convert chats.",
            "Run the dry run, inspect the worktree-to-line map, then run the migration once the source is quiet.",
            "Create a new Layr chat for the same original project folder. It uses native main.",
            "Review each imported line before running its printed merge command. The command uses the exact imported state.",
            "Keep old WT chats paused after the import. A repeated migration replays receipts and does not copy later edits."
        ],
        "commands":commands
    })
}

/// Project identity uses the canonical source folder, as native chat preparation does.
pub fn migrate(state_dir: &Path, project: &Path, dry_run: bool) -> Result<Value> {
    let project = fs::canonicalize(project)?;
    let mut record = json!({"id":"migration", "isLead":true,"autoWake":true,"cwd":project});
    configure_new_chat(
        &mut record,
        project
            .to_str()
            .ok_or_else(|| NativeLayrError::Refused("Non-UTF-8 project path".into()))?,
    );
    let project_id = project_id(&record)?;
    let mut lines = Vec::new();
    let mut seen = BTreeSet::new();
    for repo in crate::vm_preparation::import_repository_roots(&project)? {
        let relative = repo
            .strip_prefix(&project)
            .map_err(|error| NativeLayrError::Refused(error.to_string()))?;
        let output = git(&repo, &["worktree", "list", "--porcelain", "-z"])?;
        for item in output.split("\0\0") {
            let fields: Vec<&str> = item.split('\0').collect();
            let Some(path) = fields.iter().find_map(|f| f.strip_prefix("worktree ")) else {
                continue;
            };
            let source = PathBuf::from(path);
            if source == repo || !seen.insert(source.clone()) {
                continue;
            }
            if fields.iter().any(|f| f.starts_with("prunable")) || !source.join(".git").is_file() {
                lines
                    .push(json!({"source":source,"status":"skipped","reason":"prunable worktree"}));
                continue;
            }
            let basename = source.file_name().unwrap_or_default().to_string_lossy();
            let repository_name = repo.file_name().unwrap_or_default().to_string_lossy();
            let hash = studio_runtime_store::sha256_hex(path.as_bytes());
            let line = format!(
                "wt-{}-{}-{}",
                safe_name(&repository_name),
                safe_name(&basename),
                &hash[..12]
            );
            lines.push(json!({"source":source,"repository":relative,"line":line,"status":if dry_run{"planned"}else{"pending"}}));
        }
    }
    if dry_run {
        return Ok(json!({"dryRun":true,"projectId":project_id,"source":project,"operator":operator_commands(state_dir,&project,&lines),"lines":lines}));
    }
    let ready = runtime::prepare_files(state_dir, &record)?;
    let main = PathBuf::from(
        ready["cwd"]
            .as_str()
            .ok_or_else(|| NativeLayrError::Refused("Missing main cwd".into()))?,
    );
    let _lock = runtime::lock(state_dir, &format!("migration:{project_id}"))?;
    for line in &mut lines {
        if line["status"] == "skipped" {
            continue;
        }
        let source = PathBuf::from(line["source"].as_str().unwrap_or_default());
        let name = line["line"].as_str().unwrap_or_default().to_owned();
        let relative = PathBuf::from(line["repository"].as_str().unwrap_or_default());
        let receipt = operation(
            state_dir,
            &format!("migrate:{project_id}:{name}"),
            &json!({"source":source,"repository":relative,"line":name}),
            || {
                let archive = crate::vm_preparation::native_worktree_archive(&source)?;
                super::import::validate(archive.file.path(),&main)?;
                run(state_dir, &main, &["line", "new", &name, "main"])?;
                let cwd = main
                    .parent()
                    .ok_or_else(|| NativeLayrError::Refused("Missing lines directory".into()))?
                    .join(&name);
                let destination = cwd.join(&relative);
                // Replace only the repository subtree. Retain Layr's generated Git bridge.
                let git_marker = destination.join(".git");
                let saved_git = if git_marker.exists() {
                    Some(tempfile::tempdir_in(state_dir.join(NATIVE_LAYR_DIRECTORY))?)
                } else {
                    None
                };
                if let Some(saved) = &saved_git {
                    fs::rename(&git_marker, saved.path().join("git"))?;
                }
                if destination == cwd {
                    for entry in fs::read_dir(&destination)? {
                        let entry = entry?;
                        if entry.file_type()?.is_dir() {
                            fs::remove_dir_all(entry.path())?;
                        } else {
                            fs::remove_file(entry.path())?;
                        }
                    }
                } else if destination.exists() {
                    fs::remove_dir_all(&destination)?;
                }
                fs::create_dir_all(&destination)?;
                super::import::unpack(archive.file.path(),&destination)?;
                if let Some(saved) = &saved_git {
                    fs::rename(saved.path().join("git"), &git_marker)?;
                }
                run(state_dir, &cwd, &["add", "-A"])?;
                let mut archive_files = Vec::new();
                for entry in tar::Archive::new(flate2::read::GzDecoder::new(File::open(
                    archive.file.path(),
                )?))
                .entries()?
                {
                    let entry = entry?;
                    if entry.header().entry_type().is_file()
                        || entry.header().entry_type().is_symlink()
                    {
                        archive_files
                            .push(relative.join(entry.path()?).to_string_lossy().into_owned());
                    }
                }
                for paths in archive_files.chunks(64) {
                    let mut args = vec!["add", "-f", "--"];
                    args.extend(paths.iter().map(String::as_str));
                    run(state_dir, &cwd, &args)?;
                }
                run(
                    state_dir,
                    &cwd,
                    &["save", "-m", "Import Git worktree", "--allow-empty"],
                )?;
                Ok(json!({"line":name,"stateId":run(state_dir,&cwd,&["rev-parse","head"])?}))
            },
        )?;
        line["status"] = json!("imported");
        line["stateId"] = receipt["stateId"].clone();
    }
    Ok(
        json!({"dryRun":false,"projectId":project_id,"source":project,"mainPath":main,"operator":operator_commands(state_dir,&project,&lines),"lines":lines}),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_commands_preserve_paths_and_pin_imported_states() {
        let state = Path::new("/state with spaces");
        let project = Path::new("/project's code");
        let lines = vec![json!({"line":"wt-worker","status":"imported","stateId":"full-exact-state"})];
        let report = operator_commands(state, project, &lines);
        let commands = report["commands"].as_array().unwrap();
        assert!(commands[1].as_str().unwrap().contains("--dry-run '/project'\"'\"'s code'"));
        assert!(commands[2].as_str().unwrap().contains("--state '/state with spaces'"));
        assert!(commands.last().unwrap().as_str().unwrap().ends_with("merge 'wt-worker' --expect 'full-exact-state'"));
        assert!(report["steps"].as_array().unwrap().iter().any(|step|step.as_str().unwrap().contains("does not pause or convert")));
    }
}
