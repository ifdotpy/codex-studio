//! Host path identity: lexical stored identities and filesystem-resolved inputs.
//!
//! Stored identities deserialize without touching the filesystem. Input paths
//! resolve here, outside SQL, with separately named constructors for the existing
//! strict and missing-tail contracts. Guest paths use only `LexicalPath`.

use std::fs;
use std::io::{self, ErrorKind};
use std::path::{Component, Path, PathBuf};

/// An absolute, lexically normalized identity. Deserialization never follows links.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct LexicalPath(PathBuf);

/// An identity obtained by resolving host filesystem input through this owner.
/// Stored strings deserialize lexically: loading an identity must not read disk.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct CanonicalPath(PathBuf);

/// A best-effort host path observation, including an unresolved spelling when
/// resolution fails. Evidence is never deserialized as a proven stored identity.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Hash, serde::Serialize)]
#[serde(transparent)]
pub struct PathEvidence(PathBuf);

macro_rules! path_access {
    ($name:ident) => {
        impl $name {
            pub fn as_path(&self) -> &Path {
                &self.0
            }
            pub fn into_path_buf(self) -> PathBuf {
                self.0
            }
        }
        impl AsRef<std::ffi::OsStr> for $name {
            fn as_ref(&self) -> &std::ffi::OsStr {
                self.as_os_str()
            }
        }
        impl AsRef<Path> for $name {
            fn as_ref(&self) -> &Path {
                self.as_path()
            }
        }
        impl std::ops::Deref for $name {
            type Target = Path;
            fn deref(&self) -> &Path {
                self.as_path()
            }
        }
        impl std::borrow::Borrow<Path> for $name {
            fn borrow(&self) -> &Path {
                self.as_path()
            }
        }
        impl PartialEq<PathBuf> for $name {
            fn eq(&self, other: &PathBuf) -> bool {
                self.as_path() == other.as_path()
            }
        }
        impl PartialEq<$name> for PathBuf {
            fn eq(&self, other: &$name) -> bool {
                self.as_path() == other.as_path()
            }
        }
        impl PartialEq<&Path> for $name {
            fn eq(&self, other: &&Path) -> bool {
                self.as_path() == *other
            }
        }
        impl PartialEq<$name> for &Path {
            fn eq(&self, other: &$name) -> bool {
                *self == other.as_path()
            }
        }
        impl std::fmt::Display for $name {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                self.0.display().fmt(f)
            }
        }
    };
}
macro_rules! stored_path_deserialize {
    ($name:ident) => {
        impl<'de> serde::Deserialize<'de> for $name {
            fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
                let stored = <String as serde::Deserialize>::deserialize(deserializer)?;
                let lexical = LexicalPath::new(Path::new(&stored)).ok_or_else(|| {
                    serde::de::Error::custom("expected an absolute path without '..'")
                })?;
                Ok(Self(lexical.into_path_buf()))
            }
        }
    };
}
path_access!(LexicalPath);
path_access!(CanonicalPath);
path_access!(PathEvidence);
stored_path_deserialize!(LexicalPath);
stored_path_deserialize!(CanonicalPath);

/// Failures while making an absolute lexical input identity.
#[derive(Debug, thiserror::Error)]
pub enum AbsolutePathError {
    #[error(transparent)]
    WorkingDirectory(#[from] io::Error),
    #[error("path is not absolute")]
    RelativePath,
}

impl LexicalPath {
    /// Normalize separators and dots, refusing relative paths and parent traversal.
    #[must_use]
    pub fn new(path: &Path) -> Option<Self> {
        (path.is_absolute() && !path.components().any(|c| c == Component::ParentDir))
            .then(|| Self(path.components().collect()))
    }

    /// Absolute lexical input with parent components collapsed. This does not
    /// resolve links; use only where the caller's contract explicitly requires it.
    pub fn absolute(path: &Path) -> Result<Self, AbsolutePathError> {
        let path = collapse(&absolute(path)?);
        Self::new(&path).ok_or(AbsolutePathError::RelativePath)
    }

    /// Accept only the exact stored spelling, without normalizing evidence
    /// supplied by another component. This constructor never reads disk.
    #[must_use]
    pub fn exact(path: &Path) -> Option<Self> {
        let identity = Self::new(path)?;
        (identity.as_os_str() == path.as_os_str()).then_some(identity)
    }

    /// Append a relative workspace subpath, refusing traversal and absolute
    /// components. Guest and host stored identities share this lexical rule.
    #[must_use]
    pub fn within(root: &Path, subpath: &Path) -> Option<Self> {
        if subpath
            .components()
            .any(|part| !matches!(part, Component::Normal(_) | Component::CurDir))
        {
            return None;
        }
        let joined = subpath
            .components()
            .filter(|part| matches!(part, Component::Normal(_)))
            .fold(root.to_path_buf(), |path, part| path.join(part));
        Self::new(&joined)
    }
}

impl CanonicalPath {
    /// Strict filesystem resolution; preserves `std::fs::canonicalize` errors.
    pub fn existing(path: impl AsRef<Path>) -> io::Result<Self> {
        canonicalize(path.as_ref()).map(Self)
    }

    /// Strictly resolved existing directory.
    pub fn existing_directory(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = Self::existing(path)?;
        if !path.is_dir() {
            return Err(io::Error::new(
                ErrorKind::NotADirectory,
                "path is not a directory",
            ));
        }
        Ok(path)
    }

    /// Existing directory whose identity can be persisted as UTF-8 project text.
    pub fn existing_utf8_directory(path: impl AsRef<Path>) -> io::Result<Self> {
        let path = Self::existing_directory(path)?;
        if path.to_str().is_none() {
            return Err(io::Error::new(ErrorKind::InvalidData, "path is not UTF-8"));
        }
        Ok(path)
    }

    /// Async boundary for callers which are not already on the blocking pool.
    pub async fn existing_async(path: PathBuf) -> io::Result<Self> {
        outside_sql();
        crate::blocking_pool::spawn_blocking("runtime-store/path-identity", move || {
            Self::existing(path)
        })
        .await
        .map_err(io::Error::other)?
    }

    /// Resolve the longest existing prefix. Refuse traversal in a missing tail,
    /// dangling symlinks, relative input, and unreadable components.
    #[must_use]
    pub fn missing_tail(path: &Path) -> Option<Self> {
        outside_sql();
        if !path.is_absolute() {
            return None;
        }
        let mut missing = Vec::new();
        let mut current = path;
        loop {
            match canonicalize(current) {
                Ok(resolved) => {
                    return Some(Self(missing.iter().rev().fold(resolved, |p, n| p.join(n))));
                }
                Err(error) if error.kind() == ErrorKind::NotFound => {
                    if current.symlink_metadata().is_ok() {
                        return None;
                    }
                    let mut components = current.components();
                    match components.next_back() {
                        Some(Component::Normal(name)) => missing.push(name.to_owned()),
                        _ => return None,
                    }
                    current = components.as_path();
                }
                Err(_) => return None,
            }
        }
    }

    /// Submission-file contract: missing names are appended; other errors remain
    /// errors. Unlike `missing_tail`, a dangling link's spelling is retained.
    pub fn weak_missing_tail(path: &Path) -> io::Result<Self> {
        match canonicalize(path) {
            Ok(path) => Ok(Self(path)),
            Err(error) if error.kind() == ErrorKind::NotFound => {
                let Some(name) = path.file_name() else {
                    return Err(error);
                };
                let parent = path
                    .parent()
                    .ok_or_else(|| io::Error::other("missing parent"))?;
                Ok(Self(Self::weak_missing_tail(parent)?.join(name)))
            }
            Err(error) => Err(error),
        }
    }

    /// Project-profile contract: resolve the nearest readable ancestor, append
    /// the unresolved suffix, then collapse parent components lexically.
    pub fn ancestor_nonstrict(path: &Path) -> io::Result<Self> {
        let mut ancestor = path.to_path_buf();
        let mut suffix = Vec::new();
        loop {
            match canonicalize(&ancestor) {
                Ok(mut resolved) => {
                    for part in suffix.into_iter().rev() {
                        resolved.push(part);
                    }
                    return Ok(Self(collapse(&resolved)));
                }
                Err(error) => {
                    if ancestor.components().next_back() == Some(Component::ParentDir) {
                        suffix.push(std::ffi::OsString::from(".."));
                        ancestor.pop();
                        continue;
                    }
                    let part = ancestor.file_name().ok_or(error)?.to_os_string();
                    suffix.push(part);
                    if !ancestor.pop() {
                        return Err(io::Error::other("missing ancestor"));
                    }
                }
            }
        }
    }

    /// Directory-input contract (`Path.resolve(strict=False)`): follow links
    /// before applying `..`, including dangling links. Permission errors fail.
    pub fn nonstrict(path: &Path) -> io::Result<Self> {
        outside_sql();
        resolve_links(path, &mut 0).map(Self)
    }

    /// Account-home compatibility: dangling links are followed by their text;
    /// unreadable entries retain their spelling, matching the saved profile rule.
    #[cfg(unix)]
    pub fn profile_nonstrict(path: &Path) -> io::Result<Self> {
        outside_sql();

        use std::path::Component;

        fn walk(path: &Path, depth: usize) -> PathBuf {
            let mut resolved = PathBuf::new();
            for component in path.components() {
                match component {
                    Component::Prefix(_) | Component::RootDir => resolved.push(component),
                    Component::CurDir => {}
                    Component::ParentDir => {
                        resolved.pop();
                    }
                    Component::Normal(name) => {
                        let candidate = resolved.join(name);
                        let is_link = fs::symlink_metadata(&candidate)
                            .is_ok_and(|metadata| metadata.file_type().is_symlink());
                        resolved = if !is_link {
                            candidate
                        } else if let Ok(target) = canonicalize(&candidate) {
                            target
                        } else {
                            // A dangling link resolves through its target text.
                            match fs::read_link(&candidate) {
                                Ok(target) if depth < MAX_PROFILE_SYMLINK_DEPTH => {
                                    walk(&resolved.join(target), depth + 1)
                                }
                                _ => candidate,
                            }
                        };
                    }
                }
            }
            resolved
        }

        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir()?.join(path)
        };
        Ok(Self(walk(&absolute, 0)))
    }
    #[cfg(not(unix))]
    pub fn profile_nonstrict(path: &Path) -> io::Result<Self> {
        std::path::absolute(path).map(Self)
    }
}

impl PathEvidence {
    /// Mount evidence: resolve an existing ancestor, preserving every missing name.
    pub fn ancestor(path: &Path) -> Self {
        outside_sql();
        let absolute = if path.is_absolute() {
            path.to_path_buf()
        } else {
            std::env::current_dir().unwrap_or_default().join(path)
        };
        let mut tail = Vec::new();
        let mut current = absolute.as_path();
        loop {
            if let Ok(resolved) = CanonicalPath::existing(current).map(CanonicalPath::into_path_buf)
            {
                return Self(
                    tail.iter()
                        .rev()
                        .fold(resolved, |path: PathBuf, part| path.join(part)),
                );
            }
            match (current.parent(), current.file_name()) {
                (Some(parent), Some(name)) => {
                    tail.push(name.to_os_string());
                    current = parent;
                }
                _ => return Self(absolute),
            }
        }
    }

    /// Component-wise best effort used by message admission: failed resolutions
    /// retain the component spelling before processing later parent components.
    pub fn component_fallback(path: &Path) -> Self {
        outside_sql();
        let absolute = if path.is_absolute() {
            path.to_owned()
        } else {
            std::env::current_dir()
                .unwrap_or_else(|_| PathBuf::from("."))
                .join(path)
        };
        let mut resolved = PathBuf::new();
        for component in absolute.components() {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    resolved.pop();
                }
                Component::Normal(name) => {
                    let candidate = resolved.join(name);
                    resolved = canonicalize(&candidate).unwrap_or(candidate);
                }
                value => resolved.push(value.as_os_str()),
            }
        }
        Self(resolved)
    }
}

// Only this filesystem owner may bypass the runtime-wide Clippy prohibition.
#[allow(clippy::disallowed_methods)]
fn canonicalize(path: &Path) -> io::Result<PathBuf> {
    outside_sql();
    std::fs::canonicalize(path)
}

fn outside_sql() {
    crate::debug_assert_outside_sql();
}

fn absolute(path: &Path) -> io::Result<PathBuf> {
    if path.is_absolute() {
        Ok(path.to_owned())
    } else {
        Ok(std::env::current_dir()?.join(path))
    }
}

fn collapse(path: &Path) -> PathBuf {
    let mut result = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                result.pop();
            }
            value => result.push(value.as_os_str()),
        }
    }
    result
}

/// Maximum links followed by one directory-input traversal. Unit: links.
const MAX_SYMLINKS: usize = 40;
/// Maximum recursive dangling-link depth for profile compatibility. Unit: levels.
#[cfg(unix)]
const MAX_PROFILE_SYMLINK_DEPTH: usize = 40;
fn resolve_links(path: &Path, links: &mut usize) -> io::Result<PathBuf> {
    let absolute = absolute(path)?;
    let mut resolved = PathBuf::new();
    for component in absolute.components() {
        match component {
            Component::Prefix(prefix) => resolved.push(prefix.as_os_str()),
            Component::RootDir => resolved.push(component.as_os_str()),
            Component::CurDir => {}
            Component::ParentDir => {
                resolved.pop();
            }
            Component::Normal(name) => {
                let candidate = resolved.join(name);
                match std::fs::symlink_metadata(&candidate) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        *links += 1;
                        if *links > MAX_SYMLINKS {
                            return Err(io::Error::other("too many symbolic links"));
                        }
                        let target = std::fs::read_link(candidate)?;
                        resolved = resolve_links(&resolved.join(target), links)?;
                    }
                    Ok(_) => resolved.push(name),
                    Err(error) if error.kind() == ErrorKind::NotFound => resolved.push(name),
                    Err(error) => return Err(error),
                }
            }
        }
    }
    Ok(resolved)
}

/// Safe host resolution. New callers keep the returned identity typed.
#[must_use]
pub fn resolve(path: &Path) -> Option<CanonicalPath> {
    CanonicalPath::missing_tail(path)
}

/// Equal text needs no filesystem read; otherwise compare resolved identities.
#[must_use]
pub fn same(left: &Path, right: &Path) -> bool {
    left == right || resolve(left).is_some_and(|left| resolve(right) == Some(left))
}

#[must_use]
pub fn same_text(left: &str, right: &str) -> bool {
    same(Path::new(left), Path::new(right))
}

#[must_use]
pub fn lexical(path: &Path) -> Option<LexicalPath> {
    LexicalPath::new(path)
}

#[must_use]
pub fn lexical_text(path: &str) -> Option<LexicalPath> {
    lexical(Path::new(path))
}

/// Strict comparison: both paths must exist, even when their spelling is equal.
#[must_use]
pub fn same_existing(left: &Path, right: &Path) -> bool {
    CanonicalPath::existing(left)
        .ok()
        .zip(CanonicalPath::existing(right).ok())
        .is_some_and(|(left, right)| left == right)
}

impl PathEvidence {
    /// Removed-rollout watcher evidence: keep an unresolved spelling as an absolute
    /// path when possible. Failure to resolve does not invent a proven identity.
    #[must_use]
    pub fn resolved_or_absolute(path: &Path) -> Self {
        Self(
            resolve(path)
                .map(CanonicalPath::into_path_buf)
                .unwrap_or_else(|| absolute(path).unwrap_or_else(|_| path.to_owned())),
        )
    }

    /// Watch-directory evidence uses strict existence before its absolute fallback.
    #[must_use]
    pub fn existing_or_absolute(path: &Path) -> Self {
        Self(
            CanonicalPath::existing(path)
                .map(CanonicalPath::into_path_buf)
                .unwrap_or_else(|_| absolute(path).unwrap_or_else(|_| path.to_owned())),
        )
    }
}

impl PartialEq<LexicalPath> for CanonicalPath {
    fn eq(&self, other: &LexicalPath) -> bool {
        self.as_path() == other.as_path()
    }
}
impl PartialEq<CanonicalPath> for LexicalPath {
    fn eq(&self, other: &CanonicalPath) -> bool {
        self.as_path() == other.as_path()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::{CanonicalPath, resolve, same, same_text};
    use std::os::unix::fs::symlink;
    use std::path::Path;

    #[test]
    fn a_linked_folder_and_its_target_are_the_same_location() {
        let directory = tempfile::tempdir().expect("temporary folder");
        let root = CanonicalPath::existing(directory.path()).expect("canonical root");
        let target = root.join("target");
        std::fs::create_dir(&target).expect("target folder");
        let link = root.join("link");
        symlink(&target, &link).expect("link to the target");

        assert!(same(&link, &target));
        assert!(
            same(&link.join("work"), &target.join("work")),
            "missing child"
        );
        assert!(
            same(&link.join("a/b"), &target.join("a/b")),
            "missing children"
        );
        assert_eq!(
            resolve(&link.join("a/b")).as_deref(),
            Some(target.join("a/b").as_path())
        );
        assert!(!same(&link.join("a"), &target.join("b")));
        assert!(!same(&link, &root));
    }

    #[test]
    fn a_missing_remainder_never_leaves_the_resolved_prefix() {
        let directory = tempfile::tempdir().expect("temporary folder");
        let root = CanonicalPath::existing(directory.path()).expect("canonical root");
        let allowed = root.join("allowed");
        std::fs::create_dir(&allowed).expect("allowed folder");

        assert_eq!(resolve(&allowed.join("missing/../../outside")), None);
        assert_eq!(resolve(Path::new("relative/path")), None);
        let dangling = allowed.join("dangling");
        symlink(root.join("elsewhere"), &dangling).expect("dangling link");
        assert_eq!(resolve(&dangling.join("file")), None, "dangling link");
        assert!(!same(&dangling.join("file"), &root.join("elsewhere/file")));
        // Unresolvable paths are the same only as equal text.
        assert!(same(&dangling, &dangling));
    }

    /// The macOS default temporary folder is under the `/var` link. This is
    /// the path pair that made thread receipts stale on macOS.
    #[test]
    fn the_default_temporary_folder_matches_its_canonical_form() {
        let directory = tempfile::tempdir().expect("temporary folder");
        let canonical = CanonicalPath::existing(directory.path()).expect("canonical folder");
        let written = directory.path().to_str().expect("UTF-8 temporary folder");
        assert!(same_text(written, canonical.to_str().expect("UTF-8")));
        assert!(same(&directory.path().join("new"), &canonical.join("new")));
    }

    /// 2026-10-21: a worktree cwd was stored with a trailing slash.
    #[test]
    fn lexical_gives_one_form_without_a_read() {
        use super::lexical_text;
        for spelling in ["/a/b", "/a/b/", "/a//b", "/a/./b/", "//a/b"] {
            assert_eq!(
                lexical_text(spelling).as_deref().and_then(Path::to_str),
                Some("/a/b"),
                "{spelling}"
            );
        }
        assert_eq!(
            lexical_text("/").as_deref().and_then(Path::to_str),
            Some("/")
        );
        assert_eq!(lexical_text("a/b"), None);
        assert_eq!(lexical_text("/a/../b"), None);
    }
}
