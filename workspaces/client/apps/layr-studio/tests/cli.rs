#![allow(clippy::unwrap_used)]
use std::process::{Command, Output};

fn layr_studio(profile: &std::path::Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_layr-studio"))
        .args(args)
        .env("CODEX_DESKTOP_PROFILE", profile)
        .output()
        .unwrap()
}

fn profile() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("server-keys.json"),
        r#"{"drafts":{},"servers":{"alpha":{"origin":"https://alpha.example","label":"Alpha"},"beta":{"origin":"https://beta.example"}}}"#,
    )
    .unwrap();
    dir
}

#[test]
fn layr_studio_lists_and_forgets_servers_of_a_profile() {
    let dir = profile();
    let text = layr_studio(dir.path(), &["servers", "list"]);
    assert_eq!(
        String::from_utf8_lossy(&text.stdout),
        "alpha\thttps://alpha.example\tAlpha\nbeta\thttps://beta.example\tbeta\n"
    );
    let json = layr_studio(dir.path(), &["servers", "list", "--json"]);
    assert!(String::from_utf8_lossy(&json.stdout).contains(r#""id":"alpha""#));
    assert!(
        layr_studio(dir.path(), &["servers", "forget", "alpha"])
            .status
            .success()
    );
    let text = layr_studio(dir.path(), &["servers", "list"]);
    assert_eq!(
        String::from_utf8_lossy(&text.stdout),
        "beta\thttps://beta.example\tbeta\n"
    );
}

#[test]
fn layr_studio_forget_of_an_unknown_id_fails_clearly() {
    let dir = profile();
    let out = layr_studio(dir.path(), &["servers", "forget", "nope"]);
    assert!(!out.status.success());
    assert_eq!(
        String::from_utf8_lossy(&out.stderr),
        "Unknown server: nope\n"
    );
    assert!(
        !layr_studio(dir.path(), &["servers", "bogus"])
            .status
            .success()
    );
}
