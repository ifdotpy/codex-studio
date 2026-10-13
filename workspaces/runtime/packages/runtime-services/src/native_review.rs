//! Native review children use the Python request-key UUID, never a random child
//! on retry. Workspace/catalog checks stay with the ordinary creation owner.
use crate::native_actions::{NativeError, Result, bounded, checked_agent, decode};
use rusqlite::Transaction;
use serde::Deserialize;
use serde_json::{Value, json};
use studio_runtime_store::{AgentRecord, AgentRepository, ContextRepairRepository};

const MAX_PROMPT: usize = 32_000;
const DELIVERY: &str = "The review result wakes you automatically. Finish your turn while waiting.";

#[derive(Deserialize)]
#[serde(tag = "type", deny_unknown_fields)]
enum Target {
    #[serde(rename = "uncommittedChanges")]
    UncommittedChanges,
    #[serde(rename = "baseBranch")]
    BaseBranch { branch: String },
    #[serde(rename = "commit")]
    Commit { sha: String, title: Option<String> },
    #[serde(rename = "custom")]
    Custom { instructions: String },
}

/// Preserve the original target (including absent versus null title) after
/// validating its closed variants. The complete target stays in nativeReview.
pub fn target(args: &Value) -> Result<Value> {
    let object = args.as_object().ok_or(NativeError::Invalid("review"))?;
    if object.keys().any(|k| {
        !matches!(
            k.as_str(),
            "target" | "cwd" | "model" | "effort" | "request_id"
        )
    }) {
        return Err(NativeError::Invalid("review"));
    }
    for key in ["model", "effort", "request_id", "cwd"] {
        if let Some(value) = object.get(key) {
            let text = value
                .as_str()
                .ok_or(NativeError::Invalid("review.options"))?;
            let limit = if key == "cwd" { 4096 } else { 200 };
            let invalid_request_id = key == "request_id"
                && (text.trim() != text || text.chars().any(|character| character <= '\u{1f}'));
            if text.trim().is_empty()
                || text.chars().count() > limit
                || text.contains('\0')
                || invalid_request_id
            {
                return Err(NativeError::Invalid("review.options"));
            }
        }
    }
    let raw = args
        .get("target")
        .cloned()
        .unwrap_or(json!({"type":"uncommittedChanges"}));
    match decode::<Target>(raw.clone(), "review.target")? {
        Target::UncommittedChanges => {}
        Target::BaseBranch { branch } => text(&branch, 1000)?,
        Target::Commit { sha, title } => {
            text(&sha, 1000)?;
            if title.is_some_and(|v| v.chars().count() > 1000 || v.contains('\0')) {
                return Err(NativeError::Invalid("review.target.title"));
            }
        }
        Target::Custom { instructions } => text(&instructions, MAX_PROMPT)?,
    }
    Ok(raw)
}
fn text(value: &str, limit: usize) -> Result<()> {
    if value.trim().is_empty() || value.trim().chars().count() > limit || value.contains('\0') {
        return Err(NativeError::Invalid("review.target"));
    }
    Ok(())
}
pub fn worker_id(key: &str) -> String {
    uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_URL, key.as_bytes()).to_string()
}

pub struct ReviewPlan {
    actor: Value,
    root: Value,
    key: String,
    target: Value,
    args: Value,
    spec: Value,
}
impl ReviewPlan {
    /// Directory has already been resolved and checked as a repository outside
    /// the transaction. Creation rechecks the actor and delegation on reservation.
    pub fn from_records(
        actor: Value,
        root: Value,
        args: Value,
        key: String,
        directory: String,
    ) -> Result<Self> {
        bounded(&actor, "review.actor")?;
        let checked = checked_agent(&actor)?;
        let checked_root = checked_agent(&root)?;
        if checked.root_id != checked_root.id {
            return Err(NativeError::Changed);
        }
        if actor.get("provider").is_some_and(|v| v != "codex")
            || crate::event_write::truthy(actor.get("nativeReview"))
        {
            return Err(NativeError::Invalid("review.actor"));
        }
        if actor["executionMode"] == "vm" || crate::event_write::truthy(actor.get("remoteOrigin")) {
            return Err(NativeError::Unavailable("native-review/remote-worker"));
        }
        if key.is_empty() || key.len() > 200 || directory.is_empty() {
            return Err(NativeError::Invalid("review.identity"));
        }
        let target = target(&args)?;
        let defaults = checked_root
            .extra
            .get("reviewDefaults")
            .cloned()
            .unwrap_or(json!({}));
        let model = args
            .get("model")
            .filter(|v| !v.is_null())
            .or_else(|| defaults.get("model").filter(|v| !v.is_null()))
            .unwrap_or(&actor["model"])
            .clone();
        let effort = if let Some(value) = args.get("effort") {
            value.clone()
        } else if args.get("model").is_none() && defaults.get("model").is_some_and(|v| !v.is_null())
        {
            defaults.get("effort").cloned().unwrap_or(Value::Null)
        } else if model == actor["model"] {
            actor.get("effort").cloned().unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        let requested = ["model", "effort"]
            .into_iter()
            .filter_map(|k| args.get(k).map(|v| (k.to_owned(), v.clone())))
            .collect::<serde_json::Map<_, _>>();
        let prompt = format!(
            "Run a native code review: {}",
            studio_runtime_store::python_json_utf8(&target)
        );
        let spec = json!({"id":worker_id(&key),"name":"Review","cwd":directory,"role":"reviewer",
            "prompt":prompt.chars().take(MAX_PROMPT).collect::<String>(),"model":model,"effort":effort,
            "fast_mode":actor.get("fastMode").unwrap_or(&json!(false)),"daybreak_enabled":false});
        Ok(Self {
            actor,
            root,
            key,
            target,
            args: Value::Object(requested),
            spec,
        })
    }
    pub fn spec(&self) -> &Value {
        &self.spec
    }
    pub fn existing(&self, child: &Value) -> Result<Value> {
        checked_agent(child)?;
        let review = &child["nativeReview"];
        let same_args = if let Some(saved) = review.get("args").filter(|v| !v.is_null()) {
            saved == &self.args
        } else {
            ["model", "effort"]
                .iter()
                .all(|k| self.args.get(*k).is_none_or(|v| child.get(*k) == Some(v)))
        };
        if child["id"] != self.spec["id"]
            || review["requestId"] != self.key
            || review["actorId"] != self.actor["id"]
            || review["target"] != self.target
            || child["cwd"] != self.spec["cwd"]
            || !same_args
        {
            return Err(NativeError::Conflict);
        }
        let response = &review["response"];
        if response["requestId"] != self.key
            || response["agentId"] != self.spec["id"]
            || response["status"] != "queued"
            || !response["reviewModel"].is_string()
            || !response["agents"].is_array()
            || !response["delivery"].is_string()
        {
            return Err(NativeError::Invalid("review.response"));
        }
        review
            .get("response")
            .filter(|v| v.is_object())
            .cloned()
            .ok_or(NativeError::Invalid("review.response"))
    }
}

/// Ordinary creation validates frozen catalog/workspace evidence, actor account
/// connection and cancellation here. It returns the complete unsaved child and
/// persists the tool result in this same transaction; neither method does I/O.
pub trait ReviewCreator {
    fn child(&self, tx: &Transaction<'_>, actor: &Value, spec: &Value) -> Result<Value>;
    fn receipt(&self, tx: &Transaction<'_>, key: &str, response: &Value) -> Result<()>;
}
pub fn reserve(
    tx: &Transaction<'_>,
    plan: &ReviewPlan,
    creator: &impl ReviewCreator,
) -> Result<Value> {
    let id = plan.spec["id"]
        .as_str()
        .ok_or(NativeError::Invalid("review.id"))?;
    if let Some(child) = ContextRepairRepository::agent(tx, id)? {
        return plan.existing(&child);
    }
    let actor_id = plan.actor["id"]
        .as_str()
        .ok_or(NativeError::Invalid("review.actor.id"))?;
    let actor = ContextRepairRepository::agent(tx, actor_id)?.ok_or(NativeError::Changed)?;
    if crate::event_write::truthy(actor.get("daybreakEnabled")) {
        return Err(NativeError::Invalid("review.actor"));
    }
    if actor != plan.actor {
        return Err(NativeError::Changed);
    }
    let root_id = actor["rootId"]
        .as_str()
        .ok_or(NativeError::Invalid("review.rootId"))?;
    let root = ContextRepairRepository::agent(tx, root_id)?.ok_or(NativeError::Changed)?;
    if root != plan.root {
        return Err(NativeError::Changed);
    }
    crate::event_write::assert_delegation(&checked_agent(&root)?)
        .map_err(|_| NativeError::Changed)?;
    let mut child = creator.child(tx, &actor, &plan.spec)?;
    if child["id"] != id
        || child["parentId"] != actor_id
        || child["rootId"] != actor["rootId"]
        || child["cwd"] != plan.spec["cwd"]
        || child["model"] != plan.spec["model"]
        || child["status"] != "queued"
    {
        return Err(NativeError::Changed);
    }
    child["yoloMode"] = json!(false);
    child["worktree"] = json!(false);
    let brief = ["id", "name", "status", "model", "effort", "fastMode"]
        .into_iter()
        .map(|k| (k.to_owned(), child.get(k).cloned().unwrap_or(Value::Null)))
        .collect::<serde_json::Map<_, _>>();
    let response = json!({"requestId":plan.key,"agentId":id,"status":"queued","reviewModel":child["model"],
        "reviewEffort":child.get("nativeEffort").unwrap_or(&child["effort"]),"agents":[brief],"delivery":DELIVERY});
    child["nativeReview"] = json!({"target":plan.target,"requestId":plan.key,"actorId":actor_id,
        "args":plan.args,"status":"pending","response":response});
    AgentRepository::put(tx, &AgentRecord::from_value(child)?)?;
    creator.receipt(tx, &plan.key, &response)?;
    Ok(response)
}
