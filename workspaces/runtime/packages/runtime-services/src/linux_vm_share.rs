//! Mount the Finder views of the VM's layr projects again after each VM start
//! (docs/vm-layr.md, "Mac share"; Python `codex_linux_vm_share.remount_registered`).
//!
//! The view of a project is the authenticated, read-only guest SMB share at
//! `~/Studio/<name>`. The VM helper bridges `127.0.0.1:<port>` to the guest
//! over vsock. The password never appears in a process argument or a status
//! response: Studio types it on the controlling terminal that `mount_smbfs`
//! reads. The view is a convenience: a failed mount is saved with its reason
//! in `mount-error.json`, and the agents keep working in the VM.

use std::fs;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use serde_json::{Map, Value, json};

use crate::linux_vm_guest::{GuestError, GuestRpc};

/// The project folders of the VM state folder.
const PROJECTS: &str = "layr-projects";
/// The record of a mounted view, in its project folder.
const MOUNT_RECORD: &str = "mount.json";
/// The reason of the last failed mount, in its project folder.
const MOUNT_ERROR: &str = "mount-error.json";
/// The import record of a project, with its Mac source folder.
const PROJECT_RECORD: &str = "project.json";
/// The private SMB credential of the `studio-view` user, in the VM state folder.
const CREDENTIAL_FILE: &str = "smb-credential.json";
/// How long a remount waits for the layr admin channel, in seconds. It restarts
/// once after a boot that built new layr tools.
const ADMIN_WAIT_SECONDS: f64 = 120.0;
/// Wait between two `project.ensure` requests while the admin channel restarts, in seconds.
const ADMIN_RETRY_SECONDS: f64 = 2.0;
/// Wait of one share request to the guest, in seconds.
const SHARE_REQUEST_SECONDS: u64 = 30;
/// Wait of `share.configure`, in seconds.
const CONFIGURE_SECONDS: u64 = 60;
/// Wait for the password prompt and the end of `mount_smbfs`, in seconds.
const MOUNT_SECONDS: u64 = 25;
/// Wait of `mount` and `umount`, in seconds.
const MOUNT_TABLE_SECONDS: u64 = 30;
/// `nobrowse`: the folder in `~/Studio` is the view; no `127.0.0.1` server in the Finder sidebar.
const MOUNT_OPTIONS: &str = "nobrowse,rdonly,soft,nodatacache,nomdatacache,sessionencrypt";
/// The longest saved error text, in characters.
const ERROR_CHARS: usize = 500;

/// One smbfs mount of the Mac mount table.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountEntry {
    pub source: String,
    pub path: String,
    pub options: Vec<String>,
}

impl MountEntry {
    fn read_only(&self) -> bool {
        self.options.iter().any(|option| option == "read-only")
    }
}

/// The Mac side of a view mount.
pub trait ShareMountTool: Send + Sync {
    /// The smbfs mounts of the Mac.
    fn entries(&self) -> Result<Vec<MountEntry>, String>;
    /// Mount `source` read-only at `destination`; type `password` at the prompt.
    fn mount(&self, source: &str, destination: &Path, password: &str) -> Result<(), String>;
    /// Release the mount at `path`.
    fn unmount(&self, path: &Path) -> Result<(), String>;
}

/// The macOS tools: `/sbin/mount`, `/sbin/umount`, and `/sbin/mount_smbfs`
/// under `/usr/bin/script`, which gives it a controlling terminal.
pub struct MacShareMountTool {
    mount_smbfs: PathBuf,
}

impl Default for MacShareMountTool {
    fn default() -> Self {
        Self {
            mount_smbfs: PathBuf::from("/sbin/mount_smbfs"),
        }
    }
}

impl MacShareMountTool {
    /// A tool that runs `program` in place of `mount_smbfs`.
    pub fn with_program(program: PathBuf) -> Self {
        Self {
            mount_smbfs: program,
        }
    }
}

/// The standard output of `program`, or its failure: the standard error text,
/// empty when the program wrote none.
fn run(program: &str, args: &[&str]) -> Result<Vec<u8>, String> {
    use crate::bounded_child::ChildError;
    crate::bounded_child::run(crate::bounded_child::Request {
        program: std::ffi::OsStr::new(program),
        git_directory: None,
        args,
        environment: &[],
        input: None,
        stdout_limit: 1024 * 1024,
        stderr_limit: 64 * 1024,
        timeout: Duration::from_secs(MOUNT_TABLE_SECONDS),
    })
    .map(|output| output.stdout.bytes)
    .map_err(|error| match error {
        // `bounded_child` names a failure without standard error text this way.
        ChildError::Failed(message) if message == "Git operation failed" => String::new(),
        ChildError::Failed(message) => message,
        ChildError::TimedOut => {
            format!("{program} did not finish within {MOUNT_TABLE_SECONDS} seconds")
        }
        ChildError::Launch(message) => format!("{program} did not run: {message}"),
    })
}

/// The smbfs rows of `mount` output: `<source> on <path> (<options>)`.
fn parse_entries(output: &str) -> Vec<MountEntry> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"^(.+) on (.+) \((.+)\)$")
            .unwrap_or_else(|error| unreachable!("the mount line pattern is valid: {error}"))
    });
    output
        .lines()
        .filter_map(|line| pattern.captures(line))
        .filter_map(|found| {
            let options: Vec<String> = found[3].split(", ").map(str::to_owned).collect();
            options
                .iter()
                .any(|option| option == "smbfs")
                .then(|| MountEntry {
                    source: found[1].to_owned(),
                    path: found[2].to_owned(),
                    options,
                })
        })
        .collect()
}

#[allow(
    clippy::disallowed_methods,
    reason = "a view mount runs on the VM start thread"
)]
fn pause(duration: Duration) {
    std::thread::sleep(duration);
}

/// Run `program -o MOUNT_OPTIONS source destination` with a controlling
/// terminal, and type `password` when it asks. Only the output after the
/// prompt explains a failure; it never contains the password.
pub(crate) fn typed_mount(
    program: &Path,
    source: &str,
    destination: &Path,
    password: &str,
) -> Result<(), String> {
    use std::os::unix::process::CommandExt;
    let mut child = std::process::Command::new("/usr/bin/script")
        .arg("-q")
        .arg("/dev/null")
        .arg(program)
        .arg("-o")
        .arg(MOUNT_OPTIONS)
        .arg(source)
        .arg(destination)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .process_group(0)
        .spawn()
        .map_err(|error| format!("mount_smbfs did not start: {error}"))?;
    let (Some(mut input), Some(output), Some(errors)) =
        (child.stdin.take(), child.stdout.take(), child.stderr.take())
    else {
        drop(child.kill());
        drop(child.wait());
        return Err("mount_smbfs has no terminal pipes".into());
    };
    // Each pipe has its own reader; a reader that a stray holder of the pipe
    // keeps open is left behind after the drain below.
    let read_pipe = |mut pipe: Box<dyn Read + Send>| {
        let (sender, receiver) = std::sync::mpsc::channel::<Vec<u8>>();
        std::thread::spawn(move || {
            let mut buffer = [0_u8; 1024];
            while let Ok(count) = pipe.read(&mut buffer) {
                if count == 0 || sender.send(buffer[..count].to_vec()).is_err() {
                    break;
                }
            }
        });
        receiver
    };
    let receiver = read_pipe(Box::new(output));
    let error_receiver = read_pipe(Box::new(errors));
    let deadline = Instant::now() + Duration::from_secs(MOUNT_SECONDS);
    let mut seen = Vec::new();
    let mut mark = 0;
    let mut sent = false;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Some(status),
            Ok(None) => {}
            Err(_) => break None,
        }
        if Instant::now() >= deadline {
            drop(child.kill());
            break child.wait().ok();
        }
        while let Ok(chunk) = receiver.try_recv() {
            seen.extend(chunk);
        }
        if !sent && seen.windows(7).any(|window| window == b"assword") {
            if input
                .write_all(format!("{password}\n").as_bytes())
                .and_then(|()| input.flush())
                .is_err()
            {
                break child.wait().ok();
            }
            sent = true;
            mark = seen.len();
        }
        pause(Duration::from_millis(50));
    };
    drop(input);
    let drain = |receiver: &std::sync::mpsc::Receiver<Vec<u8>>, into: &mut Vec<u8>| {
        let deadline = Instant::now() + Duration::from_secs(2);
        while let Ok(chunk) =
            receiver.recv_timeout(deadline.saturating_duration_since(Instant::now()))
        {
            into.extend(chunk);
        }
    };
    drain(&receiver, &mut seen);
    let mut stderr = Vec::new();
    drain(&error_receiver, &mut stderr);
    let mut text = String::from_utf8_lossy(&seen[mark.min(seen.len())..]).into_owned();
    text.push_str(&String::from_utf8_lossy(&stderr));
    let text = text.replace(password, "***");
    match status {
        Some(status) if status.success() => Ok(()),
        status => {
            let detail = text
                .lines()
                .map(str::trim)
                .rfind(|line| !line.is_empty())
                .map(|line| line.chars().take(300).collect::<String>())
                .unwrap_or_else(|| match status.and_then(|status| status.code()) {
                    Some(code) => format!("exit code {code}"),
                    None => "no exit code".into(),
                });
            Err(format!("The authenticated SMB mount failed: {detail}"))
        }
    }
}

impl ShareMountTool for MacShareMountTool {
    fn entries(&self) -> Result<Vec<MountEntry>, String> {
        let output = run("/sbin/mount", &[])
            .map_err(|error| format!("The Mac mount table is unavailable: {error}"))?;
        Ok(parse_entries(&String::from_utf8_lossy(&output)))
    }

    fn mount(&self, source: &str, destination: &Path, password: &str) -> Result<(), String> {
        typed_mount(&self.mount_smbfs, source, destination, password)
    }

    fn unmount(&self, path: &Path) -> Result<(), String> {
        let path = path.to_string_lossy();
        run("/sbin/umount", &[&path]).map(drop).map_err(|reason| {
            let reason: String = reason.trim().chars().take(200).collect();
            format!(
                "The previous share mount is busy: {}",
                if reason.is_empty() {
                    "umount failed"
                } else {
                    &reason
                }
            )
        })
    }
}

/// The parts of one remount.
pub(crate) struct Remount<'a> {
    pub state_dir: &'a Path,
    /// `~/Studio`, the parent of every view.
    pub studio_root: &'a Path,
    pub guest: &'a dyn GuestRpc,
    /// The `share` part of the helper status: `state` and `port` of its bridge.
    pub bridge: Option<&'a Value>,
    pub tool: &'a dyn ShareMountTool,
}

fn project_id(value: Option<&Value>) -> Result<String, String> {
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9_-]{0,99}$")
            .unwrap_or_else(|error| unreachable!("the project ID pattern is valid: {error}"))
    });
    value
        .and_then(Value::as_str)
        .filter(|value| pattern.is_match(value))
        .map(str::to_owned)
        .ok_or_else(|| {
            "projectId must start with an ASCII letter or digit and contain letters, digits, \
             hyphens, or underscores."
                .into()
        })
}

fn read_object(path: &Path) -> Option<Map<String, Value>> {
    match serde_json::from_str(&fs::read_to_string(path).ok()?).ok()? {
        Value::Object(map) => Some(map),
        _ => None,
    }
}

/// Save `value` atomically with mode 0600.
fn save_private(path: &Path, value: &Value) -> io::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let parent = path.parent().unwrap_or(Path::new("."));
    fs::create_dir_all(parent)?;
    let mut temporary = tempfile::Builder::new()
        .prefix(".studio-share.")
        .tempfile_in(parent)?;
    temporary.write_all(value.to_string().as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    temporary.persist(path).map_err(|error| error.error)?;
    Ok(())
}

/// The private SMB password, made once and kept, and given to the guest.
fn credential(state_dir: &Path, guest: &dyn GuestRpc) -> Result<String, String> {
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    static PATTERN: OnceLock<regex::Regex> = OnceLock::new();
    let pattern = PATTERN.get_or_init(|| {
        regex::Regex::new(r"^[a-f0-9]{64}$")
            .unwrap_or_else(|error| unreachable!("the password pattern is valid: {error}"))
    });
    let path = state_dir.join(CREDENTIAL_FILE);
    let mut bytes = [0_u8; 32];
    getrandom::fill(&mut bytes).map_err(|error| format!("No random SMB password: {error}"))?;
    let fresh: String = bytes.iter().map(|byte| format!("{byte:02x}")).collect();
    // A new file is made exclusively, so two starts keep one password.
    match fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
    {
        Ok(mut file) => file
            .write_all(json!({"password": fresh}).to_string().as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| format!("The SMB credential is not saved: {error}"))?,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(format!("The SMB credential is not saved: {error}")),
    }
    let meta = fs::symlink_metadata(&path).map_err(|error| error.to_string())?;
    if meta.file_type().is_symlink()
        || meta.uid() != nix::unistd::getuid().as_raw()
        || meta.mode() & 0o077 != 0
    {
        return Err("The SMB credential file must be private and owned by this user.".into());
    }
    let password = read_object(&path)
        .and_then(|saved| {
            saved
                .get("password")
                .and_then(Value::as_str)
                .map(str::to_owned)
        })
        .filter(|password| pattern.is_match(password))
        .ok_or("The saved SMB credential is invalid.")?;
    let identity = format!(
        "share-configure:{}",
        studio_runtime_store::sha256_hex(password.as_bytes())
    );
    guest
        .call(
            "share.configure",
            &json!({"password": password}),
            Some(&identity),
            Duration::from_secs(CONFIGURE_SECONDS),
        )
        .map_err(|error| format!("The guest share is not configured: {error}"))?;
    Ok(password)
}

/// `project.ensure`, again while the guest says that the layr admin channel
/// is unavailable: such a request has not reached the broker, so a repeat
/// cannot run it twice.
fn ensure_project(guest: &dyn GuestRpc, identity: &str) -> Result<Map<String, Value>, String> {
    ensure_project_with_sources(guest, identity, Instant::now, pause)
}

fn ensure_project_with_sources(
    guest: &dyn GuestRpc,
    identity: &str,
    now: impl Fn() -> Instant,
    wait: impl Fn(Duration),
) -> Result<Map<String, Value>, String> {
    let deadline = now() + Duration::from_secs_f64(ADMIN_WAIT_SECONDS);
    loop {
        match guest.call(
            "project.ensure",
            &json!({"projectId": identity}),
            None,
            Duration::from_secs(SHARE_REQUEST_SECONDS),
        ) {
            Ok(Value::Object(project)) => return Ok(project),
            Ok(_) => return Err("The guest returned an invalid project.".into()),
            Err(error @ GuestError::Rejected { .. })
                if error.code() == Some("unavailable") && now() < deadline => {}
            Err(error) => return Err(format!("The VM project is unavailable: {error}")),
        }
        wait(Duration::from_secs_f64(ADMIN_RETRY_SECONDS));
    }
}

/// The preferred share name: the Mac project folder name in plain ASCII.
/// Finder shows the mounted share by this name; the guest makes it unique.
fn share_hint(source: Option<&str>) -> String {
    static UNSAFE: OnceLock<regex::Regex> = OnceLock::new();
    let unsafe_run = UNSAFE.get_or_init(|| {
        regex::Regex::new(r"[^A-Za-z0-9._-]+")
            .unwrap_or_else(|error| unreachable!("the share name pattern is valid: {error}"))
    });
    let name = source
        .and_then(|source| Path::new(source).file_name())
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let trim = |text: &str| {
        text.trim_matches(|c| matches!(c, '.' | '_' | '-'))
            .to_owned()
    };
    let name = trim(&unsafe_run.replace_all(&name, "-"));
    let name = trim(&name.chars().take(40).collect::<String>());
    let valid = name
        .chars()
        .next()
        .is_some_and(|first| first.is_ascii_alphanumeric());
    if valid
        && !matches!(
            name.to_lowercase().as_str(),
            "global" | "homes" | "printers"
        )
    {
        name
    } else {
        "project".into()
    }
}

/// A read-only mount of one of this project's share names, at any address or port.
fn ours(entry: &MountEntry, names: &[String]) -> bool {
    entry.read_only()
        && names.iter().any(|name| {
            entry
                .source
                .strip_prefix("//studio-view@")
                .and_then(|rest| rest.split_once('/'))
                .is_some_and(|(host, share)| !host.is_empty() && share == name)
        })
}

fn mount_project(
    parts: &Remount<'_>,
    identity: &str,
    project: &Map<String, Value>,
    password: &str,
    hint: &str,
) -> Result<(), String> {
    static NAME: OnceLock<regex::Regex> = OnceLock::new();
    let valid_name = NAME.get_or_init(|| {
        regex::Regex::new(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
            .unwrap_or_else(|error| unreachable!("the share name pattern is valid: {error}"))
    });
    if project.get("share").and_then(|share| share.get("state")) != Some(&json!("ready")) {
        return Err("The guest SMB share is not ready.".into());
    }
    let port = parts
        .bridge
        .filter(|bridge| bridge.get("state") == Some(&json!("listening")))
        .and_then(|bridge| bridge.get("port"))
        .and_then(Value::as_u64)
        .ok_or_else(|| {
            format!(
                "The VM share bridge is unavailable: {}",
                parts
                    .bridge
                    .and_then(|bridge| bridge.get("error"))
                    .and_then(Value::as_str)
                    .unwrap_or("not listening")
            )
        })?;
    let name = parts
        .guest
        .call(
            "share.name",
            &json!({"projectId": identity, "name": hint}),
            None,
            Duration::from_secs(SHARE_REQUEST_SECONDS),
        )
        .map_err(|error| format!("The guest share name is unavailable: {error}"))?
        .get("name")
        .and_then(Value::as_str)
        .filter(|name| valid_name.is_match(name))
        .map(str::to_owned)
        .ok_or("The guest returned an invalid share name.")?;
    let record = parts
        .state_dir
        .join(PROJECTS)
        .join(identity)
        .join(MOUNT_RECORD);
    let previous = read_object(&record).unwrap_or_default();
    let previous_name = previous.get("name").and_then(Value::as_str);
    let mut names = vec![identity.to_owned(), name.clone()];
    names.extend(previous_name.map(str::to_owned));
    let destination = parts.studio_root.join(&name);
    {
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new().mode(0o700).create(parts.studio_root) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                return Err(format!("The Studio share folder is unavailable: {error}"));
            }
            _ => {}
        }
    }
    let is_link =
        |path: &Path| fs::symlink_metadata(path).is_ok_and(|meta| meta.file_type().is_symlink());
    if is_link(parts.studio_root) || is_link(&destination) {
        return Err("The Studio share path must not contain symlinks.".into());
    }
    let saved = json!({"projectId": identity, "name": name, "path": destination.to_string_lossy(), "port": port});
    let save = || {
        save_private(&record, &saved)
            .map_err(|error| format!("The share mount record is not saved: {error}"))
    };
    let entries = parts.tool.entries()?;
    // A rename moves the view: release this project's mount at its former path.
    if let Some(former) = previous
        .get("path")
        .and_then(Value::as_str)
        .map(PathBuf::from)
        && former != destination
        && former.parent() == destination.parent()
        && !is_link(&former)
    {
        if entries
            .iter()
            .any(|entry| Path::new(&entry.path) == former && ours(entry, &names))
        {
            parts.tool.unmount(&former)?;
        }
        if fs::read_dir(&former).is_ok_and(|mut entries| entries.next().is_none()) {
            drop(fs::remove_dir(&former));
        }
    }
    let expected = format!("//studio-view@127.0.0.1:{port}/{name}");
    for entry in parts
        .tool
        .entries()?
        .into_iter()
        .filter(|entry| Path::new(&entry.path) == destination)
    {
        if entry.source == expected && entry.read_only() {
            return save();
        }
        // Only this share at an older address, port or name is replaced; anything else stays.
        if !ours(&entry, &names) {
            return Err("A different filesystem already uses the project share path.".into());
        }
        parts.tool.unmount(&destination)?;
    }
    {
        use std::os::unix::fs::DirBuilderExt;
        match fs::DirBuilder::new().mode(0o700).create(&destination) {
            Err(error) if error.kind() != io::ErrorKind::AlreadyExists => {
                return Err(format!(
                    "The project share mount folder is unavailable: {error}"
                ));
            }
            _ => {}
        }
    }
    if fs::read_dir(&destination).map_or(true, |mut entries| entries.next().is_some()) {
        return Err("The project share mount folder is not empty.".into());
    }
    parts.tool.mount(&expected, &destination, password)?;
    let rows: Vec<MountEntry> = parts
        .tool
        .entries()?
        .into_iter()
        .filter(|entry| Path::new(&entry.path) == destination)
        .collect();
    if rows.len() != 1 || !rows[0].read_only() {
        return Err("The SMB mount has no proven read-only result.".into());
    }
    save()
}

/// Mount every registered view again. A failed view keeps its reason in its
/// `mount-error.json`; the other views still mount.
pub(crate) fn remount_registered(parts: &Remount<'_>) -> Result<(), String> {
    let mut records: Vec<PathBuf> = fs::read_dir(parts.state_dir.join(PROJECTS))
        .map(|entries| {
            entries
                .filter_map(Result::ok)
                .map(|entry| entry.path().join(MOUNT_RECORD))
                .filter(|record| record.is_file())
                .collect()
        })
        .unwrap_or_default();
    if records.is_empty() {
        return Ok(());
    }
    records.sort();
    let password = credential(parts.state_dir, parts.guest)?;
    for record in records {
        let Some(directory) = record.parent() else {
            continue;
        };
        let saved = read_object(&record).unwrap_or_default();
        let outcome = project_id(saved.get("projectId")).and_then(|identity| {
            let mut project = ensure_project(parts.guest, &identity)?;
            let share = parts
                .guest
                .call(
                    "share.status",
                    &json!({}),
                    None,
                    Duration::from_secs(SHARE_REQUEST_SECONDS),
                )
                .map_err(|error| format!("The guest share state is unavailable: {error}"))?;
            project.insert("share".into(), share);
            let hint = match saved.get("name").and_then(Value::as_str) {
                Some(name) if !name.is_empty() => name.to_owned(),
                _ => share_hint(
                    read_object(&directory.join(PROJECT_RECORD))
                        .as_ref()
                        .and_then(|project| project.get("source"))
                        .and_then(Value::as_str),
                ),
            };
            mount_project(parts, &identity, &project, &password, &hint).map(|()| identity)
        });
        let report = directory.join(MOUNT_ERROR);
        match outcome {
            Ok(_) => match fs::remove_file(&report) {
                Err(error) if error.kind() != io::ErrorKind::NotFound => {
                    eprintln!("linux-vm: an old share mount failure stays: {error}");
                }
                _ => {}
            },
            Err(error) => {
                let error: String = error.chars().take(ERROR_CHARS).collect();
                let identity = saved.get("projectId").cloned().unwrap_or(Value::Null);
                let at = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0.0, |elapsed| elapsed.as_secs_f64());
                if let Err(failure) = save_private(
                    &report,
                    &json!({"projectId": identity, "error": error, "at": at}),
                ) {
                    eprintln!("linux-vm: the share mount failure is not saved: {failure}");
                }
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests;
