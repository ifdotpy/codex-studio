//! Dictation recordings stay in the client database until the user deletes them.
//! The document keeps the web Recording fields; audio is 16-bit mono PCM chunks.
use crate::protocol::{Request, Response};
use base64::{Engine, engine::general_purpose::STANDARD};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::{Number, Value, json};
use std::collections::BTreeMap;
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
/// A historical field may be absent or malformed. Malformed individual fields
/// remain opaque so unrelated edits do not erase data from earlier clients.
#[derive(Clone, Debug, Default)]
enum Field<T> {
    #[default]
    Missing,
    Null,
    Known(T),
    Other(Value),
}

impl<T> Field<T> {
    fn is_missing(&self) -> bool {
        matches!(self, Self::Missing)
    }
    fn known(&self) -> Option<&T> {
        if let Self::Known(value) = self {
            Some(value)
        } else {
            None
        }
    }
    fn merge(&mut self, patch: Self) {
        match patch {
            Self::Missing => {}
            Self::Null => *self = Self::Missing,
            value => *self = value,
        }
    }
}
impl<T: Serialize> Serialize for Field<T> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        match self {
            Self::Known(value) => value.serialize(serializer),
            Self::Other(value) => value.serialize(serializer),
            Self::Missing | Self::Null => serializer.serialize_none(),
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for Field<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Present<T> {
            Known(T),
            Other(Value),
        }
        Ok(match Present::deserialize(deserializer)? {
            Present::Known(value) => Self::Known(value),
            Present::Other(Value::Null) => Self::Null,
            Present::Other(value) => Self::Other(value),
        })
    }
}

/// Command input discards malformed fields after applying the existing fallback.
#[derive(Default)]
enum InputField<T> {
    #[default]
    Missing,
    Known(T),
    Invalid,
}
impl<T> InputField<T> {
    fn known(&self) -> Option<&T> {
        if let Self::Known(value) = self {
            Some(value)
        } else {
            None
        }
    }
}
impl<'de, T: Deserialize<'de>> Deserialize<'de> for InputField<T> {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Present<T> {
            Known(T),
            Invalid(serde::de::IgnoredAny),
        }
        Ok(match Present::deserialize(deserializer)? {
            Present::Known(value) => Self::Known(value),
            Present::Invalid(_) => Self::Invalid,
        })
    }
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "lowercase")]
enum RecordingState {
    Recording,
    Ready,
}
impl<'de> Deserialize<'de> for RecordingState {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "recording" => Ok(Self::Recording),
            "ready" => Ok(Self::Ready),
            _ => Err(serde::de::Error::custom("unknown recording state")),
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Recording {
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    chat_id: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    created: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    sample_rate: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    samples: Field<Number>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    state: Field<RecordingState>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    transcript: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    reviewed_text: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    transcription_attempt: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    error: Field<String>,
    #[serde(default, skip_serializing_if = "Field::is_missing")]
    deleted_at: Field<Number>,
    #[serde(flatten)]
    extra: BTreeMap<String, Value>,
}

impl Recording {
    fn merge(&mut self, patch: Self) {
        self.id.merge(patch.id);
        self.chat_id.merge(patch.chat_id);
        self.created.merge(patch.created);
        self.sample_rate.merge(patch.sample_rate);
        self.samples.merge(patch.samples);
        self.state.merge(patch.state);
        self.transcript.merge(patch.transcript);
        self.reviewed_text.merge(patch.reviewed_text);
        self.transcription_attempt
            .merge(patch.transcription_attempt);
        self.error.merge(patch.error);
        self.deleted_at.merge(patch.deleted_at);
        for (key, value) in patch.extra {
            if value.is_null() {
                self.extra.remove(&key);
            } else {
                self.extra.insert(key, value);
            }
        }
    }
    fn chat(&self) -> &str {
        self.chat_id.known().map(String::as_str).unwrap_or("")
    }
    fn created(&self) -> Option<f64> {
        self.created.known().and_then(Number::as_f64)
    }
    fn deleted_at(&self) -> Option<f64> {
        self.deleted_at.known().and_then(Number::as_f64)
    }
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "camelCase")]
struct Params {
    #[serde(default)]
    row: InputField<Recording>,
    #[serde(default)]
    id: InputField<String>,
    #[serde(default)]
    chat_id: InputField<String>,
    #[serde(default)]
    attempt: InputField<String>,
    #[serde(default)]
    index: InputField<Number>,
    #[serde(default)]
    base64: InputField<String>,
    #[serde(default)]
    directory: InputField<String>,
    #[serde(flatten)]
    _extra: BTreeMap<String, serde::de::IgnoredAny>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum WireParams {
    Object(Params),
    Other(serde::de::IgnoredAny),
}

enum Command {
    List {
        chat: String,
    },
    Deleted {
        chat: String,
    },
    Save {
        id: String,
        row: Recording,
    },
    Chunk {
        id: String,
        row: Recording,
        position: u64,
        pcm: Vec<u8>,
    },
    Delete {
        id: String,
    },
    Update {
        id: String,
        chat: String,
        patch: Recording,
        attempt: Option<String>,
    },
    Begin {
        id: String,
        chat: String,
        attempt: String,
    },
    Wav {
        id: String,
        directory: PathBuf,
    },
}

fn identity(value: Option<&String>, key: &str, limit: usize) -> Result<String, String> {
    value
        .filter(|v| !v.is_empty() && v.len() <= limit && !v.contains('\0'))
        .cloned()
        .ok_or_else(|| format!("Invalid recording {key}."))
}
fn document(row: InputField<Recording>) -> Result<(String, String, Recording), String> {
    let InputField::Known(row) = row else {
        return Err("Invalid recording.".into());
    };
    if serde_json::to_vec(&row).map_err(|e| e.to_string())?.len() > MAX_CHUNK_BYTES {
        return Err("The recording details are too large.".into());
    }
    Ok((
        identity(row.id.known(), "id", 128)?,
        identity(row.chat_id.known(), "chatId", 256)?,
        row,
    ))
}

fn command(request: &Request) -> Result<Command, String> {
    let method = request.method.as_str();
    if !matches!(
        method,
        "dictation.list"
            | "dictation.deleted"
            | "dictation.save"
            | "dictation.chunk"
            | "dictation.delete"
            | "dictation.update"
            | "dictation.begin"
            | "dictation.wav"
    ) {
        return Err("Unknown dictation command.".into());
    }
    // Non-object params previously behaved as an empty object at each field lookup.
    let params = match serde_json::from_value::<WireParams>(request.params.clone())
        .map_err(|error| error.to_string())?
    {
        WireParams::Object(params) => params,
        WireParams::Other(_) => Params::default(),
    };
    match method {
        "dictation.list" => Ok(Command::List {
            chat: identity(params.chat_id.known(), "chatId", 256)?,
        }),
        "dictation.deleted" => Ok(Command::Deleted {
            chat: identity(params.chat_id.known(), "chatId", 256)?,
        }),
        "dictation.save" => {
            let (id, _, row) = document(params.row)?;
            Ok(Command::Save { id, row })
        }
        "dictation.chunk" => {
            let (id, _, row) = document(params.row)?;
            let position = params
                .index
                .known()
                .and_then(Number::as_u64)
                .filter(|n| *n < 1_000_000)
                .ok_or("Invalid audio chunk index.")?;
            let encoded = params
                .base64
                .known()
                .filter(|s| s.len() <= MAX_CHUNK_BYTES.div_ceil(3) * 4)
                .ok_or("The audio chunk is too large.")?;
            let pcm = STANDARD
                .decode(encoded)
                .map_err(|_| "Invalid audio bytes.")?;
            if pcm.is_empty() || pcm.len() % 2 != 0 {
                return Err("Audio chunks contain 16-bit samples.".into());
            }
            Ok(Command::Chunk {
                id,
                row,
                position,
                pcm,
            })
        }
        "dictation.delete" => Ok(Command::Delete {
            id: identity(params.id.known(), "id", 128)?,
        }),
        "dictation.update" => {
            let (id, chat, patch) = document(params.row)?;
            Ok(Command::Update {
                id,
                chat,
                patch,
                attempt: params.attempt.known().cloned(),
            })
        }
        "dictation.begin" => {
            let (id, chat, _) = document(params.row)?;
            Ok(Command::Begin {
                id,
                chat,
                attempt: identity(params.attempt.known(), "attempt", 128)?,
            })
        }
        "dictation.wav" => Ok(Command::Wav {
            id: identity(params.id.known(), "id", 128)?,
            directory: PathBuf::from(params.directory.known().ok_or("A directory is required.")?),
        }),
        _ => unreachable!("method was checked above"),
    }
}

fn decode_record(id: &str, raw: &str) -> Result<Recording, String> {
    let record: Recording = serde_json::from_str(raw).map_err(|e| e.to_string())?;
    if record.id.known().map(String::as_str) != Some(id) {
        return Err("Invalid recording identity.".into());
    }
    Ok(record)
}
fn load(db: &Connection, id: &str) -> Result<Option<Recording>, String> {
    let raw: Option<String> = db
        .query_row(
            "SELECT document FROM client_dictation_recordings WHERE id=?1",
            [id],
            |r| r.get(0),
        )
        .optional()
        .map_err(|e| e.to_string())?;
    raw.map(|raw| decode_record(id, &raw)).transpose()
}
fn put(db: &Connection, id: &str, row: &Recording) -> Result<(), String> {
    db.execute("INSERT INTO client_dictation_recordings(id,document) VALUES(?1,?2) ON CONFLICT(id) DO UPDATE SET document=excluded.document", params![id,serde_json::to_string(row).map_err(|e| e.to_string())?]).map(|_| ()).map_err(|e| e.to_string())
}
fn remove(db: &Connection, id: &str) -> Result<(), String> {
    db.execute("DELETE FROM client_dictation_chunks WHERE id=?1", [id])
        .and_then(|_| db.execute("DELETE FROM client_dictation_recordings WHERE id=?1", [id]))
        .map(|_| ())
        .map_err(|e| e.to_string())
}
fn all(db: &Connection) -> Result<Vec<Recording>, String> {
    let mut statement = db
        .prepare("SELECT id,document FROM client_dictation_recordings")
        .map_err(|e| e.to_string())?;
    let rows = statement
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .map_err(|e| e.to_string())?;
    rows.map(|row| {
        let (id, raw) = row.map_err(|e| e.to_string())?;
        decode_record(&id, &raw)
    })
    .collect()
}

fn apply(db: &mut Connection, request: &Request) -> Result<Value, String> {
    match command(request)? {
        Command::List { chat } => {
            let mut rows: Vec<_> = all(db)?
                .into_iter()
                .filter(|row| row.chat() == chat && row.deleted_at().is_none())
                .collect();
            rows.sort_by(|a, b| {
                b.created()
                    .unwrap_or(0.0)
                    .total_cmp(&a.created().unwrap_or(0.0))
            });
            serde_json::to_value(rows).map_err(|e| e.to_string())
        }
        Command::Deleted { chat } => {
            let now = now_ms();
            let (expired, kept): (Vec<_>, Vec<_>) = all(db)?
                .into_iter()
                .filter(|row| row.deleted_at().is_some())
                .partition(|row| now - row.deleted_at().unwrap_or(0.0) >= UNDO_MS);
            let tx = db.transaction().map_err(|e| e.to_string())?;
            for row in expired {
                remove(&tx, row.id.known().map(String::as_str).unwrap_or(""))?;
            }
            tx.commit().map_err(|e| e.to_string())?;
            serde_json::to_value(
                kept.into_iter()
                    .filter(|row| row.chat() == chat)
                    .collect::<Vec<_>>(),
            )
            .map_err(|e| e.to_string())
        }
        Command::Save { id, row } => {
            put(db, &id, &row)?;
            Ok(Value::Null)
        }
        Command::Chunk {
            id,
            row,
            position,
            pcm,
        } => {
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
        Command::Delete { id } => {
            let tx = db.transaction().map_err(|e| e.to_string())?;
            remove(&tx, &id)?;
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        Command::Update {
            id,
            chat,
            patch,
            attempt,
        } => {
            let tx = db.transaction().map_err(|e| e.to_string())?;
            if let Some(mut row) = load(&tx, &id)? {
                let current = row.transcription_attempt.known().map(String::as_str);
                if row.chat() == chat
                    && attempt
                        .as_deref()
                        .is_none_or(|value| current == Some(value))
                {
                    row.merge(patch);
                    put(&tx, &id, &row)?;
                }
            }
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Value::Null)
        }
        Command::Begin { id, chat, attempt } => {
            let tx = db.transaction().map_err(|e| e.to_string())?;
            let claimed = match load(&tx, &id)? {
                Some(mut row) if row.chat() == chat => {
                    row.transcription_attempt = Field::Known(attempt);
                    put(&tx, &id, &row)?;
                    true
                }
                _ => false,
            };
            tx.commit().map_err(|e| e.to_string())?;
            Ok(Value::Bool(claimed))
        }
        Command::Wav { id, directory } => Ok(json!({"path":export(db, &id, &directory)?})),
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
    if row.deleted_at().is_some() {
        return Err("The recording was deleted.".into());
    }
    let rate = row
        .sample_rate
        .known()
        .and_then(Number::as_f64)
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
            drop(std::fs::remove_file(&path));
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
    fn web_recording_producer_fields_match_the_typed_storage_owner() {
        let source = include_str!("../../../apps/web/src/dictation/storage.ts");
        let declaration = source
            .split_once("export type Recording = {")
            .unwrap()
            .1
            .split_once("\n};")
            .unwrap()
            .0;
        let fields = declaration
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (name, ty) = line.split_once(':').unwrap();
                (
                    name.trim_end_matches('?'),
                    name.ends_with('?'),
                    ty.trim().trim_end_matches(';'),
                )
            })
            .collect::<Vec<_>>();
        let expected = [
            ("id", false, "string"),
            ("chatId", false, "string"),
            ("created", false, "number"),
            ("sampleRate", false, "number"),
            ("samples", false, "number"),
            ("state", false, "\"recording\" | \"ready\""),
            ("transcript", true, "string | undefined"),
            ("reviewedText", true, "string | undefined"),
            ("transcriptionAttempt", true, "string | undefined"),
            ("error", true, "string | undefined"),
            ("deletedAt", true, "number | undefined"),
        ];
        assert_eq!(
            fields, expected,
            "web Recording is the producer contract for stored fields"
        );
        let raw = json!({"id":"i","chatId":"c","created":1.0,"sampleRate":16000.0,"samples":1,"state":"ready","transcript":"t","reviewedText":"r","transcriptionAttempt":"a","error":"e","deletedAt":2.0});
        let row: Recording = serde_json::from_value(raw.clone()).unwrap();
        assert!(
            row.id.known().is_some()
                && row.chat_id.known().is_some()
                && row.created.known().is_some()
                && row.sample_rate.known().is_some()
                && row.samples.known().is_some()
                && row.state.known().is_some()
                && row.transcript.known().is_some()
                && row.reviewed_text.known().is_some()
                && row.transcription_attempt.known().is_some()
                && row.error.known().is_some()
                && row.deleted_at.known().is_some()
        );
        assert_eq!(serde_json::to_value(row).unwrap(), raw);
        assert_eq!(
            serde_json::to_value(RecordingState::Recording).unwrap(),
            "recording"
        );
        assert_eq!(
            serde_json::to_value(RecordingState::Ready).unwrap(),
            "ready"
        );
    }
    #[test]
    fn malformed_optional_fields_and_extensions_survive_unrelated_patches() {
        let mut db = database();
        let record = json!({"id":"legacy","chatId":"chat","created":"bad","sampleRate":{"legacy":true},"deletedAt":"bad","state":"future","transcriptionAttempt":7,"unknown":{"keep":[1,2]}});
        call(&mut db, "dictation.save", json!({"row":record})).unwrap();
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"legacy","chatId":"chat","transcript":"ok","error":null}}),
        )
        .unwrap();
        let list = call(&mut db, "dictation.list", json!({"chatId":"chat"})).unwrap();
        let row = &list[0];
        assert_eq!(row["created"], "bad");
        assert_eq!(row["sampleRate"], json!({"legacy":true}));
        assert_eq!(row["deletedAt"], "bad");
        assert_eq!(row["state"], "future");
        assert_eq!(row["transcriptionAttempt"], 7);
        assert_eq!(row["unknown"], json!({"keep":[1,2]}));
        assert_eq!(row["transcript"], "ok");
        assert!(row.get("error").is_none());
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"legacy","chatId":"chat","state":{"ready":null}}}),
        )
        .unwrap();
        let row = call(&mut db, "dictation.list", json!({"chatId":"chat"})).unwrap();
        assert_eq!(row[0]["state"], json!({"ready":null}));
    }

    #[test]
    fn explicit_null_is_preserved_on_save_and_cleared_only_by_patch() {
        let mut db = database();
        call(
            &mut db,
            "dictation.save",
            json!({"row":{"id":"n","chatId":"chat","state":null,"error":null,"future":null}}),
        )
        .unwrap();
        let saved = call(&mut db, "dictation.list", json!({"chatId":"chat"})).unwrap();
        for key in ["state", "error", "future"] {
            assert!(saved[0].get(key).unwrap().is_null());
        }
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"n","chatId":"chat","state":null,"error":null,"future":null}}),
        )
        .unwrap();
        let saved = call(&mut db, "dictation.list", json!({"chatId":"chat"})).unwrap();
        for key in ["state", "error", "future"] {
            assert!(saved[0].get(key).is_none());
        }
    }

    #[test]
    fn command_validation_retains_numeric_aliases_and_rejects_without_writes() {
        let mut db = database();
        let row = json!({"id":"r","chatId":"c","created":1.0,"sampleRate":16000.0,"samples":1.0,"state":"ready","future":"kept"});
        call(
            &mut db,
            "dictation.chunk",
            json!({"row":row,"index":0,"base64":"AAA="}),
        )
        .unwrap();
        let initial: String = db
            .query_row(
                "SELECT document FROM client_dictation_recordings WHERE id='r'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        for bad in [
            json!({"row":row,"index":1.0,"base64":"AAA="}),
            json!({"row":row,"index":1000000,"base64":"AAA="}),
            json!({"row":row,"index":1,"base64":"!"}),
            json!({"row":{"id":"r","chatId":"\u{0}"},"index":1,"base64":"AAA="}),
        ] {
            assert!(call(&mut db, "dictation.chunk", bad).is_err());
        }
        assert_eq!(
            call(&mut db, "dictation.save", json!({"row":[]})).unwrap_err(),
            "Invalid recording."
        );
        assert_eq!(
            call(&mut db, "dictation.list", json!([])).unwrap_err(),
            "Invalid recording chatId."
        );
        assert_eq!(
            call(
                &mut db,
                "dictation.begin",
                json!({"row":{"id":"r","chatId":"c"},"attempt":{}})
            )
            .unwrap_err(),
            "Invalid recording attempt."
        );
        let after: String = db
            .query_row(
                "SELECT document FROM client_dictation_recordings WHERE id='r'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(after, initial);
        assert_eq!(
            db.query_row("SELECT count(*) FROM client_dictation_chunks", [], |r| r
                .get::<_, i64>(0))
                .unwrap(),
            1
        );
        let directory = tempfile::tempdir().unwrap();
        assert!(
            call(
                &mut db,
                "dictation.wav",
                json!({"id":"r","directory":directory.path()})
            )
            .is_ok()
        );
        call(
            &mut db,
            "dictation.update",
            json!({"row":{"id":"r","chatId":"c","future":null,"transcript":null}}),
        )
        .unwrap();
        let saved = serde_json::to_value(load(&db, "r").unwrap().unwrap()).unwrap();
        assert!(saved.get("future").is_none());
        assert!(saved.get("transcript").is_none());
        assert_eq!(saved["sampleRate"], json!(16000.0));
    }
    #[test]
    fn corrupt_record_identity_cannot_delete_another_recording_or_its_audio() {
        let mut db = database();
        call(
            &mut db,
            "dictation.chunk",
            json!({"row":{"id":"victim","chatId":"chat"},"index":0,"base64":"AAA="}),
        )
        .unwrap();
        db.execute(
            "INSERT INTO client_dictation_recordings(id,document) VALUES('poison',?1)",
            [json!({"id":"victim","chatId":"chat","deletedAt":1}).to_string()],
        )
        .unwrap();
        assert!(call(&mut db, "dictation.deleted", json!({"chatId":"chat"})).is_err());
        let records: i64 = db
            .query_row(
                "SELECT count(*) FROM client_dictation_recordings",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let chunks: i64 = db
            .query_row(
                "SELECT count(*) FROM client_dictation_chunks WHERE id='victim'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!((records, chunks), (2, 1));
        assert!(call(&mut db, "dictation.list", json!({"chatId":"chat"})).is_err());
        db.execute(
            "UPDATE client_dictation_recordings SET document=?1 WHERE id='poison'",
            [json!({"chatId":"chat","deletedAt":1}).to_string()],
        )
        .unwrap();
        assert!(call(&mut db, "dictation.deleted", json!({"chatId":"chat"})).is_err());
        assert_eq!(
            db.query_row(
                "SELECT count(*) FROM client_dictation_chunks WHERE id='victim'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
            1
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
        let a = serde_json::to_value(load(&db, "a").unwrap().unwrap()).unwrap();
        assert_eq!(a["transcript"], "Привет 🦀");
        assert!(a.get("error").is_none());
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
