//! Public access is an ordinary sealed reader slot with a published opener.
//! The project vault identity is returned only in memory, never serialized here.

use crate::{canonical::canonical, format::Descriptor, keys, record};
use anyhow::{bail, Context, Result};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

const DOMAIN: &[u8] = b"layr/store/v2/public\0";

/// `public.json`. `identity` opens `slots/public.age`; it is deliberately public.
/// Do not add a Debug implementation: callers must not log key material.
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub v: u32,
    pub project: String,
    pub recipient: String,
    pub identity: String,
    pub machine: String,
    pub public_key: String,
    pub sig: String,
}

impl Manifest {
    pub fn new(descriptor: &Descriptor, identity: String, machine: String, key: &SigningKey) -> Result<Self> {
        let mut manifest = Self {
            v: 1,
            project: descriptor.project.clone(),
            recipient: descriptor.recipient.clone(),
            identity,
            machine,
            public_key: hex::encode(key.verifying_key().to_bytes()),
            sig: String::new(),
        };
        manifest.validate(descriptor)?;
        manifest.sig = keys::sign_bytes(&manifest.message()?, key);
        Ok(manifest)
    }

    fn message(&self) -> Result<Vec<u8>> {
        let mut value = serde_json::to_value(self)?;
        value.as_object_mut().expect("manifest object").remove("sig");
        let mut bytes = DOMAIN.to_vec();
        bytes.extend(canonical(&value)?);
        Ok(bytes)
    }

    fn validate(&self, descriptor: &Descriptor) -> Result<()> {
        if self.v != 1 || descriptor.format != 2 || self.project != descriptor.project || self.recipient != descriptor.recipient {
            bail!("public manifest does not match the store descriptor");
        }
        if !crate::format::is_uuid(&self.project)
            || !crate::format::is_uuid(&self.machine)
            || !keys::valid_recipient(&self.recipient)
        {
            bail!("invalid public manifest");
        }
        let opener = keys::recipient_of(&self.identity).context("invalid public opener")?;
        if opener == self.recipient {
            bail!("public opener must differ from the vault identity");
        }
        Ok(())
    }

    /// Verify descriptor binding and the machine signature. After reading authenticated
    /// project records, also call `verify_signer` before exposing any project data.
    pub fn parse(bytes: &[u8], descriptor: &Descriptor) -> Result<Self> {
        if bytes.len() > 16 << 10 {
            bail!("public manifest is too large");
        }
        let manifest: Self = serde_json::from_slice(bytes).map_err(|_| anyhow::anyhow!("invalid public manifest JSON"))?;
        manifest.validate(descriptor)?;
        keys::verify_bytes(&manifest.message()?, &manifest.sig, &manifest.key()?)
            .context("public manifest signature does not verify")?;
        Ok(manifest)
    }

    fn key(&self) -> Result<VerifyingKey> {
        let bytes: [u8; 32] =
            hex::decode(&self.public_key)?.try_into().map_err(|_| anyhow::anyhow!("invalid public manifest signer key"))?;
        Ok(VerifyingKey::from_bytes(&bytes)?)
    }

    /// Open the existing age slot. The vault identity must remain in memory.
    pub fn open(&self, slot: &[u8]) -> Result<String> {
        let identity = keys::open_slot(slot, &self.identity).context("cannot open public slot")?;
        if keys::recipient_of(&identity)? != self.recipient {
            bail!("public slot does not match the store recipient");
        }
        Ok(identity)
    }

    pub fn verify_signer(&self, trusted: &BTreeMap<String, VerifyingKey>) -> Result<()> {
        if trusted.get(&self.machine) != Some(&self.key()?) {
            bail!("public manifest signer is not a trusted project writer");
        }
        Ok(())
    }
}

/// Resolve writer trust from authenticated project history. The creator is the root;
/// subsequent machine.trust records must come from an already trusted machine.
/// Callers supply the complete verified record prefixes, just as for normal readers.
pub fn trusted_writers(project: &str, lines: &[String]) -> Result<BTreeMap<String, VerifyingKey>> {
    let mut parsed = Vec::new();
    let mut keys = BTreeMap::new();
    let mut creator = None;
    for line in lines {
        let raw = record::parse_line(line, None)?;
        let value: serde_json::Value = serde_json::from_str(raw.get())?;
        if value["project"].as_str() != Some(project) {
            bail!("public writer records belong to another project");
        }
        if value["op"] == "machine" && value["seq"] == 1 {
            let machine = value["machine"].as_str().context("machine record has no id")?;
            let key = record::machine_key(raw)?;
            if keys.insert(machine.to_string(), key).is_some() {
                bail!("duplicate writer machine record");
            }
        }
        parsed.push((line, value));
    }
    for (line, value) in &parsed {
        let machine = value["machine"].as_str().context("record has no machine")?;
        record::verify_line(line.as_bytes(), keys.get(machine).context("writer has no machine record")?)?;
        if value["op"] == "project" {
            if value["data"]["id"].as_str() != Some(project) || creator.replace(machine.to_string()).is_some() {
                bail!("ambiguous project creator");
            }
        }
    }
    let mut trusted = BTreeSet::from([creator.context("push the project before making its store public")?]);
    loop {
        let before = trusted.len();
        for (_, value) in &parsed {
            if value["op"] != "machine.trust" || !trusted.contains(value["machine"].as_str().unwrap_or("")) {
                continue;
            }
            let target = value["data"]["machine"].as_str().unwrap_or("");
            if keys
                .get(target)
                .is_some_and(|key| Some(hex::encode(key.to_bytes()).as_str()) == value["data"]["public_key"].as_str())
            {
                trusted.insert(target.to_string());
            }
        }
        if trusted.len() == before {
            break;
        }
    }
    Ok(keys.into_iter().filter(|(machine, _)| trusted.contains(machine)).collect())
}

#[cfg(all(test, feature = "native"))]
mod tests {
    use super::*;
    const PROJECT: &str = "01a12290-0000-7000-8000-000000000001";
    const OWNER: &str = "01a12290-0000-7000-8000-000000000002";
    const WRITER: &str = "01a12290-0000-7000-8000-000000000003";
    const ROGUE: &str = "01a12290-0000-7000-8000-000000000004";

    fn fixture() -> (Descriptor, String, Manifest, SigningKey) {
        let vault = keys::generate_identity();
        let descriptor = Descriptor { format: 2, project: PROJECT.into(), recipient: keys::recipient_of(&vault).unwrap() };
        let key = SigningKey::from_bytes(&[7; 32]);
        let manifest = Manifest::new(&descriptor, keys::generate_identity(), OWNER.into(), &key).unwrap();
        (descriptor, vault, manifest, key)
    }
    fn line(machine: &str, seq: u64, op: &str, data: serde_json::Value, key: &SigningKey) -> String {
        let r = serde_json::json!({"project":PROJECT,"machine":machine,"seq":seq,"op":op,"data":data});
        let bytes = serde_json::to_vec(&r).unwrap();
        serde_json::to_string(&serde_json::json!({"r":r,"s":keys::sign_bytes(&bytes,key)})).unwrap()
    }
    fn registration(machine: &str, key: &SigningKey) -> String {
        line(machine, 1, "machine", serde_json::json!({"public_key":hex::encode(key.verifying_key().to_bytes())}), key)
    }
    #[test]
    fn public_opener_opens_only_its_sealed_vault() {
        let (descriptor, vault, manifest, key) = fixture();
        let bytes = serde_json::to_vec(&manifest).unwrap();
        assert!(!bytes.windows(vault.len()).any(|w| w == vault.as_bytes()));
        let parsed = Manifest::parse(&bytes, &descriptor).unwrap();
        parsed.verify_signer(&BTreeMap::from([(OWNER.into(), key.verifying_key())])).unwrap();
        let slot = keys::seal_slot(&vault, &keys::recipient_of(&parsed.identity).unwrap()).unwrap();
        assert!(parsed.open(&slot).unwrap() == vault);
        assert!(keys::open_slot(&slot, &keys::generate_identity()).is_err());
    }
    #[test]
    fn every_manifest_field_is_authenticated() {
        let (descriptor, _, manifest, _) = fixture();
        let value = serde_json::to_value(manifest).unwrap();
        for (field, replacement) in [
            ("v", serde_json::json!(2)),
            ("project", serde_json::json!(WRITER)),
            ("recipient", serde_json::json!(keys::recipient_of(&keys::generate_identity()).unwrap())),
            ("identity", serde_json::json!(keys::generate_identity())),
            ("machine", serde_json::json!(WRITER)),
            ("public_key", serde_json::json!("00".repeat(32))),
            ("sig", serde_json::json!("AAAA")),
        ] {
            let mut changed = value.clone();
            changed[field] = replacement;
            assert!(Manifest::parse(&serde_json::to_vec(&changed).unwrap(), &descriptor).is_err());
        }
        assert!(Manifest::parse(br#"{"v":1,"identity":"invalid"}"#, &descriptor).is_err());
    }
    #[test]
    fn rejects_a_wrong_slot_and_a_published_vault_identity() {
        let (descriptor, vault, manifest, key) = fixture();
        let other = keys::generate_identity();
        let slot = keys::seal_slot(&other, &keys::recipient_of(&manifest.identity).unwrap()).unwrap();
        assert!(manifest.open(&slot).is_err());
        assert!(Manifest::new(&descriptor, vault, OWNER.into(), &key).is_err());
    }
    #[test]
    fn writer_trust_follows_only_authenticated_trusted_machines() {
        let (descriptor, _, _, owner) = fixture();
        let writer = SigningKey::from_bytes(&[8; 32]);
        let rogue = SigningKey::from_bytes(&[9; 32]);
        let mut lines = vec![
            registration(OWNER, &owner),
            registration(WRITER, &writer),
            registration(ROGUE, &rogue),
            line(OWNER, 2, "project", serde_json::json!({"id":PROJECT}), &owner),
            line(
                ROGUE,
                2,
                "machine.trust",
                serde_json::json!({"machine":ROGUE,"public_key":hex::encode(rogue.verifying_key().to_bytes())}),
                &rogue,
            ),
        ];
        let public = Manifest::new(&descriptor, keys::generate_identity(), WRITER.into(), &writer).unwrap();
        assert!(public.verify_signer(&trusted_writers(PROJECT, &lines).unwrap()).is_err());
        lines.push(line(
            OWNER,
            3,
            "machine.trust",
            serde_json::json!({"machine":WRITER,"public_key":hex::encode(writer.verifying_key().to_bytes())}),
            &owner,
        ));
        let trusted = trusted_writers(PROJECT, &lines).unwrap();
        public.verify_signer(&trusted).unwrap();
        assert!(!trusted.contains_key(ROGUE));
        lines[0] = registration(OWNER, &rogue);
        assert!(trusted_writers(PROJECT, &lines).is_err());
    }
    #[test]
    fn rejects_missing_or_ambiguous_project_authority() {
        let (_, _, _, owner) = fixture();
        let mut lines = vec![registration(OWNER, &owner)];
        assert!(trusted_writers(PROJECT, &lines).is_err());
        lines.push(line(OWNER, 2, "project", serde_json::json!({"id":PROJECT}), &owner));
        lines.push(line(OWNER, 3, "project", serde_json::json!({"id":PROJECT}), &owner));
        assert!(trusted_writers(PROJECT, &lines).is_err());
    }
}
