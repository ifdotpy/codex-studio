//! Desktop pairing keys stay in the OS vault and never enter renderer messages.
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::{
    rand::{SecureRandom, SystemRandom},
    signature::{Ed25519KeyPair, KeyPair},
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub trait Vault: Send + Sync {
    fn read(&self) -> Result<Value, String>;
    fn write(&self, state: &Value) -> Result<(), String>;
}

#[cfg(target_os = "macos")]
pub struct KeychainVault {
    pub account: String,
}
#[cfg(target_os = "macos")]
impl Vault for KeychainVault {
    fn read(&self) -> Result<Value, String> {
        match security_framework::passwords::get_generic_password(
            "Codex Studio native server keys",
            &self.account,
        ) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| "The server key store is invalid.".into()),
            Err(error) if error.code() == -25300 => Ok(json!({"drafts":{},"servers":{}})),
            Err(_) => Err("The system key store could not be opened.".into()),
        }
    }
    fn write(&self, state: &Value) -> Result<(), String> {
        let bytes =
            serde_json::to_vec(state).map_err(|_| "The server key store could not be encoded.")?;
        security_framework::passwords::set_generic_password(
            "Codex Studio native server keys",
            &self.account,
            &bytes,
        )
        .map_err(|_| "The server key store could not be saved.".into())
    }
}

pub fn origin(value: &str) -> Result<String, String> {
    let url = reqwest::Url::parse(value).map_err(|_| "Invalid server origin.")?;
    if url.scheme() != "https"
        || !url.host_str().is_some_and(|v| v.ends_with(".ts.net"))
        || url.port() == Some(0)
        || !url.username().is_empty()
        || url.password().is_some()
        || url.path() != "/"
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err("Use a Tailscale Serve HTTPS origin.".into());
    }
    Ok(url.origin().ascii_serialization())
}
pub fn field<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value[name]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 16384)
        .ok_or_else(|| format!("Missing or invalid {name}."))
}
fn key(value: &Value) -> Result<Ed25519KeyPair, String> {
    let pem = field(value, "privateKey")?;
    let body = pem
        .strip_prefix("-----BEGIN PRIVATE KEY-----")
        .and_then(|v| v.trim().strip_suffix("-----END PRIVATE KEY-----"))
        .ok_or("Invalid saved server key.")?;
    let bytes = STANDARD
        .decode(body.split_whitespace().collect::<String>())
        .map_err(|_| "Invalid saved server key.")?;
    Ed25519KeyPair::from_pkcs8_maybe_unchecked(&bytes)
        .map_err(|_| "Invalid saved server key.".into())
}
fn pem(label: &str, bytes: &[u8]) -> String {
    let encoded = STANDARD.encode(bytes);
    let body = encoded
        .as_bytes()
        .chunks(64)
        .map(|v| std::str::from_utf8(v).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    format!("-----BEGIN {label}-----\n{body}\n-----END {label}-----\n")
}
/// Multi-server request nonces encode 24 random bytes as 32 URL-safe characters.
pub fn request_nonce() -> Result<String, String> {
    let mut bytes = [0u8; 24];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "Secure randomness is unavailable.")?;
    Ok(URL_SAFE_NO_PAD.encode(bytes))
}

pub fn random_id() -> Result<String, String> {
    let mut bytes = [0u8; 16];
    SystemRandom::new()
        .fill(&mut bytes)
        .map_err(|_| "Secure randomness is unavailable.")?;
    bytes[6] = (bytes[6] & 15) | 64;
    bytes[8] = (bytes[8] & 63) | 128;
    Ok(uuid::Uuid::from_bytes(bytes).to_string())
}
pub fn new_pair(invitation: &Value, request_id: &str) -> Result<Value, String> {
    if invitation["protocol"] != 1 || field(invitation, "serverId")? == "local" {
        return Err("Invalid server invitation.".into());
    }
    let origin = origin(field(invitation, "origin")?)?;
    for name in ["inviteId", "token", "publicKey", "tailscaleUser"] {
        field(invitation, name)?;
    }
    let generated = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| "Could not create a server key.")?;
    let key = Ed25519KeyPair::from_pkcs8(generated.as_ref())
        .map_err(|_| "Could not read the new server key.")?;
    let mut spki = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    spki.extend_from_slice(key.public_key().as_ref());
    let public_key = pem("PUBLIC KEY", &spki);
    let client_id = random_id()?;
    let body = json!({"protocol":1,"inviteId":invitation["inviteId"],"token":invitation["token"],"clientId":client_id,"label":"Studio UI","kind":"ui","publicKey":public_key,"requestId":request_id}).to_string();
    Ok(
        json!({"origin":origin,"serverId":invitation["serverId"],"clientId":client_id,"inviteId":invitation["inviteId"],"publicKey":public_key,"privateKey":pem("PRIVATE KEY",generated.as_ref()),"body":body}),
    )
}

pub fn signature(
    credential: &Value,
    method: &str,
    target: &str,
    timestamp: &str,
    nonce: &str,
    request_id: &str,
    bytes: &[u8],
) -> Result<String, String> {
    let digest = Sha256::digest(bytes)
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    let text = [
        "studio-multi-server-v1",
        method,
        target,
        field(credential, "serverId")?,
        field(credential, "clientId")?,
        timestamp,
        nonce,
        request_id,
        &digest,
    ]
    .join("\n");
    Ok(STANDARD.encode(key(credential)?.sign(text.as_bytes()).as_ref()))
}

// The caller serializes vault mutations. Import is additive and refuses a
// conflicting identity; retrying cannot replace keys created after migration.
pub fn import(vault: &dyn Vault, source: &Value) -> Result<(), String> {
    let fingerprint =
        Sha256::digest(serde_json::to_vec(source).map_err(|_| "Invalid server key archive.")?)
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
    let mut current = vault.read()?;
    if current["imports"]
        .as_array()
        .is_some_and(|v| v.iter().any(|v| v == &fingerprint))
    {
        return Ok(());
    }

    for group in ["drafts", "servers"] {
        let incoming = source[group]
            .as_object()
            .ok_or("Invalid server key archive.")?;
        let target = current[group]
            .as_object_mut()
            .ok_or("Invalid server key store.")?;
        for (id, row) in incoming {
            origin(field(row, "origin")?)?;
            field(row, "serverId")?;
            field(row, "clientId")?;
            if group == "servers" || row.get("privateKey").is_some_and(|v| !v.is_null()) {
                key(row)?;
            }
            if let Some(existing) = target.get(id) {
                if existing != row {
                    return Err("A different server credential already exists. The original profile was kept.".into());
                }
            } else {
                target.insert(id.clone(), row.clone());
            }
        }
    }
    if current["imports"].is_null() {
        current["imports"] = json!([]);
    }
    current["imports"]
        .as_array_mut()
        .ok_or("Invalid key import receipts.")?
        .push(json!(fingerprint));
    vault.write(&current)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    struct Memory(Mutex<Value>);
    impl Vault for Memory {
        fn read(&self) -> Result<Value, String> {
            Ok(self.0.lock().unwrap().clone())
        }
        fn write(&self, value: &Value) -> Result<(), String> {
            *self.0.lock().unwrap() = value.clone();
            Ok(())
        }
    }
    #[test]
    fn keys_sign_exact_bytes_and_import_refuses_partial_or_conflicting_state() {
        let invitation = json!({"protocol":1,"origin":"https://fixture.tailnet.ts.net","serverId":"server","inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
        let credential = new_pair(&invitation, "request").unwrap();
        let signed = signature(
            &credential,
            "POST",
            "/api/test?q=1",
            "123",
            "nonce",
            "request",
            b"null",
        )
        .unwrap();
        let digest = Sha256::digest(b"null")
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        let input = format!(
            "studio-multi-server-v1\nPOST\n/api/test?q=1\nserver\n{}\n123\nnonce\nrequest\n{digest}",
            credential["clientId"].as_str().unwrap()
        );
        ring::signature::UnparsedPublicKey::new(
            &ring::signature::ED25519,
            key(&credential).unwrap().public_key().as_ref(),
        )
        .verify(input.as_bytes(), &STANDARD.decode(signed).unwrap())
        .unwrap();
        let vault = Memory(Mutex::new(json!({"drafts":{},"servers":{}})));
        let source = json!({"drafts":{},"servers":{"server":credential}});
        import(&vault, &source).unwrap();
        import(&vault, &source).unwrap();
        let mut conflict = source.clone();
        conflict["servers"]["server"]["clientId"] = json!("other");
        assert!(import(&vault, &conflict).is_err());
        assert_eq!(vault.read().unwrap()["servers"], source["servers"]);
        let mut invalid = source.clone();
        invalid["servers"]["bad"] = json!({"origin":"http://localhost"});
        assert!(import(&vault, &invalid).is_err());
        assert_eq!(vault.read().unwrap()["servers"], source["servers"]);
        // A completed import receipt survives later credential changes.
        let mut newer = vault.read().unwrap();
        newer["servers"]["server"] = new_pair(&invitation, "later").unwrap();
        vault.write(&newer).unwrap();
        import(&vault, &source).unwrap();
        assert_eq!(vault.read().unwrap(), newer);
    }
}
