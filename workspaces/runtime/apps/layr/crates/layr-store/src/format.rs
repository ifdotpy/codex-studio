//! Store v2 trees. Record segments retain the exact signed JSONL bytes.

use super::canonical::canonical;
use anyhow::{bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// Public store metadata. The recipient identifies the vault encryption key.
#[derive(Clone, Serialize, Deserialize)]
pub struct Descriptor {
    pub format: u32,
    pub project: String,
    pub recipient: String,
}

pub fn is_uuid(s: &str) -> bool {
    s.len() == 36 && uuid::Uuid::parse_str(s).map(|u| u.hyphenated().to_string() == s).unwrap_or(false)
}

pub const PART_LIMIT: usize = 16 << 20;
pub const FULL_TREE_EVERY: usize = 32;
pub const MAX_FILE_DELTAS: usize = 16;
pub const DELTA_LIMIT: usize = 32 << 20;
pub const MAX_PARTS: u32 = 10_000;
const DOMAIN: &[u8] = b"layr/store/v2/tree\0";

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Header {
    pub v: u32,
    pub state: String,
    pub attempt: String,
    pub tree: String,
    pub base: Option<(String, String)>,
    pub parts: u32,
    pub machine: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub btrfs: Option<BtrfsIdentity>,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub sig: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct BtrfsIdentity {
    pub uuid: String,
    pub transid: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Link(pub String, pub String, pub u32, pub u64, pub u64, pub String);

impl Link {
    pub fn object(&self) -> String {
        data_name(&self.0, &self.1, self.2)
    }
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Entry(pub String, pub u64, pub String, pub Vec<Link>);

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct Tree {
    pub files: BTreeMap<String, Entry>,
}

impl Tree {
    pub fn canonical(&self) -> Result<Vec<u8>> {
        canonical(&serde_json::to_value(self)?)
    }
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let tree: Self = serde_json::from_slice(bytes)?;
        if tree.canonical()? != bytes {
            bail!("noncanonical tree");
        }
        for (path, entry) in &tree.files {
            if !safe_path(path) {
                bail!("unsafe tree path {path:?}");
            }
            if !matches!(entry.0.as_str(), "100644" | "100755" | "120000") {
                bail!("invalid file mode");
            }
            if entry.2.len() != 64 || hex::decode(&entry.2).is_err() {
                bail!("invalid SHA-256");
            }
            if entry.3.is_empty() || entry.3.len() > MAX_FILE_DELTAS + 1 {
                bail!("invalid file chain length");
            }
            for (i, link) in entry.3.iter().enumerate() {
                if !is_uuid(&link.0) || !valid_attempt(&link.1) || link.2 >= MAX_PARTS || link.4 == 0 {
                    bail!("invalid frame address");
                }
                let expected = if i + 1 == entry.3.len() { "full" } else { "delta" };
                if link.5 != expected {
                    bail!("file chain must end with one full frame");
                }
                link.3.checked_add(link.4).context("frame range overflow")?;
            }
            // No file can be the parent folder of another file, including a symlink.
            let mut parent = std::path::Path::new(path).parent();
            while let Some(p) = parent {
                if tree.files.contains_key(p.to_str().context("invalid tree path")?) {
                    bail!("tree path has a file parent: {path}");
                }
                parent = p.parent();
            }
        }
        Ok(tree)
    }
}

pub fn safe_path(path: &str) -> bool {
    !path.is_empty()
        && !path.contains(['\\', '\0'])
        && !path.split('/').any(|s| s.is_empty() || s == "." || s == "..")
        && crate::paths::check_rel(path).is_ok()
}

pub fn valid_attempt(attempt: &str) -> bool {
    attempt.len() == 8 && attempt.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

pub fn tree_name(state: &str, attempt: &str) -> String {
    format!("states/{state}/{attempt}.tree.age")
}
pub fn data_name(state: &str, attempt: &str, part: u32) -> String {
    format!("states/{state}/{attempt}.{part:04}.data.age")
}

pub fn parse_tree_name(key: &str) -> Option<(String, String)> {
    let (state, rest) = key.strip_prefix("states/")?.split_once('/')?;
    let attempt = rest.strip_suffix(".tree.age")?;
    (is_uuid(state) && valid_attempt(attempt)).then(|| (state.to_string(), attempt.to_string()))
}

pub fn parse_data_name(key: &str) -> Option<(String, String, u32)> {
    let (state, rest) = key.strip_prefix("states/")?.split_once('/')?;
    let (attempt, part) = rest.strip_suffix(".data.age")?.split_once('.')?;
    if !is_uuid(state) || !valid_attempt(attempt) || part.len() != 4 || !part.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((state.to_string(), attempt.to_string(), part.parse().ok()?))
}

pub fn segment_name(machine: &str, first: u64, last: u64) -> String {
    format!("records/{machine}/{first:020}-{last:020}.age")
}

pub fn parse_segment_name(key: &str) -> Option<(String, u64, u64)> {
    let (machine, range) = key.strip_prefix("records/")?.split_once('/')?;
    let (first, last) = range.strip_suffix(".age")?.split_once('-')?;
    if !is_uuid(machine)
        || first.len() != 20
        || last.len() != 20
        || !first.bytes().all(|b| b.is_ascii_digit())
        || !last.bytes().all(|b| b.is_ascii_digit())
    {
        return None;
    }
    let (first, last): (u64, u64) = (first.parse().ok()?, last.parse().ok()?);
    (first > 0 && first <= last).then(|| (machine.to_string(), first, last))
}

fn signed_bytes(header: &Header, tree: &[u8]) -> Result<Vec<u8>> {
    let mut unsigned = header.clone();
    unsigned.sig.clear();
    let mut bytes = DOMAIN.to_vec();
    bytes.extend(canonical(&serde_json::to_value(unsigned)?)?);
    bytes.push(b'\n');
    bytes.extend(tree);
    Ok(bytes)
}

impl Header {
    pub fn sign(&mut self, tree: &[u8], key: &SigningKey) -> Result<()> {
        self.sig = crate::keys::sign_bytes(&signed_bytes(self, tree)?, key);
        Ok(())
    }
    pub fn verify(&self, tree: &[u8], key: &VerifyingKey) -> Result<()> {
        if self.v != 2 {
            bail!("unsupported tree format {}", self.v);
        }
        if !is_uuid(&self.state)
            || !is_uuid(&self.machine)
            || !valid_attempt(&self.attempt)
            || self.parts > MAX_PARTS
        {
            bail!("invalid tree header");
        }
        if self.btrfs.as_ref().is_some_and(|i| !is_uuid(&i.uuid) || i.transid == 0) {
            bail!("invalid btrfs tree identity");
        }
        match (self.tree.as_str(), &self.base) {
            ("full", None) => {}
            ("delta", Some((state, attempt))) if is_uuid(state) && valid_attempt(attempt) => {}
            _ => bail!("invalid tree base"),
        }
        crate::keys::verify_bytes(&signed_bytes(self, tree)?, &self.sig, key)
    }
}

pub fn split_tree(object: &[u8]) -> Result<(Header, &[u8])> {
    let newline = object.iter().position(|&b| b == b'\n').context("tree has no header line")?;
    if newline > 16384 {
        bail!("tree header exceeds 16 KiB");
    }
    Ok((serde_json::from_slice(&object[..newline])?, &object[newline + 1..]))
}

#[cfg(test)]
mod tests {
    use super::*;
    const ID: &str = "01a12290-0000-7000-8000-000000000001";
    #[test]
    fn signatures_bind_header_tree_and_domain() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let tree = b"{\"files\":{}}";
        let mut header = Header {
            v: 2,
            state: ID.into(),
            attempt: "0123abcd".into(),
            tree: "full".into(),
            base: None,
            parts: 0,
            machine: ID.into(),
            btrfs: None,
            sig: String::new(),
        };
        header.sign(tree, &key).unwrap();
        assert_eq!(header.sig, "yIMZ3tHimsJVUe2aJs7RJkVQr9mPaS8QazZnExdCmtVy2i063+jYI5TERQrvxlqYQsxOHTKk0yLlkvPfHfNiCQ==");
        header.verify(tree, &key.verifying_key()).unwrap();
        header.btrfs = Some(BtrfsIdentity { uuid: ID.into(), transid: 7 });
        header.sign(tree, &key).unwrap();
        assert_eq!(header.sig, "S7eww50UaRLUiyu646IdgZiC53cdiJQ7gTfMrT7cD9FnDeikWWfB3I4VC6LGDsUi7lSkm3ex6DkYBY7XCL+VDg==");
        header.verify(tree, &key.verifying_key()).unwrap();
        header.btrfs.as_mut().unwrap().transid = 8;
        assert!(header.verify(tree, &key.verifying_key()).is_err());
        header.btrfs.as_mut().unwrap().transid = 7;
        assert!(header.verify(b"{}", &key.verifying_key()).is_err());
        header.parts = 1;
        assert!(header.verify(tree, &key.verifying_key()).is_err());
    }
    #[test]
    fn object_names_are_strict() {
        assert_eq!(parse_tree_name(&tree_name(ID, "0123abcd")), Some((ID.into(), "0123abcd".into())));
        assert!(parse_tree_name("states/../0123abcd.tree.age").is_none());
        assert_eq!(parse_segment_name(&segment_name(ID, 1, 12)), Some((ID.into(), 1, 12)));
        for bad in ["../a", "/a", "a//b", "a/./b", "a\0"] {
            assert!(!safe_path(bad));
        }
    }
}
