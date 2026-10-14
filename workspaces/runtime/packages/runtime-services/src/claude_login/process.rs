//! Sign-in adapter for the shared owned streaming child.
use super::{LoginError, ports::*};
use studio_runtime_process::stream_child::{
    ChildEvent, ChildSpec, ExitReason, OutputStream, StreamChild,
};

pub struct StreamLauncher;
struct Child(StreamChild);
impl LoginLauncher for StreamLauncher {
    fn gone(&self, identity: ChildIdentity) -> FutureResult<'_, bool> {
        Box::pin(async move { Ok(super::identity::observe(identity.pid).await? != Some(identity)) })
    }
    fn resolve(
        &self,
        requested: &std::path::Path,
        environment: &std::collections::BTreeMap<std::ffi::OsString, std::ffi::OsString>,
    ) -> Result<std::path::PathBuf, LoginError> {
        crate::account_driver::claude::resolve_login_binary(requested.to_str(), environment)
            .ok_or(LoginError::Unavailable)
    }
    fn spawn(&self, command: LoginCommand) -> FutureResult<'_, Box<dyn LoginChild>> {
        Box::pin(async move {
            let child = StreamChild::spawn(ChildSpec {
                program: command.program,
                args: command.args,
                cwd: command.cwd,
                environment: command.environment,
                timeout: command.timeout,
                inherited_lease: command.inherited_lease,
            })
            .await
            .map_err(|_| LoginError::Unavailable)?;
            Ok(Box::new(Child(child)) as Box<dyn LoginChild>)
        })
    }
}
impl LoginChild for Child {
    fn identity(&self) -> FutureResult<'_, Option<ChildIdentity>> {
        Box::pin(async move {
            match self.0.pid() {
                Some(pid) => super::identity::observe(pid).await,
                None => Ok(None),
            }
        })
    }
    fn next_event(&mut self) -> FutureResult<'_, Option<LoginEvent>> {
        Box::pin(async move {
            Ok(self.0.next_event().await.map(|event| match event {
                ChildEvent::Output {
                    stream: OutputStream::Stdout,
                    bytes,
                } => LoginEvent::Output(bytes),
                ChildEvent::Output {
                    stream: OutputStream::Stderr,
                    bytes,
                } => LoginEvent::Diagnostic(bytes),
                ChildEvent::Exited { code, reason } => {
                    LoginEvent::Exited(code == Some(0) && reason == ExitReason::Completed)
                }
            }))
        })
    }
    fn write(&self, bytes: Vec<u8>) -> FutureResult<'_, ()> {
        Box::pin(async move {
            self.0
                .write(bytes, false)
                .await
                .map_err(|_| LoginError::Unavailable)
        })
    }
    fn close(self: Box<Self>) -> FutureResult<'static, ()> {
        Box::pin(async move { self.0.close().await.map_err(|_| LoginError::Unavailable) })
    }
}
