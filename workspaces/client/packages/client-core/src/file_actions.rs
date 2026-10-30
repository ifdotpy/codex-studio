//! Resolve file identities through the verified backend before any desktop action.
use crate::{backend::Backend, protocol::Request};
use serde::{Deserialize, de::IgnoredAny};
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::PathBuf};
use studio_client_domain::json_object::JsonObject as ObjectOnly;

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Open,
    Reveal,
    Preview,
}

impl<'de> Deserialize<'de> for Action {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        match String::deserialize(deserializer)?.as_str() {
            "open" => Ok(Self::Open),
            "reveal" => Ok(Self::Reveal),
            "preview" => Ok(Self::Preview),
            _ => Err(serde::de::Error::custom("Invalid file action.")),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TargetWire {
    agent: Option<String>,
    path: Option<String>,
    asset: Option<String>,
}

enum Target {
    Asset {
        agent: Option<String>,
        asset: String,
    },
    Workspace {
        agent: String,
        path: String,
    },
}

impl TryFrom<TargetWire> for Target {
    type Error = String;
    fn try_from(wire: TargetWire) -> Result<Self, Self::Error> {
        for part in [&wire.agent, &wire.path, &wire.asset].into_iter().flatten() {
            if part.is_empty() || part.len() > 4096 || part.contains('\0') {
                return Err("Invalid file identity.".into());
            }
        }
        match (wire.agent, wire.path, wire.asset) {
            (agent, None, Some(asset)) => Ok(Self::Asset { agent, asset }),
            (Some(agent), Some(path), None) => Ok(Self::Workspace { agent, path }),
            _ => Err("Select a workspace file or attachment.".into()),
        }
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    action: Action,
    target: ObjectOnly<TargetWire>,
}

#[derive(Default)]
enum TextField {
    #[default]
    Missing,
    Text(String),
    Other,
}
impl<'de> Deserialize<'de> for TextField {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum TextOrOther {
            Text(String),
            Other(IgnoredAny),
        }
        Ok(match TextOrOther::deserialize(deserializer)? {
            TextOrOther::Text(value) => Self::Text(value),
            TextOrOther::Other(_) => Self::Other,
        })
    }
}

/// The file-info boundary consumes only path and the optional error envelope.
#[derive(Deserialize)]
struct FileInfo {
    #[serde(default)]
    path: TextField,
    #[serde(default)]
    error: TextField,
}

#[derive(Debug)]
pub struct ResolvedFile {
    pub action: Action,
    pub path: PathBuf,
    pub name: String,
}

fn selection(value: Value) -> Result<(Action, String), String> {
    let ObjectOnly(selected): ObjectOnly<Selection> =
        serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok((
        selected.action,
        target_path(selected.target.0.try_into()?, "/api/file-info")?,
    ))
}

pub fn content_path(value: Value) -> Result<String, String> {
    let ObjectOnly(target): ObjectOnly<TargetWire> =
        serde_json::from_value(value).map_err(|e| e.to_string())?;
    target_path(target.try_into()?, "/api/file")
}

fn target_path(target: Target, endpoint: &str) -> Result<String, String> {
    // Both callers pass a fixed absolute API path on this HTTP base.
    let mut query =
        reqwest::Url::parse(&format!("http://127.0.0.1{endpoint}")).expect("valid file API path");
    match target {
        Target::Asset { agent, asset } => {
            if let Some(agent) = agent {
                query.query_pairs_mut().append_pair("agent", &agent);
            }
            query.query_pairs_mut().append_pair("asset", &asset);
        }
        Target::Workspace { agent, path } => {
            query.query_pairs_mut().append_pair("agent", &agent);
            query.query_pairs_mut().append_pair("path", &path);
        }
    }
    // Validation requires an asset or an agent/path, so a pair was appended above.
    Ok(format!(
        "{}?{}",
        query.path(),
        query.query().expect("file query has a pair")
    ))
}

fn validate_file(action: Action, info: FileInfo) -> Result<ResolvedFile, String> {
    match info.error {
        TextField::Missing => {}
        TextField::Text(error) => return Err(error),
        TextField::Other => return Err("Cannot resolve the selected file.".into()),
    }
    let TextField::Text(path) = info.path else {
        return Err("Invalid resolved file path.".into());
    };
    if path.contains('\0') || path.len() > 4096 {
        return Err("Invalid resolved file path.".into());
    }
    let path = PathBuf::from(path);
    if !path.is_absolute() || path.canonicalize().map_err(|e| e.to_string())? != path {
        return Err("The selected file is no longer available.".into());
    }
    let metadata = path.metadata().map_err(|e| e.to_string())?;
    if !metadata.is_file() {
        return Err("The selected file is no longer available.".into());
    }
    let name = path
        .file_name()
        .and_then(|v| v.to_str())
        .filter(|v| v.len() <= 1024 && !v.chars().any(|c| c <= '\u{1f}'))
        .ok_or("Invalid file name.")?
        .to_owned();
    if action == Action::Open {
        // Match the desktop document policy. Preview and reveal do not execute files.
        const DOCUMENTS: &str = "txt md markdown pdf rtf csv tsv json xml yaml yml toml log diff patch png jpg jpeg gif webp avif heic heif tif tiff bmp svg mp3 m4a wav aac ogg flac mp4 m4v mov webm doc docx xls xlsx ppt pptx odt ods odp pages numbers key";
        let extension = path
            .extension()
            .and_then(|v| v.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if metadata.permissions().mode() & 0o111 != 0
            || !DOCUMENTS.split_whitespace().any(|v| v == extension)
        {
            return Err("Preview or reveal this file instead.".into());
        }
    }
    Ok(ResolvedFile { action, path, name })
}

pub async fn resolve(backend: &Backend, value: Value) -> Result<ResolvedFile, String> {
    let (action, path) = selection(value)?;
    let response = backend
        .dispatch(Request {
            id: "native-file-info".into(),
            method: "api.request".into(),
            params: json!({"path":path,"timeoutMs":15000}),
        })
        .await;
    let info = response.result.ok_or_else(|| {
        response
            .error
            .map(|e| e.message)
            .unwrap_or("Cannot resolve the selected file.".into())
    })?;
    let ObjectOnly(info): ObjectOnly<FileInfo> =
        serde_json::from_value(info).map_err(|_| "Invalid resolved file path.".to_owned())?;
    tokio::task::spawn_blocking(move || validate_file(action, info))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(value: Value) -> FileInfo {
        serde_json::from_value::<ObjectOnly<FileInfo>>(value)
            .unwrap()
            .0
    }

    #[test]
    fn identities_are_unambiguous_and_query_values_are_encoded() {
        for value in [
            json!({"action":{"open":null},"target":{"asset":"one"}}),
            json!({"action":"open","target":["a","x"]}),
            json!(["open",{"asset":"one"}]),
            json!({"action":"open","target":{"path":"/etc/passwd"}}),
            json!({"action":"open","target":{"asset":"one","path":"x"}}),
            json!({"action":"open","target":{"agent":"a","path":"x","extra":true}}),
            json!({"action":"execute","target":{"asset":"one"}}),
            json!({"action":"open","target":{"asset":""}}),
            json!({"action":"open","target":{"asset":"x\u{0}"}}),
            json!({"action":"open","target":{"agent":"a","path":"x".repeat(4097)}}),
            json!({"action":"open","target":{"asset":"one","agent":""}}),
        ] {
            assert!(selection(value).is_err());
        }
        let (_, path) =
            selection(json!({"action":"reveal","target":{"agent":"a&b","path":"/Привет 🦀.md"}}))
                .unwrap();
        let url = reqwest::Url::parse(&format!("http://127.0.0.1{path}")).unwrap();
        assert_eq!(
            url.query_pairs().collect::<Vec<_>>(),
            [
                ("agent".into(), "a&b".into()),
                ("path".into(), "/Привет 🦀.md".into())
            ]
        );
        assert_eq!(
            content_path(json!({"asset":"id&x","agent":"agent","path":null})).unwrap(),
            "/api/file?agent=agent&asset=id%26x"
        );
    }

    #[test]
    fn only_canonical_regular_documents_can_open() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let file = root.join("Отчёт.PDF");
        std::fs::write(&file, "test").unwrap();
        let info = json!({"path":file});
        assert_eq!(
            validate_file(Action::Open, self::info(info.clone()))
                .unwrap()
                .name,
            "Отчёт.PDF"
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(validate_file(Action::Open, self::info(info.clone())).is_err());
        assert!(validate_file(Action::Preview, self::info(info.clone())).is_ok());
        assert!(validate_file(Action::Reveal, self::info(info)).is_ok());
        let alias = root.join("alias.pdf");
        std::os::unix::fs::symlink(&file, &alias).unwrap();
        for path in [
            alias,
            root.clone(),
            root.join("gone.pdf"),
            PathBuf::from("relative.pdf"),
        ] {
            assert!(validate_file(Action::Reveal, self::info(json!({"path":path}))).is_err());
        }
        let script = root.join("script.sh");
        std::fs::write(&script, "echo unsafe").unwrap();
        assert!(validate_file(Action::Open, self::info(json!({"path":script}))).is_err());
        assert!(
            validate_file(
                Action::Preview,
                self::info(json!({"error":"Not authorized","path":file}))
            )
            .is_err()
        );
        for error in [
            json!(null),
            json!({"nested":true}),
            json!(false),
            json!([1]),
        ] {
            assert_eq!(
                validate_file(
                    Action::Preview,
                    self::info(json!({"error":error,"path":{"bad":true},"unknown":17}))
                )
                .unwrap_err(),
                "Cannot resolve the selected file."
            );
        }
        assert_eq!(
            validate_file(
                Action::Preview,
                self::info(json!({"error":"denied","path":false}))
            )
            .unwrap_err(),
            "denied"
        );
        for path in [
            std::env::temp_dir()
                .join("invalid\0name")
                .to_string_lossy()
                .into_owned(),
            format!("/{}", "x".repeat(4096)),
        ] {
            assert_eq!(
                validate_file(Action::Reveal, self::info(json!({"path":path}))).unwrap_err(),
                "Invalid resolved file path."
            );
        }
    }

    #[tokio::test]
    async fn native_file_resolution_uses_the_configured_backend_and_propagates_denial() {
        use crate::backend::tests::{read_http, reply};
        use tokio::{io::AsyncWriteExt, net::TcpListener};
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().canonicalize().unwrap().join("test.txt");
        std::fs::write(&file, "hello").unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let backend = Backend::new(&format!("http://{}", listener.local_addr().unwrap())).unwrap();
        let resolved_path = file.clone();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(
                read_http(&mut stream)
                    .await
                    .0
                    .starts_with("GET /api/file-info?asset=asset-one ")
            );
            reply(
                &mut stream,
                &json!({"path":resolved_path,"name":"test.txt"}).to_string(),
            )
            .await;
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(
                read_http(&mut stream)
                    .await
                    .0
                    .starts_with("GET /api/file-info?asset=missing ")
            );
            let body = r#"{"error":"Not authorized"}"#;
            stream.write_all(format!("HTTP/1.1 403 Forbidden\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(
                read_http(&mut stream)
                    .await
                    .0
                    .starts_with("GET /api/file-info?agent=a&asset=priority ")
            );
            reply(
                &mut stream,
                r#"{"error":null,"path":{"malformed":true},"future":[1]}"#,
            )
            .await;
            let (mut stream, _) = listener.accept().await.unwrap();
            assert!(
                read_http(&mut stream)
                    .await
                    .0
                    .starts_with("GET /api/file-info?agent=a&path=bad ")
            );
            reply(&mut stream, r#"{"path":false}"#).await;
        });
        assert_eq!(
            resolve(
                &backend,
                json!({"action":"open","target":{"asset":"asset-one"}})
            )
            .await
            .unwrap()
            .path,
            file
        );
        assert_eq!(
            resolve(
                &backend,
                json!({"action":"open","target":{"asset":"missing"}})
            )
            .await
            .unwrap_err(),
            "Not authorized"
        );
        assert_eq!(
            resolve(
                &backend,
                json!({"action":"reveal","target":{"agent":"a","asset":"priority"}})
            )
            .await
            .unwrap_err(),
            "Cannot resolve the selected file."
        );
        assert_eq!(
            resolve(
                &backend,
                json!({"action":"preview","target":{"agent":"a","path":"bad"}})
            )
            .await
            .unwrap_err(),
            "Invalid resolved file path."
        );
        server.await.unwrap();
    }
}
