//! A Codex "sign in again" receipt completes from the provider's
//! `account/login/completed` notification, and a receipt whose completion was
//! lost cannot stay active forever. A receipt is never ready for an account id
//! other than the one it expects.
use serde_json::{Value, json};
use std::path::Path;
use studio_runtime_services::account_driver::{
    AccountDriver, CODEX_LOGIN_RECEIPT_LIFETIME_SECONDS, CodexLoginRequest, LOGIN_EXPIRED_ERROR,
    LoginReservation,
};

const REQUEST: &str = "e2a23555-f94f-4bdc-be88-8e0eb4b187d5";
const NATIVE_LOGIN: &str = "d10bc5c7-dc36-47a3-b341-d67ddc5121f2";
const EXPECTED: &str = "expected-account";
const OTHER: &str = "other-account";

fn now() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs_f64()
}

fn credentials(home: &Path, account_id: &str) {
    std::fs::write(
        home.join("auth.json"),
        json!({"tokens":{"account_id":account_id,"access_token":"fixture"}}).to_string(),
    )
    .expect("credentials");
}

/// A ready Codex account `default` with credentials for [`EXPECTED`], and a
/// pending "sign in again" receipt created at `created`.
fn pending_reauth(root: &Path, created: f64) -> AccountDriver {
    let home = root.join("home");
    std::fs::create_dir_all(&home).expect("home");
    credentials(&home, EXPECTED);
    let path = root.join("registry.json");
    std::fs::write(
        &path,
        json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{
            "id":"default","home":home,"label":"Default","source":"Fixture",
            "status":"ready","accountId":EXPECTED}},"logins":{}})
        .to_string(),
    )
    .expect("registry");
    let driver = AccountDriver::open(&path).expect("open");
    let request = CodexLoginRequest {
        login_id: REQUEST.into(),
        account_key: Some("default".into()),
        email: None,
        label: None,
    };
    let LoginReservation::Dispatch(_) = driver
        .reserve_codex_login(&request, &home, created)
        .expect("reserve")
    else {
        panic!("first reservation");
    };
    let receipt = driver
        .finish_codex_login(
            REQUEST,
            "default",
            Some(&json!({"type":"chatgptDeviceCode","loginId":NATIVE_LOGIN,
                "verificationUrl":"https://example.invalid/device","userCode":"FIXTURE"})),
            true,
            None,
            created,
        )
        .expect("pending");
    assert_eq!(receipt["status"], "pending");
    driver
}

/// The persisted receipt; the registry file is the durable owner.
fn stored(root: &Path) -> Value {
    let registry: Value =
        serde_json::from_slice(&std::fs::read(root.join("registry.json")).expect("registry"))
            .expect("registry JSON");
    registry["logins"][REQUEST].clone()
}

fn home(root: &Path) -> std::path::PathBuf {
    root.join("home")
}

#[test]
fn completion_notification_finishes_only_the_same_native_login() {
    let root = tempfile::tempdir().expect("state");
    let driver = pending_reauth(root.path(), now());
    for foreign in [
        json!({"loginId":"another-login","success":true,"error":null}),
        json!({"success":true,"error":null}),
    ] {
        assert!(
            driver
                .codex_login_completed("default", &foreign)
                .expect("foreign")
                .is_empty()
        );
        assert_eq!(stored(root.path())["status"], "pending");
    }
    let changed = driver
        .codex_login_completed(
            "other-account-key",
            &json!({"loginId":NATIVE_LOGIN,"success":true,"error":null}),
        )
        .expect("other account");
    assert!(changed.is_empty());
    assert_eq!(stored(root.path())["status"], "pending");

    credentials(&home(root.path()), EXPECTED);
    driver
        .codex_login_completed(
            "default",
            &json!({"loginId":NATIVE_LOGIN,"success":true,"error":null}),
        )
        .expect("completion");
    let ready = stored(root.path());
    assert_eq!(ready["status"], "ready", "{ready}");
    assert_eq!(ready["resolvedAccountKey"], "default");
    assert!(ready.get("userCode").is_none());
}

#[test]
fn completion_for_another_account_id_is_an_error_never_ready() {
    let root = tempfile::tempdir().expect("state");
    let driver = pending_reauth(root.path(), now());
    credentials(&home(root.path()), OTHER);
    driver
        .codex_login_completed(
            "default",
            &json!({"loginId":NATIVE_LOGIN,"success":true,"error":null}),
        )
        .expect("completion");
    let receipt = stored(root.path());
    assert_eq!(receipt["status"], "error", "{receipt}");
    assert!(receipt.get("resolvedAccountKey").is_none());
}

#[test]
fn failed_completion_ends_the_receipt_with_an_error() {
    let root = tempfile::tempdir().expect("state");
    let driver = pending_reauth(root.path(), now());
    driver
        .codex_login_completed(
            "default",
            &json!({"loginId":NATIVE_LOGIN,"success":false,"error":"expired"}),
        )
        .expect("failure");
    assert_eq!(stored(root.path())["status"], "error");
}

#[test]
fn lost_completion_is_recovered_from_credentials_written_during_the_sign_in() {
    let root = tempfile::tempdir().expect("state");
    let created = now() - 60.0;
    let driver = pending_reauth(root.path(), created);
    // Credentials written before the request started prove nothing.
    let unchanged = tempfile::tempdir().expect("state");
    let waiting = pending_reauth(unchanged.path(), now() + 60.0);
    assert!(!waiting.reconcile_codex_logins(now()).expect("reconcile"));
    assert_eq!(stored(unchanged.path())["status"], "pending");

    credentials(&home(root.path()), EXPECTED);
    assert!(driver.reconcile_codex_logins(now()).expect("reconcile"));
    let ready = stored(root.path());
    assert_eq!(ready["status"], "ready", "{ready}");
    assert_eq!(ready["resolvedAccountKey"], "default");
    assert!(!driver.reconcile_codex_logins(now()).expect("replay"));
}

#[test]
fn lost_completion_with_another_account_id_is_an_error_never_ready() {
    let root = tempfile::tempdir().expect("state");
    let driver = pending_reauth(root.path(), now() - 60.0);
    credentials(&home(root.path()), OTHER);
    driver.reconcile_codex_logins(now()).expect("reconcile");
    let receipt = stored(root.path());
    assert_eq!(receipt["status"], "error", "{receipt}");
    assert!(receipt.get("resolvedAccountKey").is_none());
}

#[test]
fn an_unfinished_receipt_expires_after_its_deadline() {
    let root = tempfile::tempdir().expect("state");
    let created = now() + 60.0;
    let driver = pending_reauth(root.path(), created);
    assert!(
        !driver
            .reconcile_codex_logins(created + CODEX_LOGIN_RECEIPT_LIFETIME_SECONDS)
            .expect("at the deadline")
    );
    assert_eq!(stored(root.path())["status"], "pending");
    assert!(
        driver
            .reconcile_codex_logins(created + CODEX_LOGIN_RECEIPT_LIFETIME_SECONDS + 1.0)
            .expect("after the deadline")
    );
    let expired = stored(root.path());
    assert_eq!(expired["status"], "error");
    assert_eq!(expired["error"], LOGIN_EXPIRED_ERROR);
    assert!(expired.get("userCode").is_none());
    // The account can start a new sign-in again.
    let retry = CodexLoginRequest {
        login_id: "1f6d1b8e-3f0e-4b8e-9d55-6c3f2a1b0c9d".into(),
        account_key: Some("default".into()),
        email: None,
        label: None,
    };
    assert!(matches!(
        driver
            .reserve_codex_login(&retry, &home(root.path()), now())
            .expect("new sign-in"),
        LoginReservation::Dispatch(_)
    ));
}

#[test]
fn credentials_written_after_the_deadline_do_not_complete_an_old_receipt() {
    let root = tempfile::tempdir().expect("state");
    let driver = pending_reauth(
        root.path(),
        now() - 2.0 * CODEX_LOGIN_RECEIPT_LIFETIME_SECONDS,
    );
    credentials(&home(root.path()), EXPECTED);
    driver.reconcile_codex_logins(now()).expect("reconcile");
    let expired = stored(root.path());
    assert_eq!(expired["status"], "error", "{expired}");
    assert_eq!(expired["error"], LOGIN_EXPIRED_ERROR);
}
