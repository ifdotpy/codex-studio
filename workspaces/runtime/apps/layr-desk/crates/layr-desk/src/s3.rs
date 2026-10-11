//! The S3 client of the writer: a put of a new object (`If-None-Match: *`) and ListObjectsV2,
//! which is all that its write-only key may do; `deploy` also reads and replaces vault.json with
//! a person's key. `send` signs any AWS call. desk_core::aws signs and finds the dual-stack
//! endpoint, as for the gateway (src/aws.rs); this client differs in its credentials (the key of
//! the vault configuration, no role) and in its time limits, which grow with the size of a part.
//! reqwest takes its roots from webpki-roots, so a static binary needs no certificate store.

use anyhow::{anyhow, bail, Context, Result};
use desk_core::aws::{self, Credentials, EMPTY_SHA256};
use reqwest::Method;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The time limit of a call without a body, and the base of one with a body.
const LIMIT: Duration = Duration::from_secs(120);
/// The slowest upload that a put waits for, in bytes a second.
const SLOWEST: u64 = 100_000;

pub struct S3 {
    http: reqwest::Client,
    credentials: Credentials,
    bucket: String,
    region: String,
    endpoint: Option<String>,
}

impl S3 {
    /// `endpoint` (`http://127.0.0.1:9000`) replaces AWS with a path-style test store.
    pub fn new(
        bucket: &str,
        region: &str,
        credentials: Credentials,
        endpoint: Option<&str>,
    ) -> Result<Self> {
        let http = reqwest::Client::builder()
            .user_agent("layr-desk")
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;
        Ok(S3 {
            http,
            credentials,
            bucket: bucket.to_string(),
            region: region.to_string(),
            endpoint: endpoint.map(str::to_string),
        })
    }

    /// The status and body of one signed call.
    async fn call(
        &self,
        method: Method,
        key: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<(u16, String)> {
        let (base, path) =
            aws::s3_location(&self.bucket, &self.region, self.endpoint.as_deref(), key);
        // A time limit for the whole call: reqwest's read timeout would end an upload that
        // takes longer than the limit, however steadily it goes.
        let limit = LIMIT + Duration::from_secs(body.len() as u64 / SLOWEST);
        let call = Call {
            method,
            base: &base,
            path: &path,
            query,
            headers,
            body,
        };
        send(
            &self.http,
            call,
            &self.credentials,
            &self.region,
            "s3",
            limit,
        )
        .await
    }

    /// A text object, or None when there is none.
    pub async fn get_text(&self, key: &str) -> Result<Option<String>> {
        let (status, reply) = self
            .call(Method::GET, key, &[], &[], Vec::new())
            .await
            .with_context(|| format!("S3 get {key}"))?;
        match status {
            200 => Ok(Some(reply)),
            404 if aws::xml_text(&reply, "Code").as_deref() == Some("NoSuchKey") => Ok(None),
            _ => Err(failure(&format!("S3 get {key}"), status, &reply)),
        }
    }

    /// Put an object, new or replacing one.
    pub async fn put(&self, key: &str, body: Vec<u8>, content_type: &str) -> Result<()> {
        let (status, reply) = self
            .call(
                Method::PUT,
                key,
                &[],
                &[("content-type", content_type)],
                body,
            )
            .await
            .with_context(|| format!("S3 put {key}"))?;
        match status {
            200 => Ok(()),
            _ => Err(failure(&format!("S3 put {key}"), status, &reply)),
        }
    }
}

/// One signed AWS call. `path` is not encoded yet.
pub struct Call<'a> {
    pub method: Method,
    /// Scheme, host and port.
    pub base: &'a str,
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    pub headers: &'a [(&'a str, &'a str)],
    pub body: Vec<u8>,
}

/// Send one call signed for `service` in `region`, within `limit`: its status and body.
pub async fn send(
    http: &reqwest::Client,
    call: Call<'_>,
    credentials: &Credentials,
    region: &str,
    service: &str,
    limit: Duration,
) -> Result<(u16, String)> {
    let url = reqwest::Url::parse(call.base)?;
    // The Host header that reqwest sends: the port only when it is not the default one.
    let host = match (url.host_str(), url.port()) {
        (Some(h), Some(p)) => format!("{h}:{p}"),
        (Some(h), None) => h.to_string(),
        _ => bail!("no host in {}", call.base),
    };
    let payload = if call.body.is_empty() {
        EMPTY_SHA256.to_string()
    } else {
        hex::encode(Sha256::digest(&call.body))
    };
    let unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
    let signed = aws::sign(
        &aws::Request {
            method: call.method.as_str(),
            host: &host,
            path: call.path,
            query: call.query,
            headers: call.headers,
            payload_sha256: &payload,
        },
        credentials,
        region,
        service,
        unix,
    );
    let mut target = format!("{}{}", call.base, aws::uri_encode(call.path, true));
    if !call.query.is_empty() {
        target = format!("{target}?{}", aws::query_string(call.query));
    }
    let body = call.method == Method::PUT || call.method == Method::POST;
    let mut request = http.request(call.method, target).timeout(limit);
    for (name, value) in call.headers.iter().copied() {
        request = request.header(name, value);
    }
    for (name, value) in &signed {
        request = request.header(name, value);
    }
    if body {
        request = request.body(call.body);
    }
    let response = request.send().await?;
    let status = response.status().as_u16();
    Ok((status, response.text().await?))
}

/// What a failed call says: S3's code and message, which hold no secret.
fn failure(what: &str, status: u16, reply: &str) -> anyhow::Error {
    let code = aws::xml_text(reply, "Code").unwrap_or_default();
    let message = aws::xml_text(reply, "Message").unwrap_or_default();
    anyhow!("{what}: HTTP {status} {code} {message}")
}

impl desk_core::writer::Bucket for S3 {
    async fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        let (status, reply) = self
            .call(Method::PUT, key, &[], &[("if-none-match", "*")], body)
            .await
            .with_context(|| format!("S3 put {key}"))?;
        match status {
            200 => Ok(true),
            412 => Ok(false),
            _ => Err(failure(&format!("S3 put {key}"), status, &reply)),
        }
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let (mut keys, mut token) = (Vec::new(), None::<String>);
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(token) = &token {
                query.push(("continuation-token", token));
            }
            let (status, reply) = self
                .call(Method::GET, "", &query, &[], Vec::new())
                .await
                .with_context(|| format!("S3 list {prefix}"))?;
            if status != 200 {
                return Err(failure(&format!("S3 list {prefix}"), status, &reply));
            }
            if !reply.contains("<ListBucketResult") {
                bail!("S3 list {prefix}: the reply is not a listing");
            }
            keys.extend(aws::xml_texts(&reply, "Key"));
            token = aws::xml_text(&reply, "NextContinuationToken").filter(|t| !t.is_empty());
            if token.is_none() {
                return Ok(keys);
            }
        }
    }
}
