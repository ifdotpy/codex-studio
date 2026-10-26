//! Read real CLI-created indexes through the wasm-safe readers and shared model.
#[path = "support/store.rs"]
mod fixture;
use anyhow::{ensure, Context, Result};
use fixture::*;
use layr_model::{index as model_index, Model};
use layr_store::{format, index, keys, reader};
use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    ensure!(args.len() == 5, "usage: index_golden <store> <paper> <out> <state> <path>");
    let out = PathBuf::from(&args[2]);
    fs::create_dir_all(&out)?;
    let objects = objects(&PathBuf::from(&args[0]))?;
    let descriptor: format::Descriptor = serde_json::from_slice(get(&objects, "store.json")?)?;
    let paper = keys::identity_from_paper(&fs::read_to_string(&args[1])?)?;
    let identity = keys::open_slot(get(&objects, "slots/paper.age")?, &paper)?;
    let (lines, trusted) = records(&objects, &identity, &descriptor.project, None)?;
    let mut snapshots = Vec::new();
    let mut paths = BTreeMap::new();
    let mut graphs = BTreeMap::new();
    for (name, bytes) in objects.iter().filter(|(name, _)| name.starts_with("index/")) {
        let object = index::open(&identity, bytes, name, &descriptor.project, &trusted)?;
        match &object.data {
            index::Data::Heads(_) => {
                model_index::reconcile(&object, &lines)?;
                snapshots.push(object);
            }
            index::Data::Paths(p) => {
                paths.insert(p.state.clone(), p.clone());
            }
            index::Data::Graph(g) => {
                graphs.insert(g.state.clone(), g.clone());
            }
        }
    }
    let newest = index::newest(&snapshots)?.context("no heads snapshot")?;
    let loaded = model_index::load_or_full(Ok(newest.clone()), &descriptor.project, &lines)?;
    ensure!(loaded.used_index, "index was not used");
    let model = Model::from_verified_lines(lines.iter().map(String::as_str), "")?;
    let project = model.projects.get(&descriptor.project).context("missing project")?;
    let mut source = store(objects, identity, trusted);
    let mut attempts = BTreeMap::new();
    for name in source.objects.keys() {
        if let Some((state, attempt)) = format::parse_tree_name(name) {
            attempts.entry(state).or_insert(attempt);
        }
    }
    let history = model_index::History { project, paths: &paths, graphs: &graphs };
    let states = history.file_history(&args[3], &args[4])?;
    if !project.state(&args[3]).context("missing starting state")?.parents.is_empty() {
        let older = history.file_history_from(&args[3], &args[4], 1)?;
        fs::write(out.join("history-older.json"), serde_json::to_vec(&older)?)?;
    }
    // Compare every path-cache answer to the authenticated tree before displaying it.
    for state in project.states.values().map(|s| &s.state) {
        if let Some(attempt) = attempts.get(&state.id) {
            let tree = source.tree(&state.id, attempt)?.1;
            let parent = match state.parents.first() {
                Some(id) => source.tree(id, attempts.get(id).context("missing parent tree")?)?.1,
                None => format::Tree::default(),
            };
            history.check_paths(&state.id, &parent, &tree)?;
        }
    }
    fs::write(out.join("history.json"), serde_json::to_vec(&states)?)?;
    let mut read_states = Vec::new();
    let blame = history.blame(&args[3], &args[4], |state, path| {
        let tree = source.tree(state, attempts.get(state).context("missing file state tree")?)?.1;
        read_states.push(state.to_string());
        tree.files.get(path).map(|e| reader::file(&source.identity, e, &source.objects)).transpose()
    })?;
    fs::write(out.join("blame.json"), serde_json::to_vec(&blame)?)?;
    fs::write(out.join("blob-reads.json"), serde_json::to_vec(&read_states)?)?;
    Ok(())
}
