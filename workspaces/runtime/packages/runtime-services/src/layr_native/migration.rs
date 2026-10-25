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

fn repositories(project: &Path) -> Result<Vec<PathBuf>> {
    let mut found = Vec::new();
    let mut pending = vec![project.to_owned()];
    while let Some(folder) = pending.pop() {
        if folder.join(".git").is_dir() {
            found.push(folder.clone());
        }
        for entry in fs::read_dir(&folder)? {
            let entry = entry?;
            let name = entry.file_name();
            if entry.file_type()?.is_dir()
                && !matches!(
                    name.to_str(),
                    Some(
                        ".git"
                            | ".worktrees"
                            | "node_modules"
                            | "target"
                            | ".venv"
                            | "venv"
                            | "__pycache__"
                            | ".next"
                            | ".turbo"
                    )
                )
            {
                if !entry.path().join(".git").is_file() {
                    pending.push(entry.path());
                }
            }
        }
    }
    found.sort();
    Ok(found)
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
    for repo in repositories(&project)? {
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
        return Ok(json!({"dryRun":true,"projectId":project_id,"source":project,"lines":lines}));
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
                tar::Archive::new(flate2::read::GzDecoder::new(File::open(
                    archive.file.path(),
                )?))
                .unpack(&destination)?;
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
        json!({"dryRun":false,"projectId":project_id,"source":project,"mainPath":main,"lines":lines}),
    )
}
