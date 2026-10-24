//! Checked projections of stored rows and immutable runtime preparation resources.
use super::*;
use crate::provider_adapter::{ProviderKind, codex_user_mcp_servers};
use crate::{model_catalog::ModelCatalogService, thread_prepare as thread, turn_prepare as turn};
use rusqlite::OptionalExtension;
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Map;
use std::path::{Path, PathBuf};
use std::{future::Future, pin::Pin};
use studio_runtime_process::ProviderLaunchSpec;
use studio_runtime_store::path_identity::CanonicalPath;
use studio_runtime_store::{AgentRecord, Transaction};

/// Skill files appended to the role guidance of a new layr (VM) session, in this order,
/// relative to `.agents/skills`. The Python runtime (`codex_runtime.VM_ROLE_GUIDANCE`) lists
/// the same files.
const VM_ROLE_GUIDANCE: [&str; 2] = ["codex-workspace/references/layr.md", "layr/SKILL.md"];
/// Where the guest keeps the Studio skills; a VM session's guidance names these paths. The
/// Python runtime (`codex_runtime.VM_GUEST_SKILLS_ROOT`) uses the same folder.
const VM_GUEST_SKILLS_ROOT: &str = "/opt/codex-studio/.agents/skills";

/// Actor-specific inputs needed to launch a provider inside a Linux VM.
/// Credentials are deliberately absent: the resolver reads and copies them
/// through its private guest boundary and never returns or logs their values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VmProviderLaunchRequest {
    pub agent_id: String,
    pub epoch: i64,
    pub generation: u64,
    pub account_key: String,
    pub provider: String,
    pub cwd: String,
    pub guest_home: String,
}

/// Provider launch resolved for one actor. `launch.environment` is the guest
/// environment only; callers must not merge in the host process environment.
pub struct VmProviderLaunch {
    pub provider: String,
    pub profile: String,
    pub launch: ProviderLaunchSpec,
    /// Host-refreshed login payload sent only through the retained guest session.
    /// It is transient and must never be persisted or logged.
    pub bootstrap: Option<Value>,
}

pub type VmProviderLaunchFuture<'a> =
    Pin<Box<dyn Future<Output = Result<VmProviderLaunch, WorkerError>> + Send + 'a>>;

/// Host-owned VM launch resolver. It selects the provider/profile and durably
/// copies the minimal provider credential material into the guest before the
/// caller starts the provider process.
pub trait VmProviderLaunchResolver: Send + Sync {
    fn resolve(&self, request: VmProviderLaunchRequest) -> VmProviderLaunchFuture<'_>;

    /// Host-owned Codex refresh for a token callback emitted by a guest actor.
    fn refresh_codex(
        &self,
        _account_key: String,
        _previous_account_id: String,
        _actor_id: String,
        _actor_epoch: i64,
        _generation: u64,
    ) -> VmProviderAuthFuture<'_> {
        Box::pin(async {
            Err(WorkerError::PreparationPending(
                crate::account_worker::PreparationBlock::new(
                    studio_runtime_schema::agent_progress::StartWaitReason::ProviderSession,
                    "provider-session/vm-token-refresh",
                ),
            ))
        })
    }

    /// Refresh the shared host Claude profile once, then copy it to every
    /// currently attached actor in the account worker's fenced sweep.
    fn refresh_claude_credentials(
        &self,
        _actors: Vec<VmProviderLaunchRequest>,
    ) -> VmProviderRefreshFuture<'_> {
        Box::pin(async { Ok(()) })
    }
}

pub type VmProviderAuthFuture<'a> =
    Pin<Box<dyn Future<Output = Result<Value, WorkerError>> + Send + 'a>>;

pub type VmProviderRefreshFuture<'a> =
    Pin<Box<dyn Future<Output = Result<(), WorkerError>> + Send + 'a>>;

#[derive(Clone)]
pub struct PreparationResources {
    pub instructions: String,
    pub thread_config: Map<String, Value>,
    pub tools: Vec<thread::ToolDefinition>,
    pub legacy_tools: Vec<thread::ToolDefinition>,
    pub skills_root: PathBuf,
    pub role_guidance: BTreeMap<PathBuf, String>,
    pub model_catalog: ModelCatalogService,
    pub account_home: String,
    /// Filesystem observations captured before SQL callbacks; retained by commit guards.
    pub filesystem_inputs: Option<Arc<PreparationFilesystem>>,
    /// The shared (base) Codex home whose `node_repl` and Chrome plugin the
    /// browser setup copies; empty leaves host Codex threads without a browser.
    pub shared_home: String,
    pub claude_account_pin: Value,
    pub account_driver: Option<Arc<crate::account_driver::AccountDriver>>,
    /// The host's registry-backed session surface for preparation calls that
    /// need to coordinate another live account.
    pub account_session: Option<Arc<dyn crate::account_session_runtime::AccountSessionPort>>,
    /// Fakeable guest RPC for VM workspace preparation in the turn-start path.
    pub guest_rpc: Option<Arc<dyn crate::linux_vm_guest::GuestRpc>>,
    /// Host-owned per-actor VM provider launch and credential-copy boundary.
    pub vm_provider_launch: Option<Arc<dyn VmProviderLaunchResolver>>,
}
/// Account guidance and receipt path evidence captured outside SQLite callbacks.
#[derive(Clone, Default)]
pub struct PreparationFilesystem {
    browser: Option<crate::browser::AccountBrowser>,
    mcp_servers: Vec<String>,
    receipt_cwd: Option<(String, String, bool)>,
}

impl PreparationResources {
    pub fn bundled(
        account_home: String,
        model_catalog: ModelCatalogService,
    ) -> Result<Self, WorkerError> {
        let root =
            CanonicalPath::existing(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../.."))
                .map_err(|_| invalid("repository root"))?;
        Self::bundled_at(account_home, model_catalog, root.to_path_buf())
    }

    pub fn bundled_at(
        account_home: String,
        model_catalog: ModelCatalogService,
        root: PathBuf,
    ) -> Result<Self, WorkerError> {
        #[derive(Deserialize)]
        struct Bundled {
            instructions: String,
            thread_config: Map<String, Value>,
            tools: Vec<thread::ToolDefinition>,
        }
        let mut data: Bundled = decode(
            serde_json::from_str(include_str!("../preparation-resources.json"))
                .map_err(|_| invalid("preparation resources"))?,
            "preparation resources",
        )?;
        let mut legacy_tools: Vec<thread::ToolDefinition> = decode(
            serde_json::from_str(include_str!(
                "../../../../../../../.agents/skills/codex-workspace/references/native-tools.json"
            ))
            .map_err(|_| invalid("legacy tools"))?,
            "legacy tools",
        )?;
        for tool in data.tools.iter_mut().chain(legacy_tools.iter_mut()) {
            studio_runtime_schema::task_results::apply_tool_contract(
                &tool.name,
                &mut tool.input_schema,
            );
            studio_runtime_schema::task_gate::apply_tool_contract(
                &tool.name,
                &mut tool.input_schema,
            );
            if tool.name == "orchestration_task" {
                tool.description.push_str(&format!(" gate on create/update runs a managed acceptance check at the submitted Git commit. set_default_gate sets the lead default for new tasks. Gate timeout defaults to {} ms. Layr and non-Git submissions report not_run and require override_reason. Failed or unknown gates refuse acceptance unless the lead supplies override_reason. Submit again to rerun a gate.", studio_runtime_schema::task_gate::DEFAULT_TIMEOUT_MS));
            }
            if tool.name == "orchestration_spawn" {
                let properties = tool
                    .input_schema
                    .get_mut("properties")
                    .and_then(|value| value.pointer_mut("/agents/items/properties"))
                    .and_then(Value::as_object_mut)
                    .ok_or_else(|| invalid("spawn properties"))?;
                properties.insert("permission_mode".into(), json!({
                    "type": "string",
                    "enum": crate::claude_controls::PERMISSION_MODES,
                    "description": "Claude child permission mode. Defaults to the parent's mode. Bypass requires a bypass parent. Reviewers always use plan."
                }));
            }
        }
        let skills_root = root.join(".agents/skills");
        let mut role_guidance = BTreeMap::new();
        for relative in [
            "codex-orchestrator/SKILL.md",
            "codex-subagent/SKILL.md",
            "codex-orchestrator/references/native-role.md",
            "codex-orchestrator/references/legacy-move-role.md",
            "codex-subagent/references/native-role.md",
        ] {
            role_guidance.insert(
                PathBuf::from(relative),
                std::fs::read_to_string(skills_root.join(relative))
                    .map_err(|_| refusal("thread-preparation/missing-guidance".into()))?,
            );
        }
        // Only VM sessions use these; preparing one without them is refused below.
        for relative in VM_ROLE_GUIDANCE {
            match std::fs::read_to_string(skills_root.join(relative)) {
                Ok(content) => {
                    role_guidance.insert(PathBuf::from(relative), content);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(_) => return Err(refusal("thread-preparation/missing-guidance".into())),
            }
        }
        Ok(Self {
            role_guidance,
            instructions: data.instructions,
            thread_config: data.thread_config,
            tools: data.tools,
            legacy_tools,
            skills_root,
            model_catalog,
            account_home,
            filesystem_inputs: None,
            shared_home: String::new(),
            claude_account_pin: Value::Null,
            account_driver: None,
            account_session: None,
            guest_rpc: None,
            vm_provider_launch: None,
        })
    }

    fn capture_filesystem(mut self, receipt_cwd: Option<(String, String)>) -> Self {
        studio_runtime_store::debug_assert_outside_sql();
        let browser = (!self.account_home.is_empty() && !self.shared_home.is_empty()).then(|| {
            crate::browser::account_browser(
                Path::new(&self.account_home),
                Path::new(&self.shared_home),
            )
        });
        let mcp_servers = if self.account_home.is_empty() {
            Vec::new()
        } else {
            codex_user_mcp_servers(&self.account_home)
        };
        let receipt_cwd = receipt_cwd.map(|(stored, prepared)| {
            let same = studio_runtime_store::path_identity::same_text(&stored, &prepared);
            (stored, prepared, same)
        });
        self.filesystem_inputs = Some(Arc::new(PreparationFilesystem {
            browser,
            mcp_servers,
            receipt_cwd,
        }));
        self
    }

    pub(crate) async fn for_sql(&self) -> Result<Self, WorkerError> {
        let resources = self.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/thread-guidance/filesystem",
            move || resources.capture_filesystem(None),
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)
    }

    pub(crate) async fn for_receipt(
        &self,
        store: &RuntimeStore,
        agent_id: &str,
        prepared_cwd: Option<&str>,
    ) -> Result<Self, WorkerError> {
        let receipt_cwd = if let Some(prepared_cwd) = prepared_cwd {
            let agent_id = agent_id.to_owned();
            let stored = store
                .read_async("runtime-services/thread-receipt/cwd", move |tx| {
                    Ok::<_, WorkerError>(
                        AgentRepository::get(tx, &AgentId::from(agent_id.as_str()))?.and_then(
                            |agent| {
                                agent
                                    .extra
                                    .get("cwd")
                                    .and_then(Value::as_str)
                                    .map(str::to_owned)
                            },
                        ),
                    )
                })
                .await?;
            stored.map(|stored| (stored, prepared_cwd.to_owned()))
        } else {
            None
        };
        let resources = self.clone();
        studio_runtime_process::blocking_pool::spawn_blocking(
            "runtime-services/thread-receipt/filesystem",
            move || resources.capture_filesystem(receipt_cwd),
        )
        .await
        .map_err(|_| WorkerError::TaskFailed)
    }

    pub fn with_account_session(
        mut self,
        account_session: Option<Arc<dyn crate::account_session_runtime::AccountSessionPort>>,
    ) -> Self {
        self.account_session = account_session;
        self
    }
}
pub(super) fn decode<T: DeserializeOwned>(value: Value, location: &str) -> Result<T, WorkerError> {
    serde_json::from_value(value).map_err(|_| invalid(location))
}
pub(super) fn invalid(location: &str) -> WorkerError {
    StoreError::InvalidStoredObject {
        table: "turn_preparation",
        id: location.into(),
    }
    .into()
}
pub(super) fn agent(tx: &Transaction<'_>, id: &str) -> Result<AgentRecord, WorkerError> {
    AgentRepository::get(tx, &AgentId::from(id))?.ok_or_else(|| invalid(id))
}
fn table_exists(tx: &rusqlite::Connection, table: &str) -> Result<bool, WorkerError> {
    Ok(tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1 AND type='table')",
        [table],
        |r| r.get::<_, bool>(0),
    )?)
}
/// One `id,record` row of an optional feature table.
fn record_by_id(
    tx: &rusqlite::Connection,
    table: &'static str,
    id: &str,
) -> Result<Option<String>, WorkerError> {
    use rusqlite::OptionalExtension;
    if !table_exists(tx, table)? {
        return Ok(None);
    }
    Ok(tx
        .prepare_cached(&format!("SELECT record FROM {table} WHERE id=?1"))?
        .query_row([id], |r| r.get(0))
        .optional()?)
}
/// Complaints addressed to `lead`, in id order, through the
/// `runtime_conversion_complaints_leadId` index.
const LEAD_COMPLAINTS_SQL: &str =
    "SELECT record FROM runtime_complaints WHERE json_extract(record,'$.leadId')=?1 ORDER BY id";
fn lead_complaints(tx: &rusqlite::Connection, lead: &str) -> Result<Vec<String>, WorkerError> {
    if !table_exists(tx, "runtime_complaints")? {
        return Ok(Vec::new());
    }
    Ok(tx
        .prepare_cached(LEAD_COMPLAINTS_SQL)?
        .query_map([lead], |r| r.get(0))?
        .collect::<Result<_, _>>()?)
}
pub(super) fn records(
    tx: &rusqlite::Connection,
    table: &str,
) -> Result<BTreeMap<String, String>, WorkerError> {
    if !table_exists(tx, table)? {
        return Ok(BTreeMap::new());
    }
    let mut query = tx.prepare(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
    Ok(query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?)
}
/// Stored rows for exactly `ids`, keyed by id; absent ids are skipped.
pub(super) fn records_by_ids(
    db: &rusqlite::Connection,
    table: &str,
    ids: &std::collections::BTreeSet<String>,
) -> Result<BTreeMap<String, String>, WorkerError> {
    let mut rows = BTreeMap::new();
    if ids.is_empty()
        || !db.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1 AND type='table')",
            [table],
            |r| r.get::<_, bool>(0),
        )?
    {
        return Ok(rows);
    }
    let mut query = db.prepare_cached(&format!("SELECT record FROM {table} WHERE id=?1"))?;
    for id in ids {
        if let Some(raw) = query
            .query_row([id], |r| r.get::<_, String>(0))
            .optional()?
        {
            let raw = if table == studio_runtime_store::agent_volatile::AGENT_TABLE {
                studio_runtime_store::agent_volatile::merged_text(db, id, raw)?
            } else {
                raw
            };
            rows.insert(id.clone(), raw);
        }
    }
    Ok(rows)
}

/// The events authorization checks: radio turns are checked by the radio
/// service instead when the intent carries that authority.
fn authorization_events(intent: &TurnIntent) -> Vec<crate::submit_gates::StoredEvent> {
    intent
        .events
        .iter()
        .filter(|event| !(intent.radio_authorized && event.kind == "radio_turn"))
        .map(|e| crate::submit_gates::StoredEvent {
            id: e.id.clone(),
            kind: e.kind.clone(),
            text: e.text.clone(),
        })
        .collect()
}

/// The stored rows one authorization decision reads, without the resolved
/// paths: the recipient, senders, rooms, complaints and the follow-up
/// metadata the events name, plus the few projects. Whole agent, room,
/// complaint and event-metadata tables are never read.
fn authorization_records(
    db: &rusqlite::Connection,
    intent: &TurnIntent,
) -> Result<crate::submit_gates::AuthorizationRecords, WorkerError> {
    event_authorization_records(db, &intent.agent_id, authorization_events(intent))
}

/// [`authorization_records`] of `events` for their recipient `agent_id`.
pub(in crate::account_worker) fn event_authorization_records(
    db: &rusqlite::Connection,
    agent_id: &str,
    events: Vec<crate::submit_gates::StoredEvent>,
) -> Result<crate::submit_gates::AuthorizationRecords, WorkerError> {
    let meta_ids = crate::submit_gates::followup_event_ids(&events)
        .map(str::to_owned)
        .collect();
    let event_meta = records_by_ids(db, "runtime_event_meta", &meta_ids)?;
    let mut scope =
        crate::submit_gates::AuthorizationScope::of_events(agent_id, &events, &event_meta);
    let rooms = records_by_ids(db, "runtime_rooms", &scope.rooms)?;
    scope.add_private_members(&rooms);
    Ok(crate::submit_gates::AuthorizationRecords {
        recipient_id: agent_id.to_owned(),
        agents: records_by_ids(db, "runtime_agents", &scope.agents)?,
        projects: records(db, "runtime_projects")?,
        resolved_paths: BTreeMap::new(),
        rooms,
        complaints: records_by_ids(db, "runtime_complaints", &scope.complaints)?,
        event_meta,
        events,
    })
}

/// Read the rows whose paths [`FilesystemSnapshot::capture_for_authorization`] resolves.
pub(super) async fn authorization_rows(
    store: &RuntimeStore,
    intent: &TurnIntent,
) -> Result<crate::submit_gates::AuthorizationRecords, WorkerError> {
    let intent = intent.clone();
    store
        .read_async(
            "runtime-services/account_worker_authorization_rows",
            move |tx| authorization_records(tx, &intent),
        )
        .await
}

/// The resolved paths that one authorization decision over `rows` reads:
/// every agent cwd and project path. Run it before SQL; a VM cwd gets a
/// stable guest identity instead of a host resolution.
pub(in crate::account_worker) fn authorization_paths(
    rows: &crate::submit_gates::AuthorizationRecords,
) -> Result<BTreeMap<String, String>, WorkerError> {
    let mut resolved_paths = BTreeMap::new();
    for (rows, key) in [(&rows.agents, "cwd"), (&rows.projects, "path")] {
        for raw in rows.values() {
            let record: Value = serde_json::from_str(raw).map_err(|_| invalid(key))?;
            if key == "cwd" && record.get("executionMode").and_then(Value::as_str) == Some("vm") {
                // VM cwd is guest-owned. Give authorization a stable,
                // normalized host-shaped identity without resolving it on
                // the host or treating it as a filesystem location.
                if let Some(path) = record
                    .get(key)
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                {
                    let agent_id = record
                        .get("id")
                        .and_then(Value::as_str)
                        .ok_or_else(|| invalid("vm-agent-id"))?;
                    let identity = vm_guest_identity(agent_id, path);
                    resolved_paths.insert(path.to_owned(), identity);
                }
                continue;
            }
            if let Some(path) = record
                .get(key)
                .and_then(Value::as_str)
                .filter(|p| !p.is_empty())
            {
                let resolved =
                    studio_runtime_store::path_identity::CanonicalPath::ancestor_nonstrict(
                        Path::new(path),
                    )
                    .map_err(|_| invalid(key))?;
                resolved_paths.insert(path.into(), resolved.to_string_lossy().into_owned());
            }
        }
    }
    Ok(resolved_paths)
}

/// Filesystem observations are captured on the preparation executor, before SQL.
/// Authorization still validates current stored owners in its writer transaction.
#[derive(Clone)]
pub(super) struct FilesystemSnapshot {
    resolved_paths: BTreeMap<String, String>,
}

impl FilesystemSnapshot {
    /// Capture the observations of one admission: the resolved paths of the
    /// rows its authorization decision reads. `rows` come from
    /// [`authorization_rows`].
    pub fn capture_for_authorization(
        rows: &crate::submit_gates::AuthorizationRecords,
    ) -> Result<Self, WorkerError> {
        Ok(Self {
            resolved_paths: authorization_paths(rows)?,
        })
    }
}

fn vm_guest_identity(agent_id: &str, guest_cwd: &str) -> String {
    format!(
        "/__codex_vm__/{agent_id}/{}",
        studio_runtime_store::sha256_hex(guest_cwd.as_bytes())
    )
}

#[cfg(test)]
mod vm_identity_tests {
    use super::vm_guest_identity;

    #[test]
    fn guest_cwd_gets_a_normalized_host_independent_identity() {
        let identity = vm_guest_identity("agent-1", "/guest/project/lines/main");
        assert!(identity.starts_with("/__codex_vm__/agent-1/"));
        let path = std::path::Path::new(&identity);
        assert!(path.is_absolute());
        assert!(
            !path
                .components()
                .any(|part| part == std::path::Component::ParentDir)
        );
        assert_eq!(
            identity,
            vm_guest_identity("agent-1", "/guest/project/lines/main")
        );
        assert_ne!(
            identity,
            vm_guest_identity("agent-1", "/guest/project/lines/other")
        );
    }
}
pub(super) fn authorize(
    tx: &Transaction<'_>,
    intent: &TurnIntent,
    filesystem: &FilesystemSnapshot,
    prepared_host: Option<&crate::host_project_runtime::PreparedHost>,
) -> Result<(), WorkerError> {
    if intent.radio_authorized {
        let prepared = prepared_host.ok_or_else(|| invalid("radio/prepared-host"))?;
        let radio_events = intent
            .events
            .iter()
            .filter(|event| event.kind == "radio_turn")
            .map(|event| {
                studio_runtime_store::QueueEventRepository::get(
                    tx,
                    &studio_runtime_store::QueueEventId::from(event.id.as_str()),
                )?
                .ok_or_else(|| invalid("radio/event"))
            })
            .collect::<Result<Vec<_>, WorkerError>>()?;
        if radio_events.len() != 1 {
            return Err(refusal("radio/turn-selection-changed".into()));
        }
        let selected = crate::radio_runtime::select_pending(
            tx,
            prepared,
            &agent(tx, &intent.agent_id)?.value(),
            &radio_events,
            now(),
        )
        .map_err(|error| refusal(error.to_string()))?;
        if selected.len() != 1 || selected[0].id.0 != radio_events[0].id.0 {
            return Err(refusal("radio/turn-selection-changed".into()));
        }
    }
    let mut rows = authorization_records(tx, intent)?;
    rows.resolved_paths = filesystem.resolved_paths.clone();
    let checked = crate::submit_gates::AuthorizationSnapshot::from_records(&rows)
        .map_err(|e| refusal(e.to_string()))?;
    crate::submit_gates::authorize_events(&checked).map_err(|e| refusal(e.to_string()))
}
fn optional<T: DeserializeOwned>(record: &Value, key: &str) -> Result<Option<T>, WorkerError> {
    record
        .get(key)
        .filter(|v| !v.is_null())
        .cloned()
        .map(|v| decode(v, key))
        .transpose()
}
fn flag(record: &Value, key: &str) -> Result<bool, WorkerError> {
    Ok(optional(record, key)?.unwrap_or(false))
}
fn text(record: &Value, key: &str) -> Result<Option<String>, WorkerError> {
    optional(record, key)
}
fn stored<T: DeserializeOwned>(
    record: &Value,
    key: &str,
) -> Result<thread::Stored<Option<T>>, WorkerError> {
    Ok(if record.get(key).is_some() {
        thread::Stored::Present(optional(record, key)?)
    } else {
        thread::Stored::Absent
    })
}
pub(super) fn guidance(
    resources: &PreparationResources,
    record: &Value,
    state_root: &Path,
) -> Result<thread::GuidanceBundle, WorkerError> {
    let role = role_guidance(resources, record)?;
    let mut thread_config = resources.thread_config.clone();
    // A managed worker (a child agent) runs without the user's MCP servers, so
    // none of them is spawned per thread. The lead and root chats keep them.
    // (Claude workers get the same rule from the adapter's `userMcp` param.)
    if record["parentId"].is_string() && ProviderKind::of_record(record) == Ok(ProviderKind::Codex)
    {
        let servers: Map<String, Value> = resources
            .filesystem_inputs
            .as_ref()
            .map(|inputs| inputs.mcp_servers.clone())
            .unwrap_or_default()
            .into_iter()
            .map(|name| (name, serde_json::json!({"enabled": false})))
            .collect();
        if !servers.is_empty() {
            thread_config.insert("mcp_servers".into(), Value::Object(servers));
        }
    }
    if !resources.account_home.is_empty() && resources.filesystem_inputs.is_none() {
        return Err(invalid("thread guidance filesystem inputs"));
    }
    let browser = browser_guidance(resources, record, &mut thread_config);
    // Python `codex_browser_host.thread_environment`, at the same call site as
    // `configure_browser`: host Codex threads send their sign-in links to Studio.
    if ProviderKind::of_record(record) == Ok(ProviderKind::Codex)
        && record["executionMode"] != "vm"
        && let Some(agent_id) = record["id"].as_str()
    {
        thread_config.extend(crate::browser_environment::thread_environment(
            state_root, agent_id,
        ));
    }
    Ok(thread::GuidanceBundle {
        instructions: resources.instructions.clone(),
        role,
        thread_config,
        browser,
    })
}

/// Python `codex_browser.configure_browser`: every host Codex agent (lead,
/// worker or reviewer) gets the account's native Chrome setup; Claude and VM
/// agents get none.
fn browser_guidance(
    resources: &PreparationResources,
    record: &Value,
    thread_config: &mut Map<String, Value>,
) -> Option<thread::BrowserGuidance> {
    if ProviderKind::of_record(record) != Ok(ProviderKind::Codex)
        || record["executionMode"] == "vm"
        || resources.account_home.is_empty()
        || resources.shared_home.is_empty()
    {
        return None;
    }
    let browser = resources.filesystem_inputs.as_ref()?.browser.clone()?;
    crate::browser::report_browser_status(
        record["accountKey"].as_str().unwrap_or("default"),
        browser.unavailable_reason.as_deref(),
    );
    for (key, value) in browser.overrides {
        // The order in which Codex applies these keys is not part of its
        // contract, and a whole `mcp_servers` table applied after
        // `mcp_servers.node_repl` could replace it. A worker's table of
        // disabled servers therefore carries the browser server itself, which
        // also replaces its own disabled entry.
        match (
            key.strip_prefix("mcp_servers."),
            thread_config
                .get_mut("mcp_servers")
                .and_then(Value::as_object_mut),
        ) {
            (Some(server), Some(servers)) => {
                servers.insert(server.to_owned(), value);
            }
            _ => {
                thread_config.insert(key, value);
            }
        }
    }
    Some(thread::BrowserGuidance {
        instructions: browser.developer_instructions.map(str::to_owned),
        skill_roots: browser.skill_roots,
    })
}

/// The role skill text of `record`. A turn needs only this; the thread config
/// (with its account MCP read) is the thread snapshot's.
fn role_guidance(resources: &PreparationResources, record: &Value) -> Result<String, WorkerError> {
    let lead = flag(record, "isLead")?;
    let name = if lead {
        "codex-orchestrator"
    } else {
        "codex-subagent"
    };
    let path = resources.skills_root.join(name).join("SKILL.md");
    let role = if let Some(role) = text(record, "nativeRoleGuidance")?.filter(|s| !s.is_empty()) {
        role
    } else {
        let content_path = if record["threadId"].is_string() {
            path.parent()
                .ok_or_else(|| invalid("role path"))?
                .join("references")
                .join(
                    if lead && record["nativeTeleportTool"] != "orchestration_teleport" {
                        "legacy-move-role.md"
                    } else {
                        "native-role.md"
                    },
                )
        } else {
            path.clone()
        };
        let relative = content_path
            .strip_prefix(&resources.skills_root)
            .map_err(|_| invalid("guidance path"))?;
        let content = resources
            .role_guidance
            .get(relative)
            .ok_or_else(|| refusal("thread-preparation/missing-guidance".into()))?;
        if content.trim().is_empty() {
            return Err(refusal("thread-preparation/missing-guidance".into()));
        }
        let vm = record["executionMode"] == "vm";
        let mut content = content.trim().to_owned();
        if vm {
            for relative in VM_ROLE_GUIDANCE {
                let guide = resources
                    .role_guidance
                    .get(Path::new(relative))
                    .ok_or_else(|| refusal("thread-preparation/missing-guidance".into()))?;
                content.push('\n');
                content.push_str(guide);
            }
        }
        // A VM session reads the skills inside the guest, not on this machine.
        let shown_root = if vm {
            Path::new(VM_GUEST_SKILLS_ROOT)
        } else {
            resources.skills_root.as_path()
        };
        format!(
            "[Studio role skill: {name}]\nSource: {}\nShared tool guidance: {}\n{}\n[End Studio role skill]",
            shown_root.join(name).join("SKILL.md").display(),
            shown_root.join("codex-workspace/SKILL.md").display(),
            content
        )
    };
    Ok(role)
}

/// The catalog an agent's thread uses: its saved source, else legacy for an
/// agent that already has a native thread.
fn catalog_source(record: &Value) -> Result<thread::CatalogSource, WorkerError> {
    Ok(
        optional(record, "nativeToolSource")?.unwrap_or(if record["threadId"].is_string() {
            thread::CatalogSource::Legacy
        } else {
            thread::CatalogSource::Current
        }),
    )
}

/// The definitions an agent's tools come from (Python `Runtime.tool_definitions`):
/// those of its catalog source, except that a VM agent always gets the
/// current catalog, also on a thread that Python started with legacy tools.
/// The legacy catalog lacks tools of VM agents, such as `orchestration_sign_in`.
fn catalog_definitions<'a>(
    resources: &'a PreparationResources,
    record: &Value,
) -> Result<&'a Vec<thread::ToolDefinition>, WorkerError> {
    Ok(
        if catalog_source(record)? == thread::CatalogSource::Legacy
            && record["executionMode"] != "vm"
        {
            &resources.legacy_tools
        } else {
            &resources.tools
        },
    )
}

/// The thread role of an agent record: its saved role, else the role of a
/// lead or a worker.
fn role(record: &Value) -> Result<thread::Role, WorkerError> {
    Ok(
        optional(record, "role")?.unwrap_or(if flag(record, "isLead")? {
            thread::Role::Orchestrator
        } else {
            thread::Role::Implementer
        }),
    )
}

fn federation_enabled(tx: &Transaction<'_>) -> Result<bool, WorkerError> {
    Ok(record_by_id(tx, "runtime_federation_settings", "global")?
        .map(|raw| serde_json::from_str::<Value>(&raw).map_err(|_| invalid("federation settings")))
        .transpose()?
        .is_some_and(|v| v["enabled"] == true))
}

/// The tool definitions of one agent, as Python's
/// `Runtime.tool_definitions(actor)` reports them to `orchestration_context`.
pub(crate) fn actor_tool_definitions(
    tx: &Transaction<'_>,
    resources: &PreparationResources,
    record: &Value,
) -> Result<Value, WorkerError> {
    let root_id = record["rootId"]
        .as_str()
        .ok_or_else(|| invalid("agent/rootId"))?;
    let root = agent(tx, root_id)?.value();
    let delegation = can_delegate(tx, record, &root)?;
    if let Some(frozen) = record.get("frozenNativeParams").filter(|v| !v.is_null()) {
        let mut definitions = frozen["dynamicTools"]
            .as_array()
            .cloned()
            .ok_or_else(|| invalid("agent/frozenNativeParams"))?;
        if !delegation {
            definitions.retain(|tool| tool["name"] != "orchestration_spawn");
        }
        return Ok(Value::Array(definitions));
    }
    if studio_runtime_store::truthy(record.get("nativeReview")) {
        return Ok(Value::Array(Vec::new()));
    }
    let definitions = catalog_definitions(resources, record)?;
    let cwd = text(record, "cwd")?.unwrap_or_default();
    let filtered = thread::filter_tool_definitions(
        definitions,
        &thread::CatalogFilter {
            provider: studio_runtime_providers::ProviderKind::of_record(record)
                .map_err(|_| invalid("agent/provider"))?,
            is_lead: flag(record, "isLead")?,
            can_delegate: delegation,
            federation_enabled: federation_enabled(tx)?,
            cwd: &cwd,
            session_tool: thread::session_tool_of(
                optional(record, "nativeTeleportTool")?,
                text(record, "threadId")?.is_some(),
            ),
            vm: thread::VmRole::of(record["executionMode"] == "vm", role(record)?),
        },
    )
    .map_err(|_| invalid("tool catalog"))?;
    serde_json::to_value(filtered).map_err(|_| invalid("tool catalog"))
}

pub(super) fn native_thread_blocked(record: &Value) -> bool {
    let Some(thread) = record["threadId"].as_str().filter(|s| !s.is_empty()) else {
        return false;
    };
    record["nativeThreadBlock"]["threadId"] == thread
        || record["error"]["codexErrorInfo"] == "misalignmentPolicyViolation"
        || (nonempty(&record["nativeTurnError"]["turnId"])
            && record["nativeTurnError"]["turnId"] == record["turnId"]
            && record["nativeTurnError"]["error"]["codexErrorInfo"]
                == "misalignmentPolicyViolation")
}
pub(super) fn context_repair_blocked(record: &Value) -> bool {
    nonempty(&record["contextRepairWait"])
        || matches!(
            record["contextRepair"]["phase"].as_str(),
            Some("preparing" | "submitted" | "unknown" | "ready")
        )
}
/// Whether a saved native release keeps this agent's queued input out of
/// thread preparation. Python `Runtime.prepare_locked` refuses a pending tool
/// reset (`resetPending`), then `codex_native_release.reconcile_unknown` acts
/// only on an unsettled `unknown`, `checking` or `unsubscribing` release: a
/// captured reset waits for exact native closure, and an unsettled release on
/// this thread and connection retries `thread/unsubscribe`. This adapter has no
/// native release driver, so it holds in both cases. Every other release
/// prepares normally, including `blocked`, where the inspection found native
/// work and never unsubscribed, and an unsettled release on an earlier
/// connection, which Python also ignores.
pub(super) fn native_release_held(record: &Value, connection: &str) -> bool {
    let release = &record["nativeRelease"];
    if nonempty(&release["resetPending"]) {
        return true;
    }
    let phase = release["phase"].as_str();
    if !matches!(phase, Some("unknown" | "checking" | "unsubscribing")) {
        return false;
    }
    if release.get("resetActorEpoch").is_some() {
        return true;
    }
    if phase == Some("checking") && nonempty(&release["inspectionPhase"]) {
        return false;
    }
    release["threadId"] == record["threadId"]
        && release["connectionId"].as_str() == Some(connection)
}
pub(super) fn nonempty(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Bool(value) => *value,
        Value::Object(value) => !value.is_empty(),
        Value::Array(value) => !value.is_empty(),
        Value::String(value) => !value.is_empty(),
        Value::Number(value) => value.as_f64() != Some(0.0),
    }
}
fn preserve_complaint(
    tx: &Transaction<'_>,
    agent: &str,
    event: &crate::scheduler::TurnEvent,
) -> Result<bool, WorkerError> {
    if event.kind != "complaint" {
        return Ok(false);
    }
    let value: Value = match serde_json::from_str(&event.text) {
        Ok(value) => value,
        Err(_) => return Ok(true),
    };
    let Some(entries) = value["complaints"].as_array().filter(|v| !v.is_empty()) else {
        return Ok(true);
    };
    for entry in entries {
        let Some(id) = entry["complaint_id"].as_str().filter(|s| !s.is_empty()) else {
            return Ok(true);
        };
        use rusqlite::OptionalExtension;
        let raw: Option<String> = tx
            .query_row(
                "SELECT record FROM runtime_complaints WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(raw) = raw else {
            return Ok(true);
        };
        let complaint: Value = serde_json::from_str(&raw).map_err(|_| invalid("complaint"))?;
        if complaint["leadId"] != agent {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Use ready workspace facts, not the lead's inherited workspace preference.
fn host_worktree(
    record: &Value,
    root: &Value,
    role: thread::Role,
    lead: bool,
) -> Result<Option<thread::HostWorktree>, WorkerError> {
    if record["provider"] != "claude"
        || lead
        || role != thread::Role::Implementer
        || record["parentId"].as_str().is_none()
        || record["executionMode"] == "vm"
        || !((record["worktree"] == true && record["worktreeReady"] == true)
            || (record["imageWorkspace"] == true && record["imageWorkspaceReady"] == true))
    {
        return Ok(None);
    }
    let tree = text(record, "cwd")?.ok_or_else(|| invalid("agent/cwd"))?;
    if !Path::new(&tree).is_absolute() {
        return Err(invalid("agent/cwd"));
    }
    // hostProjectPath survives worktree provisioning. Image copies also retain
    // imageWorkspaceRepo. The root paths protect the lead for nested workers.
    let mut fence = Vec::new();
    for value in [
        &record["imageWorkspaceRepo"],
        &record["hostProjectPath"],
        &root["cwd"],
        &root["hostProjectPath"],
    ] {
        if let Some(path) = value.as_str().filter(|path| !path.is_empty()) {
            if !Path::new(path).is_absolute() {
                return Err(invalid("agent/workspace-fence"));
            }
            if path != tree && !fence.iter().any(|entry| entry == path) {
                fence.push(path.to_owned());
            }
        }
    }
    if fence.is_empty() {
        return Err(invalid("agent/workspace-fence"));
    }
    Ok(Some(thread::HostWorktree { tree, fence }))
}

/// Python `frozenNativeParams` of a moved agent. Python reads the three saved
/// fields and the tools by key, so a missing one is an invalid stored agent.
fn frozen_params(record: &Value) -> Result<Option<thread::FrozenParams>, WorkerError> {
    let Some(frozen) = record.get("frozenNativeParams").filter(|v| !v.is_null()) else {
        return Ok(None);
    };
    let field = |key: &str| {
        frozen
            .get(key)
            .cloned()
            .ok_or_else(|| invalid("agent/frozenNativeParams"))
    };
    let dynamic_tools = serde_json::from_value(field("dynamicTools")?)
        .map_err(|_| invalid("agent/frozenNativeParams"))?;
    Ok(Some(thread::FrozenParams {
        base_instructions: field("baseInstructions")?,
        developer_instructions: field("developerInstructions")?,
        model: field("model")?,
        dynamic_tools,
        effort: frozen.get("effort").cloned(),
        summary: frozen.get("summary").cloned(),
    }))
}

pub(super) fn thread_snapshot(
    tx: &Transaction<'_>,
    resources: &PreparationResources,
    record: &Value,
    connection: &str,
    state_root: &Path,
    account_transfer_owner: Option<&str>,
) -> Result<thread::Snapshot, WorkerError> {
    let id = record["id"].as_str().ok_or_else(|| invalid("agent/id"))?;
    let root_id = record["rootId"]
        .as_str()
        .ok_or_else(|| invalid("agent/rootId"))?;
    let root = agent(tx, root_id)?.value();
    let source = catalog_source(record)?;
    let definitions = catalog_definitions(resources, record)?.clone();
    let lead = flag(record, "isLead")?;
    let role = role(record)?;
    let owned_transfer = account_transfer_owner
        .map(|transfer_id| {
            crate::account_transfer_driver::owns_reading_member(tx, transfer_id, id)
                .map_err(|error| refusal(format!("account-transfer/{error}")))
        })
        .transpose()?
        .unwrap_or(false);
    let admission = if record
        .get("accountTransferId")
        .is_some_and(|v| !v.is_null())
        && !owned_transfer
    {
        Some(thread::HoldReason::AccountTransfer)
    } else if context_repair_blocked(record) {
        Some(thread::HoldReason::ContextRepair)
    } else if native_release_held(record, connection) {
        Some(thread::HoldReason::NativeRelease)
    } else {
        None
    };
    let frozen = frozen_params(record)?;
    let unsupported = if record.get("nativeToolUpdate").is_some_and(|v| !v.is_null()) {
        Some(thread::Refusal::NativeCatalogMutation)
    } else {
        None
    };
    Ok(thread::Snapshot {
        actor: thread::Actor {
            id: id.into(),
            epoch: decode(record["epoch"].clone(), "agent/epoch")?,
            thread_id: text(record, "threadId")?,
            cwd: text(record, "cwd")?.ok_or_else(|| invalid("agent/cwd"))?,
            compactions: optional(record, "compactions")?.unwrap_or(0),
            prepare_attempt: text(record, "prepareAttempt")?,
            native_teleport_tool: optional(record, "nativeTeleportTool")?,
            native_role_guidance: text(record, "nativeRoleGuidance")?,
            native_tool_source: optional(record, "nativeToolSource")?,
            native_tool_update: optional(record, "nativeToolUpdate")?,
            error: text(record, "error")?,
            native_release: optional(record, "nativeRelease")?,
            root_id: root_id.into(),
            parent_id: text(record, "parentId")?,
            auto_wake: flag(record, "autoWake")?,
            native_effort_present: record.get("nativeEffort").is_some(),
            deleted: record.get("deletedAt").is_some_and(|v| !v.is_null()),
            native_review: studio_runtime_store::truthy(record.get("nativeReview")),
            vm: record["executionMode"] == "vm",
        },
        account: thread::Account {
            key: text(record, "accountKey")?.unwrap_or_else(|| "default".into()),
            provider: studio_runtime_providers::ProviderKind::of_record(record)
                .map_err(|_| invalid("agent/provider"))?,
            connection_id: connection.into(),
            available: true,
            supervisor_identity: None,
        },
        root: thread::Root {
            can_delegate: can_delegate(tx, record, &root)?,
            federation_enabled: federation_enabled(tx)?,
        },
        settings: thread::Settings {
            model: text(record, "model")?,
            effort: text(record, "effort")?,
            native_effort: text(record, "nativeEffort")?,
            fast_mode: flag(record, "fastMode")?,
            yolo_mode: optional(record, "yoloMode")?,
            profile_instructions: text(record, "profileInstructions")?.unwrap_or_default(),
            role,
            is_lead: lead,
            needs_title: flag(record, "needsTitle")?,
            claude_options: optional(record, "claudeOptions")?.unwrap_or_default(),
            daybreak_enabled: stored(record, "daybreakEnabled")?,
            cyber_access_program: stored(record, "cyberAccessProgram")?,
        },
        guidance: guidance(resources, record, state_root)?,
        catalog: thread::CatalogSnapshot {
            source,
            definitions,
            saved_thread_id: record
                .get("nativeToolCatalog")
                .and_then(|v| v["threadId"].as_str())
                .map(str::to_owned),
            saved_digest: record
                .get("nativeToolCatalog")
                .and_then(|v| v["digest"].as_str())
                .map(str::to_owned),
        },
        workspace: thread::WorkspaceReadiness::Ready,
        host_worktree: host_worktree(record, &root, role, lead)?,
        unsupported,
        admission,
        pending: None,
        loaded: None,
        closed: false,
        frozen,
    })
}

/// Give a receipt-time snapshot the cwd text of its attempt when the stored
/// cwd names the same folder. Preparation sends the canonical cwd of the
/// validated workspace, and the stored cwd can name that folder through a link
/// (macOS `/var` and `/tmp`). The pure receipt reducer compares text, so
/// without this step a valid receipt is stale. A cwd that names another
/// folder stays as stored, and the reducer refuses it.
pub(super) fn align_receipt_cwd(
    current: &mut thread::Snapshot,
    attempt: &thread::Snapshot,
    resources: &PreparationResources,
) {
    if let Some((stored, prepared, same)) = resources
        .filesystem_inputs
        .as_ref()
        .and_then(|inputs| inputs.receipt_cwd.as_ref())
        && *same
        && current.actor.cwd == *stored
        && attempt.actor.cwd == *prepared
    {
        current.actor.cwd.clone_from(&attempt.actor.cwd);
    }
}

pub(super) fn submission_snapshot(
    tx: &Transaction<'_>,
    resources: &PreparationResources,
    record: &Value,
    intent: &TurnIntent,
) -> Result<(turn::PreparationSnapshot, Vec<turn::Event>), WorkerError> {
    let root_id = record["rootId"]
        .as_str()
        .ok_or_else(|| invalid("agent/rootId"))?;
    let root = agent(tx, root_id)?.value();
    let role = role_guidance(resources, record)?;
    let plan=record_by_id(tx, "runtime_plans", root_id)?.map(|s|serde_json::from_str::<Value>(&s).map_err(|_|invalid("plan"))).transpose()?.filter(|v|v["native"].is_object()).map(|v|decode(json!({"id":v["id"],"rootId":v["rootId"],"steps":v["native"].get("plan").cloned().unwrap_or_else(||json!([])),"explanation":v["native"].get("explanation").filter(|v|!v.is_null()).cloned().unwrap_or(json!(""))}),"plan")).transpose()?;
    let mut complaints = Vec::new();
    for raw in lead_complaints(tx, &intent.agent_id)? {
        let v: Value = serde_json::from_str(&raw).map_err(|_| invalid("complaint"))?;
        // Python only projects unanswered lead-addressed complaints into context.
        // Unrelated and closed records need no body projection.
        if v["leadId"] != intent.agent_id
            || v["recipient"] != "lead"
            || matches!(v["status"].as_str(), Some("resolved" | "declined"))
        {
            continue;
        }
        let authors = v["responses"]
            .as_array()
            .ok_or_else(|| invalid("complaint/responses"))?
            .iter()
            .map(|r| decode(r["author"].clone(), "response/author"))
            .collect::<Result<Vec<String>, _>>()?;
        if !(v["sourceType"] == "user_task" && v["status"] == "open")
            && authors.contains(&intent.agent_id)
        {
            continue;
        }
        let author: String = decode(v["author"].clone(), "complaint/author")?;
        let author_name = AgentRepository::get(tx, &AgentId::from(author.as_str()))?
            .map(|a| text(&a.value(), "name"))
            .transpose()?
            .flatten();
        complaints.push(turn::Complaint {
            id: decode(v["id"].clone(), "complaint/id")?,
            lead_id: intent.agent_id.clone(),
            recipient: decode(v["recipient"].clone(), "complaint/recipient")?,
            status: decode(v["status"].clone(), "complaint/status")?,
            legacy_user_task: v["sourceType"] == "user_task",
            response_authors: authors,
            author,
            author_name,
            text: decode(v["text"].clone(), "complaint/text")?,
            version: optional(&v, "version")?,
            created: decode(v["created"].clone(), "complaint/created")?,
        });
    }
    ensure_context_tables(tx)?;
    let mut stmt = tx.prepare(
        "SELECT record FROM runtime_context_manifests WHERE agent=?1 ORDER BY sequence DESC",
    )?;
    let delivered_manifests: Vec<turn::ContextManifest> = stmt
        .query_map([&intent.agent_id], |r| r.get::<_, String>(0))?
        .map(|r| {
            let r = r?;
            decode(
                serde_json::from_str(&r).map_err(|_| invalid("manifest"))?,
                "manifest",
            )
        })
        .collect::<Result<Vec<_>, WorkerError>>()?;
    let epoch = turn::Epoch(
        text(record, "threadId")?,
        optional(record, "compactions")?.unwrap_or(0),
    );
    let restore = turn::needs_team_restore(&epoch, &delivered_manifests);
    let team_state = if restore {
        super::team_restore::snapshot(tx, record)?
    } else {
        None
    };
    let mut workers = Vec::new();
    // Only a lead's turn context (finished-worktree reminder) or a batch carrying
    // child events (team progress line) reads the team.
    let child_events = intent
        .events
        .iter()
        .any(|e| matches!(e.kind.as_str(), "child_result" | "child_stopped"));
    let team = if child_events || flag(record, "isLead")? {
        studio_runtime_store::AgentIndexedReads::team_records(tx, root_id)?
    } else {
        Vec::new()
    };
    for (_, raw) in team {
        let v: Value = serde_json::from_str(&raw).map_err(|_| invalid("worker"))?;
        let status = match v["status"].as_str() {
            Some("completed") => turn::WorkerStatus::Completed,
            Some("failed") => turn::WorkerStatus::Failed,
            Some("interrupted") => turn::WorkerStatus::Interrupted,
            Some("paused") => turn::WorkerStatus::Paused,
            Some("queued") => turn::WorkerStatus::Queued,
            Some(_) => turn::WorkerStatus::Running,
            None => return Err(invalid("worker/status")),
        };
        workers.push(turn::Worker {
            id: decode(v["id"].clone(), "worker/id")?,
            root_id: root_id.into(),
            parent_id: text(&v, "parentId")?,
            worktree_ready: flag(&v, "worktreeReady")?,
            deleted: v
                .get("deletedAt")
                .is_some_and(|v| !v.is_null() && v != &json!(0)),
            status,
            auto_wake: flag(&v, "autoWake")?,
        });
    }
    let mut assets = Vec::new();
    let ids = intent
        .events
        .iter()
        .map(|e| optional::<Vec<String>>(&e.metadata, "assets"))
        .collect::<Result<Vec<_>, _>>()?
        .into_iter()
        .flatten()
        .flatten()
        .collect::<BTreeSet<_>>();
    for id in ids {
        use rusqlite::OptionalExtension;
        let raw: String = tx
            .query_row(
                "SELECT record FROM runtime_assets WHERE id=?1",
                [&id],
                |r| r.get(0),
            )
            .optional()?
            .ok_or_else(|| refusal("Unknown attachment".into()))?;
        let v: Value = serde_json::from_str(&raw).map_err(|_| invalid("asset"))?;
        let owner = text(&v, "agentId")?
            .or(text(&v, "agent")?)
            .ok_or_else(|| invalid("asset/agent"))?;
        let owner = agent(tx, &owner)?.value();
        assets.push(turn::Asset {
            id,
            owner_id: decode(owner["id"].clone(), "owner/id")?,
            owner_root_id: decode(owner["rootId"].clone(), "owner/rootId")?,
            owner_deleted: owner.get("deletedAt").is_some_and(|v| !v.is_null()),
            image: flag(&v, "image")?,
            path: decode(v["path"].clone(), "asset/path")?,
            name: decode(v["name"].clone(), "asset/name")?,
            size: decode(v["size"].clone(), "asset/size")?,
            view: v.as_object().ok_or_else(|| invalid("asset"))?.clone(),
        });
    }
    let mut stmt = tx.prepare("SELECT version FROM runtime_context_reminders WHERE agent=?1")?;
    let delivered_reminders = stmt
        .query_map([&intent.agent_id], |r| r.get(0))?
        .collect::<Result<BTreeSet<String>, _>>()?;
    let events = intent
        .events
        .iter()
        .map(|e| {
            Ok(turn::Event {
                id: e.id.clone(),
                kind: e.kind.clone(),
                text: e.text.clone(),
                created: e.created,
                metadata: turn::EventMetadata {
                    assets: optional(&e.metadata, "assets")?.unwrap_or_default(),
                    accepted_at: optional(&e.metadata, "acceptedAt")?,
                    extra: e
                        .metadata
                        .as_object()
                        .ok_or_else(|| invalid("event metadata"))?
                        .clone(),
                },
                preserve_progress: flag(&e.metadata, "preserveProgress")?,
                preserve_complaint: preserve_complaint(tx, &intent.agent_id, e)?,
            })
        })
        .collect::<Result<Vec<_>, WorkerError>>()?;
    let concurrency = root_concurrency(&root)?;
    Ok((
        turn::PreparationSnapshot {
            provider: Some(intent.provider),
            actor: turn::Actor {
                id: intent.agent_id.clone(),
                root_id: root_id.into(),
                epoch: turn::Epoch(
                    text(record, "threadId")?,
                    optional(record, "compactions")?.unwrap_or(0),
                ),
                is_lead: flag(record, "isLead")?,
                forked_from: text(record, "forkedFrom")?,
                prepared_context: optional(record, "preparedContext")?,
                delivered_mode: optional(record, "deliveredMode")?,
            },
            root: turn::RootSettings {
                concurrency,
                mode_revision: optional(&root, "agentModeRevision")?.unwrap_or(0),
            },
            delivered_manifests,
            plan,
            complaints,
            workers,
            team_state,
            delivered_reminders,
            assets,
            guidance: turn::GuidanceBundle { role },
            artifact_paths: BTreeMap::new(),
            prepared_at: now(),
            operation_id: String::new(),
        },
        events,
    ))
}
pub(crate) fn ensure_context_tables(tx: &Transaction<'_>) -> Result<(), WorkerError> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS runtime_context_manifests(agent TEXT NOT NULL,thread_id TEXT NOT NULL,compactions INTEGER NOT NULL,sequence INTEGER NOT NULL,event_id TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(agent,thread_id,compactions)); CREATE TABLE IF NOT EXISTS runtime_context_reminders(agent TEXT NOT NULL,version TEXT NOT NULL,event_id TEXT NOT NULL,PRIMARY KEY(agent,version));")?;
    Ok(())
}

fn root_concurrency(root: &Value) -> Result<u16, WorkerError> {
    let value = if root["agentMode"] == "single"
        && root["subagentConcurrencyVersion"].as_u64().unwrap_or(0) < 2
    {
        0
    } else {
        optional(root, "concurrency")?.unwrap_or(if root["agentMode"] == "single" { 0 } else { 32 })
    };
    if value > 512 {
        return Err(invalid("root/concurrency"));
    }
    Ok(value)
}
fn can_delegate(tx: &Transaction<'_>, record: &Value, root: &Value) -> Result<bool, WorkerError> {
    if record["role"] == "reviewer"
        || !flag(root, "isLead")?
        || text(root, "parentId")?.is_some()
        || root_concurrency(root)? == 0
    {
        return Ok(false);
    }
    let limit = root["workerDefaults"]["delegationDepth"]
        .as_u64()
        .filter(|v| (1..=3).contains(v))
        .unwrap_or(1);
    let mut cursor = record.clone();
    let mut depth = 0;
    let mut seen = BTreeSet::new();
    while cursor["id"] != root["id"] {
        if !seen.insert(text(&cursor, "id")?.ok_or_else(|| invalid("delegation/id"))?) {
            return Ok(false);
        }
        let Some(parent) = text(&cursor, "parentId")? else {
            return Ok(false);
        };
        let Some(parent) = AgentRepository::get(tx, &AgentId::from(parent.as_str()))? else {
            return Ok(false);
        };
        cursor = parent.value();
        if cursor["rootId"] != root["id"] {
            return Ok(false);
        }
        depth += 1;
        if depth >= limit {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod resource_tests {
    fn guidance(
        resources: &super::PreparationResources,
        record: &serde_json::Value,
        state_root: &std::path::Path,
    ) -> Result<crate::thread_prepare::GuidanceBundle, super::WorkerError> {
        super::guidance(
            &resources.clone().capture_filesystem(None),
            record,
            state_root,
        )
    }

    use super::*;

    /// igor-mbp rehearsal 2026-10-22: two moved Claude agents carried Python's
    /// `frozenNativeParams`; Rust refused them (`thread-preparation/FrozenImport`)
    /// and a child result turned the completed parent into a failed agent.
    #[test]
    fn a_moved_agent_keeps_its_frozen_native_parameters() {
        let tool = json!({"name":"orchestration_message","description":"Saved","inputSchema":{"type":"object"}});
        let record = json!({"frozenNativeParams":{"baseInstructions":"base","developerInstructions":"guidance",
            "model":"default","dynamicTools":[tool],"effort":"high"}});
        let frozen = frozen_params(&record).expect("valid").expect("frozen");
        assert_eq!(frozen.model, json!("default"));
        assert_eq!(frozen.developer_instructions, json!("guidance"));
        assert_eq!(frozen.dynamic_tools.len(), 1);
        assert_eq!(frozen.effort, Some(json!("high")));
        assert_eq!(frozen.summary, None);
        assert!(
            frozen_params(&json!({"frozenNativeParams":null}))
                .expect("absent")
                .is_none()
        );
        for missing in [
            "baseInstructions",
            "developerInstructions",
            "model",
            "dynamicTools",
        ] {
            let mut broken = record.clone();
            broken["frozenNativeParams"]
                .as_object_mut()
                .expect("object")
                .remove(missing);
            assert!(frozen_params(&broken).is_err(), "{missing} is required");
        }
        let mut malformed = record;
        malformed["frozenNativeParams"]["dynamicTools"] = json!([{"name":"orchestration_message"}]);
        assert!(
            frozen_params(&malformed).is_err(),
            "a tool needs its schema"
        );
    }

    #[test]
    fn native_thread_blocks_follow_the_current_thread_and_turn() {
        assert!(native_thread_blocked(
            &json!({"threadId":"current","nativeThreadBlock":{"threadId":"current"}})
        ));
        assert!(!native_thread_blocked(
            &json!({"threadId":"current","nativeThreadBlock":{"threadId":"old"}})
        ));
        assert!(native_thread_blocked(
            &json!({"threadId":"current","error":{"codexErrorInfo":"misalignmentPolicyViolation"}})
        ));
        assert!(!native_thread_blocked(
            &json!({"threadId":null,"error":{"codexErrorInfo":"misalignmentPolicyViolation"}})
        ));
        assert!(native_thread_blocked(
            &json!({"threadId":"current","turnId":"turn","nativeTurnError":{"turnId":"turn","error":{"codexErrorInfo":"misalignmentPolicyViolation"}}})
        ));
        assert!(!native_thread_blocked(
            &json!({"threadId":"current","turnId":"new","nativeTurnError":{"turnId":"old","error":{"codexErrorInfo":"misalignmentPolicyViolation"}}})
        ));
    }

    /// The live schema's complaint index serves a lead's complaints; a
    /// submission never scans the complaints of other leads.
    #[test]
    fn lead_complaints_use_the_lead_index() {
        let fixture: Value = serde_json::from_str(include_str!(
            "../../../../runtime-store/tests/fixtures/live-state-schema.json"
        ))
        .expect("live-state schema fixture");
        let connection = rusqlite::Connection::open_in_memory().expect("memory SQLite opens");
        connection
            .execute_batch(
                "CREATE TABLE runtime_complaints(id TEXT PRIMARY KEY, record TEXT NOT NULL)",
            )
            .expect("complaints table creates");
        for object in fixture["canvas.sqlite3"]["sqlite_master"]
            .as_array()
            .expect("fixture lists schema objects")
        {
            if object["table"] == "runtime_complaints"
                && object["type"] == "index"
                && let Some(sql) = object["sql"].as_str()
            {
                connection
                    .execute_batch(sql)
                    .expect("fixture index creates");
            }
        }
        for (id, lead) in [("b", "lead"), ("a", "lead"), ("c", "other")] {
            connection
                .execute(
                    "INSERT INTO runtime_complaints(id,record) VALUES(?1,?2)",
                    [id, &json!({"id":id,"leadId":lead}).to_string()],
                )
                .expect("complaint inserts");
        }
        let plan = connection
            .prepare(&format!("EXPLAIN QUERY PLAN {LEAD_COMPLAINTS_SQL}"))
            .expect("plan prepares")
            .query_map(["lead"], |row| row.get::<_, String>(3))
            .expect("plan runs")
            .collect::<Result<Vec<_>, _>>()
            .expect("plan rows")
            .join(" | ");
        assert!(
            plan.contains("runtime_conversion_complaints_leadId"),
            "{plan}"
        );
        let ids: Vec<String> = lead_complaints(&connection, "lead")
            .expect("lead complaints")
            .iter()
            .map(|raw| serde_json::from_str::<Value>(raw).expect("record")["id"].to_string())
            .collect();
        assert_eq!(ids, ["\"a\"", "\"b\""]);
    }

    /// Python stores a review child's `nativeReview` as its review object and
    /// tests it for truth. Reading it as a boolean ended the account worker
    /// (19 October 2026, review children of root 85bc2231).
    #[tokio::test]
    async fn a_native_review_object_marks_the_review_child() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = studio_runtime_store::RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store opens");
        let resources = PreparationResources::bundled(String::new(), ModelCatalogService::new())
            .expect("runtime resources");
        store
            .transaction_with(move |tx| {
                let root = json!({"id":"root","rootId":"root","epoch":1,"status":"completed",
                    "isLead":true,"name":"Lead","provider":"codex","accountKey":"default"});
                AgentRepository::put(tx, &AgentRecord::from_value(root).expect("root"))?;
                let review = json!({"id":"review","rootId":"root","parentId":"root",
                    "role":"reviewer","provider":"codex",
                    "nativeReview":{"target":{"type":"uncommittedChanges"},"requestId":"q"}});
                let tools = actor_tool_definitions(tx, &resources, &review)
                    .map_err(|error| StoreError::SchemaMismatch(error.to_string()))?;
                assert_eq!(tools, json!([]));
                Ok::<_, StoreError>(())
            })
            .await
            .expect("a review object is read as a review child");
    }

    /// Python `Runtime.tool_definitions` gives a VM agent the current catalog,
    /// also on a thread that Python started before `nativeToolSource` existed
    /// and on a new thread of an agent that Python saved as `legacy`. The
    /// legacy catalog lacks `orchestration_sign_in`; Rust gave it to these VM
    /// agents. The saved source and a Codex resume stay as they were.
    #[tokio::test]
    async fn a_vm_agent_on_an_old_thread_gets_the_current_catalog() {
        let directory = tempfile::tempdir().expect("temporary state directory");
        let store = studio_runtime_store::RuntimeStore::open(Some(directory.path()))
            .await
            .expect("runtime store opens");
        let resources = PreparationResources::bundled(String::new(), ModelCatalogService::new())
            .expect("runtime resources")
            .capture_filesystem(None);
        store
            .transaction_with(move |tx| {
                let failed = |error: WorkerError| StoreError::SchemaMismatch(error.to_string());
                let names = |tools: Vec<thread::ToolDefinition>| {
                    tools.into_iter().map(|tool| tool.name).collect::<Vec<_>>()
                };
                for mode in ["vm", "native"] {
                    let vm = mode == "vm";
                    // An old thread without a saved source, and a new thread
                    // of an agent whose saved source is legacy.
                    for (thread_id, source) in [(Some("thread-old"), None), (None, Some("legacy"))]
                    {
                        let mut record = json!({"id":mode, "rootId":mode, "epoch":1,
                            "status":"running", "isLead":true, "name":"Lead", "provider":"codex",
                            "accountKey":"default", "executionMode":mode, "cwd":"/line"});
                        if let Some(thread_id) = thread_id {
                            record["threadId"] = json!(thread_id);
                        }
                        if let Some(source) = source {
                            record["nativeToolSource"] = json!(source);
                        }
                        AgentRepository::put(
                            tx,
                            &AgentRecord::from_value(record.clone()).expect("agent"),
                        )?;
                        let snapshot = thread_snapshot(
                            tx,
                            &resources,
                            &record,
                            "connection",
                            Path::new("/state"),
                            None,
                        )
                        .map_err(failed)?;
                        let case = format!("{mode} {thread_id:?} {source:?}");
                        assert_eq!(
                            snapshot.catalog.source,
                            thread::CatalogSource::Legacy,
                            "{case}"
                        );
                        let expected = if vm {
                            &resources.tools
                        } else {
                            &resources.legacy_tools
                        };
                        assert_eq!(&snapshot.catalog.definitions, expected, "{case}");
                        let tools = names(thread::tool_definitions(&snapshot).expect("tools"));
                        assert_eq!(
                            tools.iter().any(|name| name == "orchestration_sign_in"),
                            vm,
                            "{case}"
                        );
                        let attempt = match thread::plan(&snapshot, "attempt", "ticket") {
                            thread::Outcome::Start(attempt) | thread::Outcome::Resume(attempt) => {
                                attempt
                            }
                            other => panic!("{case}: {other:?}"),
                        };
                        assert_eq!(
                            attempt.before_submit.set["nativeToolSource"],
                            json!("legacy"),
                            "{case}"
                        );
                        // A Codex resume sends no catalog; a start sends the
                        // catalog above.
                        let sent = attempt.params.get("dynamicTools").map(|tools| {
                            tools
                                .as_array()
                                .expect("tools")
                                .iter()
                                .map(|tool| tool["name"].as_str().expect("name").to_owned())
                                .collect::<Vec<_>>()
                        });
                        assert_eq!(sent, thread_id.is_none().then_some(tools), "{case}");
                    }
                }
                Ok::<_, StoreError>(())
            })
            .await
            .expect("a VM agent keeps its tools");
    }

    #[test]
    fn only_host_codex_threads_send_sign_in_links_to_studio() {
        let resources = PreparationResources::bundled(String::new(), ModelCatalogService::new())
            .expect("runtime resources");
        let state = tempfile::tempdir().expect("state root");
        let folder = state
            .path()
            .join(crate::browser_environment::BROWSER_HOST_FOLDER);
        std::fs::create_dir_all(&folder).expect("host bridge folder");
        std::fs::write(
            folder.join(crate::browser_environment::BROWSER_LAUNCHER),
            "#!/bin/sh\n",
        )
        .expect("launcher");
        let config = |record: Value| {
            guidance(&resources, &record, state.path())
                .expect("guidance")
                .thread_config
        };
        let host = config(json!({"id":"host", "isLead":true, "provider":"codex"}));
        assert_eq!(
            host["shell_environment_policy.set.STUDIO_BROWSER_SESSION"],
            "host"
        );
        assert!(host.contains_key("shell_environment_policy.set.BROWSER"));
        assert!(host.contains_key("shell_environment_policy.set.STUDIO_BROWSER_SOCKET"));
        for record in [
            json!({"id":"claude", "isLead":true, "provider":"claude"}),
            json!({"id":"vm", "isLead":true, "provider":"codex", "executionMode":"vm"}),
        ] {
            let config = config(record);
            assert!(
                !config
                    .keys()
                    .any(|key| key.starts_with("shell_environment_policy.set.")),
                "{config:?}"
            );
        }
    }

    #[test]
    fn every_host_codex_agent_gets_the_account_browser_like_python() {
        let crate::browser::BrowserHomes {
            root: _root,
            shared,
            account,
            node,
            skills,
        } = crate::browser::browser_homes();
        let mut resources = PreparationResources::bundled(
            account.to_string_lossy().into_owned(),
            ModelCatalogService::new(),
        )
        .expect("runtime resources");
        let bundle = |resources: &PreparationResources, record: Value| {
            guidance(resources, &record, Path::new("/state")).expect("guidance")
        };
        let lead = json!({"id":"l", "isLead":true, "provider":"codex", "accountKey":"profile-a"});
        // Without a shared home (tests and the default constructor) nothing changes.
        assert_eq!(bundle(&resources, lead.clone()).browser, None);
        resources.shared_home = shared.to_string_lossy().into_owned();

        let expected = Some(thread::BrowserGuidance {
            instructions: Some(crate::browser::BROWSER_GUIDANCE.to_owned()),
            skill_roots: vec![skills.to_string_lossy().into_owned()],
        });
        let lead = bundle(&resources, lead);
        assert_eq!(lead.browser, expected);
        let server = &lead.thread_config["mcp_servers.node_repl"];
        assert_eq!(server["command"], json!(node));
        assert_eq!(
            server["env"]["CODEX_HOME"],
            json!(CanonicalPath::existing(&account).expect("account"))
        );
        assert!(server["env"].get("OPENAI_API_KEY").is_none());
        assert!(lead.thread_config.get("mcp_servers").is_none());

        // A worker keeps the browser; its table of disabled user servers
        // carries node_repl, so no key order can drop it.
        for role in ["implementer", "reviewer"] {
            let worker = bundle(
                &resources,
                json!({"id":"w", "parentId":"l", "role":role, "provider":"codex"}),
            );
            assert_eq!(worker.browser, expected, "{role}");
            assert!(worker.thread_config.get("mcp_servers.node_repl").is_none());
            let servers = &worker.thread_config["mcp_servers"];
            assert_eq!(servers["anarlog"], json!({"enabled": false}));
            assert_eq!(servers["node_repl"], *server, "{role}");
        }

        // Python's call site excludes Claude and VM agents.
        for record in [
            json!({"id":"c", "provider":"claude"}),
            json!({"id":"v", "provider":"codex", "executionMode":"vm"}),
        ] {
            let other = bundle(&resources, record);
            assert_eq!(other.browser, None);
            assert!(other.thread_config.get("mcp_servers.node_repl").is_none());
        }

        // A disabled plugin clears the skill root and adds no server.
        std::fs::write(
            account.join("config.toml"),
            "[plugins.\"chrome@openai-bundled\"]\nenabled = false\n",
        )
        .expect("disabled plugin");
        let off = bundle(
            &resources,
            json!({"id":"l", "isLead":true, "provider":"codex"}),
        );
        assert_eq!(
            off.browser,
            Some(thread::BrowserGuidance {
                instructions: None,
                skill_roots: Vec::new(),
            })
        );
        assert!(off.thread_config.get("mcp_servers.node_repl").is_none());
    }

    #[test]
    fn only_a_codex_worker_switches_off_the_homes_mcp_servers() {
        let directory = tempfile::tempdir().expect("owned test directory");
        let home = directory.path().to_path_buf();
        std::fs::create_dir_all(&home).expect("home");
        std::fs::write(
            home.join("config.toml"),
            "[mcp_servers.playwright]\ncommand = \"npx\"\n[mcp_servers.\"aws.mcp\"]\ncommand = \"uvx\"\n",
        )
        .expect("config");
        let resources = PreparationResources::bundled(
            home.to_string_lossy().into_owned(),
            ModelCatalogService::new(),
        )
        .expect("runtime resources");
        let servers = |record: Value| {
            guidance(&resources, &record, Path::new("/state"))
                .expect("guidance")
                .thread_config
                .get("mcp_servers")
                .cloned()
        };
        assert_eq!(
            servers(json!({"id":"w", "parentId":"lead", "provider":"codex"})),
            Some(json!({"playwright":{"enabled":false},"aws.mcp":{"enabled":false}}))
        );
        assert_eq!(servers(json!({"id":"l", "isLead":true})), None);
        assert_eq!(servers(json!({"id":"r", "provider":"codex"})), None);
        assert_eq!(
            servers(json!({"id":"c", "parentId":"lead", "provider":"claude"})),
            None
        );
    }

    #[test]
    fn layr_sessions_get_the_layr_guidance_and_skill() {
        let mut resources =
            PreparationResources::bundled(String::new(), ModelCatalogService::new())
                .expect("runtime resources");
        let skill = resources.role_guidance[Path::new("layr/SKILL.md")].clone();
        let reference =
            resources.role_guidance[Path::new("codex-workspace/references/layr.md")].clone();
        for lead in [true, false] {
            let vm = guidance(
                &resources,
                &json!({"id":"agent", "isLead":lead, "executionMode":"vm"}),
                Path::new("/state"),
            )
            .expect("layr guidance");
            assert!(
                vm.role
                    .contains(&format!("\n{reference}\n{skill}\n[End Studio role skill]"))
            );
            let name = if lead {
                "codex-orchestrator"
            } else {
                "codex-subagent"
            };
            assert!(vm.role.contains(&format!(
                "Source: {VM_GUEST_SKILLS_ROOT}/{name}/SKILL.md\nShared tool guidance: \
                 {VM_GUEST_SKILLS_ROOT}/codex-workspace/SKILL.md\n"
            )));
            let native = guidance(
                &resources,
                &json!({"id":"agent", "isLead":lead}),
                Path::new("/state"),
            )
            .expect("native guidance");
            assert!(!native.role.contains("# layr by task"));
            assert!(!native.role.contains(VM_GUEST_SKILLS_ROOT));
        }
        resources.role_guidance.remove(Path::new("layr/SKILL.md"));
        let refused = guidance(
            &resources,
            &json!({"id":"agent", "isLead":true, "executionMode":"vm"}),
            Path::new("/state"),
        );
        assert!(matches!(
            refused,
            Err(WorkerError::PreparationRefused(reason))
                if reason == "thread-preparation/missing-guidance"
        ));
    }
}

#[cfg(test)]
mod workspace_fence_tests {
    use super::*;

    #[test]
    fn task_result_schema_native_and_legacy_catalogs_use_the_owner() {
        let resources = PreparationResources::bundled(String::new(), ModelCatalogService::new())
            .expect("resources");
        for tools in [&resources.tools, &resources.legacy_tools] {
            let task = tools
                .iter()
                .find(|tool| tool.name == "orchestration_task")
                .expect("task");
            assert_eq!(
                task.input_schema["properties"]["result_schema"],
                studio_runtime_schema::task_results::schema_input()
            );
            assert!(
                task.input_schema["properties"]["result"]
                    .get("type")
                    .is_none()
            );
            let spawn = tools
                .iter()
                .find(|tool| tool.name == "orchestration_spawn")
                .expect("spawn");
            assert!(spawn.input_schema["properties"]["agents"]["items"]["properties"]["task_id"]["description"].as_str().expect("description").contains("result_schema"));
        }
    }

    #[test]
    fn claude_child_spawn_catalog_exposes_valid_permission_modes() {
        let resources = PreparationResources::bundled(String::new(), ModelCatalogService::new())
            .expect("resources");
        for tools in [&resources.tools, &resources.legacy_tools] {
            let spawn = tools
                .iter()
                .find(|tool| tool.name == "orchestration_spawn")
                .expect("spawn");
            let field = &spawn.input_schema["properties"]["agents"]["items"]["properties"]["permission_mode"];
            assert_eq!(
                field["enum"],
                json!(crate::claude_controls::PERMISSION_MODES)
            );
        }
    }

    #[tokio::test]
    async fn claude_child_saved_mode_reaches_start_and_resume() {
        let directory = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store");
        let resources = PreparationResources::bundled(String::new(), ModelCatalogService::new())
            .expect("resources");
        store.transaction_with(move |tx| {
            AgentRepository::put(tx, &AgentRecord::from_value(json!({"id":"lead","rootId":"lead","epoch":1,
                "status":"running","isLead":true,"cwd":"/project"})).expect("lead"))?;
            for mode in crate::claude_controls::PERMISSION_MODES {
                for resumed in [false, true] {
                    let mut record = json!({"id":"worker","rootId":"lead","parentId":"lead","epoch":1,
                        "status":"queued","isLead":false,"role":"implementer","provider":"claude","cwd":"/project",
                        "workspaceMode":"shared","yoloMode":*mode == "bypassPermissions","claudeOptions":{"permissionMode": mode}});
                    if resumed {
                        record["threadId"] = json!("existing-thread");
                        record["nativeToolSource"] = json!("current");
                    }
                    let snapshot = thread_snapshot(tx, &resources, &record, "connection", Path::new("/state"),
                        None)?;
                    let attempt = match thread::plan(&snapshot, "attempt", "ticket") {
                        thread::Outcome::Start(attempt) | thread::Outcome::Resume(attempt) => attempt,
                        other => panic!("expected thread launch, got {other:?}"),
                    };
                    assert_eq!(attempt.params["claude"]["permissionMode"], *mode);
                }
            }
            Ok::<_, WorkerError>(())
        }).await.expect("thread plans");
    }

    #[tokio::test]
    async fn claude_workspace_fence_reaches_thread_launch_options() {
        let directory = tempfile::tempdir().expect("temporary state");
        let store = RuntimeStore::open(Some(directory.path()))
            .await
            .expect("store");
        let resources = PreparationResources::bundled(String::new(), ModelCatalogService::new())
            .expect("resources");
        store.transaction_with(move |tx| {
            let root = json!({"id":"lead","rootId":"lead","epoch":1,"status":"running",
                "isLead":true,"cwd":"/source checkout","hostProjectPath":"/source checkout"});
            AgentRepository::put(tx, &AgentRecord::from_value(root).expect("lead"))?;
            for (mode, provider, role, ready, fenced) in [
                ("worktree", "claude", "implementer", true, true),
                ("image", "claude", "implementer", true, true),
                ("shared", "claude", "implementer", true, false),
                ("worktree", "claude", "reviewer", true, false),
                ("image", "claude", "reviewer", true, false),
                ("worktree", "claude", "implementer", false, false),
                ("image", "claude", "implementer", false, false),
                ("worktree", "codex", "implementer", true, false),
            ] {
                let record = json!({"id":"worker","rootId":"lead","parentId":"lead",
                    "epoch":1,"status":"queued","isLead":false,"role":role,"provider":provider,
                    "cwd":"/worker copy:quoted\"path","hostProjectPath":"/parent copy",
                    "imageWorkspaceRepo":if mode == "image" { json!("/image source") } else { Value::Null },
                    "workspaceMode":mode,"worktree":mode == "worktree","worktreeReady":ready,
                    "imageWorkspace":mode == "image","imageWorkspaceReady":ready,
                    "yoloMode":true,"claudeOptions":{"permissionMode":"bypassPermissions"}});
                for resumed in [false, true] {
                    let mut record = record.clone();
                    if resumed {
                        // A current thread retains the catalog selected at start.
                        record["threadId"] = json!("existing-thread");
                        record["nativeToolSource"] = json!("current");
                    }
                    let snapshot = thread_snapshot(tx, &resources, &record, "connection", Path::new("/state"),
                        None)?;
                    let attempt = match thread::plan(&snapshot, "attempt", "ticket") {
                        thread::Outcome::Start(attempt) | thread::Outcome::Resume(attempt) => attempt,
                        other => panic!("expected thread launch, got {other:?}"),
                    };
                    let options = attempt.params.get("claude").unwrap_or(&Value::Null);
                    if provider == "claude" && role == "reviewer" {
                        assert_eq!(options["permissionMode"], "plan");
                        assert_eq!(attempt.params["sandbox"], "read-only");
                        assert_eq!(attempt.params["approvalPolicy"], "on-request");
                    }
                    if fenced {
                        assert_eq!(options["permissionMode"], "bypassPermissions");
                        assert_eq!(options["env"]["CLAUDE_CODE_HOST_WORKTREE"], record["cwd"]);
                        let fence: Vec<String> = serde_json::from_str(
                            options["env"]["CLAUDE_CODE_HOST_WORKTREE_FENCE"].as_str().expect("JSON fence"))
                            .expect("fence array");
                        let expected = if mode == "image" {
                            vec!["/image source", "/parent copy", "/source checkout"]
                        } else { vec!["/parent copy", "/source checkout"] };
                        assert_eq!(fence, expected);
                        let mut current = snapshot.clone();
                        current.actor.prepare_attempt = Some(attempt.attempt_id.clone());
                        let receipt = thread::ProviderReceipt {
                            identity: attempt.receipt_identity(),
                            thread_id: Some("existing-thread".into()),
                            fields: Map::new(),
                            reattached: false,
                        };
                        assert!(matches!(thread::reduce_receipt(&attempt, &current, Some(&receipt), None, json!(1)),
                            thread::ReceiptOutcome::Accepted { .. }));
                        current.host_worktree.as_mut().expect("worker fence").fence.push("/changed-source".into());
                        assert_eq!(thread::reduce_receipt(&attempt, &current, Some(&receipt), None, json!(1)),
                            thread::ReceiptOutcome::Stale(thread::StaleReason::Settings));
                    } else {
                        assert!(options.get("env").is_none(), "{mode}/{provider}/{role}/{ready}: {options}");
                    }
                    assert_eq!(attempt.params["sandbox"], if provider == "claude" && role == "reviewer" {
                        "read-only"
                    } else {
                        "danger-full-access"
                    });
                }
            }
            Ok::<_, WorkerError>(())
        }).await.expect("production snapshot and provider params");
    }

    #[test]
    fn claude_workspace_fence_rejects_missing_or_relative_source() {
        let mut record = json!({"provider":"claude","role":"implementer","parentId":"lead",
            "worktree":true,"worktreeReady":true,"cwd":"/worker","hostProjectPath":"relative"});
        assert!(host_worktree(&record, &json!({}), thread::Role::Implementer, false).is_err());
        record["hostProjectPath"] = Value::Null;
        assert!(host_worktree(&record, &json!({}), thread::Role::Implementer, false).is_err());
        // A lead's workspace preference must not turn it into a fenced worker.
        assert!(
            host_worktree(&record, &json!({}), thread::Role::Implementer, true)
                .expect("lead")
                .is_none()
        );
    }
}
