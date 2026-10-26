//! Immutable objects in a folder or an S3 bucket.

use anyhow::{bail, Context, Result};
use std::fs::{self, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

pub use layr_store::s3::Object;

#[derive(Clone)]
pub enum Backend {
    File(PathBuf, u32, u32),
    S3(super::s3::S3),
    Gateway(super::gateway::Gateway),
    Prefix(Box<Backend>, String),
    Epoch(Box<EpochBackend>),
}

#[derive(Clone)]
pub struct EpochBackend {
    pub root: Backend,
    pub current: Backend,
    pub history: Vec<(Backend, String)>,
}
fn epoch_key(key: &str) -> std::borrow::Cow<'_, str> {
    if let Some(id) = key.strip_prefix("requests/claims/") {
        format!("claims/{id}").into()
    } else {
        key.into()
    }
}
fn epoch_object(key: &str) -> bool {
    key == "store.json"
        || key == "public.json"
        || ["records/", "states/", "files/", "slots/", "history/", "index/", "requests/", "claims/"]
            .iter()
            .any(|p| key.starts_with(p))
}
impl Backend {
    pub fn set_epoch(&self, epoch: Option<&str>) -> Result<()> {
        match self {
            Self::Gateway(g) => g.set_epoch(epoch),
            Self::Prefix(base, _) => base.set_epoch(epoch),
            Self::Epoch(e) => {
                e.root.set_epoch(epoch)?;
                e.current.set_epoch(epoch)?;
                for (backend, _) in &e.history {
                    backend.set_epoch(epoch)?;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }
    pub fn prove_reader(&self) -> Result<()> {
        match self {
            Self::Gateway(g) => g.prove_reader(),
            Self::Prefix(base, _) => base.prove_reader(),
            Self::Epoch(epoch) => epoch.current.prove_reader(),
            _ => Ok(()),
        }
    }

    pub fn root(&self) -> &Backend {
        if let Self::Epoch(epoch) = self {
            &epoch.root
        } else {
            self
        }
    }
    fn namespace_epoch(&self) -> &str {
        if let Self::Prefix(_, scope) = self {
            if let Some((epoch, _)) = scope.strip_prefix("project/epochs/").and_then(|s| s.split_once('/')) {
                return epoch;
            }
        }
        "root"
    }
    /// Return the object's actual namespace and its key, binding unsigned outer routing
    /// fields to authenticated epoch discovery before the request parser runs.
    pub fn request_object(&self, key: &str, identity: &str) -> Result<(Vec<u8>, String, String)> {
        if let Self::Epoch(epoch) = self {
            if let Some(bytes) = epoch.current.get(key)? {
                return Ok((bytes, epoch.current.namespace_epoch().into(), identity.into()));
            }
            for (backend, old_identity) in &epoch.history {
                if let Some(bytes) = backend.get(key)? {
                    return Ok((bytes, backend.namespace_epoch().into(), old_identity.clone()));
                }
            }
            bail!("request disappeared");
        }
        Ok((self.get(key)?.context("request disappeared")?, self.namespace_epoch().into(), identity.into()))
    }
    pub fn decrypted(&self, key: &str, identity: &str) -> Result<Vec<u8>> {
        if let Self::Epoch(epoch) = self {
            if key.starts_with("states/") && epoch.current.get(key)?.is_none() {
                for (backend, old_identity) in &epoch.history {
                    if let Some(bytes) = backend.get(key)? {
                        return super::keys::decrypt(old_identity, &bytes);
                    }
                }
            }
        }
        let bytes = self.get(key)?.with_context(|| format!("missing remote object {key}"))?;
        super::keys::decrypt(identity, &bytes)
    }

    /// Stream a release object and select the authenticated identity of its epoch.
    pub fn release_stream(&self, key: &str, identity: &str) -> Result<(Box<dyn Read>, String)> {
        if !super::format::release_file(key) {
            bail!("invalid release object");
        }
        if let Self::Epoch(epoch) = self {
            if let Some(stream) = epoch.current.get_stream(key)? {
                return Ok((stream, identity.into()));
            }
            for (backend, old_identity) in &epoch.history {
                if let Some(stream) = backend.get_stream(key)? {
                    return Ok((stream, old_identity.clone()));
                }
            }
            bail!("missing release object {key}");
        }
        Ok((self.get_stream(key)?.with_context(|| format!("missing release object {key}"))?, identity.into()))
    }

    fn get_stream(&self, key: &str) -> Result<Option<Box<dyn Read>>> {
        check_key(key)?;
        match self {
            Self::Epoch(_) => bail!("select an epoch before streaming"),
            Self::Prefix(base, prefix) => base.get_stream(&format!("{prefix}{key}")),
            Self::S3(s) => Ok(s.get_stream(key)?.map(|r| Box::new(r) as Box<dyn Read>)),
            Self::Gateway(g) => Ok(g.release_stream(key)?.map(|r| Box::new(r) as Box<dyn Read>)),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                let path = crate::fsutil::safe_join(root, key)?;
                match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path) {
                    Ok(file) => {
                        if !file.metadata()?.is_file() {
                            bail!("remote object is not a file");
                        }
                        Ok(Some(Box::new(file) as Box<dyn Read>))
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
                    Err(e) => Err(e.into()),
                }
            }),
        }
    }

    pub fn put_file_new(&self, key: &str, mut source: fs::File) -> Result<bool> {
        check_key(key)?;
        match self {
            Self::Epoch(epoch) => epoch.current.put_file_new(key, source),
            Self::Prefix(base, prefix) => base.put_file_new(&format!("{prefix}{key}"), source),
            Self::S3(s) => s.put_file_new(key, source),
            Self::Gateway(g) => g.put_file_new(key, source),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                fs::create_dir_all(root)?;
                let path = crate::fsutil::safe_join(root, key)?;
                let parent = path.parent().context("object parent")?;
                fs::create_dir_all(parent)?;
                crate::fsutil::safe_join(root, key)?;
                let temporary = parent.join(format!(".put-{}", crate::ids::new_id()));
                let result = (|| {
                    let mut out = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
                    std::io::copy(&mut source, &mut out)?;
                    out.sync_all()?;
                    match fs::hard_link(&temporary, &path) {
                        Ok(()) => {
                            fs::File::open(parent)?.sync_all()?;
                            Ok(true)
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                        Err(e) => Err(e.into()),
                    }
                })();
                let _ = fs::remove_file(temporary);
                result
            }),
        }
    }

    pub fn scoped(&self, prefix: &str) -> Result<Self> {
        check_key(prefix)?;
        Ok(Self::Prefix(Box::new(self.clone()), format!("{prefix}/")))
    }
    pub fn file(root: PathBuf) -> Self {
        Self::File(root, rustix::process::geteuid().as_raw(), rustix::process::getegid().as_raw())
    }
    pub fn as_caller(&mut self, ctx: &crate::ctx::Ctx) {
        if let Self::File(_, uid, gid) = self {
            *uid = ctx.caller.uid;
            *gid = ctx.caller.gid;
        }
    }

    pub fn open(url: &str) -> Result<Self> {
        if url.starts_with("layr+https://") {
            return Ok(Self::Gateway(super::gateway::Gateway::open(url)?));
        }
        if url.starts_with("file://") {
            let parsed = reqwest::Url::parse(url).context("invalid file remote URL")?;
            if parsed.query().is_some() || parsed.fragment().is_some() {
                bail!("a file remote cannot have a query or fragment");
            }
            let path = parsed.to_file_path().map_err(|_| anyhow::anyhow!("a file remote needs an absolute local path"))?;
            return Ok(Self::file(path));
        }
        if let Some(path) = url.strip_prefix("s3://") {
            let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
            if bucket.is_empty() || bucket.contains(['@', ':', '?', '#']) || prefix.contains(['?', '#']) {
                bail!("invalid S3 bucket");
            }
            return Ok(Self::S3(super::s3::S3::unconnected(bucket, prefix)?));
        }
        bail!("store URL must start with file://, s3:// or layr+https://")
    }

    /// Read-only clients use caller environment without a local project or service context.
    pub fn for_reader(url: &str, env: &std::collections::HashMap<String, String>) -> Result<Self> {
        let mut backend = Self::open(url)?;
        match &mut backend {
            Self::S3(s3) => {
                let path = url.strip_prefix("s3://").expect("S3 URL checked");
                let (bucket, prefix) = path.split_once('/').unwrap_or((path, ""));
                *s3 = super::s3::S3::with_env(bucket, prefix, env)?;
            }
            Self::Gateway(gateway) => gateway.connect_reader(env)?,
            Self::File(..) | Self::Prefix(..) | Self::Epoch(..) => {}
        }
        Ok(backend)
    }

    pub fn unlock_reader(&self, identity: &str) -> Result<()> {
        match self {
            Self::Epoch(epoch) => {
                epoch.root.unlock_reader(identity)?;
                epoch.current.unlock_reader(identity)?;
                for (backend, _) in &epoch.history {
                    backend.unlock_reader(identity)?;
                }
                Ok(())
            }
            Self::Prefix(base, _) => base.unlock_reader(identity),
            Self::Gateway(gateway) => gateway.set_identity(identity),
            Self::File(..) | Self::S3(_) => Ok(()),
        }
    }

    pub fn can_read(&self) -> bool {
        match self {
            Self::Gateway(gateway) => gateway.has_read_identity(),
            Self::Epoch(epoch) => epoch.root.can_read(),
            Self::Prefix(base, _) => base.can_read(),
            _ => true,
        }
    }

    pub(super) fn public_manifest(&self) -> Result<Option<Vec<u8>>> {
        match self {
            Self::Gateway(gateway) => gateway.public_get("public.json"),
            _ => self.get("public.json"),
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        match self {
            Self::Epoch(epoch) => {
                if !epoch_object(key) {
                    return epoch.root.get(key);
                }
                let data = epoch.current.get(&epoch_key(key))?;
                if data.is_some()
                    || !(key.starts_with("states/")
                        || key.starts_with("files/")
                        || key.starts_with("requests/")
                        || key.starts_with("claims/"))
                {
                    return Ok(data);
                }
                for (backend, _) in &epoch.history {
                    if let Some(bytes) = backend.get(key)? {
                        return Ok(Some(bytes));
                    }
                }
                Ok(None)
            }
            Self::Prefix(base, prefix) => base.get(&format!("{prefix}{key}")),
            Self::S3(s) => s.get(key),
            Self::Gateway(g) => g.get(key),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                let path = crate::fsutil::safe_join(root, key)?;
                let mut f = match OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
                    Ok(f) => f,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    Err(e) => return Err(e).with_context(|| format!("read remote object {key}")),
                };
                if !f.metadata()?.is_file() {
                    bail!("remote object is not a file: {key}");
                }
                let mut data = Vec::new();
                f.read_to_end(&mut data)?;
                Ok(Some(data))
            }),
        }
    }

    /// Publish a whole object or leave its destination absent. Never replace an object.
    pub fn put_new(&self, key: &str, body: Vec<u8>) -> Result<bool> {
        check_key(key)?;
        match self {
            Self::Epoch(epoch) => {
                if epoch_object(key) {
                    epoch.current.put_new(&epoch_key(key), body)
                } else {
                    epoch.root.put_new(key, body)
                }
            }
            Self::Prefix(base, prefix) => base.put_new(&format!("{prefix}{key}"), body),
            Self::S3(s) => s.put_new(key, body),
            Self::Gateway(g) => g.put_new(key, body),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                fs::create_dir_all(root)?;
                let path = crate::fsutil::safe_join(root, key)?;
                let parent = path.parent().expect("object parent");
                fs::create_dir_all(parent)?;
                crate::fsutil::safe_join(root, key)?;
                let temporary = parent.join(format!(".put-{}", crate::ids::new_id()));
                let mut f = OpenOptions::new().write(true).create_new(true).mode(0o600).open(&temporary)?;
                let result = (|| -> Result<bool> {
                    f.write_all(&body)?;
                    f.sync_all()?;
                    match fs::hard_link(&temporary, &path) {
                        Ok(()) => {
                            fs::File::open(parent)?.sync_all()?;
                            Ok(true)
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Ok(false),
                        Err(e) => Err(e.into()),
                    }
                })();
                let _ = fs::remove_file(temporary);
                result
            }),
        }
    }

    pub fn list(&self, prefix: &str) -> Result<Vec<Object>> {
        if !prefix.is_empty() {
            check_key(prefix.trim_end_matches('/'))?;
        }
        match self {
            Self::Epoch(epoch) => {
                if !epoch_object(prefix) {
                    return epoch.root.list(prefix);
                }
                let mut objects: std::collections::BTreeMap<String, Object> = std::collections::BTreeMap::new();
                if prefix.starts_with("states/")
                    || prefix.starts_with("files/")
                    || prefix.starts_with("requests/")
                    || prefix.starts_with("claims/")
                {
                    for (backend, _) in epoch.history.iter().rev() {
                        for object in backend.list(prefix)? {
                            objects.insert(object.key.clone(), object);
                        }
                    }
                }
                for object in epoch.current.list(prefix)? {
                    objects.insert(object.key.clone(), object);
                }
                Ok(objects.into_values().collect())
            }
            Self::Prefix(base, scope) => base
                .list(&format!("{scope}{prefix}"))?
                .into_iter()
                .map(|mut object| {
                    object.key = object.key.strip_prefix(scope).context("object is outside its namespace")?.to_string();
                    Ok(object)
                })
                .collect(),
            Self::S3(s) => s.list(prefix),
            Self::Gateway(g) => g.list(prefix),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                let mut out = Vec::new();
                if root.exists() {
                    // A state prefix must not enumerate every sibling state directory.
                    let directory = prefix.rsplit_once('/').map(|(parent, _)| parent).unwrap_or("");
                    let start = if directory.is_empty() { root.clone() } else { crate::fsutil::safe_join(root, directory)? };
                    match fs::symlink_metadata(&start) {
                        Ok(meta) if meta.file_type().is_symlink() => bail!("symbolic link in remote store: {directory}"),
                        Ok(meta) if meta.is_dir() => walk(root, &start, prefix, &mut out)?,
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(error.into()),
                    }
                }
                out.sort_by(|a, b| a.key.cmp(&b.key));
                Ok(out)
            }),
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        match self {
            Self::Epoch(epoch) => {
                if key.starts_with("files/") {
                    epoch.current.delete(key)?;
                    for (backend, _) in &epoch.history {
                        backend.delete(key)?;
                    }
                    Ok(())
                } else if epoch_object(key) {
                    epoch.current.delete(&epoch_key(key))
                } else {
                    epoch.root.delete(key)
                }
            }
            Self::Prefix(base, prefix) => base.delete(&format!("{prefix}{key}")),
            Self::S3(s) => s.delete(key),
            Self::Gateway(g) => g.delete(key),
            Self::File(root, uid, gid) => crate::privs::as_user_fs(*uid, *gid, || {
                let path = crate::fsutil::safe_join(root, key)?;
                match fs::remove_file(path) {
                    Ok(()) => Ok(()),
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
                    Err(e) => Err(e.into()),
                }
            }),
        }
    }
}

fn walk(root: &Path, dir: &Path, prefix: &str, out: &mut Vec<Object>) -> Result<()> {
    for entry in fs::read_dir(dir)? {
        walk_entry(root, entry?, prefix, out)?;
    }
    Ok(())
}

fn walk_entry(root: &Path, entry: fs::DirEntry, prefix: &str, out: &mut Vec<Object>) -> Result<()> {
    let path = entry.path();
    let name = path.strip_prefix(root)?.to_str().context("non-UTF-8 remote object")?.to_string();
    let temporary = entry.file_name().to_string_lossy().starts_with(".put-");
    let m = match fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        // Publication unlinks its internal temporary file after creating the final
        // hard link. A listing may have already read that directory entry.
        Err(error) if temporary && error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    if m.file_type().is_symlink() {
        bail!("symbolic link in remote store: {name}");
    }
    if m.is_dir() {
        if prefix.starts_with(&format!("{name}/")) || name.starts_with(prefix) {
            walk(root, &path, prefix, out)?;
        }
    } else if name.starts_with(prefix) && !temporary {
        let modified = m.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
        out.push(Object { key: name, modified });
    }
    Ok(())
}

pub(super) fn check_key(key: &str) -> Result<()> {
    if key.is_empty() || key.split('/').any(|p| p.is_empty() || p == "." || p == "..") || key.contains(['\\', '\0']) {
        bail!("invalid remote object key: {key:?}");
    }
    crate::fsutil::check_rel(key)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn reader_hooks_use_s3_caller_credentials_and_leave_file_reads_local() {
        use std::net::TcpListener;
        use std::sync::mpsc;
        use std::time::{Duration, Instant};
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let deadline = Instant::now() + Duration::from_secs(5);
            while Instant::now() < deadline {
                if let Ok((mut stream, _)) = listener.accept() {
                    stream.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
                    let mut bytes = Vec::new();
                    let mut byte = [0];
                    while !bytes.ends_with(b"\r\n\r\n") && bytes.len() < 8192 {
                        if stream.read_exact(&mut byte).is_err() {
                            break;
                        }
                        bytes.push(byte[0]);
                    }
                    let request = String::from_utf8_lossy(&bytes).to_ascii_lowercase();
                    let valid = request.contains("credential=disk-reader-test/")
                        && request.contains("x-amz-security-token: reader-session-marker");
                    let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
                    let _ = tx.send(valid);
                    return;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let env = std::collections::HashMap::from([
            ("AWS_ACCESS_KEY_ID".into(), "disk-reader-test".into()),
            ("AWS_SECRET_ACCESS_KEY".into(), "reader-secret-marker".into()),
            ("AWS_SESSION_TOKEN".into(), "reader-session-marker".into()),
            ("AWS_ENDPOINT_URL_S3".into(), endpoint),
        ]);
        let backend = Backend::for_reader("s3://reader-test/prefix", &env).unwrap();
        backend.unlock_reader("file and S3 need no reader identity").unwrap();
        assert!(backend.get("store.json").unwrap().is_none());
        assert!(rx.recv_timeout(Duration::from_secs(5)).unwrap());
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        fs::create_dir(&dir).unwrap();
        fs::write(dir.join("store.json"), b"local").unwrap();
        let backend = Backend::for_reader(&format!("file://{}", dir.display()), &env).unwrap();
        backend.unlock_reader("not used").unwrap();
        assert_eq!(backend.get("store.json").unwrap().unwrap(), b"local");
        fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn scoped_objects_use_the_same_exclusive_publication_and_stay_in_their_namespace() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let backend = Backend::file(root.clone());
        let overlay = backend.scoped("overlays/one").unwrap();
        assert!(overlay.put_new("states/a", b"overlay".to_vec()).unwrap());
        assert!(!overlay.put_new("states/a", b"replacement".to_vec()).unwrap());
        assert!(backend.put_new("states/a", b"shared".to_vec()).unwrap());
        assert_eq!(overlay.get("states/a").unwrap().unwrap(), b"overlay");
        assert_eq!(backend.get("states/a").unwrap().unwrap(), b"shared");
        assert_eq!(overlay.list("states/").unwrap()[0].key, "states/a");
        assert_eq!(overlay.list("").unwrap().len(), 1);
        assert!(backend.scoped("../bad").is_err());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn listing_tolerates_only_a_vanished_publication_temporary() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        fs::create_dir(&root).unwrap();
        let mut objects = Vec::new();
        for (name, tolerated) in [(".put-published", true), ("published-object", false)] {
            let path = root.join(name);
            fs::write(&path, b"publication bytes").unwrap();
            let entry = fs::read_dir(&root).unwrap().next().unwrap().unwrap();
            // Force the interval between read_dir and stat: the uploader removes
            // its temporary name once the public hard link exists.
            fs::remove_file(&path).unwrap();
            assert_eq!(walk_entry(&root, entry, "", &mut objects).is_ok(), tolerated);
        }
        for name in ["public-link", ".put-link"] {
            let path = root.join(name);
            std::os::unix::fs::symlink("missing-target", &path).unwrap();
            assert!(Backend::file(root.clone()).list("").is_err());
            fs::remove_file(path).unwrap();
        }
        assert!(objects.is_empty());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn file_listing_visits_only_the_requested_directory_and_refuses_its_links() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let backend = Backend::file(root.clone());
        backend.put_new("states/one/a.age", b"one".to_vec()).unwrap();
        backend.put_new("states/two/b.age", b"two".to_vec()).unwrap();
        // This sibling must not be inspected for an exact state-directory prefix.
        std::os::unix::fs::symlink("/missing", root.join("states/two/link")).unwrap();
        let files = backend.list("states/one/").unwrap();
        assert_eq!(files.len(), 1);
        assert_eq!(files[0].key, "states/one/a.age");
        assert_eq!(backend.list("states/one/a").unwrap().len(), 1);
        assert!(backend.list("states/missing/").unwrap().is_empty());
        assert!(backend.list("states/two/").is_err());
        std::os::unix::fs::symlink("one", root.join("states/link")).unwrap();
        assert!(backend.list("states/link/").is_err());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn publication_is_exclusive_and_never_changes_bytes() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let s = Backend::file(root.clone());
        assert!(s.put_new("states/a", b"first".to_vec()).unwrap());
        assert!(!s.put_new("states/a", b"second".to_vec()).unwrap());
        assert_eq!(s.get("states/a").unwrap().unwrap(), b"first");
        assert_eq!(s.list("states/").unwrap().len(), 1);
        assert!(s.put_new("../bad", vec![]).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}

#[cfg(test)]
mod epoch_tests {
    use super::*;
    #[test]
    fn historical_states_use_sealed_history_keys_but_slots_never_fall_back() {
        let path = std::env::temp_dir().join(crate::ids::new_id());
        let root = Backend::file(path.clone());
        let current = root.scoped("project/epochs/new").unwrap();
        let old = super::super::keys::generate_identity();
        let new = super::super::keys::generate_identity();
        root.put_new(
            "states/old",
            super::super::keys::encrypt(&super::super::keys::recipient_of(&old).unwrap(), b"old bytes").unwrap(),
        )
        .unwrap();
        root.put_new("slots/removed.age", b"old slot".to_vec()).unwrap();
        let backend = Backend::Epoch(Box::new(EpochBackend {
            root: root.clone(),
            current: current.clone(),
            history: vec![(root.clone(), old)],
        }));
        assert_eq!(backend.decrypted("states/old", &new).unwrap(), b"old bytes");
        assert!(backend.get("slots/removed.age").unwrap().is_none());
        backend.put_new("requests/one.json", b"sealed request".to_vec()).unwrap();
        backend.put_new("requests/claims/one.json", b"claim".to_vec()).unwrap();
        assert_eq!(current.get("requests/one.json").unwrap().unwrap(), b"sealed request");
        assert_eq!(current.get("claims/one.json").unwrap().unwrap(), b"claim");
        assert!(root.get("requests/one.json").unwrap().is_none());
        std::fs::remove_dir_all(path).unwrap();
    }
}
