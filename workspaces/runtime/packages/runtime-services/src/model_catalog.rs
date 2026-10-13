//! Bounded, process-local model catalog reads for display.

use crate::provider_session::{
    ProviderProcess, ProviderSession, ProviderSessionConfig, ProviderSessionEvent,
};
use process_wrap::std::CommandWrap;
#[cfg(unix)]
use process_wrap::std::ProcessGroup;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::process::{ChildStderr, ChildStdin, ChildStdout, Command, Stdio};
use std::sync::mpsc::{self, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};
use studio_runtime_process::child_priority::ChildPriority;
use studio_runtime_providers::{Method, ResponseBody};
type ManagedChildProcess = Box<dyn process_wrap::std::ChildWrapper>;

const CATALOG_TTL: Duration = Duration::from_secs(300);
const CATALOG_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_CATALOG_PAGE_BYTES: usize = 2 * 1024 * 1024;
const MAX_CATALOG_RESPONSE_BYTES: usize = 16 * 1024 * 1024;
const MAX_CATALOG_PAGES: usize = 100;
pub type ChangeCallback = Arc<dyn Fn() + Send + Sync + 'static>;

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
    on_change: ChangeCallback,
}

/// A single HTTP-state-owned cache and its provider process configuration.
#[derive(Clone)]
pub struct ModelCatalogService(Arc<CatalogInner>);

impl ModelCatalogService {
    #[must_use]
    pub fn new(executable: Option<String>) -> Self {
        Self::with_change_callback(executable, Arc::new(|| {}))
    }

    /// Construct the cache with a shared resource-change callback for completed reads.
    #[must_use]
    pub fn with_change_callback(executable: Option<String>, on_change: ChangeCallback) -> Self {
        Self(Arc::new(CatalogInner {
            executable: executable.map(PathBuf::from),
            cache: Mutex::new(HashMap::new()),
            next_generation: Mutex::new(0),
            on_change,
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
                let _stored = store_result(&service.0, &worker_cache_key, generation, result);
            });
        if spawned.is_err() {
            let _stored = store_result(&self.0, &cache_key, generation, Err(()));
            return CatalogRead::Unavailable;
        }
        stale.map_or(CatalogRead::Pending, CatalogRead::Ready)
    }
}

fn store_result(
    inner: &CatalogInner,
    key: &(String, String, String),
    generation: u64,
    result: Result<Value, ()>,
) -> bool {
    let stored = if let Ok(mut cache) = inner.cache.lock()
        && let Some(entry) = cache.get_mut(key)
        && entry.generation == generation
        && entry.pending
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
        true
    } else {
        false
    };
    if stored {
        (inner.on_change)();
    }
    stored
}

fn read_provider_catalog(
    executable: Option<&std::path::Path>,
    home: &str,
    isolated: bool,
) -> Result<Value, ()> {
    let started = Instant::now();
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
    let child = spawn_provider(command).map_err(|_| ())?;
    let process = ChildProcess::new(child, started + CATALOG_TIMEOUT);
    let mut config = ProviderSessionConfig::new();
    config.generation = 1;
    config.request_timeout_ms =
        duration_ms(CATALOG_TIMEOUT.checked_sub(started.elapsed()).ok_or(())?);
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
    child: Option<ManagedChildProcess>,
    write_deadline: Instant,
    writer_thread: Option<JoinHandle<()>>,
    stderr_reader: Option<JoinHandle<()>>,
}

impl ChildProcess {
    fn new(child: ManagedChildProcess, write_deadline: Instant) -> Self {
        Self {
            child: Some(child),
            write_deadline,
            writer_thread: None,
            stderr_reader: None,
        }
    }
}

impl ProviderProcess for ChildProcess {
    fn split(&mut self) -> io::Result<(Box<dyn Read + Send>, Box<dyn Write + Send>)> {
        let child = self
            .child
            .as_mut()
            .ok_or_else(|| io::Error::other("provider child is closed"))?;
        let stdout =
            take_stdout(child).ok_or_else(|| io::Error::other("provider stdout was not piped"))?;
        let stdin =
            take_stdin(child).ok_or_else(|| io::Error::other("provider stdin was not piped"))?;
        let stderr =
            take_stderr(child).ok_or_else(|| io::Error::other("provider stderr was not piped"))?;
        let (writer, writer_thread) = deadline_writer(stdin, self.write_deadline)?;
        self.writer_thread = Some(writer_thread);
        self.stderr_reader = Some(
            thread::Builder::new()
                .name("model-catalog-stderr".to_owned())
                .spawn(move || discard_stderr(stderr))?,
        );
        Ok((Box::new(BoundedReader::new(stdout)), Box::new(writer)))
    }

    fn stop(&mut self) -> io::Result<()> {
        let mut result = Ok(());
        if let Some(mut child) = self.child.take() {
            result = stop_child(&mut child);
        }
        if let Some(writer) = self.writer_thread.take()
            && writer.join().is_err()
            && result.is_ok()
        {
            result = Err(io::Error::other("provider stdin writer failed"));
        }
        if let Some(reader) = self.stderr_reader.take()
            && reader.join().is_err()
            && result.is_ok()
        {
            result = Err(io::Error::other("provider stderr reader failed"));
        }
        result
    }
}

fn spawn_provider(command: Command) -> io::Result<ManagedChildProcess> {
    let mut command = CommandWrap::from(command);
    #[cfg(unix)]
    command.wrap(ProcessGroup::leader());
    ChildPriority::from_environment().apply_before_spawn(&mut command);
    command.spawn()
}

fn take_stdin(child: &mut ManagedChildProcess) -> Option<ChildStdin> {
    child.stdin().take()
}

fn take_stdout(child: &mut ManagedChildProcess) -> Option<ChildStdout> {
    child.stdout().take()
}

fn take_stderr(child: &mut ManagedChildProcess) -> Option<ChildStderr> {
    child.stderr().take()
}

fn stop_child(child: &mut ManagedChildProcess) -> io::Result<()> {
    let kill = child.kill();
    if let Err(error) = kill
        && !matches!(
            error.kind(),
            io::ErrorKind::NotFound | io::ErrorKind::InvalidInput
        )
    {
        return Err(error);
    }
    child.wait().map(|_| ())
}

enum WriterRequest {
    Write(Vec<u8>, SyncSender<io::Result<usize>>),
    Flush(SyncSender<io::Result<()>>),
}

struct DeadlineWriter {
    requests: SyncSender<WriterRequest>,
    deadline: Instant,
}

fn deadline_writer(
    mut stdin: ChildStdin,
    deadline: Instant,
) -> io::Result<(DeadlineWriter, JoinHandle<()>)> {
    let (requests, receiver) = mpsc::sync_channel(1);
    let thread = thread::Builder::new()
        .name("model-catalog-stdin".to_owned())
        .spawn(move || {
            while let Ok(request) = receiver.recv() {
                match request {
                    WriterRequest::Write(bytes, response) => {
                        let _ignored = response.send(stdin.write(&bytes));
                    }
                    WriterRequest::Flush(response) => {
                        let _ignored = response.send(stdin.flush());
                    }
                }
            }
        })?;
    Ok((DeadlineWriter { requests, deadline }, thread))
}

impl DeadlineWriter {
    fn remaining(&self) -> io::Result<Duration> {
        self.deadline
            .checked_duration_since(Instant::now())
            .filter(|remaining| !remaining.is_zero())
            .ok_or_else(|| io::Error::new(io::ErrorKind::TimedOut, "provider request timed out"))
    }
}

impl Write for DeadlineWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        let remaining = self.remaining()?;
        let (response, result) = mpsc::sync_channel(1);
        self.requests
            .try_send(WriterRequest::Write(bytes.to_vec(), response))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::TimedOut, "provider request timed out")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "provider stdin closed")
                }
            })?;
        result
            .recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    io::Error::new(io::ErrorKind::TimedOut, "provider request timed out")
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "provider stdin closed")
                }
            })?
    }

    fn flush(&mut self) -> io::Result<()> {
        let remaining = self.remaining()?;
        let (response, result) = mpsc::sync_channel(1);
        self.requests
            .try_send(WriterRequest::Flush(response))
            .map_err(|error| match error {
                mpsc::TrySendError::Full(_) => {
                    io::Error::new(io::ErrorKind::TimedOut, "provider request timed out")
                }
                mpsc::TrySendError::Disconnected(_) => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "provider stdin closed")
                }
            })?;
        result
            .recv_timeout(remaining)
            .map_err(|error| match error {
                mpsc::RecvTimeoutError::Timeout => {
                    io::Error::new(io::ErrorKind::TimedOut, "provider request timed out")
                }
                mpsc::RecvTimeoutError::Disconnected => {
                    io::Error::new(io::ErrorKind::BrokenPipe, "provider stdin closed")
                }
            })?
    }
}

struct BoundedReader<R> {
    inner: R,
    frame_bytes: usize,
    total_bytes: usize,
}

impl<R> BoundedReader<R> {
    fn new(inner: R) -> Self {
        Self {
            inner,
            frame_bytes: 0,
            total_bytes: 0,
        }
    }
}

impl<R: Read> Read for BoundedReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let count = self.inner.read(buffer)?;
        if count == 0 {
            return Ok(0);
        }
        self.total_bytes = self.total_bytes.checked_add(count).ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidData, "provider output too large")
        })?;
        if self.total_bytes > MAX_CATALOG_RESPONSE_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "provider output too large",
            ));
        }
        for byte in &buffer[..count] {
            self.frame_bytes = self.frame_bytes.checked_add(1).ok_or_else(|| {
                io::Error::new(io::ErrorKind::InvalidData, "provider frame too large")
            })?;
            if self.frame_bytes > MAX_CATALOG_PAGE_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "provider frame too large",
                ));
            }
            if *byte == b'\n' {
                self.frame_bytes = 0;
            }
        }
        Ok(count)
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
    use super::{
        BoundedReader, CATALOG_TIMEOUT, CacheEntry, CatalogRead, ChildProcess,
        MAX_CATALOG_PAGE_BYTES, MAX_CATALOG_RESPONSE_BYTES, ModelCatalogService, spawn_provider,
        store_result,
    };
    use crate::provider_session::ProviderProcess;
    use std::io::{Cursor, Read, Write};
    use std::process::{Command, Stdio};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::time::{Duration, Instant};

    #[test]
    fn claude_catalog_is_reported_unavailable_without_starting_codex() {
        let service = ModelCatalogService::new(None);
        assert_eq!(
            service.read("account", "/disposable/home", "claude", false),
            CatalogRead::Unavailable
        );
    }

    #[test]
    fn completion_callback_fires_once_after_refresh_result_is_stored() {
        let notifications = Arc::new(AtomicUsize::new(0));
        let observed = Arc::clone(&notifications);
        let service = ModelCatalogService::with_change_callback(
            None,
            Arc::new(move || {
                observed.fetch_add(1, Ordering::SeqCst);
            }),
        );
        let key = ("account".to_owned(), "/home".to_owned(), "codex".to_owned());
        let generation = 7;
        service
            .0
            .cache
            .lock()
            .expect("cache lock should be available")
            .insert(
                key.clone(),
                CacheEntry {
                    generation,
                    value: None,
                    expires_at: None,
                    error: false,
                    pending: true,
                },
            );

        assert!(store_result(
            &service.0,
            &key,
            generation,
            Ok(serde_json::json!({"data": []})),
        ));
        assert!(!store_result(
            &service.0,
            &key,
            generation,
            Ok(serde_json::json!({"data": []})),
        ));
        assert_eq!(notifications.load(Ordering::SeqCst), 1);
        assert!(
            service
                .0
                .cache
                .lock()
                .expect("cache lock should be available")[&key]
                .value
                .is_some()
        );
    }

    #[test]
    fn raw_stdout_limit_counts_each_frame_before_json_decoding() {
        let input = vec![b' '; MAX_CATALOG_PAGE_BYTES + 1];
        let mut reader = BoundedReader::new(Cursor::new(input));
        let mut buffer = [0_u8; 8192];
        let rejected = loop {
            match reader.read(&mut buffer) {
                Ok(0) => break false,
                Ok(_) => {}
                Err(_) => break true,
            }
        };
        assert!(rejected);
    }

    #[test]
    fn raw_stdout_limit_counts_unknown_frames_toward_total() {
        let mut input = Vec::new();
        let frame = vec![b' '; MAX_CATALOG_PAGE_BYTES - 1];
        for _ in 0..9 {
            input.extend_from_slice(&frame);
            input.push(b'\n');
        }
        let mut reader = BoundedReader::new(Cursor::new(input));
        let mut buffer = [0_u8; 8192];
        let mut rejected = false;
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(_) => {}
                Err(_) => {
                    rejected = true;
                    break;
                }
            }
        }
        assert!(rejected);
        assert!(reader.total_bytes > MAX_CATALOG_RESPONSE_BYTES);
    }

    #[cfg(unix)]
    #[test]
    fn blocked_provider_stdin_times_out_then_child_is_killed_and_reaped() {
        let mut command = Command::new("sh");
        command
            .args(["-c", "exec sleep 5"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let child = spawn_provider(command).expect("fake provider should start");
        let mut process = ChildProcess::new(child, Instant::now() + Duration::from_millis(25));
        let (_reader, mut writer) = process.split().expect("provider streams should split");
        let started = Instant::now();
        let failure = writer
            .write_all(&vec![b'x'; 8 * 1024 * 1024])
            .expect_err("the provider is not reading stdin");
        assert_eq!(failure.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < CATALOG_TIMEOUT);
        drop(writer);
        process
            .stop()
            .expect("provider group should be killed and reaped");
        assert!(process.child.is_none());
    }
}
