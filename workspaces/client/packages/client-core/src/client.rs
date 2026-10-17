//! Host-side connection to one server-scoped core. This API is shared by Tauri
//! desktop and mobile; native descriptors never cross the webview boundary.
use crate::{
    native::{self, ConnectionConfig},
    protocol::{self, Request, Response},
};
use serde_json::Value;
use std::{
    collections::HashMap,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    task::AbortHandle,
};

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Response, String>>>>>;
const CLOSED: &str = "The native client disconnected. Pending command outcomes may be unknown.";

pub struct Client {
    queue: mpsc::Sender<(Vec<u8>, OwnedSemaphorePermit)>,
    writer: AbortHandle,
    closed: Arc<AtomicBool>,
    pending: Pending,
    bytes: Arc<Semaphore>,
    reader: AbortHandle,
}
impl Client {
    pub fn open(
        config: ConnectionConfig,
        event: impl Fn(Value) + Send + Sync + 'static,
    ) -> Result<Self, String> {
        let socket = native::connect(config).map_err(|error| error.to_string())?;
        let socket = tokio::net::UnixStream::from_std(socket).map_err(|error| error.to_string())?;
        let (mut reader, mut writer) = socket.into_split();
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let responses = pending.clone();
        let closed = Arc::new(AtomicBool::new(false));
        let reader_closed = closed.clone();
        let task = tokio::spawn(async move {
            let result: Result<(), String> = async {
                let mut assembler = protocol::Assembler::default();
                loop {
                    let length =
                        reader.read_u32_le().await.map_err(|_| CLOSED.to_owned())? as usize;
                    if !(2..=protocol::MAX_FRAME).contains(&length) {
                        return Err("Invalid native response frame.".into());
                    }
                    let mut frame = vec![0; length];
                    reader
                        .read_exact(&mut frame)
                        .await
                        .map_err(|_| CLOSED.to_owned())?;
                    let Some(bytes) = assembler.push(&frame).map_err(|error| error.to_string())?
                    else {
                        continue;
                    };
                    let value: Value =
                        serde_json::from_slice(&bytes).map_err(|error| error.to_string())?;
                    if value.get("event").and_then(Value::as_str).is_some() {
                        event(value);
                        continue;
                    }
                    let response: Response =
                        serde_json::from_value(value).map_err(|error| error.to_string())?;
                    if response.result.is_some() == response.error.is_some() {
                        return Err("Invalid native response envelope.".into());
                    }
                    let sender = responses
                        .lock()
                        .map_err(|_| CLOSED)?
                        .remove(&response.id)
                        .ok_or("Unexpected native response identity.")?;
                    let _ = sender.send(Ok(response));
                }
            }
            .await;
            let error = result.err().unwrap_or_else(|| CLOSED.into());
            reader_closed.store(true, Ordering::Release);
            if let Ok(mut pending) = responses.lock() {
                for (_, sender) in pending.drain() {
                    let _ = sender.send(Err(error.clone()));
                }
            }
        });
        let (queue, mut outgoing) = mpsc::channel::<(Vec<u8>, OwnedSemaphorePermit)>(64);
        let writer_closed = closed.clone();
        let writer_pending = pending.clone();
        let reader_abort = task.abort_handle();
        let writer_task = tokio::spawn(async move {
            while let Some((encoded, _bytes)) = outgoing.recv().await {
                let send = async {
                    if writer_closed.load(Ordering::Acquire) {
                        return Err(std::io::Error::other(CLOSED));
                    }
                    for (flag, bytes) in protocol::fragments(&encoded)? {
                        writer.write_u32_le((bytes.len() + 1) as u32).await?;
                        writer.write_all(&[flag]).await?;
                        writer.write_all(bytes).await?;
                    }
                    Ok::<(), std::io::Error>(())
                }
                .await;
                if send.is_err() {
                    writer_closed.store(true, Ordering::Release);
                    reader_abort.abort();
                    if let Ok(mut pending) = writer_pending.lock() {
                        for (_, sender) in pending.drain() {
                            let _ = sender.send(Err(CLOSED.into()));
                        }
                    }
                    break;
                }
            }
        });
        Ok(Self {
            queue,
            writer: writer_task.abort_handle(),
            closed,
            pending,
            bytes: Arc::new(Semaphore::new(protocol::MAX_MESSAGE * 2)),
            reader: task.abort_handle(),
        })
    }
    pub async fn request(&self, request: Request) -> Result<Response, String> {
        if request.id.is_empty()
            || request.id.len() > 128
            || request.method.is_empty()
            || request.method.len() > 128
        {
            return Err("Invalid client request identity or method.".into());
        }
        let encoded = serde_json::to_vec(&request).map_err(|error| error.to_string())?;
        drop(protocol::fragments(&encoded).map_err(|error| error.to_string())?);
        let _bytes = self
            .bytes
            .clone()
            .try_acquire_many_owned(encoded.len() as u32)
            .map_err(|_| "The native client byte queue is full.")?;
        let (sender, receiver) = oneshot::channel();
        {
            let mut pending = self.pending.lock().map_err(|_| CLOSED)?;
            if self.closed.load(Ordering::Acquire) {
                return Err(CLOSED.into());
            }
            if pending.len() >= 64 || pending.contains_key(&request.id) {
                return Err(
                    "The native client request is already pending or its queue is full.".into(),
                );
            }
            pending.insert(request.id.clone(), sender);
        }
        // Enqueue synchronously before yielding so concurrent requests preserve
        // submission order. Cancelling a waiter never interrupts a queued frame.
        if let Err(error) = self.queue.try_send((encoded, _bytes)) {
            self.pending.lock().map_err(|_| CLOSED)?.remove(&request.id);
            return Err(match error {
                mpsc::error::TrySendError::Full(_) => "The native client queue is full.".into(),
                mpsc::error::TrySendError::Closed(_) => CLOSED.into(),
            });
        }
        receiver.await.map_err(|_| CLOSED.to_owned())?
    }
}
impl Drop for Client {
    fn drop(&mut self) {
        self.closed.store(true, Ordering::Release);
        self.reader.abort();
        self.writer.abort();
        if let Ok(mut pending) = self.pending.lock() {
            for (_, sender) in pending.drain() {
                let _ = sender.send(Err(CLOSED.into()));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    fn request(id: &str, method: &str, params: Value) -> Request {
        Request {
            id: id.into(),
            method: method.into(),
            params,
        }
    }
    fn open(directory: &std::path::Path) -> Client {
        Client::open(
            ConnectionConfig {
                state_directory: directory.into(),
                backend_origin: "http://127.0.0.1:9".into(),
            },
            |_| {},
        )
        .unwrap()
    }
    #[tokio::test]
    async fn native_connection_isolates_stores_and_preserves_write_identity() {
        let directory = tempfile::tempdir().unwrap();
        let first = open(&directory.path().join("first"));
        let second = open(&directory.path().join("second"));
        let command = || {
            request(
                "exact",
                "preferences.raw_set",
                json!({"key":"draft","raw":"Ж🦀"}),
            )
        };
        assert!(first.request(command()).await.unwrap().error.is_none());
        assert!(first.request(command()).await.unwrap().error.is_none());
        assert!(
            first
                .request(request(
                    "exact",
                    "preferences.raw_set",
                    json!({"key":"draft","raw":"changed"})
                ))
                .await
                .unwrap()
                .error
                .is_some()
        );
        assert_eq!(
            first
                .request(request("read", "preferences.raw_all", Value::Null))
                .await
                .unwrap()
                .result
                .unwrap(),
            json!({"draft":"Ж🦀"})
        );
        assert_eq!(
            second
                .request(request("read", "preferences.raw_all", Value::Null))
                .await
                .unwrap()
                .result
                .unwrap(),
            json!({})
        );
        let denied = first
            .request(request(
                "denied",
                "desktop.openExternal",
                json!({"value":"https://example.invalid/"}),
            ))
            .await
            .unwrap();
        assert!(denied.error.is_some());
    }
    #[tokio::test]
    async fn cancelled_wait_does_not_interrupt_framing_or_erase_the_commit() {
        let directory = tempfile::tempdir().unwrap();
        let client = Arc::new(open(directory.path()));
        let text = "Ж🦀".repeat(500_000);
        // Poll once to enqueue the owned write, then drop only the caller's wait.
        let mut wait = Box::pin(client.request(request(
            "cancelled-wait",
            "preferences.raw_set",
            json!({"key":"large","raw":text}),
        )));
        tokio::select! { biased; result = &mut wait => { result.unwrap(); }, _ = tokio::task::yield_now() => {} }
        drop(wait);
        let result = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            client.request(request("after", "preferences.raw_all", Value::Null)),
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(result.result.unwrap()["large"], text);
    }
}
