//! Dictation recordings stay in the client database until the user deletes them.
//! The document keeps the web Recording fields; audio is 16-bit mono PCM chunks.
//! The core assigns recording and attempt identities. The page reads WAV slices.
use crate::protocol::{Request, Response};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Map, Value, json};
use std::time::{SystemTime, UNIX_EPOCH};

const UNDO_MS: f64 = 86_400_000.0;
/// Largest decoded audio chunk, in bytes.
const MAX_CHUNK_BYTES: usize = 1024 * 1024;
/// Largest decoded WAV slice that one `dictation.read` returns, in bytes.
const MAX_READ_BYTES: u64 = 512 * 1024;

pub fn initialize(db: &Connection) -> rusqlite::Result<()> {
    db.execute_batch(
        "CREATE TABLE IF NOT EXISTS client_dictation_recordings(id TEXT PRIMARY KEY,document TEXT NOT NULL);
        CREATE TABLE IF NOT EXISTS client_dictation_chunks(id TEXT NOT NULL,position INTEGER NOT NULL,pcm BLOB NOT NULL,PRIMARY KEY(id,position));
        CREATE TABLE IF NOT EXISTS client_dictation_imports(id TEXT PRIMARY KEY);",
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
fn imported(db: &Connection, id: &str) -> Result<bool, String> {
    db.query_row(
        "SELECT EXISTS(SELECT 1 FROM client_dictation_imports WHERE id=?1)",
        [id],
        |r| r.get(0),
    )
    .map_err(|e| e.to_string())
}

fn apply(db: &Connection, request: &Request) -> Result<Value, String> {
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
            for row in expired {
                remove(db, row.get("id").and_then(Value::as_str).unwrap_or(""))?;
            }
            Ok(json!(
                kept.into_iter()
                    .filter(|row| chat(row) == chat_id)
                    .collect::<Vec<_>>()
            ))
        }
        "dictation.create" => {
            let chat_id = identity(p, "chatId", 256)?;
            let rate = p["sampleRate"]
                .as_f64()
                .filter(|r| r.fract() == 0.0 && (8000.0..=192_000.0).contains(r))
                .ok_or("The recording has an invalid sample rate.")?;
            let id = crate::platform::new_identity()?;
            let row: Map<String, Value> = [
                ("id", json!(id)),
                ("chatId", json!(chat_id)),
                ("created", json!(now_ms())),
                ("sampleRate", json!(rate)),
                ("samples", json!(0)),
                ("state", json!("recording")),
            ]
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect();
            put(db, &id, &row)?;
            Ok(Value::Object(row))
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
            db.execute(
                "INSERT OR REPLACE INTO client_dictation_chunks(id,position,pcm) VALUES(?1,?2,?3)",
                params![id, position as i64, pcm],
            )
            .map_err(|e| e.to_string())?;
            put(db, &id, &row)?;
            Ok(Value::Null)
        }
        "dictation.delete" => {
            remove(db, &identity(p, "id", 128)?)?;
            Ok(Value::Null)
        }
        // Late transcription replies must not recreate or change a deleted recording.
        "dictation.update" => {
            let (id, chat_id, change) = document(&p["row"])?;
            let attempt = p["attempt"].as_str();
            if let Some(mut row) = load(db, &id)? {
                let current = row.get("transcriptionAttempt").and_then(Value::as_str);
                if chat(&row) == chat_id && attempt.is_none_or(|a| current == Some(a)) {
                    merge(&mut row, change);
                    put(db, &id, &row)?;
                }
            }
            Ok(Value::Null)
        }
        // A claim returns a new attempt from the core. A retried claim replays its reply.
        "dictation.begin" => {
            let id = identity(p, "id", 128)?;
            let chat_id = identity(p, "chatId", 256)?;
            match load(db, &id)? {
                Some(mut row) if chat(&row) == chat_id => {
                    let attempt = crate::platform::new_identity()?;
                    row.insert(
                        "transcriptionAttempt".into(),
                        Value::String(attempt.clone()),
                    );
                    put(db, &id, &row)?;
                    Ok(json!(attempt))
                }
                _ => Ok(Value::Null),
            }
        }
        // Legacy rows arrive once. A row that already exists or was imported is kept.
        "dictation.import" => {
            let (id, _, row) = document(&p["row"])?;
            if imported(db, &id)? {
                return Ok(json!({"done":true}));
            }
            db.execute(
                "INSERT OR IGNORE INTO client_dictation_recordings(id,document) VALUES(?1,?2)",
                params![id, serde_json::to_string(&row).map_err(|e| e.to_string())?],
            )
            .map_err(|e| e.to_string())?;
            Ok(json!({"done":false}))
        }
        "dictation.imported" => {
            db.execute(
                "INSERT OR IGNORE INTO client_dictation_imports(id) VALUES(?1)",
                [identity(p, "id", 128)?],
            )
            .map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        // Returns a slice of the WAV file that the recording would export.
        "dictation.read" => {
            let id = identity(p, "id", 128)?;
            let offset = p["offset"].as_u64().ok_or("Invalid read offset.")?;
            let length = p["length"]
                .as_u64()
                .filter(|n| *n <= MAX_READ_BYTES)
                .ok_or("The read is too large.")?;
            let row = load(db, &id)?
                .filter(|row| number(row, "deletedAt").is_none())
                .ok_or("The recording was deleted.")?;
            let rate = number(&row, "sampleRate")
                .filter(|r| r.fract() == 0.0 && (8000.0..=192_000.0).contains(r))
                .ok_or("The recording has an invalid sample rate.")? as u32;
            let mut statement = db
                .prepare("SELECT position,length(pcm) FROM client_dictation_chunks WHERE id=?1 ORDER BY position")
                .map_err(|e| e.to_string())?;
            let parts = statement
                .query_map([&id], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?)))
                .map_err(|e| e.to_string())?
                .collect::<Result<Vec<_>, _>>()
                .map_err(|e| e.to_string())?;
            let data = parts.iter().map(|(_, len)| *len as u64).sum::<u64>();
            let total = 44 + data;
            let from = offset.min(total);
            let to = offset.saturating_add(length).min(total);
            let mut out = Vec::with_capacity((to - from) as usize);
            if from < 44 && to > from {
                let header = wav_header((data / 2) as usize, rate);
                out.extend_from_slice(&header[from as usize..to.min(44) as usize]);
            }
            let mut start = 44u64;
            for (position, len) in parts {
                let end = start + len as u64;
                let (lo, hi) = (from.max(start), to.min(end));
                if lo < hi {
                    let bytes: Vec<u8> = db
                        .query_row(
                            "SELECT substr(pcm,?2,?3) FROM client_dictation_chunks WHERE id=?1 AND position=?4",
                            params![id, (lo - start + 1) as i64, (hi - lo) as i64, position],
                            |r| r.get(0),
                        )
                        .map_err(|e| e.to_string())?;
                    out.extend_from_slice(&bytes);
                }
                start = end;
            }
            Ok(json!({"base64": STANDARD.encode(out), "total": total}))
        }
        _ => Err("Unknown dictation command.".into()),
    }
}

pub fn stored(db: &mut Connection, request: &Request) -> Response {
    let result = (|| -> Result<Response, String> {
        let tx = db
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .map_err(|e| e.to_string())?;
        let durable = matches!(
            request.method.as_str(),
            "dictation.create" | "dictation.begin"
        );
        if durable
            && let Some(reply) = crate::store::replay(&tx, request).map_err(|e| e.to_string())?
        {
            return Ok(reply);
        }
        let value = apply(&tx, request)?;
        let response = Response::success(request.id.clone(), value);
        if durable {
            crate::store::record(&tx, request, &response).map_err(|e| e.to_string())?;
        }
        tx.commit().map_err(|e| e.to_string())?;
        Ok(response)
    })();
    result.unwrap_or_else(|e| Response::failure(request.id.clone(), "dictation", e))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::Store;
    use std::sync::atomic::{AtomicU64, Ordering};
    static REQUEST: AtomicU64 = AtomicU64::new(0);
    fn store() -> (tempfile::TempDir, Store) {
        let directory = tempfile::tempdir().unwrap();
        let store = Store::open(&directory.path().join("client.sqlite3")).unwrap();
        (directory, store)
    }
    fn dispatch(store: &mut Store, id: &str, method: &str, params: Value) -> Result<Value, String> {
        let response = store.dispatch(Request {
            id: id.into(),
            method: method.into(),
            params,
        });
        match response.error {
            Some(error) => Err(error.message),
            None => Ok(response.result.unwrap()),
        }
    }
    fn call(store: &mut Store, method: &str, params: Value) -> Result<Value, String> {
        let id = format!("{method}-{}", REQUEST.fetch_add(1, Ordering::Relaxed));
        dispatch(store, &id, method, params)
    }
    fn pcm(values: &[i16]) -> String {
        STANDARD.encode(
            values
                .iter()
                .flat_map(|v| v.to_le_bytes())
                .collect::<Vec<_>>(),
        )
    }
    #[test]
    fn recordings_keep_order_scope_and_late_update_rules() {
        let (_directory, mut store) = store();
        let row = |id: &str, chat: &str, created: u64| json!({"id":id,"chatId":chat,"created":created,"sampleRate":16000,"samples":0,"state":"recording"});
        call(
            &mut store,
            "dictation.save",
            json!({"row":row("a","one",1)}),
        )
        .unwrap();
        call(
            &mut store,
            "dictation.save",
            json!({"row":row("b","one",2)}),
        )
        .unwrap();
        call(
            &mut store,
            "dictation.save",
            json!({"row":row("c","two",3)}),
        )
        .unwrap();
        let list = call(&mut store, "dictation.list", json!({"chatId":"one"})).unwrap();
        assert_eq!(
            list.as_array()
                .unwrap()
                .iter()
                .map(|r| r["id"].as_str().unwrap())
                .collect::<Vec<_>>(),
            ["b", "a"]
        );
        // Another chat cannot claim the recording.
        assert_eq!(
            call(
                &mut store,
                "dictation.begin",
                json!({"id":"a","chatId":"two"})
            )
            .unwrap(),
            Value::Null
        );
        let first = call(
            &mut store,
            "dictation.begin",
            json!({"id":"a","chatId":"one"}),
        )
        .unwrap();
        let second = call(
            &mut store,
            "dictation.begin",
            json!({"id":"a","chatId":"one"}),
        )
        .unwrap();
        assert_ne!(first, second, "a new claim supersedes the old attempt");
        call(
            &mut store,
            "dictation.update",
            json!({"row":{"id":"a","chatId":"one","transcript":"late"},"attempt":first}),
        )
        .unwrap();
        call(&mut store, "dictation.update", json!({"row":{"id":"a","chatId":"one","transcript":"Привет 🦀","error":"old"},"attempt":second})).unwrap();
        call(
            &mut store,
            "dictation.update",
            json!({"row":{"id":"a","chatId":"one","error":null}}),
        )
        .unwrap();
        let a = load(&store.connection, "a").unwrap().unwrap();
        assert_eq!(a["transcript"], "Привет 🦀");
        assert!(!a.contains_key("error"));
        // Trash, undo within a day, and purge after a day.
        call(
            &mut store,
            "dictation.update",
            json!({"row":{"id":"b","chatId":"one","deletedAt":now_ms()}}),
        )
        .unwrap();
        assert_eq!(
            call(&mut store, "dictation.list", json!({"chatId":"one"}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            call(&mut store, "dictation.deleted", json!({"chatId":"one"}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            1
        );
        call(
            &mut store,
            "dictation.update",
            json!({"row":{"id":"b","chatId":"one","deletedAt":null}}),
        )
        .unwrap();
        assert_eq!(
            call(&mut store, "dictation.list", json!({"chatId":"one"}))
                .unwrap()
                .as_array()
                .unwrap()
                .len(),
            2
        );
        call(
            &mut store,
            "dictation.update",
            json!({"row":{"id":"c","chatId":"two","deletedAt":now_ms()-UNDO_MS}}),
        )
        .unwrap();
        assert_eq!(
            call(&mut store, "dictation.deleted", json!({"chatId":"one"})).unwrap(),
            json!([])
        );
        assert!(load(&store.connection, "c").unwrap().is_none());
        // A late update cannot recreate a deleted recording.
        call(&mut store, "dictation.delete", json!({"id":"a"})).unwrap();
        call(
            &mut store,
            "dictation.update",
            json!({"row":{"id":"a","chatId":"one","transcript":"x"}}),
        )
        .unwrap();
        assert!(load(&store.connection, "a").unwrap().is_none());
    }
    #[test]
    fn core_assigns_identities_and_replays_retries() {
        let (_directory, mut store) = store();
        let params = json!({"chatId":"one","sampleRate":16000});
        let created = dispatch(&mut store, "create-1", "dictation.create", params.clone()).unwrap();
        assert_eq!(created["id"].as_str().unwrap().len(), 36);
        assert_eq!(created["samples"], 0);
        assert_eq!(created["state"], "recording");
        // A retried create returns the same recording, and reused identities with other content fail.
        assert_eq!(
            dispatch(&mut store, "create-1", "dictation.create", params.clone()).unwrap(),
            created
        );
        assert!(
            dispatch(
                &mut store,
                "create-1",
                "dictation.create",
                json!({"chatId":"two","sampleRate":16000})
            )
            .is_err()
        );
        assert!(
            call(
                &mut store,
                "dictation.create",
                json!({"chatId":"one","sampleRate":1})
            )
            .is_err()
        );
        let id = created["id"].as_str().unwrap().to_owned();
        let begin = json!({"id":id,"chatId":"one"});
        let attempt = dispatch(&mut store, "begin-1", "dictation.begin", begin.clone()).unwrap();
        assert_eq!(attempt.as_str().unwrap().len(), 36);
        // A retried claim returns the same attempt, so the transcription is not started twice.
        assert_eq!(
            dispatch(&mut store, "begin-1", "dictation.begin", begin).unwrap(),
            attempt
        );
    }
    #[test]
    fn trash_clears_the_attempt_so_a_late_transcript_is_refused() {
        let (_directory, mut store) = store();
        let created = call(
            &mut store,
            "dictation.create",
            json!({"chatId":"one","sampleRate":16000}),
        )
        .unwrap();
        let id = created["id"].as_str().unwrap().to_owned();
        let attempt = call(
            &mut store,
            "dictation.begin",
            json!({"id":id,"chatId":"one"}),
        )
        .unwrap();
        call(&mut store, "dictation.update", json!({"row":{"id":id,"chatId":"one","deletedAt":now_ms(),"transcriptionAttempt":null}})).unwrap();
        call(
            &mut store,
            "dictation.update",
            json!({"row":{"id":id,"chatId":"one","transcript":"late"},"attempt":attempt}),
        )
        .unwrap();
        let row = load(&store.connection, &id).unwrap().unwrap();
        assert!(!row.contains_key("transcript"));
        assert!(!row.contains_key("transcriptionAttempt"));
    }
    #[test]
    fn read_serves_an_exact_wav_across_chunk_boundaries_and_rejects_bad_input() {
        let (_directory, mut store) = store();
        let row = json!({"id":"r","chatId":"c","created":1,"sampleRate":16000,"samples":3,"state":"recording"});
        call(
            &mut store,
            "dictation.chunk",
            json!({"row":row,"index":1,"base64":pcm(&[3])}),
        )
        .unwrap();
        call(
            &mut store,
            "dictation.chunk",
            json!({"row":row,"index":0,"base64":pcm(&[1,-2])}),
        )
        .unwrap();
        for bad in [
            json!({"row":row,"index":2,"base64":"AA=="}),
            json!({"row":row,"index":2,"base64":"!"}),
            json!({"row":{"id":"r"},"index":2,"base64":pcm(&[1])}),
        ] {
            assert!(call(&mut store, "dictation.chunk", bad).is_err());
        }
        let mut expected = wav_header(3, 16000).to_vec();
        expected.extend_from_slice(&[1, 0, 0xfe, 0xff, 3, 0]);
        assert_eq!(
            &expected[..44],
            b"RIFF*\0\0\0WAVEfmt \x10\0\0\0\x01\0\x01\0\x80>\0\0\0}\0\0\x02\0\x10\0data\x06\0\0\0"
        );
        let read = |store: &mut Store, offset: u64, length: u64| -> Vec<u8> {
            let value = call(
                store,
                "dictation.read",
                json!({"id":"r","offset":offset,"length":length}),
            )
            .unwrap();
            assert_eq!(value["total"], 50);
            STANDARD.decode(value["base64"].as_str().unwrap()).unwrap()
        };
        assert_eq!(read(&mut store, 0, 44), expected[..44]);
        assert_eq!(read(&mut store, 40, 10), expected[40..50]);
        assert_eq!(read(&mut store, 46, 100), expected[46..50]);
        assert!(read(&mut store, 50, 1).is_empty());
        assert!(
            call(
                &mut store,
                "dictation.read",
                json!({"id":"r","offset":0,"length":2_000_000})
            )
            .is_err()
        );
        call(&mut store, "dictation.delete", json!({"id":"r"})).unwrap();
        assert!(
            call(
                &mut store,
                "dictation.read",
                json!({"id":"r","offset":0,"length":1})
            )
            .is_err()
        );
        let count: i64 = store
            .connection
            .query_row("SELECT count(*) FROM client_dictation_chunks", [], |r| {
                r.get(0)
            })
            .unwrap();
        assert_eq!(count, 0);
    }
    #[test]
    fn legacy_import_runs_once_and_never_recreates_a_deleted_recording() {
        let (_directory, mut store) = store();
        let row = json!({"id":"legacy-1","chatId":"one","created":1,"sampleRate":16000,"samples":2,"state":"ready"});
        assert_eq!(
            call(&mut store, "dictation.import", json!({"row":row})).unwrap(),
            json!({"done":false})
        );
        call(
            &mut store,
            "dictation.chunk",
            json!({"row":row,"index":0,"base64":pcm(&[5,6])}),
        )
        .unwrap();
        call(&mut store, "dictation.imported", json!({"id":"legacy-1"})).unwrap();
        assert_eq!(
            call(&mut store, "dictation.import", json!({"row":row})).unwrap(),
            json!({"done":true})
        );
        call(&mut store, "dictation.delete", json!({"id":"legacy-1"})).unwrap();
        assert_eq!(
            call(&mut store, "dictation.import", json!({"row":row})).unwrap(),
            json!({"done":true})
        );
        assert_eq!(
            call(&mut store, "dictation.list", json!({"chatId":"one"})).unwrap(),
            json!([])
        );
    }
}
