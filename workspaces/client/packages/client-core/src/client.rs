//! Host-side connection to one server-scoped core. This API is shared by Tauri
//! desktop and mobile; native descriptors never cross the webview boundary.
use crate::{
    native::{self, ConnectionConfig},
    protocol::{self, NativeFrame, NativeResponse, Request},
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

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<NativeResponse, String>>>>>;
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
        Ok(Self::from_socket(socket, event))
    }

    fn from_socket(
        socket: tokio::net::UnixStream,
        event: impl Fn(Value) + Send + Sync + 'static,
    ) -> Self {
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
                    let response = match serde_json::from_slice::<NativeFrame>(&bytes)
                        .map_err(|error| error.to_string())?
                    {
                        NativeFrame::Event(value) => {
                            event(value);
                            continue;
                        }
                        NativeFrame::Response(response) => response,
                    };
                    let sender = responses
                        .lock()
                        .map_err(|_| CLOSED)?
                        .remove(&response.id)
                        .ok_or("Unexpected native response identity.")?;
                    drop(sender.send(Ok(response)));
                }
            }
            .await;
            let error = result.err().unwrap_or_else(|| CLOSED.into());
            reader_closed.store(true, Ordering::Release);
            if let Ok(mut pending) = responses.lock() {
                for (_, sender) in pending.drain() {
                    drop(sender.send(Err(error.clone())));
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
                            drop(sender.send(Err(CLOSED.into())));
                        }
                    }
                    break;
                }
            }
        });
        Self {
            queue,
            writer: writer_task.abort_handle(),
            closed,
            pending,
            bytes: Arc::new(Semaphore::new(protocol::MAX_MESSAGE * 2)),
            reader: task.abort_handle(),
        }
    }
    pub async fn request(&self, request: Request) -> Result<NativeResponse, String> {
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
                drop(sender.send(Err(CLOSED.into())));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::NativeOutcome;
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
    async fn reader_prioritizes_event_and_rejects_unknown_response_identity() {
        let (socket, mut peer) = tokio::net::UnixStream::pair().unwrap();
        let (events, mut seen) = tokio::sync::mpsc::unbounded_channel();
        let client = Arc::new(Client::from_socket(socket, move |event| {
            events.send(event).unwrap();
        }));
        let request_client = client.clone();
        let waiter = tokio::spawn(async move {
            request_client
                .request(request("expected", "test", Value::Null))
                .await
        });
        let length = peer.read_u32_le().await.unwrap() as usize;
        let mut request_frame = vec![0; length];
        peer.read_exact(&mut request_frame).await.unwrap();
        let event =
            json!({"event":"changed", "id":"unexpected", "result":null, "payload":[1,null]});
        for value in [event.clone(), json!({"id":"unexpected", "result":null})] {
            let encoded = serde_json::to_vec(&value).unwrap();
            peer.write_u32_le((encoded.len() + 1) as u32).await.unwrap();
            peer.write_u8(0).await.unwrap();
            peer.write_all(&encoded).await.unwrap();
        }
        assert_eq!(seen.recv().await.unwrap(), event);
        assert_eq!(
            waiter.await.unwrap().unwrap_err(),
            "Unexpected native response identity."
        );
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
        assert!(matches!(
            first.request(command()).await.unwrap().outcome,
            NativeOutcome::Success(_)
        ));
        assert!(matches!(
            first.request(command()).await.unwrap().outcome,
            NativeOutcome::Success(_)
        ));
        assert!(matches!(
            first
                .request(request(
                    "exact",
                    "preferences.raw_set",
                    json!({"key":"draft","raw":"changed"})
                ))
                .await
                .unwrap()
                .outcome,
            NativeOutcome::Failure(_)
        ));
        assert!(
            matches!(first.request(request("read", "preferences.raw_all", Value::Null)).await.unwrap().outcome, NativeOutcome::Success(value) if value == json!({"draft":"Ж🦀"}))
        );
        assert!(
            matches!(second.request(request("read", "preferences.raw_all", Value::Null)).await.unwrap().outcome, NativeOutcome::Success(value) if value == json!({}))
        );
        let denied = first
            .request(request(
                "denied",
                "desktop.openExternal",
                json!({"value":"https://example.invalid/"}),
            ))
            .await
            .unwrap();
        assert!(matches!(
            denied.outcome,
            crate::protocol::NativeOutcome::Failure(_)
        ));
    }
    #[tokio::test]
    async fn cancelled_wait_does_not_interrupt_framing_or_erase_the_commit() {
        let directory = tempfile::tempdir().unwrap();
        let client = Arc::new(open(directory.path()));
        let symbol = "Ж🦀";
        let text = symbol.repeat(protocol::MAX_FRAME / symbol.len() + 1);
        let command = request(
            "cancelled-wait",
            "preferences.raw_set",
            json!({"key":"large","raw":text}),
        );
        assert!(
            protocol::fragments(&serde_json::to_vec(&command).unwrap())
                .unwrap()
                .count()
                > 1,
            "the cancelled write must span multiple transport frames"
        );
        // Enqueue without yielding to the writer, then cancel only the caller's wait.
        let mut wait = Box::pin(client.request(command));
        std::future::poll_fn(|context| {
            assert!(std::future::Future::poll(wait.as_mut(), context).is_pending());
            std::task::Poll::Ready(())
        })
        .await;
        drop(wait);
        // The suite's hang guard bounds this correctness check; it is not a latency test.
        let result = client
            .request(request("after", "preferences.raw_all", Value::Null))
            .await
            .unwrap();
        assert!(matches!(result.outcome, NativeOutcome::Success(value) if value["large"] == text));
    }
}
