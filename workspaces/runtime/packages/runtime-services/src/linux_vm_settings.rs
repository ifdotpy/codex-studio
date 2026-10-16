//! Host-side Linux VM resource settings, backed by the Python-compatible VM files.

use std::fs;
use std::path::{Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::{Command, Stdio};
#[cfg(target_os = "macos")]
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use thiserror::Error;

use crate::linux_vm_guest::{GuestError, GuestRpc};

const GIB: u64 = 1024 * 1024 * 1024;
const CONTROL_TIMEOUT: Duration = Duration::from_secs(5);
#[cfg(target_os = "macos")]
const MACOS_MEMORY_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

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
    #[error("{0}")]
    Invalid(String),
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
        #[cfg(target_os = "macos")]
        let macos_memory_bytes = Some(read_macos_host_memory_bytes()?);
        #[cfg(not(target_os = "macos"))]
        let macos_memory_bytes = None;
        self.validate_for_host(host_logical_cpu_count(), macos_memory_bytes)
    }

    /// Validate with host limits; `Some` models Darwin's `hw.memsize` check.
    /// This also keeps the sysctl boundary replaceable in cross-platform corpus tests.
    pub fn validate_for_host(
        self,
        host_cpus: u64,
        macos_memory_bytes: Option<u64>,
    ) -> Result<Self, SettingsError> {
        if self.cpus == 0 || self.cpus > host_cpus {
            return Err(SettingsError::Invalid(
                "VM CPU count exceeds the available host CPUs.".to_owned(),
            ));
        }
        if !(GIB..=64 * GIB).contains(&self.memory_bytes) {
            return Err(SettingsError::Invalid(
                "VM memory must be between 1 and 64 GiB.".to_owned(),
            ));
        }
        if let Some(host_memory) = macos_memory_bytes {
            let limit = macos_memory_limit(host_memory);
            if self.memory_bytes > limit {
                return Err(SettingsError::Invalid(format!(
                    "VM memory must leave macOS enough memory: at most {:.1} GiB on this Mac.",
                    limit as f64 / GIB as f64
                )));
            }
        }
        if !(8 * GIB..=128 * GIB).contains(&self.system_disk_bytes) {
            return Err(SettingsError::Invalid(
                "VM system disk must be between 8 and 128 GiB.".to_owned(),
            ));
        }
        if !(8 * GIB..=1024 * GIB).contains(&self.data_disk_bytes) {
            return Err(SettingsError::Invalid(
                "VM data disk must be between 8 and 1024 GiB.".to_owned(),
            ));
        }
        Ok(self)
    }
}

/// Python's codex_linux_vm.memory_limit formula, in bytes.
pub const fn macos_memory_limit(host_memory_bytes: u64) -> u64 {
    let three_quarters = host_memory_bytes.saturating_mul(3) / 4;
    let half = host_memory_bytes / 2;
    let leave_eight_gib = host_memory_bytes.saturating_sub(8 * GIB);
    let retained_host_memory = if half > leave_eight_gib {
        half
    } else {
        leave_eight_gib
    };
    if three_quarters < retained_host_memory {
        three_quarters
    } else {
        retained_host_memory
    }
}

#[cfg(target_os = "macos")]
fn read_macos_host_memory_bytes() -> Result<u64, SettingsError> {
    let mut child = Command::new("sysctl")
        .args(["-n", "hw.memsize"])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(SettingsError::Io)?;
    let deadline = std::time::Instant::now() + MACOS_MEMORY_COMMAND_TIMEOUT;
    loop {
        match child.try_wait().map_err(SettingsError::Io)? {
            Some(status) if status.success() => break,
            Some(_) => {
                return Err(SettingsError::Invalid(
                    "Could not read macOS host memory.".to_owned(),
                ));
            }
            None if std::time::Instant::now() < deadline => {
                thread::sleep(Duration::from_millis(10));
            }
            None => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(SettingsError::Invalid(
                    "Reading macOS host memory exceeded its timeout.".to_owned(),
                ));
            }
        }
    }
    let output = child.wait_with_output().map_err(SettingsError::Io)?;
    String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<u64>()
        .map_err(|_| SettingsError::Invalid("Could not read macOS host memory.".to_owned()))
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
    set_validated_settings(vm_dir, settings, control)
}

/// The `set_settings` path with an injected Darwin host-memory result.
/// Callers normally use `set_settings`; this entry makes the sysctl boundary testable.
pub fn set_settings_with_macos_memory(
    vm_dir: &Path,
    settings: VmSettings,
    control: &dyn GuestRpc,
    host_memory_bytes: u64,
) -> Result<VmSettings, SettingsError> {
    let settings = settings.validate_for_host(host_logical_cpu_count(), Some(host_memory_bytes))?;
    set_validated_settings(vm_dir, settings, control)
}

fn set_validated_settings(
    vm_dir: &Path,
    settings: VmSettings,
    control: &dyn GuestRpc,
) -> Result<VmSettings, SettingsError> {
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

#[cfg(unix)]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    use std::os::unix::fs::MetadataExt;
    metadata.blocks().saturating_mul(512)
}

#[cfg(not(unix))]
fn allocated_bytes(metadata: &fs::Metadata) -> u64 {
    metadata.len()
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
        assert_eq!(
            get_settings(directory.path()).expect("fixture operation succeeds"),
            expected
        );
        set_settings(directory.path(), expected, &StoppedControl)
            .expect("fixture operation succeeds");
        assert_eq!(
            get_settings(directory.path()).expect("fixture operation succeeds"),
            expected
        );
        assert!(directory.path().join("settings.json").is_file());
    }

    #[test]
    fn settings_reads_python_fallback_and_prefers_config() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let fallback = VmSettings {
            cpus: 1,
            ..VmSettings::defaults()
        };
        atomic_json(&directory.path().join("settings.json"), &fallback)
            .expect("fixture operation succeeds");
        assert_eq!(
            get_settings(directory.path()).expect("fixture operation succeeds"),
            fallback
        );
        let configured = VmSettings {
            cpus: fallback.cpus + 1,
            ..fallback
        };
        atomic_json(&directory.path().join("config.json"), &configured)
            .expect("fixture operation succeeds");
        assert_eq!(
            get_settings(directory.path()).expect("fixture operation succeeds"),
            configured
        );
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
        atomic_json(&directory.path().join("config.json"), &initial)
            .expect("fixture operation succeeds");
        let result = set_settings(
            directory.path(),
            VmSettings {
                system_disk_bytes: initial.system_disk_bytes - GIB,
                ..initial
            },
            &StoppedControl,
        );
        assert!(matches!(result, Err(SettingsError::DiskShrink)));
    }

    #[test]
    fn provisioned_disks_grow_without_shrinking_existing_files() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let current = VmSettings::defaults();
        atomic_json(&directory.path().join("config.json"), &current)
            .expect("fixture operation succeeds");
        fs::write(directory.path().join("system.raw"), []).expect("fixture operation succeeds");
        fs::write(directory.path().join("data.raw"), []).expect("fixture operation succeeds");
        let desired = VmSettings {
            system_disk_bytes: current.system_disk_bytes + GIB,
            data_disk_bytes: current.data_disk_bytes + GIB,
            ..current
        };
        set_settings(directory.path(), desired, &StoppedControl)
            .expect("fixture operation succeeds");
        assert_eq!(
            fs::metadata(directory.path().join("system.raw"))
                .expect("fixture operation succeeds")
                .len(),
            desired.system_disk_bytes
        );
        assert_eq!(
            fs::metadata(directory.path().join("data.raw"))
                .expect("fixture operation succeeds")
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
        atomic_json(&directory.path().join("settings.json"), &settings)
            .expect("fixture operation succeeds");
        let original =
            fs::read(directory.path().join("settings.json")).expect("fixture operation succeeds");
        assert!(matches!(
            set_settings(directory.path(), settings, &RunningControl),
            Err(SettingsError::Running)
        ));
        assert_eq!(
            fs::read(directory.path().join("settings.json")).expect("fixture operation succeeds"),
            original
        );
    }
}
