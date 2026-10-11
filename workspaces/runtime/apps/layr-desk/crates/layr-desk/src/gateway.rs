//! A client of the gateway API (docs/design.md, "Gateway"): the slots of a vault, and its state
//! objects with a token that only a holder of the vault identity can get.

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine;
use desk_core::{keys, Store};
use reqwest::StatusCode;
use serde::Deserialize;
use tokio::sync::Mutex;

#[derive(Clone)]
pub struct Site {
    http: reqwest::Client,
    base: reqwest::Url,
}

#[derive(Deserialize)]
struct Projects {
    projects: Vec<String>,
}

#[derive(Deserialize)]
struct Slots {
    slots: Vec<Slot>,
}

#[derive(Deserialize)]
pub struct Slot {
    pub name: String,
    data: String,
}

impl Slot {
    pub fn bytes(&self) -> Result<Vec<u8>> {
        Ok(B64.decode(&self.data)?)
    }
}

#[derive(Deserialize)]
struct Challenge {
    challenge: String,
}

#[derive(Deserialize)]
struct States {
    states: Vec<String>,
}

impl Site {
    pub fn new(site: &str) -> Result<Self> {
        let mut base = reqwest::Url::parse(site).context("the site is not a URL")?;
        base.set_path("/");
        let http = reqwest::Client::builder().user_agent("layr-desk").build()?;
        Ok(Site { http, base })
    }

    async fn get(&self, path: &str, token: Option<&str>) -> Result<reqwest::Response> {
        let url = self.base.join(path)?;
        let mut request = self.http.get(url.clone());
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.with_context(|| format!("GET {url}"))?;
        Ok(response)
    }

    async fn json<T: for<'a> Deserialize<'a>>(&self, path: &str) -> Result<T> {
        let response = self.get(path, None).await?;
        if !response.status().is_success() {
            bail!("GET {path}: HTTP {}", response.status());
        }
        Ok(response.json().await?)
    }

    /// The vaults that this gateway serves.
    pub async fn projects(&self) -> Result<Vec<String>> {
        Ok(self.json::<Projects>("api/projects").await?.projects)
    }

    pub fn vault(&self, project: &str) -> Vault {
        Vault {
            site: self.clone(),
            project: project.to_string(),
            token: Mutex::new(None),
            identity: None,
        }
    }
}

/// One vault of the gateway. It needs the vault identity for its state objects.
pub struct Vault {
    site: Site,
    project: String,
    token: Mutex<Option<String>>,
    identity: Option<String>,
}

impl Vault {
    fn path(&self, rest: &str) -> String {
        format!("api/p/{}/{rest}", self.project)
    }

    pub async fn slots(&self) -> Result<Vec<Slot>> {
        Ok(self.site.json::<Slots>(&self.path("slots")).await?.slots)
    }

    /// Open the paper slot with the paper key and keep the vault identity.
    pub async fn unlock_with_paper(&mut self, paper: &str) -> Result<String> {
        let opener = keys::identity_from_paper(paper)?;
        let slots = self.slots().await?;
        let slot = slots
            .iter()
            .find(|s| s.name == "paper")
            .ok_or_else(|| anyhow!("this vault has no paper slot"))?;
        let identity = keys::open_slot(&slot.bytes()?, &opener)
            .context("this paper key does not open the vault")?;
        self.identity = Some(identity.clone());
        Ok(identity)
    }

    /// This vault with a known vault identity (from the key store).
    pub fn unlocked(mut self, identity: &str) -> Self {
        self.identity = Some(identity.to_string());
        self
    }

    pub fn identity(&self) -> Result<&str> {
        self.identity
            .as_deref()
            .ok_or_else(|| anyhow!("the vault is locked"))
    }

    async fn token(&self, fresh: bool) -> Result<String> {
        let mut token = self.token.lock().await;
        if fresh || token.is_none() {
            let challenge: Challenge = self.site.json(&self.path("challenge")).await?;
            let plain = keys::decrypt(self.identity()?, &B64.decode(challenge.challenge)?)
                .context("the gateway challenge is not for this vault")?;
            *token = Some(String::from_utf8(plain)?);
        }
        Ok(token.clone().expect("set above"))
    }

    /// GET with the token; a 401 (tokens live 10 minutes) gets one fresh token and one retry.
    async fn authorized(&self, rest: &str) -> Result<reqwest::Response> {
        let path = self.path(rest);
        for fresh in [false, true] {
            let response = self
                .site
                .get(&path, Some(&self.token(fresh).await?))
                .await?;
            match response.status() {
                StatusCode::UNAUTHORIZED if !fresh => continue,
                s if s.is_success() => return Ok(response),
                s => bail!("GET {path}: HTTP {s}"),
            }
        }
        unreachable!()
    }

    /// The object names below `states/`.
    pub async fn states(&self) -> Result<Vec<String>> {
        Ok(self
            .authorized("states")
            .await?
            .json::<States>()
            .await?
            .states)
    }
}

impl Store for Vault {
    async fn get(&self, name: &str) -> Result<Vec<u8>> {
        Ok(self
            .authorized(&format!("states/{name}"))
            .await?
            .bytes()
            .await?
            .to_vec())
    }
}
