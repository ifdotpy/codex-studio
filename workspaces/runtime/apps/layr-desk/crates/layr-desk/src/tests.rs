//! The network disk end to end: a fake gateway serves the fixture vault that the Python writer
//! made, the disk serves it over WebDAV, and every file of the latest state reads back as the
//! writer wrote it. A vault without a key on this machine shows how to unlock it, and a vault
//! whose vault.json asks for signatures that its states lack shows why it is refused. A paper
//! slot that holds another identity than the vault's is refused.

use crate::{
    disk::Disk,
    keystore::{KeyStore, Memory},
};
use base64::Engine;
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Request, Response, StatusCode};
use sha2::{Digest, Sha256};
use std::path::PathBuf;
use std::sync::Arc;

const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../../js/vault/test/fixture");
const TOKEN: &str = "token-of-the-test";

fn fixture_identity() -> String {
    std::fs::read_to_string(PathBuf::from(FIXTURE).join("identity.txt"))
        .unwrap()
        .trim()
        .to_string()
}

fn fixture_recipient() -> String {
    desk_core::keys::recipient_of(&fixture_identity()).unwrap()
}

/// What the fake gateway serves besides the fixture states: the vault.json reply and the slots.
#[derive(Clone)]
struct Fake {
    vault: serde_json::Value,
    slots: serde_json::Value,
}

impl Default for Fake {
    fn default() -> Self {
        Fake {
            vault: serde_json::json!({"recipient": fixture_recipient(), "writer": null, "signedFrom": null}),
            slots: serde_json::json!([]),
        }
    }
}

/// A fake gateway on a free port.
async fn fake_gateway(fake: Fake) -> String {
    let fake = Arc::new(fake);
    listen(move |request| gateway(fake.clone(), request)).await
}

async fn gateway(
    fake: Arc<Fake>,
    request: Request<hyper::body::Incoming>,
) -> Result<Response<Full<Bytes>>, std::convert::Infallible> {
    let states = PathBuf::from(FIXTURE).join("objects/proj/states");
    let path = request.uri().path().to_string();
    let authorized = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        == Some(&format!("Bearer {TOKEN}"));
    let json = |v: serde_json::Value| Response::new(Full::new(Bytes::from(v.to_string())));
    let status = |s: StatusCode| {
        let mut r = Response::new(Full::new(Bytes::new()));
        *r.status_mut() = s;
        r
    };
    Ok(match path.as_str() {
        "/api/projects" => json(serde_json::json!({"projects": ["locked", "proj"]})),
        "/api/p/proj/vault" => json(fake.vault.clone()),
        "/api/p/proj/slots" => json(serde_json::json!({ "slots": fake.slots })),
        "/api/p/proj/challenge" => {
            let sealed = desk_core::keys::encrypt(&fixture_recipient(), TOKEN.as_bytes()).unwrap();
            json(
                serde_json::json!({"challenge": base64::engine::general_purpose::STANDARD.encode(sealed)}),
            )
        }
        _ if !authorized => status(StatusCode::UNAUTHORIZED),
        "/api/p/proj/states" => {
            let names: Vec<String> = std::fs::read_dir(&states)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            json(serde_json::json!({"states": names}))
        }
        p => match p
            .strip_prefix("/api/p/proj/states/")
            .and_then(|n| std::fs::read(states.join(n)).ok())
        {
            Some(data) => Response::new(Full::new(Bytes::from(data))),
            None => status(StatusCode::NOT_FOUND),
        },
    })
}

async fn listen<F, Fut>(handler: F) -> String
where
    F: Fn(Request<hyper::body::Incoming>) -> Fut + Clone + Send + Sync + 'static,
    Fut: std::future::Future<Output = Result<Response<Full<Bytes>>, std::convert::Infallible>>
        + Send
        + 'static,
{
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!(
        "http://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    tokio::spawn(async move {
        loop {
            let (stream, _) = listener.accept().await.unwrap();
            let handler = handler.clone();
            tokio::spawn(async move {
                let service = hyper::service::service_fn(handler);
                let _ = hyper::server::conn::http1::Builder::new()
                    .serve_connection(hyper_util::rt::TokioIo::new(stream), service)
                    .await;
            });
        }
    });
    url
}

/// The disk of a fake gateway with the fixture key kept: its WebDAV base and its cache folder.
async fn disk(fake: Fake, name: &str) -> (String, PathBuf) {
    let site = fake_gateway(fake).await;
    let keys = Memory::default();
    keys.set(
        &crate::keystore::account(&site, "proj").unwrap(),
        &fixture_identity(),
    )
    .unwrap();
    let cache = std::env::temp_dir().join(format!("layr-desk-{name}-{}", std::process::id()));
    let disk = Arc::new(
        Disk::new(
            &site,
            Box::new(keys),
            cache.clone(),
            std::time::Duration::from_secs(60),
        )
        .unwrap(),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!(
        "http://127.0.0.1:{}/",
        listener.local_addr().unwrap().port()
    );
    tokio::spawn(crate::run(listener, disk));
    (base, cache)
}

#[tokio::test]
async fn the_disk_shows_the_latest_state_of_each_vault() {
    let (base, cache) = disk(Fake::default(), "test").await;
    let http = reqwest::Client::new();
    let propfind = reqwest::Method::from_bytes(b"PROPFIND").unwrap();

    let root = http
        .request(propfind.clone(), &base)
        .header("Depth", "1")
        .send()
        .await
        .unwrap();
    assert_eq!(root.status(), StatusCode::MULTI_STATUS);
    let root = root.text().await.unwrap();
    assert!(
        root.contains("<D:href>/proj/</D:href>") && root.contains("<D:href>/locked/</D:href>"),
        "{root}"
    );

    let locked = http
        .get(format!("{base}locked/LOCKED.txt"))
        .send()
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(
        locked.contains("layr-desk unlock --site") && locked.contains("--project locked"),
        "{locked}"
    );

    let expected: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(FIXTURE).join("expected.json")).unwrap(),
    )
    .unwrap();
    let last = expected["states"]
        .as_array()
        .unwrap()
        .last()
        .unwrap()
        .as_object()
        .unwrap();
    for (path, want) in last {
        let response = http.get(format!("{base}proj/{path}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let data = response.bytes().await.unwrap();
        if want[1] != "120000" {
            assert_eq!(hex::encode(Sha256::digest(&data)), want[0], "{path}");
        }
    }
    let first = last.keys().find(|p| last[*p][1] == "100644").unwrap();
    let part = http
        .get(format!("{base}proj/{first}"))
        .header("Range", "bytes=0-0")
        .send()
        .await
        .unwrap();
    assert_eq!(part.status(), StatusCode::PARTIAL_CONTENT);
    let put = http
        .put(format!("{base}proj/new.txt"))
        .body("x")
        .send()
        .await
        .unwrap();
    assert_eq!(put.status(), StatusCode::METHOD_NOT_ALLOWED);
    assert_eq!(
        http.get(format!("{base}proj/missing"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::NOT_FOUND
    );
    let _ = std::fs::remove_dir_all(cache);
}

struct Folder(PathBuf);

impl desk_core::Store for Folder {
    async fn get(&self, name: &str) -> anyhow::Result<Vec<u8>> {
        Ok(std::fs::read(self.0.join(name))?)
    }
}

#[tokio::test]
async fn pull_writes_each_state_into_a_new_folder() {
    use std::os::unix::fs::PermissionsExt;
    let states = PathBuf::from(FIXTURE).join("objects/proj/states");
    let names: Vec<String> = std::fs::read_dir(&states)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    let policy = desk_core::Policy::new(
        "proj",
        &desk_core::VaultInfo {
            recipient: fixture_recipient(),
            ..Default::default()
        },
    )
    .unwrap();
    let reader =
        desk_core::Reader::new(Folder(states), &fixture_identity(), &names, policy).unwrap();
    let expected: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(PathBuf::from(FIXTURE).join("expected.json")).unwrap(),
    )
    .unwrap();
    let root = std::env::temp_dir().join(format!("layr-desk-pull-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    for (state, want) in reader
        .states()
        .into_iter()
        .zip(expected["states"].as_array().unwrap())
    {
        let to = root.join(format!("s{}", state.0));
        let summary = crate::restore::pull(&reader, state, &to).await.unwrap();
        let want = want.as_object().unwrap();
        assert_eq!(summary.files, want.len());
        for (path, w) in want {
            let target = to.join(path);
            let meta = std::fs::symlink_metadata(&target).unwrap();
            let data = if w[1] == "120000" {
                assert!(meta.file_type().is_symlink(), "{path}");
                use std::os::unix::ffi::OsStrExt;
                std::fs::read_link(&target)
                    .unwrap()
                    .as_os_str()
                    .as_bytes()
                    .to_vec()
            } else {
                assert_eq!(
                    meta.permissions().mode() & 0o111 != 0,
                    w[1] == "100755",
                    "{path}"
                );
                std::fs::read(&target).unwrap()
            };
            assert_eq!(hex::encode(Sha256::digest(&data)), w[0], "{path}");
        }
        let again = crate::restore::pull(&reader, reader.latest().unwrap(), &to).await;
        assert!(again.unwrap_err().to_string().contains("new folder"));
    }
    let leftovers: Vec<_> = std::fs::read_dir(&root)
        .unwrap()
        .map(|e| e.unwrap().file_name())
        .filter(|n| n.to_string_lossy().contains("partial"))
        .collect();
    assert!(leftovers.is_empty());
    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn the_disk_shows_why_an_unsigned_state_is_refused() {
    let writer = desk_core::sign::Signer::from_base64(
        &base64::engine::general_purpose::STANDARD.encode([4u8; 32]),
    )
    .unwrap()
    .public();
    let fake = Fake {
        vault: serde_json::json!({"recipient": fixture_recipient(), "writer": writer, "signedFrom": 4}),
        ..Fake::default()
    };
    let (base, cache) = disk(fake, "unsigned").await;
    let error = reqwest::get(format!("{base}proj/ERROR.txt"))
        .await
        .unwrap()
        .text()
        .await
        .unwrap();
    assert!(error.contains("state 4 has no writer signature"), "{error}");
    let _ = std::fs::remove_dir_all(cache);
}

#[tokio::test]
async fn a_paper_slot_with_another_identity_is_refused() {
    use desk_core::keys;
    let paper = keys::generate_identity();
    let words = keys::words_of_identity(&paper).unwrap();
    let slot = |identity: &str| {
        let sealed = keys::seal_slot(identity, &keys::recipient_of(&paper).unwrap()).unwrap();
        serde_json::json!([{"name": "paper", "data": base64::engine::general_purpose::STANDARD.encode(sealed)}])
    };
    let forged = Fake {
        slots: slot(&keys::generate_identity()),
        ..Fake::default()
    };
    let site = fake_gateway(forged).await;
    let mut vault = crate::gateway::Site::new(&site).unwrap().vault("proj");
    let err = vault.unlock_with_paper(&words).await.unwrap_err();
    assert!(format!("{err:#}").contains("may be forged"), "{err:#}");
    assert!(vault.identity().is_err());

    let genuine = Fake {
        slots: slot(&fixture_identity()),
        ..Fake::default()
    };
    let site = fake_gateway(genuine).await;
    let mut vault = crate::gateway::Site::new(&site).unwrap().vault("proj");
    assert_eq!(
        vault.unlock_with_paper(&words).await.unwrap(),
        fixture_identity()
    );
}
