//! A native gesture starts a bounded transfer. Only Save As chooses a path.
//! A transfer can be reserved at the gesture and declared once the UI has the data.
use base64::Engine;
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Mutex, OnceLock},
    time::{Duration, Instant},
};

pub const LIMIT: usize = 64 * 1024 * 1024;
pub const CHUNK: usize = 256 * 1024;
const LIFETIME: Duration = Duration::from_secs(120);
struct Transfer {
    state: TransferState,
    expires: Instant,
}

enum TransferState {
    Reserved,
    Declared {
        name: String,
        size: usize,
        bytes: Vec<u8>,
    },
}

struct Declaration {
    name: String,
    size: usize,
}

struct Chunk {
    offset: usize,
    bytes: Vec<u8>,
}

// Keep validation results until execution so missing or expired transfers and
// reserved state retain their existing error precedence over payload errors.
enum Command {
    Begin(Result<Option<Declaration>, String>),
    Declare {
        id: Result<String, String>,
        declaration: Result<Declaration, String>,
    },
    Chunk {
        id: Result<String, String>,
        chunk: Result<Chunk, String>,
    },
    Cancel(Result<String, String>),
}

impl Command {
    fn parse(method: &str, value: &Value) -> Result<Self, String> {
        let id = || transfer_id(value, "A save transfer is required.");
        match method {
            "desktop.saveBegin" => Ok(Self::Begin(begin_declaration(value))),
            "desktop.saveDeclare" => Ok(Self::Declare {
                id: id(),
                declaration: declaration(value),
            }),
            "desktop.saveChunk" => Ok(Self::Chunk {
                id: id(),
                chunk: parse_chunk(value),
            }),
            "desktop.saveCancel" => Ok(Self::Cancel(id())),
            _ => Err("Unknown file transfer command.".into()),
        }
    }
}

fn transfer_id(value: &Value, error: &str) -> Result<String, String> {
    value["transfer"]
        .as_str()
        .map(str::to_owned)
        .ok_or_else(|| error.into())
}

fn begin_declaration(value: &Value) -> Result<Option<Declaration>, String> {
    if value.get("name").is_none() && value.get("size").is_none() {
        Ok(None)
    } else {
        declaration(value).map(Some)
    }
}
#[derive(Default)]
pub(crate) struct Transfers {
    items: HashMap<String, Transfer>,
}
fn transfers() -> &'static Mutex<Transfers> {
    static POOL: OnceLock<Mutex<Transfers>> = OnceLock::new();
    POOL.get_or_init(Mutex::default)
}
pub(crate) fn uuid() -> Result<String, String> {
    crate::platform::dispatch(&crate::protocol::Request {
        id: "file-save-id".into(),
        method: "system.uuid".into(),
        params: Value::Null,
    })
    .result
    .and_then(|v| v.as_str().map(str::to_owned))
    .ok_or("Cannot create a save identity.".into())
}
fn declaration(value: &Value) -> Result<Declaration, String> {
    let size = value["size"]
        .as_u64()
        .filter(|n| *n <= LIMIT as u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or("Save accepts at most 64 MiB of file data.")?;
    Ok(Declaration {
        name: name(&value["name"])?,
        size,
    })
}
fn name(value: &Value) -> Result<String, String> {
    let raw = value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 1024)
        .ok_or("Invalid file name.")?;
    let normalized = raw.replace('\\', "/");
    // str::rsplit always yields at least one segment, including for an empty input.
    let name = normalized
        .rsplit('/')
        .next()
        .expect("split has a final segment");
    if name.is_empty() || matches!(name, "." | "..") || name.chars().any(|c| c <= '\u{1f}') {
        return Err("Invalid file name.".into());
    }
    Ok(name.into())
}
impl Transfers {
    fn execute(&mut self, command: Command, now: Instant) -> Result<Value, String> {
        match command {
            Command::Begin(declared) => self.begin_typed(declared, now),
            Command::Declare { id, declaration } => self.declare_typed(id, declaration, now),
            Command::Chunk { id, chunk } => self.chunk_typed(id, chunk, now),
            Command::Cancel(id) => self.cancel_id(id?),
        }
    }
    pub(crate) fn cancel(&mut self, value: &Value) -> Result<Value, String> {
        self.cancel_id(transfer_id(value, "A transfer is required.")?)
    }

    fn cancel_id(&mut self, id: String) -> Result<Value, String> {
        self.items.remove(&id);
        Ok(Value::Null)
    }

    fn expire(&mut self, now: Instant) {
        self.items.retain(|_, v| v.expires > now);
    }
    pub(crate) fn begin(&mut self, value: &Value, now: Instant) -> Result<Value, String> {
        self.begin_typed(begin_declaration(value), now)
    }
    fn begin_typed(
        &mut self,
        declared: Result<Option<Declaration>, String>,
        now: Instant,
    ) -> Result<Value, String> {
        self.expire(now);
        let declared = declared?;
        if self.items.len() >= 2 {
            return Err("Finish the current file saves first.".into());
        }
        let id = uuid()?;
        let state = match declared {
            None => TransferState::Reserved,
            Some(Declaration { name, size }) => TransferState::Declared {
                name,
                size,
                bytes: Vec::new(),
            },
        };
        self.items.insert(
            id.clone(),
            Transfer {
                state,
                expires: now + LIFETIME,
            },
        );
        Ok(json!({"transfer":id,"chunkBytes":CHUNK}))
    }
    fn declare_typed(
        &mut self,
        id: Result<String, String>,
        declaration: Result<Declaration, String>,
        now: Instant,
    ) -> Result<Value, String> {
        self.expire(now);
        let transfer = self.get(&id?)?;
        transfer.declare(declaration?)?;
        Ok(json!({"chunkBytes":CHUNK}))
    }
    fn get(&mut self, id: &str) -> Result<&mut Transfer, String> {
        self.items
            .get_mut(id)
            .ok_or("The save transfer expired or is no longer available.".into())
    }
    pub(crate) fn chunk(&mut self, value: &Value, now: Instant) -> Result<Value, String> {
        self.chunk_typed(
            transfer_id(value, "A save transfer is required."),
            parse_chunk(value),
            now,
        )
    }
    fn chunk_typed(
        &mut self,
        id: Result<String, String>,
        chunk: Result<Chunk, String>,
        now: Instant,
    ) -> Result<Value, String> {
        self.expire(now);
        let transfer = self.get(&id?)?;
        if matches!(transfer.state, TransferState::Reserved) {
            return Err("The save transfer has no file yet.".into());
        }
        Ok(json!({"received":transfer.chunk(chunk?)?}))
    }
    pub(crate) fn take(
        &mut self,
        value: &Value,
        now: Instant,
    ) -> Result<(String, Vec<u8>), String> {
        self.take_id(transfer_id(value, "A save transfer is required."), now)
    }
    fn take_id(
        &mut self,
        id: Result<String, String>,
        now: Instant,
    ) -> Result<(String, Vec<u8>), String> {
        self.expire(now);
        let id = id?;
        let transfer = self
            .items
            .get(&id)
            .ok_or("The save transfer expired or is no longer available.")?;
        if !matches!(&transfer.state, TransferState::Declared { size, bytes, .. } if bytes.len() == *size)
        {
            return Err("The file transfer is incomplete.".into());
        }
        // The get above found this entry, and no map mutation occurred afterward.
        let transfer = self
            .items
            .remove(&id)
            .expect("validated save transfer exists");
        match transfer.state {
            TransferState::Declared { name, bytes, .. } => Ok((name, bytes)),
            // The completeness check above proves the state is declared.
            TransferState::Reserved => unreachable!("validated declared transfer"),
        }
    }
}

impl Transfer {
    fn declare(&mut self, Declaration { name, size }: Declaration) -> Result<(), String> {
        match &self.state {
            TransferState::Reserved => {
                self.state = TransferState::Declared {
                    name,
                    size,
                    bytes: Vec::new(),
                };
            }
            // A lost reply may repeat the same declaration, never a different file.
            TransferState::Declared {
                name: existing_name,
                size: existing_size,
                ..
            } if *existing_size == size && *existing_name == name => {}
            TransferState::Declared { .. } => {
                return Err("The save transfer already has a different file.".into());
            }
        }
        Ok(())
    }

    fn chunk(&mut self, chunk: Chunk) -> Result<usize, String> {
        let TransferState::Declared {
            size,
            bytes: received,
            ..
        } = &mut self.state
        else {
            return Err("The save transfer has no file yet.".into());
        };
        if chunk.bytes.is_empty()
            || chunk.bytes.len() > CHUNK
            || chunk
                .offset
                .checked_add(chunk.bytes.len())
                .is_none_or(|end| end > *size)
        {
            return Err("Invalid save chunk size.".into());
        }
        if chunk.offset < received.len() {
            if received.get(chunk.offset..chunk.offset + chunk.bytes.len())
                != Some(chunk.bytes.as_slice())
            {
                return Err("A save retry changed the file bytes.".into());
            }
        } else if chunk.offset == received.len() {
            received.extend(chunk.bytes);
        } else {
            return Err("The save chunk is out of order.".into());
        }
        Ok(received.len())
    }
}

fn parse_chunk(value: &Value) -> Result<Chunk, String> {
    let offset = value["offset"]
        .as_u64()
        .filter(|n| *n <= LIMIT as u64)
        .and_then(|n| usize::try_from(n).ok())
        .ok_or("Invalid save offset.")?;
    let encoded = value["base64"]
        .as_str()
        .filter(|s| s.len() <= CHUNK.div_ceil(3) * 4)
        .ok_or("The save chunk is too large.")?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid save bytes.")?;
    Ok(Chunk { offset, bytes })
}
pub fn command(method: &str, value: &Value) -> Result<Value, String> {
    let mut pool = transfers()
        .lock()
        .map_err(|_| "The file save service stopped.")?;
    pool.execute(Command::parse(method, value)?, Instant::now())
}
pub fn finish(value: &Value) -> Result<(String, Vec<u8>), String> {
    transfers()
        .lock()
        .map_err(|_| "The file save service stopped.")?
        .take(value, Instant::now())
}
pub fn write_atomic_if(path: &Path, bytes: &[u8], active: impl Fn() -> bool) -> Result<(), String> {
    use std::{fs::OpenOptions, io::Write, os::unix::fs::OpenOptionsExt};
    if !path.is_absolute() || path.file_name().is_none() || bytes.len() > LIMIT {
        return Err("Invalid file save destination or size.".into());
    }
    let parent = path
        .parent()
        .ok_or("The destination has no parent directory.")?;
    let temporary = parent.join(format!(".studio-save-{}.tmp", uuid()?));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&temporary)
        .map_err(|e| e.to_string())?;
    let result = (|| {
        if !active() {
            return Err(std::io::Error::other("The save owner disconnected."));
        }
        file.write_all(bytes)?;
        file.sync_all()?;
        if !active() {
            return Err(std::io::Error::other("The save owner disconnected."));
        }
        std::fs::rename(&temporary, path)?;
        std::fs::File::open(parent)?.sync_all()
    })()
    .map_err(|e: std::io::Error| e.to_string());
    if result.is_err() {
        drop(std::fs::remove_file(&temporary));
    }
    result
}
#[cfg(test)]
mod tests {
    use super::*;
    fn write_atomic(path: &Path, bytes: &[u8]) -> Result<(), String> {
        write_atomic_if(path, bytes, || true)
    }
    #[test]
    fn save_transfer_preserves_bytes_bounds_retry_order_expiry_and_single_use() {
        let now = Instant::now();
        let mut pool = Transfers::default();
        let body = "Ж🦀".repeat(80_000).into_bytes();
        let begin = pool
            .begin(&json!({"name":"../message.txt","size":body.len()}), now)
            .unwrap();
        let id = begin["transfer"].as_str().unwrap();
        assert!(pool.take(&json!({"transfer":id}), now).is_err());
        for (index, chunk) in body.chunks(CHUNK).enumerate() {
            let value = json!({"transfer":id,"offset":index*CHUNK,"base64":base64::engine::general_purpose::STANDARD.encode(chunk)});
            pool.chunk(&value, now).unwrap();
            pool.chunk(&value, now).unwrap();
        }
        assert!(
            pool.chunk(&json!({"transfer":id,"offset":0,"base64":"eA=="}), now)
                .is_err()
        );
        let (name, actual) = pool.take(&json!({"transfer":id}), now).unwrap();
        assert_eq!(name, "message.txt");
        assert_eq!(actual, body);
        assert!(pool.take(&json!({"transfer":id}), now).is_err());
        let empty = pool
            .begin(&json!({"name":"empty.txt","size":0}), now)
            .unwrap();
        assert!(pool.take(&empty, now + LIFETIME).is_err());
        for value in [
            json!({"name":"..","size":0}),
            json!({"name":"x\n","size":0}),
            json!({"name":"x","size":LIMIT+1}),
        ] {
            assert!(pool.begin(&value, now).is_err());
        }
        pool.begin(&json!({"name":"a","size":LIMIT}), now).unwrap();
        pool.begin(&json!({"name":"b","size":LIMIT}), now).unwrap();
        assert!(pool.begin(&json!({"name":"c","size":1}), now).is_err());
    }
    #[test]
    fn reserved_transfer_accepts_one_declaration_before_bytes() {
        let now = Instant::now();
        let mut pool = Transfers::default();
        let id = pool.begin(&json!({}), now).unwrap()["transfer"].clone();
        let chunk = json!({"transfer":id,"offset":0,"base64":"eA=="});
        assert!(pool.chunk(&chunk, now).is_err());
        assert!(pool.take(&json!({"transfer":id}), now).is_err());
        let declare = json!({"transfer":id,"name":"log.txt","size":1});
        pool.declare_typed(
            transfer_id(&declare, "A save transfer is required."),
            declaration(&declare),
            now,
        )
        .unwrap();
        pool.declare_typed(
            transfer_id(&declare, "A save transfer is required."),
            declaration(&declare),
            now,
        )
        .unwrap();
        assert!(
            pool.declare_typed(
                Ok(id.as_str().unwrap().into()),
                declaration(&json!({"name":"other.txt","size":1})),
                now
            )
            .is_err()
        );
        assert!(
            pool.declare_typed(
                Ok(id.as_str().unwrap().into()),
                declaration(&json!({"name":"log.txt","size":2})),
                now
            )
            .is_err()
        );
        pool.chunk(&chunk, now).unwrap();
        assert_eq!(
            pool.take(&json!({"transfer":id}), now).unwrap(),
            ("log.txt".into(), b"x".to_vec())
        );
        let late = pool.begin(&json!({}), now).unwrap()["transfer"].clone();
        assert!(
            pool.declare_typed(
                Ok(late.as_str().unwrap().into()),
                declaration(&json!({"name":"a","size":1})),
                now + LIFETIME
            )
            .is_err()
        );
        assert!(pool.begin(&json!({"name":"x"}), now).is_err());
    }
    #[test]
    fn command_boundary_reserves_declares_retries_and_cancels() {
        let now = Instant::now();
        let mut pool = Transfers::default();
        let mut run =
            |method: &str, value: Value| pool.execute(Command::parse(method, &value).unwrap(), now);
        let id = run("desktop.saveBegin", json!({})).unwrap()["transfer"].clone();
        let declaration = json!({"transfer":id,"name":"../x","size":1});
        run("desktop.saveDeclare", declaration.clone()).unwrap();
        run("desktop.saveDeclare", declaration).unwrap();
        let chunk = json!({"transfer":id,"offset":0,"base64":"eA=="});
        assert_eq!(
            run("desktop.saveChunk", chunk.clone()).unwrap()["received"],
            1
        );
        assert_eq!(run("desktop.saveChunk", chunk).unwrap()["received"], 1);
        run("desktop.saveCancel", json!({"transfer":id})).unwrap();
        assert!(
            run(
                "desktop.saveChunk",
                json!({"transfer":id,"offset":0,"base64":"eA=="})
            )
            .is_err()
        );
    }
    #[test]
    fn save_command_keeps_validation_order_and_cancel_semantics() {
        let now = Instant::now();
        let mut pool = Transfers::default();
        let id = pool.begin(&json!({}), now).unwrap()["transfer"].clone();
        assert_eq!(
            pool.chunk(&json!({"transfer":id,"offset":"bad"}), now)
                .unwrap_err(),
            "The save transfer has no file yet."
        );
        assert_eq!(
            pool.declare_typed(
                Ok("missing".into()),
                declaration(&json!({"size":"bad"})),
                now
            )
            .unwrap_err(),
            "The save transfer expired or is no longer available."
        );
        pool.declare_typed(
            Ok(id.as_str().unwrap().into()),
            declaration(&json!({"name":"x","size":1})),
            now,
        )
        .unwrap();
        assert_eq!(
            pool.chunk(&json!({"transfer":id,"offset":LIMIT,"base64":"eA=="}), now)
                .unwrap_err(),
            "Invalid save chunk size."
        );
        assert_eq!(
            pool.chunk(
                &json!({"transfer":id,"offset":u64::MAX,"base64":"eA=="}),
                now
            )
            .unwrap_err(),
            "Invalid save offset."
        );
        pool.cancel(&json!({"transfer":id})).unwrap();
        pool.cancel(&json!({"transfer":id})).unwrap();
        assert!(pool.take(&json!({"transfer":id}), now).is_err());
    }
    #[test]
    fn save_writes_exact_bytes_and_cleans_up_failed_replace() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("draft.txt");
        std::fs::write(&path, "old").unwrap();
        write_atomic(&path, "Черновик 🦀".as_bytes()).unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "Черновик 🦀");
        assert_eq!(
            std::fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o600
        );
        let folder = directory.path().join("folder");
        std::fs::create_dir(&folder).unwrap();
        assert!(write_atomic(&folder, b"cannot replace a directory").is_err());
        assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 2);
        write_atomic(&path, b"").unwrap();
        assert_eq!(std::fs::metadata(path).unwrap().len(), 0);
    }
    #[test]
    fn disconnected_owner_cannot_replace_the_destination() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("keep.txt");
        std::fs::write(&path, "original").unwrap();
        for disconnect_at in [1, 2] {
            let checks = std::cell::Cell::new(0);
            assert!(
                write_atomic_if(&path, b"replacement", || {
                    checks.set(checks.get() + 1);
                    checks.get() < disconnect_at
                })
                .is_err()
            );
            assert_eq!(std::fs::read_to_string(&path).unwrap(), "original");
            assert_eq!(std::fs::read_dir(directory.path()).unwrap().count(), 1);
        }
    }
}
