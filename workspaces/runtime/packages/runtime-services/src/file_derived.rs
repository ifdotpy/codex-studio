//! Process-wide cache of values derived from one file, valid while the file is unchanged.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::SystemTime;

/// Entries one cache keeps before it starts over; the unit is files.
const FILE_DERIVED_CAPACITY: usize = 4096;

/// What identifies one version of a file: size, modification time and inode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Stamp {
    len: u64,
    modified: Option<SystemTime>,
    #[cfg(unix)]
    inode: u64,
}

impl Stamp {
    fn of(path: &Path) -> Option<Self> {
        let metadata = std::fs::metadata(path).ok()?;
        Some(Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(&metadata),
        })
    }
}

/// Derived values per file path. `load` runs again only when a file's stamp changed.
pub struct FileDerived<T> {
    entries: Mutex<HashMap<PathBuf, (Stamp, Arc<T>)>>,
}

impl<T> Default for FileDerived<T> {
    fn default() -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
        }
    }
}

impl<T> FileDerived<T> {
    /// The value derived from `path`, or `None` when the file is missing or `load` fails.
    /// The stamp is taken before `load` reads, so a file that changes meanwhile is
    /// derived again on the next call. The lock is held while loading: concurrent callers
    /// of one file wait for a single derivation instead of repeating it.
    pub fn get_or_load(
        &self,
        path: &Path,
        load: impl FnOnce(&Path) -> Option<T>,
    ) -> Option<Arc<T>> {
        let stamp = Stamp::of(path)?;
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((cached, value)) = entries.get(path)
            && *cached == stamp
        {
            return Some(Arc::clone(value));
        }
        let value = Arc::new(load(path)?);
        if entries.len() >= FILE_DERIVED_CAPACITY {
            entries.clear();
        }
        entries.insert(path.to_owned(), (stamp, Arc::clone(&value)));
        Some(value)
    }
}

#[cfg(test)]
mod tests {
    use super::FileDerived;
    use std::cell::Cell;

    #[test]
    fn derives_again_only_when_the_file_changes() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let path = directory.path().join("source.json");
        let cache = FileDerived::<usize>::default();
        let loads = Cell::new(0);
        let load = |path: &std::path::Path| {
            loads.set(loads.get() + 1);
            Some(std::fs::read(path).expect("source").len())
        };
        assert!(cache.get_or_load(&path, load).is_none(), "a missing file");
        std::fs::write(&path, "abc").expect("write source");
        assert_eq!(cache.get_or_load(&path, load).as_deref(), Some(&3));
        assert_eq!(cache.get_or_load(&path, load).as_deref(), Some(&3));
        assert_eq!(loads.get(), 1);
        std::fs::write(&path, "abcdef").expect("grow source");
        assert_eq!(cache.get_or_load(&path, load).as_deref(), Some(&6));
        assert_eq!(loads.get(), 2);
    }
}
