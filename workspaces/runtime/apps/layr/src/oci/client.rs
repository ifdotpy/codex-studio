//! OCI Distribution uploads. A failed request never advances a local ref.

use super::{Artifact, Cache, Descriptor, MANIFEST};
use anyhow::{anyhow, bail, Context, Result};
use reqwest::blocking::{Client, Response};
use reqwest::{Method, StatusCode, Url};
use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

pub struct Registry {
    http: Client,
    base: Url,
    pub repository: String,
    username: Option<String>,
    password: Option<String>,
    token: Option<String>,
    insecure: bool,
}

impl Registry {
    pub fn new(url: &str, insecure: bool, username: Option<String>, password: Option<String>) -> Result<Self> {
        let raw = url.strip_prefix("oci://").ok_or_else(|| anyhow!("expected oci://host/repository"))?;
        let u = Url::parse(&format!("{}://{raw}", if insecure { "http" } else { "https" }))?;
        if u.host_str().is_none()
            || !u.username().is_empty()
            || u.password().is_some()
            || u.query().is_some()
            || u.fragment().is_some()
        {
            bail!("invalid OCI remote; credentials belong in remote config");
        }
        let repository = u.path().trim_matches('/').to_string();
        if repository.is_empty() {
            bail!("OCI remote needs a repository path");
        }
        for c in repository.split('/') {
            super::valid_component(c)?;
        }
        let mut base = u;
        base.set_path("/");
        let http = Client::builder()
            .timeout(Duration::from_secs(300))
            .connect_timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self { http, base, repository, username, password, token: None, insecure })
    }

    fn endpoint(&self, repo: &str, suffix: &str) -> Result<Url> {
        Ok(self.base.join(&format!("v2/{repo}/{suffix}"))?)
    }

    fn request(&mut self, method: Method, url: Url, body: Option<&Path>, media: Option<&str>) -> Result<Response> {
        if url.origin() != self.base.origin() {
            bail!("registry upload location changes origin");
        }
        for attempt in 0..2 {
            let mut req = self.http.request(method.clone(), url.clone());
            if let Some(t) = &self.token {
                req = req.bearer_auth(t);
            } else if let Some(u) = &self.username {
                req = req.basic_auth(u, self.password.as_ref());
            }
            if let Some(p) = body {
                let tree = crate::tree::Tree::open(p.parent().context("upload path has no parent")?)?;
                req = req.body(tree.open_read(p.file_name().and_then(|s| s.to_str()).context("invalid upload filename")?)?);
            }
            if let Some(m) = media {
                req = req.header("Content-Type", m);
            }
            let response = req.send().map_err(|e| e.without_url()).context("registry request failed")?;
            if response.status() != StatusCode::UNAUTHORIZED || attempt == 1 {
                return Ok(response);
            }
            let challenge = response.headers().get("www-authenticate").and_then(|h| h.to_str().ok()).unwrap_or("").to_string();
            if !challenge.to_ascii_lowercase().starts_with("bearer ") {
                return Ok(response);
            }
            self.authenticate(&challenge)?;
        }
        unreachable!()
    }

    fn authenticate(&mut self, challenge: &str) -> Result<()> {
        let params = challenge_params(challenge)?;
        let mut realm = Url::parse(params.get("realm").ok_or_else(|| anyhow!("Bearer challenge has no realm"))?)?;
        if !realm.username().is_empty()
            || realm.password().is_some()
            || realm.fragment().is_some()
            || (realm.scheme() != "https" && !(self.insecure && realm.origin() == self.base.origin()))
        {
            bail!("registry token realm must use HTTPS (or the configured insecure registry origin)");
        }
        {
            let mut q = realm.query_pairs_mut();
            for key in ["service", "scope"] {
                if let Some(v) = params.get(key) {
                    q.append_pair(key, v);
                }
            }
        }
        let mut req = self.http.get(realm);
        if let Some(u) = &self.username {
            req = req.basic_auth(u, self.password.as_ref());
        }
        let response = req.send().map_err(|e| e.without_url()).context("registry token request failed")?;
        require(&response, &[200], "token request")?;
        let value: serde_json::Value = response.json().context("invalid registry token response")?;
        self.token = Some(
            value
                .get("token")
                .or_else(|| value.get("access_token"))
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .ok_or_else(|| anyhow!("registry token response has no token"))?
                .to_string(),
        );
        Ok(())
    }

    fn blob(&mut self, repo: &str, cache: &Cache, d: &Descriptor) -> Result<bool> {
        let r = self.request(Method::HEAD, self.endpoint(repo, &format!("blobs/{}", d.digest))?, None, None)?;
        if r.status() == StatusCode::OK {
            return Ok(false);
        }
        require(&r, &[404], "blob lookup")?;
        let r = self.request(Method::POST, self.endpoint(repo, "blobs/uploads/")?, None, None)?;
        require(&r, &[202], "start blob upload")?;
        let loc = r
            .headers()
            .get("location")
            .and_then(|h| h.to_str().ok())
            .ok_or_else(|| anyhow!("upload response has no Location"))?;
        let mut loc = r.url().join(loc)?;
        loc.query_pairs_mut().append_pair("digest", &d.digest);
        let r = self.request(Method::PUT, loc, Some(&cache.blob_path(&d.digest)?), Some("application/octet-stream"))?;
        require(&r, &[201], "finish blob upload")?;
        Ok(true)
    }

    pub fn push(&mut self, repo: &str, tag: &str, cache: &Cache, a: &Artifact) -> Result<usize> {
        if !super::valid_tag(tag) {
            bail!("invalid registry tag '{tag}'");
        }
        let mut uploaded = 0;
        for d in a.layers.iter().chain(std::iter::once(&a.config)) {
            uploaded += usize::from(self.blob(repo, cache, d)?);
        }
        let r = self.request(
            Method::PUT,
            self.endpoint(repo, &format!("manifests/{tag}"))?,
            Some(&cache.blob_path(&a.manifest.digest)?),
            Some(MANIFEST),
        )?;
        require(&r, &[201, 202], "manifest upload")?;
        if let Some(d) = r.headers().get("docker-content-digest").and_then(|h| h.to_str().ok()) {
            if d != a.manifest.digest {
                bail!("registry returned a different manifest digest");
            }
        }
        Ok(uploaded)
    }
}

fn require(r: &Response, codes: &[u16], what: &str) -> Result<()> {
    if !codes.contains(&r.status().as_u16()) {
        bail!("registry {what}: HTTP {}", r.status().as_u16());
    }
    Ok(())
}

fn challenge_params(s: &str) -> Result<BTreeMap<String, String>> {
    let (_, mut rest) = s.split_once(' ').ok_or_else(|| anyhow!("invalid Bearer challenge"))?;
    let mut out = BTreeMap::new();
    while !rest.trim().is_empty() {
        rest = rest.trim_start().trim_start_matches(',').trim_start();
        let (key, val) = rest.split_once('=').ok_or_else(|| anyhow!("invalid Bearer challenge parameter"))?;
        if let Some(v) = val.strip_prefix('"') {
            let end = v.find('"').ok_or_else(|| anyhow!("invalid quoted Bearer parameter"))?;
            out.insert(key.to_lowercase(), v[..end].to_string());
            rest = &v[end + 1..];
        } else {
            let (v, tail) = val.split_once(',').unwrap_or((val, ""));
            out.insert(key.to_lowercase(), v.trim().to_string());
            rest = tail;
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn challenge_keeps_scope_commas() {
        let p =
            challenge_params(r#"Bearer realm="https://auth.example/token",service="r",scope="repository:a:pull,push""#).unwrap();
        assert_eq!(p["scope"], "repository:a:pull,push");
        assert!(challenge_params("Bearer realm=\"unterminated").is_err());
    }
    #[test]
    fn remote_validation() {
        assert!(Registry::new("oci://localhost:5000/p/app", true, None, None).is_ok());
        for u in ["oci://x", "oci://u:p@x/a", "oci://x/a?token=bad", "oci://x/A", "https://x/a"] {
            assert!(Registry::new(u, false, None, None).is_err(), "{u}");
        }
    }
}
