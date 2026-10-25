//! The credentials of a named AWS profile on this Mac, read as the AWS CLI keeps them: keys in
//! ~/.aws/credentials or ~/.aws/config, or an IAM Identity Center (SSO) login whose token is in
//! ~/.aws/sso/cache. A token near its end is renewed with its refresh token. `login` signs in
//! with the device flow and writes the token where the AWS CLI reads it too. As for the AWS CLI,
//! AWS_CONFIG_FILE and AWS_SHARED_CREDENTIALS_FILE replace the files, and AWS_ENDPOINT_URL_SSO
//! and AWS_ENDPOINT_URL_SSO_OIDC the endpoints (for a test).

use super::Credentials;
use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use sha1::{Digest, Sha1};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// Renew a token when less than this is left.
const MARGIN: Duration = Duration::from_secs(5 * 60);
/// The scope that gives a refresh token.
const SCOPE: &str = "sso:account:access";
const LIMIT: Duration = Duration::from_secs(30);

/// A profile needs a new sign-in through `layr remote login <name>`.
#[derive(Debug)]
pub struct SignInNeeded(pub String);

impl std::fmt::Display for SignInNeeded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the AWS profile {0} needs a sign-in; run layr remote login <name>", self.0)
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

fn home(env: &HashMap<String, String>) -> Result<PathBuf> {
    Ok(PathBuf::from(env.get("HOME").context("HOME")?))
}

fn read_sections(env: &HashMap<String, String>, variable: &str, default: &str) -> Result<BTreeMap<String, Section>> {
    let path = match env.get(variable) {
        Some(path) if !path.is_empty() => PathBuf::from(path),
        _ => home(env)?.join(default),
    };
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(sections(&text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e).with_context(|| path.display().to_string()),
    }
}

fn files(env: &HashMap<String, String>) -> Result<(BTreeMap<String, Section>, BTreeMap<String, Section>)> {
    Ok((
        read_sections(env, "AWS_CONFIG_FILE", ".aws/config")?,
        read_sections(env, "AWS_SHARED_CREDENTIALS_FILE", ".aws/credentials")?,
    ))
}

/// The names of the profiles in the AWS files, sorted.
pub fn names(env: &HashMap<String, String>) -> Result<Vec<String>> {
    let (config, credentials) = files(env)?;
    let mut names: Vec<String> = config
        .keys()
        .filter_map(|n| (n == "default").then(|| n.clone()).or_else(|| n.strip_prefix("profile ").map(str::to_string)))
        .chain(credentials.keys().cloned())
        .collect();
    names.sort();
    names.dedup();
    Ok(names)
}

/// One profile: its region and how it gets credentials.
#[derive(Clone)]
pub struct Profile {
    pub name: String,
    pub region: Option<String>,
    kind: Kind,
}

#[derive(Clone)]
enum Kind {
    Keys(Credentials),
    Sso(Sso),
}

/// An IAM Identity Center login: the role of one account, and the token cache of the login.
#[derive(Clone)]
struct Sso {
    env: HashMap<String, String>,
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
        Self::load_with_env(name, &std::env::vars().collect())
    }

    pub fn load_with_env(name: &str, env: &HashMap<String, String>) -> Result<Self> {
        let (config, credentials) = files(env)?;
        let empty = Section::new();
        let section = config
            .get(&if name == "default" { "default".to_string() } else { format!("profile {name}") })
            .or_else(|| credentials.get(name).map(|_| &empty))
            .ok_or_else(|| anyhow!("no AWS profile {name} in the AWS config"))?;
        let region = section.get("region").cloned();
        let get = |section: &Section, key: &str, what: &str| {
            section.get(key).cloned().ok_or_else(|| anyhow!("the AWS {what} has no {key}"))
        };
        if section.contains_key("credential_process")
            || credentials.get(name).is_some_and(|s| s.contains_key("credential_process"))
        {
            bail!("AWS credential_process is not supported; use static keys or IAM Identity Center (SSO)");
        }
        // A profile with a role means "assume this role". Using its keys directly would act as
        // another identity without saying so, and Layr does not assume roles.
        const ROLE_KEYS: [&str; 4] = ["role_arn", "source_profile", "credential_source", "web_identity_token_file"];
        if ROLE_KEYS.iter().any(|k| section.contains_key(*k) || credentials.get(name).is_some_and(|s| s.contains_key(*k))) {
            bail!("AWS profile {name} assumes a role, which Layr does not support; use static keys or IAM Identity Center (SSO)");
        }
        let kind = if let Some(found) = credentials.get(name).and_then(keys).or_else(|| keys(section)) {
            Kind::Keys(found)
        } else if let Some(session) = section.get("sso_session") {
            let what = format!("sso-session {session}");
            let login = config.get(&what).ok_or_else(|| anyhow!("the AWS config has no [{what}]"))?;
            Kind::Sso(Sso {
                env: env.clone(),
                cache_key: session.clone(),
                start_url: get(login, "sso_start_url", &what)?,
                region: get(login, "sso_region", &what)?,
                account: get(section, "sso_account_id", &format!("profile {name}"))?,
                role: get(section, "sso_role_name", &format!("profile {name}"))?,
                scopes: login
                    .get("sso_registration_scopes")
                    .map(|s| s.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect())
                    .unwrap_or_else(|| vec![SCOPE.to_string()]),
            })
        } else if let Some(start_url) = section.get("sso_start_url") {
            let what = format!("profile {name}");
            Kind::Sso(Sso {
                env: env.clone(),
                cache_key: start_url.clone(),
                start_url: start_url.clone(),
                region: get(section, "sso_region", &what)?,
                account: get(section, "sso_account_id", &what)?,
                role: get(section, "sso_role_name", &what)?,
                scopes: Vec::new(),
            })
        } else {
            bail!("the AWS profile {name} has neither keys nor an SSO login, the two kinds that layr reads")
        };
        Ok(Profile { name: name.to_string(), region, kind })
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
                    || (cached["refreshToken"].is_string() && time("registrationExpiresAt").is_some_and(|t| t > now()))
            }),
        }
    }

    /// Credentials for AWS calls; SignInNeeded when the SSO login has ended.
    pub fn credentials(&self, http: &reqwest::blocking::Client) -> Result<Credentials> {
        match &self.kind {
            Kind::Keys(keys) => Ok(keys.clone()),
            Kind::Sso(sso) => {
                let token = sso.token(http, &self.name)?;
                sso.role_credentials(http, &token, &self.name)
            }
        }
    }

    /// Sign in with the device flow: `show` gets the page to open and the code it shows; the
    /// token then goes where the AWS CLI reads it.
    pub fn login(&self, http: &reqwest::blocking::Client, show: impl FnOnce(&str, &str)) -> Result<()> {
        match &self.kind {
            Kind::Keys(_) => bail!("the AWS profile {} uses keys; it needs no sign-in", self.name),
            Kind::Sso(sso) => sso.login(http, show),
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
    chrono::DateTime::parse_from_rfc3339(&text).ok().map(SystemTime::from)
}

fn format_time(time: SystemTime) -> String {
    chrono::DateTime::<chrono::Utc>::from(time).to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
}

fn endpoint(env: &HashMap<String, String>, variable: &str, default: String) -> String {
    env.get(variable).cloned().filter(|v| !v.is_empty()).unwrap_or(default).trim_end_matches('/').to_string()
}

/// POST a JSON body to the OIDC service: the status and the JSON reply.
fn post(http: &reqwest::blocking::Client, url: &str, body: &Value) -> Result<(u16, Value)> {
    let response = http.post(url).json(body).timeout(LIMIT).send()?;
    let status = response.status().as_u16();
    let text = response.text()?;
    Ok((status, serde_json::from_str(&text).unwrap_or(Value::Null)))
}

fn text(value: &Value, name: &str) -> Result<String> {
    value[name].as_str().map(str::to_string).ok_or_else(|| anyhow!("the AWS reply has no {name}"))
}

/// Keep untrusted service response bodies out of errors.
fn oidc_error(what: &str, status: u16, reply: &Value) -> anyhow::Error {
    let _ = reply;
    anyhow!("{what}: HTTP {status}")
}

impl Sso {
    fn cache_path(&self) -> Result<PathBuf> {
        Ok(home(&self.env)?.join(".aws/sso/cache").join(format!("{}.json", hex::encode(Sha1::digest(&self.cache_key)))))
    }

    fn cached(&self) -> Option<Value> {
        serde_json::from_str(&std::fs::read_to_string(self.cache_path().ok()?).ok()?).ok()
    }

    fn oidc(&self) -> String {
        endpoint(&self.env, "AWS_ENDPOINT_URL_SSO_OIDC", format!("https://oidc.{}.amazonaws.com", self.region))
    }

    fn write_cache(&self, cached: &Value) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        let path = self.cache_path()?;
        let folder = path.parent().expect("a cache folder");
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(folder)?;
        let temporary = folder.join(format!(".{}.layr", uuid::Uuid::now_v7()));
        let mut file = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
        file.write_all(serde_json::to_string(cached)?.as_bytes())?;
        file.sync_all()?;
        std::fs::rename(&temporary, &path)?;
        Ok(())
    }

    /// The access token of the login, renewed when it ends within MARGIN.
    fn token(&self, http: &reqwest::blocking::Client, profile: &str) -> Result<String> {
        let path = self.cache_path()?;
        let mut cached: Value = match std::fs::read_to_string(&path) {
            Ok(text) => serde_json::from_str(&text).with_context(|| path.display().to_string())?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Err(SignInNeeded(profile.to_string()).into()),
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
        let registered = field(&cached, "registrationExpiresAt").as_deref().and_then(parse_time).is_some_and(|t| t > now());
        if let (true, Some(id), Some(secret), Some(refresh)) =
            (registered, field(&cached, "clientId"), field(&cached, "clientSecret"), field(&cached, "refreshToken"))
        {
            let (status, reply) = post(
                http,
                &format!("{}/token", self.oidc()),
                &json!({"clientId": id, "clientSecret": secret, "grantType": "refresh_token", "refreshToken": refresh}),
            )
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

    fn role_credentials(&self, http: &reqwest::blocking::Client, token: &str, profile: &str) -> Result<Credentials> {
        let portal = endpoint(&self.env, "AWS_ENDPOINT_URL_SSO", format!("https://portal.sso.{}.amazonaws.com", self.region));
        let response = http
            .get(format!("{portal}/federation/credentials"))
            .query(&[("account_id", &self.account), ("role_name", &self.role)])
            .header("x-amz-sso_bearer_token", token)
            .timeout(LIMIT)
            .send()
            .context("AWS SSO role credentials")?;
        let status = response.status().as_u16();
        let reply: Value = serde_json::from_str(&response.text()?).unwrap_or(Value::Null);
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
                bail!("AWS SSO role {} of {}: HTTP {status}", self.role, self.account)
            }
        }
    }

    fn login(&self, http: &reqwest::blocking::Client, show: impl FnOnce(&str, &str)) -> Result<()> {
        let oidc = self.oidc();
        let mut register = json!({"clientName": "layr", "clientType": "public"});
        if !self.scopes.is_empty() {
            register["scopes"] = json!(self.scopes);
        }
        let (status, client) = post(http, &format!("{oidc}/client/register"), &register)?;
        if status != 200 {
            return Err(oidc_error("AWS sign-in: register", status, &client));
        }
        let (id, secret) = (text(&client, "clientId")?, text(&client, "clientSecret")?);
        let (status, start) = post(
            http,
            &format!("{oidc}/device_authorization"),
            &json!({"clientId": id, "clientSecret": secret, "startUrl": self.start_url}),
        )?;
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
            std::thread::sleep(every);
            let (status, reply) = post(
                http,
                &format!("{oidc}/token"),
                &json!({"clientId": id, "clientSecret": secret, "deviceCode": device,
                        "grantType": "urn:ietf:params:oauth:grant-type:device_code"}),
            )?;
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
            cached["registrationExpiresAt"] = json!(format_time(SystemTime::UNIX_EPOCH + Duration::from_secs(at)));
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
            env: std::env::vars().collect(),
            cache_key: "ifdotpy".into(),
            start_url: String::new(),
            region: String::new(),
            account: String::new(),
            role: String::new(),
            scopes: Vec::new(),
        };
        // The AWS CLI's name for the cache of the session "ifdotpy".
        assert!(sso.cache_path().unwrap().ends_with(".aws/sso/cache/94ccba1da4f2b47dedb27f8ed5de98c4139fd980.json"));
    }
    fn fixture() -> (PathBuf, HashMap<String, String>) {
        let root = std::env::temp_dir().join(format!("layr-profile-{}", uuid::Uuid::now_v7()));
        std::fs::create_dir_all(&root).unwrap();
        let env = HashMap::from([
            ("HOME".into(), root.to_string_lossy().into_owned()),
            ("AWS_CONFIG_FILE".into(), root.join("config").to_string_lossy().into_owned()),
            ("AWS_SHARED_CREDENTIALS_FILE".into(), root.join("credentials").to_string_lossy().into_owned()),
        ]);
        (root, env)
    }

    #[test]
    fn profiles_read_custom_files_and_refuse_processes() {
        let (root, env) = fixture();
        std::fs::write(root.join("config"),"[profile work]\nregion=eu-central-1\n[profile refuse]\ncredential_process=echo never\naws_access_key_id=ignored\naws_secret_access_key=ignored\n").unwrap();
        std::fs::write(
            root.join("credentials"),
            "[work]\naws_access_key_id=test-key\naws_secret_access_key=test-secret\naws_session_token=test-token\n",
        )
        .unwrap();
        let profile = Profile::load_with_env("work", &env).unwrap();
        assert_eq!(profile.region.as_deref(), Some("eu-central-1"));
        let credentials = profile.credentials(&reqwest::blocking::Client::new()).unwrap();
        assert!(
            credentials.key_id == "test-key"
                && credentials.secret == "test-secret"
                && credentials.token.as_deref() == Some("test-token")
        );
        assert!(Profile::load_with_env("refuse", &env)
            .err()
            .unwrap()
            .to_string()
            .contains("credential_process is not supported"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn role_profiles_are_refused() {
        let root = std::env::temp_dir().join(format!("layr-profile-role-{}", std::process::id()));
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(
            root.join("config"),
            "[profile admin]\nrole_arn=arn:aws:iam::1:role/x\nsource_profile=base\n[profile base]\nregion=eu-central-1\n",
        )
        .unwrap();
        std::fs::write(root.join("credentials"), "[admin]\naws_access_key_id=AKIA0000\naws_secret_access_key=secret\n").unwrap();
        let env: HashMap<String, String> = [
            ("AWS_CONFIG_FILE".to_string(), root.join("config").display().to_string()),
            ("AWS_SHARED_CREDENTIALS_FILE".to_string(), root.join("credentials").display().to_string()),
        ]
        .into();
        let error = Profile::load_with_env("admin", &env).err().expect("a role profile is refused").to_string();
        assert!(error.contains("assumes a role"), "{error}");
        std::fs::remove_dir_all(&root).ok();
    }

    fn fake(replies: Vec<(&'static str, Value)>) -> (String, std::thread::JoinHandle<()>) {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            for (path, reply) in replies {
                let (mut stream, _) = listener.accept().unwrap();
                stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
                let mut bytes = Vec::new();
                let mut byte = [0];
                while !bytes.ends_with(b"\r\n\r\n") {
                    stream.read_exact(&mut byte).unwrap();
                    bytes.push(byte[0]);
                }
                let header = String::from_utf8(bytes).unwrap();
                assert!(header.lines().next().unwrap().split_whitespace().nth(1).unwrap().starts_with(path));
                let len = header
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase().strip_prefix("content-length:").map(|n| n.trim().parse::<usize>().unwrap())
                    })
                    .unwrap_or(0);
                let mut body = vec![0; len];
                stream.read_exact(&mut body).unwrap();
                if path == "/token" {
                    let request = serde_json::from_slice::<Value>(&body).unwrap();
                    assert_eq!(
                        request["grantType"],
                        if request["deviceCode"].is_string() {
                            "urn:ietf:params:oauth:grant-type:device_code"
                        } else {
                            "refresh_token"
                        }
                    );
                }
                if path == "/federation/credentials" {
                    assert!(header.to_ascii_lowercase().contains("x-amz-sso_bearer_token: fresh-token"));
                }
                let body = serde_json::to_vec(&reply).unwrap();
                write!(stream, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n", body.len()).unwrap();
                stream.write_all(&body).unwrap();
            }
        });
        (endpoint, worker)
    }

    #[test]
    fn sso_refreshes_the_standard_cache_and_gets_role_keys_from_fake_endpoints() {
        use std::os::unix::fs::PermissionsExt;
        let (root, mut env) = fixture();
        std::fs::write(root.join("config"),"[profile work]\nsso_session=team\nsso_account_id=123456789012\nsso_role_name=Reader\n[sso-session team]\nsso_start_url=https://example.com/start\nsso_region=eu-west-1\n").unwrap();
        let (oidc, refresh) =
            fake(vec![("/token", json!({"accessToken":"fresh-token","expiresIn":3600,"refreshToken":"next-refresh"}))]);
        let (portal, role) = fake(vec![(
            "/federation/credentials",
            json!({"roleCredentials":{"accessKeyId":"role-key","secretAccessKey":"role-secret","sessionToken":"role-token"}}),
        )]);
        env.insert("AWS_ENDPOINT_URL_SSO_OIDC".into(), oidc);
        env.insert("AWS_ENDPOINT_URL_SSO".into(), portal);
        let profile = Profile::load_with_env("work", &env).unwrap();
        let Kind::Sso(sso) = &profile.kind else { panic!("expected SSO") };
        sso.write_cache(&json!({"accessToken":"old-token","expiresAt":format_time(now()+Duration::from_secs(30)),"refreshToken":"old-refresh","clientId":"client","clientSecret":"secret","registrationExpiresAt":format_time(now()+Duration::from_secs(3600))})).unwrap();
        // A refresh replaces a public old cache with a private file.
        std::fs::set_permissions(sso.cache_path().unwrap(), std::fs::Permissions::from_mode(0o644)).unwrap();
        let keys = profile.credentials(&reqwest::blocking::Client::new()).unwrap();
        assert!(keys.key_id == "role-key" && keys.secret == "role-secret" && keys.token.as_deref() == Some("role-token"));
        refresh.join().unwrap();
        role.join().unwrap();
        let cached = sso.cached().unwrap();
        assert_eq!(cached["accessToken"], "fresh-token");
        assert_eq!(cached["refreshToken"], "next-refresh");
        assert_eq!(std::fs::metadata(sso.cache_path().unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_sso_and_missing_or_expired_tokens_require_login() {
        let (root, env) = fixture();
        std::fs::write(root.join("config"),"[default]\nsso_start_url=https://example.com/start\nsso_region=eu-west-1\nsso_account_id=123\nsso_role_name=Reader\n").unwrap();
        let profile = Profile::load_with_env("default", &env).unwrap();
        let Kind::Sso(sso) = &profile.kind else { panic!("expected SSO") };
        assert_eq!(sso.cache_key, "https://example.com/start");
        let http = reqwest::blocking::Client::new();
        assert!(profile.credentials(&http).unwrap_err().is::<SignInNeeded>());
        sso.write_cache(&json!({"accessToken":"expired","expiresAt":"2000-01-01T00:00:00Z"})).unwrap();
        assert!(profile.credentials(&http).unwrap_err().is::<SignInNeeded>());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn device_login_shows_the_url_and_code_and_saves_a_private_cache() {
        use std::os::unix::fs::PermissionsExt;
        let (root, mut env) = fixture();
        std::fs::write(root.join("config"),"[profile work]\nsso_session=team\nsso_account_id=123\nsso_role_name=Reader\n[sso-session team]\nsso_start_url=https://example.com/start\nsso_region=eu-west-1\n").unwrap();
        let (endpoint, worker) = fake(vec![
            (
                "/client/register",
                json!({"clientId":"client","clientSecret":"secret","clientSecretExpiresAt":now().duration_since(SystemTime::UNIX_EPOCH).unwrap().as_secs()+3600}),
            ),
            (
                "/device_authorization",
                json!({"verificationUriComplete":"https://example.com/device","userCode":"TEST-CODE","deviceCode":"device","interval":1,"expiresIn":30}),
            ),
            ("/token", json!({"accessToken":"fresh-token","refreshToken":"refresh","expiresIn":3600})),
        ]);
        env.insert("AWS_ENDPOINT_URL_SSO_OIDC".into(), endpoint);
        let profile = Profile::load_with_env("work", &env).unwrap();
        let mut shown = false;
        profile
            .login(&reqwest::blocking::Client::new(), |page, code| {
                assert_eq!(page, "https://example.com/device");
                assert_eq!(code, "TEST-CODE");
                shown = true;
            })
            .unwrap();
        assert!(shown);
        worker.join().unwrap();
        let Kind::Sso(sso) = &profile.kind else { panic!("expected SSO") };
        assert_eq!(sso.cached().unwrap()["accessToken"], "fresh-token");
        assert_eq!(std::fs::metadata(sso.cache_path().unwrap()).unwrap().permissions().mode() & 0o777, 0o600);
        std::fs::remove_dir_all(root).unwrap();
    }
}
