use std::fs::{self, File};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use studio_runtime_services::image_workspace::{
    ImageWorkspaceError, WorkspaceAttachment, WorkspaceBackend, WorkspaceBaseStaging,
    WorkspaceDelta, WorkspaceLifecycle, WorkspaceMount,
};

struct ScriptedBackend {
    store: PathBuf,
    failures: Mutex<Vec<&'static str>>,
    /// Mount point -> image. A detach moves the volume into `<image>.volume`.
    mounted: Mutex<BTreeMap<PathBuf, PathBuf>>,
}

impl ScriptedBackend {
    fn fail(&self, operation: &'static str) -> Result<(), ImageWorkspaceError> {
        let mut failures = self.failures.lock().expect("scripted failures");
        if failures.first() == Some(&operation) {
            failures.remove(0);
            return Err(ImageWorkspaceError::Backend(format!(
                "failpoint:{operation}"
            )));
        }
        Ok(())
    }
}

impl WorkspaceBackend for ScriptedBackend {
    fn supported(&self, _root: &Path) -> Result<(), ImageWorkspaceError> {
        Ok(())
    }

    fn current_event_id(&self, _root: &Path) -> Result<Option<Value>, ImageWorkspaceError> {
        Ok(Some(json!(41)))
    }

    fn open_base_staging(
        &self,
        _root: &Path,
        key: &str,
        version: &str,
    ) -> Result<WorkspaceBaseStaging, ImageWorkspaceError> {
        let version_path = self
            .store
            .join("bases")
            .join(key)
            .join("versions")
            .join(version);
        let root = version_path.join("repo");
        fs::create_dir_all(&root)?;
        let image = version_path.join("base.asif");
        File::create(&image)?;
        Ok(WorkspaceBaseStaging {
            root,
            token: Some(json!(41)),
            version_path,
            image: Some(image),
            mount: None,
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
        check()?;
        copy_tree(root, destination, root, excludes)
    }

    fn seal_base(&self, staging: &WorkspaceBaseStaging) -> Result<Value, ImageWorkspaceError> {
        self.fail("seal")?;
        Ok(json!({"image":staging.image,"versionPath":staging.version_path,"token":staging.token}))
    }

    fn clone_workspace(
        &self,
        image: &Path,
        agent_dir: &Path,
    ) -> Result<PathBuf, ImageWorkspaceError> {
        fs::create_dir_all(agent_dir)?;
        self.fail("clone")?;
        let layer = agent_dir.join("workspace.asif");
        fs::copy(image, &layer)?;
        Ok(layer)
    }

    fn mount_workspace(
        &self,
        layer: &Path,
        mount: &Path,
        _base_image: Option<&Path>,
    ) -> Result<WorkspaceMount, ImageWorkspaceError> {
        let volume = layer.with_extension("volume");
        if volume.exists() {
            fs::remove_dir(mount)?;
            fs::rename(&volume, mount)?;
        }
        fs::create_dir_all(mount.join("repo"))?;
        self.mounted
            .lock()
            .expect("mounted volumes")
            .insert(mount.to_path_buf(), layer.to_path_buf());
        Ok(WorkspaceMount {
            mount: mount.to_string_lossy().into_owned(),
            layer: Some(layer.to_string_lossy().into_owned()),
            ..WorkspaceMount::default()
        })
    }

    fn sync_delta(
        &self,
        _root: &Path,
        _target: &Path,
        token: Option<&Value>,
        _excludes: &[String],
    ) -> Result<WorkspaceDelta, ImageWorkspaceError> {
        Ok(WorkspaceDelta {
            token: token.cloned(),
            changed_paths: vec![".".into()],
            history_lost: false,
            scan_paths: vec![".".into()],
            refresh_base: false,
        })
    }

    fn unmount_workspace(&self, mount: &Path, _force: bool) -> Result<Value, ImageWorkspaceError> {
        let layer = self.mounted.lock().expect("mounted volumes").remove(mount);
        if let Some(layer) = layer {
            fs::rename(mount, layer.with_extension("volume"))?;
            fs::create_dir(mount)?;
        }
        Ok(json!({"method":"scripted","terminatedPids":0}))
    }

    fn is_mounted(&self, path: &Path) -> Result<bool, ImageWorkspaceError> {
        Ok(self
            .mounted
            .lock()
            .expect("mounted volumes")
            .contains_key(path))
    }

    fn attachments(&self) -> Result<Vec<WorkspaceAttachment>, ImageWorkspaceError> {
        Ok(self
            .mounted
            .lock()
            .expect("mounted volumes")
            .iter()
            .map(|(mount, layer)| WorkspaceAttachment {
                image: Some(layer.clone()),
                mounts: vec![mount.clone()],
                device: None,
            })
            .collect())
    }

    fn remove_layer(&self, agent_dir: &Path) -> Result<(), ImageWorkspaceError> {
        fs::remove_dir_all(agent_dir)?;
        Ok(())
    }

    fn remove_base_version(&self, path: &Path) -> Result<(), ImageWorkspaceError> {
        if path.exists() {
            fs::remove_dir_all(path)?;
        }
        Ok(())
    }

    fn private_bytes(&self, _path: &Path) -> Result<Option<u64>, ImageWorkspaceError> {
        Ok(Some(0))
    }

    fn available_bytes(&self, _path: &Path) -> Result<u64, ImageWorkspaceError> {
        Ok(u64::MAX)
    }

    fn exec_prefix(&self) -> Vec<String> {
        Vec::new()
    }
}

fn copy_tree(
    source: &Path,
    target: &Path,
    root: &Path,
    excludes: &[String],
) -> Result<(), ImageWorkspaceError> {
    fs::create_dir_all(target).map_err(|error| {
        ImageWorkspaceError::Backend(format!("mkdir {}: {error}", target.display()))
    })?;
    for entry in fs::read_dir(source)? {
        let entry = entry.map_err(|error| {
            ImageWorkspaceError::Backend(format!("read entry in {}: {error}", source.display()))
        })?;
        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        if excludes
            .iter()
            .any(|value| relative == *value || relative.starts_with(&format!("{value}/")))
        {
            continue;
        }
        let destination = target.join(entry.file_name());
        let kind = entry.file_type().map_err(|error| {
            ImageWorkspaceError::Backend(format!("stat {}: {error}", path.display()))
        })?;
        if kind.is_dir() {
            copy_tree(&path, &destination, root, excludes)?;
        } else if kind.is_symlink() {
            std::os::unix::fs::symlink(fs::read_link(path)?, destination)?;
        } else if kind.is_file() {
            fs::copy(&path, &destination).map_err(|error| {
                ImageWorkspaceError::Backend(format!(
                    "copy {} to {}: {error}",
                    path.display(),
                    destination.display()
                ))
            })?;
        }
    }
    Ok(())
}

fn wait_for_build(manager: &WorkspaceLifecycle<ScriptedBackend>, root: &Path) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let status = manager.base_status(root).expect("base state");
        if status["state"] != "building" {
            return status;
        }
        assert!(Instant::now() < deadline, "base build completion");
        thread::sleep(Duration::from_millis(5));
    }
}

fn normalize(value: Value, store: &Path) -> Value {
    match value {
        Value::Object(values) => Value::Object(
            values
                .into_iter()
                .filter(|(key, _)| {
                    !matches!(
                        key.as_str(),
                        "createdAt"
                            | "startedAt"
                            | "failedAt"
                            | "builderPid"
                            | "attachedAt"
                            | "checkedAt"
                            | "imageDevice"
                            | "imageInode"
                    )
                })
                .map(|(key, value)| (key, normalize(value, store)))
                .collect(),
        ),
        Value::Array(values) => Value::Array(
            values
                .into_iter()
                .map(|value| normalize(value, store))
                .collect(),
        ),
        Value::String(value) => {
            let value = value.replace(store.to_string_lossy().as_ref(), "<store>");
            let version = regex::Regex::new(r"v-[0-9]+").expect("version regex");
            Value::String(version.replace_all(&value, "v-{version}").into_owned())
        }
        scalar => scalar,
    }
}

fn read_row(path: &Path, store: &Path) -> Value {
    normalize(
        serde_json::from_slice(&fs::read(path).expect("persisted row")).expect("JSON row"),
        store,
    )
}

fn repo_key(root: &Path) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(root.to_string_lossy().as_bytes()))[..32]
        .to_owned()
}

fn make_git_project(root: &Path) {
    fs::create_dir_all(root).expect("project root");
    fs::write(root.join("tracked.txt"), "tracked\n").expect("tracked fixture");
    let init = Command::new("git")
        .args(["init", "-q"])
        .current_dir(root)
        .status()
        .expect("git init");
    assert!(init.success());
    let add = Command::new("git")
        .args(["add", "tracked.txt"])
        .current_dir(root)
        .status()
        .expect("git add");
    assert!(add.success());
    let commit = Command::new("git")
        .args([
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.invalid",
            "commit",
            "-qm",
            "fixture",
        ])
        .current_dir(root)
        .status()
        .expect("git commit");
    assert!(commit.success());
}

#[test]
fn lifecycle_writes_state_rows_through_failure_retry_archive_and_plain_folder_refusal() {
    let fixture = tempfile::tempdir().expect("fixture");
    let root = fixture.path().join("project");
    let rust_store = fixture.path().join("rust-store");
    make_git_project(&root);
    fs::create_dir_all(&rust_store).expect("Rust store");

    let manager = WorkspaceLifecycle::with_backend(
        &rust_store,
        ScriptedBackend {
            store: rust_store.clone(),
            failures: Mutex::new(vec!["seal", "clone"]),
            mounted: Mutex::new(BTreeMap::new()),
        },
    );
    manager.start_base_build(&root).expect("start base build");
    assert_eq!(wait_for_build(&manager, &root)["state"], "failed");
    let key = repo_key(&root);
    let rust_base = rust_store.join("bases").join(&key).join("base.json");
    let mut rust_rows =
        vec![json!({"phase":"seal-failed","base":read_row(&rust_base, &rust_store)})];
    manager
        .start_base_build_retry(&root, true)
        .expect("retry base build");
    let build = wait_for_build(&manager, &root);
    assert_eq!(
        build["state"],
        "ready",
        "base row: {}",
        fs::read_to_string(&rust_base).unwrap_or_default()
    );
    rust_rows.push(json!({"phase":"base-ready","base":read_row(&rust_base, &rust_store)}));

    assert!(manager.create_workspace(&root, "agent-corpus").is_err());
    let rust_agent = rust_store.join("agents/agent-corpus/agent.json");
    rust_rows.push(json!({"phase":"clone-failed","agent":read_row(&rust_agent, &rust_store)}));
    manager
        .create_workspace(&root, "agent-corpus")
        .expect("workspace retry");
    rust_rows.push(json!({"phase":"workspace-ready","agent":read_row(&rust_agent, &rust_store)}));
    manager.archive_workspace("agent-corpus").expect("archive");
    rust_rows
        .push(json!({"phase":"workspace-archived","agent":read_row(&rust_agent, &rust_store)}));
    manager.ensure_mounted("agent-corpus").expect("remount");
    rust_rows
        .push(json!({"phase":"workspace-remounted","agent":read_row(&rust_agent, &rust_store)}));
    rust_rows.push(json!({
        "phase": "check-mounted",
        "report": normalize(manager.check_mounts().expect("check mounts"), &rust_store),
    }));
    manager
        .archive_workspace("agent-corpus")
        .expect("archive again");
    let mount = rust_store.join("mnt/agent-corpus");
    fs::write(mount.join("restored-copy.txt"), "only copy\n").expect("plain folder file");
    let mut refusals = serde_json::Map::new();
    for (name, result) in [
        ("ensure", manager.ensure_mounted("agent-corpus")),
        ("archive", manager.archive_workspace("agent-corpus")),
        ("remove", manager.remove_workspace("agent-corpus", false)),
    ] {
        match result {
            Err(error @ ImageWorkspaceError::MountCheck { .. }) => {
                refusals.insert(
                    name.to_owned(),
                    normalize(Value::String(error.to_string()), &rust_store),
                );
            }
            other => panic!("{name} used a plain folder at the mount point: {other:?}"),
        }
    }
    rust_rows.push(json!({
        "phase": "plain-folder-refused",
        "refusals": refusals,
        "imageKept": rust_store.join("agents/agent-corpus/workspace.asif").exists(),
        "report": normalize(manager.check_mounts().expect("check mounts"), &rust_store),
    }));
    fs::remove_file(mount.join("restored-copy.txt")).expect("clear plain folder");
    manager
        .ensure_mounted("agent-corpus")
        .expect("attach after clearing");
    rust_rows
        .push(json!({"phase":"plain-folder-cleared","agent":read_row(&rust_agent, &rust_store)}));

    let phases: Vec<&str> = rust_rows
        .iter()
        .map(|row| row["phase"].as_str().expect("phase"))
        .collect();
    assert_eq!(
        phases,
        [
            "seal-failed",
            "base-ready",
            "clone-failed",
            "workspace-ready",
            "workspace-archived",
            "workspace-remounted",
            "check-mounted",
            "plain-folder-refused",
            "plain-folder-cleared"
        ]
    );
}
