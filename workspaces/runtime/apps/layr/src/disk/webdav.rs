//! Read-only WebDAV. Adapted from Layr Desk, using the shared HTTP parser.

use super::{Node, View};
use crate::http::{self, Request};
use anyhow::Result;
use std::io::Write;

const ALLOW: &str = "OPTIONS, GET, HEAD, PROPFIND";

pub(super) fn xml(text: &str) -> String {
    text.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;").replace('"', "&quot;").replace('\'', "&apos;")
}

fn href(path: &str, folder: bool) -> String {
    let mut out = String::from("/");
    for byte in path.bytes() {
        if byte.is_ascii_alphanumeric() || b"/-_.~".contains(&byte) {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    if folder && !path.is_empty() {
        out.push('/');
    }
    out
}

fn path(raw: &str) -> Option<String> {
    let raw = raw.split('?').next()?;
    let mut out = Vec::new();
    let mut bytes = raw.bytes();
    while let Some(byte) = bytes.next() {
        out.push(if byte == b'%' {
            let a = (bytes.next()? as char).to_digit(16)?;
            let b = (bytes.next()? as char).to_digit(16)?;
            ((a << 4) | b) as u8
        } else {
            byte
        });
    }
    let path = String::from_utf8(out).ok()?.trim_matches('/').to_owned();
    if path.chars().any(|c| c.is_control() || c == '\\') || path.split('/').any(|p| p == "." || p == "..") {
        return None;
    }
    Some(path)
}

fn date(time: i64) -> String {
    chrono::DateTime::from_timestamp(time.max(0), 0).unwrap_or_default().format("%a, %d %b %Y %H:%M:%S GMT").to_string()
}

fn properties(path: &str, node: &Node) -> String {
    let name = path.rsplit('/').next().filter(|s| !s.is_empty()).unwrap_or("Layr");
    let (folder, time, rest) = match node {
        Node::Folder { time } => (true, *time, "<D:resourcetype><D:collection/></D:resourcetype>".into()),
        Node::File { size, etag, time, .. } => (false, *time, format!("<D:resourcetype/><D:getcontentlength>{size}</D:getcontentlength><D:getcontenttype>application/octet-stream</D:getcontenttype><D:getetag>&quot;{etag}&quot;</D:getetag>")),
    };
    let created = chrono::DateTime::from_timestamp(time.max(0), 0).unwrap_or_default().to_rfc3339();
    format!("<D:response><D:href>{}</D:href><D:propstat><D:prop><D:displayname>{}</D:displayname>{rest}<D:getlastmodified>{}</D:getlastmodified><D:creationdate>{created}</D:creationdate></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>", xml(&href(path, folder)), xml(name), date(time))
}

pub(super) fn serve(disk: &dyn View, request: &Request, stream: &mut impl Write) -> Result<()> {
    let headers = vec![("Allow".into(), ALLOW.into()), ("DAV".into(), "1".into()), ("MS-Author-Via".into(), "DAV".into())];
    if request.method == "OPTIONS" {
        http::response(stream, 200, &headers, b"")?;
        return Ok(());
    }
    if !matches!(request.method.as_str(), "GET" | "HEAD" | "PROPFIND") {
        http::response(stream, 405, &headers, b"")?;
        return Ok(());
    }
    let Some(path) = path(&request.target) else {
        http::response(stream, 400, &[], b"")?;
        return Ok(());
    };
    let Some((node, children)) = disk.lookup(&path) else {
        http::response(stream, if disk.locked(&path) { 423 } else { 404 }, &[], b"")?;
        return Ok(());
    };
    if request.method == "PROPFIND" {
        let depth = request.header("depth").unwrap_or("1");
        if !matches!(depth, "0" | "1") {
            http::response(stream, 403, &[], b"")?;
            return Ok(());
        }
        let mut body = String::from("<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">");
        body.push_str(&properties(&path, &node));
        if depth == "1" {
            for (path, node) in children {
                body.push_str(&properties(&path, &node));
            }
        }
        body.push_str("</D:multistatus>");
        http::response(stream, 207, &[("Content-Type".into(), "application/xml; charset=utf-8".into())], body.as_bytes())?;
        return Ok(());
    }
    let Node::File { size, etag, time, content } = node else {
        http::response(stream, 405, &headers, b"")?;
        return Ok(());
    };
    if disk.locked(&path) {
        http::response(stream, 423, &[], b"")?;
        return Ok(());
    }
    let etag = format!("\"{etag}\"");
    let mut headers = vec![
        ("Accept-Ranges".into(), "bytes".into()),
        ("ETag".into(), etag.clone()),
        ("Last-Modified".into(), date(time)),
        ("Content-Type".into(), "application/octet-stream".into()),
    ];
    if request.header("if-none-match") == Some(etag.as_str()) {
        http::response(stream, 304, &headers, b"")?;
        return Ok(());
    }
    let range = request.header("range").map(http::Range::parse).transpose();
    let interval = match range {
        Ok(None) => None,
        Ok(Some(range)) => match range.resolve(size) {
            Some(interval) => Some(interval),
            None => {
                http::response(stream, 416, &[("Content-Range".into(), format!("bytes */{size}"))], b"")?;
                return Ok(());
            }
        },
        Err(_) => {
            http::response(stream, 416, &[("Content-Range".into(), format!("bytes */{size}"))], b"")?;
            return Ok(());
        }
    };
    let (status, start, end) = interval.map(|(start, end)| (206, start, end + 1)).unwrap_or((200, 0, size));
    if status == 206 {
        headers.push(("Content-Range".into(), format!("bytes {start}-{}/{size}", end - 1)));
    }
    if request.method == "HEAD" {
        http::headers(stream, status, end - start, &headers)?;
        stream.flush()?;
        return Ok(());
    }
    let bytes = match disk.read(&path, &content) {
        Ok(bytes) => bytes,
        Err(_) if disk.locked(&path) => {
            http::response(stream, 423, &[], b"")?;
            return Ok(());
        }
        Err(e) => return Err(e),
    };
    let payload = &bytes[usize::try_from(start)?..usize::try_from(end)?];
    http::response(stream, status, &headers, payload)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn paths_and_hrefs_preserve_encoded_line_names() {
        assert_eq!(href("a%2Fb/f ü.txt", false), "/a%252Fb/f%20%C3%BC.txt");
        assert_eq!(path("/a%252Fb/f%20%C3%BC.txt").as_deref(), Some("a%2Fb/f ü.txt"));
        for raw in ["/%2E%2E/etc", "/a/../etc", "/%00", "/a%5cb", "/bad%q2"] {
            assert!(path(raw).is_none());
        }
    }
}
