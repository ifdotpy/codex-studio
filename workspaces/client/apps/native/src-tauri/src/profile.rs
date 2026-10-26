use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::OnceLock,
};

const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
static ARCHIVE: OnceLock<Result<Option<(Value, PathBuf)>, String>> = OnceLock::new();

pub(crate) fn migration_policy() -> &'static Value {
    static POLICY: OnceLock<Value> = OnceLock::new();
    POLICY.get_or_init(|| {
        serde_json::from_str(include_str!("../../../../contracts/native-migration.json"))
            .expect("valid native migration contract")
    })
}

pub(crate) fn valid_sha256(value: &str) -> bool {
    value.len() == <sha2::Sha256 as sha2::Digest>::output_size() * 2
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

/// Host-only native directory coverage. The profile lock owns the root path.
/// This is separate from WebKit origin coverage and never exports file paths.
pub(crate) struct RetainedNativeProfiles {
    pub(crate) server_ids: Vec<String>,
    pub(crate) evidence: Value,
}
pub(crate) fn retained_native_profiles(
    profile: &Path,
    paired_storage: &std::collections::BTreeMap<String, String>,
) -> Result<RetainedNativeProfiles, String> {
    use std::{
        collections::{BTreeMap, BTreeSet},
        os::unix::fs::MetadataExt,
    };
    fn metadata(path: &Path, directory: bool) -> Result<Option<std::fs::Metadata>, String> {
        let value = match std::fs::symlink_metadata(path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(_) => return Err("The retained native profile metadata cannot be read.".into()),
        };
        if value.file_type().is_symlink() {
            return Err("The retained native profile contains symbolic storage.".into());
        }
        if value.is_dir() != directory
            || (!directory && !value.is_file())
            || value.uid() != unsafe { libc::geteuid() }
            || value.mode() & 0o022 != 0
        {
            return Err(
                "The retained native profile metadata has an unsupported type or owner.".into(),
            );
        }
        Ok(Some(value))
    }
    fn entries(path: &Path) -> Result<Vec<std::ffi::OsString>, String> {
        let mut names = std::fs::read_dir(path)
            .map_err(|_| "The retained native profile cannot be listed.")?
            .map(|entry| {
                entry
                    .map(|entry| entry.file_name())
                    .map_err(|_| "The retained native profile entry cannot be read.".to_owned())
            })
            .collect::<Result<Vec<_>, _>>()?;
        names.sort();
        Ok(names)
    }
    fn record(evidence: &mut BTreeMap<String, Value>, key: &str, value: &std::fs::Metadata) {
        evidence.insert(
            key.into(),
            json!({"device":value.dev(),"inode":value.ino()}),
        );
    }
    fn store(path: &Path, key: &str, evidence: &mut BTreeMap<String, Value>) -> Result<(), String> {
        let Some(value) = metadata(path, true)? else {
            return Ok(());
        };
        record(evidence, key, &value);
        // native::open_store owns these names. The real Client fixture checks
        // parity, including an acknowledged Store read before this audit.
        for name in entries(path)? {
            let name = name
                .to_str()
                .ok_or("The retained native profile filename is invalid.")?;
            if !matches!(
                name,
                "studio-client.sqlite3"
                    | "studio-client.sqlite3-wal"
                    | "studio-client.sqlite3-shm"
                    | "studio-native-client.lock"
            ) {
                return Err("The retained native profile contains an unknown file.".into());
            }
            let value = metadata(&path.join(name), false)?
                .ok_or("The retained native profile file changed.")?;
            // SQLite's WAL and shared-memory files can disappear after a normal
            // owner close. Their content remains covered by SQLite, not a stale
            // inode assertion in a migration proof.
            if !name.ends_with("-wal") && !name.ends_with("-shm") {
                record(evidence, &format!("{key}/{name}"), &value);
            }
        }
        Ok(())
    }
    let mut storage = BTreeSet::new();
    let mut servers = BTreeSet::from(["local".to_owned()]);
    for (server, key) in paired_storage {
        if server == "local"
            || server.is_empty()
            || server.len() > 128
            || !server
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
            || !valid_sha256(key)
            || !storage.insert(key.clone())
        {
            return Err("The retained native profile binding is invalid or ambiguous.".into());
        }
        servers.insert(server.clone());
    }
    let root = profile
        .canonicalize()
        .map_err(|_| "The locked native profile cannot be found.")?;
    let mut evidence = BTreeMap::new();
    record(
        &mut evidence,
        "profile",
        &metadata(&root, true)?.ok_or("The locked native profile disappeared.")?,
    );
    let client = root.join("client");
    if let Some(value) = metadata(&client, true)? {
        record(&mut evidence, "client", &value);
        for name in entries(&client)? {
            match name.to_str() {
                Some("local") => store(&client.join("local"), "local", &mut evidence)?,
                Some("servers") => {
                    let directory = client.join("servers");
                    record(
                        &mut evidence,
                        "servers",
                        &metadata(&directory, true)?
                            .ok_or("The retained server profile directory changed.")?,
                    );
                    for name in entries(&directory)? {
                        let name = name
                            .to_str()
                            .ok_or("The retained server profile name is invalid.")?;
                        if !storage.contains(name) {
                            return Err("An unmapped retained native profile requires review. Its source was preserved.".into());
                        }
                        store(
                            &directory.join(name),
                            &format!("servers/{name}"),
                            &mut evidence,
                        )?;
                    }
                }
                _ => {
                    return Err("The retained native profile contains an unknown directory.".into());
                }
            }
        }
    }
    Ok(RetainedNativeProfiles {
        server_ids: servers.into_iter().collect(),
        evidence: json!(evidence),
    })
}

fn read_archive(path: &Path) -> Result<Value, String> {
    let metadata = std::fs::metadata(path).map_err(|e| e.to_string())?;
    if !metadata.is_file() || metadata.len() > MAX_ARCHIVE {
        return Err("The profile archive must be a regular file below 256 MiB.".into());
    }
    let mut bytes = Vec::new();
    std::fs::File::open(path)
        .map_err(|e| e.to_string())?
        .take(MAX_ARCHIVE + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    if bytes.len() as u64 > MAX_ARCHIVE {
        return Err("The profile archive exceeds 256 MiB.".into());
    }
    serde_json::from_slice(&bytes).map_err(|e| e.to_string())
}

fn select(archive: &Value, owner: &str) -> Result<Value, String> {
    let id = archive["id"]
        .as_str()
        .filter(|v| !v.is_empty())
        .ok_or("The profile archive has no identity.")?;
    if archive["version"] != 1 && archive["version"] != 2 {
        return Err("Unsupported profile archive version.".into());
    }
    let owners = archive["owners"]
        .as_object()
        .ok_or("The profile archive needs an explicit destination owner mapping.")?;
    let origin = owners
        .get(owner)
        .and_then(Value::as_str)
        .ok_or("The profile archive does not map this destination owner.")?;
    let snapshots = archive["snapshots"]
        .as_array()
        .ok_or("Invalid profile snapshots.")?;
    let matching: Vec<_> = snapshots
        .iter()
        .filter(|v| v["origin"].as_str() == Some(origin))
        .collect();
    if matching.len() != 1 {
        return Err("The profile archive must contain exactly one snapshot for this owner.".into());
    }
    Ok(json!({"id":id, "snapshot":matching[0]}))
}

fn archive() -> Result<Option<&'static (Value, PathBuf)>, String> {
    match ARCHIVE.get_or_init(|| {
        std::env::var_os("CODEX_NATIVE_PROFILE_IMPORT")
            .map(|path| {
                let path = Path::new(&path).canonicalize().map_err(|e| e.to_string())?;
                Ok((read_archive(&path)?, path))
            })
            .transpose()
    }) {
        Ok(value) => Ok(value.as_ref()),
        Err(error) => Err(error.clone()),
    }
}

/// Cover every configured snapshot, including empty ones. The host never
/// treats an omitted owner mapping as an empty or irrelevant profile.
pub(crate) fn configured_archive_owners() -> Result<Vec<String>, String> {
    let Some((value, _)) = archive()? else {
        return Ok(Vec::new());
    };
    let object = value
        .as_object()
        .ok_or("The profile archive container is invalid.")?;
    if object.keys().any(|key| {
        !matches!(
            key.as_str(),
            "version" | "id" | "owners" | "snapshots" | "parts" | "profile" | "created"
        )
    }) {
        return Err(
            "The profile archive has an unknown source field. Its source remains held.".into(),
        );
    }
    select_workspace(value)?;
    let owners = value["owners"]
        .as_object()
        .ok_or("The archive owner inventory is missing.")?;
    let mapped = owners
        .values()
        .map(|origin| {
            origin
                .as_str()
                .ok_or("The archive source origin is invalid.")
        })
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    for snapshot in value["snapshots"]
        .as_array()
        .ok_or("The archive source inventory is invalid.")?
    {
        if snapshot["version"] != value["version"]
            || snapshot.as_object().is_none_or(|object| {
                object.keys().any(|key| {
                    !matches!(
                        key.as_str(),
                        "version" | "origin" | "preferences" | "databases"
                    )
                })
            })
        {
            return Err("The archive snapshot layout is unknown. Its source remains held.".into());
        }
        if !mapped.contains(
            snapshot["origin"]
                .as_str()
                .ok_or("The archive snapshot origin is invalid.")?,
        ) {
            return Err(
                "An archive snapshot has no explicit destination owner. Its source remains held."
                    .into(),
            );
        }
    }
    Ok(owners.keys().cloned().collect())
}

pub(crate) fn optional_sealed(owner: &str) -> Result<Option<SealedSource<'static>>, String> {
    let Some((value, path)) = archive()? else {
        return Ok(None);
    };
    let owners = value["owners"]
        .as_object()
        .ok_or("The archive owner inventory is invalid.")?;
    if !owners.contains_key(owner) {
        return Ok(None);
    }
    select_sealed(value, path, owner).map(Some)
}
pub(crate) fn optional_seal_identity(owner: &str) -> Result<Value, String> {
    Ok(match optional_sealed(owner)? {
        Some(source) => {
            json!({"id":source.identity["id"],"owner":owner,"sha256":source.identity["sha256"]})
        }
        None => Value::Null,
    })
}

pub(crate) fn archive_fingerprint() -> Result<String, String> {
    use sha2::{Digest, Sha256};
    let Some((value, path)) = archive()? else {
        return Ok(Sha256::digest(b"no-configured-profile-archive")
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect());
    };
    if read_archive(path)? != *value {
        return Err(
            "The configured archive source changed. Its original capture remains held.".into(),
        );
    }
    for owner in configured_archive_owners()? {
        let selected = select_sealed(value, path, &owner)?;
        for db in selected.snapshot["databases"]
            .as_array()
            .ok_or("The archive database inventory is invalid.")?
        {
            for store in db["stores"]
                .as_array()
                .ok_or("The archive store inventory is invalid.")?
            {
                if let Some(parts) = store.get("chunks") {
                    for part in parts
                        .as_array()
                        .ok_or("The archive chunk inventory is invalid.")?
                    {
                        read_chunk(
                            value,
                            path,
                            &owner,
                            part["sha256"]
                                .as_str()
                                .ok_or("The archive chunk identity is invalid.")?,
                        )?;
                    }
                }
            }
        }
    }
    Ok(Sha256::digest(
        serde_json::to_vec(value).map_err(|_| "The archive source cannot be encoded.")?,
    )
    .iter()
    .map(|byte| format!("{byte:02x}"))
    .collect())
}
pub fn snapshot(owner: &str) -> Result<Value, String> {
    match archive()? {
        Some((value, _)) => select(value, owner),
        None => Ok(Value::Null),
    }
}

/// Seal the exact host-selected source bytes, independent of renderer JSON order.
pub fn seal(owner: &str) -> Result<Value, String> {
    selected_seal(owner, true)
}

/// Compact identity for repeated proof checks, without copying source bytes.
pub fn seal_identity(owner: &str) -> Result<Value, String> {
    selected_seal(owner, false)
}

fn selected_seal(owner: &str, include_bytes: bool) -> Result<Value, String> {
    static SEALS: OnceLock<std::sync::Mutex<std::collections::HashMap<String, Value>>> =
        OnceLock::new();
    let mut seals = SEALS
        .get_or_init(Default::default)
        .lock()
        .map_err(|_| "The profile seal registry is unavailable.")?;
    if !seals.contains_key(owner) {
        let Some((archive, _)) = archive()? else {
            return Ok(Value::Null);
        };
        seals.insert(owner.into(), seal_selected(archive, owner)?);
    }
    let seal = &seals[owner];
    Ok(if include_bytes {
        seal.clone()
    } else {
        json!({"id":seal["id"],"owner":seal["owner"],"sha256":seal["sha256"]})
    })
}

fn seal_selected(archive: &Value, owner: &str) -> Result<Value, String> {
    use sha2::{Digest, Sha256};
    let selected = select(archive, owner)?;
    let serialized = serde_json::to_string(&selected["snapshot"]).map_err(|e| e.to_string())?;
    let sha256: String = Sha256::digest(serialized.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    Ok(json!({"id":selected["id"],"owner":owner,"sha256":sha256,"serializedSnapshot":serialized}))
}

fn select_workspace(archive: &Value) -> Result<Value, String> {
    let owners = archive["owners"]
        .as_object()
        .ok_or("Missing profile owners.")?;
    if !owners.contains_key("root") {
        return Err("The profile archive needs a root owner.".into());
    }
    let mut profiles = Vec::new();
    for owner in owners.keys() {
        if owner != "root"
            && !owner.strip_prefix("server:").is_some_and(|id| {
                !id.is_empty()
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
            })
        {
            return Err("Invalid profile owner.".into());
        }
        let selected = select(archive, owner)?;
        profiles.push(json!({"owner":owner,"snapshot":selected["snapshot"]}));
    }
    Ok(json!({"id":archive["id"],"profiles":profiles}))
}

pub fn workspace() -> Result<Value, String> {
    match archive()? {
        Some((value, _)) => select_workspace(value),
        None => Ok(Value::Null),
    }
}

/// Only host code can construct a selected source. Renderer labels are not a source.
pub(crate) struct SealedSource<'a> {
    archive: &'a Value,
    path: &'a Path,
    owner: String,
    pub(crate) snapshot: Value,
    pub(crate) identity: Value,
}
pub(crate) fn sealed(owner: &str) -> Result<SealedSource<'static>, String> {
    let (archive, path) = archive()?.ok_or("No profile import is configured.")?;
    select_sealed(archive, path, owner)
}
pub(crate) fn select_sealed<'a>(
    archive: &'a Value,
    path: &'a Path,
    owner: &str,
) -> Result<SealedSource<'a>, String> {
    Ok(SealedSource {
        archive,
        path,
        owner: owner.into(),
        snapshot: select(archive, owner)?["snapshot"].clone(),
        identity: seal_selected(archive, owner)?,
    })
}
impl SealedSource<'_> {
    pub(crate) fn records(&self, store: &Value) -> Result<Vec<Value>, String> {
        match self.snapshot["version"].as_u64() {
            Some(1) if store.get("chunks").is_none() => {
                let records = store["records"]
                    .as_array()
                    .ok_or("Invalid inline archive store.")?;
                if records
                    .iter()
                    .map(|record| record.to_string().len())
                    .sum::<usize>()
                    > crate::archive_import::MAX_PREPARE_RECORD_BYTES
                {
                    return Err("The host archive preparation record budget was exceeded. Its source remains held.".into());
                }
                Ok(records.clone())
            }
            Some(2) if store.get("records").is_none() => {
                let mut records = Vec::new();
                let mut encoded_bytes = 0usize;
                for part in store["chunks"].as_array().ok_or("Invalid archive parts.")? {
                    if part.get("owner").is_some_and(|owner| owner != &self.owner) {
                        return Err("The archive part belongs to another owner.".into());
                    }
                    let digest = part["sha256"]
                        .as_str()
                        .ok_or("Missing archive part digest.")?;
                    let text = read_chunk(self.archive, self.path, &self.owner, digest)?;
                    let text = text.as_str().ok_or("Invalid archive part text.")?;
                    encoded_bytes=encoded_bytes.checked_add(text.len()).filter(|bytes|*bytes<=crate::archive_import::MAX_PREPARE_RECORD_BYTES)
                        .ok_or("The host archive preparation record budget was exceeded. Its source remains held.")?;
                    if part["bytes"].as_u64() != Some(text.len() as u64) {
                        return Err("The archive part length changed.".into());
                    }
                    let page: Vec<Value> = serde_json::from_str(text).map_err(|e| e.to_string())?;
                    if part["records"].as_u64() != Some(page.len() as u64) {
                        return Err("The archive part record count changed.".into());
                    }
                    records.extend(page);
                }
                Ok(records)
            }
            _ => Err("Unsupported archive store representation.".into()),
        }
    }
}

fn read_chunk(archive: &Value, path: &Path, owner: &str, digest: &str) -> Result<Value, String> {
    use sha2::{Digest, Sha256};
    if archive["version"] != 2
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("Invalid profile chunk.".into());
    }
    let selected = select(archive, owner)?;
    let reference = selected["snapshot"]["databases"]
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|db| db["stores"].as_array().into_iter().flatten())
        .flat_map(|store| store["chunks"].as_array().into_iter().flatten())
        .find(|chunk| chunk["sha256"].as_str() == Some(digest))
        .ok_or("This chunk does not belong to the profile owner.")?;
    let parts = archive["parts"]
        .as_str()
        .filter(|s| {
            !s.is_empty() && !s.contains('/') && !s.contains('\\') && *s != "." && *s != ".."
        })
        .ok_or("Invalid profile chunk directory.")?;
    let directory = path
        .parent()
        .ok_or("Invalid profile archive path.")?
        .join(parts);
    if directory.canonicalize().map_err(|e| e.to_string())? != directory {
        return Err("Profile chunks cannot use a redirected directory.".into());
    }
    let chunk = directory.join(format!("{digest}.json"));
    let metadata = std::fs::symlink_metadata(&chunk).map_err(|e| e.to_string())?;
    if !metadata.is_file()
        || metadata.len() > 64 * 1024 * 1024
        || reference["bytes"].as_u64() != Some(metadata.len())
    {
        return Err("The profile chunk size changed.".into());
    }
    let mut bytes = Vec::new();
    use std::os::unix::fs::OpenOptionsExt;
    std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(chunk)
        .map_err(|e| e.to_string())?
        .take(64 * 1024 * 1024 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != digest {
        return Err("The profile chunk checksum changed.".into());
    }
    let records: Value = serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if !records.is_array()
        || records.as_array().map(|v| v.len() as u64) != reference["records"].as_u64()
    {
        return Err("The profile chunk record count changed.".into());
    }
    // Keep a large nested array out of WebKit's callback source. The renderer
    // verifies the exact text bytes before decoding with JSON.parse.
    Ok(Value::String(
        String::from_utf8(bytes).map_err(|e| e.to_string())?,
    ))
}
pub fn chunk(owner: &str, digest: &str) -> Result<Value, String> {
    let (value, path) = archive()?.ok_or("No profile import is configured.")?;
    read_chunk(value, path, owner, digest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn native_host_capture_profile_inventory_includes_empty_paired_and_native_stores() {
        use studio_client_core::{backend::Backend, client::Client};
        let directory = tempfile::tempdir().unwrap();
        let local = Client::open_with_backend(
            directory.path().join("client/local"),
            Backend::new("http://127.0.0.1:9").unwrap(),
            |_| {},
        )
        .unwrap();
        let remote = Client::open_with_backend(
            directory.path().join("client/servers").join("a".repeat(64)),
            Backend::new("http://127.0.0.1:9").unwrap(),
            |_| {},
        )
        .unwrap();
        for client in [&local, &remote] {
            assert!(
                client
                    .request(studio_client_core::protocol::Request {
                        id: "fixture-initialized".into(),
                        method: "preferences.raw_all".into(),
                        params: json!({})
                    })
                    .await
                    .unwrap()
                    .error
                    .is_none()
            );
        }
        let paired = std::collections::BTreeMap::from([
            ("first".into(), "a".repeat(64)),
            ("empty".into(), "b".repeat(64)),
        ]);
        let inventory = retained_native_profiles(directory.path(), &paired)
            .expect("Every existing and empty paired native profile needs host coverage");
        assert_eq!(inventory.server_ids, vec!["empty", "first", "local"]);
        assert!(inventory.evidence.is_object());
        std::fs::create_dir(directory.path().join("client/servers").join("c".repeat(64))).unwrap();
        assert!(
            retained_native_profiles(directory.path(), &paired).is_err(),
            "An unmapped retained profile cannot disappear, even when its directory is empty"
        );
        drop((local, remote));
    }

    #[tokio::test]
    async fn native_host_capture_profile_inventory_refuses_symlinked_storage() {
        use studio_client_core::{backend::Backend, client::Client};
        let directory = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let local = Client::open_with_backend(
            directory.path().join("client/local"),
            Backend::new("http://127.0.0.1:9").unwrap(),
            |_| {},
        )
        .unwrap();
        assert!(
            local
                .request(studio_client_core::protocol::Request {
                    id: "fixture-initialized".into(),
                    method: "preferences.raw_all".into(),
                    params: json!({})
                })
                .await
                .unwrap()
                .error
                .is_none()
        );
        std::fs::create_dir(directory.path().join("client/servers")).unwrap();
        std::os::unix::fs::symlink(
            outside.path(),
            directory.path().join("client/servers").join("a".repeat(64)),
        )
        .unwrap();
        let paired = std::collections::BTreeMap::from([("first".into(), "a".repeat(64))]);
        assert!(retained_native_profiles(directory.path(), &paired).is_err());
        assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
        drop(local);
    }
    #[test]
    fn native_profile_seal_binds_exact_selected_bytes_and_owner() {
        use sha2::{Digest, Sha256};
        let archive = json!({"version":1,"id":"archive","owners":{"root":"local","server:remote":"remote"},"snapshots":[{"origin":"local","preferences":{"2":"two","10":"ten"}},{"origin":"remote","preferences":{}}]});
        let seal = seal_selected(&archive, "root").unwrap();
        let raw = seal["serializedSnapshot"].as_str().unwrap();
        let hash: String = Sha256::digest(raw.as_bytes())
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect();
        assert_eq!(seal["sha256"], hash);
        assert_eq!(seal["owner"], "root");
        assert_eq!(
            serde_json::from_str::<Value>(raw).unwrap(),
            select(&archive, "root").unwrap()["snapshot"]
        );
        assert_ne!(
            seal["sha256"],
            seal_selected(&archive, "server:remote").unwrap()["sha256"]
        );
        assert!(seal_selected(&archive, "server:missing").is_err());
    }
    #[test]
    fn workspace_requires_explicit_complete_owner_mappings() {
        let mut archive = json!({"version":1,"id":"all-owners",
            "owners":{"root":"local","server:remote":"remote"},
            "snapshots":[{"origin":"local","version":1},{"origin":"remote","version":1}]});
        let workspace = select_workspace(&archive).unwrap();
        assert_eq!(workspace["profiles"].as_array().unwrap().len(), 2);
        assert_eq!(workspace["id"], "all-owners");
        archive["owners"]["server:missing"] = json!("absent");
        assert!(select_workspace(&archive).is_err());
        archive["owners"]
            .as_object_mut()
            .unwrap()
            .remove("server:missing");
        archive["owners"]["server:../other"] = json!("remote");
        assert!(select_workspace(&archive).is_err());
        archive["owners"] = json!({"server:remote":"remote"});
        assert_eq!(
            select_workspace(&archive).expect_err("missing root"),
            "The profile archive needs a root owner."
        );
    }
    #[test]
    fn chunks_require_the_owner_exact_bytes_and_a_regular_file() {
        use sha2::{Digest, Sha256};
        let dir = std::env::temp_dir().join(format!(
            "studio-chunks-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(dir.join("parts")).unwrap();
        let dir = dir.canonicalize().unwrap();
        let bytes = br#"[[["value","key"],["value","data"]]]"#;
        let hash: String = Sha256::digest(bytes)
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        let file = dir.join("parts").join(format!("{hash}.json"));
        std::fs::write(&file, bytes).unwrap();
        let mut archive = json!({"version":2,"id":"test","parts":"parts",
          "owners":{"root":"local","server:remote":"remote"},
          "snapshots":[{"origin":"local","databases":[{"stores":[{"chunks":[{"sha256":hash,"bytes":bytes.len(),"records":1}]}]}]},
                       {"origin":"remote","databases":[]}]});
        let path = dir.join("archive.json");
        assert_eq!(
            serde_json::from_str::<Value>(
                read_chunk(&archive, &path, "root", &hash)
                    .unwrap()
                    .as_str()
                    .unwrap()
            )
            .unwrap()
            .as_array()
            .unwrap()
            .len(),
            1
        );
        assert!(read_chunk(&archive, &path, "server:remote", &hash).is_err());
        assert!(read_chunk(&archive, &path, "root", "../escape").is_err());
        archive["parts"] = json!("../parts");
        assert!(read_chunk(&archive, &path, "root", &hash).is_err());
        archive["parts"] = json!("parts");
        std::fs::write(&file, br#"[[["value","key"],["value","evil"]]]"#).unwrap();
        assert!(read_chunk(&archive, &path, "root", &hash).is_err());
        std::fs::remove_file(&file).unwrap();
        let target = dir.join("target");
        std::fs::write(&target, bytes).unwrap();
        std::os::unix::fs::symlink(&target, &file).unwrap();
        assert!(read_chunk(&archive, &path, "root", &hash).is_err());
        std::fs::remove_dir_all(dir).unwrap();
    }
    #[test]
    fn archive_selection_is_explicit_and_never_falls_back_to_another_owner() {
        let mut archive = json!({"version":1,"id":"stable", "owners":{"root":"http://127.0.0.1:4620"},
            "snapshots":[{"origin":"http://127.0.0.1:4620", "preferences":{"key":"value"}}]});
        assert_eq!(
            select(&archive, "root").unwrap()["snapshot"]["preferences"]["key"],
            "value"
        );
        assert!(select(&archive, "server:remote").is_err());
        let duplicate = archive["snapshots"][0].clone();
        archive["snapshots"].as_array_mut().unwrap().push(duplicate);
        assert!(select(&archive, "root").is_err());
    }
}
