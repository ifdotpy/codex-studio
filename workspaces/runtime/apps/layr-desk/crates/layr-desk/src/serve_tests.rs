//! The gateway (`layr-desk serve`) with a fake bucket: the challenge proves the vault identity,
//! the token is short and bound to one vault, only state objects and passkey slots are
//! reachable, and the host is checked. Then the certificate and its reload, and the AWS client
//! against a fake AWS that checks each signature.

use crate::aws::Aws;
use crate::routes::{Config, Gateway};
use crate::serve::serve;
use crate::tls;
use crate::vault::{Body, Bucket, Clock, Object, Vault};
use anyhow::Result;
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use bytes::Bytes;
use desk_core::keys;
use http_body_util::{BodyExt, Full};
use hyper::header::HeaderMap;
use hyper::Request;
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio_rustls::TlsAcceptor;

const TREE: &str = "0000000001.aaaaaaaa.tree.age";
const HOST: &str = "desk.example";

fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into())
        .map_err(|e| match e {})
        .boxed_unsync()
}

#[derive(Clone, Default)]
struct FakeBucket(Arc<Mutex<BTreeMap<String, Vec<u8>>>>);

impl FakeBucket {
    fn has(&self, key: &str) -> bool {
        self.0.lock().unwrap().contains_key(key)
    }
}

impl Bucket for FakeBucket {
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let objects = self.0.lock().unwrap();
        Ok(objects
            .keys()
            .filter(|k| k.starts_with(prefix))
            .cloned()
            .collect())
    }

    async fn get(&self, key: &str) -> Result<Option<Object>> {
        let objects = self.0.lock().unwrap();
        Ok(objects.get(key).map(|data| Object {
            length: Some(data.len() as u64),
            body: full(data.clone()),
        }))
    }

    async fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        let mut objects = self.0.lock().unwrap();
        if objects.contains_key(key) {
            return Ok(false);
        }
        objects.insert(key.to_string(), body);
        Ok(true)
    }

    async fn delete(&self, key: &str) -> Result<()> {
        self.0.lock().unwrap().remove(key);
        Ok(())
    }
}

/// A folder below the temporary folder, removed at the end.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let path = std::env::temp_dir().join(format!(
            "layr-desk-{name}-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).unwrap();
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

struct Reply {
    status: u16,
    headers: HeaderMap,
    body: Vec<u8>,
}

impl Reply {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.body).into_owned()
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or(Value::Null)
    }

    fn header(&self, name: &str) -> &str {
        self.headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    }
}

/// One request on its own connection; the path goes out exactly as given.
async fn request_on<S>(
    io: S,
    method: &str,
    path: &str,
    host: &str,
    token: Option<&str>,
    body: Option<&str>,
) -> Reply
where
    S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let (mut sender, connection) = hyper::client::conn::http1::handshake(TokioIo::new(io))
        .await
        .unwrap();
    tokio::spawn(connection);
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .header("host", host);
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    if body.is_some() {
        request = request.header("content-type", "application/json");
    }
    let request = request
        .body(Full::new(Bytes::from(body.unwrap_or_default().to_string())))
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    let status = response.status().as_u16();
    let headers = response.headers().clone();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    Reply {
        status,
        headers,
        body: body.to_vec(),
    }
}

struct Setup {
    address: SocketAddr,
    identity: String,
    bucket: FakeBucket,
    clock: Arc<AtomicU64>,
    _site: Scratch,
}

async fn setup(tls: Option<TlsAcceptor>) -> Setup {
    let clock = Arc::new(AtomicU64::new(1_000_000));
    let identity = keys::generate_identity();
    let recipient = keys::recipient_of(&identity).unwrap();
    let bucket = FakeBucket::default();
    for (key, data) in [
        (format!("p/states/{TREE}"), "tree"),
        ("p/slots/paper.age".into(), "paper-slot"),
        ("q/states/0000000001.bbbbbbbb.tree.age".into(), "other"),
    ] {
        bucket.put_new(&key, data.into()).await.unwrap();
    }
    let secret = Arc::new([7u8; 32]);
    let time = clock.clone();
    let now: Clock = Arc::new(move || time.load(Ordering::SeqCst));
    let vaults: HashMap<String, Arc<Vault<FakeBucket>>> = ["p", "q"]
        .into_iter()
        .map(|prefix| {
            let vault = Vault::new(
                bucket.clone(),
                prefix,
                &recipient,
                secret.clone(),
                now.clone(),
            );
            (prefix.to_string(), Arc::new(vault))
        })
        .collect();
    let site = Scratch::new("site");
    std::fs::create_dir(site.0.join("assets")).unwrap();
    std::fs::write(site.0.join("index.html"), "<!doctype html>index").unwrap();
    std::fs::write(site.0.join("assets/app-1.js"), "js").unwrap();
    let config = Arc::new(Config {
        host: Some(HOST.into()),
        listen: None,
        projects: [("p".to_string(), json!({})), ("q".to_string(), json!({}))]
            .into_iter()
            .collect(),
    });
    let gateway = Arc::new(Gateway {
        config: Box::new(move || Ok(config.clone())),
        vault_for: Box::new(move |name, _| Ok(vaults.get(name).cloned())),
        site: site.0.clone(),
    });
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(serve(listener, tls, gateway));
    Setup {
        address,
        identity,
        bucket,
        clock,
        _site: site,
    }
}

impl Setup {
    async fn on_host(
        &self,
        host: &str,
        method: &str,
        path: &str,
        token: Option<&str>,
        body: Option<&str>,
    ) -> Reply {
        let stream = TcpStream::connect(self.address).await.unwrap();
        request_on(stream, method, path, host, token, body).await
    }

    async fn call(&self, method: &str, path: &str, token: Option<&str>) -> Reply {
        self.on_host(HOST, method, path, token, None).await
    }

    async fn status(&self, method: &str, path: &str, token: Option<&str>) -> u16 {
        self.call(method, path, token).await.status
    }

    async fn token(&self, project: &str) -> String {
        let reply = self
            .call("GET", &format!("/api/p/{project}/challenge"), None)
            .await;
        let sealed = B64
            .decode(reply.json()["challenge"].as_str().unwrap())
            .unwrap();
        String::from_utf8(keys::decrypt(&self.identity, &sealed).unwrap()).unwrap()
    }
}

#[tokio::test]
async fn the_holder_of_the_vault_identity_lists_and_reads_state_objects() {
    let s = setup(None).await;
    let tk = s.token("p").await;
    let listed = s.call("GET", "/api/p/p/states", Some(&tk)).await;
    assert_eq!(listed.status, 200);
    assert_eq!(listed.json()["states"], json!([TREE]));
    let object = s
        .call("GET", &format!("/api/p/p/states/{TREE}"), Some(&tk))
        .await;
    assert_eq!(object.status, 200);
    assert_eq!(object.text(), "tree");
    assert_eq!(object.header("content-length"), "4");
    assert_eq!(object.header("cache-control"), "private, no-store");
    assert!(object
        .header("content-security-policy")
        .contains("default-src 'self'"));
    assert!(object
        .header("content-security-policy")
        .contains("frame-src blob:"));
    assert_eq!(
        object.header("strict-transport-security"),
        "max-age=63072000"
    );
    for name in [
        "..%2Fslots%2Fpaper.age",
        "paper.age",
        "0000000001.aaaaaaaa.tree.age.bak",
    ] {
        let path = format!("/api/p/p/states/{name}");
        assert_eq!(s.status("GET", &path, Some(&tk)).await, 400, "{name}");
    }
    let missing = "/api/p/p/states/0000000002.aaaaaaaa.tree.age";
    assert_eq!(s.status("GET", missing, Some(&tk)).await, 404);
}

#[tokio::test]
async fn a_token_opens_only_its_own_vault_and_expires() {
    let s = setup(None).await;
    let tk = s.token("p").await;
    let (expiry, _) = tk.split_once('.').unwrap();
    let forged = format!("{expiry}.{}", "0".repeat(64));
    for bad in [None, Some(""), Some("123.abc"), Some(forged.as_str())] {
        assert_eq!(
            s.status("GET", "/api/p/p/states", bad).await,
            401,
            "{bad:?}"
        );
    }
    assert_eq!(s.status("GET", "/api/p/q/states", Some(&tk)).await, 401);
    assert_eq!(s.status("GET", "/api/p/p/states", Some(&tk)).await, 200);
    s.clock.fetch_add(11 * 60 * 1000, Ordering::SeqCst);
    assert_eq!(s.status("GET", "/api/p/p/states", Some(&tk)).await, 401);
}

#[tokio::test]
async fn slots_are_open_to_read_and_only_passkey_slots_change_once() {
    let s = setup(None).await;
    let slots = s.call("GET", "/api/p/p/slots", None).await;
    assert_eq!(
        slots.json(),
        json!({"slots": [{"name": "paper", "data": B64.encode("paper-slot")}]})
    );
    let tk = s.token("p").await;
    let body = json!({ "data": B64.encode("slot") }).to_string();
    let add = || {
        s.on_host(
            HOST,
            "PUT",
            "/api/p/p/slots/passkey-abc_1",
            Some(&tk),
            Some(&body),
        )
    };
    let added = add().await;
    assert_eq!(
        (added.status, added.json()),
        (200, json!({"added": "passkey-abc_1"}))
    );
    let again = add().await;
    assert_eq!(again.status, 409);
    assert_eq!(
        again.json()["error"],
        "this passkey already opens the vault"
    );
    for name in ["paper", "passkey-", "passkey-a.b"] {
        let path = format!("/api/p/p/slots/{name}");
        let put = s.on_host(HOST, "PUT", &path, Some(&tk), Some(&body)).await;
        assert_eq!(put.status, 400, "{name}");
        assert_eq!(s.status("DELETE", &path, Some(&tk)).await, 400, "{name}");
    }
    let path = "/api/p/p/slots/passkey-x";
    assert_eq!(
        s.on_host(HOST, "PUT", path, None, Some(&body)).await.status,
        401
    );
    let removed = s
        .call("DELETE", "/api/p/p/slots/passkey-abc_1", Some(&tk))
        .await;
    assert_eq!(
        (removed.status, removed.json()),
        (200, json!({"removed": "passkey-abc_1"}))
    );
    assert!(s.bucket.has("p/slots/paper.age"));
    assert!(!s.bucket.has("p/slots/passkey-abc_1.age"));
}

#[tokio::test]
async fn bodies_are_small_json_with_a_slot_of_1_to_4096_bytes() {
    let s = setup(None).await;
    let tk = s.token("p").await;
    let put = |body: String| {
        let tk = tk.clone();
        let s = &s;
        async move {
            s.on_host(
                HOST,
                "PUT",
                "/api/p/p/slots/passkey-b",
                Some(&tk),
                Some(&body),
            )
            .await
        }
    };
    assert_eq!(put("x".repeat(17 * 1024)).await.status, 413);
    assert_eq!(put("{not json".into()).await.status, 400);
    assert_eq!(put("{}".into()).await.status, 400);
    let big = json!({ "data": B64.encode(vec![1u8; 4097]) }).to_string();
    assert_eq!(put(big).await.status, 400);
    // The site sends standard base64; URL-safe base64 without padding works too.
    let url_safe = json!({ "data": "-_8" }).to_string();
    assert_eq!(put(url_safe).await.status, 200);
    assert_eq!(
        s.bucket.0.lock().unwrap()["p/slots/passkey-b.age"],
        vec![0xfb, 0xff]
    );
}

#[tokio::test]
async fn the_site_the_project_list_and_the_host_check() {
    let s = setup(None).await;
    assert_eq!(
        s.call("GET", "/api/projects", None).await.json(),
        json!({"projects": ["p", "q"]})
    );
    for path in ["/", "/p/", "/p/anything", "/p/a/../b"] {
        let page = s.call("GET", path, None).await;
        assert_eq!(page.text(), "<!doctype html>index", "{path}");
        assert_eq!(page.header("cache-control"), "no-cache");
        assert_eq!(page.header("content-type"), "text/html; charset=utf-8");
    }
    let asset = s.call("GET", "/assets/app-1.js", None).await;
    assert_eq!(asset.text(), "js");
    assert_eq!(asset.header("content-type"), "text/javascript");
    assert!(asset.header("cache-control").contains("immutable"));
    assert_eq!(asset.header("x-frame-options"), "DENY");
    let head = s.call("HEAD", "/assets/app-1.js", None).await;
    assert_eq!((head.status, head.body.len()), (200, 0));
    assert_eq!(head.header("content-length"), "2");
    assert_eq!(s.status("GET", "/assets/missing.js", None).await, 404);
    assert_eq!(s.status("GET", "/assets/app-1.js/", None).await, 404);
    assert_eq!(s.status("GET", "/%2e%2e/etc/passwd.txt", None).await, 404);
    assert_eq!(
        s.status("GET", "/assets/..%2F..%2Fetc.txt", None).await,
        404
    );
    assert_eq!(s.status("GET", "/api/p/nope/slots", None).await, 404);
    assert_eq!(s.status("GET", "/api/p/p/slots/x/y/z", None).await, 404);
    assert_eq!(s.status("GET", "/api/p/p/other", None).await, 401);
    assert_eq!(s.status("GET", "/api/p/%E0%A4%A/slots", None).await, 400);
    assert_eq!(s.status("GET", "/bad%zz.js", None).await, 400);
    assert_eq!(s.status("POST", "/", None).await, 405);
    let wrong = s.on_host("evil.example", "GET", "/", None, None).await;
    assert_eq!(
        (wrong.status, wrong.json()),
        (421, json!({"error": "wrong host name"}))
    );
    let with_port = s.on_host("desk.example:443", "GET", "/", None, None).await;
    assert_eq!(with_port.status, 200);
}

/// A CA, and certificates for the host that it signs.
struct Authority {
    cert: rcgen::Certificate,
    key: rcgen::KeyPair,
}

impl Authority {
    fn new() -> Self {
        let key = rcgen::KeyPair::generate().unwrap();
        let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
        params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
        Authority {
            cert: params.self_signed(&key).unwrap(),
            key,
        }
    }

    /// PEM certificate, PEM key, and the certificate as the client sees it.
    fn issue(&self) -> (String, String, Vec<u8>) {
        let key = rcgen::KeyPair::generate().unwrap();
        let cert = rcgen::CertificateParams::new(vec![HOST.to_string()])
            .unwrap()
            .signed_by(&key, &self.cert, &self.key)
            .unwrap();
        (cert.pem(), key.serialize_pem(), cert.der().to_vec())
    }
}

/// Write a file with a modification time `seconds` from now, as a renewal would.
fn write_at(path: &Path, text: &str, seconds: u64) {
    std::fs::write(path, text).unwrap();
    let file = std::fs::File::options().write(true).open(path).unwrap();
    file.set_modified(SystemTime::now() + Duration::from_secs(seconds))
        .unwrap();
}

#[tokio::test]
async fn tls_serves_the_certificate_and_reads_it_again_when_it_changes() {
    let authority = Authority::new();
    let folder = Scratch::new("tls");
    let (cert, key) = (folder.0.join("tls.crt"), folder.0.join("tls.key"));
    let (first_pem, first_key, first) = authority.issue();
    write_at(&cert, &first_pem, 0);
    write_at(&key, &first_key, 0);
    let resolver =
        Arc::new(tls::Reloading::new(cert.clone(), key.clone(), tls::provider()).unwrap());
    let s = setup(Some(tls::acceptor(resolver).unwrap())).await;

    let mut roots = rustls::RootCertStore::empty();
    roots.add(authority.cert.der().clone()).unwrap();
    let mut client = rustls::ClientConfig::builder_with_provider(tls::provider())
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_root_certificates(roots)
        .with_no_client_auth();
    // A resumed session shows the certificate of its first handshake.
    client.resumption = rustls::client::Resumption::disabled();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client));
    let connect = || async {
        let tcp = TcpStream::connect(s.address).await.unwrap();
        let name = rustls::pki_types::ServerName::try_from(HOST).unwrap();
        let stream = connector.connect(name, tcp).await.unwrap();
        let presented = stream.get_ref().1.peer_certificates().unwrap()[0].to_vec();
        (stream, presented)
    };

    let (stream, presented) = connect().await;
    assert_eq!(presented, first);
    let page = request_on(stream, "GET", "/", HOST, None, None).await;
    assert_eq!(page.text(), "<!doctype html>index");
    let (stream, _) = connect().await;
    let wrong = request_on(stream, "GET", "/", "evil.example", None, None).await;
    assert_eq!(wrong.status, 421);

    // A renewed certificate is served from the next connection on.
    let (second_pem, second_key, second) = authority.issue();
    write_at(&cert, &second_pem, 10);
    write_at(&key, &second_key, 10);
    assert_eq!(connect().await.1, second);

    // A certificate without its key (one file written, the other not yet) keeps the old pair.
    let (third_pem, _, _) = authority.issue();
    write_at(&cert, &third_pem, 20);
    assert_eq!(connect().await.1, second);
}

#[test]
fn the_tailscale_name_comes_from_status() {
    let status = r#"{"BackendState": "Running", "Self": {"DNSName": "gateway.tail1234.ts.net."}}"#;
    assert_eq!(
        tls::tailscale_name(status).unwrap(),
        "gateway.tail1234.ts.net"
    );
    assert!(tls::tailscale_name(r#"{"Self": {"DNSName": ""}}"#).is_err());
}

/// A fake AWS on one port: instance metadata (IMDSv2), STS and path-style S3. Each STS and S3
/// call must carry a valid SigV4 signature of what arrived, made with the right credentials.
#[derive(Default)]
struct FakeAws {
    objects: Mutex<BTreeMap<String, Vec<u8>>>,
    calls: Mutex<Vec<String>>,
}

const BASE: (&str, &str, &str) = ("ASIABASE", "base/secret", "base-token");
const ROLE: (&str, &str, &str) = ("ASIAROLE", "role+secret", "role-token");

fn decode(text: &str) -> String {
    percent_encoding::percent_decode_str(text)
        .decode_utf8()
        .unwrap()
        .into_owned()
}

/// The key id whose valid signature the request carries, or why not.
fn signer(
    method: &str,
    uri: &hyper::Uri,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<&'static str, String> {
    use sha2::Digest;
    // AWS joins the values of a header sent twice, so such a request never matches its
    // signature (the gateway once sent content-type twice to STS).
    for name in headers.keys() {
        if headers.get_all(name).iter().count() > 1 {
            return Err(format!("the header {name} is sent twice"));
        }
    }
    let header = |n: &str| {
        headers
            .get(n)
            .and_then(|v| v.to_str().ok())
            .unwrap_or_default()
    };
    let auth = header("authorization");
    let key_id = auth
        .split("Credential=")
        .nth(1)
        .and_then(|c| c.split('/').next())
        .unwrap_or_default();
    let (id, secret, token) = match key_id {
        k if k == BASE.0 => BASE,
        k if k == ROLE.0 => ROLE,
        k => return Err(format!("unknown key {k:?}")),
    };
    let scope: Vec<&str> = auth.split('/').collect();
    let (region, service) = (scope[2], scope[3]);
    let names = auth
        .split("SignedHeaders=")
        .nth(1)
        .and_then(|s| s.split(',').next())
        .unwrap_or_default();
    let added = [
        "host",
        "x-amz-date",
        "x-amz-content-sha256",
        "x-amz-security-token",
    ];
    let signed: Vec<(&str, &str)> = names
        .split(';')
        .filter(|n| !added.contains(n))
        .map(|n| (n, header(n)))
        .collect();
    let pairs: Vec<(String, String)> = uri
        .query()
        .unwrap_or_default()
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap_or((p, ""));
            (decode(k), decode(v))
        })
        .collect();
    let query: Vec<(&str, &str)> = pairs
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let date = header("x-amz-date");
    let unix = humantime::parse_rfc3339(&format!(
        "{}-{}-{}T{}:{}:{}Z",
        &date[0..4],
        &date[4..6],
        &date[6..8],
        &date[9..11],
        &date[11..13],
        &date[13..15]
    ))
    .map_err(|e| e.to_string())?
    .duration_since(SystemTime::UNIX_EPOCH)
    .unwrap()
    .as_secs();
    let path = decode(uri.path());
    let payload = hex::encode(sha2::Sha256::digest(body));
    let credentials = desk_core::aws::Credentials {
        key_id: id.into(),
        secret: secret.into(),
        token: Some(token.into()),
    };
    let want = desk_core::aws::sign(
        &desk_core::aws::Request {
            method,
            host: header("host"),
            path: &path,
            query: &query,
            headers: &signed,
            payload_sha256: &payload,
        },
        &credentials,
        region,
        service,
        unix,
    );
    let ok = want.last().map(|(_, v)| v.as_str()) == Some(auth)
        && header("x-amz-security-token") == token
        && (service != "s3" || header("x-amz-content-sha256") == payload);
    if ok {
        Ok(id)
    } else {
        Err(format!("signature mismatch for {method} {uri}"))
    }
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;")
}

impl FakeAws {
    fn answer(
        &self,
        method: &str,
        uri: &hyper::Uri,
        headers: &HeaderMap,
        body: &[u8],
    ) -> (u16, String) {
        let path = uri.path();
        self.calls.lock().unwrap().push(format!("{method} {path}"));
        let metadata = "/latest/meta-data/iam/security-credentials/";
        if path == "/latest/api/token" {
            return (200, "imds-token".into());
        }
        if let Some(role) = path.strip_prefix(metadata) {
            if headers
                .get("x-aws-ec2-metadata-token")
                .map(|v| v.as_bytes())
                != Some(b"imds-token")
            {
                return (401, String::new());
            }
            return match role {
                "" => (200, "gateway-role\n".into()),
                "gateway-role" => (
                    200,
                    json!({"Code": "Success", "AccessKeyId": BASE.0, "SecretAccessKey": BASE.1,
                           "Token": BASE.2, "Expiration": "2099-01-01T00:00:00Z"})
                    .to_string(),
                ),
                _ => (404, String::new()),
            };
        }
        let signer = match signer(method, uri, headers, body) {
            Ok(id) => id,
            Err(e) => {
                return (
                    403,
                    format!(
                        "<Error><Code>SignatureDoesNotMatch</Code><Message>{e}</Message></Error>"
                    ),
                )
            }
        };
        if method == "POST" && path == "/" {
            let form = String::from_utf8_lossy(body);
            let ok = signer == BASE.0
                && form.contains("Action=AssumeRole")
                && form.contains("RoleArn=arn%3Aaws%3Aiam%3A%3A123456789012%3Arole%2Freader")
                && form.contains("RoleSessionName=vault-gateway");
            if !ok {
                return (
                    400,
                    format!("<Error><Code>Bad</Code><Message>{form}</Message></Error>"),
                );
            }
            return (
                200,
                format!(
                    "<AssumeRoleResponse><AssumeRoleResult><Credentials><AccessKeyId>{}</AccessKeyId>\
                     <SecretAccessKey>{}</SecretAccessKey><SessionToken>{}</SessionToken>\
                     <Expiration>2099-01-01T00:00:00Z</Expiration></Credentials></AssumeRoleResult></AssumeRoleResponse>",
                    ROLE.0, xml_escape(ROLE.1), ROLE.2
                ),
            );
        }
        if signer != ROLE.0 {
            return (403, "<Error><Code>AccessDenied</Code></Error>".into());
        }
        let key = decode(path.strip_prefix("/b/").unwrap_or_default());
        let mut objects = self.objects.lock().unwrap();
        match method {
            "GET" if key.is_empty() => {
                let query: HashMap<String, String> = uri
                    .query()
                    .unwrap_or_default()
                    .split('&')
                    .filter_map(|p| p.split_once('='))
                    .map(|(k, v)| (decode(k), decode(v)))
                    .collect();
                let prefix = &query["prefix"];
                let start: usize = query
                    .get("continuation-token")
                    .map_or(0, |t| t.parse().unwrap());
                let keys: Vec<&String> = objects.keys().filter(|k| k.starts_with(prefix)).collect();
                // One key a page, so that every listing pages.
                let mut xml = String::from(
                    "<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
                );
                if let Some(k) = keys.get(start) {
                    xml.push_str(&format!(
                        "<Contents><Key>{}</Key></Contents>",
                        xml_escape(k)
                    ));
                }
                if start + 1 < keys.len() {
                    xml.push_str(&format!(
                        "<NextContinuationToken>{}</NextContinuationToken>",
                        start + 1
                    ));
                }
                (200, xml + "</ListBucketResult>")
            }
            "GET" => match objects.get(&key) {
                Some(data) => (200, String::from_utf8_lossy(data).into_owned()),
                None => (404, "<Error><Code>NoSuchKey</Code></Error>".into()),
            },
            "PUT" => {
                if headers.get("if-none-match").map(|v| v.as_bytes()) == Some(b"*")
                    && objects.contains_key(&key)
                {
                    return (412, "<Error><Code>PreconditionFailed</Code></Error>".into());
                }
                objects.insert(key, body.to_vec());
                (200, String::new())
            }
            "DELETE" => {
                objects.remove(&key);
                (204, String::new())
            }
            _ => (405, String::new()),
        }
    }
}

#[tokio::test]
async fn the_aws_client_signs_assumes_the_role_and_makes_the_four_s3_calls() {
    let fake = Arc::new(FakeAws::default());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let server = fake.clone();
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let fake = server.clone();
            tokio::spawn(async move {
                let service =
                    hyper::service::service_fn(move |request: Request<hyper::body::Incoming>| {
                        let fake = fake.clone();
                        async move {
                            let (parts, body) = request.into_parts();
                            let body = body.collect().await.unwrap().to_bytes();
                            let (status, text) = fake.answer(
                                parts.method.as_str(),
                                &parts.uri,
                                &parts.headers,
                                &body,
                            );
                            let mut response = hyper::Response::new(Full::new(Bytes::from(text)));
                            *response.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
                            Ok::<_, std::convert::Infallible>(response)
                        }
                    });
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });

    let aws = Aws::for_test(None, &endpoint, &endpoint).unwrap();
    let s3 = aws.bucket("b", "eu-central-1", "arn:aws:iam::123456789012:role/reader");
    let odd = "p/slots/a b&c+d.age";
    assert!(s3.put_new(odd, b"one".to_vec()).await.unwrap());
    assert!(!s3.put_new(odd, b"two".to_vec()).await.unwrap());
    assert!(s3.put_new("p/slots/z.age", b"zz".to_vec()).await.unwrap());
    assert!(s3.put_new("q/slots/x.age", b"x".to_vec()).await.unwrap());
    assert_eq!(s3.list("p/slots/").await.unwrap(), [odd, "p/slots/z.age"]);
    let object = s3.get(odd).await.unwrap().unwrap();
    assert_eq!(object.length, Some(3));
    assert_eq!(object.body.collect().await.unwrap().to_bytes(), "one");
    assert!(s3.get("p/slots/missing.age").await.unwrap().is_none());
    s3.delete(odd).await.unwrap();
    assert_eq!(s3.list("p/").await.unwrap(), ["p/slots/z.age"]);

    // Credentials are fetched once and kept until they near their end.
    let calls = fake.calls.lock().unwrap().clone();
    let count = |call: &str| calls.iter().filter(|c| *c == call).count();
    assert_eq!(count("PUT /latest/api/token"), 1);
    assert_eq!(count("POST /"), 1);

    // A wrong signature is an error with the S3 code.
    let wrong = Aws::for_test(
        Some(desk_core::aws::Credentials {
            key_id: BASE.0.into(),
            secret: "not the secret".into(),
            token: Some(BASE.2.into()),
        }),
        &endpoint,
        &endpoint,
    )
    .unwrap()
    .bucket("b", "eu-central-1", "arn:aws:iam::123456789012:role/reader");
    let error = wrong.list("p/").await.unwrap_err().to_string();
    assert!(error.contains("SignatureDoesNotMatch"), "{error}");
}
