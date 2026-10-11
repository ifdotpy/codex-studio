//! Writer signatures (docs/design.md, "Signatures"). The writer signs each tree with an Ed25519
//! key. `<prefix>/vault.json` names the public key (`writer`) and the first `seq` that must carry
//! a signature (`signedFrom`). A reader refuses a state from `signedFrom` on without a valid
//! signature, and a vault identity whose recipient is not the vault's.

use crate::format::Header;
use crate::keys;
use anyhow::{anyhow, bail, Result};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use ed25519_dalek::{Signature, Signer as _, SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// The first line of every signed message.
pub const CONTEXT: &str = "layr-desk tree v1";

/// The signed message of one tree: its header fields and the SHA-256 of its canonical tree bytes
/// (after delta decoding).
pub fn message(prefix: &str, header: &Header, canonical: &[u8]) -> String {
    let base = match &header.base {
        Some((seq, attempt)) => format!("{seq}.{attempt}"),
        None => "-".to_string(),
    };
    format!(
        "{CONTEXT}\n{prefix}\n{}\n{}\n{}\n{}\n{}\n{base}\n{}",
        header.seq,
        header.attempt,
        header.state,
        header.time,
        header.tree,
        hex::encode(Sha256::digest(canonical))
    )
}

fn decode<const N: usize>(text: &str) -> Option<[u8; N]> {
    STANDARD.decode(text.trim()).ok()?.try_into().ok()
}

/// The writer's signing key. It has no `Debug`, so that it does not reach a log.
pub struct Signer(SigningKey);

impl Signer {
    /// The `signingKey` of the writer configuration: standard base64 of a 32-byte Ed25519 seed.
    pub fn from_base64(seed: &str) -> Result<Self> {
        let seed: [u8; 32] = decode(seed).ok_or_else(|| {
            anyhow!("the signing key is not standard base64 of a 32-byte Ed25519 seed")
        })?;
        Ok(Signer(SigningKey::from_bytes(&seed)))
    }

    /// The public key, as `writer` in vault.json names it.
    pub fn public(&self) -> String {
        STANDARD.encode(self.0.verifying_key().as_bytes())
    }

    /// The `sig` of a tree header.
    pub fn sign(&self, prefix: &str, header: &Header, canonical: &[u8]) -> String {
        let message = message(prefix, header, canonical);
        STANDARD.encode(self.0.sign(message.as_bytes()).to_bytes())
    }
}

/// The public part of `<prefix>/vault.json`, as the gateway route `/api/p/<p>/vault` returns it.
#[derive(Clone, Debug, Default, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct VaultInfo {
    pub recipient: String,
    /// The writer's Ed25519 public key; a vault without it has no signature checks.
    #[serde(default)]
    pub writer: Option<String>,
    /// The first `seq` that must be signed; every `seq` when it is missing.
    #[serde(default)]
    pub signed_from: Option<u64>,
}

/// What a reader of one vault checks: the vault identity against the recipient, and the
/// signature of each state from `signedFrom` on.
#[derive(Clone, Debug)]
pub struct Policy {
    prefix: String,
    recipient: String,
    writer: Option<VerifyingKey>,
    signed_from: u64,
}

impl Policy {
    /// The policy of the vault at `prefix`, from its vault.json.
    pub fn new(prefix: &str, vault: &VaultInfo) -> Result<Self> {
        let writer = match &vault.writer {
            None => None,
            Some(text) => {
                let bytes: [u8; 32] = decode(text).ok_or_else(|| {
                    anyhow!("the writer key in vault.json is not standard base64 of 32 bytes")
                })?;
                Some(
                    VerifyingKey::from_bytes(&bytes)
                        .map_err(|_| anyhow!("the writer key in vault.json is not valid"))?,
                )
            }
        };
        Ok(Policy {
            prefix: prefix.to_string(),
            recipient: vault.recipient.trim().to_string(),
            writer,
            signed_from: vault.signed_from.unwrap_or(0),
        })
    }

    pub fn prefix(&self) -> &str {
        &self.prefix
    }

    /// Whether state `seq` must carry a valid signature: the vault names a writer, and `seq` is
    /// `signedFrom` or later. Earlier states read as before and show as unsigned.
    pub fn signed(&self, seq: u64) -> bool {
        self.writer.is_some() && seq >= self.signed_from
    }

    /// Refuse a vault identity whose recipient is not the vault's: a forged slot holds one.
    pub fn check_identity(&self, identity: &str) -> Result<()> {
        if keys::recipient_of(identity)? != self.recipient {
            bail!(
                "this key does not belong to the vault {}: its recipient is not the one in \
                 vault.json, so the slot may be forged",
                self.prefix
            );
        }
        Ok(())
    }

    /// Check the signature of one decoded tree, when its state must be signed.
    pub fn check(&self, header: &Header, canonical: &[u8]) -> Result<()> {
        let Some(writer) = self.writer.filter(|_| self.signed(header.seq)) else {
            return Ok(());
        };
        let seq = header.seq;
        let Some(sig) = &header.sig else {
            bail!(
                "state {seq} has no writer signature, which this vault requires from state {} \
                 on; it may be forged",
                self.signed_from
            );
        };
        let sig: [u8; 64] = decode(sig).ok_or_else(|| {
            anyhow!("state {seq}: the writer signature is not base64 of 64 bytes; it may be forged")
        })?;
        let message = message(&self.prefix, header, canonical);
        writer
            .verify_strict(message.as_bytes(), &Signature::from_bytes(&sig))
            .map_err(|_| {
                anyhow!("state {seq}: the writer signature does not match; it may be forged")
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn header() -> Header {
        Header {
            v: 1,
            seq: 12,
            attempt: "0a1b2c3d".into(),
            state: "st1".into(),
            time: 1791700000,
            tree: "delta".into(),
            base: Some((11, "99887766".into())),
            parts: 2,
            sig: None,
        }
    }

    #[test]
    fn the_message_names_every_field() {
        let canonical = b"{\"files\":{}}";
        assert_eq!(
            message("proj", &header(), canonical),
            format!(
                "layr-desk tree v1\nproj\n12\n0a1b2c3d\nst1\n1791700000\ndelta\n11.99887766\n{}",
                hex::encode(Sha256::digest(canonical))
            )
        );
        let full = Header {
            tree: "full".into(),
            base: None,
            ..header()
        };
        assert!(message("proj", &full, canonical).contains("\nfull\n-\n"));
    }

    #[test]
    fn a_signed_header_verifies_and_any_change_fails() {
        let signer = Signer::from_base64(&STANDARD.encode([7u8; 32])).unwrap();
        let vault = VaultInfo {
            recipient: "age1x".into(),
            writer: Some(signer.public()),
            signed_from: Some(10),
        };
        let policy = Policy::new("proj", &vault).unwrap();
        let canonical = b"{\"files\":{}}".to_vec();
        let mut signed = header();
        signed.sig = Some(signer.sign("proj", &signed, &canonical));
        policy.check(&signed, &canonical).unwrap();

        type Change = (&'static str, fn(&mut Header));
        let changes: [Change; 8] = [
            ("seq", |h| h.seq += 1),
            ("attempt", |h| h.attempt = "0a1b2c3e".into()),
            ("state", |h| h.state = "st2".into()),
            ("time", |h| h.time += 1),
            ("tree", |h| h.tree = "full".into()),
            ("base", |h| h.base = Some((10, "99887766".into()))),
            ("no base", |h| h.base = None),
            ("sig", |h| h.sig = Some(STANDARD.encode([1u8; 64]))),
        ];
        for (field, change) in changes {
            let mut changed = signed.clone();
            change(&mut changed);
            let err = policy.check(&changed, &canonical).unwrap_err().to_string();
            assert!(err.contains("does not match"), "{field}: {err}");
        }
        let err = policy.check(&signed, b"{\"files\":{\"a\":1}}");
        assert!(err.unwrap_err().to_string().contains("does not match"));
        // Another vault, or another writer key.
        let other = Policy::new("other", &vault).unwrap();
        assert!(other.check(&signed, &canonical).is_err());
        let stranger = Signer::from_base64(&STANDARD.encode([8u8; 32])).unwrap();
        let mut forged = header();
        forged.sig = Some(stranger.sign("proj", &forged, &canonical));
        assert!(policy.check(&forged, &canonical).is_err());

        let unsigned = header();
        let err = policy.check(&unsigned, &canonical).unwrap_err().to_string();
        assert!(err.contains("no writer signature"), "{err}");
        let mut short = header();
        short.sig = Some(STANDARD.encode([1u8; 63]));
        let err = policy.check(&short, &canonical).unwrap_err().to_string();
        assert!(err.contains("64 bytes"), "{err}");
        // Below signedFrom, and in a vault without a writer, nothing is checked.
        let mut old = header();
        old.seq = 9;
        policy.check(&old, &canonical).unwrap();
        assert!(!policy.signed(9) && policy.signed(10));
        let open = Policy::new(
            "proj",
            &VaultInfo {
                recipient: "age1x".into(),
                ..VaultInfo::default()
            },
        )
        .unwrap();
        open.check(&unsigned, &canonical).unwrap();
        assert!(!open.signed(12));
    }

    #[test]
    fn keys_that_are_not_ed25519_are_refused() {
        assert!(Signer::from_base64("not base64").is_err());
        assert!(Signer::from_base64(&STANDARD.encode([1u8; 31])).is_err());
        let vault = |writer: &str| VaultInfo {
            recipient: "age1x".into(),
            writer: Some(writer.into()),
            signed_from: None,
        };
        assert!(Policy::new("p", &vault(&STANDARD.encode([1u8; 33]))).is_err());
        assert!(Policy::new("p", &vault("@@@")).is_err());
        // Without signedFrom every state must be signed.
        let key = Signer::from_base64(&STANDARD.encode([3u8; 32])).unwrap();
        assert!(Policy::new("p", &vault(&key.public())).unwrap().signed(0));
    }

    #[test]
    fn the_identity_must_belong_to_the_recipient() {
        let identity = keys::generate_identity();
        let vault = VaultInfo {
            recipient: keys::recipient_of(&identity).unwrap(),
            ..VaultInfo::default()
        };
        let policy = Policy::new("proj", &vault).unwrap();
        policy.check_identity(&identity).unwrap();
        let err = policy
            .check_identity(&keys::generate_identity())
            .unwrap_err()
            .to_string();
        assert!(err.contains("may be forged"), "{err}");
    }
}
