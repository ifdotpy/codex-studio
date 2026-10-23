#![allow(clippy::unwrap_used, clippy::expect_used)]

use super::*;
use std::time::Duration;

const TOKEN: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
const BUILD: &str = "build-a";

fn repository() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../../..")
}

struct Fixture {
    _temp: tempfile::TempDir,
    root: PathBuf,
    state: PathBuf,
    line: PathBuf,
    service: Arc<HostService>,
}

impl Fixture {
    fn new() -> Self {
        Self::build(None)
    }

    /// A service whose commands send their sign-in links to the host bridge in `folder`.
    fn with_browser_host(folder: &Path) -> Self {
        Self::build(Some(folder.to_path_buf()))
    }

    fn build(browser_host: Option<PathBuf>) -> Self {
        let temp = tempfile::tempdir().unwrap();
        let root =
            studio_runtime_store::path_identity::CanonicalPath::existing_directory(temp.path())
                .unwrap()
                .into_path_buf();
        let state = root.join("state");
        fs::create_dir(&state).unwrap();
        fs::write(state.join(TOKEN_FILE), TOKEN).unwrap();
        let line = root.join("line");
        fs::create_dir(&line).unwrap();
        let service = Arc::new(
            HostService::with_limits(&state, BUILD.into(), 2, DEFAULT_BUDGET)
                .unwrap()
                .with_browser_host(browser_host),
        );
        Self {
            _temp: temp,
            root,
            state,
            line,
            service,
        }
    }

    async fn request(&self, id: &str, method: &str, params: Value) -> Value {
        self.service
            .request(&json!({"id": id, "method": method, "params": params, "token": TOKEN}))
            .await
            .unwrap()
    }

    async fn call(&self, id: &str, method: &str, params: Value) -> Result<Value, (String, String)> {
        let response = self.request(id, method, params).await;
        assert_eq!(response["id"], id, "{response}");
        match response.get("error") {
            Some(error) => Err((
                error["code"].as_str().unwrap().to_owned(),
                error["message"].as_str().unwrap().to_owned(),
            )),
            None => Ok(response["result"].clone()),
        }
    }

    async fn acquire(&self, operation: &str) -> Result<Value, (String, String)> {
        self.call(
            &format!("acquire:{operation}"),
            "acquire",
            json!({"operationId": operation, "agentId": "agent", "projectId": "project",
                   "linePath": self.line.display().to_string()}),
        )
        .await
    }

    fn settings(&self, value: &Value) {
        let folder = self.state.join("layr-projects/project");
        fs::create_dir_all(&folder).unwrap();
        fs::write(folder.join(SETTINGS_FILE), value.to_string()).unwrap();
    }

    fn manifest_digest(slot_path: &Value) -> String {
        let manifest = protocol::scan(Path::new(slot_path.as_str().unwrap()), &[], None).unwrap();
        protocol::manifest_digest(&manifest)
    }

    async fn start(&self, operation: &str, argv: &[&str], timeout: f64) -> Value {
        let op = self.acquire(operation).await.unwrap();
        self.call(
            &format!("run:{operation}"),
            "run",
            json!({"operationId": operation, "agentId": "agent", "argv": argv,
                   "manifestSha256": Self::manifest_digest(&op["slotPath"]), "timeoutSeconds": timeout}),
        )
        .await
        .unwrap()
    }

    /// Attach until the command exited; returns its status and the output text.
    async fn finish(&self, operation: &str) -> (Value, String) {
        let mut after = 0;
        let mut output = String::new();
        for round in 0..400 {
            let attached = self
                .call(
                    &format!("attach:{operation}:{round}"),
                    "attach",
                    json!({"operationId": operation, "agentId": "agent", "afterSeq": after, "waitSeconds": 1}),
                )
                .await
                .unwrap();
            let events = attached["events"].as_array().unwrap();
            for event in events {
                after = event["seq"].as_i64().unwrap();
                output.push_str(
                    event
                        .get("text")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                );
            }
            if attached["status"]["state"] != "running" && events.is_empty() {
                return (attached["status"].clone(), output);
            }
        }
        panic!("the command of {operation} did not finish");
    }

    fn op_status(&self, operation: &str) -> Op {
        self.service.load(operation).unwrap()
    }
}

/// The Rust copies of the guest protocol limits: `vm-guest/host_exec_protocol.py` owns them.
#[test]
fn the_protocol_limits_match_the_guest_owner() {
    fn evaluate(expression: &str) -> i128 {
        expression
            .split('*')
            .map(|factor| factor.trim().replace('_', "").parse::<i128>().unwrap())
            .product()
    }
    fn owner_value(source: &str, name: &str) -> i128 {
        let line = source
            .lines()
            .find(|line| line.starts_with(&format!("{name} = ")))
            .unwrap_or_else(|| panic!("{name} has no owner value"));
        evaluate(line.split_once(" = ").unwrap().1)
    }
    let source = fs::read_to_string(
        repository().join("workspaces/runtime/apps/vm-guest/host_exec_protocol.py"),
    )
    .unwrap();
    let copies = [
        ("FRAME_LIMIT", protocol::FRAME_LIMIT as i128),
        ("CHUNK", protocol::CHUNK as i128),
        ("MAX_ENTRIES", protocol::MAX_ENTRIES as i128),
        ("PAGE_ENTRIES", protocol::PAGE_ENTRIES as i128),
        ("PAGE_BYTES", protocol::PAGE_BYTES as i128),
        ("RACY_NS", protocol::RACY_NS),
    ];
    for (name, copy) in copies {
        assert_eq!(owner_value(&source, name), copy, "{name}");
    }
    // A changed owner value fails the check.
    let changed = source.replacen("CHUNK = 512 * 1024", "CHUNK = 256 * 1024", 1);
    assert_ne!(changed, source);
    assert_ne!(owner_value(&changed, "CHUNK"), protocol::CHUNK as i128);
    // The native helper forwards the guest port to the socket of this service.
    let helper = fs::read_to_string(
        repository().join("workspaces/client/apps/desktop/native/linux-vm/main.swift"),
    )
    .unwrap();
    assert!(helper.contains(&format!("appendingPathComponent(\"{SOCKET_FILE}\")")));
    // The bundled tool definition names the environment and the timeout of this service.
    let tool = crate::host_exec::tool_definition().unwrap();
    for name in [
        ENV_DERIVED_DATA,
        ENV_PACKAGE_CACHE,
        ENV_ARTIFACTS,
        ENV_STATE,
        ENV_TREE,
    ] {
        assert!(tool.description.contains(name), "{name}");
    }
    assert_eq!(
        tool.input_schema
            .get("properties")
            .and_then(|properties| properties.pointer("/timeout_seconds/maximum"))
            .and_then(Value::as_f64),
        Some(crate::host_exec::MAX_TIMEOUT_SECONDS)
    );
}

/// The guest computes `manifestSha256` with Python `json.dumps(sort_keys=True)`;
/// the values below are what `host_exec_protocol.scan` gives for the same tree.
#[test]
fn the_manifest_hashes_as_the_guest_hashes_it() {
    use std::os::unix::fs::PermissionsExt;
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir(root.join("src")).unwrap();
    fs::write(root.join("src/main.swift"), "print(\"Hello\")\n").unwrap();
    fs::set_permissions(
        root.join("src/main.swift"),
        fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    fs::write(root.join("tool.sh"), "#!/bin/sh\nexit 0\n").unwrap();
    fs::set_permissions(root.join("tool.sh"), fs::Permissions::from_mode(0o755)).unwrap();
    fs::write(root.join("caf\u{e9} \"q\""), "x").unwrap();
    fs::set_permissions(
        root.join("caf\u{e9} \"q\""),
        fs::Permissions::from_mode(0o600),
    )
    .unwrap();
    std::os::unix::fs::symlink("src/\u{fc}ber\u{1f600}", root.join("link")).unwrap();
    fs::create_dir(root.join("empty")).unwrap();
    let manifest = protocol::scan(root, &[], None).unwrap();
    assert_eq!(
        protocol::sorted_json(&protocol::manifest_value(&manifest)),
        r#"{"caf\u00e9 \"q\"": {"bytes": 1, "kind": "file", "mode": 384, "sha256": "2d711642b726b04401627ca9fbac32f5c8530fb1903cc4db02258717921a4881"}, "empty": {"kind": "directory"}, "link": {"kind": "link", "target": "src/\u00fcber\ud83d\ude00"}, "src": {"kind": "directory"}, "src/main.swift": {"bytes": 15, "kind": "file", "mode": 420, "sha256": "18a6066e8dd25f6d99b3274b3232ebae6750fc8d1637729125863d7919df784f"}, "tool.sh": {"bytes": 17, "kind": "file", "mode": 493, "sha256": "306c6ca7407560340797866e077e053627ad409277d1b9da58106fce4cf717cb"}}"#
    );
    assert_eq!(
        protocol::manifest_digest(&manifest),
        "646deef4182c19349905920ea0cb4f07893b09d9b3d5487c0c62a60ea2fdb607"
    );
    // A skipped path stays out with everything inside it.
    let skipped = protocol::scan(root, &["src".into()], None).unwrap();
    assert!(!skipped.contains_key("src") && !skipped.contains_key("src/main.swift"));
}

#[test]
fn name_alias_and_page_rules() {
    assert!(!protocol::conflicts(&["README".to_owned(), "readme".to_owned()]).is_empty());
    assert!(!protocol::conflicts(&["caf\u{e9}".to_owned(), "cafe\u{301}".to_owned()]).is_empty());
    assert!(!protocol::conflicts(&["Folder/a".to_owned(), "folder/b".to_owned()]).is_empty());
    assert!(protocol::conflicts(&["a".to_owned(), "b/a".to_owned()]).is_empty());
    assert_eq!(
        protocol::mapped(
            "/var/slot/source/file /private/var/slot/source/file /private/var/slot/sourcex",
            "/private/var/slot/source",
            "/line"
        ),
        "/line/file /line/file /private/var/slot/sourcex"
    );
    let items: Vec<(String, Value)> = (0..protocol::PAGE_ENTRIES + 10)
        .map(|index| (format!("f{index:05}"), json!({"kind": "directory"})))
        .collect();
    let first = protocol::page(&items, Some(&json!(""))).unwrap();
    assert_eq!(
        first["entries"].as_object().unwrap().len(),
        protocol::PAGE_ENTRIES
    );
    let second = protocol::page(&items, Some(&first["next"])).unwrap();
    assert_eq!(second["entries"].as_object().unwrap().len(), 10);
    assert!(second["next"].is_null());
    assert!(protocol::page(&items, Some(&json!(5))).is_err());
}

#[test]
fn the_hash_cache_keeps_only_settled_files() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("file");
    fs::write(&path, "content").unwrap();
    let meta = fs::symlink_metadata(&path).unwrap();
    let mut hashes = Map::new();
    // A file that just changed is not kept: a second write in the same clock step would hide.
    let (checksum, known) =
        protocol::file_hash(&path, &meta, "file", Some(&mut hashes), protocol::now_ns()).unwrap();
    assert!(!known && hashes.is_empty());
    let later = protocol::now_ns() + protocol::RACY_NS + 1_000_000_000;
    let (again, known) =
        protocol::file_hash(&path, &meta, "file", Some(&mut hashes), later).unwrap();
    assert_eq!((again.as_str(), known), (checksum.as_str(), true));
    assert_eq!(hashes["file"][4], json!(checksum));
    // The cached hash answers without a read while the stat key is the same.
    fs::write(&path, "changed").unwrap();
    hashes.insert("file".into(), {
        let mut row = protocol::stamp(&fs::symlink_metadata(&path).unwrap()).to_vec();
        row.push(json!("cached"));
        Value::Array(row)
    });
    let meta = fs::symlink_metadata(&path).unwrap();
    let (cached, known) =
        protocol::file_hash(&path, &meta, "file", Some(&mut hashes), later).unwrap();
    assert_eq!((cached.as_str(), known), ("cached", true));
}

#[tokio::test]
async fn an_exact_retry_returns_the_saved_answer_and_changed_content_is_refused() {
    let fixture = Fixture::new();
    let params = json!({"operationId": "retry", "agentId": "agent", "projectId": "project",
                        "linePath": fixture.line.display().to_string()});
    let first = fixture.request("acquire", "acquire", params.clone()).await;
    assert_eq!(
        fixture.request("acquire", "acquire", params.clone()).await,
        first
    );
    let mut changed = params.clone();
    changed["projectId"] = json!("other");
    let refused = fixture.request("acquire", "acquire", changed).await;
    assert_eq!(refused["error"]["code"], "id_conflict");
    let slot = first["result"]["slotPath"].clone();
    let run = json!({"operationId": "retry", "agentId": "agent",
        "argv": ["/bin/sh", "-c", "printf x >> count"], "manifestSha256": Fixture::manifest_digest(&slot)});
    let started = fixture.request("run-once", "run", run.clone()).await;
    assert_eq!(started["result"]["state"], "running", "{started}");
    fixture.finish("retry").await;
    assert_eq!(fixture.request("run-once", "run", run).await, started);
    assert_eq!(
        fs::read_to_string(Path::new(slot.as_str().unwrap()).join("count")).unwrap(),
        "x"
    );
}

#[tokio::test]
async fn an_unknown_receipt_never_replays() {
    let fixture = Fixture::new();
    let params = json!({"operationId": "unknown", "agentId": "agent", "projectId": "project",
                        "linePath": fixture.line.display().to_string()});
    let digest = protocol::key(&protocol::sorted_json(&json!(["acquire", params])));
    fixture
        .service
        .db()
        .execute("INSERT INTO receipts VALUES ('lost', ?1, NULL)", [&digest])
        .unwrap();
    let response = fixture.request("lost", "acquire", params).await;
    assert_eq!(response["error"]["code"], "outcome_unknown");
    assert!(!fixture.service.opdir("unknown").exists());
}

#[tokio::test]
async fn the_token_and_the_agent_identity_are_checked() {
    let fixture = Fixture::new();
    let denied = fixture
        .service
        .request(&json!({"id": "bad-token", "method": "health", "token": "b".repeat(64)}))
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], "forbidden");
    fixture.acquire("owned").await.unwrap();
    let other = fixture
        .call(
            "other",
            "status",
            json!({"operationId": "owned", "agentId": "other"}),
        )
        .await;
    assert_eq!(other.unwrap_err().0, "forbidden");
    // A frame that is not an object gets no answer.
    assert!(fixture.service.request(&json!(["health"])).await.is_none());
}

#[tokio::test]
async fn retirement_needs_the_token_the_build_and_an_idle_service() {
    let fixture = Fixture::new();
    let params = json!({"buildId": BUILD, "pid": std::process::id()});
    let denied = fixture
        .service
        .request(&json!({"id": "retire-auth", "method": "service.retire", "params": params, "token": "b".repeat(64)}))
        .await
        .unwrap();
    assert_eq!(denied["error"]["code"], "forbidden");
    let wrong = fixture
        .call(
            "retire-build",
            "service.retire",
            json!({"buildId": "old", "pid": std::process::id()}),
        )
        .await;
    assert_eq!(wrong.unwrap_err().0, "id_conflict");
    let health = fixture.call("health", "health", json!({})).await.unwrap();
    assert_eq!(
        (health["buildId"].clone(), health["busy"].clone()),
        (json!(BUILD), json!(false))
    );
    // A running command keeps the service.
    fixture.start("busy", &["/bin/sleep", "2"], 30.0).await;
    let busy = fixture
        .call("retire-busy", "service.retire", params.clone())
        .await;
    assert_eq!(busy.unwrap_err().0, "busy");
    fixture.finish("busy").await;
    let retired = fixture
        .call("retire", "service.retire", params)
        .await
        .unwrap();
    assert_eq!(retired["state"], "retiring");
    let refused = fixture
        .call(
            "after-retire",
            "status",
            json!({"operationId": "busy", "agentId": "agent"}),
        )
        .await;
    assert_eq!(refused.unwrap_err().0, "busy");
}

#[tokio::test]
async fn slots_are_exclusive_and_settings_set_the_count_and_budget() {
    let fixture = Fixture::new();
    fixture.acquire("a").await.unwrap();
    fixture.acquire("b").await.unwrap();
    assert_eq!(fixture.acquire("c").await.unwrap_err().0, "busy");
    fixture
        .call(
            "release-a",
            "release",
            json!({"operationId": "a", "agentId": "agent"}),
        )
        .await
        .unwrap();
    fixture
        .call(
            "release-b",
            "release",
            json!({"operationId": "b", "agentId": "agent"}),
        )
        .await
        .unwrap();
    fixture.settings(&json!({"slots": 1, "diskBudgetGiB": 300, "macOnly": ["out"]}));
    let one = fixture.acquire("one").await.unwrap();
    assert_eq!(
        (one["diskBudget"].clone(), one["macOnly"].clone()),
        (json!(300_u64 << 30), json!(["out"]))
    );
    // The smaller slot count removed the least recently used idle slot.
    assert_eq!(one["slotId"], fixture.op_status("b")["slotId"]);
    assert_eq!(fixture.acquire("two").await.unwrap_err().0, "busy");
    for (index, value) in [
        json!({"slots": 0}),
        json!({"slots": true}),
        json!({"slots": 1.0}),
        json!({"diskBudgetGiB": -1}),
        json!({"diskBudgetGiB": "300"}),
        json!({"budget": 300}),
        json!({"macOnly": ["../out"]}),
        json!({"macOnly": "out"}),
    ]
    .into_iter()
    .enumerate()
    {
        fixture.settings(&value);
        let (code, message) = fixture
            .acquire(&format!("invalid-{index}"))
            .await
            .unwrap_err();
        assert_eq!(code, "invalid_config", "{value}");
        assert!(message.contains(SETTINGS_FILE), "{message}");
        assert!(!fixture.service.opdir(&format!("invalid-{index}")).exists());
    }
}

#[tokio::test]
async fn a_command_streams_mapped_output_and_returns_its_changes_and_artifacts() {
    let fixture = Fixture::new();
    let script = "pwd; printf 'host edit\\n' > main.swift; printf artifact > \"$HOST_EXEC_ARTIFACTS/result.txt\"; \
                  ln -s main.swift link; printf \"${HOST_EXEC_TREE:-none}\" >&2";
    let op = fixture.acquire("first").await.unwrap();
    let started = fixture
        .call(
            "run:first",
            "run",
            json!({"operationId": "first", "agentId": "agent", "argv": ["/bin/sh", "-c", script],
                   "manifestSha256": Fixture::manifest_digest(&op["slotPath"]), "timeoutSeconds": 30,
                   "layrState": "0123456789abcdef", "layrTree": "5e2352431d25bc530ae038e5958b5c3ec3574d26"}),
        )
        .await
        .unwrap();
    assert_eq!(
        started["layrTree"],
        "5e2352431d25bc530ae038e5958b5c3ec3574d26"
    );
    let (status, output) = fixture.finish("first").await;
    assert_eq!(
        (
            status["state"].clone(),
            status["exitCode"].clone(),
            status["reason"].clone()
        ),
        (json!("exited"), json!(0), json!("exited"))
    );
    // The slot path is mapped to the line path in the output.
    assert!(
        output.contains(&fixture.line.display().to_string()),
        "{output}"
    );
    assert!(
        !output.contains(op["slotPath"].as_str().unwrap()),
        "{output}"
    );
    assert!(
        output.contains("5e2352431d25bc530ae038e5958b5c3ec3574d26"),
        "{output}"
    );
    let changes = fixture
        .call(
            "changes",
            "result.manifest",
            json!({"operationId": "first", "agentId": "agent", "area": "changes", "after": ""}),
        )
        .await
        .unwrap();
    let entries = changes["entries"].as_object().unwrap();
    assert_eq!(entries["main.swift"]["kind"], "file");
    assert_eq!(
        entries["link"],
        json!({"kind": "link", "target": "main.swift"})
    );
    let read = fixture
        .call("read", "read", json!({"operationId": "first", "agentId": "agent", "area": "changes", "path": "main.swift"}))
        .await
        .unwrap();
    use base64::Engine;
    assert_eq!(
        base64::engine::general_purpose::STANDARD
            .decode(read["data"].as_str().unwrap())
            .unwrap(),
        b"host edit\n"
    );
    let artifacts = fixture
        .call(
            "artifacts",
            "result.manifest",
            json!({"operationId": "first", "agentId": "agent", "area": "artifacts", "after": ""}),
        )
        .await
        .unwrap();
    assert_eq!(artifacts["entries"]["result.txt"]["bytes"], 8);
    let released = fixture
        .call(
            "release",
            "release",
            json!({"operationId": "first", "agentId": "agent", "collected": true}),
        )
        .await
        .unwrap();
    assert_eq!(
        (released["state"].clone(), released["collected"].clone()),
        (json!("released"), json!(true))
    );
    // A warm slot keeps its paths for the next command.
    let second = fixture.acquire("second").await.unwrap();
    assert_eq!(second["slotPath"], op["slotPath"]);
    assert_eq!(second["generation"], op["generation"]);
}

/// A VM chat's sign-in command on the Mac reaches the host bridge as that chat
/// (docs/vm-browser-bridge.md, "Native chats"): `BROWSER` is the launcher, and the session
/// is the calling agent. Without a launcher the command gets none of these names.
#[tokio::test]
async fn a_command_gets_the_browser_launcher_of_its_calling_chat() {
    let script = "printf '%s|%s|%s' \"${BROWSER-unset}\" \"${STUDIO_BROWSER_SOCKET-unset}\" \"${STUDIO_BROWSER_SESSION-unset}\"";
    let folder = tempfile::tempdir().unwrap();
    let folder =
        studio_runtime_store::path_identity::CanonicalPath::existing_directory(folder.path())
            .unwrap()
            .into_path_buf();
    let browser_host = folder.join("browser-host");
    fs::create_dir(&browser_host).unwrap();
    let fixture = Fixture::with_browser_host(&browser_host);
    fixture
        .start("before", &["/bin/sh", "-c", script], 30.0)
        .await;
    let (_, output) = fixture.finish("before").await;
    assert_eq!(output, "unset|unset|unset", "no launcher yet");
    fixture
        .call(
            "release:before",
            "release",
            json!({"operationId": "before", "agentId": "agent", "collected": true}),
        )
        .await
        .unwrap();
    fs::write(
        browser_host.join(crate::browser_environment::BROWSER_LAUNCHER),
        "#!/bin/sh\n",
    )
    .unwrap();
    fixture
        .start("after", &["/bin/sh", "-c", script], 30.0)
        .await;
    let (_, output) = fixture.finish("after").await;
    assert_eq!(
        output,
        format!(
            "{}|{}|agent",
            browser_host.join("studio-browser").display(),
            browser_host.join("open.sock").display()
        )
    );
}

#[tokio::test]
async fn the_mac_refuses_an_invalid_tree_id_and_a_changed_manifest() {
    let fixture = Fixture::new();
    let op = fixture.acquire("bad").await.unwrap();
    let run = json!({"operationId": "bad", "agentId": "agent", "argv": ["/usr/bin/true"],
                     "manifestSha256": Fixture::manifest_digest(&op["slotPath"])});
    for (index, fields) in [
        json!({"layrState": "0123456789abcdef", "layrTree": "abc"}),
        json!({"layrState": "0123456789abcdef", "layrTree": "5E2352431D25BC530AE038E5958B5C3EC3574D26"}),
        json!({"layrTree": "5e2352431d25bc530ae038e5958b5c3ec3574d26"}),
        json!({"layrState": "XYZ"}),
    ]
    .into_iter()
    .enumerate()
    {
        let mut params = run.clone();
        params.as_object_mut().unwrap().extend(fields.as_object().unwrap().clone());
        let refused = fixture.call(&format!("run-{index}"), "run", params).await;
        assert_eq!(refused.unwrap_err().0, "invalid_params");
    }
    let mut changed = run.clone();
    changed["manifestSha256"] = json!("bad");
    assert_eq!(
        fixture
            .call("run-changed", "run", changed)
            .await
            .unwrap_err()
            .0,
        "sync_conflict"
    );
    for path in ["../escape", "/absolute"] {
        let escaped = fixture
            .call(&format!("entry:{path}"), "sync.entry", json!({"operationId": "bad", "agentId": "agent", "path": path, "entry": {"kind": "directory"}}))
            .await;
        assert_eq!(escaped.unwrap_err().0, "invalid_params");
    }
    assert!(fixture.service.inner().commands.is_empty());
    assert_eq!(fixture.op_status("bad")["state"], "leased");
}

#[tokio::test]
async fn cancel_and_timeout_stop_the_command_and_keep_its_changes() {
    let fixture = Fixture::new();
    fixture
        .start(
            "cancel",
            &[
                "/bin/sh",
                "-c",
                "printf before > edit; touch started; exec sleep 30",
            ],
            60.0,
        )
        .await;
    let slot = PathBuf::from(fixture.op_status("cancel")["slotPath"].as_str().unwrap());
    for _ in 0..400 {
        if slot.join("started").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    let cancelled = fixture
        .call(
            "cancel",
            "cancel",
            json!({"operationId": "cancel", "agentId": "agent"}),
        )
        .await
        .unwrap();
    assert_eq!(cancelled["cancelRequested"], true);
    let (status, _) = fixture.finish("cancel").await;
    assert_eq!(status["reason"], "cancelled");
    assert_eq!(status["changedPaths"], 2);
    fixture
        .start(
            "timeout",
            &["/bin/sh", "-c", "printf before > edit2; exec sleep 30"],
            1.0,
        )
        .await;
    let (status, _) = fixture.finish("timeout").await;
    assert_eq!(status["reason"], "timeout");
}

/// A group signal after the leader is reaped could reach another group that
/// reuses the ID. Every signal to a command's group comes before its leader is
/// reaped, for a command that exits by itself and for one that is cancelled.
#[tokio::test]
async fn command_group_signals_come_before_the_leader_is_reaped() {
    let fixture = Fixture::new();
    fixture
        .start("plain", &["/bin/sh", "-c", "exit 0"], 30.0)
        .await;
    let (status, _) = fixture.finish("plain").await;
    assert_eq!(status["reason"], "exited");
    fixture
        .start(
            "stopped",
            &["/bin/sh", "-c", "touch started; exec sleep 30"],
            60.0,
        )
        .await;
    let slot = PathBuf::from(fixture.op_status("stopped")["slotPath"].as_str().unwrap());
    for _ in 0..400 {
        if slot.join("started").exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    fixture
        .call(
            "cancel-stopped",
            "cancel",
            json!({"operationId": "stopped", "agentId": "agent"}),
        )
        .await
        .unwrap();
    let (status, _) = fixture.finish("stopped").await;
    assert_eq!(status["reason"], "cancelled");
    let signals = super::command::LEADER_UNREAPED_AT_SIGNAL
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clone();
    for operation in ["plain", "stopped"] {
        let pid = i32::try_from(fixture.op_status(operation)["pid"].as_i64().unwrap()).unwrap();
        let seen = signals.get(&pid).cloned().unwrap_or_default();
        assert!(!seen.is_empty(), "{operation}: the group is signalled");
        assert!(
            seen.iter().all(|unreaped| *unreaped),
            "{operation}: a group signal went out after the leader was reaped: {seen:?}"
        );
    }
}

#[tokio::test]
async fn a_lost_owner_is_unknown_and_keeps_the_lease() {
    let fixture = Fixture::new();
    let mut op = fixture
        .acquire("lost-owner")
        .await
        .unwrap()
        .as_object()
        .unwrap()
        .clone();
    op.insert("state".into(), json!("running"));
    fixture.service.save("lost-owner", &op).unwrap();
    assert_eq!(fixture.op_status("lost-owner")["state"], "unknown");
    let release = fixture
        .call(
            "release",
            "release",
            json!({"operationId": "lost-owner", "agentId": "agent"}),
        )
        .await;
    assert_eq!(release.unwrap_err().0, "invalid_params");
    let cancel = fixture
        .call(
            "cancel",
            "cancel",
            json!({"operationId": "lost-owner", "agentId": "agent"}),
        )
        .await;
    assert_eq!(cancel.unwrap_err().0, "outcome_unknown");
    assert!(
        fixture
            .service
            .slot(&op)
            .unwrap()
            .join("lease.json")
            .exists()
    );
}

#[tokio::test]
async fn a_guest_restart_frees_idle_slots_and_orphans_running_commands() {
    let fixture = Fixture::new();
    let params = |operation: &str, owner: &str| {
        json!({"operationId": operation, "agentId": "agent", "projectId": "project",
               "linePath": fixture.line.display().to_string(), "owner": owner})
    };
    let idle = fixture
        .call("acquire-idle", "acquire", params("idle", "old"))
        .await
        .unwrap();
    let mine = fixture
        .call("acquire-mine", "acquire", params("mine", "new"))
        .await
        .unwrap();
    let report = fixture
        .call(
            "recover",
            "recover",
            json!({"operationId": "guest-owner:new", "owner": "new"}),
        )
        .await
        .unwrap();
    assert_eq!(report["released"], json!(["idle"]));
    assert_eq!(report["releasedSlots"][0]["generation"], idle["generation"]);
    assert_eq!(fixture.op_status("idle")["abandoned"], "guest_owner_lost");
    assert!(
        fixture
            .service
            .slot(mine.as_object().unwrap())
            .unwrap()
            .join("lease.json")
            .exists()
    );
    let slot_json: Value = serde_json::from_str(
        &fs::read_to_string(
            Path::new(idle["slotPath"].as_str().unwrap())
                .parent()
                .unwrap()
                .join("slot.json"),
        )
        .unwrap(),
    )
    .unwrap();
    assert_ne!(slot_json["generation"], idle["generation"]);
    fixture
        .call(
            "release-mine",
            "release",
            json!({"operationId": "mine", "agentId": "agent"}),
        )
        .await
        .unwrap();
    // A running command of a lost guest keeps its slot until it exits, then frees it.
    let gate = fixture.root.join("gate");
    let op = fixture
        .call("acquire-running", "acquire", params("running", "old"))
        .await
        .unwrap();
    fixture
        .call(
            "run-running",
            "run",
            json!({"operationId": "running", "agentId": "agent",
                   "argv": ["/bin/sh", "-c", format!("while [ ! -e '{}' ]; do sleep 0.05; done", gate.display())],
                   "manifestSha256": Fixture::manifest_digest(&op["slotPath"])}),
        )
        .await
        .unwrap();
    let report = fixture
        .call(
            "recover-2",
            "recover",
            json!({"operationId": "guest-owner:newer", "owner": "newer"}),
        )
        .await
        .unwrap();
    assert_eq!(report["orphaned"], json!(["running"]));
    let listed = fixture
        .call("list", "list", json!({"agentId": "agent"}))
        .await
        .unwrap();
    let row = listed["operations"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["operationId"] == "running")
        .unwrap()
        .clone();
    assert_eq!(
        (
            row["state"].clone(),
            row["leased"].clone(),
            row["orphaned"].clone()
        ),
        (json!("running"), json!(true), json!(true))
    );
    let release = fixture
        .call(
            "release-running",
            "release",
            json!({"operationId": "running", "agentId": "agent"}),
        )
        .await;
    assert_eq!(release.unwrap_err().0, "invalid_params");
    fs::write(&gate, "exit").unwrap();
    let (status, _) = fixture.finish("running").await;
    assert_eq!(
        (
            status["state"].clone(),
            status["exitCode"].clone(),
            status["abandoned"].clone()
        ),
        (json!("released"), json!(0), json!("guest_owner_lost"))
    );
}

#[tokio::test]
async fn an_interrupted_slot_removal_blocks_reuse_until_recovery() {
    use std::os::unix::fs::PermissionsExt;
    let fixture = Fixture::new();
    let op = fixture.acquire("retire-old").await.unwrap();
    fixture
        .call(
            "release",
            "release",
            json!({"operationId": "retire-old", "agentId": "agent"}),
        )
        .await
        .unwrap();
    let slot = Path::new(op["slotPath"].as_str().unwrap())
        .parent()
        .unwrap()
        .to_path_buf();
    // A folder that the cleanup cannot empty interrupts the removal.
    let locked = slot.join("DerivedData/locked");
    fs::create_dir_all(&locked).unwrap();
    fs::write(locked.join("evidence"), vec![b'x'; 65536]).unwrap();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o500)).unwrap();
    fixture.settings(&json!({"diskBudgetGiB": 8192.0 / (1u64 << 30) as f64}));
    let failed = fixture.acquire("retire-failed").await.unwrap_err();
    fs::set_permissions(&locked, fs::Permissions::from_mode(0o700)).unwrap();
    assert_eq!(failed.0, "slot_cleanup_failed", "{failed:?}");
    let marker = slot.with_file_name(format!(
        ".retiring-{}.json",
        slot.file_name().unwrap().to_str().unwrap()
    ));
    let saved: Value = serde_json::from_str(&fs::read_to_string(&marker).unwrap()).unwrap();
    assert_eq!(saved["metadata"]["generation"], op["generation"]);
    assert!(locked.join("evidence").exists());
    assert!(!fixture.service.opdir("retire-failed").exists());
    // Spare capacity does not make the remaining tree reusable.
    fs::remove_file(
        fixture
            .state
            .join("layr-projects/project")
            .join(SETTINGS_FILE),
    )
    .unwrap();
    assert_eq!(
        fixture.acquire("retire-followup").await.unwrap_err().0,
        "slot_cleanup_failed"
    );
}

#[test]
fn a_service_without_a_build_identity_is_never_stopped() {
    let temp = tempfile::tempdir().unwrap();
    let kept = lifecycle::stop_old_service(temp.path(), TOKEN, &json!({"protocol": 1}));
    assert_eq!(kept, Ok(Some("it has no build identity".to_owned())));
}

/// The real socket: one request per connection, an answer line, and no answer
/// for a frame that is not JSON.
#[tokio::test]
async fn the_socket_serves_one_request_per_connection() {
    let fixture = Fixture::new();
    let server = tokio::spawn(serve(Arc::clone(&fixture.service)));
    let socket = fixture.state.join(SOCKET_FILE);
    for _ in 0..200 {
        if socket.exists() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    let state = fixture.state.clone();
    let health = studio_runtime_store::blocking_pool::spawn_blocking("test", move || {
        lifecycle::service_call(&state, TOKEN, "health", None)
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(
        (health["protocol"].clone(), health["buildId"].clone()),
        (json!(PROTOCOL), json!(BUILD))
    );
    let state = fixture.state.clone();
    let retired = studio_runtime_store::blocking_pool::spawn_blocking("test", move || {
        lifecycle::service_call(
            &state,
            TOKEN,
            "service.retire",
            Some(&json!({"buildId": BUILD, "pid": std::process::id()})),
        )
    })
    .await
    .unwrap()
    .unwrap();
    assert_eq!(retired["state"], "retiring");
    tokio::time::timeout(Duration::from_secs(10), server)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
}
