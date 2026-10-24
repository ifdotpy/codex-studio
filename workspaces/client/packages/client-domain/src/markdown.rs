//! Markdown interpretation and an inert presentation tree. The UI never parses HTML.
use crate::protocol::{Request, Response};
use html5ever::tokenizer::{
    BufferQueue, StartTag, Token, TokenSink, TokenSinkResult, Tokenizer, TokenizerOpts,
};
use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag};
use schemars::JsonSchema;
use serde::Serialize;
use std::{cell::RefCell, collections::BTreeMap};
use unicode_segmentation::UnicodeSegmentation;

// Parser budgets count UTF-8 input bytes and appended presentation nodes.
const MAX_DOCUMENT_BYTES: usize = 8 * 1024 * 1024;
const MAX_DOCUMENT_ELEMENTS: usize = 100_000;

#[derive(Default, Serialize, Debug, JsonSchema)]
pub struct MarkdownNode {
    #[serde(skip_serializing_if = "String::is_empty")]
    tag: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    text: String,
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    props: BTreeMap<String, String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    children: Vec<MarkdownNode>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    sentences: Vec<MarkdownSentence>,
}
#[derive(Serialize, Debug, JsonSchema)]
struct MarkdownSentence {
    text: String,
    index: usize,
}

#[derive(Serialize, Debug, JsonSchema)]
pub struct MarkdownTree {
    nodes: Vec<MarkdownNode>,
    length: usize,
}

#[derive(Serialize, Debug, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum MarkdownKind {
    Html,
    Mermaid,
}

#[derive(Serialize, Debug, JsonSchema)]
#[serde(untagged)]
pub enum MarkdownBlock {
    Tree(MarkdownTree),
    Rich { kind: MarkdownKind, source: String },
}

#[derive(Default)]
struct Tree {
    stack: Vec<MarkdownNode>,
    nodes: usize,
    error: Option<String>,
    blocked: Vec<String>,
}
impl Tree {
    fn append(&mut self, node: MarkdownNode) {
        self.nodes += 1;
        if self.nodes > MAX_DOCUMENT_ELEMENTS {
            self.error = Some("This Markdown document has too many elements.".into());
            return;
        }
        let children = &mut self.stack.last_mut().unwrap().children;
        // Tokenizer chunk boundaries differ between native and WASM builds.
        // Canonicalize text before sentence segmentation and React keys.
        if node.tag.is_empty()
            && let Some(previous) = children
                .last_mut()
                .filter(|previous| previous.tag.is_empty())
        {
            previous.text.push_str(&node.text);
            return;
        }
        children.push(node);
    }
    fn close(&mut self) {
        let node = self.stack.pop().unwrap();
        self.append(node);
    }
}
struct Sink(RefCell<Tree>);
fn allowed(tag: &str) -> bool {
    matches!(
        tag,
        "p" | "div"
            | "span"
            | "a"
            | "img"
            | "pre"
            | "code"
            | "em"
            | "strong"
            | "b"
            | "i"
            | "s"
            | "del"
            | "u"
            | "sup"
            | "sub"
            | "br"
            | "hr"
            | "h1"
            | "h2"
            | "h3"
            | "h4"
            | "h5"
            | "h6"
            | "ul"
            | "ol"
            | "li"
            | "blockquote"
            | "table"
            | "thead"
            | "tbody"
            | "tfoot"
            | "tr"
            | "th"
            | "td"
            | "caption"
            | "dl"
            | "dt"
            | "dd"
            | "details"
            | "summary"
            | "kbd"
            | "samp"
            | "small"
            | "mark"
            | "abbr"
    )
}
fn safe_uri(value: &str, image: bool) -> bool {
    let value = value.trim();
    if value.chars().any(|c| c.is_control()) {
        return false;
    }
    let Some((scheme, _)) = value.split_once(':') else {
        return true;
    };
    if !scheme
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || "+.-".contains(c))
    {
        return true;
    }
    if matches!(
        scheme.to_ascii_lowercase().as_str(),
        "http" | "https" | "mailto" | "tel" | "sms" | "cid" | "xmpp" | "file" | "sandbox"
    ) {
        return true;
    }
    if image
        && [
            "data:image/png;base64,",
            "data:image/jpeg;base64,",
            "data:image/gif;base64,",
            "data:image/webp;base64,",
        ]
        .iter()
        .any(|prefix| value.to_ascii_lowercase().starts_with(prefix))
    {
        return true;
    }
    // A filename with a line suffix is a local file, not a URL scheme.
    scheme.contains('.')
        && value
            .split(':')
            .skip(1)
            .all(|p| !p.is_empty() && p.bytes().all(|c| c.is_ascii_digit()))
}
impl TokenSink for Sink {
    type Handle = ();
    fn process_token(&self, token: Token, _: u64) -> TokenSinkResult<()> {
        let mut tree = self.0.borrow_mut();
        if tree.error.is_some() {
            return TokenSinkResult::Continue;
        }
        if let Token::TagToken(tag) = &token {
            let name = tag.name.as_ref();
            if tag.kind == StartTag
                && matches!(
                    name,
                    "script"
                        | "style"
                        | "iframe"
                        | "object"
                        | "embed"
                        | "svg"
                        | "math"
                        | "template"
                        | "video"
                        | "audio"
                )
            {
                if tree.blocked.len() >= 128 {
                    tree.error = Some("This Markdown document is nested too deeply.".into());
                } else {
                    tree.blocked.push(name.to_owned());
                }
                return TokenSinkResult::Continue;
            }
            if tag.kind != StartTag
                && let Some(index) = tree.blocked.iter().rposition(|n| n == name)
            {
                tree.blocked.truncate(index);
                return TokenSinkResult::Continue;
            }
        }
        if !tree.blocked.is_empty() {
            return TokenSinkResult::Continue;
        }
        match token {
            Token::CharacterTokens(text) => tree.append(MarkdownNode {
                text: text.to_string(),
                ..MarkdownNode::default()
            }),
            Token::TagToken(tag) => {
                let name = tag.name.as_ref();
                // Unknown HTML can supply text, never an executable element or attributes.
                if !allowed(name) {
                    return TokenSinkResult::Continue;
                }
                if tag.kind == StartTag {
                    let mut node = MarkdownNode {
                        tag: name.to_owned(),
                        ..MarkdownNode::default()
                    };
                    for attr in tag.attrs {
                        if !attr.name.ns.is_empty() {
                            continue;
                        }
                        let key = attr.name.local.as_ref();
                        let value = attr.value.to_string();
                        let key = match key {
                            "href" if name == "a" && safe_uri(&value, false) => "href",
                            "src" if name == "img" && safe_uri(&value, true) => "src",
                            "alt" if name == "img" => "alt",
                            "title" => "title",
                            "class" if name == "code" && value.starts_with("language-") => {
                                "className"
                            }
                            "colspan"
                                if matches!(name, "td" | "th") && value.parse::<u16>().is_ok() =>
                            {
                                "colSpan"
                            }
                            "rowspan"
                                if matches!(name, "td" | "th") && value.parse::<u16>().is_ok() =>
                            {
                                "rowSpan"
                            }
                            "start" if name == "ol" && value.parse::<u64>().is_ok() => "start",
                            _ => continue,
                        };
                        node.props.insert(key.into(), value);
                    }
                    if matches!(name, "br" | "hr" | "img") || tag.self_closing {
                        tree.append(node);
                    } else if tree.stack.len() >= 128 {
                        tree.error = Some("This Markdown document is nested too deeply.".into());
                    } else {
                        tree.stack.push(node);
                    }
                } else if let Some(index) = tree.stack.iter().rposition(|n| n.tag == name) {
                    while tree.stack.len() > index && tree.stack.len() > 1 {
                        tree.close();
                    }
                }
            }
            _ => {}
        }
        TokenSinkResult::Continue
    }
}
fn segments(node: &mut MarkdownNode, offset: &mut usize, literal: bool) {
    let literal = literal || matches!(node.tag.as_str(), "pre" | "code");
    if !node.text.is_empty() {
        if !literal && !node.text.trim().is_empty() {
            let start = *offset;
            let mut local = 0;
            node.sentences = node
                .text
                .split_sentence_bounds()
                .map(|part| {
                    let item = MarkdownSentence {
                        text: part.into(),
                        index: start + local,
                    };
                    local += part.encode_utf16().count();
                    item
                })
                .collect();
        }
        *offset += node.text.encode_utf16().count();
    }
    for child in &mut node.children {
        segments(child, offset, literal);
    }
}
fn tree(html: String) -> Result<MarkdownTree, String> {
    let sink = Sink(RefCell::new(Tree {
        stack: vec![MarkdownNode::default()],
        ..Tree::default()
    }));
    let tokenizer = Tokenizer::new(sink, TokenizerOpts::default());
    let input = BufferQueue::default();
    input.push_back(html.into());
    let _ = tokenizer.feed(&input);
    tokenizer.end();
    let mut tree = tokenizer.sink.0.into_inner();
    if let Some(error) = tree.error {
        return Err(error);
    }
    while tree.stack.len() > 1 {
        tree.close();
    }
    let mut root = tree.stack.pop().unwrap();
    let mut length = 0;
    segments(&mut root, &mut length, false);
    Ok(MarkdownTree {
        nodes: root.children,
        length,
    })
}
fn closed_fence(raw: &str) -> bool {
    let lines: Vec<_> = raw.trim_end().lines().collect();
    if lines.len() < 2 {
        return false;
    }
    let first = lines[0].trim_start();
    let last = lines.last().unwrap().trim();
    let Some(ch) = first.chars().next().filter(|c| matches!(c, '`' | '~')) else {
        return false;
    };
    let count = first.chars().take_while(|c| *c == ch).count();
    count >= 3 && last.len() >= count && last.chars().all(|c| c == ch)
}
pub fn parse(text: &str, streaming: bool) -> Result<Vec<MarkdownBlock>, String> {
    if text.len() > MAX_DOCUMENT_BYTES {
        return Err("This Markdown document exceeds 8 MiB.".into());
    }
    let events = Parser::new_ext(
        text,
        Options::ENABLE_TABLES
            | Options::ENABLE_STRIKETHROUGH
            | Options::ENABLE_TASKLISTS
            | Options::ENABLE_GFM,
    )
    .into_offset_iter();
    let mut blocks: Vec<MarkdownBlock> = vec![];
    let mut group = vec![];
    let mut depth = 0usize;
    let mut start = 0;
    let mut end = 0;
    let mut previous_raw_html = false;
    for (event, range) in events {
        if group.is_empty() {
            start = range.start;
        }
        end = end.max(range.end);
        match &event {
            Event::Start(_) => depth += 1,
            Event::End(_) => depth = depth.saturating_sub(1),
            _ => {}
        }
        group.push(event);
        if depth != 0 {
            continue;
        }
        let raw_html = matches!(group.first(), Some(Event::Start(Tag::HtmlBlock))) && !streaming;
        let kind = match group.first() {
            Some(Event::Start(Tag::CodeBlock(CodeBlockKind::Fenced(lang))))
                if matches!(
                    lang.trim().to_ascii_lowercase().as_str(),
                    "html" | "mermaid"
                ) && closed_fence(&text[start..end]) =>
            {
                Some(if lang.trim().eq_ignore_ascii_case("html") {
                    MarkdownKind::Html
                } else {
                    MarkdownKind::Mermaid
                })
            }
            Some(Event::Start(Tag::HtmlBlock)) if !streaming => Some(MarkdownKind::Html),
            _ => None,
        };
        if let Some(kind) = kind {
            let source = group
                .iter()
                .filter_map(|e| match e {
                    Event::Text(s) | Event::Html(s) => Some(s.as_ref()),
                    _ => None,
                })
                .collect::<String>();
            if raw_html
                && previous_raw_html
                && let Some(MarkdownBlock::Rich {
                    source: previous, ..
                }) = blocks.last_mut()
            {
                previous.push('\n');
                previous.push_str(&source);
            } else {
                blocks.push(MarkdownBlock::Rich { kind, source });
            }
        } else {
            let mut html = String::new();
            pulldown_cmark::html::push_html(&mut html, group.drain(..));
            blocks.push(MarkdownBlock::Tree(tree(html)?));
        }
        previous_raw_html = raw_html;
        group.clear();
    }
    Ok(blocks)
}
pub fn dispatch(request: &Request) -> Response {
    let result = request.params["text"]
        .as_str()
        .ok_or_else(|| "Markdown text is required.".to_owned())
        .and_then(|text| parse(text, request.params["streaming"].as_bool().unwrap_or(false)));
    match result {
        Ok(value) => Response::success(
            request.id.clone(),
            serde_json::to_value(value)
                .expect("the inert Markdown output contains only serializable fields"),
        ),
        Err(error) => Response::failure(request.id.clone(), "markdown", error),
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn parse(text: &str, streaming: bool) -> Result<Value, String> {
        super::parse(text, streaming).map(|blocks| serde_json::to_value(blocks).unwrap())
    }
    fn tree(html: String) -> Result<Value, String> {
        super::tree(html).map(|tree| serde_json::to_value(tree).unwrap())
    }

    #[test]
    fn markdown_has_structure_references_and_utf16_sentences() {
        let value=parse("# Title\n\n🦀 Привет. Next **bold** [link][ref].\n\n[ref]: https://example.test\n\n|A|B|\n|-|-|\n|1|2|",false).unwrap();
        let raw = value.to_string();
        assert!(raw.contains("https://example.test"));
        assert!(raw.contains("table"));
        assert!(raw.contains("strong"));
        assert_eq!(
            value[1]["nodes"][0]["children"][0]["sentences"][1]["index"],
            11
        );
    }
    #[test]
    fn rich_fences_must_close_and_text_never_becomes_executable() {
        assert!(
            parse("```html\n<b>Hi</b>", true).unwrap()[0]
                .get("kind")
                .is_none()
        );
        assert_eq!(
            parse("```html\n<b>Hi</b>\n```", true).unwrap()[0]["kind"],
            "html"
        );
        let safe=tree("<a href='java&#x73;cript:alert(1)' onclick='x()'>link</a><img src='data:text/html,x' onerror='x()'><svg><script>x()</script></svg><input name='__proto__'><b style='x'>bold</b>".into()).unwrap().to_string();
        for denied in [
            "href",
            "onclick",
            "onerror",
            "src",
            "script",
            "svg",
            "style",
            "__proto__",
        ] {
            assert!(!safe.contains(denied), "{safe}");
        }
        assert!(safe.contains("bold"));
        assert!(!safe.contains("x()"));
    }
    #[test]
    fn oversized_or_deep_fragments_fail_without_partial_success() {
        assert_eq!(
            tree("<div>".repeat(140)).expect_err("depth limit"),
            "This Markdown document is nested too deeply."
        );
        let mut text = " ".repeat(MAX_DOCUMENT_BYTES);
        assert!(
            super::parse(&text, false)
                .expect("size boundary")
                .is_empty()
        );
        text.push(' ');
        assert_eq!(
            super::parse(&text, false).expect_err("size limit"),
            "This Markdown document exceeds 8 MiB."
        );
        let mut tree = Tree {
            nodes: MAX_DOCUMENT_ELEMENTS - 1,
            stack: vec![MarkdownNode::default()],
            ..Tree::default()
        };
        tree.append(MarkdownNode::default());
        assert!(tree.error.is_none());
        tree.append(MarkdownNode::default());
        assert_eq!(
            tree.error.as_deref(),
            Some("This Markdown document has too many elements.")
        );
        assert!(!safe_uri("java\nscript:bad", false));
        assert!(!safe_uri("javascript:bad", false));
        assert!(safe_uri("src/lib.rs:12", false));
    }
    #[test]
    fn independent_html_fences_remain_separate_previews() {
        let value = parse(
            "```html\n<b>One</b>\n```\n\n```html\n<b>Two</b>\n```",
            false,
        )
        .unwrap();
        assert_eq!(value.as_array().unwrap().len(), 2);
        assert_eq!(value[0]["source"], "<b>One</b>\n");
        assert_eq!(value[1]["source"], "<b>Two</b>\n");
        let value = parse(
            "<style>p {color: red}</style>\n\n<div>One document</div>",
            false,
        )
        .unwrap();
        assert_eq!(value.as_array().unwrap().len(), 1);
        assert!(
            value[0]["source"]
                .as_str()
                .unwrap()
                .contains("One document")
        );
    }
}
