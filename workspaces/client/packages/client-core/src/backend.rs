//! HTTP, credentials and request deadlines stay outside the UI runtime.
use crate::protocol::{CommandError, Request, Response};
use base64::Engine;
use reqwest::{Client, Url};
use serde::Deserialize;
use serde_json::Value;
use std::{sync::Arc, time::Duration};
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Backend {
    origin: Url,
    client: Client,
    credentials: Arc<Mutex<Option<String>>>,
    authorized: Arc<std::sync::atomic::AtomicBool>,
    remote: Option<crate::server_transport::RemoteBinding>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields, rename_all = "camelCase")]
struct ApiRequest {
    path: String,
    #[serde(default)]
    query: Option<Value>,
    #[serde(default)]
    method: Option<String>,
    #[serde(default)]
    request_id: Option<String>,
    #[serde(default)]
    api_schema: Option<String>,
    #[serde(default)]
    body: Option<Value>,
    #[serde(default)]
    workspace_id: Option<String>,
    #[serde(default)]
    timeout_ms: Option<u64>,
    #[serde(default)]
    etag: Option<String>,
    #[serde(default)]
    read_metadata: bool,
    #[serde(default)]
    download: bool,
    #[serde(default)]
    http_response: bool,
    #[serde(default)]
    json_response: bool,
}

fn error(code: &str, message: impl ToString) -> CommandError {
    CommandError {
        code: code.into(),
        message: message.to_string(),
        status: None,
        details: None,
    }
}

impl Backend {
    pub fn new(origin: &str) -> Result<Self, String> {
        let origin = Url::parse(origin).map_err(|e| e.to_string())?;
        // The desktop backend is local. UI commands cannot select a remote host.
        if origin.scheme() != "http"
            || !matches!(origin.host_str(), Some("127.0.0.1" | "[::1]"))
            || !origin.username().is_empty()
            || origin.password().is_some()
            || origin.path() != "/"
            || origin.query().is_some()
            || origin.fragment().is_some()
        {
            return Err("The backend origin must be a loopback HTTP URL.".into());
        }
        let client = crate::tls::client_builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .connect_timeout(Duration::from_secs(5))
            .build()
            .map_err(|e| e.to_string())?;
        Ok(Self {
            origin,
            client,
            credentials: Arc::new(Mutex::new(None)),
            authorized: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            remote: None,
        })
    }

    /// The native host selects an existing pairing, never a renderer URL or key.
    pub fn from_remote(binding: crate::server_transport::RemoteBinding) -> Result<Self, String> {
        let origin = Url::parse(binding.origin()).map_err(|e| e.to_string())?;
        Ok(Self {
            origin,
            client: crate::tls::client_builder()
                .redirect(reqwest::redirect::Policy::none())
                .retry(reqwest::retry::never())
                .build()
                .map_err(|e| e.to_string())?,
            credentials: Arc::new(Mutex::new(None)),
            authorized: Arc::new(std::sync::atomic::AtomicBool::new(true)),
            remote: Some(binding),
        })
    }

    pub fn validate_identity(&self) -> Result<(), String> {
        match &self.remote {
            Some(binding) => binding.validate(),
            None => Ok(()),
        }
    }

    /// The host-selected pairing identity. Local transport has no remote server.
    pub fn remote_server_id(&self) -> Option<&str> {
        self.remote
            .as_ref()
            .map(crate::server_transport::RemoteBinding::server_id)
    }

    /// Host-stamped transport metadata. No key or session token leaves this owner.
    pub fn public_identity(&self) -> Value {
        match &self.remote {
            Some(binding) => binding.public_identity(),
            None => {
                serde_json::json!({"origin":self.origin.origin().ascii_serialization(),"credentialId":null})
            }
        }
    }

    pub fn desktop_client(origin: &str) -> Result<Self, String> {
        let client = Self::new(origin)?;
        client
            .authorized
            .store(false, std::sync::atomic::Ordering::Release);
        Ok(client)
    }
    /// The native host calls this only after verifying its configured local server.
    pub fn authorize(&self) {
        self.authorized
            .store(true, std::sync::atomic::Ordering::Release);
    }
    pub(crate) fn port(&self) -> u16 {
        self.origin.port_or_known_default().unwrap()
    }
    pub(crate) async fn desktop_probe(
        &self,
        timeout: Duration,
    ) -> Result<reqwest::Response, reqwest::Error> {
        self.client
            .get(self.origin.join("/api/desktop").unwrap())
            .timeout(timeout)
            .send()
            .await
    }

    fn url(&self, path: &str) -> Result<Url, CommandError> {
        if !path.starts_with("/api/") || path.contains('\\') || path.contains('#') {
            return Err(error(
                "invalid_path",
                "The request must name a Studio API path.",
            ));
        }
        let url = self
            .origin
            .join(path)
            .map_err(|e| error("invalid_path", e))?;
        if url.origin() != self.origin.origin() || !url.path().starts_with("/api/") {
            return Err(error("invalid_path", "The request left the Studio API."));
        }
        Ok(url)
    }

    pub(crate) async fn event_stream(
        &self,
        path: &str,
        last_id: &str,
    ) -> Result<Option<reqwest::Response>, CommandError> {
        if !self.authorized.load(std::sync::atomic::Ordering::Acquire) {
            return Err(error(
                "backend_unverified",
                "The local backend identity has not been verified.",
            ));
        }
        let mut request = self
            .client
            .get(self.url(path)?)
            .header("Accept", "text/event-stream");
        if !last_id.is_empty() {
            request = request.header("Last-Event-ID", last_id);
        }
        let response = if let Some(remote) = &self.remote {
            remote
                .send(
                    "GET",
                    self.url(path)?.as_str(),
                    &serde_json::json!([
                        ["accept", "text/event-stream"],
                        ["last-event-id", last_id]
                    ]),
                    Vec::new(),
                    &crate::server_keys::random_id().map_err(|e| error("identity", e))?,
                )
                .await
                .map_err(|e| error("network", e))?
        } else {
            tokio::time::timeout(Duration::from_secs(15), request.send())
                .await
                .map_err(|_| error("timeout", "The server event stream did not open in time."))?
                .map_err(Self::network)?
        };
        if response.status().as_u16() == 204 {
            return Ok(None);
        }
        if response.status().as_u16() != 200 {
            return Err(CommandError {
                code: "http".into(),
                message: format!("The server event stream returned {}.", response.status()),
                status: Some(response.status().as_u16()),
                details: None,
            });
        }
        if response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .split(';')
            .next()
            .unwrap_or("")
            .trim()
            != "text/event-stream"
        {
            return Err(error(
                "invalid_response",
                "The server did not return an event stream.",
            ));
        }
        Ok(Some(response))
    }

    async fn decode(response: reqwest::Response) -> Result<Value, CommandError> {
        let status = response.status();
        let value = response.json::<Value>().await.map_err(|e| CommandError {
            code: "invalid_response".into(),
            message: e.to_string(),
            status: Some(status.as_u16()),
            details: None,
        })?;
        if !status.is_success() {
            let message = value
                .get("error")
                .or_else(|| value.get("message"))
                .and_then(Value::as_str)
                .or_else(|| value.as_str())
                .map(str::to_owned)
                .unwrap_or_else(|| format!("Request failed ({status})."));
            return Err(CommandError {
                code: "http".into(),
                message,
                status: Some(status.as_u16()),
                details: Some(value),
            });
        }
        Ok(value)
    }

    fn network(error_value: reqwest::Error) -> CommandError {
        if error_value.is_timeout() {
            error(
                "timeout",
                "The server did not respond in time. The command outcome may be unknown.",
            )
        } else {
            error("network", error_value)
        }
    }

    async fn token(&self) -> Result<String, CommandError> {
        if !self.authorized.load(std::sync::atomic::Ordering::Acquire) {
            return Err(error(
                "backend_unverified",
                "The local backend identity has not been verified.",
            ));
        }
        let mut cached = self.credentials.lock().await;
        if let Some(token) = &*cached {
            return Ok(token.clone());
        }
        let response = self
            .client
            .get(self.url("/api/session")?)
            .timeout(Duration::from_secs(15))
            .send()
            .await
            .map_err(Self::network)?;
        let response = if response.status().as_u16() == 404 {
            self.client
                .get(self.url("/api/state?view=chat")?)
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .map_err(Self::network)?
        } else {
            response
        };
        let value = Self::decode(response).await?;
        let token = value
            .get("token")
            .and_then(Value::as_str)
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                error(
                    "invalid_session",
                    "The backend did not return a session token.",
                )
            })?
            .to_owned();
        *cached = Some(token.clone());
        Ok(token)
    }

    pub async fn prepare_session(&self) -> Result<(), CommandError> {
        if let Some(remote) = &self.remote {
            return remote.validate().map_err(|e| error("server_identity", e));
        }
        self.token().await.map(|_| ())
    }

    async fn request(&self, request: ApiRequest) -> Result<Value, CommandError> {
        if !self.authorized.load(std::sync::atomic::Ordering::Acquire) {
            return Err(error(
                "backend_unverified",
                "The local backend identity has not been verified.",
            ));
        }
        let mut url = self.url(&request.path)?;
        if let Some(query) = &request.query {
            let query = query
                .as_object()
                .ok_or_else(|| error("invalid_query", "The API query must be an object."))?;
            for (key, value) in query {
                let values = if let Value::Array(values) = value {
                    values.as_slice()
                } else {
                    std::slice::from_ref(value)
                };
                for value in values {
                    let value = match value {
                        Value::Null => continue,
                        Value::String(value) => value.clone(),
                        Value::Bool(value) => value.to_string(),
                        Value::Number(value) => value.to_string(),
                        _ => {
                            return Err(error(
                                "invalid_query",
                                "The API query contains an invalid value.",
                            ));
                        }
                    };
                    url.query_pairs_mut().append_pair(key, &value);
                }
            }
        }
        // Validate before acquiring a token or sending any network request.
        for (name, value) in [
            ("request ID", request.request_id.as_deref()),
            ("API schema", request.api_schema.as_deref()),
            ("workspace", request.workspace_id.as_deref()),
        ] {
            if let Some(value) = value
                && (value.is_empty()
                    || value.len() > 1024
                    || reqwest::header::HeaderValue::from_str(value).is_err())
            {
                return Err(error(
                    "invalid_header",
                    format!("The {name} header is invalid."),
                ));
            }
        }
        let method = request
            .method
            .as_deref()
            .unwrap_or(if request.body.is_some() {
                "POST"
            } else {
                "GET"
            });
        if !matches!(method, "GET" | "POST") || (method == "GET" && request.body.is_some()) {
            return Err(error(
                "invalid_method",
                "The request must be a GET read or a POST command.",
            ));
        }
        let conditional_read = method == "GET";
        let mut builder = if method == "POST" {
            let body = request.body.as_ref().unwrap_or(&Value::Null);
            if self.remote.is_some() {
                self.client.post(url.clone()).json(body)
            } else {
                let token = self.token().await?;
                let builder = self
                    .client
                    .post(url.clone())
                    .header("X-Canvas-Token", token)
                    .json(&body);
                builder
            }
        } else {
            self.client
                .get(url.clone())
                .timeout(Duration::from_secs(15))
        };
        if let Some(id) = &request.request_id {
            builder = builder.header("X-Studio-Request-Id", id);
        }
        if let Some(workspace) = &request.workspace_id {
            builder = builder.header("X-Canvas-Workspace", workspace);
        }
        if let Some(schema) = &request.api_schema {
            builder = builder.header("X-Studio-API-Schema", schema);
        }
        if conditional_read && let Some(etag) = request.etag.as_deref() {
            if etag.len() > 8192 || etag.chars().any(char::is_control) {
                return Err(error(
                    "invalid_etag",
                    "The conditional read validator is invalid.",
                ));
            }
            builder = builder.header(reqwest::header::IF_NONE_MATCH, etag);
        }
        if let Some(timeout) = request.timeout_ms {
            if timeout == 0 || timeout > 86_400_000 {
                return Err(error("invalid_timeout", "The request deadline is invalid."));
            }
            builder = builder.timeout(Duration::from_millis(timeout));
        }
        let response = if let Some(remote) = &self.remote {
            let id = request
                .request_id
                .clone()
                .or_else(|| {
                    request.body.as_ref().and_then(|body| {
                        body["id"]
                            .as_str()
                            .or_else(|| body["request_id"].as_str())
                            .map(str::to_owned)
                    })
                })
                .filter(|id| !id.is_empty());
            let id = match id {
                Some(id) => id,
                None if method == "GET" => {
                    crate::server_keys::random_id().map_err(|e| error("identity", e))?
                }
                _ => {
                    return Err(error(
                        "request_identity",
                        "A remote write needs its original request ID.",
                    ));
                }
            };
            let mut headers = vec![serde_json::json!(["content-type", "application/json"])];
            for (key, value) in [
                ("x-studio-api-schema", request.api_schema.as_deref()),
                ("x-canvas-workspace", request.workspace_id.as_deref()),
                ("if-none-match", request.etag.as_deref()),
            ] {
                if let Some(value) = value {
                    headers.push(serde_json::json!([key, value]));
                }
            }
            let body = request
                .body
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()
                .map_err(|e| error("invalid_request", e))?
                .unwrap_or_default();
            let headers = Value::Array(headers);
            let send = remote.send(method, url.as_str(), &headers, body, &id);
            tokio::time::timeout(
                Duration::from_millis(request.timeout_ms.unwrap_or(15_000)),
                send,
            )
            .await
            .map_err(|_| {
                error(
                    "timeout",
                    "The server did not respond. The command outcome may be unknown.",
                )
            })?
            .map_err(|e| error("network", e))?
        } else {
            builder.send().await.map_err(Self::network)?
        };
        if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
            *self.credentials.lock().await = None;
        }
        if request.json_response {
            return Self::json_response(response).await;
        }
        if request.http_response {
            return Self::http_response(response).await;
        }
        if response.status().as_u16() == 304 && request.read_metadata {
            let etag = response
                .headers()
                .get(reqwest::header::ETAG)
                .and_then(|value| value.to_str().ok())
                .map(str::to_owned)
                .or(request.etag);
            return Ok(serde_json::json!({
                "_studioApiRead": true,
                "etag": etag,
                "notModified": true,
                "value": null
            }));
        }
        if request.download {
            const DOWNLOAD_LIMIT: usize = 20 * 1024 * 1024;
            let status = response.status();
            if !status.is_success() {
                return Self::decode(response).await;
            }
            if response
                .content_length()
                .is_some_and(|size| size > DOWNLOAD_LIMIT as u64)
            {
                return Err(error(
                    "download_too_large",
                    "The download exceeds the 20 MiB renderer limit.",
                ));
            }
            let mime = response
                .headers()
                .get(reqwest::header::CONTENT_TYPE)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("application/octet-stream")
                .to_owned();
            let name = response
                .headers()
                .get(reqwest::header::CONTENT_DISPOSITION)
                .and_then(|value| value.to_str().ok())
                .and_then(|value| value.split("filename=\"").nth(1))
                .and_then(|value| value.split('\"').next())
                .filter(|value| !value.is_empty() && !value.chars().any(char::is_control))
                .unwrap_or("download")
                .to_owned();
            let truncated = response
                .headers()
                .get("x-log-truncated")
                .and_then(|value| value.to_str().ok())
                == Some("true");
            let mut response = response;
            let mut bytes = Vec::new();
            while let Some(chunk) = response.chunk().await.map_err(Self::network)? {
                if bytes.len().saturating_add(chunk.len()) > DOWNLOAD_LIMIT {
                    return Err(error(
                        "download_too_large",
                        "The download exceeds the 20 MiB renderer limit.",
                    ));
                }
                bytes.extend_from_slice(&chunk);
            }
            return Ok(serde_json::json!({
                "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
                "mime": mime,
                "name": name,
                "truncated": truncated,
            }));
        }
        let response_etag = response
            .headers()
            .get(reqwest::header::ETAG)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        // Never repeat a mutation after refreshing credentials or losing a response.
        let value = Self::decode(response).await?;
        if let Some(token) = value.get("token").and_then(Value::as_str) {
            *self.credentials.lock().await = Some(token.into());
        }
        if request.read_metadata {
            Ok(serde_json::json!({
                "_studioApiRead": true,
                "etag": response_etag,
                "notModified": false,
                "value": value
            }))
        } else {
            Ok(value)
        }
    }

    // Preserve HTTP errors as HTTP responses so the renderer can apply the same
    // schema-mismatch and command-error rules as its generated API client.
    async fn http_response(mut response: reqwest::Response) -> Result<Value, CommandError> {
        const LIMIT: usize = 20 * 1024 * 1024;
        let status = response.status().as_u16();
        let mut headers = serde_json::Map::new();
        for name in [
            "content-type",
            "content-disposition",
            "etag",
            "x-log-truncated",
            "x-studio-api-schema",
            "x-studio-api-schema-mismatch",
            "retry-after",
        ] {
            if let Some(value) = response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
            {
                headers.insert(name.into(), Value::String(value.into()));
            }
        }
        if response
            .content_length()
            .is_some_and(|size| size > LIMIT as u64)
        {
            return Err(error(
                "response_too_large",
                "The response exceeds the 20 MiB renderer limit.",
            ));
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(Self::network)? {
            if bytes.len().saturating_add(chunk.len()) > LIMIT {
                return Err(error(
                    "response_too_large",
                    "The response exceeds the 20 MiB renderer limit.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(serde_json::json!({
            "status": status, "headers": headers,
            "base64": base64::engine::general_purpose::STANDARD.encode(bytes),
        }))
    }

    /// Typed UI port: status and schema headers stay intact without browser HTTP objects.
    async fn json_response(mut response: reqwest::Response) -> Result<Value, CommandError> {
        const LIMIT: usize = 20 * 1024 * 1024;
        let status = response.status().as_u16();
        let mut headers = serde_json::Map::new();
        for name in [
            "etag",
            "x-studio-api-schema",
            "x-studio-api-schema-mismatch",
            "retry-after",
        ] {
            if let Some(value) = response
                .headers()
                .get(name)
                .and_then(|value| value.to_str().ok())
            {
                headers.insert(name.into(), value.into());
            }
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(Self::network)? {
            if bytes.len().saturating_add(chunk.len()) > LIMIT {
                return Err(error(
                    "response_size",
                    "The JSON response exceeds the native transport limit.",
                ));
            }
            bytes.extend_from_slice(&chunk);
        }
        let value = if matches!(status, 204 | 205 | 304) {
            Value::Null
        } else {
            serde_json::from_slice::<Value>(&bytes).map_err(|e| error("invalid_response", e))?
        };
        Ok(serde_json::json!({"status":status,"headers":headers,"body":value}))
    }

    pub(crate) async fn read_bounded(&self, path: &str, limit: usize) -> Result<Value, String> {
        if !self.authorized.load(std::sync::atomic::Ordering::Acquire) {
            return Err("The local backend identity has not been verified.".into());
        }
        let mut response = if let Some(remote) = &self.remote {
            remote
                .send(
                    "GET",
                    self.url(path).map_err(|e| e.message)?.as_str(),
                    &Value::Array(Vec::new()),
                    Vec::new(),
                    &crate::server_keys::random_id()?,
                )
                .await?
        } else {
            self.client
                .get(self.url(path).map_err(|e| e.message)?)
                .timeout(Duration::from_secs(15))
                .send()
                .await
                .map_err(|e| e.to_string())?
        };
        let status = response.status();
        if response.content_length().is_some_and(|n| n > limit as u64) {
            return Err("The file response exceeds the preview limit.".into());
        }
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|e| e.to_string())? {
            if bytes.len() + chunk.len() > limit {
                return Err("The file response exceeds the preview limit.".into());
            }
            bytes.extend_from_slice(&chunk);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
        if !status.is_success() {
            return Err(value["error"]
                .as_str()
                .unwrap_or("Cannot read the selected file.")
                .into());
        }
        Ok(value)
    }

    pub async fn dispatch(&self, request: Request) -> Response {
        if request.method == "api.prepareSession" {
            return match self.prepare_session().await {
                Ok(()) => Response::success(request.id, serde_json::json!({"ready":true})),
                Err(error) => Response {
                    id: request.id,
                    result: None,
                    error: Some(error),
                },
            };
        }
        let result = match serde_json::from_value::<ApiRequest>(request.params) {
            Ok(params) => self.request(params).await,
            Err(value) => Err(error("invalid_params", value)),
        };
        match result {
            Ok(result) => Response::success(request.id, result),
            Err(error) => Response {
                id: request.id,
                result: None,
                error: Some(error),
            },
        }
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };
    #[tokio::test]
    async fn native_api_query_and_json_metadata_preserve_workspace_without_browser_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, body) = read_http(&mut socket).await;
            let workspace = headers
                .lines()
                .any(|line| line.eq_ignore_ascii_case("x-canvas-workspace: exact-workspace"));
            assert!(
                headers.starts_with("GET /api/accounts?key=a+%26+b&many=one&many=two HTTP/1.1"),
                "{headers}"
            );
            assert_eq!(body, Value::Null);
            socket.write_all(b"HTTP/1.1 304 Not Modified\r\nETag: exact\r\nX-Studio-API-Schema: schema\r\nConnection: close\r\n\r\n").await.unwrap();
            workspace
        });
        let response=Backend::new(&origin).unwrap().dispatch(Request{id:"read".into(),method:"api.request".into(),params:serde_json::json!({"path":"/api/accounts","workspaceId":"exact-workspace","query":{"key":"a & b","many":["one","two"]},"jsonResponse":true,"etag":"exact"})}).await;
        assert!(
            server.await.unwrap(),
            "The native GET must carry its exact workspace assertion."
        );
        let result = response.result.expect("JSON metadata response");
        assert_eq!(result["status"], 304);
        assert_eq!(result["headers"]["etag"], "exact");
        assert_eq!(result["body"], Value::Null);
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let invalid=Backend::new(&origin).unwrap().dispatch(Request{id:"invalid".into(),method:"api.request".into(),params:serde_json::json!({"path":"/api/accounts","query":{"nested":{"foreign":"value"}}})}).await;
        assert_eq!(invalid.error.unwrap().code, "invalid_query");
        assert!(
            tokio::time::timeout(Duration::from_millis(20), listener.accept())
                .await
                .is_err()
        );
    }
    #[tokio::test]
    async fn native_backend_negotiates_and_decodes_brotli_json() {
        use std::io::Write;
        let expected = serde_json::json!({"agents": ["agent content ".repeat(512)]});
        let mut compressed = Vec::new();
        {
            let mut encoder = brotli::CompressorWriter::new(&mut compressed, 4096, 4, 22);
            encoder
                .write_all(&serde_json::to_vec(&expected).unwrap())
                .unwrap();
        }
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, _) = read_http(&mut stream).await;
            assert!(
                headers
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("accept-encoding: br")),
                "{headers}"
            );
            let header = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Encoding: br\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                compressed.len()
            );
            stream.write_all(header.as_bytes()).await.unwrap();
            stream.write_all(&compressed).await.unwrap();
        });
        let response = Backend::new(&origin)
            .unwrap()
            .dispatch(Request {
                id: "brotli".into(),
                method: "api.request".into(),
                params: serde_json::json!({"path": "/api/state"}),
            })
            .await;
        server.await.unwrap();
        assert!(response.error.is_none(), "{:?}", response.error);
        assert_eq!(response.result.unwrap(), expected);
    }

    #[tokio::test]
    async fn brotli_native_event_stream_decodes_each_flush_before_the_next_event() {
        use std::io::Write;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (advance, mut next) = tokio::sync::mpsc::channel::<()>(1);
        let events = [
            "event: resources\nid: 1\ndata: {\"revision\":1}\n\n",
            "event: heartbeat\nid: 2\ndata: {\"revision\":2}\n\n",
        ];
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let (headers, _) = read_http(&mut socket).await;
            assert!(
                headers
                    .lines()
                    .any(|line| line.eq_ignore_ascii_case("accept-encoding: br")),
                "{headers}"
            );
            socket.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Encoding: br\r\nConnection: close\r\n\r\n").await.unwrap();
            let mut encoder = brotli::CompressorWriter::new(Vec::new(), 4096, 4, 20);
            for event in events {
                encoder.write_all(event.as_bytes()).unwrap();
                encoder.flush().unwrap();
                socket
                    .write_all(&std::mem::take(encoder.get_mut()))
                    .await
                    .unwrap();
                next.recv()
                    .await
                    .expect("client read event before next production");
            }
        });
        let backend = Backend::new(&origin).unwrap();
        let mut response = backend
            .event_stream("/api/sync/stream", "")
            .await
            .unwrap()
            .unwrap();
        for event in events {
            let mut actual = Vec::new();
            tokio::time::timeout(Duration::from_secs(2), async {
                while actual.len() < event.len() {
                    actual.extend_from_slice(
                        &response.chunk().await.unwrap().expect("decoded event"),
                    );
                }
            })
            .await
            .expect("flush reaches native client immediately");
            assert_eq!(actual, event.as_bytes());
            advance.send(()).await.unwrap();
        }
        drop(response);
        server.await.unwrap();
    }

    #[test]
    fn ui_cannot_choose_an_external_host_or_leave_the_api() {
        assert!(Backend::new("https://example.com").is_err());
        assert!(Backend::new("http://user@127.0.0.1:4620").is_err());
        let backend = Backend::new("http://127.0.0.1:4620").unwrap();
        for path in [
            "//example.com/api/state",
            "/api/../../secret",
            "/api/../secret",
            "/api/%2e%2e/secret",
            "/api/a\\b",
            "/api/state#ignored",
        ] {
            assert!(backend.url(path).is_err(), "{path}");
        }
        assert_eq!(
            backend.url("/api/state?view=chat").unwrap().as_str(),
            "http://127.0.0.1:4620/api/state?view=chat"
        );
    }

    pub(crate) async fn read_http(stream: &mut tokio::net::TcpStream) -> (String, Value) {
        let mut bytes = Vec::new();
        while !bytes.ends_with(b"\r\n\r\n") {
            let mut byte = [0];
            stream.read_exact(&mut byte).await.unwrap();
            bytes.push(byte[0]);
            assert!(bytes.len() < 8192);
        }
        let headers = String::from_utf8(bytes).unwrap();
        let length: usize = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .map(|value| value.trim().parse().unwrap())
            })
            .unwrap_or(0);
        let mut body = vec![0; length];
        stream.read_exact(&mut body).await.unwrap();
        (
            headers,
            if body.is_empty() {
                Value::Null
            } else {
                serde_json::from_slice(&body).unwrap()
            },
        )
    }

    pub(crate) async fn reply(stream: &mut tokio::net::TcpStream, body: &str) {
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream.write_all(response.as_bytes()).await.unwrap();
    }

    #[tokio::test]
    async fn bounded_download_preserves_bytes_and_response_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, _) = read_http(&mut stream).await;
            assert!(headers.starts_with("GET /api/monitor/log?id=one "));
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=\"one.log\"\r\nX-Log-Truncated: true\r\nContent-Length: 5\r\nConnection: close\r\n\r\na\0b\xffc")
                .await
                .unwrap();
        });
        let result = Backend::new(&origin)
            .unwrap()
            .request(ApiRequest {
                path: "/api/monitor/log?id=one".into(),
                query: None,
                json_response: false,
                method: None,
                request_id: None,
                api_schema: None,
                body: None,
                workspace_id: None,
                timeout_ms: None,
                etag: None,
                read_metadata: false,
                download: true,
                http_response: false,
            })
            .await
            .unwrap();
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(result["base64"].as_str().unwrap())
            .unwrap();
        assert_eq!(decoded, b"a\0b\xffc");
        assert_eq!(result["name"], "one.log");
        assert_eq!(result["mime"], "application/octet-stream");
        assert_eq!(result["truncated"], true);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn download_rejects_declared_oversize_before_reading_body() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http(&mut stream).await;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: 20971521\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let failure = Backend::new(&origin)
            .unwrap()
            .request(ApiRequest {
                path: "/api/analytics?export=1".into(),
                query: None,
                json_response: false,
                method: None,
                request_id: None,
                api_schema: None,
                body: None,
                workspace_id: None,
                timeout_ms: None,
                etag: None,
                read_metadata: false,
                download: true,
                http_response: false,
            })
            .await
            .unwrap_err();
        assert_eq!(failure.code, "download_too_large");
        server.await.unwrap();
    }

    #[tokio::test]
    async fn explicit_null_post_is_not_changed_to_a_read() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut session, _) = listener.accept().await.unwrap();
            read_http(&mut session).await;
            reply(&mut session, r#"{"token":"native-token"}"#).await;
            let (mut command, _) = listener.accept().await.unwrap();
            let (headers, body) = read_http(&mut command).await;
            assert!(headers.starts_with("POST /api/messages "));
            assert!(headers.to_ascii_lowercase().contains("content-length: 4"));
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("x-canvas-token: native-token")
            );
            assert_eq!(body, Value::Null);
            command
                .write_all(b"HTTP/1.1 204 No Content\r\nConnection: close\r\n\r\n")
                .await
                .unwrap();
        });
        let response = Backend::new(&origin).unwrap().dispatch(Request { id: "null-post".into(), method: "api.request".into(), params: serde_json::json!({"path":"/api/messages", "method":"POST", "body":null, "httpResponse":true}) }).await;
        assert!(response.error.is_none());
        assert_eq!(response.result.unwrap()["status"], 204);
        server.await.unwrap();
    }

    #[tokio::test]
    async fn lost_mutation_response_is_not_replayed() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut session, _) = listener.accept().await.unwrap();
            assert!(
                read_http(&mut session)
                    .await
                    .0
                    .starts_with("GET /api/session ")
            );
            reply(&mut session, r#"{"token":"test-token"}"#).await;
            drop(session);
            let (mut mutation, _) = listener.accept().await.unwrap();
            let (headers, body) = read_http(&mut mutation).await;
            assert!(headers.starts_with("POST /api/messages "));
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("x-canvas-token: test-token")
            );
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("x-canvas-workspace: workspace-one")
            );
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("x-studio-request-id: original-operation-id")
            );
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("x-studio-api-schema: client-schema")
            );
            assert_eq!(
                body,
                serde_json::json!({"id":"original-message-id","text":"Привет"})
            );
            // Simulate a committed server mutation whose response is lost.
            drop(mutation);
            listener
        });
        let backend = Backend::new(&origin).unwrap();
        let response = backend.dispatch(Request { id: "transport-id".into(), method: "api.request".into(), params: serde_json::json!({
            "path":"/api/messages", "body":{"id":"original-message-id","text":"Привет"}, "workspaceId":"workspace-one", "requestId":"original-operation-id", "apiSchema":"client-schema",
        }) }).await;
        assert_eq!(response.id, "transport-id");
        assert_eq!(response.error.unwrap().code, "network");
        let listener = server.await.expect("server").into_std().expect("listener");
        assert_eq!(
            listener.accept().expect_err("no connection").kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn http_envelope_preserves_schema_errors_binary_bytes_and_empty_responses() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for (status, body) in [
                (409, &b"schema mismatch"[..]),
                (200, &b"a\0\xff"[..]),
                (304, &b""[..]),
            ] {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, _) = read_http(&mut stream).await;
                assert!(
                    headers
                        .to_ascii_lowercase()
                        .contains("x-studio-api-schema: client-schema")
                );
                let head = format!(
                    "HTTP/1.1 {status} Test\r\nContent-Type: application/octet-stream\r\nX-Studio-API-Schema: server-schema\r\nX-Studio-API-Schema-Mismatch: 1\r\nSet-Cookie: private=secret\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                    body.len()
                );
                stream.write_all(head.as_bytes()).await.unwrap();
                stream.write_all(body).await.unwrap();
            }
        });
        let backend = Backend::new(&origin).unwrap();
        for (status, body) in [
            (409, &b"schema mismatch"[..]),
            (200, &b"a\0\xff"[..]),
            (304, &b""[..]),
        ] {
            let response = backend.dispatch(Request { id: status.to_string(), method: "api.request".into(), params: serde_json::json!({"path":"/api/state", "apiSchema":"client-schema", "httpResponse":true}) }).await;
            assert!(response.error.is_none());
            let result = response.result.unwrap();
            assert_eq!(result["status"], status);
            assert_eq!(result["headers"]["x-studio-api-schema"], "server-schema");
            assert_eq!(result["headers"]["x-studio-api-schema-mismatch"], "1");
            assert!(result["headers"].get("set-cookie").is_none());
            assert_eq!(
                base64::engine::general_purpose::STANDARD
                    .decode(result["base64"].as_str().unwrap())
                    .unwrap(),
                body
            );
        }
        server.await.unwrap();
    }

    #[tokio::test]
    async fn invalid_headers_and_unverified_sessions_do_not_contact_the_backend() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let backend = Backend::new(&origin).unwrap();
        for field in ["requestId", "apiSchema", "workspaceId"] {
            let mut params = serde_json::json!({"path":"/api/messages", "body":{"text":"hello"}});
            params[field] = Value::String("injected\r\nX-Extra: value".into());
            let response = backend
                .dispatch(Request {
                    id: "invalid".into(),
                    method: "api.request".into(),
                    params,
                })
                .await;
            assert_eq!(response.error.unwrap().code, "invalid_header");
        }
        let unverified = Backend::desktop_client(&origin).unwrap();
        assert_eq!(
            unverified.prepare_session().await.unwrap_err().code,
            "backend_unverified"
        );
        let listener = listener.into_std().expect("listener");
        assert_eq!(
            listener.accept().expect_err("no connection").kind(),
            std::io::ErrorKind::WouldBlock
        );
    }

    #[tokio::test]
    async fn redirects_do_not_forward_the_request() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http(&mut stream).await;
            stream.write_all(b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1:1/secret\r\nContent-Type: application/json\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}").await.unwrap();
        });
        let response = Backend::new(&origin)
            .unwrap()
            .dispatch(Request {
                id: "read".into(),
                method: "api.request".into(),
                params: serde_json::json!({"path":"/api/state"}),
            })
            .await;
        assert_eq!(response.error.unwrap().status, Some(302));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn conditional_reads_forward_etag_and_report_304_metadata() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut first, _) = listener.accept().await.unwrap();
            read_http(&mut first).await;
            first.write_all(b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nETag: \"rev-1\"\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"value\":1}").await.unwrap();
            let (mut second, _) = listener.accept().await.unwrap();
            let (headers, _) = read_http(&mut second).await;
            assert!(
                headers
                    .to_ascii_lowercase()
                    .contains("if-none-match: \"rev-1\"")
            );
            second.write_all(b"HTTP/1.1 304 Not Modified\r\nETag: \"rev-1\"\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await.unwrap();
        });
        let backend = Backend::new(&origin).unwrap();
        let read = |id: &str, etag: Option<&str>| Request {
            id: id.into(),
            method: "api.request".into(),
            params: serde_json::json!({"path":"/api/read","readMetadata":true,"etag":etag}),
        };
        let first = backend.dispatch(read("first", None)).await.result.unwrap();
        assert_eq!(first["etag"], "\"rev-1\"");
        assert_eq!(first["notModified"], false);
        assert_eq!(first["value"], serde_json::json!({"value":1}));
        let second = backend
            .dispatch(read("second", Some("\"rev-1\"")))
            .await
            .result
            .unwrap();
        assert_eq!(second["etag"], "\"rev-1\"");
        assert_eq!(second["notModified"], true);
        assert!(second["value"].is_null());
        server.await.unwrap();
    }
}
