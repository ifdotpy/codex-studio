//! Reproducible host installation, separate from the user's Cargo bin directory.
use super::runtime::NativeLayrError;
use super::{LAYR_VERSION, NATIVE_LAYR_DIRECTORY, binary_path};
use std::{fs, path::Path, time::Duration};

pub fn ensure_binary(state_dir: &Path) -> Result<(), NativeLayrError> {
    let _lock = super::runtime::lock(state_dir, "install-binary")?;
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
    let source = source_directory()?;
    install_unlocked(state_dir, &source)
}

/// The explicit source option supports validation before the vendored release is available.
pub fn install(state_dir: &Path, source: &Path) -> Result<(), NativeLayrError> {
    let _lock = super::runtime::lock(state_dir, "install-binary")?;
    install_unlocked(state_dir, source)
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
        environment: &[],
        input: None,
        stdout_limit: 16 * 1024,
        stderr_limit: 64 * 1024,
        timeout: Duration::from_secs(1800),
    })
    .map_err(|error| NativeLayrError::Refused(format!("Host Layr build failed: {error:?}")))?;
    let temporary = directory.join("bin/layr.new");
    fs::copy(target.join("release/layr"), &temporary)?;
    fs::rename(temporary, binary_path(state_dir))?;
    super::git_guard::ensure(state_dir)?;
    Ok(())
}

fn source_directory() -> Result<std::path::PathBuf, NativeLayrError> {
    if let Ok(executable) = std::env::current_exe() {
        for ancestor in executable.ancestors() {
            for relative in ["workspaces/runtime/apps/layr", "vm/layr"] {
                let candidate = ancestor.join(relative);
                if candidate.join("Cargo.toml").is_file()
                    && candidate.join("manifest.json").is_file()
                {
                    return Ok(candidate);
                }
            }
        }
    }
    #[cfg(debug_assertions)]
    {
        Ok(Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/layr"))
    }
    #[cfg(not(debug_assertions))]
    Err(NativeLayrError::Refused(
        "The installed package has no vendored Layr source".into(),
    ))
}
