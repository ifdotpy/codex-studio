//! Bundled, unprivileged server views use a different origin from the shell.
use tauri::{
    Runtime, UriSchemeContext,
    http::{Request, Response},
};

pub fn respond<R: Runtime>(
    context: UriSchemeContext<'_, R>,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    respond_with_app(context.app_handle(), request)
}

pub(crate) fn respond_with_app<R: Runtime>(
    app: &tauri::AppHandle<R>,
    request: Request<Vec<u8>>,
) -> Response<Vec<u8>> {
    let fail = |status| Response::builder().status(status).body(Vec::new()).unwrap();
    if !matches!(request.method().as_str(), "GET" | "HEAD") {
        return fail(405);
    }
    let Some(host) = request.uri().host() else {
        return fail(400);
    };
    if !host.starts_with("studio-")
        || !host.ends_with(".localhost")
        || !host
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'.')
    {
        return fail(400);
    }
    let path = request.uri().path();
    // Only immutable build assets and the entry point are exposed. There is no
    // filesystem or API forwarding, and no SPA fallback for arbitrary paths.
    if !(matches!(path, "/" | "/index.html") || path.starts_with("/assets/"))
        || path.contains('%')
        || path.contains('\\')
        || path.split('/').any(|part| part == "..")
    {
        return fail(404);
    }
    let resolver = app.asset_resolver();
    let asset_path = if path == "/" { "/index.html" } else { path };
    // Tauri's resolver falls back to index.html for missing files. Check the
    // embedded asset names first so an unknown asset never becomes a document.
    if !resolver.iter().any(|(name, _)| name.as_ref() == asset_path) {
        return fail(404);
    }
    let Some(asset) = resolver.get(asset_path.into()) else {
        return fail(404);
    };
    // Keep generated script hashes/nonces, but grant neither IPC nor network
    // access to a server view. Its shell derives its owner from the live frame.
    let base = asset.csp_header.unwrap_or_else(|| {
        "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline'".into()
    });
    let mut directives: Vec<_> = base
        .split(';')
        .map(str::trim)
        .filter(|part| {
            !part.starts_with("frame-ancestors")
                && !part.starts_with("connect-src")
                && !part.starts_with("frame-src")
        })
        .collect();
    directives.extend([
        "connect-src 'none'",
        "frame-src 'self' blob:",
        "frame-ancestors tauri://localhost",
    ]);
    Response::builder()
        .header("Content-Type", asset.mime_type)
        .header("Content-Security-Policy", directives.join("; "))
        .header("X-Content-Type-Options", "nosniff")
        .body(if request.method() == "HEAD" {
            Vec::new()
        } else {
            asset.bytes
        })
        .unwrap()
}
