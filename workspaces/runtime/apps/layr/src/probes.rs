//! Explicit probes and comparisons of their stored outputs.

use crate::ctx::Ctx;
use crate::fsutil::FType;
use crate::model::{Line, Record, RefChange, RefState, StateKind, Subject};
use crate::repo::{Repo, Temp};
use crate::tree::Tree;
use anyhow::{anyhow, bail, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;
use std::path::Path;

mod screenshots;

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
use layr_model::probes::{default_ignore_attributes, default_tolerance, is_default_ignore_attributes, is_default_tolerance};
fn default_noise() -> f64 {
    0.05
}
impl Probe {
    pub fn validate(&self, name: &str) -> Result<()> {
        crate::oci::valid_component(name).with_context(|| format!("probe '{name}'"))?;
        if self.run.is_empty() || self.run[0].is_empty() || self.run.iter().any(|s| s.contains('\0')) {
            bail!("probe '{name}': run must be a nonempty argv array without NUL");
        }
        if self.outputs.is_empty() {
            bail!("probe '{name}': outputs must contain a file or folder path");
        }
        for p in &self.outputs {
            crate::fsutil::check_rel(p).with_context(|| format!("probe '{name}' output"))?;
            if p.is_empty() || p.contains('\0') || p.split('/').any(|s| s.is_empty() || s == ".git" || s == ".") {
                bail!("probe '{name}': invalid output path '{p}'");
            }
        }
        if !self.noise.is_finite() || self.noise < 0.0 {
            bail!("probe '{name}': noise must be a finite nonnegative relative threshold");
        }
        if self.fail_over.is_some_and(|limit| !limit.is_finite() || limit < 0.0) {
            bail!("probe '{name}': fail_over must be a finite nonnegative relative limit");
        }
        Ok(())
    }
}

/// Resolve exact view identity. Stored outputs remain readable after source collection.
pub struct Input {
    pub subject: Subject,
    pub state: String,
    pub layers: Vec<crate::overlay::Layer>,
}
impl Input {
    fn view(&self) -> crate::overlay::View {
        crate::overlay::View::new(self.state.clone(), self.layers.clone())
    }
}
pub fn resolve(repo: &Repo, line: Option<&Line>, rev: &str) -> Result<Input> {
    let view = match crate::revs::resolve_view(repo, line, rev) {
        Ok(view) => view,
        Err(error) => {
            // The normal resolver rejects collected sources. Probe comparisons need only metadata.
            if let Some(id) = rev.strip_prefix("views/") {
                repo.saved_view(id)?
            } else if let Some(id) = rev.strip_prefix("attestations/") {
                let a = repo.p.db.attestation(id)?.context("unknown probe attestation")?;
                if a.subject.kind == "view" {
                    repo.saved_view(&a.subject.id)?
                } else if repo.p.db.state_flags(&a.subject.id)?.map(|(_, deleted)| deleted).unwrap_or(false) {
                    crate::overlay::View::new(a.subject.id, vec![])
                } else {
                    return Err(error);
                }
            } else {
                let state = crate::revs::resolve(repo, line, rev)?;
                if !repo.p.db.state_flags(&state)?.map(|(_, deleted)| deleted).unwrap_or(false) {
                    return Err(error);
                }
                crate::overlay::View::new(state, vec![])
            }
        }
    };
    repo.source_state(&view.shared)?;
    let subject = if view.layers.is_empty() {
        Subject { kind: "state".into(), id: view.shared.clone() }
    } else {
        Subject { kind: "view".into(), id: view.id()? }
    };
    Ok(Input { subject, state: view.shared, layers: view.layers })
}
fn readers_cover(project: &crate::access::Policy, overlay: &crate::access::Policy) -> bool {
    use crate::access::Role;
    let denies_read = |actions: &[String]| actions.iter().any(|a| a == "read" || a == "*");
    if overlay.deny.iter().any(|(principal, actions)| {
        denies_read(actions)
            && !project.deny.get(principal).map(|a| denies_read(a)).unwrap_or(false)
            && !project.deny.get("*").map(|a| denies_read(a)).unwrap_or(false)
    }) {
        return false;
    }
    let everyone = overlay.access.get("*").copied().unwrap_or(Role::None) >= Role::Reader;
    project
        .access
        .iter()
        .map(|(p, r)| (p, *r))
        .chain(project.grants.iter().map(|(p, (r, _))| (p, *r)))
        .filter(|(p, role)| p.as_str() != "user:0" && *role >= Role::Reader)
        .all(|(principal, _)| everyone || overlay.access.get(principal).copied().unwrap_or(Role::None) >= Role::Reader)
}
fn shared_outputs(repo: &Repo, layers: &[crate::overlay::Layer]) -> Result<()> {
    let mut project = crate::access::Policy::load(&repo.p);
    // Repo::can_read also permits line owners, even without a reader role.
    for line in repo.p.db.lines()? {
        project
            .access
            .entry(format!("user:{}", line.owner))
            .and_modify(|role| *role = (*role).max(crate::access::Role::Reader))
            .or_insert(crate::access::Role::Reader);
    }
    for layer in layers {
        let overlay = repo.overlay_project(&layer.overlay)?;
        if !readers_cover(&project, &crate::access::Policy::load(&overlay)) {
            bail!("probe outputs refuse private overlay '{}'; every project reader must be an overlay reader", layer.overlay);
        }
    }
    Ok(())
}

pub fn definitions(repo: &Repo, input: &Input) -> Result<BTreeMap<String, Probe>> {
    let assembled = if input.layers.is_empty() {
        None
    } else {
        let line = Line {
            id: crate::ids::new_id(),
            name: "probe-definition".into(),
            head: input.state.clone(),
            owner: 0,
            machine: repo.p.store.machine.id.clone(),
        };
        let assembled = repo.assemble(&line, &input.state, &input.layers)?;
        if source_status(repo, input) != "available" {
            bail!("{}", source_status(repo, input));
        }
        Some(assembled)
    };
    let root = assembled.as_ref().map(|t| t.path.clone()).unwrap_or(repo.state_root(&input.state)?);
    let tree = Tree::open(&root)?;
    let mut file = match tree.open_read("layr.toml") {
        Ok(f) => f.take((1 << 20) + 1),
        Err(_) if tree.stat("layr.toml").is_none() => return Ok(BTreeMap::new()),
        Err(e) => return Err(e),
    };
    let mut text = String::new();
    file.read_to_string(&mut text)?;
    if text.len() > 1 << 20 {
        bail!("layr.toml exceeds 1 MiB");
    }
    Ok(crate::oci::parse(&text)?.probe)
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
fn reference(name: &str, subject: &str) -> String {
    format!("derived/probe-v1/{name}/{subject}")
}

/// Core matches the exact argv; probe output and active attestation are checked separately.
fn check_exit(repo: &Repo, subject: &str, argv: &[String]) -> Result<Option<i64>> {
    repo.p.db.check_exit(subject, argv, None)
}

fn active_checks(repo: &Repo, input: &Input) -> Result<BTreeMap<String, (crate::model::Attestation, Record)>> {
    let mut active = BTreeMap::new();
    // Attestation order and signer/actor checks match Core's revocation rule.
    for a in repo.p.db.attestations(&input.subject.id, None)? {
        let Some(rec) = repo.p.db.record(&a.attestation_id)? else { continue };
        if a.kind == "check" && a.subject == input.subject {
            active.insert(a.attestation_id.clone(), (a, rec));
        } else if a.kind == "revoke" {
            for id in rec.data["revokes"].as_array().into_iter().flatten().filter_map(Value::as_str) {
                let remove = active
                    .get(id)
                    .map(|(old, r): &(crate::model::Attestation, Record)| old.signer == a.signer && r.actor.uid == rec.actor.uid)
                    .unwrap_or(false);
                if remove {
                    active.remove(id);
                }
            }
        }
    }
    Ok(active)
}

pub fn source_status(repo: &Repo, input: &Input) -> &'static str {
    let missing = |repo: &Repo, id: &str| {
        if repo.p.has_state(id) {
            "available"
        } else if repo.p.db.state_flags(id).ok().flatten().map(|(_, deleted)| deleted).unwrap_or(false) {
            "source collected"
        } else {
            "source unavailable"
        }
    };
    let shared = missing(repo, &input.state);
    if shared != "available" {
        return shared;
    }
    for layer in &input.layers {
        match repo.overlay_repo(&layer.overlay) {
            Ok(r) => {
                let status = missing(&r, &layer.state);
                if status != "available" {
                    return status;
                }
            }
            Err(_) => return "source unavailable",
        }
    }
    "available"
}
fn comparison_definitions(repo: &Repo, input: &Input) -> Result<BTreeMap<String, Probe>> {
    if source_status(repo, input) == "available" {
        return definitions(repo, input);
    }
    let mut definitions = BTreeMap::new();
    for (_, record) in active_checks(repo, input)?.into_values() {
        let check = &record.data["check"];
        if let Ok(manifest) = serde_json::from_value::<BTreeMap<String, Probe>>(check["probes"].clone()) {
            for (name, probe) in manifest {
                if probe.validate(&name).is_ok() {
                    definitions.insert(name, probe);
                }
            }
        }
        let Some(name) = check["probe"].as_str() else { continue };
        let Ok(probe) = serde_json::from_value::<Probe>(check["definition"].clone()) else { continue };
        if probe.validate(name).is_ok() {
            definitions.insert(name.into(), probe);
        }
    }
    Ok(definitions)
}

fn stored(repo: &Repo, input: &Input, name: &str, probe: &Probe) -> Result<Option<RunResult>> {
    if check_exit(repo, &input.subject.id, &probe.run)?.is_none() {
        return Ok(None);
    }
    let active = active_checks(repo, input)?;
    let root = repo.p.db.ref_value(&reference(name, &input.subject.id), crate::ids::now_ms())?;
    let mut latest = None;
    for (_, rec) in active.values() {
        let c = &rec.data["check"];
        if c["probe"].as_str() != Some(name) || c["definition"] != serde_json::to_value(probe)? {
            continue;
        }
        let Ok(mut r) = serde_json::from_value::<RunResult>(c.clone()) else { continue };
        if r.subject != input.subject || r.argv != probe.run || root.as_ref().map(|v| &v.target) != Some(&r.output_state) {
            continue;
        }
        let state = match repo.state(&r.output_state) {
            Ok(s) => s,
            Err(_) => continue,
        };
        if state.kind != StateKind::Derived
            || state.context.as_deref() != Some(&format!("probe-v1/{name}"))
            || state.derived_from.as_deref() != Some(&input.subject.id)
            || repo.state_root(&r.output_state).is_err()
        {
            continue;
        }
        r.reused = true;
        latest = Some(r);
    }
    Ok(latest)
}

pub fn run(ctx: &Ctx, repo: &Repo, input: &Input, selected: Option<&str>) -> Result<Vec<RunResult>> {
    repo.require(ctx, "try")?;
    shared_outputs(repo, &input.layers)?;
    if !input.layers.is_empty() {
        let _lock = repo.p.lock()?;
        repo.save_view(ctx, &input.view())?;
    }
    let definitions = comparison_definitions(repo, input)?;
    if definitions.is_empty() && source_status(repo, input) != "available" {
        bail!("{}; probe definitions are unavailable", source_status(repo, input));
    }
    if let Some(name) = selected {
        if !definitions.contains_key(name) {
            if source_status(repo, input) != "available" {
                bail!("{}; probe '{name}' has no retained definition", source_status(repo, input));
            }
            bail!("unknown probe '{name}' in the exact revision");
        }
    }
    let manifest = serde_json::to_value(&definitions)?;
    let mut results = Vec::new();
    for (name, probe) in definitions {
        if selected.is_some() && selected != Some(&name) {
            continue;
        }
        let lock_dir = repo.p.meta_dir().join("probe-locks").join(&name);
        std::fs::create_dir_all(&lock_dir)?;
        let execution_lock =
            std::fs::OpenOptions::new().create(true).write(true).truncate(false).open(lock_dir.join(&input.subject.id))?;
        rustix::fs::flock(&execution_lock, rustix::fs::FlockOperation::LockExclusive)?;
        if let Some(r) = stored(repo, input, &name, &probe)? {
            results.push(r);
            continue;
        }
        if source_status(repo, input) != "available" {
            bail!("{}; probe '{name}' has no retained result", source_status(repo, input));
        }
        let result = crate::cmd::admin::with_try_copy(ctx, repo, &input.state, &input.layers, false, |dir| {
            if source_status(repo, input) != "available" {
                bail!("{}", source_status(repo, input));
            }
            let env =
                if input.subject.kind == "view" { vec![("LAYR_TRY_VIEW".into(), input.subject.id.clone())] } else { vec![] };
            let code = crate::cmd::admin::try_command(ctx, repo, &input.state, &probe.run, dir, &env, true)?;
            // Freeze outputs before root reads them; the command may have left a child process.
            let snapshot = repo.temp_snapshot(dir, true)?;
            let work = repo.p.work_dir().join(crate::ids::new_id());
            crate::storage::create_subvolume(&work)?;
            let cleanup = Temp { path: work.clone() };
            collect(&Tree::open(&snapshot.path)?, &Tree::open(&work)?, &probe.outputs, code != 0)?;
            validate_outputs(&work, &probe)?;
            let _lock = repo.p.lock()?;
            let mut state =
                repo.new_state(ctx, &work, StateKind::Derived, vec![input.state.clone()], &format!("probe {name}"), None)?;
            state.derived_from = Some(input.subject.id.clone());
            state.context = Some(format!("probe-v1/{name}"));
            let result = RunResult {
                probe: name.clone(),
                subject: input.subject.clone(),
                argv: probe.run.clone(),
                exit: code,
                output_state: state.id.clone(),
                reused: false,
            };
            let mut check = serde_json::to_value(&result)?;
            check["command"] = json!(probe.run.join(" "));
            check["definition"] = serde_json::to_value(&probe)?;
            check["probes"] = manifest.clone();
            let payload = serde_json::to_vec(&check)?;
            let mut rec = if input.subject.kind == "view" {
                crate::cmd::attest::record_view(repo, &input.view(), "check", &payload)?
            } else {
                crate::cmd::attest::record(repo, &input.state, "check", &payload)?
            };
            rec.states.push(state.clone());
            rec.data["check"] = check;
            let key = reference(&name, &input.subject.id);
            rec.data["ref_change"] = serde_json::to_value(RefChange {
                name: key.clone(),
                before: repo.p.db.ref_value(&key, crate::ids::now_ms())?,
                after: Some(RefState { name: key, target: state.id, expires_at: None }),
            })?;
            repo.commit_record(ctx, rec)?;
            drop(cleanup);
            Ok(result)
        })?;
        results.push(result);
    }
    Ok(results)
}

fn collect(source: &Tree, dest: &Tree, paths: &[String], missing_ok: bool) -> Result<()> {
    fn copy(source: &Tree, dest: &Tree, path: &str) -> Result<()> {
        let meta = source.stat(path).ok_or_else(|| anyhow!("missing probe output '{path}'"))?;
        match meta.ftype {
            FType::Dir => {
                dest.mkdirs(path, None)?;
                for name in source.list(path)? {
                    copy(source, dest, &format!("{path}/{name}"))?;
                }
            }
            FType::File => {
                std::io::copy(&mut source.open_read(path)?, &mut dest.create(path, 0o600)?)?;
            }
            _ => bail!("probe output '{path}' must be a regular file or folder; links and special files are refused"),
        }
        Ok(())
    }
    for path in paths {
        if missing_ok && source.stat(path).is_none() {
            continue;
        }
        copy(source, dest, path)?;
    }
    Ok(())
}

#[derive(Clone, Debug, Serialize)]
struct FileData {
    size: u64,
    sha256: String,
    #[serde(skip)]
    text: Option<String>,
}
fn files(root: &Path) -> Result<BTreeMap<String, FileData>> {
    fn visit(tree: &Tree, path: &str, out: &mut BTreeMap<String, FileData>) -> Result<()> {
        let m = tree.stat(path).ok_or_else(|| anyhow!("missing probe output '{path}'"))?;
        if m.ftype == FType::Dir {
            for name in tree.list(path)? {
                visit(tree, &if path.is_empty() { name } else { format!("{path}/{name}") }, out)?;
            }
        } else if m.ftype == FType::File {
            let mut file = tree.open_read(path)?;
            let mut hash = Sha256::new();
            let mut bytes = Vec::new();
            let mut buf = [0; 65536];
            loop {
                let n = file.read(&mut buf)?;
                if n == 0 {
                    break;
                }
                hash.update(&buf[..n]);
                if m.size <= crate::fsutil::MAX_TEXT {
                    bytes.extend_from_slice(&buf[..n]);
                }
            }
            let text = if m.size <= crate::fsutil::MAX_TEXT && !crate::fsutil::is_binary(&bytes) {
                String::from_utf8(bytes).ok()
            } else {
                None
            };
            out.insert(path.into(), FileData { size: m.size, sha256: hex::encode(hash.finalize()), text });
        } else {
            bail!("invalid stored probe output '{path}'");
        }
        Ok(())
    }
    let mut out = BTreeMap::new();
    visit(&Tree::open(root)?, "", &mut out)?;
    Ok(out)
}
fn validate_outputs(root: &Path, probe: &Probe) -> Result<()> {
    let kind = probe.kind;
    if kind == Kind::Files {
        return Ok(());
    }
    if kind == Kind::Screenshots {
        return screenshots::validate(root, &files(root)?, probe);
    }
    for (path, file) in files(root)? {
        let text =
            file.text.as_deref().ok_or_else(|| anyhow!("probe output '{path}' needs JSON text within the text size limit"))?;
        let v: Value = serde_json::from_str(text).with_context(|| format!("invalid JSON probe output '{path}'"))?;
        if kind == Kind::Number {
            metrics(&v).with_context(|| format!("invalid number probe output '{path}'"))?;
        }
    }
    Ok(())
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
    quiet_noise: BTreeMap<String, f64>,
}
pub fn diff(repo: &Repo, base: &Input, head: &Input) -> Result<Behavior> {
    shared_outputs(repo, &base.layers)?;
    shared_outputs(repo, &head.layers)?;
    let old = comparison_definitions(repo, base)?;
    let new = comparison_definitions(repo, head)?;
    let names: BTreeSet<_> = old.keys().chain(new.keys()).collect();
    let mut probes = Vec::new();
    let mut quiet_noise = BTreeMap::new();
    for name in names {
        let a = old.get(name).map(|p| stored(repo, base, name, p)).transpose()?.flatten();
        let b = new.get(name).map(|p| stored(repo, head, name, p)).transpose()?.flatten();
        let (Some(a), Some(b)) = (&a, &b) else {
            probes.push(json!({"probe":name,"status":"not run","base_run":a.is_some(),"head_run":b.is_some()}));
            continue;
        };
        let p = &new[name];
        if old[name].kind != p.kind {
            probes.push(json!({"probe":name,"status":"kind changed","base":a,"head":b}));
            continue;
        }
        let base_root = repo.state_root(&a.output_state)?;
        let head_root = repo.state_root(&b.output_state)?;
        let before = files(&base_root)?;
        let after = files(&head_root)?;
        let changes = if p.kind == Kind::Screenshots {
            screenshots::compare(&base_root, &before, &head_root, &after, p)?
        } else {
            compare(&before, &after, p)?
        };
        if p.kind == Kind::Number
            && changes.is_empty()
            && before.iter().map(|(path, f)| (path, &f.sha256)).ne(after.iter().map(|(path, f)| (path, &f.sha256)))
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
        base: base.subject.id.clone(),
        head: head.subject.id.clone(),
        base_source: source_status(repo, base).into(),
        head_source: source_status(repo, head).into(),
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
pub fn print(ctx: &Ctx, behavior: &Behavior) {
    let views: BTreeSet<_> = behavior.base_views.iter().chain(&behavior.head_views).collect();
    if !views.is_empty() {
        outln!(
            ctx,
            "  results exist for views: {}; use layr probe diff views/<a> views/<b>",
            views.into_iter().cloned().collect::<Vec<_>>().join(", ")
        );
    }
    if behavior.base_source != "available" {
        outln!(ctx, "  base: {}", behavior.base_source);
    }
    if behavior.head_source != "available" {
        outln!(ctx, "  head: {}", behavior.head_source);
    }
    for p in &behavior.probes {
        let name = p["probe"].as_str().unwrap_or("");
        outln!(ctx, "  {}: {}", name, p["status"].as_str().unwrap_or(""));
        for error in p["errors"].as_array().into_iter().flatten() {
            outln!(ctx, "    {}: {}", error["side"].as_str().unwrap_or(""), error["error"].as_str().unwrap_or(""));
        }
        if p["exit_changed"] == true {
            outln!(ctx, "    exit {} → {}", p["base"]["exit"], p["head"]["exit"]);
        }
        if let Some(noise) = behavior.quiet_noise.get(name) {
            outln!(ctx, "    no change beyond noise ({}%)", noise * 100.0);
        }
        let mut json_rows = 0;
        for c in p["changes"].as_array().into_iter().flatten() {
            if c["kind"] == "screenshots" {
                outln!(ctx, "    {}: {}", name, screenshots::summary(c));
                continue;
            }
            let path = c["path"].as_str().unwrap_or("");
            if let Some(patch) = c["diff"].as_str() {
                let (mut added, mut removed) = (0, 0);
                for line in patch.lines().skip(2) {
                    if line.starts_with('+') {
                        added += 1;
                    } else if line.starts_with('-') {
                        removed += 1;
                    }
                }
                outln!(ctx, "    {path}   +{added} −{removed} lines");
            } else if c["kind"] == "binary" {
                let size = |v: &Value| v["size"].as_u64().map(|n| n.to_string()).unwrap_or_else(|| "(absent)".into());
                outln!(ctx, "    {path}   binary, {} → {}", size(&c["before"]), size(&c["after"]));
            } else {
                for row in c["changes"].as_array().into_iter().flatten() {
                    if let Some(metric) = row["metric"].as_str() {
                        let before = number_value(&row["before"]);
                        let after = number_value(&row["after"]);
                        let relative =
                            row["relative_change"].as_f64().map(|n| format!(" ({:+.0}%)", n * 100.0)).unwrap_or_default();
                        outln!(ctx, "    {path}   {metric} {before} → {after}{relative}");
                    } else {
                        json_rows += 1;
                        if json_rows <= 20 {
                            let key = row["key"].as_str().unwrap_or("");
                            let before = if row["change"] == "add" { "(absent)".into() } else { row["before"].to_string() };
                            let after = if row["change"] == "remove" { "(absent)".into() } else { row["after"].to_string() };
                            outln!(ctx, "    {path}   {key}: {before} → {after}");
                        }
                    }
                }
            }
        }
        if json_rows > 20 {
            outln!(ctx, "    … {} more", json_rows - 20);
        }
        for reason in p["limit_reasons"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            outln!(ctx, "    {reason}");
        }
    }
}

fn number_value(value: &Value) -> String {
    value.as_f64().map(|n| n.to_string()).unwrap_or_else(|| "(absent)".into())
}

/// Queue execution uses the same sandbox, attestations and output refs as explicit runs.
pub fn queue_enabled(repo: &Repo) -> Result<bool> {
    match repo.p.config_value("queue.probes")? {
        None | Some(Value::Null) => Ok(false),
        Some(Value::Bool(enabled)) => Ok(enabled),
        Some(_) => bail!("queue.probes must be true or false"),
    }
}

pub fn queue_run(ctx: &Ctx, repo: &Repo, base: &str, head: &str) -> Result<Value> {
    // These are exact shared states, even if a line name equals the state id.
    let input =
        |state: &str| Input { subject: Subject { kind: "state".into(), id: state.into() }, state: state.into(), layers: vec![] };
    let base = input(base);
    let head = input(head);
    let old = definitions(repo, &base)?;
    let new = definitions(repo, &head)?;
    let mut errors: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    // Run probes separately so one broken probe cannot hide the remaining outcomes.
    for (side, input, definitions) in [("base", &base, &old), ("head", &head, &new)] {
        for name in definitions.keys() {
            let error = match run(ctx, repo, input, Some(name)) {
                Ok(results) => results.first().filter(|r| r.exit != 0).map(|r| format!("exit {}", r.exit)),
                Err(error) => Some(format!("{error:#}")),
            };
            if let Some(error) = error {
                errors.entry(name.clone()).or_default().push(json!({"side":side,"subject":input.subject,"error":error}));
            }
        }
    }
    let mut behavior = diff(repo, &base, &head)?;
    let mut outcomes = Vec::new();
    for row in &mut behavior.probes {
        let name = row["probe"].as_str().context("probe comparison has no name")?.to_owned();
        let probe = new.get(&name).or_else(|| old.get(&name)).context("missing probe definition")?;
        let failures = errors.entry(name.clone()).or_default();
        let mut reasons = Vec::new();
        if let Some(limit) = probe.fail_over {
            match queue_limit_reasons(repo, &name, probe, row, limit) {
                Ok(growth) => reasons = growth,
                Err(error) => failures.push(json!({"side":"comparison","error":format!("{error:#}")})),
            }
            for failure in failures.iter() {
                reasons.push(format!(
                    "probe {name}: {}: {}",
                    failure["side"].as_str().unwrap_or(""),
                    failure["error"].as_str().unwrap_or("")
                ));
            }
        }
        if !failures.is_empty() {
            row["errors"] = json!(failures);
            row["status"] = json!("error");
        }
        let status = if !reasons.is_empty() {
            "failed"
        } else if probe.fail_over.is_none() {
            "observed"
        } else if row["status"] == "not run" || row["status"] == "kind changed" {
            "not compared"
        } else {
            "passed"
        };
        outcomes.push(json!({"probe":name,"status":status,"reasons":reasons,"errors":failures}));
    }
    let failed = outcomes.iter().any(|p| p["status"] == "failed");
    Ok(json!({"status":if failed {"failed"}else{"passed"},"probes":outcomes,"behavior":behavior}))
}

fn queue_limit_reasons(repo: &Repo, name: &str, probe: &Probe, row: &Value, limit: f64) -> Result<Vec<String>> {
    if probe.kind == Kind::Screenshots && row["status"] != "kind changed" {
        return Ok(screenshots::limit_reasons(name, row, limit));
    }
    let mut reasons = Vec::new();
    let (Some(a), Some(b)) = (row.get("base"), row.get("head")) else { return Ok(reasons) };
    let before = files(&repo.state_root(a["output_state"].as_str().context("missing base output")?)?)?;
    let after = files(&repo.state_root(b["output_state"].as_str().context("missing head output")?)?)?;
    // Limits ignore display noise. Check metrics before file sizes for a useful reason.
    if probe.kind == Kind::Number && row["status"] != "kind changed" {
        for (path, file) in &after {
            let read = |file: &FileData| -> Result<BTreeMap<String, f64>> {
                metrics(&serde_json::from_str(file.text.as_deref().context("stored metric is not text")?)?)
            };
            let old = before.get(path).map(read).transpose()?.unwrap_or_default();
            for (metric, value) in read(file)? {
                if let Some(reason) = over_limit(name, &metric, old.get(&metric).copied().unwrap_or(0.0), value, limit) {
                    reasons.push(reason);
                }
            }
        }
    }
    for (path, file) in &after {
        let old = before.get(path).map(|file| file.size).unwrap_or(0);
        if let Some(reason) = over_limit(name, path, old as f64, file.size as f64, limit) {
            reasons.push(reason);
        }
    }
    Ok(reasons)
}

fn over_limit(probe: &str, name: &str, before: f64, after: f64, limit: f64) -> Option<String> {
    if after <= before {
        return None;
    }
    let relative = if before == 0.0 { f64::INFINITY } else { (after - before) / before.abs() };
    if relative <= limit {
        return None;
    }
    let percent = if relative.is_infinite() { "+∞".into() } else { format!("{:+.0}", relative * 100.0) };
    Some(format!("probe {probe}: {name} {before} → {after} ({percent} %) is over the limit +{} %", limit * 100.0))
}

/// Compare the latest change version with its merge base against the target line.
pub fn change_behavior(repo: &Repo, state: &crate::model::StateRec) -> Result<Behavior> {
    // A queue compares the target head with the actual merge result, including target changes.
    for entry in repo.p.db.queue_entries(None)?.into_iter().rev() {
        if entry.state == state.id {
            if let Some(outcome) = entry.probes {
                return Ok(serde_json::from_value(outcome["behavior"].clone())?);
            }
        }
    }
    let source = state.line.as_deref().map(|id| repo.line(id)).transpose()?;
    let upstream =
        source.as_ref().map(|line| repo.p.config_value(&format!("line.{}.upstream", line.name))).transpose()?.flatten();
    let target = match &upstream {
        None | Some(Value::Null) => "main",
        Some(Value::String(name)) => name,
        _ => bail!("the line upstream must be a line name"),
    };
    let target = repo.line(target)?;
    let base = repo.merge_base(&target.head, &state.id)?.context("change and target have no common ancestor")?;
    let base = resolve(repo, None, &base)?;
    let head = resolve(repo, None, &state.id)?;
    let mut behavior = diff(repo, &base, &head)?;
    let has_base = behavior.probes.iter().any(|p| p["base_run"] == true || p["base"].is_object());
    let has_head = behavior.probes.iter().any(|p| p["head_run"] == true || p["head"].is_object());
    if !has_base {
        behavior.base_views = view_results(repo, &base.state)?;
    }
    if !has_head {
        behavior.head_views = view_results(repo, &head.state)?;
    }
    Ok(behavior)
}

fn view_results(repo: &Repo, shared: &str) -> Result<Vec<String>> {
    let mut views = BTreeSet::new();
    for record in repo.p.db.records(usize::MAX >> 1)? {
        if record.op != "view.save" || record.data["view"]["shared"].as_str() != Some(shared) {
            continue;
        }
        let Some(id) = record.data["view_id"].as_str() else { continue };
        let Ok(layers) = serde_json::from_value::<Vec<crate::overlay::Layer>>(record.data["view"]["layers"].clone()) else {
            continue;
        };
        let input = Input { subject: Subject { kind: "view".into(), id: id.into() }, state: shared.into(), layers };
        for (_, check) in active_checks(repo, &input)?.into_values() {
            let Some(name) = check.data["check"]["probe"].as_str() else { continue };
            let Ok(probe) = serde_json::from_value::<Probe>(check.data["check"]["definition"].clone()) else { continue };
            if probe.validate(name).is_ok() && stored(repo, &input, name, &probe)?.is_some() {
                views.insert(id.into());
                break;
            }
        }
    }
    Ok(views.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn definition(kind: Kind) -> Probe {
        Probe {
            run: vec!["/bin/true".into()],
            outputs: vec!["out".into()],
            kind,
            noise: 0.05,
            fail_over: None,
            tolerance: default_tolerance(),
            ignore_attributes: default_ignore_attributes(),
        }
    }
    #[test]
    fn private_layer_outputs_require_all_project_readers() {
        use crate::access::{Policy, Role};
        let project = Policy { access: BTreeMap::from([("group:10".into(), Role::Reader)]), ..Default::default() };
        let mut overlay = Policy { access: BTreeMap::from([("user:42".into(), Role::Admin)]), ..Default::default() };
        assert!(!readers_cover(&project, &overlay));
        overlay.access.insert("group:10".into(), Role::Reader);
        assert!(readers_cover(&project, &overlay));
        overlay.deny.insert("user:50".into(), vec!["read".into()]);
        assert!(!readers_cover(&project, &overlay));
        overlay.deny.clear();
        overlay.access.insert("*".into(), Role::Reader);
        assert!(readers_cover(&project, &overlay));
    }

    #[test]
    fn schema_requires_safe_outputs_and_argv() {
        let text = "[probe.test]\nrun=['echo','hello']\noutputs=['result']\n";
        assert_eq!(crate::oci::parse(text).unwrap().probe["test"].kind, Kind::Files);
        assert_eq!(crate::oci::parse(&format!("{text}fail_over=0.30\n")).unwrap().probe["test"].fail_over, Some(0.30));
        for tail in [
            "run=[]\noutputs=['out']",
            "run=['sh']\noutputs=['../escape']",
            "run=['sh']\noutputs=['.git/config']",
            "run=['sh']\noutputs=[]",
            "run=['sh']\noutputs=['out']\nkind='number'\nnoise=-1",
            "run=['sh']\noutputs=['out']\nunknown=true",
            "run=['sh']\noutputs=['out']\nfail_over=-0.1",
            "run=['sh']\noutputs=['out']\nfail_over=nan",
            "run=['sh']\noutputs=['out']\nfail_over=inf",
        ] {
            assert!(crate::oci::parse(&format!("[probe.test]\n{tail}")).is_err(), "{tail}");
        }
    }
    #[test]
    fn screenshot_settings_keep_existing_definition_identity() {
        let old = json!({"run":["true"],"outputs":["out"],"kind":"files","noise":0.05});
        let probe: Probe = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(probe.tolerance, 8);
        assert_eq!(serde_json::to_value(&probe).unwrap(), old);
        for tolerance in ["-1", "256", "1.5"] {
            assert!(crate::oci::parse(&format!(
                "[probe.screen]\nrun=['true']\noutputs=['out']\nkind='screenshots'\ntolerance={tolerance}"
            ))
            .is_err());
        }
        let probe = crate::oci::parse(
            "[probe.screen]\nrun=['true']\noutputs=['out']\nkind='screenshots'\ntolerance=0\nignore_attributes=[]",
        )
        .unwrap()
        .probe
        .remove("screen")
        .unwrap();
        assert_eq!(probe.tolerance, 0);
        assert!(probe.ignore_attributes.is_empty());
    }
    #[test]
    fn queue_limits_reject_only_growth_strictly_over_the_limit() {
        assert_eq!(
            over_limit("bench-checkout", "p95_ms", 41.0, 52.0, 0.2).unwrap(),
            "probe bench-checkout: p95_ms 41 → 52 (+27 %) is over the limit +20 %"
        );
        assert!(over_limit("p", "m", 100.0, 120.0, 0.2).is_none());
        assert!(over_limit("p", "m", 100.0, 99.0, 0.0).is_none());
        assert!(over_limit("p", "m", 0.0, 0.0, 0.2).is_none());
        assert!(over_limit("p", "m", 0.0, 1.0, 0.2).unwrap().contains("+∞ %"));
        assert!(over_limit("p", "m", -100.0, -79.0, 0.2).is_some());
        assert!(over_limit("p", "m", -100.0, -121.0, 0.2).is_none());
    }

    #[test]
    fn structural_json_diff_tracks_keys_arrays_and_null() {
        let a = json!({"a/b":{"~key":[1,null]},"removed":null});
        let b = json!({"a/b":{"~key":[2,null,3]},"added":null});
        let mut changes = Vec::new();
        json_changes(Some(&a), Some(&b), "", &mut changes);
        assert_eq!(changes.len(), 4);
        assert!(changes.iter().any(|c| c["key"] == "/a~1b/~0key/0" && c["after"] == 2));
        assert!(changes.iter().any(|c| c["key"] == "/removed" && c["change"] == "remove"));
        assert!(changes.iter().any(|c| c["key"] == "/added" && c["change"] == "add"));
    }
    #[test]
    fn metric_diff_applies_noise_and_handles_zero_baselines() {
        let a = json!({"quiet":100,"slow":100,"zero":0,"negative":-100,"gone":2});
        let b = json!({"quiet":104,"slow":110,"zero":1,"negative":-110,"new":4});
        let rows = number_changes(Some(&a), Some(&b), 0.05).unwrap();
        assert_eq!(rows.len(), 5);
        assert!(!rows.iter().any(|c| c["metric"] == "quiet"));
        assert_eq!(rows.iter().find(|c| c["metric"] == "slow").unwrap()["relative_change"], 0.1);
        assert_eq!(rows.iter().find(|c| c["metric"] == "negative").unwrap()["relative_change"], -0.1);
        assert!(rows.iter().find(|c| c["metric"] == "zero").unwrap()["relative_change"].is_null());
        assert!(metrics(&json!({"bad":"1"})).is_err());
    }
    #[test]
    fn file_diff_reports_hashes_for_binary_and_patches_for_text() {
        let file = |text: Option<&str>, hash: &str| FileData { size: 3, sha256: hash.into(), text: text.map(str::to_owned) };
        let old = BTreeMap::from([("text".into(), file(Some("old\n"), "a")), ("binary".into(), file(None, "b"))]);
        let new = BTreeMap::from([("text".into(), file(Some("new\n"), "c")), ("binary".into(), file(None, "d"))]);
        let changes = compare(&old, &new, &definition(Kind::Files)).unwrap();
        assert_eq!(changes[0]["kind"], "binary");
        assert_eq!(changes[0]["before"]["sha256"], "b");
        assert!(changes[1]["diff"].as_str().unwrap().contains("+new"));
    }
}
