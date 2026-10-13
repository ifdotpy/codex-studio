//! Pure projections used by the skills and capabilities discovery endpoints.

use crate::provider_session_task::{RequestSpec, SessionHandle, SessionOutcome, WriteOutcome};
use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use studio_runtime_process::{OperationId, provider_seam::Shutdown};
use studio_runtime_providers::{Method, RequestId, RequestOutcome, ResponseBody};
use tokio::time::Instant;

const CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_CACHE_ENTRIES: usize = 256;
const CLIENT_INFO: &str = "codex_studio_catalog_reads";
const MAX_TERMINAL_PAGES: usize = 100;
const TERMINAL_PAGE_SIZE: u64 = 100;

#[derive(Clone, Debug, PartialEq)]
pub struct ProviderRequest {
    pub method: String,
    pub params: Value,
}

/// Return the full native tool inventory carried by the selected preparation catalog.
pub fn managed_tool_catalog(
    catalog: &crate::thread_prepare::CatalogSnapshot,
) -> Result<Vec<Value>, &'static str> {
    crate::thread_prepare::full_tool_catalog(catalog)
        .into_iter()
        .map(|tool| serde_json::to_value(tool).map_err(|_| "Invalid tool catalog"))
        .collect()
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CatalogReadError {
    ProviderExchangeFailed,
}

#[derive(Clone, Debug, PartialEq)]
pub struct CapabilityAgentSnapshot {
    model: String,
    effort: Value,
    role: String,
}

impl CapabilityAgentSnapshot {
    pub fn from_record(agent: &Value) -> Result<Self, &'static str> {
        let model = agent
            .get("model")
            .and_then(Value::as_str)
            .ok_or("Invalid stored agent")?
            .to_owned();
        let role = agent
            .get("role")
            .and_then(Value::as_str)
            .filter(|role| matches!(*role, "orchestrator" | "implementer" | "reviewer"))
            .ok_or("Invalid stored agent")?
            .to_owned();
        Ok(Self {
            model,
            effort: agent.get("effort").cloned().unwrap_or(Value::Null),
            role,
        })
    }
}

/// Shared bounded one-off exchange for provider calls made outside an agent turn.
pub async fn provider_exchange(
    executable: Option<&Path>,
    home: &str,
    isolated: bool,
    requests: Vec<ProviderRequest>,
    timeout: Duration,
) -> Result<Vec<Result<Value, CatalogReadError>>, CatalogReadError> {
    let responses = crate::provider_exchange::run(
        executable,
        home,
        isolated,
        crate::provider_exchange::ExchangeSpec {
            timeout,
            shutdown: Shutdown::default(),
            initialize_operation: OperationId("catalog-reads-initialize".into()),
            initialize_params: json!({
                "clientInfo": {"name": CLIENT_INFO, "version": "1.0.0"},
                "capabilities": {"experimentalApi": true}
            }),
        },
        move |session, started| Box::pin(read_requests(session, started, requests, timeout)),
    )
    .await
    .map_err(|_| CatalogReadError::ProviderExchangeFailed)?;
    Ok(responses
        .into_iter()
        .map(|response| response.map_err(|_| CatalogReadError::ProviderExchangeFailed))
        .collect())
}

/// Cancel only after the provider confirms the exact terminal item and process ID.
pub async fn cancel_terminal(
    executable: Option<&Path>,
    home: &str,
    isolated: bool,
    thread_id: Option<String>,
    item_id: &str,
    process_id: &str,
    timeout: Duration,
) -> Result<Option<Value>, CatalogReadError> {
    let item_id = item_id.to_owned();
    let process_id = process_id.to_owned();
    crate::provider_exchange::run(
        executable,
        home,
        isolated,
        crate::provider_exchange::ExchangeSpec {
            timeout,
            shutdown: Shutdown::default(),
            initialize_operation: OperationId("native-command-cancel-initialize".into()),
            initialize_params: json!({
                "clientInfo": {"name": CLIENT_INFO, "version": "1.0.0"},
                "capabilities": {"experimentalApi": true}
            }),
        },
        move |session, started| {
            Box::pin(cancel_terminal_in_session(
                session, started, thread_id, item_id, process_id, timeout,
            ))
        },
    )
    .await
    .map_err(|_| CatalogReadError::ProviderExchangeFailed)
}

async fn cancel_terminal_in_session(
    session: &mut SessionHandle,
    started: Instant,
    thread_id: Option<String>,
    item_id: String,
    process_id: String,
    timeout: Duration,
) -> Result<Option<Value>, ()> {
    await_ready(session).await?;
    let deadline = started + timeout;
    let mut params = json!({"threadId":thread_id,"limit":TERMINAL_PAGE_SIZE});
    let mut seen = HashSet::new();
    let mut found = false;
    for index in 0..MAX_TERMINAL_PAGES {
        let response = send_request(
            session,
            "thread/backgroundTerminals/list",
            params.clone(),
            index,
            deadline,
        )
        .await?;
        let entries = response.get("data").and_then(Value::as_array).ok_or(())?;
        if entries.iter().any(|entry| {
            entry.get("itemId").and_then(Value::as_str) == Some(item_id.as_str())
                && entry.get("processId").and_then(Value::as_str) == Some(process_id.as_str())
        }) {
            found = true;
            break;
        }
        let Some(cursor) = response
            .get("nextCursor")
            .and_then(Value::as_str)
            .filter(|cursor| !cursor.is_empty())
        else {
            break;
        };
        if !seen.insert(cursor.to_owned()) || index + 1 == MAX_TERMINAL_PAGES {
            return Err(());
        }
        params["cursor"] = Value::String(cursor.to_owned());
    }
    if !found {
        return Ok(None);
    }
    let result = send_request(
        session,
        "thread/backgroundTerminals/terminate",
        json!({"threadId":thread_id,"processId":process_id}),
        MAX_TERMINAL_PAGES,
        deadline,
    )
    .await?;
    Ok(Some(result))
}

async fn await_ready(session: &mut SessionHandle) -> Result<(), ()> {
    while session.status.borrow().initialization
        != studio_runtime_process::provider_seam::InitializationStatus::Ready
    {
        tokio::select! {
            status = session.status.changed() => status.map_err(|_| ())?,
            event = session.events.recv() => {
                let event = event.ok_or(())?;
                if let Some(journal) = event.journal {
                    session.control.settled(journal.cursor).await.map_err(|_| ())?;
                }
                if event.outcomes.iter().any(|outcome| matches!(outcome, SessionOutcome::GenerationClosed { .. })) {
                    return Err(());
                }
            }
        }
    }
    Ok(())
}

async fn send_request(
    session: &mut SessionHandle,
    method_name: &str,
    params: Value,
    index: usize,
    deadline: Instant,
) -> Result<Value, ()> {
    let method = Method::from_wire(method_name).ok_or(())?;
    let timeout_ms = u64::try_from(
        deadline
            .checked_duration_since(Instant::now())
            .ok_or(())?
            .as_millis(),
    )
    .unwrap_or(u64::MAX);
    let operation_id = OperationId(format!("native-command-cancel-{index}"));
    let prepared = session
        .control
        .prepare(RequestSpec {
            id: RequestId::String(format!("native-command-cancel-{index}")),
            operation_id: operation_id.clone(),
            method,
            params,
            thread_id: None,
            lightweight: true,
            timeout_ms,
        })
        .await
        .map_err(|_| ())?;
    let ticket = session.control.submit(prepared).await.map_err(|_| ())?;
    loop {
        let event = session.events.recv().await.ok_or(())?;
        if let Some(journal) = event.journal {
            session
                .control
                .settled(journal.cursor)
                .await
                .map_err(|_| ())?;
        }
        for outcome in event.outcomes {
            match outcome {
                SessionOutcome::Resolved {
                    ticket: received,
                    outcome:
                        RequestOutcome::Reply {
                            body: ResponseBody::Result(value),
                            late: false,
                        },
                } if received == ticket => return Ok(value),
                SessionOutcome::Resolved {
                    ticket: received, ..
                } if received == ticket => {
                    return Err(());
                }
                SessionOutcome::Write {
                    operation_id: written,
                    outcome: WriteOutcome::NotSent | WriteOutcome::Unknown,
                    ..
                } if written == operation_id => return Err(()),
                SessionOutcome::GenerationClosed { .. } => return Err(()),
                _ => {}
            }
        }
    }
}

async fn read_requests(
    session: &mut SessionHandle,
    started: Instant,
    requests: Vec<ProviderRequest>,
    timeout: Duration,
) -> Result<Vec<Result<Value, ()>>, ()> {
    await_ready(session).await?;
    let deadline = started + timeout;
    let mut tickets = Vec::with_capacity(requests.len());
    let mut results = vec![None; requests.len()];
    for (index, request) in requests.into_iter().enumerate() {
        let Some(method) = Method::from_wire(&request.method) else {
            results[index] = Some(Err(()));
            tickets.push(None);
            continue;
        };
        let Some(remaining) = deadline.checked_duration_since(Instant::now()) else {
            results[index] = Some(Err(()));
            tickets.push(None);
            continue;
        };
        let timeout_ms = u64::try_from(remaining.as_millis()).unwrap_or(u64::MAX);
        let operation_id = OperationId(format!("catalog-reads-request-{index}"));
        let prepared = match session
            .control
            .prepare(RequestSpec {
                id: RequestId::String(format!("catalog-reads-{index}")),
                operation_id: operation_id.clone(),
                method,
                params: request.params,
                thread_id: None,
                lightweight: true,
                timeout_ms,
            })
            .await
        {
            Ok(prepared) => prepared,
            Err(_) => {
                results[index] = Some(Err(()));
                tickets.push(None);
                continue;
            }
        };
        let ticket = match session.control.submit(prepared).await {
            Ok(ticket) => ticket,
            Err(_) => {
                results[index] = Some(Err(()));
                tickets.push(None);
                continue;
            }
        };
        tickets.push(Some((ticket, operation_id)));
    }
    // Submit independent probes together, then settle by their correlation tickets. This
    // matches Python's shared five-second wait and preserves one probe's failure independently.
    while results.iter().any(Option::is_none) {
        if deadline <= Instant::now() {
            break;
        }
        let event = match tokio::time::timeout_at(deadline, session.events.recv()).await {
            Ok(Some(event)) => event,
            Ok(None) => return Err(()),
            Err(_) => break,
        };
        if let Some(journal) = event.journal {
            session
                .control
                .settled(journal.cursor)
                .await
                .map_err(|_| ())?;
        }
        for outcome in event.outcomes {
            match outcome {
                SessionOutcome::Resolved {
                    ticket: received,
                    outcome:
                        RequestOutcome::Reply {
                            body: ResponseBody::Result(value),
                            late: false,
                        },
                } => {
                    if let Some((index, _)) = tickets.iter().enumerate().find(|(_, ticket)| {
                        ticket
                            .as_ref()
                            .is_some_and(|(ticket, _)| *ticket == received)
                    }) {
                        results[index] = Some(Ok(value));
                    }
                }
                SessionOutcome::Resolved {
                    ticket: received, ..
                } => {
                    if let Some((index, _)) = tickets.iter().enumerate().find(|(_, ticket)| {
                        ticket
                            .as_ref()
                            .is_some_and(|(ticket, _)| *ticket == received)
                    }) {
                        results[index] = Some(Err(()));
                    }
                }
                SessionOutcome::Write {
                    operation_id: written,
                    outcome: WriteOutcome::NotSent | WriteOutcome::Unknown,
                    ..
                } => {
                    if let Some((index, _)) = tickets.iter().enumerate().find(|(_, ticket)| {
                        ticket
                            .as_ref()
                            .is_some_and(|(_, operation)| *operation == written)
                    }) {
                        results[index] = Some(Err(()));
                    }
                }
                SessionOutcome::GenerationClosed { .. } => return Err(()),
                _ => {}
            }
        }
    }
    Ok(results
        .into_iter()
        .map(|result| result.unwrap_or(Err(())))
        .collect())
}

/// Per-agent in-memory cache for capabilities discovery results.
#[derive(Default)]
pub struct CapabilitiesCache(Mutex<HashMap<String, std::sync::Arc<CapabilityEntry>>>);

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CapabilitiesCacheError {
    LockPoisoned,
    Capacity,
}

#[derive(Default)]
struct CapabilityEntry {
    value: tokio::sync::OnceCell<Value>,
    expires_at: Mutex<Option<Instant>>,
}

impl CapabilitiesCache {
    pub fn get(&self, agent: &str, now: Instant) -> Option<Value> {
        let cache = self.0.lock().ok()?;
        let entry = cache.get(agent)?;
        if entry
            .expires_at
            .lock()
            .ok()?
            .is_some_and(|expires| expires > now)
        {
            return entry.value.get().cloned();
        }
        None
    }

    /// Cache one consumer's discovery and share an in-flight miss with concurrent requests.
    pub async fn get_or_fetch<F, Fut>(
        &self,
        agent: &str,
        fetch: F,
    ) -> Result<Value, CapabilitiesCacheError>
    where
        F: FnOnce() -> Fut,
        Fut: Future<Output = Value>,
    {
        let now = Instant::now();
        let expires_at = now + CACHE_TTL;
        let entry = {
            let mut cache = self
                .0
                .lock()
                .map_err(|_| CapabilitiesCacheError::LockPoisoned)?;
            cache.retain(|_, entry| {
                entry
                    .expires_at
                    .lock()
                    .is_ok_and(|expires| expires.is_none_or(|expires| expires > now))
            });
            if let Some(entry) = cache.get(agent) {
                entry.clone()
            } else {
                if cache.len() >= MAX_CACHE_ENTRIES {
                    let oldest =
                        cache
                            .iter()
                            .filter_map(|(key, entry)| {
                                entry.expires_at.lock().ok().and_then(|expires| {
                                    expires.map(|expires| (key.clone(), expires))
                                })
                            })
                            .min_by_key(|(_, expires)| *expires)
                            .map(|(key, _)| key);
                    if let Some(oldest) = oldest {
                        cache.remove(&oldest);
                    } else {
                        return Err(CapabilitiesCacheError::Capacity);
                    }
                }
                let entry = std::sync::Arc::new(CapabilityEntry::default());
                cache.insert(agent.to_owned(), entry.clone());
                entry
            }
        };
        let value = entry.value.get_or_init(fetch).await.clone();
        let mut expiry = entry
            .expires_at
            .lock()
            .map_err(|_| CapabilitiesCacheError::LockPoisoned)?;
        if expiry.is_none() {
            *expiry = Some(expires_at);
        }
        Ok(value)
    }
}

/// Project a provider `skills/list` response onto the requested working directory.
pub fn skills_for_cwd(cwd: &str, response: &Value) -> Result<Value, &'static str> {
    let groups = response
        .get("data")
        .and_then(Value::as_array)
        .ok_or("The provider did not return a skill catalog")?;
    let mut skills = Vec::new();
    let mut errors = Vec::new();
    let mut names = HashSet::new();
    for group in groups {
        if group.get("cwd").and_then(Value::as_str) != Some(cwd) {
            continue;
        }
        if let Some(error_value) = group.get("errors") {
            let items = error_value
                .as_array()
                .ok_or("The provider returned an invalid skill catalog")?;
            errors.extend(items.iter().filter_map(|item| {
                let message = item
                    .get("message")
                    .and_then(Value::as_str)
                    .or_else(|| item.as_str());
                message
                    .filter(|message| !message.is_empty())
                    .map(str::to_owned)
            }));
        }
        let Some(skill_value) = group.get("skills") else {
            continue;
        };
        let items = skill_value
            .as_array()
            .ok_or("The provider returned an invalid skill catalog")?;
        for item in items {
            if item.get("enabled") == Some(&Value::Bool(false)) {
                continue;
            }
            let (Some(name), Some(path)) = (
                item.get("name")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty()),
                item.get("path")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty()),
            ) else {
                continue;
            };
            if !names.insert(name.to_owned()) {
                continue;
            }
            let description = item
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or("");
            skills.push(json!({"name": name, "path": path, "description": description}));
        }
    }
    skills.sort_by(|left, right| {
        left["name"]
            .as_str()
            .unwrap_or_default()
            .to_lowercase()
            .cmp(&right["name"].as_str().unwrap_or_default().to_lowercase())
    });
    Ok(json!({"skills": skills, "errors": errors}))
}

/// Host agents expose their persisted working directory. VM finder status is supplied by its
/// platform service and must not be inferred from the record alone.
pub fn host_finder_view(agent: &Value) -> Option<Value> {
    if agent.get("executionMode").and_then(Value::as_str) == Some("vm") {
        return None;
    }
    Some(json!({
        "kind": "native",
        "path": agent.get("cwd").cloned().unwrap_or(Value::Null),
    }))
}

/// Construct the stable capability fields before the independent provider probes run.
pub fn capabilities_base(
    agent_id: &str,
    agent: &CapabilityAgentSnapshot,
    managed: Vec<Value>,
    observed: impl IntoIterator<Item = String>,
    at: f64,
) -> Value {
    let mut observed = observed.into_iter().collect::<BTreeSet<_>>();
    let observed = std::mem::take(&mut observed)
        .into_iter()
        .collect::<Vec<_>>();
    json!({
        "agent": agent_id,
        "at": at,
        "managed": managed,
        "observed": observed,
        "skills": [],
        "servers": [],
        "errors": [],
        "model": agent.model,
        "effort": agent.effort,
        "role": agent.role,
        "nativeInventory": "Codex selects native tools per model and configuration. Observed calls appear below.",
        "observedNative": observed,
        "mcp": [],
        "skillsCursor": null,
        "serversCursor": null
    })
}

/// Apply one provider result using the Python response's `data`/`servers` fallback.
pub fn apply_capability_result(result: &mut Value, label: &str, response: &Value) {
    let data = response
        .get("data")
        .or_else(|| {
            (label == "servers")
                .then(|| response.get("servers"))
                .flatten()
        })
        .cloned()
        .unwrap_or_else(|| json!([]));
    let cursor = response.get("nextCursor").cloned().unwrap_or(Value::Null);
    let (field, cursor_field) = if label == "skills" {
        ("skills", "skillsCursor")
    } else {
        ("servers", "serversCursor")
    };
    result[field] = data.clone();
    result[cursor_field] = cursor;
    if field == "servers" {
        result["mcp"] = data;
    }
}

pub fn append_capability_error(result: &mut Value, label: &str, error: &str) {
    if let Some(errors) = result.get_mut("errors").and_then(Value::as_array_mut) {
        errors.push(Value::String(format!("{label}: {error}")));
    }
}

#[cfg(test)]
mod tests {
    use super::{
        CapabilitiesCache, CapabilityAgentSnapshot, ProviderRequest, apply_capability_result,
        cancel_terminal_in_session, capabilities_base, host_finder_view, read_requests,
        skills_for_cwd,
    };
    use serde_json::{Value, json};
    use std::{
        collections::BTreeMap,
        sync::{Arc, Mutex},
        time::Duration,
    };
    use studio_runtime_process::{
        EventCursor, Generation, OperationId, ProviderHandle, ProviderLaunchSpec,
        provider_seam::{
            AdapterCommand, AttachmentStatus, CommandReceipt, InitializationStatus, JournalEntry,
            JournalPayload, LANE_CAPACITY, OpenFuture, OpenOptions, ProviderAttachment,
            ProviderTaskFactory, Shutdown, TaskExit,
        },
    };
    use studio_runtime_providers::{Frame, RequestId, ResponseBody};
    use tokio::sync::{mpsc, watch};
    use tokio::time::Instant;

    struct Script {
        responses: Vec<serde_json::Value>,
        requests: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl ProviderTaskFactory for Script {
        fn open(
            &self,
            handle: ProviderHandle,
            options: OpenOptions,
            shutdown: Shutdown,
        ) -> OpenFuture<'_> {
            let responses = self.responses.clone();
            let requests = Arc::clone(&self.requests);
            Box::pin(async move {
                let (commands, mut inbox) = mpsc::channel(LANE_CAPACITY);
                let (entries, journal) = mpsc::channel(LANE_CAPACITY);
                let (state, status) = watch::channel(AttachmentStatus {
                    handle,
                    open_attempted: true,
                    info: None,
                    initialization: InitializationStatus::AwaitingReply,
                });
                let mut stop = shutdown.subscribe();
                let task = tokio::spawn(async move {
                    let serve = async {
                        let mut cursor = 0_u64;
                        let mut response_index = 0_usize;
                        while let Some(command) = inbox.recv().await {
                            match command {
                                AdapterCommand::Write {
                                    write,
                                    reply,
                                    observed,
                                } => {
                                    let message = write.message().clone();
                                    requests
                                        .lock()
                                        .expect("script request list")
                                        .push(message.clone());
                                    let initialize = message.get("method").and_then(Value::as_str)
                                        == Some("initialize");
                                    if initialize {
                                        reply
                                            .send(CommandReceipt::CachedInitialize(json!({})))
                                            .ok();
                                    } else {
                                        reply
                                            .send(CommandReceipt::Written {
                                                remote_id: None,
                                                duplicate: false,
                                            })
                                            .ok();
                                    }
                                    observed.await.ok();
                                    if !initialize && let Some(id) = message.get("id").cloned() {
                                        let Some(value) = responses.get(response_index) else {
                                            break;
                                        };
                                        cursor += 1;
                                        response_index += 1;
                                        entries
                                            .send(JournalEntry {
                                                cursor: EventCursor(cursor),
                                                generation: Generation(1),
                                                received_at_ms: options.clock.now_ms(),
                                                payload: JournalPayload::Frame(Frame::Response {
                                                    id: serde_json::from_value::<RequestId>(id)
                                                        .expect("request ID"),
                                                    body: ResponseBody::Result(value.clone()),
                                                    fields: Default::default(),
                                                }),
                                            })
                                            .await
                                            .ok();
                                    }
                                }
                                AdapterCommand::Ack { cursor, reply } => {
                                    reply.send(CommandReceipt::Acknowledged(cursor)).ok();
                                }
                            }
                        }
                    };
                    tokio::select! { _ = serve => {}, _ = stop.changed() => {} }
                    drop(state);
                    TaskExit {
                        result: Ok(()),
                        recovery_needed: None,
                    }
                });
                Ok(ProviderAttachment::from_task(
                    commands, journal, status, shutdown, task,
                ))
            })
        }
    }

    #[test]
    fn skills_match_cwd_filter_disabled_dedup_and_error_projection() {
        let response = json!({"data":[
            {"cwd":"/other","skills":[{"name":"ignored","path":"/ignored"}]},
            {"cwd":"/repo","errors":[{"message":"broken"},"warning",{"message":""}],"skills":[
                {"name":"Beta","path":"/b","description":17},
                {"name":"alpha","path":"/a"},
                {"name":"alpha","path":"/duplicate"},
                {"name":"off","path":"/off","enabled":false},
                {"name":"missing-path"},
                {"name":"","path":"/empty"}
            ]}
        ]});
        assert_eq!(
            skills_for_cwd("/repo", &response).expect("valid provider response"),
            json!({"skills":[
                {"name":"alpha","path":"/a","description":""},
                {"name":"Beta","path":"/b","description":""}
            ],"errors":["broken","warning"]})
        );
    }

    #[test]
    fn malformed_skill_catalog_is_reported_and_host_finder_view_is_record_only() {
        assert_eq!(
            skills_for_cwd("/repo", &json!({"data":null})),
            Err("The provider did not return a skill catalog")
        );
        assert_eq!(
            host_finder_view(&json!({"cwd":"/repo","executionMode":"native"})),
            Some(json!({"kind":"native","path":"/repo"}))
        );
        assert_eq!(host_finder_view(&json!({"executionMode":"vm"})), None);
        assert_eq!(
            skills_for_cwd(
                "/repo",
                &json!({"data":[{"cwd":"/repo","errors":null,"skills":[{"name":"hidden","path":"/hidden"}]}]})
            ),
            Err("The provider returned an invalid skill catalog")
        );
        assert_eq!(
            skills_for_cwd("/repo", &json!({"data":[{"cwd":"/repo","skills":null}]})),
            Err("The provider returned an invalid skill catalog")
        );
    }

    #[test]
    fn capabilities_projection_keeps_scoped_observations_and_provider_aliases() {
        let mut result = capabilities_base(
            "agent-a",
            &CapabilityAgentSnapshot::from_record(
                &json!({"model":"model-x","role":"implementer","effort":null}),
            )
            .expect("valid agent snapshot"),
            vec![json!({"name":"managed-tool"})],
            ["tool-z".into(), "tool-a".into(), "tool-z".into()],
            123.0,
        );
        apply_capability_result(
            &mut result,
            "servers",
            &json!({"servers":[{"name":"mcp"}],"nextCursor":"next"}),
        );
        assert_eq!(result["observed"], json!(["tool-a", "tool-z"]));
        assert_eq!(result["mcp"], json!([{"name":"mcp"}]));
        assert_eq!(result["serversCursor"], "next");
        assert_eq!(result["agent"], "agent-a");
    }

    #[test]
    fn malformed_capability_agent_is_rejected_by_the_snapshot_constructor() {
        assert!(
            CapabilityAgentSnapshot::from_record(&json!({"model":"m","role":"unknown"})).is_err()
        );
        assert!(CapabilityAgentSnapshot::from_record(&json!({"model":"m","role":"lead"})).is_err());
        assert!(
            CapabilityAgentSnapshot::from_record(
                &json!({"model":"m","role":"reviewer","frozenNativeParams":{"dynamicTools":{}}})
            )
            .is_ok()
        );
    }

    #[tokio::test(start_paused = true)]
    async fn discovery_cache_coalesces_per_consumer_and_expires_at_thirty_seconds() {
        let cache = std::sync::Arc::new(CapabilitiesCache::default());
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let mut requests = Vec::new();
        for _ in 0..16 {
            let cache = std::sync::Arc::clone(&cache);
            let calls = std::sync::Arc::clone(&calls);
            requests.push(tokio::spawn(async move {
                cache
                    .get_or_fetch("agent-a", || async move {
                        calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        tokio::task::yield_now().await;
                        json!({"servers":[]})
                    })
                    .await
                    .expect("coalesced discovery")
            }));
        }
        for request in requests {
            assert_eq!(request.await.expect("cache task"), json!({"servers":[]}));
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert_eq!(
            cache.get("agent-a", Instant::now()),
            Some(json!({"servers":[]}))
        );
        assert_eq!(cache.get("agent-b", Instant::now()), None);
        let other = cache
            .get_or_fetch("agent-b", || async { json!({"other":true}) })
            .await;
        assert_eq!(other, Ok(json!({"other":true})));
        tokio::time::advance(Duration::from_secs(30)).await;
        assert_eq!(cache.get("agent-a", Instant::now()), None);
        let refreshed = cache
            .get_or_fetch("agent-a", || async { json!({"refreshed":true}) })
            .await;
        assert_eq!(refreshed, Ok(json!({"refreshed":true})));
    }

    #[tokio::test(start_paused = true)]
    async fn oneoff_session_uses_scripted_skills_and_mcp_provider_replies() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(Script {
            responses: vec![
                json!({"data":[{"cwd":"/repo","skills":[]}]}),
                json!({"servers":[],"nextCursor":null}),
            ],
            requests: Arc::clone(&requests),
        });
        let launch = ProviderLaunchSpec::new(
            vec!["scripted-provider".into()],
            BTreeMap::new(),
            "/var/tmp".into(),
        );
        let started = Instant::now();
        let result = crate::provider_exchange::run_with_factory(
            factory,
            launch,
            started,
            crate::provider_exchange::ExchangeSpec {
                timeout: Duration::from_secs(5),
                shutdown: Shutdown::default(),
                initialize_operation: OperationId("scripted-init".into()),
                initialize_params: json!({"clientInfo":{"name":"scripted","version":"1"},"capabilities":{"experimentalApi":true}}),
            },
            |session, started| Box::pin(read_requests(session, started, vec![
                ProviderRequest { method:"skills/list".into(), params:json!({"cwds":["/repo"],"forceReload":true}) },
                ProviderRequest { method:"mcpServerStatus/list".into(), params:json!({"threadId":"thread","limit":100}) },
            ], Duration::from_secs(5))),
        ).await.expect("scripted replies complete");
        assert_eq!(
            result,
            vec![
                Ok(json!({"data":[{"cwd":"/repo","skills":[]}]})),
                Ok(json!({"servers":[],"nextCursor":null})),
            ]
        );
        let calls = requests.lock().expect("script request list");
        assert_eq!(calls.len(), 4);
        assert_eq!(calls[0]["method"], "initialize");
        assert_eq!(calls[2]["method"], "skills/list");
        assert_eq!(calls[2]["params"]["forceReload"], true);
        assert_eq!(calls[3]["method"], "mcpServerStatus/list");
        assert_eq!(calls[3]["params"]["threadId"], "thread");
    }

    #[tokio::test(start_paused = true)]
    async fn scripted_cancel_requires_the_exact_item_and_process_before_terminate() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(Script {
            responses: vec![
                json!({"data":[{"itemId":"item-a","processId":"44"}]}),
                json!({"terminated":true}),
            ],
            requests: Arc::clone(&requests),
        });
        let launch = ProviderLaunchSpec::new(
            vec!["scripted-provider".into()],
            BTreeMap::new(),
            "/var/tmp".into(),
        );
        let result = crate::provider_exchange::run_with_factory(
            factory,
            launch,
            Instant::now(),
            crate::provider_exchange::ExchangeSpec {
                timeout: Duration::from_secs(5),
                shutdown: Shutdown::default(),
                initialize_operation: OperationId("scripted-cancel-init".into()),
                initialize_params: json!({"clientInfo":{"name":"scripted","version":"1"},"capabilities":{"experimentalApi":true}}),
            },
            |session, started| Box::pin(cancel_terminal_in_session(
                session,
                started,
                Some("thread-a".into()),
                "item-a".into(),
                "44".into(),
                Duration::from_secs(5),
            )),
        )
        .await
        .expect("scripted cancel completes");
        assert_eq!(result, Some(json!({"terminated":true})));
        let calls = requests.lock().expect("script request list");
        assert_eq!(calls[2]["method"], "thread/backgroundTerminals/list");
        assert_eq!(calls[2]["params"]["threadId"], "thread-a");
        assert_eq!(calls[3]["method"], "thread/backgroundTerminals/terminate");
        assert_eq!(calls[3]["params"]["processId"], "44");
    }

    #[tokio::test(start_paused = true)]
    async fn scripted_cancel_does_not_terminate_a_different_process_for_the_same_item() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(Script {
            responses: vec![json!({"data":[{"itemId":"item-a","processId":"45"}]} )],
            requests: Arc::clone(&requests),
        });
        let launch = ProviderLaunchSpec::new(
            vec!["scripted-provider".into()],
            BTreeMap::new(),
            "/var/tmp".into(),
        );
        let result = crate::provider_exchange::run_with_factory(
            factory,
            launch,
            Instant::now(),
            crate::provider_exchange::ExchangeSpec {
                timeout: Duration::from_secs(5),
                shutdown: Shutdown::default(),
                initialize_operation: OperationId("scripted-cancel-mismatch-init".into()),
                initialize_params: json!({"clientInfo":{"name":"scripted","version":"1"},"capabilities":{"experimentalApi":true}}),
            },
            |session, started| Box::pin(cancel_terminal_in_session(
                session,
                started,
                Some("thread-a".into()),
                "item-a".into(),
                "44".into(),
                Duration::from_secs(5),
            )),
        )
        .await
        .expect("scripted refusal completes");
        assert_eq!(result, None);
        let calls = requests.lock().expect("script request list");
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2]["method"], "thread/backgroundTerminals/list");
    }

    #[tokio::test(start_paused = true)]
    async fn scripted_cancel_refuses_a_different_item_with_the_same_process_id() {
        let requests = Arc::new(Mutex::new(Vec::new()));
        let factory = Arc::new(Script {
            responses: vec![json!({"data":[{"itemId":"item-b","processId":"44"}]})],
            requests: Arc::clone(&requests),
        });
        let launch = ProviderLaunchSpec::new(
            vec!["scripted-provider".into()],
            BTreeMap::new(),
            "/var/tmp".into(),
        );
        let result = crate::provider_exchange::run_with_factory(
            factory, launch, Instant::now(),
            crate::provider_exchange::ExchangeSpec {
                timeout: Duration::from_secs(5),
                shutdown: Shutdown::default(),
                initialize_operation: OperationId("scripted-cancel-item-mismatch-init".into()),
                initialize_params: json!({"clientInfo":{"name":"scripted","version":"1"},"capabilities":{"experimentalApi":true}}),
            },
            |session, started| Box::pin(cancel_terminal_in_session(
                session, started, Some("thread-a".into()), "item-a".into(), "44".into(), Duration::from_secs(5),
            )),
        ).await.expect("scripted refusal completes");
        assert_eq!(result, None);
        let calls = requests.lock().expect("script request list");
        assert_eq!(calls.len(), 3);
        assert_eq!(calls[2]["method"], "thread/backgroundTerminals/list");
    }
}
