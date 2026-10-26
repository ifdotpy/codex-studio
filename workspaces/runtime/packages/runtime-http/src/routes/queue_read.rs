//! Shared queue projection used by both queue reads and writes.

use crate::error::ApiError;
use axum::http::StatusCode;
use studio_runtime_services::queue_write::QueueWriteError;

pub(super) use studio_runtime_services::queue_write::{is_live, queue_snapshot};

pub(super) fn api_error(error: QueueWriteError) -> ApiError {
    match error {
        QueueWriteError::InvalidRequest(message) | QueueWriteError::Conflict(message) => {
            ApiError::new(StatusCode::BAD_REQUEST, message)
        }
        QueueWriteError::InvalidStoredObject { object, id } => {
            ApiError::invalid_stored(object, &id)
        }
        QueueWriteError::RemoteEventsUnavailable => ApiError::new(
            StatusCode::NOT_IMPLEMENTED,
            "Not implemented: requires cross-server/remote-events",
        ),
        QueueWriteError::Store(error) => ApiError::stored(error),
        QueueWriteError::Json(_) => ApiError::internal(),
    }
}

#[cfg(test)]
mod tests {
    use super::api_error;
    use crate::error::{InternalErrorCause, InternalErrorKind};
    use axum::http::StatusCode;
    use axum::response::IntoResponse;
    use studio_runtime_services::queue_write::QueueWriteError;
    use studio_runtime_store::StoreError;

    #[test]
    fn queue_store_mapping_keeps_database_failures_internal() {
        let malformed = api_error(QueueWriteError::Store(StoreError::InvalidStoredObject {
            table: "runtime_agents",
            id: "deleted-owner".to_owned(),
        }))
        .into_response();
        assert_eq!(malformed.status(), StatusCode::SERVICE_UNAVAILABLE);

        let database_failure = api_error(QueueWriteError::Store(StoreError::Sqlite(
            rusqlite::Error::InvalidQuery,
        )))
        .into_response();
        assert_eq!(
            database_failure
                .extensions()
                .get::<InternalErrorCause>()
                .map(|cause| cause.0),
            Some(InternalErrorKind::StoreDatabase)
        );
    }
}

inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "queue_read",
        routes: None,
        dependencies: &["event-driver/annotations", "event-driver/plan-updates"],
    }
}
