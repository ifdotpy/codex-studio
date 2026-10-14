//! HTTP adaptation for project secrets and the platform-bound vault actions.

use super::extractors::ApiJson;
use super::manifest::{Dependency, OperationStatus, RouteGroup, dependency_unavailable, post};
use crate::error::ApiError;
use crate::router::HttpState;
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::time::Duration;
use studio_runtime_services::project_secrets::{self, GuestRpc, ProjectSecretsError};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

const VAULT_DESK: &[Dependency] = &[Dependency::new("macos-platform/layr-desk")];
const VAULT_SETUP_TIMEOUT: Duration = studio_runtime_services::vault::VAULT_SETUP_TIMEOUT;
const VAULT_LOGIN_TIMEOUT: Duration = studio_runtime_services::vault::VAULT_LOGIN_TIMEOUT;
const VAULT_PROGRESS_LINE_MAX_BYTES: usize = 16 * 1024;
const VAULT_JOB_CAPACITY: usize = 50;
const VAULT_JOB_MESSAGE_CAPACITY: usize = 256;
const INVALID_LOGIN_JSON: &str = "layr-desk aws-login returned invalid JSON";
const OVERSIZED_PROGRESS_LINE: &str = "layr-desk emitted an oversized progress line";

#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
enum VaultJobKind {
    Setup,
    Login,
}

struct VaultJobs {
    jobs: HashMap<(VaultJobKind, String), Value>,
    order: HashMap<VaultJobKind, VecDeque<String>>,
}

enum VaultJobMessage {
    Find {
        kind: VaultJobKind,
        request_id: String,
        reply: oneshot::Sender<Option<Value>>,
    },
    Reserve {
        kind: VaultJobKind,
        request_id: String,
        initial: Value,
        reply: oneshot::Sender<(Value, bool)>,
    },
    Update {
        kind: VaultJobKind,
        request_id: String,
        fields: Value,
        clear_progress: bool,
        reply: Option<oneshot::Sender<()>>,
    },
    Shutdown,
}

pub struct VaultJobClient {
    sender: mpsc::Sender<VaultJobMessage>,
}

impl Clone for VaultJobClient {
    fn clone(&self) -> Self {
        Self {
            sender: self.sender.clone(),
        }
    }
}

pub struct VaultJobOwner {
    client: VaultJobClient,
    task: JoinHandle<()>,
}

impl VaultJobOwner {
    /// Start the vault job owner on the current Tokio runtime.
    pub fn start() -> Self {
        let (sender, receiver) = mpsc::channel(VAULT_JOB_MESSAGE_CAPACITY);
        let task = tokio::spawn(run_vault_job_owner(receiver));
        Self {
            client: VaultJobClient { sender },
            task,
        }
    }

    pub fn client(&self) -> VaultJobClient {
        self.client.clone()
    }

    /// Ask the owner to stop and await its task before returning.
    pub async fn shutdown(self) -> std::io::Result<()> {
        let sent = self
            .client
            .sender
            .send(VaultJobMessage::Shutdown)
            .await
            .is_ok();
        let task_result = self.task.await.map_err(std::io::Error::other);
        if !sent {
            task_result?;
            return Err(std::io::Error::other(
                "vault job owner stopped before shutdown was requested",
            ));
        }
        task_result
    }
}

async fn run_vault_job_owner(mut receiver: mpsc::Receiver<VaultJobMessage>) {
    let mut jobs = VaultJobs {
        jobs: HashMap::new(),
        order: HashMap::new(),
    };
    while let Some(message) = receiver.recv().await {
        match message {
            VaultJobMessage::Find {
                kind,
                request_id,
                reply,
            } => {
                drop(reply.send(jobs.jobs.get(&(kind, request_id)).cloned()));
            }
            VaultJobMessage::Reserve {
                kind,
                request_id,
                initial,
                reply,
            } => {
                let key = (kind, request_id.clone());
                if let Some(job) = jobs.jobs.get(&key) {
                    drop(reply.send((job.clone(), false)));
                } else {
                    jobs.order.entry(kind).or_default().push_back(request_id);
                    jobs.jobs.insert(key, initial.clone());
                    loop {
                        let expired = {
                            let order = jobs.order.entry(kind).or_default();
                            if order.len() > VAULT_JOB_CAPACITY {
                                order.pop_front()
                            } else {
                                None
                            }
                        };
                        let Some(expired) = expired else {
                            break;
                        };
                        jobs.jobs.remove(&(kind, expired));
                    }
                    drop(reply.send((initial, true)));
                }
            }
            VaultJobMessage::Update {
                kind,
                request_id,
                mut fields,
                clear_progress,
                reply,
            } => {
                if let Some(job) = jobs.jobs.get_mut(&(kind, request_id))
                    && let Some(job) = job.as_object_mut()
                {
                    if clear_progress {
                        job.remove("progress");
                    }
                    if let Some(fields) = fields.as_object_mut() {
                        job.extend(std::mem::take(fields));
                    }
                }
                if let Some(reply) = reply
                    && reply.send(()).is_err()
                {
                    continue;
                }
            }
            VaultJobMessage::Shutdown => break,
        }
    }
}

pub(crate) fn routes() -> RouteGroup {
    RouteGroup::new(vec![
        post(
            "/api/project-secrets/list",
            OperationStatus::Implemented,
            list_secrets,
        ),
        post(
            "/api/project-secrets/remove",
            OperationStatus::Implemented,
            remove_secret,
        ),
        post(
            "/api/project-secrets/set",
            OperationStatus::Implemented,
            set_secret,
        ),
        post(
            "/api/vault/aws-login",
            OperationStatus::Implemented,
            aws_login,
        ),
        post(
            "/api/vault/pull",
            OperationStatus::partial(VAULT_DESK),
            pull,
        ),
        post("/api/vault/setup", OperationStatus::Implemented, setup),
        post(
            "/api/vault/setup/facts",
            OperationStatus::Implemented,
            setup_facts,
        ),
        post(
            "/api/vault/unlock",
            OperationStatus::partial(VAULT_DESK),
            unlock,
        ),
    ])
}

fn guest(state: &HttpState) -> Result<std::sync::Arc<dyn GuestRpc>, ApiError> {
    state.project_secrets_guest.clone().ok_or_else(|| {
        ApiError::new(
            StatusCode::SERVICE_UNAVAILABLE,
            "Linux VM guest RPC is unavailable",
        )
    })
}

fn request_string<'a>(body: &'a Value, key: &str) -> Result<&'a str, ApiError> {
    body.get(key)
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::validation(vec![Value::String("body".to_owned())]))
}

fn guest_error(error: ProjectSecretsError) -> ApiError {
    let status = match error.code() {
        Some("invalid_params" | "not_found" | "id_conflict") => StatusCode::BAD_REQUEST,
        _ => StatusCode::SERVICE_UNAVAILABLE,
    };
    let mut detail = error.detail().to_owned();
    if error.uncertain() {
        detail.push_str(" The change may have applied; list the names to check.");
    }
    ApiError::new(status, detail)
}

async fn guest_call(
    state: &HttpState,
    mutation: bool,
    call: impl FnOnce(&dyn GuestRpc) -> Result<Value, ProjectSecretsError> + Send + 'static,
) -> Result<Json<Value>, ApiError> {
    let rpc = guest(state)?;
    let result = tokio::task::spawn_blocking(move || call(rpc.as_ref()))
        .await
        .map_err(|_| {
            let message = if mutation {
                "The request outcome is unknown; the change may have applied. Retry only with the same requestId."
            } else {
                "The guest request was interrupted."
            };
            ApiError::new(StatusCode::SERVICE_UNAVAILABLE, message)
        })?;
    result.map(Json).map_err(guest_error)
}

async fn list_secrets(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?.to_owned();
    guest_call(&state, false, move |rpc| {
        project_secrets::list_names(rpc, &project_id)
    })
    .await
}

async fn set_secret(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?.to_owned();
    let name = request_string(&body, "name")?.to_owned();
    let value = request_string(&body, "value")?.to_owned();
    let request_id = request_string(&body, "requestId")?.to_owned();
    guest_call(&state, true, move |rpc| {
        project_secrets::set_secret(rpc, &project_id, &name, &value, &request_id)
    })
    .await
}

async fn remove_secret(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?.to_owned();
    let name = request_string(&body, "name")?.to_owned();
    let request_id = request_string(&body, "requestId")?.to_owned();
    guest_call(&state, true, move |rpc| {
        project_secrets::remove_secret(rpc, &project_id, &name, &request_id)
    })
    .await
}

async fn aws_login(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let profile = request_string(&body, "profile")?.to_owned();
    let request_id = request_string(&body, "requestId")?.to_owned();
    let job_client = vault_job_client(&state)?;
    if let Some(job) = find_job(&job_client, VaultJobKind::Login, &request_id).await? {
        return Ok(Json(job));
    }
    if !studio_runtime_services::vault::valid_profile(&profile) {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "Name an AWS profile",
        ));
    }
    let host_command = state
        .vault_host_command
        .clone()
        .ok_or_else(ApiError::internal)?;
    let initial = serde_json::json!({"requestId":request_id,"state":"running"});
    let (job, started) =
        reserve_job(&job_client, VaultJobKind::Login, &request_id, initial).await?;
    if started {
        tokio::spawn(run_login_job(request_id, profile, host_command, job_client));
    }
    Ok(Json(job))
}

async fn pull(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    unavailable("POST", "/api/vault/pull", VAULT_DESK)
}

async fn setup(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?.to_owned();
    let request_id = request_string(&body, "requestId")?.to_owned();
    let job_client = vault_job_client(&state)?;
    if let Some(job) = find_job(&job_client, VaultJobKind::Setup, &request_id).await? {
        return Ok(Json(job));
    }
    let requested_prefix = request_string(&body, "prefix")?.to_owned();
    let profile = request_string(&body, "profile")?.to_owned();
    let region = request_string(&body, "region")?.to_owned();
    let settings = settings_dir(&state.state_dir);
    let vm_state_dir =
        studio_runtime_services::vault::linux_vm_state_dir(std::path::Path::new(&state.state_dir));
    let (source, prefix) = studio_runtime_services::vault::prepare_new_vault(
        &vm_state_dir,
        &settings,
        &project_id,
        Some(&requested_prefix),
        &profile,
        &region,
    )
    .map_err(vault_error)?;
    let host_command = state
        .vault_host_command
        .as_ref()
        .ok_or_else(ApiError::internal)?;
    let writer = host_command.writer(&source);
    let initial = serde_json::json!({"requestId":request_id,"state":"running","prefix":prefix});
    let (job, started) =
        reserve_job(&job_client, VaultJobKind::Setup, &request_id, initial).await?;
    if started {
        tokio::spawn(run_setup_job(
            request_id,
            prefix,
            profile,
            region,
            writer,
            host_command.clone(),
            job_client,
        ));
    }
    Ok(Json(job))
}

async fn setup_facts(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?;
    let requested_prefix = body.get("prefix").and_then(Value::as_str);
    let vm_state_dir =
        studio_runtime_services::vault::linux_vm_state_dir(std::path::Path::new(&state.state_dir));
    studio_runtime_services::vault::prepare_setup_facts(
        &vm_state_dir,
        project_id,
        requested_prefix,
    )
    .map_err(vault_error)?;
    let args = vec!["aws-profiles".to_owned(), "--json".to_owned()];
    let mut ignore_output = |_: bool, _: &str| {};
    let host_command = state
        .vault_host_command
        .as_ref()
        .ok_or_else(ApiError::internal)?;
    let output = host_command
        .run_desk(
            &args,
            studio_runtime_services::vault::DESK_COMMAND_TIMEOUT,
            &mut ignore_output,
        )
        .await
        .map_err(vault_error)?;
    if output.timed_out {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "layr-desk aws-profiles timed out",
        ));
    }
    if output.code != Some(0) {
        let reason = desk_reason(&output.stderr, &output.stdout);
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            format!("layr-desk aws-profiles: {reason}"),
        ));
    }
    let profiles: Value = serde_json::from_str(&output.stdout).map_err(|_| {
        ApiError::new(
            StatusCode::BAD_REQUEST,
            "layr-desk aws-profiles returned invalid JSON",
        )
    })?;
    let settings = settings_dir(&state.state_dir);
    studio_runtime_services::vault::setup_facts(
        &vm_state_dir,
        &settings,
        project_id,
        requested_prefix,
        &profiles,
    )
    .map(Json)
    .map_err(vault_error)
}

fn vault_job_client(state: &HttpState) -> Result<VaultJobClient, ApiError> {
    state
        .vault_job_client
        .clone()
        .ok_or_else(ApiError::internal)
}

async fn find_job(
    client: &VaultJobClient,
    kind: VaultJobKind,
    request_id: &str,
) -> Result<Option<Value>, ApiError> {
    let (reply, result) = oneshot::channel();
    client
        .sender
        .send(VaultJobMessage::Find {
            kind,
            request_id: request_id.to_owned(),
            reply,
        })
        .await
        .map_err(|_| ApiError::internal())?;
    result.await.map_err(|_| ApiError::internal())
}

async fn reserve_job(
    client: &VaultJobClient,
    kind: VaultJobKind,
    request_id: &str,
    initial: Value,
) -> Result<(Value, bool), ApiError> {
    let (reply, result) = oneshot::channel();
    client
        .sender
        .send(VaultJobMessage::Reserve {
            kind,
            request_id: request_id.to_owned(),
            initial,
            reply,
        })
        .await
        .map_err(|_| ApiError::internal())?;
    result.await.map_err(|_| ApiError::internal())
}

fn update_job(
    client: &VaultJobClient,
    kind: VaultJobKind,
    request_id: &str,
    fields: Value,
    clear_progress: bool,
) {
    client
        .sender
        .try_send(VaultJobMessage::Update {
            kind,
            request_id: request_id.to_owned(),
            fields,
            clear_progress,
            reply: None,
        })
        .ok();
}

async fn finish_job(client: &VaultJobClient, kind: VaultJobKind, request_id: &str, fields: Value) {
    let (reply, result) = oneshot::channel();
    if client
        .sender
        .send(VaultJobMessage::Update {
            kind,
            request_id: request_id.to_owned(),
            fields,
            clear_progress: true,
            reply: Some(reply),
        })
        .await
        .is_ok()
    {
        drop(result.await);
    }
}

/// Consume newline-delimited output while keeping an unfinished record bounded.
/// Oversized records are discarded through their next newline and reported to the caller.
fn consume_progress_lines(
    pending: &mut String,
    discarding_oversized: &mut bool,
    chunk: &str,
    mut on_line: impl FnMut(&str),
) -> bool {
    let mut oversized = false;
    for part in chunk.split_inclusive('\n') {
        let complete = part.ends_with('\n');
        if *discarding_oversized {
            if complete {
                *discarding_oversized = false;
            }
            continue;
        }
        let line_part = if complete {
            &part[..part.len() - 1]
        } else {
            part
        };
        if pending.len().saturating_add(line_part.len()) > VAULT_PROGRESS_LINE_MAX_BYTES {
            pending.clear();
            oversized = true;
            *discarding_oversized = !complete;
            continue;
        }
        pending.push_str(line_part);
        if complete {
            on_line(pending.trim_end_matches('\r'));
            pending.clear();
        }
    }
    oversized
}

fn login_progress(
    client: &VaultJobClient,
    request_id: &str,
    line: &str,
) -> Result<(), &'static str> {
    let value = serde_json::from_str::<Value>(line).map_err(|_| INVALID_LOGIN_JSON)?;
    if let Some(page) = value.get("page").and_then(Value::as_str)
        && page.starts_with("https://")
    {
        let code = value
            .get("code")
            .map(|code| match code {
                Value::String(value) => value.clone(),
                other => other.to_string(),
            })
            .unwrap_or_default();
        update_job(
            client,
            VaultJobKind::Login,
            request_id,
            serde_json::json!({"page":page,"code":code.chars().take(40).collect::<String>()}),
            false,
        );
    }
    Ok(())
}

fn setup_progress(client: &VaultJobClient, request_id: &str, line: &str) {
    if let Some(progress) = line.strip_prefix("layr-desk: ") {
        update_job(
            client,
            VaultJobKind::Setup,
            request_id,
            serde_json::json!({"progress":progress.trim().chars().take(200).collect::<String>()}),
            false,
        );
    }
}

async fn run_login_job(
    request_id: String,
    profile: String,
    host_command: studio_runtime_services::vault::VaultHostCommand,
    job_client: VaultJobClient,
) {
    let args = vec![
        "aws-login".to_owned(),
        "--profile".to_owned(),
        profile,
        "--json".to_owned(),
    ];
    let mut pending = String::new();
    let mut discarding_oversized = false;
    let mut output_error = None;
    let mut on_output = |stdout: bool, chunk: &str| {
        if !stdout {
            return;
        }
        let oversized =
            consume_progress_lines(&mut pending, &mut discarding_oversized, chunk, |line| {
                if output_error.is_none()
                    && let Err(error) = login_progress(&job_client, &request_id, line)
                {
                    output_error = Some(error);
                }
            });
        if oversized {
            output_error = Some(OVERSIZED_PROGRESS_LINE);
        }
    };
    let result = host_command
        .run_desk(&args, VAULT_LOGIN_TIMEOUT, &mut on_output)
        .await;
    drop(on_output);
    if output_error.is_none()
        && !discarding_oversized
        && !pending.is_empty()
        && let Err(error) = login_progress(&job_client, &request_id, pending.trim_end_matches('\r'))
    {
        output_error = Some(error);
    }
    if let Some(error) = output_error {
        finish_job(
            &job_client,
            VaultJobKind::Login,
            &request_id,
            serde_json::json!({"state":"failed","error":error}),
        )
        .await;
        return;
    }
    match result {
        Ok(result) if result.code == Some(0) && !result.timed_out => {
            finish_job(
                &job_client,
                VaultJobKind::Login,
                &request_id,
                serde_json::json!({"state":"done"}),
            )
            .await;
        }
        Ok(result) => {
            let error = desk_reason(&result.stderr, &result.stdout);
            finish_job(
                &job_client,
                VaultJobKind::Login,
                &request_id,
                serde_json::json!({"state":"failed","error":error}),
            )
            .await;
        }
        Err(error) => {
            finish_job(
                &job_client,
                VaultJobKind::Login,
                &request_id,
                serde_json::json!({"state":"failed","error":error.to_string()}),
            )
            .await;
        }
    }
}

async fn run_setup_job(
    request_id: String,
    prefix: String,
    profile: String,
    region: String,
    writer: String,
    host_command: studio_runtime_services::vault::VaultHostCommand,
    job_client: VaultJobClient,
) {
    let args = vec![
        "deploy".to_owned(),
        "--project".to_owned(),
        prefix.clone(),
        "--profile".to_owned(),
        profile,
        "--region".to_owned(),
        region,
        "--writer".to_owned(),
        writer,
    ];
    let mut pending = String::new();
    let mut discarding_oversized = false;
    let mut output_error = None;
    let mut on_output = |stdout: bool, chunk: &str| {
        if stdout {
            return;
        }
        let oversized =
            consume_progress_lines(&mut pending, &mut discarding_oversized, chunk, |line| {
                setup_progress(&job_client, &request_id, line)
            });
        if oversized {
            output_error = Some(OVERSIZED_PROGRESS_LINE);
        }
    };
    let result = host_command
        .run_desk(&args, VAULT_SETUP_TIMEOUT, &mut on_output)
        .await;
    drop(on_output);
    if output_error.is_none() && !discarding_oversized && !pending.is_empty() {
        setup_progress(&job_client, &request_id, pending.trim_end_matches('\r'));
    }
    if let Some(error) = output_error {
        finish_job(
            &job_client,
            VaultJobKind::Setup,
            &request_id,
            serde_json::json!({"state":"failed","error":error}),
        )
        .await;
        return;
    }
    match result {
        Ok(result) if result.code == Some(0) && !result.timed_out => {
            let site = result
                .stdout
                .lines()
                .rfind(|line| !line.trim().is_empty())
                .and_then(|line| serde_json::from_str::<Value>(line.trim()).ok())
                .and_then(|value| value.get("site").and_then(Value::as_str).map(str::to_owned));
            match site {
                Some(site) => {
                    finish_job(
                        &job_client,
                        VaultJobKind::Setup,
                        &request_id,
                        serde_json::json!({"state":"done","site":site}),
                    )
                    .await
                }
                None => finish_job(
                    &job_client,
                    VaultJobKind::Setup,
                    &request_id,
                    serde_json::json!({"state":"failed","error":"layr-desk returned invalid JSON"}),
                )
                .await,
            }
        }
        Ok(result) if result.code == Some(3) => {
            let error = desk_reason(&result.stderr, &result.stdout);
            finish_job(
                &job_client,
                VaultJobKind::Setup,
                &request_id,
                serde_json::json!({"state":"signin","error":error}),
            )
            .await;
        }
        Ok(result) => {
            let error = desk_reason(&result.stderr, &result.stdout);
            finish_job(
                &job_client,
                VaultJobKind::Setup,
                &request_id,
                serde_json::json!({"state":"failed","error":error}),
            )
            .await;
        }
        Err(error) => {
            finish_job(
                &job_client,
                VaultJobKind::Setup,
                &request_id,
                serde_json::json!({"state":"failed","error":error.to_string()}),
            )
            .await
        }
    }
}

fn settings_dir(state_dir: &str) -> std::path::PathBuf {
    std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(state_dir))
        .join(".config/studio-vault")
}

fn vault_error(error: studio_runtime_services::vault::VaultError) -> ApiError {
    ApiError::new(StatusCode::BAD_REQUEST, error.to_string())
}

fn desk_reason(stderr: &str, stdout: &str) -> String {
    let text = stderr.trim();
    let text = if let Some(at) = text.rfind("Error: ") {
        &text[at + "Error: ".len()..]
    } else if !text.is_empty() {
        text.lines().last().unwrap_or("layr-desk failed")
    } else if !stdout.trim().is_empty() {
        stdout.lines().last().unwrap_or("layr-desk failed")
    } else {
        "layr-desk failed"
    };
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(500)
        .collect()
}

async fn unlock(ApiJson(_body): ApiJson<Value>) -> Result<Json<Value>, ApiError> {
    unavailable("POST", "/api/vault/unlock", VAULT_DESK)
}

fn unavailable(
    method: &str,
    path: &str,
    dependencies: &[Dependency],
) -> Result<Json<Value>, ApiError> {
    Err(dependency_unavailable(method, path, dependencies).unwrap_or_else(|_| ApiError::internal()))
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "host_secrets",
        routes: Some(routes),
        dependencies: &[
            "linux-vm/guest-rpc",
            "macos-platform/layr-desk",
        ],
    }
}

#[cfg(test)]
mod tests {
    use super::{
        GuestRpc, INVALID_LOGIN_JSON, OVERSIZED_PROGRESS_LINE, VAULT_JOB_CAPACITY,
        VAULT_PROGRESS_LINE_MAX_BYTES, VaultJobClient, VaultJobKind, VaultJobOwner,
        consume_progress_lines, find_job, reserve_job, run_login_job, run_setup_job,
    };
    use crate::router::HttpState;
    use axum::body::Body;
    use axum::extract::ConnectInfo;
    use axum::http::{Method, Request, StatusCode};
    use serde_json::{Value, json};
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::sync::{Arc, Mutex};
    use std::time::Duration;
    use studio_runtime_schema::shared_values::PROJECT_SECRETS_GUEST_TIMEOUT_SECONDS;
    use studio_runtime_services::project_secrets::{GuestCallError, ProjectSecretsError};
    use tower::ServiceExt;

    const TEST_HTTP_PORT: u16 = 49_171;
    const TEST_HTTP_HOST: &str = "127.0.0.1:49171";

    #[derive(Default)]
    struct FakeGuest {
        calls: Mutex<Vec<(String, Value, Option<String>)>>,
    }

    impl GuestRpc for FakeGuest {
        fn call(
            &self,
            method: &str,
            params: &Value,
            request_id: Option<&str>,
            timeout: Duration,
        ) -> Result<Value, studio_runtime_services::project_secrets::GuestCallError> {
            assert_eq!(
                timeout,
                Duration::from_secs(PROJECT_SECRETS_GUEST_TIMEOUT_SECONDS)
            );
            self.calls.lock().unwrap().push((
                method.to_owned(),
                params.clone(),
                request_id.map(str::to_owned),
            ));
            Ok(json!({"names":["TOKEN"]}))
        }
    }

    fn app(
        state_dir: &std::path::Path,
        guest: Option<Arc<dyn GuestRpc>>,
        host_command: Option<studio_runtime_services::vault::VaultHostCommand>,
    ) -> axum::Router {
        let state = HttpState::new(
            "test-token".to_owned(),
            "test-workspace".to_owned(),
            None,
            state_dir.to_string_lossy().into_owned(),
            TEST_HTTP_PORT,
            None,
            None,
            None,
        );
        let mut state = match guest {
            Some(guest) => state.with_project_secrets_guest_rpc(guest),
            None => state,
        };
        state.vault_host_command = host_command;
        crate::router::router(state).expect("runtime HTTP router starts")
    }

    #[cfg(unix)]
    fn fake_host_command(
        directory: &std::path::Path,
        script_body: &str,
    ) -> studio_runtime_services::vault::VaultHostCommand {
        use std::os::unix::fs::PermissionsExt;

        let executable = directory.join("layr-desk");
        std::fs::write(&executable, format!("#!/bin/sh\n{script_body}\n")).unwrap();
        std::fs::set_permissions(&executable, std::fs::Permissions::from_mode(0o755)).unwrap();
        studio_runtime_services::vault::VaultHostCommand::new(
            std::path::PathBuf::from("/tmp/python"),
            std::path::PathBuf::from("/tmp/server"),
        )
        .with_desk_program(executable)
    }

    fn test_request_id(label: &str) -> String {
        format!(
            "{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        )
    }

    async fn test_job_client() -> (VaultJobOwner, VaultJobClient) {
        let owner = VaultJobOwner::start();
        let client = owner.client();
        (owner, client)
    }

    #[test]
    fn progress_buffer_stays_bounded_and_discards_until_the_next_newline() {
        let mut pending = String::new();
        let mut discarding = false;
        let mut lines = Vec::new();
        let oversized = "x".repeat(VAULT_PROGRESS_LINE_MAX_BYTES + 1);

        assert!(consume_progress_lines(
            &mut pending,
            &mut discarding,
            &oversized,
            |line| lines.push(line.to_owned()),
        ));
        assert!(pending.is_empty());
        assert!(pending.len() <= VAULT_PROGRESS_LINE_MAX_BYTES);
        assert!(discarding);
        assert!(lines.is_empty());

        assert!(!consume_progress_lines(
            &mut pending,
            &mut discarding,
            "\nnext record\n",
            |line| lines.push(line.to_owned()),
        ));
        assert!(!discarding);
        assert_eq!(lines, ["next record"]);
    }

    fn authorized_request(body: Value) -> Request<Body> {
        authorized_request_at("/api/project-secrets/set", body)
    }

    fn authorized_request_at(uri: &str, body: Value) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri(uri)
            .header("host", TEST_HTTP_HOST)
            .header("content-type", "application/json")
            .header("x-canvas-token", "test-token")
            .header(
                crate::schema::API_SCHEMA_HASH_HEADER,
                crate::schema::api_schema_hash(),
            )
            .extension(ConnectInfo(SocketAddr::new(
                IpAddr::V4(Ipv4Addr::LOCALHOST),
                12345,
            )))
            .body(Body::from(body.to_string()))
            .unwrap()
    }

    #[tokio::test]
    async fn vault_setup_facts_route_uses_the_host_command_and_cached_project() {
        let state = tempfile::tempdir().unwrap();
        let project = state.path().join("linux-vm/layr-projects/p1");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("project.json"),
            br#"{"projectId":"p1","source":"/tmp/Project One"}"#,
        )
        .unwrap();
        let host_command = fake_host_command(
            state.path(),
            "[ \"$1\" = aws-profiles ] && [ \"$2\" = --json ] || exit 91\nprintf '%s\\n' '{\"profiles\":[{\"name\":\"fixture\"}]}'",
        );
        let app = app(state.path(), None, Some(host_command));
        let request = authorized_request_at("/api/vault/setup/facts", json!({"projectId":"p1"}));

        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(
            response,
            json!({
                "prefix":"project-one",
                "keys":false,
                "site":null,
                "region":"eu-central-1",
                "gatewayProfile":null,
                "profiles":[{"name":"fixture"}]
            })
        );
    }

    #[tokio::test]
    async fn project_secret_errors_redact_guest_detail() {
        let error = super::guest_error(ProjectSecretsError::Guest(GuestCallError {
            code: "unknown".to_owned(),
            detail: "secret value must not be returned".to_owned(),
            uncertain: true,
        }));
        let response = axum::response::IntoResponse::into_response(error);
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let response: Value = serde_json::from_slice(&body).unwrap();

        assert!(!response.to_string().contains("secret value"));
        assert!(
            response["error"]
                .as_str()
                .unwrap()
                .contains("may have applied")
        );
    }

    #[tokio::test]
    async fn project_secret_set_calls_guest_and_never_returns_the_value() {
        let state = tempfile::tempdir().unwrap();
        let fake = Arc::new(FakeGuest::default());
        let app = app(state.path(), Some(fake.clone()), None);
        let request = authorized_request(
            json!({"projectId":"p1","name":"TOKEN","value":"generated-test-value","requestId":"req-1"}),
        );

        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let response: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(response, json!({"projectId":"p1","names":["TOKEN"]}));
        assert!(!response.to_string().contains("generated-test-value"));
        assert_eq!(
            fake.calls.lock().unwrap()[0].2.as_deref(),
            Some("secrets-set:req-1")
        );
    }

    #[tokio::test]
    async fn project_secret_route_refuses_without_guest_and_writes_nothing() {
        let state = tempfile::tempdir().unwrap();
        let app = app(state.path(), None, None);
        let request = authorized_request(
            json!({"projectId":"p1","name":"TOKEN","value":"generated-test-value","requestId":"req-1"}),
        );

        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            String::from_utf8_lossy(&body)
        );
        let body = String::from_utf8_lossy(&body);
        assert!(body.contains("Linux VM guest RPC is unavailable"));
        assert!(!body.contains("generated-test-value"));
        assert!(!state.path().join("layr-projects").exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn malformed_login_json_fails_the_job() {
        let root = tempfile::tempdir().unwrap();
        let (owner, client) = test_job_client().await;
        let request_id = test_request_id("malformed-login");
        let host_command = fake_host_command(root.path(), "printf '%s\\n' '{malformed'");
        reserve_job(
            &client,
            VaultJobKind::Login,
            &request_id,
            json!({"requestId":request_id,"state":"running"}),
        )
        .await
        .unwrap();

        run_login_job(
            request_id.clone(),
            "default".to_owned(),
            host_command,
            client.clone(),
        )
        .await;

        let job = find_job(&client, VaultJobKind::Login, &request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job["state"], "failed");
        assert_eq!(job["error"], INVALID_LOGIN_JSON);
        owner.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_login_and_setup_lines_fail_the_jobs_with_bounded_buffers() {
        let root = tempfile::tempdir().unwrap();
        let (owner, client) = test_job_client().await;
        let login_id = test_request_id("oversized-login");
        reserve_job(
            &client,
            VaultJobKind::Login,
            &login_id,
            json!({"requestId":login_id,"state":"running"}),
        )
        .await
        .unwrap();
        run_login_job(
            login_id.clone(),
            "default".to_owned(),
            fake_host_command(root.path(), "printf '%16385s' ''"),
            client.clone(),
        )
        .await;
        let login = find_job(&client, VaultJobKind::Login, &login_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(login["state"], "failed");
        assert_eq!(login["error"], OVERSIZED_PROGRESS_LINE);

        let setup_id = test_request_id("oversized-setup");
        reserve_job(
            &client,
            VaultJobKind::Setup,
            &setup_id,
            json!({"requestId":setup_id,"state":"running"}),
        )
        .await
        .unwrap();
        run_setup_job(
            setup_id.clone(),
            "project".to_owned(),
            "default".to_owned(),
            "eu-central-1".to_owned(),
            "writer".to_owned(),
            fake_host_command(root.path(), "printf '%16385s' '' >&2"),
            client.clone(),
        )
        .await;
        let setup = find_job(&client, VaultJobKind::Setup, &setup_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(setup["state"], "failed");
        assert_eq!(setup["error"], OVERSIZED_PROGRESS_LINE);
        owner.shutdown().await.unwrap();
    }

    #[tokio::test]
    async fn login_capacity_does_not_evict_setup_receipts() {
        let (owner, client) = test_job_client().await;
        let setup_id = test_request_id("retained-setup");
        reserve_job(
            &client,
            VaultJobKind::Setup,
            &setup_id,
            json!({"requestId":setup_id,"state":"done"}),
        )
        .await
        .unwrap();

        for index in 0..VAULT_JOB_CAPACITY {
            let request_id = test_request_id(&format!("login-{index}"));
            reserve_job(
                &client,
                VaultJobKind::Login,
                &request_id,
                json!({"requestId":request_id,"state":"done"}),
            )
            .await
            .unwrap();
        }

        assert_eq!(
            find_job(&client, VaultJobKind::Setup, &setup_id)
                .await
                .unwrap()
                .unwrap()["state"],
            "done"
        );
        owner.shutdown().await.unwrap();
    }
}
