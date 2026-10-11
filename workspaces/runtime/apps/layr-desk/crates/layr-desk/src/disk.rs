//! The network disk (docs/design.md, "Network disk"): one folder per vault of a gateway, each with
//! the latest state of its project, read only. Files decrypt on this machine when they are read,
//! and stay in a cache folder keyed by their SHA-256.

use crate::gateway::{Site, Vault};
use crate::keystore::{self, KeyStore};
use anyhow::{anyhow, Result};
use desk_core::{Entry, Reader};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, RwLock};

/// What a path of the disk is.
pub enum Node {
    Folder {
        time: i64,
    },
    File {
        size: u64,
        etag: String,
        time: i64,
        content: Content,
    },
}

pub enum Content {
    Stored { project: String, entry: Entry },
    Text(Arc<Vec<u8>>),
}

struct Project {
    time: i64,
    reader: Option<Arc<Reader<Vault>>>,
    files: BTreeMap<String, Node>,
    folders: BTreeMap<String, BTreeSet<String>>,
}

struct Snapshot {
    loaded: Instant,
    projects: BTreeMap<String, Project>,
}

pub struct Disk {
    site: Site,
    site_url: String,
    keys: Box<dyn KeyStore>,
    cache: PathBuf,
    every: Duration,
    snapshot: RwLock<Option<Arc<Snapshot>>>,
    loading: Arc<Mutex<()>>,
}

fn note(text: String, time: i64) -> Node {
    let bytes = Arc::new(text.into_bytes());
    let etag = hex::encode(Sha256::digest(bytes.as_slice()));
    Node::File {
        size: bytes.len() as u64,
        etag,
        time,
        content: Content::Text(bytes),
    }
}

/// Folders with their children, for the paths of one state.
fn folders<'a>(paths: impl Iterator<Item = &'a String>) -> BTreeMap<String, BTreeSet<String>> {
    let mut folders: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    folders.insert(String::new(), BTreeSet::new());
    for path in paths {
        let parts: Vec<&str> = path.split('/').collect();
        for i in 1..parts.len() {
            let parent = parts[..i - 1].join("/");
            let folder = parts[..i].join("/");
            folders.entry(folder.clone()).or_default();
            folders.entry(parent).or_default().insert(folder);
        }
        folders
            .entry(parts[..parts.len() - 1].join("/"))
            .or_default()
            .insert(path.clone());
    }
    folders
}

impl Disk {
    pub fn new(
        site_url: &str,
        keys: Box<dyn KeyStore>,
        cache: PathBuf,
        every: Duration,
    ) -> Result<Self> {
        std::fs::create_dir_all(&cache)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o700))?;
        }
        Ok(Disk {
            site: Site::new(site_url)?,
            site_url: site_url.to_string(),
            keys,
            cache,
            every,
            snapshot: RwLock::new(None),
            loading: Arc::new(Mutex::new(())),
        })
    }

    async fn project(&self, name: &str) -> Project {
        let now = chrono_now();
        let locked = |text: String| {
            let mut files = BTreeMap::new();
            files.insert("LOCKED.txt".to_string(), note(text, now));
            Project {
                time: now,
                reader: None,
                folders: folders(files.keys()),
                files,
            }
        };
        let identity = match keystore::account(&self.site_url, name).and_then(|a| self.keys.get(&a)) {
            Ok(Some(identity)) => identity,
            Ok(None) => {
                return locked(format!(
                    "This vault is locked on this Mac. Open it once with its paper key:\n\n    layr-desk unlock --site {} --project {name}\n",
                    self.site_url
                ))
            }
            Err(e) => return locked(format!("The key of this vault cannot be read: {e:#}\n")),
        };
        match self.open(name, &identity).await {
            Ok(project) => project,
            Err(e) => {
                let mut project = locked(format!("This vault cannot be read now: {e:#}\n"));
                let note = project.files.remove("LOCKED.txt").expect("made above");
                project.files.insert("ERROR.txt".into(), note);
                project.folders = folders(project.files.keys());
                project
            }
        }
    }

    async fn open(&self, name: &str, identity: &str) -> Result<Project> {
        let vault = self.site.vault(name).unlocked(identity);
        let names = vault.states().await?;
        let reader = Arc::new(Reader::new(vault, identity, &names)?);
        let Ok((seq, attempt)) = reader.latest() else {
            let mut files = BTreeMap::new();
            files.insert(
                "EMPTY.txt".into(),
                note(
                    "The writer has not pushed a state yet.\n".into(),
                    chrono_now(),
                ),
            );
            return Ok(Project {
                time: chrono_now(),
                reader: None,
                folders: folders(files.keys()),
                files,
            });
        };
        let (header, _) = reader.tree(seq, &attempt).await?;
        let entries = reader.files(Some((seq, attempt))).await?;
        let mut files = BTreeMap::new();
        for (path, entry) in &entries {
            // A link shows the file it points to inside the state.
            let shown = resolve(path, entry, &entries, &reader).await;
            files.insert(
                path.clone(),
                Node::File {
                    size: shown.size(),
                    etag: shown.sha256().to_string(),
                    time: header.time,
                    content: Content::Stored {
                        project: name.to_string(),
                        entry: shown,
                    },
                },
            );
        }
        Ok(Project {
            time: header.time,
            reader: Some(reader),
            folders: folders(files.keys()),
            files,
        })
    }

    async fn refresh(&self) -> Result<Arc<Snapshot>> {
        let mut projects = BTreeMap::new();
        for name in self.site.projects().await? {
            projects.insert(name.clone(), self.project(&name).await);
        }
        let snapshot = Arc::new(Snapshot {
            loaded: Instant::now(),
            projects,
        });
        *self.snapshot.write().await = Some(snapshot.clone());
        Ok(snapshot)
    }

    async fn current(self: &Arc<Self>) -> Result<Arc<Snapshot>> {
        let snapshot = self.snapshot.read().await.clone();
        match snapshot {
            Some(s) if s.loaded.elapsed() < self.every => Ok(s),
            Some(s) => {
                // Serve the old view and load the new one once, in the background.
                if let Ok(guard) = self.loading.clone().try_lock_owned() {
                    let disk = self.clone();
                    tokio::spawn(async move {
                        let _guard = guard;
                        if let Err(e) = disk.refresh().await {
                            eprintln!("layr-desk: refresh failed: {e:#}");
                        }
                    });
                }
                Ok(s)
            }
            None => {
                let _guard = self.loading.lock().await;
                if let Some(s) = self.snapshot.read().await.clone() {
                    return Ok(s);
                }
                self.refresh().await
            }
        }
    }

    /// The node of a path (no leading or trailing slash), and the children of a folder.
    pub async fn lookup(
        self: &Arc<Self>,
        path: &str,
    ) -> Result<Option<(Node, Vec<(String, Node)>)>> {
        let snapshot = self.current().await?;
        let newest = snapshot
            .projects
            .values()
            .map(|p| p.time)
            .max()
            .unwrap_or_else(chrono_now);
        if path.is_empty() {
            let children = snapshot
                .projects
                .iter()
                .map(|(n, p)| (n.clone(), Node::Folder { time: p.time }))
                .collect();
            return Ok(Some((Node::Folder { time: newest }, children)));
        }
        let (name, rest) = path.split_once('/').unwrap_or((path, ""));
        let Some(project) = snapshot.projects.get(name) else {
            return Ok(None);
        };
        let full = |p: &str| {
            if p.is_empty() {
                name.to_string()
            } else {
                format!("{name}/{p}")
            }
        };
        if let Some(children) = project.folders.get(rest) {
            let listed = children
                .iter()
                .map(|c| (full(c), shallow(project, c)))
                .collect();
            return Ok(Some((Node::Folder { time: project.time }, listed)));
        }
        Ok(project
            .files
            .get(rest)
            .map(|_| (shallow(project, rest), Vec::new())))
    }

    /// The bytes of a file node, decrypted here and checked against its tree entry.
    pub async fn read(self: &Arc<Self>, content: &Content) -> Result<Arc<Vec<u8>>> {
        let (project, entry) = match content {
            Content::Text(bytes) => return Ok(bytes.clone()),
            Content::Stored { project, entry } => (project, entry),
        };
        let cached = self.cache.join(entry.sha256());
        if let Ok(data) = std::fs::read(&cached) {
            if hex::encode(Sha256::digest(&data)) == entry.sha256() {
                return Ok(Arc::new(data));
            }
        }
        let snapshot = self.current().await?;
        let reader = snapshot
            .projects
            .get(project)
            .and_then(|p| p.reader.clone())
            .ok_or_else(|| anyhow!("the vault {project} is not open"))?;
        let data = reader.read(entry).await?;
        store(&cached, &data)?;
        Ok(Arc::new(data))
    }
}

fn shallow(project: &Project, path: &str) -> Node {
    match project.files.get(path) {
        Some(Node::File {
            size,
            etag,
            time,
            content,
        }) => Node::File {
            size: *size,
            etag: etag.clone(),
            time: *time,
            content: match content {
                Content::Stored { project, entry } => Content::Stored {
                    project: project.clone(),
                    entry: entry.clone(),
                },
                Content::Text(bytes) => Content::Text(bytes.clone()),
            },
        },
        _ => Node::Folder { time: project.time },
    }
}

/// The entry that a link shows: the file it points to inside the state (up to 8 hops), or the
/// link itself when it points outside or nowhere.
async fn resolve(
    path: &str,
    entry: &Entry,
    files: &BTreeMap<String, Entry>,
    reader: &Reader<Vault>,
) -> Entry {
    let mut at = (path.to_string(), entry.clone());
    for _ in 0..8 {
        if !at.1.is_link() {
            return at.1;
        }
        let Ok(text) = reader.read(&at.1).await else {
            break;
        };
        let Some(target) = join(&at.0, &String::from_utf8_lossy(&text)) else {
            break;
        };
        match files.get(&target) {
            Some(next) => at = (target, next.clone()),
            None => break,
        }
    }
    if at.1.is_link() {
        entry.clone()
    } else {
        at.1
    }
}

/// `target` relative to the folder of `link`, normalized; none when it leaves the state.
fn join(link: &str, target: &str) -> Option<String> {
    if target.starts_with('/') {
        return None;
    }
    let mut parts: Vec<&str> = link.split('/').collect();
    parts.pop();
    for part in target.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            p => parts.push(p),
        }
    }
    Some(parts.join("/"))
}

fn store(path: &Path, data: &[u8]) -> Result<()> {
    let partial = path.with_extension("part");
    std::fs::write(&partial, data)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&partial, std::fs::Permissions::from_mode(0o600))?;
    }
    std::fs::rename(&partial, path)?;
    Ok(())
}

pub fn chrono_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn links_resolve_inside_the_state_only() {
        assert_eq!(join("a/b/link", "../c.txt").as_deref(), Some("a/c.txt"));
        assert_eq!(join("link", "dir/./f").as_deref(), Some("dir/f"));
        assert_eq!(join("link", "../outside"), None);
        assert_eq!(join("a/link", "/etc/passwd"), None);
    }

    #[test]
    fn folders_list_their_children() {
        let paths = [
            "a/b/c.txt".to_string(),
            "a/d.txt".to_string(),
            "e".to_string(),
        ];
        let f = folders(paths.iter());
        assert_eq!(f[""], BTreeSet::from(["a".to_string(), "e".to_string()]));
        assert_eq!(
            f["a"],
            BTreeSet::from(["a/b".to_string(), "a/d.txt".to_string()])
        );
        assert_eq!(f["a/b"], BTreeSet::from(["a/b/c.txt".to_string()]));
    }
}
