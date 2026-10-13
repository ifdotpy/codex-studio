//! Provider executable identity, selection, and version notice decisions.
//!
//! Filesystem discovery, child execution, provider sessions, and publication
//! belong to callers. This module only interprets supplied snapshots.

use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;
use std::time::UNIX_EPOCH;

use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use thiserror::Error;

const COMPANION_UNIX: &[&str] = &["codex-code-mode-host"];
const COMPANION_WINDOWS: &[&str] = &["codex-code-mode-host.exe"];
const UPSTREAM_CHECK_INTERVAL_SECONDS: f64 = 86_400.0;
const UPSTREAM_REQUEST_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_REGISTRY_RESPONSE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PACKAGE_METADATA_BYTES: usize = 1024 * 1024;
const UPSTREAM_PACKAGES: &[(&str, &str, &str)] = &[
    (
        "claude",
        "@anthropic-ai/claude-code",
        "https://registry.npmjs.org/@anthropic-ai%2fclaude-code",
    ),
    (
        "codex",
        "@openai/codex",
        "https://registry.npmjs.org/@openai%2fcodex",
    ),
];

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct SemanticVersion {
    pub major: u64,
    pub minor: u64,
    pub patch: u64,
    pub prerelease: Option<Vec<PrereleasePart>>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PrereleasePart {
    Numeric(u64),
    Text(String),
}

impl Ord for SemanticVersion {
    fn cmp(&self, other: &Self) -> Ordering {
        (self.major, self.minor, self.patch)
            .cmp(&(other.major, other.minor, other.patch))
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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClaudeVersionError {
    ExecutableNotFound,
    CommandFailed,
    InvalidVersion,
}

impl ClaudeVersionError {
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::ExecutableNotFound => "Claude Code executable was not found for this profile.",
            Self::CommandFailed => "Claude Code --version failed.",
            Self::InvalidVersion => "Claude Code did not report a recognized semantic version.",
        }
    }
}

/// Interpret captured stdout and process status from an injected Claude CLI
/// version probe. Output is truncated to the same 200-character display bound.
pub fn claude_cli_version(
    executable_found: bool,
    command_succeeded: bool,
    output: &[u8],
) -> Result<String, ClaudeVersionError> {
    if !executable_found {
        return Err(ClaudeVersionError::ExecutableNotFound);
    }
    if !command_succeeded {
        return Err(ClaudeVersionError::CommandFailed);
    }
    let output = String::from_utf8_lossy(output);
    let version = output.trim().chars().take(200).collect::<String>();
    if parse_version(&version).is_none() {
        return Err(ClaudeVersionError::InvalidVersion);
    }
    Ok(version)
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
                    PrereleasePart::Numeric(part.parse().ok()?)
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

fn number(bytes: &[u8], start: usize) -> Option<(u64, usize)> {
    let mut end = start;
    while bytes.get(end).is_some_and(u8::is_ascii_digit) {
        end += 1;
    }
    if end == start {
        return None;
    }
    let value = std::str::from_utf8(&bytes[start..end]).ok()?.parse().ok()?;
    Some((value, end))
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Provider {
    Codex,
    Claude,
}

impl Provider {
    #[must_use]
    pub const fn key(self) -> &'static str {
        match self {
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    #[must_use]
    pub const fn label(self) -> &'static str {
        match self {
            Self::Codex => "Codex CLI",
            Self::Claude => "Claude Code",
        }
    }

    #[must_use]
    pub const fn tested_baseline(self) -> &'static str {
        match self {
            Self::Codex => "0.153.4",
            Self::Claude => "2.1.278",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionSource {
    Installed,
    Upstream,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct UpdateNotice {
    pub available_version: String,
    pub source: VersionSource,
}

/// A notice is produced only when both versions are readable and the supplied
/// available version sorts after the installed version.
#[must_use]
pub fn compare_versions(
    installed: Option<&str>,
    available: Option<&str>,
    source: VersionSource,
) -> Option<UpdateNotice> {
    let installed = parse_version(installed?)?;
    let available_version = available?;
    let available_key = parse_version(available_version)?;
    (available_key > installed).then(|| UpdateNotice {
        available_version: available_version.to_owned(),
        source,
    })
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum VersionEvidence {
    Running,
    LaunchedExecutable,
    ConfiguredProfile,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct VersionWarning {
    pub provider: Provider,
    pub version: String,
    pub baseline: &'static str,
    pub message: String,
}

#[must_use]
pub fn version_warning(
    provider: Provider,
    version: &str,
    evidence: VersionEvidence,
) -> Option<VersionWarning> {
    let current = parse_version(version)?;
    let baseline = provider.tested_baseline();
    if current >= parse_version(baseline)? {
        return None;
    }
    let detail = match evidence {
        VersionEvidence::Running => format!(
            "The running {} version {version} is below",
            provider.label()
        ),
        VersionEvidence::LaunchedExecutable => format!(
            "The executable configured for this {} connection currently reports {version}, below",
            provider.label()
        ),
        VersionEvidence::ConfiguredProfile => format!(
            "The current {} profile executable reports {version}, below",
            provider.label()
        ),
    };
    let running_caveat = if evidence == VersionEvidence::Running {
        ""
    } else {
        " The exact version already running may be different."
    };
    Some(VersionWarning {
        provider,
        version: version.to_owned(),
        baseline,
        message: format!(
            "{detail} this repository's lowest recorded tested version ({baseline}).{running_caveat} It may work poorly or fail. You can continue at your own risk."
        ),
    })
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderVersionSnapshot {
    account_key: String,
    provider: Provider,
    running_version: Option<String>,
    installed_version: Option<String>,
    configured_version: Option<String>,
    available_version: Option<String>,
    upstream_version: Option<String>,
    error: Option<String>,
    running_note: Option<String>,
    at: f64,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderVersionInput {
    pub account_key: String,
    pub provider: Provider,
    pub running_version: Option<String>,
    pub installed_version: Option<String>,
    pub configured_version: Option<String>,
    pub available_version: Option<String>,
    pub upstream_version: Option<String>,
    pub error: Option<String>,
    pub running_note: Option<String>,
    pub at: f64,
}

#[derive(Clone, Copy, Debug, Error, Eq, PartialEq)]
pub enum ProviderVersionSnapshotError {
    #[error("provider version snapshot has an empty account key")]
    EmptyAccountKey,
    #[error("provider version snapshot has an invalid timestamp")]
    InvalidTimestamp,
}

impl ProviderVersionSnapshot {
    pub fn new(input: ProviderVersionInput) -> Result<Self, ProviderVersionSnapshotError> {
        if input.account_key.is_empty() {
            return Err(ProviderVersionSnapshotError::EmptyAccountKey);
        }
        if !input.at.is_finite() {
            return Err(ProviderVersionSnapshotError::InvalidTimestamp);
        }
        Ok(Self {
            account_key: input.account_key,
            provider: input.provider,
            running_version: input.running_version,
            installed_version: input.installed_version,
            configured_version: input.configured_version,
            available_version: input.available_version,
            upstream_version: input.upstream_version,
            error: input.error,
            running_note: input.running_note,
            at: input.at,
        })
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CheckStatus {
    Current,
    Outdated,
    Error,
    Unknown,
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderVersionDecision {
    pub id: String,
    pub account_key: String,
    pub provider: Provider,
    pub status: CheckStatus,
    pub running_version: Option<String>,
    pub installed_version: Option<String>,
    pub configured_version: Option<String>,
    pub notices: Vec<UpdateNotice>,
    pub baseline: &'static str,
    pub error: Option<String>,
    pub message: Option<String>,
    pub at: f64,
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

    const fn target_triple(self) -> &'static str {
        match self {
            Self::LinuxX64 => "x86_64-unknown-linux-musl",
            Self::LinuxArm64 => "aarch64-unknown-linux-musl",
            Self::MacX64 => "x86_64-apple-darwin",
            Self::MacArm64 => "aarch64-apple-darwin",
            Self::WindowsX64 => "x86_64-pc-windows-msvc",
            Self::WindowsArm64 => "aarch64-pc-windows-msvc",
        }
    }

    const fn executable_name(self) -> &'static str {
        match self {
            Self::WindowsX64 | Self::WindowsArm64 => "codex.exe",
            _ => "codex",
        }
    }

    const fn is_windows(self) -> bool {
        matches!(self, Self::WindowsX64 | Self::WindowsArm64)
    }
}

#[derive(Debug, Error, Eq, PartialEq)]
pub enum BinaryResolutionError {
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
    let path = path.canonicalize().map_err(|_| BinaryIdentityError::Io)?;
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
        path,
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

/// Project one connection snapshot into the stable diagnostics shape.
#[must_use]
pub fn decide_provider_version(snapshot: ProviderVersionSnapshot) -> ProviderVersionDecision {
    let provider = snapshot.provider;
    let mut notices = Vec::new();
    if let Some(notice) = compare_versions(
        snapshot.installed_version.as_deref(),
        snapshot.available_version.as_deref(),
        VersionSource::Installed,
    ) {
        notices.push(notice);
    }
    if let Some(notice) = compare_versions(
        snapshot.installed_version.as_deref(),
        snapshot.upstream_version.as_deref(),
        VersionSource::Upstream,
    ) {
        notices.push(notice);
    }
    let warning = snapshot
        .running_version
        .as_deref()
        .and_then(|version| version_warning(provider, version, VersionEvidence::Running))
        .or_else(|| {
            snapshot.installed_version.as_deref().and_then(|version| {
                version_warning(provider, version, VersionEvidence::LaunchedExecutable)
            })
        })
        .or_else(|| {
            snapshot.configured_version.as_deref().and_then(|version| {
                version_warning(provider, version, VersionEvidence::ConfiguredProfile)
            })
        });
    let baseline_key = parse_version(provider.tested_baseline());
    let outdated = [
        snapshot.running_version.as_deref(),
        snapshot.installed_version.as_deref(),
        snapshot.configured_version.as_deref(),
    ]
    .into_iter()
    .flatten()
    .filter_map(parse_version)
    .any(|version| {
        baseline_key
            .as_ref()
            .is_some_and(|baseline| version < *baseline)
    });
    let status = if outdated {
        CheckStatus::Outdated
    } else if snapshot.running_version.is_some() {
        CheckStatus::Current
    } else if snapshot.error.is_some()
        && snapshot.installed_version.is_none()
        && snapshot.configured_version.is_none()
    {
        CheckStatus::Error
    } else {
        CheckStatus::Unknown
    };
    let message = warning
        .map(|warning| warning.message)
        .or(snapshot.running_note)
        .or_else(|| {
            matches!(status, CheckStatus::Unknown | CheckStatus::Error)
                .then(|| "Could not determine a provider CLI version.".to_owned())
        });
    ProviderVersionDecision {
        id: format!("provider-version:{}", snapshot.account_key),
        account_key: snapshot.account_key,
        provider,
        status,
        running_version: snapshot.running_version,
        installed_version: snapshot.installed_version,
        configured_version: snapshot.configured_version,
        notices,
        baseline: provider.tested_baseline(),
        error: snapshot.error,
        message,
        at: snapshot.at,
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RegistryResponse {
    pub status: u16,
    pub headers: BTreeMap<String, String>,
    pub body: Vec<u8>,
}

pub trait RegistryRequest {
    fn get(
        &mut self,
        url: &str,
        etag: Option<&str>,
        timeout: Duration,
    ) -> Result<RegistryResponse, RegistryRequestError>;
}

impl<F> RegistryRequest for F
where
    F: FnMut(&str, Option<&str>, Duration) -> Result<RegistryResponse, RegistryRequestError>,
{
    fn get(
        &mut self,
        url: &str,
        etag: Option<&str>,
        timeout: Duration,
    ) -> Result<RegistryResponse, RegistryRequestError> {
        self(url, etag, timeout)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct RegistryRequestError;

/// Production unauthenticated npm metadata request. Tests inject a fake
/// `RegistryRequest` and never construct this adapter.
#[derive(Default)]
pub struct UreqRegistryRequest;

impl RegistryRequest for UreqRegistryRequest {
    fn get(
        &mut self,
        url: &str,
        etag: Option<&str>,
        timeout: Duration,
    ) -> Result<RegistryResponse, RegistryRequestError> {
        let config = ureq::Agent::config_builder()
            .http_status_as_error(false)
            .max_redirects(0)
            .timeout_global(Some(timeout))
            .build();
        let agent = ureq::Agent::new_with_config(config);
        let mut request = ureq::http::Request::builder()
            .method("GET")
            .uri(url)
            .header("Accept", "application/vnd.npm.install-v1+json");
        if let Some(etag) = etag {
            request = request.header("If-None-Match", etag);
        }
        let request = request.body(Vec::new()).map_err(|_| RegistryRequestError)?;
        let response = agent.run(request).map_err(|_| RegistryRequestError)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .filter_map(|(name, value)| {
                value
                    .to_str()
                    .ok()
                    .map(|value| (name.as_str().to_owned(), value.to_owned()))
            })
            .collect();
        let body = response
            .into_body()
            .into_with_config()
            .limit(MAX_REGISTRY_RESPONSE_BYTES as u64)
            .read_to_vec()
            .map_err(|_| RegistryRequestError)?;
        Ok(RegistryResponse {
            status,
            headers,
            body,
        })
    }
}

#[derive(Debug, Error)]
pub enum UpstreamReleaseError {
    #[error("provider release cache could not be read or written")]
    Io(#[from] std::io::Error),
    #[error("provider release cache could not be serialized")]
    Serialize(#[from] serde_json::Error),
    #[error("provider release cache has an invalid root shape")]
    InvalidCacheShape,
}

/// Cache one latest npm tag per provider with ETag validation and a daily
/// attempt limit. The enabled flag is supplied from the existing preference.
pub struct UpstreamReleaseChecker {
    path: PathBuf,
    packages: Value,
}

impl UpstreamReleaseChecker {
    pub fn open(path: PathBuf) -> Self {
        let packages = fs::read(&path)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .and_then(|document| document.get("packages").cloned())
            .filter(Value::is_object)
            .unwrap_or_else(|| json!({}));
        Self { path, packages }
    }

    pub fn check<R: RegistryRequest>(
        &mut self,
        enabled: bool,
        now: f64,
        request: &mut R,
    ) -> Result<BTreeMap<String, String>, UpstreamReleaseError> {
        let mut latest = BTreeMap::new();
        if !enabled {
            return Ok(latest);
        }
        let mut changed = false;
        for (provider, package, url) in UPSTREAM_PACKAGES {
            let entry = self
                .packages
                .as_object_mut()
                .ok_or(UpstreamReleaseError::InvalidCacheShape)?
                .entry((*package).to_owned())
                .or_insert_with(|| json!({}));
            if !entry.is_object() {
                *entry = json!({});
            }
            let last_attempt = entry.get("lastAttemptAt").and_then(Value::as_f64);
            if last_attempt.is_some_and(|at| now - at < UPSTREAM_CHECK_INTERVAL_SECONDS) {
                if let Some(version) = cached_upstream_version(entry, now) {
                    latest.insert((*provider).to_owned(), version);
                }
                continue;
            }

            let etag = entry.get("etag").and_then(Value::as_str).map(str::to_owned);
            entry["lastAttemptAt"] = json!(now);
            changed = true;
            let response = request.get(url, etag.as_deref(), UPSTREAM_REQUEST_TIMEOUT);
            let Ok(response) = response else {
                continue;
            };
            if response.body.len() > MAX_REGISTRY_RESPONSE_BYTES {
                continue;
            }
            let response_etag = header_value(&response.headers, "etag");
            let version = match response.status {
                304 => {
                    let Some(version) = entry
                        .get("version")
                        .and_then(Value::as_str)
                        .map(str::to_owned)
                    else {
                        continue;
                    };
                    if parse_version(&version).is_none() {
                        continue;
                    }
                    entry["fetchedAt"] = json!(now);
                    if let Some(value) = response_etag {
                        entry["etag"] = json!(value);
                    }
                    Some(version)
                }
                200 => {
                    let document = serde_json::from_slice::<Value>(&response.body).ok();
                    let version = document
                        .as_ref()
                        .and_then(|document| document.get("dist-tags"))
                        .and_then(|tags| tags.get("latest"))
                        .and_then(Value::as_str);
                    let Some(version) = version.filter(|version| parse_version(version).is_some())
                    else {
                        continue;
                    };
                    entry["version"] = json!(version);
                    entry["fetchedAt"] = json!(now);
                    if let Some(value) = response_etag {
                        entry["etag"] = json!(value);
                    }
                    Some(version.to_owned())
                }
                _ => None,
            };
            if let Some(version) = version {
                latest.insert((*provider).to_owned(), version);
            }
        }
        if changed {
            self.save()?;
        }
        Ok(latest)
    }

    fn save(&self) -> Result<(), UpstreamReleaseError> {
        let parent = self.path.parent().unwrap_or_else(|| Path::new("."));
        fs::create_dir_all(parent)?;
        let encoded = serde_json::to_vec(&json!({ "version": 1, "packages": self.packages }))?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)?;
        temporary.write_all(&encoded)?;
        temporary.as_file().sync_all()?;
        temporary.persist(&self.path).map_err(|error| error.error)?;
        Ok(())
    }
}

fn cached_upstream_version(entry: &Value, now: f64) -> Option<String> {
    let version = entry.get("version")?.as_str()?;
    let fetched_at = entry.get("fetchedAt")?.as_f64()?;
    (now - fetched_at < UPSTREAM_CHECK_INTERVAL_SECONDS && parse_version(version).is_some())
        .then(|| version.to_owned())
}

fn header_value<'a>(headers: &'a BTreeMap<String, String>, name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}

#[cfg(test)]
mod tests {
    use super::{
        BinaryCandidate, CandidateStatus, CheckStatus, ClaudeVersionError, DiscoveryInputs,
        NativePlatform, Provider, ProviderVersionInput, ProviderVersionSnapshot,
        ProviderVersionSnapshotError, RegistryResponse, SemanticVersion, UpstreamReleaseChecker,
        VersionEvidence, VersionReadError, VersionSource, bundle_digest, claude_cli_version,
        codex_cli_version, companion_identities, compare_versions, decide_provider_version,
        discovery_paths, order_candidates, parse_native_version, parse_version,
        resolve_npm_launcher, select_approved_candidate, version_warning,
    };
    use serde_json::Value;
    use std::collections::BTreeMap;
    use std::fs;
    use std::path::PathBuf;
    use std::time::Duration;

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
                major: 1,
                minor: 2,
                patch: 3,
                prerelease: Some(vec![
                    super::PrereleasePart::Text("rc".to_owned()),
                    super::PrereleasePart::Numeric(11),
                ]),
            })
        );
    }

    #[test]
    fn malformed_or_overflowing_versions_are_not_comparable() {
        assert!(parse_version("1.2").is_none());
        assert!(parse_version("x01.2.3").is_some());
        assert!(parse_version("999999999999999999999999.2.3").is_none());
        assert!(parse_version("1.2.3-").is_none());
    }

    #[test]
    fn notices_require_a_newer_available_version() {
        assert!(compare_versions(Some("1.0.0"), Some("1.0.0"), VersionSource::Installed).is_none());
        assert!(compare_versions(Some("1.0.0"), Some("bad"), VersionSource::Upstream).is_none());
        assert_eq!(
            compare_versions(Some("1.0.0"), Some("1.1.0"), VersionSource::Upstream)
                .expect("newer upstream version")
                .available_version,
            "1.1.0"
        );
    }

    #[test]
    fn warning_distinguishes_a_running_cli_from_a_selected_executable() {
        let running = version_warning(Provider::Codex, "0.153.3", VersionEvidence::Running)
            .expect("version is below tested baseline");
        assert!(
            !running
                .message
                .contains("exact version already running may be different")
        );
        let selected = version_warning(
            Provider::Codex,
            "0.153.3",
            VersionEvidence::LaunchedExecutable,
        )
        .expect("version is below tested baseline");
        assert!(
            selected
                .message
                .contains("exact version already running may be different")
        );
        assert!(version_warning(Provider::Codex, "0.153.4", VersionEvidence::Running).is_none());
    }

    #[test]
    fn decision_prefers_running_warning_and_projects_notices_in_source_order() {
        let result = decide_provider_version(
            ProviderVersionSnapshot::new(ProviderVersionInput {
                account_key: "default".to_owned(),
                provider: Provider::Codex,
                running_version: Some("0.153.3".to_owned()),
                installed_version: Some("0.153.5".to_owned()),
                configured_version: None,
                available_version: Some("0.153.6".to_owned()),
                upstream_version: Some("0.153.7".to_owned()),
                error: None,
                running_note: None,
                at: 1_800_000_000.0,
            })
            .expect("valid version snapshot"),
        );
        assert_eq!(result.status, CheckStatus::Outdated);
        assert_eq!(
            result.message.as_deref(),
            Some(
                "The running Codex CLI version 0.153.3 is below this repository's lowest recorded tested version (0.153.4). It may work poorly or fail. You can continue at your own risk."
            )
        );
        assert_eq!(result.notices.len(), 2);
        assert_eq!(result.notices[0].source, VersionSource::Installed);
        assert_eq!(result.notices[1].source, VersionSource::Upstream);
    }

    #[test]
    fn checked_version_snapshot_refuses_empty_identity_and_nonfinite_time() {
        let mut input = ProviderVersionInput {
            account_key: String::new(),
            provider: Provider::Codex,
            running_version: None,
            installed_version: None,
            configured_version: None,
            available_version: None,
            upstream_version: None,
            error: None,
            running_note: None,
            at: 1.0,
        };
        assert_eq!(
            ProviderVersionSnapshot::new(input.clone()).err(),
            Some(ProviderVersionSnapshotError::EmptyAccountKey)
        );
        input.account_key = "default".to_owned();
        input.at = f64::NAN;
        assert_eq!(
            ProviderVersionSnapshot::new(input).err(),
            Some(ProviderVersionSnapshotError::InvalidTimestamp)
        );
    }

    #[test]
    fn unknown_running_version_retains_a_known_profile_error() {
        let result = decide_provider_version(
            ProviderVersionSnapshot::new(ProviderVersionInput {
                account_key: "worker-1".to_owned(),
                provider: Provider::Claude,
                running_version: None,
                installed_version: None,
                configured_version: None,
                available_version: None,
                upstream_version: None,
                error: Some("Could not read the Claude Code executable version.".to_owned()),
                running_note: Some(
                    "The exact version already running in Claude sessions is unknown.".to_owned(),
                ),
                at: 1_800_000_000.0,
            })
            .expect("valid version snapshot"),
        );
        assert_eq!(result.status, CheckStatus::Error);
        assert_eq!(result.account_key, "worker-1");
        assert!(
            result
                .message
                .unwrap_or_default()
                .contains("exact version already running")
        );
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
    fn claude_version_output_uses_profile_presence_and_exit_status() {
        assert_eq!(
            claude_cli_version(false, true, b"Claude Code 2.1.278"),
            Err(ClaudeVersionError::ExecutableNotFound)
        );
        assert_eq!(
            claude_cli_version(true, false, b"Claude Code 2.1.278"),
            Err(ClaudeVersionError::CommandFailed)
        );
        assert_eq!(
            claude_cli_version(true, true, b" Claude Code v2.1.278 (Claude Code)\n"),
            Ok("Claude Code v2.1.278 (Claude Code)".to_owned())
        );
        assert_eq!(
            claude_cli_version(true, true, b"latest"),
            Err(ClaudeVersionError::InvalidVersion)
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

    #[test]
    fn upstream_checker_uses_only_the_injected_request_and_persists_latest_tags() {
        let root = tempfile::tempdir().expect("fixture root");
        let cache = root.path().join("provider-upstream-releases-v1.json");
        let mut checker = UpstreamReleaseChecker::open(cache.clone());
        let mut calls = Vec::new();
        let mut fake = |url: &str, etag: Option<&str>, timeout: std::time::Duration| {
            calls.push((url.to_owned(), etag.map(str::to_owned), timeout));
            let version = if url.contains("anthropic") {
                "2.2.0"
            } else {
                "0.162.2"
            };
            Ok(RegistryResponse {
                status: 200,
                headers: BTreeMap::from([("ETag".to_owned(), "fixture-tag".to_owned())]),
                body: format!(r#"{{"dist-tags":{{"latest":"{version}"}}}}"#).into_bytes(),
            })
        };
        let latest = checker
            .check(true, 100_000.0, &mut fake)
            .expect("fake registry response accepted");
        assert_eq!(latest.get("claude").map(String::as_str), Some("2.2.0"));
        assert_eq!(latest.get("codex").map(String::as_str), Some("0.162.2"));
        assert_eq!(calls.len(), 2);
        assert!(
            calls
                .iter()
                .all(|(_, _, timeout)| *timeout == Duration::from_secs(5))
        );
        let persisted: Value =
            serde_json::from_slice(&fs::read(cache).expect("release cache persisted"))
                .expect("release cache remains JSON");
        assert_eq!(persisted["version"], 1);
        assert_eq!(persisted["packages"]["@openai/codex"]["version"], "0.162.2");
    }

    #[test]
    fn upstream_checker_respects_opt_out_and_the_daily_cache_window() {
        let root = tempfile::tempdir().expect("fixture root");
        let cache = root.path().join("release-cache.json");
        let mut checker = UpstreamReleaseChecker::open(cache.clone());
        let mut forbidden =
            |_: &str, _: Option<&str>, _: Duration| Err(super::RegistryRequestError);
        assert!(
            checker
                .check(false, 100_000.0, &mut forbidden)
                .expect("opt-out is a no-op")
                .is_empty()
        );
        assert!(!cache.exists());

        let mut initial = |_: &str, _: Option<&str>, _: Duration| {
            Ok(RegistryResponse {
                status: 200,
                headers: BTreeMap::new(),
                body: br#"{"dist-tags":{"latest":"2.0.0"}}"#.to_vec(),
            })
        };
        checker
            .check(true, 100_000.0, &mut initial)
            .expect("first fetch accepted");
        let mut no_request = |_: &str, _: Option<&str>, _: Duration| {
            panic!("daily cache caused an unexpected registry request")
        };
        assert_eq!(
            checker
                .check(true, 100_001.0, &mut no_request)
                .expect("daily cache read"),
            BTreeMap::from([
                ("claude".to_owned(), "2.0.0".to_owned()),
                ("codex".to_owned(), "2.0.0".to_owned()),
            ])
        );
    }

    #[test]
    fn upstream_304_requires_and_refreshes_a_valid_cached_version() {
        let root = tempfile::tempdir().expect("fixture root");
        let cache = root.path().join("release-cache.json");
        let mut checker = UpstreamReleaseChecker::open(cache.clone());
        let mut initial = |_: &str, _: Option<&str>, _: Duration| {
            Ok(RegistryResponse {
                status: 200,
                headers: BTreeMap::from([("etag".to_owned(), "v1".to_owned())]),
                body: br#"{"dist-tags":{"latest":"2.0.0"}}"#.to_vec(),
            })
        };
        checker
            .check(true, 100_000.0, &mut initial)
            .expect("first response accepted");
        let mut calls = Vec::new();
        let mut not_modified = |_: &str, etag: Option<&str>, _: Duration| {
            calls.push(etag.map(str::to_owned));
            Ok(RegistryResponse {
                status: 304,
                headers: BTreeMap::from([("ETag".to_owned(), "v2".to_owned())]),
                body: Vec::new(),
            })
        };
        assert_eq!(
            checker
                .check(true, 186_400.0, &mut not_modified)
                .expect("304 reuses cached version")
                .len(),
            2
        );
        assert_eq!(calls, vec![Some("v1".to_owned()), Some("v1".to_owned())]);
        let persisted: Value =
            serde_json::from_slice(&fs::read(cache).expect("cache update persisted"))
                .expect("cache remains JSON");
        assert_eq!(persisted["packages"]["@openai/codex"]["etag"], "v2");
    }
}
