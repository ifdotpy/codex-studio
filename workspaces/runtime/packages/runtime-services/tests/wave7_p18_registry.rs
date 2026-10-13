#[allow(clippy::duplicate_mod)]
#[path = "support/prepared_python.rs"]
mod prepared_python_support;
use serde_json::{Value, json};
use std::{path::Path, process::Command};
use studio_runtime_services::account_driver::{AccountDriver, CodexLoginRequest, LoginReservation};

const REQUEST: &str = "4b154e4b-80bb-4f1f-b07f-dcaa15563190";
fn request(existing: bool) -> CodexLoginRequest {
    CodexLoginRequest {
        login_id: REQUEST.into(),
        account_key: existing.then(|| "default".into()),
        email: None,
        label: None,
    }
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("registry")).expect("JSON")
}

#[test]
fn registry_login_writes_match_python_and_replay_without_writes() {
    let source = Path::new(env!("CARGO_MANIFEST_DIR"));
    let output = Command::new(prepared_python_support::prepared_python(
        &source.join("../../apps/server/src"),
    ))
    .arg(source.join("tests/wave7_p18_registry_oracle.py"))
    .output()
    .expect("oracle");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let cases: Vec<Value> = serde_json::from_slice(&output.stdout).expect("corpus");
    assert_eq!(cases.len(), 6);
    for case in cases {
        let root = tempfile::tempdir().expect("state");
        let path = root.path().join("registry.json");
        std::fs::write(&path, case["initial"].to_string()).expect("Python-written registry");
        let driver = AccountDriver::open(&path).expect("open");
        let request = request(case["existing"].as_bool().expect("existing"));
        let home = Path::new(case["home"].as_str().expect("home"));
        let LoginReservation::Dispatch(reserved) = driver
            .reserve_codex_login(&request, home, 10.0)
            .expect("reserve")
        else {
            panic!("first reservation")
        };
        assert_eq!(read(&path)["logins"][REQUEST], reserved);
        let result = driver
            .finish_codex_login(
                REQUEST,
                reserved["accountKey"].as_str().expect("account"),
                (case["outcome"] == "written").then_some(&case["response"]),
                case["outcome"] != "not_sent",
                10.0,
            )
            .expect("receipt");
        assert_eq!(result, case["result"]);
        assert_eq!(read(&path), case["stored"]);
        let saved = std::fs::read(&path).expect("saved");
        assert!(matches!(
            driver
                .reserve_codex_login(&request, home, 20.0)
                .expect("replay"),
            LoginReservation::Replay(_)
        ));
        assert_eq!(std::fs::read(&path).expect("unchanged"), saved);
        let mut conflict = request.clone();
        conflict.label = Some("different".into());
        assert!(driver.reserve_codex_login(&conflict, home, 20.0).is_err());
        assert_eq!(std::fs::read(&path).expect("unchanged"), saved);
    }
}

#[test]
fn restart_and_cancellation_latch_never_authorize_another_send() {
    let root = tempfile::tempdir().expect("state");
    let path = root.path().join("registry.json");
    let initial = json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":root.path(),"source":"Fixture","label":"Default","status":"signedOut"}},"logins":{}});
    std::fs::write(&path, initial.to_string()).expect("state");
    let driver = AccountDriver::open(&path).expect("open");
    driver
        .reserve_codex_login(&request(false), root.path(), 1.0)
        .expect("reserve");
    drop(driver);
    let driver = AccountDriver::open(&path).expect("restart");
    driver.recover_codex_logins().expect("recover");
    assert!(matches!(
        driver
            .reserve_codex_login(&request(false), root.path(), 2.0)
            .expect("replay"),
        LoginReservation::Replay(_)
    ));
    let account = format!("login-{REQUEST}");
    driver.finish_codex_login(REQUEST, &account, Some(&json!({"type":"chatgptDeviceCode","loginId":"native","verificationUrl":"https://example.invalid","userCode":"fixture"})), true, 3.0).expect("late receipt");
    assert!(matches!(
        driver.reserve_codex_cancel(REQUEST).expect("cancel intent"),
        LoginReservation::Dispatch(_)
    ));
    driver.finish_codex_login(REQUEST, &account, Some(&json!({"type":"chatgptDeviceCode","loginId":"native","verificationUrl":"https://example.invalid","userCode":"fixture"})), true, 4.0).expect("duplicate start receipt cannot clear cancellation latch");
    drop(driver);
    let driver = AccountDriver::open(&path).expect("restart cancellation");
    assert!(matches!(
        driver
            .reserve_codex_cancel(REQUEST)
            .expect("unknown cancellation"),
        LoginReservation::Replay(_)
    ));
    assert_eq!(
        driver
            .finish_codex_cancel(REQUEST, &account, true)
            .expect("late cancellation")["status"],
        "cancelled"
    );
    assert!(matches!(
        driver
            .reserve_codex_cancel(REQUEST)
            .expect("terminal cancellation"),
        LoginReservation::Replay(_)
    ));
}

#[test]
fn early_completion_and_foreign_notification_preserve_exact_identity() {
    use studio_runtime_services::account_driver::{AuthMetadata, AuthStatus};
    let root = tempfile::tempdir().expect("state");
    let path = root.path().join("registry.json");
    std::fs::write(&path,json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":root.path(),"label":"Default","source":"Fixture","status":"ready","accountId":"subscription"}},"logins":{}}).to_string()).expect("registry");
    let driver = AccountDriver::open(&path).expect("open");
    driver
        .reserve_codex_login(&request(true), root.path(), 1.0)
        .expect("reserve");
    let auth = AuthMetadata {
        status: AuthStatus::Ready,
        account_id: Some("subscription".into()),
        email: None,
        plan: None,
        credential_identity: None,
        error: None,
        error_kind: None,
    };
    let completed = driver
        .complete_codex_login(REQUEST, "default", "native", true, Some(&auth))
        .expect("completion before start acknowledgement");
    assert_eq!(completed["status"], "ready");
    let before = read(&path);
    let result = driver.finish_codex_login(REQUEST,"default",Some(&json!({"type":"chatgptDeviceCode","loginId":"native","verificationUrl":"https://example.invalid","userCode":"fixture"})),true,2.0).expect("late acknowledgement");
    assert_eq!(result, completed);
    assert_eq!(read(&path), before);
    assert!(
        driver
            .complete_codex_login(REQUEST, "default", "foreign", false, None)
            .is_err()
    );
    assert_eq!(read(&path), before);
}

#[test]
fn registry_failpoints_before_every_login_write_leave_a_recoverable_receipt() {
    let response = json!({"type":"chatgptDeviceCode","loginId":"native","verificationUrl":"https://example.invalid","userCode":"fixture"});
    for boundary in [
        "start_intent",
        "start_receipt",
        "cancel_intent",
        "cancel_receipt",
    ] {
        let root = tempfile::tempdir().expect("state");
        let path = root.path().join("registry.json");
        std::fs::write(&path,json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{"id":"default","home":root.path(),"label":"Default","source":"Fixture","status":"signedOut"}},"logins":{}}).to_string()).expect("registry");
        let driver = AccountDriver::open(&path).expect("open");
        let account = format!("login-{REQUEST}");
        if boundary != "start_intent" {
            driver
                .reserve_codex_login(&request(false), root.path(), 1.0)
                .expect("intent");
        }
        if matches!(boundary, "cancel_intent" | "cancel_receipt") {
            driver
                .finish_codex_login(REQUEST, &account, Some(&response), true, 2.0)
                .expect("receipt");
        }
        if boundary == "cancel_receipt" {
            driver.reserve_codex_cancel(REQUEST).expect("cancel intent");
        }
        let before = read(&path);
        let saved = root.path().join("saved.json");
        std::fs::rename(&path, &saved).expect("hold state");
        std::fs::create_dir(&path).expect("fail replace");
        match boundary {
            "start_intent" => assert!(
                driver
                    .reserve_codex_login(&request(false), root.path(), 3.0)
                    .is_err()
            ),
            "start_receipt" => assert!(
                driver
                    .finish_codex_login(REQUEST, &account, Some(&response), true, 3.0)
                    .is_err()
            ),
            "cancel_intent" => assert!(driver.reserve_codex_cancel(REQUEST).is_err()),
            _ => assert!(driver.finish_codex_cancel(REQUEST, &account, true).is_err()),
        }
        std::fs::remove_dir(&path).expect("clear failure");
        std::fs::rename(saved, &path).expect("restore");
        assert_eq!(read(&path), before, "{boundary}");
        drop(driver);
        let driver = AccountDriver::open(&path).expect("restart");
        driver.recover_codex_logins().expect("recover");
        if boundary != "start_intent" {
            assert!(
                matches!(
                    driver
                        .reserve_codex_login(&request(false), root.path(), 4.0)
                        .expect("replay"),
                    LoginReservation::Replay(_)
                ),
                "{boundary}"
            );
        }
    }
}
