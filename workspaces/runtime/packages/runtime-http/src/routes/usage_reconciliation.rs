//! Explicit, read-only usage evidence comparison for one current or archived agent.

use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;

use super::extractors::QueryParams;
use super::manifest::{OperationStatus, RouteGroup, get};
use crate::{error::ApiError, router::HttpState};

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![get(
        "/api/diagnostics/usage-reconciliation",
        OperationStatus::Implemented,
        reconcile,
    )])
}

async fn reconcile(
    State(state): State<HttpState>,
    params: QueryParams,
) -> Result<Json<crate::generated::UsageReconciliationResponse>, ApiError> {
    let agent = params
        .typed_value("agent")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "Agent is required"))?
        .to_owned();
    let store = state.store.clone().ok_or_else(ApiError::internal)?;
    let policy = state
        .account_policy
        .clone()
        .ok_or_else(ApiError::internal)?;
    let accounts = policy.accounts();
    let profiles = accounts
        .accounts
        .into_iter()
        .chain(accounts.archived_accounts)
        .filter_map(|account| Some((account.get("id")?.as_str()?.to_owned(), account)))
        .collect();
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/usage-reconciliation",
        move || studio_runtime_services::costs::usage_reconciliation(&store, &agent, &profiles),
    )
    .await
    .map_err(|_| ApiError::internal())?
    .map_err(|error| match error {
        studio_runtime_services::costs::CostsError::Analytics(
            studio_runtime_store::analytics::AnalyticsError::InvalidRequest(_),
        ) => ApiError::new(StatusCode::NOT_FOUND, "Unknown agent"),
        _ => ApiError::internal(),
    })?;
    Ok(Json(super::generated_json(result)?))
}

inventory::submit! {
    super::manifest::RouteModuleRegistration {
        name: "usage_reconciliation", routes: Some(routes), dependencies: &[],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{body::Body, extract::ConnectInfo, http::Request};
    use serde_json::json;
    use std::{
        net::{Ipv4Addr, SocketAddr},
        sync::Arc,
    };
    use studio_runtime_store::{
        RuntimeStore, StoreError,
        analytics_capture::{AnalyticsCaptureRepository, CaptureTable},
    };
    use tower::ServiceExt;

    #[tokio::test]
    async fn usage_reconciliation_http_reads_archived_legacy_rows_and_native_conflicts() {
        let root = tempfile::tempdir().expect("temporary state");
        let store = Arc::new(RuntimeStore::open(Some(root.path())).await.expect("store"));
        let profile = root.path().join("profile");
        let project = profile.join("projects/project");
        std::fs::create_dir_all(&project).expect("project");
        let journal = json!({"type":"assistant","timestamp":"2026-09-01T12:00:00Z",
            "message":{"id":"response-1","model":"claude-sonnet-4-5",
                "usage":{"input_tokens":100,"cache_read_input_tokens":20,"cache_creation_input_tokens":5,"output_tokens":10}}});
        let mut conflict = journal.clone();
        conflict["message"]["usage"]["output_tokens"] = json!(99);
        let mut missing = journal.clone();
        missing["message"]["id"] = json!("response-missing-recorded");
        std::fs::write(
            project.join("thread.jsonl"),
            format!("{journal}\n{conflict}\n{missing}\n"),
        )
        .expect("journal");
        std::fs::create_dir_all(root.path().join("model-pricing")).expect("pricing directory");
        std::fs::write(root.path().join("model-pricing/models-dev-v1.json"), json!({"version":1,"catalog":{"providers":{
            "anthropic":{"models":{"claude-sonnet-4-5":{"cost":{"input":1,"output":2,"cache_read":0.5,"cache_write":1}}}},
            "openai":{"models":{"gpt-test":{"cost":{"input":1,"output":1}}}}
        }}}).to_string()).expect("pricing");
        store.analytics_transaction::<_,StoreError>(|tx,_| {
            AnalyticsCaptureRepository::write(tx, CaptureTable::Agents, &json!({"id":"archived","rootId":"archived",
                "provider":"claude","accountKey":"claude","threadId":"thread","archived":true}))?;
            for (id,response) in [("legacy","response-1"),("missing-evidence","response-missing-evidence")] {
                AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&json!({"id":id,"agentId":"archived","rootId":"archived",
                    "threadId":"thread","turnId":"turn","at":1,"accountKey":"claude","responseId":response,"model":"claude-sonnet-4-5",
                    "inputTokensAreUncached":true,"last":{"inputTokens":100,"cachedInputTokens":20,"cacheWriteInputTokens":5,"outputTokens":10},"total":{}}))?;
            }
            for (id,at,observation) in [
                ("gap",2,json!({"kind":"gap","responseId":"response-gap","reason":"streamInterrupted"})),
                ("cost-before",3,json!({"kind":"cumulative","scope":"claudeQuery","total":{"uncachedInput":100,"cacheReadInput":0,"cacheWriteInput":0,"output":10},
                    "models":[{"model":"claude-sonnet-4-5","tokens":{"uncachedInput":100,"cacheReadInput":0,"cacheWriteInput":0,"output":10},"providerCostUsd":0.2}]})),
                ("cost-after",4,json!({"kind":"cumulative","scope":"claudeQuery","total":{"uncachedInput":100,"cacheReadInput":0,"cacheWriteInput":0,"output":10},
                    "models":[{"model":"claude-sonnet-4-5","tokens":{"uncachedInput":100,"cacheReadInput":0,"cacheWriteInput":0,"output":10},"providerCostUsd":0.3}]})),
            ] {
                AnalyticsCaptureRepository::write(tx,CaptureTable::Usage,&json!({"id":id,"agentId":"archived","rootId":"archived","at":at,
                    "total":{},"last":{},"studioUsage":observation}))?;
            }
            Ok(())
        }).await.expect("archived analytics");
        let registry = json!({"version":1,"defaultAccountKey":"claude","logins":{},"accounts":{"claude":{
            "id":"claude","provider":"claude","home":profile,"label":"Claude","source":"Fixture","status":"ready","deleted":true,
            "claudeOptions":{"configDir":profile}}}});
        let policy = studio_runtime_services::account_policy::AccountPolicyState::from_startup(
            Some(&serde_json::to_vec(&registry).expect("registry")),
            [],
        )
        .expect("policy");
        let state = HttpState::new(
            "token".into(),
            "fixture".into(),
            root.path().display().to_string(),
            49172,
            None,
            Some(store),
            Some(Arc::new(policy)),
        );
        let app = crate::router::router(state).expect("router");
        for (uri, status) in [
            (
                "/api/diagnostics/usage-reconciliation?agent=archived",
                StatusCode::OK,
            ),
            (
                "/api/diagnostics/usage-reconciliation?agent=unknown",
                StatusCode::NOT_FOUND,
            ),
            (
                "/api/diagnostics/usage-reconciliation",
                StatusCode::BAD_REQUEST,
            ),
            (
                "/api/diagnostics/usage-reconciliation?agent=archived&extra=1",
                StatusCode::BAD_REQUEST,
            ),
        ] {
            let request = Request::builder()
                .uri(uri)
                .header("host", "127.0.0.1:49172")
                .header("x-canvas-token", "token")
                .extension(ConnectInfo(SocketAddr::from((Ipv4Addr::LOCALHOST, 12345))))
                .body(Body::empty())
                .expect("request");
            let response = app.clone().oneshot(request).await.expect("route");
            assert_eq!(response.status(), status, "{uri}");
            if status != StatusCode::OK {
                continue;
            }
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("bounded body");
            let value: Value = serde_json::from_slice(&bytes).expect("JSON");
            let _: crate::generated::UsageReconciliationResponse =
                serde_json::from_value(value.clone()).expect("generated route contract");
            assert_eq!(
                value["counts"],
                json!({"responses":3,"missingRecorded":1,"missingEvidence":1,"mismatched":1})
            );
            assert_eq!(value["gaps"][0]["responseId"], "response-gap");
            assert_eq!(
                value["providerCosts"],
                json!([{"model":"claude-sonnet-4-5","providerCostUsd":0.3}])
            );
            let row = value["responses"]
                .as_array()
                .expect("responses")
                .iter()
                .find(|row| row["responseId"] == "response-1")
                .expect("matched response");
            assert_eq!(row["recordedEvidence"], "legacy");
            assert_eq!(row["recordedTokens"]["output"], 10);
            assert_eq!(row["evidenceTokens"]["output"], 10);
            assert_eq!(row["journalConflict"], true);
            assert_eq!(row["recordedCostUsd"], json!(0.000135));
            assert_eq!(row["evidenceCostUsd"], json!(0.000135));
            assert_eq!(value["totals"]["recordedCostUsd"], json!(0.00027));
            assert_eq!(value["totals"]["evidenceCostUsd"], json!(0.00027));
        }
        // A malformed or oversized native state file is a file-local coverage
        // failure; the direct journal remains useful and the route stays 200.
        let sessions = root.path().join("account-servers/claude/sessions");
        std::fs::create_dir_all(&sessions).expect("native states");
        let state_path = sessions.join("thread.json");
        for size in [
            4,
            studio_runtime_services::costs::MAX_RECONCILIATION_NATIVE_BYTES + 1,
        ] {
            let file = std::fs::File::create(&state_path).expect("state file");
            file.set_len(size).expect("sparse state boundary");
            let request = Request::builder()
                .uri("/api/diagnostics/usage-reconciliation?agent=archived")
                .header("host", "127.0.0.1:49172")
                .header("x-canvas-token", "token")
                .extension(ConnectInfo(SocketAddr::from((Ipv4Addr::LOCALHOST, 12345))))
                .body(Body::empty())
                .expect("request");
            let response = app.clone().oneshot(request).await.expect("comparison");
            assert_eq!(response.status(), StatusCode::OK);
            let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
                .await
                .expect("bounded body");
            let value: Value = serde_json::from_slice(&bytes).expect("JSON");
            let report = value["files"]
                .as_array()
                .expect("files")
                .iter()
                .find(|file| file["kind"] == "claudeState")
                .expect("state report");
            assert_eq!(report["incomplete"], true);
            assert_eq!(
                report["truncated"],
                size > studio_runtime_services::costs::MAX_RECONCILIATION_NATIVE_BYTES
            );
            assert_eq!(value["counts"]["responses"], 3);
            assert_eq!(value["responses"][0]["evidenceTokens"]["output"], 10);
        }
    }
}
