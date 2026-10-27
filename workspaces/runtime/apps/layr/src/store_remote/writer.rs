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
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Clone, Serialize, Deserialize)]
struct Pushed {
    attempt: String,
    tree: Tree,
    depth: usize,
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
    let (paper, mut vault) = if let Some(text) = existing_file {
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
    let paper_slot = keys::seal_slot(&vault, &keys::recipient_of(&paper)?)?;
    if !store.backend.put_new("slots/paper.age", paper_slot)? {
        let slot = store.backend.get("slots/paper.age")?.context("paper slot disappeared")?;
        vault = keys::open_slot(&slot, &paper).map_err(|_| anyhow::anyhow!("store is being created by another writer"))?;
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
    Ok((path.to_string_lossy().into_owned(), keys::recipient_of(&paper)?))
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
    if opts.caches && serde_json::from_value::<crate::model::StateKind>(serde_json::json!("cache")).is_err() {
        bail!("store --caches is not available until cache states exist");
    }
    store.private_dir()?;
    let descriptor = store.descriptor()?;
    let blind_layer = store.layer.is_some() && !store.backend.can_read();
    let mut report = Report { omitted: reader::omitted_cache_refs(repo, opts)?, ..Default::default() };
    repo.p.verify_records()?;
    // A list is enough for a writer without permission to read encrypted objects.
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
        let first = oplog::parse_line(&lines[0], None)?.record.seq;
        let last = oplog::parse_line(lines.last().expect("nonempty"), None)?.record.seq;
        let key = format::segment_name(&machine, first, last);
        let mut bytes = lines.join("\n").into_bytes();
        bytes.push(b'\n');
        // The name and signed plaintext are stable after a lost response. 412 means done.
        store.backend.put_new(&key, keys::encrypt(&descriptor.recipient, &bytes)?)?;
        report.records += lines.len();
    }
    let mut present = if blind_layer { BTreeMap::new() } else { reader::attempts(&store.backend)? };
    let index_file = store.dir.join("pushed.json");
    let mut index: BTreeMap<String, Pushed> = if index_file.exists() {
        serde_json::from_slice(&std::fs::read(&index_file)?).context("invalid local pushed index")?
    } else {
        BTreeMap::new()
    };
    // Only committed remote trees can be reused as delta bases.
    if blind_layer {
        // A write token cannot list epoch records or states. The local index contains only committed uploads.
        present.extend(index.iter().map(|(id, pushed)| (id.clone(), pushed.attempt.clone())));
    } else {
        let committed: std::collections::BTreeSet<String> = store.backend.list("states/")?.into_iter().map(|o| o.key).collect();
        index.retain(|id, pushed| committed.contains(&format::tree_name(id, &pushed.attempt)));
    }
    for (state, pushed) in &index {
        if !crate::model::is_uuid(state) || !format::valid_attempt(&pushed.attempt) || pushed.depth >= format::FULL_TREE_EVERY {
            bail!("invalid local pushed index metadata");
        }
        Tree::parse(&pushed.tree.canonical()?)?;
    }
    // Other writers' states are usable bases only if this writer has a readable slot.
    if let Some(identity) = store.identity(&descriptor, opts.paper_key.as_deref()).ok().filter(|_| store.backend.can_read()) {
        let records = reader::records(&store.backend, &identity, &descriptor.project)?;
        let mut source = reader::Reader::new(&store.backend, &identity, &records.keys);
        for (id, attempt) in &present {
            if index.contains_key(id) {
                continue;
            }
            let (header, _, tree) = source.tree(id, attempt)?;
            let depth = if header.tree == "full" { 0 } else { format::FULL_TREE_EVERY - 1 };
            index.insert(id.clone(), Pushed { attempt: attempt.clone(), tree, depth });
        }
    }
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
        states.push(state);
    }
    states.sort_by(|a, b| (a.time, &a.id).cmp(&(b.time, &b.id)));
    // Every state that leaves the machine passes the secret detector before any upload.
    for state in &states {
        crate::secrets::check_state(ctx, repo, &state.id)?;
    }
    // Parents first, including a parent whose time is newer than its child.
    while !states.is_empty() {
        let position = states
            .iter()
            .position(|s| !s.parents.iter().any(|p| states.iter().any(|other| &other.id == p)))
            .context("local state ancestry has a cycle")?;
        let state = states.remove(position);
        let base =
            reader::state_ancestry(repo, &state.id)?.into_iter().skip(1).find_map(|id| index.get(&id).cloned().map(|p| (id, p)));
        let mut hashes = BTreeMap::new();
        for pushed in index.values() {
            for entry in pushed.tree.files.values() {
                hashes.entry(entry.2.clone()).or_insert_with(|| entry.3.clone());
            }
        }
        let root = repo.state_root(&state.id)?;
        let base_root = base.as_ref().map(|(id, _)| repo.state_root(id)).transpose()?;
        let (pushed, files, bytes) = upload_state(
            store,
            &descriptor,
            &state.id,
            &root,
            base.as_ref(),
            &hashes,
            base_root.as_deref(),
            btrfs_identity(&root, &state.subvol),
        )?;
        index.insert(state.id.clone(), pushed);
        fsutil::atomic_write(&index_file, &serde_json::to_vec(&index)?, 0o600)?;
        report.states += 1;
        report.state_ids.push(state.id.clone());
        report.files += files;
        report.bytes += bytes;
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

fn upload_state(
    store: &Store,
    descriptor: &Descriptor,
    state: &str,
    root: &Path,
    base: Option<&(String, Pushed)>,
    hashes: &BTreeMap<String, Vec<Link>>,
    base_root: Option<&Path>,
    btrfs: Option<format::BtrfsIdentity>,
) -> Result<(Pushed, usize, u64)> {
    let started = std::time::Instant::now();
    let mut random = [0u8; 4];
    rand::rngs::OsRng.fill_bytes(&mut random);
    let attempt = hex::encode(random);
    let mut files = BTreeMap::new();
    let mut hashes = hashes.clone();
    let mut current = Vec::new();
    let mut part = 0;
    let mut frames = 0;
    let mut bytes = 0;
    walk_state(root, &mut |path, meta| {
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
    })?;
    if !current.is_empty() {
        put_part(store, descriptor, state, &attempt, part, current)?;
        part += 1;
    }
    let tree = Tree { files };
    let canonical_tree = tree.canonical()?;
    Tree::parse(&canonical_tree)?;
    let delta_base = base.filter(|(_, p)| p.depth + 1 < format::FULL_TREE_EVERY);
    let base_bytes = delta_base.map(|(_, p)| p.tree.canonical()).transpose()?;
    let body = zst::compress(&canonical_tree, base_bytes.as_deref())?;
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
    if !store.backend.put_new(&format::tree_name(state, &attempt), keys::encrypt(&descriptor.recipient, &object)?)? {
        bail!("state attempt already exists; retry with a new attempt");
    }
    bytes += body.len() as u64;
    Ok((Pushed { attempt, tree, depth: delta_base.map_or(0, |(_, p)| p.depth + 1) }, frames, bytes))
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
            let opts = super::super::ConfigureOptions { paper_key_out: Some(path.clone()) };
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
        store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")) }).unwrap();
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
    fn an_existing_key_file_is_not_reused_without_an_openable_unfinished_slot() {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        std::fs::create_dir_all(&root).unwrap();
        let ctx = context(&root);
        let mut store = remote(&root, &ctx);
        let path = root.join("keep.txt");
        fsutil::atomic_write(&path, b"do not replace", 0o600).unwrap();
        assert!(store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(path.clone()) }).is_err());
        assert_eq!(std::fs::read(&path).unwrap(), b"do not replace");
        assert!(store.backend.get("store.json").unwrap().is_none());
        assert!(store
            .configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(ctx.store.root.join("bad.txt")) })
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
                        store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(own.join("paper.txt")) });
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
        store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")) }).unwrap();
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
                upload_state(&store, &descriptor, &id, &source, base.as_ref(), &BTreeMap::new(), base_root.as_deref(), None)
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
        let hashes = last.tree.files.values().map(|e| (e.2.clone(), e.3.clone())).collect();
        let duplicate = crate::ids::new_id();
        let (dedup, frames, _) =
            upload_state(&store, &descriptor, &duplicate, base_root.as_deref().unwrap(), None, &hashes, None, None).unwrap();
        assert_eq!(frames, 0);
        assert_eq!(dedup.tree.files["a"].3, last.tree.files["a"].3);
        // A second complete upload after a lost response still represents one logical state.
        upload_state(&store, &descriptor, &id, base_root.as_deref().unwrap(), None, &hashes, None, None).unwrap();
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
        store.configure(&ctx, &super::super::ConfigureOptions { paper_key_out: Some(root.join("paper")) }).unwrap();
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
        let (pushed, _, _) = upload_state(&store, &descriptor, &id, &source, None, &BTreeMap::new(), None, None).unwrap();
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
                upload_state(&store, &descriptor, &concurrent, &source, None, &BTreeMap::new(), None, None).unwrap().0
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
        upload_state(&store, &descriptor, &state_id, &source, None, &BTreeMap::new(), None, None).unwrap();
        std::fs::remove_dir_all(local).unwrap();
    }
}
