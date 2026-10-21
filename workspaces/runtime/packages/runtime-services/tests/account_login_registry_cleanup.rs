//! Sign-in accounts in the account list and their cleanup after a sign-in
//! that never received a provider identity.
use serde_json::{Value, json};
use std::path::Path;
use studio_runtime_services::account_driver::{AccountDriver, CodexLoginRequest, LoginReservation};

const REQUEST: &str = "4b154e4b-80bb-4f1f-b07f-dcaa15563190";
fn request() -> CodexLoginRequest {
    CodexLoginRequest {
        login_id: REQUEST.into(),
        account_key: None,
        email: None,
        label: None,
    }
}
fn read(path: &Path) -> Value {
    serde_json::from_slice(&std::fs::read(path).expect("registry")).expect("JSON")
}

/// Registry rows for the sign-in listing and cleanup checks: one account per
/// sign-in outcome, each with its own home under `root`.
fn sign_in_outcomes(root: &Path) -> Value {
    let row = |key: &str, status: &str, identity: bool| {
        let home = root.join(key);
        std::fs::create_dir_all(&home).expect("home");
        json!({"id":key,"home":home,"label":"Fixture","source":"Codex Agents",
            "accountId":identity.then_some("fixture-subscription"),"email":null,"plan":null,"status":status})
    };
    let receipt = |request: &str, key: &str, status: &str| json!({"requestId":request,"accountKey":key,"status":status,"createdAt":1.0});
    json!({
        "version":1,"defaultAccountKey":"default",
        "accounts":{
            "default":{"id":"default","home":root,"source":"Fixture","label":"Default","status":"signedOut"},
            "login-identity":row("login-identity","signedOut",true),
            "login-failed":row("login-failed","error",false),
            "login-cancelled":row("login-cancelled","signedOut",false),
            "login-pending":row("login-pending","pending",false),
            "login-no-receipt":row("login-no-receipt","error",false)
        },
        "logins":{
            "00000000-0000-4000-8000-000000000001":receipt("00000000-0000-4000-8000-000000000001","login-identity","ready"),
            "00000000-0000-4000-8000-000000000002":receipt("00000000-0000-4000-8000-000000000002","login-failed","error"),
            "00000000-0000-4000-8000-000000000003":receipt("00000000-0000-4000-8000-000000000003","login-cancelled","cancelled"),
            "00000000-0000-4000-8000-000000000004":receipt("00000000-0000-4000-8000-000000000004","login-pending","pending")
        }
    })
}

#[test]
fn signed_out_sign_in_account_with_identity_stays_listed() {
    let root = tempfile::tempdir().expect("state");
    let path = root.path().join("registry.json");
    std::fs::write(&path, sign_in_outcomes(root.path()).to_string()).expect("registry");
    let driver = AccountDriver::open(&path).expect("open");
    let listed: Vec<String> = driver
        .listing_snapshot()
        .expect("listing")
        .accounts
        .iter()
        .map(|row| row["id"].as_str().expect("id").to_owned())
        .collect();
    assert_eq!(listed, ["default", "login-identity"]);
}

#[test]
fn new_sign_in_removes_only_abandoned_sign_in_accounts() {
    let root = tempfile::tempdir().expect("state");
    let path = root.path().join("registry.json");
    std::fs::write(&path, sign_in_outcomes(root.path()).to_string()).expect("registry");
    let driver = AccountDriver::open(&path).expect("open");
    let home = root.path().join(format!("login-{REQUEST}"));
    assert!(matches!(
        driver
            .reserve_codex_login(&request(), &home, 2.0)
            .expect("reserve"),
        LoginReservation::Dispatch(_)
    ));
    let stored = read(&path);
    let accounts: std::collections::BTreeSet<String> = stored["accounts"]
        .as_object()
        .expect("accounts")
        .keys()
        .cloned()
        .collect();
    let new_account = format!("login-{REQUEST}");
    assert_eq!(
        accounts,
        [
            "default",
            "login-identity",
            "login-pending",
            "login-no-receipt",
            new_account.as_str(),
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    // Receipts stay: a repeated request id still replays its saved outcome.
    assert_eq!(
        stored["logins"]["00000000-0000-4000-8000-000000000002"]["status"],
        "error"
    );
    // Homes stay on disk.
    assert!(root.path().join("login-failed").is_dir());
}
