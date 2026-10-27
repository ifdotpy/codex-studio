//! Create-only person requests, applied once on a trusted machine.
use super::here;
use crate::{
    args::{parse, Spec},
    ctx::Ctx,
    ids,
    model::{Actor, Comment, QueueChange, QueueEntry, Record, RefChange, RefState},
    repo::Repo,
};
use anyhow::{bail, Context, Result};
use layr_store::request::{self, Action, Request, Sealed, Signed};
use serde_json::json;
use sha2::{Digest, Sha256};

pub fn request(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(
        &Spec::new()
            .flag("--person")
            .flag("--json")
            .value("--remote")
            .value("--into")
            .value("--expect")
            .value("--version")
            .value("--path")
            .value("--line")
            .value("--reply-to")
            .value("--expires"),
        args,
    )?;
    if !a.has("--person") {
        bail!("requests need --person and LAYR_PERSON_KEY_FILE");
    }
    let h = here(ctx)?;
    let mut store = crate::store_remote::open_named(a.get("--remote").unwrap_or("origin"), &h.repo)?;
    let access = store.request_person(ctx)?.context("set LAYR_PERSON_KEY_FILE to sign a request")?;
    let key = crate::privs::as_caller_fs(ctx, || crate::person::signing_key(ctx, &access))?;
    let kind = a.pos.first().map(String::as_str).unwrap_or("");
    let pos = &a.pos[1.min(a.pos.len())..];
    let resolve = |rev: &str| crate::revs::resolve(&h.repo, h.line.as_ref(), rev);
    let into = || a.get("--into").unwrap_or("main").to_owned();
    let usage = "usage: layr request (approve|withdraw|queue-add|queue-remove|ref-move|comment|declassify) ... --person [--remote <name>]";
    let action = match (kind, pos) {
        ("approve", [state]) => Action::Approve { state: resolve(state)?, into: into() },
        ("withdraw", [state]) => Action::Withdraw { state: resolve(state)?, into: into() },
        ("queue-add" | "queue_add", [state]) => Action::QueueAdd { state: resolve(state)?, into: into() },
        ("queue-remove" | "queue_remove", [entry]) => Action::QueueRemove { entry: entry.clone() },
        ("ref-move" | "ref_move", [name, target]) => Action::RefMove {
            name: name.clone(),
            target: resolve(target)?,
            expected: match a.get("--expect").context("ref-move needs --expect <state|none>")? {
                "none" => None,
                rev => Some(resolve(rev)?),
            },
        },
        ("comment", [change, text]) => {
            let change_id = super::change::find(&h.repo, change)?;
            let versions = h.repo.p.db.change_versions(&change_id)?;
            let n = a.get("--version").map(super::change::version).transpose()?.unwrap_or(versions.len().saturating_sub(1));
            Action::Comment {
                change_id,
                version: versions.get(n).context("unknown change version")?.id.clone(),
                path: a.get("--path").map(str::to_owned),
                line: a.get("--line").map(str::parse).transpose()?,
                text: text.clone(),
                reply_to: a.get("--reply-to").map(str::to_owned),
            }
        }
        ("declassify", [state, scope]) => Action::Declassify { state: resolve(state)?, scope: scope.clone() },
        _ => bail!("{usage}"),
    };
    let mut request = Request::new(ids::new_id(), h.repo.p.id.clone(), access.login, ids::now_ms(), action)?;
    if let Some(expires) = a.get("--expires") {
        request.expires = expires.parse().context("--expires must be epoch milliseconds")?;
    }
    let signed = request.sign(&key)?;
    let (epoch, recipient) = store.request_epoch(ctx)?;
    let sealed = request::seal_for_epoch(&signed.to_bytes()?, &epoch, &recipient, &key)?;
    let backend = store.request_backend(ctx)?;
    let name = format!("requests/{}.json", request.id);
    if !backend.put_new(&name, sealed.clone())? {
        bail!("request id already exists");
    }
    if a.has("--json") {
        outln!(ctx, "{}", json!({"id":request.id,"request":serde_json::from_slice::<serde_json::Value>(&sealed)?}));
    } else {
        outln!(ctx, "request {}", request.id);
    }
    Ok(0)
}

fn digest(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

enum ClaimState {
    Owned,
    Other,
    Refused(&'static str),
}

/// Another trusted machine owns its claim until it publishes the result.
/// Untrusted or invalid claims cause a terminal signed refusal.
fn claim(ctx: &Ctx, repo: &Repo, backend: &crate::store_remote::backend::Backend, id: &str) -> Result<ClaimState> {
    use layr_store::request::Claim;
    let machine = &ctx.store.machine;
    let own = Claim { format: 1, project: repo.p.id.clone(), request: id.into(), machine: machine.id.clone() };
    let name = format!("requests/claims/{id}.json");
    if backend.put_new(&name, own.sign(&machine.key)?.to_bytes()?)? {
        return Ok(ClaimState::Owned);
    }
    let prior = backend.get(&name)?.context("request claim disappeared")?;
    let parsed = Signed::parse(&prior).and_then(|s| Ok(serde_json::from_str::<Claim>(s.r.get())?));
    let Ok(claim) = parsed else {
        return Ok(ClaimState::Refused("invalid request claim"));
    };
    if claim.validate().is_err() || claim.project != repo.p.id || claim.request != id {
        return Ok(ClaimState::Refused("invalid request claim"));
    }
    let public = if claim.machine == machine.id {
        machine.key.verifying_key()
    } else {
        let Some(public) = repo.p.db.trusted(&claim.machine)? else {
            return Ok(ClaimState::Refused("claimed by an untrusted machine"));
        };
        layr_store::person::verifying_key(&public)?
    };
    if Claim::verify(&prior, &public).is_err() {
        return Ok(ClaimState::Refused("request claim signature does not verify"));
    }
    Ok(if claim.machine == machine.id { ClaimState::Owned } else { ClaimState::Other })
}

pub fn requests(ctx: &Ctx, args: &[String]) -> Result<i32> {
    let a = parse(&Spec::new().value("--remote").flag("--json"), args)?;
    if a.pos != ["apply"] {
        bail!("usage: layr requests apply [--remote <name>] [--json]");
    }
    let h = here(ctx)?;
    h.repo.require(ctx, "sync")?;
    let mut store = crate::store_remote::open_named(a.get("--remote").unwrap_or("origin"), &h.repo)?;
    let identity = store.request_identity(ctx)?;
    let backend = store.request_backend(ctx)?;
    let _queue_lock = super::queue::require_lock(&h.repo)?;
    let mut results = Vec::new();
    let mut queue_targets = std::collections::BTreeSet::new();
    for object in backend.list("requests/")? {
        let Some(id) = object
            .key
            .strip_prefix("requests/")
            .and_then(|s| s.strip_suffix(".json"))
            .filter(|s| layr_store::request::request_id(s))
        else {
            continue;
        };
        let _lock = h.repo.p.lock()?;
        let (bytes, epoch, identity) = backend.request_object(&object.key, &identity)?;
        let hash = digest(&bytes);
        if let Some(old) = h.repo.p.db.records(usize::MAX >> 1)?.into_iter().find(|r| r.data["request_id"] == id) {
            if old.data["request_digest"] != hash {
                bail!("request id was reused with different content");
            }
            results.push(json!({"id":id,"status":"ignored"}));
            continue;
        }
        let claim = claim(ctx, &h.repo, backend, id)?;
        if matches!(claim, ClaimState::Other) {
            results.push(json!({"id":id,"status":"claimed"}));
            continue;
        }
        let verified = verify_request(&h.repo, id, &bytes, &epoch, &identity);
        let mut record = match (&claim, &verified) {
            (ClaimState::Refused(reason), _) => refusal(reason),
            (_, Ok((request, _, _))) => match effect(ctx, &h.repo, request) {
                Ok(record) => record,
                Err(error) => refusal(&error.to_string()),
            },
            (_, Err(error)) => refusal(&error.to_string()),
        };
        record.data["request_id"] = json!(id);
        record.data["request_digest"] = json!(hash);
        let actor = match verified {
            Ok((request, public, inner)) => {
                record.data["request"] = json!(std::str::from_utf8(&inner)?);
                record.data["encrypted_request"] = json!(std::str::from_utf8(&bytes)?);
                record.data["person_key"] = json!(public);
                if let Some(a) = record.data.get_mut("attestation") {
                    a["payload_digest"] = json!(digest(Signed::parse(&inner)?.r.get().as_bytes()));
                }
                Actor { uid: u32::MAX, name: format!("person:{}", request.person), session: None }
            }
            Err(_) => ctx.actor(),
        };
        let status = if record.op == "request.refused" { "refused" } else { "applied" };
        let record = h.repo.p.append(record, &actor)?;
        if record.op == "queue.add" {
            if let Some(target) = record.data["queue_change"]["after"]["target_line"].as_str() {
                queue_targets.insert(target.to_owned());
            }
        }
        results.push(json!({"id":id,"status":status,"record":record.id,"reason":record.data.get("reason")}));
    }
    drop(_queue_lock);
    for target in queue_targets {
        if let Err(error) = super::queue::run_configured(ctx, &h.repo, Some(&target)) {
            eprintln!("layr queue: {error:#}");
        }
    }
    if a.has("--json") {
        outln!(ctx, "{}", serde_json::to_string(&results)?);
    } else {
        for result in results {
            outln!(
                ctx,
                "{} {}{}",
                result["id"].as_str().unwrap_or(""),
                result["status"].as_str().unwrap_or(""),
                result["reason"].as_str().map(|s| format!(": {s}")).unwrap_or_default()
            );
        }
    }
    Ok(0)
}
fn refusal(reason: &str) -> Record {
    let mut record = Record::new("request.refused");
    record.data = json!({"reason":reason});
    record
}
fn verify_request(repo: &Repo, id: &str, bytes: &[u8], epoch: &str, identity: &str) -> Result<(Request, String, Vec<u8>)> {
    if Signed::parse(bytes).is_ok() {
        bail!("plaintext requests are not accepted");
    }
    let sealed = Sealed::parse(bytes)?;
    if sealed.id != id || sealed.project != repo.p.id {
        bail!("request id or project does not match");
    }
    if sealed.epoch != epoch {
        bail!("request epoch does not match its object namespace");
    }
    let public = repo
        .p
        .config_value("person.keys")?
        .and_then(|v| v[&sealed.person].as_str().map(str::to_owned))
        .context("person signing key is not pinned")?;
    let key = layr_store::person::verifying_key(&public)?;
    sealed.verify(&key)?;
    let inner = request::open(bytes, identity)?;
    let request = Signed::parse(&inner)?.verify(&key)?;
    Ok((request, public, inner))
}

fn effect(ctx: &Ctx, repo: &Repo, request: &Request) -> Result<Record> {
    if ids::now_ms() >= request.expires {
        bail!("request has expired");
    }
    if request.created > ids::now_ms() {
        bail!("request creation time is in the future");
    }
    let login = &request.person;
    let policy = repo.policy();
    policy.check_person(login, "read").map_err(anyhow::Error::msg)?;
    let mut record = match &request.action {
        Action::Approve { state, into } | Action::Withdraw { state, into } => {
            repo.source_state(state)?;
            repo.line(into)?;
            let rule = policy.rule_for(into).unwrap_or_else(|| crate::access::RuleSpec::default().rule());
            if !policy.person_may_approve(&rule, login) {
                bail!("person is not an approver for this line");
            }
            let withdraw = matches!(request.action, Action::Withdraw { .. });
            let mut record = super::attest::record(repo, state, if withdraw { "revoke" } else { "approval" }, b"")?;
            if withdraw {
                record.data["revokes"] = json!(repo.read_model()?.person_attestations(state, login));
            }
            record
        }
        Action::QueueAdd { state, into } => {
            policy.check_person(login, "merge").map_err(anyhow::Error::msg)?;
            let source = repo.source_state(state)?;
            repo.state_root(state)?;
            let target = repo.line(into)?;
            if !repo.working(&target).exists() {
                bail!("queue target line has no local work folder");
            }
            let entry = QueueEntry {
                id: request.id.clone(),
                into: target.name,
                target_line: target.id,
                state: state.clone(),
                change_id: source.change_id,
                added_by: u32::MAX,
                time: ids::now_ms(),
                status: "pending".into(),
                attempts: 0,
                result: None,
                reason: None,
                probes: None,
            };
            let hold = format!("holds/queue/{}", entry.id);
            let mut record = Record::new("queue.add");
            record.data = json!({"queue_change":QueueChange { id: entry.id.clone(), before: None, after: Some(entry) }, "ref_change":RefChange { name:hold.clone(), before:None, after:Some(RefState { name:hold,target:state.clone(),expires_at:None }) }});
            record
        }
        Action::QueueRemove { entry } => {
            policy.check_person(login, "merge").map_err(anyhow::Error::msg)?;
            let before = repo.p.db.queue_entry(entry)?.context("unknown queue entry")?;
            let own = repo.p.db.records(usize::MAX >> 1)?.iter().any(|r| {
                r.op == "queue.add" && r.data["queue_change"]["id"] == *entry && r.actor.name == format!("person:{login}")
            });
            if !own {
                policy.check_person(login, "line.manage").map_err(anyhow::Error::msg)?;
            }
            let hold = format!("holds/queue/{entry}");
            let mut record = Record::new("queue.remove");
            record.data = json!({"queue_change":QueueChange { id:entry.clone(), before:Some(before), after:None }});
            if let Some(value) = repo.p.db.ref_value(&hold, ids::now_ms())? {
                record.data["ref_change"] = json!(RefChange { name: hold, before: Some(value), after: None });
            }
            record
        }
        Action::RefMove { name, target, expected } => {
            if !crate::model::ref_namespace(name) || !crate::model::is_ref(name) {
                bail!("invalid mutable ref name");
            }
            repo.source_state(target)?;
            repo.state_root(target)?;
            let before = repo.p.db.ref_value(name, ids::now_ms())?;
            if before.as_ref().map(|r| &r.target) != expected.as_ref() {
                bail!("ref changed: expected value does not match");
            }
            let after = RefState { name: name.clone(), target: target.clone(), expires_at: None };
            super::refs::authorize_person_update(repo, login, name, before.as_ref(), Some(&after))?;
            let mut record = Record::new("ref.set");
            record.data = json!({"ref_change":RefChange { name:name.clone(),before,after:Some(after) }});
            record
        }
        Action::Comment { change_id, version, path, line, text, reply_to } => {
            policy.check_person(login, "try").map_err(anyhow::Error::msg)?;
            let comment = Comment {
                id: request.id.clone(),
                change_id: change_id.clone(),
                version: version.clone(),
                path: path.clone(),
                line: *line,
                text: text.clone(),
                reply_to: reply_to.clone(),
            };
            super::change::validate_comment(repo, &comment)?;
            let mut record = Record::new("comment");
            record.data = json!({"comment":comment});
            record
        }
        Action::Declassify { state, scope } => {
            policy.check_person(login, "access").map_err(anyhow::Error::msg)?;
            crate::secrets::declassify(ctx, repo, scope, state)?
        }
    };
    if let Some(a) = record.data.get_mut("attestation") {
        a["signer"] = json!(format!("person:{login}"));
    }
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{engine::general_purpose::STANDARD, Engine};
    use ed25519_dalek::SigningKey;

    #[test]
    fn executor_verifies_encrypted_inner_signature_and_person_binding() {
        let root = std::env::temp_dir().join(format!("layr-request-verifier-{}", ids::new_id()));
        let ctx = Ctx {
            store: crate::store::Store::open(&root).unwrap(),
            caller: crate::ctx::Caller::from_uid(rustix::process::geteuid().as_raw()),
            cwd: root.clone(),
            env: Default::default(),
            out: std::cell::RefCell::new(Box::new(std::io::sink())),
        };
        let dir = ctx.store.projects_dir().join("p");
        std::fs::create_dir_all(dir.join("meta/db")).unwrap();
        let repo = Repo {
            p: crate::store::Project {
                store: &ctx.store,
                name: "p".into(),
                dir: dir.clone(),
                id: ids::new_id(),
                db: crate::db::Db::open(&dir.join("meta/db/layr.sqlite")).unwrap(),
            },
        };
        let key = SigningKey::from_bytes(&[7; 32]);
        let mut record = Record::new("project");
        record.data = json!({"id":repo.p.id,"name":"p","config":{
            "person.keys":{"ann@example.com":hex::encode(key.verifying_key().to_bytes())}
        }});
        repo.p.append(record, &ctx.actor()).unwrap();
        let identity = layr_store::keys::generate_identity();
        let recipient = layr_store::keys::recipient_of(&identity).unwrap();
        let request = Request::new(
            ids::new_id(),
            repo.p.id.clone(),
            "ann@example.com".into(),
            ids::now_ms(),
            Action::Comment {
                change_id: ids::new_id(),
                version: ids::new_id(),
                path: None,
                line: None,
                text: "private comment".into(),
                reply_to: None,
            },
        )
        .unwrap();
        let inner = request.sign(&key).unwrap().to_bytes().unwrap();
        let bytes = request::seal(&inner, &recipient, &key).unwrap();
        assert!(verify_request(&repo, &request.id, &bytes, "root", &identity).is_ok());
        let reason = verify_request(&repo, &request.id, &inner, "root", &identity).err().unwrap().to_string();
        assert_eq!(reason, "plaintext requests are not accepted");
        assert!(verify_request(&repo, &ids::new_id(), &bytes, "root", &identity).is_err());
        // Construct hostile envelopes directly: seal intentionally refuses an invalid
        // inner signature, so normal submissions cannot construct these cases.
        let wrap = |inner: &[u8]| {
            let ct = layr_store::keys::encrypt(&recipient, inner).unwrap();
            let mut sealed = Sealed::parse(&bytes).unwrap();
            sealed.ct = STANDARD.encode(&ct);
            sealed.sig = hex::encode(
                STANDARD
                    .decode(layr_store::keys::sign_bytes(&[b"layr/request/v2\0".as_slice(), ct.as_slice()].concat(), &key))
                    .unwrap(),
            );
            serde_json::to_vec(&sealed).unwrap()
        };
        let mut bad = request.sign(&key).unwrap();
        bad.s = STANDARD.encode([0; 64]);
        let malicious = wrap(&bad.to_bytes().unwrap());
        let reason = verify_request(&repo, &request.id, &malicious, "root", &identity).err().unwrap().to_string();
        assert_eq!(reason, "person request signature does not verify");
        let mut other = request.clone();
        other.person = "bob@example.com".into();
        let malicious = wrap(&other.sign(&key).unwrap().to_bytes().unwrap());
        let reason = verify_request(&repo, &request.id, &malicious, "root", &identity).err().unwrap().to_string();
        assert!(reason.contains("outer and inner"));
        let mut sealed = Sealed::parse(&bytes).unwrap();
        sealed.sig = "0".repeat(128);
        assert!(verify_request(&repo, &request.id, &serde_json::to_vec(&sealed).unwrap(), "root", &identity).is_err());
        sealed = Sealed::parse(&bytes).unwrap();
        sealed.epoch = ids::new_id();
        let reason = verify_request(&repo, &request.id, &serde_json::to_vec(&sealed).unwrap(), "root", &identity)
            .err()
            .unwrap()
            .to_string();
        assert_eq!(reason, "request epoch does not match its object namespace");
        drop(repo);
        std::fs::remove_dir_all(root).unwrap();
    }
}
