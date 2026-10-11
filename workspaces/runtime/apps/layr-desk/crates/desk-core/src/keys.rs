//! Vault keys and slots (docs/design.md, "Keys"). A slot is the vault identity encrypted with age
//! to the recipient of one opener: the paper key, or a passkey whose PRF output is an X25519
//! secret.

use anyhow::{anyhow, bail, Context, Result};
use bip39::{Language, Mnemonic};
use std::str::FromStr;

const SECRET_HRP: &str = "age-secret-key-";

/// The passkey PRF input of one vault: one passkey gives each vault a different secret.
pub fn prf_salt(prefix: &str) -> Vec<u8> {
    format!("layr-vault/v1/{prefix}").into_bytes()
}

/// The age identity whose X25519 secret is these 32 bytes (a passkey PRF output or a paper key).
pub fn identity_from_secret(secret: &[u8]) -> Result<String> {
    if secret.len() != 32 {
        bail!("an X25519 secret has 32 bytes");
    }
    let hrp = bech32::Hrp::parse(SECRET_HRP)?;
    Ok(bech32::encode::<bech32::Bech32>(hrp, secret)?.to_uppercase())
}

/// The 32-byte X25519 secret of an `AGE-SECRET-KEY-1…` identity.
pub fn secret_of_identity(identity: &str) -> Result<[u8; 32]> {
    let (hrp, data) = bech32::decode(identity.trim()).context("the identity is not bech32")?;
    if hrp.as_str().to_lowercase() != SECRET_HRP {
        bail!("not an age identity");
    }
    data.try_into()
        .map_err(|_| anyhow!("an age identity holds 32 bytes"))
}

fn parse(identity: &str) -> Result<age::x25519::Identity> {
    age::x25519::Identity::from_str(identity.trim())
        .map_err(|e| anyhow!("not an age identity: {e}"))
}

/// A new random identity.
pub fn generate_identity() -> String {
    use age::secrecy::ExposeSecret;
    age::x25519::Identity::generate()
        .to_string()
        .expose_secret()
        .to_string()
}

/// The `age1…` recipient of an identity.
pub fn recipient_of(identity: &str) -> Result<String> {
    Ok(parse(identity)?.to_public().to_string())
}

/// Bytes encrypted to one recipient.
pub fn encrypt(recipient: &str, data: &[u8]) -> Result<Vec<u8>> {
    let recipient = age::x25519::Recipient::from_str(recipient.trim())
        .map_err(|e| anyhow!("not an age recipient: {e}"))?;
    Ok(age::encrypt(&recipient, data)?)
}

/// Bytes encrypted to an identity, decrypted.
pub fn decrypt(identity: &str, data: &[u8]) -> Result<Vec<u8>> {
    decrypt_with(&parse(identity)?, data)
}

pub(crate) fn decrypt_with(identity: &age::x25519::Identity, data: &[u8]) -> Result<Vec<u8>> {
    Ok(age::decrypt(identity, data)?)
}

/// Encrypt the vault identity to one opener.
pub fn seal_slot(vault_identity: &str, opener_recipient: &str) -> Result<Vec<u8>> {
    parse(vault_identity)?;
    encrypt(
        opener_recipient,
        format!("{}\n", vault_identity.trim()).as_bytes(),
    )
}

/// The vault identity in a slot, opened with the opener's identity.
pub fn open_slot(slot: &[u8], opener_identity: &str) -> Result<String> {
    let text = String::from_utf8(decrypt(opener_identity, slot)?)
        .map_err(|_| anyhow!("the slot holds no vault identity"))?;
    let text = text.trim();
    let well_formed = text.starts_with("AGE-SECRET-KEY-1")
        && text[16..]
            .chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit());
    if !well_formed || parse(text).is_err() {
        bail!("the slot holds no vault identity");
    }
    Ok(text.to_string())
}

/// The paper key as 24 BIP39 words: the 32-byte X25519 secret of the paper identity.
pub fn words_of_identity(identity: &str) -> Result<String> {
    let secret = secret_of_identity(identity)?;
    Ok(Mnemonic::from_entropy_in(Language::English, &secret)?.to_string())
}

/// The age identity on paper: 24 BIP39 words (numbers between them and the first four letters
/// of a word are fine, as BIP39 allows) or the `AGE-SECRET-KEY-1…` form.
pub fn identity_from_paper(text: &str) -> Result<String> {
    let compact: String = text.split_whitespace().collect::<String>().to_uppercase();
    if compact.starts_with("AGE-SECRET-KEY-1") {
        secret_of_identity(&compact)?;
        return Ok(compact);
    }
    let list = Language::English.word_list();
    let mut words = Vec::new();
    let lower = text.to_lowercase();
    for word in lower.split(|c: char| c.is_whitespace() || c == ',' || c == ';') {
        let number = word.trim_end_matches(['.', ')']);
        if word.is_empty()
            || (!number.is_empty()
                && number.chars().all(|c| c.is_ascii_digit())
                && word.len() - number.len() <= 1)
        {
            continue;
        }
        if list.contains(&word) {
            words.push(word.to_string());
            continue;
        }
        let found: Vec<&str> = if word.chars().count() >= 4 {
            let start: String = word.chars().take(4).collect();
            list.iter()
                .copied()
                .filter(|w| w.starts_with(&start))
                .collect()
        } else {
            Vec::new()
        };
        if found.len() != 1 {
            bail!("\"{word}\" is not a word of the paper key list");
        }
        words.push(found[0].to_string());
    }
    let mnemonic = Mnemonic::parse_in_normalized(Language::English, &words.join(" "));
    match mnemonic {
        Ok(m) if words.len() == 24 => identity_from_secret(&m.to_entropy()),
        _ => {
            bail!("The paper key is 24 words; these words or their order do not match (checksum).")
        }
    }
}
