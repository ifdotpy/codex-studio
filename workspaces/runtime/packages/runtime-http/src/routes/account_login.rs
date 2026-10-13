//! Persisted account registry operations that do not start provider sessions.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, get, post};
use axum::Json;
use axum::extract::State;
use axum::http::StatusCode;
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_services::account_driver::{
    AccountDriver, AccountDriverError, SystemProviderAuthReader,
};

use crate::error::ApiError;
use crate::router::HttpState;

const CLAUDE_DISCOVERY: [Dependency; 1] = [Dependency::new("provider-session/claude-discovery")];
const CODEX_LOGIN: [Dependency; 1] = [Dependency::new("provider-session/codex-login")];
const CLAUDE_LOGIN: [Dependency; 1] = [Dependency::new("provider-session/claude-login")];
const CLAUDE_PROFILE: [Dependency; 1] = [Dependency::new("provider-session/claude-profile-auth")];

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/accounts/claude/add",
            OperationStatus::partial(&CLAUDE_LOGIN),
            claude_add,
        ),
        get(
            "/api/accounts/claude/login",
            OperationStatus::partial(&CLAUDE_LOGIN),
            claude_login_status,
        ),
        post(
            "/api/accounts/claude/login",
            OperationStatus::partial(&CLAUDE_LOGIN),
            claude_login,
        ),
        post(
            "/api/accounts/claude/login/cancel",
            OperationStatus::partial(&CLAUDE_LOGIN),
            claude_login_cancel,
        ),
        post(
            "/api/accounts/claude/login/code",
            OperationStatus::partial(&CLAUDE_LOGIN),
            claude_login_code,
        ),
        post(
            "/api/accounts/default",
            OperationStatus::Implemented,
            set_default,
        ),
        post("/api/accounts/delete", OperationStatus::Implemented, delete),
        post(
            "/api/accounts/disconnect",
            OperationStatus::Implemented,
            disconnect,
        ),
        post(
            "/api/accounts/discover",
            OperationStatus::partial(&CLAUDE_DISCOVERY),
            discover,
        ),
        post(
            "/api/accounts/login",
            OperationStatus::partial(&CODEX_LOGIN),
            codex_login,
        ),
        post(
            "/api/accounts/login/cancel",
            OperationStatus::partial(&CODEX_LOGIN),
            codex_login_cancel,
        ),
        post("/api/accounts/name", OperationStatus::Implemented, set_name),
        post(
            "/api/accounts/reconnect",
            OperationStatus::Implemented,
            reconnect,
        ),
        post(
            "/api/accounts/register",
            OperationStatus::Implemented,
            register,
        ),
        post(
            "/api/claude/profiles",
            OperationStatus::partial(&CLAUDE_PROFILE),
            claude_profile,
        ),
    ])
}

#[derive(Deserialize)]
struct AccountKeyBody {
    account_key: Option<String>,
}

#[derive(Deserialize)]
struct RequiredAccountKeyBody {
    account_key: String,
}

#[derive(Deserialize)]
struct AccountNameBody {
    account_key: String,
    label: String,
    request_id: String,
}

#[derive(Deserialize)]
struct DeleteAccountBody {
    account_key: String,
    request_id: String,
}

#[derive(Deserialize)]
struct RegisterAccountBody {
    home: String,
}

#[derive(Deserialize)]
struct CancelLoginBody {
    login_id: String,
}

async fn set_default(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<AccountKeyBody>,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    run_mutation(state, move |driver| {
        driver.set_default(body.account_key.as_deref())
    })
    .await
}

async fn set_name(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<AccountNameBody>,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    run_mutation(state, move |driver| {
        driver.set_name(
            &body.account_key,
            &body.label,
            &body.request_id.to_ascii_lowercase(),
        )
    })
    .await
}

async fn delete(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<DeleteAccountBody>,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    run_mutation(state, move |driver| {
        driver.delete(&body.account_key, &body.request_id.to_ascii_lowercase())
    })
    .await
}

async fn disconnect(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<RequiredAccountKeyBody>,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    run_mutation(state, move |driver| driver.disconnect(&body.account_key)).await
}

async fn reconnect(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<RequiredAccountKeyBody>,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    run_mutation(state, move |driver| driver.reconnect(&body.account_key)).await
}

async fn register(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<RegisterAccountBody>,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    run_mutation(state, move |driver| driver.register(&body.home)).await
}

async fn run_mutation(
    state: HttpState,
    mutation: impl FnOnce(
        &AccountDriver,
    ) -> Result<
        studio_runtime_services::account_policy::AccountsSnapshot,
        AccountDriverError,
    > + Send
    + 'static,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    let Some(driver) = state.account_driver else {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires account-driver/unavailable",
        ));
    };
    let wake = state.event_wake.clone();
    let snapshot = tokio::task::spawn_blocking(move || mutation(&driver))
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(account_error)?;
    wake.notify_one();
    accounts_response(snapshot)
}

fn accounts_response(
    snapshot: studio_runtime_services::account_policy::AccountsSnapshot,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    let value = json!({
        "accounts": snapshot.accounts,
        "archivedAccounts": snapshot.archived_accounts,
        "defaultAccountKey": snapshot.default_account_key,
        "logins": snapshot.logins,
        "supportsDisconnect": true,
        "supportsDelete": true,
    });
    serde_json::from_value(value)
        .map(Json)
        .map_err(|_| ApiError::internal())
}

fn account_error(error: AccountDriverError) -> ApiError {
    match error {
        AccountDriverError::UnknownAccount => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown account")
        }
        AccountDriverError::SignInRequired => {
            ApiError::new(StatusCode::BAD_REQUEST, "Sign in to this account first")
        }
        AccountDriverError::DeletedAccount => {
            ApiError::new(StatusCode::BAD_REQUEST, "This account was deleted")
        }
        AccountDriverError::NameRequestConflict => ApiError::new(
            StatusCode::BAD_REQUEST,
            "This name request id has different content",
        ),
        AccountDriverError::DeleteRequestConflict => ApiError::new(
            StatusCode::BAD_REQUEST,
            "This delete request id has different content",
        ),
        AccountDriverError::NoDisconnectReplacement => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Connect another account before disconnecting the application default.",
        ),
        AccountDriverError::NoDeleteReplacement => ApiError::new(
            StatusCode::BAD_REQUEST,
            "Add another connected account before deleting the application default.",
        ),
        AccountDriverError::InvalidCodexProfile => ApiError::new(
            StatusCode::BAD_REQUEST,
            "This directory has no readable Codex auth.json. Use Sign in for a new account",
        ),
        AccountDriverError::LoginResponseUnknown => ApiError::new(
            StatusCode::BAD_REQUEST,
            "The sign-in response is still unknown. Check its status first.",
        ),
        AccountDriverError::UnknownLogin => {
            ApiError::new(StatusCode::BAD_REQUEST, "Unknown sign-in request")
        }
        AccountDriverError::ClaudeDiscoveryUnavailable => {
            unavailable("POST", "/api/accounts/discover", &CLAUDE_DISCOVERY)
        }
        AccountDriverError::LoginCancellationUnavailable => {
            unavailable("POST", "/api/accounts/login/cancel", &CODEX_LOGIN)
        }
        AccountDriverError::ReadRegistry
        | AccountDriverError::InvalidRegistry
        | AccountDriverError::WriteRegistry
        | AccountDriverError::DurabilityUnconfirmed
        | AccountDriverError::Poisoned => ApiError::internal(),
    }
}

async fn discover(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    let Some(driver) = state.account_driver else {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires account-driver/unavailable",
        ));
    };
    let snapshot = tokio::task::spawn_blocking(move || driver.discover_codex())
        .await
        .map_err(|_| ApiError::internal())?
        .map_err(account_error)?;
    state.event_wake.notify_one();
    if AccountDriver::<SystemProviderAuthReader>::claude_discovery_available() {
        return Err(unavailable(
            "POST",
            "/api/accounts/discover",
            &CLAUDE_DISCOVERY,
        ));
    }
    accounts_response(snapshot)
}

async fn codex_login() -> Result<Json<Value>, ApiError> {
    Err(unavailable("POST", "/api/accounts/login", &CODEX_LOGIN))
}

async fn codex_login_cancel(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<CancelLoginBody>,
) -> Result<Json<crate::generated::AccountLoginResponse>, ApiError> {
    let Some(driver) = state.account_driver else {
        return Err(ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires account-driver/unavailable",
        ));
    };
    let receipt =
        tokio::task::spawn_blocking(move || driver.terminal_login_receipt(&body.login_id))
            .await
            .map_err(|_| ApiError::internal())?
            .map_err(account_error)?;
    serde_json::from_value(receipt)
        .map(Json)
        .map_err(|_| ApiError::internal())
}

async fn claude_login_status() -> Result<Json<Value>, ApiError> {
    Err(unavailable(
        "GET",
        "/api/accounts/claude/login",
        &CLAUDE_LOGIN,
    ))
}

async fn claude_login() -> Result<Json<Value>, ApiError> {
    Err(unavailable(
        "POST",
        "/api/accounts/claude/login",
        &CLAUDE_LOGIN,
    ))
}

async fn claude_add() -> Result<Json<Value>, ApiError> {
    Err(unavailable(
        "POST",
        "/api/accounts/claude/add",
        &CLAUDE_LOGIN,
    ))
}

async fn claude_login_cancel() -> Result<Json<Value>, ApiError> {
    Err(unavailable(
        "POST",
        "/api/accounts/claude/login/cancel",
        &CLAUDE_LOGIN,
    ))
}

async fn claude_login_code() -> Result<Json<Value>, ApiError> {
    Err(unavailable(
        "POST",
        "/api/accounts/claude/login/code",
        &CLAUDE_LOGIN,
    ))
}

async fn claude_profile() -> Result<Json<Value>, ApiError> {
    Err(unavailable("POST", "/api/claude/profiles", &CLAUDE_PROFILE))
}

fn unavailable(method: &str, path: &str, dependencies: &[Dependency]) -> ApiError {
    dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal())
}

#[cfg(test)]
mod tests {
    use super::run_mutation;
    use crate::router::HttpState;
    use axum::body::to_bytes;
    use axum::http::StatusCode;
    use axum::response::IntoResponse;

    #[tokio::test]
    async fn missing_account_driver_has_named_501() {
        let state = HttpState::new(
            String::new(),
            String::new(),
            None,
            String::new(),
            0,
            None,
            None,
            None,
        );
        let error = run_mutation(state, |_| unreachable!())
            .await
            .expect_err("missing driver is a partial operation");
        let response = error.into_response();
        assert_eq!(response.status(), StatusCode::NOT_IMPLEMENTED);
        let body = to_bytes(response.into_body(), 4096)
            .await
            .expect("error body");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).expect("JSON error")["error"],
            "Not implemented: requires account-driver/unavailable"
        );
    }
}
