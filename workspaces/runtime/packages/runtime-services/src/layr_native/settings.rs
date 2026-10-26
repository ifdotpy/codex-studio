//! Host store selection. Existing chats retain their recorded store.
use super::*;
use std::fs;

type Result<T> = std::result::Result<T, NativeLayrError>;

#[derive(Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct Settings {
    pub store_root: Option<PathBuf>,
}

fn path(state: &Path) -> PathBuf {
    state.join(NATIVE_LAYR_DIRECTORY).join("settings.json")
}

pub fn selected_store(state: &Path) -> Result<PathBuf> {
    let settings = match fs::read(path(state)) {
        Ok(bytes) => serde_json::from_slice::<Settings>(&bytes)
            .map_err(|e| NativeLayrError::Refused(format!("Invalid native Layr settings: {e}")))?,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Settings::default(),
        Err(e) => return Err(e.into()),
    };
    match settings.store_root {
        Some(root) if root.is_absolute() && root.is_dir() => {
            Ok(CanonicalPath::existing(root)?.into_path_buf())
        }
        Some(_) => Err(NativeLayrError::Refused(
            "The native Layr store root must be an existing absolute folder".into(),
        )),
        None => Ok(store_path(state)),
    }
}

/// Change only the destination for new imports and mappings. Existing roots stay intact.
pub fn configure(state: &Path, root: Option<&Path>) -> Result<Value> {
    let root = root
        .map(|root| {
            if !root.is_absolute() || !root.is_dir() {
                return Err(NativeLayrError::Refused(
                    "The native Layr store root must be an existing absolute folder".into(),
                ));
            }
            Ok(CanonicalPath::existing(root)?.into_path_buf())
        })
        .transpose()?;
    let settings = Settings { store_root: root };
    let folder = state.join(NATIVE_LAYR_DIRECTORY);
    fs::create_dir_all(&folder)?;
    crate::atomic_file::replace(
        &path(state),
        serde_json::to_string(&settings)
            .map_err(|e| NativeLayrError::Refused(e.to_string()))?
            .as_bytes(),
        Some(0o600),
        true,
    )?;
    Ok(json!({"storeRoot":selected_store(state)?}))
}

pub(super) fn same_root(left: &Path, right: &Path) -> bool {
    left.canonicalize().unwrap_or_else(|_| left.to_owned())
        == right.canonicalize().unwrap_or_else(|_| right.to_owned())
}

/// A link to an operator's store does not make that store Studio-owned.
pub(super) fn owns_store(state: &Path, root: &Path) -> bool {
    let expected = state
        .canonicalize()
        .unwrap_or_else(|_| state.to_owned())
        .join(NATIVE_LAYR_DIRECTORY)
        .join(STORE_DIRECTORY);
    CanonicalPath::missing_tail(root).is_some_and(|root| root.as_path() == expected)
        && !fs::symlink_metadata(store_path(state))
            .is_ok_and(|metadata| metadata.file_type().is_symlink())
}

/// Keep the original metadata location for old Studio imports.
pub(super) fn metadata(state: &Path, root: &Path) -> PathBuf {
    let base = state.join(NATIVE_LAYR_DIRECTORY);
    if same_root(root, &store_path(state)) {
        base
    } else {
        base.join("stores").join(studio_runtime_store::sha256_hex(
            root.to_string_lossy().as_bytes(),
        ))
    }
}

pub(super) fn root_from_cwd(cwd: &Path) -> Option<PathBuf> {
    cwd.ancestors().find_map(|line| {
        let lines = line.parent()?;
        let project = lines.parent()?;
        let projects = project.parent()?;
        if lines.file_name()? == "lines" && projects.file_name()? == "projects" {
            Some(projects.parent()?.to_owned())
        } else {
            None
        }
    })
}

pub fn record_store(state: &Path, record: &Value) -> Result<PathBuf> {
    if let Some(root) = record["layrStoreRoot"].as_str() {
        let root = PathBuf::from(root);
        if !root.is_absolute() {
            return Err(NativeLayrError::Refused(
                "Invalid native Layr store association".into(),
            ));
        }
        return Ok(root);
    }
    // Records from before store selection have an authoritative saved line path.
    if record["layrReady"] == true {
        let cwd = record[if record["role"] == "reviewer" {
            "layrReadCwd"
        } else {
            "cwd"
        }]
        .as_str()
        .ok_or_else(|| NativeLayrError::Refused("Missing native Layr cwd".into()))?;
        return root_from_cwd(Path::new(cwd))
            .ok_or_else(|| NativeLayrError::Refused("Invalid native Layr cwd association".into()));
    }
    selected_store(state)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_layr_owned_store_detection_resolves_aliases_before_first_import() {
        let fixture = tempfile::tempdir().expect("fixture");
        let root = store_path(fixture.path());
        assert!(!root.exists());
        assert!(owns_store(fixture.path(), &root));
    }

    #[cfg(unix)]
    #[test]
    fn native_layr_a_default_path_symlink_does_not_make_a_developer_store_owned() {
        let fixture = tempfile::tempdir().expect("fixture");
        let state = fixture.path().join("state");
        let developer = fixture.path().join("developer");
        fs::create_dir_all(state.join(NATIVE_LAYR_DIRECTORY)).expect("native folder");
        fs::create_dir(&developer).expect("developer store");
        std::os::unix::fs::symlink(&developer, store_path(&state)).expect("store link");
        assert!(!owns_store(&state, &developer));
        assert!(!owns_store(&state, &store_path(&state)));
    }

    #[test]
    fn native_layr_root_switch_keeps_existing_chats_on_their_original_store() {
        let state = tempfile::tempdir().expect("state");
        let old = store_path(state.path());
        let other = tempfile::tempdir().expect("other store");
        let record = json!({"layrStoreRoot": old, "layrReady":true});
        fs::create_dir_all(state.path().join(NATIVE_LAYR_DIRECTORY)).expect("settings folder");
        fs::write(
            state
                .path()
                .join(NATIVE_LAYR_DIRECTORY)
                .join("settings.json"),
            json!({"storeRoot":other.path()}).to_string(),
        )
        .expect("settings");
        assert!(same_root(
            &selected_store(state.path()).expect("selected store"),
            other.path()
        ));
        assert_eq!(
            record_store(state.path(), &record).expect("bound root"),
            old
        );
        assert_eq!(
            thread_environment_for_record(state.path(), &record).expect("environment")["LAYR_ROOT"],
            json!(old)
        );
        let legacy = json!({"layrReady":true,"cwd":old.join("projects/old/lines/main")});
        assert_eq!(
            record_store(state.path(), &legacy).expect("legacy root"),
            old
        );
        assert_ne!(
            metadata(state.path(), &old),
            metadata(state.path(), other.path())
        );
    }

    #[test]
    fn native_layr_invalid_settings_never_select_the_default_store() {
        let state = tempfile::tempdir().expect("state");
        fs::create_dir_all(state.path().join(NATIVE_LAYR_DIRECTORY)).expect("folder");
        fs::write(path(state.path()), "{broken").expect("settings");
        assert!(selected_store(state.path()).is_err());
        assert!(configure(state.path(), Some(Path::new("relative"))).is_err());
    }

    #[test]
    fn native_layr_selected_store_has_its_own_service_socket() {
        let state = tempfile::tempdir().expect("state");
        let other = tempfile::tempdir().expect("store");
        configure(state.path(), Some(other.path())).expect("configure");
        let env = super::super::transport_environment_in_store(
            state.path(),
            &selected_store(state.path()).expect("store"),
            true,
        );
        assert_eq!(
            env["LAYR_SOCKET"],
            json!(selected_socket_path(state.path()).expect("socket"))
        );
        assert_ne!(env["LAYR_SOCKET"], json!(socket_path(state.path())));
    }
}
