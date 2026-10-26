//! Reproducible host installation, separate from the user's Cargo bin directory.
use super::runtime::NativeLayrError;
use super::{LAYR_VERSION, NATIVE_LAYR_DIRECTORY, binary_path};
use std::{
    fs::{self, File, OpenOptions, TryLockError},
    io::Write,
    path::{Path, PathBuf},
    time::{Duration, Instant},
};
use studio_runtime_store::path_identity::CanonicalPath;

/// Manifest beside the packaged host executable. Its version uses LAYR_VERSION.
pub(super) const SHIPPED_MANIFEST_NAME: &str = "layr-manifest.json";
/// Maximum wait for another native binary installation, in seconds.
const INSTALL_LOCK_TIMEOUT: Duration = Duration::from_secs(60);
/// Maximum time for a host executable to report its version, in seconds.
const VERSION_CHECK_TIMEOUT: Duration = Duration::from_secs(10);

pub fn ensure_binary(state_dir: &Path) -> Result<(), NativeLayrError> {
    let executable = CanonicalPath::existing(std::env::current_exe()?)?.into_path_buf();
    let shipped = shipped_binary_near(&executable);
    ensure_binary_with(state_dir, shipped.as_deref(), INSTALL_LOCK_TIMEOUT, || {
        install_unlocked(state_dir, &source_directory()?)
    })
}

fn ensure_binary_with(
    state_dir: &Path,
    shipped: Option<&Path>,
    lock_timeout: Duration,
    developer_build: impl FnOnce() -> Result<(), NativeLayrError>,
) -> Result<(), NativeLayrError> {
    let _lock = install_lock(state_dir, lock_timeout)?;
    if let Some(shipped) = shipped {
        return install_shipped(state_dir, shipped);
    }
    let binary = binary_path(state_dir);
    if binary.is_file() {
        let version = super::run(state_dir, state_dir, &["--version"])?;
        if version != format!("layr version {LAYR_VERSION}") {
            return Err(NativeLayrError::Refused(format!(
                "Expected Layr {LAYR_VERSION}, found {version}"
            )));
        }
        super::git_guard::ensure(state_dir)?;
        return Ok(());
    }
    developer_build()
}

/// Explicit developer installation. Packaged callers must use ensure_binary.
pub fn install(state_dir: &Path, source: &Path) -> Result<(), NativeLayrError> {
    if shipped_binary_near(&CanonicalPath::existing(std::env::current_exe()?)?.into_path_buf())
        .is_some()
    {
        return Err(NativeLayrError::Refused(
            "A packaged application must use its bundled Layr executable".into(),
        ));
    }
    let _lock = install_lock(state_dir, INSTALL_LOCK_TIMEOUT)?;
    install_unlocked(state_dir, source)
}

#[allow(
    clippy::disallowed_methods,
    reason = "The installer runs on the workspace blocking pool or the synchronous operator CLI"
)]
fn install_lock(state_dir: &Path, timeout: Duration) -> Result<File, NativeLayrError> {
    let directory = state_dir.join(NATIVE_LAYR_DIRECTORY).join("locks");
    fs::create_dir_all(&directory)?;
    // Use the same lock identity as native runtime operations.
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(directory.join(studio_runtime_store::sha256_hex(b"install-binary")))?;
    let started = Instant::now();
    loop {
        match file.try_lock() {
            Ok(()) => return Ok(file),
            Err(TryLockError::Error(error)) => return Err(error.into()),
            Err(TryLockError::WouldBlock) => {
                let remaining = timeout.saturating_sub(started.elapsed());
                if remaining.is_zero() {
                    return Err(NativeLayrError::Refused(format!(
                        "Timed out after {} ms while waiting for the native Layr install-binary lock. Another installation is still active.",
                        timeout.as_millis()
                    )));
                }
                std::thread::sleep(remaining.min(Duration::from_millis(100)));
            }
        }
    }
}

#[derive(serde::Deserialize)]
struct ShippedManifest {
    version: String,
    sha256: String,
}

fn install_shipped(state_dir: &Path, shipped: &Path) -> Result<(), NativeLayrError> {
    let manifest_path = shipped.with_file_name(SHIPPED_MANIFEST_NAME);
    let manifest_bytes = fs::read(&manifest_path).map_err(|error| {
        NativeLayrError::Refused(format!(
            "Cannot read the packaged Layr manifest at {}: {error}. Reinstall the application.",
            manifest_path.display()
        ))
    })?;
    let manifest: ShippedManifest = serde_json::from_slice(&manifest_bytes).map_err(|error| {
        NativeLayrError::Refused(format!("Invalid packaged Layr manifest: {error}"))
    })?;
    if manifest.version != LAYR_VERSION {
        return Err(NativeLayrError::Refused(format!(
            "Expected Layr {LAYR_VERSION}, found package manifest version {}",
            manifest.version
        )));
    }
    let bytes = fs::read(shipped).map_err(|error| {
        NativeLayrError::Refused(format!(
            "Cannot read the packaged Layr executable at {}: {error}. Reinstall the application.",
            shipped.display()
        ))
    })?;
    if studio_runtime_store::sha256_hex(&bytes) != manifest.sha256 {
        return Err(NativeLayrError::Refused(
            "Packaged Layr hash mismatch. Reinstall the application.".into(),
        ));
    }
    let executable = super::git_guard::executable_path(state_dir);
    if fs::read(&executable)
        .is_ok_and(|installed| studio_runtime_store::sha256_hex(&installed) == manifest.sha256)
    {
        verify_version(&executable)?;
    } else {
        let bin = executable
            .parent()
            .ok_or_else(|| NativeLayrError::Refused("Missing managed binary folder".into()))?;
        fs::create_dir_all(bin)?;
        let mut temporary = tempfile::Builder::new().prefix(".layr-").tempfile_in(bin)?;
        temporary.write_all(&bytes)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            temporary
                .as_file()
                .set_permissions(fs::Permissions::from_mode(0o755))?;
        }
        temporary.as_file().sync_all()?;
        // Close the writer before execution, including on hosts that refuse an
        // executable with an open write descriptor. TempPath removes failed copies.
        let temporary = temporary.into_temp_path();
        verify_version(&temporary)?;
        fs::rename(&temporary, &executable)?;
        File::open(bin)?.sync_all()?;
    }
    super::git_guard::ensure(state_dir)
}

fn verify_version(binary: &Path) -> Result<(), NativeLayrError> {
    let output = crate::bounded_child::run(crate::bounded_child::Request {
        program: binary.as_os_str(),
        git_directory: None,
        args: &["--version"],
        environment: &[],
        input: None,
        stdout_limit: 1024,
        stderr_limit: 4096,
        timeout: VERSION_CHECK_TIMEOUT,
    })
    .map_err(|error| NativeLayrError::Refused(format!("Layr version check failed: {error:?}")))?;
    let version = String::from_utf8_lossy(&output.stdout.bytes);
    if version.trim() != format!("layr version {LAYR_VERSION}") {
        return Err(NativeLayrError::Refused(format!(
            "Expected Layr {LAYR_VERSION}, found {}",
            version.trim()
        )));
    }
    Ok(())
}

// Package identity survives missing bundle files. In particular, an application
// with a missing executable and manifest must never enter the Cargo fallback.
fn shipped_binary_near(executable: &Path) -> Option<PathBuf> {
    for ancestor in executable.parent()?.ancestors() {
        if ancestor.file_name().is_some_and(|name| name == "Contents")
            && ancestor
                .parent()
                .is_some_and(|app| app.extension().is_some_and(|ext| ext == "app"))
        {
            return Some(ancestor.join("Resources/workspace/bin/layr"));
        }
        if ancestor.join("studio-install.json").is_file() {
            return Some(ancestor.join("bin/layr"));
        }
        if ancestor.join("workspace/studio-install.json").is_file() {
            return Some(ancestor.join("workspace/bin/layr"));
        }
    }
    None
}

fn install_unlocked(state_dir: &Path, source: &Path) -> Result<(), NativeLayrError> {
    let manifest = source.join("Cargo.toml");
    let parsed: toml::Value = toml::from_str(&fs::read_to_string(&manifest)?)
        .map_err(|error| NativeLayrError::Refused(error.to_string()))?;
    if parsed
        .get("package")
        .and_then(|p| p.get("version"))
        .and_then(toml::Value::as_str)
        != Some(LAYR_VERSION)
    {
        return Err(NativeLayrError::Refused(format!(
            "The host source must contain Layr {LAYR_VERSION}"
        )));
    }
    let directory = state_dir.join(NATIVE_LAYR_DIRECTORY);
    fs::create_dir_all(directory.join("bin"))?;
    if source.join("manifest.json").exists() {
        let value: serde_json::Value =
            serde_json::from_slice(&fs::read(source.join("manifest.json"))?)
                .map_err(|error| NativeLayrError::Refused(error.to_string()))?;
        let files = value["files"]
            .as_object()
            .ok_or_else(|| NativeLayrError::Refused("Missing source manifest files".into()))?;
        for (path, hash) in files {
            if Path::new(path).is_absolute()
                || Path::new(path)
                    .components()
                    .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return Err(NativeLayrError::Refused(
                    "Invalid source manifest path".into(),
                ));
            }
            if Some(studio_runtime_store::sha256_hex(&fs::read(source.join(path))?).as_str())
                != hash.as_str()
            {
                return Err(NativeLayrError::Refused(format!(
                    "Layr source hash mismatch: {path}"
                )));
            }
        }
    }
    let target = directory.join("build");
    let args = [
        "build",
        "--locked",
        "--release",
        "-j",
        "4",
        "--manifest-path",
        manifest.to_str().unwrap_or_default(),
        "--target-dir",
        target.to_str().unwrap_or_default(),
    ];
    crate::bounded_child::run(crate::bounded_child::Request {
        program: std::ffi::OsStr::new("cargo"),
        git_directory: None,
        args: &args,
        environment: &[("CARGO_INCREMENTAL", "0")],
        input: None,
        stdout_limit: 16 * 1024,
        stderr_limit: 64 * 1024,
        timeout: Duration::from_secs(1800),
    })
    .map_err(|error| NativeLayrError::Refused(format!("Host Layr build failed: {error:?}")))?;
    let built = target.join("release/layr");
    verify_version(&built)?;
    crate::atomic_file::replace(
        &super::git_guard::executable_path(state_dir),
        &fs::read(built)?,
        Some(0o755),
        true,
    )?;
    super::git_guard::ensure(state_dir)?;
    Ok(())
}

fn source_directory() -> Result<std::path::PathBuf, NativeLayrError> {
    if let Ok(executable) = std::env::current_exe() {
        for ancestor in executable.ancestors() {
            let candidate = ancestor.join("workspaces/runtime/apps/layr");
            if candidate.join("Cargo.toml").is_file() && candidate.join("manifest.json").is_file() {
                return Ok(candidate);
            }
        }
    }
    #[cfg(debug_assertions)]
    {
        Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/layr"))
    }
    #[cfg(not(debug_assertions))]
    Err(NativeLayrError::Refused(
        "No developer Layr source was found. Reinstall the application to restore its bundled executable.".into(),
    ))
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn fixture(root: &Path, version: &str) -> std::path::PathBuf {
        let bin = root.join("package/workspace/bin");
        fs::create_dir_all(&bin).expect("fixture operation succeeds");
        let binary = bin.join("layr");
        fs::write(
            &binary,
            format!("#!/bin/sh\nprintf '%s\\n' 'layr version {version}'\n"),
        )
        .expect("fixture operation succeeds");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755))
            .expect("fixture operation succeeds");
        let manifest = serde_json::json!({
            "version": LAYR_VERSION,
            "sha256": studio_runtime_store::sha256_hex(&fs::read(&binary).expect("fixture operation succeeds")),
        });
        fs::write(
            bin.join(SHIPPED_MANIFEST_NAME),
            serde_json::to_vec(&manifest).expect("fixture operation succeeds"),
        )
        .expect("fixture operation succeeds");
        binary
    }

    fn no_cargo() -> Result<(), NativeLayrError> {
        Err(NativeLayrError::Refused(
            "Developer Cargo build requested".into(),
        ))
    }

    #[test]
    fn native_layr_shipped_binary_installs_and_retries_without_cargo() {
        let root = tempfile::tempdir().expect("temporary install fixture");
        let shipped = fixture(root.path(), LAYR_VERSION);
        let state = root.path().join("state");
        for _ in 0..2 {
            ensure_binary_with(&state, Some(&shipped), INSTALL_LOCK_TIMEOUT, no_cargo)
                .expect("The shipped binary must bypass the Cargo fallback");
        }
        assert_eq!(
            fs::read(super::super::git_guard::executable_path(&state))
                .expect("fixture operation succeeds"),
            fs::read(&shipped).expect("fixture operation succeeds")
        );
        assert_eq!(
            super::super::run(&state, &state, &["--version"]).expect("fixture operation succeeds"),
            format!("layr version {LAYR_VERSION}")
        );
        assert!(!state.join(NATIVE_LAYR_DIRECTORY).join("build").exists());
    }

    #[test]
    fn native_layr_modified_installed_binary_is_replaced_without_execution() {
        let root = tempfile::tempdir().expect("temporary install fixture");
        let shipped = fixture(root.path(), LAYR_VERSION);
        let state = root.path().join("state");
        ensure_binary_with(&state, Some(&shipped), INSTALL_LOCK_TIMEOUT, no_cargo)
            .expect("fixture operation succeeds");
        let installed = super::super::git_guard::executable_path(&state);
        let marker = root.path().join("modified-binary-executed");
        fs::write(
            &installed,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .expect("fixture operation succeeds");
        ensure_binary_with(&state, Some(&shipped), INSTALL_LOCK_TIMEOUT, no_cargo)
            .expect("fixture operation succeeds");
        assert_eq!(
            fs::read(installed).expect("fixture operation succeeds"),
            fs::read(shipped).expect("fixture operation succeeds")
        );
        assert!(!marker.exists());
    }

    #[test]
    fn native_layr_missing_bundle_files_refuse_even_with_a_cached_binary() {
        let root = tempfile::tempdir().expect("temporary install fixture");
        let shipped = fixture(root.path(), LAYR_VERSION);
        let state = root.path().join("state");
        ensure_binary_with(&state, Some(&shipped), INSTALL_LOCK_TIMEOUT, no_cargo)
            .expect("fixture operation succeeds");
        fs::remove_file(&shipped).expect("fixture operation succeeds");
        let error = ensure_binary_with(&state, Some(&shipped), INSTALL_LOCK_TIMEOUT, no_cargo)
            .expect_err("installation must refuse the fixture")
            .to_string();
        assert!(error.contains("packaged Layr executable"), "{error}");
        fs::remove_file(shipped.with_file_name(SHIPPED_MANIFEST_NAME))
            .expect("fixture operation succeeds");
        let error = ensure_binary_with(&state, Some(&shipped), INSTALL_LOCK_TIMEOUT, no_cargo)
            .expect_err("installation must refuse the fixture")
            .to_string();
        assert!(error.contains("packaged Layr manifest"), "{error}");
    }

    #[test]
    fn native_layr_package_detection_survives_missing_binary_and_manifest() {
        let root = tempfile::tempdir().expect("temporary install fixture");
        let contents = root.path().join("Studio.app/Contents");
        let expected = contents.join("Resources/workspace/bin/layr");
        for host in [
            contents.join("MacOS/Studio"),
            contents.join("Resources/workspace/bin/studio-runtime-server"),
        ] {
            let shipped = shipped_binary_near(&host).expect("Packaged host");
            assert_eq!(shipped, expected);
            let error = ensure_binary_with(
                &root.path().join("state"),
                Some(&shipped),
                INSTALL_LOCK_TIMEOUT,
                no_cargo,
            )
            .expect_err("installation must refuse the fixture")
            .to_string();
            assert!(error.contains("packaged Layr manifest"), "{error}");
        }
        let resources = root.path().join("resources");
        fs::create_dir_all(resources.join("workspace")).expect("fixture operation succeeds");
        fs::write(resources.join("workspace/studio-install.json"), "{}")
            .expect("fixture operation succeeds");
        assert_eq!(
            shipped_binary_near(&resources.join("bin/studio-runtime-server")),
            Some(resources.join("workspace/bin/layr"))
        );
        assert_eq!(
            shipped_binary_near(&root.path().join("target/debug/server")),
            None
        );
        assert_eq!(
            shipped_binary_near(&root.path().join("workspace/target/debug/server")),
            None
        );
    }

    #[test]
    fn native_layr_shipped_hash_mismatch_is_refused_before_execution() {
        let root = tempfile::tempdir().expect("temporary install fixture");
        let shipped = fixture(root.path(), LAYR_VERSION);
        let marker = root.path().join("executed");
        fs::write(
            &shipped,
            format!("#!/bin/sh\ntouch '{}'\n", marker.display()),
        )
        .expect("fixture operation succeeds");
        let state = root.path().join("state");
        let error = ensure_binary_with(&state, Some(&shipped), INSTALL_LOCK_TIMEOUT, no_cargo)
            .expect_err("installation must refuse the fixture")
            .to_string();
        assert!(error.contains("hash mismatch"), "{error}");
        assert!(!marker.exists());
        assert!(!binary_path(&state).exists());
    }

    #[test]
    fn native_layr_shipped_version_mismatch_preserves_previous_executable() {
        let root = tempfile::tempdir().expect("temporary install fixture");
        let shipped = fixture(root.path(), "0.0.0");
        let state = root.path().join("state");
        let installed = super::super::git_guard::executable_path(&state);
        fs::create_dir_all(installed.parent().expect("managed binary parent"))
            .expect("fixture operation succeeds");
        fs::write(&installed, b"previous executable").expect("fixture operation succeeds");
        let error = ensure_binary_with(&state, Some(&shipped), INSTALL_LOCK_TIMEOUT, no_cargo)
            .expect_err("installation must refuse the fixture")
            .to_string();
        assert!(error.contains("Expected Layr"), "{error}");
        assert_eq!(
            fs::read(installed).expect("fixture operation succeeds"),
            b"previous executable"
        );
    }

    #[test]
    fn native_layr_install_lock_timeout_is_visible() {
        let root = tempfile::tempdir().expect("temporary install fixture");
        let state = root.path().join("state");
        let _held = super::super::runtime::lock(&state, "install-binary")
            .expect("fixture install lock is available");
        let error = ensure_binary_with(&state, None, Duration::from_millis(5), no_cargo)
            .expect_err("installation must refuse the fixture")
            .to_string();
        assert!(error.contains("Timed out"), "{error}");
        assert!(error.contains("install-binary"), "{error}");
        assert!(error.contains("5 ms"), "{error}");
    }
}
