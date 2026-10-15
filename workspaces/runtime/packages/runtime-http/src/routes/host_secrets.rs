//! HTTP adaptation for project secrets and the platform-bound vault actions.

use super::extractors::ApiJson;
use super::manifest::{OperationStatus, RouteGroup, post};
use crate::error::ApiError;
use crate::router::HttpState;
use axum::{Json, extract::State, http::StatusCode};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use studio_runtime_services::project_secrets::{self, GuestRpc, ProjectSecretsError};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;

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
    Pull,
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
        post("/api/vault/pull", OperationStatus::Implemented, pull),
        post("/api/vault/setup", OperationStatus::Implemented, setup),
        post(
            "/api/vault/setup/facts",
            OperationStatus::Implemented,
            setup_facts,
        ),
        post("/api/vault/unlock", OperationStatus::Implemented, unlock),
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
    let result = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/host_secrets",
        move || call(rpc.as_ref()),
    )
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

async fn pull(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?.to_owned();
    let request_id = request_string(&body, "requestId")?.to_owned();
    let job_client = vault_job_client(&state)?;
    if let Some(job) = find_job(&job_client, VaultJobKind::Pull, &request_id).await? {
        return Ok(Json(job));
    }
    let view = studio_runtime_services::vault::view(&state.vm_state_dir, &project_id)
        .map_err(vault_error)?
        .filter(|view| view.get("state").and_then(Value::as_str) != Some("none"))
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The project has no vault"))?;
    let site = view
        .get("site")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "The Desk site of this vault is not known; run codex_vault.py site",
            )
        })?;
    let prefix = view
        .get("prefix")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::new(StatusCode::BAD_REQUEST, "The project has no vault"))?;
    let home = std::env::var_os("HOME")
        .map(std::path::PathBuf::from)
        .ok_or_else(ApiError::internal)?;
    let requested_parent = body.get("to").and_then(Value::as_str);
    let parent = match requested_parent {
        Some("~") => home.clone(),
        Some(path) if path.starts_with("~/") => home.join(&path[2..]),
        Some(path) => std::path::PathBuf::from(path),
        None => home.join("Downloads"),
    };
    let target =
        studio_runtime_services::vault::pull_target(&parent, prefix).map_err(vault_error)?;
    let host_command = state
        .vault_host_command
        .clone()
        .ok_or_else(ApiError::internal)?;
    let desk_program = host_command.desk_program().map_err(vault_error)?;
    let host_command = host_command.with_desk_program(desk_program);
    let initial = serde_json::json!({"requestId":request_id,"state":"running","folder":target});
    let (job, started) = reserve_job(&job_client, VaultJobKind::Pull, &request_id, initial).await?;
    if started {
        let args = vec![
            "pull".to_owned(),
            "--site".to_owned(),
            site.to_owned(),
            "--project".to_owned(),
            prefix.to_owned(),
            "--to".to_owned(),
            target.to_string_lossy().into_owned(),
            "--json".to_owned(),
        ];
        tokio::spawn(run_pull_job(request_id, args, host_command, job_client));
    }
    Ok(Json(job))
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
    let host_command = state
        .vault_host_command
        .as_ref()
        .ok_or_else(ApiError::internal)?;
    let python_tool = host_command.python_tool().map_err(vault_error)?;
    let settings = settings_dir(&state.state_dir);
    let vm_state_dir = state.vm_state_dir.clone();
    let (source, prefix) = studio_runtime_services::vault::prepare_new_vault(
        &vm_state_dir,
        &settings,
        &project_id,
        Some(&requested_prefix),
        &profile,
        &region,
    )
    .map_err(vault_error)?;
    let writer = host_command.writer(&source, &python_tool);
    let initial = serde_json::json!({"requestId":request_id,"state":"running","prefix":prefix});
    let (job, started) =
        reserve_job(&job_client, VaultJobKind::Setup, &request_id, initial).await?;
    if started {
        tokio::spawn(run_setup_job(SetupJob {
            request_id,
            prefix,
            profile,
            region,
            writer,
            host_command: host_command.clone(),
            vm_state_dir,
            sync_changes: Arc::clone(&state.sync_changes),
            job_client,
        }));
    }
    Ok(Json(job))
}

async fn setup_facts(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?;
    let requested_prefix = body.get("prefix").and_then(Value::as_str);
    let vm_state_dir = state.vm_state_dir.clone();
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

struct SetupJob {
    request_id: String,
    prefix: String,
    profile: String,
    region: String,
    writer: String,
    host_command: studio_runtime_services::vault::VaultHostCommand,
    vm_state_dir: std::path::PathBuf,
    sync_changes: Arc<studio_runtime_services::change_notifications::ChangeNotificationHub>,
    job_client: VaultJobClient,
}

async fn run_setup_job(job: SetupJob) {
    let SetupJob {
        request_id,
        prefix,
        profile,
        region,
        writer,
        host_command,
        vm_state_dir,
        sync_changes,
        job_client,
    } = job;
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
                    if let Err(error) = refresh_mac_views(&vm_state_dir, &host_command).await {
                        // The deploy already succeeded; keep its receipt successful. The local
                        // status refresh is best-effort.
                        eprintln!("Vault setup succeeded, but Mac status refresh failed: {error}");
                    }
                    sync_changes.notify_one();
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

async fn unlock(
    State(state): State<HttpState>,
    ApiJson(body): ApiJson<Value>,
) -> Result<Json<Value>, ApiError> {
    let project_id = request_string(&body, "projectId")?.to_owned();
    let words = request_string(&body, "words")?.to_owned();
    let view = studio_runtime_services::vault::view(&state.vm_state_dir, &project_id)
        .map_err(vault_error)?
        .filter(|view| {
            view.get("site").and_then(Value::as_str).is_some()
                && view.get("prefix").and_then(Value::as_str).is_some()
        })
        .ok_or_else(|| {
            ApiError::new(
                StatusCode::BAD_REQUEST,
                "The project has no vault with a Desk site",
            )
        })?;
    let host_command = state
        .vault_host_command
        .clone()
        .ok_or_else(ApiError::internal)?;
    let desk_program = host_command.desk_program().map_err(vault_error)?;
    let host_command = host_command.with_desk_program(desk_program);
    let args = vec![
        "unlock".to_owned(),
        "--site".to_owned(),
        view["site"].as_str().unwrap_or_default().to_owned(),
        "--project".to_owned(),
        view["prefix"].as_str().unwrap_or_default().to_owned(),
        "--paper-file".to_owned(),
        "-".to_owned(),
    ];
    let mut discard_output = |_: bool, _: &str| {};
    let result = host_command
        .run_desk_with_input(
            &args,
            words.as_bytes(),
            studio_runtime_services::vault::DESK_COMMAND_TIMEOUT,
            &mut discard_output,
        )
        .await
        .map_err(vault_error)?;
    if result.timed_out {
        return Err(ApiError::new(
            StatusCode::BAD_REQUEST,
            "layr-desk command timed out",
        ));
    }
    if result.code != Some(0) {
        let output = if result.stderr.trim().is_empty() {
            &result.stdout
        } else {
            &result.stderr
        };
        let lines = output.trim().lines().collect::<Vec<_>>();
        let reason = lines
            .iter()
            .find(|line| {
                ["does not open", "checksum", "not a word"]
                    .iter()
                    .any(|part| line.contains(part))
            })
            .copied()
            .or_else(|| lines.last().copied())
            .unwrap_or("layr-desk failed");
        let sanitized = sanitize_unlock_reason(reason);
        return Err(ApiError::new(StatusCode::BAD_REQUEST, sanitized));
    }
    refresh_mac_views(&state.vm_state_dir, &host_command)
        .await
        .map_err(vault_error)?;
    state.sync_changes.notify_one();
    Ok(Json(serde_json::json!({"unlocked":true})))
}

async fn run_pull_job(
    request_id: String,
    args: Vec<String>,
    host_command: studio_runtime_services::vault::VaultHostCommand,
    job_client: VaultJobClient,
) {
    let mut discard_output = |_: bool, _: &str| {};
    let result = host_command
        .run_desk(
            &args,
            studio_runtime_services::vault::VAULT_PULL_TIMEOUT,
            &mut discard_output,
        )
        .await;
    let fields = match result {
        Ok(result) if result.timed_out => {
            serde_json::json!({"state":"failed","error":"layr-desk pull timed out"})
        }
        Ok(result) if result.code == Some(0) => match serde_json::from_str::<Value>(&result.stdout)
        {
            Ok(value)
                if value.get("seq").is_some()
                    && value.get("files").is_some()
                    && value.get("bytes").is_some() =>
            {
                serde_json::json!({"state":"done","seq":value["seq"],"files":value["files"],"bytes":value["bytes"]})
            }
            _ => serde_json::json!({"state":"failed","error":"KeyError: 'seq'"}),
        },
        Ok(result) => {
            serde_json::json!({"state":"failed","error":desk_reason(&result.stderr,&result.stdout)})
        }
        Err(error) => serde_json::json!({"state":"failed","error":error.to_string()}),
    };
    finish_job(&job_client, VaultJobKind::Pull, &request_id, fields).await;
}

async fn refresh_mac_views(
    state_dir: &std::path::Path,
    host_command: &studio_runtime_services::vault::VaultHostCommand,
) -> Result<(), studio_runtime_services::vault::VaultError> {
    use std::collections::{BTreeMap, BTreeSet};
    let state_dir = state_dir.to_owned();
    let update_dir = state_dir.clone();
    let projects = studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/host_secrets",
        move || {
            let mut projects = BTreeMap::new();
            let directory = state_dir.join("layr-projects");
            let Ok(entries) = std::fs::read_dir(directory) else {
                return Ok(projects);
            };
            for entry in entries {
                let entry = entry.map_err(|_| {
                    studio_runtime_services::vault::VaultError::InvalidStoredRecord("project.json")
                })?;
                let Some(project_id) = entry.file_name().to_str().map(str::to_owned) else {
                    continue;
                };
                if !entry.path().join("project.json").is_file() {
                    continue;
                }
                if let Some(view) = studio_runtime_services::vault::view(&state_dir, &project_id)?
                    && let (Some(site), Some(prefix)) = (
                        view.get("site").and_then(Value::as_str),
                        view.get("prefix").and_then(Value::as_str),
                    )
                {
                    projects.insert(project_id, (site.to_owned(), prefix.to_owned()));
                }
            }
            Ok(projects)
        },
    )
    .await
    .map_err(|_| studio_runtime_services::vault::VaultError::DeskFailed)??;
    if projects.is_empty() {
        return Ok(());
    }
    let sites = projects
        .values()
        .map(|(site, _)| site.clone())
        .collect::<BTreeSet<_>>();
    let first_site = sites
        .first()
        .cloned()
        .ok_or(studio_runtime_services::vault::VaultError::DeskFailed)?;
    let mut discard_output = |_: bool, _: &str| {};
    let service_status = host_command
        .run_desk(
            &["service".to_owned(), "status".to_owned()],
            studio_runtime_services::vault::DESK_COMMAND_TIMEOUT,
            &mut discard_output,
        )
        .await?;
    let mut service = serde_json::from_str::<Value>(&service_status.stdout)
        .map_err(|_| studio_runtime_services::vault::VaultError::DeskFailed)?;
    let program = host_command.desk_program()?;
    let current_program = service
        .get("program")
        .and_then(Value::as_str)
        .map(std::path::PathBuf::from);
    let current = service.get("installed").and_then(Value::as_bool) == Some(true)
        && service.get("site").and_then(Value::as_str) == Some(first_site.trim_end_matches('/'))
        && current_program.is_some_and(|path| {
            path.canonicalize().unwrap_or(path) == program.canonicalize().unwrap_or(program.clone())
        });
    if host_command.is_bundled_desk()? && !current {
        let result = host_command
            .run_desk(
                &[
                    "service".to_owned(),
                    "install".to_owned(),
                    "--site".to_owned(),
                    first_site.trim_end_matches('/').to_owned(),
                ],
                studio_runtime_services::vault::DESK_COMMAND_TIMEOUT,
                &mut discard_output,
            )
            .await?;
        if result.code != Some(0) {
            return Err(studio_runtime_services::vault::VaultError::DeskFailed);
        }
        service = serde_json::from_str::<Value>(
            &host_command
                .run_desk(
                    &["service".to_owned(), "status".to_owned()],
                    studio_runtime_services::vault::DESK_COMMAND_TIMEOUT,
                    &mut discard_output,
                )
                .await?
                .stdout,
        )
        .map_err(|_| studio_runtime_services::vault::VaultError::DeskFailed)?;
    }
    let disk = service
        .get("at")
        .and_then(Value::as_str)
        .filter(|_| {
            service.get("installed").and_then(Value::as_bool) == Some(true)
                && service.get("site").and_then(Value::as_str)
                    == Some(first_site.trim_end_matches('/'))
        })
        .map(str::to_owned);
    let mut unlocked_by_site = BTreeMap::new();
    for site in &sites {
        let result = host_command
            .run_desk(
                &[
                    "status".to_owned(),
                    "--site".to_owned(),
                    site.clone(),
                    "--json".to_owned(),
                ],
                studio_runtime_services::vault::DESK_COMMAND_TIMEOUT,
                &mut discard_output,
            )
            .await?;
        if result.code != Some(0) {
            return Err(studio_runtime_services::vault::VaultError::DeskFailed);
        }
        let status = serde_json::from_str::<Value>(&result.stdout)
            .map_err(|_| studio_runtime_services::vault::VaultError::DeskFailed)?;
        let unlocked = status
            .get("projects")
            .and_then(Value::as_array)
            .ok_or(studio_runtime_services::vault::VaultError::DeskFailed)?
            .iter()
            .filter_map(|project| {
                Some((
                    project.get("name")?.as_str()?.to_owned(),
                    project.get("unlocked")?.as_bool()?,
                ))
            })
            .collect::<BTreeMap<_, _>>();
        unlocked_by_site.insert(site.clone(), unlocked);
    }
    studio_runtime_process::blocking_pool::spawn_blocking(
        "runtime-http/routes/host_secrets",
        move || {
            for (project_id, (site, prefix)) in projects {
                let unlocked = unlocked_by_site
                    .get(&site)
                    .and_then(|projects| projects.get(&prefix))
                    .copied()
                    .unwrap_or(false);
                let project_disk = (site == first_site)
                    .then(|| {
                        disk.as_ref().map(|path| {
                            std::path::PathBuf::from(path)
                                .join(&prefix)
                                .to_string_lossy()
                                .into_owned()
                        })
                    })
                    .flatten();
                studio_runtime_services::vault::store_mac_state(
                    &update_dir,
                    &project_id,
                    unlocked,
                    project_disk.as_deref(),
                )?;
            }
            Ok::<(), studio_runtime_services::vault::VaultError>(())
        },
    )
    .await
    .map_err(|_| studio_runtime_services::vault::VaultError::DeskFailed)??;
    Ok(())
}

fn sanitize_unlock_reason(reason: &str) -> String {
    let mut result = String::new();
    let mut quoted = false;
    for character in reason.chars() {
        if character == '"' {
            if !quoted {
                result.push_str("one word");
            }
            quoted = !quoted;
        } else if !quoted {
            result.push(character);
        }
        if result.len() >= 300 {
            break;
        }
    }
    let result = result
        .trim()
        .strip_prefix("Error: ")
        .unwrap_or(result.trim())
        .to_owned();
    result.chars().take(300).collect()
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "host_secrets",
        routes: Some(routes),
        dependencies: &["linux-vm/guest-rpc"],
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::{
        GuestRpc, INVALID_LOGIN_JSON, OVERSIZED_PROGRESS_LINE, SetupJob, VAULT_JOB_CAPACITY,
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
        state = state.with_vm_state_dir(state_dir.join("linux-vm"));
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
        studio_runtime_services::vault::VaultHostCommand::new(std::path::PathBuf::from(
            "/tmp/server",
        ))
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

    fn setup_refresh_fake_script(refresh_succeeds: bool) -> String {
        let service_status = if refresh_succeeds {
            "printf '{\"installed\":true,\"site\":\"https://desk.example\",\"at\":\"/tmp/disk\",\"program\":\"%s\"}\\n' \"$0\""
        } else {
            "exit 92"
        };
        format!(
            "if [ \"$1\" = deploy ]; then\n  printf '%s\\n' '{{\"site\":\"https://desk.example/\"}}'\nelif [ \"$1\" = service ] && [ \"$2\" = status ]; then\n  {service_status}\nelif [ \"$1\" = status ] && [ \"$2\" = --site ]; then\n  printf '%s\\n' '{{\"projects\":[{{\"name\":\"project-one\",\"unlocked\":true}}]}}'\nelse\n  exit 94\nfi"
        )
    }

    fn vault_project(state: &std::path::Path) {
        let project = state.join("linux-vm/layr-projects/p1");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("project.json"),
            br#"{"projectId":"p1","source":"/tmp/Project One"}"#,
        )
        .unwrap();
        std::fs::write(
            project.join("vault.json"),
            br#"{"configured":true,"prefix":"project-one"}"#,
        )
        .unwrap();
        std::fs::write(
            project.join("vault-site.json"),
            br#"{"site":"https://desk.example/"}"#,
        )
        .unwrap();
    }

    struct SetupRecipientFixture(std::path::PathBuf);

    impl SetupRecipientFixture {
        fn create(prefix: &str) -> Self {
            let settings = super::settings_dir("unused");
            let directory = settings.join(prefix);
            std::fs::create_dir_all(&directory).unwrap();
            let recipient = directory.join("recipient.txt");
            std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(recipient)
                .unwrap();
            Self(directory)
        }
    }

    impl Drop for SetupRecipientFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn valid_vault_setup_request_completes_over_http_with_fake_desk() {
        let state = tempfile::tempdir().unwrap();
        let prefix = test_request_id("fixture");
        let _recipient = SetupRecipientFixture::create(&prefix);
        let source = state.path().join("source");
        std::fs::create_dir_all(&source).unwrap();
        let project = state.path().join("linux-vm/layr-projects/p1");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(
            project.join("project.json"),
            serde_json::to_vec(&json!({
                "projectId":"p1",
                "source":source.to_string_lossy()
            }))
            .unwrap(),
        )
        .unwrap();
        let fake_script = format!(
            "[ \"$1\" = deploy ] && [ \"$2\" = --project ] && [ \"$3\" = '{prefix}' ] && [ \"$4\" = --profile ] && [ \"$5\" = fixture ] && [ \"$6\" = --region ] && [ \"$7\" = eu-central-1 ] || exit 91\nprintf '%s\\n' '{{\"site\":\"https://desk.example/\"}}'"
        );
        let host_command = fake_host_command(state.path(), &fake_script);
        let (owner, client) = test_job_client().await;
        let mut state_http = HttpState::new(
            "test-token".to_owned(),
            "test-workspace".to_owned(),
            None,
            state.path().to_string_lossy().into_owned(),
            TEST_HTTP_PORT,
            None,
            None,
            None,
        );
        state_http = state_http.with_vm_state_dir(state.path().join("linux-vm"));
        state_http.vault_host_command = Some(host_command);
        state_http.vault_job_client = Some(client.clone());
        let app = crate::router::router(state_http).expect("runtime HTTP router starts");
        let request_id = test_request_id("setup");
        let response = app
            .oneshot(authorized_request_at(
                "/api/vault/setup",
                json!({
                    "projectId":"p1",
                    "requestId":request_id,
                    "prefix":prefix,
                    "profile":"fixture",
                    "region":"eu-central-1"
                }),
            ))
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let initial: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(initial["requestId"], request_id);
        assert_eq!(initial["state"], "running");
        assert_eq!(initial["prefix"], prefix);

        let job = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(job) = find_job(&client, VaultJobKind::Setup, &request_id)
                    .await
                    .unwrap()
                    && job["state"] != "running"
                {
                    break job;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake setup job completes");
        assert_eq!(job["state"], "done");
        assert_eq!(job["site"], "https://desk.example/");
        owner.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vault_unlock_passes_words_only_on_stdin_and_records_success() {
        let state = tempfile::tempdir().unwrap();
        vault_project(state.path());
        let host_command = fake_host_command(
            state.path(),
            "if [ \"$1\" = unlock ]; then\n  [ \"$2\" = --site ] && [ \"$3\" = https://desk.example/ ] || exit 91\n  [ \"$4\" = --project ] && [ \"$5\" = project-one ] && [ \"$6\" = --paper-file ] && [ \"$7\" = - ] || exit 92\n  [ \"$(cat)\" = 'fixture paper words' ] || exit 93\nelif [ \"$1\" = service ] && [ \"$2\" = status ]; then\n  printf '%s\\n' '{\"installed\":true,\"site\":\"https://desk.example\",\"at\":\"/tmp\"}'\nelif [ \"$1\" = status ] && [ \"$2\" = --site ] && [ \"$3\" = https://desk.example/ ] && [ \"$4\" = --json ]; then\n  printf '%s\\n' '{\"projects\":[{\"name\":\"project-one\",\"unlocked\":true}]}'\nelse\n  exit 94\nfi",
        );
        let app = app(state.path(), None, Some(host_command));
        let response = app
            .oneshot(authorized_request_at(
                "/api/vault/unlock",
                json!({"projectId":"p1","words":"fixture paper words"}),
            ))
            .await
            .unwrap();
        let status = response.status();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(
            serde_json::from_slice::<Value>(&body).unwrap(),
            json!({"unlocked":true})
        );
        assert!(
            !body
                .windows(b"fixture paper words".len())
                .any(|window| window == b"fixture paper words")
        );
        let mac: Value = serde_json::from_slice(
            &std::fs::read(
                state
                    .path()
                    .join("linux-vm/layr-projects/p1/vault-mac.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(mac, json!({"unlocked":true,"disk":"/tmp/project-one"}));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_vault_setup_refreshes_mac_view_and_wakes_sync() {
        let state = tempfile::tempdir().unwrap();
        vault_project(state.path());
        let host_command = fake_host_command(state.path(), &setup_refresh_fake_script(true));
        let (owner, client) = test_job_client().await;
        let request_id = test_request_id("setup-refresh");
        reserve_job(
            &client,
            VaultJobKind::Setup,
            &request_id,
            json!({"requestId":request_id,"state":"running"}),
        )
        .await
        .unwrap();
        let changes =
            Arc::new(studio_runtime_services::change_notifications::ChangeNotificationHub::new());
        let mut receiver = changes.subscribe();

        run_setup_job(SetupJob {
            request_id: request_id.clone(),
            prefix: "project-one".to_owned(),
            profile: "fixture".to_owned(),
            region: "eu-central-1".to_owned(),
            writer: "writer".to_owned(),
            host_command,
            vm_state_dir: state.path().join("linux-vm"),
            sync_changes: changes,
            job_client: client.clone(),
        })
        .await;

        let job = find_job(&client, VaultJobKind::Setup, &request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job["state"], "done");
        assert_eq!(job["site"], "https://desk.example/");
        let mac: Value = serde_json::from_slice(
            &std::fs::read(
                state
                    .path()
                    .join("linux-vm/layr-projects/p1/vault-mac.json"),
            )
            .unwrap(),
        )
        .unwrap();
        assert_eq!(mac, json!({"unlocked":true,"disk":"/tmp/disk/project-one"}));
        assert_eq!(receiver.borrow_and_update().revision, 1);
        owner.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn successful_vault_setup_stays_done_when_best_effort_refresh_fails() {
        let state = tempfile::tempdir().unwrap();
        vault_project(state.path());
        let host_command = fake_host_command(state.path(), &setup_refresh_fake_script(false));
        let (owner, client) = test_job_client().await;
        let request_id = test_request_id("setup-refresh-failed");
        reserve_job(
            &client,
            VaultJobKind::Setup,
            &request_id,
            json!({"requestId":request_id,"state":"running"}),
        )
        .await
        .unwrap();
        let changes =
            Arc::new(studio_runtime_services::change_notifications::ChangeNotificationHub::new());
        let mut receiver = changes.subscribe();

        run_setup_job(SetupJob {
            request_id: request_id.clone(),
            prefix: "project-one".to_owned(),
            profile: "fixture".to_owned(),
            region: "eu-central-1".to_owned(),
            writer: "writer".to_owned(),
            host_command,
            vm_state_dir: state.path().join("linux-vm"),
            sync_changes: changes,
            job_client: client.clone(),
        })
        .await;

        let job = find_job(&client, VaultJobKind::Setup, &request_id)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(job["state"], "done");
        assert_eq!(job["site"], "https://desk.example/");
        assert_eq!(receiver.borrow_and_update().revision, 1);
        owner.shutdown().await.unwrap();
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn vault_pull_runs_fake_layr_desk_and_replays_the_same_job() {
        let state = tempfile::tempdir().unwrap();
        vault_project(state.path());
        let host_command = fake_host_command(
            state.path(),
            "[ \"$1\" = pull ] && [ \"$2\" = --site ] && [ \"$3\" = https://desk.example/ ] && [ \"$4\" = --project ] && [ \"$5\" = project-one ] && [ \"$6\" = --to ] && [ \"$8\" = --json ] || exit 91\nprintf '%s\\n' '{\"seq\":7,\"files\":3,\"bytes\":1024}'",
        );
        let owner = VaultJobOwner::start();
        let client = owner.client();
        let mut state_http = HttpState::new(
            "test-token".to_owned(),
            "test-workspace".to_owned(),
            None,
            state.path().to_string_lossy().into_owned(),
            TEST_HTTP_PORT,
            None,
            None,
            None,
        );
        state_http = state_http.with_vm_state_dir(state.path().join("linux-vm"));
        state_http.vault_host_command = Some(host_command);
        state_http.vault_job_client = Some(client.clone());
        let app = crate::router::router(state_http).expect("runtime HTTP router starts");
        let request = json!({"projectId":"p1","requestId":"pull-fixture","to":state.path().to_string_lossy()});
        let response = app
            .oneshot(authorized_request_at("/api/vault/pull", request.clone()))
            .await
            .unwrap();
        let body = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let initial: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(initial["state"], "running");
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                if let Some(job) = find_job(&client, VaultJobKind::Pull, "pull-fixture")
                    .await
                    .unwrap()
                    && job["state"] != "running"
                {
                    break job;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("fake pull job completes");
        assert_eq!(result["state"], "done");
        assert_eq!(result["seq"], 7);
        let replay_app = crate::router::router({
            let mut state_http = HttpState::new(
                "test-token".to_owned(),
                "test-workspace".to_owned(),
                None,
                state.path().to_string_lossy().into_owned(),
                TEST_HTTP_PORT,
                None,
                None,
                None,
            );
            state_http = state_http.with_vm_state_dir(state.path().join("linux-vm"));
            state_http.vault_job_client = Some(client.clone());
            state_http
        })
        .expect("replay router starts");
        let replay = replay_app
            .oneshot(authorized_request_at("/api/vault/pull", request))
            .await
            .unwrap();
        let replay_body = axum::body::to_bytes(replay.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<Value>(&replay_body).unwrap(),
            result
        );
        owner.shutdown().await.unwrap();
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
        run_setup_job(SetupJob {
            request_id: setup_id.clone(),
            prefix: "project".to_owned(),
            profile: "default".to_owned(),
            region: "eu-central-1".to_owned(),
            writer: "writer".to_owned(),
            host_command: fake_host_command(root.path(), "printf '%16385s' '' >&2"),
            vm_state_dir: root.path().join("linux-vm"),
            sync_changes: Arc::new(
                studio_runtime_services::change_notifications::ChangeNotificationHub::new(),
            ),
            job_client: client.clone(),
        })
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
