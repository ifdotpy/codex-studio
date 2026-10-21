//! The provider's `account/login/completed` notification reaches the account
//! registry through the account's retained session. Before this, the journal
//! settled the notification with no effect, so a "sign in again" receipt
//! stayed pending after the browser sign-in finished (Lumina 2 account, 21
//! October 2026).
use super::*;
use crate::account_driver::{AccountDriver, CodexLoginRequest, LoginReservation};

const REQUEST: &str = "e2a23555-f94f-4bdc-be88-8e0eb4b187d5";
const NATIVE_LOGIN: &str = "d10bc5c7-dc36-47a3-b341-d67ddc5121f2";
const EXPECTED: &str = "expected-account";

fn credentials(home: &std::path::Path, account_id: &str) {
    std::fs::write(
        home.join("auth.json"),
        json!({"tokens":{"account_id":account_id,"access_token":"fixture"}}).to_string(),
    )
    .unwrap();
}

/// A ready Codex account `default` and its pending "sign in again" receipt.
fn pending_reauth(state: &State) -> (Arc<AccountDriver>, PathBuf) {
    let home = state.root.join("codex-home");
    let registry = state.root.join("accounts/registry.json");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(registry.parent().unwrap()).unwrap();
    credentials(&home, EXPECTED);
    std::fs::write(
        &registry,
        json!({"version":1,"defaultAccountKey":"default","accounts":{"default":{
            "id":"default","home":home,"label":"Fixture","source":"Codex CLI",
            "status":"ready","provider":"codex","accountId":EXPECTED}},
            "logins":{},"deleteReceipts":{}})
        .to_string(),
    )
    .unwrap();
    let driver = Arc::new(AccountDriver::open(&registry).unwrap());
    let created = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs_f64();
    let request = CodexLoginRequest {
        login_id: REQUEST.into(),
        account_key: Some("default".into()),
        email: None,
        label: None,
    };
    let LoginReservation::Dispatch(_) = driver
        .reserve_codex_login(&request, &home, created)
        .unwrap()
    else {
        panic!("first reservation");
    };
    let pending = driver
        .finish_codex_login(
            REQUEST,
            "default",
            Some(&json!({"type":"chatgptDeviceCode","loginId":NATIVE_LOGIN,
                "verificationUrl":"https://example.invalid/device","userCode":"FIXTURE"})),
            true,
            None,
            created,
        )
        .unwrap();
    assert_eq!(pending["status"], "pending");
    (driver, registry)
}

fn stored(registry: &std::path::Path) -> Value {
    let value: Value = serde_json::from_slice(&std::fs::read(registry).unwrap()).unwrap();
    value["logins"][REQUEST].clone()
}

#[tokio::test]
async fn retained_session_login_completion_finishes_the_reauth_receipt() {
    let state = State::new().await;
    let (driver, registry) = pending_reauth(&state);
    let mut resources = test_resources().unwrap();
    resources.claude_account_pin = driver.internal_record("default").unwrap();
    resources.account_driver = Some(Arc::clone(&driver));
    let mut peer = Peer::new();
    let mut worker = peer
        .open_worker_with_resources(state.bridge.clone(), resources)
        .await;

    // A completion of another native login changes nothing.
    let cursor = peer
        .frame(json!({"method":"account/login/completed",
            "params":{"loginId":"another-login","success":true,"error":null}}))
        .await;
    peer.acknowledged(cursor).await;
    assert_eq!(stored(&registry)["status"], "pending");

    // The provider wrote the new credentials, then reported the completion.
    credentials(&state.root.join("codex-home"), EXPECTED);
    let cursor = peer
        .frame(json!({"method":"account/login/completed",
            "params":{"loginId":NATIVE_LOGIN,"success":true,"error":null}}))
        .await;
    peer.acknowledged(cursor).await;
    let receipt = stored(&registry);
    assert_eq!(receipt["status"], "ready", "{receipt}");
    assert_eq!(receipt["nativeCompleted"], true, "{receipt}");
    assert_eq!(receipt["resolvedAccountKey"], "default", "{receipt}");
    worker.shutdown().await.unwrap();
}
