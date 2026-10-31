#![allow(clippy::unwrap_used)]

use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

use futures_util::future::BoxFuture;
use studio_runtime_store::wal_backup::restore;

use super::*;
use crate::multi_server_auth::{AccessError, refused as access_refused};
use crate::multi_server_orchestration::{OrchestrationDriver, RuntimeOrchestrationEffects};

fn receive(root: &Path, principal: &str, payload: &Value) -> Result<Value, ReceiveError> {
    super::receive(root, principal, &Put::deserialize(payload).unwrap())
}

fn chunk(file: &str, offset: u64, data: &[u8], end: bool) -> Value {
    json!({
        "generation": "000000000001-aa", "file": file, "offset": offset,
        "data": base64::engine::general_purpose::STANDARD.encode(data),
        "sha256": data_encoding::HEXLOWER.encode(&Sha256::digest(data)),
        "end": end, "retain": 2,
    })
}

fn held(root: &Path) -> PathBuf {
    root.join(RECEIVED_DIR).join("mac").join("000000000001-aa")
}

#[test]
fn a_chunk_lands_only_at_the_end_of_what_the_receiver_holds() {
    let root = tempfile::tempdir().unwrap();
    let reply = |value: Value| {
        (
            value["received"].as_u64().unwrap(),
            value["done"].as_bool().unwrap(),
        )
    };
    let first = receive(root.path(), "mac", &chunk("snapshot", 0, b"abcd", false)).unwrap();
    assert_eq!(reply(first), (4, false));
    // A replay of the same chunk, and a chunk after a gap, change nothing.
    let replay = receive(root.path(), "mac", &chunk("snapshot", 0, b"abcd", false)).unwrap();
    assert_eq!(reply(replay), (4, false));
    let gap = receive(root.path(), "mac", &chunk("snapshot", 9, b"zz", false)).unwrap();
    assert_eq!(reply(gap), (4, false));
    let last = receive(root.path(), "mac", &chunk("snapshot", 4, b"efg", true)).unwrap();
    assert_eq!(reply(last), (7, true));
    assert_eq!(
        std::fs::read(held(root.path()).join(SNAPSHOT_FILE)).unwrap(),
        b"abcdefg"
    );
    let done = receive(root.path(), "mac", &chunk("snapshot", 0, b"abcd", false)).unwrap();
    assert_eq!(reply(done), (7, true));
}

#[test]
fn a_bad_chunk_is_refused_and_a_segment_needs_its_snapshot() {
    let root = tempfile::tempdir().unwrap();
    let mut wrong_hash = chunk("snapshot", 0, b"abcd", false);
    wrong_hash["sha256"] = json!("0".repeat(64));
    assert!(matches!(
        receive(root.path(), "mac", &wrong_hash),
        Err(ReceiveError::Refused(_))
    ));
    assert!(matches!(
        receive(root.path(), "../mac", &chunk("snapshot", 0, b"a", true)),
        Err(ReceiveError::Refused(_))
    ));
    assert!(matches!(
        receive(root.path(), "mac", &chunk("../etc", 0, b"a", true)),
        Err(ReceiveError::Refused(_))
    ));
    let segment = "w00000000-0000000000000000-0000000000000001-0000000100000001-00000001.seg";
    let reply = receive(root.path(), "mac", &chunk(segment, 0, b"a", true)).unwrap();
    assert_eq!(reply["noSnapshot"], true);
}

#[test]
fn a_finished_snapshot_prunes_older_generations() {
    let root = tempfile::tempdir().unwrap();
    for generation in ["000000000001-a", "000000000002-a", "000000000003-a"] {
        let mut put = chunk("snapshot", 0, b"x", true);
        put["generation"] = json!(generation);
        put["retain"] = json!(2);
        receive(root.path(), "mac", &put).unwrap();
    }
    let mut names: Vec<_> = std::fs::read_dir(root.path().join(RECEIVED_DIR).join("mac"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["000000000002-a", "000000000003-a"]);
}

#[test]
fn settings_are_off_by_default_and_checked() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(Settings::read(root.path()), Settings::default());
    assert!(!Settings::default().enabled);
    let on = Settings {
        enabled: true,
        target: None,
        ..Settings::default()
    };
    assert!(on.write(root.path()).is_err(), "a target is required");
    let bad = Settings {
        retain_generations: 0,
        ..Settings::default()
    };
    assert!(bad.write(root.path()).is_err());
    let good = Settings {
        enabled: true,
        target: Some("server".into()),
        retain_generations: 3,
    };
    good.write(root.path()).unwrap();
    assert_eq!(Settings::read(root.path()), good);
}

#[test]
fn a_chunk_and_its_envelope_fit_the_paired_server_request_limit() {
    let encoded = BACKUP_CHUNK_BYTES.div_ceil(3) * 4;
    // 4 KiB covers the envelope fields, the hash, and the signature headers.
    assert!(encoded + 4096 < crate::multi_server_transport_limits::MAX_REQUEST_BYTES);
}

/// The paired server as an in-process link. It can lose one answer after the request landed.
struct Link {
    peer: Arc<OrchestrationDriver>,
    calls: AtomicUsize,
    lose_answer_at: Mutex<Option<usize>>,
}

impl PairedTransport for Link {
    fn exchange<'a>(
        &'a self,
        _server: &'a str,
        envelope: &'a Value,
        _timeout: Duration,
    ) -> BoxFuture<'a, Result<studio_runtime_store::multi_server_delivery::RemoteReply, AccessError>>
    {
        Box::pin(async move {
            let key = envelope["requestId"].as_str().unwrap().to_owned();
            let call = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
            let reply = self
                .peer
                .receive(
                    "mac",
                    envelope,
                    crate::event_write::unix_time().unwrap() as i64,
                )
                .await?
                .to_reply(&key);
            let mut lose = self.lose_answer_at.lock().unwrap();
            if *lose == Some(call) {
                *lose = None;
                return Err(access_refused(503, "lost", "The answer was lost"));
            }
            Ok(reply)
        })
    }
}

async fn pair(store: &Arc<RuntimeStore>, id: &'static str) {
    let record = json!({
        "id": id, "clientId": id, "kind": "server", "label": id, "created": 1,
        "publicKey": "public-key", "origin": "https://peer.invalid", "status": "paired",
    });
    store
        .transaction(move |tx| {
            tx.execute(
                "INSERT INTO runtime_access_clients(id,record) VALUES (?1,?2)",
                rusqlite::params![id, record.to_string()],
            )?;
            Ok::<(), studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
}

async fn insert(store: &RuntimeStore, rows: i64) {
    store
        .transaction(move |tx| {
            tx.execute_batch("CREATE TABLE IF NOT EXISTS backup_witness(value BLOB NOT NULL);")?;
            for _ in 0..rows {
                tx.execute("INSERT INTO backup_witness VALUES (zeroblob(3000))", [])?;
            }
            Ok::<(), studio_runtime_store::StoreError>(())
        })
        .await
        .unwrap();
}

fn count(path: &Path) -> i64 {
    rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
        .unwrap()
        .query_row("SELECT count(*) FROM backup_witness", [], |row| row.get(0))
        .unwrap()
}

// The owner's server sends a snapshot and segments to the paired server, which rebuilds
// the same database from them. A lost answer in the middle of the snapshot costs nothing.
#[tokio::test]
async fn a_server_backs_up_to_a_paired_server_and_the_copy_restores() {
    let source_dir = tempfile::tempdir().unwrap();
    let peer_dir = tempfile::tempdir().unwrap();
    let source = Arc::new(RuntimeStore::open(Some(source_dir.path())).await.unwrap());
    let peer = Arc::new(RuntimeStore::open(Some(peer_dir.path())).await.unwrap());
    pair(&source, "server").await;
    pair(&peer, "mac").await;
    insert(&source, 40).await;
    let link = Arc::new(Link {
        peer: Arc::new(OrchestrationDriver::new(
            Arc::clone(&peer),
            Arc::new(HttpsPairedTransport::new(Arc::clone(&peer))),
            Arc::new(RuntimeOrchestrationEffects::new(Arc::clone(&peer))),
        )),
        calls: AtomicUsize::new(0),
        lose_answer_at: Mutex::new(Some(3)),
    });
    let (_stop, stop_rx) = watch::channel(false);
    let sender = Sender::new(
        Arc::clone(&source),
        Arc::clone(&link) as Arc<dyn PairedTransport>,
        stop_rx,
    );
    // Off by default: a pass sends nothing.
    sender.pass().await.unwrap();
    assert_eq!(link.calls.load(Ordering::SeqCst), 0);
    Settings {
        enabled: true,
        target: Some("server".into()),
        retain_generations: 2,
    }
    .write(source_dir.path())
    .unwrap();
    // The answer to the third request is lost; the request is sent again at its offset.
    sender.pass().await.unwrap();
    assert_eq!(source.wal_backup().unwrap().status().broken, None);
    insert(&source, 25).await;
    source.checkpoint_wal_passive().await.unwrap();
    insert(&source, 5).await;
    source.checkpoint_wal_passive().await.unwrap();
    sender.pass().await.unwrap();

    let status = source.wal_backup().unwrap().status();
    assert_eq!((status.broken, status.pending_segments), (None, 0));
    let generation = status.generation.unwrap();
    let folder = peer_dir
        .path()
        .join(RECEIVED_DIR)
        .join("mac")
        .join(generation);
    let out = peer_dir.path().join("restored.sqlite3");
    let restored = restore(&folder, &out).unwrap();
    assert!(restored.segments >= 2, "{restored:?}");
    assert_eq!(count(&out), 70);
}
