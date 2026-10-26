
// SCRATCH EXPERIMENT (not committed): hold the canvas write lock while the fire job writes
// its last record, so the stopped timer cancels that submitted writer job.
struct BlockingHost {
    inner: HostPort,
    blocker: Arc<Mutex<Option<rusqlite::Connection>>>,
    admitted: Arc<std::sync::atomic::AtomicU64>,
}
impl Host for BlockingHost {
    fn compact<'a>(&'a self, agent: &'a str, episode: Episode, limits: Value) -> idle::HostFuture<'a> {
        Box::pin(async move {
            let result = self.inner.compact(agent, episode, limits).await;
            let blocker = rusqlite::Connection::open(self.inner.store.state_dir().join("canvas.sqlite3")).unwrap();
            blocker.execute_batch("BEGIN EXCLUSIVE").unwrap();
            *self.blocker.lock().unwrap() = Some(blocker);
            self.admitted.store(self.inner.store.writer_admissions(), std::sync::atomic::Ordering::SeqCst);
            result
        })
    }
}
async fn scratch_run(fixed: bool) {
    let root = tempfile::tempdir().unwrap();
    let (store, _episode) = fixture(root.path(), Mode::Compact).await;
    let session = Arc::new(Session::default());
    let blocker = Arc::new(Mutex::new(None));
    let admitted = Arc::new(std::sync::atomic::AtomicU64::new(u64::MAX));
    let (stop, stopped) = tokio::sync::watch::channel(false);
    let timer = tokio::spawn(idle::run(
        store.clone(),
        session.clone(),
        Arc::new(BlockingHost { inner: HostPort { store: store.clone(), session: session.clone() }, blocker: blocker.clone(), admitted: admitted.clone() }),
        stopped,
    ));
    tokio::time::timeout(HANG_BOUND, session.finished.notified()).await.unwrap();
    // The fire job submits its last record after compact returns; wait for that admission.
    while admitted.load(std::sync::atomic::Ordering::SeqCst) == u64::MAX
        || store.writer_admissions() <= admitted.load(std::sync::atomic::Ordering::SeqCst)
    {
        tokio::task::yield_now().await;
    }
    stop.send(true).unwrap();
    timer.await.unwrap();
    drop(store);
    if fixed {
        let release = blocker.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(300)).await;
            drop(release.lock().unwrap().take());
        });
        let store = reopen(root.path()).await;
        assert!(idle::next(&store).await.unwrap().is_none());
    } else {
        // The original test reopened at once here.
        let reopened = RuntimeStore::open(Some(root.path())).await;
        drop(blocker.lock().unwrap().take());
        reopened.unwrap();
    }
}
#[tokio::test]
async fn scratch_original_reopen_with_slow_writer() {
    scratch_run(false).await;
}
#[tokio::test]
async fn scratch_fixed_reopen_with_slow_writer() {
    scratch_run(true).await;
}
