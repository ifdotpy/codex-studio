#![allow(clippy::unwrap_used)]
use super::*;
use std::sync::mpsc;

fn fixture() -> (tempfile::TempDir, Arc<Supervisor>) {
    let root = tempfile::tempdir().unwrap();
    let journal = Arc::new(SupervisorJournal::open(root.path()).unwrap());
    let journal_id = journal
        .ensure_journal_id(SUPERVISOR_JOURNAL_ID_KEY)
        .unwrap();
    let supervisor = Arc::new(Supervisor {
        root: root.path().to_owned(),
        journal,
        journal_id,
        children: Mutex::new(BTreeMap::new()),
        active_children: AtomicUsize::new(0),
        ownership: Mutex::new(Ownership::default()),
        recovery: Mutex::new(json!({"blocked":null})),
        connections: AtomicUsize::new(1),
    });
    (root, supervisor)
}

// The durable supervisor uses Linux kernel process identities.
#[cfg(target_os = "linux")]
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
            native_id: Some(1.into()),
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
        native_id: Some(1.into()),
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
        let exit_deadline = Instant::now() + Duration::from_secs(5);
        while child.durable_exit_sequence().unwrap().is_none() {
            assert!(
                Instant::now() < exit_deadline,
                "exit event commit timed out: {handle}"
            );
            thread::yield_now();
        }
        let exit_sequence = child.durable_exit_sequence().unwrap().unwrap();
        let sequence = supervisor
            .journal
            .handle(handle)
            .unwrap()
            .unwrap()
            .snapshot
            .sequence();
        assert_eq!(sequence, exit_sequence);
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
        // The worker has closed its socket. Darwin rejects changing SO_RCVTIMEO
        // after peer shutdown; use the five-second timeout installed above.
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let error: Value = serde_json::from_str(&line).unwrap();
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

// This exercises a real retained child with Linux kernel process identity.
#[cfg(target_os = "linux")]
#[test]
fn v2_wait_disconnect_preserves_child_and_shared_backend_owner() {
    use std::io::{BufRead, Write};
    use std::os::unix::net::UnixStream;

    fn connected(
        supervisor: Arc<Supervisor>,
        backend: &str,
    ) -> (
        UnixStream,
        thread::JoinHandle<()>,
        mpsc::Receiver<()>,
        Value,
    ) {
        let (server, mut client) = UnixStream::pair().unwrap();
        client
            .set_read_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        client
            .set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let root = supervisor.root.to_string_lossy().into_owned();
        supervisor.connections.fetch_add(1, Ordering::AcqRel);
        let worker_supervisor = supervisor.clone();
        let (done_tx, done_rx) = mpsc::channel();
        let worker = thread::spawn(move || {
            connection_with_budget(worker_supervisor, server, Duration::from_secs(3));
            let _ignored = done_tx.send(());
        });
        writeln!(
            client,
            "{}",
            json!({"protocol":SUPERVISOR_PROTOCOL_V2,"stateDir":root,"backendId":backend})
        )
        .unwrap();
        client.flush().unwrap();
        let mut reader = std::io::BufReader::new(&mut client);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let response: Value = serde_json::from_str(&line).unwrap();
        drop(reader);
        (client, worker, done_rx, response)
    }

    let (root, supervisor) = fixture();
    let handle = "v2-disconnect-child";
    let opened = supervisor
        .handle(
            Command::Open {
                handle: handle.into(),
                command: vec!["/bin/sh".into(), "-c".into(), "sleep 30".into()],
                env: BTreeMap::new(),
                cwd: Some(root.path().to_string_lossy().into_owned()),
            },
            false,
        )
        .unwrap();
    let child = supervisor
        .child_slot(handle, false)
        .unwrap()
        .unwrap()
        .child
        .lock()
        .unwrap()
        .clone()
        .unwrap();

    let (mut control, control_worker, control_done, control_handshake) =
        connected(supervisor.clone(), "backend-a");
    let (mut waiter, waiter_worker, waiter_done, waiter_handshake) =
        connected(supervisor.clone(), "backend-a");
    for handshake in [&control_handshake, &waiter_handshake] {
        assert_eq!(handshake["ok"], true);
        assert_eq!(handshake["protocol"], SUPERVISOR_PROTOCOL_V2);
    }
    writeln!(
        waiter,
        "{}",
        json!({
            "requestId":1,
            "action":"nextBatch",
            "handle":handle,
            "cursor":0,
            "generation":opened["generation"],
            "limit":SUPERVISOR_NEXT_BATCH_MAX_EVENTS
        })
    )
    .unwrap();
    waiter.flush().unwrap();
    drop(waiter);
    waiter_done
        .recv_timeout(Duration::from_secs(3))
        .expect("disconnected event wait must finish promptly");
    waiter_worker.join().unwrap();
    assert!(
        child.code().unwrap().is_none(),
        "disconnecting the event wait must leave its native child running"
    );

    let (other_backend, other_worker, other_done, rejection) =
        connected(supervisor.clone(), "backend-b");
    assert!(
        rejection["error"]
            .as_str()
            .unwrap()
            .contains("Another backend")
    );
    other_done
        .recv_timeout(Duration::from_secs(3))
        .expect("rejected backend handshake must finish");
    other_worker.join().unwrap();

    writeln!(
        control,
        "{}",
        json!({"requestId":2,"action":"status","handle":handle})
    )
    .unwrap();
    control.flush().unwrap();
    let mut line = String::new();
    std::io::BufReader::new(&mut control)
        .read_line(&mut line)
        .unwrap();
    let status: Value = serde_json::from_str(&line).unwrap();
    assert_eq!(status["requestId"], 2);
    assert!(status["result"]["returnCode"].is_null());

    drop(other_backend);
    drop(control);
    control_done
        .recv_timeout(Duration::from_secs(3))
        .expect("control disconnect must finish");
    control_worker.join().unwrap();
    let row = supervisor.journal.handle(handle).unwrap().unwrap();
    stop_verified(child.pid, row.pgid.unwrap(), &child.start).unwrap();
    child.stopping.store(true, Ordering::Release);
    child.reap().unwrap();
}

// This exercises a real retained child with Linux kernel process identity.
#[cfg(target_os = "linux")]
#[test]
#[allow(clippy::disallowed_methods)]
fn v2_batches_do_not_mark_terminal_until_the_exit_sequence_is_returned() {
    let (root, supervisor) = fixture();
    let handle = "server-command:batch-exit-tail";
    let opened = supervisor
        .handle(
            Command::Open {
                handle: handle.into(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "i=0; while [ $i -lt 70 ]; do printf '{\"id\":1,\"method\":\"tick\"}\\n'; i=$((i+1)); done"
                        .into(),
                ],
                env: BTreeMap::new(),
                cwd: Some(root.path().to_string_lossy().into_owned()),
            },
            false,
        )
        .unwrap();
    let child = supervisor
        .child_slot(handle, false)
        .unwrap()
        .unwrap()
        .child
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    child.reap().unwrap();
    // Hang guard only: 71 durable appends take well under a second when idle.
    let deadline = Instant::now() + Duration::from_secs(30);
    let exit_sequence = loop {
        if let Some(sequence) = child.durable_exit_sequence().unwrap() {
            break sequence;
        }
        assert!(
            Instant::now() < deadline,
            "child exit event did not become durable"
        );
        thread::sleep(Duration::from_millis(10));
    };
    assert_eq!(exit_sequence, 71);

    // The open receipt names this handle row of this journal.
    assert!(
        opened[SUPERVISOR_JOURNAL_EPOCH_FIELD]
            .as_str()
            .unwrap()
            .starts_with(&format!("{}:", supervisor.journal_id))
    );
    let generation = crate::Generation(opened["generation"].as_u64().unwrap());
    let first = supervisor
        .next_batch_ready(&crate::NextBatchRequest {
            handle: crate::SupervisorHandle(handle.into()),
            cursor: crate::EventCursor(0),
            generation,
            limit: SUPERVISOR_NEXT_BATCH_MAX_EVENTS,
        })
        .unwrap()
        .unwrap();
    assert_eq!(first.0.events.len(), SUPERVISOR_NEXT_BATCH_MAX_EVENTS);
    assert_eq!(first.0.events.last().unwrap().sequence.0, 64);
    assert!(first.0.return_code.is_none());

    let last = supervisor
        .next_batch_ready(&crate::NextBatchRequest {
            handle: crate::SupervisorHandle(handle.into()),
            cursor: first.0.events.last().unwrap().sequence,
            generation,
            limit: SUPERVISOR_NEXT_BATCH_MAX_EVENTS,
        })
        .unwrap()
        .unwrap();
    assert_eq!(last.0.events.len(), 7);
    assert_eq!(last.0.events.last().unwrap().sequence.0, exit_sequence);
    assert_eq!(last.0.events.last().unwrap().kind, crate::EventKind::Exit);
    assert_eq!(last.0.return_code, Some(0));
}

// This exercises a real retained child with Linux kernel process identity.
#[cfg(target_os = "linux")]
#[test]
fn empty_batch_queried_before_exit_commit_is_not_terminal_after_commit() {
    let (root, supervisor) = fixture();
    let handle = "server-command:query-exit-race";
    let opened = supervisor
        .handle(
            Command::Open {
                handle: handle.into(),
                command: vec![
                    "/bin/sh".into(),
                    "-c".into(),
                    "IFS= read -r line; exit 0".into(),
                ],
                env: BTreeMap::new(),
                cwd: Some(root.path().to_string_lossy().into_owned()),
            },
            false,
        )
        .unwrap();
    let child = supervisor
        .child_slot(handle, false)
        .unwrap()
        .unwrap()
        .child
        .lock()
        .unwrap()
        .clone()
        .unwrap();
    let generation = opened["generation"].as_u64().unwrap();

    let append_gate = child.lock_append_for_test().unwrap();
    let stale_query = supervisor
        .journal
        .events_v2(
            handle,
            0,
            generation,
            SUPERVISOR_NEXT_BATCH_MAX_EVENTS,
            crate::transport::MAX_FRAME_BYTES,
        )
        .unwrap();
    assert!(stale_query.is_empty());

    child.write(&json!({"finish":true})).unwrap();
    child.reap().unwrap();
    let deadline = Instant::now() + Duration::from_secs(5);
    while child.stdout_alive.load(Ordering::Acquire) || child.stderr_alive.load(Ordering::Acquire) {
        assert!(Instant::now() < deadline, "child readers did not finish");
        thread::yield_now();
    }
    assert!(child.durable_exit_sequence().unwrap().is_none());
    drop(append_gate);

    let deadline = Instant::now() + Duration::from_secs(5);
    let exit_sequence = loop {
        if let Some(sequence) = child.durable_exit_sequence().unwrap() {
            break sequence;
        }
        assert!(Instant::now() < deadline, "exit append did not commit");
        thread::yield_now();
    };
    let stale_last = stale_query.last().map(|event| event.sequence);
    assert!(!exit_delivered(0, stale_last, Some(exit_sequence)));

    let fresh_query = supervisor
        .journal
        .events_v2(
            handle,
            0,
            generation,
            SUPERVISOR_NEXT_BATCH_MAX_EVENTS,
            crate::transport::MAX_FRAME_BYTES,
        )
        .unwrap();
    assert_eq!(fresh_query.last().unwrap().sequence, exit_sequence);
    assert!(exit_delivered(
        0,
        fresh_query.last().map(|event| event.sequence),
        Some(exit_sequence)
    ));
}

#[test]
fn frame_trimming_cannot_hide_terminal_exit_or_succeed_with_oversized_first_event() {
    use serde_json::value::RawValue;

    let event = |sequence, kind, payload| crate::SupervisorEventV2 {
        sequence: crate::EventCursor(sequence),
        generation: crate::Generation(1),
        kind,
        payload: RawValue::from_string(payload).unwrap(),
    };
    let mut result = crate::NextBatchResult {
        events: vec![
            event(1, crate::EventKind::Stdout, "{}".into()),
            event(
                2,
                crate::EventKind::Exit,
                format!("\"{}\"", "x".repeat(crate::transport::MAX_FRAME_BYTES)),
            ),
        ],
        return_code: Some(0),
        backpressure: false,
        stdout_reader_alive: false,
        stderr_reader_alive: false,
    };
    let bytes = batch_response(Some(1), 0, Some(2), result.clone()).unwrap();
    let response: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(response["result"]["events"].as_array().unwrap().len(), 1);
    assert_eq!(response["result"]["events"][0]["sequence"], 1);
    assert!(response["result"]["returnCode"].is_null());

    result.events = vec![event(
        1,
        crate::EventKind::Stdout,
        format!("\"{}\"", "x".repeat(crate::transport::MAX_FRAME_BYTES)),
    )];
    result.return_code = None;
    assert!(batch_response(Some(2), 0, None, result).is_err());
}

#[test]
fn an_accept_error_skips_one_connection_and_keeps_listening() {
    let error = std::io::Error::from(rustix::io::Errno::MFILE);
    assert!(accepted(Err(error)).is_none());
    let (left, _right) = UnixStream::pair().unwrap();
    assert!(accepted(Ok(left)).is_some());
}
