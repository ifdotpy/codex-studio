//! Requests from the local backend over the Unix socket. One connection carries one request:
//!
//! - in: a JSON head line, then `bodyLength` body bytes;
//! - out: a JSON line `{"status","headers"}`, then body chunks (u32 length and data) and a
//!   zero length at the end; or a JSON line `{"failure","sent"}`.
//!
//! `sent: false` means the paired server cannot have run the request. A connection that closes
//! without the zero length means the response was lost.

use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWrite, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::Links;
use crate::frame::{self, RequestHead, ResponseHead};

const MAX_TIMEOUT_SECONDS: f64 = 3600.0;
const CHUNK: usize = 256 * 1024;

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct OutboundHead {
    address: SocketAddr,
    public_key: String,
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body_length: u64,
    timeout: f64,
}

#[derive(Default)]
struct Progress {
    sent: bool,
    replied: bool,
}

pub async fn listen(listener: UnixListener, links: Arc<Links>) {
    while let Ok((stream, _)) = listener.accept().await {
        let links = links.clone();
        tokio::spawn(async move { handle(stream, links).await });
    }
}

async fn reply<W: AsyncWrite + Unpin>(writer: &mut W, value: &serde_json::Value) {
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    writer.write_all(&line).await.ok();
    writer.flush().await.ok();
}

async fn handle(stream: UnixStream, links: Arc<Links>) {
    let (reader, mut writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut line = Vec::new();
    let read = (&mut reader)
        .take(frame::MAX_HEAD as u64 + 1)
        .read_until(b'\n', &mut line)
        .await;
    let head = match read {
        Ok(_) if line.ends_with(b"\n") => serde_json::from_slice::<OutboundHead>(&line).ok(),
        _ => None,
    };
    let Some(head) = head.filter(|head| head.timeout > 0.0 && head.timeout <= MAX_TIMEOUT_SECONDS)
    else {
        reply(&mut writer, &json!({"failure": "invalid", "sent": false})).await;
        return;
    };
    let mut progress = Progress::default();
    let deadline = Duration::from_secs_f64(head.timeout);
    let outcome = tokio::time::timeout(
        deadline,
        exchange(&mut reader, &mut writer, head, &links, &mut progress),
    )
    .await;
    let failure = match outcome {
        Ok(Ok(())) => return,
        Ok(Err(failure)) => failure,
        Err(_) => "timeout",
    };
    if !progress.replied {
        reply(
            &mut writer,
            &json!({"failure": failure, "sent": progress.sent}),
        )
        .await;
    }
}

async fn exchange<R, W>(
    reader: &mut R,
    writer: &mut W,
    head: OutboundHead,
    links: &Links,
    progress: &mut Progress,
) -> Result<(), &'static str>
where
    R: tokio::io::AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let request = RequestHead {
        method: head.method,
        path: head.path,
        headers: head.headers,
        body_length: head.body_length,
    };
    request.validate().map_err(|_| "invalid")?;
    let key = crate::tls::spki(&head.public_key).map_err(|_| "invalid")?;
    let connection = links
        .connect(head.address, key)
        .await
        .map_err(|_| "unreachable")?;
    let (mut send, mut recv) = connection.open_bi().await.map_err(|_| "unreachable")?;
    frame::write(&mut send, &request)
        .await
        .map_err(|_| "unreachable")?;
    let copied = tokio::io::copy(&mut (&mut *reader).take(request.body_length), &mut send)
        .await
        .map_err(|_| "invalid")?;
    if copied != request.body_length {
        // The paired server never forwards an incomplete body.
        send.reset(quinn::VarInt::from_u32(1)).ok();
        return Err("invalid");
    }
    progress.sent = true;
    let response = match send.finish() {
        Ok(()) => frame::read::<_, ResponseHead>(&mut recv).await.ok(),
        Err(_) => None,
    };
    let Some(response) = response else {
        // TLS 1.3 lets the client send before the server checks the client key. A server that
        // refused the key closed the handshake, so it ran nothing.
        if refused(&connection) {
            progress.sent = false;
            return Err("refused");
        }
        return Err("lost");
    };
    reply(
        writer,
        &json!({"status": response.status, "headers": response.headers}),
    )
    .await;
    progress.replied = true;
    let mut buffer = vec![0; CHUNK];
    loop {
        let count = match recv.read(&mut buffer).await.map_err(|_| "lost")? {
            Some(0) => continue,
            Some(count) => count,
            None => break,
        };
        writer.write_u32(count as u32).await.map_err(|_| "lost")?;
        writer
            .write_all(&buffer[..count])
            .await
            .map_err(|_| "lost")?;
    }
    writer.write_u32(0).await.map_err(|_| "lost")?;
    writer.flush().await.map_err(|_| "lost")
}

fn refused(connection: &quinn::Connection) -> bool {
    match connection.close_reason() {
        Some(quinn::ConnectionError::ConnectionClosed(close)) => {
            (0x100..0x200).contains(&u64::from(close.error_code))
        }
        _ => false,
    }
}

pub fn bind(path: &Path) -> std::io::Result<UnixListener> {
    match std::fs::remove_file(path) {
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error),
        _ => {}
    }
    let listener = UnixListener::bind(path)?;
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(listener)
}
