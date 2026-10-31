//! Moves what the removed client store held to the server: drafts, unsent
//! uploads and recordings. A local row is deleted only after the server returns
//! the same data. Any failure keeps the local row and tries again at the next start.
use crate::backend::Backend;
use crate::protocol::Request;
use base64::Engine;
use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::{SystemTime, UNIX_EPOCH},
};

/// Draft documents per page of the server pull. Unit: documents.
const DRAFT_PAGE: usize = 100;
/// File name of the removed store inside the local client folder.
pub const STORE_FILE: &str = "studio-client.sqlite3";

/// What one run moved. `kept` lists each local row left in place, with the reason.
#[derive(Debug, Default, PartialEq)]
pub struct Report {
    pub drafts: usize,
    pub uploads: usize,
    pub recordings: usize,
    pub kept: Vec<String>,
}

/// A draft document in the wire format of `/api/sync/drafts` and `/api/sync/pull`.
#[derive(Clone, Debug, Deserialize)]
pub(crate) struct Doc {
    id: String,
    payload: String,
    #[serde(default)]
    seq: u64,
    #[serde(default, alias = "deleted", rename = "_deleted")]
    deleted: bool,
}
impl Doc {
    fn wire(&self) -> Value {
        json!({"id":self.id,"payload":self.payload,"seq":self.seq,"deleted":self.deleted})
    }
    fn same(&self, other: &Doc) -> bool {
        self.deleted == other.deleted
            && serde_json::from_str::<Value>(&self.payload).ok()
                == serde_json::from_str::<Value>(&other.payload).ok()
    }
}

/// The server calls of the move. The tests replace it with an in-memory server.
pub(crate) trait Server {
    async fn workspace(&self) -> Result<String, String>;
    /// Returns the ids of rows that the server refused as conflicts.
    async fn push_drafts(&self, workspace: &str, rows: Vec<Value>) -> Result<Vec<String>, String>;
    async fn drafts(&self, workspace: &str) -> Result<Vec<Doc>, String>;
    async fn upload(
        &self,
        workspace: &str,
        agent: &str,
        id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), String>;
    async fn read_asset(&self, agent: &str, id: &str) -> Result<Vec<u8>, String>;
}

impl Server for Backend {
    async fn workspace(&self) -> Result<String, String> {
        let value = call(self, json!({"path":"/api/sync/identity","timeoutMs":15000})).await?;
        value["workspaceId"]
            .as_str()
            .filter(|id| !id.is_empty())
            .map(str::to_owned)
            .ok_or_else(|| "The server returned no workspace identity.".into())
    }
    async fn push_drafts(&self, workspace: &str, rows: Vec<Value>) -> Result<Vec<String>, String> {
        let value = call(
            self,
            json!({"path":"/api/sync/drafts","body":{"rows":rows},"workspaceId":workspace,"timeoutMs":15000}),
        )
        .await?;
        value
            .as_array()
            .ok_or("The server returned an invalid draft response.")?
            .iter()
            .map(|conflict| {
                conflict["id"]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "The server returned a draft conflict without an id.".into())
            })
            .collect()
    }
    async fn drafts(&self, workspace: &str) -> Result<Vec<Doc>, String> {
        let mut after = 0u64;
        let mut documents = BTreeMap::new();
        loop {
            let page = call(
                self,
                json!({"path":format!("/api/sync/pull?scope=drafts&after={after}&limit={DRAFT_PAGE}"),"timeoutMs":15000}),
            )
            .await?;
            if page["workspaceId"].as_str() != Some(workspace) {
                return Err("The server workspace changed during the draft move.".into());
            }
            let next = page["checkpoint"]["seq"]
                .as_u64()
                .ok_or("The server returned an invalid draft checkpoint.")?;
            let batch = page["documents"]
                .as_array()
                .ok_or("The server returned an invalid draft batch.")?;
            for value in batch {
                let doc = Doc::deserialize(value).map_err(|error| error.to_string())?;
                documents.insert(doc.id.clone(), doc);
            }
            if batch.len() < DRAFT_PAGE {
                return Ok(documents.into_values().collect());
            }
            if next <= after {
                return Err("The draft checkpoint did not advance.".into());
            }
            after = next;
        }
    }
    async fn upload(
        &self,
        workspace: &str,
        agent: &str,
        id: &str,
        name: &str,
        bytes: &[u8],
    ) -> Result<(), String> {
        let base64 = base64::engine::general_purpose::STANDARD.encode(bytes);
        let record = call(
            self,
            json!({"path":"/api/assets","body":{"agent":agent,"name":name,"base64":base64,"id":id},
                "workspaceId":workspace,"timeoutMs":120000}),
        )
        .await?;
        if record["id"].as_str() == Some(id) {
            Ok(())
        } else {
            Err("The upload response does not match the saved file.".into())
        }
    }
    async fn read_asset(&self, agent: &str, id: &str) -> Result<Vec<u8>, String> {
        let value = call(
            self,
            json!({"path":format!("/api/file?agent={}&asset={}", query(agent), query(id)),"timeoutMs":120000}),
        )
        .await?;
        base64::engine::general_purpose::STANDARD
            .decode(value["base64"].as_str().unwrap_or_default())
            .map_err(|error| error.to_string())
    }
}

fn query(value: &str) -> String {
    percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC).to_string()
}

async fn call(backend: &Backend, params: Value) -> Result<Value, String> {
    let response = backend
        .dispatch(Request {
            id: "store-move".into(),
            method: "api.request".into(),
            params,
        })
        .await;
    match response.error {
        Some(error) => Err(error.message),
        None => response
            .result
            .ok_or_else(|| "The server returned no result.".into()),
    }
}

/// Moves the old store at `path` to the server behind `backend`. A missing file
/// is a finished move. An error means the file could not be read at all.
pub async fn move_to_server(backend: &Backend, path: &Path) -> Result<Report, String> {
    run(backend, path).await
}

async fn run(server: &impl Server, path: &Path) -> Result<Report, String> {
    let mut report = Report::default();
    if !path.is_file() {
        return Ok(report);
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| error.to_string())?;
    if has_table(&db, "client_draft_branches")? || has_table(&db, "client_drafts")? {
        let rows = draft_rows(&db)?;
        move_drafts(&db, server, rows, &mut report).await;
    }
    if has_table(&db, "client_uploads")? {
        move_uploads(&db, server, &mut report).await;
    }
    if has_table(&db, "client_dictation_recordings")? && has_table(&db, "client_dictation_chunks")?
    {
        move_recordings(&db, server, &mut report).await;
    }
    if has_table(&db, "client_file_sources")? {
        let staged: i64 = db
            .query_row("SELECT count(*) FROM client_file_sources", [], |row| {
                row.get(0)
            })
            .map_err(|error| error.to_string())?;
        if staged > 0 {
            // A staged pick names no chat, and the server stores an asset for a chat only.
            report.kept.push(format!(
                "{staged} staged files stay in client_file_sources: no chat to upload them to"
            ));
        }
    }
    db.execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
        .map_err(|error| error.to_string())?;
    Ok(report)
}

/// Preference rows of the removed store for one scope (`root` or `server:<id>`),
/// with the scope prefix removed. Rows without a `browser:` prefix belong to `root`.
pub fn legacy_preferences(path: &Path, scope: &str) -> Result<BTreeMap<String, String>, String> {
    let Some(db) = open_preferences(path)? else {
        return Ok(BTreeMap::new());
    };
    let prefix = scope_prefix(scope)?;
    let mut query = db
        .prepare("SELECT key,value FROM client_preferences")
        .map_err(|error| error.to_string())?;
    let rows = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut found = BTreeMap::new();
    for row in rows {
        let (key, value) = row.map_err(|error| error.to_string())?;
        if let Some(name) = key.strip_prefix(&prefix) {
            found.insert(name.to_owned(), value);
        } else if scope == "root" && !key.starts_with("browser:") {
            found.insert(key, value);
        }
    }
    Ok(found)
}

/// Deletes preference rows after the page has stored and read back their values.
pub fn clear_legacy_preferences(path: &Path, scope: &str, keys: &[String]) -> Result<(), String> {
    let Some(db) = open_preferences(path)? else {
        return Ok(());
    };
    let prefix = scope_prefix(scope)?;
    for key in keys {
        db.execute(
            "DELETE FROM client_preferences WHERE key=?1 OR (?3 AND key=?2)",
            params![format!("{prefix}{key}"), key, scope == "root"],
        )
        .map_err(|error| error.to_string())?;
    }
    Ok(())
}

fn scope_prefix(scope: &str) -> Result<String, String> {
    if scope == "root" || scope.starts_with("server:") {
        Ok(format!(
            "browser:{}:",
            serde_json::to_string(scope).map_err(|error| error.to_string())?
        ))
    } else {
        Err("Unknown preference scope.".into())
    }
}

fn open_preferences(path: &Path) -> Result<Option<Connection>, String> {
    if !path.is_file() {
        return Ok(None);
    }
    let db = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_WRITE)
        .map_err(|error| error.to_string())?;
    Ok(has_table(&db, "client_preferences")?.then_some(db))
}

fn has_table(db: &Connection, name: &str) -> Result<bool, String> {
    db.query_row(
        "SELECT 1 FROM sqlite_master WHERE type='table' AND name=?1",
        [name],
        |_| Ok(()),
    )
    .optional()
    .map(|found| found.is_some())
    .map_err(|error| error.to_string())
}

enum Origin {
    Branch,
    Legacy(String),
}
struct DraftRow {
    workspace: String,
    local: Doc,
    master: Option<Doc>,
    pushed: bool,
    origin: Origin,
}

fn draft_rows(db: &Connection) -> Result<Vec<DraftRow>, String> {
    let sql = |error: rusqlite::Error| error.to_string();
    let mut rows = Vec::new();
    if has_table(db, "client_draft_branches")? {
        let mut query = db
            .prepare("SELECT workspace,document,master,dirty FROM client_draft_branches ORDER BY workspace,id")
            .map_err(sql)?;
        let found = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, bool>(3)?,
                ))
            })
            .map_err(sql)?;
        for row in found {
            let (workspace, local, master, dirty) = row.map_err(sql)?;
            rows.push(DraftRow {
                workspace,
                local: serde_json::from_str(&local).map_err(|error| error.to_string())?,
                master: master
                    .map(|text| serde_json::from_str(&text))
                    .transpose()
                    .map_err(|error| error.to_string())?,
                pushed: !dirty,
                origin: Origin::Branch,
            });
        }
    }
    if has_table(db, "client_drafts")? {
        let device = if has_table(db, "client_draft_device")? {
            db.query_row(
                "SELECT device FROM client_draft_device WHERE singleton=1",
                [],
                |row| row.get::<_, String>(0),
            )
            .optional()
            .map_err(sql)?
        } else {
            None
        }
        .unwrap_or_else(|| "legacy".into());
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|error| error.to_string())?
            .as_millis() as u64;
        let mut query = db
            .prepare("SELECT workspace,room,text FROM client_drafts ORDER BY workspace,room")
            .map_err(sql)?;
        let found = query
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })
            .map_err(sql)?;
        for row in found {
            let (workspace, room, text) = row.map_err(sql)?;
            let id = format!("{device}:{room}");
            let version = json!({"id":id,"session":room,"device":device,"text":text,"updated":now});
            rows.push(DraftRow {
                workspace,
                local: Doc {
                    id,
                    payload: version.to_string(),
                    seq: 0,
                    deleted: false,
                },
                master: None,
                pushed: false,
                origin: Origin::Legacy(room),
            });
        }
    }
    Ok(rows)
}

async fn move_drafts(
    db: &Connection,
    server: &impl Server,
    rows: Vec<DraftRow>,
    report: &mut Report,
) {
    let mut by_workspace = BTreeMap::<String, Vec<DraftRow>>::new();
    for row in rows {
        by_workspace
            .entry(row.workspace.clone())
            .or_default()
            .push(row);
    }
    let current = server.workspace().await;
    for (workspace, rows) in by_workspace {
        match &current {
            Ok(id) if *id == workspace => {}
            Ok(_) => {
                report.kept.push(format!(
                    "{} drafts of workspace {workspace} belong to another server",
                    rows.len()
                ));
                continue;
            }
            Err(error) => {
                report.kept.push(format!("drafts of {workspace}: {error}"));
                continue;
            }
        }
        let refused = match push_pending(server, &workspace, &rows).await {
            Ok(refused) => refused,
            Err(error) => {
                report.kept.push(format!("drafts of {workspace}: {error}"));
                continue;
            }
        };
        let confirmed = match server.drafts(&workspace).await {
            Ok(documents) => documents,
            Err(error) => {
                report.kept.push(format!("drafts of {workspace}: {error}"));
                continue;
            }
        };
        let confirmed: BTreeMap<_, _> = confirmed.iter().map(|doc| (&doc.id, doc)).collect();
        for row in rows {
            let held = confirmed
                .get(&row.local.id)
                .is_some_and(|doc| doc.same(&row.local));
            if refused.contains(&row.local.id) || !held {
                report.kept.push(format!(
                    "draft {} of {workspace} is not on the server",
                    row.local.id
                ));
                continue;
            }
            let deleted = match &row.origin {
                Origin::Branch => db.execute(
                    "DELETE FROM client_draft_branches WHERE workspace=?1 AND id=?2",
                    params![workspace, row.local.id],
                ),
                Origin::Legacy(room) => db.execute(
                    "DELETE FROM client_drafts WHERE workspace=?1 AND room=?2",
                    params![workspace, room],
                ),
            };
            match deleted {
                Ok(_) => report.drafts += 1,
                Err(error) => report.kept.push(format!("draft {}: {error}", row.local.id)),
            }
        }
    }
}

/// Pushes the rows that the server may not hold yet. Returns the refused ids.
async fn push_pending(
    server: &impl Server,
    workspace: &str,
    rows: &[DraftRow],
) -> Result<BTreeSet<String>, String> {
    let wire: Vec<Value> = rows
        .iter()
        .filter(|row| !row.pushed)
        .map(|row| {
            json!({
                "newDocumentState": row.local.wire(),
                "assumedMasterState": row.master.as_ref().map(Doc::wire),
            })
        })
        .collect();
    let mut refused = BTreeSet::new();
    for batch in wire.chunks(DRAFT_PAGE) {
        refused.extend(server.push_drafts(workspace, batch.to_vec()).await?);
    }
    Ok(refused)
}

/// A UUID for the server asset. A stored id that is a UUID stays; any other id
/// maps to the same UUID on every run, so a retry replays the same upload.
fn asset_id(stored: &str) -> String {
    if let Ok(id) = uuid::Uuid::parse_str(stored) {
        return id.to_string();
    }
    let digest = Sha256::digest(format!("store-move:{stored}"));
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    uuid::Builder::from_random_bytes(bytes)
        .into_uuid()
        .to_string()
}

fn text<'a>(document: &'a Value, names: &[&str]) -> Option<&'a str> {
    names
        .iter()
        .find_map(|name| document[*name].as_str())
        .filter(|value| !value.is_empty())
}

/// Uploads one file and deletes the local copy only if the server returns the same bytes.
async fn move_file(
    server: &impl Server,
    workspace: &str,
    agent: &str,
    stored_id: &str,
    name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    let id = asset_id(stored_id);
    server.upload(workspace, agent, &id, name, bytes).await?;
    if server.read_asset(agent, &id).await? == bytes {
        Ok(())
    } else {
        Err("The server returned different bytes.".into())
    }
}

/// One row of `client_uploads`.
struct UploadRow {
    id: String,
    workspace: String,
    agent: String,
    document: String,
    bytes: Vec<u8>,
}

async fn move_uploads(db: &Connection, server: &impl Server, report: &mut Report) {
    let rows = (|| -> rusqlite::Result<Vec<UploadRow>> {
        let cancelled = if has_table(db, "client_upload_tombstones").unwrap_or(false) {
            let mut query =
                db.prepare("SELECT id FROM client_upload_tombstones WHERE cancelled<>0")?;
            query
                .query_map([], |row| row.get::<_, String>(0))?
                .collect::<Result<BTreeSet<_>, _>>()?
        } else {
            BTreeSet::new()
        };
        let mut query =
            db.prepare("SELECT id,workspace,agent,document,bytes FROM client_uploads ORDER BY id")?;
        let rows = query.query_map([], |row| {
            Ok(UploadRow {
                id: row.get(0)?,
                workspace: row.get(1)?,
                agent: row.get(2)?,
                document: row.get(3)?,
                bytes: row.get(4)?,
            })
        })?;
        let mut found = Vec::new();
        for row in rows {
            let row = row?;
            if !cancelled.contains(&row.id) {
                found.push(row);
            }
        }
        Ok(found)
    })();
    let rows = match rows {
        Ok(rows) => rows,
        Err(error) => {
            report.kept.push(format!("uploads: {error}"));
            return;
        }
    };
    let current = server.workspace().await;
    for UploadRow {
        id,
        workspace,
        agent,
        document,
        bytes,
    } in rows
    {
        let name = serde_json::from_str::<Value>(&document)
            .ok()
            .and_then(|value| text(&value, &["name"]).map(str::to_owned))
            .unwrap_or_else(|| id.clone());
        let moved = match &current {
            Ok(server_workspace) if *server_workspace == workspace => {
                move_file(server, &workspace, &agent, &id, &name, &bytes).await
            }
            Ok(_) => Err("the upload belongs to another server".into()),
            Err(error) => Err(error.clone()),
        };
        match moved.and_then(|()| {
            db.execute("DELETE FROM client_uploads WHERE id=?1", [&id])
                .map_err(|error| error.to_string())
        }) {
            Ok(_) => report.uploads += 1,
            Err(error) => report.kept.push(format!("upload {id}: {error}")),
        }
    }
}

async fn move_recordings(db: &Connection, server: &impl Server, report: &mut Report) {
    let recordings = (|| -> rusqlite::Result<Vec<(String, String)>> {
        let mut query =
            db.prepare("SELECT id,document FROM client_dictation_recordings ORDER BY id")?;
        query
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect()
    })();
    let recordings = match recordings {
        Ok(rows) => rows,
        Err(error) => {
            report.kept.push(format!("recordings: {error}"));
            return;
        }
    };
    let current = server.workspace().await;
    for (id, document) in recordings {
        let moved = async {
            let workspace = current.clone()?;
            let value = serde_json::from_str::<Value>(&document).unwrap_or(Value::Null);
            // Fields of the removed dictation writer: chatId, sampleRate, deletedAt.
            if value["deletedAt"].is_number() {
                return Err("the user deleted the recording".to_owned());
            }
            let agent = text(&value, &["chatId"])
                .ok_or("the recording names no chat")?
                .to_owned();
            let rate = value["sampleRate"]
                .as_u64()
                .filter(|rate| (8000..=192_000).contains(rate))
                .ok_or("the recording has no valid sample rate")? as u32;
            let pcm = recording_bytes(db, &id)?;
            if pcm.is_empty() || pcm.len() % 2 != 0 {
                return Err("the recording has no whole 16-bit audio".to_owned());
            }
            // The same mono 16-bit WAV that the removed export wrote.
            let mut wav = wav_header(pcm.len() / 2, rate).to_vec();
            wav.extend(pcm);
            move_file(server, &workspace, &agent, &id, &format!("{id}.wav"), &wav).await
        }
        .await;
        match moved.and_then(|()| delete_recording(db, &id)) {
            Ok(()) => report.recordings += 1,
            Err(error) => report.kept.push(format!("recording {id}: {error}")),
        }
    }
}

fn wav_header(samples: usize, sample_rate: u32) -> [u8; 44] {
    let data = (samples * 2) as u32;
    let mut header = [0u8; 44];
    header[0..4].copy_from_slice(b"RIFF");
    header[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    header[8..16].copy_from_slice(b"WAVEfmt ");
    header[16..20].copy_from_slice(&16u32.to_le_bytes());
    header[20..22].copy_from_slice(&1u16.to_le_bytes());
    header[22..24].copy_from_slice(&1u16.to_le_bytes());
    header[24..28].copy_from_slice(&sample_rate.to_le_bytes());
    header[28..32].copy_from_slice(&(sample_rate * 2).to_le_bytes());
    header[32..34].copy_from_slice(&2u16.to_le_bytes());
    header[34..36].copy_from_slice(&16u16.to_le_bytes());
    header[36..40].copy_from_slice(b"data");
    header[40..44].copy_from_slice(&data.to_le_bytes());
    header
}

fn recording_bytes(db: &Connection, id: &str) -> Result<Vec<u8>, String> {
    let sql = |error: rusqlite::Error| error.to_string();
    let mut query = db
        .prepare("SELECT pcm FROM client_dictation_chunks WHERE id=?1 ORDER BY position")
        .map_err(sql)?;
    let chunks = query
        .query_map([id], |row| row.get::<_, Vec<u8>>(0))
        .map_err(sql)?;
    let mut bytes = Vec::new();
    for chunk in chunks {
        bytes.extend(chunk.map_err(sql)?);
    }
    Ok(bytes)
}

fn delete_recording(db: &Connection, id: &str) -> Result<(), String> {
    let sql = |error: rusqlite::Error| error.to_string();
    let transaction = db.unchecked_transaction().map_err(sql)?;
    transaction
        .execute("DELETE FROM client_dictation_chunks WHERE id=?1", [id])
        .map_err(sql)?;
    transaction
        .execute("DELETE FROM client_dictation_recordings WHERE id=?1", [id])
        .map_err(sql)?;
    transaction.commit().map_err(sql)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    const WORKSPACE: &str = "workspace-one";
    const AGENT: &str = "11111111-1111-4111-8111-111111111111";

    #[derive(Default)]
    struct Fake {
        drafts: Mutex<BTreeMap<String, Doc>>,
        assets: Mutex<BTreeMap<String, Vec<u8>>>,
        /// Reads return wrong data, as a lost or damaged copy would.
        damaged_reads: bool,
        /// The server refuses these draft ids as conflicts.
        conflicts: Vec<String>,
    }
    impl Server for Fake {
        async fn workspace(&self) -> Result<String, String> {
            Ok(WORKSPACE.into())
        }
        async fn push_drafts(
            &self,
            _workspace: &str,
            rows: Vec<Value>,
        ) -> Result<Vec<String>, String> {
            let mut refused = Vec::new();
            for row in rows {
                let doc = Doc::deserialize(&row["newDocumentState"]).map_err(|e| e.to_string())?;
                if self.conflicts.contains(&doc.id) {
                    refused.push(doc.id);
                } else {
                    self.drafts.lock().unwrap().insert(doc.id.clone(), doc);
                }
            }
            Ok(refused)
        }
        async fn drafts(&self, _workspace: &str) -> Result<Vec<Doc>, String> {
            Ok(self.drafts.lock().unwrap().values().cloned().collect())
        }
        async fn upload(
            &self,
            _workspace: &str,
            _agent: &str,
            id: &str,
            _name: &str,
            bytes: &[u8],
        ) -> Result<(), String> {
            self.assets.lock().unwrap().insert(id.into(), bytes.into());
            Ok(())
        }
        async fn read_asset(&self, _agent: &str, id: &str) -> Result<Vec<u8>, String> {
            if self.damaged_reads {
                return Ok(b"damaged".to_vec());
            }
            self.assets
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| "missing".into())
        }
    }

    fn version(session: &str, text: &str) -> String {
        json!({"id":format!("dev:{session}"),"session":session,"device":"dev","text":text,"updated":1})
            .to_string()
    }
    fn branch(session: &str, text: &str) -> String {
        json!({"id":format!("dev:{session}"),"payload":version(session, text),"seq":0,"_deleted":false})
            .to_string()
    }

    /// A store with two draft branches (one sent, one not), one legacy draft,
    /// one unsent upload and one recording of two chunks.
    fn fixture(path: &Path) {
        let db = Connection::open(path).unwrap();
        db.execute_batch(
            "CREATE TABLE client_draft_branches (workspace TEXT NOT NULL,id TEXT NOT NULL,document TEXT NOT NULL,master TEXT,dirty INTEGER NOT NULL,PRIMARY KEY(workspace,id));
             CREATE TABLE client_drafts (workspace TEXT NOT NULL, room TEXT NOT NULL, text TEXT NOT NULL, revision INTEGER NOT NULL, PRIMARY KEY(workspace, room));
             CREATE TABLE client_uploads(id TEXT PRIMARY KEY,workspace TEXT NOT NULL,state_dir TEXT NOT NULL,agent TEXT NOT NULL,document TEXT NOT NULL,bytes BLOB NOT NULL);
             CREATE TABLE client_upload_tombstones(id TEXT PRIMARY KEY,cancelled INTEGER NOT NULL);
             CREATE TABLE client_file_sources(id TEXT PRIMARY KEY,name TEXT NOT NULL,mime TEXT NOT NULL,bytes BLOB NOT NULL);
             CREATE TABLE client_preferences (key TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE client_dictation_recordings(id TEXT PRIMARY KEY, document TEXT NOT NULL);
             CREATE TABLE client_dictation_chunks(id TEXT NOT NULL, position INTEGER NOT NULL, pcm BLOB NOT NULL, PRIMARY KEY(id, position));",
        )
        .unwrap();
        db.execute(
            "INSERT INTO client_draft_branches VALUES(?1,'dev:one',?2,NULL,1)",
            params![WORKSPACE, branch("one", "first")],
        )
        .unwrap();
        db.execute(
            "INSERT INTO client_draft_branches VALUES(?1,'dev:two',?2,?2,0)",
            params![WORKSPACE, branch("two", "second")],
        )
        .unwrap();
        db.execute(
            "INSERT INTO client_drafts VALUES(?1,'room',?2,3)",
            params![WORKSPACE, "legacy text"],
        )
        .unwrap();
        db.execute(
            "INSERT INTO client_uploads VALUES('not-a-uuid',?1,'dir',?2,?3,X'0102')",
            params![WORKSPACE, AGENT, json!({"name":"note.txt"}).to_string()],
        )
        .unwrap();
        db.execute(
            "INSERT INTO client_dictation_recordings VALUES('rec',?1)",
            [json!({"chatId":AGENT,"sampleRate":16000,"state":"ready"}).to_string()],
        )
        .unwrap();
        db.execute_batch(
            "INSERT INTO client_dictation_chunks VALUES('rec',1,X'0304'); INSERT INTO client_dictation_chunks VALUES('rec',0,X'0102');",
        )
        .unwrap();
    }
    fn count(path: &Path, table: &str) -> i64 {
        Connection::open(path)
            .unwrap()
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap()
    }

    #[tokio::test]
    async fn the_move_keeps_every_draft_upload_and_recording() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STORE_FILE);
        fixture(&path);
        // The sent branch was confirmed by the server before the move.
        let server = Fake::default();
        let sent: Value = serde_json::from_str(&branch("two", "second")).unwrap();
        server
            .drafts
            .lock()
            .unwrap()
            .insert("dev:two".into(), Doc::deserialize(&sent).unwrap());
        let report = run(&server, &path).await.unwrap();
        assert_eq!(
            report,
            Report {
                drafts: 3,
                uploads: 1,
                recordings: 1,
                kept: vec![]
            },
            "{report:?}"
        );
        let drafts = server.drafts.lock().unwrap();
        assert!(drafts.contains_key("dev:one"));
        assert!(drafts.keys().any(|id| id.ends_with(":room")));
        let assets = server.assets.lock().unwrap();
        assert!(assets.values().any(|bytes| bytes == &[1, 2]));
        let mut wav = wav_header(2, 16000).to_vec();
        wav.extend([1, 2, 3, 4]);
        assert!(assets.values().any(|bytes| bytes == &wav));
        for table in [
            "client_draft_branches",
            "client_drafts",
            "client_uploads",
            "client_dictation_recordings",
            "client_dictation_chunks",
        ] {
            assert_eq!(count(&path, table), 0, "{table}");
        }
    }

    #[tokio::test]
    async fn a_failed_read_back_keeps_the_local_copy() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STORE_FILE);
        fixture(&path);
        let server = Fake {
            damaged_reads: true,
            conflicts: vec!["dev:one".into()],
            ..Fake::default()
        };
        let report = run(&server, &path).await.unwrap();
        assert_eq!(
            (report.drafts, report.uploads, report.recordings),
            (1, 0, 0)
        );
        assert_eq!(count(&path, "client_uploads"), 1);
        assert_eq!(count(&path, "client_dictation_recordings"), 1);
        assert_eq!(count(&path, "client_dictation_chunks"), 2);
        // The refused branch and the branch that the server does not hold both stay.
        assert_eq!(count(&path, "client_draft_branches"), 2);
        assert_eq!(count(&path, "client_drafts"), 0);
        assert!(report.kept.iter().any(|line| line.contains("dev:one")));
        assert!(report.kept.iter().any(|line| line.contains("dev:two")));
    }

    #[tokio::test]
    async fn a_store_of_another_workspace_stays_untouched() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STORE_FILE);
        fixture(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "UPDATE client_draft_branches SET workspace='other'; UPDATE client_drafts SET workspace='other'; UPDATE client_uploads SET workspace='other';",
            )
            .unwrap();
        let server = Fake::default();
        let report = run(&server, &path).await.unwrap();
        assert_eq!((report.drafts, report.uploads), (0, 0));
        assert_eq!(count(&path, "client_draft_branches"), 2);
        assert_eq!(count(&path, "client_drafts"), 1);
        assert_eq!(count(&path, "client_uploads"), 1);
    }

    #[tokio::test]
    async fn a_missing_store_is_a_finished_move() {
        let directory = tempfile::tempdir().unwrap();
        let report = run(&Fake::default(), &directory.path().join(STORE_FILE))
            .await
            .unwrap();
        assert_eq!(report, Report::default());
    }

    #[tokio::test]
    async fn a_recording_without_a_chat_stays_and_is_listed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STORE_FILE);
        fixture(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "UPDATE client_dictation_recordings SET document='{\"sampleRate\":16000}';
                 INSERT INTO client_file_sources VALUES('pick','a.txt','text/plain',X'01');",
            )
            .unwrap();
        let report = run(&Fake::default(), &path).await.unwrap();
        assert_eq!(report.recordings, 0);
        assert_eq!(count(&path, "client_dictation_recordings"), 1);
        assert!(
            report
                .kept
                .iter()
                .any(|line| line.contains("recording rec"))
        );
        // Staged picks stay and are listed.
        assert_eq!(count(&path, "client_file_sources"), 1);
        assert!(report.kept.iter().any(|line| line.contains("staged files")));
    }

    #[test]
    fn legacy_preferences_are_read_by_scope_and_cleared_by_key() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(STORE_FILE);
        fixture(&path);
        Connection::open(&path)
            .unwrap()
            .execute_batch(
                "INSERT INTO client_preferences VALUES('browser:\"root\":theme','dark');
                 INSERT INTO client_preferences VALUES('browser:\"server:s1\":theme','light');
                 INSERT INTO client_preferences VALUES('old-key','kept');",
            )
            .unwrap();
        let root = legacy_preferences(&path, "root").unwrap();
        assert_eq!(root["theme"], "dark");
        assert_eq!(root["old-key"], "kept");
        assert_eq!(
            legacy_preferences(&path, "server:s1").unwrap()["theme"],
            "light"
        );
        assert!(legacy_preferences(&path, "other").is_err());
        clear_legacy_preferences(&path, "root", &["theme".into(), "old-key".into()]).unwrap();
        assert!(legacy_preferences(&path, "root").unwrap().is_empty());
        assert_eq!(legacy_preferences(&path, "server:s1").unwrap().len(), 1);
    }
}
