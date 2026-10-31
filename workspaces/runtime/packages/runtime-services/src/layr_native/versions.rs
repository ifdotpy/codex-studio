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
    #[serde(default)]
    upgrade: Option<Upgrade>,
}

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct Upgrade {
    from_format: u64,
    to_format: u64,
    metadata_only: bool,
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
    let mut bundled = inspect(&super::git_guard::executable_path(state), root)?;
    if !bundled.compatible
        && settings::owns_store(state, root)
        && let Some(upgrade) = &bundled.upgrade
        && upgrade.metadata_only
        && upgrade.from_format < upgrade.to_format
        && upgrade.to_format == bundled.binary.format
        && (bundled.state == "legacy"
            || bundled
                .store
                .as_ref()
                .is_some_and(|version| version.format == upgrade.from_format))
    {
        super::runtime::operation_in_store(
            state,
            root,
            &format!("store-upgrade:{}", upgrade.to_format),
            &json!({"kind":"store-upgrade","root":root,"fromFormat":upgrade.from_format,"toFormat":upgrade.to_format}),
            || {
                super::runtime::run_in_store(state, root, state, &["store-version", "--upgrade"])?;
                Ok(json!({"storeRoot":root,"format":upgrade.to_format}))
            },
        )?;
        bundled = inspect(&super::git_guard::executable_path(state), root)?;
    }
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
    if settings::owns_store(state, root) {
        return Ok(None);
    }
    let developer =
        PathBuf::from(std::env::var_os("HOME").unwrap_or_default()).join(".local/bin/layr");
    verify_developer(&bundled.binary, root, &developer)
}

fn verify_developer(bundled: &Identity, root: &Path, developer: &Path) -> Result<Option<String>> {
    // Unguarded releases open the root before rejecting store-version. Probe only scratch.
    let scratch = tempfile::tempdir()?;
    let inspected = inspect(developer, scratch.path()).map_err(|error| NativeLayrError::Refused(format!("Store {} requires a guarded developer binary. Studio: {} (format {}, build {}). Developer {}: {error}. Upgrade the developer binary; see docs/native-layr.md.", root.display(), bundled.version, bundled.format, bundled.build, developer.display())))?;
    compare(bundled, &inspected.binary, root, developer)
}

fn compare(
    bundled: &Identity,
    developer: &Identity,
    root: &Path,
    binary: &Path,
) -> Result<Option<String>> {
    // The store format is the compatibility contract; any release of the same format may share it.
    if bundled.format != developer.format {
        return Err(NativeLayrError::Refused(format!(
            "Store {} requires the same Layr store format. Studio: {} (format {}, build {}). Developer {}: {} (format {}, build {}). Use a developer binary with the bundled store format; see docs/native-layr.md.",
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
    Ok((bundled.version != developer.version || bundled.build != developer.build).then(|| {
        format!(
            "Store {} uses Layr format {} with different releases. Studio: {} ({}). Developer {}: {} ({}).",
            root.display(),
            bundled.format,
            bundled.version,
            bundled.build,
            binary.display(),
            developer.version,
            developer.build
        )
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn native_layr_an_unguarded_developer_probe_never_receives_the_selected_root() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let fixture = tempfile::tempdir().expect("fixture");
        let root = fixture.path().join("developer-store");
        fs::create_dir(&root).expect("store");
        let binary = fixture.path().join("legacy-layr");
        fs::write(&binary, "#!/bin/sh\nprintf changed > \"$2/touched\"\nprintf 'unknown command\\n' >&2\nexit 128\n").expect("legacy binary");
        fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).expect("executable");
        let bundled = Identity {
            format: 1,
            version: LAYR_VERSION.into(),
            build: "bundled-build".into(),
        };
        let error = verify_developer(&bundled, &root, &binary)
            .expect_err("unsupported probe")
            .to_string();
        assert!(error.contains("bundled-build") && error.contains(root.to_str().expect("root")));
        assert!(fs::read_dir(root).expect("store").next().is_none());
    }

    #[cfg(unix)]
    #[test]
    fn native_layr_owned_store_adopts_metadata_once_and_selected_store_remains_untouched() {
        use std::{fs, os::unix::fs::PermissionsExt};
        let fixture = tempfile::tempdir().expect("fixture");
        let state = fixture.path().join("state");
        let root = store_path(&state);
        fs::create_dir_all(&root).expect("legacy store");
        fs::write(root.join("legacy-data"), "preserved").expect("legacy data");
        let script = r#"#!/bin/sh
case "$*" in
  *--json)
    if [ -f "$2/upgraded" ]; then
      printf '%s\n' '{"binary":{"format":1,"version":"0.5.1","build":"fixture"},"store":{"format":1,"version":"0.5.1","build":"fixture"},"compatible":true,"state":"compatible"}'
    else
      printf '%s\n' '{"binary":{"format":1,"version":"0.5.1","build":"fixture"},"store":null,"compatible":false,"state":"legacy","upgrade":{"fromFormat":0,"toFormat":1,"metadataOnly":true}}'
    fi ;;
  *--upgrade)
    printf 'upgrade\n' >> "$LAYR_ROOT/calls"
    printf 'complete\n' > "$LAYR_ROOT/upgraded" ;;
esac
"#;
        fs::create_dir_all(binary_path(&state).parent().expect("bin")).expect("bin");
        for binary in [
            binary_path(&state),
            super::super::git_guard::executable_path(&state),
        ] {
            fs::write(&binary, script).expect("binary fixture");
            fs::set_permissions(binary, fs::Permissions::from_mode(0o755)).expect("executable");
        }
        verify(&state, &root).expect("metadata adoption");
        verify(&state, &root).expect("repeat is a no-op");
        assert_eq!(
            fs::read_to_string(root.join("calls")).expect("calls"),
            "upgrade\n"
        );
        assert_eq!(
            fs::read_to_string(root.join("legacy-data")).expect("data"),
            "preserved"
        );
        let developer = fixture.path().join("developer-store");
        fs::create_dir(&developer).expect("developer store");
        let error = verify(&state, &developer)
            .expect_err("operator store requires explicit upgrade")
            .to_string();
        assert!(error.contains("upgrade-store"));
        assert!(!developer.join("upgraded").exists());
        assert!(
            fs::read_dir(developer)
                .expect("developer store")
                .next()
                .is_none()
        );
    }
    #[test]
    fn native_layr_format_mismatch_refuses_but_version_difference_is_visible() {
        let bundled = Identity {
            format: 1,
            version: "0.5.1".into(),
            build: "upstream".into(),
        };
        let mut developer = Identity {
            format: 2,
            version: "0.6.0".into(),
            build: "new".into(),
        };
        let compare_now = |developer: &Identity| {
            compare(
                &bundled,
                developer,
                Path::new("/store"),
                Path::new("/developer/layr"),
            )
        };
        let error = compare_now(&developer)
            .expect_err("different store format")
            .to_string();
        assert!(
            error.contains("format 2") && error.contains("format 1") && error.contains("/store")
        );
        developer.format = 1;
        developer.version = "0.5.2".into();
        let note = compare_now(&developer)
            .expect("same store format")
            .expect("visible difference");
        assert!(note.contains("0.5.2") && note.contains("0.5.1"));
        developer.version = bundled.version.clone();
        assert!(
            compare_now(&developer)
                .expect("compatible")
                .expect("warning")
                .contains("new")
        );
    }
}
