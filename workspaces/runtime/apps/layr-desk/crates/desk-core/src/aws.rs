//! AWS Signature Version 4 and the XML of S3 and STS replies: the pure part of a small AWS client
//! (no SDK, no I/O), for the gateway and the writer.

use hmac::{Hmac, Mac};
use sha2::{Digest, Sha256};

/// The SHA-256 of an empty payload.
pub const EMPTY_SHA256: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

#[derive(Clone, PartialEq, Eq)]
pub struct Credentials {
    pub key_id: String,
    pub secret: String,
    /// The session token of temporary credentials.
    pub token: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("key_id", &self.key_id)
            .finish_non_exhaustive()
    }
}

/// One request to sign. `path` is not encoded yet; `headers` are the headers to sign besides
/// host, x-amz-date, x-amz-security-token and (for S3) x-amz-content-sha256, which `sign` adds.
pub struct Request<'a> {
    pub method: &'a str,
    /// The Host header: the name, and the port when it is not the default one.
    pub host: &'a str,
    pub path: &'a str,
    pub query: &'a [(&'a str, &'a str)],
    pub headers: &'a [(&'a str, &'a str)],
    pub payload_sha256: &'a str,
}

/// Percent-encoding as SigV4 wants it: everything but `A-Z a-z 0-9 - _ . ~` (and `/` when
/// `keep_slash`).
pub fn uri_encode(text: &str, keep_slash: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric()
            || matches!(byte, b'-' | b'_' | b'.' | b'~')
            || (keep_slash && byte == b'/')
        {
            out.push(byte as char);
        } else {
            out.push_str(&format!("%{byte:02X}"));
        }
    }
    out
}

/// The query as SigV4 signs it, sorted by name; the URL uses the same text.
pub fn query_string(query: &[(&str, &str)]) -> String {
    let mut pairs: Vec<(String, String)> = query
        .iter()
        .map(|(k, v)| (uri_encode(k, false), uri_encode(v, false)))
        .collect();
    pairs.sort();
    pairs
        .iter()
        .map(|(k, v)| format!("{k}={v}"))
        .collect::<Vec<_>>()
        .join("&")
}

/// `YYYYMMDDTHHMMSSZ` of a Unix time.
pub fn amz_date(unix: u64) -> String {
    let (days, rest) = (unix / 86_400, unix % 86_400);
    // Howard Hinnant's civil_from_days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}{month:02}{day:02}T{:02}{:02}{:02}Z",
        rest / 3600,
        rest / 60 % 60,
        rest % 60
    )
}

fn hmac(key: &[u8], data: &str) -> Vec<u8> {
    let mut mac = Hmac::<Sha256>::new_from_slice(key).expect("HMAC takes any key");
    mac.update(data.as_bytes());
    mac.finalize().into_bytes().to_vec()
}

/// The headers that the signature adds to `request` at Unix time `unix`, the Authorization
/// header last. Send them together with the headers of the request, which they do not repeat:
/// a header sent twice reaches AWS as one header with both values and breaks the signature.
pub fn sign(
    request: &Request,
    credentials: &Credentials,
    region: &str,
    service: &str,
    unix: u64,
) -> Vec<(String, String)> {
    let stamp = amz_date(unix);
    let day = &stamp[..8];
    let mut signed: Vec<(String, String)> = request
        .headers
        .iter()
        .map(|(k, v)| (k.to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    signed.push(("host".into(), request.host.to_string()));
    signed.push(("x-amz-date".into(), stamp.clone()));
    if service == "s3" {
        signed.push((
            "x-amz-content-sha256".into(),
            request.payload_sha256.to_string(),
        ));
    }
    if let Some(token) = &credentials.token {
        signed.push(("x-amz-security-token".into(), token.clone()));
    }
    signed.sort();
    let names = signed
        .iter()
        .map(|(k, _)| k.as_str())
        .collect::<Vec<_>>()
        .join(";");
    let canonical_headers: String = signed.iter().map(|(k, v)| format!("{k}:{v}\n")).collect();
    let canonical = [
        request.method,
        &uri_encode(request.path, true),
        &query_string(request.query),
        &canonical_headers,
        &names,
        request.payload_sha256,
    ]
    .join("\n");
    let scope = format!("{day}/{region}/{service}/aws4_request");
    let to_sign = format!(
        "AWS4-HMAC-SHA256\n{stamp}\n{scope}\n{}",
        hex::encode(Sha256::digest(canonical.as_bytes()))
    );
    let mut key = format!("AWS4{}", credentials.secret).into_bytes();
    for part in [day, region, service, "aws4_request"] {
        key = hmac(&key, part);
    }
    let signature = hex::encode(hmac(&key, &to_sign));
    let own: Vec<String> = request
        .headers
        .iter()
        .map(|(k, _)| k.to_ascii_lowercase())
        .collect();
    signed.retain(|(k, _)| k != "host" && !own.contains(k));
    signed.push((
        "authorization".into(),
        format!(
            "AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={names}, Signature={signature}",
            credentials.key_id
        ),
    ));
    signed
}

/// The base URL (scheme, host, port) and the path, not encoded yet, of an S3 key: virtual-hosted
/// at the dual-stack endpoint of the region, so an IPv6-only host reaches it; path-style on a
/// test endpoint and for a bucket name with a dot, which the certificate of that host does not
/// cover.
pub fn s3_location(
    bucket: &str,
    region: &str,
    endpoint: Option<&str>,
    key: &str,
) -> (String, String) {
    match endpoint {
        Some(endpoint) => (
            endpoint.trim_end_matches('/').to_string(),
            format!("/{bucket}/{key}"),
        ),
        None if bucket.contains('.') => (
            format!("https://s3.dualstack.{region}.amazonaws.com"),
            format!("/{bucket}/{key}"),
        ),
        None => (
            format!("https://{bucket}.s3.dualstack.{region}.amazonaws.com"),
            format!("/{key}"),
        ),
    }
}

/// The text of each `<tag>…</tag>` element of an XML reply, in order, with entities decoded.
pub fn xml_texts(xml: &str, tag: &str) -> Vec<String> {
    let (open, close) = (format!("<{tag}>"), format!("</{tag}>"));
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        rest = &rest[start + open.len()..];
        let Some(end) = rest.find(&close) else { break };
        found.push(unescape(&rest[..end]));
        rest = &rest[end + close.len()..];
    }
    found
}

/// The first `<tag>` text, if any.
pub fn xml_text(xml: &str, tag: &str) -> Option<String> {
    xml_texts(xml, tag).into_iter().next()
}

fn unescape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        rest = &rest[at..];
        let Some(end) = rest.find(';') else { break };
        let entity = &rest[1..end];
        let decoded = match entity {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => entity
                .strip_prefix("#x")
                .or_else(|| entity.strip_prefix("#X"))
                .map(|h| u32::from_str_radix(h, 16))
                .or_else(|| entity.strip_prefix('#').map(|d| d.parse()))
                .and_then(|n| n.ok())
                .and_then(char::from_u32),
        };
        match decoded {
            Some(c) => {
                out.push(c);
                rest = &rest[end + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}
