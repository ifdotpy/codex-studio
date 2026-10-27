//! Route one WebDAV root to independently locked store views.
use super::*;

struct StoreView {
    known: catalog::Known,
    disk: Mutex<Option<Arc<Disk>>>,
}
pub(super) struct All {
    stores: BTreeMap<String, StoreView>,
    env: HashMap<String, String>,
    cache: PathBuf,
    limit: u64,
}
impl All {
    fn open(home: &Path, cache: PathBuf, limit: u64) -> Result<Arc<Self>> {
        let stores =
            catalog::known(home)?.into_iter().map(|s| (s.name.clone(), StoreView { known: s, disk: Mutex::new(None) })).collect();
        Ok(Arc::new(Self { stores, env: std::env::vars().collect(), cache, limit }))
    }
    fn disk(&self, store: &StoreView) -> Result<Option<Arc<Disk>>> {
        let identity = keystore::get(&keystore::account(&store.known.url))?;
        let mut disk = store.disk.lock().unwrap();
        let Some(identity) = identity else {
            *disk = None;
            return Ok(None);
        };
        if disk.is_none() {
            let mut env = self.env.clone();
            if !env.get("AWS_PROFILE").is_some_and(|v| !v.is_empty()) {
                if let Some(p) = &store.known.profile {
                    env.insert("AWS_PROFILE".into(), p.clone());
                }
            }
            // An all-store mount only uses keys explicitly kept in the Keychain.
            env.remove("LAYR_PERSON_KEY_FILE");
            let access = Access { identity: Some(identity), pin: store.known.pin.clone(), ..Default::default() };
            let (opened, _) = Disk::open(
                store.known.url.clone(),
                env,
                access,
                self.cache.join(keystore::account(&store.known.url)),
                self.limit,
            )?;
            opened.keychain.store(cfg!(target_os = "macos"), std::sync::atomic::Ordering::SeqCst);
            opened.background(Duration::from_secs(120));
            *disk = Some(opened);
        }
        Ok(disk.clone())
    }
}
impl View for All {
    fn lookup(&self, path: &str) -> Option<(Node, Vec<(String, Node)>)> {
        if path.is_empty() {
            return Some((
                Node::Folder { time: 0 },
                self.stores.keys().map(|name| (name.clone(), Node::Folder { time: 0 })).collect(),
            ));
        }
        let (name, rest) = path.split_once('/').unwrap_or((path, ""));
        let store = self.stores.get(name)?;
        match self.disk(store) {
            Ok(Some(disk)) => {
                let (node, children) = disk.lookup(rest)?;
                Some((node, children.into_iter().map(|(path, node)| (format!("{name}/{path}"), node)).collect()))
            }
            state => {
                let (file, body) = if state.is_ok() {
                    ("LOCKED.txt", format!("Run: layr mount --unlock {}\n", store.known.name))
                } else {
                    ("ERROR.txt", "This store cannot be read now.\n".into())
                };
                let note = text(body.into_bytes(), 0);
                if rest.is_empty() {
                    Some((Node::Folder { time: 0 }, vec![(format!("{name}/{file}"), note)]))
                } else if rest == file {
                    Some((note, vec![]))
                } else {
                    None
                }
            }
        }
    }
    fn read(&self, path: &str, content: &Content) -> Result<Arc<Vec<u8>>> {
        if let Content::Text(bytes) = content {
            if path.ends_with("/LOCKED.txt") || path.ends_with("/ERROR.txt") {
                return Ok(bytes.clone());
            }
        }
        let (name, _) = path.split_once('/').context("store path has no folder")?;
        self.disk(self.stores.get(name).context("unknown store")?)?.context("the store is locked")?.read(content)
    }
    fn served_urls(&self) -> Vec<String> {
        vec!["--all".into()]
    }
    fn locked(&self, path: &str) -> bool {
        let (name, rest) = path.split_once('/').unwrap_or((path, ""));
        if rest == "LOCKED.txt" || rest == "ERROR.txt" {
            return false;
        }
        self.stores.get(name).is_some_and(|s| !keystore::get(&keystore::account(&s.known.url)).is_ok_and(|key| key.is_some()))
    }
}

pub(super) fn run(a: &crate::args::Parsed, home: &Path) -> Result<i32> {
    if a.has("--remove") {
        return launch::remove_all(home);
    }
    if a.has("--paper-key-file") || a.has("--paper-key-stdin") {
        bail!("unlock each store with layr mount --unlock <remote> first");
    }
    let listen = a.get("--listen").unwrap_or("127.0.0.1:8095");
    let address: std::net::SocketAddr = listen.parse().context("--listen needs an IP address and port")?;
    if !address.ip().is_loopback() {
        bail!("the disk must listen on a loopback address");
    }
    let limit = a.get("--cache-size").map(str::parse).transpose()?.unwrap_or(1 << 30);
    let cache = a.get("--cache").map(PathBuf::from).unwrap_or_else(|| cache_folder(home));
    if a.has("--install") {
        launch::install_all(listen, &cache, limit, home)?;
        return Ok(0);
    }
    let disk = All::open(home, cache, limit)?;
    serve(disk, address, a.get("--at"))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn locked_stores_have_one_note_and_distinct_folders() {
        #[cfg(target_os = "macos")]
        keystore::test_service();
        let url = format!("file:///layr-test-{}", crate::ids::new_id());
        let store = catalog::Known { name: "origin".into(), url, pin: None, profile: None };
        let disk = All {
            stores: BTreeMap::from([("origin".into(), StoreView { known: store, disk: Mutex::new(None) })]),
            env: HashMap::new(),
            cache: PathBuf::new(),
            limit: 0,
        };
        let (_, children) = disk.lookup("").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].0, "origin");
        let (_, children) = disk.lookup("origin").unwrap();
        assert_eq!(children.len(), 1);
        assert_eq!(children[0].0, "origin/LOCKED.txt");
        let Node::File { content, .. } = &children[0].1 else { panic!("expected note") };
        assert_eq!(disk.read("origin/LOCKED.txt", content).unwrap().as_slice(), b"Run: layr mount --unlock origin\n");
        assert!(disk.lookup("origin/main/private").is_none());
        assert!(disk.locked("origin/main/private"));
        assert!(!disk.locked("origin/LOCKED.txt"));
    }
}

/// An installed single-store agent never reopens a machine or person slot.
struct Single {
    all: All,
}
impl Single {
    fn store(&self) -> &StoreView {
        self.all.stores.values().next().unwrap()
    }
}
impl View for Single {
    fn lookup(&self, path: &str) -> Option<(Node, Vec<(String, Node)>)> {
        match self.all.disk(self.store()) {
            Ok(Some(disk)) => disk.lookup(path),
            _ if path.is_empty() => Some((Node::Folder { time: 0 }, vec![])),
            _ => None,
        }
    }
    fn read(&self, _path: &str, content: &Content) -> Result<Arc<Vec<u8>>> {
        self.all.disk(self.store())?.context("the store is locked")?.read(content)
    }
    fn locked(&self, _path: &str) -> bool {
        !keystore::get(&keystore::account(&self.store().known.url)).is_ok_and(|key| key.is_some())
    }
    fn served_urls(&self) -> Vec<String> {
        vec![self.store().known.url.clone()]
    }
}
pub(super) fn keychain_only(
    url: String,
    env: HashMap<String, String>,
    pin: Option<StorePin>,
    cache: PathBuf,
    limit: u64,
) -> Arc<dyn View> {
    let store = StoreView { known: catalog::Known { name: "store".into(), url, pin, profile: None }, disk: Mutex::new(None) };
    Arc::new(Single { all: All { stores: BTreeMap::from([("store".into(), store)]), env, cache, limit } })
}

#[cfg(test)]
mod installed_tests {
    use super::*;
    #[test]
    fn a_single_store_agent_stays_locked_without_a_keychain_item() {
        #[cfg(target_os = "macos")]
        keystore::test_service();
        // No backend exists. A machine slot must never be consulted on this path.
        let url = format!("file:///no-backend-{}", crate::ids::new_id());
        let disk = keychain_only(url.clone(), HashMap::new(), None, PathBuf::new(), 0);
        assert!(disk.locked("main/private"));
        assert!(disk.lookup("").unwrap().1.is_empty());
        assert!(disk.lookup("main/private").is_none());
        assert_eq!(disk.served_urls(), vec![url]);
    }
}
