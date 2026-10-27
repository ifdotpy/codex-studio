//! Person requests. The envelope signs the exact JSON bytes of `r`, like operation records.
//! The caller supplies UUIDv7 ids and times, so this module also builds for wasm32.
use anyhow::{bail, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::{SigningKey, VerifyingKey};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;

pub const DEFAULT_LIFETIME_MS: i64 = 24 * 60 * 60 * 1000;
pub const MAX_BYTES: usize = 96 * 1024;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Action {
    Approve {
        state: String,
        into: String,
    },
    Withdraw {
        state: String,
        into: String,
    },
    QueueAdd {
        state: String,
        into: String,
    },
    QueueRemove {
        entry: String,
    },
    RefMove {
        name: String,
        target: String,
        #[serde(deserialize_with = "required_expected")]
        expected: Option<String>,
    },
    Comment {
        change_id: String,
        version: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        path: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        line: Option<u32>,
        text: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        reply_to: Option<String>,
    },
    Declassify {
        state: String,
        scope: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub format: u32,
    pub id: String,
    pub project: String,
    pub person: String,
    pub created: i64,
    pub expires: i64,
    pub action: Action,
}

/// Unknown envelope fields and duplicate fields fail closed. `r` preserves its signed bytes.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Signed {
    pub r: Box<RawValue>,
    pub s: String,
}

fn required_expected<'de, D: serde::Deserializer<'de>>(deserializer: D) -> std::result::Result<Option<String>, D::Error> {
    Option::<String>::deserialize(deserializer)
}
fn uuid(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value)
}
pub fn request_id(value: &str) -> bool {
    uuid::Uuid::parse_str(value).is_ok_and(|id| id.to_string() == value && id.get_version_num() == 7)
}
fn path(value: &str) -> bool {
    !value.is_empty()
        && !value.starts_with('/')
        && !value.contains(['\\', '\0'])
        && value.split('/').all(|p| !matches!(p, "" | "." | ".."))
}
fn name(value: &str) -> bool {
    !value.is_empty() && value.len() <= 256 && !value.chars().any(|c| c.is_control())
}

impl Request {
    pub fn new(id: String, project: String, person: String, created: i64, action: Action) -> Result<Self> {
        let expires = created.checked_add(DEFAULT_LIFETIME_MS).context("request expiry exceeds the supported time range")?;
        let request = Self { format: 1, id, project, person, created, expires, action };
        request.validate()?;
        Ok(request)
    }
    pub fn validate(&self) -> Result<()> {
        if self.format != 1 || !request_id(&self.id) || !uuid(&self.project) || !crate::person::login_name(&self.person) {
            bail!("invalid request format, UUIDv7 id, project or person");
        }
        if self.created < 0 || self.expires <= self.created {
            bail!("invalid request lifetime");
        }
        let valid = match &self.action {
            Action::Approve { state, into } | Action::Withdraw { state, into } | Action::QueueAdd { state, into } => {
                uuid(state) && name(into)
            }
            Action::QueueRemove { entry } => uuid(entry),
            Action::RefMove { name: reference, target, expected } => {
                name(reference) && uuid(target) && expected.as_deref().is_none_or(uuid)
            }
            Action::Comment { change_id, version, path: file, line, text, reply_to } => {
                uuid(change_id)
                    && uuid(version)
                    && file.as_deref().is_none_or(path)
                    && *line != Some(0)
                    && (line.is_none() || file.is_some())
                    && !text.trim().is_empty()
                    && text.len() <= 65536
                    && reply_to.as_deref().is_none_or(uuid)
            }
            Action::Declassify { state, scope } => {
                uuid(state)
                    && scope
                        .rsplit_once('@')
                        .is_some_and(|(p, hash)| path(p) && hash.len() == 16 && hash.bytes().all(|b| b.is_ascii_hexdigit()))
            }
        };
        if !valid {
            bail!("invalid request action");
        }
        Ok(())
    }
    pub fn sign(&self, key: &SigningKey) -> Result<Signed> {
        self.validate()?;
        let r = RawValue::from_string(serde_json::to_string(self)?)?;
        let s = crate::keys::sign_bytes(r.get().as_bytes(), key);
        Ok(Signed { r, s })
    }
}
impl Signed {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_BYTES {
            bail!("request exceeds 96 KiB");
        }
        Ok(serde_json::from_slice(bytes).context("invalid request envelope")?)
    }
    pub fn request(&self) -> Result<Request> {
        let request: Request = serde_json::from_str(self.r.get()).context("invalid request body")?;
        request.validate()?;
        Ok(request)
    }
    pub fn verify(&self, key: &VerifyingKey) -> Result<Request> {
        crate::keys::verify_bytes(self.r.get().as_bytes(), &self.s, key).context("person request signature does not verify")?;
        self.request()
    }
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(serde_json::to_vec(self)?)
    }
}

/// Maximum v2 wire size, including age and base64 overhead around a bounded v1 request.
pub const MAX_SEALED_BYTES: usize = 144 * 1024;
const DOMAIN: &[u8] = b"layr/request/v2\0";

/// Public routing metadata and encrypted exact signed v1 envelope bytes.
/// `ct` uses standard base64 with padding. The lowercase hex `sig` signs
/// b"layr/request/v2\0" || the decoded age ciphertext bytes, not the base64 text.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sealed {
    pub v: u32,
    pub id: String,
    pub person: String,
    pub project: String,
    pub epoch: String,
    pub ct: String,
    pub sig: String,
}

impl Sealed {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() > MAX_SEALED_BYTES {
            bail!("encrypted request exceeds 144 KiB");
        }
        let sealed: Self = serde_json::from_slice(bytes).context("invalid encrypted request envelope")?;
        sealed.validate()?;
        Ok(sealed)
    }
    fn validate(&self) -> Result<()> {
        if self.v != 2
            || !request_id(&self.id)
            || !uuid(&self.project)
            || !crate::person::login_name(&self.person)
            || !(self.epoch == "root" || uuid(&self.epoch))
        {
            bail!("invalid encrypted request routing metadata");
        }
        Ok(())
    }
    fn ciphertext(&self) -> Result<Vec<u8>> {
        let ct = STANDARD.decode(&self.ct).context("invalid request ciphertext encoding")?;
        if STANDARD.encode(&ct) != self.ct || ct.len() > MAX_BYTES + 4096 {
            bail!("invalid request ciphertext encoding or size");
        }
        Ok(ct)
    }
    /// Verify without opening the request. Also bind id, person and project to the
    /// authenticated gateway login, project and object path at the call site.
    pub fn verify(&self, key: &VerifyingKey) -> Result<()> {
        self.validate()?;
        let ct = self.ciphertext()?;
        let signature = hex::decode(&self.sig).context("invalid outer request signature")?;
        if signature.len() != 64 || hex::encode(&signature) != self.sig {
            bail!("invalid outer request signature");
        }
        let message = [DOMAIN, ct.as_slice()].concat();
        crate::keys::verify_bytes(&message, &STANDARD.encode(signature), key).context("outer request signature does not verify")
    }
    /// Bind the decrypted request to every public routing field it contains.
    pub fn matches(&self, request: &Request) -> Result<()> {
        if self.id != request.id || self.person != request.person || self.project != request.project {
            bail!("outer and inner request id, person or project does not match");
        }
        Ok(())
    }
}

/// Encrypt exact signed v1 bytes to the current project vault recipient (root epoch).
/// The person key verifies the inner envelope and signs the outer ciphertext.
/// No clock, filesystem or native-only API is used; this also builds for wasm32.
pub fn seal(signed_v1: &[u8], epoch_recipient: &str, person_key: &SigningKey) -> Result<Vec<u8>> {
    seal_for_epoch(signed_v1, "root", epoch_recipient, person_key)
}

/// Epoch-aware builder for project rotation. `seal` uses epoch "root".
/// Epoch is routing metadata: the executor must bind it to the selected project identity.
pub fn seal_for_epoch(signed_v1: &[u8], epoch: &str, epoch_recipient: &str, person_key: &SigningKey) -> Result<Vec<u8>> {
    if epoch != "root" && !uuid(epoch) {
        bail!("invalid request epoch");
    }
    let request = Signed::parse(signed_v1)?.verify(&person_key.verifying_key())?;
    let ct = crate::keys::encrypt(epoch_recipient, signed_v1)?;
    let message = [DOMAIN, ct.as_slice()].concat();
    let sig = hex::encode(STANDARD.decode(crate::keys::sign_bytes(&message, person_key))?);
    Ok(serde_json::to_vec(&Sealed {
        v: 2,
        id: request.id,
        person: request.person,
        project: request.project,
        epoch: epoch.into(),
        ct: STANDARD.encode(ct),
        sig,
    })?)
}

/// Decrypt and return the exact original signed v1 envelope bytes, checking routing
/// bindings. Callers must verify the outer signature before opening and verify the
/// returned inner signature against the pinned person key before applying an action.
pub fn open(v2: &[u8], identity: &str) -> Result<Vec<u8>> {
    let sealed = Sealed::parse(v2)?;
    let bytes = crate::keys::decrypt(identity, &sealed.ciphertext()?).context("cannot decrypt person request")?;
    sealed.matches(&Signed::parse(&bytes)?.request()?)?;
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    fn sample() -> Request {
        Request::new(
            "01a12290-0000-7000-8000-000000000001".into(),
            "01a12290-0000-7000-8000-000000000002".into(),
            "ann@example.com".into(),
            1,
            Action::Approve { state: "01a12290-0000-7000-8000-000000000003".into(), into: "main".into() },
        )
        .unwrap()
    }
    #[test]
    fn request_signature_covers_exact_body_bytes() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let request = sample();
        assert_eq!(request.expires, 1 + DEFAULT_LIFETIME_MS);
        let signed = request.sign(&key).unwrap();
        assert_eq!(Signed::parse(&signed.to_bytes().unwrap()).unwrap().verify(&key.verifying_key()).unwrap(), request);
        let mut changed = signed.clone();
        changed.r = RawValue::from_string(signed.r.get().replace("main", "other")).unwrap();
        assert!(changed.verify(&key.verifying_key()).is_err());
        changed.r = RawValue::from_string(format!("{{ {}", &signed.r.get()[1..])).unwrap();
        assert!(changed.verify(&key.verifying_key()).is_err());
        assert!(signed.verify(&SigningKey::from_bytes(&[8; 32]).verifying_key()).is_err());
    }
    fn wrap(inner: &[u8], request: &Request, recipient: &str, key: &SigningKey) -> Vec<u8> {
        let ct = crate::keys::encrypt(recipient, inner).unwrap();
        serde_json::to_vec(&Sealed {
            v: 2,
            id: request.id.clone(),
            person: request.person.clone(),
            project: request.project.clone(),
            epoch: "root".into(),
            ct: STANDARD.encode(&ct),
            sig: hex::encode(STANDARD.decode(crate::keys::sign_bytes(&[DOMAIN, ct.as_slice()].concat(), key)).unwrap()),
        })
        .unwrap()
    }
    #[test]
    fn encryption_preserves_exact_signed_bytes_and_hides_comments() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let identity = crate::keys::generate_identity();
        let recipient = crate::keys::recipient_of(&identity).unwrap();
        let mut request = sample();
        request.action = Action::Comment {
            change_id: request.project.clone(),
            version: request.project.clone(),
            path: None,
            line: None,
            text: "Only the project may read this comment.".into(),
            reply_to: None,
        };
        let signed = request.sign(&key).unwrap();
        // Include envelope whitespace to ensure seal/open do not reserialize signed bytes.
        let inner = format!(" {} \n", String::from_utf8(signed.to_bytes().unwrap()).unwrap()).into_bytes();
        let bytes = seal(&inner, &recipient, &key).unwrap();
        assert!(!String::from_utf8_lossy(&bytes).contains("Only the project"));
        let sealed = Sealed::parse(&bytes).unwrap();
        assert_eq!(sealed.epoch, "root");
        sealed.verify(&key.verifying_key()).unwrap();
        assert_eq!(open(&bytes, &identity).unwrap(), inner);
        assert_eq!(Signed::parse(&open(&bytes, &identity).unwrap()).unwrap().verify(&key.verifying_key()).unwrap(), request);
        assert!(open(&bytes, &crate::keys::generate_identity()).is_err());
        assert!(seal(&inner, &recipient, &SigningKey::from_bytes(&[8; 32])).is_err());
    }
    #[test]
    fn encrypted_requests_verify_both_signatures_and_bind_routing() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let identity = crate::keys::generate_identity();
        let recipient = crate::keys::recipient_of(&identity).unwrap();
        let request = sample();
        let inner = request.sign(&key).unwrap().to_bytes().unwrap();
        let bytes = seal(&inner, &recipient, &key).unwrap();
        let mut sealed = Sealed::parse(&bytes).unwrap();
        sealed.sig.replace_range(..2, if sealed.sig.starts_with("00") { "01" } else { "00" });
        assert!(sealed.verify(&key.verifying_key()).is_err());
        sealed = Sealed::parse(&bytes).unwrap();
        sealed.ct = STANDARD.encode(b"changed ciphertext");
        assert!(sealed.verify(&key.verifying_key()).is_err());
        sealed = Sealed::parse(&bytes).unwrap();
        sealed.person = "bob@example.com".into();
        // Metadata is bound to the authenticated login at the gateway and to the
        // signed inner request at the executor; it cannot change the actor.
        sealed.verify(&key.verifying_key()).unwrap();
        assert!(open(&serde_json::to_vec(&sealed).unwrap(), &identity).is_err());
        sealed = Sealed::parse(&bytes).unwrap();
        sealed.id = "01a12290-0000-7000-8000-000000000009".into();
        assert!(open(&serde_json::to_vec(&sealed).unwrap(), &identity).is_err());
        sealed = Sealed::parse(&bytes).unwrap();
        sealed.project = sealed.id.clone();
        assert!(open(&serde_json::to_vec(&sealed).unwrap(), &identity).is_err());
        let mut bad_inner = request.sign(&key).unwrap();
        bad_inner.r = RawValue::from_string(bad_inner.r.get().replace("main", "other")).unwrap();
        let malicious = wrap(&bad_inner.to_bytes().unwrap(), &request, &recipient, &key);
        Sealed::parse(&malicious).unwrap().verify(&key.verifying_key()).unwrap();
        assert!(Signed::parse(&open(&malicious, &identity).unwrap()).unwrap().verify(&key.verifying_key()).is_err());
        assert!(Sealed::parse(&inner).is_err());
        assert!(Sealed::parse(&vec![0; MAX_SEALED_BYTES + 1]).is_err());
        assert!(Sealed::parse(br#"{"v":2,"v":2}"#).is_err());
        // A maximum-size comment fits the encrypted wire bound.
        let mut large = sample();
        large.action = Action::Comment {
            change_id: large.project.clone(),
            version: large.project.clone(),
            path: None,
            line: None,
            text: "x".repeat(65536),
            reply_to: None,
        };
        let large = seal(&large.sign(&key).unwrap().to_bytes().unwrap(), &recipient, &key).unwrap();
        assert!(large.len() <= MAX_SEALED_BYTES);
        Sealed::parse(&large).unwrap().verify(&key.verifying_key()).unwrap();
    }

    #[test]
    fn claims_bind_the_executor_and_ref_moves_require_an_expected_value() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let claim = Claim {
            format: 1,
            project: sample().project,
            request: sample().id,
            machine: "01a12290-0000-7000-8000-000000000005".into(),
        };
        let bytes = claim.sign(&key).unwrap().to_bytes().unwrap();
        assert_eq!(Claim::verify(&bytes, &key.verifying_key()).unwrap(), claim);
        assert!(Claim::verify(&bytes, &SigningKey::from_bytes(&[8; 32]).verifying_key()).is_err());
        let missing = r#"{"kind":"ref_move","name":"release/v1","target":"01a12290-0000-7000-8000-000000000003"}"#;
        assert!(serde_json::from_str::<Action>(missing).is_err());
        let explicit = missing.replace("}", ",\"expected\":null}");
        assert!(serde_json::from_str::<Action>(&explicit).is_ok());
    }

    #[test]
    fn request_boundaries_reject_ambiguous_or_unbounded_inputs() {
        let mut request = sample();
        request.id = "01a12290-0000-4000-8000-000000000001".into();
        assert!(request.validate().is_err());
        request = sample();
        request.expires = request.created;
        assert!(request.validate().is_err());
        request = sample();
        request.person = "../ann@example.com".into();
        assert!(request.validate().is_err());
        assert!(Signed::parse(&vec![0; MAX_BYTES + 1]).is_err());
        assert!(Signed::parse(br#"{"r":{},"r":{},"s":"x"}"#).is_err());
        request = sample();
        request.action = Action::Comment {
            change_id: request.project.clone(),
            version: request.project.clone(),
            path: Some("../secret".into()),
            line: Some(1),
            text: "text".into(),
            reply_to: None,
        };
        assert!(request.validate().is_err());
    }
}

/// The machine that owns application of one request. Claims never expire or transfer.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(deny_unknown_fields)]
pub struct Claim {
    pub format: u32,
    pub project: String,
    pub request: String,
    pub machine: String,
}
impl Claim {
    pub fn validate(&self) -> Result<()> {
        if self.format != 1 || !uuid(&self.project) || !request_id(&self.request) || !uuid(&self.machine) {
            bail!("invalid request claim");
        }
        Ok(())
    }
    pub fn sign(&self, key: &SigningKey) -> Result<Signed> {
        self.validate()?;
        let r = serde_json::value::to_raw_value(self)?;
        let s = crate::keys::sign_bytes(r.get().as_bytes(), key);
        Ok(Signed { r, s })
    }
    pub fn verify(bytes: &[u8], key: &VerifyingKey) -> Result<Self> {
        let signed = Signed::parse(bytes)?;
        crate::keys::verify_bytes(signed.r.get().as_bytes(), &signed.s, key)?;
        let claim: Self = serde_json::from_str(signed.r.get())?;
        claim.validate()?;
        Ok(claim)
    }
}
