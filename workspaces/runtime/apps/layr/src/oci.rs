//! Runnable snapshots as deterministic OCI images and Helm artifacts.

pub mod client;
pub mod registry;

use crate::fsutil::{self, FType};
use crate::repo::Repo;
use crate::tree::Tree;
use anyhow::{anyhow, bail, Context, Result};
use flate2::{Compression, GzBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// Transport boundary used by the generic remote dispatcher.
pub fn push(ctx: &crate::ctx::Ctx, url: &str, rev: &str, runnable: Option<&str>) -> Result<i32> {
    crate::cmd::oci::push(ctx, url, rev, runnable, false)
}

/// JSON output variant for a remote command with --json.
pub fn push_json(ctx: &crate::ctx::Ctx, url: &str, rev: &str, runnable: Option<&str>) -> Result<i32> {
    crate::cmd::oci::push(ctx, url, rev, runnable, true)
}

pub const MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub const CONFIG: &str = "application/vnd.oci.image.config.v1+json";
pub const LAYER: &str = "application/vnd.oci.image.layer.v1.tar+gzip";
pub const HELM_CONFIG: &str = "application/vnd.cncf.helm.config.v1+json";
pub const HELM_LAYER: &str = "application/vnd.cncf.helm.chart.content.v1.tar+gzip";

#[derive(Debug, Deserialize)]
pub struct ProjectFile {
    #[serde(default)]
    pub runnable: BTreeMap<String, Runnable>,
    #[serde(default)]
    pub probe: BTreeMap<String, crate::probes::Probe>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Runnable {
    pub layers: Vec<LayerSpec>,
    #[serde(default)]
    pub entrypoint: Vec<String>,
    #[serde(default)]
    pub cmd: Vec<String>,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default = "root_dir")]
    pub working_dir: String,
    #[serde(default)]
    pub ports: Vec<String>,
    pub platform: Platform,
    pub chart: Option<String>,
}

fn root_dir() -> String {
    "/".into()
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Platform {
    pub os: String,
    pub arch: String,
}

#[derive(Debug, Deserialize)]
#[serde(untagged)]
pub enum LayerSpec {
    Paths(Vec<String>),
    Detail(LayerPaths),
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LayerPaths {
    pub paths: Vec<String>,
    #[serde(default)]
    pub private: bool,
    #[serde(default)]
    pub secret: bool,
}

impl LayerSpec {
    fn paths(&self) -> &[String] {
        match self {
            Self::Paths(p) => p,
            Self::Detail(p) => &p.paths,
        }
    }
    fn restricted(&self) -> bool {
        matches!(self, Self::Detail(p) if p.private || p.secret)
    }
}

pub fn parse(text: &str) -> Result<ProjectFile> {
    let v: ProjectFile = toml::from_str(text).context("invalid layr.toml")?;
    if v.runnable.is_empty() && v.probe.is_empty() {
        bail!("layr.toml: define at least one [runnable.<name>] or [probe.<name>]");
    }
    for (name, r) in &v.runnable {
        valid_component(name).with_context(|| format!("runnable '{name}'"))?;
        if r.layers.is_empty() {
            bail!("runnable '{name}': layers must contain at least one path set");
        }
        if r.entrypoint.is_empty() && r.cmd.is_empty() {
            bail!("runnable '{name}': entrypoint or cmd is required");
        }
        if !["linux", "windows"].contains(&r.platform.os.as_str())
            || !["amd64", "arm64", "386", "arm", "ppc64le", "s390x", "riscv64"].contains(&r.platform.arch.as_str())
        {
            bail!("runnable '{name}': unsupported platform {}/{}; use OCI os/arch names", r.platform.os, r.platform.arch);
        }
        if !r.working_dir.starts_with('/') || r.working_dir.contains('\0') {
            bail!("runnable '{name}': working_dir must be an absolute container path");
        }
        for (key, val) in &r.env {
            if key.is_empty() || key.contains(['=', '\0']) || val.contains('\0') {
                bail!("runnable '{name}': invalid environment variable '{key}'");
            }
        }
        if r.entrypoint.iter().chain(&r.cmd).any(|s| s.contains('\0')) {
            bail!("runnable '{name}': command arguments cannot contain NUL");
        }
        for port in &r.ports {
            let (n, proto) = port.split_once('/').unwrap_or((port, "tcp"));
            if n.parse::<u16>().ok().filter(|n| *n > 0).is_none() || !["tcp", "udp", "sctp"].contains(&proto) {
                bail!("runnable '{name}': invalid exposed port '{port}'; use 8080/tcp");
            }
        }
        for layer in &r.layers {
            if layer.restricted() {
                bail!("runnable '{name}': OCI export refuses private or secret layers");
            }
            for p in layer.paths() {
                validate_path(p).with_context(|| format!("runnable '{name}' layer path"))?;
            }
        }
        if let Some(p) = &r.chart {
            validate_path(p).with_context(|| format!("runnable '{name}' chart"))?;
        }
    }
    for (name, probe) in &v.probe {
        probe.validate(name)?;
    }
    Ok(v)
}

fn validate_path(p: &str) -> Result<()> {
    if p.is_empty() || p.starts_with('/') || p.contains('\0') || p.split('/').any(|c| c == ".." || c.is_empty()) {
        bail!("invalid snapshot path '{p}'; use a relative path without '..'");
    }
    Ok(())
}

pub fn valid_component(s: &str) -> Result<()> {
    if s.is_empty()
        || s.len() > 128
        || !s.as_bytes()[0].is_ascii_lowercase() && !s.as_bytes()[0].is_ascii_digit()
        || !s.bytes().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || b"._-".contains(&c))
    {
        bail!("invalid registry path component '{s}'; use lowercase letters, digits, '.', '_' or '-'");
    }
    Ok(())
}

pub fn digest(data: &[u8]) -> String {
    format!("sha256:{}", hex::encode(Sha256::digest(data)))
}

pub fn digest_hex(s: &str) -> Result<&str> {
    let h = s.strip_prefix("sha256:").ok_or_else(|| anyhow!("unsupported digest"))?;
    if h.len() != 64 || !h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        bail!("invalid sha256 digest");
    }
    Ok(h)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(rename = "mediaType")]
    pub media_type: String,
    pub digest: String,
    pub size: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Artifact {
    pub manifest: Descriptor,
    pub config: Descriptor,
    pub layers: Vec<Descriptor>,
    pub diff_ids: Vec<String>,
}

#[derive(Clone)]
pub struct Cache {
    pub root: PathBuf,
    states: Option<PathBuf>,
}

impl Cache {
    pub fn open(root: &Path) -> Result<Self> {
        for d in ["blobs", "layers", "manifests", "derived"] {
            fs::create_dir_all(root.join(d))?;
        }
        Ok(Self { root: root.to_owned(), states: None })
    }
    pub fn for_repo(repo: &Repo) -> Result<Self> {
        let mut c = Self::open(&repo.p.meta_dir().join("oci"))?;
        c.states = Some(repo.p.states_dir());
        Ok(c)
    }
    pub fn blob_path(&self, hash: &str) -> Result<PathBuf> {
        let key = digest_hex(hash)?;
        if let Some(states) = &self.states {
            if let Ok(id) = fs::read_to_string(self.root.join("derived").join(key)) {
                if uuid::Uuid::parse_str(&id).is_ok() {
                    let p = states.join(id).join("blob");
                    if fs::symlink_metadata(&p).map(|m| m.is_file()).unwrap_or(false) {
                        return Ok(p);
                    }
                }
            }
        }
        Ok(self.root.join("blobs").join(key))
    }
    pub fn put(&self, media: &str, bytes: &[u8]) -> Result<Descriptor> {
        let d = Descriptor { media_type: media.into(), digest: digest(bytes), size: bytes.len() as u64 };
        let path = self.blob_path(&d.digest)?;
        if !path.exists() {
            fsutil::atomic_write(&path, bytes, 0o600)?;
        }
        Ok(d)
    }
    pub fn open_blob(&self, hash: &str) -> Result<File> {
        let p = self.blob_path(hash)?;
        Tree::open(p.parent().context("blob has no parent")?)?
            .open_read(p.file_name().and_then(|s| s.to_str()).context("invalid blob filename")?)
    }
    pub fn read(&self, d: &Descriptor) -> Result<Vec<u8>> {
        let mut bytes = Vec::new();
        self.open_blob(&d.digest)?.read_to_end(&mut bytes)?;
        Ok(bytes)
    }
}

#[derive(Clone, Serialize)]
struct Entry {
    path: String,
    mode: u32,
    kind: u8,
    size: u64,
    // File content digest, or the link target. No host metadata enters the cache key.
    content: String,
}

fn entries(tree: &Tree) -> Result<Vec<Entry>> {
    fn visit(tree: &Tree, at: &str, out: &mut Vec<Entry>) -> Result<()> {
        let dir = fsutil::safe_join(&tree.path, at)?;
        let mut names = fs::read_dir(dir)?
            .map(|entry| entry?.file_name().into_string().map_err(|_| anyhow!("OCI export requires UTF-8 file names")))
            .collect::<Result<Vec<_>>>()?;
        names.sort();
        for name in names {
            let p = if at.is_empty() { name } else { format!("{at}/{name}") };
            if crate::repo::skip_path(&p) {
                continue;
            }
            let m = fsutil::lmeta(&tree.path, &p).ok_or_else(|| anyhow!("cannot read snapshot entry '{p}'"))?;
            let (kind, size, content) = match m.ftype {
                FType::Dir => (b'5', 0, String::new()),
                FType::Symlink => (
                    b'2',
                    0,
                    fs::read_link(fsutil::safe_join(&tree.path, &p)?)?
                        .into_os_string()
                        .into_string()
                        .map_err(|_| anyhow!("OCI export requires UTF-8 symbolic-link targets"))?,
                ),
                FType::File => {
                    let mut h = Sha256::new();
                    std::io::copy(&mut tree.open_read(&p)?, &mut HashSink(&mut h))?;
                    (b'0', m.size, hex::encode(h.finalize()))
                }
                _ => bail!("OCI export refuses special file '{p}'"),
            };
            if p.split('/').any(|c| c.starts_with(".wh.")) {
                bail!("OCI reserved whiteout path '{p}'");
            }
            out.push(Entry { path: p.clone(), mode: if kind == b'2' { 0o777 } else { m.mode }, kind, size, content });
            if m.is_dir() {
                visit(tree, &p, out)?;
            }
        }
        Ok(())
    }
    let mut out = Vec::new();
    visit(tree, "", &mut out)?;
    out.sort_by(|a, b| a.path.cmp(&b.path));
    Ok(out)
}

struct HashSink<'a>(&'a mut Sha256);
impl Write for HashSink<'_> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        self.0.update(b);
        Ok(b.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

struct HashWriter<W> {
    inner: W,
    hash: Sha256,
}
impl<W: Write> Write for HashWriter<W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(b)?;
        self.hash.update(&b[..n]);
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

#[derive(Serialize, Deserialize)]
struct CachedLayer {
    descriptor: Descriptor,
    diff_id: String,
}

fn select(all: &[Entry], paths: &[String]) -> Result<Vec<Entry>> {
    // An empty path set is an explicit empty base layer.
    if paths.is_empty() {
        return Ok(Vec::new());
    }
    let specs: Vec<String> = paths.iter().map(|p| if p == "." { "".into() } else { p.clone() }).collect();
    for p in &specs {
        if !p.is_empty() && !all.iter().any(|e| crate::cmd::matches(std::slice::from_ref(p), &e.path)) {
            bail!("runnable layer path '{p}' matches no snapshot entries");
        }
    }
    let mut keep = BTreeSet::new();
    for e in all {
        if crate::cmd::matches(&specs, &e.path) {
            keep.insert(e.path.clone());
            let mut p = Path::new(&e.path).parent();
            while let Some(parent) = p.filter(|p| !p.as_os_str().is_empty()) {
                keep.insert(parent.to_string_lossy().into_owned());
                p = parent.parent();
            }
        }
    }
    Ok(all.iter().filter(|e| keep.contains(&e.path)).cloned().collect())
}

fn pack(
    tree: &Tree,
    selected: &[Entry],
    replacements: &BTreeMap<String, Vec<u8>>,
    cache: &Cache,
    media: &str,
    rename: Option<(&str, &str)>,
) -> Result<CachedLayer> {
    let key = digest(&serde_json::to_vec(&("oci-layer-v1", selected, replacements, rename, media))?);
    let record = cache.root.join("layers").join(digest_hex(&key)?);
    if let Ok(data) = fs::read(&record) {
        if let Ok(layer) = serde_json::from_slice::<CachedLayer>(&data) {
            if cache.blob_path(&layer.descriptor.digest)?.is_file() {
                return Ok(layer);
            }
        }
    }
    let tmp = cache.root.join(format!("layer-{}", crate::ids::new_id()));
    let result = (|| -> Result<CachedLayer> {
        let output = File::create(&tmp)?;
        let compressed = HashWriter { inner: output, hash: Sha256::new() };
        let gz = GzBuilder::new().mtime(0).operating_system(255).write(compressed, Compression::new(6));
        let uncompressed = HashWriter { inner: gz, hash: Sha256::new() };
        let mut tar = tar::Builder::new(uncompressed);
        for e in selected {
            let tar_path = match rename {
                Some((from, to)) => {
                    format!("{to}{}", e.path.strip_prefix(from).ok_or_else(|| anyhow!("chart path outside its folder"))?)
                }
                None => e.path.clone(),
            };
            let mut h = tar::Header::new_gnu();
            h.set_uid(0);
            h.set_gid(0);
            h.set_mtime(0);
            h.set_mode(e.mode);
            h.set_entry_type(tar::EntryType::new(e.kind));
            h.set_size(replacements.get(&e.path).map(|v| v.len() as u64).unwrap_or(e.size));
            h.set_cksum();
            if e.kind == b'2' {
                tar.append_link(&mut h, &tar_path, &e.content)?;
            } else if let Some(data) = replacements.get(&e.path) {
                tar.append_data(&mut h, &tar_path, &data[..])?;
            } else if e.kind == b'0' {
                tar.append_data(&mut h, &tar_path, tree.open_read(&e.path)?)?;
            } else {
                tar.append_data(&mut h, &tar_path, std::io::empty())?;
            }
        }
        let uncompressed = tar.into_inner()?;
        let diff_id = format!("sha256:{}", hex::encode(uncompressed.hash.finalize()));
        let compressed = uncompressed.inner.finish()?;
        compressed.inner.sync_all()?;
        let size = compressed.inner.metadata()?.len();
        let hash = format!("sha256:{}", hex::encode(compressed.hash.finalize()));
        fs::rename(&tmp, cache.blob_path(&hash)?)?;
        let layer = CachedLayer { descriptor: Descriptor { media_type: media.into(), digest: hash, size }, diff_id };
        fsutil::atomic_write(&record, &serde_json::to_vec(&layer)?, 0o600)?;
        Ok(layer)
    })();
    if tmp.exists() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

fn manifest(cache: &Cache, config: Descriptor, layers: Vec<Descriptor>, diff_ids: Vec<String>) -> Result<Artifact> {
    let m = cache.put(
        MANIFEST,
        &serde_json::to_vec(&json!({"schemaVersion": 2, "mediaType": MANIFEST, "config": config, "layers": layers}))?,
    )?;
    Ok(Artifact { manifest: m, config, layers, diff_ids })
}

pub fn image(root: &Path, r: &Runnable, cache: &Cache, forbidden: &[String]) -> Result<Artifact> {
    let tree = Tree::open(root)?;
    let all = entries(&tree)?;
    let mut layers = Vec::new();
    let mut diff_ids = Vec::new();
    for spec in &r.layers {
        if spec.restricted() {
            bail!("OCI export refuses private or secret layers");
        }
        let selected = select(&all, spec.paths())?;
        for e in &selected {
            if forbidden.iter().any(|p| crate::access::path_matches(p, &e.path)) {
                bail!("OCI export refuses private or secret path '{}'", e.path);
            }
        }
        let layer = pack(&tree, &selected, &BTreeMap::new(), cache, LAYER, None)?;
        layers.push(layer.descriptor);
        diff_ids.push(layer.diff_id);
    }
    let env: Vec<_> = r.env.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let ports: BTreeMap<_, _> =
        r.ports.iter().map(|p| (if p.contains('/') { p.clone() } else { format!("{p}/tcp") }, json!({}))).collect();
    let config = cache.put(
        CONFIG,
        &serde_json::to_vec(&json!({
            "architecture": r.platform.arch, "os": r.platform.os,
            "config": {"Entrypoint": r.entrypoint, "Cmd": r.cmd, "Env": env, "WorkingDir": r.working_dir, "ExposedPorts": ports},
            "rootfs": {"type": "layers", "diff_ids": diff_ids}
        }))?,
    )?;
    manifest(cache, config, layers, diff_ids)
}

pub struct Export {
    pub image: Artifact,
    pub chart: Option<Artifact>,
    pub chart_version: Option<String>,
    pub cache: Cache,
    pub state: String,
    pub runnable: String,
    pub restrictions: String,
}

/// Only this boundary translates a revision into an exportable shared snapshot.
pub fn export(
    ctx: &crate::ctx::Ctx,
    repo: &Repo,
    rev: &str,
    line: Option<&crate::model::Line>,
    name: Option<&str>,
) -> Result<Export> {
    if repo.p.is_overlay() {
        bail!("OCI export refuses private overlays");
    }
    let id = crate::revs::resolve(repo, line, rev)?;
    let state = repo.state(&id)?;
    let named_line = repo.p.db.line_by_name(rev)?;
    if let Some(l) = named_line.as_ref().or_else(|| line.filter(|l| l.head == id)) {
        if !repo.layers(l)?.is_empty() {
            bail!("OCI export refuses a view with private overlays");
        }
    }
    if !state.conflicts.is_empty() {
        bail!("OCI export refuses a state with open conflicts");
    }
    if let Some(lid) = &state.line {
        if let Some(l) = repo.p.db.line_by_id(lid)? {
            if !repo.layers(&l)?.is_empty() {
                bail!("OCI export refuses a view with private overlays");
            }
        }
    }
    let root = repo.state_root(&id)?;
    let tree = Tree::open(&root)?;
    let mut text = String::new();
    tree.open_read("layr.toml").context("the revision has no readable layr.toml")?.read_to_string(&mut text)?;
    let file = parse(&text)?;
    if file.runnable.is_empty() {
        bail!("the revision has no runnable; define [runnable.<name>] in layr.toml");
    }
    let name = match name {
        Some(n) => n.to_string(),
        None if file.runnable.len() == 1 => file.runnable.keys().next().unwrap().clone(),
        None => bail!("the revision has multiple runnables; select one with --runnable <name>"),
    };
    let r = file.runnable.get(&name).ok_or_else(|| anyhow!("unknown runnable '{name}' in layr.toml"))?;
    let cache = Cache::for_repo(repo)?;
    let mut forbidden = repo.p.config_strings("secret.paths");
    forbidden.extend(repo.p.config_strings("oci.secret_paths"));
    let img = image(&root, r, &cache, &forbidden)?;
    let (chart, chart_version) = match &r.chart {
        Some(folder) => {
            let (c, v) = chart(&root, folder, &name, &img.manifest.digest, &cache, &forbidden)?;
            (Some(c), Some(v))
        }
        None => (None, None),
    };
    for layer in img.layers.iter().chain(chart.iter().flat_map(|c| c.layers.iter())) {
        register_layer(ctx, repo, &cache, &id, layer)?;
    }
    let restrictions = restriction_key(repo, &id)?;
    let e = Export { image: img, chart, chart_version, cache, state: id, runnable: name, restrictions };
    registry::remember(&e, rev)?;
    Ok(e)
}

/// The Core ref is the cache root. A local locator only avoids a database lookup for blob reads.
fn register_layer(ctx: &crate::ctx::Ctx, repo: &Repo, cache: &Cache, source: &str, layer: &Descriptor) -> Result<()> {
    use crate::model::{Record, RefChange, RefState, StateKind};
    let key = digest_hex(&layer.digest)?;
    let name = format!("derived/oci-layer-v1/{key}");
    let before = repo.p.db.ref_value(&name, crate::ids::now_ms())?;
    let id = if let Some(r) = &before {
        let state = repo.state(&r.target)?;
        if state.kind != StateKind::Derived || state.context.as_deref() != Some("oci-layer-v1") {
            bail!("OCI cache ref '{name}' does not name an OCI derived state");
        }
        let tree = Tree::open(&repo.state_root(&state.id)?)?;
        let blob = tree.open_read("blob").context("OCI derived state needs a regular layer blob")?;
        let stored: Descriptor = serde_json::from_reader(tree.open_read("descriptor.json")?)?;
        if stored.digest != layer.digest || blob.metadata()?.len() != layer.size {
            bail!("OCI derived cache descriptor does not match its content key");
        }
        state.id
    } else {
        let src = cache.blob_path(&layer.digest)?;
        let work = repo.p.work_dir().join(crate::ids::new_id());
        crate::storage::create_subvolume(&work)?;
        let cleanup = crate::repo::Temp { path: work.clone() };
        fsutil::copy_entry(
            src.parent().context("layer path has no parent")?,
            src.file_name().and_then(|s| s.to_str()).context("invalid layer filename")?,
            &work,
            "blob",
            None,
        )?;
        fsutil::atomic_write(&work.join("descriptor.json"), &serde_json::to_vec(layer)?, 0o600)?;
        let mut state = repo.new_state(ctx, &work, StateKind::Derived, vec![source.into()], "OCI layer", None)?;
        state.context = Some("oci-layer-v1".into());
        let id = state.id.clone();
        let mut rec = Record::new("derive");
        rec.states.push(state);
        rec.data = json!({"ref_change": RefChange { name: name.clone(), before, after: Some(RefState { name, target: id.clone(), expires_at: None }) }});
        repo.commit_record(ctx, rec)?;
        drop(cleanup);
        id
    };
    fsutil::atomic_write(&cache.root.join("derived").join(key), id.as_bytes(), 0o600)?;
    // The immutable Core state now owns the blob. Keep no second compressed copy in meta/oci.
    let staging = cache.root.join("blobs").join(key);
    if staging.is_file() {
        fs::remove_file(staging)?;
    }
    Ok(())
}

/// Cached exports lose access when the source view or secret policy becomes more restrictive.
pub fn restriction_key(repo: &Repo, state: &str) -> Result<String> {
    let st = repo.state(state)?;
    let mut layers = BTreeMap::new();
    for l in repo.p.db.lines()? {
        if l.head == state || st.line.as_deref() == Some(&l.id) {
            let stack = repo.layers(&l)?;
            if !stack.is_empty() {
                layers.insert(l.id, stack);
            }
        }
    }
    Ok(digest(&serde_json::to_vec(&(
        repo.p.is_overlay(),
        repo.p.config_strings("secret.paths"),
        repo.p.config_strings("oci.secret_paths"),
        layers,
    ))?))
}

fn yaml_read(tree: &Tree, p: &str) -> Result<Value> {
    let f = tree.open_read(p).with_context(|| format!("chart needs '{p}'"))?;
    Ok(serde_yaml::from_reader(f).with_context(|| format!("invalid chart YAML '{p}'"))?)
}

pub fn semver(v: &str) -> bool {
    // SemVer core and optional prerelease/build identifiers. Reject leading zeroes in numeric identifiers.
    let re = regex::Regex::new(r"^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)(-[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?(\+[0-9A-Za-z-]+(\.[0-9A-Za-z-]+)*)?$").unwrap();
    re.is_match(v)
        && v.split('+')
            .next()
            .unwrap()
            .split_once('-')
            .map(|(_, p)| p.split('.').all(|i| !i.bytes().all(|b| b.is_ascii_digit()) || i == "0" || !i.starts_with('0')))
            .unwrap_or(true)
}

fn chart(root: &Path, folder: &str, name: &str, image: &str, cache: &Cache, forbidden: &[String]) -> Result<(Artifact, String)> {
    validate_path(folder)?;
    let source = Tree::open(root)?;
    // Tree::stat and all reads below refuse links in parent folders.
    let chart_file = format!("{folder}/Chart.yaml");
    let metadata = yaml_read(&source, &chart_file)?;
    if metadata.get("name").and_then(Value::as_str) != Some(name) {
        bail!("Chart.yaml name must equal runnable name '{name}'");
    }
    let version = metadata
        .get("version")
        .and_then(Value::as_str)
        .filter(|v| semver(v))
        .ok_or_else(|| anyhow!("Chart.yaml needs a SemVer version"))?
        .to_string();
    let values_path = format!("{folder}/values.yaml");
    let mut values = yaml_read(&source, &values_path)?;
    let map = values.as_object_mut().ok_or_else(|| anyhow!("chart values.yaml must contain a mapping"))?;
    let img = map
        .entry("image")
        .or_insert_with(|| json!({}))
        .as_object_mut()
        .ok_or_else(|| anyhow!("values.yaml image must be a mapping"))?;
    img.insert("digest".into(), json!(image));
    let all = entries(&source)?;
    let mut selected = select(&all, &[folder.into()])?;
    selected.retain(|e| e.path != folder);
    for e in &selected {
        if forbidden.iter().any(|p| crate::access::path_matches(p, &e.path)) {
            bail!("OCI chart refuses private or secret path '{}'", e.path);
        }
        if e.kind == b'2' {
            bail!("OCI chart refuses symbolic link '{}'", e.path);
        }
    }
    let replacements = BTreeMap::from([(values_path, serde_yaml::to_string(&values)?.into_bytes())]);
    let l = pack(&source, &selected, &replacements, cache, HELM_LAYER, Some((folder, name)))?;
    let cfg = cache.put(HELM_CONFIG, &serde_json::to_vec(&metadata)?)?;
    Ok((manifest(cache, cfg, vec![l.descriptor], vec![])?, version))
}

/// Single resolver hook. Core extends revs::resolve for refs, changes and exact views.
pub fn resolve_tag(repo: &Repo, tag: &str) -> Result<String> {
    let rev = tag_revision(tag)?;
    crate::revs::resolve(repo, None, &rev)
}

pub fn tag_revision(tag: &str) -> Result<String> {
    if let Some(n) = tag.strip_prefix("state-") {
        return Ok(n.into());
    }
    if let Some(n) = tag.strip_prefix("line-") {
        return Ok(n.into());
    }
    if let Some(id) = tag.strip_prefix("view-") {
        return Ok(format!("views/{id}"));
    }
    if let Some(id) = tag.strip_prefix("change-") {
        return Ok(format!("changes/{id}"));
    }
    if tag.contains('_') && semver(&tag.replace('_', "+")) {
        return Ok(format!("release/{}", tag.replace('_', "+")));
    }
    if semver(tag) {
        return Ok(format!("release/{tag}"));
    }
    if !valid_tag(tag) {
        bail!("invalid OCI tag '{tag}'");
    }
    Ok(format!("env/{tag}"))
}

pub fn valid_tag(tag: &str) -> bool {
    !tag.is_empty()
        && tag.len() <= 128
        && tag.bytes().all(|c| c.is_ascii_alphanumeric() || b"_.-".contains(&c))
        && !tag.starts_with(['.', '-'])
}

pub fn revision_tag(repo: &Repo, rev: &str, state: &str) -> Result<String> {
    let rev = rev.trim_start_matches("refs/");
    let tag = if let Some(n) = rev.strip_prefix("env/").or_else(|| rev.strip_prefix("release/")) {
        n.to_owned()
    } else if let Some(n) = rev.strip_prefix("changes/") {
        format!("change-{n}")
    } else if let Some(n) = rev.strip_prefix("views/") {
        format!("view-{n}")
    } else if let Some(l) = repo.p.db.line_by_name(rev)? {
        format!("line-{}", l.name)
    } else {
        format!("state-{state}")
    };
    if !valid_tag(&tag) {
        bail!("revision '{rev}' has no valid OCI tag; push its state id instead");
    }
    Ok(tag)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct Tmp(PathBuf);
    impl Tmp {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("layr-oci-{}", crate::ids::new_id()));
            fs::create_dir_all(&p).unwrap();
            Self(p)
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    const RUNNABLE: &str = r#"
[runnable.app]
layers = [["bin"], ["etc"]]
entrypoint = ["/bin/app"]
cmd = ["serve"]
working_dir = "/srv"
ports = ["8080/tcp", "5353/udp"]
env = { Z = "last", A = "first" }
platform = { os = "linux", arch = "amd64" }
"#;

    fn fixture(root: &Path, reverse: bool) {
        for p in if reverse { ["etc", "bin"] } else { ["bin", "etc"] } {
            fs::create_dir_all(root.join(p)).unwrap();
        }
        fs::write(root.join("bin/app"), b"#!/bin/sh\necho hello\n").unwrap();
        fs::set_permissions(root.join("bin/app"), fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(root.join("etc/config"), b"answer=42\n").unwrap();
        for p in ["bin", "etc"] {
            fs::set_permissions(root.join(p), fs::Permissions::from_mode(0o755)).unwrap();
        }
        fs::set_permissions(root.join("etc/config"), fs::Permissions::from_mode(0o644)).unwrap();
        std::os::unix::fs::symlink("app", root.join("bin/link")).unwrap();
    }

    #[test]
    fn schema_errors_are_specific() {
        assert_eq!(parse(RUNNABLE).unwrap().runnable.len(), 1);
        for (text, error) in [
            (RUNNABLE.replace("layers = [[\"bin\"], [\"etc\"]]", "layers = []"), "layers"),
            (RUNNABLE.replace("\"bin\"", "\"../secret\""), "invalid snapshot path"),
            (RUNNABLE.replace("amd64", "x86_64"), "platform"),
            (RUNNABLE.replace("8080/tcp", "0/tcp"), "port"),
            (
                RUNNABLE.replace("layers = [[\"bin\"], [\"etc\"]]", "layers = [{ paths = [\"bin\"], private = true }]"),
                "private or secret",
            ),
            (
                RUNNABLE.replace("layers = [[\"bin\"], [\"etc\"]]", "layers = [{ paths = [\"bin\"], secret = true }]"),
                "private or secret",
            ),
            (RUNNABLE.replace("working_dir", "workdir"), "unknown field"),
        ] {
            assert!(format!("{:#}", parse(&text).unwrap_err()).contains(error), "{error}");
        }
    }

    #[test]
    fn deterministic_fixture() {
        let a = Tmp::new();
        let b = Tmp::new();
        fixture(&a.0, false);
        fixture(&b.0, true);
        let later = std::time::UNIX_EPOCH + std::time::Duration::from_secs(123456789);
        File::options()
            .write(true)
            .open(b.0.join("bin/app"))
            .unwrap()
            .set_times(std::fs::FileTimes::new().set_modified(later))
            .unwrap();
        let r = parse(RUNNABLE).unwrap().runnable.remove("app").unwrap();
        let ca = Cache::open(&a.0.join("cache")).unwrap();
        let cb = Cache::open(&b.0.join("cache")).unwrap();
        let ia = image(&a.0, &r, &ca, &[]).unwrap();
        let ib = image(&b.0, &r, &cb, &[]).unwrap();
        assert_eq!(ia.manifest.digest, ib.manifest.digest);
        assert_eq!(ia.layers.len(), 2);
        // Run this fixture unchanged on macOS and Linux to verify cross-machine identity.
        println!("deterministic_fixture={}", ia.manifest.digest);
        assert_eq!(ia.manifest.digest, "sha256:55ed1d514d29902527747dc430ad4642f9ff76739d57ff6be6aa0511fc90429e");
        let compressed = ca.read(&ia.layers[0]).unwrap();
        assert_eq!(&compressed[4..8], &[0, 0, 0, 0]);
        assert_eq!(compressed[9], 255);
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&compressed[..]));
        let mut names = Vec::new();
        for e in archive.entries().unwrap() {
            let e = e.unwrap();
            assert_eq!(e.header().mtime().unwrap(), 0);
            assert_eq!(e.header().uid().unwrap(), 0);
            assert_eq!(e.header().gid().unwrap(), 0);
            names.push(e.path().unwrap().into_owned());
        }
        assert_eq!(names, ["bin", "bin/app", "bin/link"].map(PathBuf::from));
        let before = fs::metadata(ca.blob_path(&ia.layers[0].digest).unwrap()).unwrap().modified().unwrap();
        let again = image(&a.0, &r, &ca, &[]).unwrap();
        assert_eq!(again.manifest.digest, ia.manifest.digest);
        assert_eq!(fs::metadata(ca.blob_path(&ia.layers[0].digest).unwrap()).unwrap().modified().unwrap(), before);
        fs::write(a.0.join("etc/config"), "changed\n").unwrap();
        let changed = image(&a.0, &r, &ca, &[]).unwrap();
        assert_eq!(changed.layers[0].digest, ia.layers[0].digest);
        assert_ne!(changed.layers[1].digest, ia.layers[1].digest);
        assert!(image(&a.0, &r, &ca, &["etc".into()]).unwrap_err().to_string().contains("private or secret"));
    }

    #[test]
    fn chart_binds_the_exact_image() {
        let t = Tmp::new();
        let dir = t.0.join("deployment");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("Chart.yaml"), "apiVersion: v2\nname: app\nversion: 1.2.3\n").unwrap();
        fs::write(dir.join("values.yaml"), "image:\n  repository: example/app\n  digest: old\n").unwrap();
        let c = Cache::open(&t.0.join("cache")).unwrap();
        let (a, v) = chart(&t.0, "deployment", "app", "sha256:exact", &c, &[]).unwrap();
        assert_eq!(v, "1.2.3");
        assert_eq!(a.config.media_type, HELM_CONFIG);
        assert_eq!(a.layers[0].media_type, HELM_LAYER);
        let bytes = c.read(&a.layers[0]).unwrap();
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
        let mut found = false;
        for e in tar.entries().unwrap() {
            let mut e = e.unwrap();
            if e.path().unwrap() == Path::new("app/values.yaml") {
                let mut s = String::new();
                e.read_to_string(&mut s).unwrap();
                assert_eq!(serde_yaml::from_str::<Value>(&s).unwrap()["image"]["digest"], "sha256:exact");
                found = true;
            }
        }
        assert!(found);
        assert_eq!(chart(&t.0, "deployment", "app", "sha256:exact", &c, &[]).unwrap().0.manifest.digest, a.manifest.digest);
        assert_ne!(chart(&t.0, "deployment", "app", "sha256:new", &c, &[]).unwrap().0.manifest.digest, a.manifest.digest);
        assert!(chart(&t.0, "deployment", "app", "sha256:exact", &c, &["deployment/values.yaml".into()]).is_err());
    }

    #[test]
    fn symlinks_never_read_their_targets_and_long_links_work() {
        let t = Tmp::new();
        let external = Tmp::new();
        fs::write(external.0.join("secret"), "secret bytes must stay outside").unwrap();
        fs::create_dir_all(t.0.join("bin")).unwrap();
        fs::create_dir_all(t.0.join("etc")).unwrap();
        std::os::unix::fs::symlink(&external.0, t.0.join("bin/escape")).unwrap();
        let long = "x".repeat(150);
        std::os::unix::fs::symlink(&long, t.0.join("bin/long")).unwrap();
        let r = parse(RUNNABLE).unwrap().runnable.remove("app").unwrap();
        let cache = Cache::open(&t.0.join("cache")).unwrap();
        let a = image(&t.0, &r, &cache, &[]).unwrap();
        let bytes = cache.read(&a.layers[0]).unwrap();
        let mut archive = tar::Archive::new(flate2::read::GzDecoder::new(&bytes[..]));
        let mut found = false;
        for entry in archive.entries().unwrap() {
            let entry = entry.unwrap();
            if entry.path().unwrap() == Path::new("bin/long") {
                assert_eq!(entry.link_name().unwrap().unwrap(), Path::new(&long));
                found = true;
            }
            assert!(!entry.path().unwrap().starts_with("bin/escape/secret"));
        }
        assert!(found);
        let mut r = r;
        r.layers = vec![LayerSpec::Paths(vec!["bin/escape/secret".into()])];
        assert!(image(&t.0, &r, &cache, &[]).is_err());
    }

    // APFS rejects this filename before export; Linux permits it.
    #[cfg(target_os = "linux")]
    #[test]
    fn non_utf8_paths_are_rejected_without_lossy_conversion() {
        use std::os::unix::ffi::OsStringExt;
        let t = Tmp::new();
        fixture(&t.0, false);
        let r = parse(RUNNABLE).unwrap().runnable.remove("app").unwrap();
        let cache = Cache::open(&t.0.join("cache")).unwrap();
        let invalid = t.0.join("bin").join(std::ffi::OsString::from_vec(vec![0xff]));
        fs::write(&invalid, "must not be omitted").unwrap();
        assert!(image(&t.0, &r, &cache, &[]).unwrap_err().to_string().contains("UTF-8 file names"));
        fs::remove_file(invalid).unwrap();
        let target = std::ffi::OsString::from_vec(vec![0xff]);
        std::os::unix::fs::symlink(target, t.0.join("bin/invalid-link")).unwrap();
        assert!(image(&t.0, &r, &cache, &[]).unwrap_err().to_string().contains("UTF-8 symbolic-link targets"));
    }

    #[test]
    fn tag_mapping_and_semver() {
        for (tag, rev) in [
            ("prod", "env/prod"),
            ("staging", "env/staging"),
            ("1.2.3", "release/1.2.3"),
            ("line-main", "main"),
            ("change-abc", "changes/abc"),
            ("view-abc", "views/abc"),
            ("state-abc", "abc"),
        ] {
            assert_eq!(tag_revision(tag).unwrap(), rev);
        }
        for v in ["1.2.3", "0.0.0", "1.2.3-rc.1+build.2"] {
            assert!(semver(v), "{v}");
        }
        for v in ["01.2.3", "1.2", "v1.2.3", "1.2.3-01"] {
            assert!(!semver(v), "{v}");
        }
        for v in ["../prod", "line-a/b", "", ".x", "-x"] {
            assert!(!valid_tag(v), "{v}");
        }
    }
}
