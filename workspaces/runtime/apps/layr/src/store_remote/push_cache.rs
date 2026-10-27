//! Rebuildable, content-addressed local tree cache. No remote bytes use this format.

use super::{
    format::{Entry, Tree},
    zst,
};
use anyhow::{bail, Result};
use rusqlite::{params, Connection, OptionalExtension};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

// Grow with compressed uploaded payload while retaining a fixed upper bound.
// The rollback journal is held in memory; this database is never authoritative.
const BASE_BYTES: usize = 512 << 10;
const MAX_BYTES: usize = 16 << 20;
const PAGE_SIZE: usize = 4096;
const NODE_LIMIT: usize = 64 << 20;

#[derive(Default, Serialize, Deserialize)]
struct Node {
    files: BTreeMap<String, Entry>,
    dirs: BTreeMap<String, String>,
}

pub(super) struct TreeCache(Connection);

impl TreeCache {
    pub(super) fn open(dir: &Path) -> Result<Self> {
        let path = dir.join("push-cache.sqlite3");
        let open = || -> Result<Self> {
            let db = Connection::open(&path)?;
            db.execute_batch(&format!(
                "PRAGMA page_size={PAGE_SIZE}; PRAGMA max_page_count={};
                 PRAGMA journal_mode=MEMORY; PRAGMA synchronous=OFF;
                 CREATE TABLE IF NOT EXISTS nodes (hash BLOB PRIMARY KEY, data BLOB NOT NULL) WITHOUT ROWID;
                 CREATE TABLE IF NOT EXISTS usage (id INTEGER PRIMARY KEY, bytes INTEGER NOT NULL);
                 INSERT OR IGNORE INTO usage VALUES (1, 0);",
                MAX_BYTES / PAGE_SIZE
            ))?;
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
            let cache = Self(db);
            cache.record_upload(0)?;
            Ok(cache)
        };
        match open() {
            Ok(cache) => Ok(cache),
            Err(_) => {
                // An interrupted cache write may damage the cache; signed remote trees survive.
                if path.exists() {
                    std::fs::remove_file(&path)?;
                }
                open()
            }
        }
    }

    pub(super) fn record_upload(&self, bytes: u64) -> Result<()> {
        let previous: u64 = self.0.query_row("SELECT bytes FROM usage WHERE id=1", [], |row| row.get(0))?;
        let total = previous.saturating_add(bytes).min(i64::MAX as u64);
        let limit = (total / 32).clamp(BASE_BYTES as u64, MAX_BYTES as u64) / PAGE_SIZE as u64;
        self.0.pragma_update(None, "max_page_count", limit)?;
        self.0.execute("UPDATE usage SET bytes=?1 WHERE id=1", [total])?;
        Ok(())
    }

    pub(super) fn put(&mut self, tree: &Tree) -> Option<String> {
        match self.insert(tree) {
            Ok(hash) => Some(hash),
            Err(_) => {
                // Evict this generation on capacity or corruption. Old roots become cache misses.
                // DELETE reuses pages without growing the file or requiring a large VACUUM copy.
                self.0.execute("DELETE FROM nodes", []).ok()?;
                self.insert(tree).ok()
            }
        }
    }

    fn insert(&mut self, tree: &Tree) -> Result<String> {
        let mut dirs: BTreeMap<&str, Node> = BTreeMap::from([("", Node::default())]);
        for (path, entry) in &tree.files {
            let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
            dirs.entry(parent).or_default().files.insert(name.to_string(), entry.clone());
            let mut ancestor = parent;
            while !ancestor.is_empty() {
                ancestor = ancestor.rsplit_once('/').map_or("", |(parent, _)| parent);
                dirs.entry(ancestor).or_default();
            }
        }
        let tx = self.0.transaction()?;
        // A directory sorts before its children; reverse order publishes children first.
        let mut root = None;
        while let Some((path, node)) = dirs.pop_last() {
            let bytes = serde_json::to_vec(&node)?;
            let hash = Sha256::digest(&bytes);
            let exists = tx.query_row("SELECT 1 FROM nodes WHERE hash=?1", [hash.as_slice()], |_| Ok(())).optional()?.is_some();
            if !exists {
                tx.execute("INSERT INTO nodes VALUES (?1, ?2)", params![hash.as_slice(), zst::compress(&bytes, None)?])?;
            }
            let hash = hex::encode(hash);
            if path.is_empty() {
                root = Some(hash);
            } else {
                let (parent, name) = path.rsplit_once('/').unwrap_or(("", path));
                dirs.get_mut(parent).unwrap().dirs.insert(name.to_string(), hash);
            }
        }
        tx.commit()?;
        Ok(root.unwrap())
    }

    pub(super) fn get(&self, root: &str) -> Result<Tree> {
        let mut tree = Tree::default();
        let mut pending = vec![(String::new(), root.to_string())];
        let mut visited = BTreeSet::new();
        while let Some((prefix, hash)) = pending.pop() {
            if !visited.insert((prefix.clone(), hash.clone())) || pending.len() > 1_000_000 || prefix.len() > 4096 {
                bail!("invalid local tree cache graph");
            }
            let key = hex::decode(&hash)?;
            if key.len() != 32 {
                bail!("invalid local tree cache hash");
            }
            let bytes: Vec<u8> = self.0.query_row("SELECT data FROM nodes WHERE hash=?1", [key], |row| row.get(0))?;
            let bytes = zst::decompress_with_limit(&bytes, None, NODE_LIMIT)?;
            if hex::encode(Sha256::digest(&bytes)) != hash {
                bail!("invalid local tree cache node");
            }
            let node: Node = serde_json::from_slice(&bytes)?;
            for name in node.files.keys().chain(node.dirs.keys()) {
                if name.is_empty() || name.contains('/') || name == "." || name == ".." {
                    bail!("invalid local cache name");
                }
            }
            for (name, entry) in node.files {
                tree.files.insert(format!("{prefix}{name}"), entry);
            }
            for (name, child) in node.dirs {
                pending.push((format!("{prefix}{name}/"), child));
            }
        }
        Tree::parse(&tree.canonical()?)?;
        Ok(tree)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn tree(version: usize) -> Tree {
        let mut tree = Tree::default();
        for dir in 0..10 {
            for file in 0..10 {
                let content = if dir == 0 && file == 0 { version } else { dir * 10 + file };
                tree.files.insert(
                    format!("dir{dir}/file{file}"),
                    Entry(
                        "100644".into(),
                        0,
                        hex::encode(Sha256::digest(content.to_le_bytes())),
                        vec![super::super::format::Link(
                            "01a12290-0000-7000-8000-000000000002".into(),
                            "0123abcd".into(),
                            0,
                            0,
                            1,
                            "full".into(),
                        )],
                    ),
                );
            }
        }
        tree
    }
    #[test]
    fn subtrees_share_hashes_and_corruption_is_a_cache_miss() {
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&dir).unwrap();
        let mut cache = TreeCache::open(&dir).unwrap();
        let first = cache.put(&tree(0)).unwrap();
        assert_eq!(cache.get(&first).unwrap(), tree(0));
        let count = || cache.0.query_row("SELECT count(*) FROM nodes", [], |row| row.get::<_, usize>(0)).unwrap();
        assert_eq!(count(), 11);
        assert_eq!(cache.put(&tree(0)).unwrap(), first);
        let second = cache.put(&tree(99)).unwrap();
        assert_eq!(cache.0.query_row("SELECT count(*) FROM nodes", [], |row| row.get::<_, usize>(0)).unwrap(), 13);
        assert_eq!(cache.get(&first).unwrap(), tree(0));
        assert_eq!(cache.get(&second).unwrap(), tree(99));
        cache.0.execute("UPDATE nodes SET data=x'00' WHERE hash=?1", [hex::decode(&second).unwrap()]).unwrap();
        assert!(cache.get(&second).is_err());
        drop(cache);
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn budget_grows_with_payload_and_survives_eviction_and_reopen() {
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&dir).unwrap();
        let cache = TreeCache::open(&dir).unwrap();
        cache.record_upload(64 << 20).unwrap();
        assert_eq!(
            cache.0.pragma_query_value(None, "max_page_count", |row| row.get::<_, usize>(0)).unwrap(),
            (2 << 20) / PAGE_SIZE
        );
        cache.0.execute("DELETE FROM nodes", []).unwrap();
        drop(cache);
        let cache = TreeCache::open(&dir).unwrap();
        assert_eq!(
            cache.0.pragma_query_value(None, "max_page_count", |row| row.get::<_, usize>(0)).unwrap(),
            (2 << 20) / PAGE_SIZE
        );
        cache.record_upload(1 << 30).unwrap();
        assert_eq!(
            cache.0.pragma_query_value(None, "max_page_count", |row| row.get::<_, usize>(0)).unwrap(),
            MAX_BYTES / PAGE_SIZE
        );
        drop(cache);
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn cache_eviction_bounds_disk_and_keeps_the_current_tree() {
        let dir = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&dir).unwrap();
        let mut cache = TreeCache::open(&dir).unwrap();
        let first = cache.put(&tree(0)).unwrap();
        for version in 1..1000 {
            let root = cache.put(&tree(version)).unwrap();
            assert_eq!(cache.get(&root).unwrap(), tree(version));
            assert!(std::fs::metadata(dir.join("push-cache.sqlite3")).unwrap().len() <= BASE_BYTES as u64);
        }
        assert!(cache.get(&first).is_err(), "old generation should have been evicted");
        drop(cache);
        std::fs::remove_dir_all(dir).unwrap();
    }
}
