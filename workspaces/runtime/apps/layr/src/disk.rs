//! Read-only store views and a bounded cache. Adapted from Layr Desk's disk module.

mod all;
mod catalog;
pub mod keystore;
mod launch;
mod webdav;

use crate::store_remote::browse::{Access, Browse, Entry};
use crate::store_remote::StorePin;
use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;

#[derive(Clone)]
pub enum Content {
    Stored { entry: Entry, reader: Arc<Mutex<Browse>> },
    Text(Arc<Vec<u8>>),
}

#[derive(Clone)]
pub enum Node {
    Folder { time: i64 },
    File { size: u64, etag: String, time: i64, content: Content },
}

struct Snapshot {
    nodes: BTreeMap<String, Node>,
    folders: BTreeMap<String, BTreeSet<String>>,
}

pub struct Disk {
    url: String,
    env: HashMap<String, String>,
    access: Access,
    cache: Cache,
    snapshot: RwLock<Arc<Snapshot>>,
    keychain: std::sync::atomic::AtomicBool,
}

pub fn line_folder(name: &str) -> String {
    name.replace('%', "%25").replace('/', "%2F")
}

fn text(bytes: Vec<u8>, time: i64) -> Node {
    Node::File {
        size: bytes.len() as u64,
        etag: hex::encode(Sha256::digest(&bytes)),
        time,
        content: Content::Text(Arc::new(bytes)),
    }
}

fn folders(nodes: &BTreeMap<String, Node>) -> BTreeMap<String, BTreeSet<String>> {
    let mut folders: BTreeMap<String, BTreeSet<String>> = BTreeMap::from([(String::new(), BTreeSet::new())]);
    for path in nodes.keys() {
        let parts: Vec<_> = path.split('/').collect();
        for i in 1..=parts.len() {
            let child = parts[..i].join("/");
            folders.entry(parts[..i - 1].join("/")).or_default().insert(child.clone());
            if i < parts.len() || matches!(nodes.get(path), Some(Node::Folder { .. })) {
                folders.entry(child).or_default();
            }
        }
    }
    folders
}

fn join(link: &str, target: &str) -> Option<String> {
    if target.starts_with('/') || target.contains('\0') {
        return None;
    }
    let mut parts: Vec<_> = link.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            part => parts.push(part),
        }
    }
    Some(parts.join("/"))
}

fn resolve(path: &str, entry: &Entry, files: &BTreeMap<String, Entry>, reader: &mut Browse) -> Result<Entry> {
    let mut at = (path.to_string(), entry.clone());
    for _ in 0..8 {
        if at.1 .0 != "120000" {
            return Ok(at.1);
        }
        let bytes = reader.read(&at.1)?;
        let Ok(target) = std::str::from_utf8(&bytes) else {
            return Ok(entry.clone());
        };
        let Some(target) = join(&at.0, target) else {
            return Ok(entry.clone());
        };
        let Some(next) = files.get(&target) else {
            return Ok(entry.clone());
        };
        at = (target, next.clone());
    }
    Ok(entry.clone())
}

impl Disk {
    fn load(url: &str, env: &HashMap<String, String>, access: &Access) -> Result<(Snapshot, String, StorePin)> {
        let span = crate::trace::span("disk store read");
        let mut reader = Browse::open(url, env, access)?;
        drop(span);
        let identity = reader.identity().to_owned();
        let pin = reader.pin.clone();
        let lines = std::mem::take(&mut reader.lines);
        let reader = Arc::new(Mutex::new(reader));
        let mut nodes = BTreeMap::new();
        nodes.insert(".layr".into(), Node::Folder { time: 0 });
        for line in lines {
            let name = line_folder(&line.name);
            let time = line.time / 1000;
            nodes.insert(name.clone(), Node::Folder { time });
            let info = serde_json::to_vec_pretty(&serde_json::json!({"state":line.head,"kind":line.kind,
                "machine":line.machine,"time":line.time}))?;
            nodes.insert(format!(".layr/{name}.json"), text(info, time));
            for (path, entry) in &line.files {
                let shown = resolve(path, entry, &line.files, &mut reader.lock().unwrap())?;
                nodes.insert(
                    format!("{name}/{path}"),
                    Node::File {
                        size: shown.1,
                        etag: shown.2.clone(),
                        time,
                        content: Content::Stored { entry: shown, reader: reader.clone() },
                    },
                );
            }
        }
        Ok((Snapshot { folders: folders(&nodes), nodes }, identity, pin))
    }

    pub fn open(
        url: String,
        env: HashMap<String, String>,
        mut access: Access,
        cache: PathBuf,
        limit: u64,
    ) -> Result<(Arc<Self>, String)> {
        let (snapshot, identity, pin) = Self::load(&url, &env, &access)?;
        access.pin = Some(pin);
        access.paper = None;
        access.identity = Some(identity.clone());
        let disk = Arc::new(Self {
            url,
            env,
            access,
            cache: Cache::open(cache, limit)?,
            snapshot: RwLock::new(Arc::new(snapshot)),
            keychain: std::sync::atomic::AtomicBool::new(false),
        });
        Ok((disk, identity))
    }

    fn refresh(&self) -> Result<()> {
        let (snapshot, _, _) = Self::load(&self.url, &self.env, &self.access)?;
        *self.snapshot.write().unwrap() = Arc::new(snapshot);
        Ok(())
    }

    fn background(self: &Arc<Self>, interval: Duration) {
        let weak = Arc::downgrade(self);
        std::thread::spawn(move || loop {
            std::thread::sleep(interval);
            let Some(disk) = weak.upgrade() else {
                return;
            };
            if disk.refresh().is_err() {
                eprintln!("layr mount: refresh failed; the previous view remains available");
            }
        });
    }

    fn locked(&self) -> bool {
        self.keychain.load(std::sync::atomic::Ordering::SeqCst)
            && !keystore::get(&keystore::account(&self.url)).is_ok_and(|key| key.is_some())
    }

    pub fn lookup(&self, path: &str) -> Option<(Node, Vec<(String, Node)>)> {
        let locked = self.locked();
        let view = self.snapshot.read().unwrap().clone();
        let node = |path: &str| view.nodes.get(path).cloned().unwrap_or(Node::Folder { time: 0 });
        if let Some(children) = view.folders.get(path) {
            return Some((
                node(path),
                children
                    .iter()
                    .filter(|path| !locked || view.folders.contains_key(*path))
                    .map(|path| (path.clone(), node(path)))
                    .collect(),
            ));
        }
        if locked {
            return None;
        }
        view.nodes.get(path).cloned().map(|node| (node, vec![]))
    }

    pub fn read(&self, content: &Content) -> Result<Arc<Vec<u8>>> {
        if self.locked() {
            bail!("the store is locked");
        }
        match content {
            Content::Text(bytes) => Ok(bytes.clone()),
            Content::Stored { entry, reader } => {
                if let Some(bytes) = self.cache.get(&entry.2)? {
                    return Ok(Arc::new(bytes));
                }
                let bytes = reader.lock().unwrap().read(entry)?;
                self.cache.put(&entry.2, &bytes)?;
                Ok(Arc::new(bytes))
            }
        }
    }
}

struct Cache {
    dir: PathBuf,
    limit: u64,
    lock: Mutex<()>,
}
impl Cache {
    fn open(dir: PathBuf, limit: u64) -> Result<Self> {
        use std::os::unix::fs::{DirBuilderExt, MetadataExt, PermissionsExt};
        std::fs::DirBuilder::new().recursive(true).mode(0o700).create(&dir)?;
        let metadata = std::fs::symlink_metadata(&dir)?;
        if !metadata.is_dir() || metadata.uid() != rustix::process::geteuid().as_raw() {
            bail!("invalid disk cache folder");
        }
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))?;
        let cache = Self { dir, limit, lock: Mutex::new(()) };
        cache.evict()?;
        Ok(cache)
    }
    fn get(&self, hash: &str) -> Result<Option<Vec<u8>>> {
        use std::io::Read;
        use std::os::unix::fs::OpenOptionsExt;
        let _lock = self.lock.lock().unwrap();
        let path = self.dir.join(hash);
        let file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path);
        let Ok(mut file) = file else {
            return Ok(None);
        };
        if !file.metadata()?.is_file() {
            return Ok(None);
        }
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;
        if hex::encode(Sha256::digest(&bytes)) != hash {
            std::fs::remove_file(path)?;
            return Ok(None);
        }
        file.set_times(std::fs::FileTimes::new().set_modified(std::time::SystemTime::now()))?;
        Ok(Some(bytes))
    }
    fn put(&self, hash: &str, bytes: &[u8]) -> Result<()> {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        if bytes.len() as u64 > self.limit {
            return Ok(());
        }
        let _lock = self.lock.lock().unwrap();
        let partial = self.dir.join(format!(".part-{}", crate::ids::new_id()));
        let mut file =
            std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&partial)?;
        file.write_all(bytes)?;
        std::fs::rename(partial, self.dir.join(hash))?;
        self.evict()
    }
    fn evict(&self) -> Result<()> {
        let mut files = Vec::new();
        let mut total = 0u64;
        for item in std::fs::read_dir(&self.dir)? {
            let item = item?;
            let name = item.file_name().to_string_lossy().into_owned();
            if name.len() != 64 || !name.bytes().all(|c| c.is_ascii_hexdigit()) {
                continue;
            }
            let metadata = std::fs::symlink_metadata(item.path())?;
            if !metadata.is_file() {
                continue;
            }
            total = total.saturating_add(metadata.len());
            files.push((metadata.modified()?, item.path(), metadata.len()));
        }
        files.sort();
        for (_, path, size) in files {
            if total <= self.limit {
                break;
            }
            std::fs::remove_file(path)?;
            total -= size;
        }
        Ok(())
    }
}

#[derive(serde::Serialize, serde::Deserialize)]
struct Preferences {
    remote: String,
    listen: String,
    cache_size: u64,
}

fn user_home() -> Result<PathBuf> {
    Ok(PathBuf::from(std::env::var_os("HOME").context("HOME is not set")?))
}

fn cache_folder(home: &Path) -> PathBuf {
    #[cfg(target_os = "macos")]
    {
        home.join("Library/Caches/layr/mount")
    }
    #[cfg(not(target_os = "macos"))]
    {
        std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".cache")).join("layr/mount")
    }
}

fn preferences_file(home: &Path) -> PathBuf {
    std::env::var_os("XDG_CONFIG_HOME").map(PathBuf::from).unwrap_or_else(|| home.join(".config")).join("layr/mount.json")
}

fn pins_file(home: &Path) -> PathBuf {
    preferences_file(home).with_file_name("mount-pins.json")
}

fn load_pins(path: &Path) -> Result<BTreeMap<String, StorePin>> {
    match std::fs::read(path) {
        Ok(bytes) => Ok(serde_json::from_slice(&bytes).context("invalid mount pins file")?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(BTreeMap::new()),
        Err(e) => Err(e.into()),
    }
}

fn save_pins(path: &Path, pins: &BTreeMap<String, StorePin>) -> Result<()> {
    std::fs::create_dir_all(path.parent().context("mount pins file has no parent")?)?;
    crate::fsutil::atomic_write(path, &serde_json::to_vec(pins)?, 0o600)
}

fn lock_pins(path: &Path) -> Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::create_dir_all(path.parent().context("mount pins file has no parent")?)?;
    let lock = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(false)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path.with_extension("lock"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(lock)
}

fn record_pin(path: &Path, url: &str, pin: &StorePin) -> Result<bool> {
    let _lock = lock_pins(path)?;
    let mut pins = load_pins(path)?;
    if let Some(previous) = pins.get(url) {
        if previous != pin {
            bail!(
                "store pin changed during mount: pinned recipient {} project {}, store.json recipient {} project {}",
                previous.recipient,
                previous.project,
                pin.recipient,
                pin.project
            );
        }
        return Ok(false);
    }
    pins.insert(url.into(), pin.clone());
    save_pins(path, &pins)?;
    Ok(true)
}

fn resolve_remote(remote: &str) -> Result<(String, Option<StorePin>)> {
    if remote.contains("://") {
        return Ok((remote.into(), None));
    }
    let root = crate::store::default_root();
    let mut project = std::env::var("LAYR_PROJECT").ok();
    if project.is_none() {
        let cwd = std::env::current_dir()?;
        if let Ok(relative) = cwd.strip_prefix(root.join("projects")) {
            project = relative.components().next().map(|part| part.as_os_str().to_string_lossy().into_owned());
        }
    }
    if let Some(project) = project {
        let path = root.join("projects").join(project);
        let db = rusqlite::Connection::open_with_flags(crate::store::db_file(&path), rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
        use rusqlite::OptionalExtension;
        let value: Option<String> = db
            .query_row("SELECT value FROM config WHERE key=?1", [format!("cloud.remote.{remote}.url")], |row| row.get(0))
            .optional()?;
        let Some(value) = value else {
            let store = catalog::known(&user_home()?)?.into_iter().find(|s| s.name == remote).context("unknown store remote")?;
            return Ok((store.url, store.pin));
        };
        let pin: String = db
            .query_row("SELECT value FROM config WHERE key=?1", [format!("cloud.remote.{remote}.store")], |row| row.get(0))
            .context("named remote has no store pin; run layr fetch <remote> first")?;
        return Ok((
            serde_json::from_str(&value).context("invalid named remote URL")?,
            Some(serde_json::from_str(&pin).context("invalid named remote store pin")?),
        ));
    }
    if let Some(store) = catalog::known(&user_home()?)?.into_iter().find(|s| s.name == remote) {
        return Ok((store.url, store.pin));
    }
    bail!("use a store URL outside a project, such as file:///path, s3://bucket/prefix or layr+https://host/project")
}

fn machine_access() -> Option<(String, ed25519_dalek::SigningKey)> {
    let dir = crate::store::default_root().join("machine");
    let id = std::fs::read_to_string(dir.join("id")).ok()?.trim().to_owned();
    if !crate::model::is_uuid(&id) {
        return None;
    }
    let key = keystore::paper_file(&dir.join("key")).ok()?;
    let bytes: [u8; 32] = hex::decode(key.trim()).ok()?.try_into().ok()?;
    Some((id, ed25519_dalek::SigningKey::from_bytes(&bytes)))
}

pub fn run(args: &[String]) -> Result<i32> {
    use crate::args::{parse, Spec};
    use std::io::IsTerminal;
    let a = parse(
        &Spec::new()
            .value("--listen")
            .value("--cache")
            .value("--cache-size")
            .value("--at")
            .value("--paper-key-file")
            .value("--forget-pin")
            .flag("--paper-key-stdin")
            .flag("--keychain-only")
            .flag("--all")
            .flag("--unlock")
            .flag("--lock")
            .flag("--status")
            .flag("--json")
            .flag("--install")
            .flag("--remove")
            .flag("--help|-h"),
        args,
    )?;
    if a.has("--help") {
        println!("Usage: layr mount <remote> [--listen 127.0.0.1:8095] [--cache-size <bytes>]\n       layr mount --unlock <remote> | --lock <remote> | --status [--json]\n       layr mount --all [--listen 127.0.0.1:8095]\n       layr mount --install [<remote> | --all] | --remove [--all]\n       layr mount --forget-pin <url>\nUse --paper-key-file <0600-file> or --paper-key-stdin once to unlock the disk.");
        return Ok(0);
    }
    if !cfg!(target_os = "macos") && (a.has("--install") || a.has("--remove") || a.has("--at")) {
        bail!("mount --install, --remove and --at require macOS");
    }
    if !cfg!(target_os = "macos") && (a.has("--unlock") || a.has("--lock") || a.has("--status")) {
        eprintln!("the Keychain is macOS only");
        return Ok(2);
    }
    let actions = ["--unlock", "--lock", "--status", "--install", "--remove"].iter().filter(|flag| a.has(flag)).count();
    if actions > 1 {
        bail!("choose one mount action");
    }
    if a.has("--all") && (!a.pos.is_empty() || a.has("--unlock") || a.has("--lock") || a.has("--status")) {
        bail!("--all takes no remote or Keychain action");
    }
    let home = user_home()?;
    if a.has("--status") {
        if !a.pos.is_empty() {
            bail!("mount --status takes no remote");
        }
        catalog::status(&home, a.has("--json"))?;
        return Ok(0);
    }
    if a.has("--all") {
        return all::run(&a, &home);
    }
    if a.has("--lock") {
        let remote = a.pos.first().context("mount --lock needs a remote")?;
        if a.pos.len() != 1 {
            bail!("mount --lock takes one remote");
        }
        let (url, _) = resolve_remote(remote)?;
        keystore::remove(&keystore::account(&url))?;
        println!("locked {url}");
        return Ok(0);
    }
    if let Some(url) = a.get("--forget-pin") {
        if !a.pos.is_empty() {
            bail!("mount --forget-pin takes only a URL");
        }
        if !url.contains("://") {
            bail!("--forget-pin requires a store URL; named remotes use layr remote trust-store");
        }
        let path = pins_file(&home);
        let _lock = lock_pins(&path)?;
        let mut pins = load_pins(&path)?;
        pins.remove(url);
        save_pins(&path, &pins)?;
        println!("removed mount pin for {url}");
        return Ok(0);
    }
    if a.has("--remove") {
        if a.has("--install") || !a.pos.is_empty() {
            bail!("mount --remove takes no remote");
        }
        return launch::remove(&home);
    }
    if a.pos.len() > 1 {
        bail!("mount takes one remote");
    }
    if a.has("--paper-key-stdin") && a.has("--paper-key-file") {
        bail!("choose one paper key input");
    }
    let saved: Option<Preferences> =
        std::fs::read(preferences_file(&home)).ok().and_then(|bytes| serde_json::from_slice(&bytes).ok());
    let remote =
        a.pos.first().cloned().or_else(|| saved.as_ref().map(|s| s.remote.clone())).context("mount needs a store remote URL")?;
    let (url, project_pin) = resolve_remote(&remote)?;
    let pins = load_pins(&pins_file(&home))?;
    let pin = project_pin.clone().or_else(|| pins.get(&url).cloned());
    let listen = a.get("--listen").or_else(|| saved.as_ref().map(|s| s.listen.as_str())).unwrap_or("127.0.0.1:8095");
    let address: std::net::SocketAddr = listen.parse().context("--listen needs an IP address and port")?;
    if !address.ip().is_loopback() {
        bail!("the disk must listen on a loopback address");
    }
    let limit =
        a.get("--cache-size").map(str::parse).transpose()?.or_else(|| saved.as_ref().map(|s| s.cache_size)).unwrap_or(1 << 30);
    let cache = a.get("--cache").map(PathBuf::from).unwrap_or_else(|| cache_folder(&home));
    if a.has("--keychain-only") {
        if a.has("--paper-key-file") || a.has("--paper-key-stdin") || actions > 0 {
            bail!("the installed agent uses only the Keychain");
        }
        let mut env = std::env::vars().collect();
        catalog::profile_env(&remote, &url, &home, &mut env)?;
        return serve(all::keychain_only(url, env, pin, cache, limit), address, a.get("--at"));
    }
    let paper = if let Some(path) = a.get("--paper-key-file") {
        Some(keystore::paper_file(Path::new(path))?)
    } else if a.has("--paper-key-stdin") {
        if std::io::stdin().is_terminal() {
            bail!("pipe the paper key to stdin; terminal input is refused");
        }
        Some(keystore::bounded(std::io::stdin().lock())?)
    } else {
        None
    };
    let account = keystore::account(&url);
    let cached = if paper.is_none() { keystore::get(&account)? } else { None };
    let access = Access { machine: machine_access(), paper, identity: cached, pin, ..Default::default() };
    let mut env = std::env::vars().collect();
    catalog::profile_env(&remote, &url, &home, &mut env)?;
    let (disk, identity) = Disk::open(url.clone(), env, access, cache.clone(), limit)?;
    let pin = disk.access.pin.as_ref().context("disk has no store pin")?;
    if project_pin.is_none() && record_pin(&pins_file(&home), &url, pin)? {
        eprintln!("pinned store recipient {} project {}", pin.recipient, pin.project);
    }
    let span = crate::trace::span("disk Keychain save");
    keystore::set(&account, &identity)?;
    drop(span);
    disk.keychain.store(cfg!(target_os = "macos"), std::sync::atomic::Ordering::SeqCst);
    if a.has("--unlock") {
        println!("unlocked {url}");
        return Ok(0);
    }
    let prefs = preferences_file(&home);
    std::fs::create_dir_all(prefs.parent().unwrap())?;
    crate::fsutil::atomic_write(
        &prefs,
        &serde_json::to_vec(&Preferences { remote: url.clone(), listen: listen.into(), cache_size: limit })?,
        0o600,
    )?;
    if a.has("--install") {
        launch::install(&url, listen, &cache, limit, &home)?;
        return Ok(0);
    }
    if a.has("--at") && !cfg!(target_os = "macos") {
        bail!("--at requires macOS");
    }
    disk.background(Duration::from_secs(120));
    serve(disk, address, a.get("--at"))
}

pub(super) trait View: Send + Sync {
    fn lookup(&self, path: &str) -> Option<(Node, Vec<(String, Node)>)>;
    fn read(&self, path: &str, content: &Content) -> Result<Arc<Vec<u8>>>;
    fn locked(&self, path: &str) -> bool;
    fn served_urls(&self) -> Vec<String>;
}
impl View for Disk {
    fn lookup(&self, path: &str) -> Option<(Node, Vec<(String, Node)>)> {
        Disk::lookup(self, path)
    }
    fn read(&self, _path: &str, content: &Content) -> Result<Arc<Vec<u8>>> {
        Disk::read(self, content)
    }
    fn locked(&self, _path: &str) -> bool {
        Disk::locked(self)
    }
    fn served_urls(&self) -> Vec<String> {
        vec![self.url.clone()]
    }
}

fn serve(disk: Arc<dyn View>, address: std::net::SocketAddr, at: Option<&str>) -> Result<i32> {
    let listener = std::net::TcpListener::bind(address)?;
    let local = listener.local_addr()?;
    let server_url = format!("http://{local}/");
    let _serving = catalog::serving(&user_home()?, disk.served_urls(), server_url.clone())?;
    eprintln!("layr mount: {server_url} (read only)");
    if let Some(at) = at {
        let at = PathBuf::from(at);
        let url = server_url.clone();
        std::thread::spawn(move || {
            if launch::mount(&url, &at).is_err() {
                eprintln!("layr mount: the WebDAV client could not mount the disk");
            }
        });
    }
    let active = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    for stream in listener.incoming() {
        let mut stream = stream?;
        use std::sync::atomic::Ordering;
        if active.fetch_add(1, Ordering::SeqCst) >= 32 {
            active.fetch_sub(1, Ordering::SeqCst);
            let _ = crate::http::response(&mut stream, 503, &[], b"");
            continue;
        }
        let disk = disk.clone();
        let active = active.clone();
        std::thread::spawn(move || {
            let _ = connection(disk.as_ref(), stream);
            active.fetch_sub(1, Ordering::SeqCst);
        });
    }
    Ok(0)
}

fn connection(disk: &dyn View, stream: std::net::TcpStream) -> Result<()> {
    let limits = crate::http::Limits {
        request_line: 8192,
        header_count: 64,
        header_bytes: 32768,
        body_bytes: 65536,
        header_timeout: Duration::from_secs(10),
        body_timeout: Duration::from_secs(10),
        write_timeout: Duration::from_secs(30),
    };
    limits.configure(&stream)?;
    let mut stream = crate::http::Socket::new(stream, limits.header_timeout);
    let request = match crate::http::request(&mut stream, &limits) {
        Ok(request) => request,
        Err(error) => {
            if std::env::var_os("LAYR_TRACE").is_some() {
                eprintln!("disk HTTP parse: {} {}", error.status, error.message);
            }
            stream.begin_write(limits.write_timeout);
            crate::http::response(&mut stream, error.status, &[], b"")?;
            return Ok(());
        }
    };
    stream.begin_write(limits.write_timeout);
    if std::env::var_os("LAYR_TRACE").is_some() {
        eprintln!("disk HTTP {}", request.method);
    }
    let host = request.header("host").unwrap_or("");
    let host = reqwest::Url::parse(&format!("http://{host}/")).ok();
    let local_host = host
        .as_ref()
        .and_then(|u| u.host_str())
        .is_some_and(|h| h == "localhost" || h == "[::1]" || h.parse::<std::net::IpAddr>().is_ok_and(|ip| ip.is_loopback()));
    if !local_host || request.header("origin").is_some() {
        crate::http::response(&mut stream, 403, &[], b"")?;
        return Ok(());
    }
    if webdav::serve(disk, &request, &mut stream).is_err() {
        // Backend errors contain object paths; they stay out of the HTTP response.
        let _ = crate::http::response(&mut stream, 503, &[], b"");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "macos")]
    #[test]
    fn keychain_lock_hides_files_and_blocks_webdav_until_unlock() {
        keystore::test_service();
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let url = format!("file://{}", root.join("remote").display());
        let backend = crate::store_remote::backend::Backend::open(&url).unwrap();
        let identity = crate::store_remote::keys::generate_identity();
        let recipient = crate::store_remote::keys::recipient_of(&identity).unwrap();
        backend
            .put_new(
                "store.json",
                serde_json::to_vec(&serde_json::json!({"format":2,"project":crate::ids::new_id(),"recipient":recipient}))
                    .unwrap(),
            )
            .unwrap();
        let (disk, _) = Disk::open(
            url.clone(),
            HashMap::new(),
            Access { identity: Some(identity.clone()), ..Default::default() },
            root.join("cache"),
            0,
        )
        .unwrap();
        let nodes =
            BTreeMap::from([("main".into(), Node::Folder { time: 0 }), ("main/file".into(), text(b"private".to_vec(), 0))]);
        *disk.snapshot.write().unwrap() = Arc::new(Snapshot { folders: folders(&nodes), nodes });
        let account = keystore::account(&url);
        keystore::set(&account, &identity).unwrap();
        disk.keychain.store(true, std::sync::atomic::Ordering::SeqCst);
        assert!(!disk.locked());
        let request = |method: &str, path: &str| {
            let raw = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\nDepth: 1\r\n\r\n");
            let limits = crate::http::Limits {
                request_line: 8192,
                header_count: 64,
                header_bytes: 32768,
                body_bytes: 65536,
                header_timeout: Duration::from_secs(1),
                body_timeout: Duration::from_secs(1),
                write_timeout: Duration::from_secs(1),
            };
            crate::http::request(&mut std::io::BufReader::new(raw.as_bytes()), &limits).unwrap()
        };
        let get = request("GET", "/main/file");
        let mut response = Vec::new();
        webdav::serve(disk.as_ref(), &get, &mut response).unwrap();
        assert!(response.ends_with(b"private"));
        keystore::remove(&account).unwrap();
        assert!(disk.locked());
        assert!(disk.lookup("main").unwrap().1.is_empty());
        response.clear();
        webdav::serve(disk.as_ref(), &get, &mut response).unwrap();
        assert!(response.starts_with(b"HTTP/1.1 423 Locked"));
        assert!(!response.ends_with(b"private"));
        keystore::set(&account, &identity).unwrap();
        response.clear();
        webdav::serve(disk.as_ref(), &get, &mut response).unwrap();
        assert!(response.ends_with(b"private"));
        keystore::remove(&account).unwrap();
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn url_pin_updates_preserve_other_urls_and_refuse_a_conflicting_first_use() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let path = root.join("mount-pins.json");
        let first = StorePin { project: crate::ids::new_id(), recipient: "age1first".into(), paper_recipient: None };
        let second = StorePin { project: crate::ids::new_id(), recipient: "age1second".into(), paper_recipient: None };
        assert!(record_pin(&path, "file:///one", &first).unwrap());
        assert!(record_pin(&path, "file:///two", &second).unwrap());
        assert!(!record_pin(&path, "file:///one", &first).unwrap());
        let error = record_pin(&path, "file:///one", &second).unwrap_err().to_string();
        assert!(error.contains(&first.project) && error.contains(&second.project));
        assert!(error.contains(&first.recipient) && error.contains(&second.recipient));
        assert_eq!(load_pins(&path).unwrap().len(), 2);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn a_refresh_rejects_another_project_and_keeps_the_previous_view() {
        use crate::store_remote::{backend::Backend, keys};
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let url = format!("file://{}", root.join("remote").display());
        let backend = Backend::open(&url).unwrap();
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let project = crate::ids::new_id();
        backend
            .put_new(
                "store.json",
                serde_json::to_vec(&serde_json::json!({
            "format":2,"project":project,"recipient":recipient}))
                .unwrap(),
            )
            .unwrap();
        let (disk, _) =
            Disk::open(url, HashMap::new(), Access { identity: Some(identity), ..Default::default() }, root.join("cache"), 0)
                .unwrap();
        let before = disk.snapshot.read().unwrap().clone();
        let other = crate::ids::new_id();
        std::fs::write(
            root.join("remote/store.json"),
            serde_json::to_vec(&serde_json::json!({
            "format":2,"project":other,"recipient":recipient}))
            .unwrap(),
        )
        .unwrap();
        let error = disk.refresh().unwrap_err().to_string();
        assert!(error.contains(&project) && error.contains(&other), "{error}");
        assert!(Arc::ptr_eq(&before, &disk.snapshot.read().unwrap()));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn names_and_links_stay_inside_each_line() {
        assert_eq!(line_folder("feature/a%b"), "feature%2Fa%25b");
        assert_eq!(join("a/b/link", "../c").as_deref(), Some("a/c"));
        assert_eq!(join("link", "../outside"), None);
        assert_eq!(join("link", "/etc/passwd"), None);
    }
    #[test]
    fn cache_checks_hashes_and_enforces_its_size_limit() {
        let dir = std::env::temp_dir().join(format!("layr-disk-cache-{}", crate::ids::new_id()));
        let cache = Cache::open(dir.clone(), 5).unwrap();
        let hash = |data: &[u8]| hex::encode(Sha256::digest(data));
        cache.put(&hash(b"abc"), b"abc").unwrap();
        assert_eq!(cache.get(&hash(b"abc")).unwrap().unwrap(), b"abc");
        std::fs::write(dir.join(hash(b"abc")), b"bad").unwrap();
        assert!(cache.get(&hash(b"abc")).unwrap().is_none());
        cache.put(&hash(b"abc"), b"abc").unwrap();
        cache.put(&hash(b"xyz"), b"xyz").unwrap();
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        cache.put(&hash(b"toolong"), b"toolong").unwrap();
        assert!(!dir.join(hash(b"toolong")).exists());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
