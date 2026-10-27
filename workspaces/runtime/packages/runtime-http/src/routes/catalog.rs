//! Read-only account and persisted usage snapshots.

use axum::Json;
use axum::extract::State;
use studio_runtime_services::account_session_runtime::LimitsSnapshot;

use crate::error::ApiError;
use crate::router::HttpState;
use crate::routes::extractors::QueryParams;
use crate::routes::manifest::{OperationStatus, RouteGroup, get as register_get};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        register_get("/api/accounts", OperationStatus::Implemented, accounts),
        register_get("/api/limits", OperationStatus::Implemented, limits),
    ])
}

pub(super) async fn accounts(
    State(state): State<HttpState>,
) -> Result<Json<crate::generated::AccountsResponse>, ApiError> {
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let account_policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let account_driver = state.account_driver.clone();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| ApiError::internal())?
        .as_secs_f64();
    let waiting = store
        .read_async("runtime-http/routes/catalog", move |tx| {
            studio_runtime_store::account_state::authentication_recovery_account_keys(tx, now)
        })
        .await
        .map_err(|_| ApiError::internal())?;
    let snapshot = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/catalog",
        move || {
            let mut snapshot = match account_driver {
                Some(driver) => driver.listing_snapshot().map_err(|_| ())?,
                None => account_policy.accounts(),
            };
            studio_runtime_services::account_policy::apply_authentication_recovery(
                &mut snapshot,
                &waiting,
            );
            Ok::<_, ()>(snapshot)
        },
    )
    .await
    .ok()
    .and_then(Result::ok);
    let snapshot = snapshot.ok_or_else(ApiError::internal)?;
    super::accounts_response(snapshot)
}

async fn limits(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<crate::generated::UsageLimitsResponse>, ApiError> {
    let account_key = params
        .typed_value("account_key")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.is_empty())
        .unwrap_or("default");
    let agent_id = params.nonempty_str("agent").map(str::to_owned);
    let cached = params
        .typed_value("cached")
        .and_then(serde_json::Value::as_str)
        == Some("1");
    if let Some(port) = state.account_session.clone() {
        let response = studio_runtime_services::account_session_runtime::limits(
            port.as_ref(),
            account_key,
            cached,
        )
        .await;
        let response = match response {
            Ok(response) => response,
            Err(
                studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                    "Unknown Codex account",
                ),
            ) => {
                return Err(super::account_login::session_error(
                    studio_runtime_services::account_session_runtime::AccountSessionError::Refused(
                        "Unknown Codex account",
                    ),
                ));
            }
            Err(error) => limit_error_snapshot(&state, account_key, &error.to_string())?,
        };
        return limits_response(&state, response, agent_id).await;
    }
    let account_policy = state
        .account_policy
        .as_ref()
        .ok_or_else(ApiError::internal)?;
    let snapshot = account_policy.limits(account_key);
    if snapshot.get("data").is_some_and(|data| !data.is_null())
        || (cached
            && snapshot
                .get("error")
                .is_some_and(|error| !error.is_null() && error != ""))
    {
        return limits_response(
            &state,
            LimitsSnapshot::read(account_key, snapshot).map_err(|_| ApiError::internal())?,
            agent_id,
        )
        .await;
    }
    let snapshot = limit_error_snapshot(&state, account_key, "Account limits could not be read.")?;
    limits_response(&state, snapshot, agent_id).await
}

async fn limits_response(
    state: &HttpState,
    snapshot: LimitsSnapshot,
    agent_id: Option<String>,
) -> Result<Json<crate::generated::UsageLimitsResponse>, ApiError> {
    let mut snapshot = snapshot.into_value();
    if let Some(id) = agent_id {
        let store = state.store.clone().ok_or_else(ApiError::internal)?;
        let agent = store
            .read_async("runtime-http/limits-eligibility", move |connection| {
                studio_runtime_store::AgentRepository::get(connection, &id.as_str().into())
            })
            .await
            .map_err(|_| ApiError::internal())?
            .ok_or_else(|| ApiError::new(axum::http::StatusCode::NOT_FOUND, "Unknown agent"))?;
        let agent = agent.into_value();
        let account = agent
            .get("accountKey")
            .and_then(serde_json::Value::as_str)
            .filter(|key| !key.is_empty())
            .unwrap_or("default");
        if snapshot["accountKey"].as_str() != Some(account) {
            return Err(ApiError::new(
                axum::http::StatusCode::BAD_REQUEST,
                "Limits belong to another account",
            ));
        }
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_err(|_| ApiError::internal())?
            .as_secs_f64();
        snapshot["eligibility"] =
            studio_runtime_services::usage_resume::eligibility(&snapshot, &agent, now);
    }
    serde_json::from_value(snapshot)
        .map(Json)
        .map_err(|_| ApiError::internal())
}

fn limit_error_snapshot(
    state: &HttpState,
    account_key: &str,
    message: &str,
) -> Result<LimitsSnapshot, ApiError> {
    let account_policy = state
        .account_policy
        .as_ref()
        .ok_or_else(ApiError::internal)?;
    let mut snapshot = account_policy.limits(account_key);
    let object = snapshot.as_object_mut().ok_or_else(ApiError::internal)?;
    object.insert(
        "error".to_owned(),
        serde_json::Value::String(message.to_owned()),
    );
    object.insert(
        "checkedAt".to_owned(),
        serde_json::Value::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| ApiError::internal())?
                .as_secs_f64(),
        ),
    );
    LimitsSnapshot::read(account_key, snapshot).map_err(|_| ApiError::internal())
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "catalog",
        routes: Some(routes),
        dependencies: &[],
    }
}

#[cfg(test)]
mod tests {
    use axum::{
        body::{Body, to_bytes},
        extract::ConnectInfo,
        http::Request,
    };
    use serde_json::{Value, json};
    use std::sync::Arc;
    use studio_runtime_services::{
        account_driver::AccountDriver, account_policy::AccountPolicyState,
    };
    use studio_runtime_store::RuntimeStore;
    use tower::ServiceExt;

    #[tokio::test]
    async fn limits_route_preserves_checked_fields_and_vendor_extensions() {
        let root = tempfile::tempdir().expect("isolated state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let data = json!({
            "accountId":"native-account", "ordinaryUsageAllowed":false,
            "rateLimits":{"planType":"plus","spendControlReached":true,"secondary":null,"primary":{
                "usedPercent":42,"resetsAt":null,"vendorWindow":{"opaque":[null,true]}
            }},
            "rateLimitResetCredits":{"credits":[{
                "id":"credit","status":"available","resetType":"five_hour",
                "vendorCredit":{"opaque":null}
            }]},
            "vendorData":{"opaque":null}
        });
        let registry = serde_json::to_vec(&json!({
            "version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,
            "defaultAccountKey":"default",
            "accounts":{"default":{
                "id":"default","provider":"codex","home":root.path(),
                "label":"Default","source":"Fixture","status":"signedOut"
            }},"logins":{}
        }))
        .expect("registry JSON");
        let policy = AccountPolicyState::from_startup(
            Some(&registry),
            [(
                "default".into(),
                json!({"accountKey":"default","at":1000,"data":data}),
            )],
        )
        .expect("checked startup limits");
        assert!(policy.limits_diagnostics().is_empty());
        let state = crate::HttpState::new(
            "fixture-token".into(),
            "fixture".into(),
            root.path().display().to_string(),
            49171,
            None,
            Some(store),
            Some(Arc::new(policy)),
        );
        let request = Request::builder()
            .uri("/api/limits?cached=1")
            .header("host", "127.0.0.1:49171")
            .header("x-canvas-token", "fixture-token")
            .header(crate::API_SCHEMA_HASH_HEADER, crate::api_schema_hash())
            .extension(ConnectInfo(
                "127.0.0.1:12345"
                    .parse::<std::net::SocketAddr>()
                    .expect("address"),
            ))
            .body(Body::empty())
            .expect("request");
        let response = crate::router(state)
            .expect("router")
            .oneshot(request)
            .await
            .expect("response");
        assert_eq!(response.status(), 200);
        let body = to_bytes(response.into_body(), 1024 * 1024)
            .await
            .expect("body");
        let body: Value = serde_json::from_slice(&body).expect("JSON");
        assert_eq!(body["accountKey"], "default");
        assert_eq!(body["data"], data);
        assert!(body.get("eligibility").is_none());
    }

    #[tokio::test]
    #[allow(clippy::disallowed_methods)] // Test fixture writes and rereads the registry file.
    async fn accounts_route_exposes_explicit_codex_and_claude_providers() {
        for with_driver in [false, true] {
            let root = tempfile::tempdir().expect("isolated state");
            let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
            let row = json!({"id":"default","provider":studio_runtime_schema::ProviderKind::Codex,"home":root.path(),"label":"Default","source":"Fixture","status":"signedOut"});
            let mut claude = row.clone();
            claude["id"] = json!("claude");
            claude["provider"] = json!("claude");
            claude["deleted"] = json!(true);
            let registry = json!({"version":studio_runtime_schema::shared_values::ACCOUNT_REGISTRY_FORMAT_VERSION,"defaultAccountKey":"default","accounts":{"default":row,"claude":claude},"logins":{}});
            let bytes = serde_json::to_vec(&registry).expect("registry JSON");
            let path = root.path().join("registry.json");
            std::fs::write(&path, &bytes).expect("registry");
            let policy = AccountPolicyState::from_startup(Some(&bytes), []).expect("policy");
            let mut state = crate::HttpState::new(
                "fixture-token".into(),
                "fixture".into(),
                root.path().display().to_string(),
                49171,
                None,
                Some(store),
                Some(Arc::new(policy)),
            );
            if with_driver {
                state = state
                    .with_account_driver(Arc::new(AccountDriver::open(&path).expect("driver")));
            }
            let request = Request::builder()
                .uri("/api/accounts")
                .header("host", "127.0.0.1:49171")
                .header("x-canvas-token", "fixture-token")
                .header(crate::API_SCHEMA_HASH_HEADER, crate::api_schema_hash())
                .extension(ConnectInfo(
                    "127.0.0.1:12345"
                        .parse::<std::net::SocketAddr>()
                        .expect("address"),
                ))
                .body(Body::empty())
                .expect("request");
            let response = crate::router(state)
                .expect("router")
                .oneshot(request)
                .await
                .expect("response");
            assert_eq!(response.status(), 200);
            let body = to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("body");
            let body: Value = serde_json::from_slice(&body).expect("JSON");
            assert_eq!(body["accounts"][0]["provider"], "codex");
            assert_eq!(body["archivedAccounts"][0]["provider"], "claude");
            assert_eq!(std::fs::read(&path).expect("registry unchanged"), bytes);
        }
    }
}
