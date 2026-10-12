use axum::routing::{MethodFilter, MethodRouter, on};
use serde::Serialize;

use super::compose::RegistrationError;
use crate::router::HttpState;

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub(crate) struct Dependency(&'static str);

/// Names use `<slice>/<capability>` and are removed when the slice that provides them merges.
const KNOWN_DEPENDENCIES: &[&str] = &[
    "coordination-policy/inbox",
    "cross-server/remote-events",
    "event-driver/annotations",
    "event-driver/plan-updates",
    "event-driver/queue-mutations",
    "event-driver/work-items",
    "monitor-driver/active-checks",
    "provider-session/account-rate-limits",
    "provider-session/model-list",
    "resource-policy/publish-drafts",
    "resource-policy/read-progress-panel",
    "resource-policy/windows-file-identity",
    "resource-policy/write-progress-layout",
    "rule-driver/event-rules",
    "store-bootstrap/workspace-profiles-schema",
    "sync-policy/entity-seeding",
    "sync-policy/entity-window-maintenance",
    "sync-policy/runtime-capabilities",
    "transcript-service/pull",
    "transcript/event-driver",
    "transcript/payload-store",
    "transcript/search-index",
    "transcript/workspace-state",
    "work-policy/views",
    "workspace-state/git-changes",
    "workspace-state/image-execution",
    "workspace-state/organization-effects",
    "workspace-state/snapshot",
    "coordination-policy/chat-history",
    "coordination-policy/room-visibility",
    "event-driver/message-receipts",
    "event-driver/message-send",
    "federation-driver/rooms",
    "agent-discovery/launcher-status",
];

impl Dependency {
    #[allow(
        dead_code,
        reason = "later route groups declare capability dependencies"
    )]
    pub(crate) const fn new(name: &'static str) -> Self {
        Self(name)
    }

    pub(super) const fn name(self) -> &'static str {
        self.0
    }

    fn is_known(self) -> bool {
        KNOWN_DEPENDENCIES.contains(&self.0)
    }
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub(crate) enum OperationStatus {
    Implemented,
    Partial { dependencies: &'static [Dependency] },
}

impl OperationStatus {
    #[allow(dead_code, reason = "later route groups declare partial operations")]
    pub(crate) const fn partial(dependencies: &'static [Dependency]) -> Self {
        Self::Partial { dependencies }
    }

    pub(super) fn validate(self) -> Result<(), RegistrationError> {
        match self {
            Self::Implemented => Ok(()),
            Self::Partial { dependencies: [] } => Err(RegistrationError::EmptyDependencies),
            Self::Partial { dependencies } => dependencies
                .iter()
                .find(|dependency| !dependency.is_known())
                .map_or(Ok(()), |dependency| {
                    Err(RegistrationError::UnknownDependency(dependency.name()))
                }),
        }
    }

    #[allow(
        dead_code,
        reason = "partial route slices use this to bind 501 behavior"
    )]
    fn matches_dependencies(self, dependencies: &[Dependency]) -> bool {
        matches!(self, Self::Partial { dependencies: declared } if declared == dependencies)
    }
}

#[allow(
    dead_code,
    reason = "partial route slices use this registered-operation helper"
)]
pub(crate) fn dependency_unavailable(
    method: &str,
    path: &str,
    dependencies: &[Dependency],
) -> Result<crate::error::ApiError, RegistrationError> {
    let declaration = super::groups()
        .into_iter()
        .flat_map(|group| group.registrations)
        .find(|route| route.operation.method == method && route.operation.path == path)
        .ok_or(RegistrationError::UndocumentedOperation {
            method: "GET",
            path: "/missing-registration",
        })?;
    if dependencies.is_empty()
        || dependencies.iter().any(|dependency| !dependency.is_known())
        || !declaration
            .operation
            .status
            .matches_dependencies(dependencies)
    {
        return Err(RegistrationError::UnknownDependency(
            dependencies
                .first()
                .map(|dependency| dependency.name())
                .unwrap_or("<empty>"),
        ));
    }
    Ok(crate::error::ApiError::new(
        axum::http::StatusCode::NOT_IMPLEMENTED,
        format!(
            "Not implemented: requires {}",
            dependencies
                .iter()
                .map(|dependency| dependency.name())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    ))
}

#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
pub(crate) struct Operation {
    method: &'static str,
    path: &'static str,
    #[serde(flatten)]
    status: OperationStatus,
}

impl Operation {
    fn new(method: &'static str, path: &'static str, status: OperationStatus) -> Self {
        Self {
            method,
            path,
            status,
        }
    }

    pub(crate) fn key(self) -> (String, String) {
        (self.method.to_owned(), self.path.to_owned())
    }

    pub(super) fn method(self) -> &'static str {
        self.method
    }
    pub(super) fn path(self) -> &'static str {
        self.path
    }
    pub(super) fn status(self) -> OperationStatus {
        self.status
    }
}

pub(crate) struct RouteRegistration {
    operation: Operation,
    method_router: MethodRouter<HttpState>,
}

impl RouteRegistration {
    pub(super) fn operation(&self) -> Operation {
        self.operation
    }
    pub(super) fn into_parts(self) -> (Operation, MethodRouter<HttpState>) {
        (self.operation, self.method_router)
    }
}

macro_rules! method_registration {
    ($function:ident, $method:literal, $filter:path) => {
        #[allow(
            dead_code,
            reason = "later route groups use method-specific registration"
        )]
        pub(crate) fn $function<H, T>(
            path: &'static str,
            status: OperationStatus,
            handler: H,
        ) -> RouteRegistration
        where
            H: axum::handler::Handler<T, HttpState>,
            T: 'static,
        {
            RouteRegistration {
                operation: Operation::new($method, path, status),
                method_router: on($filter, handler),
            }
        }
    };
}

method_registration!(get, "GET", MethodFilter::GET);
method_registration!(post, "POST", MethodFilter::POST);
method_registration!(put, "PUT", MethodFilter::PUT);
method_registration!(delete, "DELETE", MethodFilter::DELETE);
method_registration!(patch, "PATCH", MethodFilter::PATCH);
method_registration!(options, "OPTIONS", MethodFilter::OPTIONS);
method_registration!(head, "HEAD", MethodFilter::HEAD);
method_registration!(trace, "TRACE", MethodFilter::TRACE);

pub(crate) struct RouteGroup {
    registrations: Vec<RouteRegistration>,
}

impl RouteGroup {
    pub(crate) fn new(registrations: Vec<RouteRegistration>) -> Self {
        Self { registrations }
    }

    pub(super) fn into_registrations(self) -> Vec<RouteRegistration> {
        self.registrations
    }

    pub(super) fn registrations(&self) -> &[RouteRegistration] {
        &self.registrations
    }
}

#[cfg(test)]
pub(crate) fn operation_manifest() -> Result<Vec<Operation>, RegistrationError> {
    let groups = super::groups();
    let authority = crate::schema::operations().map_err(|_| RegistrationError::InvalidAuthority)?;
    super::compose::validate(&groups, authority)?;

    let mut implemented = groups
        .iter()
        .flat_map(|group| group.registrations.iter().map(RouteRegistration::operation))
        .collect::<Vec<_>>();
    implemented.sort_unstable();
    Ok(implemented)
}

#[cfg(test)]
mod tests {
    use super::{Dependency, OperationStatus, operation_manifest};

    #[test]
    fn print_operation_manifest() {
        let operations = operation_manifest().expect("route manifest validates");
        let json = serde_json::to_string(&operations).expect("route manifest serializes");
        println!("RUST_HTTP_OPERATION_MANIFEST={json}");
    }

    #[test]
    fn dependency_unavailability_requires_a_known_nonempty_list() {
        static DEPENDENCIES: [Dependency; 1] =
            [Dependency::new("provider-session/account-rate-limits")];
        assert!(
            OperationStatus::Partial {
                dependencies: &DEPENDENCIES,
            }
            .validate()
            .is_ok()
        );
        assert!(
            OperationStatus::Partial { dependencies: &[] }
                .validate()
                .is_err()
        );
        static UNKNOWN: [Dependency; 1] = [Dependency::new("unknown/capability")];
        assert!(
            OperationStatus::Partial {
                dependencies: &UNKNOWN,
            }
            .validate()
            .is_err()
        );
        assert!(super::dependency_unavailable("GET", "/api/limits", &DEPENDENCIES).is_ok());
        assert!(super::dependency_unavailable("GET", "/api/limits", &UNKNOWN).is_err());
    }
}
