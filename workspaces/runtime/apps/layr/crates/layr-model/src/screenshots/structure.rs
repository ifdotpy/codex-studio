//! Stable text for screen structure snapshots, with optional native node frames.
use anyhow::{bail, Result};
use serde_json::{json, Value};
use similar::{ChangeTag, TextDiff};
use std::collections::BTreeMap;

#[derive(Clone, Default)]
pub(super) struct Document {
    lines: Vec<Line>,
}
#[derive(Clone)]
struct Line {
    text: String,
    kind: LineKind,
    frame: Option<Value>,
    text_values: BTreeMap<String, String>,
    attribute_values: BTreeMap<String, String>,
}
#[derive(Clone)]
enum LineKind {
    Element(String),
    Text,
    Attribute,
    Other,
}
impl Document {
    fn push(&mut self, text: String, kind: LineKind, frame: Option<Value>) {
        self.lines.push(Line { text, kind, frame, text_values: BTreeMap::new(), attribute_values: BTreeMap::new() });
    }
    fn text(&self) -> String {
        self.lines.iter().map(|l| format!("{}\n", l.text)).collect()
    }
    fn flush_text(&mut self, text: &mut String, frame: Option<Value>) {
        if !text.trim().is_empty() {
            self.push(escape(text), LineKind::Text, frame);
        }
        text.clear();
    }
}
fn escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\r', "&#13;")
        .replace('\n', "&#10;")
}
fn box_value(x: f64, y: f64, w: f64, h: f64) -> Option<Value> {
    if [x, y, w, h].iter().any(|n| !n.is_finite()) || w < 0.0 || h < 0.0 {
        return None;
    }
    Some(json!({"x":x,"y":y,"width":w,"height":h}))
}
fn frame_string(text: &str, bounds: bool) -> Option<Value> {
    let numbers: Vec<f64> = text
        .split(|c: char| !(c.is_ascii_digit() || c == '.' || c == '-' || c == '+'))
        .filter(|s| !s.is_empty())
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    let [x, y, a, b] = numbers.as_slice() else {
        return None;
    };
    box_value(*x, *y, if bounds { a - x } else { *a }, if bounds { b - y } else { *b })
}
fn frame_json(value: &Value, bounds: bool) -> Option<Value> {
    if let Some(s) = value.as_str() {
        return frame_string(s, bounds);
    }
    if let Some(a) = value.as_array() {
        if a.len() == 4 {
            let (x, y, w, h) = (a[0].as_f64()?, a[1].as_f64()?, a[2].as_f64()?, a[3].as_f64()?);
            return box_value(x, y, if bounds { w - x } else { w }, if bounds { h - y } else { h });
        }
    }
    let x = value.get("x").or_else(|| value.get("left")).or_else(|| value.get("origin")?.get("x"))?.as_f64()?;
    let y = value.get("y").or_else(|| value.get("top")).or_else(|| value.get("origin")?.get("y"))?.as_f64()?;
    let w = value
        .get("width")
        .or_else(|| value.get("size")?.get("width"))
        .and_then(Value::as_f64)
        .or_else(|| Some(value.get("right")?.as_f64()? - x))?;
    let h = value
        .get("height")
        .or_else(|| value.get("size")?.get("height"))
        .and_then(Value::as_f64)
        .or_else(|| Some(value.get("bottom")?.as_f64()? - y))?;
    box_value(x, y, w, h)
}
fn xml_frame(attrs: &BTreeMap<String, String>) -> Option<Value> {
    for key in ["bounds", "frame"] {
        if let Some(frame) = attrs.get(key).and_then(|v| frame_string(v, key == "bounds")) {
            return Some(frame);
        }
    }
    let number = |name: &str| attrs.get(name)?.parse::<f64>().ok();
    box_value(number("x")?, number("y")?, number("width")?, number("height")?)
}
pub(super) fn normalize(text: &str, extension: &str, ignore: &[String]) -> Result<Document> {
    match extension {
        "html" => html(text),
        "xml" => xml(text, ignore),
        "json" => {
            let value: Value = serde_json::from_str(text)?;
            let mut doc = Document::default();
            json_lines(&value, None, 0, None, ignore, &mut doc, false);
            Ok(doc)
        }
        "txt" => Ok(Document {
            lines: text
                .lines()
                .map(|line| Line {
                    text: line.into(),
                    kind: LineKind::Text,
                    frame: None,
                    text_values: BTreeMap::new(),
                    attribute_values: BTreeMap::new(),
                })
                .collect(),
        }),
        _ => bail!("unsupported structure extension '{extension}'"),
    }
}
fn html(text: &str) -> Result<Document> {
    use html5gum::{DefaultEmitter, Token, Tokenizer};
    let mut doc = Document::default();
    let mut pending = String::new();
    let mut emitter = DefaultEmitter::default();
    emitter.naively_switch_states(true);
    for token in Tokenizer::new_with_emitter(text, emitter) {
        match token? {
            Token::String(s) => pending.push_str(&String::from_utf8_lossy(&s)),
            Token::StartTag(tag) => {
                doc.flush_text(&mut pending, None);
                let name = String::from_utf8_lossy(&tag.name).into_owned();
                let mut line = format!("<{name}");
                for (key, value) in tag.attributes {
                    let key = String::from_utf8_lossy(&key);
                    if key == "nonce" || (key.starts_with("data-") && key.ends_with("id")) {
                        continue;
                    }
                    line.push_str(&format!(" {key}=\"{}\"", escape(&String::from_utf8_lossy(&value))));
                }
                line.push_str(if tag.self_closing { "/>" } else { ">" });
                doc.push(line, LineKind::Element(name), None);
            }
            Token::EndTag(tag) => {
                doc.flush_text(&mut pending, None);
                doc.push(format!("</{}>", String::from_utf8_lossy(&tag.name)), LineKind::Other, None);
            }
            // Comments, doctypes and recoverable tokenizer errors do not describe view nodes.
            _ => {}
        }
    }
    doc.flush_text(&mut pending, None);
    Ok(doc)
}
fn xml(text: &str, ignore: &[String]) -> Result<Document> {
    use quick_xml::{events::Event, Reader};
    let mut reader = Reader::from_str(text);
    reader.config_mut().expand_empty_elements = true;
    let mut doc = Document::default();
    let mut frames = Vec::new();
    let mut pending = String::new();
    loop {
        match reader.read_event()? {
            Event::Start(tag) => {
                doc.flush_text(&mut pending, frames.last().cloned().flatten());
                let name = tag.name().as_ref().to_owned();
                let attrs = tag
                    .attributes()
                    .map(|a| {
                        let a = a?;
                        Ok((a.key.as_ref().to_owned(), a.normalized_value(quick_xml::XmlVersion::Implicit1_0)?.into_owned()))
                    })
                    .collect::<Result<BTreeMap<String, String>>>()?;
                let frame = xml_frame(&attrs);
                let mut line = format!("<{name}");
                let text_values = attrs
                    .iter()
                    .filter(|(key, _)| is_text_key(key) && !ignore.contains(key))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                let attribute_values = attrs
                    .iter()
                    .filter(|(key, _)| !is_text_key(key) && !ignore.contains(key))
                    .map(|(k, v)| (k.clone(), v.clone()))
                    .collect();
                for (key, value) in attrs {
                    if !ignore.contains(&key) {
                        line.push_str(&format!(" {key}=\"{}\"", escape(&value)));
                    }
                }
                line.push('>');
                doc.push(line, LineKind::Element(name), frame.clone());
                doc.lines.last_mut().unwrap().text_values = text_values;
                doc.lines.last_mut().unwrap().attribute_values = attribute_values;
                frames.push(frame);
            }
            Event::End(tag) => {
                doc.flush_text(&mut pending, frames.last().cloned().flatten());
                frames.pop();
                doc.push(format!("</{}>", tag.name().as_ref()), LineKind::Other, None);
            }
            Event::Text(t) => pending.push_str(t.as_ref()),
            Event::CData(t) => pending.push_str(t.as_ref()),
            Event::GeneralRef(r) => pending.push_str(&quick_xml::escape::unescape(&format!("&{};", r.as_ref()))?),
            Event::Eof => break,
            _ => {}
        }
    }
    doc.flush_text(&mut pending, None);
    if !frames.is_empty() {
        bail!("unclosed XML element");
    }
    Ok(doc)
}
fn json_lines(
    value: &Value,
    key: Option<&str>,
    depth: usize,
    parent_frame: Option<Value>,
    ignore: &[String],
    doc: &mut Document,
    comma: bool,
) {
    let frame = value
        .get("frame")
        .and_then(|v| frame_json(v, false))
        .or_else(|| value.get("bounds").and_then(|v| frame_json(v, true)))
        .or_else(|| frame_json(value, false))
        .or(parent_frame);
    let indent = "  ".repeat(depth);
    let prefix = key.map(|k| format!("{}: ", serde_json::to_string(k).unwrap())).unwrap_or_default();
    let tail = if comma { "," } else { "" };
    match value {
        Value::Object(map) => {
            doc.push(format!("{indent}{prefix}{{"), LineKind::Element(key.unwrap_or("[]").into()), frame.clone());
            let keys: Vec<_> =
                map.keys().filter(|k| !ignore.contains(k)).collect::<std::collections::BTreeSet<_>>().into_iter().collect();
            for (i, key) in keys.iter().enumerate() {
                json_lines(&map[*key], Some(key), depth + 1, frame.clone(), ignore, doc, i + 1 < keys.len());
            }
            doc.push(format!("{indent}}}{tail}"), LineKind::Other, None);
        }
        Value::Array(items) => {
            doc.push(format!("{indent}{prefix}["), LineKind::Element(key.unwrap_or("[]").into()), frame.clone());
            for (i, item) in items.iter().enumerate() {
                json_lines(item, None, depth + 1, frame.clone(), ignore, doc, i + 1 < items.len());
            }
            doc.push(format!("{indent}]{tail}"), LineKind::Other, None);
        }
        _ => doc.push(
            format!("{indent}{prefix}{value}{tail}"),
            if key.is_none_or(is_text_key) { LineKind::Text } else { LineKind::Attribute },
            frame,
        ),
    }
}
fn is_text_key(key: &str) -> bool {
    matches!(key, "text" | "label" | "value" | "title" | "content-desc" | "name")
}
fn region(line: &Line, kind: &str, out: &mut Vec<Value>) {
    if let Some(frame) = &line.frame {
        let mut value = frame.clone();
        value["change"] = json!(kind);
        if !out.contains(&value) {
            out.push(value);
        }
    }
}
pub(super) fn compare(a: &Document, b: &Document, old_path: &str, new_path: &str) -> Value {
    let old = a.text();
    let new = b.text();
    let diff = TextDiff::from_lines(&old, &new);
    let mut deleted = Vec::<&Line>::new();
    let mut inserted = Vec::<&Line>::new();
    let (mut added, mut removed, mut texts) = (0, 0, 0);
    let mut regions = Vec::new();
    let mut flush = |deleted: &mut Vec<&Line>, inserted: &mut Vec<&Line>| {
        // Match element names inside each replacement. Attribute edits keep the element count.
        let mut unmatched = inserted.iter().map(|_| true).collect::<Vec<_>>();
        for before in deleted.iter().filter(|l| matches!(l.kind, LineKind::Element(_))) {
            let LineKind::Element(name) = &before.kind else { unreachable!() };
            if let Some((i, after)) =
                inserted.iter().enumerate().find(|(i, l)| unmatched[*i] && matches!(&l.kind,LineKind::Element(n) if n==name))
            {
                unmatched[i] = false;
                let text_count = before
                    .text_values
                    .keys()
                    .chain(after.text_values.keys())
                    .collect::<std::collections::BTreeSet<_>>()
                    .into_iter()
                    .filter(|key| before.text_values.get(*key) != after.text_values.get(*key))
                    .count();
                texts += text_count;
                let kind = if text_count > 0 { "text" } else { "attribute" };
                region(after, kind, &mut regions);
                if text_count > 0 && before.attribute_values != after.attribute_values {
                    region(after, "attribute", &mut regions);
                    if before.frame != after.frame {
                        region(before, "attribute", &mut regions);
                    }
                }
                // A moved or resized native node can have distinct old and new outlines.
                if before.frame != after.frame {
                    region(before, kind, &mut regions);
                }
            } else {
                removed += 1;
                region(before, "removed", &mut regions);
            }
        }
        for (i, line) in inserted.iter().enumerate() {
            if unmatched[i] && matches!(line.kind, LineKind::Element(_)) {
                added += 1;
                region(line, "added", &mut regions);
            }
        }
        for line in deleted.iter().chain(inserted.iter()).filter(|l| matches!(l.kind, LineKind::Attribute)) {
            region(line, "attribute", &mut regions);
        }
        let old_text: Vec<_> = deleted.iter().filter(|l| matches!(l.kind, LineKind::Text)).collect();
        let new_text: Vec<_> = inserted.iter().filter(|l| matches!(l.kind, LineKind::Text)).collect();
        texts += old_text.len().max(new_text.len());
        for line in old_text.into_iter().chain(new_text) {
            region(line, "text", &mut regions);
        }
        deleted.clear();
        inserted.clear();
    };
    for change in diff.iter_all_changes() {
        match change.tag() {
            ChangeTag::Delete => deleted.push(&a.lines[change.old_index().unwrap()]),
            ChangeTag::Insert => inserted.push(&b.lines[change.new_index().unwrap()]),
            ChangeTag::Equal => flush(&mut deleted, &mut inserted),
        }
    }
    flush(&mut deleted, &mut inserted);
    json!({"elements_added":added,"elements_removed":removed,"text_changes":texts,"structure_regions":regions,
        "structure_diff":if old==new {String::new()}else{diff.unified_diff().header(old_path,new_path).to_string()}})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn html_normalizes_attributes_entities_and_generated_ids() {
        let a = normalize("<div b='2' a='1' nonce='x' data-reactid='1'>\n <p>A &amp; B</p> </div>", "html", &[]).unwrap();
        let b = normalize("<div data-otherid='2' nonce='y' a='1' b='2'><p>A &#38; B</p></div>", "html", &[]).unwrap();
        assert_eq!(a.text(), b.text());
        assert!(compare(&a, &b, "a", "b")["structure_diff"].as_str().unwrap().is_empty());
        let changed = normalize("<div a='1' b='2'><p>New</p><button>Go</button></div>", "html", &[]).unwrap();
        let result = compare(&a, &changed, "a", "b");
        assert_eq!(result["elements_added"], 1);
        assert_eq!(result["elements_removed"], 0);
        assert_eq!(result["text_changes"], 2);
        assert_eq!(result["structure_regions"], json!([]));
    }
    #[test]
    fn xml_frames_survive_ignored_coordinates_and_attribute_changes() {
        let ignore = crate::probes::default_ignore_attributes();
        let a = normalize("<node bounds='[10,20][30,40]' role='label' text='Old'/>", "xml", &ignore).unwrap();
        let b = normalize("<node text='New' role='button' bounds='[10,20][30,40]'/>", "xml", &ignore).unwrap();
        let delta = compare(&a, &b, "a", "b");
        assert_eq!(delta["text_changes"], 1);
        assert_eq!(delta["elements_added"], 0);
        assert_eq!(delta["structure_regions"][0], json!({"x":10.0,"y":20.0,"width":20.0,"height":20.0,"change":"text"}));
        let moved = normalize("<node text='Old' role='label' bounds='[0,0][5,5]'/>", "xml", &ignore).unwrap();
        assert_eq!(a.text(), moved.text());
        assert_ne!(a.text(), normalize("<node bounds='[10,20][30,40]' role='label' text='Old'/>", "xml", &[]).unwrap().text());
    }
    #[test]
    fn ignored_xml_text_attributes_cannot_inflate_change_counts() {
        let ignore = vec!["text".to_owned()];
        let a = normalize("<node text='Old' role='label'/>", "xml", &ignore).unwrap();
        let b = normalize("<node text='New' role='button'/>", "xml", &ignore).unwrap();
        let delta = compare(&a, &b, "a", "b");
        assert_eq!(delta["text_changes"], 0);
        assert!(!delta["structure_diff"].as_str().unwrap().contains("text="));
    }
    #[test]
    fn json_is_canonical_valid_json_and_keeps_native_frames() {
        let ignore = crate::probes::default_ignore_attributes();
        let a =
            normalize(r#"{"label":"Old","frame":{"x":1,"y":2,"width":3,"height":4},"role":"label"}"#, "json", &ignore).unwrap();
        let b = normalize(
            r#"{"role":"button","frame":{"origin":{"x":1,"y":2},"size":{"width":3,"height":4}},"label":"New"}"#,
            "json",
            &ignore,
        )
        .unwrap();
        let parsed: Value = serde_json::from_str(&a.text()).unwrap();
        assert_eq!(parsed, json!({"label":"Old","role":"label"}));
        let delta = compare(&a, &b, "a", "b");
        assert_eq!(delta["text_changes"], 1);
        assert_eq!(delta["structure_regions"].as_array().unwrap().len(), 2);
    }
    #[test]
    fn raw_html_style_text_is_not_parsed_as_elements() {
        let a = normalize("<style>p::after { content: '<x>'; }</style><p>Text</p>", "html", &[]).unwrap();
        assert_eq!(a.lines.iter().filter(|l| matches!(l.kind, LineKind::Element(_))).count(), 2);
        assert!(normalize("<a><b></a>", "xml", &[]).is_err());
        assert!(normalize("not json", "json", &[]).is_err());
    }
}
