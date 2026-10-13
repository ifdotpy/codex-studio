//! Application service boundary for analytics and session-cost reads.

use serde_json::Value;
use studio_runtime_store::RuntimeStore;
pub use studio_runtime_store::{AnalyticsError, AnalyticsOptions, SessionCostError};

pub fn read(store: &RuntimeStore, options: &AnalyticsOptions) -> Result<Value, AnalyticsError> {
    studio_runtime_store::analytics_read(store, options)
}

pub fn export(store: &RuntimeStore, options: &AnalyticsOptions) -> Result<Vec<u8>, AnalyticsError> {
    studio_runtime_store::analytics_export(store, options)
}

pub fn persisted_session_cost(
    store: &RuntimeStore,
    agent_id: &str,
) -> Result<Option<Value>, SessionCostError> {
    studio_runtime_store::persisted_session_cost(store, agent_id)
}
