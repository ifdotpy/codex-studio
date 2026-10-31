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
fn key(pem: &str) -> Result<Ed25519KeyPair, String> {
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
/// The one key of this device. It signs for every paired server. Its id is `dev-` and the
/// SHA-256 of the SPKI public key in hex, as in the web client (`servers/deviceKey.ts`).
pub struct Device {
    pub id: String,
    pub public_key: String,
    private_key: String,
}

fn generate_device() -> Result<Device, String> {
    let generated = Ed25519KeyPair::generate_pkcs8(&SystemRandom::new())
        .map_err(|_| "Could not create a device key.")?;
    let key = Ed25519KeyPair::from_pkcs8(generated.as_ref())
        .map_err(|_| "Could not read the new device key.")?;
    let mut spki = vec![
        0x30, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
    ];
    spki.extend_from_slice(key.public_key().as_ref());
    let hash = Sha256::digest(&spki)
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
    Ok(Device {
        id: format!("dev-{hash}"),
        public_key: pem("PUBLIC KEY", &spki),
        private_key: pem("PRIVATE KEY", generated.as_ref()),
    })
}

fn saved_device(state: &VaultState) -> Result<Option<Device>, String> {
    let Some(value) = state.object().and_then(|v| v.device.as_ref()) else {
        return Ok(None);
    };
    let text = |name| field(value, name).map(str::to_owned);
    Ok(Some(Device {
        id: text("id")?,
        public_key: text("publicKey")?,
        private_key: text("privateKey")?,
    }))
}

/// The device key, created on first use. The caller serializes vault mutations.
pub fn ensure_device(vault: &dyn Vault) -> Result<Device, String> {
    let mut state = vault.read()?;
    if let Some(device) = saved_device(&state)? {
        return Ok(device);
    }
    let device = generate_device()?;
    state
        .object_mut()
        .ok_or("Invalid server key store.")?
        .device =
        Some(json!({"id":device.id,"publicKey":device.public_key,"privateKey":device.private_key}));
    vault.write(&state)?;
    Ok(device)
}

/// A pairing made before the device key keeps its own key and client id. Every other
/// credential signs with the device key.
fn signing_key(vault: &dyn Vault, credential: &Credential) -> Result<Ed25519KeyPair, String> {
    if !credential
        .object()
        .is_some_and(|v| matches!(v.private_key, TextField::Missing))
    {
        return key(credential.field("privateKey")?);
    }
    let device = saved_device(&vault.read()?)?.ok_or("The device key is unavailable.")?;
    if device.id != credential.field("clientId")? {
        return Err("The server key is unavailable. Pair this UI again.".into());
    }
    key(&device.private_key)
}

pub fn new_pair(
    invitation: &Value,
    request_id: &str,
    device: &Device,
) -> Result<Credential, String> {
    if invitation["protocol"] != 1 || field(invitation, "serverId")? == "local" {
        return Err("Invalid server invitation.".into());
    }
    let origin = origin(field(invitation, "origin")?)?;
    for name in ["inviteId", "token", "publicKey", "tailscaleUser"] {
        field(invitation, name)?;
    }
    let body = json!({"protocol":1,"inviteId":invitation["inviteId"],"token":invitation["token"],"clientId":device.id,"label":"Studio UI","kind":"ui","publicKey":device.public_key,"requestId":request_id}).to_string();
    let mut extra = serde_json::Map::new();
    extra.insert("publicKey".into(), Value::String(device.public_key.clone()));
    Ok(Credential::Object(Box::new(CredentialObject {
        origin: TextField::Text(origin),
        server_id: TextField::Text(field(invitation, "serverId")?.to_owned()),
        client_id: TextField::Text(device.id.clone()),
        invite_id: TextField::Text(field(invitation, "inviteId")?.to_owned()),
        private_key: TextField::Missing,
        body: TextField::Text(body),
        receipt: None,
        rejected: None,
        extra,
    })))
}

/// A pairing as made before the device key: its own key and a random client id.
#[cfg(test)]
pub(crate) fn legacy_pair(invitation: &Value, request_id: &str) -> Result<Credential, String> {
    let device = generate_device()?;
    let mut credential = new_pair(invitation, request_id, &device)?;
    let object = credential.object_mut().ok_or("Invalid credential.")?;
    object.private_key = TextField::Text(device.private_key);
    object.client_id = TextField::Text(random_id()?);
    Ok(credential)
}

pub fn signature(
    vault: &dyn Vault,
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
    Ok(STANDARD.encode(
        signing_key(vault, credential)?
            .sign(text.as_bytes())
            .as_ref(),
    ))
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
                key(credential.field("privateKey")?)?;
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
        let credential = legacy_pair(&invitation, "request").unwrap().into_value();
        let no_device = Memory(Mutex::new(json!({"drafts":{},"servers":{}})));
        let signed = signature(
            &no_device,
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
            key(credential["privateKey"].as_str().unwrap())
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

    fn public_bytes(pem: &str) -> Vec<u8> {
        let body = pem
            .lines()
            .filter(|line| !line.starts_with("-----"))
            .collect::<String>();
        // The last 32 bytes of the SPKI document are the raw Ed25519 key.
        STANDARD.decode(body).unwrap()[12..].to_vec()
    }
    fn signs_for(vault: &dyn Vault, credential: &Value, public: &[u8]) {
        let signed = signature(
            vault,
            &Credential::from_value(credential.clone()),
            "GET",
            "/api/x",
            "1",
            "nonce",
            "request",
            b"",
        )
        .unwrap();
        let digest = Sha256::digest(b"")
            .iter()
            .map(|v| format!("{v:02x}"))
            .collect::<String>();
        let input = format!(
            "studio-multi-server-v1\nGET\n/api/x\n{}\n{}\n1\nnonce\nrequest\n{digest}",
            credential["serverId"].as_str().unwrap(),
            credential["clientId"].as_str().unwrap()
        );
        ring::signature::UnparsedPublicKey::new(&ring::signature::ED25519, public)
            .verify(input.as_bytes(), &STANDARD.decode(signed).unwrap())
            .unwrap();
    }
    fn invitation(server: &str) -> Value {
        json!({"protocol":1,"origin":format!("https://{server}.tailnet.ts.net"),"serverId":server,"inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"})
    }

    #[test]
    fn one_device_key_signs_for_two_paired_servers() {
        let vault = Memory(Mutex::new(json!({"drafts":{},"servers":{}})));
        let device = ensure_device(&vault).unwrap();
        assert!(device.id.starts_with("dev-") && device.id.len() == 68);
        // The key is created once and kept.
        assert_eq!(ensure_device(&vault).unwrap().id, device.id);
        let public = public_bytes(&device.public_key);
        let hash = Sha256::digest(
            [
                &[
                    0x30u8, 0x2a, 0x30, 0x05, 0x06, 0x03, 0x2b, 0x65, 0x70, 0x03, 0x21, 0x00,
                ][..],
                &public,
            ]
            .concat(),
        )
        .iter()
        .map(|v| format!("{v:02x}"))
        .collect::<String>();
        assert_eq!(device.id, format!("dev-{hash}"));
        let first = new_pair(&invitation("a"), "r1", &device)
            .unwrap()
            .into_value();
        let second = new_pair(&invitation("b"), "r2", &device)
            .unwrap()
            .into_value();
        assert_eq!(first["clientId"], second["clientId"]);
        assert_eq!(first["clientId"], json!(device.id));
        assert!(first.get("privateKey").is_none() && second.get("privateKey").is_none());
        signs_for(&vault, &first, &public);
        signs_for(&vault, &second, &public);
    }

    #[test]
    fn a_pairing_made_before_the_device_key_still_signs() {
        let old = legacy_pair(&invitation("old"), "r0").unwrap().into_value();
        let old_public = public_bytes(old["publicKey"].as_str().unwrap());
        let vault = Memory(Mutex::new(json!({"drafts":{},"servers":{"old":old}})));
        let device = ensure_device(&vault).unwrap();
        let state = vault.read().unwrap().into_value();
        // The new device key does not touch the old pairing: its client id and key stay.
        assert_eq!(state["servers"]["old"], old);
        assert_ne!(old["clientId"], json!(device.id));
        signs_for(&vault, &old, &old_public);
        let fresh = new_pair(&invitation("new"), "r1", &device)
            .unwrap()
            .into_value();
        signs_for(&vault, &fresh, &public_bytes(&device.public_key));
        // A key-less credential of another client id is refused.
        let mut other = fresh.clone();
        other["clientId"] = json!("other");
        assert!(
            signature(
                &vault,
                &Credential::from_value(other),
                "GET",
                "/api/x",
                "1",
                "n",
                "r",
                b""
            )
            .is_err()
        );
    }
}
