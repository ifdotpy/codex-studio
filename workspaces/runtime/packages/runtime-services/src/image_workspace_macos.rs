//! macOS ASIF implementation of the Python `WorkspaceBackend` protocol.
#![cfg(target_os = "macos")]
use super::*;
use std::ffi::{CStr, c_char, c_void};
use std::io::{Read, Seek, SeekFrom};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Condvar, Mutex};
use std::thread;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventsGetCurrentEventId() -> u64;
    fn FSEventStreamCreate(
        allocator: *const c_void,
        callback: unsafe extern "C" fn(
            *mut c_void,
            *mut c_void,
            usize,
            *const *const c_char,
            *const u32,
            *const u64,
        ),
        context: *const FSEventStreamContext,
        paths: *mut c_void,
        since: u64,
        latency: f64,
        flags: u32,
    ) -> *mut c_void;
    fn FSEventStreamSetDispatchQueue(stream: *mut c_void, queue: *mut c_void);
    fn FSEventStreamStart(stream: *mut c_void) -> bool;
    fn FSEventStreamStop(stream: *mut c_void);
    fn FSEventStreamInvalidate(stream: *mut c_void);
    fn FSEventStreamRelease(stream: *mut c_void);
}

#[link(name = "CoreFoundation", kind = "framework")]
unsafe extern "C" {
    fn CFStringCreateWithFileSystemRepresentation(
        allocator: *const c_void,
        path: *const c_char,
    ) -> *mut c_void;
    fn CFArrayCreate(
        allocator: *const c_void,
        values: *const *const c_void,
        count: isize,
        callbacks: *const c_void,
    ) -> *mut c_void;
    fn CFRelease(value: *const c_void);
}

#[link(name = "System")]
unsafe extern "C" {
    fn dispatch_get_global_queue(identifier: isize, flags: usize) -> *mut c_void;
}

#[repr(C)]
struct FSEventStreamContext {
    version: isize,
    info: *mut c_void,
    retain: *const c_void,
    release: *const c_void,
    copy_description: *const c_void,
}

#[derive(Clone, Debug)]
struct FSEvent {
    path: PathBuf,
    flags: u32,
    id: u64,
}

#[derive(Default)]
struct EventCollector {
    events: Mutex<Vec<FSEvent>>,
    done: Mutex<bool>,
    ready: Condvar,
}

unsafe extern "C" fn collect_fsevents(
    _stream: *mut c_void,
    info: *mut c_void,
    count: usize,
    paths: *const *const c_char,
    flags: *const u32,
    ids: *const u64,
) {
    if info.is_null() || paths.is_null() || flags.is_null() || ids.is_null() {
        return;
    }
    // SAFETY: `info` points to the stack-owned collector, which lives until the stream is released.
    let collector = unsafe { &*(info.cast::<EventCollector>()) };
    let paths = unsafe { std::slice::from_raw_parts(paths, count) };
    let flags = unsafe { std::slice::from_raw_parts(flags, count) };
    let ids = unsafe { std::slice::from_raw_parts(ids, count) };
    if let Ok(mut events) = collector.events.lock() {
        for index in 0..count {
            if !paths[index].is_null() {
                let path = unsafe { CStr::from_ptr(paths[index]) }
                    .to_string_lossy()
                    .into_owned();
                events.push(FSEvent {
                    path: PathBuf::from(path),
                    flags: flags[index],
                    id: ids[index],
                });
            }
        }
    }
    if flags.iter().any(|value| value & 0x10 != 0) {
        if let Ok(mut done) = collector.done.lock() {
            *done = true;
            collector.ready.notify_all();
        }
    }
}

#[derive(Debug)]
enum FSEventError {
    HistoryLost,
    Other(String),
}

fn read_events(root: &Path, since: u64) -> Result<(Vec<FSEvent>, u64), FSEventError> {
    // SAFETY: CoreServices documents this as a process-wide monotonic event counter.
    let current = unsafe { FSEventsGetCurrentEventId() };
    if since >= current {
        return Ok((Vec::new(), current));
    }
    let path = std::ffi::CString::new(root.as_os_str().as_encoded_bytes())
        .map_err(|error| FSEventError::Other(error.to_string()))?;
    // SAFETY: CoreFoundation copies the filesystem representation into an owned CFString.
    let cf_path =
        unsafe { CFStringCreateWithFileSystemRepresentation(std::ptr::null(), path.as_ptr()) };
    if cf_path.is_null() {
        return Err(FSEventError::Other("Could not create FSEvents path".into()));
    }
    let values = [cf_path as *const c_void];
    // SAFETY: `values` remains valid for the duration of CFArrayCreate and the array retains values.
    let array = unsafe { CFArrayCreate(std::ptr::null(), values.as_ptr(), 1, std::ptr::null()) };
    if array.is_null() {
        unsafe { CFRelease(cf_path) };
        return Err(FSEventError::Other(
            "Could not create FSEvents path array".into(),
        ));
    }
    let collector = EventCollector::default();
    let mut context = FSEventStreamContext {
        version: 0,
        info: (&collector as *const EventCollector).cast_mut().cast(),
        retain: std::ptr::null(),
        release: std::ptr::null(),
        copy_description: std::ptr::null(),
    };
    // SAFETY: The callback and context outlive the stream; path array is valid until stream creation returns.
    let stream = unsafe {
        FSEventStreamCreate(
            std::ptr::null(),
            collect_fsevents,
            &context,
            array,
            since,
            0.1,
            0x10 | 0x2,
        )
    };
    if stream.is_null() {
        unsafe { CFRelease(array) };
        unsafe { CFRelease(cf_path) };
        return Err(FSEventError::Other("FSEventStreamCreate failed".into()));
    }
    let queue = unsafe { dispatch_get_global_queue(0, 0) };
    unsafe { FSEventStreamSetDispatchQueue(stream, queue) };
    if !unsafe { FSEventStreamStart(stream) } {
        unsafe { FSEventStreamRelease(stream) };
        unsafe { CFRelease(array) };
        unsafe { CFRelease(cf_path) };
        return Err(FSEventError::Other("FSEventStreamStart failed".into()));
    }
    let done = collector
        .done
        .lock()
        .map_err(|_| FSEventError::Other("FSEvents lock poisoned".into()))?;
    let (done, timeout) = collector
        .ready
        .wait_timeout_while(done, Duration::from_secs(120), |done| !*done)
        .map_err(|_| FSEventError::Other("FSEvents wait poisoned".into()))?;
    drop(done);
    unsafe {
        FSEventStreamStop(stream);
        FSEventStreamInvalidate(stream);
        FSEventStreamRelease(stream);
    }
    unsafe { CFRelease(array) };
    unsafe { CFRelease(cf_path) };
    if timeout.timed_out() {
        return Err(FSEventError::Other("FSEvents history timed out".into()));
    }
    let events = collector
        .events
        .lock()
        .map_err(|_| FSEventError::Other("FSEvents lock poisoned".into()))?
        .clone();
    if super::fsevents_history_lost(
        &events
            .iter()
            .map(|event| (event.id, event.flags))
            .collect::<Vec<_>>(),
    ) {
        return Err(FSEventError::HistoryLost);
    }
    let newest = events.iter().map(|event| event.id).max().unwrap_or(since);
    Ok((events, newest))
}

fn is_excluded(path: &Path, excludes: &[String]) -> bool {
    excludes
        .iter()
        .any(|item| path == Path::new(item) || path.starts_with(item))
}

fn nested_excludes(path: &Path, excludes: &[String]) -> Vec<PathBuf> {
    excludes
        .iter()
        .filter_map(|item| {
            Path::new(item)
                .strip_prefix(path)
                .ok()
                .map(Path::to_path_buf)
        })
        .collect()
}

fn rsync_folder(
    source: &Path,
    target: &Path,
    excludes: &[String],
) -> Result<(), ImageWorkspaceError> {
    let mut args = vec!["-a".to_owned(), "--delete".to_owned()];
    args.extend(excludes.iter().map(|item| {
        let item = item.trim_matches('/');
        if item == ".git/index" || item.ends_with("/.git/index") {
            format!("--exclude=/{item}")
        } else {
            format!("--exclude=/{item}/***")
        }
    }));
    args.push(format!("{}/", source.display()));
    args.push(format!("{}/", target.display()));
    run(
        "/usr/bin/rsync",
        &args.iter().map(String::as_str).collect::<Vec<_>>(),
        Duration::from_secs(1800),
    )?;
    Ok(())
}

fn sync_event_path(
    source: &Path,
    target: &Path,
    excludes: &[PathBuf],
) -> Result<(), ImageWorkspaceError> {
    let metadata = match fs::symlink_metadata(source) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if target.is_dir() && !target.is_symlink() {
                fs::remove_dir_all(target)?;
            } else {
                let _remove = fs::remove_file(target);
            }
            return Ok(());
        }
        Err(error) => return Err(error.into()),
    };
    if metadata.is_dir() && !metadata.file_type().is_symlink() {
        let excludes = excludes
            .iter()
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<Vec<_>>();
        return rsync_folder(source, target, &excludes);
    }
    if let Some(parent) = target.parent() {
        fs::create_dir_all(parent)?;
    }
    if target.is_dir() && !target.is_symlink() {
        fs::remove_dir_all(target)?;
    } else {
        let _remove = fs::remove_file(target);
    }
    if metadata.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(source)?, target)?;
    } else {
        fs::copy(source, target)?;
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, Default)]
pub struct MacAsifBackend;

impl WorkspaceBackend for MacAsifBackend {
    fn supported(&self, root: &Path) -> Result<(), ImageWorkspaceError> {
        if !root.is_dir() {
            return Err(ImageWorkspaceError::Backend(
                "Repository folder does not exist".into(),
            ));
        }
        if !Path::new("/usr/sbin/diskutil").exists() {
            return Err(ImageWorkspaceError::Backend(
                "diskutil is unavailable".into(),
            ));
        }
        Ok(())
    }

    fn current_event_id(&self, _root: &Path) -> Result<Option<Value>, ImageWorkspaceError> {
        // Mirrors codex_workspace_macos.current_event_id; delta sync falls back to a full rsync.
        // SAFETY: CoreServices exposes this zero-argument function as a process-wide event counter.
        Ok(Some(json!(unsafe { FSEventsGetCurrentEventId() })))
    }

    fn base_needs_refresh(&self, root: &Path, state: &Value) -> Result<bool, ImageWorkspaceError> {
        let Some(prior) = state.get("token").and_then(Value::as_u64) else {
            return Ok(false);
        };
        let events = match read_events(root, prior) {
            Ok((events, _)) => events,
            Err(_) => return Ok(true),
        };
        if events.len() >= 1000 {
            return Ok(true);
        }
        let changed_bytes = events
            .iter()
            .filter_map(|event| {
                let path = if event.path.is_absolute() {
                    event.path.clone()
                } else {
                    root.join(&event.path)
                };
                path.metadata()
                    .ok()
                    .filter(|metadata| metadata.is_file())
                    .map(|metadata| metadata.len())
            })
            .fold(0u64, u64::saturating_add);
        let base_bytes = state
            .get("image")
            .and_then(Value::as_str)
            .and_then(|path| self.private_bytes(Path::new(path)).ok().flatten())
            .unwrap_or(0);
        Ok(changed_bytes >= (16 * 1024 * 1024u64).max(base_bytes.saturating_mul(35) / 100))
    }

    fn open_base_staging(
        &self,
        _root: &Path,
        key: &str,
        version: &str,
    ) -> Result<WorkspaceBaseStaging, ImageWorkspaceError> {
        let home = std::env::var_os("HOME")
            .map(PathBuf::from)
            .unwrap_or_default();
        let store = std::env::var_os(STORE_ENV)
            .map(PathBuf::from)
            .unwrap_or_else(|| home.join(".local/state/codex-agents/workspaces"));
        let version_path = store.join("bases").join(key).join("versions").join(version);
        let image = version_path.join("base.asif");
        let mount = version_path.join("mount");
        fs::create_dir_all(&mount)?;
        run(
            "/usr/sbin/diskutil",
            &[
                "image",
                "create",
                "blank",
                "--format",
                "ASIF",
                "--size",
                "200g",
                "--volumeName",
                "StudioBase",
                "--fs",
                "APFS",
                image.to_str().ok_or(ImageWorkspaceError::InvalidState)?,
            ],
            Duration::from_secs(900),
        )?;
        attach(&image, &mount)?;
        File::create(mount.join(".metadata_never_index"))?;
        let root = mount.join("repo");
        fs::create_dir_all(&root)?;
        fs::create_dir_all(mount.join("home"))?;
        fs::create_dir_all(mount.join("tmp"))?;
        Ok(WorkspaceBaseStaging {
            root,
            token: None,
            version_path,
            image: Some(image),
            mount: Some(mount),
            version: version.to_owned(),
            refresh: false,
        })
    }

    fn copy_base_tree(
        &self,
        root: &Path,
        destination: &Path,
        excludes: &[String],
        check: &dyn Fn() -> Result<(), ImageWorkspaceError>,
    ) -> Result<(), ImageWorkspaceError> {
        copy_tree(root, destination, excludes, check)
    }

    fn seal_base(&self, staging: &WorkspaceBaseStaging) -> Result<Value, ImageWorkspaceError> {
        if let Some(mount) = &staging.mount {
            detach(mount, false)?;
        }
        Ok(json!({"image":staging.image,"versionPath":staging.version_path,"token":staging.token}))
    }

    fn clone_workspace(
        &self,
        image: &Path,
        agent_dir: &Path,
    ) -> Result<PathBuf, ImageWorkspaceError> {
        fs::create_dir_all(agent_dir)?;
        let layer = agent_dir.join("workspace.asif");
        if !layer.exists() {
            match run(
                "/bin/cp",
                &["-c", path_arg(image)?, path_arg(&layer)?],
                Duration::from_secs(300),
            ) {
                Ok(_) => {}
                Err(_) => {
                    fs::copy(image, &layer)?;
                }
            }
        }
        Ok(layer)
    }

    fn mount_workspace(
        &self,
        layer: &Path,
        mount: &Path,
        _base_image: Option<&Path>,
    ) -> Result<WorkspaceMount, ImageWorkspaceError> {
        fs::create_dir_all(mount)?;
        if !mount.is_mount() {
            attach(layer, mount)?;
        }
        Ok(WorkspaceMount {
            mount: mount.to_string_lossy().into_owned(),
            layer: Some(layer.to_string_lossy().into_owned()),
            base_image: None,
            pid: None,
        })
    }

    fn sync_delta(
        &self,
        root: &Path,
        target: &Path,
        token: Option<&Value>,
        excludes: &[String],
    ) -> Result<WorkspaceDelta, ImageWorkspaceError> {
        let since = token.and_then(Value::as_u64).unwrap_or(0);
        let (events, newest) = match read_events(root, since) {
            Ok(events) => events,
            Err(FSEventError::HistoryLost) => {
                rsync_folder(root, target, excludes)?;
                return Ok(WorkspaceDelta {
                    token: self.current_event_id(root)?,
                    changed_paths: vec![".".into()],
                    history_lost: true,
                    scan_paths: vec![".".into()],
                    refresh_base: true,
                });
            }
            Err(FSEventError::Other(error)) => return Err(ImageWorkspaceError::Backend(error)),
        };
        let mut changed_paths = BTreeSet::new();
        let mut scan_paths = BTreeSet::new();
        for event in events {
            let relative = if event.path.is_absolute() {
                let Ok(relative) = event.path.strip_prefix(root) else {
                    continue;
                };
                relative.to_path_buf()
            } else {
                event.path.clone()
            };
            if is_excluded(&relative, excludes) {
                continue;
            }
            let label = relative.to_string_lossy().replace('\\', "/");
            let label = if label.is_empty() {
                ".".to_owned()
            } else {
                label
            };
            if event.flags & 0x1 != 0 {
                changed_paths.insert(label.clone());
                scan_paths.insert(label.clone());
                let source = root.join(&relative);
                let destination = target.join(&relative);
                if relative.as_os_str().is_empty() {
                    rsync_folder(root, target, excludes)?;
                    return Ok(WorkspaceDelta {
                        token: Some(json!(newest)),
                        changed_paths: vec![".".into()],
                        history_lost: false,
                        scan_paths: vec![".".into()],
                        refresh_base: true,
                    });
                }
                let nested = nested_excludes(&relative, excludes);
                if source.is_dir() && !source.is_symlink() {
                    rsync_folder(
                        &source,
                        &destination,
                        &nested
                            .iter()
                            .map(|p| p.to_string_lossy().into_owned())
                            .collect::<Vec<_>>(),
                    )?;
                } else {
                    sync_event_path(&source, &destination, &nested)?;
                }
            } else if relative.as_os_str().is_empty() {
                continue;
            } else {
                changed_paths.insert(label);
                sync_event_path(
                    &root.join(&relative),
                    &target.join(&relative),
                    &nested_excludes(&relative, excludes),
                )?;
            }
        }
        let root_scan = scan_paths.contains(".");
        Ok(WorkspaceDelta {
            token: Some(json!(newest)),
            changed_paths: changed_paths.into_iter().collect(),
            history_lost: false,
            scan_paths: scan_paths.into_iter().collect(),
            refresh_base: root_scan,
        })
    }

    fn unmount_workspace(&self, mount: &Path, force: bool) -> Result<Value, ImageWorkspaceError> {
        if !mount.is_mount() {
            return Ok(json!({"method":"already-unmounted","terminatedPids":0}));
        }
        let mount_arg = path_arg(mount)?;
        let info = run(
            "/usr/sbin/diskutil",
            &["info", mount_arg],
            Duration::from_secs(10),
        )?;
        let device = String::from_utf8_lossy(&info.stdout)
            .lines()
            .find_map(|line| line.trim().strip_prefix("Device Node:").map(str::trim))
            .filter(|value| value.starts_with("/dev/disk"))
            .ok_or_else(|| {
                ImageWorkspaceError::Backend("Could not resolve ASIF device for mount".into())
            })?;
        let first_eject = run(
            "/usr/sbin/diskutil",
            &["eject", device],
            Duration::from_secs(20),
        );
        if first_eject.is_ok() && !mount.is_mount() {
            return Ok(json!({"method":"eject","terminatedPids":0}));
        }
        if !force {
            return Err(first_eject.err().unwrap_or_else(|| {
                ImageWorkspaceError::Backend("ASIF mount remained attached after eject".into())
            }));
        }

        let lsof = run_allow_failure(
            "/usr/sbin/lsof",
            &["-t", "+f", "--", mount_arg],
            Duration::from_secs(2),
        );
        let (holders, lsof_timed_out) = match lsof {
            Ok(output) => (
                String::from_utf8_lossy(&output.stdout)
                    .split_whitespace()
                    .filter_map(|value| value.parse::<u32>().ok())
                    .collect::<BTreeSet<_>>(),
                false,
            ),
            Err(ImageWorkspaceError::Backend(message)) if message.contains("timed out") => {
                (BTreeSet::new(), true)
            }
            Err(error) => return Err(error),
        };
        let mut signaled = 0usize;
        for pid in &holders {
            if process_exists(*pid) && signal_process(*pid, "-TERM") {
                signaled += 1;
            }
        }
        let deadline = std::time::Instant::now() + Duration::from_secs(1);
        while std::time::Instant::now() < deadline && holders.iter().any(|pid| process_exists(*pid))
        {
            thread::sleep(Duration::from_millis(100));
        }
        for pid in &holders {
            if process_exists(*pid) {
                let _kill_result = signal_process(*pid, "-KILL");
            }
        }
        thread::sleep(Duration::from_millis(100));
        if run(
            "/usr/sbin/diskutil",
            &["eject", device],
            Duration::from_secs(4),
        )
        .is_ok()
            && !mount.is_mount()
        {
            return Ok(
                json!({"method":"eject-after-terminating-holders","holdersFound":holders.len(),"signaledPids":signaled,"lsofTimedOut":lsof_timed_out}),
            );
        }
        run(
            "/usr/sbin/diskutil",
            &["unmount", "force", mount_arg],
            Duration::from_secs(4),
        )?;
        run(
            "/usr/sbin/diskutil",
            &["eject", device],
            Duration::from_secs(4),
        )?;
        if mount.is_mount() {
            return Err(ImageWorkspaceError::Backend(
                "ASIF mount remained attached after forced unmount and eject".into(),
            ));
        }
        Ok(
            json!({"method":"force-unmount-after-terminating-holders","holdersFound":holders.len(),"signaledPids":signaled,"lsofTimedOut":lsof_timed_out}),
        )
    }

    fn remove_layer(&self, agent_dir: &Path) -> Result<(), ImageWorkspaceError> {
        if agent_dir.exists() {
            fs::remove_dir_all(agent_dir)?;
        }
        Ok(())
    }

    fn remove_base_version(&self, path: &Path) -> Result<(), ImageWorkspaceError> {
        if !path.exists() {
            return Ok(());
        }
        let mount = if path.is_dir() {
            path.join("mount")
        } else {
            path.parent().unwrap_or(path).join("mount")
        };
        if mount.is_mount() {
            detach(&mount, false)?;
        }
        if path.is_dir() {
            fs::remove_dir_all(path)?;
        } else {
            fs::remove_file(path)?;
        }
        Ok(())
    }

    fn private_bytes(&self, path: &Path) -> Result<Option<u64>, ImageWorkspaceError> {
        let output = run(
            "/usr/bin/du",
            &["-sk", path_arg(path)?],
            Duration::from_secs(30),
        )?;
        let value = String::from_utf8_lossy(&output.stdout);
        Ok(value
            .split_whitespace()
            .next()
            .and_then(|s| s.parse::<u64>().ok())
            .map(|kb| kb.saturating_mul(1024)))
    }

    fn exec_prefix(&self) -> Vec<String> {
        Vec::new()
    }
}

fn process_exists(pid: u32) -> bool {
    run(
        "/bin/kill",
        &["-0", &pid.to_string()],
        Duration::from_secs(2),
    )
    .is_ok()
}

fn signal_process(pid: u32, signal: &str) -> bool {
    run(
        "/bin/kill",
        &[signal, &pid.to_string()],
        Duration::from_secs(2),
    )
    .is_ok()
}

fn path_arg(path: &Path) -> Result<&str, ImageWorkspaceError> {
    path.to_str().ok_or(ImageWorkspaceError::InvalidState)
}

fn attach(image: &Path, mount: &Path) -> Result<(), ImageWorkspaceError> {
    run(
        "/usr/sbin/diskutil",
        &[
            "image",
            "attach",
            "--nobrowse",
            "--mountPoint",
            path_arg(mount)?,
            path_arg(image)?,
        ],
        Duration::from_secs(300),
    )?;
    Ok(())
}

fn detach(mount: &Path, force: bool) -> Result<(), ImageWorkspaceError> {
    let mount = path_arg(mount)?;
    let args = if force {
        vec!["detach", "-force", mount]
    } else {
        vec!["detach", mount]
    };
    run("/usr/bin/hdiutil", &args, Duration::from_secs(30))?;
    Ok(())
}

fn run(program: &str, args: &[&str], timeout: Duration) -> Result<Output, ImageWorkspaceError> {
    let output = run_allow_failure(program, args, timeout)?;
    if !output.status.success() {
        return Err(ImageWorkspaceError::Backend(
            String::from_utf8_lossy(&output.stderr)
                .chars()
                .rev()
                .take(3000)
                .collect::<String>()
                .chars()
                .rev()
                .collect(),
        ));
    }
    Ok(output)
}

fn run_allow_failure(
    program: &str,
    args: &[&str],
    timeout: Duration,
) -> Result<Output, ImageWorkspaceError> {
    let mut stdout_file = tempfile::tempfile()?;
    let mut stderr_file = tempfile::tempfile()?;
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout_file.try_clone()?))
        .stderr(Stdio::from(stderr_file.try_clone()?))
        .spawn()?;
    let deadline = std::time::Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10))
            }
            Ok(None) | Err(_) => {
                terminate(&mut child);
                return Err(ImageWorkspaceError::Backend(format!(
                    "{program} timed out or could not be waited"
                )));
            }
        }
    }
    let status = child.wait()?;
    stdout_file.seek(SeekFrom::Start(0))?;
    stderr_file
        .seek(SeekFrom::End(-4096))
        .or_else(|_| stderr_file.seek(SeekFrom::Start(0)))?;
    let mut stdout = Vec::new();
    let mut stderr = Vec::new();
    stdout_file.take(1024 * 1024).read_to_end(&mut stdout)?;
    stderr_file.take(4096).read_to_end(&mut stderr)?;
    let output = Output {
        status,
        stdout,
        stderr,
    };
    Ok(output)
}

fn terminate(child: &mut Child) {
    match child.kill() {
        Ok(()) | Err(_) => {}
    }
    match child.wait() {
        Ok(_) | Err(_) => {}
    }
}

fn copy_tree(
    source: &Path,
    target: &Path,
    excludes: &[String],
    check: &dyn Fn() -> Result<(), ImageWorkspaceError>,
) -> Result<(), ImageWorkspaceError> {
    fn copy(
        source: &Path,
        target: &Path,
        root: &Path,
        excludes: &[String],
        check: &dyn Fn() -> Result<(), ImageWorkspaceError>,
    ) -> Result<(), ImageWorkspaceError> {
        check()?;
        fs::create_dir_all(target)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            let name = entry.file_name();
            let path = entry.path();
            let relative = path
                .strip_prefix(root)
                .unwrap_or(&path)
                .to_string_lossy()
                .replace('\\', "/");
            if excludes
                .iter()
                .any(|exclude| relative == *exclude || relative.starts_with(&format!("{exclude}/")))
            {
                continue;
            }
            let dest = target.join(&name);
            let kind = entry.file_type()?;
            if kind.is_dir() {
                copy(&path, &dest, root, excludes, check)?;
            } else if kind.is_symlink() {
                std::os::unix::fs::symlink(fs::read_link(path)?, dest)?;
            } else if kind.is_file() {
                fs::copy(path, dest)?;
            }
        }
        Ok(())
    }
    copy(source, target, source, excludes, check)
}
