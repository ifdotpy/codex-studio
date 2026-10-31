//! The request and response heads of one QUIC stream (docs/peer-link.md).

use std::io;

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const MAX_HEAD: usize = 64 * 1024;
pub const MAX_BODY: u64 = 64 * 1024 * 1024;
const MAX_HEADERS: usize = 64;
const METHODS: [&str; 5] = ["GET", "POST", "PUT", "PATCH", "DELETE"];

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct RequestHead {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body_length: u64,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, message.to_owned())
}

pub async fn write<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    value: &T,
) -> io::Result<()> {
    let data = serde_json::to_vec(value).map_err(|_| invalid("The head cannot be encoded"))?;
    let length = u32::try_from(data.len())
        .ok()
        .filter(|length| *length as usize <= MAX_HEAD);
    writer
        .write_u32(length.ok_or_else(|| invalid("The head is too large"))?)
        .await?;
    writer.write_all(&data).await
}

pub async fn read<R: AsyncRead + Unpin, T: DeserializeOwned>(reader: &mut R) -> io::Result<T> {
    let length = reader.read_u32().await? as usize;
    if length > MAX_HEAD {
        return Err(invalid("The head is too large"));
    }
    let mut data = vec![0; length];
    reader.read_exact(&mut data).await?;
    serde_json::from_slice(&data).map_err(|_| invalid("The head is invalid"))
}

fn token(name: &str) -> bool {
    (1..=64).contains(&name.len())
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
}

/// The link sets these itself. A peer cannot supply them.
pub fn link_header(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    matches!(
        name.as_str(),
        "host"
            | "connection"
            | "content-length"
            | "transfer-encoding"
            | "keep-alive"
            | "upgrade"
            | "te"
            | "trailer"
            | "forwarded"
            | "x-real-ip"
    ) || name.starts_with("x-forwarded-")
        || name.starts_with("proxy-")
        || name.starts_with("tailscale-")
}

impl RequestHead {
    pub fn validate(&self) -> Result<(), String> {
        if !METHODS.contains(&self.method.as_str()) {
            return Err("The method is not supported".to_owned());
        }
        if !self.path.starts_with("/api/")
            || self.path.len() > 8192
            || !self.path.bytes().all(|byte| (0x21..0x7f).contains(&byte))
        {
            return Err("The path is invalid".to_owned());
        }
        if self.headers.len() > MAX_HEADERS
            || self.headers.iter().any(|(name, value)| {
                !token(name)
                    || value.len() > 4096
                    || value
                        .bytes()
                        .any(|byte| byte == b'\r' || byte == b'\n' || byte == 0)
            })
        {
            return Err("The headers are invalid".to_owned());
        }
        if self.body_length > MAX_BODY || (self.method == "GET" && self.body_length != 0) {
            return Err("The body length is invalid".to_owned());
        }
        Ok(())
    }
}

pub fn content_length(headers: &[(String, String)]) -> Option<u64> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, value)| value.trim().parse().ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn head(method: &str, path: &str, headers: &[(&str, &str)], body_length: u64) -> RequestHead {
        RequestHead {
            method: method.to_owned(),
            path: path.to_owned(),
            headers: headers
                .iter()
                .map(|(name, value)| ((*name).to_owned(), (*value).to_owned()))
                .collect(),
            body_length,
        }
    }

    #[test]
    fn accepts_a_signed_api_request() {
        assert!(
            head(
                "POST",
                "/api/servers/orchestration",
                &[("X-Studio-Signature", "abc")],
                10
            )
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn refuses_other_paths_methods_and_header_injection() {
        assert!(head("POST", "/admin", &[], 0).validate().is_err());
        assert!(head("CONNECT", "/api/x", &[], 0).validate().is_err());
        assert!(head("GET", "/api/x y", &[], 0).validate().is_err());
        assert!(
            head("GET", "/api/x", &[("X-A", "1\r\nHost: evil")], 0)
                .validate()
                .is_err()
        );
        assert!(
            head("GET", "/api/x", &[("X A", "1")], 0)
                .validate()
                .is_err()
        );
        assert!(head("GET", "/api/x", &[], 1).validate().is_err());
        assert!(
            head("POST", "/api/x", &[], MAX_BODY + 1)
                .validate()
                .is_err()
        );
    }

    #[test]
    fn the_link_owns_the_forwarding_headers() {
        for name in [
            "Host",
            "X-Forwarded-For",
            "x-forwarded-host",
            "Tailscale-User-Login",
            "Content-Length",
            "Connection",
        ] {
            assert!(link_header(name), "{name}");
        }
        assert!(!link_header("X-Studio-Signature"));
        assert!(!link_header("Content-Type"));
    }
}
