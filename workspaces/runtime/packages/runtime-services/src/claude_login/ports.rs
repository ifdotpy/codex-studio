//! Ports shared by sign-in's actor, scripted tests and the owned stream child.
use super::{LoginError, LoginSnapshot};
use crate::account_driver::{AccountDriver, AuthMetadata, ProviderAuthReader};
use serde_json::{Map, Value};
use std::{
    collections::BTreeMap,
    ffi::OsString,
    future::Future,
    path::{Path, PathBuf},
    pin::Pin,
    time::Duration,
};

pub type FutureResult<'a, T> = Pin<Box<dyn Future<Output = Result<T, LoginError>> + Send + 'a>>;
pub struct LoginCommand {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub cwd: PathBuf,
    pub environment: BTreeMap<OsString, OsString>,
    pub timeout: Duration,
    pub inherited_lease: Option<std::fs::File>,
}
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ChildIdentity {
    pub pid: u32,
    pub start: String,
}
pub enum LoginEvent {
    Output(Vec<u8>),
    Diagnostic(Vec<u8>),
    Exited(bool),
}
pub trait LoginChild: Send {
    fn identity(&self) -> FutureResult<'_, Option<ChildIdentity>> {
        Box::pin(async { Ok(None) })
    }
    fn next_event(&mut self) -> FutureResult<'_, Option<LoginEvent>>;
    fn write(&self, bytes: Vec<u8>) -> FutureResult<'_, ()>;
    fn close(self: Box<Self>) -> FutureResult<'static, ()>;
}
pub trait LoginLauncher: Send + Sync {
    fn gone(&self, _identity: ChildIdentity) -> FutureResult<'_, bool> {
        Box::pin(async { Ok(false) })
    }
    fn resolve(
        &self,
        requested: &Path,
        _environment: &BTreeMap<OsString, OsString>,
    ) -> Result<PathBuf, LoginError> {
        Ok(requested.into())
    }
    fn spawn(&self, command: LoginCommand) -> FutureResult<'_, Box<dyn LoginChild>>;
}
pub trait LoginAccounts: Send + Sync {
    fn profile(&self, key: &str) -> Result<Value, LoginError>;
    fn find_config(&self, config: &str) -> Result<Option<Value>, LoginError>;
    fn revalidate(&self, key: &str) -> Result<Value, LoginError>;
    fn register(
        &self,
        options: Map<String, Value>,
        label: &str,
        home: &Path,
        metadata: &AuthMetadata,
    ) -> Result<Value, LoginError>;
    fn refresh(
        &self,
        key: &str,
        expected: &Value,
        metadata: &AuthMetadata,
    ) -> Result<Value, LoginError>;
}
impl<R: ProviderAuthReader> LoginAccounts for AccountDriver<R> {
    fn profile(&self, key: &str) -> Result<Value, LoginError> {
        self.claude_profile(key)
            .map_err(|_| LoginError::Unavailable)
    }
    fn revalidate(&self, key: &str) -> Result<Value, LoginError> {
        self.revalidate_claude(key)
            .map_err(|_| LoginError::Unavailable)
    }
    fn find_config(&self, config: &str) -> Result<Option<Value>, LoginError> {
        self.find_claude_config(config)
            .map_err(|_| LoginError::Unavailable)
    }
    fn register(
        &self,
        options: Map<String, Value>,
        label: &str,
        home: &Path,
        metadata: &AuthMetadata,
    ) -> Result<Value, LoginError> {
        let key = self
            .register_claude_verified(options, label, home, metadata)
            .map_err(|_| LoginError::Unavailable)?;
        self.profile(&key)
    }
    fn refresh(
        &self,
        key: &str,
        expected: &Value,
        metadata: &AuthMetadata,
    ) -> Result<Value, LoginError> {
        self.refresh_claude_verified(key, expected, metadata, true)
            .map_err(|_| LoginError::Unavailable)
    }
}

pub trait LoginReceipts: Send + Sync {
    fn load(&self) -> Result<Vec<LoginSnapshot>, LoginError>;
    fn save(&self, record: &LoginSnapshot) -> Result<(), LoginError>;
}

/// Idempotent store repair after identity-verified reauthentication.
pub trait LoginChats: Send + Sync {
    fn refresh(&self, account: &str) -> Result<(), LoginError>;
}
