use std::fs;
#[cfg(unix)]
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use studio_runtime_services::provider_binaries::{
    BinaryIdentityError, BinaryResolutionError, DiscoveryInputs, NativePlatform,
    binary_file_identity, companion_identities, discovery_paths, resolve_npm_launcher,
};

#[test]
fn provider_binary_discovery_keeps_precedence_and_deduplicates_paths() {
    let paths = discovery_paths(DiscoveryInputs {
        pinned_path: None,
        explicit_path: Some(PathBuf::from("explicit")),
        path_lookup: Some(PathBuf::from("lookup")),
        path_entries: vec![PathBuf::from("path"), PathBuf::from("lookup")],
        chatgpt_bundle: Some(PathBuf::from("bundle")),
    });
    assert_eq!(
        paths,
        ["explicit", "lookup", "path", "bundle"]
            .into_iter()
            .map(PathBuf::from)
            .collect::<Vec<_>>()
    );
}

#[cfg(unix)]
#[test]
fn provider_binary_identity_rejects_a_companion_symlink_outside_the_bundle() {
    let root = tempfile::tempdir().expect("fixture root");
    let bundle = root.path().join("bundle");
    let outside = root.path().join("outside");
    fs::create_dir_all(&bundle).expect("bundle directory");
    fs::create_dir_all(&outside).expect("outside directory");
    let binary = bundle.join("codex");
    let external_companion = outside.join("codex-code-mode-host");
    fs::write(&binary, "fake binary").expect("fake binary bytes");
    fs::write(&external_companion, "fake companion").expect("fake companion bytes");
    for path in [&binary, &external_companion] {
        fs::set_permissions(path, fs::Permissions::from_mode(0o700))
            .expect("fake executable permission");
    }
    symlink(&external_companion, bundle.join("codex-code-mode-host"))
        .expect("external companion symlink");
    assert!(binary_file_identity(&binary).is_ok());
    assert_eq!(
        companion_identities(&binary, NativePlatform::LinuxX64).err(),
        Some(BinaryIdentityError::CompanionOutsideSource(
            "codex-code-mode-host"
        ))
    );
}

#[test]
fn windows_npm_cmd_shim_reports_the_named_unsupported_outcome() {
    assert_eq!(
        resolve_npm_launcher(
            Path::new("/fixture/AppData/Roaming/npm/codex.cmd"),
            b"{}",
            &[],
            NativePlatform::WindowsX64,
        ),
        Err(BinaryResolutionError::UnsupportedWindowsShimResolution)
    );
}
