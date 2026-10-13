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
    /// Resolved interpreter used to start this runtime, also embedded in guidance.
    pub python_executable: PathBuf,
}
impl PreparationResources {
    pub fn bundled(
        account_home: String,
        model_catalog: ModelCatalogService,
        python_executable: PathBuf,
    ) -> Result<Self, WorkerError> {
        if !python_executable.is_absolute() {
            return Err(refusal("preparation/resolved-python-required".into()));
        }
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
        let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../../..")
            .canonicalize()
            .map_err(|_| invalid("repository root"))?;
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
            python_executable,
            account_home,
        })
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
    tx: &Transaction<'_>,
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
pub(super) fn authorize(tx: &Transaction<'_>, intent: &TurnIntent) -> Result<(), WorkerError> {
    let agents = records(tx, "runtime_agents")?;
    let projects = records(tx, "runtime_projects")?;
    let mut resolved_paths = BTreeMap::new();
    for (rows, key) in [(&agents, "cwd"), (&projects, "path")] {
        for raw in rows.values() {
            let record: Value = serde_json::from_str(raw).map_err(|_| invalid(key))?;
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
    let progress = if lead {
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
        let script = resources
            .skills_root
            .parent()
            .and_then(Path::parent)
            .ok_or_else(|| invalid("skills root"))?
            .join("workspaces/runtime/apps/server/src/codex_progress_layout.py");
        let python = resources.python_executable.to_string_lossy();
        resources
            .progress_template
            .replace("{progress_file}", &path.to_string_lossy())
            .replace(
                "{layout_file}",
                &path
                    .with_file_name("PROGRESS.layout.json")
                    .to_string_lossy(),
            )
            .replace("{python}", &shell_quote(&python))
            .replace("{progress_script}", &shell_quote(&script.to_string_lossy()))
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
pub(super) fn thread_snapshot(
    tx: &Transaction<'_>,
    resources: &PreparationResources,
    record: &Value,
    connection: &str,
    state_root: &Path,
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
    let progress = state_root.join("progress").join(id);
    let progress_directory = (lead
        && progress.is_dir()
        && std::fs::canonicalize(&progress).ok().as_ref() == Some(&progress))
    .then(|| progress.to_string_lossy().into_owned());
    let admission = if record
        .get("accountTransferId")
        .is_some_and(|v| !v.is_null())
    {
        Some(thread::HoldReason::AccountTransfer)
    } else if record.get("contextRepair").is_some_and(|v| !v.is_null()) {
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
            provider: thread::Provider::Codex,
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
            claude_options: Map::new(),
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
    let all_assets = records(tx, "runtime_assets")?;
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
        let raw = all_assets
            .get(&id)
            .ok_or_else(|| refusal("Unknown attachment".into()))?;
        let v: Value = serde_json::from_str(raw).map_err(|_| invalid("asset"))?;
        let owner = text(&v, "agentId")?
            .or(text(&v, "agent")?)
            .ok_or_else(|| invalid("asset/agent"))?;
        let owner: Value =
            serde_json::from_str(agents.get(&owner).ok_or_else(|| invalid("asset/owner"))?)
                .map_err(|_| invalid("asset/owner"))?;
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
                preserve_complaint: flag(&e.metadata, "preserveComplaint")?,
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
    fn guidance_uses_the_resolved_runtime_interpreter() {
        let python = crate::agents_create::prepared_python(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("../../apps/server/src"),
        );
        let resources = PreparationResources::bundled(
            String::new(),
            ModelCatalogService::new(None),
            python.clone(),
        )
        .expect("runtime resources");
        let bundle = guidance(
            &resources,
            &json!({"id":"lead", "isLead":true}),
            Path::new("/state"),
        )
        .expect("progress guidance");
        assert!(
            bundle
                .progress
                .contains(&shell_quote(&python.to_string_lossy()))
        );
        assert!(
            PreparationResources::bundled(
                String::new(),
                ModelCatalogService::new(None),
                PathBuf::from("python3"),
            )
            .is_err()
        );
    }
}
