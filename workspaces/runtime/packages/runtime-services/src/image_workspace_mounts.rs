//! Mount identity for image workspaces; port of the "Mount identity" section of
//! `codex_workspace_images.py`.
//!
//! "Something is mounted here" is not proof that the agent's own volume is
//! mounted here: a moved or restored state folder can leave a plain folder,
//! another volume, or a volume of an older image file at the mount point.
//! Studio uses a mount point only when its volume carries this agent's marker
//! and the image file still has the inode recorded at attach.
use super::*;
use studio_runtime_schema::shared_values::{
    IMAGE_MOUNT_PROBLEM_STATUSES, IMAGE_WORKSPACE_MARKER_NAME, IMAGE_WORKSPACE_MARKER_SCHEMA,
};

/// One attached disk image as the operating system reports it (`hdiutil info`).
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq)]
pub struct WorkspaceAttachment {
    pub image: Option<PathBuf>,
    pub mounts: Vec<PathBuf>,
    pub device: Option<String>,
}

/// The result of one mount point check. `status` is one of
/// `IMAGE_MOUNT_STATUSES` (owner: `codex_shared_values.py`).
#[derive(Clone, Debug)]
pub(super) struct MountProbe {
    agent_id: String,
    state: Value,
    image: Option<PathBuf>,
    pub(super) mount: PathBuf,
    pub(super) status: &'static str,
    pub(super) detail: String,
    mounted: bool,
    marker: bool,
    device: Option<String>,
    elsewhere: Vec<String>,
}

impl MountProbe {
    fn view(&self, repair: Option<&str>) -> Value {
        json!({
            "agentId": self.agent_id, "state": self.state, "image": self.image,
            "mount": self.mount, "status": self.status, "detail": self.detail,
            "mounted": self.mounted, "marker": self.marker, "device": self.device,
            "elsewhere": self.elsewhere, "repair": repair,
        })
    }
}

fn mount_error(agent_id: &str, status: &'static str, detail: String) -> ImageWorkspaceError {
    ImageWorkspaceError::MountCheck {
        agent_id: agent_id.to_owned(),
        status,
        detail,
    }
}

#[cfg(unix)]
fn file_identity(path: Option<&Path>) -> Option<(u64, u64)> {
    let metadata = fs::metadata(path?).ok()?;
    Some((metadata.dev(), metadata.ino()))
}

#[cfg(not(unix))]
fn file_identity(_path: Option<&Path>) -> Option<(u64, u64)> {
    None
}

/// `os.path.realpath`: resolve the existing ancestors, keep the missing tail.
fn real_path(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir().unwrap_or_default().join(path)
    };
    let mut tail = Vec::new();
    let mut current = absolute.as_path();
    loop {
        if let Ok(resolved) = fs::canonicalize(current) {
            return tail
                .iter()
                .rev()
                .fold(resolved, |path: PathBuf, part| path.join(part));
        }
        match (current.parent(), current.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name.to_os_string());
                current = parent;
            }
            _ => return absolute,
        }
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    real_path(left) == real_path(right)
}

fn read_marker(mount: &Path) -> Option<Value> {
    let value: Value =
        serde_json::from_slice(&fs::read(mount.join(IMAGE_WORKSPACE_MARKER_NAME)).ok()?).ok()?;
    (value.get("schema").and_then(Value::as_u64) == Some(IMAGE_WORKSPACE_MARKER_SCHEMA)
        && value.get("agentId").is_some_and(Value::is_string))
    .then_some(value)
}

fn marker_agent(marker: &Value) -> &str {
    marker.get("agentId").and_then(Value::as_str).unwrap_or("")
}

fn marker_identity(marker: &Value) -> (Option<u64>, Option<u64>) {
    (
        marker.get("imageDevice").and_then(Value::as_u64),
        marker.get("imageInode").and_then(Value::as_u64),
    )
}

/// The agent that owns an attached volume: its marker, else its image path.
fn attachment_agent(attachment: &WorkspaceAttachment) -> Option<String> {
    if let Some(marker) = attachment
        .mounts
        .iter()
        .find_map(|mount| read_marker(mount))
    {
        return Some(marker_agent(&marker).to_owned());
    }
    let image = attachment.image.as_deref()?;
    let parent = image.parent()?;
    (image.file_name()? == "workspace.asif" && parent.parent()?.file_name()? == "agents")
        .then(|| {
            parent
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
        })
        .flatten()
}

fn attachment_belongs(
    attachment: &WorkspaceAttachment,
    agent_id: &str,
    image: Option<&Path>,
    identity: Option<(u64, u64)>,
) -> bool {
    let markers = attachment
        .mounts
        .iter()
        .filter_map(|mount| read_marker(mount))
        .collect::<Vec<_>>();
    if !markers.is_empty() {
        return markers
            .iter()
            .any(|marker| marker_agent(marker) == agent_id);
    }
    let Some(attached) = attachment.image.as_deref() else {
        return false;
    };
    if image.is_some_and(|image| same_path(attached, image)) {
        return true;
    }
    if identity.is_some() && file_identity(Some(attached)) == identity {
        return true;
    }
    attachment_agent(attachment).as_deref() == Some(agent_id)
}

/// Names in a plain mount point folder; `None` when the path is not a folder.
fn mount_entries(mount: &Path) -> Result<Option<usize>, ImageWorkspaceError> {
    match fs::read_dir(mount) {
        Ok(entries) => Ok(Some(entries.count())),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Some(0)),
        Err(error) if error.kind() == std::io::ErrorKind::NotADirectory => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn identity_text(identity: (Option<u64>, Option<u64>)) -> String {
    let part = |value: Option<u64>| value.map_or("null".to_owned(), |value| value.to_string());
    format!("[{}, {}]", part(identity.0), part(identity.1))
}

fn studio_attachment(attachment: &WorkspaceAttachment) -> bool {
    attachment
        .mounts
        .iter()
        .any(|mount| read_marker(mount).is_some())
        || attachment
            .image
            .as_deref()
            .and_then(Path::file_name)
            .is_some_and(|name| name == "workspace.asif" || name == "base.asif")
}

fn base_staging_attachment(attachment: &WorkspaceAttachment, store: &Path) -> bool {
    let Some(image) = attachment.image.as_deref().map(real_path) else {
        return false;
    };
    let image = image.as_path();
    let Ok(relative) = image.strip_prefix(real_path(store)) else {
        return false;
    };
    let parts = relative
        .components()
        .map(|part| part.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    let mount = image.parent().unwrap_or(image).join("mount");
    parts.len() == 5
        && parts[0] == "bases"
        && parts[2] == "versions"
        && parts[4] == "base.asif"
        && attachment.mounts.iter().all(|path| same_path(path, &mount))
}

fn inside(path: &Path, root: &Path) -> bool {
    real_path(path).starts_with(real_path(root))
}

impl<B: WorkspaceBackend + 'static> WorkspaceLifecycle<B> {
    /// Whether an attached Studio volume is this state's concern: its image or
    /// a mount point lies in this store, or it belongs to an agent of this
    /// store (for example after a move of the state folder). A volume of
    /// another state folder elsewhere is that state's concern.
    fn owns_attachment(
        &self,
        attachment: &WorkspaceAttachment,
        owners: &[(&str, Option<&Path>, Option<(u64, u64)>)],
    ) -> bool {
        attachment
            .image
            .as_deref()
            .is_some_and(|image| inside(image, &self.store))
            || attachment
                .mounts
                .iter()
                .any(|mount| inside(mount, &self.store))
            || owners.iter().any(|(agent_id, image, identity)| {
                attachment_belongs(attachment, agent_id, *image, *identity)
            })
    }

    fn agent_mount(&self, agent_id: &str, state: &Value) -> PathBuf {
        state
            .get("mount")
            .and_then(Value::as_str)
            .map(PathBuf::from)
            .unwrap_or_else(|| self.store.join("mnt").join(agent_id))
    }

    /// Classify one agent's mount point without changing anything.
    pub(super) fn probe_mount(
        &self,
        agent_id: &str,
        state: &Value,
        attachments: &[WorkspaceAttachment],
    ) -> Result<MountProbe, ImageWorkspaceError> {
        let image = state
            .get("image")
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
            .map(PathBuf::from);
        let mount = self.agent_mount(agent_id, state);
        let identity = file_identity(image.as_deref());
        let here = attachments
            .iter()
            .filter(|item| item.mounts.iter().any(|path| same_path(path, &mount)))
            .collect::<Vec<_>>();
        let elsewhere = attachments
            .iter()
            .filter(|item| !here.contains(item))
            .filter(|item| attachment_belongs(item, agent_id, image.as_deref(), identity))
            .flat_map(|item| {
                if item.mounts.is_empty() {
                    vec![item.image.as_ref().map_or_else(
                        || "an unmounted device".to_owned(),
                        |path| path.to_string_lossy().into_owned(),
                    )]
                } else {
                    item.mounts
                        .iter()
                        .map(|path| path.to_string_lossy().into_owned())
                        .collect()
                }
            })
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect::<Vec<_>>();
        let mounted = self.backend.is_mounted(&mount)?;
        let marker = if mounted { read_marker(&mount) } else { None };
        let shown_image = image
            .as_deref()
            .map_or_else(|| "None".to_owned(), |path| path.display().to_string());
        let mut status = "detached";
        let mut detail = format!(
            "{} is empty and no volume is attached there",
            mount.display()
        );
        if mounted {
            match &marker {
                Some(marker) if marker_agent(marker) != agent_id => {
                    status = "foreign-volume";
                    detail = format!(
                        "{} holds the volume of agent {}",
                        mount.display(),
                        marker_agent(marker)
                    );
                }
                Some(_) if identity.is_none() => {
                    status = "image-missing";
                    detail = format!(
                        "the image file {shown_image} is missing while its volume is attached at {}",
                        mount.display()
                    );
                }
                Some(marker)
                    if identity.map(|(device, inode)| (Some(device), Some(inode)))
                        != Some(marker_identity(marker)) =>
                {
                    let (device, inode) = identity.unwrap_or_default();
                    status = "image-replaced";
                    detail = format!(
                        "the volume at {} was attached from an earlier image file; {shown_image} changed since that attach (device and inode {}, now {})",
                        mount.display(),
                        identity_text(marker_identity(marker)),
                        identity_text((Some(device), Some(inode)))
                    );
                }
                Some(_) => {
                    status = "mounted";
                    detail = format!(
                        "{} holds the volume of image {shown_image}",
                        mount.display()
                    );
                }
                None if identity.is_some()
                    && here
                        .iter()
                        .any(|item| file_identity(item.image.as_deref()) == identity) =>
                {
                    status = "mounted";
                    detail = format!(
                        "{} holds the volume of image {shown_image}",
                        mount.display()
                    );
                }
                None => {
                    status = "foreign-volume";
                    detail = format!(
                        "{} holds a volume that is not the image {shown_image}",
                        mount.display()
                    );
                }
            }
        } else {
            match mount_entries(&mount)? {
                None => {
                    status = "plain-folder";
                    detail = format!("{} is a file, not a mount point folder", mount.display());
                }
                Some(count) if count > 0 => {
                    status = "plain-folder";
                    detail = format!(
                        "{} is a plain folder that holds {count} entries and no attached volume",
                        mount.display()
                    );
                }
                Some(_) if identity.is_none() => {
                    status = "image-missing";
                    detail = format!("the image file {shown_image} is missing");
                }
                Some(_) => {}
            }
        }
        if matches!(status, "mounted" | "detached") && !elsewhere.is_empty() {
            status = "attached-elsewhere";
            detail = format!(
                "the volume of this agent is also attached at {}",
                elsewhere.join(", ")
            );
        }
        let device = here.iter().find_map(|item| item.device.clone());
        Ok(MountProbe {
            agent_id: agent_id.to_owned(),
            state: state.get("state").cloned().unwrap_or(Value::Null),
            image,
            mount,
            status,
            detail,
            mounted,
            marker: marker.is_some(),
            device,
            elsewhere,
        })
    }

    fn record_mount_identity(
        &self,
        agent_id: &str,
        state: &mut Value,
        mount: &Path,
        device: Option<String>,
    ) -> Result<(), ImageWorkspaceError> {
        let image = state
            .get("image")
            .and_then(Value::as_str)
            .ok_or(ImageWorkspaceError::InvalidState)?
            .to_owned();
        let (device_id, inode) = file_identity(Some(Path::new(&image))).ok_or_else(|| {
            mount_error(
                agent_id,
                "image-missing",
                format!("the image file {image} is missing"),
            )
        })?;
        let attached_at = now_seconds();
        write_json(
            &mount.join(IMAGE_WORKSPACE_MARKER_NAME),
            &json!({
                "schema": IMAGE_WORKSPACE_MARKER_SCHEMA, "agentId": agent_id, "image": image,
                "imageDevice": device_id, "imageInode": inode, "attachedAt": attached_at,
            }),
        )?;
        state["mountIdentity"] = json!({
            "image": image, "imageDevice": device_id, "imageInode": inode, "mount": mount,
            "device": device, "attachedAt": attached_at,
        });
        Ok(())
    }

    /// Use the agent's own attached volume, or attach it at an empty mount point.
    pub(super) fn attach_verified(
        &self,
        agent_id: &str,
        state: &mut Value,
        base_image: Option<&Path>,
    ) -> Result<(), ImageWorkspaceError> {
        let attachments = self.backend.attachments()?;
        let mut probe = self.probe_mount(agent_id, state, &attachments)?;
        let mount = probe.mount.clone();
        match probe.status {
            "mounted" => {
                if probe.marker && state.get("mountIdentity").is_some_and(Value::is_object) {
                    return Ok(());
                }
            }
            "detached" => {
                let image = state
                    .get("image")
                    .and_then(Value::as_str)
                    .ok_or(ImageWorkspaceError::InvalidState)?
                    .to_owned();
                fs::create_dir_all(&mount)?;
                self.backend
                    .mount_workspace(Path::new(&image), &mount, base_image)?;
                if !self.backend.is_mounted(&mount)? {
                    return Err(mount_error(
                        agent_id,
                        "detached",
                        format!(
                            "the attach of {image} returned, but no volume is mounted at {}",
                            mount.display()
                        ),
                    ));
                }
                if let Some(marker) = read_marker(&mount)
                    && marker_agent(&marker) != agent_id
                {
                    self.backend.unmount_workspace(&mount, false)?;
                    return Err(mount_error(
                        agent_id,
                        "foreign-volume",
                        format!(
                            "the image {image} holds the volume of agent {}; Studio detached it again",
                            marker_agent(&marker)
                        ),
                    ));
                }
                probe.device = self.backend.attachments()?.into_iter().find_map(|item| {
                    item.mounts
                        .iter()
                        .any(|path| same_path(path, &mount))
                        .then_some(item.device)
                        .flatten()
                });
            }
            status => return Err(mount_error(agent_id, status, probe.detail)),
        }
        self.record_mount_identity(agent_id, state, &mount, probe.device)
    }

    /// Detach the agent's own volume, or prove that no part of it is attached.
    pub(super) fn detach_for_removal(
        &self,
        agent_id: &str,
        state: &Value,
        purpose: &str,
    ) -> Result<Value, ImageWorkspaceError> {
        let attachments = self.backend.attachments()?;
        let probe = self.probe_mount(agent_id, state, &attachments)?;
        if probe.status == "mounted" {
            return self.backend.unmount_workspace(&probe.mount, true);
        }
        if probe.status == "detached"
            || (probe.status == "image-missing" && !probe.mounted && probe.elsewhere.is_empty())
        {
            return Ok(json!({"method":"already-unmounted","terminatedPids":0}));
        }
        Err(mount_error(
            agent_id,
            probe.status,
            format!("{}; {purpose} was refused", probe.detail),
        ))
    }

    /// Report every agent mount point and every volume of this store attached
    /// outside its place. The check changes nothing.
    pub fn check_mounts(&self) -> Result<Value, ImageWorkspaceError> {
        let attachments = self.backend.attachments()?;
        let mut paths = Vec::new();
        let agents_root = self.store.join("agents");
        if agents_root.exists() {
            for entry in fs::read_dir(&agents_root)? {
                paths.push(entry?.path().join("agent.json"));
            }
        }
        paths.sort();
        let mut agents = Vec::new();
        for path in paths {
            let state = read_json(&path)?;
            if state
                .get("image")
                .and_then(Value::as_str)
                .is_none_or(str::is_empty)
            {
                continue;
            }
            let agent_id = path
                .parent()
                .and_then(Path::file_name)
                .map(|name| name.to_string_lossy().into_owned())
                .ok_or(ImageWorkspaceError::InvalidState)?;
            let probe = self.probe_mount(&agent_id, &state, &attachments)?;
            let repair = (probe.status == "detached"
                && state.get("state").and_then(Value::as_str) == Some("ready")
                && state.get("mounted").and_then(Value::as_bool) == Some(true))
            .then_some("attach");
            agents.push((probe, repair));
        }
        let accounted = agents
            .iter()
            .filter(|(probe, _)| {
                probe.mounted && matches!(probe.status, "mounted" | "attached-elsewhere")
            })
            .map(|(probe, _)| probe.mount.clone())
            .collect::<Vec<_>>();
        let owners = agents
            .iter()
            .map(|(probe, _)| {
                let identity = file_identity(probe.image.as_deref());
                (probe.agent_id.as_str(), probe.image.as_deref(), identity)
            })
            .collect::<Vec<_>>();
        let mut stray = Vec::new();
        for attachment in &attachments {
            if !studio_attachment(attachment)
                || base_staging_attachment(attachment, &self.store)
                || !self.owns_attachment(attachment, &owners)
            {
                continue;
            }
            if !attachment.mounts.is_empty()
                && attachment
                    .mounts
                    .iter()
                    .all(|path| accounted.iter().any(|known| same_path(path, known)))
            {
                continue;
            }
            let mut mounts = attachment.mounts.clone();
            mounts.sort();
            stray.push(json!({
                "image": attachment.image, "mounts": mounts,
                "agentId": attachment_agent(attachment),
                "imageExists": attachment.image.as_deref().is_some_and(Path::exists),
            }));
        }
        let mut under_store = attachments
            .iter()
            .flat_map(|attachment| attachment.mounts.iter())
            .filter(|path| inside(path, &self.store))
            .map(|path| path.to_string_lossy().into_owned())
            .collect::<BTreeSet<_>>();
        under_store.extend(
            agents
                .iter()
                .filter(|(probe, _)| probe.mounted)
                .map(|(probe, _)| probe.mount.to_string_lossy().into_owned()),
        );
        let problems = agents
            .iter()
            .any(|(probe, _)| IMAGE_MOUNT_PROBLEM_STATUSES.contains(&probe.status));
        Ok(json!({
            "schema": 1, "store": self.store, "checkedAt": now_seconds(),
            "ok": !problems && stray.is_empty(),
            "agents": agents.iter().map(|(probe, repair)| probe.view(*repair)).collect::<Vec<_>>(),
            "strayAttachments": stray, "mountsUnderStore": under_store,
        }))
    }

    /// Attach each ready agent whose recorded mount is detached and whose mount
    /// point is empty. Every other state stays for an operator.
    pub fn repair_mounts(&self) -> Result<Value, ImageWorkspaceError> {
        let before = self.check_mounts()?;
        let mut actions = Vec::new();
        for agent in before["agents"].as_array().into_iter().flatten() {
            if agent["repair"] != "attach" {
                continue;
            }
            let agent_id = agent["agentId"].as_str().unwrap_or_default();
            actions.push(match self.ensure_mounted(agent_id) {
                Ok(_) => json!({"agentId": agent_id, "action": "attached"}),
                Err(error) => {
                    json!({"agentId": agent_id, "action": "refused", "message": error.to_string()})
                }
            });
        }
        Ok(json!({"actions": actions, "report": self.check_mounts()?}))
    }
}

/// Startup check summary for `/api/diagnostics`: closed codes, no paths.
static MOUNT_DIAGNOSTICS: OnceLock<Mutex<Value>> = OnceLock::new();

fn mount_summary() -> &'static Mutex<Value> {
    MOUNT_DIAGNOSTICS.get_or_init(|| {
        Mutex::new(json!({"state":"pending","checkedAt":null,"problems":[],"strayAttachments":0}))
    })
}

pub fn mount_diagnostics() -> Value {
    mount_summary()
        .lock()
        .map(|value| value.clone())
        .unwrap_or_else(|poisoned| poisoned.into_inner().clone())
}

fn store_summary(summary: Value) {
    match mount_summary().lock() {
        Ok(mut value) => *value = summary,
        Err(poisoned) => *poisoned.into_inner() = summary,
    }
}

/// Summarize a check report for diagnostics and log each problem to stderr.
pub(super) fn record_startup_report(report: Result<Value, ImageWorkspaceError>) {
    let summary = match report {
        Ok(report) => {
            let agents = report["agents"].as_array().cloned().unwrap_or_default();
            let problems = agents
                .iter()
                .filter(|agent| {
                    agent["status"]
                        .as_str()
                        .is_some_and(|status| IMAGE_MOUNT_PROBLEM_STATUSES.contains(&status))
                })
                .collect::<Vec<_>>();
            for agent in &problems {
                eprintln!(
                    "Image workspace mount problem: {}: {}: {}",
                    agent["agentId"].as_str().unwrap_or_default(),
                    agent["status"].as_str().unwrap_or_default(),
                    agent["detail"].as_str().unwrap_or_default()
                );
            }
            let stray = report["strayAttachments"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for item in &stray {
                eprintln!(
                    "Image workspace volume attached outside its mount point: {} at {}",
                    item["image"].as_str().unwrap_or("an unknown image"),
                    item["mounts"]
                        .as_array()
                        .map(|mounts| mounts
                            .iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(", "))
                        .filter(|text| !text.is_empty())
                        .unwrap_or_else(|| "no mount point".to_owned())
                );
            }
            json!({
                "state": "checked", "checkedAt": report["checkedAt"],
                "problems": problems.iter().map(|agent| json!({
                    "agentId": agent["agentId"], "status": agent["status"],
                })).collect::<Vec<_>>(),
                "strayAttachments": stray.len(),
            })
        }
        Err(error) => {
            eprintln!("Image workspace mount check failed: {error}");
            json!({"state":"failed","checkedAt":now_seconds(),"problems":[],"strayAttachments":0})
        }
    };
    store_summary(summary);
}

/// Check image mounts once at backend start; repair nothing.
pub fn run_startup_mount_check() {
    #[cfg(target_os = "macos")]
    let report = Some(production().check_mounts());
    #[cfg(not(target_os = "macos"))]
    let report = None;
    match report {
        Some(report) => record_startup_report(report),
        None => store_summary(
            json!({"state":"unsupported","checkedAt":null,"problems":[],"strayAttachments":0}),
        ),
    }
}

/// `studio-runtime-server mounts check|repair [--store PATH]`.
pub fn mounts_command(args: &[String]) -> Result<(Value, bool), ImageWorkspaceError> {
    require_platform()?;
    #[cfg(target_os = "macos")]
    {
        let (action, store) = match args {
            [action] => (action.as_str(), None),
            [action, flag, store] if flag == "--store" => {
                (action.as_str(), Some(PathBuf::from(store)))
            }
            _ => {
                return Err(ImageWorkspaceError::Backend(
                    "usage: mounts check|repair [--store PATH]".into(),
                ));
            }
        };
        let lifecycle = match store {
            Some(store) => WorkspaceLifecycle::with_backend(store, MacAsifBackend),
            None => production(),
        };
        let value = match action {
            "check" => lifecycle.check_mounts()?,
            "repair" => lifecycle.repair_mounts()?,
            _ => {
                return Err(ImageWorkspaceError::Backend(
                    "usage: mounts check|repair [--store PATH]".into(),
                ));
            }
        };
        let ok = value
            .get("report")
            .unwrap_or(&value)
            .get("ok")
            .and_then(Value::as_bool)
            == Some(true);
        Ok((value, ok))
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = args;
        Err(ImageWorkspaceError::UnsupportedPlatform)
    }
}
