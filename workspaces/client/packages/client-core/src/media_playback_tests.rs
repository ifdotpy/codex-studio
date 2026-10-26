use super::*;
use std::sync::Arc;
use tokio::sync::oneshot;
const MOCK_DEVICE_WAIT: Duration = Duration::from_secs(5);

struct MockPlayer {
    id: usize,
    stopped: Arc<Mutex<Vec<usize>>>,
    _directory: tempfile::TempDir,
}
impl PlaybackPlayer for MockPlayer {
    fn stop(self) {
        self.stopped
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .push(self.id);
    }
}

#[derive(Default)]
pub(crate) struct MockDevice {
    player: Mutex<Playback<MockPlayer>>,
    stopped: Arc<Mutex<Vec<usize>>>,
    starts: Mutex<usize>,
    started: tokio::sync::Notify,
    fail_start: std::sync::atomic::AtomicBool,
    delayed_stop: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    delayed_start: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}
impl MockDevice {
    pub(crate) async fn wait_for_start(&self) {
        tokio::time::timeout(MOCK_DEVICE_WAIT, async {
            loop {
                let notified = self.started.notified();
                if self.starts() > 0 {
                    return;
                }
                notified.await;
            }
        })
        .await
        .expect("the owned mock device starts");
    }
    pub(crate) fn starts(&self) -> usize {
        *self.starts.lock().unwrap()
    }
    pub(crate) fn stopped(&self) -> Vec<usize> {
        self.stopped
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .clone()
    }
}
fn preview_mock() -> &'static Mutex<Option<Arc<MockDevice>>> {
    static MOCK: OnceLock<Mutex<Option<Arc<MockDevice>>>> = OnceLock::new();
    MOCK.get_or_init(Mutex::default)
}
pub(crate) struct PreviewMock(pub Arc<MockDevice>);
impl Drop for PreviewMock {
    fn drop(&mut self) {
        preview_mock().lock().unwrap().take();
    }
}
pub(crate) fn install_preview_mock() -> PreviewMock {
    let device = Arc::new(MockDevice::default());
    *preview_mock().lock().unwrap() = Some(device.clone());
    PreviewMock(device)
}
pub(crate) fn current_preview_mock() -> Option<Arc<MockDevice>> {
    preview_mock().lock().unwrap().clone()
}
impl Drop for MockDevice {
    fn drop(&mut self) {
        self.player
            .get_mut()
            .unwrap_or_else(|p| p.into_inner())
            .stop_all();
    }
}
impl Default for Playback<MockPlayer> {
    fn default() -> Self {
        Self::new()
    }
}
impl MockDevice {
    fn finish_player(
        &self,
        owner: crate::delivery::StoreSender,
        token: String,
        directory: tempfile::TempDir,
        duration: Option<f64>,
    ) -> Result<PlaybackReceipt, String> {
        self.player.lock().unwrap().finish(owner, &token, || {
            if self.fail_start.load(std::sync::atomic::Ordering::SeqCst) {
                return Err("The mock device cannot start.".into());
            }
            let mut starts = self.starts.lock().unwrap();
            *starts += 1;
            let id = *starts;
            self.started.notify_waiters();
            Ok((
                MockPlayer {
                    id,
                    stopped: self.stopped.clone(),
                    _directory: directory,
                },
                duration,
            ))
        })
    }
    async fn wait_preparation(&self) {
        let delayed = self.delayed_start.lock().unwrap().take();
        if let Some((entered, resume)) = delayed {
            entered.send(()).unwrap();
            resume.await.unwrap();
        }
    }
}
impl PlaybackDevice for MockDevice {
    async fn play(
        &self,
        owner: crate::delivery::StoreSender,
        token: String,
        path: String,
        directory: tempfile::TempDir,
    ) -> Result<PlaybackReceipt, String> {
        let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
        assert_eq!(&bytes[44..], &[1, 0, 2, 0, 3, 0]);
        self.wait_preparation().await;
        self.finish_player(owner, token, directory, Some(0.25))
    }
    async fn preview(
        &self,
        owner: crate::delivery::StoreSender,
        token: String,
        path: String,
        _name: String,
        directory: tempfile::TempDir,
    ) -> Result<PlaybackReceipt, String> {
        use std::os::unix::fs::PermissionsExt;
        assert!(std::path::Path::new(&path).starts_with(directory.path()));
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(!std::fs::read(path).unwrap().is_empty());
        self.wait_preparation().await;
        self.finish_player(owner, token, directory, None)
    }
    async fn stop(
        &self,
        owner: crate::delivery::StoreSender,
        token: Option<String>,
    ) -> Result<(), String> {
        let delayed = self.delayed_stop.lock().unwrap().take();
        if let Some((entered, resume)) = delayed {
            entered.send(()).unwrap();
            resume.await.unwrap();
        }
        self.player
            .lock()
            .unwrap()
            .stop_token(&owner, token.as_deref())
    }
}

fn request(id: &str, method: &str, params: Value) -> crate::protocol::Request {
    crate::protocol::Request {
        id: id.into(),
        method: method.into(),
        params,
    }
}

fn store() -> (crate::delivery::StoreSender, std::thread::JoinHandle<()>) {
    let (sender, mut commands) = mpsc::channel::<crate::delivery::StoreCommand>(16);
    let worker = std::thread::spawn(move || {
        let folder = tempfile::tempdir().unwrap();
        let mut db = crate::store::Store::open(&folder.path().join("client.sqlite3")).unwrap();
        let seeded = db.dispatch(request("seed", "dictation.chunk", json!({
            "row":{"id":"recording","chatId":"chat","created":1,"sampleRate":16000,"samples":3,"state":"ready"},
            "index":0,"base64":encode_pcm(&[1,2,3])
        })));
        assert!(seeded.error.is_none(), "{seeded:?}");
        while let Some((request, reply)) = commands.blocking_recv() {
            let _ = reply.send(db.dispatch(request));
        }
    });
    (sender, worker)
}

async fn play(store: &crate::delivery::StoreSender, device: &MockDevice) {
    let response = dispatch_with_playback(
        request(
            "play",
            "media.playRecording",
            json!({"recording":"recording"}),
        ),
        store,
        device,
    )
    .await;
    assert!(response.error.is_none(), "{response:?}");
    assert_eq!(response.result.unwrap()["duration"], 0.25);
}

#[test]
fn playback_delayed_old_stop_cannot_stop_the_new_context_device() {
    let _serial = AUDIO_TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (old, old_worker) = store();
        let (current, current_worker) = store();
        let device = Arc::new(MockDevice::default());
        play(&old, &device).await;
        let (entered, waiting) = oneshot::channel();
        let (resume, resume_wait) = oneshot::channel();
        *device.delayed_stop.lock().unwrap() = Some((entered, resume_wait));
        let old_stop = {
            let old = old.clone();
            let device = device.clone();
            tokio::spawn(async move {
                dispatch_with_playback(
                    request(
                        "late-stop",
                        "media.stopPlayback",
                        json!({"owner":"current","serverId":"current"}),
                    ),
                    &old,
                    device.as_ref(),
                )
                .await
            })
        };
        waiting.await.unwrap();
        play(&current, &device).await;
        assert_eq!(device.stopped(), vec![1]);
        resume.send(()).unwrap();
        assert!(old_stop.await.unwrap().error.is_none());
        assert_eq!(
            device.stopped(),
            vec![1],
            "An old context stopped the new physical player."
        );
        assert!(
            !audio_idle(),
            "A foreign stop released the active audio lease."
        );
        let current_clone = current.clone();
        let stopped = dispatch_with_playback(
            request("stop", "media.stopPlayback", json!({})),
            &current_clone,
            device.as_ref(),
        )
        .await;
        assert!(stopped.error.is_none(), "{stopped:?}");
        assert_eq!(device.stopped(), vec![1, 2]);
        assert!(audio_idle());
        assert!(
            dispatch_with_playback(
                request("repeat-stop", "media.stopPlayback", json!({})),
                &old,
                device.as_ref()
            )
            .await
            .error
            .is_none()
        );
        assert_eq!(device.stopped(), vec![1, 2]);
        drop(current_clone);
        drop(device);
        drop(old);
        drop(current);
        old_worker.join().unwrap();
        current_worker.join().unwrap();
    });
}

#[test]
fn playback_failed_export_preserves_the_current_device_and_owner() {
    let _serial = AUDIO_TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (owner, worker) = store();
        let device = MockDevice::default();
        play(&owner, &device).await;
        let response = dispatch_with_playback(
            request(
                "missing",
                "media.playRecording",
                json!({"recording":"missing"}),
            ),
            &owner,
            &device,
        )
        .await;
        assert!(response.error.is_some());
        assert!(device.stopped().is_empty());
        assert!(!audio_idle());
        device.player.lock().unwrap().stop_all();
        assert!(audio_idle());
        drop(device);
        drop(owner);
        worker.join().unwrap();
    });
}

#[test]
fn playback_failed_device_start_leaves_no_active_player() {
    let _serial = AUDIO_TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (owner, worker) = store();
        let device = MockDevice::default();
        play(&owner, &device).await;
        device
            .fail_start
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let response = dispatch_with_playback(
            request(
                "failed",
                "media.playRecording",
                json!({"recording":"recording"}),
            ),
            &owner,
            &device,
        )
        .await;
        assert!(response.error.is_some());
        assert_eq!(device.stopped(), vec![1]);
        assert!(device.player.lock().unwrap().current.is_none());
        assert!(
            audio_idle(),
            "A failed device start left a false busy flag."
        );
        drop(device);
        drop(owner);
        worker.join().unwrap();
    });
}

#[test]
fn playback_unknown_token_cannot_stop_the_current_player_in_the_same_store() {
    let _serial = AUDIO_TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (owner, worker) = store();
        let device = MockDevice::default();
        play(&owner, &device).await;
        let stopped = dispatch_with_playback(
            request(
                "stale-token",
                "media.stopPlayback",
                json!({"playback":"another-generation"}),
            ),
            &owner,
            &device,
        )
        .await;
        assert!(stopped.error.is_none(), "{stopped:?}");
        assert!(
            device.stopped().is_empty(),
            "An unrelated token stopped the physical player."
        );
        assert!(!audio_idle(), "A token mismatch released the audio lease.");
        device.player.lock().unwrap().stop_all();
        drop(device);
        drop(owner);
        worker.join().unwrap();
    });
}

#[test]
fn playback_old_token_cleanup_and_foreign_store_cannot_stop_the_new_generation() {
    let _serial = AUDIO_TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (owner, worker) = store();
        let (foreign, foreign_worker) = store();
        let device = MockDevice::default();
        let start = dispatch_with_playback(
            request(
                "original-first",
                "media.playRecording",
                json!({"recording":"recording"}),
            ),
            &owner,
            &device,
        )
        .await;
        assert!(start.error.is_none(), "{start:?}");
        let first = dispatch_with_playback(
            request(
                "read-first",
                "media.playbackState",
                json!({"requestId":"original-first"}),
            ),
            &owner,
            &device,
        )
        .await;
        assert!(first.error.is_none(), "{first:?}");
        let first = first.result.unwrap();
        let old_token = first["playbackId"].as_str().expect("native playback token");
        let next = dispatch_with_playback(
            request(
                "original-second",
                "media.playRecording",
                json!({"recording":"recording"}),
            ),
            &owner,
            &device,
        )
        .await;
        assert!(next.error.is_none(), "{next:?}");
        let second = dispatch_with_playback(
            request(
                "read-second",
                "media.playbackState",
                json!({"requestId":"original-second"}),
            ),
            &owner,
            &device,
        )
        .await;
        let second = second.result.expect("second native receipt");
        let token = second["playbackId"].as_str().expect("new native token");
        assert_ne!(token, old_token);
        for (sender, cleanup) in [(&owner, old_token), (&foreign, token)] {
            let stopped = dispatch_with_playback(
                request("cleanup", "media.stopPlayback", json!({"playback":cleanup})),
                sender,
                &device,
            )
            .await;
            assert!(stopped.error.is_none(), "{stopped:?}");
            assert_eq!(device.stopped(), vec![1]);
            assert!(!audio_idle());
        }
        let original = dispatch_with_playback(
            request(
                "read-exact",
                "media.playbackState",
                json!({"requestId":"original-first"}),
            ),
            &owner,
            &device,
        )
        .await;
        assert_eq!(original.result.unwrap()["status"], "stopped");
        let hidden = dispatch_with_playback(
            request(
                "foreign-read",
                "media.playbackState",
                json!({"requestId":"original-second"}),
            ),
            &foreign,
            &device,
        )
        .await;
        assert_eq!(hidden.result.unwrap()["status"], "unknown");
        let stopped = dispatch_with_playback(
            request(
                "owned-cleanup",
                "media.stopPlayback",
                json!({"playback":token}),
            ),
            &owner,
            &device,
        )
        .await;
        assert!(stopped.error.is_none(), "{stopped:?}");
        assert_eq!(device.stopped(), vec![1, 2]);
        assert!(audio_idle());
        drop(device);
        drop(owner);
        drop(foreign);
        worker.join().unwrap();
        foreign_worker.join().unwrap();
    });
}

#[test]
fn playback_failed_replacement_keeps_exact_failed_and_stopped_receipts() {
    let _serial = AUDIO_TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (owner, worker) = store();
        let device = MockDevice::default();
        play(&owner, &device).await;
        device
            .fail_start
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let failed = dispatch_with_playback(
            request(
                "replacement-failed",
                "media.playRecording",
                json!({"recording":"recording"}),
            ),
            &owner,
            &device,
        )
        .await;
        assert!(failed.error.is_some());
        for (request_id, status) in [("play", "stopped"), ("replacement-failed", "failed")] {
            let receipt = dispatch_with_playback(
                request(
                    "exact-read",
                    "media.playbackState",
                    json!({"requestId":request_id}),
                ),
                &owner,
                &device,
            )
            .await;
            assert!(receipt.error.is_none(), "{receipt:?}");
            assert_eq!(receipt.result.unwrap()["status"], status);
        }
        assert_eq!(device.stopped(), vec![1]);
        assert!(audio_idle());
        drop(device);
        drop(owner);
        worker.join().unwrap();
    });
}

#[test]
fn playback_stop_cancels_a_late_preparation_before_it_can_open() {
    let _serial = AUDIO_TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (owner, worker) = store();
        let device = Arc::new(MockDevice::default());
        let (entered, waiting) = oneshot::channel();
        let (resume, delayed) = oneshot::channel();
        *device.delayed_start.lock().unwrap() = Some((entered, delayed));
        let preparing = {
            let owner = owner.clone();
            let device = device.clone();
            tokio::spawn(async move {
                dispatch_with_playback(
                    request(
                        "original-preparing",
                        "media.playRecording",
                        json!({"recording":"recording"}),
                    ),
                    &owner,
                    device.as_ref(),
                )
                .await
            })
        };
        tokio::time::timeout(MOCK_DEVICE_WAIT, waiting)
            .await
            .unwrap()
            .unwrap();
        let stopped = dispatch_with_playback(
            request("stop-preparing", "media.stopPlayback", json!({})),
            &owner,
            device.as_ref(),
        )
        .await;
        assert!(stopped.error.is_none(), "{stopped:?}");
        resume.send(()).unwrap();
        let outcome = tokio::time::timeout(MOCK_DEVICE_WAIT, preparing)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            device.starts(),
            0,
            "A stopped preparation opened a late player."
        );
        assert!(outcome.error.is_some(), "{outcome:?}");
        assert!(audio_idle());
        drop(device);
        drop(owner);
        worker.join().unwrap();
    });
}

#[test]
fn playback_unknown_request_receipt_never_starts_or_stops_a_device() {
    let _serial = AUDIO_TEST_MUTEX.lock().unwrap_or_else(|p| p.into_inner());
    tokio::runtime::Runtime::new().unwrap().block_on(async {
        let (owner, worker) = store();
        let device = MockDevice::default();
        let state = dispatch_with_playback(
            request(
                "read-original",
                "media.playbackState",
                json!({"requestId":"original-unknown-start"}),
            ),
            &owner,
            &device,
        )
        .await;
        assert!(state.error.is_none(), "{state:?}");
        assert_eq!(state.result.unwrap()["status"], "unknown");
        assert_eq!(*device.starts.lock().unwrap(), 0);
        assert!(device.stopped().is_empty());
        drop(device);
        drop(owner);
        worker.join().unwrap();
    });
}
