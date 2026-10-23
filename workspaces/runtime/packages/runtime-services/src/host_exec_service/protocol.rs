//! The Mac side of the host_exec content transport.
//!
//! The guest module `vm-guest/host_exec_protocol.py` owns the frame protocol
//! and its limits; the copies below are checked against it by
//! `tests::the_protocol_limits_match_the_guest_owner`. The manifest of a slot
//! must hash to the same SHA-256 as the guest's manifest of the same tree, so
//! it is serialized as Python `json.dumps(sort_keys=True)` writes it.

use std::collections::BTreeMap;
use std::fs;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Largest request or response frame, in bytes.
pub(crate) const FRAME_LIMIT: usize = 2 * 1024 * 1024;
/// Largest file chunk of one transfer request, in bytes.
pub(crate) const CHUNK: usize = 512 * 1024;
/// Largest entry count of one slot scan.
pub(crate) const MAX_ENTRIES: usize = 2_000_000;
/// Largest entry count of one manifest page.
pub(crate) const PAGE_ENTRIES: usize = 4096;
/// Largest byte estimate of one manifest page.
pub(crate) const PAGE_BYTES: usize = 1024 * 1024;
/// The hash cache does not keep a file whose times are this close to the scan,
/// in nanoseconds: a second write in the same clock step would keep all key fields.
pub(crate) const RACY_NS: i128 = 2_000_000_000;

/// An explicit refusal with a protocol error code (guest `HostExecError`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct HostExecError {
    pub(crate) code: String,
    pub(crate) message: String,
}

impl HostExecError {
    pub(crate) fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.to_owned(),
            message: message.into(),
        }
    }

    pub(crate) fn object(&self) -> Value {
        json!({"code": self.code, "message": self.message})
    }
}

/// A handler result: an explicit refusal, or a failure without a protocol code
/// (an unknown outcome for a mutating request).
#[derive(Debug)]
pub(crate) enum Failure {
    Refused(HostExecError),
    Unexpected(String),
}

impl From<HostExecError> for Failure {
    fn from(error: HostExecError) -> Self {
        Self::Refused(error)
    }
}

impl From<std::io::Error> for Failure {
    fn from(error: std::io::Error) -> Self {
        Self::Unexpected(error.to_string())
    }
}

impl From<serde_json::Error> for Failure {
    fn from(error: serde_json::Error) -> Self {
        Self::Unexpected(error.to_string())
    }
}

impl From<rusqlite::Error> for Failure {
    fn from(error: rusqlite::Error) -> Self {
        Self::Unexpected(error.to_string())
    }
}

pub(crate) type Outcome<T> = Result<T, Failure>;

/// Refuse with `code` unless `condition` (guest `require`).
pub(crate) fn require(condition: bool, message: &str, code: &str) -> Result<(), HostExecError> {
    if condition {
        Ok(())
    } else {
        Err(HostExecError::new(code, message))
    }
}

pub(crate) const INVALID: &str = "invalid_params";

/// The SHA-256 hex digest of a text (guest `key`).
pub(crate) fn key(value: &str) -> String {
    hex(Sha256::digest(value.as_bytes()).as_slice())
}

pub(crate) fn hex(bytes: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(bytes)
}

/// The text of Python `json.dumps(value, sort_keys=True)`, which the guest hashes.
pub(crate) fn sorted_json(value: &Value) -> String {
    studio_runtime_store::python_json(value)
}

/// Write `value` as JSON to `path` through a synced temporary file and a rename.
pub(crate) fn atomic(path: &Path, value: &Value) -> std::io::Result<()> {
    let parent = path.parent().unwrap_or(Path::new("."));
    let mut temporary = tempfile::Builder::new()
        .prefix(".host-exec-")
        .tempfile_in(parent)?;
    temporary.write_all(serde_json::to_string(value)?.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary.persist(path).map_err(|error| error.error)?;
    fs::File::open(parent)?.sync_all()
}

/// Read a JSON file.
pub(crate) fn read_json(path: &Path) -> Outcome<Value> {
    Ok(serde_json::from_str(&fs::read_to_string(path)?)?)
}

/// A relative slash path without empty, `.` or `..` parts (guest `relative`).
pub(crate) fn relative(value: Option<&Value>) -> Result<String, HostExecError> {
    let text = value.and_then(Value::as_str).unwrap_or_default();
    require(
        !text.is_empty() && text.len() <= 4096 && !text.contains(['\0', '\n', '\r']),
        "Invalid relative file path",
        INVALID,
    )?;
    require(
        !text.starts_with('/') && text.split('/').all(|part| !matches!(part, "" | "." | "..")),
        "The file path must be relative without dot components",
        INVALID,
    )?;
    Ok(text.to_owned())
}

/// `root/value` without a symbolic link on the way below
/// `root`; with `leaf`, the last part also must not be a link (guest `inside`).
pub(crate) fn inside(
    root: &Path,
    value: Option<&Value>,
    leaf: bool,
) -> Result<PathBuf, HostExecError> {
    let name = relative(value)?;
    let path = root.join(&name);
    let mut current = if leaf {
        Some(path.as_path())
    } else {
        path.parent()
    };
    while let Some(parent) = current {
        if parent == root {
            break;
        }
        require(
            !is_symlink(parent),
            "The file path crosses a symbolic link",
            INVALID,
        )?;
        current = parent.parent();
    }
    Ok(path)
}

pub(crate) fn text_value(value: &str) -> Value {
    Value::String(value.to_owned())
}

pub(crate) fn is_symlink(path: &Path) -> bool {
    fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink())
}

/// Whether `path` is one of `roots` or inside one of them.
pub(crate) fn under(path: &str, roots: &[String]) -> bool {
    roots.iter().any(|root| {
        path == root
            || path
                .strip_prefix(root.as_str())
                .is_some_and(|rest| rest.starts_with('/'))
    })
}

/// A saved hash cache; a missing or damaged file is an empty cache.
pub(crate) fn load_hashes(path: &Path) -> Map<String, Value> {
    fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| match value {
            Value::Object(map) => Some(map),
            _ => None,
        })
        .unwrap_or_default()
}

/// The hash cache key of a file: size, modification time, inode and change time.
pub(crate) fn stamp(meta: &fs::Metadata) -> [Value; 4] {
    [
        Value::from(meta.size()),
        Value::from(nanoseconds(meta.mtime(), meta.mtime_nsec())),
        Value::from(meta.ino()),
        Value::from(nanoseconds(meta.ctime(), meta.ctime_nsec())),
    ]
}

fn nanoseconds(seconds: i64, nanoseconds: i64) -> i64 {
    seconds
        .saturating_mul(1_000_000_000)
        .saturating_add(nanoseconds)
}

pub(crate) fn now_ns() -> i128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos() as i128)
}

/// The SHA-256 of a file's content and its metadata after the read.
pub(crate) fn digest_file(path: &Path) -> std::io::Result<(String, fs::Metadata)> {
    let mut file = fs::File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 256 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok((hex(hasher.finalize().as_slice()), file.metadata()?))
}

/// The SHA-256 of a regular file and whether `hashes` holds it (guest `file_hash`).
/// The cache keeps only a hash of a file that did not change during the read and
/// whose times are older than [`RACY_NS`] before `started`.
pub(crate) fn file_hash(
    path: &Path,
    meta: &fs::Metadata,
    rel: &str,
    hashes: Option<&mut Map<String, Value>>,
    started: i128,
) -> std::io::Result<(String, bool)> {
    let key = stamp(meta);
    if let Some(Value::Array(cached)) = hashes.as_deref().and_then(|hashes| hashes.get(rel))
        && cached.len() == 5
        && cached[..4] == key
        && let Some(checksum) = cached[4].as_str()
    {
        return Ok((checksum.to_owned(), true));
    }
    let (checksum, done) = digest_file(path)?;
    let Some(hashes) = hashes else {
        return Ok((checksum, false));
    };
    let newest = i128::from(nanoseconds(meta.mtime(), meta.mtime_nsec()))
        .max(i128::from(nanoseconds(meta.ctime(), meta.ctime_nsec())));
    if stamp(&done) == key && started - newest > RACY_NS {
        let mut row = key.to_vec();
        row.push(Value::String(checksum.clone()));
        hashes.insert(rel.to_owned(), Value::Array(row));
        return Ok((checksum, true));
    }
    hashes.remove(rel);
    Ok((checksum, false))
}

/// The entries of one slot or result folder, by relative path.
pub(crate) type Manifest = BTreeMap<String, Value>;

/// The entries below `root` with content hashes (guest `scan`). The relative
/// paths in `skip` stay out with everything inside them. `hashes` is a cache:
/// path -> [size, mtime_ns, inode, ctime_ns, sha256]; the scan updates it and
/// removes the files it no longer finds, except those below a skipped path.
pub(crate) fn scan(
    root: &Path,
    skip: &[String],
    mut hashes: Option<&mut Map<String, Value>>,
) -> Outcome<Manifest> {
    let mut result = Manifest::new();
    let started = now_ns();
    let mut seen = std::collections::HashSet::new();
    if !root.exists() {
        if let Some(hashes) = hashes {
            hashes.clear();
        }
        return Ok(result);
    }
    let mut stack = vec![PathBuf::new()];
    while let Some(base) = stack.pop() {
        // A folder that cannot be listed stays out, as in the guest scan.
        let Ok(listing) = fs::read_dir(root.join(&base)) else {
            continue;
        };
        let mut names = Vec::new();
        for entry in listing.flatten() {
            let name = entry.file_name().into_string().map_err(|_| {
                HostExecError::new(INVALID, "The slot contains a file name that is not UTF-8")
            })?;
            names.push(name);
        }
        names.sort();
        for name in names {
            let path_text = if base.as_os_str().is_empty() {
                name.clone()
            } else {
                format!("{}/{name}", base.display())
            };
            let rel = relative(Some(&Value::String(path_text)))?;
            if skip.contains(&rel) {
                continue;
            }
            let path = root.join(&rel);
            let meta = fs::symlink_metadata(&path)?;
            let kind = meta.file_type();
            if kind.is_symlink() {
                let target = fs::read_link(&path)?
                    .into_os_string()
                    .into_string()
                    .map_err(|_| {
                        HostExecError::new(
                            INVALID,
                            "The slot contains a link target that is not UTF-8",
                        )
                    })?;
                result.insert(rel, json!({"kind": "link", "target": target}));
            } else if kind.is_file() {
                let (checksum, _) = file_hash(&path, &meta, &rel, hashes.as_deref_mut(), started)?;
                seen.insert(rel.clone());
                result.insert(
                    rel,
                    json!({"kind": "file", "bytes": meta.size(),
                           "mode": meta.mode() & 0o777, "sha256": checksum}),
                );
            } else if kind.is_dir() {
                stack.push(PathBuf::from(&rel));
                result.insert(rel, json!({"kind": "directory"}));
            } else {
                return Err(HostExecError::new(
                    "unsupported_file",
                    format!("The slot contains a special file: {rel}"),
                )
                .into());
            }
            require(
                result.len() <= MAX_ENTRIES,
                "The slot exceeds its file count limit",
                "disk_budget",
            )?;
        }
    }
    if let Some(hashes) = hashes {
        hashes.retain(|path, _| seen.contains(path) || under(path, skip));
    }
    Ok(result)
}

/// The manifest as a JSON object.
pub(crate) fn manifest_value(manifest: &Manifest) -> Value {
    Value::Object(
        manifest
            .iter()
            .map(|(path, entry)| (path.clone(), entry.clone()))
            .collect(),
    )
}

/// The digest that the guest sends as `manifestSha256` for the same tree.
pub(crate) fn manifest_digest(manifest: &Manifest) -> String {
    key(&sorted_json(&manifest_value(manifest)))
}

/// The entries of a sorted manifest after the path `after` (guest `page`) and
/// the cursor of the next page (`null` after the last page).
pub(crate) fn page(
    items: &[(String, Value)],
    after: Option<&Value>,
) -> Result<Value, HostExecError> {
    let after = match after {
        Some(Value::String(after)) => after.as_str(),
        _ => return Err(HostExecError::new(INVALID, "Invalid manifest cursor")),
    };
    let start = items.partition_point(|(path, _)| path.as_str() <= after);
    let mut entries = Map::new();
    let mut size = 0_usize;
    for (path, entry) in items.iter().skip(start).take(PAGE_ENTRIES) {
        size += path.len() + sorted_json(entry).len() + 8;
        if !entries.is_empty() && size > PAGE_BYTES {
            break;
        }
        entries.insert(path.clone(), entry.clone());
    }
    let end = start + entries.len();
    let next = if !entries.is_empty() && end < items.len() {
        Value::String(items[end - 1].0.clone())
    } else {
        Value::Null
    };
    Ok(json!({"entries": entries, "next": next}))
}

/// Names that differ only in case or Unicode form (guest `conflicts`), which
/// cannot coexist on APFS. Each group is sorted; groups keep first-seen order.
pub(crate) fn conflicts<'a>(paths: impl IntoIterator<Item = &'a String>) -> Vec<Vec<String>> {
    use unicode_casefold::UnicodeCaseFold;
    let normalizer = icu_normalizer::DecomposingNormalizerBorrowed::new_nfd();
    let mut order: Vec<String> = Vec::new();
    let mut names: std::collections::HashMap<String, std::collections::BTreeSet<String>> =
        std::collections::HashMap::new();
    for path in paths {
        let parts: Vec<&str> = path.split('/').collect();
        for size in 1..=parts.len() {
            let name = parts[..size].join("/");
            let normalized: String = normalizer.normalize(&name).chars().case_fold().collect();
            let group = names.entry(normalized.clone()).or_insert_with(|| {
                order.push(normalized);
                std::collections::BTreeSet::new()
            });
            group.insert(name);
        }
    }
    order
        .into_iter()
        .filter_map(|normalized| {
            let group = names.remove(&normalized)?;
            (group.len() > 1).then(|| group.into_iter().collect())
        })
        .collect()
}

/// The spellings of a path in command output, longest first (guest `aliases`).
pub(crate) fn aliases(path: &str) -> Vec<String> {
    let mut values = std::collections::BTreeSet::from([path.to_owned()]);
    if let Ok(real) = studio_runtime_store::path_identity::CanonicalPath::nonstrict(Path::new(path))
        && let Some(real) = real.as_path().to_str()
    {
        values.insert(real.to_owned());
    }
    for value in values.clone() {
        for short in ["/tmp", "/var"] {
            let long = format!("/private{short}");
            if value == long || value.starts_with(&format!("{long}/")) {
                values.insert(value["/private".len()..].to_owned());
            }
            if value == short || value.starts_with(&format!("{short}/")) {
                values.insert(format!("/private{value}"));
            }
        }
    }
    let mut values: Vec<String> = values.into_iter().collect();
    values.sort_by_key(|value| std::cmp::Reverse(value.chars().count()));
    values
}

/// The characters that can follow a path in output (guest `mapped`: `(?=$|[/\s"'():,;\]])`).
fn ends_path(next: Option<char>) -> bool {
    match next {
        None => true,
        // Python `\s` for text: Unicode white space and the separators U+001C to U+001F.
        Some(c) => {
            c.is_whitespace() || ('\u{1c}'..='\u{1f}').contains(&c) || "/\"'():,;]".contains(c)
        }
    }
}

/// Replace each spelling of the slot path with the line path (guest `mapped`).
pub(crate) fn mapped(text: &str, slot: &str, line: &str) -> String {
    let mut text = text.to_owned();
    for path in aliases(slot) {
        let mut output = String::with_capacity(text.len());
        let mut rest = text.as_str();
        while let Some(found) = rest.find(path.as_str()) {
            let after = &rest[found + path.len()..];
            output.push_str(&rest[..found]);
            if ends_path(after.chars().next()) {
                output.push_str(line);
                rest = after;
            } else {
                // As the regular expression does: try again one character later.
                let step = rest[found..].chars().next().map_or(1, char::len_utf8);
                output.push_str(&rest[found..found + step]);
                rest = &rest[found + step..];
            }
        }
        output.push_str(rest);
        text = output;
    }
    text
}
