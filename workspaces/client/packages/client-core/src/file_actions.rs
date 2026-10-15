//! Resolve file identities through the verified backend before any desktop action.
use crate::{backend::Backend, protocol::Request};
use serde::Deserialize;
use serde_json::{Value, json};
use std::{os::unix::fs::PermissionsExt, path::PathBuf};

#[derive(Clone, Copy, Debug, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum Action {
    Open,
    Reveal,
    Preview,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Target {
    agent: Option<String>,
    path: Option<String>,
    asset: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Selection {
    action: Action,
    target: Target,
}

#[derive(Debug)]
pub struct ResolvedFile {
    pub action: Action,
    pub path: PathBuf,
    pub name: String,
}

fn selection(value: Value) -> Result<(Action, String), String> {
    let selected: Selection = serde_json::from_value(value).map_err(|e| e.to_string())?;
    Ok((
        selected.action,
        target_path(selected.target, "/api/file-info")?,
    ))
}

pub fn content_path(value: Value) -> Result<String, String> {
    target_path(
        serde_json::from_value(value).map_err(|e| e.to_string())?,
        "/api/file",
    )
}

fn target_path(target: Target, endpoint: &str) -> Result<String, String> {
    for part in [&target.agent, &target.path, &target.asset]
        .into_iter()
        .flatten()
    {
        if part.is_empty() || part.len() > 4096 || part.contains('\0') {
            return Err("Invalid file identity.".into());
        }
    }
    if target.asset.is_some() {
        if target.path.is_some() {
            return Err("Select a workspace file or attachment.".into());
        }
    } else if target.agent.is_none() || target.path.is_none() {
        return Err("Select a workspace file or attachment.".into());
    }
    let mut query = reqwest::Url::parse(&format!("http://127.0.0.1{endpoint}")).unwrap();
    for (key, value) in [
        ("agent", target.agent),
        ("path", target.path),
        ("asset", target.asset),
    ] {
        if let Some(value) = value {
            query.query_pairs_mut().append_pair(key, &value);
        }
    }
    Ok(format!("{}?{}", query.path(), query.query().unwrap()))
}

fn validate_file(action: Action, info: Value) -> Result<ResolvedFile, String> {
    if let Some(error) = info.get("error") {
        return Err(error
            .as_str()
            .unwrap_or("Cannot resolve the selected file.")
            .into());
    }
    let path = info["path"]
        .as_str()
        .filter(|s| !s.contains('\0') && s.len() <= 4096)
        .ok_or("Invalid resolved file path.")?;
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
            return Err("Use Quick Look or Show in Folder for this file type.".into());
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
    tokio::task::spawn_blocking(move || validate_file(action, info))
        .await
        .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identities_are_unambiguous_and_query_values_are_encoded() {
        for value in [
            json!({"action":"open","target":{"path":"/etc/passwd"}}),
            json!({"action":"open","target":{"asset":"one","path":"x"}}),
            json!({"action":"open","target":{"agent":"a","path":"x","extra":true}}),
            json!({"action":"execute","target":{"asset":"one"}}),
            json!({"action":"open","target":{"asset":""}}),
            json!({"action":"open","target":{"asset":"x\u{0}"}}),
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
    }

    #[test]
    fn only_canonical_regular_documents_can_open() {
        let temp = tempfile::tempdir().unwrap();
        let root = temp.path().canonicalize().unwrap();
        let file = root.join("Отчёт.PDF");
        std::fs::write(&file, "test").unwrap();
        let info = json!({"path":file});
        assert_eq!(
            validate_file(Action::Open, info.clone()).unwrap().name,
            "Отчёт.PDF"
        );
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o700)).unwrap();
        assert!(validate_file(Action::Open, info.clone()).is_err());
        assert!(validate_file(Action::Preview, info.clone()).is_ok());
        assert!(validate_file(Action::Reveal, info).is_ok());
        let alias = root.join("alias.pdf");
        std::os::unix::fs::symlink(&file, &alias).unwrap();
        for path in [
            alias,
            root.clone(),
            root.join("gone.pdf"),
            PathBuf::from("relative.pdf"),
        ] {
            assert!(validate_file(Action::Reveal, json!({"path":path})).is_err());
        }
        let script = root.join("script.sh");
        std::fs::write(&script, "echo unsafe").unwrap();
        assert!(validate_file(Action::Open, json!({"path":script})).is_err());
        assert!(
            validate_file(
                Action::Preview,
                json!({"error":"Not authorized","path":file})
            )
            .is_err()
        );
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
        server.await.unwrap();
    }
}
