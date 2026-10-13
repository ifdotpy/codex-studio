use std::path::PathBuf;
use std::process::Command;
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use studio_runtime_services::multi_server_auth::{AccessError, OwnerObservation, SignedRequest};
use studio_runtime_services::multi_server_pairing::{MultiServerBoundary, OwnerLookup};
use studio_runtime_store::RuntimeStore;

pub struct Fixture {
    pub root: tempfile::TempDir,
    pub data: Value,
}
static FIXTURE: OnceLock<Fixture> = OnceLock::new();
fn test_directory() -> tempfile::TempDir {
    let root = std::env::var_os("MULTI_SERVER_TEST_TMPDIR")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir);
    tempfile::Builder::new()
        .prefix("multi-server-")
        .tempdir_in(root)
        .expect("isolated fixture root")
}
pub fn fixture() -> &'static Fixture {
    FIXTURE.get_or_init(|| {
        let root = test_directory();
        let python = std::env::var_os("CODEX_AGENTS_PYTHON").unwrap_or_else(|| {
            let resolver = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../../apps/server/src/codex_python.py");
            let output = Command::new("python3")
                .arg(resolver)
                .output()
                .expect("resolve prepared Python");
            assert!(
                output.status.success(),
                "prepare the isolated API interpreter with install-cli.py"
            );
            String::from_utf8(output.stdout)
                .expect("Python path")
                .trim()
                .into()
        });
        let script = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("packages")
            .join("runtime-services/tests/multi_server/oracle.py");
        let mut command = Command::new(python);
        if env!("CARGO_PKG_NAME") == "studio-runtime-http" {
            command.env(
                "MULTI_SERVER_ORACLE_NOW",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("clock")
                    .as_secs()
                    .to_string(),
            );
        }
        let output = command
            .arg(script)
            .arg(root.path())
            .env("PYTHONDONTWRITEBYTECODE", "1")
            .output()
            .expect("run isolated Python writer");
        assert!(
            output.status.success(),
            "Python writer: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let data = serde_json::from_slice(&output.stdout).expect("oracle JSON");
        Fixture { root, data }
    })
}
pub fn store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let template = fixture();
    let root = test_directory();
    for name in ["canvas.sqlite3", "analytics.sqlite3"] {
        std::fs::copy(template.root.path().join(name), root.path().join(name))
            .expect("clone prepared Python state");
    }
    let store = Arc::new(RuntimeStore::open(Some(root.path())).expect("open Python-written state"));
    (root, store)
}
pub fn request(case: &Value) -> Result<SignedRequest, AccessError> {
    let headers =
        serde_json::from_value::<Vec<(String, String)>>(case["headers"].clone()).expect("headers");
    SignedRequest::from_headers(
        case["method"].as_str().expect("method"),
        case["target"].as_str().expect("target"),
        &headers,
        case["raw"].as_str().expect("raw").as_bytes(),
    )
}
pub struct Owner;
impl OwnerLookup for Owner {
    fn observe(
        &self,
        address: &str,
        logins: &[String],
        _origin: Option<&str>,
    ) -> Result<(OwnerObservation, Option<Value>), AccessError> {
        let data = &fixture().data;
        Ok((
            OwnerObservation::from_json(address, logins, &data["status"], &data["whois"])?,
            None,
        ))
    }
}
pub fn boundary() -> MultiServerBoundary {
    MultiServerBoundary::new(fixture().data["identity"].clone(), Arc::new(Owner)).expect("boundary")
}
pub fn owner() -> OwnerObservation {
    OwnerObservation::from_json(
        "100.64.0.2",
        &[],
        &fixture().data["status"],
        &fixture().data["whois"],
    )
    .expect("owner observation")
}
pub fn rows(store: &RuntimeStore) -> Value {
    let db = store.read().expect("read");
    let mut output = serde_json::Map::new();
    for table in [
        "runtime_access_clients",
        "runtime_access_invites",
        "runtime_access_nonces",
        "runtime_access_receipts",
        "runtime_access_audit",
        "runtime_server_inbox",
        "runtime_server_outbox",
    ] {
        let sql=match table {
            "runtime_access_nonces"=>"SELECT json_array(client,nonce,created) FROM runtime_access_nonces ORDER BY client,nonce".to_owned(),
            "runtime_access_receipts"=>"SELECT json_array(client,request_id,hash,session,status,record) FROM runtime_access_receipts ORDER BY client,request_id".to_owned(),
            "runtime_access_audit"=>"SELECT json_array(seq,client,actor,action,created) FROM runtime_access_audit ORDER BY seq".to_owned(),
            "runtime_server_inbox"=>"SELECT json_array(id,signature,state,result,created_at,completed_at,principal,actor) FROM runtime_server_inbox ORDER BY id".to_owned(),
            "runtime_server_outbox"=>"SELECT json_array(id,server,body,state,result,fingerprint,attempts,created_at,completed_at) FROM runtime_server_outbox ORDER BY id".to_owned(),
            _=>format!("SELECT json_array(id,record) FROM {table} ORDER BY id"),
        };
        let mut statement = db.prepare(&sql).expect("prepare");
        let rows = statement
            .query_map([], |row| row.get::<_, String>(0))
            .expect("rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect");
        output.insert(table.to_owned(), serde_json::json!(rows));
    }
    Value::Object(output)
}

/// Counts every host invocation, including attempted auto-pair probes.
pub struct CountingOwner {
    pub calls: std::sync::atomic::AtomicUsize,
    pub probes: std::sync::atomic::AtomicUsize,
    pub case: Value,
}
impl CountingOwner {
    pub fn new(case: Value) -> Arc<Self> {
        Arc::new(Self {
            calls: 0.into(),
            probes: 0.into(),
            case,
        })
    }
}
impl OwnerLookup for CountingOwner {
    fn observe(
        &self,
        address: &str,
        logins: &[String],
        origin: Option<&str>,
    ) -> Result<(OwnerObservation, Option<Value>), AccessError> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        if origin.is_some() {
            self.probes
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
        Ok((
            OwnerObservation::from_json(
                address,
                logins,
                &fixture().data["status"],
                &self.case["whois"],
            )?,
            self.case.get("probe").cloned(),
        ))
    }
}
