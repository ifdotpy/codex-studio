//! Read the golden fixture through the shared store reader and the pure model.
use anyhow::{bail, ensure, Context, Result};
use ed25519_dalek::VerifyingKey;
use layr_model::{probes, Model, Project, Record, StateKind, Subject};
use layr_store::{format, keys, reader, record};
use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Deserialize)]
struct Request {
    line: String,
    change: String,
    #[serde(rename = "ref")]
    reference: String,
    attestation_state: String,
    probe_base: String,
    probe_head: String,
    merge_source: String,
    merge_candidate: String,
}

type Objects = BTreeMap<String, Vec<u8>>;
type DecodedTree = (Vec<u8>, format::Tree);

fn objects(root: &Path) -> Result<Objects> {
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

fn get<'a>(objects: &'a Objects, name: &str) -> Result<&'a [u8]> {
    objects.get(name).map(Vec::as_slice).with_context(|| format!("missing store object {name}"))
}

/// Check segment addresses, signatures, sequence continuity, and project bindings.
fn records(
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

struct Store {
    objects: Objects,
    identity: String,
    keys: BTreeMap<String, VerifyingKey>,
    trees: BTreeMap<(String, String), DecodedTree>,
    pending: BTreeSet<(String, String)>,
    metadata: BTreeMap<String, format::Tree>,
}

impl Store {
    fn tree(&mut self, state: &str, attempt: &str) -> Result<DecodedTree> {
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

    fn states(&mut self) -> Result<BTreeMap<String, BTreeMap<String, Vec<u8>>>> {
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

fn store(objects: Objects, identity: String, keys: BTreeMap<String, VerifyingKey>) -> Store {
    Store { objects, identity, keys, trees: BTreeMap::new(), pending: BTreeSet::new(), metadata: BTreeMap::new() }
}

fn write(out: &Path, name: &str, value: impl serde::Serialize) -> Result<()> {
    fs::write(out.join(format!("{name}.json")), serde_json::to_vec_pretty(&value)?)?;
    Ok(())
}

type StateFiles = BTreeMap<String, BTreeMap<String, Vec<u8>>>;

fn text(bytes: &[u8], limit: usize) -> Option<&str> {
    if bytes.len() <= limit && !bytes[..bytes.len().min(8000)].contains(&0) {
        std::str::from_utf8(bytes).ok()
    } else {
        None
    }
}

fn file_diff(base: &str, head: &str, metadata: &BTreeMap<String, format::Tree>, files: &StateFiles) -> Result<Value> {
    let old = metadata.get(base).context("change base tree is unavailable")?;
    let new = metadata.get(head).context("change head tree is unavailable")?;
    let mut rows = Vec::new();
    for path in old.files.keys().chain(new.files.keys()).collect::<BTreeSet<_>>() {
        let a = old.files.get(path);
        let b = new.files.get(path);
        if a.zip(b).is_some_and(|(a, b)| a.0 == b.0 && a.1 == b.1 && a.2 == b.2) {
            continue;
        }
        let before = a.map(|_| files.get(base).and_then(|f| f.get(path)).context("missing verified base file")).transpose()?;
        let after = b.map(|_| files.get(head).and_then(|f| f.get(path)).context("missing verified head file")).transpose()?;
        let status = if a.is_none() {
            "add"
        } else if b.is_none() {
            "remove"
        } else {
            "modify"
        };
        let mut row = json!({"path":path,"status":status,"added":0,"removed":0});
        let old_text = before.map(|v| text(v, 64 << 20));
        let new_text = after.map(|v| text(v, 64 << 20));
        if old_text.into_iter().chain(new_text).all(|v| v.is_some()) {
            let (before, after) = (old_text.flatten().unwrap_or(""), new_text.flatten().unwrap_or(""));
            let delta = similar::TextDiff::from_lines(before, after);
            let added = delta.iter_all_changes().filter(|c| c.tag() == similar::ChangeTag::Insert).count();
            let removed = delta.iter_all_changes().filter(|c| c.tag() == similar::ChangeTag::Delete).count();
            let from = if a.is_none() { "/dev/null".into() } else { format!("a/{path}") };
            let to = if b.is_none() { "/dev/null".into() } else { format!("b/{path}") };
            row["added"] = json!(added);
            row["removed"] = json!(removed);
            row["unified"] = json!(delta.unified_diff().header(&from, &to).to_string());
            if a.is_some() {
                row["before"] = json!(before);
            }
            if b.is_some() {
                row["after"] = json!(after);
            }
        } else {
            row["binary"] = json!(true);
        }
        if let Some(entry) = a {
            row["before_mode"] = json!(entry.0);
        }
        if let Some(entry) = b {
            row["after_mode"] = json!(entry.0);
        }
        rows.push(row);
    }
    Ok(json!({"base":base,"head":head,"files":rows}))
}

fn overlays(
    model: &Model,
    project: &Project,
    now: i64,
    files: &StateFiles,
    authorities: &BTreeMap<String, layr_model::authority::Authority>,
) -> Vec<Value> {
    let mut records = project.op_log(None, usize::MAX);
    for overlay in model.overlay_projects(&project.id) {
        if overlay.overlay_setting("overlay").is_some_and(|v| v["hidden"] == true) {
            records.extend(overlay.op_log(None, usize::MAX).into_iter().filter(|r| r.op == "overlay.create"));
        }
    }
    let mut result = Vec::new();
    for creation in records.iter().rev().filter(|r| r.op == "overlay.create") {
        let Some(name) = creation.data["name"].as_str() else { continue };
        let own = model.projects.get(&creation.project);
        let Some(info) = project
            .overlay_setting(&format!("overlay.{name}"))
            .or_else(|| own.and_then(|p| p.overlay_setting(&format!("overlay.{name}"))))
        else {
            continue;
        };
        let Some(id) = info["id"].as_str() else { continue };
        let overlay = model.projects.get(id);
        let mut row = json!({"id":id,"name":name,"hidden":info["hidden"].as_bool().unwrap_or(false),"patterns":[],"readers":[],"managers":[creation.machine],"readable":overlay.is_some()});
        if let Some(authority) = authorities.get(name) {
            row["managers"] = json!(authority.machines);
        }
        if let Some(owner) = info.get("owner") {
            row["owner"] = owner.clone();
        }
        if let Some(mode) = info.get("mode") {
            row["mode"] = mode.clone();
        }
        if let Some(overlay) = overlay {
            let policy = overlay.access_policy(now, true);
            let readers: Vec<_> = policy
                .access
                .iter()
                .filter(|(_, role)| **role >= layr_model::policy::Role::Reader)
                .map(|(p, _)| p.clone())
                .collect();
            row["readers"] = json!(readers);
            row["policy"] = serde_json::to_value(policy).unwrap();
            let records = overlay.op_log(None, usize::MAX);
            if let Some(record) = records.iter().find(|r| r.op == "overlay.key") {
                row["epoch"] = record.data["epoch"].clone();
            }
            let paths: BTreeSet<_> =
                overlay.line_list().iter().filter_map(|l| files.get(&l.head)).flat_map(|f| f.keys().cloned()).collect();
            row["history"] = json!(records);
            row["epochs"] = json!(records
                .iter()
                .filter(|r| r.op == "overlay.key")
                .map(|r| {
                    let mut epoch = json!({"id":r.data["epoch"],"time":r.time,"machine":r.machine});
                    if let Some(reason) = r.data.get("reason") {
                        epoch["reason"] = reason.clone();
                    }
                    epoch
                })
                .collect::<Vec<_>>());
            row["reader_details"] = json!(readers.iter().map(|principal| json!({"principal":principal})).collect::<Vec<_>>());
            row["files"] = json!(overlay
                .line_list()
                .iter()
                .flat_map(|line| files.get(&line.head).into_iter().flat_map(move |files| files.iter().map(
                    move |(path, bytes)| {
                        let mut value = json!({"state":line.head,"path":path,"size":bytes.len()});
                        if let Some(text) = text(bytes, 64 << 10) {
                            value["text"] = json!(text);
                        }
                        value
                    }
                )))
                .collect::<Vec<_>>());
            row["paths"] = json!(paths);
        }
        result.push(row);
    }
    result
}

/// Export the optional page data without changing the CLI comparison files.
fn page_outputs(
    out: &Path,
    model: &Model,
    project: &Project,
    metadata: &BTreeMap<String, format::Tree>,
    files: &StateFiles,
    overlay_files: &StateFiles,
    change: &str,
    context: &probes::Context,
    authorities: &BTreeMap<String, layr_model::authority::Authority>,
) -> Result<()> {
    let lines = project.line_list();
    let records = project.op_log(None, usize::MAX);
    let policy = project.access_policy(context.now, false);
    let overlays = overlays(model, project, context.now, overlay_files, authorities);
    let mut summary = json!({"id":project.id,"name":project.name.as_ref().context("project has no name record")?,"config":project.config,"overlays":overlays,
        "counts":{"lines":lines.len(),"states":project.states.len(),"changes":project.changes().len(),"refs":project.ref_list("",context.now).len(),"queue":project.queue_list(None).len(),"operations":records.len(),"attestations":records.iter().filter(|r|r.data.get("attestation").is_some() || matches!(r.op.as_str(),"approve"|"approve.withdraw"|"check")).count()}});
    if let Some(initial) =
        records.iter().rev().flat_map(|r| r.lines.iter()).find(|(_, c)| c.before.is_none() && c.after.is_some())
    {
        if let Some(line) = lines.iter().find(|l| l.id == *initial.0) {
            summary["default_line"] = json!(line.name);
        }
    }
    if let Some(line) = lines.iter().find(|l| Some(l.name.as_str()) == summary["default_line"].as_str()) {
        if let Some(probes) = context.definitions.get(&line.head) {
            summary["probes"] = json!(probes
                .iter()
                .map(|(name, p)| {
                    let mut p = serde_json::to_value(p).unwrap();
                    p["name"] = json!(name);
                    p
                })
                .collect::<Vec<_>>());
        }
    }
    summary["checks"] = json!(records
        .iter()
        .filter_map(|r| {
            let a = r.data.get("attestation")?;
            if a["kind"] != "check" {
                return None;
            }
            let c = r.data.get("check")?;
            Some(json!({"name":c["command"],"argv":c["argv"],"state":a["subject"]["id"],"exit":c["exit"],"machine":r.machine}))
        })
        .collect::<Vec<_>>());
    write(out, "states", project.states.values().map(|state| &state.state).collect::<Vec<_>>())?;
    write(out, "changes", project.changes().iter().map(|id| project.change_show(id, context)).collect::<Result<Vec<_>>>()?)?;
    write(out, "operations-all", &records)?;
    let subjects: BTreeSet<_> = project
        .states
        .keys()
        .map(String::as_str)
        .chain(records.iter().filter_map(|r| r.data["attestation"]["subject"]["id"].as_str()))
        .collect();
    write(out, "attestations-all", subjects.into_iter().flat_map(|id| project.attestations(id, None)).collect::<Vec<_>>())?;
    summary["policy"] = serde_json::to_value(&policy)?;
    write(out, "policy", &policy)?;
    write(out, "projects", vec![summary])?;
    let path_rules: BTreeMap<_, _> = policy
        .protect
        .iter()
        .filter(|(_, rule)| !rule.paths.is_empty())
        .map(|(pattern, rule)| (pattern, rule.paths.clone()))
        .collect();
    write(
        out,
        "config",
        json!({"project":project.id,"machine":project.machine,"values":project.config,"policy":policy,"path_rules":path_rules,"line_owners":lines.iter().map(|l|json!({"id":l.id,"name":l.name,"owner":l.owner})).collect::<Vec<_>>()}),
    )?;
    write(out, "overlays", overlays)?;
    write(out, "copies", project.copies())?;
    let mut machines = Vec::new();
    for (id, data) in &project.machines {
        let public = data["public_key"].as_str().context("machine record has no public key")?;
        let mut row = json!({"id":id,"fingerprint":format!("sha256:{}",hex::encode(Sha256::digest(hex::decode(public)?))),"trusted":project.trusted.contains_key(id)});
        if let Some(name) = data.get("name") {
            row["name"] = name.clone();
        }
        machines.push(row);
    }
    write(out, "machines", machines)?;
    write(out, "bisect", project.bisect_results())?;
    let mut trees = Vec::new();
    let mut contents = Vec::new();
    for (state, tree) in metadata {
        let mut directories = BTreeMap::<String, BTreeMap<String, Value>>::from([(String::new(), BTreeMap::new())]);
        for (path, entry) in &tree.files {
            let components: Vec<_> = path.split('/').collect();
            for i in 1..components.len() {
                let parent = components[..i - 1].join("/");
                let directory = components[..i].join("/");
                directories
                    .entry(parent)
                    .or_default()
                    .insert(directory.clone(), json!({"path":directory,"name":components[i-1],"kind":"directory","state":state}));
                directories.entry(directory).or_default();
            }
            let parent = components[..components.len() - 1].join("/");
            let mut item = json!({"path":path,"name":components.last(),"kind":if entry.0 == "120000" { "symlink" } else { "file" },"size":entry.1,"mode":entry.0,"state":state});
            let bytes = &files[state][path];
            let mut file = json!({"state":state,"path":path,"size":entry.1});
            if let Some(text) = text(bytes, 64 << 10) {
                item["text"] = json!(text);
                file["text"] = json!(text);
            }
            directories.entry(parent).or_default().insert(path.clone(), item);
            contents.push(file);
        }
        for (path, entries) in directories {
            trees.push(json!({"state":state,"path":path,"entries":entries.into_values().collect::<Vec<_>>()}));
        }
    }
    write(out, "trees", trees)?;
    write(out, "files", contents)?;
    let versions = project.change_versions(change);
    let head = versions.last().context("change has no head")?;
    let base = head.parents.first().context("change head has no parent")?;
    write(out, "diff", file_diff(base, &head.id, metadata, files)?)?;
    Ok(())
}

fn main() -> Result<()> {
    let args: Vec<PathBuf> = std::env::args_os().skip(1).map(PathBuf::from).collect();
    ensure!(args.len() == 3, "usage: store_golden <store dir> <paper key file> <out dir>");
    let request: Request = serde_json::from_slice(&fs::read(args[2].join("request.json"))?)?;
    let objects = objects(&args[0])?;
    let descriptor: format::Descriptor = serde_json::from_slice(get(&objects, "store.json")?)?;
    ensure!(descriptor.format == 2 && format::is_uuid(&descriptor.project), "invalid store descriptor");
    let paper = keys::identity_from_paper(&fs::read_to_string(&args[1])?)?;
    let identity = keys::open_slot(get(&objects, "slots/paper.age")?, &paper)?;
    ensure!(keys::recipient_of(&identity)? == descriptor.recipient, "paper slot differs from the store recipient");
    let (mut lines, public_keys) = records(&objects, &identity, &descriptor.project, None)?;
    ensure!(public_keys.len() == 1, "the golden fixture must use one machine");
    let machine = public_keys.keys().next().context("no machine")?.clone();

    let authorities = layr_model::authority::from_lines(&descriptor.project, &lines, &public_keys)?;
    // The fixture has one overlay epoch and one creator. Verify that creator's key.
    let overlay_keys: Vec<_> = objects
        .keys()
        .filter(|name| {
            let parts: Vec<_> = name.split('/').collect();
            parts.len() == 4 && parts[0] == "overlays" && parts[2] == "keys" && name.ends_with(".json")
        })
        .cloned()
        .collect();
    ensure!(overlay_keys.len() == 1, "the fixture must publish one overlay key");
    let mut overlay_files = BTreeMap::new();
    let mut overlay_project = String::new();
    for name in overlay_keys {
        let key_line = std::str::from_utf8(get(&objects, &name)?)?.trim_end_matches('\n');
        let raw = record::parse_line(key_line, None)?;
        let key_record: Record = serde_json::from_str(raw.get())?;
        let key = public_keys.get(&key_record.machine).context("unknown overlay key signer")?;
        record::parse_line(key_line, Some(key))?;
        ensure!(key_record.op == "overlay.key", "published key is not an overlay key");
        let data = &key_record.data;
        let overlay = data["overlay"].as_str().context("overlay key has no project")?;
        let epoch = data["epoch"].as_str().context("overlay key has no epoch")?;
        let layer_name = data["name"].as_str().context("overlay key has no name")?;
        ensure!(format::is_uuid(overlay) && format::is_uuid(epoch), "invalid overlay identity");
        ensure!(
            key_record.project == overlay && data["parent"] == descriptor.project && data["store"] == descriptor.recipient,
            "overlay key has wrong store bindings"
        );
        ensure!(name == format!("overlays/{overlay}/keys/{epoch}-{}.json", key_record.id), "overlay key has wrong object name");
        let created = lines
            .iter()
            .map(|line| {
                let raw = record::parse_line(line, None)?;
                Ok(serde_json::from_str::<Record>(raw.get())?)
            })
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .any(|rec| {
                rec.op == "overlay.create"
                    && rec.machine == key_record.machine
                    && rec.data["name"] == layer_name
                    && rec.data["config"][format!("overlay.{layer_name}")]["id"] == overlay
            });
        ensure!(created, "overlay key signer does not match its creator");
        let authority = authorities.get(layer_name).context("overlay has no verified parent authority")?;
        ensure!(authority.permits(&key_record, key_line), "overlay key signer is not a current manager");
        let prefix = format!("overlays/{overlay}/epochs/{epoch}/");
        let scoped: Objects = objects
            .iter()
            .filter_map(|(name, bytes)| name.strip_prefix(&prefix).map(|name| (name.into(), bytes.clone())))
            .collect();
        let layer_identity = keys::open_slot(get(&scoped, "slots/paper.age")?, &paper)?;
        ensure!(
            keys::recipient_of(&layer_identity)? == data["recipient"].as_str().context("no overlay recipient")?,
            "overlay slot has wrong recipient"
        );
        let (layer_lines, layer_keys) = records(&scoped, &layer_identity, overlay, Some(&public_keys))?;
        ensure!(layer_lines.iter().any(|line| line == key_line), "overlay log does not contain its key");
        authority.validate(&layer_lines, overlay, &public_keys)?;
        overlay_files = store(scoped, layer_identity, layer_keys).states()?;
        overlay_project = overlay.into();
        lines.extend(layer_lines);
    }

    // Hidden records enter the model only through a reader slot.
    let nonreader = Model::from_verified_lines(lines.iter().map(String::as_str), &machine)?;
    write(
        &args[2],
        "hidden-nonreader",
        nonreader
            .overlay_projects(&descriptor.project)
            .into_iter()
            .filter_map(|p| p.overlay_setting("overlay"))
            .filter(|v| v["hidden"] == true)
            .map(|v| json!({"name":v["name"],"hidden":true}))
            .collect::<Vec<_>>(),
    )?;
    let parent_before = serde_json::to_value(&nonreader.projects[&descriptor.project])?;
    let mut hidden_files = BTreeMap::new();
    for object in objects.keys().filter(|name| {
        let parts: Vec<_> = name.split('/').collect();
        parts.len() == 4 && parts[0] == "overlays" && parts[2] == "keys" && name.ends_with(".age")
    }) {
        let parts: Vec<_> = object.split('/').collect();
        let epoch = parts[3].get(..36).context("invalid hidden epoch")?;
        let prefix = format!("overlays/{}/epochs/{epoch}/", parts[1]);
        let scoped: Objects = objects
            .iter()
            .filter_map(|(name, bytes)| name.strip_prefix(&prefix).map(|name| (name.to_string(), bytes.clone())))
            .collect();
        let identity = keys::open_slot(get(&scoped, "slots/paper.age")?, &paper)?;
        let manifest = String::from_utf8(keys::decrypt(&identity, get(&objects, object)?)?)?;
        let raw = record::parse_line(&manifest, None)?;
        let key_record: Record = serde_json::from_str(raw.get())?;
        let key = public_keys.get(&key_record.machine).context("unknown hidden manifest signer")?;
        record::parse_line(&manifest, Some(key))?;
        let data = &key_record.data;
        ensure!(
            data["hidden"] == true && data["parent"] == descriptor.project && data["store"] == descriptor.recipient,
            "hidden manifest has wrong bindings"
        );
        ensure!(
            data["overlay"] == parts[1] && data["epoch"] == epoch && data["recipient"] == keys::recipient_of(&identity)?,
            "hidden manifest differs from its slot"
        );
        let (own, own_keys) = records(&scoped, &identity, parts[1], Some(&public_keys))?;
        layr_model::authority::from_overlay_lines(&descriptor.project, parts[1], &own, &public_keys)?;
        ensure!(own.contains(&manifest), "hidden manifest is absent from its own log");
        hidden_files.extend(store(scoped, identity, own_keys).states()?);
        lines.extend(own);
    }
    overlay_files.extend(hidden_files.clone());
    let model = Model::from_verified_lines(lines.iter().map(String::as_str), &machine)?;
    let project = model.projects.get(&descriptor.project).context("store project is absent from the model")?;
    ensure!(serde_json::to_value(project)? == parent_before, "hidden records changed the parent model");
    let hidden: Vec<_> = model
        .overlay_projects(&descriptor.project)
        .into_iter()
        .filter_map(|p| p.overlay_setting("overlay"))
        .filter(|v| v["hidden"] == true)
        .map(|v| json!({"name":v["name"],"hidden":true}))
        .collect();
    ensure!(hidden.len() == 1, "the reader must discover one hidden overlay");
    write(&args[2], "hidden-reader", hidden)?;
    let main = project.line_list().into_iter().find(|l| l.name == "main").context("no main line")?;
    let hidden_layers = model.layers(&descriptor.project, &main)?;
    ensure!(
        hidden_layers.iter().any(|l| l.overlay == "hidden-golden" && hidden_files.contains_key(&l.state)),
        "the reader model does not follow the hidden overlay"
    );
    ensure!(
        !nonreader.layers(&descriptor.project, &main)?.iter().any(|l| l.overlay == "hidden-golden"),
        "the parent-only model exposes a hidden layer"
    );
    let overlay = model.projects.get(&overlay_project).context("overlay is absent from the model")?;
    ensure!(
        overlay
            .overlay_setting("access")
            .and_then(Value::as_object)
            .is_some_and(|access| access.values().any(|role| role == "reader")),
        "overlay model has no reader"
    );
    ensure!(
        overlay_files.values().any(|files| files.get("private.txt").is_some_and(|bytes| bytes == b"private bytes\n")),
        "overlay file was not decrypted"
    );

    let mut source = store(objects, identity, public_keys);
    let files = source.states()?;
    let now = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis().try_into()?;
    let mut context = probes::Context { now, ..Default::default() };
    #[derive(Deserialize)]
    struct Manifest {
        #[serde(default)]
        probe: BTreeMap<String, probes::Probe>,
    }
    for (id, state) in &project.states {
        if let Some(files) = files.get(id) {
            context.sources.insert(id.clone(), "available".into());
            if state.state.kind == StateKind::Derived {
                context.outputs.insert(id.clone(), files.clone());
            }
            if let Some(manifest) = files.get("layr.toml") {
                let manifest: Manifest = toml::from_str(std::str::from_utf8(manifest)?)?;
                context.definitions.insert(id.clone(), manifest.probe);
            }
        } else {
            context.sources.insert(id.clone(), "source unavailable".into());
        }
    }
    for id in files.keys() {
        ensure!(project.states.contains_key(id), "store tree has no state record");
    }
    let line = project.line_list().into_iter().find(|line| line.name == request.line).context("requested line is absent")?;
    ensure!(project.change_versions(&request.change).len() == 3, "fixture change must have three versions");
    let subject = |id: String| Subject { kind: "state".into(), id };
    fs::create_dir_all(&args[2])?;
    write(&args[2], "lines", project.line_list())?;
    write(&args[2], "log", project.log(&[line.head.clone()], None, usize::MAX))?;
    write(&args[2], "log-auto", project.log(&[line.head.clone()], Some(&line.id), usize::MAX))?;
    write(&args[2], "change", project.change_show(&request.change, &context)?)?;
    write(&args[2], "refs", project.ref_list("", now))?;
    write(&args[2], "ref-log", project.ref_log(&request.reference))?;
    write(&args[2], "queue", project.queue_list(None))?;
    write(&args[2], "attestations", project.attestations(&request.attestation_state, None))?;
    write(&args[2], "operations", project.op_log(Some(&line.id), 30))?;
    let probe_base = subject(request.probe_base);
    let probe_head = subject(request.probe_head);
    write(&args[2], "probe-diff", probes::diff(project, &probe_base, &probe_head, &context)?)?;
    write(&args[2], "probe-same", probes::diff(project, &probe_head, &probe_head, &context)?)?;
    let mut merge_context = layr_model::protection::Context {
        now,
        actor: Some(layr_model::policy::Who { uid: 0, groups: vec![] }),
        approver_groups: BTreeMap::new(),
        changed_paths: vec![],
        candidate: None,
    };
    let change_head = project.change_versions(&request.change).last().context("change has no head")?.id.clone();
    write(&args[2], "merge_status", project.merge_status(&change_head, "main", &merge_context)?)?;
    let mut browser_context = merge_context.clone();
    browser_context.actor = None;
    let paths = |base: &str, head: &str| -> Vec<String> {
        let before = source.metadata.get(base);
        let after = source.metadata.get(head);
        let names: BTreeSet<_> =
            before.into_iter().flat_map(|t| t.files.keys()).chain(after.into_iter().flat_map(|t| t.files.keys())).collect();
        names
            .into_iter()
            .filter(|path| before.and_then(|t| t.files.get(*path)) != after.and_then(|t| t.files.get(*path)))
            .cloned()
            .collect()
    };
    let browser: Vec<_> = project
        .changes()
        .iter()
        .filter_map(|id| project.change_versions(id).last().cloned())
        .map(|state| {
            browser_context.changed_paths = paths(&line.head, &state.id);
            project.merge_status(&state.id, "main", &browser_context)
        })
        .collect::<Result<_>>()?;
    write(&args[2], "merge_status_browser", &browser)?;
    write(&args[2], "merge-status", &browser)?;
    let input: Vec<layr_model::interdiff::Version> = serde_json::from_slice(&fs::read(args[2].join("interdiff-input.json"))?)?;
    ensure!(input.len() == 3, "interdiff requires three real change versions");
    for version in &input {
        ensure!(
            serde_json::to_value(project.state(&version.state.id))? == serde_json::to_value(&version.state)?,
            "interdiff state differs from its signed record"
        );
    }
    write(&args[2], "interdiff", layr_model::interdiff::from_trees(&request.change, "v1", "v2", &input[0], &input[1]))?;
    write(&args[2], "interdiff-replay", layr_model::interdiff::from_trees(&request.change, "v2", "v3", &input[1], &input[2]))?;
    merge_context.candidate = Some(request.merge_candidate.clone());
    merge_context.changed_paths = paths(&line.head, &request.merge_candidate);
    let merge = project.merge_status(&request.merge_source, "main", &merge_context)?;
    merge_context.candidate = None;
    let reference = project.merge_status(&request.attestation_state, &request.reference, &merge_context)?;
    write(&args[2], "merge-refusals", json!({"merge":merge.allowed,"ref":reference.allowed}))?;
    write(&args[2], "merge-refusal-details", json!({"merge":merge,"ref":reference}))?;
    page_outputs(&args[2], &model, project, &source.metadata, &files, &overlay_files, &request.change, &context, &authorities)?;
    if project.line_list().len() != 2 {
        bail!("fixture must have two lines");
    }
    Ok(())
}
