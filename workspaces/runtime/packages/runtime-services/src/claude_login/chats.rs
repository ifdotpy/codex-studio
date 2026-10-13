//! Idempotent post-login repair; historical input and transcript rows stay intact.
use super::{LoginError, ports::LoginChats};
use serde_json::{Value, json};
use std::sync::Arc;
use studio_runtime_store::{AgentRecord, AgentRepository, RuntimeStore, Table};

pub struct StoredChats(pub Arc<RuntimeStore>);
impl LoginChats for StoredChats {
    fn refresh(&self, account: &str) -> Result<(), LoginError> {
        self.0
            .transaction(|tx| {
                for record in Table::<AgentRecord>::new(tx).list()? {
                    if let Some(next) = refreshed_agent(&record.value(), account) {
                        AgentRepository::put(tx, &AgentRecord::from_value(next)?)?;
                    }
                }
                Ok(())
            })
            .map_err(|_| LoginError::Persistence)
    }
}
pub fn refreshed_agent(agent: &Value, account: &str) -> Option<Value> {
    use crate::event_write::truthy;
    if agent.get("accountKey").and_then(Value::as_str) != Some(account)
        || agent.get("provider") != Some(&json!("claude"))
        || agent.get("status") != Some(&json!("failed"))
        || ["deletedAt", "inFlight", "activeTools", "workspaceOperation"]
            .iter()
            .any(|k| truthy(agent.get(*k)))
    {
        return None;
    }
    let error = agent.get("error").unwrap_or(&Value::Null);
    let text = error
        .get("message")
        .and_then(Value::as_str)
        .or_else(|| error.as_str())
        .unwrap_or("");
    static ERROR: std::sync::OnceLock<Result<regex::Regex, regex::Error>> =
        std::sync::OnceLock::new();
    if !ERROR.get_or_init(||regex::Regex::new(r"(?i)oauth session expired|failed to authenticate|profile.s account changed|sign in with claude auth login")).as_ref().is_ok_and(|r|r.is_match(text)){return None;}
    let mut next = agent.clone();
    next["status"] = json!("idle");
    next["error"] = Value::Null;
    next.as_object_mut()?.remove("nativeStatus");
    Some(next)
}
