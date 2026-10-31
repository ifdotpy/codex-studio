use serde::Deserialize;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    io::Read,
    path::{Path, PathBuf},
    sync::OnceLock,
};

const MAX_ARCHIVE: u64 = 256 * 1024 * 1024;
// Maximum profile chunk file size in bytes; shared with the web importer.
const MAX_CHUNK_BYTES: u64 = 64 * 1024 * 1024;
// These are deliberately partial views of the archive contract owned by
// web/src/native/importBrowserProfile.ts. Invalid fields stay deferred until
// the operation that needs them; snapshots remain raw for the renderer.
#[derive(Default, Deserialize)]
#[serde(untagged)]
enum Field<T> {
    Valid(T),
    Invalid(serde::de::IgnoredAny),
    #[default]
    #[serde(skip)]
    Missing,
}
impl<T> Field<T> {
    fn get(&self) -> Option<&T> {
        match self {
            Self::Valid(value) => Some(value),
            Self::Invalid(_) | Self::Missing => None,
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default)]
struct Archive {
    id: Field<String>,
    version: Field<u64>,
    owners: Field<BTreeMap<String, Field<String>>>,
    snapshots: Field<Vec<Value>>,
    parts: Field<String>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct Origin<'a> {
    #[serde(borrow)]
    origin: Field<&'a str>,
}

// Struct deserialization normally also accepts sequences. Archive traversal
// has always treated non-object array members as absent.
struct Object<T>(T);
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Object<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        struct Visitor<T>(std::marker::PhantomData<T>);
        impl<'de, T: Deserialize<'de>> serde::de::Visitor<'de> for Visitor<T> {
            type Value = Object<T>;
            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("an object")
            }
            fn visit_map<M: serde::de::MapAccess<'de>>(
                self,
                map: M,
            ) -> Result<Self::Value, M::Error> {
                T::deserialize(serde::de::value::MapAccessDeserializer::new(map)).map(Object)
            }
        }
        deserializer.deserialize_map(Visitor(std::marker::PhantomData))
    }
}

// Invalid array members are ignored exactly as in the former partial traversal.
#[derive(Default, Deserialize)]
#[serde(default)]
struct ChunkSnapshot {
    databases: Field<Vec<Field<Object<ChunkDatabase>>>>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct ChunkDatabase {
    stores: Field<Vec<Field<Object<ChunkStore>>>>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct ChunkStore {
    chunks: Field<Vec<Field<Object<ChunkReference>>>>,
}
#[derive(Default, Deserialize)]
#[serde(default)]
struct ChunkReference {
    sha256: Field<String>,
    bytes: Field<u64>,
    records: Field<u64>,
}

static ARCHIVE: OnceLock<Result<Option<(Archive, PathBuf)>, String>> = OnceLock::new();

fn read_archive(path: &Path) -> Result<Archive, String> {
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
    let field: Field<Object<Archive>> =
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    Ok(match field {
        Field::Valid(Object(archive)) => archive,
        Field::Invalid(_) | Field::Missing => Archive::default(),
    })
}

fn select(archive: &Archive, owner: &str) -> Result<Value, String> {
    let id = archive
        .id
        .get()
        .map(String::as_str)
        .filter(|v| !v.is_empty())
        .ok_or("The profile archive has no identity.")?;
    if !matches!(archive.version.get(), Some(1 | 2)) {
        return Err("Unsupported profile archive version.".into());
    }
    let owners = archive
        .owners
        .get()
        .ok_or("The profile archive needs an explicit destination owner mapping.")?;
    let origin = owners
        .get(owner)
        .and_then(Field::get)
        .map(String::as_str)
        .ok_or("The profile archive does not map this destination owner.")?;
    let snapshots = archive
        .snapshots
        .get()
        .ok_or("Invalid profile snapshots.")?;
    let matching: Vec<_> = snapshots
        .iter()
        .filter(|v| {
            v.is_object()
                && Origin::deserialize(*v)
                    .ok()
                    .and_then(|view| view.origin.get().copied())
                    == Some(origin)
        })
        .collect();
    if matching.len() != 1 {
        return Err("The profile archive must contain exactly one snapshot for this owner.".into());
    }
    Ok(json!({"id":id, "snapshot":matching[0]}))
}

fn archive() -> Result<Option<&'static (Archive, PathBuf)>, String> {
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

fn select_workspace(archive: &Archive) -> Result<Value, String> {
    let owners = archive.owners.get().ok_or("Missing profile owners.")?;
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
    Ok(json!({"id":archive.id.get(),"profiles":profiles}))
}

pub fn workspace() -> Result<Value, String> {
    match archive()? {
        Some((value, _)) => select_workspace(value),
        None => Ok(Value::Null),
    }
}

fn read_chunk(archive: &Archive, path: &Path, owner: &str, digest: &str) -> Result<Value, String> {
    use sha2::{Digest, Sha256};
    if archive.version.get() != Some(&2)
        || digest.len() != 64
        || !digest
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err("Invalid profile chunk.".into());
    }
    let selected = select(archive, owner)?;
    let snapshot = &selected["snapshot"];
    let view = if snapshot.is_object() {
        ChunkSnapshot::deserialize(snapshot).unwrap_or_default()
    } else {
        ChunkSnapshot::default()
    };
    let reference = view
        .databases
        .get()
        .into_iter()
        .flatten()
        .filter_map(Field::get)
        .map(|object| &object.0)
        .flat_map(|db| db.stores.get().into_iter().flatten())
        .filter_map(Field::get)
        .map(|object| &object.0)
        .flat_map(|store| store.chunks.get().into_iter().flatten())
        .filter_map(Field::get)
        .map(|object| &object.0)
        .find(|chunk| chunk.sha256.get().map(String::as_str) == Some(digest))
        .ok_or("This chunk does not belong to the profile owner.")?;
    let parts = archive
        .parts
        .get()
        .map(String::as_str)
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
        || metadata.len() > MAX_CHUNK_BYTES
        || reference.bytes.get().copied() != Some(metadata.len())
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
        .take(MAX_CHUNK_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| e.to_string())?;
    let actual: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if actual != digest {
        return Err("The profile chunk checksum changed.".into());
    }
    let records: Vec<serde::de::IgnoredAny> =
        serde_json::from_slice(&bytes).map_err(|e| e.to_string())?;
    if Some(records.len() as u64) != reference.records.get().copied() {
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
    fn archive(value: &Value) -> Archive {
        Archive::deserialize(value.clone()).unwrap_or_default()
    }
    fn select(value: &Value, owner: &str) -> Result<Value, String> {
        super::select(&archive(value), owner)
    }
    fn select_workspace(value: &Value) -> Result<Value, String> {
        super::select_workspace(&archive(value))
    }
    fn read_chunk(value: &Value, path: &Path, owner: &str, digest: &str) -> Result<Value, String> {
        super::read_chunk(&archive(value), path, owner, digest)
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
    fn selection_preserves_raw_snapshots_and_ignores_unselected_malformed_data() {
        let snapshot = json!({"origin":"local", "version":2, "unknown":{"value":[1, null]}, "databases":"invalid until renderer import"});
        for version in [1, 2] {
            let archive = json!({"id":"stable", "version":version, "owners":{"root":"local", "other":false},
                "snapshots":[null, 7, {"origin":false, "databases":[null]}, snapshot]});
            assert_eq!(select(&archive, "root").unwrap()["snapshot"], snapshot);
            assert_eq!(
                select(&archive, "other").unwrap_err(),
                "The profile archive does not map this destination owner."
            );
        }
        let invalid = json!({"id":"", "version":3, "owners":null, "snapshots":null});
        assert_eq!(
            select(&invalid, "root").unwrap_err(),
            "The profile archive has no identity."
        );
        let float_version =
            json!({"id":"id", "version":1.0, "owners":{"root":"x"}, "snapshots":[]});
        assert_eq!(
            select(&float_version, "root").unwrap_err(),
            "Unsupported profile archive version."
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
        for raw in ["null", "[]", "7"] {
            std::fs::write(&path, raw).unwrap();
            assert!(read_archive(&path).unwrap().id.get().is_none());
        }
        std::fs::write(&path, "{").unwrap();
        assert!(read_archive(&path).is_err());
        std::fs::write(&path, serde_json::to_vec(&archive).unwrap()).unwrap();
        let parsed = super::read_archive(&path).unwrap();
        assert_eq!(
            super::select(&parsed, "root").unwrap()["snapshot"],
            archive["snapshots"][0]
        );
        assert!(super::read_chunk(&parsed, &path, "root", &hash).is_ok());
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
        let malformed = json!({"version":2,"id":"test","parts":"parts", "owners":{"root":"local"},
          "snapshots":[{"origin":"local","databases":[[[[{"sha256":hash,"bytes":bytes.len(),"records":1}]]]]} ]});
        assert_eq!(
            read_chunk(&malformed, &path, "root", &hash).unwrap_err(),
            "This chunk does not belong to the profile owner."
        );
        let mut wrong_count = archive.clone();
        wrong_count["snapshots"][0]["databases"][0]["stores"][0]["chunks"][0]["records"] = json!(2);
        assert_eq!(
            read_chunk(&wrong_count, &path, "root", &hash).unwrap_err(),
            "The profile chunk record count changed."
        );
        let mut wrong_size = archive.clone();
        wrong_size["snapshots"][0]["databases"][0]["stores"][0]["chunks"][0]["bytes"] =
            json!(bytes.len() + 1);
        assert_eq!(
            read_chunk(&wrong_size, &path, "root", &hash).unwrap_err(),
            "The profile chunk size changed."
        );
        std::os::unix::fs::symlink(dir.join("parts"), dir.join("redirect")).unwrap();
        archive["parts"] = json!("redirect");
        assert_eq!(
            read_chunk(&archive, &path, "root", &hash).unwrap_err(),
            "Profile chunks cannot use a redirected directory."
        );
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
