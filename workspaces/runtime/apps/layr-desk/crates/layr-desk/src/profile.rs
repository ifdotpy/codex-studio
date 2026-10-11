//! The credentials of a named AWS profile on this Mac, read as the AWS CLI keeps them: keys in
//! ~/.aws/credentials or ~/.aws/config, or an IAM Identity Center (SSO) login whose token is in
//! ~/.aws/sso/cache. A token near its end is renewed with its refresh token. `login` signs in
//! with the device flow and writes the token where the AWS CLI reads it too. As for the AWS CLI,
//! AWS_CONFIG_FILE and AWS_SHARED_CREDENTIALS_FILE replace the files, and AWS_ENDPOINT_URL_SSO
//! and AWS_ENDPOINT_URL_SSO_OIDC the endpoints (for a test).

use anyhow::{anyhow, bail, Context, Result};
use desk_core::aws::Credentials;
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Renew a token when less than this is left.
const MARGIN: Duration = Duration::from_secs(5 * 60);
/// The scope that gives a refresh token.
const SCOPE: &str = "sso:account:access";
const LIMIT: Duration = Duration::from_secs(30);

/// A profile needs a new sign-in (`layr-desk aws-login`); `main` exits with 3 for it.
#[derive(Debug)]
pub struct SignInNeeded(pub String);

impl std::fmt::Display for SignInNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "the AWS profile {0} needs a sign-in: layr-desk aws-login --profile {0}",
            self.0
        )
    }
}

impl std::error::Error for SignInNeeded {}

type Section = BTreeMap<String, String>;

/// The sections of an AWS ini file by name (`profile x`, `default`, `sso-session y`).
fn sections(text: &str) -> BTreeMap<String, Section> {
    let mut all: BTreeMap<String, Section> = BTreeMap::new();
    let mut current: Option<String> = None;
    for line in text.lines() {
        // The indented lines of a nested value (`s3 =`); no setting here uses one.
        if line.starts_with([' ', '\t']) {
            continue;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with(['#', ';']) {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            let name = name.split_whitespace().collect::<Vec<_>>().join(" ");
            all.entry(name.clone()).or_default();
            current = Some(name);
        } else if let (Some(name), Some((key, value))) = (&current, line.split_once('=')) {
            if let Some(section) = all.get_mut(name) {
                section.insert(key.trim().to_lowercase(), value.trim().to_string());
            }
        }
    }
    all
}

fn home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var("HOME").context("HOME")?))
}

fn read_sections(variable: &str, default: &str) -> Result<BTreeMap<String, Section>> {
    let path = match std::env::var_os(variable) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => home()?.join(default),
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(sections(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

fn files() -> Result<(BTreeMap<String, Section>, BTreeMap<String, Section>)> {
    Ok((
        read_sections("AWS_CONFIG_FILE", ".aws/config")?,
        read_sections("AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials")?,
    ))
}

/// The names of the profiles in the AWS files, sorted.
pub fn names() -> Result<Vec<String>> {
    let (config, credentials) = files()?;
    let mut names: Vec<String> = config
        .keys()
        .filter_map(|n| {
            (n == "default")
                .then(|| n.clone())
                .or_else(|| n.strip_prefix("profile ").map(str::to_string))
        })
        .chain(credentials.keys().cloned())
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// One profile: its region and how it gets credentials.
pub struct Profile {
    pub name: String,
    pub region: Option<String>,
    kind: Kind,
}

enum Kind {
    Keys(Credentials),
    Sso(Sso),
}

/// An IAM Identity Center login: the role of one account, and the token cache of the login.
struct Sso {
    /// The cache file is named by the SHA-1 of this: the session name, or the start URL of a
    /// profile without a session (the legacy form).
    cache_key: String,
    start_url: String,
    region: String,
    account: String,
    role: String,
    /// The registration scopes; none for a legacy profile, which gets no refresh token.
    scopes: Vec<String>,
}

fn keys(section: &Section) -> Option<Credentials> {
    Some(Credentials {
        key_id: section.get("aws_access_key_id")?.clone(),
        secret: section.get("aws_secret_access_key")?.clone(),
        token: section.get("aws_session_token").cloned(),
    })
}

impl Profile {
    pub fn load(name: &str) -> Result<Self> {
        let (config, credentials) = files()?;
        let empty = Section::new();
        let section = config
            .get(&if name == "default" {
                "default".to_string()
            } else {
                format!("profile {name}")
            })
            .or_else(|| credentials.get(name).map(|_| &empty))
            .ok_or_else(|| anyhow!("no AWS profile {name} in the AWS config"))?;
        let region = section.get("region").cloned();
        let get = |section: &Section, key: &str, what: &str| {
            section
                .get(key)
                .cloned()
                .ok_or_else(|| anyhow!("the AWS {what} has no {key}"))
        };
        let kind = if let Some(found) = credentials
            .get(name)
            .and_then(keys)
            .or_else(|| keys(section))
        {
            Kind::Keys(found)
        } else if let Some(session) = section.get("sso_session") {
            let what = format!("sso-session {session}");
            let login = config
                .get(&what)
                .ok_or_else(|| anyhow!("the AWS config has no [{what}]"))?;
            Kind::Sso(Sso {
                cache_key: session.clone(),
                start_url: get(login, "sso_start_url", &what)?,
                region: get(login, "sso_region", &what)?,
                account: get(section, "sso_account_id", &format!("profile {name}"))?,
                role: get(section, "sso_role_name", &format!("profile {name}"))?,
                scopes: login
                    .get("sso_registration_scopes")
                    .map(|s| {
                        s.split(',')
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_else(|| vec![SCOPE.to_string()]),
            })
        } else if let Some(start_url) = section.get("sso_start_url") {
            let what = format!("profile {name}");
            Kind::Sso(Sso {
                cache_key: start_url.clone(),
                start_url: start_url.clone(),
                region: get(section, "sso_region", &what)?,
                account: get(section, "sso_account_id", &what)?,
                role: get(section, "sso_role_name", &what)?,
                scopes: Vec::new(),
            })
        } else {
            bail!("the AWS profile {name} has neither keys nor an SSO login, the two kinds that layr-desk reads")
        };
        Ok(Profile {
            name: name.to_string(),
            region,
            kind,
        })
    }

    /// Whether the profile signs in through IAM Identity Center.
    pub fn sso(&self) -> bool {
        matches!(self.kind, Kind::Sso(_))
    }

    /// Whether the profile has keys, or an SSO token that is valid or can be renewed. It makes
    /// no AWS call, so the role may still refuse.
    pub fn signed_in(&self) -> bool {
        match &self.kind {
            Kind::Keys(_) => true,
            Kind::Sso(sso) => sso.cached().is_some_and(|cached| {
                let time = |name: &str| cached[name].as_str().and_then(parse_time);
                time("expiresAt").is_some_and(|t| t > now() + MARGIN)
                    || (cached["refreshToken"].is_string()
                        && time("registrationExpiresAt").is_some_and(|t| t > now()))
            }),
        }
    }

    /// Credentials for AWS calls; SignInNeeded when the SSO login has ended.
    pub async fn credentials(&self, http: &reqwest::Client) -> Result<Credentials> {
        match &self.kind {
            Kind::Keys(keys) => Ok(keys.clone()),
            Kind::Sso(sso) => {
                let token = sso.token(http, &self.name).await?;
                sso.role_credentials(http, &token, &self.name).await
            }
        }
    }

    /// Sign in with the device flow: `show` gets the page to open and the code it shows; the
    /// token then goes where the AWS CLI reads it.
    pub async fn login(&self, http: &reqwest::Client, show: impl FnOnce(&str, &str)) -> Result<()> {
        match &self.kind {
            Kind::Keys(_) => bail!(
                "the AWS profile {} uses keys; it needs no sign-in",
                self.name
            ),
            Kind::Sso(sso) => sso.login(http, show).await,
        }
    }
}

fn now() -> SystemTime {
    SystemTime::now()
}

/// A time of the token cache: RFC 3339 in UTC, as the AWS CLI and the SDKs write it.
fn parse_time(text: &str) -> Option<SystemTime> {
    let text = text.trim();
    let text = text
        .strip_suffix("+00:00")
        .or_else(|| text.strip_suffix("UTC"))
        .map(|t| format!("{t}Z"))
        .unwrap_or_else(|| text.to_string());
    humantime::parse_rfc3339_weak(text.trim_end_matches('Z')).ok()
}

fn format_time(time: SystemTime) -> String {
    humantime::format_rfc3339_seconds(time).to_string()
}

fn endpoint(variable: &str, default: String) -> String {
    std::env::var(variable)
        .ok()
        .filter(|v| !v.is_empty())
        .unwrap_or(default)
        .trim_end_matches('/')
        .to_string()
}

/// POST a JSON body to the OIDC service: the status and the JSON reply.
async fn post(http: &reqwest::Client, url: &str, body: &Value) -> Result<(u16, Value)> {
    let response = http.post(url).json(body).timeout(LIMIT).send().await?;
    let status = response.status().as_u16();
    let text = response.text().await?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

fn text(value: &Value, name: &str) -> Result<String> {
    value[name]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| anyhow!("the AWS reply has no {name}"))
}

/// What a failed OIDC call says: its error code and description, which hold no secret.
fn oidc_error(what: &str, status: u16, reply: &Value) -> anyhow::Error {
    let error = reply["error"].as_str().unwrap_or_default();
    let description = reply["error_description"].as_str().unwrap_or_default();
    anyhow!("{what}: HTTP {status} {error} {description}")
}

impl Sso {
    fn cache_path(&self) -> Result<PathBuf> {
        Ok(home()?.join(".aws/sso/cache").join(format!(
            "{}.json",
            hex::encode(Sha1::digest(&self.cache_key))
        )))
    }

    fn cached(&self) -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(self.cache_path().ok()?).ok()?).ok()
    }

    fn oidc(&self) -> String {
        endpoint(
            "AWS_ENDPOINT_URL_SSO_OIDC",
            format!("https://oidc.{}.amazonaws.com", self.region),
        )
    }

    fn write_cache(&self, cached: &Value) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let path = self.cache_path()?;
        let folder = path.parent().expect("a cache folder");
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(folder)?;
        let temporary = folder.join(format!(".{}.layr-desk", std::process::id()));
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&temporary)?;
        file.write_all(serde_json::to_string(cached)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        Ok(())
    }

    /// The access token of the login, renewed when it ends within MARGIN.
    async fn token(&self, http: &reqwest::Client, profile: &str) -> Result<String> {
        let path = self.cache_path()?;
        let mut cached: Value = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| path.display().to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Err(SignInNeeded(profile.to_string()).into())
            }
            Err(e) => return Err(e).with_context(|| path.display().to_string()),
        };
        let field = |cached: &Value, name: &str| cached[name].as_str().map(str::to_string);
        let expires = field(&cached, "expiresAt").as_deref().and_then(parse_time);
        let token = field(&cached, "accessToken");
        if let (Some(token), Some(expires)) = (&token, expires) {
            if expires > now() + MARGIN {
                return Ok(token.clone());
            }
        }
        let registered = field(&cached, "registrationExpiresAt")
            .as_deref()
            .and_then(parse_time)
            .is_some_and(|t| t > now());
        if let (true, Some(id), Some(secret), Some(refresh)) = (
            registered,
            field(&cached, "clientId"),
            field(&cached, "clientSecret"),
            field(&cached, "refreshToken"),
        ) {
            let (status, reply) = post(
                http,
                &format!("{}/token", self.oidc()),
                &json!({"clientId": id, "clientSecret": secret, "grantType": "refresh_token", "refreshToken": refresh}),
            )
            .await
            .context("renew the AWS sign-in")?;
            match status {
                200 => {
                    let token = text(&reply, "accessToken")?;
                    let seconds = reply["expiresIn"].as_u64().unwrap_or(3600);
                    cached["accessToken"] = json!(token);
                    cached["expiresAt"] = json!(format_time(now() + Duration::from_secs(seconds)));
                    if let Some(refresh) = reply["refreshToken"].as_str() {
                        cached["refreshToken"] = json!(refresh);
                    }
                    self.write_cache(&cached)?;
                    return Ok(token);
                }
                // The refresh token has ended or was revoked.
                400 | 401 => {}
                _ => return Err(oidc_error("renew the AWS sign-in", status, &reply)),
            }
        }
        match (token, expires) {
            (Some(token), Some(expires)) if expires > now() => Ok(token),
            _ => Err(SignInNeeded(profile.to_string()).into()),
        }
    }

    async fn role_credentials(
        &self,
        http: &reqwest::Client,
        token: &str,
        profile: &str,
    ) -> Result<Credentials> {
        let portal = endpoint(
            "AWS_ENDPOINT_URL_SSO",
            format!("https://portal.sso.{}.amazonaws.com", self.region),
        );
        let response = http
            .get(format!("{portal}/federation/credentials"))
            .query(&[("account_id", &self.account), ("role_name", &self.role)])
            .header("x-amz-sso_bearer_token", token)
            .timeout(LIMIT)
            .send()
            .await
            .context("AWS SSO role credentials")?;
        let status = response.status().as_u16();
        let reply: Value = serde_json::from_str(&response.text().await?).unwrap_or(Value::Null);
        match status {
            200 => {
                let role = &reply["roleCredentials"];
                Ok(Credentials {
                    key_id: text(role, "accessKeyId")?,
                    secret: text(role, "secretAccessKey")?,
                    token: Some(text(role, "sessionToken")?),
                })
            }
            401 => Err(SignInNeeded(profile.to_string()).into()),
            _ => {
                let message = reply["message"].as_str().unwrap_or_default();
                bail!(
                    "AWS SSO role {} of {}: HTTP {status} {message}",
                    self.role,
                    self.account
                )
            }
        }
    }

    async fn login(&self, http: &reqwest::Client, show: impl FnOnce(&str, &str)) -> Result<()> {
        let oidc = self.oidc();
        let mut register = json!({"clientName": "layr-desk", "clientType": "public"});
        if !self.scopes.is_empty() {
            register["scopes"] = json!(self.scopes);
        }
        let (status, client) = post(http, &format!("{oidc}/client/register"), &register).await?;
        if status != 200 {
            return Err(oidc_error("AWS sign-in: register", status, &client));
        }
        let (id, secret) = (text(&client, "clientId")?, text(&client, "clientSecret")?);
        let (status, start) = post(
            http,
            &format!("{oidc}/device_authorization"),
            &json!({"clientId": id, "clientSecret": secret, "startUrl": self.start_url}),
        )
        .await?;
        if status != 200 {
            return Err(oidc_error("AWS sign-in: start", status, &start));
        }
        let page = start["verificationUriComplete"]
            .as_str()
            .or_else(|| start["verificationUri"].as_str())
            .ok_or_else(|| anyhow!("the AWS reply has no verification page"))?;
        show(page, start["userCode"].as_str().unwrap_or_default());
        let device = text(&start, "deviceCode")?;
        let mut every = Duration::from_secs(start["interval"].as_u64().unwrap_or(5).max(1));
        let deadline = now() + Duration::from_secs(start["expiresIn"].as_u64().unwrap_or(600));
        let reply = loop {
            if now() > deadline {
                bail!("AWS sign-in: the code was not confirmed in time");
            }
            tokio::time::sleep(every).await;
            let (status, reply) = post(
                http,
                &format!("{oidc}/token"),
                &json!({"clientId": id, "clientSecret": secret, "deviceCode": device,
                        "grantType": "urn:ietf:params:oauth:grant-type:device_code"}),
            )
            .await?;
            match (status, reply["error"].as_str()) {
                (200, _) => break reply,
                (_, Some("authorization_pending")) => {}
                (_, Some("slow_down")) => every += Duration::from_secs(5),
                _ => return Err(oidc_error("AWS sign-in", status, &reply)),
            }
        };
        let mut cached = json!({
            "startUrl": self.start_url,
            "region": self.region,
            "accessToken": text(&reply, "accessToken")?,
            "expiresAt": format_time(now() + Duration::from_secs(reply["expiresIn"].as_u64().unwrap_or(3600))),
            "clientId": id,
            "clientSecret": secret,
        });
        if let Some(at) = client["clientSecretExpiresAt"].as_u64() {
            cached["registrationExpiresAt"] = json!(format_time(
                SystemTime::UNIX_EPOCH + Duration::from_secs(at)
            ));
        }
        if let Some(refresh) = reply["refreshToken"].as_str() {
            cached["refreshToken"] = json!(refresh);
        }
        self.write_cache(&cached)
    }
}

#[cfg(test)]
mod unit {
    use super::*;

    #[test]
    fn the_sections_skip_comments_and_nested_values() {
        let found = sections(
            "# note\n[profile a]\nregion = eu-central-1\ns3 =\n  addressing_style = path\n[sso-session  s]\nsso_start_url=https://x\n",
        );
        assert_eq!(found["profile a"]["region"], "eu-central-1");
        assert!(!found["profile a"].contains_key("addressing_style"));
        assert_eq!(found["sso-session s"]["sso_start_url"], "https://x");
    }

    #[test]
    fn cache_times_of_the_cli_and_the_sdks_parse() {
        let at = parse_time("2026-10-11T19:27:34Z").unwrap();
        assert_eq!(parse_time("2026-10-11T19:27:34.000Z"), Some(at));
        assert_eq!(parse_time("2026-10-11T19:27:34+00:00"), Some(at));
        assert_eq!(parse_time("2026-10-11T19:27:34UTC"), Some(at));
        assert_eq!(format_time(at), "2026-10-11T19:27:34Z");
    }

    #[test]
    fn the_cache_file_is_the_sha1_of_the_session_name() {
        let sso = Sso {
            cache_key: "ifdotpy".into(),
            start_url: String::new(),
            region: String::new(),
            account: String::new(),
            role: String::new(),
            scopes: Vec::new(),
        };
        // The AWS CLI's name for the cache of the session "ifdotpy".
        assert!(sso
            .cache_path()
            .unwrap()
            .ends_with(".aws/sso/cache/94ccba1da4f2b47dedb27f8ed5de98c4139fd980.json"));
    }
}
