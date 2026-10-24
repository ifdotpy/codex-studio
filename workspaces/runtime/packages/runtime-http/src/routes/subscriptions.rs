//! Resource-kind ownership for screen subscriptions. Wire transports dispatch
//! here; domain services publish changes without session or subscriber knowledge.

use futures_util::future::BoxFuture;

use crate::error::ApiError;
use crate::generated::{ResourceRef, ResourceRevisionEntry, SubscriptionCursor};
use crate::router::HttpState;

/// Each generated resource kind has exactly one registered handler. A handler
/// reads its records and durable cursor in one database snapshot. The byte bound
/// includes its encoded payload; oversized data uses scoped recovery.
pub(crate) trait ResourceKindHandler: Send + Sync {
    fn accepts(&self, resource: &ResourceRef) -> bool;

    fn initial_snapshot(
        &self,
        state: HttpState,
        resource: ResourceRef,
        since: Option<SubscriptionCursor>,
    ) -> BoxFuture<'_, Result<ResourceRevisionEntry, ApiError>>;

    fn matches_change(&self, resource: &ResourceRef, changed: &ResourceRef) -> bool;

    /// Encoded payload bound in bytes; shared stream limit remains its owner.
    fn max_bytes(&self) -> usize;

    /// Diagnostic key only: no caller may parse this back into a reference.
    fn debug_key(&self, resource: &ResourceRef) -> String;
}
