//! Pure projections used by the skills and capabilities discovery endpoints.

use serde_json::{Value, json};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::path::Path;
use std::sync::Mutex;
use std::time::Duration;
use studio_runtime_providers::Method;
use tokio::time::Instant;

const CACHE_TTL: Duration = Duration::from_secs(30);
const MAX_CACHE_ENTRIES: usize = 256;

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

/// Read discovery methods through the selected account's already retained
/// provider session. The account worker owns the durable intent and receipt;
/// this adapter only accepts a written reply bound to the captured connection.
pub async fn account_session_exchange(
    session: &dyn crate::account_session_runtime::AccountSessionPort,
    account: &str,
    requests: Vec<ProviderRequest>,
    timeout: Duration,
) -> Result<Vec<Result<Value, CatalogReadError>>, CatalogReadError> {
    use crate::account_session_runtime::{
        ProviderRequest as SessionRequest, ProviderResponse, RequestCertainty,
    };

    const OPERATION_PREFIX: &str = "catalog-discovery";
    static REQUEST_SEQUENCE: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    let scope = session
        .current_scope(account)
        .ok_or(CatalogReadError::ProviderExchangeFailed)?;
    let deadline = std::time::Instant::now() + timeout;
    let mut responses = Vec::with_capacity(requests.len());
    for request in requests {
        let sequence = REQUEST_SEQUENCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let operation = format!(
            "{OPERATION_PREFIX}:{}:{sequence}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_err(|_| CatalogReadError::ProviderExchangeFailed)?
                .as_nanos()
        );
        let method =
            Method::from_wire(&request.method).ok_or(CatalogReadError::ProviderExchangeFailed)?;
        let receipt = session
            .provider_request(SessionRequest {
                account: account.to_owned(),
                operation: operation.clone(),
                actor: None,
                expected_scope: Some(scope.clone()),
                deleted_actor_session: None,
                method,
                params: request.params,
                deadline,
            })
            .await
            .map_err(|_| CatalogReadError::ProviderExchangeFailed)?;
        if receipt.scope != scope
            || receipt.operation != operation
            || receipt.certainty != RequestCertainty::Written
            || receipt.ticket.as_ref().is_none_or(|ticket| {
                ticket.method != method || ticket.generation != scope.generation
            })
        {
            return Err(CatalogReadError::ProviderExchangeFailed);
        }
        match receipt.result {
            Some(ProviderResponse::Reply(response)) => responses.push(Ok(response)),
            Some(ProviderResponse::Rejected(_)) | None => {
                responses.push(Err(CatalogReadError::ProviderExchangeFailed));
            }
        }
    }
    Ok(responses)
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

/// Finder's view of a Linux VM chat, derived from the host's saved mount records.
/// This deliberately does not call the guest: `codex_vm_agents.finder_view` is
/// a read-only projection and reports mount state from host observations.
pub fn vm_finder_view(state_dir: &Path, agent: &Value) -> Value {
    let unmounted = json!({"kind":"vm","state":"unmounted","path":null,"error":null});
    let Some(project_id) = agent.get("layrProjectId").and_then(Value::as_str) else {
        return unmounted;
    };
    let projects = state_dir.join("layr-projects");
    let entries = match std::fs::read_dir(&projects) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return unmounted,
        Err(error) => return finder_status_error(error.to_string()),
    };
    let mut record = None;
    let mut failure = None;
    for entry in entries {
        let entry = match entry {
            Ok(entry) => entry,
            Err(error) => return finder_status_error(error.to_string()),
        };
        let directory = entry.path();
        match std::fs::read(directory.join("mount.json")) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) if value.get("projectId").and_then(Value::as_str) == Some(project_id) => {
                    record = Some(value);
                }
                Ok(_) => {}
                Err(error) => return finder_status_error(error.to_string()),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return finder_status_error(error.to_string()),
        }
        match std::fs::read(directory.join("mount-error.json")) {
            Ok(bytes) => match serde_json::from_slice::<Value>(&bytes) {
                Ok(value) if value.get("projectId").and_then(Value::as_str) == Some(project_id) => {
                    failure = Some(value);
                }
                Ok(_) => {}
                Err(error) => return finder_status_error(error.to_string()),
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return finder_status_error(error.to_string()),
        }
    }
    let Some(record) = record else {
        return failure.map_or(unmounted, |failure| {
            json!({"kind":"vm","state":"failed","path":null,
                "error":failure.get("error").cloned().unwrap_or(Value::Null)})
        });
    };
    let Some(path) = record.get("path").and_then(Value::as_str) else {
        return finder_status_error("The saved mount record has no path".to_owned());
    };
    let mount = Path::new(path);
    let mounted = mount_is_mountpoint(mount);
    let state = if mounted { "mounted" } else { "unmounted" };
    let error = if mounted {
        Value::Null
    } else {
        failure
            .as_ref()
            .and_then(|value| value.get("error").cloned())
            .unwrap_or(Value::Null)
    };
    json!({"kind":"vm","state":state,"path":if mounted { json!(path) } else { Value::Null },"error":error})
}

#[cfg(unix)]
fn mount_is_mountpoint(mount: &Path) -> bool {
    use std::os::unix::fs::MetadataExt;
    mount
        .metadata()
        .ok()
        .zip(mount.parent().and_then(|parent| parent.metadata().ok()))
        .is_some_and(|(mount, parent)| mount.dev() != parent.dev())
}

#[cfg(not(unix))]
fn mount_is_mountpoint(_mount: &Path) -> bool {
    false
}

fn finder_status_error(message: String) -> Value {
    json!({"kind":"vm","state":"failed","path":null,
        "error":format!("The Finder view status is unavailable: {}", message.chars().take(300).collect::<String>())})
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
        CapabilitiesCache, CapabilityAgentSnapshot, ProviderRequest, account_session_exchange,
        apply_capability_result, capabilities_base, host_finder_view, skills_for_cwd,
        vm_finder_view,
    };
    use serde_json::json;
    use std::{
        sync::{Arc, Mutex},
        time::Duration,
    };

    use tokio::time::Instant;

    struct RetainedDiscovery {
        calls: Arc<Mutex<Vec<serde_json::Value>>>,
    }

    impl crate::account_session_runtime::AccountSessionPort for RetainedDiscovery {
        fn current_scope(
            &self,
            account: &str,
        ) -> Option<crate::account_session_runtime::ConnectionScope> {
            Some(crate::account_session_runtime::ConnectionScope {
                account: account.to_owned(),
                connection: "fixture".to_owned(),
                generation: 7,
            })
        }

        fn provider_request<'a>(
            &'a self,
            request: crate::account_session_runtime::ProviderRequest,
        ) -> crate::account_session_runtime::AccountSessionFuture<
            'a,
            crate::account_session_runtime::SessionReceipt,
        > {
            Box::pin(async move {
                use crate::account_session_runtime::{
                    ProviderResponse, RequestCertainty, SessionReceipt,
                };
                let scope = request
                    .expected_scope
                    .clone()
                    .expect("captured fixture scope");
                assert!(request.deadline > std::time::Instant::now());
                let method = request.method;
                let operation = request.operation.clone();
                let response = if method == studio_runtime_providers::Method::SkillsList {
                    json!({"data":[{"cwd":"/repo","skills":[]}]})
                } else {
                    json!({"servers":[],"nextCursor":null})
                };
                self.calls.lock().expect("discovery calls").push(json!({
                    "account":request.account,
                    "operation":operation,
                    "method":method.as_str(),
                    "params":request.params,
                }));
                Ok(SessionReceipt {
                    scope,
                    operation,
                    ticket: Some(studio_runtime_providers::RequestTicket {
                        generation: 7,
                        id: studio_runtime_providers::RequestId::String("fixture".to_owned()),
                        method,
                    }),
                    certainty: RequestCertainty::Written,
                    result: Some(ProviderResponse::Reply(response)),
                    not_sent_reason: None,
                    replayed: false,
                })
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
    fn vm_finder_view_without_layr_project_is_unmounted_without_guest_rpc() {
        let state = tempfile::tempdir().expect("temporary state directory");
        assert_eq!(
            vm_finder_view(state.path(), &json!({"executionMode":"vm"})),
            json!({"kind":"vm","state":"unmounted","path":null,"error":null})
        );
    }

    #[test]
    fn vm_finder_view_reads_saved_mount_failure_without_guest_rpc() {
        let state = tempfile::tempdir().expect("temporary VM state directory");
        let project = state.path().join("layr-projects").join("project");
        std::fs::create_dir_all(&project).expect("project state directory");
        std::fs::write(
            project.join("mount-error.json"),
            json!({"projectId":"project","error":"share unavailable"}).to_string(),
        )
        .expect("saved mount failure");
        assert_eq!(
            vm_finder_view(
                state.path(),
                &json!({
                    "executionMode":"vm","layrProjectId":"project"
                })
            ),
            json!({"kind":"vm","state":"failed","path":null,"error":"share unavailable"})
        );
    }

    #[test]
    #[cfg(target_os = "linux")]
    fn vm_finder_view_projects_a_saved_mountpoint_without_guest_rpc() {
        let state = tempfile::tempdir().expect("temporary VM state directory");
        let project = state.path().join("layr-projects").join("project");
        std::fs::create_dir_all(&project).expect("project state directory");
        std::fs::write(
            project.join("mount.json"),
            json!({"projectId":"project","path":"/dev"}).to_string(),
        )
        .expect("saved mount record");
        assert_eq!(
            vm_finder_view(
                state.path(),
                &json!({
                    "executionMode":"vm","layrProjectId":"project"
                })
            ),
            json!({"kind":"vm","state":"mounted","path":"/dev","error":null})
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

    #[tokio::test]
    async fn account_session_discovery_uses_bound_written_read_receipts() {
        let calls = Arc::new(Mutex::new(Vec::new()));
        let session = RetainedDiscovery {
            calls: Arc::clone(&calls),
        };
        let responses = account_session_exchange(
            &session,
            "claude-account",
            vec![
                ProviderRequest {
                    method: "skills/list".to_owned(),
                    params: json!({"cwds":["/repo"],"forceReload":false}),
                },
                ProviderRequest {
                    method: "mcpServerStatus/list".to_owned(),
                    params: json!({"threadId":"thread-1","limit":100}),
                },
            ],
            Duration::from_secs(5),
        )
        .await
        .expect("written discovery replies");
        assert_eq!(
            responses,
            vec![
                Ok(json!({"data":[{"cwd":"/repo","skills":[]}]})),
                Ok(json!({"servers":[],"nextCursor":null})),
            ]
        );
        let calls = calls.lock().expect("discovery calls");
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0]["account"], "claude-account");
        assert_eq!(calls[0]["method"], "skills/list");
        assert_eq!(calls[1]["method"], "mcpServerStatus/list");
        assert_ne!(calls[0]["operation"], calls[1]["operation"]);
    }
}
