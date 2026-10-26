//! Native publisher for the shared, disposable store index format.
use super::{reader, Store};
use crate::repo::Repo;
use anyhow::{ensure, Context, Result};
use layr_store::index::{self, Data, Index};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Default, Debug, Serialize)]
pub struct Report {
    pub heads: usize,
    pub paths: usize,
    pub graphs: usize,
    pub unavailable_paths: Vec<String>,
}

pub(super) fn write(store: &Store, repo: &Repo) -> Result<Report> {
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    repo.p.verify_records()?;
    ensure!(
        repo.p.db.trusted(&store.machine)?.as_deref() == Some(&hex::encode(store.key.verifying_key().to_bytes())),
        "index writer is not a trusted machine"
    );
    let records = reader::records(&store.backend, &identity, &descriptor.project)?;
    ensure!(records.gaps.is_empty(), "cannot index store records with gaps: {}", records.gaps.join(", "));
    ensure!(
        records.keys.get(&store.machine) == Some(&store.key.verifying_key()),
        "push this machine's signed records before writing its index"
    );
    let mut trusted = BTreeMap::new();
    for (machine, key) in &records.keys {
        ensure!(
            repo.p.db.trusted(machine)?.as_deref() == Some(&hex::encode(key.to_bytes())),
            "store machine {machine} is not trusted; fetch and trust its key before indexing"
        );
        trusted.insert(machine.clone(), *key);
    }
    trusted.insert(store.machine.clone(), store.key.verifying_key());
    let model = layr_model::Model::from_verified_lines(records.lines.iter().map(String::as_str), "")?;
    let project = model.projects.get(&descriptor.project).context("store has no project records; push first")?;
    let wrap = |data| Index { v: 1, project: descriptor.project.clone(), machine: store.machine.clone(), data };
    let publish = |index: Index| -> Result<bool> {
        let name = index.name();
        let made = if store.backend.get(&name)?.is_none() {
            store.backend.put_new(&name, index.seal(&descriptor.recipient, &store.key)?)?
        } else {
            false
        };
        let object = store.backend.get(&name)?.context("published index disappeared")?;
        let existing = index::open(&identity, &object, &name, &descriptor.project, &trusted)?;
        ensure!(
            existing.data == index.data,
            "immutable index {name} differs from signed source; remove the invalid cache object"
        );
        Ok(made)
    };
    let mut report = Report::default();
    let mut pending: BTreeMap<_, _> = project.states.iter().map(|(id, s)| (id.clone(), s.state.clone())).collect();
    let mut graphs = BTreeMap::new();
    while !pending.is_empty() {
        let id = pending
            .iter()
            .find(|(_, s)| s.parents.first().is_none_or(|p| graphs.contains_key(p)))
            .map(|(id, _)| id.clone())
            .context("store state ancestry has a gap or cycle")?;
        let state = pending.remove(&id).expect("pending state");
        let graph = layr_model::index::graph(&state, |id| graphs.get(id).cloned().context("missing parent graph"))?;
        report.graphs += usize::from(publish(wrap(Data::Graph(graph.clone())))?);
        graphs.insert(id, graph);
    }
    let attempts = reader::attempts(&store.backend)?;
    let mut source = reader::Reader::new(&store.backend, &identity, &trusted);
    for state in project.states.values().map(|s| &s.state) {
        let Some(attempt) = attempts.get(&state.id) else {
            report.unavailable_paths.push(state.id.clone());
            continue;
        };
        let tree = source.tree(&state.id, attempt)?.2;
        let parent = match state.parents.first() {
            Some(parent) => match attempts.get(parent) {
                Some(attempt) => source.tree(parent, attempt)?.2,
                None => {
                    report.unavailable_paths.push(state.id.clone());
                    continue;
                }
            },
            None => layr_store::format::Tree { files: BTreeMap::new() },
        };
        report.paths += usize::from(publish(wrap(Data::Paths(layr_model::index::paths(&state.id, &parent, &tree))))?);
    }
    let watermark = layr_model::index::watermark(&records.lines)?;
    let mut seq = 0;
    let mut snapshots = Vec::new();
    let mut latest = BTreeMap::<String, (u64, String)>::new();
    for object in store.backend.list("index/heads/")? {
        let (machine, number) = index::parse_heads_name(&object.key).context("invalid heads index name")?;
        if machine == store.machine {
            seq = seq.max(number);
        }
        let entry = latest.entry(machine).or_insert((number, object.key.clone()));
        if number > entry.0 {
            *entry = (number, object.key);
        }
    }
    for (_, name) in latest.values() {
        let body = store.backend.get(name)?.context("heads index disappeared")?;
        let snapshot = index::open(&identity, &body, name, &descriptor.project, &trusted)?;
        // Do not reuse even a correctly signed snapshot with an incorrect derived head.
        layr_model::index::reconcile(&snapshot, &records.lines)?;
        snapshots.push(snapshot);
    }
    let mut heads = layr_model::index::heads(project, watermark, 1)?;
    for snapshot in &snapshots {
        if let Data::Heads(existing) = &snapshot.data {
            heads.seq = existing.seq;
            if *existing == heads {
                return Ok(report);
            }
        }
    }
    loop {
        seq = seq.checked_add(1).context("index snapshot sequence overflow")?;
        heads.seq = seq;
        let index = wrap(Data::Heads(heads.clone()));
        if store.backend.put_new(&index.name(), index.seal(&descriptor.recipient, &store.key)?)? {
            report.heads = 1;
            break;
        }
        let object = store.backend.get(&index.name())?.context("heads index disappeared")?;
        let old = index::open(&identity, &object, &index.name(), &descriptor.project, &trusted)?;
        if old.data == index.data {
            break;
        }
    }
    Ok(report)
}
