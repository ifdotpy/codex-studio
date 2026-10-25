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

pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_dictation_recordings(id TEXT PRIMARY KEY,document TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_dictation_chunks(id TEXT NOT NULL,position INTEGER NOT NULL,pcm BLOB NOT NULL,PRIMARY KEY(id,position));",
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
