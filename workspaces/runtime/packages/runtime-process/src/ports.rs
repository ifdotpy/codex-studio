use std::path::Path;

/// User terminal port. Implementations belong in a later platform adapter.
pub trait PtyHost: Send + Sync {
    type Session: Send;
    type Error: std::error::Error + Send + Sync + 'static;
    fn open(
        &self,
        command: &[String],
        cwd: &Path,
        env: &[(String, String)],
    ) -> Result<Self::Session, Self::Error>;
    fn resize(
        &self,
        session: &mut Self::Session,
        columns: u16,
        rows: u16,
    ) -> Result<(), Self::Error>;
    fn write(&self, session: &mut Self::Session, bytes: &[u8]) -> Result<(), Self::Error>;
    fn read(&self, session: &mut Self::Session, buffer: &mut [u8]) -> Result<usize, Self::Error>;
    fn wait(&self, session: &mut Self::Session) -> Result<Option<i32>, Self::Error>;
    fn close(&self, session: Self::Session) -> Result<(), Self::Error>;
}

/// Progress-file watch port. Callbacks are delivered on the implementation's
/// worker thread and must not block event delivery.
pub trait ProgressWatch: Send + Sync {
    type Watch: Send;
    type Error: std::error::Error + Send + Sync + 'static;
    fn watch(
        &self,
        path: &Path,
        changed: Box<dyn Fn() + Send + Sync>,
    ) -> Result<Self::Watch, Self::Error>;
    fn unwatch(&self, watch: Self::Watch) -> Result<(), Self::Error>;
}
