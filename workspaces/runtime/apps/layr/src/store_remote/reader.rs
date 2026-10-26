//! Read, authenticate and restore immutable records and state attempts.

use super::{
    backend::Backend,
    format::{self, Entry, Header, Tree},
    keys, zst, Options, Report, Store,
};
use crate::{ctx::Ctx, model::Record, oplog, repo::Repo};
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug)]
pub(super) struct Records {
    pub lines: Vec<String>,
    pub keys: BTreeMap<String, VerifyingKey>,
    pub overlaps: usize,
    pub gaps: Vec<String>,
}

pub(super) fn records(backend: &Backend, identity: &str, project: &str) -> Result<Records> {
    records_with_local(backend, identity, project, &[])
}

fn records_with_local(backend: &Backend, identity: &str, project: &str, local: &[String]) -> Result<Records> {
    let mut machines: BTreeMap<String, BTreeMap<u64, String>> = BTreeMap::new();
    let mut overlaps = 0;
    for object in backend.list("records/")? {
        let (machine, first, last) = format::parse_segment_name(&object.key).context("invalid record segment name")?;
        let body = backend.get(&object.key)?.context("record segment disappeared")?;
        let body = String::from_utf8(keys::decrypt(identity, &body)?)?;
        let mut expected = first;
        for line in body.lines() {
            let signed = oplog::parse_line(line, None)?;
            if signed.record.machine != machine || signed.record.seq != expected || expected > last {
                bail!("segment {} has an invalid sequence", object.key);
            }
            if signed.record.project != project {
                bail!("segment {} belongs to another project", object.key);
            }
            crate::model::validate(&signed.record)?;
            let by_seq = machines.entry(machine.clone()).or_default();
            if let Some(previous) = by_seq.get(&expected) {
                if previous != line {
                    bail!("conflicting history of machine {machine}, record {expected}");
                }
                overlaps += 1;
            } else {
                by_seq.insert(expected, line.to_string());
            }
            expected = expected.checked_add(1).context("record sequence overflow")?;
        }
        if expected.checked_sub(1) != Some(last) {
            bail!("segment {} is incomplete", object.key);
        }
    }
    // A fetch can continue from the reader's verified prefix when old remote segments are absent.
    for line in local {
        let record = oplog::parse_line(line, None)?.record;
        if record.project != project {
            bail!("local records belong to another project");
        }
        let by_seq = machines.entry(record.machine.clone()).or_default();
        if let Some(existing) = by_seq.get(&record.seq).filter(|existing| *existing != line) {
            if record.seq == 1 {
                let remote = oplog::parse_line(existing, None)?.record;
                if oplog::machine_key(&remote)? != oplog::machine_key(&record)? {
                    bail!("remote machine {}: first-record key does not match the known public key", record.machine);
                }
            }
            bail!("conflicting history of machine {}, record {}", record.machine, record.seq);
        }
        by_seq.entry(record.seq).or_insert_with(|| line.clone());
    }
    let mut keys = BTreeMap::new();
    let mut lines = Vec::new();
    let mut gaps = Vec::new();
    for (machine, by_seq) in machines {
        let Some(first) = by_seq.get(&1) else {
            gaps.push(format!("machine {machine}: missing record 1"));
            continue;
        };
        let first = oplog::parse_line(first, None)?;
        if first.record.op != "machine" {
            bail!("machine {machine}: first record is not its key");
        }
        let key = oplog::machine_key(&first.record)?;
        // Verify the complete segment set, including records beyond a gap.
        for line in by_seq.values() {
            oplog::parse_line(line, Some(&key))?;
        }
        let mut seq = 1;
        while let Some(line) = by_seq.get(&seq) {
            lines.push(line.clone());
            seq += 1;
        }
        if by_seq.keys().next_back().is_some_and(|last| *last >= seq) {
            gaps.push(format!("machine {machine}: missing record {seq}"));
        }
        keys.insert(machine, key);
    }
    lines.sort_by_key(|l| {
        let r = oplog::parse_line(l, None).expect("record checked").record;
        (r.time, r.machine, r.seq)
    });
    Ok(Records { lines, keys, overlaps, gaps })
}

pub(super) struct Reader<'a> {
    backend: &'a Backend,
    identity: &'a str,
    keys: &'a BTreeMap<String, VerifyingKey>,
    trees: BTreeMap<(String, String), (Header, Vec<u8>, Tree)>,
    parts: BTreeMap<String, Vec<u8>>,
    part_bytes: usize,
}

impl<'a> Reader<'a> {
    pub fn new(backend: &'a Backend, identity: &'a str, keys: &'a BTreeMap<String, VerifyingKey>) -> Self {
        Self { backend, identity, keys, trees: BTreeMap::new(), parts: BTreeMap::new(), part_bytes: 0 }
    }
    fn decrypted(&self, key: &str) -> Result<Vec<u8>> {
        let body = self.backend.get(key)?.with_context(|| format!("missing remote object {key}"))?;
        keys::decrypt(self.identity, &body).with_context(|| format!("decrypt {key}"))
    }
    pub fn tree(&mut self, state: &str, attempt: &str) -> Result<(Header, Vec<u8>, Tree)> {
        let wanted = (state.to_string(), attempt.to_string());
        if let Some(tree) = self.trees.get(&wanted) {
            return Ok(tree.clone());
        }
        let mut at = wanted.clone();
        let mut pending = Vec::new();
        let mut seen = BTreeSet::new();
        let mut canonical;
        loop {
            if !seen.insert(at.clone()) {
                bail!("tree chain has a loop");
            }
            if seen.len() > format::FULL_TREE_EVERY {
                bail!("tree chain exceeds 32 states");
            }
            let object = self.decrypted(&format::tree_name(&at.0, &at.1))?;
            let (header, frame) = format::split_tree(&object)?;
            if header.v != 2 {
                bail!("unsupported tree format {}", header.v);
            }
            if (header.state.as_str(), header.attempt.as_str()) != (at.0.as_str(), at.1.as_str()) {
                bail!("tree identity differs from its object name");
            }
            let full = header.tree == "full";
            let base = header.base.clone();
            pending.push((header, frame.to_vec()));
            if full {
                canonical = Vec::new();
                break;
            }
            at = base.context("delta tree has no base")?;
        }
        while let Some((header, frame)) = pending.pop() {
            canonical = zst::decompress(&frame, if header.tree == "full" { None } else { Some(&canonical) })?;
            let key = self.keys.get(&header.machine).context("tree signer has no verified machine record")?;
            header.verify(&canonical, key)?;
            let tree = Tree::parse(&canonical)?;
            self.trees.insert((header.state.clone(), header.attempt.clone()), (header, canonical.clone(), tree));
        }
        self.trees.get(&wanted).cloned().context("tree was not decoded")
    }
    pub fn read(&mut self, entry: &Entry) -> Result<Vec<u8>> {
        let mut data = Vec::new();
        for (index, link) in entry.3.iter().enumerate().rev() {
            let key = link.object();
            if !self.parts.contains_key(&key) {
                let bytes = self.decrypted(&key)?;
                if self.part_bytes + bytes.len() > 64 << 20 {
                    self.parts.clear();
                    self.part_bytes = 0;
                }
                self.part_bytes += bytes.len();
                self.parts.insert(key.clone(), bytes);
            }
            let part = self.parts.get(&key).expect("part loaded");
            let start = usize::try_from(link.3)?;
            let end = usize::try_from(link.3.checked_add(link.4).context("frame range overflow")?)?;
            let frame = part.get(start..end).context("frame is outside its part")?;
            let limit = if index == 0 { usize::try_from(entry.1)? } else { zst::LIMIT };
            data = zst::decompress_with_limit(frame, if link.5 == "delta" { Some(&data) } else { None }, limit)?;
        }
        if data.len() as u64 != entry.1 || hex::encode(Sha256::digest(&data)) != entry.2 {
            bail!("file size or SHA-256 does not match its tree");
        }
        Ok(data)
    }
    pub fn restore(&mut self, tree: &Tree, root: &std::path::Path) -> Result<usize> {
        crate::fsutil::refuse_case_collisions(root, tree.files.keys().map(|k| k.as_str()))?;
        let target = crate::tree::Tree::open(root)?;
        for (path, entry) in &tree.files {
            let data = self.read(entry)?;
            target.mkdirs_for(path, None)?;
            if entry.0 == "120000" {
                use std::os::unix::ffi::OsStrExt;
                std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&data), crate::fsutil::safe_join(root, path)?)?;
            } else {
                use std::io::Write;
                let mut file = target.create(path, if entry.0 == "100755" { 0o755 } else { 0o644 })?;
                file.write_all(&data)?;
            }
        }
        Ok(tree.files.len())
    }
}

pub(super) fn attempts(backend: &Backend) -> Result<BTreeMap<String, String>> {
    let mut states = BTreeMap::new();
    for object in backend.list("states/")? {
        if let Some((state, attempt)) = format::parse_tree_name(&object.key) {
            states.entry(state).or_insert(attempt);
        }
    }
    Ok(states)
}

pub(super) fn deletion_dependencies(lines: &[String]) -> Result<BTreeSet<String>> {
    let mut ids = BTreeSet::new();
    for line in lines {
        let record = oplog::parse_line(line, None)?.record;
        if record.op != "line.delete" {
            continue;
        }
        if let Some(state) = record.data["final_state"].as_str() {
            ids.insert(state.to_string());
        }
        for change in record.lines.values() {
            if let Some(state) = &change.working_before {
                ids.insert(state.clone());
            }
        }
    }
    Ok(ids)
}

pub(super) fn omitted_cache_refs(repo: &Repo, opts: &Options) -> Result<Vec<String>> {
    let mut omitted = Vec::new();
    if !opts.caches {
        for reference in repo.p.db.refs("", crate::ids::now_ms())? {
            if repo.state(&reference.target)?.kind == crate::model::StateKind::Cache {
                omitted.push(format!("cache state {} of ref {}: use --caches", reference.target, reference.name));
            }
        }
    }
    Ok(omitted)
}

pub(super) fn state_ancestry(repo: &Repo, id: &str) -> Result<Vec<String>> {
    let mut queue = std::collections::VecDeque::from([id.to_string()]);
    let mut seen = BTreeSet::new();
    let mut order = Vec::new();
    while let Some(id) = queue.pop_front() {
        if !seen.insert(id.clone()) {
            continue;
        }
        if let Some(state) = repo.p.db.state(&id)? {
            queue.extend(state.parents);
            queue.extend(state.derived_from);
        }
        order.push(id);
    }
    Ok(order)
}

pub(super) fn required_states(repo: &Repo, lines: &[String], opts: &Options) -> Result<BTreeSet<String>> {
    let mut required = deletion_dependencies(lines)?;
    for line in repo.p.db.lines()? {
        required.extend(state_ancestry(repo, &line.head)?);
    }
    for reference in repo.p.db.refs("", crate::ids::now_ms())? {
        let state = repo.state(&reference.target)?;
        if state.kind == crate::model::StateKind::Cache && !opts.caches {
            continue;
        }
        required.extend(state_ancestry(repo, &reference.target)?);
    }
    Ok(required)
}

pub(super) fn wanted(state: &crate::model::StateRec, opts: &Options) -> bool {
    let kind = serde_json::to_value(state.kind).unwrap_or_default();
    match kind.as_str() {
        Some("auto") => opts.auto,
        Some("cache") => opts.caches,
        _ => true,
    }
}

pub(super) fn fetch(store: &mut Store, ctx: &Ctx, repo: &mut Repo, opts: &Options) -> Result<Report> {
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, opts.paper_key.as_deref())?;
    let local: Vec<String> = repo.p.export_jsonl()?.lines().map(str::to_string).collect();
    let records = records_with_local(&store.backend, &identity, &descriptor.project, &local)?;
    let bootstrap = repo.p.id.is_empty();
    if !bootstrap {
        for (machine, key) in &records.keys {
            if repo.p.db.trusted(machine)?.as_deref().is_some_and(|trusted| trusted != hex::encode(key.to_bytes())) {
                bail!("remote machine {machine}: first-record key does not match the trusted public key");
            }
        }
    }
    let count;
    {
        let _lock = repo.p.lock()?;
        count =
            repo.p.import_lines(&records.lines, if bootstrap { crate::store::Trust::All } else { crate::store::Trust::Known })?;
    }
    if bootstrap {
        repo.p.reload_id()?;
    }
    store.project = repo.p.id.clone();
    if repo.p.id != descriptor.project {
        bail!("the records do not belong to store.json's project");
    }
    let mut report = Report {
        records: count,
        overlaps: records.overlaps,
        gaps: records.gaps.clone(),
        omitted: omitted_cache_refs(repo, opts)?,
        ..Default::default()
    };
    let required = required_states(repo, &records.lines, opts)?;
    let states = attempts(&store.backend)?;
    let mut reader = Reader::new(&store.backend, &identity, &records.keys);
    for (id, attempt) in states {
        let Some(state) = repo.p.db.state(&id)? else {
            bail!("remote tree {id} has no state record");
        };
        if !wanted(&state, opts) && !required.contains(&id) {
            continue;
        }
        if repo.p.has_state(&id) {
            repo.p.db.set_present(&id, true)?;
            continue;
        }
        let (header, _, tree) = reader.tree(&id, &attempt)?;
        let stage = repo.p.work_dir().join(crate::ids::new_id());
        if header.btrfs.as_ref().is_some_and(|identity| identity.uuid != state.subvol) {
            bail!("tree btrfs identity differs from the signed state");
        }
        crate::storage::create_subvolume(&stage)?;
        let result = (|| -> Result<usize> {
            let n = reader.restore(&tree, &stage)?;
            let _lock = repo.p.lock()?;
            let destination = repo.p.state_path(&id);
            if destination.exists() {
                repo.p.db.set_present(&id, true)?;
                return Ok(0);
            }
            // btrfs cannot rename a read-only subvolume. Seal it under the project lock.
            crate::fsutil::rename_tree(&stage, &destination)?;
            let publish = (|| -> Result<()> {
                #[cfg(all(target_os = "linux", not(feature = "overlayfs")))]
                {
                    use std::os::fd::AsFd;
                    let (uuid, transid) =
                        header.btrfs.as_ref().map(|i| (i.uuid.as_str(), i.transid)).unwrap_or((&state.subvol, 0));
                    let file = std::fs::File::open(&destination)?;
                    crate::sys::subvol_set_received(file.as_fd(), *uuid::Uuid::parse_str(uuid)?.as_bytes(), transid)?;
                }
                crate::storage::set_readonly(&destination)?;
                repo.p.db.set_present(&id, true)?;
                Ok(())
            })();
            if publish.is_err() {
                let _ = crate::storage::delete_tree(&destination);
            }
            publish?;
            Ok(n)
        })();
        if stage.exists() {
            let _ = crate::storage::delete_tree(&stage);
        }
        report.files += result?;
        report.states += 1;
        report.state_ids.push(id.clone());
    }
    // Trust the chosen store's machines only during the initial clone, as peer bootstrap does.
    if bootstrap {
        for (machine, key) in &records.keys {
            if repo.p.db.trusted(machine)?.is_none() {
                let mut record = Record::new("machine.trust");
                record.data = serde_json::json!({"machine":machine,"public_key":hex::encode(key.to_bytes())});
                repo.commit_record(ctx, record)?;
            }
        }
        if store.layer.is_none() {
            let slot = keys::machine_slot(&identity, &store.key.verifying_key())?;
            // A read-only credential can still clone. Adding its slot is optional.
            let _ = store.backend.put_new(&format!("slots/machine-{}.age", store.machine), slot);
        }
    }
    Ok(report)
}

pub(super) fn verify(store: &Store, files: bool) -> Result<Report> {
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    let records = records(&store.backend, &identity, &descriptor.project)?;
    let mut report =
        Report { records: records.lines.len(), overlaps: records.overlaps, gaps: records.gaps.clone(), ..Default::default() };
    let mut reader = Reader::new(&store.backend, &identity, &records.keys);
    let known: BTreeMap<String, String> = records
        .lines
        .iter()
        .map(|line| oplog::parse_line(line, None).expect("checked").record)
        .flat_map(|r| r.states.into_iter().map(|s| (s.id, s.subvol)))
        .collect();
    for object in store.backend.list("states/")? {
        if let Some((state, attempt)) = format::parse_tree_name(&object.key) {
            if !known.contains_key(&state) {
                bail!("remote tree {state} has no state record");
            }
            let (header, _, tree) = reader.tree(&state, &attempt)?;
            if header.btrfs.as_ref().is_some_and(|identity| Some(&identity.uuid) != known.get(&state)) {
                bail!("tree btrfs identity differs from the signed state");
            }
            for part in 0..header.parts {
                if store.backend.get(&format::data_name(&state, &attempt, part))?.is_none() {
                    bail!("tree has a missing data part");
                }
            }
            report.states += 1;
            if files {
                for entry in tree.files.values() {
                    reader.read(entry)?;
                    report.files += 1;
                }
            }
        }
    }
    Ok(report)
}

pub(super) fn log(store: &Store) -> Result<Vec<String>> {
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    Ok(records(&store.backend, &identity, &descriptor.project)?.lines)
}

pub(super) fn gc(store: &Store, dry_run: bool) -> Result<Vec<String>> {
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    let records = records(&store.backend, &identity, &descriptor.project)?;
    if !records.gaps.is_empty() {
        bail!("remote record gaps prevent collection");
    }
    let objects = store.backend.list("states/")?;
    let mut referenced = BTreeSet::new();
    let mut reader = Reader::new(&store.backend, &identity, &records.keys);
    // A complete signed tree protects every frame it references, including cross-line reuse.
    for object in &objects {
        if let Some((state, attempt)) = format::parse_tree_name(&object.key) {
            let (_, _, tree) = reader.tree(&state, &attempt)?;
            for entry in tree.files.values() {
                for link in &entry.3 {
                    referenced.insert(link.object());
                }
            }
        }
    }
    let cutoff = chrono::Utc::now().timestamp() - 86400;
    let mut deleted = Vec::new();
    for object in objects {
        let Some((state, attempt, _)) = format::parse_data_name(&object.key) else {
            continue;
        };
        if object.modified >= cutoff || referenced.contains(&object.key) {
            continue;
        }
        // Attempts cannot publish after 24 hours, so an old orphan cannot become complete now.
        if store.backend.get(&format::tree_name(&state, &attempt))?.is_some() {
            continue;
        }
        if !dry_run {
            store.backend.delete(&object.key)?;
        }
        deleted.push(object.key);
    }
    Ok(deleted)
}

#[cfg(test)]
mod tests {
    use super::*;
    const MACHINE: &str = "01a12290-0000-7000-8000-000000000001";
    const PROJECT: &str = "01a12290-0000-7000-8000-000000000002";
    const STATE: &str = "01a12290-0000-7000-8000-000000000003";

    fn fixture() -> (Backend, String, ed25519_dalek::SigningKey, std::path::PathBuf) {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        (Backend::file(root.clone()), keys::generate_identity(), ed25519_dalek::SigningKey::from_bytes(&[9; 32]), root)
    }
    fn line(seq: u64, key: &ed25519_dalek::SigningKey) -> String {
        let mut record = Record::new(if seq == 1 { "machine" } else { "config" });
        record.id = format!("01a12290-0000-7000-8000-{seq:012x}");
        record.machine = MACHINE.into();
        record.project = PROJECT.into();
        record.seq = seq;
        if seq == 1 {
            record.data = serde_json::json!({"name":"third","public_key":hex::encode(key.verifying_key().to_bytes())});
        }
        oplog::sign_line(&record, key).unwrap()
    }
    fn segment(backend: &Backend, identity: &str, first: u64, last: u64, lines: &[String]) {
        let mut body = lines.join("\n");
        body.push('\n');
        assert!(backend
            .put_new(
                &format::segment_name(MACHINE, first, last),
                keys::encrypt(&keys::recipient_of(identity).unwrap(), body.as_bytes()).unwrap()
            )
            .unwrap());
    }
    #[test]
    fn writers_can_forward_overlapping_segments_of_a_third_machine() {
        let (backend, identity, key, root) = fixture();
        let lines: Vec<String> = (1..=12).map(|seq| line(seq, &key)).collect();
        segment(&backend, &identity, 1, 10, &lines[..10]);
        segment(&backend, &identity, 1, 12, &lines);
        let merged = records(&backend, &identity, PROJECT).unwrap();
        assert_eq!(merged.lines.len(), 12);
        assert_eq!(merged.overlaps, 10);
        assert!(merged.gaps.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn conflicting_history_rejects_the_complete_machine_set() {
        let (backend, identity, key, root) = fixture();
        let lines: Vec<String> = (1..=3).map(|seq| line(seq, &key)).collect();
        segment(&backend, &identity, 1, 2, &lines[..2]);
        let mut fork = lines;
        let mut record = oplog::parse_line(&fork[1], None).unwrap().record;
        record.actor.name = "a different signed record".into();
        fork[1] = oplog::sign_line(&record, &key).unwrap();
        segment(&backend, &identity, 1, 3, &fork);
        assert!(records(&backend, &identity, PROJECT).unwrap_err().to_string().contains("conflicting history"));
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_gap_does_not_hide_a_contiguous_prefix_and_an_overlap_can_fill_it() {
        let (backend, identity, key, root) = fixture();
        let lines: Vec<String> = (1..=5).map(|seq| line(seq, &key)).collect();
        segment(&backend, &identity, 1, 2, &lines[..2]);
        segment(&backend, &identity, 4, 5, &lines[3..]);
        let prefix = records(&backend, &identity, PROJECT).unwrap();
        assert_eq!(prefix.lines.len(), 2);
        assert_eq!(prefix.gaps.len(), 1);
        segment(&backend, &identity, 2, 4, &lines[1..4]);
        let complete = records(&backend, &identity, PROJECT).unwrap();
        assert_eq!(complete.lines.len(), 5);
        assert!(complete.gaps.is_empty());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn a_changed_machine_key_is_reported_before_local_history_merge() {
        let (backend, identity, key, root) = fixture();
        let local = vec![line(1, &key)];
        let other = ed25519_dalek::SigningKey::from_bytes(&[8; 32]);
        segment(&backend, &identity, 1, 1, &[line(1, &other)]);
        let error = records_with_local(&backend, &identity, PROJECT, &local).unwrap_err().to_string();
        assert!(error.contains("first-record key does not match"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn signed_tree_and_file_hash_detect_mutation() {
        let (backend, identity, key, root) = fixture();
        let recipient = keys::recipient_of(&identity).unwrap();
        let content = b"remote state bytes";
        let frame = zst::compress(content, None).unwrap();
        let entry = Entry(
            "100644".into(),
            content.len() as u64,
            hex::encode(Sha256::digest(content)),
            vec![format::Link(STATE.into(), "0123abcd".into(), 0, 0, frame.len() as u64, "full".into())],
        );
        let tree = Tree { files: BTreeMap::from([("a.txt".into(), entry.clone())]) };
        let canonical = tree.canonical().unwrap();
        let mut header = Header {
            v: 2,
            state: STATE.into(),
            attempt: "0123abcd".into(),
            tree: "full".into(),
            base: None,
            parts: 1,
            machine: MACHINE.into(),
            btrfs: None,
            sig: String::new(),
        };
        header.sign(&canonical, &key).unwrap();
        let mut object = super::super::canonical::canonical(&serde_json::to_value(&header).unwrap()).unwrap();
        object.push(b'\n');
        object.extend(zst::compress(&canonical, None).unwrap());
        backend.put_new(&format::tree_name(STATE, "0123abcd"), keys::encrypt(&recipient, &object).unwrap()).unwrap();
        backend.put_new(&format::data_name(STATE, "0123abcd", 0), keys::encrypt(&recipient, &frame).unwrap()).unwrap();
        let keys = BTreeMap::from([(MACHINE.into(), key.verifying_key())]);
        let mut reader = Reader::new(&backend, &identity, &keys);
        assert_eq!(reader.tree(STATE, "0123abcd").unwrap().2, tree);
        assert_eq!(reader.read(&entry).unwrap(), content);
        let mut wrong = entry;
        wrong.2 = "00".repeat(32);
        assert!(reader.read(&wrong).is_err());
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn fetch_continues_from_an_existing_verified_prefix() {
        let (backend, identity, key, root) = fixture();
        let lines: Vec<String> = (1..=5).map(|seq| line(seq, &key)).collect();
        segment(&backend, &identity, 3, 5, &lines[2..]);
        let fetched = records_with_local(&backend, &identity, PROJECT, &lines[..2]).unwrap();
        assert_eq!(fetched.lines.len(), 5);
        assert!(fetched.gaps.is_empty());
        assert_eq!(fetched.overlaps, 0);
        assert_eq!(records(&backend, &identity, PROJECT).unwrap().gaps.len(), 1);
        std::fs::remove_dir_all(root).unwrap();
    }
}
