//! Read-only Desk v1 import. The source vault is never changed.

use super::{format, keys, zst, Options, Report, Store};
use crate::{
    ctx::Ctx,
    model::{LineChange, LineState, Record, StateKind},
    repo::Repo,
};
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{Signature, VerifyingKey};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

#[derive(Clone, Deserialize)]
struct Header {
    v: u32,
    seq: u64,
    attempt: String,
    state: String,
    #[serde(default)]
    time: i64,
    tree: String,
    base: Option<(u64, String)>,
    #[serde(default)]
    sig: Option<String>,
}
#[derive(Clone, Deserialize)]
struct Link(u64, String, u32, u64, u64, String);
#[derive(Deserialize)]
struct Entry(String, u64, String, Vec<Link>);
#[derive(Deserialize)]
struct Tree {
    files: BTreeMap<String, Entry>,
}

// Ported from desk-core/src/sign.rs at 31610ce. Keep the Desk message and policy.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Vault {
    format: u32,
    recipient: String,
    writer: Option<String>,
    signed_from: Option<u64>,
}

struct Policy {
    prefix: String,
    recipient: String,
    writer: Option<VerifyingKey>,
    signed_from: u64,
}

fn decode<const N: usize>(text: &str) -> Option<[u8; N]> {
    STANDARD.decode(text.trim()).ok()?.try_into().ok()
}

fn message(prefix: &str, header: &Header, canonical: &[u8]) -> String {
    let base = match &header.base {
        Some((seq, attempt)) => format!("{seq}.{attempt}"),
        None => "-".to_string(),
    };
    format!(
        "layr-desk tree v1\n{prefix}\n{}\n{}\n{}\n{}\n{}\n{base}\n{}",
        header.seq,
        header.attempt,
        header.state,
        header.time,
        header.tree,
        hex::encode(Sha256::digest(canonical))
    )
}

impl Policy {
    fn new(prefix: &str, vault: &Vault) -> Result<Self> {
        let writer = vault
            .writer
            .as_ref()
            .map(|text| -> Result<VerifyingKey> {
                let bytes: [u8; 32] = decode(text).context("the writer key in vault.json is not standard base64 of 32 bytes")?;
                VerifyingKey::from_bytes(&bytes).map_err(|_| anyhow::anyhow!("the writer key in vault.json is not valid"))
            })
            .transpose()?;
        Ok(Self {
            prefix: prefix.into(),
            recipient: vault.recipient.trim().into(),
            writer,
            signed_from: vault.signed_from.unwrap_or(0),
        })
    }

    fn signed(&self, seq: u64) -> bool {
        self.writer.is_some() && seq >= self.signed_from
    }

    fn check_identity(&self, identity: &str) -> Result<()> {
        let recipient = keys::recipient_of(identity)?;
        if recipient != self.recipient {
            bail!("Desk slot recipient {} differs from vault.json recipient {}", recipient, self.recipient);
        }
        Ok(())
    }

    fn check(&self, header: &Header, canonical: &[u8]) -> Result<()> {
        let Some(writer) = self.writer.filter(|_| self.signed(header.seq)) else { return Ok(()) };
        let seq = header.seq;
        let sig = header.sig.as_ref().with_context(|| {
            format!(
                "state {seq} has no writer signature, which this vault requires from state {} on; it may be forged",
                self.signed_from
            )
        })?;
        let sig: [u8; 64] = decode(sig)
            .with_context(|| format!("state {seq}: the writer signature is not base64 of 64 bytes; it may be forged"))?;
        writer
            .verify_strict(message(&self.prefix, header, canonical).as_bytes(), &Signature::from_bytes(&sig))
            .map_err(|_| anyhow::anyhow!("state {seq}: the writer signature does not match; it may be forged"))
    }
}

fn prefix(store: &Store, opts: &Options) -> String {
    opts.desk_prefix.clone().unwrap_or_else(|| {
        store
            .url
            .strip_prefix("s3://")
            .and_then(|path| path.split_once('/').map(|(_, prefix)| prefix.trim_matches('/').to_string()))
            .unwrap_or_default()
    })
}

fn name(seq: u64, attempt: &str, part: Option<u32>) -> String {
    match part {
        None => format!("states/{seq:010}.{attempt}.tree.age"),
        Some(part) => format!("states/{seq:010}.{attempt}.{part:04}.data.age"),
    }
}

pub(super) fn import(store: &mut Store, ctx: &Ctx, repo: &mut Repo, opts: &Options) -> Result<Report> {
    let vault: Vault = serde_json::from_slice(&store.backend.get("vault.json")?.context("missing vault.json")?)?;
    if vault.format != 1 {
        bail!("unsupported Desk vault format");
    }
    let identity = if let Some(person) = &store.person {
        let project = prefix(store, opts);
        let pin = person.index.pin(&project).context("Desk vault has no accepted person pin")?;
        pin.check(&layr_store::person::VaultInfo {
            recipient: vault.recipient.clone(),
            writer: vault.writer.clone(),
            signed_from: vault.signed_from,
        })?;
        let slot =
            store.backend.get(&format!("slots/person-{}.age", person.login))?.context("this login has no Desk person slot")?;
        keys::open_slot(&slot, &person.identity)?
    } else {
        let paper = opts
            .paper_key
            .clone()
            .or_else(|| store.paper_key_text.clone())
            .context("set LAYR_PAPER_KEY_FILE to import a Desk v1 vault")?;
        let opener = keys::identity_from_paper(&paper).map_err(|_| anyhow::anyhow!("invalid paper key"))?;
        let slot = store.backend.get("slots/paper.age")?.context("Desk vault has no paper slot")?;
        keys::open_slot(&slot, &opener)?
    };
    let policy = Policy::new(&prefix(store, opts), &vault)?;
    policy.check_identity(&identity)?;
    // create_empty has no records; create the normal project record before its imported history.
    let mut record = Record::new("project");
    let project = crate::ids::new_id();
    record.data = serde_json::json!({"id":project,"name":repo.p.name,"config":{}});
    repo.p.append(record, &ctx.actor())?;
    repo.p.reload_id()?;
    store.project = repo.p.id.clone();
    let mut attempts = BTreeMap::new();
    for object in store.backend.list("states/")? {
        let Some(rest) = object.key.strip_prefix("states/").and_then(|s| s.strip_suffix(".tree.age")) else {
            continue;
        };
        let Some((seq, attempt)) = rest.split_once('.') else {
            continue;
        };
        if seq.len() != 10 || !seq.bytes().all(|b| b.is_ascii_digit()) || !format::valid_attempt(attempt) {
            continue;
        }
        let seq: u64 = seq.parse()?;
        attempts.entry(seq).or_insert(attempt.to_string());
    }
    let mut cache: BTreeMap<(u64, String), Vec<u8>> = BTreeMap::new();
    let mut last = None::<String>;
    let line_id = crate::ids::new_id();
    let mut report = Report::default();
    for (seq, attempt) in attempts {
        let canonical = tree(store, &identity, &policy, seq, &attempt, &mut cache)?;
        let source = header(store, &identity, seq, &attempt)?.0;
        policy.check(&source, &canonical)?;
        let tree: Tree = serde_json::from_slice(&canonical)?;
        let stage = repo.p.work_dir().join(crate::ids::new_id());
        crate::storage::create_subvolume(&stage)?;
        let result = (|| -> Result<()> {
            let target = crate::tree::Tree::open(&stage)?;
            // Validate all paths before creating any link or file.
            for path in tree.files.keys() {
                if !format::safe_path(path) {
                    bail!("unsafe Desk tree path");
                }
                let mut parent = std::path::Path::new(path).parent();
                while let Some(p) = parent {
                    if tree.files.contains_key(p.to_str().context("invalid Desk path")?) {
                        bail!("Desk tree path has a file parent");
                    }
                    parent = p.parent();
                }
            }
            crate::fsutil::refuse_case_collisions(&stage, tree.files.keys().map(|k| k.as_str()))?;
            for (path, entry) in &tree.files {
                let data = read(store, &identity, entry)?;
                target.mkdirs_for(path, None)?;
                match entry.0.as_str() {
                    "120000" => {
                        use std::os::unix::ffi::OsStrExt;
                        std::os::unix::fs::symlink(std::ffi::OsStr::from_bytes(&data), crate::fsutil::safe_join(&stage, path)?)?;
                    }
                    "100644" | "100755" => {
                        use std::io::Write;
                        target.create(path, if entry.0 == "100755" { 0o755 } else { 0o644 })?.write_all(&data)?;
                    }
                    _ => bail!("invalid Desk file mode"),
                }
                report.files += 1;
                report.bytes += data.len() as u64;
            }
            let mut state = repo.new_state(
                ctx,
                &stage,
                StateKind::Import,
                last.clone().into_iter().collect(),
                &format!("Desk v1 state {seq}"),
                None,
            )?;
            state.line = Some(line_id.clone());
            if source.time > 0 {
                state.time = source.time.checked_mul(1000).context("Desk timestamp overflow")?;
            }
            let mut record = Record::new("import");
            record.data = serde_json::json!({"desk_seq":seq,"desk_attempt":attempt,"desk_state":source.state,"desk_time":source.time,"source_url":store.url});
            record.states.push(state.clone());
            record.lines.insert(
                line_id.clone(),
                LineChange {
                    before: last.as_ref().map(|head| LineState {
                        name: "main".into(),
                        head: head.clone(),
                        owner: ctx.caller.uid,
                        machine: store.machine.clone(),
                    }),
                    after: Some(LineState {
                        name: "main".into(),
                        head: state.id.clone(),
                        owner: ctx.caller.uid,
                        machine: store.machine.clone(),
                    }),
                    working_before: None,
                    working_after: None,
                },
            );
            repo.commit_record(ctx, record)?;
            // Core stamps the attestation id, signer and time at append.
            let evidence = serde_json::json!({"desk_seq":seq,"desk_attempt":attempt,"source_url":store.url,
                "desk_prefix":policy.prefix,"unsigned":!policy.signed(seq)});
            let digest = hex::encode(Sha256::digest(serde_json::to_vec(&evidence)?));
            let mut attestation = Record::new("attestation");
            attestation.data = serde_json::json!({"attestation":{"format":1,"attestation_id":crate::ids::new_id(),
                "kind":"import","subject":{"kind":"state","id":state.id},"payload_digest":digest,
                "signer":store.machine,"time":crate::ids::now_ms()},"import":evidence});
            repo.commit_record(ctx, attestation)?;
            last = Some(state.id);
            report.states += 1;
            Ok(())
        })();
        let _ = crate::storage::delete_tree(&stage);
        result?;
    }
    if report.states == 0 {
        bail!("Desk vault has no complete state");
    }
    Ok(report)
}

fn decrypted(store: &Store, identity: &str, key: &str) -> Result<Vec<u8>> {
    keys::decrypt(identity, &store.backend.get(key)?.with_context(|| format!("missing Desk object {key}"))?)
}
fn header(store: &Store, identity: &str, seq: u64, attempt: &str) -> Result<(Header, Vec<u8>)> {
    let object = decrypted(store, identity, &name(seq, attempt, None))?;
    let at = object.iter().position(|b| *b == b'\n').context("Desk tree has no header")?;
    let h: Header = serde_json::from_slice(&object[..at])?;
    if h.v != 1 || h.seq != seq || h.attempt != attempt {
        bail!("invalid Desk tree header");
    }
    if !matches!(h.tree.as_str(), "full" | "delta") {
        bail!("invalid Desk tree type");
    }
    Ok((h, object[at + 1..].to_vec()))
}
fn tree(
    store: &Store,
    identity: &str,
    policy: &Policy,
    seq: u64,
    attempt: &str,
    cache: &mut BTreeMap<(u64, String), Vec<u8>>,
) -> Result<Vec<u8>> {
    let mut pending = Vec::new();
    let mut seen = BTreeSet::new();
    let mut at = (seq, attempt.to_string());
    let mut data = loop {
        if !seen.insert(at.clone()) || seen.len() > 32 {
            bail!("invalid Desk tree chain");
        }
        if let Some(data) = cache.get(&at) {
            break data.clone();
        }
        let (h, frame) = header(store, identity, at.0, &at.1)?;
        if h.tree == "full" {
            let data = zst::decompress(&frame, None)?;
            policy.check(&h, &data)?;
            cache.insert(at, data.clone());
            break data;
        }
        at = h.base.clone().context("Desk delta has no base")?;
        pending.push((h, frame));
    };
    while let Some((h, frame)) = pending.pop() {
        data = zst::decompress(&frame, Some(&data))?;
        policy.check(&h, &data)?;
        cache.insert((h.seq, h.attempt), data.clone());
    }
    Ok(data)
}
fn read(store: &Store, identity: &str, entry: &Entry) -> Result<Vec<u8>> {
    if entry.3.is_empty() || entry.3.len() > 17 {
        bail!("invalid Desk file chain");
    }
    let mut data = Vec::new();
    for (index, link) in entry.3.iter().enumerate().rev() {
        if !format::valid_attempt(&link.1)
            || link.2 >= 10000
            || link.5 != if index + 1 == entry.3.len() { "full" } else { "delta" }
        {
            bail!("invalid Desk file frame");
        }
        let part = decrypted(store, identity, &name(link.0, &link.1, Some(link.2)))?;
        let end = usize::try_from(link.3.checked_add(link.4).context("Desk frame range overflow")?)?;
        let frame = part.get(usize::try_from(link.3)?..end).context("Desk frame is outside its part")?;
        data = zst::decompress_with_limit(
            frame,
            if link.5 == "delta" { Some(&data) } else { None },
            if index == 0 { usize::try_from(entry.1)? } else { zst::LIMIT },
        )?;
    }
    if data.len() as u64 != entry.1 || hex::encode(Sha256::digest(&data)) != entry.2 {
        bail!("Desk file size or SHA-256 mismatch");
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixture() -> (Store, String, std::path::PathBuf) {
        let root = std::env::temp_dir().join(crate::ids::new_id());
        let identity = keys::generate_identity();
        let store = Store {
            backend: super::super::backend::Backend::file(root.clone()),
            url: format!("file://{}", root.display()),
            project: String::new(),
            machine: "01a12290-0000-7000-8000-000000000001".into(),
            key: ed25519_dalek::SigningKey::from_bytes(&[7; 32]),
            dir: root.join("index"),
            paper_key_text: None,
            person: None,
            pin: None,
            aws_profile: None,
            layer: None,
        };
        (store, identity, root)
    }
    #[test]
    fn desk_policy_binds_the_exact_message_and_enforces_the_signature_boundary() {
        use ed25519_dalek::Signer;
        let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let vault = Vault {
            format: 1,
            recipient: keys::recipient_of(&keys::generate_identity()).unwrap(),
            writer: Some(STANDARD.encode(key.verifying_key().as_bytes())),
            signed_from: Some(10),
        };
        let policy = Policy::new("proj", &vault).unwrap();
        let canonical = b"{\"files\":{}}";
        let mut h: Header = serde_json::from_value(serde_json::json!({"v":1,"seq":12,"attempt":"0a1b2c3d",
            "state":"st1","time":1791700000,"tree":"delta","base":[11,"99887766"]}))
        .unwrap();
        assert_eq!(
            message("proj", &h, canonical),
            format!(
                "layr-desk tree v1\nproj\n12\n0a1b2c3d\nst1\n1791700000\ndelta\n11.99887766\n{}",
                hex::encode(Sha256::digest(canonical))
            )
        );
        assert!(policy.check(&h, canonical).is_err());
        h.sig = Some(STANDARD.encode(key.sign(message("proj", &h, canonical).as_bytes()).to_bytes()));
        policy.check(&h, canonical).unwrap();
        for change in [
            (|h: &mut Header| h.seq += 1) as fn(&mut Header),
            |h| h.attempt = "11223344".into(),
            |h| h.state = "other".into(),
            |h| h.time += 1,
            |h| h.tree = "full".into(),
            |h| h.base = None,
            |h| h.sig = Some(STANDARD.encode([0; 64])),
            |h| h.sig = Some("@@@".into()),
            |h| h.sig = Some(STANDARD.encode([0; 63])),
        ] {
            let mut changed = h.clone();
            change(&mut changed);
            assert!(policy.check(&changed, canonical).is_err());
        }
        assert!(policy.check(&h, b"changed").is_err());
        assert!(Policy::new("other", &vault).unwrap().check(&h, canonical).is_err());
        h.seq = 9;
        h.sig = None;
        policy.check(&h, canonical).unwrap();
        assert!(!policy.signed(9) && policy.signed(10));
        let policy = Policy::new("proj", &Vault { signed_from: None, ..vault }).unwrap();
        assert!(policy.check(&h, canonical).is_err());
        assert!(policy.check_identity(&keys::generate_identity()).is_err());
        let vault = Vault { format: 1, recipient: String::new(), writer: Some("@@@".into()), signed_from: None };
        assert!(Policy::new("proj", &vault).is_err());
    }

    #[test]
    fn desk_v1_full_and_delta_trees_and_files_are_read_without_mutation() {
        let (store, identity, root) = fixture();
        let recipient = keys::recipient_of(&identity).unwrap();
        let old = b"an old Desk state\n";
        let new = b"an old Desk state\nnew bytes\n";
        let full = zst::compress(old, None).unwrap();
        let delta = zst::compress(new, Some(old)).unwrap();
        let first = serde_json::json!({"files":{"a":["100644",old.len(),hex::encode(Sha256::digest(old)),[[1,"0123abcd",0,0,full.len(),"full"]]]}});
        let second = serde_json::json!({"files":{"a":["100644",new.len(),hex::encode(Sha256::digest(new)),[[2,"0123abcd",0,0,delta.len(),"delta"],[1,"0123abcd",0,0,full.len(),"full"]]]}});
        let first = super::super::canonical::canonical(&first).unwrap();
        let second = super::super::canonical::canonical(&second).unwrap();
        for (seq, data, tree, base) in [(1, &full, &first, None), (2, &delta, &second, Some((1, "0123abcd")))] {
            store.backend.put_new(&name(seq, "0123abcd", Some(0)), keys::encrypt(&recipient, data).unwrap()).unwrap();
            let mut header = serde_json::json!({"v":1,"seq":seq,"attempt":"0123abcd","state":"desk-state","tree":if base.is_some(){"delta"}else{"full"},"base":base,"parts":1,"time":0});
            if seq >= 2 {
                use ed25519_dalek::Signer;
                let key = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
                let h: Header = serde_json::from_value(header.clone()).unwrap();
                header["sig"] = serde_json::json!(STANDARD.encode(key.sign(message("", &h, tree).as_bytes()).to_bytes()));
            }
            let mut body = serde_json::to_vec(&header).unwrap();
            body.push(b'\n');
            body.extend(zst::compress(tree, if base.is_some() { Some(first.as_slice()) } else { None }).unwrap());
            store.backend.put_new(&name(seq, "0123abcd", None), keys::encrypt(&recipient, &body).unwrap()).unwrap();
        }
        let before: Vec<_> = store
            .backend
            .list("")
            .unwrap()
            .into_iter()
            .map(|o| (o.key.clone(), store.backend.get(&o.key).unwrap().unwrap()))
            .collect();
        let policy = Policy::new(
            "",
            &Vault {
                format: 1,
                recipient: recipient.clone(),
                writer: Some(STANDARD.encode(ed25519_dalek::SigningKey::from_bytes(&[7; 32]).verifying_key().as_bytes())),
                signed_from: Some(2),
            },
        )
        .unwrap();
        let canonical = tree(&store, &identity, &policy, 2, "0123abcd", &mut BTreeMap::new()).unwrap();
        assert_eq!(canonical, second);
        let decoded: Tree = serde_json::from_slice(&canonical).unwrap();
        assert_eq!(read(&store, &identity, &decoded.files["a"]).unwrap(), new);
        for (key, body) in before {
            assert_eq!(store.backend.get(&key).unwrap().unwrap(), body);
        }
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    #[ignore = "fixture writer invoked by t21-desk-import"]
    fn emit_desk_v1_fixture() {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;
        // The fixture writer runs only from t21, which sets its folders.
        let Ok(output) = std::env::var("LAYR_TEST_DESK_FIXTURE_DIR") else { return };
        let paper_file = std::env::var("LAYR_TEST_DESK_KEY_FILE").expect("fixture key path");
        let backend = super::super::backend::Backend::open(&format!("file://{output}")).unwrap();
        let identity = keys::generate_identity();
        let recipient = keys::recipient_of(&identity).unwrap();
        let signer = ed25519_dalek::SigningKey::from_bytes(&[7; 32]);
        let paper = keys::generate_identity();
        let mut key = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(paper_file).unwrap();
        writeln!(key, "{}", keys::words_of_identity(&paper).unwrap()).unwrap();
        backend
            .put_new(
                "vault.json",
                serde_json::to_vec(&serde_json::json!({"format":1,"recipient":recipient,"created":0,"writer":STANDARD.encode(signer.verifying_key().as_bytes()),"signedFrom":2})).unwrap(),
            )
            .unwrap();
        backend
            .put_new(
                "slots/paper.age",
                keys::seal_slot(
                    &if std::env::var("LAYR_TEST_DESK_TAMPER").as_deref() == Ok("slot") {
                        keys::generate_identity()
                    } else {
                        identity.clone()
                    },
                    &keys::recipient_of(&paper).unwrap(),
                )
                .unwrap(),
            )
            .unwrap();
        for (seq, content) in [(1, b"Desk first state\n".as_slice()), (2, b"Desk final state\n".as_slice())] {
            let frame = zst::compress(content, None).unwrap();
            backend.put_new(&name(seq, "0123abcd", Some(0)), keys::encrypt(&recipient, &frame).unwrap()).unwrap();
            let tree = serde_json::json!({"files":{"notes.txt":["100644",content.len(),hex::encode(Sha256::digest(content)),[[seq,"0123abcd",0,0,frame.len(),"full"]]]}});
            let canonical = super::super::canonical::canonical(&tree).unwrap();
            let mut header = serde_json::json!({"v":1,"seq":seq,"attempt":"0123abcd","state":format!("Desk source {seq}"),"time":seq,"tree":"full","base":null,"parts":1});
            if seq >= 2 {
                use ed25519_dalek::Signer;
                let h: Header = serde_json::from_value(header.clone()).unwrap();
                let prefix = std::env::var("LAYR_TEST_DESK_PREFIX").unwrap_or_default();
                header["sig"] =
                    serde_json::json!(STANDARD.encode(signer.sign(message(&prefix, &h, &canonical).as_bytes()).to_bytes()));
                match std::env::var("LAYR_TEST_DESK_TAMPER").unwrap_or_default().as_str() {
                    "header" => header["time"] = serde_json::json!(99),
                    "missing" => {
                        header.as_object_mut().unwrap().remove("sig");
                    }
                    "signature" => header["sig"] = serde_json::json!(STANDARD.encode([0; 64])),
                    _ => {}
                }
            }
            let mut body = serde_json::to_vec(&header).unwrap();
            body.push(b'\n');
            body.extend(zst::compress(&super::super::canonical::canonical(&tree).unwrap(), None).unwrap());
            backend.put_new(&name(seq, "0123abcd", None), keys::encrypt(&recipient, &body).unwrap()).unwrap();
        }
    }
}
