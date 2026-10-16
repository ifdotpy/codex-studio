use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;

const STATIC_ALLOWLIST: &[&str] = &[
    "index.html",
    "studio-sw.js",
    "studio-renderer.json",
    "manifest.webmanifest",
    "apple-touch-icon.png",
    "icon.svg",
    "icon-192.png",
    "icon-512.png",
];
const BUILD_INSTRUCTIONS: &str =
    "Build the interface from the repository root with `pnpm --filter codex-agents-web run build`.";

#[derive(Clone, Debug)]
pub(super) struct WebAssets {
    root: Option<PathBuf>,
}

impl WebAssets {
    pub(super) fn discover() -> Self {
        let root = env::current_exe()
            .ok()
            .and_then(|executable| executable.parent().map(Path::to_path_buf))
            .and_then(|directory| find_dist_directory(&directory));
        Self { root }
    }
}

pub(super) fn router(assets: WebAssets) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/index.html", get(index))
        .route("/assets/{*relative}", get(asset))
        .route("/studio-sw.js", get(allowlisted))
        .route("/studio-renderer.json", get(allowlisted))
        .route("/manifest.webmanifest", get(allowlisted))
        .route("/apple-touch-icon.png", get(allowlisted))
        .route("/icon.svg", get(allowlisted))
        .route("/icon-192.png", get(allowlisted))
        .route("/icon-512.png", get(allowlisted))
        .with_state(assets)
}

async fn index(State(assets): State<WebAssets>) -> Response {
    match read_asset(&assets, "index.html").await {
        Ok(bytes) => response("index.html", bytes),
        Err(ReadAssetError::BuildMissing) => {
            (StatusCode::SERVICE_UNAVAILABLE, BUILD_INSTRUCTIONS).into_response()
        }
        Err(ReadAssetError::NotFound) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn allowlisted(State(assets): State<WebAssets>, request: Request) -> Response {
    let Some(relative) = request.uri().path().strip_prefix('/') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !STATIC_ALLOWLIST.contains(&relative) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match read_asset(&assets, relative).await {
        Ok(bytes) => response(relative, bytes),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn asset(State(assets): State<WebAssets>, AxumPath(relative): AxumPath<String>) -> Response {
    let relative = format!("assets/{relative}");
    match read_asset(&assets, &relative).await {
        Ok(bytes) => response(&relative, bytes),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn read_asset(assets: &WebAssets, relative: &str) -> Result<Vec<u8>, ReadAssetError> {
    let Some(root) = &assets.root else {
        return Err(ReadAssetError::BuildMissing);
    };
    let requested = Path::new(relative);
    if requested
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(ReadAssetError::NotFound);
    }
    let candidate = root.join(requested);
    let resolved = candidate
        .canonicalize()
        .map_err(|_| ReadAssetError::NotFound)?;
    if !resolved.starts_with(root) || !resolved.is_file() {
        return Err(ReadAssetError::NotFound);
    }
    let resolved = resolved.clone();
    tokio::task::spawn_blocking(move || fs::read(resolved))
        .await
        .map_err(|_| ReadAssetError::NotFound)?
        .map_err(|_| ReadAssetError::NotFound)
}

fn response(relative: &str, bytes: Vec<u8>) -> Response {
    let mime = if relative.ends_with(".webmanifest") {
        "application/manifest+json".to_owned()
    } else {
        mime_guess::from_path(relative)
            .first_or_octet_stream()
            .essence_str()
            .to_owned()
    };
    let content_type = if matches!(mime.as_str(), "text/html" | "text/javascript" | "text/css") {
        format!("{mime}; charset=utf-8")
    } else {
        mime
    };
    let cache_control = if relative == "studio-sw.js" {
        "no-cache"
    } else if is_hashed_asset(relative) {
        "private, max-age=31536000, immutable"
    } else {
        "no-store"
    };
    let mut response = Body::from(bytes).into_response();
    if let Ok(value) = HeaderValue::from_str(&content_type) {
        response.headers_mut().insert(header::CONTENT_TYPE, value);
    }
    if let Ok(value) = HeaderValue::from_str(cache_control) {
        response.headers_mut().insert(header::CACHE_CONTROL, value);
    }
    response
}

fn is_hashed_asset(relative: &str) -> bool {
    let Some(filename) = relative.strip_prefix("assets/") else {
        return false;
    };
    let Some((stem, extension)) = filename.rsplit_once('.') else {
        return false;
    };
    if !matches!(extension, "js" | "css" | "png" | "svg" | "woff" | "woff2") {
        return false;
    }
    let Some((name, hash)) = stem.rsplit_once('-') else {
        return false;
    };
    !name.is_empty()
        && hash.len() >= 8
        && hash
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn find_dist_directory(executable_directory: &Path) -> Option<PathBuf> {
    for ancestor in executable_directory.ancestors() {
        for relative in [
            Path::new("workspaces/client/apps/web/dist"),
            Path::new("web/dist"),
        ] {
            let candidate = ancestor.join(relative);
            if candidate.join("index.html").is_file() {
                if let Ok(root) = candidate.canonicalize() {
                    return Some(root);
                }
            }
        }
    }
    None
}

enum ReadAssetError {
    BuildMissing,
    NotFound,
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::{Body, to_bytes};
    use axum::http::Request;
    use axum::routing::any;
    use tower::ServiceExt;

    #[test]
    fn discovers_workspace_assets_before_legacy_assets_and_keeps_packaged_fallback() {
        let directory = tempfile::tempdir().expect("repository fixture");
        let root = directory.path();
        let executable_directory = root.join("target/release");
        let legacy = root.join("web/dist");
        let workspace = root.join("workspaces/client/apps/web/dist");
        fs::create_dir_all(&executable_directory).expect("executable directory");
        fs::create_dir_all(&legacy).expect("legacy directory");
        fs::write(legacy.join("index.html"), "legacy").expect("legacy renderer");
        assert_eq!(
            find_dist_directory(&executable_directory),
            Some(legacy.canonicalize().expect("canonical legacy root"))
        );
        fs::create_dir_all(&workspace).expect("workspace directory");
        fs::write(workspace.join("index.html"), "current").expect("current renderer");
        assert_eq!(
            find_dist_directory(&executable_directory),
            Some(workspace.canonicalize().expect("canonical workspace root"))
        );
        assert_eq!(
            fs::read_to_string(legacy.join("index.html")).expect("legacy renderer retained"),
            "legacy"
        );
    }

    fn fixture() -> (tempfile::TempDir, WebAssets) {
        let directory = tempfile::tempdir().expect("temporary web root");
        let root = directory.path();
        fs::create_dir_all(root.join("assets")).expect("assets directory");
        fs::write(root.join("index.html"), "<html>ok</html>").expect("index html");
        fs::write(root.join("studio-sw.js"), "self.skipWaiting();").expect("service worker");
        fs::write(root.join("assets/app-0123456789.js"), "console.log('ok');")
            .expect("hashed bundle");
        let assets = WebAssets {
            root: Some(root.canonicalize().expect("canonical web root")),
        };
        (directory, assets)
    }

    #[tokio::test]
    async fn serves_index_worker_and_hashed_assets_with_expected_headers() {
        let (_directory, assets) = fixture();
        let app = Router::new()
            .route("/api/test", get(|| async { "api route" }))
            .fallback(any(|| async { StatusCode::NOT_FOUND }))
            .merge(router(assets));

        let index = app
            .clone()
            .oneshot(
                Request::get("/")
                    .body(Body::empty())
                    .expect("index request"),
            )
            .await
            .expect("index response");
        assert_eq!(index.status(), StatusCode::OK);
        assert_eq!(index.headers()[header::CACHE_CONTROL], "no-store");
        assert_eq!(
            to_bytes(index.into_body(), usize::MAX).await.unwrap(),
            "<html>ok</html>"
        );

        let worker = app
            .clone()
            .oneshot(
                Request::get("/studio-sw.js")
                    .body(Body::empty())
                    .expect("worker request"),
            )
            .await
            .expect("worker response");
        assert_eq!(worker.status(), StatusCode::OK);
        assert_eq!(worker.headers()[header::CACHE_CONTROL], "no-cache");
        assert!(
            worker.headers()[header::CONTENT_TYPE]
                .to_str()
                .unwrap()
                .starts_with("text/javascript")
        );

        let bundle = app
            .clone()
            .oneshot(
                Request::get("/assets/app-0123456789.js")
                    .body(Body::empty())
                    .expect("bundle request"),
            )
            .await
            .expect("bundle response");
        assert_eq!(bundle.status(), StatusCode::OK);
        assert_eq!(
            bundle.headers()[header::CACHE_CONTROL],
            "private, max-age=31536000, immutable"
        );

        let api = app
            .oneshot(
                Request::get("/api/test")
                    .body(Body::empty())
                    .expect("API request"),
            )
            .await
            .expect("API response");
        assert_eq!(api.status(), StatusCode::OK);
        assert_eq!(
            to_bytes(api.into_body(), usize::MAX).await.unwrap(),
            "api route"
        );
    }

    #[tokio::test]
    async fn missing_build_is_reported_on_index_and_unknown_paths_are_hidden() {
        let app = router(WebAssets { root: None });
        let index = app
            .clone()
            .oneshot(
                Request::get("/")
                    .body(Body::empty())
                    .expect("index request"),
            )
            .await
            .expect("index response");
        assert_eq!(index.status(), StatusCode::SERVICE_UNAVAILABLE);

        let unknown = app
            .oneshot(
                Request::get("/not-an-asset.js")
                    .body(Body::empty())
                    .expect("unknown request"),
            )
            .await
            .expect("unknown response");
        assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    }
}
