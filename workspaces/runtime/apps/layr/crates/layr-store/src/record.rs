//! Record signatures cover the exact JSON bytes in the `r` field.

use anyhow::{Context, Result};
use ed25519_dalek::VerifyingKey;
use serde::Deserialize;
use serde_json::value::RawValue;

#[derive(Deserialize)]
struct Line<'a> {
    #[serde(borrow)]
    r: &'a RawValue,
    s: String,
}

/// Parse a record line. If a key is supplied, verify its signature first.
pub fn parse_line<'a>(line: &'a str, key: Option<&VerifyingKey>) -> Result<&'a RawValue> {
    parse_bytes(line.as_bytes(), key)
}

/// Verify a record line without changing the bytes of its `r` value.
pub fn verify_line<'a>(line: &'a [u8], key: &VerifyingKey) -> Result<&'a RawValue> {
    parse_bytes(line, Some(key))
}

fn parse_bytes<'a>(line: &'a [u8], key: Option<&VerifyingKey>) -> Result<&'a RawValue> {
    let line: Line<'a> = serde_json::from_slice(line).context("parse log line")?;
    if let Some(key) = key {
        crate::keys::verify_bytes(line.r.get().as_bytes(), &line.s, key).context("record signature does not verify")?;
    }
    Ok(line.r)
}

/// Read the public key from a machine record's `data.public_key` field.
/// The caller must check the record's operation, machine, and sequence.
pub fn machine_key(record: &RawValue) -> Result<VerifyingKey> {
    let record: serde_json::Value = serde_json::from_str(record.get())?;
    let text = record["data"]["public_key"].as_str().context("machine record without key")?;
    let bytes: [u8; 32] = hex::decode(text)?.try_into().map_err(|_| anyhow::anyhow!("bad key length"))?;
    Ok(VerifyingKey::from_bytes(&bytes)?)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ed25519_dalek::SigningKey;

    #[test]
    fn signatures_cover_original_bytes() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let body = r#"{ "z": 1, "a": 2 }"#;
        let signature = crate::keys::sign_bytes(body.as_bytes(), &key);
        let line = format!(r#"{{"r":{body},"s":"{signature}"}}"#);
        assert_eq!(verify_line(line.as_bytes(), &key.verifying_key()).unwrap().get(), body);
        let changed = line.replace(body, r#"{"a":2,"z":1}"#);
        assert!(verify_line(changed.as_bytes(), &key.verifying_key()).is_err());
        assert!(verify_line(line.as_bytes(), &SigningKey::from_bytes(&[8; 32]).verifying_key()).is_err());
        assert!(parse_line(r#"{"r":{},"r":{},"s":""}"#, None).is_err());
    }
}
