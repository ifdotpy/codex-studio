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
    let directory = tempfile::tempdir().unwrap();
    let store = Arc::new(RuntimeStore::open(Some(directory.path())).await.unwrap());
    let agent = json!({"id":"worker","rootId":"worker","epoch":1,"status":"idle",
        "accountKey":"claude","provider":"claude","model":"sonnet","cwd":"/fixture","isLead":true});
    let stored = AgentRecord::from_value(agent.clone()).unwrap();
    store
        .transaction(move |tx| AgentRepository::put(tx, &stored))
        .await
        .unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
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
        Some(&serde_json::to_vec(&registry).unwrap()),
        [("claude".into(), limits.clone())],
    )
    .unwrap();
    let state = HttpState::new(
        "token".into(),
        "fixture".into(),
        directory.path().display().to_string(),
        49171,
        None,
        Some(store),
        Some(Arc::new(policy)),
    );
    let app = router(state).unwrap();
    let get = |path: &'static str| {
        Request::builder()
            .uri(path)
            .header("host", "127.0.0.1:49171")
            .header("x-canvas-token", "token")
            .extension(axum::extract::ConnectInfo(
                "127.0.0.1:12345".parse::<std::net::SocketAddr>().unwrap(),
            ))
            .body(Body::empty())
            .unwrap()
    };
    let response = app
        .clone()
        .oneshot(get("/api/limits?account_key=claude&cached=1&agent=worker"))
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
    let expected = usage_resume::eligibility(
        &limits,
        &agent,
        body["eligibility"]["checkedAt"].as_f64().unwrap(),
    );
    assert_eq!(body["eligibility"], expected);
    assert_eq!(body["eligibility"]["canRun"], true);
    let response = app.oneshot(get("/api/accounts")).await.unwrap();
    assert_eq!(response.status(), 200);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), usize::MAX).await.unwrap()).unwrap();
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
