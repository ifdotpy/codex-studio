//! Bounded, process-local model catalog reads for display.

use crate::provider_session::{
    ProviderProcess, ProviderSession, ProviderSessionConfig, ProviderSessionEvent,
};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{Child, ChildStderr, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use studio_runtime_providers::{Method, ResponseBody};

const CATALOG_TTL: Duration = Duration::from_secs(300);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CATALOG_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CATALOG_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CATALOG_PAGES: usize = 100;

#[derive(Clone, Debug, PartialEq)]
pub enum CatalogRead {
    Ready(Value),
    Pending,
    Unavailable,
}

struct CacheEntry {
    generation: u64,
    value: Option<Value>,
    expires_at: Option<Instant>,
    error: bool,
    pending: bool,
}

struct CatalogInner {
    executable: Option<PathBuf>,
    cache: Mutex<HashMap<(String, String, String), CacheEntry>>,
    next_generation: Mutex<u64>,
}

/// A single HTTP-state-owned cache and its provider process configuration.
#[derive(Clone)]
pub struct ModelCatalogService(Arc<CatalogInner>);

impl ModelCatalogService {
    #[must_use]
    pub fn new(executable: Option<String>) -> Self {
        Self(Arc::new(CatalogInner {
            executable: executable.map(PathBuf::from),
            cache: Mutex::new(HashMap::new()),
            next_generation: Mutex::new(0),
        }))
    }

    /// Display reads return stale data immediately while one refresh runs.
    pub fn read(&self, account_key: &str, home: &str, provider: &str, retry: bool) -> CatalogRead {
        if provider != "codex" {
            return CatalogRead::Unavailable;
        }

        let now = Instant::now();
        let cache_key = (account_key.to_owned(), home.to_owned(), provider.to_owned());
        let (generation, stale) = {
            let Ok(mut cache) = self.0.cache.lock() else {
                return CatalogRead::Unavailable;
            };
            if let Some(entry) = cache.get_mut(&cache_key) {
                if entry.pending {
                    return match &entry.value {
                        Some(value) => CatalogRead::Ready(value.clone()),
                        None => CatalogRead::Pending,
                    };
                }
                if entry.expires_at.is_some_and(|expires| expires > now) {
                    if entry.error {
                        if !retry {
                            return CatalogRead::Unavailable;
                        }
                    } else if let Some(value) = &entry.value {
                        return CatalogRead::Ready(value.clone());
                    }
                }
            }
            let stale = cache.get(&cache_key).and_then(|entry| entry.value.clone());
            let Ok(mut next) = self.0.next_generation.lock() else {
                return CatalogRead::Unavailable;
            };
            *next = next.saturating_add(1);
            let generation = *next;
            cache.insert(
                cache_key.clone(),
                CacheEntry {
                    generation,
                    value: stale.clone(),
                    expires_at: None,
                    error: false,
                    pending: true,
                },
            );
            (generation, stale)
        };

        let service = self.clone();
        let account = account_key.to_owned();
        let home = home.to_owned();
        let worker_cache_key = cache_key.clone();
        let spawned = thread::Builder::new()
            .name("model-catalog-refresh".to_owned())
            .spawn(move || {
                let result = read_provider_catalog(
                    service.0.executable.as_deref(),
                    &home,
                    account != "default",
                );
                if let Ok(mut cache) = service.0.cache.lock()
                    && let Some(entry) = cache.get_mut(&worker_cache_key)
                    && entry.generation == generation
                {
                    entry.pending = false;
                    entry.expires_at = Some(Instant::now() + CATALOG_TTL);
                    match result {
                        Ok(value) => {
                            entry.value = Some(value);
                            entry.error = false;
                        }
                        Err(()) => entry.error = true,
                    }
                }
            });
        if spawned.is_err() {
            if let Ok(mut cache) = self.0.cache.lock()
                && let Some(entry) = cache.get_mut(&cache_key)
                && entry.generation == generation
            {
                entry.pending = false;
                entry.error = true;
                entry.expires_at = Some(Instant::now() + CATALOG_TTL);
            }
            return CatalogRead::Unavailable;
        }
        stale.map_or(CatalogRead::Pending, CatalogRead::Ready)
    }
}

fn read_provider_catalog(
    executable: Option<&std::path::Path>,
    home: &str,
    isolated: bool,
) -> Result<Value, ()> {
    let executable = executable.unwrap_or_else(|| std::path::Path::new("codex"));
    let mut command = Command::new(executable);
    command
        .args(["app-server", "--listen", "stdio://"])
        .env("CODEX_HOME", home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if isolated {
        command
            .args(["-c", "cli_auth_credentials_store=\"file\""])
            .env_remove("OPENAI_API_KEY")
            .env_remove("CODEX_API_KEY");
    }
    let child = command.spawn().map_err(|_| ())?;
    let process = ChildProcess::new(child).map_err(|_| ())?;
    let started = Instant::now();
    let mut config = ProviderSessionConfig::new();
    config.generation = 1;
    config.request_timeout_ms = duration_ms(CATALOG_TIMEOUT);
    config.initialize_params = json!({
        "clientInfo": {"name": "codex_studio_catalog", "version": "1.0.0"},
        "capabilities": {"experimentalApi": true}
    });
    let mut session = ProviderSession::connect(Box::new(process), config).map_err(|_| ())?;
    let result = read_pages(&mut session, started);
    let closed = session.close();
    match (result, closed) {
        (Ok(value), Ok(())) => Ok(value),
        _ => Err(()),
    }
}

fn read_pages(session: &mut ProviderSession, started: Instant) -> Result<Value, ()> {
    let mut rows = Vec::new();
    let mut first_page = None;
    let mut cursor: Option<String> = None;
    let mut seen_cursors = std::collections::HashSet::new();
    let mut seen_models = std::collections::HashSet::new();
    let mut total_bytes = 0_usize;

    loop {
        if cursor.is_some() && seen_cursors.len() >= MAX_CATALOG_PAGES {
            return Err(());
        }
        let remaining = CATALOG_TIMEOUT.checked_sub(started.elapsed()).ok_or(())?;
        let timeout_ms = duration_ms(remaining);
        let params = match &cursor {
            Some(cursor) => json!({"limit": 100, "includeHidden": true, "cursor": cursor}),
            None => json!({"limit": 100, "includeHidden": true}),
        };
        let ticket = session
            .send_request(Method::ModelList, params, None, true, timeout_ms)
            .map_err(|_| ())?;
        let page = loop {
            match session.receive_event().map_err(|_| ())? {
                ProviderSessionEvent::Reply {
                    ticket: received,
                    body: ResponseBody::Result(value),
                    ..
                } if received.id == ticket.id => break value,
                ProviderSessionEvent::Reply {
                    ticket: received, ..
                }
                | ProviderSessionEvent::UnknownOutcome { ticket: received }
                    if received.id == ticket.id =>
                {
                    return Err(());
                }
                ProviderSessionEvent::Closed { .. } => return Err(()),
                ProviderSessionEvent::Reply { .. }
                | ProviderSessionEvent::UnknownOutcome { .. }
                | ProviderSessionEvent::Inbound(_) => {}
            }
        };
        let page_bytes = serde_json::to_vec(&page).map_err(|_| ())?.len();
        if page_bytes > MAX_CATALOG_PAGE_BYTES {
            return Err(());
        }
        total_bytes = total_bytes.checked_add(page_bytes).ok_or(())?;
        if total_bytes > MAX_CATALOG_RESPONSE_BYTES {
            return Err(());
        }
        let page_object = page.as_object().ok_or(())?;
        let data = page_object
            .get("data")
            .and_then(Value::as_array)
            .ok_or(())?;
        if first_page.is_none() {
            first_page = Some(page_object.clone());
        }
        for row in data {
            let model = row
                .as_object()
                .and_then(|row| row.get("model"))
                .and_then(Value::as_str)
                .filter(|model| !model.trim().is_empty())
                .ok_or(())?;
            if !seen_models.insert(model.to_owned()) {
                return Err(());
            }
            rows.push(row.clone());
        }
        match page_object.get("nextCursor") {
            None | Some(Value::Null) => break,
            Some(Value::String(value))
                if !value.is_empty() && seen_cursors.insert(value.clone()) =>
            {
                cursor = Some(value.clone());
            }
            _ => return Err(()),
        }
    }

    let mut result = first_page.ok_or(())?;
    result.insert("data".to_owned(), Value::Array(rows));
    result.insert("nextCursor".to_owned(), Value::Null);
    Ok(Value::Object(result))
}

fn duration_ms(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

struct ChildProcess {
    child: Option<Child>,
    stderr_reader: Option<JoinHandle<()>>,
}

impl ChildProcess {
    fn new(mut child: Child) -> io::Result<Self> {
        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| io::Error::other("provider stderr was not piped"))?;
        let stderr_reader = thread::spawn(move || discard_stderr(stderr));
        Ok(Self {
            child: Some(child),
            stderr_reader: Some(stderr_reader),
        })
    }
}

impl ProviderProcess for ChildProcess {
    fn split(&mut self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("provider child is closed"))?;
        let reader = child
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("provider stdout was not piped"))?;
        let writer = child
            .stdin
            .take()
            .ok_or_else(|| io::Error::other("provider stdin was not piped"))?;
        Ok((Box::new(reader), Box::new(writer)))
    }

    fn stop(&mut self) -> io::Result<()> {
        let Some(mut child) = self.child.take() else {
            return Ok(());
        };
        // Switch to process-wrap process-group termination when that adapter lands.
        match child.kill() {
            Ok(()) => child.wait().map(|_| ())?,
            Err(error) if error.kind() == io::ErrorKind::InvalidInput => {
                let _status = child.wait()?;
            }
            Err(error) => return Err(error),
        }
        if let Some(reader) = self.stderr_reader.take() {
            reader
                .join()
                .map_err(|_| io::Error::other("provider stderr reader failed"))?;
        }
        Ok(())
    }
}

fn discard_stderr(mut stderr: ChildStderr) {
    let mut buffer = [0_u8; 8192];
    loop {
        match stderr.read(&mut buffer) {
            Ok(0) | Err(_) => return,
            Ok(_) => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CatalogRead, ModelCatalogService};
    #[test]
    fn claude_catalog_is_reported_unavailable_without_starting_codex() {
        let service = ModelCatalogService::new(None);
        assert_eq!(
            service.read("account", "/disposable/home", "claude", false),
            CatalogRead::Unavailable
        );
    }
}
