//! The person key (docs/design.md, "Person key"). One age identity per person opens the person
//! slots of the vaults that the person may read. The person index lists those vaults and pins the
//! public part of each `vault.json`; a MAC with a key from the person identity shows that the
//! person wrote it. An invite carries the pin of one vault to the person.

use crate::keys;
/// Desk v1 public metadata, kept for its index and invite format.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct VaultInfo {
    pub recipient: String,
    pub writer: Option<String>,
    pub signed_from: Option<u64>,
}

/// The public person record at the gateway.
#[derive(Clone, Debug, serde::Deserialize, serde::Serialize)]
pub struct Record {
    pub format: u32,
    pub recipient: String,
    pub created: u64,
}

use anyhow::{anyhow, bail, Context, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use hmac::{Hmac, Mac};
use serde::{Deserialize, Serialize};
use sha2::Sha256;

/// The first line of the index plaintext, and the input of its MAC key.
pub const INDEX_HEADER: &str = "layr-desk person index v1";

/// A vault name: what the gateway serves at `/<name>/`, and the prefix in the vault bucket.
pub fn project_name(name: &str) -> bool {
    let b = name.as_bytes();
    (1..=44).contains(&b.len())
        && b[0].is_ascii_alphanumeric()
        && b[1..].iter().all(|c| c.is_ascii_alphanumeric() || *c == b'_' || *c == b'-')
}

/// A tailnet login as the gateway uses it in object names and paths: `name@domain`, such as
/// `ann@example.com` or `ann@github`. Tagged devices (`tagged-devices`) have none.
pub fn login_name(login: &str) -> bool {
    let b = login.as_bytes();
    (3..=128).contains(&b.len())
        && b.iter().filter(|c| **c == b'@').count() == 1
        && b[0] != b'@'
        && b[b.len() - 1] != b'@'
        && b.iter().all(|c| c.is_ascii_alphanumeric() || b"._+-@".contains(c))
}

/// The v2 slot name binds one login to one public person key.
pub fn slot_name(login: &str, recipient: &str) -> Result<String> {
    use sha2::Digest;
    if !login_name(login) || !keys::valid_recipient(recipient) {
        bail!("invalid person login or recipient");
    }
    let fingerprint = hex::encode(sha2::Sha256::digest(recipient.as_bytes()));
    Ok(format!("person-{login}-{}.age", &fingerprint[..16]))
}

/// The login in a v2 person slot. Desk v1 names use a separate reader.
pub fn slot_login(name: &str) -> Option<&str> {
    let (login, fp) = name.strip_prefix("person-")?.strip_suffix(".age")?.rsplit_once('-')?;
    (login_name(login) && fp.len() == 16 && fp.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))).then_some(login)
}

/// The public part of one vault's `vault.json`, as the person index keeps it.
#[derive(Clone, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Pin {
    pub project: String,
    pub recipient: String,
    #[serde(default)]
    pub writer: Option<String>,
    #[serde(default)]
    pub signed_from: Option<u64>,
}

impl Pin {
    /// The pin of a vault, from its `vault.json`.
    pub fn of(project: &str, vault: &VaultInfo) -> Self {
        Pin {
            project: project.to_string(),
            recipient: vault.recipient.trim().to_string(),
            writer: vault.writer.clone(),
            signed_from: vault.signed_from,
        }
    }

    /// The v2 store pin. V2 trees carry machine signatures in their headers.
    pub fn of_store(store: &crate::format::Descriptor) -> Self {
        Self { project: store.project.clone(), recipient: store.recipient.trim().into(), writer: None, signed_from: None }
    }

    /// Check the v2 project and recipient before any slot opens.
    pub fn check_store(&self, store: &crate::format::Descriptor) -> Result<()> {
        if self.project != store.project {
            bail!("store project changed: pinned {}, store.json {}", self.project, store.project);
        }
        if self.recipient != store.recipient {
            bail!("store recipient changed: pinned {}, store.json {}", self.recipient, store.recipient);
        }
        if self.writer.is_some() || self.signed_from.is_some() {
            bail!("v2 store pin requires null writer and signedFrom");
        }
        Ok(())
    }

    /// Refuse a pin that names no valid vault, recipient or writer key.
    pub fn validate(&self) -> Result<()> {
        if !project_name(&self.project) {
            bail!("the pin names no valid vault: {:?}", self.project);
        }
        if !keys::valid_recipient(&self.recipient) {
            bail!("the pin of {} has no valid recipient", self.project);
        }
        if let Some(writer) = &self.writer {
            let valid = STANDARD.decode(writer).is_ok_and(|bytes| bytes.len() == 32);
            if !valid {
                bail!("the writer key in the pin of {} is not base64 of 32 bytes", self.project);
            }
        }
        Ok(())
    }

    /// Refuse a `vault.json` that differs from this pin: another recipient, writer key or first
    /// signed state. A larger `signedFrom` would let unsigned states through.
    pub fn check(&self, vault: &VaultInfo) -> Result<()> {
        let mut differ = Vec::new();
        if vault.recipient.trim() != self.recipient {
            differ.push("recipient");
        }
        if vault.writer != self.writer {
            differ.push("writer key");
        }
        if vault.signed_from != self.signed_from {
            differ.push("first signed state");
        }
        if !differ.is_empty() {
            bail!(
                "the vault.json of {} differs from your pin ({}); someone may have changed it, \
                 so ask the owner of the vault for a new invite",
                self.project,
                differ.join(", ")
            );
        }
        Ok(())
    }
}

/// The vaults of one person, sorted by name, one pin each.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct Index {
    pub vaults: Vec<Pin>,
}

impl Index {
    pub fn pin(&self, project: &str) -> Option<&Pin> {
        self.vaults.iter().find(|p| p.project == project)
    }

    /// Add a pin, or replace the pin of the same vault.
    pub fn set(&mut self, pin: Pin) {
        self.vaults.retain(|p| p.project != pin.project);
        self.vaults.push(pin);
        self.vaults.sort_by(|a, b| a.project.cmp(&b.project));
    }
}

/// HMAC-SHA256 with the index key: HMAC-SHA256(secret, INDEX_HEADER), where `secret` is the
/// X25519 secret of the person identity.
fn index_mac(identity: &str) -> Result<Hmac<Sha256>> {
    let secret = keys::secret_of_identity(identity)?;
    let mut derive = Hmac::<Sha256>::new_from_slice(&secret).expect("any key");
    derive.update(INDEX_HEADER.as_bytes());
    let key = derive.finalize().into_bytes();
    Ok(Hmac::<Sha256>::new_from_slice(&key).expect("any key"))
}

/// The index, with its MAC, encrypted to the person recipient.
pub fn seal_index(identity: &str, index: &Index) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(index)?;
    let mut mac = index_mac(identity)?;
    mac.update(&body);
    let mut plain = format!("{INDEX_HEADER}\n{}\n", hex::encode(mac.finalize().into_bytes())).into_bytes();
    plain.extend_from_slice(&body);
    keys::encrypt(&keys::recipient_of(identity)?, &plain)
}

/// The index in `data`, decrypted with the person identity. An index without a valid MAC is
/// refused: anyone can encrypt to the recipient, but only the person can make the MAC.
pub fn open_index(identity: &str, data: &[u8]) -> Result<Index> {
    let plain = keys::decrypt(identity, data).context("the index is not for this person key")?;
    let mut parts = plain.splitn(3, |c| *c == b'\n');
    let (Some(header), Some(given), Some(body)) = (parts.next(), parts.next(), parts.next()) else {
        bail!("the person index is not complete");
    };
    if header != INDEX_HEADER.as_bytes() {
        bail!("the person index has an unknown format");
    }
    let given = std::str::from_utf8(given)
        .ok()
        .and_then(|text| hex::decode(text).ok())
        .ok_or_else(|| anyhow!("the person index has no valid MAC"))?;
    let mut mac = index_mac(identity)?;
    mac.update(body);
    mac.verify_slice(&given)
        .map_err(|_| anyhow!("the MAC of the person index does not match; someone else may have written it"))?;
    let index: Index = serde_json::from_slice(body).context("the person index is not valid")?;
    for pin in &index.vaults {
        pin.validate()?;
    }
    Ok(index)
}

#[derive(Deserialize, Serialize)]
struct Invite {
    invite: u32,
    #[serde(flatten)]
    pin: Pin,
}

/// An invite to one vault, encrypted to the person recipient.
pub fn seal_invite(recipient: &str, pin: &Pin) -> Result<Vec<u8>> {
    pin.validate()?;
    let body = serde_json::to_vec(&Invite { invite: 1, pin: pin.clone() })?;
    keys::encrypt(recipient, &body)
}

/// The pin in an invite, decrypted with the person identity.
pub fn open_invite(identity: &str, data: &[u8]) -> Result<Pin> {
    let plain = keys::decrypt(identity, data).context("the invite is not for this person key")?;
    let invite: Invite = serde_json::from_slice(&plain).context("the invite is not valid")?;
    if invite.invite != 1 {
        bail!("the invite has an unknown format");
    }
    invite.pin.validate()?;
    Ok(invite.pin)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pin(project: &str) -> Pin {
        Pin {
            project: project.into(),
            recipient: keys::recipient_of(&keys::generate_identity()).unwrap(),
            writer: Some(STANDARD.encode([5u8; 32])),
            signed_from: Some(4),
        }
    }

    #[test]
    fn names_of_vaults_and_logins() {
        for good in ["p", "proj-1", "A_b"] {
            assert!(project_name(good), "{good}");
        }
        for bad in ["", "-p", "a/b", "a.b", &"x".repeat(45)] {
            assert!(!project_name(bad), "{bad}");
        }
        for good in ["ann@example.com", "ann@github", "a.b+c_d-e@x.y"] {
            assert!(login_name(good), "{good}");
        }
        for bad in ["tagged-devices", "@x", "x@", "a@b@c", "a b@c", "a/b@c", "a:b@c", "a%40@c", &format!("{}@x", "a".repeat(127))]
        {
            assert!(!login_name(bad), "{bad}");
        }
    }

    #[test]
    fn an_index_opens_for_its_person_only_and_any_change_fails() {
        let person = keys::generate_identity();
        let mut index = Index::default();
        index.set(pin("q"));
        index.set(pin("p"));
        let replaced = pin("q");
        index.set(replaced.clone());
        assert_eq!(index.vaults.len(), 2);
        assert_eq!(index.vaults[0].project, "p");
        assert_eq!(index.pin("q"), Some(&replaced));

        let sealed = seal_index(&person, &index).unwrap();
        assert_eq!(open_index(&person, &sealed).unwrap(), index);
        assert!(open_index(&keys::generate_identity(), &sealed).is_err());

        // Someone who knows only the recipient writes an index: the MAC fails.
        let plain = keys::decrypt(&person, &sealed).unwrap();
        let text = String::from_utf8(plain).unwrap();
        let forged = text.replacen("\"p\"", "\"x\"", 1);
        let recipient = keys::recipient_of(&person).unwrap();
        let err = open_index(&person, &keys::encrypt(&recipient, forged.as_bytes()).unwrap()).unwrap_err().to_string();
        assert!(err.contains("does not match"), "{err}");
        let stranger = keys::generate_identity();
        let theirs = seal_index(&stranger, &index).unwrap();
        let plain = keys::decrypt(&stranger, &theirs).unwrap();
        let err = open_index(&person, &keys::encrypt(&recipient, &plain).unwrap()).unwrap_err().to_string();
        assert!(err.contains("does not match"), "{err}");
        for broken in ["", "layr-desk person index v2\n00\n{}", "layr-desk person index v1\nzz\n{}"] {
            let data = keys::encrypt(&recipient, broken.as_bytes()).unwrap();
            assert!(open_index(&person, &data).is_err(), "{broken:?}");
        }
    }

    #[test]
    fn an_invite_carries_one_valid_pin() {
        let person = keys::generate_identity();
        let recipient = keys::recipient_of(&person).unwrap();
        let given = pin("p");
        let sealed = seal_invite(&recipient, &given).unwrap();
        assert_eq!(open_invite(&person, &sealed).unwrap(), given);
        assert!(open_invite(&keys::generate_identity(), &sealed).is_err());
        let unsigned = Pin { writer: None, signed_from: None, ..pin("q") };
        let sealed = seal_invite(&recipient, &unsigned).unwrap();
        assert_eq!(open_invite(&person, &sealed).unwrap(), unsigned);
        let bad = [
            Pin { project: "../x".into(), ..pin("p") },
            Pin { recipient: "age1nope".into(), ..pin("p") },
            Pin { writer: Some("AAAA".into()), ..pin("p") },
        ];
        for bad in bad {
            assert!(seal_invite(&recipient, &bad).is_err(), "{bad:?}");
            let raw = serde_json::json!({"invite": 1, "project": bad.project,
                "recipient": bad.recipient, "writer": bad.writer, "signedFrom": 1});
            let data = keys::encrypt(&recipient, raw.to_string().as_bytes()).unwrap();
            assert!(open_invite(&person, &data).is_err(), "{bad:?}");
        }
        let other = serde_json::json!({"invite": 2, "project": "p", "recipient": recipient});
        let data = keys::encrypt(&recipient, other.to_string().as_bytes()).unwrap();
        assert!(open_invite(&person, &data).is_err());
    }

    #[test]
    fn a_vault_json_must_match_its_pin() {
        let pinned = pin("p");
        let vault = VaultInfo {
            recipient: format!("{}\n", pinned.recipient),
            writer: pinned.writer.clone(),
            signed_from: pinned.signed_from,
        };
        pinned.check(&vault).unwrap();
        assert_eq!(Pin::of("p", &vault), pinned);
        let cases: [(&str, VaultInfo); 4] = [
            ("recipient", VaultInfo { recipient: keys::recipient_of(&keys::generate_identity()).unwrap(), ..vault.clone() }),
            ("writer key", VaultInfo { writer: Some(STANDARD.encode([6u8; 32])), ..vault.clone() }),
            ("writer key", VaultInfo { writer: None, ..vault.clone() }),
            ("first signed state", VaultInfo { signed_from: Some(99), ..vault.clone() }),
        ];
        for (what, changed) in cases {
            let err = pinned.check(&changed).unwrap_err().to_string();
            assert!(err.contains(&format!("({what})")), "{what}: {err}");
        }
    }
    #[test]
    fn v2_pins_bind_the_project_and_recipient() {
        let store = crate::format::Descriptor {
            format: 2,
            project: "01a12290-0000-7000-8000-000000000001".into(),
            recipient: keys::recipient_of(&keys::generate_identity()).unwrap(),
        };
        let pin = Pin::of_store(&store);
        pin.validate().unwrap();
        pin.check_store(&store).unwrap();
        let mut changed = store.clone();
        changed.project = "01a12290-0000-7000-8000-000000000002".into();
        assert!(pin.check_store(&changed).unwrap_err().to_string().contains("project changed"));
        changed = store;
        changed.recipient = keys::recipient_of(&keys::generate_identity()).unwrap();
        assert!(pin.check_store(&changed).unwrap_err().to_string().contains("recipient"));
    }
    #[test]
    fn person_slot_names_bind_one_login_and_key() {
        let first = keys::recipient_of(&keys::generate_identity()).unwrap();
        let second = keys::recipient_of(&keys::generate_identity()).unwrap();
        let login = "ann-ops@example-domain.com";
        let name = slot_name(login, &first).unwrap();
        assert_eq!(slot_login(&name), Some(login));
        assert_ne!(name, slot_name(login, &second).unwrap());
        for bad in [
            "person-ann@example.com.age",
            "person-ann@example.com-ABCD012345678901.age",
            "person-ann@example.com-0123.age",
            "person-../ann-0123456789abcdef.age",
        ] {
            assert_eq!(slot_login(bad), None);
        }
        assert!(slot_name("../ann", &first).is_err());
        assert!(slot_name(login, "age1invalid").is_err());
    }
}
