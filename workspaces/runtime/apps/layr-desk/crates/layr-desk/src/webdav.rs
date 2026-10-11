//! A read-only WebDAV server for the network disk: OPTIONS, PROPFIND, GET and HEAD with ranges,
//! which is what Finder needs to mount a disk without write access.

use crate::disk::{Disk, Node};
use anyhow::Result;
use bytes::Bytes;
use http_body_util::Full;
use hyper::body::Incoming;
use hyper::{Method, Request, Response, StatusCode};
use percent_encoding::{percent_decode_str, utf8_percent_encode, AsciiSet, NON_ALPHANUMERIC};
use std::sync::Arc;
use std::time::{Duration, UNIX_EPOCH};

const ALLOW: &str = "OPTIONS, GET, HEAD, PROPFIND";
/// Characters that stay as they are in an href path segment.
const SEGMENT: &AsciiSet = &NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'_')
    .remove(b'.')
    .remove(b'~');

type Reply = Response<Full<Bytes>>;

fn reply(status: StatusCode) -> Reply {
    let mut r = Response::new(Full::new(Bytes::new()));
    *r.status_mut() = status;
    r
}

fn xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

fn href(path: &str, folder: bool) -> String {
    let mut out = String::from("/");
    out.push_str(
        &path
            .split('/')
            .filter(|p| !p.is_empty())
            .map(|p| utf8_percent_encode(p, SEGMENT).to_string())
            .collect::<Vec<_>>()
            .join("/"),
    );
    if folder && !path.is_empty() {
        out.push('/');
    }
    out
}

fn http_date(time: i64) -> String {
    httpdate::fmt_http_date(UNIX_EPOCH + Duration::from_secs(time.max(0) as u64))
}

fn iso_date(time: i64) -> String {
    humantime::format_rfc3339_seconds(UNIX_EPOCH + Duration::from_secs(time.max(0) as u64))
        .to_string()
}

fn properties(path: &str, node: &Node) -> String {
    let name = path
        .rsplit('/')
        .next()
        .filter(|n| !n.is_empty())
        .unwrap_or("Desk");
    let (folder, time, rest) = match node {
        Node::Folder { time } => (true, *time, "<D:resourcetype><D:collection/></D:resourcetype>".to_string()),
        Node::File { size, etag, time, .. } => (
            false,
            *time,
            format!(
                "<D:resourcetype/><D:getcontentlength>{size}</D:getcontentlength><D:getcontenttype>application/octet-stream</D:getcontenttype><D:getetag>\"{etag}\"</D:getetag>"
            ),
        ),
    };
    format!(
        "<D:response><D:href>{}</D:href><D:propstat><D:prop><D:displayname>{}</D:displayname>{rest}<D:getlastmodified>{}</D:getlastmodified><D:creationdate>{}</D:creationdate></D:prop><D:status>HTTP/1.1 200 OK</D:status></D:propstat></D:response>",
        xml(&href(path, folder)),
        xml(name),
        http_date(time),
        iso_date(time)
    )
}

/// `start..=end` of a `Range: bytes=…` header, or the status that refuses it.
fn range(header: Option<&str>, length: usize) -> Result<Option<(usize, usize)>, ()> {
    let Some(spec) = header.and_then(|h| h.strip_prefix("bytes=")) else {
        return Ok(None);
    };
    let Some((a, b)) = spec.split_once('-') else {
        return Ok(None);
    };
    if a.is_empty() && b.is_empty() || spec.contains(',') {
        return Ok(None);
    }
    let last = length.checked_sub(1);
    let (start, end) = if a.is_empty() {
        let suffix: usize = b.parse().map_err(|_| ())?;
        (length.saturating_sub(suffix), last.ok_or(())?)
    } else {
        let start: usize = a.parse().map_err(|_| ())?;
        let end = if b.is_empty() {
            last.ok_or(())?
        } else {
            b.parse::<usize>().map_err(|_| ())?.min(last.ok_or(())?)
        };
        (start, end)
    };
    if start > end {
        return Err(());
    }
    Ok(Some((start, end)))
}

pub async fn handle(
    disk: Arc<Disk>,
    request: Request<Incoming>,
) -> Result<Reply, std::convert::Infallible> {
    Ok(match serve(&disk, &request).await {
        Ok(r) => r,
        Err(e) => {
            let mut r = Response::new(Full::new(Bytes::from(format!("{e:#}\n"))));
            *r.status_mut() = StatusCode::INTERNAL_SERVER_ERROR;
            r
        }
    })
}

async fn serve(disk: &Arc<Disk>, request: &Request<Incoming>) -> Result<Reply> {
    let raw = request.uri().path();
    let path = percent_decode_str(raw)
        .decode_utf8()?
        .trim_matches('/')
        .to_string();
    let method = request.method().as_str();
    if method == "OPTIONS" {
        return Ok(Response::builder()
            .header("DAV", "1")
            .header("Allow", ALLOW)
            .header("MS-Author-Via", "DAV")
            .body(Full::new(Bytes::new()))?);
    }
    if !matches!(method, "PROPFIND" | "GET" | "HEAD") {
        // Read only: Finder mounts the disk without write access.
        return Ok(Response::builder()
            .status(StatusCode::METHOD_NOT_ALLOWED)
            .header("Allow", ALLOW)
            .body(Full::new(Bytes::new()))?);
    }
    let Some((node, children)) = disk.lookup(&path).await? else {
        return Ok(reply(StatusCode::NOT_FOUND));
    };
    if method == "PROPFIND" {
        let depth_zero = request.headers().get("depth").and_then(|d| d.to_str().ok()) == Some("0");
        let mut body = String::from(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><D:multistatus xmlns:D=\"DAV:\">",
        );
        body.push_str(&properties(&path, &node));
        if !depth_zero {
            for (child, child_node) in &children {
                body.push_str(&properties(child, child_node));
            }
        }
        body.push_str("</D:multistatus>");
        return Ok(Response::builder()
            .status(StatusCode::MULTI_STATUS)
            .header("content-type", "application/xml; charset=utf-8")
            .body(Full::new(Bytes::from(body)))?);
    }
    let Node::File {
        etag,
        time,
        content,
        ..
    } = node
    else {
        return Ok(reply(StatusCode::METHOD_NOT_ALLOWED));
    };
    let data = disk.read(&content).await?;
    let header = request.headers().get("range").and_then(|r| r.to_str().ok());
    let (status, start, end) = match range(header, data.len()) {
        Err(()) => {
            return Ok(Response::builder()
                .status(StatusCode::RANGE_NOT_SATISFIABLE)
                .header("content-range", format!("bytes */{}", data.len()))
                .body(Full::new(Bytes::new()))?)
        }
        Ok(Some((s, e))) => (StatusCode::PARTIAL_CONTENT, s, e + 1),
        Ok(None) => (StatusCode::OK, 0, data.len()),
    };
    let mut response = Response::builder()
        .status(status)
        .header("accept-ranges", "bytes")
        .header("etag", format!("\"{etag}\""))
        .header("last-modified", http_date(time))
        .header("content-type", "application/octet-stream")
        .header("content-length", end - start);
    if status == StatusCode::PARTIAL_CONTENT {
        response = response.header(
            "content-range",
            format!("bytes {start}-{}/{}", end - 1, data.len()),
        );
    }
    let body = if request.method() == Method::HEAD {
        Bytes::new()
    } else {
        Bytes::copy_from_slice(&data[start..end])
    };
    Ok(response.body(Full::new(body))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_follow_rfc_9110() {
        assert_eq!(range(None, 10), Ok(None));
        assert_eq!(range(Some("bytes=0-3"), 10), Ok(Some((0, 3))));
        assert_eq!(range(Some("bytes=5-"), 10), Ok(Some((5, 9))));
        assert_eq!(range(Some("bytes=-4"), 10), Ok(Some((6, 9))));
        assert_eq!(range(Some("bytes=8-100"), 10), Ok(Some((8, 9))));
        assert_eq!(range(Some("bytes=12-14"), 10), Err(()));
        assert_eq!(range(Some("bytes=0-0"), 0), Err(()));
    }

    #[test]
    fn hrefs_encode_each_segment() {
        assert_eq!(href("", true), "/");
        assert_eq!(href("proj/a b/ü.txt", false), "/proj/a%20b/%C3%BC.txt");
        assert_eq!(href("proj/src", true), "/proj/src/");
    }
}
