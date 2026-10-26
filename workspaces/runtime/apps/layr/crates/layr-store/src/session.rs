//! Person session proof shared by native clients, the browser, and the gateway.
use anyhow::{bail, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use ed25519_dalek::SigningKey;

/// Domain-separated proof over the canonical base64 challenge's 32 random bytes.
pub fn message(login: &str, challenge: &str) -> Result<Vec<u8>> {
    if !crate::person::login_name(login) || challenge.len() != 44 {
        bail!("invalid session login or challenge");
    }
    let bytes = STANDARD.decode(challenge).map_err(|_| anyhow::anyhow!("invalid session challenge"))?;
    if bytes.len() != 32 || STANDARD.encode(&bytes) != challenge {
        bail!("invalid session challenge");
    }
    let mut message = b"layr/session/v1\0".to_vec();
    message.extend_from_slice(login.as_bytes());
    message.push(0);
    message.extend_from_slice(&bytes);
    Ok(message)
}

/// Lowercase hex Ed25519 signature; signing uses the existing key helper.
pub fn sign(login: &str, challenge: &str, key: &SigningKey) -> Result<String> {
    let signature = crate::keys::sign_bytes(&message(login, challenge)?, key);
    Ok(hex::encode(STANDARD.decode(signature)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn proof_binds_domain_login_challenge_and_key() {
        let key = SigningKey::from_bytes(&[7; 32]);
        let challenge = STANDARD.encode([9; 32]);
        let mut expected = b"layr/session/v1\0ann@example.com\0".to_vec();
        expected.extend_from_slice(&[9; 32]);
        assert_eq!(message("ann@example.com", &challenge).unwrap(), expected);
        let signature = sign("ann@example.com", &challenge, &key).unwrap();
        assert_eq!(signature.len(), 128);
        let signature = STANDARD.encode(hex::decode(signature).unwrap());
        crate::keys::verify_bytes(&expected, &signature, &key.verifying_key()).unwrap();
        for changed in [
            message("bob@example.com", &challenge).unwrap(),
            message("ann@example.com", &STANDARD.encode([8; 32])).unwrap(),
            b"request".to_vec(),
        ] {
            assert!(crate::keys::verify_bytes(&changed, &signature, &key.verifying_key()).is_err());
        }
        assert!(crate::keys::verify_bytes(&expected, &signature, &SigningKey::from_bytes(&[8; 32]).verifying_key()).is_err());
    }
    #[test]
    fn rejects_invalid_logins_and_noncanonical_challenges() {
        for login in ["", "ann", "a@b\0c"] {
            assert!(message(login, &STANDARD.encode([0; 32])).is_err());
        }
        for challenge in [
            "".into(),
            STANDARD.encode([0; 31]),
            STANDARD.encode([0; 33]),
            "A".repeat(44),
            STANDARD.encode([0; 32]).replace('=', ""),
        ] {
            assert!(message("ann@example.com", &challenge).is_err());
        }
    }
}
