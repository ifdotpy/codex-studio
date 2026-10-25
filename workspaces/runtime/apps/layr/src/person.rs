//! Person keys, Desk index MACs, and explicit invite acceptance.
use crate::store_remote::{backend::Backend, gateway::Gateway};
use anyhow::{anyhow, bail, Context, Result};
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use layr_store::{
    format::Descriptor,
    keys,
    person::{self, Index, Pin},
};
use reqwest::Method;
use serde_json::json;
use std::{
    collections::HashMap,
    fs,
    io::{Read, Write},
    os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt},
    path::{Path, PathBuf},
};

pub fn folder(env: &HashMap<String, String>, home: &Path) -> PathBuf {
    env.get("LAYR_PERSON_DIR").map(PathBuf::from).unwrap_or_else(|| home.join(".config/layr/person"))
}
fn read(path: &Path, limit: usize, uid: u32) -> Result<Vec<u8>> {
    let file = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.uid() != uid || meta.mode() & 0o777 != 0o600 {
        bail!("person files must belong to the caller and have mode 0600");
    }
    let mut data = Vec::new();
    file.take(limit as u64 + 1).read_to_end(&mut data)?;
    if data.len() > limit {
        bail!("person file is too large");
    }
    Ok(data)
}
pub fn create(path: &Path, data: &[u8]) -> Result<()> {
    let mut file = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
    file.write_all(data)?;
    file.sync_all()?;
    Ok(())
}
fn local_login(env: &HashMap<String, String>) -> Result<String> {
    let login = env.get("LAYR_PERSON_LOGIN").context("set LAYR_PERSON_LOGIN for a file:// or s3:// person grant or accept")?;
    if !person::login_name(login) {
        bail!("LAYR_PERSON_LOGIN must be a tailnet login such as ann@example.com");
    }
    Ok(login.clone())
}
pub fn site(site: &str, env: &HashMap<String, String>) -> Result<Gateway> {
    let mut url = reqwest::Url::parse(site).context("invalid gateway site URL")?;
    if url.scheme() != "https"
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        bail!("the site must be an HTTPS URL without credentials or query");
    }
    url.set_path("/person");
    let mut gateway = Gateway::open(&format!("layr+{}", url))?;
    gateway.connect_reader(env)?;
    Ok(gateway)
}
pub fn gateway_login(gateway: &Gateway) -> Result<String> {
    let record = gateway.person_view()?;
    let login = record["login"].as_str().context("the gateway cannot tell who you are")?;
    if !person::login_name(login) {
        bail!("the gateway gives no valid person login");
    }
    Ok(login.into())
}

#[derive(Clone)]
pub struct Access {
    pub login: String,
    pub identity: String,
    pub index: Index,
}
impl Access {
    pub fn check(&self, descriptor: &Descriptor) -> Result<()> {
        let pin = self.index.pin(&descriptor.project).ok_or_else(|| {
            anyhow!(
                "store project has no accepted person pin: pinned {:?}, store.json {}; use layr person accept",
                self.index.vaults.iter().map(|p| &p.project).collect::<Vec<_>>(),
                descriptor.project
            )
        })?;
        pin.check_store(descriptor)
    }
    pub fn open(&self, backend: &Backend, descriptor: &Descriptor) -> Result<String> {
        self.check(descriptor)?;
        let slot = backend
            .get(&format!("slots/{}", person::slot_name(&self.login, &keys::recipient_of(&self.identity)?)?))?
            .context("this login has no person slot in the store")?;
        let identity = keys::open_slot(&slot, &self.identity).context("the person key does not open this store")?;
        if keys::recipient_of(&identity)? != descriptor.recipient {
            bail!("person slot does not match store recipient");
        }
        Ok(identity)
    }
}
/// Decrypted keys stay in memory. A key file may hold the person identity or its paper opener.
pub fn load(env: &HashMap<String, String>, home: &Path, cwd: &Path, uid: u32, login: Option<&str>) -> Result<Option<Access>> {
    let Some(key_file) = env.get("LAYR_PERSON_KEY_FILE") else {
        return Ok(None);
    };
    let dir = folder(env, home);
    let recipient = String::from_utf8(read(&dir.join("recipient.txt"), 256, uid)?)?.trim().to_string();
    let text = String::from_utf8(read(&cwd.join(key_file), 4096, uid)?)?;
    let opener = keys::identity_from_paper(&text).map_err(|_| anyhow!("invalid person key file"))?;
    let identity = if keys::recipient_of(&opener)? == recipient {
        opener
    } else {
        keys::open_slot(&read(&dir.join("paper.age"), 4096, uid)?, &opener)
            .context("the paper key does not open your person key")?
    };
    if keys::recipient_of(&identity)? != recipient {
        bail!("person paper slot does not match person recipient");
    }
    let index = match read(&dir.join("index.age"), 49152, uid) {
        Ok(data) => person::open_index(&identity, &data)?,
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {
            Index::default()
        }
        Err(e) => return Err(e),
    };
    let login = match login {
        Some(login) => login.into(),
        None => local_login(env)?,
    };
    Ok(Some(Access { login, identity, index }))
}
/// Use the gateway's login and authenticated index for a gateway store.
pub fn gateway_access(gateway: &Gateway, access: &mut Access) -> Result<()> {
    let record = gateway.person_view()?;
    let login =
        record["login"].as_str().filter(|login| person::login_name(login)).context("the gateway cannot tell who you are")?;
    if record["recipient"].as_str() != Some(keys::recipient_of(&access.identity)?.as_str()) {
        bail!("gateway person recipient differs from your person key");
    }
    access.login = login.into();
    access.index = match record["index"].as_str() {
        Some(data) => person::open_index(&access.identity, &B64.decode(data)?)?,
        None => Index::default(),
    };
    Ok(())
}

fn with_index_lock<T>(dir: &Path, run: impl FnOnce() -> Result<T>) -> Result<T> {
    let file = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(dir.join("index.lock"))?;
    rustix::fs::flock(&file, rustix::fs::FlockOperation::LockExclusive)?;
    run()
}
fn save_index(dir: &Path, identity: &str, index: &Index) -> Result<()> {
    let data = person::seal_index(identity, index)?;
    if data.len() > 49152 {
        bail!("the person index exceeds 48 KiB");
    }
    crate::fsutil::atomic_write(&dir.join("index.age"), &data, 0o600)
}

/// Local addresses are hints. The accepted pin still authenticates each store descriptor.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
pub struct StoreAddress {
    pub pin: Pin,
    pub url: String,
    pub name: String,
}
pub fn addresses(dir: &Path, uid: u32) -> Result<std::collections::BTreeMap<String, StoreAddress>> {
    match read(&dir.join("addresses.json"), 49152, uid) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("invalid person store addresses")?),
        Err(e) if e.downcast_ref::<std::io::Error>().is_some_and(|e| e.kind() == std::io::ErrorKind::NotFound) => {
            Ok(Default::default())
        }
        Err(e) => Err(e),
    }
}
fn save_address(dir: &Path, uid: u32, pin: &Pin, url: &str, name: &str) -> Result<()> {
    let mut addresses = addresses(dir, uid)?;
    addresses.insert(pin.project.clone(), StoreAddress { pin: pin.clone(), url: url.into(), name: name.into() });
    let bytes = serde_json::to_vec(&addresses)?;
    if bytes.len() > 49152 {
        bail!("person store addresses exceed 48 KiB");
    }
    crate::fsutil::atomic_write(&dir.join("addresses.json"), &bytes, 0o600)
}
fn check_remote_invite(url: &str, env: &HashMap<String, String>, access: &Access, pin: &Pin) -> Result<String> {
    let backend = Backend::for_reader(url, env)?;
    let descriptor: Descriptor = serde_json::from_slice(&backend.get("store.json")?.context("store.json is missing")?)?;
    if descriptor.format != 2 {
        bail!("unsupported store format");
    }
    pin.check_store(&descriptor)?;
    let mut accepted = access.clone();
    accepted.index.set(pin.clone());
    accepted.open(&backend, &descriptor)?;
    let parsed = reqwest::Url::parse(url)?;
    Ok(parsed.path().trim_end_matches('/').rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("project").to_owned())
}

pub fn run(args: &[String]) -> Result<i32> {
    use crate::args::{parse, Spec};
    use std::io::IsTerminal;
    let a = parse(
        &Spec::new()
            .value("--paper-key-out")
            .value("--site")
            .value("--invite-file")
            .value("--invite")
            .value("--remote")
            .flag("--json"),
        args,
    )?;
    let env: HashMap<String, String> = std::env::vars().collect();
    let uid = rustix::process::getuid().as_raw();
    let home = crate::ctx::Caller::from_uid(uid).home;
    let dir = folder(&env, &home);
    let cwd = std::env::current_dir()?;
    if a.pos.len() != 1 {
        bail!("use layr person keys, show, or accept");
    }
    match a.pos[0].as_str() {
        "keys" => {
            let gateway = a.get("--site").map(|s| site(s, &env)).transpose()?;
            let login = match &gateway {
                Some(gateway) => Some(gateway_login(gateway)?),
                None => env.get("LAYR_PERSON_LOGIN").cloned(),
            };
            if login.as_ref().is_some_and(|login| !person::login_name(login)) {
                bail!("invalid person login");
            }
            if dir.join("recipient.txt").exists() {
                if gateway.is_none() {
                    bail!("this person key already exists; use layr person show");
                }
                if a.has("--paper-key-out") {
                    bail!("the existing paper key cannot be printed again");
                }
            } else {
                if !a.has("--paper-key-out") && !std::io::stdout().is_terminal() {
                    bail!("use --paper-key-out <path> outside a private terminal");
                }
                let identity = keys::generate_identity();
                let paper = keys::generate_identity();
                let words = keys::words_of_identity(&paper)?;
                fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
                let _lock = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(dir.join("creating"))?;
                let result = (|| -> Result<()> {
                    if let Some(path) = a.get("--paper-key-out") {
                        let path = cwd.join(path);
                        let parent = path.parent().context("paper key file has no parent")?.canonicalize()?;
                        let path = parent.join(path.file_name().context("paper key file has no name")?);
                        if let Ok(root) = crate::store::default_root().canonicalize() {
                            if path.starts_with(root) {
                                bail!("the paper key file must be outside the local store and line folders");
                            }
                        }
                        create(&path, format!("{words}\n").as_bytes())?;
                    } else {
                        println!("{words}");
                    }
                    crate::fsutil::atomic_write(
                        &dir.join("paper.age"),
                        &keys::seal_slot(&identity, &keys::recipient_of(&paper)?)?,
                        0o600,
                    )?;
                    save_index(&dir, &identity, &Index::default())?;
                    create(&dir.join("recipient.txt"), format!("{}\n", keys::recipient_of(&identity)?).as_bytes())?;
                    Ok(())
                })();
                fs::remove_file(dir.join("creating"))?;
                result?;
            }
            let recipient = String::from_utf8(read(&dir.join("recipient.txt"), 256, uid)?)?.trim().to_string();
            if let Some(gateway) = &gateway {
                gateway.person_call(
                    Method::PUT,
                    "/api/person",
                    None,
                    Some(&json!({"recipient":recipient,"paper":B64.encode(read(&dir.join("paper.age"),4096,uid)?)})),
                )?;
            }
            if let Some(login) = login {
                crate::fsutil::atomic_write(&dir.join("login.txt"), login.as_bytes(), 0o600)?;
            }
            println!("recipient: {recipient}");
        }
        "show" => {
            let gateway = a.get("--site").map(|s| site(s, &env)).transpose()?;
            let login = match &gateway {
                Some(gateway) => Some(gateway_login(gateway)?),
                None => env.get("LAYR_PERSON_LOGIN").cloned().or_else(|| fs::read_to_string(dir.join("login.txt")).ok()),
            };
            let recipient =
                String::from_utf8(read(&dir.join("recipient.txt"), 256, uid).context("no person key; use layr person keys")?)?
                    .trim()
                    .to_string();
            let mut access = load(&env, &home, &cwd, uid, login.as_deref())?;
            if let (Some(gateway), Some(access)) = (&gateway, &mut access) {
                gateway_access(gateway, access)?;
            }

            let stores = access.as_ref().map(|access| &access.index.vaults);
            if a.has("--json") {
                println!("{}", json!({"login":login,"recipient":recipient,"stores":stores}));
            } else {
                println!("login: {}\nrecipient: {recipient}", login.as_deref().unwrap_or("unknown"));
                if let Some(stores) = stores {
                    for pin in stores {
                        println!("store: {} {}", pin.project, pin.recipient);
                    }
                } else {
                    println!("stores: locked; set LAYR_PERSON_KEY_FILE");
                }
            }
        }
        "accept" => {
            if a.has("--invite-file") {
                if a.has("--site") || a.has("--invite") {
                    bail!("choose an invite file or a gateway invite");
                }
                local_login(&env)?;
                with_index_lock(&dir, || {
                    let mut access =
                        load(&env, &home, &cwd, uid, None)?.context("set LAYR_PERSON_KEY_FILE to open your person key")?;
                    let sealed = read(&cwd.join(a.get("--invite-file").unwrap()), 4096, uid)?;
                    let pin = person::open_invite(&access.identity, &sealed)?;
                    let address = a
                        .get("--remote")
                        .map(|url| check_remote_invite(url, &env, &access, &pin).map(|name| (url, name)))
                        .transpose()?;
                    access.index.set(pin.clone());
                    save_index(&dir, &access.identity, &access.index)?;
                    if let Some((url, name)) = address {
                        save_address(&dir, uid, &pin, url, &name)?;
                    }
                    println!("accepted store {} recipient {}", pin.project, pin.recipient);
                    Ok(())
                })?;
            } else {
                if a.has("--remote") {
                    bail!("--remote requires --invite-file");
                }
                let gateway =
                    site(a.get("--site").context("accept needs --invite-file <path> or --site <url> --invite <id>")?, &env)?;
                let id = a.get("--invite").context("name an invite with --invite <id>")?;
                with_index_lock(&dir, || {
                    let record = gateway.person_view()?;
                    let login = record["login"].as_str().context("the gateway cannot tell who you are")?;
                    let access =
                        load(&env, &home, &cwd, uid, Some(login))?.context("set LAYR_PERSON_KEY_FILE to open your person key")?;
                    if record["recipient"].as_str() != Some(keys::recipient_of(&access.identity)?.as_str()) {
                        bail!("gateway person recipient differs from your person key");
                    }
                    let invite = record["inbox"]
                        .as_array()
                        .and_then(|items| items.iter().find(|item| item["id"] == id))
                        .context("no such invite")?;
                    let pin = person::open_invite(
                        &access.identity,
                        &B64.decode(invite["data"].as_str().context("invite has no data")?)?,
                    )?;
                    let name = check_gateway_invite(&gateway, login, &access.identity, &pin)?;
                    let mut url = reqwest::Url::parse(a.get("--site").unwrap())?;
                    url.set_path(&format!("/{name}"));
                    let store_url = format!("layr+{url}");
                    let mut index = match record["index"].as_str() {
                        Some(data) => person::open_index(&access.identity, &B64.decode(data)?)?,
                        None => Index::default(),
                    };
                    index.set(pin.clone());
                    let challenge = gateway.person_call(Method::GET, "/api/person/challenge", None, None)?;
                    let token = String::from_utf8(keys::decrypt(
                        &access.identity,
                        &B64.decode(challenge["challenge"].as_str().context("missing person challenge")?)?,
                    )?)?;
                    let previous = record["index"].as_str().map(|data| B64.decode(data)).transpose()?.map(|data| {
                        use sha2::Digest;
                        hex::encode(sha2::Sha256::digest(data))
                    });
                    gateway.person_call(
                        Method::PUT,
                        "/api/person/index",
                        Some(&token),
                        Some(&json!({"data":B64.encode(person::seal_index(&access.identity,&index)?),"previous":previous})),
                    )?;
                    save_index(&dir, &access.identity, &index)?;
                    save_address(&dir, uid, &pin, &store_url, &name)?;
                    gateway.person_call(Method::DELETE, &format!("/api/person/inbox/{id}"), Some(&token), None)?;
                    println!("accepted store {} recipient {}", pin.project, pin.recipient);
                    Ok(())
                })?;
            }
        }
        _ => bail!("use layr person keys, show, or accept"),
    }
    Ok(0)
}
fn check_gateway_invite(gateway: &Gateway, login: &str, identity: &str, pin: &Pin) -> Result<String> {
    let projects = gateway.person_call(Method::GET, "/api/projects", None, None)?;
    for name in projects["projects"].as_array().context("gateway projects have no list")? {
        let name = name.as_str().filter(|name| person::project_name(name)).context("invalid gateway project name")?;
        let descriptor: Descriptor =
            serde_json::from_value(gateway.person_call(Method::GET, &format!("/v1/p/{name}/store.json"), None, None)?)?;
        if descriptor.project != pin.project {
            continue;
        }
        pin.check_store(&descriptor)?;
        let slot = gateway.person_slot_at(name, login, &keys::recipient_of(identity)?)?;
        let opened = keys::open_slot(&slot, identity)?;
        if keys::recipient_of(&opened)? != pin.recipient {
            bail!("person slot does not match store recipient");
        }
        return Ok(name.into());
    }
    bail!("the gateway has no store for this invite")
}
