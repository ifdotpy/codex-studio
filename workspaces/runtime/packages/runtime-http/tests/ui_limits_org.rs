use axum::{
    body::{Body, to_bytes},
    http::Request,
};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_http::{HttpState, router};
use studio_runtime_services::{account_policy::AccountPolicyState, usage_resume};
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore};
use tower::ServiceExt;

#[tokio::test]
async fn limits_exposes_agent_policy_and_accounts_exposes_only_safe_organization() {
    let directory = tempfile::tempdir().expect("isolated directory creates");
    let store = Arc::new(
        RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store opens"),
    );
    let agent = json!({"id":"worker","rootId":"worker","epoch":1,"status":"idle",
        "accountKey":"claude","provider":"claude","model":"sonnet","cwd":"/fixture","isLead":true});
    let stored = AgentRecord::from_value(agent.clone()).expect("agent fixture is valid");
    store
        .transaction(move |tx| AgentRepository::put(tx, &stored))
        .await
        .expect("agent fixture persists");
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock is after the Unix epoch")
        .as_secs_f64();
    let limits = json!({"accountKey":"claude","at":now,"data":{
        "rateLimits":{"limitId":"claude","primary":{"usedPercent":10,"resetsAt":now+3600.0}},
        "rateLimitsByLimitId":{"claude-opus":{"secondary":{"usedPercent":100,"resetsAt":now+7200.0}}}
    }});
    let identity = format!(
        "claude:v2:{}",
        json!([
            "fixture@example.test",
            "private-org-12345678",
            "private-account-uuid"
        ])
    );
    let registry = json!({"version":1,"defaultAccountKey":"claude","logins":{},"accounts":{
        "claude":{"id":"claude","provider":"claude","home":"/fixture","label":"Claude","source":"Fixture","status":"ready",
            "_credentialIdentity":identity,"_organizationName":"Fixture organization"}
    }});
    let policy = AccountPolicyState::from_startup(
        Some(&serde_json::to_vec(&registry).expect("registry fixture serializes")),
        [("claude".into(), limits.clone())],
    )
    .expect("account policy fixture is valid");
    let state = HttpState::new(
        "token".into(),
        "fixture".into(),
        directory.path().display().to_string(),
        49171,
        None,
        Some(store),
        Some(Arc::new(policy)),
    );
    let app = router(state).expect("HTTP router creates");
    let get = |path: &'static str| {
        Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:49171")
            .header("x-canvas-token", "token")
            .extension(axum::extract::ConnectInfo(
                "127.0.0.1:12345"
                    .parse::<std::net::SocketAddr>()
                    .expect("peer address parses"),
            ))
            .body(Body::empty())
            .expect("HTTP request builds")
    };
    let response = app
        .clone()
        .oneshot(get("/api/limits?account_key=claude&cached=1&agent=worker"))
        .await
        .expect("limits request completes");
    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("limits body reads"),
    )
    .expect("limits response parses");
    let expected = usage_resume::eligibility(
        &limits,
        &agent,
        body["eligibility"]["checkedAt"]
            .as_f64()
            .expect("eligibility check time is numeric"),
    );
    assert_eq!(body["eligibility"], expected);
    assert_eq!(body["eligibility"]["canRun"], true);
    for (path, status) in [
        ("/api/limits?account_key=claude&cached=1&agent=unknown", 404),
        ("/api/limits?account_key=default&cached=1&agent=worker", 400),
    ] {
        let response = app
            .clone()
            .oneshot(get(path))
            .await
            .expect("invalid identity request completes");
        assert_eq!(response.status(), status, "{path}");
    }
    let response = app
        .clone()
        .oneshot(get("/api/limits?account_key=claude&cached=1"))
        .await
        .expect("shared limits request completes");
    let shared: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("shared limits body reads"),
    )
    .expect("shared limits response parses");
    assert!(shared.get("eligibility").is_none());
    let response = app
        .oneshot(get("/api/accounts"))
        .await
        .expect("accounts request completes");
    assert_eq!(response.status(), 200);
    let body: Value = serde_json::from_slice(
        &to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("accounts body reads"),
    )
    .expect("accounts response parses");
    assert_eq!(
        body["accounts"][0]["organization"],
        json!({"name":"Fixture organization","idSuffix":"12345678"})
    );
    let text = body.to_string();
    for private in [
        "_credentialIdentity",
        "_organizationName",
        "private-org-",
        "private-account-uuid",
    ] {
        assert!(!text.contains(private));
    }
}
