use axum::extract::{FromRequest, FromRequestParts};
use axum::http::request::Parts;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::sync::Arc;

use crate::error::ApiError;
use crate::validation::ValidatedRequest;

#[allow(dead_code, reason = "shared typed extractors consume serde paths")]
fn serde_location(section: &str, path: &serde_path_to_error::Path) -> Vec<Value> {
    let mut location = vec![Value::String(section.to_owned())];
    location.extend(path.iter().filter_map(|segment| match segment {
        serde_path_to_error::Segment::Seq { index } => Some(Value::String(index.to_string())),
        serde_path_to_error::Segment::Map { key } => Some(Value::String(key.clone())),
        serde_path_to_error::Segment::Enum { variant } => Some(Value::String(variant.clone())),
        serde_path_to_error::Segment::Unknown => None,
    }));
    location
}

/// JSON request bodies validated against their registered OpenAPI schema.
#[allow(dead_code, reason = "route handlers consume the shared JSON extractor")]
pub(crate) struct ApiJson<T>(pub T);

impl<T, S> FromRequest<S> for ApiJson<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(
        request: axum::extract::Request,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let (mut parts, _) = request.into_parts();
        let value = parts
            .extensions
            .remove::<ValidatedRequest>()
            .ok_or_else(ApiError::internal)?
            .body
            .ok_or_else(ApiError::internal)?;
        match serde_path_to_error::deserialize(value) {
            Ok(value) => Ok(Self(value)),
            Err(error) => Err(ApiError::validation(serde_location("body", error.path()))),
        }
    }
}

/// Optional JSON bodies validated by the shared boundary.
#[cfg(test)]
pub(crate) struct ApiJsonOptional<T>(pub Option<T>);

#[cfg(test)]
impl<T, S> FromRequest<S> for ApiJsonOptional<T>
where
    T: DeserializeOwned + Send,
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request(
        request: axum::extract::Request,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let (parts, _) = request.into_parts();
        let Some(value) = parts
            .extensions
            .get::<ValidatedRequest>()
            .ok_or_else(ApiError::internal)?
            .body
            .clone()
        else {
            return Ok(Self(None));
        };
        serde_path_to_error::deserialize(value)
            .map(|value| Self(Some(value)))
            .map_err(|error| ApiError::validation(serde_location("body", error.path())))
    }
}

/// Schema-validated scalar header parameters.
pub(crate) struct ApiHeaders(pub Arc<serde_json::Map<String, Value>>);

impl<S> FromRequestParts<S> for ApiHeaders
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<ValidatedRequest>()
            .map(|validated| Self(validated.headers.clone()))
            .ok_or_else(ApiError::internal)
    }
}

/// Raw query pairs plus values validated and typed from the registered schema.
#[allow(dead_code, reason = "route modules consume shared query extraction")]
pub(crate) struct QueryParams {
    typed: Arc<serde_json::Map<String, Value>>,
}

impl QueryParams {
    #[allow(dead_code, reason = "route modules consume schema-typed query values")]
    pub(crate) fn typed_value(&self, name: &str) -> Option<&Value> {
        self.typed.get(name)
    }

    pub(crate) fn nonempty_str(&self, name: &str) -> Option<&str> {
        self.typed_value(name)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty())
    }

    pub(crate) fn into_typed(self) -> Arc<serde_json::Map<String, Value>> {
        self.typed
    }
}

impl<S> FromRequestParts<S> for QueryParams
where
    S: Send + Sync,
{
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        let typed = parts
            .extensions
            .get::<ValidatedRequest>()
            .ok_or_else(ApiError::internal)?
            .query
            .clone();
        Ok(Self { typed })
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods)]
mod tests {
    use axum::body::{Body, to_bytes};
    use axum::http::{Request, StatusCode, header};
    use axum::middleware::{Next, from_fn};
    use axum::routing::{get, post};
    use axum::{Json, Router};
    use serde::Deserialize;
    use serde_json::{Value, json};
    use std::sync::Arc;
    use tower::ServiceExt;

    use crate::validation::ValidatedRequest;

    use super::{ApiJson, QueryParams};

    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct CountBody {
        count: u32,
    }

    async fn json_handler(ApiJson(body): ApiJson<CountBody>) -> Json<Value> {
        Json(json!({"count": body.count}))
    }

    async fn query_handler(params: QueryParams) -> Json<Value> {
        Json(json!({"typed": params.typed_value("value")}))
    }

    async fn response(router: Router, request: Request<Body>) -> (StatusCode, Value) {
        let response = router.oneshot(request).await.expect("router responds");
        let status = response.status();
        let bytes = to_bytes(response.into_body(), usize::MAX)
            .await
            .expect("response body reads");
        (
            status,
            serde_json::from_slice(&bytes).expect("response is JSON"),
        )
    }

    fn request(uri: &str, body: Body) -> Request<Body> {
        Request::builder()
            .uri(uri)
            .body(body)
            .expect("request builds")
    }

    fn with_validated(router: Router, validated: ValidatedRequest) -> Router {
        router.layer(from_fn(move |mut request: Request<Body>, next: Next| {
            let validated = validated.clone();
            async move {
                request.extensions_mut().insert(validated);
                next.run(request).await
            }
        }))
    }

    #[tokio::test]
    async fn query_params_expose_only_the_validated_value() {
        let app = with_validated(
            Router::new().route("/items", get(query_handler)),
            ValidatedRequest {
                query: Arc::new(serde_json::Map::from_iter([("value".to_owned(), json!(1))])),
                ..ValidatedRequest::default()
            },
        );
        let (status, value) = response(app, request("/items?value=999", Body::empty())).await;

        assert_eq!(status, StatusCode::OK);
        assert_eq!(value, json!({"typed":1}));

        let app = with_validated(
            Router::new().route("/items", get(query_handler)),
            ValidatedRequest::default(),
        );
        let (status, value) = response(app, request("/items?value=999", Body::empty())).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(value, json!({"typed":null}));
    }

    #[tokio::test]
    async fn wrongly_typed_json_uses_one_located_error() {
        let app = with_validated(
            Router::new().route("/items", post(json_handler)),
            ValidatedRequest {
                body: Some(json!({"count":"wrong"})),
                ..ValidatedRequest::default()
            },
        );
        let request = Request::builder()
            .method("POST")
            .uri("/items")
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(r#"{"count":"wrong"}"#))
            .expect("request builds");
        let (status, value) = response(app, request).await;

        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(value["error"], "Invalid request");
        assert_eq!(value["location"], json!(["body", "count"]));
    }
}
inventory::submit! {
    crate::routes::manifest::RouteModuleRegistration {
        name: "extractors",
        routes: None,
        dependencies: &[],
    }
}
