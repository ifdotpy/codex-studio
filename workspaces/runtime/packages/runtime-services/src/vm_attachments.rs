//! Transport copies of authorized attachments for one exact VM actor.

use std::{collections::BTreeMap, io::Read, path::Path, sync::Arc, time::Duration};

use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use serde_json::{Value, json};
use studio_runtime_store::{
    AgentId, AgentRepository, RuntimeStore, WorkspaceFilesRepository, sha256_hex,
};

use crate::{
    account_worker::WorkerError,
    linux_vm_guest::GuestRpc,
    scheduler::TurnIntent,
    turn_prepare::{Asset, AssetPath, PreparationSnapshot, asset_fingerprint},
};

/// Raw bytes in one layr.exec stdin. The maintained guest permits at most 1 MiB.
const CHUNK_BYTES: usize = 1024 * 1024;
/// Guest supervised wait plus five seconds for response collection. The parity
/// witness below checks both guest budgets in layr_agents.py::dispatch.
const EXEC_WAIT: Duration = Duration::from_secs(315);

// Run as the exact line owner through layr.exec, never as the guest broker.
// Every open stays below a no-follow directory descriptor. Chunk identities
// include their bytes; finalization checks the complete source hash again.
const COPY: &str = r#"import hashlib,os,stat,sys,uuid
home,namespace,key,name,action,total,digest,chunk_limit,*chunks=sys.argv[1:]
assert os.path.isabs(home) and os.path.basename(name)==name and name not in ('.','..')
flags=os.O_RDONLY|os.O_DIRECTORY|os.O_NOFOLLOW
fd=os.open(home,flags)
assert os.fstat(fd).st_uid==os.geteuid()
def private(parent,name):
    try: os.mkdir(name,0o700,dir_fd=parent)
    except FileExistsError: pass
    child=os.open(name,flags,dir_fd=parent)
    info=os.fstat(child)
    assert info.st_uid==os.geteuid() and stat.S_IMODE(info.st_mode)&0o077==0
    return child
for part in ('.studio-attachments',namespace,key):
    next_fd=private(fd,part); os.close(fd); fd=next_fd
chunk_fd=private(fd,'chunks')
payload_fd=private(fd,'payload')
os.close(fd)
fd=chunk_fd
def read(name,limit):
    source=os.open(name,os.O_RDONLY|os.O_NOFOLLOW|os.O_NONBLOCK,dir_fd=fd)
    with os.fdopen(source,'rb') as stream:
        info=os.fstat(stream.fileno())
        assert stat.S_ISREG(info.st_mode) and info.st_size<=limit
        return stream.read(limit+1)
def put(name,data):
    try: existing=read(name,len(data))
    except FileNotFoundError: existing=None
    if existing is not None:
        assert existing==data
        return
    temporary='.tmp.'+uuid.uuid4().hex
    output=os.open(temporary,os.O_WRONLY|os.O_CREAT|os.O_EXCL|os.O_NOFOLLOW,0o600,dir_fd=fd)
    try:
        with os.fdopen(output,'wb') as stream:
            stream.write(data); stream.flush(); os.fsync(stream.fileno())
        try: os.link(temporary,name,src_dir_fd=fd,dst_dir_fd=fd,follow_symlinks=False)
        except FileExistsError: assert read(name,len(data))==data
    finally: os.unlink(temporary,dir_fd=fd)
    os.fsync(fd)
if action=='chunk':
    data=sys.stdin.buffer.read(int(chunk_limit)+1)
    assert len(data)<=int(chunk_limit) and hashlib.sha256(data).hexdigest()==chunks[1]
    put(chunks[0]+'.'+chunks[1]+'.part',data)
else:
    assert action=='finish'
    parts=[read(str(i)+'.'+value+'.part',int(chunk_limit)) for i,value in enumerate(chunks)]
    data=b''.join(parts)
    assert len(data)==int(total) and hashlib.sha256(data).hexdigest()==digest
    fd=payload_fd
    put(name,data)
os.close(chunk_fd)
os.close(payload_fd)
"#;

fn refused() -> WorkerError {
    WorkerError::PreparationRefused(
        "VM attachment identity, content or guest copy is unconfirmed".into(),
    )
}

fn binding(record: &Value) -> Value {
    json!([
        record["id"],
        record["epoch"],
        record["rootId"],
        record["accountKey"],
        record["provider"],
        record["executionMode"],
        record["layrReady"],
        record["layrHome"],
        record["layrProjectId"],
        record["layrLine"],
        record["cwd"],
        record["deletedAt"],
        record["deleted"]
    ])
}

async fn current(
    store: &RuntimeStore,
    intent: &TurnIntent,
    snapshot: &PreparationSnapshot,
    expected: Option<Value>,
) -> Result<Value, WorkerError> {
    let intent = intent.clone();
    let snapshot = snapshot.clone();
    let root = store.root().to_path_buf();
    store
        .read_async("runtime-services/vm-attachment-identity", move |tx| {
            if !crate::linux_vm_provider::vm_actor_matches(
                tx,
                &intent.agent_id,
                intent.epoch,
                &intent.account_key,
                Some(intent.provider.key()),
            )? {
                return Err(refused());
            }
            let record = AgentRepository::get(tx, &AgentId::from(intent.agent_id.as_str()))?
                .ok_or_else(refused)?
                .value();
            if record["layrReady"] != true
                || record["rootId"] != snapshot.actor.root_id
                || snapshot.actor.id != intent.agent_id
                || expected.is_some_and(|value| value != binding(&record))
            {
                return Err(refused());
            }
            for asset in &snapshot.assets {
                let saved = WorkspaceFilesRepository::asset(tx, &asset.id)?.ok_or_else(refused)?;
                if !crate::payload_store::valid_asset_record(saved.value(), &asset.id, &root)
                    || saved.value() != &Value::Object(asset.view.clone())
                    || saved.value()["path"] != asset.path
                    || saved.value()["name"] != asset.name
                    || saved.value()["size"] != asset.size
                    || saved.value()["image"] != asset.image
                    || asset.owner_deleted
                    || (asset.owner_root_id != snapshot.actor.root_id
                        && snapshot.actor.forked_from.as_ref() != Some(&asset.owner_id))
                {
                    return Err(refused());
                }
                let owner = AgentRepository::get(tx, &AgentId::from(asset.owner_id.as_str()))?
                    .ok_or_else(refused)?;
                let value = owner.value();
                if value["rootId"] != asset.owner_root_id
                    || !value["deletedAt"].is_null()
                    || value["deleted"] == true
                    || saved.value()["agent"] != asset.owner_id
                {
                    return Err(refused());
                }
            }
            Ok(record)
        })
        .await
}

async fn call(
    guest: Arc<dyn GuestRpc>,
    method: &'static str,
    params: Value,
    id: Option<String>,
) -> Result<Value, WorkerError> {
    crate::host_exec::on_guest_thread(move || guest.call(method, &params, id.as_deref(), EXEC_WAIT))
        .await
        .map_err(|_| refused())?
        .map_err(|_| refused())
}

fn source(root: &Path, asset: &Asset) -> Result<Vec<u8>, WorkerError> {
    use std::fs;
    for path in [
        root.join("uploads"),
        root.join("uploads").join(&asset.id),
        Path::new(&asset.path).to_path_buf(),
    ] {
        let metadata = fs::symlink_metadata(path).map_err(|_| refused())?;
        if metadata.file_type().is_symlink() {
            return Err(refused());
        }
    }
    let mut options = fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(nix::libc::O_NOFOLLOW | nix::libc::O_NONBLOCK);
    }
    let file = options.open(&asset.path).map_err(|_| refused())?;
    let info = file.metadata().map_err(|_| refused())?;
    if !info.is_file() || info.len() != asset.size {
        return Err(refused());
    }
    let mut bytes = Vec::new();
    file.take(asset.size + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| refused())?;
    if bytes.len() as u64 != asset.size
        || !asset.view["hash"]
            .as_str()
            .is_some_and(|hash| sha256_hex(&bytes).eq_ignore_ascii_case(hash))
    {
        return Err(refused());
    }
    Ok(bytes)
}

fn context_matches(context: &Value, record: &Value) -> bool {
    ["agentId", "home", "projectId", "line", "cwd"]
        .into_iter()
        .zip(["id", "layrHome", "layrProjectId", "layrLine", "cwd"])
        .all(|(guest, host)| {
            context[guest]
                .as_str()
                .is_some_and(|value| !value.is_empty())
                && context[guest] == record[host]
        })
        && context["home"].as_str().is_some_and(|home| {
            Path::new(home).is_absolute()
                && !Path::new(home)
                    .components()
                    .any(|part| matches!(part, std::path::Component::ParentDir))
        })
}

/// Stage only already-authorized snapshot assets. No provider command runs here.
/// Lost copy replies reuse exact guest request identities; native input remains unsent.
pub(crate) async fn prepare(
    store: &Arc<RuntimeStore>,
    guest: Arc<dyn GuestRpc>,
    intent: &TurnIntent,
    snapshot: &PreparationSnapshot,
) -> Result<BTreeMap<String, AssetPath>, WorkerError> {
    let record = current(store, intent, snapshot, None).await?;
    let expected = binding(&record);
    let context = call(
        Arc::clone(&guest),
        "agent.context",
        json!({"agentId":intent.agent_id}),
        None,
    )
    .await?;
    if !context_matches(&context, &record) {
        return Err(refused());
    }
    let home = context["home"].as_str().ok_or_else(refused)?;
    let namespace = sha256_hex(&serde_json::to_vec(&expected).map_err(|_| refused())?);
    let mut paths = BTreeMap::new();
    for asset in &snapshot.assets {
        let root = store.root().to_path_buf();
        let owned = asset.clone();
        let bytes = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/vm-attachment-read",
            move || source(&root, &owned),
        )
        .await
        .map_err(|_| refused())??;
        let fingerprint = asset_fingerprint(asset).map_err(|_| refused())?;
        let digest = sha256_hex(&bytes);
        let path = Path::new(home)
            .join(".studio-attachments")
            .join(&namespace)
            .join(&fingerprint)
            .join("payload")
            .join(&asset.name);
        let hashes: Vec<String> = bytes.chunks(CHUNK_BYTES).map(sha256_hex).collect();
        let base = vec![
            "/usr/bin/python3".to_owned(),
            "-I".into(),
            "-S".into(),
            "-c".into(),
            COPY.into(),
            home.into(),
            namespace.clone(),
            fingerprint.clone(),
            asset.name.clone(),
        ];
        for (index, chunk) in bytes.chunks(CHUNK_BYTES).enumerate() {
            current(store, intent, snapshot, Some(expected.clone())).await?;
            let mut argv = base.clone();
            argv.extend([
                "chunk".into(),
                asset.size.to_string(),
                digest.clone(),
                CHUNK_BYTES.to_string(),
                index.to_string(),
                hashes[index].clone(),
            ]);
            let id = format!(
                "vm-attachment:{}",
                sha256_hex(&serde_json::to_vec(&argv).map_err(|_| refused())?)
            );
            let result = call(
                Arc::clone(&guest),
                "layr.exec",
                json!({"agentId":intent.agent_id,"argv":argv,"stdin":BASE64.encode(chunk)}),
                Some(id),
            )
            .await?;
            if result["exitCode"] != 0 || result["reason"] != "exited" {
                return Err(refused());
            }
        }
        current(store, intent, snapshot, Some(expected.clone())).await?;
        let mut argv = base;
        argv.extend([
            "finish".into(),
            asset.size.to_string(),
            digest.clone(),
            CHUNK_BYTES.to_string(),
        ]);
        argv.extend(hashes);
        let id = format!(
            "vm-attachment:{}",
            sha256_hex(&serde_json::to_vec(&argv).map_err(|_| refused())?)
        );
        let result = call(
            Arc::clone(&guest),
            "layr.exec",
            json!({"agentId":intent.agent_id,"argv":argv}),
            Some(id),
        )
        .await?;
        if result["exitCode"] != 0 || result["reason"] != "exited" {
            return Err(refused());
        }
        let proof = call(
            Arc::clone(&guest),
            "layr.file.stat",
            json!({"agentId":intent.agent_id,"path":path}),
            None,
        )
        .await?;
        if proof["path"] != path.to_string_lossy().as_ref()
            || proof["bytes"] != asset.size
            || proof["sha256"] != digest
        {
            return Err(refused());
        }
        paths.insert(
            asset.id.clone(),
            AssetPath {
                source_fingerprint: fingerprint,
                path: path.to_string_lossy().into_owned(),
            },
        );
    }
    current(store, intent, snapshot, Some(expected)).await?;
    let final_context = call(
        guest,
        "agent.context",
        json!({"agentId":intent.agent_id}),
        None,
    )
    .await?;
    if !context_matches(&final_context, &record) {
        return Err(refused());
    }
    Ok(paths)
}

#[cfg(test)]
pub(crate) mod tests {
    use super::{COPY, prepare};
    use crate::turn_prepare::*;
    use serde_json::json;
    use studio_runtime_store::sha256_hex;

    fn snapshot() -> PreparationSnapshot {
        serde_json::from_value(json!({
            "provider":"claude","actor":{"id":"lead","root_id":"lead","epoch":["thread",0],
                "is_lead":true,"forked_from":null,"prepared_context":null,"delivered_mode":null},
            "root":{"concurrency":32,"mode_revision":0},"delivered_manifests":[],"plan":null,
            "complaints":[],"workers":[],"delivered_reminders":[],"assets":[],
            "guidance":{"role":"Role guidance"},"artifact_paths":{},
            "prepared_at":1.0,"operation_id":"original-operation"
        }))
        .expect("snapshot")
    }

    fn event() -> Event {
        serde_json::from_value(
            json!({"id":"original-event","kind":"user","text":"original input",
            "created":1.0,"metadata":{"assets":["image","file"],"accepted_at":null,"extra":{}},
            "preserve_progress":false,"preserve_complaint":false}),
        )
        .expect("event")
    }

    #[test]
    fn vm_attachment_guest_limits_match_the_maintained_layr_exec_owner() {
        use std::io::Write;
        // Owner: AgentHandlers.dispatch's layr.exec branch. Parse the actual
        // Python expressions, not an unrelated equal literal elsewhere.
        let parser = r#"import ast,json,sys
tree=ast.parse(sys.stdin.read())
branch=next(node for node in ast.walk(tree) if isinstance(node,ast.If) and
    ast.dump(node.test)==ast.dump(ast.parse("method == 'layr.exec'",mode='eval').body))
def value(node):
    if isinstance(node,ast.Constant): return node.value
    if isinstance(node,ast.BinOp) and isinstance(node.op,ast.Mult): return value(node.left)*value(node.right)
    raise ValueError('unsupported contract expression')
limit=next(node for node in ast.walk(branch) if isinstance(node,ast.Compare) and
    ast.dump(node.left)==ast.dump(ast.parse('len(data)',mode='eval').body))
config=next(node.value for node in ast.walk(branch) if isinstance(node,ast.Assign) and
    any(isinstance(target,ast.Name) and target.id=='config' for target in node.targets))
command=next(value(v) for k,v in zip(config.keys,config.values) if isinstance(k,ast.Constant) and k.value=='timeoutSeconds')
wait=next(value(node.args[-1]) for node in ast.walk(branch) if isinstance(node,ast.Call) and
    isinstance(node.func,ast.Name) and node.func.id=='supervised')
print(json.dumps({'stdin':value(limit.comparators[0]),'command':command,'wait':wait}))
"#;
        let mut child = std::process::Command::new("/usr/bin/python3")
            .args(["-I", "-S", "-c", parser])
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .expect("guest contract parser");
        child
            .stdin
            .as_mut()
            .expect("stdin")
            .write_all(include_bytes!("../../../apps/vm-guest/layr_agents.py"))
            .expect("guest owner source");
        let output = child.wait_with_output().expect("contract parser result");
        assert!(output.status.success());
        let contract: serde_json::Value =
            serde_json::from_slice(&output.stdout).expect("guest contracts");
        assert_eq!(contract["stdin"], super::CHUNK_BYTES);
        assert!(
            contract["command"].as_u64().expect("command budget")
                < contract["wait"].as_u64().expect("guest wait")
        );
        assert_eq!(
            super::EXEC_WAIT.as_secs(),
            contract["wait"].as_u64().expect("guest wait") + 5
        );
    }

    #[test]
    fn vm_attachment_projection_keeps_event_and_transcript_identity() {
        let directory = tempfile::tempdir().expect("attachment fixture");
        let mut s = snapshot();
        for (id, image) in [("image", true), ("file", false)] {
            let asset = Asset {
                id: id.into(),
                owner_id: "lead".into(),
                owner_root_id: "lead".into(),
                owner_deleted: false,
                image,
                path: directory.path().join(id).to_string_lossy().into_owned(),
                name: if image { "image.png" } else { "notes.txt" }.into(),
                size: 4,
                view: serde_json::Map::new(),
            };
            let source_fingerprint = sha256_hex(&serde_json::to_vec(&asset).expect("asset"));
            s.asset_paths.insert(
                id.into(),
                AssetPath {
                    source_fingerprint,
                    path: format!("/home/actor/.studio-attachments/{id}"),
                },
            );
            s.assets.push(asset);
        }
        let mut original = s.clone();
        original.asset_paths.clear();
        let before = compile(&original, &[event()]).expect("host projection");
        let after = compile(&s, &[event()]).expect("guest projection");
        let wire = serde_json::to_value(&after.provider_inputs)
            .expect("wire")
            .to_string();
        assert!(
            !wire.contains(directory.path().to_str().expect("path")),
            "host-only path reaches the guest: {wire}"
        );
        assert!(wire.contains("/home/actor/.studio-attachments/image"));
        assert!(wire.contains("/home/actor/.studio-attachments/file"));
        assert_eq!(
            serde_json::to_value(&before.transcript).expect("transcript"),
            serde_json::to_value(&after.transcript).expect("transcript")
        );
        assert_eq!(after.event_ids, ["original-event"]);
        assert_eq!(after.operation_id, "original-operation");
    }

    #[test]
    fn vm_attachment_projection_rejects_changed_source_and_preserves_overflow_proof() {
        let mut s = snapshot();
        let asset = Asset {
            id: "file".into(),
            owner_id: "lead".into(),
            owner_root_id: "lead".into(),
            owner_deleted: false,
            image: false,
            path: "/host/attachment".into(),
            name: "notes".into(),
            size: 4,
            view: serde_json::Map::new(),
        };
        s.asset_paths.insert(
            "file".into(),
            AssetPath {
                source_fingerprint: asset_fingerprint(&asset).expect("fingerprint"),
                path: "/home/actor/.studio-attachments/notes".into(),
            },
        );
        s.assets.push(asset);
        s.guidance.role = "large guidance ".repeat(5_000);
        let mut row = event();
        row.metadata.assets = vec!["file".into()];
        let result = compile(&s, &[row.clone()]).expect("overflow projection");
        let full = result.studio_input.expect("full overflow body");
        assert!(full.contains("/home/actor/.studio-attachments/notes"));
        assert!(!full.contains("/host/attachment"));
        let wire = serde_json::to_value(result.provider_inputs)
            .expect("wire")
            .to_string();
        assert!(wire.contains(&format!("studio-input:{}", sha256_hex(full.as_bytes()))));
        s.assets[0].size += 1;
        assert!(matches!(
            compile(&s, &[row]),
            Err(PrepareError::InvalidAssetProjection)
        ));
    }

    pub(crate) struct Guest {
        context: serde_json::Value,
        receipts: std::sync::Mutex<
            std::collections::BTreeMap<String, (serde_json::Value, serde_json::Value)>,
        >,
        writes: std::sync::atomic::AtomicUsize,
        lose_reply: std::sync::atomic::AtomicBool,
        after_write: std::sync::Mutex<Option<Box<dyn FnOnce() + Send>>>,
    }
    impl Guest {
        pub(crate) fn new(context: serde_json::Value) -> std::sync::Arc<Self> {
            std::sync::Arc::new(Self {
                context,
                receipts: std::sync::Mutex::new(std::collections::BTreeMap::new()),
                writes: std::sync::atomic::AtomicUsize::new(0),
                lose_reply: std::sync::atomic::AtomicBool::new(false),
                after_write: std::sync::Mutex::new(None),
            })
        }
        pub(crate) fn write_count(&self) -> usize {
            self.writes.load(std::sync::atomic::Ordering::SeqCst)
        }
        pub(crate) fn failures(&self) -> Vec<String> {
            use base64::Engine as _;
            self.receipts
                .lock()
                .expect("receipts")
                .values()
                .filter(|(_, result)| result["exitCode"] != 0)
                .map(|(_, result)| {
                    String::from_utf8_lossy(
                        &base64::engine::general_purpose::STANDARD
                            .decode(result["stderr"].as_str().expect("stderr"))
                            .expect("base64"),
                    )
                    .into_owned()
                })
                .collect()
        }
    }
    impl crate::linux_vm_guest::GuestRpc for Guest {
        fn call(
            &self,
            method: &str,
            params: &serde_json::Value,
            id: Option<&str>,
            _: std::time::Duration,
        ) -> Result<serde_json::Value, crate::linux_vm_guest::GuestError> {
            use base64::Engine as _;
            use std::io::Write;
            use std::sync::atomic::Ordering::SeqCst;
            if method == "agent.context" {
                return Ok(self.context.clone());
            }
            if method == "layr.file.stat" {
                let path = params["path"].as_str().expect("path");
                let bytes = std::fs::read(path).expect("guest file exists");
                return Ok(json!({"path":path,"bytes":bytes.len(),"sha256":sha256_hex(&bytes)}));
            }
            assert_eq!(method, "layr.exec");
            let id = id.expect("stable mutation identity");
            let mut receipts = self.receipts.lock().expect("receipts");
            if let Some((prior, result)) = receipts.get(id) {
                assert_eq!(
                    prior, params,
                    "changed bytes cannot reuse a mutation identity"
                );
                return Ok(result.clone());
            }
            let argv: Vec<&str> = params["argv"]
                .as_array()
                .expect("argv")
                .iter()
                .map(|v| v.as_str().expect("arg"))
                .collect();
            assert_eq!(argv[4], COPY);
            let mut child = std::process::Command::new(argv[0])
                .args(&argv[1..])
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .expect("synthetic guest script");
            if let Some(data) = params["stdin"].as_str() {
                child
                    .stdin
                    .as_mut()
                    .expect("stdin")
                    .write_all(
                        &base64::engine::general_purpose::STANDARD
                            .decode(data)
                            .expect("base64"),
                    )
                    .expect("input");
            }
            let result = child.wait_with_output().expect("script result");
            let response = json!({"exitCode":result.status.code(),"reason":"exited",
                "stdout":base64::engine::general_purpose::STANDARD.encode(result.stdout),
                "stderr":base64::engine::general_purpose::STANDARD.encode(result.stderr)});
            self.writes.fetch_add(1, SeqCst);
            receipts.insert(id.into(), (params.clone(), response.clone()));
            if let Some(hook) = self.after_write.lock().expect("hook").take() {
                hook();
            }
            if self.lose_reply.swap(false, SeqCst) {
                return Err(crate::linux_vm_guest::GuestError::Transport {
                    message: "lost synthetic reply".into(),
                    uncertain: true,
                });
            }
            Ok(response)
        }
        fn stream(
            &self,
            _: &str,
            _: &serde_json::Value,
            _: Option<&str>,
            _: std::time::Duration,
            _: &mut dyn FnMut(serde_json::Value) -> Result<(), crate::linux_vm_guest::GuestError>,
        ) -> Result<serde_json::Value, crate::linux_vm_guest::GuestError> {
            unreachable!("call only")
        }
    }

    async fn fixture(
        name: Option<&str>,
    ) -> (
        tempfile::TempDir,
        std::sync::Arc<studio_runtime_store::RuntimeStore>,
        std::sync::Arc<Guest>,
        crate::scheduler::TurnIntent,
        PreparationSnapshot,
        Vec<u8>,
    ) {
        use base64::Engine as _;
        let name = name.unwrap_or("image.png");
        let directory = tempfile::tempdir().expect("VM attachment fixture");
        let home = directory.path().join("guest-home");
        std::fs::create_dir(&home).expect("guest home");
        std::fs::create_dir(directory.path().join("state")).expect("state root");
        let store = std::sync::Arc::new(
            studio_runtime_store::RuntimeStore::open(Some(&directory.path().join("state")))
                .await
                .expect("store"),
        );
        let home_text = home.to_string_lossy().into_owned();
        let record = json!({"id":"lead","rootId":"lead","epoch":6,"status":"queued","provider":"claude",
            "accountKey":"account","executionMode":"vm","layrReady":true,"layrHome":home_text,
            "layrProjectId":"project","layrLine":"main","cwd":"/guest/line"});
        store
            .transaction_with(move |tx| {
                studio_runtime_store::AgentRepository::put(
                    tx,
                    &studio_runtime_store::AgentRecord::from_value(record)?,
                )
            })
            .await
            .expect("actor");
        let mut bytes = vec![b'x'; super::CHUNK_BYTES + 7];
        bytes[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let view = crate::payload_store::upload(
            &store,
            "lead",
            name,
            Some("3be811b3-460c-4931-8974-9fedfa497af5"),
            &base64::engine::general_purpose::STANDARD.encode(&bytes),
        )
        .await
        .expect("host upload");
        let id = view["id"].as_str().expect("id").to_owned();
        let saved = store
            .read_async("test/vm-attachment", move |tx| {
                studio_runtime_store::WorkspaceFilesRepository::asset(tx, &id)
            })
            .await
            .expect("asset")
            .expect("saved asset")
            .value()
            .clone();
        let mut s = snapshot();
        s.assets.push(Asset {
            id: saved["id"].as_str().expect("id").into(),
            owner_id: "lead".into(),
            owner_root_id: "lead".into(),
            owner_deleted: false,
            image: saved["image"].as_bool().expect("image"),
            path: saved["path"].as_str().expect("path").into(),
            name: name.into(),
            size: bytes.len() as u64,
            view: saved.as_object().expect("record").clone(),
        });
        let guest = Guest::new(
            json!({"agentId":"lead","home":home_text,"projectId":"project","line":"main","cwd":"/guest/line"}),
        );
        let intent = crate::scheduler::TurnIntent {
            agent_id: "lead".into(),
            epoch: 6,
            attempt_id: "same-attempt".into(),
            account_key: "account".into(),
            provider: studio_runtime_providers::ProviderKind::Claude,
            thread_id: Some("thread".into()),
            cwd: Some("/guest/line".into()),
            model: None,
            events: vec![],
            radio_authorized: false,
        };
        (directory, store, guest, intent, s, bytes)
    }

    #[tokio::test]
    async fn vm_attachments_stage_exact_chunks_and_reuse_a_lost_reply() {
        use std::sync::atomic::Ordering::SeqCst;
        let (_directory, store, guest, intent, s, bytes) = fixture(None).await;
        guest.lose_reply.store(true, SeqCst);
        assert!(prepare(&store, guest.clone(), &intent, &s).await.is_err());
        let projection = prepare(&store, guest.clone(), &intent, &s)
            .await
            .expect("exact retry");
        assert_eq!(
            std::fs::read(&projection[&s.assets[0].id].path).expect("guest bytes"),
            bytes
        );
        assert_eq!(
            guest.writes.load(SeqCst),
            3,
            "two chunks plus one finalization, no duplicate write"
        );
        prepare(&store, guest.clone(), &intent, &s)
            .await
            .expect("same identity replay");
        assert_eq!(guest.writes.load(SeqCst), 3);
        assert!(
            projection[&s.assets[0].id]
                .path
                .contains(".studio-attachments")
        );
        assert_eq!(
            projection[&s.assets[0].id].source_fingerprint,
            asset_fingerprint(&s.assets[0]).expect("fingerprint")
        );
    }

    #[tokio::test]
    async fn vm_attachments_refuse_stale_actor_foreign_owner_and_changed_bytes() {
        use std::sync::atomic::Ordering::SeqCst;
        let (_directory, store, guest, mut intent, mut s, _) = fixture(None).await;
        intent.epoch += 1;
        assert!(prepare(&store, guest.clone(), &intent, &s).await.is_err());
        intent.epoch -= 1;
        intent.account_key = "foreign-account".into();
        assert!(prepare(&store, guest.clone(), &intent, &s).await.is_err());
        intent.account_key = "account".into();
        s.assets[0].owner_root_id = "foreign".into();
        assert!(prepare(&store, guest.clone(), &intent, &s).await.is_err());
        s.assets[0].owner_root_id = "lead".into();
        std::fs::write(&s.assets[0].path, vec![b'y'; s.assets[0].size as usize])
            .expect("source changes");
        assert!(prepare(&store, guest.clone(), &intent, &s).await.is_err());
        assert_eq!(guest.writes.load(SeqCst), 0);
    }

    #[tokio::test]
    async fn vm_attachments_recheck_epoch_during_copy_and_refuse_wrong_guest_context() {
        use std::sync::atomic::Ordering::SeqCst;
        let (_directory, store, mut guest, intent, s, _) = fixture(None).await;
        std::sync::Arc::get_mut(&mut guest)
            .expect("sole guest")
            .context["line"] = json!("foreign-line");
        assert!(prepare(&store, guest.clone(), &intent, &s).await.is_err());
        assert_eq!(guest.writes.load(SeqCst), 0);
        std::sync::Arc::get_mut(&mut guest)
            .expect("sole guest")
            .context["line"] = json!("main");
        let runtime = tokio::runtime::Handle::current();
        let hook_store = store.clone();
        *guest.after_write.lock().expect("hook") = Some(Box::new(move || {
            #[allow(
                clippy::disallowed_methods,
                reason = "The guest copy hook runs on its dedicated blocking pool thread"
            )]
            runtime
                .block_on(hook_store.transaction_with(|tx| {
                    let mut record = studio_runtime_store::AgentRepository::get(
                        tx,
                        &studio_runtime_store::AgentId::from("lead"),
                    )?
                    .expect("actor");
                    record.epoch += 1;
                    studio_runtime_store::AgentRepository::put(tx, &record)
                }))
                .expect("epoch changes during guest copy");
        }));
        assert!(prepare(&store, guest.clone(), &intent, &s).await.is_err());
        assert_eq!(
            guest.writes.load(SeqCst),
            1,
            "the second chunk and finalization must not run"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vm_attachments_refuse_host_and_guest_symlink_escapes() {
        use std::sync::atomic::Ordering::SeqCst;
        let (directory, store, guest, intent, s, _) = fixture(None).await;
        let outside = directory.path().join("outside");
        std::fs::create_dir(&outside).expect("outside fixture");
        let source = std::path::Path::new(&s.assets[0].path);
        let original = directory.path().join("original");
        std::fs::rename(source, &original).expect("move original");
        std::os::unix::fs::symlink(&original, source).expect("source symlink");
        assert!(prepare(&store, guest.clone(), &intent, &s).await.is_err());
        assert_eq!(guest.writes.load(SeqCst), 0);
        std::fs::remove_file(source).expect("remove fixture symlink");
        std::fs::rename(original, source).expect("restore fixture file");
        let home = std::path::Path::new(guest.context["home"].as_str().expect("home"));
        std::os::unix::fs::symlink(&outside, home.join(".studio-attachments"))
            .expect("guest symlink");
        assert!(prepare(&store, guest.clone(), &intent, &s).await.is_err());
        assert_eq!(
            std::fs::read_dir(outside).expect("outside").count(),
            0,
            "no write outside private home"
        );
    }

    #[tokio::test]
    async fn vm_attachments_keep_valid_names_separate_from_chunk_cache() {
        let mut first = vec![b'x'; super::CHUNK_BYTES];
        first[..8].copy_from_slice(b"\x89PNG\r\n\x1a\n");
        let name = format!("0.{}.part", sha256_hex(&first));
        let (_directory, store, guest, intent, s, bytes) = fixture(Some(&name)).await;
        let paths = prepare(&store, guest.clone(), &intent, &s)
            .await
            .expect("a valid upload name cannot collide with transport bookkeeping");
        let path = std::path::Path::new(&paths[&s.assets[0].id].path);
        assert_eq!(
            path.file_name().and_then(|name| name.to_str()),
            Some(name.as_str())
        );
        assert_eq!(std::fs::read(path).expect("complete staged file"), bytes);
        assert!(guest.failures().is_empty());
    }
}
