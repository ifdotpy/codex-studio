//! Inspect the Layr-owned store format before Studio prepares a native line.
use super::*;
use std::time::Duration;

type Result<T> = std::result::Result<T, NativeLayrError>;

#[derive(serde::Deserialize)]
struct Identity {
    format: u64,
    version: String,
    build: String,
}
#[derive(serde::Deserialize)]
struct Inspection {
    binary: Identity,
    store: Option<Identity>,
    compatible: bool,
    state: String,
}

fn inspect(binary: &Path, root: &Path) -> Result<Inspection> {
    let root = root
        .to_str()
        .ok_or_else(|| NativeLayrError::Refused("Non-UTF-8 store root".into()))?;
    let result = crate::bounded_child::run_without_git_environment(crate::bounded_child::Request {
        program: binary.as_os_str(), git_directory: None,
        args: &["--root", root, "store-version", "--json"],
        environment: &[("LAYR_DIRECT", "1")], input: None,
        stdout_limit: 16 * 1024, stderr_limit: 4096, timeout: Duration::from_secs(10),
    }).map_err(|e| NativeLayrError::Refused(format!("Cannot inspect Layr at {} for store {root}: {e:?}. Upgrade this binary to the Studio Layr release; see docs/native-layr.md.", binary.display())))?;
    serde_json::from_slice(&result.stdout.bytes).map_err(|e| NativeLayrError::Refused(format!("Layr at {} has no valid store-version response for {root}: {e}. Upgrade this binary; see docs/native-layr.md.", binary.display())))
}

pub(super) fn verify(state: &Path, root: &Path) -> Result<Option<String>> {
    let bundled = inspect(&super::git_guard::executable_path(state), root)?;
    if !bundled.compatible {
        let written = bundled
            .store
            .as_ref()
            .map(|store| {
                format!(
                    "{} (format {}, build {})",
                    store.version, store.format, store.build
                )
            })
            .unwrap_or_else(|| bundled.state.clone());
        return Err(NativeLayrError::Refused(format!(
            "Store {} was written by {written}. Studio uses {} (format {}, build {}). Use native-layr upgrade-store for a legacy store; see docs/native-layr.md for the switch procedure.",
            root.display(),
            bundled.binary.version,
            bundled.binary.format,
            bundled.binary.build
        )));
    }
    if settings::same_root(root, &store_path(state)) {
        return Ok(None);
    }
    let developer =
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/bin/layr");
    compare(
        &bundled.binary,
        &inspect(&developer, root)?.binary,
        root,
        &developer,
    )
}

fn compare(
    bundled: &Identity,
    developer: &Identity,
    root: &Path,
    binary: &Path,
) -> Result<Option<String>> {
    if bundled.version != developer.version || bundled.format != developer.format {
        return Err(NativeLayrError::Refused(format!(
            "Store {} requires matching Layr versions. Studio: {} (format {}, build {}). Developer {}: {} (format {}, build {}). Upgrade the developer binary to the bundled release; see docs/native-layr.md.",
            root.display(),
            bundled.version,
            bundled.format,
            bundled.build,
            binary.display(),
            developer.version,
            developer.format,
            developer.build
        )));
    }
    Ok((bundled.build != developer.build).then(|| {
        format!(
            "Store {} uses Layr {} with different builds. Studio: {}. Developer {}: {}.",
            root.display(),
            bundled.version,
            bundled.build,
            binary.display(),
            developer.build
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn package_mismatch_refuses_but_build_difference_is_visible() {
        let bundled = Identity {
            format: 1,
            version: "0.4.0".into(),
            build: "upstream".into(),
        };
        let mut developer = Identity {
            format: 1,
            version: "0.2.3".into(),
            build: "old".into(),
        };
        let error = compare(
            &bundled,
            &developer,
            Path::new("/store"),
            Path::new("/developer/layr"),
        )
        .expect_err("different package")
        .to_string();
        assert!(error.contains("0.2.3") && error.contains("0.4.0") && error.contains("/store"));
        developer.version = bundled.version.clone();
        assert!(
            compare(
                &bundled,
                &developer,
                Path::new("/store"),
                Path::new("/developer/layr")
            )
            .expect("compatible")
            .expect("warning")
            .contains("old")
        );
    }
}
