//! Submodule data must remain recoverable before a forced worktree removal.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Component, Path, PathBuf};
use studio_runtime_store::path_identity::CanonicalPath;

use super::git_output;

fn unreadable(path: &Path) -> String {
    format!("submodule state at {} cannot be read", path.display())
}

fn git(path: &Path, args: &[&str]) -> Result<String, String> {
    String::from_utf8(git_output(path, args, None).map_err(|_| unreadable(path))?)
        .map_err(|_| unreadable(path))
}

fn metadata(path: &Path) -> Result<Option<std::fs::Metadata>, String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => Ok(Some(metadata)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(_) => Err(unreadable(path)),
    }
}

fn config(path: &Path, file: &Path) -> Result<Vec<(String, String)>, String> {
    let Some(metadata) = metadata(file)? else {
        return Ok(Vec::new());
    };
    if !metadata.file_type().is_file() {
        return Err(unreadable(file));
    }
    let text = git(
        path,
        &[
            "config",
            "--file",
            file.to_str().ok_or_else(|| unreadable(file))?,
            "--no-includes",
            "--null",
            "--list",
        ],
    )?;
    Ok(text
        .split('\0')
        .filter(|entry| !entry.is_empty())
        .map(|entry| {
            let (key, value) = entry.split_once('\n').unwrap_or((entry, ""));
            (key.to_owned(), value.to_owned())
        })
        .collect())
}

fn module_paths(parent: &Path) -> Result<BTreeMap<String, String>, String> {
    let mut configured = BTreeSet::new();
    for (key, value) in config(parent, &parent.join(".gitmodules"))? {
        if key.starts_with("submodule.") && key.ends_with(".path") && !configured.insert(value) {
            return Err(unreadable(&parent.join(".gitmodules")));
        }
    }
    let index = git(parent, &["ls-files", "--stage", "-z"])?;
    let mut modules = BTreeMap::new();
    for entry in index.split('\0').filter(|entry| !entry.is_empty()) {
        let (header, path) = entry.split_once('\t').ok_or_else(|| unreadable(parent))?;
        let mut fields = header.split(' ');
        if fields.next() != Some("160000") {
            continue;
        }
        let commit = fields.next().ok_or_else(|| unreadable(parent))?;
        if fields.next() != Some("0") || !configured.remove(path) {
            return Err(format!(
                "submodule {} has an unmerged or unconfigured Git link",
                parent.join(path).display()
            ));
        }
        modules.insert(path.to_owned(), commit.to_owned());
    }
    if !configured.is_empty() {
        return Err(format!(
            "submodule configuration at {} does not match the index",
            parent.display()
        ));
    }
    Ok(modules)
}

// Inspect components separately: canonicalization alone would accept a symlink
// to a different checkout inside the same parent directory.
fn checkout(parent: &Path, relative: &str) -> Result<Option<PathBuf>, String> {
    let mut path = parent.to_path_buf();
    if relative.is_empty() {
        return Err(unreadable(parent));
    }
    for component in Path::new(relative).components() {
        let Component::Normal(name) = component else {
            return Err(unreadable(&parent.join(relative)));
        };
        if name
            .to_str()
            .is_some_and(|name| name.eq_ignore_ascii_case(".git"))
        {
            return Err(unreadable(&parent.join(relative)));
        }
        path.push(name);
        let Some(metadata) = metadata(&path)? else {
            return Ok(None);
        };
        if !metadata.file_type().is_dir() {
            return Err(unreadable(&path));
        }
    }
    Ok(Some(path))
}

fn own_gitdir(parent: &Path, module: &Path) -> Result<PathBuf, String> {
    let foreign = || {
        format!(
            "submodule {} has a foreign Git directory or configuration",
            module.display()
        )
    };
    let link_path = module.join(".git");
    if !metadata(&link_path)?.is_some_and(|meta| meta.file_type().is_file()) {
        return Err(foreign());
    }
    let link = std::fs::read_to_string(&link_path).map_err(|_| unreadable(module))?;
    let destination = link
        .trim_end()
        .strip_prefix("gitdir: ")
        .filter(|value| !value.is_empty())
        .ok_or_else(foreign)?;
    let gitdir =
        CanonicalPath::existing(module.join(destination)).map_err(|_| unreadable(module))?;
    let parent_gitdir = PathBuf::from(git(parent, &["rev-parse", "--absolute-git-dir"])?.trim());
    let parent_gitdir = CanonicalPath::existing(parent_gitdir).map_err(|_| unreadable(parent))?;
    if !gitdir.starts_with(parent_gitdir.join("modules"))
        || metadata(&gitdir.join("commondir"))?.is_some()
    {
        return Err(foreign());
    }
    for file in ["config", "config.worktree"] {
        for (key, _) in config(parent, &gitdir.join(file))? {
            if key == "include.path" || key.starts_with("includeif.") {
                return Err(foreign());
            }
        }
    }
    if !metadata(&gitdir.join("objects"))?.is_some_and(|meta| meta.file_type().is_dir()) {
        return Err(foreign());
    }
    // Alternate object stores can disappear independently of this checkout.
    for name in ["objects/info/alternates", "objects/info/http-alternates"] {
        if let Some(meta) = metadata(&gitdir.join(name))?
            && (!meta.file_type().is_file() || meta.len() != 0)
        {
            return Err(foreign());
        }
    }
    let actual = PathBuf::from(git(module, &["rev-parse", "--absolute-git-dir"])?.trim());
    let top = PathBuf::from(git(module, &["rev-parse", "--show-toplevel"])?.trim());
    if CanonicalPath::existing(actual).map_err(|_| unreadable(module))? != gitdir
        || CanonicalPath::existing(top).map_err(|_| unreadable(module))? != module
    {
        return Err(foreign());
    }
    Ok(gitdir.into_path_buf())
}

// The parent archive stores a Git link, not the submodule commit's objects.
// Even a recorded HEAD needs an independent copy before its object store disappears.
fn recorded_commit_survives(
    module: &Path,
    recorded: &str,
    main_parent: &Path,
    relative: &str,
) -> Result<(), String> {
    let unpublished = git(
        module,
        &[
            "rev-list",
            "--max-count=1",
            recorded,
            "--not",
            "--remotes",
            "--",
        ],
    )?;
    if unpublished.is_empty() {
        return Ok(());
    }
    if let Some(main_module) = checkout(main_parent, relative)?
        && metadata(&main_module.join(".git"))?.is_some()
    {
        own_gitdir(main_parent, &main_module)?;
        if git_output(
            &main_module,
            &["cat-file", "-e", &format!("{recorded}^{{commit}}")],
            None,
        )
        .is_ok()
        {
            return Ok(());
        }
    }
    Err(format!(
        "submodule {} has a recorded commit without a remote ref or a verified copy in the main checkout",
        module.display()
    ))
}

pub(super) fn inspect(root: &Path, repository: &Path) -> Result<(), String> {
    let mut pending = vec![(root.to_path_buf(), repository.to_path_buf())];
    let mut visited = BTreeSet::new();
    while let Some((parent, main_parent)) = pending.pop() {
        for (relative, recorded) in module_paths(&parent)? {
            let Some(module) = checkout(&parent, &relative)? else {
                continue;
            };
            if metadata(&module.join(".git"))?.is_none() {
                let mut entries = std::fs::read_dir(&module).map_err(|_| unreadable(&module))?;
                if entries.next().is_none() {
                    continue; // An empty directory is an uninitialized submodule.
                }
                return Err(format!(
                    "submodule {} contains files but has no Git directory",
                    module.display()
                ));
            }
            let gitdir = own_gitdir(&parent, &module)?;
            if !visited.insert(gitdir) {
                return Err(format!(
                    "submodule {} repeats a Git directory",
                    module.display()
                ));
            }
            // Explicitly override ignore=all, ignore=dirty, and status.showUntrackedFiles.
            let status = git(
                &module,
                &[
                    "status",
                    "--porcelain=v1",
                    "-z",
                    "--ignore-submodules=none",
                    "--untracked-files=all",
                ],
            )?;
            if !status.is_empty() {
                return Err(format!(
                    "submodule {} has modified or untracked files",
                    module.display()
                ));
            }
            let head = git(&module, &["rev-parse", "--verify", "HEAD"])?;
            if head.trim() != recorded {
                return Err(format!(
                    "submodule {} has a checkout that differs from its recorded commit",
                    module.display()
                ));
            }
            // A clean HEAD can hide an unpublished local branch, tag, stash, or reflog.
            // Remote-tracking refs are local evidence; cleanup never fetches.
            let local = git(
                &module,
                &[
                    "rev-list",
                    "--max-count=1",
                    "HEAD",
                    "--all",
                    "--reflog",
                    "--not",
                    &recorded,
                    "--remotes",
                    "--",
                ],
            )?;
            if !local.is_empty() {
                return Err(format!(
                    "submodule {} has local commits outside its recorded and remote refs",
                    module.display()
                ));
            }
            recorded_commit_survives(&module, &recorded, &main_parent, &relative)?;
            pending.push((module, main_parent.join(relative)));
        }
    }
    Ok(())
}
