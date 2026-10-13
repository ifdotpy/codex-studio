//! Typed HTTP boundary for persisted agent operations.
use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, post};
use crate::{error::ApiError, generated, router::HttpState};
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use studio_runtime_services::agents_create::{self, Context, Creation, OperationError};

pub(crate) fn routes() -> RouteGroup {
    const LEAD_DEPENDENCIES: [Dependency; 6] = [
        Dependency::new("account-driver/unavailable"),
        Dependency::new("agent-management/empty-chat-reuse"),
        Dependency::new("project-driver/chat-binding"),
        Dependency::new("provider-session/model-list"),
        Dependency::new("worker-driver/settings-catalog"),
        Dependency::new("workspace-state/lead-workspace"),
    ];
    const RENAME_DEPENDENCIES: [Dependency; 1] =
        [Dependency::new("provider-session/title-generation")];
    const AGENT_DEPENDENCIES: [Dependency; 6] = [
        Dependency::new("account-driver/unavailable"),
        Dependency::new("provider-session/model-list"),
        Dependency::new("worker-driver/settings-catalog"),
        Dependency::new("worker-driver/work-assignment"),
        Dependency::new("worker-driver/workspace-preparation"),
        Dependency::new("workspace-state/lead-workspace"),
    ];
    const CONFIGURE_DEPENDENCIES: [Dependency; 1] =
        [Dependency::new("agent-management/agent-mode-settings")];
    const CONVERSATION_DEPENDENCIES: [Dependency; 1] =
        [Dependency::new("agent-management/conversation-settings")];
    const DELETE_DEPENDENCY: [Dependency; 1] =
        [Dependency::new("agent-management/conversation-termination")];
    RouteGroup::new(vec![
        post(
            "/api/agents",
            OperationStatus::partial(&AGENT_DEPENDENCIES),
            create_agent,
        ),
        post(
            "/api/configure",
            OperationStatus::partial(&CONFIGURE_DEPENDENCIES),
            configure,
        ),
        post(
            "/api/conversation",
            OperationStatus::partial(&CONVERSATION_DEPENDENCIES),
            conversation,
        ),
        post(
            "/api/conversation/delete",
            OperationStatus::partial(&DELETE_DEPENDENCY),
            delete_conversation,
        ),
        post(
            "/api/leads",
            OperationStatus::partial(&LEAD_DEPENDENCIES),
            create_lead,
        ),
        post(
            "/api/rename",
            OperationStatus::partial(&RENAME_DEPENDENCIES),
            rename,
        ),
    ])
}

async fn dispatch<T: serde::de::DeserializeOwned + serde::Serialize>(
    state: HttpState,
    operation: impl FnOnce(&Context<'_>) -> Result<Value, OperationError> + Send + 'static,
) -> Result<Json<Value>, ApiError> {
    let result = tokio::task::spawn_blocking(move || {
        let store = state.store.as_deref().ok_or_else(ApiError::internal)?;
        let context = Context {
            store,
            accounts: state.account_policy.as_deref(),
            driver: state.account_driver.as_deref(),
            validate: &|schema, value| state.validators.validate_stored(schema, value),
        };
        let result = operation(&context).map_err(operation_error)?;
        state.event_wake.notify_one();
        super::generated_json::<T>(result).map(Json)
    })
    .await
    .map_err(|_| ApiError::internal())??;
    Ok(result)
}

async fn create_lead(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::CreateLeadRequest>,
) -> Result<Json<Value>, ApiError> {
    let request = Creation {
        intent: serde_json::to_value(&body).map_err(|_| ApiError::internal())?,
        catalog: body.effort.is_some()
            || body.fast_mode.is_some()
            || body.daybreak_enabled.is_some()
            || body.model.as_ref().is_some_and(Option::is_some),
        worker_catalog: body.worker_defaults.is_some() || body.review_defaults.is_some(),
        project_binding: body.project_id.flatten().is_some()
            || body.project_server_id.flatten().is_some()
            || body.project_folder.flatten().is_some(),
        id: body.id.flatten(),
        cwd: body.cwd,
        account: body.account_key,
        workspace: body.workspace_mode.map(|value| value.to_string()),
        yolo: body.yolo_mode,
        previous: body.previous.flatten(),
        reuse_empty: body.reuse_empty,
        ..Creation::default()
    };
    dispatch::<generated::AgentResponse>(state, move |context| {
        agents_create::lead(context, request)
    })
    .await
}
async fn create_agent(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::CreateAgentRequest>,
) -> Result<Json<Value>, ApiError> {
    if state.store.is_none() && state.account_driver.is_none() {
        return Err(operation_error(OperationError::Unavailable(
            "account-driver/unavailable",
        )));
    }
    let request = Creation {
        intent: serde_json::to_value(&body).map_err(|_| ApiError::internal())?,
        catalog: body.model.is_some()
            || body.effort.is_some()
            || body.fast_mode.is_some()
            || body.daybreak_enabled.is_some(),
        worker_catalog: body.worker_defaults.is_some(),
        id: body.id.flatten(),
        cwd: body.cwd,
        account: body.account_key,
        workspace: body.workspace_mode.map(|value| value.to_string()),
        name: body.name,
        prompt: body.prompt,
        role: body.role.map(|value| value.to_string()),
        yolo: body.yolo_mode,
        concurrency: body.concurrency,
        max_agents: body.max_agents,
        token_budget: body.token_budget.flatten(),
        profile: body.profile_id.flatten(),
        parent: body.parent.flatten(),
        ..Creation::default()
    };
    dispatch::<generated::AgentResponse>(state, move |context| {
        agents_create::agent(context, request)
    })
    .await
}
async fn configure(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::ConfigureRequest>,
) -> Result<Json<Value>, ApiError> {
    let request = agents_create::Configuration {
        id: body.id,
        concurrency: body.concurrency,
        max_agents: body.max_agents,
        token_budget: body.token_budget,
        mode_receipt: body.expected_mode_revision.is_some() || body.request_id.is_some(),
    };
    dispatch::<generated::ConfigureResponse>(state, move |context| {
        agents_create::configure(context, request)
    })
    .await
}
async fn conversation(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::ConversationRequest>,
) -> Result<Json<Value>, ApiError> {
    let request = agents_create::Conversation {
        non_defaults: body.next_turn.is_some() || body.request_id.is_some(),
        settings: body.cwd.is_some()
            || body.yolo_mode.is_some()
            || body.model.is_some()
            || body.effort.is_some()
            || body.fast_mode.is_some()
            || body.daybreak_enabled.is_some()
            || body.next_turn.flatten().is_some()
            || body.request_id.flatten().is_some()
            || body.worker_defaults.is_some()
            || body.review_defaults.is_some()
            || body.agent_mode.is_some()
            || body.subagent_concurrency.is_some()
            || body.expected_mode_revision.is_some(),
        id: body.id,
        expected_account: body.expected_account_key,
        excluded: body.excluded_account_keys.map(|mut keys| {
            keys.sort();
            keys
        }),
    };
    dispatch::<generated::AgentResponse>(state, move |context| {
        agents_create::conversation(context, request)
    })
    .await
}
async fn rename(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<generated::RenameRequest>,
) -> Result<Json<Value>, ApiError> {
    dispatch::<generated::RenameResponse>(state, move |context| {
        agents_create::rename_operation(
            context,
            &body.id,
            body.name.flatten().as_deref(),
            body.request_id.flatten().as_deref(),
        )
    })
    .await
}
async fn delete_conversation(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    Err(operation_error(OperationError::Unavailable(
        "agent-management/conversation-termination",
    )))
}
fn operation_error(error: OperationError) -> ApiError {
    use agents_create::MutationError;
    if matches!(
        error,
        OperationError::Create(MutationError::RequestIdRequired)
    ) {
        return ApiError::validation(vec![Value::String("body".to_owned())]);
    }
    let status = match &error {
        OperationError::Unavailable(_)
        | OperationError::Create(MutationError::TitleGenerationUnavailable) => {
            StatusCode::NOT_IMPLEMENTED
        }
        OperationError::Internal
        | OperationError::Store(_)
        | OperationError::Create(
            MutationError::Store(_) | MutationError::InvalidStoredObject { .. },
        ) => StatusCode::INTERNAL_SERVER_ERROR,
        _ => StatusCode::BAD_REQUEST,
    };
    if matches!(
        error,
        OperationError::Store(_) | OperationError::Create(MutationError::Store(_))
    ) {
        return ApiError::internal();
    }
    ApiError::new(status, error.to_string())
}
