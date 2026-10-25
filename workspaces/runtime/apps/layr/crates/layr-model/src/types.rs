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
    /// A named state with a message.
    Commit,
    /// An automatic snapshot of a line's working content (an undo point).
    Auto,
    /// Saved work of `layr park`.
    Stash,
    /// A state made from a git commit of the remote.
    Import,
    /// Reusable data outside source history.
    Cache,
    /// Output of a computation outside source history.
    Derived,
}

impl StateKind {
    pub fn auxiliary(self) -> bool {
        matches!(self, Self::Cache | Self::Derived)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateRec {
    pub id: String,
    pub kind: StateKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derived_from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context: Option<String>,
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
    /// Snapshots of regenerable caches: cache path to snapshot name in `states/`.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    #[serde(rename = "layers", alias = "caches")]
    pub caches: BTreeMap<String, String>,
    /// The user who made the cache snapshots (a uid of the state's machine).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "layer_by", alias = "cache_by")]
    pub cache_by: Option<u32>,
    /// The caches may go to lines of other users: they were saved on a protected line by someone
    /// who may save there. Other caches go only to lines of the user who made them, because a
    /// cache holds code that review does not see.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    #[serde(rename = "layer_shared", alias = "cache_shared")]
    pub cache_shared: bool,
    /// The platform the cache snapshots were made on (`cache_platform()`). Build output of one
    /// operating system or processor does not run on another, so a line takes a cache only from
    /// its own platform. None: saved by an older layr.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    #[serde(rename = "layer_platform", alias = "cache_platform")]
    pub cache_platform: Option<String>,
}

pub const REF_PREFIXES: &[&str] = &["env/", "release/", "pins/", "holds/", "leases/", "caches/", "derived/"];

pub fn ref_namespace(name: &str) -> bool {
    REF_PREFIXES.iter().any(|p| name.starts_with(p))
}

pub const RESERVED_PREFIXES: &[&str] =
    &["env/", "release/", "pins/", "holds/", "leases/", "caches/", "derived/", "changes/", "views/", "attestations/"];

pub fn reserved_revision(name: &str) -> bool {
    RESERVED_PREFIXES.iter().any(|p| name.starts_with(p))
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefState {
    pub name: String,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct RefChange {
    pub name: String,
    pub before: Option<RefState>,
    pub after: Option<RefState>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueEntry {
    pub id: String,
    pub into: String,
    pub target_line: String,
    pub state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub added_by: u32,
    pub time: i64,
    pub status: String,
    #[serde(default)]
    pub attempts: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub probes: Option<serde_json::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct QueueChange {
    pub id: String,
    pub before: Option<QueueEntry>,
    pub after: Option<QueueEntry>,
}

pub const ATTESTATION_KINDS: &[&str] = &["check", "approval", "origin", "import", "quarantine", "revoke"];

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Attestation {
    pub format: u32,
    pub attestation_id: String,
    pub subject: Subject,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change_id: Option<String>,
    pub kind: String,
    pub payload_digest: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payload_ref: Option<String>,
    pub signer: String,
    pub time: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Subject {
    pub kind: String,
    pub id: String,
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

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Line {
    pub id: String,
    pub name: String,
    pub head: String,
    pub owner: u32,
    pub machine: String,
}
