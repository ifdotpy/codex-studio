use serde_json::{Value, json};
use std::{
    io::Read,
    path::{Path, PathBuf},
    sync::OnceLock,
};

const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
static ARCHIVE: OnceLock<Result<Option<(Value, PathBuf)>, String>> = OnceLock::new();

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
pub fn snapshot(owner: &str) -> Result<Value, String> {
    match archive()? {
        Some((value, _)) => select(value, owner),
        None => Ok(Value::Null),
    }
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
        archive["owners"].as_object_mut().unwrap().remove("root");
        assert!(select_workspace(&archive).is_err());
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
