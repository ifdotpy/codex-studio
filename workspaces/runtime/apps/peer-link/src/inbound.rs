//! Requests from paired servers: each QUIC stream becomes one local HTTP request to the
//! backend, with the same forwarding headers that Tailscale Serve sets. The backend checks
//! the request signature, the receipt and the Tailscale owner as for a Serve request.

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;

use quinn::{Connection, Endpoint, RecvStream, SendStream, VarInt};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::Links;
use crate::frame::{self, RequestHead, ResponseHead};

pub struct Inbound {
    pub backend_port: u16,
    pub forwarded_host: String,
    pub loopback: bool,
}

/// Tailscale addresses (CGNAT range and the Tailscale ULA prefix). The backend runs whois on it.
pub fn tailnet(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(v4) => {
            let octets = v4.octets();
            octets[0] == 100 && (64..128).contains(&octets[1])
        }
        IpAddr::V6(v6) => {
            let segments = v6.segments();
            segments[0] == 0xfd7a && segments[1] == 0x115c && segments[2] == 0xa1e0
        }
    }
}

pub async fn serve(endpoint: Endpoint, inbound: Arc<Inbound>, links: Arc<Links>) {
    while let Some(incoming) = endpoint.accept().await {
        let source = incoming.remote_address().ip();
        if !(tailnet(source) || (inbound.loopback && source.is_loopback())) {
            incoming.refuse();
            continue;
        }
        let inbound = inbound.clone();
        let links = links.clone();
        tokio::spawn(async move {
            let Ok(connection) = incoming.await else {
                return;
            };
            links.track_inbound(&connection);
            accept_streams(connection, inbound).await;
        });
    }
}

async fn accept_streams(connection: Connection, inbound: Arc<Inbound>) {
    let peer = connection.remote_address().ip();
    while let Ok((mut send, recv)) = connection.accept_bi().await {
        let inbound = inbound.clone();
        tokio::spawn(async move {
            if forward(&mut send, recv, peer, &inbound).await.is_err() {
                // The client sees a reset stream: the outcome of a sent request is unknown.
                send.reset(VarInt::from_u32(1)).ok();
            }
        });
    }
}

async fn forward(
    send: &mut SendStream,
    mut recv: RecvStream,
    peer: IpAddr,
    inbound: &Inbound,
) -> Result<(), String> {
    let head: RequestHead = frame::read(&mut recv)
        .await
        .map_err(|error| error.to_string())?;
    head.validate()?;
    let mut backend = TcpStream::connect((Ipv4Addr::LOCALHOST, inbound.backend_port))
        .await
        .map_err(|error| error.to_string())?;
    let mut text = format!("{} {} HTTP/1.0\r\n", head.method, head.path);
    for (name, value) in head
        .headers
        .iter()
        .filter(|(name, _)| !frame::link_header(name))
    {
        text.push_str(&format!("{name}: {value}\r\n"));
    }
    text.push_str(&format!(
        "Host: 127.0.0.1:{}\r\nX-Forwarded-Host: {}\r\nX-Forwarded-Proto: https\r\nX-Forwarded-For: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        inbound.backend_port, inbound.forwarded_host, peer, head.body_length
    ));
    backend
        .write_all(text.as_bytes())
        .await
        .map_err(|error| error.to_string())?;
    // A short body leaves the backend request incomplete, so it does not run.
    let copied = tokio::io::copy(&mut (&mut recv).take(head.body_length), &mut backend)
        .await
        .map_err(|error| error.to_string())?;
    if copied != head.body_length {
        return Err("The request body is incomplete".to_owned());
    }
    let (status, headers, rest) = read_response_head(&mut backend).await?;
    let expected = frame::content_length(&headers);
    frame::write(send, &ResponseHead { status, headers })
        .await
        .map_err(|error| error.to_string())?;
    send.write_all(&rest)
        .await
        .map_err(|error| error.to_string())?;
    let more = tokio::io::copy(&mut backend, send)
        .await
        .map_err(|error| error.to_string())?;
    if expected.is_some_and(|length| length != rest.len() as u64 + more) {
        return Err("The backend response is incomplete".to_owned());
    }
    send.finish().map_err(|error| error.to_string())
}

/// The status line and headers of the backend's HTTP/1 response, and the body bytes read after them.
async fn read_response_head<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<(u16, Vec<(String, String)>, Vec<u8>), String> {
    let mut buffer = Vec::with_capacity(4096);
    let end = loop {
        if let Some(position) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
            break position;
        }
        if buffer.len() > frame::MAX_HEAD {
            return Err("The backend response head is too large".to_owned());
        }
        let mut chunk = [0; 8192];
        let count = reader
            .read(&mut chunk)
            .await
            .map_err(|error| error.to_string())?;
        if count == 0 {
            return Err("The backend closed the connection".to_owned());
        }
        buffer.extend_from_slice(&chunk[..count]);
    };
    let text = std::str::from_utf8(&buffer[..end])
        .map_err(|_| "The backend response head is not text".to_owned())?;
    let mut lines = text.split("\r\n");
    let status = lines
        .next()
        .and_then(|line| line.split(' ').nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .filter(|code| (100..600).contains(code))
        .ok_or_else(|| "The backend status line is invalid".to_owned())?;
    let headers = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_owned(), value.trim().to_owned()))
        .collect();
    Ok((status, headers, buffer[end + 4..].to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_tailnet_sources() {
        assert!(tailnet(
            "100.75.160.63"
                .parse()
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        ));
        assert!(tailnet(
            "fd7a:115c:a1e0::1"
                .parse()
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        ));
        assert!(!tailnet(
            "100.128.0.1"
                .parse()
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        ));
        assert!(!tailnet(
            "192.168.1.152"
                .parse()
                .unwrap_or(IpAddr::V4(Ipv4Addr::UNSPECIFIED))
        ));
    }

    #[tokio::test]
    async fn parses_the_backend_head_and_keeps_the_body_start() {
        let mut response: &[u8] = b"HTTP/1.1 201 Created\r\ncontent-type: application/json\r\ncontent-length: 4\r\n\r\n{\"a\"";
        let (status, headers, rest) = read_response_head(&mut response).await.unwrap_or_default();
        assert_eq!(status, 201);
        assert_eq!(frame::content_length(&headers), Some(4));
        assert_eq!(rest, b"{\"a\"");
    }
}
