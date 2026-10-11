//! The objects of one vault (docs/design.md, "Objects"): below `<prefix>/states/`,
//! `<seq>.<attempt>.tree.age` is the tree of one state and `<seq>.<attempt>.<part>.data.age`
//! holds its data frames.

use anyhow::{bail, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

/// The plaintext header line of a tree object.
#[derive(Clone, Debug, Deserialize)]
pub struct Header {
    pub v: u32,
    pub seq: u64,
    pub attempt: String,
    pub state: String,
    pub time: i64,
    /// `full`, or `delta` against the tree of `base`.
    pub tree: String,
    pub base: Option<(u64, String)>,
    pub parts: u32,
    /// The writer's signature (docs/design.md, "Signatures"); old trees have none.
    #[serde(default)]
    pub sig: Option<String>,
}

/// One frame of a file's chain: where it is and whether it is a delta on the next one.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Link(pub u64, pub String, pub u32, pub u64, pub u64, pub String);

impl Link {
    pub fn object(&self) -> String {
        name(self.0, &self.1, Some(self.2))
    }
    pub fn range(&self) -> std::ops::Range<usize> {
        self.3 as usize..(self.3 + self.4) as usize
    }
    pub fn is_delta(&self) -> bool {
        self.5 == "delta"
    }
}

/// A file of a tree: `[mode, size, sha256, chain]`; the chain starts at the newest frame and ends
/// at a full frame.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq)]
pub struct Entry(pub String, pub u64, pub String, pub Vec<Link>);

impl Entry {
    pub fn mode(&self) -> &str {
        &self.0
    }
    pub fn size(&self) -> u64 {
        self.1
    }
    pub fn sha256(&self) -> &str {
        &self.2
    }
    pub fn is_link(&self) -> bool {
        self.0 == "120000"
    }
    pub fn is_executable(&self) -> bool {
        self.0 == "100755"
    }
}

#[derive(Deserialize)]
pub struct Tree {
    pub files: BTreeMap<String, Entry>,
}

/// The object name of a tree (`part` none) or of a data part.
pub fn name(seq: u64, attempt: &str, part: Option<u32>) -> String {
    match part {
        None => format!("{seq:010}.{attempt}.tree.age"),
        Some(p) => format!("{seq:010}.{attempt}.{p:04}.data.age"),
    }
}

/// `(seq, attempt)` of a tree object name.
pub fn parse_tree_name(name: &str) -> Option<(u64, String)> {
    let rest = name.strip_suffix(".tree.age")?;
    let (seq, attempt) = rest.split_once('.')?;
    let ok = seq.len() == 10
        && seq.bytes().all(|b| b.is_ascii_digit())
        && attempt.len() == 8
        && attempt
            .bytes()
            .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    ok.then(|| (seq.parse().ok()).map(|s| (s, attempt.to_string())))
        .flatten()
}

/// The header and the zstd frame of a decrypted tree object.
pub fn split_tree(object: &[u8]) -> Result<(Header, &[u8])> {
    let Some(newline) = object.iter().position(|&b| b == b'\n') else {
        bail!("a tree object has no header line");
    };
    let header: Header = serde_json::from_slice(&object[..newline])?;
    if header.v != 1 {
        bail!("tree format {} is not known", header.v);
    }
    if header.tree != "full" && header.tree != "delta" {
        bail!("tree kind {:?} is not known", header.tree);
    }
    if header.tree == "delta" && header.base.is_none() {
        bail!("a delta tree names no base");
    }
    Ok((header, &object[newline + 1..]))
}
