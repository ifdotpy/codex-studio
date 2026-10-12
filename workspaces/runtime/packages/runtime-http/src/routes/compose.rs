use std::collections::{BTreeMap, BTreeSet};
use std::fmt;

use axum::Router;
use axum::routing::MethodRouter;

use crate::router::HttpState;

use super::manifest::{Operation, RouteGroup};

#[derive(Debug, Eq, PartialEq)]
pub(crate) enum RegistrationError {
    #[allow(
        dead_code,
        reason = "authority decoding failures are propagated during startup"
    )]
    InvalidAuthority,
    UndocumentedOperation {
        method: &'static str,
        path: &'static str,
    },
    DuplicateOperation {
        method: &'static str,
        path: &'static str,
    },
    EmptyDependencies,
    UnknownDependency(&'static str),
}

impl fmt::Display for RegistrationError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::InvalidAuthority => formatter.write_str("committed OpenAPI authority is invalid"),
            Self::UndocumentedOperation { method, path } => write!(
                formatter,
                "registered operation {method} {path} is absent from OpenAPI"
            ),
            Self::DuplicateOperation { method, path } => {
                write!(formatter, "operation {method} {path} is registered twice")
            }
            Self::EmptyDependencies => formatter.write_str("partial operation has no dependencies"),
            Self::UnknownDependency(name) => {
                write!(
                    formatter,
                    "partial operation declares unknown dependency {name}"
                )
            }
        }
    }
}

impl std::error::Error for RegistrationError {}

pub(crate) fn validate(
    groups: &[RouteGroup],
    authority: &BTreeSet<(String, String)>,
) -> Result<Vec<Operation>, RegistrationError> {
    let mut operations = BTreeSet::new();
    let mut registered = Vec::new();
    for group in groups {
        for route in group.registrations() {
            let operation = route.operation();
            if !authority.contains(&(operation.method().to_owned(), operation.path().to_owned())) {
                return Err(RegistrationError::UndocumentedOperation {
                    method: operation.method(),
                    path: operation.path(),
                });
            }
            if !operations.insert((operation.method(), operation.path())) {
                return Err(RegistrationError::DuplicateOperation {
                    method: operation.method(),
                    path: operation.path(),
                });
            }
            operation.status().validate()?;
            registered.push(operation);
        }
    }
    registered.sort_unstable();
    Ok(registered)
}

pub(crate) fn compose(
    groups: Vec<RouteGroup>,
    authority: &BTreeSet<(String, String)>,
) -> Result<(Router<HttpState>, Vec<Operation>), RegistrationError> {
    let registered = validate(&groups, authority)?;
    let mut paths = BTreeMap::<&'static str, MethodRouter<HttpState>>::new();
    for registration in groups.into_iter().flat_map(RouteGroup::into_registrations) {
        let (operation, method_router) = registration.into_parts();
        match paths.remove(operation.path()) {
            Some(existing) => {
                paths.insert(operation.path(), existing.merge(method_router));
            }
            None => {
                paths.insert(operation.path(), method_router);
            }
        }
    }
    let router = paths
        .into_iter()
        .fold(Router::<HttpState>::new(), |router, (path, methods)| {
            router.route(path, methods)
        });
    Ok((router, registered))
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use axum::Json;
    use axum::http::{Method, Request, StatusCode};
    use serde_json::json;
    use tower::ServiceExt;

    use crate::schema::operations;

    use super::{RegistrationError, compose, validate};
    use crate::router::HttpState;
    use crate::routes::manifest::{OperationStatus, RouteGroup};

    async fn get_handler() -> Json<serde_json::Value> {
        Json(json!({"ok":true}))
    }

    async fn post_handler() -> Json<serde_json::Value> {
        Json(json!({"ok":true}))
    }

    fn authority() -> BTreeSet<(String, String)> {
        operations().expect("committed OpenAPI parses").clone()
    }

    #[test]
    fn rejects_a_route_whose_path_is_not_in_openapi() {
        let group = RouteGroup::new(vec![super::super::manifest::get(
            "/api/not-documented",
            OperationStatus::Implemented,
            get_handler,
        )]);

        assert!(matches!(
            validate(&[group], &authority()),
            Err(RegistrationError::UndocumentedOperation { .. })
        ));
    }

    #[test]
    fn rejects_a_path_registered_twice() {
        let first = RouteGroup::new(vec![super::super::manifest::get(
            "/api/session",
            OperationStatus::Implemented,
            get_handler,
        )]);
        let second = RouteGroup::new(vec![super::super::manifest::get(
            "/api/session",
            OperationStatus::Implemented,
            get_handler,
        )]);
        assert_eq!(
            validate(&[first, second], &authority()),
            Err(RegistrationError::DuplicateOperation {
                method: "GET",
                path: "/api/session",
            })
        );
    }

    #[tokio::test]
    async fn registration_builds_a_handler_and_its_manifest_entry_together() {
        let group = RouteGroup::new(vec![
            super::super::manifest::get(
                "/api/accounts/claude/login",
                OperationStatus::Implemented,
                get_handler,
            ),
            super::super::manifest::post(
                "/api/accounts/claude/login",
                OperationStatus::Implemented,
                post_handler,
            ),
        ]);
        let (router, entries) = compose(vec![group], &authority()).expect("compose");
        assert_eq!(entries.len(), 2);

        let state = HttpState::new(
            "token".to_owned(),
            "workspace".to_owned(),
            None,
            "state".to_owned(),
            1,
            None,
        );
        let app = router.with_state(state);
        for (method, expected) in [(Method::GET, 200), (Method::POST, 200)] {
            let request = Request::builder()
                .method(method)
                .uri("/api/accounts/claude/login")
                .body(axum::body::Body::empty())
                .expect("request builds");
            assert_eq!(
                app.clone()
                    .oneshot(request)
                    .await
                    .expect("response")
                    .status(),
                StatusCode::from_u16(expected).expect("status")
            );
        }
    }
}
