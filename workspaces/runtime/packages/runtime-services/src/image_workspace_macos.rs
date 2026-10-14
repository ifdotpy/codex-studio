//! macOS ASIF implementation of the Python `WorkspaceBackend` protocol.
#![cfg(target_os = "macos")]
use super::*;
use std::io::{Read, Seek, SeekFrom};
use std::process::{Child, Command, Output, Stdio};
use std::thread;

#[link(name = "CoreServices", kind = "framework")]
unsafe extern "C" {
    fn FSEventsGetCurrentEventId() -> u64;
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
            return Ok(true);
        };
        Ok(self
            .current_event_id(root)?
            .and_then(|value| value.as_u64())
            .is_some_and(|current| current != prior))
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
        // Reconcile the full tree because this backend does not retain an
        // FSEvents stream between Rust server processes. The itemized output
        // preserves changed-path reporting; the root scan requests a base refresh.
        let mut args = vec![
            "-a".to_owned(),
            "--delete".to_owned(),
            "--itemize-changes".to_owned(),
            "--out-format=%i\t%n".to_owned(),
        ];
        args.extend(excludes.iter().map(|value| {
            let value = value.trim_matches('/');
            if value.ends_with("/.git/index") || value == ".git/index" {
                format!("--exclude=/{value}")
            } else {
                format!("--exclude=/{value}/***")
            }
        }));
        args.push(format!("{}/", root.display()));
        args.push(format!("{}/", target.display()));
        let args = args.iter().map(String::as_str).collect::<Vec<_>>();
        let output = run("/usr/bin/rsync", &args, Duration::from_secs(1800))?;
        let mut changed_paths = output
            .stdout
            .split(|byte| *byte == b'\n')
            .filter_map(|line| line.split_once(|byte| *byte == b'\t'))
            .map(|(_, path)| String::from_utf8_lossy(path).into_owned())
            .filter(|path| !path.is_empty())
            .collect::<Vec<_>>();
        changed_paths.sort();
        changed_paths.dedup();
        if output.stdout.len() >= 1024 * 1024 {
            changed_paths = vec![".".into()];
        }
        let current = self.current_event_id(root)?;
        let history_lost = token
            .and_then(Value::as_u64)
            .zip(current.as_ref().and_then(Value::as_u64))
            .is_some_and(|(prior, now)| prior != now);
        Ok(WorkspaceDelta {
            token: current,
            changed_paths,
            history_lost,
            scan_paths: if history_lost {
                vec![".".into()]
            } else {
                Vec::new()
            },
            refresh_base: history_lost,
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
