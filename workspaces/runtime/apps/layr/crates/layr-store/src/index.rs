//! Disposable, encrypted indexes. Keys supplied here must already be trusted by the caller.
//! Authentication is separate from reconciliation with authoritative signed records.
use crate::{canonical, keys, record};
use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Watermark {
    pub seq: u64,
    /// SHA-256 of the exact signed JSON line, excluding its newline delimiter.
    pub hash: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Line {
    pub name: String,
    pub id: String,
    pub owner: u32,
    pub machine: String,
    pub head: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ref {
    pub name: String,
    pub target: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Change {
    pub id: String,
    pub state: String,
    pub title: String,
    pub author: serde_json::Value,
    pub time: i64,
}
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Counts {
    pub states: u64,
    pub records: u64,
    pub overlays: u64,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Heads {
    /// Per-writer snapshot sequence, independent of record sequences.
    pub seq: u64,
    pub watermark: BTreeMap<String, Watermark>,
    pub lines: Vec<Line>,
    pub refs: Vec<Ref>,
    pub changes: Vec<Change>,
    pub counts: Counts,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PathChange {
    pub path: String,
    pub old: Option<String>,
    pub new: Option<String>,
    /// New mode, or the old mode for a deletion.
    pub mode: String,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Paths {
    pub state: String,
    pub paths: Vec<PathChange>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Graph {
    pub state: String,
    /// Ancestors at distances 1, 2, 4, 8, ... on the first-parent chain.
    pub ancestors: Vec<String>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "lowercase")]
pub enum Data {
    Heads(Heads),
    Paths(Paths),
    Graph(Graph),
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Index {
    pub v: u32,
    pub project: String,
    pub machine: String,
    pub data: Data,
}

pub fn line_hash(line: &str) -> String {
    hex::encode(Sha256::digest(line.as_bytes()))
}
fn id(value: &str) -> Result<()> {
    ensure!(uuid::Uuid::parse_str(value).is_ok_and(|v| v.to_string() == value), "invalid index identity");
    Ok(())
}
fn hash(value: &str) -> bool {
    value.len() == 64 && value.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c))
}
pub fn heads_name(machine: &str, seq: u64) -> String {
    format!("index/heads/{machine}/{seq:020}.age")
}
pub fn parse_heads_name(name: &str) -> Option<(String, u64)> {
    let (machine, seq) = name.strip_prefix("index/heads/")?.split_once('/')?;
    let seq = seq.strip_suffix(".age")?;
    if id(machine).is_err() || seq.len() != 20 || !seq.bytes().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some((machine.into(), seq.parse().ok()?))
}
pub fn paths_name(state: &str) -> String {
    format!("index/paths/{state}.age")
}
pub fn graph_name(state: &str) -> String {
    format!("index/graph/{state}.age")
}
impl Index {
    pub fn name(&self) -> String {
        match &self.data {
            Data::Heads(h) => heads_name(&self.machine, h.seq),
            Data::Paths(p) => paths_name(&p.state),
            Data::Graph(g) => graph_name(&g.state),
        }
    }
    pub fn validate(&self) -> Result<()> {
        ensure!(self.v == 1, "unsupported index version");
        id(&self.project)?;
        id(&self.machine)?;
        match &self.data {
            Data::Heads(h) => {
                ensure!(h.seq > 0 && !h.watermark.is_empty(), "empty index watermark or sequence");
                for (machine, mark) in &h.watermark {
                    id(machine)?;
                    ensure!(mark.seq > 0 && hash(&mark.hash), "invalid index watermark");
                }
                let mut ids = BTreeSet::new();
                for l in &h.lines {
                    id(&l.id)?;
                    id(&l.machine)?;
                    id(&l.head)?;
                    ensure!(ids.insert(&l.id), "duplicate index line");
                }
                let mut names = BTreeSet::new();
                for r in &h.refs {
                    id(&r.target)?;
                    ensure!(names.insert(&r.name), "duplicate index ref");
                }
                let mut ids = BTreeSet::new();
                for c in &h.changes {
                    id(&c.id)?;
                    id(&c.state)?;
                    ensure!(ids.insert(&c.id), "duplicate index change");
                }
            }
            Data::Paths(p) => {
                id(&p.state)?;
                let mut paths = BTreeSet::new();
                for p in &p.paths {
                    ensure!(crate::format::safe_path(&p.path), "invalid index path");
                    ensure!(paths.insert(&p.path), "duplicate index path");
                    ensure!(p.old.is_some() || p.new.is_some(), "empty path change");
                    ensure!(p.old.iter().chain(&p.new).all(|h| hash(h)), "invalid index blob hash");
                    ensure!(matches!(p.mode.as_str(), "100644" | "100755" | "120000"), "invalid index mode");
                }
            }
            Data::Graph(g) => {
                id(&g.state)?;
                ensure!(g.ancestors.len() <= 64, "index graph exceeds sequence range");
                let mut seen = BTreeSet::from([&g.state]);
                for ancestor in &g.ancestors {
                    id(ancestor)?;
                    ensure!(seen.insert(ancestor), "index graph has a cycle");
                }
            }
        }
        Ok(())
    }
    /// Uses the same exact-byte signature envelope and age encryption as store records.
    pub fn seal(&self, recipient: &str, key: &SigningKey) -> Result<Vec<u8>> {
        self.validate()?;
        let body = canonical::canonical(&serde_json::to_value(self)?)?;
        let sig = keys::sign_bytes(&body, key);
        let body = String::from_utf8(body)?;
        keys::encrypt(recipient, format!(r#"{{"r":{body},"s":"{sig}"}}"#).as_bytes())
    }
}

/// Decrypt, authenticate with a *trusted* machine key, and bind the object to its address.
/// This does not establish that derived data agrees with records; reconcile before use.
pub fn open(identity: &str, object: &[u8], name: &str, project: &str, trusted: &BTreeMap<String, VerifyingKey>) -> Result<Index> {
    let plain = keys::decrypt(identity, object)?;
    #[derive(Deserialize)]
    struct Signer {
        machine: String,
    }
    let raw = record::parse_line(std::str::from_utf8(&plain)?, None)?;
    let signer: Signer = serde_json::from_str(raw.get())?;
    let key = trusted.get(&signer.machine).context("index signer is not trusted")?;
    let raw = record::verify_line(&plain, key)?;
    let index: Index = serde_json::from_str(raw.get())?;
    index.validate()?;
    ensure!(index.project == project, "index belongs to another project");
    ensure!(index.name() == name, "index identity differs from its object name");
    Ok(index)
}

impl Heads {
    /// Check exact watermark lines already authenticated and authorized by the caller.
    pub fn check_watermark(&self, mut verified_line: impl FnMut(&str, u64) -> Option<String>) -> Result<()> {
        for (machine, mark) in &self.watermark {
            let line = verified_line(machine, mark.seq).context("index watermark record is missing")?;
            ensure!(line_hash(&line) == mark.hash, "index watermark hash differs from its record");
        }
        Ok(())
    }
    pub fn dominates(&self, other: &Self) -> bool {
        other.watermark.iter().all(|(machine, mark)| self.watermark.get(machine).is_some_and(|v| v.seq > mark.seq || v == mark))
    }
    fn same_view(&self, other: &Self) -> bool {
        self.watermark == other.watermark
            && self.lines == other.lines
            && self.refs == other.refs
            && self.changes == other.changes
            && self.counts == other.counts
    }
}

/// Select by watermark, never by machine clocks. Concurrent incomparable snapshots
/// cannot establish a newest cache and require the authoritative full load.
pub fn newest(indexes: &[Index]) -> Result<Option<&Index>> {
    if indexes.is_empty() {
        return Ok(None);
    }
    for candidate in indexes {
        let Data::Heads(heads) = &candidate.data else { bail!("expected heads snapshot") };
        if indexes.iter().all(|other| matches!(&other.data, Data::Heads(h) if heads.dominates(h))) {
            for other in indexes {
                let Data::Heads(h) = &other.data else { unreachable!() };
                ensure!(candidate.project == other.project, "index snapshot projects differ");
                if heads.watermark == h.watermark {
                    ensure!(heads.same_view(h), "index snapshots disagree at the same watermark; load signed records");
                }
            }
            return Ok(Some(candidate));
        }
    }
    bail!("index watermarks conflict; load signed records")
}

#[cfg(test)]
mod tests {
    use super::*;
    const MACHINE: &str = "01a12290-0000-7000-8000-000000000001";
    const PROJECT: &str = "01a12290-0000-7000-8000-000000000002";
    fn snapshot(seq: u64, watermark: u64) -> Index {
        Index {
            v: 1,
            project: PROJECT.into(),
            machine: MACHINE.into(),
            data: Data::Heads(Heads {
                seq,
                watermark: BTreeMap::from([(MACHINE.into(), Watermark { seq: watermark, hash: "a".repeat(64) })]),
                lines: vec![],
                refs: vec![],
                changes: vec![],
                counts: Counts::default(),
            }),
        }
    }
    #[test]
    fn encrypted_exact_byte_signatures_bind_project_address_and_trusted_signer() {
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let key = SigningKey::from_bytes(&[7; 32]);
        let trusted = BTreeMap::from([(MACHINE.into(), key.verifying_key())]);
        let index = snapshot(1, 4);
        let object = index.seal(&recipient, &key).unwrap();
        assert_eq!(open(&identity, &object, &index.name(), PROJECT, &trusted).unwrap(), index);
        assert!(open(&identity, &object, &index.name(), MACHINE, &trusted).is_err());
        assert!(open(&identity, &object, &heads_name(MACHINE, 2), PROJECT, &trusted).is_err());
        assert!(open(&identity, &object, &index.name(), PROJECT, &BTreeMap::new()).is_err());
        let changed = keys::decrypt(&identity, &object).unwrap();
        let changed = String::from_utf8(changed).unwrap().replace("\"states\":0", "\"states\":9");
        let changed = keys::encrypt(&recipient, changed.as_bytes()).unwrap();
        assert!(open(&identity, &changed, &index.name(), PROJECT, &trusted).is_err());
        let wrong_key = index.seal(&recipient, &SigningKey::from_bytes(&[8; 32])).unwrap();
        assert!(open(&identity, &wrong_key, &index.name(), PROJECT, &trusted).is_err());
    }
    #[test]
    fn newest_uses_watermark_and_declines_incomparable_snapshots() {
        let values = [snapshot(100, 3), snapshot(1, 4)];
        assert_eq!(newest(&values).unwrap(), Some(&values[1]));
        let mut fork = snapshot(2, 3);
        let Data::Heads(h) = &mut fork.data else { unreachable!() };
        h.watermark.insert(PROJECT.into(), Watermark { seq: 1, hash: "b".repeat(64) });
        assert!(newest(&[values[1].clone(), fork]).is_err());
        assert!(newest(&[]).unwrap().is_none());
        let mut wrong_view = snapshot(2, 4);
        let Data::Heads(h) = &mut wrong_view.data else { unreachable!() };
        h.counts.states = 1;
        assert!(newest(&[values[1].clone(), wrong_view]).is_err());
    }
    #[test]
    fn malformed_paths_graphs_versions_and_addresses_are_refused() {
        let mut index = snapshot(1, 1);
        index.v = 2;
        assert!(index.validate().is_err());
        index.v = 1;
        index.data = Data::Paths(Paths {
            state: PROJECT.into(),
            paths: vec![PathChange { path: "../secret".into(), old: None, new: Some("a".repeat(64)), mode: "100644".into() }],
        });
        assert!(index.validate().is_err());
        index.data = Data::Graph(Graph { state: PROJECT.into(), ancestors: vec![PROJECT.into()] });
        assert!(index.validate().is_err());
        assert_eq!(parse_heads_name(&heads_name(MACHINE, 10)), Some((MACHINE.into(), 10)));
        assert!(parse_heads_name(&format!("index/heads/{MACHINE}/1.age")).is_none());
    }
}
