//! The store: machine identity, projects, their folders and the project database.
//!
//! ```text
//! <root>/machine/{id,name,key}        machine identity (0700)
//! <root>/projects/<name>/
//!   states/<state-id>                 read-only snapshots (0700)
//!   lines/<line-name>                 writable snapshots (0711, no listing)
//!   git/                              git object store for the remote bridge (0755)
//!   meta/db/layr.sqlite               signed operation records and derived tables (0700); meta/db
//!                                     is its own subvolume, so a snapshot elsewhere does not turn
//!                                     its fsync into a full transaction commit (100 ms against 6)
//!   meta/stage/<line-id>              staging lines (the index of `layr add`)
//!   meta/scan, meta/work              temporary snapshots
//! ```
//!
//! Write order of an operation: snapshots first, then one database transaction with the record
//! and its derived rows. After a crash only a snapshot without a record can exist; `layr fsck`
//! deletes it. Records leave the database as JSONL lines (replication, backups, export).

use crate::db::Db;
use crate::ids;
use crate::model::{Actor, Record};
use crate::oplog;
use anyhow::{anyhow, bail, Context, Result};
use ed25519_dalek::SigningKey;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub struct Machine {
    pub id: String,
    pub name: String,
    pub key: SigningKey,
}

pub struct Store {
    pub root: PathBuf,
    pub machine: Machine,
}

pub fn default_root() -> PathBuf {
    std::env::var_os("LAYR_ROOT").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("/var/lib/layr"))
}

fn mkdir_mode(p: &Path, mode: u32) -> Result<()> {
    if !p.exists() {
        fs::create_dir_all(p).with_context(|| format!("mkdir {}", p.display()))?;
    }
    fs::set_permissions(p, fs::Permissions::from_mode(mode))?;
    Ok(())
}

fn hostname() -> String {
    fs::read_to_string("/etc/hostname").map(|s| s.trim().to_string()).unwrap_or_else(|_| "machine".into())
}

impl Store {
    pub fn open(root: &Path) -> Result<Store> {
        let mdir = root.join("machine");
        if !mdir.join("key").exists() {
            if !root.exists() {
                mkdir_mode(root, 0o755)?;
            }
            mkdir_mode(&mdir, 0o700)?;
            let id = ids::new_id();
            let key = SigningKey::generate(&mut rand::rngs::OsRng);
            crate::fsutil::atomic_write(&mdir.join("id"), id.as_bytes(), 0o600)?;
            crate::fsutil::atomic_write(&mdir.join("name"), hostname().as_bytes(), 0o600)?;
            crate::fsutil::atomic_write(&mdir.join("key"), hex::encode(key.to_bytes()).as_bytes(), 0o600)?;
        }
        let id = fs::read_to_string(mdir.join("id"))?.trim().to_string();
        let name = fs::read_to_string(mdir.join("name")).unwrap_or_else(|_| hostname()).trim().to_string();
        let kb = hex::decode(fs::read_to_string(mdir.join("key"))?.trim())?;
        let arr: [u8; 32] = kb.try_into().map_err(|_| anyhow!("bad machine key"))?;
        let key = SigningKey::from_bytes(&arr);
        mkdir_mode(&root.join("projects"), 0o755)?;
        Ok(Store { root: root.to_path_buf(), machine: Machine { id, name, key } })
    }

    pub fn projects_dir(&self) -> PathBuf {
        self.root.join("projects")
    }

    pub fn project_names(&self) -> Result<Vec<String>> {
        let mut v = Vec::new();
        for e in fs::read_dir(self.projects_dir())?.flatten() {
            if db_file(&e.path()).is_file() {
                v.push(e.file_name().to_string_lossy().into_owned());
            }
        }
        v.sort();
        Ok(v)
    }
}

/// The database of a project (`meta/db/layr.sqlite`; older projects: `meta/layr.sqlite`).
pub fn db_file(project_dir: &Path) -> PathBuf {
    let old = project_dir.join("meta").join("layr.sqlite");
    if old.is_file() {
        old
    } else {
        project_dir.join("meta").join("db").join("layr.sqlite")
    }
}

pub fn check_name(kind: &str, name: &str) -> Result<()> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && name.chars().next().map(|c| c.is_ascii_alphanumeric()).unwrap_or(false)
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
        && !name.ends_with(".lock")
        && !name.contains("..");
    if !ok {
        bail!("invalid {kind} name '{name}': use letters, digits, '-', '_' and '.'; start with a letter or digit");
    }
    Ok(())
}

/// Which machines an import takes records from: this machine, machines this project trusts,
/// and machines vouched for by a `machine.trust` record of a trusted machine in the same batch.
#[derive(Clone, Copy)]
pub enum Trust<'a> {
    Known,
    /// Also this machine with this key, for this batch only (the peer of a sync; the sync records
    /// its trust after the records were taken).
    Peer(&'a str, &'a str),
    /// Every machine of the batch (a new replica or a restore).
    All,
}

pub struct Project<'a> {
    pub store: &'a Store,
    pub name: String,
    pub dir: PathBuf,
    pub id: String,
    pub db: Db,
}

pub struct Lock {
    _f: fs::File,
}

impl<'a> Project<'a> {
    pub fn states_dir(&self) -> PathBuf {
        self.dir.join("states")
    }
    pub fn lines_dir(&self) -> PathBuf {
        self.dir.join("lines")
    }
    pub fn meta_dir(&self) -> PathBuf {
        self.dir.join("meta")
    }
    pub fn git_dir(&self) -> PathBuf {
        self.dir.join("git")
    }
    /// Path of a state snapshot. Only a canonical UUID names one; anything else gives a path
    /// that never exists, so no record can make the service touch another path.
    pub fn state_path(&self, id: &str) -> PathBuf {
        if crate::model::is_uuid(id) {
            self.states_dir().join(id)
        } else {
            self.states_dir().join(".invalid-id")
        }
    }
    pub fn line_path(&self, name: &str) -> PathBuf {
        if check_name("line", name).is_ok() || (name.starts_with(".try-") && name[5..].chars().all(|c| c.is_ascii_hexdigit())) {
            self.lines_dir().join(name)
        } else {
            self.lines_dir().join(".invalid-name")
        }
    }
    /// Path of a layer snapshot `<state-id>@<path>`.
    pub fn layer_path(&self, state: &str, name: &str) -> PathBuf {
        if crate::model::is_layer_name(name, state) {
            self.states_dir().join(name)
        } else {
            self.states_dir().join(".invalid-layer")
        }
    }
    pub fn stage_path(&self, line_id: &str) -> PathBuf {
        self.meta_dir().join("stage").join(line_id)
    }
    pub fn scan_dir(&self) -> PathBuf {
        self.meta_dir().join("scan")
    }
    pub fn work_dir(&self) -> PathBuf {
        self.meta_dir().join("work")
    }

    pub fn create(store: &'a Store, name: &str, actor: &Actor, config: serde_json::Value) -> Result<Project<'a>> {
        check_name("project", name)?;
        let dir = store.projects_dir().join(name);
        if db_file(&dir).exists() {
            bail!("project '{name}' already exists");
        }
        if !crate::btrfs::is_btrfs(&store.projects_dir()) {
            bail!("{} is not on btrfs", store.projects_dir().display());
        }
        mkdir_mode(&dir, 0o755)?;
        mkdir_mode(&dir.join("states"), 0o700)?;
        mkdir_mode(&dir.join("lines"), 0o711)?;
        mkdir_mode(&dir.join("meta"), 0o700)?;
        for d in ["stage", "scan", "work"] {
            mkdir_mode(&dir.join("meta").join(d), 0o700)?;
        }
        if !dir.join("meta").join("db").exists() {
            crate::btrfs::create_subvolume(&dir.join("meta").join("db"))?;
            std::fs::set_permissions(dir.join("meta").join("db"), fs::Permissions::from_mode(0o700))?;
        }
        let id = ids::new_id();
        let db = Db::open(&db_file(&dir))?;
        let p = Project { store, name: name.to_string(), dir, id: id.clone(), db };
        let mut rec = Record::new("project");
        rec.data = serde_json::json!({"name": name, "id": id, "config": config});
        p.append(rec, actor)?;
        Ok(p)
    }

    /// Folders of a project without records (for a restore or a replica).
    pub fn create_empty(store: &'a Store, name: &str) -> Result<Project<'a>> {
        check_name("project", name)?;
        let dir = store.projects_dir().join(name);
        if db_file(&dir).exists() {
            bail!("project '{name}' already exists");
        }
        if !crate::btrfs::is_btrfs(&store.projects_dir()) {
            bail!("{} is not on btrfs", store.projects_dir().display());
        }
        mkdir_mode(&dir, 0o755)?;
        mkdir_mode(&dir.join("states"), 0o700)?;
        mkdir_mode(&dir.join("lines"), 0o711)?;
        mkdir_mode(&dir.join("meta"), 0o700)?;
        for d in ["stage", "scan", "work"] {
            mkdir_mode(&dir.join("meta").join(d), 0o700)?;
        }
        if !dir.join("meta").join("db").exists() {
            crate::btrfs::create_subvolume(&dir.join("meta").join("db"))?;
            std::fs::set_permissions(dir.join("meta").join("db"), fs::Permissions::from_mode(0o700))?;
        }
        let db = Db::open(&db_file(&dir))?;
        Ok(Project { store, name: name.to_string(), dir, id: String::new(), db })
    }

    /// Open a project that may not have its records yet.
    pub fn open_partial(store: &'a Store, name: &str) -> Result<Project<'a>> {
        let dir = store.projects_dir().join(name);
        let db = Db::open(&db_file(&dir))?;
        let mut p = Project { store, name: name.to_string(), dir, id: String::new(), db };
        p.id = p.project_id().unwrap_or_default();
        Ok(p)
    }

    pub fn open(store: &'a Store, name: &str) -> Result<Project<'a>> {
        let _t = crate::trace::span("open project");
        check_name("project", name)?;
        let dir = store.projects_dir().join(name);
        if !db_file(&dir).is_file() {
            bail!("project '{name}' not found");
        }
        let db = Db::open(&db_file(&dir))?;
        let mut p = Project { store, name: name.to_string(), dir, id: String::new(), db };
        if p.db.stale {
            // A schema upgrade: derived tables come back from the records.
            p.rebuild_index()?;
            p.db.mark_current()?;
        }
        p.id = p.project_id()?;
        Ok(p)
    }

    /// Take the project id from the records (after a replica or a restore received them).
    pub fn reload_id(&mut self) -> Result<()> {
        self.id = self.project_id()?;
        Ok(())
    }

    fn project_id(&self) -> Result<String> {
        let id: Option<String> = self
            .db
            .conn
            .query_row("SELECT json_extract(raw, '$.r.data.id') FROM records WHERE op='project' ORDER BY time LIMIT 1", [], |r| {
                r.get(0)
            })
            .ok();
        id.ok_or_else(|| anyhow!("project record missing"))
    }

    pub fn lock(&self) -> Result<Lock> {
        let f = fs::OpenOptions::new().create(true).write(true).truncate(false).open(self.meta_dir().join("lock"))?;
        rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)?;
        Ok(Lock { _f: f })
    }

    fn state_present(&self) -> impl Fn(&str) -> bool + '_ {
        let sd = self.states_dir();
        move |id: &str| sd.join(id).exists()
    }

    /// Rebuild the derived tables from the records.
    pub fn rebuild_index(&self) -> Result<()> {
        let present = self.state_present();
        self.db.with_tx(|| {
            self.db.clear_derived()?;
            for line in self.db.all_lines()? {
                let s = oplog::parse_line(&line, None)?;
                self.db.derive(&s.record, &present, &self.store.machine.id)?;
            }
            Ok(())
        })?;
        for (id, deleted) in self.db.present_states()? {
            if deleted || !self.state_path(&id).exists() {
                self.db.set_present(&id, false)?;
            }
        }
        Ok(())
    }

    /// Write a record: sign it and store it with its derived rows in one transaction.
    /// The caller holds the project lock and has created all snapshots the record names.
    pub fn append(&self, mut rec: Record, actor: &Actor) -> Result<Record> {
        let _t = crate::trace::span(format!("record {}", rec.op));
        let m = &self.store.machine;
        let present = self.state_present();
        let project = if rec.op == "project" { rec.data["id"].as_str().unwrap_or("").to_string() } else { self.id.clone() };
        if project.is_empty() {
            bail!("project id unknown: the project has no records yet");
        }
        self.db.with_tx(|| {
            let mut seq = self.db.last_seq(&m.id)?;
            if seq == 0 {
                // The first record of this machine publishes its key.
                let mut mr = Record::new("machine");
                mr.id = ids::new_id();
                mr.machine = m.id.clone();
                mr.seq = 1;
                mr.time = ids::now_ms();
                mr.project = project.clone();
                mr.actor = actor.clone();
                mr.data = serde_json::json!({"name": m.name, "public_key": hex::encode(m.key.verifying_key().to_bytes())});
                let line = oplog::sign_line(&mr, &m.key)?;
                self.db.insert_record(&mr, &line)?;
                self.db.derive(&mr, &present, &m.id)?;
                seq = 1;
            }
            rec.id = ids::new_id();
            rec.machine = m.id.clone();
            rec.seq = seq + 1;
            rec.time = ids::now_ms();
            rec.project = project.clone();
            rec.actor = actor.clone();
            crate::model::validate(&rec)?;
            let line = oplog::sign_line(&rec, &m.key)?;
            self.db.insert_record(&rec, &line)?;
            self.db.derive(&rec, &present, &m.id)?;
            Ok(())
        })?;
        Ok(rec)
    }

    /// Records of other machines (replication, restore): check each machine's chain (its key
    /// from its first record, contiguous sequence numbers, signatures) and the project id, then
    /// store them with their derived rows in one transaction. Returns the number of new records.
    ///
    /// Only records of trusted machines are taken (see `Trust`).
    pub fn import_lines(&self, lines: &[String], trust: Trust) -> Result<usize> {
        use std::collections::{BTreeMap, BTreeSet};
        let mut parsed: Vec<oplog::Signed> = Vec::new();
        for l in lines {
            parsed.push(oplog::parse_line(l, None)?);
        }
        let project = if self.id.is_empty() {
            parsed
                .iter()
                .find(|s| s.record.op == "project")
                .and_then(|s| s.record.data["id"].as_str().map(|x| x.to_string()))
                .unwrap_or_default()
        } else {
            self.id.clone()
        };
        for s in &parsed {
            crate::model::validate(&s.record)?;
            if !project.is_empty() && s.record.project != project {
                bail!(
                    "record {} of machine {} belongs to another project ({})",
                    s.record.seq,
                    s.record.machine,
                    s.record.project
                );
            }
        }
        let mut by: BTreeMap<String, Vec<oplog::Signed>> = BTreeMap::new();
        for s in parsed {
            by.entry(s.record.machine.clone()).or_default().push(s);
        }
        let present = self.state_present();
        let mut fresh: Vec<oplog::Signed> = Vec::new();
        let mut keys: BTreeMap<String, String> = BTreeMap::new();
        for (machine, recs) in by.iter_mut() {
            recs.sort_by_key(|s| s.record.seq);
            let mut last = self.db.last_seq(machine)?;
            let key = if last > 0 {
                let first = self.db.record_line(machine, 1)?.ok_or_else(|| anyhow!("no records"))?;
                oplog::machine_key(&oplog::parse_line(&first, None)?.record)?
            } else {
                let f = recs.first().ok_or_else(|| anyhow!("no records"))?;
                if f.record.op != "machine" || f.record.seq != 1 {
                    bail!("records of machine {machine} do not start with its key");
                }
                oplog::machine_key(&f.record)?
            };
            // Records that claim this machine must carry this machine's key.
            if machine == &self.store.machine.id && key != self.store.machine.key.verifying_key() {
                bail!("records claim this machine's id with another key");
            }
            keys.insert(machine.clone(), hex::encode(key.to_bytes()));
            for s in recs.iter() {
                if s.record.seq <= last {
                    // A record we have: the copy must be the same bytes (else a changed or a
                    // forked history).
                    let stored = self.db.record_line(machine, s.record.seq)?;
                    if stored.as_deref() != Some(s.line.as_str()) {
                        bail!(
                            "record {} of machine {machine} differs from the stored record (changed or forked history)",
                            s.record.seq
                        );
                    }
                    continue;
                }
                if s.record.seq != last + 1 {
                    bail!("gap in the records of machine {machine}: have {last}, got {}", s.record.seq);
                }
                oplog::parse_line(&s.line, Some(&key))
                    .with_context(|| format!("record {} of machine {machine}", s.record.seq))?;
                for st in &s.record.states {
                    if self.db.state(&st.id)?.is_some() || fresh.iter().any(|f| f.record.states.iter().any(|x| x.id == st.id)) {
                        bail!("record {} of machine {machine} reuses state id {}", s.record.seq, st.id);
                    }
                }
                last = s.record.seq;
                fresh.push(oplog::Signed { record: s.record.clone(), line: s.line.clone() });
            }
        }
        // Trust: known machines, then machines vouched for by trusted ones in this batch.
        if !matches!(trust, Trust::All) {
            let mut trusted: BTreeSet<String> = BTreeSet::new();
            for m in keys.keys() {
                if let Some(k) = self.db.trusted(m)? {
                    if Some(&k) == keys.get(m) {
                        trusted.insert(m.clone());
                    }
                }
            }
            if let Trust::Peer(m, k) = trust {
                if keys.get(m).map(|x| x == k).unwrap_or(false) {
                    trusted.insert(m.to_string());
                }
            }
            trusted.insert(self.store.machine.id.clone());
            loop {
                let before = trusted.len();
                for s in &fresh {
                    if s.record.op == "machine.trust" && trusted.contains(&s.record.machine) {
                        let (m, k) =
                            (s.record.data["machine"].as_str().unwrap_or(""), s.record.data["public_key"].as_str().unwrap_or(""));
                        if keys.get(m).map(|x| x == k).unwrap_or(false) {
                            trusted.insert(m.to_string());
                        }
                    }
                }
                if trusted.len() == before {
                    break;
                }
            }
            let untrusted: Vec<String> = keys.keys().filter(|m| !trusted.contains(*m)).cloned().collect();
            if !untrusted.is_empty() {
                let list: Vec<String> = untrusted
                    .iter()
                    .map(|m| format!("{} (key {})", &crate::ids::display(m)[..8], keys.get(m).map(|k| k.as_str()).unwrap_or("")))
                    .collect();
                bail!(
                    "records of untrusted machine(s): {}. If you trust them, run: layr machines trust <id> --key <key>",
                    list.join(", ")
                );
            }
        }
        fresh.sort_by(|a, b| {
            (a.record.time, &a.record.machine, a.record.seq).cmp(&(b.record.time, &b.record.machine, b.record.seq))
        });
        let n = fresh.len();
        let local = self.store.machine.id.clone();
        self.db.with_tx(|| {
            for s in &fresh {
                if self.db.insert_record(&s.record, &s.line)? {
                    self.db.derive(&s.record, &present, &local)?;
                }
            }
            Ok(())
        })?;
        Ok(n)
    }

    /// Machines whose records this project has but does not trust yet, with their keys.
    pub fn machine_keys_of(&self, lines: &[String]) -> Vec<(String, String)> {
        let mut v = Vec::new();
        for l in lines {
            if let Ok(s) = oplog::parse_line(l, None) {
                if s.record.op == "machine" {
                    if let Some(k) = s.record.data["public_key"].as_str() {
                        v.push((s.record.machine.clone(), k.to_string()));
                    }
                }
            }
        }
        v
    }

    /// All records as JSONL (by machine and sequence number): the exchange and backup format.
    pub fn export_jsonl(&self) -> Result<String> {
        let mut out = String::new();
        for m in self.db.record_machines()? {
            for l in self.db.lines_after(&m, 0)? {
                out.push_str(&l);
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Check every machine's chain of records: key, sequence numbers, signatures.
    pub fn verify_records(&self) -> Result<Vec<(String, u64)>> {
        let mut out = Vec::new();
        for m in self.db.record_machines()? {
            let lines = self.db.lines_after(&m, 0)?;
            let first = oplog::parse_line(lines.first().ok_or_else(|| anyhow!("no records"))?, None)?;
            if first.record.op != "machine" {
                bail!("machine {m}: the first record is not its key");
            }
            let key = oplog::machine_key(&first.record)?;
            let mut seq = 0;
            for l in &lines {
                let s = oplog::parse_line(l, Some(&key)).with_context(|| format!("machine {m}"))?;
                if s.record.seq != seq + 1 {
                    bail!("machine {m}: sequence gap at {}", s.record.seq);
                }
                seq = s.record.seq;
            }
            out.push((m, seq));
        }
        Ok(out)
    }

    pub fn config_value(&self, key: &str) -> Result<Option<serde_json::Value>> {
        self.db.config(key)
    }

    pub fn config_u64(&self, key: &str, default: u64) -> u64 {
        self.db.config(key).ok().flatten().and_then(|v| v.as_u64()).unwrap_or(default)
    }

    pub fn config_bool(&self, key: &str, default: bool) -> bool {
        self.db.config(key).ok().flatten().and_then(|v| v.as_bool()).unwrap_or(default)
    }

    pub fn config_strings(&self, key: &str) -> Vec<String> {
        self.db
            .config(key)
            .ok()
            .flatten()
            .and_then(|v| v.as_array().cloned())
            .map(|a| a.into_iter().filter_map(|x| x.as_str().map(|s| s.to_string())).collect())
            .unwrap_or_default()
    }

    /// The single admin of stores made before roles (`admin`, earlier `lead`); see `access`.
    pub fn legacy_admin(&self) -> u32 {
        self.config_u64("admin", self.config_u64("lead", 0)) as u32
    }
}
