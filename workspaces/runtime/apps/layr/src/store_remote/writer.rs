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

#[derive(Default)]
struct ContentHashes {
    entries: BTreeMap<String, Vec<Link>>,
    dirty: BTreeMap<String, Vec<Link>>,
    tentative: BTreeMap<String, (Vec<Link>, i64)>,
    pending: BTreeMap<String, Vec<Entry>>,
}
impl ContentHashes {
    fn get(&self, sha: &str) -> Option<&Vec<Link>> {
        self.entries.get(sha).or_else(|| self.tentative.get(sha).map(|value| &value.0))
    }
    fn insert(&mut self, sha: String, links: Vec<Link>) {
        self.dirty.insert(sha.clone(), links.clone());
        self.entries.insert(sha, links);
    }
    fn seed(&mut self, sha: String, links: Vec<Link>) {
        if !self.entries.contains_key(&sha) {
            self.insert(sha, links);
        }
    }
    fn pending_part(&mut self, object: String, modified: i64, entries: Vec<Entry>) {
        for entry in &entries {
            if !self.entries.contains_key(&entry.2) {
                self.tentative.insert(entry.2.clone(), (entry.3.clone(), modified));
            }
        }
        self.pending.insert(object, entries);
    }
    fn protect(&mut self, tree: &Tree) -> Vec<String> {
        let objects: BTreeSet<_> = tree.files.values().flat_map(|entry| entry.3.iter().map(Link::object)).collect();
        let protected: Vec<_> = self.pending.keys().filter(|object| objects.contains(*object)).cloned().collect();
        for object in &protected {
            for entry in self.pending.remove(object).unwrap() {
                self.tentative.remove(&entry.2);
                self.seed(entry.2, entry.3);
            }
        }
        protected
    }
    fn deadline(&self, tree: &Tree, started: i64) -> i64 {
        tree.files.values().filter_map(|entry| self.tentative.get(&entry.2).map(|value| value.1)).fold(started, i64::min)
            + 23 * 3600
    }
}

fn push_now() -> i64 {
    #[cfg(test)]
    if let Some(path) = std::env::var_os("LAYR_TEST_PUSH_CLOCK") {
        return std::fs::read_to_string(path).unwrap().trim().parse().unwrap();
    }
    chrono::Utc::now().timestamp()
}

// Completed uploads are durable remotely; both local caches can be discarded.
const CHECKPOINT_STATES: usize = 128;
const RECORD_SEGMENT_BYTES: usize = 4 << 20;
const INDEX_STATES: usize = 16_384;
#[cfg(test)]
thread_local! {
    static PUSH_STOP_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
    static PUSH_DETERMINISTIC: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    static PUSH_CACHE_DISABLED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}
#[derive(Clone, Serialize, Deserialize)]
struct Cached {
    attempt: String,
    depth: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    root: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct PushIndex {
    format: u32,
    states: BTreeMap<String, Cached>,
}
fn valid_cached(state: &str, entry: &Cached) -> bool {
    crate::model::is_uuid(state)
        && format::valid_attempt(&entry.attempt)
        && entry.depth < format::FULL_TREE_EVERY
        && entry.root.as_ref().is_none_or(|root| root.len() == 64 && hex::decode(root).is_ok())
}
fn cache_tree(cache: &mut Option<super::push_cache::TreeCache>, pushed: &Pushed, uploaded_bytes: u64) -> Cached {
    Cached {
        attempt: pushed.attempt.clone(),
        depth: pushed.depth,
        root: cache.as_mut().and_then(|cache| {
            cache.record_upload(uploaded_bytes).ok()?;
            cache.put(&pushed.tree)
        }),
    }
}
fn cached_tree(cache: &Option<super::push_cache::TreeCache>, entry: &Cached) -> Option<Pushed> {
    let tree = cache.as_ref()?.get(entry.root.as_deref()?).ok()?;
    let canonical = tree.canonical().ok()?;
    Some(Pushed { attempt: entry.attempt.clone(), tree, depth: entry.depth, canonical })
}
fn load_index(dir: &Path) -> Result<BTreeMap<String, Cached>> {
    let path = dir.join("pushed-v2.json.zst");
    let bytes = if path.exists() {
        match std::fs::read(&path).ok().and_then(|bytes| zst::decompress_with_limit(&bytes, None, 8 << 20).ok()) {
            Some(bytes) => bytes,
            None => return Ok(BTreeMap::new()),
        }
    } else if dir.join("pushed.json").exists() {
        std::fs::read(dir.join("pushed.json"))?
    } else {
        return Ok(BTreeMap::new());
    };
    let value: serde_json::Value = match serde_json::from_slice(&bytes) {
        Ok(value) => value,
        Err(_) => return Ok(BTreeMap::new()),
    };
    let mut states = if value.get("format").is_some() {
        let Ok(index) = serde_json::from_value::<PushIndex>(value) else {
            return Ok(BTreeMap::new());
        };
        if index.format != 1 && index.format != 2 {
            return Ok(BTreeMap::new());
        }
        index.states
    } else {
        let Ok(legacy) = serde_json::from_value::<BTreeMap<String, Pushed>>(value) else {
            return Ok(BTreeMap::new());
        };
        legacy.into_iter().map(|(id, pushed)| (id, Cached { attempt: pushed.attempt, depth: pushed.depth, root: None })).collect()
    };
    states.retain(|id, entry| valid_cached(id, entry));
    Ok(states)
}
fn checkpoint(dir: &Path, index: &BTreeMap<String, Cached>) -> Result<()> {
    // UUIDv7 sorting keeps recent completions. Evicted metadata is rediscovered by state prefix.
    let states = index.iter().rev().take(INDEX_STATES).map(|(id, entry)| (id.clone(), entry.clone())).collect();
    let bytes = serde_json::to_vec(&PushIndex { format: 2, states })?;
    fsutil::atomic_write(&dir.join("pushed-v2.json.zst"), &zst::compress(&bytes, None)?, 0o600)?;
    // Only retire old formats after the replacement metadata is safely published.
    if dir.join("pushed.json").exists() {
        std::fs::remove_file(dir.join("pushed.json"))?;
    }
    if dir.join("pushed").exists() {
        std::fs::remove_dir_all(dir.join("pushed"))?;
    }
    Ok(())
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
        Ok(std::fs::OpenOptions::new().read(true).write(true).custom_flags(libc::O_NOFOLLOW | libc::O_NOCTTY).open("/dev/tty")?)
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
    let mut journal = super::push_journal::Journal::open(&store.dir)?;
    for done in journal.completed()? {
        let root = index.get(&done.state).filter(|entry| entry.attempt == done.attempt).and_then(|entry| entry.root.clone());
        index.insert(done.state, Cached { attempt: done.attempt, depth: done.depth, root });
    }
    // The old per-state tree files are unnecessary even before a new checkpoint. Free
    // them first so upgrading a disk-full store does not need extra scratch space.
    if store.dir.join("pushed").exists() {
        std::fs::remove_dir_all(store.dir.join("pushed"))?;
    }
    let mut tree_cache = super::push_cache::TreeCache::open(&store.dir).ok();
    #[cfg(test)]
    if PUSH_CACHE_DISABLED.with(|disabled| disabled.get()) {
        tree_cache = None;
    }
    // Completed immutable trees are never collected. Cache metadata is written only after
    // publication, so a regular push need not enumerate every historical remote object.
    index.retain(|id, entry| valid_cached(id, entry));
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
    let mut recovered = Vec::new();
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
                // A fresh writer can discover another writer's completed history without
                // having a reader slot. Only an existing transfer journal needs recovery.
                if journal.resuming {
                    recovered.push(id.clone());
                }
                present.insert(id.clone(), attempt.clone());
                // No checkpoint authenticates a local root for this publication.
                // Ignore every remaining local node and hydrate its signed remote tree.
                index.insert(id, Cached { attempt, depth: format::FULL_TREE_EVERY - 1, root: None });
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
    let mut hashes = ContentHashes { entries: journal.hashes()?, ..Default::default() };
    let pending = journal.pending()?;
    if !pending.is_empty() || !recovered.is_empty() {
        let started = std::time::Instant::now();
        journal.stats.seed_started_ms = crate::ids::now_ms();
        let identity = store.identity(&descriptor, opts.paper_key.as_deref())?;
        if remote_records.is_none() {
            remote_records = Some(reader::records(&store.backend, &identity, &descriptor.project)?);
        }
        let records = remote_records.as_ref().unwrap();
        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
        for part in pending {
            journal.stats.seed_pending_parts += 1;
            journal.stats.seed_metadata_lists += 1;
            let prefix = part.object.rsplit_once('/').unwrap().0.to_string() + "/";
            let modified =
                store.backend.list(&prefix)?.into_iter().find(|object| object.key == part.object).map(|object| object.modified);
            let modified = modified.filter(|modified| push_now() - *modified < 12 * 3600);
            if let Some(modified) = modified.filter(|_| part.entries.iter().all(|entry| source.read(entry).is_ok())) {
                journal.validate_part(&part.object, modified)?;
                hashes.pending_part(part.object, modified, part.entries);
            } else {
                journal.discard_part(&part.object)?;
            }
        }
        for id in recovered {
            let attempt = &present[&id];
            let (_, _, tree) = source.tree(&id, attempt)?;
            let depth = source.tree_depth(&id, attempt)?;
            let protected = hashes.protect(&tree);
            for entry in tree.files.values() {
                hashes.seed(entry.2.clone(), entry.3.clone());
            }
            journal.checkpoint(&id, attempt, depth, &hashes.dirty, &protected)?;
            hashes.dirty.clear();
            index.get_mut(&id).unwrap().depth = depth;
            journal.stats.seed_states += 1;
        }
        journal.stats.seed_reader_gets = source.get_count();
        journal.stats.seed_finished_ms = crate::ids::now_ms();
        journal.stats.seed_seconds = started.elapsed().as_secs_f64();
    }
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
                let mut pushed = index.get(&id).and_then(|entry| cached_tree(&tree_cache, entry));
                if pushed.is_none() && store.backend.can_read() {
                    if let Ok(identity) = store.identity(&descriptor, opts.paper_key.as_deref()) {
                        if remote_records.is_none() {
                            remote_records = Some(reader::records(&store.backend, &identity, &descriptor.project)?);
                        }
                        let records = remote_records.as_ref().unwrap();
                        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
                        let attempt = &present[&id];
                        let (_, canonical, tree) = source.tree(&id, attempt)?;
                        let depth = source.tree_depth(&id, attempt)?;
                        let restored = Pushed { attempt: attempt.clone(), tree, depth, canonical };
                        index.insert(id.clone(), cache_tree(&mut tree_cache, &restored, 0));
                        pushed = Some(restored);
                    }
                }
                pushed.map(|pushed| {
                    for entry in pushed.tree.files.values() {
                        hashes.seed(entry.2.clone(), entry.3.clone());
                    }
                    (id, pushed)
                })
            }
            None => None,
        };
        drop(stage);
        let stage = crate::trace::span("store upload state");
        let root = repo.state_root(&state.id)?;
        // A base that a history limit deleted here (docs/cache-overlays.md) cannot be compared:
        // the state goes up as a complete tree.
        let base = base.filter(|(id, _)| repo.p.has_state(id));
        let base_root = base.as_ref().map(|(id, _)| repo.state_root(id)).transpose()?;
        let (pushed, files, bytes) = upload_state_journal(
            store,
            &descriptor,
            &state.id,
            &root,
            base.as_ref(),
            &mut hashes,
            base_root.as_deref(),
            btrfs_identity(&root, &state.subvol),
            Some(&mut journal),
        )?;
        drop(stage);
        let stage = crate::trace::span("store checkpoint");
        let protected = hashes.protect(&pushed.tree);
        journal.checkpoint(&state.id, &pushed.attempt, pushed.depth, &hashes.dirty, &protected)?;
        hashes.dirty.clear();
        present.insert(state.id.clone(), pushed.attempt.clone());
        index.insert(state.id.clone(), cache_tree(&mut tree_cache, &pushed, bytes));
        #[cfg(test)]
        PUSH_STOP_AFTER.with(|stop| -> Result<()> {
            if stop.get() == Some(report.states + 1) {
                return Err(std::io::Error::from_raw_os_error(libc::ENOSPC).into());
            }
            Ok(())
        })?;
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
    let stats = journal.finish()?;
    if std::env::var_os("LAYR_TRACE").is_some() {
        eprintln!("trace push-journal {}", serde_json::to_string(&stats)?);
    }
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

fn upload_state_journal(
    store: &Store,
    descriptor: &Descriptor,
    state: &str,
    root: &Path,
    base: Option<&(String, Pushed)>,
    hashes: &mut ContentHashes,
    base_root: Option<&Path>,
    btrfs: Option<format::BtrfsIdentity>,
    mut journal: Option<&mut super::push_journal::Journal>,
) -> Result<(Pushed, usize, u64)> {
    let started = std::time::Instant::now();
    let started_at = push_now();
    let mut random = [0u8; 4];
    rand::rngs::OsRng.fill_bytes(&mut random);
    let attempt = hex::encode(random);
    #[cfg(test)]
    let attempt = if PUSH_DETERMINISTIC.with(|enabled| enabled.get()) {
        hex::encode(Sha256::digest(state.as_bytes()))[..8].to_string()
    } else {
        attempt
    };
    let delta_root = base_root.filter(|base| crate::storage::is_subvolume(base) && crate::storage::is_subvolume(root));
    let mut files =
        if delta_root.is_some() { base.map(|(_, p)| p.tree.files.clone()).unwrap_or_default() } else { BTreeMap::new() };
    let mut current = Vec::new();
    let mut part_entries = Vec::new();
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
            if let Some(journal) = journal.as_deref_mut() {
                journal.prepare_part(state, &attempt, part, &part_entries)?;
            }
            put_part(store, descriptor, state, &attempt, part, std::mem::take(&mut current), &part_entries)?;
            part_entries.clear();
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
        let entry = Entry(mode, data.len() as u64, sha, chain);
        part_entries.push(entry.clone());
        files.insert(path.to_string(), entry);
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
    drop(visit);
    if !current.is_empty() {
        if let Some(journal) = journal.as_deref_mut() {
            journal.prepare_part(state, &attempt, part, &part_entries)?;
        }
        put_part(store, descriptor, state, &attempt, part, current, &part_entries)?;
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
    push_failpoint("before_publish", state);
    #[cfg(test)]
    if std::env::var("LAYR_TEST_GC_STATE").ok().as_deref() == Some(state) {
        std::fs::write(std::env::var_os("LAYR_TEST_PUSH_CLOCK").unwrap(), chrono::Utc::now().timestamp().to_string()).unwrap();
        store.gc(false).unwrap();
    }
    // GC may collect unpublished parts at server modified +24h. Pending links keep
    // that server timestamp across crashes; publish at least one hour before it.
    if started.elapsed().as_secs() >= 23 * 3600 || push_now() >= hashes.deadline(&tree, started_at) {
        bail!("state upload or recovered part reached its safe publication deadline; retry with a new attempt");
    }
    drop(stage);
    let _publish = crate::trace::span("store tree publish");
    if !store.backend.put_new(&format::tree_name(state, &attempt), keys::encrypt(&descriptor.recipient, &object)?)? {
        bail!("state attempt already exists; retry with a new attempt");
    }
    push_failpoint("after_publish", state);
    bytes += body.len() as u64;
    Ok((Pushed { attempt, tree, depth: delta_base.map_or(0, |(_, p)| p.depth + 1), canonical: canonical_tree }, frames, bytes))
}

#[cfg(test)]
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
    let mut index = ContentHashes { entries: std::mem::take(hashes), ..Default::default() };
    let result = upload_state_journal(store, descriptor, state, root, base, &mut index, base_root, btrfs, None);
    *hashes = index.entries;
    result
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

fn push_failpoint(point: &str, state: &str) {
    #[cfg(test)]
    if std::env::var("LAYR_TEST_KILL_POINT").ok().as_deref() == Some(point)
        && std::env::var("LAYR_TEST_KILL_STATE").ok().as_deref() == Some(state)
    {
        std::process::Command::new("kill").args(["-KILL", &std::process::id().to_string()]).status().unwrap();
    }
    let _ = (point, state);
}

fn put_part(
    store: &Store,
    descriptor: &Descriptor,
    state: &str,
    attempt: &str,
    part: u32,
    data: Vec<u8>,
    entries: &[Entry],
) -> Result<()> {
    push_failpoint("before_part", state);
    let _publish = crate::trace::span("store part publish");
    if part >= format::MAX_PARTS {
        bail!("state exceeds 10000 data parts");
    }
    if !store.backend.put_new(&format::data_name(state, attempt, part), keys::encrypt(&descriptor.recipient, &data)?)? {
        bail!("state attempt has an existing part; retry with a new attempt");
    }
    #[cfg(test)]
    if let Ok(path) = std::env::var("LAYR_TEST_PUT_LOG") {
        use std::io::Write;
        let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
        writeln!(file, "{}", serde_json::json!({"object":format::data_name(state,attempt,part),"hashes":entries.iter().map(|entry|entry.2.clone()).collect::<Vec<_>>()})).unwrap();
        file.sync_all().unwrap();
    }
    let _ = entries;
    push_failpoint("after_part", state);
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
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
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

    fn branch_fixture<'a>(ctx: &'a Ctx, store: &Store) -> Repo<'a> {
        use crate::model::{Record, StateRec};
        let dir = ctx.store.projects_dir().join("p");
        let existing = dir.join("meta/db/layr.sqlite").exists();
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        std::fs::create_dir_all(dir.join("states")).unwrap();
        let project = crate::store::Project {
            store: &ctx.store,
            name: "p".into(),
            dir: dir.clone(),
            id: store.project.clone(),
            db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
        };
        if !existing {
            let mut record = Record::new("project");
            record.data = serde_json::json!({"id":store.project,"name":"p","config":{}});
            project.append(record, &ctx.actor()).unwrap();
            let mut parents = Vec::<String>::new();
            let mut first = None;
            for n in 0..4 {
                let id = crate::ids::new_id();
                std::fs::create_dir(project.state_path(&id)).unwrap();
                std::fs::write(project.state_path(&id).join("payload"), format!("version {}\n", if n == 3 { 1 } else { n }))
                    .unwrap();
                std::fs::write(project.state_path(&id).join("stable"), b"unchanged parent content\n").unwrap();
                let state: StateRec = serde_json::from_value(serde_json::json!({"id":id,"kind":"commit",
                    "subvol":crate::ids::new_id(),"author":{"name":"Test","email":"test@example.test"},
                    "parents":parents,"time":crate::ids::now_ms()}))
                .unwrap();
                let mut record = Record::new("state");
                record.states.push(state);
                project.append(record, &ctx.actor()).unwrap();
                if n == 0 {
                    first = Some(id.clone());
                }
                parents = vec![if n == 1 { first.clone().unwrap() } else { id }];
            }
        }
        Repo { p: project }
    }

    #[test]
    fn enospc_after_publication_resumes_with_missing_or_corrupt_cache() {
        for corrupt in [false, true] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            store
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() },
                )
                .unwrap();
            let repo = branch_fixture(&ctx, &store);
            // With no cache, the third state must hydrate a parent uploaded in this same push.
            PUSH_DETERMINISTIC.with(|enabled| enabled.set(true));
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(corrupt));
            PUSH_STOP_AFTER.with(|stop| stop.set(Some(3)));
            let error = push(&store, &ctx, &repo, &Options::default()).unwrap_err();
            PUSH_STOP_AFTER.with(|stop| stop.set(None));
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(false));
            assert_eq!(error.downcast_ref::<std::io::Error>().unwrap().raw_os_error(), Some(libc::ENOSPC));
            assert!(!store.dir.join("pushed-v2.json.zst").exists());
            let before: BTreeMap<_, _> = store
                .backend
                .list("")
                .unwrap()
                .into_iter()
                .map(|object| {
                    let bytes = store.backend.get(&object.key).unwrap().unwrap();
                    (object.key, Sha256::digest(bytes))
                })
                .collect();
            assert_eq!(reader::attempts(&store.backend).unwrap().len(), 3);
            if corrupt {
                std::fs::write(store.dir.join("push-cache.sqlite3"), b"damaged cache").unwrap();
            }
            let report = push(&store, &ctx, &repo, &Options::default()).unwrap();
            assert_eq!(report.states, 1);
            assert_eq!(reader::attempts(&store.backend).unwrap().len(), 4);
            assert_eq!(store.backend.list("states/").unwrap().iter().filter(|o| o.key.ends_with(".tree.age")).count(), 4);
            for (key, digest) in before {
                assert_eq!(Sha256::digest(store.backend.get(&key).unwrap().unwrap()), digest);
            }
            store.verify(true).unwrap();
            // A zero-capacity cache must publish byte-for-byte the same compressed object payloads as
            // the warm-cache push, including the signed tree compression/checkpoint cadence.
            let mut empty_cache = remote(&root, &ctx);
            empty_cache.backend = super::super::backend::Backend::file(root.join("uncached-remote"));
            empty_cache.dir = root.join("uncached-meta");
            empty_cache
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("uncached-paper")), ..Default::default() },
                )
                .unwrap();
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(true));
            assert_eq!(push(&empty_cache, &ctx, &repo, &Options::default()).unwrap().states, 4);
            PUSH_CACHE_DISABLED.with(|disabled| disabled.set(false));
            PUSH_DETERMINISTIC.with(|enabled| enabled.set(false));
            let payloads = |store: &Store| -> BTreeMap<String, Vec<u8>> {
                let descriptor = store.descriptor().unwrap();
                let identity = store.identity(&descriptor, None).unwrap();
                store
                    .backend
                    .list("states/")
                    .unwrap()
                    .into_iter()
                    .map(|object| {
                        let payload = store.backend.decrypted(&object.key, &identity).unwrap();
                        (object.key, payload)
                    })
                    .collect()
            };
            // age adds randomized header stanzas whose wire lengths can differ even
            // for identical input. Compare the exact bytes passed to encryption.
            assert_eq!(payloads(&store), payloads(&empty_cache));
            // Losing both caches must only rediscover immutable uploads, never republish them.
            std::fs::remove_file(store.dir.join("push-cache.sqlite3")).unwrap();
            std::fs::remove_file(store.dir.join("pushed-v2.json.zst")).unwrap();
            assert_eq!(push(&store, &ctx, &repo, &Options::default()).unwrap().states, 0);
            drop(repo);
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    #[ignore = "subprocess helper for crash_content_hashes"]
    fn crash_push_process() {
        // CI runs ignored tests too (--include-ignored). Without the variable this is not a
        // subprocess run, so do nothing. The crash tests fail if their subprocess does not push.
        let Some(root) = std::env::var_os("LAYR_TEST_CRASH_ROOT").map(std::path::PathBuf::from) else {
            return;
        };
        let ctx = context(&root);
        let store = remote(&root, &ctx);
        let repo = branch_fixture(&ctx, &store);
        PUSH_CACHE_DISABLED.with(|disabled| disabled.set(true));
        push(&store, &ctx, &repo, &Options::default()).unwrap();
    }

    #[test]
    fn crash_content_hashes_are_reused_at_each_publication_boundary() {
        use std::{
            io::Write,
            os::unix::process::ExitStatusExt,
            process::{Command, Stdio},
        };
        for (point, corrupt) in [("before_part", false), ("after_part", false), ("after_publish", false), ("after_part", true)] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            store
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() },
                )
                .unwrap();
            let repo = branch_fixture(&ctx, &store);
            let states =
                parents_first(repo.p.db.present_states().unwrap().into_iter().map(|(id, _)| repo.state(&id).unwrap()).collect())
                    .unwrap();
            let kill_state = states[2].id.clone();
            drop(repo);
            let puts = root.join("puts.jsonl");
            let invoke = |kill: bool, log: &str| {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--ignored", "--exact", "store_remote::writer::tests::crash_push_process", "--nocapture"])
                    .env("LAYR_TEST_CRASH_ROOT", &root)
                    .env("LAYR_TEST_PUT_LOG", &puts)
                    .env("LAYR_TRACE", "1")
                    .env_remove("LAYR_TEST_KILL_POINT")
                    .env_remove("LAYR_TEST_KILL_STATE")
                    .stdout(Stdio::null())
                    .stderr(std::fs::File::create(root.join(log)).unwrap());
                if kill {
                    command.env("LAYR_TEST_KILL_POINT", point).env("LAYR_TEST_KILL_STATE", &kill_state);
                }
                command.status().unwrap()
            };
            assert_eq!(invoke(true, "killed.log").signal(), Some(libc::SIGKILL));
            let read_puts = || -> Vec<serde_json::Value> {
                std::fs::read_to_string(&puts).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect()
            };
            let before_puts = read_puts();
            let damaged = corrupt.then(|| before_puts.last().unwrap()["object"].as_str().unwrap().to_string());
            if let Some(key) = &damaged {
                let path = root.join("remote").join(key);
                let mut bytes = std::fs::read(&path).unwrap();
                *bytes.last_mut().unwrap() ^= 1;
                std::fs::write(path, bytes).unwrap();
            }
            let existing_hashes: BTreeSet<String> = before_puts
                .iter()
                .filter(|row| damaged.as_deref() != row["object"].as_str())
                .flat_map(|row| row["hashes"].as_array().unwrap().iter().map(|value| value.as_str().unwrap().to_string()))
                .collect();
            let before: BTreeMap<String, _> = store
                .backend
                .list("")
                .unwrap()
                .into_iter()
                .filter(|object| damaged.as_deref() != Some(object.key.as_str()))
                .map(|object| {
                    let digest = Sha256::digest(store.backend.get(&object.key).unwrap().unwrap());
                    (object.key, digest)
                })
                .collect();
            assert!(invoke(false, "resumed.log").success());
            let after_puts = read_puts();
            for row in &after_puts[before_puts.len()..] {
                for hash in row["hashes"].as_array().unwrap() {
                    assert!(!existing_hashes.contains(hash.as_str().unwrap()), "{point}: duplicate content {hash}");
                }
            }
            for (key, digest) in &before {
                assert_eq!(&Sha256::digest(store.backend.get(key).unwrap().unwrap()), digest);
            }
            assert_eq!(reader::attempts(&store.backend).unwrap().len(), 4);
            store.verify(true).unwrap();
            let descriptor = store.descriptor().unwrap();
            let identity = store.identity(&descriptor, None).unwrap();
            let records = reader::records(&store.backend, &identity, &descriptor.project).unwrap();
            let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
            let mut referenced = BTreeSet::new();
            for (state, attempt) in reader::attempts(&store.backend).unwrap() {
                for entry in source.tree(&state, &attempt).unwrap().2.files.into_values() {
                    referenced.extend(entry.3.iter().map(|link| link.object()));
                }
            }
            for object in store.backend.list("states/").unwrap() {
                if format::parse_data_name(&object.key).is_some() {
                    if damaged.as_deref() == Some(object.key.as_str()) {
                        assert!(!referenced.contains(&object.key), "corrupt pending part was reused");
                        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(90000);
                        std::fs::File::options()
                            .write(true)
                            .open(root.join("remote").join(&object.key))
                            .unwrap()
                            .set_modified(old)
                            .unwrap();
                    } else {
                        assert!(referenced.contains(&object.key), "{point}: orphan {}", object.key);
                    }
                }
            }
            let collected = store.gc(false).unwrap();
            assert_eq!(collected, damaged.into_iter().collect::<Vec<_>>());
            assert!(!store.dir.join("push-journal.sqlite3").exists());
            let log = std::fs::read_to_string(root.join("resumed.log")).unwrap();
            let stats: serde_json::Value =
                serde_json::from_str(log.lines().find_map(|line| line.strip_prefix("trace push-journal ")).unwrap()).unwrap();
            assert!(stats["seed_states"].as_u64().unwrap() <= 1);
            let report = serde_json::json!({"point":point,"corrupt_pending":corrupt,"preserved_objects":before.len(),"duplicate_content_uploads":0,"orphan_parts":0,"gc_parts":collected.len(),"journal":stats});
            println!("crash recovery {report}");
            if let Some(path) = std::env::var_os("LAYR_TEST_CRASH_REPORT") {
                let mut file = std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap();
                writeln!(file, "{report}").unwrap();
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn pending_part_age_survives_double_crash_and_gc_before_publication() {
        use std::{
            os::unix::process::ExitStatusExt,
            process::{Command, Stdio},
        };
        for mode in ["expired", "double_crash", "gc_before_publish"] {
            let root = std::env::temp_dir().join(crate::ids::new_id());
            std::fs::create_dir_all(&root).unwrap();
            let ctx = context(&root);
            let mut store = remote(&root, &ctx);
            store
                .configure(
                    &ctx,
                    &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() },
                )
                .unwrap();
            let repo = branch_fixture(&ctx, &store);
            let states =
                parents_first(repo.p.db.present_states().unwrap().into_iter().map(|(id, _)| repo.state(&id).unwrap()).collect())
                    .unwrap();
            let target = states[2].id.clone();
            drop(repo);
            let puts = root.join("puts.jsonl");
            let clock = root.join("clock");
            let invoke = |point: Option<&str>, simulated_clock: bool, gc: bool, log: &str| {
                let mut command = Command::new(std::env::current_exe().unwrap());
                command
                    .args(["--ignored", "--exact", "store_remote::writer::tests::crash_push_process", "--nocapture"])
                    .env("LAYR_TEST_CRASH_ROOT", &root)
                    .env("LAYR_TEST_PUT_LOG", &puts)
                    .env_remove("LAYR_TEST_KILL_POINT")
                    .env_remove("LAYR_TEST_KILL_STATE")
                    .env_remove("LAYR_TEST_PUSH_CLOCK")
                    .env_remove("LAYR_TEST_GC_STATE")
                    .stdout(Stdio::null())
                    .stderr(std::fs::File::create(root.join(log)).unwrap());
                if let Some(point) = point {
                    command.env("LAYR_TEST_KILL_POINT", point).env("LAYR_TEST_KILL_STATE", &target);
                }
                if simulated_clock {
                    command.env("LAYR_TEST_PUSH_CLOCK", &clock);
                }
                if gc {
                    command.env("LAYR_TEST_GC_STATE", &target);
                }
                command.status().unwrap()
            };
            assert_eq!(invoke(Some("after_part"), false, false, "first.log").signal(), Some(libc::SIGKILL));
            let initial: Vec<serde_json::Value> =
                std::fs::read_to_string(&puts).unwrap().lines().map(|line| serde_json::from_str(line).unwrap()).collect();
            let part = initial.last().unwrap()["object"].as_str().unwrap().to_string();
            let sha = initial.last().unwrap()["hashes"][0].as_str().unwrap();
            if mode == "double_crash" {
                assert_eq!(invoke(Some("before_publish"), false, false, "second.log").signal(), Some(libc::SIGKILL));
                let journal = super::super::push_journal::Journal::open(&store.dir).unwrap();
                assert!(!journal.hashes().unwrap().contains_key(sha), "unpublished hash escaped its pending object");
                assert!(journal.pending().unwrap().iter().any(|pending| pending.object == part));
            }
            let age = if mode == "expired" { 13 * 3600 } else { 25 * 3600 };
            let old = std::time::SystemTime::now() - std::time::Duration::from_secs(age);
            std::fs::File::options().write(true).open(root.join("remote").join(&part)).unwrap().set_modified(old).unwrap();
            if mode == "gc_before_publish" {
                std::fs::write(&clock, (chrono::Utc::now().timestamp() - 24 * 3600).to_string()).unwrap();
                assert!(!invoke(None, true, true, "gc.log").success());
                assert!(std::fs::read_to_string(root.join("gc.log")).unwrap().contains("safe publication deadline"));
                assert!(store.backend.get(&part).unwrap().is_none());
                assert!(!reader::attempts(&store.backend).unwrap().contains_key(&target));
                store.verify(true).unwrap();
            }
            assert!(invoke(None, false, false, "final.log").success());
            assert_eq!(reader::attempts(&store.backend).unwrap().len(), 4);
            store.verify(true).unwrap();
            let descriptor = store.descriptor().unwrap();
            let identity = store.identity(&descriptor, None).unwrap();
            let records = reader::records(&store.backend, &identity, &descriptor.project).unwrap();
            let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
            for (state, attempt) in reader::attempts(&store.backend).unwrap() {
                for entry in source.tree(&state, &attempt).unwrap().2.files.values() {
                    assert!(entry.3.iter().all(|link| link.object() != part), "{mode}: expired part reused");
                }
            }
            if store.backend.get(&part).unwrap().is_some() {
                std::fs::File::options()
                    .write(true)
                    .open(root.join("remote").join(&part))
                    .unwrap()
                    .set_modified(std::time::SystemTime::now() - std::time::Duration::from_secs(25 * 3600))
                    .unwrap();
                assert_eq!(store.gc(false).unwrap(), vec![part]);
            }
            std::fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    fn rolling_back_journal_rows_only_causes_authenticated_reseeding() {
        use std::{
            os::unix::process::ExitStatusExt,
            process::{Command, Stdio},
        };
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
        let repo = branch_fixture(&ctx, &store);
        let states =
            parents_first(repo.p.db.present_states().unwrap().into_iter().map(|(id, _)| repo.state(&id).unwrap()).collect())
                .unwrap();
        drop(repo);
        let invoke = |point: Option<(&str, &str)>, log: &str| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args(["--ignored", "--exact", "store_remote::writer::tests::crash_push_process", "--nocapture"])
                .env("LAYR_TEST_CRASH_ROOT", &root)
                .env("LAYR_TRACE", "1")
                .env_remove("LAYR_TEST_KILL_POINT")
                .env_remove("LAYR_TEST_KILL_STATE")
                .stdout(Stdio::null())
                .stderr(std::fs::File::create(root.join(log)).unwrap());
            if let Some((point, state)) = point {
                command.env("LAYR_TEST_KILL_POINT", point).env("LAYR_TEST_KILL_STATE", state);
            }
            command.status().unwrap()
        };
        assert_eq!(invoke(Some(("before_part", &states[1].id)), "first.log").signal(), Some(libc::SIGKILL));
        let path = store.dir.join("push-journal.sqlite3");
        let older = root.join("older.sqlite3");
        {
            let db = rusqlite::Connection::open(&path).unwrap();
            db.execute("VACUUM INTO ?1", [older.to_str().unwrap()]).unwrap();
        }
        assert_eq!(invoke(Some(("after_publish", &states[2].id)), "second.log").signal(), Some(libc::SIGKILL));
        let before: BTreeMap<_, _> = store
            .backend
            .list("")
            .unwrap()
            .into_iter()
            .map(|object| {
                let digest = Sha256::digest(store.backend.get(&object.key).unwrap().unwrap());
                (object.key, digest)
            })
            .collect();
        let parts_before = before.keys().filter(|key| format::parse_data_name(key).is_some()).count();
        // Drop the newer WAL as well: restore a coherent older committed database.
        for suffix in ["-wal", "-shm"] {
            let path = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
            if path.exists() {
                std::fs::remove_file(path).unwrap();
            }
        }
        std::fs::copy(older, &path).unwrap();
        assert!(invoke(None, "resumed.log").success());
        for (key, digest) in before {
            assert_eq!(Sha256::digest(store.backend.get(&key).unwrap().unwrap()), digest);
        }
        store.verify(true).unwrap();
        assert_eq!(reader::attempts(&store.backend).unwrap().len(), 4);
        let parts_after =
            store.backend.list("states/").unwrap().iter().filter(|object| format::parse_data_name(&object.key).is_some()).count();
        assert_eq!(parts_before, parts_after);
        let log = std::fs::read_to_string(root.join("resumed.log")).unwrap();
        let stats: serde_json::Value =
            serde_json::from_str(log.lines().find_map(|line| line.strip_prefix("trace push-journal ")).unwrap()).unwrap();
        assert_eq!(stats["seed_states"], 2);
        let report =
            serde_json::json!({"case":"older_journal_copy","extra_uploaded_parts":parts_after-parts_before,"journal":stats});
        if let Some(path) = std::env::var_os("LAYR_TEST_CRASH_REPORT") {
            use std::io::Write;
            writeln!(std::fs::OpenOptions::new().create(true).append(true).open(path).unwrap(), "{report}").unwrap();
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn fresh_push_discards_wal_sidecars_without_their_main_database() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
        let repo = branch_fixture(&ctx, &store);
        store.private_dir().unwrap();
        let path = store.dir.join("push-journal.sqlite3");
        {
            let mut old = super::super::push_journal::Journal::open(&store.dir).unwrap();
            old.checkpoint("old-state", "deadbeef", 0, &BTreeMap::new(), &[]).unwrap();
            old.prepare_part("old-state", "deadbeef", 0, &[]).unwrap();
            for suffix in ["-wal", "-shm"] {
                std::fs::copy(format!("{}{suffix}", path.display()), root.join(format!("old{suffix}"))).unwrap();
            }
        }
        std::fs::remove_file(&path).unwrap();
        for suffix in ["-wal", "-shm"] {
            std::fs::copy(root.join(format!("old{suffix}")), format!("{}{suffix}", path.display())).unwrap();
        }
        {
            let fresh = super::super::push_journal::Journal::open(&store.dir).unwrap();
            assert!(!fresh.resuming);
            assert!(fresh.completed().unwrap().is_empty());
            assert!(fresh.pending().unwrap().is_empty());
            fresh.finish().unwrap();
        }
        assert_eq!(push(&store, &ctx, &repo, &Options::default()).unwrap().states, 4);
        store.verify(true).unwrap();
        for name in ["push-journal.sqlite3", "push-journal.sqlite3-wal", "push-journal.sqlite3-shm"] {
            assert!(!store.dir.join(name).exists());
        }
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn completion_metadata_is_bounded_and_invalid_metadata_rebuilds() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let index: BTreeMap<_, _> = (0..INDEX_STATES + 2)
            .map(|_| (crate::ids::new_id(), Cached { attempt: "0123abcd".into(), depth: 0, root: None }))
            .collect();
        checkpoint(&root, &index).unwrap();
        let restored = load_index(&root).unwrap();
        assert_eq!(restored.len(), INDEX_STATES);
        assert!(!restored.contains_key(index.first_key_value().unwrap().0));
        assert!(restored.contains_key(index.last_key_value().unwrap().0));
        for bytes in [b"damaged".as_slice(), b"{\"format\":2,\"states\":false}".as_slice()] {
            std::fs::write(root.join("pushed-v2.json.zst"), zst::compress(bytes, None).unwrap()).unwrap();
            assert!(load_index(&root).unwrap().is_empty());
        }
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn legacy_pushed_cache_migrates_without_changing_completion() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let id = crate::ids::new_id();
        let pushed =
            Pushed { attempt: "0123abcd".into(), tree: Tree { files: BTreeMap::new() }, depth: 0, canonical: Vec::new() };
        let legacy = BTreeMap::from([(id.clone(), pushed)]);
        std::fs::write(root.join("pushed.json"), serde_json::to_vec(&legacy).unwrap()).unwrap();
        let index = load_index(&root).unwrap();
        assert_eq!(index[&id].attempt, "0123abcd");
        assert_eq!(index[&id].depth, 0);
        assert!(index[&id].root.is_none());
        checkpoint(&root, &index).unwrap();
        assert_eq!(load_index(&root).unwrap()[&id].attempt, "0123abcd");
        let json: serde_json::Value =
            serde_json::from_slice(&zst::decompress(&std::fs::read(root.join("pushed-v2.json.zst")).unwrap(), None).unwrap())
                .unwrap();
        assert_eq!(json["format"], 2);
        assert!(!root.join("pushed.json").exists());
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
        assert!(store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(path.clone()), ..Default::default() })
            .is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"do not replace");
        assert!(store.backend.get("store.json").unwrap().is_none());
        assert!(store
            .configure(
                &ctx,
                &super::super::ConfigureOptions { paper_key_out: Some(ctx.store.root.join("bad.txt")), ..Default::default() }
            )
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
                    let result = store.configure(
                        &ctx,
                        &super::super::ConfigureOptions { paper_key_out: Some(own.join("paper.txt")), ..Default::default() },
                    );
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
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
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
        store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")), ..Default::default() })
            .unwrap();
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
