use rusqlite::{Connection, params};
use serde::Deserialize;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;
use studio_runtime_services::{
    coordination,
    linux_vm_guest::{GuestError, GuestRpc, GuestSpaceProbe, UnixGuestRpc},
    linux_vm_settings, mac_sync,
    message_admission::MessageAdmission,
};
use studio_runtime_store::Transaction;

struct FakeGuest(std::sync::Mutex<Vec<(String, Value, Option<String>)>>);
impl GuestRpc for FakeGuest {
    fn stream(
        &self,
        method: &str,
        params: &Value,
        request_id: Option<&str>,
        _timeout: Duration,
        _on_frame: &mut dyn FnMut(Value) -> Result<(), GuestError>,
    ) -> Result<Value, GuestError> {
        self.0.lock().expect("fake guest log").push((
            method.to_owned(),
            params.clone(),
            request_id.map(str::to_owned),
        ));
        Ok(match method {
            "host.status" => json!({"state":"stopped"}),
            "sync.mac.head" => json!({"mainStateId":"main-1"}),
            "sync.mac.read" => {
                json!({"kind":"file","mode":420,"data":"Z3Vlc3QgdmVyc2lvbg==","eof":true})
            }
            "sync.mac.handoff" => {
                json!({"line":"mac-conflict","stateId":"handoff-1","mainStateId":"main-1"})
            }
            _ => json!({"ok":true}),
        })
    }
}

struct AllowAdmission;
impl MessageAdmission for AllowAdmission {
    fn workspace(
        &self,
        _tx: &Transaction<'_>,
        _agent: &studio_runtime_store::AgentRecord,
    ) -> Result<Vec<Value>, coordination::CoordinationError> {
        Ok(Vec::new())
    }
}

fn seed(root: &Path) {
    let state = root.join("state");
    let project = state.join("layr-projects/p1");
    let folder = state.join("mac-folder");
    std::fs::create_dir_all(project.join("sync")).expect("project fixture");
    std::fs::create_dir_all(&folder).expect("Mac folder fixture");
    std::fs::create_dir_all(state.join("linux-vm")).expect("VM settings fixture");
    std::fs::write(state.join("linux-vm/settings.json"), json!({"cpus":2,"memoryBytes":4294967296u64,"systemDiskBytes":17179869184u64,"dataDiskBytes":137438953472u64}).to_string()).expect("settings fixture");
    std::fs::write(
        project.join("project.json"),
        json!({"projectId":"p1","source":folder}).to_string(),
    )
    .expect("project record");
    std::fs::write(folder.join("file.txt"), b"mac version").expect("Mac version");
    std::fs::write(
        project.join("sync/held.json"),
        json!({"file.txt":{"h":"old","why":"conflict"}}).to_string(),
    )
    .expect("held conflict");
    std::fs::write(project.join("sync/status.json"), json!({"state":"conflict","conflicts":["file.txt"],"choices":[],"withAgent":[],"deletes":0}).to_string()).expect("sync status");
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct ResolutionFixture {
    settings_read: Value,
    settings_set: Value,
    configured: Value,
    resolved: Value,
    events: Vec<EventFixture>,
    event_metadata: BTreeMap<String, Value>,
    guest_calls: Vec<GuestCallFixture>,
    persisted: BTreeMap<String, Value>,
    mac_file: String,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct EventFixture {
    id: String,
    agent: String,
    kind: String,
    text: String,
    #[serde(rename = "status")]
    _status: String,
    #[serde(rename = "created")]
    _created: f64,
    epoch: i64,
    #[serde(rename = "turnId")]
    _turn_id: Option<String>,
    #[serde(rename = "error")]
    _error: Option<String>,
}

#[derive(Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct GuestCallFixture {
    method: String,
    params: Value,
    request_id: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct MemoryFixture {
    error: String,
    settings_before: Value,
    settings_after: Value,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct PreflightFixture {
    preflights: Vec<Value>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Fixtures {
    vm: ResolutionFixture,
    mac: ResolutionFixture,
    agent: ResolutionFixture,
    #[serde(rename = "memory-ceiling")]
    memory: MemoryFixture,
    #[serde(rename = "write-space-preflight")]
    preflight: PreflightFixture,
}

fn fixtures() -> Fixtures {
    serde_json::from_str(include_str!("vm_macsync_fixtures.json"))
        .expect("frozen Python VM and Mac sync corpus")
}

fn runtime_database(path: &Path) -> Connection {
    let connection = Connection::open(path).expect("P03 database opens");
    connection.execute_batch(&studio_runtime_store::agent_columns::fixture_sql("CREATE TABLE groups(id TEXT PRIMARY KEY,name TEXT NOT NULL,members TEXT NOT NULL);
        CREATE TABLE graph_edges(id TEXT PRIMARY KEY,source TEXT NOT NULL,target TEXT NOT NULL,kind TEXT NOT NULL,UNIQUE(source,target,kind));
        CREATE TABLE messages(id TEXT PRIMARY KEY,room TEXT NOT NULL,author TEXT NOT NULL,text TEXT NOT NULL,at REAL NOT NULL,deliveries TEXT NOT NULL);
        CREATE TABLE runtime_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
        CREATE TABLE runtime_events(id TEXT PRIMARY KEY,agent TEXT,kind TEXT,text TEXT,status TEXT,created REAL,epoch INTEGER,turn_id TEXT,error TEXT);
        CREATE TABLE runtime_event_meta(id TEXT PRIMARY KEY,record TEXT NOT NULL);
        CREATE TABLE graph_agents(id TEXT PRIMARY KEY,record TEXT NOT NULL);
        CREATE TABLE sync_entities(collection TEXT NOT NULL,id TEXT NOT NULL,seq INTEGER NOT NULL,hash TEXT NOT NULL,payload TEXT,deleted INTEGER NOT NULL DEFAULT 0,PRIMARY KEY(collection,id));
        CREATE TABLE sync_documents(seq INTEGER PRIMARY KEY,scope TEXT NOT NULL,id TEXT NOT NULL,payload TEXT NOT NULL,deleted INTEGER NOT NULL DEFAULT 0);
        CREATE TABLE sync_versions(seq INTEGER PRIMARY KEY,scope TEXT NOT NULL,hash TEXT NOT NULL,deleted INTEGER NOT NULL,updated REAL NOT NULL);")).expect("P03 schema creates");
    connection
        .execute_batch(studio_runtime_store::agent_volatile::agent_volatile_table_sql())
        .expect("volatile agent table creates");
    let record = json!({"id":"lead","rootId":"lead","epoch":0,"status":"running","autoWake":true,"inFlight":true}).to_string();
    connection
        .execute(
            "INSERT INTO runtime_agents(id,record) VALUES('lead',?1)",
            [record],
        )
        .expect("lead inserts");
    connection
}

fn clean_time(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("at");
        object.remove("acceptedAt");
        object.remove("timing");
    }
    if let Some(object) = value.as_object_mut() {
        for value in object.values_mut() {
            clean_time(value);
        }
    } else if let Some(array) = value.as_array_mut() {
        for value in array {
            clean_time(value);
        }
    } else if let Some(text) = value.as_str()
        && text.ends_with("/state/mac-folder")
    {
        *value = json!("<mac-folder>");
    }
}

fn clean_manifest_mtime(value: &mut Value) {
    if let Some(object) = value.as_object_mut() {
        object.remove("m");
        for value in object.values_mut() {
            clean_manifest_mtime(value);
        }
    } else if let Some(array) = value.as_array_mut() {
        for value in array {
            clean_manifest_mtime(value);
        }
    }
}

#[test]
fn vm_and_mac_functions_match_frozen_corpus_for_each_resolution_choice() {
    let fixtures = fixtures();
    for (choice, expected) in [
        ("vm", fixtures.vm),
        ("mac", fixtures.mac),
        ("agent", fixtures.agent),
    ] {
        let rust_root = tempfile::tempdir().expect("Rust fixture root");
        seed(rust_root.path());

        let state = rust_root.path().join("state");
        let guest = FakeGuest(std::sync::Mutex::new(Vec::new()));
        let settings_read = serde_json::to_value(
            linux_vm_settings::get_settings(&state.join("linux-vm")).expect("Rust settings read"),
        )
        .expect("settings JSON");
        let settings_set = serde_json::to_value(
            linux_vm_settings::set_settings(
                &state.join("linux-vm"),
                linux_vm_settings::VmSettings {
                    cpus: 1,
                    memory_bytes: 4 * 1024 * 1024 * 1024,
                    system_disk_bytes: 16 * 1024 * 1024 * 1024,
                    data_disk_bytes: 128 * 1024 * 1024 * 1024,
                },
                &guest,
            )
            .expect("Rust settings write"),
        )
        .expect("settings JSON");
        let configured =
            mac_sync::configure(&state, "p1", Some(false), false).expect("Rust configure");
        let mut resolved =
            mac_sync::resolve(&state, "p1", "file.txt", choice, &guest).expect("Rust resolve");
        if choice == "agent" {
            resolved.response["leadId"] = json!("lead");
            let id = resolved.handoff_id.as_deref().expect("handoff id");
            let text = resolved.handoff_text.as_deref().expect("handoff text");
            let database = runtime_database(&rust_root.path().join("runtime.sqlite"));
            let tx = database
                .unchecked_transaction()
                .expect("P03 transaction starts");
            coordination::send_system_followup(
                &tx,
                coordination::SystemFollowup {
                    recipient_id: "lead",
                    id,
                    text,
                    delivery: studio_runtime_schema::delivery::DeliveryMoment::NextTurn,
                    now: 123.0,
                },
                &AllowAdmission,
            )
            .expect("actorless P03 follow-up writes");
            tx.commit().expect("P03 transaction commits");
        }

        assert_eq!(
            settings_read, expected.settings_read,
            "settings read ({choice})"
        );
        assert_eq!(
            settings_set, expected.settings_set,
            "settings write ({choice})"
        );
        let mut actual_configured = configured;
        let mut expected_configured = expected.configured.clone();
        clean_time(&mut actual_configured);
        clean_time(&mut expected_configured);
        assert_eq!(
            actual_configured, expected_configured,
            "config persisted state ({choice})"
        );
        let mut actual_response = resolved.response;
        let mut expected_response = expected.resolved.clone();
        clean_time(&mut actual_response);
        clean_time(&mut expected_response);
        assert_eq!(
            actual_response, expected_response,
            "resolve response ({choice})"
        );
        let rust_sync = state.join("layr-projects/p1/sync");
        for name in ["manifest.json", "held.json", "status.json", "config.json"] {
            let left = expected
                .persisted
                .get(name)
                .expect("frozen persisted file")
                .clone();
            let right: Value = serde_json::from_slice(
                &std::fs::read(rust_sync.join(name)).expect("Rust persisted file"),
            )
            .expect("Rust JSON");
            let mut left = left;
            let mut right = right;
            clean_time(&mut left);
            clean_time(&mut right);
            if choice == "vm" && name == "manifest.json" {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    let rust_mtime =
                        std::fs::metadata(rust_root.path().join("state/mac-folder/file.txt"))
                            .expect("Rust synced file metadata")
                            .mtime_nsec() as i128
                            + std::fs::metadata(rust_root.path().join("state/mac-folder/file.txt"))
                                .expect("Rust synced file metadata")
                                .mtime() as i128
                                * 1_000_000_000;
                    assert_eq!(
                        right["file.txt"]["m"]
                            .as_i64()
                            .expect("Rust manifest mtime") as i128,
                        rust_mtime
                    );
                }
                clean_manifest_mtime(&mut left);
                clean_manifest_mtime(&mut right);
            }
            assert_eq!(left, right, "persisted {name} ({choice})");
        }
        assert_eq!(
            expected.mac_file.as_bytes(),
            std::fs::read(rust_root.path().join("state/mac-folder/file.txt"))
                .expect("Rust Mac file"),
            "Mac file ({choice})"
        );
        let frozen_sync_calls: Vec<Value> = expected
            .guest_calls
            .iter()
            .filter(|call| call.method.starts_with("sync.mac."))
            .map(|call| serde_json::to_value(call).expect("guest call JSON"))
            .collect();
        let rust_sync_calls: Vec<Value> = guest
            .0
            .lock()
            .expect("guest calls")
            .iter()
            .filter(|call| call.0.starts_with("sync.mac."))
            .map(|(method, params, id)| json!({"method":method,"params":params,"requestId":id}))
            .collect();
        assert_eq!(
            rust_sync_calls, frozen_sync_calls,
            "fake guest RPC corpus ({choice})"
        );
        if choice == "agent" {
            let expected_event = &expected.events[0];
            let expected_meta = &expected.event_metadata[&expected_event.id];
            let database = Connection::open(rust_root.path().join("runtime.sqlite"))
                .expect("read Rust P03 database");
            let (id, agent, kind, text, status, epoch): (
                String,
                String,
                String,
                String,
                String,
                i64,
            ) = database
                .query_row(
                    "SELECT id,agent,kind,text,status,epoch FROM runtime_events",
                    [],
                    |row| {
                        Ok((
                            row.get(0)?,
                            row.get(1)?,
                            row.get(2)?,
                            row.get(3)?,
                            row.get(4)?,
                            row.get(5)?,
                        ))
                    },
                )
                .expect("Rust P03 row");
            assert_eq!(id, expected_event.id);
            assert_eq!(agent, expected_event.agent);
            assert_eq!(kind, expected_event.kind);
            assert_eq!(text, expected_event.text);
            assert_eq!(status, "pending");
            assert_eq!(epoch, expected_event.epoch);
            let metadata: String = database
                .query_row(
                    "SELECT record FROM runtime_event_meta WHERE id=?1",
                    params![id],
                    |row| row.get(0),
                )
                .expect("Rust P03 metadata");
            let mut rust_meta: Value = serde_json::from_str(&metadata).expect("Rust metadata JSON");
            let mut python_meta = expected_meta.clone();
            clean_time(&mut rust_meta);
            clean_time(&mut python_meta);
            assert_eq!(
                rust_meta, python_meta,
                "actorless queued P03 metadata matches Runtime.send"
            );
        }
    }
}

#[test]
fn darwin_memory_over_the_host_ceiling_is_rejected_before_persisting() {
    let rust_root = tempfile::tempdir().expect("Rust fixture root");
    seed(rust_root.path());
    let expected = fixtures().memory;

    let settings_dir = rust_root.path().join("state/linux-vm");
    let before = std::fs::read(settings_dir.join("settings.json")).expect("seeded settings");
    let guest = FakeGuest(std::sync::Mutex::new(Vec::new()));
    let error = linux_vm_settings::set_settings_with_macos_memory(
        &settings_dir,
        linux_vm_settings::VmSettings {
            cpus: 1,
            memory_bytes: 9 * 1024 * 1024 * 1024,
            system_disk_bytes: 16 * 1024 * 1024 * 1024,
            data_disk_bytes: 128 * 1024 * 1024 * 1024,
        },
        &guest,
        16 * 1024 * 1024 * 1024,
    )
    .expect_err("settings over Darwin host limit are rejected");

    assert_eq!(error.to_string(), expected.error);
    assert_eq!(
        serde_json::from_slice::<Value>(&before).expect("Rust seed JSON"),
        expected.settings_before
    );
    assert_eq!(
        std::fs::read(settings_dir.join("settings.json")).expect("unchanged Rust settings"),
        before,
        "invalid Darwin setting must not be persisted"
    );
    assert_eq!(
        serde_json::from_slice::<Value>(
            &std::fs::read(settings_dir.join("settings.json")).expect("persisted Rust settings"),
        )
        .expect("Rust settings JSON"),
        expected.settings_after
    );
    assert!(guest.0.lock().expect("guest call log").is_empty());
}

struct EmptyDisk;

impl GuestSpaceProbe for EmptyDisk {
    fn free_bytes(&self, _path: &Path) -> std::io::Result<u64> {
        Ok(0)
    }
}

#[cfg(unix)]
#[test]
fn write_space_preflight_refuses_before_opening_the_guest_socket() {
    use std::os::unix::net::UnixListener;

    let rust_root = tempfile::tempdir().expect("Rust fixture root");
    let expected = fixtures().preflight;

    let mut actual_preflights = Vec::new();
    for (index, method) in [
        "project.import",
        "line.branch",
        "upload.begin",
        "upload.commit",
        "sync.push",
    ]
    .into_iter()
    .enumerate()
    {
        let state_dir = rust_root.path().join(format!("state/linux-vm-{index}"));
        std::fs::create_dir_all(&state_dir).expect("VM state directory");
        let rpc = UnixGuestRpc::with_space_probe(&state_dir, Arc::new(EmptyDisk));
        let listener = UnixListener::bind(rpc.socket_path()).expect("fake guest endpoint binds");
        listener
            .set_nonblocking(true)
            .expect("fake endpoint is nonblocking");
        let error = rpc
            .call(
                method,
                &json!({"agentId":"agent-1"}),
                Some(&format!("request-{method}")),
                Duration::from_secs(2),
            )
            .expect_err("write below free-space floor is refused");
        let channel_opened = match listener.accept() {
            Ok(_) => true,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => false,
            Err(error) => panic!("fake endpoint accept failed: {error}"),
        };
        assert!(matches!(error, GuestError::SpacePreflight { .. }));
        assert!(!error.uncertain(), "preflight refusal is known not sent");
        actual_preflights.push(json!({
            "method": method,
            "error": error.to_string(),
            "channelOpened": channel_opened,
        }));
    }

    assert_eq!(
        actual_preflights, expected.preflights,
        "Python and Rust refuse all disk-consuming guest RPCs before socket connect"
    );
}
