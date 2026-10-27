//! Built files bound to an exact release ref and state by a signed record.
use crate::{validation, Project};
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

pub const MAX_FILE_BYTES: u64 = layr_store::format::RELEASE_FILE_LIMIT;
pub const MAX_FILES: usize = 64;
pub const WEB_MAX_FILE_BYTES: u64 = 256 << 20;
/// Age adds one authentication tag per 64 KiB chunk and a small header.
pub const MAX_OBJECT_BYTES: u64 = layr_store::format::RELEASE_OBJECT_LIMIT;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct File {
    pub name: String,
    pub size: u64,
    pub sha256: String,
    pub object: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Files {
    pub reference: String,
    pub state: String,
    pub files: Vec<File>,
}

pub fn object(key: &str) -> bool {
    layr_store::format::release_file(key)
}

pub fn filename(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.chars().any(|c| c.is_control() || matches!(c, '/' | '\\' | ':'))
        && !name.ends_with([' ', '.'])
}

impl Files {
    pub fn validate(&self) -> Result<()> {
        if !self.reference.starts_with("release/")
            || !validation::is_ref(&self.reference)
            || !validation::is_uuid(&self.state)
            || self.files.len() > MAX_FILES
        {
            bail!("invalid release files ref, state or count (maximum {MAX_FILES})");
        }
        let mut names = BTreeSet::new();
        let mut objects = BTreeSet::new();
        for file in &self.files {
            if !filename(&file.name)
                || !names.insert(file.name.to_lowercase())
                || !objects.insert(&file.object)
                || file.size > MAX_FILE_BYTES
                || !object(&file.object)
                || file.sha256.len() != 64
                || !file.sha256.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
            {
                bail!("invalid release file name, size, digest or object: {}", file.name);
            }
        }
        Ok(())
    }
}

impl Project {
    /// A moved, deleted or expired ref cannot expose a stale attachment list.
    pub fn release_files(&self, name: &str, now: i64) -> Result<Vec<File>> {
        let Some(reference) = self.ref_list(name, now).into_iter().find(|r| r.name == name) else {
            return Ok(vec![]);
        };
        for record in self.records.iter().rev().filter(|r| r.op == "release.files") {
            let files: Files = serde_json::from_value(record.data.clone())?;
            files.validate()?;
            if files.reference == name && files.state == reference.target {
                return Ok(files.files);
            }
        }
        Ok(vec![])
    }

    /// Historical lists stay reachable for the lifetime of their release ref.
    pub fn release_objects(&self, now: i64) -> Result<(BTreeSet<String>, BTreeSet<String>)> {
        let refs: BTreeSet<_> = self.ref_list("release/", now).into_iter().map(|r| r.name).collect();
        let mut live = BTreeSet::new();
        let mut known = BTreeSet::new();
        for record in self.records.iter().filter(|r| r.op == "release.files") {
            let files: Files = serde_json::from_value(record.data.clone())?;
            files.validate()?;
            for file in files.files {
                if refs.contains(&files.reference) {
                    live.insert(file.object.clone());
                }
                known.insert(file.object);
            }
        }
        Ok((live, known))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Record, RefChange, RefState};
    use serde_json::json;
    fn id(n: u64) -> String {
        format!("01a12290-0000-7000-8000-{n:012x}")
    }
    fn record(n: u64, op: &str, data: serde_json::Value) -> Record {
        let mut r = Record::new(op);
        r.id = id(n);
        r.machine = id(100);
        r.project = id(200);
        r.seq = n;
        r.time = n as i64;
        r.data = data;
        r
    }
    fn files(n: u64) -> Files {
        Files {
            reference: "release/1.2.0".into(),
            state: id(300),
            files: vec![File {
                name: "layr-linux-arm64".into(),
                size: 12,
                sha256: "a".repeat(64),
                object: format!("files/{}.age", id(n)),
            }],
        }
    }
    fn reference(n: u64, target: Option<&str>) -> Record {
        record(
            n,
            "ref.set",
            json!({"ref_change":RefChange { name:"release/1.2.0".into(), before:None, after:target.map(|t| RefState { name:"release/1.2.0".into(), target:t.into(), expires_at:None }) }}),
        )
    }
    #[test]
    fn latest_exact_list_and_gc_lifetime_follow_the_release_ref() {
        let mut records = vec![
            reference(1, Some(&id(300))),
            record(2, "release.files", json!(files(10))),
            record(3, "release.files", json!(files(11))),
        ];
        let read = |records: Vec<Record>| Project::from_records(&id(200), records, &id(100)).unwrap();
        let p = read(records.clone());
        assert_eq!(p.release_files("release/1.2.0", 20).unwrap(), files(11).files);
        assert_eq!(p.release_objects(20).unwrap().0.len(), 2);
        records.push(reference(4, Some(&id(301))));
        assert!(read(records.clone()).release_files("release/1.2.0", 20).unwrap().is_empty());
        records.push(reference(5, None));
        let (live, known) = read(records).release_objects(20).unwrap();
        assert!(live.is_empty());
        assert_eq!(known.len(), 2);
    }
    #[test]
    fn limits_names_keys_and_digests_are_validated_before_folding() {
        for bad in ["", ".", "..", "../layr", "dir/layr", "dir\\layr", "a\nb", "a:", "a.", "a "] {
            let mut f = files(10);
            f.files[0].name = bad.into();
            assert!(f.validate().is_err(), "{bad:?}");
        }
        let mut f = files(10);
        f.files[0].size = MAX_FILE_BYTES;
        assert!(f.validate().is_ok());
        f.files[0].size += 1;
        assert!(f.validate().is_err());
        let mut f = files(10);
        f.files[0].object = format!("states/{}.age", id(1));
        assert!(f.validate().is_err());
        let mut f = files(10);
        f.files[0].sha256 = "G".repeat(64);
        assert!(f.validate().is_err());
        let mut f = files(10);
        f.files.push(f.files[0].clone());
        assert!(f.validate().is_err());
        let mut f = files(10);
        f.reference = "env/prod".into();
        assert!(Project::from_records(&id(200), vec![record(1, "release.files", json!(f))], &id(100)).is_err());
    }
}
