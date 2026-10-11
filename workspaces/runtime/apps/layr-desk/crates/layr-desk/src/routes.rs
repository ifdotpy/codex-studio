//! The gateway's HTTP side (docs/design.md, "Gateway"): the site, and the API of every vault in
//! the configuration. `serve` runs it with TLS on the tailnet address only.

use crate::vault::{fail, Body, Bucket, Vault, VaultError};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::Incoming;
use hyper::{Method, Request, Response};
use percent_encoding::percent_decode;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::path::PathBuf;
use std::sync::Arc;

const SECURITY: [(&str, &str); 6] = [
    (
        "content-security-policy",
        "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; style-src 'self' 'unsafe-inline'; \
         connect-src 'self'; img-src 'self' blob: data:; media-src 'self' blob:; frame-src blob:; \
         object-src 'none'; frame-ancestors 'none'; base-uri 'none'; form-action 'none'",
    ),
    ("strict-transport-security", "max-age=63072000"),
    ("x-content-type-options", "nosniff"),
    ("x-frame-options", "DENY"),
    ("referrer-policy", "no-referrer"),
    ("cross-origin-opener-policy", "same-origin"),
];
const TYPES: [(&str, &str); 8] = [
    ("html", "text/html; charset=utf-8"),
    ("js", "text/javascript"),
    ("css", "text/css"),
    ("wasm", "application/wasm"),
    ("json", "application/json"),
    ("svg", "image/svg+xml"),
    ("png", "image/png"),
    ("ico", "image/x-icon"),
];
const BODY_LIMIT: usize = 16 * 1024;

/// `/etc/vault-gateway/config.json`, which the gateway host writes.
#[derive(Deserialize, Default, Clone, Debug)]
pub struct Config {
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub listen: Option<String>,
    /// Each vault's `{bucket, region, role, recipient}`, read when the vault is used.
    #[serde(default)]
    pub projects: BTreeMap<String, Value>,
}

pub type ConfigFn = Box<dyn Fn() -> Result<Arc<Config>> + Send + Sync>;
pub type VaultFn<B> = Box<dyn Fn(&str, &Config) -> Result<Option<Arc<Vault<B>>>> + Send + Sync>;

pub struct Gateway<B> {
    pub config: ConfigFn,
    pub vault_for: VaultFn<B>,
    /// The folder of the built site.
    pub site: PathBuf,
}

fn full(data: impl Into<Bytes>) -> Body {
    Full::new(data.into())
        .map_err(|e| match e {})
        .boxed_unsync()
}

fn send(status: u16, body: Value) -> Response<Body> {
    let mut response = Response::builder().status(status);
    for (name, value) in SECURITY {
        response = response.header(name, value);
    }
    response
        .header("content-type", "application/json")
        .header("cache-control", "no-store")
        .body(full(body.to_string()))
        .expect("valid headers")
}

fn error(status: u16, message: &str) -> Response<Body> {
    send(status, json!({ "error": message }))
}

/// The path as a URL parser gives it: `\` as `/`, and `.` and `..` segments (also as `%2e`)
/// resolved.
fn normalize(raw: &str) -> String {
    let raw = raw.replace('\\', "/");
    let segments: Vec<&str> = raw.split('/').skip(1).collect();
    let mut out: Vec<&str> = Vec::new();
    for (i, segment) in segments.iter().enumerate() {
        let last = i + 1 == segments.len();
        match segment.to_ascii_lowercase().replace("%2e", ".").as_str() {
            ".." => {
                out.pop();
                if last {
                    out.push("");
                }
            }
            "." if last => out.push(""),
            "." => {}
            _ => out.push(segment),
        }
    }
    format!("/{}", out.join("/"))
}

/// A path segment decoded as `decodeURIComponent` does: a broken escape or UTF-8 is an error.
fn decode(segment: &str) -> Option<String> {
    let b = segment.as_bytes();
    for (i, c) in b.iter().enumerate() {
        if *c == b'%'
            && !(b.len() > i + 2 && b[i + 1].is_ascii_hexdigit() && b[i + 2].is_ascii_hexdigit())
        {
            return None;
        }
    }
    percent_decode(b).decode_utf8().ok().map(|s| s.into_owned())
}

/// The extension of the last part of a path, as Node's `extname`: none for `.name`.
fn has_extension(path: &str) -> bool {
    let base = path
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or_default();
    match base.rfind('.') {
        Some(at) => !base[..at].bytes().all(|c| c == b'.'),
        None => false,
    }
}

impl<B: Bucket> Gateway<B> {
    pub async fn handle(
        self: Arc<Self>,
        request: Request<Incoming>,
    ) -> Result<Response<Body>, Infallible> {
        Ok(match self.route(request).await {
            Ok(response) => response,
            Err(e) => match e.downcast_ref::<VaultError>() {
                Some(v) => error(v.status, v.message),
                None => {
                    eprintln!("layr-desk serve: {e:#}");
                    error(500, "internal error")
                }
            },
        })
    }

    async fn route(&self, request: Request<Incoming>) -> Result<Response<Body>> {
        let config = (self.config)()?;
        // A name that resolves to the gateway is not enough: the host must be the configured one.
        if let Some(host) = config.host.as_deref().filter(|h| !h.is_empty()) {
            let given = request
                .headers()
                .get("host")
                .and_then(|h| h.to_str().ok())
                .unwrap_or_default();
            let given = match given.rsplit_once(':') {
                Some((name, port))
                    if !port.is_empty() && port.bytes().all(|c| c.is_ascii_digit()) =>
                {
                    name
                }
                _ => given,
            };
            if given != host {
                return Ok(error(421, "wrong host name"));
            }
        }
        let pathname = normalize(request.uri().path());
        let mut parts = Vec::new();
        for segment in pathname.split('/').filter(|s| !s.is_empty()) {
            match decode(segment) {
                Some(part) => parts.push(part),
                None => return Ok(error(400, "the path is not valid")),
            }
        }
        if parts.first().map(String::as_str) == Some("api") {
            return self.api(request, &config, &parts[1..]).await;
        }
        let method = request.method();
        if method != Method::GET && method != Method::HEAD {
            return Ok(error(405, "the site is read-only"));
        }
        let head = method == Method::HEAD;
        // Each vault is a page of the site: /<project>/ shows the same index.html.
        if !has_extension(&pathname) {
            return self.file(head, &["index.html".to_string()], false).await;
        }
        if pathname.ends_with('/') {
            return Ok(error(404, "no such file"));
        }
        // Vite names the files below assets/ by their content.
        self.file(head, &parts, pathname.starts_with("/assets/"))
            .await
    }

    async fn file(&self, head: bool, parts: &[String], immutable: bool) -> Result<Response<Body>> {
        let unsafe_part = |p: &String| p == "." || p == ".." || p.contains(['/', '\\', '\0']);
        if parts.iter().any(unsafe_part) {
            return Ok(error(404, "no such file"));
        }
        let path: PathBuf = parts.iter().fold(self.site.clone(), |p, part| p.join(part));
        let length = match tokio::fs::metadata(&path).await {
            Ok(meta) if meta.is_file() => meta.len(),
            _ => return Ok(error(404, "no such file")),
        };
        let (data, length) = if head {
            (Bytes::new(), length)
        } else {
            let data = Bytes::from(tokio::fs::read(&path).await?);
            let length = data.len() as u64;
            (data, length)
        };
        let extension = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        let kind = TYPES
            .iter()
            .find(|(e, _)| *e == extension)
            .map_or("application/octet-stream", |(_, t)| t);
        let mut response = Response::builder().status(200);
        for (name, value) in SECURITY {
            response = response.header(name, value);
        }
        Ok(response
            .header("content-type", kind)
            .header("content-length", length)
            .header(
                "cache-control",
                if immutable {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
            )
            .body(full(data))?)
    }

    async fn api(
        &self,
        request: Request<Incoming>,
        config: &Config,
        parts: &[String],
    ) -> Result<Response<Body>> {
        let method = request.method().clone();
        if parts.len() == 1 && parts[0] == "projects" && method == Method::GET {
            return Ok(send(
                200,
                json!({ "projects": config.projects.keys().collect::<Vec<_>>() }),
            ));
        }
        let project = parts.get(1).map(String::as_str).unwrap_or_default();
        if parts.first().map(String::as_str) != Some("p") || !crate::vault::project_name(project) {
            return Ok(error(404, "no such route"));
        }
        let Some(vault) = (self.vault_for)(project, config)? else {
            return Ok(error(404, "no such vault"));
        };
        if parts.len() > 4 {
            return Ok(error(404, "no such route"));
        }
        let kind = parts.get(2).map(String::as_str);
        let name = parts.get(3).map(String::as_str);
        match (&method, kind, name) {
            (&Method::GET, Some("slots"), None) => {
                let slots: Vec<Value> = vault
                    .slots()
                    .await?
                    .into_iter()
                    .map(|(name, data)| json!({ "name": name, "data": data }))
                    .collect();
                return Ok(send(200, json!({ "slots": slots })));
            }
            (&Method::GET, Some("challenge"), None) => {
                return Ok(send(200, json!({ "challenge": vault.challenge()? })));
            }
            (&Method::GET, Some("vault"), None) => {
                return Ok(send(200, serde_json::to_value(vault.info().await?)?));
            }
            _ => {}
        }

        let auth = request
            .headers()
            .get("authorization")
            .and_then(|h| h.to_str().ok())
            .unwrap_or_default();
        match auth.strip_prefix("Bearer ") {
            Some(token) if vault.valid(token) => {}
            _ => return Ok(error(401, "the token is missing, wrong or expired")),
        }

        match (&method, kind, name) {
            (&Method::GET, Some("states"), None) => {
                Ok(send(200, json!({ "states": vault.states().await? })))
            }
            (&Method::GET, Some("states"), Some(name)) => {
                let object = vault.object(name).await?;
                let mut response = Response::builder().status(200);
                for (name, value) in SECURITY {
                    response = response.header(name, value);
                }
                response = response
                    .header("content-type", "application/octet-stream")
                    .header("cache-control", "private, no-store");
                if let Some(length) = object.length {
                    response = response.header("content-length", length);
                }
                Ok(response.body(object.body)?)
            }
            (&Method::PUT, Some("slots"), Some(name)) => {
                let body = read_json(request).await?;
                let data = body.get("data").and_then(Value::as_str).unwrap_or_default();
                vault.add_slot(name, data).await?;
                Ok(send(200, json!({ "added": name })))
            }
            (&Method::DELETE, Some("slots"), Some(name)) => {
                vault.remove_slot(name).await?;
                Ok(send(200, json!({ "removed": name })))
            }
            _ => Ok(error(404, "no such route")),
        }
    }
}

async fn read_json(request: Request<Incoming>) -> Result<Value> {
    let bytes = match Limited::new(request.into_body(), BODY_LIMIT)
        .collect()
        .await
    {
        Ok(collected) => collected.to_bytes(),
        Err(e) if e.is::<http_body_util::LengthLimitError>() => {
            return fail(413, "the body is too large")
        }
        Err(e) => return Err(anyhow::anyhow!(e)),
    };
    if bytes.is_empty() {
        return Ok(json!({}));
    }
    match serde_json::from_slice(&bytes) {
        Ok(value) => Ok(value),
        Err(_) => fail(400, "the body is not JSON"),
    }
}
