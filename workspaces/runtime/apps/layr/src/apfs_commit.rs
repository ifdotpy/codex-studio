//! Move a private APFS index into a state without copying it. The project lock is required.
//! The durable journal protects the only staged copy until the record transaction commits.
use crate::{fsutil, ids, model, storage, store::Project};
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::{self, File};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Journal {
    version: u32,
    project: String,
    line: String,
    state: String,
    subvol: String,
    device: u64,
    inode: u64,
    mode: u32,
    /// The index is a thin layer: the state goes to `deltas/<id>`.
    #[serde(default)]
    thin: bool,
}

fn journal_path(p: &Project) -> PathBuf {
    p.meta_dir().join("apfs-index-commit.json")
}

pub fn pending(p: &Project) -> Result<bool> {
    exists(&journal_path(p))
}

fn exists(path: &Path) -> Result<bool> {
    match fs::symlink_metadata(path) {
        Ok(_) => Ok(true),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

fn read(p: &Project) -> Result<Option<Journal>> {
    if !pending(p)? {
        return Ok(None);
    }
    let file = crate::tree::Tree::open(&p.meta_dir())?.open_read("apfs-index-commit.json")?;
    let j: Journal = serde_json::from_reader(file.take(65536)).context("read APFS index state journal")?;
    if j.version != 1
        || j.project != p.id
        || !model::is_uuid(&j.line)
        || !model::is_uuid(&j.state)
        || !model::is_uuid(&j.subvol)
        || j.mode & !0o7777 != 0
    {
        bail!("invalid APFS index state journal");
    }
    Ok(Some(j))
}

/// A failed record must not delete the only staged copy. Recovery owns this state.
pub fn preserves(p: &Project, state: &str) -> Result<bool> {
    Ok(read(p)?.is_some_and(|j| j.state == state))
}

fn target(p: &Project, j: &Journal) -> PathBuf {
    if j.thin {
        p.delta_path(&j.state)
    } else {
        p.state_path(&j.state)
    }
}

fn verify_tree(path: &Path, j: &Journal) -> Result<()> {
    let m = fs::symlink_metadata(path)?;
    if !m.is_dir() || m.dev() != j.device || m.ino() != j.inode || storage::subvol_info(path)?.uuid != j.subvol {
        bail!("APFS index state tree changed: {}", path.display());
    }
    Ok(())
}

fn sync_parent(path: &Path) -> Result<()> {
    File::open(path.parent().context("missing parent")?)?.sync_all()?;
    Ok(())
}

fn move_tree(src: &Path, dst: &Path, mode: u32) -> Result<()> {
    // An immutable root cannot be renamed. Children can remain immutable.
    crate::sys::apfs_immutable(src, false)?;
    fs::set_permissions(src, fs::Permissions::from_mode(mode | 0o200))?;
    crate::sys::apfs_publish(src, dst)?;
    fs::set_permissions(dst, fs::Permissions::from_mode(mode))?;
    sync_parent(src)?;
    sync_parent(dst)?;
    Ok(())
}

fn remove_if_present(path: &Path) -> Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e.into()),
    }
}

/// Reconcile the journal against the committed database record, with the project lock held.
/// A missing record restores the index. A committed record retains the immutable state.
pub fn recover(p: &Project) -> Result<()> {
    let Some(j) = read(p)? else {
        return Ok(());
    };
    let stage = p.stage_path(&j.line);
    let state = target(p, &j);
    if let Some(rec) = p.db.state(&j.state)? {
        if rec.subvol != j.subvol || rec.line.as_deref() != Some(&j.line) || exists(&stage)? {
            bail!("APFS index state record does not match its journal");
        }
        verify_tree(&state, &j)?;
        if !storage::subvol_info(&state)?.readonly {
            bail!("saved APFS index is not immutable");
        }
        remove_if_present(&stage.with_extension("base"))?;
        sync_parent(&stage)?;
    } else {
        match (exists(&stage)?, exists(&state)?) {
            (true, false) => verify_tree(&stage, &j)?,
            (false, true) => {
                verify_tree(&state, &j)?;
                storage::set_writable(&state)?;
                move_tree(&state, &stage, j.mode)?;
            }
            _ => bail!("cannot recover APFS index state: expected exactly one copy of the index"),
        }
        // This also handles interruption while sealing or restoring the index.
        storage::set_writable(&stage)?;
        fs::set_permissions(&stage, fs::Permissions::from_mode(j.mode))?;
        File::open(&stage)?.sync_all()?;
        sync_parent(&stage)?;
    }
    remove_if_present(&journal_path(p))?;
    sync_parent(&journal_path(p))?;
    Ok(())
}

pub struct IndexCommit<'p, 's> {
    project: &'p Project<'s>,
    pub id: String,
    active: bool,
}

impl<'p, 's> IndexCommit<'p, 's> {
    pub fn prepare(project: &'p Project<'s>, line: &str) -> Result<Self> {
        let _t = crate::trace::span("APFS promote index");
        if !model::is_uuid(line) || pending(project)? {
            bail!("APFS index state requires a valid line and no pending journal");
        }
        let stage = project.stage_path(line);
        let m = fs::symlink_metadata(&stage)?;
        let info = storage::subvol_info(&stage)?;
        if !m.is_dir() || info.readonly {
            bail!("APFS index must be a writable tree");
        }
        let j = Journal {
            version: 1,
            project: project.id.clone(),
            line: line.into(),
            state: ids::new_id(),
            subvol: info.uuid,
            device: m.dev(),
            inode: m.ino(),
            mode: m.mode() & 0o7777,
            thin: info.base.is_some(),
        };
        fsutil::atomic_write(&journal_path(project), &serde_json::to_vec(&j)?, 0o600)?;
        let prepared = Self { project, id: j.state.clone(), active: true };
        let state = target(project, &j);
        crate::thin::ensure_parent(&state)?;
        move_tree(&stage, &state, j.mode)?;
        storage::set_readonly(&state)?;
        File::open(&state)?.sync_all()?;
        sync_parent(&state)?;
        Ok(prepared)
    }

    pub fn finish(mut self) -> Result<()> {
        if self.project.db.state(&self.id)?.is_none() {
            bail!("APFS index state has no saved record");
        }
        recover(self.project)?;
        self.active = false;
        Ok(())
    }

    pub fn rollback(mut self) -> Result<()> {
        recover(self.project)?;
        self.active = false;
        Ok(())
    }
}

impl Drop for IndexCommit<'_, '_> {
    fn drop(&mut self) {
        if self.active {
            if let Err(e) = recover(self.project) {
                eprintln!("warning: APFS index recovery is pending: {e:#}");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Actor, Author, Record, StateKind, StateRec};
    use crate::store::Store;

    struct Sandbox(PathBuf);
    impl Sandbox {
        fn new() -> Self {
            let root = PathBuf::from(std::env::var_os("LAYR_APFS_TEST_ROOT").expect("set LAYR_APFS_TEST_ROOT"));
            let p = root.join(format!("index-commit-{}", ids::new_id()));
            fs::create_dir(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Sandbox {
        fn drop(&mut self) {
            let _ = storage::delete_tree(&self.0);
        }
    }
    fn actor() -> Actor {
        Actor { uid: rustix::process::geteuid().as_raw(), name: "test".into(), session: None }
    }
    fn stage(p: &Project, line: &str) -> Result<u64> {
        let path = p.stage_path(line);
        storage::create_subvolume(&path)?;
        fs::write(path.join("file"), b"staged bytes")?;
        fs::hard_link(path.join("file"), path.join("alias"))?;
        std::os::unix::fs::symlink("file", path.join("link"))?;
        crate::sys::apfs_attribute(&path.join("file"), c"com.layr.keep", Some(b"metadata"))?;
        fs::write(path.with_extension("base"), "original base")?;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o555))?;
        Ok(fs::metadata(path)?.ino())
    }
    fn recorded(p: &Project, prepared: &IndexCommit, line: &str) -> Result<()> {
        let mut r = Record::new("commit");
        r.states.push(StateRec {
            id: prepared.id.clone(),
            kind: StateKind::Commit,
            change_id: None,
            derived_from: None,
            context: None,
            parents: vec![],
            line: Some(line.into()),
            subvol: storage::subvol_info(&p.state_path(&prepared.id))?.uuid,
            author: Author::default(),
            message: "test".into(),
            time: ids::now_ms(),
            running: false,
            conflicts: vec![],
            caches: Default::default(),
            cache_by: None,
            cache_shared: false,
            cache_platform: None,
        });
        p.append(r, &actor())?;
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn promotion_keeps_inode_and_drop_restores_the_index() -> Result<()> {
        let s = Sandbox::new();
        let store = Store::open(&s.0.join("store"))?;
        let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
        let _lock = p.lock()?;
        let line = ids::new_id();
        let inode = stage(&p, &line)?;
        let old_info = storage::subvol_info(&p.stage_path(&line))?;
        let prepared = IndexCommit::prepare(&p, &line)?;
        let state = p.state_path(&prepared.id);
        assert!(!p.stage_path(&line).exists());
        assert_eq!(fs::metadata(&state)?.ino(), inode);
        assert_eq!(storage::subvol_info(&state)?.uuid, old_info.uuid);
        assert!(storage::subvol_info(&state)?.readonly);
        assert_eq!(fs::metadata(state.join("file"))?.ino(), fs::metadata(state.join("alias"))?.ino());
        assert!(fs::write(state.join("file"), b"wrong").is_err());
        drop(prepared);
        let restored = p.stage_path(&line);
        assert_eq!(fs::metadata(&restored)?.ino(), inode);
        assert_eq!(fs::metadata(&restored)?.mode() & 0o777, 0o555);
        assert_eq!(fs::read(restored.join("file"))?, b"staged bytes");
        assert_eq!(crate::sys::apfs_attribute(&restored.join("file"), c"com.layr.keep", None)?, b"metadata");
        assert_eq!(fs::read_link(restored.join("link"))?, Path::new("file"));
        assert_eq!(fs::read_to_string(restored.with_extension("base"))?, "original base");
        assert!(!state.exists());
        assert!(!pending(&p)?);
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn reopen_recovers_each_uncommitted_publication_boundary() -> Result<()> {
        for phase in 0..4 {
            let s = Sandbox::new();
            let store = Store::open(&s.0.join("store"))?;
            let line = ids::new_id();
            let inode;
            {
                let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
                let _lock = p.lock()?;
                inode = stage(&p, &line)?;
                if phase == 3 {
                    std::mem::forget(IndexCommit::prepare(&p, &line)?);
                } else {
                    let src = p.stage_path(&line);
                    let m = fs::metadata(&src)?;
                    let j = Journal {
                        version: 1,
                        project: p.id.clone(),
                        line: line.clone(),
                        state: ids::new_id(),
                        subvol: storage::subvol_info(&src)?.uuid,
                        device: m.dev(),
                        inode,
                        mode: m.mode() & 0o7777,
                        thin: false,
                    };
                    fsutil::atomic_write(&journal_path(&p), &serde_json::to_vec(&j)?, 0o600)?;
                    if phase > 0 {
                        move_tree(&src, &p.state_path(&j.state), j.mode)?;
                    }
                    if phase == 2 {
                        crate::sys::apfs_immutable(&p.state_path(&j.state).join("file"), true)?;
                    }
                }
            }
            let p = Project::open(&store, "p")?;
            assert_eq!(fs::metadata(p.stage_path(&line))?.ino(), inode);
            assert_eq!(fs::read(p.stage_path(&line).join("file"))?, b"staged bytes");
            assert!(!storage::subvol_info(&p.stage_path(&line))?.readonly);
            assert!(!pending(&p)?);
            assert_eq!(fs::read_dir(p.states_dir())?.count(), 0);
        }
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn committed_publication_survives_reopen_and_retry() -> Result<()> {
        let s = Sandbox::new();
        let store = Store::open(&s.0.join("store"))?;
        let line = ids::new_id();
        let id;
        let inode;
        {
            let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
            let _lock = p.lock()?;
            inode = stage(&p, &line)?;
            let prepared = IndexCommit::prepare(&p, &line)?;
            id = prepared.id.clone();
            recorded(&p, &prepared, &line)?;
            std::mem::forget(prepared);
        }
        let p = Project::open(&store, "p")?;
        assert!(p.db.state(&id)?.is_some());
        assert!(!p.stage_path(&line).exists());
        assert!(!p.stage_path(&line).with_extension("base").exists());
        assert_eq!(fs::metadata(p.state_path(&id))?.ino(), inode);
        assert!(storage::subvol_info(&p.state_path(&id))?.readonly);
        assert!(!pending(&p)?);
        let _lock = p.lock()?;
        recover(&p)?;
        assert_eq!(fs::read(p.state_path(&id).join("file"))?, b"staged bytes");
        Ok(())
    }

    #[test]
    #[ignore = "requires a case-sensitive APFS test volume"]
    fn recovery_refuses_a_substitute_tree_and_invalid_journal() -> Result<()> {
        let s = Sandbox::new();
        let store = Store::open(&s.0.join("store"))?;
        let p = Project::create(&store, "p", &actor(), serde_json::json!({}))?;
        let _lock = p.lock()?;
        let line = ids::new_id();
        stage(&p, &line)?;
        let prepared = IndexCommit::prepare(&p, &line)?;
        let state = p.state_path(&prepared.id);
        std::mem::forget(prepared);
        let keep = p.work_dir().join("keep");
        move_tree(&state, &keep, 0o555)?;
        storage::create_subvolume(&state)?;
        fs::write(state.join("other"), b"keep this")?;
        assert!(recover(&p).is_err());
        assert_eq!(fs::read(state.join("other"))?, b"keep this");
        assert_eq!(fs::read(keep.join("file"))?, b"staged bytes");
        let mut j = read(&p)?.unwrap();
        j.line = "../outside".into();
        fsutil::atomic_write(&journal_path(&p), &serde_json::to_vec(&j)?, 0o600)?;
        assert!(recover(&p).is_err());
        Ok(())
    }
}
