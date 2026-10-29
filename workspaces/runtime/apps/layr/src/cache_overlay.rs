//! Cache overlays (docs/cache-overlays.md): folder overlays that hold regenerable build output.
//!
//! A folder overlay owns one folder of the tree. In a working folder that folder is a nested layer
//! (a nested subvolume on btrfs, a marked folder on APFS), so scans, snapshots and automatic
//! states of the line never see its files, as for caches. A state of the overlay is one snapshot
//! of that nested layer; a line shows a state as one writable snapshot at the path. Each line has
//! its own overlay line (`line-<line id>`, follow mode): a line has caches, not a state.
//!
//! `save --caches` makes the cache overlay of each cache path when it is missing (properties:
//! quiet, history last, merge union-theirs, readers `*`) and saves a state of it for the line.

use crate::cmd::exit;
use crate::ctx::Ctx;
use crate::fsutil;
use crate::ids;
use crate::ignore_rules::Rules;
use crate::model::{Line, StateKind, StateRec};
use crate::overlay::{files_of, MergeKind, Mode, Props};
use crate::repo::Repo;
use crate::storage as btrfs;
use anyhow::{bail, Result};
use std::collections::BTreeSet;
use std::path::Path;

/// A folder path as a folder overlay keeps it: relative, without `.`, `..` or empty parts, not
/// `.git`.
pub fn folder_path(s: &str) -> Result<String> {
    let t = s.trim_end_matches('/');
    let bad = t.is_empty()
        || t.starts_with('/')
        || t.split('/').any(|c| c.is_empty() || c == "." || c == "..")
        || t == ".git"
        || t.starts_with(".git/");
    if bad {
        return Err(exit(128, format!("fatal: invalid folder '{s}': use a relative path inside the line")));
    }
    Ok(t.to_string())
}

/// The name of the cache overlay that `save --caches` makes for `folder`: `cache.<folder>` with
/// `/` as `.`, or a hash when the folder has other characters.
pub fn cache_overlay_name(folder: &str) -> String {
    let name = format!("cache.{}", folder.replace('/', "."));
    if crate::store::check_name("overlay", &name).is_ok() {
        return name;
    }
    use sha2::Digest;
    format!("cache.{}", &hex::encode(sha2::Sha256::digest(folder.as_bytes()))[..12])
}

/// The properties of a cache overlay (docs/cache-overlays.md, section 2).
pub fn cache_props() -> Props {
    Props { history: Some(1), quiet: true, merge: MergeKind::UnionTheirs, gc_lru: None }
}

/// The apparent size of the files below `root` (not following links, not entering other layers).
pub fn tree_size(root: &Path) -> u64 {
    let mut total = 0u64;
    let _ = fsutil::walk(root, "", &mut |_, m| {
        if !m.is_dir() {
            total += m.size;
        }
        Ok(true)
    });
    total
}

/// Where a line takes caches from.
pub enum Source<'s> {
    Line(&'s Line),
    State(&'s str),
}

/// The platform key of a cache (docs/cache-overlays.md, section 6): `<os>-<arch>`, and with key
/// files the first 16 hex digits of the SHA-256 of their names and contents in the working folder
/// `w` (a missing file counts as empty). layr knows no toolchains: the files name them.
pub fn platform_key(w: &Path, key_files: &[String]) -> String {
    let base = crate::model::cache_platform();
    if key_files.is_empty() {
        return base;
    }
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    for f in key_files {
        let data = fsutil::safe_join(w, f).ok().and_then(|p| std::fs::read(p).ok()).unwrap_or_default();
        h.update((f.len() as u64).to_le_bytes());
        h.update(f.as_bytes());
        h.update((data.len() as u64).to_le_bytes());
        h.update(&data);
    }
    format!("{base}-{}", &hex::encode(h.finalize())[..16])
}

/// A state of a cache overlay made on another operating system or processor: a machine never
/// imports it from a layr remote (docs/cache-overlays.md, section 6).
pub fn other_platform(platform: Option<&str>) -> bool {
    let base = crate::model::cache_platform();
    platform.is_some_and(|p| p != base && !p.starts_with(&format!("{base}-")))
}

impl<'a> Repo<'a> {
    /// The key files of overlay `name` (`overlay.<name>.key-files`).
    pub fn key_files(&self, name: &str) -> Result<Vec<String>> {
        Ok(self
            .overlay_setting(name, "key-files")?
            .and_then(|v| serde_json::from_value::<Vec<String>>(v).ok())
            .unwrap_or_default())
    }

    /// The platform key of overlay `name` for the working folder `w`.
    pub fn cache_key(&self, name: &str, w: &Path) -> Result<String> {
        Ok(platform_key(w, &self.key_files(name)?))
    }

    /// The folder overlays of the project: name and folder.
    pub fn folder_overlays(&self) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        for name in self.overlay_names()? {
            if let Some(f) = self.overlay_setting(&name, "folder")?.and_then(|v| v.as_str().map(str::to_owned)) {
                out.push((name, f));
            }
        }
        Ok(out)
    }

    pub fn folder_overlay_of(&self, folder: &str) -> Result<Option<String>> {
        Ok(self.folder_overlays()?.into_iter().find(|(_, f)| f == folder).map(|(n, _)| n))
    }

    /// The cache overlay of `folder`, made when missing. The caller holds the project lock.
    fn ensure_cache_overlay(&self, ctx: &Ctx, folder: &str) -> Result<String> {
        if let Some(n) = self.folder_overlay_of(folder)? {
            self.sync_cache_readers(ctx, &n);
            return Ok(n);
        }
        let name = cache_overlay_name(folder);
        if self.overlay_info(&name)?.is_some() {
            bail!("overlay '{name}' exists and does not hold folder {folder}");
        }
        // The project setting `cache.key-files` gives new cache overlays their key files.
        let key_files = self.p.config_value("cache.key-files")?.filter(|v| v.is_array());
        crate::cmd::overlaycmd::make_overlay(
            ctx,
            self,
            &name,
            &self.cache_readers()?,
            Mode::Follow,
            false,
            &cache_props(),
            Some(folder),
            true,
            key_files.as_ref(),
        )?;
        Ok(name)
    }

    /// The readers of a cache overlay besides its owner: the machines this project trusts (this
    /// machine included), so that store copies and peer sync give caches only to them. Local users
    /// take a cache by the rules of `usable_cache`.
    fn cache_readers(&self) -> Result<Vec<String>> {
        Ok(crate::store_remote::overlays::trusted_keys(self)?.into_keys().map(|m| format!("machine:{m}")).collect())
    }

    /// Add machines that the project trusts since the cache overlay was made to its readers. Only a
    /// key manager of the overlay may; elsewhere the readers stay, and the next save on a manager
    /// adds them. A failure leaves the readers as they were.
    fn sync_cache_readers(&self, ctx: &Ctx, name: &str) {
        let r = (|| -> Result<()> {
            let orepo = self.overlay_repo(name)?;
            let have: BTreeSet<String> = orepo
                .policy()
                .access
                .iter()
                .filter(|(_, r)| **r >= crate::access::Role::Reader)
                .map(|(p, _)| p.clone())
                .collect();
            let want = self.cache_readers()?;
            if want.iter().all(|m| have.contains(m)) || !orepo.is_admin(ctx) {
                return Ok(());
            }
            crate::overlay_managers::of(self, name)?.require(&ctx.store.machine.id)?;
            let info = self.need_overlay(name)?;
            let mut readers: Vec<String> = have.into_iter().filter(|p| *p != format!("user:{}", info.owner)).collect();
            for m in want {
                if !readers.contains(&m) {
                    readers.push(m);
                }
            }
            let _lock = orepo.p.lock()?;
            let mut record = crate::model::Record::new("overlay.readers");
            record.data = serde_json::json!({"config":{"access": crate::cmd::overlaycmd::access_map(info.owner, &readers)}});
            orepo.commit_record(ctx, record).map(|_| ())
        })();
        if let Err(e) = r {
            eprintln!("warning: overlay '{name}': readers not updated: {e:#}");
        }
    }

    /// Save the nested layers at `paths` of the line's working folder as new states of their
    /// cache overlays, on the overlay line of this line. `shared`: saved on a protected line, so
    /// lines of other users may take them. The caller holds the project lock. Returns the folder,
    /// the overlay and the new state of each.
    pub fn save_cache_overlays(
        &self,
        ctx: &Ctx,
        line: &Line,
        paths: &[String],
        shared: bool,
    ) -> Result<Vec<(String, String, String)>> {
        let w = self.working(line);
        let mut out = Vec::new();
        for p in paths {
            let cp = fsutil::safe_join(&w, p)?;
            if !btrfs::is_subvolume(&cp) {
                continue;
            }
            let name = self.ensure_cache_overlay(ctx, p)?;
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            let id = ids::new_id();
            {
                let _lock = orepo.p.lock()?;
                let ol = orepo.ensure_overlay_line(
                    ctx,
                    &Repo::overlay_line_name(&info, &line.id, line.owner),
                    &info.root,
                    line.owner,
                )?;
                let dst = orepo.p.state_path(&id);
                let made = btrfs::snapshot(&cp, &dst, true).and_then(|_| btrfs::subvol_info(&dst));
                let sub = match made {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = btrfs::delete_tree(&dst);
                        return Err(e);
                    }
                };
                let st = StateRec {
                    id: id.clone(),
                    kind: StateKind::Commit,
                    change_id: Some(ids::new_id()),
                    derived_from: None,
                    context: None,
                    parents: vec![ol.head.clone()],
                    line: Some(ol.id.clone()),
                    subvol: sub.uuid,
                    author: ctx.author(),
                    message: format!("cache {p} of line {}", line.name),
                    time: ids::now_ms(),
                    running: false,
                    conflicts: Vec::new(),
                    caches: Default::default(),
                    cache_by: Some(ctx.caller.uid),
                    cache_shared: shared,
                    cache_platform: Some(self.cache_key(&name, &w)?),
                };
                if let Err(e) = crate::overlay::move_overlay_line(&orepo, ctx, "commit", &ol, &id, vec![st]) {
                    let _ = btrfs::delete_tree(&dst);
                    return Err(e);
                }
            }
            self.after_overlay_move(ctx, &name);
            out.push((p.clone(), name, id));
        }
        Ok(out)
    }

    /// A state of overlay `name` that a line of `uid` may show: on this machine, saved on this
    /// platform, and made by `uid` or on a protected line (a cache holds code that review does not
    /// see). The empty first state is no cache.
    fn usable_cache(&self, orepo: &Repo, root: &str, id: &str, uid: u32, key: &str) -> Result<bool> {
        if id == root || !orepo.p.has_state(id) {
            return Ok(false);
        }
        let Some(st) = orepo.p.db.state(id)? else { return Ok(false) };
        let allowed = uid == 0 || st.cache_shared || st.cache_by == Some(uid);
        Ok(allowed && st.cache_platform.as_deref() == Some(key))
    }

    /// The cache state of overlay `name` that a line of `uid` takes: the newest cache of the
    /// source line, or for a state the newest cache of the line that made it or of the line of its
    /// nearest ancestor that has one.
    pub fn cache_source(&self, name: &str, from: &Source, uid: u32, key: &str) -> Result<Option<String>> {
        let info = self.need_overlay(name)?;
        let orepo = self.overlay_repo(name)?;
        let of_line = |line_id: &str| -> Result<Option<String>> {
            let Some(ol) = orepo.p.db.line_by_name(&format!("line-{line_id}"))? else { return Ok(None) };
            Ok(self.usable_cache(&orepo, &info.root, &ol.head, uid, key)?.then_some(ol.head))
        };
        let start = match from {
            Source::Line(l) => {
                if let Some(s) = of_line(&l.id)? {
                    return Ok(Some(s));
                }
                l.head.clone()
            }
            Source::State(s) => s.to_string(),
        };
        const SEARCH: usize = 200;
        let mut seen = BTreeSet::new();
        let mut q = std::collections::VecDeque::from([start]);
        while let Some(id) = q.pop_front() {
            if seen.len() >= SEARCH || !seen.insert(id.clone()) {
                continue;
            }
            let Some(rec) = self.p.db.state(&id)? else { continue };
            if let Some(l) = &rec.line {
                if let Some(s) = of_line(l)? {
                    return Ok(Some(s));
                }
            }
            q.extend(rec.parents);
        }
        Ok(None)
    }

    /// Put the cache overlays into the new working folder `w` of `line` (made from `start`, or
    /// from the line `source`): each folder overlay whose folder the ignore rules ignore and the
    /// state does not hold gets a writable snapshot of its cache state, and the line gets an
    /// overlay line at that state. Returns the folders that got a cache.
    pub fn take_caches(&self, ctx: &Ctx, line: &Line, start: &str, source: Option<&Line>, w: &Path) -> Result<Vec<String>> {
        let mut taken = Vec::new();
        if self.p.is_overlay() {
            return Ok(taken);
        }
        let ignored = Rules::new(w);
        let (uid, gid) = self.owner_ids(line);
        let (regenerable, setting) = (Rules::attribute(w, "regenerable"), self.p.config_strings("regenerable"));
        for (name, folder) in self.folder_overlays()? {
            if crate::repo::holds_files(w, &folder) || !ignored.ignored(&folder, true) {
                continue;
            }
            // A cache overlay only while the folder is a cache path of the new line (its
            // `.layrattributes` still marks it, or the machine setting names it).
            let is_cache = self.overlay_setting(&name, "cache")?.and_then(|v| v.as_bool()).unwrap_or(false);
            if is_cache && !setting.contains(&folder) && !regenerable.ignored(&folder, true) {
                continue;
            }
            let from = match source {
                Some(s) => Source::Line(s),
                None => Source::State(start),
            };
            let key = self.cache_key(&name, w)?;
            let Some(sid) = self.cache_source(&name, &from, uid, &key)? else { continue };
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            let cp = fsutil::safe_join(w, &folder)?;
            if cp.symlink_metadata().is_ok() {
                fsutil::remove_entry(w, &folder)?;
            }
            fsutil::make_parents(w, &folder, Some((uid, gid)))?;
            btrfs::snapshot(&orepo.p.state_path(&sid), &cp, false)?;
            crate::repo::give_tree(&cp, uid, gid)?;
            let _lock = orepo.p.lock()?;
            orepo.ensure_overlay_line(ctx, &Repo::overlay_line_name(&info, &line.id, line.owner), &sid, line.owner)?;
            taken.push(folder);
        }
        Ok(taken)
    }

    /// Bring the caches of `from` into the line (`merge` of a line): for each
    /// folder overlay, a fast-forward of the line's overlay line, or a `union-theirs` state; the
    /// files of the incoming state are copied over the line's folder (a missing or empty folder
    /// gets one snapshot). The caller holds the project lock. Returns folder and new state.
    pub fn pull_caches(&self, ctx: &Ctx, line: &Line, from: &Source) -> Result<Vec<(String, String)>> {
        let mut out = Vec::new();
        if self.p.is_overlay() {
            return Ok(out);
        }
        let w = self.working(line);
        let (uid, gid) = self.owner_ids(line);
        let own = self.own(line);
        let shared = self.policy().rule_for(&line.name).is_some();
        for (name, folder) in self.folder_overlays()? {
            let key = self.cache_key(&name, &w)?;
            let Some(sid) = self.cache_source(&name, from, uid, &key)? else { continue };
            let info = self.need_overlay(&name)?;
            let orepo = self.overlay_repo(&name)?;
            let ol_name = Repo::overlay_line_name(&info, &line.id, line.owner);
            let made = {
                let _lock = orepo.p.lock()?;
                let ol = orepo.ensure_overlay_line(ctx, &ol_name, &info.root, line.owner)?;
                if ol.head == sid || orepo.is_ancestor(&sid, &ol.head)? {
                    continue;
                }
                let fast = ol.head == info.root || orepo.is_ancestor(&ol.head, &sid)?;
                let (head, states) = if fast {
                    (sid.clone(), Vec::new())
                } else {
                    let mut st =
                        crate::overlay::union_state(&orepo, ctx, &ol, &ol.head, &sid, &format!("cache {folder}: union"))?;
                    st.cache_by = Some(ctx.caller.uid);
                    st.cache_shared = shared;
                    st.cache_platform = Some(key.clone());
                    (st.id.clone(), vec![st])
                };
                // The working folder first: a failure leaves the overlay line where it was.
                let cp = fsutil::safe_join(&w, &folder)?;
                let src = orepo.p.state_path(&sid);
                let empty = cp.symlink_metadata().is_err() || (btrfs::is_subvolume(&cp) && files_of(&cp)?.is_empty());
                let r = if crate::repo::holds_files(&self.state_root(&line.head)?, &folder) {
                    Err(exit(1, format!("error: {folder} holds files of the line's head; it is no cache")))
                } else if empty {
                    (|| -> Result<()> {
                        if cp.symlink_metadata().is_ok() {
                            fsutil::remove_entry(&w, &folder)?;
                        }
                        fsutil::make_parents(&w, &folder, Some((uid, gid)))?;
                        btrfs::snapshot(&src, &cp, false)?;
                        crate::repo::give_tree(&cp, uid, gid)
                    })()
                } else {
                    let ours: BTreeSet<String> = files_of(&cp)?.into_iter().collect();
                    crate::overlay::union_ops(&ours, files_of(&src)?).apply(&cp, &src, own)
                };
                if let Err(e) =
                    r.and_then(|_| crate::overlay::move_overlay_line(&orepo, ctx, "merge", &ol, &head, states.clone()))
                {
                    orepo.discard_states(&states);
                    return Err(e);
                }
                head
            };
            self.after_overlay_move(ctx, &name);
            out.push((folder, made));
        }
        Ok(out)
    }

    /// `gc lru:<size>` of the folder overlays: in the working folder of each line, delete the
    /// files of the folder that were used least recently (the newer of access and change time)
    /// until the folder is at most the size. Returns the deleted bytes.
    pub fn cache_gc_lru(&self) -> Result<u64> {
        let mut freed = 0;
        for (name, folder) in self.folder_overlays()? {
            let Some(limit) = self.overlay_props(&name)?.gc_lru else { continue };
            for line in self.lines()? {
                let w = self.working(&line);
                let Ok(cp) = fsutil::safe_join(&w, &folder) else { continue };
                if !btrfs::is_subvolume(&cp) {
                    continue;
                }
                freed += lru_trim(&cp, limit)?;
            }
        }
        Ok(freed)
    }
}

/// Delete the least recently used files below `root` until their sizes add up to `limit` at most.
pub fn lru_trim(root: &Path, limit: u64) -> Result<u64> {
    use std::os::unix::fs::MetadataExt;
    let mut files: Vec<(i64, u64, String)> = Vec::new();
    let mut total = 0u64;
    fsutil::walk(root, "", &mut |rel, m| {
        if !m.is_dir() {
            let used = std::fs::symlink_metadata(root.join(rel)).map(|x| x.atime().max(x.mtime())).unwrap_or(0);
            files.push((used, m.size, rel.to_string()));
            total += m.size;
        }
        Ok(true)
    })?;
    if total <= limit {
        return Ok(0);
    }
    files.sort();
    let mut freed = 0;
    for (_, size, rel) in files {
        if total - freed <= limit {
            break;
        }
        if fsutil::remove_entry(root, &rel).is_ok() {
            freed += size;
        }
    }
    Ok(freed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn folder_paths_and_cache_names() {
        assert_eq!(folder_path("target/").unwrap(), "target");
        assert_eq!(folder_path("web/node_modules").unwrap(), "web/node_modules");
        for bad in ["", "/abs", "a/../b", "./x", ".git", ".git/x", "a//b"] {
            assert!(folder_path(bad).is_err(), "{bad}");
        }
        assert_eq!(cache_overlay_name("target"), "cache.target");
        assert_eq!(cache_overlay_name("web/node_modules"), "cache.web.node_modules");
        let odd = cache_overlay_name("a b/c");
        assert!(odd.starts_with("cache.") && odd.len() == 18, "{odd}");
        assert_eq!(cache_props().describe(), ["history=last", "quiet", "merge=union-theirs"]);
    }

    #[test]
    fn the_platform_key_names_the_key_files_and_their_content() {
        let w = std::env::temp_dir().join(format!("layr-key-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&w);
        std::fs::create_dir_all(&w).unwrap();
        let base = crate::model::cache_platform();
        assert_eq!(platform_key(&w, &[]), base);
        let files = vec!["Cargo.lock".to_string(), "rust-toolchain.toml".to_string()];
        let missing = platform_key(&w, &files);
        assert!(missing.starts_with(&format!("{base}-")) && missing.len() == base.len() + 17);
        std::fs::write(w.join("Cargo.lock"), "a").unwrap();
        let one = platform_key(&w, &files);
        assert_ne!(one, missing, "content counts");
        assert_eq!(one, platform_key(&w, &files), "the key is stable");
        std::fs::write(w.join("Cargo.lock"), "b").unwrap();
        assert_ne!(one, platform_key(&w, &files));
        assert!(!other_platform(Some(&base)));
        assert!(!other_platform(Some(&one)));
        assert!(!other_platform(None), "an old state names no platform");
        assert!(other_platform(Some("plan9-mips")));
        assert!(other_platform(Some(&format!("{base}x-0011"))));
        let _ = std::fs::remove_dir_all(&w);
    }

    #[test]
    fn lru_trim_deletes_the_oldest_files_first() {
        let root = std::env::temp_dir().join(format!("layr-lru-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("d")).unwrap();
        for (i, name) in ["old", "d/mid", "new"].iter().enumerate() {
            let p = root.join(name);
            std::fs::write(&p, vec![0u8; 100]).unwrap();
            let t = std::time::SystemTime::UNIX_EPOCH + std::time::Duration::from_secs(1_000_000 + i as u64 * 1000);
            let f = std::fs::File::options().write(true).open(&p).unwrap();
            f.set_times(std::fs::FileTimes::new().set_accessed(t).set_modified(t)).unwrap();
        }
        assert_eq!(lru_trim(&root, 300).unwrap(), 0, "under the limit nothing goes");
        assert_eq!(lru_trim(&root, 150).unwrap(), 200);
        assert!(!root.join("old").exists() && !root.join("d/mid").exists() && root.join("new").exists());
        assert_eq!(tree_size(&root), 100);
        let _ = std::fs::remove_dir_all(&root);
    }
}
