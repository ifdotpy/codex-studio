//! Read-only Git facts for native migration admission and operator reports.
use super::{NativeLayrError, import::Plan};
use serde_json::{Value, json};
use std::{path::Path, time::Duration};

type Result<T> = std::result::Result<T, NativeLayrError>;
pub(super) fn git(at: &Path, args: &[&str]) -> Result<Vec<u8>> {
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("git"),
        git_directory: Some(at.as_os_str()),
        args,
        environment: &[
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_LITERAL_PATHSPECS", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("LC_ALL", "C"),
        ],
        input: None,
        stdout_limit: 64 * 1024 * 1024,
        stderr_limit: 64 * 1024,
        timeout: Duration::from_secs(60),
    })
    .map_err(|error| {
        NativeLayrError::Refused(format!("Git read at {} failed: {error:?}", at.display()))
    })?;
    if output.stdout.truncated {
        return Err(NativeLayrError::Refused(format!(
            "Git report at {} exceeds the output limit",
            at.display()
        )));
    }
    Ok(output.stdout.bytes)
}

pub(super) fn repositories(source: &Path, plan: &Plan, lines: &[Value]) -> Vec<Value> {
    plan.repositories.iter().map(|repo| {
        let path = source.join(&repo.relative);
        let mut report = json!({"path":path,"relativePath":repo.relative,"kind":repo.kind,"included":repo.included,"cache":repo.cache,
            "files":"All files, including Git-ignored data, enter Layr history. Build output becomes a warm cache.",
            "gitMetadata":if repo.kind=="worktree" {"The .git pointer is omitted. Parent Git metadata comes from the first main import. Later Git refs and objects are not refreshed. The original Git store remains intact; inspect the unpushed commit report before retiring it."} else if repo.relative.as_os_str().is_empty() {"The complete source .git tree is retained in .studio-native-import/git. Layr creates its own root Git bridge."} else if repo.kind=="submodule" {"Internal Git metadata is retained. The cloned .git pointer and core.worktree point to that metadata."} else {"The nested .git tree enters Layr history, including local refs and objects."},
            "worktrees":lines.iter().filter(|line| line["repository"].as_str().is_some_and(|r| Path::new(r)==repo.relative)).cloned().collect::<Vec<_>>()});
        if repo.cache {
            report["files"]=json!("This repository is inside build output. Its complete folder becomes a warm cache, outside Layr history.");
        }
        if !repo.included {
            report["files"] = json!("This folder is excluded from main. A linked worktree becomes its own line. An external object store causes refusal.");
        }
        let facts = (|| -> Result<Value> {
            let status = git(&path,&["-c","core.fsmonitor=false","status","--porcelain=v1","-z","--branch","--untracked-files=all"])?;
            let mut changes=0u64;
            let mut untracked=0u64;
            let mut branch=None;
            let mut rename_tail=false;
            for entry in status.split(|b| *b==0).filter(|r| !r.is_empty()) {
                if rename_tail {rename_tail=false; continue;}
                if let Some(header) = entry.strip_prefix(b"## ") {
                    branch=Some(String::from_utf8_lossy(header).split("...").next().unwrap_or_default().trim_start_matches("No commits yet on ").trim_start_matches("Initial commit on ").to_owned());
                } else if entry.starts_with(b"??") {untracked+=1;}
                else {
                    changes+=1;
                    rename_tail = entry.first().is_some_and(|b| matches!(b,b'R'|b'C')) || entry.get(1).is_some_and(|b| matches!(b,b'R'|b'C'));
                }
            }
            let unpushed = if branch.as_deref().is_some_and(|name| name.starts_with("HEAD ")) {
                git(&path,&["rev-list","--count","HEAD","--branches","--not","--remotes"])?
            } else {git(&path,&["rev-list","--count","--branches","--not","--remotes"])?};
            let unpushed = String::from_utf8_lossy(&unpushed).trim().parse::<u64>().map_err(|error| NativeLayrError::Refused(error.to_string()))?;
            Ok(json!({"currentBranch":branch,"uncommittedChanges":changes,"untrackedFiles":untracked,"unpushedCommits":unpushed,"unpushedBasis":"Commits on local branches and detached HEAD that no remote-tracking ref contains. No network fetch runs."}))
        })();
        match facts {
            Ok(facts) => if let Some(object) = facts.as_object() { for (name,value) in object {report[name]=value.clone();} },
            Err(error) => {report["statusError"]=json!(error.to_string());}
        }
        report
    }).collect()
}

/// Invalid markers retain their bytes and do not enter Git status or worktree discovery.
pub(super) fn plain_git_metadata(source: &Path, plan: &Plan) -> Vec<Value> {
    plan.plain_git_metadata.iter().map(|item| json!({
        "path":source.join(&item.relative_path),
        "relativePath":item.relative_path,
        "reason":item.reason,
        "cache":item.cache,
        "treatment":if item.cache {"Preserved byte-for-byte as plain data inside a warm build cache, outside history. No repository status read or worktree discovery."} else if item.relative_path == Path::new(".git") {"Versioned as plain data in .studio-native-import/git. No source Git store is imported."} else {"Versioned byte-for-byte as plain data. No repository admission, status read, or worktree discovery."}
    })).collect()
}

#[derive(Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub(super) enum WorktreeClass {
    #[serde(rename = "a")]
    Dirty,
    #[serde(rename = "b")]
    Commits,
    #[serde(rename = "c")]
    Clean,
}

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(super) struct Classification {
    class: WorktreeClass,
    uncommitted_changes: u64,
    untracked_files: u64,
    head_unpushed_commits: u64,
    head_commits_outside_local_branches: u64,
    head_commits_outside_all_refs: u64,
    selected_by_dirty: bool,
    head_commits_outside_main_head: u64,
    main_repository_head: String,
    ignored_data: &'static str,
}

/// Classify HEAD, rather than counting unrelated branches in the shared Git store.
pub(super) fn classify_worktree(
    source: &Path,
    repository: &Path,
    facts: &Value,
) -> Result<Classification> {
    let changes = facts["uncommittedChanges"].as_u64().ok_or_else(|| {
        NativeLayrError::Refused("Missing worktree status for classification".into())
    })?;
    let hidden = hidden_index_changes(source)?;
    let changes = changes.saturating_add(hidden);
    let untracked = facts["untrackedFiles"].as_u64().ok_or_else(|| {
        NativeLayrError::Refused("Missing worktree untracked count for classification".into())
    })?;
    let count = |args: &[&str]| -> Result<u64> {
        let value = git(source, args)?;
        String::from_utf8_lossy(&value)
            .trim()
            .parse()
            .map_err(|error| {
                NativeLayrError::Refused(format!("Invalid worktree commit count: {error}"))
            })
    };
    let main_head = String::from_utf8(git(repository, &["rev-parse", "HEAD"])?)
        .map_err(|error| NativeLayrError::Refused(error.to_string()))?
        .trim()
        .to_owned();
    let unpushed = count(&["rev-list", "--count", "HEAD", "--not", "--remotes"])?;
    let outside_branches = count(&["rev-list", "--count", "HEAD", "--not", "--branches"])?;
    let outside_all_refs = count(&[
        "rev-list",
        "--count",
        "HEAD",
        "--not",
        "--branches",
        "--remotes",
    ])?;
    let outside_main = count(&["rev-list", "--count", "HEAD", "--not", &main_head])?;
    let class = if changes != 0 || untracked != 0 {
        WorktreeClass::Dirty
    } else if unpushed != 0 || outside_branches != 0 || outside_main != 0 {
        WorktreeClass::Commits
    } else {
        WorktreeClass::Clean
    };
    Ok(Classification {
        class,
        uncommitted_changes: changes,
        untracked_files: untracked,
        head_unpushed_commits: unpushed,
        head_commits_outside_local_branches: outside_branches,
        head_commits_outside_all_refs: outside_all_refs,
        selected_by_dirty: changes != 0 || untracked != 0 || outside_all_refs != 0,
        head_commits_outside_main_head: outside_main,
        main_repository_head: main_head,
        ignored_data: "Git status does not count ignored data. Such files can differ even in a clean worktree. All worktrees are selected by default.",
    })
}

pub(super) fn worktree_summary(lines: &[Value]) -> Value {
    let count = |class| {
        lines
            .iter()
            .filter(|line| line["classification"]["class"] == json!(class))
            .count()
    };
    let dirty = count(WorktreeClass::Dirty);
    let commits = count(WorktreeClass::Commits);
    let clean = count(WorktreeClass::Clean);
    let selected = lines
        .iter()
        .filter(|line| line["classification"]["selectedByDirty"] == true)
        .count();
    json!({"selectedByDirty":selected,"total":lines.len(),"uncommittedOrUntracked":dirty,"commitsToRetain":commits,"cleanAndMerged":clean,"unclassified":lines.len() - dirty - commits - clean,"basis":"Class a has uncommitted or untracked files. Class b has HEAD commits absent from remote-tracking refs, local branches, or the parent repository HEAD. Class c has none of these. Ignored data is not part of Git status. No network fetch runs."})
}

/// Git status can conceal edits under assume-unchanged and skip-worktree flags.
fn hidden_index_changes(source: &Path) -> Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let flagged = git(source, &["ls-files", "-v", "-z"])?;
    let mut changed = 0u64;
    for entry in flagged
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        if entry.len() < 3 || !(entry[0].is_ascii_lowercase() || entry[0] == b'S') {
            continue;
        }
        let path = std::str::from_utf8(&entry[2..])
            .map_err(|_| NativeLayrError::Refused("Non-UTF-8 flagged index path".into()))?;
        let equal = (|| -> Result<bool> {
            let stage = git(source, &["ls-files", "--stage", "-z", "--", path])?;
            let record =
                std::str::from_utf8(&stage).map_err(|e| NativeLayrError::Refused(e.to_string()))?;
            let exact = record
                .split('\0')
                .filter_map(|record| record.split_once('\t'))
                .find(|(_, name)| *name == path)
                .ok_or_else(|| NativeLayrError::Refused("Missing exact index entry".into()))?;
            let (index, _) = exact;
            let mut fields = index.split_whitespace();
            let mode = fields.next().unwrap_or_default();
            let oid = fields.next().unwrap_or_default();
            let bytes = git(source, &["cat-file", "blob", oid])?;
            let at = source.join(path);
            let metadata = std::fs::symlink_metadata(&at)?;
            let actual = if metadata.file_type().is_symlink() {
                std::fs::read_link(&at)?.as_os_str().as_bytes().to_vec()
            } else if metadata.is_file() && metadata.len() <= 64 * 1024 * 1024 {
                std::fs::read(&at)?
            } else {
                return Ok(false);
            };
            use std::os::unix::fs::PermissionsExt;
            let actual_mode = if metadata.file_type().is_symlink() {
                "120000"
            } else if metadata.permissions().mode() & 0o111 != 0 {
                "100755"
            } else {
                "100644"
            };
            Ok(mode == actual_mode && bytes == actual)
        })()
        .unwrap_or(false);
        if !equal {
            changed += 1;
        }
    }
    Ok(changed)
}

#[cfg(test)]
mod review_tests {
    use super::*;
    #[test]
    fn review_dirty_selection_detects_hidden_index_edits() {
        let root = tempfile::tempdir().expect("fixture operation succeeds");
        let at = root.path();
        git(at, &["init", "-q"]).expect("fixture operation succeeds");
        for name in ["assumed", "skipped"] {
            std::fs::write(at.join(name), "base").expect("fixture operation succeeds");
        }
        git(at, &["add", "."]).expect("fixture operation succeeds");
        git(
            at,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "base",
            ],
        )
        .expect("fixture operation succeeds");
        git(at, &["update-index", "--assume-unchanged", "assumed"])
            .expect("fixture operation succeeds");
        git(at, &["update-index", "--skip-worktree", "skipped"])
            .expect("fixture operation succeeds");
        for name in ["assumed", "skipped"] {
            std::fs::write(at.join(name), "edit").expect("fixture operation succeeds");
            let class =
                classify_worktree(at, at, &json!({"uncommittedChanges":0,"untrackedFiles":0}))
                    .expect("fixture operation succeeds");
            assert!(
                class.selected_by_dirty,
                "Hidden index edits must select the line"
            );
            std::fs::write(at.join(name), "base").expect("fixture operation succeeds");
        }
    }
}

#[cfg(test)]
mod review2_tests {
    use super::*;
    #[test]
    fn review2_hidden_index_paths_are_literal() {
        let root = tempfile::tempdir().expect("fixture operation succeeds");
        let at = root.path();
        git(at, &["init", "-q"]).expect("fixture operation succeeds");
        std::fs::write(at.join("a"), "edited").expect("fixture operation succeeds");
        std::fs::write(at.join("a*"), "base").expect("fixture operation succeeds");
        git(at, &["add", "."]).expect("fixture operation succeeds");
        git(
            at,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.test",
                "-c",
                "core.hooksPath=/dev/null",
                "commit",
                "-qm",
                "base",
            ],
        )
        .expect("fixture operation succeeds");
        git(at, &["update-index", "--assume-unchanged", "a*"]).expect("fixture operation succeeds");
        std::fs::write(at.join("a*"), "edited").expect("fixture operation succeeds");
        assert!(
            hidden_index_changes(at).expect("fixture operation succeeds") > 0,
            "A glob must not select another index record"
        );
    }
}
