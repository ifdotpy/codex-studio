#[path = "claude_runtime/login_fixture.rs"]
mod fixture;
use fixture::*;
use std::sync::atomic::Ordering;
use studio_runtime_services::claude_login::ports::LoginReceipts;
#[tokio::test]
async fn add_replay_completion_and_restart_keep_identity_and_no_secrets() {
    let f = Fixture::new();
    let s = f.open().await;
    let starting = s
        .start(ID.into(), None, None, None)
        .await
        .expect("valid fixture");
    let child = f.launcher.next().await;
    assert!(
        !child
            .command
            .environment
            .contains_key(std::ffi::OsStr::new("ANTHROPIC_API_KEY"))
    );
    assert_eq!(
        s.start(ID.into(), None, None, None)
            .await
            .expect("valid fixture"),
        starting
    );
    assert!(
        s.start(ID.into(), None, Some("other@example.test".into()), None)
            .await
            .is_err()
    );
    link(&f, &s, &child).await;
    assert_eq!(
        s.code(ID, "fixture-code".into())
            .await
            .expect("valid fixture")["codeSubmitted"],
        true
    );
    s.code(ID, "another-code".into())
        .await
        .expect("valid fixture");
    assert_eq!(child.writes.lock().expect("valid fixture").len(), 1);
    let ready = complete(&f, &s, &child).await;
    assert_eq!(ready["email"], "fixture@example.test");
    assert_eq!(ready["chatsRefreshed"], true);
    s.shutdown().await.expect("valid fixture");
    assert!(child.closed.load(Ordering::SeqCst));
    let bytes = std::fs::read(f.receipts.file.directory().join(format!("{ID}.json")))
        .expect("valid fixture");
    let text = String::from_utf8(bytes).expect("valid fixture");
    assert!(!text.contains("fixture-code") && !text.contains(URL));
    let restart = f.open().await;
    assert_eq!(restart.status(ID).await.expect("valid fixture"), ready);
    restart.shutdown().await.expect("valid fixture");
}
#[tokio::test]
async fn code_write_unknown_is_latched_and_never_repeated() {
    let f = Fixture::new();
    let s = f.open().await;
    s.start(ID.into(), None, None, None)
        .await
        .expect("valid fixture");
    let child = f.launcher.next().await;
    link(&f, &s, &child).await;
    child.write_unknown.store(true, Ordering::SeqCst);
    s.code(ID, "fixture-code".into())
        .await
        .expect("valid fixture");
    assert_eq!(
        s.status(ID).await.expect("valid fixture")["status"],
        "error"
    );
    s.code(ID, "fixture-code".into())
        .await
        .expect("valid fixture");
    assert_eq!(child.writes.lock().expect("valid fixture").len(), 1);
    s.shutdown().await.expect("valid fixture");
    let restart = f.open().await;
    s.code(ID, "fixture-code".into())
        .await
        .expect("valid fixture");
    assert_eq!(child.writes.lock().expect("valid fixture").len(), 1);
    restart.shutdown().await.expect("valid fixture");
}
#[tokio::test]
async fn receipt_failpoints_before_and_after_code_intent_never_send() {
    for after in [false, true] {
        let f = Fixture::new();
        let s = f.open().await;
        s.start(ID.into(), None, None, None)
            .await
            .expect("valid fixture");
        let child = f.launcher.next().await;
        link(&f, &s, &child).await;
        f.receipts.after.store(after, Ordering::SeqCst);
        f.receipts.fail.store(true, Ordering::SeqCst);
        assert!(s.code(ID, "fixture-code".into()).await.is_err());
        assert!(child.writes.lock().expect("valid fixture").is_empty());
        s.shutdown().await.expect("valid fixture");
        f.receipts.fail.store(false, Ordering::SeqCst);
        let restart = f.open().await;
        assert_eq!(
            restart.status(ID).await.expect("valid fixture")["status"],
            "error"
        );
        assert!(child.writes.lock().expect("valid fixture").is_empty());
        restart.shutdown().await.expect("valid fixture");
    }
}
#[tokio::test]
async fn restart_adopts_registered_add_after_final_receipt_failure() {
    let f = Fixture::new();
    let s = f.open().await;
    s.start(ID.into(), None, None, None)
        .await
        .expect("valid fixture");
    let child = f.launcher.next().await;
    link(&f, &s, &child).await;
    // Simulate account publication committed, then the process died before ready receipt.
    let snapshot = f
        .receipts
        .load()
        .expect("valid fixture")
        .pop()
        .expect("valid fixture");
    let config = snapshot.record()["metadata"]["configDir"]
        .as_str()
        .expect("valid fixture");
    let metadata=studio_runtime_services::account_driver::claude::auth_metadata_from_output(br#"{"loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test","subscriptionType":"max"}"#,true);
    f.accounts.register_claude_verified(serde_json::json!({"configDir":config,"binaryPath":"/fixture/scripted","customModels":[]}).as_object().expect("valid fixture").clone(),"Claude Code",&f.directory,&metadata).expect("valid fixture");
    s.shutdown().await.expect("valid fixture");
    let restart = f.open().await;
    assert_eq!(
        restart.status(ID).await.expect("valid fixture")["status"],
        "ready"
    );
    assert!(std::path::Path::new(config).is_dir());
    restart.shutdown().await.expect("valid fixture");
}

#[cfg(unix)]
#[tokio::test]
async fn scripted_cli_uses_shared_stream_child_end_to_end() {
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Arc};
    use studio_runtime_services::claude_login::{
        process::StreamLauncher,
        service::{LoginConfig, LoginService},
    };
    let f = Fixture::new();
    let script = f.directory.join("scripted-login");
    std::fs::write(&script,br##"#!/bin/sh
if [ "${ANTHROPIC_API_KEY+x}" = x ]; then exit 9; fi
if [ "$2" = status ]; then
  printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test","subscriptionType":"max"}'
  printf '%s\n' 'fixture diagnostic' >&2
  exit 0
fi
printf '%s\n' 'https://claude.com/cai/oauth/authorize?fixture=1'
IFS= read -r fixture_code || exit 7
exit 0
"##).expect("valid fixture");
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
        .expect("valid fixture");
    let changed = f.changes.clone();
    let s = LoginService::open(
        LoginConfig {
            directory: f.receipts.file.directory().into(),
            program: script,
            cwd: f.directory.clone(),
            environment: BTreeMap::from([
                ("HOME".into(), f.directory.as_os_str().into()),
                ("ANTHROPIC_API_KEY".into(), "fixture-value".into()),
            ]),
        },
        f.accounts.clone(),
        Arc::new(StreamLauncher),
        f.receipts.clone(),
        f.chats.clone(),
        Arc::new(move || changed.notify_one()),
    )
    .await
    .expect("valid fixture");
    s.start(ID.into(), None, None, None)
        .await
        .expect("valid fixture");
    f.status(&s, ID, "pending").await;
    s.code(ID, "fixture-code".into())
        .await
        .expect("valid fixture");
    let ready = f.status(&s, ID, "ready").await;
    assert_eq!(ready["email"], "fixture@example.test");
    s.shutdown().await.expect("valid fixture");
}

#[cfg(unix)]
#[tokio::test]
async fn restarted_unknown_keeps_config_until_recorded_child_is_proven_gone() {
    use serde_json::json;
    use std::{collections::BTreeMap, sync::Arc, time::Duration};
    use studio_runtime_process::stream_child::{ChildSpec, StreamChild};
    use studio_runtime_services::claude_login::{
        LoginError, LoginSnapshot,
        process::StreamLauncher,
        service::{LoginConfig, LoginService},
    };
    let f = Fixture::new();
    let config = f.receipts.file.directory().join(format!("config-{ID}"));
    std::fs::create_dir(&config).expect("config");
    std::fs::write(config.join("keep"), "fixture data").expect("marker");
    let lock = f.receipts.file.directory().join(format!(
        "{}.lock",
        studio_runtime_store::sha256_hex(config.to_string_lossy().as_bytes())
    ));
    let lease = std::fs::File::create(lock).expect("lease");
    lease.try_lock().expect("lock");
    // The real child owns the only descriptor; its output is the synchronization barrier.
    let mut child = StreamChild::spawn(ChildSpec {
        program: "/bin/sh".into(),
        args: vec!["-c".into(), "printf ready; read line".into()],
        cwd: f.directory.clone(),
        environment: BTreeMap::new(),
        timeout: Duration::from_secs(30),
        inherited_lease: Some(lease),
    })
    .await
    .expect("child");
    assert!(child.next_event().await.is_some());
    let pid = child.pid().expect("pid");
    // Observe through the same bounded leaf used by production, without reading credentials.
    let observed = studio_runtime_process::stream_child::capture(ChildSpec {
        program: "/bin/ps".into(),
        args: vec![
            "-p".into(),
            pid.to_string().into(),
            "-o".into(),
            "lstart=".into(),
        ],
        cwd: "/".into(),
        environment: BTreeMap::from([("LC_ALL".into(), "C".into()), ("TZ".into(), "UTC".into())]),
        timeout: Duration::from_secs(2),
        inherited_lease: None,
    })
    .await
    .expect("identity");
    let start = std::str::from_utf8(&observed.stdout).expect("start").trim();
    let record = LoginSnapshot::from_record(json!({"receipt":{"requestId":ID,"accountKey":"claude-one","status":"pending"},"metadata":{"flow":"add","configDir":config,"inheritedLease":true,"child":{"pid":pid,"start":start}}})).expect("record");
    f.receipts.save(&record).expect("receipt");
    let service = LoginService::open(
        LoginConfig {
            directory: f.receipts.file.directory().into(),
            program: "/fixture/scripted".into(),
            cwd: f.directory.clone(),
            environment: BTreeMap::new(),
        },
        f.accounts.clone(),
        Arc::new(StreamLauncher),
        f.receipts.clone(),
        f.chats.clone(),
        Arc::new(|| {}),
    )
    .await
    .expect("open");
    assert_eq!(service.status(ID).await.expect("status")["status"], "error");
    assert!(config.join("keep").is_file());
    assert!(matches!(
        service
            .start(
                "987edcdc-0e86-4fdf-a1c6-1513517d3ad0".into(),
                Some("claude-one".into()),
                None,
                None
            )
            .await,
        Err(LoginError::Busy)
    ));
    child.close().await.expect("reap");
    service.status(ID).await.expect("settled");
    assert!(!config.exists());
    let receipt = f.receipts.load().expect("receipts").pop().expect("saved");
    assert_eq!(receipt.record()["metadata"]["outcomeUnknown"], false);
    assert!(!matches!(
        service
            .start(
                "987edcdc-0e86-4fdf-a1c6-1513517d3ad0".into(),
                Some("claude-one".into()),
                None,
                None
            )
            .await,
        Err(LoginError::Busy)
    ));
    service.shutdown().await.expect("shutdown");
}

#[cfg(unix)]
#[tokio::test]
async fn receipt_write_matrix_reopens_without_relaunch_or_config_loss() {
    use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Arc};
    use studio_runtime_services::claude_login::{
        process::StreamLauncher,
        service::{LoginConfig, LoginService},
    };
    let full = std::env::var_os("STUDIO_RUNTIME_CORPUS_FULL").is_some();
    // start, login identity, login close, probe identity, probe close, ready, chat repair.
    for boundary in 1..=7 {
        if !full && ![1, 6].contains(&boundary) {
            continue;
        }
        for after in [false, true] {
            let f = Fixture::new();
            let script = f.directory.join("instant-login");
            std::fs::write(&script,br##"#!/bin/sh
if [ "$2" = status ]; then
printf '%s\n' '{"loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test","subscriptionType":"max"}'
fi
exit 0
"##).expect("script");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                .expect("mode");
            f.receipts.fail_at.store(boundary, Ordering::SeqCst);
            f.receipts.after.store(after, Ordering::SeqCst);
            let changes = f.changes.clone();
            let service = LoginService::open(
                LoginConfig {
                    directory: f.receipts.file.directory().into(),
                    program: script,
                    cwd: f.directory.clone(),
                    environment: BTreeMap::new(),
                },
                f.accounts.clone(),
                Arc::new(StreamLauncher),
                f.receipts.clone(),
                f.chats.clone(),
                Arc::new(move || changes.notify_one()),
            )
            .await
            .expect("open");
            let started = service.start(ID.into(), None, None, None).await;
            if started.is_ok() {
                tokio::time::timeout(std::time::Duration::from_secs(10), async {
                    loop {
                        let changed = f.changes.notified();
                        let value = service.status(ID).await.expect("status");
                        if value["status"] == "error" || value["status"] == "ready" {
                            break;
                        }
                        changed.await;
                    }
                })
                .await
                .expect("actor settled");
            }
            service.shutdown().await.expect("close owned children");
            assert!(
                f.receipts.writes.load(Ordering::SeqCst) >= boundary,
                "{boundary}/{after}"
            );
            let restart = f.open().await;
            if boundary == 1 && !after {
                assert!(restart.status(ID).await.is_err());
            } else {
                let value = restart.status(ID).await.expect("reopened receipt");
                assert!(["error", "ready"].contains(&value["status"].as_str().expect("status")));
                if value["status"] == "ready" {
                    assert_eq!(value["email"], "fixture@example.test");
                }
            }
            assert!(
                f.launcher
                    .receiver
                    .try_lock()
                    .expect("script queue")
                    .try_recv()
                    .is_err(),
                "restart must not relaunch"
            );
            restart.shutdown().await.expect("shutdown");
        }
    }
}
