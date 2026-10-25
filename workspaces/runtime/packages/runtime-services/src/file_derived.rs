//! Process-wide cache of values derived from one file, valid while the file is unchanged.

use std::collections::HashMap;
use std::fs::Metadata;
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
        Some(Self::from_metadata(&metadata))
    }
    fn from_metadata(metadata: &Metadata) -> Self {
        Self {
            len: metadata.len(),
            modified: metadata.modified().ok(),
            #[cfg(unix)]
            inode: std::os::unix::fs::MetadataExt::ino(metadata),
        }
    }
}

/// Derived values per file path. `load` runs again only when a file's stamp changed.
pub struct FileDerived<T> {
    entries: Mutex<HashMap<(PathBuf, String), (Stamp, Arc<T>)>>,
    capacity: usize,
}

impl<T> Default for FileDerived<T> {
    fn default() -> Self {
        Self::with_capacity(FILE_DERIVED_CAPACITY)
    }
}

impl<T> FileDerived<T> {
    /// Bound the number of retained file/context derivations (unit: entries).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: Mutex::new(HashMap::new()),
            capacity,
        }
    }

    /// Use metadata of an already validated open file, and include parser inputs
    /// (such as expected session and byte allowance) in the derivation identity.
    pub fn get_or_load_opened(
        &self,
        path: &Path,
        metadata: &Metadata,
        context: String,
        load: impl FnOnce() -> T,
    ) -> Arc<T> {
        self.get_or_load_stamped(path, Stamp::from_metadata(metadata), context, load)
    }

    fn get_or_load_stamped(
        &self,
        path: &Path,
        stamp: Stamp,
        context: String,
        load: impl FnOnce() -> T,
    ) -> Arc<T> {
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (path.to_owned(), context);
        if let Some((cached, value)) = entries.get(&key)
            && *cached == stamp
        {
            return Arc::clone(value);
        }
        let value = Arc::new(load());
        if entries.len() >= self.capacity {
            entries.clear();
        }
        entries.insert(key, (stamp, Arc::clone(&value)));
        value
    }

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
        // Optional loaders retain their original absent/error semantics.
        let mut entries = self.entries.lock().unwrap_or_else(PoisonError::into_inner);
        let key = (path.to_owned(), String::new());
        if let Some((cached, value)) = entries.get(&key)
            && *cached == stamp
        {
            return Some(Arc::clone(value));
        }
        let value = Arc::new(load(path)?);
        if entries.len() >= self.capacity {
            entries.clear();
        }
        entries.insert(key, (stamp, Arc::clone(&value)));
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
