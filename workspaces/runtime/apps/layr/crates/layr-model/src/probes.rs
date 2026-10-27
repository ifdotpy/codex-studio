//! Pure comparisons of stored probe results. The caller supplies decrypted file bytes.
use crate::{screenshots, Project, StateKind, Subject};
use anyhow::{Context as _, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Probe {
    pub run: Vec<String>,
    pub outputs: Vec<String>,
    #[serde(default)]
    pub kind: Kind,
    #[serde(default = "default_noise")]
    pub noise: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fail_over: Option<f64>,
    #[serde(default = "default_tolerance", skip_serializing_if = "is_default_tolerance")]
    pub tolerance: u8,
    #[serde(default = "default_ignore_attributes", skip_serializing_if = "is_default_ignore_attributes")]
    pub ignore_attributes: Vec<String>,
}
#[derive(Clone, Copy, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    #[default]
    Files,
    Json,
    Number,
    Screenshots,
}
pub fn default_tolerance() -> u8 {
    8
}
pub fn is_default_tolerance(value: &u8) -> bool {
    *value == default_tolerance()
}
pub fn default_ignore_attributes() -> Vec<String> {
    ["bounds", "frame", "x", "y", "width", "height", "left", "top", "right", "bottom"].map(str::to_owned).to_vec()
}
pub fn is_default_ignore_attributes(value: &Vec<String>) -> bool {
    *value == default_ignore_attributes()
}

impl Probe {
    /// Validate the same definition constraints as the CLI.
    pub fn validate(&self, name: &str) -> Result<()> {
        use anyhow::bail;
        let bytes = name.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 128
            || !bytes[0].is_ascii_alphanumeric()
            || !bytes.iter().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b"._-".contains(b))
        {
            bail!("invalid probe name");
        }
        if self.run.is_empty() || self.run[0].is_empty() || self.run.iter().any(|s| s.contains('\0')) {
            bail!("probe '{name}': run must be a nonempty argv array without NUL");
        }
        if self.outputs.is_empty() {
            bail!("probe '{name}': outputs must contain a file or folder path");
        }
        for path in &self.outputs {
            if path.is_empty()
                || path.starts_with('/')
                || path.contains('\0')
                || path.split('/').any(|p| p.is_empty() || p == ".git" || p == "." || p == "..")
            {
                bail!("probe '{name}': invalid output path '{path}'");
            }
        }
        if !self.noise.is_finite() || self.noise < 0.0 {
            bail!("probe '{name}': noise must be a finite nonnegative relative threshold");
        }
        if self.fail_over.is_some_and(|n| !n.is_finite() || n < 0.0) {
            bail!("probe '{name}': fail_over must be a finite nonnegative relative limit");
        }
        Ok(())
    }
}
fn default_noise() -> f64 {
    0.05
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RunResult {
    pub probe: String,
    pub subject: Subject,
    pub argv: Vec<String>,
    pub exit: i32,
    pub output_state: String,
    pub reused: bool,
}
/// File data needed to compare a probe. Hashes come from the supplied bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct FileData {
    size: u64,
    sha256: String,
    #[serde(skip)]
    text: Option<String>,
}
fn files(input: &BTreeMap<String, Vec<u8>>) -> BTreeMap<String, FileData> {
    input
        .iter()
        .map(|(path, bytes)| {
            let text = if bytes.len() <= 64 << 20 && !bytes[..bytes.len().min(8000)].contains(&0) {
                String::from_utf8(bytes.clone()).ok()
            } else {
                None
            };
            (path.clone(), FileData { size: bytes.len() as u64, sha256: hex::encode(Sha256::digest(bytes)), text })
        })
        .collect()
}
fn metrics(value: &Value) -> Result<BTreeMap<String, f64>> {
    let object = value.as_object().context("expected an object of numeric metrics")?;
    object
        .iter()
        .map(|(key, value)| {
            Ok((
                key.clone(),
                value.as_f64().filter(|n| n.is_finite()).with_context(|| format!("metric '{key}' must be a finite number"))?,
            ))
        })
        .collect()
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Behavior {
    pub base: String,
    pub head: String,
    pub base_source: String,
    pub head_source: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub base_views: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub head_views: Vec<String>,
    pub probes: Vec<Value>,
    #[serde(skip)]
    pub quiet_noise: BTreeMap<String, f64>,
}
/// Data supplied on demand by the store reader. Keys use full subject or state IDs.
/// Definitions come from each source's `layr.toml`. Files must pass store verification first.
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Context {
    pub now: i64,
    pub definitions: BTreeMap<String, BTreeMap<String, Probe>>,
    pub outputs: BTreeMap<String, BTreeMap<String, Vec<u8>>>,
    pub sources: BTreeMap<String, String>,
}
impl Context {
    pub fn source(&self, subject: &str) -> &str {
        self.sources.get(subject).map(String::as_str).unwrap_or("source unavailable")
    }
    fn definitions(&self, project: &Project, subject: &Subject) -> BTreeMap<String, Probe> {
        if self.source(&subject.id) == "available" {
            return self.definitions.get(&subject.id).cloned().unwrap_or_default();
        }
        let mut definitions = BTreeMap::new();
        for record in active_checks(project, subject).into_values() {
            let result = &record.data["check"];
            if let Ok(manifest) = serde_json::from_value::<BTreeMap<String, Probe>>(result["probes"].clone()) {
                definitions.extend(manifest.into_iter().filter(|(name, probe)| probe.validate(name).is_ok()));
            }
            if let (Some(name), Ok(definition)) =
                (result["probe"].as_str(), serde_json::from_value::<Probe>(result["definition"].clone()))
            {
                if definition.validate(name).is_ok() {
                    definitions.insert(name.into(), definition);
                }
            }
        }
        definitions
    }
}

fn active_checks<'a>(project: &'a Project, subject: &Subject) -> BTreeMap<&'a str, &'a crate::Record> {
    project
        .attestations
        .iter()
        .filter(|(_, a)| a.kind == "check" && a.subject == *subject && !project.is_revoked(&a.attestation_id))
        .map(|(index, a)| (a.attestation_id.as_str(), &project.records[*index]))
        .collect()
}

pub(crate) fn stored(
    project: &Project,
    subject: &Subject,
    name: &str,
    definition: &Probe,
    context: &Context,
) -> Result<Option<RunResult>> {
    if project.check_exit(&subject.id, &definition.run, None).is_none() {
        return Ok(None);
    }
    let reference = format!("derived/probe-v1/{name}/{}", subject.id);
    let root = project.ref_list(&reference, context.now).into_iter().find(|r| r.name == reference);
    let active = active_checks(project, subject);
    let mut latest = None;
    for rec in active.values() {
        let c = &rec.data["check"];
        if c["probe"].as_str() != Some(name) || c["definition"] != serde_json::to_value(definition)? {
            continue;
        }
        let Ok(mut r) = serde_json::from_value::<RunResult>(c.clone()) else { continue };
        if r.subject != *subject || r.argv != definition.run || root.as_ref().map(|v| &v.target) != Some(&r.output_state) {
            continue;
        }
        let Some(state) = project.state(&r.output_state) else { continue };
        if state.kind != StateKind::Derived
            || state.context.as_deref() != Some(&format!("probe-v1/{name}"))
            || state.derived_from.as_deref() != Some(&subject.id)
            || !context.outputs.contains_key(&r.output_state)
        {
            continue;
        }
        r.reused = true;
        latest = Some(r);
    }
    Ok(latest)
}

/// Compare exact subjects without I/O. Missing results retain the CLI's `not run` status.
pub fn diff(project: &Project, base: &Subject, head: &Subject, context: &Context) -> Result<Behavior> {
    let old = context.definitions(project, base);
    let new = context.definitions(project, head);
    let names: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    let mut probes = Vec::new();
    let mut quiet_noise = BTreeMap::new();
    for name in names {
        let a = old.get(name).map(|p| stored(project, base, name, p, context)).transpose()?.flatten();
        let b = new.get(name).map(|p| stored(project, head, name, p, context)).transpose()?.flatten();
        let (Some(a), Some(b)) = (&a, &b) else {
            probes.push(json!({"probe":name,"status":"not run","base_run":a.is_some(),"head_run":b.is_some()}));
            continue;
        };
        let p = &new[name];
        if old[name].kind != p.kind {
            probes.push(json!({"probe":name,"status":"kind changed","base":a,"head":b}));
            continue;
        }
        let (before, after) = if p.kind == Kind::Screenshots {
            (BTreeMap::new(), BTreeMap::new())
        } else {
            (files(&context.outputs[&a.output_state]), files(&context.outputs[&b.output_state]))
        };
        let changes = if p.kind == Kind::Screenshots {
            screenshots::compare(&context.outputs[&a.output_state], &context.outputs[&b.output_state], p)?
        } else {
            compare(&before, &after, p)?
        };
        if p.kind == Kind::Number
            && changes.is_empty()
            && before.iter().map(|(p, f)| (p, &f.sha256)).ne(after.iter().map(|(p, f)| (p, &f.sha256)))
        {
            quiet_noise.insert(name.clone(), p.noise);
        }
        let changed = if p.kind == Kind::Screenshots { changes.iter().any(screenshots::changed) } else { !changes.is_empty() };
        let mut row = json!({"probe":name,"status":if !changed && a.exit == b.exit {"unchanged"}else{"changed"},"base":a,"head":b,"exit_changed":a.exit != b.exit,"changes":changes});
        if p.kind == Kind::Screenshots {
            row["limit_reasons"] =
                json!(p.fail_over.map(|limit| screenshots::limit_reasons(name, &row, limit)).unwrap_or_default());
        }
        probes.push(row);
    }
    Ok(Behavior {
        base: base.id.clone(),
        head: head.id.clone(),
        base_source: context.source(&base.id).into(),
        head_source: context.source(&head.id).into(),
        base_views: vec![],
        head_views: vec![],
        probes,
        quiet_noise,
    })
}

fn compare(old: &BTreeMap<String, FileData>, new: &BTreeMap<String, FileData>, probe: &Probe) -> Result<Vec<Value>> {
    let paths: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    let mut out = Vec::new();
    for path in paths {
        let (a, b) = (old.get(path), new.get(path));
        if a.zip(b).map(|(a, b)| a.sha256 == b.sha256).unwrap_or(false) {
            continue;
        }
        match probe.kind {
            Kind::Screenshots => unreachable!("screenshots compare decoded outputs"),
            Kind::Files => {
                let text = a.map(|f| f.text.as_deref().unwrap_or("")).unwrap_or("");
                let next = b.map(|f| f.text.as_deref().unwrap_or("")).unwrap_or("");
                if a.into_iter().chain(b).all(|f| f.text.is_some()) {
                    let patch = similar::TextDiff::from_lines(text, next)
                        .unified_diff()
                        .header(&format!("a/{path}"), &format!("b/{path}"))
                        .to_string();
                    out.push(json!({"path":path,"kind":"text","change":if a.is_none(){"add"}else if b.is_none(){"remove"}else{"modify"},"diff":patch}));
                } else {
                    out.push(json!({"path":path,"kind":"binary","before":a,"after":b}));
                }
            }
            Kind::Json | Kind::Number => {
                let parse = |f: Option<&FileData>| -> Result<Option<Value>> {
                    f.map(|f| {
                        serde_json::from_str(f.text.as_deref().context("stored JSON probe is not text")?).map_err(Into::into)
                    })
                    .transpose()
                };
                let (a, b) = (parse(a)?, parse(b)?);
                let changes = if probe.kind == Kind::Json {
                    let mut rows = Vec::new();
                    json_changes(a.as_ref(), b.as_ref(), "", &mut rows);
                    rows
                } else {
                    number_changes(a.as_ref(), b.as_ref(), probe.noise)?
                };
                if !changes.is_empty() {
                    out.push(json!({"path":path,"changes":changes}));
                }
            }
        }
    }
    Ok(out)
}
fn json_changes(a: Option<&Value>, b: Option<&Value>, path: &str, out: &mut Vec<Value>) {
    if a == b {
        return;
    }
    match (a, b) {
        (Some(Value::Object(a)), Some(Value::Object(b))) => {
            for key in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
                let escaped = key.replace('~', "~0").replace('/', "~1");
                json_changes(a.get(key), b.get(key), &format!("{path}/{escaped}"), out);
            }
        }
        (Some(Value::Array(a)), Some(Value::Array(b))) => {
            for i in 0..a.len().max(b.len()) {
                json_changes(a.get(i), b.get(i), &format!("{path}/{i}"), out);
            }
        }
        _ => out.push(
            json!({"key":path,"change":if a.is_none(){"add"}else if b.is_none(){"remove"}else{"modify"},"before":a,"after":b}),
        ),
    }
}
fn number_changes(a: Option<&Value>, b: Option<&Value>, noise: f64) -> Result<Vec<Value>> {
    let a = a.map(metrics).transpose()?.unwrap_or_default();
    let b = b.map(metrics).transpose()?.unwrap_or_default();
    let mut out = Vec::new();
    for metric in a.keys().chain(b.keys()).collect::<BTreeSet<_>>() {
        let (old, new) = (a.get(metric), b.get(metric));
        if old == new {
            continue;
        }
        let relative = old.zip(new).and_then(|(a, b)| if *a == 0.0 { None } else { Some((b - a) / a.abs()) });
        if relative.map(|r| r.abs() <= noise).unwrap_or(false) {
            continue;
        }
        out.push(json!({"metric":metric,"before":old,"after":new,"relative_change":relative,"noise":noise}));
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definition(kind: Kind) -> Probe {
        Probe {
            run: vec!["true".into()],
            outputs: vec!["out".into()],
            kind,
            noise: 0.05,
            fail_over: None,
            tolerance: default_tolerance(),
            ignore_attributes: default_ignore_attributes(),
        }
    }
    fn compare_bytes(kind: Kind, a: &[u8], b: &[u8]) -> Vec<Value> {
        compare(
            &files(&BTreeMap::from([("out".into(), a.to_vec())])),
            &files(&BTreeMap::from([("out".into(), b.to_vec())])),
            &definition(kind),
        )
        .unwrap()
    }
    #[test]
    fn text_and_binary_outputs_match_cli_shapes() {
        let text = compare_bytes(Kind::Files, b"old\n", b"new\n");
        assert_eq!(text[0]["kind"], "text");
        assert!(text[0]["diff"].as_str().unwrap().contains("+new"));
        let binary = compare_bytes(Kind::Files, b"\0old", b"\0new");
        assert_eq!(binary[0]["kind"], "binary");
        assert_eq!(binary[0]["before"]["size"], 4);
        assert_ne!(binary[0]["before"]["sha256"], binary[0]["after"]["sha256"]);
    }
    #[test]
    fn json_paths_escape_reserved_characters() {
        let delta = compare_bytes(Kind::Json, br#"{"a/b":{"~key":[1]}}"#, br#"{"a/b":{"~key":[2]}}"#);
        assert_eq!(delta[0]["changes"][0]["key"], "/a~1b/~0key/0");
    }
    #[test]
    fn numeric_noise_zero_and_negative_values_follow_cli() {
        assert!(compare_bytes(Kind::Number, br#"{"quiet":100}"#, br#"{"quiet":104}"#).is_empty());
        let delta = compare_bytes(Kind::Number, br#"{"n":-100,"z":0}"#, br#"{"n":-110,"z":1}"#);
        assert_eq!(delta[0]["changes"][0]["relative_change"], -0.1);
        assert!(delta[0]["changes"][1]["relative_change"].is_null());
        assert!(metrics(&json!({"wrong":"1"})).is_err());
    }
    #[test]
    fn absence_does_not_claim_a_probe_ran() {
        let project = Project::from_records("p", vec![], "").unwrap();
        let mut context = Context::default();
        context.sources.insert("s".into(), "available".into());
        context.definitions.insert("s".into(), BTreeMap::from([("test".into(), definition(Kind::Files))]));
        let subject = Subject { kind: "state".into(), id: "s".into() };
        let behavior = diff(&project, &subject, &subject, &context).unwrap();
        assert_eq!(behavior.probes[0], json!({"probe":"test","status":"not run","base_run":false,"head_run":false}));
    }
    #[test]
    fn collected_source_definitions_follow_attestation_id_order() {
        let mut records = Vec::new();
        for (seq, id, argv) in [(1, "z", "last-by-id"), (2, "a", "last-by-time")] {
            let mut record = crate::Record::new("attest");
            record.id = id.into();
            record.seq = seq;
            record.time = seq as i64;
            record.machine = "machine".into();
            record.project = "project".into();
            let mut probe = definition(Kind::Files);
            probe.run = vec![argv.into()];
            record.data = json!({"attestation":{"format":1,"attestation_id":id,"subject":{"kind":"state","id":"subject"},"kind":"check","payload_digest":"00","signer":"machine","time":seq},"check":{"probe":"test","definition":probe}});
            records.push(record);
        }
        let project = Project::from_records("project", records, "machine").unwrap();
        let subject = Subject { kind: "state".into(), id: "subject".into() };
        let definitions = Context::default().definitions(&project, &subject);
        assert_eq!(definitions["test"].run, ["last-by-id"]);
    }
}
