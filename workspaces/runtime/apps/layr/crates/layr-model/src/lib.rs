//! Pure read model for verified Layr records.
//!
//! Verify signatures and authorize machine keys with `layr-store` before calling this crate.
//! No function reads files, uses a clock, or opens a network connection. Pass the machine
//! perspective and current epoch milliseconds explicitly. Results implement serde traits.
//! Large file contents stay outside the model. Probe comparisons accept bytes on demand.
#![forbid(unsafe_code)]

pub mod authority;
pub mod diff;
pub mod interdiff;
pub mod operations;
pub mod overlays;
pub mod policy;
pub mod probes;
pub mod protection;
pub mod screenshots;
pub mod types;
pub mod validation;

use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};
pub use types::*;

/// A collection of projects. Overlay records retain their separate project identity.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Model {
    pub projects: BTreeMap<String, Project>,
}

impl Model {
    /// Fold authenticated JSON Lines (`{"r":record,"s":signature}`), in any arrival order.
    /// The caller must verify each signature and authorize its signer first.
    pub fn from_verified_lines<'a>(lines: impl IntoIterator<Item = &'a str>, machine: &str) -> Result<Self> {
        #[derive(Deserialize)]
        struct Envelope {
            r: Record,
        }
        let mut records = Vec::new();
        for line in lines {
            let record = serde_json::from_str::<Envelope>(line).context("parse verified record line")?.r;
            validation::validate(&record)?;
            records.push(record);
        }
        Self::from_records(records, machine)
    }

    /// Readable overlay projects are discovered only from their own records.
    /// A parent-only model contains no hidden name, count, or layer identifier.
    pub fn overlay_projects(&self, parent: &str) -> Vec<&Project> {
        self.projects
            .values()
            .filter(|project| project.overlay_setting("overlay").is_some_and(|v| v["project"] == parent))
            .collect()
    }

    /// Follow hidden overlay lines without adding metadata to the parent project.
    pub fn layers(&self, parent: &str, line: &Line) -> Result<Vec<validation::Layer>> {
        let project = self.projects.get(parent).context("unknown parent project")?;
        let key = format!("stack.{}", line.id);
        let value = project.setting(&format!("{key}@{}", line.machine)).or_else(|| project.setting(&key));
        let mut layers: Vec<validation::Layer> = value.cloned().map(serde_json::from_value).transpose()?.unwrap_or_default();
        let mut hidden = Vec::new();
        for overlay in self.overlay_projects(parent) {
            let Some(info) = overlay.overlay_setting("overlay") else {
                continue;
            };
            if info["hidden"] != true {
                continue;
            }
            let name = info["name"].as_str().context("hidden overlay has no name")?;
            let selection = overlay.overlay_setting(&format!("hidden.stack.{}", line.id));
            if selection.is_some_and(|v| v["enabled"] == false) {
                continue;
            }
            if let Some(ol) = overlay.line_list().into_iter().find(|l| l.name == format!("line-{}", line.id)) {
                hidden.push((
                    selection.and_then(|v| v["position"].as_u64()).unwrap_or(u64::MAX),
                    validation::Layer { overlay: name.into(), state: ol.head },
                ));
            }
        }
        hidden.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.overlay.cmp(&b.1.overlay)));
        for (position, layer) in hidden {
            layers.insert((position as usize).min(layers.len()), layer);
        }
        Ok(layers)
    }

    /// Fold verified records, including any readable overlay projects.
    pub fn from_records(records: impl IntoIterator<Item = Record>, machine: &str) -> Result<Self> {
        let mut grouped: BTreeMap<String, Vec<Record>> = BTreeMap::new();
        for record in records {
            // The machine announcement has no project in old logs. It belongs to the supplied
            // project stream. `Project::from_records` handles that case without guessing here.
            grouped.entry(record.project.clone()).or_default().push(record);
        }
        let announcements = grouped.remove("").unwrap_or_default();
        let mut projects = BTreeMap::new();
        for (id, mut records) in grouped {
            records.extend(announcements.iter().cloned());
            projects.insert(id.clone(), Project::from_records(&id, records, machine)?);
        }
        Ok(Self { projects })
    }
}

/// A line and the independent head last published by each machine.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct LineView {
    #[serde(flatten)]
    pub line: Line,
    pub heads: BTreeMap<String, String>,
    pub deleted: bool,
}

/// State metadata. Store availability is a caller concern, separate from signed history.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct StateView {
    pub state: StateRec,
    pub machine: String,
    pub dropped: bool,
}

/// Derived project metadata. All records, including unknown operations, remain in the log.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Project {
    pub id: String,
    pub machine: String,
    pub name: Option<String>,
    pub states: BTreeMap<String, StateView>,
    pub lines: BTreeMap<String, LineView>,
    pub config: BTreeMap<String, Value>,
    pub tags: BTreeMap<String, Value>,
    pub remote_refs: BTreeMap<String, Value>,
    pub machines: BTreeMap<String, Value>,
    pub trusted: BTreeMap<String, Value>,
    records: Vec<Record>,
    refs: BTreeMap<String, RefState>,
    ref_history: Vec<(usize, RefChange)>,
    queue: BTreeMap<String, QueueEntry>,
    attestations: Vec<(usize, Attestation)>,
}

fn order(r: &Record) -> (i64, &str, u64) {
    (r.time.max(0), &r.machine, r.seq)
}

impl Project {
    /// Build one project from verified records. `machine` selects the CLI policy perspective.
    /// Empty `machine` selects a store view with no preferred local machine.
    /// Exact duplicate records are accepted. Conflicting identities or sequences fail.
    pub fn from_records(id: &str, records: impl IntoIterator<Item = Record>, machine: &str) -> Result<Self> {
        let mut unique = BTreeMap::<String, Record>::new();
        let mut sequences = BTreeMap::new();
        for record in records {
            if !record.project.is_empty() && record.project != id {
                bail!("record belongs to another project");
            }
            if let Some(old) = unique.get(&record.id) {
                if serde_json::to_value(old)? != serde_json::to_value(&record)? {
                    bail!("conflicting record identity");
                }
                continue;
            }
            if sequences.insert((record.machine.clone(), record.seq), record.id.clone()).is_some() {
                bail!("conflicting machine sequence");
            }
            unique.insert(record.id.clone(), record);
        }
        let mut records: Vec<_> = unique.into_values().collect();
        records.sort_by(|a, b| order(a).cmp(&order(b)));
        let mut p = Self {
            id: id.into(),
            machine: machine.into(),
            name: None,
            states: BTreeMap::new(),
            lines: BTreeMap::new(),
            config: BTreeMap::new(),
            tags: BTreeMap::new(),
            remote_refs: BTreeMap::new(),
            machines: BTreeMap::new(),
            trusted: BTreeMap::new(),
            records: vec![],
            refs: BTreeMap::new(),
            ref_history: vec![],
            queue: BTreeMap::new(),
            attestations: vec![],
        };
        for (index, rec) in records.iter().enumerate() {
            p.derive(index, rec)?;
        }
        p.records = records;
        Ok(p)
    }

    fn derive(&mut self, index: usize, r: &Record) -> Result<()> {
        let foreign = r.machine != self.machine;
        for state in &r.states {
            if self.states.contains_key(&state.id) {
                bail!("state {} is named by two records", state.id);
            }
            if state.parents.iter().any(|id| self.states.get(id).is_some_and(|s| s.state.kind.auxiliary())) {
                bail!("source history cannot have a cache or derived parent");
            }
            self.states.insert(state.id.clone(), StateView { state: state.clone(), machine: r.machine.clone(), dropped: false });
        }
        for (id, change) in &r.lines {
            let heads = self.lines.entry(id.clone()).or_insert_with(|| LineView {
                line: Line { id: id.clone(), name: String::new(), head: String::new(), owner: 0, machine: r.machine.clone() },
                heads: BTreeMap::new(),
                deleted: true,
            });
            if let Some(after) = &change.after {
                if self.states.get(&after.head).is_some_and(|s| s.state.kind.auxiliary()) {
                    bail!("line head cannot be a cache or derived state");
                }
                heads.heads.insert(r.machine.clone(), after.head.clone());
            }
            if foreign && heads.line.machine == self.machine {
                continue;
            }
            if let Some(after) = &change.after {
                heads.line = Line {
                    id: id.clone(),
                    name: after.name.clone(),
                    head: after.head.clone(),
                    owner: after.owner,
                    machine: after.machine.clone(),
                };
                heads.deleted = false;
            } else {
                heads.deleted = true;
            }
        }
        let d = &r.data;
        if let Some(v) = d.get("queue_change") {
            let ch: QueueChange = serde_json::from_value(v.clone())?;
            if let Some(after) = ch.after {
                self.queue.insert(ch.id, after);
            } else {
                self.queue.remove(&ch.id);
            }
        }
        if let Some(v) = d.get("ref_change") {
            let ch: RefChange = serde_json::from_value(v.clone())?;
            if let Some(after) = &ch.after {
                self.refs.insert(ch.name.clone(), after.clone());
            } else {
                self.refs.remove(&ch.name);
            }
            self.ref_history.push((index, ch));
        }
        let a = if let Some(v) = d.get("attestation") {
            Some(serde_json::from_value::<Attestation>(v.clone())?)
        } else if matches!(r.op.as_str(), "approve" | "approve.withdraw" | "check") {
            d["state"].as_str().map(|state| Attestation {
                format: 1,
                attestation_id: r.id.clone(),
                subject: Subject { kind: "state".into(), id: state.into() },
                change_id: self.states.get(state).and_then(|s| s.state.change_id.clone()),
                kind: match r.op.as_str() {
                    "approve" => "approval",
                    "check" => "check",
                    _ => "revoke",
                }
                .into(),
                payload_digest: hex::encode(Sha256::digest(serde_json::to_vec(d).unwrap_or_default())),
                payload_ref: None,
                signer: r.machine.clone(),
                time: r.time,
            })
        } else {
            None
        };
        if let Some(a) = a {
            self.attestations.push((index, a));
        }
        let str_at = |key: &str| d[key].as_str().unwrap_or("").to_string();
        match r.op.as_str() {
            "machine" => {
                self.machines.entry(r.machine.clone()).or_insert(d.clone());
                if !foreign {
                    self.trusted.entry(r.machine.clone()).or_insert(d.clone());
                }
            }
            "machine.trust" => {
                self.trusted.entry(str_at("machine")).or_insert(d.clone());
            }
            "project" | "config" | "overlay.create" | "overlay.readers" | "overlay.use" | "overlay.show-to-project" => {
                if r.op == "project" {
                    self.name = d["name"].as_str().map(String::from);
                }
                if let Some(config) = d["config"].as_object() {
                    for (key, value) in config {
                        let key = if foreign { format!("{key}@{}", r.machine) } else { key.clone() };
                        self.config.insert(key, value.clone());
                    }
                }
            }
            "tag" => {
                self.tags.insert(str_at("name"), d.clone());
            }
            "tag.delete" => {
                self.tags.remove(&str_at("name"));
            }
            "import" | "remote.ref" => {
                if d["ref"].is_string() && d["state"].is_string() {
                    self.remote_refs.insert(str_at("ref"), d.clone());
                }
            }
            "export" if d["status"] == "pushed" => {
                self.remote_refs.insert(format!("{}/{}", str_at("remote"), str_at("branch")), d.clone());
            }
            "stash.drop" => {
                if let Some(s) = self.states.get_mut(&str_at("state")) {
                    s.dropped = true;
                }
            }
            "save.caches" | "save.layers" => {
                if let Some(s) = self.states.get_mut(&str_at("state")) {
                    if s.machine == r.machine {
                        if let Some(caches) = d.get("caches").or_else(|| d.get("layers")).and_then(Value::as_object) {
                            for (path, value) in caches {
                                s.state.caches.insert(path.clone(), value.as_str().context("invalid cache name")?.into());
                            }
                        }
                        s.state.cache_by = Some(r.actor.uid);
                        s.state.cache_shared = d["shared"].as_bool().unwrap_or(false);
                        s.state.cache_platform = d["platform"].as_str().map(String::from);
                    }
                }
            }
            _ => {}
        }
        if let Some(stack) = d.get("stack") {
            if let (Some(line), Some(layers)) = (stack["line"].as_str(), stack.get("layers")) {
                let key = if foreign { format!("stack.{line}@{}", r.machine) } else { format!("stack.{line}") };
                self.config.insert(key, layers.clone());
            }
        }
        Ok(())
    }

    /// Active lines in CLI name order. Local working paths are outside this model.
    pub fn line_list(&self) -> Vec<Line> {
        let mut out: Vec<_> = self.lines.values().filter(|v| !v.deleted).map(|v| v.line.clone()).collect();
        out.sort_by(|a, b| a.name.cmp(&b.name).then(a.id.cmp(&b.id)));
        out
    }
    pub fn state(&self, id: &str) -> Option<&StateRec> {
        self.states.get(id).map(|s| &s.state)
    }
    /// Automatic or named states of one line, newest first.
    pub fn line_states(&self, line: &str, kind: StateKind) -> Vec<&StateRec> {
        let mut out: Vec<_> = self
            .states
            .values()
            .filter(|s| !s.dropped && s.state.line.as_deref() == Some(line) && s.state.kind == kind)
            .map(|s| &s.state)
            .collect();
        out.sort_by(|a, b| (b.time, &b.id).cmp(&(a.time, &a.id)));
        out
    }
    /// Walk source history in the CLI's date and state ID order.
    pub fn log(&self, starts: &[String], auto_line: Option<&str>, limit: usize) -> Vec<StateRec> {
        let mut heap = BinaryHeap::new();
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        for id in starts {
            if let Some(s) = self.state(id).filter(|s| !s.kind.auxiliary()) {
                heap.push((s.time, s.id.clone()));
            }
        }
        if let Some(line) = auto_line {
            for s in self.line_states(line, StateKind::Auto) {
                heap.push((s.time, s.id.clone()));
            }
        }
        while let Some((_, id)) = heap.pop() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(s) = self.state(&id) {
                if out.len() >= limit {
                    break;
                }
                for parent in &s.parents {
                    if let Some(p) = self.state(parent) {
                        heap.push((p.time, p.id.clone()));
                    }
                }
                out.push(s.clone());
            }
        }
        out
    }
    /// Live refs at the supplied epoch time, in name order.
    pub fn ref_list(&self, prefix: &str, now: i64) -> Vec<RefState> {
        self.refs.values().filter(|r| r.name.starts_with(prefix) && r.expires_at.is_none_or(|end| end > now)).cloned().collect()
    }
    pub fn ref_log(&self, name: &str) -> Vec<RefChange> {
        self.ref_history.iter().rev().filter(|(_, ch)| ch.name == name).map(|(_, ch)| ch.clone()).collect()
    }
    pub fn queue_list(&self, target: Option<&str>) -> Vec<QueueEntry> {
        let mut out: Vec<_> = self.queue.values().filter(|e| target.is_none_or(|t| t == e.target_line)).cloned().collect();
        out.sort_by(|a, b| (a.time, &a.id).cmp(&(b.time, &b.id)));
        out
    }
    pub fn changes(&self) -> Vec<String> {
        self.states.values().filter_map(|s| s.state.change_id.clone()).collect::<BTreeSet<_>>().into_iter().collect()
    }
    pub fn change_versions(&self, id: &str) -> Vec<StateRec> {
        let mut out: Vec<_> =
            self.states.values().filter(|s| s.state.change_id.as_deref() == Some(id)).map(|s| s.state.clone()).collect();
        out.sort_by(|a, b| (a.time, &a.id).cmp(&(b.time, &b.id)));
        out
    }
    /// All evidence, including revoked evidence, in the same order as `attestations --json`.
    pub fn attestations(&self, subject: &str, kind: Option<&str>) -> Vec<Attestation> {
        self.attestations
            .iter()
            .filter(|(_, a)| a.subject.id == subject && kind.is_none_or(|k| k == a.kind))
            .map(|(_, a)| a.clone())
            .collect()
    }
    /// A revocation only affects earlier evidence from the same machine and user.
    pub fn is_revoked(&self, id: &str) -> bool {
        let Some((index, a)) = self.attestations.iter().find(|(_, a)| a.attestation_id == id) else {
            return false;
        };
        let owner = &self.records[*index];
        self.attestations.iter().any(|(i, r)| {
            if *i <= *index || r.kind != "revoke" || r.subject.id != a.subject.id {
                return false;
            }
            let rec = &self.records[*i];
            rec.machine == owner.machine
                && rec.actor.uid == owner.actor.uid
                && (rec.data["revokes"].as_array().is_some_and(|v| v.iter().any(|v| v.as_str() == Some(id)))
                    || (rec.op == "approve.withdraw" && a.kind == "approval"))
        })
    }
    /// Operation log, newest first. The optional line filter matches `op log`.
    pub fn op_log(&self, line: Option<&str>, limit: usize) -> Vec<Record> {
        let mut records: Vec<_> = self
            .records
            .iter()
            .filter(|r| {
                line.is_none_or(|l| {
                    matches!(r.op.as_str(), "push" | "sync") || r.lines.contains_key(l) || r.data["line"].as_str() == Some(l)
                })
            })
            .cloned()
            .collect();
        records.sort_by(|a, b| (b.time, b.seq).cmp(&(a.time, a.seq)));
        records.truncate(limit);
        records
    }
    /// Exact signed metadata of probe checks. Output bytes are not retained here.
    pub fn probe_results(&self, subject: &str) -> Vec<Value> {
        self.attestations(subject, Some("check"))
            .iter()
            .filter(|a| !self.is_revoked(&a.attestation_id))
            .filter_map(|a| self.records.iter().find(|r| r.id == a.attestation_id))
            .filter_map(|r| r.data.get("check").filter(|c| c.get("probe").is_some()).cloned())
            .collect()
    }
    /// Effective policy at the supplied time. Foreign policy stays machine scoped.
    pub fn access_policy(&self, now: i64, overlay: bool) -> policy::Policy {
        policy::Policy::from_project(self, now, overlay)
    }
    /// The current local setting, or null when it was deleted.
    pub fn setting(&self, key: &str) -> Option<&Value> {
        self.config.get(key).filter(|v| !v.is_null())
    }
    /// Latest overlay policy across machine scopes, including deletion markers.
    pub fn overlay_setting(&self, key: &str) -> Option<&Value> {
        self.records
            .iter()
            .rev()
            .filter(|r| {
                matches!(
                    r.op.as_str(),
                    "project" | "config" | "overlay.create" | "overlay.readers" | "overlay.use" | "overlay.show-to-project"
                )
            })
            .find_map(|r| r.data["config"].get(key))
            .filter(|v| !v.is_null())
    }
}

impl Project {
    /// The latest matching check result. Arguments, revocations, and legacy commands follow Core.
    pub fn check_exit(&self, subject: &str, argv: &[String], wanted: Option<i64>) -> Option<i64> {
        for a in self.attestations(subject, Some("check")).iter().rev() {
            if self.is_revoked(&a.attestation_id) {
                continue;
            }
            let rec = self.records.iter().find(|r| r.id == a.attestation_id)?;
            let result = rec.data.get("check").unwrap_or(&rec.data);
            let matches = match result.get("argv") {
                Some(v) => serde_json::from_value::<Vec<String>>(v.clone()).is_ok_and(|args| args == argv),
                None => {
                    argv.len() == 1
                        && !argv[0].is_empty()
                        && !argv[0].chars().any(char::is_whitespace)
                        && result["command"].as_str() == Some(&argv[0])
                }
            };
            if matches {
                if let Some(code) = result["exit"].as_i64() {
                    if wanted.is_none_or(|v| v == code) {
                        return Some(code);
                    }
                }
            }
        }
        None
    }
    /// Ancestry includes automatic states and follows all parents in breadth-first order.
    pub fn ancestors(&self, id: &str) -> Vec<String> {
        let mut seen = BTreeSet::new();
        let mut out = Vec::new();
        let mut queue = std::collections::VecDeque::from([id.to_string()]);
        while let Some(id) = queue.pop_front() {
            if !seen.insert(id.clone()) {
                continue;
            }
            if let Some(s) = self.state(&id) {
                queue.extend(s.parents.iter().cloned());
            }
            out.push(id);
        }
        out
    }
    pub fn merge_base(&self, a: &str, b: &str) -> Option<String> {
        let right: BTreeSet<_> = self.ancestors(b).into_iter().collect();
        let common: Vec<_> = self.ancestors(a).into_iter().filter(|id| right.contains(id)).collect();
        common.iter().find(|c| !common.iter().any(|d| d != *c && self.ancestors(d).contains(c))).cloned()
    }
    /// CLI `change show --json`, including versions, all evidence, and stored queue behavior.
    /// Other behavior uses the supplied source definitions and probe output bytes.
    pub fn change_show(&self, id: &str, context: &probes::Context) -> Result<Value> {
        let versions = self.change_versions(id);
        if versions.is_empty() {
            bail!("unknown change '{id}'");
        }
        let rows: Vec<_> = versions
            .iter()
            .enumerate()
            .map(|(i, s)| {
                let mut row =
                    serde_json::json!({"version":format!("v{}",i+1),"state":s,"attestations":self.attestations(&s.id,None)});
                if let Some(version) = self.version_info(&s.id) {
                    row["operation"] = version;
                }
                row
            })
            .collect();
        let behavior = match versions.last().map(|s| self.change_behavior(s, context)).transpose() {
            Ok(Some(value)) => value,
            Ok(None) => serde_json::json!({"probes":[]}),
            Err(error) => serde_json::json!({"status":"unavailable","reason":format!("{error:#}")}),
        };
        Ok(serde_json::json!({"change_id":id,"versions":rows,"behavior":behavior}))
    }
    fn change_behavior(&self, state: &StateRec, context: &probes::Context) -> Result<Value> {
        for entry in self.queue_list(None).iter().rev() {
            if entry.state == state.id {
                if let Some(probes) = &entry.probes {
                    return Ok(probes["behavior"].clone());
                }
            }
        }
        let source = state.line.as_ref().and_then(|id| self.lines.get(id));
        let upstream = source.and_then(|s| self.setting(&format!("line.{}.upstream", s.line.name)));
        let target = match upstream {
            None | Some(Value::Null) => "main",
            Some(Value::String(s)) => s,
            _ => bail!("the line upstream must be a line name"),
        };
        let target =
            self.line_list().into_iter().find(|l| l.name == target).with_context(|| format!("line '{target}' not found"))?;
        let base = self.merge_base(&target.head, &state.id).context("change and target have no common ancestor")?;
        let mut behavior = probes::diff(
            self,
            &Subject { kind: "state".into(), id: base.clone() },
            &Subject { kind: "state".into(), id: state.id.clone() },
            context,
        )?;
        if !behavior.probes.iter().any(|p| p["base_run"] == true || p["base"].is_object()) {
            behavior.base_views = self.view_results(&base, context)?;
        }
        if !behavior.probes.iter().any(|p| p["head_run"] == true || p["head"].is_object()) {
            behavior.head_views = self.view_results(&state.id, context)?;
        }
        Ok(serde_json::to_value(behavior)?)
    }
    fn view_results(&self, state: &str, context: &probes::Context) -> Result<Vec<String>> {
        let mut views = BTreeSet::new();
        for rec in &self.records {
            if rec.op != "view.save" || rec.data["view"]["shared"].as_str() != Some(state) {
                continue;
            }
            let Some(id) = rec.data["view_id"].as_str() else { continue };
            let subject = Subject { kind: "view".into(), id: id.into() };
            for result in self.probe_results(id) {
                let Some(name) = result["probe"].as_str() else { continue };
                let Ok(definition) = serde_json::from_value::<probes::Probe>(result["definition"].clone()) else { continue };
                if definition.validate(name).is_ok() && probes::stored(self, &subject, name, &definition, context)?.is_some() {
                    views.insert(id.into());
                    break;
                }
            }
        }
        Ok(views.into_iter().collect())
    }
}

impl Project {
    /// Local approvals that still apply to the exact subject. Scoped declassifications are excluded.
    pub fn approvals(&self, subject: &str) -> Vec<(u32, i64)> {
        let mut by_actor = BTreeMap::<(String, u32), i64>::new();
        for (index, a) in &self.attestations {
            let record = &self.records[*index];
            if a.subject.id != subject
                || a.kind != "approval"
                || self.is_revoked(&a.attestation_id)
                || record.data.get("declassify").is_some()
                || record.machine != self.machine
            {
                continue;
            }
            by_actor
                .entry((record.machine.clone(), record.actor.uid))
                .and_modify(|time| *time = (*time).max(a.time))
                .or_insert(a.time);
        }
        let mut out: Vec<_> = by_actor.into_iter().map(|((_, uid), time)| (uid, time)).collect();
        out.sort_by_key(|(uid, _)| *uid);
        out
    }
    /// Active check details in record order, including failed checks.
    pub fn checks(&self, subject: &str) -> Vec<Value> {
        self.attestations(subject, Some("check"))
            .into_iter()
            .filter(|a| !self.is_revoked(&a.attestation_id))
            .filter_map(|a| self.records.iter().find(|r| r.id == a.attestation_id))
            .map(|r| r.data.get("check").unwrap_or(&r.data).clone())
            .collect()
    }
}

#[cfg(test)]
mod tests;
