//! Disposable metadata streams for immutable states. No records depend on this cache.

use crate::{fsutil, model, sys};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::fd::AsFd;
use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
use std::path::{Path, PathBuf};

const MAX_ENTRY: u64 = 8 << 20;
const MAX_TOTAL: u64 = 32 << 20;
const MAX_ENTRIES: usize = 256;

pub struct Cache {
    dir: PathBuf,
    name: String,
}

impl Cache {
    pub fn for_states(parent: &Path, child: &Path, p: &File, c: &File) -> Option<Self> {
        let states = parent.parent()?;
        if child.parent()? != states || states.file_name()? != "states" {
            return None;
        }
        for path in [parent, child] {
            if !model::is_uuid(path.file_name()?.to_str()?) {
                return None;
            }
        }
        let meta = states.parent()?.join("meta");
        let trusted = |path: &Path| -> Option<()> {
            let m = fsutil::open_dir_nofollow(path).ok()?.metadata().ok()?;
            (m.uid() == rustix::process::geteuid().as_raw() && m.mode() & 0o077 == 0).then_some(())
        };
        trusted(&meta)?;
        let dir = meta.join("diff-cache");
        if !dir.exists() {
            use std::os::unix::fs::DirBuilderExt;
            let _ = fs::DirBuilder::new().mode(0o700).create(&dir);
        }
        trusted(&dir)?;
        let mut hash = Sha256::new();
        hash.update(b"layr-no-data-v1");
        for file in [p, c] {
            if sys::subvol_flags(file.as_fd()).ok()? & sys::SUBVOL_RDONLY == 0 {
                return None;
            }
            let info = sys::subvol_info(file.as_fd()).ok()?;
            hash.update(sys::filesystem_uuid(file.as_fd()).ok()?);
            hash.update(info.uuid);
            hash.update(info.generation.to_le_bytes());
            hash.update(info.ctransid.to_le_bytes());
        }
        Some(Self { dir, name: hex::encode(hash.finalize()) })
    }

    pub fn read(&self) -> Option<Vec<u8>> {
        let dir = fsutil::open_dir_nofollow(&self.dir).ok()?;
        let f = fsutil::open_file_at(&dir, &self.name).ok()?;
        let len = f.metadata().ok()?.len();
        if !(32..=MAX_ENTRY).contains(&len) {
            return None;
        }
        let mut data = Vec::new();
        f.take(MAX_ENTRY + 1).read_to_end(&mut data).ok()?;
        if data.len() as u64 != len || Sha256::digest(&data[32..])[..] != data[..32] {
            return None;
        }
        Some(data.split_off(32))
    }

    pub fn write(&self, data: &[u8]) -> anyhow::Result<()> {
        let size = data.len() as u64 + 32;
        if size > MAX_ENTRY {
            return Ok(());
        }
        let dir = fsutil::open_dir_nofollow(&self.dir)?;
        // Readers can use an entry while another command replaces or removes its name.
        rustix::fs::flock(&dir, rustix::fs::FlockOperation::LockExclusive)?;
        let mut entries = Vec::new();
        let mut total = 0;
        for entry in fs::read_dir(&self.dir)?.flatten() {
            let m = fs::symlink_metadata(entry.path())?;
            if m.is_file() {
                total += m.len();
                entries.push((m.modified()?, entry.path(), m.len()));
            }
        }
        entries.sort();
        let mut count = entries.len();
        for (_, path, len) in entries {
            if total + size <= MAX_TOTAL && count < MAX_ENTRIES {
                break;
            }
            fs::remove_file(path)?;
            total -= len;
            count -= 1;
        }
        let mut saved = Sha256::digest(data).to_vec();
        saved.extend_from_slice(data);
        // This cache is disposable. Publish atomically, but do not force a disk transaction.
        // A partial write after a crash fails the checksum and causes a fresh stream.
        let tmp = self.dir.join(format!(".tmp-{}", crate::ids::new_id()));
        let result = (|| {
            let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&tmp)?;
            f.write_all(&saved)?;
            fs::rename(&tmp, self.dir.join(&self.name))?;
            Ok(())
        })();
        let _ = fs::remove_file(&tmp);
        result
    }
}
