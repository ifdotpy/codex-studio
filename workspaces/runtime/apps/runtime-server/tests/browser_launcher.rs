//! The launcher of agent sign-in links runs this binary (docs/vm-browser-bridge.md,
//! "Native chats"): `xdg-open URL` in the host bridge's PATH folder reaches the bridge with
//! no Python, with the exit codes and messages of `studio_browser.py`.
#![cfg(unix)]

use std::process::Command;
use std::sync::Arc;
use studio_runtime_services::browser_host::{
    ChatContext, ChatLookup, HostBridgeOptions, HostBridgeService, current_uid, write_launcher,
};

struct OneChat;

impl ChatLookup for OneChat {
    fn chat<'a>(
        &'a self,
        session: &'a str,
    ) -> futures_util::future::BoxFuture<'a, Option<ChatContext>> {
        let chat = (session == "6f1c2b3a-4d5e-4f60-8a9b-0c1d2e3f4a5b").then(|| ChatContext {
            uid: current_uid(),
            project_id: None,
            owner: "person".into(),
            agent_id: "chat-1".into(),
        });
        Box::pin(std::future::ready(chat))
    }
}

fn open(service: &HostBridgeService, folder: &std::path::Path, url: &str) -> std::process::Output {
    Command::new(folder.join("bin/xdg-open"))
        .arg(url)
        .env("STUDIO_BROWSER_SOCKET", &service.open_socket)
        .env(
            "STUDIO_BROWSER_SESSION",
            "6f1c2b3a-4d5e-4f60-8a9b-0c1d2e3f4a5b",
        )
        .env_remove("LAYR_SESSION")
        .output()
        .expect("run the launcher")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn xdg_open_queues_a_link_through_the_runtime_binary() {
    let state = tempfile::tempdir().expect("test step");
    let service = HostBridgeService::start(
        state.path(),
        Arc::new(OneChat),
        HostBridgeOptions::default(),
        Some(std::path::Path::new(env!(
            "CARGO_BIN_EXE_studio-runtime-server"
        ))),
    )
    .expect("test step");
    let folder = state.path().join("browser-host");
    let launcher = write_launcher(
        &folder,
        std::path::Path::new(env!("CARGO_BIN_EXE_studio-runtime-server")),
    )
    .expect("test step");
    assert_eq!(launcher, folder.join("studio-browser"));

    let accepted = tokio::task::block_in_place(|| {
        open(
            &service,
            &folder,
            "https://device.example.com/?user_code=ABCD-EFGH",
        )
    });
    assert!(
        accepted.status.success(),
        "{}",
        String::from_utf8_lossy(&accepted.stderr)
    );
    assert_eq!(
        String::from_utf8_lossy(&accepted.stderr).trim(),
        "studio-browser: Studio asks the person to open device.example.com. The sign-in continues in the browser on the Mac after the approval."
    );
    let queue: serde_json::Value = serde_json::from_slice(
        &std::fs::read(folder.join("browser/requests.json")).expect("test step"),
    )
    .expect("test step");
    let item = queue["items"]
        .as_object()
        .expect("test step")
        .values()
        .next()
        .expect("test step")
        .clone();
    assert_eq!(item["agentId"], "chat-1");
    assert_eq!(item["state"], "pending");

    let refused =
        tokio::task::block_in_place(|| open(&service, &folder, "ftp://files.example.com/"));
    assert_eq!(refused.status.code(), Some(1));
    assert_eq!(
        String::from_utf8_lossy(&refused.stderr).trim(),
        "studio-browser: Only https sign-in links and loopback sign-in pages can open"
    );
    let usage = Command::new(&launcher).output().expect("test step");
    assert_eq!(usage.status.code(), Some(2));
    service.stop().await;
}
