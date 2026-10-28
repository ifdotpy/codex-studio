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
    fn read(&self) -> Result<Value, String> {
        match std::fs::read(&self.path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .map_err(|_| "The server key store is invalid.".into()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(json!({"drafts":{},"servers":{}}))
            }
            Err(_) => Err("The server key store could not be opened.".into()),
        }
    }
    fn write(&self, state: &Value) -> Result<(), String> {
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

/// Server management (pairing, access, the server-to-server protocol) belongs
/// to the workspace shell of the local server. A device key for a paired
/// server never sends these requests. `path` is the decoded URL path.
pub fn management_request(method: &str, path: &str) -> bool {
    let path = path.trim_end_matches('/');
    path.starts_with("/api/multi-server/v1/")
        || (method == "POST"
            && (path == "/api/multi-server" || path.starts_with("/api/multi-server/")))
}

/// Authentication headers of one signed request to a paired server. The
/// server checks the signature over the method, target, identities, time,
/// nonce, request ID and the SHA-256 of the exact body bytes.
pub fn signed_headers(
    credential: &Value,
    method: &str,
    target: &str,
    request_id: &str,
    body: &[u8],
) -> Result<Vec<(&'static str, String)>, String> {
    if request_id.is_empty() || request_id.len() > 128 || request_id.contains(['\n', '\r']) {
        return Err("Invalid server request identity.".into());
    }
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|e| e.to_string())?
        .as_secs()
        .to_string();
    let nonce = request_nonce()?;
    let signature = signature(
        credential, method, target, &timestamp, &nonce, request_id, body,
    )?;
    Ok(vec![
        ("x-studio-client", field(credential, "clientId")?.to_owned()),
        ("x-studio-server", field(credential, "serverId")?.to_owned()),
        ("x-studio-timestamp", timestamp),
        ("x-studio-nonce", nonce),
        ("x-studio-request-id", request_id.to_owned()),
        ("x-studio-signature", signature),
    ])
}

/// Signs the requests of one client core to one paired server.
pub trait Signer: Send + Sync {
    /// The exact origin of the paired server. A core sends signed requests only there.
    fn origin(&self) -> &str;
    /// Authentication headers for one request.
    fn headers(
        &self,
        method: &str,
        target: &str,
        request_id: &str,
        body: &[u8],
    ) -> Result<Vec<(&'static str, String)>, String>;
}

/// The key of one paired server in a vault. Each request reads the vault
/// again, so a forgotten or replaced pairing stops signing at once.
pub struct VaultSigner {
    vault: std::sync::Arc<dyn Vault>,
    server: String,
    client: String,
    origin: String,
}
impl VaultSigner {
    pub fn new(vault: std::sync::Arc<dyn Vault>, server: &str) -> Result<Self, String> {
        let state = vault.read()?;
        let credential = &state["servers"][server];
        if credential.is_null() {
            return Err("This device is not paired with the server.".into());
        }
        let origin = origin(field(credential, "origin")?)?;
        if field(credential, "serverId")? != server {
            return Err("The saved server key belongs to another server.".into());
        }
        Ok(Self {
            client: field(credential, "clientId")?.to_owned(),
            server: server.to_owned(),
            vault,
            origin,
        })
    }
}
impl Signer for VaultSigner {
    fn origin(&self) -> &str {
        &self.origin
    }
    fn headers(
        &self,
        method: &str,
        target: &str,
        request_id: &str,
        body: &[u8],
    ) -> Result<Vec<(&'static str, String)>, String> {
        let state = self.vault.read()?;
        let credential = &state["servers"][&self.server];
        if credential.is_null()
            || field(credential, "clientId")? != self.client
            || origin(field(credential, "origin")?)? != self.origin
        {
            return Err("The server key changed or was removed. Open the server again.".into());
        }
        signed_headers(credential, method, target, request_id, body)
    }
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
    fn file_vault_keeps_keys_in_a_private_file() {
        let directory = tempfile::tempdir().unwrap();
        let vault = FileVault::new(directory.path());
        assert_eq!(vault.read().unwrap(), json!({"drafts":{},"servers":{}}));
        let state = json!({"drafts":{},"servers":{"a":{"origin":"https://a.ts.net"}}});
        vault.write(&state).unwrap();
        assert_eq!(FileVault::new(directory.path()).read().unwrap(), state);
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

    #[test]
    fn vault_signer_signs_only_for_its_paired_server_and_stops_after_forget() {
        let invitation = json!({"protocol":1,"origin":"https://a.tailnet.ts.net","serverId":"a","inviteId":"invite","token":"test","publicKey":"fixture","tailscaleUser":"user"});
        let credential = new_pair(&invitation, "request").unwrap();
        let vault: std::sync::Arc<dyn Vault> = std::sync::Arc::new(Memory(Mutex::new(
            json!({"drafts":{},"servers":{"a":credential}}),
        )));
        assert!(VaultSigner::new(vault.clone(), "b").is_err());
        let signer = VaultSigner::new(vault.clone(), "a").unwrap();
        assert_eq!(signer.origin(), "https://a.tailnet.ts.net");
        let headers = signer
            .headers("GET", "/api/sync/identity", "id", b"")
            .unwrap();
        assert!(headers.contains(&("x-studio-server", "a".to_owned())));
        assert!(headers.contains(&("x-studio-request-id", "id".to_owned())));
        assert!(
            headers
                .iter()
                .all(|(_, value)| !value.contains("PRIVATE KEY"))
        );
        vault.write(&json!({"drafts":{},"servers":{}})).unwrap();
        assert!(
            signer
                .headers("GET", "/api/sync/identity", "id", b"")
                .is_err()
        );
    }
}
