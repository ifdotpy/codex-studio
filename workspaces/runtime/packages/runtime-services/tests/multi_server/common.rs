use futures_util::future::BoxFuture;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use serde_json::Value;
use studio_runtime_services::multi_server_auth::{AccessError, OwnerObservation, SignedRequest};
use studio_runtime_services::multi_server_pairing::{MultiServerBoundary, OwnerLookup};
use studio_runtime_store::RuntimeStore;

#[derive(serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Fixture {
    canvas_sql: String,
    signing_identity: Box<serde_json::value::RawValue>,
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
        serde_json::from_str(include_str!("fixture.json")).expect("frozen multi-server fixture")
    })
}
pub async fn store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let root = test_directory();
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("open fixture state"),
    );
    store
        .transaction_with(|tx| {
            tx.execute_batch(&fixture().canvas_sql)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("restore frozen multi-server rows");
    (root, store)
}

/// The disposable oracle key only signs test requests at the HTTP wall clock.
#[allow(clippy::disallowed_methods)]
pub fn signing_identity() -> &'static studio_runtime_services::remote_transport::RemoteIdentity {
    static IDENTITY: OnceLock<studio_runtime_services::remote_transport::RemoteIdentity> =
        OnceLock::new();
    IDENTITY.get_or_init(|| {
        let root = test_directory();
        let directory = root.path().join("multi-server");
        std::fs::create_dir(&directory).expect("fixture identity directory");
        let path = directory.join("identity.json");
        std::fs::write(&path, fixture().signing_identity.get()).expect("fixture identity");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o700))
                .expect("private identity directory");
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))
                .expect("private identity file");
        }
        studio_runtime_services::remote_transport::load_multi_server_identity(root.path())
            .expect("frozen signing identity")
    })
}
/// Request fields shared by the service corpus and HTTP caller; cases also carry expectations.
#[derive(serde::Deserialize)]
pub struct FixtureRequest {
    pub method: String,
    pub target: String,
    pub raw: String,
    pub headers: Vec<(String, String)>,
}

pub fn request(case: &Value) -> Result<SignedRequest, AccessError> {
    let case: FixtureRequest = serde_json::from_value(case.clone()).expect("fixture request");
    SignedRequest::from_headers(
        &case.method,
        &case.target,
        &case.headers,
        case.raw.as_bytes(),
    )
}
pub struct Owner;
impl OwnerLookup for Owner {
    fn observe<'a>(
        &'a self,
        address: &'a str,
        logins: &'a [String],
        _origin: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(OwnerObservation, Option<Value>), AccessError>> {
        Box::pin(async move {
            let data = &fixture().data;
            Ok((
                OwnerObservation::from_json(address, logins, &data["status"], &data["whois"])?,
                None,
            ))
        })
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
    let db = store.test_reader().expect("read");
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
    fn observe<'a>(
        &'a self,
        address: &'a str,
        logins: &'a [String],
        origin: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(OwnerObservation, Option<Value>), AccessError>> {
        Box::pin(async move {
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
        })
    }
}
