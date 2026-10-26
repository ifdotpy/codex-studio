//! Durable file bytes and upload identities. Cancellation wins over late replies.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::{Request, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
pub const LIMIT: usize = studio_runtime_schema::shared_values::MAX_USER_ATTACHMENT_BYTES;
/// Maximum bytes in one legacy import chunk. The begin reply publishes this bound.
pub const BYTE_IMPORT_CHUNK_BYTES: usize = 256 * 1024;
/// Maximum retained bytes in one legacy import manifest.
const BYTE_IMPORT_TOTAL_BYTES: usize = 256 * 1024 * 1024;
pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch("CREATE TABLE IF NOT EXISTS client_file_sources(id TEXT PRIMARY KEY,name TEXT NOT NULL,mime TEXT NOT NULL,bytes BLOB NOT NULL); CREATE TABLE IF NOT EXISTS client_uploads(id TEXT PRIMARY KEY,workspace TEXT NOT NULL,state_dir TEXT NOT NULL,agent TEXT NOT NULL,document TEXT NOT NULL,bytes BLOB NOT NULL); CREATE TABLE IF NOT EXISTS client_upload_tombstones(id TEXT PRIMARY KEY,cancelled INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS client_byte_imports(id TEXT PRIMARY KEY,manifest TEXT NOT NULL,status TEXT NOT NULL,receipt TEXT);
        CREATE TABLE IF NOT EXISTS client_byte_import_chunks(manifest TEXT NOT NULL,entry INTEGER NOT NULL,offset INTEGER NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(manifest,entry,offset));
        CREATE TABLE IF NOT EXISTS client_byte_hashes(id TEXT PRIMARY KEY,size INTEGER NOT NULL,digest TEXT);
        CREATE TABLE IF NOT EXISTS client_download_saves(id TEXT PRIMARY KEY,content TEXT NOT NULL,receipt TEXT);
        CREATE TABLE IF NOT EXISTS client_byte_hash_chunks(id TEXT NOT NULL,offset INTEGER NOT NULL,bytes BLOB NOT NULL,PRIMARY KEY(id,offset));")?;
    crate::attachment_selections::initialize(db)
}

/// Metadata only. The immutable client identity owns the upload view.
fn snapshot_data(db: &Connection) -> Result<Value, String> {
    let identity: Option<String> = db
        .query_row(
            "SELECT value FROM client_identity WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    let identity = identity
        .map(|v| serde_json::from_str::<Value>(&v))
        .transpose()
        .map_err(|e| e.to_string())?
        .unwrap_or(Value::Null);
    let workspace = identity["workspaceId"].as_str().unwrap_or("");
    let mut q=db.prepare("SELECT document FROM client_uploads WHERE workspace=?1 ORDER BY json_extract(document,'$.created'),id").map_err(|e|e.to_string())?;
    let mut entries = vec![];
    for row in q
        .query_map([workspace], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?
    {
        entries.push(
            serde_json::from_str::<Value>(&row.map_err(|e| e.to_string())?)
                .map_err(|e| e.to_string())?,
        );
    }
    let mut tombstones = serde_json::Map::new();
    let mut q = db
        .prepare("SELECT id,cancelled FROM client_upload_tombstones ORDER BY id")
        .map_err(|e| e.to_string())?;
    for row in q
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, bool>(1)?)))
        .map_err(|e| e.to_string())?
    {
        let (id, cancelled) = row.map_err(|e| e.to_string())?;
        tombstones.insert(id, json!({"cancelled":cancelled,"settled":true}));
    }
    Ok(json!({"workspaceId":workspace,"entries":entries,"tombstones":tombstones}))
}
fn snapshot(db: &Connection) -> Result<Value, String> {
    let mut result = snapshot_data(db)?;
    result["revision"] = json!(crate::preferences::revision(db, "uploads")?);
    Ok(result)
}

fn apply_hash(db: &Connection, request: &Request) -> Result<Value, String> {
    let p = &request.params;
    let id = text(p, "id")?;
    if id.len() > 128 {
        return Err("Invalid source hash identity.".into());
    }
    if request.method == "files.hashBegin" {
        let size = p["bytes"]
            .as_u64()
            .filter(|n| *n <= BYTE_IMPORT_TOTAL_BYTES as u64)
            .ok_or("The source bytes exceed the import bound.")? as i64;
        let previous: Option<i64> = db
            .query_row(
                "SELECT size FROM client_byte_hashes WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(previous) = previous {
            if previous != size {
                return Err("The source hash identity has another size.".into());
            }
        } else {
            let(active,total):(i64,i64)=db.query_row("SELECT count(*),COALESCE(sum(size),0) FROM client_byte_hashes WHERE digest IS NULL",[],|r|Ok((r.get(0)?,r.get(1)?))).map_err(|e|e.to_string())?;
            if active >= 8 || total + size > BYTE_IMPORT_TOTAL_BYTES as i64 {
                return Err("Finish the pending source hashes first.".into());
            }
            db.execute(
                "INSERT INTO client_byte_hashes VALUES(?1,?2,NULL)",
                params![id, size],
            )
            .map_err(|e| e.to_string())?;
        }
        let digest: Option<String> = db
            .query_row(
                "SELECT digest FROM client_byte_hashes WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        return Ok(json!({"chunkBytes":BYTE_IMPORT_CHUNK_BYTES,"bytes":size,"sha256":digest}));
    }
    let (size, digest): (i64, Option<String>) = db
        .query_row(
            "SELECT size,digest FROM client_byte_hashes WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "Unknown source hash.")?;
    if request.method == "files.hashFinish" {
        if let Some(digest) = digest {
            return Ok(json!({"bytes":size,"sha256":digest}));
        }
        let mut query = db
            .prepare("SELECT offset,bytes FROM client_byte_hash_chunks WHERE id=?1 ORDER BY offset")
            .map_err(|e| e.to_string())?;
        let mut rows = query.query([id]).map_err(|e| e.to_string())?;
        let mut expected = 0i64;
        let mut hash = Sha256::new();
        while let Some(row) = rows.next().map_err(|e| e.to_string())? {
            let offset: i64 = row.get(0).map_err(|e| e.to_string())?;
            let bytes: Vec<u8> = row.get(1).map_err(|e| e.to_string())?;
            if offset != expected
                || bytes.is_empty()
                || bytes.len() > BYTE_IMPORT_CHUNK_BYTES
                || expected + bytes.len() as i64 > size
            {
                return Err("The source hash chunks changed.".into());
            }
            expected += bytes.len() as i64;
            hash.update(bytes);
        }
        if expected != size {
            return Err("The source hash bytes are incomplete.".into());
        }
        let digest = hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        db.execute(
            "UPDATE client_byte_hashes SET digest=?2 WHERE id=?1",
            params![id, digest],
        )
        .map_err(|e| e.to_string())?;
        db.execute("DELETE FROM client_byte_hash_chunks WHERE id=?1", [id])
            .map_err(|e| e.to_string())?;
        return Ok(json!({"bytes":size,"sha256":digest}));
    }
    if request.method != "files.hashChunk" || digest.is_some() {
        return Err("The source hash is already complete or its command is invalid.".into());
    }
    let offset = p["offset"]
        .as_i64()
        .filter(|n| *n >= 0 && *n <= size)
        .ok_or("Invalid source hash offset.")?;
    let encoded = p["base64"]
        .as_str()
        .filter(|s| s.len() <= BYTE_IMPORT_CHUNK_BYTES.div_ceil(3) * 4)
        .ok_or("The source hash chunk is too large.")?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid source bytes.")?;
    if bytes.is_empty()
        || bytes.len() > BYTE_IMPORT_CHUNK_BYTES
        || offset + bytes.len() as i64 > size
    {
        return Err("Invalid source hash chunk size.".into());
    }
    let previous: Option<Vec<u8>> = db
        .query_row(
            "SELECT bytes FROM client_byte_hash_chunks WHERE id=?1 AND offset=?2",
            params![id, offset],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    if let Some(previous) = previous {
        if previous != bytes {
            return Err("The source hash chunk has different bytes.".into());
        }
    } else {
        let next: i64 = db
            .query_row(
                "SELECT COALESCE(sum(length(bytes)),0) FROM client_byte_hash_chunks WHERE id=?1",
                [id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        if next != offset {
            return Err("Source hash chunks must be contiguous.".into());
        }
        db.execute(
            "INSERT INTO client_byte_hash_chunks VALUES(?1,?2,?3)",
            params![id, offset, bytes],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(json!({"nextOffset":offset+bytes.len() as i64}))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ByteManifest {
    version: u32,
    id: String,
    source: String,
    workspace: String,
    entries: Vec<ByteEntry>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ByteEntry {
    kind: String,
    id: String,
    bytes: usize,
    sha256: String,
    #[serde(default)]
    document: Value,
    cancelled: Option<bool>,
}
pub(crate) fn byte_hash(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
pub(crate) fn import_pending(db: &Connection) -> Result<bool, String> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM client_byte_imports WHERE status='pending')",
        [],
        |r| r.get(0),
    )
    .map_err(|e| e.to_string())
}
fn import_scope(db: &Connection, manifest: &ByteManifest) -> Result<(), String> {
    let workspace: Option<String> = db
        .query_row(
            "SELECT json_extract(value,'$.workspaceId') FROM client_identity WHERE singleton=1",
            [],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?
        .flatten();
    if !crate::projections::valid_workspace(&manifest.workspace)
        || workspace.as_deref() != Some(manifest.workspace.as_str())
    {
        return Err("The import belongs to another or an unverified workspace.".into());
    }
    Ok(())
}
fn import_manifest(raw: &Value) -> Result<ByteManifest, String> {
    let manifest: ByteManifest =
        serde_json::from_value(raw.clone()).map_err(|_| "Invalid byte import manifest.")?;
    if manifest.version != 1
        || manifest.id.is_empty()
        || manifest.id.len() > 128
        || manifest.source.is_empty()
        || manifest.source.len() > 4096
        || manifest.entries.len() > 64
        || raw.to_string().len() > 1024 * 1024
    {
        return Err("Invalid byte import manifest.".into());
    }
    let mut total = 0usize;
    let mut identities = std::collections::BTreeSet::new();
    for entry in &manifest.entries {
        total = total
            .checked_add(entry.bytes)
            .ok_or("The byte import is too large.")?;
        if total > BYTE_IMPORT_TOTAL_BYTES
            || entry.id.is_empty()
            || entry.id.len() > 128
            || entry.id.contains('\0')
            || !identities.insert((entry.kind.clone(), entry.id.clone()))
            || entry.sha256.len() != 64
            || !entry
                .sha256
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        {
            return Err("Invalid byte import entry.".into());
        }
        match entry.kind.as_str() {
            "upload" => {
                let row = &entry.document;
                if row["id"] != entry.id
                    || row["workspace"] != manifest.workspace
                    || row["size"].as_u64() != Some(entry.bytes as u64)
                    || entry.bytes == 0
                    || entry.bytes > LIMIT
                    || entry.cancelled.is_some()
                    || ["agent", "stateDir", "name"]
                        .iter()
                        .any(|key| text(row, key).is_err())
                    || row
                        .get("asset")
                        .is_some_and(|asset| !asset.is_null() && asset["id"] != entry.id)
                {
                    return Err("Invalid saved upload identity or bytes.".into());
                }
            }
            "recording" => {
                if entry.document["id"] != entry.id || entry.cancelled.is_some() {
                    return Err("Invalid saved recording identity.".into());
                }
                crate::dictation::validate_import(&entry.document, entry.bytes)?;
            }
            "uploadTombstone"
                if entry.bytes == 0
                    && entry.sha256 == byte_hash(&[])
                    && entry.document.is_null()
                    && entry.cancelled.is_some() => {}
            _ => return Err("Invalid byte import entry kind.".into()),
        }
    }
    if manifest.entries.iter().any(|entry| {
        entry.kind == "upload"
            && manifest
                .entries
                .iter()
                .any(|other| other.kind == "uploadTombstone" && other.id == entry.id)
    }) {
        return Err("The import contains both a pending and a settled upload.".into());
    }
    Ok(manifest)
}
fn saved_import(db: &Connection, id: &str) -> Result<(Value, ByteManifest, String), String> {
    let (raw, status): (String, String) = db
        .query_row(
            "SELECT manifest,status FROM client_byte_imports WHERE id=?1",
            [id],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .map_err(|_| "Unknown byte import.")?;
    let raw: Value = serde_json::from_str(&raw).map_err(|e| e.to_string())?;
    let manifest = import_manifest(&raw)?;
    import_scope(db, &manifest)?;
    Ok((raw, manifest, status))
}
fn import_receipt(raw: &Value, manifest: &ByteManifest) -> Value {
    json!({"manifestId":manifest.id,"manifestHash":byte_hash(raw.to_string().as_bytes()),"source":manifest.source,"workspace":manifest.workspace,
        "entries":manifest.entries.iter().map(|entry|json!({"id":entry.id,"kind":entry.kind,"bytes":entry.bytes,"sha256":entry.sha256,
            "documentHash":byte_hash(entry.document.to_string().as_bytes()),"cancelled":entry.cancelled})).collect::<Vec<_>>()})
}
fn imported_bytes(
    db: &Connection,
    manifest: &str,
    index: usize,
    expected: usize,
) -> Result<Vec<u8>, String> {
    let mut query=db.prepare("SELECT offset,bytes FROM client_byte_import_chunks WHERE manifest=?1 AND entry=?2 ORDER BY offset").map_err(|e|e.to_string())?;
    let mut rows = query
        .query(params![manifest, index as i64])
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::with_capacity(expected);
    while let Some(row) = rows.next().map_err(|e| e.to_string())? {
        let offset = usize::try_from(row.get::<_, i64>(0).map_err(|e| e.to_string())?)
            .map_err(|_| "Invalid import chunk offset.")?;
        let chunk: Vec<u8> = row.get(1).map_err(|e| e.to_string())?;
        if offset != bytes.len()
            || chunk.is_empty()
            || chunk.len() > BYTE_IMPORT_CHUNK_BYTES
            || bytes.len() + chunk.len() > expected
        {
            return Err("The saved import chunks are incomplete or changed.".into());
        }
        bytes.extend_from_slice(&chunk);
    }
    if bytes.len() != expected {
        return Err("The saved import bytes are incomplete.".into());
    }
    Ok(bytes)
}
fn verify_import(db: &Connection, raw: &Value, manifest: &ByteManifest) -> Result<Value, String> {
    for entry in &manifest.entries {
        let (document, bytes) = match entry.kind.as_str() {
            "upload" => {
                let document = row(db, &entry.id)?;
                let bytes: Vec<u8> = db
                    .query_row(
                        "SELECT bytes FROM client_uploads WHERE id=?1",
                        [&entry.id],
                        |r| r.get(0),
                    )
                    .map_err(|_| "The imported upload is missing.")?;
                if status(db, &entry.id)?["settled"] == true {
                    return Err("The imported upload changed.".into());
                }
                (document, bytes)
            }
            "recording" => crate::dictation::read_import(db, &entry.id)?,
            "uploadTombstone" => {
                let saved: Option<bool> = db
                    .query_row(
                        "SELECT cancelled FROM client_upload_tombstones WHERE id=?1",
                        [&entry.id],
                        |r| r.get(0),
                    )
                    .optional()
                    .map_err(|e| e.to_string())?;
                if saved != entry.cancelled || !row(db, &entry.id)?.is_null() {
                    return Err("The imported upload settlement changed.".into());
                }
                (Value::Null, Vec::new())
            }
            _ => return Err("Invalid byte import entry.".into()),
        };
        if document != entry.document
            || bytes.len() != entry.bytes
            || byte_hash(&bytes) != entry.sha256
        {
            return Err("The imported document or bytes changed.".into());
        }
    }
    Ok(import_receipt(raw, manifest))
}

/// The cutover owner proves each exact committed byte receipt before admission.
pub(crate) fn verify_import_receipt(
    db: &Connection,
    manifest_id: &str,
    receipt: &Value,
) -> Result<(), String> {
    let (raw, manifest, phase) = saved_import(db, manifest_id)?;
    import_scope(db, &manifest)?;
    if phase != "complete" {
        return Err("The byte import is not committed.".into());
    }
    if verify_import(db, &raw, &manifest)? != *receipt {
        return Err("The byte import receipt changed.".into());
    }
    Ok(())
}
fn apply_import(db: &Connection, request: &Request) -> Result<Value, String> {
    let p = &request.params;
    if request.method == "files.importPending" {
        return Ok(json!(import_pending(db)?));
    }
    if request.method == "files.importBegin" {
        let raw = &p["manifest"];
        let manifest = import_manifest(raw)?;
        import_scope(db, &manifest)?;
        let saved: Option<String> = db
            .query_row(
                "SELECT manifest FROM client_byte_imports WHERE id=?1",
                [&manifest.id],
                |r| r.get(0),
            )
            .optional()
            .map_err(|e| e.to_string())?;
        if let Some(saved) = saved {
            if serde_json::from_str::<Value>(&saved).map_err(|e| e.to_string())? != *raw {
                return Err("The byte import identity has different content.".into());
            }
        } else {
            let pending: i64 = db
                .query_row(
                    "SELECT count(*) FROM client_byte_imports WHERE status='pending'",
                    [],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            if pending >= 8 {
                return Err("Finish the pending byte imports first.".into());
            }
            db.execute(
                "INSERT INTO client_byte_imports VALUES(?1,?2,'pending',NULL)",
                params![manifest.id, raw.to_string()],
            )
            .map_err(|e| e.to_string())?;
        }
        let status: String = db
            .query_row(
                "SELECT status FROM client_byte_imports WHERE id=?1",
                [&manifest.id],
                |r| r.get(0),
            )
            .map_err(|e| e.to_string())?;
        return Ok(
            json!({"manifestId":manifest.id,"manifestHash":byte_hash(raw.to_string().as_bytes()),"chunkBytes":BYTE_IMPORT_CHUNK_BYTES,"status":status}),
        );
    }
    let id = text(p, "manifestId")?;
    let (raw, manifest, phase) = saved_import(db, id)?;
    if request.method == "files.importReadback" {
        if phase != "complete" {
            return Err("The byte import is not committed.".into());
        }
        return verify_import(db, &raw, &manifest);
    }
    if phase == "complete" {
        if request.method == "files.importCommit" {
            return Ok(import_receipt(&raw, &manifest));
        }
        return Err("The byte import is already committed.".into());
    }
    if request.method == "files.importChunk" {
        let index = p["entry"]
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .ok_or("Invalid import entry.")?;
        let entry = manifest.entries.get(index).ok_or("Unknown import entry.")?;
        let offset = p["offset"]
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .ok_or("Invalid import offset.")?;
        let encoded = p["base64"]
            .as_str()
            .filter(|s| s.len() <= BYTE_IMPORT_CHUNK_BYTES.div_ceil(3) * 4)
            .ok_or("The import chunk is too large.")?;
        let bytes = STANDARD
            .decode(encoded)
            .map_err(|_| "Invalid import bytes.")?;
        if bytes.is_empty()
            || bytes.len() > BYTE_IMPORT_CHUNK_BYTES
            || offset
                .checked_add(bytes.len())
                .is_none_or(|end| end > entry.bytes)
        {
            return Err("Invalid import chunk size.".into());
        }
        let prior:Option<Vec<u8>>=db.query_row("SELECT bytes FROM client_byte_import_chunks WHERE manifest=?1 AND entry=?2 AND offset=?3",params![id,index as i64,offset as i64],|r|r.get(0)).optional().map_err(|e|e.to_string())?;
        if let Some(prior) = prior {
            if prior != bytes {
                return Err("The saved import chunk has different bytes.".into());
            }
        } else {
            let next:i64=db.query_row("SELECT COALESCE(sum(length(bytes)),0) FROM client_byte_import_chunks WHERE manifest=?1 AND entry=?2",params![id,index as i64],|r|r.get(0)).map_err(|e|e.to_string())?;
            if next != offset as i64 {
                return Err("Import chunks must be contiguous.".into());
            }
            db.execute(
                "INSERT INTO client_byte_import_chunks VALUES(?1,?2,?3,?4)",
                params![id, index as i64, offset as i64, bytes],
            )
            .map_err(|e| e.to_string())?;
        }
        return Ok(json!({"nextOffset":offset+bytes.len()}));
    }
    if request.method != "files.importCommit" {
        return Err("Unknown byte import command.".into());
    }
    for (index, entry) in manifest.entries.iter().enumerate() {
        let bytes = imported_bytes(db, id, index, entry.bytes)?;
        if byte_hash(&bytes) != entry.sha256 {
            return Err("The imported bytes do not match their SHA-256.".into());
        }
        match entry.kind.as_str() {
            "upload" => {
                if !row(db, &entry.id)?.is_null() || status(db, &entry.id)?["settled"] == true {
                    return Err("The destination already owns this upload ID.".into());
                }
                let d = &entry.document;
                db.execute(
                    "INSERT INTO client_uploads VALUES(?1,?2,?3,?4,?5,?6)",
                    params![
                        entry.id,
                        manifest.workspace,
                        text(d, "stateDir")?,
                        text(d, "agent")?,
                        d.to_string(),
                        bytes
                    ],
                )
                .map_err(|e| e.to_string())?;
            }
            "recording" => crate::dictation::import_recording(db, &entry.document, &bytes)?,
            "uploadTombstone" => {
                if !row(db, &entry.id)?.is_null() || status(db, &entry.id)?["settled"] == true {
                    return Err("The destination already owns this upload ID.".into());
                }
                db.execute(
                    "INSERT INTO client_upload_tombstones VALUES(?1,?2)",
                    params![entry.id, entry.cancelled],
                )
                .map_err(|e| e.to_string())?;
            }
            _ => return Err("Invalid byte import kind.".into()),
        }
    }
    let receipt = verify_import(db, &raw, &manifest)?;
    db.execute(
        "UPDATE client_byte_imports SET status='complete',receipt=?2 WHERE id=?1",
        params![id, receipt.to_string()],
    )
    .map_err(|e| e.to_string())?;
    db.execute(
        "DELETE FROM client_byte_import_chunks WHERE manifest=?1",
        [id],
    )
    .map_err(|e| e.to_string())?;
    Ok(receipt)
}
fn text<'a>(p: &'a Value, key: &str) -> Result<&'a str, String> {
    p[key]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= 4096)
        .ok_or_else(|| format!("{key} is required."))
}
fn uuid() -> Result<String, String> {
    crate::platform::dispatch(&Request {
        id: "file-identity".into(),
        method: "system.uuid".into(),
        params: Value::Null,
    })
    .result
    .and_then(|v| v.as_str().map(str::to_owned))
    .ok_or("Cannot create a file identity.".into())
}
fn row(db: &Connection, id: &str) -> Result<Value, String> {
    let raw: Option<String> = db
        .query_row(
            "SELECT document FROM client_uploads WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    raw.map(|s| serde_json::from_str(&s).map_err(|e| e.to_string()))
        .transpose()
        .map(|v| v.unwrap_or(Value::Null))
}
fn status(db: &Connection, id: &str) -> Result<Value, String> {
    let cancelled: Option<bool> = db
        .query_row(
            "SELECT cancelled FROM client_upload_tombstones WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    Ok(json!({"cancelled":cancelled==Some(true),"settled":cancelled.is_some()}))
}
fn apply(db: &Connection, request: &Request) -> Result<Value, String> {
    let p = &request.params;
    if request.method.starts_with("files.hash") {
        return apply_hash(db, request);
    }
    if request.method.starts_with("files.import") {
        return apply_import(db, request);
    }
    if request.method == "uploads.queue" && import_pending(db)? {
        return Err("Finish the byte import before queuing uploads.".into());
    }
    match request.method.as_str() {
        "uploads.snapshot" => snapshot(db),
        "files.saveClaim" => {
            let previous: Option<(String, Option<String>)> = db
                .query_row(
                    "SELECT content,receipt FROM client_download_saves WHERE id=?1",
                    [&request.id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .optional()
                .map_err(|e| e.to_string())?;
            if let Some((content, receipt)) = previous {
                if serde_json::from_str::<Value>(&content).map_err(|e| e.to_string())? != *p {
                    return Err("The Save request identity has different content.".into());
                }
                return Ok(
                    json!({"fresh":false,"receipt":receipt.map(|s|serde_json::from_str::<Value>(&s)).transpose().map_err(|e|e.to_string())?}),
                );
            }
            db.execute(
                "INSERT INTO client_download_saves VALUES(?1,?2,NULL)",
                params![request.id, p.to_string()],
            )
            .map_err(|e| e.to_string())?;
            Ok(json!({"fresh":true}))
        }
        "files.saveSettle" => {
            let id = text(p, "requestId")?;
            let receipt = &p["receipt"];
            if !receipt.is_object() || receipt["saved"].as_bool().is_none() {
                return Err("Invalid native Save receipt.".into());
            }
            let previous: Option<String> = db
                .query_row(
                    "SELECT receipt FROM client_download_saves WHERE id=?1",
                    [id],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            if let Some(previous) = previous {
                if serde_json::from_str::<Value>(&previous).map_err(|e| e.to_string())? != *receipt
                {
                    return Err("The native Save receipt changed.".into());
                }
            } else {
                db.execute(
                    "UPDATE client_download_saves SET receipt=?2 WHERE id=?1",
                    params![id, receipt.to_string()],
                )
                .map_err(|e| e.to_string())?;
                if let Some(handle) = receipt["nativeHandle"].as_str() {
                    db.execute("DELETE FROM client_file_sources WHERE id=?1", [handle])
                        .map_err(|e| e.to_string())?;
                }
            }
            Ok(receipt.clone())
        }
        "files.exportForSave" => {
            let handle = text(p, "nativeHandle")?;
            let (name, mime, bytes): (String, String, Vec<u8>) = db
                .query_row(
                    "SELECT name,mime,bytes FROM client_file_sources WHERE id=?1",
                    [handle],
                    |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
                )
                .map_err(|e| e.to_string())?;
            Ok(json!({"name":name,"mime":mime,"base64":STANDARD.encode(bytes)}))
        }
        "files.stage_many" => {
            let files = p["files"]
                .as_array()
                .filter(|v| v.len() <= 20)
                .ok_or("Select at most 20 files.")?;
            let mut result = vec![];
            for file in files {
                result.push(apply(
                    db,
                    &Request {
                        id: request.id.clone(),
                        method: "files.stage".into(),
                        params: file.clone(),
                    },
                )?);
            }
            Ok(json!(result))
        }
        "files.stage" | "files.text" | "files.stageDownload" => {
            if request.method == crate::attachment_selections::contract()["textFile"]["stageMethod"]
            {
                crate::attachment_selections::validate_text_stage(db, request)?;
            }
            let name = text(p, "name")?;
            if name.contains(['\0', '/', '\\']) {
                return Err("The file name must not contain a path.".into());
            }
            let bytes = if request.method == "files.text" {
                p["text"]
                    .as_str()
                    .ok_or("File text is required.")?
                    .as_bytes()
                    .to_vec()
            } else {
                let value = p["base64"]
                    .as_str()
                    .filter(|v| v.len() <= LIMIT.div_ceil(3) * 4)
                    .ok_or("Invalid file data.")?;
                STANDARD.decode(value).map_err(|e| e.to_string())?
            };
            if (bytes.is_empty() && request.method != "files.stageDownload") || bytes.len() > LIMIT
            {
                return Err("Files must contain 1 byte to 20 MiB.".into());
            }
            let (count, total): (i64, i64) = db
                .query_row(
                    "SELECT count(*),COALESCE(sum(length(bytes)),0) FROM client_file_sources",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| e.to_string())?;
            if count >= 100 || total + bytes.len() as i64 > 256 * 1024 * 1024 {
                return Err("Save or remove the selected files before adding more files.".into());
            }
            let id = uuid()?;
            let mime = p["mime"]
                .as_str()
                .filter(|v| v.len() <= 255)
                .unwrap_or("application/octet-stream");
            db.execute(
                "INSERT INTO client_file_sources VALUES(?1,?2,?3,?4)",
                params![id, name, mime, bytes],
            )
            .map_err(|e| e.to_string())?;
            Ok(json!({"nativeHandle":id,"name":name,"type":mime,"size":bytes.len()}))
        }
        "uploads.queue" => {
            crate::attachment_selections::validate_enqueue(db, request)?;
            let workspace = text(p, "workspace")?;
            if !crate::projections::valid_workspace(workspace) {
                return Err("Invalid workspace identity.".into());
            }
            let state_dir = text(p, "stateDir")?;
            let agent = text(p, "agent")?;
            let files = p["files"]
                .as_array()
                .filter(|v| !v.is_empty() && v.len() <= 8)
                .ok_or("Attach one to eight files.")?;
            let mut result = vec![];
            for (index, file) in files.iter().enumerate() {
                let source = text(file, "nativeHandle")?;
                let (name,mime,bytes):(String,String,Vec<u8>)=db.query_row("SELECT name,mime,bytes FROM client_file_sources WHERE id=?1",[source],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).map_err(|_|"The selected file is unavailable. Check saved uploads before selecting it again.")?;
                let id = uuid()?;
                let created = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_err(|e| e.to_string())?
                    .as_millis() as u64
                    + index as u64;
                let document = json!({"id":id,"workspace":workspace,"stateDir":state_dir,"agent":agent,"name":name,"mime":mime,"size":bytes.len(),"created":created});
                db.execute(
                    "INSERT INTO client_uploads VALUES(?1,?2,?3,?4,?5,?6)",
                    params![id, workspace, state_dir, agent, document.to_string(), bytes],
                )
                .map_err(|e| e.to_string())?;
                db.execute("DELETE FROM client_file_sources WHERE id=?1", [source])
                    .map_err(|e| e.to_string())?;
                result.push(document);
            }
            Ok(json!(result))
        }
        "uploads.list" => {
            let mut query=db.prepare("SELECT document FROM client_uploads WHERE workspace=?1 AND state_dir=?2 ORDER BY json_extract(document,'$.created'),id").map_err(|e|e.to_string())?;
            let rows = query
                .query_map(params![text(p, "workspace")?, text(p, "stateDir")?], |r| {
                    r.get::<_, String>(0)
                })
                .map_err(|e| e.to_string())?;
            let mut result = vec![];
            for row in rows {
                result.push(
                    serde_json::from_str::<Value>(&row.map_err(|e| e.to_string())?)
                        .map_err(|e| e.to_string())?,
                );
            }
            Ok(json!(result))
        }
        "uploads.status" => status(db, text(p, "id")?),
        "uploads.get" => row(db, text(p, "id")?),
        "uploads.content" => {
            let id = text(p, "id")?;
            let bytes: Option<Vec<u8>> = db
                .query_row("SELECT bytes FROM client_uploads WHERE id=?1", [id], |r| {
                    r.get(0)
                })
                .optional()
                .map_err(|e| e.to_string())?;
            Ok(bytes
                .map(|v| json!(STANDARD.encode(v)))
                .unwrap_or(Value::Null))
        }
        "uploads.cancel" | "uploads.finish" => {
            let id = text(p, "id")?;
            let cancelled = request.method == "uploads.cancel";
            db.execute("INSERT INTO client_upload_tombstones VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET cancelled=max(cancelled,excluded.cancelled)",params![id,cancelled]).map_err(|e|e.to_string())?;
            db.execute("DELETE FROM client_uploads WHERE id=?1", [id])
                .map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "uploads.receipt" => {
            let id = text(p, "id")?;
            if p["asset"]["id"] != id {
                return Err("The upload response does not match the saved file.".into());
            }
            let mut current = row(db, id)?;
            if current.is_null() || status(db, id)?["settled"] == true {
                return Ok(Value::Null);
            }
            current["asset"] = p["asset"].clone();
            db.execute(
                "UPDATE client_uploads SET document=?2 WHERE id=?1",
                params![id, current.to_string()],
            )
            .map_err(|e| e.to_string())?;
            Ok(current["asset"].clone())
        }
        _ => Err("Unknown upload command.".into()),
    }
}
/// Stage a native capture in its caller's existing transaction. The file owner
/// retains byte validation, quota checks, and opaque handle allocation.
pub(crate) fn stage_selection_files(db: &Connection, files: &[Value]) -> Result<Value, String> {
    apply(
        db,
        &Request {
            id: "native-attachment-capture".into(),
            method: "files.stage_many".into(),
            params: json!({"files":files}),
        },
    )
}
pub fn stored(db: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Response, String> {
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let durable = matches!(
            request.method.as_str(),
            "files.stage_many"
                | "files.stage"
                | "files.text"
                | "files.stageDownload"
                | "uploads.queue"
        );
        if durable
            && let Some(reply) = crate::store::replay(&tx, request).map_err(|e| e.to_string())?
        {
            return Ok(reply);
        }
        let tracked = studio_client_domain::preferences::contract()["uploadMutationMethods"]
            .as_array()
            .expect("upload mutation owner")
            .iter()
            .any(|v| v.as_str() == Some(&request.method));
        let before = if tracked {
            Some(snapshot_data(&tx)?)
        } else {
            None
        };
        let value = apply(&tx, request)?;
        if let Some(before) = before
            && before != snapshot_data(&tx)?
        {
            crate::preferences::bump(&tx, "uploads")?;
        }
        let response = Response::success(request.id.clone(), value);
        if durable {
            crate::store::record(&tx, request, &response).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(response)
    })();
    result.unwrap_or_else(|e| Response::failure(request.id.clone(), "upload_state", e))
}
fn upload_lock(id: &str) -> Result<std::sync::Arc<tokio::sync::Mutex<()>>, String> {
    use std::sync::{Arc, Mutex, OnceLock, Weak};
    static LOCKS: OnceLock<Mutex<std::collections::HashMap<String, Weak<tokio::sync::Mutex<()>>>>> =
        OnceLock::new();
    let mut locks = LOCKS
        .get_or_init(Mutex::default)
        .lock()
        .map_err(|e| e.to_string())?;
    locks.retain(|_, value| value.strong_count() > 0);
    if let Some(lock) = locks.get(id).and_then(Weak::upgrade) {
        return Ok(lock);
    }
    if locks.len() >= 64 {
        return Err("The upload queue is busy. Retry after an upload completes.".into());
    }
    let lock = Arc::new(tokio::sync::Mutex::new(()));
    locks.insert(id.to_owned(), Arc::downgrade(&lock));
    Ok(lock)
}

/// A privileged desktop action stages bytes in its exact context Store.
pub(crate) async fn download(
    backend: &Backend,
    sender: &StoreSender,
    request: &Request,
) -> Result<Value, String> {
    backend.validate_identity()?;
    let path = text(&request.params, "path")?;
    let response=backend.dispatch(Request{id:format!("{}:http",request.id),method:"api.request".into(),params:json!({"path":path,"query":request.params["query"],"method":"GET","download":true,"timeoutMs":60000})}).await;
    let data = response.result.ok_or_else(|| {
        response
            .error
            .map(|e| e.message)
            .unwrap_or("The download was not confirmed.".into())
    })?;
    let name = text(&data, "name")?;
    if name.len() > 1024
        || name.chars().any(char::is_control)
        || name.contains(['/', '\\'])
        || matches!(name, "." | "..")
    {
        return Err("The download name is invalid.".into());
    }
    let mut handle = stage_download(
        sender,
        &request.id,
        json!({"name":name,"mime":data["mime"],"base64":data["base64"]}),
    )
    .await?;
    handle["truncated"] = json!(data["truncated"] == true);
    Ok(handle)
}

pub(crate) async fn stage_download(
    sender: &StoreSender,
    id: &str,
    params: Value,
) -> Result<Value, String> {
    let (reply, receive) = tokio::sync::oneshot::channel();
    sender
        .send((
            Request {
                id: format!("{id}:stage"),
                method: "files.stageDownload".into(),
                params,
            },
            reply,
        ))
        .await
        .map_err(|e| e.to_string())?;
    let response = receive.await.map_err(|e| e.to_string())?;
    response.result.ok_or_else(|| {
        response
            .error
            .map(|e| e.message)
            .unwrap_or("The download bytes were not saved.".into())
    })
}
pub async fn dispatch(backend: &Backend, sender: &StoreSender, request: Request) -> Response {
    let result=async {
        if state(sender,"files.importPending",Value::Null).await?==true{return Err("Finish the byte import before uploading files.".into());}
        let p=&request.params;
        if request.method=="uploads.pending"||request.method=="uploads.enqueue"{
            if request.method=="uploads.enqueue" {text(p,"workspace")?;}
            let identity=crate::sync::open(backend,sender).await.map_err(|e|e.message)?;
            let workspace=identity["workspaceId"].as_str().ok_or("Workspace identity is unavailable.")?;
            if p["workspace"].as_str().is_some_and(|expected| !expected.is_empty()&&expected!=workspace){return Err("The workspace changed. Saved files remain on this device.".into());}
            let mut params=p.clone();params["workspace"]=json!(workspace);
            if request.method=="uploads.pending"{return state(sender,"uploads.list",params).await;}
            let (reply,receive)=tokio::sync::oneshot::channel();
            sender.send((Request{id:request.id.clone(),method:"uploads.queue".into(),params},reply)).await.map_err(|e|e.to_string())?;
            let response=receive.await.map_err(|e|e.to_string())?;
            return response.result.ok_or_else(||response.error.map(|e|e.message).unwrap_or("Missing file state response.".into()));
        }
        let expected=&p["row"];let id=text(expected,"id")?;
        let lock=upload_lock(id)?;let _guard=lock.lock().await;
        let current=state(sender,"uploads.get",json!({"id":id})).await?;
        if current.is_null(){return Ok(Value::Null);}
        for key in ["workspace","agent","stateDir"] {if current[key]!=expected[key]{return Err("The saved upload belongs to another workspace or chat.".into());}}
        let workspace=text(&current,"workspace")?;
        let identity=crate::sync::identity(backend).await.map_err(|e|e.message)?;
        if identity["workspaceId"]!=workspace{return Err("The server workspace changed. Saved files remain on this device.".into());}
        if current["asset"].is_object(){return Ok(state(sender,"uploads.get",json!({"id":id})).await?["asset"].clone());}
        let bytes=state(sender,"uploads.content",json!({"id":id})).await?;
        if bytes.is_null(){return Ok(Value::Null);}
        let response=backend.dispatch(Request{id:request.id.clone(),method:"api.request".into(),params:json!({"path":"/api/assets","workspaceId":workspace,"timeoutMs":60000,"body":{"id":id,"agent":current["agent"],"name":current["name"],"base64":bytes}})}).await;
        let value=response.result.ok_or_else(||response.error.map(|e|e.message).unwrap_or("Missing upload response.".into()))?;
        state(sender,"uploads.receipt",json!({"id":id,"asset":value.get("asset").unwrap_or(&value)})).await
    }.await;
    match result {
        Ok(value) => Response::success(request.id, value),
        Err(error) => Response::failure(request.id, "upload", error),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    fn command(id: &str, method: &str, params: Value) -> Request {
        Request {
            id: id.into(),
            method: method.into(),
            params,
        }
    }

    #[test]
    fn native_download_save_claim_refuses_lost_reply_reentry_and_changed_content() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite");
        let request = || {
            command(
                "original-save",
                "files.saveClaim",
                json!({"path":"/api/monitor/log","query":{"id":"original-monitor"}}),
            )
        };
        let mut store = Store::open(&path).unwrap();
        let first = store.dispatch(request());
        assert!(first.error.is_none(), "{:?}", first.error);
        assert_eq!(first.result.unwrap()["fresh"], true);
        drop(store);
        let mut store = Store::open(&path).unwrap();
        let replay = store.dispatch(request()).result.unwrap();
        assert_eq!(replay["fresh"], false);
        assert!(replay["receipt"].is_null());
        let mut changed = request();
        changed.params["query"]["id"] = json!("other-monitor");
        assert!(store.dispatch(changed).error.is_some());
    }

    fn import_manifest() -> Value {
        use sha2::{Digest, Sha256};
        let hash = |bytes: &[u8]| {
            Sha256::digest(bytes)
                .iter()
                .map(|byte| format!("{byte:02x}"))
                .collect::<String>()
        };
        let bytes = b"original";
        json!({"version":1,"id":"legacy-bytes","source":"browser:server-a:v2","workspace":"a".repeat(32),
            "entries":[{"kind":"upload","id":"old-upload","bytes":bytes.len(),
                "sha256":hash(bytes),"document":{
                "id":"old-upload","workspace":"a".repeat(32),"stateDir":"/original-state",
                "agent":"old-chat","name":"old.txt","mime":"text/plain","size":bytes.len(),
                "created":123,"requestId":"original-request","asset":{"id":"old-upload","name":"old.txt"}}},
                {"kind":"recording","id":"old-recording","bytes":4,
                 "sha256":hash(&[1,0,255,127]),"document":{
                 "id":"old-recording","chatId":"old-chat","created":124,"sampleRate":16000,
                 "samples":2,"state":"ready","transcriptionAttempt":"original-attempt","deletedAt":125}},
                {"kind":"uploadTombstone","id":"cancelled-upload","bytes":0,
                 "sha256":hash(&[]),"cancelled":true}]
        })
    }

    fn import_fixture(path: &std::path::Path) -> Store {
        let store = Store::open(path).unwrap();
        store
            .connection
            .execute(
                "INSERT INTO client_identity VALUES(1,?1)",
                [json!({"workspaceId":"a".repeat(32)}).to_string()],
            )
            .unwrap();
        store
    }

    #[test]
    fn byte_begin_replay_exposes_completed_hash_and_import_without_new_chunks() {
        let directory = tempfile::tempdir().unwrap();
        let mut store = import_fixture(&directory.path().join("client.sqlite"));
        let hash = || {
            command(
                "begin",
                "files.hashBegin",
                json!({"id":"empty-hash","bytes":0}),
            )
        };
        assert!(store.dispatch(hash()).error.is_none());
        let digest = store
            .dispatch(command(
                "finish",
                "files.hashFinish",
                json!({"id":"empty-hash"}),
            ))
            .result
            .unwrap();
        assert_eq!(
            store.dispatch(hash()).result.unwrap()["sha256"],
            digest["sha256"]
        );
        let manifest = json!({"version":1,"id":"empty-manifest","source":"sealed-profile","workspace":"a".repeat(32),"entries":[]});
        let begin = || command("begin", "files.importBegin", json!({"manifest":manifest}));
        assert_eq!(store.dispatch(begin()).result.unwrap()["status"], "pending");
        store
            .dispatch(command(
                "commit",
                "files.importCommit",
                json!({"manifestId":"empty-manifest"}),
            ))
            .result
            .unwrap();
        assert_eq!(
            store.dispatch(begin()).result.unwrap()["status"],
            "complete"
        );
    }

    #[test]
    fn byte_import_preserves_exact_ids_receipts_pcm_and_cancellation_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("client.sqlite3");
        let manifest = import_manifest();
        {
            let mut store = import_fixture(&path);
            let begin = store.dispatch(command(
                "begin",
                "files.importBegin",
                json!({"manifest":manifest}),
            ));
            assert!(begin.error.is_none(), "{begin:?}");
            for (entry, bytes) in [
                (0, b"original".as_slice()),
                (1, [1, 0, 255, 127].as_slice()),
            ] {
                let params = json!({"manifestId":"legacy-bytes","entry":entry,"offset":0,"base64":STANDARD.encode(bytes)});
                assert!(
                    store
                        .dispatch(command("chunk", "files.importChunk", params.clone()))
                        .error
                        .is_none()
                );
                assert!(
                    store
                        .dispatch(command("retry", "files.importChunk", params))
                        .error
                        .is_none()
                );
            }
        }
        let mut store = Store::open(&path).unwrap();
        let commit = store.dispatch(command(
            "commit",
            "files.importCommit",
            json!({"manifestId":"legacy-bytes"}),
        ));
        assert!(commit.error.is_none(), "{commit:?}");
        assert_eq!(
            store
                .dispatch(command(
                    "readback",
                    "files.importReadback",
                    json!({"manifestId":"legacy-bytes"})
                ))
                .result,
            commit.result
        );
        assert_eq!(
            store
                .dispatch(command(
                    "commit-again",
                    "files.importCommit",
                    json!({"manifestId":"legacy-bytes"})
                ))
                .result,
            commit.result
        );
        assert_eq!(
            row(&store.connection, "old-upload").unwrap(),
            manifest["entries"][0]["document"]
        );
        assert_eq!(
            status(&store.connection, "cancelled-upload").unwrap(),
            json!({"cancelled":true,"settled":true})
        );
        let pcm: Vec<u8> = store
            .connection
            .query_row(
                "SELECT pcm FROM client_dictation_chunks WHERE id='old-recording'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pcm, [1, 0, 255, 127]);
        let saved: String = store
            .connection
            .query_row(
                "SELECT document FROM client_dictation_recordings WHERE id='old-recording'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            serde_json::from_str::<Value>(&saved).unwrap(),
            manifest["entries"][1]["document"]
        );
    }

    #[test]
    fn byte_import_refuses_changed_scope_chunks_and_partial_commit_without_destination_writes() {
        let root = tempfile::tempdir().unwrap();
        let mut store = import_fixture(&root.path().join("client.sqlite3"));
        let manifest = import_manifest();
        assert!(
            store
                .dispatch(command(
                    "begin",
                    "files.importBegin",
                    json!({"manifest":manifest})
                ))
                .error
                .is_none()
        );
        let mut changed = manifest.clone();
        changed["source"] = json!("another-source");
        assert!(
            store
                .dispatch(command(
                    "changed",
                    "files.importBegin",
                    json!({"manifest":changed})
                ))
                .error
                .is_some()
        );
        assert!(
            store
                .dispatch(command(
                    "commit",
                    "files.importCommit",
                    json!({"manifestId":"legacy-bytes"})
                ))
                .error
                .is_some()
        );
        assert!(row(&store.connection, "old-upload").unwrap().is_null());
        let chunk = json!({"manifestId":"legacy-bytes","entry":0,"offset":0,"base64":STANDARD.encode(b"wrong!!!")});
        assert!(
            store
                .dispatch(command("chunk", "files.importChunk", chunk))
                .error
                .is_none()
        );
        assert!(store.dispatch(command("changed-chunk","files.importChunk",json!({"manifestId":"legacy-bytes","entry":0,"offset":0,"base64":STANDARD.encode(b"original")}))).error.is_some());
        assert!(
            store
                .dispatch(command(
                    "commit2",
                    "files.importCommit",
                    json!({"manifestId":"legacy-bytes"})
                ))
                .error
                .is_some()
        );
        assert!(row(&store.connection, "old-upload").unwrap().is_null());
        store
            .connection
            .execute(
                "UPDATE client_identity SET value=?1",
                [json!({"workspaceId":"b".repeat(32)}).to_string()],
            )
            .unwrap();
        assert!(
            store
                .dispatch(command(
                    "wrong-scope",
                    "files.importCommit",
                    json!({"manifestId":"legacy-bytes"})
                ))
                .error
                .is_some()
        );
    }

    #[test]
    fn byte_hash_stream_preserves_offsets_and_resumes_after_restart() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("client.sqlite3");
        let mut store = Store::open(&path).unwrap();
        assert!(
            store
                .dispatch(command(
                    "hash-begin",
                    "files.hashBegin",
                    json!({"id":"source-hash","bytes":8})
                ))
                .error
                .is_none()
        );
        assert!(
            store
                .dispatch(command(
                    "hash-chunk",
                    "files.hashChunk",
                    json!({"id":"source-hash","offset":0,"base64":STANDARD.encode(b"orig")})
                ))
                .error
                .is_none()
        );
        assert!(
            store
                .dispatch(command(
                    "hash-gap",
                    "files.hashChunk",
                    json!({"id":"source-hash","offset":5,"base64":STANDARD.encode(b"nal")})
                ))
                .error
                .is_some()
        );
        assert!(
            store
                .dispatch(command(
                    "hash-early",
                    "files.hashFinish",
                    json!({"id":"source-hash"})
                ))
                .error
                .is_some()
        );
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert!(
            store
                .dispatch(command(
                    "hash-retry",
                    "files.hashChunk",
                    json!({"id":"source-hash","offset":0,"base64":STANDARD.encode(b"orig")})
                ))
                .error
                .is_none()
        );
        assert!(
            store
                .dispatch(command(
                    "hash-conflict",
                    "files.hashChunk",
                    json!({"id":"source-hash","offset":0,"base64":STANDARD.encode(b"else")})
                ))
                .error
                .is_some()
        );
        assert!(
            store
                .dispatch(command(
                    "hash-end",
                    "files.hashChunk",
                    json!({"id":"source-hash","offset":4,"base64":STANDARD.encode(b"inal")})
                ))
                .error
                .is_none()
        );
        let result = store.dispatch(command(
            "hash-finish",
            "files.hashFinish",
            json!({"id":"source-hash"}),
        ));
        assert_eq!(
            result.result,
            Some(json!({"bytes":8,"sha256":byte_hash(b"original")}))
        );
        assert_eq!(
            store
                .dispatch(command(
                    "hash-replay",
                    "files.hashFinish",
                    json!({"id":"source-hash"})
                ))
                .result,
            result.result
        );
    }
    #[test]
    fn queue_is_atomic_and_cancelled_upload_cannot_return_after_restart() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("client.sqlite3");
        let mut store = Store::open(&path).unwrap();
        let file = store
            .dispatch(command(
                "file",
                "files.text",
                json!({"name":"message.txt","text":"Привет 🦀","mime":"text/plain"}),
            ))
            .result
            .unwrap();
        let inputs =
            json!({"workspace":"a".repeat(32),"stateDir":"/state","agent":"lead","files":[file]});
        let mut invalid = inputs.clone();
        invalid["files"]
            .as_array_mut()
            .unwrap()
            .push(json!({"nativeHandle":"missing"}));
        assert!(
            store
                .dispatch(command("invalid", "uploads.queue", invalid))
                .error
                .is_some()
        );
        let rows = store
            .dispatch(command("queue", "uploads.queue", inputs.clone()))
            .result
            .unwrap();
        let id = rows[0]["id"].clone();
        assert_eq!(rows[0]["size"], "Привет 🦀".len());
        assert_eq!(
            store
                .dispatch(command("queue", "uploads.queue", inputs))
                .result,
            Some(rows)
        );
        assert!(
            store
                .dispatch(command("cancel", "uploads.cancel", json!({"id":id})))
                .error
                .is_none()
        );
        drop(store);
        let mut store = Store::open(&path).unwrap();
        assert_eq!(
            store
                .dispatch(command(
                    "late",
                    "uploads.receipt",
                    json!({"id":id,"asset":{"id":id,"name":"message.txt"}})
                ))
                .result,
            Some(Value::Null)
        );
        assert_eq!(
            store
                .dispatch(command("status", "uploads.status", json!({"id":id})))
                .result
                .unwrap()["cancelled"],
            true
        );
        assert_eq!(
            store
                .dispatch(command("bytes", "uploads.content", json!({"id":id})))
                .result,
            Some(Value::Null)
        );
    }
    #[tokio::test]
    async fn upload_retry_keeps_exact_bytes_and_cancel_wins_over_a_late_http_receipt() {
        use crate::backend::tests::{read_http, reply};
        use tokio::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let origin = format!("http://{}", listener.local_addr().unwrap());
        let (arrived, arrival) = tokio::sync::oneshot::channel();
        let (release, released) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let mut first = None;
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let (headers, body) = read_http(&mut stream).await;
                if headers.starts_with("GET /api/sync/identity ") {
                    reply(
                        &mut stream,
                        &json!({"workspaceId":"a".repeat(32)}).to_string(),
                    )
                    .await;
                } else if headers.starts_with("GET /api/session ") {
                    reply(&mut stream, r#"{"token":"test"}"#).await;
                } else {
                    assert!(headers.starts_with("POST /api/assets "));
                    assert!(
                        headers
                            .to_ascii_lowercase()
                            .contains(&format!("x-canvas-workspace: {}", "a".repeat(32)))
                    );
                    assert_eq!(
                        STANDARD.decode(body["base64"].as_str().unwrap()).unwrap(),
                        "Привет 🦀".as_bytes()
                    );
                    if first.is_none() {
                        first = Some(body);
                        drop(stream);
                        continue;
                    }
                    assert_eq!(first.as_ref(), Some(&body));
                    arrived.send(()).unwrap();
                    released.await.unwrap();
                    reply(&mut stream,&json!({"asset":{"id":body["id"],"name":"message.txt","mime":"text/plain","size":17,"image":false}}).to_string()).await;
                    break;
                }
            }
        });
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("client.sqlite3");
        let row = {
            let mut store = Store::open(&path).unwrap();
            let file = store
                .dispatch(command(
                    "file",
                    "files.text",
                    json!({"name":"message.txt","text":"Привет 🦀"}),
                ))
                .result
                .unwrap();
            store.dispatch(command("queue","uploads.queue",json!({"workspace":"a".repeat(32),"stateDir":"/state","agent":"lead","files":[file]}))).result.unwrap()[0].clone()
        };
        let start = |path: std::path::PathBuf| {
            let (sender, mut receive) =
                tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(64);
            let thread = std::thread::spawn(move || {
                let mut store = Store::open(&path).unwrap();
                while let Some((request, reply)) = receive.blocking_recv() {
                    let _ = reply.send(store.dispatch(request));
                }
            });
            (sender, thread)
        };
        let (sender, thread) = start(path.clone());
        let backend = Backend::new(&origin).unwrap();
        assert!(
            dispatch(
                &backend,
                &sender,
                command("first", "uploads.deliver", json!({"row":row}))
            )
            .await
            .error
            .is_some()
        );
        drop(sender);
        thread.join().unwrap();
        let (sender, thread) = start(path);
        let delivery_sender = sender.clone();
        let delivery_row = row.clone();
        let delivery = tokio::spawn(async move {
            dispatch(
                &backend,
                &delivery_sender,
                command("retry", "uploads.deliver", json!({"row":delivery_row})),
            )
            .await
        });
        tokio::time::timeout(std::time::Duration::from_secs(3), arrival)
            .await
            .unwrap()
            .unwrap();
        state(&sender, "uploads.cancel", json!({"id":row["id"]}))
            .await
            .unwrap();
        release.send(()).unwrap();
        let response = delivery.await.unwrap();
        assert!(response.error.is_none());
        assert_eq!(response.result, Some(Value::Null));
        assert!(
            state(&sender, "uploads.get", json!({"id":row["id"]}))
                .await
                .unwrap()
                .is_null()
        );
        drop(sender);
        thread.join().unwrap();
        server.await.unwrap();
    }
}
