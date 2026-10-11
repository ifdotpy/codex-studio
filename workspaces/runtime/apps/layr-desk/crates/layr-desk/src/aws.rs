//! A small AWS client for the gateway: credentials from the environment or from the instance
//! (IMDSv2), a role per vault through STS AssumeRole, and the four S3 calls that the gateway
//! makes. Endpoints are dual-stack, so an IPv6-only host reaches them. desk-core signs.

use crate::vault::{Bucket, Object};
use anyhow::{anyhow, bail, Context, Result};
use desk_core::aws::{self, Credentials, EMPTY_SHA256};
use http_body_util::BodyExt;
use reqwest::{Method, StatusCode};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

/// Renew credentials when less than this is left.
const MARGIN: Duration = Duration::from_secs(5 * 60);
const SESSION_NAME: &str = "vault-gateway";
/// The limit of each call but a streamed object.
const LIMIT: Duration = Duration::from_secs(30);

#[derive(Clone)]
struct Timed {
    credentials: Arc<Credentials>,
    expires: Option<SystemTime>,
}

impl Timed {
    fn fresh(&self) -> bool {
        self.expires
            .is_none_or(|at| at > SystemTime::now() + MARGIN)
    }
}

fn expiry(text: &str) -> Result<SystemTime> {
    humantime::parse_rfc3339(text).with_context(|| format!("expiration {text:?}"))
}

/// Endpoints and the HTTP client, from the environment as the AWS SDKs read it:
/// `AWS_ENDPOINT_URL_S3` and `AWS_ENDPOINT_URL_STS` (path-style S3, for a local test),
/// `AWS_EC2_METADATA_SERVICE_ENDPOINT` or `AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE=IPv6`.
pub struct Aws {
    http: reqwest::Client,
    s3_endpoint: Option<String>,
    sts_endpoint: Option<String>,
    base: Base,
}

enum Source {
    Static(Arc<Credentials>),
    Instance { endpoint: String },
}

/// The gateway's own credentials: from the environment, else from the instance role.
struct Base {
    source: Source,
    cached: Mutex<Option<Timed>>,
}

impl Aws {
    pub fn from_env() -> Result<Arc<Self>> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.is_empty());
        let source = match (var("AWS_ACCESS_KEY_ID"), var("AWS_SECRET_ACCESS_KEY")) {
            (Some(key_id), Some(secret)) => Source::Static(Arc::new(Credentials {
                key_id,
                secret,
                token: var("AWS_SESSION_TOKEN"),
            })),
            _ => Source::Instance {
                endpoint: var("AWS_EC2_METADATA_SERVICE_ENDPOINT").unwrap_or_else(|| {
                    match var("AWS_EC2_METADATA_SERVICE_ENDPOINT_MODE").as_deref() {
                        Some(mode) if mode.eq_ignore_ascii_case("ipv6") => {
                            "http://[fd00:ec2::254]".into()
                        }
                        _ => "http://169.254.169.254".into(),
                    }
                }),
            },
        };
        Self::new(
            source,
            var("AWS_ENDPOINT_URL_S3"),
            var("AWS_ENDPOINT_URL_STS"),
        )
    }

    fn new(
        source: Source,
        s3_endpoint: Option<String>,
        sts_endpoint: Option<String>,
    ) -> Result<Arc<Self>> {
        let http = reqwest::Client::builder()
            .user_agent("layr-desk")
            .connect_timeout(Duration::from_secs(10))
            .read_timeout(Duration::from_secs(60))
            .no_proxy()
            .build()?;
        Ok(Arc::new(Aws {
            http,
            s3_endpoint: s3_endpoint.map(|e| e.trim_end_matches('/').to_string()),
            sts_endpoint: sts_endpoint.map(|e| e.trim_end_matches('/').to_string()),
            base: Base {
                source,
                cached: Mutex::new(None),
            },
        }))
    }

    /// For tests: fixed credentials, or an instance metadata endpoint, and both endpoints.
    #[cfg(test)]
    pub fn for_test(
        credentials: Option<Credentials>,
        metadata: &str,
        endpoint: &str,
    ) -> Result<Arc<Self>> {
        let source = match credentials {
            Some(c) => Source::Static(Arc::new(c)),
            None => Source::Instance {
                endpoint: metadata.to_string(),
            },
        };
        Self::new(source, Some(endpoint.into()), Some(endpoint.into()))
    }

    async fn base(&self) -> Result<Arc<Credentials>> {
        let endpoint = match &self.base.source {
            Source::Static(c) => return Ok(c.clone()),
            Source::Instance { endpoint } => endpoint,
        };
        let mut cached = self.base.cached.lock().await;
        if let Some(timed) = cached.as_ref().filter(|t| t.fresh()) {
            return Ok(timed.credentials.clone());
        }
        let timed = self
            .instance(endpoint)
            .await
            .context("credentials from the instance metadata (IMDSv2)")?;
        *cached = Some(timed.clone());
        Ok(timed.credentials)
    }

    async fn instance(&self, endpoint: &str) -> Result<Timed> {
        let token = self
            .http
            .put(format!("{endpoint}/latest/api/token"))
            .header("x-aws-ec2-metadata-token-ttl-seconds", "300")
            .timeout(Duration::from_secs(5))
            .send()
            .await?
            .error_for_status()?
            .text()
            .await?;
        let get = |path: String| {
            self.http
                .get(format!(
                    "{endpoint}/latest/meta-data/iam/security-credentials/{path}"
                ))
                .header("x-aws-ec2-metadata-token", &token)
                .timeout(Duration::from_secs(5))
                .send()
        };
        let roles = get(String::new()).await?.error_for_status()?.text().await?;
        let role = roles
            .lines()
            .next()
            .filter(|r| !r.is_empty())
            .ok_or_else(|| anyhow!("the instance has no role"))?;
        let found: serde_json::Value = get(role.to_string())
            .await?
            .error_for_status()?
            .json()
            .await?;
        let field = |name: &str| {
            found[name]
                .as_str()
                .map(str::to_string)
                .ok_or_else(|| anyhow!("the instance credentials have no {name}"))
        };
        Ok(Timed {
            credentials: Arc::new(Credentials {
                key_id: field("AccessKeyId")?,
                secret: field("SecretAccessKey")?,
                token: Some(field("Token")?),
            }),
            expires: Some(expiry(&field("Expiration")?)?),
        })
    }

    /// The bucket of one vault, read through a role of the vault's account.
    pub fn bucket(self: &Arc<Self>, bucket: &str, region: &str, role: &str) -> S3 {
        S3 {
            aws: self.clone(),
            bucket: bucket.to_string(),
            region: region.to_string(),
            role: role.to_string(),
            cached: Mutex::new(None),
        }
    }

    /// Send a signed request.
    async fn send(
        &self,
        call: Call<'_>,
        credentials: &Credentials,
        region: &str,
        service: &str,
    ) -> Result<reqwest::Response> {
        let base = reqwest::Url::parse(&call.base)?;
        let host = match (base.host_str(), base.port()) {
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
                path: &call.path,
                query: call.query,
                headers: call.headers,
                payload_sha256: &payload,
            },
            credentials,
            region,
            service,
            unix,
        );
        let query = aws::query_string(call.query);
        let mut url = format!("{}{}", call.base, aws::uri_encode(&call.path, true));
        if !query.is_empty() {
            url = format!("{url}?{query}");
        }
        let mut request = self.http.request(call.method.clone(), url);
        for (name, value) in call.headers.iter().copied() {
            request = request.header(name, value);
        }
        for (name, value) in &signed {
            request = request.header(name, value);
        }
        if call.method == Method::PUT || call.method == Method::POST {
            request = request.body(call.body);
        }
        if call.limited {
            request = request.timeout(LIMIT);
        }
        Ok(request.send().await?)
    }
}

/// One AWS call: `path` is not encoded yet.
struct Call<'a> {
    method: Method,
    /// Scheme, host and port.
    base: String,
    path: String,
    query: &'a [(&'a str, &'a str)],
    headers: &'a [(&'a str, &'a str)],
    body: Vec<u8>,
    /// A time limit for the whole call, body included; none for an object that streams.
    limited: bool,
}

/// The text of a reply that failed, for the log.
async fn failure(what: &str, response: reqwest::Response) -> anyhow::Error {
    let status = response.status();
    let text = response.text().await.unwrap_or_default();
    let code = aws::xml_text(&text, "Code").unwrap_or_default();
    let message = aws::xml_text(&text, "Message").unwrap_or_default();
    anyhow!("{what}: HTTP {status} {code} {message}")
}

/// One vault's bucket.
pub struct S3 {
    aws: Arc<Aws>,
    bucket: String,
    region: String,
    role: String,
    cached: Mutex<Option<Timed>>,
}

impl S3 {
    /// Temporary credentials of the vault's reader role.
    async fn credentials(&self) -> Result<Arc<Credentials>> {
        let mut cached = self.cached.lock().await;
        if let Some(timed) = cached.as_ref().filter(|t| t.fresh()) {
            return Ok(timed.credentials.clone());
        }
        let base = self.aws.base().await?;
        let endpoint = self
            .aws
            .sts_endpoint
            .clone()
            .unwrap_or_else(|| format!("https://sts.{}.api.aws", self.region));
        let body = format!(
            "Action=AssumeRole&RoleArn={}&RoleSessionName={SESSION_NAME}&Version=2011-06-15",
            aws::uri_encode(&self.role, false)
        );
        let call = Call {
            method: Method::POST,
            base: endpoint,
            path: "/".into(),
            query: &[],
            headers: &[(
                "content-type",
                "application/x-www-form-urlencoded; charset=utf-8",
            )],
            body: body.into_bytes(),
            limited: true,
        };
        let response = self.aws.send(call, &base, &self.region, "sts").await?;
        if response.status() != StatusCode::OK {
            return Err(failure(&format!("AssumeRole {}", self.role), response).await);
        }
        let text = response.text().await?;
        let field = |name: &str| {
            aws::xml_text(&text, name).ok_or_else(|| anyhow!("AssumeRole gave no {name}"))
        };
        let timed = Timed {
            credentials: Arc::new(Credentials {
                key_id: field("AccessKeyId")?,
                secret: field("SecretAccessKey")?,
                token: Some(field("SessionToken")?),
            }),
            expires: Some(expiry(&field("Expiration")?)?),
        };
        *cached = Some(timed.clone());
        Ok(timed.credentials)
    }

    /// The base URL and path of a key: virtual-hosted on AWS, path-style on a test endpoint or
    /// for a bucket name with a dot.
    fn target(&self, key: &str) -> (String, String) {
        aws::s3_location(
            &self.bucket,
            &self.region,
            self.aws.s3_endpoint.as_deref(),
            key,
        )
    }

    async fn call(
        &self,
        method: Method,
        key: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: Vec<u8>,
    ) -> Result<reqwest::Response> {
        let credentials = self.credentials().await?;
        let (base, path) = self.target(key);
        let limited = !(method == Method::GET && !key.is_empty());
        let call = Call {
            method,
            base,
            path,
            query,
            headers,
            body,
            limited,
        };
        self.aws.send(call, &credentials, &self.region, "s3").await
    }
}

impl Bucket for S3 {
    async fn list(&self, prefix: &str) -> Result<Vec<String>> {
        let mut keys = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix)];
            if let Some(t) = &token {
                query.push(("continuation-token", t));
            }
            let response = self.call(Method::GET, "", &query, &[], Vec::new()).await?;
            if response.status() != StatusCode::OK {
                return Err(failure(&format!("S3 list {prefix}"), response).await);
            }
            let text = response.text().await?;
            keys.extend(aws::xml_texts(&text, "Key"));
            token = aws::xml_text(&text, "NextContinuationToken").filter(|t| !t.is_empty());
            if token.is_none() {
                return Ok(keys);
            }
        }
    }

    async fn get(&self, key: &str) -> Result<Option<Object>> {
        let response = self.call(Method::GET, key, &[], &[], Vec::new()).await?;
        match response.status() {
            StatusCode::OK => Ok(Some(Object {
                length: response.content_length(),
                body: reqwest::Body::from(response)
                    .map_err(anyhow::Error::from)
                    .boxed_unsync(),
            })),
            StatusCode::NOT_FOUND => {
                let text = response.text().await.unwrap_or_default();
                match aws::xml_text(&text, "Code").as_deref() {
                    Some("NoSuchKey") => Ok(None),
                    code => bail!("S3 get {key}: HTTP 404 {}", code.unwrap_or_default()),
                }
            }
            _ => Err(failure(&format!("S3 get {key}"), response).await),
        }
    }

    async fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        let response = self
            .call(Method::PUT, key, &[], &[("if-none-match", "*")], body)
            .await?;
        match response.status() {
            StatusCode::OK => Ok(true),
            StatusCode::PRECONDITION_FAILED => Ok(false),
            _ => Err(failure(&format!("S3 put {key}"), response).await),
        }
    }

    async fn delete(&self, key: &str) -> Result<()> {
        let response = self.call(Method::DELETE, key, &[], &[], Vec::new()).await?;
        match response.status() {
            StatusCode::OK | StatusCode::NO_CONTENT => Ok(()),
            _ => Err(failure(&format!("S3 delete {key}"), response).await),
        }
    }
}

#[cfg(test)]
mod live {
    use super::*;

    /// Run with AWS credentials in the environment (for example from `aws configure
    /// export-credentials --format env`) and LIVE_ROLE, a role those credentials may not
    /// assume: real STS must refuse the role (AccessDenied), not the signature.
    #[tokio::test]
    #[ignore = "live: needs AWS credentials and LIVE_ROLE in the environment"]
    async fn real_sts_accepts_the_signature() {
        let aws = Aws::from_env().unwrap();
        let role = std::env::var("LIVE_ROLE").unwrap();
        let s3 = aws.bucket("layr-desk-live-check", "eu-central-1", &role);
        let error = s3.list("x/").await.unwrap_err().to_string();
        assert!(!error.contains("SignatureDoesNotMatch"), "{error}");
        assert!(error.contains("AccessDenied"), "{error}");
    }
}
