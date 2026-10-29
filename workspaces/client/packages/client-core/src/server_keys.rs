//! Desktop pairing keys stay in the OS vault and never enter renderer messages.
pub use crate::server_key_state::{
    Credential, CredentialObject, Group, TextField, VaultObject, VaultState,
};
use base64::{
    Engine,
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
};
use ring::{
    rand::{SecureRandom, SystemRandom},
    signature::{Ed25519KeyPair, KeyPair},
};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

pub trait Vault: Send + Sync {
    fn read(&self) -> Result<VaultState, String>;
    fn write(&self, state: &VaultState) -> Result<(), String>;
}

/// File name of the desktop pairing key store inside the client data folder.
pub const SERVER_KEYS_FILE: &str = "server-keys.json";

/// Keeps pairing keys in a file that only the user can read. A keychain item
/// is bound to the exact build of an app without a team identity, so each
/// update asked for the login password again.
pub struct FileVault {
    path: std::path::PathBuf,
}
impl FileVault {
    pub fn new(directory: &std::path::Path) -> Self {
        Self {
            path: directory.join(SERVER_KEYS_FILE),
        }
    }
}
impl Vault for FileVault {
    fn read(&self) -> Result<VaultState, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                .and_then(VaultState::deserialize)
                .map_err(|_| "The server key store is invalid.".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(VaultState::from_value(json!({"drafts":{},"servers":{}})))
            }
            Err(_) => Err("The server key store could not be opened.".into()),
        }
    }
    fn write(&self, state: &VaultState) -> Result<(), String> {
        use std::io::Write;
        let bytes =
            serde_json::to_vec(state).map_err(|_| "The server key store could not be encoded.")?;
        let directory = self
            .path
            .parent()
            .ok_or("The server key store has no folder.")?;
        // tempfile creates the file with mode 0600; the rename keeps it.
        let mut file = tempfile::NamedTempFile::new_in(directory)
            .map_err(|_| "The server key store could not be saved.")?;
        file.write_all(&bytes)
            .and_then(|()| file.as_file().sync_all())
            .map_err(|_| "The server key store could not be saved.")?;
        file.persist(&self.path)
            .map_err(|_| "The server key store could not be saved.")?;
        Ok(())
    }
}

/// Copies keys from the keychain item of earlier builds into the file store
/// once, when the file does not exist yet.
#[cfg(target_os = "macos")]
pub fn move_keychain_keys(file: &FileVault, account: &str) -> Result<(), String> {
    if file.path.exists() {
        return Ok(());
    }
    let state = KeychainVault {
        account: account.to_owned(),
    }
    .read()?;
    file.write(&state)
}

#[cfg(target_os = "macos")]
pub struct KeychainVault {
    pub account: String,
}
#[cfg(target_os = "macos")]
impl Vault for KeychainVault {
    fn read(&self) -> Result<VaultState, String> {
        match security_framework::passwords::get_generic_password(
            "Codex Studio native server keys",
            &self.account,
        ) {
            Ok(bytes) => serde_json::from_slice::<Value>(&bytes)
                .and_then(VaultState::deserialize)
                .map_err(|_| "The server key store is invalid.".into()),
            Err(error) if error.code() == -25300 => {
                Ok(VaultState::from_value(json!({"drafts":{},"servers":{}})))
            }
            Err(_) => Err("The system key store could not be opened.".into()),
        }
    }
    fn write(&self, state: &VaultState) -> Result<(), String> {
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
    field_value(&value[name], name)
}

pub(crate) fn field_value<'a>(value: &'a Value, name: &str) -> Result<&'a str, String> {
    value
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 16384)
        .ok_or_else(|| format!("Missing or invalid {name}."))
}
fn key(value: &Credential) -> Result<Ed25519KeyPair, String> {
    let pem = value.field("privateKey")?;
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
    // Base64 is ASCII, so every byte-aligned chunk is valid UTF-8.
    let body = encoded
        .as_bytes()
        .chunks(64)
        .map(|v| std::str::from_utf8(v).expect("base64 is ASCII"))
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
pub fn new_pair(invitation: &Value, request_id: &str) -> Result<Credential, String> {
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
    let mut extra = serde_json::Map::new();
    extra.insert("publicKey".into(), Value::String(public_key));
    Ok(Credential::Object(Box::new(CredentialObject {
        origin: TextField::Text(origin),
        server_id: TextField::Text(field(invitation, "serverId")?.to_owned()),
        client_id: TextField::Text(client_id),
        invite_id: TextField::Text(field(invitation, "inviteId")?.to_owned()),
        private_key: TextField::Text(pem("PRIVATE KEY", generated.as_ref())),
        body: TextField::Text(body),
        receipt: None,
        rejected: None,
        extra,
    })))
}

pub fn signature(
    credential: &Credential,
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
        credential.field("serverId")?,
        credential.field("clientId")?,
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
    if current
        .object_mut()
        .and_then(|v| v.imports.as_ref())
        .and_then(Value::as_array)
        .is_some_and(|v| v.iter().any(|v| v == &fingerprint))
    {
        return Ok(());
    }
    for group in ["drafts", "servers"] {
        let incoming = source[group]
            .as_object()
            .ok_or("Invalid server key archive.")?;
        let target = current
            .group_mut(group)
            .and_then(Group::entries_mut)
            .ok_or("Invalid server key store.")?;
        for (id, row) in incoming {
            let credential = Credential::from_value(row.clone());
            origin(credential.field("origin")?)?;
            credential.field("serverId")?;
            credential.field("clientId")?;
            if group == "servers" || row.get("privateKey").is_some_and(|v| !v.is_null()) {
                key(&credential)?;
            }
            if let Some(existing) = target.get(id) {
                if existing.clone().into_value() != *row {
                    return Err("A different server credential already exists. The original profile was kept.".into());
                }
            } else {
                target.insert(id.clone(), credential);
            }
        }
    }
    let object = current.object_mut().ok_or("Invalid key import receipts.")?;
    let imports = object.imports.get_or_insert(Value::Null);
    if imports.is_null() {
        *imports = json!([]);
    }
    imports
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
        fn read(&self) -> Result<VaultState, String> {
            Ok(VaultState::from_value(self.0.lock().unwrap().clone()))
        }
        fn write(&self, value: &VaultState) -> Result<(), String> {
            *self.0.lock().unwrap() = value.clone().into_value();
            Ok(())
        }
    }
    #[test]
    fn file_vault_keeps_keys_in_a_private_file() {
        let directory = tempfile::tempdir().unwrap();
        let vault = FileVault::new(directory.path());
        assert_eq!(
            vault.read().unwrap().into_value(),
            json!({"drafts":{},"servers":{}})
        );
        let state = json!({"drafts":{},"servers":{"a":{"origin":"https://a.ts.net"}}});
        vault.write(&VaultState::from_value(state.clone())).unwrap();
        assert_eq!(
            FileVault::new(directory.path())
                .read()
                .unwrap()
                .into_value(),
            state
        );
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(directory.path().join(SERVER_KEYS_FILE))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn vault_reopens_partial_and_duplicate_data_without_eager_validation() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(SERVER_KEYS_FILE);
        let vault = FileVault::new(directory.path());
        for raw in [
            r#"null"#,
            r#"[1,{"future":true}]"#,
            r#"{"drafts":null,"servers":{"a":null,"b":42,"c":{"serverId":"x","serverId":"y","future":[1,null]}} ,"imports":null,"future":{"v":1.0}}"#,
        ] {
            std::fs::write(&path, raw).unwrap();
            let expected: Value = serde_json::from_str(raw).unwrap();
            let actual = vault.read().unwrap().into_value();
            assert_eq!(actual, expected);
            vault
                .write(&VaultState::from_value(actual.clone()))
                .unwrap();
            assert_eq!(
                FileVault::new(directory.path())
                    .read()
                    .unwrap()
                    .into_value(),
                expected
            );
        }
        let archive = json!({"drafts":{},"servers":{}});
        let malformed = Memory(Mutex::new(
            json!({"drafts":{},"servers":{},"imports":7,"future":[1]}),
        ));
        let before = malformed.read().unwrap().into_value();
        assert!(import(&malformed, &archive).is_err());
        assert_eq!(malformed.read().unwrap().into_value(), before);
        let partial = Memory(Mutex::new(json!({"drafts":null,"servers":{}})));
        assert!(import(&partial, &archive).is_err());
        assert_eq!(
            partial.read().unwrap().into_value(),
            json!({"drafts":null,"servers":{}})
        );
    }

    #[test]
    fn keys_sign_exact_bytes_and_import_refuses_partial_or_conflicting_state() {
        let invitation = json!({"protocol":1,"origin":"https://fixture.tailnet.ts.net","serverId":"server","inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
        let credential = new_pair(&invitation, "request").unwrap().into_value();
        let signed = signature(
            &Credential::from_value(credential.clone()),
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
            key(&Credential::from_value(credential.clone()))
                .unwrap()
                .public_key()
                .as_ref(),
        )
        .verify(input.as_bytes(), &STANDARD.decode(signed).unwrap())
        .unwrap();
        let alias_vault = Memory(Mutex::new(
            json!({"drafts":{},"servers":{},"imports":null,"future":{"unchanged":true}}),
        ));
        let alias_archive =
            json!({"drafts":{},"servers":{"alias":credential},"archiveEvidence":[1,null]});
        import(&alias_vault, &alias_archive).unwrap();
        let imported = alias_vault.read().unwrap().into_value();
        assert_eq!(imported["servers"]["alias"], credential);
        assert_eq!(imported["future"], json!({"unchanged":true}));
        assert_eq!(imported["imports"].as_array().unwrap().len(), 1);
        import(&alias_vault, &alias_archive).unwrap();
        assert_eq!(alias_vault.read().unwrap().into_value(), imported);
        let directory = tempfile::tempdir().unwrap();
        let file = FileVault::new(directory.path());
        import(&file, &alias_archive).unwrap();
        let saved = FileVault::new(directory.path())
            .read()
            .unwrap()
            .into_value();
        import(&file, &alias_archive).unwrap();
        assert_eq!(file.read().unwrap().into_value(), saved);
        assert_eq!(saved["servers"]["alias"], credential);
        let vault = Memory(Mutex::new(json!({"drafts":{},"servers":{}})));
        let source = json!({"drafts":{},"servers":{"server":credential}});
        import(&vault, &source).unwrap();
        import(&vault, &source).unwrap();
        let mut conflict = source.clone();
        conflict["servers"]["server"]["clientId"] = json!("other");
        assert!(import(&vault, &conflict).is_err());
        assert_eq!(
            vault.read().unwrap().into_value()["servers"],
            source["servers"]
        );
        let mut invalid = source.clone();
        invalid["servers"]["bad"] = json!({"origin":"http://localhost"});
        assert!(import(&vault, &invalid).is_err());
        assert_eq!(
            vault.read().unwrap().into_value()["servers"],
            source["servers"]
        );
        // A completed import receipt survives later credential changes.
        let mut newer = vault.read().unwrap().into_value();
        newer["servers"]["server"] = new_pair(&invitation, "later").unwrap().into_value();
        vault.write(&VaultState::from_value(newer.clone())).unwrap();
        import(&vault, &source).unwrap();
        assert_eq!(vault.read().unwrap().into_value(), newer);
    }
}
