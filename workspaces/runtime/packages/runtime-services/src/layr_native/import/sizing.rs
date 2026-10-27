//! Allocation estimates follow the exact host version enforced by the installer.
use super::*;

struct HeadFile {
    path: PathBuf,
    bytes: u64,
    oid: String,
    regular: bool,
}
fn git_head(source: &Path) -> Result<Vec<HeadFile>> {
    let output = match super::super::inventory::git(source, &["ls-tree", "-r", "-l", "-z", "HEAD"])
    {
        Ok(output) => output,
        Err(error) if error.to_string().contains("Not a valid object name HEAD") => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(error),
    };
    let mut files = Vec::new();
    for record in output.split(|b| *b == 0).filter(|r| !r.is_empty()) {
        let text = std::str::from_utf8(record)
            .map_err(|_| NativeLayrError::Refused("Non-UTF-8 Git HEAD path".into()))?;
        let (metadata, path) = text
            .split_once('\t')
            .ok_or_else(|| NativeLayrError::Refused("Invalid Git HEAD listing".into()))?;
        let fields: Vec<_> = metadata.split_whitespace().collect();
        if fields.get(1) == Some(&"blob") {
            let bytes = fields
                .get(3)
                .and_then(|s| s.parse().ok())
                .ok_or_else(|| NativeLayrError::Refused("Invalid Git HEAD size".into()))?;
            files.push(HeadFile {
                path: PathBuf::from(path),
                bytes,
                oid: fields[2].to_owned(),
                regular: matches!(fields.first(), Some(&"100644" | &"100755")),
            });
        }
    }
    Ok(files)
}

pub(super) fn git_allocation(
    source: &Path,
    entries: &[Entry],
    ignores: &BTreeSet<PathBuf>,
    version: &str,
    destination_case_sensitive: bool,
) -> Result<u64> {
    let head = git_head(source)?;
    validate_names(
        head.iter().map(|file| file.path.as_path()),
        destination_case_sensitive,
        cfg!(target_os = "macos"),
    )?;
    let selected: BTreeMap<_, _> = entries
        .iter()
        .filter(|entry| entry.metadata.is_file() && !entry.cache)
        .map(|entry| (entry.relative.as_path(), entry))
        .collect();
    if !clones_head(version) {
        // 0.2.1 writes all HEAD blobs, then copies overlapping source files into existing inodes.
        return Ok(head.iter().fold(0u64, |sum, file| {
            sum.saturating_add(file.bytes).saturating_add(
                selected
                    .get(file.path.as_path())
                    .map(|entry| entry.metadata.len())
                    .unwrap_or(0),
            )
        }));
    }
    let candidates: Vec<_> = head
        .iter()
        .filter(|file| {
            file.regular
                && selected
                    .get(file.path.as_path())
                    .is_some_and(|entry| entry.metadata.len() == file.bytes)
                && !(file.path.file_name() == Some(std::ffi::OsStr::new(".layrignore"))
                    && ignores.contains(file.path.parent().unwrap_or(Path::new(""))))
        })
        .collect();
    let mut matching = BTreeSet::new();
    for batch in candidates.chunks(64) {
        let mut args = vec!["hash-object", "--no-filters", "--"];
        for file in batch {
            args.push(
                file.path
                    .to_str()
                    .ok_or_else(|| NativeLayrError::Refused("Non-UTF-8 Git path".into()))?,
            );
        }
        // hash-object has no -w: it reads exact source bytes and writes no Git object or index.
        let hashes = super::super::inventory::git(source, &args)?;
        let hashes: Vec<_> = hashes
            .split(|b| *b == b'\n')
            .filter(|line| !line.is_empty())
            .collect();
        if hashes.len() != batch.len() {
            return Err(NativeLayrError::Refused(
                "Incomplete Git source hash report".into(),
            ));
        }
        for (file, hash) in batch.iter().zip(hashes) {
            if hash == file.oid.as_bytes() {
                matching.insert(file.path.as_path());
            }
        }
    }
    Ok(head
        .iter()
        .filter(|file| !matching.contains(file.path.as_path()))
        .fold(0u64, |sum, file| sum.saturating_add(file.bytes)))
}

fn clones_head(version: &str) -> bool {
    let mut parts = version
        .split('.')
        .filter_map(|part| part.parse::<u64>().ok());
    matches!((parts.next(),parts.next(),parts.next()),(Some(major),Some(minor),Some(patch)) if (major,minor,patch)>=(0,2,2))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn head_clone_contract_starts_at_layr_022() {
        assert!(!clones_head("0.2.1"));
        assert!(clones_head("0.2.2"));
        assert!(!clones_head("unknown"));
    }

    #[test]
    fn allocation_counts_changed_missing_symlink_and_cache_blobs_by_host_version() {
        let source = tempfile::tempdir().expect("source");
        let git = |args: &[&str]| {
            super::super::super::inventory::git(source.path(), args).expect("fixture Git command")
        };
        git(&["init", "-q"]);
        for (name, bytes) in [
            ("same", "same"),
            ("changed", "before"),
            ("missing", "gone"),
            ("executable", "#!/bin/sh\n"),
        ] {
            fs::write(source.path().join(name), bytes).expect("file");
        }
        fs::set_permissions(
            source.path().join("executable"),
            fs::Permissions::from_mode(0o755),
        )
        .expect("executable mode");
        std::os::unix::fs::symlink("same", source.path().join("link")).expect("link");
        fs::create_dir(source.path().join("node_modules")).expect("cache");
        fs::write(source.path().join("node_modules/cached"), "warm").expect("cache file");
        git(&["add", "."]);
        git(&[
            "-c",
            "user.name=Fixture",
            "-c",
            "user.email=fixture@example.test",
            "-c",
            "core.hooksPath=/dev/null",
            "commit",
            "-qm",
            "Base",
        ]);
        fs::write(source.path().join("changed"), "after!!").expect("change");
        fs::remove_file(source.path().join("missing")).expect("delete");
        let mut tree = Selection::default();
        collect_tree(source.path(), Path::new(""), false, false, false, &mut tree)
            .expect("selection");
        assert_eq!(
            git_allocation(
                source.path(),
                &tree.entries,
                &BTreeSet::new(),
                "0.2.1",
                true
            )
            .expect("old estimate"),
            53
        );
        assert_eq!(
            git_allocation(
                source.path(),
                &tree.entries,
                &BTreeSet::new(),
                "0.2.2",
                true
            )
            .expect("clone estimate"),
            18
        );
    }
}
