//! Native test fixture reader, shared by the model and index CLI goldens.
use anyhow::{ensure, Context, Result};
use ed25519_dalek::VerifyingKey;
use layr_model::Record;
use layr_store::{format, keys, reader, record};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

pub(crate) type Objects = BTreeMap<String, Vec<u8>>;
pub(crate) type DecodedTree = (Vec<u8>, format::Tree);

pub(crate) fn objects(root: &Path) -> Result<Objects> {
    fn visit(root: &Path, dir: &Path, out: &mut Objects) -> Result<()> {
        for entry in fs::read_dir(dir)? {
            let entry = entry?;
            let kind = entry.file_type()?;
            if kind.is_dir() {
                visit(root, &entry.path(), out)?;
            } else {
                ensure!(kind.is_file(), "store contains a non-file object");
                let path = entry.path();
                let name = path.strip_prefix(root)?.to_str().context("non-UTF-8 store path")?.replace('\\', "/");
                out.insert(name, fs::read(path)?);
            }
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(root, root, &mut out)?;
    Ok(out)
}

pub(crate) fn get<'a>(objects: &'a Objects, name: &str) -> Result<&'a [u8]> {
    objects.get(name).map(Vec::as_slice).with_context(|| format!("missing store object {name}"))
}

/// Check segment addresses, signatures, sequence continuity, and project bindings.
pub(crate) fn records(
    objects: &Objects,
    identity: &str,
    project: &str,
    trusted: Option<&BTreeMap<String, VerifyingKey>>,
) -> Result<(Vec<String>, BTreeMap<String, VerifyingKey>)> {
    let mut machines: BTreeMap<String, BTreeMap<u64, String>> = BTreeMap::new();
    for (name, bytes) in objects.iter().filter(|(name, _)| name.starts_with("records/")) {
        let (machine, first, last) = format::parse_segment_name(name).context("invalid segment name")?;
        let plain = String::from_utf8(keys::decrypt(identity, bytes)?)?;
        let mut expected = first;
        for line in plain.lines() {
            let raw = record::parse_line(line, None)?;
            let rec: Record = serde_json::from_str(raw.get())?;
            ensure!(rec.machine == machine && rec.seq == expected && expected <= last, "invalid segment sequence");
            ensure!(rec.project == project, "record belongs to another project");
            let by_seq = machines.entry(machine.clone()).or_default();
            if let Some(old) = by_seq.insert(expected, line.into()) {
                ensure!(old == line, "conflicting record segment");
            }
            expected = expected.checked_add(1).context("record sequence overflow")?;
        }
        ensure!(expected.checked_sub(1) == Some(last), "incomplete segment");
    }
    ensure!(!machines.is_empty(), "store has no records");
    let mut verified = Vec::new();
    let mut public_keys = BTreeMap::new();
    for (machine, by_seq) in machines {
        let first = by_seq.get(&1).context("missing machine announcement")?;
        let raw = record::parse_line(first, None)?;
        let rec: Record = serde_json::from_str(raw.get())?;
        ensure!(rec.op == "machine", "first record is not a machine announcement");
        let key = record::machine_key(raw)?;
        if let Some(trusted) = trusted {
            ensure!(trusted.get(&machine) == Some(&key), "overlay machine is not trusted");
        }
        for (index, (seq, line)) in by_seq.into_iter().enumerate() {
            ensure!(seq == index as u64 + 1, "record history has a gap");
            record::parse_line(&line, Some(&key))?;
            verified.push(line);
        }
        public_keys.insert(machine, key);
    }
    Ok((verified, public_keys))
}

pub(crate) struct Store {
    pub(crate) objects: Objects,
    pub(crate) identity: String,
    pub(crate) keys: BTreeMap<String, VerifyingKey>,
    pub(crate) trees: BTreeMap<(String, String), DecodedTree>,
    pub(crate) pending: BTreeSet<(String, String)>,
    pub(crate) metadata: BTreeMap<String, format::Tree>,
}

impl Store {
    pub(crate) fn tree(&mut self, state: &str, attempt: &str) -> Result<DecodedTree> {
        let address = (state.to_string(), attempt.to_string());
        if let Some(tree) = self.trees.get(&address) {
            return Ok(tree.clone());
        }
        ensure!(self.pending.len() < format::FULL_TREE_EVERY, "tree chain exceeds the store limit");
        ensure!(self.pending.insert(address.clone()), "tree chain has a cycle");
        let object = get(&self.objects, &format::tree_name(state, attempt))?.to_vec();
        let plain = keys::decrypt(&self.identity, &object)?;
        let (header, _) = format::split_tree(&plain)?;
        ensure!(header.state == state && header.attempt == attempt, "tree address differs from its header");
        let base = match &header.base {
            Some((state, attempt)) => Some(self.tree(state, attempt)?.0),
            None => None,
        };
        let key = self.keys.get(&header.machine).context("unknown tree signer")?;
        let (_, canonical, tree) = reader::tree(&self.identity, &object, base.as_deref(), key)?;
        self.pending.remove(&address);
        self.trees.insert(address, (canonical.clone(), tree.clone()));
        Ok((canonical, tree))
    }

    pub(crate) fn states(&mut self) -> Result<BTreeMap<String, BTreeMap<String, Vec<u8>>>> {
        let mut attempts = BTreeMap::new();
        for name in self.objects.keys().filter(|name| name.starts_with("states/") && name.ends_with(".tree.age")) {
            let (state, attempt) = format::parse_tree_name(name).context("invalid tree name")?;
            attempts.entry(state).or_insert(attempt);
        }
        let mut states = BTreeMap::new();
        for (state, attempt) in attempts {
            let (_, tree) = self.tree(&state, &attempt)?;
            self.metadata.insert(state.clone(), tree.clone());
            let mut files = BTreeMap::new();
            for (path, entry) in tree.files {
                files.insert(path, reader::file(&self.identity, &entry, &self.objects)?);
            }
            states.insert(state, files);
        }
        Ok(states)
    }
}

pub(crate) fn store(objects: Objects, identity: String, keys: BTreeMap<String, VerifyingKey>) -> Store {
    Store { objects, identity, keys, trees: BTreeMap::new(), pending: BTreeSet::new(), metadata: BTreeMap::new() }
}
