#![allow(dead_code)]
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};
use studio_runtime_services::{
    account_driver::{AccountDriver, AuthMetadata, AuthProfile, ProviderAuthReader},
    claude_login::{
        LoginError, LoginSnapshot,
        ports::*,
        receipts::FileReceipts,
        service::{LoginConfig, LoginService},
    },
};
use tokio::sync::{Notify, mpsc};
pub struct NoProbe;
impl ProviderAuthReader for NoProbe {
    fn read(&self, _: &AuthProfile) -> AuthMetadata {
        panic!("no credentials or real provider probes")
    }
}
pub struct Chats(pub AtomicUsize);
impl LoginChats for Chats {
    fn refresh(&self, _: &str) -> Result<(), LoginError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Ok(())
    }
}
pub struct Receipts {
    pub file: FileReceipts,
    pub fail: AtomicBool,
    pub after: AtomicBool,
    pub writes: AtomicUsize,
    pub fail_at: AtomicUsize,
}
impl LoginReceipts for Receipts {
    fn load(&self) -> Result<Vec<LoginSnapshot>, LoginError> {
        self.file.load()
    }
    fn save(&self, s: &LoginSnapshot) -> Result<(), LoginError> {
        let ordinal = self.writes.fetch_add(1, Ordering::SeqCst) + 1;
        let fail =
            self.fail.load(Ordering::SeqCst) || ordinal == self.fail_at.load(Ordering::SeqCst);
        if fail && !self.after.load(Ordering::SeqCst) {
            return Err(LoginError::Persistence);
        }
        self.file.save(s)?;
        if fail {
            return Err(LoginError::Persistence);
        }
        Ok(())
    }
}
pub struct Launches {
    pub receiver: tokio::sync::Mutex<mpsc::Receiver<Script>>,
    sender: mpsc::Sender<Script>,
}
pub struct Script {
    pub command: LoginCommand,
    pub output: mpsc::Sender<LoginEvent>,
    pub writes: Arc<Mutex<Vec<Vec<u8>>>>,
    pub closed: Arc<AtomicBool>,
    pub write_unknown: Arc<AtomicBool>,
}
struct Child {
    lease: Option<std::fs::File>,
    events: mpsc::Receiver<LoginEvent>,
    writes: Arc<Mutex<Vec<Vec<u8>>>>,
    closed: Arc<AtomicBool>,
    write_unknown: Arc<AtomicBool>,
}
impl Launches {
    pub fn new() -> Arc<Self> {
        let (sender, receiver) = mpsc::channel(8);
        Arc::new(Self {
            sender,
            receiver: tokio::sync::Mutex::new(receiver),
        })
    }
    pub async fn next(&self) -> Script {
        tokio::time::timeout(
            std::time::Duration::from_secs(10),
            self.receiver.lock().await.recv(),
        )
        .await
        .expect("valid fixture")
        .expect("valid fixture")
    }
}
impl LoginLauncher for Launches {
    fn spawn(&self, mut command: LoginCommand) -> FutureResult<'_, Box<dyn LoginChild>> {
        Box::pin(async move {
            let lease = command.inherited_lease.take();
            let (output, events) = mpsc::channel(8);
            let writes = Arc::new(Mutex::new(vec![]));
            let closed = Arc::new(AtomicBool::new(false));
            let write_unknown = Arc::new(AtomicBool::new(false));
            self.sender
                .send(Script {
                    command,
                    output,
                    writes: writes.clone(),
                    closed: closed.clone(),
                    write_unknown: write_unknown.clone(),
                })
                .await
                .map_err(|_| LoginError::Unavailable)?;
            Ok(Box::new(Child {
                lease,
                events,
                writes,
                closed,
                write_unknown,
            }) as Box<dyn LoginChild>)
        })
    }
}
impl LoginChild for Child {
    fn next_event(&mut self) -> FutureResult<'_, Option<LoginEvent>> {
        Box::pin(async move { Ok(self.events.recv().await) })
    }
    fn write(&self, bytes: Vec<u8>) -> FutureResult<'_, ()> {
        Box::pin(async move {
            self.writes.lock().expect("valid fixture").push(bytes);
            if self.write_unknown.load(Ordering::SeqCst) {
                Err(LoginError::Unavailable)
            } else {
                Ok(())
            }
        })
    }
    fn close(self: Box<Self>) -> FutureResult<'static, ()> {
        Box::pin(async move {
            self.closed.store(true, Ordering::SeqCst);
            drop(self.lease);
            Ok(())
        })
    }
}
pub struct Fixture {
    pub directory: PathBuf,
    pub accounts: Arc<AccountDriver<NoProbe>>,
    pub receipts: Arc<Receipts>,
    pub chats: Arc<Chats>,
    pub launcher: Arc<Launches>,
    pub changes: Arc<Notify>,
}
impl Fixture {
    pub fn new() -> Self {
        let directory = std::env::temp_dir().join(format!(
            "claude-fixture-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("valid fixture")
                .as_nanos()
        ));
        std::fs::create_dir(&directory).expect("valid fixture");
        let registry = directory.join("registry.json");
        std::fs::write(&registry,serde_json::to_vec(&json!({"version":1,"logins":{},"accounts":{"default":{"id":"default","home":"/fixture/codex","label":"Codex","source":"Local profile","status":"signedOut"}},"defaultAccountKey":"default"})).expect("valid fixture")).expect("valid fixture");
        Self {
            accounts: Arc::new(
                AccountDriver::open_with_reader(registry, NoProbe).expect("valid fixture"),
            ),
            receipts: Arc::new(Receipts {
                file: FileReceipts::new(directory.join("claude-logins")).expect("valid fixture"),
                fail: AtomicBool::new(false),
                after: AtomicBool::new(false),
                writes: AtomicUsize::new(0),
                fail_at: AtomicUsize::new(0),
            }),
            chats: Arc::new(Chats(AtomicUsize::new(0))),
            launcher: Launches::new(),
            changes: Arc::new(Notify::new()),
            directory,
        }
    }
    pub async fn open(&self) -> Arc<LoginService> {
        let changes = self.changes.clone();
        LoginService::open(
            LoginConfig {
                directory: self.receipts.file.directory().to_owned(),
                program: "/fixture/scripted".into(),
                cwd: self.directory.clone(),
                environment: BTreeMap::from([
                    ("HOME".into(), self.directory.clone().into_os_string()),
                    ("ANTHROPIC_API_KEY".into(), "fixture-not-a-secret".into()),
                ]),
            },
            self.accounts.clone(),
            self.launcher.clone(),
            self.receipts.clone(),
            self.chats.clone(),
            Arc::new(move || changes.notify_one()),
        )
        .await
        .expect("valid fixture")
    }
    pub async fn status(&self, service: &LoginService, id: &str, wanted: &str) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let notified = self.changes.notified();
                let value = service.status(id).await.expect("valid fixture");
                if value["status"] == wanted {
                    return value;
                }
                notified.await;
            }
        })
        .await
        .expect("valid fixture")
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        drop(std::fs::remove_dir_all(&self.directory));
    }
}
pub const ID: &str = "e0166868-592a-4dca-8994-04d34a730af5";
pub const URL: &str = "https://claude.com/cai/oauth/authorize?fixture=1";
pub async fn link(f: &Fixture, s: &LoginService, child: &Script) {
    child
        .output
        .send(LoginEvent::Output(format!("{URL}\n").into_bytes()))
        .await
        .expect("valid fixture");
    f.status(s, ID, "pending").await;
}
pub async fn complete(f: &Fixture, s: &LoginService, child: &Script) -> Value {
    child
        .output
        .send(LoginEvent::Exited(true))
        .await
        .expect("valid fixture");
    let probe = f.launcher.next().await;
    assert_eq!(probe.command.args, vec!["auth", "status", "--json"]);
    probe
        .output
        .send(LoginEvent::Diagnostic(b"ignored diagnostic".to_vec()))
        .await
        .expect("valid fixture");
    probe.output.send(LoginEvent::Output(br#"{"loggedIn":true,"authMethod":"claude.ai","email":"fixture@example.test","subscriptionType":"max"}"#.to_vec())).await.expect("valid fixture");
    probe
        .output
        .send(LoginEvent::Exited(true))
        .await
        .expect("valid fixture");
    f.status(s, ID, "ready").await
}
