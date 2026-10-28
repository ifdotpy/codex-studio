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
    pub canvas_sql: String,
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
            // Receipt rows in the frozen snapshot predate the cutover. Tests
            // create current receipt owners through their caller or migrate
            // the individual legacy row they exercise.
            let sql = fixture()
                .canvas_sql
                .lines()
                .filter(|line| {
                    !line.contains("runtime_access_receipts")
                        && !line.starts_with("INSERT INTO \"runtime_server_inbox\"")
                        && !line.starts_with("INSERT INTO \"runtime_server_outbox\"")
                })
                .collect::<Vec<_>>()
                .join("\n");
            tx.execute_batch(&sql)?;
            Ok::<_, studio_runtime_store::StoreError>(())
        })
        .await
        .expect("restore frozen multi-server rows");
    (root, store)
}

/// Restore classifiable frozen transport rows in their source-era tables and
/// run the real receipt migration. The unproven python-location row has its
/// own migration-stop witness and is not mixed into this successful fixture.
pub async fn migrated_store() -> (tempfile::TempDir, Arc<RuntimeStore>) {
    let (root, store) = store().await;
    drop(store);
    {
        let db = rusqlite::Connection::open(root.path().join("canvas.sqlite3")).expect("source");
        db.execute_batch("DELETE FROM runtime_payload_migrations WHERE name LIKE 'receipts:%';
            CREATE TABLE runtime_access_receipts(client TEXT,request_id TEXT,hash TEXT,session TEXT,status TEXT,record TEXT,PRIMARY KEY(client,request_id));
            DROP TABLE runtime_server_inbox; DROP TABLE runtime_server_outbox;
            CREATE TABLE runtime_server_inbox(id TEXT PRIMARY KEY,signature TEXT NOT NULL,state TEXT NOT NULL,result TEXT,created_at REAL NOT NULL DEFAULT 0,completed_at REAL NOT NULL DEFAULT 0,principal TEXT,actor TEXT);
            CREATE TABLE runtime_server_outbox(id TEXT PRIMARY KEY,server TEXT NOT NULL,body TEXT NOT NULL,state TEXT NOT NULL,result TEXT,attempts INTEGER NOT NULL DEFAULT 0,next_at REAL NOT NULL DEFAULT 0,error TEXT,created_at REAL NOT NULL DEFAULT 0,completed_at REAL NOT NULL DEFAULT 0,fingerprint TEXT);")
            .expect("source-era receipt tables");
        for line in fixture().canvas_sql.lines().filter(|line| {
            (line.starts_with("INSERT INTO \"runtime_access_receipts\"")
                || line.starts_with("INSERT INTO \"runtime_server_inbox\"")
                || line.starts_with("INSERT INTO \"runtime_server_outbox\""))
                && !line
                    .starts_with("INSERT INTO \"runtime_server_outbox\" VALUES('python-location'")
        }) {
            db.execute_batch(line).expect("frozen source row");
        }
    }
    let store = Arc::new(
        RuntimeStore::open(Some(root.path()))
            .await
            .expect("migrate frozen receipts"),
    );
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
#[derive(serde::Serialize, serde::Deserialize)]
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

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairBody {
    client_id: String,
    invite_id: String,
    kind: String,
    label: String,
    origin: Option<String>,
    protocol: u64,
    public_key: String,
    request_id: String,
    token: String,
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct AutoPairBody {
    label: String,
    origin: String,
    protocol: u64,
    public_key: String,
    request_id: String,
    server_id: String,
}

#[derive(serde::Deserialize)]
struct SigningCase {
    name: String,
    raw: String,
}
#[derive(serde::Deserialize)]
struct SigningData {
    auth: Vec<SigningCase>,
}

/// Sign a current UUIDv7 root with the frozen test identity.
pub fn current_pair(root: &str, nonce: &str, now: i64) -> SignedRequest {
    let data: SigningData = serde_json::from_value(fixture().data.clone()).expect("fixture cases");
    let mut body: PairBody =
        serde_json::from_str(&data.auth.first().expect("pair case").raw).expect("pair body");
    body.request_id = root.to_owned();
    let raw = serde_json::to_vec(&body).expect("pair JSON");
    current_signed(
        "/api/multi-server/v1/pair",
        &body.client_id,
        root,
        nonce,
        now,
        &raw,
    )
}

pub fn current_auto_pair(root: &str, nonce: &str, now: i64) -> SignedRequest {
    let data: SigningData = serde_json::from_value(fixture().data.clone()).expect("fixture cases");
    let case = data
        .auth
        .iter()
        .find(|case| case.name == "auto-pair")
        .expect("auto pair case");
    let mut body: AutoPairBody = serde_json::from_str(&case.raw).expect("auto pair body");
    body.request_id = root.to_owned();
    let raw = serde_json::to_vec(&body).expect("auto pair JSON");
    current_signed(
        "/api/multi-server/v1/auto-pair",
        &body.server_id,
        root,
        nonce,
        now,
        &raw,
    )
}

fn current_signed(
    target: &str,
    client: &str,
    root: &str,
    nonce: &str,
    now: i64,
    raw: &[u8],
) -> SignedRequest {
    use studio_runtime_services::remote_transport::{
        MultiServerSigningInput, multi_server_signing_bytes,
    };
    let timestamp = now.to_string();
    let bytes = multi_server_signing_bytes(MultiServerSigningInput {
        method: "POST",
        target,
        server_id: "local-server",
        client_id: client,
        timestamp: &timestamp,
        nonce,
        request_id: root,
        body: raw,
    });
    let signature = signing_identity().sign(&bytes);
    let headers = vec![
        ("X-Studio-Client".into(), client.into()),
        ("X-Studio-Server".into(), "local-server".into()),
        ("X-Studio-Timestamp".into(), timestamp),
        ("X-Studio-Nonce".into(), nonce.into()),
        ("X-Studio-Request-Id".into(), root.into()),
        ("X-Studio-Signature".into(), signature),
    ];
    SignedRequest::from_headers("POST", target, &headers, raw).expect("signed request")
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
        "request_results",
        "runtime_outbox",
        "runtime_access_audit",
        "runtime_server_inbox",
        "runtime_server_outbox",
    ] {
        let sql=match table {
            "runtime_access_nonces"=>"SELECT json_array(client,nonce,created) FROM runtime_access_nonces ORDER BY client,nonce".to_owned(),
            "request_results"=>"SELECT json_array(id,signature,result,created) FROM request_results ORDER BY id".to_owned(),
            "runtime_outbox"=>"SELECT json_array(id,kind,status,payload) FROM runtime_outbox ORDER BY id".to_owned(),
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
