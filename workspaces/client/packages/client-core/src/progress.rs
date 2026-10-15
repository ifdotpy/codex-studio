//! Bounded progress documents and coalesced reads for every presentation caller.
use crate::{
    backend::Backend,
    delivery::{StoreSender, state},
    protocol::{CommandError, Request, Response},
    sync,
};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value, json};
use std::{
    collections::{HashMap, VecDeque},
    sync::{Arc, Weak},
    time::{Duration, Instant},
};
use tokio::sync::Mutex;
const MAX_ENTRIES: usize = 32;
const MEMORY_BYTES: usize = 1024 * 1024;
const STORED_BYTES: usize = 256 * 1024;

pub fn initialize(connection: &Connection) -> rusqlite::Result<()> {
    connection.execute_batch("CREATE TABLE IF NOT EXISTS client_progress (scope TEXT PRIMARY KEY, document TEXT NOT NULL, bytes INTEGER NOT NULL, accessed INTEGER NOT NULL);")
}
fn size(value: &Value) -> usize {
    ["markdown", "path"]
        .into_iter()
        .map(|key| value[key].as_str().unwrap_or("").encode_utf16().count() * 2)
        .sum()
}
fn validate(value: &Value, scope: &str) -> bool {
    value["scope"].as_str() == Some(scope)
        && value["markdown"].is_string()
        && value["path"].is_string()
        && (value["revision"].is_null() || value["revision"].is_string())
}
pub fn stored(connection: &Connection, request: &Request) -> rusqlite::Result<Response> {
    let scope = request.params["scope"].as_str().unwrap_or("");
    let clock: i64 = connection.query_row(
        "SELECT COALESCE(MAX(accessed),0)+1 FROM client_progress",
        [],
        |r| r.get(0),
    )?;
    let value = match request.method.as_str() {
        "progress.cache_get" => {
            let encoded: Option<String> = connection
                .query_row(
                    "SELECT document FROM client_progress WHERE scope=?1",
                    [scope],
                    |r| r.get(0),
                )
                .optional()?;
            connection.execute(
                "UPDATE client_progress SET accessed=?1 WHERE scope=?2",
                params![clock, scope],
            )?;
            encoded
                .and_then(|v| serde_json::from_str::<Value>(&v).ok())
                .filter(|v| validate(v, scope))
                .unwrap_or(Value::Null)
        }
        "progress.cache_put" => {
            let value = &request.params["value"];
            if !validate(value, scope) {
                return Ok(Response::failure(
                    request.id.clone(),
                    "invalid_progress",
                    "Invalid progress document.",
                ));
            }
            let encoded = serde_json::to_string(value).unwrap();
            let bytes = encoded.encode_utf16().count() * 2;
            if bytes > STORED_BYTES {
                connection.execute("DELETE FROM client_progress WHERE scope=?1", [scope])?;
            } else {
                connection.execute("INSERT INTO client_progress(scope,document,bytes,accessed) VALUES(?1,?2,?3,?4) ON CONFLICT(scope) DO UPDATE SET document=excluded.document,bytes=excluded.bytes,accessed=excluded.accessed",params![scope,encoded,bytes as i64,clock])?;
                loop {
                    let (count, bytes): (i64, i64) = connection.query_row(
                        "SELECT COUNT(*),COALESCE(SUM(bytes),0) FROM client_progress",
                        [],
                        |r| Ok((r.get(0)?, r.get(1)?)),
                    )?;
                    if count <= MAX_ENTRIES as i64 && bytes <= STORED_BYTES as i64 {
                        break;
                    }
                    connection.execute("DELETE FROM client_progress WHERE scope=(SELECT scope FROM client_progress ORDER BY accessed,scope LIMIT 1)",[])?;
                }
            }
            Value::Null
        }
        _ => {
            return Ok(Response::failure(
                request.id.clone(),
                "unknown_method",
                "Unknown progress cache command.",
            ));
        }
    };
    Ok(Response::success(request.id.clone(), value))
}

struct Entry {
    scope: String,
    value: Value,
    fetched: Option<Instant>,
}
#[derive(Default)]
struct Memory {
    entries: VecDeque<Entry>,
}
impl Memory {
    fn get(&mut self, scope: &str) -> Option<(Value, Option<Instant>)> {
        let index = self.entries.iter().position(|v| v.scope == scope)?;
        let entry = self.entries.remove(index).unwrap();
        let result = (entry.value.clone(), entry.fetched);
        self.entries.push_back(entry);
        Some(result)
    }
    fn put(&mut self, scope: &str, value: Value, fetched: Option<Instant>) {
        self.entries.retain(|v| v.scope != scope);
        self.entries.push_back(Entry {
            scope: scope.into(),
            value,
            fetched,
        });
        while self.entries.len() > MAX_ENTRIES
            || self.entries.iter().map(|v| size(&v.value)).sum::<usize>() > MEMORY_BYTES
        {
            self.entries.pop_front();
        }
    }
}
#[derive(Default)]
pub struct Progress {
    memory: Mutex<Memory>,
    pending: Mutex<HashMap<String, Weak<Mutex<()>>>>,
}
impl Progress {
    async fn peek(&self, store: &StoreSender, scope: &str) -> Result<Value, CommandError> {
        if let Some((value, _)) = self.memory.lock().await.get(scope) {
            return Ok(value);
        }
        let value = state(store, "progress.cache_get", json!({"scope":scope}))
            .await
            .map_err(|e| sync::error("storage_error", e))?;
        if !value.is_null() {
            self.memory.lock().await.put(scope, value.clone(), None);
        }
        Ok(value)
    }
    async fn read(
        &self,
        backend: &Backend,
        store: &StoreSender,
        scope: &str,
        agent: &str,
    ) -> Result<Value, CommandError> {
        let started = Instant::now();
        let lock = {
            let mut pending = self.pending.lock().await;
            pending.retain(|_, v| v.strong_count() > 0);
            if let Some(lock) = pending.get(scope).and_then(Weak::upgrade) {
                lock
            } else {
                let lock = Arc::new(Mutex::new(()));
                pending.insert(scope.into(), Arc::downgrade(&lock));
                lock
            }
        };
        let _guard = lock.lock().await;
        if let Some((value, Some(fetched))) = self.memory.lock().await.get(scope) {
            if fetched >= started {
                return Ok(value);
            }
        }
        let mut url = reqwest::Url::parse("http://localhost/api/panel").unwrap();
        url.query_pairs_mut().append_pair("agent", agent);
        let path = format!("{}?{}", url.path(), url.query().unwrap());
        let response = backend
            .dispatch(Request {
                id: "progress-read".into(),
                method: "api.request".into(),
                params: json!({"path":path,"timeoutMs":8000}),
            })
            .await;
        if let Some(error) = response.error {
            return Err(error);
        }
        let response = response.result.unwrap_or(Value::Null);
        let value = json!({"scope":scope,"markdown":response["markdown"],"path":response["path"],"revision":response["revision"]});
        if response["agent"].as_str() != Some(agent)
            || response["format"] != "markdown"
            || !validate(&value, scope)
        {
            return Err(sync::error(
                "invalid_progress",
                "The PROGRESS.md response is invalid.",
            ));
        }
        // A disposable disk copy must not turn a valid server read into a failure.
        let _ = state(
            store,
            "progress.cache_put",
            json!({"scope":scope,"value":value}),
        )
        .await;
        self.memory
            .lock()
            .await
            .put(scope, value.clone(), Some(Instant::now()));
        Ok(value)
    }
    pub async fn dispatch(
        &self,
        backend: &Result<Backend, String>,
        store: &StoreSender,
        request: Request,
    ) -> Response {
        let result = async {
            let state_dir = request.params["stateDir"]
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= 4096)
                .ok_or_else(|| sync::error("invalid_params", "A state directory is required."))?;
            let agent = request.params["agentId"]
                .as_str()
                .filter(|v| !v.is_empty() && v.len() <= 256)
                .ok_or_else(|| sync::error("invalid_params", "An agent identity is required."))?;
            let scope = serde_json::to_string(&[state_dir, agent]).unwrap();
            match request.method.as_str() {
                "progress.peek" => self.peek(store, &scope).await,
                "progress.read" => {
                    let backend = backend
                        .as_ref()
                        .map_err(|e| sync::error("backend_config", e))?;
                    tokio::time::timeout(
                        Duration::from_secs(8),
                        self.read(backend, store, &scope, agent),
                    )
                    .await
                    .map_err(|_| {
                        sync::error("timeout", "The progress document did not load in time.")
                    })?
                }
                _ => Err(sync::error("unknown_method", "Unknown progress command.")),
            }
        }
        .await;
        match result {
            Ok(value) => Response::success(request.id, value),
            Err(error) => Response {
                id: request.id,
                result: None,
                error: Some(error),
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[tokio::test]
    async fn concurrent_views_share_one_http_read_and_restart_uses_the_disk_copy() {
        use tokio::net::TcpListener;
        let directory = tempfile::tempdir().unwrap();
        let mut store = crate::store::Store::open(&directory.path().join("client.sqlite")).unwrap();
        let (sender, mut receive) = tokio::sync::mpsc::channel::<crate::delivery::StoreCommand>(16);
        let worker = std::thread::spawn(move || {
            while let Some((request, reply)) = receive.blocking_recv() {
                let _ = reply.send(store.dispatch(request));
            }
        });
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let (headers, _) = crate::backend::tests::read_http(&mut stream).await;
            assert!(headers.starts_with("GET /api/panel?agent=one "));
            crate::backend::tests::reply(&mut stream,r#"{"agent":"one","format":"markdown","markdown":"Saved 🦀","path":"/tmp/PROGRESS.md","revision":"r1"}"#).await;
            assert!(
                tokio::time::timeout(Duration::from_millis(100), listener.accept())
                    .await
                    .is_err()
            );
        });
        let request = |id: &str, method: &str| Request {
            id: id.into(),
            method: method.into(),
            params: json!({"stateDir":"/tmp/state","agentId":"one"}),
        };
        let service = Progress::default();
        let (first, second) = tokio::join!(
            service.dispatch(&backend, &sender, request("one", "progress.read")),
            service.dispatch(&backend, &sender, request("two", "progress.read"))
        );
        assert!(first.error.is_none() && second.error.is_none());
        assert_eq!(first.result, second.result);
        let restored = Progress::default()
            .dispatch(&backend, &sender, request("peek", "progress.peek"))
            .await;
        assert_eq!(restored.result, first.result);
        server.await.unwrap();
        drop(sender);
        worker.join().unwrap();
    }
    #[test]
    fn disposable_caches_evict_old_entries_within_size_limits() {
        let dir = tempfile::tempdir().unwrap();
        let mut store = crate::store::Store::open(&dir.path().join("client.sqlite")).unwrap();
        for index in 0..40 {
            let scope = format!("scope-{index}");
            let result=store.dispatch(Request{id:"cache".into(),method:"progress.cache_put".into(),params:json!({"scope":scope,"value":{"scope":scope,"markdown":"x".repeat(6000),"path":"/tmp/PROGRESS.md","revision":null}})});
            assert!(result.error.is_none());
        }
        let (count, bytes): (i64, i64) = store
            .connection
            .query_row("SELECT COUNT(*),SUM(bytes) FROM client_progress", [], |r| {
                Ok((r.get(0)?, r.get(1)?))
            })
            .unwrap();
        assert!(count <= MAX_ENTRIES as i64 && bytes <= STORED_BYTES as i64);
        let oldest = store.dispatch(Request {
            id: "get".into(),
            method: "progress.cache_get".into(),
            params: json!({"scope":"scope-0"}),
        });
        assert_eq!(oldest.result, Some(Value::Null));
        let mut memory = Memory::default();
        for index in 0..40 {
            memory.put(
                &index.to_string(),
                json!({"markdown":"x".repeat(30000)}),
                None,
            );
        }
        assert!(memory.entries.len() <= MAX_ENTRIES);
        assert!(memory.entries.iter().map(|v| size(&v.value)).sum::<usize>() <= MEMORY_BYTES);
    }
}
