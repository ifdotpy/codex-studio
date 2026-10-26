//! Dictation recordings stay in the client database until the user deletes them.
//! The document keeps the web Recording fields; audio is 16-bit mono PCM chunks.
use crate::protocol::{Request, Response};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::{
    io::Write,
    path::{Path, PathBuf},
    time::{SystemTime, UNIX_EPOCH},
};

const UNDO_MS: f64 = 86_400_000.0;
const MAX_CHUNK_BYTES: usize = 1024 * 1024;
const MAX_SECONDS: u64 = 1801;
/// Maximum saved PCM bytes accepted by the import and transcription owners.
const MAX_IMPORTED_PCM_BYTES: usize = 64 * 1024 * 1024;

pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_dictation_recordings(id TEXT PRIMARY KEY,document TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_dictation_chunks(id TEXT NOT NULL,position INTEGER NOT NULL,pcm BLOB NOT NULL,PRIMARY KEY(id,position));
        CREATE TABLE IF NOT EXISTS client_dictation_attempts(recording TEXT NOT NULL,attempt TEXT NOT NULL,chat_id TEXT NOT NULL,locale TEXT NOT NULL,outcome TEXT NOT NULL,PRIMARY KEY(recording,attempt));
        CREATE INDEX IF NOT EXISTS client_dictation_attempt_identity ON client_dictation_attempts(attempt);",
    )
}

fn now_ms() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as f64)
        .unwrap_or(0.0)
}
fn identity(value: &Value, key: &str, limit: usize) -> Result<String, String> {
    value[key]
        .as_str()
        .filter(|v| !v.is_empty() && v.len() <= limit && !v.contains('\0'))
        .map(str::to_owned)
        .ok_or_else(|| format!("Invalid recording {key}."))
}
fn document(value: &Value) -> Result<(String, String, Map<String, Value>), String> {
    let object = value.as_object().ok_or("Invalid recording.")?;
    if serde_json::to_vec(value).map_err(|e| e.to_string())?.len() > MAX_CHUNK_BYTES {
        return Err("The recording details are too large.".into());
    }
    Ok((
        identity(value, "id", 128)?,
        identity(value, "chatId", 256)?,
        object.clone(),
    ))
}

/// Validate the complete legacy document without changing its identity or state.
pub(crate) fn validate_import(value: &Value, bytes: usize) -> Result<(), String> {
    document(value)?;
    let rate = value["sampleRate"]
        .as_u64()
        .filter(|rate| (8000..=192_000).contains(rate))
        .ok_or("Invalid recording sample rate.")?;
    let samples = value["samples"]
        .as_u64()
        .ok_or("Invalid recording sample count.")?;
    if bytes > MAX_IMPORTED_PCM_BYTES
        || bytes % 2 != 0
        || samples.checked_mul(2) != Some(bytes as u64)
        || samples > MAX_SECONDS * rate
        || !matches!(value["state"].as_str(), Some("recording" | "ready"))
    {
        return Err("The recording bytes do not match the saved samples.".into());
    }
    Ok(())
}

/// Called only inside the byte import owner's transaction, never by a renderer command.
pub(crate) fn import_recording(db: &Connection, value: &Value, bytes: &[u8]) -> Result<(), String> {
    validate_import(value, bytes.len())?;
    let (id, _, row) = document(value)?;
    if load(db, &id)?.is_some() {
        return Err("The destination already owns this recording ID.".into());
    }
    put(db, &id, &row)?;
    if let Some(attempt) = row
        .get("transcriptionAttempt")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty() && s.len() <= 128)
    {
        db.execute(
            "INSERT INTO client_dictation_attempts VALUES(?1,?2,?3,'',?4)",
            params![
                id,
                attempt,
                chat(&row),
                json!({"transcriptionState":"unknown"}).to_string()
            ],
        )
        .map_err(|e| e.to_string())?;
    }
    for (index, chunk) in bytes.chunks(MAX_CHUNK_BYTES).enumerate() {
        db.execute(
            "INSERT INTO client_dictation_chunks VALUES(?1,?2,?3)",
            params![id, index as i64, chunk],
        )
        .map_err(|e| e.to_string())?;
    }
    Ok(())
}

pub(crate) fn read_import(db: &Connection, id: &str) -> Result<(Value, Vec<u8>), String> {
    let row = load(db, id)?.ok_or("The imported recording is missing.")?;
    let mut query = db
        .prepare("SELECT pcm FROM client_dictation_chunks WHERE id=?1 ORDER BY position")
        .map_err(|e| e.to_string())?;
    let chunks = query
        .query_map([id], |r| r.get::<_, Vec<u8>>(0))
        .map_err(|e| e.to_string())?;
    let mut bytes = Vec::new();
    for chunk in chunks {
        let chunk = chunk.map_err(|e| e.to_string())?;
        if bytes.len() + chunk.len() > 64 * 1024 * 1024 {
            return Err("The imported recording is too large.".into());
        }
        bytes.extend(chunk);
    }
    Ok((Value::Object(row), bytes))
}
fn load(db: &Connection, id: &str) -> Result<Option<Map<String, Value>>, String> {
    let raw: Option<String> = db
        .query_row(
            "SELECT document FROM client_dictation_recordings WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    raw.map(|raw| serde_json::from_str::<Map<String, Value>>(&raw).map_err(|e| e.to_string()))
        .transpose()
}
fn put(db: &Connection, id: &str, row: &Map<String, Value>) -> Result<(), String> {
    db.execute(
        "INSERT INTO client_dictation_recordings(id,document) VALUES(?1,?2)
         ON CONFLICT(id) DO UPDATE SET document=excluded.document",
        params![id, serde_json::to_string(row).map_err(|e| e.to_string())?],
    )
    .map(|_| ())
    .map_err(|e| e.to_string())
}
fn remove(db: &Connection, id: &str) -> Result<(), String> {
    db.execute("DELETE FROM client_dictation_chunks WHERE id=?1", [id])
        .and_then(|_| db.execute("DELETE FROM client_dictation_recordings WHERE id=?1", [id]))
        .map(|_| ())
        .map_err(|e| e.to_string())
}
fn all(db: &Connection) -> Result<Vec<Map<String, Value>>, String> {
    let mut statement = db
        .prepare("SELECT document FROM client_dictation_recordings")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |r| r.get::<_, String>(0))
        .map_err(|e| e.to_string())?;
    let mut result = vec![];
    for raw in rows {
        result.push(
            serde_json::from_str(&raw.map_err(|e| e.to_string())?).map_err(|e| e.to_string())?,
        );
    }
    Ok(result)
}
fn number(row: &Map<String, Value>, key: &str) -> Option<f64> {
    row.get(key).and_then(Value::as_f64)
}
fn chat(row: &Map<String, Value>) -> &str {
    row.get("chatId").and_then(Value::as_str).unwrap_or("")
}
// JSON has no undefined. A null field removes it, as the web merge does.
fn merge(target: &mut Map<String, Value>, change: Map<String, Value>) {
    for (key, value) in change {
        if value.is_null() {
            target.remove(&key);
        } else {
            target.insert(key, value);
        }
    }
}

fn apply(db: &mut Connection, request: &Request) -> Result<Value, String> {
    let p = &request.params;
    match request.method.as_str() {
        "dictation.get" => {
            let id = identity(p, "id", 128)?;
            let chat_id = identity(p, "chatId", 256)?;
            let row = load(db, &id)?
                .filter(|row| chat(row) == chat_id)
                .ok_or("The recording is unavailable in this chat.")?;
            Ok(Value::Object(row))
        }
        "dictation.captureSnapshot" => {
            let id = identity(p, "id", 128)?;
            Ok(load(db, &id)?.map(Value::Object).unwrap_or(Value::Null))
        }
        "dictation.capturePrepare" => {
            if crate::uploads::import_pending(db)? {
                return Err("Finish the byte import before recording.".into());
            }
            let chat_id = identity(p, "chatId", 256)?;
            let tx = db.transaction().map_err(|e| e.to_string())?;
            if let Some(previous) = crate::store::replay(&tx, request).map_err(|e| e.to_string())? {
                let previous = previous
                    .result
                    .ok_or("The recording request has no confirmed result.")?;
                return Ok(json!({"row":previous["row"],"replayed":true}));
            }
            let id = crate::file_save::uuid()?;
            let row = json!({"id":id,"chatId":chat_id,"captureRequestId":request.id,"created":now_ms(),"sampleRate":crate::media::DICTATION_RATE,"samples":0,"state":"recording"});
            put(&tx, &id, row.as_object().expect("recording object"))?;
            crate::store::record(
                &tx,
                request,
                &Response::success(request.id.clone(), json!({"row":row})),
            )
            .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(json!({"row":row,"replayed":false}))
        }
        "dictation.captureAppend" => {
            let id = identity(p, "id", 128)?;
            let mut row = load(db, &id)?
                .filter(|row| row.get("state").and_then(Value::as_str) == Some("recording"))
                .ok_or("The recording ended.")?;
            let encoded = p["base64"]
                .as_str()
                .filter(|value| value.len() <= MAX_CHUNK_BYTES.div_ceil(3) * 4)
                .ok_or("The audio chunk is too large.")?;
            let pcm = STANDARD
                .decode(encoded)
                .map_err(|_| "Invalid audio bytes.")?;
            if pcm.is_empty() || pcm.len() % 2 != 0 {
                return Err("Audio chunks contain 16-bit samples.".into());
            }
            let samples = row
                .get("samples")
                .and_then(Value::as_u64)
                .ok_or("Invalid saved sample count.")?;
            let next = samples + pcm.len() as u64 / 2;
            if next > MAX_SECONDS * crate::media::DICTATION_RATE as u64 {
                return Err("The 30 minute recording limit was reached.".into());
            }
            let tx = db.transaction().map_err(|e| e.to_string())?;
            let position: i64 = tx
                .query_row(
                    "SELECT COALESCE(MAX(position)+1,0) FROM client_dictation_chunks WHERE id=?1",
                    [&id],
                    |r| r.get(0),
                )
                .map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT INTO client_dictation_chunks VALUES(?1,?2,?3)",
                params![id, position, pcm],
            )
            .map_err(|e| e.to_string())?;
            row.insert("samples".into(), json!(next));
            put(&tx, &id, &row)?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Value::Object(row))
        }
        "dictation.captureFinish" => {
            let id = identity(p, "id", 128)?;
            let mut row = load(db, &id)?.ok_or("The recording is unavailable.")?;
            row.insert(
                "state".into(),
                json!(if p["uncertain"] == true {
                    "recording"
                } else {
                    "ready"
                }),
            );
            if let Some(error) = p["error"].as_str() {
                row.insert("error".into(), json!(error));
            }
            put(db, &id, &row)?;
            Ok(Value::Object(row))
        }
        "dictation.trash" | "dictation.restore" => {
            let id = identity(p, "id", 128)?;
            let chat_id = identity(p, "chatId", 256)?;
            let mut row = load(db, &id)?
                .filter(|row| chat(row) == chat_id)
                .ok_or("The recording is unavailable in this chat.")?;
            if request.method == "dictation.trash" {
                row.insert("deletedAt".into(), json!(now_ms()));
                row.insert(
                    "transcriptionAttempt".into(),
                    json!(crate::file_save::uuid()?),
                );
            } else {
                row.remove("deletedAt");
            }
            put(db, &id, &row)?;
            Ok(Value::Object(row))
        }
        "dictation.claimTranscription" => {
            let id = identity(p, "id", 128)?;
            let chat_id = identity(p, "chatId", 256)?;
            let attempt = identity(p, "attempt", 128)?;
            let locale = p["locale"]
                .as_str()
                .filter(|s| crate::speech::valid_locale(s))
                .ok_or("Invalid speech language.")?;
            let tx = db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| e.to_string())?;
            let mut row = load(&tx, &id)?
                .filter(|row| chat(row) == chat_id && !row.contains_key("deletedAt"))
                .ok_or("The recording was deleted or belongs to another chat.")?;
            let other:bool=tx.query_row("SELECT EXISTS(SELECT 1 FROM client_dictation_attempts WHERE attempt=?1 AND recording<>?2)",params![attempt,id],|r|r.get(0)).map_err(|e|e.to_string())?;
            if other {
                return Err("The transcription attempt belongs to another recording.".into());
            }
            let previous:Option<(String,String,String)>=tx.query_row("SELECT chat_id,locale,outcome FROM client_dictation_attempts WHERE recording=?1 AND attempt=?2",params![id,attempt],|r|Ok((r.get(0)?,r.get(1)?,r.get(2)?))).optional().map_err(|e|e.to_string())?;
            if let Some((owner, language, outcome)) = previous {
                if owner != chat_id || (!language.is_empty() && language != locale) {
                    return Err("The transcription attempt has different content.".into());
                }
                return Ok(
                    json!({"claimed":false,"row":serde_json::from_str::<Value>(&outcome).map_err(|e|e.to_string())?}),
                );
            }
            let legacy = row.get("transcriptionAttempt").and_then(Value::as_str) == Some(&attempt);
            let outcome = json!({"transcriptionState":if legacy{"unknown"}else{"running"}});
            tx.execute(
                "INSERT INTO client_dictation_attempts VALUES(?1,?2,?3,?4,?5)",
                params![id, attempt, chat_id, locale, outcome.to_string()],
            )
            .map_err(|e| e.to_string())?;
            if legacy {
                tx.commit().map_err(|e| e.to_string())?;
                return Ok(json!({"claimed":false,"row":outcome}));
            }
            row.insert("transcriptionAttempt".into(), json!(attempt));
            row.insert("transcriptionState".into(), json!("running"));
            put(&tx, &id, &row)?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(json!({"claimed":true,"row":row}))
        }
        "dictation.settleTranscription" => {
            let id = identity(p, "id", 128)?;
            let attempt = identity(p, "attempt", 128)?;
            let tx = db
                .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
                .map_err(|e| e.to_string())?;
            let outcome = if let Some(text) = p["text"].as_str() {
                json!({"transcriptionState":"completed","transcript":text})
            } else {
                json!({"transcriptionState":"failed","error":p["error"]})
            };
            let previous:String=tx.query_row("SELECT outcome FROM client_dictation_attempts WHERE recording=?1 AND attempt=?2",params![id,attempt],|r|r.get(0)).map_err(|_|"The transcription attempt was not claimed.")?;
            let previous: Value = serde_json::from_str(&previous).map_err(|e| e.to_string())?;
            if matches!(
                previous["transcriptionState"].as_str(),
                Some("completed" | "failed")
            ) && previous != outcome
            {
                return Err("The transcription receipt changed.".into());
            }
            tx.execute(
                "UPDATE client_dictation_attempts SET outcome=?3 WHERE recording=?1 AND attempt=?2",
                params![id, attempt, outcome.to_string()],
            )
            .map_err(|e| e.to_string())?;
            let Some(mut row) = load(&tx, &id)?.filter(|row| {
                !row.contains_key("deletedAt")
                    && row.get("transcriptionAttempt").and_then(Value::as_str) == Some(&attempt)
            }) else {
                tx.commit().map_err(|e| e.to_string())?;
                return Ok(Value::Null);
            };
            if let Some(text) = p["text"].as_str() {
                row.insert("transcript".into(), json!(text));
                row.insert("transcriptionState".into(), json!("completed"));
                row.remove("error");
                row.remove("reviewedText");
            } else {
                row.insert("transcriptionState".into(), json!("failed"));
                row.insert("error".into(), p["error"].clone());
            }
            put(&tx, &id, &row)?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Value::Object(row))
        }
        "dictation.list" => {
            let chat_id = identity(p, "chatId", 256)?;
            let mut rows: Vec<_> = all(db)?
                .into_iter()
                .filter(|row| chat(row) == chat_id && number(row, "deletedAt").is_none())
                .collect();
            rows.sort_by(|a, b| {
                number(b, "created")
                    .unwrap_or(0.0)
                    .total_cmp(&number(a, "created").unwrap_or(0.0))
            });
            Ok(json!(rows))
        }
        "dictation.deleted" => {
            let chat_id = identity(p, "chatId", 256)?;
            let now = now_ms();
            let (expired, kept): (Vec<_>, Vec<_>) = all(db)?
                .into_iter()
                .filter(|row| number(row, "deletedAt").is_some())
                .partition(|row| now - number(row, "deletedAt").unwrap_or(0.0) >= UNDO_MS);
            let tx = db.transaction().map_err(|e| e.to_string())?;
            for row in expired {
                remove(&tx, row.get("id").and_then(Value::as_str).unwrap_or(""))?;
            }
            tx.commit().map_err(|e| e.to_string())?;
            Ok(json!(
                kept.into_iter()
                    .filter(|row| chat(row) == chat_id)
                    .collect::<Vec<_>>()
            ))
        }
        "dictation.save" => {
            let (id, _, row) = document(&p["row"])?;
            put(db, &id, &row)?;
            Ok(Value::Null)
        }
        "dictation.chunk" => {
            let (id, _, row) = document(&p["row"])?;
            let position = p["index"]
                .as_u64()
                .filter(|n| *n < 1_000_000)
                .ok_or("Invalid audio chunk index.")?;
            let encoded = p["base64"]
                .as_str()
                .filter(|s| s.len() <= MAX_CHUNK_BYTES.div_ceil(3) * 4)
                .ok_or("The audio chunk is too large.")?;
            let pcm = STANDARD
                .decode(encoded)
                .map_err(|_| "Invalid audio bytes.")?;
            if pcm.is_empty() || pcm.len() % 2 != 0 {
                return Err("Audio chunks contain 16-bit samples.".into());
            }
            let tx = db.transaction().map_err(|e| e.to_string())?;
            tx.execute(
                "INSERT OR REPLACE INTO client_dictation_chunks(id,position,pcm) VALUES(?1,?2,?3)",
                params![id, position as i64, pcm],
            )
            .map_err(|e| e.to_string())?;
            put(&tx, &id, &row)?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "dictation.delete" => {
            let id = identity(p, "id", 128)?;
            let tx = db.transaction().map_err(|e| e.to_string())?;
            remove(&tx, &id)?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        // Late transcription replies must not recreate or change a deleted recording.
        "dictation.update" => {
            let (id, chat_id, change) = document(&p["row"])?;
            let attempt = p["attempt"].as_str();
            let tx = db.transaction().map_err(|e| e.to_string())?;
            if let Some(mut row) = load(&tx, &id)? {
                let current = row.get("transcriptionAttempt").and_then(Value::as_str);
                if chat(&row) == chat_id && attempt.is_none_or(|a| current == Some(a)) {
                    merge(&mut row, change);
                    put(&tx, &id, &row)?;
                }
            }
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        "dictation.begin" => {
            let (id, chat_id, _) = document(&p["row"])?;
            let attempt = identity(p, "attempt", 128)?;
            let tx = db.transaction().map_err(|e| e.to_string())?;
            let claimed = match load(&tx, &id)? {
                Some(mut row) if chat(&row) == chat_id => {
                    row.insert("transcriptionAttempt".into(), Value::String(attempt));
                    put(&tx, &id, &row)?;
                    true
                }
                _ => false,
            };
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Value::Bool(claimed))
        }
        "dictation.wav" => {
            let id = identity(p, "id", 128)?;
            let directory =
                PathBuf::from(p["directory"].as_str().ok_or("A directory is required.")?);
            Ok(json!({"path": export(db, &id, &directory)?}))
        }
        _ => Err("Unknown dictation command.".into()),
    }
}

pub fn stored(db: &mut Connection, request: &Request) -> Response {
    match apply(db, request) {
        Ok(value) => Response::success(request.id.clone(), value),
        Err(error) => Response::failure(request.id.clone(), "dictation", error),
    }
}

pub fn wav_header(samples: usize, sample_rate: u32) -> [u8; 44] {
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

/// Writes the recording as a private WAV file in an existing private directory.
fn export(db: &Connection, id: &str, directory: &Path) -> Result<String, String> {
    use std::os::unix::fs::OpenOptionsExt;
    let row = load(db, id)?.ok_or("The recording was deleted.")?;
    if number(&row, "deletedAt").is_some() {
        return Err("The recording was deleted.".into());
    }
    let rate = number(&row, "sampleRate")
        .filter(|r| r.fract() == 0.0 && (8000.0..=192_000.0).contains(r))
        .ok_or("The recording has an invalid sample rate.")? as u32;
    let mut statement = db
        .prepare("SELECT pcm FROM client_dictation_chunks WHERE id=?1 ORDER BY position")
        .map_err(|e| e.to_string())?;
    let chunks = statement
        .query_map([id], |r| r.get::<_, Vec<u8>>(0))
        .map_err(|e| e.to_string())?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|e| e.to_string())?;
    let samples = chunks.iter().map(|c| c.len() / 2).sum::<usize>();
    if samples == 0 {
        return Err("The recording has no audio.".into());
    }
    if samples as u64 > MAX_SECONDS * rate as u64 {
        return Err("The recording exceeds 30 minutes.".into());
    }
    let path = directory.join(format!("{id}.wav").replace('/', "_"));
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)
        .map_err(|e| e.to_string())?;
    file.write_all(&wav_header(samples, rate))
        .and_then(|_| chunks.iter().try_for_each(|c| file.write_all(c)))
        .and_then(|_| file.sync_all())
        .map_err(|e| {
            let _ = std::fs::remove_file(&path);
            e.to_string()
        })?;
    Ok(path.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    fn call(db: &mut Connection, method: &str, params: Value) -> Result<Value, String> {
        let response = stored(
            db,
            &Request {
                id: "t".into(),
                method: method.into(),
                params,
            },
        );
        match response.error {
            Some(error) => Err(error.message),
            None => Ok(response.result.unwrap()),
        }
    }
    fn database() -> Connection {
        let db = Connection::open_in_memory().unwrap();
        initialize(&db).unwrap();
        db
    }

    #[test]
    fn native_transcription_does_not_reclaim_an_old_attempt_after_a_new_attempt() {
        let mut db = database();
        call(&mut db,"dictation.save",json!({"row":{"id":"recording","chatId":"chat","created":1,"sampleRate":16000,"samples":0,"state":"ready"}})).unwrap();
        let claim = |attempt: &str| json!({"id":"recording","chatId":"chat","attempt":attempt,"locale":"en-US"});
        assert_eq!(
            call(&mut db, "dictation.claimTranscription", claim("original")).unwrap()["claimed"],
            true
        );
        assert_eq!(
            call(
                &mut db,
                "dictation.claimTranscription",
                claim("new-attempt")
            )
            .unwrap()["claimed"],
            true
        );
        assert_eq!(
            call(&mut db, "dictation.claimTranscription", claim("original")).unwrap()["claimed"],
            false
        );
        assert_eq!(
            load(&db, "recording").unwrap().unwrap()["transcriptionAttempt"],
            "new-attempt"
        );
    }
    #[test]
    fn recordings_keep_order_scope_undo_and_late_attempt_rules() {
        let mut db = database();
        let row = |id: &str, chat: &str, created: u64| json!({"id":id,"chatId":chat,"created":created,"sampleRate":16000,"samples":0,"state":"recording"});
        call(&mut db, "dictation.save", json!({"row":row("a","one",1)})).unwrap();
        call(&mut db, "dictation.save", json!({"row":row("b","one",2)})).unwrap();
        call(&mut db, "dictation.save", json!({"row":row("c","two",3)})).unwrap();
        let list = call(&mut db, "dictation.list", json!({"chatId":"one"})).unwrap();
        assert_eq!(
            list.as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        // Another chat cannot claim or change the recording.
        assert_eq!(
            call(
                &mut db,
                "dictation.begin",
                json!({"row":row("a","two",1),"attempt":"x"})
            )
            .unwrap(),
            false
        );
        assert_eq!(
            call(
                &mut db,
                "dictation.begin",
                json!({"row":row("a","one",1),"attempt":"first"})
            )
            .unwrap(),
            true
        );
        call(
            &mut db,
            "dictation.begin",
            json!({"row":row("a","one",1),"attempt":"second"}),
        )
        .unwrap();
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"a","chatId":"one","transcript":"late"},"attempt":"first"}),
        )
        .unwrap();
        call(&mut db, "dictation.update", json!({"row":{"id":"a","chatId":"one","transcript":"Привет 🦀","error":"old"},"attempt":"second"})).unwrap();
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"a","chatId":"one","error":null}}),
        )
        .unwrap();
        let a = load(&db, "a").unwrap().unwrap();
        assert_eq!(a["transcript"], "Привет 🦀");
        assert!(!a.contains_key("error"));
        // Trash, undo within a day, and purge after a day.
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"b","chatId":"one","deletedAt":now_ms()}}),
        )
        .unwrap();
        assert_eq!(
            call(&mut db, "dictation.list", json!({"chatId":"one"}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            call(&mut db, "dictation.deleted", json!({"chatId":"one"}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"b","chatId":"one","deletedAt":null}}),
        )
        .unwrap();
        assert_eq!(
            call(&mut db, "dictation.list", json!({"chatId":"one"}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"c","chatId":"two","deletedAt":now_ms()-UNDO_MS}}),
        )
        .unwrap();
        assert_eq!(
            call(&mut db, "dictation.deleted", json!({"chatId":"one"})).unwrap(),
            json!([])
        );
        assert!(load(&db, "c").unwrap().is_none());
        // A late update cannot recreate a deleted recording.
        call(&mut db, "dictation.delete", json!({"id":"a"})).unwrap();
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"a","chatId":"one","transcript":"x"}}),
        )
        .unwrap();
        assert!(load(&db, "a").unwrap().is_none());
    }
    #[test]
    fn audio_chunks_export_an_exact_wav_and_reject_bad_input() {
        let mut db = database();
        let directory = tempfile::tempdir().unwrap();
        let row = json!({"id":"r","chatId":"c","created":1,"sampleRate":16000,"samples":3,"state":"recording"});
        let pcm = |values: &[i16]| {
            STANDARD.encode(
                values
                    .iter()
                    .flat_map(|v| v.to_le_bytes())
                    .collect::<Vec<_>>(),
            )
        };
        call(
            &mut db,
            "dictation.chunk",
            json!({"row":row,"index":1,"base64":pcm(&[3])}),
        )
        .unwrap();
        call(
            &mut db,
            "dictation.chunk",
            json!({"row":row,"index":0,"base64":pcm(&[1,-2])}),
        )
        .unwrap();
        for bad in [
            json!({"row":row,"index":2,"base64":"AA=="}),
            json!({"row":row,"index":2,"base64":"!"}),
            json!({"row":{"id":"r"},"index":2,"base64":pcm(&[1])}),
        ] {
            assert!(call(&mut db, "dictation.chunk", bad).is_err());
        }
        let path = call(
            &mut db,
            "dictation.wav",
            json!({"id":"r","directory":directory.path()}),
        )
        .unwrap()["path"]
            .as_str()
            .unwrap()
            .to_owned();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            &bytes[..44],
            b"RIFF*\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80>\0\0\0}\0\0\x02\0\x10\0data\x06\0\0\0"
        );
        assert_eq!(&bytes[44..], &[1, 0, 0xfe, 0xff, 3, 0]);
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        assert!(
            call(
                &mut db,
                "dictation.wav",
                json!({"id":"r","directory":directory.path()})
            )
            .is_err(),
            "no overwrite"
        );
        call(&mut db, "dictation.delete", json!({"id":"r"})).unwrap();
        assert!(
            call(
                &mut db,
                "dictation.wav",
                json!({"id":"r","directory":directory.path().join("x")})
            )
            .is_err()
        );
        let count: i64 = db
            .query_row("SELECT count(*) FROM client_dictation_chunks", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
}
