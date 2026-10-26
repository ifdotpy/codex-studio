use super::*;
use serde_json::json;
use std::sync::{Mutex, atomic::AtomicBool};
use studio_client_core::{
    backend::Backend,
    client::Client,
    protocol::Request,
    server_keys::{self, Vault},
};

struct Memory(Mutex<Value>);
impl Vault for Memory {
    fn read(&self) -> Result<Value, String> {
        Ok(self.0.lock().unwrap().clone())
    }
    fn write(&self, value: &Value) -> Result<(), String> {
        *self.0.lock().unwrap() = value.clone();
        Ok(())
    }
}
fn vault() -> Arc<Memory> {
    let credential = |server: &str| {
        let mut value=server_keys::new_pair(&json!({"protocol":1,"origin":format!("https://{server}.tailnet.ts.net"),"serverId":server,
            "inviteId":"fixture-invite","token":"fixture-token","publicKey":"fixture-public","tailscaleUser":"fixture-user"}),"fixture-pair").unwrap();
        value.as_object_mut().unwrap().remove("body");
        value
    };
    Arc::new(Memory(Mutex::new(
        json!({"drafts":{},"servers":{"selected":credential("selected"),"unopened":credential("unopened")}}),
    )))
}
async fn context(
    directory: &std::path::Path,
    server: &str,
    backend: Backend,
    workspace: &str,
) -> Context {
    let client =
        Arc::new(Client::open_with_backend(directory.into(), backend.clone(), |_| {}).unwrap());
    client
        .request(Request {
            id: format!("identity-{server}"),
            method: "identity.set".into(),
            params: json!({"workspaceId":workspace}),
        })
        .await
        .unwrap()
        .result
        .unwrap();
    Context {
        server_id: server.into(),
        workspace_id: workspace.into(),
        client,
        backend: Some(backend),
        source_identity: "c".repeat(64),
        migration_complete: AtomicBool::new(true),
        archive_hold: false,
    }
}

#[tokio::test]
async fn native_registry_host_snapshot_uses_exact_context_and_only_public_vault_rows() {
    let directory = tempfile::tempdir().unwrap();
    let vault = vault();
    let remote = Arc::new(RemoteClient::new(vault.clone()).unwrap());
    let local = context(
        &directory.path().join("local"),
        "local",
        Backend::new("http://127.0.0.1:4620").unwrap(),
        &"a".repeat(32),
    )
    .await;
    let selected = context(
        &directory.path().join("remote"),
        "selected",
        Backend::from_remote(remote.bind("selected").unwrap()).unwrap(),
        &"b".repeat(32),
    )
    .await;
    let value = snapshot(&remote, &local, &selected).unwrap();
    let contract = server_keys::registry_contract();
    let mut expected = contract["snapshotFields"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect::<Vec<_>>();
    expected.sort();
    assert_eq!(
        value
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<Vec<_>>(),
        expected
    );
    assert_eq!(value["serverId"], "selected");
    assert_eq!(value["workspaceId"], "b".repeat(32));
    assert_eq!(value["local"]["workspaceId"], "a".repeat(32));
    let rows = value["servers"].as_array().unwrap();
    assert_eq!(
        rows.iter().find(|v| v["id"] == "selected").unwrap()["workspaceId"],
        "b".repeat(32)
    );
    assert!(
        rows.iter()
            .find(|v| v["id"] == "unopened")
            .unwrap()
            .get("workspaceId")
            .is_none()
    );
    let public = value.to_string();
    for forbidden in [
        "privateKey",
        "publicKey",
        "storageKey",
        "imports",
        "token",
        "signature",
    ] {
        assert!(!public.contains(forbidden));
    }
    let state = vault.read().unwrap();
    assert!(!public.contains(state["servers"]["selected"]["privateKey"].as_str().unwrap()));
}

#[tokio::test]
async fn native_registry_host_refuses_stale_foreign_and_missing_local_contexts() {
    let directory = tempfile::tempdir().unwrap();
    let vault = vault();
    let remote = Arc::new(RemoteClient::new(vault.clone()).unwrap());
    let local = context(
        &directory.path().join("local"),
        "local",
        Backend::new("http://127.0.0.1:4620").unwrap(),
        &"a".repeat(32),
    )
    .await;
    let mut selected = context(
        &directory.path().join("remote"),
        "selected",
        Backend::from_remote(remote.bind("selected").unwrap()).unwrap(),
        &"b".repeat(32),
    )
    .await;
    snapshot(&remote, &local, &selected).unwrap();
    selected.server_id = "foreign".into();
    assert!(snapshot(&remote, &local, &selected).is_err());
    selected.server_id = "selected".into();
    selected.workspace_id = "invalid".into();
    assert!(snapshot(&remote, &local, &selected).is_err());
    selected.workspace_id = "b".repeat(32);
    {
        let mut state = vault.0.lock().unwrap();
        let mut other = state["servers"]["selected"].clone();
        other["serverId"] = json!("unopened");
        state["servers"]["unopened"] = other;
    }
    selected.server_id = "unopened".into();
    assert!(
        snapshot(&remote, &local, &selected).is_err(),
        "Matching public metadata cannot replace the binding's exact server ID."
    );
    selected.server_id = "selected".into();
    assert!(
        snapshot(&remote, &selected, &local).is_err(),
        "A remote context cannot supply local authority."
    );
    remote
        .dispatch("servers.forget", &json!({"serverId":"selected"}))
        .await
        .unwrap();
    assert!(
        snapshot(&remote, &local, &selected).is_err(),
        "A forgotten context must not be silently rebound."
    );
}

#[tokio::test]
async fn native_registry_host_event_contains_full_snapshot_and_relay_drop_closes_delivery() {
    let snapshot = json!({"serverId":"local","workspaceId":"a".repeat(32),"revision":"1",
        "local":{"id":"local","label":"This device","origin":"http://127.0.0.1:4620","workspaceId":"a".repeat(32)},"servers":[]});
    let (changes, changed) = tokio::sync::watch::channel("0".to_owned());
    let (send, mut receive) = tokio::sync::mpsc::unbounded_channel();
    let expected = snapshot.clone();
    let producer = relay(
        changed,
        move || {
            let value = snapshot.clone();
            async move { vec![("owned-context".into(), Ok(value))] }
        },
        move |value| {
            send.send(value).unwrap();
        },
    );
    changes.send_replace("1".into());
    let packet = receive
        .recv()
        .await
        .expect("A revision must carry its full metadata snapshot.");
    let topic = server_keys::registry_contract()["eventTopic"]
        .as_str()
        .unwrap();
    assert_eq!(
        packet,
        json!({"event":format!("owned-context:{topic}"),"value":expected})
    );
    drop(producer);
    changes.send_replace("2".into());
    assert!(
        receive.recv().await.is_none(),
        "Relay teardown must release its callback and event receiver."
    );
}

#[test]
fn native_registry_host_error_event_keeps_scope_and_shared_contract() {
    let value = event(
        "owned-context",
        Err("The selected pairing was removed.".into()),
    );
    let contract = server_keys::registry_contract();
    assert_eq!(
        value["event"],
        format!("owned-context:{}", contract["eventTopic"].as_str().unwrap())
    );
    assert!(value.get("value").is_none());
    assert_eq!(
        value["error"]["message"],
        "The selected pairing was removed."
    );
}
