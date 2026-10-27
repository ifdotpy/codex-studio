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
}

impl Backend {
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
            Self::File(..) | Self::Prefix(..) => {}
        }
        Ok(backend)
    }

    pub fn unlock_reader(&self, identity: &str) -> Result<()> {
        match self {
            Self::Prefix(base, _) => base.unlock_reader(identity),
            Self::Gateway(gateway) => gateway.set_identity(identity),
            Self::File(..) | Self::S3(_) => Ok(()),
        }
    }

    pub fn can_read(&self) -> bool {
        match self {
            Self::Gateway(gateway) => gateway.has_read_identity(),
            Self::Prefix(base, _) => base.can_read(),
            _ => true,
        }
    }

    pub fn get(&self, key: &str) -> Result<Option<Vec<u8>>> {
        check_key(key)?;
        match self {
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
                    walk(root, root, prefix, &mut out)?;
                }
                out.sort_by(|a, b| a.key.cmp(&b.key));
                Ok(out)
            }),
        }
    }

    pub fn delete(&self, key: &str) -> Result<()> {
        check_key(key)?;
        match self {
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
    for e in fs::read_dir(dir)? {
        let e = e?;
        let path = e.path();
        let name = path.strip_prefix(root)?.to_str().context("non-UTF-8 remote object")?.to_string();
        let m = fs::symlink_metadata(&path)?;
        if m.file_type().is_symlink() {
            bail!("symbolic link in remote store: {name}");
        }
        if m.is_dir() {
            if prefix.starts_with(&format!("{name}/")) || name.starts_with(prefix) {
                walk(root, &path, prefix, out)?;
            }
        } else if name.starts_with(prefix) && !e.file_name().to_string_lossy().starts_with(".put-") {
            let modified = m.modified()?.duration_since(std::time::UNIX_EPOCH)?.as_secs() as i64;
            out.push(Object { key: name, modified });
        }
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
