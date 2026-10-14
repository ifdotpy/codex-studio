//! One checked boundary for the JSON records consumed by pure notification rules.
use super::{NotificationDispatchError, stored};
use serde_json::Value;

pub(super) struct NotificationSnapshot {
    agent: Value,
    task: Option<Value>,
    item: Option<Value>,
}
impl NotificationSnapshot {
    pub(super) fn from_records(
        agent: Value,
        task: Option<Value>,
        item: Option<Value>,
    ) -> Result<Self, NotificationDispatchError> {
        checked("runtime_agents", &agent)?;
        if let Some(record) = &task {
            checked("runtime_tasks", record)?;
        }
        if let Some(record) = &item {
            checked("runtime_items", record)?;
        }
        Ok(Self { agent, task, item })
    }
    pub(super) fn agent(&self) -> &Value {
        &self.agent
    }
    pub(super) fn task(&self) -> Option<&Value> {
        self.task.as_ref()
    }
    pub(super) fn item(&self) -> Option<&Value> {
        self.item.as_ref()
    }
}
pub(super) fn checked(
    table: &'static str,
    record: &Value,
) -> Result<(), NotificationDispatchError> {
    let id = record["id"].as_str().unwrap_or("<missing>");
    let schema = studio_runtime_schema::table_schema(table).ok_or_else(|| stored(table, id))?;
    if !studio_runtime_schema::validate(schema, record).map_err(|_| stored(table, id))? {
        return Err(stored(table, id));
    }
    Ok(())
}
