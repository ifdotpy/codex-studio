//! Provider executable identity, selection, and version notice decisions.
//!
//! Filesystem discovery, child execution, provider sessions, and publication
//! belong to callers. This module only interprets supplied snapshots.
//! Windows npm `codex.cmd` shim resolution is an explicit unsupported outcome
//! until Windows becomes a runtime cutover target.

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::UNIX_EPOCH;
use studio_runtime_store::path_identity::CanonicalPath;

use serde::Deserialize;
use sha2::{Digest, Sha256};
use thiserror::Error;

const COMPANION_UNIX: &[&str] = &["codex-code-mode-host"];
const MAX_PACKAGE_METADATA_BYTES: usize = 1024 * 1024;
const COMPANION_WINDOWS: &[&str] = &["codex-code-mode-host.exe"];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticVersion {
    pub major: VersionNumber,
    pub minor: VersionNumber,
    pub patch: VersionNumber,
    pub prerelease: Option<Vec<PrereleasePart>>,
}

/// Non-negative decimal integer used by Python's unbounded version parser.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionNumber(String);

impl VersionNumber {
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Ord for VersionNumber {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0
            .len()
            .cmp(&other.0.len())
            .then_with(|| self.0.cmp(&other.0))
    }
}

impl PartialOrd for VersionNumber {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrereleasePart {
    Numeric(VersionNumber),
    Text(String),
}

impl Ord for SemanticVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (&self.major, &self.minor, &self.patch)
            .cmp(&(&other.major, &other.minor, &other.patch))
            .then_with(|| match (&self.prerelease, &other.prerelease) {
                (None, None) => Ordering::Equal,
                (None, Some(_)) => Ordering::Greater,
                (Some(_), None) => Ordering::Less,
                (Some(left), Some(right)) => compare_prerelease(left, right),
            })
    }
}

impl PartialOrd for SemanticVersion {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

fn compare_prerelease(left: &[PrereleasePart], right: &[PrereleasePart]) -> Ordering {
    for (left, right) in left.iter().zip(right) {
        let ordering = match (left, right) {
            (PrereleasePart::Numeric(left), PrereleasePart::Numeric(right)) => left.cmp(right),
            (PrereleasePart::Numeric(_), PrereleasePart::Text(_)) => Ordering::Less,
            (PrereleasePart::Text(_), PrereleasePart::Numeric(_)) => Ordering::Greater,
            (PrereleasePart::Text(left), PrereleasePart::Text(right)) => left.cmp(right),
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left.len().cmp(&right.len())
}

/// Finds the first `major.minor.patch` token, optionally followed by a
/// prerelease. Build metadata is intentionally ignored.
#[must_use]
pub fn parse_version(value: &str) -> Option<SemanticVersion> {
    let bytes = value.as_bytes();
    for start in 0..bytes.len() {
        if !bytes[start].is_ascii_digit() || (start > 0 && bytes[start - 1].is_ascii_digit()) {
            continue;
        }
        if let Some((version, _end)) = parse_at(bytes, start) {
            return Some(version);
        }
    }
    None
}

/// Parse the complete version token reported by the native Codex CLI.
#[must_use]
pub fn parse_native_version(value: &str) -> Option<SemanticVersion> {
    let (version, mut end) = parse_at(value.as_bytes(), 0)?;
    let bytes = value.as_bytes();
    if bytes.get(end) == Some(&b'+') {
        end += 1;
        let start = end;
        while bytes
            .get(end)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            end += 1;
        }
        if end == start {
            return None;
        }
    }
    (end == bytes.len()).then_some(version)
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DiscoveryInputs {
    pub pinned_path: Option<PathBuf>,
    pub explicit_path: Option<PathBuf>,
    pub path_lookup: Option<PathBuf>,
    pub path_entries: Vec<PathBuf>,
    pub chatgpt_bundle: Option<PathBuf>,
}

/// Return discovery paths in Python runtime precedence order, de-duplicated
/// without resolving or touching the filesystem.
#[must_use]
pub fn discovery_paths(inputs: DiscoveryInputs) -> Vec<PathBuf> {
    let paths = if let Some(pin) = inputs.pinned_path {
        vec![pin]
    } else {
        inputs
            .explicit_path
            .into_iter()
            .chain(inputs.path_lookup)
            .chain(inputs.path_entries)
            .chain(inputs.chatgpt_bundle)
            .collect()
    };
    let mut seen = std::collections::HashSet::new();
    paths
        .into_iter()
        .filter(|path| seen.insert(path.clone()))
        .collect()
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum VersionReadError {
    #[error("Native executable did not report a Codex CLI version")]
    MissingVersion,
    #[error("Native executable did not report a semantic version")]
    InvalidVersion,
}

/// Interpret captured output from a fakeable `codex --version` effect.
pub fn codex_cli_version(output: &[u8]) -> Result<String, VersionReadError> {
    let output = String::from_utf8_lossy(output);
    let fields = output.split_whitespace().collect::<Vec<_>>();
    if fields.len() != 2 || fields[0] != "codex-cli" {
        return Err(VersionReadError::MissingVersion);
    }
    if parse_native_version(fields[1]).is_none() {
        return Err(VersionReadError::InvalidVersion);
    }
    Ok(fields[1].to_owned())
}

fn parse_at(bytes: &[u8], start: usize) -> Option<(SemanticVersion, usize)> {
    let (major, next) = number(bytes, start)?;
    if bytes.get(next) != Some(&b'.') {
        return None;
    }
    let (minor, next) = number(bytes, next + 1)?;
    if bytes.get(next) != Some(&b'.') {
        return None;
    }
    let (patch, mut end) = number(bytes, next + 1)?;
    let prerelease = if bytes.get(end) == Some(&b'-') {
        let start = end + 1;
        let mut finish = start;
        while bytes
            .get(finish)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-'))
        {
            finish += 1;
        }
        if finish == start {
            return None;
        }
        end = finish;
        let suffix = std::str::from_utf8(&bytes[start..finish]).ok()?;
        let mut parts = Vec::new();
        for part in suffix.split('.') {
            parts.push(
                if !part.is_empty() && part.bytes().all(|byte| byte.is_ascii_digit()) {
                    let normalized = part.trim_start_matches('0');
                    PrereleasePart::Numeric(VersionNumber(
                        if normalized.is_empty() {
                            "0"
                        } else {
                            normalized
                        }
                        .to_owned(),
                    ))
                } else {
                    PrereleasePart::Text(part.to_owned())
                },
            );
        }
        Some(parts)
    } else {
        None
    };
    Some((
        SemanticVersion {
            major,
            minor,
            patch,
            prerelease,
        },
        end,
    ))
}

fn number(bytes: &[u8], start: usize) -> Option<(VersionNumber, usize)> {
    let mut end = start;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == start {
        return None;
    }
    let digits = std::str::from_utf8(&bytes[start..end]).ok()?;
    let normalized = digits.trim_start_matches('0');
    Some((
        VersionNumber(
            if normalized.is_empty() {
                "0"
            } else {
                normalized
            }
            .to_owned(),
        ),
        end,
    ))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum NativePlatform {
    LinuxX64,
    LinuxArm64,
    MacX64,
    MacArm64,
    WindowsX64,
    WindowsArm64,
}

impl NativePlatform {
    const fn package_name(self) -> &'static str {
        match self {
            Self::LinuxX64 => "codex-linux-x64",
            Self::LinuxArm64 => "codex-linux-arm64",
            Self::MacX64 => "codex-darwin-x64",
            Self::MacArm64 => "codex-darwin-arm64",
            Self::WindowsX64 => "codex-win32-x64",
            Self::WindowsArm64 => "codex-win32-arm64",
        }
    }

    pub const fn target_triple(self) -> &'static str {
        match self {
            Self::LinuxX64 => "x86_64-unknown-linux-musl",
            Self::LinuxArm64 => "aarch64-unknown-linux-musl",
            Self::MacX64 => "x86_64-apple-darwin",
            Self::MacArm64 => "aarch64-apple-darwin",
            Self::WindowsX64 => "x86_64-pc-windows-msvc",
            Self::WindowsArm64 => "aarch64-pc-windows-msvc",
        }
    }

    pub const fn executable_name(self) -> &'static str {
        match self {
            Self::WindowsX64 | Self::WindowsArm64 => "codex.exe",
            _ => "codex",
        }
    }

    pub const fn is_windows(self) -> bool {
        matches!(self, Self::WindowsX64 | Self::WindowsArm64)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BinaryResolutionError {
    #[error("Windows npm codex.cmd shim resolution is unsupported")]
    UnsupportedWindowsShimResolution,
    #[error("Codex npm launcher has no valid package metadata")]
    InvalidPackageMetadata,
    #[error("Codex npm launcher has an unexpected package name")]
    UnexpectedPackageName,
    #[error("Codex npm platform package has no native executable")]
    MissingPlatformExecutable,
    #[error("Codex npm command shim has no package launcher")]
    MissingPackageLauncher,
}

#[derive(Deserialize)]
struct NpmPackageMetadata {
    name: Option<String>,
}

/// Resolve an npm launcher from caller-supplied metadata and package search
/// roots. Roots are ordered like Node's upward `node_modules` lookup.
pub fn resolve_npm_launcher(
    launcher: &Path,
    metadata: &[u8],
    package_search_roots: &[PathBuf],
    platform: NativePlatform,
) -> Result<PathBuf, BinaryResolutionError> {
    let name = launcher
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or("");
    let in_bin = launcher
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        .is_some_and(|value| value.eq_ignore_ascii_case("bin"));
    let is_launcher = name.eq_ignore_ascii_case("codex.js")
        || (platform.is_windows() && name.eq_ignore_ascii_case("codex.cmd"));
    if platform.is_windows() && name.eq_ignore_ascii_case("codex.cmd") {
        return Err(BinaryResolutionError::UnsupportedWindowsShimResolution);
    }
    if !is_launcher || !in_bin {
        return Ok(launcher.to_path_buf());
    }
    if metadata.len() > MAX_PACKAGE_METADATA_BYTES {
        return Err(BinaryResolutionError::InvalidPackageMetadata);
    }
    let record: NpmPackageMetadata = serde_json::from_slice(metadata)
        .map_err(|_| BinaryResolutionError::InvalidPackageMetadata)?;
    if record.name.as_deref() != Some("@openai/codex") {
        return Err(BinaryResolutionError::UnexpectedPackageName);
    }
    let package = launcher
        .parent()
        .and_then(Path::parent)
        .unwrap_or_else(|| Path::new("."));
    let mut vendor = package.join("vendor");
    for root in package_search_roots {
        let dependency = root
            .join("node_modules")
            .join("@openai")
            .join(platform.package_name());
        if dependency.join("package.json").is_file() {
            vendor = dependency.join("vendor");
            break;
        }
    }
    let binary = vendor
        .join(platform.target_triple())
        .join("bin")
        .join(platform.executable_name());
    binary
        .is_file()
        .then_some(binary)
        .ok_or(BinaryResolutionError::MissingPlatformExecutable)
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinaryFileIdentity {
    pub path: PathBuf,
    pub size: u64,
    pub modified_ns: u128,
    pub device: Option<u64>,
    pub inode: Option<u64>,
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BinaryIdentityError {
    #[error("Native candidate is not an executable file")]
    NotExecutable,
    #[error("Native candidate lacks a required executable {0}")]
    MissingCompanion(&'static str),
    #[error("Native companion {0} is outside its source directory")]
    CompanionOutsideSource(&'static str),
    #[error("Native candidate path could not be inspected")]
    Io,
}

/// Canonicalize and snapshot a regular executable without opening or running it.
pub fn binary_file_identity(path: &Path) -> Result<BinaryFileIdentity, BinaryIdentityError> {
    let path = CanonicalPath::existing(path).map_err(|_| BinaryIdentityError::Io)?;
    let metadata = fs::metadata(&path).map_err(|_| BinaryIdentityError::Io)?;
    if !metadata.is_file() || !is_executable(&path, &metadata) {
        return Err(BinaryIdentityError::NotExecutable);
    }
    let modified_ns = metadata
        .modified()
        .map_err(|_| BinaryIdentityError::Io)?
        .duration_since(UNIX_EPOCH)
        .map_err(|_| BinaryIdentityError::Io)?
        .as_nanos();
    #[cfg(unix)]
    let (device, inode) = {
        use std::os::unix::fs::MetadataExt;
        (Some(metadata.dev()), Some(metadata.ino()))
    };
    #[cfg(not(unix))]
    let (device, inode) = (None, None);
    Ok(BinaryFileIdentity {
        path: path.to_path_buf(),
        size: metadata.len(),
        modified_ns,
        device,
        inode,
    })
}

#[cfg(unix)]
fn is_executable(_path: &Path, metadata: &fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    metadata.permissions().mode() & 0o111 != 0
}

#[cfg(windows)]
fn is_executable(path: &Path, _metadata: &fs::Metadata) -> bool {
    path.extension()
        .and_then(|value| value.to_str())
        .is_some_and(|value| {
            matches!(
                value.to_ascii_lowercase().as_str(),
                "exe" | "com" | "cmd" | "bat"
            )
        })
}

#[cfg(not(any(unix, windows)))]
fn is_executable(_path: &Path, _metadata: &fs::Metadata) -> bool {
    false
}

/// Return identities for the same-platform required companions.
pub fn companion_identities(
    path: &Path,
    platform: NativePlatform,
) -> Result<Vec<(String, BinaryFileIdentity)>, BinaryIdentityError> {
    let binary = binary_file_identity(path)?;
    let directory = binary.path.parent().ok_or(BinaryIdentityError::Io)?;
    let names = if platform.is_windows() {
        COMPANION_WINDOWS
    } else {
        COMPANION_UNIX
    };
    let mut companions = Vec::with_capacity(names.len());
    for name in names {
        let identity = binary_file_identity(&directory.join(name))
            .map_err(|_| BinaryIdentityError::MissingCompanion(name))?;
        if identity.path.parent() != Some(directory) {
            return Err(BinaryIdentityError::CompanionOutsideSource(name));
        }
        companions.push(((*name).to_owned(), identity));
    }
    Ok(companions)
}

/// Hash a bundle's name-to-SHA256 map in canonical key order.
pub fn bundle_digest(hashes: &[(String, String)]) -> Result<String, serde_json::Error> {
    let sorted = hashes
        .iter()
        .map(|(name, hash)| (name.as_str(), hash.as_str()))
        .collect::<std::collections::BTreeMap<_, _>>();
    let encoded = serde_json::to_vec(&sorted)?;
    Ok(data_encoding::HEXLOWER.encode(&Sha256::digest(encoded)))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CandidateStatus {
    Discovered,
    Rejected,
    Approved,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct BinaryCandidate {
    pub path: PathBuf,
    pub status: CandidateStatus,
    pub version: Option<String>,
    pub error: Option<String>,
}

/// Newest valid candidate first; rejected candidates follow all valid ones.
pub fn order_candidates(candidates: &mut [BinaryCandidate]) {
    candidates.sort_by(|left, right| {
        let left_valid = left.error.is_none();
        let right_valid = right.error.is_none();
        right_valid.cmp(&left_valid).then_with(|| {
            let left_version = left.version.as_deref().and_then(parse_native_version);
            let right_version = right.version.as_deref().and_then(parse_native_version);
            right_version.cmp(&left_version)
        })
    });
}

/// Preserve an existing selected binary unless a validated candidate is at
/// least as new. Ties deliberately prefer the later candidate, matching scan
/// order when the same version was rebuilt with a different bundle digest.
#[must_use]
pub fn select_approved_candidate<'a>(
    existing: Option<&'a BinaryCandidate>,
    candidates: &'a [BinaryCandidate],
) -> Option<&'a BinaryCandidate> {
    let mut selected = existing.filter(|candidate| {
        candidate.status == CandidateStatus::Approved
            && candidate
                .version
                .as_deref()
                .and_then(parse_native_version)
                .is_some()
    });
    for candidate in candidates.iter().filter(|candidate| {
        candidate.status == CandidateStatus::Approved
            && candidate
                .version
                .as_deref()
                .and_then(parse_native_version)
                .is_some()
    }) {
        let should_replace = selected.is_none_or(|current| {
            candidate.version.as_deref().and_then(parse_native_version)
                >= current.version.as_deref().and_then(parse_native_version)
        });
        if should_replace {
            selected = Some(candidate);
        }
    }
    selected
}

#[cfg(test)]
mod tests {
    use super::{
        BinaryCandidate, CandidateStatus, DiscoveryInputs, NativePlatform, SemanticVersion,
        VersionReadError, bundle_digest, codex_cli_version, companion_identities, discovery_paths,
        order_candidates, parse_native_version, parse_version, resolve_npm_launcher,
        select_approved_candidate,
    };
    use std::fs;
    use std::path::PathBuf;

    #[test]
    fn semantic_versions_order_prereleases_before_releases() {
        let alpha = parse_version("codex-cli 1.2.3-alpha.9").expect("alpha version parses");
        let beta = parse_version("1.2.3-beta.1").expect("beta version parses");
        let release = parse_version("1.2.3+build.4").expect("release version parses");
        assert!(alpha < beta);
        assert!(beta < release);
        assert_eq!(
            parse_version("v1.2.3-rc.11"),
            Some(SemanticVersion {
                major: super::VersionNumber("1".to_owned()),
                minor: super::VersionNumber("2".to_owned()),
                patch: super::VersionNumber("3".to_owned()),
                prerelease: Some(vec![
                    super::PrereleasePart::Text("rc".to_owned()),
                    super::PrereleasePart::Numeric(super::VersionNumber("11".to_owned())),
                ]),
            })
        );
    }

    #[test]
    fn malformed_versions_are_not_comparable() {
        assert!(parse_version("1.2").is_none());
        assert!(parse_version("x01.2.3").is_some());
        assert!(parse_version("999999999999999999999999.2.3").is_some());
        assert!(parse_version("1.2.3-").is_none());
    }

    #[test]
    fn candidate_order_uses_version_and_keeps_rejected_rows_last() {
        let mut candidates = vec![
            BinaryCandidate {
                path: PathBuf::from("old"),
                status: CandidateStatus::Discovered,
                version: Some("1.0.0".to_owned()),
                error: None,
            },
            BinaryCandidate {
                path: PathBuf::from("bad"),
                status: CandidateStatus::Rejected,
                version: None,
                error: Some("invalid".to_owned()),
            },
            BinaryCandidate {
                path: PathBuf::from("new"),
                status: CandidateStatus::Approved,
                version: Some("2.0.0".to_owned()),
                error: None,
            },
        ];
        order_candidates(&mut candidates);
        assert_eq!(candidates[0].path, PathBuf::from("new"));
        assert_eq!(candidates[1].path, PathBuf::from("old"));
        assert_eq!(candidates[2].path, PathBuf::from("bad"));
    }

    #[test]
    fn selection_ignores_unapproved_and_older_candidates() {
        let existing = BinaryCandidate {
            path: PathBuf::from("selected"),
            status: CandidateStatus::Approved,
            version: Some("2.0.0".to_owned()),
            error: None,
        };
        let candidates = [
            BinaryCandidate {
                path: PathBuf::from("unapproved"),
                status: CandidateStatus::Discovered,
                version: Some("9.0.0".to_owned()),
                error: None,
            },
            BinaryCandidate {
                path: PathBuf::from("older"),
                status: CandidateStatus::Approved,
                version: Some("1.0.0".to_owned()),
                error: None,
            },
        ];
        assert_eq!(
            select_approved_candidate(Some(&existing), &candidates)
                .map(|candidate| candidate.path.clone()),
            Some(PathBuf::from("selected"))
        );
    }

    #[test]
    fn npm_launcher_resolves_the_platform_bundle_without_running_it() {
        let root = tempfile::tempdir().expect("fixture root");
        let package = root.path().join("node_modules/@openai/codex");
        let launcher = package.join("bin/codex.js");
        let binary = package.join("vendor/x86_64-unknown-linux-musl/bin/codex");
        fs::create_dir_all(binary.parent().expect("binary parent")).expect("fixture directories");
        fs::write(&binary, "opaque fake executable").expect("fake binary bytes");
        let result = resolve_npm_launcher(
            &launcher,
            br#"{"name":"@openai/codex"}"#,
            &[root.path().to_path_buf()],
            NativePlatform::LinuxX64,
        )
        .expect("platform binary resolves");
        assert_eq!(result, binary);
        assert!(
            resolve_npm_launcher(
                &launcher,
                br#"{"name":"@other/codex"}"#,
                &[root.path().to_path_buf()],
                NativePlatform::LinuxX64,
            )
            .is_err()
        );
    }

    #[cfg(unix)]
    #[test]
    fn file_identity_checks_the_binary_and_required_companion_files() {
        use std::os::unix::fs::PermissionsExt;

        let root = tempfile::tempdir().expect("fixture root");
        let binary = root.path().join("codex");
        let companion = root.path().join("codex-code-mode-host");
        fs::write(&binary, "fake binary").expect("fake binary bytes");
        fs::write(&companion, "fake companion").expect("fake companion bytes");
        for path in [&binary, &companion] {
            fs::set_permissions(path, fs::Permissions::from_mode(0o700))
                .expect("fake executable permission");
        }
        let companions = companion_identities(&binary, NativePlatform::LinuxX64)
            .expect("required companion identity");
        assert_eq!(companions.len(), 1);
        assert_eq!(companions[0].0, "codex-code-mode-host");
    }

    #[test]
    fn bundle_digest_is_independent_of_input_order() {
        let forward = vec![
            ("codex".to_owned(), "a".repeat(64)),
            ("helper".to_owned(), "b".repeat(64)),
        ];
        let reverse = forward.iter().cloned().rev().collect::<Vec<_>>();
        assert_eq!(
            bundle_digest(&forward).expect("hash map serializes"),
            bundle_digest(&reverse).expect("hash map serializes")
        );
    }

    #[test]
    fn native_versions_must_be_complete_tokens() {
        assert!(parse_native_version("1.2.3").is_some());
        assert!(parse_native_version("1.2.3-beta.2+fixture").is_some());
        assert!(parse_native_version("codex-cli 1.2.3").is_none());
        assert!(parse_native_version("1.2.3 trailing").is_none());
        assert!(parse_native_version("1.2.3+").is_none());
    }

    #[test]
    fn codex_version_output_is_validated_without_executing_a_binary() {
        assert_eq!(
            codex_cli_version(b"codex-cli 1.2.3\n"),
            Ok("1.2.3".to_owned())
        );
        assert_eq!(
            codex_cli_version(b"codex-cli latest\n"),
            Err(VersionReadError::InvalidVersion)
        );
        assert_eq!(
            codex_cli_version(b"unexpected output"),
            Err(VersionReadError::MissingVersion)
        );
    }

    #[test]
    fn a_runtime_pin_is_the_only_discovery_path() {
        let paths = discovery_paths(DiscoveryInputs {
            pinned_path: Some(PathBuf::from("pin")),
            explicit_path: Some(PathBuf::from("explicit")),
            path_lookup: Some(PathBuf::from("lookup")),
            path_entries: vec![PathBuf::from("path"), PathBuf::from("lookup")],
            chatgpt_bundle: Some(PathBuf::from("bundle")),
        });
        assert_eq!(paths, vec![PathBuf::from("pin")]);
    }

    #[test]
    fn candidate_discovery_order_deduplicates_and_preserves_precedence() {
        let paths = discovery_paths(DiscoveryInputs {
            pinned_path: None,
            explicit_path: Some(PathBuf::from("explicit")),
            path_lookup: Some(PathBuf::from("lookup")),
            path_entries: vec![PathBuf::from("path"), PathBuf::from("lookup")],
            chatgpt_bundle: Some(PathBuf::from("bundle")),
        });
        assert_eq!(
            paths,
            vec![
                PathBuf::from("explicit"),
                PathBuf::from("lookup"),
                PathBuf::from("path"),
                PathBuf::from("bundle"),
            ]
        );
    }
}
