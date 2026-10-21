//! The URL rules of the browser bridge (docs/vm-browser-bridge.md, "URL policy" and
//! section 6.3).
//!
//! The owner of these rules is `workspaces/runtime/apps/vm-guest/browser_policy.py`; the guest
//! and the Python server import it. This module is its Rust port, and
//! `tests/browser_policy_differential.rs` runs both on one corpus so every accepted link, every
//! field and every refusal message stays equal. The splitting follows Python's `urlsplit`,
//! `parse_qs(keep_blank_values=True)` and `ipaddress` rules that the owner depends on.

use std::collections::{BTreeMap, BTreeSet};
use std::net::{Ipv4Addr, Ipv6Addr};

/// The longest link, in characters (`MAX_URL` of the owner).
pub const MAX_URL: usize = 8192;
/// The longest device code that the card shows, in characters (`USER_CODE_LIMIT`).
pub const USER_CODE_LIMIT: usize = 64;
/// The longest query value that the card shows, in characters (`_first` of the owner).
const QUERY_VALUE_LIMIT: usize = 500;
const LOOPBACK_HOSTS: [&str; 3] = ["localhost", "127.0.0.1", "::1"];

/// The link cannot open. The message is for the agent and the person.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct PolicyError(pub String);

fn refuse<T>(message: impl Into<String>) -> Result<T, PolicyError> {
    Err(PolicyError(message.into()))
}

/// The loopback address that receives the sign-in result.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Redirect {
    pub host: String,
    pub port: u16,
    pub path: String,
}

/// A sign-in page that the person's browser opens.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Provider {
    pub url: String,
    pub host: String,
    pub redirect: Option<Redirect>,
    pub client_id: Option<String>,
    pub scope: Option<String>,
    /// The OAuth `state` of the link. A pasted redirect must carry the same value.
    pub state: Option<String>,
    /// The `user_code` of a device code page.
    pub user_code: Option<String>,
}

/// A link from an agent: a tool's own loopback start page or a provider page.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Checked {
    StartPage { url: String, port: u16 },
    Provider(Provider),
}

/// A sign-in redirect that the person pasted: the path and query for the tool's listener.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Callback {
    pub target: String,
    pub state: Option<String>,
    pub error: bool,
}

/// The parts of Python's `urlsplit` that the rules read.
struct Parts {
    scheme: String,
    netloc: String,
    path: String,
    query: String,
}

impl Parts {
    fn hostinfo(&self) -> &str {
        self.netloc
            .rsplit_once('@')
            .map_or(&self.netloc, |(_, host)| host)
    }

    fn has_user(&self) -> bool {
        self.netloc.contains('@')
    }

    /// `hostname` and the raw port text, as Python `_hostinfo`.
    fn host_and_port(&self) -> (String, Option<String>) {
        let info = self.hostinfo();
        let (host, port) = if let Some((_, bracketed)) = info.split_once('[') {
            let (host, rest) = bracketed.split_once(']').unwrap_or((bracketed, ""));
            (host, rest.split_once(':').map_or("", |(_, port)| port))
        } else {
            info.split_once(':').unwrap_or((info, ""))
        };
        // Python lowers `hostname`, and the rules lower it again with its zone.
        let host = host.to_lowercase();
        (host, (!port.is_empty()).then(|| port.to_owned()))
    }

    fn port(&self) -> Result<Option<u16>, ()> {
        match self.host_and_port().1 {
            None => Ok(None),
            Some(text) => {
                if text.is_empty() || !text.bytes().all(|byte| byte.is_ascii_digit()) {
                    return Err(());
                }
                // Python accepts any digit count; a value above 65535 is invalid.
                let digits = text.trim_start_matches('0');
                if digits.len() > 5 {
                    return Err(());
                }
                let value: u32 = if digits.is_empty() {
                    0
                } else {
                    digits.parse().map_err(|_| ())?
                };
                u16::try_from(value).map(Some).map_err(|_| ())
            }
        }
    }
}

fn split(url: &str) -> Result<Parts, PolicyError> {
    let mut rest = url;
    let mut scheme = String::new();
    if let Some(index) = rest.find(':')
        && index > 0
        && rest.as_bytes()[0].is_ascii_alphabetic()
        && rest[..index]
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'+' | b'-' | b'.'))
    {
        scheme = rest[..index].to_ascii_lowercase();
        rest = &rest[index + 1..];
    }
    let mut netloc = String::new();
    if let Some(after) = rest.strip_prefix("//") {
        let end = after.find(['/', '?', '#']).unwrap_or(after.len());
        netloc = after[..end].to_owned();
        rest = &after[end..];
        let open = netloc.contains('[');
        let close = netloc.contains(']');
        if open != close {
            return refuse("Invalid IPv6 URL");
        }
        if open {
            let bracketed = netloc
                .split_once('[')
                .map(|(_, after)| after.split_once(']').map_or(after, |(host, _)| host))
                .unwrap_or_default();
            if !bracketed_host(bracketed) {
                return refuse("Invalid IPv6 URL");
            }
        }
    }
    let rest = rest.split_once('#').map_or(rest, |(before, _)| before);
    let (path, query) = rest.split_once('?').unwrap_or((rest, ""));
    Ok(Parts {
        scheme,
        netloc,
        path: path.to_owned(),
        query: query.to_owned(),
    })
}

/// Python `_check_bracketed_host`: an IPv6 address (with a zone) or an IPvFuture literal.
fn bracketed_host(text: &str) -> bool {
    if let Some(future) = text.strip_prefix(['v', 'V']) {
        let Some((version, rest)) = future.split_once('.') else {
            return false;
        };
        return !version.is_empty()
            && version.bytes().all(|byte| byte.is_ascii_hexdigit())
            && !rest.is_empty()
            && rest
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"-._~!$&'()*+,;=:".contains(&byte));
    }
    parse_ipv6(text).is_some()
}

fn unquote(text: &str) -> String {
    let text = text.replace('+', " ");
    if !text.contains('%') {
        return text;
    }
    // Python decodes each ASCII run and keeps other characters as they are.
    let mut output = String::new();
    let mut run = Vec::new();
    let flush = |run: &mut Vec<u8>, output: &mut String| {
        if !run.is_empty() {
            output.push_str(&String::from_utf8_lossy(&percent_decode(run)));
            run.clear();
        }
    };
    for character in text.chars() {
        if character.is_ascii() {
            run.push(character as u8);
        } else {
            flush(&mut run, &mut output);
            output.push(character);
        }
    }
    flush(&mut run, &mut output);
    output
}

fn percent_decode(bytes: &[u8]) -> Vec<u8> {
    let mut output = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'%'
            && index + 2 < bytes.len()
            && let (Some(high), Some(low)) = (hex(bytes[index + 1]), hex(bytes[index + 2]))
        {
            output.push(high * 16 + low);
            index += 3;
            continue;
        }
        output.push(bytes[index]);
        index += 1;
    }
    output
}

fn hex(byte: u8) -> Option<u8> {
    (byte as char).to_digit(16).map(|value| value as u8)
}

type Query = BTreeMap<String, Vec<String>>;

fn parse_query(query: &str) -> Query {
    let mut values = Query::new();
    for pair in query.split('&') {
        if pair.is_empty() {
            continue;
        }
        let (name, value) = pair.split_once('=').unwrap_or((pair, ""));
        values
            .entry(unquote(name))
            .or_default()
            .push(unquote(value));
    }
    values
}

fn truncate(text: &str, limit: usize) -> String {
    text.chars().take(limit).collect()
}

fn first(query: &Query, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| {
        query
            .get(*name)
            .and_then(|values| values.first())
            .filter(|value| !value.is_empty())
            .map(|value| truncate(value, QUERY_VALUE_LIMIT))
    })
}

/// The one value of `name`, untruncated, or None when it is missing, empty or repeated.
fn exact(query: &Query, name: &str) -> Option<String> {
    match query.get(name).map(Vec::as_slice) {
        Some([value]) if !value.is_empty() => Some(value.clone()),
        _ => None,
    }
}

fn text(value: &str) -> Result<&str, PolicyError> {
    if value.is_empty() {
        return refuse("Give one link");
    }
    if value.chars().count() > MAX_URL {
        return refuse("The link is longer than 8192 characters");
    }
    if value
        .chars()
        .any(|character| (character as u32) <= 32 || character as u32 == 127)
    {
        return refuse("The link contains spaces or control characters");
    }
    Ok(value)
}

fn parts(text: &str) -> Result<(Parts, String, Option<u16>), PolicyError> {
    let parts = split(text)?;
    if parts.has_user() {
        return refuse("A link with a user name or password cannot open");
    }
    let host = parts.host_and_port().0;
    if host.is_empty() {
        return refuse("The link has no host");
    }
    let port = parts
        .port()
        .map_err(|()| PolicyError("The link has an invalid port".into()))?;
    Ok((parts, host, port))
}

fn origin(host: &str, port: u16) -> String {
    if host.contains(':') {
        format!("[{host}]:{port}")
    } else {
        format!("{host}:{port}")
    }
}

fn effective_port(port: Option<u16>) -> u16 {
    match port {
        None | Some(0) => 80,
        Some(port) => port,
    }
}

/// Python `ipaddress.IPv4Address`: four decimal octets without leading zeros.
fn parse_ipv4(text: &str) -> Option<Ipv4Addr> {
    let octets: Vec<&str> = text.split('.').collect();
    if octets.len() != 4 {
        return None;
    }
    let mut bytes = [0u8; 4];
    for (slot, octet) in bytes.iter_mut().zip(octets) {
        if octet.is_empty()
            || octet.len() > 3
            || !octet.bytes().all(|byte| byte.is_ascii_digit())
            || (octet.len() > 1 && octet.starts_with('0'))
        {
            return None;
        }
        *slot = octet.parse().ok()?;
    }
    Some(Ipv4Addr::from(bytes))
}

fn parse_ipv6(text: &str) -> Option<Ipv6Addr> {
    let address = match text.split_once('%') {
        Some((address, zone)) if !zone.is_empty() && !zone.contains('%') => address,
        Some(_) => return None,
        None => text,
    };
    address.parse().ok()
}

fn in_v4(address: Ipv4Addr, network: [u8; 4], prefix: u32) -> bool {
    let mask = if prefix == 0 {
        0
    } else {
        u32::MAX << (32 - prefix)
    };
    u32::from(address) & mask == u32::from(Ipv4Addr::from(network)) & mask
}

fn in_v6(address: Ipv6Addr, network: &str, prefix: u32) -> bool {
    let network: Ipv6Addr = network.parse().expect("static IPv6 network");
    let mask = if prefix == 0 {
        0
    } else {
        u128::MAX << (128 - prefix)
    };
    u128::from(address) & mask == u128::from(network) & mask
}

/// Python 3.13 `is_private or is_loopback or is_link_local or is_unspecified or
/// is_multicast or is_reserved` of an IPv4 address.
fn local_v4(address: Ipv4Addr) -> bool {
    const PRIVATE: [([u8; 4], u32); 14] = [
        ([0, 0, 0, 0], 8),
        ([10, 0, 0, 0], 8),
        ([127, 0, 0, 0], 8),
        ([169, 254, 0, 0], 16),
        ([172, 16, 0, 0], 12),
        ([192, 0, 0, 0], 24),
        ([192, 0, 0, 170], 31),
        ([192, 0, 2, 0], 24),
        ([192, 168, 0, 0], 16),
        ([198, 18, 0, 0], 15),
        ([198, 51, 100, 0], 24),
        ([203, 0, 113, 0], 24),
        ([240, 0, 0, 0], 4),
        ([255, 255, 255, 255], 32),
    ];
    const NOT_PRIVATE: [([u8; 4], u32); 2] = [([192, 0, 0, 9], 32), ([192, 0, 0, 10], 32)];
    let private = PRIVATE
        .iter()
        .any(|(network, prefix)| in_v4(address, *network, *prefix))
        && !NOT_PRIVATE
            .iter()
            .any(|(network, prefix)| in_v4(address, *network, *prefix));
    private || in_v4(address, [224, 0, 0, 0], 4)
}

/// The same union for an IPv6 address.
fn local_v6(address: Ipv6Addr) -> bool {
    const PRIVATE: [(&str, u32); 10] = [
        ("::1", 128),
        ("::", 128),
        ("::ffff:0:0", 96),
        ("64:ff9b:1::", 48),
        ("100::", 64),
        ("2001::", 23),
        ("2001:db8::", 32),
        ("2001:10::", 28),
        ("fc00::", 7),
        ("fe80::", 10),
    ];
    const NOT_PRIVATE: [(&str, u32); 6] = [
        ("2001:1::1", 128),
        ("2001:1::2", 128),
        ("2001:3::", 32),
        ("2001:4:112::", 48),
        ("2001:20::", 28),
        ("2001:30::", 28),
    ];
    const RESERVED: [(&str, u32); 15] = [
        ("::", 8),
        ("100::", 8),
        ("200::", 7),
        ("400::", 6),
        ("800::", 5),
        ("1000::", 4),
        ("4000::", 3),
        ("6000::", 3),
        ("8000::", 3),
        ("a000::", 3),
        ("c000::", 3),
        ("e000::", 4),
        ("f000::", 5),
        ("f800::", 6),
        ("fe00::", 9),
    ];
    // Python 3.13 answers every property of an IPv4-mapped address with its IPv4 address.
    if let Some(mapped) = address.to_ipv4_mapped() {
        return local_v4(mapped);
    }
    let private = PRIVATE
        .iter()
        .any(|(network, prefix)| in_v6(address, network, *prefix))
        && !NOT_PRIVATE
            .iter()
            .any(|(network, prefix)| in_v6(address, network, *prefix));
    private
        || RESERVED
            .iter()
            .any(|(network, prefix)| in_v6(address, network, *prefix))
        || in_v6(address, "fe80::", 10)
        || in_v6(address, "ff00::", 8)
}

fn local_host(host: &str) -> bool {
    if host == "localhost" || host.ends_with(".localhost") {
        return true;
    }
    if let Some(address) = parse_ipv4(host) {
        return local_v4(address);
    }
    parse_ipv6(host).is_some_and(local_v6)
}

/// Sort a link from an agent into a loopback start page or a provider page.
///
/// `http_providers` holds test origins (`host:port`) that may use http. Production passes
/// nothing, so every provider page uses https.
pub fn classify(url: &str, http_providers: &BTreeSet<String>) -> Result<Checked, PolicyError> {
    let value = text(url)?;
    let (parts, host, port) = parts(value)?;
    if parts.scheme == "http"
        && LOOPBACK_HOSTS.contains(&host.as_str())
        && !http_providers.contains(&origin(&host, effective_port(port)))
    {
        return match port {
            Some(port) if port >= 1024 => Ok(Checked::StartPage {
                url: value.to_owned(),
                port,
            }),
            _ => refuse("A loopback page needs a port from 1024 to 65535"),
        };
    }
    provider(value, http_providers).map(Checked::Provider)
}

/// Check a provider page and find its loopback redirect.
pub fn provider(url: &str, http_providers: &BTreeSet<String>) -> Result<Provider, PolicyError> {
    let value = text(url)?;
    let (parts, host, port) = parts(value)?;
    if parts.scheme == "https" {
        if local_host(&host) {
            return refuse("An https link to a local or private address cannot open");
        }
    } else if !(parts.scheme == "http"
        && http_providers.contains(&origin(&host, effective_port(port))))
    {
        return refuse("Only https sign-in links and loopback sign-in pages can open");
    }
    let query = parse_query(&parts.query);
    let mut redirect = None;
    if let Some(target_text) = first(&query, &["redirect_uri"]) {
        let target = split(&target_text)?;
        let target_host = target.host_and_port().0;
        if LOOPBACK_HOSTS.contains(&target_host.as_str()) {
            let target_port = target
                .port()
                .map_err(|()| PolicyError("The loopback redirect has an invalid port".into()))?;
            if target.scheme != "http" {
                return refuse("A loopback redirect must use http");
            }
            if target.has_user() {
                return refuse("A loopback redirect with a user name cannot open");
            }
            let Some(target_port) = target_port.filter(|port| *port >= 1024) else {
                return refuse("A loopback redirect needs a port from 1024 to 65535");
            };
            redirect = Some(Redirect {
                host: target_host,
                port: target_port,
                path: if target.path.is_empty() {
                    "/".into()
                } else {
                    target.path.clone()
                },
            });
        }
    }
    let user_code = if redirect.is_none() {
        first(&query, &["user_code"]).map(|code| truncate(&code, USER_CODE_LIMIT))
    } else {
        None
    };
    Ok(Provider {
        url: value.to_owned(),
        host,
        redirect,
        client_id: first(&query, &["client_id"]),
        scope: first(&query, &["scope", "scopes"]),
        state: exact(&query, "state"),
        user_code,
    })
}

/// Check the address of the page that did not load after a sign-in (section 6.3). It must be
/// the request's loopback redirect with a code or an error and the `state` of the provider
/// link.
pub fn pasted(
    pasted_text: &str,
    redirect_port: u16,
    redirect_path: &str,
    state: Option<&str>,
) -> Result<Callback, PolicyError> {
    let value = text(pasted_text)?;
    // Some phone browsers copy the address without the scheme.
    let value = if value.contains("://") {
        value.to_owned()
    } else {
        format!("http://{value}")
    };
    let (parts, host, port) = parts(&value)?;
    if parts.scheme != "http" || !LOOPBACK_HOSTS.contains(&host.as_str()) {
        return refuse(
            "Paste the address of the page that did not load. It starts with http://localhost or http://127.0.0.1",
        );
    }
    let path = if parts.path.is_empty() {
        "/"
    } else {
        &parts.path
    };
    if port != Some(redirect_port) || path != redirect_path {
        return refuse(format!(
            "The address is not for this sign-in: it must use port {redirect_port} and the path {redirect_path}"
        ));
    }
    let query = parse_query(&parts.query);
    if first(&query, &["code"]).is_none() && first(&query, &["error"]).is_none() {
        return refuse("The address has no sign-in result. Paste it after the sign-in ends");
    }
    let Some(state) = state.filter(|state| !state.is_empty()) else {
        return refuse(
            "This sign-in has no state value, so Studio cannot check a pasted address. Use a desktop or the device code",
        );
    };
    if exact(&query, "state").as_deref() != Some(state) {
        return refuse("The address belongs to another sign-in: its state value is different");
    }
    Ok(Callback {
        target: format!("{path}?{}", parts.query),
        state: Some(state.to_owned()),
        error: first(&query, &["error"]).is_some(),
    })
}

/// The protocol markers of a sign-in link in an agent's message (`SIGN_IN_MARKERS` of the
/// owner): an OAuth or OpenID Connect authorization request, a pushed authorization request, a
/// device verification link with its code, and a SAML request.
const SIGN_IN_MARKERS: [&[&str]; 5] = [
    &["client_id", "response_type"],
    &["client_id", "redirect_uri"],
    &["client_id", "request_uri"],
    &["user_code"],
    &["SAMLRequest"],
];
/// The most sign-in links that one agent message can give (`MESSAGE_LINK_LIMIT`).
pub const MESSAGE_LINK_LIMIT: usize = 4;
/// A link ends at a character that is not printable ASCII or at one of these (`_LINK_STOP`).
const LINK_STOP: &[u8] = b"<>\"'`";
/// Text punctuation after a link, not part of it (`_LINK_TRAIL`).
const LINK_TRAIL: &[u8] = b".,;:!?*";
/// Names for documentation and examples (`DOCUMENTATION_DOMAINS`, `DOCUMENTATION_SUFFIXES`).
const DOCUMENTATION_DOMAINS: [&str; 3] = ["example.com", "example.net", "example.org"];
const DOCUMENTATION_SUFFIXES: [&str; 2] = [".example", ".invalid"];

/// The start and end of the next `https?://` (any case) at or after `from`.
fn link_scheme(bytes: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut index = from;
    while index + 7 <= bytes.len() {
        let rest = &bytes[index..];
        if rest[..4].eq_ignore_ascii_case(b"http") {
            if rest.len() >= 8 && rest[4].eq_ignore_ascii_case(&b's') && &rest[5..8] == b"://" {
                return Some((index, index + 8));
            }
            if &rest[4..7] == b"://" {
                return Some((index, index + 7));
            }
        }
        index += 1;
    }
    None
}

fn trim_link(mut link: &str) -> &str {
    while let Some(&last) = link.as_bytes().last() {
        let count = |byte: u8| link.bytes().filter(|value| *value == byte).count();
        if LINK_TRAIL.contains(&last)
            || (last == b')' && count(b')') > count(b'('))
            || (last == b']' && count(b']') > count(b'['))
        {
            link = &link[..link.len() - 1];
        } else {
            break;
        }
    }
    link
}

/// Whether the link's host is a name for documentation (`_documentation_host`).
fn documentation_host(link: &str) -> bool {
    let after = link.split_once("://").map_or("", |(_, rest)| rest);
    let authority = after.split(['/', '?', '#']).next().unwrap_or_default();
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host)
        .to_ascii_lowercase();
    let host = match host.strip_prefix('[') {
        Some(rest) => rest.split(']').next().unwrap_or_default().to_owned(),
        None => host.split(':').next().unwrap_or_default().to_owned(),
    };
    let host = host.trim_end_matches('.');
    DOCUMENTATION_DOMAINS
        .iter()
        .any(|name| host == *name || host.ends_with(&format!(".{name}")))
        || DOCUMENTATION_SUFFIXES
            .iter()
            .any(|suffix| host.ends_with(suffix) || host == &suffix[1..])
}

/// The query names with a value (`_query_names`).
fn query_names(link: &str) -> BTreeSet<String> {
    let before = link.split('#').next().unwrap_or_default();
    let query = before.split_once('?').map_or("", |(_, query)| query);
    parse_query(query)
        .into_iter()
        .filter(|(_, values)| values.iter().any(|value| !value.is_empty()))
        .map(|(name, _)| name)
        .collect()
}

/// The links in an agent's message that the person must open to sign in or approve
/// (`sign_in_links` of the owner, docs/vm-browser-bridge.md section 7). Only protocol markers in
/// the query count, so an ordinary link stays an ordinary link.
pub fn sign_in_links(text: &str) -> Vec<String> {
    let mut links: Vec<String> = Vec::new();
    if !text.contains("://") {
        return links;
    }
    let bytes = text.as_bytes();
    let mut position = 0;
    while links.len() < MESSAGE_LINK_LIMIT {
        let Some((start, scheme_end)) = link_scheme(bytes, position) else {
            break;
        };
        let mut end = scheme_end;
        while end < bytes.len()
            && (33..127).contains(&bytes[end])
            && !LINK_STOP.contains(&bytes[end])
        {
            end += 1;
        }
        position = end;
        let link = trim_link(&text[start..end]);
        if link.len() > MAX_URL
            || links.iter().any(|known| known == link)
            || documentation_host(link)
        {
            continue;
        }
        let names = query_names(link);
        if SIGN_IN_MARKERS
            .iter()
            .any(|marker| marker.iter().all(|name| names.contains(*name)))
        {
            links.push(link.to_owned());
        }
    }
    links
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none() -> BTreeSet<String> {
        BTreeSet::new()
    }

    #[test]
    fn sign_in_links_follow_the_shared_corpus() {
        // The owner (vm-guest/browser_policy.py) checks the same file.
        let corpus: Vec<serde_json::Value> =
            serde_json::from_str(include_str!("../../../apps/vm-guest/sign_in_links.json"))
                .expect("corpus");
        assert!(corpus.len() > 20);
        for case in corpus {
            let expected: Vec<String> =
                serde_json::from_value(case["links"].clone()).expect("links");
            assert_eq!(
                sign_in_links(case["text"].as_str().expect("text")),
                expected,
                "{}",
                case["name"]
            );
        }
    }

    #[test]
    fn provider_link_names_redirect_state_and_client() {
        let checked = provider(
            "https://id.example.com/authorize?client_id=k&redirect_uri=http%3A%2F%2Flocalhost%3A8000&state=abc&scope=openid",
            &none(),
        )
        .expect("provider link");
        assert_eq!(
            checked.redirect,
            Some(Redirect {
                host: "localhost".into(),
                port: 8000,
                path: "/".into()
            })
        );
        assert_eq!(checked.state.as_deref(), Some("abc"));
        assert_eq!(checked.client_id.as_deref(), Some("k"));
        assert_eq!(checked.user_code, None);
    }

    #[test]
    fn pasted_redirect_needs_port_path_result_and_state() {
        let ok = pasted("localhost:8000/?code=x&state=abc", 8000, "/", Some("abc"))
            .expect("matching paste");
        assert_eq!(ok.target, "/?code=x&state=abc");
        for bad in [
            "http://localhost:8001/?code=x&state=abc",
            "http://localhost:8000/?state=abc",
            "http://localhost:8000/?code=x&state=abd",
            "https://localhost:8000/?code=x&state=abc",
            "http://evil.com:8000/?code=x&state=abc",
            "http://localhost:8000/?code=x&state=abc&state=abc",
        ] {
            assert!(pasted(bad, 8000, "/", Some("abc")).is_err(), "{bad}");
        }
        assert!(pasted("http://localhost:8000/?code=x&state=abc", 8000, "/", None).is_err());
    }

    #[test]
    fn private_and_reserved_https_hosts_are_refused() {
        for host in [
            "10.0.0.1",
            "[::1]",
            "[fe80::1]",
            "localhost",
            "a.localhost",
            "224.0.0.1",
        ] {
            assert!(
                provider(&format!("https://{host}/"), &none()).is_err(),
                "{host}"
            );
        }
        for host in ["8.8.8.8", "[2606:4700::1]", "accounts.example.com"] {
            assert!(
                provider(&format!("https://{host}/"), &none()).is_ok(),
                "{host}"
            );
        }
    }
}
