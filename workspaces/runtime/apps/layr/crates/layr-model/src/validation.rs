//! Shared validation of record fields, names, paths, and exact views.
use crate::types::*;
use anyhow::{bail, Result};
use serde::{Deserialize, Serialize};

pub fn check_name(kind: &str, name: &str) -> Result<()> {
    if matches!(kind, "line" | "tag" | "ref") && matches!(name, "HEAD" | "head") {
        bail!("reserved revision name: {name}");
    }
    if kind == "line" && reserved_revision(name) {
        bail!("reserved revision prefix in line name '{name}'");
    }
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

/// One overlay of a line's stack: its name and the overlay state the line shows.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layer {
    pub overlay: String,
    pub state: String,
}

/// An addressable stack. Field order defines the canonical JSON used for its identity.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct View {
    pub format: u32,
    pub shared: String,
    pub layers: Vec<Layer>,
}

impl View {
    pub fn new(shared: String, layers: Vec<Layer>) -> Self {
        Self { format: 1, shared, layers }
    }

    pub fn id(&self) -> Result<String> {
        use sha2::{Digest, Sha256};
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(self)?)))
    }

    pub fn validate(&self) -> Result<()> {
        if self.format != 1 {
            bail!("unsupported view format {}", self.format);
        }
        if !is_uuid(&self.shared) {
            bail!("invalid shared state in view");
        }
        for layer in &self.layers {
            check_name("overlay", &layer.overlay)?;
            if !is_uuid(&layer.state) {
                bail!("invalid state of overlay '{}'", layer.overlay);
            }
        }
        Ok(())
    }
}

pub fn is_view_id(id: &str) -> bool {
    id.len() == 64 && id.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
}

// ----- checks of records that come from other machines -----

pub fn is_uuid(s: &str) -> bool {
    layr_store::format::is_uuid(s)
}

pub fn is_name(s: &str) -> bool {
    check_name("name", s).is_ok()
}

/// A cache snapshot name: `<state-id>@<path with '/' as %2F>`.
pub fn is_cache_name(s: &str, state: &str) -> bool {
    match s.split_once('@') {
        Some((id, p)) => {
            // `.venv` is a cache path; the whole name starts with the state ID, so a dot
            // after the `@` makes no hidden or parent entry.
            id == state
                && !p.is_empty()
                && p.chars().all(|c| c.is_ascii_alphanumeric() || "._-%".contains(c))
                && p != "."
                && p != ".."
        }
        None => false,
    }
}

pub fn is_ref(s: &str) -> bool {
    !s.is_empty()
        && !s.contains("..")
        && !s.starts_with('/')
        && !s.ends_with('/')
        && !s.contains("//")
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
        if let Some(id) = &s.change_id {
            uuid("change", id)?;
        }
        if let Some(id) = &s.derived_from {
            let view = s.kind == StateKind::Derived
                && id.len() == 64
                && id.bytes().all(|c| c.is_ascii_digit() || (b'a'..=b'f').contains(&c));
            if !view {
                uuid("derived source", id)?;
            }
        }
        if s.kind.auxiliary() && (s.line.is_some() || !s.parents.is_empty() || s.change_id.is_some()) {
            bail!("cache and derived states must be outside line history");
        }
        for p in &s.parents {
            uuid("parent", p)?;
            if r.states.iter().any(|state| &state.id == p && state.kind.auxiliary()) {
                bail!("source history cannot have a cache or derived parent");
            }
        }
        if let Some(l) = &s.line {
            uuid("line", l)?;
        }
        for (path, snap) in &s.caches {
            layr_store::paths::check_rel(path)?;
            if !is_cache_name(snap, &s.id) {
                bail!("record {}: bad cache name {snap:?}", r.seq);
            }
        }
        for c in &s.conflicts {
            layr_store::paths::check_rel(&c.path)?;
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
            if r.states.iter().any(|s| s.id == st.head && s.kind.auxiliary()) {
                bail!("line head cannot be a cache or derived state");
            }
            uuid("line machine", &st.machine)?;
        }
        for x in [&ch.working_before, &ch.working_after].into_iter().flatten() {
            uuid("working state", x)?;
        }
    }
    let d = &r.data;
    if let Some(value) = d.get("queue_change") {
        let change: QueueChange = serde_json::from_value(value.clone())?;
        uuid("queue entry", &change.id)?;
        for entry in [&change.before, &change.after].into_iter().flatten() {
            if entry.id != change.id || !is_name(&entry.into) || !["pending", "done", "failed"].contains(&entry.status.as_str()) {
                bail!("invalid queue entry");
            }
            uuid("queued state", &entry.state)?;
            uuid("queue target line", &entry.target_line)?;
            if let Some(id) = &entry.change_id {
                uuid("queue change", id)?;
            }
            if let Some(id) = &entry.result {
                uuid("queue result", id)?;
            }
        }
    }
    if matches!(r.op.as_str(), "push" | "sync") {
        let transfer: crate::operations::Transfer = serde_json::from_value(d.clone())?;
        if d["result"] != "success" || transfer.direction != r.op {
            bail!("invalid completed transfer result or direction");
        }
        let url = d["url"].as_str().ok_or_else(|| anyhow::anyhow!("transfer URL missing"))?;
        if url.is_empty() || crate::operations::public_url(url) != url || url.chars().any(char::is_control) {
            bail!("transfer URL must omit credentials, query and fragment");
        }
        check_name("remote", d["remote"].as_str().unwrap_or(""))?;
        for (ids, count) in
            [(&transfer.state_ids_sent, transfer.states_sent), (&transfer.state_ids_received, transfer.states_received)]
        {
            if ids.len() != count || ids.iter().collect::<std::collections::BTreeSet<_>>().len() != count {
                bail!("transfer state count differs from its state ids");
            }
            for id in ids {
                uuid("transferred state", id)?;
            }
        }
        if r.op == "push" && (transfer.records_received != 0 || transfer.states_received != 0 || transfer.bytes_received != 0) {
            bail!("push cannot receive data");
        }
    }
    if let Some(version) = d.get("version") {
        let version: crate::operations::VersionOperation = serde_json::from_value(version.clone())?;
        uuid("version state", &version.state)?;
        uuid("version source", &version.source)?;
        uuid("version target head", &version.target_head)?;
        if let Some(line) = &version.target_line {
            uuid("version target line", line)?;
        }
        if !matches!(version.kind.as_str(), "amend" | "replay" | "pick") || !r.states.iter().any(|s| s.id == version.state) {
            bail!("invalid version kind or state");
        }
    }
    if r.op == "bisect.run" {
        for key in ["good", "bad", "first_bad"] {
            uuid(key, d[key].as_str().unwrap_or(""))?;
        }
        let argv: Vec<String> = serde_json::from_value(d["argv"].clone())?;
        if argv.is_empty() || d["first_bad"] != d["state"] || d["good"] == d["bad"] {
            bail!("invalid bisect command or bounds");
        }
        for step in d["steps"].as_array().ok_or_else(|| anyhow::anyhow!("bisect steps missing"))? {
            uuid("bisect step", step["state"].as_str().unwrap_or(""))?;
            if step["exit"].as_i64().and_then(|v| i32::try_from(v).ok()).is_none() || !step["reused"].is_boolean() {
                bail!("invalid bisect check result");
            }
        }
    }
    if r.op == "view.save" {
        let view: View =
            serde_json::from_value(d.get("view").cloned().ok_or_else(|| anyhow::anyhow!("view.save has no descriptor"))?)?;
        view.validate()?;
        let id = d.get("view_id").and_then(|v| v.as_str()).unwrap_or("");
        if !is_view_id(id) || view.id()? != id {
            bail!("view descriptor does not match its id");
        }
    }
    if let Some(v) = d.get("ref_change") {
        let ch: RefChange = serde_json::from_value(v.clone())?;
        if !is_ref(&ch.name) {
            bail!("invalid ref name {:?}", ch.name);
        }
        for value in [&ch.before, &ch.after].into_iter().flatten() {
            if value.name != ch.name {
                bail!("ref change names differ");
            }
            uuid("ref target", &value.target)?;
        }
    }
    if let Some(v) = d.get("attestation") {
        let a: Attestation = serde_json::from_value(v.clone())?;
        uuid("attestation", &a.attestation_id)?;
        if a.attestation_id != r.id {
            bail!("attestation id must match its signed record");
        }
        match a.subject.kind.as_str() {
            "state" => uuid("subject", &a.subject.id)?,
            "view" if is_view_id(&a.subject.id) => {}
            _ => bail!("unsupported attestation subject"),
        }
        if let Some(id) = &a.change_id {
            uuid("change", id)?;
        }
        if a.format != 1 || !ATTESTATION_KINDS.contains(&a.kind.as_str()) {
            bail!("unsupported attestation format, subject or kind");
        }
        if a.signer != r.machine || a.time != r.time {
            bail!("attestation signer and time must match its signed record");
        }
        if a.payload_digest.len() != 64 || !a.payload_digest.bytes().all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()) {
            bail!("invalid attestation payload digest");
        }
        if let Some(id) = &a.payload_ref {
            uuid("payload reference", id)?;
        }
        if let Some(result) = d.get("check") {
            use sha2::{Digest, Sha256};
            if a.kind != "check" || a.payload_digest != hex::encode(Sha256::digest(serde_json::to_vec(result)?)) {
                bail!("check payload digest does not match its signed result");
            }
        }
    }
    if let Some(v) = d.get("revokes") {
        for id in v.as_array().ok_or_else(|| anyhow::anyhow!("invalid revocations"))? {
            uuid("revoked attestation", id.as_str().unwrap_or(""))?;
        }
    }
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
            // An overlay is the internal project `<project>%<overlay>`.
            let overlay_project =
                r.op == "project" && k == "name" && v.split_once('%').map(|(a, b)| is_name(a) && is_name(b)) == Some(true);
            if !is_name(v) && !overlay_project {
                bail!("record {}: bad {k} {v:?}", r.seq);
            }
        }
    }
    // The overlays a line shows: names of overlays and their states.
    if let Some(st) = d.get("stack") {
        uuid("stack line", st.get("line").and_then(|v| v.as_str()).unwrap_or(""))?;
        for l in st.get("layers").and_then(|v| v.as_array()).ok_or_else(|| anyhow::anyhow!("record {}: bad stack", r.seq))? {
            let name = l.get("overlay").and_then(|v| v.as_str()).unwrap_or("");
            if !is_name(name) {
                bail!("record {}: bad overlay name {name:?}", r.seq);
            }
            uuid("overlay state", l.get("state").and_then(|v| v.as_str()).unwrap_or(""))?;
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
    for field in ["layers", "caches"] {
        if let (Some(st), Some(c)) = (d.get("state").and_then(|v| v.as_str()), d.get(field).and_then(|v| v.as_object())) {
            for (path, snap) in c {
                layr_store::paths::check_rel(path)?;
                if !is_cache_name(snap.as_str().unwrap_or(""), st) {
                    bail!("record {}: bad cache name", r.seq);
                }
            }
        }
    }
    if let (Some(st), Some(u)) = (d.get("state").and_then(|v| v.as_str()), d.get("uuids").and_then(|v| v.as_object())) {
        for (snap, id) in u {
            if !is_cache_name(snap, st) {
                bail!("record {}: bad cache name", r.seq);
            }
            uuid("cache subvolume", id.as_str().unwrap_or(""))?;
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
    use std::collections::BTreeMap;

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
            change_id: None,
            derived_from: None,
            context: None,
            parents: vec![],
            line: None,
            subvol: String::new(),
            author: Author::default(),
            message: String::new(),
            time: 0,
            running: false,
            conflicts: vec![],
            caches: BTreeMap::new(),
            cache_by: None,
            cache_shared: false,
            cache_platform: None,
        }
    }

    #[test]
    fn reads_the_session_label_of_older_records() {
        let a: Actor = serde_json::from_str(r#"{"uid":1,"name":"u","agent":"old label"}"#).unwrap();
        assert_eq!(a.session.as_deref(), Some("old label"));
        assert!(serde_json::to_string(&a).unwrap().contains(r#""session":"old label""#));
    }

    #[test]
    fn a_cache_path_may_start_with_a_dot() {
        let id = "01a12290-0000-7000-8000-000000000003";
        assert!(is_cache_name(&format!("{id}@.venv"), id));
        assert!(is_cache_name(&format!("{id}@web%2Fnode_modules"), id));
        for bad in [".", "..", "", "a/b", "a b"] {
            assert!(!is_cache_name(&format!("{id}@{bad}"), id), "{bad}");
        }
        assert!(!is_cache_name(&format!("{id}@.venv"), "01a12290-0000-7000-8000-000000000004"));
    }

    #[test]
    fn accepts_a_normal_record() {
        let mut r = rec();
        r.states.push(state("01a12290-0000-7000-8000-000000000003"));
        assert!(validate(&r).is_ok());
    }

    #[test]
    fn checks_the_stack_of_a_line() {
        let line = "01a12290-0000-7000-8000-000000000004";
        let st = "01a12290-0000-7000-8000-000000000005";
        let mut r = rec();
        r.data = serde_json::json!({"stack": {"line": line, "layers": [{"overlay": "keys", "state": st}]}});
        assert!(validate(&r).is_ok());
        r.data = serde_json::json!({"stack": {"line": line, "layers": [{"overlay": "../keys", "state": st}]}});
        assert!(validate(&r).is_err(), "an overlay name is a name");
        r.data = serde_json::json!({"stack": {"line": line, "layers": [{"overlay": "keys", "state": "../x"}]}});
        assert!(validate(&r).is_err(), "a state is an id");
        r.data = serde_json::json!({"stack": {"line": "main", "layers": []}});
        assert!(validate(&r).is_err(), "a line is an id");
    }

    #[test]
    fn an_overlay_project_name_only_in_its_project_record() {
        let mut r = rec();
        r.op = "project".into();
        r.data = serde_json::json!({"name": "sap%keys"});
        assert!(validate(&r).is_ok());
        r.data = serde_json::json!({"name": "sap%../keys"});
        assert!(validate(&r).is_err());
        r.op = "line.create".into();
        r.data = serde_json::json!({"name": "sap%keys"});
        assert!(validate(&r).is_err());
    }

    #[test]
    fn refuses_paths_as_ids_and_names() {
        let mut r = rec();
        r.states.push(state("/etc"));
        assert!(validate(&r).is_err());
        let mut r = rec();
        let mut s = state("01a12290-0000-7000-8000-000000000003");
        s.caches.insert("target".into(), "../../etc".into());
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
    #[test]
    fn optional_state_fields_preserve_legacy_bytes() {
        let old = state("01a12290-0000-7000-8000-000000000003");
        let raw = serde_json::to_value(&old).unwrap();
        for field in ["change_id", "derived_from", "context"] {
            assert!(raw.get(field).is_none());
        }
        let decoded: StateRec = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(serde_json::to_value(decoded).unwrap(), raw);
    }

    #[test]
    fn auxiliary_states_cannot_join_source_history() {
        let mut r = rec();
        let mut s = state("01a12290-0000-7000-8000-000000000003");
        s.kind = StateKind::Derived;
        s.derived_from = Some("01a12290-0000-7000-8000-000000000004".into());
        s.context = Some("oci-layer-v1".into());
        r.states.push(s);
        assert!(validate(&r).is_ok());
        r.states[0].parents.push("01a12290-0000-7000-8000-000000000004".into());
        assert!(validate(&r).is_err());
        r.states[0].parents.clear();
        r.states[0].line = Some("01a12290-0000-7000-8000-000000000005".into());
        assert!(validate(&r).is_err());
    }

    #[test]
    fn derived_sources_allow_exact_views_but_cache_sources_remain_states() {
        let mut r = rec();
        let mut s = state("01a12290-0000-7000-8000-000000000003");
        s.kind = StateKind::Derived;
        s.derived_from = Some("a".repeat(64));
        r.states.push(s);
        assert!(validate(&r).is_ok());
        for invalid in ["A".repeat(64), "a".repeat(63), "g".repeat(64)] {
            r.states[0].derived_from = Some(invalid);
            assert!(validate(&r).is_err());
        }
        r.states[0].derived_from = Some("a".repeat(64));
        r.states[0].kind = StateKind::Cache;
        assert!(validate(&r).is_err());
        r.states[0].derived_from = Some("01a12290-0000-7000-8000-000000000004".into());
        assert!(validate(&r).is_ok());
        r.states[0].kind = StateKind::Derived;
        assert!(validate(&r).is_ok());
    }

    #[test]
    fn new_records_validate_ref_and_attestation_boundaries() {
        let mut r = rec();
        r.data =
            serde_json::json!({"ref_change":{"name":"env/prod","before":null,"after":{"name":"env/prod","target":"../escape"}}});
        assert!(validate(&r).is_err());
        let a = Attestation {
            format: 1,
            attestation_id: r.id.clone(),
            subject: Subject { kind: "state".into(), id: "01a12290-0000-7000-8000-000000000003".into() },
            change_id: None,
            kind: "approval".into(),
            payload_digest: "0".repeat(64),
            payload_ref: None,
            signer: r.machine.clone(),
            time: r.time,
        };
        r.data = serde_json::json!({"attestation":a});
        assert!(validate(&r).is_ok());
        r.data["attestation"]["signer"] = serde_json::json!("01a12290-0000-7000-8000-000000000004");
        assert!(validate(&r).is_err());
    }
    #[test]
    fn saved_views_and_view_subjects_validate_their_identity() {
        let mut r = rec();
        let view = View::new("01a12290-0000-7000-8000-000000000003".into(), Vec::new());
        r.op = "view.save".into();
        r.data = serde_json::json!({"view_id": view.id().unwrap(), "view":view});
        assert!(validate(&r).is_ok());
        r.data["view_id"] = serde_json::json!("0".repeat(64));
        assert!(validate(&r).is_err());
        r.op = "attest".into();
        r.data = serde_json::json!({"attestation":Attestation {
            format:1, attestation_id:r.id.clone(), subject:Subject{kind:"view".into(),id:view.id().unwrap()},
            change_id:None,kind:"check".into(),payload_digest:"0".repeat(64),payload_ref:None,
            signer:r.machine.clone(),time:r.time,
        }});
        assert!(validate(&r).is_ok());
        r.data["attestation"]["subject"]["id"] = serde_json::json!("A".repeat(64));
        assert!(validate(&r).is_err());
        r.data["attestation"]["subject"]["kind"] = serde_json::json!("state");
        assert!(validate(&r).is_err());
    }
}
