//! Owner of the directory that holds Studio worker Git worktrees.
//!
//! A worker worktree lives at `<root>/codex-agents/<agent id>`. Without
//! configuration, `<root>` is `.worktrees` inside the worker's repository. The
//! file `worktree-root.json` in the server state directory,
//! `{"root": "/absolute/path"}`, selects one shared root for every repository,
//! for example a directory on another disk. Provisioning reads the file for
//! each new worktree, so a change applies without a restart. Cleanup accepts
//! both the configured root and the repository default, so worktrees created
//! before a change remain manageable.

use serde::Deserialize;
use std::path::{Path, PathBuf};
use studio_runtime_store::path_identity::CanonicalPath;

/// State-directory file that configures the worker worktree root.
pub const WORKTREE_ROOT_FILE: &str = "worktree-root.json";
/// Repository-relative worktree root when no root is configured.
pub const REPOSITORY_WORKTREE_ROOT: &str = ".worktrees";
/// Directory under a worktree root with one worktree per worker agent id.
pub const WORKER_DIRECTORY: &str = "codex-agents";

#[derive(Debug, thiserror::Error)]
pub enum WorktreeRootError {
    #[error("Cannot read {WORKTREE_ROOT_FILE}: {0}")]
    Read(std::io::Error),
    #[error("{WORKTREE_ROOT_FILE} must be {{\"root\": \"/absolute/path\"}}")]
    Invalid,
    #[error("The configured worktree root {0} is not an existing directory")]
    Missing(PathBuf),
}

/// The configured root as an existing canonical directory, or `None` when the
/// state directory has no configuration. An invalid file or missing directory
/// is an error, never a silent fallback to the repository default.
pub fn configured(state_dir: &Path) -> Result<Option<CanonicalPath>, WorktreeRootError> {
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Configuration {
        root: PathBuf,
    }
    let bytes = match std::fs::read(state_dir.join(WORKTREE_ROOT_FILE)) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(WorktreeRootError::Read(error)),
    };
    let configuration: Configuration =
        serde_json::from_slice(&bytes).map_err(|_| WorktreeRootError::Invalid)?;
    if !configuration.root.is_absolute() {
        return Err(WorktreeRootError::Invalid);
    }
    CanonicalPath::existing_utf8_directory(&configuration.root)
        .map(Some)
        .map_err(|_| WorktreeRootError::Missing(configuration.root))
}

/// The worktree root for `repository`: the configured root or the repository default.
pub fn root_for(configured: Option<&Path>, repository: &Path) -> PathBuf {
    configured.map_or_else(
        || repository.join(REPOSITORY_WORKTREE_ROOT),
        Path::to_path_buf,
    )
}

/// The directory that holds the worker worktrees of `repository`.
pub fn workers_directory(configured: Option<&Path>, repository: &Path) -> PathBuf {
    root_for(configured, repository).join(WORKER_DIRECTORY)
}

/// Where an owned worker worktree root sits, as found from its own path.
#[derive(Debug, PartialEq, Eq)]
pub enum OwnedRoot<'a> {
    /// Under the configured root; the repository must be read from Git.
    Configured(&'a Path),
    /// Under `<repository>/.worktrees`; the repository is the returned path.
    Repository {
        root: &'a Path,
        repository: &'a Path,
    },
}

impl<'a> OwnedRoot<'a> {
    /// The worker worktree directory.
    pub fn root(&self) -> &'a Path {
        match self {
            Self::Configured(root) | Self::Repository { root, .. } => root,
        }
    }
}

/// The ancestor of `path` that is the worktree of worker `id`, under the
/// configured root or a repository default root. `path` and `configured` must
/// be in the same form, both lexical or both canonical.
pub fn owned_root<'a>(
    path: &'a Path,
    id: &str,
    configured: Option<&Path>,
) -> Option<OwnedRoot<'a>> {
    let root = path.ancestors().find(|candidate| {
        candidate.file_name().and_then(|name| name.to_str()) == Some(id)
            && candidate
                .parent()
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some(WORKER_DIRECTORY)
    })?;
    let worktree_root = root.parent().and_then(Path::parent)?;
    if configured.is_some_and(|configured| configured == worktree_root) {
        return Some(OwnedRoot::Configured(root));
    }
    if worktree_root.file_name().and_then(|name| name.to_str()) == Some(REPOSITORY_WORKTREE_ROOT) {
        return Some(OwnedRoot::Repository {
            root,
            repository: worktree_root.parent()?,
        });
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_keeps_the_repository_default() {
        let state = tempfile::tempdir().expect("state directory");
        assert!(
            configured(state.path())
                .expect("absent file reads")
                .is_none()
        );
        assert_eq!(
            workers_directory(None, Path::new("/project")),
            Path::new("/project/.worktrees/codex-agents")
        );
    }

    #[test]
    fn configured_root_is_canonical_and_shared_by_repositories() {
        let state = tempfile::tempdir().expect("state directory");
        let disk = tempfile::tempdir().expect("root directory");
        let target = disk.path().join("roots");
        std::fs::create_dir(&target).expect("root creates");
        let link = disk.path().join("link");
        std::os::unix::fs::symlink(&target, &link).expect("link creates");
        std::fs::write(
            state.path().join(WORKTREE_ROOT_FILE),
            serde_json::json!({ "root": link }).to_string(),
        )
        .expect("configuration writes");
        let root = configured(state.path())
            .expect("configuration reads")
            .expect("root is configured");
        let canonical = CanonicalPath::existing(&target).expect("target canonicalizes");
        assert_eq!(root, canonical);
        assert_eq!(
            workers_directory(Some(&root), Path::new("/project")),
            canonical.join("codex-agents")
        );
    }

    #[test]
    fn invalid_or_missing_configured_root_fails_closed() {
        let state = tempfile::tempdir().expect("state directory");
        for body in [
            "{".to_owned(),
            r#"{"root":"relative/path"}"#.to_owned(),
            r#"{"root":"/x","extra":1}"#.to_owned(),
        ] {
            std::fs::write(state.path().join(WORKTREE_ROOT_FILE), body).expect("writes");
            assert!(matches!(
                configured(state.path()),
                Err(WorktreeRootError::Invalid)
            ));
        }
        let missing = state.path().join("absent");
        std::fs::write(
            state.path().join(WORKTREE_ROOT_FILE),
            serde_json::json!({ "root": missing }).to_string(),
        )
        .expect("writes");
        assert!(matches!(
            configured(state.path()),
            Err(WorktreeRootError::Missing(_))
        ));
    }

    #[test]
    fn ownership_accepts_both_roots_and_nothing_else() {
        let configured = Path::new("/disk/worktrees");
        assert_eq!(
            owned_root(
                Path::new("/disk/worktrees/codex-agents/a1/sub"),
                "a1",
                Some(configured)
            ),
            Some(OwnedRoot::Configured(Path::new(
                "/disk/worktrees/codex-agents/a1"
            )))
        );
        assert_eq!(
            owned_root(
                Path::new("/project/.worktrees/codex-agents/a1"),
                "a1",
                Some(configured)
            ),
            Some(OwnedRoot::Repository {
                root: Path::new("/project/.worktrees/codex-agents/a1"),
                repository: Path::new("/project"),
            })
        );
        for foreign in [
            "/disk/worktrees/codex-agents/a1",
            "/other/codex-agents/a1",
            "/disk/worktrees/a1",
            "/disk/worktrees/codex-agents/b2",
        ] {
            let accepted = if foreign == "/disk/worktrees/codex-agents/a1" {
                owned_root(Path::new(foreign), "a1", None)
            } else {
                owned_root(Path::new(foreign), "a1", Some(configured))
            };
            assert_eq!(accepted, None, "{foreign}");
        }
    }
}
