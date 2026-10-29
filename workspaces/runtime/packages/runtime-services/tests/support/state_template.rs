/// Closed SQLite seed files retained in memory; a static cache must never own a TempDir.
#[derive(Clone)]
#[allow(dead_code)]
pub(crate) struct StateTemplate {
    files: [Vec<u8>; 2],
}

#[allow(dead_code)]
impl StateTemplate {
    const DATABASES: [&'static str; 2] = ["canvas.sqlite3", "analytics.sqlite3"];

    pub(crate) async fn migrate(root: &std::path::Path) -> Self {
        let store = studio_runtime_store::RuntimeStore::open(Some(root))
            .await
            .expect("migrate fixture");
        store
            .flush_wal_for_shutdown()
            .await
            .expect("checkpoint fixture");
        drop(store);
        Self::capture(root)
    }

    // Synchronous test seed preparation; this helper never runs in an HTTP handler.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn capture(root: &std::path::Path) -> Self {
        Self {
            files: Self::DATABASES
                .map(|name| std::fs::read(root.join(name)).expect("closed fixture database bytes")),
        }
    }

    // Synchronous test seed preparation; this helper never runs in an HTTP handler.
    #[allow(clippy::disallowed_methods)]
    pub(crate) fn write(&self, root: &std::path::Path) {
        for (name, bytes) in Self::DATABASES.into_iter().zip(&self.files) {
            std::fs::write(root.join(name), bytes).expect("clone cached fixture database");
        }
    }
}
