use std::env;
use std::fs;
use std::path::{Component, Path, PathBuf};
use studio_runtime_store::path_identity::CanonicalPath;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path as AxumPath, Request, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
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
// Shared with the Vite precompression plugin: extensions eligible for encoding negotiation.
const COMPRESSIBLE_EXTENSIONS: &str = include_str!("web_asset_extensions.txt");
const BUILD_INSTRUCTIONS: &str =
    "Build the interface from the repository root with `pnpm --filter codex-agents-web run build`.";

/// Environment variable naming the built renderer directory (a path containing `index.html`).
const WEB_DIST_ENV: &str = "CODEX_STUDIO_WEB_DIST";

#[derive(Clone, Debug)]
pub(super) struct WebAssets {
    root: Option<PathBuf>,
}

impl WebAssets {
    pub(super) fn discover() -> Self {
        // An explicit build directory wins; fixtures use it when the binary is built outside the checkout.
        if let Some(directory) = env::var_os(WEB_DIST_ENV).map(PathBuf::from)
            && directory.join("index.html").is_file()
            && let Ok(root) = CanonicalPath::existing(&directory)
        {
            return Self {
                root: Some(root.to_path_buf()),
            };
        }
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

async fn index(State(assets): State<WebAssets>, headers: HeaderMap) -> Response {
    match read_asset(&assets, "index.html", &headers).await {
        Ok(asset) => response("index.html", asset),
        Err(ReadAssetError::BuildMissing) => {
            (StatusCode::SERVICE_UNAVAILABLE, BUILD_INSTRUCTIONS).into_response()
        }
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn allowlisted(State(assets): State<WebAssets>, request: Request) -> Response {
    let Some(relative) = request.uri().path().strip_prefix('/') else {
        return StatusCode::NOT_FOUND.into_response();
    };
    if !STATIC_ALLOWLIST.contains(&relative) {
        return StatusCode::NOT_FOUND.into_response();
    }
    match read_asset(&assets, relative, request.headers()).await {
        Ok(asset) => response(relative, asset),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

async fn asset(
    State(assets): State<WebAssets>,
    AxumPath(relative): AxumPath<String>,
    headers: HeaderMap,
) -> Response {
    let relative = format!("assets/{relative}");
    match read_asset(&assets, &relative, &headers).await {
        Ok(asset) => response(&relative, asset),
        Err(_) => StatusCode::NOT_FOUND.into_response(),
    }
}

struct EncodedAsset {
    bytes: Vec<u8>,
    encoding: Option<&'static str>,
}

fn is_compressible(relative: &str) -> bool {
    Path::new(relative)
        .extension()
        .and_then(|extension| extension.to_str())
        .is_some_and(|extension| {
            COMPRESSIBLE_EXTENSIONS
                .split_ascii_whitespace()
                .any(|item| item == extension)
        })
}

// Prefer Brotli whenever allowed, then gzip. Explicit entries override `*`,
// including q=0. Missing/empty headers choose identity. If no encoded sibling
// is usable, keep serving identity so static assets remain available.
fn accepts_encoding(headers: &HeaderMap, encoding: &str) -> bool {
    let mut explicit = None;
    let mut wildcard = None;
    for value in headers.get_all(header::ACCEPT_ENCODING) {
        let Ok(value) = value.to_str() else { continue };
        for entry in value.split(',') {
            let mut fields = entry.split(';');
            let token = fields.next().unwrap_or_default().trim();
            let destination = if token.eq_ignore_ascii_case(encoding) {
                &mut explicit
            } else if token == "*" {
                &mut wildcard
            } else {
                continue;
            };
            let mut allowed = true;
            for parameter in fields {
                let Some((name, value)) = parameter.trim().split_once('=') else {
                    allowed = false;
                    continue;
                };
                if name.trim().eq_ignore_ascii_case("q") {
                    allowed &= valid_positive_quality(value.trim());
                } else {
                    allowed = false;
                }
            }
            // A duplicate entry must never undo an explicit refusal.
            *destination = Some(destination.unwrap_or(true) && allowed);
        }
    }
    explicit.or(wildcard).unwrap_or(false)
}

fn valid_positive_quality(value: &str) -> bool {
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if fraction.len() > 3 || !fraction.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    match whole {
        "0" => fraction.bytes().any(|byte| byte != b'0'),
        "1" => fraction.bytes().all(|byte| byte == b'0'),
        _ => false,
    }
}

async fn read_asset(
    assets: &WebAssets,
    relative: &str,
    headers: &HeaderMap,
) -> Result<EncodedAsset, ReadAssetError> {
    let Some(root) = &assets.root else {
        return Err(ReadAssetError::BuildMissing);
    };
    let requested = Path::new(relative);
    if requested
        .components()
        .any(|component| !matches!(component, Component::Normal(_)))
        || matches!(
            requested
                .extension()
                .and_then(|extension| extension.to_str()),
            Some("br" | "gz")
        )
    {
        return Err(ReadAssetError::NotFound);
    }
    let root = root.clone();
    let relative = relative.to_owned();
    let brotli = accepts_encoding(headers, "br");
    let gzip = accepts_encoding(headers, "gzip");
    studio_runtime_process::blocking_pool::spawn_blocking("runtime-server/web_assets", move || {
        // Resolve the original first: an orphan sibling cannot create a new URL.
        let original = resolve_asset(&root, &relative)?;
        if is_compressible(&relative) {
            for (allowed, suffix, encoding) in [(brotli, "br", "br"), (gzip, "gz", "gzip")] {
                if allowed
                    && let Ok(path) = resolve_asset(&root, &format!("{relative}.{suffix}"))
                    && let Ok(bytes) = fs::read(path)
                {
                    return Ok(EncodedAsset {
                        bytes,
                        encoding: Some(encoding),
                    });
                }
            }
        }
        fs::read(original)
            .map(|bytes| EncodedAsset {
                bytes,
                encoding: None,
            })
            .map_err(|_| ReadAssetError::NotFound)
    })
    .await
    .map_err(|_| ReadAssetError::NotFound)?
}

fn resolve_asset(root: &Path, relative: &str) -> Result<CanonicalPath, ReadAssetError> {
    let resolved =
        CanonicalPath::existing(root.join(relative)).map_err(|_| ReadAssetError::NotFound)?;
    if !resolved.starts_with(root) || !resolved.is_file() {
        return Err(ReadAssetError::NotFound);
    }
    Ok(resolved)
}

fn response(relative: &str, asset: EncodedAsset) -> Response {
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
    let mut response = Body::from(asset.bytes).into_response();
    if is_compressible(relative) {
        response
            .headers_mut()
            .insert(header::VARY, HeaderValue::from_static("Accept-Encoding"));
    }
    if let Some(encoding) = asset.encoding {
        response
            .headers_mut()
            .insert(header::CONTENT_ENCODING, HeaderValue::from_static(encoding));
    }
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
    if !matches!(
        extension,
        "js" | "css" | "png" | "svg" | "wasm" | "woff" | "woff2"
    ) {
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
            if candidate.join("index.html").is_file()
                && let Ok(root) = CanonicalPath::existing(&candidate)
            {
                return Some(root.to_path_buf());
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
            Some(
                CanonicalPath::existing(&legacy)
                    .expect("canonical legacy root")
                    .into_path_buf()
            )
        );
        fs::create_dir_all(&workspace).expect("workspace directory");
        fs::write(workspace.join("index.html"), "current").expect("current renderer");
        assert_eq!(
            find_dist_directory(&executable_directory),
            Some(
                CanonicalPath::existing(&workspace)
                    .expect("canonical workspace root")
                    .into_path_buf()
            )
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
        fs::write(root.join("assets/client_bg-i8Hpi7VY.wasm"), b"\0asm")
            .expect("hashed WebAssembly module");
        let assets = WebAssets {
            root: Some(
                CanonicalPath::existing(root)
                    .expect("canonical web root")
                    .to_path_buf(),
            ),
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
            to_bytes(index.into_body(), usize::MAX)
                .await
                .expect("index body bytes"),
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
                .expect("service worker content type")
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

        let module = app
            .clone()
            .oneshot(
                Request::get("/assets/client_bg-i8Hpi7VY.wasm")
                    .body(Body::empty())
                    .expect("WebAssembly request"),
            )
            .await
            .expect("WebAssembly response");
        assert_eq!(module.status(), StatusCode::OK);
        assert_eq!(
            module.headers()[header::CACHE_CONTROL],
            "private, max-age=31536000, immutable"
        );
        assert_eq!(module.headers()[header::CONTENT_TYPE], "application/wasm");

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
            to_bytes(api.into_body(), usize::MAX)
                .await
                .expect("API body bytes"),
            "api route"
        );
    }

    #[tokio::test]
    async fn negotiates_precompressed_assets_without_changing_types_or_cache_policy() {
        let (directory, assets) = fixture();
        let cases = [
            ("/", "index.html", "text/html; charset=utf-8", "no-store"),
            (
                "/index.html",
                "index.html",
                "text/html; charset=utf-8",
                "no-store",
            ),
            (
                "/studio-sw.js",
                "studio-sw.js",
                "text/javascript; charset=utf-8",
                "no-cache",
            ),
            (
                "/assets/app-0123456789.js",
                "assets/app-0123456789.js",
                "text/javascript; charset=utf-8",
                "private, max-age=31536000, immutable",
            ),
            (
                "/assets/client_bg-i8Hpi7VY.wasm",
                "assets/client_bg-i8Hpi7VY.wasm",
                "application/wasm",
                "private, max-age=31536000, immutable",
            ),
        ];
        let app = router(assets);
        for (url, path, mime, cache) in cases {
            let identity = fs::read(directory.path().join(path)).expect("original");
            fs::write(
                directory.path().join(format!("{path}.br")),
                b"brotli fixture",
            )
            .expect("br");
            fs::write(directory.path().join(format!("{path}.gz")), b"gzip fixture").expect("gzip");
            for (accept, encoding, body) in [
                (Some("gzip, br"), Some("br"), b"brotli fixture".as_slice()),
                (Some("gzip"), Some("gzip"), b"gzip fixture".as_slice()),
                (Some("*"), Some("br"), b"brotli fixture".as_slice()),
                (
                    Some("br;q=0, *;q=1"),
                    Some("gzip"),
                    b"gzip fixture".as_slice(),
                ),
                (Some("gzip;q=0, br;q=0"), None, identity.as_slice()),
                (Some("*;q=0, identity;q=1"), None, identity.as_slice()),
                (Some("*;q=0, identity;q=0"), None, identity.as_slice()),
                (
                    Some("br;q=0.000, gzip;q=1.000"),
                    Some("gzip"),
                    b"gzip fixture".as_slice(),
                ),
                (
                    Some("BR;Q=0.001, gzip;q=1"),
                    Some("br"),
                    b"brotli fixture".as_slice(),
                ),
                (Some("br;q=bogus, gzip;q=0"), None, identity.as_slice()),
                (Some("br;q=1.001, gzip;q=0"), None, identity.as_slice()),
                (
                    Some("br;q=0, br;q=1, gzip;q=1"),
                    Some("gzip"),
                    b"gzip fixture".as_slice(),
                ),
                (Some("deflate"), None, identity.as_slice()),
                (Some(""), None, identity.as_slice()),
                (None, None, identity.as_slice()),
            ] {
                let mut request = Request::get(url);
                if let Some(accept) = accept {
                    request = request.header(header::ACCEPT_ENCODING, accept);
                }
                let response = app
                    .clone()
                    .oneshot(request.body(Body::empty()).expect("request"))
                    .await
                    .expect("response");
                assert_eq!(response.status(), StatusCode::OK, "{url} {accept:?}");
                assert_eq!(
                    response
                        .headers()
                        .get(header::CONTENT_ENCODING)
                        .map(|v| v.to_str().expect("encoding")),
                    encoding,
                    "{url} {accept:?}"
                );
                assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
                assert_eq!(response.headers()[header::CONTENT_TYPE], mime);
                assert_eq!(response.headers()[header::CACHE_CONTROL], cache);
                assert_eq!(
                    to_bytes(response.into_body(), usize::MAX)
                        .await
                        .expect("body"),
                    body
                );
            }
        }
    }

    #[tokio::test]
    async fn missing_siblings_fall_back_and_head_preserves_encoded_headers() {
        let (directory, assets) = fixture();
        let app = router(assets);
        let path = "assets/app-0123456789.js";
        fs::write(directory.path().join(format!("{path}.gz")), b"gzip fixture").expect("gzip");
        for (accept, encoding) in [("gzip, br", Some("gzip")), ("br", None)] {
            let response = app
                .clone()
                .oneshot(
                    Request::get(format!("/{path}"))
                        .header(header::ACCEPT_ENCODING, accept)
                        .body(Body::empty())
                        .expect("request"),
                )
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::OK);
            assert_eq!(
                response
                    .headers()
                    .get(header::CONTENT_ENCODING)
                    .map(|v| v.to_str().expect("encoding")),
                encoding
            );
            assert_eq!(response.headers()[header::VARY], "Accept-Encoding");
        }
        let head = app
            .clone()
            .oneshot(
                Request::head(format!("/{path}"))
                    .header(header::ACCEPT_ENCODING, "gzip")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(head.headers()[header::CONTENT_ENCODING], "gzip");
        assert_eq!(head.headers()[header::CONTENT_LENGTH], "12");
        assert!(
            to_bytes(head.into_body(), usize::MAX)
                .await
                .expect("head body")
                .is_empty()
        );
        for url in [
            "/assets/app-0123456789.js.gz",
            "/assets/app-0123456789.js.br",
            "/index.html.br",
            "/assets/%2e%2e/index.html",
            "/assets/%2fetc/passwd",
        ] {
            let response = app
                .clone()
                .oneshot(Request::get(url).body(Body::empty()).expect("request"))
                .await
                .expect("response");
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{url}");
        }
        let refused = app
            .oneshot(
                Request::get("/")
                    .header(header::ACCEPT_ENCODING, "br, gzip, identity;q=0")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(refused.status(), StatusCode::OK);
        assert_eq!(refused.headers()[header::VARY], "Accept-Encoding");
        assert_eq!(
            to_bytes(refused.into_body(), usize::MAX)
                .await
                .expect("identity fallback"),
            "<html>ok</html>"
        );
    }

    #[tokio::test]
    async fn handles_repeated_encoding_headers_and_hides_orphan_siblings() {
        let (directory, assets) = fixture();
        fs::write(directory.path().join("index.html.br"), b"brotli fixture").expect("br");
        fs::write(directory.path().join("assets/orphan.js.br"), b"orphan").expect("orphan");
        let app = router(assets);
        let response = app
            .clone()
            .oneshot(
                Request::get("/")
                    .header(header::ACCEPT_ENCODING, "gzip;q=1")
                    .header(header::ACCEPT_ENCODING, "br;q=0.5")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.headers()[header::CONTENT_ENCODING], "br");
        let orphan = app
            .oneshot(
                Request::get("/assets/orphan.js")
                    .header(header::ACCEPT_ENCODING, "br")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(orphan.status(), StatusCode::NOT_FOUND);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn encoded_symlinks_cannot_escape_the_asset_root() {
        let (directory, assets) = fixture();
        let outside = tempfile::tempdir().expect("outside root");
        let secret = outside.path().join("private.txt");
        fs::write(&secret, b"private").expect("private file");
        std::os::unix::fs::symlink(&secret, directory.path().join("index.html.br"))
            .expect("encoded symlink");
        let response = router(assets)
            .oneshot(
                Request::get("/")
                    .header(header::ACCEPT_ENCODING, "br")
                    .body(Body::empty())
                    .expect("request"),
            )
            .await
            .expect("response");
        assert_eq!(response.status(), StatusCode::OK);
        assert!(response.headers().get(header::CONTENT_ENCODING).is_none());
        assert_eq!(
            to_bytes(response.into_body(), usize::MAX)
                .await
                .expect("identity"),
            "<html>ok</html>"
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
