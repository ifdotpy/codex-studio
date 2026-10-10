//! The project database (SQLite). The `records` table holds the signed, immutable operation
//! records: the source of truth. All other tables are derived from the records. A record and
//! its derived rows are written in one transaction, so they cannot disagree after a crash. The
//! derived tables can be rebuilt from the records at any time.

use crate::ids;
use crate::model::{Line, Record, StateKind, StateRec};
use anyhow::{anyhow, Result};
use rusqlite::{params, Connection, OptionalExtension};
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
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        let has_records: bool =
            conn.query_row("SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name='records'", [], |r| {
                r.get::<_, i64>(0)
            })? > 0;
        let mut db = Db { conn, stale: false };
        if version < SCHEMA_VERSION {
            if has_records {
                // The project rebuilds the derived tables and only then writes the version
                // (`mark_current`): a failed rebuild is tried again on the next open.
                for t in DERIVED.iter().chain(RETIRED) {
                    db.conn.execute_batch(&format!("DROP TABLE IF EXISTS {t}"))?;
                }
                db.stale = true;
                db.conn.execute_batch(SCHEMA)?;
            } else {
                db.conn.execute_batch(SCHEMA)?;
                db.conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
            }
        } else if version > SCHEMA_VERSION {
            anyhow::bail!("{} was written by a newer layr (schema {version})", path.display());
        } else {
            db.conn.execute_batch(SCHEMA)?;
        }
        Ok(db)
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
             DELETE FROM trusted; DELETE FROM approvals;",
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
    /// members, merge commands, remotes, the backup folder) and the lines that have their
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
            for (i, p) in s.parents.iter().enumerate() {
                tx.execute("INSERT OR IGNORE INTO parents(state, parent, ord) VALUES(?1,?2,?3)", params![s.id, p, i as i64])?;
            }
        }
        for (id, ch) in &rec.lines {
            // Heads per machine: each machine's own records, in its order.
            if let Some(a) = &ch.after {
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
            "project" | "config" => {
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
                        if !foreign {
                            tx.execute("UPDATE states SET deleted=1, present=0 WHERE id=?1", params![id])?;
                        }
                    }
                }
            }
            "save.layers" => {
                // Only the machine that made the state adds its layers (names and subvolume
                // UUIDs, which a receiver checks).
                if let Some(st) = s("state") {
                    let row: Option<(String, String)> = tx
                        .query_row("SELECT rec, machine FROM states WHERE id=?1", params![st], |r| Ok((r.get(0)?, r.get(1)?)))
                        .optional()?;
                    if let Some((raw, machine)) = row {
                        if machine == rec.machine {
                            let mut v: serde_json::Value = serde_json::from_str(&raw)?;
                            v["layers"] = d.get("layers").cloned().unwrap_or_default();
                            v["layer_uuids"] = d.get("uuids").cloned().unwrap_or_default();
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
        Ok(())
    }

    /// The subvolume UUID of a layer snapshot, from the signed save.layers record.
    pub fn layer_uuid(&self, state: &str, name: &str) -> Result<Option<String>> {
        let raw: Option<String> =
            self.conn.query_row("SELECT rec FROM states WHERE id=?1", params![state], |r| r.get(0)).optional()?;
        Ok(raw
            .and_then(|r| serde_json::from_str::<serde_json::Value>(&r).ok())
            .and_then(|v| v["layer_uuids"][name].as_str().map(|s| s.to_string())))
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
        self.conn.execute("UPDATE states SET present=?2 WHERE id=?1", params![id, present as i64])?;
        Ok(())
    }

    pub fn states_by_prefix(&self, prefix: &str) -> Result<Vec<String>> {
        let mut st = self.conn.prepare("SELECT id FROM states WHERE display LIKE ?1 || '%' LIMIT 3")?;
        let v = st.query_map(params![prefix], |r| r.get::<_, String>(0))?.collect::<Result<Vec<_>, _>>()?;
        Ok(v)
    }

    /// Users (uids of this machine) whose approval of `state` stands, with its time.
    pub fn approvals(&self, state: &str) -> Result<Vec<(u32, i64)>> {
        let mut st =
            self.conn.prepare("SELECT uid, MAX(time) FROM approvals WHERE state=?1 AND withdrawn=0 GROUP BY uid ORDER BY uid")?;
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

    pub fn checks(&self, state: &str) -> Result<Vec<(String, i64, i64, String)>> {
        let mut st = self.conn.prepare("SELECT command, exit, time, actor FROM checks WHERE state=?1 ORDER BY time")?;
        let v = st
            .query_map(params![state], |r| Ok((r.get(0)?, r.get::<_, Option<i64>>(1)?.unwrap_or(-1), r.get(2)?, r.get(3)?)))?
            .collect::<Result<Vec<_>, _>>()?;
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
/// push policy, layers, backups): settings never come from another machine's records. Those
/// values stay as `<key>@<machine>`; a restore takes over the values of the machine that wrote
/// the backup.
pub fn local_key(_k: &str) -> bool {
    true
}

pub fn kind_str(k: StateKind) -> &'static str {
    match k {
        StateKind::Commit => "commit",
        StateKind::Auto => "auto",
        StateKind::Stash => "stash",
        StateKind::Import => "import",
    }
}

pub fn need<T>(v: Option<T>, what: &str) -> Result<T> {
    v.ok_or_else(|| anyhow!("{what} not found"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{Author, StateKind, StateRec};

    fn state_record(seq: u64, state: &str, parents: Vec<String>) -> Record {
        let mut r = Record::new("commit");
        r.id = format!("01a12290-0000-7000-8000-{:012}", seq);
        r.machine = "01a12290-0000-7000-8000-00000000aaaa".into();
        r.seq = seq;
        r.time = seq as i64;
        r.states.push(StateRec {
            id: state.into(),
            kind: StateKind::Commit,
            parents,
            line: None,
            subvol: String::new(),
            author: Author::default(),
            message: String::new(),
            time: 0,
            running: false,
            conflicts: vec![],
            layers: Default::default(),
            layer_by: None,
            layer_shared: false,
        });
        r
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
}
