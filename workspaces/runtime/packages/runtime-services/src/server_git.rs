//! Hardened Git reads and branch bundles for the paired-server channel.
//!
//! Python `codex_multi_server_orchestration.py` owns these rules
//! (`GIT_DATA_KEYS`, `GIT_OVERRIDES`, `git_command`, `bounded_command`,
//! `export_bundle`, `_git_action`). Every command runs with a neutral global
//! configuration, every helper-capable repository key is overridden, and an
//! unknown repository key refuses the command before it reads content.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

/// Bytes of merged stdout and stderr that a Git read keeps (Python `bounded_command`).
pub const GIT_OUTPUT_LIMIT: usize = 64 * 1024;
/// Seconds that one bounded Git read may run (Python `bounded_command`).
const GIT_READ_SECONDS: u64 = 10;
/// Seconds that `git bundle create` may run (Python `export_bundle`).
const BUNDLE_SECONDS: u64 = 60;
/// Largest Git bundle that crosses the channel, in bytes (Python `MAX_BUNDLE`).
pub const MAX_BUNDLE: u64 = 256 * 1024 * 1024;
/// Bytes of one bundle chunk before base64 (Python `CHUNK`).
pub const CHUNK: usize = 128 * 1024;
/// Folder under the state directory that holds exported bundles.
pub const EXPORT_FOLDER: &str = "server-exports";

/// Repository values that only describe data (Python `GIT_DATA_KEYS`).
const GIT_DATA_KEYS: &[&str] = &[
    "core.repositoryformatversion",
    "core.bare",
    "core.filemode",
    "core.ignorecase",
    "core.precomposeunicode",
    "core.logallrefupdates",
    "core.symlinks",
    "core.worktree",
    "core.autocrlf",
    "core.eol",
    "core.safecrlf",
    "core.ignorestat",
    "core.trustctime",
    "core.checkstat",
    "core.bigfilethreshold",
    "core.compression",
    "core.abbrev",
    "core.untrackedcache",
    "core.sparsecheckout",
    "core.sparsecheckoutcone",
    "core.protecthfs",
    "core.protectntfs",
    "core.warnambiguousrefs",
    "extensions.objectformat",
    "extensions.compatobjectformat",
    "extensions.refstorage",
    "extensions.worktreeconfig",
    "extensions.preciousobjects",
    "extensions.relativeworktrees",
    "user.name",
    "user.email",
    "user.signingkey",
    "init.defaultbranch",
    "gpg.format",
    "gpg.ssh.allowedsignersfile",
    "gpg.ssh.revocationfile",
    "gc.autopacklimit",
    "gc.autodetach",
    "gc.pruneexpire",
    "gc.worktreepruneexpire",
    "gc.reflogexpire",
    "gc.reflogexpireunreachable",
    "gc.aggressivewindow",
    "gc.aggressivedepth",
    "gc.bigpackthreshold",
    "gc.writecommitgraph",
    "pack.window",
    "pack.depth",
    "pack.threads",
    "pack.windowmemory",
    "pack.deltacachesize",
    "pack.deltacachelimit",
    "pack.packsizelimit",
    "pack.compression",
    "pack.reuseobjects",
    "pack.reusedeltas",
    "pack.usebitmaps",
    "pack.writebitmaps",
    "pack.writebitmaphashcache",
    "pack.usebitmapindex",
    "pack.writebitmaplookuptable",
    "pack.indexversion",
    "pack.island",
    "pack.islandcore",
    "pack.allowpackreuse",
    "index.version",
    "index.threads",
    "index.recordendofindexentries",
    "index.sparse",
    "feature.manyfiles",
    "feature.experimental",
    "fetch.prune",
    "fetch.prunetags",
    "fetch.fsckobjects",
    "fetch.writecommitgraph",
    "fetch.parallel",
    "fetch.unpacklimit",
    "fetch.negotiationalgorithm",
    "fetch.showforcedupdates",
    "fetch.bundlecreationtoken",
    "transfer.fsckobjects",
    "transfer.unpacklimit",
    "status.showuntrackedfiles",
    "status.relativepaths",
    "status.short",
    "status.branch",
    "status.aheadbehind",
    "status.renames",
    "status.renamelimit",
    "log.decorate",
    "log.abbrevcommit",
    "log.date",
    "log.follow",
    "log.allrefupdates",
    "diff.algorithm",
    "diff.renamelimit",
    "diff.ignoresubmodules",
    "diff.mnemonicprefix",
    "diff.noprefix",
    "diff.context",
    "diff.interhunkcontext",
    "lfs.repositoryformatversion",
    "lfs.url",
    "lfs.pushurl",
    "lfs.storage",
    "lfs.fetchinclude",
    "lfs.fetchexclude",
    "lfs.locksverify",
    "lfs.basictransfersonly",
    "lfs.concurrenttransfers",
    "lfs.pruneoffsetdays",
    "lfs.pruneverifyremotealways",
];

/// Neutral values that disable helpers and transports (Python `GIT_OVERRIDES`,
/// in its insertion order).
const GIT_OVERRIDES: &[(&str, &str)] = &[
    ("core.fsmonitor", "false"),
    ("core.hookspath", "/dev/null"),
    ("core.pager", "cat"),
    ("core.attributesfile", "/dev/null"),
    ("core.excludesfile", "/dev/null"),
    ("core.alternaterefscommand", ""),
    ("core.alternaterefsprefixes", ""),
    ("core.askpass", "false"),
    ("core.sshcommand", "false"),
    ("core.gitproxy", "false"),
    ("credential.helper", ""),
    ("uploadpack.packobjectshook", ""),
    ("fetch.recursesubmodules", "false"),
    ("submodule.recurse", "false"),
    ("status.submodulesummary", "false"),
    ("maintenance.auto", "false"),
    ("gc.auto", "0"),
    ("diff.renames", "false"),
    ("diff.external", ""),
    ("log.showsignature", "false"),
    ("commit.gpgsign", "false"),
    ("gpg.program", "false"),
    ("gpg.ssh.program", "false"),
    ("gpg.x509.program", "false"),
    ("protocol.allow", "never"),
    ("protocol.ext.allow", "never"),
    ("protocol.ssh.allow", "never"),
    ("protocol.http.allow", "never"),
    ("protocol.https.allow", "never"),
    ("protocol.file.allow", "never"),
];

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum GitError {
    /// A refusal that Python raises as `ValueError`: the request did not run.
    #[error("{0}")]
    Refused(String),
    /// A process or file failure that Python raises outside its refusal types.
    #[error("{0}")]
    Failed(String),
}

fn refused(message: impl Into<String>) -> GitError {
    GitError::Refused(message.into())
}

fn failed(message: impl Into<String>) -> GitError {
    GitError::Failed(message.into())
}

/// One prepared Git process (Python `GitCommand`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GitCommand {
    pub argv: Vec<String>,
    pub environment: Vec<(String, String)>,
}

/// Python `git_environment`: the server environment without `GIT_*` names,
/// a neutral global configuration and the structured overrides.
fn git_environment(settings: &[(String, String)]) -> Vec<(String, String)> {
    let mut environment: Vec<(String, String)> = std::env::vars()
        .filter(|(key, _)| !key.starts_with("GIT_"))
        .collect();
    for (key, value) in [
        ("GIT_CONFIG_NOSYSTEM", "1"),
        ("GIT_CONFIG_GLOBAL", "/dev/null"),
        ("GIT_TERMINAL_PROMPT", "0"),
        ("GIT_NO_LAZY_FETCH", "1"),
    ] {
        environment.push((key.to_owned(), value.to_owned()));
    }
    environment.push(("GIT_CONFIG_COUNT".to_owned(), settings.len().to_string()));
    for (index, (key, value)) in settings.iter().enumerate() {
        environment.push((format!("GIT_CONFIG_KEY_{index}"), key.clone()));
        environment.push((format!("GIT_CONFIG_VALUE_{index}"), value.clone()));
    }
    environment
}

/// Split `group.name.field` like Python `partition` and `rpartition`.
fn key_parts(key: &str) -> Option<(&str, &str)> {
    let (group, rest) = key.split_once('.')?;
    let (name, field) = rest.rsplit_once('.')?;
    (!name.is_empty()).then_some((group, field))
}

/// Python `git_data_key`.
pub fn git_data_key(key: &str) -> bool {
    if GIT_DATA_KEYS.contains(&key) {
        return true;
    }
    key_parts(key).is_some_and(|(group, field)| {
        (group == "remote" && matches!(field, "url" | "fetch" | "pushurl"))
            || (group == "branch" && matches!(field, "remote" | "merge" | "rebase"))
            || (group == "submodule" && matches!(field, "url" | "active" | "path"))
    })
}

/// Python `git_neutral_value`.
pub fn git_neutral_value(key: &str) -> Option<&'static str> {
    if let Some((_, value)) = GIT_OVERRIDES.iter().find(|(name, _)| *name == key) {
        return Some(value);
    }
    let (group, field) = key_parts(key)?;
    match (group, field) {
        ("filter", "clean" | "smudge" | "process") => Some(""),
        ("filter", "required") => Some("false"),
        ("diff", "command" | "textconv") => Some(""),
        ("credential", "helper") => Some(""),
        ("protocol", "allow") => Some("never"),
        _ => None,
    }
}

/// Python `git_command`: inspect the effective repository configuration and
/// neutralize every key that can run a helper.
pub fn git_command(
    directory: &Path,
    arguments: &[&str],
    file_transport: bool,
) -> Result<GitCommand, GitError> {
    let mut settings: Vec<(String, String)> = GIT_OVERRIDES
        .iter()
        .map(|(key, value)| ((*key).to_owned(), (*value).to_owned()))
        .collect();
    let base = vec![
        "git".to_owned(),
        "--no-pager".to_owned(),
        "--no-optional-locks".to_owned(),
        "-C".to_owned(),
        directory.to_string_lossy().into_owned(),
    ];
    let mut inspect = base.clone();
    inspect.extend(
        [
            "config",
            "--list",
            "--includes",
            "--show-scope",
            "--name-only",
            "-z",
        ]
        .map(str::to_owned),
    );
    let (raw, code, truncated) = bounded_command(&GitCommand {
        argv: inspect,
        environment: git_environment(&settings),
    })?;
    if truncated || code != 0 {
        return Err(refused(
            "Git could not read the effective repository configuration",
        ));
    }
    let entries: Vec<&[u8]> = if raw.ends_with(b"\0") {
        raw[..raw.len() - 1].split(|byte| *byte == 0).collect()
    } else {
        Vec::new()
    };
    if !raw.is_empty() && (!raw.ends_with(b"\0") || entries.len() % 2 == 1) {
        return Err(refused("Invalid Git repository configuration"));
    }
    for pair in entries.chunks(2) {
        let (scope, entry) = (pair[0], pair[1]);
        if scope == b"command" {
            continue;
        }
        let key = String::from_utf8_lossy(entry).into_owned();
        if scope != b"local" && scope != b"worktree" {
            return Err(refused(format!(
                "Unsupported Git configuration scope for key: {}",
                python_repr(&key)
            )));
        }
        if let Some(neutral) = git_neutral_value(&key) {
            settings.push((key, neutral.to_owned()));
        } else if !git_data_key(&key) {
            return Err(refused(format!(
                "Unsupported Git repository configuration key: {}",
                python_repr(&key)
            )));
        }
    }
    if file_transport {
        settings.push(("protocol.file.allow".to_owned(), "always".to_owned()));
    }
    let mut argv = base;
    argv.extend(arguments.iter().map(|value| (*value).to_owned()));
    Ok(GitCommand {
        argv,
        environment: git_environment(&settings),
    })
}

/// Python `repr` of a configuration key in a refusal text.
fn python_repr(value: &str) -> String {
    if value.contains('\'') && !value.contains('"') {
        format!("\"{value}\"")
    } else {
        format!("'{}'", value.replace('\\', "\\\\").replace('\'', "\\'"))
    }
}

fn spawn(
    command: &GitCommand,
    merged: bool,
) -> Result<(std::process::Child, std::io::PipeReader), GitError> {
    let (reader, writer) = std::io::pipe().map_err(|error| failed(error.to_string()))?;
    let stderr = if merged {
        Stdio::from(
            writer
                .try_clone()
                .map_err(|error| failed(error.to_string()))?,
        )
    } else {
        Stdio::null()
    };
    let mut process = Command::new(&command.argv[0]);
    process
        .args(&command.argv[1..])
        .env_clear()
        .envs(command.environment.iter().map(|(key, value)| (key, value)))
        .stdin(Stdio::null())
        .stdout(Stdio::from(writer))
        .stderr(stderr);
    let child = process
        .spawn()
        .map_err(|error| failed(format!("Git could not start: {error}")))?;
    // The parent's copies of the write end close with `process`.
    drop(process);
    Ok((child, reader))
}

/// Read a pipe on a helper thread so the caller can apply one deadline.
fn read_pipe(
    mut reader: std::io::PipeReader,
    limit: usize,
) -> std::sync::mpsc::Receiver<std::io::Result<(Vec<u8>, bool)>> {
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    std::thread::spawn(move || {
        let mut data = Vec::new();
        let mut buffer = [0_u8; 8192];
        let result = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break Ok((data, false)),
                Ok(count) => {
                    data.extend_from_slice(&buffer[..count]);
                    if data.len() > limit {
                        break Ok((data, true));
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => break Err(error),
            }
        };
        drop(sender.send(result));
    });
    receiver
}

fn kill(child: &mut std::process::Child) {
    if matches!(child.try_wait(), Ok(None)) {
        drop(child.kill());
    }
    drop(child.wait());
}

/// Python `bounded_command`: merged output, at most 64 KiB, a 10-second
/// deadline. The result is (output, exit code, truncated).
pub fn bounded_command(command: &GitCommand) -> Result<(Vec<u8>, i32, bool), GitError> {
    let (mut child, reader) = spawn(command, true)?;
    let receiver = read_pipe(reader, GIT_OUTPUT_LIMIT);
    let deadline = Instant::now() + Duration::from_secs(GIT_READ_SECONDS);
    let read = receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()));
    let (mut data, truncated) = match read {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            kill(&mut child);
            return Err(failed(error.to_string()));
        }
        Err(_) => {
            kill(&mut child);
            return Err(failed("The Git command exceeded 10 seconds"));
        }
    };
    if truncated {
        kill(&mut child);
    }
    data.truncate(GIT_OUTPUT_LIMIT);
    let status = wait_until(&mut child, Instant::now() + Duration::from_secs(2))?;
    Ok((data, exit_code(status), truncated))
}

#[allow(
    clippy::disallowed_methods,
    reason = "Git operations run on multi-server labeled blocking workers"
)]
fn wait_until(
    child: &mut std::process::Child,
    deadline: Instant,
) -> Result<std::process::ExitStatus, GitError> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            Ok(None) => {
                kill(child);
                return Err(failed("The Git process did not exit"));
            }
            Err(error) => {
                kill(child);
                return Err(failed(error.to_string()));
            }
        }
    }
}

/// Python `Popen.wait()` code: a signal is its negative number.
fn exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
        if let Some(signal) = status.signal() {
            return -signal;
        }
    }
    status.code().unwrap_or(-1)
}

/// Python `git_read`: refuse output above 64 KiB.
pub fn git_read(directory: &Path, arguments: &[&str]) -> Result<(Vec<u8>, i32), GitError> {
    let (output, code, truncated) = bounded_command(&git_command(directory, arguments, false)?)?;
    if truncated {
        return Err(refused("The Git metadata exceeds 64 KiB"));
    }
    Ok((output, code))
}

/// Run a hardened Git command that must succeed within `seconds`
/// (Python `subprocess.run(..., check=True, timeout=...)`).
pub fn git_run(command: &GitCommand, seconds: u64) -> Result<(), GitError> {
    let (mut child, reader) = spawn(command, true)?;
    let receiver = read_pipe(reader, GIT_OUTPUT_LIMIT);
    let deadline = Instant::now() + Duration::from_secs(seconds);
    if receiver
        .recv_timeout(deadline.saturating_duration_since(Instant::now()))
        .is_err()
    {
        kill(&mut child);
        return Err(failed("The Git command did not finish in time"));
    }
    let status = wait_until(
        &mut child,
        deadline.max(Instant::now() + Duration::from_secs(2)),
    )?;
    if !status.success() {
        return Err(failed("The Git command failed"));
    }
    Ok(())
}

/// Python `commit_hash`: a complete SHA-1 or SHA-256 commit name.
pub fn commit_hash(value: &str) -> bool {
    matches!(value.len(), 40 | 64)
        && value
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
}

/// Python `file_digest`.
pub fn file_digest(path: &Path) -> Result<String, GitError> {
    let mut file = std::fs::File::open(path).map_err(|error| failed(error.to_string()))?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1024 * 1024];
    loop {
        let count = file
            .read(&mut buffer)
            .map_err(|error| failed(error.to_string()))?;
        if count == 0 {
            break;
        }
        digest.update(&buffer[..count]);
    }
    Ok(data_encoding::HEXLOWER.encode(&digest.finalize()))
}

/// Python `export_bundle`: stream `git bundle create -` into `path` with a
/// 256 MiB bound and a 60-second deadline. A failure removes the file.
pub fn export_bundle(
    directory: &Path,
    branch: &str,
    path: &Path,
    exclusions: &[String],
) -> Result<(), GitError> {
    let reference = format!("refs/heads/{branch}");
    let excluded: Vec<String> = exclusions
        .iter()
        .map(|commit| format!("^{commit}"))
        .collect();
    let mut arguments = vec!["bundle", "create", "-", reference.as_str()];
    arguments.extend(excluded.iter().map(String::as_str));
    let command = git_command(directory, &arguments, false)?;
    let result = write_bundle(&command, path);
    if result.is_err() {
        drop(std::fs::remove_file(path));
    }
    result
}

fn write_bundle(command: &GitCommand, path: &Path) -> Result<(), GitError> {
    use std::io::Write;
    let (mut child, reader) = spawn(command, false)?;
    let (sender, receiver) = std::sync::mpsc::sync_channel(1);
    let target = path.to_path_buf();
    std::thread::spawn(move || {
        let result = (|| -> Result<(), GitError> {
            let mut reader = reader;
            let mut output = std::fs::OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&target)
                .map_err(|error| failed(error.to_string()))?;
            let mut buffer = vec![0_u8; 128 * 1024];
            let mut size: u64 = 0;
            loop {
                let count = match reader.read(&mut buffer) {
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(failed(error.to_string())),
                };
                if count == 0 {
                    break;
                }
                size += count as u64;
                if size > MAX_BUNDLE {
                    return Err(refused("The Git bundle exceeds 256 MiB"));
                }
                output
                    .write_all(&buffer[..count])
                    .map_err(|error| failed(error.to_string()))?;
            }
            output.sync_all().map_err(|error| failed(error.to_string()))
        })();
        drop(sender.send(result));
    });
    let deadline = Instant::now() + Duration::from_secs(BUNDLE_SECONDS);
    let written = match receiver.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
        Ok(result) => result,
        Err(_) => Err(failed("The Git bundle export exceeded 60 seconds")),
    };
    if let Err(error) = written {
        kill(&mut child);
        return Err(error);
    }
    let status = wait_until(&mut child, Instant::now() + Duration::from_secs(2))?;
    if !status.success() {
        return Err(refused("Git could not export the worker branch"));
    }
    Ok(())
}

/// Python `identity`: the shared cross-server UUID name.
fn identity(parts: &[&str]) -> String {
    crate::projects::cross_server_identity(parts)
}

fn export_path(root: &Path, export: &str) -> Result<PathBuf, GitError> {
    let id = uuid::Uuid::parse_str(export)
        .map_err(|_| refused("badly formed hexadecimal UUID string"))?;
    Ok(root
        .join(EXPORT_FOLDER)
        .join(format!("{}.bundle", id.hyphenated())))
}

fn text<'a>(payload: &'a Value, key: &str) -> Option<&'a str> {
    payload.get(key).and_then(Value::as_str)
}

/// The receive side of the Git channel actions: Python `_git_action` for
/// `git`, `export`, `chunk` and `release`. `root` is the state directory.
pub fn receive(root: &Path, action: &str, payload: &Value, key: &str) -> Result<Value, GitError> {
    match action {
        "release" => {
            let export = text(payload, "export").ok_or_else(|| refused("Supply an export"))?;
            let path = export_path(root, export)?;
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(failed(error.to_string())),
            }
            return Ok(json!({"released": export}));
        }
        "chunk" => {
            let export = text(payload, "export").ok_or_else(|| refused("Supply an export"))?;
            let path = export_path(root, export)?;
            if !path.is_file() {
                return Err(refused("The Git bundle has expired or was released"));
            }
            let size = std::fs::metadata(&path)
                .map_err(|error| failed(error.to_string()))?
                .len();
            let offset = payload
                .get("offset")
                .filter(|value| value.is_u64() || value.is_i64())
                .and_then(Value::as_i64)
                .filter(|offset| *offset >= 0 && (*offset as u64) < size)
                .ok_or_else(|| refused("Invalid bundle offset"))? as u64;
            use std::io::{Seek, SeekFrom};
            let mut file = std::fs::File::open(&path).map_err(|error| failed(error.to_string()))?;
            file.seek(SeekFrom::Start(offset))
                .map_err(|error| failed(error.to_string()))?;
            let mut data = Vec::with_capacity(CHUNK);
            file.take(CHUNK as u64)
                .read_to_end(&mut data)
                .map_err(|error| failed(error.to_string()))?;
            return Ok(json!({
                "offset": offset,
                "nextOffset": offset + data.len() as u64,
                "data": base64::engine::general_purpose::STANDARD.encode(&data),
                "sha256": data_encoding::HEXLOWER.encode(&Sha256::digest(&data)),
            }));
        }
        _ => {}
    }
    let directory = PathBuf::from(text(payload, "cwd").unwrap_or_default());
    if !directory.is_absolute() || !directory.is_dir() {
        return Err(refused("Supply an existing absolute repository folder"));
    }
    if action == "export" {
        return export(root, &directory, payload, key);
    }
    let argv = payload
        .get("argv")
        .and_then(Value::as_array)
        .filter(|argv| !argv.is_empty() && argv.len() <= 8)
        .and_then(|argv| {
            argv.iter()
                .map(|value| value.as_str().filter(|value| value.chars().count() <= 1024))
                .collect::<Option<Vec<&str>>>()
        })
        .ok_or_else(|| refused("Supply a bounded Git argument list"))?;
    if !allowed_read(&argv) {
        return Err(refused("Unsupported read-only Git command"));
    }
    let mut arguments: Vec<&str> = Vec::with_capacity(argv.len() + 3);
    if matches!(argv[0], "log" | "show") {
        arguments.push(argv[0]);
        arguments.extend(["--no-show-signature", "--no-ext-diff", "--no-textconv"]);
        arguments.extend(&argv[1..]);
    } else if argv[0] == "status" {
        arguments.extend(&argv);
        arguments.extend(["--no-renames", "--ignore-submodules=all"]);
    } else {
        arguments.extend(&argv);
    }
    let (raw, code, truncated) = bounded_command(&git_command(&directory, &arguments, false)?)?;
    Ok(json!({
        "exitCode": code,
        "output": String::from_utf8_lossy(&raw),
        "truncated": truncated,
    }))
}

/// The complete read-only command forms (Python `_git_action`).
fn allowed_read(argv: &[&str]) -> bool {
    let fixed: &[&[&str]] = &[
        &["status", "--porcelain=v1"],
        &["branch", "--list"],
        &["rev-parse", "HEAD"],
        &["rev-parse", "--show-toplevel"],
    ];
    if argv.len() >= 2 && argv[..2] == ["log", "-n"] {
        return argv.len() == 3
            && !argv[2].is_empty()
            && argv[2].chars().all(|c| c.is_ascii_digit())
            && argv[2]
                .parse::<u64>()
                .is_ok_and(|count| (1..=50).contains(&count));
    }
    if argv.len() >= 2 && argv[..2] == ["show", "--no-patch"] {
        return argv.len() == 3 && commit_hash(argv[2]);
    }
    fixed.contains(&argv)
}

fn export(root: &Path, directory: &Path, payload: &Value, key: &str) -> Result<Value, GitError> {
    let branch = text(payload, "branch")
        .filter(|branch| !branch.starts_with('-'))
        .ok_or_else(|| refused("Supply a worker branch"))?;
    if git_read(directory, &["check-ref-format", "--branch", branch])?.1 != 0 {
        return Err(refused("Invalid worker branch"));
    }
    let known = match payload.get("known") {
        None => Vec::new(),
        Some(Value::Array(values)) if values.len() <= 16 => values
            .iter()
            .map(|value| value.as_str().filter(|commit| commit_hash(commit)))
            .collect::<Option<Vec<&str>>>()
            .ok_or_else(|| refused("Supply at most 16 complete commit hashes"))?,
        Some(_) => return Err(refused("Supply at most 16 complete commit hashes")),
    };
    let mut exclusions = Vec::new();
    for commit in known {
        let object = format!("{commit}^{{commit}}");
        if git_read(directory, &["cat-file", "-e", &object])?.1 == 0 {
            exclusions.push(commit.to_owned());
        }
    }
    let reference = format!("refs/heads/{branch}");
    let (tip, code) = git_read(directory, &["rev-parse", "--verify", &reference])?;
    let tip = String::from_utf8_lossy(&tip).trim().to_owned();
    if code != 0 || !commit_hash(&tip) {
        return Err(refused("Unknown worker branch"));
    }
    for commit in &exclusions {
        if git_read(directory, &["merge-base", "--is-ancestor", &tip, commit])?.1 == 0 {
            return Ok(json!({"alreadyHave": tip, "bytes": 0, "branch": branch}));
        }
    }
    let export_id = identity(&[key, "bundle"]);
    let folder = root.join(EXPORT_FOLDER);
    match std::fs::create_dir(&folder) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(failed(error.to_string())),
    }
    let path = folder.join(format!("{export_id}.bundle"));
    if !path.exists() {
        if crate::disk_space::free_bytes(&folder).is_some_and(|free| free < MAX_BUNDLE) {
            return Err(failed("Insufficient disk space for a Git bundle"));
        }
        let temporary = folder.join(format!("{export_id}.partial"));
        export_bundle(directory, branch, &temporary, &exclusions)?;
        std::fs::rename(&temporary, &path).map_err(|error| failed(error.to_string()))?;
    }
    let bytes = std::fs::metadata(&path)
        .map_err(|error| failed(error.to_string()))?
        .len();
    Ok(json!({
        "export": export_id,
        "bytes": bytes,
        "sha256": file_digest(&path)?,
        "branch": branch,
    }))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use super::*;

    fn git(directory: &Path, arguments: &[&str]) {
        let status = Command::new("git")
            .arg("-C")
            .arg(directory)
            .args(arguments)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", "/dev/null")
            .env("GIT_AUTHOR_NAME", "Test")
            .env("GIT_AUTHOR_EMAIL", "test@example.com")
            .env("GIT_COMMITTER_NAME", "Test")
            .env("GIT_COMMITTER_EMAIL", "test@example.com")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("git runs");
        assert!(status.success(), "git {arguments:?}");
    }

    fn repository() -> tempfile::TempDir {
        let folder = tempfile::tempdir().expect("temporary folder");
        git(folder.path(), &["init", "-q", "-b", "main"]);
        std::fs::write(folder.path().join("a.txt"), "a\n").expect("write file");
        git(folder.path(), &["add", "a.txt"]);
        git(folder.path(), &["commit", "-q", "-m", "first"]);
        folder
    }

    #[test]
    fn helper_keys_are_neutral_and_unknown_keys_refuse_the_command() {
        assert_eq!(git_neutral_value("core.fsmonitor"), Some("false"));
        assert_eq!(git_neutral_value("filter.lfs.smudge"), Some(""));
        assert_eq!(git_neutral_value("filter.lfs.required"), Some("false"));
        assert_eq!(git_neutral_value("diff.tool.textconv"), Some(""));
        assert!(git_data_key("remote.origin.url"));
        assert!(git_data_key("branch.main.merge"));
        assert!(!git_data_key("alias.st"));
        let folder = repository();
        git(folder.path(), &["config", "alias.st", "!touch pwned"]);
        let refusal = git_command(folder.path(), &["status"], false).expect_err("alias refused");
        assert_eq!(
            refusal,
            GitError::Refused("Unsupported Git repository configuration key: 'alias.st'".into())
        );
    }

    #[test]
    fn a_repository_hook_or_fsmonitor_never_runs_during_a_read() {
        let folder = repository();
        let marker = folder.path().join("ran");
        git(
            folder.path(),
            &[
                "config",
                "core.fsmonitor",
                &format!("touch {}", marker.display()),
            ],
        );
        let value = receive(
            Path::new("/nonexistent-state"),
            "git",
            &json!({"cwd": folder.path(), "argv": ["status", "--porcelain=v1"]}),
            "key",
        )
        .expect("status read");
        assert_eq!(value["exitCode"], 0);
        assert!(!marker.exists(), "fsmonitor helper ran");
    }

    #[test]
    fn only_complete_read_forms_are_accepted() {
        let folder = repository();
        for argv in [
            json!(["log", "-n", "51"]),
            json!(["log", "-n", "0"]),
            json!(["log", "-n", "5", "--all"]),
            json!(["show", "--no-patch", "HEAD"]),
            json!(["config", "--list"]),
            json!(["status"]),
        ] {
            let refusal = receive(
                Path::new("/x"),
                "git",
                &json!({"cwd": folder.path(), "argv": argv}),
                "key",
            )
            .expect_err("refused");
            assert_eq!(
                refusal,
                GitError::Refused("Unsupported read-only Git command".into())
            );
        }
        let log = receive(
            Path::new("/x"),
            "git",
            &json!({"cwd": folder.path(), "argv": ["log", "-n", "1"]}),
            "key",
        )
        .expect("log read");
        assert!(log["output"].as_str().unwrap().contains("first"));
        assert_eq!(log["truncated"], false);
        let relative = receive(
            Path::new("/x"),
            "git",
            &json!({"cwd": "relative", "argv": ["status", "--porcelain=v1"]}),
            "key",
        )
        .expect_err("relative refused");
        assert_eq!(
            relative,
            GitError::Refused("Supply an existing absolute repository folder".into())
        );
    }

    #[test]
    fn a_branch_bundle_exports_in_chunks_and_is_released() {
        let folder = repository();
        let state = tempfile::tempdir().expect("state");
        let export = receive(
            state.path(),
            "export",
            &json!({"cwd": folder.path(), "branch": "main", "known": []}),
            "fetch-key",
        )
        .expect("export");
        let id = export["export"].as_str().expect("export id").to_owned();
        assert_eq!(id, identity(&["fetch-key", "bundle"]));
        let bytes = export["bytes"].as_u64().expect("size");
        assert!(bytes > 0);
        let chunk = receive(
            state.path(),
            "chunk",
            &json!({"export": id, "offset": 0}),
            "c",
        )
        .expect("chunk");
        let data = base64::engine::general_purpose::STANDARD
            .decode(chunk["data"].as_str().unwrap())
            .unwrap();
        assert_eq!(chunk["nextOffset"].as_u64(), Some(data.len() as u64));
        assert_eq!(data.len() as u64, bytes);
        assert_eq!(
            data_encoding::HEXLOWER
                .decode(chunk["sha256"].as_str().unwrap().as_bytes())
                .unwrap(),
            ring::digest::digest(&ring::digest::SHA256, &data).as_ref()
        );
        assert_eq!(export["sha256"], chunk["sha256"]);
        let past = receive(
            state.path(),
            "chunk",
            &json!({"export": id, "offset": bytes}),
            "c",
        )
        .expect_err("offset past the end");
        assert_eq!(past, GitError::Refused("Invalid bundle offset".into()));
        // A known tip proves the receiver already has the branch.
        let (tip, _) = git_read(folder.path(), &["rev-parse", "HEAD"]).unwrap();
        let tip = String::from_utf8(tip).unwrap().trim().to_owned();
        let already = receive(
            state.path(),
            "export",
            &json!({"cwd": folder.path(), "branch": "main", "known": [tip]}),
            "fetch-key-2",
        )
        .expect("already have");
        assert_eq!(
            already,
            json!({"alreadyHave": tip, "bytes": 0, "branch": "main"})
        );
        assert_eq!(
            receive(state.path(), "release", &json!({"export": id}), "r").unwrap(),
            json!({"released": id})
        );
        let gone = receive(
            state.path(),
            "chunk",
            &json!({"export": id, "offset": 0}),
            "c",
        )
        .expect_err("released");
        assert_eq!(
            gone,
            GitError::Refused("The Git bundle has expired or was released".into())
        );
    }
}
