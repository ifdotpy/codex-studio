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
        Ok(Some(json!(unsafe { FSEventsGetCurrentEventId() })))
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
        _token: Option<&Value>,
        excludes: &[String],
    ) -> Result<WorkspaceDelta, ImageWorkspaceError> {
        let mut args = vec!["-a", "--delete"];
        let exclude_args: Vec<String> = excludes
            .iter()
            .map(|v| format!("--exclude=/{}/***", v.trim_matches('/')))
            .collect();
        args.extend(exclude_args.iter().map(String::as_str));
        let source = format!("{}/", root.display());
        let dest = format!("{}/", target.display());
        args.push(&source);
        args.push(&dest);
        run("/usr/bin/rsync", &args, Duration::from_secs(1800))?;
        Ok(WorkspaceDelta {
            token: None,
            changed_paths: vec![".".into()],
            history_lost: false,
            scan_paths: vec![".".into()],
            refresh_base: false,
        })
    }

    fn unmount_workspace(&self, mount: &Path, force: bool) -> Result<Value, ImageWorkspaceError> {
        if !mount.is_mount() {
            return Ok(json!({"method":"already-unmounted","terminatedPids":0}));
        }
        match detach(mount, false) {
            Ok(()) => Ok(json!({"method":"eject","terminatedPids":0})),
            Err(error) if force => {
                let mount_arg = path_arg(mount)?;
                run(
                    "/usr/sbin/diskutil",
                    &["unmount", "force", mount_arg],
                    Duration::from_secs(10),
                )?;
                detach(mount, true)?;
                Ok(
                    json!({"method":"force-unmount-after-eject-failure","terminatedPids":0,"priorError":error.to_string()}),
                )
            }
            Err(error) => Err(error),
        }
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
