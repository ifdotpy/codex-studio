#![allow(clippy::unwrap_used, clippy::duplicate_mod)]

use super::*;

#[test]
fn output_tail_append_does_not_move_retained_bytes() {
    let mut buffer = Buffer::new(16);
    buffer.append(b"0123456789abcdef");
    buffer.tail.reserve(1);
    let retained: Vec<_> = (1..buffer.tail.len())
        .map(|index| &buffer.tail[index] as *const u8)
        .collect();
    buffer.append(b"g");
    let moved_bytes = retained
        .iter()
        .enumerate()
        .filter(|(index, address)| !std::ptr::eq(**address, &buffer.tail[*index]))
        .count();
    assert_eq!(moved_bytes, 0, "appending must not shift the retained tail");
    assert_eq!(buffer.read(8, 8, true)["text"], "9abcdefg");
    assert_eq!(Buffer::from_snapshot(Some(&buffer.snapshot())), buffer);
}

/// Frozen results from the former backend for the same buffer and request inputs.
fn oracle() -> Value {
    serde_json::from_str(include_str!("../../tests/server_exec/expected.json")).unwrap()
}

fn hex(value: &Value) -> Vec<u8> {
    data_encoding::HEXLOWER
        .decode(value.as_str().unwrap().as_bytes())
        .unwrap()
}

#[test]
fn pages_redaction_and_request_checks_match_python() {
    let oracle = oracle();
    for case in oracle["buffers"].as_array().unwrap() {
        let mut buffer = Buffer::new(case["limit"].as_u64().unwrap() as usize);
        for chunk in case["chunks"].as_array().unwrap() {
            buffer.append(&hex(chunk));
        }
        assert_eq!(buffer.head, hex(&case["head"]), "{case}");
        assert_eq!(buffer.tail, hex(&case["tail"]), "{case}");
        assert_eq!(json!(buffer.total), case["total"], "{case}");
        for read in case["reads"].as_array().unwrap() {
            let page = buffer.read(
                read["offset"].as_u64().unwrap() as usize,
                read["limit"].as_u64().unwrap() as usize,
                read["final"].as_bool().unwrap(),
            );
            assert_eq!(page, read["page"], "{case} {read}");
        }
    }
    for case in oracle["redactions"].as_array().unwrap() {
        let mut redactor = MarkerRedactor::new(case["secret"].as_str().unwrap());
        let chunks = case["chunks"].as_array().unwrap();
        for (index, (chunk, expected)) in chunks
            .iter()
            .zip(case["out"].as_array().unwrap())
            .enumerate()
        {
            let out = redactor.feed(&hex(chunk), index + 1 == chunks.len());
            assert_eq!(out, hex(expected), "{case} chunk {index}");
        }
    }
    for case in oracle["validate"].as_array().unwrap() {
        let result = crate::multi_server_tools::validate_exec(
            &case["payload"],
            case["foreign"].as_bool().unwrap(),
        );
        match (&result, case.get("value")) {
            (Ok(value), Some(expected)) => assert_eq!(value, expected, "{case}"),
            (Err(message), None) => assert_eq!(json!(message), case["error"], "{case}"),
            _ => panic!("{case}: {result:?}"),
        }
    }
}

struct NoEvents;

impl ExecEvents for NoEvents {
    fn completed<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a Value,
    ) -> futures_util::future::BoxFuture<'a, ()> {
        Box::pin(async {})
    }
}

async fn snapshot_job(root: &Path) -> Job {
    let store = Arc::new(RuntimeStore::open(Some(root)).await.unwrap());
    let folder = root.join(OUTPUT_FOLDER);
    create_private_folder(&folder).unwrap();
    Job {
        runner: Arc::new(ServerExec {
            store,
            folder,
            events: Arc::new(NoEvents),
            active: Mutex::new(HashMap::new()),
            restored: tokio::sync::OnceCell::new(),
            pruned: Mutex::new(None),
            snapshot_reads: std::sync::atomic::AtomicUsize::new(0),
        }),
        principal: "principal".into(),
        actor: "actor".into(),
        key: "snapshot-witness".into(),
        argv: Vec::new(),
        cwd: root.to_string_lossy().into_owned(),
        environment: Vec::new(),
        marker: "private-fixture-marker".into(),
        timeout: SYNC_SECONDS,
        stdout: Buffer::new(READ_OUTPUT),
        stderr: Buffer::new(READ_OUTPUT),
        record: json!({"status": "running"}),
        snapshot_dirty: true,
        last_publish: tokio::time::Instant::now(),
    }
}

// Keep the old inode alive so an atomic replacement cannot reuse its identity.
#[cfg(unix)]
fn retain_snapshot(path: &Path, witness: &str) -> u64 {
    use std::os::unix::fs::MetadataExt;
    std::fs::hard_link(path, path.with_extension(witness)).unwrap();
    std::fs::metadata(path).unwrap().ino()
}

#[cfg(unix)]
#[tokio::test]
async fn snapshots_skip_idle_intervals_and_coalesce_changed_output() {
    use std::os::unix::fs::MetadataExt;
    let root = tempfile::tempdir().unwrap();
    let mut job = snapshot_job(root.path()).await;
    tokio::time::pause();
    job.publish().await;
    let path = job_path(&job.runner.folder, &job.key, ".output.json");
    let first = retain_snapshot(&path, "initial");
    for _ in 0..3 {
        tokio::time::advance(SNAPSHOT_PUBLISH_INTERVAL).await;
        job.append_output(true, b"");
        job.publish_if_due().await;
        assert_eq!(std::fs::metadata(&path).unwrap().ino(), first);
    }
    // Several output events before the next publication form one snapshot.
    job.append_output(true, b"one");
    job.append_output(false, b"two");
    job.publish_if_due().await;
    let changed = retain_snapshot(&path, "changed");
    assert_ne!(changed, first);
    job.append_output(true, b"three");
    job.publish_if_due().await;
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), changed);
    tokio::time::advance(SNAPSHOT_PUBLISH_INTERVAL).await;
    job.publish_if_due().await;
    let next = retain_snapshot(&path, "next");
    assert_ne!(next, changed);
    let snapshot = job.runner.snapshot(&job.key).unwrap();
    assert_eq!(hex(&snapshot["stdout"]["head"]), b"onethree");
    assert_eq!(hex(&snapshot["stderr"]["head"]), b"two");
    tokio::time::advance(SNAPSHOT_PUBLISH_INTERVAL).await;
    job.publish_if_due().await;
    assert_eq!(std::fs::metadata(&path).unwrap().ino(), next);

    // Final metadata must publish immediately, even with no output or elapsed interval.
    let runner = Arc::clone(&job.runner);
    let key = job.key.clone();
    job.record["status"] = json!("completed");
    job.finish(Instant::now()).await;
    let final_snapshot = runner.snapshot(&key).unwrap();
    assert_eq!(final_snapshot["record"]["status"], "completed");
    assert!(final_snapshot["record"]["finishedAt"].is_number());
    assert_ne!(std::fs::metadata(&path).unwrap().ino(), next);
}

#[tokio::test]
async fn snapshot_write_failure_stays_dirty_and_retries_at_the_interval() {
    let root = tempfile::tempdir().unwrap();
    let mut job = snapshot_job(root.path()).await;
    tokio::time::pause();
    std::fs::remove_dir(&job.runner.folder).unwrap();
    job.publish().await;
    assert!(job.snapshot_dirty);
    create_private_folder(&job.runner.folder).unwrap();
    job.publish_if_due().await;
    assert!(job.runner.snapshot(&job.key).is_none());
    tokio::time::advance(SNAPSHOT_PUBLISH_INTERVAL).await;
    job.publish_if_due().await;
    assert!(!job.snapshot_dirty);
    assert!(job.runner.snapshot(&job.key).is_some());
}

#[tokio::test]
async fn existing_start_fence_finishes_unknown_without_starting_a_process() {
    let root = tempfile::tempdir().unwrap();
    // An empty argv would panic at process construction if the fence were bypassed.
    let job = snapshot_job(root.path()).await;
    std::fs::write(job_path(&job.runner.folder, &job.key, ".started"), b"").unwrap();
    let runner = Arc::clone(&job.runner);
    let key = job.key.clone();
    let (_sender, receiver) = tokio::sync::mpsc::channel(1);
    job.run(receiver).await;
    let snapshot = runner.snapshot(&key).unwrap();
    assert_eq!(snapshot["record"]["status"], "unknown");
    assert_eq!(
        snapshot["record"]["error"],
        "A prior command start exists. The command was not run again."
    );
}

#[cfg(unix)]
#[tokio::test]
async fn snapshots_include_real_command_output_and_final_state() {
    let root = tempfile::tempdir().unwrap();
    let mut job = snapshot_job(root.path()).await;
    job.argv = vec![
        "/bin/sh".into(),
        "-c".into(),
        "printf stdout; printf stderr >&2".into(),
    ];
    let runner = Arc::clone(&job.runner);
    let key = job.key.clone();
    let (_sender, receiver) = tokio::sync::mpsc::channel(1);
    job.run(receiver).await;
    let snapshot = runner.snapshot(&key).unwrap();
    assert_eq!(snapshot["record"]["status"], "completed");
    assert_eq!(snapshot["record"]["exitCode"], 0);
    assert_eq!(hex(&snapshot["stdout"]["head"]), b"stdout");
    assert_eq!(hex(&snapshot["stderr"]["head"]), b"stderr");
    assert!(job_path(&runner.folder, &key, ".started").exists());
}

#[test]
fn snapshots_wait_for_the_blocking_pool_without_blocking_the_async_worker() {
    const FIXTURE_BLOCKING_THREADS: usize = 1;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .max_blocking_threads(FIXTURE_BLOCKING_THREADS)
        .build()
        .unwrap();
    let root = tempfile::tempdir().unwrap();
    runtime.block_on(async {
        let mut job = snapshot_job(root.path()).await;
        let path = job_path(&job.runner.folder, &job.key, ".output.json");
        let (entered, waiting) = tokio::sync::oneshot::channel();
        let (release, gate) = std::sync::mpsc::channel();
        let held = studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/server_exec/test_gate",
            move || {
                entered.send(()).unwrap();
                gate.recv().unwrap();
            },
        );
        waiting.await.unwrap();
        let mut publish = std::pin::pin!(job.publish());
        let queued = futures_util::poll!(&mut publish).is_pending();
        let written_before_release = path.exists();
        // Release even when the assertion will fail, so the runtime can shut down.
        release.send(()).unwrap();
        held.await.unwrap();
        assert!(queued, "publication must await the occupied blocking pool");
        assert!(!written_before_release);
        publish.await;
        assert!(path.exists());
    });
}

struct CompletedEvent(tokio::sync::Notify);

impl ExecEvents for CompletedEvent {
    fn completed<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a Value,
    ) -> futures_util::future::BoxFuture<'a, ()> {
        Box::pin(async { self.0.notify_one() })
    }
}

#[cfg(unix)]
#[tokio::test]
async fn synchronous_start_reads_one_snapshot_after_job_completion() {
    let root = tempfile::tempdir().unwrap();
    let mut job = snapshot_job(root.path()).await;
    let completed = Arc::new(CompletedEvent(tokio::sync::Notify::new()));
    Arc::get_mut(&mut job.runner).unwrap().events = completed.clone();
    let runner = job.runner;
    let payload = json!({
        "actor": "actor", "cwd": root.path(), "timeout": SYNC_SECONDS,
        "command": ["/bin/sh", "-c", "printf stdout; printf stderr >&2"]
    });
    tokio::time::pause();
    let start = runner.start("principal", "local", &payload, "waited-command");
    tokio::pin!(start);
    let (result, ()) = with_clock_frozen(async {
        tokio::join!(&mut start, async {
            completed.0.notified().await;
            tokio::time::advance(Duration::from_secs(1)).await;
        })
    })
    .await;
    let result = result.unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["exitCode"], 0);
    assert_eq!(result["stdout"], "stdout");
    assert_eq!(result["stderr"], "stderr");
    assert_eq!(
        runner
            .snapshot_reads
            .load(std::sync::atomic::Ordering::Relaxed),
        1
    );
    assert!(runner.active.lock().unwrap().is_empty());
}

// Drive real filesystem/process work without allowing Tokio's paused clock to
// advance automatically while the blocking pool or child process is scheduled.
async fn with_clock_frozen<F: std::future::Future>(future: F) -> F::Output {
    const FIXTURE_COMPLETION_TIMEOUT: Duration = Duration::from_secs(10);
    tokio::pin!(future);
    let began = Instant::now();
    loop {
        assert!(began.elapsed() < FIXTURE_COMPLETION_TIMEOUT);
        tokio::select! {
            biased;
            result = &mut future => return result,
            () = tokio::task::yield_now() => {}
        }
    }
}

struct HeldCompletion {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}

impl ExecEvents for HeldCompletion {
    fn completed<'a>(
        &'a self,
        _: &'a str,
        _: &'a str,
        _: &'a Value,
    ) -> futures_util::future::BoxFuture<'a, ()> {
        Box::pin(async {
            self.entered.notify_one();
            self.release.notified().await;
        })
    }
}

async fn held_completion_runner(root: &Path) -> (Arc<ServerExec>, Arc<HeldCompletion>) {
    let mut job = snapshot_job(root).await;
    let events = Arc::new(HeldCompletion {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    Arc::get_mut(&mut job.runner).unwrap().events = events.clone();
    (job.runner, events)
}

#[cfg(unix)]
#[tokio::test]
async fn synchronous_start_wait_timeout_detaches_the_job_without_reading_output() {
    let root = tempfile::tempdir().unwrap();
    let (runner, events) = held_completion_runner(root.path()).await;
    let payload = json!({
        "actor": "actor", "cwd": root.path(), "timeout": SYNC_SECONDS,
        "command": ["/bin/sh", "-c", "printf retained"]
    });
    tokio::time::pause();
    let start = runner.start("principal", "local", &payload, "wait-timeout");
    tokio::pin!(start);
    with_clock_frozen(async {
        tokio::select! {
            result = &mut start => panic!("returned before completion: {result:?}"),
            () = events.entered.notified() => {}
        }
    })
    .await;
    assert_eq!(
        runner
            .snapshot_reads
            .load(std::sync::atomic::Ordering::Relaxed),
        0
    );
    tokio::time::advance(SYNC_WAIT_TIMEOUT).await;
    let result = start.await.unwrap();
    assert_eq!(result["status"], "starting");
    assert!(runner.active.lock().unwrap().contains_key("wait-timeout"));
    events.release.notify_one();
    with_clock_frozen(async {
        while !runner.active.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let result = runner
        .read(
            "principal",
            &json!({"actor": "actor", "handle": "wait-timeout"}),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["stdout"], "retained");
}

#[cfg(unix)]
#[tokio::test]
async fn synchronous_start_lost_response_keeps_the_job_and_rejects_reexecution() {
    let root = tempfile::tempdir().unwrap();
    let (runner, events) = held_completion_runner(root.path()).await;
    let payload = json!({
        "actor": "actor", "cwd": root.path(), "timeout": SYNC_SECONDS,
        "command": ["/bin/sh", "-c", "printf retained"]
    });
    tokio::time::pause();
    {
        let start = runner.start("principal", "local", &payload, "lost-response");
        tokio::pin!(start);
        with_clock_frozen(async {
            tokio::select! {
                result = &mut start => panic!("returned before completion: {result:?}"),
                () = events.entered.notified() => {}
            }
        })
        .await;
        // Drop the caller while Job::finish is still delivering its completion.
    }
    assert!(runner.active.lock().unwrap().contains_key("lost-response"));
    events.release.notify_one();
    with_clock_frozen(async {
        while !runner.active.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await;
    let result = runner
        .read(
            "principal",
            &json!({"actor": "actor", "handle": "lost-response"}),
        )
        .await
        .unwrap();
    assert_eq!(result["status"], "completed");
    assert_eq!(result["stdout"], "retained");
    assert!(matches!(
        runner
            .start("principal", "local", &payload, "lost-response")
            .await,
        Err(ExecError::Failed(_))
    ));
}

#[cfg(unix)]
#[tokio::test]
async fn synchronous_start_explicit_cancel_still_stops_the_job() {
    let root = tempfile::tempdir().unwrap();
    let runner = snapshot_job(root.path()).await.runner;
    let payload = json!({
        "actor": "actor", "cwd": root.path(), "timeout": SYNC_SECONDS,
        "command": ["/bin/sh", "-c", "read input"]
    });
    tokio::time::pause();
    let (result, cancelled) = with_clock_frozen(async {
        tokio::join!(
            runner.start("principal", "local", &payload, "cancel-waited"),
            async {
                let fence = job_path(&runner.folder, "cancel-waited", ".started");
                while !fence.exists() {
                    tokio::task::yield_now().await;
                }
                runner
                    .control(
                        "principal",
                        "exec_cancel",
                        &json!({"actor": "actor", "handle": "cancel-waited"}),
                        "cancel-request",
                    )
                    .await
            }
        )
    })
    .await;
    assert_eq!(cancelled.unwrap()["accepted"], true);
    let result = result.unwrap();
    assert_eq!(result["status"], "cancelled");
    assert_eq!(result["timedOut"], false);
    assert!(runner.active.lock().unwrap().is_empty());
}
