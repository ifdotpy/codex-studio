//! Bound restored cache metadata independently of the mutable main working tree.
use super::*;

pub(super) fn include(plan: &mut Plan, project: &Path, destination: &Path) -> Result<()> {
    let mut largest: BTreeMap<PathBuf, Plan> = BTreeMap::new();
    for tree in fs::read_dir(project.join("states"))? {
        let tree = tree?;
        let name = tree.file_name();
        let Some((state, cache)) = name.to_str().and_then(|name| name.split_once('@')) else {
            continue;
        };
        if state.len() != 36 || uuid::Uuid::parse_str(state).is_err() {
            continue;
        }
        let path = PathBuf::from(cache.replace("%2F", "/"));
        if path.as_os_str().is_empty()
            || path
                .components()
                .any(|part| !matches!(part, std::path::Component::Normal(_)))
        {
            return Err(NativeLayrError::Refused("Invalid saved cache path".into()));
        }
        let candidate = Plan::existing_main(&tree.path(), destination)?;
        if largest
            .get(&path)
            .is_none_or(|current| current.size.entries < candidate.size.entries)
        {
            largest.insert(path, candidate);
        }
    }
    // A line can restore at most one snapshot per cache path. Count the largest local candidate.
    for (path, mut cache) in largest {
        cache.size.cache_entries = cache.size.entries;
        cache.size.cache_bytes = cache.size.logical_bytes;
        cache.size.tracked_bytes = 0;
        for entry in &mut cache.entries {
            entry.relative = path.join(&entry.relative);
            entry.cache = true;
        }
        plan.size.include(cache.size);
        plan.entries.extend(cache.entries);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn review2_saved_cache_size_uses_largest_tree_per_path() {
        let root = tempfile::tempdir().expect("fixture operation succeeds");
        let main = root.path().join("main");
        fs::create_dir(&main).expect("fixture operation succeeds");
        let mut plan = Plan::existing_main(&main, root.path()).expect("fixture operation succeeds");
        for (state, path, count) in [
            ("aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa", "node_modules", 10),
            ("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "node_modules", 30),
            ("bbbbbbbb-bbbb-4bbb-8bbb-bbbbbbbbbbbb", "sub%2Ftarget", 7),
        ] {
            let tree = root.path().join("states").join(format!("{state}@{path}"));
            fs::create_dir_all(&tree).expect("fixture operation succeeds");
            for i in 0..count {
                fs::write(tree.join(format!("file-{i}")), "cache")
                    .expect("fixture operation succeeds");
            }
        }
        include(&mut plan, root.path(), root.path()).expect("fixture operation succeeds");
        assert_eq!(plan.size.cache_entries, 39);
        assert_eq!(plan.size.entries, 40);
        assert!(
            plan.entries
                .iter()
                .any(|entry| entry.relative == Path::new("sub/target/file-6"))
        );
    }
}
