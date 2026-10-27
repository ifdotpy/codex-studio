//! Project vault epochs. The root descriptor remains the trust anchor.
use super::{
    backend::{Backend, EpochBackend},
    keys, reader, writer, Descriptor, Options, Store,
};
use crate::{ctx::Ctx, model::Record, oplog, repo::Repo};
use anyhow::{bail, Context, Result};
use layr_store::epochs::{Key, Mode};
use std::collections::BTreeMap;

#[derive(serde::Serialize)]
pub struct ModeChange {
    pub mode: Mode,
    pub tightened: Vec<String>,
    pub rotated: bool,
    pub opened: bool,
}

pub(super) struct Current {
    pub key: Key,
    pub identity: String,
}
#[derive(Clone)]
struct Published {
    key: Key,
    record: Record,
    line: String,
}
fn publications(root: &Backend, descriptor: &Descriptor) -> Result<Vec<Published>> {
    let mut pending = Vec::new();
    for object in root.list("keys/")? {
        let bytes = root.get(&object.key)?.context("project epoch disappeared")?;
        let line = String::from_utf8(bytes)?;
        let record = oplog::parse_line(&line, None)?.record;
        if record.op != "project.key" || record.project != descriptor.project {
            bail!("invalid project epoch record");
        }
        let key: Key = serde_json::from_value(record.data.clone())?;
        key.validate()?;
        if key.project != descriptor.project || key.object(&record.id) != object.key {
            bail!("project epoch object binding differs");
        }
        pending.push(Published { key, record, line });
    }
    let mut chain = Vec::new();
    let mut parent = "root".to_string();
    let mut recipient = descriptor.recipient.clone();
    loop {
        let children: Vec<_> = pending.iter().enumerate().filter(|(_, p)| p.key.parent == parent).map(|(i, _)| i).collect();
        if children.len() > 1 {
            bail!("project epochs forked; refusing ambiguous access");
        }
        let Some(&index) = children.first() else { break };
        let next = pending.remove(index);
        if next.key.parent_recipient != recipient {
            bail!("project epoch lineage changed recipient");
        }
        parent = next.key.epoch.clone();
        recipient = next.key.recipient.clone();
        chain.push(next);
    }
    if !pending.is_empty() {
        bail!("project epoch has no authenticated parent");
    }
    Ok(chain)
}

/// Resolve the newest epoch before reading any current data. A removed reader must not
/// silently fall back to an older opener. `open` uses existing machine/person/paper slots.
pub(super) fn resolve(
    root: &Backend,
    descriptor: &Descriptor,
    open: impl FnMut(&Backend, &Descriptor) -> Result<(String, bool)>,
) -> Result<Option<(Backend, Current, bool)>> {
    let chain = publications(root, descriptor)?;
    resolve_chain(root, descriptor, chain, open)
}
fn resolve_chain(
    root: &Backend,
    descriptor: &Descriptor,
    chain: Vec<Published>,
    mut open: impl FnMut(&Backend, &Descriptor) -> Result<(String, bool)>,
) -> Result<Option<(Backend, Current, bool)>> {
    root.set_epoch(chain.last().map(|p| p.key.epoch.as_str()))?;
    let Some(latest) = chain.last() else { return Ok(None) };
    let scoped = root.scoped(&latest.key.prefix())?;
    let next: Descriptor = serde_json::from_slice(&scoped.get("store.json")?.context("project epoch has no descriptor")?)?;
    if next.format != 2 || next.project != descriptor.project || next.recipient != latest.key.recipient {
        bail!("project epoch descriptor differs from its signed key");
    }
    let (identity, public) =
        open(&scoped, &next).context("no slot for the current project epoch; access was removed or rotated")?;
    if keys::recipient_of(&identity)? != next.recipient {
        bail!("epoch slot recipient differs");
    }
    scoped.unlock_reader(&identity)?;
    let mut history = Vec::new();
    let mut recipients = BTreeMap::from([("root".to_string(), descriptor.recipient.clone())]);
    for p in &chain[..chain.len() - 1] {
        recipients.insert(p.key.epoch.clone(), p.key.recipient.clone());
    }
    for (epoch, recipient) in &recipients {
        let sealed = scoped.get(&format!("history/{epoch}.age"))?.context("epoch has no sealed history identity")?;
        let old = keys::open_slot(&sealed, &identity)?;
        if keys::recipient_of(&old)? != *recipient {
            bail!("sealed history identity differs from the signed lineage");
        }
        let backend = if epoch == "root" { root.clone() } else { root.scoped(&format!("project/epochs/{epoch}"))? };
        history.push((backend, old));
    }
    let (_, root_identity) = history
        .iter()
        .find(|(_, id)| keys::recipient_of(id).ok().as_ref() == Some(&descriptor.recipient))
        .context("root identity missing")?;
    root.unlock_reader(&identity)?;
    scoped.unlock_reader(&identity)?;
    for (backend, _) in &history {
        backend.unlock_reader(&identity)?;
    }
    let original = reader::records(root, root_identity, &descriptor.project)?;
    let root_writers = layr_store::public::trusted_writers(&descriptor.project, &original.lines)?;
    root.unlock_reader(&identity)?;
    let current = reader::records(&scoped, &identity, &descriptor.project)?;
    if current.lines.is_empty() || !current.gaps.is_empty() {
        bail!("rotation in progress: current epoch record history is incomplete; retry after the writer resumes");
    }
    // The exact root log must survive, preventing a forged creator/trust graph in a new epoch.
    if original.lines.iter().any(|line| !current.lines.contains(line)) {
        bail!("rotation in progress: current epoch does not yet preserve the complete root history");
    }
    let trusted = layr_store::public::trusted_writers(&descriptor.project, &current.lines)?;
    if root_writers.iter().any(|(id, key)| trusted.get(id) != Some(key)) {
        bail!("project epoch changes root writer identity");
    }
    for p in &chain {
        let signer = trusted.get(&p.record.machine).context("project epoch signer is not a trusted writer")?;
        oplog::parse_line(&p.line, Some(signer))?;
        if !current.lines.contains(&p.line) {
            bail!("rotation in progress: epoch discovery is not yet in its encrypted history");
        }
    }
    let readers = current
        .lines
        .iter()
        .rev()
        .filter_map(|line| oplog::parse_line(line, None).ok())
        .find(|r| r.record.op == "project.readers" && r.record.data["epoch"] == latest.key.epoch)
        .context("project epoch has no encrypted reader set")?;
    let readers: BTreeMap<String, String> = serde_json::from_value(readers.record.data["readers"].clone())?;
    if layr_store::epochs::readers_hash(&readers)? != latest.key.readers_hash {
        bail!("project epoch reader hash differs");
    }
    let backend = Backend::Epoch(Box::new(EpochBackend { root: root.clone(), current: scoped, history }));
    Ok(Some((backend, Current { key: latest.key.clone(), identity }, public)))
}

fn opener(store: &Store, backend: &Backend, descriptor: &Descriptor) -> Result<(String, bool)> {
    let own = || -> Result<String> {
        if let Some(slot) = backend.get(&format!("slots/machine-{}.age", store.machine))? {
            return keys::open_machine_slot(&slot, &store.key);
        }
        if let Some(person) = &store.person {
            person.check(&store.root_descriptor()?)?;
            let slot = backend
                .get(&format!("slots/{}", layr_store::person::slot_name(&person.login, &keys::recipient_of(&person.identity)?)?))?
                .context("no person slot")?;
            return keys::open_slot(&slot, &person.identity);
        }
        if let Some(paper) = &store.paper_key_text {
            let opener = keys::identity_from_paper(paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?;
            return keys::open_slot(&backend.get("slots/paper.age")?.context("no paper slot")?, &opener);
        }
        bail!("no reader slot")
    };
    if !store.force_public {
        if let Ok(identity) = own() {
            return Ok((identity, false));
        }
    }
    Ok((super::public::identity(backend, descriptor)?.context("the store is not public")?, true))
}
pub(super) fn connect(store: &mut Store) -> Result<()> { connect_before_pending(store, None) }
fn connect_before_pending(store: &mut Store, pending: Option<&Published>) -> Result<()> {
    if store.layer.is_some() {
        return Ok(());
    }
    if store.backend.get("store.json")?.is_none() {return Ok(());}
    let root=store.root_descriptor()?;
    if let Some(pin) = &store.pin {
        pin.check(&root)?;
    }
    if let Some(pin) = store.person.as_ref().and_then(|person|person.index.pin(&root.project)) {
        pin.check_store(&root)?;
    }
    let mut chain = publications(&store.backend, &root)?;
    if let Some(pending) = pending {
        if chain.iter().any(|p| p.key.epoch == pending.key.epoch) {
            let latest = chain.last().context("pending epoch disappeared")?;
            if latest.line.trim_end() != pending.line.trim_end() { bail!("pending rotation discovery differs from this machine's signed record"); }
            chain.pop();
        }
        let parent = chain.last().map_or("root", |p| p.key.epoch.as_str());
        let recipient = chain.last().map_or(root.recipient.as_str(), |p| p.key.recipient.as_str());
        if parent != pending.key.parent || recipient != pending.key.parent_recipient {
            bail!("project epoch changed while a rotation was pending");
        }
    }
    if let Some((backend, current, public)) = resolve_chain(&store.backend, &root, chain, |b, d| opener(store, b, d))? {
        store.backend = backend;
        store.epoch = Some(current);
        store.epoch_public = public;
        store.public_reader.store(public, std::sync::atomic::Ordering::SeqCst);
    }
    Ok(())
}

fn pending_rotation(store: &Store, repo: &Repo) -> Result<Option<Published>> {
    let mut found = None;
    for line in repo.p.export_jsonl()?.lines() {
        let record = oplog::parse_line(line, None)?.record;
        if record.op != "project.key" || record.machine != store.machine { continue; }
        let key: Key = serde_json::from_value(record.data.clone())?;
        key.validate()?;
        let path = store.dir.join("project-keys").join(format!("{}.age", key.epoch));
        if !path.exists() { continue; }
        oplog::parse_line(line, Some(&store.key.verifying_key()))?;
        let identity = keys::open_machine_slot(&std::fs::read(&path)?, &store.key)?;
        if key.project != repo.p.id || keys::recipient_of(&identity)? != key.recipient {
            bail!("pending rotation identity differs from its signed record");
        }
        if found.replace(Published { key, record, line:line.into() }).is_some() {
            bail!("more than one local rotation is pending");
        }
    }
    Ok(found)
}
fn pending_readers(repo: &Repo, key: &Key) -> Result<BTreeMap<String,String>> {
    let mut readers = None;
    for line in repo.p.export_jsonl()?.lines() {
        let r = oplog::parse_line(line, None)?.record;
        if r.op == "project.readers" && r.data["epoch"] == key.epoch {
            readers = Some(serde_json::from_value(r.data["readers"].clone())?);
        }
    }
    let readers = readers.context("pending rotation has no reader set")?;
    if layr_store::epochs::readers_hash(&readers)? != key.readers_hash { bail!("pending rotation reader hash differs"); }
    Ok(readers)
}

pub(crate) fn project_mode(repo: &Repo) -> Result<Mode> {
    let mut mode = Mode::Private;
    for line in repo.p.export_jsonl()?.lines() {
        let r = oplog::parse_line(line, None)?.record;
        if r.op == "project.mode" {
            mode = serde_json::from_value(r.data["mode"].clone())?;
        }
    }
    Ok(mode)
}
pub(crate) fn overlay_mode(repo: &Repo) -> Result<Mode> {
    Ok(repo.p.db.overlay_config("overlay.access-mode")?.map(serde_json::from_value).transpose()?.unwrap_or_default())
}
fn readers(store: &Store, repo: &Repo, remove: &[String]) -> Result<BTreeMap<String, String>> {
    let mut map = BTreeMap::new();
    let anchor = store.root_descriptor()?.recipient;
    for line in repo.p.export_jsonl()?.lines() {
        let r = oplog::parse_line(line, None)?.record;
        if r.op == "project.reader" && r.data["store"] == anchor {
            map.insert(
                r.data["principal"].as_str().context("reader principal missing")?.to_string(),
                r.data["recipient"].as_str().context("reader recipient missing")?.to_string(),
            );
        }
        if r.op == "project.readers" && r.data["store"] == anchor {
            map = serde_json::from_value(r.data["readers"].clone())?;
        }
    }
    let known = super::overlays::trusted_keys(repo)?;
    let descriptor = store.descriptor()?;
    let identity = store.identity(&descriptor, None)?;
    let remote = reader::records(&store.backend, &identity, &descriptor.project)?;
    for slot in store.backend.list("slots/")? {
        let name = slot.key.strip_prefix("slots/").context("invalid slot")?;
        if name.ends_with(".key.age") {
            continue;
        }
        if let Some(id) = name.strip_prefix("machine-").and_then(|s| s.strip_suffix(".age")) {
            if !super::format::is_uuid(id) {
                bail!("invalid machine slot name");
            }
            if remove.iter().any(|p| p == &format!("machine:{id}") || p == id) {
                map.insert(format!("machine:{id}"), String::new());
                continue;
            }
            let mut public = repo
                .p
                .db
                .machine_key(id)?
                .or_else(|| map.get(&format!("machine:{id}")).cloned())
                .or_else(|| known.get(id).or(remote.keys.get(id)).map(|k| hex::encode(k.to_bytes())));
            if public.is_none() {
                if let Some(bytes) = store.backend.get(&format!("slots/machine-{id}.key.age"))? {
                    let line = String::from_utf8(keys::decrypt(&identity, &bytes)?)?;
                    let record = oplog::parse_line(&line, None)?.record;
                    if record.op != "machine" || record.seq != 1 || record.machine != id || record.project != descriptor.project {
                        bail!("reader key metadata has invalid machine or project binding");
                    }
                    let key = oplog::machine_key(&record)?;
                    oplog::parse_line(&line, Some(&key))?;
                    public = Some(hex::encode(key.to_bytes()));
                }
            }
            let public = public.with_context(|| format!("cannot rotate: no public key for machine {id}"))?;
            map.insert(format!("machine:{id}"), public);
        } else if let Some(login) = layr_store::person::slot_login(name) {
            if remove.iter().any(|p| p == &format!("person:{login}")) {
                map.insert(format!("person:{login}"), String::new());
                continue;
            }
            if !map.keys().any(|p| p == &format!("person:{login}")) {
                bail!("cannot rotate: re-run remote grant for person {login} to record their public recipient");
            }
        }
    }
    let paper = if let Some(text) = &store.paper_key_text {
        Some(keys::recipient_of(&keys::identity_from_paper(text).map_err(|_| anyhow::anyhow!("invalid paper key"))?)?)
    } else {
        store.pin.as_ref().and_then(|p| p.paper_recipient.clone())
    };
    if store.backend.get("slots/paper.age")?.is_some() {
        map.insert("paper".into(), paper.context("cannot rotate: set LAYR_PAPER_KEY_FILE once to retain the paper reader")?);
    }
    Ok(map)
}
fn write_slots(backend: &Backend, identity: &str, readers: &BTreeMap<String, String>) -> Result<()> {
    for (principal, recipient) in readers {
        let (path, slot) = if let Some(id) = principal.strip_prefix("machine:") {
            let key = layr_store::person::verifying_key(recipient)?;
            (format!("slots/machine-{id}.age"), keys::machine_slot(identity, &key)?)
        } else if let Some(login) = principal.strip_prefix("person:") {
            (format!("slots/{}", layr_store::person::slot_name(login, recipient)?), keys::seal_slot(identity, recipient)?)
        } else if principal == "paper" || principal == "service" {
            (format!("slots/{principal}.age"), keys::seal_slot(identity, recipient)?)
        } else {
            continue;
        };
        backend.put_new(&path, slot)?;
    }
    Ok(())
}
impl Store {
    pub(super) fn service_recipient(&self, repo: &Repo) -> Result<Option<String>> {
        for (key, _) in repo.p.db.config_all()? {
            let Some(name) = key.strip_prefix("cloud.remote.").and_then(|k| k.strip_suffix(".url")) else { continue };
            if repo.p.config_value(&key)?.and_then(|v| v.as_str().map(str::to_owned)).as_deref() == Some(self.url.as_str()) {
                if let Some(recipient) =
                    repo.p.config_value(&format!("remote.{name}.service-recipient"))?.and_then(|v| v.as_str().map(str::to_owned))
                {
                    return Ok(Some(recipient));
                }
            }
        }
        Ok(repo.p.config_value("service.recipient")?.and_then(|v| v.as_str().map(str::to_owned)))
    }
}

impl Store {
    pub fn change_mode(
        &mut self,
        ctx: &Ctx,
        repo: &Repo,
        name: &str,
        mode: Mode,
        remove: &[String],
        reason: Option<&str>,
        force: bool,
    ) -> Result<ModeChange> {
        repo.require(ctx, "remote")?;
        self.private_dir()?;
        self.connect_transport(ctx)?;
        let pending = pending_rotation(self, repo)?;
        connect_before_pending(self, pending.as_ref())?;
        let descriptor = self.descriptor()?;
        let identity = self.identity(&descriptor, None)?;
        self.require_private_access()?;
        let remote = reader::records(&self.backend, &identity, &descriptor.project)?;
        if !remote.gaps.is_empty() {
            bail!("complete the remote record history before rotating");
        }
        let trusted = layr_store::public::trusted_writers(&descriptor.project, &remote.lines)?;
        if trusted.get(&self.machine) != Some(&self.key.verifying_key()) {
            bail!("rotation requires a trusted writer machine");
        }
        let old_mode = if self.backend.get("public.json")?.is_some() {
            Mode::Public
        } else if self.backend.get("slots/service.age")?.is_some() {
            Mode::Service
        } else {
            Mode::Private
        };
        let mode = if let Some(pending) = &pending {
            if !force && mode != pending.key.mode { bail!("resume the pending rotation before changing its mode"); }
            pending.key.mode
        } else if force { old_mode } else { mode };
        let rotated = pending.is_some() || force || !remove.is_empty() || mode < old_mode;
        let service = repo
            .p
            .config_value(&format!("remote.{name}.service-recipient"))?
            .or(repo.p.config_value("service.recipient")?)
            .and_then(|v| v.as_str().map(str::to_owned));
        if mode == Mode::Service && service.as_ref().is_none_or(|s| !keys::valid_recipient(s)) {
            bail!("service mode requires layr config remote.{name}.service-recipient <age recipient>");
        }
        // Validate the complete tightening before any write, including manager authority.
        let mut tightened = Vec::new();
        let mut remote_overlays=None;
        for overlay in repo.visible_overlay_names(ctx)? {
            let orepo = match repo.overlay_repo(&overlay) {
                Ok(repo)=>repo,
                Err(_) => {
                    if mode>=old_mode {continue;}
                    if remote_overlays.is_none() {
                        let authorities=crate::overlay_managers::from_lines(&descriptor.project,&remote.lines,&trusted)?;
                        let (published,refused)=super::overlays::published(&self.backend,&descriptor.project,&self.root_descriptor()?.recipient,&trusted,&authorities)?;
                        if !refused.is_empty() {bail!("cannot verify overlay access before closing: {}",refused.join("; "));}
                        remote_overlays=Some(published);
                    }
                    if remote_overlays.as_ref().and_then(|overlays|overlays.get(&overlay)).is_some_and(|p|p.key.access_mode>mode) {
                        bail!("cannot tighten overlay {overlay}: its key manager needs reader access on this machine");
                    }
                    continue;
                }
            };
            if overlay_mode(&orepo)? > mode {
                crate::overlay_managers::of(repo, &overlay)?.require(&self.machine)?;
                tightened.push(overlay);
            }
        }
        let _lock = repo.p.lock()?;
        if rotated {
            let mut readers = if let Some(pending) = &pending { pending_readers(repo, &pending.key)? } else { readers(self, repo, remove)? };
            for principal in remove {
                let canonical = if crate::model::is_uuid(principal) { format!("machine:{principal}") } else { principal.clone() };
                if canonical == format!("machine:{}", self.machine) {
                    bail!("the rotating machine must remain a reader");
                }
                if pending.is_some() {
                    if readers.contains_key(&canonical) { bail!("resume the pending rotation before removing another reader"); }
                } else if readers.remove(&canonical).is_none() {
                    bail!("unknown project reader {principal}");
                }
            }
            readers.remove("service");
            if mode == Mode::Service {
                readers.insert("service".into(), service.clone().context("service recipient missing")?);
            }
            let root = self.backend.root().clone();
            let parent = self.epoch.as_ref().map(|e| e.key.epoch.as_str()).unwrap_or("root").to_string();
            let hash = layr_store::epochs::readers_hash(&readers)?;
            // A pending signed record and machine-sealed local identity are the retry source.
            let (key, record, line, next_identity) = if let Some(pending) = pending {
                let Published { key, record, line } = pending;
                if key.mode != mode || key.readers_hash != hash {
                    bail!("retry the pending project rotation before changing its audience");
                }
                let path = self.dir.join("project-keys").join(format!("{}.age", key.epoch));
                let identity = keys::open_machine_slot(&std::fs::read(path)?, &self.key)?;
                if keys::recipient_of(&identity)? != key.recipient {
                    bail!("pending epoch identity differs from its signed recipient");
                }
                (key, record, line, identity)
            } else {
                let next_identity = keys::generate_identity();
                let key = Key {
                    project: descriptor.project.clone(),
                    parent: parent.clone(),
                    parent_recipient: descriptor.recipient.clone(),
                    epoch: crate::ids::new_id(),
                    recipient: keys::recipient_of(&next_identity)?,
                    mode,
                    readers_hash: hash,
                };
                let path = self.dir.join("project-keys").join(format!("{}.age", key.epoch));
                std::fs::create_dir_all(path.parent().context("key folder")?)?;
                crate::fsutil::atomic_write(&path, &keys::machine_slot(&next_identity, &self.key.verifying_key())?, 0o600)?;
                let mut audience = Record::new("project.readers");
                audience.data =
                    serde_json::json!({"epoch":key.epoch,"readers":readers,"store":self.root_descriptor()?.recipient});
                repo.commit_record(ctx, audience)?;
                let mut record = Record::new("project.key");
                record.data = serde_json::to_value(&key)?;
                let record = repo.p.append(record, &ctx.actor())?;
                let line = repo.p.db.record_line(&record.machine, record.seq)?.context("epoch record missing")?;
                (key, record, line, next_identity)
            };
            let current = root.scoped(&key.prefix())?;
            write_slots(&current, &next_identity, &readers)?;
            let next = Descriptor { format: 2, project: descriptor.project.clone(), recipient: key.recipient.clone() };
            current.put_new("store.json", serde_json::to_vec(&next)?)?;
            current.put_new(&format!("history/{parent}.age"), keys::seal_slot(&identity, &key.recipient)?)?;
            let mut history = vec![(
                if parent == "root" { root.clone() } else { root.scoped(&format!("project/epochs/{parent}"))? },
                identity.clone(),
            )];
            if let Backend::Epoch(old) = &self.backend {
                for (backend, old_identity) in &old.history {
                    let old_recipient = keys::recipient_of(old_identity)?;
                    let root_descriptor: Descriptor = serde_json::from_slice(&root.get("store.json")?.context("missing root")?)?;
                    let old_id = if old_recipient == root_descriptor.recipient {
                        "root".to_string()
                    } else {
                        publications(&root, &root_descriptor)?
                            .into_iter()
                            .find(|p| p.key.recipient == old_recipient)
                            .context("missing old epoch")?
                            .key
                            .epoch
                    };
                    current.put_new(&format!("history/{old_id}.age"), keys::seal_slot(old_identity, &key.recipient)?)?;
                    history.push((backend.clone(), old_identity.clone()));
                }
            }
            let recorded = repo.p.export_jsonl()?.lines().filter_map(|line|oplog::parse_line(line,None).ok())
                .any(|r|r.record.op == "project.mode" && r.record.data["epoch"] == key.epoch);
            if !recorded {
                let mut event = Record::new("project.mode");
                event.data = serde_json::json!({"mode":mode,"epoch":key.epoch,"reason":reason});
                repo.commit_record(ctx, event)?;
            }
            // Publish routing only after slots, descriptor and sealed history exist.
            // An older proof must never be used to list or read the new namespace.
            if !root.put_new(&key.object(&record.id), line.as_bytes().to_vec())? {
                let existing = root.get(&key.object(&record.id))?.context("pending discovery disappeared")?;
                if std::str::from_utf8(&existing)?.trim_end() != line.trim_end() { bail!("pending epoch publication changed"); }
            }
            self.backend = Backend::Epoch(Box::new(EpochBackend { root: root.clone(), current: current.clone(), history }));
            self.backend.set_epoch(Some(&key.epoch))?;
            self.backend.unlock_reader(&next_identity)?;
            self.backend.prove_reader()?;
            self.epoch = Some(Current { key: key.clone(), identity: next_identity });
            // A new epoch carries the complete signed log; states stay in their original epochs.
            // Carry remote-only reader announcements and signed history forward too.
            // The writer then appends the local suffix after these exact prefixes.
            let mut by_machine: BTreeMap<String, Vec<String>> = BTreeMap::new();
            for line in &remote.lines {
                let r = oplog::parse_line(line, None)?.record;
                by_machine.entry(r.machine).or_default().push(line.clone());
            }
            for (machine, mut lines) in by_machine {
                lines.sort_by_key(|l| oplog::parse_line(l, None).expect("verified record").record.seq);
                let last = oplog::parse_line(lines.last().context("empty machine history")?, None)?.record.seq;
                let bytes = format!("{}\n", lines.join("\n"));
                self.backend
                    .put_new(&super::format::segment_name(&machine, 1, last), keys::encrypt(&key.recipient, bytes.as_bytes())?)?;
            }
            self.publishing_epoch = true;
            writer::push_locked(self, ctx, repo, &Options::default())?;
            if mode == Mode::Public {
                self.publish_public(ctx, repo)?;
            }
            for name in &tightened {
                let orepo = repo.overlay_repo(name)?;
                let _overlay_lock = orepo.p.lock()?;
                let mut event = Record::new("overlay.mode");
                event.data = serde_json::json!({"mode":mode,"config":{"overlay.access-mode":mode,"overlay.revocation":crate::ids::new_id()}});
                orepo.commit_record(ctx, event)?;
            }
            if !tightened.is_empty() {
                let mut report = super::Report::default();
                super::overlays::push(self, ctx, repo, &Options::default(), &mut report)?;
                if !report.omitted.is_empty() {
                    bail!("overlay tightening incomplete: {}", report.omitted.join("; "));
                }
            }
            self.publishing_epoch = false;
            let pending_path = self.dir.join("project-keys").join(format!("{}.age", key.epoch));
            std::fs::remove_file(&pending_path)?;
        } else {
            if mode == Mode::Service {
                self.backend.put_new(
                    "slots/service.age",
                    keys::seal_slot(&identity, service.as_deref().context("service recipient missing")?)?,
                )?;
            }
            if mode == Mode::Public {
                self.publish_public(ctx, repo)?;
            }
            if mode != old_mode || project_mode(repo)? != mode {
                let mut event = Record::new("project.mode");
                event.data = serde_json::json!({"mode":mode,"reason":reason});
                repo.commit_record(ctx, event)?;
                writer::push_locked(self, ctx, repo, &Options::default())?;
            }
        }
        Ok(ModeChange { mode, tightened, rotated, opened: mode > old_mode })
    }
}

pub(super) fn check_current(store: &Store) -> Result<()> {
    if store.layer.is_some() || store.publishing_epoch {
        return Ok(());
    }
    let root = store.backend.root();
    let Some(bytes) = root.get("store.json")? else { return Ok(()) };
    let descriptor: Descriptor = serde_json::from_slice(&bytes)?;
    let chain = publications(root, &descriptor)?;
    if chain.last().map(|p| p.key.epoch.as_str()) != store.epoch.as_ref().map(|p| p.key.epoch.as_str()) {
        bail!("project epoch changed; reconnect before writing");
    }
    Ok(())
}

/// Invite pins bind the immutable root, while newly granted people open the current slot.
pub(crate) fn accept_person(backend: &Backend, descriptor: &Descriptor, person: &crate::person::Access) -> Result<()> {
    let current = resolve(backend, descriptor, |scoped, next| {
        let name = layr_store::person::slot_name(&person.login, &keys::recipient_of(&person.identity)?)?;
        let slot = scoped.get(&format!("slots/{name}"))?.context("this login has no current person slot")?;
        let identity = keys::open_slot(&slot, &person.identity)?;
        if keys::recipient_of(&identity)? != next.recipient {
            bail!("person slot recipient differs");
        }
        Ok((identity, false))
    })?;
    if current.is_none() {
        person.open(backend, descriptor)?;
    }
    Ok(())
}
