#![allow(clippy::unwrap_used)]
use super::*;
use std::sync::mpsc;

fn fixture() -> (tempfile::TempDir, Arc<Supervisor>) {
    let root = tempfile::tempdir().unwrap();
    let supervisor = Arc::new(Supervisor {
        root: root.path().to_owned(),
        journal: Arc::new(SupervisorJournal::open(root.path()).unwrap()),
        children: Mutex::new(BTreeMap::new()),
        ownership: Mutex::new(Ownership::default()),
        recovery: Mutex::new(json!({"blocked":null})),
        connections: AtomicUsize::new(1),
    });
    (root, supervisor)
}

#[test]
fn stalled_handshake_and_started_frame_release_connection_slot() {
    // Real socket reads, no sleeps or minimum-duration timing assertion. The short
    // budget is injected only here; a channel provides the outer hang guard.
    for phase in ["no handshake", "partial handshake", "partial request"] {
        let (_root, supervisor) = fixture();
        let (mut peer, server) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
        let handshake =
            json!({"protocol":PROTOCOL,"stateDir":supervisor.root,"backendId":"stalled"});
        if phase == "partial request" {
            write_frame(&mut peer, &handshake).unwrap();
        } else if phase == "partial handshake" {
            peer.write_all(b"{").unwrap();
        }
        let (tx, rx) = mpsc::channel();
        let copy = supervisor.clone();
        let worker = thread::spawn(move || {
            connection_with_budget(copy, server, Duration::from_millis(50));
            tx.send(()).unwrap();
        });
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        if phase == "partial request" {
            assert_eq!(
                read_frame(
                    &mut reader,
                    Some(Instant::now() + Duration::from_secs(5)),
                    FRAME_BUDGET
                )
                .unwrap()
                .unwrap()["ok"],
                true
            );
            peer.write_all(b"{\"action\":").unwrap();
        }
        rx.recv_timeout(Duration::from_secs(5)).unwrap();
        worker.join().unwrap();
        assert_eq!(supervisor.connections.load(Ordering::Acquire), 0, "{phase}");
        assert!(
            lock(&supervisor.ownership)
                .unwrap()
                .attach(
                    &Handshake {
                        protocol: PROTOCOL,
                        state_dir: supervisor.root.to_string_lossy().into_owned(),
                        backend_id: Some("replacement".into()),
                        probe: false,
                        operator: false,
                    },
                    &supervisor.root.to_string_lossy()
                )
                .is_ok()
        );
        let error = read_frame(
            &mut reader,
            Some(Instant::now() + Duration::from_secs(5)),
            FRAME_BUDGET,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            error["error"], "Supervisor handshake or frame deadline exceeded",
            "{phase}"
        );
    }
}

#[test]
fn established_idle_connection_has_no_frame_deadline() {
    let (_root, supervisor) = fixture();
    let (mut peer, server) = UnixStream::pair().unwrap();
    let (tx, rx) = mpsc::channel();
    let copy = supervisor.clone();
    write_frame(
        &mut peer,
        &json!({"protocol":PROTOCOL,"stateDir":supervisor.root,"backendId":"idle"}),
    )
    .unwrap();
    let worker = thread::spawn(move || {
        connection_with_budget(copy, server, Duration::from_millis(50));
        tx.send(()).unwrap();
    });
    let mut reader = BufReader::new(peer.try_clone().unwrap());
    assert_eq!(
        read_frame(
            &mut reader,
            Some(Instant::now() + Duration::from_secs(5)),
            FRAME_BUDGET
        )
        .unwrap()
        .unwrap()["ok"],
        true
    );
    assert_eq!(
        rx.recv_timeout(Duration::from_millis(150)),
        Err(mpsc::RecvTimeoutError::Timeout)
    );
    write_frame(&mut peer, &json!({"action":"health","requestId":1})).unwrap();
    let result = read_frame(
        &mut reader,
        Some(Instant::now() + Duration::from_secs(5)),
        FRAME_BUDGET,
    )
    .unwrap()
    .unwrap();
    assert!(result.get("result").is_some(), "{result}");
    drop(reader);
    drop(peer);
    rx.recv_timeout(Duration::from_secs(5)).unwrap();
    worker.join().unwrap();
    assert_eq!(supervisor.connections.load(Ordering::Acquire), 0);
}

#[test]
fn expired_frame_budget_is_not_rearmed_by_buffered_bytes() {
    let (mut peer, server) = UnixStream::pair().unwrap();
    peer.write_all(b"{}\n").unwrap();
    let mut reader = BufReader::new(server);
    assert!(!reader.fill_buf().unwrap().is_empty());
    let result = read_frame(&mut reader, Some(Instant::now()), FRAME_BUDGET);
    assert!(matches!(
        result,
        Err(Error::Refused(
            "Supervisor handshake or frame deadline exceeded"
        ))
    ));
}
