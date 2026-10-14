use serde_json::json;
use std::fs;
use studio_runtime_store::local_server_id;

fn private_identity(root: &std::path::Path, contents: &[u8]) -> std::path::PathBuf {
    let directory = root.join("multi-server");
    fs::create_dir(&directory).expect("credential directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("private credential directory");
    }
    let path = directory.join("identity.json");
    fs::write(&path, contents).expect("identity file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))
            .expect("private identity file");
    }
    path
}

fn assert_identity_error(root: &std::path::Path, reason: &str) {
    let error = local_server_id(root).expect_err("invalid identity must refuse startup");
    let message = error.to_string();
    assert!(message.contains("identity.json"), "{message}");
    assert!(message.contains(reason), "{message}");
}

#[test]
fn missing_identity_is_a_read_only_absent_value() {
    let root = tempfile::tempdir().expect("isolated state");
    assert_eq!(
        local_server_id(root.path()).expect("missing is allowed"),
        None
    );
    assert!(!root.path().join("multi-server").exists());

    let directory = root.path().join("multi-server");
    fs::create_dir(&directory).expect("credential directory");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&directory, fs::Permissions::from_mode(0o700))
            .expect("private credential directory");
    }
    assert_eq!(
        local_server_id(root.path()).expect("missing file is allowed"),
        None
    );
    assert!(!directory.join("identity.json").exists());
}

#[test]
fn malformed_identity_is_refused() {
    let root = tempfile::tempdir().expect("isolated state");
    private_identity(root.path(), b"not json");
    assert_identity_error(root.path(), "malformed JSON");
}

#[test]
fn safe_partial_identity_yields_only_its_usable_server_id() {
    let root = tempfile::tempdir().expect("isolated state");
    private_identity(root.path(), br#"{"serverId":"fixture-server"}"#);
    assert_eq!(
        local_server_id(root.path()).expect("usable ID"),
        Some("fixture-server".to_owned())
    );
}

#[test]
fn missing_empty_and_invalid_server_ids_are_refused() {
    for identity in [
        json!({}),
        json!({"serverId": ""}),
        json!({"serverId": "bad/id"}),
    ] {
        let root = tempfile::tempdir().expect("isolated state");
        private_identity(root.path(), identity.to_string().as_bytes());
        assert_identity_error(root.path(), "serverId");
    }
}

#[cfg(unix)]
#[test]
fn unsafe_identity_permissions_are_refused() {
    use std::os::unix::fs::PermissionsExt;

    let directory_root = tempfile::tempdir().expect("isolated state");
    private_identity(directory_root.path(), br#"{"serverId":"fixture-server"}"#);
    fs::set_permissions(
        directory_root.path().join("multi-server"),
        fs::Permissions::from_mode(0o755),
    )
    .expect("unsafe directory mode");
    assert_identity_error(directory_root.path(), "permissions");

    let file_root = tempfile::tempdir().expect("isolated state");
    let path = private_identity(file_root.path(), br#"{"serverId":"fixture-server"}"#);
    fs::set_permissions(&path, fs::Permissions::from_mode(0o644)).expect("unsafe file mode");
    assert_identity_error(file_root.path(), "permissions");
}
