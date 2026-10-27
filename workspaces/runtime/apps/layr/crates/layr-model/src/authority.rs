//! Overlay key authority from verified parent proofs. No I/O or local clock is used.
//! Keys must come from the caller's trusted machine set, not arbitrary announcements.
use crate::Record;
use anyhow::{bail, Context, Result};
use ed25519_dalek::VerifyingKey;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::{BTreeMap, BTreeSet};

fn parse(line: &str, key: Option<&VerifyingKey>) -> Result<Record> {
    Ok(serde_json::from_str(layr_store::record::parse_line(line, key)?.get())?)
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Authority {
    pub machines: BTreeSet<String>,
    pub previous: Option<String>,
    retained: BTreeMap<String, String>,
    changes: BTreeMap<String, String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Change {
    pub parent: String,
    pub overlay: String,
    pub name: String,
    pub machines: BTreeSet<String>,
    pub previous: Option<String>,
    pub retained: BTreeMap<String, String>,
}

pub fn digest(line: &str) -> String {
    hex::encode(Sha256::digest(line.as_bytes()))
}

impl Authority {
    /// Accept a current manager or one exact record retained by a valid manager change.
    pub fn permits(&self, record: &Record, line: &str) -> bool {
        self.machines.contains(&record.machine) || self.retained.get(&record.id) == Some(&digest(line))
    }
    pub fn require(&self, machine: &str) -> Result<()> {
        if !self.machines.contains(machine) {
            bail!("machine {machine} is not an overlay key manager");
        }
        Ok(())
    }
    /// Verify every signature and refuse unproved manager changes and unauthorized key records.
    pub fn validate(&self, lines: &[String], overlay: &str, keys: &BTreeMap<String, VerifyingKey>) -> Result<()> {
        for line in lines {
            let record = parse(line, None)?;
            let key = keys.get(&record.machine).with_context(|| format!("overlay signer {} is not trusted", record.machine))?;
            let record = parse(line, Some(key))?;
            crate::validation::validate(&record)?;
            if record.project != overlay {
                bail!("overlay record belongs to another project");
            }
            if record.op == "overlay.managers" {
                if self.changes.get(&record.id) != Some(&digest(line)) {
                    bail!("overlay manager record from {} has no valid parent authority proof", record.machine);
                }
            } else if protected(&record) && !self.permits(&record, line) {
                bail!("overlay record {} signer {} is not a key manager", record.op, record.machine);
            }
        }
        Ok(())
    }
}

pub fn protected(record: &Record) -> bool {
    record.op == "overlay.show-to-project"
        || record.op == "overlay.key"
        || record.op == "overlay.readers"
        || record.data["config"].get("access").is_some()
        || record.data["config"].get("deny").is_some()
        || record.data["config"].get("overlay.revocation").is_some()
        || record.data["config"].get("overlay").is_some()
}

/// Derive authority from the parent project's signed creation and nested manager proofs.
/// The caller supplies its trusted machine keys. Arrival order and machine clocks do not decide authority.
pub fn from_lines(parent: &str, lines: &[String], keys: &BTreeMap<String, VerifyingKey>) -> Result<BTreeMap<String, Authority>> {
    let mut records = Vec::new();
    for line in lines {
        let record = parse(line, None)?;
        if record.op == "overlay.create" || record.op == "overlay.authority" {
            let key = keys.get(&record.machine).context("overlay authority signer is not trusted")?;
            parse(line, Some(key))?;
            if record.project != parent {
                bail!("overlay authority record belongs to another parent project");
            }
            records.push(record);
        }
    }
    fold(parent, records, keys)
}

fn fold(parent: &str, mut records: Vec<Record>, keys: &BTreeMap<String, VerifyingKey>) -> Result<BTreeMap<String, Authority>> {
    records.sort_by(|a, b| (a.time, &a.machine, a.seq).cmp(&(b.time, &b.machine, b.seq)));
    let mut authorities = BTreeMap::new();
    let mut ids = BTreeMap::new();
    let mut pending = Vec::new();
    for record in records {
        if record.op == "overlay.create" {
            let name = record.data["name"].as_str().context("overlay creation has no name")?;
            crate::validation::check_name("overlay", name)?;
            if authorities.contains_key(name) {
                bail!("conflicting overlay creator for {name}");
            }
            let id = record.data["config"][format!("overlay.{name}")]["id"].as_str().map(str::to_owned);
            let creator = if let Some(origin) = record.data["hidden_origin"].as_str() {
                let origin_record = parse(origin, None)?;
                let key = keys.get(&origin_record.machine).context("overlay origin signer is not trusted")?;
                let origin_record = parse(origin, Some(key))?;
                crate::validation::validate(&origin_record)?;
                if origin_record.op != "overlay.create"
                    || Some(&origin_record.project) != id.as_ref()
                    || origin_record.data["parent"] != parent
                    || origin_record.data["name"] != name
                    || origin_record.data["config"][format!("overlay.{name}")]["hidden"] != true
                {
                    bail!("invalid hidden overlay origin");
                }
                origin_record.machine
            } else {
                record.machine.clone()
            };
            ids.insert(name.to_string(), id);
            authorities.insert(
                name.to_string(),
                Authority {
                    machines: BTreeSet::from([creator]),
                    previous: None,
                    retained: BTreeMap::new(),
                    changes: BTreeMap::new(),
                },
            );
        } else {
            let signed = record.data["authority"].as_str().context("overlay authority proof is missing")?;
            let change = parse(signed, None)?;
            let key = keys.get(&change.machine).context("overlay manager signer is not trusted")?;
            let change = parse(signed, Some(key))?;
            crate::validation::validate(&change)?;
            if change.op != "overlay.managers" {
                bail!("invalid overlay authority proof");
            }
            let data: Change = serde_json::from_value(change.data.clone())?;
            if data.parent != parent
                || change.project != data.overlay
                || !crate::validation::is_uuid(&data.overlay)
                || data.machines.is_empty()
                || data.machines.iter().any(|id| !crate::validation::is_uuid(id))
                || record.data["name"].as_str() != Some(&data.name)
            {
                bail!("overlay manager proof has invalid project bindings");
            }
            pending.push((change, signed.to_string(), data));
        }
    }
    // The signed previous-record link gives the causal order, regardless of machine clocks.
    while !pending.is_empty() {
        let position = pending
            .iter()
            .position(|(_, _, change)| authorities.get(&change.name).is_some_and(|a| a.previous == change.previous))
            .context("overlay manager history has a gap or a fork")?;
        let (record, line, change) = pending.remove(position);
        let authority = authorities.get_mut(&change.name).context("unknown overlay authority")?;
        if ids[&change.name].as_ref().is_some_and(|id| id != &change.overlay) {
            bail!("overlay manager proof names another overlay");
        }
        authority.require(&record.machine)?;
        if !change.machines.contains(&record.machine) {
            bail!("a remaining key manager must sign manager removal: {}", record.machine);
        }
        for (id, hash) in &authority.retained {
            if change.retained.get(id) != Some(hash) {
                bail!("overlay manager change loses authenticated history");
            }
        }
        if change.retained.values().any(|hash| hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit())) {
            bail!("invalid retained overlay record digest");
        }
        authority.machines = change.machines;
        authority.retained = change.retained;
        authority.changes.insert(record.id.clone(), digest(&line));
        authority.previous = Some(record.id);
    }
    Ok(authorities)
}

/// Hidden authority comes from the overlay's own authenticated creation and manager chain.
/// The caller opens a reader slot before it supplies these records.
pub fn from_overlay_lines(
    parent: &str,
    overlay: &str,
    lines: &[String],
    keys: &BTreeMap<String, VerifyingKey>,
) -> Result<(String, Authority)> {
    let mut records = Vec::new();
    let mut name = None;
    let mut creator = None;
    for line in lines {
        let record = parse(line, None)?;
        let key = keys.get(&record.machine).context("hidden overlay signer is not trusted")?;
        let mut record = parse(line, Some(key))?;
        crate::validation::validate(&record)?;
        if record.project != overlay {
            bail!("hidden overlay record belongs to another project");
        }
        if record.op == "project" {
            let metadata = &record.data["config"]["overlay"];
            if metadata["project"] != parent || metadata["hidden"] != true || metadata["mode"] != "follow" {
                bail!("hidden overlay has invalid parent or mode");
            }
            name = metadata["name"].as_str().map(str::to_owned);
            creator = Some(record.machine.clone());
        }
        if record.op == "overlay.create" {
            record.project = parent.to_string();
            records.push(record);
        } else if record.op == "overlay.managers" {
            let name = record.data["name"].clone();
            record.op = "overlay.authority".into();
            record.project = parent.to_string();
            record.data = serde_json::json!({"name":name,"authority":line});
            records.push(record);
        }
    }
    let name = name.context("hidden overlay has no project record")?;
    let created = records.iter().find(|r| r.op == "overlay.create").context("hidden overlay has no creation record")?;
    if created.data["parent"] != parent
        || created.data["name"] != name
        || created.data["config"][format!("overlay.{name}")]["id"] != overlay
        || created.data["config"][format!("overlay.{name}")]["hidden"] != true
        || Some(&created.machine) != creator.as_ref()
    {
        bail!("hidden overlay creation differs from its project");
    }
    let mut authorities = fold(parent, records, keys)?;
    if authorities.len() != 1 {
        bail!("hidden overlay has conflicting creations");
    }
    let authority = authorities.remove(&name).context("hidden overlay name differs from creation")?;
    authority.validate(lines, overlay, keys)?;
    Ok((name, authority))
}

/// Retain the exact protected records that the current authority accepts.
pub fn retained(authority: &Authority, lines: &[String]) -> Result<BTreeMap<String, String>> {
    let mut retained = authority.retained.clone();
    for line in lines {
        let record = parse(line, None)?;
        if protected(&record) {
            if !authority.permits(&record, line) {
                bail!("overlay record signer {} is not a key manager", record.machine);
            }
            retained.insert(record.id, digest(line));
        }
    }
    Ok(retained)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    fn id() -> String {
        static NEXT: AtomicU64 = AtomicU64::new(1);
        format!("01a12290-0000-7000-8000-{:012x}", NEXT.fetch_add(1, Ordering::Relaxed))
    }
    fn sign(record: &Record, key: &ed25519_dalek::SigningKey) -> Result<String> {
        let body = serde_json::to_string(record)?;
        let signature = layr_store::keys::sign_bytes(body.as_bytes(), key);
        Ok(format!("{{\"r\":{body},\"s\":\"{signature}\"}}"))
    }

    fn record(op: &str, project: &str, machine: &str, data: serde_json::Value, seq: u64) -> Record {
        let mut r = Record::new(op);
        r.id = id();
        r.project = project.into();
        r.machine = machine.into();
        r.seq = seq;
        r.time = seq as i64;
        r.data = data;
        r
    }
    #[test]
    fn hidden_authority_uses_own_creation_and_rejects_another_parent_or_nonmanager() {
        let (parent, overlay, a, b) = (id(), id(), id(), id());
        let ka = ed25519_dalek::SigningKey::from_bytes(&[1; 32]);
        let kb = ed25519_dalek::SigningKey::from_bytes(&[2; 32]);
        let keys = BTreeMap::from([(a.clone(), ka.verifying_key()), (b.clone(), kb.verifying_key())]);
        let project = record(
            "project",
            &overlay,
            &a,
            serde_json::json!({"config":{"overlay":{"project":parent,"name":"hidden","hidden":true,"mode":"follow"}}}),
            1,
        );
        let create = record(
            "overlay.create",
            &overlay,
            &a,
            serde_json::json!({"name":"hidden","parent":parent,"config":{"overlay.hidden":{"id":overlay,"hidden":true}}}),
            2,
        );
        let mut own = vec![sign(&project, &ka).unwrap(), sign(&create, &ka).unwrap()];
        let (name, authority) = from_overlay_lines(&parent, &overlay, &own, &keys).unwrap();
        assert_eq!(name, "hidden");
        assert_eq!(authority.machines, BTreeSet::from([a]));
        assert!(from_lines(&parent, &[], &keys).unwrap().is_empty());
        assert!(from_overlay_lines(&id(), &overlay, &own, &keys).is_err());
        let forged = record("overlay.readers", &overlay, &b, serde_json::json!({"config":{"access":{}}}), 1);
        own.push(sign(&forged, &kb).unwrap());
        assert!(from_overlay_lines(&parent, &overlay, &own, &keys).is_err());
    }

    #[test]
    fn manager_removal_keeps_exact_old_records_and_refuses_new_records_from_that_signer() {
        let (a, b, d) = (id(), id(), id());
        let (parent, overlay) = (id(), id());
        let ka = ed25519_dalek::SigningKey::from_bytes(&[1; 32]);
        let kb = ed25519_dalek::SigningKey::from_bytes(&[2; 32]);
        let kd = ed25519_dalek::SigningKey::from_bytes(&[3; 32]);
        let keys =
            BTreeMap::from([(a.clone(), ka.verifying_key()), (b.clone(), kb.verifying_key()), (d.clone(), kd.verifying_key())]);
        let creation = record(
            "overlay.create",
            &parent,
            &a,
            serde_json::json!({"name":"secrets","config":{"overlay.secrets":{"id":overlay}}}),
            1,
        );
        let old = record("overlay.key", &overlay, &a, serde_json::json!({}), 3);
        let old_line = sign(&old, &ka).unwrap();
        let retained = BTreeMap::from([(old.id.clone(), digest(&old_line))]);
        let first = record(
            "overlay.managers",
            &overlay,
            &a,
            serde_json::to_value(Change {
                parent: parent.clone(),
                overlay: overlay.clone(),
                name: "secrets".into(),
                machines: BTreeSet::from([a.clone(), d.clone()]),
                previous: None,
                retained: retained.clone(),
            })
            .unwrap(),
            4,
        );
        let second = record(
            "overlay.managers",
            &overlay,
            &d,
            serde_json::to_value(Change {
                parent: parent.clone(),
                overlay: overlay.clone(),
                name: "secrets".into(),
                machines: BTreeSet::from([d.clone()]),
                previous: Some(first.id.clone()),
                retained,
            })
            .unwrap(),
            2,
        );
        let mut lines = vec![sign(&creation, &ka).unwrap()];
        for (change, key) in [(&second, &kd), (&first, &ka)] {
            let proof = sign(change, key).unwrap();
            let wrapper =
                record("overlay.authority", &parent, &change.machine, serde_json::json!({"name":"secrets","authority":proof}), 9);
            lines.push(sign(&wrapper, key).unwrap());
        }
        let authority = from_lines(&parent, &lines, &keys).unwrap().remove("secrets").unwrap();
        assert_eq!(authority.machines, BTreeSet::from([d.clone()]));
        assert!(authority.permits(&old, &old_line));
        let mut changed = old.clone();
        changed.data = serde_json::json!({"recipient":"different"});
        assert!(!authority.permits(&changed, &sign(&changed, &ka).unwrap()));
        let bad = record("overlay.readers", &overlay, &b, serde_json::json!({"config":{"access":{}}}), 10);
        assert!(authority.validate(&[sign(&bad, &kb).unwrap()], &overlay, &keys).unwrap_err().to_string().contains(&b));
        let next = record("overlay.key", &overlay, &d, serde_json::json!({}), 10);
        assert!(authority.permits(&next, &sign(&next, &kd).unwrap()));
    }
    #[test]
    fn a_removed_reader_cannot_forge_an_overlay_key_with_a_valid_machine_signature() {
        let (parent, overlay, owner, reader) = (id(), id(), id(), id());
        let owner_key = ed25519_dalek::SigningKey::from_bytes(&[5; 32]);
        let reader_key = ed25519_dalek::SigningKey::from_bytes(&[6; 32]);
        let keys = BTreeMap::from([(owner.clone(), owner_key.verifying_key()), (reader.clone(), reader_key.verifying_key())]);
        let creation = record(
            "overlay.create",
            &parent,
            &owner,
            serde_json::json!({"name":"private","config":{"overlay.private":{"id":overlay}}}),
            1,
        );
        let parent_lines = vec![sign(&creation, &owner_key).unwrap()];
        let authority = from_lines(&parent, &parent_lines, &keys).unwrap().remove("private").unwrap();
        let removal = record("overlay.readers", &overlay, &owner, serde_json::json!({"config":{"access":{"user:0":"admin"}}}), 2);
        let forged = record("overlay.key", &overlay, &reader, serde_json::json!({}), 3);
        let forged_line = sign(&forged, &reader_key).unwrap();
        layr_store::record::verify_line(forged_line.as_bytes(), &reader_key.verifying_key()).unwrap();
        assert!(!authority.permits(&forged, &forged_line));
        assert!(authority
            .validate(&[sign(&removal, &owner_key).unwrap(), forged_line], &overlay, &keys)
            .unwrap_err()
            .to_string()
            .contains(&reader));
        assert!(from_lines(&id(), &parent_lines, &keys).is_err());
    }
}
