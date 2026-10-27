//! Derive and reconcile disposable indexes against authenticated project history.
use crate::{history, Project, Record, StateRec};
use anyhow::{bail, ensure, Context, Result};
use layr_store::{format::Tree, index::*};
use std::collections::{BTreeMap, BTreeSet};

/// Watermarks cover original signed line bytes, never reserialized records.
pub fn watermark(lines: &[String]) -> Result<BTreeMap<String, Watermark>> {
    let mut out = BTreeMap::<String, Watermark>::new();
    let mut seen = BTreeMap::new();
    for line in lines {
        let record: Record = serde_json::from_str(layr_store::record::parse_line(line, None)?.get())?;
        let hash = line_hash(line);
        if let Some(old) = seen.insert((record.machine.clone(), record.seq), hash.clone()) {
            ensure!(old == hash, "conflicting index source record");
        }
        let mark = out.entry(record.machine).or_insert(Watermark { seq: 0, hash: String::new() });
        if record.seq > mark.seq {
            *mark = Watermark { seq: record.seq, hash };
        }
    }
    Ok(out)
}

pub fn heads(project: &Project, watermark: BTreeMap<String, Watermark>, seq: u64) -> Result<Heads> {
    let lines = project
        .line_list()
        .into_iter()
        .map(|l| Line { name: l.name, id: l.id, owner: l.owner, machine: l.machine, head: l.head })
        .collect::<Vec<_>>();
    let refs = project.refs.values().map(|r| Ref { name: r.name.clone(), target: r.target.clone() }).collect();
    let mut changes = Vec::new();
    for s in project.open_changes() {
        changes.push(Change {
            id: s.change_id.clone().expect("open change has an identity"),
            state: s.id.clone(),
            title: s.message.lines().next().unwrap_or("").into(),
            author: serde_json::to_value(&s.author)?,
            time: s.time,
        });
    }
    let overlays = project.config.keys().filter(|k| k.starts_with("overlay.") && !k.contains('@')).count() as u64;
    Ok(Heads {
        seq,
        watermark,
        lines,
        refs,
        changes,
        counts: Counts { states: project.states.len() as u64, records: project.records.len() as u64, overlays },
    })
}

/// Reconcile at the exact watermark, even when more recent records have arrived.
/// Any mismatch invalidates the whole index. The caller then uses its full verified model.
pub fn reconcile(index: &Index, verified_lines: &[String]) -> Result<Project> {
    index.validate()?;
    let Data::Heads(snapshot) = &index.data else { bail!("expected heads snapshot") };
    let mut by_seq = BTreeMap::new();
    let mut prefix = Vec::new();
    for line in verified_lines {
        let r: Record = serde_json::from_str(layr_store::record::parse_line(line, None)?.get())?;
        ensure!(r.project == index.project, "index source belongs to another project");
        if snapshot.watermark.get(&r.machine).is_some_and(|m| r.seq <= m.seq) {
            if let Some(old) = by_seq.insert((r.machine.clone(), r.seq), line.clone()) {
                ensure!(old == *line, "conflicting watermark source");
            } else {
                prefix.push(r);
            }
        }
    }
    snapshot.check_watermark(|machine, seq| by_seq.get(&(machine.into(), seq)).cloned())?;
    for (machine, mark) in &snapshot.watermark {
        // Count and endpoints avoid an unbounded loop over an attacker-supplied sequence.
        let sequences: Vec<_> = by_seq.keys().filter(|(m, _)| m == machine).map(|(_, s)| *s).collect();
        ensure!(sequences.len() as u64 == mark.seq && sequences.first() == Some(&1), "index watermark has a gap");
    }
    let project = Project::from_records(&index.project, prefix, "")?;
    let expected = heads(&project, snapshot.watermark.clone(), snapshot.seq)?;
    ensure!(expected == *snapshot, "index snapshot differs from signed records");
    Ok(project)
}

pub struct Loaded {
    pub project: Project,
    pub used_index: bool,
    pub note: Option<String>,
}

/// A native/browser reconciliation boundary: a bad cache never prevents a verified load.
/// Supply the result of `layr_store::index::open`, including authentication failures.
pub fn load_or_full(snapshot: Result<Index>, project: &str, verified_lines: &[String]) -> Result<Loaded> {
    let cached = snapshot.and_then(|snapshot| {
        ensure!(snapshot.project == project, "index project differs");
        reconcile(&snapshot, verified_lines)
    });
    match cached {
        Ok(project) => Ok(Loaded { project, used_index: true, note: None }),
        Err(error) => {
            let model = crate::Model::from_verified_lines(verified_lines.iter().map(String::as_str), "")?;
            let project = model.projects.get(project).context("project has no verified records")?.clone();
            Ok(Loaded { project, used_index: false, note: Some(format!("Index discarded: {error}")) })
        }
    }
}

/// Changed paths are derived from authenticated tree hashes and executable/symlink modes.
pub fn paths(state: &str, parent: &Tree, tree: &Tree) -> Paths {
    let mut paths = Vec::new();
    for path in parent.files.keys().chain(tree.files.keys()).collect::<BTreeSet<_>>() {
        let old = parent.files.get(path);
        let new = tree.files.get(path);
        if old.zip(new).is_some_and(|(a, b)| a.0 == b.0 && a.2 == b.2) {
            continue;
        }
        paths.push(PathChange {
            path: path.clone(),
            old: old.map(|e| e.2.clone()),
            new: new.map(|e| e.2.clone()),
            mode: new.or(old).expect("one entry").0.clone(),
        });
    }
    Paths { state: state.into(), paths }
}

/// Compute powers-of-two ancestors using already built parent graph objects.
pub fn graph(state: &StateRec, mut ancestor_graph: impl FnMut(&str) -> Result<Graph>) -> Result<Graph> {
    let mut ancestors = Vec::new();
    if let Some(parent) = state.parents.first() {
        ancestors.push(parent.clone());
        for power in 0..63 {
            let halfway = ancestor_graph(&ancestors[power])?;
            let Some(next) = halfway.ancestors.get(power) else { break };
            ensure!(next != &state.id && !ancestors.contains(next), "index ancestry cycle");
            ancestors.push(next.clone());
        }
    }
    Ok(Graph { state: state.id.clone(), ancestors })
}

/// Indexed history over caller-supplied objects. Authenticate index objects with layr-store
/// first; validate paths against signed trees and graph pointers against signed states.
pub struct History<'a> {
    pub project: &'a Project,
    pub paths: &'a BTreeMap<String, Paths>,
    pub graphs: &'a BTreeMap<String, Graph>,
}
impl History<'_> {
    pub fn check_paths(&self, state: &str, parent: &Tree, tree: &Tree) -> Result<()> {
        ensure!(self.paths.get(state) == Some(&paths(state, parent, tree)), "index paths differ from signed tree");
        Ok(())
    }
    fn checked_graph(&self, state: &str) -> Result<&Graph> {
        let g = self.graphs.get(state).context("missing index graph")?;
        ensure!(g.state == state, "graph state differs");
        let mut at = self.project.state(state).context("missing state record")?;
        let mut distance = 0usize;
        let mut next_distance = 1usize;
        let mut expected = Vec::new();
        let mut seen = BTreeSet::from([state]);
        while let Some(parent) = at.parents.first() {
            ensure!(seen.insert(parent), "source ancestry cycle");
            at = self.project.state(parent).context("missing ancestor record")?;
            distance += 1;
            if distance == next_distance {
                expected.push(at.id.clone());
                next_distance = next_distance.checked_mul(2).context("ancestry overflow")?;
            }
        }
        ensure!(g.ancestors == expected, "index graph differs from signed records");
        Ok(g)
    }
    /// Locate an ancestor in O(log distance) object lookups. Call after graph reconciliation.
    pub fn ancestor(&self, start: &str, mut distance: u64) -> Result<String> {
        self.checked_graph(start)?;
        let mut at = start.to_string();
        while distance > 0 {
            let bit = 63 - distance.leading_zeros() as usize;
            at = self.checked_graph(&at)?.ancestors.get(bit).context("ancestor is outside history")?.clone();
            distance -= 1 << bit;
        }
        Ok(at)
    }
    pub fn file_history(&self, start: &str, path: &str) -> Result<Vec<StateRec>> {
        self.file_history_from(start, path, 0)
    }
    /// Select an older first-parent range with graph skip pointers before reading its
    /// path objects. Offset counts source states, including states that kept the file.
    pub fn file_history_from(&self, start: &str, path: &str, offset: u64) -> Result<Vec<StateRec>> {
        let start = self.ancestor(start, offset)?;
        let mut out = Vec::new();
        history::file_history(
            &[start],
            &BTreeSet::new(),
            true,
            |id| Ok(self.project.state(id).cloned()),
            |s| {
                let p = self.paths.get(&s.id).context("missing path index")?;
                ensure!(p.state == s.id, "path index state differs");
                Ok(p.paths.iter().map(|p| p.path.clone()).collect())
            },
            |p| p == path,
            |s| {
                out.push(s);
                Ok(true)
            },
        )?;
        Ok(out)
    }
    /// Load only versions of this file, never unrelated blobs. `blob` must verify the
    /// bytes against the authenticated state tree; a deletion terminates attribution.
    pub fn blame(
        &self,
        start: &str,
        path: &str,
        mut blob: impl FnMut(&str, &str) -> Result<Option<Vec<u8>>>,
    ) -> Result<Vec<history::BlameLine>> {
        let mut versions: Vec<history::FileVersion> = Vec::new();
        let mut held = 0usize;
        for s in self.file_history(start, path)? {
            let change = self
                .paths
                .get(&s.id)
                .and_then(|p| p.paths.iter().find(|p| p.path == path))
                .context("missing file path change")?;
            // CLI blame reads regular files without following symlinks. A deletion or
            // symlink bounds that file's regular-file history in either caller.
            if change.new.is_none() || change.mode == "120000" {
                break;
            }
            let Some(bytes) = blob(&s.id, path)? else { break };
            ensure!(bytes.len() <= 64 << 20, "file is larger than 64 MiB");
            if let Some(previous) = versions.last_mut().filter(|v| v.bytes == bytes) {
                previous.state = Some(s);
                continue;
            }
            held += bytes.len();
            if held > 256 << 20 && !versions.is_empty() {
                break;
            }
            versions.push(history::FileVersion { state: Some(s), bytes });
        }
        history::blame(&versions)
    }
}
