//! The project database (SQLite). The `records` table holds the signed, immutable operation
//! records: the source of truth. All other tables are derived from the records. A record and
//! its derived rows are written in one transaction, so they cannot disagree after a crash. The
//! derived tables can be rebuilt from the records at any time.

use crate::ids;
use crate::model::{Attestation, Line, QueueChange, QueueEntry, Record, RefChange, RefState, StateKind, StateRec, Subject};
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
use sha2::{Digest, Sha256};
use std::path::Path;

pub struct Db {
    pub conn: Connection,
    /// The derived tables were dropped by a schema upgrade: the project rebuilds them.
    pub stale: bool,
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS records (id TEXT PRIMARY KEY, machine TEXT, seq INTEGER, time INTEGER, op TEXT, actor_uid INTEGER, raw TEXT);
CREATE INDEX IF NOT EXISTS records_time ON records(time);
CREATE UNIQUE INDEX IF NOT EXISTS records_seq ON records(machine, seq);
CREATE TABLE IF NOT EXISTS states (id TEXT PRIMARY KEY, display TEXT UNIQUE, kind TEXT, line TEXT, time INTEGER, rec TEXT, present INTEGER DEFAULT 1, deleted INTEGER DEFAULT 0, dropped INTEGER DEFAULT 0, machine TEXT);
CREATE INDEX IF NOT EXISTS states_line ON states(line, kind, time);
CREATE TABLE IF NOT EXISTS parents (state TEXT, parent TEXT, ord INTEGER, PRIMARY KEY(state, ord));
CREATE INDEX IF NOT EXISTS parents_parent ON parents(parent);
CREATE TABLE IF NOT EXISTS lines (id TEXT PRIMARY KEY, name TEXT, owner INTEGER, machine TEXT, head TEXT, deleted INTEGER DEFAULT 0, time INTEGER, ord TEXT);
CREATE TABLE IF NOT EXISTS line_heads (line TEXT, machine TEXT, head TEXT, time INTEGER, ord TEXT, PRIMARY KEY(line, machine));
CREATE TABLE IF NOT EXISTS tags (name TEXT PRIMARY KEY, state TEXT, message TEXT, time INTEGER, deleted INTEGER DEFAULT 0, ord TEXT);
CREATE TABLE IF NOT EXISTS remote_refs (name TEXT PRIMARY KEY, state TEXT, gcommit TEXT, time INTEGER, ord TEXT);
CREATE TABLE IF NOT EXISTS git_map (state TEXT, gcommit TEXT, PRIMARY KEY(state, gcommit));
CREATE INDEX IF NOT EXISTS git_map_commit ON git_map(gcommit);
CREATE TABLE IF NOT EXISTS exports (id TEXT PRIMARY KEY, state TEXT, remote TEXT, branch TEXT, gcommit TEXT, status TEXT, time INTEGER, ord TEXT);
CREATE TABLE IF NOT EXISTS checks (id TEXT PRIMARY KEY, state TEXT, command TEXT, exit INTEGER, time INTEGER, actor TEXT);
CREATE TABLE IF NOT EXISTS machines (id TEXT PRIMARY KEY, name TEXT, public_key TEXT);
CREATE TABLE IF NOT EXISTS config (key TEXT PRIMARY KEY, value TEXT, ord TEXT);
CREATE TABLE IF NOT EXISTS trusted (machine TEXT PRIMARY KEY, public_key TEXT, by TEXT);
CREATE TABLE IF NOT EXISTS approvals (id TEXT PRIMARY KEY, state TEXT, uid INTEGER, time INTEGER, withdrawn INTEGER DEFAULT 0);
CREATE TABLE IF NOT EXISTS refs (name TEXT PRIMARY KEY, target TEXT, expires_at INTEGER, deleted INTEGER DEFAULT 0, ord TEXT);
CREATE TABLE IF NOT EXISTS ref_history (id TEXT PRIMARY KEY, name TEXT, rec TEXT, ord TEXT);
CREATE INDEX IF NOT EXISTS ref_history_name ON ref_history(name, ord);
CREATE TABLE IF NOT EXISTS attestations (id TEXT PRIMARY KEY, subject TEXT, kind TEXT, rec TEXT, machine TEXT, uid INTEGER, local INTEGER DEFAULT 0, ord TEXT);
CREATE INDEX IF NOT EXISTS attestations_subject ON attestations(subject, kind, ord);
CREATE TABLE IF NOT EXISTS queue_entries (id TEXT PRIMARY KEY, rec TEXT, deleted INTEGER DEFAULT 0, ord TEXT);
CREATE TABLE IF NOT EXISTS attestation_revocations (id TEXT, target TEXT, machine TEXT, uid INTEGER, ord TEXT, PRIMARY KEY(id,target));
"#;

/// Version of the derived tables. Records never change shape; a database with an older version
/// drops its derived tables, creates them new and rebuilds them from the records.
pub const SCHEMA_VERSION: i64 = 6;

const DERIVED: &[&str] = &[
    "states",
    "parents",
    "lines",
    "line_heads",
    "tags",
    "remote_refs",
    "git_map",
    "exports",
    "checks",
    "machines",
    "config",
    "trusted",
    "approvals",
    "refs",
    "ref_history",
    "attestations",
    "attestation_revocations",
    "queue_entries",
];

/// Derived tables of older versions, dropped on migration.
const RETIRED: &[&str] = &["slots", "mirrors", "groups", "group_lines"];

impl Db {
    /// Open the database. Returns it and whether the derived tables must be rebuilt.
    pub fn open(path: &Path) -> Result<Db> {
        let conn = Connection::open(path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        // FULL: a committed record survives a power loss. The database has its own subvolume,
        // so this fsync costs a few milliseconds even right after a snapshot.
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(std::time::Duration::from_secs(30))?;
        // One write transaction: another process (the service's periodic pass) may open a
        // project that `init` is creating. Without it, the other process saw the new tables
        // before the version and took the database for an old one: it dropped the derived
        // tables, and `init` failed with "no such table: states".
        conn.execute_batch("BEGIN IMMEDIATE")?;
        let opened = Self::prepare(&conn);
        match &opened {
            Ok(_) => conn.execute_batch("COMMIT")?,
            Err(_) => {
                let _ = conn.execute_batch("ROLLBACK");
            }
        }
        let stale = opened.map_err(|e| e.context(path.display().to_string()))?;
        let db = Db { conn, stale };
        // Older binaries can still add legacy records. Import their evidence on each open.
        db.with_tx(|| {
            let mut st = db.conn.prepare(
                "SELECT raw FROM records WHERE op IN ('approve','approve.withdraw','check') ORDER BY time,machine,seq",
            )?;
            let raw = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
            for line in raw {
                db.derive_attestation(&crate::oplog::parse_line(&line, None)?.record, false)?;
            }
            Ok(())
        })?;
        Ok(db)
    }

    /// Create or upgrade the schema inside the open transaction. Returns whether the derived
    /// tables must be rebuilt from the records.
    fn prepare(conn: &Connection) -> Result<bool> {
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        // Only a database that holds records has derived tables to rebuild. A database that a
        // concurrent `init` is creating has the tables but no record yet.
        let has_records: bool =
            conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='records'", [], |r| {
                r.get::<_, i64>(0)
            })? > 0
                && conn.query_row("SELECT EXISTS(SELECT 1 FROM records)", [], |r| r.get::<_, bool>(0))?;
        if version < SCHEMA_VERSION {
            if has_records {
                // The project rebuilds the derived tables and only then writes the version
                // (`mark_current`): a failed rebuild is tried again on the next open.
                for t in DERIVED.iter().chain(RETIRED) {
                    conn.execute_batch(&format!("DROP TABLE IF EXISTS {t}"))?;
                }
                conn.execute_batch(SCHEMA)?;
                return Ok(true);
            }
            conn.execute_batch(SCHEMA)?;
            conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        } else if version > SCHEMA_VERSION {
            anyhow::bail!("written by a newer layr (schema {version})");
        } else {
            conn.execute_batch(SCHEMA)?;
        }
        Ok(false)
    }

    /// Run `f` in one write transaction (`BEGIN IMMEDIATE`): all or nothing.
    /// Write the schema version after the derived tables were rebuilt.
    pub fn mark_current(&mut self) -> Result<()> {
        self.conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        self.stale = false;
        Ok(())
    }

    pub fn with_tx<T>(&self, f: impl FnOnce() -> Result<T>) -> Result<T> {
        self.conn.execute_batch("BEGIN IMMEDIATE")?;
        match f() {
            Ok(v) => {
                self.conn.execute_batch("COMMIT")?;
                Ok(v)
            }
            Err(e) => {
                let _ = self.conn.execute_batch("ROLLBACK");
                Err(e)
            }
        }
    }

    /// Delete all derived rows (records stay).
    pub fn clear_derived(&self) -> Result<()> {
        self.conn.execute_batch(
            "DELETE FROM states; DELETE FROM parents; DELETE FROM lines; DELETE FROM line_heads;
             DELETE FROM tags; DELETE FROM remote_refs; DELETE FROM git_map; DELETE FROM exports; DELETE FROM checks;
             DELETE FROM machines; DELETE FROM config;
             DELETE FROM trusted; DELETE FROM approvals; DELETE FROM refs; DELETE FROM ref_history;
             DELETE FROM attestations; DELETE FROM attestation_revocations; DELETE FROM queue_entries;",
        )?;
        Ok(())
    }

    /// Insert a stored record line. Returns false if the record exists already.
    pub fn insert_record(&self, rec: &Record, line: &str) -> Result<bool> {
        let n = self.conn.execute(
            "INSERT OR IGNORE INTO records(id, machine, seq, time, op, actor_uid, raw) VALUES(?1,?2,?3,?4,?5,?6,?7)",
            params![rec.id, rec.machine, rec.seq as i64, rec.time, rec.op, rec.actor.uid as i64, line],
        )?;
        Ok(n > 0)
    }

    pub fn last_seq(&self, machine: &str) -> Result<u64> {
        let v: Option<i64> =
            self.conn.query_row("SELECT MAX(seq) FROM records WHERE machine=?1", params![machine], |r| r.get(0))?;
        Ok(v.unwrap_or(0) as u64)
    }

    /// Stored lines of one machine after a sequence number, in order.
    pub fn lines_after(&self, machine: &str, seq: u64) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT raw FROM records WHERE machine=?1 AND seq>?2 ORDER BY seq")?;
        let v = st.query_map(params![machine, seq as i64], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn record_line(&self, machine: &str, seq: u64) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT raw FROM records WHERE machine=?1 AND seq=?2", params![machine, seq as i64], |r| r.get(0))
            .optional()?)
    }

    pub fn record_machines(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT machine FROM records ORDER BY machine")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// All stored lines in apply order.
    pub fn all_lines(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT raw FROM records ORDER BY time, machine, seq")?;
        let v = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Update the derived tables for one record. Runs inside the caller's transaction.
    ///
    /// The result does not depend on the order in which records arrive: every row that a
    /// later record can replace keeps the order key of the record that wrote it (time,
    /// machine, sequence number), and only a record with a larger key replaces it. Deletes
    /// leave a marked row for the same reason. Settings that act on this machine (admin,
    /// members, merge commands, remotes) and the lines that have their
    /// working folder here change only by this machine's records; the values written by other
    /// machines are kept under `<key>@<machine>`.
    pub fn derive(&self, rec: &Record, state_present: &dyn Fn(&str) -> bool, local: &str) -> Result<()> {
        let tx = &self.conn;
        let ord = format!("{:016}:{}:{:012}", rec.time.max(0), rec.machine, rec.seq);
        let foreign = rec.machine != local;
        for s in &rec.states {
            // A state id names one state forever: a second record with the same id is refused
            // (the import checks this before; here it is a guard).
            let exists: bool =
                tx.query_row("SELECT COUNT(*) FROM states WHERE id=?1", params![s.id], |r| r.get::<_, i64>(0))? > 0;
            if exists {
                anyhow::bail!("state {} is named by two records", s.id);
            }
            let present = state_present(&s.id);
            tx.execute(
                "INSERT INTO states(id, display, kind, line, time, rec, present, machine) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)",
                params![
                    s.id,
                    ids::display(&s.id),
                    kind_str(s.kind),
                    s.line,
                    s.time,
                    serde_json::to_string(s)?,
                    present as i64,
                    rec.machine
                ],
            )?;
            for p in &s.parents {
                if self.state(p)?.map(|p| p.kind.auxiliary()).unwrap_or(false) {
                    anyhow::bail!("source history cannot have a cache or derived parent");
                }
            }
            for (i, p) in s.parents.iter().enumerate() {
                tx.execute("INSERT OR IGNORE INTO parents(state, parent, ord) VALUES(?1,?2,?3)", params![s.id, p, i as i64])?;
            }
        }
        for (id, ch) in &rec.lines {
            // Heads per machine: each machine's own records, in its order.
            if let Some(a) = &ch.after {
                if self.state(&a.head)?.map(|s| s.kind.auxiliary()).unwrap_or(false) {
                    anyhow::bail!("line head cannot be a cache or derived state");
                }
                tx.execute(
                    "INSERT INTO line_heads(line, machine, head, time, ord) VALUES(?1,?2,?3,?4,?5)
                     ON CONFLICT(line, machine) DO UPDATE SET head=?3, time=?4, ord=?5 WHERE excluded.ord > line_heads.ord",
                    params![id, rec.machine, a.head, rec.time, ord],
                )?;
            }
            let row_machine: Option<String> =
                tx.query_row("SELECT machine FROM lines WHERE id=?1", params![id], |r| r.get(0)).optional()?;
            if foreign && row_machine.as_deref() == Some(local) {
                continue;
            }
            match &ch.after {
                Some(a) => {
                    tx.execute(
                        "INSERT INTO lines(id, name, owner, machine, head, deleted, time, ord) VALUES(?1,?2,?3,?4,?5,0,?6,?7)
                         ON CONFLICT(id) DO UPDATE SET name=?2, owner=?3, machine=?4, head=?5, deleted=0, time=?6, ord=?7
                         WHERE excluded.ord > lines.ord OR (lines.machine <> ?4 AND ?4 = ?8)",
                        params![id, a.name, a.owner as i64, a.machine, a.head, rec.time, ord, local],
                    )?;
                }
                None => {
                    tx.execute(
                        "INSERT INTO lines(id, name, owner, machine, head, deleted, time, ord) VALUES(?1,'',0,?3,'',1,?2,?4)
                         ON CONFLICT(id) DO UPDATE SET deleted=1, time=?2, ord=?4 WHERE excluded.ord > lines.ord",
                        params![id, rec.time, rec.machine, ord],
                    )?;
                }
            }
        }
        if let Some(value) = rec.data.get("queue_change") {
            let change: QueueChange = serde_json::from_value(value.clone())?;
            tx.execute(
                "INSERT INTO queue_entries(id,rec,deleted,ord) VALUES(?1,?2,?3,?4)
                ON CONFLICT(id) DO UPDATE SET rec=?2,deleted=?3,ord=?4 WHERE excluded.ord>queue_entries.ord",
                params![
                    change.id,
                    change.after.as_ref().map(serde_json::to_string).transpose()?,
                    change.after.is_none() as i64,
                    ord
                ],
            )?;
        }
        self.derive_attestation(rec, !foreign)?;
        if !foreign {
            if let Some(a) = rec.data.get("attestation") {
                if a["kind"] == "revoke" {
                    for id in rec.data["revokes"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                        tx.execute("UPDATE approvals SET withdrawn=1 WHERE id=?1 AND uid=?2", params![id, rec.actor.uid as i64])?;
                    }
                }
                if a["kind"] == "approval" && rec.data.get("declassify").is_none() {
                    tx.execute(
                        "INSERT OR IGNORE INTO approvals(id,state,uid,time) VALUES(?1,?2,?3,?4)",
                        params![rec.id, a["subject"]["id"].as_str(), rec.actor.uid as i64, rec.time],
                    )?;
                }
            }
        }
        if let Some(v) = rec.data.get("ref_change") {
            let ch: RefChange = serde_json::from_value(v.clone())?;
            tx.execute(
                "INSERT OR IGNORE INTO ref_history(id,name,rec,ord) VALUES(?1,?2,?3,?4)",
                params![rec.id, ch.name, serde_json::to_string(&ch)?, ord],
            )?;
            tx.execute(
                "INSERT INTO refs(name,target,expires_at,deleted,ord) VALUES(?1,?2,?3,?4,?5)
                ON CONFLICT(name) DO UPDATE SET target=?2,expires_at=?3,deleted=?4,ord=?5 WHERE excluded.ord > refs.ord",
                params![
                    ch.name,
                    ch.after.as_ref().map(|v| &v.target),
                    ch.after.as_ref().and_then(|v| v.expires_at),
                    ch.after.is_none() as i64,
                    ord
                ],
            )?;
        }
        let d = &rec.data;
        let s = |k: &str| d.get(k).and_then(|v| v.as_str()).map(|x| x.to_string());
        match rec.op.as_str() {
            "machine" => {
                tx.execute(
                    "INSERT OR IGNORE INTO machines(id, name, public_key) VALUES(?1,?2,?3)",
                    params![rec.machine, s("name"), s("public_key")],
                )?;
                if !foreign {
                    tx.execute(
                        "INSERT OR IGNORE INTO trusted(machine, public_key, by) VALUES(?1,?2,?1)",
                        params![rec.machine, s("public_key")],
                    )?;
                }
            }
            "machine.trust" => {
                tx.execute(
                    "INSERT OR IGNORE INTO trusted(machine, public_key, by) VALUES(?1,?2,?3)",
                    params![s("machine"), s("public_key"), rec.machine],
                )?;
            }
            "project" | "config" | "overlay.create" | "overlay.readers" | "overlay.use" | "overlay.show-to-project" => {
                if let Some(obj) = d.get("config").and_then(|v| v.as_object()) {
                    for (k, v) in obj {
                        let key = if foreign && local_key(k) { format!("{k}@{}", rec.machine) } else { k.clone() };
                        let val = if v.is_null() { None } else { Some(v.to_string()) };
                        tx.execute(
                            "INSERT INTO config(key, value, ord) VALUES(?1,?2,?3)
                             ON CONFLICT(key) DO UPDATE SET value=?2, ord=?3 WHERE excluded.ord > config.ord",
                            params![key, val, ord],
                        )?;
                    }
                }
            }
            "tag" => {
                tx.execute(
                    "INSERT INTO tags(name, state, message, time, deleted, ord) VALUES(?1,?2,?3,?4,0,?5)
                     ON CONFLICT(name) DO UPDATE SET state=?2, message=?3, time=?4, deleted=0, ord=?5 WHERE excluded.ord > tags.ord",
                    params![s("name"), s("state"), s("message"), rec.time, ord],
                )?;
            }
            "tag.delete" => {
                tx.execute(
                    "INSERT INTO tags(name, state, message, time, deleted, ord) VALUES(?1,NULL,NULL,?2,1,?3)
                     ON CONFLICT(name) DO UPDATE SET deleted=1, time=?2, ord=?3 WHERE excluded.ord > tags.ord",
                    params![s("name"), rec.time, ord],
                )?;
            }
            "import" | "remote.ref" => {
                if let (Some(name), Some(st)) = (s("ref"), s("state")) {
                    tx.execute(
                        "INSERT INTO remote_refs(name, state, gcommit, time, ord) VALUES(?1,?2,?3,?4,?5)
                         ON CONFLICT(name) DO UPDATE SET state=?2, gcommit=?3, time=?4, ord=?5 WHERE excluded.ord > remote_refs.ord",
                        params![name, st, s("commit"), rec.time, ord],
                    )?;
                }
                if let (Some(st), Some(c)) = (s("state"), s("commit")) {
                    tx.execute("INSERT OR IGNORE INTO git_map(state, gcommit) VALUES(?1,?2)", params![st, c])?;
                }
            }
            "export" => {
                tx.execute(
                    "INSERT INTO exports(id, state, remote, branch, gcommit, status, time, ord) VALUES(?1,?2,?3,?4,?5,?6,?7,?8)
                     ON CONFLICT(id) DO UPDATE SET state=?2, remote=?3, branch=?4, gcommit=?5, status=?6, time=?7, ord=?8
                     WHERE excluded.ord > exports.ord",
                    params![s("export"), s("state"), s("remote"), s("branch"), s("commit"), s("status"), rec.time, ord],
                )?;
                if let (Some(st), Some(c)) = (s("state"), s("commit")) {
                    tx.execute("INSERT OR IGNORE INTO git_map(state, gcommit) VALUES(?1,?2)", params![st, c])?;
                }
                // The commits made for the ancestors of the exported state.
                if let Some(m) = d.get("commits").and_then(|v| v.as_object()) {
                    for (st, c) in m {
                        if let Some(c) = c.as_str() {
                            tx.execute("INSERT OR IGNORE INTO git_map(state, gcommit) VALUES(?1,?2)", params![st, c])?;
                        }
                    }
                }
                if s("status").as_deref() == Some("pushed") {
                    if let (Some(r), Some(b), Some(st), Some(c)) = (s("remote"), s("branch"), s("state"), s("commit")) {
                        tx.execute(
                            "INSERT INTO remote_refs(name, state, gcommit, time, ord) VALUES(?1,?2,?3,?4,?5)
                             ON CONFLICT(name) DO UPDATE SET state=?2, gcommit=?3, time=?4, ord=?5 WHERE excluded.ord > remote_refs.ord",
                            params![format!("{r}/{b}"), st, c, rec.time, ord],
                        )?;
                    }
                }
            }
            "check" => {
                tx.execute(
                    "INSERT OR IGNORE INTO checks(id, state, command, exit, time, actor) VALUES(?1,?2,?3,?4,?5,?6)",
                    params![rec.id, s("state"), s("command"), d.get("exit").and_then(|v| v.as_i64()), rec.time, rec.actor.name],
                )?;
            }
            "gc" => {
                if let Some(arr) = d.get("deleted").and_then(|v| v.as_array()) {
                    for id in arr.iter().filter_map(|v| v.as_str()) {
                        if !foreign && !state_present(id) {
                            tx.execute("UPDATE states SET deleted=1, present=0 WHERE id=?1", params![id])?;
                        }
                    }
                }
            }
            "save.caches" | "save.layers" => {
                // Only the machine that made the state adds its caches (names and subvolume
                // UUIDs, which a receiver checks). A record adds to the caches of the state and
                // never drops one that an earlier record of the state gave it.
                if let Some(st) = s("state") {
                    let row: Option<(String, String)> = tx
                        .query_row("SELECT rec, machine FROM states WHERE id=?1", params![st], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    if let Some((raw, machine)) = row {
                        if machine == rec.machine {
                            let mut v: serde_json::Value = serde_json::from_str(&raw)?;
                            for (key, alias, field) in [("layers", "caches", "caches"), ("layer_uuids", "cache_uuids", "uuids")] {
                                let mut all =
                                    v.get(key).or_else(|| v.get(alias)).and_then(|x| x.as_object()).cloned().unwrap_or_default();
                                if let Some(new) = d
                                    .get(field)
                                    .or_else(|| if field == "caches" { d.get("layers") } else { None })
                                    .and_then(|x| x.as_object())
                                {
                                    all.extend(new.clone());
                                }
                                v.as_object_mut().unwrap().remove(alias);
                                v[key] = serde_json::Value::Object(all);
                            }
                            v.as_object_mut().unwrap().remove("cache_by");
                            v.as_object_mut().unwrap().remove("cache_shared");
                            v.as_object_mut().unwrap().remove("cache_platform");
                            // The platform of the newest save; all saves of a state are made on
                            // its own machine.
                            match d.get("platform").and_then(|x| x.as_str()) {
                                Some(p) => v["layer_platform"] = serde_json::json!(p),
                                None => {
                                    v.as_object_mut().unwrap().remove("layer_platform");
                                }
                            }
                            v["layer_by"] = serde_json::json!(rec.actor.uid);
                            v["layer_shared"] = serde_json::json!(d.get("shared").and_then(|x| x.as_bool()).unwrap_or(false));
                            tx.execute("UPDATE states SET rec=?2 WHERE id=?1", params![st, v.to_string()])?;
                        }
                    }
                }
            }
            // Approvals name users of this machine: only local records count.
            "approve" if !foreign && s("state").map(|x| crate::model::is_uuid(&x)).unwrap_or(false) => {
                tx.execute(
                    "INSERT OR IGNORE INTO approvals(id, state, uid, time) VALUES(?1,?2,?3,?4)",
                    params![rec.id, s("state"), rec.actor.uid as i64, rec.time],
                )?;
            }
            "approve.withdraw" if !foreign => {
                tx.execute(
                    "UPDATE approvals SET withdrawn=1 WHERE state=?1 AND uid=?2",
                    params![s("state"), rec.actor.uid as i64],
                )?;
            }
            "stash.drop" => {
                tx.execute("UPDATE states SET dropped=1 WHERE id=?1", params![s("state")])?;
            }
            _ => {}
        }
        // The overlays a line shows (`stack.<line id>`), from any record that changes them.
        if let Some(st) = d.get("stack") {
            if let (Some(line), Some(layers)) = (st.get("line").and_then(|v| v.as_str()), st.get("layers")) {
                let key = if foreign { format!("stack.{line}@{}", rec.machine) } else { format!("stack.{line}") };
                tx.execute(
                    "INSERT INTO config(key, value, ord) VALUES(?1,?2,?3)
                     ON CONFLICT(key) DO UPDATE SET value=?2, ord=?3 WHERE excluded.ord > config.ord",
                    params![key, layers.to_string(), ord],
                )?;
            }
        }
        Ok(())
    }

    /// The subvolume UUID of a cache snapshot, from the signed save.caches record.
    pub fn cache_uuid(&self, state: &str, name: &str) -> Result<Option<String>> {
        let raw: Option<String> =
            self.conn.query_row("SELECT rec FROM states WHERE id=?1", params![state], |r| r.get(0)).optional()?;
        Ok(raw
            .and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok())
            .and_then(|v| v["layer_uuids"][name].as_str().or_else(|| v["cache_uuids"][name].as_str()).map(|s| s.to_string())))
    }

    pub fn trusted(&self, machine: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT public_key FROM trusted WHERE machine=?1", params![machine], |r| r.get(0)).optional()?)
    }

    pub fn state(&self, id: &str) -> Result<Option<StateRec>> {
        let raw: Option<String> =
            self.conn.query_row("SELECT rec FROM states WHERE id=?1", params![id], |r| r.get(0)).optional()?;
        Ok(match raw {
            Some(r) => Some(serde_json::from_str(&r)?),
            None => None,
        })
    }

    pub fn state_flags(&self, id: &str) -> Result<Option<(bool, bool)>> {
        Ok(self
            .conn
            .query_row("SELECT present, deleted FROM states WHERE id=?1", params![id], |r| {
                Ok((r.get::<_, i64>(0)? != 0, r.get::<_, i64>(1)? != 0))
            })
            .optional()?)
    }

    pub fn set_present(&self, id: &str, present: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE states SET present=?2, deleted=CASE WHEN ?2=1 THEN 0 ELSE deleted END WHERE id=?1",
            params![id, present as i64],
        )?;
        Ok(())
    }

    pub fn states_by_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT id FROM states WHERE display LIKE ?1 || '%' LIMIT 3")?;
        let v = st.query_map(params![prefix], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Users of this machine whose approval of the exact subject still stands.
    pub fn approvals(&self, state: &str) -> Result<Vec<(u32, i64)>> {
        let mut st = self.conn.prepare(
            "SELECT a.uid,MAX(json_extract(a.rec,'$.time')) FROM attestations a
            WHERE (a.local=1 OR EXISTS (SELECT 1 FROM approvals p WHERE p.id=a.id)) AND a.subject=?1 AND a.kind='approval'
            AND NOT EXISTS (SELECT 1 FROM records scoped WHERE scoped.id=a.id AND json_type(scoped.raw,'$.r.data.declassify') IS NOT NULL) AND NOT EXISTS
            (SELECT 1 FROM attestation_revocations r WHERE r.target=a.id AND r.machine=a.machine AND r.uid=a.uid AND r.ord>a.ord)
            GROUP BY a.machine,a.uid ORDER BY a.uid",
        )?;
        let v = st.query_map(params![state], |r| Ok((r.get::<_, i64>(0)? as u32, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// The machine that made a state.
    pub fn state_machine(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT machine FROM states WHERE id=?1", params![id], |r| r.get(0)).optional()?)
    }

    /// The state whose signed record names subvolume `uuid`.
    pub fn state_by_subvol(&self, uuid: &str) -> Result<Option<String>> {
        Ok(self
            .conn
            .query_row("SELECT id FROM states WHERE json_extract(rec, '$.subvol')=?1 LIMIT 1", params![uuid], |r| r.get(0))
            .optional()?)
    }

    pub fn present_states(&self) -> Result<Vec<(String, bool)>> {
        let mut st = self.conn.prepare("SELECT id, deleted FROM states WHERE present=1")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? != 0)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    fn line_row(r: &rusqlite::Row) -> rusqlite::Result<Line> {
        Ok(Line { id: r.get(0)?, name: r.get(1)?, owner: r.get::<_, i64>(2)? as u32, machine: r.get(3)?, head: r.get(4)? })
    }

    pub fn line_by_name(&self, name: &str) -> Result<Option<Line>> {
        Ok(self
            .conn
            .query_row(
                "SELECT id, name, owner, machine, head FROM lines WHERE name=?1 AND deleted=0 ORDER BY time DESC LIMIT 1",
                params![name],
                Self::line_row,
            )
            .optional()?)
    }

    pub fn line_by_id(&self, id: &str) -> Result<Option<Line>> {
        Ok(self
            .conn
            .query_row("SELECT id, name, owner, machine, head FROM lines WHERE id=?1", params![id], Self::line_row)
            .optional()?)
    }

    pub fn lines(&self) -> Result<Vec<Line>> {
        let mut st = self.conn.prepare("SELECT id, name, owner, machine, head FROM lines WHERE deleted=0 ORDER BY name")?;
        let v = st.query_map([], Self::line_row)?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn line_heads(&self, line: &str) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT machine, head FROM line_heads WHERE line=?1")?;
        let v = st
            .query_map(params![line], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// States of a kind for a line, newest first.
    pub fn line_states(&self, line: &str, kind: StateKind) -> Result<Vec<StateRec>> {
        let mut st = self.conn.prepare(
            "SELECT rec FROM states WHERE line=?1 AND kind=?2 AND deleted=0 AND dropped=0 ORDER BY time DESC, id DESC",
        )?;
        let v = st.query_map(params![line, kind_str(kind)], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|r| Ok(serde_json::from_str(&r)?)).collect()
    }

    pub fn tag(&self, name: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT state FROM tags WHERE name=?1 AND deleted=0", params![name], |r| r.get(0)).optional()?)
    }

    pub fn tags(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let mut st = self.conn.prepare("SELECT name, state, message FROM tags WHERE deleted=0 ORDER BY name")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn remote_ref(&self, name: &str) -> Result<Option<(String, Option<String>)>> {
        Ok(self
            .conn
            .query_row("SELECT state, gcommit FROM remote_refs WHERE name=?1", params![name], |r| Ok((r.get(0)?, r.get(1)?)))
            .optional()?)
    }

    pub fn remote_refs(&self) -> Result<Vec<(String, String, Option<String>)>> {
        let mut st = self.conn.prepare("SELECT name, state, gcommit FROM remote_refs ORDER BY name")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn git_commit_of(&self, state: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT gcommit FROM git_map WHERE state=?1 LIMIT 1", params![state], |r| r.get(0)).optional()?)
    }

    pub fn state_of_commit(&self, prefix: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT state FROM git_map WHERE gcommit LIKE ?1 || '%' LIMIT 3")?;
        let v = st.query_map(params![prefix], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn exports(&self) -> Result<Vec<(String, String, String, String, String, String)>> {
        let mut st = self.conn.prepare("SELECT id, state, remote, branch, gcommit, status FROM exports ORDER BY time")?;
        let v = st
            .query_map([], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Active value declassifications, across all state subjects in this project.
    pub fn declassifications(&self) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare("SELECT r.raw FROM attestations a JOIN records r ON r.id=a.id
            WHERE a.kind='approval' AND json_extract(r.raw,'$.r.data.declassify') IS NOT NULL
            AND NOT EXISTS (SELECT 1 FROM attestation_revocations v WHERE v.target=a.id AND v.machine=a.machine AND v.uid=a.uid AND v.ord>a.ord)")?;
        let raw = st.query_map([], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(|r| Ok(crate::oplog::parse_line(&r, None)?.record)).collect()
    }

    pub fn checks(&self, state: &str) -> Result<Vec<(String, i64, i64, String)>> {
        let mut st = self.conn.prepare("SELECT COALESCE(json_extract(r.raw,'$.r.data.check.command'),json_extract(r.raw,'$.r.data.command')),
            COALESCE(json_extract(r.raw,'$.r.data.check.exit'),json_extract(r.raw,'$.r.data.exit'),-1),r.time,json_extract(r.raw,'$.r.actor.name')
            FROM attestations a JOIN records r ON r.id=a.id WHERE a.subject=?1 AND a.kind='check'
            AND NOT EXISTS (SELECT 1 FROM attestation_revocations v WHERE v.target=a.id AND v.machine=a.machine AND v.uid=a.uid AND v.ord>a.ord)
            ORDER BY a.ord")?;
        let v = st
            .query_map(params![state], |r| {
                Ok((r.get::<_, Option<String>>(0)?.unwrap_or_default(), r.get(1)?, r.get(2)?, r.get(3)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Match command arguments, not their display text. With no requested exit, use the newest result.
    pub fn check_exit(&self, state: &str, argv: &[String], wanted_exit: Option<i64>) -> Result<Option<i64>> {
        let mut statement=self.conn.prepare("SELECT COALESCE(json_extract(r.raw,'$.r.data.check'),json_extract(r.raw,'$.r.data'))
            FROM attestations a JOIN records r ON r.id=a.id WHERE a.subject=?1 AND a.kind='check'
            AND NOT EXISTS (SELECT 1 FROM attestation_revocations v WHERE v.target=a.id AND v.machine=a.machine AND v.uid=a.uid AND v.ord>a.ord)
            ORDER BY a.ord DESC")?;
        let rows = statement.query_map(params![state], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        for row in rows {
            let result: serde_json::Value = serde_json::from_str(&row)?;
            let matches = match result.get("argv") {
                Some(value) => {
                    serde_json::from_value::<Vec<String>>(value.clone()).map(|recorded| recorded == argv).unwrap_or(false)
                }
                None => {
                    argv.len() == 1
                        && !argv[0].is_empty()
                        && !argv[0].chars().any(|c| c.is_whitespace())
                        && result["command"].as_str() == Some(argv[0].as_str())
                }
            };
            if matches {
                if let Some(code) = result["exit"].as_i64() {
                    if wanted_exit.map(|wanted| wanted == code).unwrap_or(true) {
                        return Ok(Some(code));
                    }
                }
            }
        }
        Ok(None)
    }

    fn derive_attestation(&self, rec: &Record, local: bool) -> Result<()> {
        let ord = format!("{:016}:{}:{:012}", rec.time.max(0), rec.machine, rec.seq);
        let a = if let Some(v) = rec.data.get("attestation") {
            Some(serde_json::from_value::<Attestation>(v.clone())?)
        } else if matches!(rec.op.as_str(), "approve" | "approve.withdraw" | "check") {
            rec.data["state"].as_str().map(|state| Attestation {
                format: 1,
                attestation_id: rec.id.clone(),
                subject: Subject { kind: "state".into(), id: state.into() },
                change_id: self.state(state).ok().flatten().and_then(|s| s.change_id),
                kind: match rec.op.as_str() {
                    "approve" => "approval",
                    "check" => "check",
                    _ => "revoke",
                }
                .into(),
                payload_digest: hex::encode(Sha256::digest(serde_json::to_vec(&rec.data).unwrap_or_default())),
                payload_ref: None,
                signer: rec.machine.clone(),
                time: rec.time,
            })
        } else {
            None
        };
        if let Some(a) = a {
            self.conn.execute(
                "INSERT INTO attestations(id,subject,kind,rec,machine,uid,ord,local) VALUES(?1,?2,?3,?4,?5,?6,?7,?8) ON CONFLICT(id) DO UPDATE SET local=MAX(attestations.local,excluded.local)",
                params![
                    a.attestation_id,
                    a.subject.id,
                    a.kind,
                    serde_json::to_string(&a)?,
                    rec.machine,
                    rec.actor.uid as i64,
                    ord,
                    local as i64
                ],
            )?;
            if rec.op == "approve.withdraw" {
                self.conn.execute("INSERT OR IGNORE INTO attestation_revocations(id,target,machine,uid,ord)
                    SELECT ?1,id,?2,?3,?4 FROM attestations WHERE subject=?5 AND kind='approval' AND machine=?2 AND uid=?3 AND ord<?4",
                    params![a.attestation_id,rec.machine,rec.actor.uid as i64,ord,a.subject.id])?;
            }
            if a.kind == "revoke" {
                for id in rec.data["revokes"].as_array().into_iter().flatten().filter_map(|v| v.as_str()) {
                    self.conn.execute(
                        "INSERT OR IGNORE INTO attestation_revocations(id,target,machine,uid,ord)
                         SELECT ?1,?2,?3,?4,?5 FROM attestations WHERE id=?2 AND subject=?6",
                        params![a.attestation_id, id, rec.machine, rec.actor.uid as i64, ord, a.subject.id],
                    )?;
                }
            }
        }
        Ok(())
    }

    pub fn attestation(&self, id: &str) -> Result<Option<Attestation>> {
        let raw: Option<String> =
            self.conn.query_row("SELECT rec FROM attestations WHERE id=?1", params![id], |r| r.get(0)).optional()?;
        raw.map(|r| Ok(serde_json::from_str(&r)?)).transpose()
    }

    pub fn attestations(&self, subject: &str, kind: Option<&str>) -> Result<Vec<Attestation>> {
        let mut st =
            self.conn.prepare("SELECT rec FROM attestations WHERE subject=?1 AND (?2 IS NULL OR kind=?2) ORDER BY ord,id")?;
        let raw = st.query_map(params![subject, kind], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(|r| Ok(serde_json::from_str(&r)?)).collect()
    }

    pub fn own_attestations(&self, subject: &str, machine: &str, uid: u32, kind: Option<&str>) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT id FROM attestations WHERE subject=?1 AND machine=?2 AND uid=?3 AND (?4 IS NULL OR kind=?4) AND kind<>'revoke'")?;
        let v = st.query_map(params![subject, machine, uid as i64, kind], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn refs(&self, prefix: &str, now: i64) -> Result<Vec<RefState>> {
        let mut st = self.conn.prepare("SELECT name,target,expires_at FROM refs WHERE deleted=0 AND substr(name,1,length(?1))=?1 AND (expires_at IS NULL OR expires_at>?2) ORDER BY name")?;
        let v = st
            .query_map(params![prefix, now], |r| Ok(RefState { name: r.get(0)?, target: r.get(1)?, expires_at: r.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn ref_value(&self, name: &str, now: i64) -> Result<Option<RefState>> {
        Ok(self.refs(name, now)?.into_iter().find(|r| r.name == name))
    }

    pub fn expired_refs(&self, now: i64) -> Result<Vec<RefState>> {
        let mut st =
            self.conn.prepare("SELECT name,target,expires_at FROM refs WHERE deleted=0 AND expires_at<=?1 ORDER BY name")?;
        let v = st
            .query_map(params![now], |r| Ok(RefState { name: r.get(0)?, target: r.get(1)?, expires_at: r.get(2)? }))?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn ref_log(&self, name: &str) -> Result<Vec<RefChange>> {
        let mut st = self.conn.prepare("SELECT rec FROM ref_history WHERE name=?1 ORDER BY ord DESC,id DESC")?;
        let raw = st.query_map(params![name], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(|r| Ok(serde_json::from_str(&r)?)).collect()
    }

    pub fn queue_entries(&self, target: Option<&str>) -> Result<Vec<QueueEntry>> {
        let mut st=self.conn.prepare("SELECT rec FROM queue_entries WHERE deleted=0 AND (?1 IS NULL OR json_extract(rec,'$.target_line')=?1) ORDER BY json_extract(rec,'$.time'),id")?;
        let rows = st.query_map(params![target], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        rows.into_iter().map(|raw| Ok(serde_json::from_str(&raw)?)).collect()
    }

    pub fn queue_entry(&self, id: &str) -> Result<Option<QueueEntry>> {
        let raw: Option<String> = self
            .conn
            .query_row("SELECT rec FROM queue_entries WHERE id=?1 AND deleted=0", params![id], |r| r.get(0))
            .optional()?;
        raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
    }

    pub fn change_versions(&self, id: &str) -> Result<Vec<StateRec>> {
        let mut st = self.conn.prepare("SELECT rec FROM states WHERE json_extract(rec,'$.change_id')=?1 ORDER BY time,id")?;
        let raw = st.query_map(params![id], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        raw.into_iter().map(|r| Ok(serde_json::from_str(&r)?)).collect()
    }

    pub fn changes(&self) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT DISTINCT json_extract(rec,'$.change_id') FROM states WHERE json_extract(rec,'$.change_id') IS NOT NULL ORDER BY 1")?;
        let v = st.query_map([], |r| r.get(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn config(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let v: Option<String> = self
            .conn
            .query_row("SELECT value FROM config WHERE key=?1", params![key], |r| r.get::<_, Option<String>>(0))
            .optional()?
            .flatten();
        Ok(match v {
            Some(s) => Some(serde_json::from_str(&s)?),
            None => None,
        })
    }

    pub fn config_all(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT key, value FROM config WHERE value IS NOT NULL ORDER BY key")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Overlay policy and metadata follow their signed records across machines.
    pub fn overlay_config(&self, key: &str) -> Result<Option<serde_json::Value>> {
        let value: Option<Option<String>> = self
            .conn
            .query_row(
                "SELECT value FROM config WHERE key=?1 OR substr(key,1,length(?1)+1)=?1||'@' ORDER BY ord DESC LIMIT 1",
                params![key],
                |row| row.get(0),
            )
            .optional()?;
        value.flatten().map(|value| serde_json::from_str(&value).map_err(Into::into)).transpose()
    }

    pub fn machine_key(&self, id: &str) -> Result<Option<String>> {
        Ok(self.conn.query_row("SELECT public_key FROM machines WHERE id=?1", params![id], |r| r.get(0)).optional()?)
    }

    /// A machine by its short id (the display form, random part first), full id or a unique name.
    pub fn machine_by_name(&self, name: &str) -> Result<Option<String>> {
        let all = self.machines()?;
        let by_id: Vec<&(String, String)> =
            all.iter().filter(|(id, _)| id == name || (name.len() >= 4 && ids::display(id).starts_with(name))).collect();
        if by_id.len() == 1 {
            return Ok(Some(by_id[0].0.clone()));
        }
        let by_name: Vec<&(String, String)> = all.iter().filter(|(_, n)| n == name).collect();
        if by_name.len() == 1 {
            return Ok(Some(by_name[0].0.clone()));
        }
        Ok(None)
    }

    pub fn machines(&self) -> Result<Vec<(String, String)>> {
        let mut st = self.conn.prepare("SELECT id, COALESCE(name, '') FROM machines ORDER BY id")?;
        let v = st.query_map([], |r| Ok((r.get(0)?, r.get(1)?)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn record(&self, id: &str) -> Result<Option<Record>> {
        let raw: Option<String> =
            self.conn.query_row("SELECT raw FROM records WHERE id=?1", params![id], |r| r.get(0)).optional()?;
        match raw {
            Some(l) => Ok(Some(crate::oplog::parse_line(&l, None)?.record)),
            None => Ok(None),
        }
    }

    /// Records newest first.
    pub fn records(&self, limit: usize) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare("SELECT raw FROM records ORDER BY time DESC, seq DESC LIMIT ?1")?;
        let v = st.query_map(params![limit as i64], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|l| Ok(crate::oplog::parse_line(&l, None)?.record)).collect()
    }

    pub fn records_since(&self, time: i64) -> Result<Vec<Record>> {
        let mut st = self.conn.prepare("SELECT raw FROM records WHERE time>=?1 ORDER BY time")?;
        let v = st.query_map(params![time], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        v.into_iter().map(|l| Ok(crate::oplog::parse_line(&l, None)?.record)).collect()
    }

    pub fn max_seqs(&self) -> Result<Vec<(String, u64)>> {
        let mut st = self.conn.prepare("SELECT machine, MAX(seq) FROM records GROUP BY machine")?;
        let v = st.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)? as u64)))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    pub fn undone(&self, id: &str) -> Result<bool> {
        let n: i64 = self.conn.query_row(
            "SELECT COUNT(*) FROM records WHERE op='undo' AND json_extract(raw, '$.r.data.undo')=?1",
            params![id],
            |r| r.get(0),
        )?;
        Ok(n > 0)
    }
}

/// Every setting acts on one machine (admin, members, merge commands, remotes, retention,
/// push policy, caches): settings never come from another machine's records. Those
/// values stay as `<key>@<machine>`.
pub fn local_key(_k: &str) -> bool {
    true
}

pub fn kind_str(k: StateKind) -> &'static str {
    match k {
        StateKind::Commit => "commit",
        StateKind::Auto => "auto",
        StateKind::Stash => "stash",
        StateKind::Import => "import",
        StateKind::Cache => "cache",
        StateKind::Derived => "derived",
    }
}

pub fn need<T>(v: Option<T>, what: &str) -> Result<T> {
    v.ok_or_else(|| anyhow!("{what} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, StateKind, StateRec};

    #[test]
    fn opening_a_database_that_init_is_creating_keeps_its_tables() {
        // The state between the schema and the version of a fresh database: an opener must not
        // take it for an old database and drop the derived tables under the creating process.
        let dir = std::env::temp_dir().join(format!("layr-db-creating-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layr.sqlite");
        let creating = Connection::open(&path).unwrap();
        creating.execute_batch(SCHEMA).unwrap();
        creating.execute("INSERT INTO states(id, display, kind) VALUES('s', 's', 'commit')", []).unwrap();
        let db = Db::open(&path).unwrap();
        assert!(!db.stale);
        let kept: i64 = db.conn.query_row("SELECT COUNT(*) FROM states", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 1);
        let version: i64 = db.conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
        assert_eq!(version, SCHEMA_VERSION);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_old_database_with_records_is_still_rebuilt() {
        let dir = std::env::temp_dir().join(format!("layr-db-old-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("layr.sqlite");
        let old = Connection::open(&path).unwrap();
        old.execute_batch(SCHEMA).unwrap();
        old.execute("INSERT INTO records(id, raw) VALUES('r', '{}')", []).unwrap();
        old.execute("INSERT INTO states(id, display, kind) VALUES('s', 's', 'commit')", []).unwrap();
        drop(old);
        let db = Db::open(&path).unwrap();
        assert!(db.stale);
        let kept: i64 = db.conn.query_row("SELECT COUNT(*) FROM states", [], |r| r.get(0)).unwrap();
        assert_eq!(kept, 0, "derived rows are rebuilt from the records");
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn state_record(seq: u64, state: &str, parents: Vec<String>) -> Record {
        let mut r = Record::new("commit");
        r.id = format!("01a12290-0000-7000-8000-{:012}", seq);
        r.machine = "01a12290-0000-7000-8000-00000000aaaa".into();
        r.seq = seq;
        r.time = seq as i64;
        r.states.push(StateRec {
            id: state.into(),
            kind: StateKind::Commit,
            change_id: None,
            derived_from: None,
            context: None,
            parents,
            line: None,
            subvol: String::new(),
            author: Author::default(),
            message: String::new(),
            time: 0,
            running: false,
            conflicts: vec![],
            caches: Default::default(),
            cache_by: None,
            cache_shared: false,
            cache_platform: None,
        });
        r
    }

    #[test]
    fn restored_states_clear_local_gc_tombstones() {
        let path = std::env::temp_dir().join(format!("layr-db-restore-{}", crate::ids::new_id()));
        let db = Db::open(&path).unwrap();
        let state = "01a12290-0000-7000-8000-00000000c0c0";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let creation = state_record(1, state, vec![]);
        let mut gc = Record::new("gc");
        gc.machine = machine.into();
        gc.data = serde_json::json!({"deleted": [state]});
        db.derive(&creation, &|_| true, machine).unwrap();
        db.derive(&gc, &|_| false, machine).unwrap();
        assert_eq!(db.state_flags(state).unwrap(), Some((false, true)));
        db.set_present(state, true).unwrap();
        assert_eq!(db.state_flags(state).unwrap(), Some((true, false)));
        // Reindex replays an older gc record against the snapshots that exist now.
        db.derive(&gc, &|_| true, machine).unwrap();
        assert_eq!(db.state_flags(state).unwrap(), Some((true, false)));
        // Another machine's gc never deletes the local copy.
        gc.machine = "01a12290-0000-7000-8000-00000000bbbb".into();
        db.derive(&gc, &|_| false, machine).unwrap();
        assert_eq!(db.state_flags(state).unwrap(), Some((true, false)));
        drop(db);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn records_set_the_stack_of_a_line() {
        let dir = std::env::temp_dir().join(format!("layr-db-stack-{}", std::process::id()));
        let db = Db::open(&dir).unwrap();
        let line = "01a12290-0000-7000-8000-00000000eeee";
        let layers = |s: &str| serde_json::json!([{"overlay": "keys", "state": s}]);
        let rec = |seq: u64, time: i64, machine: &str, st: &str| {
            let mut r = Record::new("commit");
            r.id = format!("01a12290-0000-7000-8000-{:012}", seq);
            r.machine = machine.into();
            r.seq = seq;
            r.time = time;
            r.data = serde_json::json!({"line": line, "stack": {"line": line, "layers": layers(st)}});
            r
        };
        let (a, b) = ("01a12290-0000-7000-8000-000000000a01", "01a12290-0000-7000-8000-000000000a02");
        let local = "01a12290-0000-7000-8000-00000000aaaa";
        db.derive(&rec(2, 20, local, b), &|_| true, local).unwrap();
        // An older record that arrives later does not replace the newer stack.
        db.derive(&rec(1, 10, local, a), &|_| true, local).unwrap();
        assert_eq!(db.config(&format!("stack.{line}")).unwrap(), Some(layers(b)));
        // Another machine's stacks stay apart.
        let other = "01a12290-0000-7000-8000-00000000bbbb";
        db.derive(&rec(3, 30, other, a), &|_| true, local).unwrap();
        assert_eq!(db.config(&format!("stack.{line}")).unwrap(), Some(layers(b)));
        assert_eq!(db.config(&format!("stack.{line}@{other}")).unwrap(), Some(layers(a)));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn records_of_caches_add_to_the_caches_of_a_state() {
        let dir = std::env::temp_dir().join(format!("layr-db-caches-{}", std::process::id()));
        let db = Db::open(&dir).unwrap();
        let st = "01a12290-0000-7000-8000-00000000c0c0";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        db.derive(&state_record(1, st, vec![]), &|_| true, machine).unwrap();
        let caches = |seq: u64, path: &str, uuid: &str| {
            let mut r = Record::new("save.caches");
            r.id = format!("01a12290-0000-7000-8000-{:012}", seq);
            r.machine = machine.into();
            r.seq = seq;
            r.time = seq as i64;
            let snap = format!("{st}@{path}");
            r.data = serde_json::json!({"state": st, "caches": {path: snap}, "uuids": {snap: uuid}, "shared": true});
            r
        };
        // Two saves of caches on the same state, one for each folder.
        db.derive(&caches(2, "target", "01a12290-0000-7000-8000-000000000001"), &|_| true, machine).unwrap();
        db.derive(&caches(3, ".cache", "01a12290-0000-7000-8000-000000000002"), &|_| true, machine).unwrap();
        let s = db.state(st).unwrap().unwrap();
        assert_eq!(s.caches.keys().map(|k| k.as_str()).collect::<Vec<_>>(), [".cache", "target"]);
        assert!(s.cache_shared);
        for (path, uuid) in
            [("target", "01a12290-0000-7000-8000-000000000001"), (".cache", "01a12290-0000-7000-8000-000000000002")]
        {
            assert_eq!(db.cache_uuid(st, &format!("{st}@{path}")).unwrap().as_deref(), Some(uuid));
        }
        // Records of an older layr name no platform.
        assert_eq!(s.cache_platform, None);
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn a_state_keeps_the_platform_of_its_caches() {
        let dir = std::env::temp_dir().join(format!("layr-db-platform-{}", std::process::id()));
        let db = Db::open(&dir).unwrap();
        let st = "01a12290-0000-7000-8000-00000000c1c1";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        db.derive(&state_record(1, st, vec![]), &|_| true, machine).unwrap();
        let mut r = Record::new("save.layers");
        r.id = "01a12290-0000-7000-8000-000000000002".into();
        r.machine = machine.into();
        r.seq = 2;
        r.time = 2;
        let snap = format!("{st}@target");
        r.data = serde_json::json!({"state": st, "layers": {"target": snap}, "uuids": {snap: "01a12290-0000-7000-8000-000000000003"}, "shared": false, "platform": "macos-aarch64"});
        db.derive(&r, &|_| true, machine).unwrap();
        assert_eq!(db.state(st).unwrap().unwrap().cache_platform.as_deref(), Some("macos-aarch64"));
        let _ = std::fs::remove_file(&dir);
    }

    #[test]
    fn a_state_id_cannot_be_named_twice() {
        let dir = std::env::temp_dir().join(format!("layr-db-test-{}", std::process::id()));
        let db = Db::open(&dir).unwrap();
        let st = "01a12290-0000-7000-8000-00000000bbbb";
        let parent = "01a12290-0000-7000-8000-00000000cccc".to_string();
        db.derive(&state_record(1, st, vec![parent.clone()]), &|_| true, "x").unwrap();
        let other = "01a12290-0000-7000-8000-00000000dddd".to_string();
        assert!(db.derive(&state_record(2, st, vec![other]), &|_| true, "x").is_err());
        assert_eq!(db.state(st).unwrap().unwrap().parents, vec![parent]);
        let _ = std::fs::remove_file(&dir);
    }
    fn memory() -> Db {
        let conn = Connection::open_in_memory().unwrap();
        conn.execute_batch(SCHEMA).unwrap();
        Db { conn, stale: false }
    }

    #[test]
    fn refs_replay_in_order_and_keep_their_history() {
        let db = memory();
        let a = "01a12290-0000-7000-8000-000000000003";
        let b = "01a12290-0000-7000-8000-000000000004";
        let rec = |n: u64, target: &str| {
            let mut r = state_record(n, a, vec![]);
            r.states.clear();
            r.op = "ref.set".into();
            r.data = serde_json::json!({"ref_change":RefChange { name:"env/prod".into(),before:None,after:Some(RefState{name:"env/prod".into(),target:target.into(),expires_at:Some(100)}) }});
            r
        };
        db.derive(&rec(2, b), &|_| true, "local").unwrap();
        db.derive(&rec(1, a), &|_| true, "local").unwrap();
        assert_eq!(db.ref_value("env/prod", 99).unwrap().unwrap().target, b);
        assert_eq!(db.ref_log("env/prod").unwrap().len(), 2);
        assert!(db.refs("env/", 100).unwrap().is_empty());
        assert_eq!(db.expired_refs(100).unwrap().len(), 1);
    }

    #[test]
    fn legacy_approvals_checks_and_withdrawals_become_attestations() {
        let db = memory();
        let state = "01a12290-0000-7000-8000-000000000003";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        for (seq, op) in [(1, "approve"), (2, "check"), (3, "approve.withdraw"), (4, "approve")] {
            let mut r = state_record(seq, state, vec![]);
            r.states.clear();
            r.op = op.into();
            r.data = serde_json::json!({"state":state,"command":"true","exit":0});
            let raw = crate::oplog::sign_line(&r, &key).unwrap();
            db.insert_record(&r, &raw).unwrap();
            db.derive(&r, &|_| true, machine).unwrap();
            if seq == 3 {
                assert!(db.approvals(state).unwrap().is_empty());
            }
        }
        assert_eq!(db.attestations(state, None).unwrap().len(), 4);
        assert_eq!(db.approvals(state).unwrap(), vec![(0, 4)]);
        assert_eq!(db.checks(state).unwrap()[0].0, "true");
        // The original signed bytes are unchanged.
        assert!(crate::oplog::parse_line(&db.record_line(machine, 2).unwrap().unwrap(), Some(&key.verifying_key())).is_ok());
    }

    #[test]
    fn a_revoke_does_not_withdraw_another_signers_evidence() {
        let db = memory();
        let state = "01a12290-0000-7000-8000-000000000003";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let mut first = state_record(1, state, vec![]);
        first.states.clear();
        first.op = "approve".into();
        first.data = serde_json::json!({"state":state});
        db.derive(&first, &|_| true, machine).unwrap();
        let mut other = first.clone();
        other.id = "01a12290-0000-7000-8000-000000000002".into();
        other.seq = 2;
        other.op = "attest".into();
        other.time = 2;
        other.actor.uid = 99;
        other.data = serde_json::json!({"attestation":Attestation {format:1,attestation_id:other.id.clone(),subject:Subject{kind:"state".into(),id:state.into()},change_id:None,kind:"revoke".into(),payload_digest:"0".repeat(64),payload_ref:None,signer:machine.into(),time:2},"revokes":[first.id]});
        db.derive(&other, &|_| true, machine).unwrap();
        assert_eq!(db.approvals(state).unwrap(), vec![(0, 1)]);
    }
    #[test]
    fn scoped_declassification_does_not_count_as_subject_approval() {
        let db = memory();
        let state = "01a12290-0000-7000-8000-000000000003";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let mut rec = state_record(1, state, vec![]);
        rec.states.clear();
        rec.op = "attest".into();
        rec.data = serde_json::json!({"attestation":Attestation {format:1,attestation_id:rec.id.clone(),subject:Subject{kind:"state".into(),id:state.into()},change_id:None,kind:"approval".into(),payload_digest:"0".repeat(64),payload_ref:None,signer:machine.into(),time:1},"declassify":{"path":"a:1","hmac":"digest"}});
        let raw = crate::oplog::sign_line(&rec, &key).unwrap();
        db.insert_record(&rec, &raw).unwrap();
        db.derive(&rec, &|_| true, machine).unwrap();
        assert!(db.approvals(state).unwrap().is_empty());
        assert_eq!(db.conn.query_row("SELECT COUNT(*) FROM approvals", [], |r| r.get::<_, i64>(0)).unwrap(), 0);
        assert_eq!(db.attestations(state, Some("approval")).unwrap().len(), 1);
    }

    #[test]
    fn queue_status_replay_is_order_independent() {
        let db = memory();
        let id = "01a12290-0000-7000-8000-000000000003";
        let entry = QueueEntry {
            id: id.into(),
            into: "main".into(),
            target_line: "01a12290-0000-7000-8000-000000000004".into(),
            state: "01a12290-0000-7000-8000-000000000005".into(),
            change_id: None,
            added_by: 0,
            time: 1,
            status: "pending".into(),
            attempts: 0,
            result: None,
            reason: None,
            probes: None,
        };
        let event = |seq, status: Option<&str>| {
            let mut rec = state_record(seq, id, vec![]);
            rec.states.clear();
            rec.op = "queue.result".into();
            let after = status.map(|status| {
                let mut entry = entry.clone();
                entry.status = status.into();
                entry
            });
            rec.data = serde_json::json!({"queue_change":QueueChange{id:id.into(),before:None,after}});
            rec
        };
        db.derive(&event(3, Some("done")), &|_| true, "local").unwrap();
        db.derive(&event(1, Some("pending")), &|_| true, "local").unwrap();
        assert_eq!(db.queue_entry(id).unwrap().unwrap().status, "done");
        db.derive(&event(4, None), &|_| true, "local").unwrap();
        assert!(db.queue_entry(id).unwrap().is_none());
        db.derive(&event(5, Some("pending")), &|_| true, "local").unwrap();
        assert_eq!(db.queue_entries(Some(&entry.target_line)).unwrap().len(), 1);
    }
    #[test]
    fn check_reuse_preserves_argument_boundaries() {
        let db = memory();
        let state = "01a12290-0000-7000-8000-000000000003";
        let machine = "01a12290-0000-7000-8000-00000000aaaa";
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let insert = |seq, data: serde_json::Value| {
            let mut rec = state_record(seq, state, vec![]);
            rec.states.clear();
            rec.op = "check".into();
            rec.data = data;
            let raw = crate::oplog::sign_line(&rec, &key).unwrap();
            db.insert_record(&rec, &raw).unwrap();
            db.derive(&rec, &|_| true, machine).unwrap();
        };
        insert(1, serde_json::json!({"state":state,"command":"sh -c exit 1","argv":["sh","-c","exit 1"],"exit":1}));
        assert_eq!(db.check_exit(state, &["sh".into(), "-c".into(), "exit 1".into()], None).unwrap(), Some(1));
        assert_eq!(db.check_exit(state, &["sh".into(), "-c".into(), "exit".into(), "1".into()], None).unwrap(), None);
        insert(2, serde_json::json!({"state":state,"command":"sh -c true","exit":0}));
        assert_eq!(db.check_exit(state, &["sh".into(), "-c".into(), "true".into()], None).unwrap(), None);
        insert(3, serde_json::json!({"state":state,"command":"true","exit":0}));
        assert_eq!(db.check_exit(state, &["true".into()], None).unwrap(), Some(0));
        insert(4, serde_json::json!({"state":state,"command":"true","argv":["true"],"exit":1}));
        assert_eq!(db.check_exit(state, &["true".into()], None).unwrap(), Some(1));
        assert_eq!(db.check_exit(state, &["true".into()], Some(0)).unwrap(), Some(0));
    }
}

// Addressable view descriptors stay in their signed records; no derived schema is needed.
impl Db {
    pub fn saved_view(&self, id: &str) -> Result<Option<crate::overlay::View>> {
        let raw: Option<String> = self
            .conn
            .query_row(
                "SELECT json_extract(raw,'$.r.data.view') FROM records WHERE op='view.save'
             AND json_extract(raw,'$.r.data.view_id')=?1 ORDER BY time,seq LIMIT 1",
                params![id],
                |row| row.get(0),
            )
            .optional()?;
        raw.map(|raw| Ok(serde_json::from_str(&raw)?)).transpose()
    }
}

#[cfg(test)]
mod view_tests {
    use super::*;

    #[test]
    fn saved_descriptors_and_checks_accept_view_subjects() -> Result<()> {
        let path = std::env::temp_dir().join(format!("layr-view-db-{}", crate::ids::new_id()));
        std::fs::create_dir(&path)?;
        let db = Db::open(&path.join("test.sqlite"))?;
        let view = crate::overlay::View::new("01a12290-0000-7000-8000-000000000003".into(), Vec::new());
        let subject = view.id()?;
        let raw = serde_json::json!({"r":{"data":{"view_id":subject,"view":view}}});
        db.conn.execute("INSERT INTO records(id,op,raw,time,seq) VALUES('view','view.save',?1,1,1)", params![raw.to_string()])?;
        assert_eq!(db.saved_view(&subject)?, Some(view));
        let argv = vec!["sh".to_string(), "-c".into(), "check argument".into()];
        for (id, code, ord) in [("first", 7, "1"), ("second", 0, "2")] {
            let raw = serde_json::json!({"r":{"data":{"check":{"argv":argv,"command":"sh -c check argument","exit":code}}}});
            db.conn.execute("INSERT INTO records(id,op,raw) VALUES(?1,'attest',?2)", params![id, raw.to_string()])?;
            db.conn.execute(
                "INSERT INTO attestations(id,subject,kind,machine,uid,ord) VALUES(?1,?2,'check','machine',1,?3)",
                params![id, subject, ord],
            )?;
        }
        assert_eq!(db.check_exit(&subject, &argv, None)?, Some(0));
        assert_eq!(db.check_exit(&subject, &argv, Some(7))?, Some(7));
        assert_eq!(db.check_exit(&subject, &["sh -c check argument".into()], None)?, None);
        assert_eq!(db.check_exit(&"0".repeat(64), &argv, None)?, None);
        db.conn.execute(
            "INSERT INTO attestation_revocations(id,target,machine,uid,ord) VALUES('revoke','second','machine',1,'3')",
            [],
        )?;
        assert_eq!(db.check_exit(&subject, &argv, None)?, Some(7));
        drop(db);
        std::fs::remove_dir_all(path)?;
        Ok(())
    }
}
