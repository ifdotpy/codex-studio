//! Records of the operation log and the objects they describe.

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Author {
    pub name: String,
    pub email: String,
    #[serde(default)]
    pub uid: u32,
    /// The label of the session that made it (a terminal, a CI job, a tool), from
    /// `LAYR_SESSION`. Free text: layr shows it and never decides anything from it.
    #[serde(default, alias = "agent", skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Conflict {
    pub path: String,
    pub kind: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub base: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ours: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub theirs: Option<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum StateKind {
    /// A commit: a named state with a message.
    Commit,
    /// An automatic snapshot of a line's working content (an undo point).
    Auto,
    /// Saved work of `layr stash`.
    Stash,
    /// A state made from a git commit of the remote.
    Import,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRec {
    pub id: String,
    pub kind: StateKind,
    #[serde(default)]
    pub parents: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<String>,
    /// btrfs UUID of the snapshot on the machine that made it.
    pub subvol: String,
    pub author: Author,
    #[serde(default)]
    pub message: String,
    pub time: i64,
    /// Taken while processes ran in the line: crash-consistent only.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub running: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<Conflict>,
    /// Snapshots of regenerable layers: layer path to snapshot name in `states/`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub layers: BTreeMap<String, String>,
    /// The user who made the layer snapshots (a uid of the state's machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub layer_by: Option<u32>,
    /// The layers may go to lines of other users: they were made on a protected line (by its
    /// checks or a save there). Other layers go only to lines of the user who made them, because
    /// a layer holds code that review does not see.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub layer_shared: bool,
}

/// The pointers of a line at one time.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineState {
    pub name: String,
    pub head: String,
    pub owner: u32,
    pub machine: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LineChange {
    pub before: Option<LineState>,
    pub after: Option<LineState>,
    /// Automatic states of the working content before and after the operation, when the
    /// operation changed the working content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub working_after: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Actor {
    pub uid: u32,
    pub name: String,
    /// See `Author::session`.
    #[serde(default, alias = "agent", skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    pub machine: String,
    pub seq: u64,
    pub time: i64,
    pub op: String,
    pub project: String,
    pub actor: Actor,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub states: Vec<StateRec>,
    /// Line id to its pointer change.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub lines: BTreeMap<String, LineChange>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub data: serde_json::Value,
}

impl Record {
    pub fn new(op: &str) -> Record {
        Record {
            id: String::new(),
            machine: String::new(),
            seq: 0,
            time: 0,
            op: op.to_string(),
            project: String::new(),
            actor: Actor { uid: 0, name: String::new(), session: None },
            states: Vec::new(),
            lines: BTreeMap::new(),
            data: serde_json::Value::Null,
        }
    }
}

#[derive(Debug, Clone)]
pub struct Line {
    pub id: String,
    pub name: String,
    pub head: String,
    pub owner: u32,
    pub machine: String,
}

// ----- checks of records that come from other machines -----

pub fn is_uuid(s: &str) -> bool {
    s.len() == 36 && uuid::Uuid::parse_str(s).map(|u| u.hyphenated().to_string() == s).unwrap_or(false)
}

pub fn is_name(s: &str) -> bool {
    crate::store::check_name("name", s).is_ok()
}

/// A layer snapshot name: `<state-id>@<path with '/' as %2F>`.
pub fn is_layer_name(s: &str, state: &str) -> bool {
    match s.split_once('@') {
        Some((id, p)) => {
            id == state
                && !p.is_empty()
                && p.chars().all(|c| c.is_ascii_alphanumeric() || "._-%".contains(c))
                && !p.starts_with('.')
        }
        None => false,
    }
}

fn is_ref(s: &str) -> bool {
    !s.is_empty()
        && !s.contains("..")
        && !s.starts_with('/')
        && s.chars().all(|c| c.is_ascii_alphanumeric() || "._/-".contains(c))
}

/// Every id, name and path in a record is checked before the record is stored: ids are
/// canonical UUIDs, names follow the name rules, paths are relative. A record from another
/// machine can then never name a path outside the project.
pub fn validate(r: &Record) -> anyhow::Result<()> {
    use anyhow::bail;
    let uuid = |what: &str, v: &str| -> anyhow::Result<()> {
        if !is_uuid(v) {
            bail!("record {}: {what} is not an id: {v:?}", r.seq);
        }
        Ok(())
    };
    uuid("id", &r.id)?;
    uuid("machine", &r.machine)?;
    if !r.project.is_empty() {
        uuid("project", &r.project)?;
    }
    for s in &r.states {
        uuid("state", &s.id)?;
        for p in &s.parents {
            uuid("parent", p)?;
        }
        if let Some(l) = &s.line {
            uuid("line", l)?;
        }
        for (path, snap) in &s.layers {
            crate::fsutil::check_rel(path)?;
            if !is_layer_name(snap, &s.id) {
                bail!("record {}: bad layer name {snap:?}", r.seq);
            }
        }
        for c in &s.conflicts {
            crate::fsutil::check_rel(&c.path)?;
            for x in [&c.base, &c.ours, &c.theirs].into_iter().flatten() {
                uuid("conflict state", x)?;
            }
        }
    }
    for (id, ch) in &r.lines {
        uuid("line", id)?;
        for st in [&ch.before, &ch.after].into_iter().flatten() {
            if !is_name(&st.name) {
                bail!("record {}: bad line name {:?}", r.seq, st.name);
            }
            uuid("head", &st.head)?;
            uuid("line machine", &st.machine)?;
        }
        for x in [&ch.working_before, &ch.working_after].into_iter().flatten() {
            uuid("working state", x)?;
        }
    }
    let d = &r.data;
    for k in ["state", "line", "start", "final_state", "undo", "base", "theirs", "onto", "upstream", "source", "export", "id"] {
        if let Some(v) = d.get(k).and_then(|v| v.as_str()) {
            uuid(k, v)?;
        }
    }
    if let Some(v) = d.get("machine").and_then(|v| v.as_str()) {
        uuid("machine", v)?;
    }
    for k in ["name", "from", "to", "remote"] {
        if let Some(v) = d.get(k).and_then(|v| v.as_str()) {
            if !is_name(v) {
                bail!("record {}: bad {k} {v:?}", r.seq);
            }
        }
    }
    for k in ["ref", "branch"] {
        if let Some(v) = d.get(k).and_then(|v| v.as_str()) {
            if !is_ref(v) {
                bail!("record {}: bad {k} {v:?}", r.seq);
            }
        }
    }
    if let Some(a) = d.get("deleted").and_then(|v| v.as_array()) {
        for v in a {
            uuid("deleted state", v.as_str().unwrap_or(""))?;
        }
    }
    if let (Some(st), Some(l)) = (d.get("state").and_then(|v| v.as_str()), d.get("layers").and_then(|v| v.as_object())) {
        for (path, snap) in l {
            crate::fsutil::check_rel(path)?;
            if !is_layer_name(snap.as_str().unwrap_or(""), st) {
                bail!("record {}: bad layer name", r.seq);
            }
        }
    }
    if let (Some(st), Some(u)) = (d.get("state").and_then(|v| v.as_str()), d.get("uuids").and_then(|v| v.as_object())) {
        for (snap, id) in u {
            if !is_layer_name(snap, st) {
                bail!("record {}: bad layer name", r.seq);
            }
            uuid("layer subvolume", id.as_str().unwrap_or(""))?;
        }
    }
    if let Some(c) = d.get("config").and_then(|v| v.as_object()) {
        for k in c.keys() {
            if !k.chars().all(|ch| ch.is_ascii_alphanumeric() || "._-@*".contains(ch)) {
                bail!("record {}: bad setting name {k:?}", r.seq);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rec() -> Record {
        let mut r = Record::new("commit");
        r.id = "01a12290-0000-7000-8000-000000000001".into();
        r.machine = "01a12290-0000-7000-8000-000000000002".into();
        r.seq = 2;
        r
    }

    fn state(id: &str) -> StateRec {
        StateRec {
            id: id.into(),
            kind: StateKind::Commit,
            parents: vec![],
            line: None,
            subvol: String::new(),
            author: Author::default(),
            message: String::new(),
            time: 0,
            running: false,
            conflicts: vec![],
            layers: BTreeMap::new(),
            layer_by: None,
            layer_shared: false,
        }
    }

    #[test]
    fn reads_the_session_label_of_older_records() {
        let a: Actor = serde_json::from_str(r#"{"uid":1,"name":"u","agent":"old label"}"#).unwrap();
        assert_eq!(a.session.as_deref(), Some("old label"));
        assert!(serde_json::to_string(&a).unwrap().contains(r#""session":"old label""#));
    }

    #[test]
    fn accepts_a_normal_record() {
        let mut r = rec();
        r.states.push(state("01a12290-0000-7000-8000-000000000003"));
        assert!(validate(&r).is_ok());
    }

    #[test]
    fn refuses_paths_as_ids_and_names() {
        let mut r = rec();
        r.states.push(state("/etc"));
        assert!(validate(&r).is_err());
        let mut r = rec();
        let mut s = state("01a12290-0000-7000-8000-000000000003");
        s.layers.insert("target".into(), "../../etc".into());
        r.states.push(s);
        assert!(validate(&r).is_err());
        let mut r = rec();
        r.lines.insert(
            "01a12290-0000-7000-8000-000000000004".into(),
            LineChange {
                before: None,
                after: Some(LineState {
                    name: "../main".into(),
                    head: "01a12290-0000-7000-8000-000000000003".into(),
                    owner: 0,
                    machine: "01a12290-0000-7000-8000-000000000002".into(),
                }),
                working_before: None,
                working_after: None,
            },
        );
        assert!(validate(&r).is_err());
        let mut r = rec();
        r.data = serde_json::json!({"deleted": ["/"]});
        assert!(validate(&r).is_err());
        let mut r = rec();
        r.data = serde_json::json!({"state": "01A12290-0000-7000-8000-000000000003"});
        assert!(validate(&r).is_err(), "ids must be canonical (lower case)");
    }
}
