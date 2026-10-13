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
    for reauth in [false, true] {
        for boundary in 1..=if reauth { 7 } else { 6 } {
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
                let account = if reauth {
                    let config = f.directory.join("reauth-config");
                    std::fs::create_dir(&config).expect("reauth config");
                    let metadata = studio_runtime_services::account_driver::claude::auth_metadata_from_output(br#"{"loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test","subscriptionType":"max"}"#,true);
                    Some(f.accounts.register_claude_verified(serde_json::json!({"configDir":config,"binaryPath":script,"customModels":[]}).as_object().expect("options").clone(),"Fixture",&f.directory,&metadata).expect("registered fixture"))
                } else {
                    None
                };
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
                let started = service.start(ID.into(), account, None, None).await;
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
                    .unwrap_or_else(|error| panic!("reauth={reauth}, boundary={boundary}, after={after}: actor did not settle: {error}; writes={}", f.receipts.writes.load(Ordering::SeqCst)));
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
                    // Observe the durable restore without invoking the deliberately forbidden
                    // credential reader in this fixture. A real caller revalidates chat repair.
                    let value = f
                        .receipts
                        .load()
                        .expect("restored receipts")
                        .pop()
                        .expect("restored receipt")
                        .receipt();
                    assert!(
                        ["error", "ready"].contains(&value["status"].as_str().expect("status"))
                    );
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
}

fn oracle(args: &[&str]) -> serde_json::Value {
    let output = std::process::Command::new(
        std::env::var_os("CODEX_AGENTS_PYTHON").expect("isolated Python"),
    )
    .args([
        "-B",
        concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/tests/claude_runtime/login_oracle.py"
        ),
    ])
    .args(args)
    .output()
    .expect("Python writer");
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    if output.stdout.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&output.stdout).expect("oracle")
    }
}

#[tokio::test]
async fn add_label_and_email_admission_match_python_before_any_write() {
    for case in oracle(&[])["admissions"].as_array().expect("admissions") {
        let f = Fixture::new();
        let registry = std::fs::read(f.directory.join("registry.json")).expect("registry");
        let s = f.open().await;
        let result = s
            .start(
                ID.into(),
                None,
                case["email"].as_str().map(str::to_owned),
                case["label"].as_str().map(str::to_owned),
            )
            .await;
        assert_eq!(
            result.is_ok(),
            case["admitted"].as_bool().expect("admitted"),
            "{case}"
        );
        if result.is_ok() {
            let saved = f.receipts.load().expect("receipt").pop().expect("saved");
            assert_eq!(
                saved.record()["metadata"]["label"],
                case["label"]
                    .as_str()
                    .map(str::trim)
                    .unwrap_or("Claude Code")
            );
            assert_eq!(
                saved.record()["metadata"]["emailHint"].as_str(),
                case["email"].as_str().map(str::trim)
            );
            s.cancel(ID).await.expect("cancel");
        } else {
            assert!(f.receipts.load().expect("receipts").is_empty());
            assert!(
                !f.receipts
                    .file
                    .directory()
                    .join(format!("config-{ID}"))
                    .exists()
            );
        }
        s.shutdown().await.expect("shutdown");
        assert_eq!(
            std::fs::read(f.directory.join("registry.json")).expect("registry"),
            registry
        );
    }
}

#[tokio::test]
async fn python_legacy_reauth_resolves_free_lease_or_explicit_exact_cancel() {
    use serde_json::json;
    use studio_runtime_services::claude_login::LoginError;
    const OTHER: &str = "987edcdc-0e86-4fdf-a1c6-1513517d3ad0";
    for explicit in [false, true] {
        let f = Fixture::new();
        let config = f.directory.join("reauth-config");
        std::fs::create_dir(&config).expect("config");
        std::fs::write(config.join("keep"), "fixture data").expect("marker");
        let metadata = studio_runtime_services::account_driver::claude::auth_metadata_from_output(br#"{"loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test","subscriptionType":"max"}"#, true);
        let key = f
            .accounts
            .register_claude_verified(
                json!({"configDir":config,"binaryPath":"/fixture/scripted","customModels":[]})
                    .as_object()
                    .expect("options")
                    .clone(),
                "Fixture",
                &f.directory,
                &metadata,
            )
            .expect("account");
        oracle(&["legacy", f.directory.to_str().expect("path"), &key]);
        let original = f.receipts.load().expect("Python receipts");
        assert_eq!(original.len(), 2);
        assert!(original.iter().all(|s| s.record()["metadata"] == json!({})));
        let registry = std::fs::read(f.directory.join("registry.json")).expect("registry");
        let lease = std::fs::File::create(f.receipts.file.directory().join(format!(
            "{}.lock",
            studio_runtime_store::sha256_hex(config.to_string_lossy().as_bytes())
        )))
        .expect("lease");
        lease.try_lock().expect("locked");
        let service = f.open().await;
        let held = service.status(ID).await.expect("status");
        assert_eq!(held["status"], "error");
        assert!(
            held["error"]
                .as_str()
                .expect("visible hold")
                .contains("Cancel this request")
        );
        assert!(matches!(
            service
                .start(
                    "80c4d37a-0823-488b-96d8-e30d52e28d18".into(),
                    Some(key.clone()),
                    None,
                    None
                )
                .await,
            Err(LoginError::Busy)
        ));
        if explicit {
            let before_other = f
                .receipts
                .load()
                .expect("receipts")
                .into_iter()
                .find(|s| s.request_id() == OTHER)
                .expect("other")
                .record()
                .clone();
            assert_eq!(
                service.cancel(ID).await.expect("explicit cancel")["status"],
                "cancelled"
            );
            let records = f.receipts.load().expect("receipts");
            let cleared = records
                .iter()
                .find(|s| s.request_id() == ID)
                .expect("cleared");
            assert_eq!(cleared.record()["metadata"]["outcomeUnknown"], false);
            assert_eq!(cleared.record()["metadata"]["operatorCleared"], true);
            assert_eq!(
                records
                    .iter()
                    .find(|s| s.request_id() == OTHER)
                    .expect("other")
                    .record(),
                &before_other
            );
            assert_eq!(
                service.cancel(ID).await.expect("idempotent cancel")["status"],
                "cancelled"
            );
        }
        drop(lease);
        service.status(ID).await.expect("free lease settlement");
        assert!(
            f.receipts
                .load()
                .expect("receipts")
                .iter()
                .all(|s| s.record()["metadata"]["outcomeUnknown"] == false)
        );
        assert!(config.join("keep").is_file());
        assert_eq!(
            std::fs::read(f.directory.join("registry.json")).expect("registry"),
            registry
        );
        assert!(f.launcher.receiver.lock().await.try_recv().is_err());
        service.shutdown().await.expect("shutdown");
        let restart = f.open().await;
        assert_eq!(
            restart
                .start(ID.into(), Some(key.clone()), None, None)
                .await
                .expect("old id replays")["status"],
            if explicit { "cancelled" } else { "error" }
        );
        assert!(f.launcher.receiver.lock().await.try_recv().is_err());
        let new_id = "80c4d37a-0823-488b-96d8-e30d52e28d18";
        restart
            .start(new_id.into(), Some(key), None, None)
            .await
            .expect("free lease admits a new identity");
        restart.cancel(new_id).await.expect("cancel new request");
        restart.shutdown().await.expect("shutdown");
    }
}

#[cfg(unix)]
#[path = "claude_runtime/login_failures.rs"]
mod failures;

#[tokio::test]
async fn unknown_receipt_cancel_write_failures_reopen_without_resend() {
    for after in [false, true] {
        let f = Fixture::new();
        // An unavailable legacy account prevents automatic lease proof.
        oracle(&[
            "legacy",
            f.directory.to_str().expect("path"),
            "removed-account",
        ]);
        let service = f.open().await;
        f.receipts.fail.store(true, Ordering::SeqCst);
        f.receipts.after.store(after, Ordering::SeqCst);
        assert!(service.cancel(ID).await.is_err());
        service.shutdown().await.expect("shutdown");
        f.receipts.fail.store(false, Ordering::SeqCst);
        let restarted = f.open().await;
        let saved = restarted.status(ID).await.expect("status");
        assert_eq!(saved["status"], if after { "cancelled" } else { "error" });
        restarted.cancel(ID).await.expect("resolve exact receipt");
        restarted
            .code(ID, "fixture-code".into())
            .await
            .expect("terminal code is not retried");
        assert!(f.launcher.receiver.lock().await.try_recv().is_err());
        assert_eq!(
            restarted
                .status("987edcdc-0e86-4fdf-a1c6-1513517d3ad0")
                .await
                .expect("other receipt")["status"],
            "error"
        );
        restarted.shutdown().await.expect("shutdown");
    }
}
