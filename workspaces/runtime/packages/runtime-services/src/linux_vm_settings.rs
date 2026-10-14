//! Host-side Linux VM resource settings, backed by the Python-compatible VM files.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::linux_vm_guest::{GuestError, GuestRpc};

const GIB: u64 = 1024 * 1024 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct VmSettings {
    pub cpus: u64,
    pub memory_bytes: u64,
    pub system_disk_bytes: u64,
    pub data_disk_bytes: u64,
}

#[derive(Debug, Error)]
pub enum SettingsError {
    #[error("Invalid VM settings: {0}")]
    Invalid(&'static str),
    #[error("Stop the Linux VM before changing its resource limits.")]
    Running,
    #[error("VM disks cannot shrink. Keep the existing disk limit.")]
    DiskShrink,
    #[error("The VM control service is unavailable.")]
    Control(#[source] GuestError),
    #[error("Linux VM settings could not be read or written.")]
    Io(#[source] std::io::Error),
    #[error("Linux VM settings are invalid.")]
    Json(#[source] serde_json::Error),
}

impl VmSettings {
    pub fn defaults() -> Self {
        Self {
            cpus: host_logical_cpu_count().min(4),
            memory_bytes: 4 * GIB,
            system_disk_bytes: 16 * GIB,
            data_disk_bytes: 128 * GIB,
        }
    }

    pub fn validate(self) -> Result<Self, SettingsError> {
        let cpus = host_logical_cpu_count();
        if self.cpus == 0 || self.cpus > cpus {
            return Err(SettingsError::Invalid(
                "CPU count must be between 1 and the host CPU count",
            ));
        }
        if !(GIB..=64 * GIB).contains(&self.memory_bytes) {
            return Err(SettingsError::Invalid(
                "Memory must be between 1 and 64 GiB",
            ));
        }
        if !(8 * GIB..=128 * GIB).contains(&self.system_disk_bytes) {
            return Err(SettingsError::Invalid(
                "System disk must be between 8 and 128 GiB",
            ));
        }
        if !(8 * GIB..=1024 * GIB).contains(&self.data_disk_bytes) {
            return Err(SettingsError::Invalid(
                "Data disk must be between 8 and 1024 GiB",
            ));
        }
        Ok(self)
    }
}

fn host_logical_cpu_count() -> u64 {
    let mut system = sysinfo::System::new();
    system.refresh_cpu_all();
    u64::try_from(system.cpus().len())
        .ok()
        .filter(|count| *count > 0)
        .unwrap_or_else(|| std::thread::available_parallelism().map_or(1, usize::from) as u64)
}

pub fn get_settings(vm_dir: &Path) -> Result<VmSettings, SettingsError> {
    for name in ["config.json", "settings.json"] {
        let path = vm_dir.join(name);
        match fs::read(&path) {
            Ok(bytes) => {
                return serde_json::from_slice::<VmSettings>(&bytes)
                    .map_err(SettingsError::Json)?
                    .validate();
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(SettingsError::Io(error)),
        }
    }
    VmSettings::defaults().validate()
}

pub fn status(vm_dir: &Path, control: &dyn GuestRpc) -> Result<Value, SettingsError> {
    let socket_present = vm_dir.join("control.sock").exists() || vm_dir.join("host.lock").exists();
    let mut value = match control.call("host.status", &json!({}), None, CONTROL_TIMEOUT) {
        Ok(value) => value,
        Err(_) if !socket_present => json!({"state":"stopped"}),
        Err(error) => return Err(SettingsError::Control(error)),
    };
    let state = value
        .get("state")
        .and_then(Value::as_str)
        .unwrap_or("stopped");
    value["state"] = Value::String(state.to_owned());
    value["allocatedDiskBytes"] = Value::from(allocated_disk_bytes(vm_dir));
    Ok(value)
}

pub fn set_settings(
    vm_dir: &Path,
    settings: VmSettings,
    control: &dyn GuestRpc,
) -> Result<VmSettings, SettingsError> {
    let settings = settings.validate()?;
    let current_status = status(vm_dir, control)?;
    if current_status["state"].as_str() != Some("stopped") {
        return Err(SettingsError::Running);
    }
    fs::create_dir_all(vm_dir).map_err(SettingsError::Io)?;
    let config = vm_dir.join("config.json");
    if config.exists() {
        let previous: VmSettings =
            serde_json::from_slice(&fs::read(&config).map_err(SettingsError::Io)?)
                .map_err(SettingsError::Json)?;
        if settings.system_disk_bytes < previous.system_disk_bytes
            || settings.data_disk_bytes < previous.data_disk_bytes
        {
            return Err(SettingsError::DiskShrink);
        }
        resize_disk(&vm_dir.join("system.raw"), settings.system_disk_bytes)?;
        resize_disk(&vm_dir.join("data.raw"), settings.data_disk_bytes)?;
        atomic_json(&config, &settings)?;
    } else {
        atomic_json(&vm_dir.join("settings.json"), &settings)?;
    }
    Ok(settings)
}

fn resize_disk(path: &Path, size: u64) -> Result<(), SettingsError> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .map_err(SettingsError::Io)?;
    let existing = file.metadata().map_err(SettingsError::Io)?.len();
    if size < existing {
        return Err(SettingsError::DiskShrink);
    }
    if size > existing {
        file.set_len(size).map_err(SettingsError::Io)?;
    }
    Ok(())
}

fn atomic_json<T: Serialize>(path: &Path, value: &T) -> Result<(), SettingsError> {
    let mut temporary: PathBuf = path.to_owned();
    temporary.set_extension(format!("tmp-{}", std::process::id()));
    let bytes = serde_json::to_vec(value).map_err(SettingsError::Json)?;
    fs::write(&temporary, bytes).map_err(SettingsError::Io)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&temporary, fs::Permissions::from_mode(0o600))
            .map_err(SettingsError::Io)?;
    }
    fs::rename(temporary, path).map_err(SettingsError::Io)
}

fn allocated_disk_bytes(vm_dir: &Path) -> u64 {
    ["system.raw", "data.raw"]
        .into_iter()
        .filter_map(|name| fs::metadata(vm_dir.join(name)).ok())
        .map(|metadata| allocated_bytes(&metadata))
        .sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    struct StoppedControl;

    impl GuestRpc for StoppedControl {
        fn stream(
            &self,
            method: &str,
            _params: &Value,
            _request_id: Option<&str>,
            _timeout: Duration,
            _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
        ) -> Result<Value, GuestError> {
            assert_eq!(method, "host.status");
            Ok(json!({"state":"stopped"}))
        }
    }

    #[test]
    fn settings_defaults_round_trip_through_python_file_names() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let expected = VmSettings::defaults();
        assert_eq!(get_settings(directory.path()).unwrap(), expected);
        set_settings(directory.path(), expected, &StoppedControl).unwrap();
        assert_eq!(get_settings(directory.path()).unwrap(), expected);
        assert!(directory.path().join("settings.json").is_file());
    }

    #[test]
    fn settings_reads_python_fallback_and_prefers_config() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let fallback = VmSettings {
            cpus: 1,
            ..VmSettings::defaults()
        };
        atomic_json(&directory.path().join("settings.json"), &fallback).unwrap();
        assert_eq!(get_settings(directory.path()).unwrap(), fallback);
        let configured = VmSettings {
            cpus: fallback.cpus + 1,
            ..fallback
        };
        atomic_json(&directory.path().join("config.json"), &configured).unwrap();
        assert_eq!(get_settings(directory.path()).unwrap(), configured);
    }

    #[test]
    fn settings_validate_ranges_and_reject_shrinking_provisioned_disks() {
        assert!(
            VmSettings {
                cpus: 0,
                ..VmSettings::defaults()
            }
            .validate()
            .is_err()
        );
        let directory = tempfile::tempdir().expect("temporary state directory");
        let initial = VmSettings::defaults();
        atomic_json(&directory.path().join("config.json"), &initial).unwrap();
        let err = set_settings(
            directory.path(),
            VmSettings {
                system_disk_bytes: initial.system_disk_bytes - GIB,
                ..initial
            },
            &StoppedControl,
        )
        .unwrap_err();
        assert!(matches!(err, SettingsError::DiskShrink));
    }

    #[test]
    fn provisioned_disks_grow_without_shrinking_existing_files() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let current = VmSettings::defaults();
        atomic_json(&directory.path().join("config.json"), &current).unwrap();
        fs::write(directory.path().join("system.raw"), []).unwrap();
        fs::write(directory.path().join("data.raw"), []).unwrap();
        let desired = VmSettings {
            system_disk_bytes: current.system_disk_bytes + GIB,
            data_disk_bytes: current.data_disk_bytes + GIB,
            ..current
        };
        set_settings(directory.path(), desired, &StoppedControl).unwrap();
        assert_eq!(
            fs::metadata(directory.path().join("system.raw"))
                .unwrap()
                .len(),
            desired.system_disk_bytes
        );
        assert_eq!(
            fs::metadata(directory.path().join("data.raw"))
                .unwrap()
                .len(),
            desired.data_disk_bytes
        );
    }

    struct RunningControl;

    impl GuestRpc for RunningControl {
        fn stream(
            &self,
            _method: &str,
            _params: &Value,
            _request_id: Option<&str>,
            _timeout: Duration,
            _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
        ) -> Result<Value, GuestError> {
            Ok(json!({"state":"running"}))
        }
    }

    #[test]
    fn running_vm_settings_refusal_has_no_write() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let settings = VmSettings::defaults();
        atomic_json(&directory.path().join("settings.json"), &settings).unwrap();
        let original = fs::read(directory.path().join("settings.json")).unwrap();
        assert!(matches!(
            set_settings(directory.path(), settings, &RunningControl),
            Err(SettingsError::Running)
        ));
        assert_eq!(
            fs::read(directory.path().join("settings.json")).unwrap(),
            original
        );
    }
}

#[cfg(unix)]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
}
