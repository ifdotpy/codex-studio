//! Local Markdown destinations. The backend still checks access to each file.
use crate::protocol::{Request, Response};
use regex::Regex;
use serde_json::{Value, json};
use std::sync::OnceLock;
struct Patterns {
    scheme: Regex,
    relative: Regex,
    location: Regex,
    anchor: Regex,
}
fn patterns() -> &'static Patterns {
    static PATTERNS: OnceLock<Patterns> = OnceLock::new();
    PATTERNS.get_or_init(|| Patterns {
        scheme: Regex::new(r"(?i)^[a-z][a-z0-9+.-]*:").unwrap(),
        relative: Regex::new(r"^[^/:?#]+\.[^/:?#]+:[0-9]+(?::[0-9]+)?(?:#.*)?$").unwrap(),
        location: Regex::new(r":([0-9]+)(?::[0-9]+)?$").unwrap(),
        anchor: Regex::new(r"(?i)^L?([0-9]+)(?:C[0-9]+|-L?[0-9]+)?$").unwrap(),
    })
}
fn padding(c: char) -> bool {
    matches!(c, '\u{9}'..='\u{d}' | ' ' | '\u{a0}' | '\u{1680}' | '\u{2000}'..='\u{200b}' | '\u{2028}' | '\u{2029}' | '\u{202f}' | '\u{205f}' | '\u{3000}' | '\u{feff}')
}
fn decode(path: &str) -> Result<String, String> {
    let bytes = path.as_bytes();
    let mut output = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let digits = bytes
                .get(i + 1..i + 3)
                .ok_or("This file link has an invalid escape.")?;
            let high = (digits[0] as char)
                .to_digit(16)
                .ok_or("This file link has an invalid escape.")?;
            let low = (digits[1] as char)
                .to_digit(16)
                .ok_or("This file link has an invalid escape.")?;
            output.push((high * 16 + low) as u8);
            i += 3;
        } else {
            output.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(output).map_err(|_| "This file link has invalid UTF-8.".into())
}
fn bounded(value: &str) -> Result<&str, String> {
    if value.len() > 16384 {
        return Err("This file link is too long.".into());
    }
    Ok(value)
}
pub fn local(href: &str) -> Result<Value, String> {
    let value = bounded(href)?.trim_matches(padding);
    if value.is_empty() || value.starts_with('#') || value.starts_with("//") {
        return Ok(Value::Null);
    }
    let lower = value.to_ascii_lowercase();
    if patterns().scheme.is_match(value)
        && !lower.starts_with("file:")
        && !lower.starts_with("sandbox:")
        && !patterns().relative.is_match(value)
    {
        return Ok(Value::Null);
    }
    let path = if lower.starts_with("file:") {
        let url = reqwest::Url::parse(value).map_err(|_| "This file link is invalid.")?;
        if url
            .host_str()
            .is_some_and(|s| !s.is_empty() && s != "localhost")
        {
            return Err("Remote file links are not supported.".into());
        }
        format!(
            "{}{}",
            url.path(),
            url.fragment().map(|v| format!("#{v}")).unwrap_or_default()
        )
    } else if lower.starts_with("sandbox:") {
        let value = &value[8..];
        if !value.starts_with('/') || value.starts_with("//") {
            return Err("This sandbox file link is invalid.".into());
        }
        value.to_owned()
    } else {
        value.to_owned()
    };
    let (path, anchor) = path.split_once('#').unwrap_or((&path, ""));
    let decoded = decode(path)?;
    let path = decoded.trim_matches('\u{200b}');
    let location = patterns().location.captures(path);
    let start = location
        .as_ref()
        .map(|c| c.get(0).unwrap().start())
        .unwrap_or(path.len());
    let path = &path[..start];
    if path.is_empty() || path.chars().any(|c| c <= '\u{1f}') {
        return Err("This file link is invalid.".into());
    }
    let anchor = patterns().anchor.captures(anchor);
    let line = anchor
        .as_ref()
        .or(location.as_ref())
        .and_then(|c| c.get(1))
        .and_then(|m| m.as_str().parse::<u64>().ok())
        .filter(|n| *n > 0 && *n <= 9_007_199_254_740_991);
    let mut value = json!({"path":path});
    if let Some(line) = line {
        value["line"] = json!(line);
    }
    Ok(value)
}
fn execute(request: &Request) -> Result<Value, String> {
    match request.method.as_str() {
        "file_link.local" => local(
            request.params["value"]
                .as_str()
                .ok_or("A file link is required.")?,
        ),
        "file_link.relative_location" => Ok(json!(
            patterns().relative.is_match(bounded(
                request.params["value"]
                    .as_str()
                    .ok_or("A file link is required.")?
            )?)
        )),
        "file_link.relative_to_document" => {
            let mut target = request.params["target"].clone();
            let path = bounded(target["path"].as_str().ok_or("A file path is required.")?)?;
            if let Some(base) = request.params["base"]
                .as_str()
                .filter(|_| !path.starts_with('/'))
            {
                let base = bounded(base)?;
                if let Some(slash) = base.rfind('/') {
                    let joined = format!("{}{path}", &base[..slash + 1]);
                    bounded(&joined)?;
                    target["path"] = json!(joined);
                }
            }
            Ok(target)
        }
        _ => Err("Unknown file link command.".into()),
    }
}
pub fn dispatch(request: &Request) -> Response {
    match execute(request) {
        Ok(value) => Response::success(request.id.clone(), value),
        Err(error) => Response::failure(request.id.clone(), "file_link", error),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn synchronous_link_commands_reject_unbounded_input() {
        let long = "x".repeat(16385);
        for (method, params) in [
            ("file_link.local", json!({"value":long})),
            ("file_link.relative_location", json!({"value":long})),
            (
                "file_link.relative_to_document",
                json!({"target":{"path":long}}),
            ),
            (
                "file_link.relative_to_document",
                json!({"target":{"path":"a"},"base":long}),
            ),
            (
                "file_link.relative_to_document",
                json!({"target":{"path":"a".repeat(8192)},"base":format!("{}/b", "b".repeat(8192))}),
            ),
        ] {
            assert!(
                execute(&Request {
                    id: "limit".into(),
                    method: method.into(),
                    params
                })
                .unwrap_err()
                .contains("too long")
            );
        }
    }
    #[test]
    fn local_destinations_preserve_paths_and_line_identity() {
        for (link, expected) in [
            (
                "\u{200b} file:///tmp/%D0%96%F0%9F%A6%80.rs#L12C2 \u{200b}",
                json!({"path":"/tmp/Ж🦀.rs","line":12}),
            ),
            (
                "sandbox:/tmp/report.md:9:2",
                json!({"path":"/tmp/report.md","line":9}),
            ),
            (
                "file://localhost/tmp/a.md#L4-L8",
                json!({"path":"/tmp/a.md","line":4}),
            ),
            ("report.md:3:20#L8", json!({"path":"report.md","line":8})),
            ("a+b.ts:2", json!({"path":"a+b.ts","line":2})),
            ("a.ts:0", json!({"path":"a.ts"})),
            ("a.ts:9007199254740992", json!({"path":"a.ts"})),
            (
                "./encoded%23name.ts#12",
                json!({"path":"./encoded#name.ts","line":12}),
            ),
            ("chapter.md", json!({"path":"chapter.md"})),
        ] {
            assert_eq!(local(link).unwrap(), expected, "{link}");
        }
        for link in [
            "https://example.com/a.md",
            "javascript:alert(1)",
            "//remote/file",
            "#heading",
            "mailto:a@b",
            "",
        ] {
            assert_eq!(local(link).unwrap(), Value::Null, "{link}");
        }
        for link in [
            "file://server/a",
            "sandbox://remote/a",
            "sandbox:relative",
            "bad%ZZ.md",
            "bad%.md",
            "bad%FF.md",
            "bad%00.md",
        ] {
            assert!(local(link).is_err(), "{link}");
        }
        let reply = execute(&Request {
            id: "relative".into(),
            method: "file_link.relative_to_document".into(),
            params: json!({"target":{"path":"chapter.md","line":2},"base":"/docs/book/readme.md"}),
        })
        .unwrap();
        assert_eq!(reply, json!({"path":"/docs/book/chapter.md","line":2}));
    }
}
