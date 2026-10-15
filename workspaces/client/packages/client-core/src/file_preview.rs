//! Immutable file snapshots and presentation data, scoped to one UI connection.
use crate::{
    backend::Backend,
    protocol::{Request, Response},
};
use base64::{Engine, engine::general_purpose::STANDARD};
use serde_json::{Value, json};
use std::{
    collections::HashMap,
    io::Cursor,
    sync::{Arc, Mutex},
};
use tokio::sync::Semaphore;

const FILE_LIMIT: usize = 20 * 1024 * 1024;
const CACHE_LIMIT: usize = 256 * 1024 * 1024;
const CHUNK: usize = 256 * 1024;
struct File {
    name: String,
    format: &'static str,
    bytes: Arc<Vec<u8>>,
    presentation: String,
}
#[derive(Clone)]
pub struct Previews {
    files: Arc<Mutex<HashMap<String, File>>>,
    preparation: Arc<Semaphore>,
}
impl Default for Previews {
    fn default() -> Self {
        Self {
            files: Arc::default(),
            preparation: Arc::new(Semaphore::new(2)),
        }
    }
}
impl Previews {
    fn insert(&self, file: File) -> Result<Value, String> {
        let mut files = self
            .files
            .lock()
            .map_err(|_| "The preview service stopped.")?;
        let occupied: usize = files
            .values()
            .map(|f| f.bytes.len() + f.presentation.len())
            .sum();
        if files.len() >= 8 || occupied + file.bytes.len() + file.presentation.len() > CACHE_LIMIT {
            return Err("Close another file preview first.".into());
        }
        let handle = crate::file_save::uuid()?;
        let length = file.presentation.len();
        files.insert(handle.clone(), file);
        Ok(json!({"handle":handle,"length":length}))
    }
    fn read(&self, value: &Value) -> Result<Value, String> {
        let files = self
            .files
            .lock()
            .map_err(|_| "The preview service stopped.")?;
        let file = files
            .get(handle(value)?)
            .ok_or("This file preview is closed.")?;
        let offset = value["offset"]
            .as_u64()
            .filter(|n| *n <= file.presentation.len() as u64)
            .ok_or("Invalid preview offset.")? as usize;
        if !file.presentation.is_char_boundary(offset) {
            return Err("Invalid preview offset.".into());
        }
        let mut end = (offset + CHUNK).min(file.presentation.len());
        while !file.presentation.is_char_boundary(end) {
            end -= 1;
        }
        Ok(
            json!({"text":&file.presentation[offset..end],"next":end,"done":end == file.presentation.len()}),
        )
    }
    pub fn snapshot(&self, value: &Value) -> Result<(String, Arc<Vec<u8>>), String> {
        let files = self
            .files
            .lock()
            .map_err(|_| "The preview service stopped.")?;
        let file = files
            .get(handle(value)?)
            .ok_or("This file preview is closed.")?;
        Ok((file.name.clone(), file.bytes.clone()))
    }
    pub async fn dispatch(&self, backend: Option<&Backend>, request: Request) -> Response {
        let result = async {
            match request.method.as_str() {
                "preview.open" => {
                    let path = crate::file_actions::content_path(request.params["target"].clone())?;
                    let backend = backend.ok_or("The native backend is unavailable.")?;
                    let permit = self
                        .preparation
                        .clone()
                        .acquire_owned()
                        .await
                        .map_err(|e| e.to_string())?;
                    let data = backend
                        .read_bounded(&path, FILE_LIMIT.div_ceil(3) * 4 + 65536)
                        .await?;
                    let file = tokio::task::spawn_blocking(move || {
                        let _permit = permit;
                        prepare(data)
                    })
                    .await
                    .map_err(|e| e.to_string())??;
                    self.insert(file)
                }
                "preview.read" => self.read(&request.params),
                "preview.pdfPage" => {
                    let bytes = {
                        let files = self
                            .files
                            .lock()
                            .map_err(|_| "The preview service stopped.")?;
                        let file = files
                            .get(handle(&request.params)?)
                            .ok_or("This file preview is closed.")?;
                        if file.format != "pdf" {
                            return Err("This file is not a PDF.".into());
                        }
                        file.bytes.clone()
                    };
                    let page = request.params["page"].as_u64().ok_or("Invalid PDF page.")?;
                    crate::pdf::page(bytes, page).await
                }
                "preview.release" => {
                    self.files
                        .lock()
                        .map_err(|_| "The preview service stopped.")?
                        .remove(handle(&request.params)?);
                    Ok(Value::Null)
                }
                _ => Err("Unknown file preview command.".into()),
            }
        }
        .await;
        match result {
            Ok(value) => Response::success(request.id, value),
            Err(error) => Response::failure(request.id, "file_preview", error),
        }
    }
}
fn handle(value: &Value) -> Result<&str, String> {
    value["handle"]
        .as_str()
        .filter(|s| s.len() == 36)
        .ok_or("Invalid file preview handle.".into())
}
fn format(name: &str, mime: &str) -> &'static str {
    let extension = name.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    let ext = extension.as_str();
    if mime == "image/svg+xml" || ext == "svg" {
        "svg"
    } else if matches!(
        mime,
        "image/png"
            | "image/jpeg"
            | "image/gif"
            | "image/webp"
            | "image/avif"
            | "image/bmp"
            | "image/x-icon"
    ) {
        "image"
    } else if mime.starts_with("audio/") {
        "audio"
    } else if mime.starts_with("video/") {
        "video"
    } else if mime == "application/pdf" {
        "pdf"
    } else if mime == "text/html" || matches!(ext, "html" | "htm") {
        "html"
    } else if mime == "text/markdown" || matches!(ext, "md" | "markdown") {
        "markdown"
    } else if mime == "text/csv" || ext == "csv" {
        "csv"
    } else if mime == "text/tab-separated-values" || ext == "tsv" {
        "tsv"
    } else if mime == "application/json"
        || (mime.starts_with("application/") && mime.ends_with("+json"))
        || ext == "json"
    {
        "json"
    } else if mime.starts_with("text/")
        || matches!(mime, "application/xml" | "application/javascript")
        || matches!(
            ext,
            "ts" | "tsx"
                | "js"
                | "jsx"
                | "py"
                | "rs"
                | "toml"
                | "yaml"
                | "yml"
                | "log"
                | "diff"
                | "sh"
                | "ini"
                | "conf"
        )
    {
        "text"
    } else {
        "unknown"
    }
}
fn prepare(data: Value) -> Result<File, String> {
    let name = data["name"]
        .as_str()
        .filter(|s| !s.is_empty() && s.len() <= 1024 && !s.chars().any(char::is_control))
        .ok_or("Invalid preview file name.")?
        .to_owned();
    let mime = data["mime"]
        .as_str()
        .filter(|s| {
            !s.is_empty()
                && s.len() <= 256
                && s.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"/-+._".contains(&b))
        })
        .ok_or("Invalid preview MIME type.")?;
    let encoded = data["base64"]
        .as_str()
        .filter(|s| s.len() <= FILE_LIMIT.div_ceil(3) * 4)
        .ok_or("This file exceeds the 20 MiB preview limit.")?;
    let bytes = STANDARD
        .decode(encoded)
        .map_err(|_| "Invalid file bytes.")?;
    if bytes.len() > FILE_LIMIT {
        return Err("This file exceeds the 20 MiB preview limit.".into());
    }
    let format = format(&name, mime);
    let text = if matches!(
        format,
        "markdown" | "csv" | "tsv" | "json" | "text" | "html" | "svg"
    ) {
        // Match TextDecoder: replace invalid UTF-8 and consume the initial BOM.
        let decoded = String::from_utf8_lossy(&bytes);
        decoded
            .strip_prefix('\u{feff}')
            .unwrap_or(&decoded)
            .to_owned()
    } else {
        String::new()
    };
    let table = match format {
        "csv" => delimited(&text, ','),
        "tsv" => delimited(&text, '\t'),
        _ => Value::Null,
    };
    let formatted_json = if format == "json" {
        match serde_json::from_str::<Value>(&text) {
            Ok(value) => {
                json!({"text":serde_json::to_string_pretty(&value).map_err(|e| e.to_string())?,"error":""})
            }
            Err(_) => json!({"text":text,"error":"Invalid JSON. The original source is shown."}),
        }
    } else {
        Value::Null
    };
    let (url, dimensions) = if format == "svg" {
        let (clean, dimensions) = svg(&text)?;
        (
            format!("data:image/svg+xml;base64,{}", STANDARD.encode(clean)),
            dimensions,
        )
    } else if matches!(format, "image" | "audio" | "video") {
        let dimensions = if format == "image" {
            image::ImageReader::new(Cursor::new(&bytes))
                .with_guessed_format()
                .ok()
                .and_then(|reader| reader.into_dimensions().ok())
                .map(|(width, height)| json!({"width":width,"height":height}))
                .unwrap_or(Value::Null)
        } else {
            Value::Null
        };
        (format!("data:{mime};base64,{encoded}"), dimensions)
    } else {
        (String::new(), Value::Null)
    };
    let presentation = serde_json::to_string(&json!({"name":name,"mime":mime,"format":format,
        "url":url,"text":text,"table":table,"json":formatted_json,"dimensions":dimensions}))
    .map_err(|e| e.to_string())?;
    Ok(File {
        name,
        format,
        bytes: Arc::new(bytes),
        presentation,
    })
}
fn svg(source: &str) -> Result<(String, Value), String> {
    let mut options = usvg::Options::default();
    // No file or network access from document-controlled image references.
    options.image_href_resolver.resolve_string = Box::new(|_, _| None);
    static FONTS: std::sync::OnceLock<Arc<usvg::fontdb::Database>> = std::sync::OnceLock::new();
    options.fontdb = FONTS
        .get_or_init(|| {
            let mut fonts = usvg::fontdb::Database::new();
            fonts.load_system_fonts();
            Arc::new(fonts)
        })
        .clone();
    let tree = usvg::Tree::from_str(source, &options).map_err(|e| format!("Invalid SVG: {e}"))?;
    let size = tree.size();
    Ok((
        tree.to_string(&usvg::WriteOptions::default()),
        json!({"width":size.width(),"height":size.height()}),
    ))
}

// Preserve the browser's strict quote rules and visible row/column limits.
fn delimited(source: &str, delimiter: char) -> Value {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut row = Vec::new();
    let mut field = String::new();
    let (mut quoted, mut closed, mut truncated) = (false, false, false);
    let mut chars = source.chars().peekable();
    let mut units = 0;
    while let Some(c) = chars.next() {
        units += c.len_utf16();
        if units > 1024 * 1024 {
            truncated = true;
            break;
        }
        if quoted {
            if c == '"' {
                if chars.peek() == Some(&'"') {
                    field.push('"');
                    chars.next();
                    units += 1;
                } else {
                    quoted = false;
                    closed = true;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        if c == '"' && field.is_empty() && !closed {
            quoted = true;
            continue;
        }
        if c == delimiter || c == '\n' || c == '\r' {
            row.push(std::mem::take(&mut field));
            closed = false;
            if row.len() > 100 || (c == delimiter && row.len() >= 100) {
                truncated = true;
                break;
            }
            if c != delimiter {
                rows.push(std::mem::take(&mut row));
                if c == '\r' && chars.peek() == Some(&'\n') {
                    chars.next();
                    units += 1;
                }
                if rows.len() >= 200 && chars.peek().is_some() {
                    truncated = true;
                    break;
                }
            }
        } else {
            if closed || c == '"' {
                return json!({"rows":rows,"truncated":false,"error":"Invalid quoted field. Use Source to read the original file."});
            }
            field.push(c);
        }
    }
    if !truncated && quoted {
        return json!({"rows":rows,"truncated":false,"error":"Unclosed quoted field. Use Source to read the original file."});
    }
    if !truncated && (!field.is_empty() || closed || !row.is_empty()) {
        row.push(field);
        rows.push(row);
    }
    json!({"rows":rows,"truncated":truncated,"error":""})
}

#[cfg(test)]
mod tests {
    use super::*;
    fn data(name: &str, mime: &str, bytes: &[u8]) -> Value {
        json!({"name":name,"mime":mime,"base64":STANDARD.encode(bytes)})
    }
    #[test]
    fn source_bytes_survive_display_conversion_and_table_limits() {
        let source = "\u{feff}name,comment\r\n\"Ж🦀\",\"one\r\ntwo \"\"quoted\"\"\"\r\n";
        let file = prepare(data("Data.CSV", "text/plain", source.as_bytes())).unwrap();
        assert_eq!(file.bytes.as_slice(), source.as_bytes());
        let view: Value = serde_json::from_str(&file.presentation).unwrap();
        assert_eq!(view["format"], "csv");
        assert_eq!(
            view["table"]["rows"][1],
            json!(["Ж🦀", "one\r\ntwo \"quoted\""])
        );
        assert!(!view["text"].as_str().unwrap().starts_with('\u{feff}'));
        assert_eq!(
            delimited(&"a,b\n".repeat(201), ',')["rows"]
                .as_array()
                .unwrap()
                .len(),
            200
        );
        assert_eq!(delimited(&"a,b\n".repeat(201), ',')["truncated"], true);
        assert!(
            !delimited("a\"b,c", ',')["error"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        assert!(
            !delimited("\"a,b", ',')["error"]
                .as_str()
                .unwrap()
                .is_empty()
        );
        let invalid = prepare(data("test.txt", "text/plain", b"\xff\x00")).unwrap();
        let view: Value = serde_json::from_str(&invalid.presentation).unwrap();
        assert_eq!(view["text"], "\u{fffd}\u{0}");
        assert_eq!(invalid.bytes.as_slice(), b"\xff\x00");
        let json_file =
            prepare(data("test.json", "application/json", br#"{"z":1,"a":2}"#)).unwrap();
        let view: Value = serde_json::from_str(&json_file.presentation).unwrap();
        assert_eq!(view["json"]["text"], "{\n  \"z\": 1,\n  \"a\": 2\n}");
    }
    #[test]
    fn svg_normalization_has_no_script_file_or_network_references() {
        let directory = tempfile::tempdir().unwrap();
        let image = directory.path().join("secret.png");
        let pixel = STANDARD.decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+aN1sAAAAASUVORK5CYII=").unwrap();
        std::fs::write(&image, &pixel).unwrap();
        let source = format!(
            r#"<svg xmlns="http://www.w3.org/2000/svg" width="120" height="60" onload="alert(1)"><script>evil()</script><foreignObject><p>hidden</p></foreignObject><image href="{}" width="20" height="20"/><image href="https://evil.invalid/test.png"/><rect width="120" height="60" fill="red"/></svg>"#,
            image.display()
        );
        let (clean, dimensions) = svg(&source).unwrap();
        for forbidden in [
            "script",
            "onload",
            "foreignObject",
            "evil",
            "secret.png",
            "data:image/png",
        ] {
            assert!(!clean.contains(forbidden), "{forbidden}: {clean}");
        }
        assert!(clean.contains("#ff0000"));
        assert_eq!(dimensions, json!({"width":120.0,"height":60.0}));
        assert!(svg("<svg").is_err());
    }
    #[tokio::test]
    #[cfg(target_os = "macos")]
    async fn pdf_pages_use_connection_snapshots_and_reject_closed_or_wrong_files() {
        let previews = Previews::default();
        let bytes = include_bytes!("../tests/fixtures/pages.pdf");
        let file = prepare(data("pages.pdf", "application/pdf", bytes)).unwrap();
        let view: Value = serde_json::from_str(&file.presentation).unwrap();
        assert_eq!(view["url"], ""); // The UI gets pages, not the PDF bytes.
        let metadata = previews.insert(file).unwrap();
        let request = |method: &str, params: Value| Request {
            id: "pdf".into(),
            method: method.into(),
            params,
        };
        let params = json!({"handle":metadata["handle"],"page":1});
        let response = previews
            .dispatch(None, request("preview.pdfPage", params.clone()))
            .await;
        assert!(response.error.is_none());
        assert_eq!(response.result.unwrap()["pages"], 2);
        assert_eq!(previews.snapshot(&metadata).unwrap().1.as_slice(), bytes);
        assert!(
            Previews::default()
                .dispatch(None, request("preview.pdfPage", params.clone()))
                .await
                .error
                .is_some()
        );
        for page in [json!(-1), json!(2), json!(0.5), Value::Null] {
            assert!(
                previews
                    .dispatch(
                        None,
                        request(
                            "preview.pdfPage",
                            json!({"handle":metadata["handle"],"page":page})
                        )
                    )
                    .await
                    .error
                    .is_some()
            );
        }
        previews
            .dispatch(None, request("preview.release", metadata.clone()))
            .await;
        assert!(
            previews
                .dispatch(None, request("preview.pdfPage", params))
                .await
                .error
                .is_some()
        );
        let text = previews
            .insert(prepare(data("text.txt", "text/plain", bytes)).unwrap())
            .unwrap();
        assert!(
            previews
                .dispatch(
                    None,
                    request("preview.pdfPage", json!({"handle":text["handle"],"page":0}))
                )
                .await
                .error
                .is_some()
        );
    }
    #[tokio::test]
    async fn bounded_chunks_and_release_preserve_an_immutable_save_snapshot() {
        let previews = Previews::default();
        let source = "Ж🦀\0\n".repeat(100_000);
        let metadata = previews
            .insert(prepare(data("message.txt", "text/plain", source.as_bytes())).unwrap())
            .unwrap();
        let handle = metadata["handle"].as_str().unwrap();
        let mut combined = String::new();
        let mut offset = 0;
        loop {
            let part = previews
                .read(&json!({"handle":handle,"offset":offset}))
                .unwrap();
            assert!(part["text"].as_str().unwrap().len() <= CHUNK);
            combined.push_str(part["text"].as_str().unwrap());
            offset = part["next"].as_u64().unwrap();
            if part["done"] == true {
                break;
            }
        }
        assert_eq!(combined.len() as u64, offset);
        let view: Value = serde_json::from_str(&combined).unwrap();
        assert_eq!(view["text"], source);
        assert_eq!(
            Previews::default().snapshot(&metadata).unwrap_err(),
            "This file preview is closed."
        );
        let (name, bytes) = previews.snapshot(&metadata).unwrap();
        let response = previews
            .dispatch(
                None,
                Request {
                    id: "release".into(),
                    method: "preview.release".into(),
                    params: metadata.clone(),
                },
            )
            .await;
        assert!(response.error.is_none());
        assert!(previews.snapshot(&metadata).is_err());
        // A save that already owns the snapshot keeps the exact original bytes.
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join(name);
        crate::file_save::write_atomic_if(&path, &bytes, || true).unwrap();
        assert_eq!(std::fs::read(path).unwrap(), source.as_bytes());
    }
    #[tokio::test]
    async fn preview_reads_real_http_and_rejects_denial_and_oversize_before_caching() {
        use crate::backend::tests::{read_http, reply};
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(
                read_http(&mut stream)
                    .await
                    .0
                    .starts_with("GET /api/file?asset=one%26two ")
            );
            reply(
                &mut stream,
                &data("test.txt", "text/plain", "Привет 🦀".as_bytes()).to_string(),
            )
            .await;
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http(&mut stream).await;
            let body = r#"{"error":"Not authorized"}"#;
            stream.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",body.len()).as_bytes()).await.unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            read_http(&mut stream).await;
            stream
                .write_all(
                    b"HTTP/1.1 200 OK\r\nContent-Length: 50000000\r\nConnection: close\r\n\r\n",
                )
                .await
                .unwrap();
        });
        let previews = Previews::default();
        for (index, asset) in ["one&two", "denied", "large"].into_iter().enumerate() {
            let response = previews
                .dispatch(
                    Some(&backend),
                    Request {
                        id: asset.into(),
                        method: "preview.open".into(),
                        params: json!({"target":{"asset":asset}}),
                    },
                )
                .await;
            if index == 0 {
                let (_, bytes) = previews.snapshot(&response.result.unwrap()).unwrap();
                assert_eq!(bytes.as_slice(), "Привет 🦀".as_bytes());
            } else {
                let message = response.error.unwrap().message;
                assert!(message.contains(if index == 1 {
                    "Not authorized"
                } else {
                    "exceeds"
                }));
            }
        }
        assert_eq!(previews.files.lock().unwrap().len(), 1);
        server.await.unwrap();
    }
}
