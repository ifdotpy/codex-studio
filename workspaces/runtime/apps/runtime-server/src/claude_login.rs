//! Retained sign-in composition; native children are started only by login requests.
use std::{path::PathBuf, sync::Arc};
use studio_runtime_http::HttpState;
use studio_runtime_services::{
    account_driver::AccountDriver,
    claude_login::{
        LoginError,
        chats::StoredChats,
        process::StreamLauncher,
        receipts::FileReceipts,
        service::{LoginConfig, LoginService},
    },
};
use studio_runtime_store::RuntimeStore;
pub async fn attach(
    state: &mut HttpState,
    root: PathBuf,
    store: Arc<RuntimeStore>,
    accounts: Arc<AccountDriver>,
) -> Result<(), LoginError> {
    let directory = root.join("claude-logins");
    let receipts = Arc::new(FileReceipts::new(directory.clone())?);
    let environment = std::env::vars_os().collect();
    let program = studio_runtime_services::account_driver::claude::login_binary(&environment)
        .unwrap_or_else(|| PathBuf::from("claude"));
    let on_change = state.claude_login_change_callback();
    state.claude_login = Some(
        LoginService::open(
            LoginConfig {
                directory,
                program,
                cwd: root,
                environment,
            },
            accounts,
            Arc::new(StreamLauncher),
            receipts,
            Arc::new(StoredChats(store)),
            on_change,
        )
        .await?,
    );
    Ok(())
}
