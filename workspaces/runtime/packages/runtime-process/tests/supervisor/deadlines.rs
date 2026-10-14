#![allow(clippy::unwrap_used)]
use super::*;
use std::sync::mpsc;

fn fixture() -> (tempfile::TempDir, Arc<Supervisor>) {
    let root = tempfile::tempdir().unwrap();
    let supervisor = Arc::new(Supervisor {
        root: root.path().to_owned(),
        journal: Arc::new(SupervisorJournal::open(root.path()).unwrap()),
        children: Mutex::new(BTreeMap::new()),
        active_children: AtomicUsize::new(0),
        ownership: Mutex::new(Ownership::default()),
        recovery: Mutex::new(json!({"blocked":null})),
        connections: AtomicUsize::new(1),
    });
    (root, supervisor)
}

#[test]
fn blocked_write_on_one_handle_does_not_block_another_handle() {
    let (_root, supervisor) = fixture();
    let open = |handle: &str| {
        supervisor
            .handle(
                Command::Open {
                    handle: handle.to_owned(),
                    command: vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        "IFS= read -r line; exit 0".into(),
                    ],
                    env: BTreeMap::new(),
                    cwd: Some(supervisor.root.to_string_lossy().into_owned()),
                },
                false,
            )
            .unwrap();
        supervisor
            .child_slot(handle, false)
            .unwrap()
            .unwrap()
            .child
            .lock()
            .unwrap()
            .clone()
            .unwrap()
    };
    let slow_handle = "server-command:slow-write";
    let other_handle = "server-command:other-operation";
    let slow_child = open(slow_handle);
    let other_child = open(other_handle);
    let slow_slot = supervisor.child_slot(slow_handle, false).unwrap().unwrap();
    let write_gate = slow_child.write_lock.lock().unwrap();
    let (slow_done_tx, slow_done_rx) = mpsc::channel();
    let slow_supervisor = supervisor.clone();
    let slow_write = thread::spawn(move || {
        let result = slow_supervisor.child_action(Command::Write {
            handle: slow_handle.into(),
            operation_id: "monitor:blocked-child-write".into(),
            native_id: Some(1),
            message: json!({"method":"finish"}).as_object().unwrap().clone(),
        });
        slow_done_tx.send(result).unwrap();
    });

    // Wait until the real child_action owns this child's operation lock and is
    // blocked at its write lock. This establishes the gate without a timing
    // threshold; the deadline below is only a hang guard.
    let operation_deadline = Instant::now() + Duration::from_secs(5);
    let write_is_gated = loop {
        match slow_slot.operation.try_lock() {
            Err(std::sync::TryLockError::WouldBlock) => break true,
            Err(std::sync::TryLockError::Poisoned(_)) => break false,
            Ok(guard) => drop(guard),
        }
        if Instant::now() >= operation_deadline {
            break false;
        }
        thread::yield_now();
    };

    let (other_done_tx, other_done_rx) = mpsc::channel();
    let other_supervisor = supervisor.clone();
    let independent_operation = thread::spawn(move || {
        let result = other_supervisor.child_action(Command::Status {
            handle: other_handle.into(),
        });
        other_done_tx.send(result).unwrap();
    });
    // The slow child's actual write lock stays held until this other child's
    // supervisor operation finishes.
    let other_finished_while_gated = other_done_rx.recv_timeout(Duration::from_secs(5)).ok();
    let other_finished_before_release = other_finished_while_gated.is_some();
    drop(write_gate);
    let slow_result = slow_done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    slow_write.join().unwrap();
    let other_result = other_finished_while_gated
        .unwrap_or_else(|| other_done_rx.recv_timeout(Duration::from_secs(5)).unwrap());
    independent_operation.join().unwrap();

    // The gated write exits the first child. Finish the second so neither real
    // child outlives the test.
    let finish_other = supervisor.child_action(Command::Write {
        handle: other_handle.into(),
        operation_id: "monitor:finish-other-child".into(),
        native_id: Some(1),
        message: json!({"method":"finish"}).as_object().unwrap().clone(),
    });
    assert!(write_is_gated);
    assert!(
        other_finished_before_release,
        "other operation waited for the gated write"
    );
    slow_result.unwrap();
    other_result.unwrap();
    finish_other.unwrap();

    for (handle, child) in [(slow_handle, slow_child), (other_handle, other_child)] {
        let reap_deadline = Instant::now() + Duration::from_secs(5);
        while child.code().unwrap().is_none()
            || child.stdout_alive.load(Ordering::Acquire)
            || child.stderr_alive.load(Ordering::Acquire)
        {
            assert!(
                Instant::now() < reap_deadline,
                "child cleanup timed out: {handle}"
            );
            thread::yield_now();
        }
        let sequence = supervisor
            .journal
            .handle(handle)
            .unwrap()
            .unwrap()
            .snapshot
            .sequence();
        supervisor
            .child_action(Command::Ack {
                handle: handle.into(),
                sequence,
            })
            .unwrap();
        supervisor
            .child_action(Command::RetireCommand {
                handle: handle.into(),
                generation: child.generation,
            })
            .unwrap();
    }
}

#[test]
fn health_skips_a_child_while_open_is_installing_it() {
    let (_root, supervisor) = fixture();
    let slot = supervisor
        .child_slot("opening-child", true)
        .unwrap()
        .unwrap();
    let _opening = OpeningGuard(slot);
    supervisor
        .journal
        .reserve("opening-child", "signature", now().unwrap())
        .unwrap();

    let health = supervisor.handle(Command::Health, false).unwrap();
    assert!(health["handles"].as_array().unwrap().is_empty());
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
