//! Read-only OCI Distribution endpoints, served by the daemon's request processes.

use super::{Artifact, Cache, Export, MANIFEST};
use crate::ctx::{Caller, Ctx};
use crate::repo::Repo;
use crate::store::{Project, Store};
use anyhow::{anyhow, bail, Context, Result};
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::json;
use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::time::Duration;

#[derive(Clone, Copy)]
pub struct Identity {
    pub uid: u32,
    pub gid: u32,
}

fn identity(p: &Project) -> Result<Identity> {
    let name = p.config_value("registry.user")?.and_then(|v| v.as_str().map(str::to_owned)).unwrap_or_else(|| "nobody".into());
    let user = crate::sys::user_by_name(&name).ok_or_else(|| anyhow!("registry.user '{name}' does not exist"))?;
    if user.uid == 0 {
        bail!("registry.user must be unprivileged");
    }
    if user.gid == 0 {
        bail!("registry.user must use an unprivileged primary group");
    }
    Ok(Identity { uid: user.uid, gid: user.gid })
}

/// Configuration is read at daemon start. Projects on one listener share its unprivileged user.
pub fn listeners(root: &Path) -> Result<Vec<(String, TcpListener, Identity)>> {
    let store = Store::open(root)?;
    let mut names = BTreeMap::new();
    for name in store.project_names()? {
        let p = Project::open(&store, &name)?;
        if let Some(v) = p.config_value("registry.listen")? {
            let addr = v.as_str().ok_or_else(|| anyhow!("registry.listen must be a socket address"))?;
            addr.parse::<SocketAddr>().context("registry.listen needs an IP address and port")?;
            let user = identity(&p)?;
            if let Some(old) = names.insert(addr.to_string(), user) {
                if old.uid != user.uid || old.gid != user.gid {
                    bail!("projects on registry.listen {addr} must use the same registry.user");
                }
            }
        }
    }
    let mut out = Vec::new();
    for (name, user) in names {
        match crate::sys::fork()? {
            crate::sys::Fork::Child => {
                std::process::exit(if crate::sys::drop_to_user(user.uid, user.gid).is_ok() { 0 } else { 1 })
            }
            crate::sys::Fork::Parent(pid) => {
                let (_, status) =
                    rustix::process::waitpid(rustix::process::Pid::from_raw(pid), rustix::process::WaitOptions::empty())?
                        .context("registry privilege check failed")?;
                if status.exit_status() != Some(0) {
                    bail!("cannot drop registry privileges");
                }
            }
        }
        let l = TcpListener::bind(&name).with_context(|| format!("registry.listen {name}"))?;
        l.set_nonblocking(true)?;
        eprintln!("layr registry: listening on {} (uid {})", l.local_addr()?, user.uid);
        out.push((name, l, user));
    }
    Ok(out)
}

/// Exec clears the daemon's memory and closes its descriptors before HTTP parsing starts.
pub fn exec_http(stream: TcpStream, socket: &Path, listen: &str, user: Identity) -> Result<()> {
    use std::os::unix::process::CommandExt;
    crate::sys::drop_to_user(user.uid, user.gid)?;
    let fd: std::os::fd::OwnedFd = stream.into();
    let error = std::process::Command::new(std::env::current_exe()?)
        .arg("__registry-http")
        .arg(socket)
        .arg(listen)
        .stdin(std::process::Stdio::from(fd))
        .stdout(std::process::Stdio::null())
        .env_clear()
        .current_dir("/")
        .exec();
    Err(error).context("cannot execute registry HTTP worker; registry.user needs access to the executable and its parent folders")
}

#[derive(Debug, Serialize, Deserialize)]
struct Stored {
    artifact: Artifact,
    state: String,
    revision: String,
    restrictions: String,
}

fn index(cache: &Cache, runnable: &str, chart: bool) -> std::path::PathBuf {
    cache.root.join("manifests").join(if chart { "charts" } else { "images" }).join(runnable)
}

pub fn remember(e: &Export, revision: &str) -> Result<()> {
    for (chart, artifact) in [(false, Some(&e.image)), (true, e.chart.as_ref())] {
        if let Some(a) = artifact {
            let dir = index(&e.cache, &e.runnable, chart);
            fs::create_dir_all(&dir)?;
            let stored = Stored {
                artifact: a.clone(),
                state: e.state.clone(),
                revision: revision.into(),
                restrictions: e.restrictions.clone(),
            };
            crate::fsutil::atomic_write(&dir.join(super::digest_hex(&a.manifest.digest)?), &serde_json::to_vec(&stored)?, 0o600)?;
        }
    }
    Ok(())
}

#[derive(Deserialize, Serialize)]
pub struct TokenRecord {
    pub uid: u32,
    pub machine: String,
    pub until: i64,
}

fn token_uid(p: &Project, token: &str) -> Result<Option<u32>> {
    let tokens: BTreeMap<String, TokenRecord> =
        p.config_value("registry.tokens")?.map(serde_json::from_value).transpose()?.unwrap_or_default();
    let hash = super::digest(token.as_bytes());
    Ok(tokens.get(&hash).filter(|r| r.until > crate::ids::now_ms() && r.machine == p.store.machine.id).map(|r| r.uid))
}

struct Request {
    method: String,
    target: String,
    authorization: Option<String>,
}

fn request(stream: &TcpStream) -> Result<Request> {
    let limits = layr_store::http::Limits {
        request_line: 4096,
        header_count: 64,
        header_bytes: 16 * 1024,
        body_bytes: 0,
        header_timeout: Duration::from_secs(10),
        body_timeout: Duration::from_secs(10),
        write_timeout: Duration::from_secs(60),
    };
    limits.configure(stream)?;
    let mut stream = layr_store::http::Socket::new(stream.try_clone()?, limits.header_timeout);
    let parsed = layr_store::http::request(&mut stream, &limits)?;
    let authorization = parsed.header("authorization").map(str::to_string);
    Ok(Request { method: parsed.method, target: parsed.target, authorization })
}

fn token(header: Option<&str>, basic: bool) -> Option<String> {
    let h = header?;
    if let Some(t) = h.strip_prefix("Bearer ") {
        return Some(t.into());
    }
    if basic {
        let raw = base64::engine::general_purpose::STANDARD.decode(h.strip_prefix("Basic ")?).ok()?;
        let text = String::from_utf8(raw).ok()?;
        return Some(text.split_once(':')?.1.into());
    }
    None
}

fn context(root: &Path, uid: u32) -> Result<Ctx> {
    Ok(Ctx {
        store: Store::open(root)?,
        caller: Caller::from_uid(uid),
        cwd: "/".into(),
        env: HashMap::new(),
        out: std::cell::RefCell::new(Box::new(std::io::sink())),
    })
}

fn enabled(p: &Project, listen: &str) -> Result<bool> {
    Ok(p.config_value("registry.listen")?.and_then(|v| v.as_str().map(str::to_owned)).as_deref() == Some(listen))
}

fn anonymous(p: &Project) -> bool {
    let mut policy = crate::access::Policy::load(p);
    // Anonymous access has only the '*' principal, without a local uid or groups.
    policy.access.retain(|p, _| p == "*");
    policy.grants.retain(|p, _| p == "*");
    policy.deny.retain(|p, _| p == "*");
    p.config_bool("registry.anonymous", false)
        && policy.check(&crate::access::Who { uid: u32::MAX, groups: vec![] }, "read").is_ok()
}

fn principal(p: &Project, credential: Option<&str>) -> Result<Option<u32>> {
    match credential {
        Some(t) => token_uid(p, t),
        None if anonymous(p) => Ok(Some(u32::MAX)),
        None => Ok(None),
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub listen: String,
    pub head: bool,
    pub token: Option<String>,
    pub operation: Operation,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
pub enum Operation {
    Probe,
    Exchange {
        project: Option<String>,
    },
    Read {
        project: String,
        runnable: String,
        chart: bool,
        endpoint: Endpoint,
        reference: String,
        last: Option<String>,
        n: Option<usize>,
    },
}
#[derive(Serialize, Deserialize)]
pub enum Endpoint {
    Manifest,
    Blob,
    Tags,
}
#[derive(Serialize, Deserialize)]
struct Metadata {
    code: u16,
    media: String,
    digest: Option<String>,
    extra: Vec<(String, String)>,
    length: u64,
}
enum Body {
    Bytes(Vec<u8>),
    File(File),
}
struct Reply {
    metadata: Metadata,
    body: Body,
}
fn reply(code: u16, data: Vec<u8>, media: &str, digest: Option<String>) -> Reply {
    Reply {
        metadata: Metadata { code, media: media.into(), digest, extra: vec![], length: data.len() as u64 },
        body: Body::Bytes(data),
    }
}
fn failure(code: u16, name: &str) -> Reply {
    reply(code, serde_json::to_vec(&json!({"errors": [{"code": name, "message": name}]})).unwrap(), "application/json", None)
}
fn challenge(realm: &str, repository: Option<&str>) -> Reply {
    let mut value = format!("Bearer realm=\"{realm}\",service=\"layr\"");
    if let Some(r) = repository {
        value.push_str(&format!(",scope=\"repository:{r}:pull\""));
    }
    let mut r = failure(401, "UNAUTHORIZED");
    r.metadata.extra.push(("WWW-Authenticate".into(), value));
    r
}

fn realm(p: Option<&Project>, address: SocketAddr) -> Result<String> {
    let base = p
        .map(|p| p.config_value("registry.public_url"))
        .transpose()?
        .flatten()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_else(|| format!("http://{address}"));
    let u = reqwest::Url::parse(&base)?;
    if !["https", "http"].contains(&u.scheme())
        || u.host_str().is_none()
        || !u.username().is_empty()
        || u.password().is_some()
        || u.query().is_some()
        || u.fragment().is_some()
    {
        bail!("registry.public_url must be an HTTP(S) origin");
    }
    Ok(format!("{}/token", base.trim_end_matches('/')))
}

/// Ref read authorization stays here when Core gains narrower ref policy.
fn authorize_ref(repo: &Repo, ctx: &Ctx, _revision: &str) -> Result<()> {
    repo.can_read(ctx)
}

fn tags(repo: &Repo) -> Result<Vec<String>> {
    let mut names = BTreeSet::new();
    for l in repo.lines()? {
        let tag = format!("line-{}", l.name);
        if super::valid_tag(&tag) {
            names.insert(tag);
        }
        names.insert(format!("state-{}", l.head));
    }
    for r in repo.p.db.refs("", crate::ids::now_ms())? {
        if let Some(tag) = r.name.strip_prefix("env/") {
            if super::valid_tag(tag) {
                names.insert(tag.into());
            }
        }
        if let Some(v) = r.name.strip_prefix("release/") {
            if super::semver(v) {
                names.insert(v.replace('+', "_"));
            }
        }
    }
    for id in repo.p.db.changes()? {
        names.insert(format!("change-{id}"));
    }
    Ok(names.into_iter().collect())
}

fn has_runnable(repo: &Repo, tag: &str, runnable: &str, chart: bool) -> bool {
    (|| -> Result<bool> {
        let id = super::resolve_tag(repo, tag)?;
        let tree = crate::tree::Tree::open(&repo.state_root(&id)?)?;
        let mut text = String::new();
        tree.open_read("layr.toml")?.take(1 << 20).read_to_string(&mut text)?;
        Ok(super::parse(&text)?.runnable.get(runnable).map(|r| !chart || r.chart.is_some()).unwrap_or(false))
    })()
    .unwrap_or(false)
}

fn backend(root: &Path, peer: u32, query: Query) -> Result<Reply> {
    let store = Store::open(root)?;
    let allowed = store.project_names()?.into_iter().any(|name| {
        Project::open(&store, &name)
            .ok()
            .filter(|p| enabled(p, &query.listen).unwrap_or(false))
            .and_then(|p| identity(&p).ok())
            .map(|i| i.uid == peer)
            .unwrap_or(false)
    });
    if peer == 0 || !allowed {
        return Ok(failure(403, "DENIED"));
    }
    let address: SocketAddr = query.listen.parse()?;
    let credential = query.token.as_deref();
    let listen = &query.listen;
    let (project, runnable, chart, endpoint, reference, last, n) = match query.operation {
        Operation::Probe | Operation::Exchange { .. } => {
            let project = match &query.operation {
                Operation::Exchange { project } => project.as_deref(),
                _ => None,
            };
            let exchange = matches!(query.operation, Operation::Exchange { .. });
            let mut token_realm = realm(None, address)?;
            for name in store.project_names()? {
                if project.is_some() && project != Some(name.as_str()) {
                    continue;
                }
                let p = Project::open(&store, &name)?;
                if !enabled(&p, listen)? {
                    continue;
                }
                token_realm = realm(Some(&p), address)?;
                if let Some(uid) = principal(&p, credential)? {
                    let ctx = context(root, uid)?;
                    if Repo::open(&ctx, &name).is_err() {
                        continue;
                    }
                    let body = if exchange { json!({"token": credential.unwrap_or(""), "expires_in": 60}) } else { json!({}) };
                    return Ok(reply(200, serde_json::to_vec(&body)?, "application/json", None));
                }
            }
            return Ok(challenge(&token_realm, None));
        }
        Operation::Read { project, runnable, chart, endpoint, reference, last, n } => {
            (project, runnable, chart, endpoint, reference, last, n)
        }
    };
    if super::valid_component(&project).is_err() || super::valid_component(&runnable).is_err() {
        return Ok(failure(404, "NAME_UNKNOWN"));
    }
    let p = match Project::open(&store, &project) {
        Ok(p) if enabled(&p, listen)? => p,
        _ => return Ok(failure(404, "NAME_UNKNOWN")),
    };
    let repo_path = if chart { format!("{project}/charts/{runnable}") } else { format!("{project}/{runnable}") };
    let uid = match principal(&p, credential)? {
        Some(u) => u,
        None => return Ok(challenge(&realm(Some(&p), address)?, Some(&repo_path))),
    };
    let ctx = context(root, uid)?;
    let repo = match Repo::open(&ctx, &project) {
        Ok(r) => r,
        Err(_) => return Ok(failure(403, "DENIED")),
    };
    let cache = Cache::for_repo(&repo)?;
    match endpoint {
        Endpoint::Tags => {
            let mut list = tags(&repo)?;
            list.retain(|t| has_runnable(&repo, t, &runnable, chart));
            list.retain(|t| super::tag_revision(t).map(|r| authorize_ref(&repo, &ctx, &r).is_ok()).unwrap_or(false));
            if let Some(last) = last {
                list.retain(|s| s > &last);
            }
            let mut extra = Vec::new();
            if let Some(n) = n.filter(|n| *n > 0) {
                if list.len() > n {
                    list.truncate(n);
                    extra.push((
                        "Link".into(),
                        format!("</v2/{repo_path}/tags/list?n={n}&last={}>; rel=\"next\"", list.last().unwrap()),
                    ));
                }
            }
            let mut r = reply(200, serde_json::to_vec(&json!({"name": repo_path, "tags": list}))?, "application/json", None);
            r.metadata.extra = extra;
            Ok(r)
        }
        Endpoint::Manifest => {
            let a = if reference.starts_with("sha256:") {
                let path = match super::digest_hex(&reference) {
                    Ok(h) => index(&cache, &runnable, chart).join(h),
                    Err(_) => return Ok(failure(404, "MANIFEST_UNKNOWN")),
                };
                let st: Stored = match fs::read(path).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                    Some(s) => s,
                    None => return Ok(failure(404, "MANIFEST_UNKNOWN")),
                };
                if !stored_allowed(&repo, &ctx, &st) {
                    return Ok(failure(403, "DENIED"));
                }
                st.artifact
            } else {
                let revision = match super::tag_revision(&reference) {
                    Ok(r) => r,
                    Err(_) => return Ok(failure(404, "MANIFEST_UNKNOWN")),
                };
                if authorize_ref(&repo, &ctx, &revision).is_err() {
                    return Ok(failure(403, "DENIED"));
                }
                let state = match super::resolve_tag(&repo, &reference) {
                    Ok(s) => s,
                    Err(_) => return Ok(failure(404, "MANIFEST_UNKNOWN")),
                };
                let source_line = repo.p.db.line_by_name(&revision)?;
                let _lock = repo.p.lock()?;
                let e = match super::export(&ctx, &repo, &state, source_line.as_ref(), Some(&runnable)) {
                    Ok(e) => e,
                    Err(e) => {
                        eprintln!("layr registry: export failed: {e:#}");
                        return Ok(failure(404, "MANIFEST_UNKNOWN"));
                    }
                };
                remember(&e, &revision)?;
                if chart {
                    match e.chart {
                        Some(c) => c,
                        None => return Ok(failure(404, "MANIFEST_UNKNOWN")),
                    }
                } else {
                    e.image
                }
            };
            Ok(reply(200, cache.read(&a.manifest)?, MANIFEST, Some(a.manifest.digest)))
        }
        Endpoint::Blob => {
            if super::digest_hex(&reference).is_err() {
                return Ok(failure(404, "BLOB_UNKNOWN"));
            }
            let mut descriptor = None;
            if let Ok(files) = fs::read_dir(index(&cache, &runnable, chart)) {
                for file in files.flatten() {
                    let st: Stored = match fs::read(file.path()).ok().and_then(|b| serde_json::from_slice(&b).ok()) {
                        Some(s) => s,
                        None => continue,
                    };
                    if !stored_allowed(&repo, &ctx, &st) {
                        continue;
                    }
                    for d in st.artifact.layers.iter().chain(std::iter::once(&st.artifact.config)) {
                        if d.digest == reference {
                            descriptor = Some(d.clone());
                            break;
                        }
                    }
                    if descriptor.is_some() {
                        break;
                    }
                }
            }
            let d = match descriptor {
                Some(d) => d,
                None => return Ok(failure(404, "BLOB_UNKNOWN")),
            };
            let f = match cache.open_blob(&reference) {
                Ok(f) => f,
                Err(_) => return Ok(failure(404, "BLOB_UNKNOWN")),
            };
            Ok(Reply {
                metadata: Metadata {
                    code: 200,
                    media: d.media_type,
                    digest: Some(reference),
                    extra: vec![],
                    length: f.metadata()?.len(),
                },
                body: Body::File(f),
            })
        }
    }
}

fn stored_allowed(repo: &Repo, ctx: &Ctx, st: &Stored) -> bool {
    authorize_ref(repo, ctx, &st.revision).is_ok()
        && super::restriction_key(repo, &st.state).map(|k| k == st.restrictions).unwrap_or(false)
}

/// Root-side narrow RPC. No network descriptor or HTTP parser reaches this function.
pub fn serve_backend(mut sock: std::os::unix::net::UnixStream, root: &Path, peer: u32, value: serde_json::Value) -> Result<()> {
    let query: Query = serde_json::from_value(value)?;
    let head = query.head;
    let r = backend(root, peer, query).unwrap_or_else(|e| {
        eprintln!("layr registry backend: {e:#}");
        failure(500, "UNKNOWN")
    });
    let meta = serde_json::to_vec(&r.metadata)?;
    sock.set_write_timeout(Some(Duration::from_secs(60)))?;
    sock.write_all(&(meta.len() as u32).to_be_bytes())?;
    sock.write_all(&meta)?;
    if head {
        return Ok(());
    }
    match r.body {
        Body::Bytes(data) => sock.write_all(&data)?,
        Body::File(mut f) => {
            std::io::copy(&mut f, &mut sock)?;
        }
    }
    Ok(())
}

fn query(req: &Request, listen: &str) -> Result<Query> {
    let target = reqwest::Url::parse(&format!("http://registry{}", req.target))?;
    let operation = if target.path() == "/token" {
        let project = target
            .query_pairs()
            .find(|(k, _)| k == "scope")
            .map(|(_, v)| v.into_owned())
            .and_then(|s| s.strip_prefix("repository:").map(str::to_owned))
            .and_then(|s| s.split(':').next().map(str::to_owned))
            .and_then(|s| s.split('/').next().map(str::to_owned));
        Operation::Exchange { project }
    } else if target.path().trim_end_matches('/') == "/v2" {
        Operation::Probe
    } else {
        let path = target.path().strip_prefix("/v2/").context("unknown endpoint")?;
        let parts: Vec<_> = path.split('/').collect();
        let (project, runnable, chart, endpoint, reference) = match parts.as_slice() {
            [p, r, e, v] => (*p, *r, false, *e, *v),
            [p, "charts", r, e, v] => (*p, *r, true, *e, *v),
            _ => bail!("unknown endpoint"),
        };
        let endpoint = match (endpoint, reference) {
            ("manifests", _) => Endpoint::Manifest,
            ("blobs", _) => Endpoint::Blob,
            ("tags", "list") => Endpoint::Tags,
            _ => bail!("unknown endpoint"),
        };
        Operation::Read {
            project: project.into(),
            runnable: runnable.into(),
            chart,
            endpoint,
            reference: reference.into(),
            last: target.query_pairs().find(|(k, _)| k == "last").map(|(_, v)| v.into_owned()),
            n: target.query_pairs().find(|(k, _)| k == "n").and_then(|(_, v)| v.parse().ok()),
        }
    };
    let basic = matches!(operation, Operation::Probe | Operation::Exchange { .. });
    Ok(Query { listen: listen.into(), head: req.method == "HEAD", token: token(req.authorization.as_deref(), basic), operation })
}

fn write_headers(stream: &mut TcpStream, m: &Metadata) -> Result<()> {
    let mut headers = vec![
        ("Content-Type".into(), m.media.clone()),
        ("Docker-Distribution-Api-Version".into(), "registry/2.0".into()),
    ];
    if let Some(digest) = &m.digest {
        headers.push(("Docker-Content-Digest".into(), digest.clone()));
    }
    headers.extend(m.extra.iter().cloned());
    layr_store::http::headers(stream, m.code, m.length, &headers)?;
    Ok(())
}

/// Unprivileged HTTP process. One bounded request, one response, then exit.
pub fn handle(mut stream: TcpStream, socket: &Path, listen: &str) -> Result<()> {
    if rustix::process::geteuid().is_root() {
        bail!("registry HTTP process must not run as root");
    }
    stream.set_nonblocking(false)?;
    stream.set_read_timeout(Some(Duration::from_secs(10)))?;
    stream.set_write_timeout(Some(Duration::from_secs(60)))?;
    let req = match request(&stream) {
        Ok(r) => r,
        Err(_) => {
            let r = failure(400, "DENIED");
            write_headers(&mut stream, &r.metadata)?;
            if let Body::Bytes(data) = r.body {
                stream.write_all(&data)?;
            }
            return Ok(());
        }
    };
    let head = req.method == "HEAD";
    let local_error = if req.method != "GET" && !head { Some(failure(405, "UNSUPPORTED")) } else { None };
    let q = query(&req, listen);
    if let Some(r) = local_error.or_else(|| if q.is_err() { Some(failure(404, "NAME_UNKNOWN")) } else { None }) {
        write_headers(&mut stream, &r.metadata)?;
        if !head {
            if let Body::Bytes(data) = r.body {
                stream.write_all(&data)?;
            }
        }
        return Ok(());
    }
    let data = serde_json::to_vec(&json!({"registry": q?}))?;
    if data.len() > 32768 {
        bail!("registry request too large");
    }
    let mut sock = std::os::unix::net::UnixStream::connect(socket)?;
    sock.set_read_timeout(Some(Duration::from_secs(300)))?;
    sock.set_write_timeout(Some(Duration::from_secs(10)))?;
    sock.write_all(&(data.len() as u32).to_be_bytes())?;
    sock.write_all(&data)?;
    let mut size = [0u8; 4];
    sock.read_exact(&mut size)?;
    let size = u32::from_be_bytes(size) as usize;
    if size > 16384 {
        bail!("registry response metadata too large");
    }
    let mut data = vec![0; size];
    sock.read_exact(&mut data)?;
    let metadata: Metadata = serde_json::from_slice(&data)?;
    write_headers(&mut stream, &metadata)?;
    if !head && std::io::copy(&mut sock.take(metadata.length), &mut stream)? != metadata.length {
        bail!("short registry response");
    }
    Ok(())
}
