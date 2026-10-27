//! HTTPS object transport. Read proofs and write grants exist only in memory.

use super::{backend::Object, keys};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use reqwest::{
    blocking::{Client, Response},
    Method,
};
use serde::Deserialize;
use std::io::Read;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::sync::Mutex;
use std::time::Duration;

const OBJECT_LIMIT: usize = 32 << 20;
const SLOTS_LIMIT: usize = 8 << 20;

pub struct Gateway {
    http: Client,
    base: reqwest::Url,
    token: Option<String>,
    identity: Mutex<Option<String>>,
    read: Mutex<Option<(String, i64)>>,
}

impl Clone for Gateway {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            base: self.base.clone(),
            token: self.token.clone(),
            identity: Mutex::new(self.identity.lock().expect("gateway identity lock").clone()),
            read: Mutex::new(self.read.lock().expect("gateway read lock").clone()),
        }
    }
}

fn bytes(response: Response, limit: usize) -> Result<Vec<u8>> {
    if response.content_length().is_some_and(|n| n > limit as u64) {
        bail!("gateway response is too large");
    }
    let mut data = Vec::new();
    response.take(limit as u64 + 1).read_to_end(&mut data).map_err(|_| anyhow!("gateway body read failed"))?;
    if data.len() > limit {
        bail!("gateway response is too large");
    }
    Ok(data)
}
fn json<T: serde::de::DeserializeOwned>(response: Response, limit: usize) -> Result<T> {
    if !response.status().is_success() {
        bail!("gateway: HTTP {}", response.status().as_u16());
    }
    serde_json::from_slice(&bytes(response, limit)?).map_err(|_| anyhow!("invalid gateway JSON response"))
}

fn ca_file(path: &std::path::Path) -> Result<Vec<u8>> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("open gateway CA file")?;
    if !file.metadata()?.is_file() {
        bail!("gateway CA must be a regular file");
    }
    let mut bytes = Vec::new();
    file.take(1_048_577).read_to_end(&mut bytes)?;
    if bytes.len() > 1_048_576 {
        bail!("gateway CA file is too large");
    }
    Ok(bytes)
}

impl Gateway {
    pub fn open(url: &str) -> Result<Self> {
        let address = url.strip_prefix("layr+https://").context("invalid gateway URL scheme")?;
        let (_, project_text) = address.split_once('/').context("gateway URL needs a project name")?;
        if project_text.is_empty() || !project_text.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-')) {
            bail!("invalid gateway project path");
        }
        let mut base = reqwest::Url::parse(&format!("https://{address}")).map_err(|_| anyhow!("invalid gateway URL"))?;
        let project = base.path().strip_prefix('/').unwrap_or("");
        if base.host_str().is_none()
            || !base.username().is_empty()
            || base.password().is_some()
            || base.query().is_some()
            || base.fragment().is_some()
            || !(1..=44).contains(&project.len())
            || !project.as_bytes()[0].is_ascii_alphanumeric()
            || !project.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-'))
        {
            bail!("gateway URL needs a host and one project name, without credentials or query");
        }
        let path = format!("/v1/p/{project}/");
        base.set_path(&path);
        let http = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(150))
            .build()?;
        Ok(Self { http, base, token: None, identity: Mutex::new(None), read: Mutex::new(None) })
    }

    pub fn connect(&mut self, ctx: &crate::ctx::Ctx) -> Result<()> {
        self.token = ctx
            .env("LAYR_GATEWAY_TOKEN_FILE")
            .map(|path| crate::privs::as_caller_fs(ctx, || token_file(&ctx.cwd.join(path), ctx.caller.uid)))
            .transpose()?;
        if let Some(path) = ctx.env("LAYR_GATEWAY_CA_FILE") {
            let pem = crate::privs::as_caller_fs(ctx, || ca_file(&ctx.cwd.join(path)))?;
            self.configure_ca(&pem)?;
        }
        Ok(())
    }

    /// A standalone reader never loads a write or GC token.
    pub fn connect_reader(&mut self, env: &std::collections::HashMap<String, String>) -> Result<()> {
        self.token = None;
        if let Some(path) = env.get("LAYR_GATEWAY_CA_FILE") {
            self.configure_ca(&ca_file(std::path::Path::new(path))?)?;
        }
        Ok(())
    }

    fn configure_ca(&mut self, pem: &[u8]) -> Result<()> {
        let certificates = reqwest::Certificate::from_pem_bundle(pem).context("invalid gateway CA file")?;
        if certificates.is_empty() {
            bail!("gateway CA file has no certificates");
        }
        let mut builder = Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(Duration::from_secs(5))
            .timeout(Duration::from_secs(150));
        for certificate in certificates {
            builder = builder.add_root_certificate(certificate);
        }
        self.http = builder.build()?;
        Ok(())
    }

    /// Desk person API on the same authenticated HTTPS origin.
    pub fn person_call(
        &self,
        method: Method,
        path: &str,
        token: Option<&str>,
        body: Option<&serde_json::Value>,
    ) -> Result<serde_json::Value> {
        let url = self.base.join(path)?;
        let mut request = self.http.request(method.clone(), url).header("content-type", "application/json");
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        if let Some(body) = body {
            request = request.json(body);
        }
        let response = request.send().map_err(reqwest::Error::without_url)?;
        json(response, 512 << 10)
    }

    pub fn person_slot_at(&self, project: &str, login: &str, recipient: &str) -> Result<Vec<u8>> {
        if !layr_store::person::project_name(project) {
            bail!("invalid gateway project name");
        }
        let slot = layr_store::person::slot_name(login, recipient)?;
        let url = self.base.join(&format!("/v1/p/{project}/slots/{slot}"))?;
        let response = self.http.get(url).send().map_err(reqwest::Error::without_url)?;
        if !response.status().is_success() {
            bail!("gateway person slot: HTTP {}", response.status().as_u16());
        }
        bytes(response, 4096)
    }

    pub fn person_view(&self) -> Result<serde_json::Value> {
        self.person_call(Method::GET, "/api/person", None, None)
    }

    pub fn set_identity(&self, identity: &str) -> Result<()> {
        *self.identity.lock().map_err(|_| anyhow!("gateway identity lock failed"))? = Some(identity.into());
        Ok(())
    }

    fn send(
        &self,
        method: Method,
        action: &str,
        bearer: Option<&str>,
        query: Option<(&str, &str)>,
        data: Option<Vec<u8>>,
    ) -> Result<Response> {
        let mut url = self.base.clone();
        url.set_path(&format!("{}{action}", self.base.path()));
        if let Some((key, value)) = query {
            url.query_pairs_mut().append_pair(key, value);
        }
        let mut request = self.http.request(method, url);
        if let Some(token) = bearer {
            request = request.bearer_auth(token);
        }
        if let Some(data) = data {
            request = request
                .header("content-type", if action == "token" { "application/json" } else { "application/octet-stream" })
                .body(data);
        }
        request.send().map_err(reqwest::Error::without_url).map_err(Into::into)
    }

    fn proof(&self) -> Result<String> {
        let mut cached = self.read.lock().map_err(|_| anyhow!("gateway read token lock failed"))?;
        if let Some((token, _)) = cached.as_ref().filter(|(_, expires)| *expires > crate::ids::now_ms() + 10_000) {
            return Ok(token.clone());
        }
        let identity = self
            .identity
            .lock()
            .map_err(|_| anyhow!("gateway identity lock failed"))?
            .clone()
            .context("gateway reads require a machine slot or paper key")?;
        #[derive(Deserialize)]
        struct Challenge {
            challenge: String,
        }
        let challenge: Challenge = json(self.send(Method::GET, "challenge", None, None, None)?, 16 << 10)?;
        let sealed = base64::engine::general_purpose::STANDARD
            .decode(challenge.challenge)
            .map_err(|_| anyhow!("invalid gateway challenge encoding"))?;
        let plain =
            keys::decrypt(&identity, &sealed).map_err(|_| anyhow!("gateway challenge does not match the store identity"))?;
        let proof = String::from_utf8(plain).map_err(|_| anyhow!("invalid gateway challenge"))?;
        #[derive(Deserialize)]
        struct Token {
            token: String,
            expires: i64,
        }
        let token: Token = json(
            self.send(Method::POST, "token", None, None, Some(serde_json::to_vec(&serde_json::json!({"challenge":proof}))?))?,
            16 << 10,
        )?;
        let now = crate::ids::now_ms();
        if token.token.len() > 256
            || token.expires <= now
            || token.expires > now + 600_000
            || !token.token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
        {
            bail!("invalid gateway read token");
        }
        *cached = Some((token.token.clone(), token.expires));
        Ok(token.token)
    }

    fn read_request(&self, action: &str, query: Option<(&str, &str)>) -> Result<Response> {
        let token = self.proof()?;
        let response = self.send(Method::GET, action, Some(&token), query, None)?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        // A gateway restart discards its in-memory HMAC key. Exchange one new proof.
        *self.read.lock().map_err(|_| anyhow!("gateway read token lock failed"))? = None;
        let token = self.proof()?;
        self.send(Method::GET, action, Some(&token), query, None)
    }

    pub fn has_read_identity(&self) -> bool {
        self.identity.lock().map(|identity| identity.is_some()).unwrap_or(false)
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        if key == "vault.json" {
            return Ok(None);
        }
        if key.strip_prefix("slots/").is_some_and(|name| layr_store::person::slot_login(name).is_some()) {
            let response = self.send(Method::GET, key, None, None, None)?;
            return match response.status().as_u16() {
                200 => Ok(Some(bytes(response, 4096)?)),
                404 => Ok(None),
                status => bail!("gateway person slot: HTTP {status}"),
            };
        }
        if key.starts_with("slots/") {
            #[derive(Deserialize)]
            struct Slot {
                name: String,
                data: String,
            }
            #[derive(Deserialize)]
            struct Slots {
                slots: Vec<Slot>,
            }
            let slots: Slots = json(self.send(Method::GET, "slots", None, None, None)?, SLOTS_LIMIT)?;
            if slots.slots.len() > 1024 {
                bail!("too many gateway slots");
            }
            let name = key.strip_prefix("slots/").unwrap().strip_suffix(".age").context("invalid slot name")?;
            let mut found = None;
            for slot in slots.slots {
                if slot.name == name {
                    if found.is_some() {
                        bail!("duplicate gateway slot");
                    }
                    let data = base64::engine::general_purpose::STANDARD
                        .decode(slot.data)
                        .map_err(|_| anyhow!("invalid gateway slot encoding"))?;
                    if data.len() > 16 << 10 {
                        bail!("gateway slot is too large");
                    }
                    found = Some(data);
                }
            }
            return Ok(found);
        }
        let response = if overlay_keys_path(key) && !self.has_read_identity() {
            self.send(Method::GET, &format!("objects/{key}"), self.token.as_deref(), None, None)?
        } else if key == "store.json" {
            self.send(Method::GET, "store.json", None, None, None)?
        } else {
            self.read_request(&format!("objects/{key}"), None)?
        };
        match response.status().as_u16() {
            404 => Ok(None),
            200 => Ok(Some(bytes(response, if key == "store.json" { 16 << 10 } else { OBJECT_LIMIT })?)),
            status => bail!("gateway get: HTTP {status}"),
        }
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<Object>> {
        #[derive(Deserialize)]
        struct Entry {
            key: String,
            modified: i64,
        }
        #[derive(Deserialize)]
        struct Objects {
            objects: Vec<Entry>,
        }
        let has_identity = self.identity.lock().map_err(|_| anyhow!("gateway identity lock failed"))?.is_some();
        let response = if !has_identity
            && (prefix.starts_with("records/") || prefix.starts_with("states/") || overlay_keys_prefix(prefix))
        {
            self.send(Method::GET, "objects", self.token.as_deref(), Some(("prefix", prefix)), None)?
        } else {
            self.read_request("objects", Some(("prefix", prefix)))?
        };
        let listing: Objects = json(response, 8 << 20)?;
        if listing.objects.len() > 10_000 {
            bail!("too many gateway objects");
        }
        let mut found = std::collections::BTreeSet::new();
        let mut objects = Vec::new();
        for entry in listing.objects {
            super::backend::check_key(&entry.key)?;
            if !entry.key.starts_with(prefix) || !found.insert(entry.key.clone()) {
                bail!("invalid gateway object listing");
            }
            objects.push(Object { key: entry.key, modified: entry.modified / 1000 });
        }
        objects.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(objects)
    }

    pub fn put_new(&self, key: &str, data: Vec<u8>) -> Result<bool> {
        if key == "store.json" {
            bail!("create the store with owner credentials before adding a gateway remote");
        }
        if data.len() > OBJECT_LIMIT {
            bail!("gateway object exceeds 32 MiB");
        }
        let token = if key.starts_with("slots/") {
            self.proof()?
        } else {
            self.token.clone().context("set LAYR_GATEWAY_TOKEN_FILE to a private write token file")?
        };
        let response = self.send(Method::PUT, &format!("objects/{key}"), Some(&token), None, Some(data))?;
        match response.status().as_u16() {
            201 => Ok(true),
            412 => Ok(false),
            status => bail!("gateway put: HTTP {status}"),
        }
    }
    pub fn delete(&self, key: &str) -> Result<()> {
        let token = self.token.as_deref().context("set LAYR_GATEWAY_TOKEN_FILE to a private gc token file")?;
        let response = self.send(Method::DELETE, &format!("objects/{key}"), Some(token), None, None)?;
        match response.status().as_u16() {
            204 => Ok(()),
            status => bail!("gateway delete: HTTP {status}"),
        }
    }
}

fn overlay_keys_prefix(prefix: &str) -> bool {
    let parts: Vec<_> = prefix.split('/').collect();
    parts.len() == 4 && parts[0] == "overlays" && crate::model::is_uuid(parts[1]) && parts[2] == "keys" && parts[3].is_empty()
}

fn overlay_keys_path(key: &str) -> bool {
    let mut parts = key.split('/');
    parts.next() == Some("overlays")
        && parts.next().is_some_and(crate::model::is_uuid)
        && parts.next() == Some("keys")
        && parts.next().is_some_and(|name| name.ends_with(".json"))
        && parts.next().is_none()
}

fn token_file(path: &std::path::Path, uid: u32) -> Result<String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(path)
        .context("open gateway token file")?;
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.uid() != uid || metadata.mode() & 0o7777 != 0o600 {
        bail!("gateway token file must be a regular file owned by the caller with mode 0600");
    }
    let mut text = String::new();
    file.take(257).read_to_string(&mut text).map_err(|_| anyhow!("invalid gateway token file"))?;
    let token = text.trim_end_matches(['\r', '\n']);
    if text.len() > 256 || token.is_empty() || !token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.') {
        bail!("invalid gateway token file");
    }
    Ok(token.into())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{symlink, PermissionsExt};
    #[test]
    fn reader_hooks_ignore_write_grants_set_identity_and_check_the_ca_file() {
        use super::super::backend::Backend;
        let mut env = std::collections::HashMap::from([("LAYR_GATEWAY_TOKEN_FILE".into(), "/missing/write-token".into())]);
        let backend = Backend::for_reader("layr+https://localhost/example", &env).unwrap();
        backend.unlock_reader("reader-test-identity").unwrap();
        let Backend::Gateway(gateway) = backend else { unreachable!() };
        assert!(gateway.token.is_none());
        assert!(gateway.identity.lock().unwrap().as_deref() == Some("reader-test-identity"));
        env.insert("LAYR_GATEWAY_CA_FILE".into(), "/missing/reader-ca".into());
        assert!(Backend::for_reader("layr+https://localhost/example", &env).is_err());
    }

    #[test]
    fn urls_refuse_credentials_queries_and_path_escapes() {
        assert!(Gateway::open("layr+https://cloud.example/project").is_ok());
        for url in [
            "layr+https://u:secret@cloud.example/p",
            "layr+https://cloud.example/p?token=secret",
            "layr+https://cloud.example/p#fragment",
            "layr+https://cloud.example/a/b",
            "layr+https://cloud.example/%2e%2e",
            "layr+https://cloud.example/",
        ] {
            let error = Gateway::open(url).err().expect("invalid URL").to_string();
            assert!(!error.contains("secret"));
        }
    }
    #[test]
    fn token_files_require_owner_private_mode_and_regular_no_follow_file() {
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&dir).unwrap();
        let path = dir.join("token");
        std::fs::write(&path, "v1.write.123.abc\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        let uid = rustix::process::geteuid().as_raw();
        assert_eq!(token_file(&path, uid).unwrap(), "v1.write.123.abc");
        assert!(token_file(&path, uid.wrapping_add(1)).is_err());
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        assert!(token_file(&path, uid).is_err());
        symlink(&path, dir.join("link")).unwrap();
        assert!(token_file(&dir.join("link"), uid).is_err());
        assert!(token_file(&dir, uid).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
