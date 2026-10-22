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

#[test]
fn buffer_keeps_head_and_tail_like_python() {
    let mut buffer = Buffer::new(6);
    buffer.append(b"abcdefghij");
    assert_eq!(buffer.head, b"abc");
    assert_eq!(buffer.tail, b"hij");
    assert_eq!(buffer.total, 10);
    let head = buffer.read(0, 32, true);
    assert_eq!(head["text"], "abc");
    assert_eq!(head["nextOffset"], 3);
    assert_eq!(head["truncated"], true);
    let tail = buffer.read(3, 32, true);
    assert_eq!(tail["offset"], 7);
    assert_eq!(tail["gapBytes"], 4);
    assert_eq!(tail["text"], "hij");
}

#[test]
fn a_page_never_splits_a_character_until_the_output_is_final() {
    let mut buffer = Buffer::new(100);
    buffer.append("é".as_bytes());
    buffer.append(&[0xe2, 0x82]);
    let open = buffer.read(0, 32, false);
    assert_eq!(open["text"], "é");
    assert_eq!(open["nextOffset"], 2);
    let last = buffer.read(0, 32, true);
    assert_eq!(last["text"], "é\u{fffd}");
    assert_eq!(last["nextOffset"], 4);
}

#[test]
fn the_private_marker_is_redacted_across_chunks() {
    let mut redactor = MarkerRedactor::new("secret");
    let mut out = redactor.feed(b"a sec", false);
    out.extend(redactor.feed(b"ret b", false));
    out.extend(redactor.feed(b"", true));
    assert_eq!(out, b"a [private marker] b");
    let mut redactor = MarkerRedactor::new("secret");
    let mut out = redactor.feed(b"sec", false);
    out.extend(redactor.feed(b"", true));
    assert_eq!(out, b"sec");
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
