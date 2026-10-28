use super::*;
use crate::linux_vm_guest::GuestError;
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering};

/// A guest that serves files, `find` and the containment check from memory.
struct Guest {
    files: BTreeMap<String, Vec<u8>>,
    contained: AtomicBool,
    corrupt: AtomicBool,
    calls: Mutex<Vec<String>>,
}

impl Guest {
    fn calls(&self, method: &str) -> usize {
        self.calls
            .lock()
            .unwrap()
            .iter()
            .filter(|name| *name == method)
            .count()
    }
}

fn sha(data: &[u8]) -> String {
    data_encoding::HEXLOWER.encode(&Sha256::digest(data))
}

impl GuestRpc for Guest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        _: Option<&str>,
        _: Duration,
        _: &mut dyn FnMut(Value) -> std::result::Result<(), GuestError>,
    ) -> std::result::Result<Value, GuestError> {
        self.calls.lock().unwrap().push(method.to_owned());
        let path = params["path"].as_str().unwrap_or_default();
        Ok(match method {
            "layr.exec" => {
                let argv: Vec<&str> = params["argv"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .filter_map(Value::as_str)
                    .collect();
                if argv[0] == "sh" {
                    return Ok(
                        json!({"exitCode": i32::from(!self.contained.load(Ordering::SeqCst)), "stdout": ""}),
                    );
                }
                let (directory, pattern) = (argv[1], argv[argv.len() - 2]);
                let found: Vec<u8> = self
                    .files
                    .keys()
                    .filter(|file| {
                        let name = file.rsplit('/').next().unwrap();
                        file.starts_with(&format!("{directory}/"))
                            && match (pattern.strip_prefix('*'), pattern.strip_suffix(".*")) {
                                (Some(suffix), _) => name.ends_with(suffix),
                                (_, Some(prefix)) => name.starts_with(&format!("{prefix}.")),
                                _ => name == pattern,
                            }
                    })
                    .flat_map(|file| file.bytes().chain([0]))
                    .collect();
                json!({"exitCode":0,"stdout":BASE64.encode(found)})
            }
            "layr.file.stat" => {
                let data = &self.files[path];
                json!({"bytes":data.len(),"sha256":sha(data),"token":"t"})
            }
            "layr.file.read" => {
                let data = &self.files[path];
                let offset = params["offset"].as_u64().unwrap() as usize;
                let end = (offset + params["maxBytes"].as_u64().unwrap() as usize).min(data.len());
                let mut chunk = data[offset..end].to_vec();
                let proof = sha(&chunk);
                if self.corrupt.load(Ordering::SeqCst) {
                    chunk[0] ^= 1;
                }
                json!({"data":BASE64.encode(&chunk),"nextOffset":end,"sha256":proof})
            }
            other => panic!("unexpected guest method {other}"),
        })
    }
}

struct Files {
    needed: u64,
    brought: Mutex<u32>,
}

impl ProjectFiles for Files {
    fn needed_bytes(&self, _: &str) -> Result<u64> {
        Ok(self.needed)
    }
    fn bring(&self, _: &str) -> Result<()> {
        *self.brought.lock().unwrap() += 1;
        Ok(())
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    root: PathBuf,
    store: Arc<RuntimeStore>,
    host: Arc<Host>,
    guest: Arc<Guest>,
    files: Arc<Files>,
}

const NATIVE: &[u8] = b"{\"type\":\"user\"}\n{\"type\":\"assistant\"}\n";
const BRIDGE: &[u8] = b"{\"id\":\"thread-1\"}";
const ROLLOUT: &[u8] = b"{\"rollout\":1}\n";

fn records(project: &Path, status: &str) -> Vec<Value> {
    let lead = json!({"provider":"claude","id":"lead","rootId":"lead","epoch":3,"status":status,
        "autoWake":true,"isLead":true,"executionMode":"vm","workspaceMode":"layr",
        "layrReady":true,"layrHome":"/home/u","layrProjectId":"project","layrLine":"main",
        "layrStateId":"s1","environment":"linux","nativeRoleGuidance":"VM text",
        "cwd":"/var/lib/layr/projects/project/lines/main","hostProjectPath":project,
        "threadId":"thread-1","accountKey":"claude-account"});
    let worker = json!({"provider":"codex","id":"worker","rootId":"lead","parentId":"lead",
        "role":"implementer","epoch":1,"status":"idle","autoWake":false,"executionMode":"vm",
        "workspaceMode":"layr","layrReady":true,"layrHome":"/home/u","layrLine":"agent-1",
        "cwd":"/var/lib/layr/projects/project/lines/agent-1","threadId":"thread-2",
        "accountKey":"codex-account"});
    vec![lead, worker]
}

async fn fixture(status: &str, free: fn(&Path) -> Option<u64>, needed: u64) -> Fixture {
    let directory = tempfile::tempdir().unwrap();
    let root = directory.path().to_owned();
    let project = root.join("project");
    std::fs::create_dir(&project).unwrap();
    let state = root.join("state");
    std::fs::create_dir(&state).unwrap();
    let store = Arc::new(RuntimeStore::open(Some(&state)).await.unwrap());
    let rows = records(&project, status);
    store
        .transaction_with(move |tx| -> std::result::Result<(), StoreError> {
            for row in rows {
                AgentRepository::put(tx, &AgentRecord::from_value(row)?)?;
            }
            Ok(())
        })
        .await
        .unwrap();
    let digest = |key: &str| account_digest(key);
    let claude = format!(
        "/home/u/.claude/studio-accounts/{}",
        digest("claude-account")
    );
    let codex = format!("/home/u/.codex/studio-accounts/{}", digest("codex-account"));
    let files = BTreeMap::from([
        (
            format!("{claude}/projects/-var-lib-layr/thread-1.jsonl"),
            NATIVE.to_vec(),
        ),
        (
            format!("{claude}/projects/-var-lib-layr/other.jsonl"),
            b"other\n".to_vec(),
        ),
        (
            "/home/u/.codex/studio-bridges/lead/sessions/thread-1.json".to_owned(),
            BRIDGE.to_vec(),
        ),
        (
            format!("{codex}/sessions/2026/10/28/rollout-1-thread-2.jsonl"),
            ROLLOUT.to_vec(),
        ),
    ]);
    let guest = Arc::new(Guest {
        files,
        contained: AtomicBool::new(true),
        corrupt: AtomicBool::new(false),
        calls: Mutex::new(Vec::new()),
    });
    let files = Arc::new(Files {
        needed,
        brought: Mutex::new(0),
    });
    let accounts = vec![
        json!({"id":"claude-account","provider":"claude","claudeOptions":{"configDir":root.join("claude")}}),
        json!({"id":"codex-account","provider":"codex","home":root.join("codex")}),
    ];
    let host = Arc::new(Host {
        guest: guest.clone(),
        files: files.clone(),
        state_dir: state,
        accounts,
        free_bytes: free,
    });
    Fixture {
        _directory: directory,
        root,
        store,
        host,
        guest,
        files,
    }
}

fn plenty(_: &Path) -> Option<u64> {
    Some(u64::MAX)
}

async fn stored(store: &RuntimeStore, id: &str) -> Value {
    crate::layr_native::agent_value(store, id).await.unwrap()
}

#[tokio::test]
async fn refuses_a_chat_with_an_active_turn() {
    let f = fixture("running", plenty, 0).await;
    let error = move_to_host(&f.store, &f.host, "lead", "r1")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, MoveError::Refused(text) if text.contains("Finish the active")),
        "{error}"
    );
    assert_eq!(stored(&f.store, "lead").await["executionMode"], "vm");
    assert_eq!(f.guest.calls("layr.file.read"), 0);
}

#[tokio::test]
async fn refuses_when_the_disk_is_too_small_and_copies_nothing() {
    let f = fixture("idle", |_| Some(1000), 500).await;
    let error = move_to_host(&f.store, &f.host, "lead", "r1")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, MoveError::Refused(text) if text.contains("Not enough free space")),
        "{error}"
    );
    assert_eq!(*f.files.brought.lock().unwrap(), 0);
    assert_eq!(f.guest.calls("layr.file.read"), 0);
    assert_eq!(stored(&f.store, "lead").await["executionMode"], "vm");
}

#[tokio::test]
async fn refuses_a_worker_line_that_main_does_not_hold() {
    let f = fixture("idle", plenty, 0).await;
    f.guest.contained.store(false, Ordering::SeqCst);
    let error = move_to_host(&f.store, &f.host, "lead", "r1")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, MoveError::Refused(text) if text.contains("not in main")),
        "{error}"
    );
    assert_eq!(*f.files.brought.lock().unwrap(), 0);
}

#[tokio::test]
async fn copies_history_byte_for_byte_and_sets_the_host_fields() {
    let f = fixture("idle", plenty, 10).await;
    let result = move_to_host(&f.store, &f.host, "lead", "r1").await.unwrap();
    assert_eq!(result["movedAgents"], json!(["lead", "worker"]));
    assert_eq!(*f.files.brought.lock().unwrap(), 1);
    let claude = f.root.join("claude/projects/-var-lib-layr/thread-1.jsonl");
    assert_eq!(std::fs::read(&claude).unwrap(), NATIVE);
    assert!(
        !f.root
            .join("claude/projects/-var-lib-layr/other.jsonl")
            .exists()
    );
    let bridge = f
        .root
        .join("state/account-servers/claude-account/sessions/thread-1.json");
    assert_eq!(std::fs::read(&bridge).unwrap(), BRIDGE);
    let rollout = f
        .root
        .join("codex/sessions/2026/10/28/rollout-1-thread-2.jsonl");
    assert_eq!(sha(&std::fs::read(&rollout).unwrap()), sha(ROLLOUT));
    let project = f.root.join("project");
    for (id, thread) in [("lead", "thread-1"), ("worker", "thread-2")] {
        let record = stored(&f.store, id).await;
        assert_eq!(record["executionMode"], "native");
        assert_eq!(record["workspaceMode"], "shared");
        assert_eq!(record["cwd"], project.to_str().unwrap());
        assert_eq!(record["threadId"], thread);
        for key in VM_FIELDS {
            assert!(
                record.get(*key).is_none_or(Value::is_null),
                "{id} keeps {key}"
            );
        }
    }
    assert_eq!(stored(&f.store, "lead").await["id"], "lead");
}

#[tokio::test]
async fn a_corrupt_chunk_leaves_no_history_and_no_move() {
    let f = fixture("idle", plenty, 0).await;
    f.guest.corrupt.store(true, Ordering::SeqCst);
    let error = move_to_host(&f.store, &f.host, "lead", "r1")
        .await
        .unwrap_err();
    assert!(matches!(error, MoveError::Failed(_)), "{error}");
    assert!(
        !f.root
            .join("claude/projects/-var-lib-layr/thread-1.jsonl")
            .exists()
    );
    assert_eq!(stored(&f.store, "lead").await["executionMode"], "vm");
}

#[tokio::test]
async fn a_repeat_with_the_same_request_is_idempotent() {
    let f = fixture("idle", plenty, 10).await;
    let first = move_to_host(&f.store, &f.host, "lead", "r1").await.unwrap();
    let reads = f.guest.calls("layr.file.read");
    let again = move_to_host(&f.store, &f.host, "lead", "r1").await.unwrap();
    assert_eq!(first, again);
    assert_eq!(f.guest.calls("layr.file.read"), reads);
    assert_eq!(*f.files.brought.lock().unwrap(), 1);
    let other = move_to_host(&f.store, &f.host, "worker", "r1")
        .await
        .unwrap_err();
    assert!(matches!(other, MoveError::Refused(_)), "{other}");
}

#[tokio::test]
async fn a_repeat_after_a_lost_response_does_not_copy_twice() {
    // The first attempt copied the files but its records were never written: a new
    // request ID repeats the move, keeps the equal bytes and writes the records once.
    let f = fixture("idle", plenty, 0).await;
    let first = copy_everything(
        &f.host,
        &admit_members(&f).await,
        f.root.join("project").to_str().unwrap(),
        None,
        "tag-1234567890abcdef",
    );
    first.unwrap();
    let reads = f.guest.calls("layr.file.read");
    let result = move_to_host(&f.store, &f.host, "lead", "r2").await.unwrap();
    assert_eq!(result["movedAgents"], json!(["lead", "worker"]));
    assert_eq!(f.guest.calls("layr.file.read"), reads);
}

async fn admit_members(f: &Fixture) -> Vec<Value> {
    f.store
        .read_async("test/vm-to-host", |tx| {
            admit(tx, "lead").map(|plan| plan.members)
        })
        .await
        .unwrap()
}

#[tokio::test]
async fn different_history_on_this_device_is_never_replaced() {
    let f = fixture("idle", plenty, 0).await;
    let claude = f.root.join("claude/projects/-var-lib-layr");
    std::fs::create_dir_all(&claude).unwrap();
    std::fs::write(claude.join("thread-1.jsonl"), b"different\n").unwrap();
    let error = move_to_host(&f.store, &f.host, "lead", "r1")
        .await
        .unwrap_err();
    assert!(
        matches!(&error, MoveError::Refused(text) if text.contains("different history")),
        "{error}"
    );
    assert_eq!(
        std::fs::read(claude.join("thread-1.jsonl")).unwrap(),
        b"different\n"
    );
}
