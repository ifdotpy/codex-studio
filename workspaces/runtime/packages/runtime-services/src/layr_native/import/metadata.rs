//! Recognize exact Git markers without falling back to a parent repository.
use super::*;

#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub(in super::super) struct PlainMetadata {
    pub relative_path: PathBuf,
    pub reason: String,
    pub cache: bool,
}

/// A recognized invalid marker is data. Unknown read failures remain refusals.
pub(super) fn plain_reason(directory: &Path, marker: &Path) -> Result<Option<String>> {
    let marker = marker
        .to_str()
        .ok_or_else(|| NativeLayrError::Refused("Non-UTF-8 Git marker path".into()))?;
    let output = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("git"),
        git_directory: Some(directory.as_os_str()),
        args: &["--git-dir", marker, "rev-parse", "--is-bare-repository"],
        environment: &[
            ("GIT_OPTIONAL_LOCKS", "0"),
            ("GIT_NO_LAZY_FETCH", "1"),
            ("GIT_TERMINAL_PROMPT", "0"),
            ("LC_ALL", "C"),
        ],
        input: None,
        stdout_limit: 64 * 1024,
        stderr_limit: 64 * 1024,
        timeout: std::time::Duration::from_secs(60),
    });
    match output {
        Ok(output)
            if !output.stdout.truncated
                && matches!(output.stdout.bytes.as_slice(), b"true\n" | b"false\n") =>
        {
            Ok(None)
        }
        Err(crate::bounded_child::ChildError::Failed(reason))
            if [
                "fatal: not a git repository",
                "fatal: invalid gitfile format:",
                "fatal: bad config line ",
                "fatal: bad boolean config value ",
            ]
            .iter()
            .any(|prefix| reason.starts_with(prefix)) =>
        {
            Ok(Some(reason))
        }
        result => Err(NativeLayrError::Refused(format!(
            "Cannot classify Git metadata at {marker}: {result:?}"
        ))),
    }
}

/// Native admission uses the whole project, including repositories shared by its worktrees.
pub(super) fn validate_repository(project: &Path, directory: &Path) -> Result<()> {
    object_paths(project, directory).map(|_| ())
}

pub(super) fn object_paths(project: &Path, directory: &Path) -> Result<BTreeSet<PathBuf>> {
    let marker = directory.join(".git");
    let metadata = fs::symlink_metadata(&marker)?;
    let git = if metadata.is_dir() && !metadata.file_type().is_symlink() {
        marker
    } else if metadata.is_file() && !metadata.file_type().is_symlink() {
        crate::vm_preparation::read_gitdir_pointer(directory, &marker)?.into_path_buf()
    } else {
        return Err(NativeLayrError::Refused(format!(
            "Unsupported Git metadata link: {}",
            marker.display()
        )));
    };
    let mut objects = BTreeSet::new();
    validate_git_directory(project, &git, &mut BTreeSet::new(), &mut objects)?;
    Ok(objects)
}

pub(super) fn validate_stored_git(project: &Path, git: &Path) -> Result<()> {
    validate_git_directory(project, git, &mut BTreeSet::new(), &mut BTreeSet::new())
}

fn internal(project: &Path, path: &Path) -> Result<PathBuf> {
    let canonical = fs::canonicalize(path).map_err(|error| {
        NativeLayrError::Refused(format!(
            "Cannot resolve Git store path {}: {error}",
            path.display()
        ))
    })?;
    if !canonical.starts_with(project) {
        return Err(NativeLayrError::Refused(format!(
            "Git store outside the project: {} -> {}",
            path.display(),
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn validate_git_directory(
    project: &Path,
    git: &Path,
    seen: &mut BTreeSet<PathBuf>,
    objects_seen: &mut BTreeSet<PathBuf>,
) -> Result<()> {
    let git = internal(project, git)?;
    if !seen.insert(git.clone()) {
        return Ok(());
    }
    let common = git.join("commondir");
    if common.exists() {
        let value = fs::read_to_string(internal(project, &common)?)?;
        let value = value.trim_end_matches(['\r', '\n']);
        if value.is_empty() || value.contains(['\r', '\n']) {
            return Err(NativeLayrError::Refused(format!(
                "Invalid Git commondir: {}",
                common.display()
            )));
        }
        validate_git_directory(project, &git.join(value), seen, objects_seen)?;
    } else {
        validate_objects(project, &git.join("objects"), objects_seen)?;
    }
    let modules = git.join("modules");
    if modules.is_dir() {
        let mut pending = vec![modules];
        let mut visited = BTreeSet::new();
        while let Some(folder) = pending.pop() {
            let folder = internal(project, &folder)?;
            if !visited.insert(folder.clone()) {
                continue;
            }
            if folder.join("objects").exists() || folder.join("commondir").exists() {
                validate_git_directory(project, &folder, seen, objects_seen)?;
            } else {
                for entry in fs::read_dir(folder)? {
                    let entry = entry?;
                    if entry.path().is_dir() {
                        pending.push(entry.path());
                    }
                }
            }
        }
    }
    Ok(())
}

fn validate_objects(project: &Path, objects: &Path, seen: &mut BTreeSet<PathBuf>) -> Result<()> {
    let objects = internal(project, objects)?;
    if !seen.insert(objects.clone()) {
        return Ok(());
    }
    let alternates = objects.join("info/alternates");
    if alternates.exists() {
        for alternate in fs::read_to_string(internal(project, &alternates)?)?
            .lines()
            .filter(|line| !line.is_empty())
        {
            validate_objects(project, &objects.join(alternate), seen)?;
        }
    }
    let mut pending = vec![objects];
    let mut visited = BTreeSet::new();
    while let Some(folder) = pending.pop() {
        let folder = internal(project, &folder)?;
        if !visited.insert(folder.clone()) {
            continue;
        }
        for entry in fs::read_dir(folder)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            let path = if kind.is_symlink() {
                let target = internal(project, &entry.path())?;
                seen.insert(target.clone());
                target
            } else {
                entry.path()
            };
            if kind.is_dir() || (kind.is_symlink() && path.is_dir()) {
                pending.push(path);
            }
        }
    }
    Ok(())
}
