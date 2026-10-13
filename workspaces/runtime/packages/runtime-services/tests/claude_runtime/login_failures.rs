//! Real harmless scripts at the shared child boundary; barriers replace sleeps.
use super::fixture::*;
use std::{collections::BTreeMap, os::unix::fs::PermissionsExt, sync::Arc, time::Duration};
use studio_runtime_services::claude_login::{
    LoginError,
    ports::*,
    process::StreamLauncher,
    service::{LoginConfig, LoginService},
};
use tokio::sync::mpsc;

struct ObservedLauncher(mpsc::Sender<bool>);
struct ObservedChild {
    child: Box<dyn LoginChild>,
    stage: bool,
    observed: Option<mpsc::Sender<bool>>,
}
impl LoginLauncher for ObservedLauncher {
    fn spawn(&self, command: LoginCommand) -> FutureResult<'_, Box<dyn LoginChild>> {
        Box::pin(async move {
            let stage = command.args.get(1).is_some_and(|arg| arg == "status");
            let child = StreamLauncher.spawn(command).await?;
            Ok(Box::new(ObservedChild {
                child,
                stage,
                observed: Some(self.0.clone()),
            }) as Box<dyn LoginChild>)
        })
    }
}
impl LoginChild for ObservedChild {
    fn identity(&self) -> FutureResult<'_, Option<ChildIdentity>> {
        self.child.identity()
    }
    fn next_event(&mut self) -> FutureResult<'_, Option<LoginEvent>> {
        Box::pin(async move {
            let event = self.child.next_event().await?;
            if matches!(
                event,
                Some(LoginEvent::Output(_) | LoginEvent::Diagnostic(_))
            ) && let Some(sender) = self.observed.take()
            {
                sender
                    .try_send(self.stage)
                    .map_err(|_| LoginError::Unavailable)?;
            }
            Ok(event)
        })
    }
    fn write(&self, bytes: Vec<u8>) -> FutureResult<'_, ()> {
        self.child.write(bytes)
    }
    fn close(self: Box<Self>) -> FutureResult<'static, ()> {
        self.child.close()
    }
}

#[tokio::test]
async fn scripted_login_and_probe_exit_hang_and_cancel_settle_without_account_writes() {
    for probe in [false, true] {
        for action in ["exit", "hang", "cancel"] {
            let f = Fixture::new();
            let registry = std::fs::read(f.directory.join("registry.json")).expect("registry");
            let script = f.directory.join("failure-cli");
            // Login emits a real URL barrier. Probe emits a diagnostic barrier only.
            // A read on piped stdin blocks without synthetic load or wall-clock sleeps.
            std::fs::write(
                &script,
                format!(
                    r##"#!/bin/sh
if [ "$2" = login ]; then
  if [ "{probe}" = true ]; then exit 0; fi
  printf '%s\n' '{URL}'
else
  printf '%s\n' 'probe barrier' >&2
  if [ "{probe}" = false ]; then exit 7; fi
fi
if [ "{action}" = exit ]; then exit 7; fi
IFS= read -r fixture_line
exit 7
"##
                ),
            )
            .expect("script");
            std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o700))
                .expect("mode");
            let (sender, mut barriers) = mpsc::channel(4);
            let changes = f.changes.clone();
            let service = LoginService::open(
                LoginConfig {
                    directory: f.receipts.file.directory().into(),
                    program: script,
                    cwd: f.directory.clone(),
                    environment: BTreeMap::new(),
                },
                f.accounts.clone(),
                Arc::new(ObservedLauncher(sender)),
                f.receipts.clone(),
                f.chats.clone(),
                Arc::new(move || changes.notify_one()),
            )
            .await
            .expect("open");
            service
                .start(ID.into(), None, None, None)
                .await
                .expect("start");
            assert_eq!(barriers.recv().await.expect("script barrier"), probe);
            if action == "cancel" {
                assert_eq!(
                    service.cancel(ID).await.expect("cancel")["status"],
                    "cancelled"
                );
            } else if action == "hang" {
                // The output barrier proves the actor armed this stage's deadline.
                // Advance only virtual service time; external processes remain ordinary.
                tokio::time::pause();
                tokio::time::advance(Duration::from_secs(if probe { 16 } else { 601 })).await;
                tokio::time::resume();
            }
            let expected = if action == "cancel" {
                "cancelled"
            } else {
                "error"
            };
            f.status(&service, ID, expected).await;
            service.shutdown().await.expect("owned actor reaped");
            let saved = f.receipts.load().expect("receipts").pop().expect("receipt");
            assert_eq!(
                saved.receipt()["status"],
                expected,
                "probe={probe}, action={action}"
            );
            assert_ne!(saved.record()["metadata"]["outcomeUnknown"], true);
            assert!(saved.record()["metadata"]["child"].is_null());
            assert_eq!(saved.record()["metadata"]["cleanupDone"], true);
            assert!(
                !f.receipts
                    .file
                    .directory()
                    .join(format!("config-{ID}"))
                    .exists()
            );
            assert_eq!(
                std::fs::read(f.directory.join("registry.json")).expect("registry"),
                registry
            );
            let config = saved.record()["metadata"]["configDir"]
                .as_str()
                .expect("config");
            let lease = std::fs::File::open(f.receipts.file.directory().join(format!(
                "{}.lock",
                studio_runtime_store::sha256_hex(config.as_bytes())
            )))
            .expect("lease");
            lease.try_lock().expect("lease released after reap");
            let restarted = f.open().await;
            assert_eq!(
                restarted.status(ID).await.expect("durable terminal")["status"],
                expected
            );
            assert!(f.launcher.receiver.lock().await.try_recv().is_err());
            restarted.shutdown().await.expect("shutdown");
        }
    }
}

#[tokio::test]
async fn cancellation_before_launch_has_no_child_or_account_write() {
    let f = Fixture::new();
    let registry = std::fs::read(f.directory.join("registry.json")).expect("registry");
    let service = f.open().await;
    service
        .start(ID.into(), None, None, None)
        .await
        .expect("start");
    service.cancel(ID).await.expect("cancel before actor runs");
    service.shutdown().await.expect("shutdown");
    assert!(f.launcher.receiver.lock().await.try_recv().is_err());
    let saved = f.receipts.load().expect("receipt").pop().expect("receipt");
    assert_eq!(saved.receipt()["status"], "cancelled");
    assert_eq!(saved.record()["metadata"]["cleanupDone"], true);
    assert_eq!(
        std::fs::read(f.directory.join("registry.json")).expect("registry"),
        registry
    );
}
