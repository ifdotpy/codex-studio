//! Signed operation records. A stored record is one JSON line
//! `{"r": <record>, "s": "<Ed25519 signature of the exact record bytes>"}`; the same line is the
//! JSONL form used for replication, backups and export.

use crate::model::Record;
use anyhow::{anyhow, Context, Result};
use base64::Engine;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::Deserialize;

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(borrow)]
    r: &'a serde_json::value::RawValue,
    s: String,
}

pub struct Signed {
    pub record: Record,
    /// The exact line as stored, without the newline.
    pub line: String,
}

pub fn sign_line(rec: &Record, key: &SigningKey) -> Result<String> {
    let body = serde_json::to_string(rec)?;
    let sig = key.sign(body.as_bytes());
    let s = base64::engine::general_purpose::STANDARD.encode(sig.to_bytes());
    Ok(format!("{{\"r\":{body},\"s\":\"{s}\"}}"))
}

/// Parse one stored line. With `key`, also check the signature.
pub fn parse_line(line: &str, key: Option<&VerifyingKey>) -> Result<Signed> {
    let l: Line = serde_json::from_str(line).context("parse log line")?;
    if let Some(k) = key {
        let sig = base64::engine::general_purpose::STANDARD.decode(&l.s)?;
        let sig = Signature::from_slice(&sig).map_err(|e| anyhow!("bad signature: {e}"))?;
        k.verify(l.r.get().as_bytes(), &sig).map_err(|_| anyhow!("record signature does not verify"))?;
    }
    let record: Record = serde_json::from_str(l.r.get())?;
    Ok(Signed { record, line: line.to_string() })
}

/// The public key that a machine publishes in its first record (`op: "machine"`).
pub fn machine_key(rec: &Record) -> Result<VerifyingKey> {
    let k = rec.data.get("public_key").and_then(|v| v.as_str()).ok_or_else(|| anyhow!("machine record without key"))?;
    let b = hex::decode(k)?;
    let arr: [u8; 32] = b.try_into().map_err(|_| anyhow!("bad key length"))?;
    Ok(VerifyingKey::from_bytes(&arr)?)
}
