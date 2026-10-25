use super::aws::{self, Credentials, EMPTY_SHA256};
#[derive(Clone, Debug)]
pub struct Object {
    pub key: String,
    pub modified: i64,
}

use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::Client;
use reqwest::Method;
use sha2::{Digest, Sha256};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// The time limit of a call without a body, and the base of one with a body.
const LIMIT: Duration = Duration::from_secs(120);
/// The slowest upload that a put waits for, in bytes a second.
const SLOWEST: u64 = 100_000;
/// Switch to S3 multipart before the single-request limit of 5 GB.
const MULTIPART_THRESHOLD: usize = 256 << 20;
/// S3 permits at most 10,000 upload parts, each at most 5 GiB.
const S3_MAX_PARTS: usize = 10_000;
const S3_MAX_PART_BYTES: usize = 5 * 1024 * 1024 * 1024;
const S3_PART_BYTES: usize = 64 << 20;

struct Reply {
    status: u16,
    headers: reqwest::header::HeaderMap,
    body: Vec<u8>,
}

fn xml_escape(value: &str) -> String {
    value.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

#[derive(Clone)]
pub struct S3 {
    http: Client,
    credentials: Credentials,
    profile: Option<aws::profile::Profile>,
    // Shared by clones, so one refresh serves every copy of the client.
    refreshed: std::sync::Arc<std::sync::Mutex<Option<(Credentials, std::time::Instant)>>>,
    bucket: String,
    region: String,
    endpoint: Option<String>,
    prefix: String,
}

impl S3 {
    /// `endpoint` (`http://127.0.0.1:9000`) replaces AWS with a path-style test store.
    pub fn new(bucket: &str, region: &str, credentials: Credentials, endpoint: Option<&str>) -> Result<Self> {
        let http = Client::builder()
            .user_agent("layr")
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(30))
            .tcp_keepalive(Duration::from_secs(60))
            .build()?;
        Self::new_with_client(bucket, region, credentials, endpoint, http)
    }

    /// Use a caller-owned client with its connection policy and time limits.
    pub fn new_with_client(
        bucket: &str,
        region: &str,
        credentials: Credentials,
        endpoint: Option<&str>,
        http: Client,
    ) -> Result<Self> {
        Ok(S3 {
            http,
            credentials,
            profile: None,
            refreshed: std::sync::Arc::new(std::sync::Mutex::new(None)),
            bucket: bucket.to_string(),
            region: region.to_string(),
            endpoint: endpoint.map(str::to_string),
            prefix: String::new(),
        })
    }

    /// Return a signed response without consuming its body. The caller must bound reads.
    pub fn request(
        &self,
        method: Method,
        key: &str,
        query: &[(&str, &str)],
        headers: &[(&str, &str)],
        body: Vec<u8>,
        limit: Duration,
    ) -> Result<reqwest::blocking::Response> {
        let credentials = if let Some(profile) = &self.profile {
            let mut cached = self.refreshed.lock().unwrap();
            if cached.as_ref().is_none_or(|(_, at)| at.elapsed() >= Duration::from_secs(300)) {
                *cached = Some((profile.credentials(&self.http)?, std::time::Instant::now()));
            }
            cached.as_ref().unwrap().0.clone()
        } else {
            self.credentials.clone()
        };
        if credentials.key_id.is_empty() || credentials.secret.is_empty() {
            bail!("set AWS_ACCESS_KEY_ID and AWS_SECRET_ACCESS_KEY");
        }
        let (base, path) = aws::s3_location(&self.bucket, &self.region, self.endpoint.as_deref(), key);
        let url = reqwest::Url::parse(&base).map_err(|_| anyhow!("invalid S3 endpoint"))?;
        // The Host header that reqwest sends: the port only when it is not the default one.
        let host = match (url.host_str(), url.port()) {
            (Some(h), Some(p)) => format!("{h}:{p}"),
            (Some(h), None) => h.to_string(),
            _ => bail!("S3 endpoint has no host"),
        };
        let payload = if body.is_empty() { EMPTY_SHA256.to_string() } else { hex::encode(Sha256::digest(&body)) };
        let unix = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let signed = aws::sign(
            &aws::Request { method: method.as_str(), host: &host, path: &path, query, headers, payload_sha256: &payload },
            &credentials,
            &self.region,
            "s3",
            unix,
        );
        let mut target = format!("{base}{}", aws::uri_encode(&path, true));
        if !query.is_empty() {
            target = format!("{target}?{}", aws::query_string(query));
        }
        // A time limit for the whole call: reqwest's read timeout would end an upload that
        // takes longer than the limit, however steadily it goes.
        let has_body = method == Method::PUT || method == Method::POST;
        let mut request = self.http.request(method, target).timeout(limit);
        for (name, value) in headers.iter().copied() {
            request = request.header(name, value);
        }
        for (name, value) in &signed {
            request = request.header(name, value);
        }
        if has_body {
            request = request.body(body);
        }
        request.send().map_err(reqwest::Error::without_url).map_err(Into::into)
    }

    /// The status and body of one signed call.
    fn call(&self, method: Method, key: &str, query: &[(&str, &str)], headers: &[(&str, &str)], body: Vec<u8>) -> Result<Reply> {
        let limit = LIMIT + Duration::from_secs(body.len() as u64 / SLOWEST);
        let response = self.request(method, key, query, headers, body, limit)?;
        let status = response.status().as_u16();
        let headers = response.headers().clone();
        Ok(Reply { status, headers, body: response.bytes().map_err(reqwest::Error::without_url)?.to_vec() })
    }

    fn abort(&self, key: &str, id: &str) -> Result<()> {
        for attempt in 0..3 {
            let reply = self.call(Method::DELETE, key, &[("uploadId", id)], &[], Vec::new());
            match reply {
                Ok(reply) if matches!(reply.status, 200 | 204 | 404) => return Ok(()),
                Ok(reply) if attempt == 2 || (reply.status < 500 && reply.status != 429) => {
                    return Err(failure("S3 abort multipart", reply.status, &reply.body))
                }
                Err(error) if attempt == 2 => return Err(error),
                _ => std::thread::sleep(Duration::from_secs(attempt + 1)),
            }
        }
        unreachable!("the final abort attempt returns")
    }

    fn multipart(&self, key: &str, body: Vec<u8>, part_size: usize) -> Result<bool> {
        // Validate before creating an upload that would need manual cleanup.
        if part_size == 0 || part_size > S3_MAX_PART_BYTES || body.len().div_ceil(part_size) > S3_MAX_PARTS {
            bail!("S3 multipart object exceeds the part limits");
        }
        let reply = self.call(Method::POST, key, &[("uploads", "")], &[], Vec::new())?;
        if reply.status != 200 {
            return Err(failure("S3 create multipart", reply.status, &reply.body));
        }
        let id = aws::xml_text(&String::from_utf8_lossy(&reply.body), "UploadId")
            .filter(|s| !s.is_empty())
            .context("S3 create multipart returned no upload ID")?;
        let outcome = self.upload_parts(key, &id, body, part_size);
        if !matches!(outcome, Ok(true)) {
            if let Err(cleanup) = self.abort(key, &id) {
                bail!("S3 multipart {key}, upload {id}: {outcome:?}; cleanup failed: {cleanup:#}");
            }
        }
        outcome.with_context(|| format!("S3 multipart {key}, upload {id}"))
    }

    fn upload_parts(&self, key: &str, id: &str, body: Vec<u8>, part_size: usize) -> Result<bool> {
        let mut complete = String::from("<CompleteMultipartUpload>");
        for (index, start) in (0..body.len()).step_by(part_size).enumerate() {
            let number = (index + 1).to_string();
            let part = body[start..(start + part_size).min(body.len())].to_vec();
            let query = [("partNumber", number.as_str()), ("uploadId", id)];
            // UploadPart replaces only this part of this upload. Retrying the same bytes
            // after a lost response does not create another object or upload.
            let mut attempt = 0;
            let reply = loop {
                let result = self.call(Method::PUT, key, &query, &[], part.clone());
                let retry = match &result {
                    Err(_) => true,
                    Ok(r) => r.status == 429 || r.status >= 500,
                };
                if !retry || attempt == 2 {
                    break result?;
                }
                attempt += 1;
                std::thread::sleep(Duration::from_secs(attempt));
            };
            if reply.status != 200 {
                return Err(failure("S3 upload part", reply.status, &reply.body));
            }
            let etag = reply.headers.get("etag").context("S3 upload part returned no ETag")?.to_str()?;
            if etag.is_empty() {
                bail!("S3 upload part returned an empty ETag");
            }
            complete.push_str(&format!("<Part><PartNumber>{number}</PartNumber><ETag>{}</ETag></Part>", xml_escape(etag)));
        }
        complete.push_str("</CompleteMultipartUpload>");
        let reply = self.call(
            Method::POST,
            key,
            &[("uploadId", id)],
            &[("if-none-match", "*"), ("content-type", "application/xml")],
            complete.into_bytes(),
        )?;
        if reply.status == 412
            || (reply.status == 200
                && String::from_utf8_lossy(&reply.body).contains("<Error")
                && aws::xml_text(&String::from_utf8_lossy(&reply.body), "Code").as_deref() == Some("PreconditionFailed"))
        {
            return Ok(false);
        }
        // S3 can send HTTP 200 first, then an Error XML body. Only a complete result
        // confirms publication. A lost response remains an error, never false success.
        if reply.status != 200
            || !String::from_utf8_lossy(&reply.body).contains("<CompleteMultipartUploadResult")
            || String::from_utf8_lossy(&reply.body).contains("<Error")
            || aws::xml_text(&String::from_utf8_lossy(&reply.body), "ETag").is_none()
        {
            return Err(failure("S3 complete multipart", reply.status, &reply.body));
        }
        Ok(true)
    }
}

/// The service can echo credentials in XML. Keep response bodies out of errors.
fn failure(what: &str, status: u16, _reply: &[u8]) -> anyhow::Error {
    anyhow!("{what}: HTTP {status}")
}

impl S3 {
    pub fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        let key = self.key(key);
        let key = key.as_str();
        if body.len() > MULTIPART_THRESHOLD {
            let part_size = S3_PART_BYTES.max(body.len().div_ceil(S3_MAX_PARTS));
            return self.multipart(key, body, part_size);
        }
        let reply = self.call(Method::PUT, key, &[], &[("if-none-match", "*")], body).with_context(|| format!("S3 put {key}"))?;
        match reply.status {
            200 => Ok(true),
            412 => Ok(false),
            _ => Err(failure(&format!("S3 put {key}"), reply.status, &reply.body)),
        }
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<Object>> {
        let prefix = self.key(prefix);
        let (mut keys, mut token) = (Vec::new(), None::<String>);
        loop {
            let mut query = vec![("list-type", "2"), ("prefix", prefix.as_str())];
            if let Some(token) = &token {
                query.push(("continuation-token", token));
            }
            let reply = self.call(Method::GET, "", &query, &[], Vec::new())?;
            if reply.status != 200 {
                return Err(failure("S3 list", reply.status, &reply.body));
            }
            let xml = String::from_utf8(reply.body)?;
            if !xml.contains("<ListBucketResult") {
                bail!("S3 returned no object listing");
            }
            for item in xml.split("<Contents>").skip(1) {
                let item = item.split("</Contents>").next().unwrap_or(item);
                let key = aws::xml_text(item, "Key").context("S3 list entry has no key")?;
                let modified = aws::xml_text(item, "LastModified").context("S3 list entry has no time")?;
                let modified = chrono::DateTime::parse_from_rfc3339(&modified)?.timestamp();
                let relative = key.strip_prefix(&self.prefix).context("S3 returned a key outside the prefix")?;
                keys.push(Object { key: relative.to_string(), modified });
            }
            let next = aws::xml_text(&xml, "NextContinuationToken").filter(|t| !t.is_empty());
            if next.is_none() {
                if aws::xml_text(&xml, "IsTruncated").as_deref() == Some("true") {
                    bail!("S3 listing has no continuation token");
                }
                keys.sort_by(|a, b| a.key.cmp(&b.key));
                return Ok(keys);
            }
            if next == token {
                bail!("S3 listing repeats a continuation token");
            }
            token = next;
        }
    }
}

impl S3 {
    /// Resolve a short-lived command's credentials inside the caller's filesystem context.
    /// Read-only mounts retain their profile so later requests can refresh it.
    pub fn resolve_credentials(&mut self) -> Result<()> {
        if let Some(profile) = &self.profile {
            self.credentials = profile.credentials(&self.http)?;
            self.profile = None;
        }
        Ok(())
    }

    /// Parse the target without reading credentials or contacting AWS.
    pub fn unconnected(bucket: &str, prefix: &str) -> Result<Self> {
        let mut s =
            Self::new(bucket, "us-east-1", Credentials { key_id: String::new(), secret: String::new(), token: None }, None)?;
        let prefix = prefix.trim_matches('/');
        if !prefix.is_empty() {
            crate::paths::check_rel(prefix)?;
            s.prefix = format!("{prefix}/");
        }
        Ok(s)
    }

    pub fn with_env(bucket: &str, prefix: &str, env: &std::collections::HashMap<String, String>) -> Result<Self> {
        let credentials = Credentials {
            key_id: env.get("AWS_ACCESS_KEY_ID").cloned().unwrap_or_default(),
            secret: env.get("AWS_SECRET_ACCESS_KEY").cloned().unwrap_or_default(),
            token: env.get("AWS_SESSION_TOKEN").cloned(),
        };
        let profile = if credentials.key_id.is_empty() && credentials.secret.is_empty() {
            let name = env.get("AWS_PROFILE").map(String::as_str).filter(|s| !s.is_empty()).unwrap_or("default");
            Some(aws::profile::Profile::load_with_env(name, env)?)
        } else {
            None
        };
        let region = env
            .get("AWS_REGION")
            .or_else(|| env.get("AWS_DEFAULT_REGION"))
            .cloned()
            .or_else(|| profile.as_ref().and_then(|p| p.region.clone()))
            .unwrap_or_else(|| "us-east-1".into());
        let endpoint = env.get("AWS_ENDPOINT_URL_S3").or_else(|| env.get("AWS_ENDPOINT_URL")).cloned();
        let mut s = Self::new(bucket, &region, credentials, endpoint.as_deref())?;
        s.profile = profile;
        let prefix = prefix.trim_matches('/');
        if !prefix.is_empty() {
            crate::paths::check_rel(prefix)?;
            s.prefix = format!("{prefix}/");
        }
        Ok(s)
    }

    fn key(&self, key: &str) -> String {
        format!("{}{key}", self.prefix)
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        let reply = self.call(Method::GET, &self.key(key), &[], &[], Vec::new())?;
        match reply.status {
            200 => Ok(Some(reply.body)),
            404 => Ok(None),
            _ => Err(failure("S3 get", reply.status, &reply.body)),
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        let reply = self.call(Method::DELETE, &self.key(key), &[], &[], Vec::new())?;
        match reply.status {
            200 | 204 | 404 => Ok(()),
            _ => Err(failure("S3 delete", reply.status, &reply.body)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_errors_do_not_repeat_response_secrets() {
        let response = b"<Error><Code>private-access-key</Code><Message>private-token</Message></Error>";
        let error = failure("S3 put", 403, response).to_string();
        assert_eq!(error, "S3 put: HTTP 403");
    }

    /// Run against a disposable MinIO endpoint in the Linux integration VM.
    #[test]
    #[ignore = "requires LAYR_TEST_S3_BUCKET and caller AWS environment"]
    fn conditional_multipart_encrypted_round_trip() {
        let env: std::collections::HashMap<String, String> = std::env::vars().collect();
        let bucket = env.get("LAYR_TEST_S3_BUCKET").expect("test bucket");
        let prefix = format!("unit-{}", uuid::Uuid::now_v7());
        let s3 = S3::with_env(bucket, &prefix, &env).unwrap();
        let create = s3.call(Method::PUT, "", &[], &[], Vec::new()).unwrap();
        assert!(matches!(create.status, 200 | 409));
        let identity = super::super::keys::generate_identity();
        let recipient = super::super::keys::recipient_of(&identity).unwrap();
        let mut plaintext = vec![0u8; 11 << 20];
        use rand::RngCore;
        rand::rngs::OsRng.fill_bytes(&mut plaintext);
        let ciphertext = super::super::keys::encrypt(&recipient, &plaintext).unwrap();
        let object = s3.key("multipart.age");
        assert!(s3.multipart(&object, ciphertext.clone(), 5 << 20).unwrap());
        assert!(!s3.multipart(&object, ciphertext.clone(), 5 << 20).unwrap());
        let fetched = s3.get("multipart.age").unwrap().unwrap();
        assert_eq!(fetched, ciphertext);
        assert_eq!(super::super::keys::decrypt(&identity, &fetched).unwrap(), plaintext);
        assert!(s3.put_new("odd space & name", b"original".to_vec()).unwrap());
        assert!(!s3.put_new("odd space & name", b"replacement".to_vec()).unwrap());
        assert_eq!(s3.get("odd space & name").unwrap().unwrap(), b"original");
        let objects = s3.list("").unwrap();
        assert_eq!(objects.len(), 2);
        for object in objects {
            s3.delete(&object.key).unwrap();
        }
        assert!(s3.list("").unwrap().is_empty());
    }
}
