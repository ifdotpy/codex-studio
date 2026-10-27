//! Publish immutable records, packed file frames and signed state trees.

use super::{
    canonical::canonical,
    format::{self, Entry, Header, Link, Tree},
    keys, reader, zst, Descriptor, Options, Report, Store,
};
use crate::{ctx::Ctx, fsutil, oplog, repo::Repo};
use anyhow::{bail, Context, Result};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
struct Pushed {
    attempt: String,
    tree: Tree,
    depth: usize,
    #[serde(skip)]
    canonical: Vec<u8>,
}

// Local metadata is small; full trees live in separate cache files and are loaded one at a time.
// Remote state objects remain the source of truth after a lost checkpoint.
const CHECKPOINT_STATES: usize = 128;
const RECORD_SEGMENT_BYTES: usize = 4 << 20;
#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    attempt: String,
    depth: usize,
}
#[derive(Serialize, Deserialize)]
struct PushIndex {
    format: u32,
    states: BTreeMap<String, Cached>,
}
fn cache_path(dir: &Path, state: &str, entry: &Cached) -> Result<std::path::PathBuf> {
    if !crate::model::is_uuid(state) || !format::valid_attempt(&entry.attempt) || entry.depth >= format::FULL_TREE_EVERY {
        bail!("invalid local pushed index metadata");
    }
    Ok(dir.join("pushed").join(format!("{state}.{}.json", entry.attempt)))
}
fn cache_tree(dir: &Path, state: &str, pushed: &Pushed) -> Result<Cached> {
    let entry = Cached { attempt: pushed.attempt.clone(), depth: pushed.depth };
    let path = cache_path(dir, state, &entry)?;
    std::fs::create_dir_all(path.parent().unwrap())?;
    fsutil::atomic_write(&path, &serde_json::to_vec(pushed)?, 0o600)?;
    Ok(entry)
}
fn cached_tree(dir: &Path, state: &str, entry: &Cached) -> Result<Pushed> {
    let pushed: Pushed = serde_json::from_slice(&std::fs::read(cache_path(dir, state, entry)?)?)?;
    if pushed.attempt != entry.attempt || pushed.depth != entry.depth {
        bail!("local pushed tree identity differs");
    }
    Tree::parse(&pushed.tree.canonical()?)?;
    Ok(pushed)
}
fn recovered_cache(dir: &Path, state: &str, attempt: String) -> Result<Cached> {
    let entry = Cached { attempt, depth: format::FULL_TREE_EVERY - 1 };
    // With no checkpoint, the local cache is not an authenticated description of
    // the published tree. Discard it and let the parent selection read that tree
    // through the normal remote signature checks (or upload a full child).
    match std::fs::remove_file(cache_path(dir, state, &entry)?) {
        Ok(()) => {},
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {},
        Err(error) => return Err(error.into()),
    }
    Ok(entry)
}
fn load_index(dir: &Path) -> Result<BTreeMap<String, Cached>> {
    let path = dir.join("pushed.json");
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let bytes = std::fs::read(path)?;
    let value: serde_json::Value = serde_json::from_slice(&bytes).context("invalid local pushed index")?;
    if value.get("format").is_some() {
        let index: PushIndex = serde_json::from_value(value)?;
        if index.format != 1 {
            bail!("unsupported local pushed index format {}", index.format);
        }
        for (id, entry) in &index.states {
            cache_path(dir, id, entry)?;
        }
        return Ok(index.states);
    }
    let legacy: BTreeMap<String, Pushed> = serde_json::from_value(value)?;
    legacy.into_iter().map(|(id, pushed)| Ok((id.clone(), cache_tree(dir, &id, &pushed)?))).collect()
}
fn checkpoint(dir: &Path, index: &BTreeMap<String, Cached>) -> Result<()> {
    fsutil::atomic_write(&dir.join("pushed.json"), &serde_json::to_vec(&PushIndex { format: 1, states: index.clone() })?, 0o600)
}
pub(super) fn parents_first(states: Vec<crate::model::StateRec>) -> Result<Vec<crate::model::StateRec>> {
    let mut pending: BTreeMap<_, _> = states.into_iter().map(|s| (s.id.clone(), s)).collect();
    let mut counts = BTreeMap::new();
    let mut children: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut ready = BTreeSet::new();
    for (id, state) in &pending {
        let mut count = 0;
        for parent in state.parents.iter().chain(state.derived_from.iter()) {
            if pending.contains_key(parent) {
                count += 1;
                children.entry(parent.clone()).or_default().push(id.clone());
            }
        }
        counts.insert(id.clone(), count);
        if count == 0 {
            ready.insert((state.time, id.clone()));
        }
    }
    let mut ordered = Vec::new();
    while let Some((_, id)) = ready.pop_first() {
        ordered.push(pending.remove(&id).unwrap());
        for child in children.remove(&id).unwrap_or_default() {
            let count = counts.get_mut(&child).unwrap();
            *count -= 1;
            if *count == 0 {
                ready.insert((pending[&child].time, child));
            }
        }
    }
    if !pending.is_empty() {
        bail!("local state ancestry has a cycle");
    }
    Ok(ordered)
}

pub(super) fn initialize(store: &Store, ctx: &crate::ctx::Ctx, output: &Path) -> Result<(String, String)> {
    use std::io::{Read, Write};
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    let path = std::path::absolute(if output.is_absolute() { output.to_path_buf() } else { ctx.cwd.join(output) })?;
    let parent = path.parent().context("paper key file has no parent")?;
    let parent = crate::privs::as_caller_fs(ctx, || Ok(std::fs::canonicalize(parent)?))?;
    let path = parent.join(path.file_name().context("paper key file has no name")?);
    let local_root = std::fs::canonicalize(&ctx.store.root)?;
    if path.starts_with(&local_root) {
        bail!("the paper key file must be outside the local store and line folders");
    }
    if let super::backend::Backend::File(root, _, _) = &store.backend {
        let remote_root = root.canonicalize().unwrap_or_else(|_| root.clone());
        if path.starts_with(remote_root) {
            bail!("the paper key file must be outside the remote store");
        }
    }
    let existing_slot = store.backend.get("slots/paper.age")?;
    let existing_file = crate::privs::as_caller_fs(ctx, || {
        match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(&path) {
            Ok(file) => {
                let meta = file.metadata()?;
                if !meta.is_file() || meta.uid() != ctx.caller.uid || meta.mode() & 0o777 != 0o600 {
                    bail!("paper key file must be owned by the caller with mode 0600");
                }
                let mut text = String::new();
                file.take(4097).read_to_string(&mut text)?;
                if text.len() > 4096 {
                    bail!("invalid paper key file");
                }
                Ok(Some(text))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    })?;
    let (paper, vault) = if let Some(text) = existing_file {
        let slot = existing_slot.as_ref().context("paper key file already exists; use a new --paper-key-out path")?;
        let paper = keys::identity_from_paper(&text).map_err(|_| anyhow::anyhow!("invalid paper key file"))?;
        let vault = keys::open_slot(slot, &paper).map_err(|_| anyhow::anyhow!("store is being created by another writer"))?;
        (paper, vault)
    } else {
        let paper = keys::generate_identity();
        let words = keys::words_of_identity(&paper)?;
        crate::privs::as_caller_fs(ctx, || {
            let mut file = std::fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .mode(0o600)
                .custom_flags(libc::O_NOFOLLOW)
                .open(&path)?;
            file.write_all(format!("{words}\n").as_bytes())?;
            file.sync_all()?;
            std::fs::File::open(&parent)?.sync_all()?;
            Ok(())
        })?;
        (paper, keys::generate_identity())
    };
    let recipient = publish_initial_slots(store, &paper, vault)?;
    Ok((path.to_string_lossy().into_owned(), recipient))
}

pub(super) fn initialize_tty(store: &Store, ctx: &Ctx) -> Result<String> {
    use std::io::{BufRead, IsTerminal, Read, Write};
    use std::os::unix::fs::OpenOptionsExt;
    if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
        bail!("--paper-key-tty requires terminal stdin and stdout; run it in your private terminal without redirection");
    }
    let mut terminal = crate::privs::as_caller_fs(ctx, || {
        Ok(std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NOCTTY)
            .open("/dev/tty")?)
    })
    .context("--paper-key-tty requires a controlling terminal; run it in your private terminal")?;
    if !terminal.is_terminal() {
        bail!("--paper-key-tty requires a controlling terminal");
    }
    // A service worker can inherit another session's controlling terminal.
    // Never display a caller's key on that terminal.
    let foreground = rustix::termios::tcgetpgrp(&terminal).context("cannot check the controlling terminal")?;
    if rustix::termios::tcgetpgrp(std::io::stdin()).ok() != Some(foreground) {
        bail!("--paper-key-tty requires stdin on your controlling terminal");
    }
    if store.backend.get("slots/paper.age")?.is_some() {
        bail!("store creation already started; recover with the original paper key");
    }
    let paper = keys::generate_identity();
    let words = keys::words_of_identity(&paper)?;
    writeln!(terminal, "Paper key (24 words):\n{words}\n\nSave these words privately. They will not be shown again.")?;
    write!(terminal, "Type the final word of the paper key to confirm: ")?;
    terminal.flush()?;
    let mut confirmation = String::new();
    std::io::BufReader::new((&terminal).take(128)).read_line(&mut confirmation)?;
    if confirmation.trim() != words.split_whitespace().last().expect("paper key has words") {
        bail!("paper key confirmation did not match; no store objects were published");
    }
    publish_initial_slots(store, &paper, keys::generate_identity())
}

fn publish_initial_slots(store: &Store, paper: &str, mut vault: String) -> Result<String> {
    let paper_slot = keys::seal_slot(&vault, &keys::recipient_of(paper)?)?;
    if !store.backend.put_new("slots/paper.age", paper_slot)? {
        let slot = store.backend.get("slots/paper.age")?.context("paper slot disappeared")?;
        vault = keys::open_slot(&slot, paper).map_err(|_| anyhow::anyhow!("store is being created by another writer"))?;
    }
    initialization_failpoint("paper")?;
    let machine_slot = format!("slots/machine-{}.age", store.machine);
    if !store.backend.put_new(&machine_slot, keys::machine_slot(&vault, &store.key.verifying_key())?)? {
        let slot = store.backend.get(&machine_slot)?.context("machine slot disappeared")?;
        if keys::open_machine_slot(&slot, &store.key)? != vault {
            bail!("existing machine slot belongs to another store");
        }
    }
    initialization_failpoint("machine")?;
    let descriptor = Descriptor { format: 2, project: store.project.clone(), recipient: keys::recipient_of(&vault)? };
    if !store.backend.put_new("store.json", canonical(&serde_json::to_value(&descriptor)?)?)? {
        if store.descriptor()?.recipient != descriptor.recipient {
            bail!("store is being created by another writer");
        }
    }
    initialization_failpoint("store")?;
    keys::recipient_of(paper)
}

// Unit tests inject interruption after each immutable publication.
#[cfg(test)]
thread_local! { static INIT_STOP: std::cell::RefCell<Option<&'static str>> = const {std::cell::RefCell::new(None)}; }
fn initialization_failpoint(point: &str) -> Result<()> {
    #[cfg(test)]
    if INIT_STOP.with(|value| *value.borrow() == Some(point)) {
        bail!("injected initialization interruption");
    }
    let _ = point;
    Ok(())
}

pub(super) fn push(store: &Store, ctx: &Ctx, repo: &Repo, opts: &Options) -> Result<Report> {
    let _lock = repo.p.lock()?;
    push_locked(store, ctx, repo, opts)
}

pub(super) fn push_locked(store: &Store, ctx: &Ctx, repo: &Repo, opts: &Options) -> Result<Report> {
    let _push = crate::trace::span("store push");
    if opts.caches && serde_json::from_value::<crate::model::StateKind>(serde_json::json!("cache")).is_err() {
        bail!("store --caches is not available until cache states exist");
    }
    super::epochs::check_current(store)?;
    store.private_dir()?;
    let descriptor = store.descriptor()?;
    let blind_layer = store.layer.is_some() && !store.backend.can_read();
    let mut report = Report { omitted: reader::omitted_cache_refs(repo, opts)?, ..Default::default() };
    repo.p.verify_records()?;
    // A list is enough for a writer without permission to read encrypted objects.
    let stage = crate::trace::span("store records");
    for machine in repo.p.db.record_machines()? {
        let mut last = 0;
        for object in if blind_layer { Vec::new() } else { store.backend.list(&format!("records/{machine}/"))? } {
            let (m, _, end) = format::parse_segment_name(&object.key).context("invalid remote segment name")?;
            if m != machine {
                bail!("segment is outside its machine prefix");
            }
            last = last.max(end);
        }
        let lines = repo.p.db.lines_after(&machine, last)?;
        if lines.is_empty() {
            continue;
        }
        let mut start = 0;
        while start < lines.len() {
            let mut end = start;
            let mut size = 0;
            while end < lines.len() && (end == start || size + lines[end].len() + 1 <= RECORD_SEGMENT_BYTES) {
                size += lines[end].len() + 1;
                end += 1;
            }
            let first = oplog::parse_line(&lines[start], None)?.record.seq;
            let last = oplog::parse_line(&lines[end - 1], None)?.record.seq;
            let mut bytes = lines[start..end].join("\n").into_bytes();
            bytes.push(b'\n');
            // Publish ordered, bounded segments. A retry resumes after the last complete segment.
            store.backend.put_new(&format::segment_name(&machine, first, last), keys::encrypt(&descriptor.recipient, &bytes)?)?;
            report.records += end - start;
            start = end;
        }
    }
    drop(stage);
    let stage = crate::trace::span("store load index");
    let mut index = load_index(&store.dir)?;
    // Completed immutable trees are never collected. Cache metadata is written only after
    // publication, so a regular push need not enumerate every historical remote object.
    index.retain(|id, entry| cache_path(&store.dir, id, entry).is_ok());
    if !blind_layer {
        // A completed upload can be missing from a partial replica. Recheck advertised tips
        // without scanning historical states, so a later push can complete that replica.
        let mut tips: std::collections::BTreeSet<String> = repo.p.db.lines()?.into_iter().map(|line| line.head).collect();
        tips.extend(repo.p.db.refs("", crate::ids::now_ms())?.into_iter().map(|reference| reference.target));
        for tip in tips {
            if let Some(cached) = index.get(&tip) {
                let expected = format::tree_name(&tip, &cached.attempt);
                if !store.backend.list(&format!("states/{tip}/"))?.iter().any(|object| object.key == expected) {
                    index.remove(&tip);
                }
            }
        }
    }
    let mut present: BTreeMap<String, String> = index.iter().map(|(id, entry)| (id.clone(), entry.attempt.clone())).collect();
    // Authenticate another writer's records only if a missing local parent cache is needed.
    // An ordinary incremental push reuses its already verified local parent directly.
    let mut remote_records = None;
    drop(stage);
    let stage = crate::trace::span("store select history");
    let required = reader::required_states(repo, &repo.p.export_jsonl()?.lines().map(str::to_string).collect::<Vec<_>>(), opts)?;
    let mut states = Vec::new();
    for (id, deleted) in repo.p.db.present_states()? {
        if deleted || present.contains_key(&id) {
            continue;
        }
        let state = repo.state(&id)?;
        if !reader::wanted(&state, opts) && !required.contains(&id) {
            report.omitted.push(format!("{} {}", id, serde_json::to_value(state.kind)?));
            continue;
        }
        if !blind_layer {
            let prefix = format!("states/{id}/");
            let mut complete = BTreeMap::new();
            for object in store.backend.list(&prefix)? {
                if let Some((remote, attempt)) = format::parse_tree_name(&object.key) {
                    if remote != id {
                        bail!("remote tree is outside its state prefix");
                    }
                    complete.entry(remote).or_insert(attempt);
                }
            }
            if let Some(attempt) = complete.remove(&id) {
                present.insert(id.clone(), attempt.clone());
                let cached = recovered_cache(&store.dir, &id, attempt)?;
                index.insert(id, cached);
                continue;
            }
        }
        states.push(state);
    }
    states.sort_by(|a, b| (a.time, &a.id).cmp(&(b.time, &b.id)));
    drop(stage);
    let stage = crate::trace::span("store secret checks");
    // Check changed files against one current secret source index before uploading any state.
    crate::secrets::check_states(ctx, repo, states.iter().map(|state| state.id.as_str()))?;
    drop(stage);
    let stage = crate::trace::span("store collect hashes");
    let mut hashes = BTreeMap::new();
    drop(stage);
    let mut latest: Option<(String, Pushed)> = None;
    for state in parents_first(states)? {
        let stage = crate::trace::span("store select parent");
        let direct = state
            .parents
            .iter()
            .chain(state.derived_from.iter())
            .find(|id| present.contains_key(*id) || index.contains_key(*id))
            .cloned();
        let base_id = if direct.is_some() || index.is_empty() {
            direct
        } else {
            reader::state_ancestry(repo, &state.id)?.into_iter().skip(1).find(|id| index.contains_key(id))
        };
        let base = match base_id {
            Some(id) if latest.as_ref().is_some_and(|(last, _)| last == &id) => latest.take(),
            Some(id) => {
                if index.get(&id).map(|entry| cache_path(&store.dir, &id, entry).is_ok_and(|p| p.exists())) != Some(true)
                    && store.backend.can_read()
                {
                    if let Ok(identity) = store.identity(&descriptor, opts.paper_key.as_deref()) {
                        if remote_records.is_none() {
                            remote_records = Some(reader::records(&store.backend, &identity, &descriptor.project)?);
                        }
                        let records = remote_records.as_ref().unwrap();
                        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
                        let attempt = &present[&id];
                        let (header, canonical, tree) = source.tree(&id, attempt)?;
                        let depth = if header.tree == "full" { 0 } else { format::FULL_TREE_EVERY - 1 };
                        let pushed = Pushed { attempt: attempt.clone(), tree, depth, canonical };
                        index.insert(id.clone(), cache_tree(&store.dir, &id, &pushed)?);
                    }
                }
                if let Some(entry) = index.get(&id).filter(|entry| cache_path(&store.dir, &id, entry).is_ok_and(|p| p.exists())) {
                    let pushed = cached_tree(&store.dir, &id, entry)?;
                    for entry in pushed.tree.files.values() {
                        hashes.entry(entry.2.clone()).or_insert_with(|| entry.3.clone());
                    }
                    Some((id, pushed))
                } else {
                    None
                }
            }
            None => None,
        };
        drop(stage);
        let stage = crate::trace::span("store upload state");
        let root = repo.state_root(&state.id)?;
        let base_root = base.as_ref().map(|(id, _)| repo.state_root(id)).transpose()?;
        let (pushed, files, bytes) = upload_state(
            store,
            &descriptor,
            &state.id,
            &root,
            base.as_ref(),
            &mut hashes,
            base_root.as_deref(),
            btrfs_identity(&root, &state.subvol),
        )?;
        drop(stage);
        let stage = crate::trace::span("store checkpoint");
        index.insert(state.id.clone(), cache_tree(&store.dir, &state.id, &pushed)?);
        if (report.states + 1) % CHECKPOINT_STATES == 0 {
            checkpoint(&store.dir, &index)?;
        }
        latest = Some((state.id.clone(), pushed));
        drop(stage);
        report.states += 1;
        report.state_ids.push(state.id.clone());
        report.files += files;
        report.bytes += bytes;
    }
    checkpoint(&store.dir, &index)?;
    for (id, _) in repo.p.db.present_states()? {
        for name in repo.state(&id)?.caches.values() {
            report.omitted.push(format!("cache snapshot {name}: legacy cache, use a peer remote or save it as a cache state"));
        }
    }
    Ok(report)
}

fn content(root: &Path, path: &str, mode: &str) -> Result<Vec<u8>> {
    if mode == "120000" {
        use std::os::unix::ffi::OsStrExt;
        return Ok(std::fs::read_link(fsutil::safe_join(root, path)?)?.as_os_str().as_bytes().to_vec());
    }
    let path = fsutil::safe_join(root, path)?;
    use std::io::Read;
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    if !file.metadata()?.is_file() {
        bail!("state contains a special file");
    }
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;
    Ok(data)
}

/// Fail on unreadable folders or non-UTF-8 names rather than omit their files.
fn walk_state(root: &Path, visit: &mut dyn FnMut(&str, &fsutil::Meta) -> Result<bool>) -> Result<()> {
    let mut pending = vec![root.to_path_buf()];
    while let Some(folder) = pending.pop() {
        let mut entries = std::fs::read_dir(&folder)?.collect::<std::io::Result<Vec<_>>>()?;
        entries.sort_by_key(|e| e.file_name());
        for entry in entries {
            let path = entry.path();
            let relative = path.strip_prefix(root)?.to_str().context("state path is not UTF-8")?;
            let metadata = std::fs::symlink_metadata(&path)?;
            let meta = fsutil::meta_of(&metadata);
            if meta.is_dir() && crate::storage::is_subvolume(&path) {
                continue;
            }
            if visit(relative, &meta)? && meta.is_dir() {
                pending.push(path);
            }
        }
    }
    Ok(())
}

fn upload_state(
    store: &Store,
    descriptor: &Descriptor,
    state: &str,
    root: &Path,
    base: Option<&(String, Pushed)>,
    hashes: &mut BTreeMap<String, Vec<Link>>,
    base_root: Option<&Path>,
    btrfs: Option<format::BtrfsIdentity>,
) -> Result<(Pushed, usize, u64)> {
    let started = std::time::Instant::now();
    let mut random = [0u8; 4];
    rand::rngs::OsRng.fill_bytes(&mut random);
    let attempt = hex::encode(random);
    let delta_root = base_root.filter(|base| crate::storage::is_subvolume(base) && crate::storage::is_subvolume(root));
    let mut files =
        if delta_root.is_some() { base.map(|(_, p)| p.tree.files.clone()).unwrap_or_default() } else { BTreeMap::new() };
    let mut current = Vec::new();
    let mut part = 0;
    let mut frames = 0;
    let mut bytes = 0;
    let stage = crate::trace::span("store walk and frames");
    let changed = delta_root.map(|base| crate::changes::file_changes(base, root)).transpose()?;
    if let Some(changes) = &changed {
        for change in changes {
            if let Some(old) = &change.old_path {
                files.remove(old);
            }
            files.remove(&change.path);
        }
    }
    let mut visit = |path: &str, meta: &fsutil::Meta| -> Result<bool> {
        if crate::repo::skip_path(path) {
            return Ok(false);
        }
        if meta.is_dir() {
            return Ok(true);
        }
        if !format::safe_path(path) {
            bail!("state has an unsafe path");
        }
        if meta.ftype == fsutil::FType::Special {
            bail!("state has a special file: {path}");
        }
        let mode = format!("{:o}", meta.git_mode());
        let data = content(root, path, &mode)?;
        let sha = hex::encode(Sha256::digest(&data));
        if let Some(chain) = hashes.get(&sha) {
            files.insert(path.to_string(), Entry(mode, data.len() as u64, sha, chain.clone()));
            return Ok(true);
        }
        let full = zst::compress(&data, None)?;
        let mut frame = full;
        let mut chain = Vec::new();
        if let (Some((_, pushed)), Some(base_root)) = (base, base_root) {
            if let Some(previous) = pushed.tree.files.get(path) {
                if previous.3.len() <= format::MAX_FILE_DELTAS
                    && data.len() <= format::DELTA_LIMIT
                    && previous.1 <= format::DELTA_LIMIT as u64
                {
                    if let Ok(old) = content(base_root, path, &previous.0) {
                        if !old.is_empty() && hex::encode(Sha256::digest(&old)) == previous.2 {
                            let delta = zst::compress(&data, Some(&old))?;
                            if delta.len() < frame.len() {
                                frame = delta;
                                chain = previous.3.clone();
                            }
                        }
                    }
                }
            }
        }
        if !current.is_empty() && current.len() + frame.len() > format::PART_LIMIT {
            put_part(store, descriptor, state, &attempt, part, std::mem::take(&mut current))?;
            part += 1;
        }
        let link = Link(
            state.to_string(),
            attempt.clone(),
            part,
            current.len() as u64,
            frame.len() as u64,
            if chain.is_empty() { "full" } else { "delta" }.into(),
        );
        bytes += frame.len() as u64;
        current.extend(frame);
        chain.insert(0, link);
        hashes.insert(sha.clone(), chain.clone());
        files.insert(path.to_string(), Entry(mode, data.len() as u64, sha, chain));
        frames += 1;
        Ok(true)
    };
    if let Some(changes) = &changed {
        for change in changes {
            if let Some(meta) = &change.new {
                visit(&change.path, meta)?;
            }
        }
    } else {
        walk_state(root, &mut visit)?;
    }
    if !current.is_empty() {
        put_part(store, descriptor, state, &attempt, part, current)?;
        part += 1;
    }
    drop(stage);
    let stage = crate::trace::span("store tree encode");
    let tree = Tree { files };
    let canonical_tree = tree.canonical()?;
    let delta_base = base.filter(|(_, p)| p.depth + 1 < format::FULL_TREE_EVERY);
    let encoded_base;
    let base_bytes = if let Some((_, parent)) = delta_base {
        if parent.canonical.is_empty() {
            encoded_base = parent.tree.canonical()?;
            Some(encoded_base.as_slice())
        } else {
            Some(parent.canonical.as_slice())
        }
    } else {
        None
    };
    let body = zst::compress(&canonical_tree, base_bytes)?;
    let mut header = Header {
        v: 2,
        state: state.into(),
        attempt: attempt.clone(),
        tree: if delta_base.is_some() { "delta" } else { "full" }.into(),
        base: delta_base.map(|(id, p)| (id.clone(), p.attempt.clone())),
        parts: part,
        machine: store.machine.clone(),
        btrfs,
        sig: String::new(),
    };
    header.sign(&canonical_tree, &store.key)?;
    let mut object = canonical(&serde_json::to_value(&header)?)?;
    object.push(b'\n');
    object.extend(&body);
    // Never race collection of parts whose attempt is older than the grace period.
    if started.elapsed().as_secs() >= 86400 {
        bail!("state upload exceeded 24 hours; retry with a new attempt");
    }
    drop(stage);
    let _publish = crate::trace::span("store tree publish");
    if !store.backend.put_new(&format::tree_name(state, &attempt), keys::encrypt(&descriptor.recipient, &object)?)? {
        bail!("state attempt already exists; retry with a new attempt");
    }
    bytes += body.len() as u64;
    Ok((Pushed { attempt, tree, depth: delta_base.map_or(0, |(_, p)| p.depth + 1), canonical: canonical_tree }, frames, bytes))
}

fn btrfs_identity(root: &Path, signed_uuid: &str) -> Option<format::BtrfsIdentity> {
    #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
    {
        let (uuid, received_transid) = crate::storage::clone_identity(root).ok()?;
        if uuid != signed_uuid {
            return None;
        }
        let transid = received_transid.or_else(|| crate::storage::send_clone_identity(root).ok().map(|(_, t)| t))?;
        return Some(format::BtrfsIdentity { uuid, transid });
    }
    #[cfg(not(all(target_os = "linux", not(feature = "overlayfs"))))]
    {
        let _ = (root, signed_uuid);
        None
    }
}

fn put_part(store: &Store, descriptor: &Descriptor, state: &str, attempt: &str, part: u32, data: Vec<u8>) -> Result<()> {
    let _publish = crate::trace::span("store part publish");
    if part >= format::MAX_PARTS {
        bail!("state exceeds 10000 data parts");
    }
    if !store.backend.put_new(&format::data_name(state, attempt, part), keys::encrypt(&descriptor.recipient, &data)?)? {
        bail!("state attempt has an existing part; retry with a new attempt");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::{Caller, Ctx};
    use std::collections::HashMap;
    use std::os::unix::fs::PermissionsExt;

    fn context(root: &Path) -> Ctx {
        Ctx {
            store: crate::store::Store::open(&root.join("local")).unwrap(),
            caller: Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.to_path_buf(),
            env: HashMap::new(),
            out: std::cell::RefCell::new(Box::new(Vec::<u8>::new())),
        }
    }
    fn remote(root: &Path, ctx: &Ctx) -> Store {
        Store {
            backend: super::super::backend::Backend::file(root.join("remote")),
            url: format!("file://{}", root.join("remote").display()),
            project: "01a12290-0000-7000-8000-000000000002".into(),
            machine: ctx.store.machine.id.clone(),
            key: ctx.store.machine.key.clone(),
            dir: root.join("local/meta"),
            paper_key_text: None,
            person: None,
            pin: None,
            aws_profile: None,
            layer: None,
            epoch: None,
            epoch_public: false,
            publishing_epoch: false,
            force_public: false,
            public_clone: false,
            public_reader: std::sync::atomic::AtomicBool::new(false),
        }
    }
    #[test]
    fn initialization_recovers_after_each_publication_and_keeps_the_key_private() {
        for stop in ["paper", "machine", "store"] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            let path = root.join("recovery.txt");
            let opts = super::super::ConfigureOptions { paper_key_out: Some(path.clone()), ..Default::default() };
            INIT_STOP.with(|value| *value.borrow_mut() = Some(stop));
            assert!(store.configure(&ctx, &opts).is_err());
            INIT_STOP.with(|value| *value.borrow_mut() = None);
            let result = store.configure(&ctx, &opts).unwrap();
            assert_eq!(result.created, stop != "store");
            assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
            let words = std::fs::read_to_string(&path).unwrap();
            assert_eq!(words.split_whitespace().count(), 24);
            let descriptor = store.descriptor().unwrap();
            let identity = store.identity(&descriptor, Some(&words)).unwrap();
            assert_eq!(keys::recipient_of(&identity).unwrap(), descriptor.recipient);
            for object in store.backend.list("").unwrap() {
                let bytes = store.backend.get(&object.key).unwrap().unwrap();
                assert!(!bytes.windows(words.trim().len()).any(|part| part == words.trim().as_bytes()));
            }
            assert!(!serde_json::to_string(&result).unwrap().contains(words.trim()));
            std::fs::remove_dir_all(root).unwrap();
        }
    }
    #[test]
    fn live_refs_keep_auto_states_and_auxiliary_states_obey_the_cache_option() {
        use crate::model::{Record, StateRec};
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() }).unwrap();
        let dir = ctx.store.projects_dir().join("p");
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        std::fs::create_dir_all(dir.join("states")).unwrap();
        let project = crate::store::Project {
            store: &ctx.store,
            name: "p".into(),
            dir: dir.clone(),
            id: store.project.clone(),
            db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        let mut record = Record::new("project");
        record.data = serde_json::json!({"id":store.project,"name":"p","config":{}});
        project.append(record, &ctx.actor()).unwrap();
        let mut source = None;
        let mut cache = String::new();
        for kind in ["commit", "auto", "derived", "cache"] {
            let id = crate::ids::new_id();
            std::fs::create_dir(project.state_path(&id)).unwrap();
            std::fs::write(project.state_path(&id).join("content"), kind).unwrap();
            let state: StateRec = serde_json::from_value(serde_json::json!({"id":id,"kind":kind,
                "subvol":crate::ids::new_id(),"author":{"name":"Test","email":"test@example.test"},
                "derived_from":if kind=="derived"||kind=="cache" {source.clone()}else{None},
                "parents":if kind=="auto" {source.clone().into_iter().collect::<Vec<_>>()}else{vec![]},
                "time":crate::ids::now_ms()}))
            .unwrap();
            let mut record = Record::new("state");
            record.states.push(state);
            project.append(record, &ctx.actor()).unwrap();
            if kind == "commit" {
                source = Some(id);
                continue;
            }
            let name = format!(
                "{}/test",
                if kind == "auto" {
                    "pins"
                } else if kind == "cache" {
                    "caches"
                } else {
                    "derived"
                }
            );
            let mut record = Record::new("ref.set");
            record.data = serde_json::json!({"ref_change":{"name":name,"before":null,"after":{"name":name,"target":id}}});
            project.append(record, &ctx.actor()).unwrap();
            if kind == "cache" {
                cache = id;
            }
        }
        let repo = Repo { p: project };
        let report = push(&store, &ctx, &repo, &Options::default()).unwrap();
        assert_eq!(report.states, 3);
        assert!(report.omitted.contains(&format!("cache state {cache} of ref caches/test: use --caches")));
        assert!(!reader::attempts(&store.backend).unwrap().contains_key(&cache));
        let report = push(&store, &ctx, &repo, &Options { caches: true, ..Default::default() }).unwrap();
        assert_eq!(report.states, 1);
        assert!(reader::attempts(&store.backend).unwrap().contains_key(&cache));
        store.verify(true).unwrap();
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_pushed_cache_migrates_without_changing_attempt_or_tree() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let id = crate::ids::new_id();
        let pushed =
            Pushed { attempt: "0123abcd".into(), tree: Tree { files: BTreeMap::new() }, depth: 0, canonical: Vec::new() };
        let legacy = BTreeMap::from([(id.clone(), pushed)]);
        std::fs::write(root.join("pushed.json"), serde_json::to_vec(&legacy).unwrap()).unwrap();
        let index = load_index(&root).unwrap();
        let restored = cached_tree(&root, &id, &index[&id]).unwrap();
        assert_eq!(restored.attempt, "0123abcd");
        assert!(restored.tree.files.is_empty());
        checkpoint(&root, &index).unwrap();
        assert_eq!(load_index(&root).unwrap()[&id].attempt, "0123abcd");
        let json: serde_json::Value = serde_json::from_slice(&std::fs::read(root.join("pushed.json")).unwrap()).unwrap();
        assert_eq!(json["format"], 1);
        assert!(json["states"][&id].get("tree").is_none());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn an_existing_key_file_is_not_reused_without_an_openable_unfinished_slot() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        let path = root.join("keep.txt");
        fsutil::atomic_write(&path, b"do not replace", 0o600).unwrap();
        assert!(store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(path.clone()), ..Default::default() }).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"do not replace");
        assert!(store.backend.get("store.json").unwrap().is_none());
        assert!(store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(ctx.store.root.join("bad.txt")), ..Default::default() })
            .is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn two_initializers_never_publish_different_slot_identities() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));
        let threads: Vec<_> = (0..2)
            .map(|number| {
                let root = root.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    let own = root.join(format!("writer-{number}"));
                    std::fs::create_dir(&own).unwrap();
                    let ctx = context(&own);
                    let mut store = remote(&root, &ctx);
                    barrier.wait();
                    let result =
                        store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(own.join("paper.txt")), ..Default::default() });
                    result.map(|r| r.created)
                })
            })
            .collect();
        let results: Vec<_> = threads.into_iter().map(|t| t.join().unwrap()).collect();
        assert_eq!(results.iter().filter(|r| matches!(r, Ok(true))).count(), 1);
        let ctx = context(&root);
        let store = remote(&root, &ctx);
        let descriptor = store.descriptor().unwrap();
        let slot = store.backend.get("slots/paper.age").unwrap().unwrap();
        let opened: Vec<_> = (0..2)
            .filter_map(|n| std::fs::read_to_string(root.join(format!("writer-{n}/paper.txt"))).ok())
            .filter_map(|words| keys::identity_from_paper(&words).ok())
            .filter_map(|key| keys::open_slot(&slot, &key).ok())
            .collect();
        assert_eq!(opened.len(), 1);
        assert_eq!(keys::recipient_of(&opened[0]).unwrap(), descriptor.recipient);
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn state_deltas_have_bounded_chains_and_reuse_content_across_lines() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() }).unwrap();
        let descriptor = store.descriptor().unwrap();
        let mut base = None;
        let mut base_root = None;
        let mut content =
            (0..3000).map(|n| format!("some sufficiently varied file data {n}: {:x}\n", n * 16127)).collect::<String>();
        let mut checkpoints = 0;
        let mut saw_delta = false;
        for number in 0..34 {
            let source = root.join(format!("state-{number}"));
            std::fs::create_dir(&source).unwrap();
            content.push_str(&format!("change number {number}\n"));
            std::fs::write(source.join("a"), &content).unwrap();
            let id = crate::ids::new_id();
            let (pushed, _, _) =
                upload_state(&store, &descriptor, &id, &source, base.as_ref(), &mut BTreeMap::new(), base_root.as_deref(), None)
                    .unwrap();
            assert!(pushed.tree.files["a"].3.len() <= 17);
            saw_delta |= pushed.tree.files["a"].3.len() > 1;
            if pushed.depth == 0 {
                checkpoints += 1;
            }
            base = Some((id, pushed));
            base_root = Some(source);
        }
        assert!(saw_delta);
        assert_eq!(checkpoints, 2);
        let (id, last) = base.unwrap();
        let mut hashes = last.tree.files.values().map(|e| (e.2.clone(), e.3.clone())).collect();
        let duplicate = crate::ids::new_id();
        let (dedup, frames, _) =
            upload_state(&store, &descriptor, &duplicate, base_root.as_deref().unwrap(), None, &mut hashes, None, None).unwrap();
        assert_eq!(frames, 0);
        assert_eq!(dedup.tree.files["a"].3, last.tree.files["a"].3);
        // A second complete upload after a lost response still represents one logical state.
        upload_state(&store, &descriptor, &id, base_root.as_deref().unwrap(), None, &mut hashes, None, None).unwrap();
        assert_eq!(reader::attempts(&store.backend).unwrap().len(), 35);
        let identity = store.identity(&descriptor, None).unwrap();
        let machine_keys = BTreeMap::from([(store.machine.clone(), store.key.verifying_key())]);
        let mut reader = reader::Reader::new(&store.backend, &identity, &machine_keys);
        let (_, _, tree) = reader.tree(&id, &last.attempt).unwrap();
        assert_eq!(reader.read(&tree.files["a"]).unwrap(), content.as_bytes());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn collection_keeps_complete_reused_and_young_parts() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() }).unwrap();
        let descriptor = store.descriptor().unwrap();
        let mut record = crate::model::Record::new("machine");
        record.id = crate::ids::new_id();
        record.seq = 1;
        record.machine = store.machine.clone();
        record.project = store.project.clone();
        record.data = serde_json::json!({"public_key":hex::encode(store.key.verifying_key().to_bytes())});
        let line = crate::oplog::sign_line(&record, &store.key).unwrap();
        store
            .backend
            .put_new(
                &format::segment_name(&store.machine, 1, 1),
                keys::encrypt(&descriptor.recipient, format!("{line}\n").as_bytes()).unwrap(),
            )
            .unwrap();
        let source = root.join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("a"), b"live bytes").unwrap();
        let id = crate::ids::new_id();
        let (pushed, _, _) = upload_state(&store, &descriptor, &id, &source, None, &mut BTreeMap::new(), None, None).unwrap();
        let complete = pushed.tree.files["a"].3[0].object();
        let orphan = format::data_name(&crate::ids::new_id(), "1234abcd", 0);
        let young = format::data_name(&crate::ids::new_id(), "1234abcd", 0);
        store.backend.put_new(&orphan, b"unfinished old upload".to_vec()).unwrap();
        store.backend.put_new(&young, b"concurrent young upload".to_vec()).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(90000);
        for key in [&complete, &orphan] {
            std::fs::File::options().write(true).open(root.join("remote").join(key)).unwrap().set_modified(old).unwrap();
        }
        assert_eq!(store.gc(true).unwrap(), vec![orphan.clone()]);
        assert!(store.backend.get(&orphan).unwrap().is_some());
        assert_eq!(store.gc(false).unwrap(), vec![orphan.clone()]);
        assert!(store.backend.get(&orphan).unwrap().is_none());
        assert!(store.backend.get(&young).unwrap().is_some());
        assert!(store.backend.get(&complete).unwrap().is_some());
        let barrier = std::sync::Barrier::new(2);
        let concurrent = crate::ids::new_id();
        let pushed = std::thread::scope(|scope| {
            let upload = scope.spawn(|| {
                barrier.wait();
                upload_state(&store, &descriptor, &concurrent, &source, None, &mut BTreeMap::new(), None, None).unwrap().0
            });
            barrier.wait();
            for _ in 0..10 {
                store.gc(false).unwrap();
            }
            upload.join().unwrap()
        });
        let identity = store.identity(&descriptor, None).unwrap();
        let machine_keys = BTreeMap::from([(store.machine.clone(), store.key.verifying_key())]);
        let mut reader = reader::Reader::new(&store.backend, &identity, &machine_keys);
        let (_, _, restored) = reader.tree(&concurrent, &pushed.attempt).unwrap();
        assert_eq!(reader.read(&restored.files["a"]).unwrap(), b"live bytes");
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "portable-state fixture invoked by t23-store-portable-peer"]
    fn emit_portable_v2_fixture() {
        use crate::model::{LineChange, LineState, Record, StateRec};
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // The fixture writer runs only from t23, which sets its folders.
        let Ok(destination) = std::env::var("LAYR_TEST_PORTABLE_DIR") else { return };
        let paper_file = std::env::var("LAYR_TEST_PORTABLE_KEY").expect("fixture key path");
        let local = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir(&local).unwrap();
        let ctx = context(&local);
        let mut store = remote(&local, &ctx);
        store.backend = super::super::backend::Backend::file(destination.into());
        let identity = keys::generate_identity();
        let paper = keys::generate_identity();
        let descriptor =
            Descriptor { format: 2, project: store.project.clone(), recipient: keys::recipient_of(&identity).unwrap() };
        let mut output = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(paper_file).unwrap();
        writeln!(output, "{}", keys::words_of_identity(&paper).unwrap()).unwrap();
        store
            .backend
            .put_new("slots/paper.age", keys::seal_slot(&identity, &keys::recipient_of(&paper).unwrap()).unwrap())
            .unwrap();
        store.backend.put_new("store.json", serde_json::to_vec(&descriptor).unwrap()).unwrap();
        let state_id = crate::ids::new_id();
        let line_id = crate::ids::new_id();
        let state:StateRec=serde_json::from_value(serde_json::json!({"id":state_id,"kind":"commit","parents":[],"line":line_id,
            "subvol":crate::ids::new_id(),"author":{"name":"Fixture","email":"fixture@example.test"},"message":"portable source","time":1})).unwrap();
        let mut first = Record::new("machine");
        first.data = serde_json::json!({"name":"portable","public_key":hex::encode(store.key.verifying_key().to_bytes())});
        let mut project = Record::new("project");
        project.data = serde_json::json!({"id":store.project,"name":"project","config":{}});
        let mut commit = Record::new("commit");
        commit.states.push(state);
        commit.lines.insert(
            line_id,
            LineChange {
                before: None,
                after: Some(LineState {
                    name: "main".into(),
                    head: state_id.clone(),
                    owner: ctx.caller.uid,
                    machine: store.machine.clone(),
                }),
                working_before: None,
                working_after: None,
            },
        );
        let mut lines = Vec::new();
        for (index, mut record) in [first, project, commit].into_iter().enumerate() {
            record.id = crate::ids::new_id();
            record.machine = store.machine.clone();
            record.project = store.project.clone();
            record.seq = index as u64 + 1;
            lines.push(crate::oplog::sign_line(&record, &store.key).unwrap());
        }
        store
            .backend
            .put_new(
                &format::segment_name(&store.machine, 1, 3),
                keys::encrypt(&descriptor.recipient, format!("{}\n", lines.join("\n")).as_bytes()).unwrap(),
            )
            .unwrap();
        let source = local.join("files");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("notes.txt"), b"portable file bytes\n").unwrap();
        upload_state(&store, &descriptor, &state_id, &source, None, &mut BTreeMap::new(), None, None).unwrap();
        std::fs::remove_dir_all(local).unwrap();
    }
}
