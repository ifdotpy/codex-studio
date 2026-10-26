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
use std::sync::{Arc, Mutex};
use std::time::Duration;

const OBJECT_LIMIT: usize = 32 << 20;
const SLOTS_LIMIT: usize = 8 << 20;
const CLOCK_SKEW_MS: i64 = 5 * 60 * 1000;

fn valid_token_expiry(expires: i64, now: i64, lifetime: i64) -> bool {
    expires > now && expires <= now + lifetime + CLOCK_SKEW_MS
}

struct PersonSession {
    login: String,
    key: ed25519_dalek::SigningKey,
    token: Option<SessionToken>,
}
#[derive(Deserialize)]
struct SessionToken {
    token: String,
    expires: i64,
    login: String,
}

pub struct Gateway {
    http: Client,
    base: reqwest::Url,
    token: Option<String>,
    machine: Option<String>,
    identity: Mutex<Option<String>>,
    epoch: Mutex<Option<String>>,
    read: Mutex<Option<(String, i64)>>,
    public: std::sync::atomic::AtomicBool,
    person: Arc<Mutex<Option<PersonSession>>>,
}

impl Clone for Gateway {
    fn clone(&self) -> Self {
        Self {
            http: self.http.clone(),
            base: self.base.clone(),
            token: self.token.clone(),
            machine: self.machine.clone(),
            identity: Mutex::new(self.identity.lock().expect("gateway identity lock").clone()),
            epoch: Mutex::new(self.epoch.lock().expect("gateway epoch lock").clone()),
            read: Mutex::new(self.read.lock().expect("gateway read lock").clone()),
            public: std::sync::atomic::AtomicBool::new(self.is_public()),
            person: self.person.clone(),
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
        Ok(Self {
            http,
            base,
            token: None,
            machine: None,
            identity: Mutex::new(None),
            epoch: Mutex::new(None),
            read: Mutex::new(None),
            public: std::sync::atomic::AtomicBool::new(false),
            person: Arc::new(Mutex::new(None)),
        })
    }

    pub fn connect(&mut self, ctx: &crate::ctx::Ctx) -> Result<()> {
        self.machine = Some(ctx.store.machine.id.clone());
        self.token = ctx
            .env("LAYR_GATEWAY_TOKEN_FILE")
            .map(|path| crate::privs::as_caller_fs(ctx, || token_file(&ctx.cwd.join(path), ctx.caller.uid)))
            .transpose()?;
        if let Some(path) = ctx.env("LAYR_GATEWAY_CA_FILE") {
            let pem = crate::privs::as_caller_fs(ctx, || ca_file(&ctx.cwd.join(path)))?;
            self.configure_ca(&pem)?;
        }
        crate::privs::as_caller_fs(ctx, || self.connect_person(&ctx.env, &ctx.caller.home, &ctx.cwd, ctx.caller.uid))?;
        Ok(())
    }

    /// A standalone reader never loads a write or GC token.
    pub fn connect_reader(&mut self, env: &std::collections::HashMap<String, String>) -> Result<()> {
        self.connect_public(env)?;
        let uid = rustix::process::getuid().as_raw();
        self.connect_person(env, &crate::ctx::Caller::from_uid(uid).home, &std::env::current_dir()?, uid)?;
        Ok(())
    }

    /// Explicit anonymous access must not open person keys or exchange a session.
    pub fn connect_public(&mut self, env: &std::collections::HashMap<String, String>) -> Result<()> {
        self.token = None;
        self.machine = None;
        *self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))? = None;
        if let Some(path) = env.get("LAYR_GATEWAY_CA_FILE") {
            self.configure_ca(&ca_file(std::path::Path::new(path))?)?;
        }
        Ok(())
    }

    fn connect_person(
        &self,
        env: &std::collections::HashMap<String, String>,
        home: &std::path::Path,
        cwd: &std::path::Path,
        uid: u32,
    ) -> Result<()> {
        if let Some(access) = crate::person::load(env, home, cwd, uid, None)? {
            let key = crate::person::signing_key_at(env, home, uid, &access)?;
            self.set_person(&access.login, key)?;
        }
        Ok(())
    }

    pub fn set_person(&self, login: &str, key: ed25519_dalek::SigningKey) -> Result<()> {
        if !layr_store::person::login_name(login) {
            bail!("invalid person login");
        }
        let mut person = self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?;
        if !person.as_ref().is_some_and(|p| p.login == login && p.key == key) {
            *person = Some(PersonSession { login: login.into(), key, token: None });
        }
        Ok(())
    }

    fn session_token(&self, renew: bool) -> Result<String> {
        let mut person = self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?;
        let person = person.as_mut().context("set LAYR_PERSON_KEY_FILE to sign in to the gateway")?;
        if !renew {
            if let Some(token) = person.token.as_ref().filter(|t| t.expires > crate::ids::now_ms() + 10_000) {
                return Ok(token.token.clone());
            }
        }
        person.token = None;
        let mut url = self.base.join("/api/session/challenge")?;
        url.query_pairs_mut().append_pair("login", &person.login);
        #[derive(Deserialize)]
        struct Challenge {
            challenge: String,
            expires: i64,
        }
        let challenge: Challenge = json(self.http.get(url).send().map_err(reqwest::Error::without_url)?, 4096)?;
        let now = crate::ids::now_ms();
        if challenge.expires <= now || challenge.expires > now + 60_000 {
            bail!("invalid or expired gateway session challenge");
        }
        let signature = layr_store::session::sign(&person.login, &challenge.challenge, &person.key)?;
        let response = self
            .http
            .post(self.base.join("/api/session")?)
            .json(&serde_json::json!({"login":person.login,"challenge":challenge.challenge,"signature":signature}))
            .send()
            .map_err(reqwest::Error::without_url)?;
        let token: SessionToken = json(response, 4096)?;
        let now = crate::ids::now_ms();
        if token.login != person.login
            || token.token.is_empty()
            || token.token.len() > 256
            || !token.token.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-' | b'+' | b'/' | b'='))
            || !valid_token_expiry(token.expires, now, 43_200_000)
        {
            bail!("invalid gateway person session");
        }
        let result = token.token.clone();
        person.token = Some(token);
        Ok(result)
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
        let send = |bearer: &str| -> Result<Response> {
            let mut request = self
                .http
                .request(method.clone(), self.base.join(path)?)
                .header("content-type", "application/json")
                .bearer_auth(bearer);
            if let Some(body) = body {
                request = request.json(body);
            }
            request.send().map_err(reqwest::Error::without_url).map_err(Into::into)
        };
        if let Some(token) = token {
            return json(send(token)?, 512 << 10);
        }
        let response = send(&self.session_token(false)?)?;
        if response.status().as_u16() != 401 {
            return json(response, 512 << 10);
        }
        // A rejected session has not authorized the operation. Renew once, including after restart.
        json(send(&self.session_token(true)?)?, 512 << 10)
    }

    pub fn person_slot_at(&self, project: &str, login: &str, recipient: &str) -> Result<Vec<u8>> {
        if !layr_store::person::project_name(project) {
            bail!("invalid gateway project name");
        }
        let slot = layr_store::person::slot_name(login, recipient)?;
        let url = self.base.join(&format!("/v1/p/{project}/slots/{slot}"))?;
        let response =
            self.http.get(url.clone()).bearer_auth(self.session_token(false)?).send().map_err(reqwest::Error::without_url)?;
        let response = if response.status().as_u16() == 401 {
            self.http.get(url).bearer_auth(self.session_token(true)?).send().map_err(reqwest::Error::without_url)?
        } else {
            response
        };
        if !response.status().is_success() {
            bail!("gateway person slot: HTTP {}", response.status().as_u16());
        }
        bytes(response, 4096)
    }

    pub fn person_view(&self) -> Result<serde_json::Value> {
        let record = self.person_call(Method::GET, "/api/person", None, None)?;
        let person = self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?;
        if record["login"].as_str() != person.as_ref().map(|p| p.login.as_str()) {
            bail!("gateway person record differs from the session login");
        }
        Ok(record)
    }

    pub fn set_epoch(&self, epoch: Option<&str>) -> Result<()> {
        let mut current = self.epoch.lock().map_err(|_| anyhow!("gateway epoch lock failed"))?;
        if current.as_deref() != epoch {
            *current = epoch.map(str::to_owned);
            drop(current);
            *self.read.lock().map_err(|_| anyhow!("gateway read token lock failed"))? = None;
        }
        Ok(())
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
        self.send_query(method, action, bearer, query.as_slice(), data)
    }

    fn send_query(
        &self,
        method: Method,
        action: &str,
        bearer: Option<&str>,
        query: &[(&str, &str)],
        data: Option<Vec<u8>>,
    ) -> Result<Response> {
        let mut url = self.base.clone();
        url.set_path(&format!("{}{action}", self.base.path()));
        for (key, value) in query {
            url.query_pairs_mut().append_pair(key, value);
        }
        let person = bearer.is_none() && self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?.is_some();
        let send = |token: Option<&str>| -> Result<Response> {
            let mut request = self.http.request(method.clone(), url.clone());
            if let Some(token) = token {
                request = request.bearer_auth(token);
            }
            if let Some(data) = &data {
                request = request
                    .header("content-type", if action == "token" { "application/json" } else { "application/octet-stream" })
                    .body(data.clone());
            }
            request.send().map_err(reqwest::Error::without_url).map_err(Into::into)
        };
        if person {
            let response = send(Some(&self.session_token(false)?))?;
            if response.status().as_u16() == 401 {
                return send(Some(&self.session_token(true)?));
            }
            Ok(response)
        } else {
            let bootstrap = self.bootstrap_action(&method, action)
                && (query.is_empty()
                    || query.len() == 1
                        && query.first().is_some_and(|(name, epoch)| {
                            *name == "epoch" && super::format::is_uuid(epoch) && matches!(action, "challenge" | "token")
                        }));
            let response = send(bearer.or_else(|| if bootstrap { self.token.as_deref() } else { None }))?;
            if bootstrap && response.status().as_u16() == 404 && (action != "store.json" || self.token.is_some()) {
                bail!("gateway bootstrap: HTTP 404 (no such project or machine slot); check the project name and a missing or wrong LAYR_GATEWAY_TOKEN_FILE grant");
            }
            Ok(response)
        }
    }

    fn machine_slot(&self) -> Option<String> {
        self.machine.as_ref().map(|id| format!("slots/machine-{id}.age"))
    }

    fn bootstrap_action(&self, method: &Method, action: &str) -> bool {
        let logical = epoch_path(action).map(|(_, key)| key).unwrap_or(action);
        (*method == Method::GET
            && (matches!(action, "store.json" | "challenge") || self.machine_slot().as_deref() == Some(logical)))
            || (*method == Method::POST && action == "token")
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
        let epoch = self.epoch.lock().map_err(|_| anyhow!("gateway epoch lock failed"))?.clone();
        let query = epoch.as_deref().map(|e| ("epoch", e));
        let challenge: Challenge = json(self.send(Method::GET, "challenge", None, query, None)?, 16 << 10)?;
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
            self.send(Method::POST, "token", None, query, Some(serde_json::to_vec(&serde_json::json!({"challenge":proof}))?))?,
            16 << 10,
        )?;
        let now = crate::ids::now_ms();
        if token.token.len() > 256
            || !valid_token_expiry(token.expires, now, 600_000)
            || !token.token.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'.')
        {
            bail!("invalid gateway read token");
        }
        *cached = Some((token.token.clone(), token.expires));
        Ok(token.token)
    }

    fn read_request(&self, action: &str, query: Option<(&str, &str)>) -> Result<Response> {
        self.read_request_query(action, query.as_slice())
    }

    fn read_request_query(&self, action: &str, query: &[(&str, &str)]) -> Result<Response> {
        let token = self.proof().with_context(|| format!("gateway read {action}"))?;
        let response = self.send_query(Method::GET, action, Some(&token), query, None)?;
        if response.status().as_u16() != 401 {
            return Ok(response);
        }
        // A gateway restart discards its in-memory HMAC key. Exchange one new proof.
        *self.read.lock().map_err(|_| anyhow!("gateway read token lock failed"))? = None;
        let token = self.proof()?;
        self.send_query(Method::GET, action, Some(&token), query, None)
    }

    pub fn prove_reader(&self) -> Result<()> {
        self.proof().map(|_| ())
    }

    pub fn has_read_identity(&self) -> bool {
        self.identity.lock().map(|identity| identity.is_some()).unwrap_or(false)
    }

    pub(super) fn use_public_access(&self) {
        self.public.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    fn is_public(&self) -> bool {
        self.public.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn public_request(&self, action: &str, prefix: Option<&str>) -> Result<Response> {
        self.public_request_query(action, prefix.map(|value| ("prefix", value)).as_slice())
    }

    fn public_request_query(&self, action: &str, query: &[(&str, &str)]) -> Result<Response> {
        let mut url = self.base.clone();
        let path = self.base.path().strip_prefix("/v1").context("invalid gateway base")?;
        url.set_path(&format!("{path}{action}"));
        for (key, value) in query {
            url.query_pairs_mut().append_pair(key, value);
        }
        // Anonymous routes are GET only and never carry a token or private read proof.
        self.http.get(url).send().map_err(reqwest::Error::without_url).map_err(Into::into)
    }

    /// A side metadata request must not turn a writer into a public-only reader.
    pub(super) fn public_metadata(&self) -> Result<Option<Vec<u8>>> {
        self.public_object("public.json", false)
    }

    pub(super) fn public_get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        self.public_object(key, true)
    }

    fn public_object(&self, key: &str, reader: bool) -> Result<Option<Vec<u8>>> {
        let logical = epoch_path(key).map(|(_, k)| k).unwrap_or(key);
        if !key.starts_with("keys/")
            && !logical.starts_with("history/")
            && !matches!(logical, "store.json" | "public.json" | "slots/public.age")
            && super::format::parse_segment_name(logical).is_none()
            && super::format::parse_tree_name(logical).is_none()
            && super::format::parse_data_name(logical).is_none()
            && !super::format::release_file(logical)
        {
            return Ok(None);
        }
        let response = self.public_request(key, None)?;
        match response.status().as_u16() {
            200 => {
                let body = bytes(
                    response,
                    if matches!(key, "store.json" | "public.json" | "slots/public.age") { 16 << 10 } else { OBJECT_LIMIT },
                )?;
                if reader && key == "public.json" {
                    self.public.store(true, std::sync::atomic::Ordering::SeqCst);
                }
                Ok(Some(body))
            }
            404 => Ok(None),
            status => bail!("gateway public object: HTTP {status}"),
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        if key == "vault.json" {
            return Ok(None);
        }
        if key == "public.json" {
            return self.public_metadata();
        }
        if epoch_path(key).is_some_and(|(_, k)| k == "public.json") {
            return self.public_object(key, false);
        }
        if self.is_public() {
            return self.public_get(key);
        }
        let (slot_key, slot_epoch) = if let Some((epoch, inner)) = epoch_path(key) { (inner, Some(epoch)) } else { (key, None) };
        if slot_key.starts_with("slots/") && self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?.is_none() {
            let response = if self.machine_slot().as_deref() == Some(slot_key) {
                self.send(Method::GET, key, None, None, None)?
            } else {
                self.read_request(&format!("objects/{key}"), None)?
            };
            return match response.status().as_u16() {
                200 => Ok(Some(bytes(response, 16 << 10)?)),
                404 => Ok(None),
                status => bail!("gateway machine slot: HTTP {status}"),
            };
        }
        if slot_key.strip_prefix("slots/").is_some_and(|name| layr_store::person::slot_login(name).is_some()) {
            let response = self.send(Method::GET, key, None, None, None)?;
            return match response.status().as_u16() {
                200 => Ok(Some(bytes(response, 4096)?)),
                404 => Ok(None),
                status => bail!("gateway person slot: HTTP {status}"),
            };
        }
        if slot_key.starts_with("slots/") {
            #[derive(Deserialize)]
            struct Slot {
                name: String,
                data: String,
            }
            #[derive(Deserialize)]
            struct Slots {
                slots: Vec<Slot>,
            }
            let slots: Slots = json(self.send(Method::GET, "slots", None, slot_epoch.map(|e| ("epoch", e)), None)?, SLOTS_LIMIT)?;
            if slots.slots.len() > 1024 {
                bail!("too many gateway slots");
            }
            let name = slot_key.strip_prefix("slots/").unwrap().strip_suffix(".age").context("invalid slot name")?;
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
        let response = if key.starts_with("keys/")
            || epoch_path(key).is_some_and(|(_, k)| k == "store.json")
            || (overlay_keys_path(key) && !self.has_read_identity())
        {
            self.send(Method::GET, &format!("objects/{key}"), self.token.as_deref(), None, None)?
        } else if key == "store.json" {
            self.send(Method::GET, "store.json", None, None, None)?
        } else {
            self.read_request(&format!("objects/{key}"), None)?
        };
        if key == "store.json"
            && matches!(response.status().as_u16(), 401 | 403 | 404)
            && self.token.is_none()
            && self.public_get("public.json")?.is_some()
        {
            return self.public_get("store.json");
        }
        match response.status().as_u16() {
            404 if key == "store.json" && self.machine.is_some()
                && self.person.lock().map_err(|_| anyhow!("gateway session lock failed"))?.is_none() => bail!("gateway bootstrap: HTTP 404 (no such project); check the project name and a missing or wrong LAYR_GATEWAY_TOKEN_FILE grant"),
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
            #[serde(default)]
            continuation: Option<String>,
        }
        let has_identity = self.identity.lock().map_err(|_| anyhow!("gateway identity lock failed"))?.is_some();
        if self.is_public() {
            let logical = epoch_path(prefix).map(|(_, k)| k).unwrap_or(prefix);
            if !prefix.starts_with("keys/")
                && !logical.starts_with("records/")
                && !logical.starts_with("states/")
                && !logical.starts_with("files/")
            {
                return Ok(Vec::new());
            }
        }
        let mut found = std::collections::BTreeSet::new();
        let mut cursors = std::collections::BTreeSet::new();
        let mut continuation: Option<String> = None;
        let mut objects = Vec::new();
        let mut legacy = false;
        loop {
            let mut query = vec![("prefix", prefix)];
            if !legacy {
                query.push(("limit", "1000"));
            }
            if let Some(cursor) = &continuation {
                query.push(("continuation", cursor));
            }
            let response = if self.is_public() {
                self.public_request_query("objects", &query)?
            } else if !has_identity
                && (prefix.starts_with("records/")
                    || prefix.starts_with("states/")
                    || prefix == "keys/"
                    || overlay_keys_prefix(prefix))
            {
                self.send_query(Method::GET, "objects", self.token.as_deref(), &query, None)?
            } else {
                self.read_request_query("objects", &query)?
            };
            if response.status().as_u16() == 400 && continuation.is_none() && !legacy {
                // Older servers may reject the opt-in pagination query. Retry only the first
                // page with the legacy prefix query and the same authentication boundary.
                legacy = true;
                continue;
            }
            // Older gateways may omit the cursor and return their bounded complete listing.
            // New gateways return at most 1,000 objects per page, each at most 8 MiB.
            let listing: Objects = json(response, 8 << 20)?;
            if objects.len() + listing.objects.len() > 100_000 {
                bail!("too many gateway objects");
            }
            for entry in listing.objects {
                super::backend::check_key(&entry.key)?;
                if !entry.key.starts_with(prefix) || !found.insert(entry.key.clone()) {
                    bail!("invalid gateway object listing");
                }
                objects.push(Object { key: entry.key, modified: entry.modified / 1000 });
            }
            let Some(cursor) = listing.continuation else { break };
            if cursor.is_empty() || cursor.len() > 8192 || !cursors.insert(cursor.clone()) || cursors.len() > 1024 {
                bail!("invalid gateway listing continuation");
            }
            continuation = Some(cursor);
        }
        objects.sort_by(|a, b| a.key.cmp(&b.key));
        Ok(objects)
    }

    pub fn release_stream(&self, key: &str) -> Result<Option<Response>> {
        let logical = epoch_path(key).map(|(_, key)| key).unwrap_or(key);
        if !super::format::release_file(logical) {
            bail!("invalid release object");
        }
        let response =
            if self.is_public() { self.public_request(key, None)? } else { self.read_request(&format!("objects/{key}"), None)? };
        match response.status().as_u16() {
            200 => {
                if response.content_length().is_some_and(|n| n > super::format::RELEASE_OBJECT_LIMIT) {
                    bail!("release object is too large");
                }
                Ok(Some(response))
            }
            404 => Ok(None),
            status => bail!("gateway get: HTTP {status}"),
        }
    }

    pub fn put_file_new(&self, key: &str, file: std::fs::File) -> Result<bool> {
        let logical = epoch_path(key).map(|(_, key)| key).unwrap_or(key);
        if self.is_public() || !super::format::release_file(logical) {
            bail!("invalid release write");
        }
        let length = file.metadata()?.len();
        if length > super::format::RELEASE_OBJECT_LIMIT {
            bail!("release object is too large");
        }
        let token = self.token.as_deref().context("set LAYR_GATEWAY_TOKEN_FILE to a private write token file")?;
        let response = self
            .http
            .put(self.base.join(&format!("objects/{key}"))?)
            .bearer_auth(token)
            .timeout(Duration::from_secs(7200))
            .header("content-type", "application/octet-stream")
            .body(reqwest::blocking::Body::sized(file, length))
            .send()
            .map_err(reqwest::Error::without_url)?;
        match response.status().as_u16() {
            201 => Ok(true),
            412 => Ok(false),
            status => bail!("gateway put: HTTP {status}"),
        }
    }

    pub fn put_new(&self, key: &str, data: Vec<u8>) -> Result<bool> {
        if self.is_public() {
            bail!("public gateway access is read only");
        }
        if key == "store.json" {
            bail!("create the store with owner credentials before adding a gateway remote");
        }
        if data.len() > OBJECT_LIMIT {
            bail!("gateway object exceeds 32 MiB");
        }
        let logical = epoch_path(key).map(|(_, key)| key).unwrap_or(key);
        let person_request =
            logical.strip_prefix("requests/").and_then(|s| s.strip_suffix(".json")).is_some_and(layr_store::request::request_id);
        if person_request {
            let action = format!("objects/{key}");
            let response = self.send(Method::PUT, &action, Some(&self.session_token(false)?), None, Some(data.clone()))?;
            let response = if response.status().as_u16() == 401 {
                self.send(Method::PUT, &action, Some(&self.session_token(true)?), None, Some(data))?
            } else {
                response
            };
            return match response.status().as_u16() {
                201 => Ok(true),
                412 => Ok(false),
                status => bail!("gateway put: HTTP {status}"),
            };
        }
        // Pending epoch slots are staged before discovery exists, so they cannot
        // obtain a read proof yet. A project writer uses its grant for those PUTs.
        let token = if logical.starts_with("slots/") && (epoch_path(key).is_none() || self.token.is_none()) {
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
        if self.is_public() {
            bail!("public gateway access is read only");
        }
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
    fn listing_pages(pages: Vec<serde_json::Value>, public: bool) -> Result<Vec<Object>> {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let server = std::thread::spawn(move || {
            for (number, page) in pages.into_iter().enumerate() {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut request = Vec::new();
                let mut byte = [0];
                while !request.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    request.push(byte[0]);
                }
                let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                assert!(request.starts_with(if public { "get /p/project/objects?" } else { "get /v1/p/project/objects?" }));
                assert!(request.contains("limit=1000"));
                assert_eq!(request.contains("continuation="), number > 0);
                assert_eq!(request.contains("authorization: bearer fixture.write"), !public);
                let body = serde_json::to_vec(&page).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                let _ = stream.write_all(&body);
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        gateway.base = reqwest::Url::parse(&format!("http://{address}/v1/p/project/")).unwrap();
        gateway.token = Some("fixture.write".into());
        if public {
            gateway.use_public_access();
        }
        let result = gateway.list("states/");
        server.join().unwrap();
        result
    }

    #[test]
    fn listings_follow_pages_above_ten_thousand_and_preserve_access_modes() {
        for public in [false, true] {
            let pages = (0..11).map(|page| serde_json::json!({
                "objects": (page*1000..(page+1)*1000).map(|n| serde_json::json!({"key":format!("states/{n:05}/data"),"modified":1000})).collect::<Vec<_>>(),
                "continuation": if page < 10 { Some(format!("page{page}")) } else { None }
            })).collect();
            assert_eq!(listing_pages(pages, public).unwrap().len(), 11_000);
        }
    }

    #[test]
    fn old_gateways_can_reject_pagination_and_return_a_bounded_legacy_listing() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        for public in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                for first in [true, false] {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut request = Vec::new();
                    let mut byte = [0];
                    while !request.ends_with(b"\r\n\r\n") {
                        stream.read_exact(&mut byte).unwrap();
                        request.push(byte[0]);
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    assert!(request.starts_with(if public { "get /p/project/objects?" } else { "get /v1/p/project/objects?" }));
                    assert_eq!(request.contains("limit="), first);
                    assert_eq!(request.contains("authorization: bearer fixture.write"), !public);
                    let (status, body) =
                        if first { (400, "{}") } else { (200, "{\"objects\":[{\"key\":\"states/a\",\"modified\":1000}]}") };
                    write!(
                        stream,
                        "HTTP/1.1 {status} response\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                        body.len()
                    )
                    .unwrap();
                }
            });
            let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
            gateway.base = reqwest::Url::parse(&format!("http://{address}/v1/p/project/")).unwrap();
            gateway.token = Some("fixture.write".into());
            if public {
                gateway.use_public_access();
            }
            assert_eq!(gateway.list("states/").unwrap().len(), 1);
            server.join().unwrap();
        }
    }

    #[test]
    fn listings_reject_duplicates_cursor_loops_bad_prefixes_and_excess_totals() {
        let entry = |key: &str| serde_json::json!({"key":key,"modified":1000});
        for pages in [
            vec![
                serde_json::json!({"objects":[entry("states/a")],"continuation":"next"}),
                serde_json::json!({"objects":[entry("states/a")]}),
            ],
            vec![
                serde_json::json!({"objects":[entry("states/a")],"continuation":"next"}),
                serde_json::json!({"objects":[entry("states/b")],"continuation":"next"}),
            ],
            vec![serde_json::json!({"objects":[entry("records/a")]})],
        ] {
            assert!(listing_pages(pages, false).is_err());
        }
        let pages =
            vec![serde_json::json!({"objects":[],"continuation":"next"}), serde_json::json!({"objects":[entry("states/a")]})];
        assert_eq!(listing_pages(pages, true).unwrap().len(), 1);
        let oversized = (0..100_001).map(|n| entry(&format!("states/{n}"))).collect::<Vec<_>>();
        assert!(listing_pages(vec![serde_json::json!({"objects":oversized})], false).is_err());
    }

    #[test]
    fn token_expiry_tolerates_a_client_clock_behind_the_gateway() {
        let gateway_now = 1_792_951_000_000;
        let client_now = gateway_now - 30_000;
        for lifetime in [600_000, 43_200_000] {
            assert!(valid_token_expiry(gateway_now + lifetime, client_now, lifetime));
            assert!(valid_token_expiry(client_now + lifetime + 300_000, client_now, lifetime));
            assert!(!valid_token_expiry(client_now + lifetime + 300_001, client_now, lifetime));
            assert!(!valid_token_expiry(client_now, client_now, lifetime));
            assert!(!valid_token_expiry(client_now - 1, client_now, lifetime));
        }
    }

    #[test]
    fn missing_descriptor_keeps_optional_person_reads_and_hints_machine_grants() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        for mode in ["reader", "person", "machine", "grant"] {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let server = std::thread::spawn(move || {
                // Without a grant, automatic public discovery still probes public.json anonymously.
                for n in 0..if mode == "grant" { 1 } else { 2 } {
                    let (mut stream, _) = listener.accept().unwrap();
                    stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                    let mut request = Vec::new();
                    let mut part = [0; 2048];
                    while !request.windows(4).any(|p| p == b"\r\n\r\n") {
                        let n = stream.read(&mut part).unwrap();
                        assert!(n > 0);
                        request.extend_from_slice(&part[..n]);
                    }
                    let request = String::from_utf8(request).unwrap().to_ascii_lowercase();
                    if n == 0 {
                        assert!(request.starts_with("get /v1/p/project/store.json "));
                        assert_eq!(request.contains("authorization: bearer person.fixture"), mode == "person");
                        assert_eq!(request.contains("authorization: bearer grant.fixture"), mode == "grant");
                    } else {
                        assert!(request.starts_with("get /p/project/public.json "));
                        assert!(!request.contains("authorization:"));
                    }
                    write!(stream, "HTTP/1.1 404 Not Found\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{{}}").unwrap();
                }
            });
            let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
            gateway.base = reqwest::Url::parse(&format!("http://{address}/v1/p/project/")).unwrap();
            if mode != "reader" {
                gateway.machine = Some("test-machine".into());
            }
            if mode == "person" {
                *gateway.person.lock().unwrap() = Some(PersonSession {
                    login: "ann@example.com".into(),
                    key: ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
                    token: Some(SessionToken {
                        token: "person.fixture".into(),
                        expires: crate::ids::now_ms() + 60_000,
                        login: "ann@example.com".into(),
                    }),
                });
            }
            if mode == "grant" {
                gateway.token = Some("grant.fixture".into());
            }
            let result = gateway.get("store.json");
            if matches!(mode, "reader" | "person") {
                assert!(result.unwrap().is_none());
            } else {
                let error = result.unwrap_err().to_string();
                assert!(error.contains("404") && error.contains("missing or wrong"));
                assert!(!error.contains("grant.fixture"));
            }
            server.join().unwrap();
        }
    }

    #[test]
    fn epoch_slot_staging_uses_the_project_grant_before_a_new_proof_exists() {
        use std::io::{Read, Write};
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let epoch = "01a12290-0000-7000-8000-000000000008";
        let server = std::thread::spawn(move || {
            for (path, bearer) in [
                (format!("project/epochs/{epoch}/slots/machine-test.age"), "project.grant"),
                ("slots/machine-test.age".into(), "old.read.proof"),
                (format!("project/epochs/{epoch}/history/root.age"), "project.grant"),
            ] {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut bytes = Vec::new();
                let mut part = [0; 2048];
                while !bytes.windows(4).any(|p| p == b"\r\n\r\n") {
                    let count = stream.read(&mut part).unwrap();
                    assert!(count > 0);
                    bytes.extend_from_slice(&part[..count]);
                }
                let request = String::from_utf8(bytes).unwrap().to_ascii_lowercase();
                assert!(request.starts_with(&format!("put /v1/p/project/objects/{path} ")));
                assert!(request.contains(&format!("authorization: bearer {bearer}\r\n")));
                write!(stream, "HTTP/1.1 201 Created\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").unwrap();
            }
        });
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        gateway.base = reqwest::Url::parse(&format!("http://{address}/v1/p/project/")).unwrap();
        gateway.token = Some("project.grant".into());
        *gateway.read.lock().unwrap() = Some(("old.read.proof".into(), crate::ids::now_ms() + 60_000));
        for key in [
            format!("project/epochs/{epoch}/slots/machine-test.age"),
            "slots/machine-test.age".into(),
            format!("project/epochs/{epoch}/history/root.age"),
        ] {
            assert!(gateway.put_new(&key, b"sealed object".to_vec()).unwrap());
        }
        server.join().unwrap();
    }

    #[test]
    fn explicit_public_access_never_opens_person_keys() {
        let mut gateway = Gateway::open("layr+https://localhost/project").unwrap();
        let env = std::collections::HashMap::from([("LAYR_PERSON_KEY_FILE".into(), "/missing/person-key".into())]);
        gateway.set_person("ann@example.com", ed25519_dalek::SigningKey::from_bytes(&[7; 32])).unwrap();
        gateway.connect_public(&env).unwrap();
        assert!(gateway.person.lock().unwrap().is_none());
    }

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

fn epoch_path(key: &str) -> Option<(&str, &str)> {
    let (epoch, inner) = key.strip_prefix("project/epochs/")?.split_once('/')?;
    super::format::is_uuid(epoch).then_some((epoch, inner))
}
