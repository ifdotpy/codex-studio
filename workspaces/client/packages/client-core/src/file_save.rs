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
    name: String,
    size: Option<usize>,
    bytes: Vec<u8>,
    expires: Instant,
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
fn declaration(value: &Value) -> Result<(String, usize), String> {
    let size = value["size"]
        .as_u64()
        .filter(|n| *n <= LIMIT as u64)
        .ok_or("Save accepts at most 64 MiB of file data.")? as usize;
    Ok((name(&value["name"])?, size))
}
fn name(value: &Value) -> Result<String, String> {
    let raw = value
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 1024)
        .ok_or("Invalid file name.")?;
    let normalized = raw.replace('\\', "/");
    let name = normalized.rsplit('/').next().unwrap();
    if name.is_empty() || matches!(name, "." | "..") || name.chars().any(|c| c <= '\u{1f}') {
        return Err("Invalid file name.".into());
    }
    Ok(name.into())
}
impl Transfers {
    pub(crate) fn cancel(&mut self, value: &Value) -> Result<Value, String> {
        let id = value["transfer"]
            .as_str()
            .ok_or("A transfer is required.")?;
        self.items.remove(id);
        Ok(Value::Null)
    }

    fn expire(&mut self, now: Instant) {
        self.items.retain(|_, v| v.expires > now);
    }
    pub(crate) fn begin(&mut self, value: &Value, now: Instant) -> Result<Value, String> {
        self.expire(now);
        // An empty value reserves the gesture's single save for a later declaration.
        let declared = if value.get("name").is_none() && value.get("size").is_none() {
            None
        } else {
            Some(declaration(value)?)
        };
        if self.items.len() >= 2 {
            return Err("Finish the current file saves first.".into());
        }
        let id = uuid()?;
        let (name, size) = declared.map_or((String::new(), None), |(n, s)| (n, Some(s)));
        self.items.insert(
            id.clone(),
            Transfer {
                name,
                size,
                bytes: Vec::new(),
                expires: now + LIFETIME,
            },
        );
        Ok(json!({"transfer":id,"chunkBytes":CHUNK}))
    }
    fn declare(&mut self, value: &Value, now: Instant) -> Result<Value, String> {
        self.expire(now);
        let transfer = self.get(value)?;
        let (name, size) = declaration(value)?;
        match transfer.size {
            None => {
                transfer.name = name;
                transfer.size = Some(size);
            }
            // A lost reply may repeat the same declaration, never a different file.
            Some(existing) if existing == size && transfer.name == name => {}
            Some(_) => return Err("The save transfer already has a different file.".into()),
        }
        Ok(json!({"chunkBytes":CHUNK}))
    }
    fn get(&mut self, value: &Value) -> Result<&mut Transfer, String> {
        let id = value["transfer"]
            .as_str()
            .ok_or("A save transfer is required.")?;
        self.items
            .get_mut(id)
            .ok_or("The save transfer expired or is no longer available.".into())
    }
    pub(crate) fn chunk(&mut self, value: &Value, now: Instant) -> Result<Value, String> {
        self.expire(now);
        let transfer = self.get(value)?;
        let size = transfer.size.ok_or("The save transfer has no file yet.")?;
        let offset = value["offset"]
            .as_u64()
            .filter(|n| *n <= LIMIT as u64)
            .ok_or("Invalid save offset.")? as usize;
        let encoded = value["base64"]
            .as_str()
            .filter(|s| s.len() <= CHUNK.div_ceil(3) * 4)
            .ok_or("The save chunk is too large.")?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map_err(|_| "Invalid save bytes.")?;
        if bytes.is_empty() || bytes.len() > CHUNK || offset + bytes.len() > size {
            return Err("Invalid save chunk size.".into());
        }
        if offset < transfer.bytes.len() {
            if transfer.bytes.get(offset..offset + bytes.len()) != Some(bytes.as_slice()) {
                return Err("A save retry changed the file bytes.".into());
            }
        } else if offset == transfer.bytes.len() {
            transfer.bytes.extend(bytes);
        } else {
            return Err("The save chunk is out of order.".into());
        }
        Ok(json!({"received":transfer.bytes.len()}))
    }
    pub(crate) fn take(
        &mut self,
        value: &Value,
        now: Instant,
    ) -> Result<(String, Vec<u8>), String> {
        self.expire(now);
        let id = value["transfer"]
            .as_str()
            .ok_or("A save transfer is required.")?;
        let transfer = self
            .items
            .get(id)
            .ok_or("The save transfer expired or is no longer available.")?;
        if Some(transfer.bytes.len()) != transfer.size {
            return Err("The file transfer is incomplete.".into());
        }
        let transfer = self.items.remove(id).unwrap();
        Ok((transfer.name, transfer.bytes))
    }
}
pub fn command(method: &str, value: &Value) -> Result<Value, String> {
    let mut pool = transfers()
        .lock()
        .map_err(|_| "The file save service stopped.")?;
    match method {
        "desktop.saveBegin" => pool.begin(value, Instant::now()),
        "desktop.saveDeclare" => pool.declare(value, Instant::now()),
        "desktop.saveChunk" => pool.chunk(value, Instant::now()),
        "desktop.saveCancel" => {
            let id = value["transfer"]
                .as_str()
                .ok_or("A save transfer is required.")?;
            pool.items.remove(id);
            Ok(Value::Null)
        }
        _ => Err("Unknown file transfer command.".into()),
    }
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
        let _ = std::fs::remove_file(&temporary);
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
        pool.declare(&declare, now).unwrap();
        pool.declare(&declare, now).unwrap();
        assert!(
            pool.declare(&json!({"transfer":id,"name":"other.txt","size":1}), now)
                .is_err()
        );
        assert!(
            pool.declare(&json!({"transfer":id,"name":"log.txt","size":2}), now)
                .is_err()
        );
        pool.chunk(&chunk, now).unwrap();
        assert_eq!(
            pool.take(&json!({"transfer":id}), now).unwrap(),
            ("log.txt".into(), b"x".to_vec())
        );
        let late = pool.begin(&json!({}), now).unwrap()["transfer"].clone();
        assert!(
            pool.declare(
                &json!({"transfer":late,"name":"a","size":1}),
                now + LIFETIME
            )
            .is_err()
        );
        assert!(pool.begin(&json!({"name":"x"}), now).is_err());
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
