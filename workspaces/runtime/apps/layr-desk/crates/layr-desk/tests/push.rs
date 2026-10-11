//! `layr-desk push` end to end: the binary reads its configuration from a file or from standard
//! input, runs a fake layr command (states as folders), and puts the vault into a fake S3 that
//! checks every signature and pages its listings. desk-core's reader decrypts every pushed
//! state with the vault identity.

use bytes::Bytes;
use desk_core::{keys, Reader, Store};
use http_body_util::{BodyExt, Full};
use hyper::{Request, Response};
use percent_encoding::percent_decode_str;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

const KEY_ID: &str = "AKIAIOSFODNN7EXAMPLE";
const SECRET: &str = "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY";
const BUCKET: &str = "vault-test";
const PAGE: usize = 2;

type Objects = Arc<Mutex<BTreeMap<String, Vec<u8>>>>;

/// The authorization header that the request should carry, from what the server received.
fn expected_authorization(parts: &hyper::http::request::Parts) -> Option<String> {
    let header = |name: &str| parts.headers.get(name)?.to_str().ok();
    let authorization = header("authorization")?;
    let names = authorization
        .split("SignedHeaders=")
        .nth(1)?
        .split(',')
        .next()?;
    let signed: Vec<(&str, &str)> = names
        .split(';')
        .filter(|n| !matches!(*n, "host" | "x-amz-date" | "x-amz-content-sha256"))
        .map(|n| Some((n, header(n)?)))
        .collect::<Option<_>>()?;
    // The time of the request, from its x-amz-date (YYYYMMDDTHHMMSSZ).
    let d = header("x-amz-date")?;
    let time = humantime::parse_rfc3339(&format!(
        "{}-{}-{}T{}:{}:{}Z",
        &d[0..4],
        &d[4..6],
        &d[6..8],
        &d[9..11],
        &d[11..13],
        &d[13..15]
    ))
    .ok()?;
    let unix = time.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    let decode = |s: &str| percent_decode_str(s).decode_utf8().unwrap().into_owned();
    let query: Vec<(String, String)> = parts
        .uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter(|p| !p.is_empty())
        .map(|p| {
            let (k, v) = p.split_once('=').unwrap();
            (decode(k), decode(v))
        })
        .collect();
    let query: Vec<(&str, &str)> = query
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    let headers = desk_core::aws::sign(
        &desk_core::aws::Request {
            method: parts.method.as_str(),
            host: header("host")?,
            path: &decode(parts.uri.path()),
            query: &query,
            headers: &signed,
            payload_sha256: header("x-amz-content-sha256")?,
        },
        &desk_core::aws::Credentials {
            key_id: KEY_ID.into(),
            secret: SECRET.into(),
            token: None,
        },
        "us-east-1",
        "s3",
        unix,
    );
    headers
        .into_iter()
        .find(|(k, _)| k == "authorization")
        .map(|(_, v)| v)
}

fn reply(status: u16, body: impl Into<Bytes>) -> Response<Full<Bytes>> {
    let mut response = Response::new(Full::new(body.into()));
    *response.status_mut() = hyper::StatusCode::from_u16(status).unwrap();
    response
}

async fn s3(objects: Objects, request: Request<hyper::body::Incoming>) -> Response<Full<Bytes>> {
    let (parts, body) = request.into_parts();
    let body = body.collect().await.unwrap().to_bytes();
    let authorization = parts
        .headers
        .get("authorization")
        .and_then(|v| v.to_str().ok());
    if authorization.is_none()
        || authorization.map(str::to_string) != expected_authorization(&parts)
    {
        return reply(403, "<Error><Code>SignatureDoesNotMatch</Code></Error>");
    }
    let sent_hash = parts.headers["x-amz-content-sha256"].to_str().unwrap();
    if sent_hash != hex::encode(Sha256::digest(&body)) {
        return reply(400, "<Error><Code>XAmzContentSHA256Mismatch</Code></Error>");
    }
    let path = percent_decode_str(parts.uri.path()).decode_utf8().unwrap();
    let Some(key) = path.strip_prefix(&format!("/{BUCKET}/")) else {
        return reply(404, "<Error><Code>NoSuchBucket</Code></Error>");
    };
    let mut objects = objects.lock().unwrap();
    if parts.method == hyper::Method::PUT {
        if parts.headers.get("if-none-match").map(|v| v.as_bytes()) != Some(b"*") {
            return reply(400, "a put must not replace an object");
        }
        if objects.contains_key(key) {
            return reply(412, "<Error><Code>PreconditionFailed</Code></Error>");
        }
        objects.insert(key.to_string(), body.to_vec());
        return reply(200, "");
    }
    // ListObjectsV2, PAGE keys a page; the token holds characters that a URL must escape.
    let query: BTreeMap<String, String> = parts
        .uri
        .query()
        .unwrap_or("")
        .split('&')
        .filter_map(|p| p.split_once('='))
        .map(|(k, v)| {
            let decode = |s: &str| percent_decode_str(s).decode_utf8().unwrap().into_owned();
            (decode(k), decode(v))
        })
        .collect();
    assert_eq!(query["list-type"], "2");
    let start: usize = match query.get("continuation-token") {
        Some(token) => token.strip_prefix("t+/=").unwrap().parse().unwrap(),
        None => 0,
    };
    let keys: Vec<&String> = objects
        .keys()
        .filter(|k| k.starts_with(&query["prefix"]))
        .collect();
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<ListBucketResult xmlns=\"http://s3.amazonaws.com/doc/2006-03-01/\">",
    );
    for key in keys.iter().skip(start).take(PAGE) {
        xml += &format!("<Contents><Key>{key}</Key><Size>1</Size></Contents>");
    }
    if start + PAGE < keys.len() {
        xml += &format!(
            "<NextContinuationToken>t+/={}</NextContinuationToken>",
            start + PAGE
        );
    }
    xml += "</ListBucketResult>";
    reply(200, xml)
}

async fn serve(objects: Objects) -> String {
    use hyper::server::conn::http1;
    use hyper::service::service_fn;
    use hyper_util::rt::TokioIo;
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let objects = objects.clone();
            tokio::spawn(async move {
                let service = service_fn(move |request| {
                    let objects = objects.clone();
                    async move { Ok::<_, std::convert::Infallible>(s3(objects, request).await) }
                });
                let _ = http1::Builder::new()
                    .serve_connection(TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    endpoint
}

/// A layr command for project `proj`: `<root>/states/<state>` is a state, `<root>/head` the main
/// head. Its diff names every path of both states, which a writer must take well.
const FAKE_LAYR: &str = r#"#!/bin/sh
set -eu
root="$(dirname "$0")"
[ "$1" = --project ] && [ "$2" = proj ] || { echo "not the project: $*" >&2; exit 2; }
shift 2
case "$1" in
rev-parse) [ "$2" = main ] && cat "$root/head" ;;
export) rm -rf "$3"; cp -Rp "$root/states/$2" "$3"; printf '{"state": "%s"}' "$2" > "$3/.layr-export.json" ;;
diff) tab="$(printf '\t')"
  for s in "$3" "$4"; do (cd "$root/states/$s" && find . ! -type d | sed "s|^\./|M$tab|"); done | sort -u ;;
*) echo "not faked: $*" >&2; exit 2 ;;
esac
"#;

struct Fixture {
    root: PathBuf,
}

impl Fixture {
    fn new(name: &str) -> Fixture {
        use std::os::unix::fs::PermissionsExt;
        let root = std::env::temp_dir().join(format!("desk-push-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("layr/states")).unwrap();
        let layr = root.join("layr/layr");
        std::fs::write(&layr, FAKE_LAYR).unwrap();
        std::fs::set_permissions(&layr, std::fs::Permissions::from_mode(0o755)).unwrap();
        Fixture { root }
    }

    /// A new state of the project, which becomes the main head.
    fn commit(&self, state: &str, files: &[(&str, &[u8], u32)]) {
        use std::os::unix::fs::PermissionsExt;
        let folder = self.root.join("layr/states").join(state);
        for (path, data, mode) in files {
            let target = folder.join(path);
            std::fs::create_dir_all(target.parent().unwrap()).unwrap();
            if *mode == 0o120000 {
                std::os::unix::fs::symlink(std::str::from_utf8(data).unwrap(), &target).unwrap();
            } else {
                std::fs::write(&target, data).unwrap();
                std::fs::set_permissions(&target, std::fs::Permissions::from_mode(*mode)).unwrap();
            }
        }
        std::fs::write(self.root.join("layr/head"), state).unwrap();
    }

    /// `layr-desk push`, with the configuration from a file or (`None`) from standard input.
    fn push(&self, config: &serde_json::Value, from_file: bool) -> std::process::Output {
        let mut command = Command::new(env!("CARGO_BIN_EXE_layr-desk"));
        command.args(["push", "--project", "proj", "--layr"]);
        command.arg(self.root.join("layr/layr"));
        command.arg("--state-dir").arg(self.root.join("writer"));
        let text = config.to_string();
        if from_file {
            std::fs::write(self.root.join("config.json"), &text).unwrap();
            command.arg("--config").arg(self.root.join("config.json"));
        }
        let mut child = command
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        use std::io::Write;
        let mut stdin = child.stdin.take().unwrap();
        if !from_file {
            stdin.write_all(text.as_bytes()).unwrap();
        }
        drop(stdin);
        child.wait_with_output().unwrap()
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

struct Memory(Objects);

impl Store for Memory {
    async fn get(&self, name: &str) -> anyhow::Result<Vec<u8>> {
        let objects = self.0.lock().unwrap();
        objects
            .get(&format!("proj/states/{name}"))
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no object {name}"))
    }
}

fn summary(output: &std::process::Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn push_puts_each_new_head_into_the_vault() {
    let fixture = Fixture::new("push");
    let objects = Objects::default();
    // Trees of earlier pushes that the writer has no record of: it continues after them.
    for n in 1..=5 {
        let name = format!("proj/states/{n:010}.0000000{n}.tree.age");
        objects.lock().unwrap().insert(name, b"old".to_vec());
    }
    let endpoint = serve(objects.clone()).await;
    let identity = keys::generate_identity();
    let mut config = serde_json::json!({
        "bucket": BUCKET, "region": "us-east-1", "prefix": "proj",
        "recipient": keys::recipient_of(&identity).unwrap(),
        "accessKeyId": KEY_ID, "secretAccessKey": SECRET, "endpoint": endpoint,
        "projectId": "ignored",
    });
    let text: Vec<u8> = (0..2000)
        .flat_map(|i| format!("line {i} of the text\n").into_bytes())
        .collect();
    fixture.commit(
        "s1",
        &[
            ("app.txt", &text, 0o644),
            ("bin/run", b"#!/bin/sh\n", 0o755),
            ("link", b"app.txt", 0o120000),
        ],
    );
    let first = summary(&fixture.push(&config, true));
    assert_eq!(first["pushed"], true);
    assert_eq!(first["seq"], 6);

    let mut changed = text.clone();
    changed.extend(b"one more line\n");
    fixture.commit(
        "s2",
        &[
            ("app.txt", &changed, 0o644),
            ("bin/run", b"#!/bin/sh\n", 0o755),
            ("link", b"app.txt", 0o120000),
        ],
    );
    let second = summary(&fixture.push(&config, false));
    assert_eq!(second["seq"], 7);
    assert_eq!(second["files"], 1);
    assert_eq!(summary(&fixture.push(&config, false))["pushed"], false);

    let names: Vec<String> = objects
        .lock()
        .unwrap()
        .keys()
        .filter_map(|k| k.strip_prefix("proj/states/"))
        .map(str::to_string)
        .collect();
    let reader = Reader::new(Memory(objects.clone()), &identity, &names).unwrap();
    let states = reader.states();
    let pushed: Vec<_> = states
        .iter()
        .filter(|(seq, _)| *seq >= 6)
        .cloned()
        .collect();
    assert_eq!(pushed.len(), 2);
    for (state, want) in pushed.into_iter().zip([&text, &changed]) {
        let files = reader.files(Some(state)).await.unwrap();
        assert_eq!(
            files.keys().collect::<Vec<_>>(),
            ["app.txt", "bin/run", "link"]
        );
        assert_eq!(&reader.read(&files["app.txt"]).await.unwrap(), want);
        assert!(files["bin/run"].is_executable());
        assert!(files["link"].is_link());
    }
    let latest = reader.files(None).await.unwrap();
    assert_eq!(latest["app.txt"].3[0].5, "delta");

    // A wrong secret is refused by the store, and the push says so.
    config["secretAccessKey"] = "wrong-secret-0000000000".into();
    fixture.commit("s3", &[("app.txt", b"three", 0o644)]);
    let refused = fixture.push(&config, true);
    assert!(!refused.status.success());
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert!(stderr.contains("HTTP 403"), "{stderr}");
    assert!(!stderr.contains("wrong-secret"), "{stderr}");
}

#[test]
fn a_bad_configuration_is_refused_before_layr_runs() {
    let fixture = Fixture::new("config");
    let identity = keys::generate_identity();
    let good = serde_json::json!({
        "bucket": BUCKET, "region": "us-east-1", "prefix": "proj",
        "recipient": keys::recipient_of(&identity).unwrap(),
        "accessKeyId": KEY_ID, "secretAccessKey": SECRET,
    });
    for (field, value, message) in [
        ("recipient", "age1nothing", "recipient"),
        ("bucket", "Bad_Bucket", "bucket"),
        ("region", "eu-central-1.evil.com", "region"),
        ("prefix", "a/b", "prefix"),
    ] {
        let mut config = good.clone();
        config[field] = value.into();
        let output = fixture.push(&config, true);
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(message), "{field}: {stderr}");
    }
    let mut config = good.clone();
    config.as_object_mut().unwrap().remove("secretAccessKey");
    let stderr = String::from_utf8_lossy(&fixture.push(&config, false).stderr).into_owned();
    assert!(stderr.contains("secretAccessKey"), "{stderr}");
    assert!(!Path::new(&fixture.root.join("writer")).exists());
}
