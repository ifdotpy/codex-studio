//! OverlayFS trees for a Linux VM. The private registry restores mounts after reboot.
//! Writable branches share an immutable lower tree. A snapshot of a writable tree
//! copies its changed upper layer. Saved layers are immutable and shared by branches.
use crate::sys;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::{BTreeMap, HashMap, HashSet},
    fs::{self, File},
    io,
    os::{
        fd::AsRawFd,
        unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt},
    },
    path::{Path, PathBuf},
    process::Command,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubvolInfo {
    pub uuid: String,
    pub received_uuid: Option<String>,
    pub parent_uuid: Option<String>,
    pub readonly: bool,
}
#[derive(Clone, Serialize, Deserialize)]
struct Entry {
    info: SubvolInfo,
    // Older binaries require `lower`; omit it so they reject this registry
    // instead of ignoring delta layers or remounting an ordinary cache.
    #[serde(rename = "base", alias = "lower")]
    lower: String,
    #[serde(default)]
    hard_links: bool,
    #[serde(default)]
    bases: Vec<String>,
    #[serde(default)]
    plain: bool,
    #[serde(default)]
    restore_cache: bool,
}
#[derive(Default, Serialize, Deserialize)]
struct Registry {
    trees: BTreeMap<PathBuf, Entry>,
    #[serde(default)]
    pending_move: Option<(PathBuf, PathBuf)>,
}
struct Locked {
    home: PathBuf,
    _lock: File,
    registry: Registry,
}
fn absolute(path: &Path) -> Result<PathBuf> {
    let path = if path.is_absolute() { path.to_path_buf() } else { std::env::current_dir()?.join(path) };
    if path.components().any(|c| matches!(c, std::path::Component::ParentDir)) {
        bail!("parent component in managed path");
    }
    Ok(path.components().collect())
}
fn private_dir(path: &Path) -> Result<()> {
    if !path.exists() {
        fs::DirBuilder::new().mode(0o700).create(path)?;
    }
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.uid() != 0 || m.mode() & 0o077 != 0 {
        bail!("unsafe OverlayFS private directory {}", path.display());
    }
    Ok(())
}
fn lock_at(home: PathBuf) -> Result<Locked> {
    let lock = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(home.join("lock"))?;
    rustix::fs::flock(&lock, rustix::fs::FlockOperation::LockExclusive)?;
    let registry = match fs::read(home.join("registry.json")) {
        Ok(data) => serde_json::from_slice(&data)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if home.parent().is_some_and(|p| p.join("machine/key").exists()) {
                bail!("OverlayFS registry is missing; restore the store before use");
            }
            Registry::default()
        }
        Err(e) => return Err(e.into()),
    };
    Ok(Locked { home, _lock: lock, registry })
}
fn lock(path: &Path) -> Result<Locked> {
    for parent in absolute(path)?.ancestors() {
        let home = parent.join(".layr-overlayfs");
        if home.is_dir() {
            private_dir(&home)?;
            return lock_at(home);
        }
    }
    bail!("no OverlayFS registry for {}", path.display())
}
impl Locked {
    fn save(&self) -> Result<()> {
        File::open(self.home.join("objects"))?.sync_all()?;
        crate::fsutil::atomic_write(&self.home.join("registry.json"), &serde_json::to_vec(&self.registry)?, 0o600)
    }
    fn object(&self, id: &str) -> PathBuf {
        self.home.join("objects").join(id)
    }
    fn new_object(&self) -> Result<String> {
        let id = crate::ids::new_id();
        private_dir(&self.object(&id))?;
        Ok(id)
    }
    fn mount(&self, path: &Path, e: &Entry) -> Result<()> {
        if e.plain {
            return Ok(());
        }
        let data = self.object(&e.lower).join("data");
        let lowers = std::iter::once(&e.lower)
            .chain(e.bases.iter())
            .map(|id| self.object(id).join("data").display().to_string())
            .collect::<Vec<_>>()
            .join(":");
        if e.info.readonly && !e.bases.is_empty() {
            let opts = format!("lowerdir={lowers},redirect_dir=nofollow,index=off");
            sys::overlay_mount(None, path, Some(c"overlay"), libc::MS_RDONLY, Some(&opts))?;
        } else if e.info.readonly {
            sys::overlay_mount(Some(&data), path, None, libc::MS_BIND, None)?;
            if let Err(err) = sys::overlay_mount(None, path, None, libc::MS_BIND | libc::MS_REMOUNT | libc::MS_RDONLY, None) {
                let _ = sys::overlay_unmount(path);
                return Err(err.into());
            }
        } else {
            let object = self.object(&e.info.uuid);
            let upper = object.join("upper");
            let work = object.join("work");
            if !upper.exists() {
                if e.hard_links {
                    let temp = object.join("upper-pending");
                    if temp.exists() {
                        fs::remove_dir_all(&temp)?;
                    }
                    copy_tree(&data, &temp, &HashSet::new(), &mut HashMap::new())?;
                    sync_tree(&temp)?;
                    fs::rename(&temp, &upper)?;
                    File::open(&object)?.sync_all()?;
                } else {
                    fs::create_dir(&upper)?;
                }
            }
            if !work.exists() {
                fs::create_dir(&work)?;
            }
            let opts = format!(
                "lowerdir={},upperdir={},workdir={},index=on,redirect_dir=on,metacopy=off",
                lowers,
                upper.display(),
                work.display()
            );
            sys::overlay_mount(None, path, Some(c"overlay"), 0, Some(&opts))
                .with_context(|| format!("mount OverlayFS {}", path.display()))?;
        }
        Ok(())
    }
    fn collect(&self) -> Result<()> {
        let used: HashSet<_> = self
            .registry
            .trees
            .values()
            .filter(|e| !e.plain || e.restore_cache)
            .flat_map(|e| std::iter::once(&e.lower).chain(std::iter::once(&e.info.uuid)).chain(e.bases.iter()))
            .collect();
        for e in fs::read_dir(self.home.join("objects"))? {
            let e = e?;
            let name = e.file_name().to_string_lossy().into_owned();
            if !used.contains(&name) {
                fs::remove_dir_all(e.path())?;
            }
        }
        Ok(())
    }
}
fn mounted(path: &Path) -> Result<bool> {
    // Managed roots reject whitespace and mount-option separators on creation.
    let name = path.to_str().context("non-UTF8 mountpoint")?;
    Ok(fs::read_to_string("/proc/self/mountinfo")?.lines().any(|l| l.split_whitespace().nth(4) == Some(name)))
}
fn valid_path(path: &Path) -> Result<()> {
    let s = path.to_str().context("non-UTF8 OverlayFS path")?;
    if s.chars().any(|c| c.is_whitespace() || ",:\\".contains(c)) {
        bail!("OverlayFS store paths cannot contain whitespace, commas, colons, or backslashes");
    }
    // Do not resolve mount paths through a caller-controlled symlink.
    for p in path.ancestors() {
        if fs::symlink_metadata(p).is_ok_and(|m| m.file_type().is_symlink()) {
            bail!("symlink in OverlayFS path");
        }
    }
    Ok(())
}
fn shared_parent(path: &Path, mounts: &str) -> bool {
    mounts
        .lines()
        .filter_map(|line| {
            let fields: Vec<_> = line.split_whitespace().collect();
            let mount = Path::new(*fields.get(4)?);
            if !path.starts_with(mount) {
                return None;
            }
            let shared = fields.iter().skip(6).take_while(|f| **f != "-").any(|f| f.starts_with("shared:"));
            Some((mount.components().count(), shared))
        })
        .max_by_key(|(depth, _)| *depth)
        .is_some_and(|(_, shared)| shared)
}
pub fn recover(root: &Path) -> Result<()> {
    if !rustix::process::geteuid().is_root() {
        bail!("OverlayFS requires the root service or a root command");
    }
    let root = absolute(root)?;
    valid_path(&root)?;
    // MS_MOVE cannot publish a received tree below a shared parent mount.
    // Refuse this setup before creating a store or changing its registry.
    let mounts = fs::read_to_string("/proc/self/mountinfo")?;
    if shared_parent(&root, &mounts) {
        bail!(
            "OverlayFS store requires a private mount; bind-mount the store onto itself and run mount --make-private on that mount"
        );
    }
    fs::create_dir_all(&root)?;
    let home = root.join(".layr-overlayfs");
    if !home.exists() && root.join("machine/key").exists() {
        bail!("this store uses another backend; restore a backup into a new OverlayFS store");
    }
    private_dir(&home)?;
    private_dir(&home.join("objects"))?;
    let mut state = lock_at(home)?;
    if !state.home.join("registry.json").exists() {
        state.save()?;
    }
    if let Some((from, to)) = state.registry.pending_move.clone() {
        if !from.starts_with(&root) || !to.starts_with(&root) {
            bail!("rename outside OverlayFS store");
        }
        valid_path(&from)?;
        valid_path(&to)?;
        if mounted(&from)? {
            sys::overlay_mount(Some(&from), &to, None, libc::MS_MOVE, None)?;
        } else if !state.registry.trees.get(&from).is_some_and(|e| !e.plain) && from.exists() {
            if to.exists() {
                bail!("both paths exist during OverlayFS rename recovery");
            }
            fs::rename(&from, &to)?;
        }
        let paths: Vec<_> = state.registry.trees.keys().filter(|p| p.starts_with(&from)).cloned().collect();
        for old in paths {
            let new = absolute(&to.join(old.strip_prefix(&from)?))?;
            let e = state.registry.trees.remove(&old).unwrap();
            state.registry.trees.insert(new, e);
        }
        state.registry.pending_move = None;
        state.save()?;
        if from.exists() {
            fs::remove_dir(&from)?;
        }
    }
    for (path, e) in state.registry.trees.clone().iter() {
        if !path.starts_with(&root) {
            bail!("OverlayFS mount outside its store");
        }
        valid_path(path)?;
        if e.restore_cache {
            finish_cache_restore(&mut state, path, e)?;
        }
        if e.plain {
            continue;
        }
        if !mounted(path)? {
            fs::create_dir_all(path)?;
            state.mount(path, e)?;
        }
    }
    // Upgrade old nested cache mounts after their parent mounts exist.
    let caches: Vec<_> = state
        .registry
        .trees
        .iter()
        .filter(|(p, e)| {
            !e.info.readonly
                && !e.plain
                && state
                    .registry
                    .trees
                    .iter()
                    .any(|(parent, pe)| *p != parent && p.starts_with(parent) && !pe.info.readonly && !pe.plain)
        })
        .map(|(p, e)| (p.clone(), e.clone()))
        .collect();
    for (path, mut e) in caches {
        let id = state.new_object()?;
        // Freeze the old cache before its durable migration record. Open
        // writers make this fail rather than lose writes after the copy.
        sys::overlay_mount(None, &path, None, libc::MS_REMOUNT | libc::MS_RDONLY, None)
            .context("close writers in the cache before its migration")?;
        let copied = (|| -> Result<()> {
            copy_stable(&path, &state.object(&id).join("data"), &HashSet::new())?;
            sync_tree(&state.object(&id).join("data"))
        })();
        if let Err(error) = copied {
            sys::overlay_mount(None, &path, None, libc::MS_REMOUNT, None)?;
            return Err(error);
        }
        e.lower = id;
        e.bases.clear();
        e.plain = true;
        e.restore_cache = true;
        state.registry.trees.insert(path.clone(), e.clone());
        state.save()?;
        finish_cache_restore(&mut state, &path, &e)?;
    }
    Ok(())
}
fn finish_cache_restore(state: &mut Locked, path: &Path, entry: &Entry) -> Result<()> {
    if mounted(path)? {
        sys::overlay_unmount(path).context("close files in the cache before its migration")?;
    }
    let parent = pinned_directory(path.parent().context("cache parent")?)?;
    let name = path.file_name().context("cache name")?;
    let anchored = PathBuf::from(format!("/proc/self/fd/{}", parent.as_raw_fd()));
    if sys::overlay_attribute(&anchored.join(name), c"trusted.layr.cache-restore", None).ok().as_deref()
        != Some(entry.lower.as_bytes())
    {
        let temp_name = format!(".layr-cache-restore-{}", entry.lower);
        let temp = anchored.join(&temp_name);
        if fs::symlink_metadata(&temp).is_ok() {
            fs::remove_dir_all(&temp)?;
        }
        copy_cache_at(
            &state.object(&entry.lower).join("data"),
            &parent,
            std::ffi::OsStr::new(&temp_name),
            Some(entry.lower.as_bytes()),
        )?;
        // Replaces only an empty mountpoint. Never replace existing user data.
        rustix::fs::renameat(&parent, &temp_name, &parent, name)?;
        parent.sync_all()?;
    }
    state.registry.trees.get_mut(path).context("cache registry entry")?.restore_cache = false;
    state.save()?;
    state.collect()
}
pub fn is_supported(path: &Path) -> bool {
    rustix::process::geteuid().is_root() && lock(path).is_ok()
}
pub fn is_subvolume(path: &Path) -> bool {
    path.is_dir() && subvol_info(path).is_ok()
}
pub fn subvol_info(path: &Path) -> Result<SubvolInfo> {
    let path = absolute(path)?;
    let state = lock(&path)?;
    Ok(state.registry.trees.get(&path).context("not an OverlayFS tree")?.info.clone())
}
pub fn create_subvolume(path: &Path) -> Result<()> {
    let path = absolute(path)?;
    valid_path(&path)?;
    let mut state = lock(&path)?;
    if path.exists() {
        bail!("destination already exists");
    }
    let id = state.new_object()?;
    fs::create_dir(state.object(&id).join("data"))?;
    let entry = Entry {
        info: SubvolInfo { uuid: id.clone(), received_uuid: None, parent_uuid: None, readonly: false },
        lower: id,
        hard_links: false,
        bases: Vec::new(),
        restore_cache: false,
        plain: state.registry.trees.iter().any(|(p, e)| path.starts_with(p) && !e.info.readonly && !e.plain),
    };
    fs::create_dir(&path)?;
    state.registry.trees.insert(path.clone(), entry.clone());
    state.save()?;
    if let Err(e) = state.mount(&path, &entry) {
        state.registry.trees.remove(&path);
        state.save()?;
        fs::remove_dir(&path)?;
        state.collect()?;
        return Err(e);
    }
    Ok(())
}
// A source writer must not silently produce a mixed snapshot.
type Fingerprint = (u64, u64, u32, u64, i64, i64, i64, i64);
fn scan(path: &Path, nested: &HashSet<PathBuf>) -> Result<BTreeMap<PathBuf, Fingerprint>> {
    fn visit(root: &Path, p: &Path, nested: &HashSet<PathBuf>, out: &mut BTreeMap<PathBuf, Fingerprint>) -> Result<()> {
        let m = fs::symlink_metadata(p)?;
        out.insert(
            p.strip_prefix(root)?.to_path_buf(),
            (m.dev(), m.ino(), m.mode(), m.len(), m.mtime(), m.mtime_nsec(), m.ctime(), m.ctime_nsec()),
        );
        if m.is_dir() && !nested.contains(p) {
            for e in fs::read_dir(p)? {
                visit(root, &e?.path(), nested, out)?;
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(path, path, nested, &mut out)?;
    Ok(out)
}
fn copy_tree(src: &Path, dst: &Path, nested: &HashSet<PathBuf>, links: &mut HashMap<(u64, u64), PathBuf>) -> Result<()> {
    copy_tree_at(src, src, dst, nested, links, false)
}
fn copy_tree_at(
    src: &Path,
    logical: &Path,
    dst: &Path,
    nested: &HashSet<PathBuf>,
    links: &mut HashMap<(u64, u64), PathBuf>,
    root_exists: bool,
) -> Result<()> {
    let m = fs::symlink_metadata(src)?;
    if m.is_dir() {
        let dir = fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(src)?;
        let actual = dir.metadata()?;
        if (actual.dev(), actual.ino()) != (m.dev(), m.ino()) {
            bail!("source directory changed during snapshot");
        }
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        if !root_exists {
            fs::create_dir(dst)?;
        }
        if !nested.contains(logical) {
            for e in fs::read_dir(&anchored)? {
                let name = e?.file_name();
                copy_tree_at(&anchored.join(&name), &logical.join(&name), &dst.join(&name), nested, links, false)?;
            }
        }
    } else if m.file_type().is_symlink() {
        std::os::unix::fs::symlink(fs::read_link(src)?, dst)?;
    } else if m.is_file() {
        if let Some(first) = links.get(&(m.dev(), m.ino())) {
            fs::hard_link(first, dst)?;
        } else {
            let s = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(src)?;
            let actual = s.metadata()?;
            if !actual.is_file() || (actual.dev(), actual.ino()) != (m.dev(), m.ino()) {
                bail!("source file changed during snapshot");
            }
            let d = fs::OpenOptions::new().write(true).create_new(true).open(dst)?;
            if clone_file(&s, &d).is_err() {
                io::copy(&mut &s, &mut &d)?;
            }
            links.insert((m.dev(), m.ino()), dst.to_path_buf());
        }
    } else {
        bail!("cannot snapshot special file {}", src.display());
    }
    rustix::fs::chownat(
        rustix::fs::CWD,
        dst,
        Some(rustix::fs::Uid::from_raw(m.uid())),
        Some(rustix::fs::Gid::from_raw(m.gid())),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )?;
    if !m.file_type().is_symlink() {
        fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
    }
    for (name, value) in sys::overlay_xattrs(src)? {
        if name.starts_with(b"trusted.overlay.") || name.starts_with(b"user.overlay.") {
            continue;
        }
        sys::overlay_attribute(dst, &std::ffi::CString::new(name)?, Some(&value))?;
    }
    sys::overlay_attribute(dst, c"trusted.layr.origin", Some(&file_identity(src)?))?;
    sys::overlay_mtime(dst, m.mtime(), m.mtime_nsec())?;
    Ok(())
}
// Cache destinations are inside a user's tree. Pin the parent and new root,
// and keep the root private until all children have been copied. A rename or
// symlink replacement cannot redirect privileged writes outside this directory.
fn pinned_directory(path: &Path) -> Result<File> {
    use rustix::fs::{Mode, OFlags, open, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    let mut dir = open("/", flags, Mode::empty())?;
    for component in absolute(path)?.components() {
        if let std::path::Component::Normal(name) = component {
            dir = openat(&dir, name, flags, Mode::empty())?;
        }
    }
    Ok(File::from(dir))
}
fn copy_cache(src: &Path, dst: &Path) -> Result<()> {
    let parent = pinned_directory(dst.parent().context("cache parent")?)?;
    copy_cache_at(src, &parent, dst.file_name().context("cache name")?, None)
}
fn copy_cache_at(src: &Path, parent: &File, name: &std::ffi::OsStr, marker: Option<&[u8]>) -> Result<()> {
    use rustix::fs::{Mode, OFlags, mkdirat, openat};
    let flags = OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC;
    mkdirat(parent, name, Mode::from_raw_mode(0o700))?;
    let root = File::from(openat(parent, name, flags, Mode::empty())?);
    let m = root.metadata()?;
    if m.uid() != 0 || m.mode() & 0o777 != 0o700 {
        bail!("cache destination changed during restore");
    }
    let anchored = PathBuf::from(format!("/proc/self/fd/{}/.", root.as_raw_fd()));
    let before = scan(src, &HashSet::new())?;
    copy_tree_at(src, src, &anchored, &HashSet::new(), &mut HashMap::new(), true)?;
    if before != scan(src, &HashSet::new())? {
        bail!("cache source changed during restore");
    }
    if let Some(value) = marker {
        sys::overlay_attribute(&anchored, c"trusted.layr.cache-restore", Some(value))?;
    }
    sync_tree(&anchored)?;
    let named = rustix::fs::statat(parent, name, rustix::fs::AtFlags::SYMLINK_NOFOLLOW)?;
    if (named.st_dev as u64, named.st_ino as u64) != (m.dev(), m.ino()) {
        bail!("cache destination moved during restore");
    }
    Ok(())
}

// OverlayFS can update directory metadata on the first access. Retry a full
// copy, but publish only a copy whose before and after scans agree.
fn copy_stable(src: &Path, dst: &Path, nested: &HashSet<PathBuf>) -> Result<()> {
    for _ in 0..3 {
        let before = scan(src, nested)?;
        copy_tree(src, dst, nested, &mut HashMap::new())?;
        if before == scan(src, nested)? {
            return Ok(());
        }
        fs::remove_dir_all(dst)?;
    }
    bail!("source changed during OverlayFS snapshot; retry when writers are idle")
}
// Persist the copied tree before publishing its registry entry. One filesystem
// flush covers file data, xattrs and directory entries without one fsync per file.
fn sync_tree(path: &Path) -> Result<()> {
    rustix::fs::syncfs(File::open(path)?)?;
    Ok(())
}
// The live mount and its open file descriptors stay unchanged. Copy only its
// upper layer, then mount that immutable copy above the same immutable bases.
// Compact long chains and hard-linked trees into one portable tree.
fn snapshot_layers(state: &Locked, src: &Path, parent: &Entry, id: &str) -> Result<(String, Vec<String>, bool)> {
    let nested: HashSet<_> = state.registry.trees.keys().filter(|p| **p != src && p.starts_with(src)).cloned().collect();
    let data = state.object(id).join("data");
    if parent.plain {
        // A build can replace a cache directory. Never follow its replacement
        // symlink with the service's credentials.
        let pinned = pinned_directory(src)?;
        let anchored = PathBuf::from(format!("/proc/self/fd/{}/.", pinned.as_raw_fd()));
        let excluded = nested.iter().map(|p| anchored.join(p.strip_prefix(src).unwrap())).collect();
        copy_stable(&anchored, &data, &excluded)?;
        sync_tree(&data)?;
        return Ok((id.into(), Vec::new(), tree_has_links(&data)?));
    }
    let hard_links = tree_has_links_excluding(src, &nested)?;
    let bases: Vec<_> = std::iter::once(parent.lower.clone()).chain(parent.bases.iter().cloned()).collect();
    let option_bytes: usize = bases.iter().map(|id| state.object(id).as_os_str().len() + 6).sum();
    if hard_links || bases.len() >= 16 || option_bytes > 2800 {
        copy_stable(src, &data, &nested)?;
        sync_tree(&data)?;
        return Ok((id.into(), Vec::new(), hard_links));
    }
    let upper = state.object(&parent.info.uuid).join("upper");
    for _ in 0..3 {
        let excluded: HashSet<_> = nested.iter().map(|p| upper.join(p.strip_prefix(src).unwrap())).collect();
        let upper_scan = || -> Result<_> {
            let mut found = scan(&upper, &excluded)?;
            for p in &excluded {
                found.remove(p.strip_prefix(&upper)?);
            }
            Ok(found)
        };
        let before = upper_scan()?;
        copy_delta(&upper, src, &data, &nested, &mut HashMap::new())?;
        // A cache may exist only in a lower layer. Mask it in this snapshot.
        for cache in &nested {
            let rel = cache.strip_prefix(src)?;
            let target = data.join(rel);
            // Validate every parent before writing below a copied directory.
            let mut p = data.clone();
            for component in rel.components() {
                p.push(component);
                match fs::symlink_metadata(&p) {
                    Ok(m) if m.is_dir() => {}
                    Ok(_) => bail!("cache path crosses a non-directory"),
                    Err(e) if e.kind() == io::ErrorKind::NotFound => fs::create_dir(&p)?,
                    Err(e) => return Err(e.into()),
                }
            }
            sys::overlay_attribute(&target, c"trusted.overlay.opaque", Some(b"y"))?;
        }
        if before == upper_scan()? {
            sync_tree(&data)?;
            return Ok((id.into(), bases, false));
        }
        fs::remove_dir_all(&data)?;
    }
    bail!("source changed during OverlayFS snapshot; retry when writers are idle")
}
fn tree_has_links_excluding(path: &Path, nested: &HashSet<PathBuf>) -> Result<bool> {
    if nested.contains(path) {
        return Ok(false);
    }
    let m = fs::symlink_metadata(path)?;
    if m.is_file() {
        return Ok(m.nlink() > 1);
    }
    if m.is_dir() {
        for e in fs::read_dir(path)? {
            if tree_has_links_excluding(&e?.path(), nested)? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
fn copy_delta(
    src: &Path,
    visible: &Path,
    dst: &Path,
    nested: &HashSet<PathBuf>,
    links: &mut HashMap<(u64, u64), PathBuf>,
) -> Result<()> {
    use std::os::unix::fs::FileTypeExt;
    let m = fs::symlink_metadata(src)?;
    let attrs = sys::overlay_xattrs(src)?;
    if nested.contains(visible) {
        fs::create_dir(dst)?;
        sys::overlay_attribute(dst, c"trusted.overlay.opaque", Some(b"y"))?;
        return Ok(());
    }
    if attrs.iter().any(|(name, _)| name == b"trusted.overlay.redirect") {
        // Redirects refer to the old lower namespace. Materialize only this
        // moved directory, with an opaque marker to hide its previous target.
        copy_stable(visible, dst, nested)?;
        sys::overlay_attribute(dst, c"trusted.overlay.opaque", Some(b"y"))?;
        return Ok(());
    }
    if m.is_dir() {
        let dir = fs::OpenOptions::new().read(true).custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW).open(src)?;
        let actual = dir.metadata()?;
        if (actual.dev(), actual.ino()) != (m.dev(), m.ino()) {
            bail!("source directory changed during snapshot");
        }
        let anchored = PathBuf::from(format!("/proc/self/fd/{}", dir.as_raw_fd()));
        fs::create_dir(dst)?;
        for e in fs::read_dir(&anchored)? {
            let name = e?.file_name();
            copy_delta(&anchored.join(&name), &visible.join(&name), &dst.join(&name), nested, links)?;
        }
    } else if m.file_type().is_char_device() && m.rdev() == 0 {
        rustix::fs::mknodat(rustix::fs::CWD, dst, rustix::fs::FileType::CharacterDevice, rustix::fs::Mode::empty(), 0)?;
    } else {
        copy_tree(src, dst, &HashSet::new(), links)?;
    }
    rustix::fs::chownat(
        rustix::fs::CWD,
        dst,
        Some(rustix::fs::Uid::from_raw(m.uid())),
        Some(rustix::fs::Gid::from_raw(m.gid())),
        rustix::fs::AtFlags::SYMLINK_NOFOLLOW,
    )?;
    if !m.file_type().is_symlink() {
        fs::set_permissions(dst, fs::Permissions::from_mode(m.mode() & 0o7777))?;
    }
    for (name, value) in attrs {
        if name.starts_with(b"trusted.overlay.") && name != b"trusted.overlay.opaque" {
            continue;
        }
        sys::overlay_attribute(dst, &std::ffi::CString::new(name)?, Some(&value))?;
    }
    sys::overlay_mtime(dst, m.mtime(), m.mtime_nsec())?;
    Ok(())
}

pub fn snapshot(src: &Path, dst: &Path, readonly: bool) -> Result<()> {
    let src = absolute(src)?;
    let dst = absolute(dst)?;
    valid_path(&dst)?;
    let mut state = lock(&src)?;
    if lock_home(&dst)? != state.home {
        bail!("snapshot must stay in one store");
    }
    if dst.exists() {
        bail!("snapshot destination exists");
    }
    let parent = state.registry.trees.get(&src).context("snapshot source is unmanaged")?.clone();
    let id = state.new_object()?;
    let (lower, bases, hard_links) = if parent.info.readonly {
        (parent.lower.clone(), parent.bases.clone(), parent.hard_links)
    } else {
        match snapshot_layers(&state, &src, &parent, &id) {
            Ok(value) => value,
            Err(error) => {
                state.collect()?;
                return Err(error);
            }
        }
    };
    let plain = !readonly && state.registry.trees.iter().any(|(p, e)| dst.starts_with(p) && !e.info.readonly && !e.plain);
    let entry = Entry {
        info: SubvolInfo { uuid: id, received_uuid: None, parent_uuid: Some(parent.info.uuid), readonly },
        lower,
        hard_links,
        bases,
        plain,
        restore_cache: false,
    };
    if plain {
        // A cache is an ordinary directory. Its saved copy remains private.
        if parent.plain {
            copy_cache(&state.object(&entry.lower).join("data"), &dst)?;
        } else {
            copy_cache(&src, &dst)?;
        }
        sync_tree(&dst)?;
    }
    File::open(state.object(&entry.lower))?.sync_all()?;
    if !plain {
        fs::create_dir(&dst)?;
    }
    state.registry.trees.insert(dst.clone(), entry.clone());
    state.save()?;
    if let Err(e) = state.mount(&dst, &entry) {
        state.registry.trees.remove(&dst);
        state.save()?;
        fs::remove_dir(&dst)?;
        state.collect()?;
        return Err(e);
    }
    Ok(())
}
fn lock_home(path: &Path) -> Result<PathBuf> {
    for p in absolute(path)?.ancestors() {
        if p.join(".layr-overlayfs").is_dir() {
            return Ok(p.join(".layr-overlayfs"));
        }
    }
    bail!("path is outside the store")
}
pub fn set_readonly(path: &Path) -> Result<()> {
    let path = absolute(path)?;
    let mut state = lock(&path)?;
    let mut e = state.registry.trees.get(&path).context("unmanaged tree")?.clone();
    if e.info.readonly {
        return Ok(());
    }
    if e.plain {
        bail!("cannot seal a working cache in place; snapshot it instead");
    }
    let id = state.new_object()?;
    let (lower, bases, hard_links) = match snapshot_layers(&state, &path, &e, &id) {
        Ok(value) => value,
        Err(error) => {
            state.collect()?;
            return Err(error);
        }
    };
    sys::overlay_unmount(&path).with_context(|| format!("unmount {}", path.display()))?;
    e.hard_links = hard_links;
    e.lower = lower;
    e.bases = bases;
    e.info.readonly = true;
    state.registry.trees.insert(path.clone(), e.clone());
    state.save()?;
    state.mount(&path, &e)?;
    // The sealed copy owns the visible data. The old writable upper is unused.
    for name in ["upper", "work"] {
        let old = state.object(&e.info.uuid).join(name);
        if old.exists() {
            fs::remove_dir_all(old)?;
        }
    }
    File::open(state.object(&e.info.uuid))?.sync_all()?;
    state.collect()
}
pub fn set_writable(path: &Path) -> Result<()> {
    let path = absolute(path)?;
    let mut state = lock(&path)?;
    let mut e = state.registry.trees.get(&path).context("unmanaged tree")?.clone();
    if !e.info.readonly {
        return Ok(());
    }
    sys::overlay_unmount(&path).with_context(|| format!("unmount {}", path.display()))?;
    // Use a new upper directory after every read-only transition.
    for name in ["upper", "work"] {
        let p = state.object(&e.info.uuid).join(name);
        if p.exists() {
            fs::remove_dir_all(p)?;
        }
    }
    e.info.readonly = false;
    state.registry.trees.insert(path.clone(), e.clone());
    state.save()?;
    state.mount(&path, &e)
}
pub fn rename_tree(from: &Path, to: &Path) -> Result<()> {
    let from = absolute(from)?;
    let to = absolute(to)?;
    valid_path(&to)?;
    let mut state = lock(&from)?;
    if lock_home(&to)? != state.home {
        bail!("rename must stay in one store");
    }
    let managed_root = state.registry.trees.get(&from).is_some_and(|e| !e.plain);
    if !state.registry.trees.keys().any(|p| p.starts_with(&from)) {
        return Ok(fs::rename(from, to)?);
    }
    if to.exists() {
        bail!("destination already exists");
    }
    valid_path(&from)?;
    if managed_root {
        fs::create_dir(&to)?;
    }
    state.registry.pending_move = Some((from.clone(), to.clone()));
    state.save()?;
    let moved =
        if managed_root { sys::overlay_mount(Some(&from), &to, None, libc::MS_MOVE, None) } else { fs::rename(&from, &to) };
    if let Err(e) = moved {
        state.registry.pending_move = None;
        state.save()?;
        if managed_root {
            fs::remove_dir(&to)?;
        }
        return Err(e.into());
    }
    let moves: Vec<_> = state.registry.trees.keys().filter(|p| p.starts_with(&from)).cloned().collect();
    for old in moves {
        let new = absolute(&to.join(old.strip_prefix(&from)?))?;
        let e = state.registry.trees.remove(&old).unwrap();
        state.registry.trees.insert(new, e);
    }
    state.registry.pending_move = None;
    state.save()?;
    if managed_root {
        fs::remove_dir(from)?;
    }
    Ok(())
}
pub fn delete_tree(path: &Path) -> Result<()> {
    let path = absolute(path)?;
    let mut state = lock(&path)?;
    valid_path(&path)?;
    let mut paths: Vec<_> = state.registry.trees.keys().filter(|p| p.starts_with(&path)).cloned().collect();
    paths.sort_by_key(|p| std::cmp::Reverse(p.components().count()));
    for p in paths {
        if mounted(&p)? {
            sys::overlay_unmount(&p).with_context(|| format!("unmount before removal {}", p.display()))?;
        }
        state.registry.trees.remove(&p);
        state.save()?;
    }
    match fs::symlink_metadata(&path) {
        Ok(m) if m.is_dir() => fs::remove_dir_all(&path)?,
        Ok(_) => fs::remove_file(&path)?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    state.collect()
}
pub fn delete_subvolume(path: &Path) -> Result<()> {
    delete_tree(path)
}
pub fn is_not_empty(e: &anyhow::Error) -> bool {
    e.chain().any(|e| e.downcast_ref::<io::Error>().is_some_and(|e| e.raw_os_error() == Some(libc::ENOTEMPTY)))
}
pub fn clone_file(src: &File, dst: &File) -> io::Result<()> {
    match rustix::fs::ioctl_ficlone(dst, src) {
        Ok(()) => return Ok(()),
        Err(e)
            if matches!(
                e,
                rustix::io::Errno::OPNOTSUPP | rustix::io::Errno::XDEV | rustix::io::Errno::INVAL | rustix::io::Errno::NOTTY
            ) => {}
        Err(e) => return Err(e.into()),
    }
    // ext4 has no reflinks. Preserve holes instead of allocating their zero bytes.
    use rustix::fs::{SeekFrom, seek};
    use std::io::Read;
    let source = src.metadata()?;
    let dest = dst.metadata()?;
    if (source.dev(), source.ino()) == (dest.dev(), dest.ino()) {
        return Err(io::Error::other("source and destination are the same file"));
    }
    let size = source.len();
    dst.set_len(0)?;
    dst.set_len(size)?;
    let mut at = 0;
    while at < size {
        let data = match seek(src, SeekFrom::Data(at)) {
            Ok(n) => n,
            Err(rustix::io::Errno::NXIO) => break,
            Err(e) => return Err(e.into()),
        };
        if data >= size {
            break;
        }
        let end = seek(src, SeekFrom::Hole(data))?.min(size);
        if end <= data {
            return Err(io::Error::other("invalid sparse extent"));
        }
        seek(src, SeekFrom::Start(data))?;
        seek(dst, SeekFrom::Start(data))?;
        if io::copy(&mut src.take(end - data), &mut &*dst)? != end - data {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "source changed during copy"));
        }
        at = end;
    }
    Ok(())
}
pub fn clone_range(src: &File, off: u64, len: u64, dst: &File, dst_off: u64) -> io::Result<()> {
    sys::clone_range(src, off, len, dst, dst_off)
}
pub fn file_identity(path: &Path) -> Result<Vec<u8>> {
    match sys::overlay_attribute(path, c"trusted.layr.origin", None) {
        Ok(v) => Ok(v),
        Err(e) if e.raw_os_error() == Some(libc::ENODATA) => {
            let m = fs::symlink_metadata(path)?;
            Ok(format!("{}:{}", m.dev(), m.ino()).into_bytes())
        }
        Err(e) => Err(e.into()),
    }
}
pub fn mark_received(path: &Path, uuid: &str) -> Result<()> {
    uuid::Uuid::parse_str(uuid)?;
    let path = absolute(path)?;
    let mut s = lock(&path)?;
    s.registry.trees.get_mut(&path).context("unmanaged receive")?.info.received_uuid = Some(uuid.into());
    s.save()
}
pub fn clone_identity(path: &Path) -> Result<(String, Option<u64>)> {
    let i = subvol_info(path)?;
    if !i.readonly {
        bail!("clone source is writable");
    }
    Ok((i.received_uuid.unwrap_or(i.uuid), None))
}
pub fn send_clone_identity(path: &Path) -> Result<(String, u64)> {
    Ok((clone_identity(path)?.0, 1))
}
pub fn send_command(_parent: Option<&Path>, child: &Path) -> Result<Command> {
    let mut c = Command::new(std::env::current_exe()?);
    c.arg("__apfs-send").arg(child);
    Ok(c)
}
pub fn send_optimized(parent: Option<&Path>, child: &Path, _clones: &[PathBuf], _compressed: bool) -> Result<Command> {
    send_command(parent, child)
}
pub fn supports_compressed_receive() -> bool {
    false
}
pub fn supports_local_clone_receive() -> bool {
    true
}

struct ScanEntry {
    meta: crate::fsutil::Meta,
}
fn scan_entries(root: &Path) -> Result<BTreeMap<String, ScanEntry>> {
    let mut out = BTreeMap::new();
    crate::fsutil::walk(root, "", &mut |rel, m| {
        out.insert(rel.into(), ScanEntry { meta: m.clone() });
        Ok(true)
    })?;
    Ok(out)
}
pub fn candidates(parent: &Path, child: &Path) -> Result<Vec<crate::changes::Change>> {
    let _t = crate::trace::span("OverlayFS candidates");
    let (a, b) = (scan_entries(parent)?, scan_entries(child)?);
    let names: std::collections::BTreeSet<_> = a.keys().chain(b.keys()).filter(|s| !s.is_empty()).collect();
    let mut changes: BTreeMap<_, _> = names
        .into_iter()
        .map(|p| {
            (
                p.clone(),
                crate::changes::Change {
                    path: p.clone(),
                    old_path: None,
                    old: a.get(p).map(|entry| entry.meta.clone()),
                    new: b.get(p).map(|entry| entry.meta.clone()),
                    ranges: None,
                },
            )
        })
        .collect();
    let mut removed: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    let mut added: BTreeMap<Vec<u8>, Vec<String>> = BTreeMap::new();
    for (p, c) in &changes {
        if c.new.is_none() {
            removed.entry(file_identity(&parent.join(p))?).or_default().push(p.clone());
        }
        if c.old.is_none() {
            added.entry(file_identity(&child.join(p))?).or_default().push(p.clone());
        }
    }
    for (id, srcs) in removed {
        let Some(dsts) = added.get(&id) else {
            continue;
        };
        // Multiple hard links have the same identity. Do not invent a unique move.
        if srcs.len() != 1 || dsts.len() != 1 {
            continue;
        }
        let (src, dst) = (&srcs[0], &dsts[0]);
        if changes[src].old.as_ref().map(|m| m.ftype) != changes[dst].new.as_ref().map(|m| m.ftype) {
            continue;
        }
        let old = changes.remove(src).unwrap().old;
        let c = changes.get_mut(dst).unwrap();
        c.old = old;
        c.old_path = Some(src.clone());
    }
    for (p, c) in &mut changes {
        if c.old_path.is_none() && c.old.as_ref().is_some_and(|m| m.is_file()) && c.new.as_ref().is_some_and(|m| m.is_file()) {
            c.ranges = Some(changed_ranges(&parent.join(p), &child.join(p))?);
        }
    }
    Ok(changes.into_values().collect())
}

// Compare fixed blocks when the backend cannot report changed extents.
fn changed_ranges(a: &Path, b: &Path) -> Result<Vec<(u64, u64)>> {
    use std::io::Read;
    let (mut a, mut b) = (File::open(a)?, File::open(b)?);
    let (alen, blen) = (a.metadata()?.len(), b.metadata()?.len());
    let mut ranges: Vec<(u64, u64)> = Vec::new();
    let (mut ab, mut bb) = ([0u8; 4096], [0u8; 4096]);
    let mut pos = 0;
    while pos < alen.max(blen) {
        let an = (alen.saturating_sub(pos).min(4096)) as usize;
        let bn = (blen.saturating_sub(pos).min(4096)) as usize;
        a.read_exact(&mut ab[..an])?;
        b.read_exact(&mut bb[..bn])?;
        let len = an.max(bn) as u64;
        if ab[..an] != bb[..bn] {
            match ranges.last_mut() {
                Some((start, n)) if *start + *n == pos => *n += len,
                _ => ranges.push((pos, len)),
            }
        }
        pos += len;
    }
    Ok(ranges)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    #[ignore = "requires root and an isolated test filesystem"]
    fn shared_store_is_rejected_before_registry_changes() -> Result<()> {
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("shared-{}", crate::ids::new_id()));
        fs::create_dir(&root)?;
        sys::overlay_mount(Some(&root), &root, None, libc::MS_BIND, None)?;
        let result = (|| -> Result<()> {
            sys::overlay_mount(None, &root, None, libc::MS_SHARED, None)?;
            let error = recover(&root).unwrap_err();
            assert!(error.to_string().contains("private mount"));
            assert!(!root.join(".layr-overlayfs").exists());
            sys::overlay_mount(None, &root, None, libc::MS_PRIVATE, None)?;
            recover(&root)?;
            assert!(root.join(".layr-overlayfs/registry.json").is_file());
            Ok(())
        })();
        sys::overlay_unmount(&root)?;
        fs::remove_dir_all(&root)?;
        result
    }
    #[test]
    fn private_store_mount_overrides_shared_parent() {
        let mounts = "1 0 0:1 / / rw shared:1 - ext4 /dev/root rw\n2 1 0:1 /store /store rw - ext4 /dev/root rw\n";
        assert!(shared_parent(Path::new("/other/store"), mounts));
        assert!(!shared_parent(Path::new("/store/project"), mounts));
        assert!(shared_parent(Path::new("/store-other"), mounts));
    }
    #[test]
    #[ignore = "requires root and an OverlayFS test filesystem"]
    fn cache_copy_stays_in_its_pinned_parent() -> Result<()> {
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("cache-copy-{}", crate::ids::new_id()));
        fs::create_dir(&root)?;
        let src = root.join("source");
        fs::create_dir(&src)?;
        fs::write(src.join("file"), b"cache")?;
        fs::hard_link(src.join("file"), src.join("hard"))?;
        let parent = root.join("parent");
        let moved = root.join("moved");
        let outside = root.join("outside");
        fs::create_dir(&parent)?;
        fs::create_dir(&outside)?;
        let fd = pinned_directory(&parent)?;
        fs::rename(&parent, &moved)?;
        std::os::unix::fs::symlink(&outside, &parent)?;
        copy_cache_at(&src, &fd, std::ffi::OsStr::new("cache"), None)?;
        assert_eq!(fs::read(moved.join("cache/file"))?, b"cache");
        assert_eq!(fs::metadata(moved.join("cache/file"))?.ino(), fs::metadata(moved.join("cache/hard"))?.ino());
        assert!(copy_cache(&src, &parent.join("escape")).is_err());
        assert_eq!(fs::read_dir(&outside)?.count(), 0);
        drop(fd);
        fs::remove_dir_all(root)?;
        Ok(())
    }
    #[test]
    fn legacy_entries_read_but_new_entries_require_the_new_reader() -> Result<()> {
        let old = serde_json::json!({
            "info": {"uuid":"id", "received_uuid":null,"parent_uuid":null,"readonly":false},
            "lower":"base", "hard_links":false
        });
        let entry: Entry = serde_json::from_value(old)?;
        assert_eq!(entry.lower, "base");
        assert!(entry.bases.is_empty());
        assert!(!entry.plain);
        let new = serde_json::to_value(entry)?;
        assert!(new.get("lower").is_none());
        assert_eq!(new["base"], "base");
        Ok(())
    }
    #[test]
    #[ignore = "requires root and a local filesystem with OverlayFS index support"]
    fn delta_snapshots_keep_open_handles_and_bound_layers() -> Result<()> {
        use std::io::Write;
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("delta-{}", crate::ids::new_id()));
        recover(&root)?;
        let initial = root.join("initial");
        create_subvolume(&initial)?;
        fs::create_dir(initial.join("dir"))?;
        fs::write(initial.join("dir/remove"), b"remove")?;
        fs::write(initial.join("dir/keep"), b"keep")?;
        fs::write(initial.join("unchanged"), vec![7; 1024 * 1024])?;
        fs::write(initial.join("edit"), b"before")?;
        set_readonly(&initial)?;
        let work = root.join("work");
        snapshot(&initial, &work, false)?;
        let cache = work.join("cache");
        create_subvolume(&cache)?;
        fs::write(cache.join("output"), b"private")?;
        fs::remove_dir_all(&cache)?;
        fs::create_dir(&cache)?;
        fs::write(cache.join("output"), b"rebuilt")?;
        assert!(!mounted(&cache)?);
        // A cache replaced by a symlink must never expose its target as history.
        let outside = root.join("outside");
        fs::create_dir(&outside)?;
        fs::write(outside.join("secret"), b"private")?;
        fs::remove_dir_all(&cache)?;
        std::os::unix::fs::symlink(&outside, &cache)?;
        assert!(snapshot(&cache, &root.join("escape"), true).is_err());
        assert!(!root.join("escape").exists());
        fs::remove_file(&cache)?;
        fs::create_dir(&cache)?;
        fs::write(cache.join("output"), b"rebuilt")?;
        let mut writer = fs::OpenOptions::new().write(true).truncate(true).open(work.join("edit"))?;
        writer.write_all(b"saved")?;
        fs::remove_file(work.join("dir/remove"))?;
        fs::rename(work.join("dir"), work.join("moved"))?;
        let saved = root.join("saved");
        snapshot(&work, &saved, true)?;
        assert_eq!(fs::read(saved.join("edit"))?, b"saved");
        assert!(!saved.join("dir").exists());
        assert!(!saved.join("moved/remove").exists());
        assert_eq!(fs::read(saved.join("moved/keep"))?, b"keep");
        assert!(!saved.join("cache/output").exists());
        {
            let s = lock(&saved)?;
            let e = &s.registry.trees[&saved];
            assert!(!e.bases.is_empty());
            assert!(!s.object(&e.lower).join("data/unchanged").exists());
        }
        writer.write_all(b" later")?;
        assert_eq!(fs::read(work.join("edit"))?, b"saved later");
        assert_eq!(fs::read(saved.join("edit"))?, b"saved");
        drop(writer);
        // Each branch adds a layer. The backend must compact before its limit.
        let mut previous = saved;
        for n in 0..20 {
            let branch = root.join(format!("branch-{n}"));
            snapshot(&previous, &branch, false)?;
            fs::write(branch.join("edit"), format!("version {n}"))?;
            let next = root.join(format!("state-{n}"));
            snapshot(&branch, &next, true)?;
            delete_tree(&branch)?;
            delete_tree(&previous)?;
            assert_eq!(fs::read(next.join("edit"))?, format!("version {n}").as_bytes());
            assert_eq!(fs::read(next.join("unchanged"))?.len(), 1024 * 1024);
            assert!(!next.join("dir/remove").exists());
            assert!(lock(&next)?.registry.trees[&next].bases.len() < 16);
            previous = next;
        }
        delete_tree(&work)?;
        delete_tree(&initial)?;
        delete_tree(&previous)?;
        Ok(())
    }
    #[test]
    #[ignore = "requires root and a local filesystem with OverlayFS index support"]
    fn old_cache_mount_migrates_without_losing_data() -> Result<()> {
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("migration-{}", crate::ids::new_id()));
        recover(&root)?;
        let work = root.join("work");
        create_subvolume(&work)?;
        let cache = work.join("cache");
        create_subvolume(&cache)?;
        // Reproduce the previous registry format and mounted cache.
        {
            let mut state = lock(&cache)?;
            let mut e = state.registry.trees[&cache].clone();
            e.plain = false;
            state.mount(&cache, &e)?;
            state.registry.trees.insert(cache.clone(), e);
            state.save()?;
        }
        fs::write(cache.join("output"), b"old cache")?;
        let open_dir = File::open(&cache)?;
        assert!(recover(&root).is_err());
        assert_eq!(fs::read(cache.join("output"))?, b"old cache");
        drop(open_dir);
        recover(&root)?;
        assert!(!mounted(&cache)?);
        assert_eq!(fs::read(cache.join("output"))?, b"old cache");
        fs::write(cache.join("output"), b"new cache")?;
        recover(&root)?;
        assert_eq!(fs::read(cache.join("output"))?, b"new cache");
        fs::remove_dir_all(&cache)?;
        fs::create_dir(&cache)?;
        fs::write(cache.join("rebuilt"), b"rebuilt")?;
        let saved = root.join("saved");
        snapshot(&work, &saved, true)?;
        assert!(!saved.join("cache/rebuilt").exists());
        delete_tree(&saved)?;
        delete_tree(&work)?;
        Ok(())
    }
    #[test]
    #[ignore = "requires root and a local filesystem with OverlayFS index support"]
    fn snapshots_preserve_links_and_isolate_writes() -> Result<()> {
        let root = PathBuf::from(std::env::var("LAYR_OVERLAYFS_TEST_ROOT")?).join(format!("unit-{}", crate::ids::new_id()));
        recover(&root)?;
        let source = root.join("source");
        create_subvolume(&source)?;
        fs::write(source.join("file"), b"base")?;
        fs::hard_link(source.join("file"), source.join("hard"))?;
        let sparse = File::create(source.join("sparse"))?;
        sparse.set_len(2 * 1024 * 1024 * 1024)?;
        std::os::unix::fs::FileExt::write_all_at(&sparse, b"tail", 2 * 1024 * 1024 * 1024 - 4)?;
        drop(sparse);
        let cache = source.join("cache");
        create_subvolume(&cache)?;
        fs::write(cache.join("generated"), b"cache data")?;
        assert!(crate::fsutil::is_layer(&source, "cache", &crate::fsutil::meta_of(&fs::metadata(&cache)?)));
        let state = root.join("state");
        snapshot(&source, &state, true)?;
        assert!(state.join("cache").is_dir());
        assert!(!state.join("cache/generated").exists());
        let sparse = File::open(state.join("sparse"))?;
        assert_eq!(sparse.metadata()?.len(), 2 * 1024 * 1024 * 1024);
        assert!(sparse.metadata()?.blocks() < 1024);
        let mut tail = [0; 4];
        std::os::unix::fs::FileExt::read_exact_at(&sparse, &mut tail, 2 * 1024 * 1024 * 1024 - 4)?;
        assert_eq!(&tail, b"tail");
        drop(sparse);
        assert!(!crate::fsutil::is_layer(&state, "cache", &crate::fsutil::meta_of(&fs::metadata(state.join("cache"))?)));
        assert_eq!(fs::metadata(state.join("file"))?.ino(), fs::metadata(state.join("hard"))?.ino());
        assert!(fs::write(state.join("file"), b"bad").is_err());
        let branch = root.join("branch");
        snapshot(&state, &branch, false)?;
        fs::write(branch.join("file"), b"changed")?;
        assert_eq!(fs::read(branch.join("hard"))?, b"changed");
        assert_eq!(fs::read(state.join("file"))?, b"base");
        delete_tree(&state)?;
        assert_eq!(fs::read(branch.join("hard"))?, b"changed");
        set_readonly(&branch)?;
        assert_eq!(fs::read(branch.join("hard"))?, b"changed");
        {
            let store = lock(&branch)?;
            let entry = &store.registry.trees[&branch];
            assert!(!store.object(&entry.info.uuid).join("upper").exists());
        }
        delete_tree(&branch)?;
        assert!(crate::repo::uncache(&source, "cache", 0, 0)?);
        assert!(!is_subvolume(&cache));
        assert_eq!(fs::read(cache.join("generated"))?, b"cache data");
        assert!(crate::repo::convert_cache(&source, "cache", 0, 0)?);
        assert!(is_subvolume(&cache));
        assert_eq!(fs::read(cache.join("generated"))?, b"cache data");
        fs::create_dir(source.join("group"))?;
        rename_tree(&cache, &source.join("group/cache"))?;
        rename_tree(&source.join("group"), &source.join("moved"))?;
        assert!(is_subvolume(&source.join("moved/cache")));
        assert_eq!(fs::read(source.join("moved/cache/generated"))?, b"cache data");
        delete_tree(&source)?;
        Ok(())
    }
}

fn tree_has_links(path: &Path) -> Result<bool> {
    let m = fs::symlink_metadata(path)?;
    if m.is_file() {
        return Ok(m.nlink() > 1);
    }
    if m.is_dir() {
        for e in fs::read_dir(path)? {
            if tree_has_links(&e?.path())? {
                return Ok(true);
            }
        }
    }
    Ok(false)
}
pub fn contains_managed_tree(path: &Path) -> bool {
    let Ok(path) = absolute(path) else {
        return false;
    };
    lock(&path).is_ok_and(|s| s.registry.trees.keys().any(|p| p.starts_with(&path)))
}
