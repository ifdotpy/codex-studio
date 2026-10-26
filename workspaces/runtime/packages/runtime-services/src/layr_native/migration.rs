//! Import project worktrees as durable native lines. Original folders stay intact.
use super::{
    runtime::{self, NativeLayrError},
    *,
};
use serde_json::{Value, json};
#[cfg(test)]
use std::fs;
use std::{collections::BTreeSet, path::Path};
use studio_runtime_store::path_identity::CanonicalPath;

type Result<T> = std::result::Result<T, NativeLayrError>;

fn git(directory: &Path, args: &[&str]) -> Result<String> {
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("git"),
        git_directory: Some(directory.as_os_str()),
        args,
        environment: &[
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
        ],
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

fn operator_commands(state: &Path, project: &Path, lines: &[Value], options: &Options) -> Value {
    let state_arg = quote(&state.to_string_lossy());
    let project_arg = quote(&project.to_string_lossy());
    let mut selection = if options.without_git {
        format!("--without-git {project_arg}")
    } else {
        project_arg.clone()
    };
    selection.push_str(&format!(" --worktrees {}", options.worktrees.as_str()));
    for path in &options.skip_worktrees {
        selection.push_str(&format!(
            " --skip-worktree {}",
            quote(&path.to_string_lossy())
        ));
    }
    let mut commands = vec![
        format!("git -C {project_arg} worktree list --porcelain"),
        format!(
            "studio-runtime-server native-layr migrate --state {state_arg} --dry-run {selection}"
        ),
        format!("studio-runtime-server native-layr migrate --state {state_arg} {selection}"),
    ];
    let project_id = studio_runtime_store::sha256_hex(project.to_string_lossy().as_bytes());
    let main = main_path(state, &project_id);
    let environment = super::transport_environment(state, cfg!(target_os = "linux"))
        .into_iter()
        .map(|(name, value)| format!("{name}={}", quote(value.as_str().unwrap_or_default())))
        .collect::<Vec<_>>()
        .join(" ");
    let layr = format!(
        "env {environment} {} -C {}",
        quote(&binary_path(state).to_string_lossy()),
        quote(&main.to_string_lossy())
    );
    commands.push(format!("{layr} line list"));
    for line in lines.iter().filter(|line| line["status"] != "skipped") {
        let name = line["line"].as_str().unwrap_or_default();
        commands.push(format!("{layr} diff {}", quote(&format!("main...{name}"))));
        if let Some(state_id) = line["stateId"].as_str() {
            commands.push(format!(
                "{layr} merge {} --expect {}",
                quote(name),
                quote(state_id)
            ));
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

#[derive(Clone, Copy, Default, PartialEq, Eq)]
pub enum Worktrees {
    #[default]
    All,
    Dirty,
}
impl Worktrees {
    pub fn parse(value: &str) -> Option<Self> {
        match value {
            "all" => Some(Self::All),
            "dirty" => Some(Self::Dirty),
            _ => None,
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Self::All => "all",
            Self::Dirty => "dirty",
        }
    }
}

#[derive(Clone, Default)]
pub struct Options {
    pub worktrees: Worktrees,
    /// Omit only the root Git metadata. Nested repositories retain normal admission.
    pub without_git: bool,
    /// Explicit operator exclusions. Original worktree folders remain intact.
    pub skip_worktrees: Vec<PathBuf>,
}

pub fn migrate(state_dir: &Path, project: &Path, dry_run: bool) -> Result<Value> {
    migrate_with_options(state_dir, project, dry_run, Options::default())
}

/// Project identity uses the canonical source folder, as native chat preparation does.
pub fn migrate_with_options(
    state_dir: &Path,
    project: &Path,
    dry_run: bool,
    options: Options,
) -> Result<Value> {
    let project = CanonicalPath::existing(project).map(CanonicalPath::into_path_buf)?;
    let mut record = json!({"id":"migration", "isLead":true,"autoWake":true,"cwd":project});
    configure_new_chat(
        &mut record,
        project
            .to_str()
            .ok_or_else(|| NativeLayrError::Refused("Non-UTF-8 project path".into()))?,
    );
    let project_id = project_id(&record)?;
    let target = state_dir.join(NATIVE_LAYR_DIRECTORY);
    let main_plan =
        super::import::Plan::new_with_git_policy(&project, false, &target, options.without_git)?;
    let existing_path = main_path(state_dir, &project_id);
    // A failed first import can leave an incomplete main. Only ready imports supply estimates.
    let existing_main =
        if existing_path.exists() && target.join(format!("{project_id}.json")).exists() {
            Some(super::import::Plan::saved_main(
                state_dir,
                &existing_path,
                &target,
            )?)
        } else {
            None
        };
    let clone_main = existing_main.as_ref().unwrap_or(&main_plan);
    let mut plans = Vec::new();
    let initial_size = if existing_main.is_some() {
        super::import::Size::default()
    } else {
        main_plan.size
    };
    let mut size_all = initial_size;
    let mut size_dirty = initial_size;
    let mut lines = Vec::new();
    let mut seen = BTreeSet::new();
    let mut discovery_refusals = Vec::new();
    let mut sizes_complete = true;
    for repository in main_plan
        .repositories
        .iter()
        .filter(|repo| repo.included && repo.kind != "worktree")
    {
        let repo = project.join(&repository.relative);
        let relative = repo
            .strip_prefix(&project)
            .map_err(|error| NativeLayrError::Refused(error.to_string()))?;
        let output = match git(&repo, &["worktree", "list", "--porcelain", "-z"]) {
            Ok(output) => output,
            Err(error) => {
                discovery_refusals.push(format!(
                    "Cannot list worktrees for {}: {error}",
                    repo.display()
                ));
                sizes_complete = false;
                continue;
            }
        };
        for item in output.split("\0\0") {
            let fields: Vec<&str> = item.split('\0').collect();
            let Some(path) = fields.iter().find_map(|f| f.strip_prefix("worktree ")) else {
                continue;
            };
            let source = PathBuf::from(path);
            let head = fields.iter().find_map(|field| field.strip_prefix("HEAD "));
            if source == repo || !seen.insert(source.clone()) {
                continue;
            }
            if options.skip_worktrees.iter().any(|path| path == &source) {
                lines.push(json!({"source":source,"repository":relative,"headCommit":head,"status":"skipped","reason":"The operator explicitly skipped this worktree."}));
                continue;
            }
            if fields.iter().any(|f| f.starts_with("prunable")) || !source.join(".git").is_file() {
                lines
                    .push(json!({"source":source,"repository":relative,"headCommit":head,"status":"skipped","reason":"prunable worktree"}));
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
            let plan = match super::import::Plan::worktree(&source, &project, relative, &target) {
                Ok(plan) => plan,
                Err(error) => {
                    let reason = format!("Cannot size worktree {}: {error}", source.display());
                    discovery_refusals.push(reason.clone());
                    sizes_complete = false;
                    lines.push(json!({"source":source,"repository":relative,"line":line,"status":"refused","reason":reason}));
                    continue;
                }
            };
            let line_size = plan.migration_size(clone_main, relative);
            size_all.include(line_size);
            let mut entry = json!({"source":source,"repository":relative,"line":line,"headCommit":head,"status":if dry_run{"planned"}else{"pending"}});
            if let Some(fields) = serde_json::to_value(line_size)
                .map_err(|error| NativeLayrError::Refused(error.to_string()))?
                .as_object()
            {
                for (key, value) in fields {
                    entry[key] = value.clone();
                }
            }
            entry["lineNewEntries"] = json!(clone_main.size.entries);
            entry["lineNewEstimatedMetadataBytes"] = json!(clone_main.line_new_metadata_bytes());
            entry["refusals"] = json!(plan.refusals);
            entry["repositories"] = json!(super::inventory::repositories(&source, &plan, &[]));
            let root_facts = entry["repositories"]
                .as_array()
                .and_then(|repos| repos.iter().find(|repo| repo["relativePath"] == ""))
                .cloned()
                .unwrap_or(Value::Null);
            entry["classification"] =
                match super::inventory::classify_worktree(&source, &repo, &root_facts) {
                    Ok(classification) => json!(classification),
                    Err(error) => json!({"error":error.to_string()}),
                };
            entry["plainGitMetadata"] = json!(super::inventory::plain_git_metadata(&source, &plan));
            let selected_by_dirty = entry["classification"]["selectedByDirty"]
                .as_bool()
                .unwrap_or(true);
            if selected_by_dirty {
                size_dirty.include(line_size);
            }
            if options.worktrees == Worktrees::Dirty && !selected_by_dirty {
                entry["status"] = json!("skipped");
                entry["reason"] = json!(
                    "No uncommitted or untracked files. HEAD is contained in a local branch or remote-tracking ref. Its Git metadata is retained with main; ignored data remains in this source worktree."
                );
                let existing_main = main_path(state_dir, &project_id);
                if existing_main.exists() {
                    let proof = head
                        .ok_or_else(|| {
                            NativeLayrError::Refused("Missing skipped worktree HEAD".into())
                        })
                        .and_then(|head| {
                            super::import::verify_imported_head(&existing_main, relative, head)
                        });
                    if let Err(error) = proof {
                        discovery_refusals.push(format!("Cannot prove skipped HEAD is in the existing native main for {}: {error}. Use --worktrees all or inspect the earlier import.", source.display()));
                    }
                }
            } else {
                plans.push((lines.len(), plan));
            }

            lines.push(entry);
        }
    }
    let repositories = super::inventory::repositories(&project, &main_plan, &lines);
    let mut refusals = main_plan.refusals.clone();
    if let Err(error) = runtime::admit_git_policy(state_dir, &project_id, options.without_git) {
        refusals.push(error.to_string());
    }
    refusals.extend(discovery_refusals);
    for (root, plan) in
        std::iter::once((project.as_path(), &main_plan)).chain(plans.iter().map(|(index, plan)| {
            (
                Path::new(lines[*index]["source"].as_str().unwrap_or_default()),
                plan,
            )
        }))
    {
        for repository in plan
            .repositories
            .iter()
            .filter(|repo| !repo.included && repo.kind == "worktree")
        {
            let path = root.join(&repository.relative);
            if !lines.iter().any(|line| {
                line["source"]
                    .as_str()
                    .is_some_and(|source| Path::new(source) == path)
            }) {
                refusals.push(format!(
                    "Excluded worktree has no admitted parent repository: {}",
                    path.display()
                ));
            }
        }
    }
    for line in &lines {
        if let Some(reasons) = line["refusals"].as_array() {
            refusals.extend(reasons.iter().filter_map(Value::as_str).map(str::to_owned));
        }
    }
    for repository in &repositories {
        if let Some(error) = repository["statusError"].as_str() {
            refusals.push(error.to_owned());
        }
    }
    for (index, plan) in &plans {
        let relative = PathBuf::from(lines[*index]["repository"].as_str().unwrap_or_default());
        let mut changes = Vec::new();
        for cache in &main_plan.caches {
            if let Ok(local) = cache.strip_prefix(&relative)
                && plan.has_versioned_files_under(local)
            {
                changes.push(json!({"path":cache,"kind":"versioned","reason":"This worktree keeps files at a path that is a cache in main."}));
                if cfg!(target_os = "linux") {
                    refusals.push(format!("Worktree {} needs a plain versioned folder at {}. Native btrfs cannot convert the inherited cache subvolume yet; layr cache materialize is needed.", lines[*index]["source"], cache.display()));
                }
            }
        }
        for cache in &plan.caches {
            let global = relative.join(cache);
            if main_plan.has_versioned_files_under(&global) {
                changes.push(json!({"path":global,"kind":"cache","reason":"This worktree treats the path as build output. Main retains its versioned files."}));
            }
        }
        lines[*index]["cachePathChanges"] = json!(changes);
    }
    for line in lines.iter().filter(|line| line["status"] != "skipped") {
        if let Some(error) = line["classification"]["error"].as_str() {
            refusals.push(error.to_owned());
        }
        if let Some(repos) = line["repositories"].as_array() {
            for repo in repos {
                if let Some(error) = repo["statusError"].as_str() {
                    refusals.push(error.to_owned());
                }
            }
        }
    }
    let main_destination = main_path(state_dir, &project_id);
    let missing_line = |line: &&Value| {
        line["entries"].is_number()
            && main_destination.parent().is_none_or(|parent| {
                !parent
                    .join(line["line"].as_str().unwrap_or_default())
                    .exists()
            })
    };
    let main_to_create = usize::from(!main_destination.exists());
    let all_lines_to_create = lines.iter().filter(missing_line).count() + main_to_create;
    let dirty_lines_to_create = lines
        .iter()
        .filter(|line| line["classification"]["selectedByDirty"] == true)
        .filter(missing_line)
        .count()
        + main_to_create;
    let (size, lines_to_create) = if options.worktrees == Worktrees::All {
        (size_all, all_lines_to_create)
    } else {
        (size_dirty, dirty_lines_to_create)
    };
    let mut all_estimate = json!(size_all);
    all_estimate["linesToCreate"] = json!(all_lines_to_create);
    let mut dirty_estimate = json!(size_dirty);
    dirty_estimate["linesToCreate"] = json!(dirty_lines_to_create);
    let mut report = json!({"dryRun":dry_run,"withoutGit":options.without_git,"worktreeSelection":options.worktrees.as_str(),"selectionEstimates":{"all":all_estimate,"dirty":dirty_estimate},"skipWorktrees":options.skip_worktrees,"gitMetadataDisposition":if options.without_git {"Top-level .git is omitted and is not preserved. Source files and nested repositories follow normal admission. The source folder remains unchanged."} else {"Git metadata follows normal admission and preservation."},"projectId":project_id,"source":project,"layrVersion":super::LAYR_VERSION,"allocationModel":super::import::allocation_model(),"mainSize":clone_main.size,"sourceSize":main_plan.size,"worktreeData":main_plan.worktree_data,"worktreeSummary":super::inventory::worktree_summary(&lines),"linesToCreate":lines_to_create,"sizesComplete":sizes_complete,"excludedWorktreeFolders":main_plan.excluded_worktree_folders,"repositories":repositories,"plainGitMetadata":super::inventory::plain_git_metadata(&project, &main_plan),"refusals":refusals,"admissible":refusals.is_empty()});
    if let Some(fields) = serde_json::to_value(size)
        .map_err(|error| NativeLayrError::Refused(error.to_string()))?
        .as_object()
    {
        for (key, value) in fields {
            report[key] = value.clone();
        }
    }
    if dry_run {
        report["operator"] = operator_commands(state_dir, &project, &lines, &options);
        report["lines"] = json!(lines);
        return Ok(report);
    }
    if !refusals.is_empty() {
        return Err(NativeLayrError::Refused(refusals.join("\n")));
    }
    size.preflight(&target)?;
    let ready = runtime::prepare_files_with_git_policy(state_dir, &record, options.without_git)?;
    let main = PathBuf::from(
        ready["cwd"]
            .as_str()
            .ok_or_else(|| NativeLayrError::Refused("Missing main cwd".into()))?,
    );
    let _lock = runtime::lock(state_dir, &format!("migration:{project_id}"))?;
    for (index, plan) in plans {
        let line = &mut lines[index];
        let source = PathBuf::from(line["source"].as_str().unwrap_or_default());
        let name = line["line"].as_str().unwrap_or_default().to_owned();
        let relative = PathBuf::from(line["repository"].as_str().unwrap_or_default());
        let mut input = json!({"source":source,"repository":relative,"line":name});
        if options.without_git {
            input["withoutGit"] = json!(true);
        }
        let actual_main = super::import::Plan::saved_main(state_dir, &main, &target)?;
        let allocation = plan.migration_size(&actual_main, &relative);
        let imported_head = line["headCommit"].clone();
        let selection = options.worktrees.as_str();
        let receipt = runtime::operation_with_preflight(
            state_dir,
            &format!("migrate:{project_id}:{name}"),
            &input,
            || {
                plan.admit()?;
                allocation.preflight(&main)
            },
            || {
                run(state_dir, &main, &["line", "new", &name, "main"])?;
                let cwd = main
                    .parent()
                    .ok_or_else(|| NativeLayrError::Refused("Missing lines directory".into()))?
                    .join(&name);
                let line_directory = super::private_tree::Directory::open(&cwd)?;
                let destination = line_directory.child(&relative, true)?;
                let root_mode = destination.mode()?;
                // Move only the entry itself through the pinned destination parent.
                let saved_git = if destination.exists(std::ffi::OsStr::new(".git"))? {
                    Some(tempfile::tempdir_in(state_dir.join(NATIVE_LAYR_DIRECTORY))?)
                } else {
                    None
                };
                let saved_directory = saved_git
                    .as_ref()
                    .map(|saved| super::private_tree::Directory::open(saved.path()))
                    .transpose()?;
                if let Some(saved) = &saved_directory {
                    destination.rename(Path::new(".git"), saved, Path::new("git"))?;
                }
                let mut cache_roots: BTreeSet<PathBuf> = run(state_dir, &cwd, &["cache", "list"])?
                    .lines()
                    .filter_map(|line| line.split_once('\t'))
                    .filter(|(_, kind)| *kind == "cache")
                    .map(|(path, _)| PathBuf::from(path))
                    .collect();
                let versioned_roots: Vec<_> = cache_roots
                    .iter()
                    .filter(|cache| {
                        cache
                            .strip_prefix(&relative)
                            .is_ok_and(|local| plan.has_versioned_files_under(local))
                    })
                    .cloned()
                    .collect();
                for cache in versioned_roots {
                    discard_inherited_cache(&line_directory, &cache)?;
                    cache_roots.remove(&cache);
                }
                let cache_roots = cache_roots
                    .iter()
                    .filter_map(|cache| cache.strip_prefix(&relative).ok().map(Path::to_owned))
                    .collect();
                clear_at(&destination, &cache_roots)?;
                // Reuse the pinned subtree even if its pathname was replaced after opening it.
                plan.stage_open(&destination, &line_directory, &relative)?;
                if let Some(saved) = &saved_directory {
                    saved.rename(Path::new("git"), &destination, Path::new(".git"))?;
                }
                // A top-level line is a private container, distinct from a nested repository root.
                if relative.as_os_str().is_empty() {
                    destination.set_mode(root_mode)?;
                }
                super::git_guard::install_hook(state_dir, &cwd)?;
                runtime::save_imported_tree(state_dir, &cwd, "Import Git worktree")?;
                plan.warm_caches(state_dir, &cwd, &relative)?;
                Ok(
                    json!({"line":name,"headCommit":imported_head,"worktreeSelection":selection,"stateId":run(state_dir,&cwd,&["rev-parse","head"])?}),
                )
            },
        )?;
        line["status"] = json!("imported");
        line["stateId"] = receipt["stateId"].clone();
    }
    report["mainPath"] = json!(main);
    report["operator"] = operator_commands(state_dir, &project, &lines, &options);
    report["lines"] = json!(lines);
    Ok(report)
}

mod clear;
use clear::{clear_at, discard_inherited_cache};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn without_git_omits_only_root_metadata_and_records_operator_choice() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        fs::create_dir_all(project.join(".git/objects/info")).expect("broken Git metadata");
        git(&project, &["init", "-q"]).expect("valid source repository");
        fs::write(
            project.join(".git/objects/info/alternates"),
            "/absent/objects\n",
        )
        .expect("alternates");
        fs::write(project.join("code"), "retain").expect("source file");
        fs::create_dir(project.join("nested")).expect("nested repository");
        git(&project.join("nested"), &["init", "-q"]).expect("nested Git");
        let state = fixture.path().join("absent-state");
        let refused = migrate(&state, &project, true).expect("default report");
        assert_eq!(refused["admissible"], false);
        let report = migrate_with_options(
            &state,
            &project,
            true,
            Options {
                without_git: true,
                ..Default::default()
            },
        )
        .expect("plain-folder report");
        assert_eq!(report["admissible"], true);
        assert_eq!(report["withoutGit"], true);
        assert_eq!(
            report["repositories"]
                .as_array()
                .expect("nested report")
                .len(),
            1
        );
        assert!(
            report["operator"]["commands"]
                .to_string()
                .contains("--without-git")
        );
        assert!(project.join(".git/objects/info/alternates").is_file());
        assert!(!state.exists());
        let receipts = state.join(NATIVE_LAYR_DIRECTORY);
        fs::create_dir_all(&receipts).expect("existing import");
        fs::write(
            receipts.join(format!(
                "{}.json",
                report["projectId"].as_str().expect("project id")
            )),
            "{}",
        )
        .expect("old import receipt");
        let existing = migrate_with_options(
            &state,
            &project,
            true,
            Options {
                without_git: true,
                ..Default::default()
            },
        )
        .expect("policy mismatch report");
        assert_eq!(existing["admissible"], false);
        assert!(
            existing["refusals"]
                .to_string()
                .contains("cannot change an existing import")
        );
    }

    fn cache_policy_fixture(project: &Path) {
        fs::create_dir_all(project.join("sub/target")).expect("source");
        git(project, &["init", "-q"]).expect("Git init");
        fs::write(project.join("Cargo.toml"), "[package]\n").expect("manifest");
        fs::write(project.join(".gitignore"), "/target/\n").expect("ignore");
        fs::write(project.join("sub/target/from-main"), "base-data").expect("versioned data");
        git(project, &["add", "."]).expect("Git add");
        git(
            project,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "Base",
            ],
        )
        .expect("Git commit");
        let worker = project.join(".worktrees/worker");
        git(
            project,
            &[
                "worktree",
                "add",
                "-b",
                "worker",
                worker.to_str().expect("path"),
            ],
        )
        .expect("Git worktree");
        fs::create_dir(project.join("target")).expect("main cache");
        fs::write(project.join("target/warm"), "main-cache").expect("warm cache");
        fs::remove_file(worker.join("Cargo.toml")).expect("worktree has no root manifest");
        fs::create_dir(worker.join("target")).expect("versioned folder");
        fs::write(worker.join("target/line-file"), "line-data").expect("worktree data");
        fs::write(worker.join("sub/Cargo.toml"), "[package]\n").expect("worktree cache manifest");
    }

    #[test]
    fn review_existing_tree_counts_directory_entries() {
        let fixture = tempfile::tempdir().expect("fixture operation succeeds");
        let project = fixture.path().join("project");
        cache_policy_fixture(&project);
        let state = fixture.path().join("state");
        let initial = migrate(&state, &project, true).expect("fixture operation succeeds");
        let main = main_path(
            &state,
            initial["projectId"]
                .as_str()
                .expect("fixture operation succeeds"),
        );
        fs::create_dir_all(main.join("extra")).expect("fixture operation succeeds");
        for i in 0..200 {
            fs::write(main.join("extra").join(format!("file-{i}")), "bytes")
                .expect("fixture operation succeeds");
        }
        let plan = super::super::import::Plan::existing_main(&main, &state)
            .expect("fixture operation succeeds");
        assert_eq!(plan.size.entries, 202);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn review2_saved_main_controls_line_clone_space() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().expect("fixture operation succeeds");
        let project = fixture.path().join("project");
        cache_policy_fixture(&project);
        let state = fixture.path().join("state");
        let initial = migrate(&state, &project, true).expect("fixture operation succeeds");
        let id = initial["projectId"]
            .as_str()
            .expect("fixture operation succeeds");
        let main = main_path(&state, id);
        fs::create_dir_all(&main).expect("fixture operation succeeds");
        let saved = super::super::store_path(&state)
            .join("projects")
            .join(id)
            .join("states")
            .join("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa");
        fs::create_dir_all(saved.join("removed-from-working")).expect("fixture operation succeeds");
        for i in 0..200 {
            fs::write(
                saved.join("removed-from-working").join(format!("file-{i}")),
                "bytes",
            )
            .expect("fixture operation succeeds");
        }
        let binary = binary_path(&state);
        fs::create_dir_all(binary.parent().expect("fixture operation succeeds"))
            .expect("fixture operation succeeds");
        fs::write(
            &binary,
            format!(
                "#!/bin/sh\nprintf '%s\\n' '[{{\"name\":\"main\",\"head\":\"{}\"}}]'\n",
                "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa"
            ),
        )
        .expect("fixture operation succeeds");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o700))
            .expect("fixture operation succeeds");
        let partial_report = migrate(&state, &project, true).expect("partial import report");
        assert_eq!(
            partial_report["lines"][0]["lineNewEntries"], initial["lines"][0]["lineNewEntries"],
            "An unfinished import must not supply the saved-main estimate"
        );
        fs::write(
            state.join(NATIVE_LAYR_DIRECTORY).join(format!("{id}.json")),
            "{}",
        )
        .expect("ready import marker");
        let report = migrate(&state, &project, true).expect("fixture operation succeeds");
        assert_eq!(
            report["lines"][0]["lineNewEntries"], 202,
            "The saved main has 202 entries"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires the real vendored Layr binary on APFS"]
    fn review_migration_state_keeps_empty_directories_and_modes() {
        use std::os::unix::fs::PermissionsExt;
        let fixture = tempfile::tempdir().expect("fixture operation succeeds");
        let project = fixture.path().join("project");
        cache_policy_fixture(&project);
        let worker = project.join(".worktrees/worker");
        fs::create_dir_all(worker.join("empty")).expect("fixture operation succeeds");
        fs::set_permissions(worker.join("empty"), fs::Permissions::from_mode(0o710))
            .expect("fixture operation succeeds");
        fs::set_permissions(worker.join("sub"), fs::Permissions::from_mode(0o750))
            .expect("fixture operation succeeds");
        fs::set_permissions(
            worker.join("sub/target/from-main"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("fixture operation succeeds");
        // Same bytes, a mode change at an ordinary versioned file.
        fs::write(project.join("same"), "same").expect("fixture operation succeeds");
        fs::write(worker.join("same"), "same").expect("fixture operation succeeds");
        fs::set_permissions(worker.join("same"), fs::Permissions::from_mode(0o751))
            .expect("fixture operation succeeds");
        rustix::fs::lsetxattr(
            project.join("sub"),
            "com.studio.fixture",
            b"main-attribute",
            rustix::fs::XattrFlags::empty(),
        )
        .expect("fixture operation succeeds");
        rustix::fs::lsetxattr(
            worker.join("sub"),
            "com.studio.fixture",
            b"worker-attribute",
            rustix::fs::XattrFlags::empty(),
        )
        .expect("fixture operation succeeds");
        let state = fixture.path().join("state");
        fs::create_dir_all(
            binary_path(&state)
                .parent()
                .expect("fixture operation succeeds"),
        )
        .expect("fixture operation succeeds");
        fs::copy(
            std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY").expect("fixture operation succeeds"),
            binary_path(&state),
        )
        .expect("fixture operation succeeds");
        let report = migrate(&state, &project, false).expect("fixture operation succeeds");
        let main = PathBuf::from(
            report["mainPath"]
                .as_str()
                .expect("fixture operation succeeds"),
        );
        let line = main.parent().expect("fixture operation succeeds").join(
            report["lines"][0]["line"]
                .as_str()
                .expect("fixture operation succeeds"),
        );
        let mut attribute = [0u8; 64];
        let len = rustix::fs::lgetxattr(main.join("sub"), "com.studio.fixture", &mut attribute)
            .expect("fixture operation succeeds");
        assert_eq!(&attribute[..len], b"main-attribute");
        run(&state, &line, &["line", "new", "metadata-proof", "head"])
            .expect("fixture operation succeeds");
        let proof = line
            .parent()
            .expect("fixture operation succeeds")
            .join("metadata-proof");
        let len = rustix::fs::lgetxattr(proof.join("sub"), "com.studio.fixture", &mut attribute)
            .expect("fixture operation succeeds");
        assert_eq!(&attribute[..len], b"worker-attribute");
        for (name, mode) in [("empty", 0o710), ("sub", 0o750), ("same", 0o751)] {
            assert_eq!(
                fs::symlink_metadata(proof.join(name))
                    .expect("fixture operation succeeds")
                    .permissions()
                    .mode()
                    & 0o7777,
                mode,
                "State metadata at {name}"
            );
        }
    }

    #[test]
    fn migration_can_explicitly_skip_a_temporary_worktree_without_writes() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        cache_policy_fixture(&project);
        let worktree = CanonicalPath::existing(project.join(".worktrees/worker"))
            .map(CanonicalPath::into_path_buf)
            .expect("worktree identity");
        let state = fixture.path().join("absent-state");
        let report = migrate_with_options(
            &state,
            &project,
            true,
            Options {
                skip_worktrees: vec![worktree],
                ..Options::default()
            },
        )
        .expect("skip report");
        assert_eq!(report["admissible"], true);
        assert_eq!(report["linesToCreate"], 1);
        assert_eq!(report["lines"][0]["status"], "skipped");
        assert!(
            report["operator"]["commands"]
                .to_string()
                .contains("--skip-worktree")
        );
        assert!(project.join(".worktrees/worker/target/line-file").is_file());
        assert!(!state.exists());
    }

    #[test]
    fn migration_keeps_each_worktrees_cache_and_versioned_paths() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        cache_policy_fixture(&project);
        let state = fixture.path().join("absent-state");
        let report = migrate(&state, &project, true).expect("dry run");
        assert_eq!(report["admissible"], !cfg!(target_os = "linux"));
        if cfg!(target_os = "linux") {
            assert!(
                report["refusals"]
                    .to_string()
                    .contains("layr cache materialize")
            );
        }
        let changes = report["lines"][0]["cachePathChanges"]
            .as_array()
            .expect("cache changes");
        assert!(
            changes
                .iter()
                .any(|item| item["path"] == "target" && item["kind"] == "versioned")
        );
        assert!(
            changes
                .iter()
                .any(|item| item["path"] == "sub/target" && item["kind"] == "cache")
        );
        assert!(!state.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires the real vendored Layr binary on APFS"]
    fn migration_cache_path_can_hold_versioned_files_in_another_line() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        cache_policy_fixture(&project);
        let state = fixture.path().join("state");
        fs::create_dir_all(binary_path(&state).parent().expect("bin parent")).expect("bin");
        fs::copy(
            std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY").expect("binary"),
            binary_path(&state),
        )
        .expect("binary copy");
        let report =
            migrate(&state, &project, false).expect("migration with per-line cache policy");
        let main = PathBuf::from(report["mainPath"].as_str().expect("main"));
        let line = main
            .parent()
            .expect("lines")
            .join(report["lines"][0]["line"].as_str().expect("line"));
        assert_eq!(
            run(&state, &line, &["show", "head:target/line-file"])
                .expect("versioned worktree data"),
            "line-data"
        );
        assert!(run(&state, &line, &["show", "head:sub/target/from-main"]).is_err());
        assert_eq!(
            fs::read_to_string(line.join("sub/target/from-main")).expect("warm worktree cache"),
            "base-data"
        );
        let caches = run(&state, &line, &["cache", "list"]).expect("line cache list");
        assert!(
            caches
                .lines()
                .any(|entry| entry.starts_with("target\tnot a cache: has versioned files"))
        );
        assert!(caches.lines().any(|entry| entry == "sub/target\tcache"));
        assert!(
            run(&state, &main, &["cache", "list"])
                .expect("main caches")
                .lines()
                .any(|entry| entry == "target\tcache")
        );
        assert_eq!(
            run(&state, &main, &["show", "head:sub/target/from-main"])
                .expect("main versioned data"),
            "base-data"
        );
        assert_eq!(
            fs::read_to_string(project.join("target/warm")).expect("original cache"),
            "main-cache"
        );
    }

    fn shared_project_worktrees(project: &Path) -> (PathBuf, PathBuf) {
        for name in ["one", "two"] {
            let repo = project.join(name);
            fs::create_dir_all(&repo).expect("repository");
            git(&repo, &["init", "-q"]).expect("Git init");
            fs::write(repo.join("code"), name).expect("source code");
            git(&repo, &["add", "."]).expect("Git add");
            git(
                &repo,
                &[
                    "-c",
                    "user.name=Fixture",
                    "-c",
                    "user.email=fixture@example.test",
                    "-c",
                    "core.hooksPath=/dev/null",
                    "commit",
                    "-qm",
                    "Base",
                ],
            )
            .expect("commit");
        }
        let first = project.join("one/.worktrees/first");
        let second = first.join(".worktrees/second");
        git(
            &project.join("one"),
            &[
                "worktree",
                "add",
                "-b",
                "first",
                first.to_str().expect("path"),
            ],
        )
        .expect("first worktree");
        git(
            &project.join("two"),
            &[
                "worktree",
                "add",
                "-b",
                "second",
                second.to_str().expect("path"),
            ],
        )
        .expect("second worktree");
        fs::write(first.join("dirty"), "first work").expect("first edit");
        fs::write(second.join("dirty"), "second work").expect("second edit");
        (first, second)
    }

    #[test]
    fn migration_admits_git_stores_inside_the_whole_project_boundary() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        let (first, second) = shared_project_worktrees(&project);
        let state = fixture.path().join("absent-state");
        let report = migrate(&state, &project, true).expect("shared project report");
        assert_eq!(report["admissible"], true, "{}", report["refusals"]);
        assert_eq!(report["linesToCreate"], 3);
        assert!(
            report["inheritedEntries"]
                .as_u64()
                .expect("inherited entries")
                > 0
        );
        assert!(
            report["inheritedMetadataBytes"]
                .as_u64()
                .expect("inherited metadata")
                > 0
        );
        let entry_bytes = report["allocationModel"]["versionedEntryMetadataBytes"]
            .as_u64()
            .expect("entry allowance");
        assert_eq!(
            report["metadataBytes"].as_u64().expect("metadata"),
            (report["entries"].as_u64().expect("source entries")
                + report["inheritedEntries"]
                    .as_u64()
                    .expect("inherited entries"))
                * entry_bytes
        );

        assert_eq!(report["lines"].as_array().expect("lines").len(), 2);
        assert!(!state.exists());
        assert_eq!(
            fs::read_to_string(first.join("dirty")).expect("first edit"),
            "first work"
        );
        assert_eq!(
            fs::read_to_string(second.join("dirty")).expect("second edit"),
            "second work"
        );
        // Shared worktrees stay admitted. Source alternates need private materialization.
        git(&project, &["clone", "--shared", "one", "shared"]).expect("internal alternate");
        let internal_refusal = migrate(&state, &project, true).expect("internal alternate report");
        assert_eq!(internal_refusal["admissible"], false);
        assert!(
            internal_refusal["refusals"]
                .to_string()
                .contains("private materialization")
        );
        fs::write(
            project.join("shared/.git/objects/info/alternates"),
            format!("{}\n", fixture.path().display()),
        )
        .expect("external alternate");
        let refused = migrate(&state, &project, true).expect("external alternate report");
        assert_eq!(refused["admissible"], false);
        assert!(
            refused["refusals"]
                .to_string()
                .contains("outside the project")
        );
        fs::write(
            project.join("shared/.git/objects/info/alternates"),
            format!("{}\n", project.join("one/.git/objects").display()),
        )
        .expect("restore internal alternate");
        let worktree_git = crate::vm_preparation::read_gitdir_pointer(&first, &first.join(".git"))
            .expect("worktree metadata");
        fs::write(
            worktree_git.join("commondir"),
            format!("{}\n", fixture.path().display()),
        )
        .expect("external common directory");
        let refused = migrate(&state, &project, true).expect("external common report");
        assert_eq!(refused["admissible"], false);
        assert!(
            refused["refusals"]
                .to_string()
                .contains("outside the project")
        );
        assert!(!state.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires the real vendored Layr binary on APFS"]
    fn migration_imports_worktrees_that_share_an_internal_project_store() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        let (first, second) = shared_project_worktrees(&project);
        let state = fixture.path().join("state");
        fs::create_dir_all(binary_path(&state).parent().expect("bin parent")).expect("bin");
        fs::copy(
            std::env::var_os("STUDIO_LAYR_NATIVE_TEST_BINARY").expect("binary"),
            binary_path(&state),
        )
        .expect("binary copy");
        let report = migrate(&state, &project, false).expect("internal store migration");
        let main = PathBuf::from(report["mainPath"].as_str().expect("main"));
        assert!(
            run(&state, &main, &["show", "head:one/.git/HEAD"])
                .expect("nested Git metadata")
                .contains("refs/heads/")
        );
        for (source, repo, content) in [
            (&first, "one", "first work"),
            (&second, "two", "second work"),
        ] {
            let entry = report["lines"]
                .as_array()
                .expect("lines")
                .iter()
                .find(|entry| {
                    Path::new(entry["source"].as_str().expect("source"))
                        == CanonicalPath::existing(source)
                            .map(CanonicalPath::into_path_buf)
                            .expect("canonical source")
                })
                .expect("worktree line");
            let line = main
                .parent()
                .expect("lines root")
                .join(entry["line"].as_str().expect("line"));
            assert_eq!(
                run(&state, &line, &["show", &format!("head:{repo}/dirty")])
                    .expect("worktree edit"),
                content
            );
            assert!(run(&state, &line, &["show", "head:one/.worktrees/first/dirty"]).is_err());
            assert_eq!(
                fs::read_to_string(source.join("dirty")).expect("original edit"),
                content
            );
        }
    }

    #[test]
    fn migration_classifies_dirty_unpushed_and_merged_worktrees() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        fs::create_dir(&project).expect("project");
        git(&project, &["init", "-q"]).expect("Git init");
        fs::write(project.join("code"), "base").expect("source");
        git(&project, &["add", "."]).expect("add");
        git(
            &project,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "Base",
            ],
        )
        .expect("commit");
        let head = git(&project, &["rev-parse", "HEAD"]).expect("base HEAD");
        git(
            &project,
            &["update-ref", "refs/remotes/origin/main", head.trim()],
        )
        .expect("remote tracking fixture");
        for name in ["clean", "dirty", "commits", "detached"] {
            let worktree = project.join(".worktrees").join(name);
            if name == "detached" {
                git(
                    &project,
                    &[
                        "worktree",
                        "add",
                        "--detach",
                        worktree.to_str().expect("path"),
                    ],
                )
                .expect("detached worktree");
            } else {
                git(
                    &project,
                    &[
                        "worktree",
                        "add",
                        "-b",
                        name,
                        worktree.to_str().expect("path"),
                    ],
                )
                .expect("worktree");
            }
            if name == "dirty" {
                fs::write(worktree.join("untracked"), "retain").expect("untracked work");
            }
            if matches!(name, "commits" | "detached") {
                fs::write(worktree.join("code"), name).expect("new work");
                git(&worktree, &["add", "code"]).expect("add change");
                git(
                    &worktree,
                    &[
                        "-c",
                        "user.name=Fixture",
                        "-c",
                        "user.email=fixture@example.test",
                        "-c",
                        "core.hooksPath=/dev/null",
                        "commit",
                        "-qm",
                        "Worker",
                    ],
                )
                .expect("worker commit");
            }
        }
        let retained = fixture.path().join("retained-main");
        fs::create_dir_all(retained.join(".studio-native-import")).expect("owned main");
        git(
            &project,
            &[
                "clone",
                "--bare",
                "--no-local",
                project.to_str().expect("project path"),
                retained
                    .join(".studio-native-import/git")
                    .to_str()
                    .expect("store path"),
            ],
        )
        .expect("retained Git fixture");
        super::super::import::verify_imported_head(&retained, Path::new(""), head.trim())
            .expect("retained base commit");
        let detached = git(&project.join(".worktrees/detached"), &["rev-parse", "HEAD"])
            .expect("detached HEAD");
        assert!(
            super::super::import::verify_imported_head(&retained, Path::new(""), detached.trim())
                .is_err()
        );
        let state = fixture.path().join("absent-state");
        let report = migrate(&state, &project, true).expect("classification report");
        assert_eq!(report["admissible"], true, "{}", report["refusals"]);
        assert_eq!(report["worktreeSummary"]["uncommittedOrUntracked"], 1);
        assert_eq!(report["worktreeSummary"]["commitsToRetain"], 2);
        assert_eq!(report["worktreeSummary"]["cleanAndMerged"], 1);
        assert_eq!(report["linesToCreate"], 5);
        let dirty = migrate_with_options(
            &state,
            &project,
            true,
            Options {
                worktrees: Worktrees::Dirty,
                ..Options::default()
            },
        )
        .expect("dirty selection report");
        assert_eq!(dirty["admissible"], true, "{}", dirty["refusals"]);
        assert_eq!(dirty["selectionEstimates"]["all"]["linesToCreate"], 5);
        assert_eq!(dirty["selectionEstimates"]["dirty"]["linesToCreate"], 3);
        assert_eq!(dirty["linesToCreate"], 3);
        let skipped: Vec<_> = dirty["lines"]
            .as_array()
            .expect("lines")
            .iter()
            .filter(|line| line["status"] == "skipped")
            .collect();
        assert_eq!(skipped.len(), 2);
        for line in skipped {
            assert_eq!(
                line["headCommit"].as_str().expect("retained HEAD").len(),
                40
            );
            assert!(
                line["reason"]
                    .as_str()
                    .expect("skip reason")
                    .contains("Git metadata")
            );
        }
        assert!(!state.exists());
    }

    #[test]
    fn invalid_nested_git_metadata_is_plain_versioned_data_without_repo_reads() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        fs::create_dir_all(project.join("evidence/.git/refs")).expect("incomplete metadata");
        fs::write(project.join("evidence/.git/HEAD"), "ref: refs/heads/main\n")
            .expect("evidence HEAD");
        fs::create_dir_all(project.join("uv-cache/source")).expect("cached source");
        fs::write(project.join("uv-cache/source/.git"), "not a Git pointer\n")
            .expect("malformed marker");
        let state = fixture.path().join("absent-state");
        let report = migrate(&state, &project, true).expect("plain metadata report");
        assert_eq!(report["admissible"], true);
        assert_eq!(report["repositories"], json!([]));
        assert_eq!(
            report["plainGitMetadata"]
                .as_array()
                .expect("plain metadata entries")
                .len(),
            2
        );
        assert_eq!(report["files"], 2);
        assert_eq!(
            fs::read_to_string(project.join("evidence/.git/HEAD")).expect("source evidence"),
            "ref: refs/heads/main\n"
        );
        assert!(!state.exists());
    }

    #[test]
    fn dry_run_reports_invalid_metadata_and_unregistered_worktree_folders_with_totals() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        fs::create_dir_all(project.join("invalid/.git")).expect("invalid metadata");
        fs::write(project.join("invalid/code"), "keep").expect("code");
        let external = fixture.path().join("external");
        fs::create_dir(&external).expect("external repository");
        git(&external, &["init", "-q"]).expect("Git fixture");
        let worktree = project.join(".worktrees/unregistered");
        fs::create_dir_all(&worktree).expect("worktree");
        fs::write(
            worktree.join(".git"),
            format!("gitdir: {}\n", external.join(".git").display()),
        )
        .expect("pointer");
        fs::write(worktree.join("unsaved"), "retain original").expect("uncommitted work");
        let state = fixture.path().join("absent-state");
        let report = migrate(&state, &project, true).expect("complete refusal report");
        assert_eq!(report["files"], 3);
        assert_eq!(
            report["logicalBytes"],
            4 + fs::metadata(worktree.join(".git"))
                .expect("fixture operation succeeds")
                .len()
                + 15
        );
        assert_eq!(
            report["repositories"]
                .as_array()
                .expect("repositories")
                .len(),
            1
        );
        assert_eq!(
            report["plainGitMetadata"]
                .as_array()
                .expect("plain metadata")
                .len(),
            1
        );
        assert_eq!(report["excludedWorktreeFolders"], json!([".worktrees"]));
        assert_eq!(report["admissible"], false);
        assert!(
            report["refusals"]
                .to_string()
                .contains("External Git object store")
        );
        assert!(
            report["refusals"]
                .to_string()
                .contains("no admitted parent repository")
        );
        assert_eq!(
            fs::read_to_string(worktree.join("unsaved")).expect("original work"),
            "retain original"
        );
        assert!(!state.exists());
    }

    #[test]
    fn dry_run_sizes_selected_files_and_worktrees_without_creating_state() {
        let fixture = tempfile::tempdir().expect("fixture directory");
        let project = fixture.path().join("project");
        fs::create_dir(&project).expect("project directory");
        fs::write(project.join("code"), b"code").expect("code file");
        fs::write(project.join(".env"), b"secret").expect("ignored file");
        fs::create_dir(project.join("node_modules")).expect("cache directory");
        fs::write(project.join("node_modules/large"), b"excluded").expect("cache file");
        let state = fixture.path().join("state");
        let report = migrate(&state, &project, true).expect("dry run");
        assert_eq!(report["files"], 3);
        assert_eq!(report["logicalBytes"], 18);
        assert_eq!(report["trackedBytes"], 10);
        assert_eq!(report["cacheBytes"], 8);
        assert!(report["estimatedNewBytes"].as_u64().is_some());
        assert_eq!(report["crossVolumeBytes"], 0);
        assert_eq!(
            report["estimatedNewBytes"].as_u64().expect("estimate"),
            [
                "metadataBytes",
                "crossVolumeBytes",
                "gitHeadBytes",
                "gitTransferBytes"
            ]
            .iter()
            .map(|key| report[*key].as_u64().expect("component"))
            .sum::<u64>()
        );
        assert_eq!(report["linesToCreate"], 1);
        assert!(!state.exists());
    }

    #[test]
    fn dry_run_reports_nested_work_and_external_metadata_without_writes() {
        let fixture = tempfile::tempdir().expect("fixture");
        let project = fixture.path().join("project");
        let repo = project.join("nested");
        fs::create_dir_all(&repo).expect("repository");
        let command = |at: &Path, args: &[&str]| {
            git(at, args).expect("fixture Git command");
        };
        command(&repo, &["init", "-q"]);
        fs::write(repo.join("code"), "base").expect("code");
        fs::write(repo.join(".gitignore"), "data/\n").expect("ignore");
        command(&repo, &["add", "."]);
        command(
            &repo,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "Base",
            ],
        );
        let worktree = project.join(".worktrees/worker");
        command(
            &repo,
            &[
                "worktree",
                "add",
                "-b",
                "worker",
                worktree.to_str().expect("worktree path"),
            ],
        );
        fs::write(repo.join("code"), "changed").expect("change");
        fs::write(repo.join("untracked"), "keep").expect("untracked");
        fs::create_dir(repo.join("data")).expect("data");
        fs::write(repo.join("data/file"), "ignored data").expect("data");
        let external = fixture.path().join("external");
        fs::create_dir(&external).expect("external");
        command(&external, &["init", "-q"]);
        let pointer = project.join("pointer");
        fs::create_dir(&pointer).expect("pointer");
        fs::write(
            pointer.join(".git"),
            format!("gitdir: {}\n", external.join(".git").display()),
        )
        .expect("pointer");
        let state = fixture.path().join("absent-state");
        let report = migrate(&state, &project, true).expect("dry run");
        let nested = report["repositories"]
            .as_array()
            .expect("repositories")
            .iter()
            .find(|item| item["relativePath"] == "nested")
            .expect("nested report");
        assert_eq!(nested["uncommittedChanges"], 1);
        assert_eq!(nested["untrackedFiles"], 1);
        assert_eq!(nested["unpushedCommits"], 1);
        assert!(nested["currentBranch"].is_string());
        assert_eq!(report["linesToCreate"], 2);
        assert_eq!(report["lines"].as_array().expect("lines").len(), 1);
        assert_eq!(report["admissible"], false);
        assert!(
            report["refusals"]
                .to_string()
                .contains("External Git object store")
        );
        assert!(!state.exists());
    }

    #[test]
    fn migration_commands_preserve_paths_and_pin_imported_states() {
        let state = Path::new("/state with spaces");
        let project = Path::new("/project's code");
        let lines =
            vec![json!({"line":"wt-worker","status":"imported","stateId":"full-exact-state"})];
        let report = operator_commands(state, project, &lines, &Options::default());
        let commands = report["commands"]
            .as_array()
            .expect("fixture operation succeeds");
        assert!(
            commands[1]
                .as_str()
                .expect("fixture operation succeeds")
                .contains("--dry-run '/project'\"'\"'s code'")
        );
        assert!(
            commands[2]
                .as_str()
                .expect("fixture operation succeeds")
                .contains("--state '/state with spaces'")
        );
        assert!(
            commands
                .last()
                .expect("fixture operation succeeds")
                .as_str()
                .expect("fixture operation succeeds")
                .ends_with("merge 'wt-worker' --expect 'full-exact-state'")
        );
        assert!(
            report["steps"]
                .as_array()
                .expect("fixture operation succeeds")
                .iter()
                .any(|step| step
                    .as_str()
                    .expect("fixture operation succeeds")
                    .contains("does not pause or convert"))
        );
    }
}
