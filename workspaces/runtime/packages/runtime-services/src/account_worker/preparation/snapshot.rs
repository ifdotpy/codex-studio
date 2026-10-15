//! Checked projections of stored rows and immutable runtime preparation resources.
use super::*;
use crate::{model_catalog::ModelCatalogService, thread_prepare as thread, turn_prepare as turn};
use serde::{Deserialize, de::DeserializeOwned};
use serde_json::Map;
use std::path::{Path, PathBuf};
use studio_runtime_store::{AgentRecord, Transaction};

#[derive(Clone)]
pub struct PreparationResources {
    pub instructions: String,
    pub progress_template: String,
    pub thread_config: Map<String, Value>,
    pub tools: Vec<thread::ToolDefinition>,
    pub legacy_tools: Vec<thread::ToolDefinition>,
    pub skills_root: PathBuf,
    pub role_guidance: BTreeMap<PathBuf, String>,
    pub model_catalog: ModelCatalogService,
    pub account_home: String,
    pub server_source_root: PathBuf,
    pub claude_account_pin: Value,
    pub account_driver: Option<Arc<crate::account_driver::AccountDriver>>,
    /// The host's registry-backed session surface for preparation calls that
    /// need to coordinate another live account.
    pub account_session: Option<Arc<dyn crate::account_session_runtime::AccountSessionPort>>,
    /// Fakeable guest RPC for VM workspace preparation in the turn-start path.
    pub guest_rpc: Option<Arc<dyn crate::linux_vm_guest::GuestRpc>>,
}
impl PreparationResources {
    pub fn bundled(
        account_home: String,
        model_catalog: ModelCatalogService,
    ) -> Result<Self, WorkerError> {
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../..")
            .canonicalize()
            .map_err(|_| invalid("repository root"))?;
        Self::bundled_at(
            account_home,
            model_catalog,
            root.clone(),
            root.join("workspaces/runtime/apps/server/src"),
        )
    }

    pub fn bundled_at(
        account_home: String,
        model_catalog: ModelCatalogService,
        root: PathBuf,
        server_source_root: PathBuf,
    ) -> Result<Self, WorkerError> {
        #[derive(Deserialize)]
        struct Bundled {
            instructions: String,
            progress_template: String,
            thread_config: Map<String, Value>,
            tools: Vec<thread::ToolDefinition>,
        }
        let data: Bundled = decode(
            serde_json::from_str(include_str!("../preparation-resources.json"))
                .map_err(|_| invalid("preparation resources"))?,
            "preparation resources",
        )?;
        let legacy_tools = decode(
            serde_json::from_str(include_str!(
                "../../../../../../../.agents/skills/codex-workspace/references/native-tools.json"
            ))
            .map_err(|_| invalid("legacy tools"))?,
            "legacy tools",
        )?;
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
        Ok(Self {
            role_guidance,
            instructions: data.instructions,
            progress_template: data.progress_template,
            thread_config: data.thread_config,
            tools: data.tools,
            legacy_tools,
            skills_root,
            model_catalog,
            account_home,
            server_source_root,
            claude_account_pin: Value::Null,
            account_driver: None,
            account_session: None,
            guest_rpc: None,
        })
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
pub(super) fn records(
    tx: &rusqlite::Connection,
    table: &str,
) -> Result<BTreeMap<String, String>, WorkerError> {
    if !tx.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name=?1 AND type='table')",
        [table],
        |r| r.get::<_, bool>(0),
    )? {
        return Ok(BTreeMap::new());
    }
    let mut query = tx.prepare(&format!("SELECT id,record FROM {table} ORDER BY rowid"))?;
    Ok(query
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?
        .collect::<Result<_, _>>()?)
}
/// Filesystem observations are captured on the preparation executor, before SQL.
/// Authorization still validates current stored owners in its writer transaction.
#[derive(Clone)]
pub(super) struct FilesystemSnapshot {
    resolved_paths: BTreeMap<String, String>,
    pub progress_directory: Option<String>,
}
impl FilesystemSnapshot {
    pub fn capture(store: &RuntimeStore, id: &str) -> Result<Self, WorkerError> {
        let db = store.read()?;
        let agents = records(&db, "runtime_agents")?;
        let projects = records(&db, "runtime_projects")?;
        drop(db);
        let mut resolved_paths = BTreeMap::new();
        for (rows, key) in [(&agents, "cwd"), (&projects, "path")] {
            for raw in rows.values() {
                let record: Value = serde_json::from_str(raw).map_err(|_| invalid(key))?;
                if key == "cwd" && record.get("executionMode").and_then(Value::as_str) == Some("vm")
                {
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
                    let resolved = crate::projects::resolve_nonstrict(Path::new(path))
                        .map_err(|_| invalid(key))?;
                    resolved_paths.insert(path.into(), resolved.to_string_lossy().into_owned());
                }
            }
        }
        let progress = store.root().join("progress").join(id);
        let progress_directory = (progress.is_dir()
            && std::fs::canonicalize(&progress).ok().as_ref() == Some(&progress))
        .then(|| progress.to_string_lossy().into_owned());
        Ok(Self {
            resolved_paths,
            progress_directory,
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
    let agents = records(tx, "runtime_agents")?;
    let projects = records(tx, "runtime_projects")?;
    let resolved_paths = filesystem.resolved_paths.clone();
    let rows = crate::submit_gates::AuthorizationRecords {
        recipient_id: intent.agent_id.clone(),
        agents,
        projects,
        resolved_paths,
        rooms: records(tx, "runtime_rooms")?,
        complaints: records(tx, "runtime_complaints")?,
        event_meta: records(tx, "runtime_event_meta")?,
        events: intent
            .events
            .iter()
            .filter(|event| !(intent.radio_authorized && event.kind == "radio_turn"))
            .map(|e| crate::submit_gates::StoredEvent {
                id: e.id.clone(),
                kind: e.kind.clone(),
                text: e.text.clone(),
            })
            .collect(),
    };
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
    guidance_with_python(
        resources,
        record,
        state_root,
        crate::python_tool::resolve_python_tool(),
    )
}

fn guidance_with_python(
    resources: &PreparationResources,
    record: &Value,
    state_root: &Path,
    python: Option<PathBuf>,
) -> Result<thread::GuidanceBundle, WorkerError> {
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
        format!(
            "[Studio role skill: {name}]\nSource: {}\nShared tool guidance: {}\n{}\n[End Studio role skill]",
            path.display(),
            resources
                .skills_root
                .join("codex-workspace/SKILL.md")
                .display(),
            content.trim()
        )
    };
    let progress = if lead && !resources.progress_template.is_empty() {
        let id = record["id"].as_str().ok_or_else(|| invalid("agent/id"))?;
        if id.is_empty()
            || id.len() > 200
            || !id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
        {
            return Err(invalid("progress agent"));
        }
        let path = state_root.join("progress").join(id).join("PROGRESS.md");
        let progress = resources
            .progress_template
            .replace("{progress_file}", &path.to_string_lossy())
            .replace(
                "{layout_file}",
                &path
                    .with_file_name("PROGRESS.layout.json")
                    .to_string_lossy(),
            );
        let layout_guidance = progress_layout_guidance(
            python,
            &resources
                .server_source_root
                .join("codex_progress_layout.py"),
            &path,
        );
        progress.replace("{progress_layout_check}", &layout_guidance)
    } else {
        String::new()
    };
    Ok(thread::GuidanceBundle {
        instructions: resources.instructions.clone(),
        role,
        progress,
        thread_config: resources.thread_config.clone(),
    })
}

fn progress_layout_guidance(
    python: Option<PathBuf>,
    script: &Path,
    progress_file: &Path,
) -> String {
    python.map_or_else(
            String::new,
            |python| {
                format!(
                    "After editing PROGRESS.md, run {} {} {} --wait 3 once to check which top-level lines or list items are visible. Trim only when important lines are hidden. Exit 0 means the current revision is visible, including clipped content (or is empty); 1 means unsupported content or a file-read error; 2 means no current measurement exists. The hint describes the least visible current client. Exact revisions and recent clients remain separate. Do not loop or wake another agent. ",
                    shell_quote(&python.to_string_lossy()),
                    shell_quote(&script.to_string_lossy()),
                    shell_quote(&progress_file.to_string_lossy()),
                )
            },
        )
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

pub(super) fn thread_snapshot(
    tx: &Transaction<'_>,
    resources: &PreparationResources,
    record: &Value,
    connection: &str,
    state_root: &Path,
    progress_directory: Option<String>,
) -> Result<thread::Snapshot, WorkerError> {
    let id = record["id"].as_str().ok_or_else(|| invalid("agent/id"))?;
    let root_id = record["rootId"]
        .as_str()
        .ok_or_else(|| invalid("agent/rootId"))?;
    let root = agent(tx, root_id)?.value();
    let source: thread::CatalogSource =
        optional(record, "nativeToolSource")?.unwrap_or(if record["threadId"].is_string() {
            thread::CatalogSource::Legacy
        } else {
            thread::CatalogSource::Current
        });
    let definitions = if source == thread::CatalogSource::Legacy {
        resources.legacy_tools.clone()
    } else {
        resources.tools.clone()
    };
    let lead = flag(record, "isLead")?;
    let role = optional(record, "role")?.unwrap_or(if lead {
        thread::Role::Orchestrator
    } else {
        thread::Role::Implementer
    });
    let progress_directory = lead.then_some(progress_directory).flatten();
    let admission = if record
        .get("accountTransferId")
        .is_some_and(|v| !v.is_null())
    {
        Some(thread::HoldReason::AccountTransfer)
    } else if context_repair_blocked(record) {
        Some(thread::HoldReason::ContextRepair)
    } else if record.get("nativeRelease").is_some_and(|v| {
        !v.is_null() && !matches!(v["phase"].as_str(), Some("resumed" | "released"))
    }) {
        Some(thread::HoldReason::NativeRelease)
    } else {
        None
    };
    let unsupported = if flag(record, "nativeReview")? {
        Some(thread::Refusal::NativeReview)
    } else if record
        .get("frozenNativeParams")
        .is_some_and(|v| !v.is_null())
    {
        Some(thread::Refusal::FrozenImport)
    } else if record.get("nativeToolUpdate").is_some_and(|v| !v.is_null()) {
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
        },
        account: thread::Account {
            key: text(record, "accountKey")?.unwrap_or_else(|| "default".into()),
            provider: match record["provider"].as_str().unwrap_or("codex") {
                "codex" => thread::Provider::Codex,
                "claude" => thread::Provider::Claude,
                _ => return Err(invalid("agent/provider")),
            },
            connection_id: connection.into(),
            available: true,
            supervisor_identity: None,
        },
        root: thread::Root {
            can_delegate: can_delegate(tx, record, &root)?,
            federation_enabled: records(tx, "runtime_federation_settings")?
                .get("global")
                .map(|raw| {
                    serde_json::from_str::<Value>(raw).map_err(|_| invalid("federation settings"))
                })
                .transpose()?
                .is_some_and(|v| v["enabled"] == true),
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
        workspace: thread::WorkspaceReadiness::Ready { progress_directory },
        unsupported,
        admission,
        pending: None,
        loaded: None,
        closed: false,
    })
}

pub(super) fn submission_snapshot(
    tx: &Transaction<'_>,
    resources: &PreparationResources,
    record: &Value,
    intent: &TurnIntent,
    state_root: &Path,
) -> Result<(turn::PreparationSnapshot, Vec<turn::Event>), WorkerError> {
    let root_id = record["rootId"]
        .as_str()
        .ok_or_else(|| invalid("agent/rootId"))?;
    let root = agent(tx, root_id)?.value();
    let guidance = guidance(resources, record, state_root)?;
    let plans = records(tx, "runtime_plans")?;
    let plan=plans.get(root_id).map(|s|serde_json::from_str::<Value>(s).map_err(|_|invalid("plan"))).transpose()?.filter(|v|v["native"].is_object()).map(|v|decode(json!({"id":v["id"],"rootId":v["rootId"],"steps":v["native"].get("plan").cloned().unwrap_or_else(||json!([])),"explanation":v["native"].get("explanation").filter(|v|!v.is_null()).cloned().unwrap_or(json!(""))}),"plan")).transpose()?;
    let mut complaints = Vec::new();
    for raw in records(tx, "runtime_complaints")?.values() {
        let v: Value = serde_json::from_str(raw).map_err(|_| invalid("complaint"))?;
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
    let mut workers = Vec::new();
    let agents = records(tx, "runtime_agents")?;
    for raw in agents.values() {
        let v: Value = serde_json::from_str(raw).map_err(|_| invalid("worker"))?;
        if v["rootId"] != root_id {
            continue;
        }
        let status = match v["status"].as_str() {
            Some("completed") => turn::WorkerStatus::Completed,
            Some("failed") => turn::WorkerStatus::Failed,
            Some("interrupted") => turn::WorkerStatus::Interrupted,
            Some("paused") => turn::WorkerStatus::Paused,
            Some(_) => turn::WorkerStatus::Active,
            None => return Err(invalid("worker/status")),
        };
        workers.push(turn::Worker {
            id: decode(v["id"].clone(), "worker/id")?,
            root_id: root_id.into(),
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
    ensure_context_tables(tx)?;
    let mut stmt = tx.prepare(
        "SELECT record FROM runtime_context_manifests WHERE agent=?1 ORDER BY sequence DESC",
    )?;
    let delivered_manifests = stmt
        .query_map([&intent.agent_id], |r| r.get::<_, String>(0))?
        .map(|r| {
            let r = r?;
            decode(
                serde_json::from_str(&r).map_err(|_| invalid("manifest"))?,
                "manifest",
            )
        })
        .collect::<Result<Vec<_>, WorkerError>>()?;
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
            delivered_reminders,
            assets,
            guidance: turn::GuidanceBundle {
                role: guidance.role,
                progress: guidance.progress,
            },
            artifact_paths: BTreeMap::new(),
            prepared_at: now(),
            operation_id: String::new(),
        },
        events,
    ))
}
pub(super) fn ensure_context_tables(tx: &Transaction<'_>) -> Result<(), WorkerError> {
    tx.execute_batch("CREATE TABLE IF NOT EXISTS runtime_context_manifests(agent TEXT NOT NULL,thread_id TEXT NOT NULL,compactions INTEGER NOT NULL,sequence INTEGER NOT NULL,event_id TEXT NOT NULL,record TEXT NOT NULL,PRIMARY KEY(agent,thread_id,compactions)); CREATE TABLE IF NOT EXISTS runtime_context_reminders(agent TEXT NOT NULL,version TEXT NOT NULL,event_id TEXT NOT NULL,PRIMARY KEY(agent,version));")?;
    Ok(())
}

fn shell_quote(value: &str) -> String {
    if !value.is_empty()
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"_@%+=:,./-".contains(&b))
    {
        value.into()
    } else {
        format!("'{}'", value.replace('\'', "'\"'\"'"))
    }
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
    use super::*;

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

    #[test]
    fn guidance_includes_the_optional_python_layout_tool() {
        let python = crate::agents_create::prepared_python(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
        let resources =
            PreparationResources::bundled(String::new(), ModelCatalogService::new(None))
                .expect("runtime resources");
        let bundle = guidance_with_python(
            &resources,
            &json!({"id":"lead", "isLead":true}),
            Path::new("/state"),
            Some(python.clone()),
        )
        .expect("progress guidance");
        assert!(
            bundle
                .progress
                .contains(&shell_quote(&python.to_string_lossy()))
        );
        let without_python = guidance_with_python(
            &resources,
            &json!({"id":"lead", "isLead":true}),
            Path::new("/state"),
            None,
        )
        .expect("progress guidance without Python");
        assert!(!without_python.progress.contains("--wait 3"));
        assert!(without_python.progress.contains("Do not repeat rewrites"));
    }
}
